//! Module names, import maps, project declarations, and the 4-priority lightweight receiver
//! type resolution (Advisory lane only; never reports type errors).
//!
//! Priorities (docs/ANALYSIS_AND_KB_SPEC.md section 2.1):
//! 1. Exact: local construction and direct imports (`LoadGlobal`, `Import`, `AttrLoad` on a
//!    module, calls of classes, `AllocBuiltin`, `Copy`).
//! 2. Typed: PEP 484 parameter / local / return annotations, `self` / `cls`.
//! 3. Typed: `self.attr` types established in `__init__` / `__post_init__` and module global
//!    types established in the module body, each the join of every store of it
//!    ([`stores`]), and annotations.
//! 4. Heuristic: CHA over project subclasses, at most `CHA_CANDIDATE_LIMIT` candidates.
//!
//! Reading a property (`obj.attr` on a project-class instance whose `attr` is a getter) runs
//! its getter, so `resolve_attr_load` resolves it like a method call.

use crate::capacity::CapacityOf;
use crate::decorators;
use crate::only_read::{LiteralAnnotation, ParamAnnotation};
use crate::ownership;
use crate::readonly::{ContainerFamily, ContainerKind, FamilyLevel, FamilyRole};
use crate::types::{Added, Args, Elem, Generic, Ty, generic, typing_generic};
use crate::{FuncKey, ModuleInput};
use heki_ast::Span;
use heki_env::HekiConfig;
use heki_ir::walk::{Answer, Budget, FnShape, ReadLog, SharedMemo, Visit, reach};
use heki_ir::{
    BinOpKind, BlockId, ConstLiteral, FnIndex, FunctionId, FunctionKind, GlobalStores, Inst,
    InstKind, IrFunction, LastBindings, ModuleIr, SmallDisplay, Terminator, VarId,
};
use heki_kb::{
    ArgFact, CopyDepth, EcosystemEntry, EffectiveEcosystemKb, EffectiveSoundnessKb, ExecEffectKind,
    FieldItems, LiteralValue,
};
use heki_semantic::{ClassInfo, ModuleImport, ScopeKind, SemanticModel, StarImport, mangling};
use rayon::prelude::*;
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::BuildHasher;
use std::ops::Bound;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

mod chained_methods;
mod fields;
mod forwarded;
mod getattr_exports;
mod init_containers;
mod package_paths;
mod returns;
mod sentinels;
pub(crate) mod stores;
mod type_names;
mod workspace;

pub use fields::{Changeable, FieldDefault, FieldValues, InitField, InitFields};
pub use forwarded::{ForwardedReturn, ForwardingCall};
pub use returns::{CallItems, NoItems, ReturnFact, ReturnedItems};
pub use sentinels::object_sentinel_calls;
pub(crate) use type_names::type_expression_text;

/// The derived indexes of a [`ProjectDecls`] version (`ProjectDecls::index_changes`).
struct Indexes {
    subclasses: FxHashMap<String, Vec<String>>,
    property_names: FxHashSet<String>,
    returning_names: FxHashSet<String>,
    alias_names: FxHashSet<String>,
    frozen: FxHashSet<String>,
    built: FxHashSet<String>,
    typed_dicts: FxHashSet<String>,
    protocols: FxHashMap<String, Option<Vec<String>>>,
    grown_elsewhere: FxHashSet<String>,
    rebound_elsewhere: FxHashSet<String>,
    getattr_called: FxHashSet<String>,
    getattr_open: FxHashSet<String>,
    changed_paths: BTreeMap<String, PathChange>,
    copy_registered: FxHashSet<String>,
    families: FxHashMap<(ContainerKind, FamilyLevel), FamilyMembers>,
    setstate_called: bool,
}

/// The project classes a value of a container family's class may be at one level (R2 over the
/// family: [`ProjectDecls::implementers`]), and what may make a class outside them one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FamilyMembers {
    /// Sorted, each once.
    pub classes: Vec<String>,
    pub open: Option<FamilyOpen>,
}

/// What lets a value of a container ABC be of a class heki does not list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilyOpen {
    /// A `register` call registers a class heki cannot name with this class.
    Register(String),
    /// This class of the family binds `__subclasshook__`, which may accept any class: an ABC's
    /// subclass check asks the hooks of its subclasses.
    SubclassHook(String),
}

static NO_MEMBERS: FamilyMembers = FamilyMembers {
    classes: Vec::new(),
    open: None,
};

/// The key of the declarations' read log an answer from [`ProjectDecls::implementers`] notes:
/// the first class of the table read at the level (`builtins.list`, `typing.Sequence`), among
/// the classes whose project-wide facts were looked up.
fn family_key(kind: ContainerKind, level: FamilyLevel) -> &'static str {
    (ContainerFamily::TABLE.iter())
        .find(|r| r.kind == kind && level.role() == r.role)
        .map_or("", |r| r.class)
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

/// The classes whose implementations [`ProjectDecls::instance_targets_complete`] checks at most:
/// a receiver class with more subclasses than this has no complete set of targets.
pub const DISPATCH_CLASS_LIMIT: usize = 64;

/// Upper bound on re-export alias chasing (`pkg.__init__` re-exporting `pkg.sub.Foo`).
const MAX_ALIAS_CHASE: usize = 8;

/// The most star imports nested in one another that deciding the name they bind follows
/// ([`ProjectDecls::star_binding`]): each is a level of the native stack, and past it the name
/// may be bound below, so it stays open. Packages nest a few.
const MAX_STAR_IMPORT_DEPTH: usize = 128;

/// A base heki names no class for, in [`ClassDecl::bases`]: an expression, a module variable,
/// or a value its class statement's scope binds (`def factory(Base): class Derived(Base)`). The
/// MRO keeps it as a base outside the project that heki knows nothing of, which may define
/// anything; no class derives from it. Each is a class of its own, named after its class
/// statement and its place among the bases (`<unknown pkg.mod.Left 0>`), so two unknown bases
/// never order each other in an MRO, as two distinct classes do not.
const UNKNOWN_BASE: &str = "<unknown";

/// Whether the base `base` of a [`ClassDecl`] is one heki names no class for
/// ([`UNKNOWN_BASE`]).
pub fn is_unknown_base(base: &str) -> bool {
    base.starts_with(UNKNOWN_BASE)
}

/// Upper bound on the attribute loads behind a module-level constant's value
/// (`KEY = pkg.sub.keys.OTHER`: three): a dotted name a few modules deep takes a few.
const MAX_CONSTANT_ATTRS: usize = 16;

/// The most dotted segments past the first that a module alias's object may have
/// ([`module_aliases`]).
const MAX_ALIAS_SEGMENTS: usize = 8;

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

/// The exception and warning classes of `builtins` (`IOError` and `EnvironmentError`, which name
/// `OSError`, are left out): a value annotated with one, or built by calling one, is an instance
/// of a class whose methods and namespace the language fixes, as one of [`BUILTIN_TYPES`] is.
const BUILTIN_EXCEPTIONS: &[&str] = &[
    "ArithmeticError",
    "AssertionError",
    "AttributeError",
    "BaseException",
    "BaseExceptionGroup",
    "BlockingIOError",
    "BrokenPipeError",
    "BufferError",
    "BytesWarning",
    "ChildProcessError",
    "ConnectionAbortedError",
    "ConnectionError",
    "ConnectionRefusedError",
    "ConnectionResetError",
    "DeprecationWarning",
    "EOFError",
    "EncodingWarning",
    "Exception",
    "ExceptionGroup",
    "FileExistsError",
    "FileNotFoundError",
    "FloatingPointError",
    "FutureWarning",
    "GeneratorExit",
    "ImportError",
    "ImportWarning",
    "IndentationError",
    "IndexError",
    "InterruptedError",
    "IsADirectoryError",
    "KeyError",
    "KeyboardInterrupt",
    "LookupError",
    "MemoryError",
    "ModuleNotFoundError",
    "NameError",
    "NotADirectoryError",
    "NotImplementedError",
    "OSError",
    "OverflowError",
    "PendingDeprecationWarning",
    "PermissionError",
    "ProcessLookupError",
    "PythonFinalizationError",
    "RecursionError",
    "ReferenceError",
    "ResourceWarning",
    "RuntimeError",
    "RuntimeWarning",
    "StopAsyncIteration",
    "StopIteration",
    "SyntaxError",
    "SyntaxWarning",
    "SystemError",
    "SystemExit",
    "TabError",
    "TimeoutError",
    "TypeError",
    "UnboundLocalError",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
    "UnicodeError",
    "UnicodeTranslateError",
    "UnicodeWarning",
    "UserWarning",
    "ValueError",
    "Warning",
    "ZeroDivisionError",
];

/// Whether `name` is a class of `builtins` whose instances heki types.
fn is_builtin_type(name: &str) -> bool {
    BUILTIN_TYPES.contains(&name) || BUILTIN_EXCEPTIONS.contains(&name)
}

// ------------------------------------------------------------------------------ module names

/// How module names derive from project-relative paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleLayout {
    /// The source roots below the project root: project-relative POSIX directories whose files
    /// are named relative to them (`src/acme/io.py` is `acme.io` under `src`).
    pub source_roots: Vec<String>,
    /// The packages of the workspace members without a source root of their own (a flat
    /// layout: `packages/b/beta/__init__.py`), each named below its member (`beta.io`) as
    /// installing the member names it. The member's other files (`main.py`, `tests/`) keep their
    /// names below the project root, where the same files of other members do not clash.
    pub member_packages: Vec<String>,
    /// The project root is a package itself (`__init__.py`, and no other source root): heki names
    /// its modules top-level, and relative imports may climb to it.
    pub root_is_package: bool,
}

impl ModuleLayout {
    /// The layout of the project at `root`: the `src` setting's directories, else the ones ty
    /// detects ([`detect_source_roots`]) and those of the members of the project's uv, Hatch,
    /// and PDM workspaces (`HekiConfig::workspaces`).
    pub fn detect(root: &Path, cfg: &HekiConfig) -> Self {
        Self::detect_without(root, cfg, &|_| false)
    }

    /// [`Self::detect`] with the files `gone` names counted absent, although the disk may still
    /// list them: a deletion the editor reported (on Windows a deleted file stays listed until
    /// its last handle closes).
    pub fn detect_without(root: &Path, cfg: &HekiConfig, gone: &dyn Fn(&Path) -> bool) -> Self {
        let is_file = |p: &Path| p.is_file() && !gone(p);
        let (source_roots, member_packages) = match &cfg.src {
            Some(dirs) => (
                dirs.iter().filter(|d| !d.is_empty()).cloned().collect(),
                Vec::new(),
            ),
            None => {
                let mut roots = source_roots(root, cfg.project_name.as_deref(), &is_file);
                let members = workspace::member_layout(root, &cfg.workspaces, &is_file);
                roots.extend(members.source_roots);
                (roots, members.packages)
            }
        };
        Self {
            root_is_package: source_roots.is_empty()
                && member_packages.is_empty()
                && is_file(&root.join("__init__.py")),
            source_roots,
            member_packages,
        }
    }

    /// Module name of `rel` (the normalized project-relative path `rel_posix` produces) below
    /// the deepest source root or member holding it, else below the project root
    /// ([`module_name_for_path`]).
    pub fn module_name(&self, rel: &Path) -> Option<(String, bool)> {
        let below_member = self
            .member_packages
            .iter()
            .map(Path::new)
            .filter(|package| rel.starts_with(package))
            .filter_map(|package| rel.strip_prefix(package.parent()?).ok());
        let below_root = self
            .source_root_of(rel)
            .and_then(|r| rel.strip_prefix(r).ok())
            .into_iter()
            .chain(below_member)
            .min_by_key(|rest| rest.components().count());
        module_name_for_path(below_root.unwrap_or(rel))
    }

    /// The deepest source root holding `rel` (a path as for [`Self::module_name`]), `None` when
    /// none does (a member's package or the project root names it).
    pub fn source_root_of(&self, rel: &Path) -> Option<&str> {
        self.source_roots
            .iter()
            .filter_map(|r| Some((r, rel.strip_prefix(r).ok()?)))
            .min_by_key(|(_, rest)| rest.components().count())
            .map(|(r, _)| r.as_str())
    }
}

/// A directory that is not a package (no `__init__.py`, or a stub for one). As a source root it
/// holds the project's top-level packages, namespace packages, and modules (`src/` is the layout
/// packaging tools install from); a regular package is imported by its own name (`src.pkg`).
pub fn is_source_dir(dir: &Path, is_file: &dyn Fn(&Path) -> bool) -> bool {
    dir.is_dir()
        && !["__init__.py", "__init__.pyi"]
            .iter()
            .any(|f| is_file(&dir.join(f)))
}

/// The source roots ty detects without a configured one: `src`, `<project-name>` when
/// `<project-name>/<project-name>` is a directory, and `python` (maturin), each a directory that
/// is not a package. `project_name` is `project.name` of the `pyproject.toml`, else the root's
/// own name.
pub fn detect_source_roots(root: &Path, project_name: Option<&str>) -> Vec<String> {
    source_roots(root, project_name, &Path::is_file)
}

/// [`detect_source_roots`], with `is_file` telling which package markers are there.
fn source_roots(
    root: &Path,
    project_name: Option<&str>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Vec<String> {
    let own_name = root.file_name().and_then(|n| n.to_str());
    let named = project_name
        .or(own_name)
        .filter(|name| root.join(name).join(name).is_dir());
    let mut roots: Vec<String> = Vec::new();
    for dir in ["src"].into_iter().chain(named).chain(["python"]) {
        if is_source_dir(&root.join(dir), is_file) && !roots.iter().any(|r| r == dir) {
            roots.push(dir.to_owned());
        }
    }
    roots
}

/// Module name of `rel`, a normalized path below a source root (or the project root):
/// `pkg/mod.py` -> `pkg.mod`, `pkg/__init__.py` -> `pkg` (package). Returns
/// `(module_name, is_package)`.
pub fn module_name_for_path(rel: &Path) -> Option<(String, bool)> {
    let mut parts: Vec<String> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_owned()),
            _ => return None,
        }
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

/// The longest dotted prefix of `name` (itself included) that `is_module` accepts: the module a
/// dotted name resolves into, the rest of the name naming something in it.
pub fn module_prefix(name: &str, mut is_module: impl FnMut(&str) -> bool) -> Option<&str> {
    let mut end = name.len();
    loop {
        if is_module(&name[..end]) {
            return Some(&name[..end]);
        }
        end = name[..end].rfind('.')?;
    }
}

/// Resolve the module part of `from <level dots><module> import ...` inside `current`.
/// A package `__init__` is its own base; a plain module's base is its parent package. `None`
/// when the level climbs past the top-level package, where Python raises `ImportError`; when
/// `root_is_package`, the import root is that package (heki names its modules top-level), and
/// `""` is the root package itself.
pub fn resolve_relative_module(
    current: &str,
    current_is_package: bool,
    level: u32,
    module: Option<&str>,
    root_is_package: bool,
) -> Option<String> {
    if level == 0 {
        return module.map(str::to_owned);
    }
    let mut package: Vec<&str> = current.split('.').collect();
    if !current_is_package {
        package.pop();
    }
    // Each dot after the first climbs one package; at least the top-level one must remain.
    let keep = (package.len() + 1).checked_sub(usize::try_from(level).ok()?)?;
    if keep == 0 && !root_is_package {
        return None;
    }
    package.truncate(keep);
    let base = package.join(".");
    Some(match module {
        None => base,
        Some(m) if base.is_empty() => m.to_owned(),
        Some(m) => format!("{base}.{m}"),
    })
}

/// The module the star import `star` of the module `input` names (`from ._utils import *`).
fn star_import_module(input: &ModuleInput<'_>, star: &StarImport) -> Option<String> {
    resolve_relative_module(
        &input.module_name,
        input.is_package,
        star.level,
        star.module.as_deref(),
        input.root_is_package,
    )
}

/// Fully qualified symbol bound by one import.
fn import_target(imp: &ModuleImport, input: &ModuleInput<'_>) -> Option<String> {
    if !imp.is_from {
        // `import a.b.c` binds `a`; `import a.b.c as x` binds `a.b.c`.
        return Some(if imp.has_alias {
            imp.imported_name.clone()
        } else {
            imp.imported_name
                .split('.')
                .next()
                .unwrap_or(&imp.imported_name)
                .to_owned()
        });
    }
    let base = resolve_relative_module(
        &input.module_name,
        input.is_package,
        imp.level,
        imp.module.as_deref(),
        input.root_is_package,
    )?;
    Some(if base.is_empty() {
        imp.imported_name.clone()
    } else {
        format!("{base}.{}", imp.imported_name)
    })
}

// ------------------------------------------------------------------------------ declarations

/// What reading one attribute on a value of a class may run besides finding it
/// ([`ProjectDecls::attr_read`]), over the classes the value may be under R2.
#[derive(Debug, Default)]
pub struct AttrRead {
    /// Every class along the MRO is a project class or a plain base (`object`, `NamedTuple`,
    /// `Generic`): an external base may define anything.
    pub bases_known: bool,
    /// A project class along the MRO has the attribute as a member.
    pub declared: bool,
    /// A class has a metaclass other than `type` and `abc.ABCMeta`, which may install a
    /// descriptor.
    pub custom_metaclass: bool,
    /// A class defines `__getattribute__`, which runs on every read.
    pub getattribute: bool,
    /// A class defines `__getattr__`, which runs when the instance lacks the attribute.
    pub getattr: bool,
    /// The classes that bind the name (in the body or on `self`).
    pub binders: Vec<String>,
    /// The classes whose body binds the name to something that may run code when read on an
    /// instance (`ClassDecl::code_attrs`): a method, a property, a call.
    pub code_binders: Vec<String>,
}

impl ClassDecl {
    /// Whether the body binds `model_config` or `Config`: a model's configuration, which may ask
    /// it to validate its fields' defaults (`validate_default`) or the values written to them
    /// (`validate_assignment`).
    fn configures_model(&self) -> bool {
        self.members.contains("model_config") || self.members.contains("Config")
    }

    /// What the class's methods store in `attr` (`ClassDecl::attr_stores`).
    pub fn stored(&self, attr: &str) -> Option<&AttrStore> {
        let i = self
            .attr_stores
            .binary_search_by(|(a, _)| (**a).cmp(attr))
            .ok()?;
        Some(&self.attr_stores[i].1)
    }

    /// The Priority 3 attribute types, sorted by attribute: the declared ones and those
    /// `__init__` or `__post_init__` establishes (both for a bare container annotation).
    pub fn typed_attrs(&self) -> Vec<(&str, &Ty)> {
        let established = self.attr_stores.iter().filter_map(|(a, s)| match s {
            AttrStore::Established(t) => Some((&**a, t)),
            AttrStore::Of(_) | AttrStore::Open => None,
        });
        let mut out: Vec<(&str, &Ty)> = self
            .attr_types
            .iter()
            .map(|(a, t)| (a.as_str(), t))
            .chain(established)
            .collect();
        out.sort_by_key(|(a, _)| *a);
        out
    }
}

impl AttrRead {
    /// Whether a read may run a hook of the project: `__getattribute__`, a metaclass heki does
    /// not know, and with `missing` (the instance may lack the attribute) `__getattr__`.
    pub fn runs_hook(&self, missing: bool) -> bool {
        self.getattribute || self.custom_metaclass || (missing && self.getattr)
    }
}

/// What the stores of a module's code bind one attribute of the instances of a class to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttrStore {
    /// `__init__` or `__post_init__` binds it through `self` to a value of known type, and
    /// every store binds a value of this type (Priority 3).
    Established(Ty),
    /// Every store binds a value of this type, and none of `__init__`'s does.
    Of(Ty),
    /// A store binds a value of unknown type, or values no tracked type covers.
    Open,
}

/// What a module's code stores into the globals of other project modules and into the
/// instances of their classes, through a receiver whose type the inference knows
/// (`cfg.client = v`, `setattr(cfg, "client", v)`, `vars(cfg)["client"] = v`,
/// `obj.client = v` with `obj: Helper`): the join of the values' types, `None` when one is
/// unknown or no tracked type covers them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoresElsewhere {
    /// By the global's fqn.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub globals: BTreeMap<String, StoredElsewhere>,
    /// By the class's fqn, then the attribute.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, BTreeMap<String, StoredElsewhere>>,
    /// The modules and classes, by fqn, it stores into under a name heki cannot read
    /// (`setattr(cfg, name, v)`, `vars(obj).update(other)`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub unnamed: BTreeSet<String>,
    /// The attributes, by name, it stores through a receiver heki types as no project class and
    /// no project module (`app.db = v` with `app` a parameter without an annotation, `C.db = v`
    /// on a class object), here or in its own module: such a store may bind the attribute of an
    /// instance of any class ([`ProjectDecls::held_attr`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub untyped_attrs: BTreeSet<String>,
    /// It stores under a name heki cannot read through a receiver heki types as no project
    /// class and no project module (`setattr(obj, name, v)` with `obj` a parameter without an
    /// annotation): such a store may bind any attribute of an instance of any class
    /// ([`ProjectDecls::held_attr`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub untyped_unnamed: bool,
    /// The names it stores under, here or in its own module, through a receiver that may be a
    /// class object: one heki does not establish as an instance (`Child.__setattr__ = hook`,
    /// `setattr(cls, "__setattr__", hook)` in a class method, `target.__setattr__ = hook` with
    /// `target` a parameter without an annotation). Such a store may bind what Python looks up on
    /// the instances of a class ([`ProjectDecls::bound_after_body`]); one through an instance does
    /// not, as Python looks special methods up on the type. A store under a name of unknown value
    /// is not listed (R1: a special method is installed under its own name).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub on_classes: BTreeSet<StoredName>,
}

/// A name a store binds ([`StoresElsewhere::on_classes`]), as far as heki reads it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum StoredName {
    /// The one a string constant gives.
    Named(String),
    /// One built at run time that starts with `head` and ends with `tail`, which do not overlap
    /// and are not both empty (`stores::AttrName::Built`).
    Built { head: String, tail: String },
}

/// What a module's stores bind one name of another module to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredElsewhere {
    /// The join of the values' types, `Some(None)` when one is unknown or no tracked type covers
    /// them; `None` when every store rebinds the name in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ty: Option<Option<Ty>>,
    /// A store rebinds the name to what an in-place operator gives on its own value
    /// (`obj.items += [x]`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_place: bool,
    /// What the module's code changing the object the name holds in place adds to it
    /// (`cfg.HANDLERS[kind] = run`, `obj.items.append(x)`): its items join what they add.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<Added>,
    /// A store binds a value that may be `None`, which `ty` leaves out: also a store of `None`
    /// alone is one into the name ([`ProjectDecls::held_attr`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub none: bool,
}

impl StoredElsewhere {
    /// Whether a store binds the name to an object: what a change in place alone leaves is the
    /// object the name held.
    pub(crate) fn binds(&self) -> bool {
        self.ty.is_some() || self.in_place || self.none
    }
}

/// A module's global and attribute types decided without what reading them gives, where they
/// differ from [`ModuleDecls::global_types`] and [`ClassDecl::attr_stores`]. The fallback of a
/// name holds once another module stores into a name its stores read, directly or through the
/// stores of the names they read ([`Self::dependencies`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreFallbacks {
    /// By global: its type, `None` for none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub globals: BTreeMap<String, Option<Ty>>,
    /// By class qualname, then attribute.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, BTreeMap<String, AttrStore>>,
    /// The classes that store attributes heki cannot name, once another module stores into
    /// the module.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub any_attr: BTreeSet<String>,
    /// Each function whose stores read names being decided, once: what it stores and reads.
    #[serde(default, skip_serializing_if = "<[_]>::is_empty")]
    pub dependencies: Box<[StoreDependency]>,
}

/// The names of a module one function stores into, and those of the names being decided it
/// reads (sorted).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreDependency {
    pub stores: Box<[StoreTarget]>,
    pub reads: Box<[StoreTarget]>,
}

/// A global of a module, or an attribute of the instances of one of its classes (by
/// qualname), whose type the module's stores decide.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StoreTarget {
    Global(String),
    Attr(String, String),
}

/// A class's own `__init__` that uses the instance only as the object of attribute stores (none
/// of `__dict__` or `__class__`) and as `super()` for a call of the next `__init__` along the
/// MRO handed nothing that may hold the instance (`init_containers`). Whether the stores run
/// code depends on the class constructed ([`ProjectDecls::constructor`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlainInit {
    /// The attributes it stores through the instance, sorted, each once.
    pub stores: Box<[Box<str>]>,
    /// It calls `super().__init__(...)`.
    pub calls_next: bool,
    /// The attributes it binds to a builtin container it makes there, with the container's
    /// kind, sorted: when it returns, the attribute holds that container and nothing else does.
    pub containers: Box<[(Box<str>, ContainerKind)]>,
    /// The global names it reads as builtins, since its module binds them no other way
    /// (`super`, the container classes it calls), sorted: a star import may still bind one.
    #[serde(default, skip_serializing_if = "<[_]>::is_empty")]
    pub builtins: Box<[Box<str>]>,
}

/// What a construction of a project class keeps of the instance it makes
/// ([`ProjectDecls::constructor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Constructor<'d> {
    /// Nothing beyond stores into the instance's own attributes: the object is new when the
    /// call returns. `containers`: the builtin containers it holds by attribute that the call
    /// made and nothing else holds ([`PlainInit::containers`]).
    KeepsNothing {
        containers: &'d [(Box<str>, ContainerKind)],
    },
    /// The `__init__` the call runs uses the instance otherwise: the call hands the instance to
    /// that `__init__`'s first parameter, which keeps it when the `__init__` keeps that
    /// parameter (`EffectSummary::retains_params`).
    HandsToInit,
    /// The call may give an object something else holds, or heki cannot tell what it runs.
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassDecl {
    /// Base classes, resolved within the defining module (canonicalized on lookup), each the
    /// value its class statement passes, named where the statement runs: a module-level name
    /// (an import, a class, a module alias, an attribute of a module) through the module's
    /// names, and in a function or a class body also a local alias of one or an import there.
    /// A base heki names no class for (a parameter, a local, a closure cell, a class-body name
    /// bound to anything else, an expression, a module variable) is an unknown base
    /// ([`is_unknown_base`]); a name the module does not bind (a builtin heki does not list) is
    /// left out.
    pub bases: Vec<String>,
    /// The declared instance attribute types, `attr -> type`: annotations in the class body or
    /// on `self` in a method, which type every store of the attribute. A bare container
    /// annotation (`list`, `dict`) types only the class: the stores give the items
    /// ([`Self::attr_stores`], [`ProjectDecls::member_type`]).
    pub attr_types: BTreeMap<String, Ty>,
    /// The attributes annotated with one of the class's type parameters (`obj: T` in `class
    /// Site(Generic[T])` or `class Site[T]:`), sorted, each with the parameter's position: an
    /// instance an annotation gives arguments (`Site[Schema]`, [`Args::TypeArgs`]) holds the
    /// argument there ([`ProjectDecls::type_argument`]). Only classes of the module body.
    #[serde(default, skip_serializing_if = "<[_]>::is_empty")]
    pub param_attrs: Box<[(Box<str>, u8)]>,
    /// What the module's code stores in the attributes of the class's instances it does not
    /// annotate or annotates with a bare container class (`list`), through `self` or another
    /// receiver of the class, sorted by attribute
    /// (`stores::record`). A slice rather than a map: most classes store a few, and a map's
    /// first node would hold room for eleven.
    #[serde(default, skip_serializing_if = "<[_]>::is_empty")]
    pub attr_stores: Box<[(Box<str>, AttrStore)]>,
    /// The module's code stores an attribute of an instance heki cannot name
    /// (`setattr(self, name, v)`, `self.__dict__.update(other)`): any attribute may hold any
    /// value.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stores_any_attr: bool,
    /// One of those stores may add an attribute an instance did not have: its name is no key of
    /// the namespace of an instance of the class or a subclass (a copy,
    /// `self.__dict__.update(other.__dict__)`) and no field they declare
    /// ([`ProjectDecls::keys_exist_in`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adds_unnamed_attrs: bool,
    /// The attributes the module's code rebinds to what an in-place operator gives on their own
    /// value (`self.items += [x]`), sorted: such a store binds the object it read when the
    /// attribute holds a value the operator changes in place ([`ProjectDecls::join_family`]).
    #[serde(default, skip_serializing_if = "<[_]>::is_empty")]
    pub in_place_attrs: Box<[Box<str>]>,
    /// The attributes the module's code may bind otherwise than to an instance through `self`
    /// in a method of the class, sorted: a store through another receiver of the class (another
    /// instance, `cls()` in a class method, a parameter annotated with the class), a store of a
    /// value that may be `None`, or an annotation of the class that allows `None`. Their value
    /// is no class the class's own methods alone decide ([`ProjectDecls::held_attr`]).
    #[serde(default, skip_serializing_if = "<[_]>::is_empty")]
    pub unowned_attrs: Box<[Box<str>]>,
    /// The only stores heki cannot name are those `__setstate__` makes from its state, which
    /// restore what an instance held when pickle or copy took the state
    /// ([`ProjectDecls::restores_hold`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restores_state: bool,
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
    /// The class statement names a metaclass other than `type` and `abc.ABCMeta`, also one heki
    /// cannot resolve, or has a base heki names no class for ([`is_unknown_base`]) or unpacks
    /// a mapping of keywords whose keys heki does not know (`**options`), whose metaclass is
    /// unknown ([`ProjectDecls::has_custom_metaclass`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom_metaclass: bool,
    /// The `frozen=` keyword of the class statement (`class C(Base, frozen=False)`).
    pub frozen_keyword: Option<bool>,
    /// The class statement passes a keyword other than `metaclass=`, or unpacks a mapping of
    /// them (`class M(BaseModel, validate_assignment=True)`): a configuration of what a base or
    /// the metaclass makes of the class ([`ProjectDecls::attr_write_stores_value`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub passes_keywords: bool,
    /// `@dataclass_transform(...)` on this class (as a base class or metaclass of the classes it
    /// builds): `Some(frozen_default)`.
    pub transform: Option<bool>,
    /// `@dataclass` / `@dataclasses.dataclass` on the class: its class-body annotations declare
    /// instance fields ([`ProjectDecls::field_defaults`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataclass: Option<Dataclass>,
    /// The fields the constructor a `NamedTuple` or a `@dataclass(frozen=True)` generates for
    /// the class binds, its own in order ([`ProjectDecls::constructor_fields`]): what an
    /// instance it makes holds in them. Boxed: few classes have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_fields: Option<Box<InitFields>>,
    /// What the class's own body and methods store in its attributes (`attr = [...]` in the
    /// body, `self.attr = ...` in a method): `Some` when every store is a small display that
    /// none of them grows (`heki_ir::FnIndex::stored_display`), `None` when an attribute is
    /// stored anything else, deleted, or grown through `self` (`OPT112` / `OPT113` read the
    /// attributes of a class family through [`ProjectDecls::small_attr`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attr_displays: BTreeMap<String, Option<SmallDisplay>>,
    /// What the class's own body and methods store in its attributes (`attr = v` in the body,
    /// `self.attr = v` in a method), for the attributes one store of which is a queue (a
    /// construction of a catalogued queue class, or a parameter annotated with one) or, for a
    /// name the module reads as a queue's `maxsize`, an integer constant: the join of every store
    /// (`crate::capacity`). An attribute the class defines without an entry has an unknown
    /// capacity ([`ProjectDecls::attr_capacity`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attr_capacities: BTreeMap<String, CapacityOf>,
    /// What the class's own `__init__` does with the instance when all it does is store
    /// attributes on it and call the next `__init__` along the MRO ([`PlainInit`]); `None` for
    /// any other `__init__`, and without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plain_init: Option<Box<PlainInit>>,
    /// `typing.Protocol` is one of the bases (PEP 544: the class is a protocol, not an
    /// implementation of one).
    pub is_protocol: bool,
    /// The names the class defines: class-body names (methods, annotations, assignments) and
    /// the attributes its methods set or annotate on `self` (structural typing, `Protocol`).
    pub members: BTreeSet<String>,
    /// The class-body names bound to a `property` (`@property`, `@x.setter`, `@x.deleter`,
    /// `x = property(...)`): storing or deleting the attribute on an instance runs the
    /// property's setter or deleter, or raises without one, instead of changing the instance's
    /// own attribute ([`ProjectDecls::attr_write_runs_code`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub properties: BTreeSet<String>,
    /// The `@property` getters whose return annotation is a container type, as for
    /// `container_fields`, with the access the type promises: reading the attribute gives a
    /// container, which a type checker stops changing in place when the type is read-only
    /// (`Sequence`, `Mapping`, `AbstractSet`) (`MUT120`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub container_properties: BTreeMap<String, ParamAccess>,
    /// Every name the class defines (`members`), at least one, is a property without a setter
    /// or deleter: a `properties` name whose getter is a `FunctionDecl::is_property`
    /// ([`ProjectDecls::is_readonly_view`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub only_getters: bool,
    /// The attributes its module stores on the class object or deletes outside the class body
    /// (`Keys.KEY = "x"`, `cls.KEY = "x"`, [`class_attrs_rebound`]): the class's own value of
    /// the name from then on, which no string constant of the class or its bases fixes.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub rebound_attrs: BTreeSet<String>,
    /// The methods of its body named in some catalog class's `effect_methods` that call the
    /// next definition along the MRO (`super().save(...)`) on every path to a return, each the
    /// only binding of its name in the body (`chained_methods`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub chains_to_next: BTreeSet<String>,
    /// The class-body names bound to something other than a constant, a display, or a field
    /// specifier (`dataclasses.field(...)`, `attrs.field(...)`): methods, properties, and any
    /// call, which may make a descriptor. Reading such a name on an instance may run code.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub code_attrs: BTreeSet<String>,
    /// The attributes the class's own code binds to a module-level name that may be an
    /// `object()` sentinel: one of its module's [`ModuleDecls::object_sentinels`], or a name an
    /// import binds, whose module decides ([`ProjectDecls::held_sentinels`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub named_attrs: BTreeMap<String, NamedAttr>,
}

/// What the code of a class binds one attribute to among the module-level names
/// ([`ClassDecl::named_attrs`]), each name by the fqn its module gives it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedAttr {
    /// The class body binds the attribute last to this name on every path through it
    /// (`attr = NAME`, `attr: T = NAME`, and `attr: T = field(default=NAME)` in a stdlib
    /// dataclass; a binding every path runs replaces the ones before it): the class attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The class body binds it to `staticmethod(NAME)` (`body` names `NAME`): reading the
    /// attribute on an instance gives the object `NAME` names, never a bound method.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub body_static: bool,
    /// The class body binds the attribute last to different objects on different paths
    /// (`def fetch(...)`, then `if flag: fetch = _quiet`): the names among them, each with
    /// whether `staticmethod` wraps it. `body` is then `None`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub body_paths: Vec<(String, bool)>,
    /// With `body_paths`: the class's own `def` of the attribute is what it holds on the other
    /// paths.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub body_defined: bool,
    /// The class body binds the attribute last, on every path, to a `functools.partial` object
    /// of a name that holds keywords only (`copy = partial(copy_field, deep=False)`, also
    /// wrapped in `staticmethod`): `body` is then `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_partial: Option<Box<PartialAttr>>,
    /// The class body binds the attribute last, on every path, to what a call of this name gives
    /// (`objects = StreamManager()`): `body` is then `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_call: Option<String>,
    /// The class body annotates the attribute outside `ClassVar` and `InitVar`: a field, where
    /// a base the project does not define may build an `__init__` that stores the default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub annotated: bool,
    /// The annotation names `validate_default` (`Annotated[T, Field(validate_default=True)]`):
    /// a library may store what validating the default returns instead of the default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub validates_default: bool,
    /// The names an instance holds in the attribute from the class's own code: the default of a
    /// field that the `__init__` a stdlib `@dataclass` generates stores (`init` left on, no
    /// `__init__` in the class body), and the names its methods store with `self.attr = NAME`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub held: BTreeSet<String>,
}

/// A `functools.partial` object a class body binds an attribute to ([`NamedAttr::body_partial`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialAttr {
    /// The name of the callable it holds, by the fqn its module gives it.
    pub callable: String,
    /// It holds keywords, whose values the calls through it do not see.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keywords: bool,
    /// `staticmethod` wraps it: reading it on an instance gives the object itself. A plain
    /// `functools.partial` read on an instance binds the instance from Python 3.14 on (a method
    /// descriptor), and gives the object itself before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_static: bool,
}

impl PartialAttr {
    /// What the object holds for its callable: keywords or nothing.
    pub(crate) fn held(&self) -> crate::types::Held {
        match self.keywords {
            true => crate::types::Held::Keywords,
            false => crate::types::Held::Nothing,
        }
    }
}

/// What reading a class attribute finds when its class body binds it to module-level names
/// ([`ProjectDecls::class_aliases`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassAlias<'d> {
    /// The names it may hold, each with whether `staticmethod` wraps it.
    pub names: Vec<(&'d str, bool)>,
    /// On the other paths it holds what the class's own `def` of it makes: that function, by
    /// fqn.
    pub defined: Option<String>,
    /// It holds this `functools.partial` object of a name instead (`names` is empty).
    pub partial: Option<&'d PartialAttr>,
}

/// An `object()` sentinel an instance of a class may hold in an attribute
/// ([`ProjectDecls::held_sentinels`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HeldSentinel {
    pub attr: String,
    /// The sentinel's canonical fqn.
    pub sentinel: String,
    /// The class's code stores it on the instance itself; otherwise the class only declares it
    /// as the default of a field that a base heki does not know may store.
    pub stored: bool,
}

/// What `copy.deepcopy` or `copy.copy` makes of a value of a class ([`ProjectDecls::copy_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMade {
    /// A new instance whose attributes hold the original's values, deep copies of them for
    /// `copy.deepcopy` (a new object for an `object()` sentinel), for every class the value may
    /// be.
    Attributes,
    /// A copy hook along the class's own MRO, or a `copyreg` registration of the class, makes
    /// the copy, and may keep any value.
    Hooked,
    /// Heki cannot tell for some class the value may be: a hook in a subclass or a mixin, a
    /// base outside the project the catalog does not describe, a metaclass, or a `__getattr__`
    /// / `__getattribute__` that the copy may ask for its hook.
    Open,
}

/// What `@dataclass` makes of the class attributes of a class's fields ([`ClassDecl::dataclass`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dataclass {
    /// Each field's default stays the class attribute.
    Defaults,
    /// `slots=True`: a slot descriptor replaces the class attribute of every field, the class's
    /// own and those of its dataclass bases.
    Slots,
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
    /// How many leading parameters only a position binds (those before `/`, the receiver
    /// included).
    pub positional_only: u8,
    /// How many leading parameters have no default (the receiver included): a call leaving one
    /// unbound raises `TypeError`. Python gives every positional parameter after one with a
    /// default a default too, so these are the positional parameters without one.
    pub required_positional: u8,
    /// The keyword-only parameters without a default, one bit each from the first keyword-only
    /// parameter (`positional_params`) on; past the 8th none counts (one byte, not a list: every
    /// declaration of the project keeps it).
    pub required_keyword_only: u8,
    /// How many leading parameters have a name a keyword can give (all but `*args` /
    /// `**kwargs`, which come last); a keyword binds those past the positional-only ones
    /// ([`Self::keyword_param`]).
    pub named_params: u8,
    /// It takes `*args`: positions past the positional parameters land there, where the call
    /// would otherwise raise `TypeError`.
    pub has_varargs: bool,
    /// It takes `**kwargs`: a keyword binding no parameter lands there, where the call would
    /// otherwise raise `TypeError`.
    pub has_kwargs: bool,
    /// The literal defaults, by parameter position (`guards`: an omitted argument).
    pub defaults: Vec<(u8, ArgFact)>,
    /// Calling it returns a fresh (async) generator: a generator function whose every decorator
    /// keeps the call (`decorators::Decorator::keeps_call`: `staticmethod`, `typing.final`, ...;
    /// a `@contextmanager` generator returns a context manager). `MUT106` single-pass sources.
    pub is_generator: bool,
    /// No decorator replaces it: every decorator it has keeps the call
    /// (`decorators::Decorator::keeps_call`: `staticmethod`, `typing.final`, ...), so calling it
    /// does what its definition says (see [`FunctionDecl::makes_no_coroutine`]).
    pub called_as_defined: bool,
    /// A property getter that reading the attribute on an instance runs: a method decorated
    /// with the builtin `property` or a `cached_property` (by the name of what the decorator is
    /// bound to, `decorators::Decorator::getter`) whose every definition is such a getter. A
    /// property with a setter or deleter is not one: `@x.setter def x(...)` is a second
    /// function with the getter's qualname, whose facts share the getter's summary, so reading
    /// the property would pick up the setter's writes.
    pub is_property: bool,
    /// Parameters (bit `i`: parameter `i < 64`) annotated with a read-only container type:
    /// `Sequence[...]`, `Mapping[...]`, `AbstractSet[...]`, or `ReadOnly[...]` after `Optional`
    /// / `| None` / `Annotated` (`MUT108`). Variadic parameters never are.
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
    /// An `@abstractmethod`: a call never runs it.
    pub is_abstract: bool,
    /// The return annotation of the getter of a property (`@property` / `cached_property`,
    /// with or without a setter): the type reading the attribute gives.
    pub property_type: Option<Ty>,
    /// `(i, None)` when every `return` of every definition returns parameter `i` as itself
    /// (`return x`, `return cast(list[int], x)`), `(i, Some(attr))` when each returns attribute
    /// `attr` of it (`return self.items`) ([`returned_param`]), or what a call returning it
    /// returns, which completing the declarations decides ([`Self::forwards`]): calling the
    /// function, or reading the property, gives the argument, or that object of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns_param: Option<(u8, Option<String>)>,
    /// What every `return` of every definition gives when some return gives the result of a
    /// call (`return same(x)`) and the others a parameter or an attribute of one
    /// ([`returned_param`]): the calls, from which completing the declarations decides
    /// [`Self::returns_param`] against what the callees return ([`forwarded::complete`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwards: Option<Box<ForwardedReturn>>,
    /// The fqn an import binds when every `return` of every definition returns what that import
    /// binds, a module or an object of one ([`returned_import`]): an import in the function
    /// (`from pkg import ops` then `return ops`, which breaks an import cycle) or one of the
    /// module. Calling the function gives that object when it is a project module or the return
    /// annotation names no class of its own (`-> ModuleType`, [`Self::declared_return`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returned_import: Option<Box<str>>,
    /// What every `return` of every definition gives, past what the annotation says, inferred
    /// in pass 1 against the module's own declarations ([`returns::record_return_facts`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<Box<ReturnFact>>,
    /// A `__getstate__` returning a state built only from the instance's own attributes
    /// (`stores::state_of_self`), which `__setstate__` restores.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub state_of_self: bool,
    /// The parameters of enclosing functions a nested scope reads through its closure cells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_params: Option<Box<CapturedParams>>,
}

/// The parameters of the functions enclosing a nested scope (a comprehension or generator
/// expression with a scope of its own, a lambda, a nested `def`) that the scopes nested in them
/// read through closure cells, each a cell its function binds only as that parameter and no
/// nested scope rebinds. Each is one more parameter of the scope, past its own, that a call binds
/// to what the caller holds there: the caller's own parameter when the caller binds the cell, the
/// caller's position of it when the caller is nested in that function too, and nothing for a
/// closure called away from that function. The masks of the scope's facts (`obj_params`, an
/// argument's `may_params`) name them by position, so its accesses to the enclosing method's
/// `self` map to that `self` at the method, as those to an argument do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedParams {
    /// The position of the first: past the parameters of every scope with the qualname.
    pub first: u8,
    /// The enclosing function (its qualname) and its parameter each position holds, in order.
    pub params: Vec<(String, u8)>,
}

impl CapturedParams {
    /// The mask bit of parameter `param` of the enclosing function `owner`, when the scope
    /// reads it.
    pub fn bit(&self, owner: &str, param: u8) -> u64 {
        self.params
            .iter()
            .position(|(o, p)| o == owner && *p == param)
            .map_or(0, |j| 1u64 << (usize::from(self.first) + j))
    }

    /// The parameters of the enclosing function `owner` that the positions in `mask` hold.
    pub fn owner_params(&self, owner: &str, mask: u64) -> u64 {
        self.params
            .iter()
            .enumerate()
            .filter(|(j, (o, _))| o == owner && mask & (1u64 << (usize::from(self.first) + j)) != 0)
            .fold(0, |acc, (_, (_, p))| acc | 1u64 << p)
    }
}

impl FunctionDecl {
    /// A property's getter, with or without a setter: reading its name on an instance gives
    /// what the getter returns, and a call of the name calls that value.
    pub(crate) fn reads_as_property(&self) -> bool {
        self.is_property || self.property_type.is_some()
    }

    /// What calling it gives when its declaration says it returns `returned`: for an
    /// `async def` that no decorator replaces, the coroutine that awaiting gives `returned`.
    /// The value calling it gives by the declarations: what every return gives when it is
    /// narrower than the annotation ([`ReturnFact::ty`]), else what the return annotation names
    /// (`Self` left unbound), unless every return gives an object an import binds
    /// ([`Self::returned_import`]) and that object is a project module or the annotation names
    /// none of its own (`-> ModuleType`, none).
    pub(crate) fn declared_return(&self, project: &ProjectDecls) -> Option<Ty> {
        let declared = self
            .inferred_return(project)
            .or(self.return_type.as_ref())
            .and_then(|t| project.declared_ty(t))
            .and_then(|t| t.bind_self(None));
        let Some(fqn) = &self.returned_import else {
            return declared;
        };
        let module = matches!(project.symbol(&project.object_named(fqn)), Symbol::Module);
        if module || self.annotation_names_no_object() {
            Some(project.named_ty(fqn))
        } else {
            declared
        }
    }

    /// [`Self::declared_return`] as the module's declarations name what calling it gives,
    /// before the project resolves the classes they name, with the globals it was inferred from
    /// ([`ReturnFact::ty_globals`]): pass 1 reads it, where a class of another module is no
    /// class yet (`getattr_exports`).
    pub(crate) fn declared_return_as_named(&self) -> Option<(Ty, &[String])> {
        if let Some(fqn) = self
            .returned_import
            .as_deref()
            .filter(|_| self.annotation_names_no_object())
        {
            return Some((Ty::Ref(fqn.to_owned()), &[]));
        }
        match self.returns.as_deref() {
            Some(ReturnFact {
                ty: Some(t),
                ty_globals,
                ..
            }) => Some((t.clone(), ty_globals)),
            _ => self.return_type.clone().map(|t| (t, &[][..])),
        }
    }

    /// Its return annotation names no object of its own: there is none, or it is `ModuleType`.
    fn annotation_names_no_object(&self) -> bool {
        self.return_type
            .as_ref()
            .is_none_or(|t| t.obj().is_some_and(|o| is_any_module(&o.class)))
    }

    /// What every return gives past the annotation ([`ReturnFact::ty`]), while no other module
    /// rebinds a global it was inferred from ([`ReturnFact::ty_globals`]).
    pub(crate) fn inferred_return(&self, project: &ProjectDecls) -> Option<&Ty> {
        let r = self.returns.as_deref()?;
        let rebound = r.ty_globals.iter().any(|g| project.rebound_elsewhere(g));
        r.ty.as_ref().filter(|_| !rebound)
    }

    pub(crate) fn call_type(&self, returned: Ty) -> Ty {
        if self.called_as_defined && self.is_async && !self.is_generator {
            Ty::Coroutine(Box::new(returned))
        } else {
            returned
        }
    }

    /// Calling it creates no coroutine: it is a plain `def` or a generator function that no
    /// decorator replaces (`EFF107`: no new task). A plain `def` may still return a coroutine
    /// another call created; that is not followed.
    pub fn makes_no_coroutine(&self) -> bool {
        self.called_as_defined && (!self.is_async || self.is_generator)
    }

    /// The parameter a positional argument binds in `slot` (counted from the receiver): one of
    /// the positional parameters. A position past them lands in `*args`, a new tuple holding
    /// it, or the call raises `TypeError`; it never binds a keyword-only parameter.
    pub fn positional_param(&self, slot: usize) -> Option<usize> {
        (slot < usize::from(self.positional_params)).then_some(slot)
    }

    /// The position of `*args`, the tuple holding the positional arguments past the positional
    /// parameters (it follows the named parameters, before `**kwargs`).
    pub fn varargs_param(&self) -> Option<usize> {
        let after = 1 + usize::from(self.has_kwargs);
        self.param_names
            .len()
            .checked_sub(after)
            .filter(|_| self.has_varargs)
    }

    /// The parameter a keyword argument `name` binds: a named parameter past the
    /// positional-only ones. Any other keyword (one naming a positional-only parameter, `*args`,
    /// or `**kwargs`) lands in `**kwargs`, or the call raises `TypeError`.
    pub fn keyword_param(&self, name: &str) -> Option<usize> {
        let first = usize::from(self.positional_only);
        let named = self
            .param_names
            .get(first..usize::from(self.named_params))?;
        Some(first + named.iter().position(|p| p == name)?)
    }
}

/// What a name an annotation reads is bound to ([`ModuleDecls::lookup`]).
#[derive(Debug)]
enum NameBinding {
    /// An object, by its fqn.
    Object(String),
    /// A binding of the function or the module whose object heki does not know (an assignment,
    /// a parameter, several imports): the name reads no builtin and no `typing` object.
    Unknown,
    /// No binding of the function or the module, and no builtin class heki types.
    Unbound,
}

impl NameBinding {
    fn object(self) -> Option<String> {
        match self {
            Self::Object(fqn) => Some(fqn),
            Self::Unknown | Self::Unbound => None,
        }
    }
}

/// The names a function binds itself, which an annotation of one of its locals reads before the
/// module's ([`TypeCtx::local_names`]): each with what it names when one import statement is its
/// only binding, and with nothing otherwise (an assignment, a parameter, two imports), since
/// which binding the annotation reads is not decided.
#[derive(Debug, Default)]
pub(crate) struct LocalNames(FxHashMap<String, Option<String>>);

impl LocalNames {
    /// The function's binding of `name`: `Some(None)` for one heki names no object for.
    fn binding(&self, name: &str) -> Option<Option<&str>> {
        self.0.get(name).map(Option::as_deref)
    }

    /// Whether the function binds the first name of the dotted `text` itself.
    fn shadows(&self, text: &str) -> bool {
        self.0.contains_key(text.split('.').next().unwrap_or(text))
    }
}

/// Per-module declarations other modules need (stored in `FileSummary`, IR-free).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleDecls {
    pub module_name: String,
    pub is_package: bool,
    /// Module-scope runtime imports: `bound name -> fqn`.
    pub imports: BTreeMap<String, String>,
    /// `if TYPE_CHECKING:` imports: for annotation resolution, and for another module's name of
    /// a type the module binds no other way ([`Self::type_checking_binding`]).
    pub type_checking_imports: BTreeMap<String, String>,
    /// The names of [`Self::type_checking_imports`] that no other statement of the module
    /// stores or deletes: at run time the module does not bind them.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub type_checking_only: BTreeSet<String>,
    /// Class qualname -> declaration. Boxed, like `functions`: a map node has room for eleven
    /// entries and most modules define a few, so a declaration kept in the node would cost
    /// eleven times its size.
    pub classes: BTreeMap<String, Box<ClassDecl>>,
    /// Function qualname -> declaration (module body excluded).
    pub functions: BTreeMap<String, Box<FunctionDecl>>,
    /// Whether a class or function here is a member written with a private name (`def __run`
    /// in class `Parser`, held as `_Parser__run`): only then may a path name a definition by
    /// another spelling ([`Self::definition_path`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub private_members: bool,
    /// Module global `name -> type` of the instance it holds (`client = httpx.Client()`): the
    /// join of every store of the module (`stores::record`).
    pub global_types: BTreeMap<String, Ty>,
    /// Module globals bound once, by the module body, to a dict display whose keys are string
    /// literals and some of whose values are functions, classes, or instances whose fields hold
    /// them, that nothing changes in
    /// place (`HANDLERS = {"create": create, "drop": drop}`): each key with its value's type
    /// (`None`: unknown), in order (`stores::record`). `HANDLERS["create"]` is that value
    /// ([`ProjectDecls::table`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tables: BTreeMap<String, Table>,
    /// The annotations of the module globals that declare their items (`SLOTS: dict[str, Slot]`,
    /// `HANDLERS: dict[str, Callable]`), by name: what another module changing the object in
    /// place leaves of the type, whose items the values stored may refine
    /// (`ProjectDecls::refine`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub declared_items: BTreeMap<String, Ty>,
    /// Module-level names bound once to another object by name (`_set = object.__setattr__`,
    /// `run = helpers.run`), or so after imports of the name that binding always replaces, by
    /// the fqn of that object ([`module_aliases`]): as a value or in
    /// an annotation, in the module or imported from it, the name is that object, unless
    /// another module rebinds it ([`ProjectDecls::object_named`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
    /// Module-level classes and functions the module body binds last to different objects on
    /// different paths (`def fetch(...)`, then `if flag: fetch = _quiet`), with the fqn of each
    /// object the name may hold once the module has run, the definition's own among them
    /// ([`module_aliases`]). A definition the body binds last to one other object on every path
    /// is an alias ([`Self::aliases`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub alias_paths: BTreeMap<String, Vec<String>>,
    /// Module-level names an annotation reads as a type they name, though they are no class, by
    /// name, with that type ([`type_names`]): a type alias (`Operation: TypeAlias = Added |
    /// Removed`, `type Operation = Added | Removed`, or an assignment of a subscript or a union
    /// the body makes once, `Operation = Added | Removed`), the type it names; a `TypeVar` with
    /// a bound or constraints (`C = TypeVar("C", bound=View)`), the bound or the union of the
    /// constraints, which a value of the variable's type is one of. They are so in the module
    /// and where an import names them ([`ProjectDecls::declared_ty`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub type_names: BTreeMap<String, Ty>,
    /// The [`Self::type_names`] whose type may also be `None`, which their type leaves out
    /// (`MaybeModel = Optional[Model]`, a `TypeVar` bound to `Model | None`): calling a
    /// `Callable[..., MaybeModel]` gives no known class ([`Self::admits_none`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub nullable_type_names: BTreeSet<String>,
    /// The module globals the module stores or deletes (in its body, or in a function after
    /// `global name`) that are no runtime import, class, or function of it: its own objects,
    /// of a value heki may not know ([`Symbol::Unknown`]), never a builtin of the same name.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub unknown_globals: BTreeSet<String>,
    /// The runtime imports the module binds again (another module-level store, a store or
    /// `del` in a function declaring the name `global`), and all of them when a star import,
    /// `exec`, or `globals()` may rebind its names: such a name may hold another object than
    /// the import, so no constant of the imported module ([`Self::fixed_import`]).
    /// Boxed: few modules bind an import again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebound_imports: Option<Box<BTreeSet<String>>>,
    /// Module globals bound to a small display that no function of the module grows
    /// (`heki_ir::ModuleConstants::displays`): other modules read them through
    /// [`ProjectDecls::small_global`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub small_globals: BTreeMap<String, SmallDisplay>,
    /// Globals of other modules, by the fqn this module's imports give them, whose object a
    /// function of this module may grow or that it rebinds (`cfg.QUEUE.extend(more)`,
    /// `cfg.QUEUE = []`): no longer a display their own module fixes
    /// ([`ProjectDecls::grown_elsewhere`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub grown_globals: BTreeSet<String>,
    /// The `grown_globals` this module rebinds or deletes (`cfg.QUEUE = []`, `del
    /// cfg.QUEUE`, `setattr(cfg, "QUEUE", [])`): bound to another object, so not even a tuple
    /// keeps its display, and no longer the string constant their own module binds
    /// ([`ProjectDecls::rebound_elsewhere`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub rebound_globals: BTreeSet<String>,
    /// The `__getattr__` of other modules this module loads, by the fqn its imports give it
    /// (`keys.__getattr__`, `from keys import __getattr__`): it may call the hook for a name
    /// the hook's module binds ([`ProjectDecls::rebound_by_getattr`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub getattr_loads: BTreeSet<String>,
    /// The names the module binds stay fixed past its `__getattr__` only while Python alone
    /// calls it (`heki_ir::ModuleConstants::getattr_rebinds`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub getattr_rebinds: bool,
    /// The module's own functions and classes, by qualname (`f`, `C`, `C.m`), whose name its
    /// code binds to another object than their `def` or `class` statement: another store of
    /// the module global (also in a function after `global f`, and any name when `globals()`
    /// or `exec` may bind it) or of the class-body name, a deletion of either, or a store of a
    /// function's `__code__` ([`rebound_definitions`]). A call through such a name may run
    /// that object instead ([`ProjectDecls::call_rebound`]). A module global the module body
    /// binds again to named objects only is none: the name holds them ([`Self::aliases`],
    /// [`Self::alias_paths`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub rebound_definitions: BTreeSet<String>,
    /// The module's classes and functions whose `class` or `def` statement a function or a
    /// class body runs under a `global` declaration of the name (`global Config`, then
    /// `class Config: ...` in `_define`), by qualname, with the qualname of that scope
    /// ([`made_in`]). CPython names them as module-level definitions, which they are once that
    /// scope has run; the code written in them is that scope's. Boxed: few modules have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made_in: Option<Box<BTreeMap<String, String>>>,
    /// What the module's code stores into the globals of other project modules and into the
    /// instances of their classes (`stores::record`). Boxed: most modules store into none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stores_elsewhere: Option<Box<StoresElsewhere>>,
    /// What the module's calls making instances of project classes bind to their read-only
    /// container fields (`fields::field_values`). Boxed: most modules make none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_values: Option<Box<FieldValues>>,
    /// The types of the module's globals and attributes without what deciding them assumed
    /// reading them, where those differ: they hold once another module stores into the module's
    /// globals or into the instances of its classes (`stores::record`). Boxed: most modules
    /// have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_fallbacks: Option<Box<StoreFallbacks>>,
    /// The module calls `__setstate__` on an object, or reads the method or names it, outside a
    /// `__setstate__` handing its own state on: the state may be anything
    /// ([`ProjectDecls::restores_hold`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub calls_setstate: bool,
    /// The names a star import of the module binds when the module fixes them: its `__all__`,
    /// bound once to a tuple or an unchanged list of string literals (`stores::exported_names`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exports: Option<Box<[String]>>,
    /// The modules the module's runtime star imports name (`from ._utils import *`), in source
    /// order: they bind the names the module binds no other way (`ProjectDecls::star_binding`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub star_imports: Option<Box<[String]>>,
    /// The module's own module-level classes and functions that one of its star imports may bind
    /// again after their statement has run, each with the index in `star_imports` of the first
    /// such import: the first after the name's first binding in the module body, or the first of
    /// all when the body does not bind the name on every path ([`ProjectDecls::late_star_binding`]).
    /// Boxed: few modules star-import after a definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_stars: Option<Box<BTreeMap<String, u32>>>,
    /// The names no statement of the module binds that its `__getattr__` (PEP 562) serves,
    /// where heki follows the hook for that literal name ([`getattr_exports::getattr_exports`]):
    /// another module's `from module import name` or `module.name` reads that
    /// ([`ProjectDecls::served_by_getattr`]). Boxed: few modules have such a hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub getattr_exports: Option<Box<GetattrExports>>,
    /// The module-level functions that return a function another module may bind as its
    /// `__getattr__` (`__getattr__, __dir__ = make_hooks(__name__, _EXPORTS)`), by qualname, with
    /// what each such hook serves for the arguments of the call making it
    /// ([`getattr_exports::hook_factories`]). Boxed: few modules have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_factories: Option<Box<BTreeMap<String, Box<[MadeHook]>>>>,
    /// The packages whose `__path__` the module changes, by the fqn its imports give them
    /// (`plugins.__path__.append(d)`), or by its own name when it is a package changing its own
    /// (`__path__ = extend_path(__path__, __name__)`), and how
    /// ([`ProjectDecls::holds_modules_starting_with`]). Boxed: few modules change one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_changes: Option<Box<BTreeMap<String, PathChange>>>,
    /// String constants, by name: module-level names bound once (`KEY = "_key"`,
    /// `KEY: Final = "_key"`), and class-body names declared `ClassVar` or `Final` and bound
    /// once (`KEY: ClassVar[str] = "_key"`), under `Class.KEY`. A constant names an attribute in
    /// `obj.__dict__[KEY]`, `getattr(obj, KEY)`, and `setattr(obj, KEY, v)`, and a key in
    /// `obj.attr[KEY]`, as its literal would. A module-level name bound once to a tuple display
    /// of string constants (`RECORDS = (ORIGIN_KEY, "_number")`) is one too: a loop over it
    /// takes the names of its items, as a loop over the display would.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub str_constants: BTreeMap<String, StrConstant>,
    /// Module-level names bound once to a number or boolean literal (`PAUSE = 0`, `DELAY = 0.5`,
    /// `STRICT = False`), as [`Self::str_constants`] holds the strings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub literal_constants: BTreeMap<String, LiteralConstant>,
    /// Module-level names the module body binds once, outside a loop, to a value that names a
    /// file ([`names_a_file`]: `DB_PATH = Path(__file__).parent / "app.db"`), with the
    /// conditions of a type variable: neither `exec` / `globals()` nor a star import may rebind
    /// them ([`ProjectDecls::file_constant`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub file_constants: BTreeSet<String>,
    /// Module-level names bound once, outside a loop, to a new object of the builtin `object`
    /// (`MISSING = object()`), with the conditions of a string constant: no import binds the
    /// name, and neither `exec` / `globals()` nor a star import may rebind it. Such an object
    /// is equal only to itself, and `copy.deepcopy` makes a new one
    /// ([`ProjectDecls::object_sentinel`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub object_sentinels: BTreeSet<String>,
    /// The classes the module registers with `copyreg` (`copyreg.pickle(C, reduce)`,
    /// `copyreg.dispatch_table[C] = reduce`), which `copy.deepcopy` then copies by the
    /// registered function, by the fqn the module's names give them; `*` for a class heki
    /// cannot name ([`ProjectDecls::copy_registered`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub copy_registrations: BTreeSet<String>,
    /// The classes the module registers with another class's `register` (`Mapping.register(C)`,
    /// `@Mapping.register`), by the fqn the module's names give the class registered with; `*`
    /// for a class heki cannot name ([`ProjectDecls::implementers`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub abc_registrations: BTreeMap<String, BTreeSet<String>>,
    /// What the module binds its globals to (in its body, and in functions declaring them
    /// `global`), for the globals one binding of which is a queue: the join of every binding
    /// (`crate::capacity`). Any other global has an unknown capacity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub global_capacities: BTreeMap<String, CapacityOf>,
    /// A file of the project that heki could not read or parse: the module exists, and heki
    /// knows nothing it defines ([`Self::unread_module`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unread: bool,
}

/// The entries of a table ([`ModuleDecls::tables`]): each string key with the type of its value
/// (`None`: unknown), in order.
pub type Table = Box<[(Box<str>, Option<Ty>)]>;

/// What a module's `__getattr__` serves ([`ModuleDecls::getattr_exports`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetattrExports {
    /// By name, what the hook serves: a named object (`Ty::Ref`: what an import binds, a class
    /// a function the hook calls defines in its body), or a class object that may be a
    /// subclass of the class (`Ty::Class`: what a function annotated `-> type[C]` returns).
    pub served: BTreeMap<String, Ty>,
    /// The module's own globals the hook reads (fqns), the hook's own name among them, and
    /// those the return facts of the functions it calls were inferred from
    /// (`ReturnFact::ty_globals`): another module rebinding one, or storing into one, may make
    /// the hook serve other objects ([`ProjectDecls::getattr_open`]).
    pub globals: Box<[String]>,
    /// The call of a project function the module binds its `__getattr__` to, when that is
    /// where the hook comes from: what it serves is what the function's hook
    /// ([`ModuleDecls::hook_factories`]) serves for the call's arguments
    /// ([`ProjectDecls::served_by_getattr`]), and `served` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made_by: Option<Box<MadeBy>>,
}

/// The call making a module's `__getattr__` ([`GetattrExports::made_by`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MadeBy {
    /// The function called, by the fqn the module's names give it.
    pub factory: String,
    /// Which item of the tuple the call returns is the hook (`hooks[0]`, the first target of
    /// an unpacking), `None` for the value itself.
    pub item: Option<u8>,
    pub args: Vec<HookArg>,
    pub kwargs: Vec<(String, HookArg)>,
}

/// An argument of the call making a module's `__getattr__`, as far as the hook may read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookArg {
    /// A global of the module, by its name: the hook reads it as a table when it is one
    /// ([`ModuleDecls::tables`]).
    Global(String),
    /// A literal (`False`, `None`).
    Literal(ConstLiteral),
    /// Any other value (`__name__`).
    Other,
}

/// What a function a factory returns serves once a module binds it as its `__getattr__`
/// ([`ModuleDecls::hook_factories`]): under the keys of the table the factory takes, what each
/// key's value is or what calling it gives, and under the literal names its own code decides,
/// what [`GetattrExports::served`] would say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MadeHook {
    /// Which item of the tuple the factory returns the hook is, `None` for the value itself.
    pub item: Option<u8>,
    /// The factory's parameter holding the table the hook reads by the name, and what it gives
    /// for a key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<(HookParam, KeyServes)>,
    /// What the hook serves under the literal names its code decides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub served: BTreeMap<String, Ty>,
    /// The literal names the hook's tests or its own tables name: the hook may treat a key of
    /// the factory's table among them otherwise than the other keys, so such a key gets nothing.
    #[serde(default, skip_serializing_if = "<[String]>::is_empty")]
    pub literals: Box<[String]>,
    /// The factory's parameters that guard a store of the hook (`if cache: setattr(module,
    /// name, value)`, which may bind the name in the module).
    #[serde(default, skip_serializing_if = "<[HookParam]>::is_empty")]
    pub guards: Box<[HookParam]>,
    /// The combinations of [`Self::guards`], as bit sets of their positions, that reach a
    /// store when taken together (`if cache: if enabled: setattr(...)` is the pair): the hook
    /// serves only when the call binds to a false literal at least one guard of each.
    #[serde(default, skip_serializing_if = "<[u32]>::is_empty")]
    pub storing: Box<[u32]>,
    /// The globals of the factory's module the hook and the factory read (fqns), the factory
    /// among them ([`GetattrExports::globals`]).
    pub globals: Box<[String]>,
}

/// What a hook made by a factory serves under a key of the factory's table
/// ([`MadeHook::table`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyServes {
    /// The key's value.
    Value,
    /// What calling the key's value gives.
    Called,
}

/// A parameter of a hook factory ([`MadeHook`]): what a call binds it to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookParam {
    pub position: u8,
    pub name: String,
    pub pos_only: bool,
    pub kw_only: bool,
    /// The default when it is a literal.
    pub default: Option<ConstLiteral>,
}

/// How a module changes the `__path__` of a package, the directories Python finds the modules
/// under the package in, which start as the package's own directory
/// ([`ModuleDecls::path_changes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathChange {
    /// Directories after the package's own (`__path__.append(d)`, `__path__ += [d]`,
    /// `pkgutil.extend_path`, `pkg_resources.declare_namespace`): a module the package's own
    /// directory holds is still found there first; any other name may be found outside the
    /// project.
    Appended,
    /// Any other change, or a use heki does not follow (`__path__.insert(0, d)`, `__path__ =
    /// [d]`, the list handed to a function): any module under the package may be found outside
    /// the project.
    Replaced,
}

/// A number or boolean constant of a module ([`ModuleDecls::literal_constants`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiteralConstant {
    /// A `Bool`, `Int`, or `Float` literal.
    pub value: heki_ir::ConstLiteral,
    /// As [`StrConstant::rebindable`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rebindable: bool,
}

/// A string constant of a module or class ([`ModuleDecls::str_constants`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrConstant {
    pub value: StrValue,
    /// A name without `Final`: another module rebinding it (`keys.KEY = "x"`,
    /// `Keys.KEY = "x"`, [`ProjectDecls::rebound_elsewhere`]) makes it no constant.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rebindable: bool,
    /// A class-body name declared `Final` without `ClassVar`: in a class whose annotations
    /// declare instance fields (a dataclass, a class a `dataclass_transform` builds) it is a
    /// field, which each instance holds itself ([`ProjectDecls::field_defaults`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub declares_field: bool,
}

/// What a string constant is bound to ([`StrConstant`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrValue {
    /// `KEY = "_key"`; a module-level one shares its text with the module's constants
    /// (`heki_ir::ModuleConstants::strs`), and with the names bound to it there.
    Literal(Arc<str>),
    /// `KEY = OTHER` / `KEY = keys.OTHER`: the fully qualified name of the constant it is
    /// bound to, resolved within the module (chased by [`ProjectDecls::str_constant`]).
    Alias(String),
    /// `RECORDS = (ORIGIN_KEY, "_number")`, a module-level tuple display: its items in order,
    /// each a literal or an alias ([`ProjectDecls::str_tuple`]). A module-level tuple of
    /// literals shares its texts with the module's constants
    /// (`heki_ir::ModuleConstants::str_tuples`).
    Tuple(Box<[StrValue]>),
}

impl StrValue {
    /// The text of a literal, which a constant bound to a tuple or another name is not.
    fn text(&self) -> Option<&str> {
        match self {
            Self::Literal(v) => Some(v),
            _ => None,
        }
    }
}

impl ModuleDecls {
    /// `path`, a qualname in this module, as the qualname of the class or function it names: a
    /// class member reached by the mangled name its class holds it under (`Parser._Parser__run`)
    /// is the definition written with the private name (`def __run`, qualname `Parser.__run`).
    fn definition_path<'p>(&self, path: &'p str) -> Cow<'p, str> {
        // Every lookup of a definition comes here, and few modules define a private member.
        if !self.private_members {
            return Cow::Borrowed(path);
        }
        let defines = |q: &str| self.functions.contains_key(q) || self.classes.contains_key(q);
        if defines(path) {
            return Cow::Borrowed(path);
        }
        match mangling::source_path(path) {
            Some(written) if defines(&written) => Cow::Owned(written),
            _ => Cow::Borrowed(path),
        }
    }

    /// The fqn the runtime import of `name` binds, when nothing else in the module binds the
    /// name ([`Self::rebound_imports`]): reading it gives what the import bound.
    pub fn fixed_import(&self, name: &str) -> Option<&String> {
        self.imports.get(name).filter(|_| {
            !self
                .rebound_imports
                .as_ref()
                .is_some_and(|names| names.contains(name))
        })
    }

    /// The fqn a function of the module reads as the global `name`: what its runtime import
    /// binds ([`Self::fixed_import`]), or the module's own global (`ReturnFact::ty_globals`).
    pub fn global_read(&self, name: &str) -> String {
        self.fixed_import(name)
            .cloned()
            .unwrap_or_else(|| format!("{}.{name}", self.module_name))
    }

    /// Whether a statement of the module itself binds the module global `name` at run time: an
    /// import, a `def` or `class` statement, a module alias, or any other store or deletion.
    pub fn binds(&self, name: &str) -> bool {
        self.imports.contains_key(name)
            || self.aliases.contains_key(name)
            || self.classes.contains_key(name)
            || self.functions.contains_key(name)
            || self.unknown_globals.contains(name)
    }

    /// What the `if TYPE_CHECKING:` import of `name` binds when no statement of the module binds
    /// the name at run time: a type checker reads `from module import name` elsewhere as that
    /// ([`Chase::reads_type_checking`]).
    pub(crate) fn type_checking_binding(&self, name: &str) -> Option<&String> {
        self.type_checking_imports
            .get(name)
            .filter(|_| self.type_checking_only.contains(name))
    }

    /// The declarations of a module heki could not read or parse: none. A name under it is
    /// unresolved ([`ProjectDecls::outside`]), never looked up in the catalog: the module may
    /// shadow a module the catalog lists, as any project module does.
    pub fn unread_module(module_name: &str, is_package: bool) -> Self {
        Self {
            module_name: module_name.to_owned(),
            is_package,
            unread: true,
            ..Self::default()
        }
    }

    /// The pass-1 declarations these were completed from (`project_decls_for`): without the
    /// inferred instance attribute and module global types, the queue capacities, and the
    /// offloading decorators resolved through the project (`FunctionDecl::offloaded_by`).
    pub fn structural(&self) -> Self {
        let mut out = self.clone();
        for class in out.classes.values_mut() {
            class.attr_types.clear();
            class.attr_stores = Box::default();
            class.stores_any_attr = false;
            class.adds_unnamed_attrs = false;
            class.restores_state = false;
            class.in_place_attrs = Box::default();
            class.unowned_attrs = Box::default();
            class.attr_capacities.clear();
        }
        for function in out.functions.values_mut() {
            function.offloaded_by = None;
            function.state_of_self = false;
            if function.forwards.is_some() {
                function.returns_param = None;
            }
        }
        out.global_types.clear();
        out.tables.clear();
        out.declared_items.clear();
        out.stores_elsewhere = None;
        out.field_values = None;
        out.store_fallbacks = None;
        out.global_capacities.clear();
        out
    }

    /// Whether the decorator `text` of a function of this module is one of `names`
    /// (`cached_property`, `lru_cache`, ...) by the last name of the symbol its head is bound
    /// to ([`Self::decorator_symbol`]): an alias counts by that symbol's last name (`from
    /// functools import cached_property as cached`, `cached = functools.cached_property`); an
    /// object the module binds itself is none of `names` (an identity `def cache(fn)` caches
    /// nothing).
    pub fn decorator_is(&self, text: &str, names: &[&str]) -> bool {
        self.decorator_symbol(text)
            .is_some_and(|s| names.contains(&s.rsplit('.').next().unwrap_or(&s)))
    }

    /// The symbol the head of the decorator `text` is bound to at module scope: through this
    /// module's imports and aliases (`typing.final` of `@final` after `from typing import
    /// final`, of `@t.final` after `import typing as t`), and a builtin for a name no statement
    /// of the module binds (`builtins.staticmethod`, also a name only a star import may bind);
    /// `None` for an object the module binds itself (a class or function it defines, any other
    /// store of the name).
    pub fn decorator_symbol(&self, text: &str) -> Option<String> {
        let (head, _) = decorator_parts(text);
        let first = head.split('.').next().unwrap_or(head);
        if self.imports.contains_key(first) || self.aliases.contains_key(first) {
            self.resolve_dotted(head, false)
        } else if self.binds(first) {
            None
        } else {
            Some(format!("builtins.{head}"))
        }
    }

    /// The object the dotted name `text` reads at module scope ([`Self::lookup`]).
    /// `type_checking` also consults `if TYPE_CHECKING:` imports.
    pub(crate) fn resolve_dotted(&self, text: &str, type_checking: bool) -> Option<String> {
        self.lookup(text, type_checking, &LocalNames::default())
            .object()
    }

    /// What the dotted name `text` reads, in a function that binds `locals` itself
    /// ([`TypeCtx::local_names`]): the function's own binding of its first name, then the
    /// module's (runtime imports, `if TYPE_CHECKING:` ones with `type_checking`, aliases, local
    /// definitions, any other store), then a builtin class. A binding of the module or the
    /// function shadows the builtin of the same name, whether or not heki knows its object.
    fn lookup(&self, text: &str, type_checking: bool, locals: &LocalNames) -> NameBinding {
        let (first, rest) = match text.split_once('.') {
            Some((f, r)) => (f, Some(r)),
            None => (text, None),
        };
        let join = |base: &str| match rest {
            Some(r) => NameBinding::Object(format!("{base}.{r}")),
            None => NameBinding::Object(base.to_owned()),
        };
        if let Some(local) = locals.binding(first) {
            return local.map_or(NameBinding::Unknown, join);
        }
        if let Some(t) = self.imports.get(first) {
            return join(t);
        }
        if type_checking && let Some(t) = self.type_checking_imports.get(first) {
            return join(t);
        }
        if let Some(t) = self.aliases.get(first) {
            return join(t);
        }
        if self.classes.contains_key(first) || self.functions.contains_key(first) {
            return NameBinding::Object(format!("{}.{text}", self.module_name));
        }
        if self.unknown_globals.contains(first) {
            return NameBinding::Unknown;
        }
        if rest.is_none() && is_builtin_type(first) {
            return NameBinding::Object(format!("builtins.{first}"));
        }
        NameBinding::Unbound
    }

    /// The name of the `typing` / `typing_extensions` object the annotation name `head` reads
    /// (`Optional` for `Optional`, `t.Optional` or `Opt` after `from typing import Optional as
    /// Opt`), or the name itself when nothing binds it (`"Optional[Model]"` without the import):
    /// `None` when it reads any other object, or one heki does not know.
    fn typing_name(&self, head: &str, locals: &LocalNames) -> Option<String> {
        match self.lookup(head, true, locals) {
            NameBinding::Object(fqn) => fqn
                .strip_prefix("typing.")
                .or_else(|| fqn.strip_prefix("typing_extensions."))
                .map(str::to_owned),
            NameBinding::Unbound => Some(head.rsplit('.').next().unwrap_or(head).to_owned()),
            NameBinding::Unknown => None,
        }
    }

    /// The type an annotation names (receiver typing): a class with the arguments of a known
    /// generic container (`list[Model]`, `Mapping[str, Model]`, `tuple[Model, int]`,
    /// `Iterator[Model]`), or the class object `type[Model]` / `typing.Type[Model]` names, after
    /// `Optional` / `| None` / `Annotated` / `Final` / `ClassVar` / `Required` / `NotRequired` /
    /// `ReadOnly` and quotes. An unresolved name of a `typing` generic counts as that generic
    /// (`"Iterable[Model]"` without the import).
    pub fn resolve_annotation_ty(&self, text: &str) -> Option<Ty> {
        self.resolve_annotation_ty_in(text, &LocalNames::default())
    }

    /// [`Self::resolve_annotation_ty`] for an annotation of a local of a function that binds
    /// `locals` itself ([`TypeCtx::local_names`]): every name it reads is looked up by
    /// [`Self::lookup`].
    pub(crate) fn resolve_annotation_ty_in(&self, text: &str, locals: &LocalNames) -> Option<Ty> {
        let t = unquote(text);
        let mut parts = split_top_level(t, '|');
        if parts.len() > 1 {
            parts.retain(|p| *p != "None");
            return self.union_annotation(&parts, locals);
        }
        let Some((head, inner)) = subscript(t) else {
            // `typing.Self` (also unimported, in a string annotation): the receiver's type.
            if self.typing_name(t, locals).as_deref() == Some("Self") {
                return Some(Ty::SelfType);
            }
            if let Some(named) = self.module_type_name(t, locals) {
                return Some(named.clone());
            }
            return self
                .resolve_type_head(t, locals)
                .object()
                .filter(|c| !special_form(c))
                .map(Ty::inst);
        };
        match self.typing_name(head, locals).as_deref() {
            Some(
                "Optional" | "Annotated" | "Final" | "ClassVar" | "Required" | "NotRequired"
                | "ReadOnly",
            ) => return self.resolve_annotation_ty_in(first_arg(inner), locals),
            Some("Union") => {
                let mut members = split_top_level(inner, ',');
                members.retain(|p| *p != "None");
                return self.union_annotation(&members, locals);
            }
            _ => {}
        }
        if self.is_type_form(head, locals) {
            // Whether `C` is a project class, and which, is decided across the project
            // (`ProjectDecls::declared_ty`).
            let class = self
                .resolve_annotation_ty_in(first_arg(inner), locals)?
                .obj()?
                .class
                .clone();
            return Some(Ty::Class(class));
        }
        let class = match self.resolve_type_head(head, locals) {
            NameBinding::Object(class) => class,
            NameBinding::Unbound => typing_generic(head.rsplit('.').next().unwrap_or(head))?,
            NameBinding::Unknown => return None,
        };
        if special_form(&class) {
            return None;
        }
        let args = split_top_level(inner, ',');
        let elem = |i: usize| {
            Elem::of(
                args.get(i)
                    .and_then(|a| self.resolve_annotation_ty_in(a, locals)),
            )
        };
        let args = match generic(&class) {
            // Whether the class is a project class with type parameters is decided across the
            // project (`ProjectDecls::declared_ty`).
            None => Args::type_args(
                args.iter()
                    .map(|a| match self.admits_none(a) {
                        true => Elem::Unknown,
                        false => Elem::of(self.resolve_annotation_ty(a)),
                    })
                    .collect(),
            ),
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
            Some(Generic::Held) => Args::result(elem(0)),
            // `Callable[[A, B], T]`, `Callable[..., T]`: a result that may be `None` is no
            // instance of a class, though the annotation reads `T | None` as `T`.
            Some(Generic::Returns) => match args.as_slice() {
                [_, result] if !self.admits_none_in(result, locals) => {
                    Args::returns(self.resolve_annotation_ty_in(result, locals))
                }
                _ => Args::None,
            },
        };
        Some(Ty::container(&class, args))
    }

    /// The type the module's name `text` names ([`Self::type_names`]), unless the function
    /// binds the name itself (`locals`).
    fn module_type_name(&self, text: &str, locals: &LocalNames) -> Option<&Ty> {
        if locals.shadows(text) {
            return None;
        }
        self.type_names.get(text)
    }

    /// Whether the type the annotation `text` names may be `None`: `None`, a union with a member
    /// that may be (`T | None`, `Optional[T]`, `Union[T, None]`), what the qualifiers
    /// [`Self::resolve_annotation_ty`] looks through wrap, and a name of the module whose type
    /// may be ([`Self::nullable_type_names`]; one an import names is
    /// [`ProjectDecls::nullable_type_name`]).
    pub(crate) fn admits_none(&self, text: &str) -> bool {
        self.admits_none_in(text, &LocalNames::default())
    }

    /// [`Self::admits_none`] for an annotation of a local of a function that binds `locals`
    /// itself ([`TypeCtx::local_names`]).
    fn admits_none_in(&self, text: &str, locals: &LocalNames) -> bool {
        let t = unquote(text);
        let parts = split_top_level(t, '|');
        if parts.len() > 1 {
            return parts.iter().any(|p| self.admits_none_in(p, locals));
        }
        let Some((head, inner)) = subscript(t) else {
            return t == "None" || (!locals.shadows(t) && self.nullable_type_names.contains(t));
        };
        match self.typing_name(head, locals).as_deref() {
            Some("Optional") => true,
            Some("Union") => split_top_level(inner, ',')
                .iter()
                .any(|m| self.admits_none_in(m, locals)),
            Some("Annotated" | "Final" | "ClassVar" | "Required" | "NotRequired" | "ReadOnly") => {
                self.admits_none_in(first_arg(inner), locals)
            }
            _ => false,
        }
    }

    /// The members of a union annotation other than `None`: one type, or instances of at most
    /// [`crate::types::MAX_DECLARED_UNION`] classes.
    fn union_annotation(&self, members: &[&str], locals: &LocalNames) -> Option<Ty> {
        if let [one] = members {
            return self.resolve_annotation_ty_in(one, locals);
        }
        let mut objs = Vec::new();
        for m in members {
            let t = self.resolve_annotation_ty_in(m, locals)?;
            if t.members().is_empty() {
                return None;
            }
            objs.extend(t.members().iter().cloned());
        }
        Ty::of_declared(objs)
    }

    /// Whether the subscripted head `head` is `type` or `typing.Type` (an unresolved `Type`
    /// counts, as an unresolved `typing` generic does).
    fn is_type_form(&self, head: &str, locals: &LocalNames) -> bool {
        match self.lookup(head, true, locals) {
            NameBinding::Object(fqn) => matches!(
                fqn.as_str(),
                "builtins.type" | "typing.Type" | "typing_extensions.Type"
            ),
            NameBinding::Unbound => matches!(head, "type" | "Type"),
            NameBinding::Unknown => false,
        }
    }

    /// The class the annotation name `head` reads ([`Self::lookup`]), with the `typing` aliases
    /// of builtin containers (`List`, `Dict`, ...) read as those containers, also when nothing
    /// binds the name.
    fn resolve_type_head(&self, head: &str, locals: &LocalNames) -> NameBinding {
        if head.is_empty()
            || head == "None"
            || !head
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        {
            return NameBinding::Unknown;
        }
        let typing_alias = |name: &str| match name {
            "List" => Some("list"),
            "Dict" | "DefaultDict" | "OrderedDict" => Some("dict"),
            "Set" => Some("set"),
            "FrozenSet" => Some("frozenset"),
            "Tuple" => Some("tuple"),
            _ => None,
        };
        let builtin = |b: &str| NameBinding::Object(format!("builtins.{b}"));
        match self.lookup(head, true, locals) {
            NameBinding::Object(r) => match r.strip_prefix("typing.").and_then(typing_alias) {
                Some(b) => builtin(b),
                None => NameBinding::Object(r),
            },
            NameBinding::Unbound => match typing_alias(head.rsplit('.').next().unwrap_or(head)) {
                Some(b) => builtin(b),
                None => NameBinding::Unbound,
            },
            NameBinding::Unknown => NameBinding::Unknown,
        }
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

/// Whether the annotation `text` allows `None` (`T | None`, `Optional[T]`, `Union[T, None]`,
/// also inside `Annotated`, `Final`, `ClassVar`, and quotes), which the type it names
/// ([`ModuleDecls::resolve_annotation_ty`]) leaves out.
fn annotation_allows_none(text: &str) -> bool {
    let t = unquote(text);
    let parts = split_top_level(t, '|');
    if parts.len() > 1 {
        return parts.iter().any(|p| unquote(p) == "None");
    }
    let Some((head, inner)) = subscript(t) else {
        return t == "None";
    };
    match head.rsplit('.').next().unwrap_or(head) {
        "Optional" => true,
        "Union" => split_top_level(inner, ',')
            .iter()
            .any(|p| unquote(p) == "None"),
        "Annotated" | "Final" | "ClassVar" | "Required" | "NotRequired" | "ReadOnly" => {
            annotation_allows_none(first_arg(inner))
        }
        _ => false,
    }
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
) -> (Qualifiers, Option<ParamAccess>) {
    let (q, inner) = qualifiers(text, resolve);
    let container = inner
        .filter(|_| !q.class_var && !q.pseudo_field)
        .and_then(|t| param_access(t, resolve));
    (q, container)
}

/// What a parameter annotation promises about changing the argument (`MUT108`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamAccess {
    /// `Sequence[...]` / `Mapping[...]` / `AbstractSet[...]` / `ReadOnly[...]`: the argument
    /// is only read.
    ReadOnly,
    /// A mutable container type: the argument may be changed in place.
    Mutable,
}

/// The mutable container types besides the builtins, the derived classes, and the mutable ABCs of
/// the families of `list`, `dict`, and `set` ([`ContainerFamily`]), which [`param_access`]
/// reads as mutable too.
const OTHER_MUTABLE_TYPES: &[&str] = &["builtins.bytearray", "typing.Deque", "collections.deque"];

/// The access the parameter annotation `text` promises, after `Optional[T]`, `T | None`,
/// `Union[T, None]`, `Annotated[T, ...]`, and quotes; `None` for any other type (`Iterable`,
/// `Any`, a project class) and for unions of several types. An unresolved name only counts as
/// mutable (a report needs evidence).
pub(crate) fn param_access(
    text: &str,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Option<ParamAccess> {
    match annotated_head(text, resolve)? {
        AnnotatedHead::ReadOnly => Some(ParamAccess::ReadOnly),
        // The read-only ABCs (`typing.AbstractSet` is `collections.abc.Set`; `typing.Set` is
        // the builtin `set`), and the builtins, derived classes, and mutable ABCs of the families.
        AnnotatedHead::Resolved(fqn) => match ContainerFamily::role_of(&fqn) {
            Some(FamilyRole::ReadOnly) => Some(ParamAccess::ReadOnly),
            Some(FamilyRole::Builtin | FamilyRole::Derived | FamilyRole::Mutable) => {
                Some(ParamAccess::Mutable)
            }
            _ => (OTHER_MUTABLE_TYPES.contains(&fqn.as_str())).then_some(ParamAccess::Mutable),
        },
        AnnotatedHead::Unresolved(last) => MUTABLE_NAMES
            .contains(&last)
            .then_some(ParamAccess::Mutable),
    }
}

/// The builtin list, dict, or set a parameter annotation names, among the mutable container
/// types of [`param_access`]: `list`, `dict`, `set`, their `typing` aliases, and the mutable
/// abstract types (`MutableSequence`, `MutableMapping`, `MutableSet`), whose read-only types are
/// `Sequence`, `Mapping`, and `AbstractSet`. `None` for every other annotation.
pub(crate) fn mutable_container(
    text: &str,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Option<ContainerKind> {
    let name = match annotated_head(text, resolve)? {
        AnnotatedHead::ReadOnly => return None,
        AnnotatedHead::Resolved(fqn) => {
            return ContainerFamily::of(&fqn)
                .filter(|r| matches!(r.role, FamilyRole::Builtin | FamilyRole::Mutable))
                .map(|r| r.kind);
        }
        AnnotatedHead::Unresolved(last) if MUTABLE_NAMES.contains(&last) => last,
        AnnotatedHead::Unresolved(_) => return None,
    };
    match name {
        "list" | "List" | "MutableSequence" => Some(ContainerKind::List),
        "dict" | "Dict" | "MutableMapping" => Some(ContainerKind::Dict),
        "set" | "Set" | "MutableSet" => Some(ContainerKind::Set),
        _ => None,
    }
}

/// The names [`param_access`] reads as mutable containers when they do not resolve.
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

/// The type a parameter annotation names, as [`param_access`] reads it.
enum AnnotatedHead<'t> {
    /// `ReadOnly[...]`.
    ReadOnly,
    /// The resolved name of the type.
    Resolved(String),
    /// The last component of a name that does not resolve.
    Unresolved(&'t str),
}

/// The type the annotation `text` names after `Optional[T]`, `T | None`, `Union[T, None]`,
/// `Annotated[T, ...]`, and quotes; `None` for a union of several types.
fn annotated_head<'t>(
    text: &'t str,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Option<AnnotatedHead<'t>> {
    Some(head_of(text, resolve)?.named)
}

/// The type an annotation names ([`head_of`]) and where it is written.
struct Head<'t> {
    named: AnnotatedHead<'t>,
    /// The head as written, a slice of the annotation (`list` of `Optional["list[int]"]`).
    head: &'t str,
    /// The type arguments as written (`int`), `None` without a subscript.
    args: Option<&'t str>,
    /// The way to the head went through `Annotated[...]`.
    annotated: bool,
}

/// [`annotated_head`], with where the head is written.
fn head_of<'t>(text: &'t str, resolve: &impl Fn(&str) -> Option<String>) -> Option<Head<'t>> {
    let mut t = text;
    let mut annotated = false;
    loop {
        let u = unquote(t);
        let mut parts = split_top_level(u, '|');
        if parts.len() > 1 {
            parts.retain(|p| *p != "None");
            t = single(&parts)?;
            continue;
        }
        let (head, args) = head_and_args(u);
        let found = |named| {
            Some(Head {
                named,
                head,
                args,
                annotated,
            })
        };
        let Some(fqn) = resolve(head) else {
            let last = head.rsplit('.').next().unwrap_or(head);
            return found(AnnotatedHead::Unresolved(last));
        };
        let wrapper = fqn
            .strip_prefix("typing.")
            .or_else(|| fqn.strip_prefix("typing_extensions."));
        match (wrapper, args) {
            (Some("Optional"), Some(inner)) => t = first_arg(inner),
            (Some("Annotated"), Some(inner)) => {
                annotated = true;
                t = first_arg(inner);
            }
            (Some("Union"), Some(inner)) => {
                let mut members = split_top_level(inner, ',');
                members.retain(|p| *p != "None");
                t = single(&members)?;
            }
            (Some("ReadOnly"), _) => return found(AnnotatedHead::ReadOnly),
            _ => return found(AnnotatedHead::Resolved(fqn)),
        }
    }
}

/// What the parameter annotation `text` says to a read-only annotation of the parameter
/// ([`ParamAnnotation`]); `span` is where `text` is in the source. A literal container is a
/// resolved `list`, `dict`, or `set`, or its `typing` form, after `Optional`, `| None`, and
/// quotes: not one written through `Annotated[...]` (its metadata is there for a reader), a
/// `Mutable*` ABC (the author stated mutability), or an alias.
pub(crate) fn param_annotation(
    text: Option<&str>,
    span: Option<Span>,
    resolve: &impl Fn(&str) -> Option<String>,
) -> ParamAnnotation {
    let (Some(text), Some(span)) = (text, span) else {
        return ParamAnnotation::Any;
    };
    let Some(found) = head_of(text, resolve) else {
        return ParamAnnotation::Other;
    };
    let AnnotatedHead::Resolved(fqn) = found.named else {
        return ParamAnnotation::Other;
    };
    if found.annotated {
        return ParamAnnotation::Other;
    }
    let kind = match fqn.as_str() {
        "builtins.list" | "typing.List" => ContainerKind::List,
        "builtins.dict" | "typing.Dict" => ContainerKind::Dict,
        "builtins.set" | "typing.Set" => ContainerKind::Set,
        "typing.Any" | "typing_extensions.Any" | "builtins.object" => return ParamAnnotation::Any,
        other => return abstract_annotation(other),
    };
    let offset = |part: &str| {
        let at = part.as_ptr() as usize - text.as_ptr() as usize;
        span.start + u32::try_from(at).expect("an annotation is shorter than 4 GiB")
    };
    let start = offset(found.head);
    ParamAnnotation::Literal(Box::new(LiteralAnnotation {
        kind,
        text: text.into(),
        span,
        head: Span {
            start,
            end: start + u32::try_from(found.head.len()).expect("a short head"),
        },
        str_items: kind == ContainerKind::List && found.args.map(str::trim) == Some("str"),
    }))
}

/// What a parameter annotation resolved to `fqn`, no literal container (`typing.Set` is one) and
/// not `Any`, says to a read-only annotation ([`ParamAnnotation::Admits`] or
/// [`ParamAnnotation::Other`]).
fn abstract_annotation(fqn: &str) -> ParamAnnotation {
    let (list, dict, set) = (
        ContainerKind::List.bit(),
        ContainerKind::Dict.bit(),
        ContainerKind::Set.bit(),
    );
    let Some(name) = (fqn.strip_prefix("typing.")).or_else(|| fqn.strip_prefix("collections.abc."))
    else {
        return ParamAnnotation::Other;
    };
    match name {
        "Iterable" | "Collection" | "Container" | "Sized" => {
            ParamAnnotation::Admits(list | dict | set)
        }
        "Sequence" | "Reversible" => ParamAnnotation::Admits(list),
        "Mapping" => ParamAnnotation::Admits(dict),
        "AbstractSet" | "Set" => ParamAnnotation::Admits(set),
        _ => ParamAnnotation::Other,
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

/// The bases outside the project a `TypedDict` may list: they declare no key
/// ([`ProjectDecls::typed_dict_keys`]).
const TYPED_DICT_KEYLESS_BASES: &[&str] = &[
    "typing.TypedDict",
    "typing_extensions.TypedDict",
    "typing.Generic",
    "typing_extensions.Generic",
];

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
    /// Names looked up among the names of the project functions returning a parameter or an
    /// attribute of one (`FunctionDecl::returns_param`), or what calls return
    /// (`FunctionDecl::forwards`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub returning_names: BTreeSet<String>,
    /// Attribute names looked up among the names class bodies bind to a module-level name
    /// (`ClassDecl::named_attrs`, [`ProjectDecls::class_alias`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub alias_names: BTreeSet<String>,
    /// Classes whose project-wide derived facts were looked up: whether they are frozen (a
    /// `dataclass_transform` or a base in another module can make them so) or a `TypedDict`.
    pub class_facts: BTreeSet<String>,
    /// Protocols whose structural implementations were listed (a class in any module can start
    /// or stop satisfying one).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub protocols: BTreeSet<String>,
    /// Module globals asked whether another module may grow them (a module can start or stop
    /// growing any module's global).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub grown_globals: BTreeSet<String>,
    /// Packages asked how a module changes their `__path__`, or the path of a package under them
    /// (a module can start or stop changing any package's, and a package can appear under
    /// another): a change of one package's is a change of the package and of each package above
    /// it.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub package_paths: BTreeSet<String>,
}

impl DeclKeys {
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
            && self.subclasses.is_empty()
            && self.property_names.is_empty()
            && self.returning_names.is_empty()
            && self.alias_names.is_empty()
            && self.class_facts.is_empty()
            && self.protocols.is_empty()
            && self.grown_globals.is_empty()
            && self.package_paths.is_empty()
    }

    pub fn extend(&mut self, other: Self) {
        self.modules.extend(other.modules);
        self.subclasses.extend(other.subclasses);
        self.property_names.extend(other.property_names);
        self.returning_names.extend(other.returning_names);
        self.alias_names.extend(other.alias_names);
        self.class_facts.extend(other.class_facts);
        self.protocols.extend(other.protocols);
        self.grown_globals.extend(other.grown_globals);
        self.package_paths.extend(other.package_paths);
    }

    /// Whether the two share a key (an analysis that read `self` is affected by the change
    /// `other`).
    pub fn intersects(&self, other: &Self) -> bool {
        !self.modules.is_disjoint(&other.modules)
            || !self.subclasses.is_disjoint(&other.subclasses)
            || !self.property_names.is_disjoint(&other.property_names)
            || !self.returning_names.is_disjoint(&other.returning_names)
            || !self.alias_names.is_disjoint(&other.alias_names)
            || !self.class_facts.is_disjoint(&other.class_facts)
            || !self.protocols.is_disjoint(&other.protocols)
            || !self.grown_globals.is_disjoint(&other.grown_globals)
            || !self.package_paths.is_disjoint(&other.package_paths)
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
    keys: Mutex<Recorded>,
}

/// What a recording has read so far.
#[derive(Debug, Default)]
struct Recorded {
    keys: DeclKeys,
    /// What computing each kept answer read, once per time the answer is read again
    /// ([`ProjectDecls::class_str_constant`]): merged into `keys` once per answer when the
    /// recording ends, so reading an answer again copies no keys.
    replayed: Vec<Arc<DeclKeys>>,
}

impl Recorded {
    fn into_keys(self) -> DeclKeys {
        let mut keys = self.keys;
        let mut merged = FxHashSet::default();
        for reads in self.replayed {
            if merged.insert(Arc::as_ptr(&reads)) {
                keys.extend(DeclKeys::clone(&reads));
            }
        }
        keys
    }
}

/// What a class-level constant is read on ([`ProjectDecls::class_str_constant`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConstantRead {
    /// The class object the code names (`Model.KEY`).
    Class,
    /// A class object that may be a subclass (`cls.KEY`).
    ClassOrSubclass,
    /// An instance of the class or of a subclass (`self.KEY`).
    Instance,
}

/// What the class object of a class holds for its fields, the class-body names its annotations
/// declare as instance fields ([`ProjectDecls::field_defaults`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldDefaults {
    /// The class has no fields: its class-body annotations declare class attributes.
    NoFields,
    /// A dataclass: each field's default stays the class attribute.
    Kept,
    /// A dataclass with `slots=True`, whose slot descriptors replace the defaults, or a class
    /// a `dataclass_transform` builds, whose runtime decides.
    Replaced,
}

/// A kept answer of [`ProjectDecls`] (`ProjectDecls::kept`), with what computing it read of the
/// declarations (`None`: it was computed without recording).
#[derive(Debug, Clone)]
struct Kept<T> {
    value: T,
    reads: Option<Arc<DeclKeys>>,
}

/// The answers [`ProjectDecls`] keeps of one question, by its key. Every analysis thread asks,
/// some questions for each name it looks up ([`ProjectDecls::star_binding`]): behind one lock,
/// the threads would wait on each other, so each shard of the keys has its own.
#[derive(Debug)]
struct KeptAnswers<K, T> {
    shards: Box<[KeptShard<K, T>]>,
}

/// One shard of a [`KeptAnswers`].
type KeptShard<K, T> = RwLock<FxHashMap<K, Kept<T>>>;

/// The shards of each [`KeptAnswers`], a few per analysis thread.
const KEPT_SHARDS: usize = 64;

impl<K, T> Default for KeptAnswers<K, T> {
    fn default() -> Self {
        Self {
            shards: (0..KEPT_SHARDS).map(|_| RwLock::default()).collect(),
        }
    }
}

impl<K: Eq + std::hash::Hash, T> KeptAnswers<K, T> {
    /// The shard keeping the answer for `key`.
    fn shard(&self, key: &K) -> &KeptShard<K, T> {
        let hash = FxBuildHasher.hash_one(key);
        &self.shards[(hash >> 32) as usize % KEPT_SHARDS]
    }

    fn clear(&mut self) {
        for shard in &mut self.shards {
            shard
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        }
    }
}

/// Declarations of every project module plus derived indexes.
#[derive(Debug, Default)]
pub struct ProjectDecls {
    /// Shared with the index's file summaries and the other passes: one copy of each module's
    /// declarations.
    modules: FxHashMap<String, Arc<ModuleDecls>>,
    /// Class fqn -> direct project subclasses.
    subclasses: FxHashMap<String, Vec<String>>,
    /// Names of the property getters (`FunctionDecl::is_property`) of every class: an
    /// attribute read with another name runs no getter, without walking any class hierarchy.
    property_names: FxHashSet<String>,
    /// Names of the functions, methods, and getters returning a parameter or an attribute of
    /// one (`FunctionDecl::returns_param`), or what calls return (`FunctionDecl::forwards`): a
    /// call or a read with another name hands out no such object, without resolving it.
    returning_names: FxHashSet<String>,
    /// Names class bodies bind to a module-level name outside a field
    /// (`ClassDecl::named_attrs`): an attribute with another name is no class alias
    /// ([`Self::class_alias`]), without walking any class hierarchy.
    alias_names: FxHashSet<String>,
    /// Every project class deriving (transitively) from `TypedDict`.
    typed_dicts: FxHashSet<String>,
    /// Every frozen project class (`Self::transform_builds`).
    frozen: FxHashSet<String>,
    /// Every project class a PEP 681 `dataclass_transform` builds (`Self::transform_builds`).
    built: FxHashSet<String>,
    /// Every project protocol with the project classes implementing it structurally
    /// (`Self::protocol_implementations`), `None` past `CHA_CANDIDATE_LIMIT`.
    protocols: FxHashMap<String, Option<Vec<String>>>,
    /// Canonical fqns of module globals another project module may grow or rebinds
    /// (`ModuleDecls::grown_globals`).
    grown_elsewhere: FxHashSet<String>,
    /// Those another project module rebinds or deletes (`ModuleDecls::rebound_globals`).
    rebound_elsewhere: FxHashSet<String>,
    /// The modules whose `__getattr__` binds the global its argument names
    /// (`ModuleDecls::getattr_rebinds`) and another project module loads
    /// (`ModuleDecls::getattr_loads`).
    getattr_called: FxHashSet<String>,
    /// The modules whose `__getattr__` another project module may make serve other objects
    /// than [`ModuleDecls::getattr_exports`] says: it stores into the module under a name heki
    /// cannot read, or rebinds or stores into a global the hook reads
    /// ([`GetattrExports::globals`]).
    getattr_open: FxHashSet<String>,
    /// The dotted prefixes of the names in `rebound_elsewhere` (`pkg.mod.C` of
    /// `pkg.mod.C.run.__code__`): another module rebinds no attribute of a class not among them.
    rebound_owners: FxHashSet<String>,
    /// What project modules store into the globals of other modules and into the instances of
    /// their classes, joined (`ModuleDecls::stores_elsewhere`), and the modules whose own types
    /// then hold no longer.
    elsewhere: stores::Elsewhere,
    /// What every project module binds to each read-only container field of a class, by class
    /// and field (`ModuleDecls::field_values`, [`Self::field_rejects_changes`]).
    field_values: FxHashMap<(String, String), Changeable>,
    /// Some project module calls `__setstate__` itself (`ModuleDecls::calls_setstate`).
    setstate_called: bool,
    /// The packages whose `__path__` a project module changes (`ModuleDecls::path_changes`, the
    /// changes joined), and the packages heki could not read, as `PathChange::Replaced`; sorted,
    /// so the packages under a name are one range.
    changed_paths: BTreeMap<String, PathChange>,
    /// The canonical fqns of the classes a project module registers with `copyreg`
    /// (`ModuleDecls::copy_registrations`), with `*` when one names a class heki cannot.
    copy_registered: FxHashSet<String>,
    /// For each family and level of the container families, the project classes a value may be
    /// (`Self::family_members`).
    families: FxHashMap<(ContainerKind, FamilyLevel), FamilyMembers>,
    recorder: Recorder,
    /// The C3 linearizations computed so far of classes with multiple inheritance along their
    /// bases, by how the bases are named (`Self::mro`, `Self::declared_mro`); cleared with the
    /// indexes.
    linearized: RwLock<FxHashMap<(Chase, String), Vec<String>>>,
    /// One shared [`crate::ClassLine`] per class (`Self::lineage`); cleared with the indexes.
    lines: RwLock<FxHashMap<String, crate::SharedLine>>,
    /// The answers of [`Self::class_str_constant`] so far, by class, name, and whether the
    /// object may be of a subclass; cleared with the indexes.
    class_constants: KeptAnswers<(String, String, ConstantRead), Option<String>>,
    /// The answers of [`Self::stored_attr_type`] so far, by class and attribute; cleared with
    /// the indexes.
    attr_joins: KeptAnswers<(String, String), Option<Ty>>,
    /// The answers of [`Self::held_attr`] so far, by class and attribute; cleared with the
    /// indexes.
    held_attrs: KeptAnswers<(String, String), Option<Ty>>,
    /// The answers of [`Self::capacity`] so far; cleared with the indexes.
    capacities: KeptAnswers<CapacityOf, CapacityOf>,
    /// The answers of [`Self::star_binding`] so far, by module and name; cleared with the
    /// indexes.
    star_bindings: KeptAnswers<(String, String), StarBinding>,
    /// The answers of [`Self::late_star_binding`] so far, by module and name; cleared with the
    /// indexes.
    late_star_bindings: KeptAnswers<(String, String), StarBinding>,
    /// The answers of [`Self::catalog_attribute`] so far, by class and name; cleared with the
    /// indexes.
    catalog_attributes: SharedMemo<(String, String), Option<String>, DeclKeys>,
    /// The answers of [`Self::class_object_attribute`] so far, by class, name, and whether a
    /// subclass may be read; cleared with the indexes.
    class_object_attributes: SharedMemo<(String, String, bool), Option<String>, DeclKeys>,
    /// The answers of [`Self::effect_methods_replaced`] so far, by library base; cleared with
    /// the indexes.
    effect_methods_replaced: SharedMemo<String, bool, DeclKeys>,
    /// The answers of [`Self::object_attr_read_runs_no_code`] so far, by class and name;
    /// cleared with the indexes.
    object_attr_reads: SharedMemo<(String, String), bool, DeclKeys>,
}

/// Why an attribute (or `TypedDict` key) of a project class is read-only (`MUT108`, `MUT120`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyKind {
    /// A field of a frozen class: a frozen dataclass, a `NamedTuple`, or a class that a
    /// `dataclass_transform(frozen_default=True)` builds.
    Frozen,
    /// Declared `Final[...]` or `ReadOnly[...]` (`ClassDecl::readonly_attrs`).
    Declared,
    /// A property of a read-only view ([`ProjectDecls::is_readonly_view`]): the class declares
    /// the whole value read-only, not one field (`MUT120`).
    View,
}

/// The declaration that makes an attribute read-only: the first class along the MRO that
/// declares it, by fqn, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyAttr {
    pub class: String,
    pub kind: ReadOnlyKind,
    /// The attribute holds a container. Fields and declarations are read-only only when they
    /// do (`ClassDecl::container_fields`, `readonly_attrs`); a view's property holds one when
    /// its getter's return annotation is a container type (`ClassDecl::container_properties`).
    pub holds_container: bool,
    /// A view's property annotated with a read-only container type (`Sequence`, `Mapping`,
    /// `AbstractSet`): a type checker stops changing in place what it returns, through the view.
    pub typed_read_only: bool,
}

/// The bases outside the project a read-only view may have: they define no attribute of its
/// instances ([`ProjectDecls::is_readonly_view`]).
const VIEW_BASES: &[&str] = &[
    "object",
    "builtins.object",
    "abc.ABC",
    "typing.Generic",
    "typing_extensions.Generic",
    "typing.Protocol",
    "typing_extensions.Protocol",
];

/// Where an import looks for the modules under a package ([`ProjectDecls::search_under`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Search {
    /// Only in directories of the project: a name under the package is a module of the project,
    /// or no module.
    Project,
    /// In directories of the project first, then in others: a module the project has is found
    /// in the project, any other name may be found outside it.
    ProjectFirst,
    /// In directories that may be outside the project, in any order.
    Anywhere,
}

/// What the name `n` of a project package `p` holds once `p/__init__.py` has run, when the
/// project also has the module `p.n` ([`ProjectDecls::from_import`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FromImport {
    /// The submodule `p.n`.
    Submodule,
    /// What the body of `p` binds `n` to after loading `p.n` (`from .n import n`).
    Attribute,
    /// What the body of `p` binds `n` to (`def n`, `from .other import n`) when no statement
    /// binding it loads `p.n` first: the first import of `p.n` anywhere replaces it, so it
    /// depends on the order modules are imported in. Also any name of a package heki could not
    /// read.
    Open,
}

/// What the star imports of a module bind a name to that the module binds no other way
/// ([`ProjectDecls::star_binding`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StarBinding {
    /// No star import binds it, as far as heki reads the modules they name: a module outside
    /// the project, which only the catalog describes, is not followed.
    Unbound,
    /// A star import binds it to `target`, the name read on the module it imports
    /// (`pkg.utils._utils.fetch`), and every other star import binding it binds the same
    /// object: the name read on `origin`, the module whose own statement binds it.
    Bound { target: String, origin: String },
    /// It may hold an object heki cannot name: a star import binds it through a module heki
    /// could not read or an `__all__` heki cannot read; or star imports bind it to different
    /// objects (`try: from ._speedups import *` with a fallback), which one of a module outside
    /// the project may do for any name, and which of them ran last is not known.
    Open,
}

/// What a project name of a definition holds where code reads it ([`ProjectDecls::name_holds`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NameHolds {
    /// The object its statement made, and no other.
    Definition,
    /// The object its statement made, or the named object `0` (by fqn) a star import after the
    /// statement binds it to.
    Also(String),
    /// Possibly an object heki cannot name.
    Unknown,
}

/// How a call reaches the object it calls ([`TypeCtx::call_result`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CalledThrough {
    /// A read of the object's name (`Base()`, `make = lib.build` then `make()`), which may hold
    /// another object than the definition ([`ProjectDecls::name_holds`]).
    Name,
    /// A class object a value holds by its type (`cls` in a class method, a parameter of
    /// `type[Base]`): the class or a subclass, whatever the class's name holds.
    ClassObject,
}

/// What binds a project name again past its definition ([`ProjectDecls::rebinding`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rebinding {
    None,
    /// Another named object, by fqn.
    To(String),
    /// An object heki cannot name.
    Unknown,
}

/// What one star import binds a name to ([`ProjectDecls::star_import_binds`]).
enum StarSource {
    /// It binds no such name.
    Nothing,
    /// It imports a module outside the project, which may bind any name.
    Anything,
    /// It binds the name ([`StarBinding::Bound`]), or binds it to an object heki cannot name
    /// (`None`).
    Binds(Option<(String, String)>),
}

/// Whether a module's namespace holds a name once its body has run, as its own statements and
/// its star imports bind it ([`ProjectDecls::module_holds`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Holds {
    No,
    /// The name read on the module whose own statement binds it.
    Yes(String),
    /// Through a star import heki cannot follow ([`StarBinding::Open`]).
    Maybe,
}

/// How far [`ProjectDecls::follow`] chases a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Chase {
    /// Through re-exports ([`ProjectDecls::canonical`]).
    Reexports,
    /// Through re-exports, `None` past one its module binds again
    /// ([`ProjectDecls::canonical_value`]).
    Values,
    /// Through re-exports and the module aliases they name ([`ProjectDecls::object_named`]).
    Objects,
    /// [`Self::Objects`] up to a name that names a type ([`ModuleDecls::type_names`]), which a
    /// type expression reads as that type, whatever object its module also binds to it
    /// ([`ProjectDecls::type_name`]).
    Types,
    /// [`Self::Reexports`] for the class a class statement names as a base, read as a type
    /// checker reads it ([`ProjectDecls::declared_mro`]).
    Bases,
    /// [`Self::Objects`] for the class an annotation names ([`ProjectDecls::declared_ty`]).
    Declared,
}

impl Chase {
    /// Whether the chase reads a name as a type checker does: past a module's `if
    /// TYPE_CHECKING:` import of a name it binds no other way
    /// ([`ModuleDecls::type_checking_binding`]), and past the class object of a class or a
    /// subclass its `__getattr__` serves under the name (`-> type[C]`). At run time the hook
    /// may serve another object under it, or none, so no chase deciding what a name holds or
    /// runs does.
    fn reads_type_checking(self) -> bool {
        matches!(self, Chase::Types | Chase::Bases | Chase::Declared)
    }

    /// Whether the chase reads an annotation, which names what a type checker reads for a name
    /// first: its `if TYPE_CHECKING:` import before what the module's `__getattr__` serves at
    /// run time ([`ProjectDecls::unbound_name`]). A class base is the class the hook serves.
    fn reads_annotations(self) -> bool {
        matches!(self, Chase::Types | Chase::Declared)
    }
}

/// What [`ProjectDecls::follow`] finds.
struct Followed {
    name: String,
    /// The name is the object a class or function statement made rather than what it holds
    /// once its module has run.
    definition: bool,
    /// The chase went past a binding that names only the class a type checker reads
    /// ([`Chase::reads_type_checking`]): at run time the name may hold another class.
    declared_only: bool,
}

/// `a`, `a.b`, and `a.b.c` of `a.b.c`.
fn dotted_prefixes(name: &str) -> impl Iterator<Item = &str> {
    name.match_indices('.')
        .map(|(dot, _)| &name[..dot])
        .chain(std::iter::once(name))
}

/// Where a special method of an instance comes from ([`ProjectDecls::special_method_owner`]),
/// from the least to the most code it may run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MethodOwner {
    /// `object`'s, which runs nothing else.
    Object,
    /// The interpreter's own of a builtin type, which may run the same of the items it holds
    /// (a container's comparison, hash, or `repr()`, an exception's string of its arguments).
    Builtin,
    /// May be the one this class outside the project defines.
    Library(String),
    /// May be code of the project.
    Project,
}

/// What a dotted fqn names inside the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Symbol<'a> {
    Module,
    Class(&'a ClassDecl),
    Function(FuncKey, &'a FunctionDecl),
    Global(Ty),
    /// A name a project module binds itself that heki has no declaration for, or a name under
    /// one: a module global it holds no type of ([`ModuleDecls::unknown_globals`]). Calling it
    /// is unresolved.
    Unknown,
    /// A name under a project module heki could not read.
    Unread,
    External,
}

/// The answers kept in a [`SharedMemo`] over the declarations record what deciding them read.
impl ReadLog for ProjectDecls {
    type Reads = DeclKeys;

    fn recording(&self) -> bool {
        self.recorder.on.load(Ordering::Relaxed)
    }

    fn isolated<A>(&self, decide: impl FnOnce() -> A) -> (A, Arc<DeclKeys>) {
        let (answer, reads) = self.reads_of(decide);
        (answer, Arc::new(reads.unwrap_or_default()))
    }

    fn replay(&self, reads: Arc<DeclKeys>) {
        ProjectDecls::replay(self, reads);
    }
}

impl ProjectDecls {
    pub fn new(modules: impl IntoIterator<Item = impl Into<Arc<ModuleDecls>>>) -> Self {
        let mut out = Self::default();
        for m in modules {
            let m = m.into();
            out.modules.insert(m.module_name.clone(), m);
        }
        out.rebuild_indexes();
        out
    }

    /// Replace (or add) one module's declarations; the derived indexes are rebuilt because a
    /// base may be canonicalized through any module.
    pub fn replace(&mut self, decls: impl Into<Arc<ModuleDecls>>) {
        let decls = decls.into();
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
        let keys = std::mem::take(&mut *self.recorded()).into_keys();
        (out, keys)
    }

    fn recorded(&self) -> MutexGuard<'_, Recorded> {
        self.recorder
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Note a key read during [`Self::record`].
    fn read(&self, note: impl FnOnce(&mut DeclKeys)) {
        if self.recorder.on.load(Ordering::Relaxed) {
            note(&mut self.recorded().keys);
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
        out.returning_names = differ(&before.returning_names, &self.returning_names);
        out.alias_names = differ(&before.alias_names, &self.alias_names);
        out.class_facts = differ(&before.frozen, &self.frozen);
        out.class_facts.extend(differ(&before.built, &self.built));
        out.class_facts
            .extend(differ(&before.typed_dicts, &self.typed_dicts));
        for p in before.protocols.keys().chain(self.protocols.keys()) {
            if before.protocols.get(p) != self.protocols.get(p) {
                out.protocols.insert(p.clone());
            }
        }
        out.grown_globals = differ(&before.grown_elsewhere, &self.grown_elsewhere);
        out.grown_globals
            .extend(differ(&before.rebound_elsewhere, &self.rebound_elsewhere));
        out.grown_globals
            .extend(differ(&before.getattr_called, &self.getattr_called));
        out.grown_globals.extend(
            differ(&before.getattr_open, &self.getattr_open)
                .into_iter()
                .map(|module| format!("{module}.__getattr__")),
        );
        out.class_facts
            .extend(differ(&before.copy_registered, &self.copy_registered));
        for &(kind, level) in before.families.keys().chain(self.families.keys()) {
            if before.families.get(&(kind, level)) != self.families.get(&(kind, level)) {
                out.class_facts.insert(family_key(kind, level).to_owned());
            }
        }
        if before.setstate_called != self.setstate_called {
            out.class_facts.insert(stores::SETSTATE_KEY.to_owned());
        }
        for package in before.changed_paths.keys().chain(self.changed_paths.keys()) {
            if before.changed_paths.get(package) != self.changed_paths.get(package) {
                out.package_paths
                    .extend(dotted_prefixes(package).map(str::to_owned));
            }
        }
        out
    }

    /// The joins only the completed declarations decide, which [`DeclPasses`] compares across
    /// an update.
    fn completed_joins(&self) -> CompletedJoins {
        (self.elsewhere.clone(), self.field_values.clone())
    }

    fn indexes(&self) -> Indexes {
        Indexes {
            subclasses: self.subclasses.clone(),
            property_names: self.property_names.clone(),
            returning_names: self.returning_names.clone(),
            alias_names: self.alias_names.clone(),
            frozen: self.frozen.clone(),
            built: self.built.clone(),
            typed_dicts: self.typed_dicts.clone(),
            protocols: self.protocols.clone(),
            grown_elsewhere: self.grown_elsewhere.clone(),
            rebound_elsewhere: self.rebound_elsewhere.clone(),
            getattr_called: self.getattr_called.clone(),
            getattr_open: self.getattr_open.clone(),
            changed_paths: self.changed_paths.clone(),
            copy_registered: self.copy_registered.clone(),
            families: self.families.clone(),
            setstate_called: self.setstate_called,
        }
    }

    fn rebuild_indexes(&mut self) {
        self.linearized
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.lines
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.class_constants.clear();
        self.attr_joins.clear();
        self.held_attrs.clear();
        self.capacities.clear();
        self.star_bindings.clear();
        self.late_star_bindings.clear();
        self.catalog_attributes.clear();
        self.class_object_attributes.clear();
        self.effect_methods_replaced.clear();
        self.object_attr_reads.clear();
        // What other modules change, which deciding what a module's `__getattr__` serves reads
        // (`Self::getattr_open`) while the indexes are built: as for new declarations, nothing
        // until each is built again.
        self.rebound_elsewhere.clear();
        self.getattr_called.clear();
        self.elsewhere = stores::Elsewhere::default();
        self.getattr_open.clear();
        // Maintenance, not a lookup of an analysis.
        let recording = std::mem::replace(self.recorder.on.get_mut(), false);
        self.build_indexes();
        *self.recorder.on.get_mut() = recording;
    }

    fn build_indexes(&mut self) {
        let out = &*self;
        let mut subclasses: FxHashMap<String, Vec<String>> = FxHashMap::default();
        let mut property_names = FxHashSet::default();
        let mut returning_names = FxHashSet::default();
        let mut alias_names = FxHashSet::default();
        for m in out.modules.values() {
            for (qualname, class) in &m.classes {
                let fqn = format!("{}.{qualname}", m.module_name);
                for (attr, named) in &class.named_attrs {
                    let binds = named.body.is_some()
                        || !named.body_paths.is_empty()
                        || named.body_partial.is_some();
                    if binds && !named.annotated {
                        alias_names.insert(attr.clone());
                    }
                }
                // A class a type checker reads as a subclass may be one at run time: an
                // instance of the base may be one of it.
                for base in class.bases.iter().filter(|b| !is_unknown_base(b)) {
                    subclasses
                        .entry(out.chase(base, Chase::Bases))
                        .or_default()
                        .push(fqn.clone());
                }
            }
            for (qualname, f) in &m.functions {
                // A method is read by the name its class holds it under (`_Parser__run` of
                // `def __run`).
                let name = mangling::member_name(qualname);
                if f.reads_as_property() {
                    property_names.insert(name.clone().into_owned());
                }
                // A function returning what calls return is listed in pass 1 too, before its
                // completion decides what it returns: a name the completion adds to the index
                // would change no key pass 1's changes name.
                if f.returns_param.is_some() || f.forwards.is_some() {
                    returning_names.insert(name.into_owned());
                }
            }
        }
        for subs in subclasses.values_mut() {
            subs.sort();
            subs.dedup();
        }
        self.subclasses = subclasses;
        self.property_names = property_names;
        self.returning_names = returning_names;
        self.alias_names = alias_names;
        // Before the indexes below: `Self::value_classes` of a family's class reads it.
        self.families = self.family_members();
        self.typed_dicts = TYPED_DICT_BASES
            .iter()
            .flat_map(|b| self.subclasses_of(b))
            .collect();
        (self.frozen, self.built) = self.transform_builds();
        self.protocols = self.protocol_implementations();
        self.grown_elsewhere = self.globals_changed_elsewhere(|m| &m.grown_globals);
        self.rebound_elsewhere = self.globals_changed_elsewhere(|m| &m.rebound_globals);
        self.getattr_called = (self
            .globals_changed_elsewhere(|m| &m.getattr_loads)
            .into_iter())
        .filter_map(|hook| {
            let module = hook.strip_suffix(".__getattr__")?;
            let rebinds = self.module(module).is_some_and(|m| m.getattr_rebinds);
            rebinds.then(|| module.to_owned())
        })
        .collect();
        self.rebound_owners = (self.rebound_elsewhere.iter())
            .flat_map(|fqn| fqn.match_indices('.').map(|(dot, _)| fqn[..dot].to_owned()))
            .collect();
        self.setstate_called = self.modules.values().any(|m| m.calls_setstate);
        self.elsewhere = self.stores_elsewhere();
        self.field_values = self.joined_field_values();
        self.getattr_open = self.getattr_hooks_changed();
        self.changed_paths = self.package_path_changes();
        self.copy_registered = self
            .modules
            .values()
            .flat_map(|m| &m.copy_registrations)
            .map(|c| self.canonical(c))
            .collect();
    }

    /// [`Self::changed_paths`].
    fn package_path_changes(&self) -> BTreeMap<String, PathChange> {
        let mut out: BTreeMap<String, PathChange> = BTreeMap::new();
        let mut join = |package: String, change: PathChange| {
            let joined = out.entry(package).or_insert(change);
            *joined = (*joined).max(change);
        };
        for m in self.modules.values() {
            if m.unread && m.is_package {
                join(m.module_name.clone(), PathChange::Replaced);
            }
            for (package, change) in m.path_changes.iter().flat_map(|changes| changes.iter()) {
                join(self.canonical(package), *change);
            }
        }
        out
    }

    /// The canonical fqns of the globals that another module than their owner lists in
    /// `changed` (`ModuleDecls::grown_globals`, `ModuleDecls::rebound_globals`).
    fn globals_changed_elsewhere(
        &self,
        changed: impl Fn(&ModuleDecls) -> &BTreeSet<String>,
    ) -> FxHashSet<String> {
        let mut out = FxHashSet::default();
        for m in self.modules.values() {
            for fqn in changed(m) {
                let fqn = self.canonical(fqn);
                if self.split_module(&fqn).is_some_and(|(owner, rest)| {
                    owner.module_name != m.module_name && !rest.is_empty()
                }) {
                    out.insert(fqn);
                }
            }
        }
        out
    }

    /// Whether another project module may grow or rebinds the module global `fqn` (a display
    /// its own module fixes is then fixed no longer).
    pub fn grown_elsewhere(&self, fqn: &str) -> bool {
        let fqn = self.canonical(fqn);
        self.read(|k| note(&mut k.grown_globals, &fqn));
        self.grown_elsewhere.contains(&fqn) || self.rebound_by_getattr(&fqn)
    }

    /// Whether another project module rebinds or deletes the module global `fqn` (a string
    /// its own module binds once is then no constant).
    pub fn rebound_elsewhere(&self, fqn: &str) -> bool {
        let fqn = self.canonical(fqn);
        self.read(|k| note(&mut k.grown_globals, &fqn));
        self.rebound_elsewhere.contains(&fqn) || self.rebound_by_getattr(&fqn)
    }

    /// Whether another project module may rebind the constant `fqn` of a module or a class:
    /// it rebinds the name, which type checkers hold a constant declared `Final` to
    /// (`rebindable`: not declared so), or calls the `__getattr__` of the constant's module,
    /// which binds any global it is given ([`Self::rebound_by_getattr`]).
    pub fn constant_rebound(&self, fqn: &str, rebindable: bool) -> bool {
        if rebindable {
            return self.rebound_elsewhere(fqn);
        }
        self.rebound_by_getattr(&self.canonical(fqn))
    }

    /// Whether the global `fqn` (canonical) of a module may be bound by the module's
    /// `__getattr__`, which another project module loads and may call with the name, `Final`
    /// or not ([`Self::getattr_called`]).
    fn rebound_by_getattr(&self, fqn: &str) -> bool {
        fqn.rsplit_once('.').is_some_and(|(module, _)| {
            self.read(|k| note(&mut k.grown_globals, module));
            self.getattr_called.contains(module)
        })
    }

    /// Whether another project module may bind the global `name` of the project module
    /// `module`: it stores into it or deletes it, by name (`cfg.name = v`, `setattr(cfg,
    /// "name", v)`) or under a name heki cannot read (`setattr(cfg, key, v)`).
    pub fn global_bound_elsewhere(&self, module: &str, name: &str) -> bool {
        let fqn = format!("{module}.{name}");
        // The stores are kept by the object a name gives (`ProjectDecls::canonical`); a name
        // the module binds to another module's object is looked up both ways.
        let stored = |fqn: &str| {
            (self.split_module(fqn)).is_some_and(|(m, _)| self.global_rebound_by_store(m, fqn))
        };
        let canonical = self.canonical(&fqn);
        self.rebound_elsewhere(&fqn) || stored(&fqn) || (canonical != fqn && stored(&canonical))
    }

    /// [`Self::rebound_elsewhere`] of `fqn`, canonical as it is (an attribute of a class along
    /// an MRO, of a module's function).
    fn rebound_elsewhere_canonical(&self, fqn: &str) -> bool {
        self.read(|k| note(&mut k.grown_globals, fqn));
        self.rebound_elsewhere.contains(fqn) || self.rebound_by_getattr(fqn)
    }

    /// Whether calling the object the project name `fqn` names (a module's function or class,
    /// a class's method read on the class) may run another object than its definition (R1):
    /// the name may hold another object ([`Self::name_holds`]), or the `__init__` constructing
    /// a class runs may be another than its MRO finds ([`Self::lookup_rebound`]).
    pub fn call_rebound(&self, fqn: &str) -> bool {
        let fqn = self.canonical(fqn);
        if self.rebinding(&fqn) != Rebinding::None {
            return true;
        }
        self.split_module(&fqn).is_some_and(|(m, rest)| {
            m.classes.contains_key(rest) && self.lookup_rebound(&self.mro(&fqn), "__init__")
        })
    }

    /// What the project name `fqn` (a module's function or class, a class's member read on the
    /// class) holds where code reads it (R1): the object its statement made, unless
    /// [`Self::rebinding`] binds it again; a star import after the definition binds it to the
    /// object another module's name holds, which is that module's definition while its own
    /// name holds nothing else.
    pub(crate) fn name_holds(&self, fqn: &str) -> NameHolds {
        let fqn = self.canonical(fqn);
        match self.rebinding(&fqn) {
            Rebinding::None => NameHolds::Definition,
            Rebinding::To(other) if self.rebinding(&other) == Rebinding::None => {
                NameHolds::Also(other)
            }
            Rebinding::To(_) | Rebinding::Unknown => NameHolds::Unknown,
        }
    }

    /// Whether the project name `fqn` holds the object its statement made, and no other
    /// ([`Self::name_holds`]).
    pub(crate) fn holds_definition(&self, fqn: &str) -> bool {
        self.rebinding(&self.canonical(fqn)) == Rebinding::None
    }

    /// What binds the project name `fqn` (canonical) again, past its definition
    /// ([`Self::global_rebinding`]). A member read on a class through its name (`C.m`,
    /// `Outer.Inner.m`) is another object when the module global its qualname starts with is,
    /// or when the lookup of a name along the MRO of a class on the way may find another
    /// object than the class bodies bind ([`Self::lookup_rebound`]).
    fn rebinding(&self, fqn: &str) -> Rebinding {
        let Some((m, rest)) = self.split_module(fqn).filter(|(_, rest)| !rest.is_empty()) else {
            return Rebinding::None;
        };
        let Some((owner, _)) = rest.rsplit_once('.') else {
            return self.global_rebinding(m, fqn, rest);
        };
        let class_of = |qualname: &str| format!("{}.{qualname}", m.module_name);
        let member_rebound = || {
            rest.match_indices('.').any(|(at, _)| {
                let class = class_of(&rest[..at]);
                let name = rest[at + 1..].split('.').next().unwrap_or_default();
                self.is_project_class(&class) && self.lookup_rebound(&self.mro(&class), name)
            })
        };
        let top = rest.split('.').next().unwrap_or(rest);
        let rebound = self.is_project_class(&class_of(owner))
            && (member_rebound()
                || self.global_rebinding(m, &class_of(top), top) != Rebinding::None);
        if rebound {
            Rebinding::Unknown
        } else {
            Rebinding::None
        }
    }

    /// What binds the module global `name` of `m` (`fqn`, canonical), a definition of the
    /// module, again past its statement: the module binds it to another object
    /// ([`ModuleDecls::rebound_definitions`]: a store, a deletion, `globals()` or `exec`, a
    /// store of its `__code__`), another module stores into it or deletes it, by name (`lib.f =
    /// g`, `setattr(lib, "f", g)`) or under a name heki cannot read (`setattr(lib, name, g)`),
    /// or replaces its code (`lib.f.__code__ = ...`), or a star import after its statement
    /// binds it ([`Self::late_star_binding`]: to the object it names, or to one heki cannot
    /// name).
    fn global_rebinding(&self, m: &ModuleDecls, fqn: &str, name: &str) -> Rebinding {
        if m.rebound_definitions.contains(name)
            || self.rebound_elsewhere_canonical(fqn)
            || self.rebound_elsewhere_canonical(&format!("{fqn}.__code__"))
            || self.global_rebound_by_store(m, fqn)
        {
            return Rebinding::Unknown;
        }
        match self.late_star_binding(m, name) {
            StarBinding::Unbound => Rebinding::None,
            StarBinding::Open => Rebinding::Unknown,
            // A star import binding the name to the definition itself (a module importing it
            // from here) leaves it.
            StarBinding::Bound { target, .. } => match self.object_named(&target) {
                object if object == fqn => Rebinding::None,
                object => Rebinding::To(object),
            },
        }
    }

    /// Whether what looking `name` up along `mro` (canonical classes) finds may be another
    /// object than the class bodies bind (R1): a project class before the first whose body
    /// binds the name, or that one, binds it again: its module stores it on the class object or
    /// on `self` outside the body (`ClassDecl::rebound_attrs`), its class body binds it again
    /// or replaces the method's code ([`ModuleDecls::rebound_definitions`]), or another module
    /// stores into it, deletes it, or replaces its code ([`Self::rebound_elsewhere`]); or the
    /// project gives a project class of `mro` other bases (`__bases__`), or its instances
    /// another class (`__class__`); or the lookup reaches a base that names only the class a
    /// type checker reads ([`Self::declared_only_base`]) before a class binding the name.
    pub(crate) fn lookup_rebound(&self, mro: &[String], name: &str) -> bool {
        // Only an analysis whose reads are recorded asks for keys no module stores yet. Every
        // call whose targets are checked asks for each class it may run on, along its MRO.
        let recording = self.recording();
        let mut found = false;
        for c in mro {
            let Some((m, qualname, decl)) = self
                .split_module(c)
                .and_then(|(m, q)| Some((m, q, m.classes.get(q)?)))
            else {
                if !found && self.declared_only_base(c) {
                    return true;
                }
                continue;
            };
            let elsewhere = recording || self.rebound_owners.contains(c);
            let stored = |attr: &str| {
                decl.rebound_attrs.contains(attr)
                    || (elsewhere && self.rebound_elsewhere_canonical(&format!("{c}.{attr}")))
            };
            let rebinds_name = || {
                stored(name)
                    || (!m.rebound_definitions.is_empty()
                        && m.rebound_definitions
                            .contains(format!("{qualname}.{name}").as_str()))
                    || (elsewhere
                        && self.rebound_elsewhere_canonical(&format!("{c}.{name}.__code__")))
            };
            if (!found && rebinds_name()) || stored("__bases__") || stored("__class__") {
                return true;
            }
            found |= decl.members.contains(name);
        }
        false
    }

    /// Whether the base `base` of an MRO ([`Self::mro`]) names only the class a type checker
    /// reads ([`Self::declared_mro`]): a name its module binds only under `if TYPE_CHECKING:`,
    /// or the class object of a class or a subclass its module's `__getattr__` serves. At run
    /// time the module may serve another class under it.
    fn declared_only_base(&self, base: &str) -> bool {
        self.follow(base, Chase::Bases)
            .is_some_and(|found| found.declared_only)
    }

    /// Whether another project module may change what the module global `fqn` holds: grow it
    /// or rebind it, or only rebind it when it holds `display` and that is a tuple or a
    /// string.
    pub fn changed_elsewhere(&self, fqn: &str, display: &SmallDisplay) -> bool {
        if display.is_immutable() {
            self.rebound_elsewhere(fqn)
        } else {
            self.grown_elsewhere(fqn)
        }
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
    pub fn protocol_implementers(&self, class: &str) -> Option<&[String]> {
        self.protocol(class).flatten()
    }

    /// Whether `class` is a project protocol that declares the member `name` itself or through
    /// its protocol bases: an access through a value typed with it is an access of the
    /// protocol's member, whatever class implements it.
    pub fn protocol_declares(&self, class: &str, name: &str) -> bool {
        // The class's own declaration tells whether it is a protocol: its implementations,
        // which any module can change, do not matter here.
        self.class(class).is_some_and(|d| d.is_protocol)
            && self.mro(class).iter().any(|c| {
                self.class(c)
                    .is_some_and(|d| d.is_protocol && d.members.contains(name))
            })
    }

    /// `Some` when the project class `class` is a protocol: with the project classes
    /// implementing it structurally, or `None` inside when heki cannot list them (too many, a
    /// protocol without members).
    pub(crate) fn protocol(&self, class: &str) -> Option<Option<&[String]>> {
        self.read(|k| note(&mut k.protocols, class));
        self.protocols.get(class).map(Option::as_deref)
    }

    /// The frozen project classes, and the project classes a PEP 681 `dataclass_transform`
    /// builds: through a decorator function carrying it, or through a base class or metaclass
    /// carrying it (the classes deriving from it, or using it or a subclass of it as their
    /// metaclass, and their subclasses). Frozen are `ClassDecl::is_frozen`
    /// (`@dataclass(frozen=True)`, `NamedTuple`), and the classes a
    /// `dataclass_transform(frozen_default=True)` builds unless they pass `frozen=False`.
    fn transform_builds(&self) -> (FxHashSet<String>, FxHashSet<String>) {
        let mut frozen = FxHashSet::default();
        let mut built = FxHashSet::default();
        let mut transforms = Vec::new();
        let mut metaclass_users = Vec::new();
        for m in self.modules.values() {
            for (q, c) in &m.classes {
                // Whether a decorator function carrying a transform builds the class frozen.
                let decorated = c
                    .decorators
                    .iter()
                    .filter_map(
                        |(dec, frozen_arg)| match self.symbol(&self.canonical(dec)) {
                            Symbol::Function(_, d) => d.transform.map(|t| frozen_arg.unwrap_or(t)),
                            _ => None,
                        },
                    )
                    .reduce(|a, b| a || b);
                let fqn = || format!("{}.{q}", m.module_name);
                if decorated.is_some() {
                    built.insert(fqn());
                }
                if c.is_frozen || decorated == Some(true) {
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
            let mut built_by = self.subclasses_of(&t);
            let users: Vec<String> = metaclass_users
                .iter()
                .filter(|(_, meta)| *meta == t || built_by.binary_search(meta).is_ok())
                .map(|(fqn, _)| fqn.clone())
                .collect();
            for u in users {
                built_by.extend(self.subclasses_of(&u));
                built_by.push(u);
            }
            for b in built_by {
                // The first `frozen=` class keyword along the MRO decides.
                let keyword = self
                    .mro(&b)
                    .iter()
                    .find_map(|c| self.class(c)?.frozen_keyword);
                if keyword.unwrap_or(default) {
                    frozen.insert(b.clone());
                }
                built.insert(b);
            }
        }
        (frozen, built)
    }

    /// What the class object of the project class `class` holds for the class-body names its
    /// annotations declare as instance fields.
    fn field_defaults(&self, class: &str) -> FieldDefaults {
        self.read(|k| note(&mut k.class_facts, class));
        if self.built.contains(class) {
            return FieldDefaults::Replaced;
        }
        match self.class(class).and_then(|d| d.dataclass) {
            Some(Dataclass::Defaults) => FieldDefaults::Kept,
            Some(Dataclass::Slots) => FieldDefaults::Replaced,
            None => FieldDefaults::NoFields,
        }
    }

    pub fn module(&self, name: &str) -> Option<&ModuleDecls> {
        self.read(|k| note(&mut k.modules, name));
        self.modules.get(name).map(Arc::as_ref)
    }

    /// Whether importing `name` (`pkg.config`) loads the project's module of that name: the
    /// project has it, and the packages along the name lead the import to it before any
    /// directory outside the project ([`Self::search_under`] its package).
    pub fn holds_module(&self, name: &str) -> bool {
        self.module(name).is_some()
            && name
                .rsplit_once('.')
                .is_none_or(|(package, _)| self.search_under(package) != Search::Anywhere)
    }

    /// Whether every module whose name starts with `head` (`pkg.parser.`) is one of the
    /// project's: the import looks for the rest of the name under the package `head` names
    /// before its last dot only in directories of the project ([`Self::search_under`]), and no
    /// project module changes the `__path__` of a package the rest of the name may go through.
    pub fn holds_modules_starting_with(&self, head: &str) -> bool {
        // The key of `package` that `search_under` notes changes with the path of any package
        // under it (`DeclKeys::package_paths`).
        head.rsplit_once('.').is_some_and(|(package, _)| {
            self.search_under(package) == Search::Project
                && self
                    .changed_paths
                    .range::<str, _>((Bound::Included(head), Bound::Unbounded))
                    .next()
                    .is_none_or(|(below, _)| !below.starts_with(head))
        })
    }

    /// Where an import looks for the modules directly under `package`, from the packages along
    /// its name: Python finds `a.b.c` on the `__path__` of `a.b`, which it found on the path of
    /// `a`, and `a` on `sys.path`, where heki takes the project's own top-level names to be found
    /// in the project. A regular package of the project is found before anything after the
    /// project's directories; a directory without `__init__.py` is a namespace package, with a
    /// portion in each directory of its parent's path, in that order. A module that is not a
    /// package is not followed (binding `__path__` makes it one).
    fn search_under(&self, package: &str) -> Search {
        let mut search = Search::ProjectFirst;
        for name in dotted_prefixes(package) {
            match self.module(name) {
                Some(m) if !m.is_package => return Search::Anywhere,
                Some(_) => search = Search::Project,
                None => {}
            }
            self.read(|k| note(&mut k.package_paths, name));
            search = match self.changed_paths.get(name) {
                None => search,
                Some(PathChange::Appended) => search.max(Search::ProjectFirst),
                Some(PathChange::Replaced) => return Search::Anywhere,
            };
        }
        search
    }

    /// Longest project-module prefix of `fqn` and the remaining dotted part. A whole name that
    /// is a module is its package's attribute when the package binds it to something else
    /// ([`FromImport::Attribute`]): `pkg.parse` after `from pkg import parse` is the function
    /// `pkg/__init__.py` imports with `from .parse import parse`. A longer name keeps the
    /// module (`pkg.parse.parse` is what `from pkg.parse import parse` binds, which takes the
    /// module `pkg.parse` itself).
    fn split_module<'s>(&self, fqn: &'s str) -> Option<(&ModuleDecls, &'s str)> {
        let mut found = None;
        let module = module_prefix(fqn, |prefix| {
            found = self.module(prefix);
            found.is_some()
        })?;
        if module.len() == fqn.len()
            && let Some((package, name)) = fqn.rsplit_once('.')
            && self.from_import(package, name) == FromImport::Attribute
            && let Some(package) = self.module(package)
        {
            return Some((package, name));
        }
        Some((found?, fqn.get(module.len() + 1..).unwrap_or("")))
    }

    /// What `from package import name` binds, and `name` read on the module `package`, when
    /// the project has the module `package.name`. Python takes the attribute `name` of
    /// `package` once its `__init__.py` has run and imports the submodule only when there is
    /// none; the first import of a submodule binds it on its package, and a later binding in
    /// the package's body replaces it. So the package's last binding of `name` decides: none,
    /// or an import of the submodule itself, leaves the submodule; an import, alias, or star
    /// import whose object is read through the submodule (`from .parse import parse`, `parse =
    /// parse.parse` after `from . import parse`, `from .parse import *`) is bound after the
    /// submodule was loaded, for good; any other binding holds only until something first
    /// imports the submodule.
    pub fn from_import(&self, package: &str, name: &str) -> FromImport {
        let Some(decls) = self.module(package) else {
            return FromImport::Submodule;
        };
        if decls.unread {
            return FromImport::Open;
        }
        // An import the package's body binds again (`try: from ._speedups import parse` with a
        // fallback, or a later assignment) holds whichever binding ran last.
        if !decls.aliases.contains_key(name)
            && decls.imports.contains_key(name)
            && decls.fixed_import(name).is_none()
        {
            return FromImport::Open;
        }
        // The object the package's binding names: its import or alias, or the one a star
        // import takes the name from when the package binds it no other way.
        let star;
        let bound = match decls.imports.get(name).or_else(|| decls.aliases.get(name)) {
            Some(target) => Some(target.as_str()),
            None if decls.binds(name) => return FromImport::Open,
            None => match self.star_binding(decls, name) {
                StarBinding::Unbound => None,
                StarBinding::Bound { target, .. } => {
                    star = target;
                    Some(star.as_str())
                }
                StarBinding::Open => return FromImport::Open,
            },
        };
        // The name of the object bound past the submodule's: `""` for the submodule itself.
        let past_submodule = bound.map(|target| {
            target
                .strip_prefix(package)
                .and_then(|t| t.strip_prefix('.'))
                .and_then(|t| t.strip_prefix(name))
        });
        match past_submodule {
            Some(Some("")) => FromImport::Submodule,
            Some(Some(rest)) if rest.starts_with('.') => FromImport::Attribute,
            Some(_) => FromImport::Open,
            None => FromImport::Submodule,
        }
    }

    /// What the star imports of the module `m` bind `name` to, for a name `m` binds no other
    /// way ([`ModuleDecls::binds`]). `from s import *` binds the names `s` lists in its
    /// `__all__` ([`ModuleDecls::exports`]), and without one the public names (no leading
    /// underscore) the namespace of `s` holds, those its own star imports bind among them.
    /// Through an `__all__` heki cannot read (built at run time, changed in place), it binds
    /// any name the namespace may hold to an object heki cannot name. A star import of a
    /// module outside the project binds names only the catalog describes; it leaves open a
    /// name another star import binds. The answer is kept for the life of the indexes.
    pub(crate) fn star_binding(&self, m: &ModuleDecls, name: &str) -> StarBinding {
        if m.star_imports.is_none() || m.binds(name) {
            return StarBinding::Unbound;
        }
        let key = (m.module_name.clone(), name.to_owned());
        self.kept(&self.star_bindings, key, || {
            let mut held = FxHashMap::default();
            held.insert(m.module_name.clone(), Holds::No);
            self.stars_bind(m, name, &mut held, 0)
        })
    }

    /// What the star imports of `m` that run after the statement of its module-level definition
    /// `name` bind the name to ([`ModuleDecls::late_stars`]), as [`Self::stars_bind`] reads
    /// them, except that a module outside the project may bind it to any object (open). The
    /// answer is kept for the life of the indexes.
    fn late_star_binding(&self, m: &ModuleDecls, name: &str) -> StarBinding {
        let Some(&from) = m.late_stars.as_deref().and_then(|late| late.get(name)) else {
            return StarBinding::Unbound;
        };
        let key = (m.module_name.clone(), name.to_owned());
        self.kept(&self.late_star_bindings, key, || {
            let stars = m.star_imports.as_deref().unwrap_or_default();
            let mut held = FxHashMap::default();
            held.insert(m.module_name.clone(), Holds::No);
            let late = stars.get(from as usize..).unwrap_or_default();
            match self.sources_bind(late, name, &mut held, 0) {
                (_, true) => StarBinding::Open,
                (binding, false) => binding,
            }
        })
    }

    /// Whether `from source import *`, a star import of the module `importer`, may bind `name`:
    /// a module outside the project may bind any name; one of the project binds what
    /// [`Self::star_binding`] reads through it.
    pub fn star_import_may_bind(&self, importer: &str, source: &str, name: &str) -> bool {
        let mut held = FxHashMap::default();
        held.insert(importer.to_owned(), Holds::No);
        !matches!(
            self.star_import_binds(source, name, &mut held, 1),
            StarSource::Nothing
        )
    }

    /// [`Self::star_binding`] for the star imports of `m`, which `depth` star imports nest;
    /// `held` keeps what [`Self::module_holds`] answered so far, and `No` for the modules it is
    /// still answering, so a star import back into one of them binds nothing more: each module
    /// is read once.
    fn stars_bind(
        &self,
        m: &ModuleDecls,
        name: &str,
        held: &mut FxHashMap<String, Holds>,
        depth: usize,
    ) -> StarBinding {
        let sources = m.star_imports.as_deref().unwrap_or_default();
        match self.sources_bind(sources, name, held, depth) {
            (StarBinding::Bound { .. }, true) => StarBinding::Open,
            (binding, _) => binding,
        }
    }

    /// What the star imports of the modules `sources` bind `name` to, as [`Self::stars_bind`]
    /// reads them, past the modules outside the project; and whether one of them imports such
    /// a module.
    fn sources_bind(
        &self,
        sources: &[String],
        name: &str,
        held: &mut FxHashMap<String, Holds>,
        depth: usize,
    ) -> (StarBinding, bool) {
        let mut outside = false;
        let mut binding = StarBinding::Unbound;
        for source in sources {
            match self.star_import_binds(source, name, held, depth + 1) {
                StarSource::Nothing => {}
                StarSource::Anything => outside = true,
                StarSource::Binds(Some((target, origin))) => match &binding {
                    StarBinding::Unbound => binding = StarBinding::Bound { target, origin },
                    // Whichever ran last, the name holds the object of `origin`.
                    StarBinding::Bound { origin: kept, .. } if *kept == origin => {}
                    _ => return (StarBinding::Open, outside),
                },
                StarSource::Binds(None) => return (StarBinding::Open, outside),
            }
        }
        (binding, outside)
    }

    /// What `from source import *` binds `name` to ([`Self::stars_bind`]), `source` nested
    /// `depth` star imports deep.
    fn star_import_binds(
        &self,
        source: &str,
        name: &str,
        held: &mut FxHashMap<String, Holds>,
        depth: usize,
    ) -> StarSource {
        let Some(s) = self.module(source) else {
            return StarSource::Anything;
        };
        if s.unread {
            return StarSource::Binds(None);
        }
        let read_there = || format!("{source}.{name}");
        let listed = match s.exports.as_deref() {
            Some(names) if names.iter().any(|n| n == name) => true,
            Some(_) => return StarSource::Nothing,
            None if s.binds("__all__") => {
                return match self.module_holds(s, name, held, depth) {
                    Holds::No => StarSource::Nothing,
                    Holds::Yes(_) | Holds::Maybe => StarSource::Binds(None),
                };
            }
            None if name.starts_with('_') => return StarSource::Nothing,
            None => false,
        };
        match self.module_holds(s, name, held, depth) {
            Holds::Yes(origin) => StarSource::Binds(Some((read_there(), origin))),
            // A listed name the module does not bind is the submodule the import loads.
            Holds::No if listed => StarSource::Binds(Some((read_there(), read_there()))),
            Holds::No => StarSource::Nothing,
            Holds::Maybe => StarSource::Binds(None),
        }
    }

    /// Whether the namespace of the module `s`, nested `depth` star imports deep, holds `name`
    /// once its body has run ([`Self::stars_bind`]).
    fn module_holds(
        &self,
        s: &ModuleDecls,
        name: &str,
        held: &mut FxHashMap<String, Holds>,
        depth: usize,
    ) -> Holds {
        if s.binds(name) {
            // An import the module does not bind again holds the object it imports.
            let origin = s.fixed_import(name).cloned();
            return Holds::Yes(origin.unwrap_or_else(|| format!("{}.{name}", s.module_name)));
        }
        if s.star_imports.is_none() {
            return Holds::No;
        }
        if let Some(holds) = held.get(&s.module_name) {
            return holds.clone();
        }
        if depth == MAX_STAR_IMPORT_DEPTH {
            return Holds::Maybe;
        }
        held.insert(s.module_name.clone(), Holds::No);
        let holds = match self.stars_bind(s, name, held, depth) {
            StarBinding::Unbound => Holds::No,
            StarBinding::Bound { origin, .. } => Holds::Yes(origin),
            StarBinding::Open => Holds::Maybe,
        };
        held.insert(s.module_name.clone(), holds.clone());
        holds
    }

    /// The direct project subclasses of the canonical class `class`.
    fn direct_subclasses(&self, class: &str) -> &[String] {
        self.read(|k| note(&mut k.subclasses, class));
        self.subclasses.get(class).map_or(&[], Vec::as_slice)
    }

    /// Chase re-exports (`pkg.Foo` where `pkg/__init__.py` does `from .sub import Foo`).
    pub fn canonical(&self, fqn: &str) -> String {
        self.chase(fqn, Chase::Reexports)
    }

    /// [`Self::canonical`] for a value read through the name (a constant, a small display):
    /// `None` when a module on the way binds the name it re-exports again
    /// ([`ModuleDecls::fixed_import`]).
    pub fn canonical_value(&self, fqn: &str) -> Option<String> {
        self.follow(fqn, Chase::Values).map(|found| found.name)
    }

    /// `fqn` chased as `how` says (at most `MAX_ALIAS_CHASE` steps).
    fn chase(&self, fqn: &str, how: Chase) -> String {
        self.follow(fqn, how)
            .map_or_else(|| fqn.to_owned(), |found| found.name)
    }

    /// [`Self::chase`]: `None` only for [`Chase::Values`]. A module alias names the object it
    /// bound (`_orig = fetch`, then `fetch = _patched`), so a definition the module binds again
    /// stays that definition past it ([`Followed::definition`]).
    fn follow(&self, fqn: &str, how: Chase) -> Option<Followed> {
        let aliases = matches!(how, Chase::Objects | Chase::Types | Chase::Declared);
        let values = how == Chase::Values;
        let mut cur = fqn.to_owned();
        let mut aliased_in: Option<String> = None;
        let mut declared_only = false;
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
            if how == Chase::Types && tail.is_none() && m.type_names.contains_key(first) {
                break;
            }
            // `imports` holds the names an import binds once the module has run
            // (`ModuleImport::rebound_at`): a class or function of the same name is shadowed by
            // it.
            let bound_elsewhere;
            let target = match m.imports.get(first) {
                Some(_) if values && m.fixed_import(first).is_none() => return None,
                Some(target) => {
                    aliased_in = None;
                    target
                }
                None => {
                    if aliases {
                        let rebound_definition = (m.aliases.contains_key(first)
                            || m.alias_paths.contains_key(first))
                            && (m.classes.contains_key(first) || m.functions.contains_key(first));
                        if rebound_definition
                            && aliased_in.as_deref() == Some(m.module_name.as_str())
                        {
                            return Some(Followed {
                                name: cur,
                                definition: true,
                                declared_only,
                            });
                        }
                    }
                    match m.aliases.get(first) {
                        Some(target) if aliases && !self.alias_rebound(&m.module_name, first) => {
                            aliased_in = Some(m.module_name.clone());
                            target
                        }
                        Some(_) => break,
                        // A name the module binds no other way is what its star imports bind,
                        // else what its `__getattr__` serves or its `if TYPE_CHECKING:` import
                        // binds.
                        None => match self.star_binding(m, first) {
                            StarBinding::Bound { target, .. } => {
                                aliased_in = None;
                                bound_elsewhere = target;
                                &bound_elsewhere
                            }
                            StarBinding::Open if values => return None,
                            StarBinding::Unbound => match self.unbound_name(m, first, how) {
                                Some((target, declared)) => {
                                    aliased_in = None;
                                    declared_only |= declared;
                                    bound_elsewhere = target;
                                    &bound_elsewhere
                                }
                                None => break,
                            },
                            StarBinding::Open => break,
                        },
                    }
                }
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
        Some(Followed {
            name: cur,
            definition: false,
            declared_only,
        })
    }

    /// What the name `name`, which no statement of `m` binds nor a star import, is for the chase
    /// `how`, and whether that names only the class a type checker reads
    /// ([`Followed::declared_only`]): what the module's `__getattr__` serves under it
    /// ([`Self::served_by_getattr`]), a class object that may be a subclass only for the chases
    /// reading names as a type checker does; and for those, what its `if TYPE_CHECKING:` import
    /// binds, first for an annotation.
    fn unbound_name(&self, m: &ModuleDecls, name: &str, how: Chase) -> Option<(String, bool)> {
        let checked = || {
            m.type_checking_binding(name)
                .filter(|_| how.reads_type_checking())
                .map(|target| (target.clone(), true))
        };
        if how.reads_annotations()
            && let Some(found) = checked()
        {
            return Some(found);
        }
        let served = match self.served_by_getattr(m, name) {
            Some(Ty::Ref(target)) => Some((target, false)),
            Some(Ty::Class(class)) if how.reads_type_checking() => Some((class, true)),
            _ => None,
        };
        served.or_else(checked)
    }

    /// What the `__getattr__` of `m` serves under `name` ([`ModuleDecls::getattr_exports`],
    /// or what the hook a factory made serves, [`GetattrExports::made_by`]), unless another
    /// module may make it serve another object ([`Self::getattr_open`]).
    fn served_by_getattr(&self, m: &ModuleDecls, name: &str) -> Option<Ty> {
        let exports = m.getattr_exports.as_deref()?;
        let module = &m.module_name;
        self.read(|k| note(&mut k.grown_globals, &format!("{module}.__getattr__")));
        if self.getattr_open.contains(module) {
            return None;
        }
        match exports.made_by.as_deref() {
            None => exports.served.get(name).cloned(),
            // Python calls the hook only for a name the module does not bind.
            Some(_) if m.binds(name) && !m.type_checking_only.contains(name) => None,
            Some(made) => self.made_hook(m, made)?.serves(name).map(|(ty, _)| ty),
        }
    }

    /// The hook the call `made` makes for `m` ([`MadeHook`]), the factory's module read through
    /// [`Self::module`], when the call binds the parameters the hook depends on as it needs:
    /// the table to a table of `m`, and each guard of a store to a false literal. The factory
    /// is the function its fqn names in its module, not followed through re-exports: following
    /// names reads what modules serve, which this decides.
    fn made_hook<'p>(
        &'p self,
        m: &'p ModuleDecls,
        made: &'p MadeBy,
    ) -> Option<getattr_exports::Instance<'p>> {
        let (fm, qualname) = self.split_module(&made.factory)?;
        let hook =
            (fm.hook_factories.as_deref()?.get(qualname)?.iter()).find(|h| h.item == made.item)?;
        getattr_exports::Instance::of(m, made, hook)
    }

    /// [`Self::getattr_open`]: the modules with a `__getattr__` heki follows that another
    /// module stores into under a name heki cannot read (`setattr(module, key, v)`, which may
    /// bind a name the hook serves), or one of whose hook's globals another module rebinds or
    /// stores into (its table, the function it calls, the hook itself).
    fn getattr_hooks_changed(&self) -> FxHashSet<String> {
        let touched = |fqn: &str| {
            self.rebound_elsewhere.contains(fqn)
                || self.rebound_by_getattr(fqn)
                || self.call_rebound(fqn)
                || (self.split_module(fqn))
                    .is_some_and(|(m, _)| self.global_stored_elsewhere(m, fqn))
        };
        // The hook's own name and the factories it calls are among its globals (another module
        // may replace their code), and a store under a name heki cannot read touches every
        // global of the module.
        (self.modules.values())
            .filter(|m| {
                let Some(exports) = m.getattr_exports.as_deref() else {
                    return false;
                };
                let made = (exports.made_by.as_deref()).and_then(|made| self.made_hook(m, made));
                exports.globals.iter().any(|g| touched(g))
                    || made.is_some_and(|made| made.globals().iter().any(|g| touched(g)))
            })
            .map(|m| m.module_name.clone())
            .collect()
    }

    /// Whether another module rebinds or deletes the module alias `name` of `module`
    /// ([`ModuleDecls::aliases`]): it is then no longer the object its module binds.
    fn alias_rebound(&self, module: &str, name: &str) -> bool {
        let fqn = format!("{module}.{name}");
        self.read(|k| note(&mut k.grown_globals, &fqn));
        self.rebound_elsewhere.contains(&fqn) || self.rebound_by_getattr(&fqn)
    }

    /// The object the name `fqn` stands for: through re-exports, as [`Self::canonical`], and
    /// through the module aliases it names (`pkg.model._set` for `_set = object.__setattr__` in
    /// `pkg.model`). The name itself stays the alias's: only values are the object.
    pub fn object_named(&self, fqn: &str) -> String {
        self.named_through(fqn, Chase::Objects)
    }

    /// The class the name `fqn` names in an annotation: [`Self::object_named`], read as a type
    /// checker reads it ([`Chase::Declared`]).
    fn declared_class(&self, fqn: &str) -> String {
        self.named_through(fqn, Chase::Declared)
    }

    /// The name `fqn` chased as `how` says, also past the class body binding it to another
    /// name ([`Self::class_aliased`]).
    fn named_through(&self, fqn: &str, how: Chase) -> String {
        let named = self.chase(fqn, how);
        self.class_aliased(&named)
            .map_or(named, |target| self.chase(&target, how))
    }

    /// The name `Class.attr` stands for when the class body defining `attr` binds it last to one
    /// (`Holder.REQUIRE` of `REQUIRE = staticmethod(require)`, [`Self::class_alias`]); a `def`
    /// of the name before that binding no longer holds it.
    fn class_aliased(&self, fqn: &str) -> Option<String> {
        let (class, attr) = fqn.rsplit_once('.')?;
        let (m, qualname) = self.split_module(class)?;
        if !m.classes.contains_key(qualname) {
            return None;
        }
        self.class_alias(class, attr)
            .map(|(named, _)| named.to_owned())
    }

    /// The value the name `fqn` gives when its module or class body binds it last to different
    /// objects on different paths ([`ModuleDecls::alias_paths`], [`Self::class_aliases`]): one
    /// of them, each through [`Self::object_named`], a definition of the module or class as the
    /// object its statement made.
    pub(crate) fn named_on_paths(&self, fqn: &str) -> Option<Ty> {
        let Followed {
            name: named,
            definition,
            ..
        } = self.follow(fqn, Chase::Objects)?;
        if definition {
            return None;
        }
        let (m, rest) = self.split_module(&named)?;
        if let Some(objects) = m.alias_paths.get(rest) {
            let objects = objects.iter().map(|o| {
                let definition = o
                    .strip_prefix(m.module_name.as_str())
                    .and_then(|o| o.strip_prefix('.'))
                    .is_some_and(|q| m.classes.contains_key(q) || m.functions.contains_key(q));
                if definition {
                    o.clone()
                } else {
                    self.object_named(o)
                }
            });
            // Another module storing the name may leave anything there.
            return Ty::one_of(objects).filter(|_| !self.alias_rebound(&m.module_name, rest));
        }
        let (class, attr) = named.rsplit_once('.')?;
        if !self.is_project_class(class) {
            return None;
        }
        let alias = self.class_aliases(class, attr)?;
        if alias.defined.is_none() && alias.names.len() == 1 {
            return None;
        }
        let names = alias.names.iter().map(|(name, _)| self.object_named(name));
        Ty::one_of(names.chain(alias.defined))
    }

    /// The value the name `fqn` gives: a project global holding an instance is that instance,
    /// a module alias the object it names, a name bound to different objects on different paths
    /// one of them ([`Self::named_on_paths`]), anything else the object of that name.
    pub(crate) fn named_ty(&self, fqn: &str) -> Ty {
        if let Some(t) = self.named_on_paths(fqn) {
            return t;
        }
        let fqn = self.object_named(fqn);
        match self.symbol(&fqn) {
            Symbol::Global(t) => self.declared_ty(&t).unwrap_or(Ty::Ref(fqn)),
            _ => Ty::Ref(fqn),
        }
    }

    /// The value of the number or boolean constant `fqn` (`pkg.settings.PAUSE`), through
    /// re-exports ([`Self::canonical_value`]), unless another module rebinds it.
    pub fn literal_constant(&self, fqn: &str) -> Option<LiteralValue> {
        let fqn = self.canonical_value(fqn)?;
        let (m, rest) = self.split_module(&fqn)?;
        let constant = m.literal_constants.get(rest)?;
        (!self.constant_rebound(&fqn, constant.rebindable))
            .then(|| literal_value(&constant.value))
            .flatten()
    }

    /// Whether the module constant `fqn` names a file ([`ModuleDecls::file_constants`]),
    /// through re-exports, unless another module rebinds it.
    pub fn file_constant(&self, fqn: &str) -> bool {
        self.canonical_value(fqn).is_some_and(|fqn| {
            self.split_module(&fqn)
                .is_some_and(|(m, rest)| m.file_constants.contains(rest))
                && !self.constant_rebound(&fqn, true)
        })
    }

    /// The value of the string constant `fqn` (`pkg.keys.KEY`, `pkg.model.Model.KEY`), through
    /// re-exports and aliases ([`Self::constant_value`]).
    pub fn str_constant(&self, fqn: &str) -> Option<&str> {
        self.constant_value(fqn)?.text()
    }

    /// The texts of the items of the tuple constant `fqn` (`pkg.keys.RECORDS`), through
    /// re-exports and aliases ([`Self::constant_value`]), when each item is a string constant.
    pub fn str_tuple(&self, fqn: &str) -> Option<Vec<&str>> {
        self.tuple_texts(self.constant_value(fqn)?)
    }

    /// The texts of the items of `value` when it is a tuple of string constants.
    fn tuple_texts<'s>(&'s self, value: &'s StrValue) -> Option<Vec<&'s str>> {
        let StrValue::Tuple(items) = value else {
            return None;
        };
        (items.iter())
            .map(|item| match item {
                StrValue::Alias(fqn) => self.str_constant(fqn),
                item => item.text(),
            })
            .collect()
    }

    /// What the constant `fqn` is bound to, a text or a tuple, through re-exports
    /// ([`Self::canonical_value`]) and constants bound to other constants (at most
    /// `MAX_ALIAS_CHASE` steps).
    fn constant_value(&self, fqn: &str) -> Option<&StrValue> {
        let mut cur = self.canonical_value(fqn)?;
        for _ in 0..MAX_ALIAS_CHASE {
            let (m, rest) = self.split_module(&cur)?;
            let constant = m.str_constants.get(rest)?;
            if self.constant_rebound(&cur, constant.rebindable) {
                return None;
            }
            match &constant.value {
                StrValue::Alias(next) => cur = self.canonical_value(next)?,
                value => return Some(value),
            }
        }
        None
    }

    /// The value of the class-level string constant `attr` read on an instance of the project
    /// class `class` or on the class object (`self.KEY`, `cls.KEY`, `Model.KEY`): the
    /// definition of `attr` the MRO finds first must be a constant. Read on anything but the
    /// class object the code names, the object may be of a subclass, and the MRO of each must
    /// find the same definition: a subclass defining `attr` again, or listing a base that
    /// defines it before `class` (`class Child(Other, Model)`), reads its own value. A class
    /// that code stores `attr` on (`Child.KEY = "x"`, in its module or another) defines it too:
    /// its own value then hides the inherited constant. A field (`KEY: Final[str] = "x"` in a
    /// dataclass) is no constant on an instance, which holds the value it was given, and on a
    /// class object only while the default stays its class attribute along the way: no class
    /// with `slots=True` or built by a `dataclass_transform`. An instance of a protocol is one
    /// of its structural implementations, which define the name themselves: each must hold the
    /// same constant.
    ///
    /// The answer is kept for the life of the indexes: a read of it while recording reads
    /// what computing it read.
    pub fn class_str_constant(
        &self,
        class: &str,
        attr: &str,
        read: ConstantRead,
    ) -> Option<String> {
        let key = (class.to_owned(), attr.to_owned(), read);
        self.kept(&self.class_constants, key, || {
            self.find_class_str_constant(class, attr, read)
        })
    }

    /// The answer `cache` keeps for `key`, or `compute`'s, then kept with what computing it
    /// read of the declarations while recording; a read of a kept answer while recording reads
    /// what computing it read.
    fn kept<K: Eq + std::hash::Hash, T: Clone>(
        &self,
        cache: &KeptAnswers<K, T>,
        key: K,
        compute: impl FnOnce() -> T,
    ) -> T {
        let recording = self.recorder.on.load(Ordering::Relaxed);
        // An answer kept without its reads is computed again while recording, to record them.
        let shard = cache.shard(&key);
        let known = shard
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .filter(|k| !recording || k.reads.is_some())
            .cloned();
        if let Some(known) = known {
            if let Some(reads) = known.reads.filter(|_| recording) {
                self.recorded().replayed.push(reads);
            }
            return known.value;
        }
        let (value, reads) = if recording {
            // What this computation reads, apart from what the recording read before.
            let before = std::mem::take(&mut *self.recorded());
            let value = compute();
            let mut recorded = self.recorded();
            let reads = Arc::new(std::mem::replace(&mut *recorded, before).into_keys());
            recorded.replayed.push(Arc::clone(&reads));
            (value, Some(reads))
        } else {
            (compute(), None)
        };
        let known = Kept {
            value: value.clone(),
            reads,
        };
        shard
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, known);
        value
    }

    /// What attribute `attr` of an instance of the project class `class` gives a queue
    /// (`crate::capacity`): what the declarations record of every store an instance may hold,
    /// with the attributes and globals the records name followed, in terms of parameters only.
    pub fn attr_capacity(&self, class: &str, attr: &str) -> CapacityOf {
        self.capacity(CapacityOf {
            attrs: BTreeSet::from([(class.to_owned(), attr.to_owned())]),
            ..CapacityOf::default()
        })
    }

    /// What the module global `fqn` gives a queue, as [`Self::attr_capacity`].
    pub fn global_capacity(&self, fqn: &str) -> CapacityOf {
        self.capacity(CapacityOf {
            globals: BTreeSet::from([fqn.to_owned()]),
            ..CapacityOf::default()
        })
    }

    /// `start` resolved through the declarations ([`crate::capacity::resolve_in_decls`], which
    /// keeps the answer of each attribute and global until the indexes change).
    fn capacity(&self, start: CapacityOf) -> CapacityOf {
        crate::capacity::resolve_in_decls(self, start)
    }

    /// The answer kept for an attribute or a global (`name`, a `CapacityOf` naming only it), with
    /// what computing it read. One kept without its reads is no answer while recording: it is
    /// computed again, to record them.
    pub(crate) fn known_capacity(
        &self,
        name: &CapacityOf,
    ) -> Option<(CapacityOf, Option<Arc<DeclKeys>>)> {
        let recording = self.recorder.on.load(Ordering::Relaxed);
        self.capacities
            .shard(name)
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .filter(|k| !recording || k.reads.is_some())
            .map(|k| (k.value.clone(), k.reads.clone()))
    }

    pub(crate) fn keep_capacity(
        &self,
        name: CapacityOf,
        value: CapacityOf,
        reads: Option<Arc<DeclKeys>>,
    ) {
        self.capacities
            .shard(&name)
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name, Kept { value, reads });
    }

    /// Run `f`, and return with its result what it read of these declarations while recording
    /// (`None` when not recording), apart from what the recording read before.
    pub(crate) fn reads_of<T>(&self, f: impl FnOnce() -> T) -> (T, Option<DeclKeys>) {
        if !self.recorder.on.load(Ordering::Relaxed) {
            return (f(), None);
        }
        let before = std::mem::take(&mut *self.recorded());
        let out = f();
        let reads = std::mem::replace(&mut *self.recorded(), before).into_keys();
        (out, Some(reads))
    }

    /// Add what computing a kept answer read to the recording.
    pub(crate) fn replay(&self, reads: Arc<DeclKeys>) {
        if self.recorder.on.load(Ordering::Relaxed) {
            self.recorded().replayed.push(reads);
        }
    }

    /// What the classes whose stores an instance of `class` may hold record of `attr`
    /// (`ClassDecl::attr_capacities`): `class`, its project subclasses, and the project classes
    /// along their MROs. A class defining the attribute without a record, or whose attribute
    /// another module stores, adds an unknown capacity.
    pub(crate) fn stored_attr_capacities(&self, class: &str, attr: &str) -> Vec<CapacityOf> {
        let class = self.canonical(class);
        let mut instances = self.subclasses_of(&class);
        instances.push(class);
        let family: BTreeSet<String> = instances
            .iter()
            .flat_map(|c| self.mro(c))
            .filter(|c| self.is_project_class(c))
            .collect();
        let mut out = Vec::new();
        for c in &family {
            let decl = self.class(c).expect("a project class has a declaration");
            let defined = decl.members.contains(attr) || decl.rebound_attrs.contains(attr);
            let elsewhere = self.rebound_elsewhere(&format!("{c}.{attr}"));
            match decl.attr_capacities.get(attr) {
                Some(stored) if !elsewhere => out.push(stored.clone()),
                None if !defined && !elsewhere => {}
                _ => out.push(CapacityOf::unknown()),
            }
        }
        out
    }

    /// What the module of the global `fqn` records of it (`ModuleDecls::global_capacities`);
    /// unknown without a record, or when another module rebinds it.
    pub(crate) fn stored_global_capacity(&self, fqn: &str) -> CapacityOf {
        let fqn = self.canonical(fqn);
        let stored = self
            .split_module(&fqn)
            .and_then(|(m, rest)| m.global_capacities.get(rest))
            .filter(|_| !self.rebound_elsewhere(&fqn));
        stored.cloned().unwrap_or_else(CapacityOf::unknown)
    }

    /// [`Self::class_str_constant`], computed.
    fn find_class_str_constant(
        &self,
        class: &str,
        attr: &str,
        read: ConstantRead,
    ) -> Option<String> {
        let defines = |c: &str| {
            self.class(c)
                .is_some_and(|d| d.members.contains(attr) || d.rebound_attrs.contains(attr))
                || self.rebound_elsewhere(&format!("{c}.{attr}"))
        };
        let class = self.canonical(class);
        let owner = self.mro(&class).into_iter().find(|m| defines(m))?;
        let fqn = format!("{owner}.{attr}");
        let value = self.str_constant(&fqn)?;
        let defaults = self.field_defaults(&owner);
        let field = self.declares_field(&fqn) && defaults != FieldDefaults::NoFields;
        if field && (read == ConstantRead::Instance || defaults == FieldDefaults::Replaced) {
            return None;
        }
        let mut family = vec![class.clone()];
        if read != ConstantRead::Class {
            family.extend(self.subclasses_of(&class));
        }
        for c in &family {
            let mro = self.mro(c);
            let at = mro.iter().position(|m| defines(m))?;
            // A class with `slots=True` (or built by a transform) ahead of the owner replaces
            // the field's default with its own class attribute.
            let replaced = || {
                mro[..at]
                    .iter()
                    .any(|m| self.field_defaults(m) == FieldDefaults::Replaced)
            };
            if mro[at] != owner || field && replaced() {
                return None;
            }
        }
        if read != ConstantRead::Class
            && let Some(implementers) = self.protocol(&class)
        {
            let same =
                |i: &String| self.class_str_constant(i, attr, read).as_deref() == Some(value);
            if !implementers?.iter().all(same) {
                return None;
            }
        }
        Some(value.to_owned())
    }

    /// Whether the class-level string constant `fqn` declares a field where its class's
    /// annotations declare instance fields (`StrConstant::declares_field`).
    fn declares_field(&self, fqn: &str) -> bool {
        self.split_module(fqn)
            .and_then(|(m, rest)| m.str_constants.get(rest))
            .is_some_and(|c| c.declares_field)
    }

    pub(crate) fn symbol(&self, fqn: &str) -> Symbol<'_> {
        let Some((m, rest)) = self.split_module(fqn) else {
            return Symbol::External;
        };
        if rest.is_empty() {
            return match fqn.rsplit_once('.') {
                Some((package, name)) if self.from_import(package, name) == FromImport::Open => {
                    Symbol::Unknown
                }
                _ => Symbol::Module,
            };
        }
        if m.unread {
            return Symbol::Unread;
        }
        let definition = m.definition_path(rest);
        if let Some(c) = m.classes.get(definition.as_ref()) {
            return Symbol::Class(c);
        }
        if let Some(f) = m.functions.get(definition.as_ref()) {
            let key = FuncKey {
                module: m.module_name.clone(),
                qualname: definition.into_owned(),
            };
            return Symbol::Function(key, f);
        }
        if let Some(t) = self.global_type(m, rest) {
            return Symbol::Global(t);
        }
        // A name the module does not bind is bound where heki does not read the module: a
        // submodule it does not index (a compiled extension) or a name a star import of a
        // module outside the project binds, which only the catalog describes. A star import of
        // a project module binds what `canonical` reads through it, unless that is an object
        // heki cannot name.
        let head = rest.split('.').next().unwrap_or(rest);
        if m.unknown_globals.contains(head)
            || m.classes.contains_key(head)
            || m.functions.contains_key(head)
            || self.star_binding(m, head) != StarBinding::Unbound
        {
            Symbol::Unknown
        } else {
            Symbol::External
        }
    }

    /// Whether `fqn` is a name under a project module heki could not read.
    fn under_unread(&self, fqn: &str) -> bool {
        self.split_module(fqn).is_some_and(|(m, _)| m.unread)
    }

    /// Whether `fqn` names a project module or something under one.
    pub(crate) fn in_project_module(&self, fqn: &str) -> bool {
        self.split_module(fqn).is_some()
    }

    /// The target of a call to `fqn`, a name no project definition holds: the external function
    /// the catalog may list, or nothing heki knows when `fqn` is under a project module it could
    /// not read (that module, not the catalog's, is what the name refers to).
    pub(crate) fn outside(&self, fqn: String) -> Callee {
        if self.under_unread(&fqn) {
            Callee::Unresolved
        } else {
            Callee::External(fqn)
        }
    }

    /// The catalog entry of `symbol`, a name the project's code reaches; none for a name under
    /// a project module heki could not read ([`Self::outside`]).
    pub fn catalog<'k>(
        &self,
        kb: &'k EffectiveEcosystemKb,
        symbol: &str,
    ) -> Option<&'k EcosystemEntry> {
        if self.under_unread(symbol) {
            return None;
        }
        kb.lookup(symbol)
    }

    fn class(&self, fqn: &str) -> Option<&ClassDecl> {
        match self.symbol(fqn) {
            Symbol::Class(c) => Some(c),
            _ => None,
        }
    }

    /// The declaration of the project class `fqn`.
    pub fn class_decl(&self, fqn: &str) -> Option<&ClassDecl> {
        self.class(&self.canonical(fqn))
    }

    /// The method resolution order of `class` ([`Self::mro`]): `class` first, then its bases,
    /// each external one standing for itself.
    pub fn mro_of(&self, class: &str) -> Vec<String> {
        self.mro(class)
    }

    pub fn is_project_class(&self, fqn: &str) -> bool {
        self.class(fqn).is_some()
    }

    /// Whether an `isinstance` test of `classes` is decided by the MRO of the object's class
    /// alone ([`crate::InstanceTest::nominal`]): each is a project class whose MRO holds only
    /// project classes and `object`, none naming a metaclass.
    pub(crate) fn nominal_classes(&self, classes: &[String]) -> bool {
        classes.iter().all(|class| {
            self.mro(class).iter().all(|c| {
                c == "builtins.object" || self.class(c).is_some_and(|d| d.metaclass.is_none())
            })
        })
    }

    /// Whether the module of the project class `class` may add attributes heki cannot name to
    /// its instances (`setattr(self, name, v)`, `self.__dict__.update(other)`), past the copies
    /// of the namespaces of instances of the class family ([`ClassDecl::adds_unnamed_attrs`]).
    pub fn adds_unnamed_attrs(&self, class: &str) -> bool {
        self.class(class).is_some_and(|c| c.adds_unnamed_attrs)
    }

    /// The names the project class `class` declares itself: its members (class-body names and
    /// the attributes its methods set or annotate on `self`), the attributes its module stores
    /// on instances through other receivers, and those stored on the class object outside the
    /// class body. Empty for a name that is no project class.
    pub fn declared_names(&self, class: &str) -> Vec<&str> {
        let names = self.class(class).into_iter().flat_map(|c| {
            (c.members.iter().map(String::as_str))
                .chain(c.attr_types.keys().map(String::as_str))
                .chain(c.attr_stores.iter().map(|(name, _)| &**name))
                .chain(c.rebound_attrs.iter().map(String::as_str))
        });
        names.collect()
    }

    /// Whether the canonical project class `class` is a `TypedDict`.
    pub fn is_typed_dict(&self, class: &str) -> bool {
        self.read(|k| note(&mut k.class_facts, class));
        self.typed_dicts.contains(class)
    }

    /// The keys a value of the project `TypedDict` `class` holds, sorted: the names the class
    /// bodies along its MRO declare ([`ClassDecl::members`]; a `TypedDict` body declares its keys
    /// and nothing else). `None` when such a value may hold keys none of them names or heki
    /// cannot list them: a class along the MRO passes a class keyword (`total=False`, PEP 728's
    /// `closed=` and `extra_items=`), names a metaclass, binds a name of its body to code (a
    /// method, a call: [`ClassDecl::code_attrs`]), or is no project `TypedDict` (besides
    /// `TypedDict` and `Generic` themselves).
    pub(crate) fn typed_dict_keys(&self, class: &str) -> Option<Vec<String>> {
        if !self.is_typed_dict(class) {
            return None;
        }
        let mut keys = BTreeSet::new();
        for c in self.mro(class) {
            if TYPED_DICT_KEYLESS_BASES.contains(&c.as_str()) {
                continue;
            }
            let decl = self.class(&c).filter(|_| self.is_typed_dict(&c))?;
            let binds_code = decl.members.iter().any(|m| decl.code_attrs.contains(m));
            if decl.passes_keywords || decl.custom_metaclass || binds_code {
                return None;
            }
            keys.extend(decl.members.iter().cloned());
        }
        Some(keys.into_iter().collect())
    }

    /// The small display the module global `fqn` of a project module holds
    /// (`ModuleDecls::small_globals`), through re-exports ([`Self::canonical_value`]), when no
    /// other module changes it
    /// ([`Self::changed_elsewhere`]).
    pub fn small_global(&self, fqn: &str) -> Option<&SmallDisplay> {
        let fqn = self.canonical_value(fqn)?;
        let (m, rest) = self.split_module(&fqn)?;
        let display = m.small_globals.get(rest)?;
        (!self.changed_elsewhere(&fqn, display)).then_some(display)
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

    /// Why attribute `attr` of an instance of `class` is read-only (`MUT108`, `MUT120`): the
    /// first class along the MRO that has it as a field of a frozen class, or declares it
    /// `Final` / `ReadOnly`; or, when `class` is a read-only view, the class along its MRO that
    /// defines the property.
    pub fn readonly_attr(&self, class: &str, attr: &str) -> Option<ReadOnlyAttr> {
        let mro = self.mro(class);
        let declared = mro.iter().find_map(|c| {
            let decl = self.class(c)?;
            self.read(|k| note(&mut k.class_facts, c));
            let kind = if self.frozen.contains(c) && decl.container_fields.contains(attr) {
                ReadOnlyKind::Frozen
            } else if decl.readonly_attrs.contains(attr) {
                ReadOnlyKind::Declared
            } else {
                return None;
            };
            let class = c.clone();
            Some(ReadOnlyAttr {
                class,
                kind,
                holds_container: true,
                typed_read_only: false,
            })
        });
        declared.or_else(|| {
            if !self.is_readonly_view(&mro) {
                return None;
            }
            let (c, decl) = mro
                .iter()
                .find_map(|c| Some((c, self.class(c).filter(|d| d.members.contains(attr))?)))?;
            let access = decl.container_properties.get(attr);
            Some(ReadOnlyAttr {
                class: c.clone(),
                kind: ReadOnlyKind::View,
                holds_container: access.is_some(),
                typed_read_only: access == Some(&ParamAccess::ReadOnly),
            })
        })
    }

    /// Whether the class whose MRO is `mro` is a read-only view: a project class, a protocol or
    /// not, that defines, with every project class along its MRO, nothing but properties without
    /// a setter or deleter (`@property` getters), at least one; no plain attribute, method, or
    /// `__setattr__`, no metaclass heki does not know, and no other base than those defining no
    /// attribute ([`VIEW_BASES`]). Code holding a value through such a class can only read it.
    fn is_readonly_view(&self, mro: &[String]) -> bool {
        let mut getters = false;
        let views = mro.iter().all(|c| {
            self.read(|k| note(&mut k.class_facts, c));
            let Some(decl) = self.class(c) else {
                return VIEW_BASES.contains(&c.as_str());
            };
            getters |= decl.only_getters;
            !decl.custom_metaclass && (decl.only_getters || decl.members.is_empty())
        });
        views && getters
    }

    /// Whether a project class along the MRO of `class` names a metaclass other than `type` and
    /// `abc.ABCMeta` (`metaclass=Meta`, also one heki cannot resolve) or has a base heki names
    /// no class for (`ClassDecl::custom_metaclass`). Python creates every
    /// subclass with it too, and it may replace any attribute of the class with a descriptor
    /// (`setattr(cls, f"_attr_{name}", property(...))` in its `__init__`), so writing an
    /// attribute of an instance may run code heki does not see.
    pub fn has_custom_metaclass(&self, class: &str) -> bool {
        self.mro(class).into_iter().any(|c| {
            self.read(|k| note(&mut k.class_facts, &c));
            self.class(&c).is_some_and(|decl| decl.custom_metaclass)
        })
    }

    /// Whether storing or deleting `attr` on an instance of the project class `class` may run
    /// code instead of changing the instance's own attribute: a class along its MRO defines
    /// `__setattr__` or `__delattr__`, binds `attr` to a property (`ClassDecl::properties`), or
    /// has a metaclass other than `type` ([`Self::has_custom_metaclass`]), which may install a
    /// descriptor; or `__setattr__` or `__delattr__` may be bound on a class along it after its
    /// body ([`Self::bound_after_body`]).
    pub fn attr_write_runs_code(&self, class: &str, attr: &str) -> bool {
        let names = ["__setattr__", "__delattr__"];
        self.has_custom_metaclass(class)
            || self.mro(class).into_iter().any(|c| {
                self.read(|k| note(&mut k.class_facts, &c));
                self.class(&c).is_some_and(|decl| {
                    decl.properties.contains(attr)
                        || decl.members.contains("__setattr__")
                        || decl.members.contains("__delattr__")
                        || self.bound_after_body(&names)
                })
            })
    }

    /// Whether one of `names` may be bound on a class object once its body has run, otherwise
    /// than the body binds it: a store heki reads, wherever it is, may bind one when its name may
    /// be that one and its receiver may be a class object ([`Self::bound_on_classes`]), also one
    /// in a class decorator or an `__init_subclass__` storing on the class it is handed. A
    /// constant name decides, and a name built at run time is ruled out when the constant text at
    /// its ends already differs (`f"__attr_{name}"` is never `__setattr__`). A name of unknown
    /// value (a parameter, a key of `vars()` or of a mapping, a value read from data) binds none
    /// of `names`: R1 takes a special method to be installed under its own name, which gives up
    /// `setattr(cls, computed, hook)` installing `__setattr__`. A receiver heki establishes as an
    /// instance (`self`, what a class call makes, a parameter annotated with a class other than
    /// `type`) binds nothing Python looks a special method up on, and any other may be the class
    /// (`Child.__setattr__ = hook`, `setattr(cls, "__setattr__", hook)` in a function the class is
    /// handed to). Code heki does not read is taken to bind none (R1): a class decorator outside
    /// the project, and a call or a closure the class reaches, bind on it only what the stores
    /// heki reads bind.
    fn bound_after_body(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.bound_on_classes(n))
    }

    /// Whether `class` is a project class and every class a value of it may be under R2
    /// ([`Self::value_classes`], each once) is a project class that binds none of `names`, in
    /// its body or after it ([`Self::bound_after_body`]), and has no metaclass heki does not
    /// know, and every base of one outside the project is a plain base or one `known_base`
    /// accepts: the classes whose code runs on such a value are the project's, which heki
    /// reads, as their bodies and the code heki reads leave them, and those `known_base` stands
    /// for.
    pub fn binds_none_of(
        &self,
        class: &str,
        names: &[&str],
        known_base: impl Fn(&str) -> bool,
    ) -> bool {
        let known = |c: &str| PLAIN_BASES.contains(&c) || known_base(c);
        let class = self.canonical(class);
        // A class outside the project runs code heki does not read.
        self.is_project_class(&class)
            && self.value_classes(&class).iter().all(|c| {
                self.read(|k| note(&mut k.class_facts, c));
                self.class(c).is_some_and(|d| {
                    !d.custom_metaclass
                        && names.iter().all(|n| !d.members.contains(*n))
                        && !self.bound_after_body(names)
                        && d.bases.iter().all(|b| {
                            let base = self.canonical(b);
                            self.is_project_class(&base) || known(&base)
                        })
                })
            })
    }

    /// Whether storing or deleting `attr` on a value of the class `class` changes that
    /// instance's own attribute, or raises, on every class the value may be under R2: no class
    /// along their MROs defines `__setattr__` or `__delattr__`, binds `attr` to a property or to
    /// anything that may be a descriptor (`ClassDecl::code_attrs`), or has a metaclass heki does
    /// not know, and every class outside the project is a plain base or one whose `__setattr__`
    /// the ecosystem catalog says stores on the instance (`catalog_stores`: R3,
    /// `pydantic.BaseModel`); and nothing may bind a hook on one after its body
    /// ([`Self::bound_after_body`]). A strict form of [`Self::attr_write_runs_code`]'s question,
    /// for an exact claim.
    pub fn attr_store_on_instance(
        &self,
        class: &str,
        attr: &str,
        catalog_stores: impl Fn(&str) -> bool,
    ) -> bool {
        self.binds_none_of(class, &["__setattr__", "__delattr__"], &catalog_stores)
            && !self.value_classes(class).iter().any(|c| {
                self.read(|k| note(&mut k.class_facts, c));
                self.class(c)
                    .is_some_and(|d| d.properties.contains(attr) || d.code_attrs.contains(attr))
            })
    }

    /// Whether storing or deleting each of `attrs` on a value of the project class `class` runs
    /// no code on every class it may be under R2: as [`Self::attr_store_on_instance`], except
    /// that a class body may bind the name to a function it defines that no decorator replaces
    /// (a method, which an instance's own attribute shadows) or that is a getter without a
    /// setter or deleter (`FunctionDecl::is_property`: a `property` raises, a cached property's
    /// value is the instance's own attribute).
    pub(crate) fn attr_write_runs_no_code(
        &self,
        class: &str,
        attrs: &[&str],
        catalog_stores: impl Fn(&str) -> bool,
    ) -> bool {
        let plain_function = |c: &str, attr: &str| {
            self.own_method(c, attr)
                .and_then(|k| self.function_decl(&k))
                .is_some_and(|f| f.called_as_defined || f.is_property)
        };
        self.binds_none_of(class, &["__setattr__", "__delattr__"], &catalog_stores)
            && self.value_classes(class).iter().all(|c| {
                self.read(|k| note(&mut k.class_facts, c));
                self.class(c).is_some_and(|d| {
                    attrs.iter().all(|attr| {
                        !d.properties.contains(*attr)
                            && (!d.code_attrs.contains(*attr) || plain_function(c, attr))
                    })
                })
            })
    }

    /// Whether storing each of `attrs` on a value of the project class `class` binds the
    /// instance's own attribute to the value itself and runs no code of the project, on every
    /// class it may be under R2: as [`Self::attr_write_runs_no_code`], and when one of those
    /// classes derives from a base the catalog says stores on the instance (`catalog_stores`),
    /// none of them configures it: binds a model configuration ([`ClassDecl::configures_model`])
    /// or passes class keywords (`ClassDecl::passes_keywords`). With `validate_assignment`,
    /// `pydantic.BaseModel` stores a validated conversion and runs the project's validators,
    /// which may keep the value.
    pub(crate) fn attr_write_stores_value(
        &self,
        class: &str,
        attrs: &[&str],
        catalog_stores: impl Fn(&str) -> bool,
    ) -> bool {
        if !self.attr_write_runs_no_code(class, attrs, &catalog_stores) {
            return false;
        }
        let classes = self.value_classes(class);
        let decls = || classes.iter().filter_map(|c| self.class(c));
        let catalog_base = |d: &ClassDecl| {
            d.bases.iter().any(|b| {
                let base = self.canonical(b);
                !self.is_project_class(&base) && catalog_stores(&base)
            })
        };
        !decls().any(catalog_base) || decls().all(|d| !d.passes_keywords && !d.configures_model())
    }

    /// Whether reading `attr` on a value of the class `class` finds the value an instance
    /// stores there, as [`Self::plain_attr`] decides for project classes, with the classes
    /// outside the project along the MROs of every class the value may be (R2) plain bases or
    /// ones whose `__setattr__` the catalog says stores on the instance, which install no
    /// descriptor and define no `__getattribute__` (`catalog_stores`: `pydantic.BaseModel`).
    pub fn attr_read_finds_stored(
        &self,
        class: &str,
        attr: &str,
        catalog_stores: impl Fn(&str) -> bool,
    ) -> bool {
        let read = self.attr_read(class, attr);
        read.declared
            && !read.runs_hook(true)
            && read.code_binders.is_empty()
            && self.binds_none_of(class, &["__getattribute__", "__getattr__"], catalog_stores)
    }

    /// Whether a call of the class `class` may give an existing instance, or run a metaclass
    /// `__call__` that returns anything (an instance of a subclass): what the call makes is not
    /// decided by the class body.
    pub(crate) fn construction_is_open(&self, class: &str) -> bool {
        self.may_reuse_instances(class) || self.has_custom_metaclass(class)
    }

    /// What a call constructing an instance of the project class `class` keeps of it
    /// ([`Constructor`]), from the `__init__` the call runs, that of the first class along the
    /// MRO defining one, and every one it reaches through `super().__init__(...)`:
    /// - [`Constructor::Unknown`] when the class may give an existing instance
    ///   ([`Self::may_reuse_instances`]) or has a metaclass other than `type` or `abc.ABCMeta`, or
    ///   a base heki names no class for ([`Self::has_custom_metaclass`], whose `__call__` may run
    ///   anything); when the classes along the MRO up to one whose `__init__` the call runs may
    ///   bind another `__init__` than their bodies do ([`Self::lookup_rebound`]); when such an
    ///   `__init__` is a project function a decorator replaces; or when a star import of the
    ///   module of a [`PlainInit`] may bind a builtin it reads (`super`, `list`).
    /// - [`Constructor::HandsToInit`] when one of them is a project `__init__` that is no
    ///   [`PlainInit`], or a store of theirs may run code of the project (a property's setter, a
    ///   descriptor, `__setattr__`, [`Self::attr_write_stores_value`]): what keeps the instance
    ///   is what the first one does with its first parameter.
    /// - [`Constructor::KeepsNothing`] otherwise: every one is a [`PlainInit`], `object`'s, or
    ///   that of a class outside the project, which keeps nothing of the instance (a dataclass's
    ///   or a pydantic model's construction runs only such an `__init__`). Its containers are
    ///   those of the first one, when every one is a [`PlainInit`], no later one stores an
    ///   attribute the first makes a container for (it may rebind it), and no class along the MRO
    ///   has a class decorator (which may generate another `__init__`), `__init_subclass__`, or a
    ///   `dataclass_transform`. A base's `__init__` replacing the instance's namespace makes every
    ///   attribute shared. A read of one of the attributes finds its container when reading it
    ///   finds what the instance stores ([`Self::attr_read_finds_stored`]).
    pub(crate) fn constructor(
        &self,
        class: &str,
        catalog_stores: impl Fn(&str) -> bool,
    ) -> Constructor<'_> {
        if self.construction_is_open(class) {
            return Constructor::Unknown;
        }
        let mro = self.mro(class);
        let decls: Vec<Option<&ClassDecl>> = (mro.iter())
            .map(|c| {
                self.read(|k| note(&mut k.class_facts, c));
                self.class(c)
            })
            .collect();
        // From the class at `from` on, the first class defining `__init__`: a project class's
        // own, or one outside the project other than a plain base; `None` when only
        // `object.__init__` is left.
        let next_init = |from: usize| {
            (from..decls.len()).find(|&i| match decls[i] {
                Some(d) => d.members.contains("__init__"),
                None => !PLAIN_BASES.contains(&mro[i].as_str()),
            })
        };
        let star_bound = |c: &str, init: &PlainInit| {
            let module = self.split_module(c).map(|(m, _)| m);
            (init.builtins.iter()).any(|name| {
                module.is_some_and(|m| {
                    !m.binds(name)
                        && (m.star_imports.iter().flatten())
                            .any(|source| self.star_import_may_bind(&m.module_name, source, name))
                })
            })
        };
        let mut from = 0;
        let mut first: Option<&PlainInit> = None;
        let mut stores: Vec<&str> = Vec::new();
        // Every `__init__` the call runs is a project one, and none rebinds a container of the
        // first.
        let mut all_plain = true;
        loop {
            let found = next_init(from);
            let reached = found.map_or(mro.len(), |i| i + 1);
            if self.lookup_rebound(&mro[from..reached], "__init__") {
                return Constructor::Unknown;
            }
            let Some(i) = found else { break };
            let Some(d) = decls[i] else {
                all_plain = false;
                break;
            };
            let Some(init) = d.plain_init.as_deref() else {
                let decl =
                    (self.own_method(&mro[i], "__init__")).and_then(|k| self.function_decl(&k));
                // A static or class method `__init__` is not handed the instance, and its
                // call ends the chain.
                if decl
                    .as_ref()
                    .is_some_and(|f| f.called_as_defined && (f.is_staticmethod || f.is_classmethod))
                {
                    all_plain = false;
                    break;
                }
                let as_defined = decl.is_some_and(|f| f.called_as_defined);
                return if as_defined {
                    Constructor::HandsToInit
                } else {
                    Constructor::Unknown
                };
            };
            if star_bound(&mro[i], init) {
                return Constructor::Unknown;
            }
            if let Some(first) = first {
                let rebinds = |(a, _): &(Box<str>, _)| init.stores.binary_search(a).is_ok();
                all_plain &= !first.containers.iter().any(rebinds);
            }
            first = first.or(Some(init));
            stores.extend(init.stores.iter().map(|a| &**a));
            if !init.calls_next {
                break;
            }
            from = i + 1;
        }
        if !stores.is_empty() && !self.attr_write_stores_value(class, &stores, &catalog_stores) {
            return Constructor::HandsToInit;
        }
        let changes_construction = |d: &ClassDecl| {
            !d.decorators.is_empty()
                || d.transform.is_some()
                || d.members.contains("__init_subclass__")
        };
        let containers = match first {
            Some(first)
                if all_plain && !decls.iter().flatten().any(|d| changes_construction(d)) =>
            {
                &*first.containers
            }
            _ => &[],
        };
        Constructor::KeepsNothing { containers }
    }

    /// The project classes whose bodies may supply an attribute of a value of the class `class`
    /// under assumption R2 (a value typed `class` is an instance of it or of a project
    /// subclass): the project classes along its MRO, every project subclass, and the project
    /// classes along the MROs of a subclass's other bases (a mixin it also derives from), each
    /// once. For a builtin container or a container ABC, those of every class of its family a
    /// value may be ([`Self::implementers`]): a `dict` may be an instance of a project subclass
    /// of `OrderedDict`, a `Mapping` one of a project subclass of `UserDict`.
    pub fn value_classes(&self, class: &str) -> Vec<String> {
        let class = self.canonical(class);
        match ContainerFamily::level_of(&class) {
            Some((kind, level)) => self.implementers(kind, level).classes.clone(),
            None => self.own_value_classes(&class),
        }
    }

    /// [`Self::value_classes`] of the canonical `class` by its own hierarchy.
    fn own_value_classes(&self, class: &str) -> Vec<String> {
        let class = class.to_owned();
        let mut seen = FxHashSet::default();
        let mut out = Vec::new();
        let mut add = |c: String, seen: &mut FxHashSet<String>| {
            if self.is_project_class(&c) && seen.insert(c.clone()) {
                out.push(c);
            }
        };
        for c in self.mro(&class) {
            add(c, &mut seen);
        }
        let subclasses = self.subclasses_of(&class);
        for sub in &subclasses {
            add(sub.clone(), &mut seen);
        }
        // A base already seen needs no walk: the MRO of each class along an MRO is part of it,
        // and a subclass's own bases are walked as its turn comes. So each class's MRO is
        // linearized once, whatever the depth of the hierarchy below `class`.
        for sub in &subclasses {
            self.read(|k| note(&mut k.class_facts, sub));
            let bases = self.class(sub).map(|d| d.bases.clone()).unwrap_or_default();
            for base in bases.iter().map(|b| self.canonical(b)) {
                if !seen.contains(&base) && self.is_project_class(&base) {
                    for c in self.mro(&base) {
                        add(c, &mut seen);
                    }
                }
            }
        }
        out
    }

    /// The project classes a value of the family `kind` at `level` may be an instance of under
    /// R2 ([`FamilyMembers`]): those whose declared bases reach, through project classes, a
    /// class of the family the level admits ([`FamilyLevel::admits`]: the builtin and the
    /// standard-library classes deriving from it, or also the ABC, the narrower ABCs, and the
    /// standard-library classes CPython registers with it), or registered with one of those
    /// classes or with a project class among them (`Mapping.register(C)`, `@Mapping.register`),
    /// with the project classes along their MROs; `open` when a `register` names a class heki
    /// cannot, or one of them binds `__subclasshook__`. A base outside the project and the table
    /// stands for itself.
    pub fn implementers(&self, kind: ContainerKind, level: FamilyLevel) -> &FamilyMembers {
        self.read(|k| note(&mut k.class_facts, family_key(kind, level)));
        self.families.get(&(kind, level)).unwrap_or(&NO_MEMBERS)
    }

    /// Whether `class` is a nominal container ABC ([`ContainerFamily::nominal_abc`]) whose
    /// family the project leaves closed: a value of it is an instance of a builtin of the
    /// family, of another class outside the project that implements it, or of a project
    /// implementer ([`Self::implementers`]).
    pub(crate) fn established_abc(&self, class: &str) -> bool {
        ContainerFamily::nominal_abc(class)
            .is_some_and(|(kind, level)| self.implementers(kind, level).open.is_none())
    }

    /// [`Self::implementers`] of every family and level.
    fn family_members(&self) -> FxHashMap<(ContainerKind, FamilyLevel), FamilyMembers> {
        // What each class is registered with: the receivers named by a class of the table or
        // of the project (a `register` of another object is no ABC's).
        let mut registered: FxHashMap<String, BTreeSet<String>> = FxHashMap::default();
        for m in self.modules.values() {
            for (receiver, classes) in &m.abc_registrations {
                let receiver = self.canonical(receiver);
                if ContainerFamily::of(&receiver).is_some() || self.is_project_class(&receiver) {
                    let classes = classes.iter().map(|c| {
                        if c == "*" {
                            c.clone()
                        } else {
                            self.canonical(c)
                        }
                    });
                    registered.entry(receiver).or_default().extend(classes);
                }
            }
        }
        let mut out = FxHashMap::default();
        for kind in [ContainerKind::List, ContainerKind::Dict, ContainerKind::Set] {
            for level in FamilyLevel::EVERY {
                let registrations = (level != FamilyLevel::Builtin).then_some(&registered);
                // A class of the table no project class derives from, and that nothing is
                // registered with, adds no member.
                let roots = ContainerFamily::within(kind, level).filter(|c| {
                    self.subclasses.contains_key(*c)
                        || registrations.is_some_and(|r| r.contains_key(*c))
                });
                let roots = roots.map(str::to_owned);
                out.insert((kind, level), self.members_from(roots, registrations));
            }
        }
        out
    }

    /// The project classes a value of one of `roots` may be ([`Self::own_value_classes`]),
    /// and, with `registered`, those of the classes registered with one of them or with one
    /// of those. One walk visits each class at most twice: as one whose values it lists (a root
    /// or a registered class) and as a member.
    fn members_from(
        &self,
        roots: impl Iterator<Item = String>,
        registered: Option<&FxHashMap<String, BTreeSet<String>>>,
    ) -> FamilyMembers {
        let budget = Budget::unmetered("heki_effect::resolver::ProjectDecls::members_from");
        let mut names: Vec<String> = roots.collect();
        let mut ids: FxHashMap<String, usize> = (names.iter().cloned())
            .enumerate()
            .map(|(i, c)| (c, i))
            .collect();
        let starts: Vec<(usize, bool)> = (0..names.len()).map(|i| (i, true)).collect();
        let mut open = None;
        let mut members: FxHashSet<usize> = FxHashSet::default();
        // A node is a class and whether its values are listed from it (a root or a registered
        // class) or it is a member of the list.
        let next = |(id, lists): (usize, bool), out: &mut Vec<(usize, bool)>| {
            let class = names[id].clone();
            let mut next: Vec<(String, bool)> = Vec::new();
            if lists {
                next.extend(
                    self.own_value_classes(&class)
                        .into_iter()
                        .map(|c| (c, false)),
                );
            }
            for r in registered.and_then(|r| r.get(&class)).into_iter().flatten() {
                if r == "*" {
                    open.get_or_insert_with(|| FamilyOpen::Register(class.clone()));
                } else {
                    next.push((r.clone(), true));
                }
            }
            for (c, lists) in next {
                let id = *ids.entry(c).or_insert_with_key(|c| {
                    names.push(c.clone());
                    names.len() - 1
                });
                out.push((id, lists));
            }
        };
        // Unmetered: the walk ends, visiting every class of the family.
        let _ = reach(&budget, starts, next, |(id, lists)| {
            if !lists {
                members.insert(id);
            }
            Visit::<()>::Expand
        });
        let mut classes: Vec<String> = members.into_iter().map(|id| names[id].clone()).collect();
        classes.sort();
        if registered.is_some() {
            open = open.or_else(|| {
                (classes.iter())
                    .find(|c| {
                        self.class(c)
                            .is_some_and(|d| d.members.contains("__subclasshook__"))
                    })
                    .map(|c| FamilyOpen::SubclassHook(c.clone()))
            });
        }
        FamilyMembers { classes, open }
    }

    /// What reading `attr` on a value of the class `class` may run besides finding it, over
    /// [`Self::value_classes`]: the one reader of the classes a read depends on, for the
    /// attribute loads that do not raise (R7), the plain keys of `OPT113`, and the builtin
    /// methods no project subclass replaces (`MUT110`, `MUT114`).
    pub fn attr_read(&self, class: &str, attr: &str) -> AttrRead {
        let class = self.canonical(class);
        let mut read = AttrRead {
            bases_known: true,
            ..AttrRead::default()
        };
        for c in self.mro(&class) {
            self.read(|k| note(&mut k.class_facts, &c));
            match self.class(&c) {
                Some(d) => read.declared |= d.members.contains(attr),
                None => read.bases_known &= PLAIN_BASES.contains(&c.as_str()),
            }
        }
        for c in self.value_classes(&class) {
            self.read(|k| note(&mut k.class_facts, &c));
            let Some(d) = self.class(&c) else { continue };
            read.custom_metaclass |= d.custom_metaclass;
            read.getattribute |= d.members.contains("__getattribute__");
            read.getattr |= d.members.contains("__getattr__");
            if d.members.contains(attr) {
                read.binders.push(c.clone());
            }
            if d.code_attrs.contains(attr) {
                read.code_binders.push(c);
            }
        }
        read
    }

    /// Whether reading `attr`, an attribute every object has (`__class__`, `__dict__`), on a
    /// value of the project class `class` runs no code but finding it, on every class the value
    /// may be under R2: none binds the name, defines `__getattribute__`, or has a metaclass heki
    /// does not know, and every base outside the project is a plain base or one
    /// `catalog_stores` accepts (a base whose `__setattr__` the catalog says stores on the
    /// instance, R3). Kept per class and name for the life of the indexes, so the reads of one
    /// class's namespace walk its subclasses once; the catalog is fixed while the indexes are.
    pub(crate) fn object_attr_read_runs_no_code(
        &self,
        class: &str,
        attr: &str,
        catalog_stores: impl Fn(&str) -> bool,
    ) -> bool {
        let class = self.canonical(class);
        self.object_attr_reads
            .ask(self, (class.clone(), attr.to_owned()), || {
                let read = self.attr_read(&class, attr);
                read.binders.is_empty()
                    && !read.runs_hook(false)
                    && !self.bound_after_body(&["__getattribute__"])
                    && (read.bases_known || self.binds_none_of(&class, &[], catalog_stores))
            })
    }

    /// Whether reading `attr` on an instance of the project class `class` (or, under assumption
    /// R2, of a project subclass) reads a plain attribute and runs no code of the project: a
    /// class along its MRO declares it (a field, a slot, or what its body or methods store), no
    /// class a value may be ([`Self::value_classes`]) binds it to a method, property, or other
    /// call in its body or defines `__getattr__` or `__getattribute__`, every base is a project
    /// class (or `object`, `NamedTuple`, `Generic`), and no metaclass heki does not know creates
    /// the class.
    /// Whether a store of `attr` on a value of the class `class` stores it in the instance as far
    /// as the bases go: every class along the MRO of every class the value may be
    /// ([`Self::value_classes`]) is a project class, a plain base, or a library base the
    /// catalog says keeps what its subclasses' code stores in the instance and binds no data
    /// descriptor of the name (`heki_kb::EcosystemEntry::plain_attributes_except`, R3:
    /// `django.db.models.Model`).
    pub fn bases_store_plainly(&self, class: &str, attr: &str, kb: &EffectiveEcosystemKb) -> bool {
        let plain = |c: &String| {
            self.is_project_class(c)
                || PLAIN_BASES.contains(&c.as_str())
                || kb.stores_plainly(c, attr)
        };
        self.value_classes(class)
            .iter()
            .all(|c| self.mro(c).iter().all(plain))
    }

    pub fn plain_attr(&self, class: &str, attr: &str) -> bool {
        let read = self.attr_read(class, attr);
        read.bases_known && read.declared && !read.runs_hook(true) && read.code_binders.is_empty()
    }

    /// Whether reading `attr` on a value of the class `class` runs no code and gives what the
    /// instance holds in it, or the class attribute: as [`Self::plain_attr`], except that a class
    /// body binding the name to an `object()` sentinel ([`Self::object_sentinel`]) binds no
    /// descriptor (`object` defines no `__get__`), and that a base outside the project may be
    /// one whose `__setattr__` the catalog describes as storing on the instance, which installs
    /// no descriptor for the attribute and defines no `__getattribute__` (R3:
    /// `pydantic.BaseModel`). The bases of every class the value may be count, a subclass's
    /// too.
    pub fn plain_sentinel_attr(&self, class: &str, attr: &str, kb: &EffectiveEcosystemKb) -> bool {
        let read = self.attr_read(class, attr);
        let known_base = |b: &str| {
            PLAIN_BASES.contains(&b)
                || kb
                    .lookup(&format!("{b}.__setattr__"))
                    .is_some_and(|e| e.stores_on_instance)
        };
        let bases_known = self
            .mro(class)
            .iter()
            .all(|c| self.is_project_class(c) || known_base(c))
            && self.value_classes(class).iter().all(|c| {
                self.class(c).is_some_and(|d| {
                    d.bases.iter().all(|b| {
                        let b = self.canonical(b);
                        self.is_project_class(&b) || known_base(&b)
                    })
                })
            });
        bases_known
            && read.declared
            && !read.runs_hook(true)
            && read.code_binders.iter().all(|c| {
                self.class(c)
                    .and_then(|d| d.named_attrs.get(attr)?.body.as_deref())
                    .and_then(|name| self.object_sentinel(name))
                    .is_some()
            })
    }

    /// The canonical fqn of the `object()` sentinel the module-level name `fqn` holds: through
    /// re-exports ([`Self::canonical_value`]), one of its module's
    /// [`ModuleDecls::object_sentinels`] that no other module rebinds or deletes.
    pub fn object_sentinel(&self, fqn: &str) -> Option<String> {
        let fqn = self.canonical_value(fqn)?;
        let (m, rest) = self.split_module(&fqn)?;
        (m.object_sentinels.contains(rest) && !self.rebound_elsewhere(&fqn)).then_some(fqn)
    }

    /// The `object()` sentinels the code of the project class `class` itself puts in the
    /// attributes of its instances ([`ClassDecl::named_attrs`]). Stored where an instance is
    /// known to hold it: the default of a field that the `__init__` a stdlib dataclass
    /// generates stores, a store `self.attr = S` in a method, each storing on the instance
    /// itself (no property, `__setattr__`, or metaclass: [`Self::attr_write_runs_code`]), and
    /// the default of a public field under a base the catalog describes as keeping hashable
    /// defaults (R3: `pydantic.BaseModel`) that validates no default ([`NamedAttr`] and
    /// `Self::may_validate_defaults`). Not stored where the class only annotates a field with the
    /// sentinel as its default under another base, which may or may not store it.
    pub fn held_sentinels(&self, class: &str, kb: &EffectiveEcosystemKb) -> Vec<HeldSentinel> {
        let mut out: Vec<HeldSentinel> = Vec::new();
        let decl = self.class(class);
        for (attr, named) in decl.iter().flat_map(|d| &d.named_attrs) {
            let plain_store = || !self.attr_write_runs_code(class, attr);
            for name in &named.held {
                if let Some(sentinel) = self.object_sentinel(name) {
                    out.push(HeldSentinel {
                        attr: attr.clone(),
                        sentinel,
                        stored: plain_store(),
                    });
                }
            }
            let default = named
                .body
                .as_deref()
                .filter(|_| named.annotated)
                .and_then(|name| self.object_sentinel(name));
            if let Some(sentinel) = default
                && !out
                    .iter()
                    .any(|h| h.attr == *attr && h.sentinel == sentinel)
                && decl.is_some_and(|d| self.may_store_defaults(class, d))
            {
                let kept = !attr.starts_with('_')
                    && !named.validates_default
                    && self.keeps_hashable_defaults(class, kb)
                    && !self.may_validate_defaults(class);
                out.push(HeldSentinel {
                    attr: attr.clone(),
                    sentinel,
                    stored: kept && plain_store(),
                });
            }
        }
        out
    }

    /// Whether code heki does not follow may store the defaults of the class's fields on its
    /// instances: an `__init__` of a stdlib dataclass's own (which the generated one would
    /// have stored), a class a project `dataclass_transform` builds, or a base outside the
    /// project other than `object`, `typing.Generic`, `abc.ABC`, and `NamedTuple`. A class of the
    /// project's own otherwise holds an annotated default only as its class attribute.
    fn may_store_defaults(&self, class: &str, decl: &ClassDecl) -> bool {
        self.read(|k| note(&mut k.class_facts, class));
        decl.dataclass.is_some() && self.own_method(class, "__init__").is_some()
            || self.built.contains(class)
            || self.mro(class).iter().any(|c| {
                !self.is_project_class(c)
                    && !PLAIN_BASES.contains(&c.as_str())
                    && !HOOKLESS_BASES.contains(&c.as_str())
            })
    }

    /// The project classes whose instances an object of the project class `class` may hold in
    /// its attributes, by the attribute types its classes declare (`ClassDecl::typed_attrs`: a
    /// class, the members of a union, the items of a generic container, the structural
    /// implementations of a protocol), each once with the first attribute that holds it. A class
    /// whose own copy is decided otherwise ([`Self::copy_decided_by_class`]) is left out: what a
    /// deep copy makes of it is not its attributes' copies. A walk below several levels of
    /// attributes reads this once per class it reaches.
    pub fn classes_in_attributes(&self, class: &str) -> Vec<(String, String)> {
        let mut seen = FxHashSet::from_iter([self.canonical(class)]);
        let mut out: Vec<(String, String)> = Vec::new();
        let declared = self.mro(class).into_iter().filter_map(|m| self.class(&m));
        for (attr, ty) in declared.flat_map(|d| d.typed_attrs()) {
            let mut found = Vec::new();
            ty_classes(ty, &mut found);
            let mut held: Vec<String> = Vec::new();
            for t in found.iter().map(|t| self.canonical(t)) {
                held.extend(
                    self.protocol_implementers(&t)
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
                held.push(t);
            }
            for h in held {
                if self.is_project_class(&h)
                    && seen.insert(h.clone())
                    && !self.copy_decided_by_class(&h, CopyDepth::Deep)
                {
                    out.push((h, attr.to_owned()));
                }
            }
        }
        out
    }

    /// Whether a project class along the MRO of `class` configures the model
    /// ([`ClassDecl::configures_model`]): a field validating its default holds what the
    /// validation returns.
    fn may_validate_defaults(&self, class: &str) -> bool {
        (self.mro(class).iter()).any(|c| self.class(c).is_some_and(ClassDecl::configures_model))
    }

    /// Whether a base of the project class `class` outside the project is one the catalog
    /// describes as building instances that hold a field's hashable default itself
    /// (`EcosystemEntry::keeps_hashable_defaults`).
    fn keeps_hashable_defaults(&self, class: &str, kb: &EffectiveEcosystemKb) -> bool {
        self.mro(class).into_iter().any(|c| {
            !self.is_project_class(&c) && kb.lookup(&c).is_some_and(|e| e.keeps_hashable_defaults)
        })
    }

    /// Whether something of the class `class` itself decides what a copy of `depth` makes of
    /// its instances, in place of copying their attributes: a copy hook along its MRO
    /// ([`copy_hooks`]), or a `copyreg` registration of the class ([`Self::copy_registered`]).
    /// Either may keep any value.
    pub fn copy_decided_by_class(&self, class: &str, depth: CopyDepth) -> bool {
        let class = self.canonical(class);
        let hooks = copy_hooks(depth);
        self.mro(&class).iter().any(|c| {
            self.read(|k| note(&mut k.class_facts, c));
            self.class(c)
                .is_some_and(|d| hooks.iter().any(|h| d.members.contains(*h)))
        }) || self.copy_registered(&class)
    }

    /// Whether a project module registers the class `class` with `copyreg`
    /// (`ModuleDecls::copy_registrations`), or a class heki cannot name: `copy.deepcopy` then
    /// copies an instance whose exact class it is by the registered function.
    pub fn copy_registered(&self, class: &str) -> bool {
        self.read(|k| {
            note(&mut k.class_facts, class);
            note(&mut k.class_facts, "*");
        });
        self.copy_registered.contains(class) || self.copy_registered.contains("*")
    }

    /// What `copy.deepcopy` (`depth` deep, without a `memo`) or `copy.copy` (shallow) does with
    /// a value of the project class `class`, an instance of it or, under R2, of a project
    /// subclass ([`CopyMade`]). The copy protocol looks for `__deepcopy__` on the instance (where
    /// `__getattr__` and `__getattribute__` may answer; `copy.copy` looks for `__copy__` on the
    /// class), then for a `copyreg` registration of its exact class, then for `__reduce_ex__` /
    /// `__reduce__`, whose default reads `__getstate__` and makes the copy take the state
    /// through `__setstate__` when the class defines one; with none of them, the copy is a new
    /// instance whose attributes are the original's values, or deep copies of them.
    pub fn copy_of(&self, class: &str, depth: CopyDepth, kb: &EffectiveEcosystemKb) -> CopyMade {
        let class = self.canonical(class);
        let hooks = copy_hooks(depth);
        let hooked = |d: &ClassDecl| hooks.iter().any(|h| d.members.contains(*h));
        // A base outside the project: one known to define none of the hooks, or one whose
        // copy hook the catalog describes as copying the attributes to this depth (R3).
        let known_base = |c: &str| {
            HOOKLESS_BASES.contains(&c)
                || kb
                    .lookup(&format!("{c}.{}", hooks[0]))
                    .is_some_and(|e| e.copy_sanitizer == Some(depth))
        };
        if self.copy_decided_by_class(&class, depth) {
            return CopyMade::Hooked;
        }
        let mro = self.mro(&class);
        let mut answer = CopyMade::Attributes;
        for c in &mro {
            if !self.is_project_class(c) && !known_base(c) {
                answer = CopyMade::Open;
            }
        }
        for c in self.value_classes(&class) {
            self.read(|k| note(&mut k.class_facts, &c));
            let Some(d) = self.class(&c) else { continue };
            let lookup_hooks = ["__getattr__", "__getattribute__"];
            let unknown_base = d.bases.iter().any(|b| {
                let b = self.canonical(b);
                !self.is_project_class(&b) && !known_base(&b)
            });
            if hooked(d)
                || lookup_hooks.iter().any(|h| d.members.contains(*h))
                || d.custom_metaclass
                || unknown_base
                || c != class && self.copy_registered(&c)
            {
                answer = CopyMade::Open;
            }
        }
        answer
    }

    /// Whether reading the method `attr` on an instance of the project class `class` (or, under
    /// assumption R2, of a project subclass) finds a method and runs no code of the project, so
    /// the load cannot raise: its MRO finds a project method that no decorator replaces
    /// (`FunctionDecl::called_as_defined`; a property's getter runs on the read), every class a
    /// value may be that binds the name to code binds it to such a method too, and none defines
    /// `__getattribute__` or has a metaclass other than `type`.
    pub fn method_read_runs_no_code(&self, class: &str, attr: &str) -> bool {
        let plain_method = |key: Option<FuncKey>| {
            key.and_then(|k| self.function_decl(&k))
                .is_some_and(|d| d.called_as_defined)
        };
        if !plain_method(self.mro_method(class, attr)) {
            return false;
        }
        let read = self.attr_read(class, attr);
        !read.runs_hook(false)
            && read
                .code_binders
                .iter()
                .all(|c| plain_method(self.own_method(c, attr)))
    }

    /// The class whose `__hash__` the instances of the project class `class` use: the first
    /// class along its MRO whose body defines `__hash__` as a method, or binds `__eq__` without
    /// binding `__hash__` (Python then sets its `__hash__` to `None`). `None` when no project
    /// class before an external base does (heki does not know what an external class defines),
    /// or when a class binds `__hash__` otherwise (`__hash__ = Base.__hash__`).
    pub fn hash_owner(&self, class: &str) -> Option<String> {
        self.mro(class)
            .into_iter()
            .find_map(|c| {
                self.read(|k| note(&mut k.class_facts, &c));
                let binds =
                    |d: &ClassDecl| (d.members.contains("__hash__"), d.members.contains("__eq__"));
                let defines = || {
                    self.methods_of(&c)
                        .iter()
                        .any(|k| k.qualname.rsplit('.').next() == Some("__hash__"))
                };
                match self.class(&c).map(binds) {
                    None => Some(None),
                    Some((true, _)) => Some(defines().then_some(c)),
                    Some((false, true)) => Some(Some(c)),
                    Some((false, false)) => None,
                }
            })
            .flatten()
    }

    /// Whether `method` of `class` (a builtin container, say) runs on every instance of `class`
    /// the project can make: under R2 a value of type `class` may be an instance of a project
    /// subclass. No class a value may be ([`Self::value_classes`]) binds `method`, has a
    /// metaclass other than `type` (which may install it), or, when the method is looked up by
    /// name (`xs.sort()`, not the operator or item store that calls it implicitly, which skips
    /// it), defines `__getattribute__`.
    pub fn method_not_overridden(&self, class: &str, method: &str, by_name: bool) -> bool {
        let read = self.attr_read(class, method);
        read.binders.is_empty() && !read.custom_metaclass && !(by_name && read.getattribute)
    }

    /// Where the special method `name` that Python looks up on the class of an instance of
    /// `class` comes from, over every class the instance may be under R2 ([`Self::value_classes`]),
    /// the one that may run the most. A class of the project defines it when it binds the name
    /// or has a metaclass heki does not know (which may add it); past them, a builtin type has
    /// the interpreter's own, `object` or a plain base `object`'s (a `NamedTuple` a tuple's),
    /// and any other class outside the project may define it, as heki does not read it. Every
    /// base outside the project of a class the instance may be counts, wherever its MRO puts it.
    /// An instance of a nominal container ABC that is no project implementer is taken to run what
    /// the family's builtins run (a `Mapping` a `dict`'s), as R2 takes a `dict` that is a
    /// `defaultdict`; whether the family is closed is the caller's question
    /// ([`Self::established_abc`]).
    pub(crate) fn special_method_owner(&self, class: &str, name: &str) -> MethodOwner {
        let outside = |c: &str| {
            if matches!(c, "object" | "builtins.object")
                || PLAIN_BASES.contains(&c) && !c.ends_with(".NamedTuple")
            {
                MethodOwner::Object
            } else if c.starts_with("builtins.") || PLAIN_BASES.contains(&c) {
                MethodOwner::Builtin
            } else {
                MethodOwner::Library(c.to_owned())
            }
        };
        let class = self.canonical(class);
        let mut owner = if self.is_project_class(&class) {
            MethodOwner::Object
        } else if ContainerFamily::nominal_abc(&class).is_some() {
            MethodOwner::Builtin
        } else {
            outside(&class)
        };
        for c in self.value_classes(&class) {
            self.read(|k| note(&mut k.class_facts, &c));
            let Some(d) = self.class(&c) else { continue };
            if d.custom_metaclass || d.members.contains(name) {
                return MethodOwner::Project;
            }
            for base in d.bases.iter().map(|b| self.canonical(b)) {
                if !self.is_project_class(&base) {
                    owner = owner.max(outside(&base));
                }
            }
        }
        owner
    }

    /// Whether no class a value of the class `class` may be (R2) binds one of the special
    /// methods `names` in the project or through a metaclass heki does not know.
    pub fn special_methods_not_in_project(&self, class: &str, names: &[&str]) -> bool {
        names
            .iter()
            .all(|n| self.special_method_owner(class, n) != MethodOwner::Project)
    }

    /// Whether a property getter of the project (`@property`, a cached property) is named
    /// `attr`: a cached property keeps its value in the instance `__dict__` under its getter's
    /// name, so no other name holds a value a getter of the project computed.
    pub fn names_property(&self, attr: &str) -> bool {
        self.read(|k| note(&mut k.property_names, attr));
        self.property_names.contains(attr)
    }

    /// Whether a class a value of the class `class` may be (R2, [`Self::value_classes`]) binds
    /// `attr` to a property getter, so that an instance may keep a value under it.
    pub(crate) fn binds_property(&self, class: &str, attr: &str) -> bool {
        self.value_classes(class).iter().any(|c| {
            self.own_method(c, attr)
                .and_then(|k| self.function_decl(&k))
                .is_some_and(FunctionDecl::reads_as_property)
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

    /// Whether a key of the namespace of an instance of the project class `source`, or a field
    /// `source` declares, names an attribute an instance of the project class `class` may have
    /// already: `source` is `class` or derives from it, so its instances hold only attributes
    /// of the family of `class`, and a copy of such a namespace or of its fields adds none.
    pub(crate) fn keys_exist_in(&self, source: &str, class: &str) -> bool {
        self.derives_from(source, class)
    }

    /// Whether every instance of `class` is an instance of `base`: `class` is `base` or a project
    /// class deriving from it.
    pub fn derives_from(&self, class: &str, base: &str) -> bool {
        let (class, base) = (self.canonical(class), self.canonical(base));
        class == base || self.is_subclass(&class, &base)
    }

    /// The method resolution order of `class`: Python's C3 linearization of its project bases;
    /// an external base stands for itself (its bases are not known). A hierarchy C3 cannot
    /// linearize (Python rejects the class statement, or a cycle) keeps the depth-first,
    /// left-to-right order.
    fn mro(&self, class: &str) -> Vec<String> {
        self.mro_with(class, Chase::Reexports)
    }

    /// [`Self::mro`] with the bases a type checker reads ([`Chase::Bases`]): a base another
    /// module binds only under `if TYPE_CHECKING:` is the class that import names. What a
    /// method call finds along it types the call and is a target of it, never every target: at
    /// run time such a base may be another class ([`Self::lookup_rebound`]).
    fn declared_mro(&self, class: &str) -> Vec<String> {
        self.mro_with(class, Chase::Bases)
    }

    /// [`Self::mro`] with the bases chased as `how` says.
    fn mro_with(&self, class: &str, how: Chase) -> Vec<String> {
        let class = self.canonical(class);
        // The depth-first walk reads the declarations the linearization depends on.
        let (order, multiple) = self.depth_first_bases(&class, how);
        // Without multiple inheritance, the depth-first order is the linearization.
        if !multiple {
            return order;
        }
        let key = (how, class);
        let known = self
            .linearized
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned();
        if let Some(mro) = known {
            return mro;
        }
        let mut done = FxHashMap::default();
        let mro = (self.c3(&key.1, how, &mut done, &mut Vec::new())).unwrap_or(order);
        self.linearized
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, mro.clone());
        mro
    }

    /// [`Self::mro`] by C3: `class` followed by the merge of its bases' linearizations and the
    /// list of its bases. `done` holds the linearizations computed so far, `open` the classes
    /// being linearized (a cycle has none).
    fn c3(
        &self,
        class: &str,
        how: Chase,
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
            .map(|d| d.bases.iter().map(|b| self.chase(b, how)).collect())
            .unwrap_or_default();
        open.push(class.to_owned());
        let mut seqs: Vec<std::collections::VecDeque<String>> = Vec::new();
        for base in &bases {
            seqs.push(self.c3(base, how, done, open)?.into());
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
    fn depth_first_bases(&self, class: &str, how: Chase) -> (Vec<String>, bool) {
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
                    stack.push(self.chase(base, how));
                }
            }
            out.push(c);
        }
        (out, multiple)
    }

    /// Whether calling the class `class` may return an existing instance, or one of another
    /// class, instead of a new one of it: a project class along its MRO binds `__new__`, in its
    /// body or after it ([`Self::lookup_rebound`]; [`Self::bound_after_body`], also a class
    /// decorator's or an `__init_subclass__`'s store on the class it is handed), or it is an
    /// enumeration (`Color("red")` returns a member).
    pub fn may_reuse_instances(&self, class: &str) -> bool {
        const ENUMS: &[&str] = &[
            "enum.Enum",
            "enum.IntEnum",
            "enum.StrEnum",
            "enum.Flag",
            "enum.IntFlag",
            "enum.ReprEnum",
        ];
        let mro = self.mro(class);
        mro.iter().any(|c| {
            self.read(|k| note(&mut k.class_facts, c));
            ENUMS.contains(&c.as_str())
                || self.own_method(c, "__new__").is_some()
                || (self.class(c)).is_some_and(|decl| decl.members.contains("__new__"))
        }) || self.lookup_rebound(&mro, "__new__")
            || self.bound_after_body(&["__new__"])
    }

    /// The MRO of `class` as a [`crate::ClassLine`], one copy shared by every site and fact
    /// naming the class. `resolved`: the caller has just resolved a method on `class`, which
    /// read every declaration its MRO depends on, so a line computed before serves; otherwise
    /// the MRO is computed again, which reads them.
    pub(crate) fn lineage(&self, class: &str, resolved: bool) -> crate::SharedLine {
        if resolved
            && let Some(line) = self
                .lines
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .get(class)
        {
            return Arc::clone(line);
        }
        let mro = self.mro(class);
        let known = self
            .lines
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&mro[0])
            .filter(|line| line.mro == mro)
            .cloned();
        if let Some(line) = known {
            return line;
        }
        let line = Arc::new(crate::ClassLine { mro });
        self.lines
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(line.mro[0].clone(), Arc::clone(&line));
        line
    }

    /// The method `name` defined directly in project class `class`.
    fn own_method(&self, class: &str, name: &str) -> Option<FuncKey> {
        let (m, rest) = self.split_module(class)?;
        let member = format!("{rest}.{name}");
        let qualname = match m.definition_path(&member) {
            Cow::Owned(written) => written,
            Cow::Borrowed(_) => member,
        };
        m.functions.contains_key(&qualname).then(|| FuncKey {
            module: m.module_name.clone(),
            qualname,
        })
    }

    pub fn function_decl(&self, key: &FuncKey) -> Option<&FunctionDecl> {
        self.module(&key.module)?
            .functions
            .get(&key.qualname)
            .map(Box::as_ref)
    }

    /// What the attribute `attr` of the instance `obj` holds when the class annotates it with a
    /// type parameter (`obj: T` in `class Site(Generic[T])`) and the instance's annotation gives
    /// the argument at its position (`Site[Schema]`, [`Args::TypeArgs`]).
    pub(crate) fn type_argument(&self, obj: &crate::types::Obj, attr: &str) -> Option<Ty> {
        let Args::TypeArgs(args) = &obj.args else {
            return None;
        };
        let decl = self.class(&obj.class)?;
        let (_, at) = decl.param_attrs.iter().find(|(a, _)| &**a == attr)?;
        match args.get(usize::from(*at))? {
            Elem::Of(t) => Some(t.clone()),
            Elem::Empty | Elem::Unknown => None,
        }
    }

    /// The declared type of reading `attr` on an instance of `class`, from the first class along
    /// the MRO (past `class` itself with `skip_self`, for `super()`) that declares it: the
    /// return annotation of a property getter (with or without a setter; another function of a
    /// name some project property has gives none), or the Priority 3 attribute type. Overriding
    /// subclasses do not count for an annotation, as for a type checker; an inferred type joins
    /// what the project's code stores into the instances of every class whose code may store
    /// into the instance ([`Self::stored_attr_type`]). A bare container annotation (`list`)
    /// gives its class only: with the items of that join where `__init__` or `__post_init__`
    /// establishes the attribute and the join is an instance of the class. The getter's module
    /// is a declaration read like any other (`DeclKeys::modules`).
    pub(crate) fn member_type(&self, class: &str, attr: &str, skip_self: bool) -> Option<Ty> {
        self.read(|k| note(&mut k.property_names, attr));
        let property = self.property_names.contains(attr);
        for c in self.mro(class).iter().skip(usize::from(skip_self)) {
            if property && let Some(key) = self.own_method(c, attr) {
                return self.function_decl(&key)?.property_type.clone();
            }
            let Some(decl) = self.class(c) else {
                continue;
            };
            let established = || {
                matches!(
                    self.attr_store(c, decl, attr),
                    Some(AttrStore::Established(_))
                )
            };
            if let Some(t) = decl.attr_types.get(attr) {
                if !t.is_bare_container() {
                    return Some(t.clone());
                }
                let stored = established().then(|| self.stored_attr_type(class, attr));
                return Some(t.holding_stored(stored.flatten()));
            }
            if established() {
                return self.stored_attr_type(class, attr);
            }
        }
        None
    }

    /// The join of what the project's code stores in `attr` of the instances of the classes
    /// whose code may store into an instance of `class` ([`Self::instance_family`],
    /// [`Self::join_family`]). The answer is kept for the life of the indexes, as
    /// [`Self::class_str_constant`]'s.
    fn stored_attr_type(&self, class: &str, attr: &str) -> Option<Ty> {
        let class = self.canonical(class);
        let key = (class.clone(), attr.to_owned());
        self.kept(&self.attr_joins, key, || {
            let family = self.instance_family(&class);
            self.join_family(&family, attr)
        })
    }

    /// The classes whose code may store into an instance of the project class `class`:
    /// `class`, its project subclasses, and the project classes along their MROs, sorted. One
    /// walk visits each class at most twice, as an instance's class and as a base, however many
    /// subclasses share its bases.
    pub(crate) fn instance_family(&self, class: &str) -> Vec<String> {
        let budget = Budget::unmetered("heki_effect::resolver::ProjectDecls::instance_family");
        let mut names = vec![self.canonical(class)];
        let mut ids: FxHashMap<String, usize> = FxHashMap::default();
        ids.insert(names[0].clone(), 0);
        let mut found: FxHashSet<usize> = FxHashSet::default();
        // A node is a class and whether an instance may be of it (`class` and its subclasses,
        // whose own subclasses may be too) or it is only a base of one.
        let next = |(id, instance): (usize, bool), out: &mut Vec<(usize, bool)>| {
            let class = names[id].as_str();
            let mut next: Vec<(String, bool)> = Vec::new();
            if instance {
                let subclasses = self.direct_subclasses(class).iter();
                next.extend(subclasses.map(|c| (c.clone(), true)));
            }
            if let Some(decl) = self.class(class) {
                next.extend(decl.bases.iter().map(|b| (self.canonical(b), false)));
            }
            for (c, instance) in next {
                let id = *ids.entry(c).or_insert_with_key(|c| {
                    names.push(c.clone());
                    names.len() - 1
                });
                out.push((id, instance));
            }
        };
        // Unmetered: the walk ends, visiting every class of the family.
        let _ = reach(&budget, [(0, true)], next, |(id, _)| {
            found.insert(id);
            Visit::<()>::Expand
        });
        let mut family: Vec<String> = found
            .into_iter()
            .map(|id| names[id].clone())
            .filter(|c| self.is_project_class(c))
            .collect();
        family.sort_unstable();
        family
    }

    /// The declared value of the property `attr` when the definition of `attr` that an instance
    /// of `class` finds first along the MRO (past `class` itself with `skip_self`) is a
    /// property's getter, with or without a setter: its return annotation, `Some(None)` without
    /// one. Overriding subclasses do not count, as for a type checker.
    pub(crate) fn declared_property(
        &self,
        class: &str,
        attr: &str,
        skip_self: bool,
    ) -> Option<Option<&Ty>> {
        self.read(|k| note(&mut k.property_names, attr));
        if !self.property_names.contains(attr) {
            return None;
        }
        let decl = self.function_decl(&self.mro_definition(class, attr, skip_self)?)?;
        decl.reads_as_property()
            .then_some(decl.property_type.as_ref())
    }

    /// Whether `class.__new__` (found past `class` itself with `skip_self`, as for `super()`)
    /// is `object.__new__`: no class of the MRO defines `__new__`, and no base outside the
    /// project but `object` may ([`Self::inherited_external`]).
    pub(crate) fn new_is_object_new(
        &self,
        class: &str,
        skip_self: bool,
        kb: &EffectiveEcosystemKb,
    ) -> bool {
        let mro = self.mro(class);
        self.definitions_along(&mro, class, "__new__", skip_self, false, |_| true)
            .is_empty()
            && matches!(
                self.inherited_external(&mro, "__new__", skip_self, kb),
                Inherited::External(symbol) if symbol == "builtins.object.__new__"
            )
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
    /// of a project module that is no class and names no type (a constant, an unbounded
    /// `TypeVar`); an item of such a type is unknown. A project name that names a type (a type
    /// alias, a bound `TypeVar`: [`ModuleDecls::type_names`]) is that type, also as a member of a
    /// union, and inside `type[...]` ([`Self::class_named`]). A name below a module heki does not
    /// see (a compiled submodule of a project package) stays as it is. A class object (`type[C]`)
    /// is [`Self::class_object`].
    pub(crate) fn declared_ty(&self, ty: &Ty) -> Option<Ty> {
        self.declared_ty_with(ty, true)
    }

    /// [`Self::declared_ty`]; with `names`, a project name an import names is the type it names
    /// ([`ModuleDecls::type_names`]), whose own names are taken as they are.
    fn declared_ty_with(&self, ty: &Ty, names: bool) -> Option<Ty> {
        if let Ty::Class(class) = ty {
            return self.class_object(&self.class_named(class, names)?);
        }
        let named = |o: &crate::types::Obj| self.type_name(&o.class).filter(|_| names);
        let canonical = |o: &crate::types::Obj| {
            let class = self.declared_class(&o.class);
            if let Some((m, rest)) = self.split_module(&class)
                && !rest.contains('.')
                && !m.classes.contains_key(rest)
            {
                return None;
            }
            let args = match &o.args {
                // A result of a type an import names that may also be `None`.
                Args::Returns(r) if r.obj().is_some_and(|r| self.nullable_type_name(&r.class)) => {
                    Args::None
                }
                // Arguments of a class no attribute of which a type parameter types.
                Args::TypeArgs(_)
                    if self
                        .class(&class)
                        .is_none_or(|decl| decl.param_attrs.is_empty()) =>
                {
                    Args::None
                }
                args => args.map_elems(|t| self.declared_ty_with(t, names)),
            };
            Some(crate::types::Obj { class, args })
        };
        let members = ty.members();
        if let [o] = members
            && let Some(t) = named(o)
        {
            return self.declared_ty_with(t, false);
        }
        if !members.iter().any(|o| named(o).is_some()) {
            return ty.map_members(canonical);
        }
        // A union naming a type among its members (`Operation | Extra`): the instances each member
        // is.
        let mut objs = Vec::new();
        for o in members {
            match named(o) {
                Some(t) => {
                    let t = self.declared_ty_with(t, false)?;
                    if t.members().is_empty() {
                        return None;
                    }
                    objs.extend(t.members().iter().cloned());
                }
                None => objs.push(canonical(o)?),
            }
        }
        Ty::of_declared(objs)
    }

    /// The type the project name `fqn` names ([`ModuleDecls::type_names`]): that of the first
    /// name along its re-exports and module aliases ([`Self::object_named`]) that names one. An
    /// alias that is also a module alias of a class (`Names = frozenset[str]` binds the class
    /// `frozenset`) names the type with its arguments.
    fn type_name(&self, fqn: &str) -> Option<&Ty> {
        let named = self.named_through(fqn, Chase::Types);
        let (m, rest) = self.split_module(&named)?;
        m.type_names.get(rest)
    }

    /// Whether the type the project name `fqn` names ([`Self::type_name`]) may also be `None`
    /// ([`ModuleDecls::nullable_type_names`]).
    fn nullable_type_name(&self, fqn: &str) -> bool {
        let named = self.named_through(fqn, Chase::Types);
        self.split_module(&named)
            .is_some_and(|(m, rest)| m.nullable_type_names.contains(rest))
    }

    /// The canonical class of the class object type `type[C]` ([`Ty::Class`]) for the name `C`
    /// (`class`): the class `C` names through re-exports and module aliases, or with `names`, the
    /// one class of the type `C` names ([`Self::type_name`]: `type Kind = Model`, a type variable
    /// bound to `Model`); `None` when that type is no instance of one class (a union).
    fn class_named(&self, class: &str, names: bool) -> Option<String> {
        match self.type_name(class).filter(|_| names) {
            Some(t) => Some(self.declared_class(&t.obj()?.class)),
            None => Some(self.declared_class(class)),
        }
    }

    /// The type of a class object that is the canonical class `class` or one of its subclasses
    /// ([`Ty::Class`]), as `cls` in a class method is: unknown for a protocol, whose
    /// implementations need not be its subclasses, and for a class outside the project.
    pub(crate) fn class_object(&self, class: &str) -> Option<Ty> {
        (self.is_project_class(class) && self.protocol(class).is_none())
            .then(|| Ty::Class(class.to_owned()))
    }

    /// The project protocol a class object type names (`type[P]` of an annotation), whose
    /// class [`Self::class_object`] leaves unknown: the contracts still judge an access through
    /// it by the protocol (`allowed-protocols`).
    pub(crate) fn protocol_class(&self, ty: &Ty) -> Option<String> {
        let Ty::Class(class) = ty else {
            return None;
        };
        let class = self.class_named(class, true)?;
        self.protocol(&class).is_some().then_some(class)
    }

    /// Whether the inferred type `t` is narrower than the declared `declared`, so that a value
    /// keeps it: anything for a declaration of `Any` / `object`, an instance of a class that
    /// specializes the declared one ([`Self::specializes`]: `x: Base = Sub()`, `cur:
    /// sqlite3.Cursor = conn.execute(...)` on a file) or of the declared class with known
    /// fields ([`Args::Fields`]: `spec: Spec = Spec(run=f)`), and for a declared class object, a
    /// class named exactly or the class object of a subclass (`kind: type[Base] = Base`, `=
    /// Sub`, `= type(sub)`).
    pub(crate) fn narrower(&self, t: &Ty, declared: &Ty, kb: &EffectiveEcosystemKb) -> bool {
        if let Ty::Class(d) = declared {
            return match t {
                Ty::Ref(c) => c == d || self.is_subclass(c, d),
                Ty::Class(c) => c != d && self.is_subclass(c, d),
                _ => false,
            };
        }
        declared.obj().is_some_and(|d| {
            of_any_class(&d.class)
                || (is_any_callable(&d.class) && t.obj().is_none())
                || t.obj().is_some_and(|i| {
                    self.specializes(&i.class, &d.class, kb)
                        // An instance of the class whose fields hold known objects.
                        || i.class == d.class
                            && matches!(i.args, Args::Fields(_))
                            && matches!(d.args, Args::None)
                })
        })
    }

    /// Whether every instance of `class` is an instance of `base` that says more than `base`
    /// does: `class` is a project subclass of it, or a catalog class with the methods of the
    /// library class `base` ([`EffectiveEcosystemKb::api_class`]: `sqlite3.connect(file)`, a
    /// `sqlite3.Connection` on a file).
    pub(crate) fn specializes(&self, class: &str, base: &str, kb: &EffectiveEcosystemKb) -> bool {
        class != base && (self.is_subclass(class, base) || kb.api_class(class) == Some(base))
    }

    /// The declared type `declared` with what the inferred `t` says beyond it: `t` when it is
    /// narrower ([`Self::narrower`]); for an instance of the declared container class, each
    /// item slot of `t` that is narrower than the declared one or that the declaration leaves
    /// unknown (`dict[str, Callable]` declared, a display of functions stored); else
    /// `declared`.
    pub(crate) fn refine(&self, t: &Ty, declared: &Ty, kb: &EffectiveEcosystemKb) -> Ty {
        if self.narrower(t, declared, kb) {
            return t.clone();
        }
        let (Some(i), Some(d)) = (t.obj(), declared.obj()) else {
            return declared.clone();
        };
        if i.class != d.class {
            return declared.clone();
        }
        let slot = |a: &Elem, b: &Elem| match (a, b) {
            (Elem::Of(x), Elem::Of(y)) if self.narrower(x, y, kb) => a.clone(),
            (Elem::Of(_), Elem::Unknown) => a.clone(),
            _ => b.clone(),
        };
        let args = match (&i.args, &d.args) {
            (Args::Items(a), Args::Items(b)) => Args::Items(Box::new(slot(a, b))),
            // A display of known length declared as a tuple of any length.
            (Args::Tuple(_), Args::Items(b)) => Args::Items(Box::new(slot(&i.args.item(), b))),
            (Args::Map(ka, va), Args::Map(kb, vb)) => {
                Args::Map(Box::new(slot(ka, kb)), Box::new(slot(va, vb)))
            }
            (a, Args::None) => a.clone(),
            (_, b) => b.clone(),
        };
        Ty::container(&d.class, args)
    }

    /// The project functions `name` names on an instance of the canonical project class
    /// `class`: the first definition along the MRO (past `class` itself with `skip_self`, for
    /// `super()`) and, with `overrides`, the definitions of every overriding project subclass
    /// (Priority 4 CHA), each one kept when `keep` accepts it (sorted, deduplicated).
    fn definitions(
        &self,
        class: &str,
        name: &str,
        skip_self: bool,
        overrides: bool,
        keep: impl Fn(&FuncKey) -> bool,
    ) -> Vec<FuncKey> {
        self.definitions_along(&self.mro(class), class, name, skip_self, overrides, keep)
    }

    /// [`Self::definitions`] with the MRO of `class` given.
    fn definitions_along(
        &self,
        mro: &[String],
        class: &str,
        name: &str,
        skip_self: bool,
        overrides: bool,
        keep: impl Fn(&FuncKey) -> bool,
    ) -> Vec<FuncKey> {
        let start = usize::from(skip_self);
        let mut found = Vec::new();
        if let Some(k) = mro
            .iter()
            .skip(start)
            .find_map(|c| self.own_method(c, name))
            && keep(&k)
        {
            found.push(k);
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
        found
    }

    /// Where a project class with the MRO `mro` takes `name` from, past the class itself with
    /// `skip_self`, when no project class of the MRO defines it: the first base outside the
    /// project that heki knows defines it ([`external_defines`]), else a base heki knows
    /// nothing of may, else none does. Every class takes `__new__` from `object` when no base
    /// outside the project but builtin types may define it.
    fn inherited_external(
        &self,
        mro: &[String],
        name: &str,
        skip_self: bool,
        kb: &EffectiveEcosystemKb,
    ) -> Inherited {
        for c in mro.iter().skip(usize::from(skip_self)) {
            if self.is_project_class(c) {
                continue;
            }
            // An unknown base names no symbol, special methods included.
            if !is_unknown_base(c) && external_defines(kb, c, name) {
                return Inherited::External(format!("{c}.{name}"));
            }
            // A base heki knows nothing of may define it, before any later base that does.
            if !c.starts_with("builtins.") {
                return Inherited::Unknown;
            }
        }
        match name {
            "__new__" => Inherited::External("builtins.object.__new__".to_owned()),
            _ => Inherited::Absent,
        }
    }

    /// The class the catalog gives reading `attr` on an instance of `class`
    /// (`heki_kb::AttributeType`, R3): the row of the class outside the project that each class
    /// the value may be under R2 (`class` and every project subclass) finds first along its MRO,
    /// the same row for all of them. `None` when a project class on the way binds the name (its
    /// own declaration types the read, or nothing does), a base heki knows nothing of or a
    /// library class without the row comes first (it may define the name), a class along the
    /// MRO of one the value may be defines `__getattribute__` or has a metaclass heki does not
    /// know, or there are more than [`DISPATCH_CLASS_LIMIT`] classes.
    ///
    /// The answer is kept per class and name for the life of the indexes, so the reads of a
    /// catalogued name on instances of one class walk its subclasses once; the catalog is the
    /// project's, fixed while the indexes are (a change of the configuration loads the project
    /// again).
    pub(crate) fn catalog_attribute(
        &self,
        class: &str,
        attr: &str,
        kb: &EffectiveEcosystemKb,
    ) -> Option<String> {
        // No row of the name: the read depends on no declaration.
        if !kb.types_attribute_named(attr) {
            return None;
        }
        let class = self.canonical(class);
        self.catalog_attributes
            .ask(self, (class.clone(), attr.to_owned()), || {
                self.find_catalog_attribute(&class, attr, kb)
                    .map(str::to_owned)
            })
    }

    /// The class of what reading `attr` on the class object of the project class `class` gives
    /// (with `subclasses`, on one of its project subclasses too: `cls`, `type(self)`), from the
    /// `[[class_attributes]]` rows of the first base outside the project along each class's MRO
    /// (`heki_kb::ClassAttribute`, R3): where the first class along the MRO binding `attr` binds
    /// it in its body to a call of a class deriving from a row's `type`
    /// (`objects = StreamManager()`), that class; where none binds it, the `type` of the row of
    /// `attr`, unless a class body along the MRO may bind an instance of a row's `type` under
    /// another name ([`Self::may_bind_instance_of`]: the base then gives no such attribute).
    /// `None` when a class on the way binds the name otherwise or stores it outside its body,
    /// in its module or another ([`Self::lookup_rebound`]), has a metaclass heki does not know,
    /// a project class follows the base, or no row applies.
    ///
    /// The answer is kept per class and name for the life of the indexes, as
    /// [`Self::catalog_attribute`]'s.
    pub(crate) fn class_object_attribute(
        &self,
        class: &str,
        attr: &str,
        subclasses: bool,
        kb: &EffectiveEcosystemKb,
    ) -> Option<String> {
        let class = self.canonical(class);
        if !self.is_project_class(&class) {
            return None;
        }
        let mro = self.mro(&class);
        let base = mro.iter().find(|c| !self.is_project_class(c))?;
        kb.class_attributes_of(base).next()?;
        self.class_object_attributes
            .ask(self, (class.clone(), attr.to_owned(), subclasses), || {
                let along = |mro: &[String]| self.class_row_along(mro, attr, kb);
                match subclasses {
                    true => self.row_of_every_value(&class, along).map(str::to_owned),
                    false => along(&mro).map(str::to_owned),
                }
            })
    }

    /// [`Self::class_object_attribute`] along the MRO `mro` of one class.
    fn class_row_along<'s>(
        &'s self,
        mro: &[String],
        attr: &str,
        kb: &'s EffectiveEcosystemKb,
    ) -> Option<&'s str> {
        let at = mro.iter().position(|c| !self.is_project_class(c))?;
        let base = &mro[at];
        // A store of the name on a class along the MRO, in any module, replaces the manager.
        if mro[at..].iter().any(|c| self.is_project_class(c)) || self.lookup_rebound(mro, attr) {
            return None;
        }
        let kept: Vec<&str> = kb
            .class_attributes_of(base)
            .map(|r| r.class.as_str())
            .collect();
        let mut may_bind = false;
        for c in &mro[..at] {
            self.read(|k| note(&mut k.class_facts, c));
            let d = self.class(c)?;
            if d.custom_metaclass {
                return None;
            }
            if d.members.contains(attr) {
                let callee = d.named_attrs.get(attr)?.body_call.as_deref()?;
                return (kept.iter().any(|t| self.derives_from(callee, t))).then_some(callee);
            }
            may_bind |= self.may_bind_instance_of(c, d, &kept);
        }
        if may_bind {
            return None;
        }
        (kb.class_attributes_of(base))
            .find(|r| r.attribute == attr)
            .map(|r| r.class.as_str())
    }

    /// Whether the body of the project class `class` (`decl`) may bind a name to an instance of
    /// one of the library classes `kept` or of a subclass: to a call of one of them or of a
    /// project class deriving from one, to what a call of a project function or method returns
    /// (`QuerySet.as_manager()`), or to a value heki cannot name. Methods, properties, nested
    /// classes, and calls of other library names (fields) bind none.
    fn may_bind_instance_of(&self, class: &str, decl: &ClassDecl, kept: &[&str]) -> bool {
        let bound = decl
            .code_attrs
            .iter()
            .filter(|name| decl.members.contains(*name));
        bound.into_iter().any(|name| {
            match decl
                .named_attrs
                .get(name)
                .and_then(|n| n.body_call.as_deref())
            {
                Some(callee) => {
                    let callee = self.canonical(callee);
                    match self.symbol(&callee) {
                        Symbol::Class(_) | Symbol::External => {
                            kept.iter().any(|t| self.derives_from(&callee, t))
                        }
                        _ => true,
                    }
                }
                None => {
                    !decl.properties.contains(name)
                        && self.own_method(class, name).is_none()
                        && self.class(&format!("{class}.{name}")).is_none()
                }
            }
        })
    }

    /// [`Self::catalog_attribute`] of the canonical `class`, decided
    /// ([`Self::row_of_every_value`]).
    fn find_catalog_attribute<'k>(
        &self,
        class: &str,
        attr: &str,
        kb: &'k EffectiveEcosystemKb,
    ) -> Option<&'k str> {
        self.row_of_every_value(class, |mro| {
            self.row_along(mro, attr, |c| kb.attribute_type(c, attr), |_| None)
        })
    }

    /// How reading `attr`, a name the catalog keeps fields in
    /// ([`EffectiveEcosystemKb::keeps_fields_in`]), on the project class `class`, or on an
    /// instance of it, holds the fields the class declares (`heki_kb::DeclaredFields`, R3): the
    /// catalog lists the attribute for the library base, or for a decorator of the project class,
    /// that each class the value may be under R2 finds first along its MRO, the same way for all
    /// of them, as [`Self::catalog_attribute`] decides a row.
    pub(crate) fn declared_fields_attr(
        &self,
        class: &str,
        attr: &str,
        kb: &EffectiveEcosystemKb,
    ) -> Option<FieldItems> {
        let decorated = |d: &ClassDecl| {
            (d.decorators.iter()).find_map(|(head, _)| kb.decorator_fields(head, attr))
        };
        self.row_of_every_value(&self.canonical(class), |mro| {
            self.row_along(mro, attr, |c| kb.base_fields(c, attr), decorated)
        })
    }

    /// Whether reading `__class__` on a value of the project class `class` gives the class of
    /// the value, as `type(value)` does: no class it may be under R2 binds the name or hooks
    /// attribute reads, no store gives its instances another class, and every base outside the
    /// project along their MROs is a plain base or a library base keeping the fields its
    /// subclasses declare (`heki_kb::DeclaredFields`: a model base, which defines no
    /// `__class__`).
    pub(crate) fn reads_own_class(&self, class: &str, kb: &EffectiveEcosystemKb) -> bool {
        let read = self.attr_read(class, "__class__");
        let known_base = |b: &String| {
            self.is_project_class(b) || PLAIN_BASES.contains(&b.as_str()) || kb.is_fields_base(b)
        };
        read.binders.is_empty()
            && !read.runs_hook(false)
            && self.value_classes(class).iter().all(|c| {
                let mro = self.mro(c);
                !self.lookup_rebound(&mro, "__class__") && mro.iter().all(known_base)
            })
    }

    /// The row `row_along` finds along the MRO of every class a value of the canonical `class`
    /// may be under R2 (`class` and every project subclass), the same for all of them. `None`
    /// when one finds another row or none, a class along the MRO of one defines
    /// `__getattribute__` or has a metaclass heki does not know, or there are more than
    /// [`DISPATCH_CLASS_LIMIT`] classes: the walk over the classes stops at the first such.
    fn row_of_every_value<R: Copy + PartialEq>(
        &self,
        class: &str,
        row_along: impl Fn(&[String]) -> Option<R>,
    ) -> Option<R> {
        let found = row_along(&self.mro(class))?;
        let budget = Budget::unmetered("heki_effect::resolver::ProjectDecls::row_of_every_value");
        let mut visited = 0;
        let differs = reach(
            &budget,
            [class],
            |c, out| out.extend(self.direct_subclasses(c).iter().map(String::as_str)),
            |c| {
                visited += 1;
                if visited > DISPATCH_CLASS_LIMIT {
                    return Visit::Stop(());
                }
                let mro = self.mro(c);
                let same = !self.hooks_attribute_reads(&mro) && row_along(&mro) == Some(found);
                if same { Visit::Expand } else { Visit::Stop(()) }
            },
        );
        (differs == Answer::Known(None)).then_some(found)
    }

    /// Whether a project class along `mro` defines `__getattribute__`, which runs on every read,
    /// or has a metaclass heki does not know, which may install a descriptor.
    fn hooks_attribute_reads(&self, mro: &[String]) -> bool {
        mro.iter().any(|c| {
            self.read(|k| note(&mut k.class_facts, c));
            self.class(c)
                .is_some_and(|d| d.custom_metaclass || d.members.contains("__getattribute__"))
        })
    }

    /// The catalog row reading `attr` finds along `mro`: that of the first class outside the
    /// project (`library`), a builtin type without one passed over, or of the first project
    /// class whose decorators give it one (`decorated`); none past a project class binding the
    /// name, in its body, on `self`, or on the class object outside its body.
    fn row_along<R>(
        &self,
        mro: &[String],
        attr: &str,
        library: impl Fn(&str) -> Option<R>,
        decorated: impl Fn(&ClassDecl) -> Option<R>,
    ) -> Option<R> {
        for c in mro {
            if let Some(d) = self.class(c) {
                self.read(|k| note(&mut k.class_facts, c));
                if d.members.contains(attr) || d.rebound_attrs.contains(attr) {
                    return None;
                }
                if let Some(found) = decorated(d) {
                    return Some(found);
                }
                continue;
            }
            if is_unknown_base(c) || self.under_unread(c) {
                return None;
            }
            if let Some(found) = library(c) {
                return Some(found);
            }
            if !c.starts_with("builtins.") {
                return None;
            }
        }
        None
    }

    /// Whether no class of the MRO of the project class `class` has the member `name`: no
    /// project class defines it, and every base outside the project is a builtin type that
    /// does not ([`Self::inherited_external`]).
    pub(crate) fn lacks_member(&self, class: &str, name: &str, kb: &EffectiveEcosystemKb) -> bool {
        let class = self.canonical(class);
        self.is_project_class(&class) && {
            let mro = self.mro(&class);
            self.definitions_along(&mro, &class, name, false, false, |_| true)
                .is_empty()
                && matches!(
                    self.inherited_external(&mro, name, false, kb),
                    Inherited::Absent
                )
        }
    }

    /// The function an instance of the project class `class` takes `name` from: along its MRO,
    /// the first class whose body or methods bind `name` (`ClassDecl::members`) defines it.
    /// `None` when a plain class or instance attribute comes first (reading it runs nothing), a
    /// class on the way stores the name outside its body, a base outside the project comes
    /// first (heki does not know what it binds), or none binds it.
    pub fn function_taken(&self, class: &str, name: &str) -> Option<FuncKey> {
        self.function_taken_along(&self.mro(class), name)
    }

    /// Whether a class along the MRO of the project class `class` binds `name`, to whatever
    /// object: in its body or its methods (`ClassDecl::members`), or by a store elsewhere.
    /// `None` when a class on the way other than `object` is not a project class (heki does not
    /// know what it binds).
    pub fn binds_along(&self, class: &str, name: &str) -> Option<bool> {
        let mro = self.mro(class);
        if self.lookup_rebound(&mro, name) {
            return Some(true);
        }
        let mut binds = false;
        for c in mro.iter().filter(|c| *c != "builtins.object") {
            self.read(|k| note(&mut k.class_facts, c));
            binds |= self.class(c)?.members.contains(name);
        }
        Some(binds)
    }

    /// The name reading `attr` on the class `class` or on one of its instances finds, when the
    /// first class along the MRO defining it binds it last to one module-level name
    /// ([`Self::class_aliases`]), with whether the body wraps it in `staticmethod`.
    pub(crate) fn class_alias(&self, class: &str, attr: &str) -> Option<(&str, bool)> {
        match self.class_aliases(class, attr)? {
            ClassAlias {
                names,
                defined: None,
                partial: None,
            } if names.len() == 1 => names.first().copied(),
            _ => None,
        }
    }

    /// Whether some class body binds `attr` to a module-level name (`ClassDecl::named_attrs`):
    /// otherwise no class reads it as an alias ([`Self::class_aliases`]).
    pub(crate) fn binds_alias(&self, attr: &str) -> bool {
        self.read(|k| note(&mut k.alias_names, attr));
        self.alias_names.contains(attr)
    }

    /// What reading `attr` on the class `class` or on one of its instances finds, when the first
    /// class along the MRO defining it binds it in its body to a module-level name
    /// (`ClassDecl::named_attrs`: `factory = Scope`, `REQUIRE: ClassVar[...] =
    /// staticmethod(require)`), or to different names, or its own `def`, on different paths. None
    /// of the module's code may store the attribute on the class or an instance (`rebound_attrs`,
    /// what `stores::record` decides, attributes stored under names heki cannot read), another
    /// module may rebind it, or a hook run on the read (`__getattribute__`, a metaclass heki
    /// does not know); a field the class body annotates is the instance's own.
    pub(crate) fn class_aliases(&self, class: &str, attr: &str) -> Option<ClassAlias<'_>> {
        if !self.binds_alias(attr) {
            return None;
        }
        let class = self.canonical(class);
        for c in self.mro(&class) {
            self.read(|k| note(&mut k.class_facts, &c));
            let decl = self.class(&c)?;
            if decl.rebound_attrs.contains(attr) {
                return None;
            }
            if !decl.members.contains(attr) {
                continue;
            }
            let named = decl.named_attrs.get(attr)?;
            let names: Vec<(&str, bool)> = match &named.body {
                Some(body) => vec![(body.as_str(), named.body_static)],
                None => named
                    .body_paths
                    .iter()
                    .map(|(name, is_static)| (name.as_str(), *is_static))
                    .collect(),
            };
            let partial = named.body_partial.as_deref();
            let alias = (!names.is_empty() || partial.is_some()).then(|| ClassAlias {
                names,
                defined: named.body_defined.then(|| format!("{c}.{attr}")),
                partial,
            })?;
            let stored =
                self.attr_store(&c, decl, attr).is_some() || self.stores_any_attr(&c, decl);
            if named.annotated || !named.held.is_empty() || stored {
                return None;
            }
            if self.rebound_elsewhere(&format!("{c}.{attr}"))
                || self.attr_read(&class, attr).runs_hook(false)
            {
                return None;
            }
            return Some(alias);
        }
        None
    }

    /// [`Self::function_taken`] along `mro`: an MRO, or the part of one after a class (what
    /// `super()` in that class looks up).
    fn function_taken_along(&self, mro: &[String], name: &str) -> Option<FuncKey> {
        // Bound again on a class along the MRO (`Child.view = []`), the name holds that object
        // from then on.
        if self.lookup_rebound(mro, name) {
            return None;
        }
        for c in mro {
            self.read(|k| note(&mut k.class_facts, c));
            let decl = self.class(c)?;
            if decl.members.contains(name) {
                return self.own_method(c, name);
            }
        }
        None
    }

    /// Resolve `instance_of(class).method(...)`: MRO definition plus overriding project
    /// subclasses (Priority 4 CHA). A method no project class defines is the symbol of the
    /// base outside the project that defines it ([`Self::inherited_external`]), or unresolved.
    /// The MRO is the one a type checker reads ([`Self::declared_mro`]).
    pub(crate) fn method_targets(
        &self,
        class: &str,
        method: &str,
        skip_self: bool,
        kb: &EffectiveEcosystemKb,
    ) -> Callee {
        let class = self.canonical(class);
        if !self.is_project_class(&class) {
            return self.outside(format!("{class}.{method}"));
        }
        let mro = self.declared_mro(&class);
        let found = self.definitions_along(&mro, &class, method, skip_self, !skip_self, |_| true);
        if found.len() > CHA_CANDIDATE_LIMIT {
            return Callee::TooMany;
        }
        if found.is_empty() {
            return self
                .inherited_external(&mro, method, skip_self, kb)
                .callee(self);
        }
        Callee::Project(found, true)
    }

    /// Whether `callee`, the targets of calling `method` on an instance of `class`, are every
    /// implementation the call can run under R2 and R5: an instance of `class` or of any of
    /// its project subclasses runs the definition its MRO finds, which must be a target (a
    /// library one only as the call's single target). A protocol is implemented by classes
    /// that do not name it, so its implementations are never complete; neither are more than
    /// [`DISPATCH_CLASS_LIMIT`] classes.
    pub(crate) fn instance_targets_complete(
        &self,
        class: &str,
        method: &str,
        callee: &Callee,
        kb: &EffectiveEcosystemKb,
    ) -> bool {
        let class = self.canonical(class);
        let budget =
            Budget::unmetered("heki_effect::resolver::ProjectDecls::instance_targets_complete");
        let mut visited = 0;
        let missing = reach(
            &budget,
            [class.as_str()],
            |c, out| out.extend(self.direct_subclasses(c).iter().map(String::as_str)),
            |c| {
                visited += 1;
                if visited > DISPATCH_CLASS_LIMIT || !self.runs_a_target(c, method, callee, kb) {
                    Visit::Stop(())
                } else {
                    Visit::Expand
                }
            },
        );
        missing == Answer::Known(None)
    }

    /// [`Self::instance_targets_complete`] on an instance of exactly `class`: the definition its
    /// MRO finds is a target ([`Self::runs_a_target`]); no subclass's runs.
    pub(crate) fn exact_targets_complete(
        &self,
        class: &str,
        method: &str,
        callee: &Callee,
        kb: &EffectiveEcosystemKb,
    ) -> bool {
        self.runs_a_target(&self.canonical(class), method, callee, kb)
    }

    /// Whether calling the project class `class` by name makes an instance of exactly that class
    /// (`Inferred::exact`): no project class along its MRO binds `__new__`, in its body or after
    /// it, and no metaclass is custom ([`Self::construction_is_open`]; either may return an
    /// instance of another class, a subclass picked by the arguments), and the project binds the
    /// name to no other object ([`Self::call_rebound`]).
    pub(crate) fn constructs_exactly(&self, class: &str) -> bool {
        let class = self.canonical(class);
        self.is_project_class(&class)
            && !self.call_rebound(&class)
            && !self.construction_is_open(&class)
    }

    /// Whether reading `attr` on every instance of the project class `class` or of a project
    /// subclass (R2; only of `class` itself when the instance is `exact`) runs one of the getters
    /// `keys`: what each such class takes the name from is one of them
    /// ([`Self::function_taken`]: not a plain attribute, a method, or a value stored on the class
    /// outside its body, and no base outside the project before it), and no class is a protocol,
    /// defines `__getattribute__`, or has a metaclass heki does not know.
    pub(crate) fn getters_complete(
        &self,
        class: &str,
        attr: &str,
        keys: &[FuncKey],
        exact: bool,
    ) -> bool {
        let class = self.canonical(class);
        let read = self.attr_read(&class, attr);
        if read.getattribute || read.custom_metaclass {
            return false;
        }
        let taken = |c: &str| {
            self.protocol(c)
                .is_none()
                .then(|| self.function_taken(c, attr))
                .flatten()
        };
        if exact {
            return taken(&class).is_some_and(|k| keys.contains(&k));
        }
        self.every_class_finds(&class, keys, taken)
    }

    /// Whether reading `attr` through `super()` in the project class `owner` runs one of the
    /// getters `keys` on every instance (R2): for the owner and each project subclass, what its
    /// MRO takes the name from after the owner ([`Self::function_taken`] from there) is one of
    /// them, also when a subclass's MRO puts another class there.
    pub(crate) fn super_getters_complete(&self, owner: &str, attr: &str, keys: &[FuncKey]) -> bool {
        let owner = self.canonical(owner);
        self.every_class_finds(&owner, keys, |c| {
            let mro = self.mro(c);
            let after = mro.iter().position(|m| *m == owner).map_or(0, |i| i + 1);
            self.function_taken_along(&mro[after..], attr)
        })
    }

    /// Whether `find` gives one of `keys` for the project class `class` and for each of its
    /// project subclasses, at most `DISPATCH_CLASS_LIMIT` of them.
    fn every_class_finds(
        &self,
        class: &str,
        keys: &[FuncKey],
        find: impl Fn(&str) -> Option<FuncKey>,
    ) -> bool {
        let budget = Budget::unmetered("heki_effect::resolver::ProjectDecls::every_class_finds");
        let mut visited = 0;
        let missing = reach(
            &budget,
            [class],
            |c, out| out.extend(self.direct_subclasses(c).iter().map(String::as_str)),
            |c| {
                visited += 1;
                let runs = find(c).is_some_and(|k| keys.contains(&k));
                if visited > DISPATCH_CLASS_LIMIT || !runs {
                    Visit::Stop(())
                } else {
                    Visit::Expand
                }
            },
        );
        missing == Answer::Known(None)
    }

    /// Whether a project class along the MRO of `class` defines `__slots__`: an instance may then
    /// have no `__dict__`.
    pub fn defines_slots(&self, class: &str) -> bool {
        self.mro(&self.canonical(class)).into_iter().any(|c| {
            self.read(|k| note(&mut k.class_facts, &c));
            self.class(&c)
                .is_some_and(|d| d.members.contains("__slots__"))
        })
    }

    /// Whether an instance of exactly `class` calling `method` runs one of `callee`: the
    /// definition its MRO finds is a target (a library one, [`Self::inherited_external`], only
    /// as the call's single target, and only when the class binds none of the names the library
    /// method looks up on the instance, [`Self::replaces_delegate`]), or it has none and the call
    /// raises. A protocol has implementations no class names, and a base heki knows nothing of
    /// may define the method.
    fn runs_a_target(
        &self,
        class: &str,
        method: &str,
        callee: &Callee,
        kb: &EffectiveEcosystemKb,
    ) -> bool {
        if self.protocol(class).is_some() {
            return false;
        }
        let mro = self.mro(class);
        if self.lookup_rebound(&mro, method) {
            return false;
        }
        match self
            .definitions_along(&mro, class, method, false, false, |_| true)
            .first()
        {
            Some(found) => matches!(callee, Callee::Project(keys, _) if keys.contains(found)),
            None => match self.inherited_external(&mro, method, false, kb) {
                // An instance without the method raises at the call: it never returns from it.
                Inherited::Absent => true,
                other => matches!(
                    (other, callee),
                    (Inherited::External(external), Callee::External(s))
                        if *s == external && !self.replaces_delegate(&mro, &external, kb)
                ),
            },
        }
    }

    /// Whether project code replaces what the library method `symbol` runs, on an instance
    /// whose class has the MRO `mro` (R3): a project class before the library class binds a
    /// name the catalog says the method looks up on the instance
    /// (`heki_kb::EcosystemEntry::delegates_to`: Django's `Manager.get_queryset`,
    /// `QuerySet._fetch_all`), in its body or methods or by a store elsewhere, or a project
    /// class deriving from the base whose methods the call runs on an instance it makes binds
    /// one of them otherwise than as the catalog allows ([`Self::effect_methods_replaced`]:
    /// a model's `save` behind `QuerySet.create`). The call then runs that binding, which the
    /// method's row does not describe.
    pub(crate) fn replaces_delegate(
        &self,
        mro: &[String],
        symbol: &str,
        kb: &EffectiveEcosystemKb,
    ) -> bool {
        if kb
            .effect_through(symbol)
            .is_some_and(|(base, methods)| self.effect_methods_replaced(base, methods))
        {
            return true;
        }
        let names: Vec<&String> = kb.delegates_of(symbol).collect();
        if names.is_empty() {
            return false;
        }
        let library = symbol.rsplit_once('.').map_or(symbol, |(class, _)| class);
        let before = &mro[..mro.iter().position(|c| c == library).unwrap_or(mro.len())];
        let own = before.iter().filter(|c| self.is_project_class(c));
        let binds = |c: &String| {
            self.read(|k| note(&mut k.class_facts, c));
            self.class(c)
                .is_some_and(|d| names.iter().any(|n| d.members.contains(*n)))
        };
        own.into_iter().any(binds) || names.iter().any(|n| self.lookup_rebound(before, n))
    }

    /// Whether a project class deriving from the library class `base` may run code of its own,
    /// or of a library class before `base` along its MRO, for one of the methods `methods` of
    /// `base` (`heki_kb::EcosystemEntry::effect_methods`): it binds one other than as a method
    /// calling the next definition along the MRO on every path to a return
    /// ([`ClassDecl::chains_to_next`]), in its body or by a store elsewhere, or a library class
    /// that is no plain base comes before `base`. One walk over the project's subclasses of
    /// `base`, kept for the life of the indexes.
    fn effect_methods_replaced(&self, base: &str, methods: &[String]) -> bool {
        self.effect_methods_replaced.ask(self, base.to_owned(), || {
            let replaces = |class: &String| {
                let mro = self.mro(class);
                let before = &mro[..mro.iter().position(|c| c == base).unwrap_or(mro.len())];
                before.iter().any(|c| {
                    let own_binding = |d: &ClassDecl| {
                        (methods.iter()).any(|m| {
                            (d.members.contains(m) && !d.chains_to_next.contains(m))
                                || self.lookup_rebound(std::slice::from_ref(c), m)
                        })
                    };
                    self.read(|k| note(&mut k.class_facts, c));
                    match self.is_project_class(c) {
                        true => self.class(c).is_none_or(own_binding),
                        false => !PLAIN_BASES.contains(&c.as_str()),
                    }
                })
            };
            self.subclasses_of(base).iter().any(replaces)
        })
    }

    /// Resolve `class.method` on an object that is exactly `class`: the class object itself
    /// (`Base.method(obj)`, and the `__init__` of constructing it) or an instance of exactly the
    /// class ([`Recv::Exact`]). The definition its MRO finds first ([`Self::declared_mro`]); the
    /// overrides of subclasses do not count, as the object is no subclass's.
    pub(crate) fn class_targets(
        &self,
        class: &str,
        method: &str,
        kb: &EffectiveEcosystemKb,
    ) -> Callee {
        let class = self.canonical(class);
        let mro = self.declared_mro(&class);
        let found = self.definitions_along(&mro, &class, method, false, false, |_| true);
        if found.is_empty() {
            return self
                .inherited_external(&mro, method, false, kb)
                .callee(self);
        }
        Callee::Project(found, true)
    }

    /// The project method `name` that the MRO of the class `class` finds first (what calling it
    /// on an instance of exactly `class` runs; overrides of subclasses do not count).
    pub fn mro_method(&self, class: &str, name: &str) -> Option<FuncKey> {
        let class = self.canonical(class);
        self.definitions(&class, name, false, false, |_| true).pop()
    }

    /// Whether a project function named `name` (its last name) may return a parameter or an
    /// attribute of one (`FunctionDecl::returns_param`): no call or read with another name
    /// hands one out.
    pub(crate) fn may_return_param(&self, name: &str) -> bool {
        self.read(|k| note(&mut k.returning_names, name));
        self.returning_names.contains(name)
    }

    /// Resolve reading `instance_of(class).attr` as calls of the property getters it may run
    /// (`FunctionDecl::is_property`), found like [`Self::method_targets`]' candidates: the MRO
    /// definition when it is a getter, plus (unless `skip_self` or `exact`, an instance of exactly
    /// the class) the getters of overriding project subclasses (Priority 4 CHA), at most
    /// `CHA_CANDIDATE_LIMIT` of them (`TooMany` beyond). `None` when no modeled getter runs: a
    /// plain attribute, a method, an attribute of an external class (never a Layer 2 symbol: an
    /// `[[attributes]]` row types what a read gives, not what it runs), or a property with a
    /// setter or deleter (`FunctionDecl::is_property`).
    pub(crate) fn property_targets(
        &self,
        class: &str,
        attr: &str,
        skip_self: bool,
        exact: bool,
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
        let overrides = !skip_self && !exact;
        let found = self.definitions(&class, attr, skip_self, overrides, is_getter);
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

/// The joins over every module that only the completed declarations decide
/// ([`ProjectDecls::completed_joins`]).
type CompletedJoins = (stores::Elsewhere, FxHashMap<(String, String), Changeable>);

impl DeclPasses {
    /// From the completed declarations of every module (`.py` implementations after `.pyi`
    /// stubs sharing a module name, so that they win).
    pub fn new(completed: impl IntoIterator<Item = Arc<ModuleDecls>>) -> Self {
        let completed: Vec<Arc<ModuleDecls>> = completed.into_iter().collect();
        Self {
            structural: ProjectDecls::new(completed.iter().map(|d| d.structural())),
            completed: ProjectDecls::new(completed),
        }
    }

    /// The declarations summaries and rules resolve names through.
    pub fn completed(&self) -> &ProjectDecls {
        &self.completed
    }

    /// Replace (or add) one module's completed declarations; returns what changed.
    pub fn replace(&mut self, completed: impl Into<Arc<ModuleDecls>>) -> DeclKeys {
        let completed = completed.into();
        let name = completed.module_name.clone();
        if self.completed.module(&name) == Some(&*completed) {
            return DeclKeys::default();
        }
        let before = self.structural.indexes();
        let before_stores = self.completed.completed_joins();
        self.structural.replace(completed.structural());
        self.completed.replace(completed);
        self.changed(&name, Some(&before), &before_stores)
    }

    /// Forget a deleted module; returns what changed.
    pub fn remove(&mut self, module: &str) -> DeclKeys {
        let before = self.structural.indexes();
        let before_stores = self.completed.completed_joins();
        self.structural.remove(module);
        self.completed.remove(module);
        self.changed(module, Some(&before), &before_stores)
    }

    /// What changed when `module`'s declarations did: the module, the hierarchy indexes that
    /// differ from `before` (unchanged without it: the module's structure stayed), and what
    /// the joins of the completed declarations give that differs from `before_stores`: what
    /// other modules' stores change (`ProjectDecls::elsewhere`), and what the modules bind to
    /// read-only container fields (`ProjectDecls::field_values`).
    fn changed(
        &self,
        module: &str,
        before: Option<&Indexes>,
        before_stores: &CompletedJoins,
    ) -> DeclKeys {
        let mut out = before.map_or_else(DeclKeys::default, |b| self.structural.index_changes(b));
        out.modules.insert(module.to_owned());
        let (elsewhere, field_values) = before_stores;
        out.grown_globals
            .extend(elsewhere.changes(&self.completed.elsewhere));
        let now = &self.completed.field_values;
        for key in field_values.keys().chain(now.keys()) {
            if field_values.get(key) != now.get(key) {
                out.class_facts.insert(fields::field_key(&key.0, &key.1));
            }
        }
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
) -> (DeclKeys, Arc<ModuleDecls>, DeclKeys) {
    let name = &input.module_name;
    let structural = structural_decls(input, kb);
    let restructured = passes.structural.module(name) != Some(&structural);
    let before = restructured.then(|| passes.structural.indexes());
    if restructured {
        passes.structural.replace(structural.clone());
    }
    let (completed, reads) = complete_against(input, structural, &passes.structural, kb);
    if passes.completed.module(name) == Some(&completed) {
        return (DeclKeys::default(), Arc::new(completed), reads);
    }
    let completed = Arc::new(completed);
    let before_stores = passes.completed.completed_joins();
    passes.completed.replace(Arc::clone(&completed));
    let changed = passes.changed(name, before.as_ref(), &before_stores);
    (changed, completed, reads)
}

/// A module's completed declarations against pass 1 without updating it (a `.pyi` stub next to
/// its `.py` implementation, whose declarations win), and what the completion read.
pub(crate) fn own_module_decls(
    input: &ModuleInput<'_>,
    kb: &EffectiveEcosystemKb,
    passes: &DeclPasses,
) -> (Arc<ModuleDecls>, DeclKeys) {
    let (completed, reads) =
        complete_against(input, structural_decls(input, kb), &passes.structural, kb);
    (Arc::new(completed), reads)
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
    others: Option<Vec<(PathBuf, Arc<ModuleDecls>)>>,
    track: bool,
) -> (ProjectDecls, Vec<(Arc<ModuleDecls>, DeclKeys)>) {
    // The owner of a module name several files share comes last, so its declarations win
    // (`crate::module_precedence`).
    let order = |items: Vec<(PathBuf, Arc<ModuleDecls>)>| {
        let mut items = items;
        items.sort_by(|(a, _), (b, _)| {
            crate::module_precedence(a).cmp(&crate::module_precedence(b))
        });
        items.into_iter().map(|(_, d)| d)
    };
    let structural: Vec<(PathBuf, Arc<ModuleDecls>)> = modules
        .iter()
        .map(|m| (m.path.clone(), Arc::new(structural_decls(m, kb))))
        .collect();
    let others = others.unwrap_or_default();
    let pass1 = ProjectDecls::new(order(
        others
            .iter()
            .map(|(path, d)| (path.clone(), Arc::new(d.structural())))
            .chain(structural.iter().cloned())
            .collect(),
    ));
    let own: Vec<(Arc<ModuleDecls>, DeclKeys)> = if track {
        // Every module's reads go to the one recorder of `pass1`: one module at a time.
        modules
            .iter()
            .zip(&structural)
            .map(|(m, (_, d))| {
                let (completed, reads) = complete_against(m, ModuleDecls::clone(d), &pass1, kb);
                (Arc::new(completed), reads)
            })
            .collect()
    } else {
        // A module's completion depends on it and pass 1 only (indexed collect: input order).
        modules
            .par_iter()
            .zip(&structural)
            .map(|(m, (_, d))| {
                let completed = complete_decls(m, ModuleDecls::clone(d), &pass1, kb);
                (Arc::new(completed), DeclKeys::default())
            })
            .collect()
    };
    let completed = structural
        .iter()
        .zip(&own)
        .map(|((path, _), (d, _))| (path.clone(), Arc::clone(d)));
    let decls = ProjectDecls::new(order(others.into_iter().chain(completed).collect()));
    (decls, own)
}

/// The bases of the class statements outside the module body (in a function or a class body),
/// by the statement's span and in their order (`None`: a base left out). Each is what the value
/// the statement passes is ([`module_read`]): through `module_base` a module-level name, the
/// object of an import in the function, or else an unknown base ([`UNKNOWN_BASE`]): a
/// parameter, a local, a closure cell, or a class-body name of the imported class's name is
/// another value. A statement made more than once (in a `finally` body, once per way out of the
/// `try`) keeps the bases its copies agree on; one they disagree on is unknown.
fn scoped_bases(
    input: &ModuleInput<'_>,
    module_base: &dyn Fn(&str) -> Option<String>,
) -> FxHashMap<Span, Vec<Option<String>>> {
    let ir = input.ir;
    let imports: FxHashMap<Span, String> = input
        .semantic
        .imports
        .iter()
        .filter(|imp| !imp.in_type_checking)
        .filter_map(|imp| Some((imp.span, import_target(imp, input)?)))
        .collect();
    let mut holders: Vec<heki_ir::FunctionId> = ir.class_bases.iter().map(|b| b.0).collect();
    holders.dedup();
    let mut out: FxHashMap<Span, Vec<Option<String>>> = FxHashMap::default();
    for f in holders.into_iter().map(|id| ir.function(id)) {
        if f.is_module_body {
            continue;
        }
        let shape = FnShape::new(f);
        let mut statements: Vec<(VarId, Span, Vec<Option<String>>)> = Vec::new();
        for base in ir.class_base_values(&shape) {
            let value = match module_read(&shape, &imports, base.value) {
                Some(ModuleRead::Name(text)) => module_base(&text),
                Some(ModuleRead::Imported(fqn)) => Some(fqn),
                None => Some(UNKNOWN_BASE.to_owned()),
            };
            match statements.last_mut() {
                Some((class, _, values)) if *class == base.class => values.push(value),
                _ => statements.push((base.class, base.statement.span, vec![value])),
            }
        }
        for (_, span, values) in statements {
            match out.entry(span) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(values);
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    for (kept, value) in slot.get_mut().iter_mut().zip(values) {
                        if *kept != value {
                            *kept = Some(UNKNOWN_BASE.to_owned());
                        }
                    }
                }
            }
        }
    }
    out
}

/// What a value of a function holds when a module-level name or an import gives it
/// ([`module_read`]).
enum ModuleRead {
    /// A name of the module, or an attribute of one (`Rule`, `core.Rule`).
    Name(String),
    /// The object an import in the function binds, or an attribute of it.
    Imported(String),
}

/// What `value` of the function `shape` holds when a module-level name or an import gives it,
/// through copies and the origin of a generic alias (`Base[int]`): a name the function loads
/// from the module's globals (in a class body also one the body has not bound there, which
/// Python looks up in the globals), what an import in the function binds (`imports`, by the
/// span of its `Import` instruction), or an attribute of either (`core.Rule`). `None` for any
/// other value: a parameter, a local bound to anything else, a closure cell, a value merged
/// from several paths, a call's result, or a name more than `MAX_ALIAS_SEGMENTS` attributes
/// deep.
fn module_read(
    shape: &FnShape<'_>,
    imports: &FxHashMap<Span, String>,
    value: VarId,
) -> Option<ModuleRead> {
    let loaded_from = |v: VarId| match &shape.inst(v)?.kind {
        InstKind::AttrLoad { obj, .. } | InstKind::SubscriptLoad { obj, .. } => {
            Some(shape.copy_root(*obj))
        }
        _ => None,
    };
    // At most `MAX_ALIAS_SEGMENTS` attribute loads (or a generic alias and one fewer) and the
    // name they start from: a longer chain ends at a load, which names nothing.
    let chain = std::iter::successors(Some(shape.copy_root(value)), |&v| loaded_from(v))
        .take(MAX_ALIAS_SEGMENTS + 1)
        .map(|v| shape.inst(v))
        .collect::<Option<Vec<&Inst>>>()?;
    let (root, loads) = chain.split_last()?;
    // Only the class a generic alias subscripts is its base (`Base[int]`), not an item.
    let attrs = loads
        .iter()
        .skip_while(|i| matches!(i.kind, InstKind::SubscriptLoad { .. }))
        .map(|i| match &i.kind {
            InstKind::AttrLoad { attr, .. } => Some(attr.as_str()),
            _ => None,
        })
        .collect::<Option<Vec<&str>>>()?;
    let dotted = |head: &str| {
        attrs
            .iter()
            .rev()
            .fold(head.to_owned(), |name, attr| format!("{name}.{attr}"))
    };
    match &root.kind {
        InstKind::LoadGlobal { name, .. } => Some(ModuleRead::Name(dotted(name))),
        InstKind::LoadName { name, .. } if shape.f.is_class_body => {
            Some(ModuleRead::Name(dotted(name)))
        }
        InstKind::Import { .. } => Some(ModuleRead::Imported(dotted(imports.get(&root.span)?))),
        _ => None,
    }
}

/// The type parameters of the module-body class `class`, whose resolved bases are `bases`, by
/// name in order: those of its type parameter list (`class Site[T]:`), else those its
/// `Generic[...]` base names, else its `Protocol[...]` base's. `None` for a class whose
/// parameters come from its other bases' arguments, which heki does not follow.
fn class_type_params<'c>(class: &'c ClassInfo, bases: &[String]) -> Option<&'c [String]> {
    if !class.type_params.is_empty() {
        return Some(&class.type_params);
    }
    let passed_to = |names: &[&str]| {
        (class.base_type_args.iter())
            .find(|(i, _)| bases.get(*i).is_some_and(|b| names.contains(&b.as_str())))
            .map(|(_, args)| args.as_slice())
    };
    passed_to(&["typing.Generic", "typing_extensions.Generic"])
        .or_else(|| passed_to(&["typing.Protocol", "typing_extensions.Protocol"]))
}

/// Whether the module body runs the `class` statement `class` itself: the nearest block around
/// it, past a PEP 695 type-parameter scope, is the module's. A class named as a module-level one
/// may be made by a function declaring its name `global` ([`ModuleDecls::made_in`]).
fn statement_in_module_body(semantic: &SemanticModel, class: &ClassInfo) -> bool {
    let scope =
        |id: heki_semantic::ScopeId| semantic.scopes.get(id.0 as usize).filter(|sc| sc.id == id);
    let first = scope(class.scope).and_then(|sc| sc.parent).and_then(scope);
    std::iter::successors(first, |sc| sc.parent.and_then(scope))
        .find(|sc| sc.kind != ScopeKind::TypeParameters)
        .is_some_and(|sc| sc.kind == ScopeKind::Module)
}

/// Pass 1: imports, classes (bases), functions (signatures) — no inference.
fn structural_decls(input: &ModuleInput<'_>, kb: &EffectiveEcosystemKb) -> ModuleDecls {
    let semantic = input.semantic;
    let module_scope = module_scope(semantic);
    let mut decls = ModuleDecls {
        module_name: input.module_name.clone(),
        is_package: input.is_package,
        ..ModuleDecls::default()
    };
    // Imports a later statement rebinds: `(name, target, end of the import, the rebinding)`.
    let mut shadowed: Vec<(&str, String, u32, u32)> = Vec::new();
    for imp in &semantic.imports {
        if Some(imp.scope) != module_scope {
            continue;
        }
        let Some(target) = import_target(imp, input) else {
            continue;
        };
        if imp.in_type_checking {
            decls
                .type_checking_imports
                .insert(imp.bound_name.clone(), target);
        } else if let Some(at) = imp.rebound_at {
            shadowed.push((&imp.bound_name, target, imp.span.end, at));
        } else {
            decls.imports.insert(imp.bound_name.clone(), target);
        }
    }
    for class in &semantic.classes {
        decls.classes.insert(class.qualname.clone(), Box::default());
    }
    for f in &input.ir.functions {
        if is_summarized_function(f) {
            decls.functions.insert(f.qualname.clone(), Box::default());
        }
    }
    let made_in = made_in(input);
    decls.made_in = (!made_in.is_empty()).then(|| Box::new(made_in));
    let bindings = BodyBindings::of(input, &decls);
    (decls.aliases, decls.alias_paths) = module_aliases(input, &decls, &bindings);
    // Before the type names: a module global shadows the builtin of its name.
    let stores = GlobalStores::of(&input.ir.functions);
    decls.unknown_globals = unknown_globals(&stores, &decls);
    type_names::type_names(input, &mut decls);
    decls.type_checking_only = type_checking_only(semantic, &stores, &decls);
    let rebound = rebound_imports(input, &stores, &decls);
    decls.rebound_imports = (!rebound.is_empty()).then(|| Box::new(rebound));
    decls.rebound_definitions = rebound_definitions(input, &stores, &decls, &bindings);
    let stars = runtime_star_imports(input);
    let late = late_stars(&decls, &bindings, &stars);
    decls.late_stars = (!late.is_empty()).then(|| Box::new(late));
    decls.private_members = (decls.classes.keys())
        .chain(decls.functions.keys())
        .any(|qualname| mangling::member_path(qualname).is_some());
    // Qualnames whose every definition is a property getter (a setter, a deleter, or any other
    // function with the same qualname shares its summary).
    let mut getters_only: FxHashMap<&str, bool> = FxHashMap::default();
    for f in input
        .ir
        .functions
        .iter()
        .filter(|f| is_summarized_function(f))
    {
        let getter = f.owner_class_qualname.is_some()
            && f.decorators
                .iter()
                .any(|d| decorators::is(d, &decls, |k| k.getter));
        *getters_only.entry(&f.qualname).or_insert(true) &= getter;
    }
    // The module scope's bindings are sorted by name.
    let module_binds = |name: &str| {
        semantic.scopes.first().is_some_and(|sc| {
            sc.bindings
                .binary_search_by(|b| b.name.as_str().cmp(name))
                .is_ok()
        })
    };
    // A base a module-level name gives (`Rule`, `core.Rule`): the class the name resolves to; for
    // a name the module binds that heki names no class for (`Base = declarative_base()`), an
    // unknown base. A name the module does not bind (a builtin heki does not list) is left out.
    let module_base = |decls: &ModuleDecls, text: &str| {
        decls.resolve_dotted(text, false).or_else(|| {
            module_binds(text.split('.').next().unwrap_or(text)).then(|| UNKNOWN_BASE.to_owned())
        })
    };
    let scoped = scoped_bases(input, &|text| module_base(&decls, text));
    let mut type_params: FxHashMap<&str, &[String]> = FxHashMap::default();
    // Resolve bases, class keywords, and annotations now that local names are known.
    for class in &semantic.classes {
        // A module-level class statement evaluates its bases before it binds its own name: a
        // base named by an import that a later statement (this one, or one after it) rebinds
        // is the imported class (`from pkg.base import Parser`, then `class Parser(Parser)`).
        let imported_here = |name: &str| {
            let at = class.span.start;
            shadowed
                .iter()
                .find(|(n, _, end, rebound)| *n == name && *end < at && at <= *rebound)
                .map(|(_, target, _, _)| target)
        };
        let mut bases: Vec<String> = Vec::new();
        let in_module_body = statement_in_module_body(semantic, class);
        if in_module_body {
            // The module body names its bases through the module's names; an expression
            // (`with_metaclass(Meta)`, `*bases`) is an unknown base.
            bases.extend(class.bases.iter().filter_map(|b| {
                let Some(b) = b else {
                    return Some(UNKNOWN_BASE.to_owned());
                };
                let head = b.split('[').next().unwrap_or(b).trim();
                let (first, rest) = head.split_once('.').unwrap_or((head, ""));
                match imported_here(first) {
                    Some(target) if rest.is_empty() => Some(target.clone()),
                    Some(target) => Some(format!("{target}.{rest}")),
                    None => module_base(&decls, head),
                }
            }));
        } else {
            let values = scoped.get(&class.span).map_or(&[][..], Vec::as_slice);
            // A starred base (`*bases`) gives classes at a place among the bases heki does not
            // track, and a statement heki did not lower (in a function too large to model) bases
            // it does not know: an unknown base first.
            if values.len() < class.bases.len() {
                bases.push(UNKNOWN_BASE.to_owned());
            }
            bases.extend(values.iter().flatten().cloned());
        }
        let mut unknown_base = false;
        for (i, base) in bases.iter_mut().enumerate() {
            // A library class imported by another of its names is the class the catalog lists.
            if let Some(class) = kb.class_aliases.get(base.as_str()) {
                base.clone_from(class);
            }
            if base == UNKNOWN_BASE {
                *base = format!(
                    "{UNKNOWN_BASE} {}.{} {i}>",
                    decls.module_name, class.qualname
                );
                // A base heki names no class for has an unknown metaclass.
                unknown_base = true;
            }
        }
        let keyword = |name: &str| {
            class
                .keywords
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        let metaclass_text = keyword("metaclass");
        let metaclass = metaclass_text.and_then(|m| decls.resolve_dotted(m, false));
        // `type` is no name the module binds: it stays the builtin unless the module rebinds it.
        // A mapping of keywords heki does not read (`**options`) may pass `metaclass`.
        let custom_metaclass = unknown_base
            || keyword("**").is_some()
            || metaclass_text.is_some_and(|text| {
                !matches!(
                    metaclass.as_deref().unwrap_or(text),
                    "type" | "builtins.type" | "abc.ABCMeta"
                )
            });
        let frozen_keyword = keyword("frozen").and_then(bool_literal);
        let passes_keywords = class.keywords.iter().any(|(k, _)| k != "metaclass");
        let is_protocol = bases
            .iter()
            .any(|b| matches!(b.as_str(), "typing.Protocol" | "typing_extensions.Protocol"));
        if in_module_body && let Some(params) = class_type_params(class, &bases) {
            type_params.insert(&class.qualname, params);
        }
        let mut members: BTreeSet<String> = class.methods.iter().cloned().collect();
        // A name the body declares `global` / `nonlocal` is no member, even when assigned.
        let body = semantic
            .scopes
            .get(class.scope.0 as usize)
            .filter(|sc| sc.id == class.scope);
        members.extend(body.into_iter().flat_map(|sc| {
            sc.bindings
                .iter()
                .filter(|b| {
                    !sc.global_decls.contains(&b.name) && !sc.nonlocal_decls.contains(&b.name)
                })
                .map(|b| b.name.clone())
        }));
        if let Some(c) = decls.classes.get_mut(&class.qualname) {
            c.bases = bases;
            c.metaclass = metaclass;
            c.custom_metaclass = custom_metaclass;
            c.frozen_keyword = frozen_keyword;
            c.passes_keywords = passes_keywords;
            c.is_protocol = is_protocol;
            c.members = members;
        }
    }
    // Class-body annotations without a value, and what methods set or annotate on `self`.
    let mut param_attrs: FxHashMap<&str, BTreeMap<Box<str>, Option<u8>>> = FxHashMap::default();
    let mut note_param = |class: &str, attr: &str, annotation: &str| {
        let Some((class, params)) = type_params.get_key_value(class) else {
            return;
        };
        let Some(at) = params.iter().position(|p| p == unquote(annotation)) else {
            return;
        };
        let at = u8::try_from(at).ok();
        let attrs = param_attrs.entry(class).or_default();
        attrs
            .entry(Box::from(attr))
            .and_modify(|seen| {
                if *seen != at {
                    *seen = None;
                }
            })
            .or_insert(at);
    };
    for f in &input.ir.functions {
        if f.is_class_body {
            if let Some(c) = decls.classes.get_mut(&f.qualname) {
                for a in f
                    .local_annotations
                    .iter()
                    .filter(|a| !a.target.contains('.'))
                {
                    note_param(&f.qualname, &a.target, &a.annotation);
                    c.members.insert(a.target.clone());
                }
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
        for a in &f.local_annotations {
            let Some(attr) = a.target.strip_prefix(&prefix) else {
                continue;
            };
            if !attr.contains('.') {
                note_param(owner, attr, &a.annotation);
                c.members.insert(attr.to_owned());
            }
        }
        let self_vars = self_aliases(f, first.var);
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::AttrStore { obj, attr, .. } = &inst.kind
                && self_vars.contains(obj)
            {
                c.members.insert(attr.clone());
            }
        }
    }
    for (qualname, c) in &mut decls.classes {
        if let Some(attrs) = param_attrs.remove(qualname.as_str()) {
            c.param_attrs = (attrs.into_iter())
                .filter_map(|(attr, at)| Some((attr, at?)))
                .collect();
        }
    }
    class_properties(input, &mut decls);
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
    // Definitions sharing a qualname (a property's setter, conditional definitions) must agree.
    let mut returned_params: FxHashMap<&str, Option<HandedBack>> = FxHashMap::default();
    let mut returned_imports: FxHashMap<&str, Option<Box<str>>> = FxHashMap::default();
    let import_targets = import_statement_targets(input);
    for f in input
        .ir
        .functions
        .iter()
        .filter(|f| is_summarized_function(f))
    {
        let returned = returned_param(f, &decls);
        returned_params
            .entry(&f.qualname)
            .and_modify(|r| {
                if *r != returned {
                    *r = None;
                }
            })
            .or_insert(returned);
        let imported = returned_import(f, &decls, &import_targets);
        returned_imports
            .entry(&f.qualname)
            .and_modify(|r| {
                if *r != imported {
                    *r = None;
                }
            })
            .or_insert(imported);
    }
    // The return annotation of a property's getter (its setter shares the qualname).
    let mut getter_types: FxHashMap<&str, Ty> = FxHashMap::default();
    for f in &input.ir.functions {
        if is_summarized_function(f)
            && f.owner_class_qualname.is_some()
            && (f.decorators.iter()).any(|d| decorators::is(d, &decls, |k| k.getter))
            && let Some(t) = f
                .return_annotation
                .as_deref()
                .and_then(|a| decls.resolve_annotation_ty(a))
        {
            getter_types.entry(&f.qualname).or_insert(t);
        }
    }
    let captured_params = crate::infer::captured_params(input.ir);
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
            positional_only: count_u8(f.params.iter().filter(|p| p.pos_only)),
            required_positional: count_u8(
                f.params
                    .iter()
                    .take_while(|p| !p.kw_only && !p.is_vararg && !p.is_kwarg && !p.has_default),
            ),
            required_keyword_only: f
                .params
                .iter()
                .filter(|p| p.kw_only)
                .take(8)
                .enumerate()
                .filter(|(_, p)| !p.has_default)
                .fold(0, |mask, (j, _)| mask | 1 << j),
            named_params: count_u8(f.params.iter().filter(|p| !p.is_vararg && !p.is_kwarg)),
            has_varargs: f.params.iter().any(|p| p.is_vararg),
            has_kwargs: f.params.iter().any(|p| p.is_kwarg),
            defaults: f
                .params
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    let fact = crate::guards::literal_fact(p.default.as_ref()?)?;
                    Some((u8::try_from(i).ok()?, fact))
                })
                .collect(),
            is_generator: is_generator_function(f, &decls),
            called_as_defined: is_called_as_defined(f, &decls),
            is_property: getters_only[f.qualname.as_str()],
            readonly_params,
            mutable_params,
            transform: transforms[f.qualname.as_str()],
            offloaded_by: None,
            is_abstract: crate::is_abstract_method(&f.decorators),
            property_type: getter_types.get(f.qualname.as_str()).cloned(),
            returns_param: match &returned_params[f.qualname.as_str()] {
                Some(HandedBack::Param(param, attr)) => Some((*param, attr.clone())),
                _ => None,
            },
            forwards: match &returned_params[f.qualname.as_str()] {
                Some(HandedBack::Forwarded(forwards)) => Some(forwards.clone()),
                _ => None,
            },
            returned_import: returned_imports[f.qualname.as_str()].clone(),
            returns: None,
            state_of_self: false,
            captured_params: captured_params
                .get(f.qualname.as_str())
                .map(|c| Box::new(c.clone())),
        };
        decls.functions.insert(f.qualname.clone(), Box::new(decl));
    }
    readonly_metadata(input, &mut decls);
    decls.small_globals = input.ir.constants.displays.clone();
    decls.calls_setstate = stores::calls_setstate(input);
    decls.exports = stores::exported_names(input);
    let star_imports: Box<[String]> = stars.into_iter().map(|(_, module)| module).collect();
    decls.star_imports = (!star_imports.is_empty()).then_some(star_imports);
    let names = ImportNames {
        globals: &decls.imports,
        statements: &import_targets,
    };
    let (grown, rebound, getattr_loads) = foreign_grown_globals(input, &names);
    let path_changes = package_paths::path_changes(input, &decls, &names);
    (
        decls.grown_globals,
        decls.rebound_globals,
        decls.getattr_loads,
    ) = (grown, rebound, getattr_loads);
    decls.getattr_rebinds = input.ir.constants.getattr_rebinds;
    decls.path_changes = (!path_changes.is_empty()).then(|| Box::new(path_changes));
    attr_displays(input, &mut decls);
    init_containers::init_containers(input, &mut decls);
    chained_methods::chained_methods(input, &mut decls, kb);
    code_attrs(input, &mut decls);
    for (class, attr) in class_attrs_rebound(input, &decls, &import_targets) {
        if let Some(c) = decls.classes.get_mut(&class) {
            c.rebound_attrs.insert(attr);
        }
    }
    decls.str_constants = str_constants(input, &decls);
    decls.literal_constants = literal_constants(input, &decls);
    decls.file_constants = file_constants(input, &decls);
    decls.object_sentinels = sentinels::object_sentinels(input, &decls);
    sentinels::named_attrs(input, &mut decls);
    fields::init_fields(input, &mut decls);
    decls.copy_registrations = sentinels::copy_registrations(input, &decls);
    decls.abc_registrations = sentinels::abc_registrations(input, &decls);
    returns::record_return_facts(input, &mut decls, kb);
    decls.getattr_exports = getattr_exports::getattr_exports(input, &decls, kb).map(Box::new);
    decls.hook_factories = getattr_exports::hook_factories(input, &decls, kb).map(Box::new);
    decls
}

/// `ModuleDecls::grown_globals`: the loads of an imported module's global (`cfg.QUEUE`, or
/// `QUEUE` after `from cfg import QUEUE`) whose object a function may grow, and the globals
/// it stores into or deletes on an imported module (`cfg.QUEUE = []`, also through a call:
/// [`attr_calls`]), decided by the shared `heki_ir` facts ([`FnIndex::grown_vars`]); those
/// stores and deletes alone (`ModuleDecls::rebound_globals`); and the loads of an imported
/// module's `__getattr__` (`ModuleDecls::getattr_loads`).
fn foreign_grown_globals(
    input: &ModuleInput<'_>,
    imports: &ImportNames<'_>,
) -> (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>) {
    let mut out = BTreeSet::new();
    let mut rebound = BTreeSet::new();
    let mut getattr_loads = BTreeSet::new();
    if imports.globals.is_empty() && imports.statements.is_empty() {
        return (out, rebound, getattr_loads);
    }
    for f in &input.ir.functions {
        let ix = FnIndex::new(f, &input.ir.constants);
        let mut loads: Vec<(VarId, String)> = Vec::new();
        for (obj, attr) in attr_calls(input, &ix, imports) {
            if let Some(module) = imported_fqn(&ix, imports, obj, 0) {
                rebound.insert(format!("{module}.{attr}"));
            }
        }
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            match &inst.kind {
                InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr } => {
                    if let Some(module) = imported_fqn(&ix, imports, *obj, 0) {
                        rebound.insert(format!("{module}.{attr}"));
                    }
                }
                InstKind::MethodCall { recv, method, .. } if method == "__getattr__" => {
                    if let Some(module) = imported_fqn(&ix, imports, *recv, 0) {
                        getattr_loads.insert(format!("{module}.__getattr__"));
                    }
                }
                kind => {
                    if let Some(dst) = kind.dst()
                        && matches!(
                            kind,
                            InstKind::LoadGlobal { .. }
                                | InstKind::LoadName { .. }
                                | InstKind::Import { .. }
                                | InstKind::AttrLoad { .. }
                        )
                        && let Some(fqn) = imported_fqn(&ix, imports, dst, 0)
                    {
                        if fqn.ends_with(".__getattr__") {
                            getattr_loads.insert(fqn.clone());
                        }
                        loads.push((dst, fqn));
                    }
                }
            }
        }
        if loads.is_empty() {
            continue;
        }
        let grown = ix.grown_vars();
        out.extend(
            loads
                .into_iter()
                .filter(|(v, _)| grown.contains(v))
                .map(|(_, fqn)| fqn),
        );
    }
    out.extend(rebound.iter().cloned());
    (out, rebound, getattr_loads)
}

/// The attributes the function of `ix` stores or deletes by name through a call, by receiver
/// (`setattr(obj, "attr", v)`, `delattr(obj, "attr")`, `object.__setattr__(obj, "attr", v)`,
/// `obj.__setattr__("attr", v)`, also `builtins.setattr`), as [`stores::attr_op`] recognizes
/// them before inference: a builtin read by its name where the module does not bind it
/// (`imports` are the module's runtime imports), and a name given by a string literal.
/// `obj.attr = v` and `del obj.attr` are instructions of their own.
fn attr_calls<'f>(
    input: &ModuleInput<'_>,
    ix: &FnIndex<'f>,
    imports: &ImportNames<'_>,
) -> Vec<(VarId, String)> {
    const SPELLINGS: &[&str] = &["setattr", "delattr", "__setattr__", "__delattr__"];
    let calls: Vec<(&InstKind, VarId)> = ix
        .f
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| match &i.kind {
            InstKind::Call {
                callee,
                callee_var: Some(v),
                ..
            } if SPELLINGS.contains(&callee.rsplit('.').next().unwrap_or(callee)) => {
                Some((&i.kind, *v))
            }
            InstKind::MethodCall { method, recv, .. } if SPELLINGS.contains(&method.as_str()) => {
                Some((&i.kind, *recv))
            }
            _ => None,
        })
        .collect();
    if calls.is_empty() {
        return Vec::new();
    }
    let constants = &input.ir.constants;
    let mut globals = imports.globals.clone();
    for builtin in ["setattr", "delattr", "object"] {
        if !constants.dynamic && !constants.shadowed.contains(builtin) {
            globals
                .entry(builtin.to_owned())
                .or_insert_with(|| format!("builtins.{builtin}"));
        }
    }
    let names = ImportNames {
        globals: &globals,
        statements: imports.statements,
    };
    let vals: FxHashMap<VarId, Val> = calls
        .iter()
        .filter_map(|(_, v)| Some((*v, Val::Ref(imported_fqn(ix, &names, *v, 0)?))))
        .collect();
    let literal = |v: VarId| match ix.shape.inst(ix.shape.copy_root(v)).map(|i| &i.kind) {
        Some(InstKind::Const {
            literal: ConstLiteral::Str(s),
            ..
        }) => stores::AttrName::Named(Cow::Owned(s.clone())),
        _ => stores::AttrName::Unknown,
    };
    calls
        .iter()
        .filter_map(
            |(kind, _)| match stores::attr_op(kind, |v| vals.get(&v), literal)? {
                stores::AttrOp::Store {
                    obj,
                    name: stores::AttrName::Named(attr),
                    ..
                }
                | stores::AttrOp::Delete {
                    obj,
                    name: stores::AttrName::Named(attr),
                } => Some((obj, attr.into_owned())),
                _ => None,
            },
        )
        .collect()
}

/// What a module's runtime imports bind, for [`imported_fqn`].
struct ImportNames<'d> {
    /// The module-level names they bind once the module has run ([`ModuleDecls::imports`]).
    globals: &'d BTreeMap<String, String>,
    /// What each import statement binds, by its span: those of a function too, whose names are
    /// the function's own.
    statements: &'d FxHashMap<Span, String>,
}

/// What each runtime import statement of the module binds, by its span ([`import_target`]).
fn import_statement_targets(input: &ModuleInput<'_>) -> FxHashMap<Span, String> {
    (input.semantic.imports.iter())
        .filter(|imp| !imp.in_type_checking)
        .filter_map(|imp| Some((imp.span, import_target(imp, input)?)))
        .collect()
}

/// The fqn `v` names through this module's runtime imports (`cfg`, `cfg.QUEUE`), through
/// copies and attribute loads: a global read by the module-level name an import binds, and the
/// value of an import statement, of the module body or of a function (`import cfg` in a
/// function), by what that statement binds.
fn imported_fqn(
    ix: &FnIndex<'_>,
    imports: &ImportNames<'_>,
    v: VarId,
    depth: u32,
) -> Option<String> {
    if depth > 8 {
        return None;
    }
    let inst = ix.shape.inst(v)?;
    match &inst.kind {
        InstKind::Copy { src, .. } => imported_fqn(ix, imports, *src, depth + 1),
        InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
            imports.globals.get(name).cloned()
        }
        InstKind::Import { .. } => imports.statements.get(&inst.span).cloned(),
        InstKind::AttrLoad { obj, attr, .. } => {
            imported_fqn(ix, imports, *obj, depth + 1).map(|m| format!("{m}.{attr}"))
        }
        _ => None,
    }
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

/// External bases whose instances read the attributes of their subclasses as plain ones: a
/// `NamedTuple`'s fields are tuple items, and `Generic` adds only type parameters.
const PLAIN_BASES: &[&str] = &[
    "object",
    "builtins.object",
    // `abc.ABC` defines no attribute but empty `__slots__`, and its metaclass `abc.ABCMeta`
    // installs no descriptor.
    "abc.ABC",
    "typing.NamedTuple",
    "typing_extensions.NamedTuple",
    "typing.Generic",
    "typing_extensions.Generic",
];

/// The classes a type names: the class of an instance or of each member of a union, and those
/// of a generic container's items or a future's result (one level: `list[Model]`,
/// `dict[str, Model]`, `tuple[A, B]`, `list[A | B]`; `dict[str, list[Model]]` names `str` and
/// `list`).
fn ty_classes(ty: &Ty, out: &mut Vec<String>) {
    for obj in ty.members() {
        out.push(obj.class.clone());
        let items: Vec<&Elem> = match &obj.args {
            // The named objects a field holds are no classes of instances, nor is what calling
            // a callable gives, nor are the arguments of a class with type parameters, which only
            // the reads of its attributes take.
            Args::None | Args::Fields(_) | Args::Returns(_) | Args::TypeArgs(_) => Vec::new(),
            Args::Items(e) | Args::Result(e) => vec![e.as_ref()],
            Args::Map(k, v) => vec![k.as_ref(), v.as_ref()],
            Args::Tuple(es) => es.iter().collect(),
        };
        for item in items {
            if let Elem::Of(inner) = item {
                out.extend(inner.members().iter().map(|o| o.class.clone()));
            }
        }
    }
}

/// The methods that decide what a copy of `depth` makes of an instance in place of copying its
/// attributes ([`ProjectDecls::copy_of`]): `__deepcopy__` for `copy.deepcopy`, `__copy__` for
/// `copy.copy`, and the methods of pickling both fall back to.
fn copy_hooks(depth: CopyDepth) -> [&'static str; 5] {
    let own = match depth {
        CopyDepth::Deep => "__deepcopy__",
        CopyDepth::Shallow => "__copy__",
    };
    [
        own,
        "__reduce__",
        "__reduce_ex__",
        "__getstate__",
        "__setstate__",
    ]
}

/// The bases outside the project known to define none of the [`COPY_HOOKS`], `__getattr__`, or
/// `__getattribute__`, and no metaclass other than `type` / `abc.ABCMeta`.
const HOOKLESS_BASES: &[&str] = &[
    "object",
    "builtins.object",
    "typing.Generic",
    "typing_extensions.Generic",
    "abc.ABC",
];

/// The field specifiers whose call in a class body declares a field and leaves no descriptor.
const FIELD_SPECIFIERS: &[&str] = &[
    "field",
    "dataclasses.field",
    "attr.ib",
    "attr.attrib",
    "attr.field",
    "attrs.field",
];

/// `ClassDecl::code_attrs` of the module's classes, from their bodies.
fn code_attrs(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    for f in input.ir.functions.iter().filter(|f| f.is_class_body) {
        // The semantic model declares every class the IR has (one AST).
        if let Some(class) = decls.classes.get_mut(&f.qualname) {
            class.code_attrs = class_body_code_attrs(f);
        }
    }
}

/// The names a class body binds to something that may run code when read on an instance.
fn class_body_code_attrs(f: &IrFunction) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    {
        let shape = heki_ir::walk::FnShape::new(f);
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            let Some(attr) = inst
                .kind
                .dst()
                .and_then(|d| f.var_name(d))
                .filter(|n| !n.starts_with('.'))
            else {
                continue;
            };
            let value = match &inst.kind {
                InstKind::Copy { src, .. } => shape.inst(shape.copy_root(*src)).map(|i| &i.kind),
                kind => Some(kind),
            };
            let plain = match value {
                Some(
                    InstKind::Const { .. }
                    | InstKind::AllocBuiltin { .. }
                    | InstKind::DictLiteral { .. },
                ) => true,
                Some(InstKind::Call { callee, .. }) => FIELD_SPECIFIERS.contains(&callee.as_str()),
                _ => false,
            };
            if !plain {
                out.insert(attr.to_owned());
            }
        }
    }
    out
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

/// [`ModuleDecls::str_constants`]: the module-level names bound exactly once in the module and
/// the class-body names declared `ClassVar` or `Final` (`typing` / `typing_extensions`, also
/// subscripted) bound exactly once in the class body, each to a string literal or to another
/// name (`constant_source`), and the module-level names bound to a tuple display of those
/// (`tuple_source`). A module-level literal is the text of one of the module's constants
/// (`heki_ir::ModuleConstants::strs`), shared with it, and a name those leave out is no
/// constant; a module-level name bound to another module's constant is one only where the
/// module's IR fixes it too (`heki_ir::ModuleConstants::imported`: not in a loop, before a star
/// import, or where `exec` or `globals()` may rebind it). A name stored twice (`global KEY` in a function, two branches) or deleted is no
/// constant, and neither is a name an import statement binds: it resolves through the imports.
/// A class attribute the module also stores or deletes outside the class body
/// (`ClassDecl::rebound_attrs`) is none either.
fn str_constants(input: &ModuleInput<'_>, decls: &ModuleDecls) -> BTreeMap<String, StrConstant> {
    let resolve = |name: &str| decls.resolve_dotted(name, true);
    // A module body reads an imported name as a copy of its `Import`.
    let imports: FxHashMap<Span, String> = input
        .semantic
        .imports
        .iter()
        .filter_map(|imp| Some((imp.span, import_target(imp, input)?)))
        .collect();
    let module_stores = GlobalStores::of(&input.ir.functions);
    let fixed_import = |name: &str| input.ir.constants.imported.contains(name);
    let mut out = BTreeMap::new();
    for f in input
        .ir
        .functions
        .iter()
        .filter(|f| f.is_module_body || f.is_class_body)
    {
        let declared: FxHashMap<&str, Qualifiers> = f
            .local_annotations
            .iter()
            .filter(|a| !a.target.contains('.'))
            .map(|a| (a.target.as_str(), qualifiers(&a.annotation, &resolve).0))
            .collect();
        let insts = || f.blocks.iter().flat_map(|b| &b.insts);
        // A module body binds a global by storing it; a class body binds a name as a named copy.
        let bindings: Vec<(&str, VarId)> = insts()
            .filter_map(|inst| match &inst.kind {
                InstKind::StoreGlobal { name, value } if f.is_module_body => {
                    Some((name.as_str(), *value))
                }
                InstKind::Copy { dst, src } if f.is_class_body => Some((f.var_name(*dst)?, *src)),
                _ => None,
            })
            .collect();
        let mut class_stores: FxHashMap<&str, usize> = FxHashMap::default();
        if f.is_class_body {
            for (name, _) in &bindings {
                *class_stores.entry(name).or_default() += 1;
            }
            for inst in insts() {
                if let InstKind::Delete { name, .. } = &inst.kind {
                    *class_stores.entry(name.as_str()).or_default() += 1;
                }
            }
        }
        let defs: FxHashMap<VarId, &Inst> =
            insts().filter_map(|i| Some((i.kind.dst()?, i))).collect();
        let shape = FnShape::new(f);
        for (name, value) in bindings {
            let q = declared.get(name).copied().unwrap_or_default();
            let (key, once, rebindable) = if f.is_module_body {
                let once = module_stores.once(name) && !decls.imports.contains_key(name);
                (name.to_owned(), once, !q.is_final)
            } else {
                let once = (q.class_var || q.is_final)
                    && class_stores.get(name) == Some(&1)
                    && !decls
                        .classes
                        .get(f.qualname.as_str())
                        .is_some_and(|c| c.rebound_attrs.contains(name));
                (format!("{}.{name}", f.qualname), once, !q.is_final)
            };
            if !once {
                continue;
            }
            let value = match constant_source(&defs, &imports, value) {
                // A module-level name holds the text of one of the module's constants, or no
                // constant (one bound in a loop, or that a star import or `exec` may rebind).
                Some(Ok(_)) if f.is_module_body => match input.ir.constants.strs.get(name) {
                    Some(text) => StrValue::Literal(Arc::clone(text)),
                    None => continue,
                },
                Some(Ok(s)) => StrValue::Literal(Arc::from(s)),
                // As for a literal, the module's IR decides whether the name is fixed.
                Some(Err(_)) if f.is_module_body && !fixed_import(name) => continue,
                Some(Err(fqn)) => StrValue::Alias(fqn),
                None if f.is_module_body => {
                    let fixed = input.ir.constants.str_tuples.get(name);
                    match tuple_source(&shape, &defs, &imports, value, fixed, fixed_import(name)) {
                        Some(items) => StrValue::Tuple(items),
                        None => continue,
                    }
                }
                None => continue,
            };
            let constant = StrConstant {
                value,
                rebindable,
                declares_field: f.is_class_body && q.is_final && !q.class_var,
            };
            out.insert(key, constant);
        }
    }
    out
}

/// [`ModuleDecls::literal_constants`]: the number and boolean constants of the module's IR
/// (`heki_ir::ModuleConstants`: names its body stores once, outside a loop, that nothing deletes
/// and no star import or `exec` may rebind), the one view of a module name bound once that the
/// IR's facts read too. A name not declared `Final` may still be rebound by another module.
fn literal_constants(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
) -> BTreeMap<String, LiteralConstant> {
    let resolve = |name: &str| decls.resolve_dotted(name, true);
    let finals: FxHashSet<&str> = input
        .ir
        .functions
        .iter()
        .filter(|f| f.is_module_body)
        .flat_map(|f| &f.local_annotations)
        .filter(|a| qualifiers(&a.annotation, &resolve).0.is_final)
        .map(|a| a.target.as_str())
        .collect();
    let constants = &input.ir.constants;
    let ints = constants
        .ints
        .iter()
        .map(|(name, n)| (name, ConstLiteral::Int(*n)));
    // A dotted key (`t.TYPE_CHECKING`) is no module name.
    let bools = constants
        .bools
        .iter()
        .filter(|(name, _)| !name.contains('.'))
        .map(|(name, b)| (name, ConstLiteral::Bool(*b)));
    let floats = constants
        .floats
        .iter()
        .map(|(name, text)| (name, ConstLiteral::Float(text.clone())));
    ints.chain(bools)
        .chain(floats)
        .map(|(name, value)| {
            let rebindable = !finals.contains(name.as_str());
            (name.clone(), LiteralConstant { value, rebindable })
        })
        .collect()
}

/// Whether heki takes `name` to be defined by `base`, a class outside the project: a special
/// method (`__init__`, `__copy__`, ...), whose meaning the protocol fixes; a public method of a
/// builtin type other than `object` (`dict.update`); or a member the ecosystem knowledge base
/// lists (`pydantic.BaseModel.model_copy`). Any other member (`abc.ABC._copy_model_type`,
/// `typing.Generic.result_type`) is not known to be there.
fn external_defines(kb: &EffectiveEcosystemKb, base: &str, name: &str) -> bool {
    let special = name.len() > 4 && name.starts_with("__") && name.ends_with("__");
    let builtin_method = base
        .strip_prefix("builtins.")
        .is_some_and(|t| t != "object" && !name.starts_with('_'));
    special || builtin_method || kb.lookup(&format!("{base}.{name}")).is_some()
}

/// What every `return` of one definition hands back of its parameters ([`handed_back`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum HandedBack {
    /// The same parameter, or the same attribute of it ([`FunctionDecl::returns_param`]).
    Param(u8, Option<String>),
    /// That, or the result of calls ([`FunctionDecl::forwards`]).
    Forwarded(Box<ForwardedReturn>),
}

/// [`FunctionDecl::returns_param`] and [`FunctionDecl::forwards`] of one definition: a function
/// (not a generator or a coroutine, decorated by nothing but the decorators that return what the
/// body returns on every call, [`decorators::keeps_returned_value`]: `property` and those that
/// keep the call) whose every `return` returns the same positional parameter, or the same
/// attribute of it, read directly or through what hands a value back as itself (local copies,
/// `typing.cast`, [`ownership::handed_back`]), and which stores or deletes no attribute of that
/// name; or some return the result of a call passing parameters ([`ForwardingCall`]). A
/// `cached_property` is none: a read after the first returns what the instance holds, which an
/// assignment can replace.
fn returned_param(f: &IrFunction, decls: &ModuleDecls) -> Option<HandedBack> {
    (f.kind == FunctionKind::Sync)
        .then(|| handed_back(f, decls))
        .flatten()
}

/// What every `return` of one definition, a function or a coroutine function, hands back of its
/// parameters, as [`returned_param`] decides it without excluding a coroutine: awaiting the
/// coroutine gives what the body returns (`ReturnFact::self_attr`).
fn handed_back_param(f: &IrFunction, decls: &ModuleDecls) -> Option<(u8, Option<String>)> {
    match handed_back(f, decls)? {
        HandedBack::Param(param, attr) => Some((param, attr)),
        HandedBack::Forwarded(_) => None,
    }
}

/// [`handed_back_param`], with the returns giving a call's result ([`HandedBack::Forwarded`]).
fn handed_back(f: &IrFunction, decls: &ModuleDecls) -> Option<HandedBack> {
    if !matches!(f.kind, FunctionKind::Sync | FunctionKind::Async)
        || !decorators::keeps_returned_value(&f.decorators, decls)
    {
        return None;
    }
    let defs: FxHashMap<VarId, &InstKind> = f
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| Some((i.kind.dst()?, &i.kind)))
        .collect();
    // The variable each value is handed back from in the end, found once per variable: returns
    // of one value share the walk. Copies in dead code can close a cycle, where the walk ends.
    let mut roots: FxHashMap<VarId, VarId> = FxHashMap::default();
    let mut handed_back_from = |var: VarId| {
        let mut walked = Vec::new();
        let mut cur = var;
        let root = loop {
            if let Some(&root) = roots.get(&cur) {
                break root;
            }
            match defs
                .get(&cur)
                .and_then(|kind| decl_same_object(kind, &defs, decls))
            {
                Some(src) if walked.len() <= defs.len() => {
                    walked.push(cur);
                    cur = src;
                }
                _ => break cur,
            }
        };
        for v in walked {
            roots.insert(v, root);
        }
        root
    };
    let param_of = |var: VarId| {
        let i = f
            .params
            .iter()
            .position(|p| p.var == var && !p.is_vararg && !p.is_kwarg && !p.kw_only)?;
        u8::try_from(i).ok()
    };
    let mut returned = None;
    let mut calls = BTreeSet::new();
    // The values a return gives that may be an attribute (an attribute's load, a call's result),
    // with the block of the return.
    let mut attr_reads: Vec<(VarId, BlockId)> = Vec::new();
    for bb in &f.blocks {
        let Terminator::Return(value) = &bb.terminator else {
            continue;
        };
        let root = handed_back_from((*value)?);
        let def = defs.get(&root).copied();
        let mut param = |var: VarId| param_of(handed_back_from(var));
        if let Some(call) = def.and_then(|kind| ForwardingCall::of(kind, &defs, decls, &mut param))
        {
            calls.insert(call);
            attr_reads.push((root, bb.id));
            continue;
        }
        let this = match def {
            Some(InstKind::AttrLoad { obj, attr, .. }) => {
                let param = param_of(handed_back_from(*obj))?;
                attr_reads.push((root, bb.id));
                (param, Some(attr.clone()))
            }
            _ => (param_of(root)?, None),
        };
        if returned.get_or_insert_with(|| this.clone()) != &this {
            return None;
        }
    }
    // A function storing or deleting the attribute (`items = self.items; self.items = [];
    // return items`) may return what the attribute held before: no longer the attribute.
    let stores = |attr: Option<&str>| {
        f.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| match &i.kind {
                InstKind::AttrStore { attr: a, .. } | InstKind::AttrDelete { attr: a, .. } => {
                    attr.is_none_or(|attr| a == attr)
                }
                _ => false,
            })
    };
    // So may a function running code between the read and the return (`old = items_of(s);
    // reset(s); return old`), which may store it; `return items_of(s)` runs none.
    attr_reads.sort_unstable();
    let after_read = OnceCell::new();
    let rebinds_after_read =
        || *after_read.get_or_init(|| rebinds_after_reads(f, &defs, decls, &attr_reads));
    let returned_attr = returned.as_ref().and_then(|(_, attr)| attr.as_deref());
    if returned_attr.is_some_and(|a| stores(Some(a)) || rebinds_after_read()) {
        return None;
    }
    if calls.is_empty() {
        let (param, attr) = returned?;
        return Some(HandedBack::Param(param, attr));
    }
    let rebinds_attrs = stores(None) || rebinds_after_read();
    Some(HandedBack::Forwarded(Box::new(ForwardedReturn {
        direct: returned,
        calls: calls.into_iter().collect(),
        rebinds_attrs,
    })))
}

/// The owner of the budget of [`rebinds_after_reads`].
const AFTER_READS_OWNER: &str = "heki_effect::resolver::rebinds_after_reads";

/// Whether `f` may rebind an attribute ([`may_rebind_attrs`]) on some path from one of `reads`
/// (a value and the block returning it, in the order of the values) to that return: after the
/// value's instruction in its
/// block, and in the blocks walked back from the return up to that block, which a value's
/// definition dominates. The returns of one value share a walk; past a budget proportional to
/// `f`, it may.
fn rebinds_after_reads(
    f: &IrFunction,
    defs: &FxHashMap<VarId, &InstKind>,
    decls: &ModuleDecls,
    reads: &[(VarId, BlockId)],
) -> bool {
    let rebinds = |insts: &[Inst]| (insts.iter()).any(|i| may_rebind_attrs(&i.kind, defs, decls));
    if !f.blocks.iter().any(|b| rebinds(&b.insts)) {
        return false;
    }
    let shape = FnShape::new(f);
    let budget = Budget::of_function(AFTER_READS_OWNER, f);
    // Each value is an instruction's result (an attribute load, a call).
    let read_at: FxHashMap<VarId, (BlockId, usize)> = (f.blocks.iter())
        .flat_map(|b| (b.insts.iter().enumerate()).map(move |(i, inst)| (b.id, i, inst)))
        .filter_map(|(b, i, inst)| Some((inst.kind.dst()?, (b, i))))
        .filter(|(v, _)| reads.binary_search_by_key(v, |(read, _)| *read).is_ok())
        .collect();
    (reads.chunk_by(|a, b| a.0 == b.0)).any(|returns| {
        let (read_block, i) = read_at[&returns[0].0];
        let insts = &f.block(read_block).insts;
        if !budget.take(insts.len()) || rebinds(&insts[i + 1..]) {
            return true;
        }
        let starts = (returns.iter()).filter_map(|(_, ret)| (*ret != read_block).then_some(*ret));
        let next = |b: BlockId, out: &mut Vec<BlockId>| out.extend_from_slice(shape.preds(b));
        let walked = reach(&budget, starts, next, |b| {
            let insts = &f.block(b).insts;
            if b == read_block {
                Visit::Prune
            } else if !budget.take(insts.len()) || rebinds(insts) {
                Visit::Stop(())
            } else {
                Visit::Expand
            }
        });
        walked != Answer::Known(None)
    })
}

/// Whether the instruction `kind` of a function of the module `decls` may rebind an attribute of
/// an object it does not name: a call (but one handing its argument back,
/// [`decl_same_object`]), which may store one, directly or through what it calls; the entry or
/// the exit of a `with` block, which calls the manager's `__enter__` or `__exit__` (`__aenter__`
/// or `__aexit__`, awaited); or an `await`, while which other code runs.
fn may_rebind_attrs(
    kind: &InstKind,
    defs: &FxHashMap<VarId, &InstKind>,
    decls: &ModuleDecls,
) -> bool {
    matches!(
        kind,
        InstKind::Call { .. }
            | InstKind::MethodCall { .. }
            | InstKind::EnterWith { .. }
            | InstKind::ExitWith { .. }
            | InstKind::Await { .. }
    ) && decl_same_object(kind, defs, decls).is_none()
}

/// The value whose object the instruction `kind` of a function of the module `decls` describes
/// gives as itself, as the declarations see it: a copy, or a call [`ownership::handed_back`]
/// knows whose callee is an import of the module nothing rebinds (`cast(...)` after
/// `from typing import cast`, `t.cast(...)` after `import typing as t`).
fn decl_same_object(
    kind: &InstKind,
    defs: &FxHashMap<VarId, &InstKind>,
    decls: &ModuleDecls,
) -> Option<VarId> {
    let imported = |var: &VarId| match defs.get(var) {
        Some(InstKind::LoadGlobal { name, .. }) => decls.fixed_import(name),
        _ => None,
    };
    match kind {
        InstKind::Copy { src, .. } => Some(*src),
        InstKind::Call {
            callee_var: Some(c),
            args,
            star_from,
            ..
        } => ownership::handed_back(imported(c)?, args, *star_from),
        InstKind::MethodCall {
            recv,
            method,
            args,
            star_from,
            ..
        } => ownership::handed_back(&format!("{}.{method}", imported(recv)?), args, *star_from),
        _ => None,
    }
}

/// [`ModuleDecls::unknown_globals`]: the globals the module stores or deletes (`stores`) that are
/// no runtime import, class, or function of it.
/// [`FunctionDecl::returned_import`] of `f`: every `return` gives what one import binds, an
/// import of the function (`import_targets`, by the span of the import) or a module-level one
/// nothing rebinds. The function's decorators keep what its body returns: those that keep
/// calling it as defined, and a `functools` cache, which hands out the object a call returned.
fn returned_import(
    f: &IrFunction,
    decls: &ModuleDecls,
    import_targets: &FxHashMap<Span, String>,
) -> Option<Box<str>> {
    if !matches!(f.kind, FunctionKind::Sync) || is_generator_function(f, decls) {
        return None;
    }
    if !keeps_body_result(f, decls) {
        return None;
    }
    let shape = FnShape::new(f);
    let mut out: Option<String> = None;
    for bb in &f.blocks {
        let Terminator::Return(value) = bb.terminator else {
            continue;
        };
        let imported = |var: VarId| {
            let inst = shape.inst(shape.copy_root(var))?;
            match &inst.kind {
                InstKind::Import { .. } => import_targets.get(&inst.span),
                InstKind::LoadGlobal { name, .. } => decls.fixed_import(name),
                _ => None,
            }
        };
        let root = shape.copy_root(value?);
        let target = match shape.inst(root).map(|i| &i.kind) {
            // `import pkg.ops` binds `pkg`; `return pkg.ops` gives the submodule.
            Some(InstKind::AttrLoad { obj, attr, .. }) => format!("{}.{attr}", imported(*obj)?),
            _ => imported(root)?.clone(),
        };
        if out.as_ref().is_some_and(|o| *o != target) {
            return None;
        }
        out = Some(target);
    }
    out.map(Box::from)
}

/// Calling `f` gives what its body returns: its decorators keep calling it as defined, or are a
/// `functools` cache, which hands out the object a call returned.
fn keeps_body_result(f: &IrFunction, decls: &ModuleDecls) -> bool {
    f.decorators.iter().all(|d| {
        decorators::is(d, decls, |k| k.keeps_call)
            || matches!(
                decls.decorator_symbol(d).as_deref(),
                Some("functools.cache" | "functools.lru_cache")
            )
    })
}

/// [`ModuleDecls::type_checking_only`]: the names whose every module-level store is one of the
/// module's `if TYPE_CHECKING:` imports, each of which stores the name once.
fn type_checking_only(
    semantic: &SemanticModel,
    stores: &GlobalStores<'_>,
    decls: &ModuleDecls,
) -> BTreeSet<String> {
    let module_scope = module_scope(semantic);
    let mut imported: BTreeMap<&str, u32> = BTreeMap::new();
    for imp in &semantic.imports {
        if imp.in_type_checking && Some(imp.scope) == module_scope {
            *imported.entry(&imp.bound_name).or_default() += 1;
        }
    }
    imported
        .into_iter()
        .filter(|(name, times)| {
            decls.type_checking_imports.contains_key(*name) && stores.exactly(name, *times)
        })
        .map(|(name, _)| name.to_owned())
        .collect()
}

fn unknown_globals(stores: &GlobalStores<'_>, decls: &ModuleDecls) -> BTreeSet<String> {
    stores
        .names()
        .filter(|name| {
            !decls.imports.contains_key(*name)
                && !decls.classes.contains_key(*name)
                && !decls.functions.contains_key(*name)
        })
        .map(str::to_owned)
        .collect()
}

/// [`ModuleDecls::file_constants`], in the order the module body binds them: a constant may be
/// built from one bound before it (`DB_PATH = DATA_DIR / "app.db"`).
fn file_constants(input: &ModuleInput<'_>, decls: &ModuleDecls) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if input.ir.constants.dynamic || input.ir.has_star_import {
        return out;
    }
    let body = input.ir.function(input.ir.module_fn);
    let stores = GlobalStores::of(&input.ir.functions);
    let shape = FnShape::new(body);
    let imports: FxHashMap<Span, String> = input
        .semantic
        .imports
        .iter()
        .filter(|imp| !imp.in_type_checking)
        .filter_map(|imp| Some((imp.span, import_target(imp, input)?)))
        .collect();
    let name_of = |v: VarId| match module_read(&shape, &imports, v)? {
        ModuleRead::Name(text) => decls.resolve_dotted(&text, true),
        ModuleRead::Imported(fqn) => Some(fqn),
    };
    fn loaded(inst: &Inst) -> Option<&str> {
        match &inst.kind {
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => Some(name),
            _ => None,
        }
    }
    // A string constant is bound once to a literal before it is read: one naming a file is
    // already among `out`, and any other names a database in memory.
    let globals = |_: &Inst| None;
    for bb in body.blocks.iter().filter(|b| shape.loop_depth(b.id) == 0) {
        for inst in &bb.insts {
            let InstKind::StoreGlobal { name, value } = &inst.kind else {
                continue;
            };
            let files = |inst: &Inst| loaded(inst).is_some_and(|n| out.contains(n));
            let names_file = stores.once(name)
                && names_a_file(&|v| shape.inst(v), &name_of, &globals, &files, *value);
            if names_file {
                out.insert(name.clone());
            }
        }
    }
    out
}

/// [`ModuleDecls::rebound_imports`]: the import's own store is one of the module's stores of
/// the name (`stores`).
fn rebound_imports(
    input: &ModuleInput<'_>,
    stores: &GlobalStores<'_>,
    decls: &ModuleDecls,
) -> BTreeSet<String> {
    let unfixed = input.ir.constants.dynamic || input.ir.has_star_import;
    let rebound = |name: &str| unfixed || (stores.binds(name) && !stores.once(name));
    decls
        .imports
        .keys()
        .filter(|name| rebound(name))
        .cloned()
        .collect()
}

/// [`ModuleDecls::aliases`]: the module-level names bound exactly once besides the imports
/// that binding always replaces (`from . import parse`, then `parse = parse.parse`), to a name
/// the module reads as another object at run time (an import, a builtin, a class or function
/// the module defines) or to an attribute of one (`object.__setattr__`, `helpers.run`). A
/// name bound by an import that stays (also one under `if TYPE_CHECKING:`), bound in two
/// other places, or deleted is none, and neither is one bound to anything else (a call's
/// result, a decorated function, a choice between two objects).
///
/// A class or function the module body binds again ([`definitions_bound_again`]) is an alias of
/// what binds it last on every path, and [`ModuleDecls::alias_paths`] when that differs by
/// path.
fn module_aliases(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
    bindings: &BodyBindings<'_>,
) -> (BTreeMap<String, String>, BTreeMap<String, Vec<String>>) {
    let module = module_scope(input.semantic);
    let module_imports = || {
        input
            .semantic
            .imports
            .iter()
            .filter(move |imp| Some(imp.scope) == module)
    };
    // What a runtime import binds; an `if TYPE_CHECKING:` import binds nothing at run time.
    let imports: FxHashMap<Span, String> = module_imports()
        .filter(|imp| !imp.in_type_checking)
        .filter_map(|imp| Some((imp.span, import_target(imp, input)?)))
        .collect();
    // The number of imports binding each name, when a later statement always rebinds every one
    // of them at run time (`ModuleImport::rebound_at`).
    let mut replaced: FxHashMap<&str, Option<u32>> = FxHashMap::default();
    for imp in module_imports() {
        let count = replaced.entry(imp.bound_name.as_str()).or_insert(Some(0));
        *count = count
            .filter(|_| imp.rebound_at.is_some() && !imp.in_type_checking)
            .map(|n| n + 1);
    }
    // A star import may bind any name the module reads: then none is known to be a builtin.
    let builtins: FxHashSet<&str> = input
        .semantic
        .scopes
        .iter()
        .filter(|sc| sc.kind == ScopeKind::Module)
        .flat_map(|sc| &sc.reads)
        .filter(|(_, how)| *how == heki_semantic::NameResolution::Builtin)
        .map(|(name, _)| name.as_str())
        .collect();
    let stores = GlobalStores::of(&input.ir.functions);
    let insts = || {
        input
            .ir
            .functions
            .iter()
            .filter(|f| f.is_module_body)
            .flat_map(|f| f.blocks.iter().flat_map(|b| &b.insts))
    };
    let defs: FxHashMap<VarId, &Inst> = insts().filter_map(|i| Some((i.kind.dst()?, i))).collect();
    let named = Named {
        input,
        decls,
        defs: &defs,
        imports: &imports,
        builtins: &builtins,
    };
    let mut known = FxHashMap::default();
    let mut out = BTreeMap::new();
    for inst in insts() {
        let InstKind::StoreGlobal { name, value } = &inst.kind else {
            continue;
        };
        // An import's own store is no alias; the store replacing every import is.
        if imports.contains_key(&inst.span) {
            continue;
        }
        let Some(imports_replaced) = replaced.get(name.as_str()).copied().unwrap_or(Some(0)) else {
            continue;
        };
        if !stores.exactly(name, imports_replaced + 1) {
            continue;
        }
        let Some(target) = named.object(*value, &mut known) else {
            continue;
        };
        // `def f` and `class C` bind their own name.
        if target != format!("{}.{name}", decls.module_name) {
            out.insert(name.clone(), target);
        }
    }
    let mut paths = BTreeMap::new();
    for (name, objects) in
        definitions_bound_again(input, decls, bindings, &stores, &named, &mut known)
    {
        match <[String; 1]>::try_from(objects) {
            Ok([target]) => {
                if target != format!("{}.{name}", decls.module_name) {
                    out.insert(name.to_owned(), target);
                }
            }
            Err(objects) => {
                paths.insert(name.to_owned(), objects);
            }
        }
    }
    (out, paths)
}

/// The module-level classes and functions the module body binds again (`def fetch(...)`, then
/// `fetch = _quiet`), each with the objects it may hold once the module has run (sorted, the
/// definition as its own fqn): what the bindings that may be the last one bind
/// ([`LastBindings::last_of`]; a path leaving the name unbound raises where it is called), where
/// running a scope that binds the definition under `global` binds the definition itself
/// ([`BodyBindings`]). A name another function stores or deletes, an import binds, or one of
/// those bindings binds to anything but a named object is none, and so is every name where a
/// star import, `exec`, or `globals()` may bind any. A definition of the module among the
/// objects is the object its statement made, whatever its name holds later
/// ([`ProjectDecls::named_on_paths`]).
fn definitions_bound_again<'i>(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
    bindings: &BodyBindings<'i>,
    stores: &GlobalStores<'_>,
    named: &Named<'_>,
    known: &mut FxHashMap<VarId, Option<String>>,
) -> Vec<(&'i str, Vec<String>)> {
    let mut out = Vec::new();
    if input.ir.constants.dynamic || input.ir.has_star_import {
        return out;
    }
    let candidates = bindings.by_name.iter().filter(|(name, binds)| {
        binds.at.len() > 1
            && stores.exactly(name, binds.stores)
            && !decls.imports.contains_key(**name)
    });
    for (name, binds) in candidates {
        let (reaching, _) = bindings.last().last_of(&bindings.shape, &binds.at);
        let objects: Option<Vec<String>> = reaching
            .iter()
            .map(|&i| match binds.values[i] {
                Some(value) => named.object(value, known),
                None => Some(format!("{}.{name}", decls.module_name)),
            })
            .collect();
        let Some(mut objects) = objects else { continue };
        objects.sort();
        objects.dedup();
        if (1..=crate::types::MAX_NAMED).contains(&objects.len()) {
            out.push((*name, objects));
        }
    }
    out.sort();
    out
}

/// Where the module body binds each of the module's classes and functions: its stores of the
/// name, and where it runs a scope binding the name's definition on every path through it, by
/// its `class` or `def` statement under a `global` declaration of the name, the scope's one
/// store of the name ([`global_binders`]): a call of a module-level function (`_define()`
/// running `global Config`, then `class Config: ...`), or the `class` statement of a class
/// body.
struct BodyBindings<'i> {
    shape: FnShape<'i>,
    /// Built on first use: most modules bind no definition again.
    last: OnceCell<LastBindings>,
    by_name: FxHashMap<&'i str, NameBindings>,
}

/// The module body's bindings of one name ([`BodyBindings`]).
#[derive(Default)]
struct NameBindings {
    /// Each binding's block and start, in the module body's order.
    at: Vec<(BlockId, u32)>,
    /// What each binding binds: a store's value, or `None` for a scope binding the definition.
    values: Vec<Option<VarId>>,
    /// The stores of the name the bindings run: the module body's own, and one for each scope
    /// they run.
    stores: u32,
}

impl<'i> BodyBindings<'i> {
    fn of(input: &'i ModuleInput<'_>, decls: &ModuleDecls) -> Self {
        let body = input.ir.function(input.ir.module_fn);
        let shape = FnShape::new(body);
        let defines =
            |name: &str| decls.classes.contains_key(name) || decls.functions.contains_key(name);
        let binders = global_binders(input, decls);
        let mut by_name: FxHashMap<&'i str, NameBindings> = FxHashMap::default();
        let mut runs: FxHashSet<(FunctionId, &str)> = FxHashSet::default();
        for bb in &body.blocks {
            for inst in &bb.insts {
                let mut bind = |name: &'i str, value: Option<VarId>, stores: u32| {
                    let binds = by_name.entry(name).or_default();
                    binds.at.push((bb.id, inst.span.start));
                    binds.values.push(value);
                    binds.stores += stores;
                };
                // The binder the instruction runs: a function it calls, or a class body its
                // `class` statement runs.
                let runs_binder = match &inst.kind {
                    InstKind::StoreGlobal { name, value } if defines(name) => {
                        bind(name, Some(*value), 1);
                        continue;
                    }
                    _ if binders.is_empty() => continue,
                    InstKind::Call {
                        callee_var: Some(callee),
                        ..
                    } => match shape.inst(shape.copy_root(*callee)).map(|i| &i.kind) {
                        Some(InstKind::MakeClosure {
                            func_id: Some(id), ..
                        }) if !input.ir.function(*id).is_class_body => *id,
                        _ => continue,
                    },
                    InstKind::MakeClosure {
                        func_id: Some(id), ..
                    } if input.ir.function(*id).is_class_body => *id,
                    _ => continue,
                };
                for &name in binders.get(&runs_binder).into_iter().flatten() {
                    bind(name, None, u32::from(runs.insert((runs_binder, name))));
                }
            }
        }
        Self {
            shape,
            last: OnceCell::new(),
            by_name,
        }
    }

    fn last(&self) -> &LastBindings {
        self.last.get_or_init(|| LastBindings::of(&self.shape))
    }

    /// Where the module body's first binding of `name` starts, in source order.
    fn first_binding(&self, name: &str) -> Option<u32> {
        (self.by_name.get(name)?.at.iter())
            .map(|&(_, start)| start)
            .min()
    }

    /// Whether the module body binds `name` on every path ([`LastBindings::always_runs`]).
    fn always_binds(&self, name: &str) -> bool {
        self.by_name.get(name).is_some_and(|binds| {
            (binds.at.iter()).any(|&(block, _)| self.last().always_runs(&self.shape, block))
        })
    }
}

/// The scopes the module body may run that bind definitions of the module under `global` on
/// every path through them, with the names they bind: a module-level function the module body
/// can call (a plain `def`: no decorator, which could replace it, and no generator or coroutine
/// function, whose call runs nothing of its body) or a class body of a module-level `class`
/// statement, storing the name once, by the `class` or `def` statement of a definition with
/// that qualname ([`ModuleDecls::made_in`]), in a block every path through it to a return runs.
fn global_binders<'i>(
    input: &'i ModuleInput<'_>,
    decls: &ModuleDecls,
) -> FxHashMap<FunctionId, Vec<&'i str>> {
    let mut out: FxHashMap<FunctionId, Vec<&'i str>> = FxHashMap::default();
    let Some(made_in) = decls.made_in.as_deref() else {
        return out;
    };
    let makers: FxHashSet<&str> = made_in.values().map(String::as_str).collect();
    let binders = input.ir.functions.iter().filter(|f| {
        f.enclosing == Some(input.ir.module_fn)
            && makers.contains(f.qualname.as_str())
            && (f.is_class_body
                || (matches!(f.kind, FunctionKind::Sync) && f.decorators.is_empty()))
    });
    for f in binders {
        // Each global's stores in `f`, with a deletion as a store of no value.
        let mut stores: FxHashMap<&str, Vec<(BlockId, Option<VarId>)>> = FxHashMap::default();
        for bb in &f.blocks {
            for inst in &bb.insts {
                let (name, value) = match &inst.kind {
                    InstKind::StoreGlobal { name, value } => (name, Some(*value)),
                    InstKind::Delete {
                        name, global: true, ..
                    } => (name, None),
                    _ => continue,
                };
                stores.entry(name).or_default().push((bb.id, value));
            }
        }
        let shape = FnShape::new(f);
        let last = LastBindings::of(&shape);
        let makes = |value: VarId| match shape.inst(value).map(|i| &i.kind) {
            Some(InstKind::MakeClosure {
                func_id: Some(id), ..
            }) => Some(input.ir.function(*id).qualname.as_str()),
            _ => None,
        };
        let bound: Vec<&'i str> = stores
            .into_iter()
            .filter_map(|(name, at)| match at[..] {
                [(block, Some(value))]
                    if makes(value) == Some(name)
                        && made_in.get(name) == Some(&f.qualname)
                        && last.always_runs(&shape, block) =>
                {
                    Some(name)
                }
                _ => None,
            })
            .collect();
        if !bound.is_empty() {
            out.insert(f.id, bound);
        }
    }
    out
}

/// [`ModuleDecls::made_in`]: the definitions named as module-level ones (a `class` or a named
/// `def`) whose scope is made by a function or a class body, with its qualname.
fn made_in(input: &ModuleInput<'_>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for f in &input.ir.functions {
        let Some(maker) = f.enclosing.map(|id| input.ir.function(id)) else {
            continue;
        };
        if !maker.is_module_body && !f.qualname.contains('.') && !f.qualname.starts_with('<') {
            out.entry(f.qualname.clone())
                .or_insert_with(|| maker.qualname.clone());
        }
    }
    out
}

/// What the module body knows of its names, for [`module_aliases`].
struct Named<'n> {
    input: &'n ModuleInput<'n>,
    decls: &'n ModuleDecls,
    defs: &'n FxHashMap<VarId, &'n Inst>,
    /// `Import` instruction span -> bound fqn.
    imports: &'n FxHashMap<Span, String>,
    /// The names the module body reads as builtins.
    builtins: &'n FxHashSet<&'n str>,
}

impl Named<'_> {
    /// The fqn of the object the module-body variable `var` holds when it is a name or an
    /// attribute of one, or a builtin container class subscripted with types, an alias of the
    /// class (`Names = frozenset[str]`, [`crate::types::runtime_generic`]). Each variable is
    /// resolved once (`known`): a chain of aliases bound to one another (`a1 = a0`, `a2 = a1`,
    /// ...) shares the walk.
    fn object(&self, var: VarId, known: &mut FxHashMap<VarId, Option<String>>) -> Option<String> {
        /// How a variable walked through holds what its operand does.
        enum Through<'a> {
            Copy,
            Attr(&'a str),
            Subscript,
        }
        let mut walked: Vec<(VarId, Through<'_>)> = Vec::new();
        let mut cur = var;
        // SSA definitions form no cycle without a phi, which ends the walk.
        let mut object = loop {
            if let Some(object) = known.get(&cur) {
                break object.clone();
            }
            let Some(inst) = self.defs.get(&cur) else {
                break None;
            };
            let base = match &inst.kind {
                InstKind::Copy { src, .. } => {
                    walked.push((cur, Through::Copy));
                    cur = *src;
                    continue;
                }
                InstKind::AttrLoad { obj, attr, .. } => {
                    walked.push((cur, Through::Attr(attr.as_str())));
                    cur = *obj;
                    continue;
                }
                InstKind::SubscriptLoad {
                    obj,
                    is_slice: false,
                    ..
                } => {
                    walked.push((cur, Through::Subscript));
                    cur = *obj;
                    continue;
                }
                InstKind::Import { .. } => self.imports.get(&inst.span).cloned(),
                // A module body loads the names it never binds (and lazily imported ones).
                InstKind::LoadGlobal {
                    name,
                    is_lazy_import: false,
                    ..
                } if self.builtins.contains(name.as_str()) => Some(format!("builtins.{name}")),
                InstKind::MakeClosure {
                    func_id: Some(id), ..
                } if self.defines(*id) => {
                    let f = self.input.ir.function(*id);
                    Some(format!("{}.{}", self.decls.module_name, f.qualname))
                }
                _ => None,
            };
            known.insert(cur, base.clone());
            break base;
        };
        for (v, through) in walked.into_iter().rev() {
            object = match through {
                Through::Copy => object,
                // A name of more than a few dotted segments is no alias anyone writes; the cap
                // keeps a chain of attribute loads (`a2 = a1.x`) linear.
                Through::Attr(attr) => object
                    .filter(|base| base.matches('.').count() < MAX_ALIAS_SEGMENTS)
                    .map(|base| format!("{base}.{attr}")),
                Through::Subscript => object.filter(|class| crate::types::runtime_generic(class)),
            };
            known.insert(v, object.clone());
        }
        object
    }

    /// Whether the function or class body `id` is a module-level `def` or `class` statement
    /// (not a lambda, whose name all lambdas of the module share).
    fn defines(&self, id: heki_ir::FunctionId) -> bool {
        let qualname = &self.input.ir.function(id).qualname;
        !qualname.contains('.')
            && !qualname.starts_with('<')
            && (self.decls.classes.contains_key(qualname)
                || self.decls.functions.contains_key(qualname))
    }
}

/// The runtime star imports of the module, in source order: each statement's span and the module
/// it names ([`ModuleDecls::star_imports`]).
fn runtime_star_imports(input: &ModuleInput<'_>) -> Vec<(Span, String)> {
    (input.semantic.star_imports.iter())
        .filter(|star| !star.in_type_checking)
        .filter_map(|star| Some((star.span, star_import_module(input, star)?)))
        .collect()
}

/// [`ModuleDecls::late_stars`]: a star import binds the names it imports when it runs, so it
/// binds a definition's name again when it runs after the definition's statement (later in the
/// module body), or on a path where the body leaves the name unbound.
fn late_stars(
    decls: &ModuleDecls,
    bindings: &BodyBindings<'_>,
    stars: &[(Span, String)],
) -> BTreeMap<String, u32> {
    if stars.is_empty() {
        return BTreeMap::new();
    }
    (decls.classes.keys())
        .chain(decls.functions.keys())
        .filter(|q| !q.contains('.'))
        .filter_map(|q| {
            let first = bindings
                .first_binding(q)
                .filter(|_| bindings.always_binds(q));
            let from = match first {
                Some(start) => stars.iter().position(|(span, _)| span.start > start)?,
                None => 0,
            };
            Some((q.clone(), u32::try_from(from).ok()?))
        })
        .collect()
}

/// [`ModuleDecls::rebound_definitions`]. A `def` or `class` statement binds its name to the
/// object it makes (`MakeClosure` of a function of that name), at module level by a store
/// (`stores` counts every store of a module global), in a class body by the value itself; any
/// other store of the name is another object, unless the module body binds the name to named
/// objects only, and so is a class-body name bound as a named copy (`m = other`,
/// `m = staticmethod(m)`).
fn rebound_definitions(
    input: &ModuleInput<'_>,
    stores: &GlobalStores<'_>,
    decls: &ModuleDecls,
    bindings: &BodyBindings<'_>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut by_statement: FxHashMap<&str, u32> = FxHashMap::default();
    let defines = |qualname: &str| decls.functions.contains_key(qualname);
    for f in &input.ir.functions {
        let insts = || f.blocks.iter().flat_map(|b| &b.insts);
        let made: FxHashMap<VarId, &str> = insts()
            .filter_map(|i| match &i.kind {
                InstKind::MakeClosure {
                    dst,
                    func_id: Some(id),
                    ..
                } => Some((*dst, input.ir.function(*id).name.as_str())),
                _ => None,
            })
            .collect();
        for inst in insts() {
            match &inst.kind {
                InstKind::StoreGlobal { name, value }
                    if made.get(value) == Some(&name.as_str()) =>
                {
                    *by_statement.entry(name).or_default() += 1;
                }
                InstKind::Copy { dst, .. } if f.is_class_body => out.extend(
                    f.var_name(*dst)
                        .map(|name| format!("{}.{name}", f.qualname))
                        .filter(|member| defines(member)),
                ),
                InstKind::Delete { name, .. } if f.is_class_body => {
                    let member = format!("{}.{name}", f.qualname);
                    if defines(&member) {
                        out.insert(member);
                    }
                }
                _ => {}
            }
        }
        out.extend(code_replaced(input, f, decls));
    }
    let dynamic = input.ir.constants.dynamic;
    // A name the module body binds again to named objects only holds them
    // (`ModuleDecls::aliases`, `ModuleDecls::alias_paths`): a call through the name runs them,
    // and a value naming the definition is the object its statement made.
    let resolved = |q: &str| decls.aliases.contains_key(q) || decls.alias_paths.contains_key(q);
    // A definition whose statement a function runs holds the name once that function has run
    // (`ModuleDecls::made_in`): unless the module body binds the name on every path, a read
    // may find it unbound, or bound to what that function binds at some other time.
    let made_in = decls.made_in.as_deref();
    let bound_late =
        |q: &str| made_in.is_some_and(|m| m.contains_key(q)) && !bindings.always_binds(q);
    out.extend(
        decls
            .functions
            .keys()
            .chain(decls.classes.keys())
            .filter(|q| !q.contains('.'))
            .filter(|q| {
                dynamic
                    || !(resolved(q)
                        || (stores.exactly(q, by_statement.get(q.as_str()).copied().unwrap_or(0))
                            && !bound_late(q)))
            })
            .cloned(),
    );
    out
}

/// The functions of the module whose `__code__` `f` stores or deletes, by qualname: a
/// module-level function read by name (`f.__code__ = g.__code__`), or a method read on its
/// class (`C.m.__code__ = ...`); in the module body, the name is the object its statement
/// made. Calling either runs that code from then on.
fn code_replaced(input: &ModuleInput<'_>, f: &IrFunction, decls: &ModuleDecls) -> Vec<String> {
    let insts = || f.blocks.iter().flat_map(|b| &b.insts);
    let replaces_code = |kind: &InstKind| matches!(kind, InstKind::AttrStore { attr, .. } | InstKind::AttrDelete { attr, .. } if attr == "__code__");
    if !insts().any(|i| replaces_code(&i.kind)) {
        return Vec::new();
    }
    let defs: FxHashMap<VarId, &InstKind> = insts()
        .filter_map(|i| Some((i.kind.dst()?, &i.kind)))
        .collect();
    let shape = FnShape::new(f);
    let global = |v: VarId| match defs.get(&shape.copy_root(v)) {
        Some(InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. }) => {
            Some(name.as_str())
        }
        Some(InstKind::MakeClosure {
            func_id: Some(id), ..
        }) => Some(input.ir.function(*id).qualname.as_str()),
        _ => None,
    };
    insts()
        .filter_map(|inst| match &inst.kind {
            InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr }
                if attr == "__code__" =>
            {
                let qualname = match defs.get(&shape.copy_root(*obj)) {
                    Some(InstKind::AttrLoad { obj, attr, .. }) => {
                        format!("{}.{attr}", global(*obj)?)
                    }
                    _ => global(*obj)?.to_owned(),
                };
                decls.functions.contains_key(&qualname).then_some(qualname)
            }
            _ => None,
        })
        .collect()
}

/// The class attributes the module stores into or deletes outside their class bodies, as
/// `(class qualname, attribute)`: on the class object (`Keys.KEY = "x"` in a function, or in
/// the module body after the `class` statement, where the name holds the class it made;
/// `cls.KEY = "x"` in a class method) or on an instance in a method of the class
/// (`self.KEY = "x"`, which shadows the class attribute for that instance), also through a call
/// (`setattr(Keys, "KEY", "x")`, [`attr_calls`]).
fn class_attrs_rebound(
    input: &ModuleInput<'_>,
    decls: &ModuleDecls,
    import_targets: &FxHashMap<Span, String>,
) -> FxHashSet<(String, String)> {
    let imports = ImportNames {
        globals: &decls.imports,
        statements: import_targets,
    };
    let mut out = FxHashSet::default();
    for f in &input.ir.functions {
        let insts = || f.blocks.iter().flat_map(|b| &b.insts);
        let defs: FxHashMap<VarId, &InstKind> = insts()
            .filter_map(|i| Some((i.kind.dst()?, &i.kind)))
            .collect();
        let ix = FnIndex::new(f, &input.ir.constants);
        let shape = &ix.shape;
        let receiver = match (&f.owner_class_qualname, f.params.first()) {
            (Some(owner), Some(p))
                if is_summarized_function(f) && !f.has_decorator(&["staticmethod"]) =>
            {
                Some((owner.as_str(), self_aliases(f, p.var)))
            }
            _ => None,
        };
        let class_of = |obj: VarId| match defs.get(&shape.copy_root(obj)) {
            Some(InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. })
                if decls.classes.contains_key(name.as_str()) =>
            {
                Some(name.clone())
            }
            Some(InstKind::MakeClosure {
                func_id: Some(id), ..
            }) if input.ir.function(*id).is_class_body => {
                Some(input.ir.function(*id).qualname.clone())
            }
            _ => receiver
                .as_ref()
                .filter(|(_, selfs)| selfs.contains(&obj))
                .map(|(owner, _)| (*owner).to_owned()),
        };
        for inst in insts() {
            if let InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr } =
                &inst.kind
                && let Some(class) = class_of(*obj)
            {
                out.insert((class, attr.clone()));
            }
        }
        for (obj, attr) in attr_calls(input, &ix, &imports) {
            if let Some(class) = class_of(obj) {
                out.insert((class, attr));
            }
        }
    }
    out
}

/// What a module-level constant is bound to, behind `var` (through copies): a string literal
/// (`Ok`), or the fully qualified name of another constant (`Err`): an imported name
/// (`from pkg.keys import OTHER`) or an attribute of an imported module (`keys.OTHER`). A
/// name the module body loads by name instead of by its value is not bound on every path
/// before (a star import, a branch), so it is no constant. The walk takes at most
/// `MAX_ALIAS_CHASE` instructions before and between attribute loads, and at most
/// `MAX_CONSTANT_ATTRS` of those (`a1 = a0.x`, `a2 = a1.x`, ...): past either the value is no
/// constant, so each binding costs a bounded walk and keeps a bounded name.
fn constant_source<'a>(
    defs: &FxHashMap<VarId, &'a Inst>,
    imports: &FxHashMap<Span, String>,
    var: VarId,
) -> Option<Result<&'a str, String>> {
    // The attributes loaded on the way, the last one first.
    let mut attrs: Vec<&str> = Vec::new();
    let mut cur = var;
    let mut steps = 0;
    loop {
        if steps == MAX_ALIAS_CHASE {
            return None;
        }
        steps += 1;
        let inst = defs.get(&cur)?;
        match &inst.kind {
            InstKind::Const {
                literal: heki_ir::ConstLiteral::Str(s),
                ..
            } => return attrs.is_empty().then_some(Ok(s)),
            InstKind::Copy { src, .. } => cur = *src,
            InstKind::Import { .. } => {
                let mut fqn = imports.get(&inst.span)?.clone();
                for attr in attrs.iter().rev() {
                    fqn.push('.');
                    fqn.push_str(attr);
                }
                return Some(Err(fqn));
            }
            InstKind::AttrLoad { obj, attr, .. } => {
                if attrs.len() == MAX_CONSTANT_ATTRS {
                    return None;
                }
                attrs.push(attr);
                cur = *obj;
                steps = 0;
            }
            _ => return None,
        }
    }
}

/// The items of the tuple display of string constants behind `var` (through copies, `shape`),
/// bound to a name by the module body `f` ([`StrValue::Tuple`]): each a literal or another constant
/// ([`constant_source`]). A tuple of literals is one of the module's constants (`fixed`,
/// `heki_ir::ModuleConstants::str_tuples`), shared with it, and one naming another module's
/// constants is one only where the module's IR fixes the name (`fixed_import`,
/// `heki_ir::ModuleConstants::imported`), as for a string.
fn tuple_source(
    shape: &FnShape<'_>,
    defs: &FxHashMap<VarId, &Inst>,
    imports: &FxHashMap<Span, String>,
    var: VarId,
    fixed: Option<&Arc<[Arc<str>]>>,
    fixed_import: bool,
) -> Option<Box<[StrValue]>> {
    let f = shape.f;
    let InstKind::AllocBuiltin {
        dst,
        kind: heki_ir::BuiltinKind::Tuple,
        elems,
    } = &shape.inst(shape.copy_root(var))?.kind
    else {
        return None;
    };
    let dst = *dst;
    let sources: Vec<Result<&str, String>> = (elems.iter().enumerate())
        .map(|(i, e)| (!f.is_starred(dst, i)).then(|| constant_source(defs, imports, *e))?)
        .collect::<Option<_>>()?;
    if sources.iter().all(Result::is_ok) {
        return Some(
            fixed?
                .iter()
                .map(|t| StrValue::Literal(Arc::clone(t)))
                .collect(),
        );
    }
    let item = |source: Result<&str, String>| match source {
        Ok(text) => StrValue::Literal(Arc::from(text)),
        Err(fqn) => StrValue::Alias(fqn),
    };
    fixed_import.then(|| sources.into_iter().map(item).collect())
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
/// `readonly_attrs`, `decorators`, and `transform`), resolved through the module's imports, and
/// `MUT120`'s `ClassDecl::only_getters`.
fn readonly_metadata(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    struct Found {
        qualname: String,
        is_frozen: bool,
        dataclass: Option<Dataclass>,
        fields: BTreeSet<String>,
        readonly: BTreeSet<String>,
        decorators: Vec<(String, Option<bool>)>,
        transform: Option<bool>,
    }
    let resolve = |name: &str| decls.resolve_dotted(name, true);
    let mut found: Vec<Found> = Vec::new();
    for body in input.ir.functions.iter().filter(|f| f.is_class_body) {
        let decorator = body
            .decorators
            .iter()
            .find_map(|d| Some((d, dataclass_decorator(d, &resolve)?)));
        let dataclass = decorator.map(|(_, frozen)| frozen);
        let named_tuple = decls.classes.get(&body.qualname).is_some_and(|c| {
            c.bases
                .iter()
                .any(|b| NAMED_TUPLE_BASES.contains(&b.as_str()))
        });
        let (mut fields, mut readonly) = (BTreeSet::new(), BTreeSet::new());
        let shape = FnShape::new(body);
        for ann in &body.local_annotations {
            let (q, container) = container_annotation(&ann.annotation, &resolve);
            if ann.target.contains('.') || container.is_none() {
                continue;
            }
            if !compares_false(&shape, ann.span, &resolve) {
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
            dataclass: decorator.map(|(text, _)| {
                if keyword_bool(decorator_parts(text).1, "slots") == Some(true) {
                    Dataclass::Slots
                } else {
                    Dataclass::Defaults
                }
            }),
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
                    (!attr.contains('.') && q.is_final && container.is_some())
                        .then(|| attr.to_owned())
                })
                .collect();
            found.push(Found {
                qualname: owner.clone(),
                is_frozen: false,
                dataclass: None,
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
            c.dataclass = c.dataclass.or(f.dataclass);
            c.container_fields.extend(f.fields);
            c.readonly_attrs.extend(f.readonly);
            c.decorators.extend(f.decorators);
            c.transform = c.transform.max(f.transform);
        }
    }
    // `MUT120`: the classes defining nothing but property getters.
    let only_getters: Vec<String> = decls
        .classes
        .iter()
        .filter(|(qualname, c)| {
            !c.members.is_empty()
                && c.members.iter().all(|m| {
                    let getter = decls
                        .definition_path(&format!("{qualname}.{m}"))
                        .into_owned();
                    c.properties.contains(m)
                        && decls.functions.get(&getter).is_some_and(|f| f.is_property)
                })
        })
        .map(|(qualname, _)| qualname.clone())
        .collect();
    for qualname in only_getters {
        if let Some(c) = decls.classes.get_mut(&qualname) {
            c.only_getters = true;
        }
    }
}

/// `ClassDecl::properties` of the module's classes: their methods decorated with `property`
/// (`abc.abstractproperty`) or with an accessor of one (`@x.setter`, `@x.deleter`,
/// `@x.getter`), and the class-body names bound to a call of the builtin `property`.
/// `functools.cached_property` is no data descriptor: a store sets the instance's attribute.
/// Also `ClassDecl::container_properties`: the getters among them annotated to return a
/// container.
fn class_properties(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    let builtin_property = !decls.imports.contains_key("property")
        && !decls.functions.contains_key("property")
        && !decls.classes.contains_key("property");
    let resolve = |name: &str| decls.resolve_dotted(name, true);
    let mut found: Vec<(String, String)> = Vec::new();
    let mut containers: Vec<(String, String, ParamAccess)> = Vec::new();
    for f in &input.ir.functions {
        if f.is_class_body {
            let calls: FxHashSet<VarId> = f
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .filter_map(|i| match &i.kind {
                    InstKind::Call { dst, callee, .. }
                        if builtin_property && callee == "property" =>
                    {
                        Some(*dst)
                    }
                    _ => None,
                })
                .collect();
            for inst in f.blocks.iter().flat_map(|b| &b.insts) {
                if let InstKind::Copy { dst, src } = &inst.kind
                    && calls.contains(src)
                    && let Some(name) = f.var_name(*dst).filter(|n| !n.starts_with('.'))
                {
                    found.push((f.qualname.clone(), name.to_owned()));
                }
            }
            continue;
        }
        // A method defined directly in its class (`C.m`, not `C.m.<locals>.f`), by the name the
        // class holds it under (`_C__key` of `def __key`), as the IR spells the attribute.
        let method = f.owner_class_qualname.as_deref().and_then(|owner| {
            let name = f.qualname.strip_prefix(owner)?.strip_prefix('.')?;
            (!name.contains('.')).then(|| (owner, mangling::member_name(&f.qualname)))
        });
        let Some((owner, name)) = method else {
            continue;
        };
        let getter = f
            .decorators
            .iter()
            .any(|d| decls.decorator_is(d, &["property", "abstractproperty"]));
        let accessor = f.decorators.iter().any(|d| {
            decorator_parts(d)
                .0
                .rsplit_once('.')
                .is_some_and(|(_, a)| matches!(a, "setter" | "deleter" | "getter"))
        });
        let container = f
            .return_annotation
            .as_deref()
            .filter(|_| getter)
            .and_then(|a| container_annotation(a, &resolve).1);
        if let Some(access) = container {
            containers.push((owner.to_owned(), name.clone().into_owned(), access));
        }
        if getter || accessor {
            found.push((owner.to_owned(), name.into_owned()));
        }
    }
    for (class, name) in found {
        if let Some(c) = decls.classes.get_mut(&class) {
            c.properties.insert(name);
        }
    }
    for (class, name, access) in containers {
        if let Some(c) = decls.classes.get_mut(&class) {
            c.container_properties.insert(name, access);
        }
    }
}

/// Whether the class-body statement at `span` assigns `field(..., compare=False)`
/// (`dataclasses.field`): the field is state outside the instance's value.
fn compares_false(
    shape: &FnShape<'_>,
    span: Span,
    resolve: &impl Fn(&str) -> Option<String>,
) -> bool {
    let body = shape.f;
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
                    k.as_deref() == Some("compare") && shape.bool_const(*v) == Some(false)
                })
        })
}

/// Whether calling `f`, a function of the module `decls` describes, returns a fresh (async)
/// generator: a generator function whose every decorator keeps the call
/// ([`is_called_as_defined`]; a `@contextmanager` generator returns a context manager, which
/// runs the body at `with`). A generator expression is no generator function: its scope is
/// never called, and its body counts where it is written, where it is almost always consumed
/// (`sum(x for x in xs)`).
pub(crate) fn is_generator_function(f: &IrFunction, decls: &ModuleDecls) -> bool {
    f.name != "<genexpr>"
        && matches!(
            f.kind,
            FunctionKind::Generator | FunctionKind::AsyncGenerator
        )
        && is_called_as_defined(f, decls)
}

/// Every decorator of `f`, as the module `decls` describes binds it, keeps calling it what its
/// definition makes it ([`decorators::Decorator::keeps_call`]).
fn is_called_as_defined(f: &IrFunction, decls: &ModuleDecls) -> bool {
    decorators::all_keep(&f.decorators, decls, |d| d.keeps_call)
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
        let entry = cx.project.catalog(cx.kb, &symbol)?;
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
    let mut optional_attrs: Vec<(&str, &str)> = Vec::new();
    for f in &input.ir.functions {
        if f.is_class_body {
            // Class-body annotations (`fields: list[Field]`, dataclass / pydantic fields).
            for ann in &f.local_annotations {
                if ann.target.contains('.') {
                    continue;
                }
                if annotation_allows_none(&ann.annotation) {
                    optional_attrs.push((&f.qualname, &ann.target));
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
            let attr = ann.target.strip_prefix(&prefix);
            let Some(attr) = attr.filter(|a| !a.contains('.')) else {
                continue;
            };
            if annotation_allows_none(&ann.annotation)
                && let Some(owner) = &f.owner_class_qualname
            {
                optional_attrs.push((owner, attr));
            }
            if let Some(t) = cx.module.resolve_annotation_ty(&ann.annotation) {
                class.attr_types.insert(attr.to_owned(), t);
            }
        }
    }
    // After the annotations, which type every store of the attributes they declare.
    stores::record(input, project, kb, &mut decls);
    for (qualname, attr) in optional_attrs {
        if let Some(class) = decls.classes.get_mut(qualname) {
            let mut unowned: Vec<Box<str>> = std::mem::take(&mut class.unowned_attrs).into();
            unowned.push(Box::from(attr));
            unowned.sort_unstable();
            unowned.dedup();
            class.unowned_attrs = unowned.into_boxed_slice();
        }
    }
    crate::capacity::record(input, &cx, &mut decls);
    decls.field_values = fields::field_values(input, &cx);
    forwarded::complete(input, &mut decls, project);
    decls
}

/// Whether the declarations keep an inferred type of a global or a `self` attribute: an
/// instance, a union of instances, a class object (`FACTORY: type[Model] = Model`,
/// `self.kind = kind` of a `kind: type[Model]`), or a coroutine of one (`self.ready =
/// self.setup()`, awaited in another method).
fn kept_in_declarations(t: &Ty) -> bool {
    match t {
        Ty::Inst(_) | Ty::Union(_) | Ty::Class(_) => true,
        Ty::Coroutine(result) => kept_in_declarations(result),
        _ => false,
    }
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

/// `(dst, obj)` of an instruction loading the namespace of `obj`, the dict of its attributes:
/// `obj.__dict__`, or `vars(obj)`, which returns it (`val` gives what the called name holds: the
/// builtin itself, not a name shadowing it).
pub(crate) fn namespace_load<'v>(
    kind: &InstKind,
    val: impl FnOnce(VarId) -> Option<&'v Val>,
) -> Option<(VarId, VarId)> {
    if let InstKind::AttrLoad { dst, obj, attr } = kind
        && attr == "__dict__"
    {
        return Some((*dst, *obj));
    }
    plain_call(kind, &["builtins.vars"], val)
        .filter(|(_, _, args)| args.len() == 1)
        .map(|(_, dst, args)| (dst, args[0]))
}

/// Which of the objects `fqns` the call `kind`, without keywords or a `*` argument, calls, with
/// its result and its arguments ([`plain_callee`]; `builtins.setattr(obj, name, v)` calls
/// `builtins.setattr`).
pub(crate) fn plain_call<'k, 'v>(
    kind: &'k InstKind,
    fqns: &[&'static str],
    val: impl FnOnce(VarId) -> Option<&'v Val>,
) -> Option<(&'static str, VarId, &'k [VarId])> {
    let (callee, dst, args) = plain_callee(kind, val)?;
    Some((fqns.iter().copied().find(|fqn| callee.is(fqn))?, dst, args))
}

/// The object the call `kind`, without keywords or a `*` argument, calls, with its result and
/// its arguments: the one its callee holds (`val`, as for [`namespace_load`]), or the member of
/// the module or class a method is called on, as [`TypeCtx::method_call_targets`] names it.
pub(crate) fn plain_callee<'k, 'v>(
    kind: &'k InstKind,
    val: impl FnOnce(VarId) -> Option<&'v Val>,
) -> Option<(PlainCallee<'k, 'v>, VarId, &'k [VarId])> {
    let (callee, dst, args) = match kind {
        InstKind::Call {
            dst,
            callee_var: Some(f),
            args,
            kwargs,
            star_from: None,
            ..
        } if kwargs.is_empty() => match val(*f)? {
            Val::Ref(fqn) => (PlainCallee::Named(fqn), dst, args),
            _ => return None,
        },
        InstKind::MethodCall {
            dst,
            recv,
            method,
            args,
            kwargs,
            star_from: None,
            ..
        } if kwargs.is_empty() => match val(*recv)? {
            Val::Ref(owner) => (PlainCallee::Member { owner, method }, dst, args),
            _ => return None,
        },
        _ => return None,
    };
    Some((callee, *dst, args.as_slice()))
}

/// The object a plain call calls ([`plain_callee`]).
#[derive(Debug, Clone, Copy)]
pub(crate) enum PlainCallee<'k, 'v> {
    /// The one a name holds, by fqn.
    Named(&'v str),
    /// The member `method` of the module or class `owner` (fqn).
    Member { owner: &'v str, method: &'k str },
}

impl<'v> PlainCallee<'_, 'v> {
    /// Whether this is the object `fqn`.
    pub(crate) fn is(self, fqn: &str) -> bool {
        match self {
            Self::Named(named) => named == fqn,
            Self::Member { owner, method } => {
                fqn.strip_suffix(method).and_then(|o| o.strip_suffix('.')) == Some(owner)
            }
        }
    }

    /// Its fqn.
    pub(crate) fn fqn(self) -> Cow<'v, str> {
        match self {
            Self::Named(named) => Cow::Borrowed(named),
            Self::Member { owner, method } => Cow::Owned(format!("{owner}.{method}")),
        }
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

/// What a call passes at position `index` (the receiver not counted) or by `keyword`, for the
/// catalog's argument cases (`heki_kb::EcosystemEntry::case_for`), as `fact` describes it;
/// `None` when it passes nothing there. Nothing is known of an argument `*` / `**` unpacking
/// may pass.
pub(crate) fn passed_arg(
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    star_from: Option<u8>,
    at: (Option<u8>, Option<&str>),
    fact: impl Fn(VarId) -> ArgFact,
) -> Option<ArgFact> {
    match bound_arg(args, kwargs, star_from, at) {
        BoundArg::Var(v) => Some(fact(v)),
        BoundArg::Unpacked => Some(ArgFact::default()),
        BoundArg::Unbound => None,
    }
}

/// The variable a call passes at position `index` or by `keyword` ([`passed_arg`]).
pub(crate) fn passed_var(
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    star_from: Option<u8>,
    at: (Option<u8>, Option<&str>),
) -> Option<VarId> {
    match bound_arg(args, kwargs, star_from, at) {
        BoundArg::Var(v) => Some(v),
        BoundArg::Unbound | BoundArg::Unpacked => None,
    }
}

/// What a call binds to a parameter of the callable ([`bound_arg`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundArg {
    Var(VarId),
    /// No argument: the parameter keeps its default.
    Unbound,
    /// `*` or `**` unpacking may pass the argument.
    Unpacked,
}

/// What a call binds to the parameter at position `index` that the keyword `keyword` also
/// names (`None`: a parameter taken by position only, which a `**` mapping does not bind, as
/// for `list(iterable)`).
pub(crate) fn bound_arg(
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    star_from: Option<u8>,
    (index, keyword): (Option<u8>, Option<&str>),
) -> BoundArg {
    if let Some(k) = keyword
        && let Some((_, v)) = kwargs.iter().find(|(name, _)| name.as_deref() == Some(k))
    {
        return BoundArg::Var(*v);
    }
    let position = index.map(|i| (i, args.get(usize::from(i))));
    match position {
        Some((i, _)) if star_from.is_some_and(|s| i >= s) => BoundArg::Unpacked,
        Some((_, Some(v))) => BoundArg::Var(*v),
        _ if keyword.is_some() && kwargs.iter().any(|(name, _)| name.is_none()) => {
            BoundArg::Unpacked
        }
        _ => BoundArg::Unbound,
    }
}

/// The constant `var` holds (through copies), with `def` giving each variable's definition: a
/// literal, or the module constant `globals` gives the read of ([`TypeCtx::module_constant`]).
pub(crate) fn literal<'i>(
    def: impl Fn(VarId) -> Option<&'i Inst>,
    globals: &dyn Fn(&Inst) -> Option<LiteralValue>,
    var: VarId,
) -> Option<LiteralValue> {
    let inst = copy_root(def, var)?;
    match &inst.kind {
        InstKind::Const { literal, .. } => literal_value(literal),
        _ => globals(inst),
    }
}

/// The literal of the constant `var` holds (through copies), with `def` giving each variable's
/// definition.
pub(crate) fn const_literal<'i>(
    def: impl Fn(VarId) -> Option<&'i Inst>,
    var: VarId,
) -> Option<&'i ConstLiteral> {
    match copy_root_kind(def, var)? {
        InstKind::Const { literal, .. } => Some(literal),
        _ => None,
    }
}

/// What defines the value `var` copies, through local copies.
fn copy_root_kind<'i>(def: impl Fn(VarId) -> Option<&'i Inst>, var: VarId) -> Option<&'i InstKind> {
    copy_root(def, var).map(|inst| &inst.kind)
}

/// The instruction defining the value `var` copies, through local copies.
fn copy_root<'i>(def: impl Fn(VarId) -> Option<&'i Inst>, var: VarId) -> Option<&'i Inst> {
    let mut cur = var;
    // Copies of an SSA variable form a tree, so the walk ends.
    loop {
        let inst = def(cur)?;
        match &inst.kind {
            InstKind::Copy { src, .. } => cur = *src,
            _ => return Some(inst),
        }
    }
}

/// A literal's value, as a catalog case compares it.
pub(crate) fn literal_value(literal: &ConstLiteral) -> Option<LiteralValue> {
    match literal {
        ConstLiteral::Bool(b) => Some(LiteralValue::Bool(*b)),
        ConstLiteral::Int(n) => Some(LiteralValue::Int(*n)),
        ConstLiteral::Float(f) => f.parse().ok().map(LiteralValue::Float),
        ConstLiteral::Str(s) => Some(LiteralValue::Str(s.clone())),
        _ => None,
    }
}

/// [`ArgFact`] of `var`: its constant (a literal, or a module constant bound once that
/// `globals` gives, [`TypeCtx::module_constant`]), the class `class_of` gives it, the flags it
/// sets through the named objects `name_of` gives its operands, and whether it names a file
/// ([`names_a_file`], `files` giving the module constants that do,
/// [`TypeCtx::names_file_constant`]).
pub(crate) fn arg_fact<'i>(
    def: impl Fn(VarId) -> Option<&'i Inst>,
    class_of: impl Fn(VarId) -> Option<String>,
    name_of: impl Fn(VarId) -> Option<String>,
    globals: &dyn Fn(&Inst) -> Option<LiteralValue>,
    files: &dyn Fn(&Inst) -> bool,
    var: VarId,
) -> ArgFact {
    ArgFact {
        literal: literal(&def, globals, var),
        names_file: names_a_file(&def, &name_of, globals, files, var),
        class: class_of(var),
        flags: set_flags(&def, &name_of, var),
        ..ArgFact::default()
    }
}

/// Whether a database name opens a database in memory (`sqlite3.connect`), or opens none that
/// heki can name: the empty name is a temporary database.
pub(crate) fn in_memory_name(text: &str) -> bool {
    text.is_empty()
        || text == ":memory:"
        || text.starts_with("file::memory:")
        || text.contains("mode=memory")
}

/// Functions whose result is a path or text made only from their arguments, by the name the
/// module resolves them to.
const PATH_FUNCTIONS: &[&str] = &[
    "builtins.str",
    "os.fspath",
    "os.path.join",
    "os.path.abspath",
    "os.path.dirname",
    "os.path.realpath",
    "os.path.normpath",
    "pathlib.Path",
    "pathlib.PurePath",
];

/// `pathlib.Path` methods whose result is a path made from the receiver and the arguments.
const PATH_METHODS: &[&str] = &[
    "joinpath",
    "with_suffix",
    "with_name",
    "resolve",
    "absolute",
];

/// How a value is built, as far as [`names_a_file`] reads it.
enum FilePart {
    /// A part that names a file by itself (`true`), or one that may not.
    Leaf(bool),
    /// A value made only from these parts.
    Made(Vec<VarId>),
}

/// What `var` is built from: a string literal or a module string constant bound once
/// (`globals`, read by name, through an import, or as a module's attribute) names a file unless
/// it has an in-memory form ([`in_memory_name`]), a module constant that names one (`files`,
/// [`ModuleDecls::file_constants`]) names one, and so does
/// `__file__`; a copy, `+`, `/`, and `.parent` are made from their operands, a [`PATH_FUNCTIONS`]
/// call (the called object or receiver resolved by `name_of`) from its arguments, and a
/// [`PATH_METHODS`] call from its receiver and arguments. Anything else (a parameter, a phi, an
/// f-string, an environment lookup, another attribute or call) may name an in-memory database.
fn file_part<'i>(
    def: &impl Fn(VarId) -> Option<&'i Inst>,
    name_of: &impl Fn(VarId) -> Option<String>,
    globals: &dyn Fn(&Inst) -> Option<LiteralValue>,
    files: &dyn Fn(&Inst) -> bool,
    var: VarId,
) -> FilePart {
    let path_function =
        |name: Option<String>| name.is_some_and(|n| PATH_FUNCTIONS.contains(&n.as_str()));
    let inst = def(var);
    let names_file = |inst: &Inst| {
        files(inst)
            || matches!(globals(inst), Some(LiteralValue::Str(text)) if !in_memory_name(&text))
    };
    match inst.map(|i| &i.kind) {
        Some(InstKind::Const {
            literal: ConstLiteral::Str(text),
            ..
        }) => FilePart::Leaf(!in_memory_name(text)),
        Some(InstKind::LoadGlobal { name, .. }) if name == "__file__" => FilePart::Leaf(true),
        Some(InstKind::Copy { src, .. }) => FilePart::Made(vec![*src]),
        Some(InstKind::BinOp {
            op: BinOpKind::Add | BinOpKind::Div,
            lhs,
            rhs,
            ..
        }) => FilePart::Made(vec![*lhs, *rhs]),
        Some(InstKind::AttrLoad { obj, attr, .. }) if attr == "parent" => {
            FilePart::Made(vec![*obj])
        }
        Some(InstKind::Call {
            callee_var, args, ..
        }) if path_function(callee_var.and_then(name_of)) => FilePart::Made(args.clone()),
        Some(InstKind::MethodCall {
            recv, method, args, ..
        }) if path_function(name_of(*recv).map(|m| format!("{m}.{method}"))) => {
            FilePart::Made(args.clone())
        }
        Some(InstKind::MethodCall {
            recv, method, args, ..
        }) if PATH_METHODS.contains(&method.as_str()) => {
            FilePart::Made(std::iter::once(*recv).chain(args.iter().copied()).collect())
        }
        _ => FilePart::Leaf(inst.is_some_and(names_file)),
    }
}

/// Whether `var` is established to name a file (`ArgFact::names_file`): every part it is built
/// from ([`file_part`]) names one, within [`MAX_PARAMETER_STEPS`] values.
pub(crate) fn names_a_file<'i>(
    def: &impl Fn(VarId) -> Option<&'i Inst>,
    name_of: &impl Fn(VarId) -> Option<String>,
    globals: &dyn Fn(&Inst) -> Option<LiteralValue>,
    files: &dyn Fn(&Inst) -> bool,
    var: VarId,
) -> bool {
    let budget = Budget::unmetered("heki_effect::resolver::names_a_file");
    let mut steps = 0usize;
    let mut leaves = 0usize;
    let part = |v| file_part(def, name_of, globals, files, v);
    let not_a_file = reach(
        &budget,
        [var],
        |v, next| {
            if let FilePart::Made(parts) = part(v) {
                next.extend(parts);
            }
        },
        |v| {
            steps += 1;
            match part(v) {
                _ if steps > MAX_PARAMETER_STEPS => Visit::Stop(()),
                FilePart::Leaf(true) => {
                    leaves += 1;
                    Visit::Prune
                }
                FilePart::Leaf(false) => Visit::Stop(()),
                FilePart::Made(_) => Visit::Expand,
            }
        },
    );
    not_a_file == Answer::Known(None) && leaves > 0
}

/// Builtins whose result is text made from their argument (`str(path)`, `os.fspath(path)`).
const TEXT_OF_ARGUMENT: &[&str] = &["str", "os.fspath", "Path", "pathlib.Path"];

/// Whether `var` may hold a value its function computes from one of its parameters (`is_param`):
/// the parameter, through copies, the phis `phi_inputs` lists the inputs of (it returns whether
/// the value is a phi), operators (`name + ".db"`), and [`TEXT_OF_ARGUMENT`]. Any other value,
/// an f-string included, is opaque: an argument heki cannot evaluate is one the call waits on
/// (R6). Past its budget the value may be.
pub(crate) fn from_parameter<'i>(
    def: &impl Fn(VarId) -> Option<&'i Inst>,
    phi_inputs: &dyn Fn(VarId, &mut Vec<VarId>) -> bool,
    is_param: &dyn Fn(VarId) -> bool,
    var: VarId,
) -> bool {
    let budget = Budget::unmetered("heki_effect::resolver::from_parameter");
    let mut steps = 0usize;
    let found = reach(
        &budget,
        [var],
        |v, next| {
            if phi_inputs(v, next) {
                return;
            }
            match def(v).map(|i| &i.kind) {
                Some(InstKind::Copy { src, .. }) => next.push(*src),
                Some(InstKind::BinOp { lhs, rhs, .. }) => next.extend([*lhs, *rhs]),
                Some(InstKind::Call { callee, args, .. })
                    if TEXT_OF_ARGUMENT.contains(&callee.as_str()) =>
                {
                    next.extend(args.iter().copied());
                }
                _ => {}
            }
        },
        |v| {
            steps += 1;
            if is_param(v) || steps > MAX_PARAMETER_STEPS {
                Visit::Stop(())
            } else {
                Visit::Expand
            }
        },
    );
    found != Answer::Known(None)
}

/// The values [`from_parameter`] and [`names_a_file`] look at, at most: past them the value may
/// come from a parameter, or name an in-memory database.
const MAX_PARAMETER_STEPS: usize = 64;

/// The named objects `var` sets with `|` (`ArgFact::flags`), through copies. Each value is
/// visited once: `f1 = f0 | f0` shares its operands.
fn set_flags<'i>(
    def: &impl Fn(VarId) -> Option<&'i Inst>,
    name_of: &impl Fn(VarId) -> Option<String>,
    var: VarId,
) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    let mut seen: FxHashSet<VarId> = FxHashSet::default();
    let mut stack = vec![var];
    while let Some(v) = stack.pop() {
        if !seen.insert(v) {
            continue;
        }
        match def(v).map(|i| &i.kind) {
            Some(InstKind::Copy { src, .. }) => stack.push(*src),
            Some(InstKind::BinOp {
                op: BinOpKind::BitOr,
                lhs,
                rhs,
                ..
            }) => stack.extend([*lhs, *rhs]),
            _ => {
                if let Some(name) = name_of(v)
                    && !flags.contains(&name)
                {
                    flags.push(name);
                }
            }
        }
    }
    flags
}

/// Inferred value of an SSA variable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Val {
    /// A named object: module, class, function, or external symbol (by fqn).
    Ref(String),
    /// An instance of the class `fqn`; `true` when it is exactly that class, never a subclass:
    /// the function constructs it by calling the class by name (`Inferred::exact`).
    Inst(String, bool),
    /// A class object that is `fqn` or one of its subclasses ([`Ty::Class`]).
    Class(String),
    /// `super()` inside a method of class `fqn`.
    Super(String),
    /// An instance of one of the classes of a [`Ty::Union`] (sorted).
    Union(Vec<String>),
    /// One of several named objects ([`Ty::OneOf`]).
    OneOf(Vec<String>),
}

/// A receiver or callee as call resolution sees it, borrowed from a [`Val`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum Recv<'v> {
    Ref(&'v str),
    Inst(&'v str),
    /// An instance of exactly the class: a call on it runs what the class's MRO finds, and no
    /// subclass's override.
    Exact(&'v str),
    Class(&'v str),
    Super(&'v str),
}

impl Recv<'_> {
    /// Whether the receiver says nothing of the code a call on it runs: a value of `Any` or
    /// `object` ([`of_any_class`]), of `Callable` ([`is_any_callable`]), or of
    /// `types.ModuleType` ([`is_any_module`]), or a name nothing heki sees binds
    /// ([`is_unbound_name`]: one a star import of a library binds, or none, and the call raises
    /// `NameError`). Its library name (`typing.Any.save`, `types.ModuleType.run`,
    /// `builtins.undefined`) is no library's function, so the call is unresolved, with effects
    /// heki does not know.
    fn says_nothing(self) -> bool {
        match self {
            Recv::Inst(class) | Recv::Exact(class) | Recv::Class(class) => {
                of_any_class(class) || is_any_callable(class) || is_any_module(class)
            }
            Recv::Ref(fqn) => is_unbound_name(fqn),
            Recv::Super(_) => false,
        }
    }
}

/// Whether a value of `class` may be an object of any class: `Any` and `object` say nothing of
/// the methods it has.
pub(crate) fn of_any_class(class: &str) -> bool {
    matches!(
        class,
        "typing.Any" | "typing_extensions.Any" | "builtins.object"
    )
}

/// Whether a value of `class` may be any module: `types.ModuleType` says nothing of the names
/// the module binds.
pub(crate) fn is_any_module(class: &str) -> bool {
    class == "types.ModuleType"
}

/// Whether a value of `class` may be any function: `Callable` says nothing of the function.
pub(crate) fn is_any_callable(class: &str) -> bool {
    matches!(class, "typing.Callable" | "collections.abc.Callable")
}

/// Whether `fqn` is a name heki looks up in `builtins` because nothing it sees binds it, and
/// that Python's `builtins` does not define.
pub(crate) fn is_unbound_name(fqn: &str) -> bool {
    fqn.strip_prefix("builtins.")
        .is_some_and(|n| !heki_ir::is_builtin_name(n.split('.').next().unwrap_or(n)))
}

impl Val {
    /// The classes the value may be an instance of: the one of `Inst`, those of `Union`, none for
    /// any other value.
    pub(crate) fn instance_classes(&self) -> &[String] {
        match self {
            Self::Inst(c, _) => std::slice::from_ref(c),
            Self::Union(cs) => cs,
            _ => &[],
        }
    }

    /// Whether the value is an instance of one of the classes of `other`, which is an instance
    /// too: `other` covers it.
    pub(crate) fn within(&self, other: &Val) -> bool {
        let (own, all) = (Val::instance_classes(self), Val::instance_classes(other));
        !own.is_empty() && own.iter().all(|c| all.contains(c))
    }

    /// The project class the value is the class object of: a class named exactly, or one that
    /// may be a subclass (`cls`). Calling it constructs that class.
    pub(crate) fn class_object<'v>(&'v self, project: &ProjectDecls) -> Option<&'v str> {
        match self {
            Val::Ref(class) | Val::Class(class) if project.is_project_class(class) => Some(class),
            _ => None,
        }
    }

    /// The view call resolution takes (`None` for a union, resolved class by class).
    pub(crate) fn recv(&self) -> Option<Recv<'_>> {
        match self {
            Val::Ref(f) => Some(Recv::Ref(f)),
            Val::Inst(c, false) => Some(Recv::Inst(c)),
            Val::Inst(c, true) => Some(Recv::Exact(c)),
            Val::Class(c) => Some(Recv::Class(c)),
            Val::Super(c) => Some(Recv::Super(c)),
            Val::Union(_) | Val::OneOf(_) => None,
        }
    }
}

/// The targets of a call on a value of one of `classes` (`each` resolves it on one class): the
/// project functions of every class resolving it, or the one external function they all
/// resolve it to; a class not resolving it adds nothing when it has no such member (`absent`),
/// and leaves the call unresolved otherwise, as project and external targets together do.
fn union_targets(
    classes: &[String],
    each: impl Fn(&str) -> Callee,
    absent: impl Fn(&str) -> bool,
) -> Callee {
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
            Callee::Unresolved if absent(class) => {}
            Callee::Unresolved => return Callee::Unresolved,
        }
    }
    match (binds, external) {
        (Some(_), None) if keys.len() > CHA_CANDIDATE_LIMIT => Callee::TooMany,
        (Some(b), None) => Callee::Project(keys, b),
        (None, Some(symbol)) => Callee::External(symbol),
        _ => Callee::Unresolved,
    }
}

/// The targets of a call on a value that is one of the named objects `fqns` (`each` resolves it
/// on one): the project functions of all, when they bind the receiver alike, or the one external
/// function they all resolve it to; anything else leaves the call unresolved.
fn named_targets(fqns: &[String], each: impl Fn(&str) -> Callee) -> Callee {
    let mut out: Option<Callee> = None;
    for fqn in fqns {
        match join_either(out, each(fqn)) {
            Ok(joined) => out = Some(joined),
            Err(targets) => return targets,
        }
    }
    out.unwrap_or(Callee::Unresolved)
}

/// The targets of a call that may run what `out` resolves (the objects before, if any) or what
/// `callee` does: the project functions of both, when they bind the receiver alike, or the one
/// external function both are. `Err` holds the targets that end the join: too many past
/// [`CHA_CANDIDATE_LIMIT`] functions, and unresolved for anything else.
fn join_either(out: Option<Callee>, callee: Callee) -> Result<Callee, Callee> {
    match (out, callee) {
        (None, callee) => Ok(callee),
        (Some(Callee::Project(mut keys, binds)), Callee::Project(more, b)) if binds == b => {
            for k in more {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
            if keys.len() > CHA_CANDIDATE_LIMIT {
                return Err(Callee::TooMany);
            }
            Ok(Callee::Project(keys, binds))
        }
        (Some(Callee::External(a)), Callee::External(b)) if a == b => Ok(Callee::External(a)),
        _ => Err(Callee::Unresolved),
    }
}

impl Ty {
    /// The value consumers see: the class of an instance (the classes of a union), the fqn of a
    /// named object (none for a partial object).
    pub(crate) fn val(&self) -> Option<Val> {
        match self {
            Ty::Ref(f) => Some(Val::Ref(f.clone())),
            Ty::Class(c) => Some(Val::Class(c.clone())),
            Ty::Inst(o) => Some(Val::Inst(o.class.clone(), false)),
            Ty::Super(c) => Some(Val::Super(c.clone())),
            Ty::Union(os) => Some(Val::Union(os.iter().map(|o| o.class.clone()).collect())),
            Ty::OneOf(fqns) => Some(Val::OneOf(fqns.clone())),
            Ty::SelfType | Ty::Partial(..) | Ty::Coroutine(_) => None,
        }
    }
}

/// Where a member no project class of an MRO defines comes from
/// ([`ProjectDecls::inherited_external`]).
enum Inherited {
    /// The symbol of the base outside the project that defines it.
    External(String),
    /// A base outside the project that heki knows nothing of may define it.
    Unknown,
    /// No class of the MRO has it.
    Absent,
}

impl Inherited {
    /// The call targets of the member: unresolved unless a known base defines it
    /// ([`ProjectDecls::outside`]: none under a project module heki could not read).
    fn callee(self, project: &ProjectDecls) -> Callee {
        match self {
            Self::External(symbol) => project.outside(symbol),
            Self::Unknown | Self::Absent => Callee::Unresolved,
        }
    }
}

/// The targets of constructing `class` from its `__init__` targets: they bind the new
/// instance; a class without a project `__init__` is called by its own name.
fn constructor(project: &ProjectDecls, class: &str, init: Callee) -> Callee {
    match init {
        Callee::Project(keys, _) => Callee::Project(keys, true),
        _ => project.outside(class.to_owned()),
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
    /// Layer 1, which lists the library calls that only read an argument
    /// (`EffectSummary::only_read_params`); `None` reads none as only reading.
    pub layer1: Option<&'a EffectiveSoundnessKb>,
    module_name: String,
    ir: &'a ModuleIr,
    /// `Import` instruction span -> bound fqn (relative imports resolved).
    import_by_span: FxHashMap<Span, String>,
    /// Inference state shared by the functions of the module (child scopes and their parents).
    pub(crate) scopes: crate::infer::Scopes,
    /// The module's functions whose variables nested functions capture, by qualname, as indices
    /// into the module's functions ([`Self::captures`]); built on first use.
    captured: std::sync::OnceLock<FxHashMap<String, Vec<usize>>>,
    /// The module's bindings for the path facts, built on first use.
    bindings: std::cell::OnceCell<heki_ir::ModuleBindings>,
    /// [`ProjectDecls::function_taken`] by class and name, asked once per module (the module's
    /// summaries are one recording, which the first answer's reads join).
    taken: RefCell<FxHashMap<(String, String), Option<FuncKey>>>,
    /// What reading the module's globals and its classes' attributes gives while
    /// [`stores::record`] decides their types.
    store_reads: Option<&'a dyn stores::StoreReads>,
    /// Parameter types a registration context assumes (`heki inspect` only): the type of an
    /// unannotated parameter of one function, by qualname and position.
    seeded: Option<&'a SeededParams>,
}

/// The parameter types one registration context assumes, by function qualname and position.
pub type SeededParams = FxHashMap<(String, usize), Ty>;

/// Declarations used when a module is missing from `ProjectDecls`.
/// Where the value of a module global a function reads is decided ([`TypeCtx::global_read`]).
pub(crate) enum GlobalRead<'i> {
    /// A module-level name read in the reading module.
    Own(&'i str),
    /// A name of another module, by its fqn.
    Other(String),
}

static EMPTY_DECLS: ModuleDecls = ModuleDecls {
    module_name: String::new(),
    is_package: false,
    imports: BTreeMap::new(),
    type_checking_imports: BTreeMap::new(),
    type_checking_only: BTreeSet::new(),
    classes: BTreeMap::new(),
    functions: BTreeMap::new(),
    private_members: false,
    global_types: BTreeMap::new(),
    tables: BTreeMap::new(),
    declared_items: BTreeMap::new(),
    aliases: BTreeMap::new(),
    alias_paths: BTreeMap::new(),
    type_names: BTreeMap::new(),
    nullable_type_names: BTreeSet::new(),
    unknown_globals: BTreeSet::new(),
    rebound_imports: None,
    small_globals: BTreeMap::new(),
    grown_globals: BTreeSet::new(),
    rebound_globals: BTreeSet::new(),
    getattr_loads: BTreeSet::new(),
    getattr_rebinds: false,
    rebound_definitions: BTreeSet::new(),
    made_in: None,
    stores_elsewhere: None,
    field_values: None,
    store_fallbacks: None,
    calls_setstate: false,
    exports: None,
    star_imports: None,
    late_stars: None,
    getattr_exports: None,
    hook_factories: None,
    path_changes: None,
    str_constants: BTreeMap::new(),
    literal_constants: BTreeMap::new(),
    file_constants: BTreeSet::new(),
    object_sentinels: BTreeSet::new(),
    copy_registrations: BTreeSet::new(),
    abc_registrations: BTreeMap::new(),
    global_capacities: BTreeMap::new(),
    unread: false,
};

impl<'a> TypeCtx<'a> {
    /// Whether nested functions of the module's function `qualname` capture its variable
    /// `name` (a closure cell of it).
    pub(crate) fn captures(&self, qualname: &str, name: &str) -> bool {
        let functions = &self.ir.functions;
        let captured = self.captured.get_or_init(|| {
            let mut by_qualname: FxHashMap<String, Vec<usize>> = FxHashMap::default();
            for (i, f) in functions.iter().enumerate() {
                if !f.captured_vars.is_empty() {
                    by_qualname
                        .entry(func_qualname(f).to_owned())
                        .or_default()
                        .push(i);
                }
            }
            by_qualname
        });
        captured.get(qualname).is_some_and(|ids| {
            ids.iter()
                .any(|&i| functions[i].captured_vars.iter().any(|c| c == name))
        })
    }

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
            if let Some(t) = import_target(imp, input) {
                import_by_span.insert(imp.span, t);
            }
        }
        Self {
            project,
            module: project.module(&input.module_name).unwrap_or(&EMPTY_DECLS),
            kb,
            layer1: None,
            module_name: input.module_name.clone(),
            ir: input.ir,
            import_by_span,
            scopes: crate::infer::Scopes::default(),
            captured: std::sync::OnceLock::new(),
            bindings: std::cell::OnceCell::new(),
            taken: RefCell::default(),
            store_reads: None,
            seeded: None,
        }
    }

    /// The context reading `layer1` for the library calls that only read an argument.
    /// The context with the parameter types of `seeded`.
    pub(crate) fn with_seeded(self, seeded: Option<&'a SeededParams>) -> Self {
        Self { seeded, ..self }
    }

    /// The type a registration context gives parameter `i` of `f`.
    pub(crate) fn seeded_param(&self, f: &IrFunction, i: usize) -> Option<Ty> {
        self.seeded?.get(&(f.qualname.clone(), i)).cloned()
    }

    pub fn with_layer1(self, layer1: Option<&'a EffectiveSoundnessKb>) -> Self {
        Self { layer1, ..self }
    }

    /// The module's bindings ([`heki_ir::ModuleBindings`]).
    pub(crate) fn bindings(&self) -> &heki_ir::ModuleBindings {
        self.bindings
            .get_or_init(|| heki_ir::ModuleBindings::of(self.ir))
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

    /// The fully qualified name of the object a module-level name read inside this module is:
    /// what an import or a star import binds (`from pkg.locks import LOCK`), or this module's
    /// own global.
    pub fn global_fqn(&self, name: &str) -> String {
        match self.module.imports.get(name) {
            Some(t) => self.project.canonical(t),
            None => self
                .project
                .canonical(&format!("{}.{name}", self.module_name())),
        }
    }

    /// The fqn of the object a module-level name read inside this module names (a class, a
    /// function, a module, or a builtin), as [`Self::global`] gives it; `None` for a name
    /// holding an instance.
    pub fn named_object(&self, name: &str) -> Option<String> {
        match self.global(name) {
            Ty::Ref(fqn) => Some(fqn),
            _ => None,
        }
    }

    /// This context with the reads of the module's globals and attributes going to `reads`.
    pub(crate) fn reading_stores(mut self, reads: &'a dyn stores::StoreReads) -> Self {
        self.store_reads = Some(reads);
        self
    }

    /// Forget the types inferred so far, which depend on what the reads of the stores gave.
    pub(crate) fn forget_inferred(&self) {
        self.scopes.forget();
    }

    /// What reading the module global `name` gives while [`stores::record`] decides it, past
    /// the names whose value the module's declarations fix (imports, classes, functions, module
    /// aliases): `None` for what the declarations give.
    pub(crate) fn store_read_global(&self, name: &str) -> Option<stores::Read> {
        let reads = self.store_reads?;
        let m = self.module;
        let fixed = m.imports.contains_key(name)
            || m.classes.contains_key(name)
            || m.functions.contains_key(name)
            || (m.aliases.contains_key(name)
                && !self.project.alias_rebound(self.module_name(), name));
        if fixed {
            return None;
        }
        match reads.global(name)? {
            stores::Read::Pending => Some(stores::Read::Pending),
            stores::Read::Of(t) => self.project.declared_ty(&t).map(stores::Read::Of),
        }
    }

    /// What reading `attr` on an instance of `class` (past `class` itself with `skip_self`)
    /// gives while [`stores::record`] decides the module's attributes: `None` for what the
    /// declarations give.
    pub(crate) fn store_read_member(
        &self,
        class: &str,
        attr: &str,
        skip_self: bool,
    ) -> Option<stores::Read> {
        self.store_reads?.member(class, attr, skip_self)
    }

    /// Value of a module-level name read inside this module.
    pub(crate) fn global(&self, name: &str) -> Ty {
        if let Some(t) = self.module.imports.get(name) {
            return self.named(t);
        }
        if self.module.classes.contains_key(name) || self.module.functions.contains_key(name) {
            // A definition the module body binds again holds what binds it last.
            let fqn = format!("{}.{name}", self.module_name());
            if self.module.aliases.contains_key(name) || self.module.alias_paths.contains_key(name)
            {
                return self.named(&fqn);
            }
            return Ty::Ref(fqn);
        }
        if let Some(t) = self
            .project
            .global_type(self.module, name)
            .and_then(|t| self.project.declared_ty(&t))
        {
            return t;
        }
        // Through the name, not its target: past an alias naming a definition the module binds
        // again, the target is that definition ([`ProjectDecls::object_named`]).
        if self.module.aliases.contains_key(name)
            && !self.project.alias_rebound(self.module_name(), name)
        {
            return self.named(&format!("{}.{name}", self.module_name()));
        }
        // A global the module binds is its own, of no known value ([`Symbol::Unknown`]).
        if self.module.unknown_globals.contains(name) {
            return Ty::Ref(format!("{}.{name}", self.module_name()));
        }
        // A name a star import binds is the object other modules read through this one.
        if self.project.star_binding(self.module, name) != StarBinding::Unbound {
            return self.named(&format!("{}.{name}", self.module_name()));
        }
        Ty::Ref(format!("builtins.{name}"))
    }

    /// The value of the module-level constant `name` read inside this module, bound once to a
    /// literal: a string ([`Self::global_str`]), or a number or boolean
    /// ([`ModuleDecls::literal_constants`]) of its own or one it imports.
    pub fn global_literal(&self, name: &str) -> Option<LiteralValue> {
        if let Some(text) = self.global_str(name) {
            return Some(LiteralValue::Str(text.to_owned()));
        }
        let fqn = match self.module.imports.get(name) {
            Some(_) => self.module.fixed_import(name)?.clone(),
            None => format!("{}.{name}", self.module.module_name),
        };
        self.project.literal_constant(&fqn)
    }

    /// The value of the module-level string constant `name` read inside this module: one of
    /// its own constants, or one it imports (`from pkg.keys import KEY`, also through a star
    /// import).
    pub fn global_str(&self, name: &str) -> Option<&'a str> {
        self.global_constant(name)?.text()
    }

    /// The texts of the items of the module-level tuple constant `name` read inside this
    /// module, of its own or one it imports, as [`Self::global_str`] reads a string.
    pub fn global_str_tuple(&self, name: &str) -> Option<Vec<&'a str>> {
        self.project.tuple_texts(self.global_constant(name)?)
    }

    /// What the module-level constant `name` read inside this module is bound to
    /// ([`Self::global_str`]).
    fn global_constant(&self, name: &str) -> Option<&'a StrValue> {
        let Some(constant) = self.module.str_constants.get(name) else {
            return match self.project.star_binding(self.module, name) {
                StarBinding::Bound { target, .. } => self.project.constant_value(&target),
                _ => self
                    .module
                    .fixed_import(name)
                    .and_then(|fqn| self.project.constant_value(fqn)),
            };
        };
        let fqn = || format!("{}.{name}", self.module.module_name);
        if self.project.constant_rebound(&fqn(), constant.rebindable) {
            return None;
        }
        match &constant.value {
            StrValue::Alias(next) => self.project.constant_value(next),
            value => Some(value),
        }
    }

    /// A named fqn: a project global holding an instance becomes that instance, a module
    /// alias the object it names.
    pub(crate) fn named(&self, fqn: &str) -> Ty {
        self.project.named_ty(fqn)
    }

    /// What calling the name `fqn` calls ([`ProjectDecls::symbol`]): a module global that a
    /// `[[tool.heki.ecosystem]]` entry declares (`get = functools.partial(...)` listed as
    /// blocking, with its `return-type`) is the declared symbol.
    fn called_symbol(&self, fqn: &str) -> Symbol<'a> {
        match self.project.symbol(fqn) {
            Symbol::Unknown if self.kb.declared(fqn).is_some() => Symbol::External,
            symbol => symbol,
        }
    }

    /// Instance produced by calling the object `fqn` as the call reaches it (`through`), if
    /// known; `arg` tells what the call passes (for the catalog's argument cases). Through a
    /// project name that may hold another object than its definition, the call gives what
    /// calling either object gives, when heki names both ([`ProjectDecls::name_holds`]): never
    /// an instance of the definition's class alone.
    pub(crate) fn call_result(
        &self,
        fqn: &str,
        through: CalledThrough,
        mut arg: impl FnMut(Option<u8>, Option<&str>) -> Option<ArgFact>,
    ) -> Option<Ty> {
        let symbol = self.called_symbol(fqn);
        if through == CalledThrough::Name
            && matches!(symbol, Symbol::Class(_) | Symbol::Function(..))
        {
            match self.project.name_holds(fqn) {
                NameHolds::Definition => {}
                NameHolds::Also(other) => {
                    let other = self.symbol_result(&other, self.called_symbol(&other), &mut arg)?;
                    return self.symbol_result(fqn, symbol, arg)?.join(&other);
                }
                NameHolds::Unknown => return None,
            }
        }
        self.symbol_result(fqn, symbol, arg)
    }

    /// Instance produced by calling the object `fqn` names, `symbol` ([`Self::call_result`]).
    fn symbol_result(
        &self,
        fqn: &str,
        symbol: Symbol<'a>,
        arg: impl FnMut(Option<u8>, Option<&str>) -> Option<ArgFact>,
    ) -> Option<Ty> {
        match symbol {
            Symbol::Class(_) => return Some(Ty::inst(fqn)),
            Symbol::Function(_, decl) => {
                return self
                    .declared_result(fqn, arg)
                    .or_else(|| decl.declared_return(self.project))
                    .map(|t| decl.call_type(t));
            }
            Symbol::Module | Symbol::Global(_) | Symbol::Unknown | Symbol::Unread => return None,
            Symbol::External => {}
        }
        let builtin = fqn.strip_prefix("builtins.");
        if builtin.is_some_and(is_builtin_type) {
            return Some(Ty::inst(fqn));
        }
        let last = fqn.rsplit('.').next().unwrap_or(fqn);
        if builtin.is_none() && last.chars().next().is_some_and(char::is_uppercase) {
            // A class whose instances behave differently for an argument is typed as the
            // catalog's class of that case (`anyio.Lock(fast_acquire=True)`).
            let case = self.catalog(fqn).and_then(|e| e.case_for(arg));
            return Some(Ty::inst(
                case.and_then(|c| c.return_type.as_deref()).unwrap_or(fqn),
            ));
        }
        // Layer 2 factory or constructor registered with a lowercase name (`sqlite3.connect`,
        // `boto3.client`, `open`).
        let entry = self.catalog(fqn)?;
        if let Some(rt) = entry.result_class(arg) {
            return Some(Ty::inst(rt));
        }
        (entry.effects.contains(&ExecEffectKind::UnpooledClientAlloc)
            && !entry.effects.contains(&ExecEffectKind::BlockingNet))
        .then(|| Ty::inst(fqn))
    }

    /// Resolve calling a value (`f(...)`).
    pub fn resolve_call(&self, callee: Option<&Val>) -> Callee {
        if let Some(Val::OneOf(fqns)) = callee {
            return named_targets(fqns, |f| self.call_targets(Some(Recv::Ref(f))));
        }
        match self.dispatch_classes(callee) {
            Some(classes) => union_targets(
                classes,
                |c| self.call_targets(Some(Recv::Inst(c))),
                |c| self.project.lacks_member(c, "__call__", self.kb),
            ),
            None => self.call_targets(callee.and_then(Val::recv)),
        }
    }

    /// The classes a call or a property read on `value` dispatches on, class by class: the
    /// members of a union, the structural implementations of a protocol; `None` for a value of
    /// one class (or none) resolved as such.
    fn dispatch_classes<'v>(&'v self, value: Option<&'v Val>) -> Option<&'v [String]> {
        match value? {
            Val::Union(classes) => Some(classes),
            Val::Inst(class, _) => self.project.protocol_implementers(class),
            Val::Ref(_) | Val::Class(_) | Val::Super(_) | Val::OneOf(_) => None,
        }
    }

    /// Calling a named class constructs exactly that class: its `__init__` is the one its MRO
    /// finds. A class object that may be a subclass (`cls(...)`) runs any of theirs.
    pub(crate) fn call_targets(&self, callee: Option<Recv<'_>>) -> Callee {
        match callee.filter(|c| !c.says_nothing()) {
            Some(Recv::Ref(fqn)) => match self.called_symbol(fqn) {
                Symbol::Function(key, _) => Callee::Project(vec![key], false),
                Symbol::Class(_) => constructor(
                    self.project,
                    fqn,
                    self.project.class_targets(fqn, "__init__", self.kb),
                ),
                Symbol::Module | Symbol::Global(_) | Symbol::Unknown | Symbol::Unread => {
                    Callee::Unresolved
                }
                Symbol::External => Callee::External(fqn.to_owned()),
            },
            Some(Recv::Class(class)) => constructor(
                self.project,
                class,
                self.project
                    .method_targets(class, "__init__", false, self.kb),
            ),
            Some(Recv::Inst(class)) => self
                .project
                .method_targets(class, "__call__", false, self.kb),
            Some(Recv::Exact(class)) => self.project.class_targets(class, "__call__", self.kb),
            Some(Recv::Super(_)) | None => Callee::Unresolved,
        }
    }

    /// Whether `callee`, what [`Self::resolve_method`] resolves `recv.method(...)` to, is every
    /// implementation the call can run (`CallSite::complete`): each member of a union resolves
    /// and is complete, a protocol receiver never is, and a module member, a class object's
    /// own attribute, or `super()` names one implementation unless the project binds the name
    /// to another object (R1, [`ProjectDecls::call_rebound`], [`ProjectDecls::lookup_rebound`]).
    /// A call of a property's value is judged by the value ([`Self::targets_complete_on`]).
    pub fn method_targets_complete(
        &self,
        recv: Option<&Val>,
        method: &str,
        callee: &Callee,
    ) -> bool {
        match recv {
            Some(Val::Union(classes)) => classes.iter().all(|c| {
                let own = self.method_call_targets(Some(Recv::Inst(c)), method);
                matches!(own, Callee::Project(..) | Callee::External(_))
                    && self.targets_complete_on(Recv::Inst(c), method, &own)
            }),
            _ => recv
                .and_then(Val::recv)
                .is_some_and(|recv| self.targets_complete_on(recv, method, callee)),
        }
    }

    /// Whether `keys`, the property getters [`Self::resolve_attr_load`] resolves reading
    /// `recv.attr` to, are every getter the read can run (`CallSite::complete`): on an instance,
    /// each class the value may be (R2) takes the name from one of them
    /// ([`ProjectDecls::getters_complete`]), on each member of a union too; through `super()`,
    /// each class finds one of them after the class `super()` is in
    /// ([`ProjectDecls::super_getters_complete`]).
    pub fn getter_targets_complete(
        &self,
        recv: Option<&Val>,
        attr: &str,
        keys: &[FuncKey],
    ) -> bool {
        match recv {
            Some(Val::Union(classes)) => classes
                .iter()
                .all(|c| self.project.getters_complete(c, attr, keys, false)),
            Some(Val::Super(owner)) => self.project.super_getters_complete(owner, attr, keys),
            // A read on a class or module object runs no getter (`resolve_attr_load` resolves
            // none).
            _ => {
                matches!(recv, Some(Val::Inst(class, exact)) if self.project.getters_complete(class, attr, keys, *exact))
            }
        }
    }

    /// [`Self::method_targets_complete`] on a receiver of one class. A call of a property's value
    /// runs what calling the value runs, whatever class the getter is found in (the getter's run
    /// is a read of its own): it is complete when calling a value of the type the getter
    /// declares is ([`Self::call_targets_complete`]). A call of an attribute's value
    /// ([`Self::attribute_value`]) never is: the instance holds its own value of the name, which
    /// only the declaration ties to a class, and a class the receiver may be (R2) can define a
    /// method of the name.
    fn targets_complete_on(&self, recv: Recv<'_>, method: &str, callee: &Callee) -> bool {
        if let Some(value) = self.property_value(recv, method) {
            return self.call_targets_complete(value.as_ref(), callee);
        }
        if self.attribute_value(recv, method).is_some() {
            return false;
        }
        match recv {
            Recv::Inst(class) => self
                .project
                .instance_targets_complete(class, method, callee, self.kb),
            Recv::Exact(class) => self
                .project
                .exact_targets_complete(class, method, callee, self.kb),
            Recv::Class(class) => {
                self.project.is_project_class(class)
                    && self
                        .project
                        .instance_targets_complete(class, method, callee, self.kb)
            }
            Recv::Ref(owner) if self.project.is_project_class(owner) => !self
                .project
                .call_rebound(&format!("{}.{method}", self.project.canonical(owner))),
            Recv::Ref(owner) => !self
                .project
                .call_rebound(&self.project.object_named(&format!("{owner}.{method}"))),
            // The library method `super()` finds looks names up on the instance, of any class
            // the value may be (R2).
            Recv::Super(owner) => {
                let owner = self.project.canonical(owner);
                let mro = self.project.mro(&owner);
                let after = mro.iter().position(|c| *c == owner).map_or(0, |i| i + 1);
                let replaced = |symbol: &str| {
                    let delegates = self.kb.delegates_of(symbol).next().is_some();
                    let effect_through = self.kb.effect_through(symbol).is_some();
                    (delegates || effect_through)
                        && (self.project.value_classes(&owner).iter()).any(|c| {
                            let mro = self.project.mro(c);
                            self.project.replaces_delegate(&mro, symbol, self.kb)
                        })
                };
                !self.project.lookup_rebound(&mro[after..], method)
                    && !matches!(callee, Callee::External(symbol) if replaced(symbol))
            }
        }
    }

    /// Whether the call or the property read `kind` has as its targets every implementation it
    /// can run (`CallSite::complete`), `val` giving what its operands hold: what it resolves to
    /// ([`Self::resolve_method`], [`Self::resolve_call`], [`Self::resolve_attr_load`]) judged as
    /// [`Self::method_targets_complete`], [`Self::call_targets_complete`], and
    /// [`Self::getter_targets_complete`] judge it. Any other instruction runs no target.
    pub fn inst_targets_complete<'v>(
        &self,
        kind: &InstKind,
        val: impl Fn(VarId) -> Option<&'v Val>,
    ) -> bool {
        match kind {
            InstKind::MethodCall { recv, method, .. } => {
                let recv = val(*recv);
                let callee = self.resolve_method(recv, method);
                self.method_targets_complete(recv, method, &callee)
            }
            // A comprehension in a frame of its own (before Python 3.12) runs the code written
            // there and no other.
            InstKind::Call {
                callee_var, callee, ..
            } if heki_ir::comprehension_id(*callee_var, callee).is_some() => true,
            InstKind::Call { callee_var, .. } => {
                let value = callee_var.and_then(&val);
                let callee = self.resolve_call(value);
                self.call_targets_complete(value, &callee)
            }
            InstKind::AttrLoad { obj, attr, .. } => {
                let recv = val(*obj);
                matches!(
                    self.resolve_attr_load(recv, attr),
                    Some(Callee::Project(keys, _)) if self.getter_targets_complete(recv, attr, &keys)
                )
            }
            // A generator expression runs the code written there and no other.
            InstKind::MakeGenExpr {
                func_id: Some(_), ..
            } => true,
            // A subscript of an object the code names (`numpy.r_[...]`) runs the library's
            // `__getitem__`, unless the project binds the name to another object.
            InstKind::SubscriptLoad { obj, .. } => {
                matches!(val(*obj), Some(Val::Ref(fqn)) if !self.project.call_rebound(fqn))
            }
            _ => false,
        }
    }

    /// Whether `callee`, what [`Self::resolve_call`] resolves calling `value` to, is every
    /// implementation the call can run: a function or class named exactly, unless the project
    /// binds the name to another object ([`ProjectDecls::call_rebound`]), or an instance's
    /// `__call__` as for a method.
    pub fn call_targets_complete(&self, value: Option<&Val>, callee: &Callee) -> bool {
        match value {
            Some(Val::Ref(fqn)) => !self.project.call_rebound(fqn),
            Some(Val::Inst(class, false)) => self
                .project
                .instance_targets_complete(class, "__call__", callee, self.kb),
            Some(Val::Inst(class, true)) => self
                .project
                .exact_targets_complete(class, "__call__", callee, self.kb),
            Some(Val::Union(_) | Val::Class(_) | Val::Super(_) | Val::OneOf(_)) | None => false,
        }
    }

    /// Resolve `recv.method(...)` where `ty` is the type of `recv` with its arguments: an
    /// instance holding a named object in its field `method` ([`crate::types::Args::Fields`],
    /// [`ProjectDecls::field_holds_given`]) calls that object, binding no receiver; anything
    /// else is resolved by its value ([`Self::resolve_method`]).
    pub(crate) fn resolve_method_of(
        &self,
        recv: Option<&Val>,
        ty: Option<&Ty>,
        method: &str,
    ) -> Callee {
        if let Some(held) = ty.and_then(|t| self.field_held(t, method)) {
            return match held {
                Elem::Of(t) => self.resolve_call(t.val().as_ref()),
                Elem::Empty | Elem::Unknown => Callee::Unresolved,
            };
        }
        self.resolve_method(recv, method)
    }

    /// What the instance of type `ty` holds in its field `attr`, when it holds what the call
    /// constructing it bound: the one reading of [`crate::types::Args::Fields`], for typing and
    /// for call targets.
    pub(crate) fn field_held<'t>(&self, ty: &'t Ty, attr: &str) -> Option<&'t Elem> {
        let o = ty.obj()?;
        let held = o.args.field(attr)?;
        self.project
            .field_holds_given(&o.class, attr)
            .then_some(held)
    }

    /// Resolve `recv.method(...)`.
    pub fn resolve_method(&self, recv: Option<&Val>, method: &str) -> Callee {
        if let Some(Val::OneOf(fqns)) = recv {
            return named_targets(fqns, |f| {
                self.method_call_targets(Some(Recv::Ref(f)), method)
            });
        }
        match self.dispatch_classes(recv) {
            Some(classes) => union_targets(
                classes,
                |c| self.method_call_targets(Some(Recv::Inst(c)), method),
                |c| self.project.lacks_member(c, method, self.kb),
            ),
            None => self.method_call_targets(recv.and_then(Val::recv), method),
        }
    }

    pub(crate) fn method_call_targets(&self, recv: Option<Recv<'_>>, method: &str) -> Callee {
        let recv = recv.filter(|r| !r.says_nothing());
        if let Some(callee) = recv.and_then(|r| self.alias_call_targets(r, method)) {
            return callee;
        }
        self.defined_method_targets(recv, method)
    }

    /// [`Self::method_call_targets`] by the methods the classes define, past the class bodies
    /// binding `method` to a name.
    fn defined_method_targets(&self, recv: Option<Recv<'_>>, method: &str) -> Callee {
        match recv {
            Some(Recv::Ref(fqn)) => {
                if self.project.is_project_class(fqn) {
                    return self.through_class(self.project.class_targets(fqn, method, self.kb));
                }
                // A module alias called through its module (`aliased.nap(1)`) is the object it
                // names, as the name imported from the module is.
                let name = format!("{fqn}.{method}");
                if let Some(Ty::OneOf(objects)) = self.project.named_on_paths(&name) {
                    return named_targets(&objects, |f| self.call_targets(Some(Recv::Ref(f))));
                }
                let target = self.project.object_named(&name);
                self.call_targets(Some(Recv::Ref(&target)))
            }
            Some(Recv::Class(class)) => {
                self.through_class(self.project.method_targets(class, method, false, self.kb))
            }
            Some(recv @ (Recv::Inst(class) | Recv::Exact(class) | Recv::Super(class))) => {
                // `obj.prop(...)` runs the getter as a read of `obj.prop` does (where the summary
                // records it), then calls the value it gives; `obj.kind(...)` calls the value
                // the attribute holds.
                match (self.called_member(recv, method), recv) {
                    (Some(value), _) => self.resolve_call(value.as_ref()),
                    (None, Recv::Exact(_)) => self.project.class_targets(class, method, self.kb),
                    (None, _) => self.project.method_targets(
                        class,
                        method,
                        matches!(recv, Recv::Super(_)),
                        self.kb,
                    ),
                }
            }
            None => Callee::Unresolved,
        }
    }

    /// The targets of `recv.method(...)` when the class body defining `method` binds it to a
    /// name ([`ProjectDecls::class_aliases`]), on every class the receiver may be: a class object
    /// is exactly its class, an instance or `cls` also a project subclass. Calling the named
    /// object calls it as it is, except a function read on an instance outside `staticmethod`,
    /// a bound method, which binds the instance first. A class finding a method of the name, or
    /// none, or binding the name to its own `def` on some paths, runs what the methods give
    /// ([`Self::defined_method_targets`]). `None` when no class binds the name to a name;
    /// unresolved when the targets bind differently; too many past [`DISPATCH_CLASS_LIMIT`]
    /// classes.
    fn alias_call_targets(&self, recv: Recv<'_>, method: &str) -> Option<Callee> {
        let (classes, on_instance) = self.alias_classes(recv, method)?;
        // As for the implementations of a method (`instance_targets_complete`). Past the limit,
        // the first class binding the name to a name decides, not what each one binds.
        if classes.len() > DISPATCH_CLASS_LIMIT {
            return (classes.iter())
                .any(|c| self.project.class_aliases(c, method).is_some())
                .then_some(Callee::TooMany);
        }
        let found: Vec<Option<ClassAlias<'_>>> = classes
            .iter()
            .map(|c| self.project.class_aliases(c, method))
            .collect();
        if found.iter().all(Option::is_none) {
            return None;
        }
        let mut out: Option<Callee> = found
            .iter()
            .any(|alias| alias.as_ref().is_none_or(|a| a.defined.is_some()))
            .then(|| self.defined_method_targets(Some(recv), method));
        let partials: Vec<&PartialAttr> =
            found.iter().flatten().filter_map(|a| a.partial).collect();
        let names = found.into_iter().flatten().flat_map(|alias| alias.names);
        for (named, is_static) in names {
            let named = self.project.object_named(named);
            let callee = match self.called_symbol(&named) {
                Symbol::Function(key, _) if on_instance && !is_static => {
                    Callee::Project(vec![key], true)
                }
                // A library function read on an instance may bind it (a Python function) or
                // not (a builtin).
                Symbol::External if on_instance && !is_static => Callee::Unresolved,
                _ => self.call_targets(Some(Recv::Ref(&named))),
            };
            match join_either(out, callee) {
                Ok(joined) => out = Some(joined),
                Err(targets) => return Some(targets),
            }
        }
        for partial in partials {
            let callee = match self.partial_attr_callable(partial, on_instance) {
                Some(callable) => match self.call_targets(Some(Recv::Ref(&callable))) {
                    // The keywords it holds may decide what a library call does, which the
                    // catalog reads from the arguments the call shows.
                    Callee::External(_) if partial.keywords => Callee::Unresolved,
                    callee => callee,
                },
                None => Callee::Unresolved,
            };
            match join_either(out, callee) {
                Ok(joined) => out = Some(joined),
                Err(targets) => return Some(targets),
            }
        }
        out
    }

    /// The classes whose bodies may bind `method` for a call on `recv`, and whether the call is
    /// on an instance: a class object is exactly its class, an instance or `cls` also a project
    /// subclass. `None` when no class body binds the name to a name, or `recv` is no class or
    /// instance of one.
    fn alias_classes(&self, recv: Recv<'_>, method: &str) -> Option<(Vec<String>, bool)> {
        let (class, on_instance, family) = match recv {
            Recv::Ref(class) if self.project.is_project_class(class) => (class, false, false),
            Recv::Class(class) => (class, false, true),
            Recv::Inst(class) => (class, true, true),
            Recv::Exact(class) => (class, true, false),
            Recv::Ref(_) | Recv::Super(_) => return None,
        };
        // Every method call asks, and the receiver's class may have thousands of subclasses:
        // they are listed only for a name some class body binds.
        if !self.project.binds_alias(method) {
            return None;
        }
        let mut classes = vec![self.project.canonical(class)];
        if family {
            classes.extend(self.project.subclasses_of(class));
        }
        Some((classes, on_instance))
    }

    /// The callable a call through the `functools.partial` object `partial` of a class
    /// attribute calls, read on the class object or on an instance (`on_instance`), by its fqn:
    /// the object calls it with the call's arguments and then the keywords it holds. `None` for
    /// a plain partial read on an instance, which Python 3.14 binds to the instance as its first
    /// argument and older versions do not.
    fn partial_attr_callable(&self, partial: &PartialAttr, on_instance: bool) -> Option<String> {
        (partial.is_static || !on_instance).then(|| self.project.object_named(&partial.callable))
    }

    /// Whether `recv.method(...)` calls a `functools.partial` object of a class attribute that
    /// holds keywords ([`PartialAttr::keywords`]) on some class `recv` may be: the call passes
    /// the callable keywords it does not show, which may bind any parameter.
    pub(crate) fn calls_partial_with_keywords(&self, recv: Option<&Val>, method: &str) -> bool {
        let holds_keywords = |recv: Recv<'_>| {
            // Past the limit the call has too many targets to bind (`alias_call_targets`).
            self.alias_classes(recv, method)
                .filter(|(classes, _)| classes.len() <= DISPATCH_CLASS_LIMIT)
                .is_some_and(|(classes, _)| {
                    classes.iter().any(|c| {
                        self.project
                            .class_aliases(c, method)
                            .and_then(|a| a.partial)
                            .is_some_and(|p| p.keywords)
                    })
                })
        };
        match self.dispatch_classes(recv) {
            Some(classes) => classes.iter().any(|c| holds_keywords(Recv::Inst(c))),
            None => recv.and_then(Val::recv).is_some_and(holds_keywords),
        }
    }

    /// What reading `attr` on an instance of `class` gives when the class body defining it binds
    /// it to a name ([`ProjectDecls::class_alias`]), on every class the instance may be: the
    /// class and its project subclasses, as for a call ([`Self::alias_call_targets`]). The join
    /// of what each gives ([`Self::exact_instance_alias`]); unknown (`Some(None)`) when one gives
    /// a bound method, when a subclass finds another object (its own `def`, an attribute stored),
    /// and past [`DISPATCH_CLASS_LIMIT`] classes. `None` when `class` binds no such name.
    pub(crate) fn instance_alias(&self, class: &str, attr: &str) -> Option<Option<Ty>> {
        let mut value = self.exact_instance_alias(class, attr)?;
        let subclasses = self.project.subclasses_of(class);
        if subclasses.len() >= DISPATCH_CLASS_LIMIT {
            return Some(None);
        }
        for sub in &subclasses {
            let read = self.exact_instance_alias(sub, attr).flatten();
            value = value.zip(read).and_then(|(a, b)| a.join(&b));
            if value.is_none() {
                break;
            }
        }
        Some(value)
    }

    /// What reading `attr` on an instance of exactly `class` gives when the class body defining
    /// it binds it to a name ([`ProjectDecls::class_alias`]): the named object, unless it is a
    /// function outside `staticmethod`, which the read binds to the instance (`Some(None)`, a
    /// bound method); `None` for no such attribute.
    fn exact_instance_alias(&self, class: &str, attr: &str) -> Option<Option<Ty>> {
        if let Some(partial) = self
            .project
            .class_aliases(class, attr)
            .and_then(|a| a.partial)
        {
            let callable = self.partial_attr_callable(partial, true);
            return Some(callable.map(|c| Ty::Partial(Box::new(self.named(&c)), partial.held())));
        }
        let (named, is_static) = self.project.class_alias(class, attr)?;
        let named = self.project.object_named(named);
        let binds = !is_static
            && matches!(
                self.called_symbol(&named),
                Symbol::Function(..) | Symbol::External | Symbol::Unknown | Symbol::Unread
            );
        Some((!binds).then(|| self.named(&named)))
    }

    /// Whether `recv.attr(...)` calls the value of a property ([`Self::property_value`]) on some
    /// class `recv` dispatches on.
    pub(crate) fn calls_property(&self, recv: Option<&Val>, attr: &str) -> bool {
        match self.dispatch_classes(recv) {
            Some(classes) => classes
                .iter()
                .any(|c| self.property_value(Recv::Inst(c), attr).is_some()),
            None => recv
                .and_then(Val::recv)
                .is_some_and(|r| self.property_value(r, attr).is_some()),
        }
    }

    /// The value reading `attr` on the instance `recv` (or on `super()`) gives when it reads a
    /// property ([`ProjectDecls::declared_property`]): its declared type, `typing.Self` bound to the
    /// instance; `Some(None)` when it is not declared.
    pub(crate) fn property_value(&self, recv: Recv<'_>, attr: &str) -> Option<Option<Val>> {
        let (class, skip_self) = match recv {
            Recv::Inst(class) | Recv::Exact(class) => (class, false),
            Recv::Super(class) => (class, true),
            Recv::Ref(_) | Recv::Class(_) => return None,
        };
        let declared = self.project.declared_property(class, attr, skip_self)?;
        Some(
            declared
                .and_then(|t| self.project.declared_ty(t))
                .and_then(|t| t.bind_self(Some(&Ty::inst(class))))
                .and_then(|t| t.val()),
        )
    }

    /// The value reading `attr` on the instance `recv` (or on `super()`) of a project class gives
    /// when no class of its MRO defines a method or a property of the name: the attribute's
    /// declared type ([`ProjectDecls::member_type`]: its annotation along the MRO, or what the
    /// project stores there), `typing.Self` bound to the instance. Calling it calls that value,
    /// as a call of a parameter declared alike does: `self.kind(...)` of `self.kind:
    /// type[Model]` constructs a `Model` (R2: or a subclass).
    fn attribute_value(&self, recv: Recv<'_>, attr: &str) -> Option<Val> {
        let (class, skip_self) = match recv {
            Recv::Inst(class) | Recv::Exact(class) => (class, false),
            Recv::Super(class) => (class, true),
            Recv::Ref(_) | Recv::Class(_) => return None,
        };
        if !self.project.is_project_class(class)
            || self
                .project
                .mro_definition(class, attr, skip_self)
                .is_some()
        {
            return None;
        }
        self.project
            .member_type(class, attr, skip_self)
            .and_then(|t| self.project.declared_ty(&t))
            .and_then(|t| t.bind_self(Some(&Ty::inst(class))))
            .and_then(|t| t.val())
    }

    /// What `recv.attr(...)` calls when `attr` names no method: the value of a property
    /// ([`Self::property_value`]) or of an attribute ([`Self::attribute_value`]); `None` when
    /// neither gives one.
    pub(crate) fn called_member(&self, recv: Recv<'_>, attr: &str) -> Option<Option<Val>> {
        self.property_value(recv, attr)
            .or_else(|| self.attribute_value(recv, attr).map(Some))
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

    /// Whether a read or call of `name` on `recv` runs one of the project functions `callee`
    /// resolves it to whatever class `recv` is: every class of a union or implementation of a
    /// protocol, as a class of its own, takes `name` from one of them
    /// ([`ProjectDecls::function_taken`]). A named object (a function called by name, which
    /// dispatches on `__call__`, or one read on a module or a class object) runs what it names
    /// unless the project may bind it again ([`Self::call_targets_complete`],
    /// [`Self::method_targets_complete`]). `cls` and `super()` dispatch on no instance: they run
    /// what they name.
    pub(crate) fn runs_on_every_class(
        &self,
        recv: Option<&Val>,
        name: &str,
        callee: &Callee,
    ) -> bool {
        let runs = |key: &FuncKey| matches!(callee, Callee::Project(keys, _) if keys.contains(key));
        let classes = match recv {
            Some(Val::Union(classes)) => classes.as_slice(),
            Some(Val::Inst(class, _)) => self
                .project
                .protocol_implementers(class)
                .unwrap_or(std::slice::from_ref(class)),
            Some(Val::Ref(_)) if name == "__call__" => {
                return self.call_targets_complete(recv, callee);
            }
            Some(Val::Ref(_)) => return self.method_targets_complete(recv, name, callee),
            Some(Val::Class(_) | Val::Super(_) | Val::OneOf(_)) | None => {
                return true;
            }
        };
        classes.iter().all(|class| {
            let key = (class.clone(), name.to_owned());
            let known = self.taken.borrow().get(&key).cloned();
            let taken = known.unwrap_or_else(|| {
                let taken = self.project.function_taken(class, name);
                self.taken.borrow_mut().insert(key, taken.clone());
                taken
            });
            taken.is_some_and(|k| runs(&k))
        })
    }

    /// Resolve reading `recv.attr` (`AttrLoad`): the property getters it runs, which bind the
    /// receiver (`super().attr` runs the next getter along the MRO); `None` when it runs no
    /// project getter (see [`ProjectDecls::property_targets`]). Reading an attribute of a class
    /// or module object (`Val::Ref`) runs none: it yields the property object itself.
    pub fn resolve_attr_load(&self, recv: Option<&Val>, attr: &str) -> Option<Callee> {
        if let Some(classes) = self.dispatch_classes(recv) {
            // A class whose read of `attr` runs no getter adds none.
            let getters = |c: &str| {
                self.project
                    .property_targets(c, attr, false, false)
                    .unwrap_or(Callee::Unresolved)
            };
            return match union_targets(classes, getters, |_| true) {
                Callee::Unresolved => None,
                callee => Some(callee),
            };
        }
        match recv? {
            Val::Inst(class, exact) => self.project.property_targets(class, attr, false, *exact),
            Val::Super(class) => self.project.property_targets(class, attr, true, false),
            Val::Union(_) | Val::Ref(_) | Val::Class(_) | Val::OneOf(_) => None,
        }
    }

    /// The class a `[[tool.heki.ecosystem]]` entry naming the project function `fqn` gives its
    /// result (the declaration wins over its annotation).
    fn declared_result(
        &self,
        fqn: &str,
        arg: impl FnMut(Option<u8>, Option<&str>) -> Option<ArgFact>,
    ) -> Option<Ty> {
        self.kb.declared(fqn)?.result_class(arg).map(Ty::inst)
    }

    /// Value returned by a resolved call target (`arg` as for [`Self::call_result`]).
    pub(crate) fn callee_result(
        &self,
        callee: &Callee,
        arg: impl FnMut(Option<u8>, Option<&str>) -> Option<ArgFact>,
    ) -> Option<Ty> {
        match callee {
            Callee::Project(keys, _) if keys.len() == 1 => {
                let decl = self.project.function_decl(&keys[0])?;
                self.declared_result(&keys[0].fqn(), arg)
                    .or_else(|| decl.declared_return(self.project))
                    .map(|t| decl.call_type(t))
            }
            Callee::External(fqn) => self.call_result(fqn, CalledThrough::Name, arg),
            _ => None,
        }
    }

    /// Forward inference of `var -> Val` over `f` (the receiver typing of `crate::infer`).
    pub fn infer(&self, f: &IrFunction) -> FxHashMap<VarId, Val> {
        self.infer_types(f).vals()
    }

    /// [`Self::infer`], with the values `isinstance` checks give variables in the blocks they
    /// guard, the classes each check names (by its second argument), and the types with their
    /// arguments.
    #[allow(clippy::type_complexity)]
    pub(crate) fn infer_flow(
        &self,
        f: &IrFunction,
    ) -> (
        FxHashMap<VarId, Val>,
        crate::infer::NarrowedVals,
        FxHashMap<VarId, Vec<String>>,
        std::rc::Rc<crate::infer::Inferred>,
    ) {
        let inferred = self.infer_types(f);
        (
            inferred.vals(),
            inferred.narrowed_vals(),
            inferred.isinstance_args(f, self.project),
            inferred,
        )
    }

    /// The module global the instruction `inst` reads, where its value is decided: a
    /// module-level name read in this module, or a name of another module by its fqn, which an
    /// import binds (a module body's copy of its `Import`, a function's own import) or which is
    /// an attribute of a module object (`settings.PAUSE`; `module_of` names the object's module,
    /// `None` for any other object).
    pub(crate) fn global_read<'i>(
        &self,
        inst: &'i Inst,
        module_of: &dyn Fn(VarId) -> Option<String>,
    ) -> Option<GlobalRead<'i>> {
        match &inst.kind {
            InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
                Some(GlobalRead::Own(name))
            }
            InstKind::Import { .. } => {
                Some(GlobalRead::Other(self.import_target(inst.span)?.to_owned()))
            }
            InstKind::AttrLoad { obj, attr, .. } => {
                let module = module_of(*obj).filter(|m| self.project.module(m).is_some())?;
                Some(GlobalRead::Other(format!("{module}.{attr}")))
            }
            _ => None,
        }
    }

    /// The string constant the global read `read` gives ([`Self::global_str`],
    /// [`ProjectDecls::str_constant`]).
    fn global_read_str(&self, read: GlobalRead<'_>) -> Option<&'a str> {
        match read {
            GlobalRead::Own(name) => self.global_str(name),
            GlobalRead::Other(fqn) => self.project.str_constant(&fqn),
        }
    }

    /// The texts of the tuple constant the global read `read` gives
    /// ([`Self::global_str_tuple`], [`ProjectDecls::str_tuple`]).
    pub(crate) fn global_read_str_tuple(&self, read: GlobalRead<'_>) -> Option<Vec<&'a str>> {
        match read {
            GlobalRead::Own(name) => self.global_str_tuple(name),
            GlobalRead::Other(fqn) => self.project.str_tuple(&fqn),
        }
    }

    /// The module constant `inst` reads ([`Self::global_read`]): a string, a number, or a
    /// boolean bound once, under the rules on rebinding and imports of [`Self::global_literal`]
    /// and [`ProjectDecls::literal_constant`].
    /// Whether the read `inst` of a module constant (as for [`Self::module_constant`]) names a
    /// file ([`ProjectDecls::file_constant`]).
    pub(crate) fn names_file_constant(
        &self,
        inst: &Inst,
        module_of: &dyn Fn(VarId) -> Option<String>,
    ) -> bool {
        let fqn = match self.global_read(inst, module_of) {
            Some(GlobalRead::Own(name)) => match self.module.imports.get(name) {
                Some(_) => self.module.fixed_import(name).cloned(),
                None => Some(format!("{}.{name}", self.module.module_name)),
            },
            Some(GlobalRead::Other(fqn)) => Some(fqn),
            None => None,
        };
        fqn.is_some_and(|fqn| self.project.file_constant(&fqn))
    }

    pub(crate) fn module_constant(
        &self,
        inst: &Inst,
        module_of: &dyn Fn(VarId) -> Option<String>,
    ) -> Option<LiteralValue> {
        match self.global_read(inst, module_of)? {
            GlobalRead::Own(name) => self.global_literal(name),
            GlobalRead::Other(fqn) => self
                .project
                .str_constant(&fqn)
                .map(|text| LiteralValue::Str(text.to_owned()))
                .or_else(|| self.project.literal_constant(&fqn)),
        }
    }

    /// The string constant `var` holds (through copies; `def` gives each variable's defining
    /// instruction, `types` the inferred values): a string literal, a module-level string
    /// constant of this module or of another project module (`KEY`, `keys.KEY`,
    /// `from pkg.keys import KEY`), or a class-level one (`self.KEY`, `cls.KEY`, `Model.KEY`).
    /// The one resolution of the attribute names and keys every rule follows
    /// (`self.__dict__.pop(TOTAL)`, `setattr(obj, KEY, v)`, `obj.__dict__[KEY]`).
    pub(crate) fn str_constant_with<'i>(
        &self,
        var: VarId,
        def: &dyn Fn(VarId) -> Option<&'i Inst>,
        types: &FxHashMap<VarId, Val>,
    ) -> Option<String> {
        let mut cur = var;
        for _ in 0..16 {
            let inst = def(cur)?;
            match &inst.kind {
                InstKind::Const {
                    literal: ConstLiteral::Str(s),
                    ..
                } => return Some(s.clone()),
                InstKind::Copy { src, .. } => cur = *src,
                InstKind::LoadGlobal { .. }
                | InstKind::LoadName { .. }
                | InstKind::Import { .. } => {
                    let read = self.global_read(inst, &|_| None)?;
                    return self.global_read_str(read).map(str::to_owned);
                }
                InstKind::AttrLoad { obj, attr, .. } => {
                    let project = self.project;
                    let (class, read) = match types.get(obj)? {
                        Val::Ref(class) if project.is_project_class(class) => {
                            (class, ConstantRead::Class)
                        }
                        Val::Ref(module) => {
                            let read = self.global_read(inst, &|_| Some(module.clone()))?;
                            return self.global_read_str(read).map(str::to_owned);
                        }
                        Val::Inst(class, _) if project.is_project_class(class) => {
                            (class, ConstantRead::Instance)
                        }
                        Val::Class(class) if project.is_project_class(class) => {
                            (class, ConstantRead::ClassOrSubclass)
                        }
                        _ => return None,
                    };
                    return project.class_str_constant(class, attr, read);
                }
                _ => return None,
            }
        }
        None
    }

    /// [`Self::str_constant_with`] in the function of `shape`, whose variables have the inferred
    /// values `types`.
    pub fn str_constant_in(
        &self,
        shape: &FnShape<'_>,
        var: VarId,
        types: &FxHashMap<VarId, Val>,
    ) -> Option<String> {
        self.str_constant_with(var, &|v| shape.inst(v), types)
    }

    /// The integer `var` holds in the function of `shape` (through copies): a literal, or a
    /// module constant ([`Self::module_constant`]) bound to an integer once.
    pub fn int_constant_in(
        &self,
        shape: &FnShape<'_>,
        var: VarId,
        types: &FxHashMap<VarId, Val>,
    ) -> Option<i64> {
        let module_of = |v: VarId| match types.get(&v)? {
            Val::Ref(fqn) => Some(fqn.clone()),
            _ => None,
        };
        let globals = |inst: &Inst| self.module_constant(inst, &module_of);
        // The shape's table of copy roots, not a walk down the chain for each read.
        match literal(|v| shape.inst(v), &globals, shape.copy_root(var))? {
            LiteralValue::Int(n) => Some(n),
            _ => None,
        }
    }

    /// The value the item `key` of the table `read` reads gives ([`ProjectDecls::table`]):
    /// `Some(None)` for an entry of unknown value, `None` when `read` reads no table or the
    /// table has no such key (reading it raises `KeyError`).
    pub(crate) fn table_value(&self, read: GlobalRead<'_>, key: &str) -> Option<Option<Ty>> {
        let fqn = match read {
            GlobalRead::Own(name) => match self.module.imports.get(name) {
                Some(_) => self.module.fixed_import(name)?.clone(),
                None => format!("{}.{name}", self.module_name),
            },
            GlobalRead::Other(fqn) => fqn,
        };
        let entries = self.project.table(&fqn)?;
        let (_, value) = entries.iter().find(|(k, _)| &**k == key)?;
        Some(value.as_ref().and_then(|t| self.project.declared_ty(t)))
    }

    /// [`ProjectDecls::catalog`] of this module's project.
    pub fn catalog(&self, symbol: &str) -> Option<&'a EcosystemEntry> {
        self.project.catalog(self.kb, symbol)
    }

    /// The fqn the import statement at `span` binds (what a module body's copy of its
    /// `Import` refers to).
    pub fn import_target(&self, span: Span) -> Option<&str> {
        self.import_by_span.get(&span).map(String::as_str)
    }

    /// Value bound by an `Import` instruction (needs the span-keyed semantic import).
    pub(crate) fn import_type(&self, span: Span) -> Option<Ty> {
        self.import_by_span.get(&span).map(|t| self.named(t))
    }

    /// The names the function `f` binds itself ([`LocalNames`]), which shadow the module's names
    /// in the annotations of its locals. The parameters' annotations are read in the scope
    /// around the function, and a module or class body's bindings are the module's own.
    pub(crate) fn local_names(&self, f: &IrFunction) -> LocalNames {
        if f.is_module_body || f.is_class_body {
            return LocalNames::default();
        }
        // The statements binding each name, by span (`None`: a parameter). A statement lowered
        // more than once (a `finally` body, once per way out of the `try`) binds it once.
        let mut binders: FxHashMap<&str, Vec<Option<Span>>> = FxHashMap::default();
        for p in &f.params {
            binders.entry(&p.name).or_default().push(None);
        }
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            let name = match &inst.kind {
                // A load names its variable after what it reads: no binding.
                InstKind::LoadGlobal { .. }
                | InstKind::LoadDeref { .. }
                | InstKind::LoadName { .. } => continue,
                InstKind::StoreDeref { name, .. } => name.as_str(),
                kind => match kind.dst().and_then(|dst| f.var_name(dst)) {
                    Some(name) => name,
                    None => continue,
                },
            };
            let spans = binders.entry(name).or_default();
            if !spans.contains(&Some(inst.span)) {
                spans.push(Some(inst.span));
            }
        }
        LocalNames(
            binders
                .into_iter()
                .map(|(name, spans)| {
                    let import = match spans[..] {
                        [Some(span)] => self.import_by_span.get(&span).cloned(),
                        _ => None,
                    };
                    (name.to_owned(), import)
                })
                .collect(),
        )
    }
}
