//! Interprocedural propagation (docs/DETAILED_SPEC_M3_M7.md section 2.6).
//!
//! `propagate` is the single fixpoint engine: it recomputes the final summaries of a
//! `region` of functions from their local facts and the current finals of their callees,
//! visiting Tarjan SCCs callees-first and iterating inside each SCC with a worklist.
//! `ProjectIndex::build` runs it over every function; `ProjectIndex::update_file` over the
//! edited file's SCC closure and then over the affected callers only.

use crate::scc::tarjan_scc;
use crate::{
    ArgOrigin, AttrKeyPath, BoundCallee, CallHop, CallSite, CallTarget, EffectSummary, FuncKey,
    ProjectIndex, TypedFieldWriteSite, WriteKind,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

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
/// function's call sites all live in its own file).
fn load_out_sites(index: &ProjectIndex, path: &Path, out: &mut OutSites) {
    let Some(file) = index.files.get(path) else {
        return;
    };
    for (i, site) in file.call_sites.iter().enumerate() {
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

fn project_callees(site: &CallSite) -> &[BoundCallee] {
    match &site.target {
        CallTarget::Project(callees) => callees,
        CallTarget::Layer2 { .. } => &[],
    }
}

/// Reset the final summaries of `region` to their local facts.
pub(crate) fn reset_to_local(index: &mut ProjectIndex, region: &FxHashSet<FuncKey>) {
    for func in region {
        let Some(path) = index.func_files.get(func).cloned() else {
            continue;
        };
        if let Some(file) = index.files.get_mut(&path)
            && let Some(local) = file.local_functions.get(func).cloned()
        {
            file.functions.insert(func.clone(), local);
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
            for c in project_callees(&index.files[&path].call_sites[i]) {
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
            for c in project_callees(&index.files[path].call_sites[*si]) {
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

    for scc in tarjan_scc(nodes.len(), &succ) {
        let members: FxHashSet<usize> = scc.iter().copied().collect();
        let mut queue: VecDeque<usize> = scc.iter().copied().collect();
        let mut queued: FxHashSet<usize> = members.clone();
        let budget = MAX_ITERATIONS_PER_SCC_MEMBER * scc.len();
        let mut steps = 0;
        let mut converged = true;
        while let Some(i) = queue.pop_front() {
            queued.remove(&i);
            steps += 1;
            if steps > budget {
                converged = false;
                break;
            }
            let new = recompute(index, &out, &nodes[i]);
            if index.summary(&nodes[i]) != Some(&new) {
                store(index, &nodes[i], new);
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

fn store(index: &mut ProjectIndex, func: &FuncKey, summary: EffectSummary) {
    if let Some(path) = index.func_files.get(func).cloned()
        && let Some(file) = index.files.get_mut(&path)
    {
        file.functions.insert(func.clone(), summary);
    }
}

/// Final summary of `func` = its local facts joined with every outgoing edge's transfer.
fn recompute(
    index: &ProjectIndex,
    out: &FxHashMap<FuncKey, Vec<(PathBuf, usize)>>,
    func: &FuncKey,
) -> EffectSummary {
    let path = &index.func_files[func];
    let file = &index.files[path];
    let mut s = file.local_functions[func].clone();
    s.sync_driven_only = file
        .functions
        .get(func)
        .is_some_and(|cur| cur.sync_driven_only);
    for (site_path, i) in out.get(func).into_iter().flatten() {
        let site = &index.files[site_path].call_sites[*i];
        for bc in project_callees(site) {
            if let Some(callee) = index.summary(&bc.callee) {
                apply_edge(&mut s, func, site, bc, callee, site_path);
            }
        }
    }
    s.refresh_typed_field_writes();
    s
}

/// Transfer the facts of one resolved callee across one call site into the caller.
fn apply_edge(
    s: &mut EffectSummary,
    caller: &FuncKey,
    site: &CallSite,
    bc: &BoundCallee,
    callee: &EffectSummary,
    path: &Path,
) {
    let hop = CallHop {
        caller: caller.clone(),
        callee: bc.callee.fqn(),
        span: site.span,
        path: path.to_path_buf(),
    };
    let origin = |k: usize| bc.params.get(k);

    // a. Offload wrappers: a callee offloading its parameter k offloads the caller's
    //    parameter p when argument k is p. The offloaded callable is never called here,
    //    so its blocking effects never reach the caller.
    for k in bits(callee.offloads_blocking_params) {
        if let Some(ArgOrigin {
            param: Some((p, None)),
            ..
        }) = origin(k)
        {
            s.offloads_blocking_params |= 1u64 << p;
        }
    }

    // b. Execution effects. Calling an `async def` without awaiting or driving it only
    //    creates a coroutine: its body runs later on the event loop, where its own boundary
    //    reports it.
    if runs_at_call(callee, site) {
        for e in callee
            .direct_exec_effects
            .iter()
            .chain(&callee.transitive_exec_effects)
        {
            s.transitive_exec_effects.insert(*e);
            let mut chain = vec![hop.clone()];
            chain.extend(callee.exec_chains.get(e).cloned().unwrap_or_default());
            let slot = s.exec_chains.entry(*e).or_default();
            if slot.is_empty() || chain.len() < slot.len() {
                *slot = chain;
            }
        }
    }

    // c. Mutation & state effects.
    if site.recv_is_self && callee.owner_class_qualname.is_some() {
        s.reads_self_attrs
            .extend(callee.reads_self_attrs.iter().cloned());
        s.mutates_self_attrs
            .extend(callee.mutates_self_attrs.iter().cloned());
        s.invalidates_cached
            .extend(callee.invalidates_cached.iter().cloned());
    }
    for (&i, paths) in &callee.mutated_param_paths {
        let Some(o) = origin(usize::from(i)) else {
            continue;
        };
        match &o.param {
            Some((p, None)) => {
                s.mutates_params |= 1u64 << p;
                s.mutated_param_paths
                    .entry(*p)
                    .or_default()
                    .extend(paths.iter().cloned());
            }
            Some((p, Some(attr))) => {
                s.mutates_params |= 1u64 << p;
                s.mutated_param_paths
                    .entry(*p)
                    .or_default()
                    .insert(attr.clone());
            }
            None => {}
        }
        if let Some(attr) = &o.self_attr {
            s.mutates_self_attrs.insert(AttrKeyPath {
                attr: attr.clone(),
                const_key: None,
            });
        }
        let mut indirect = Vec::new();
        if let Some((class, attr)) = &o.typed_field {
            indirect.push((class.clone(), attr.clone()));
        }
        if let Some(class) = &o.typed_instance {
            indirect.extend(
                paths
                    .iter()
                    .filter(|p| !p.is_empty())
                    .map(|p| (class.clone(), p.clone())),
            );
        }
        for (class, attr) in indirect {
            // A callee that already records a typed site for this field passes it up below.
            let known = callee
                .typed_field_write_sites
                .iter()
                .any(|t| t.class_qualname == class && t.attr == attr);
            if !known {
                insert_site(
                    s,
                    caller,
                    TypedFieldWriteSite {
                        class_qualname: class,
                        attr,
                        kind: WriteKind::IndirectHelper,
                        span: site.span,
                        via_chain: vec![hop.clone()],
                    },
                );
            }
        }
    }
    for k in bits(callee.escapes_params) {
        if let Some(ArgOrigin {
            param: Some((p, None)),
            ..
        }) = origin(k)
        {
            s.escapes_params |= 1u64 << p;
        }
    }
    for t in &callee.typed_field_write_sites {
        let mut site = t.clone();
        site.via_chain.insert(0, hop.clone());
        insert_site(s, caller, site);
    }
}

/// Whether the callee's body runs at this call site. Calling an `async def` without awaiting
/// or driving it only creates a coroutine.
pub(crate) fn runs_at_call(callee: &EffectSummary, site: &CallSite) -> bool {
    !callee.is_async || site.is_awaited || site.drives_coroutine_manually
}

fn bits(mask: u64) -> impl Iterator<Item = usize> {
    (0..64).filter(move |k| mask & (1u64 << k) != 0)
}

/// Function holding the original write of `site` (`None` = the summary owner itself).
fn site_origin(site: &TypedFieldWriteSite) -> Option<&str> {
    site.via_chain.last().map(|h| h.callee.as_str())
}

/// Insert a typed write site, keeping one (shortest) chain per original write.
fn insert_site(s: &mut EffectSummary, owner: &FuncKey, site: TypedFieldWriteSite) {
    let origin = site_origin(&site).map(str::to_owned);
    if origin.as_deref() == Some(owner.fqn().as_str()) {
        // The owner's own write coming back around a cycle.
        return;
    }
    let same = |t: &TypedFieldWriteSite| {
        t.class_qualname == site.class_qualname
            && t.attr == site.attr
            && t.kind == site.kind
            && t.span == site.span
            && site_origin(t).map(str::to_owned) == origin
    };
    if let Some(existing) = s.typed_field_write_sites.iter().find(|t| same(t)).cloned() {
        if existing.via_chain.len() <= site.via_chain.len() {
            return;
        }
        s.typed_field_write_sites.remove(&existing);
    }
    s.typed_field_write_sites.insert(site);
}

fn is_private_name(qualname: &str) -> bool {
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
            for site in file.call_sites.iter().filter(|site| &site.caller == caller) {
                if project_callees(site).iter().any(|bc| &bc.callee == func) {
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
            s.sync_driven_only = want;
            changed.push(func);
        }
    }
    changed.sort();
    changed
}

pub(crate) fn recompute_sync_driven(index: &mut ProjectIndex) -> Vec<FuncKey> {
    let mut incoming: FxHashMap<FuncKey, bool> = FxHashMap::default();
    for file in index.files.values() {
        for site in &file.call_sites {
            let caller_async = index.summary(&site.caller).is_some_and(|s| s.is_async);
            let ok = !caller_async && !site.is_awaited && site.drives_coroutine_manually;
            for bc in project_callees(site) {
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
                s.sync_driven_only = want;
                changed.push(func.clone());
            }
        }
    }
    changed.sort();
    changed
}
