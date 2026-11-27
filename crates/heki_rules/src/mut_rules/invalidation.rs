//! `MUT208` (`write-without-invalidation`): a write to a declared `invalidation` target that
//! some normal path of the writing function leaves unannounced (docs/ANALYSIS_AND_KB_SPEC.md
//! section 2.3).
//!
//! The summaries carry the facts (`heki_effect::invalidation`): each function's writes that no
//! invalidator follows on every path to a normal return, directly or through calls, with the
//! writes private helpers pass up. This rule decides where they are reported: a private helper
//! hands them to its callers (they are in its callers' summaries when their call is not
//! announced either), and a function every caller path of which announces the change after
//! calling it (invalidation in bulk by a caller) is not reported.

use super::{MergedReports, is_private, place_functions};
use crate::contracts::ConfigContracts;
use crate::{CheckContext, RuleConfigError, SitesByCaller};
use heki_diagnostics::Diagnostic;
use heki_effect::{
    Construction, ContractClass, CoveredAfter, FuncKey, Reach, UnmetWrite, body_runs_at,
    builds_written_object, is_initializer, reach_across,
};
use rustc_hash::FxHashSet;
use std::collections::VecDeque;

pub(crate) fn run(cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) -> Result<(), RuleConfigError> {
    if !cx.selection.is_enabled("MUT208") || cx.config.invalidation.is_empty() {
        return Ok(());
    }
    let names = ConfigContracts::new(cx.config, cx.kb)?;
    let sites = cx.sites_by_caller();
    let covers = CoveredAfter::new(cx.index);
    let mut paths: Vec<_> = cx.index.files.keys().collect();
    paths.sort();
    // Per contract: two contracts naming one target are two reports.
    let mut merged: Vec<MergedReports<'_>> = cx
        .config
        .invalidation
        .iter()
        .map(|_| MergedReports::default())
        .collect();
    for path in paths {
        let Some(unit) = cx.unit(path) else {
            continue;
        };
        // Every function once, from its defining file.
        let mut funcs: Vec<_> = cx.index.files[path]
            .functions
            .iter()
            .filter(|(k, s)| {
                !s.unmet_invalidation.is_empty() && cx.index.file_of(k) == Some(path.as_path())
            })
            .collect();
        funcs.sort_by(|a, b| a.0.cmp(b.0));
        for (key, s) in funcs {
            // A private helper's writes are its callers' to announce: they are in the callers'
            // summaries where a call is not announced. Without resolved callers there is nothing
            // to check them against.
            if is_private(&key.qualname) {
                continue;
            }
            for u in &s.unmet_invalidation {
                // The index was built with these contracts, so the contract exists.
                let rule = &cx.config.invalidation[usize::from(u.contract)];
                if announced_by_callers(cx, &sites, &covers, &names, key, u) {
                    continue;
                }
                // The configured target that names the write, as the summary matched it.
                let classes = ContractClass::lineage(&names, cx.decls, &u.class_qualname);
                let contract = &names.invalidations[usize::from(u.contract)];
                let target = contract
                    .target(&classes, &u.attr)
                    .map_or_else(|| format!("{}.{}", u.class_qualname, u.attr), str::to_owned);
                let (report_target, named) =
                    reported_names(&rule.name, &target, &u.class_qualname, &u.attr);
                let writer = u
                    .via_chain
                    .last()
                    .map_or_else(|| key.fqn(), |h| h.callee.clone());
                let via = u
                    .via_chain
                    .last()
                    .map(|h| format!(" via `{}`", h.callee))
                    .unwrap_or_default();
                let quoted = format!("`{named}`");
                let name = super::Named {
                    target: &report_target,
                    named: &quoted,
                    rank: 0,
                    contract: usize::from(u.contract),
                };
                merged[usize::from(u.contract)].add(
                    unit,
                    key,
                    u.span,
                    (report_target.clone(), name),
                    (writer, u.origin),
                    rule.message.as_deref(),
                    || {
                        format!(
                            "`{}` writes `{named}` ({}){via} and returns on some path without calling an invalidator of {}",
                            key.qualname,
                            u.kind.describe(),
                            heki_env::contract_label("invalidation", usize::from(u.contract), &rule.name)
                        )
                    },
                    || heki_effect::owned_chain(&u.via_chain),
                );
            }
        }
    }
    for (c, m) in merged.into_iter().enumerate() {
        let rule = &cx.config.invalidation[c];
        let contract = &names.invalidations[c];
        let bit = u8::try_from(c).expect("at most `MAX_INVALIDATIONS` contracts");
        m.emit(cx, ("MUT208", "announcing writers"), out, |examples| {
            let reported = |class: &str, attr: &str| {
                let classes = ContractClass::lineage(&names, cx.decls, class);
                let target = contract.target(&classes, attr)?;
                Some(reported_names(&rule.name, target, class, attr))
            };
            let mut add = |key, path, (report_target, named): (String, String)| {
                let quoted = format!("`{named}`");
                let name = super::Named {
                    target: &report_target,
                    named: &quoted,
                    rank: 0,
                    contract: c,
                };
                examples.add((&report_target, name), key, path);
            };
            for (key, path, s) in place_functions(cx, |_| true) {
                // An exempt writer's writes need no announcement of their own.
                if contract.exempts(cx.decls, key) {
                    let own = s
                        .typed_field_write_sites
                        .iter()
                        .filter(|w| w.via_chain.is_empty() && w.construction != Construction::Yes);
                    for w in own.filter_map(|w| reported(&w.class_qualname, &w.attr)) {
                        add(key, path, w);
                    }
                }
                // A function that leaves one write of a target unannounced, its own or a
                // helper's, announces none: calling it leaves the change unannounced.
                let unannounced: FxHashSet<String> = (s.unmet_invalidation.iter())
                    .filter(|u| u.contract == bit)
                    .filter_map(|u| reported(&u.class_qualname, &u.attr))
                    .map(|(report_target, _)| report_target)
                    .collect();
                let writes = cx.index.files[path].flows.get(key).into_iter();
                let writes = writes.flat_map(|f| &f.writes);
                let targets = (writes.filter(|w| w.contracts & (1 << c) != 0))
                    .filter_map(|w| reported(&w.class_qualname, &w.attr));
                for w in targets.filter(|(target, _)| !unannounced.contains(target)) {
                    add(key, path, w);
                }
            }
        });
    }
    Ok(())
}

/// What a report of a write of `attr` on an object of `class` that the configured `target` of
/// the contract `contract` names says: the report's target and the member it quotes. The
/// report's target names the contract, so two contracts announcing one write are two reports,
/// and two baseline entries; it stands for every original write behind it (two writes of one
/// helper are two sites).
fn reported_names(contract: &str, target: &str, class: &str, attr: &str) -> (String, String) {
    let named = super::matched_name(target, || format!("{class}.{attr}"));
    (format!("{contract}:{target}"), named)
}

/// Whether every caller path of `func` announces the unmet write `u` after calling it: each
/// project caller is an exempt writer of the contract, builds the written object (its own
/// initialization, or an instance it constructs), announces the change on every normal path
/// after the call, or has callers that do so after calling it in turn (invalidation in bulk by a
/// distant caller). The walk goes breadth first over the written object as each function sees
/// it (where it comes from, and the parameters it may be), like `MUT201`'s allowed-context walk:
/// a function without project callers ends a path unannounced, and paths that only cycle
/// announce nothing.
fn announced_by_callers<'i>(
    cx: &CheckContext<'i>,
    sites: &SitesByCaller<'i>,
    covers: &CoveredAfter<'i>,
    names: &ConfigContracts,
    func: &FuncKey,
    u: &UnmetWrite,
) -> bool {
    let bit = 1u64 << u.contract;
    let contract = &names.invalidations[usize::from(u.contract)];
    let start = (func.clone(), u.reach, u.obj_params);
    let mut queue = VecDeque::from([start.clone()]);
    let mut visited: FxHashSet<(FuncKey, Reach, u64)> = FxHashSet::default();
    visited.insert(start);
    let mut announced = false;
    while let Some((callee, reach, obj_params)) = queue.pop_front() {
        let Some(callers) = cx
            .index
            .reverse_call_graph
            .get(&callee)
            .filter(|c| !c.is_empty())
        else {
            return false;
        };
        let callee_summary = cx.index.summary(&callee);
        let mut callers: Vec<&FuncKey> = callers.iter().collect();
        callers.sort();
        for caller in callers {
            if contract.exempts(cx.decls, caller) {
                announced = true;
                continue;
            }
            for (path, site) in sites.get(caller).into_iter().flatten() {
                // The callee's body runs here: a coroutine where it is awaited or driven, a
                // generator where it is iterated.
                if callee_summary.is_some_and(|s| body_runs_at(cx.index, site, s).is_none()) {
                    continue;
                }
                for bc in site
                    .project_callees()
                    .iter()
                    .filter(|bc| bc.callee == callee)
                {
                    // The caller builds the written object (its own initialization, or one it
                    // constructs, copies, or gets from a call): nothing to announce on this path.
                    let caller_reach = reach_across(reach, bc);
                    let caller_obj = bc.caller_params_of(obj_params);
                    let builds =
                        builds_written_object(is_initializer(caller), caller_reach, caller_obj);
                    let covered = site
                        .flow
                        .is_some_and(|f| covers.after(path, &f, bit) & bit != 0);
                    if builds || covered {
                        announced = true;
                    } else if visited.insert((caller.clone(), caller_reach, caller_obj)) {
                        queue.push_back((caller.clone(), caller_reach, caller_obj));
                    }
                }
            }
        }
    }
    announced
}
