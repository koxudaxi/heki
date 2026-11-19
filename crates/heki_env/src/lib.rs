//! Environment resolution, `RuntimeProfile`, and Ruff/`t-linter`-compatible `heki` configuration.

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
#[serde(default, rename_all = "kebab-case")]
pub struct LintConfig {
    pub select: Vec<String>,
    pub extend_select: Vec<String>,
    pub ignore: Vec<String>,
    pub severity: BTreeMap<String, Severity>,
    pub per_file_ignores: BTreeMap<String, Vec<String>>,
    /// `EFF101` / `EFF105` context relaxation for test code (`test_*` functions and files under
    /// `tests/`). Defaults to `true`; set `false` to check test files (e.g. rule fixtures).
    pub relax_async_in_tests: Option<bool>,
}

/// `[[tool.heki.write-owners]]` contract for `MUT201`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct WriteOwnerRule {
    pub name: String,
    pub targets: Vec<String>,
    pub allowed_writers: Vec<String>,
    pub forbid_cached_property_on: Vec<String>,
}

/// `[[tool.heki.readonly-returns]]` contract for `MUT202`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ReadOnlyReturnRule {
    pub name: String,
    pub producers: Vec<String>,
    pub deep: bool,
}

/// `[[tool.heki.ecosystem]]` user addition / override of a Layer 2 ecosystem KB entry.
///
/// `effects` holds Execution Effect names exactly as spelled in `kb/ecosystem/*.toml`
/// (snake_case, e.g. `"blocking_net"`); `heki_kb` validates them when merging.
/// An override replaces the built-in entry for the same `symbol` as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct EcosystemOverride {
    pub symbol: String,
    pub effects: Vec<String>,
    pub async_safe: bool,
    pub offloads_blocking_arg: Option<u8>,
    pub excluded: bool,
    pub copy_sanitizer: bool,
}

/// Top-level `[tool.heki]` or `heki.toml` configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct HekiConfig {
    pub target_version: Option<String>,
    pub exclude: Option<Vec<String>>,
    pub extend_exclude: Vec<String>,
    pub ignore_file: Option<PathBuf>,
    pub baseline: Option<PathBuf>,
    pub lint: LintConfig,
    pub write_owners: Vec<WriteOwnerRule>,
    pub readonly_returns: Vec<ReadOnlyReturnRule>,
    pub ecosystem: Vec<EcosystemOverride>,
    /// Lower bound of `project.requires-python` in the project's `pyproject.toml`: the
    /// `target-version` default (read by the loaders, not a `[tool.heki]` key).
    #[serde(skip)]
    pub requires_python: Option<PythonVersion>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {cause}")]
    Io {
        path: PathBuf,
        cause: std::io::Error,
    },
    #[error("failed to parse TOML in {path}: {cause}")]
    Toml {
        path: PathBuf,
        cause: toml::de::Error,
    },
}

#[derive(Debug, Default, Deserialize)]
struct PyProjectWrapper {
    #[serde(default)]
    tool: ToolSection,
    #[serde(default)]
    project: ProjectSection,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectSection {
    /// Kept as a raw value: a malformed entry only loses the inference.
    #[serde(rename = "requires-python")]
    requires_python: Option<toml::Value>,
}

impl PyProjectWrapper {
    fn requires_python(&self) -> Option<PythonVersion> {
        let spec = self.project.requires_python.as_ref()?.as_str()?;
        requires_python_lower_bound(spec)
    }
}

/// `requires-python` of a `pyproject.toml` next to a `heki.toml`-style file; any problem
/// with that file only loses the inference.
fn sibling_requires_python(pyproject: &Path) -> Option<PythonVersion> {
    let text = std::fs::read_to_string(pyproject).ok()?;
    toml::from_str::<PyProjectWrapper>(&text)
        .ok()?
        .requires_python()
}

/// The oldest Python minor version a PEP 440 `requires-python` specifier admits (the highest
/// lower bound of its `>=`, `>`, `~=`, `==` and `===` clauses), clamped to the supported
/// 3.11–3.15; `None` when no clause sets a lower bound. `>3.11` admits 3.11.1, so its bound is
/// 3.11, as for `>=3.11`.
pub fn requires_python_lower_bound(spec: &str) -> Option<PythonVersion> {
    spec.split(',')
        .filter_map(|clause| {
            let clause = clause.trim();
            let version = ["===", "~=", "==", ">=", ">"]
                .iter()
                .find_map(|op| clause.strip_prefix(op))?;
            let mut parts = version.trim().trim_end_matches(".*").split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next().map_or(Some(0), |m| m.parse().ok())?;
            Some(PythonVersion { major, minor })
        })
        .max()
        .map(|v| v.clamp(PythonVersion::PY311, PythonVersion::PY315))
}

#[derive(Debug, Default, Deserialize)]
struct ToolSection {
    heki: Option<HekiConfig>,
}

impl HekiConfig {
    /// Load an explicit configuration file: `pyproject.toml` (its `[tool.heki]` table, empty
    /// config when absent) or a `heki.toml`-style file (top-level keys).
    pub fn load_from_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|cause| ConfigError::Io {
            path: path.to_path_buf(),
            cause,
        })?;
        if path.file_name().is_some_and(|n| n == "pyproject.toml") {
            let wrapper: PyProjectWrapper =
                toml::from_str(&text).map_err(|cause| ConfigError::Toml {
                    path: path.to_path_buf(),
                    cause,
                })?;
            let requires_python = wrapper.requires_python();
            return Ok(Self {
                requires_python,
                ..wrapper.tool.heki.unwrap_or_default()
            });
        }
        let cfg: Self = toml::from_str(&text).map_err(|cause| ConfigError::Toml {
            path: path.to_path_buf(),
            cause,
        })?;
        let dir = path.parent().unwrap_or(Path::new(""));
        Ok(Self {
            requires_python: sibling_requires_python(&dir.join("pyproject.toml")),
            ..cfg
        })
    }

    /// Load configuration from either `heki.toml` / `.heki.toml` or `pyproject.toml` `[tool.heki]`.
    pub fn load_from_dir(root: &Path) -> Result<Self, ConfigError> {
        for candidate in ["heki.toml", ".heki.toml"] {
            let p = root.join(candidate);
            if p.is_file() {
                let text = std::fs::read_to_string(&p).map_err(|cause| ConfigError::Io {
                    path: p.clone(),
                    cause,
                })?;
                let cfg: Self =
                    toml::from_str(&text).map_err(|cause| ConfigError::Toml { path: p, cause })?;
                return Ok(Self {
                    requires_python: sibling_requires_python(&root.join("pyproject.toml")),
                    ..cfg
                });
            }
        }
        let pyproject = root.join("pyproject.toml");
        if pyproject.is_file() {
            let text = std::fs::read_to_string(&pyproject).map_err(|cause| ConfigError::Io {
                path: pyproject.clone(),
                cause,
            })?;
            let wrapper: PyProjectWrapper =
                toml::from_str(&text).map_err(|cause| ConfigError::Toml {
                    path: pyproject,
                    cause,
                })?;
            let requires_python = wrapper.requires_python();
            return Ok(Self {
                requires_python,
                ..wrapper.tool.heki.unwrap_or_default()
            });
        }
        Ok(Self::default())
    }

    /// Resolve `RuntimeProfile` from `target-version`, else the lower bound of
    /// `project.requires-python`, else Python 3.12.
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

[[tool.heki.readonly-returns]]
name = "shared-schema"
producers = ["pkg.parser.SchemaGraphBuilder.schema"]
deep = true
"#;
        let wrapper: PyProjectWrapper = toml::from_str(raw).unwrap();
        let cfg = wrapper.tool.heki.unwrap();
        assert!(cfg.runtime_profile().pep810_lazy_imports);
        assert_eq!(cfg.lint.severity.get("MUT101"), Some(&Severity::Warning));
        assert_eq!(cfg.write_owners.len(), 1);
        assert!(cfg.readonly_returns[0].deep);
        assert!(cfg.ecosystem.is_empty());
        assert_eq!(cfg.lint.relax_async_in_tests, None);
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
"#;
        let wrapper: PyProjectWrapper = toml::from_str(raw).unwrap();
        let cfg = wrapper.tool.heki.unwrap();
        assert_eq!(
            cfg.ecosystem,
            vec![
                EcosystemOverride {
                    symbol: "mypkg.http.fetch".into(),
                    effects: vec!["blocking_net".into(), "unpooled_client_alloc".into()],
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.workers.in_thread".into(),
                    async_safe: true,
                    offloads_blocking_arg: Some(0),
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.log.trace".into(),
                    excluded: true,
                    ..EcosystemOverride::default()
                },
                EcosystemOverride {
                    symbol: "mypkg.model.Model.clone".into(),
                    copy_sanitizer: true,
                    ..EcosystemOverride::default()
                },
            ]
        );
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
        assert_eq!(cfg.ecosystem[0].effects, vec!["blocking_db", "db_query"]);
        assert!(!cfg.ecosystem[0].async_safe);
        assert_eq!(cfg.ecosystem[0].offloads_blocking_arg, None);
    }
}
