//! Module names, import maps, project declarations, and the 4-priority lightweight receiver
//! type resolution (Advisory lane only; never reports type errors).
//!
//! Priorities (docs/ANALYSIS_AND_KB_SPEC.md section 2.1):
//! 1. Exact: local construction and direct imports (`LoadGlobal`, `Import`, `AttrLoad` on a
//!    module, calls of classes, `AllocBuiltin`, `Copy`).
//! 2. Typed: PEP 484 parameter / local / return annotations, `self` / `cls`.
//! 3. Typed: `self.attr` types collected from `__init__` / `__post_init__` and annotations.
//! 4. Heuristic: CHA over project subclasses, at most `CHA_CANDIDATE_LIMIT` candidates.
//!
//! Reading a property (`obj.attr` on a project-class instance whose `attr` is a getter) runs
//! its getter, so `resolve_attr_load` resolves it like a method call.

use crate::types::{Args, Elem, Generic, Ty, generic, typing_generic};
use crate::{FuncKey, ModuleInput};
use heki_ast::Span;
use heki_ir::{
    BlockId, FnIndex, FunctionKind, InstKind, IrFunction, ModuleIr, SmallDisplay, VarId,
};
use heki_kb::{EffectiveEcosystemKb, ExecEffectKind};
use heki_semantic::{ModuleImport, ScopeKind, SemanticModel};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};

/// The derived indexes of a [`ProjectDecls`] version (`ProjectDecls::index_changes`).
struct Indexes {
    subclasses: FxHashMap<String, Vec<String>>,
    property_names: FxHashSet<String>,
    frozen: FxHashSet<String>,
    typed_dicts: FxHashSet<String>,
    protocols: FxHashMap<String, Option<Vec<String>>>,
}

/// Names every class has, which a protocol's body defines without requiring them.
const IMPLICIT_MEMBERS: &[&str] = &[
    "__annotations__",
    "__class__",
    "__classcell__",
    "__dict__",
    "__doc__",
    "__firstlineno__",
    "__init__",
    "__module__",
    "__qualname__",
    "__slots__",
    "__static_attributes__",
];

/// CHA fan-out limit: more candidates than this leave the call unresolved (`unknown_calls`).
pub const CHA_CANDIDATE_LIMIT: usize = 8;

/// Upper bound on re-export alias chasing (`pkg.__init__` re-exporting `pkg.sub.Foo`).
const MAX_ALIAS_CHASE: usize = 8;

/// Decorators that leave a generator function's call result a generator.
const GENERATOR_KEEPING_DECORATORS: &[&str] =
    &["staticmethod", "classmethod", "abstractmethod", "override"];

/// Decorators (by their last dotted component) that make a method a getter: reading the
/// attribute on an instance runs it (`@property`, `@functools.cached_property`).
const PROPERTY_DECORATORS: &[&str] = &["property", "cached_property"];

const BUILTIN_TYPES: &[&str] = &[
    "list",
    "dict",
    "set",
    "frozenset",
    "tuple",
    "str",
    "bytes",
    "bytearray",
    "int",
    "float",
    "bool",
    "object",
    "memoryview",
    "range",
];

// ------------------------------------------------------------------------------ module names

/// `src/` layout: some `root/src/<pkg>/__init__.py` exists.
pub fn detect_src_layout(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root.join("src")) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.path().join("__init__.py").is_file())
}

/// Module name of `rel` (the normalized project-relative path `rel_posix` produces):
/// `pkg/mod.py` -> `pkg.mod`, `pkg/__init__.py` -> `pkg` (package). With `src_layout`, a
/// leading `src/` is stripped. Returns `(module_name, is_package)`.
pub fn module_name_for_path(rel: &Path, src_layout: bool) -> Option<(String, bool)> {
    let mut parts: Vec<String> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_owned()),
            _ => return None,
        }
    }
    if src_layout && parts.first().map(String::as_str) == Some("src") {
        parts.remove(0);
    }
    let file = parts.pop()?;
    let stem = file
        .strip_suffix(".py")
        .or_else(|| file.strip_suffix(".pyi"))?;
    let is_package = stem == "__init__";
    if !is_package {
        parts.push(stem.to_owned());
    }
    if parts.is_empty() {
        return None;
    }
    Some((parts.join("."), is_package))
}

/// Resolve the module part of `from <level dots><module> import ...` inside `current`.
/// A package `__init__` is its own base; a plain module's base is its parent package.
pub fn resolve_relative_module(
    current: &str,
    current_is_package: bool,
    level: u32,
    module: Option<&str>,
) -> Option<String> {
    if level == 0 {
        return module.map(str::to_owned);
    }
    let mut parts: Vec<&str> = current.split('.').collect();
    if !current_is_package {
        parts.pop();
    }
    for _ in 1..level {
        parts.pop()?;
    }
    let base = parts.join(".");
    match (base.is_empty(), module) {
        (true, None) => None,
        (true, Some(m)) => Some(m.to_owned()),
        (false, None) => Some(base),
        (false, Some(m)) => Some(format!("{base}.{m}")),
    }
}

/// Fully qualified symbol bound by one import.
fn import_target(imp: &ModuleImport, module_name: &str, is_package: bool) -> Option<String> {
    if !imp.is_from {
        let top = imp
            .imported_name
            .split('.')
            .next()
            .unwrap_or(&imp.imported_name);
        // `import a.b.c` binds `a`; `import a.b.c as x` binds `a.b.c`.
        return Some(if imp.bound_name == top {
            top.to_owned()
        } else {
            imp.imported_name.clone()
        });
    }
    let base = resolve_relative_module(module_name, is_package, imp.level, imp.module.as_deref())?;
    Some(format!("{base}.{}", imp.imported_name))
}

// ------------------------------------------------------------------------------ declarations

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassDecl {
    /// Base classes, resolved within the defining module (canonicalized on lookup).
    pub bases: Vec<String>,
    /// Priority 3 `attr -> type` (instance attribute types).
    pub attr_types: BTreeMap<String, Ty>,
    /// `@dataclass(frozen=True)` / `@dataclasses.dataclass(frozen=True)`, or a `NamedTuple`:
    /// the class promises immutable instances (`MUT108`). Classes a PEP 681
    /// `dataclass_transform(frozen_default=True)` builds are frozen too, and the subclasses of a
    /// frozen class keep the promise for its fields: both are resolved across modules
    /// (`ProjectDecls::readonly_attr`).
    pub is_frozen: bool,
    /// The class-body names that hold a container, the fields of a frozen class: annotated with
    /// a container type (`list`, `dict`, `set`, `Sequence`, `Mapping`, ... after `Optional` /
    /// `Annotated` / `Final`), other than `ClassVar` / `InitVar` and `field(compare=False)`
    /// fields (state outside the value, such as a cache). Attributes a frozen class sets
    /// otherwise (`object.__setattr__(self, "_cache", {})`) are no fields.
    pub container_fields: BTreeSet<String>,
    /// Attributes holding a container whose binding is read-only: class-body `ReadOnly[...]`
    /// names (PEP 705, `TypedDict` keys included), `Final[...]` fields of a dataclass or
    /// `NamedTuple`, and instance attributes annotated `self.x: Final[...]` in `__init__`, each
    /// with a container type inside (`MUT108`). Class variables and class-level constants of
    /// other classes are not.
    pub readonly_attrs: BTreeSet<String>,
    /// The class's decorators (resolved heads) with the `frozen=` each passes: a decorator
    /// function carrying a `dataclass_transform` builds the class.
    pub decorators: Vec<(String, Option<bool>)>,
    /// The resolved `metaclass=` of the class statement.
    pub metaclass: Option<String>,
    /// The `frozen=` keyword of the class statement (`class C(Base, frozen=False)`).
    pub frozen_keyword: Option<bool>,
    /// `@dataclass_transform(...)` on this class (as a base class or metaclass of the classes it
    /// builds): `Some(frozen_default)`.
    pub transform: Option<bool>,
    /// What the class's own body and methods store in its attributes (`attr = [...]` in the
    /// body, `self.attr = ...` in a method): `Some` when every store is a small display that
    /// none of them grows (`heki_ir::FnIndex::stored_display`), `None` when an attribute is
    /// stored anything else, deleted, or grown through `self` (`OPT112` / `OPT113` read the
    /// attributes of a class family through [`ProjectDecls::small_attr`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attr_displays: BTreeMap<String, Option<SmallDisplay>>,
    /// `typing.Protocol` is one of the bases (PEP 544: the class is a protocol, not an
    /// implementation of one).
    pub is_protocol: bool,
    /// The names the class defines: class-body names (methods, annotations, assignments) and
    /// the attributes its methods set or annotate on `self` (structural typing, `Protocol`).
    pub members: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionDecl {
    pub is_async: bool,
    pub is_staticmethod: bool,
    pub is_classmethod: bool,
    pub is_method: bool,
    /// Resolved return annotation, Advisory only.
    pub return_type: Option<Ty>,
    pub param_names: Vec<String>,
    /// How many leading parameters a position binds (the receiver included).
    pub positional_params: u8,
    /// How many leading parameters a keyword binds (all but `*args` / `**kwargs`, which come
    /// last).
    pub named_params: u8,
    /// The literal defaults, by parameter position (`guards`: an omitted argument).
    pub defaults: Vec<(u8, crate::ConstArg)>,
    /// Calling it returns a fresh (async) generator: a generator function without decorators
    /// other than `staticmethod` / `classmethod` / `abstractmethod` / `override` (a
    /// `@contextmanager` generator returns a context manager). `MUT106` single-pass sources.
    pub is_generator: bool,
    /// A property getter that reading the attribute on an instance runs: a method decorated
    /// with `property` / `cached_property` (any decorator whose last dotted component is one of
    /// them) whose every definition is such a getter. A property with a setter or deleter is
    /// not one: `@x.setter def x(...)` is a second function with the getter's qualname, whose
    /// facts share the getter's summary, so reading the property would pick up the setter's
    /// writes.
    pub is_property: bool,
    /// Parameters (bit `i`: parameter `i < 64`) annotated with a read-only container type:
    /// `Sequence[...]`, `Mapping[...]`, or `ReadOnly[...]` after `Optional` / `| None` /
    /// `Annotated` (`MUT108`). Variadic parameters never are.
    pub readonly_params: u64,
    /// Parameters annotated with a mutable container type (`list`, `dict`, `set`,
    /// `bytearray`, `MutableSequence`, `MutableMapping`, `MutableSet`, `deque`, ...): a type
    /// checker reports a read-only argument passed there (`MUT108`).
    pub mutable_params: u64,
    /// `@dataclass_transform(...)` on some definition of this decorator function (PEP 681: an
    /// overload or the implementation): `Some(frozen_default)` (`MUT108`).
    pub transform: Option<bool>,
    /// The Layer 2 symbol of a decorator that runs the function on another thread, on every
    /// definition: a catalog entry with `offloads_blocking_arg = 0`, applied directly or as a
    /// decorator factory (`@sync_to_async`, `@sync_to_async(thread_sensitive=False)`,
    /// `@database_sync_to_async`, `@asyncify`). `@d def f` is `f = d(f)`: calling `f` calls
    /// `d(f)`, which offloads the body, so the body does not run at the call.
    pub offloaded_by: Option<String>,
    /// The return annotation of the getter of a property (`@property` / `cached_property`,
    /// with or without a setter): the type reading the attribute gives.
    pub property_type: Option<Ty>,
}

/// Per-module declarations other modules need (stored in `FileSummary`, IR-free).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleDecls {
    pub module_name: String,
    pub is_package: bool,
    /// Module-scope runtime imports: `bound name -> fqn`.
    pub imports: BTreeMap<String, String>,
    /// `if TYPE_CHECKING:` imports, usable for annotation resolution only.
    pub type_checking_imports: BTreeMap<String, String>,
    /// Class qualname -> declaration.
    pub classes: BTreeMap<String, ClassDecl>,
    /// Function qualname -> declaration (module body excluded).
    pub functions: BTreeMap<String, FunctionDecl>,
    /// Module global `name -> type` of the instance it holds (`client = httpx.Client()`).
    pub global_types: BTreeMap<String, Ty>,
    /// Module globals bound to a small display that no function of the module grows
    /// (`heki_ir::ModuleConstants::displays`): other modules read them through
    /// [`ProjectDecls::small_global`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub small_globals: BTreeMap<String, SmallDisplay>,
}

impl ModuleDecls {
    /// The pass-1 declarations these were completed from (`project_decls_for`): without the
    /// inferred instance attribute and module global types, and without the offloading
    /// decorators resolved through the project (`FunctionDecl::offloaded_by`).
    pub fn structural(&self) -> Self {
        let mut out = self.clone();
        for class in out.classes.values_mut() {
            class.attr_types.clear();
        }
        for function in out.functions.values_mut() {
            function.offloaded_by = None;
        }
        out.global_types.clear();
        out
    }

    /// Resolve a dotted name written in this module (runtime imports, then local definitions,
    /// then builtin types). `type_checking` also consults `if TYPE_CHECKING:` imports.
    fn resolve_dotted(&self, text: &str, type_checking: bool) -> Option<String> {
        let (first, rest) = match text.split_once('.') {
            Some((f, r)) => (f, Some(r)),
            None => (text, None),
        };
        let join = |base: &str| match rest {
            Some(r) => format!("{base}.{r}"),
            None => base.to_owned(),
        };
        if let Some(t) = self.imports.get(first) {
            return Some(join(t));
        }
        if type_checking && let Some(t) = self.type_checking_imports.get(first) {
            return Some(join(t));
        }
        if self.classes.contains_key(first) || self.functions.contains_key(first) {
            return Some(format!("{}.{text}", self.module_name));
        }
        if rest.is_none() && BUILTIN_TYPES.contains(&first) {
            return Some(format!("builtins.{first}"));
        }
        None
    }

    /// Priority 2 annotation normalization: `Optional[T]` / `T | None` / `Annotated[T, ...]`
    /// / quoted `"T"` -> `T`; `list[...]` -> `builtins.list`; resolved to a fqn.
    pub fn resolve_annotation(&self, text: &str) -> Option<String> {
        let mut t = text.trim();
        loop {
            let unquoted = t
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| t.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
            if let Some(u) = unquoted {
                t = u.trim();
                continue;
            }
            let parts = split_top_level(t, '|');
            if parts.len() > 1 {
                let non_none: Vec<&str> = parts.into_iter().filter(|p| *p != "None").collect();
                if non_none.len() != 1 {
                    return None;
                }
                t = non_none[0];
                continue;
            }
            if let Some((head, inner)) = subscript(t) {
                let last = head.rsplit('.').next().unwrap_or(head);
                match last {
                    "Optional" | "Annotated" | "Final" | "ClassVar" | "Required"
                    | "NotRequired" | "ReadOnly" => {
                        t = split_top_level(inner, ',').first().copied()?;
                        continue;
                    }
                    "Union" => {
                        let non_none: Vec<&str> = split_top_level(inner, ',')
                            .into_iter()
                            .filter(|p| *p != "None")
                            .collect();
                        if non_none.len() != 1 {
                            return None;
                        }
                        t = non_none[0];
                        continue;
                    }
                    _ => {}
                }
                return self.resolve_type_head(head);
            }
            return self.resolve_type_head(t);
        }
    }

    /// The type an annotation names (receiver typing): a class with the arguments of a known
    /// generic container (`list[Model]`, `Mapping[str, Model]`, `tuple[Model, int]`,
    /// `Iterator[Model]`), after `Optional` / `| None` / `Annotated` / `Final` / `ClassVar` /
    /// `Required` / `NotRequired` / `ReadOnly` and quotes. An unresolved name of a `typing`
    /// generic counts as that generic (`"Iterable[Model]"` without the import).
    pub fn resolve_annotation_ty(&self, text: &str) -> Option<Ty> {
        let t = unquote(text);
        let mut parts = split_top_level(t, '|');
        if parts.len() > 1 {
            parts.retain(|p| *p != "None");
            return self.union_annotation(&parts);
        }
        let Some((head, inner)) = subscript(t) else {
            // `typing.Self` (also unimported, in a string annotation): the receiver's type.
            if matches!(
                self.resolve_dotted(t, true).as_deref(),
                Some("typing.Self" | "typing_extensions.Self") | None
            ) && t.rsplit('.').next() == Some("Self")
            {
                return Some(Ty::SelfType);
            }
            return self
                .resolve_type_head(t)
                .filter(|c| !special_form(c))
                .map(Ty::inst);
        };
        let last = head.rsplit('.').next().unwrap_or(head);
        match last {
            "Optional" | "Annotated" | "Final" | "ClassVar" | "Required" | "NotRequired"
            | "ReadOnly" => return self.resolve_annotation_ty(first_arg(inner)),
            "Union" => {
                let mut members = split_top_level(inner, ',');
                members.retain(|p| *p != "None");
                return self.union_annotation(&members);
            }
            _ => {}
        }
        let class = self
            .resolve_type_head(head)
            .or_else(|| typing_generic(last))
            .filter(|c| !special_form(c))?;
        let args = split_top_level(inner, ',');
        let elem = |i: usize| Elem::of(args.get(i).and_then(|a| self.resolve_annotation_ty(a)));
        let args = match generic(&class) {
            None => Args::None,
            Some(Generic::Items) => Args::items(elem(0)),
            Some(Generic::Map) => Args::map(elem(0), elem(1)),
            Some(Generic::Pairs) => Args::items(Elem::Of(Ty::container(
                "builtins.tuple",
                Args::Tuple(vec![elem(0), elem(1)]),
            ))),
            Some(Generic::Counter) => Args::map(elem(0), Elem::Of(Ty::inst("builtins.int"))),
            // `tuple[()]` is the empty tuple, `tuple[T, ...]` a tuple of any length.
            Some(Generic::Tuple) => match args.as_slice() {
                ["()"] => Args::Tuple(Vec::new()),
                [_, "..."] => Args::items(elem(0)),
                _ => Args::Tuple((0..args.len()).map(elem).collect()),
            },
        };
        Some(Ty::container(&class, args))
    }

    /// The members of a union annotation other than `None`: one type, or instances of at most
    /// [`crate::types::MAX_UNION`] classes.
    fn union_annotation(&self, members: &[&str]) -> Option<Ty> {
        if let [one] = members {
            return self.resolve_annotation_ty(one);
        }
        let mut objs = Vec::new();
        for m in members {
            let t = self.resolve_annotation_ty(m)?;
            if t.members().is_empty() {
                return None;
            }
            objs.extend(t.members().iter().cloned());
        }
        Ty::of_members(objs)
    }

    fn resolve_type_head(&self, head: &str) -> Option<String> {
        if head.is_empty()
            || head == "None"
            || !head
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        {
            return None;
        }
        let typing_alias = |name: &str| match name {
            "List" => Some("list"),
            "Dict" | "DefaultDict" | "OrderedDict" => Some("dict"),
            "Set" => Some("set"),
            "FrozenSet" => Some("frozenset"),
            "Tuple" => Some("tuple"),
            _ => None,
        };
        let last = head.rsplit('.').next().unwrap_or(head);
        let resolved = self.resolve_dotted(head, true);
        if let Some(r) = &resolved {
            if let Some(stripped) = r.strip_prefix("typing.")
                && let Some(b) = typing_alias(stripped)
            {
                return Some(format!("builtins.{b}"));
            }
            return resolved;
        }
        typing_alias(last).map(|b| format!("builtins.{b}"))
    }
}

/// A `typing` special form that names no class of instances (`Final` and `ClassVar` without an
/// argument take their type from the value; `TypeAlias` declares a type, not an instance).
fn special_form(class: &str) -> bool {
    const FORMS: &[&str] = &[
        "Annotated",
        "ClassVar",
        "Final",
        "Literal",
        "Never",
        "NoReturn",
        "NotRequired",
        "Optional",
        "ReadOnly",
        "Required",
        "TypeAlias",
        "Union",
    ];
    class
        .strip_prefix("typing.")
        .or_else(|| class.strip_prefix("typing_extensions."))
        .is_some_and(|name| FORMS.contains(&name))
}

/// Split at `sep` outside brackets / parentheses, trimming parts.
fn split_top_level(text: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth -= 1,
            _ if c == sep && depth == 0 => {
                out.push(text[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(text[start..].trim());
    out
}

/// `Head[inner]` -> `(Head, inner)`.
fn subscript(text: &str) -> Option<(&str, &str)> {
    let open = text.find('[')?;
    let inner = text.strip_suffix(']')?.get(open + 1..)?;
    Some((text[..open].trim(), inner))
}

// ------------------------------------------------------------------------- MUT108 annotations

/// `"T"` / `'T'` -> `T` (any depth).
fn unquote(mut text: &str) -> &str {
    loop {
        let t = text.trim();
        let inner = t
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| t.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
        match inner {
            Some(i) => text = i,
            None => return t,
        }
    }
}

/// `Head[inner]` -> `(Head, Some(inner))`; anything else is a bare head.
fn head_and_args(text: &str) -> (&str, Option<&str>) {
    subscript(text).map_or((text, None), |(h, i)| (h, Some(i)))
}

/// The first top-level argument of a subscript (`int` of `Final[int]`).
fn first_arg(inner: &str) -> &str {
    split_top_level(inner, ',')[0]
}

/// Type qualifiers around an attribute annotation, outermost first through `Annotated`,
/// `Required`, and `NotRequired` (`ClassVar[Final[int]]`, `ReadOnly[NotRequired[list[str]]]`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Qualifiers {
    pub class_var: bool,
    pub is_final: bool,
    pub read_only: bool,
    /// `InitVar[...]` / `KW_ONLY`: a dataclass pseudo-field that is no attribute.
    pub pseudo_field: bool,
}

/// The qualifier a (possibly subscripted) head names: a `typing` / `typing_extensions` one
/// (`ClassVar`, `Final`, `ReadOnly`, `Annotated`, `Required`, `NotRequired`) or a `dataclasses`
/// one (`InitVar`, `KW_ONLY`), by the fqn `resolve` gives it. An unresolved name only counts
/// when it takes protection away (`ClassVar`, `InitVar`, `KW_ONLY`): a report needs evidence.
fn qualifier_name(head: &str, resolve: &impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    const TYPING: &[&str] = &[
        "Annotated",
        "ClassVar",
        "Final",
        "NotRequired",
        "ReadOnly",
        "Required",
    ];
    const DATACLASSES: &[&str] = &["InitVar", "KW_ONLY"];
    const REMOVING: &[&str] = &["ClassVar", "InitVar", "KW_ONLY"];
    let Some(fqn) = resolve(head) else {
        let last = head.rsplit('.').next().unwrap_or(head);
        return REMOVING.iter().copied().find(|n| *n == last);
    };
    let (module, name) = fqn.rsplit_once('.')?;
    let names = match module {
        "typing" | "typing_extensions" => TYPING,
        "dataclasses" => DATACLASSES,
        _ => return None,
    };
    names.iter().copied().find(|n| *n == name)
}

/// The qualifiers of the attribute annotation `text` and the type they wrap (`None` for a bare
/// qualifier such as `Final`, or a pseudo-field); `resolve` maps a dotted name written in the
/// module to its fqn.
pub(crate) fn qualifiers<'t>(
    text: &'t str,
    resolve: &impl Fn(&str) -> Option<String>,
) -> (Qualifiers, Option<&'t str>) {
    let mut q = Qualifiers::default();
    let mut t = text;
    loop {
        let u = unquote(t);
        let (head, args) = head_and_args(u);
        match qualifier_name(head, resolve) {
            Some("ClassVar") => q.class_var = true,
            Some("Final") => q.is_final = true,
            Some("ReadOnly") => q.read_only = true,
            Some("InitVar" | "KW_ONLY") => q.pseudo_field = true,
            Some(_) => {}
            None => return (q, Some(u)),
        }
        match args {
            Some(inner) if !q.pseudo_field => t = first_arg(inner),
            _ => return (q, None),
        }
    }
}

/// The attribute annotation `text` declares a container (a read-only or mutable container type,
/// `param_access`), unless a qualifier makes it no instance attribute (`ClassVar`, `InitVar`,
/// `KW_ONLY`); returns the qualifiers with it.
fn container_annotation(
    text: &str,
    resolve: &impl Fn(&str) -> Option<String>,
) -> (Qualifiers, bool) {
    let (q, inner) = qualifiers(text, resolve);
    let container =
        !q.class_var && !q.pseudo_field && inner.and_then(|t| param_access(t, resolve)).is_some();
    (q, container)
}

/// What a parameter annotation promises about changing the argument (`MUT108`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamAccess {
    /// `Sequence[...]` / `Mapping[...]` / `ReadOnly[...]`: the argument is only read.
    ReadOnly,
    /// A mutable container type: the argument may be changed in place.
    Mutable,
}

/// Read-only container types (`ParamAccess::ReadOnly`).
const READONLY_TYPES: &[&str] = &[
    "typing.Sequence",
    "typing.Mapping",
    "collections.abc.Sequence",
    "collections.abc.Mapping",
];

/// Mutable container types (`ParamAccess::Mutable`).
const MUTABLE_TYPES: &[&str] = &[
    "builtins.list",
    "builtins.dict",
    "builtins.set",
    "builtins.bytearray",
    "typing.List",
    "typing.Dict",
    "typing.Set",
    "typing.DefaultDict",
    "typing.OrderedDict",
    "typing.Counter",
    "typing.Deque",
    "typing.MutableSequence",
    "typing.MutableMapping",
    "typing.MutableSet",
    "collections.abc.MutableSequence",
    "collections.abc.MutableMapping",
    "collections.abc.MutableSet",
    "collections.deque",
    "collections.defaultdict",
    "collections.OrderedDict",
    "collections.Counter",
];

/// The access the parameter annotation `text` promises, after `Optional[T]`, `T | None`,
/// `Union[T, None]`, `Annotated[T, ...]`, and quotes; `None` for any other type (`Iterable`,
/// `Any`, a project class) and for unions of several types. An unresolved name only counts as
/// mutable (a report needs evidence).
pub(crate) fn param_access(
    text: &str,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Option<ParamAccess> {
    const MUTABLE_NAMES: &[&str] = &[
        "List",
        "Dict",
        "Set",
        "DefaultDict",
        "OrderedDict",
        "Counter",
        "Deque",
        "MutableSequence",
        "MutableMapping",
        "MutableSet",
        "deque",
        "defaultdict",
    ];
    let mut t = text;
    loop {
        let u = unquote(t);
        let mut parts = split_top_level(u, '|');
        if parts.len() > 1 {
            parts.retain(|p| *p != "None");
            t = single(&parts)?;
            continue;
        }
        let (head, args) = head_and_args(u);
        let Some(fqn) = resolve(head) else {
            let last = head.rsplit('.').next().unwrap_or(head);
            return MUTABLE_NAMES
                .contains(&last)
                .then_some(ParamAccess::Mutable);
        };
        let wrapper = fqn
            .strip_prefix("typing.")
            .or_else(|| fqn.strip_prefix("typing_extensions."));
        match (wrapper, args) {
            (Some("Optional" | "Annotated"), Some(inner)) => t = first_arg(inner),
            (Some("Union"), Some(inner)) => {
                let mut members = split_top_level(inner, ',');
                members.retain(|p| *p != "None");
                t = single(&members)?;
            }
            (Some("ReadOnly"), _) => return Some(ParamAccess::ReadOnly),
            _ if READONLY_TYPES.contains(&fqn.as_str()) => return Some(ParamAccess::ReadOnly),
            _ => {
                return MUTABLE_TYPES
                    .contains(&fqn.as_str())
                    .then_some(ParamAccess::Mutable);
            }
        }
    }
}

/// The only element of `parts`.
fn single<'s>(parts: &[&'s str]) -> Option<&'s str> {
    match parts {
        [one] => Some(one),
        _ => None,
    }
}

/// A decorator's head and the arguments of its call (`record` and `frozen=False` of
/// `record(frozen=False)`; no arguments without a call).
fn decorator_parts(text: &str) -> (&str, &str) {
    match text.split_once('(') {
        Some((h, rest)) => (h.trim(), rest.trim_end().strip_suffix(')').unwrap_or(rest)),
        None => (text.trim(), ""),
    }
}

/// `True` / `False`; `None` for any other expression.
fn bool_literal(text: &str) -> Option<bool> {
    match text.trim() {
        "True" => Some(true),
        "False" => Some(false),
        _ => None,
    }
}

/// The value of the boolean keyword argument `name` in call arguments (`frozen=True`); `None`
/// when it is not passed or is no `True` / `False` literal.
fn keyword_bool(args: &str, name: &str) -> Option<bool> {
    split_top_level(args, ',').iter().find_map(|arg| {
        let (k, v) = arg.split_once('=')?;
        (k.trim() == name).then(|| bool_literal(v)).flatten()
    })
}

/// Whether the class decorator `text` (its source) is `dataclass` / `dataclasses.dataclass`
/// (resolved through the module's imports), and then whether it passes `frozen=True`.
pub(crate) fn dataclass_decorator(
    text: &str,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Option<bool> {
    let (head, args) = decorator_parts(text);
    (resolve(head)? == "dataclasses.dataclass").then(|| keyword_bool(args, "frozen") == Some(true))
}

/// Whether the decorator `text` is PEP 681 `typing.dataclass_transform` /
/// `typing_extensions.dataclass_transform`, and then its `frozen_default`.
pub(crate) fn transform_decorator(
    text: &str,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Option<bool> {
    let (head, args) = decorator_parts(text);
    let fqn = resolve(head)?;
    let module = fqn.strip_suffix(".dataclass_transform")?;
    matches!(module, "typing" | "typing_extensions")
        .then(|| keyword_bool(args, "frozen_default") == Some(true))
}

/// Bases that make a class a `NamedTuple` (frozen) or a `TypedDict`.
const NAMED_TUPLE_BASES: &[&str] = &["typing.NamedTuple", "typing_extensions.NamedTuple"];
const TYPED_DICT_BASES: &[&str] = &["typing.TypedDict", "typing_extensions.TypedDict"];

/// Keys into the project declarations: what one analysis read of them
/// ([`ProjectDecls::record`]), or what an update changed in them ([`DeclPasses`]). An analysis
/// that read a key an update changed may come out differently: incremental re-analysis goes by
/// this (ADR-017).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclKeys {
    /// Module names looked up, whether or not such a module exists (one that appears or
    /// disappears answers differently).
    pub modules: BTreeSet<String>,
    /// Classes whose direct project subclasses were listed (class hierarchy analysis).
    pub subclasses: BTreeSet<String>,
    /// Attribute names looked up among the project's property getter names.
    pub property_names: BTreeSet<String>,
    /// Classes whose project-wide derived facts were looked up: whether they are frozen (a
    /// `dataclass_transform` or a base in another module can make them so) or a `TypedDict`.
    pub class_facts: BTreeSet<String>,
    /// Protocols whose structural implementations were listed (a class in any module can start
    /// or stop satisfying one).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub protocols: BTreeSet<String>,
}

impl DeclKeys {
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
            && self.subclasses.is_empty()
            && self.property_names.is_empty()
            && self.class_facts.is_empty()
            && self.protocols.is_empty()
    }

    pub fn extend(&mut self, other: Self) {
        self.modules.extend(other.modules);
        self.subclasses.extend(other.subclasses);
        self.property_names.extend(other.property_names);
        self.class_facts.extend(other.class_facts);
        self.protocols.extend(other.protocols);
    }

    /// Whether the two share a key (an analysis that read `self` is affected by the change
    /// `other`).
    pub fn intersects(&self, other: &Self) -> bool {
        !self.modules.is_disjoint(&other.modules)
            || !self.subclasses.is_disjoint(&other.subclasses)
            || !self.property_names.is_disjoint(&other.property_names)
            || !self.class_facts.is_disjoint(&other.class_facts)
            || !self.protocols.is_disjoint(&other.protocols)
    }
}

fn note(keys: &mut BTreeSet<String>, key: &str) {
    if !keys.contains(key) {
        keys.insert(key.to_owned());
    }
}

/// The keys analyses read while [`ProjectDecls::record`] runs.
#[derive(Debug, Default)]
struct Recorder {
    on: AtomicBool,
    keys: Mutex<DeclKeys>,
}

/// Declarations of every project module plus derived indexes.
#[derive(Debug, Default)]
pub struct ProjectDecls {
    modules: FxHashMap<String, ModuleDecls>,
    /// Class fqn -> direct project subclasses.
    subclasses: FxHashMap<String, Vec<String>>,
    /// Names of the property getters (`FunctionDecl::is_property`) of every class: an
    /// attribute read with another name runs no getter, without walking any class hierarchy.
    property_names: FxHashSet<String>,
    /// Every project class deriving (transitively) from `TypedDict`.
    typed_dicts: FxHashSet<String>,
    /// Every frozen project class (`Self::frozen_classes`).
    frozen: FxHashSet<String>,
    /// Every project protocol with the project classes implementing it structurally
    /// (`Self::protocol_implementations`), `None` past `CHA_CANDIDATE_LIMIT`.
    protocols: FxHashMap<String, Option<Vec<String>>>,
    recorder: Recorder,
    /// The C3 linearizations computed so far of classes with multiple inheritance along their
    /// bases (`Self::mro`); cleared with the indexes.
    linearized: RwLock<FxHashMap<String, Vec<String>>>,
}

/// Why an attribute (or `TypedDict` key) of a project class is read-only (`MUT108`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyKind {
    /// A field of a frozen class: a frozen dataclass, a `NamedTuple`, or a class that a
    /// `dataclass_transform(frozen_default=True)` builds.
    Frozen,
    /// Declared `Final[...]` or `ReadOnly[...]` (`ClassDecl::readonly_attrs`).
    Declared,
}

/// The declaration that makes an attribute read-only: the first class along the MRO that
/// declares it, by fqn, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyAttr {
    pub class: String,
    pub kind: ReadOnlyKind,
}

/// What a dotted fqn names inside the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Symbol<'a> {
    Module,
    Class(&'a ClassDecl),
    Function(FuncKey, &'a FunctionDecl),
    Global(&'a Ty),
    External,
}

impl ProjectDecls {
    pub fn new(modules: impl IntoIterator<Item = ModuleDecls>) -> Self {
        let mut out = Self::default();
        for m in modules {
            out.modules.insert(m.module_name.clone(), m);
        }
        out.rebuild_indexes();
        out
    }

    /// Replace (or add) one module's declarations; the derived indexes are rebuilt because a
    /// base may be canonicalized through any module.
    pub fn replace(&mut self, decls: ModuleDecls) {
        self.modules.insert(decls.module_name.clone(), decls);
        self.rebuild_indexes();
    }

    /// Forget a deleted module.
    pub fn remove(&mut self, module: &str) {
        if self.modules.remove(module).is_some() {
            self.rebuild_indexes();
        }
    }

    /// Run `f` and return, with its result, the keys it read of these declarations (through
    /// any lookup: names, classes and their hierarchies, functions, property names).
    pub fn record<T>(&self, f: impl FnOnce() -> T) -> (T, DeclKeys) {
        self.recorder.on.store(true, Ordering::SeqCst);
        let out = f();
        self.recorder.on.store(false, Ordering::SeqCst);
        let keys = std::mem::take(&mut *self.recorded());
        (out, keys)
    }

    fn recorded(&self) -> MutexGuard<'_, DeclKeys> {
        self.recorder
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Note a key read during [`Self::record`].
    fn read(&self, note: impl FnOnce(&mut DeclKeys)) {
        if self.recorder.on.load(Ordering::Relaxed) {
            note(&mut self.recorded());
        }
    }

    /// What differs in the derived indexes between `before` (an earlier version,
    /// [`Self::indexes`]) and now: the classes whose direct subclasses changed, the property
    /// names that appeared or disappeared, and the classes that became or stopped being frozen
    /// or a `TypedDict`.
    fn index_changes(&self, before: &Indexes) -> DeclKeys {
        let mut out = DeclKeys::default();
        for class in before.subclasses.keys().chain(self.subclasses.keys()) {
            if before.subclasses.get(class) != self.subclasses.get(class) {
                out.subclasses.insert(class.clone());
            }
        }
        let differ = |a: &FxHashSet<String>, b: &FxHashSet<String>| -> BTreeSet<String> {
            a.symmetric_difference(b).cloned().collect()
        };
        out.property_names = differ(&before.property_names, &self.property_names);
        out.class_facts = differ(&before.frozen, &self.frozen);
        out.class_facts
            .extend(differ(&before.typed_dicts, &self.typed_dicts));
        for p in before.protocols.keys().chain(self.protocols.keys()) {
            if before.protocols.get(p) != self.protocols.get(p) {
                out.protocols.insert(p.clone());
            }
        }
        out
    }

    fn indexes(&self) -> Indexes {
        Indexes {
            subclasses: self.subclasses.clone(),
            property_names: self.property_names.clone(),
            frozen: self.frozen.clone(),
            typed_dicts: self.typed_dicts.clone(),
            protocols: self.protocols.clone(),
        }
    }

    fn rebuild_indexes(&mut self) {
        self.linearized
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        // Maintenance, not a lookup of an analysis.
        let recording = std::mem::replace(self.recorder.on.get_mut(), false);
        self.build_indexes();
        *self.recorder.on.get_mut() = recording;
    }

    fn build_indexes(&mut self) {
        let out = &*self;
        let mut subclasses: FxHashMap<String, Vec<String>> = FxHashMap::default();
        let mut property_names = FxHashSet::default();
        for m in out.modules.values() {
            for (qualname, class) in &m.classes {
                let fqn = format!("{}.{qualname}", m.module_name);
                for base in &class.bases {
                    subclasses
                        .entry(out.canonical(base))
                        .or_default()
                        .push(fqn.clone());
                }
            }
            for (qualname, f) in &m.functions {
                if f.is_property || f.property_type.is_some() {
                    let name = qualname.rsplit('.').next().unwrap_or(qualname);
                    property_names.insert(name.to_owned());
                }
            }
        }
        for subs in subclasses.values_mut() {
            subs.sort();
            subs.dedup();
        }
        self.subclasses = subclasses;
        self.property_names = property_names;
        self.typed_dicts = TYPED_DICT_BASES
            .iter()
            .flat_map(|b| self.subclasses_of(b))
            .collect();
        self.frozen = self.frozen_classes();
        self.protocols = self.protocol_implementations();
    }

    /// For every project protocol, the project classes implementing it structurally: those
    /// whose members (along their project MRO) include every member the protocol and its
    /// protocol bases define, without a project base that implements it too (CHA reaches the
    /// subclasses from there). An explicit subclass is one of them. `None` for more than
    /// `CHA_CANDIDATE_LIMIT` (or a protocol without members).
    fn protocol_implementations(&self) -> FxHashMap<String, Option<Vec<String>>> {
        let mut protocols = Vec::new();
        let mut classes = Vec::new();
        for m in self.modules.values() {
            for (q, c) in &m.classes {
                let fqn = format!("{}.{q}", m.module_name);
                if c.is_protocol {
                    protocols.push(fqn);
                } else {
                    classes.push(fqn);
                }
            }
        }
        let mut out = FxHashMap::default();
        if protocols.is_empty() {
            return out;
        }
        // The members of each class along its project MRO (protocols: their protocol bases).
        let closure = |fqn: &str, only_protocols: bool| -> FxHashSet<&str> {
            let mut names = FxHashSet::default();
            for c in self.mro(fqn) {
                if let Some(d) = self.class(&c)
                    && (!only_protocols || d.is_protocol)
                {
                    names.extend(d.members.iter().map(String::as_str));
                }
            }
            names
        };
        let members: FxHashMap<&str, FxHashSet<&str>> = classes
            .iter()
            .map(|c| (c.as_str(), closure(c, false)))
            .collect();
        for p in &protocols {
            let mut required = closure(p, true);
            required.retain(|n| !IMPLICIT_MEMBERS.contains(n));
            if required.is_empty() {
                out.insert(p.clone(), None);
                continue;
            }
            let satisfies = |c: &str| {
                members
                    .get(c)
                    .is_some_and(|m| required.iter().all(|n| m.contains(n)))
            };
            let mut found: Vec<String> = classes
                .iter()
                .filter(|c| satisfies(c))
                .filter(|c| self.mro(c).iter().skip(1).all(|b| b == p || !satisfies(b)))
                .cloned()
                .collect();
            found.sort();
            let entry = (found.len() <= CHA_CANDIDATE_LIMIT).then_some(found);
            out.insert(p.clone(), entry);
        }
        out
    }

    /// The project classes implementing the project protocol `class` structurally
    /// (`Self::protocol_implementations`); `None` when `class` is no protocol or too many do.
    pub(crate) fn protocol_implementers(&self, class: &str) -> Option<&[String]> {
        self.read(|k| note(&mut k.protocols, class));
        self.protocols.get(class)?.as_deref()
    }

    /// The frozen project classes: `ClassDecl::is_frozen` (`@dataclass(frozen=True)`,
    /// `NamedTuple`), and the classes a PEP 681 `dataclass_transform(frozen_default=True)`
    /// builds unless they pass `frozen=False`: through a decorator function carrying it, or
    /// through a base class or metaclass carrying it (the classes deriving from it, or using it
    /// or a subclass of it as their metaclass, and their subclasses).
    fn frozen_classes(&self) -> FxHashSet<String> {
        let mut frozen = FxHashSet::default();
        let mut transforms = Vec::new();
        let mut metaclass_users = Vec::new();
        for m in self.modules.values() {
            for (q, c) in &m.classes {
                let by_decorator = c.decorators.iter().any(|(dec, frozen_arg)| {
                    match self.symbol(&self.canonical(dec)) {
                        Symbol::Function(_, d) => {
                            d.transform.is_some_and(|t| frozen_arg.unwrap_or(t))
                        }
                        _ => false,
                    }
                });
                let fqn = || format!("{}.{q}", m.module_name);
                if c.is_frozen || by_decorator {
                    frozen.insert(fqn());
                }
                if let Some(t) = c.transform {
                    transforms.push((fqn(), t));
                }
                if let Some(meta) = &c.metaclass {
                    metaclass_users.push((fqn(), self.canonical(meta)));
                }
            }
        }
        for (t, default) in transforms {
            let mut built = self.subclasses_of(&t);
            let users: Vec<String> = metaclass_users
                .iter()
                .filter(|(_, meta)| *meta == t || built.binary_search(meta).is_ok())
                .map(|(fqn, _)| fqn.clone())
                .collect();
            for u in users {
                built.extend(self.subclasses_of(&u));
                built.push(u);
            }
            for b in built {
                // The first `frozen=` class keyword along the MRO decides.
                let keyword = self
                    .mro(&b)
                    .iter()
                    .find_map(|c| self.class(c)?.frozen_keyword);
                if keyword.unwrap_or(default) {
                    frozen.insert(b);
                }
            }
        }
        frozen
    }

    pub fn module(&self, name: &str) -> Option<&ModuleDecls> {
        self.read(|k| note(&mut k.modules, name));
        self.modules.get(name)
    }

    /// Longest project-module prefix of `fqn` and the remaining dotted part.
    fn split_module<'s>(&self, fqn: &'s str) -> Option<(&ModuleDecls, &'s str)> {
        let mut end = fqn.len();
        loop {
            let prefix = &fqn[..end];
            if let Some(m) = self.module(prefix) {
                let rest = fqn.get(end + 1..).unwrap_or("");
                return Some((m, rest));
            }
            end = prefix.rfind('.')?;
        }
    }

    /// The direct project subclasses of the canonical class `class`.
    fn direct_subclasses(&self, class: &str) -> &[String] {
        self.read(|k| note(&mut k.subclasses, class));
        self.subclasses.get(class).map_or(&[], Vec::as_slice)
    }

    /// Chase re-exports (`pkg.Foo` where `pkg/__init__.py` does `from .sub import Foo`).
    pub fn canonical(&self, fqn: &str) -> String {
        let mut cur = fqn.to_owned();
        for _ in 0..MAX_ALIAS_CHASE {
            let Some((m, rest)) = self.split_module(&cur) else {
                break;
            };
            if rest.is_empty() {
                break;
            }
            let (first, tail) = match rest.split_once('.') {
                Some((f, t)) => (f, Some(t)),
                None => (rest, None),
            };
            if m.classes.contains_key(first) || m.functions.contains_key(first) {
                break;
            }
            let Some(target) = m.imports.get(first) else {
                break;
            };
            let next = match tail {
                Some(t) => format!("{target}.{t}"),
                None => target.clone(),
            };
            if next == cur {
                break;
            }
            cur = next;
        }
        cur
    }

    pub(crate) fn symbol(&self, fqn: &str) -> Symbol<'_> {
        let Some((m, rest)) = self.split_module(fqn) else {
            return Symbol::External;
        };
        if rest.is_empty() {
            return Symbol::Module;
        }
        if let Some(c) = m.classes.get(rest) {
            return Symbol::Class(c);
        }
        if let Some(f) = m.functions.get(rest) {
            let key = FuncKey {
                module: m.module_name.clone(),
                qualname: rest.to_owned(),
            };
            return Symbol::Function(key, f);
        }
        if let Some(t) = m.global_types.get(rest) {
            return Symbol::Global(t);
        }
        Symbol::External
    }

    fn class(&self, fqn: &str) -> Option<&ClassDecl> {
        match self.symbol(fqn) {
            Symbol::Class(c) => Some(c),
            _ => None,
        }
    }

    pub fn is_project_class(&self, fqn: &str) -> bool {
        self.class(fqn).is_some()
    }

    /// Whether the canonical project class `class` is a `TypedDict`.
    pub fn is_typed_dict(&self, class: &str) -> bool {
        self.read(|k| note(&mut k.class_facts, class));
        self.typed_dicts.contains(class)
    }

    /// The small display the module global `fqn` of a project module holds
    /// (`ModuleDecls::small_globals`), through re-exports.
    pub fn small_global(&self, fqn: &str) -> Option<&SmallDisplay> {
        let fqn = self.canonical(fqn);
        let (m, rest) = self.split_module(&fqn)?;
        m.small_globals.get(rest)
    }

    /// The small display attribute `attr` of an instance of the project class `class` holds:
    /// `class` and its project subclasses store it at least once and only ever small displays
    /// that none of them grows (`ClassDecl::attr_displays`; the kinds and the longest one).
    pub fn small_attr(&self, class: &str, attr: &str) -> Option<SmallDisplay> {
        let class = self.canonical(class);
        let mut family = self.subclasses_of(&class);
        family.push(class);
        let mut out: Option<SmallDisplay> = None;
        for c in &family {
            match self.class(c)?.attr_displays.get(attr) {
                Some(Some(d)) => {
                    let merged = out.get_or_insert_with(|| SmallDisplay {
                        kinds: Vec::new(),
                        len: 0,
                    });
                    merged.len = merged.len.max(d.len);
                    for k in &d.kinds {
                        if !merged.kinds.contains(k) {
                            merged.kinds.push(*k);
                        }
                    }
                }
                Some(None) => return None,
                None => {}
            }
        }
        out
    }

    /// The module-relative qualname of the project class `fqn` (`Outer.Inner`).
    pub fn class_qualname<'f>(&self, fqn: &'f str) -> Option<&'f str> {
        let (m, rest) = self.split_module(fqn)?;
        m.classes.contains_key(rest).then_some(rest)
    }

    /// Why attribute `attr` of an instance of `class` is read-only (`MUT108`): the first class
    /// along the MRO that has it as a field of a frozen class, or declares it `Final` /
    /// `ReadOnly`.
    pub fn readonly_attr(&self, class: &str, attr: &str) -> Option<ReadOnlyAttr> {
        self.mro(class).into_iter().find_map(|c| {
            let decl = self.class(&c)?;
            self.read(|k| note(&mut k.class_facts, &c));
            let kind = if self.frozen.contains(&c) && decl.container_fields.contains(attr) {
                ReadOnlyKind::Frozen
            } else if decl.readonly_attrs.contains(attr) {
                ReadOnlyKind::Declared
            } else {
                return None;
            };
            Some(ReadOnlyAttr { class: c, kind })
        })
    }

    /// Every project class that (transitively) inherits from `class`, sorted.
    pub fn subclasses_of(&self, class: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = FxHashSet::default();
        let mut stack = vec![self.canonical(class)];
        while let Some(c) = stack.pop() {
            for sub in self.direct_subclasses(&c) {
                if seen.insert(sub.clone()) {
                    out.push(sub.clone());
                    stack.push(sub.clone());
                }
            }
        }
        out.sort();
        out
    }

    /// The project classes `class` inherits from (its MRO without itself and external bases).
    pub fn ancestors_of(&self, class: &str) -> Vec<String> {
        let class = self.canonical(class);
        self.mro(&class)
            .into_iter()
            .filter(|c| *c != class && self.is_project_class(c))
            .collect()
    }

    /// The methods defined directly in the project class `class`.
    pub fn methods_of(&self, class: &str) -> Vec<FuncKey> {
        self.split_module(class)
            .into_iter()
            .flat_map(|(m, rest)| {
                let prefix = format!("{rest}.");
                m.functions
                    .keys()
                    .filter(move |q| {
                        q.strip_prefix(&prefix)
                            .is_some_and(|name| !name.contains('.'))
                    })
                    .map(|q| FuncKey {
                        module: m.module_name.clone(),
                        qualname: q.clone(),
                    })
            })
            .collect()
    }

    /// Whether `sub` derives (transitively) from `base`, both canonical.
    pub(crate) fn is_subclass(&self, sub: &str, base: &str) -> bool {
        self.mro(sub).iter().skip(1).any(|c| c == base)
    }

    /// The method resolution order of `class`: Python's C3 linearization of its project bases;
    /// an external base stands for itself (its bases are not known). A hierarchy C3 cannot
    /// linearize (Python rejects the class statement, or a cycle) keeps the depth-first,
    /// left-to-right order.
    fn mro(&self, class: &str) -> Vec<String> {
        let class = self.canonical(class);
        // The depth-first walk reads the declarations the linearization depends on.
        let (order, multiple) = self.depth_first_bases(&class);
        // Without multiple inheritance, the depth-first order is the linearization.
        if !multiple {
            return order;
        }
        let known = self
            .linearized
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&class)
            .cloned();
        if let Some(mro) = known {
            return mro;
        }
        let mut done = FxHashMap::default();
        let mro = self.c3(&class, &mut done, &mut Vec::new()).unwrap_or(order);
        self.linearized
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(class, mro.clone());
        mro
    }

    /// [`Self::mro`] by C3: `class` followed by the merge of its bases' linearizations and the
    /// list of its bases. `done` holds the linearizations computed so far, `open` the classes
    /// being linearized (a cycle has none).
    fn c3(
        &self,
        class: &str,
        done: &mut FxHashMap<String, Vec<String>>,
        open: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        if let Some(mro) = done.get(class) {
            return Some(mro.clone());
        }
        if open.iter().any(|c| c == class) {
            return None;
        }
        let bases: Vec<String> = self
            .class(class)
            .map(|d| d.bases.iter().map(|b| self.canonical(b)).collect())
            .unwrap_or_default();
        open.push(class.to_owned());
        let mut seqs: Vec<std::collections::VecDeque<String>> = Vec::new();
        for base in &bases {
            seqs.push(self.c3(base, done, open)?.into());
        }
        open.pop();
        seqs.push(bases.into());
        let mut out = vec![class.to_owned()];
        loop {
            seqs.retain(|s| !s.is_empty());
            let Some(head) = seqs
                .iter()
                .map(|s| &s[0])
                .find(|h| !seqs.iter().any(|s| s.iter().skip(1).any(|c| c == *h)))
                .cloned()
            else {
                break;
            };
            for s in &mut seqs {
                if s.front() == Some(&head) {
                    s.pop_front();
                }
            }
            out.push(head);
        }
        if !seqs.is_empty() {
            return None;
        }
        done.insert(class.to_owned(), out.clone());
        Some(out)
    }

    /// Depth-first, left-to-right base order (project and external classes), and whether a
    /// class along it has more than one base.
    fn depth_first_bases(&self, class: &str) -> (Vec<String>, bool) {
        let mut out = Vec::new();
        let mut multiple = false;
        let mut seen = FxHashSet::default();
        let mut stack = vec![class.to_owned()];
        while let Some(c) = stack.pop() {
            if !seen.insert(c.clone()) {
                continue;
            }
            if let Some(decl) = self.class(&c) {
                multiple |= decl.bases.len() > 1;
                for base in decl.bases.iter().rev() {
                    stack.push(self.canonical(base));
                }
            }
            out.push(c);
        }
        (out, multiple)
    }

    /// The method `name` defined directly in project class `class`.
    fn own_method(&self, class: &str, name: &str) -> Option<FuncKey> {
        let (m, rest) = self.split_module(class)?;
        let qualname = format!("{rest}.{name}");
        m.functions.contains_key(&qualname).then(|| FuncKey {
            module: m.module_name.clone(),
            qualname,
        })
    }

    pub fn function_decl(&self, key: &FuncKey) -> Option<&FunctionDecl> {
        self.module(&key.module)?.functions.get(&key.qualname)
    }

    /// The declared type of reading `attr` on an instance of `class`, from the first class along
    /// the MRO (past `class` itself with `skip_self`, for `super()`) that declares it: the
    /// return annotation of a property getter (with or without a setter; another function of a
    /// name some project property has gives none), or the Priority 3 attribute type. Overriding
    /// subclasses do not count, as for a type checker. The getter's module is a declaration
    /// read like any other (`DeclKeys::modules`).
    pub(crate) fn member_type(&self, class: &str, attr: &str, skip_self: bool) -> Option<&Ty> {
        self.read(|k| note(&mut k.property_names, attr));
        let property = self.property_names.contains(attr);
        for c in self.mro(class).iter().skip(usize::from(skip_self)) {
            if property && let Some(key) = self.own_method(c, attr) {
                return self.function_decl(&key)?.property_type.as_ref();
            }
            if let Some(t) = self.class(c).and_then(|d| d.attr_types.get(attr)) {
                return Some(t);
            }
        }
        None
    }

    /// The definition of method `name` a call on an instance of `class` finds first along the
    /// MRO (past `class` itself with `skip_self`): what a type checker takes the call's type
    /// from, whatever subclasses override it with.
    pub(crate) fn mro_definition(
        &self,
        class: &str,
        name: &str,
        skip_self: bool,
    ) -> Option<FuncKey> {
        self.mro(class)
            .iter()
            .skip(usize::from(skip_self))
            .find_map(|c| self.own_method(c, name))
    }

    /// The type a declaration names (an annotation, or a type stored in the declarations), with
    /// every class named canonically (re-exports chased): `None` when it names a module, or a name
    /// of a project module that is no class (a type alias, a `TypeVar`, a constant); an item of
    /// such a type is unknown. A name below a module heki does not see (a compiled submodule of a
    /// project package) stays as it is.
    pub(crate) fn declared_ty(&self, ty: &Ty) -> Option<Ty> {
        ty.map_members(|o| {
            let class = self.canonical(&o.class);
            if let Some((m, rest)) = self.split_module(&class)
                && !rest.contains('.')
                && !m.classes.contains_key(rest)
            {
                return None;
            }
            let args = o.args.map_elems(|t| self.declared_ty(t));
            Some(crate::types::Obj { class, args })
        })
    }

    /// Whether the inferred type `t` is narrower than the declared `declared`, so that a value
    /// keeps it: anything for a declaration of `Any` / `object`, and an instance of a project
    /// subclass of the declared class (`x: Base = Sub()`).
    pub(crate) fn narrower(&self, t: &Ty, declared: &Ty) -> bool {
        declared.obj().is_some_and(|d| {
            matches!(d.class.as_str(), "typing.Any" | "builtins.object")
                || t.obj()
                    .is_some_and(|i| i.class != d.class && self.is_subclass(&i.class, &d.class))
        })
    }

    /// The project functions `name` names on an instance of the canonical project class
    /// `class`: the first definition along the MRO (past `class` itself with `skip_self`, for
    /// `super()`) and, with `overrides`, the definitions of every overriding project subclass
    /// (Priority 4 CHA), each one kept when `keep` accepts it (sorted, deduplicated). Also
    /// returns the first external base the MRO walk meets before a definition.
    fn definitions(
        &self,
        class: &str,
        name: &str,
        skip_self: bool,
        overrides: bool,
        keep: impl Fn(&FuncKey) -> bool,
    ) -> (Vec<FuncKey>, Option<String>) {
        let mro = self.mro(class);
        let start = usize::from(skip_self);
        let mut found = Vec::new();
        let mut external = None;
        for c in mro.iter().skip(start) {
            if let Some(k) = self.own_method(c, name) {
                if keep(&k) {
                    found.push(k);
                }
                break;
            }
            if !self.is_project_class(c) && external.is_none() {
                external = Some(format!("{c}.{name}"));
            }
        }
        if overrides {
            let mut stack = vec![class.to_owned()];
            let mut seen = FxHashSet::default();
            while let Some(c) = stack.pop() {
                for sub in self.direct_subclasses(&c) {
                    if seen.insert(sub.clone()) {
                        if let Some(k) = self.own_method(sub, name)
                            && keep(&k)
                        {
                            found.push(k);
                        }
                        stack.push(sub.clone());
                    }
                }
            }
        }
        found.sort();
        found.dedup();
        (found, external)
    }

    /// Resolve `instance_of(class).method(...)`: MRO definition plus overriding project
    /// subclasses (Priority 4 CHA). External bases supply a Layer 2 symbol fallback.
    pub(crate) fn method_targets(&self, class: &str, method: &str, skip_self: bool) -> Callee {
        let class = self.canonical(class);
        if !self.is_project_class(&class) {
            return Callee::External(format!("{class}.{method}"));
        }
        let (found, external) = self.definitions(&class, method, skip_self, !skip_self, |_| true);
        if found.len() > CHA_CANDIDATE_LIMIT {
            return Callee::TooMany;
        }
        if found.is_empty() {
            return external.map_or(Callee::Unresolved, Callee::External);
        }
        Callee::Project(found, true)
    }

    /// Resolve `class.method` on the class object `class` itself (`Base.method(obj)`, and the
    /// `__init__` of constructing it): the definition its MRO finds first. The overrides of
    /// subclasses do not count: the class object is no subclass.
    pub(crate) fn class_targets(&self, class: &str, method: &str) -> Callee {
        let class = self.canonical(class);
        if !self.is_project_class(&class) {
            return Callee::External(format!("{class}.{method}"));
        }
        match self.definitions(&class, method, false, false, |_| true) {
            (found, _) if !found.is_empty() => Callee::Project(found, true),
            (_, external) => external.map_or(Callee::Unresolved, Callee::External),
        }
    }

    /// Resolve reading `instance_of(class).attr` as calls of the property getters it may run
    /// (`FunctionDecl::is_property`), found like [`Self::method_targets`]' candidates: the MRO
    /// definition when it is a getter, plus the getters of overriding project subclasses
    /// (Priority 4 CHA), at most `CHA_CANDIDATE_LIMIT` of them (`TooMany` beyond). `None` when
    /// no modeled getter runs: a plain attribute, a method, an attribute of an external class
    /// (never a Layer 2 symbol: the catalog describes calls), or a property with a setter or
    /// deleter (`FunctionDecl::is_property`).
    pub(crate) fn property_targets(
        &self,
        class: &str,
        attr: &str,
        skip_self: bool,
    ) -> Option<Callee> {
        self.read(|k| note(&mut k.property_names, attr));
        if !self.property_names.contains(attr) {
            return None;
        }
        let class = self.canonical(class);
        if !self.is_project_class(&class) {
            return None;
        }
        let is_getter = |k: &FuncKey| self.function_decl(k).is_some_and(|d| d.is_property);
        let (found, _) = self.definitions(&class, attr, skip_self, !skip_self, is_getter);
        match found.len() {
            0 => None,
            n if n > CHA_CANDIDATE_LIMIT => Some(Callee::TooMany),
            _ => Some(Callee::Project(found, true)),
        }
    }
}

/// Both passes of [`project_decls_for`] over the whole project, kept up to date one module at
/// a time: pass 1 (`structural`: imports, classes and their bases, signatures; a function of
/// each module's text alone) and pass 2 (`completed`: pass 1 plus the instance attribute and
/// module global types inferred against pass 1). Inference never sees inferred types, so a
/// module's completed declarations do not depend on the order in which modules were edited.
#[derive(Debug, Default)]
pub struct DeclPasses {
    structural: ProjectDecls,
    completed: ProjectDecls,
}

impl DeclPasses {
    /// From the completed declarations of every module (`.py` implementations after `.pyi`
    /// stubs sharing a module name, so that they win).
    pub fn new(completed: impl IntoIterator<Item = ModuleDecls>) -> Self {
        let completed: Vec<ModuleDecls> = completed.into_iter().collect();
        Self {
            structural: ProjectDecls::new(completed.iter().map(ModuleDecls::structural)),
            completed: ProjectDecls::new(completed),
        }
    }

    /// The declarations summaries and rules resolve names through.
    pub fn completed(&self) -> &ProjectDecls {
        &self.completed
    }

    /// Replace (or add) one module's completed declarations; returns what changed.
    pub fn replace(&mut self, completed: ModuleDecls) -> DeclKeys {
        let name = completed.module_name.clone();
        if self.completed.module(&name) == Some(&completed) {
            return DeclKeys::default();
        }
        let before = self.structural.indexes();
        self.structural.replace(completed.structural());
        self.completed.replace(completed);
        self.changed(&name, Some(&before))
    }

    /// Forget a deleted module; returns what changed.
    pub fn remove(&mut self, module: &str) -> DeclKeys {
        let before = self.structural.indexes();
        self.structural.remove(module);
        self.completed.remove(module);
        self.changed(module, Some(&before))
    }

    /// What changed when `module`'s declarations did: the module, and the hierarchy indexes
    /// that differ from `before` (unchanged without it: the module's structure stayed).
    fn changed(&self, module: &str, before: Option<&Indexes>) -> DeclKeys {
        let mut out = before.map_or_else(DeclKeys::default, |b| self.structural.index_changes(b));
        out.modules.insert(module.to_owned());
        out
    }
}

/// Complete one edited module against pass 1 of the project, as [`project_decls_for`] does,
/// and update both passes when they changed. Returns what changed (nothing: empty), the
/// module's completed declarations, and what the completion read of pass 1.
pub(crate) fn update_module_decls(
    input: &ModuleInput<'_>,
    kb: &EffectiveEcosystemKb,
    passes: &mut DeclPasses,
) -> (DeclKeys, ModuleDecls, DeclKeys) {
    let name = &input.module_name;
    let structural = structural_decls(input);
    let restructured = passes.structural.module(name) != Some(&structural);
    let before = restructured.then(|| passes.structural.indexes());
    if restructured {
        passes.structural.replace(structural.clone());
    }
    let (completed, reads) = complete_against(input, structural, &passes.structural, kb);
    if passes.completed.module(name) == Some(&completed) {
        return (DeclKeys::default(), completed, reads);
    }
    passes.completed.replace(completed.clone());
    (passes.changed(name, before.as_ref()), completed, reads)
}

/// A module's completed declarations against pass 1 without updating it (a `.pyi` stub next to
/// its `.py` implementation, whose declarations win), and what the completion read.
pub(crate) fn own_module_decls(
    input: &ModuleInput<'_>,
    kb: &EffectiveEcosystemKb,
    passes: &DeclPasses,
) -> (ModuleDecls, DeclKeys) {
    complete_against(input, structural_decls(input), &passes.structural, kb)
}

/// [`complete_decls`] with what it read of `pass1`.
fn complete_against(
    input: &ModuleInput<'_>,
    structural: ModuleDecls,
    pass1: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
) -> (ModuleDecls, DeclKeys) {
    pass1.record(|| complete_decls(input, structural, pass1, kb))
}

/// Build the project declarations used for summarization: structural declarations of
/// `modules` (plus the pass 1 of `others`, whose completed declarations are known), then
/// Priority 3 attribute types and global types inferred against them. Also returns each
/// module's own completed declarations, with what its completion read when `track` is set.
pub(crate) fn project_decls_for(
    modules: &[ModuleInput<'_>],
    kb: &EffectiveEcosystemKb,
    others: Option<Vec<(bool, ModuleDecls)>>,
    track: bool,
) -> (ProjectDecls, Vec<(ModuleDecls, DeclKeys)>) {
    // Process stubs before implementation `.py` files so a `.py` module always wins when both
    // share one module name.
    let order = |items: Vec<(bool, ModuleDecls)>| {
        let mut items = items;
        items.sort_by_key(|(stub, _)| !*stub);
        items.into_iter().map(|(_, d)| d)
    };
    let structural: Vec<(bool, ModuleDecls)> = modules
        .iter()
        .map(|m| (crate::is_stub(&m.path), structural_decls(m)))
        .collect();
    let others = others.unwrap_or_default();
    let pass1 = ProjectDecls::new(order(
        others
            .iter()
            .map(|(stub, d)| (*stub, d.structural()))
            .chain(structural.iter().cloned())
            .collect(),
    ));
    let own: Vec<(ModuleDecls, DeclKeys)> = modules
        .iter()
        .zip(&structural)
        .map(|(m, (_, d))| {
            if track {
                complete_against(m, d.clone(), &pass1, kb)
            } else {
                (
                    complete_decls(m, d.clone(), &pass1, kb),
                    DeclKeys::default(),
                )
            }
        })
        .collect();
    let completed = structural
        .iter()
        .zip(&own)
        .map(|((stub, _), (d, _))| (*stub, d.clone()));
    let decls = ProjectDecls::new(order(others.into_iter().chain(completed).collect()));
    (decls, own)
}

/// Pass 1: imports, classes (bases), functions (signatures) — no inference.
fn structural_decls(input: &ModuleInput<'_>) -> ModuleDecls {
    let semantic = input.semantic;
    let module_scope = module_scope(semantic);
    let mut decls = ModuleDecls {
        module_name: input.module_name.clone(),
        is_package: input.is_package,
        ..ModuleDecls::default()
    };
    for imp in &semantic.imports {
        if Some(imp.scope) != module_scope {
            continue;
        }
        let Some(target) = import_target(imp, &input.module_name, input.is_package) else {
            continue;
        };
        if imp.in_type_checking {
            decls
                .type_checking_imports
                .insert(imp.bound_name.clone(), target);
        } else {
            decls.imports.insert(imp.bound_name.clone(), target);
        }
    }
    for class in &semantic.classes {
        decls
            .classes
            .insert(class.qualname.clone(), ClassDecl::default());
    }
    // Qualnames whose every definition is a property getter (a setter, a deleter, or any other
    // function with the same qualname shares its summary).
    let mut getters_only: FxHashMap<&str, bool> = FxHashMap::default();
    for f in &input.ir.functions {
        if !is_summarized_function(f) {
            continue;
        }
        decls
            .functions
            .insert(f.qualname.clone(), FunctionDecl::default());
        let getter = f.owner_class_qualname.is_some() && f.has_decorator(PROPERTY_DECORATORS);
        *getters_only.entry(&f.qualname).or_insert(true) &= getter;
    }
    // Resolve bases, class keywords, and annotations now that local names are known.
    for class in &semantic.classes {
        let bases: Vec<String> = class
            .bases
            .iter()
            .filter_map(|b| {
                let head = b.split('[').next().unwrap_or(b).trim();
                decls.resolve_dotted(head, false)
            })
            .collect();
        let keyword = |name: &str| {
            class
                .keywords
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        let metaclass = keyword("metaclass").and_then(|m| decls.resolve_dotted(m, false));
        let frozen_keyword = keyword("frozen").and_then(bool_literal);
        let is_protocol = bases
            .iter()
            .any(|b| matches!(b.as_str(), "typing.Protocol" | "typing_extensions.Protocol"));
        let mut members: BTreeSet<String> = class.methods.iter().cloned().collect();
        let bindings = semantic
            .scopes
            .get(class.scope.0 as usize)
            .filter(|sc| sc.id == class.scope)
            .into_iter()
            .flat_map(|sc| &sc.bindings);
        members.extend(
            bindings
                .filter(|b| !b.is_global_decl && !b.is_nonlocal_decl)
                .map(|b| b.name.clone()),
        );
        if let Some(c) = decls.classes.get_mut(&class.qualname) {
            c.bases = bases;
            c.metaclass = metaclass;
            c.frozen_keyword = frozen_keyword;
            c.is_protocol = is_protocol;
            c.members = members;
        }
    }
    // Class-body annotations without a value, and what methods set or annotate on `self`.
    for f in &input.ir.functions {
        if f.is_class_body {
            if let Some(c) = decls.classes.get_mut(&f.qualname) {
                c.members.extend(
                    f.local_annotations
                        .iter()
                        .filter(|a| !a.target.contains('.'))
                        .map(|a| a.target.clone()),
                );
            }
            continue;
        }
        let (Some(owner), Some(first)) = (&f.owner_class_qualname, f.params.first()) else {
            continue;
        };
        if f.has_decorator(&["staticmethod", "classmethod"]) {
            continue;
        }
        let Some(c) = decls.classes.get_mut(owner) else {
            continue;
        };
        let prefix = format!("{}.", first.name);
        c.members.extend(f.local_annotations.iter().filter_map(|a| {
            let attr = a.target.strip_prefix(&prefix)?;
            (!attr.contains('.')).then(|| attr.to_owned())
        }));
        let self_vars = self_aliases(f, first.var);
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::AttrStore { obj, attr, .. } = &inst.kind
                && self_vars.contains(obj)
            {
                c.members.insert(attr.clone());
            }
        }
    }
    // PEP 681 `dataclass_transform` on a decorator function: any definition (an overload or the
    // implementation) carries it; `Some(true)` (`frozen_default`) wins.
    let mut transforms: FxHashMap<&str, Option<bool>> = FxHashMap::default();
    for f in input
        .ir
        .functions
        .iter()
        .filter(|f| is_summarized_function(f))
    {
        let resolve = |name: &str| decls.resolve_dotted(name, true);
        let t = f
            .decorators
            .iter()
            .find_map(|d| transform_decorator(d, &resolve));
        let slot = transforms.entry(&f.qualname).or_insert(None);
        *slot = (*slot).max(t);
    }
    // The return annotation of a property's getter (its setter shares the qualname).
    let mut getter_types: FxHashMap<&str, Ty> = FxHashMap::default();
    for f in &input.ir.functions {
        if is_summarized_function(f)
            && f.owner_class_qualname.is_some()
            && f.has_decorator(PROPERTY_DECORATORS)
            && let Some(t) = f
                .return_annotation
                .as_deref()
                .and_then(|a| decls.resolve_annotation_ty(a))
        {
            getter_types.entry(&f.qualname).or_insert(t);
        }
    }
    for f in &input.ir.functions {
        if !is_summarized_function(f) {
            continue;
        }
        let is_staticmethod = f.has_decorator(&["staticmethod"]);
        let (readonly_params, mutable_params) = param_accesses(f, &decls);
        let decl = FunctionDecl {
            is_async: matches!(f.kind, FunctionKind::Async | FunctionKind::AsyncGenerator),
            is_staticmethod,
            is_classmethod: f.has_decorator(&["classmethod"]),
            is_method: f.owner_class_qualname.is_some(),
            return_type: f
                .return_annotation
                .as_deref()
                .and_then(|a| decls.resolve_annotation_ty(a)),
            param_names: f.params.iter().map(|p| p.name.clone()).collect(),
            positional_params: count_u8(
                f.params
                    .iter()
                    .take_while(|p| !p.kw_only && !p.is_vararg && !p.is_kwarg),
            ),
            named_params: count_u8(f.params.iter().filter(|p| !p.is_vararg && !p.is_kwarg)),
            defaults: f
                .params
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    let c = crate::ConstArg::of_literal(p.default.as_ref()?)?;
                    Some((u8::try_from(i).ok()?, c))
                })
                .collect(),
            is_generator: is_generator_function(f),
            is_property: getters_only[f.qualname.as_str()],
            readonly_params,
            mutable_params,
            transform: transforms[f.qualname.as_str()],
            offloaded_by: None,
            property_type: getter_types.get(f.qualname.as_str()).cloned(),
        };
        decls.functions.insert(f.qualname.clone(), decl);
    }
    readonly_metadata(input, &mut decls);
    decls.small_globals = input.ir.constants.displays.clone();
    attr_displays(input, &mut decls);
    decls
}

/// `ClassDecl::attr_displays` of the module's classes: the stores of their bodies and methods
/// (through `self`), and their in-place changes, decided by the shared `heki_ir` facts.
fn attr_displays(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    for f in &input.ir.functions {
        let (owner, selfs) = if f.is_class_body {
            (f.qualname.as_str(), FxHashSet::default())
        } else {
            match (&f.owner_class_qualname, f.params.first()) {
                (Some(owner), Some(p))
                    if is_summarized_function(f)
                        && !f.has_decorator(&["staticmethod", "classmethod"]) =>
                {
                    (owner.as_str(), self_aliases(f, p.var))
                }
                _ => continue,
            }
        };
        // The semantic model declares every class the IR has (one AST).
        if let Some(class) = decls.classes.get_mut(owner) {
            note_function_attrs(input, f, &selfs, &mut class.attr_displays);
        }
    }
}

/// [`attr_displays`] of one class body or method (`selfs`: the variables holding `self`).
fn note_function_attrs(
    input: &ModuleInput<'_>,
    f: &IrFunction,
    selfs: &FxHashSet<VarId>,
    map: &mut BTreeMap<String, Option<SmallDisplay>>,
) {
    let ix = FnIndex::new(f, &input.ir.constants);
    let grown = ix.grown_vars();
    for bb in &f.blocks {
        for (i, inst) in bb.insts.iter().enumerate() {
            match &inst.kind {
                // A class attribute: `attr = [...]` in the class body.
                InstKind::Copy { dst, src } if f.is_class_body => {
                    if let Some(attr) = f.var_name(*dst).filter(|n| !n.starts_with('.')) {
                        note_attr(map, attr, ix.stored_display(*src, (bb.id, i)));
                    }
                }
                InstKind::AttrStore { obj, attr, value } if selfs.contains(obj) => {
                    note_attr(map, attr, ix.stored_display(*value, (bb.id, i)));
                }
                InstKind::AttrDelete { obj, attr } if selfs.contains(obj) => {
                    note_attr(map, attr, None);
                }
                InstKind::AttrLoad { dst, obj, attr }
                    if selfs.contains(obj) && grown.contains(dst) =>
                {
                    note_attr(map, attr, None);
                }
                _ => {}
            }
        }
    }
}

/// Record one store (`Some`: of a small display) or change (`None`) of `attr`.
fn note_attr(
    map: &mut BTreeMap<String, Option<SmallDisplay>>,
    attr: &str,
    event: Option<SmallDisplay>,
) {
    let Some(slot) = map.get_mut(attr) else {
        map.insert(attr.to_owned(), event);
        return;
    };
    *slot = match (slot.take(), event) {
        (Some(mut a), Some(b)) => {
            a.len = a.len.max(b.len);
            for k in b.kinds {
                if !a.kinds.contains(&k) {
                    a.kinds.push(k);
                }
            }
            Some(a)
        }
        _ => None,
    };
}

/// The length of `items`, saturated to `u8` (positions past 255 never bind exactly).
fn count_u8<T>(items: impl Iterator<Item = T>) -> u8 {
    u8::try_from(items.count()).unwrap_or(u8::MAX)
}

/// `FunctionDecl::readonly_params` and `FunctionDecl::mutable_params` of `f`.
fn param_accesses(f: &IrFunction, decls: &ModuleDecls) -> (u64, u64) {
    let resolve = |name: &str| decls.resolve_dotted(name, true);
    let (mut readonly, mut mutable) = (0, 0);
    for (i, p) in f.params.iter().enumerate().take(64) {
        let access = p
            .annotation
            .as_deref()
            .filter(|_| !p.is_vararg && !p.is_kwarg)
            .and_then(|a| param_access(a, &resolve));
        match access {
            Some(ParamAccess::ReadOnly) => readonly |= 1u64 << i,
            Some(ParamAccess::Mutable) => mutable |= 1u64 << i,
            None => {}
        }
    }
    (readonly, mutable)
}

/// `MUT108` metadata of the module's classes (`ClassDecl::is_frozen`, `container_fields`,
/// `readonly_attrs`, `decorators`, and `transform`), resolved through the module's imports.
fn readonly_metadata(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    struct Found {
        qualname: String,
        is_frozen: bool,
        fields: BTreeSet<String>,
        readonly: BTreeSet<String>,
        decorators: Vec<(String, Option<bool>)>,
        transform: Option<bool>,
    }
    let resolve = |name: &str| decls.resolve_dotted(name, true);
    let mut found: Vec<Found> = Vec::new();
    for body in input.ir.functions.iter().filter(|f| f.is_class_body) {
        let dataclass = body
            .decorators
            .iter()
            .find_map(|d| dataclass_decorator(d, &resolve));
        let named_tuple = decls.classes.get(&body.qualname).is_some_and(|c| {
            c.bases
                .iter()
                .any(|b| NAMED_TUPLE_BASES.contains(&b.as_str()))
        });
        let (mut fields, mut readonly) = (BTreeSet::new(), BTreeSet::new());
        for ann in &body.local_annotations {
            let (q, container) = container_annotation(&ann.annotation, &resolve);
            if ann.target.contains('.') || !container {
                continue;
            }
            if !compares_false(body, ann.span, &resolve) {
                fields.insert(ann.target.clone());
            }
            // `Final` in the body of other classes declares a class-level constant.
            if q.read_only || q.is_final && (named_tuple || dataclass.is_some()) {
                readonly.insert(ann.target.clone());
            }
        }
        let decorators = body
            .decorators
            .iter()
            .filter_map(|d| {
                let (head, args) = decorator_parts(d);
                Some((resolve(head)?, keyword_bool(args, "frozen")))
            })
            .collect();
        found.push(Found {
            qualname: body.qualname.clone(),
            is_frozen: named_tuple || dataclass == Some(true),
            fields,
            readonly,
            decorators,
            transform: body
                .decorators
                .iter()
                .find_map(|d| transform_decorator(d, &resolve)),
        });
    }
    // Instance attributes declared `self.x: Final[...]` in `__init__`.
    for f in &input.ir.functions {
        if let (Some(owner), Some(self_param)) = (&f.owner_class_qualname, f.params.first())
            && f.name == "__init__"
            && is_summarized_function(f)
            && !f.has_decorator(&["staticmethod", "classmethod"])
        {
            let prefix = format!("{}.", self_param.name);
            let readonly = f
                .local_annotations
                .iter()
                .filter_map(|a| {
                    let (q, container) = container_annotation(&a.annotation, &resolve);
                    let attr = a.target.strip_prefix(&prefix)?;
                    (!attr.contains('.') && q.is_final && container).then(|| attr.to_owned())
                })
                .collect();
            found.push(Found {
                qualname: owner.clone(),
                is_frozen: false,
                fields: BTreeSet::new(),
                readonly,
                decorators: Vec::new(),
                transform: None,
            });
        }
    }
    for f in found {
        if let Some(c) = decls.classes.get_mut(&f.qualname) {
            c.is_frozen |= f.is_frozen;
            c.container_fields.extend(f.fields);
            c.readonly_attrs.extend(f.readonly);
            c.decorators.extend(f.decorators);
            c.transform = c.transform.max(f.transform);
        }
    }
}

/// Whether the class-body statement at `span` assigns `field(..., compare=False)`
/// (`dataclasses.field`): the field is state outside the instance's value.
fn compares_false(
    body: &IrFunction,
    span: Span,
    resolve: &impl Fn(&str) -> Option<String>,
) -> bool {
    let insts = || body.blocks.iter().flat_map(|b| &b.insts);
    insts()
        .filter(|i| span.start <= i.span.start && i.span.end <= span.end)
        .any(|i| {
            let (name, kwargs) = match &i.kind {
                InstKind::Call { callee, kwargs, .. } => (Some(callee.clone()), kwargs),
                // `dataclasses.field(...)`: the receiver is a module-level name.
                InstKind::MethodCall {
                    recv,
                    method,
                    kwargs,
                    ..
                } => {
                    let module = insts().find_map(|j| match &j.kind {
                        InstKind::LoadGlobal { dst, name, .. } if dst == recv => Some(name),
                        _ => None,
                    });
                    (module.map(|m| format!("{m}.{method}")), kwargs)
                }
                _ => return false,
            };
            name.and_then(|n| resolve(&n)).as_deref() == Some("dataclasses.field")
                && kwargs.iter().any(|(k, v)| {
                    k.as_deref() == Some("compare") && body.const_bool(*v) == Some(false)
                })
        })
}

/// Whether calling `f` returns a fresh (async) generator: a generator function without
/// decorators other than `staticmethod` / `classmethod` / `abstractmethod` / `override` (a
/// `@contextmanager` generator returns a context manager, which runs the body at `with`). A
/// generator expression is no generator function: its scope is never called, and its body
/// counts where it is written, where it is almost always consumed (`sum(x for x in xs)`).
pub(crate) fn is_generator_function(f: &IrFunction) -> bool {
    f.name != "<genexpr>"
        && matches!(
            f.kind,
            FunctionKind::Generator | FunctionKind::AsyncGenerator
        )
        && f.decorators.iter().all(|d| {
            let base = d.split('(').next().unwrap_or(d).trim();
            let last = base.rsplit('.').next().unwrap_or(base);
            GENERATOR_KEEPING_DECORATORS.contains(&last)
        })
}

/// The catalog symbol of a decorator of `f` that offloads the function it decorates
/// (`FunctionDecl::offloaded_by`): its head (`sync_to_async` of
/// `sync_to_async(thread_sensitive=False)`) resolved through the module's runtime imports and
/// the project's re-exports.
fn offloading_decorator(cx: &TypeCtx<'_>, f: &IrFunction) -> Option<String> {
    f.decorators.iter().find_map(|d| {
        let (head, _) = decorator_parts(d);
        let symbol = cx
            .project
            .canonical(&cx.module.resolve_dotted(head, false)?);
        let entry = cx.kb.lookup(&symbol)?;
        (entry.offloads_blocking_arg == Some(0)).then_some(symbol)
    })
}

/// Pass 2: Priority 3 class attribute types and module global instance types.
fn complete_decls(
    input: &ModuleInput<'_>,
    mut decls: ModuleDecls,
    project: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
) -> ModuleDecls {
    let cx = TypeCtx::new(input, project, kb);
    // `FunctionDecl::offloaded_by`: the decorator must offload every definition of the name.
    let mut offloaded: FxHashMap<&str, Option<String>> = FxHashMap::default();
    for f in input
        .ir
        .functions
        .iter()
        .filter(|f| is_summarized_function(f))
    {
        let symbol = offloading_decorator(&cx, f);
        offloaded
            .entry(&f.qualname)
            .and_modify(|s| {
                if *s != symbol {
                    *s = None;
                }
            })
            .or_insert(symbol);
    }
    for (qualname, symbol) in offloaded {
        if let Some(d) = decls.functions.get_mut(qualname) {
            d.offloaded_by = symbol;
        }
    }
    // Classes come from the semantic model, IR classes and methods from the IR of the same
    // AST, so every IR class is declared; one that is not gets no attribute types.
    for f in &input.ir.functions {
        if f.is_class_body {
            // Class-body annotations (`fields: list[Field]`, dataclass / pydantic fields).
            for ann in &f.local_annotations {
                if ann.target.contains('.') {
                    continue;
                }
                if let Some(t) = cx.module.resolve_annotation_ty(&ann.annotation)
                    && let Some(class) = decls.classes.get_mut(&f.qualname)
                {
                    class.attr_types.entry(ann.target.clone()).or_insert(t);
                }
            }
            continue;
        }
        if f.is_module_body {
            let types = cx.infer_types(f);
            for inst in f.blocks.iter().flat_map(|b| &b.insts) {
                // An annotated global has its declaration (`REGISTRY: dict[str, Model] = {}`),
                // unless what it holds is narrower.
                let InstKind::StoreGlobal { name, value } = &inst.kind else {
                    continue;
                };
                let stored = match (types.declared(name), types.ty(*value)) {
                    (Some(d), Some(t)) if !project.narrower(t, d) => Some(d),
                    (d, t) => t.or(d),
                };
                if let Some(t @ (Ty::Inst(_) | Ty::Union(_))) = stored {
                    decls.global_types.insert(name.clone(), t.clone());
                }
            }
            continue;
        }
        let (Some(class), Some(self_param)) = (
            f.owner_class_qualname
                .as_ref()
                .and_then(|owner| decls.classes.get_mut(owner)),
            f.params.first(),
        ) else {
            continue;
        };
        if f.has_decorator(&["staticmethod", "classmethod"]) {
            continue;
        }
        let prefix = format!("{}.", self_param.name);
        for ann in &f.local_annotations {
            if let Some(attr) = ann.target.strip_prefix(&prefix)
                && !attr.contains('.')
                && let Some(t) = cx.module.resolve_annotation_ty(&ann.annotation)
            {
                class.attr_types.insert(attr.to_owned(), t);
            }
        }
        if f.name != "__init__" && f.name != "__post_init__" {
            continue;
        }
        let types = cx.infer_types(f);
        let self_vars = self_aliases(f, self_param.var);
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::AttrStore { obj, attr, value } = &inst.kind
                && self_vars.contains(obj)
                && let Some(t @ (Ty::Inst(_) | Ty::Union(_))) = types.ty(*value)
            {
                class
                    .attr_types
                    .entry(attr.clone())
                    .or_insert_with(|| t.clone());
            }
        }
    }
    decls
}

fn module_scope(semantic: &SemanticModel) -> Option<heki_semantic::ScopeId> {
    semantic
        .scopes
        .iter()
        .find(|s| s.kind == ScopeKind::Module)
        .map(|s| s.id)
}

/// Functions that get their own `FuncKey` summary (class bodies are folded away).
pub(crate) fn is_summarized_function(f: &IrFunction) -> bool {
    !f.is_class_body && !f.is_module_body
}

/// `FuncKey` qualname of an IR function (`<module>` for the module body).
pub fn func_qualname(f: &IrFunction) -> &str {
    if f.is_module_body {
        "<module>"
    } else {
        &f.qualname
    }
}

/// Variables that alias `root` through `Copy` and phis whose inputs all alias it.
pub fn self_aliases(f: &IrFunction, root: VarId) -> FxHashSet<VarId> {
    aliases_of(f, [root])
}

/// Variables that are one of `roots` through `Copy` and phis whose inputs all are.
pub fn aliases_of(f: &IrFunction, roots: impl IntoIterator<Item = VarId>) -> FxHashSet<VarId> {
    let mut set: FxHashSet<VarId> = roots.into_iter().collect();
    loop {
        let before = set.len();
        for bb in &f.blocks {
            for phi in &bb.phis {
                if !phi.incoming.is_empty()
                    && phi
                        .incoming
                        .iter()
                        .all(|(_, v)| set.contains(v) || *v == phi.dst)
                {
                    set.insert(phi.dst);
                }
            }
            for inst in &bb.insts {
                if let InstKind::Copy { dst, src } = &inst.kind
                    && set.contains(src)
                {
                    set.insert(*dst);
                }
            }
        }
        if set.len() == before {
            return set;
        }
    }
}

// ------------------------------------------------------------------------------ type inference

/// Inferred value of an SSA variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Val {
    /// A named object: module, class, function, or external symbol (by fqn).
    Ref(String),
    /// An instance of the class `fqn`.
    Inst(String),
    /// A class object that is `fqn` or one of its subclasses ([`Ty::Class`]).
    Class(String),
    /// `super()` inside a method of class `fqn`.
    Super(String),
    /// An instance of one of two or three classes (sorted).
    Union(Vec<String>),
}

/// A receiver or callee as call resolution sees it, borrowed from a [`Val`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum Recv<'v> {
    Ref(&'v str),
    Inst(&'v str),
    Class(&'v str),
    Super(&'v str),
}

impl Val {
    /// The view call resolution takes (`None` for a union, resolved class by class).
    pub(crate) fn recv(&self) -> Option<Recv<'_>> {
        match self {
            Val::Ref(f) => Some(Recv::Ref(f)),
            Val::Inst(c) => Some(Recv::Inst(c)),
            Val::Class(c) => Some(Recv::Class(c)),
            Val::Super(c) => Some(Recv::Super(c)),
            Val::Union(_) => None,
        }
    }
}

/// The targets of a call on a value of one of `classes` (`each` resolves it on one class): the
/// project functions of every class resolving it, or the one external function they all
/// resolve it to; a class not resolving it adds nothing (it has no such member), and project
/// and external targets together leave it unresolved.
fn union_targets(classes: &[String], each: impl Fn(&str) -> Callee) -> Callee {
    let mut keys: Vec<FuncKey> = Vec::new();
    let mut binds = None;
    let mut external: Option<String> = None;
    for class in classes {
        match each(class) {
            Callee::Project(found, b) => {
                // A method call on an instance binds the receiver (a static method's binding
                // is decided where the call binds its arguments).
                binds.get_or_insert(b);
                for k in found {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
            }
            Callee::External(symbol) => {
                if external.as_ref().is_some_and(|e| *e != symbol) {
                    return Callee::Unresolved;
                }
                external = Some(symbol);
            }
            Callee::TooMany => return Callee::TooMany,
            Callee::Unresolved => {}
        }
    }
    match (binds, external) {
        (Some(_), None) if keys.len() > CHA_CANDIDATE_LIMIT => Callee::TooMany,
        (Some(b), None) => Callee::Project(keys, b),
        (None, Some(symbol)) => Callee::External(symbol),
        _ => Callee::Unresolved,
    }
}

impl Ty {
    /// The value consumers see: the class of an instance (the classes of a union), the fqn of a
    /// named object (none for a partial object).
    pub(crate) fn val(&self) -> Option<Val> {
        match self {
            Ty::Ref(f) => Some(Val::Ref(f.clone())),
            Ty::Class(c) => Some(Val::Class(c.clone())),
            Ty::Inst(o) => Some(Val::Inst(o.class.clone())),
            Ty::Super(c) => Some(Val::Super(c.clone())),
            Ty::Union(os) => Some(Val::Union(os.iter().map(|o| o.class.clone()).collect())),
            Ty::SelfType | Ty::Partial(_) => None,
        }
    }
}

/// The targets of constructing `class` from its `__init__` targets: they bind the new
/// instance; a class without a project `__init__` is called by its own name.
fn constructor(class: &str, init: Callee) -> Callee {
    match init {
        Callee::Project(keys, _) => Callee::Project(keys, true),
        _ => Callee::External(class.to_owned()),
    }
}

/// Resolution of a call target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callee {
    /// Project functions; `true` when the receiver binds to the callee's first parameter.
    Project(Vec<FuncKey>, bool),
    External(String),
    TooMany,
    Unresolved,
}

/// Per-module receiver type inference and call resolution (Advisory lane).
pub struct TypeCtx<'a> {
    pub project: &'a ProjectDecls,
    pub module: &'a ModuleDecls,
    pub kb: &'a EffectiveEcosystemKb,
    module_name: String,
    ir: &'a ModuleIr,
    /// `Import` instruction span -> bound fqn (relative imports resolved).
    import_by_span: FxHashMap<Span, String>,
    /// Inference state shared by the functions of the module (child scopes and their parents).
    pub(crate) scopes: crate::infer::Scopes,
}

/// Declarations used when a module is missing from `ProjectDecls`.
static EMPTY_DECLS: ModuleDecls = ModuleDecls {
    module_name: String::new(),
    is_package: false,
    imports: BTreeMap::new(),
    type_checking_imports: BTreeMap::new(),
    classes: BTreeMap::new(),
    functions: BTreeMap::new(),
    global_types: BTreeMap::new(),
    small_globals: BTreeMap::new(),
};

impl<'a> TypeCtx<'a> {
    pub fn new(
        input: &'a ModuleInput<'a>,
        project: &'a ProjectDecls,
        kb: &'a EffectiveEcosystemKb,
    ) -> Self {
        let mut import_by_span = FxHashMap::default();
        for imp in &input.semantic.imports {
            if imp.in_type_checking {
                continue;
            }
            if let Some(t) = import_target(imp, &input.module_name, input.is_package) {
                import_by_span.insert(imp.span, t);
            }
        }
        Self {
            project,
            module: project.module(&input.module_name).unwrap_or(&EMPTY_DECLS),
            kb,
            module_name: input.module_name.clone(),
            ir: input.ir,
            import_by_span,
            scopes: crate::infer::Scopes::default(),
        }
    }

    pub fn module_name(&self) -> &str {
        &self.module_name
    }

    pub fn ir(&self) -> &'a ModuleIr {
        self.ir
    }

    /// Fqn of the class owning method `f` (`module.Outer.Inner`).
    pub fn owner_fqn(&self, f: &IrFunction) -> Option<String> {
        f.owner_class_qualname
            .as_ref()
            .map(|q| format!("{}.{q}", self.module_name()))
    }

    /// Value of a module-level name read inside this module.
    pub(crate) fn global(&self, name: &str) -> Ty {
        if let Some(t) = self.module.imports.get(name) {
            return self.named(t);
        }
        if self.module.classes.contains_key(name) || self.module.functions.contains_key(name) {
            return Ty::Ref(format!("{}.{name}", self.module_name()));
        }
        if let Some(t) = self
            .module
            .global_types
            .get(name)
            .and_then(|t| self.project.declared_ty(t))
        {
            return t;
        }
        Ty::Ref(format!("builtins.{name}"))
    }

    /// A named fqn: a project global holding an instance becomes that instance.
    pub(crate) fn named(&self, fqn: &str) -> Ty {
        let fqn = self.project.canonical(fqn);
        match self.project.symbol(&fqn) {
            Symbol::Global(t) => self.project.declared_ty(t).unwrap_or(Ty::Ref(fqn)),
            _ => Ty::Ref(fqn),
        }
    }

    /// Instance produced by calling the object `fqn`, if known.
    pub(crate) fn call_result(&self, fqn: &str) -> Option<Ty> {
        match self.project.symbol(fqn) {
            Symbol::Class(_) => return Some(Ty::inst(fqn)),
            Symbol::Function(_, decl) => {
                return decl
                    .return_type
                    .as_ref()
                    .and_then(|t| self.project.declared_ty(t))
                    .and_then(|t| t.bind_self(None));
            }
            Symbol::Module | Symbol::Global(_) => return None,
            Symbol::External => {}
        }
        if let Some(b) = fqn.strip_prefix("builtins.") {
            return BUILTIN_TYPES.contains(&b).then(|| Ty::inst(fqn));
        }
        let last = fqn.rsplit('.').next().unwrap_or(fqn);
        if last.chars().next().is_some_and(char::is_uppercase) {
            return Some(Ty::inst(fqn));
        }
        // Layer 2 factory or constructor registered with a lowercase name (`sqlite3.connect`, `boto3.client`).
        let entry = self.kb.lookup(fqn)?;
        if let Some(rt) = &entry.return_type {
            return Some(Ty::inst(rt.as_str()));
        }
        (entry.effects.contains(&ExecEffectKind::UnpooledClientAlloc)
            && !entry.effects.contains(&ExecEffectKind::BlockingNet))
        .then(|| Ty::inst(fqn))
    }

    /// Resolve calling a value (`f(...)`).
    pub fn resolve_call(&self, callee: Option<&Val>) -> Callee {
        match self.dispatch_classes(callee) {
            Some(classes) => union_targets(classes, |c| self.call_targets(Some(Recv::Inst(c)))),
            None => self.call_targets(callee.and_then(Val::recv)),
        }
    }

    /// The classes a call or a property read on `value` dispatches on, class by class: the
    /// members of a union, the structural implementations of a protocol; `None` for a value of
    /// one class (or none) resolved as such.
    fn dispatch_classes<'v>(&'v self, value: Option<&'v Val>) -> Option<&'v [String]> {
        match value? {
            Val::Union(classes) => Some(classes),
            Val::Inst(class) => self.project.protocol_implementers(class),
            Val::Ref(_) | Val::Class(_) | Val::Super(_) => None,
        }
    }

    /// Calling a named class constructs exactly that class: its `__init__` is the one its MRO
    /// finds. A class object that may be a subclass (`cls(...)`) runs any of theirs.
    pub(crate) fn call_targets(&self, callee: Option<Recv<'_>>) -> Callee {
        match callee {
            Some(Recv::Ref(fqn)) => match self.project.symbol(fqn) {
                Symbol::Function(key, _) => Callee::Project(vec![key], false),
                Symbol::Class(_) => constructor(fqn, self.project.class_targets(fqn, "__init__")),
                Symbol::Module | Symbol::Global(_) => Callee::Unresolved,
                Symbol::External => Callee::External(fqn.to_owned()),
            },
            Some(Recv::Class(class)) => {
                constructor(class, self.project.method_targets(class, "__init__", false))
            }
            Some(Recv::Inst(class)) => self.project.method_targets(class, "__call__", false),
            Some(Recv::Super(_)) | None => Callee::Unresolved,
        }
    }

    /// Resolve `recv.method(...)`.
    pub fn resolve_method(&self, recv: Option<&Val>, method: &str) -> Callee {
        match self.dispatch_classes(recv) {
            Some(classes) => union_targets(classes, |c| {
                self.method_call_targets(Some(Recv::Inst(c)), method)
            }),
            None => self.method_call_targets(recv.and_then(Val::recv), method),
        }
    }

    pub(crate) fn method_call_targets(&self, recv: Option<Recv<'_>>, method: &str) -> Callee {
        match recv {
            Some(Recv::Ref(fqn)) => {
                if self.project.is_project_class(fqn) {
                    return self.through_class(self.project.class_targets(fqn, method));
                }
                let target = self.project.canonical(&format!("{fqn}.{method}"));
                self.call_targets(Some(Recv::Ref(&target)))
            }
            Some(Recv::Class(class)) => {
                self.through_class(self.project.method_targets(class, method, false))
            }
            Some(Recv::Inst(class)) => self.project.method_targets(class, method, false),
            Some(Recv::Super(class)) => self.project.method_targets(class, method, true),
            None => Callee::Unresolved,
        }
    }

    /// The targets of calling `method` through a class object, from its definitions `callee`:
    /// a class method binds the class to `cls`; a plain function takes the instance as its
    /// first argument, and a static method takes none.
    fn through_class(&self, callee: Callee) -> Callee {
        match callee {
            Callee::Project(keys, _) => {
                let binds = keys.iter().all(|k| {
                    self.project
                        .function_decl(k)
                        .is_some_and(|d| d.is_classmethod)
                });
                Callee::Project(keys, binds)
            }
            other => other,
        }
    }

    /// Resolve reading `recv.attr` (`AttrLoad`): the property getters it runs, which bind the
    /// receiver (`super().attr` runs the next getter along the MRO); `None` when it runs no
    /// project getter (see [`ProjectDecls::property_targets`]). Reading an attribute of a class
    /// or module object (`Val::Ref`) runs none: it yields the property object itself.
    pub fn resolve_attr_load(&self, recv: Option<&Val>, attr: &str) -> Option<Callee> {
        if let Some(classes) = self.dispatch_classes(recv) {
            return match union_targets(classes, |c| {
                self.project
                    .property_targets(c, attr, false)
                    .unwrap_or(Callee::Unresolved)
            }) {
                Callee::Unresolved => None,
                callee => Some(callee),
            };
        }
        match recv? {
            Val::Inst(class) => self.project.property_targets(class, attr, false),
            Val::Super(class) => self.project.property_targets(class, attr, true),
            Val::Union(_) | Val::Ref(_) | Val::Class(_) => None,
        }
    }

    /// Value returned by a resolved call target.
    pub(crate) fn callee_result(&self, callee: &Callee) -> Option<Ty> {
        match callee {
            Callee::Project(keys, _) if keys.len() == 1 => {
                let decl = self.project.function_decl(&keys[0])?;
                decl.return_type
                    .as_ref()
                    .and_then(|t| self.project.declared_ty(t))
                    .and_then(|t| t.bind_self(None))
            }
            Callee::External(fqn) => self.call_result(fqn),
            _ => None,
        }
    }

    /// Forward inference of `var -> Val` over `f` (the receiver typing of `crate::infer`).
    pub fn infer(&self, f: &IrFunction) -> FxHashMap<VarId, Val> {
        self.infer_types(f).vals()
    }

    /// [`Self::infer`], with the values `isinstance` checks give variables in the blocks they
    /// guard and the classes each check names (by its second argument).
    #[allow(clippy::type_complexity)]
    pub(crate) fn infer_flow(
        &self,
        f: &IrFunction,
    ) -> (
        FxHashMap<VarId, Val>,
        FxHashMap<(BlockId, VarId), Val>,
        FxHashMap<VarId, Vec<String>>,
    ) {
        let inferred = self.infer_types(f);
        (
            inferred.vals(),
            inferred.narrowed_vals(),
            inferred.isinstance_args(f, self.project),
        )
    }

    /// Value bound by an `Import` instruction (needs the span-keyed semantic import).
    pub(crate) fn import_type(&self, span: Span) -> Option<Ty> {
        self.import_by_span.get(&span).map(|t| self.named(t))
    }
}
