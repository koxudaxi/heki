//! Boundary-Only Reporting for `EFF101` / `EFF105` (docs/ANALYSIS_AND_KB_SPEC.md section 2.4).
//! Rule wiring (codes, severities) and Root-Cause Grouping of the output are in `heki_rules`.

use crate::summary::BLOCKING_EFFECTS;
use crate::{CallHop, FuncKey, ProjectIndex, SiteMust, SiteRole};
use heki_ast::Span;
use heki_kb::ExecEffectKind;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    /// Whenever the call returns, the effect has run on the event loop's thread: every
    /// implementation the call can run is a synchronous function or a Layer 2 call that has run
    /// it ([`ProjectIndex::site_must`]). `None` when that is not established.
    pub must: Option<SiteMust>,
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
    // `(file, span, function, effect) -> (shortest chain, it starts at an iteration, what the
    // call has run whenever it returns)`.
    type Best =
        BTreeMap<(PathBuf, Span, FuncKey, ExecEffectKind), (Vec<CallHop>, bool, Option<SiteMust>)>;
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
            // An `async def` target runs its blocking calls under its own boundary; a call the
            // code never reaches establishes nothing.
            let synchronous = site.on_every_return.live
                && site
                    .project_callees()
                    .iter()
                    .all(|bc| index.summary(&bc.callee).is_some_and(|c| !c.is_async));
            for (effect, chain) in found {
                let key = (path.clone(), site.span, site.caller.clone(), effect);
                let must = synchronous
                    .then(|| index.site_must(path, site, effect))
                    .flatten();
                let slot = best.entry(key).or_default();
                if slot.0.is_empty() || chain.len() < slot.0.len() {
                    slot.0 = chain;
                    slot.1 = iterates;
                }
                if slot.2.is_none() {
                    slot.2 = must;
                }
            }
        }
    }
    best.into_iter()
        .map(
            |((path, span, function, effect), (chain, iterates, must))| BoundaryReport {
                root_cause: chain.last().map(|h| h.callee.clone()).unwrap_or_default(),
                path,
                span,
                function,
                effect,
                chain,
                iterates,
                must,
            },
        )
        .collect()
}
