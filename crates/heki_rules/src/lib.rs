//! Rule registry spanning all 3 families derived from the single Effect & Alias theory.
//!
//! - Family 1 (`MUT`): Mutation & State Effect (`MUT101`, `MUT103`, `MUT201`, `MUT202`)
//! - Family 2 (`EFF` / `DATA`): Execution Effect (`EFF101`, `EFF102`, `EFF103`, `EFF105`, `DATA101`)
//! - Family 3 (`OPT`): Complexity Rewrite (`OPT101`, `OPT102`, `OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130`)

mod data_rules;
mod eff_rules;
mod mut_rules;
mod opt_rules;
pub mod project;
pub mod selection;
pub mod snapshot;

pub use selection::{RuleSelection, SelectionError};
pub use snapshot::{SNAPSHOT_FILE, SnapshotError, SummarySnapshot};

use globset::{Glob, GlobMatcher};
use heki_ast::{LineIndex, Span};
use heki_diagnostics::{Diagnostic, RelatedLocation};
use heki_effect::{CallHop, CallSite, FuncKey, ModuleInput, ProjectDecls, ProjectIndex};
use heki_env::{HekiConfig, Severity};
use heki_ir::IrFunction;
use heki_kb::{EffectiveEcosystemKb, EffectiveSoundnessKb};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RuleFamily {
    Mut,
    Eff,
    Data,
    Opt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuleMeta {
    pub code: &'static str,
    pub slug: &'static str,
    pub family: RuleFamily,
    pub default_enabled: bool,
    pub auto_fix_default: bool,
    pub summary: &'static str,
}

/// Core v1.0 rule catalog (empirically validated in Stage 0-A + `dcg` study).
pub const CORE_RULES: &[RuleMeta] = &[
    RuleMeta {
        code: "MUT101",
        slug: "cached-property-stale-mutation",
        family: RuleFamily::Mut,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Mutating an attribute read by `@cached_property` without invalidating the cache.",
    },
    RuleMeta {
        code: "MUT103",
        slug: "summary-effect-regression",
        family: RuleFamily::Mut,
        default_enabled: false,
        auto_fix_default: false,
        summary: "Interprocedural mutation/escape/effect regression against saved summary snapshot.",
    },
    RuleMeta {
        code: "MUT201",
        slug: "write-owner-violation",
        family: RuleFamily::Mut,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Writing a guarded attribute/container (directly, via alias, or via helper) outside declared `write-owners`.",
    },
    RuleMeta {
        code: "MUT202",
        slug: "readonly-return-mutation",
        family: RuleFamily::Mut,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Mutating a shared object returned by a declared `readonly-returns` producer or `@lru_cache`.",
    },
    RuleMeta {
        code: "EFF101",
        slug: "async-blocking-call",
        family: RuleFamily::Eff,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Direct or indirect blocking network/DB/subprocess/sleep call reachable from `async def`.",
    },
    RuleMeta {
        code: "EFF102",
        slug: "loop-unpooled-client",
        family: RuleFamily::Eff,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Allocating an unpooled HTTP/DB/RPC client or sessionless request inside a loop.",
    },
    RuleMeta {
        code: "EFF103",
        slug: "loop-db-query",
        family: RuleFamily::Eff,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Explicit DB query / external RPC executed inside a loop (N+1).",
    },
    RuleMeta {
        code: "EFF105",
        slug: "async-blocking-fs",
        family: RuleFamily::Eff,
        default_enabled: false,
        auto_fix_default: false,
        summary: "Opt-in: blocking local filesystem I/O reachable from `async def`.",
    },
    RuleMeta {
        code: "DATA101",
        slug: "loop-carried-concat",
        family: RuleFamily::Data,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Loop-carried quadratic `pandas.concat` / `numpy.vstack` / `numpy.append` accumulation.",
    },
    RuleMeta {
        code: "OPT101",
        slug: "list-quadratic-concat",
        family: RuleFamily::Opt,
        default_enabled: true,
        auto_fix_default: true,
        summary: "Unconditionally profitable `list` quadratic concatenation (`x = x + [e]` -> `x.append(e)`).",
    },
    RuleMeta {
        code: "OPT102",
        slug: "set-dict-quadratic-merge",
        family: RuleFamily::Opt,
        default_enabled: true,
        auto_fix_default: true,
        summary: "Unconditionally profitable `set`/`dict` quadratic merge (`{**d, ...}` with `d` first).",
    },
    RuleMeta {
        code: "OPT103",
        slug: "immutable-quadratic-concat",
        family: RuleFamily::Opt,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Size-dependent immutable quadratic accumulation (`bytes`/`tuple`/attr `str`).",
    },
    RuleMeta {
        code: "OPT110",
        slug: "loop-linear-membership",
        family: RuleFamily::Opt,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Size-dependent `e in lst` inside a loop -> hoist `set` outside loop.",
    },
    RuleMeta {
        code: "OPT111",
        slug: "loop-linear-dedup",
        family: RuleFamily::Opt,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Size-dependent `if v not in seen: seen.append(v)` -> auxiliary `set`.",
    },
    RuleMeta {
        code: "OPT120",
        slug: "list-front-pop",
        family: RuleFamily::Opt,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Size-dependent `q.pop(0)` / `del q[0]` / `q = q[1:]` -> `collections.deque`.",
    },
    RuleMeta {
        code: "OPT121",
        slug: "list-front-insert",
        family: RuleFamily::Opt,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Size-dependent `q.insert(0, v)` -> `collections.deque`.",
    },
    RuleMeta {
        code: "OPT130",
        slug: "loop-invariant-container-rebuild",
        family: RuleFamily::Opt,
        default_enabled: true,
        auto_fix_default: false,
        summary: "Size-dependent loop-invariant container reconstruction (`e in set(keys)`).",
    },
];

/// Look up a rule by either code (`EFF101`) or slug (`async-blocking-call`).
pub fn find_rule(query: &str) -> Option<&'static RuleMeta> {
    CORE_RULES
        .iter()
        .find(|r| r.code.eq_ignore_ascii_case(query) || r.slug.eq_ignore_ascii_case(query))
}

// ==================================================================== project check

/// One analyzed module: its summarization input plus source text for positions.
pub struct ModuleUnit<'a> {
    pub input: ModuleInput<'a>,
    pub source: &'a str,
    /// Project-relative POSIX path (`per-file-ignores`, test-path relaxation).
    pub rel_path: String,
    pub lines: LineIndex,
}

#[derive(Debug, Error)]
pub enum RuleConfigError {
    #[error("invalid write-owners target `{0}` (expected `Class.attr`)")]
    InvalidTarget(String),
    #[error("invalid glob `{pattern}` in {context}: {source}")]
    Glob {
        pattern: String,
        context: &'static str,
        #[source]
        source: globset::Error,
    },
}

/// Inputs beyond the Advisory lane: the Layer 1 KB for `OPT` (Soundness lane) and the saved
/// summary snapshot for `MUT103`.
pub struct CheckExtras<'a> {
    pub soundness_kb: &'a EffectiveSoundnessKb,
    /// `None`: no snapshot was loaded (`MUT103` reports nothing).
    pub snapshot: Option<&'a SummarySnapshot>,
    /// Declarations of every indexed module, when the caller keeps them up to date (the LSP
    /// engine); otherwise they are built from the index.
    pub decls: Option<&'a ProjectDecls>,
}

/// Shared state of one `heki check` run.
pub(crate) struct CheckContext<'a> {
    pub config: &'a HekiConfig,
    pub kb: &'a EffectiveEcosystemKb,
    pub soundness: &'a EffectiveSoundnessKb,
    pub index: &'a ProjectIndex,
    pub decls: &'a ProjectDecls,
    pub units: &'a [ModuleUnit<'a>],
    pub selection: &'a RuleSelection,
    unit_by_path: FxHashMap<&'a Path, usize>,
}

/// Run every enabled rule over an analyzed project.
/// Inline suppressions and baselines are applied by the caller (`heki check`).
pub fn check_project(
    units: &[ModuleUnit<'_>],
    config: &HekiConfig,
    kb: &EffectiveEcosystemKb,
    index: &ProjectIndex,
    selection: &RuleSelection,
    extras: &CheckExtras<'_>,
) -> Result<Vec<Diagnostic>, RuleConfigError> {
    let owned_decls;
    let decls = match extras.decls {
        Some(d) => d,
        None => {
            owned_decls = index.project_decls();
            &owned_decls
        }
    };
    let cx = CheckContext {
        config,
        kb,
        soundness: extras.soundness_kb,
        index,
        decls,
        units,
        selection,
        unit_by_path: units
            .iter()
            .enumerate()
            .map(|(i, u)| (u.input.path.as_path(), i))
            .collect(),
    };
    let mut out = Vec::new();
    mut_rules::mut101(&cx, &mut out);
    mut_rules::mut201(&cx, &mut out)?;
    eff_rules::eff101_105(&cx, &mut out);
    if let Some(snap) = extras.snapshot {
        snapshot::mut103(&cx, snap, &mut out);
    }
    let readonly = mut_rules::ReadOnlyProducers::new(config)?;
    // Assumption A1(b) is project-wide: one module patching `builtins` taints them all.
    let opt_opts = heki_soundness::AnalysisOptions {
        project_patches_builtins: units
            .iter()
            .any(|u| heki_soundness::module_patches_builtins(u.input.ir)),
        disabled_obligations: Vec::new(),
    };
    let per_unit: Vec<Vec<Diagnostic>> = units
        .par_iter()
        .map(|u| {
            let mut d = Vec::new();
            mut_rules::mut202(&cx, u, &readonly, &mut d);
            eff_rules::eff102_103(&cx, u, &mut d);
            data_rules::data101(&cx, u, &mut d);
            opt_rules::opt(&cx, u, &opt_opts, &mut d);
            d
        })
        .collect();
    out.extend(per_unit.into_iter().flatten());
    heki_diagnostics::sort_diagnostics(&mut out);
    out.dedup_by(|a, b| {
        a.path == b.path && a.span == b.span && a.rule_code == b.rule_code && a.target == b.target
    });
    Ok(out)
}

impl<'a> CheckContext<'a> {
    pub fn unit(&self, path: &Path) -> Option<&'a ModuleUnit<'a>> {
        self.unit_by_path.get(path).map(|&i| &self.units[i])
    }

    /// Whether `code` runs on `unit` (selection + `per-file-ignores`).
    pub fn enabled(&self, code: &str, unit: &ModuleUnit<'_>) -> bool {
        self.selection.is_enabled_for(code, &unit.rel_path)
    }

    /// `FuncKey` of an IR function of `unit`.
    pub fn key(unit: &ModuleUnit<'_>, f: &IrFunction) -> FuncKey {
        FuncKey {
            module: unit.input.module_name.clone(),
            qualname: heki_effect::func_qualname(f).to_owned(),
        }
    }

    /// Call sites of `unit` keyed by `(caller qualname, span)`.
    pub fn sites_by_span(
        &self,
        unit: &ModuleUnit<'_>,
    ) -> FxHashMap<(String, Span), Vec<&'a CallSite>> {
        let mut out: FxHashMap<(String, Span), Vec<&CallSite>> = FxHashMap::default();
        if let Some(file) = self.index.files.get(&unit.input.path) {
            for site in &file.call_sites {
                out.entry((site.caller.qualname.clone(), site.span))
                    .or_default()
                    .push(site);
            }
        }
        out
    }

    fn related(&self, chain: &[CallHop]) -> Vec<RelatedLocation> {
        chain
            .iter()
            .map(|hop| {
                let (line, column) = self
                    .unit(&hop.path)
                    .map(|u| u.lines.line_col(hop.span.start))
                    .unwrap_or((0, 0));
                RelatedLocation {
                    path: hop.path.clone(),
                    line,
                    column,
                    span: hop.span,
                    message: format!("`{}` calls `{}`", hop.caller.qualname, hop.callee),
                }
            })
            .collect()
    }

    /// Build a diagnostic (severity from `lint.severity`, default `error`).
    #[allow(clippy::too_many_arguments)]
    pub fn emit(
        &self,
        out: &mut Vec<Diagnostic>,
        code: &'static str,
        unit: &ModuleUnit<'_>,
        span: Span,
        qualname: &str,
        target: String,
        message: String,
        chain: &[CallHop],
    ) {
        if !self.enabled(code, unit) {
            return;
        }
        let meta = find_rule(code).expect("emitted rules are registered");
        let severity = self
            .config
            .lint
            .severity
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(meta.code) || k.eq_ignore_ascii_case(meta.slug))
            .map(|(_, v)| *v)
            .unwrap_or(Severity::Error);
        let (line, column) = unit.lines.line_col(span.start);
        out.push(Diagnostic {
            path: unit.input.path.clone(),
            line,
            column,
            span,
            qualname: qualname.to_owned(),
            rule_code: meta.code.to_owned(),
            rule_slug: meta.slug.to_owned(),
            target,
            severity,
            message,
            evidence: None,
            blocker: None,
            related: self.related(chain),
            fix: None,
        });
    }
}

/// Dotted-name pattern: a glob (`pkg.*.Model`, `*Store.*`) that also matches when it equals a
/// dot-separated suffix of the name (`Model.fields` matches `pkg.model.Model.fields`).
pub(crate) struct DottedPattern {
    text: String,
    glob: GlobMatcher,
}

impl DottedPattern {
    pub fn new(text: &str, context: &'static str) -> Result<Self, RuleConfigError> {
        let glob = Glob::new(text)
            .map_err(|source| RuleConfigError::Glob {
                pattern: text.to_owned(),
                context,
                source,
            })?
            .compile_matcher();
        Ok(Self {
            text: text.to_owned(),
            glob,
        })
    }

    pub fn matches(&self, name: &str) -> bool {
        self.glob.is_match(name)
            || name == self.text
            || name
                .strip_suffix(self.text.as_str())
                .is_some_and(|head| head.ends_with('.'))
            || self.text.contains('*')
                && name
                    .match_indices('.')
                    .any(|(i, _)| self.glob.is_match(&name[i + 1..]))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn dotted_patterns_match_suffixes_and_globs() {
        let p = DottedPattern::new("Model.fields", "test").unwrap();
        assert!(p.matches("pkg.model.Model.fields"));
        assert!(p.matches("Model.fields"));
        assert!(!p.matches("pkg.BigModel.fields"));
        let g = DottedPattern::new("Store.*", "test").unwrap();
        assert!(g.matches("Store.add_field"));
        assert!(g.matches("pkg.store.Store.add_field"));
        assert!(!g.matches("pkg.parser.parse"));
    }

    #[test]
    fn resolves_rules_by_code_or_slug() {
        assert_eq!(find_rule("EFF101").unwrap().slug, "async-blocking-call");
        assert_eq!(find_rule("write-owner-violation").unwrap().code, "MUT201");
        assert!(find_rule("OPT101").unwrap().auto_fix_default);
        assert!(!find_rule("OPT110").unwrap().auto_fix_default);
    }
}
