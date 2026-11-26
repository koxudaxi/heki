//! Environment resolution, `RuntimeProfile`, and Ruff/`t-linter`-compatible `heki` configuration.

mod declared;
mod defaults;
pub mod glob;
pub mod problem;
mod schema;
mod stdlib;

pub use declared::{Declared, EntryPoint, normalize_distribution, requirement_name};
pub use defaults::with_defaults;
pub use problem::{Docs, Explain, Problem, Setting};
pub use stdlib::is_stdlib_module;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Target Python minor version driving semantics (PEP 709, PEP 649, PEP 810) and KB projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PythonVersion {
    pub major: u8,
    pub minor: u8,
}

impl PythonVersion {
    pub const PY311: Self = Self {
        major: 3,
        minor: 11,
    };
    pub const PY312: Self = Self {
        major: 3,
        minor: 12,
    };
    pub const PY313: Self = Self {
        major: 3,
        minor: 13,
    };
    pub const PY314: Self = Self {
        major: 3,
        minor: 14,
    };
    pub const PY315: Self = Self {
        major: 3,
        minor: 15,
    };

    /// The versions heki checks, the oldest first.
    pub const SUPPORTED: [Self; 5] = [
        Self::PY311,
        Self::PY312,
        Self::PY313,
        Self::PY314,
        Self::PY315,
    ];

    pub fn parse_tag(tag: &str) -> Option<Self> {
        match tag {
            "py311" | "3.11" => Some(Self::PY311),
            "py312" | "3.12" => Some(Self::PY312),
            "py313" | "3.13" => Some(Self::PY313),
            "py314" | "3.14" => Some(Self::PY314),
            "py315" | "3.15" => Some(Self::PY315),
            _ => None,
        }
    }

    /// The `target-version` spelling of the version (`py312`), which [`Self::parse_tag`] reads.
    pub fn tag(self) -> String {
        format!("py{}{}", self.major, self.minor)
    }
}

/// Materialized runtime feature profile (no version branching inside analysis passes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProfile {
    pub python_version: PythonVersion,
    /// PEP 709 (3.12+): list/set/dict comprehensions are inlined into enclosing frames.
    pub pep709_inlined_comprehensions: bool,
    /// PEP 649 (3.14+): annotations are evaluated lazily in `__annotate__` scopes.
    pub pep649_deferred_annotations: bool,
    /// PEP 810 (3.15+): explicit `lazy import` / `lazy from ... import ...`.
    pub pep810_lazy_imports: bool,
    /// PEP 661 (3.15+): the builtin `sentinel`, whose objects `copy.copy` and `copy.deepcopy` keep.
    pub pep661_builtin_sentinel: bool,
}

impl From<PythonVersion> for RuntimeProfile {
    fn from(v: PythonVersion) -> Self {
        Self {
            python_version: v,
            pep709_inlined_comprehensions: v >= PythonVersion::PY312,
            pep649_deferred_annotations: v >= PythonVersion::PY314,
            pep810_lazy_imports: v >= PythonVersion::PY315,
            pep661_builtin_sentinel: v >= PythonVersion::PY315,
        }
    }
}

/// Diagnostic severity override (matches `t-linter` / `ty`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    /// The name the configuration and the reports write.
    pub const fn word(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// `[tool.heki.lint]` section (Ruff + `t-linter` conventions, kebab-case).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct LintConfig {
    /// `None` when the key is absent (the default rules); `Some([])` selects no rule before
    /// `extend-select`, as in Ruff.
    pub select: Option<Vec<String>>,
    pub extend_select: Vec<String>,
    pub ignore: Vec<String>,
    pub severity: BTreeMap<String, Severity>,
    pub per_file_ignores: BTreeMap<String, Vec<String>>,
    /// `EFF101` / `EFF105` / `EFF106` context relaxation for test code (`test_*` functions and files under
    /// `tests/`). Defaults to `true`; set `false` to check test files (e.g. rule fixtures).
    pub relax_async_in_tests: Option<bool>,
    /// Whether the heuristic rules that are not opt-in run by default and for a prefix or `ALL`,
    /// as the exact ones do (Ruff's `preview`). Defaults to `false`.
    pub heuristic: Option<bool>,
    /// The least severity whose reports make `heki check` fail (exit code 1). Defaults to
    /// `error`: warnings, and heuristic reports, which count as warnings, are reported without
    /// failing the run.
    pub fail_on: Option<Severity>,
    /// Report the codes of suppression comments that suppressed no report of a rule that ran,
    /// and the baseline entries that matched no report. Defaults to `false`.
    pub report_unused_suppressions: Option<bool>,
    /// Report `# heki: ignore` comments that name no rule. Defaults to `false`.
    pub require_suppression_codes: Option<bool>,
    /// What `heki check` does with a contract entry that matches nothing in the analyzed files.
    /// Unset, a run over the whole project warns about them.
    pub unmatched_contract_entries: Option<UnmatchedEntries>,
}

impl LintConfig {
    /// `fail-on`, or its default `error`.
    pub fn fail_on_severity(&self) -> Severity {
        self.fail_on.unwrap_or(Severity::Error)
    }
}

/// `lint.unmatched-contract-entries`: how `heki check` reports a contract entry that names no
/// definition, attribute, method, or `__dict__` key in the analyzed files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnmatchedEntries {
    /// A warning: the run does not fail.
    #[default]
    Warning,
    /// An error: the run fails.
    Error,
    /// Not reported.
    Ignore,
}

/// `[[tool.heki.write-owners]]` contract for `MUT201`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct WriteOwnerRule {
    pub name: String,
    pub targets: Vec<String>,
    pub allowed_writers: Vec<String>,
    /// When set, only writes by these functions (dotted patterns) are reported; `allowed-writers`
    /// and `exempt-methods` stay exceptions inside them.
    pub forbidden_writers: Vec<String>,
    pub forbid_cached_property_on: Vec<String>,
    /// Construction / copy methods allowed to write the targets (`__deepcopy__`,
    /// `Model.copy_deep`): a method name or a dotted pattern.
    pub exempt_methods: Vec<String>,
    /// The help of every `MUT201` report of this contract (`use Store.set_fields() instead`).
    pub message: Option<String>,
    /// `"allow"` (default) or `"report"`: whether writes that construct the object they write
    /// (allocated in the same call chain, held by nobody else yet) are reported.
    pub fresh_objects: Option<String>,
    /// `"report"`: also report (`MUT205`) writes of an attribute named like a target on an
    /// object of unknown class, and computed attribute names written on a target class;
    /// `"ignore"` (default) does not.
    pub unresolved: Option<String>,
    /// `"lexical"`: a write is judged in the function that holds it; `"call-graph"` (default):
    /// a private helper hands its writes to its callers, and a function that only runs inside
    /// allowed writers is allowed.
    pub attribution: Option<String>,
    /// `"allow"`: a write through the receiver of the method it is judged in (`self`, `cls`,
    /// `super()`) is allowed; `"report"` (default) judges it like any other.
    pub own_receiver: Option<String>,
    /// Project protocols (class patterns, their subclasses included): a write through a value
    /// typed with one that declares the attribute is allowed.
    pub allowed_protocols: Vec<String>,
    /// `false`: an `allowed-protocols` pattern names the protocols it matches only, not their
    /// subprotocols; `true` (default) covers them.
    pub protocol_subclasses: Option<bool>,
    /// `"report"`: also report (`MUT201`) a constant key written through the namespace of an
    /// object of a target's class (`obj.__dict__[KEY] = v`, `vars(obj)`) that no target of any
    /// `write-owners` contract covering the class names; `"ignore"` (default) does not.
    pub unlisted_dict_keys: Option<String>,
}

/// `[[tool.heki.read-owners]]` contract for `MUT203` (the read side of `write-owners`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ReadOwnerRule {
    pub name: String,
    pub targets: Vec<String>,
    pub allowed_readers: Vec<String>,
    /// When set, only reads by these functions are reported (see `WriteOwnerRule`).
    pub forbidden_readers: Vec<String>,
    /// Methods allowed to read the targets: a method name or a dotted pattern.
    pub exempt_methods: Vec<String>,
    /// The help of every `MUT203` report of this contract.
    pub message: Option<String>,
    /// `"report"`: also report (`MUT206`) reads of an attribute named like a target on an
    /// object of unknown class; `"ignore"` (default) does not.
    pub unresolved: Option<String>,
    /// `"lexical"` or `"call-graph"` (default), as for `WriteOwnerRule`.
    pub attribution: Option<String>,
    /// `"allow"` or `"report"` (default), as for `WriteOwnerRule`.
    pub own_receiver: Option<String>,
    /// Project protocols, as for `WriteOwnerRule`: a read through a value typed with one that
    /// declares the attribute is allowed.
    pub allowed_protocols: Vec<String>,
    /// Whether `allowed-protocols` covers subprotocols, as for `WriteOwnerRule`.
    pub protocol_subclasses: Option<bool>,
}

/// `[[tool.heki.call-owners]]` contract for `MUT204`: who may call a function or method, or
/// construct a class (the call side of `write-owners` / `read-owners`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct CallOwnerRule {
    pub name: String,
    /// Project functions, methods (`Class.method`, overrides in subclasses included), or classes
    /// (constructing them), as dotted names or globs.
    pub targets: Vec<String>,
    pub allowed_callers: Vec<String>,
    /// When set, only calls by these functions are reported (see `WriteOwnerRule`).
    pub forbidden_callers: Vec<String>,
    /// Methods allowed to call the targets: a method name or a dotted pattern.
    pub exempt_methods: Vec<String>,
    /// The help of every `MUT204` report of this contract.
    pub message: Option<String>,
    /// `"report"`: also report (`MUT207`) calls of a method named like a target on an object
    /// of unknown class; `"ignore"` (default) does not.
    pub unresolved: Option<String>,
    /// `false`: a class target names the class itself (constructing a subclass is no call of
    /// it); `true` (default) covers its subclasses. `Class.method` targets cover the method's
    /// overrides either way.
    pub subclasses: Option<bool>,
    /// `"calls"`: a reference to a target that hands it on to be called later (`[decide]`,
    /// `return decide`, `partial(decide)`, `field(default_factory=Rule)`) counts as a call of
    /// it; `"calls-and-bases"` also a class statement naming a class target among its bases;
    /// `"reads"` also every other use of a target's value (`Keys.of(...)`, `isinstance(x,
    /// Keys)`); `"ignore"` (default) none of them.
    pub references: Option<String>,
    /// `"lexical"` or `"call-graph"` (default), as for `WriteOwnerRule`.
    pub attribution: Option<String>,
    /// `"allow"` or `"report"` (default), as for `WriteOwnerRule`: a call of a method bound to
    /// the receiver of the method it is judged in.
    pub own_receiver: Option<String>,
    /// `"allow"` (default): the targets may call each other, as the operation they make up is
    /// checked where it is called; `"report"`: each target is judged like the one target of a
    /// contract with the same keys, so a call of one target by another is reported.
    pub targets_call_each_other: Option<String>,
    /// Project protocols, as for `WriteOwnerRule`: a call through a value typed with one that
    /// declares the method is allowed.
    pub allowed_protocols: Vec<String>,
    /// Whether `allowed-protocols` covers subprotocols, as for `WriteOwnerRule`.
    pub protocol_subclasses: Option<bool>,
}

/// `[[tool.heki.import-boundaries]]` contract for `IMP201`: which modules the modules of
/// `importers` may import. Module patterns are whole module names or globs; `<stdlib>`,
/// `<project>`, and `<third-party>` name groups.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ImportBoundaryRule {
    pub name: String,
    /// The modules judged; `None` for every module.
    pub importers: Option<Vec<String>>,
    /// The modules of `importers` that are not judged.
    pub exempt_importers: Vec<String>,
    /// The modules the importers may import: every other import is reported.
    pub allowed_imports: Vec<String>,
    /// When set, only imports of these modules are reported (less the allowed ones).
    pub forbidden_imports: Vec<String>,
    /// The modules the importers may import only through a relative import (`from . import
    /// b`): one they import by its full name is reported, whatever the other lists say.
    pub relative_imports: Vec<String>,
    /// `"importer -> imported"` pairs of module patterns whose imports the contract accepts.
    pub ignored_imports: Vec<String>,
    /// `"report"`: imports under `if TYPE_CHECKING:` count too; `"ignore"` (default) does not.
    pub type_checking: Option<String>,
    /// `"statement"` (default): each import statement or call is judged; `"import-time"`: what
    /// loading an importer loads while it loads; `"all"`: what any chain of imports that run
    /// leads to.
    pub reach: Option<String>,
    /// `"report"`: also report (`IMP202`) the imports whose module heki cannot name;
    /// `"ignore"` (default) does not.
    pub unresolved: Option<String>,
    /// The help of every `IMP201` report of this contract.
    pub message: Option<String>,
}

/// `[tool.heki.imports]`: which imports the import rules count, and which distributions
/// provide the modules they import.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ImportsConfig {
    /// The timing classes of the imports `IMP101` follows (`"body"`, `"lazy"`, `"function"`,
    /// `"type-checking"`); `None` for `["body"]`, the imports that run while a module loads.
    pub cycle_edges: Option<Vec<String>>,
    /// The distributions that may provide a module outside the project, by the module (it and
    /// the modules below it): `IMP301` and `IMP302` take them instead of the catalog's and of
    /// the distributions named like the module.
    pub module_distributions: BTreeMap<String, Vec<String>>,
}

/// `[[tool.heki.effect-boundaries]]` contract for `EFF201`: effects that must not be reached,
/// directly or through calls, from the functions of `scopes`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct EffectBoundaryRule {
    pub name: String,
    /// Modules, classes, or functions (dotted patterns) whose functions are checked.
    pub scopes: Vec<String>,
    /// Forbidden effects: `blocking-net`, `blocking-db`, `blocking-fs`, `blocking-subprocess`,
    /// `blocking-sync`, `sleep`, `blocking` (all six), `mutates-params`, `writes-targets`,
    /// `reads-targets`, `calls`, `reads-environment`.
    pub forbid: Vec<String>,
    /// `Class.attr` targets of `writes-targets`.
    pub write_targets: Vec<String>,
    /// `Class.attr` targets of `reads-targets`, or attributes of modules outside the project by
    /// their full name (`sys.platform`).
    pub read_targets: Vec<String>,
    /// Functions, methods, or classes (construction) of `calls`, as for `call-owners` targets,
    /// or functions and classes outside the project by their full name (`os.getenv`).
    pub call_targets: Vec<String>,
    /// `Class.attr` writes the boundary allows: every other reached write of `write-universe`
    /// (or of `write-targets`) is reported.
    pub allow_writes: Vec<String>,
    /// `Class.attr` entries whose writes are reported unless `allow-writes` names them.
    pub write_universe: Vec<String>,
    /// Callees (dotted patterns) whose effects are allowed: paths through them are not
    /// followed.
    pub allowed_callees: Vec<String>,
    /// `"allow"` (default) or `"report"`, as for `write-owners`: whether writes that construct
    /// the object they write count as writes of the boundary.
    pub fresh_objects: Option<String>,
    /// `"calls"`, `"calls-and-bases"`, `"reads"`, or `"ignore"` (default), as for `call-owners`:
    /// whether a reference handing a `call-targets` entry on (or using it otherwise) counts as a
    /// call of it for `calls`.
    pub references: Option<String>,
    /// `"allow"` (default) or `"report"`: whether `reads-environment` also forbids the imports
    /// that probe whether a module is installed (`try: import yaml` / `except ImportError:`).
    pub import_probes: Option<String>,
    /// `"report"`: also report what the boundary cannot judge in its scopes' reach: a call heki
    /// cannot resolve, a function it does not model or whose body only a stub declares, and a
    /// write of an attribute `write-targets` or `write-universe` names on an object of unknown
    /// class; `"ignore"` (default) does not.
    pub unresolved: Option<String>,
    /// The help of every `EFF201` report of this contract.
    pub message: Option<String>,
}

/// `[[tool.heki.readonly-returns]]` contract for `MUT202`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ReadOnlyReturnRule {
    pub name: String,
    pub producers: Vec<String>,
    pub deep: bool,
}

/// `[[tool.heki.invalidation]]` contract for `MUT208`: a write to one of `targets` must be
/// followed, on every path to a normal return, by a call that reaches one of `invalidators`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct InvalidationRule {
    pub name: String,
    /// `Class.attr` targets whose writes must be announced (subclasses included).
    pub targets: Vec<String>,
    /// Functions or methods (dotted patterns) whose call announces the change.
    pub invalidators: Vec<String>,
    /// Functions (dotted patterns) whose writes need no announcement of their own: the
    /// owner's operations, which announce or batch the change themselves.
    pub exempt_writers: Vec<String>,
    /// The help of every `MUT208` report of this contract.
    pub message: Option<String>,
}

/// `[[tool.heki.ecosystem]]` user addition / override of a Layer 2 ecosystem KB entry.
///
/// `effects` holds Execution Effect names exactly as spelled in `kb/ecosystem/*.toml`
/// (snake_case, e.g. `"blocking_net"`); `heki_kb` validates them when merging. An override
/// sets the keys it lists (`None`: left out) on the built-in entry for the same `symbol`, whose
/// other keys stay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct EcosystemOverride {
    pub symbol: String,
    pub effects: Option<Vec<String>>,
    pub async_safe: Option<bool>,
    pub offloads_blocking_arg: Option<u8>,
    pub excluded: Option<bool>,
    pub copy_sanitizer: Option<CopySanitizerSetting>,
    /// The class of the instances the call returns (a factory).
    pub return_type: Option<String>,
    /// `EFF106`: a `with` over an instance of this class, or over the call's result, holds a
    /// synchronous lock or a database transaction.
    pub sync_lock_or_tx: Option<bool>,
    /// `EFF109`: an instance of this class is a lock its holder cannot take again.
    pub non_reentrant_lock: Option<bool>,
    /// `EFF110`: tasks may not be inside this method on one object at once (`"always"`,
    /// `"when_unbound"`, `"unbinds"`, validated by `heki_kb`).
    pub one_at_a_time: Option<String>,
    /// `EFF112`: the call writes an argument, or serializes it into text, in the order of its
    /// elements.
    pub writes_in_order: Option<OrderedOutputSetting>,
    /// The class of what reading the attribute `symbol` names on an instance of a library
    /// class gives (`package.module.Client.chat`); it takes no other key.
    pub attribute_type: Option<String>,
}

/// `writes-in-order`: the argument written (`arg`, a position not counting the receiver, or
/// `param`, a keyword; with neither, every positional argument) and the keyword argument that
/// sorts the keys of the mappings written first (`sorted-by`), sorting by default when
/// `sorted-by-default` is true.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct OrderedOutputSetting {
    pub arg: Option<u8>,
    pub param: Option<String>,
    pub sorted_by: Option<String>,
    pub sorted_by_default: bool,
}

/// `copy-sanitizer`: the depth of the copy the call returns (`"shallow"` / `"deep"`, validated
/// by `heki_kb`), or `true` for a shallow copy and `false` for none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CopySanitizerSetting {
    Flag(bool),
    Depth(String),
}

/// How a report names the contract at `index` (0-based, in configuration order) of the
/// `[[tool.heki.<table>]]` array: by its `name`, or by its position when it has none.
pub fn contract_label(table: &str, index: usize, name: &str) -> String {
    if name.is_empty() {
        format!("the `{table}` contract at position {}", index + 1)
    } else {
        format!("`{name}`")
    }
}

/// Top-level `[tool.heki]` or `heki.toml` configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct HekiConfig {
    pub target_version: Option<String>,
    /// A PEP 440 specifier the running heki must satisfy (`>=0.2.6`); [`schema::check_keys`]
    /// refuses an older heki before it reads any other key.
    pub required_version: Option<String>,
    pub exclude: Option<Vec<String>>,
    pub extend_exclude: Vec<String>,
    /// Source roots ([`source_root`]): project-relative directories whose files are named
    /// relative to them. `None` detects them (`heki_effect::ModuleLayout::detect`).
    pub src: Option<Vec<String>>,
    pub baseline: Option<PathBuf>,
    pub lint: LintConfig,
    pub write_owners: Vec<WriteOwnerRule>,
    pub read_owners: Vec<ReadOwnerRule>,
    pub call_owners: Vec<CallOwnerRule>,
    pub readonly_returns: Vec<ReadOnlyReturnRule>,
    pub invalidation: Vec<InvalidationRule>,
    pub effect_boundaries: Vec<EffectBoundaryRule>,
    pub import_boundaries: Vec<ImportBoundaryRule>,
    /// The `<contract>-defaults` tables as written. The loaders have folded each into the tables
    /// of its contract, so the contracts above hold the effective values. A table a tool adds
    /// to the configuration afterwards takes them through [`with_defaults`].
    pub write_owners_defaults: Option<WriteOwnerRule>,
    pub read_owners_defaults: Option<ReadOwnerRule>,
    pub call_owners_defaults: Option<CallOwnerRule>,
    pub readonly_returns_defaults: Option<ReadOnlyReturnRule>,
    pub invalidation_defaults: Option<InvalidationRule>,
    pub effect_boundaries_defaults: Option<EffectBoundaryRule>,
    pub import_boundaries_defaults: Option<ImportBoundaryRule>,
    pub imports: ImportsConfig,
    pub ecosystem: Vec<EcosystemOverride>,
    /// The version `project.requires-python` of the project's `pyproject.toml` selects
    /// ([`requires_python_lower_bound`]): the `target-version` default (read by the loaders,
    /// not a `[tool.heki]` key).
    #[serde(skip)]
    pub requires_python: Option<PythonVersion>,
    /// `project.name` of the project's `pyproject.toml` (read by the loaders, not a
    /// `[tool.heki]` key): the `<project-name>` source root of the detected layout.
    #[serde(skip)]
    pub project_name: Option<String>,
    /// The workspace tables of the project's `pyproject.toml` and `hatch.toml` (read by the
    /// loaders, not a `[tool.heki]` key): their members are detected source roots.
    #[serde(skip)]
    pub workspaces: Vec<WorkspaceMembers>,
    /// What the project's `pyproject.toml` declares about its distribution ([`Declared`]; read
    /// by the loaders, not a `[tool.heki]` key).
    #[serde(skip)]
    pub declared: Option<Declared>,
}

/// The members one workspace table lists (`[tool.uv.workspace]`, the `workspace` of a Hatch
/// environment, `[tool.pdm.workspace]`): directories given as globs relative to the project
/// root (`packages/*`), less the ones an `exclude` glob matches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceMembers {
    pub members: Vec<String>,
    pub exclude: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read the configuration file `{}`: {cause}", path.display())]
    Io {
        path: PathBuf,
        cause: std::io::Error,
    },
    #[error("the configuration file is not valid TOML: {message}")]
    Syntax {
        path: PathBuf,
        position: Option<(u32, u32)>,
        message: String,
    },
    /// A key its table does not have.
    #[error("unknown key `{key}` in {}", problem::table_name(*layout, table))]
    UnknownKey {
        path: PathBuf,
        layout: ConfigLayout,
        position: Option<(u32, u32)>,
        table: String,
        key: String,
        /// The keys below the key, when it opens a table.
        keys_inside: Vec<String>,
    },
    /// A value of the wrong type, or not one the key takes.
    #[error("{}{message}", key.as_ref().map(|(table, key)| format!("invalid `{key}` in {}: ", problem::table_name(*layout, table))).unwrap_or_default())]
    Value {
        path: PathBuf,
        layout: ConfigLayout,
        position: Option<(u32, u32)>,
        /// The table and key of the value, when the error has a position.
        key: Option<(String, String)>,
        message: String,
    },
    #[error("`[tool.heki]` outside a `pyproject.toml`")]
    ToolTable {
        path: PathBuf,
        position: Option<(u32, u32)>,
    },
    #[error("`{key}` at the top level of a `pyproject.toml`")]
    TopLevelSetting {
        path: PathBuf,
        key: String,
        position: Option<(u32, u32)>,
    },
    /// `required-version` names a heki release this one is not.
    #[error("this is heki {running}, but the configuration requires `{required}`")]
    RequiredVersion {
        path: PathBuf,
        position: Option<(u32, u32)>,
        required: String,
        running: &'static str,
    },
    #[error("invalid `src` entry `{entry}`: {reason}")]
    SourceRoot {
        path: PathBuf,
        entry: String,
        reason: &'static str,
    },
}

impl Explain for ConfigError {
    fn explain(&self) -> Problem {
        let problem = Problem::new(self.to_string());
        let at = |problem: Problem, path: &Path, position: &Option<(u32, u32)>| {
            position.map_or(problem.clone(), |(line, column)| {
                problem.at(path, line, column)
            })
        };
        match self {
            Self::Io { .. } => problem
                .help("pass an existing file to `--config`, or run heki where heki.toml or pyproject.toml is readable")
                .docs(Docs::ConfigFiles),
            Self::Syntax { path, position, .. } => at(problem, path, position)
                .help("heki reads its settings from this file: fix the TOML here, or name another file with `--config`")
                .docs(Docs::ConfigFiles),
            Self::UnknownKey {
                path,
                layout,
                position,
                table,
                key,
                keys_inside,
            } => schema::explain_unknown_key(
                at(problem, path, position),
                *layout,
                table,
                key,
                keys_inside,
            ),
            Self::Value {
                path,
                layout,
                position,
                key,
                ..
            } => {
                let mut problem = at(problem, path, position);
                let help = key.as_ref().and_then(|(t, k)| schema::value_help(*layout, t, k));
                problem.help.extend(help);
                problem.docs(key.as_ref().map_or(Docs::ConfigTables, |(t, k)| Docs::of_key(t, k)))
            }
            Self::RequiredVersion {
                path,
                position,
                required,
                ..
            } => at(problem, path, position)
                .help(format!(
                    "install a heki that satisfies `{required}` (`pip install --upgrade heki`), or change `required-version` in this file"
                ))
                .docs(Docs::RequiredVersion),
            Self::ToolTable { path, position } => at(problem, path, position)
                .help("only a `pyproject.toml` holds the settings under `tool.heki`; this file holds them at the top level: write `[lint]` and `[[write-owners]]`, not `[tool.heki.lint]`")
                .docs(Docs::ConfigTables),
            Self::TopLevelSetting { path, key, position } => at(problem, path, position)
                .help(format!("a `pyproject.toml` holds heki's settings under `[tool.heki]`: move `{key}` into `[tool.heki]` (or its table, `[tool.heki.{key}]`)"))
                .docs(Docs::ConfigTables),
            Self::SourceRoot { path, entry, .. } => problem
                .help("list directories relative to the project root: `src = [\"src\"]`")
                .docs(Docs::SourceRoots)
                .setting(Setting {
                    key: Some("src".to_owned()),
                    value: Some(entry.clone()),
                    ..Setting::default()
                })
                .locate(Some(path)),
        }
    }
}

/// The project-relative POSIX form of a `src` entry (`./lib/` -> `lib`; `.`, the project root
/// itself, -> the empty string), or why it names no directory of the project: it must be
/// relative to the project root, stay inside it, and name directories, not globs.
pub fn source_root(entry: &str) -> Result<String, &'static str> {
    let mut parts: Vec<&str> = Vec::new();
    for c in Path::new(entry).components() {
        match c {
            std::path::Component::Normal(part) => {
                let part = part.to_str().ok_or("the path is not valid UTF-8")?;
                if part.contains(['*', '?', '[', ']', '{', '}']) {
                    return Err("globs are not accepted yet: list the directories");
                }
                parts.push(part);
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err("the directory is outside the project root");
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err("the path must be relative to the project root");
            }
        }
    }
    Ok(parts.join("/"))
}

/// Which tables of a configuration file hold the settings, by the file's name (as Ruff reads
/// `pyproject.toml` and `ruff.toml`): a `pyproject.toml` holds them in `[tool.heki]`, its other
/// tables belong to the packaging standards and other tools; any other file (`heki.toml`,
/// `.heki.toml`, a `--config` file) holds them at its top level and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLayout {
    PyProject,
    TopLevel,
}

impl ConfigLayout {
    pub fn of(path: &Path) -> Self {
        if path.file_name().is_some_and(|n| n == "pyproject.toml") {
            Self::PyProject
        } else {
            Self::TopLevel
        }
    }

    /// The table that holds the settings: `tool.heki` in a `pyproject.toml`, none (the top
    /// level) in any other file.
    pub const fn table_prefix(self) -> Option<&'static str> {
        match self {
            Self::PyProject => Some("tool.heki"),
            Self::TopLevel => None,
        }
    }

    /// The configuration in `text`, the contents of `path`, of the project rooted at `root`. A
    /// setting is never ignored: an unknown key, a `[tool.heki]` table outside a
    /// `pyproject.toml`, and a setting at the top level of a `pyproject.toml` are errors.
    ///
    /// A `pyproject.toml` holds its own project metadata. Any other file takes it from the
    /// project's `pyproject.toml` at `root`, so the same file gives the same configuration
    /// wherever it sits.
    ///
    /// A `<contract>-defaults` table gives the keys its contract's tables leave out: the
    /// contracts of the configuration returned are the expanded tables, and its errors name the
    /// file as written.
    pub fn parse(self, path: &Path, text: &str, root: &Path) -> Result<HekiConfig, ConfigError> {
        let written = self.read(path, text, root)?;
        if !written.declares_defaults() {
            return Ok(written);
        }
        Ok(HekiConfig {
            write_owners_defaults: written.write_owners_defaults,
            read_owners_defaults: written.read_owners_defaults,
            call_owners_defaults: written.call_owners_defaults,
            readonly_returns_defaults: written.readonly_returns_defaults,
            invalidation_defaults: written.invalidation_defaults,
            effect_boundaries_defaults: written.effect_boundaries_defaults,
            import_boundaries_defaults: written.import_boundaries_defaults,
            ..self.read(path, &defaults::expand(self, text), root)?
        })
    }

    /// The configuration of `text` as written: the defaults tables are read, not yet applied.
    fn read(self, path: &Path, text: &str, root: &Path) -> Result<HekiConfig, ConfigError> {
        let doc = toml_edit::Document::parse(text).map_err(|e| ConfigError::Syntax {
            path: path.to_path_buf(),
            position: e.span().map(|s| problem::line_column(text, s.start)),
            message: e.message().trim().replace('\n', "; "),
        })?;
        schema::check_keys(self, path, text, doc.as_table())?;
        let toml_error = |cause: toml::de::Error| {
            let offset = cause.span().map(|s| s.start);
            let key = offset.and_then(|offset| {
                let root = problem::settings_root(doc.as_table(), self)?;
                schema::key_at("", root, offset)
            });
            ConfigError::Value {
                path: path.to_path_buf(),
                layout: self,
                position: offset.map(|o| problem::line_column(text, o)),
                key,
                message: cause.message().to_owned(),
            }
        };
        match self {
            Self::PyProject => {
                let wrapper: PyProjectWrapper = toml::from_str(text).map_err(toml_error)?;
                let workspaces = wrapper.workspaces(path);
                let table: toml::Table = toml::from_str(text).map_err(toml_error)?;
                HekiConfig {
                    requires_python: wrapper.requires_python(),
                    project_name: wrapper.project_name(),
                    workspaces,
                    declared: Declared::of(&table),
                    ..wrapper.tool.heki.unwrap_or_default()
                }
                .with_source_roots(path)
            }
            Self::TopLevel => {
                let cfg: HekiConfig = toml::from_str(text).map_err(toml_error)?;
                let pyproject = root.join("pyproject.toml");
                let project_metadata = project_pyproject(&pyproject);
                HekiConfig {
                    requires_python: (project_metadata.as_ref())
                        .and_then(|(wrapper, _)| wrapper.requires_python()),
                    project_name: project_metadata
                        .as_ref()
                        .and_then(|(wrapper, _)| wrapper.project_name()),
                    declared: project_metadata
                        .as_ref()
                        .and_then(|(_, table)| Declared::of(table)),
                    workspaces: (project_metadata
                        .map(|(wrapper, _)| wrapper)
                        .unwrap_or_default())
                    .workspaces(&pyproject),
                    ..cfg
                }
                .with_source_roots(path)
            }
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct PyProjectWrapper {
    #[serde(default)]
    tool: ToolSection,
    #[serde(default)]
    project: ProjectSection,
}

/// Kept as raw values: a malformed entry only loses what heki infers from it.
#[derive(Debug, Default, Deserialize)]
struct ProjectSection {
    name: Option<toml::Value>,
    #[serde(rename = "requires-python")]
    requires_python: Option<toml::Value>,
}

impl PyProjectWrapper {
    fn requires_python(&self) -> Option<PythonVersion> {
        let spec = self.project.requires_python.as_ref()?.as_str()?;
        requires_python_lower_bound(spec)
    }

    fn project_name(&self) -> Option<String> {
        self.project.name.as_ref()?.as_str().map(str::to_owned)
    }

    /// The workspace tables of this `pyproject.toml` (at `path`) and of the `hatch.toml` next
    /// to it: uv's, every Hatch environment's (`[tool.hatch.envs.<env>]`, `[envs.<env>]` in
    /// `hatch.toml`), and PDM's. A malformed table only loses its members.
    fn workspaces(&self, path: &Path) -> Vec<WorkspaceMembers> {
        let hatch_toml = path
            .parent()
            .and_then(|dir| std::fs::read_to_string(dir.join("hatch.toml")).ok())
            .and_then(|text| toml::from_str::<toml::Value>(&text).ok());
        let hatch_envs = [self.tool.hatch.as_ref(), hatch_toml.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(|hatch| hatch.get("envs")?.as_table())
            .flat_map(|envs| envs.values())
            .filter_map(|env| env.get("workspace"));
        let uv = self.tool.uv.as_ref().and_then(|uv| uv.get("workspace"));
        let pdm = self.tool.pdm.as_ref().and_then(|pdm| pdm.get("workspace"));
        uv.into_iter()
            .chain(hatch_envs)
            .chain(pdm)
            .map(WorkspaceMembers::of)
            .filter(|w| !w.members.is_empty())
            .collect()
    }
}

impl WorkspaceMembers {
    /// The `members` and `exclude` of a workspace table. A member is a path string, or a Hatch
    /// table naming it by `path` (`{path = "packages/core", features = ["dev"]}`).
    fn of(table: &toml::Value) -> Self {
        let paths = |key: &str| -> Vec<String> {
            table
                .get(key)
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.as_str().or_else(|| entry.get("path")?.as_str()))
                .map(str::to_owned)
                .collect()
        };
        Self {
            members: paths("members"),
            exclude: paths("exclude"),
        }
    }
}

/// The project's `pyproject.toml`, read as heki reads its own and as a table; any problem with that file only loses what heki infers from it.
fn project_pyproject(pyproject: &Path) -> Option<(PyProjectWrapper, toml::Table)> {
    let text = std::fs::read_to_string(pyproject).ok()?;
    Some((toml::from_str(&text).ok()?, toml::from_str(&text).ok()?))
}

/// The Python version a PEP 440 `requires-python` specifier selects: the oldest supported
/// minor version (3.11–3.15) one of whose releases satisfies every clause (exclusions and
/// patch-level bounds included: `>=3.11, !=3.11.*` selects 3.12). A specifier admitting only
/// older or newer releases selects the nearest end (`>=3.9, <3.11` → 3.11, `>=3.16` → 3.15).
/// `None` (the default version) when it cannot be parsed, admits nothing, or sets no lower
/// bound and admits the default 3.12 (`<4`).
pub fn requires_python_lower_bound(spec: &str) -> Option<PythonVersion> {
    let clauses = spec
        .split(',')
        .map(VersionClause::parse)
        .collect::<Option<Vec<_>>>()?;
    let admits_minor = |major: u8, minor: u8| {
        // A clause changes its verdict only at the patch levels it names: their neighbours
        // stand for every release in between.
        let mut patches = vec![0u32];
        for c in &clauses {
            if c.release.first() == Some(&u32::from(major))
                && c.release.get(1) == Some(&u32::from(minor))
            {
                let p = c.release.get(2).copied().unwrap_or(0);
                patches.extend([p.saturating_sub(1), p, p.saturating_add(1)]);
            }
        }
        let release = |p: u32| [u32::from(major), u32::from(minor), p];
        patches
            .iter()
            .any(|&p| clauses.iter().all(|c| c.admits(&release(p))))
    };
    let supported = PythonVersion::SUPPORTED;
    let has_lower_bound = clauses.iter().any(|c| {
        matches!(
            c.op,
            VersionOp::Ge | VersionOp::Gt | VersionOp::Eq | VersionOp::Compatible
        )
    });
    if !has_lower_bound && admits_minor(3, 12) {
        return None;
    }
    if let Some(v) = supported
        .into_iter()
        .find(|v| admits_minor(v.major, v.minor))
    {
        return Some(v);
    }
    let oldest = (2..=4u8)
        .flat_map(|major| (0..=40u8).map(move |minor| (major, minor)))
        .find(|&(major, minor)| admits_minor(major, minor))?;
    Some(if oldest < (3, 11) {
        PythonVersion::PY311
    } else {
        PythonVersion::PY315
    })
}

/// The version of this heki (`workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether `version` satisfies the PEP 440 specifier `spec` (`>=0.2.6, <0.3`), compared on release
/// segments; `None` when `spec` is not a specifier. Every clause is an operator and a release of
/// digits and dots (`==1.2.*` allowed): a pre-, post-, or development-release suffix or trailing
/// text is refused, since the comparison would silently drop it.
pub fn version_satisfies(spec: &str, version: &str) -> Option<bool> {
    let is_release_only = |clause: &str| {
        let release = clause
            .trim()
            .trim_start_matches(['=', '!', '~', '<', '>'])
            .trim();
        let release = release.strip_suffix(".*").unwrap_or(release);
        release
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    if !spec.split(',').all(is_release_only) {
        return None;
    }
    let clauses = spec
        .split(',')
        .map(VersionClause::parse)
        .collect::<Option<Vec<_>>>()?;
    let release = VersionClause::parse(&format!("=={version}"))?.release;
    Some(clauses.iter().all(|c| c.admits(&release)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VersionOp {
    /// `===`: arbitrary equality, compared here as `==`.
    Eq,
    Ne,
    Ge,
    Gt,
    Le,
    Lt,
    /// `~=`.
    Compatible,
}

/// One clause of a version specifier, compared on release segments (pre-, post-, and
/// development-release suffixes do not move a release across a minor version).
#[derive(Debug)]
struct VersionClause {
    op: VersionOp,
    release: Vec<u32>,
    /// `== 3.11.*` / `!= 3.11.*`: a prefix match.
    wildcard: bool,
}

impl VersionClause {
    fn parse(clause: &str) -> Option<Self> {
        let clause = clause.trim();
        let (op, rest) = [
            ("===", VersionOp::Eq),
            ("~=", VersionOp::Compatible),
            ("==", VersionOp::Eq),
            ("!=", VersionOp::Ne),
            (">=", VersionOp::Ge),
            ("<=", VersionOp::Le),
            (">", VersionOp::Gt),
            ("<", VersionOp::Lt),
        ]
        .into_iter()
        .find_map(|(text, op)| clause.strip_prefix(text).map(|rest| (op, rest.trim())))?;
        let (version, wildcard) = match rest.strip_suffix(".*") {
            Some(v) => (v, true),
            None => (rest, false),
        };
        if wildcard && !matches!(op, VersionOp::Eq | VersionOp::Ne) {
            return None;
        }
        let digits = version
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .map_or(version, |end| &version[..end]);
        let release = digits
            .trim_end_matches('.')
            .split('.')
            .map(|part| part.parse().ok())
            .collect::<Option<Vec<u32>>>()?;
        let compatible_needs_two = op == VersionOp::Compatible && release.len() < 2;
        (!compatible_needs_two).then_some(Self {
            op,
            release,
            wildcard,
        })
    }

    fn admits(&self, version: &[u32]) -> bool {
        let segment = |r: &[u32], i: usize| r.get(i).copied().unwrap_or(0);
        let compare = |r: &[u32]| {
            (0..version.len().max(r.len()))
                .map(|i| segment(version, i).cmp(&segment(r, i)))
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        };
        let prefix = |r: &[u32]| (0..r.len()).all(|i| segment(version, i) == r[i]);
        let equal = if self.wildcard {
            prefix(&self.release)
        } else {
            compare(&self.release).is_eq()
        };
        match self.op {
            VersionOp::Eq => equal,
            VersionOp::Ne => !equal,
            VersionOp::Ge => compare(&self.release).is_ge(),
            VersionOp::Gt => compare(&self.release).is_gt(),
            VersionOp::Le => compare(&self.release).is_le(),
            VersionOp::Lt => compare(&self.release).is_lt(),
            VersionOp::Compatible => {
                compare(&self.release).is_ge() && prefix(&self.release[..self.release.len() - 1])
            }
        }
    }
}

/// The tables of other tools are kept as raw values, like [`ProjectSection`].
#[derive(Debug, Default, Deserialize)]
struct ToolSection {
    heki: Option<HekiConfig>,
    uv: Option<toml::Value>,
    hatch: Option<toml::Value>,
    pdm: Option<toml::Value>,
}

impl HekiConfig {
    /// The top-level keys of the configuration (`[tool.heki]` of a `pyproject.toml`).
    pub const KEYS: &'static [&'static str] = &[
        "target-version",
        "required-version",
        "exclude",
        "extend-exclude",
        "src",
        "baseline",
        "lint",
        "write-owners",
        "read-owners",
        "call-owners",
        "readonly-returns",
        "invalidation",
        "effect-boundaries",
        "import-boundaries",
        "write-owners-defaults",
        "read-owners-defaults",
        "call-owners-defaults",
        "readonly-returns-defaults",
        "invalidation-defaults",
        "effect-boundaries-defaults",
        "import-boundaries-defaults",
        "imports",
        "ecosystem",
    ];

    /// The keys of the contracts, each an array of tables (docs/configuration.md, "Contracts").
    pub const CONTRACT_KEYS: &'static [&'static str] = &[
        "write-owners",
        "read-owners",
        "call-owners",
        "effect-boundaries",
        "readonly-returns",
        "invalidation",
        "import-boundaries",
    ];

    /// Whether a `<contract>-defaults` table is written.
    fn declares_defaults(&self) -> bool {
        self.write_owners_defaults.is_some()
            || self.read_owners_defaults.is_some()
            || self.call_owners_defaults.is_some()
            || self.readonly_returns_defaults.is_some()
            || self.invalidation_defaults.is_some()
            || self.effect_boundaries_defaults.is_some()
            || self.import_boundaries_defaults.is_some()
    }

    /// Whether a contract is declared: a table of one of [`Self::CONTRACT_KEYS`].
    pub fn declares_contracts(&self) -> bool {
        !(self.write_owners.is_empty()
            && self.read_owners.is_empty()
            && self.call_owners.is_empty()
            && self.effect_boundaries.is_empty()
            && self.readonly_returns.is_empty()
            && self.invalidation.is_empty()
            && self.import_boundaries.is_empty())
    }

    /// Load a configuration file ([`ConfigLayout`]) of the project rooted at `root`: the
    /// `[tool.heki]` table of a `pyproject.toml` (empty config when absent), the top-level keys of
    /// any other file.
    pub fn load_from_file(path: &Path, root: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|cause| ConfigError::Io {
            path: path.to_path_buf(),
            cause,
        })?;
        ConfigLayout::of(path).parse(path, &text, root)
    }

    /// The file a project rooted at `root` reads its configuration from: the first of
    /// `heki.toml`, `.heki.toml`, and `pyproject.toml` there.
    pub fn file_in(root: &Path) -> Option<PathBuf> {
        ["heki.toml", ".heki.toml", "pyproject.toml"]
            .into_iter()
            .map(|candidate| root.join(candidate))
            .find(|p| p.is_file())
    }

    /// The configuration of `path` with its `src` entries in their project-relative form
    /// ([`source_root`]).
    fn with_source_roots(mut self, path: &Path) -> Result<Self, ConfigError> {
        if let Some(entries) = &mut self.src {
            for entry in entries {
                *entry = source_root(entry).map_err(|reason| ConfigError::SourceRoot {
                    path: path.to_path_buf(),
                    entry: entry.clone(),
                    reason,
                })?;
            }
        }
        Ok(self)
    }

    /// Resolve `RuntimeProfile` from `target-version`, else the version
    /// `project.requires-python` selects, else Python 3.12.
    pub fn runtime_profile(&self) -> RuntimeProfile {
        let ver = self
            .target_version
            .as_deref()
            .and_then(PythonVersion::parse_tag)
            .or(self.requires_python)
            .unwrap_or(PythonVersion::PY312);
        RuntimeProfile::from(ver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ruff_and_tlinter_compatible_config() {
        let raw = r#"
[tool.heki]
target-version = "py315"
extend-exclude = ["generated"]
baseline = "heki-baseline.toml"

[tool.heki.lint]
select = ["EFF", "DATA", "MUT", "OPT"]
extend-select = ["EFF105"]
ignore = ["MUT101"]

[tool.heki.lint.severity]
MUT101 = "warning"

[tool.heki.lint.per-file-ignores]
"tests/**" = ["EFF101"]

[[tool.heki.write-owners]]
name = "generation-store"
targets = ["pkg.model.DataModel.fields"]
allowed-writers = ["pkg.parser.generation.*"]
forbidden-writers = ["pkg.render.*"]
exempt-methods = ["__deepcopy__"]
message = "use Store.set_fields() instead"
fresh-objects = "report"
unresolved = "report"
attribution = "lexical"
unlisted-dict-keys = "report"

[[tool.heki.read-owners]]
name = "reference-children"
targets = ["Reference.children"]
allowed-readers = ["*GenerationIndex*"]
forbidden-readers = ["pkg.parser.*"]
exempt-methods = ["__deepcopy__"]
message = "read children through GenerationIndex instead"
unresolved = "report"
attribution = "call-graph"
own-receiver = "allow"
allowed-protocols = ["*Host"]
protocol-subclasses = false

[[tool.heki.call-owners]]
name = "reference-mutators"
targets = ["DataType.replace_reference", "FinalFacts"]
allowed-callers = ["pkg.parser.generation.*"]
forbidden-callers = ["pkg.parser.*"]
exempt-methods = ["__deepcopy__"]
message = "use a GenerationStore operation instead"
unresolved = "ignore"
subclasses = false
references = "calls-and-bases"
attribution = "lexical"
own-receiver = "report"
targets-call-each-other = "report"

[[tool.heki.readonly-returns]]
name = "shared-schema"
producers = ["pkg.parser.SchemaGraphBuilder.schema"]
deep = true

[[tool.heki.invalidation]]
name = "graph-index"
targets = ["DataType.data_types"]
invalidators = ["GenerationStore.note_changed_models"]
exempt-writers = ["pkg.parser.generation.*"]
message = "announce the change to the store"

[[tool.heki.effect-boundaries]]
name = "pure-render"
scopes = ["*.render"]
forbid = ["writes-targets", "blocking", "reads-targets", "calls"]
write-targets = ["DataModel.*"]
read-targets = ["DataType.is_optional"]
call-targets = ["FinalFacts"]
allow-writes = ["DataModel.base_classes"]
write-universe = ["DataModel.*"]
allowed-callees = ["pkg.logging.*"]
message = "render must not mutate model state"
fresh-objects = "allow"
references = "calls"
import-probes = "report"
"#;
        let wrapper: PyProjectWrapper = toml::from_str(raw).unwrap();
        let cfg = wrapper.tool.heki.unwrap();
        assert!(cfg.runtime_profile().pep810_lazy_imports);
        assert_eq!(cfg.lint.severity.get("MUT101"), Some(&Severity::Warning));
        assert_eq!(cfg.write_owners.len(), 1);
        assert_eq!(cfg.write_owners[0].exempt_methods, ["__deepcopy__"]);
        assert_eq!(cfg.write_owners[0].forbidden_writers, ["pkg.render.*"]);
        assert_eq!(cfg.write_owners[0].fresh_objects.as_deref(), Some("report"));
        assert_eq!(cfg.write_owners[0].unresolved.as_deref(), Some("report"));
        assert_eq!(cfg.write_owners[0].attribution.as_deref(), Some("lexical"));
        assert_eq!(
            cfg.write_owners[0].unlisted_dict_keys.as_deref(),
            Some("report")
        );
        assert_eq!(
            cfg.write_owners[0].message.as_deref(),
            Some("use Store.set_fields() instead")
        );
        assert_eq!(
            cfg.read_owners[0],
            ReadOwnerRule {
                name: "reference-children".into(),
                targets: vec!["Reference.children".into()],
                allowed_readers: vec!["*GenerationIndex*".into()],
                forbidden_readers: vec!["pkg.parser.*".into()],
                exempt_methods: vec!["__deepcopy__".into()],
                message: Some("read children through GenerationIndex instead".into()),
                unresolved: Some("report".into()),
                attribution: Some("call-graph".into()),
                own_receiver: Some("allow".into()),
                allowed_protocols: vec!["*Host".into()],
                protocol_subclasses: Some(false),
            }
        );
        assert_eq!(
            cfg.call_owners[0],
            CallOwnerRule {
                name: "reference-mutators".into(),
                targets: vec!["DataType.replace_reference".into(), "FinalFacts".into()],
                allowed_callers: vec!["pkg.parser.generation.*".into()],
                forbidden_callers: vec!["pkg.parser.*".into()],
                exempt_methods: vec!["__deepcopy__".into()],
                message: Some("use a GenerationStore operation instead".into()),
                unresolved: Some("ignore".into()),
                subclasses: Some(false),
                references: Some("calls-and-bases".into()),
                attribution: Some("lexical".into()),
                own_receiver: Some("report".into()),
                targets_call_each_other: Some("report".into()),
                allowed_protocols: Vec::new(),
                protocol_subclasses: None,
            }
        );
        assert_eq!(
            cfg.effect_boundaries[0],
            EffectBoundaryRule {
                name: "pure-render".into(),
                scopes: vec!["*.render".into()],
                forbid: vec![
                    "writes-targets".into(),
                    "blocking".into(),
                    "reads-targets".into(),
                    "calls".into()
                ],
                write_targets: vec!["DataModel.*".into()],
                read_targets: vec!["DataType.is_optional".into()],
                call_targets: vec!["FinalFacts".into()],
                allow_writes: vec!["DataModel.base_classes".into()],
                write_universe: vec!["DataModel.*".into()],
                allowed_callees: vec!["pkg.logging.*".into()],
                message: Some("render must not mutate model state".into()),
                fresh_objects: Some("allow".into()),
                references: Some("calls".into()),
                import_probes: Some("report".into()),
                unresolved: None,
            }
        );
        assert!(cfg.readonly_returns[0].deep);
        assert_eq!(
            cfg.invalidation[0],
            InvalidationRule {
                name: "graph-index".into(),
                targets: vec!["DataType.data_types".into()],
                invalidators: vec!["GenerationStore.note_changed_models".into()],
                exempt_writers: vec!["pkg.parser.generation.*".into()],
                message: Some("announce the change to the store".into()),
            }
        );
        assert!(cfg.ecosystem.is_empty());
        assert_eq!(cfg.lint.relax_async_in_tests, None);
    }

    #[test]
    fn requires_python_selects_the_oldest_admitted_supported_version() {
        let v = |minor| Some(PythonVersion { major: 3, minor });
        for (spec, expected) in [
            (">=3.11", v(11)),
            (">= 3.11.2, <4", v(11)),
            (">3.11", v(11)),
            (">3.11.20", v(11)),
            ("==3.11.*", v(11)),
            ("==3.11.5", v(11)),
            ("===3.13.1", v(13)),
            (">=3.8", v(11)),
            (">=3", v(11)),
            ("~=3.12", v(12)),
            ("~=3.12.4", v(12)),
            (">=3.11, !=3.11.*", v(12)),
            (">=3.11,!=3.11.*,!=3.12.*", v(13)),
            (">=3.12.0rc1", v(12)),
            ("!=3.12.*", v(11)),
            ("<3.12", v(11)),
            ("<=3.11.0", v(11)),
            (">=3.9, <3.11", v(11)),
            ("<3.11", v(11)),
            (">=3.16", v(15)),
            (">=4", v(15)),
            // No lower bound and the default admitted: the default.
            ("<3.14", None),
            ("<4", None),
            ("!=3.11.*", None),
            // Nothing admitted, or not a specifier.
            (">=3.12, <3.12", None),
            (">=3.11.4, <3.11.2", None),
            (">=three", None),
            (">=3.*", None),
            ("~=3", None),
            ("3.11", None),
        ] {
            assert_eq!(requires_python_lower_bound(spec), expected, "{spec:?}");
        }
    }

    #[test]
    fn load_from_file_reads_pyproject_and_heki_toml() {
        let dir = std::env::temp_dir().join(format!("heki_env_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("pyproject.toml");
        std::fs::write(&py, "[tool.heki.lint]\nrelax-async-in-tests = false\n").unwrap();
        let cfg = HekiConfig::load_from_file(&py, &dir).unwrap();
        assert_eq!(cfg.lint.relax_async_in_tests, Some(false));
        let heki = dir.join("custom.toml");
        std::fs::write(&heki, "target-version = \"py313\"\n").unwrap();
        assert_eq!(
            HekiConfig::load_from_file(&heki, &dir)
                .unwrap()
                .target_version
                .as_deref(),
            Some("py313")
        );
        std::fs::write(&heki, "target-version = [\n").unwrap();
        assert!(matches!(
            HekiConfig::load_from_file(&heki, &dir),
            Err(ConfigError::Syntax { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_ecosystem_overrides_kebab_case() {
        let raw = r#"
[tool.heki]
target-version = "py312"

[[tool.heki.ecosystem]]
symbol = "mypkg.http.fetch"
effects = ["blocking_net", "unpooled_client_alloc"]

[[tool.heki.ecosystem]]
symbol = "mypkg.workers.in_thread"
async-safe = true
offloads-blocking-arg = 0

[[tool.heki.ecosystem]]
symbol = "mypkg.log.trace"
excluded = true

[[tool.heki.ecosystem]]
symbol = "mypkg.model.Model.clone"
copy-sanitizer = true

[[tool.heki.ecosystem]]
symbol = "mypkg.model.Model.detach"
copy-sanitizer = "deep"

[[tool.heki.ecosystem]]
symbol = "mypkg.db.open"
return-type = "sqlite3.Connection"

[[tool.heki.ecosystem]]
symbol = "mypkg.db.Store.transaction"
effects = []
sync-lock-or-tx = true

[[tool.heki.ecosystem]]
symbol = "mypkg.sync.Mutex"
non-reentrant-lock = true

[[tool.heki.ecosystem]]
symbol = "mypkg.wire.Channel.recv"
one-at-a-time = "always"

[[tool.heki.ecosystem]]
symbol = "mypkg.out.dump"
writes-in-order = { arg = 0, param = "data", sorted-by = "sort_keys" }
"#;
        let wrapper: PyProjectWrapper = toml::from_str(raw).unwrap();
        let cfg = wrapper.tool.heki.unwrap();
        assert_eq!(
            cfg.ecosystem,
            vec![
                EcosystemOverride {
                    symbol: "mypkg.http.fetch".into(),
                    effects: Some(vec!["blocking_net".into(), "unpooled_client_alloc".into()]),
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.workers.in_thread".into(),
                    async_safe: Some(true),
                    offloads_blocking_arg: Some(0),
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.log.trace".into(),
                    excluded: Some(true),
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.model.Model.clone".into(),
                    copy_sanitizer: Some(CopySanitizerSetting::Flag(true)),
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.model.Model.detach".into(),
                    copy_sanitizer: Some(CopySanitizerSetting::Depth("deep".into())),
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.db.open".into(),
                    return_type: Some("sqlite3.Connection".into()),
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.db.Store.transaction".into(),
                    effects: Some(Vec::new()),
                    sync_lock_or_tx: Some(true),
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.sync.Mutex".into(),
                    non_reentrant_lock: Some(true),
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.wire.Channel.recv".into(),
                    one_at_a_time: Some("always".into()),
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.out.dump".into(),
                    writes_in_order: Some(OrderedOutputSetting {
                        arg: Some(0),
                        param: Some("data".into()),
                        sorted_by: Some("sort_keys".into()),
                        sorted_by_default: false,
                    }),
                    ..EcosystemOverride::default()
                },
            ]
        );
    }

    /// `HekiConfig::KEYS` names every setting (the literal lists every field).
    #[test]
    fn keys_name_every_setting() {
        let all = HekiConfig {
            target_version: Some(String::new()),
            required_version: Some(String::new()),
            exclude: Some(Vec::new()),
            extend_exclude: Vec::new(),
            src: Some(Vec::new()),
            baseline: Some(PathBuf::new()),
            lint: LintConfig::default(),
            write_owners: Vec::new(),
            read_owners: Vec::new(),
            call_owners: Vec::new(),
            readonly_returns: Vec::new(),
            invalidation: Vec::new(),
            effect_boundaries: Vec::new(),
            import_boundaries: Vec::new(),
            write_owners_defaults: Some(WriteOwnerRule::default()),
            read_owners_defaults: Some(ReadOwnerRule::default()),
            call_owners_defaults: Some(CallOwnerRule::default()),
            readonly_returns_defaults: Some(ReadOnlyReturnRule::default()),
            invalidation_defaults: Some(InvalidationRule::default()),
            effect_boundaries_defaults: Some(EffectBoundaryRule::default()),
            import_boundaries_defaults: Some(ImportBoundaryRule::default()),
            imports: ImportsConfig::default(),
            ecosystem: Vec::new(),
            requires_python: None,
            project_name: None,
            workspaces: Vec::new(),
            declared: None,
        };
        let table = toml::Table::try_from(&all).unwrap();
        let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
        let mut expected = HekiConfig::KEYS.to_vec();
        keys.sort_unstable();
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    #[test]
    fn parses_ecosystem_overrides_from_heki_toml() {
        let raw = r#"
[[ecosystem]]
symbol = "mypkg.db.run_query"
effects = ["blocking_db", "db_query"]
"#;
        let cfg: HekiConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.ecosystem.len(), 1);
        assert_eq!(cfg.ecosystem[0].symbol, "mypkg.db.run_query");
        assert_eq!(
            cfg.ecosystem[0].effects,
            Some(vec!["blocking_db".into(), "db_query".into()])
        );
        assert_eq!(cfg.ecosystem[0].async_safe, None);
        assert_eq!(cfg.ecosystem[0].offloads_blocking_arg, None);
    }
}
