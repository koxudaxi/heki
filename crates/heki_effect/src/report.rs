//! Boundary-Only Reporting for `EFF101` / `EFF105` (docs/ANALYSIS_AND_KB_SPEC.md section 2.4).
//! Rule wiring (codes, severities) and Root-Cause Grouping of the output are in `heki_rules`.

use crate::callgraph::{HopGap, MustExplainer, MustGap, site_must_steps};
use crate::summary::BLOCKING_EFFECTS;
use crate::{CallHop, CallSite, FuncKey, ProjectIndex, SiteMust, SiteRole};
use heki_ast::Span;
use heki_kb::ExecEffectKind;
use std::collections::BTreeMap;
use std::ops::ControlFlow;
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
    /// Why `must` is not established, along `chain` (`HopGap::hop` indexes it): filled only
    /// when [`boundary_reports_in`] is asked to explain, and empty when `must` is set.
    pub gaps: Vec<HopGap>,
}

/// Boundary-Only Reporting: in `async A -> await async B -> sync C -> time.sleep()` only `B`
/// is reported. A call site of an `async def` fires when its callee is a sync project function
/// (whose body runs right there) or a Layer 2 API with blocking effects left after the
/// `is_awaited` filter; awaiting another `async def` never fires (its own boundary does).
/// Offloads never fire: offload APIs carry no blocking effects and the offloaded callable is
/// not called. Functions marked `sync_driven_only` are skipped, and so is a site no path
/// reaches (`OnEveryReturn::unreached`): behind a branch the code decides against, in a loop
/// over an empty iterable, or in the `else` of a loop that leaves on its first item.
pub fn boundary_reports(index: &ProjectIndex) -> Vec<BoundaryReport> {
    boundary_reports_in(index, index.files.keys().map(PathBuf::as_path), None)
}

/// [`boundary_reports`] restricted to the call sites of `paths` (single-file checks). With an
/// `explainer`, each report whose `must` is not established says why ([`BoundaryReport::gaps`]).
pub fn boundary_reports_in<'p>(
    index: &ProjectIndex,
    paths: impl IntoIterator<Item = &'p Path>,
    explainer: Option<&MustExplainer<'_>>,
) -> Vec<BoundaryReport> {
    // `(file, span, function, effect) -> (shortest chain, its call site, it starts at an
    // iteration, what the call has run whenever it returns)`.
    type Best<'i> = BTreeMap<
        (PathBuf, Span, FuncKey, ExecEffectKind),
        (Vec<CallHop>, Option<&'i CallSite>, bool, Option<SiteMust>),
    >;
    let mut best = Best::new();
    for (path, file) in paths
        .into_iter()
        .filter_map(|p| index.files.get_key_value(p))
    {
        for site in &file.call_sites {
            let reported_caller = index
                .summary(&site.caller)
                .is_some_and(|caller| caller.is_async && !caller.sync_driven_only);
            if !reported_caller || site.on_every_return.unreached {
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
                let must =
                    boundary_must(index, path, site, effect, &mut |_| ControlFlow::Break(()));
                let slot = best.entry(key).or_default();
                if slot.0.is_empty() || chain.len() < slot.0.len() {
                    slot.0 = chain;
                    slot.1 = Some(site);
                    slot.2 = iterates;
                }
                if slot.3.is_none() {
                    slot.3 = must;
                }
            }
        }
    }
    best.into_iter()
        .map(
            |((path, span, function, effect), (chain, site, iterates, must))| {
                let gaps = match (site, explainer) {
                    (Some(site), Some(explainer)) if must.is_none() => {
                        boundary_gaps(index, explainer, &path, site, effect, &chain)
                    }
                    _ => Vec::new(),
                };
                BoundaryReport {
                    root_cause: chain.last().map(|h| h.callee.clone()).unwrap_or_default(),
                    path,
                    span,
                    function,
                    effect,
                    chain,
                    iterates,
                    must,
                    gaps,
                }
            },
        )
        .collect()
}

/// Whether the call at `site` of an `async def` has run `e` on the event loop's thread whenever
/// it returns ([`ProjectIndex::site_must`]), telling each step that does not hold to `gaps`
/// ([`boundary_steps`], then `callgraph::site_must_steps`).
fn boundary_must(
    index: &ProjectIndex,
    path: &Path,
    site: &CallSite,
    e: ExecEffectKind,
    gaps: &mut impl FnMut(MustGap) -> ControlFlow<()>,
) -> Option<SiteMust> {
    let ControlFlow::Continue(open) = boundary_steps(index, site, gaps) else {
        return None;
    };
    site_must_steps(index, path, site, e, gaps).filter(|_| !open)
}

/// The step of a boundary's call before its targets' facts: an `async def` target runs its
/// blocking calls under its own boundary. `Continue(true)` when it does not hold and the
/// decision goes on; `Break` when it stops there. A call that runs no target's body there is
/// left to `site_must_steps`.
fn boundary_steps(
    index: &ProjectIndex,
    site: &CallSite,
    gaps: &mut impl FnMut(MustGap) -> ControlFlow<()>,
) -> ControlFlow<(), bool> {
    let mut open = false;
    if !site.runs_targets_here() {
        return ControlFlow::Continue(open);
    }
    for bc in site.project_callees() {
        if index.summary(&bc.callee).is_some_and(|c| c.is_async) {
            open = true;
            gaps(MustGap::NotRunAtCall(Some(bc.callee.clone())))?;
        }
    }
    ControlFlow::Continue(open)
}

/// [`boundary_steps`]' gaps at the call, then those of `chain` from it
/// ([`MustExplainer::gaps_along`]), each told once.
fn boundary_gaps(
    index: &ProjectIndex,
    explainer: &MustExplainer<'_>,
    path: &Path,
    site: &CallSite,
    e: ExecEffectKind,
    chain: &[CallHop],
) -> Vec<HopGap> {
    let mut out: Vec<HopGap> = Vec::new();
    let _ = boundary_steps(index, site, &mut |gap| {
        out.push(HopGap { hop: 0, gap });
        ControlFlow::Continue(())
    });
    for gap in explainer.gaps_along(path, site, e, chain) {
        if !out.contains(&gap) {
            out.push(gap);
        }
    }
    out
}
