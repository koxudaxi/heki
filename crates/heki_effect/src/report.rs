//! Boundary-Only Reporting and Root-Cause Grouping helpers for `EFF101` / `EFF105`
//! (docs/ANALYSIS_AND_KB_SPEC.md section 2.4). Rule wiring (codes, severities) is in `heki_rules`.

use crate::summary::BLOCKING_EFFECTS;
use crate::{CallHop, FuncKey, ProjectIndex, SiteRole};
use heki_ast::Span;
use heki_kb::ExecEffectKind;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Reports sharing one root cause are folded into one beyond this many.
pub const ROOT_CAUSE_GROUP_THRESHOLD: usize = 10;

/// An `async def` call site that crosses into blocking synchronous code.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BoundaryReport {
    pub path: PathBuf,
    pub span: Span,
    /// The async function where the crossing happens (the only one reported).
    pub function: FuncKey,
    pub effect: ExecEffectKind,
    /// Final sink of the shortest chain (a Layer 2 symbol such as `time.sleep`).
    pub root_cause: String,
    /// Shortest chain from this call site to the sink.
    pub chain: Vec<CallHop>,
    /// The chain starts where the async function iterates a generator (`SiteRole::Iterate`, or
    /// a project generator iterating what it is handed), not at a call.
    pub iterates: bool,
}

/// One diagnostic to emit: a representative plus how many same-cause reports it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootCauseGroup {
    pub representative: BoundaryReport,
    pub additional: usize,
}

impl RootCauseGroup {
    /// Message suffix for a folded group.
    pub fn message_suffix(&self) -> Option<String> {
        (self.additional > 0).then(|| {
            format!(
                "(+{} more async call sites reaching the same blocking root cause)",
                self.additional
            )
        })
    }
}

/// Boundary-Only Reporting: in `async A -> await async B -> sync C -> time.sleep()` only `B`
/// is reported. A call site of an `async def` fires when its callee is a sync project function
/// (whose body runs right there) or a Layer 2 API with blocking effects left after the
/// `is_awaited` filter; awaiting another `async def` never fires (its own boundary does).
/// Offloads never fire: offload APIs carry no blocking effects and the offloaded callable is
/// not called. Functions marked `sync_driven_only` are skipped.
pub fn boundary_reports(index: &ProjectIndex) -> Vec<BoundaryReport> {
    boundary_reports_in(index, index.files.keys().map(PathBuf::as_path))
}

/// [`boundary_reports`] restricted to the call sites of `paths` (single-file checks).
pub fn boundary_reports_in<'p>(
    index: &ProjectIndex,
    paths: impl IntoIterator<Item = &'p Path>,
) -> Vec<BoundaryReport> {
    // `(file, span, function, effect) -> (shortest chain, it starts at an iteration)`.
    type Best = BTreeMap<(PathBuf, Span, FuncKey, ExecEffectKind), (Vec<CallHop>, bool)>;
    let mut best = Best::new();
    for (path, file) in paths
        .into_iter()
        .filter_map(|p| index.files.get_key_value(p))
    {
        for site in &file.call_sites {
            let reported_caller = index
                .summary(&site.caller)
                .is_some_and(|caller| caller.is_async && !caller.sync_driven_only);
            if !reported_caller {
                continue;
            }
            let iterates = matches!(
                site.role,
                SiteRole::Iterate | SiteRole::Through { iterated: true, .. }
            );
            // Awaiting another `async def` never fires: its own boundary does.
            let found = index
                .site_effects(path, site)
                .into_iter()
                .filter(|se| !se.callee_is_async && BLOCKING_EFFECTS.contains(&se.effect))
                .map(|se| (se.effect, se.chain));
            for (effect, chain) in found {
                let key = (path.clone(), site.span, site.caller.clone(), effect);
                let slot = best.entry(key).or_default();
                if slot.0.is_empty() || chain.len() < slot.0.len() {
                    *slot = (chain, iterates);
                }
            }
        }
    }
    best.into_iter()
        .map(
            |((path, span, function, effect), (chain, iterates))| BoundaryReport {
                root_cause: chain.last().map(|h| h.callee.clone()).unwrap_or_default(),
                path,
                span,
                function,
                effect,
                chain,
                iterates,
            },
        )
        .collect()
}

/// Root-Cause Grouping: when more than `threshold` reports share one `(root_cause, effect)`,
/// emit only the first (by path / position) with the rest counted in `additional`.
pub fn group_by_root_cause(reports: &[BoundaryReport], threshold: usize) -> Vec<RootCauseGroup> {
    let mut by_cause: BTreeMap<(&str, ExecEffectKind), Vec<&BoundaryReport>> = BTreeMap::new();
    for r in reports {
        by_cause
            .entry((r.root_cause.as_str(), r.effect))
            .or_default()
            .push(r);
    }
    let mut out = Vec::new();
    for mut group in by_cause.into_values() {
        group.sort();
        if group.len() > threshold {
            out.push(RootCauseGroup {
                representative: group[0].clone(),
                additional: group.len() - 1,
            });
        } else {
            out.extend(group.into_iter().map(|r| RootCauseGroup {
                representative: r.clone(),
                additional: 0,
            }));
        }
    }
    out.sort_by(|a, b| a.representative.cmp(&b.representative));
    out
}
