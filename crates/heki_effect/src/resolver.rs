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
use crate::types::{Args, Elem, Generic, Ty, generic, typing_generic};
use crate::{FuncKey, ModuleInput};
use heki_ast::Span;
use heki_env::HekiConfig;
use heki_ir::walk::{Answer, Budget, FnShape, ReadLog, SharedMemo, Visit, reach};
use heki_ir::{
    BinOpKind, BlockId, ConstLiteral, FnIndex, FunctionKind, GlobalStores, Inst, InstKind,
    IrFunction, ModuleIr, SmallDisplay, Terminator, VarId,
};
use heki_kb::{
    ArgFact, CopyDepth, EcosystemEntry, EffectiveEcosystemKb, ExecEffectKind, LiteralValue,
};
use heki_semantic::{ModuleImport, ScopeKind, SemanticModel};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

mod package_paths;
mod sentinels;
pub(crate) mod stores;
mod workspace;

/// The derived indexes of a [`ProjectDecls`] version (`ProjectDecls::index_changes`).
struct Indexes {
    subclasses: FxHashMap<String, Vec<String>>,
    property_names: FxHashSet<String>,
    returning_names: FxHashSet<String>,
    frozen: FxHashSet<String>,
    built: FxHashSet<String>,
    typed_dicts: FxHashSet<String>,
    protocols: FxHashMap<String, Option<Vec<String>>>,
    grown_elsewhere: FxHashSet<String>,
    rebound_elsewhere: FxHashSet<String>,
    changed_paths: BTreeMap<String, PathChange>,
    copy_registered: FxHashSet<String>,
    setstate_called: bool,
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
    /// What the class's methods store in `attr` (`ClassDecl::attr_stores`).
    pub fn stored(&self, attr: &str) -> Option<&AttrStore> {
        let i = self
            .attr_stores
            .binary_search_by(|(a, _)| (**a).cmp(attr))
            .ok()?;
        Some(&self.attr_stores[i].1)
    }

    /// The Priority 3 attribute types, sorted by attribute: the declared ones and those
    /// `__init__` or `__post_init__` establishes.
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
    /// on `self` in a method, which type every store of the attribute.
    pub attr_types: BTreeMap<String, Ty>,
    /// What the module's code stores in the attributes of the class's instances it does not
    /// annotate, through `self` or another receiver of the class, sorted by attribute
    /// (`stores::record`). A slice rather than a map: most classes store a few, and a map's
    /// first node would hold room for eleven.
    #[serde(default, skip_serializing_if = "<[_]>::is_empty")]
    pub attr_stores: Box<[(Box<str>, AttrStore)]>,
    /// The module's code stores an attribute of an instance heki cannot name
    /// (`setattr(self, name, v)`, `self.__dict__.update(other)`): any attribute may hold any
    /// value.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stores_any_attr: bool,
    /// The attributes the module's code rebinds to what an in-place operator gives on their own
    /// value (`self.items += [x]`), sorted: such a store binds the object it read when the
    /// attribute holds a value the operator changes in place ([`ProjectDecls::join_family`]).
    #[serde(default, skip_serializing_if = "<[_]>::is_empty")]
    pub in_place_attrs: Box<[Box<str>]>,
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
    /// cannot resolve, or has a base heki names no class for ([`is_unknown_base`]), whose
    /// metaclass is unknown ([`ProjectDecls::has_custom_metaclass`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom_metaclass: bool,
    /// The `frozen=` keyword of the class statement (`class C(Base, frozen=False)`).
    pub frozen_keyword: Option<bool>,
    /// `@dataclass_transform(...)` on this class (as a base class or metaclass of the classes it
    /// builds): `Some(frozen_default)`.
    pub transform: Option<bool>,
    /// `@dataclass` / `@dataclasses.dataclass` on the class: its class-body annotations declare
    /// instance fields ([`ProjectDecls::field_defaults`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataclass: Option<Dataclass>,
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
    /// The attributes its module stores on the class object or deletes outside the class body
    /// (`Keys.KEY = "x"`, `cls.KEY = "x"`, [`class_attrs_rebound`]): the class's own value of
    /// the name from then on, which no string constant of the class or its bases fixes.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub rebound_attrs: BTreeSet<String>,
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
    /// The class body binds the attribute once, to this name (`attr = NAME`, `attr: T = NAME`,
    /// and `attr: T = field(default=NAME)` in a stdlib dataclass): the class attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
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

/// What `copy.deepcopy` makes of a value of a class ([`ProjectDecls::deep_copy_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepCopy {
    /// A new instance whose attributes hold deep copies of the original's (a new object for an
    /// `object()` sentinel), for every class the value may be.
    Attributes,
    /// A copy hook along the class's own MRO, or a `copyreg` registration of the class, makes
    /// the copy, and may keep any value.
    Hooked,
    /// Heki cannot tell for some class the value may be: a hook in a subclass or a mixin, a
    /// base outside the project the catalog does not describe, a metaclass, or a `__getattr__`
    /// / `__getattribute__` that the copy asks for `__deepcopy__`.
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
    pub defaults: Vec<(u8, crate::ConstArg)>,
    /// Calling it returns a fresh (async) generator: a generator function without decorators
    /// other than `staticmethod` / `classmethod` / `abstractmethod` / `override` (a
    /// `@contextmanager` generator returns a context manager). `MUT106` single-pass sources.
    pub is_generator: bool,
    /// No decorator replaces it: it has none other than `staticmethod` / `classmethod` /
    /// `abstractmethod` / `override`, so calling it does what its definition says (see
    /// [`FunctionDecl::makes_no_coroutine`]).
    pub called_as_defined: bool,
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
    /// An `@abstractmethod`: a call never runs it.
    pub is_abstract: bool,
    /// The return annotation of the getter of a property (`@property` / `cached_property`,
    /// with or without a setter): the type reading the attribute gives.
    pub property_type: Option<Ty>,
    /// `(i, attr)` when every `return` of every definition returns attribute `attr` of
    /// parameter `i` as itself (`return self.items`, [`returned_param_attr`]): calling the
    /// function, or reading the property, gives that object of the argument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns_param_attr: Option<(u8, String)>,
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

/// Per-module declarations other modules need (stored in `FileSummary`, IR-free).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleDecls {
    pub module_name: String,
    pub is_package: bool,
    /// Module-scope runtime imports: `bound name -> fqn`.
    pub imports: BTreeMap<String, String>,
    /// `if TYPE_CHECKING:` imports, usable for annotation resolution only.
    pub type_checking_imports: BTreeMap<String, String>,
    /// Class qualname -> declaration. Boxed, like `functions`: a map node has room for eleven
    /// entries and most modules define a few, so a declaration kept in the node would cost
    /// eleven times its size.
    pub classes: BTreeMap<String, Box<ClassDecl>>,
    /// Function qualname -> declaration (module body excluded).
    pub functions: BTreeMap<String, Box<FunctionDecl>>,
    /// Module global `name -> type` of the instance it holds (`client = httpx.Client()`): the
    /// join of every store of the module (`stores::record`).
    pub global_types: BTreeMap<String, Ty>,
    /// Module-level names bound once to another object by name (`_set = object.__setattr__`,
    /// `run = helpers.run`), by the fqn of that object ([`module_aliases`]): as a value or in
    /// an annotation, in the module or imported from it, the name is that object, unless
    /// another module rebinds it ([`ProjectDecls::object_named`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
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
    /// cfg.QUEUE`): bound to another object, so not even a tuple keeps its display, and no
    /// longer the string constant their own module binds ([`ProjectDecls::rebound_elsewhere`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub rebound_globals: BTreeSet<String>,
    /// What the module's code stores into the globals of other project modules and into the
    /// instances of their classes (`stores::record`). Boxed: most modules store into none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stores_elsewhere: Option<Box<StoresElsewhere>>,
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
    /// `obj.attr[KEY]`, as its literal would.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub str_constants: BTreeMap<String, StrConstant>,
    /// Module-level names bound once to a number or boolean literal (`PAUSE = 0`, `DELAY = 0.5`,
    /// `STRICT = False`), as [`Self::str_constants`] holds the strings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub literal_constants: BTreeMap<String, LiteralConstant>,
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
}

impl ModuleDecls {
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
            class.restores_state = false;
            class.in_place_attrs = Box::default();
            class.attr_capacities.clear();
        }
        for function in out.functions.values_mut() {
            function.offloaded_by = None;
            function.state_of_self = false;
        }
        out.global_types.clear();
        out.stores_elsewhere = None;
        out.store_fallbacks = None;
        out.global_capacities.clear();
        out
    }

    /// Whether the decorator `text` of a function of this module is one of `names`
    /// (`cached_property`, `lru_cache`, ...), known by what its head is bound to at module
    /// scope: an import or a module alias names the imported symbol, so an alias counts by that
    /// symbol's last name (`from functools import cached_property as cached`,
    /// `cached = functools.cached_property`); a function or class the module
    /// defines is its own decorator and none of `names` (an identity `def cache(fn)` caches
    /// nothing); a head bound by neither (a builtin, a star import) counts by its spelling.
    pub fn decorator_is(&self, text: &str, names: &[&str]) -> bool {
        self.decorator_symbol(text)
            .is_some_and(|s| names.contains(&s.rsplit('.').next().unwrap_or(&s)))
    }

    /// The symbol the head of the decorator `text` names, through this module's imports and
    /// aliases (`functools.cached_property` of `@cached_property` after `from functools import
    /// cached_property`); `None` for a class or function the module defines; a name the module
    /// neither imports nor defines as written.
    pub fn decorator_symbol(&self, text: &str) -> Option<String> {
        let (head, _) = decorator_parts(text);
        let first = head.split('.').next().unwrap_or(head);
        if self.imports.contains_key(first) || self.aliases.contains_key(first) {
            self.resolve_dotted(head, false)
        } else if self.classes.contains_key(first) || self.functions.contains_key(first) {
            None
        } else {
            Some(head.to_owned())
        }
    }

    /// Resolve a dotted name written in this module (runtime imports, then aliases and local
    /// definitions, then builtin types). `type_checking` also consults `if TYPE_CHECKING:`
    /// imports.
    pub(crate) fn resolve_dotted(&self, text: &str, type_checking: bool) -> Option<String> {
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
        if let Some(t) = self.aliases.get(first) {
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
    /// `Iterator[Model]`), or the class object `type[Model]` / `typing.Type[Model]` names, after
    /// `Optional` / `| None` / `Annotated` / `Final` / `ClassVar` / `Required` / `NotRequired` /
    /// `ReadOnly` and quotes. An unresolved name of a `typing` generic counts as that generic
    /// (`"Iterable[Model]"` without the import).
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
        if self.is_type_form(head) {
            // Whether `C` is a project class, and which, is decided across the project
            // (`ProjectDecls::declared_ty`).
            let class = self
                .resolve_annotation_ty(first_arg(inner))?
                .obj()?
                .class
                .clone();
            return Some(Ty::Class(class));
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

    /// Whether the subscripted head `head` is `type` or `typing.Type` (an unresolved `Type`
    /// counts, as an unresolved `typing` generic does).
    fn is_type_form(&self, head: &str) -> bool {
        match self.resolve_dotted(head, true) {
            Some(fqn) => matches!(
                fqn.as_str(),
                "builtins.type" | "typing.Type" | "typing_extensions.Type"
            ),
            None => matches!(head, "type" | "Type"),
        }
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
    /// Names looked up among the names of the project functions returning an attribute of a
    /// parameter (`FunctionDecl::returns_param_attr`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub returning_names: BTreeSet<String>,
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

/// The answers [`ProjectDecls`] keeps of one question, by its key.
type KeptAnswers<K, T> = RwLock<FxHashMap<K, Kept<T>>>;

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
    /// Names of the functions, methods, and getters returning an attribute of a parameter
    /// (`FunctionDecl::returns_param_attr`): a call or a read with another name hands out no
    /// such attribute, without resolving it.
    returning_names: FxHashSet<String>,
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
    /// What project modules store into the globals of other modules and into the instances of
    /// their classes, joined (`ModuleDecls::stores_elsewhere`), and the modules whose own types
    /// then hold no longer.
    elsewhere: stores::Elsewhere,
    /// Some project module calls `__setstate__` itself (`ModuleDecls::calls_setstate`).
    setstate_called: bool,
    /// The packages whose `__path__` a project module changes (`ModuleDecls::path_changes`, the
    /// changes joined), and the packages heki could not read, as `PathChange::Replaced`; sorted,
    /// so the packages under a name are one range.
    changed_paths: BTreeMap<String, PathChange>,
    /// The canonical fqns of the classes a project module registers with `copyreg`
    /// (`ModuleDecls::copy_registrations`), with `*` when one names a class heki cannot.
    copy_registered: FxHashSet<String>,
    recorder: Recorder,
    /// The C3 linearizations computed so far of classes with multiple inheritance along their
    /// bases (`Self::mro`); cleared with the indexes.
    linearized: RwLock<FxHashMap<String, Vec<String>>>,
    /// One shared [`crate::ClassLine`] per class (`Self::lineage`); cleared with the indexes.
    lines: RwLock<FxHashMap<String, crate::SharedLine>>,
    /// The answers of [`Self::class_str_constant`] so far, by class, name, and whether the
    /// object may be of a subclass; cleared with the indexes.
    class_constants: KeptAnswers<(String, String, ConstantRead), Option<String>>,
    /// The answers of [`Self::stored_attr_type`] so far, by class and attribute; cleared with
    /// the indexes.
    attr_joins: KeptAnswers<(String, String), Option<Ty>>,
    /// The answers of [`Self::capacity`] so far; cleared with the indexes.
    capacities: KeptAnswers<CapacityOf, CapacityOf>,
    /// The answers of [`Self::catalog_attribute`] so far, by class and name; cleared with the
    /// indexes.
    catalog_attributes: SharedMemo<(String, String), Option<String>, DeclKeys>,
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
        out.class_facts
            .extend(differ(&before.copy_registered, &self.copy_registered));
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

    fn indexes(&self) -> Indexes {
        Indexes {
            subclasses: self.subclasses.clone(),
            property_names: self.property_names.clone(),
            returning_names: self.returning_names.clone(),
            frozen: self.frozen.clone(),
            built: self.built.clone(),
            typed_dicts: self.typed_dicts.clone(),
            protocols: self.protocols.clone(),
            grown_elsewhere: self.grown_elsewhere.clone(),
            rebound_elsewhere: self.rebound_elsewhere.clone(),
            changed_paths: self.changed_paths.clone(),
            copy_registered: self.copy_registered.clone(),
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
        self.class_constants
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.attr_joins
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.capacities
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.catalog_attributes.clear();
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
        for m in out.modules.values() {
            for (qualname, class) in &m.classes {
                let fqn = format!("{}.{qualname}", m.module_name);
                for base in class.bases.iter().filter(|b| !is_unknown_base(b)) {
                    subclasses
                        .entry(out.canonical(base))
                        .or_default()
                        .push(fqn.clone());
                }
            }
            for (qualname, f) in &m.functions {
                let name = qualname.rsplit('.').next().unwrap_or(qualname);
                if f.reads_as_property() {
                    property_names.insert(name.to_owned());
                }
                if f.returns_param_attr.is_some() {
                    returning_names.insert(name.to_owned());
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
        self.typed_dicts = TYPED_DICT_BASES
            .iter()
            .flat_map(|b| self.subclasses_of(b))
            .collect();
        (self.frozen, self.built) = self.transform_builds();
        self.protocols = self.protocol_implementations();
        self.grown_elsewhere = self.globals_changed_elsewhere(|m| &m.grown_globals);
        self.rebound_elsewhere = self.globals_changed_elsewhere(|m| &m.rebound_globals);
        self.setstate_called = self.modules.values().any(|m| m.calls_setstate);
        self.elsewhere = self.stores_elsewhere();
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
        self.grown_elsewhere.contains(&fqn)
    }

    /// Whether another project module rebinds or deletes the module global `fqn` (a string
    /// its own module binds once is then no constant).
    pub fn rebound_elsewhere(&self, fqn: &str) -> bool {
        let fqn = self.canonical(fqn);
        self.read(|k| note(&mut k.grown_globals, &fqn));
        self.rebound_elsewhere.contains(&fqn)
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
    pub(crate) fn protocol_implementers(&self, class: &str) -> Option<&[String]> {
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

    /// Longest project-module prefix of `fqn` and the remaining dotted part.
    fn split_module<'s>(&self, fqn: &'s str) -> Option<(&ModuleDecls, &'s str)> {
        let mut found = None;
        let module = module_prefix(fqn, |prefix| {
            found = self.module(prefix);
            found.is_some()
        })?;
        Some((found?, fqn.get(module.len() + 1..).unwrap_or("")))
    }

    /// The direct project subclasses of the canonical class `class`.
    fn direct_subclasses(&self, class: &str) -> &[String] {
        self.read(|k| note(&mut k.subclasses, class));
        self.subclasses.get(class).map_or(&[], Vec::as_slice)
    }

    /// Chase re-exports (`pkg.Foo` where `pkg/__init__.py` does `from .sub import Foo`).
    pub fn canonical(&self, fqn: &str) -> String {
        self.chase(fqn, false)
    }

    /// [`Self::canonical`] for a value read through the name (a constant, a small display):
    /// `None` when a module on the way binds the name it re-exports again
    /// ([`ModuleDecls::fixed_import`]).
    pub fn canonical_value(&self, fqn: &str) -> Option<String> {
        self.follow(fqn, false, true)
    }

    /// `fqn` through re-exports and, with `aliases`, the module aliases it names (at most
    /// `MAX_ALIAS_CHASE` steps).
    fn chase(&self, fqn: &str, aliases: bool) -> String {
        self.follow(fqn, aliases, false)
            .unwrap_or_else(|| fqn.to_owned())
    }

    /// [`Self::chase`]; with `values`, `None` past a re-export its module binds again.
    fn follow(&self, fqn: &str, aliases: bool, values: bool) -> Option<String> {
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
            // `imports` holds the names an import binds once the module has run
            // (`ModuleImport::rebound_at`): a class or function of the same name is shadowed by
            // it.
            let target = match m.imports.get(first) {
                Some(_) if values && m.fixed_import(first).is_none() => return None,
                Some(target) => target,
                None if aliases => match m.aliases.get(first) {
                    Some(target) if !self.alias_rebound(&m.module_name, first) => target,
                    _ => break,
                },
                None => break,
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
        Some(cur)
    }

    /// Whether another module rebinds or deletes the module alias `name` of `module`
    /// ([`ModuleDecls::aliases`]): it is then no longer the object its module binds.
    fn alias_rebound(&self, module: &str, name: &str) -> bool {
        let fqn = format!("{module}.{name}");
        self.read(|k| note(&mut k.grown_globals, &fqn));
        self.rebound_elsewhere.contains(&fqn)
    }

    /// The object the name `fqn` stands for: through re-exports, as [`Self::canonical`], and
    /// through the module aliases it names (`pkg.model._set` for `_set = object.__setattr__` in
    /// `pkg.model`). The name itself stays the alias's: only values are the object.
    pub fn object_named(&self, fqn: &str) -> String {
        self.chase(fqn, true)
    }

    /// The value of the number or boolean constant `fqn` (`pkg.settings.PAUSE`), through
    /// re-exports ([`Self::canonical_value`]), unless another module rebinds it.
    pub fn literal_constant(&self, fqn: &str) -> Option<LiteralValue> {
        let fqn = self.canonical_value(fqn)?;
        let (m, rest) = self.split_module(&fqn)?;
        let constant = m.literal_constants.get(rest)?;
        (!(constant.rebindable && self.rebound_elsewhere(&fqn)))
            .then(|| literal_value(&constant.value))
            .flatten()
    }

    /// The value of the string constant `fqn` (`pkg.keys.KEY`, `pkg.model.Model.KEY`), through
    /// re-exports ([`Self::canonical_value`]) and constants bound to other constants (at most
    /// `MAX_ALIAS_CHASE` steps).
    pub fn str_constant(&self, fqn: &str) -> Option<&str> {
        let mut cur = self.canonical_value(fqn)?;
        for _ in 0..MAX_ALIAS_CHASE {
            let (m, rest) = self.split_module(&cur)?;
            let constant = m.str_constants.get(rest)?;
            if constant.rebindable && self.rebound_elsewhere(&cur) {
                return None;
            }
            match &constant.value {
                StrValue::Literal(v) => return Some(v),
                StrValue::Alias(next) => cur = self.canonical_value(next)?,
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
        let known = cache
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
        cache
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
            return Symbol::Module;
        }
        if m.unread {
            return Symbol::Unread;
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
        if let Some(t) = self.global_type(m, rest) {
            return Symbol::Global(t);
        }
        // A name the module does not bind is bound where heki does not read the module: a
        // submodule it does not index (a compiled extension) or a name a star import binds,
        // which only the catalog describes.
        let head = rest.split('.').next().unwrap_or(rest);
        if m.unknown_globals.contains(head)
            || m.classes.contains_key(head)
            || m.functions.contains_key(head)
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

    pub fn is_project_class(&self, fqn: &str) -> bool {
        self.class(fqn).is_some()
    }

    /// Whether the canonical project class `class` is a `TypedDict`.
    pub fn is_typed_dict(&self, class: &str) -> bool {
        self.read(|k| note(&mut k.class_facts, class));
        self.typed_dicts.contains(class)
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
    /// descriptor.
    pub fn attr_write_runs_code(&self, class: &str, attr: &str) -> bool {
        self.has_custom_metaclass(class)
            || self.mro(class).into_iter().any(|c| {
                self.read(|k| note(&mut k.class_facts, &c));
                self.class(&c).is_some_and(|decl| {
                    decl.properties.contains(attr)
                        || decl.members.contains("__setattr__")
                        || decl.members.contains("__delattr__")
                })
            })
    }

    /// Whether `class` is a project class and every class a value of it may be under R2
    /// ([`Self::value_classes`], each once) is a project class that binds none of `names` and
    /// has no metaclass heki does not know, and every base of one outside the project is a plain
    /// base or one `known_base` accepts: the classes whose code runs on such a value are the
    /// project's, which heki reads, and those `known_base` stands for.
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
    /// `pydantic.BaseModel`). A strict form of [`Self::attr_write_runs_code`]'s question, for an
    /// exact claim.
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

    /// Whether storing or deleting `attr` on a value of the project class `class` runs no code
    /// on every class it may be under R2: as [`Self::attr_store_on_instance`], except that a
    /// class body may bind the name to a function it defines that no decorator replaces (a
    /// method, which an instance's own attribute shadows) or that is a getter without a setter
    /// or deleter (`FunctionDecl::is_property`: a `property` raises, a cached property's value
    /// is the instance's own attribute).
    pub(crate) fn attr_write_runs_no_code(
        &self,
        class: &str,
        attr: &str,
        catalog_stores: impl Fn(&str) -> bool,
    ) -> bool {
        let plain_function = |c: &str| {
            self.own_method(c, attr)
                .and_then(|k| self.function_decl(&k))
                .is_some_and(|f| f.called_as_defined || f.is_property)
        };
        self.binds_none_of(class, &["__setattr__", "__delattr__"], &catalog_stores)
            && self.value_classes(class).iter().all(|c| {
                self.read(|k| note(&mut k.class_facts, c));
                self.class(c).is_some_and(|d| {
                    !d.properties.contains(attr)
                        && (!d.code_attrs.contains(attr) || plain_function(c))
                })
            })
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
            && self.binds_none_of(class, &[], catalog_stores)
    }

    /// The project classes whose bodies may supply an attribute of a value of the class `class`
    /// under assumption R2 (a value typed `class` is an instance of it or of a project
    /// subclass): the project classes along its MRO, every project subclass, and the project
    /// classes along the MROs of a subclass's other bases (a mixin it also derives from), each
    /// once.
    pub fn value_classes(&self, class: &str) -> Vec<String> {
        let class = self.canonical(class);
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

    /// Whether reading `attr` on an instance of the project class `class` (or, under assumption
    /// R2, of a project subclass) reads a plain attribute and runs no code of the project: a
    /// class along its MRO declares it (a field, a slot, or what its body or methods store), no
    /// class a value may be ([`Self::value_classes`]) binds it to a method, property, or other
    /// call in its body or defines `__getattr__` or `__getattribute__`, every base is a project
    /// class (or `object`, `NamedTuple`, `Generic`), and no metaclass heki does not know creates
    /// the class.
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
                    && !self.copy_decided_by_class(&h)
                {
                    out.push((h, attr.to_owned()));
                }
            }
        }
        out
    }

    /// Whether a project class along the MRO of `class` binds `model_config` or `Config` in its
    /// body: a model's configuration may ask it to validate its fields' defaults
    /// (`validate_default`), and a field doing so holds what the validation returns.
    fn may_validate_defaults(&self, class: &str) -> bool {
        self.mro(class).iter().any(|c| {
            self.class(c)
                .is_some_and(|d| d.members.contains("model_config") || d.members.contains("Config"))
        })
    }

    /// Whether a base of the project class `class` outside the project is one the catalog
    /// describes as building instances that hold a field's hashable default itself
    /// (`EcosystemEntry::keeps_hashable_defaults`).
    fn keeps_hashable_defaults(&self, class: &str, kb: &EffectiveEcosystemKb) -> bool {
        self.mro(class).into_iter().any(|c| {
            !self.is_project_class(&c) && kb.lookup(&c).is_some_and(|e| e.keeps_hashable_defaults)
        })
    }

    /// Whether something of the class `class` itself decides what `copy.deepcopy` makes of its
    /// instances, in place of copying their attributes: a copy hook along its MRO
    /// (`__deepcopy__`, `__reduce__`, `__reduce_ex__`, `__getstate__`, `__setstate__`), or a
    /// `copyreg` registration of the class ([`Self::copy_registered`]). Either may keep any value.
    pub fn copy_decided_by_class(&self, class: &str) -> bool {
        let class = self.canonical(class);
        self.mro(&class).iter().any(|c| {
            self.read(|k| note(&mut k.class_facts, c));
            self.class(c)
                .is_some_and(|d| COPY_HOOKS.iter().any(|h| d.members.contains(*h)))
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

    /// What `copy.deepcopy` (without a `memo`) does with a value of the project class `class`,
    /// an instance of it or, under R2, of a project subclass ([`DeepCopy`]). The copy protocol
    /// looks for `__deepcopy__` on the instance (where `__getattr__` and `__getattribute__`
    /// may answer), then for a `copyreg` registration of its exact class, then for
    /// `__reduce_ex__` / `__reduce__`, whose default reads `__getstate__` and makes the copy
    /// take the state through `__setstate__` when the class defines one; with none of them, the
    /// copy is a new instance whose attributes are deep copies of the original's.
    pub fn deep_copy_of(&self, class: &str, kb: &EffectiveEcosystemKb) -> DeepCopy {
        let class = self.canonical(class);
        let hooked = |d: &ClassDecl| COPY_HOOKS.iter().any(|h| d.members.contains(*h));
        // A base outside the project: one known to define none of the hooks, or one whose
        // `__deepcopy__` the catalog describes as copying the attributes deeply (R3).
        let known_base = |c: &str| {
            HOOKLESS_BASES.contains(&c)
                || kb
                    .lookup(&format!("{c}.__deepcopy__"))
                    .is_some_and(|e| e.copy_sanitizer == Some(CopyDepth::Deep))
        };
        if self.copy_decided_by_class(&class) {
            return DeepCopy::Hooked;
        }
        let mro = self.mro(&class);
        let mut answer = DeepCopy::Attributes;
        for c in &mro {
            if !self.is_project_class(c) && !known_base(c) {
                answer = DeepCopy::Open;
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
                answer = DeepCopy::Open;
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

    /// Whether calling the class `class` may return an existing instance instead of a new one: a
    /// project class along its MRO defines `__new__`, or it is an enumeration (`Color("red")`
    /// returns a member).
    pub fn may_reuse_instances(&self, class: &str) -> bool {
        const ENUMS: &[&str] = &[
            "enum.Enum",
            "enum.IntEnum",
            "enum.StrEnum",
            "enum.Flag",
            "enum.IntFlag",
            "enum.ReprEnum",
        ];
        self.mro(class)
            .iter()
            .any(|c| ENUMS.contains(&c.as_str()) || self.own_method(c, "__new__").is_some())
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
        let qualname = format!("{rest}.{name}");
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

    /// The declared type of reading `attr` on an instance of `class`, from the first class along
    /// the MRO (past `class` itself with `skip_self`, for `super()`) that declares it: the
    /// return annotation of a property getter (with or without a setter; another function of a
    /// name some project property has gives none), or the Priority 3 attribute type. Overriding
    /// subclasses do not count for an annotation, as for a type checker; an inferred type joins
    /// what the project's code stores into the instances of every class whose code may store
    /// into the instance ([`Self::stored_attr_type`]). The getter's module is a declaration read
    /// like any other (`DeclKeys::modules`).
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
            if let Some(t) = decl.attr_types.get(attr) {
                return Some(t.clone());
            }
            if let Some(AttrStore::Established(_)) = self.attr_store(c, decl, attr) {
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
    /// of a project module that is no class (a type alias, a `TypeVar`, a constant); an item of
    /// such a type is unknown. A name below a module heki does not see (a compiled submodule of a
    /// project package) stays as it is. A class object (`type[C]`) is [`Self::class_object`].
    pub(crate) fn declared_ty(&self, ty: &Ty) -> Option<Ty> {
        if let Ty::Class(class) = ty {
            return self.class_object(&self.object_named(class));
        }
        ty.map_members(|o| {
            let class = self.object_named(&o.class);
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

    /// The type of a class object that is the canonical class `class` or one of its subclasses
    /// ([`Ty::Class`]), as `cls` in a class method is: unknown for a protocol, whose
    /// implementations need not be its subclasses, and for a class outside the project.
    pub(crate) fn class_object(&self, class: &str) -> Option<Ty> {
        (self.is_project_class(class) && self.protocol(class).is_none())
            .then(|| Ty::Class(class.to_owned()))
    }

    /// Whether the inferred type `t` is narrower than the declared `declared`, so that a value
    /// keeps it: anything for a declaration of `Any` / `object`, an instance of a project
    /// subclass of the declared class (`x: Base = Sub()`), and for a declared class object, a
    /// class named exactly or the class object of a subclass (`kind: type[Base] = Base`, `=
    /// Sub`, `= type(sub)`).
    pub(crate) fn narrower(&self, t: &Ty, declared: &Ty) -> bool {
        if let Ty::Class(d) = declared {
            return match t {
                Ty::Ref(c) => c == d || self.is_subclass(c, d),
                Ty::Class(c) => c != d && self.is_subclass(c, d),
                _ => false,
            };
        }
        declared.obj().is_some_and(|d| {
            of_any_class(&d.class)
                || t.obj()
                    .is_some_and(|i| i.class != d.class && self.is_subclass(&i.class, &d.class))
        })
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

    /// [`Self::catalog_attribute`] of the canonical `class`, decided: the walk over the classes
    /// the value may be stops at the first that takes another row or none, or past
    /// [`DISPATCH_CLASS_LIMIT`] of them.
    fn find_catalog_attribute<'k>(
        &self,
        class: &str,
        attr: &str,
        kb: &'k EffectiveEcosystemKb,
    ) -> Option<&'k str> {
        let found = self.attribute_row_along(&self.mro(class), attr, kb)?;
        let budget = Budget::unmetered("heki_effect::resolver::ProjectDecls::catalog_attribute");
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
                let same = !self.hooks_attribute_reads(&mro)
                    && self.attribute_row_along(&mro, attr, kb) == Some(found);
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

    /// The `[[attributes]]` row reading `attr` finds along `mro`: that of the first class
    /// outside the project, a builtin type without one passed over; none past a project class
    /// binding the name, in its body, on `self`, or on the class object outside its body.
    fn attribute_row_along<'k>(
        &self,
        mro: &[String],
        attr: &str,
        kb: &'k EffectiveEcosystemKb,
    ) -> Option<&'k str> {
        for c in mro {
            if self.is_project_class(c) {
                self.read(|k| note(&mut k.class_facts, c));
                let binds = self
                    .class(c)
                    .is_some_and(|d| d.members.contains(attr) || d.rebound_attrs.contains(attr));
                if binds {
                    return None;
                }
                continue;
            }
            if is_unknown_base(c) || self.under_unread(c) {
                return None;
            }
            if let Some(found) = kb.attribute_type(c, attr) {
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

    /// [`Self::function_taken`] along `mro`: an MRO, or the part of one after a class (what
    /// `super()` in that class looks up).
    fn function_taken_along(&self, mro: &[String], name: &str) -> Option<FuncKey> {
        for c in mro {
            self.read(|k| note(&mut k.class_facts, c));
            let decl = self.class(c)?;
            // Stored on the class object outside its body (`Child.view = []`), the name holds
            // what was stored there from then on.
            if decl.rebound_attrs.contains(name) {
                return None;
            }
            if decl.members.contains(name) {
                return self.own_method(c, name);
            }
        }
        None
    }

    /// Resolve `instance_of(class).method(...)`: MRO definition plus overriding project
    /// subclasses (Priority 4 CHA). A method no project class defines is the symbol of the
    /// base outside the project that defines it ([`Self::inherited_external`]), or unresolved.
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
        let mro = self.mro(&class);
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

    /// Whether reading `attr` on every instance of the project class `class` or of a project
    /// subclass (R2) runs one of the getters `keys`: what each such class takes the name from is
    /// one of them ([`Self::function_taken`]: not a plain attribute, a method, or a value stored on
    /// the class outside its body, and no base outside the project before it), and no class is a
    /// protocol, defines `__getattribute__`, or has a metaclass heki does not know.
    pub(crate) fn getters_complete(&self, class: &str, attr: &str, keys: &[FuncKey]) -> bool {
        let class = self.canonical(class);
        let read = self.attr_read(&class, attr);
        if read.getattribute || read.custom_metaclass {
            return false;
        }
        self.every_class_finds(&class, keys, |c| {
            self.protocol(c)
                .is_none()
                .then(|| self.function_taken(c, attr))
                .flatten()
        })
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
    /// as the call's single target), or it has none and the call raises. A protocol has
    /// implementations no class names, and a base heki knows nothing of may define the method.
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
                    (Inherited::External(external), Callee::External(s)) if *s == external
                ),
            },
        }
    }

    /// Resolve `class.method` on the class object `class` itself (`Base.method(obj)`, and the
    /// `__init__` of constructing it): the definition its MRO finds first. The overrides of
    /// subclasses do not count: the class object is no subclass.
    pub(crate) fn class_targets(
        &self,
        class: &str,
        method: &str,
        kb: &EffectiveEcosystemKb,
    ) -> Callee {
        let class = self.canonical(class);
        let mro = self.mro(&class);
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

    /// Whether a project function named `name` (its last name) may return an attribute of a
    /// parameter (`FunctionDecl::returns_param_attr`): no call or read with another name hands
    /// one out.
    pub(crate) fn may_return_param_attr(&self, name: &str) -> bool {
        self.read(|k| note(&mut k.returning_names, name));
        self.returning_names.contains(name)
    }

    /// Resolve reading `instance_of(class).attr` as calls of the property getters it may run
    /// (`FunctionDecl::is_property`), found like [`Self::method_targets`]' candidates: the MRO
    /// definition when it is a getter, plus the getters of overriding project subclasses
    /// (Priority 4 CHA), at most `CHA_CANDIDATE_LIMIT` of them (`TooMany` beyond). `None` when
    /// no modeled getter runs: a plain attribute, a method, an attribute of an external class
    /// (never a Layer 2 symbol: an `[[attributes]]` row types what a read gives, not what it
    /// runs), or a property with a setter or
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
        let found = self.definitions(&class, attr, skip_self, !skip_self, is_getter);
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
        let before_stores = self.completed.elsewhere.clone();
        self.structural.replace(completed.structural());
        self.completed.replace(completed);
        self.changed(&name, Some(&before), &before_stores)
    }

    /// Forget a deleted module; returns what changed.
    pub fn remove(&mut self, module: &str) -> DeclKeys {
        let before = self.structural.indexes();
        let before_stores = self.completed.elsewhere.clone();
        self.structural.remove(module);
        self.completed.remove(module);
        self.changed(module, Some(&before), &before_stores)
    }

    /// What changed when `module`'s declarations did: the module, the hierarchy indexes that
    /// differ from `before` (unchanged without it: the module's structure stayed), and what
    /// other modules' stores change that differs from `before_stores`
    /// (`ProjectDecls::elsewhere`, which the completed declarations decide).
    fn changed(
        &self,
        module: &str,
        before: Option<&Indexes>,
        before_stores: &stores::Elsewhere,
    ) -> DeclKeys {
        let mut out = before.map_or_else(DeclKeys::default, |b| self.structural.index_changes(b));
        out.modules.insert(module.to_owned());
        out.grown_globals
            .extend(before_stores.changes(&self.completed.elsewhere));
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
    let structural = structural_decls(input);
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
    let before_stores = passes.completed.elsewhere.clone();
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
        complete_against(input, structural_decls(input), &passes.structural, kb);
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
        .map(|m| (m.path.clone(), Arc::new(structural_decls(m))))
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

/// Pass 1: imports, classes (bases), functions (signatures) — no inference.
fn structural_decls(input: &ModuleInput<'_>) -> ModuleDecls {
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
    decls.aliases = module_aliases(input, &decls);
    let stores = GlobalStores::of(&input.ir.functions);
    decls.unknown_globals = unknown_globals(&stores, &decls);
    let rebound = rebound_imports(input, &stores, &decls);
    decls.rebound_imports = (!rebound.is_empty()).then(|| Box::new(rebound));
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
                .any(|d| decls.decorator_is(d, &decorators::names(|d| d.getter)));
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
        if !class.qualname.contains('.') {
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
        let custom_metaclass = unknown_base
            || metaclass_text.is_some_and(|text| {
                !matches!(
                    metaclass.as_deref().unwrap_or(text),
                    "type" | "builtins.type" | "abc.ABCMeta"
                )
            });
        let frozen_keyword = keyword("frozen").and_then(bool_literal);
        let is_protocol = bases
            .iter()
            .any(|b| matches!(b.as_str(), "typing.Protocol" | "typing_extensions.Protocol"));
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
    let mut returned_attrs: FxHashMap<&str, Option<(u8, String)>> = FxHashMap::default();
    for f in input
        .ir
        .functions
        .iter()
        .filter(|f| is_summarized_function(f))
    {
        let returned = returned_param_attr(f, &decls);
        returned_attrs
            .entry(&f.qualname)
            .and_modify(|r| {
                if *r != returned {
                    *r = None;
                }
            })
            .or_insert(returned);
    }
    // The return annotation of a property's getter (its setter shares the qualname).
    let mut getter_types: FxHashMap<&str, Ty> = FxHashMap::default();
    for f in &input.ir.functions {
        if is_summarized_function(f)
            && f.owner_class_qualname.is_some()
            && f.has_decorator(&decorators::names(|d| d.getter))
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
                    let c = crate::ConstArg::of_literal(p.default.as_ref()?)?;
                    Some((u8::try_from(i).ok()?, c))
                })
                .collect(),
            is_generator: is_generator_function(f),
            called_as_defined: is_called_as_defined(f),
            is_property: getters_only[f.qualname.as_str()],
            readonly_params,
            mutable_params,
            transform: transforms[f.qualname.as_str()],
            offloaded_by: None,
            is_abstract: crate::is_abstract_method(&f.decorators),
            property_type: getter_types.get(f.qualname.as_str()).cloned(),
            returns_param_attr: returned_attrs[f.qualname.as_str()].clone(),
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
    (decls.grown_globals, decls.rebound_globals) = foreign_grown_globals(input, &decls.imports);
    let path_changes = package_paths::path_changes(input, &decls);
    decls.path_changes = (!path_changes.is_empty()).then(|| Box::new(path_changes));
    attr_displays(input, &mut decls);
    code_attrs(input, &mut decls);
    let rebound: Vec<(String, String)> = class_attrs_rebound(input, &decls)
        .into_iter()
        .map(|(class, attr)| (class.to_owned(), attr.to_owned()))
        .collect();
    for (class, attr) in rebound {
        if let Some(c) = decls.classes.get_mut(&class) {
            c.rebound_attrs.insert(attr);
        }
    }
    decls.str_constants = str_constants(input, &decls);
    decls.literal_constants = literal_constants(input, &decls);
    decls.object_sentinels = sentinels::object_sentinels(input, &decls);
    sentinels::named_attrs(input, &mut decls);
    decls.copy_registrations = sentinels::copy_registrations(input, &decls);
    decls
}

/// `ModuleDecls::grown_globals`: the loads of an imported module's global (`cfg.QUEUE`, or
/// `QUEUE` after `from cfg import QUEUE`) whose object a function may grow, and the globals
/// it stores into or deletes on an imported module (`cfg.QUEUE = []`), decided by the shared
/// `heki_ir` facts ([`FnIndex::grown_vars`]); and those stores and deletes alone
/// (`ModuleDecls::rebound_globals`).
fn foreign_grown_globals(
    input: &ModuleInput<'_>,
    imports: &BTreeMap<String, String>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut out = BTreeSet::new();
    let mut rebound = BTreeSet::new();
    if imports.is_empty() {
        return (out, rebound);
    }
    for f in &input.ir.functions {
        let ix = FnIndex::new(f, &input.ir.constants);
        let mut loads: Vec<(VarId, String)> = Vec::new();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            match &inst.kind {
                InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr } => {
                    if let Some(module) = imported_fqn(&ix, imports, *obj, 0) {
                        rebound.insert(format!("{module}.{attr}"));
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
    (out, rebound)
}

/// The fqn `v` names through this module's runtime imports (`cfg`, `cfg.QUEUE`), through
/// copies and attribute loads.
fn imported_fqn(
    ix: &FnIndex<'_>,
    imports: &BTreeMap<String, String>,
    v: VarId,
    depth: u32,
) -> Option<String> {
    if depth > 8 {
        return None;
    }
    match &ix.shape.inst(v)?.kind {
        InstKind::Copy { src, .. } => imported_fqn(ix, imports, *src, depth + 1),
        InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. } => {
            imports.get(name).cloned()
        }
        InstKind::Import { dst, .. } => ix.f.var_name(*dst).and_then(|n| imports.get(n)).cloned(),
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
/// of a generic container's items (one level: `list[Model]`, `dict[str, Model]`, `tuple[A, B]`,
/// `list[A | B]`; `dict[str, list[Model]]` names `str` and `list`).
fn ty_classes(ty: &Ty, out: &mut Vec<String>) {
    for obj in ty.members() {
        out.push(obj.class.clone());
        let items: Vec<&Elem> = match &obj.args {
            Args::None => Vec::new(),
            Args::Items(e) => vec![e.as_ref()],
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

/// The methods that decide what `copy.deepcopy` makes of an instance in place of copying its
/// attributes ([`ProjectDecls::deep_copy_of`]).
const COPY_HOOKS: [&str; 5] = [
    "__deepcopy__",
    "__reduce__",
    "__reduce_ex__",
    "__getstate__",
    "__setstate__",
];

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
/// name (`constant_source`). A module-level literal is the text of one of the module's
/// constants (`heki_ir::ModuleConstants::strs`), shared with it, and a name those leave out is
/// no constant. A name stored twice (`global KEY` in a function, two branches) or deleted is no
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
                Some(Err(fqn)) => StrValue::Alias(fqn),
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

/// [`FunctionDecl::returns_param_attr`] of one definition: a function (not a generator or a
/// coroutine, decorated by nothing but `staticmethod`, `classmethod`, `abstractmethod`,
/// `override`, or `property`) whose every `return` returns the same attribute of the same
/// positional parameter, read directly or through local copies, and which stores or deletes no
/// attribute of that name. A `cached_property` is none: a read after the first returns what
/// the instance holds, which an assignment can replace.
fn returned_param_attr(f: &IrFunction, decls: &ModuleDecls) -> Option<(u8, String)> {
    let plain_decorators = f.decorators.iter().all(|d| {
        let (head, _) = decorator_parts(d);
        decorators::keeps(head.rsplit('.').next().unwrap_or(head), |d| d.keeps_call)
            || decls.decorator_is(d, &["property"])
    });
    if f.kind != FunctionKind::Sync || !plain_decorators {
        return None;
    }
    let defs: FxHashMap<VarId, &InstKind> = f
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| Some((i.kind.dst()?, &i.kind)))
        .collect();
    // The variable each copy copies in the end, found once per variable: returns of copies of
    // one value share the walk. Copies of an SSA variable form a tree, so the walk ends.
    let mut roots: FxHashMap<VarId, VarId> = FxHashMap::default();
    let mut copied_from = |var: VarId| {
        let mut walked = Vec::new();
        let mut cur = var;
        let root = loop {
            if let Some(&root) = roots.get(&cur) {
                break root;
            }
            match defs.get(&cur) {
                Some(InstKind::Copy { src, .. }) => {
                    walked.push(cur);
                    cur = *src;
                }
                _ => break cur,
            }
        };
        for v in walked {
            roots.insert(v, root);
        }
        root
    };
    let mut returned = None;
    for bb in &f.blocks {
        let Terminator::Return(value) = &bb.terminator else {
            continue;
        };
        let Some(InstKind::AttrLoad { obj, attr, .. }) = defs.get(&copied_from((*value)?)) else {
            return None;
        };
        let holder = copied_from(*obj);
        let param = f
            .params
            .iter()
            .position(|p| p.var == holder && !p.is_vararg && !p.is_kwarg && !p.kw_only)?;
        let this = (u8::try_from(param).ok()?, attr.clone());
        if returned.get_or_insert_with(|| this.clone()) != &this {
            return None;
        }
    }
    // A function storing or deleting the attribute (`items = self.items; self.items = [];
    // return items`) may return what the attribute held before: no longer the attribute.
    let (_, attr) = returned.as_ref()?;
    let stores_it = f.blocks.iter().flat_map(|b| &b.insts).any(|i| {
        matches!(&i.kind, InstKind::AttrStore { attr: a, .. } | InstKind::AttrDelete { attr: a, .. } if a == attr)
    });
    returned.filter(|_| !stores_it)
}

/// [`ModuleDecls::unknown_globals`]: the globals the module stores or deletes (`stores`) that are
/// no runtime import, class, or function of it.
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

/// [`ModuleDecls::aliases`]: the module-level names bound exactly once, not by an import
/// statement (also none under `if TYPE_CHECKING:`), to a name the module reads as another
/// object at run time (an import, a builtin, a class or function the module defines) or to an
/// attribute of one (`object.__setattr__`, `helpers.run`). A name bound in two places or
/// deleted is none, and neither is one bound to anything else (a call's result, a decorated
/// function, a choice between two objects).
fn module_aliases(input: &ModuleInput<'_>, decls: &ModuleDecls) -> BTreeMap<String, String> {
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
    let imported: FxHashSet<&str> = module_imports()
        .map(|imp| imp.bound_name.as_str())
        .collect();
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
        if !stores.once(name) || imported.contains(name.as_str()) {
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
    /// attribute of one. Each variable is resolved once (`known`): a chain of aliases bound to
    /// one another (`a1 = a0`, `a2 = a1`, ...) shares the walk.
    fn object(&self, var: VarId, known: &mut FxHashMap<VarId, Option<String>>) -> Option<String> {
        // The variables walked through, each with the attribute it loads (`None`: a copy).
        let mut walked: Vec<(VarId, Option<&str>)> = Vec::new();
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
                    walked.push((cur, None));
                    cur = *src;
                    continue;
                }
                InstKind::AttrLoad { obj, attr, .. } => {
                    walked.push((cur, Some(attr.as_str())));
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
        for (v, attr) in walked.into_iter().rev() {
            if let (Some(base), Some(attr)) = (&object, attr) {
                // A name of more than a few dotted segments is no alias anyone writes; the cap
                // keeps a chain of attribute loads (`a2 = a1.x`) linear.
                object = (base.matches('.').count() < MAX_ALIAS_SEGMENTS)
                    .then(|| format!("{base}.{attr}"));
            }
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

/// The class attributes the module stores into or deletes outside their class bodies, as
/// `(class qualname, attribute)`: on the class object (`Keys.KEY = "x"` in a function, or in
/// the module body after the `class` statement, where the name holds the class it made;
/// `cls.KEY = "x"` in a class method) or on an instance in a method of the class
/// (`self.KEY = "x"`, which shadows the class attribute for that instance).
fn class_attrs_rebound<'i>(
    input: &'i ModuleInput<'_>,
    decls: &ModuleDecls,
) -> FxHashSet<(&'i str, &'i str)> {
    let mut out = FxHashSet::default();
    for f in &input.ir.functions {
        let insts = || f.blocks.iter().flat_map(|b| &b.insts);
        let defs: FxHashMap<VarId, &InstKind> = insts()
            .filter_map(|i| Some((i.kind.dst()?, &i.kind)))
            .collect();
        let shape = FnShape::new(f);
        let receiver = match (&f.owner_class_qualname, f.params.first()) {
            (Some(owner), Some(p))
                if is_summarized_function(f) && !f.has_decorator(&["staticmethod"]) =>
            {
                Some((owner.as_str(), self_aliases(f, p.var)))
            }
            _ => None,
        };
        for inst in insts() {
            let (InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr }) =
                &inst.kind
            else {
                continue;
            };
            let class = match defs.get(&shape.copy_root(*obj)) {
                Some(InstKind::LoadGlobal { name, .. } | InstKind::LoadName { name, .. })
                    if decls.classes.contains_key(name.as_str()) =>
                {
                    Some(name.as_str())
                }
                Some(InstKind::MakeClosure {
                    func_id: Some(id), ..
                }) if input.ir.function(*id).is_class_body => {
                    Some(input.ir.function(*id).qualname.as_str())
                }
                _ => receiver
                    .as_ref()
                    .filter(|(_, selfs)| selfs.contains(obj))
                    .map(|(owner, _)| *owner),
            };
            if let Some(class) = class {
                out.insert((class, attr.as_str()));
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
            if ann.target.contains('.') || !container {
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
                    (!attr.contains('.') && q.is_final && container).then(|| attr.to_owned())
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
}

/// `ClassDecl::properties` of the module's classes: their methods decorated with `property`
/// (`abc.abstractproperty`) or with an accessor of one (`@x.setter`, `@x.deleter`,
/// `@x.getter`), and the class-body names bound to a call of the builtin `property`.
/// `functools.cached_property` is no data descriptor: a store sets the instance's attribute.
fn class_properties(input: &ModuleInput<'_>, decls: &mut ModuleDecls) {
    let builtin_property = !decls.imports.contains_key("property")
        && !decls.functions.contains_key("property")
        && !decls.classes.contains_key("property");
    let mut found: Vec<(String, String)> = Vec::new();
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
        // A method defined directly in its class (`C.m`, not `C.m.<locals>.f`).
        let method = f.owner_class_qualname.as_deref().and_then(|owner| {
            let name = f.qualname.strip_prefix(owner)?.strip_prefix('.')?;
            (!name.contains('.')).then_some((owner, name))
        });
        let Some((owner, name)) = method else {
            continue;
        };
        let is_property = f.decorators.iter().any(|d| {
            let (head, _) = decorator_parts(d);
            decls.decorator_is(d, &["property", "abstractproperty"])
                || head
                    .rsplit_once('.')
                    .is_some_and(|(_, a)| matches!(a, "setter" | "deleter" | "getter"))
        });
        if is_property {
            found.push((owner.to_owned(), name.to_owned()));
        }
    }
    for (class, name) in found {
        if let Some(c) = decls.classes.get_mut(&class) {
            c.properties.insert(name);
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
        && is_called_as_defined(f)
}

/// Every decorator of `f` keeps calling it what its definition makes it
/// ([`decorators::Decorator::keeps_call`]).
fn is_called_as_defined(f: &IrFunction) -> bool {
    decorators::all_keep(&f.decorators, |d| d.keeps_call)
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
    }
    // After the annotations, which type every store of the attributes they declare.
    stores::record(input, project, kb, &mut decls);
    crate::capacity::record(input, &cx, &mut decls);
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
/// its result and its arguments: the one its callee holds (`val`, as for [`namespace_load`]), or
/// the member of the module or class a method is called on, as [`TypeCtx::method_call_targets`]
/// names it (`builtins.setattr(obj, name, v)` calls `builtins.setattr`).
pub(crate) fn plain_call<'k, 'v>(
    kind: &'k InstKind,
    fqns: &[&'static str],
    val: impl FnOnce(VarId) -> Option<&'v Val>,
) -> Option<(&'static str, VarId, &'k [VarId])> {
    let (called, dst, args) = match kind {
        InstKind::Call {
            dst,
            callee_var: Some(f),
            args,
            kwargs,
            star_from: None,
            ..
        } if kwargs.is_empty() => match val(*f)? {
            Val::Ref(s) => (fqns.iter().find(|fqn| **fqn == s.as_str())?, dst, args),
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
            Val::Ref(owner) => {
                let member = |fqn: &&&str| {
                    fqn.strip_suffix(method.as_str())
                        .and_then(|o| o.strip_suffix('.'))
                        == Some(owner.as_str())
                };
                (fqns.iter().find(member)?, dst, args)
            }
            _ => return None,
        },
        _ => return None,
    };
    Some((called, *dst, args.as_slice()))
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
/// catalog's argument cases (`heki_kb::EcosystemEntry::case_for`), as `fact` describes it.
/// Nothing is known of an argument `*` / `**` unpacking may pass.
pub(crate) fn passed_arg(
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    star_from: Option<u8>,
    at: (Option<u8>, Option<&str>),
    fact: impl Fn(VarId) -> ArgFact,
) -> Option<ArgFact> {
    passed_var(args, kwargs, star_from, at).map(fact)
}

/// The variable a call passes at position `index` or by `keyword` ([`passed_arg`]).
pub(crate) fn passed_var(
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    star_from: Option<u8>,
    (index, keyword): (Option<u8>, Option<&str>),
) -> Option<VarId> {
    if let Some(k) = keyword
        && let Some((_, v)) = kwargs.iter().find(|(name, _)| name.as_deref() == Some(k))
    {
        return Some(*v);
    }
    let i = index?;
    if star_from.is_some_and(|s| i >= s) {
        return None;
    }
    args.get(usize::from(i)).copied()
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
fn literal_value(literal: &ConstLiteral) -> Option<LiteralValue> {
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
/// ([`names_a_file`]).
pub(crate) fn arg_fact<'i>(
    def: impl Fn(VarId) -> Option<&'i Inst>,
    class_of: impl Fn(VarId) -> Option<String>,
    name_of: impl Fn(VarId) -> Option<String>,
    globals: &dyn Fn(&Inst) -> Option<LiteralValue>,
    var: VarId,
) -> ArgFact {
    ArgFact {
        literal: literal(&def, globals, var),
        names_file: names_a_file(&def, &name_of, globals, var),
        class: class_of(var).or_else(|| bytes_literal_class(&def, var)),
        flags: set_flags(&def, &name_of, var),
    }
}

/// The class of a `bytes` literal, which carries no inferred type of its own.
fn bytes_literal_class<'i>(def: &impl Fn(VarId) -> Option<&'i Inst>, var: VarId) -> Option<String> {
    matches!(
        def(var)?.kind,
        InstKind::Const {
            literal: ConstLiteral::Bytes,
            ..
        }
    )
    .then(|| "builtins.bytes".to_owned())
}

/// Whether a database name opens a database in memory (`sqlite3.connect`), or opens none that
/// heki can name: the empty name is a temporary database.
fn in_memory_name(text: &str) -> bool {
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
/// (`globals`, read by name, through an import, or as a module's attribute) names a file unless it has an in-memory form ([`in_memory_name`]), and so does
/// `__file__`; a copy, `+`, `/`, and `.parent` are made from their operands, a [`PATH_FUNCTIONS`]
/// call (the called object or receiver resolved by `name_of`) from its arguments, and a
/// [`PATH_METHODS`] call from its receiver and arguments. Anything else (a parameter, a phi, an
/// f-string, an environment lookup, another attribute or call) may name an in-memory database.
fn file_part<'i>(
    def: &impl Fn(VarId) -> Option<&'i Inst>,
    name_of: &impl Fn(VarId) -> Option<String>,
    globals: &dyn Fn(&Inst) -> Option<LiteralValue>,
    var: VarId,
) -> FilePart {
    let path_function =
        |name: Option<String>| name.is_some_and(|n| PATH_FUNCTIONS.contains(&n.as_str()));
    let inst = def(var);
    let names_file = |inst: &Inst| matches!(globals(inst), Some(LiteralValue::Str(text)) if !in_memory_name(&text));
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
    var: VarId,
) -> bool {
    let budget = Budget::unmetered("heki_effect::resolver::names_a_file");
    let mut steps = 0usize;
    let mut leaves = 0usize;
    let part = |v| file_part(def, name_of, globals, v);
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

impl Recv<'_> {
    /// Whether the receiver says nothing of the code a call on it runs: a value of `Any` or
    /// `object` ([`of_any_class`]) or of `Callable` ([`is_any_callable`]), or a name nothing heki
    /// sees binds ([`is_unbound_name`]: one a star import of a library binds, or none, and the
    /// call raises `NameError`). Its library name (`typing.Any.save`, `builtins.undefined`) is
    /// no library's function, so the call is unresolved, with effects heki does not know.
    fn says_nothing(self) -> bool {
        match self {
            Recv::Inst(class) | Recv::Class(class) => of_any_class(class) || is_any_callable(class),
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
            Val::Inst(c) => Some(Recv::Inst(c)),
            Val::Class(c) => Some(Recv::Class(c)),
            Val::Super(c) => Some(Recv::Super(c)),
            Val::Union(_) => None,
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
            Ty::SelfType | Ty::Partial(_) | Ty::Coroutine(_) => None,
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
    /// The module-level names bound once to a set of text constants
    /// ([`Self::string_set_global`]).
    string_sets: std::cell::OnceCell<FxHashSet<String>>,
    /// [`ProjectDecls::function_taken`] by class and name, asked once per module (the module's
    /// summaries are one recording, which the first answer's reads join).
    taken: RefCell<FxHashMap<(String, String), Option<FuncKey>>>,
    /// What reading the module's globals and its classes' attributes gives while
    /// [`stores::record`] decides their types.
    store_reads: Option<&'a dyn stores::StoreReads>,
}

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
    classes: BTreeMap::new(),
    functions: BTreeMap::new(),
    global_types: BTreeMap::new(),
    aliases: BTreeMap::new(),
    unknown_globals: BTreeSet::new(),
    rebound_imports: None,
    small_globals: BTreeMap::new(),
    grown_globals: BTreeSet::new(),
    rebound_globals: BTreeSet::new(),
    stores_elsewhere: None,
    store_fallbacks: None,
    calls_setstate: false,
    exports: None,
    path_changes: None,
    str_constants: BTreeMap::new(),
    literal_constants: BTreeMap::new(),
    object_sentinels: BTreeSet::new(),
    copy_registrations: BTreeSet::new(),
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
            module_name: input.module_name.clone(),
            ir: input.ir,
            import_by_span,
            scopes: crate::infer::Scopes::default(),
            captured: std::sync::OnceLock::new(),
            bindings: std::cell::OnceCell::new(),
            string_sets: std::cell::OnceCell::new(),
            taken: RefCell::default(),
            store_reads: None,
        }
    }

    /// Whether the module-level name `name` is bound once, in the module body, to a set of
    /// text constants (a display, or `set()` / `frozenset()` of one): `EFF112`.
    pub(crate) fn string_set_global(&self, name: &str) -> bool {
        self.string_sets
            .get_or_init(|| crate::order_flow::string_set_globals(self.ir))
            .contains(name)
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
    /// what an import binds (`from pkg.locks import LOCK`), or this module's own global.
    pub fn global_fqn(&self, name: &str) -> String {
        match self.module.imports.get(name) {
            Some(t) => self.project.canonical(t),
            None => format!("{}.{name}", self.module_name()),
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
            return Ty::Ref(format!("{}.{name}", self.module_name()));
        }
        if let Some(t) = self
            .project
            .global_type(self.module, name)
            .and_then(|t| self.project.declared_ty(&t))
        {
            return t;
        }
        if let Some(target) = self.module.aliases.get(name)
            && !self.project.alias_rebound(self.module_name(), name)
        {
            return self.named(target);
        }
        // A global the module binds is its own, of no known value ([`Symbol::Unknown`]).
        if self.module.unknown_globals.contains(name) {
            return Ty::Ref(format!("{}.{name}", self.module_name()));
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
    /// its own constants, or one it imports (`from pkg.keys import KEY`).
    pub fn global_str(&self, name: &str) -> Option<&'a str> {
        let Some(constant) = self.module.str_constants.get(name) else {
            return self
                .module
                .fixed_import(name)
                .and_then(|fqn| self.project.str_constant(fqn));
        };
        let fqn = || format!("{}.{name}", self.module.module_name);
        if constant.rebindable && self.project.rebound_elsewhere(&fqn()) {
            return None;
        }
        match &constant.value {
            StrValue::Literal(v) => Some(v),
            StrValue::Alias(next) => self.project.str_constant(next),
        }
    }

    /// A named fqn: a project global holding an instance becomes that instance, a module
    /// alias the object it names.
    pub(crate) fn named(&self, fqn: &str) -> Ty {
        let fqn = self.project.object_named(fqn);
        match self.project.symbol(&fqn) {
            Symbol::Global(t) => self.project.declared_ty(&t).unwrap_or(Ty::Ref(fqn)),
            _ => Ty::Ref(fqn),
        }
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

    /// Instance produced by calling the object `fqn`, if known; `arg` tells what the call
    /// passes (for the catalog's argument cases).
    pub(crate) fn call_result(
        &self,
        fqn: &str,
        arg: impl FnMut(Option<u8>, Option<&str>) -> Option<ArgFact>,
    ) -> Option<Ty> {
        match self.called_symbol(fqn) {
            Symbol::Class(_) => return Some(Ty::inst(fqn)),
            Symbol::Function(_, decl) => {
                return self
                    .declared_result(fqn, arg)
                    .or_else(|| {
                        decl.return_type
                            .as_ref()
                            .and_then(|t| self.project.declared_ty(t))
                            .and_then(|t| t.bind_self(None))
                    })
                    .map(|t| decl.call_type(t));
            }
            Symbol::Module | Symbol::Global(_) | Symbol::Unknown | Symbol::Unread => return None,
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
        let entry = self.catalog(fqn)?;
        if let Some(rt) = entry.return_type_in(entry.case_for(arg)) {
            return Some(Ty::inst(rt));
        }
        (entry.effects.contains(&ExecEffectKind::UnpooledClientAlloc)
            && !entry.effects.contains(&ExecEffectKind::BlockingNet))
        .then(|| Ty::inst(fqn))
    }

    /// Resolve calling a value (`f(...)`).
    pub fn resolve_call(&self, callee: Option<&Val>) -> Callee {
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
            Val::Inst(class) => self.project.protocol_implementers(class),
            Val::Ref(_) | Val::Class(_) | Val::Super(_) => None,
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
            Some(Recv::Super(_)) | None => Callee::Unresolved,
        }
    }

    /// Whether `callee`, what [`Self::resolve_method`] resolves `recv.method(...)` to, is every
    /// implementation the call can run (`CallSite::complete`): each member of a union resolves
    /// and is complete, a protocol receiver never is, and a module member, a class object's
    /// own attribute, or `super()` names one implementation. A call of a property's value is
    /// judged by the value ([`Self::targets_complete_on`]).
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
                .all(|c| self.project.getters_complete(c, attr, keys)),
            Some(Val::Super(owner)) => self.project.super_getters_complete(owner, attr, keys),
            // A read on a class or module object runs no getter (`resolve_attr_load` resolves
            // none).
            _ => {
                matches!(recv, Some(Val::Inst(class)) if self.project.getters_complete(class, attr, keys))
            }
        }
    }

    /// [`Self::method_targets_complete`] on a receiver of one class. A call of a property's value
    /// runs what calling the value runs, whatever class the getter is found in (the getter's run
    /// is a read of its own): it is complete when calling a value of the type the getter
    /// declares is ([`Self::call_targets_complete`]).
    fn targets_complete_on(&self, recv: Recv<'_>, method: &str, callee: &Callee) -> bool {
        if let Some(value) = self.property_value(recv, method) {
            return self.call_targets_complete(value.as_ref(), callee);
        }
        match recv {
            Recv::Inst(class) => self
                .project
                .instance_targets_complete(class, method, callee, self.kb),
            Recv::Class(class) => {
                self.project.is_project_class(class)
                    && self
                        .project
                        .instance_targets_complete(class, method, callee, self.kb)
            }
            Recv::Ref(_) | Recv::Super(_) => true,
        }
    }

    /// Whether `callee`, what [`Self::resolve_call`] resolves calling `value` to, is every
    /// implementation the call can run: a function or class named exactly, or an instance's
    /// `__call__` as for a method.
    pub fn call_targets_complete(&self, value: Option<&Val>, callee: &Callee) -> bool {
        match value {
            Some(Val::Ref(_)) => true,
            Some(Val::Inst(class)) => self
                .project
                .instance_targets_complete(class, "__call__", callee, self.kb),
            Some(Val::Union(_) | Val::Class(_) | Val::Super(_)) | None => false,
        }
    }

    /// Resolve `recv.method(...)`.
    pub fn resolve_method(&self, recv: Option<&Val>, method: &str) -> Callee {
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
        match recv.filter(|r| !r.says_nothing()) {
            Some(Recv::Ref(fqn)) => {
                if self.project.is_project_class(fqn) {
                    return self.through_class(self.project.class_targets(fqn, method, self.kb));
                }
                // A module alias called through its module (`aliased.nap(1)`) is the object it
                // names, as the name imported from the module is.
                let target = self.project.object_named(&format!("{fqn}.{method}"));
                self.call_targets(Some(Recv::Ref(&target)))
            }
            Some(Recv::Class(class)) => {
                self.through_class(self.project.method_targets(class, method, false, self.kb))
            }
            Some(recv @ (Recv::Inst(class) | Recv::Super(class))) => {
                let skip_self = matches!(recv, Recv::Super(_));
                // `obj.prop(...)` runs the getter as a read of `obj.prop` does (where the summary
                // records it), then calls the value it gives.
                match self.property_value(recv, method) {
                    Some(value) => self.resolve_call(value.as_ref()),
                    None => self
                        .project
                        .method_targets(class, method, skip_self, self.kb),
                }
            }
            None => Callee::Unresolved,
        }
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
            Recv::Inst(class) => (class, false),
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

    /// Whether a read or call of `name` on `recv` runs one of `keys` whatever class `recv` is:
    /// every class of a union or implementation of a protocol, as a class of its own, takes
    /// `name` from one of them ([`ProjectDecls::function_taken`]). A module function, a class
    /// object, and `super()` dispatch on no instance: they run what they name.
    pub(crate) fn runs_on_every_class(
        &self,
        recv: Option<&Val>,
        name: &str,
        keys: &[FuncKey],
    ) -> bool {
        let classes = match recv {
            Some(Val::Union(classes)) => classes.as_slice(),
            Some(Val::Inst(class)) => self
                .project
                .protocol_implementers(class)
                .unwrap_or(std::slice::from_ref(class)),
            Some(Val::Ref(_) | Val::Class(_) | Val::Super(_)) | None => return true,
        };
        classes.iter().all(|class| {
            let key = (class.clone(), name.to_owned());
            let known = self.taken.borrow().get(&key).cloned();
            let taken = known.unwrap_or_else(|| {
                let taken = self.project.function_taken(class, name);
                self.taken.borrow_mut().insert(key, taken.clone());
                taken
            });
            taken.is_some_and(|k| keys.contains(&k))
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
                    .property_targets(c, attr, false)
                    .unwrap_or(Callee::Unresolved)
            };
            return match union_targets(classes, getters, |_| true) {
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

    /// The class a `[[tool.heki.ecosystem]]` entry naming the project function `fqn` gives its
    /// result (the declaration wins over its annotation).
    fn declared_result(
        &self,
        fqn: &str,
        arg: impl FnMut(Option<u8>, Option<&str>) -> Option<ArgFact>,
    ) -> Option<Ty> {
        let entry = self.kb.declared(fqn)?;
        entry.return_type_in(entry.case_for(arg)).map(Ty::inst)
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
                    .or_else(|| {
                        decl.return_type
                            .as_ref()
                            .and_then(|t| self.project.declared_ty(t))
                            .and_then(|t| t.bind_self(None))
                    })
                    .map(|t| decl.call_type(t))
            }
            Callee::External(fqn) => self.call_result(fqn, arg),
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
        FxHashMap<(BlockId, VarId), Val>,
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

    /// The module constant `inst` reads ([`Self::global_read`]): a string, a number, or a
    /// boolean bound once, under the rules on rebinding and imports of [`Self::global_literal`]
    /// and [`ProjectDecls::literal_constant`].
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
                        Val::Inst(class) if project.is_project_class(class) => {
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
}
