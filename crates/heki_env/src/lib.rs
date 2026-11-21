//! Environment resolution, `RuntimeProfile`, and Ruff/`t-linter`-compatible `heki` configuration.

mod stdlib;

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
}

impl From<PythonVersion> for RuntimeProfile {
    fn from(v: PythonVersion) -> Self {
        Self {
            python_version: v,
            pep709_inlined_comprehensions: v >= PythonVersion::PY312,
            pep649_deferred_annotations: v >= PythonVersion::PY314,
            pep810_lazy_imports: v >= PythonVersion::PY315,
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
    /// it; `"ignore"` (default) does not.
    pub references: Option<String>,
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
    /// `"calls"` or `"ignore"` (default), as for `call-owners`: whether a reference handing a
    /// `call-targets` entry on counts as a call of it for `calls`.
    pub references: Option<String>,
    /// `"allow"` (default) or `"report"`: whether `reads-environment` also forbids the imports
    /// that probe whether a module is installed (`try: import yaml` / `except ImportError:`).
    pub import_probes: Option<String>,
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

/// `[[tool.heki.invalidation]]` contract for `MUT109`: a write to one of `targets` must be
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
    /// The help of every `MUT109` report of this contract.
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
}

/// `copy-sanitizer`: the depth of the copy the call returns (`"shallow"` / `"deep"`, validated
/// by `heki_kb`), or `true` for a shallow copy and `false` for none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CopySanitizerSetting {
    Flag(bool),
    Depth(String),
}

/// Top-level `[tool.heki]` or `heki.toml` configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct HekiConfig {
    pub target_version: Option<String>,
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
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {cause}")]
    Io {
        path: PathBuf,
        cause: std::io::Error,
    },
    /// Invalid TOML, an unknown key, or a value of the wrong type.
    #[error("failed to parse TOML in {path}: {cause}")]
    Toml {
        path: PathBuf,
        cause: toml::de::Error,
    },
    #[error(
        "`[tool.heki]` in {path}: only a `pyproject.toml` holds the settings under `tool.heki`; this file holds them at the top level (`[lint]`, `[[ecosystem]]`)"
    )]
    ToolTable { path: PathBuf },
    #[error(
        "`{key}` at the top level of {path}: a `pyproject.toml` holds the settings under `[tool.heki]`"
    )]
    TopLevelSetting { path: PathBuf, key: String },
    #[error("`src` entry `{entry}` in {path}: {reason}")]
    SourceRoot {
        path: PathBuf,
        entry: String,
        reason: &'static str,
    },
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
enum ConfigLayout {
    PyProject,
    TopLevel,
}

impl ConfigLayout {
    fn of(path: &Path) -> Self {
        if path.file_name().is_some_and(|n| n == "pyproject.toml") {
            Self::PyProject
        } else {
            Self::TopLevel
        }
    }

    /// The configuration in `text`, the contents of `path`. A setting is never ignored: an
    /// unknown key, a `[tool.heki]` table outside a `pyproject.toml`, and a setting at the top
    /// level of a `pyproject.toml` are errors.
    fn parse(self, path: &Path, text: &str) -> Result<HekiConfig, ConfigError> {
        let toml_error = |cause| ConfigError::Toml {
            path: path.to_path_buf(),
            cause,
        };
        let table: toml::Table = toml::from_str(text).map_err(toml_error)?;
        match self {
            Self::PyProject => {
                if let Some(key) = table
                    .keys()
                    .find(|k| HekiConfig::KEYS.contains(&k.as_str()))
                {
                    return Err(ConfigError::TopLevelSetting {
                        path: path.to_path_buf(),
                        key: key.clone(),
                    });
                }
                let wrapper: PyProjectWrapper = toml::from_str(text).map_err(toml_error)?;
                HekiConfig {
                    requires_python: wrapper.requires_python(),
                    project_name: wrapper.project_name(),
                    ..wrapper.tool.heki.unwrap_or_default()
                }
                .with_source_roots(path)
            }
            Self::TopLevel => {
                if table.contains_key("tool") {
                    return Err(ConfigError::ToolTable {
                        path: path.to_path_buf(),
                    });
                }
                let cfg: HekiConfig = toml::from_str(text).map_err(toml_error)?;
                let dir = path.parent().unwrap_or(Path::new(""));
                let sibling = sibling_pyproject(&dir.join("pyproject.toml"));
                HekiConfig {
                    requires_python: sibling.as_ref().and_then(PyProjectWrapper::requires_python),
                    project_name: sibling.as_ref().and_then(PyProjectWrapper::project_name),
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
}

/// The `pyproject.toml` next to a `heki.toml`-style file; any problem with that file only loses
/// what heki infers from it.
fn sibling_pyproject(pyproject: &Path) -> Option<PyProjectWrapper> {
    let text = std::fs::read_to_string(pyproject).ok()?;
    toml::from_str(&text).ok()
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
    let supported = [
        PythonVersion::PY311,
        PythonVersion::PY312,
        PythonVersion::PY313,
        PythonVersion::PY314,
        PythonVersion::PY315,
    ];
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

#[derive(Debug, Default, Deserialize)]
struct ToolSection {
    heki: Option<HekiConfig>,
}

impl HekiConfig {
    /// The top-level keys of the configuration (`[tool.heki]` of a `pyproject.toml`).
    pub const KEYS: &'static [&'static str] = &[
        "target-version",
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
        "ecosystem",
    ];

    /// Load an explicit configuration file ([`ConfigLayout`]): the `[tool.heki]` table of a
    /// `pyproject.toml` (empty config when absent), the top-level keys of any other file.
    pub fn load_from_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|cause| ConfigError::Io {
            path: path.to_path_buf(),
            cause,
        })?;
        ConfigLayout::of(path).parse(path, &text)
    }

    /// Load configuration from either `heki.toml` / `.heki.toml` or `pyproject.toml` `[tool.heki]`.
    pub fn load_from_dir(root: &Path) -> Result<Self, ConfigError> {
        for candidate in ["heki.toml", ".heki.toml", "pyproject.toml"] {
            let p = root.join(candidate);
            if p.is_file() {
                return Self::load_from_file(&p);
            }
        }
        Ok(Self::default())
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

[[tool.heki.read-owners]]
name = "reference-children"
targets = ["Reference.children"]
allowed-readers = ["*GenerationIndex*"]
forbidden-readers = ["pkg.parser.*"]
exempt-methods = ["__deepcopy__"]
message = "read children through GenerationIndex instead"
unresolved = "report"

[[tool.heki.call-owners]]
name = "reference-mutators"
targets = ["DataType.replace_reference", "FinalFacts"]
allowed-callers = ["pkg.parser.generation.*"]
forbidden-callers = ["pkg.parser.*"]
exempt-methods = ["__deepcopy__"]
message = "use a GenerationStore operation instead"
unresolved = "ignore"
subclasses = false
references = "calls"

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
                references: Some("calls".into()),
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
        let cfg = HekiConfig::load_from_file(&py).unwrap();
        assert_eq!(cfg.lint.relax_async_in_tests, Some(false));
        let heki = dir.join("custom.toml");
        std::fs::write(&heki, "target-version = \"py313\"\n").unwrap();
        assert_eq!(
            HekiConfig::load_from_file(&heki)
                .unwrap()
                .target_version
                .as_deref(),
            Some("py313")
        );
        std::fs::write(&heki, "target-version = [\n").unwrap();
        assert!(matches!(
            HekiConfig::load_from_file(&heki),
            Err(ConfigError::Toml { .. })
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
            ]
        );
    }

    /// `HekiConfig::KEYS` names every setting (the literal lists every field).
    #[test]
    fn keys_name_every_setting() {
        let all = HekiConfig {
            target_version: Some(String::new()),
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
            ecosystem: Vec::new(),
            requires_python: None,
            project_name: None,
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
