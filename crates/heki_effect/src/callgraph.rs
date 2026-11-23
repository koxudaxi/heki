//! Interprocedural propagation (docs/ANALYSIS_AND_KB_SPEC.md section 2.2).
//!
//! `propagate` is the single fixpoint engine: it recomputes the final summaries of a
//! `region` of functions from their local facts and the current finals of their callees,
//! visiting Tarjan SCCs callees-first and iterating inside each SCC with a worklist.
//! `ProjectIndex::build` runs it over every function; `ProjectIndex::update_file` over the
//! edited file's SCC closure and then over the affected callers only.

use crate::fresh::FreshReturn;
use crate::guards;
use crate::iterations;
use crate::scc::tarjan_scc;
use crate::{
    ArgOrigin, AttrKeyPath, BoundCallee, CallCycle, CallHop, CallSite, CallTarget, CellChange,
    Construction, Delivery, EffectSummary, Facts, FuncKey, LocalOrigins, NewArg, NewCond, NewKind,
    Origins, ProjectIndex, ProtectedReturn, ReturnsNew, Runner, Shared, SharedChain, SharedLine,
    SharedState, SiteRole, Slot, TypedFieldWriteSite, UnresolvedWriteSite, WriteKind, cache_flow,
};
use heki_ast::Span;
use heki_kb::ExecEffectKind;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeSet, VecDeque};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// Recomputations allowed per SCC member before the SCC is declared non-converged.
const MAX_ITERATIONS_PER_SCC_MEMBER: usize = 16;

/// Web-route decorator names that keep `sync_driven_only` off (`@app.get(...)`, ...).
const ROUTE_DECORATORS: &[&str] = &[
    "get",
    "post",
    "put",
    "delete",
    "patch",
    "head",
    "options",
    "trace",
    "route",
    "api_route",
    "websocket",
];

type OutSites = FxHashMap<FuncKey, Vec<(PathBuf, usize)>>;

/// Add the outgoing call sites of every function of `path` (`caller -> [(file, index)]`; a
/// function's call sites all live in its own file). `path` is a defining file of
/// `func_files`, so it is indexed.
fn load_out_sites(index: &ProjectIndex, path: &Path, out: &mut OutSites) {
    for (i, site) in index.files[path].call_sites.iter().enumerate() {
        out.entry(site.caller.clone())
            .or_default()
            .push((path.to_path_buf(), i));
    }
}

/// Outgoing call sites of the functions of the files defining `funcs` (all files for a full
/// build, the edited region for an update: the cost follows the region, not the project).
fn out_sites_of<'f>(
    index: &ProjectIndex,
    funcs: impl IntoIterator<Item = &'f FuncKey>,
) -> OutSites {
    let files: FxHashSet<&Path> = funcs
        .into_iter()
        .filter_map(|f| index.func_files.get(f).map(PathBuf::as_path))
        .collect();
    let mut out = OutSites::default();
    for path in files {
        load_out_sites(index, path, &mut out);
    }
    for sites in out.values_mut() {
        sites.sort();
    }
    out
}

/// Reset the final summaries of `region` to their local facts. `sync_driven_only` is kept: it
/// is decided by the function's callers, not propagated from its callees, and recomputed only
/// for the functions whose callers may have changed ([`recompute_sync_driven_for`]).
pub(crate) fn reset_to_local(index: &mut ProjectIndex, region: &FxHashSet<FuncKey>) {
    for func in region {
        let Some(path) = index.func_files.get(func).cloned() else {
            continue;
        };
        let file = index
            .files
            .get_mut(&path)
            .expect("`func_files` names an indexed file");
        let mut local = file
            .local_functions
            .get(func)
            .cloned()
            .expect("the file `func_files` names defines the function");
        if file
            .functions
            .get(func)
            .is_some_and(|cur| cur.sync_driven_only)
        {
            Arc::make_mut(&mut local).sync_driven_only = true;
        }
        file.functions.insert(func.clone(), local);
        if let Some(tables) = &mut file.opt_in_tables {
            tables.fresh_returns.reset(func);
        }
    }
}

/// `own` plus every function on a call cycle through `own` (reachable from it and reaching it).
///
/// Every node on a path from `own` to a member of the closure also reaches `own`, so the
/// forward search only expands callers of `own` (computed first, on the reverse call graph):
/// call sites are loaded for the closure's files only.
pub(crate) fn scc_closure(index: &ProjectIndex, own: &FxHashSet<FuncKey>) -> FxHashSet<FuncKey> {
    let mut backward: FxHashSet<FuncKey> = own.clone();
    let mut stack: Vec<FuncKey> = own.iter().cloned().collect();
    while let Some(f) = stack.pop() {
        for caller in index.reverse_call_graph.get(&f).into_iter().flatten() {
            if backward.insert(caller.clone()) {
                stack.push(caller.clone());
            }
        }
    }
    let mut out = OutSites::default();
    let mut loaded: FxHashSet<PathBuf> = FxHashSet::default();
    let mut forward: FxHashSet<FuncKey> = own.clone();
    let mut stack: Vec<FuncKey> = own.iter().cloned().collect();
    while let Some(f) = stack.pop() {
        if let Some(path) = index.func_files.get(&f)
            && loaded.insert(path.clone())
        {
            load_out_sites(index, path, &mut out);
        }
        for (path, i) in out.get(&f).cloned().into_iter().flatten() {
            for c in index.files[&path].call_sites[i].project_callees() {
                if backward.contains(&c.callee) && forward.insert(c.callee.clone()) {
                    stack.push(c.callee.clone());
                }
            }
        }
    }
    let mut region: FxHashSet<FuncKey> = forward.intersection(&backward).cloned().collect();
    region.extend(own.iter().cloned());
    region
}

/// Recompute final summaries of `region` to a fixpoint (callees outside it stay fixed).
pub(crate) fn propagate(index: &mut ProjectIndex, region: &FxHashSet<FuncKey>) {
    propagate_with_budget(index, region, MAX_ITERATIONS_PER_SCC_MEMBER);
}

pub(crate) fn propagate_with_budget(
    index: &mut ProjectIndex,
    region: &FxHashSet<FuncKey>,
    max_iterations_per_member: usize,
) {
    let mut nodes: Vec<FuncKey> = region
        .iter()
        .filter(|k| index.func_files.contains_key(*k))
        .cloned()
        .collect();
    if nodes.is_empty() {
        return;
    }
    nodes.sort();
    let out = out_sites_of(index, &nodes);
    let pos: FxHashMap<&FuncKey, usize> = nodes.iter().enumerate().map(|(i, k)| (k, i)).collect();
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, f) in nodes.iter().enumerate() {
        for (path, si) in out.get(f).into_iter().flatten() {
            for c in index.files[path].call_sites[*si].project_callees() {
                if let Some(&j) = pos.get(&c.callee) {
                    succ[i].push(j);
                    preds[j].push(i);
                }
            }
        }
        succ[i].sort_unstable();
        succ[i].dedup();
    }
    for p in &mut preds {
        p.sort_unstable();
        p.dedup();
    }
    let on_cycle = if index.opt_in.call_cycles {
        on_call_cycles(index, &out, &nodes, &pos)
    } else {
        vec![false; nodes.len()]
    };

    for scc in tarjan_scc(nodes.len(), &succ) {
        let members: FxHashSet<usize> = scc.iter().copied().collect();
        let mut queue: VecDeque<usize> = scc.iter().copied().collect();
        let mut queued: FxHashSet<usize> = members.clone();
        let budget = max_iterations_per_member * scc.len();
        let mut steps = 0;
        let mut converged = true;
        while let Some(i) = queue.pop_front() {
            queued.remove(&i);
            steps += 1;
            if steps > budget {
                converged = false;
                break;
            }
            let in_scc = |k: &FuncKey| pos.get(k).is_some_and(|j| members.contains(j));
            let new = recompute(index, &out, &nodes[i], &in_scc, on_cycle[i]);
            let fresh = Resolver {
                index,
                in_scc: &in_scc,
            }
            .resolve_fresh_return(&nodes[i]);
            let fresh_changed = index.fresh_return(&nodes[i]) != fresh.as_ref();
            if fresh_changed {
                store_fresh_return(index, &nodes[i], fresh);
            }
            let summary_changed = index.summary(&nodes[i]) != Some(&new);
            if summary_changed {
                store(index, &nodes[i], new);
            }
            if summary_changed || fresh_changed {
                for &p in &preds[i] {
                    if members.contains(&p) && queued.insert(p) {
                        queue.push_back(p);
                    }
                }
            }
        }
        if !converged {
            for &i in &scc {
                if let Some(mut s) = index.summary(&nodes[i]).cloned() {
                    s.unknown_calls = true;
                    store(index, &nodes[i], s);
                }
            }
        }
    }
}

/// `func` is a propagation node, and `propagate_with_budget` keeps only the functions of indexed
/// files as nodes.
fn store(index: &mut ProjectIndex, func: &FuncKey, summary: EffectSummary) {
    let file = index
        .files
        .get_mut(&index.func_files[func])
        .expect("a propagation node belongs to an indexed file");
    let summary = match file.local_functions.get(func) {
        Some(local) if **local == summary => Arc::clone(local),
        _ => Arc::new(summary),
    };
    file.functions.insert(func.clone(), summary);
}

/// Store the final `MUT119` fact of `func`, a propagation node like [`store`]'s.
fn store_fresh_return(index: &mut ProjectIndex, func: &FuncKey, fact: Option<FreshReturn>) {
    let file = index
        .files
        .get_mut(&index.func_files[func])
        .expect("a propagation node belongs to an indexed file");
    match fact {
        Some(fact) => {
            (file.opt_in_tables_mut().fresh_returns.resolved).insert(func.clone(), fact);
        }
        None => {
            if let Some(tables) = &mut file.opt_in_tables {
                tables.fresh_returns.resolved.remove(func);
            }
        }
    }
}

/// The propagation nodes on a call cycle ([`CallCycle`]): the members of the components, over
/// the calls whose callee's body runs on the caller's stack ([`body_runs_on_stack`]), of more
/// than one node or with a call of the node itself. A cycle lies within the region a
/// propagation recomputes: the region holds every function sharing a cycle with an edited one,
/// and every caller of a changed one, each member of its cycle included.
fn on_call_cycles(
    index: &ProjectIndex,
    out: &OutSites,
    nodes: &[FuncKey],
    pos: &FxHashMap<&FuncKey, usize>,
) -> Vec<bool> {
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (i, f) in nodes.iter().enumerate() {
        for (path, si) in out.get(f).into_iter().flatten() {
            let site = &index.files[path].call_sites[*si];
            for c in site.project_callees() {
                let runs = index
                    .summary(&c.callee)
                    .is_some_and(|callee| body_runs_on_stack(site, callee));
                if let (true, Some(&j)) = (runs, pos.get(&c.callee)) {
                    succ[i].push(j);
                }
            }
        }
    }
    let mut on_cycle = vec![false; nodes.len()];
    for component in tarjan_scc(nodes.len(), &succ) {
        if let [only] = component[..]
            && !succ[only].contains(&only)
        {
            continue;
        }
        for i in component {
            on_cycle[i] = true;
        }
    }
    on_cycle
}

/// Whether the callee's body runs on the caller's stack at `site`: called there (not a
/// coroutine only created, nor a generator only made), or a generator iterated there, never on a
/// worker thread.
fn body_runs_on_stack(site: &CallSite, callee: &EffectSummary) -> bool {
    site.runner != Runner::Worker
        && match site.role {
            SiteRole::Call => runs_at_call(callee, site),
            SiteRole::Iterate => callee.is_generator,
            SiteRole::Return | SiteRole::Through { .. } => false,
        }
}

/// Final summary of `func` = its local facts joined with every outgoing edge's transfer;
/// `on_cycle`: `func` is on a call cycle ([`on_call_cycles`]).
fn recompute(
    index: &ProjectIndex,
    out: &FxHashMap<FuncKey, Vec<(PathBuf, usize)>>,
    func: &FuncKey,
    in_scc: &dyn Fn(&FuncKey) -> bool,
    on_cycle: bool,
) -> EffectSummary {
    let path = &index.func_files[func];
    let file = &index.files[path];
    let local = &file.local_functions[func];
    let mut s = EffectSummary::clone(local);
    if on_cycle {
        s.call_cycle = Some(CallCycle::On);
    }
    index.param_capacities.realize(local, &mut s);
    s.sync_driven_only = file
        .functions
        .get(func)
        .is_some_and(|cur| cur.sync_driven_only);
    let contracts = index.contracts.as_deref();
    let resolver = Resolver { index, in_scc };
    // The local facts first: the sites the edges add are resolved already.
    resolver.resolve_own(&mut s);
    // The callees of the `self` calls of a getter holding attributes by reference.
    let mut self_calls: Vec<(&CallSite, &EffectSummary)> = Vec::new();
    for (site_path, i) in out.get(func).into_iter().flatten() {
        let site = &index.files[site_path].call_sites[*i];
        // `MUT106`: the call consumes what every function it may run consumes.
        let mut consumed: Option<u64> = None;
        for bc in site.project_callees() {
            let Some(callee) = index.summary(&bc.callee) else {
                consumed = Some(0);
                continue;
            };
            if site.recv_is_self && !local.returns_self_attrs.is_empty() {
                self_calls.push((site, callee));
            }
            let here = apply_edge(
                index, &mut s, local, func, site, bc, callee, site_path, &resolver,
            );
            if !callee.is_abstract() {
                consumed = Some(consumed.map_or(here, |c| c & here));
            }
            return_flow_edge(&mut s, contracts, func, site, bc, callee, site_path);
        }
        s.consumes_params |= consumed.unwrap_or(0) & s.unconditional_forwards;
    }
    if index.opt_in.return_origins {
        returned_results(
            index,
            out.get(func).map(Vec::as_slice).unwrap_or(&[]),
            &mut s,
        );
    }
    if index.opt_in.set_order {
        crate::order_flow::propagate(&|k| index.summary(k), func, path, &mut s);
    }
    if index.opt_in.must_effects {
        must_through_calls(
            index,
            out.get(func).map(Vec::as_slice).unwrap_or(&[]),
            &mut s,
        );
    }
    if index.opt_in.kept_values {
        must_reads_through_self_calls(
            index,
            out.get(func).map(Vec::as_slice).unwrap_or(&[]),
            in_scc,
            &mut s,
        );
    }
    if !s.returns_self_attrs.is_empty() {
        drop_attrs_read_by_discarded_calls(index, &self_calls, local, &mut s);
    }
    let hidden = std::mem::take(&mut s.declared_hidden);
    s.hide_exec_effects(&hidden);
    s.declared_hidden = hidden;
    if s.is_cached_property {
        guards::cached_per_instance(&mut s);
    }
    // Returned objects that run nothing when iterated hand nothing out.
    if s.iterated
        .as_deref()
        .is_some_and(|i| *i == EffectSummary::default())
    {
        s.iterated = None;
    }
    if let Some(c) = contracts {
        let sites = out.get(func).map(Vec::as_slice).unwrap_or(&[]);
        crate::invalidation::propagate_facts(index, c, func, sites, &mut s);
    }
    s.refresh_typed_field_writes();
    s
}

/// [`crate::KeptFacts::must_read_self_attrs`] the function gets from the `self` methods it has
/// called and the properties it has read, and that returned, whenever it returns normally: the
/// attributes every target reads on every path, when the targets are every implementation the
/// call can run and each runs its body at every call (a cached property or function may hand
/// out what an earlier call computed). A callee of the function's own call-graph component adds nothing, so the fact
/// needs no fixpoint of its own. A name bound to two definitions gains nothing: each site's
/// paths are its own definition's, and a call runs one of them.
fn must_reads_through_self_calls(
    index: &ProjectIndex,
    sites: &[(PathBuf, usize)],
    in_scc: &dyn Fn(&FuncKey) -> bool,
    s: &mut EffectSummary,
) {
    if s.must_exec.as_ref().is_some_and(|m| m.bound_twice) {
        return;
    }
    let mut gained: BTreeSet<AttrKeyPath> = BTreeSet::new();
    for (path, i) in sites {
        let site = &index.files[path].call_sites[*i];
        if !site.recv_is_self
            || !site.complete
            || !site.on_every_return.returned
            || site.role != SiteRole::Call
            || site.runner != Runner::Caller
        {
            continue;
        }
        let mut common: Option<BTreeSet<AttrKeyPath>> = None;
        for bc in site.project_callees() {
            let callee = index.summary(&bc.callee);
            // An abstract method never runs: an override does.
            if callee.is_some_and(EffectSummary::is_abstract) {
                continue;
            }
            let reads = callee
                .filter(|callee| {
                    let bound_to_self = callee.owner_class_qualname.is_some()
                        && !callee.decorators.iter().any(|d| {
                            matches!(
                                crate::decorators::last_name(d),
                                "staticmethod" | "classmethod"
                            )
                        });
                    bound_to_self
                        && !in_scc(&bc.callee)
                        && runs_at_call(callee, site)
                        && crate::decorators::keeps_returned_value(&callee.decorators)
                })
                .map(EffectSummary::must_read_self_attrs);
            let reads: BTreeSet<AttrKeyPath> = reads.into_iter().flatten().cloned().collect();
            common = Some(match common {
                None => reads,
                Some(c) => must_reads_of_both(&c, &reads),
            });
        }
        gained.extend(common.into_iter().flatten());
    }
    if gained.is_empty() {
        return;
    }
    s.kept_mut().must_read_self_attrs.extend(gained);
}

/// The reads both `a` and `b` make on every path ([`crate::KeptFacts::must_read_self_attrs`]): an
/// attribute read as a whole covers each of its keys.
pub(crate) fn must_reads_of_both(
    a: &BTreeSet<AttrKeyPath>,
    b: &BTreeSet<AttrKeyPath>,
) -> BTreeSet<AttrKeyPath> {
    let covers = |reads: &BTreeSet<AttrKeyPath>, r: &AttrKeyPath| {
        reads.contains(r)
            || reads.contains(&AttrKeyPath {
                attr: r.attr.clone(),
                const_key: None,
            })
    };
    a.iter()
        .chain(b)
        .filter(|r| covers(a, r) && covers(b, r))
        .cloned()
        .collect()
}

/// The must-facts the function gets from the project calls it makes whenever it returns
/// normally ([`CallSite::made_on_every_return`]): an effect every target of a complete call has
/// run whenever it returns ([`site_must_chain`]).
fn must_through_calls(index: &ProjectIndex, sites: &[(PathBuf, usize)], s: &mut EffectSummary) {
    if s.must_exec.as_ref().is_some_and(|m| m.bound_twice) {
        return;
    }
    for (path, i) in sites {
        let site = &index.files[path].call_sites[*i];
        if site.project_callees().is_empty() || !site.made_on_every_return() {
            continue;
        }
        let kinds: BTreeSet<ExecEffectKind> = site
            .project_callees()
            .iter()
            .filter_map(|bc| index.summary(&bc.callee)?.must_exec.as_deref())
            .flat_map(|m| m.effects.keys().copied())
            .collect();
        for e in kinds {
            let Some(found) = site_must_chain(index, path, site, e) else {
                continue;
            };
            let must = s.must_exec.get_or_insert_with(Box::default);
            match must.effects.get(&e) {
                Some(kept) if kept.len() <= found.chain.len() => {}
                _ => {
                    must.effects.insert(e, found.chain);
                    // The claim names every call the implementations end in, not only the
                    // kept chain's.
                    if found.witnesses.len() > 1 {
                        must.alternatives.insert(e, found.witnesses);
                    } else {
                        must.alternatives.remove(&e);
                    }
                }
            }
        }
    }
}

/// Whether every implementation the call at `site` (in `path`) can run has run effect `e`
/// whenever the call returns ([`SiteMust`]): a Layer 2 call that has `e` whatever it does not
/// show, or a complete set of project targets whose bodies run at the call, each run as
/// written (its decorators call it each time), each having run `e` whenever it returns. An
/// abstract method never runs (an override does); a target that never returns normally has
/// run anything whenever it returns, and shows nothing.
pub(crate) fn site_must_chain(
    index: &ProjectIndex,
    path: &Path,
    site: &CallSite,
    e: ExecEffectKind,
) -> Option<SiteMust> {
    site_must_steps(index, path, site, e, &mut |_| ControlFlow::Break(()))
}

/// A step of a must-fact that does not hold: why a call has not been established to have run
/// an effect whenever it returns, or a call of a chain to have made the next one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum MustGap {
    /// No path reaches the call: it is behind a branch the code decides against.
    NotReached,
    /// The call never returns normally: every target is abstract or always raises.
    NeverReturns,
    /// The caller returns normally on a path on which it has not made the call
    /// ([`CallSite::made_on_every_return`]).
    NotOnEveryReturn,
    /// The targets may not be every implementation the call can run ([`CallSite::complete`]).
    IncompleteTargets,
    /// The call does not run a body there and then: it iterates or hands on what a call
    /// returns, an API runs the callable, or the target (`Some`) is a generator or an
    /// `async def`.
    NotRunAtCall(Option<FuncKey>),
    /// A decorator of the target may not run its body on every call.
    Decorated(FuncKey),
    /// The target's name is bound to more than one definition.
    DefinedTwice(FuncKey),
    /// The target has not run the effect whenever it returns.
    WithoutTheFact(FuncKey),
    /// The Layer 2 call does not have the effect on every call
    /// (`CallTarget::Layer2::unconditional`).
    Conditional,
}

/// A [`MustGap`] at the call of the hop `hop` of a reported chain.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HopGap {
    pub hop: usize,
    pub gap: MustGap,
}

/// The steps [`site_must_steps`] found open, told to `gaps`: a decision stops at the first one
/// (`ControlFlow::Break`), an explanation goes on through the others.
struct Steps<'g, G> {
    gaps: &'g mut G,
    open: bool,
}

impl<G: FnMut(MustGap) -> ControlFlow<()>> Steps<'_, G> {
    /// Records `gap`; true when the decision stops at it.
    fn stops_at(&mut self, gap: MustGap) -> bool {
        self.open = true;
        (self.gaps)(gap).is_break()
    }
}

/// The decision of [`site_must_chain`], telling each step that does not hold to `gaps`. Every
/// step that makes the decision fail is told, so the result is `None` exactly when `gaps` heard
/// of one. Past a call that runs no body at the call, the targets' facts are not read: they are
/// not what runs there.
pub(crate) fn site_must_steps(
    index: &ProjectIndex,
    path: &Path,
    site: &CallSite,
    e: ExecEffectKind,
    gaps: &mut impl FnMut(MustGap) -> ControlFlow<()>,
) -> Option<SiteMust> {
    let mut steps = Steps { gaps, open: false };
    if !site.runs_targets_here() {
        steps.stops_at(MustGap::NotRunAtCall(None));
        return None;
    }
    if !site.complete && steps.stops_at(MustGap::IncompleteTargets) {
        return None;
    }
    match &site.target {
        CallTarget::Layer2 {
            symbol,
            effects,
            unconditional,
            ..
        } => {
            // The mask is computed only for a complete call: of another, it tells nothing.
            let has_it = effects.contains(&e) && unconditional & crate::effect_bit(e) != 0;
            if site.complete && !has_it && steps.stops_at(MustGap::Conditional) {
                return None;
            }
            (!steps.open).then(|| {
                let hop = Arc::new(site_hop(site, path, symbol.clone()));
                SiteMust {
                    chain: vec![Arc::clone(&hop)],
                    witnesses: vec![hop],
                }
            })
        }
        CallTarget::Project(callees) => {
            let mut out = SiteMust::default();
            for bc in callees {
                let key = || bc.callee.clone();
                let callee = index.summary(&bc.callee);
                // An abstract method never runs.
                if callee.is_some_and(EffectSummary::is_abstract) {
                    continue;
                }
                // What the body has run is not what runs at a call that does not run it.
                if callee.is_some_and(|c| !runs_at_call(c, site)) {
                    if steps.stops_at(MustGap::NotRunAtCall(Some(key()))) {
                        return None;
                    }
                    continue;
                }
                if callee.is_some_and(|c| !crate::decorators::keeps_returned_value(&c.decorators))
                    && steps.stops_at(MustGap::Decorated(key()))
                {
                    return None;
                }
                // A summary without facts has none.
                let must = callee.and_then(|c| c.must_exec.as_deref());
                let Some(chain) = must.and_then(|m| m.chain(e)) else {
                    let gap = if must.is_some_and(|m| m.bound_twice) {
                        MustGap::DefinedTwice(key())
                    } else {
                        MustGap::WithoutTheFact(key())
                    };
                    if steps.stops_at(gap) {
                        return None;
                    }
                    continue;
                };
                let Some(sink) = chain.last() else {
                    continue;
                };
                // A callee whose own implementations end in different calls has each of them.
                let ends = must
                    .and_then(|m| m.alternatives.get(&e))
                    .map_or(std::slice::from_ref(sink), Vec::as_slice);
                for end in ends {
                    if !out.witnesses.contains(end) {
                        out.witnesses.push(Arc::clone(end));
                    }
                }
                if out.chain.is_empty() || chain.len() + 1 < out.chain.len() {
                    // The callee's hops are shared, not copied: a chain through every caller up
                    // a call chain adds one hop per caller.
                    out.chain = std::iter::once(Arc::new(site_hop(site, path, bc.callee.fqn())))
                        .chain(chain.iter().cloned())
                        .collect();
                }
            }
            if !steps.open && out.chain.is_empty() && steps.stops_at(MustGap::NeverReturns) {
                return None;
            }
            (!steps.open).then_some(out)
        }
    }
}

/// Why must-facts do not hold, for the reports of one run: the call site each chain hop names,
/// found in a table built once, and each call's own gaps ([`site_must_steps`]) decided once and
/// shared by every report whose chain passes it. A report's walk visits only the hops of its
/// own chain, so the work grows with the chains the reports show.
pub struct MustExplainer<'i> {
    index: &'i ProjectIndex,
    sites: FxHashMap<(&'i Path, &'i FuncKey, Span), Vec<&'i CallSite>>,
    gaps: Mutex<SiteGaps>,
}

/// The gaps of each call site (by its address) for an effect ([`MustExplainer::site_gaps`]).
type SiteGaps = FxHashMap<(usize, ExecEffectKind), Arc<[MustGap]>>;

impl<'i> MustExplainer<'i> {
    pub fn new(index: &'i ProjectIndex) -> Self {
        let mut sites: FxHashMap<(&Path, &FuncKey, Span), Vec<&CallSite>> = FxHashMap::default();
        for (path, file) in &index.files {
            for site in &file.call_sites {
                sites
                    .entry((path.as_path(), &site.caller, site.span))
                    .or_default()
                    .push(site);
            }
        }
        Self {
            index,
            sites,
            gaps: Mutex::default(),
        }
    }

    /// The call site a chain's hop stands for: the call of its callee its caller makes there.
    pub fn site_of(&self, hop: &CallHop) -> Option<&'i CallSite> {
        let calls = |site: &&CallSite| match &site.target {
            CallTarget::Layer2 { symbol, .. } => *symbol == hop.callee,
            CallTarget::Project(callees) => callees.iter().any(|bc| bc.callee.fqn() == hop.callee),
        };
        self.sites
            .get(&(hop.path.as_path(), &hop.caller, hop.span))?
            .iter()
            .copied()
            .find(calls)
    }

    /// The gaps of [`site_must_steps`] at `site` for `e`, each told once.
    fn site_gaps(&self, path: &Path, site: &CallSite, e: ExecEffectKind) -> Arc<[MustGap]> {
        // The index is borrowed for the explainer's life, so a site's address names it.
        let key = (std::ptr::from_ref(site) as usize, e);
        let kept = self
            .gaps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned();
        kept.unwrap_or_else(|| {
            let mut gaps = Vec::new();
            site_must_steps(self.index, path, site, e, &mut |gap| {
                gaps.push(gap);
                ControlFlow::Continue(())
            });
            let gaps: Arc<[MustGap]> = gaps.into();
            self.gaps
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key, Arc::clone(&gaps));
            gaps
        })
    }

    /// Why the call at `site` (in `path`) has not been established to have run `e` whenever it
    /// returns, along `chain`, the chain a report shows from it: the gaps of each call on the
    /// chain, the first one's from [`site_must_steps`] and the others' also from their callers'
    /// returns ([`CallSite::made_on_every_return`]). The walk goes down while the function the
    /// chain enters has not run `e` whenever it returns, and that function's own gap gives way
    /// to the gaps found below it. Empty when the call has the fact.
    pub fn gaps_along(
        &self,
        path: &Path,
        site: &CallSite,
        e: ExecEffectKind,
        chain: &[CallHop],
    ) -> Vec<HopGap> {
        let mut out: Vec<HopGap> = Vec::new();
        // Per hop, the gap of the function the chain enters there, which the gaps below replace.
        let mut entered_gaps: Vec<(usize, usize)> = Vec::new();
        let mut at = Some((path, site));
        for hop in 0..chain.len() {
            let Some((path, site)) = at.take() else {
                break;
            };
            if hop > 0 && !site.made_on_every_return() {
                out.push(HopGap {
                    hop,
                    gap: MustGap::NotOnEveryReturn,
                });
            }
            let entered = chain.get(hop + 1).map(|h| &h.caller);
            for gap in self.site_gaps(path, site, e).iter() {
                if matches!(gap, MustGap::WithoutTheFact(k) if Some(k) == entered) {
                    entered_gaps.push((hop, out.len()));
                }
                out.push(HopGap {
                    hop,
                    gap: gap.clone(),
                });
            }
            let Some(next) = chain.get(hop + 1) else {
                break;
            };
            let entered_has_it = self
                .index
                .summary(&next.caller)
                .and_then(|c| c.must_exec.as_deref()?.chain(e))
                .is_some();
            if !entered_has_it {
                at = self.site_of(next).map(|s| (next.path.as_path(), s));
            }
        }
        let deepest = out.last().map_or(0, |g| g.hop);
        let replaced: FxHashSet<usize> = entered_gaps
            .into_iter()
            .filter(|&(hop, _)| deepest > hop)
            .map(|(_, i)| i)
            .collect();
        out.into_iter()
            .enumerate()
            .filter(|(i, _)| !replaced.contains(i))
            .map(|(_, g)| g)
            .collect()
    }
}

/// What a call has run whenever it returns ([`site_must_chain`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct SiteMust {
    /// The shortest chain from the call to the effect: the call's own hop, then the hops of the
    /// callee's kept chain, shared with its summary.
    pub chain: SharedChain,
    /// Per implementation that returns normally, the call that has the effect at the end of its
    /// chain, where that call is (each call once).
    pub witnesses: SharedChain,
}

impl SiteMust {
    /// The functions called with the effect at the end of the chains, each once.
    pub fn sinks(&self) -> BTreeSet<&str> {
        self.witnesses.iter().map(|h| h.callee.as_str()).collect()
    }

    /// What a report claims the call runs: each of [`Self::sinks`].
    pub fn target(&self) -> String {
        self.sinks().into_iter().collect::<Vec<_>>().join(", ")
    }

    /// The related locations of a report: the shortest chain, then each witness off it.
    pub fn related(&self) -> Vec<CallHop> {
        let off_chain = self.witnesses.iter().filter(|w| !self.chain.contains(w));
        self.chain
            .iter()
            .chain(off_chain)
            .map(|h| CallHop::clone(h))
            .collect()
    }
}

/// Takes from [`EffectSummary::returns_self_attrs`] of a cached property the attributes its
/// `self` calls whose results it discards read, when such a call writes an attribute that
/// reaches the cached value: one the getter reads itself or through a call reaching the value
/// (the attributes it returns included), so the call may have derived it from what it reads.
fn drop_attrs_read_by_discarded_calls<'s>(
    index: &'s ProjectIndex,
    self_calls: &[(&'s CallSite, &'s EffectSummary)],
    local: &'s EffectSummary,
    s: &mut EffectSummary,
) {
    let mut reaching: BTreeSet<&str> = local
        .reads_self_attrs
        .iter()
        .map(|r| r.attr.as_str())
        .collect();
    // `(reads, writes)` of each callee of a discarded `self` call.
    let mut discarded: Vec<(BTreeSet<&str>, BTreeSet<&str>)> = Vec::new();
    for &(site, callee) in self_calls {
        // What runs in the getter's body (a generator method's body does not run at the
        // call), as for the edge itself (`transfer`).
        let delivered = delivery(|k| index.summary(k), site, callee)
            .filter(|d| d.into == Facts::Body)
            .and_then(|d| callee.facts(d.from));
        let Some(facts) = delivered.filter(|_| callee.owner_class_qualname.is_some()) else {
            continue;
        };
        let reads = facts.reads_self_attrs.iter().map(|r| r.attr.as_str());
        if site.result_discarded {
            let writes = facts.mutates_self_attrs.iter().map(|w| w.attr.as_str());
            discarded.push((reads.collect(), writes.collect()));
        } else {
            reaching.extend(reads);
        }
    }
    let mut dropped: BTreeSet<&str> = BTreeSet::new();
    loop {
        let (reach, inert): (Vec<_>, Vec<_>) = std::mem::take(&mut discarded)
            .into_iter()
            .partition(|(_, writes)| !writes.is_disjoint(&reaching));
        discarded = inert;
        if reach.is_empty() {
            break;
        }
        for (reads, _) in reach {
            reaching.extend(reads.iter().copied());
            dropped.extend(reads);
        }
    }
    s.returns_self_attrs
        .retain(|a| !dropped.contains(a.as_str()));
}

/// Resolves the construction conditions of a function's local facts (`construction`) against
/// the final summaries of its callees. A callee of the SCC being computed has no final
/// `returns_new` yet, so a condition on one fails: the fixpoint stays monotone.
struct Resolver<'i> {
    index: &'i ProjectIndex,
    in_scc: &'i dyn Fn(&FuncKey) -> bool,
}

impl Resolver<'_> {
    /// Whether `cond` holds (for `own`, the new object must own its containers), and the
    /// least kind of the objects its project functions return.
    fn holds(&self, cond: &NewCond, own: bool) -> Option<NewKind> {
        let mut kind = NewKind::Own;
        for k in cond.returns() {
            if (self.in_scc)(k) {
                return None;
            }
            let r = self.index.summary(k)?.returns_new.kind()?;
            if own && r == NewKind::Copy {
                return None;
            }
            kind = kind.min(r);
        }
        for (k, p) in cond.keeps() {
            let s = self.index.summary(k)?;
            if *p >= 64 || s.retains_params & (1u64 << p) != 0 {
                return None;
            }
        }
        Some(kind)
    }

    /// The construction, in the caller, of a write a callee makes to the object bound to its
    /// parameter, given that argument (`own`: an in-place change).
    fn through_arg(&self, arg: Option<&ArgOrigin>, own: bool) -> Construction {
        match arg.map(|o| &o.new_object) {
            Some(NewArg::New { kind, cond })
                if !(own && *kind == NewKind::Copy) && self.holds(cond, own).is_some() =>
            {
                Construction::Yes
            }
            Some(NewArg::Param { param, cond }) if self.holds(cond, own).is_some() => {
                Construction::Param {
                    param: *param,
                    own,
                    cond: NewCond::default(),
                }
            }
            _ => Construction::No,
        }
    }

    /// The final fact of `func` ([`crate::fresh::FreshReturns::resolved`]): its local fact when
    /// each project function it names returns a new container of the same kind on every return
    /// (by its final fact, outside the SCC being computed).
    fn resolve_fresh_return(&self, func: &FuncKey) -> Option<FreshReturn> {
        let file = &self.index.files[&self.index.func_files[func]];
        let local = file.opt_in_tables.as_ref()?.fresh_returns.local.get(func)?;
        let mut fact = FreshReturn {
            calls: Vec::new(),
            ..*local
        };
        local
            .calls
            .iter()
            .all(|k| {
                let callee = self.index.fresh_return(k).filter(|_| !(self.in_scc)(k));
                fact.join_callee(callee)
            })
            .then_some(fact)
    }

    /// Resolve the conditions of the function's own writes and of its returned object.
    fn resolve_own(&self, s: &mut EffectSummary) {
        let pending: Vec<TypedFieldWriteSite> = s
            .typed_field_write_sites
            .iter()
            .filter(|t| match &t.construction {
                Construction::Pending { .. } => true,
                Construction::Param { cond, .. } => !cond.is_empty(),
                _ => false,
            })
            .cloned()
            .collect();
        let unresolved: Vec<UnresolvedWriteSite> = s
            .unresolved_write_sites
            .iter()
            .filter(|t| matches!(t.construction, Construction::Pending { .. }))
            .cloned()
            .collect();
        for mut t in unresolved {
            s.unresolved_write_sites.remove(&t);
            t.construction = match &t.construction {
                Construction::Pending { cond, own } if self.holds(cond, *own).is_some() => {
                    Construction::Yes
                }
                _ => Construction::No,
            };
            s.unresolved_write_sites.insert(t);
        }
        for mut t in pending {
            s.typed_field_write_sites.remove(&t);
            t.construction = match &t.construction {
                Construction::Pending { cond, own } if self.holds(cond, *own).is_some() => {
                    Construction::Yes
                }
                Construction::Param { param, own, cond } if self.holds(cond, *own).is_some() => {
                    Construction::Param {
                        param: *param,
                        own: *own,
                        cond: NewCond::default(),
                    }
                }
                _ => Construction::No,
            };
            s.typed_field_write_sites.insert(t);
        }
        if let ReturnsNew::New { kind, cond } = &s.returns_new
            && !cond.is_empty()
        {
            s.returns_new = match self.holds(cond, false) {
                Some(k) => ReturnsNew::New {
                    kind: (*kind).min(k),
                    cond: NewCond::default(),
                },
                None => ReturnsNew::No,
            };
        }
    }
}

/// `MUT202` return flow across one call whose result the caller may return: a callee whose
/// result is shared (an `@lru_cache` function, a declared producer, or a function that returns
/// one's value) makes the caller return a shared value too.
fn return_flow_edge(
    s: &mut EffectSummary,
    contracts: Option<&dyn crate::Contracts>,
    caller: &FuncKey,
    site: &CallSite,
    bc: &BoundCallee,
    callee: &EffectSummary,
    path: &Path,
) {
    let Some(step) = site.returned else {
        return;
    };
    if !runs_at_call(callee, site) {
        return;
    }
    let fqn = bc.callee.fqn();
    let (producer, declared, produced, tail) = if callee.is_lru_cache {
        (fqn, false, Shared::produced(true), Vec::new())
    } else if let Some(deep) =
        contracts.and_then(|c| c.readonly_producer(&fqn, &bc.callee.qualname))
    {
        (fqn, true, Shared::produced(deep), Vec::new())
    } else if let Some(r) = &callee.returns_protected {
        (r.producer.clone(), r.declared, r.shared, r.chain.clone())
    } else {
        return;
    };
    let mut chain = vec![Arc::new(CallHop {
        caller: caller.clone(),
        callee: bc.callee.fqn(),
        span: site.span,
        path: path.to_path_buf(),
    })];
    chain.extend(tail);
    ProtectedReturn::add(
        &mut s.returns_protected,
        producer,
        declared,
        produced,
        step,
        chain,
    );
}

/// `MUT112` return flow: what the project calls whose results the function may return
/// (`EffectSummary::return_origins`) return, with the arguments bound to the parameters each
/// callee returns in their place, through the calls whose results those arguments may be in
/// turn, and through the phis the function's origin walks met (`ReturnOrigins::nodes`).
/// `sites` are the function's call sites.
///
/// Each call result and each phi is resolved once: what it is of its own (for a call, what its
/// callees return of their own and of the arguments' known origins), joined with what the call
/// results and phis it depends on resolve to, to a fixpoint over those dependencies (a loop makes
/// a call or a phi depend on itself). The fixpoint only joins, and each change adds a kind or a
/// parameter to one value, so it ends after a bounded number of changes per value.
fn returned_results(index: &ProjectIndex, sites: &[(PathBuf, usize)], s: &mut EffectSummary) {
    let Some(facts) = s.return_origins.as_deref_mut() else {
        return;
    };
    let at: FxHashMap<Span, &CallSite> = sites
        .iter()
        .map(|(path, i)| &index.files[path].call_sites[*i])
        .filter(|site| site.role == SiteRole::Call && site.runner == Runner::Caller)
        .filter(|site| !site.project_callees().is_empty())
        .map(|site| (site.span, site))
        .collect();
    // The calls and phis, by position: what each resolves to so far, and what it depends on,
    // each with whether it reaches it as an element.
    let mut index_of: FxHashMap<Resolved, usize> = FxHashMap::default();
    let mut value: Vec<Origins> = Vec::new();
    let mut needs: Vec<Vec<(Resolved, bool)>> = Vec::new();
    let mut pending: Vec<Resolved> = facts
        .returned
        .results
        .iter()
        .map(|r| Resolved::Call(r.span))
        .chain(facts.returned.nodes.iter().map(|n| Resolved::Node(*n)))
        .collect();
    while let Some(item) = pending.pop() {
        if index_of.contains_key(&item) {
            continue;
        }
        let mut own = Origins::default();
        let mut deps: Vec<(Resolved, bool)> = Vec::new();
        // The dependencies of origins an argument or a phi has, reached as an element or not.
        let depend = |local: &LocalOrigins, element: bool, deps: &mut Vec<(Resolved, bool)>| {
            deps.extend(
                local
                    .results
                    .iter()
                    .map(|r| (Resolved::Call(r.span), element || r.element)),
            );
            deps.extend(local.nodes.iter().map(|n| (Resolved::Node(*n), element)));
        };
        match item {
            Resolved::Call(span) => {
                let callees = at.get(&span).into_iter().flat_map(|site| {
                    site.project_callees()
                        .iter()
                        .filter_map(|bc| Some((bc, index.summary(&bc.callee)?)))
                        .filter(|(_, callee)| runs_at_call(callee, site))
                });
                for (bc, callee) in callees {
                    let returned = callee.return_origin_facts().returned.known;
                    own.join(Origins {
                        params: 0,
                        elements: 0,
                        ..returned
                    });
                    for (j, arg) in bc.params.iter().enumerate().take(64) {
                        let (itself, element) = (
                            returned.params & (1u64 << j) != 0,
                            returned.elements & (1u64 << j) != 0,
                        );
                        let Some(local) = arg.origins.as_deref().filter(|_| itself || element)
                        else {
                            continue;
                        };
                        if itself {
                            own.join(local.known);
                            depend(local, false, &mut deps);
                        }
                        if element {
                            own.join(local.known.element());
                            depend(local, true, &mut deps);
                        }
                    }
                }
            }
            Resolved::Node(n) => {
                let local = &facts.nodes[n as usize];
                own.join(local.known);
                depend(local, false, &mut deps);
            }
        }
        pending.extend(deps.iter().map(|(dep, _)| *dep));
        index_of.insert(item, value.len());
        value.push(own);
        needs.push(deps);
    }
    // What each value's resolution reaches, with whether as an element.
    let mut users: Vec<Vec<(usize, bool)>> = vec![Vec::new(); value.len()];
    for (user, deps) in needs.iter().enumerate() {
        for (dep, element) in deps
            .iter()
            .filter_map(|(dep, e)| Some((*index_of.get(dep)?, *e)))
        {
            users[dep].push((user, element));
        }
    }
    let mut work: Vec<usize> = (0..value.len()).collect();
    while let Some(dep) = work.pop() {
        for &(user, element) in &users[dep] {
            let add = if element {
                value[dep].element()
            } else {
                value[dep]
            };
            let before = value[user];
            value[user].join(add);
            if value[user] != before {
                work.push(user);
            }
        }
    }
    let resolved: Vec<Origins> = facts
        .returned
        .results
        .iter()
        .map(|r| (Resolved::Call(r.span), r.element))
        .chain(
            facts
                .returned
                .nodes
                .iter()
                .map(|n| (Resolved::Node(*n), false)),
        )
        .filter_map(|(item, element)| {
            let v = value[*index_of.get(&item)?];
            Some(if element { v.element() } else { v })
        })
        .collect();
    for o in resolved {
        facts.returned.known.join(o);
    }
}

/// A value [`returned_results`] resolves: a project call's result (by its span) or a phi (by its
/// position in `ReturnOrigins::nodes`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Resolved {
    Call(Span),
    Node(u32),
}

/// One call of a project callee: the call site, the callee it binds, its summary, and the
/// facts of it the site runs.
#[derive(Clone, Copy)]
struct Edge<'e> {
    site: &'e CallSite,
    bc: &'e BoundCallee,
    callee: &'e EffectSummary,
    facts: &'e EffectSummary,
}

/// Transfer the facts of one resolved callee across one call site into the caller: the facts
/// the site runs ([`delivery`]), into the caller's body or into what iterating the caller's
/// result runs. The caller's local summary is `local`. Returns the
/// caller's parameters the callee consumes at the call (`MUT106`, a must-fact the caller
/// takes only when every candidate of the call consumes them).
#[allow(clippy::too_many_arguments)]
fn apply_edge(
    index: &ProjectIndex,
    s: &mut EffectSummary,
    local: &EffectSummary,
    caller: &FuncKey,
    site: &CallSite,
    bc: &BoundCallee,
    callee: &EffectSummary,
    path: &Path,
    resolver: &Resolver<'_>,
) -> u64 {
    // A callee keeping a parameter keeps the caller's objects bound to it; a generator or a
    // coroutine created without running keeps every argument.
    let kept = if runs_at_call(callee, site) {
        callee.retains_params
    } else {
        u64::MAX
    };
    s.retains_params |= bound_params(kept, bc);
    let Some(d) = delivery(|k| index.summary(k), site, callee) else {
        return 0;
    };
    let Some(facts) = callee.facts(d.from) else {
        return 0;
    };
    let lead = lead_hops(site, path, &bc.callee, d.via);
    let rewrite = rewrites(callee, d.from);
    // Single-pass consumption (`MUT106`): a callee iterating its parameter k to exhaustion
    // consumes the caller's parameter passed as argument k when the callee's body runs at the
    // call, provided every call receiving that parameter runs on each normal path of the caller.
    let consumed = if d.from == Facts::Body && d.into == Facts::Body {
        caller_params(facts.consumes_params, bc)
    } else {
        0
    };
    let target = match d.into {
        Facts::Body => s,
        Facts::Result => s.iterated.get_or_insert_with(Box::default),
    };
    // Only facts that run during the call, in the caller's body, can construct the objects the
    // call hands over.
    let here = (d.from == Facts::Body && d.into == Facts::Body && runs_at_call(callee, site))
        .then_some(resolver);
    let edge = Edge {
        site,
        bc,
        callee,
        facts,
    };
    transfer(index, target, local, &edge, &lead, rewrite);
    target.join_removes_kept(facts.removes_kept());
    if !index.skips_typed_writes
        && (!facts.typed_field_write_sites.is_empty() || !facts.mutated_param_paths.is_empty())
    {
        // The functions the chains gain at this call: the callee, and the one iterating it.
        let contexts = index.contracts.as_deref().map_or(0, |c| {
            std::iter::once(&bc.callee)
                .chain(d.via)
                .fold(0, |acc, k| acc | c.writer_contexts(k))
        });
        transfer_typed_writes(
            target, caller, site, bc, callee, facts, &lead, rewrite, here, contexts,
        );
    }
    consumed
}

/// Add the closure cells the callee's `facts` change to `t` (of the caller whose local summary
/// is `local`), except the caller's own variables whose change stays inside its call: one the
/// callee rebinds, or one the caller binds only to new containers.
fn add_cell_changes(
    t: &mut EffectSummary,
    local: &EffectSummary,
    site: &CallSite,
    bc: &BoundCallee,
    facts: &EffectSummary,
) {
    let leaving: Vec<&CellChange> = facts
        .changes_cells()
        .iter()
        .filter(|cell| {
            let own = cell.owner == site.caller.qualname
                && bc.callee.module == site.caller.module
                && (cell.rebinds || local.new_cells().contains(&cell.name));
            !own
        })
        .collect();
    if !leaving.is_empty() {
        t.change_shared_state(|f| f.cells.extend(leaving.into_iter().cloned()));
    }
}

/// Add to `t` the shared state the caller (whose local summary is `local`) hands to the callee
/// `bc` at `site` when the callee's `facts` change that argument (`helper(CACHE)`, a method of a
/// module-level object changing `self`).
fn add_handed_changes(
    t: &mut EffectSummary,
    local: &EffectSummary,
    site: &CallSite,
    bc: &BoundCallee,
    facts: &EffectSummary,
) {
    if local.shared_state.is_none() {
        return;
    }
    let ci = site
        .project_callees()
        .iter()
        .position(|b| std::ptr::eq(b, bc))
        .expect("an edge's callee is one of its call site's callees");
    let changed: Vec<&SharedState> = local
        .handed_at(site.span, ci)
        .filter(|h| facts.changes_argument(h.param))
        .map(|h| &h.state)
        .collect();
    if changed.is_empty() {
        return;
    }
    t.change_shared_state(|f| {
        for state in changed {
            match state {
                SharedState::Global(_) => f.globals = true,
                SharedState::Cell(cell) => f.cells.push(cell.clone()),
            }
        }
    });
}

/// Add the facts of `edge` to `t`, a summary of the caller whose local summary is `local`;
/// `lead` and `rewrite` build each chain ([`extend_chain`]).
fn transfer(
    index: &ProjectIndex,
    t: &mut EffectSummary,
    local: &EffectSummary,
    edge: &Edge<'_>,
    lead: &[Arc<CallHop>],
    rewrite: bool,
) {
    let Edge {
        site,
        bc,
        callee,
        facts,
    } = *edge;
    let origin = |k: usize| bc.params.get(k);

    // A recursion raises on the stack it runs on: a worker thread's is not the caller's.
    if facts.call_cycle.is_some() && site.runner != Runner::Worker {
        t.call_cycle = t.call_cycle.max(Some(CallCycle::Reaches));
    }

    // a. Offload wrappers: a callee offloading its parameter k offloads the caller's
    //    parameter p when argument k is p. The offloaded callable is never called here,
    //    so its blocking effects never reach the caller.
    t.offloads_blocking_params |= caller_params(facts.offloads_blocking_params, bc);

    // Shared state that outlives the call.
    if facts.changes_globals() && !t.changes_globals() {
        t.change_shared_state(|f| f.globals = true);
    }
    add_cell_changes(t, local, site, bc, facts);
    add_handed_changes(t, local, site, bc, facts);

    // b. Execution effects, with the shortest chain to each. An effect whose guard the call
    //    decides against does not run here; the others take the call's guards and the
    //    callee's undecided tests of caller inputs (except of an attribute the caller assigns
    //    itself). On a receiver of known class, the effects only implementations of unrelated
    //    classes reach do not run.
    let receiver = site.receiver();
    for e in facts
        .direct_exec_effects
        .iter()
        .chain(&facts.transitive_exec_effects)
    {
        // Work run on a worker thread does not block the caller's thread.
        if site.hides(*e) {
            continue;
        }
        let Some(tail) = facts.effect_chain_for(*e, receiver) else {
            continue;
        };
        let Some(mut through) =
            guards::through_call(site, bc, facts.exec_guards.get(e), &index.param_capacities)
        else {
            continue;
        };
        through.guards.inputs.retain(|g| {
            g.subject.attr.as_ref().is_none_or(|a| {
                !local
                    .mutated_param_paths
                    .get(&g.subject.param)
                    .is_some_and(|paths| paths.contains(a))
            })
        });
        let new = t.transitive_exec_effects.insert(*e);
        let had = !new || t.direct_exec_effects.contains(e);
        guards::add_occurrence(&mut t.exec_guards, *e, had, &through.guards);
        let chain = extend_chain(lead, tail, rewrite);
        let kept = add_receiver_effect(t, *e, had, site, bc, facts, &chain, lead, rewrite);
        let slot = t.exec_chains.entry(*e).or_default();
        let replaces = match kept {
            KeptChain::Replaced => true,
            KeptChain::Shorter => slot.is_empty() || chain.len() < slot.len(),
            KeptChain::Kept => false,
        };
        if replaces {
            *slot = chain;
        }
    }

    // c. Mutation & state effects.
    if site.recv_is_self && callee.owner_class_qualname.is_some() {
        // A `self` method or property reading an attribute reads it for more than its object;
        // one whose result is discarded is decided with the others (`recompute`).
        if !t.returns_self_attrs.is_empty() && !site.result_discarded {
            t.returns_self_attrs
                .retain(|a| !facts.reads_self_attrs.iter().any(|r| r.attr == *a));
        }
        t.add_self_reads(facts);
        t.mutates_self_attrs
            .extend(facts.mutates_self_attrs.iter().cloned());
        t.invalidates_cached
            .extend(facts.invalidates_cached.iter().cloned());
    }
    let changes = iterations::call_changes(site, bc, callee, facts);
    if !changes.is_empty() {
        iterations::merge_changes(
            &mut t.iteration_facts_mut().changes_self_containers,
            changes,
        );
    }
    // A callee parameter bound to a caller parameter itself changes that parameter's container.
    let param_changes: Vec<(u8, iterations::ContainerChange)> = facts
        .iteration_facts()
        .changes_param_containers
        .iter()
        .filter_map(|(&i, &change)| match origin(usize::from(i))?.param {
            Some((p, None)) => Some((p, change)),
            _ => None,
        })
        .collect();
    if !param_changes.is_empty() {
        iterations::merge_changes(
            &mut t.iteration_facts_mut().changes_param_containers,
            param_changes,
        );
    }
    // A slot of a callee parameter bound to a caller parameter is that parameter's slot.
    for (&i, slots) in &facts.inplace_param_slots {
        if let Some(ArgOrigin {
            param: Some((p, None)),
            ..
        }) = origin(usize::from(i))
        {
            t.inplace_param_slots
                .entry(*p)
                .or_default()
                .extend(slots.iter().cloned());
        }
    }
    for (&i, paths) in &facts.mutated_param_paths {
        let Some(o) = origin(usize::from(i)) else {
            continue;
        };
        // A callee changing its argument in place changes the slot `p.a` / `p["k"]` it is.
        if paths.contains("")
            && let Some((p, slot)) = &o.param_slot
        {
            t.inplace_param_slots
                .entry(*p)
                .or_default()
                .insert(slot.clone());
        }
        let memos = facts.memo_param_paths.as_deref().and_then(|m| m.get(&i));
        let memo = |path: &String| memos.is_some_and(|m| m.contains(path));
        match &o.param {
            Some((p, None)) => {
                for path in paths {
                    t.note_param_change(*p, path.clone(), memo(path));
                }
                if let Some(attrs) = facts.changed_param_attrs.get(&i) {
                    t.changed_param_attrs
                        .entry(*p)
                        .or_default()
                        .extend(attrs.iter().cloned());
                }
            }
            // The callee's parameter is the caller's `p.attr`: any change of it changes the
            // object that attribute holds, which fills a memo when the callee only fills one or
            // when the attribute itself is named as a memo (`self._cache.set(k, v)`).
            Some((p, Some(attr))) => {
                let into_memo = matches!(&o.param_slot, Some((q, Slot::Attr(a)))
                    if q == p && a == attr && cache_flow::is_memo_name(a));
                t.note_param_change(*p, attr.clone(), into_memo || paths.iter().all(memo));
                t.changed_param_attrs
                    .entry(*p)
                    .or_default()
                    .insert(attr.clone());
            }
            None => {}
        }
        // The callee's parameter is an object the caller's parameter holds as an item or an
        // element, or one below it: any change of it is a change below that parameter.
        for (p, path) in &o.param_elements {
            t.note_param_change(*p, path.clone(), paths.iter().all(memo));
            t.changed_param_attrs
                .entry(*p)
                .or_default()
                .insert(path.clone());
        }
        // A shallow copy of the caller's parameter shares exactly the objects below its top
        // level with it.
        if let Some((p, copied)) = &o.param_copy
            && let Some(attrs) = facts.changed_param_attrs.get(&i)
        {
            for attr in attrs {
                let path = copied.clone().unwrap_or_else(|| attr.clone());
                t.note_param_change(*p, path.clone(), memo(attr));
                t.changed_param_attrs.entry(*p).or_default().insert(path);
            }
        }
        if let Some(attr) = &o.self_attr {
            t.mutates_self_attrs.insert(AttrKeyPath {
                attr: attr.clone(),
                const_key: None,
            });
        }
    }
    t.escapes_params |= caller_params(facts.escapes_params, bc);
    // Returning the callee's reallocation of its argument reallocates the caller's parameter.
    if site.returned.is_some() {
        t.reallocates_params |= caller_params(facts.reallocates_params, bc);
    }
    // A generator passed to a callee that iterates it runs there.
    t.iterates_params |= caller_params(facts.iterates_params, bc);

    // d. The locks the callee takes on the caller's thread or task, in the caller's names,
    //    behind the tests of state on every path to the call too; and the names it may bind.
    if let Some(callee) = facts.locks.as_deref() {
        for (lock, acq) in &callee.acquires {
            if let Some((lock, mut acq)) = crate::locks::through_call(
                site,
                bc,
                lock,
                acq,
                lead,
                rewrite,
                &local.mutated_param_paths,
                &index.param_capacities,
            ) {
                if let Some(tests) = &site.state_tests {
                    crate::locks::add_owner_tests(&mut acq, &lock, tests);
                }
                crate::locks::add_acquisition(&mut t.lock_facts_mut().acquires, lock, acq);
            }
        }
        let rebinds: Vec<_> = (callee.rebinds.iter())
            .filter_map(|name| name.through(bc))
            .collect();
        if !rebinds.is_empty() {
            t.lock_facts_mut().rebinds.extend(rebinds);
        }
    }

    // e. The objects the callee uses one operation at a time, in the caller's names.
    for (obj, u) in facts.exclusive_uses() {
        if let Some((obj, u)) = crate::one_at_a_time::through_call(site, bc, obj, u, lead, rewrite)
        {
            crate::one_at_a_time::add_use(t.exclusive_uses_mut(), obj, u);
        }
    }
}

/// The typed writes a call hands its caller: an `IndirectHelper` site for each typed field or
/// instance the callee changes without seeing its type, the callee's write facts, and (from a
/// private callee, or for what iterating the callee's result runs) its write sites. `here`
/// resolves the construction of the writes of facts that run during the call (`None`: they
/// construct nothing).
#[allow(clippy::too_many_arguments)]
fn transfer_typed_writes(
    t: &mut EffectSummary,
    caller: &FuncKey,
    site: &CallSite,
    bc: &BoundCallee,
    callee: &EffectSummary,
    facts: &EffectSummary,
    lead: &[Arc<CallHop>],
    rewrite: bool,
    here: Option<&Resolver<'_>>,
    contexts: u64,
) {
    let origin = |k: usize| bc.params.get(k);
    for w in indirect_writes(bc, callee, facts) {
        // A new object the caller hands over and the helper does not keep is under
        // construction.
        let construction = match (w.through, here) {
            (Through::Instance, Some(r)) if facts.retains_params & (1u64 << w.param) == 0 => {
                r.through_arg(Some(w.origin), true)
            }
            _ => Construction::No,
        };
        insert_site(
            t,
            caller,
            TypedFieldWriteSite {
                class_qualname: w.class.to_owned(),
                attr: w.attr.to_owned(),
                kind: WriteKind::IndirectHelper,
                span: site.span,
                via_chain: lead.to_vec(),
                obj_params: w.obj_params,
                fresh: w.origin.fresh,
                key: None,
                construction,
                contexts,
                through_protocol: false,
            },
        );
    }
    // Typed writes: every callee's facts reach the caller. A private callee hands its write
    // sites to its callers too, a public one reports its own; what iterating a plain function's
    // result runs (`rewrite`) runs nowhere in the function, so its sites go to whoever iterates.
    for w in &facts.typed_field_writes {
        if !t.typed_field_writes.contains(w) {
            t.typed_field_writes.insert(w.clone());
        }
    }
    if !rewrite && !crate::delegates_to_callers(&bc.callee.qualname) {
        return;
    }
    for w in &facts.typed_field_write_sites {
        let site = TypedFieldWriteSite {
            class_qualname: w.class_qualname.clone(),
            attr: w.attr.clone(),
            kind: w.kind,
            span: w.span,
            via_chain: extend_chain(lead, &w.via_chain, rewrite),
            obj_params: bound_params(w.obj_params, bc),
            fresh: w.fresh,
            key: w.key.clone(),
            // A write to the callee's own new object stays construction; one to its parameter
            // is construction when the call hands it a new object.
            construction: match (&w.construction, here) {
                (Construction::Yes, Some(_)) => Construction::Yes,
                (Construction::Param { param, own, .. }, Some(r)) => {
                    r.through_arg(origin(usize::from(*param)), *own)
                }
                _ => Construction::No,
            },
            contexts: w.contexts | contexts,
            through_protocol: w.through_protocol,
        };
        insert_site(t, caller, site);
    }
}

/// What an arrival of an effect does to the chain `EffectSummary::exec_chains` keeps for it:
/// that chain runs whatever the caller's `self` is when any chain does, so a receiver of any
/// class sees a path it runs.
enum KeptChain {
    /// The first chain that runs whatever `self` is replaces one through a subclass's override.
    Replaced,
    /// The shorter of the two.
    Shorter,
    /// A chain through a subclass's override leaves one that runs whatever `self` is.
    Kept,
}

/// Record in `t` whether effect `e`, arriving through `bc` at `site` with `chain` (the caller
/// `had` it already), runs only for some classes of the caller's own `self`
/// (`EffectSummary::receiver_effects`): through a call on `self`, the classes the callee's
/// facts already depend on, or the subclass providing the callee. Any other arrival runs it
/// whatever `self` is. Returns how the arrival's chain joins the kept one.
#[allow(clippy::too_many_arguments)]
fn add_receiver_effect(
    t: &mut EffectSummary,
    e: ExecEffectKind,
    had: bool,
    site: &CallSite,
    bc: &BoundCallee,
    facts: &EffectSummary,
    chain: &SharedChain,
    lead: &[Arc<CallHop>],
    rewrite: bool,
) -> KeptChain {
    let mut provided: Vec<(SharedLine, SharedChain)> = Vec::new();
    if site.recv_is_self {
        let inner = facts.receiver_effects.as_ref().and_then(|r| r.get(&e));
        match (inner, site.provider(bc)) {
            (Some(inner), _) => {
                let receiver = site.receiver();
                provided.extend(
                    inner
                        .iter()
                        .filter(|(p, _)| receiver.is_none_or(|r| p.related(r)))
                        .map(|(p, c)| (Arc::clone(p), extend_chain(lead, c, rewrite))),
                );
            }
            (None, Some(p)) => provided.push((Arc::clone(p), chain.clone())),
            (None, None) => {}
        }
    }
    let only_provided = t
        .receiver_effects
        .as_ref()
        .is_some_and(|r| r.contains_key(&e));
    if provided.is_empty() {
        if let Some(r) = &mut t.receiver_effects {
            r.remove(&e);
            if r.is_empty() {
                t.receiver_effects = None;
            }
        }
        return if only_provided {
            KeptChain::Replaced
        } else {
            KeptChain::Shorter
        };
    }
    // Already reached whatever `self` is.
    if had && !only_provided {
        return KeptChain::Kept;
    }
    let slot = t
        .receiver_effects
        .get_or_insert_with(Box::default)
        .entry(e)
        .or_default();
    for (p, c) in provided {
        match slot.iter_mut().find(|(q, _)| *q == p) {
            Some((_, kept)) if kept.len() <= c.len() => {}
            Some((_, kept)) => *kept = c,
            None => slot.push((p, c)),
        }
    }
    slot.sort();
    KeptChain::Shorter
}

/// How a callee writes a typed field it does not know through an argument ([`IndirectWrite`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Through {
    /// `helper(obj.attr)` changing the value in place (`items.append(x)`).
    Field,
    /// `helper(obj)` writing attributes of a parameter the helper does not type.
    Instance,
}

/// A typed field write a call makes through one of its arguments, which the callee cannot tie
/// to a class itself.
pub(crate) struct IndirectWrite<'a> {
    /// The callee parameter the argument is bound to (below 64).
    pub param: u8,
    pub origin: &'a ArgOrigin,
    pub through: Through,
    pub class: &'a str,
    pub attr: &'a str,
    /// The caller parameters the written object may be, as itself (for [`Through::Field`] the
    /// object holding the attribute: `p` when the argument is `p.attr`).
    pub obj_params: u64,
}

/// The typed field writes `callee` (bound by `bc`, with the facts `facts` that run here) makes
/// through the arguments, which the caller's argument binding ties to a class: the one home of
/// that decision for the ownership rules and `MUT109`. A write the callee records itself, on
/// the same object, is its own: it reports or hands over that one.
pub(crate) fn indirect_writes<'a>(
    bc: &'a BoundCallee,
    callee: &EffectSummary,
    facts: &'a EffectSummary,
) -> Vec<IndirectWrite<'a>> {
    let mut out = Vec::new();
    for (&param, paths) in &facts.mutated_param_paths {
        let Some(origin) = bc.params.get(usize::from(param)).filter(|_| param < 64) else {
            continue;
        };
        if let Some((class, attr)) = &origin.typed_field
            && paths.contains("")
        {
            // A helper writing the value's own attributes writes those, as `obj.attr.x = v`
            // does.
            let obj_params = match &origin.param {
                Some((p, Some(path))) if path == attr => 1u64 << p,
                _ => 0,
            };
            out.push(IndirectWrite {
                param,
                origin,
                through: Through::Field,
                class,
                attr,
                obj_params,
            });
        }
        if callee.typed_params >> param & 1 == 0 {
            for class in origin.instance_classes() {
                out.extend(
                    paths
                        .iter()
                        .filter(|p| !p.is_empty() && *p != crate::ELEMENTS_PATH)
                        .map(|attr| IndirectWrite {
                            param,
                            origin,
                            through: Through::Instance,
                            class,
                            attr,
                            obj_params: origin.may_params,
                        }),
                );
            }
        }
    }
    out.retain(|w| !records_write(bc, facts, w));
    out
}

/// Whether the callee records `w` among its own typed writes: one of the same field on an
/// object that may be the same caller parameter. Without a parameter to compare (the object is
/// none of the caller's parameters), any write of the field counts.
fn records_write(bc: &BoundCallee, facts: &EffectSummary, w: &IndirectWrite<'_>) -> bool {
    if w.obj_params == 0 {
        return facts
            .typed_field_writes
            .iter()
            .any(|own| own.class_qualname == w.class && own.attr == w.attr);
    }
    facts.typed_field_write_sites.iter().any(|own| {
        own.class_qualname == w.class
            && own.attr == w.attr
            && bound_params(own.obj_params, bc) & w.obj_params != 0
    })
}

/// The caller parameters the objects bound to the callee parameters in `mask` may be.
pub(crate) fn bound_params(mask: u64, bc: &BoundCallee) -> u64 {
    if mask == 0 {
        return 0;
    }
    bc.params
        .iter()
        .take(64)
        .enumerate()
        .filter(|(k, _)| mask & (1u64 << k) != 0)
        .fold(0, |acc, (_, o)| acc | o.may_params)
}

/// Whether a call runs the callee's body: calling a generator function only creates the
/// generator, and calling an `async def` without awaiting or driving it only creates a
/// coroutine. Its body runs later: where the generator is iterated, or on the event loop,
/// where the coroutine's own boundary reports it.
pub(crate) fn runs_at_call(callee: &EffectSummary, site: &CallSite) -> bool {
    !callee.is_generator && (!callee.is_async || site.is_awaited || site.drives_coroutine_manually)
}

/// [`ProjectIndex::delivery`] with the summaries `summary_of` gives (the current finals during
/// propagation).
pub(crate) fn delivery<'s, 'i>(
    summary_of: impl Fn(&FuncKey) -> Option<&'i EffectSummary>,
    site: &'s CallSite,
    callee: &EffectSummary,
) -> Option<Delivery<'s>> {
    let result = |into| {
        Some(Delivery {
            from: Facts::Result,
            into,
            via: None,
        })
    };
    match &site.role {
        SiteRole::Call => runs_at_call(callee, site).then_some(Delivery {
            from: Facts::Body,
            into: Facts::Body,
            via: None,
        }),
        SiteRole::Iterate => result(Facts::Body),
        SiteRole::Return => result(Facts::Result),
        SiteRole::Through {
            via,
            iterated,
            returned,
        } => {
            callee.facts(Facts::Result)?;
            // The first candidate that iterates the parameter: its body runs at its call, or
            // iterating its result runs what iterates the parameter.
            let (p, _) = via.iter().find(|(p, k)| {
                let facts = summary_of(p).and_then(|ps| {
                    if *iterated {
                        ps.facts(Facts::Result)
                    } else {
                        runs_at_call(ps, site).then_some(ps)
                    }
                });
                facts.is_some_and(|f| *k < 64 && f.iterates_params & (1u64 << k) != 0)
            })?;
            Some(Delivery {
                from: Facts::Result,
                into: if *returned {
                    Facts::Result
                } else {
                    Facts::Body
                },
                via: Some(p),
            })
        }
    }
}

/// The hop from the site's caller to `callee`, at the site.
pub(crate) fn site_hop(site: &CallSite, path: &Path, callee: String) -> CallHop {
    CallHop {
        caller: site.caller.clone(),
        callee,
        span: site.span,
        path: path.to_path_buf(),
    }
}

/// The hops a delivery adds in front of the callee's chains: the site's caller calls the
/// callee, or (`via`) calls the project function that iterates the callee's result inside its
/// own body.
pub(crate) fn lead_hops(
    site: &CallSite,
    path: &Path,
    callee: &FuncKey,
    via: Option<&FuncKey>,
) -> SharedChain {
    match via {
        None => vec![Arc::new(site_hop(site, path, callee.fqn()))],
        Some(p) => vec![
            Arc::new(site_hop(site, path, p.fqn())),
            Arc::new(CallHop {
                caller: p.clone(),
                callee: callee.fqn(),
                span: site.span,
                path: path.to_path_buf(),
            }),
        ],
    }
}

/// Whether the callee's chains start with a hop of its own to replace: a plain function's
/// `iterated` facts begin where it returned the generator, but the generator runs where its
/// result is iterated, called from there and not from the function that returned it.
pub(crate) fn rewrites(callee: &EffectSummary, from: Facts) -> bool {
    from == Facts::Result && !callee.is_generator
}

/// `lead` followed by `chain`. With `rewrite`, the last lead hop reaches the callee of the
/// chain's first hop directly, which it replaces.
pub(crate) fn extend_chain(
    lead: &[Arc<CallHop>],
    chain: &[Arc<CallHop>],
    rewrite: bool,
) -> SharedChain {
    let mut out = Vec::with_capacity(lead.len() + chain.len());
    out.extend_from_slice(lead);
    match chain.split_first() {
        Some((first, rest)) if rewrite => {
            if let Some(last) = out.last_mut()
                && last.callee != first.callee
            {
                Arc::make_mut(last).callee.clone_from(&first.callee);
            }
            out.extend_from_slice(rest);
        }
        _ => out.extend_from_slice(chain),
    }
    out
}

fn bits(mask: u64) -> impl Iterator<Item = usize> {
    (0..64).filter(move |k| mask & (1u64 << k) != 0)
}

/// The caller's parameters passed as themselves for the callee parameters in `mask` (the
/// callee's offloaded or escaping parameters become the caller's).
fn caller_params(mask: u64, bc: &BoundCallee) -> u64 {
    let mut out = 0;
    for k in bits(mask) {
        if let Some(ArgOrigin {
            param: Some((p, None)),
            ..
        }) = bc.params.get(k)
        {
            out |= 1u64 << p;
        }
    }
    out
}

/// Function holding the original write of `site` (`None` = the summary owner itself).
fn site_origin(site: &TypedFieldWriteSite) -> Option<&str> {
    site.via_chain.last().map(|h| h.callee.as_str())
}

/// Insert a typed write site, keeping one (shortest) chain per original write and written
/// object (`obj_params`).
fn insert_site(s: &mut EffectSummary, owner: &FuncKey, site: TypedFieldWriteSite) {
    let origin = site_origin(&site);
    if origin.is_some_and(|o| is_fqn_of(o, owner)) {
        // The owner's own write coming back around a cycle.
        return;
    }
    // Sites order by `(class, attr, kind, span, via_chain, ...)`: the ones sharing
    // the first four fields form one range, starting at the empty chain.
    let first = TypedFieldWriteSite {
        class_qualname: site.class_qualname.clone(),
        attr: site.attr.clone(),
        kind: site.kind,
        span: site.span,
        via_chain: Vec::new(),
        obj_params: 0,
        fresh: false,
        key: None,
        construction: Construction::No,
        contexts: 0,
        through_protocol: false,
    };
    let existing = s
        .typed_field_write_sites
        .range(first..)
        .take_while(|t| {
            t.class_qualname == site.class_qualname
                && t.attr == site.attr
                && t.kind == site.kind
                && t.span == site.span
        })
        .find(|t| {
            t.obj_params == site.obj_params
                && t.key == site.key
                && t.contexts == site.contexts
                && site_origin(t) == origin
        })
        .cloned();
    let mut site = site;
    if let Some(existing) = existing {
        // One chain stands for every chain of the write: it constructs its object only when
        // each of them does.
        let construction = if existing.construction == site.construction {
            site.construction.clone()
        } else {
            Construction::No
        };
        if existing.via_chain.len() <= site.via_chain.len() {
            if existing.construction != construction {
                s.typed_field_write_sites.remove(&existing);
                s.typed_field_write_sites.insert(TypedFieldWriteSite {
                    construction,
                    ..existing
                });
            }
            return;
        }
        s.typed_field_write_sites.remove(&existing);
        site.construction = construction;
    }
    s.typed_field_write_sites.insert(site);
}

/// `name == owner.fqn()`, without building the fqn.
fn is_fqn_of(name: &str, owner: &FuncKey) -> bool {
    name.strip_prefix(owner.module.as_str())
        .and_then(|rest| rest.strip_prefix('.'))
        == Some(owner.qualname.as_str())
}

/// A private function name (`_name`, not a dunder) by its qualname's last component.
pub(crate) fn is_private_name(qualname: &str) -> bool {
    let name = qualname.rsplit('.').next().unwrap_or(qualname);
    name.starts_with('_') && !(name.starts_with("__") && name.ends_with("__"))
}

fn has_route_decorator(decorators: &[String]) -> bool {
    decorators.iter().any(|d| {
        let base = d.split('(').next().unwrap_or(d).trim();
        let last = base.rsplit('.').next().unwrap_or(base);
        base.contains('.') && ROUTE_DECORATORS.contains(&last)
    })
}

/// Section 2.7 `sync_driven_only`: a private `async def` without route decorators whose every
/// project caller is a sync function driving the coroutine by hand. Returns changed functions.
/// [`recompute_sync_driven`] for `candidates` only: the functions whose incoming call sites,
/// `async`-ness, or decorators may have changed (an edited file's functions and the callees of
/// its old and new call sites).
pub(crate) fn recompute_sync_driven_for(
    index: &mut ProjectIndex,
    candidates: &FxHashSet<FuncKey>,
) -> Vec<FuncKey> {
    let mut wanted: Vec<(FuncKey, bool)> = Vec::new();
    for func in candidates {
        let Some(s) = index.summary(func) else {
            continue;
        };
        let callers = index.reverse_call_graph.get(func);
        let mut any = false;
        let mut all_ok = true;
        for caller in callers.into_iter().flatten() {
            let caller_async = index.summary(caller).is_some_and(|c| c.is_async);
            let Some(file) = index
                .func_files
                .get(caller)
                .and_then(|p| index.files.get(p))
            else {
                continue;
            };
            // Only calls drive a coroutine: the other sites of a call's result are no calls.
            for site in file
                .call_sites
                .iter()
                .filter(|site| &site.caller == caller && site.role == SiteRole::Call)
            {
                if site.project_callees().iter().any(|bc| &bc.callee == func) {
                    any = true;
                    all_ok &= !caller_async && !site.is_awaited && site.drives_coroutine_manually;
                }
            }
        }
        let want = s.is_async
            && is_private_name(&func.qualname)
            && !has_route_decorator(&s.decorators)
            && any
            && all_ok;
        wanted.push((func.clone(), want));
    }
    let mut changed = Vec::new();
    for (func, want) in wanted {
        if let Some(path) = index.func_files.get(&func).cloned()
            && let Some(s) = index
                .files
                .get_mut(&path)
                .and_then(|f| f.functions.get_mut(&func))
            && s.sync_driven_only != want
        {
            Arc::make_mut(s).sync_driven_only = want;
            changed.push(func);
        }
    }
    changed.sort();
    changed
}

pub(crate) fn recompute_sync_driven(index: &mut ProjectIndex) -> Vec<FuncKey> {
    let mut incoming: FxHashMap<FuncKey, bool> = FxHashMap::default();
    for file in index.files.values() {
        for site in file.call_sites.iter().filter(|s| s.role == SiteRole::Call) {
            let caller_async = index.summary(&site.caller).is_some_and(|s| s.is_async);
            let ok = !caller_async && !site.is_awaited && site.drives_coroutine_manually;
            for bc in site.project_callees() {
                let all = incoming.entry(bc.callee.clone()).or_insert(true);
                *all &= ok;
            }
        }
    }
    let mut changed = Vec::new();
    for file in index.files.values_mut() {
        for (func, s) in &mut file.functions {
            let want = s.is_async
                && is_private_name(&func.qualname)
                && !has_route_decorator(&s.decorators)
                && incoming.get(func).copied().unwrap_or(false);
            if s.sync_driven_only != want {
                Arc::make_mut(s).sync_driven_only = want;
                changed.push(func.clone());
            }
        }
    }
    changed.sort();
    changed
}
