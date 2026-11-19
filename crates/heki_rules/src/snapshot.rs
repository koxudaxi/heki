//! `heki snapshot` and `MUT103` (`summary-effect-regression`, docs/DETAILED_SPEC_M3_M7.md
//! section 4.7).
//!
//! The snapshot stores, for every public function (`(module, qualname)` whose name does not
//! start with `_`), its normalized interprocedural summary: mutated parameters, escaping
//! parameters, mutated `self` attributes, and transitive execution effects. `MUT103` reports a
//! function whose current summary has an element its snapshot entry lacks.

use crate::eff_rules::effect_name;
use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::Diagnostic;
use heki_effect::{AttrKeyPath, EffectSummary, FuncKey, ProjectIndex, func_qualname};
use heki_ir::IrFunction;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Snapshot file at the project root.
pub const SNAPSHOT_FILE: &str = ".heki-summary-snapshot.json";
pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEntry {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mutates_params: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escapes_params: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mutates_self_attrs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transitive_exec_effects: Vec<String>,
}

/// Normalized summaries keyed by `module:qualname`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummarySnapshot {
    pub version: u32,
    pub functions: BTreeMap<String, SnapshotEntry>,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("failed to read summary snapshot `{path}`: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write summary snapshot `{path}`: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid summary snapshot `{path}`: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "summary snapshot `{path}` has version {found}; this heki writes version {SNAPSHOT_VERSION} (run `heki snapshot --write`)"
    )]
    Version { path: PathBuf, found: u32 },
}

fn param_names(bits: u64, f: &IrFunction) -> Vec<String> {
    (0..64u32)
        .filter(|i| bits & (1u64 << i) != 0)
        .map(|i| {
            f.params
                .get(i as usize)
                .map_or_else(|| format!("#{i}"), |p| p.name.clone())
        })
        .collect()
}

fn attr_path(p: &AttrKeyPath) -> String {
    match &p.const_key {
        Some(k) => format!("{}[{k:?}]", p.attr),
        None => p.attr.clone(),
    }
}

fn entry(summary: &EffectSummary, f: &IrFunction) -> SnapshotEntry {
    let mut e = SnapshotEntry {
        mutates_params: param_names(summary.mutates_params, f),
        escapes_params: param_names(summary.escapes_params, f),
        mutates_self_attrs: summary.mutates_self_attrs.iter().map(attr_path).collect(),
        transitive_exec_effects: summary
            .transitive_exec_effects
            .iter()
            .map(|k| effect_name(*k).to_owned())
            .collect(),
    };
    for v in [
        &mut e.mutates_params,
        &mut e.escapes_params,
        &mut e.mutates_self_attrs,
        &mut e.transitive_exec_effects,
    ] {
        v.sort();
        v.dedup();
    }
    e
}

/// Public functions: named `def`s (no `<locals>` / lambda / genexpr) whose name has no
/// leading underscore.
fn is_public(f: &IrFunction) -> bool {
    !f.is_module_body && !f.is_class_body && !f.name.starts_with('_') && !f.qualname.contains('<')
}

fn key_of(unit: &ModuleUnit<'_>, f: &IrFunction) -> String {
    format!("{}:{}", unit.input.module_name, func_qualname(f))
}

impl SummarySnapshot {
    pub fn build(units: &[ModuleUnit<'_>], index: &ProjectIndex) -> Self {
        let mut functions = BTreeMap::new();
        for unit in units {
            for f in unit.input.ir.functions.iter().filter(|f| is_public(f)) {
                let key = FuncKey {
                    module: unit.input.module_name.clone(),
                    qualname: func_qualname(f).to_owned(),
                };
                if let Some(s) = index.summary(&key) {
                    functions.insert(key_of(unit, f), entry(s, f));
                }
            }
        }
        Self {
            version: SNAPSHOT_VERSION,
            functions,
        }
    }

    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_default();
        s.push('\n');
        s
    }

    pub fn load(path: &Path) -> Result<Self, SnapshotError> {
        let text = std::fs::read_to_string(path).map_err(|source| SnapshotError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let snap: Self = serde_json::from_str(&text).map_err(|source| SnapshotError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if snap.version != SNAPSHOT_VERSION {
            return Err(SnapshotError::Version {
                path: path.to_path_buf(),
                found: snap.version,
            });
        }
        Ok(snap)
    }

    pub fn save(&self, path: &Path) -> Result<(), SnapshotError> {
        std::fs::write(path, self.to_json()).map_err(|source| SnapshotError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

fn added<'v>(now: &'v [String], before: &[String]) -> Vec<&'v String> {
    now.iter().filter(|x| !before.contains(x)).collect()
}

/// `MUT103`: functions whose side effects grew since the snapshot.
pub(crate) fn mut103(cx: &CheckContext<'_>, snapshot: &SummarySnapshot, out: &mut Vec<Diagnostic>) {
    for unit in cx.units {
        if !cx.enabled("MUT103", unit) {
            continue;
        }
        for f in unit.input.ir.functions.iter().filter(|f| is_public(f)) {
            let key = FuncKey {
                module: unit.input.module_name.clone(),
                qualname: func_qualname(f).to_owned(),
            };
            let (Some(before), Some(now)) = (
                snapshot.functions.get(&key_of(unit, f)),
                cx.index.summary(&key),
            ) else {
                continue;
            };
            let now = entry(now, f);
            let mut parts = Vec::new();
            let mut target = Vec::new();
            for (label, now_v, before_v, describe) in [
                (
                    "mutates_params",
                    &now.mutates_params,
                    &before.mutates_params,
                    "mutates parameter",
                ),
                (
                    "escapes_params",
                    &now.escapes_params,
                    &before.escapes_params,
                    "lets parameter escape:",
                ),
                (
                    "mutates_self_attrs",
                    &now.mutates_self_attrs,
                    &before.mutates_self_attrs,
                    "mutates attribute",
                ),
                (
                    "transitive_exec_effects",
                    &now.transitive_exec_effects,
                    &before.transitive_exec_effects,
                    "reaches effect",
                ),
            ] {
                let new = added(now_v, before_v);
                if !new.is_empty() {
                    let names: Vec<String> = new.iter().map(|n| format!("`{n}`")).collect();
                    parts.push(format!("{describe} {}", names.join(", ")));
                    target.push(format!(
                        "{label}:{}",
                        new.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(",")
                    ));
                }
            }
            if parts.is_empty() {
                continue;
            }
            cx.emit(
                out,
                "MUT103",
                unit,
                f.span,
                func_qualname(f),
                target.join("|"),
                format!(
                    "`{}` gained side effects since the summary snapshot: {}; if intended, refresh it with `heki snapshot --write`",
                    func_qualname(f),
                    parts.join("; ")
                ),
                &[],
            );
        }
    }
}
