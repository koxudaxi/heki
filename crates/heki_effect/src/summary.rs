//! Local (one function) effect extraction (docs/ANALYSIS_AND_KB_SPEC.md section 2.2).
//!
//! Produces the function's local `EffectSummary` plus the `CallSite`s the interprocedural
//! fixpoint (`callgraph`) maps callee facts through. No IR survives this module.

use crate::UnboundCall;
use crate::blind_spots::{self, Blind, BlindCall, UnresolvedReason};
use crate::cache_flow::{self, KeptRemovalKind};
use crate::cancellation;
use crate::capacity::{self, Bound, CapacityOf, CapacityWait, SiteQueues, WhileFull};
use crate::construction::{Escape, Pos, Recorder};
use crate::consumption::{ItemArgs, IterFlow, PerItemCall, Roots, param_iteration};
use crate::decorators::keeps_returned_value;
use crate::environment::{self, AccessKind, ExternalAccess};
use crate::fresh;
use crate::guarded;
use crate::guards::{self, GuardInputs, LocalGuards, per_item_bit};
use crate::held;
use crate::imports::{ImportSite, module_imports};
use crate::infer::CellKey;
use crate::invalidation::{self, Events, FlowWrite, FuncFlow, Reach, SiteFlow};
use crate::invokes;
use crate::iterations::{
    self, BetweenSteps, ContainerChange, IteratedContainer, IteratedResult, ResultIteration,
};
use crate::locks::{self, LockStep, ObjRef};
use crate::one_at_a_time;
use crate::order_flow;
use crate::ownership::{self, LocalOrigins, Origins, ParamRole, ReturnOrigins, Source};
use crate::partial::Partial;
use crate::readonly::{
    self, ContainerKind, ContractSharing, Derivation, ProtectedReturn, ReturnStep,
    container_mutator,
};
use crate::resolver::stores::{self, AttrName};
use crate::resolver::{
    self, Callee, Constructor, DeclKeys, FunctionDecl, ModuleDecls, TypeCtx, Val, func_qualname,
    is_generator_function, self_aliases,
};
use crate::types::{Held, Ty};
use crate::{
    Access, ArgBinding, ArgOrigin, ArgTargets, ArgValue, AttrKeyPath, BoundCallee, CallHop,
    CallSite, CallTarget, CallTests, CellChange, Construction, ContainerParams, ContractClass,
    Contracts, Dispatch, EffectSummary, EntryKey, FileSummary, FuncKey, GuardSubject, HandedState,
    Implementers, InputGuard, Items, KeptFacts, ListedCache, MayGiven, ModuleInput, MustExec,
    NewArg, NewCond, NewFrom, NewKind, OnEveryReturn, OptInFacts, OverrideRoles, PerItem,
    ProjectDecls, ReadKind, ReturnsNew, Runner, SharedLine, SharedState, SharedStateFacts,
    SiteRole, Slot, TypedFieldReadSite, TypedFieldWriteSite, UnawaitedHandoff, Unresolved,
    UnresolvedAccessSite, UnresolvedWriteSite, WriteKind,
};
use heki_ast::Span;
use heki_ir::walk::{Answer, Budget, Closure, Def, FnShape, Join, KeySet, Or, Use};
use heki_ir::{
    BasicBlock, BinOpKind, BlockId, BuiltinKind, CompareOpKind, ConstLiteral, DictEntry,
    FunctionId, FunctionKind, Inst, InstKind, IrFunction, ModuleIr, Must, NaturalLoop, PhiNode,
    Progress, Terminator, UnaryOpKind, VarId,
};
use heki_kb::{
    EffectiveEcosystemKb, EffectiveSoundnessKb, EnvironmentAccess, ExecEffectKind, LiteralValue,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::Cow;
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

mod entries;
mod imports;
mod keywords;
mod reads;
mod references;
mod sentinels;
pub(crate) mod statements;

use keywords::agreed_marks;

/// The (value, place) pairs the store walk of `return_origins` may take per byte of a function's
/// source, above a floor: a function past it returns objects of unknown origin.
const KEPT_PAIRS_PER_SOURCE_BYTE: usize = 1;
const MIN_KEPT_PAIRS: usize = 4096;

/// The values the origin walks of `return_origins` may take per byte of a function's source,
/// above a floor: a function past it returns objects of unknown origin.
const ORIGIN_STEPS_PER_SOURCE_BYTE: usize = 1;
const MIN_ORIGIN_STEPS: usize = 4096;

/// Execution effects `EFF101` treats as blocking the event loop.
pub const BLOCKING_EFFECTS: &[ExecEffectKind] = ExecEffectKind::BLOCKING;

/// The callable an offloading API runs on a worker thread at a call of `symbol`, and where the
/// positional arguments it passes start: `asyncio.to_thread(f, a, b)` runs `f(a, b)`. `None`
/// when `*` unpacking may move the callable.
pub fn offloaded_callable(
    kb: &EffectiveEcosystemKb,
    symbol: &str,
    args: &[VarId],
    star_from: Option<u8>,
) -> Option<(VarId, usize)> {
    let k = usize::from(kb.lookup(symbol)?.offloads_blocking_arg?);
    if star_from.is_some_and(|s| usize::from(s) <= k) {
        return None;
    }
    Some((*args.get(k)?, k + 1))
}

/// Summarize every function of one module (local facts only; see `ProjectIndex::build`).
/// Summarize one module against the project declarations. `own` is the module's own completed
/// declarations with what completing them read; with `track`, what the summary reads of
/// `decls` is recorded too (`FileSummary::decl_reads`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn summarize_module(
    input: &ModuleInput<'_>,
    decls: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    soundness: Option<&EffectiveSoundnessKb>,
    own: (Arc<ModuleDecls>, DeclKeys),
    track: bool,
    contracts: Option<&dyn Contracts>,
    opt_in: OptInFacts,
) -> FileSummary {
    let (own_decls, completion_reads) = own;
    let (mut file, reads) = if track {
        decls.record(|| extract_module(input, decls, kb, soundness, contracts, opt_in, None))
    } else {
        (
            extract_module(input, decls, kb, soundness, contracts, opt_in, None),
            DeclKeys::default(),
        )
    };
    file.decls = own_decls;
    file.decl_reads = reads;
    file.decl_reads.extend(completion_reads);
    file
}

/// [`summarize_module`] with the parameter types `seeded` gives functions of the module (the
/// one premise of a registration context), recording no reads.
#[allow(clippy::too_many_arguments)]
pub(crate) fn summarize_module_seeded(
    input: &ModuleInput<'_>,
    decls: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    soundness: Option<&EffectiveSoundnessKb>,
    own: Arc<ModuleDecls>,
    contracts: Option<&dyn Contracts>,
    opt_in: OptInFacts,
    seeded: &crate::resolver::SeededParams,
) -> FileSummary {
    let mut file = extract_module(input, decls, kb, soundness, contracts, opt_in, Some(seeded));
    file.decls = own;
    file
}

#[allow(clippy::too_many_arguments)]
fn extract_module(
    input: &ModuleInput<'_>,
    decls: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    soundness: Option<&EffectiveSoundnessKb>,
    contracts: Option<&dyn Contracts>,
    opt_in: OptInFacts,
    seeded: Option<&crate::resolver::SeededParams>,
) -> FileSummary {
    let cx = TypeCtx::new(input, decls, kb)
        .with_layer1(soundness)
        .with_seeded(seeded);
    let mut file = FileSummary {
        path: input.path.clone(),
        module_name: input.module_name.clone(),
        content_hash: input.content_hash.clone(),
        ..FileSummary::default()
    };
    // `typing.overload` signatures are replaced by the implementation when it is defined: only
    // the implementation runs.
    let implemented: FxHashSet<&str> = input
        .ir
        .functions
        .iter()
        .filter(|f| !f.is_class_body && !is_overload(f))
        .map(func_qualname)
        .collect();
    if opt_in.suggestions {
        let facts = crate::only_read::annotation_facts(&cx, input.semantic);
        if !facts.is_empty() {
            file.opt_in_tables_mut().annotation_facts = facts;
        }
    }
    let mut cell_aliases = CellAliases::default();
    let mut imported_by_name: Vec<ImportSite> = Vec::new();
    for f in &input.ir.functions {
        if f.is_class_body || (is_overload(f) && implemented.contains(func_qualname(f))) {
            continue;
        }
        let key = FuncKey {
            module: input.module_name.clone(),
            qualname: func_qualname(f).to_owned(),
        };
        let shape = FnShape::new(f);
        let Extracted {
            mut summary,
            mut sites,
            flow,
            mut references,
            mut external,
            cells,
            kept_points,
            types,
            mut blind,
            mut unbound,
            imports: by_name,
            mut unawaited,
            fresh_return,
            mut class_runs,
        } = Extractor::new(
            &shape,
            &key,
            &cx,
            &input.path,
            contracts,
            opt_in,
            &cell_aliases,
        )
        .run();
        cell_aliases.extend(cells);
        imported_by_name.extend(by_name);
        if opt_in.dispatch {
            summary.dispatch = crate::dispatch::extract(&cx, input.ir, f);
        }
        // A `class` statement runs its body where it runs, in a function or the module body:
        // the body's effects, calls (a class attribute's value, a decorator, a default
        // argument), and accesses are that code's (its methods run only when they are called).
        for body in class_bodies_run_by(input.ir, f) {
            let ran = class_runs.get(&body.id).copied().unwrap_or_default();
            let body = FnShape::new(body);
            let facts = Extractor::new(
                &body,
                &key,
                &cx,
                &input.path,
                contracts,
                opt_in,
                &cell_aliases,
            )
            .run();
            absorb_class_body(&mut summary, facts.summary, &key, ran);
            external.extend(facts.external);
            blind.extend(facts.blind);
            unbound.extend(facts.unbound);
            imported_by_name.extend(facts.imports);
            unawaited.extend(facts.unawaited);
            let in_function = |site: CallSite| CallSite {
                flow: None,
                on_every_return: run_through(site.on_every_return, ran),
                ..site
            };
            sites.extend(facts.sites.into_iter().map(in_function));
            references.extend(facts.references);
            let nested = facts.class_runs.into_iter();
            class_runs.extend(nested.map(|(id, run)| (id, run_through(run, ran))));
        }
        if let Some(mut flow) = flow {
            // Site-list indices of the function become indices into the file's call sites.
            let offset = u32::try_from(file.call_sites.len()).unwrap_or(u32::MAX);
            let rebase = |f: &mut SiteFlow| {
                f.next_call = f.next_call.map(|i| i.saturating_add(offset));
            };
            for site in &mut sites {
                if let Some(f) = &mut site.flow {
                    rebase(f);
                }
            }
            flow.entries.iter_mut().for_each(rebase);
            for w in &mut flow.writes {
                rebase(&mut w.flow);
            }
            let slot = file.flows.entry(key.clone()).or_default();
            slot.entries.extend(flow.entries);
            slot.writes.extend(flow.writes);
        }
        // A replaced value is still kept: only removals make a getter run again.
        for point in &kept_points {
            if let cache_flow::KeptRemovalKind::Named(name) = &point.kind {
                file.opt_in_tables_mut().kept_removals.insert(name.clone());
            }
        }
        let kept = opt_in.kept_values.then_some(kept_points.as_slice());
        if let Some(flow) = cache_flow::cache_flow(&shape, &key, &sites, &cx, &types, kept) {
            summary.lazily_read_self_attrs = flow.lazily_initialized().map(str::to_owned).collect();
            file.cache_flows.entry(key.clone()).or_default().push(flow);
        }
        // Same qualname defined twice (conditional definitions): its origin nodes follow the
        // first definition's.
        if let Some(first) = file.local_functions.get(&key) {
            rebase_origin_nodes(
                &mut summary,
                &mut sites,
                first.return_origin_facts().nodes.len(),
            );
        }
        file.call_sites.extend(sites);
        file.reference_sites.extend(references);
        if !external.is_empty() {
            file.external_accesses
                .entry(key.clone())
                .or_default()
                .extend(external);
        }
        if !unbound.is_empty() {
            file.unbound_calls
                .entry(key.clone())
                .or_default()
                .extend(unbound);
        }
        if !blind.is_empty() {
            // A `finally` body is built once per way out of its `try`: its calls once each.
            let calls = (file.opt_in_tables_mut().blind_spots)
                .entry(key.clone())
                .or_default();
            calls.extend(blind);
            calls.sort();
            calls.dedup();
        }
        if !unawaited.is_empty() {
            // A `finally` body is built once per way out of its `try`: its calls once each.
            let handoffs = (file.opt_in_tables_mut().unawaited_handoffs)
                .entry(key.clone())
                .or_default();
            handoffs.extend(unawaited);
            handoffs.sort_by_key(|h| h.span);
            handoffs.dedup();
        }
        if file.local_functions.contains_key(&key) {
            if let Some(tables) = &mut file.opt_in_tables {
                tables.fresh_returns.join_definition(&key, fresh_return);
            }
        } else if let Some(fact) = fresh_return {
            (file.opt_in_tables_mut().fresh_returns.local).insert(key.clone(), fact);
        }
        // Same qualname defined twice (conditional definitions): keep the union of facts.
        match file.local_functions.get_mut(&key) {
            Some(existing) => merge_duplicate(Arc::make_mut(existing), summary, &key),
            None => {
                file.local_functions.insert(key, Arc::new(summary));
            }
        }
    }
    if opt_in.imports
        && let Some(imports) = module_imports(input, imported_by_name)
    {
        file.opt_in_tables_mut().imports = Some(Box::new(imports));
    }
    release_spare_capacity(&mut file);
    file
}

/// The class bodies the `class` statements of `f` run, nested classes included, each once (a
/// `class` statement in a `finally` body is made once per way out of its `try`), each before
/// the bodies of the classes it makes.
/// The special method a `for` loop (or a comprehension) runs on what it iterates.
fn iteration_protocol(is_async: bool) -> &'static str {
    if is_async { "__aiter__" } else { "__iter__" }
}

fn class_bodies_run_by<'m>(ir: &'m ModuleIr, f: &'m IrFunction) -> Vec<&'m IrFunction> {
    let mut out: Vec<&IrFunction> = Vec::new();
    let mut made: FxHashSet<FunctionId> = FxHashSet::default();
    let mut stack = vec![f];
    while let Some(g) = stack.pop() {
        for inst in g.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(id) = class_body_made(ir, &inst.kind)
                && made.insert(id)
            {
                out.push(ir.function(id));
                stack.push(ir.function(id));
            }
        }
    }
    out
}

/// Whether `kind` may take a lock: a `with` item or an `acquire()` (`locks::lock_step` decides).
fn may_take_lock(kind: &InstKind) -> bool {
    match kind {
        InstKind::EnterWith { .. } => true,
        InstKind::MethodCall { method, .. } => method == "acquire",
        _ => false,
    }
}

/// The class body the instruction runs, when it is a `class` statement.
fn class_body_made(ir: &ModuleIr, kind: &InstKind) -> Option<FunctionId> {
    match kind {
        InstKind::MakeClosure {
            func_id: Some(id), ..
        } if ir.function(*id).is_class_body => Some(*id),
        _ => None,
    }
}

/// [`CallSite::on_every_return`] of a point of a class body (a call, or the `class` statement of
/// a nested class) in the code that runs the body, from `body`, that code's flags of its `class`
/// statement ([`Extractor::class_runs`]). The point has run whenever the code returns only when
/// the body has returned too: an exception the body raised before the point may have been
/// caught there.
fn run_through(point: OnEveryReturn, body: OnEveryReturn) -> OnEveryReturn {
    OnEveryReturn {
        called: point.called && body.returned,
        returned: point.returned && body.returned,
        live: point.live && body.live,
    }
}

/// Add the boxed local sites `more` to `into`.
fn extend_boxed<C>(into: &mut Option<Box<C>>, more: Option<Box<C>>)
where
    C: Default + IntoIterator + Extend<<C as IntoIterator>::Item>,
{
    if let Some(more) = more {
        into.get_or_insert_with(Box::default).extend(*more);
    }
}

/// Add the facts of a class body the function runs (`body`) to the function's own: its
/// execution effects, the project attributes it writes and reads, and the locks it takes. `ran`
/// are the function's flags of the `class` statement ([`Extractor::class_runs`]): a lock the
/// body attempts to take whenever it returns is one the function attempts whenever it returns
/// only when the body has returned then too, as for its calls ([`run_through`]).
fn absorb_class_body(
    into: &mut EffectSummary,
    body: EffectSummary,
    func: &FuncKey,
    ran: OnEveryReturn,
) {
    into.only_read_params &= body.only_read_params;
    into.join_read_kinds(&body);
    for e in &body.direct_exec_effects {
        let had = into.direct_exec_effects.contains(e);
        let occ = body.exec_guards.get(e).cloned().unwrap_or_default();
        guards::add_occurrence(&mut into.exec_guards, *e, had, &occ);
    }
    into.direct_exec_effects.extend(body.direct_exec_effects);
    into.transitive_exec_effects
        .extend(body.transitive_exec_effects);
    for (k, chain) in body.exec_chains {
        into.exec_chains.entry(k).or_insert(chain);
    }
    into.typed_field_write_sites
        .extend(body.typed_field_write_sites);
    into.typed_field_read_sites
        .extend(body.typed_field_read_sites);
    extend_boxed(&mut into.sentinel_tests, body.sentinel_tests);
    extend_boxed(&mut into.deep_copies, body.deep_copies);
    into.private_calls.extend(body.private_calls);
    into.unknown_calls |= body.unknown_calls;
    add_state_changes(into, body.shared_state.as_deref());
    into.capacity_waits.extend(body.capacity_waits);
    let mut locks = body.locks;
    for acq in locks.iter_mut().flat_map(|l| l.acquires.values_mut()) {
        acq.on_every_return &= ran.returned;
    }
    into.absorb_lock_facts(locks);
    into.absorb_exclusive_uses(body.exclusive.into_iter().flat_map(|uses| *uses));
    into.refresh_typed_field_writes(func);
}

/// [`ContainerParams`] of two definitions of one name: a parameter either annotates, whose uses
/// both allow.
fn merge_container_params(
    into: &mut Option<Box<ContainerParams>>,
    other: Option<&ContainerParams>,
) {
    let none = ContainerParams::default();
    let (a, b) = (into.as_deref().unwrap_or(&none), other.unwrap_or(&none));
    let merged = ContainerParams {
        annotated: a.annotated | b.annotated,
        abstract_use: a.abstract_use & b.abstract_use,
        bound_once: a.bound_once & b.bound_once,
    };
    *into = (merged.annotated != 0).then(|| Box::new(merged));
}

/// Add the changes of shared state of another body of the function (`other`'s changed state,
/// globals, cells, and the state its calls hand on) to `into`'s, keeping the first change of
/// each.
fn add_state_changes(into: &mut EffectSummary, other: Option<&SharedStateFacts>) {
    let Some(other) = other.filter(|o| {
        o.globals || !o.cells.is_empty() || !o.changed.is_empty() || !o.handed.is_empty()
    }) else {
        return;
    };
    into.change_shared_state(|f| {
        f.globals |= other.globals;
        f.cells.extend(other.cells.iter().cloned());
        f.changed.extend(other.changed.iter().cloned());
        f.handed.extend(other.handed.iter().cloned());
    });
}

/// A `@typing.overload` signature.
fn is_overload(f: &IrFunction) -> bool {
    f.has_decorator(&["overload"])
}

/// Building by pushes leaves spare capacity in the call-site containers, and a file's summary
/// stays in the index for the whole run.
fn release_spare_capacity(file: &mut FileSummary) {
    for site in &mut file.call_sites {
        if let Some(callees) = site.project_callees_mut() {
            for bc in callees.iter_mut() {
                bc.params.shrink_to_fit();
            }
            callees.shrink_to_fit();
        }
    }
    file.call_sites.shrink_to_fit();
    file.reference_sites.shrink_to_fit();
}

/// Shift the origin nodes a definition refers to (in its own nodes, its returns, and its call
/// arguments) by `offset`: the definition's nodes follow another's of the same qualname.
fn rebase_origin_nodes(summary: &mut EffectSummary, sites: &mut [CallSite], offset: usize) {
    let offset = u32::try_from(offset).unwrap_or(u32::MAX);
    let shift = |nodes: &mut Vec<u32>| {
        for n in nodes {
            *n = n.saturating_add(offset);
        }
    };
    if let Some(facts) = &mut summary.return_origins {
        shift(&mut facts.returned.nodes);
        for o in &mut facts.nodes {
            shift(&mut o.nodes);
        }
    }
    let args = sites
        .iter_mut()
        .filter_map(CallSite::project_callees_mut)
        .flatten()
        .flat_map(|bc| &mut bc.params)
        .filter_map(|p| p.origins.as_deref_mut());
    for o in args {
        shift(&mut o.nodes);
    }
}

fn merge_duplicate(into: &mut EffectSummary, other: EffectSummary, func: &FuncKey) {
    // A name defined twice is not followed by the registration contexts.
    into.dispatch = None;
    for p in (0..64u8).filter(|p| other.retains_params & (1u64 << p) != 0) {
        into.keep_params(1u64 << p, &other.retained_given(p));
    }
    // A must-fact: only read where both definitions only read it.
    into.only_read_params &= other.only_read_params;
    into.join_read_kinds(&other);
    merge_container_params(
        &mut into.container_params,
        other.container_params.as_deref(),
    );
    // `returns_self_attrs` holds for every definition: an attribute that either one reads for
    // more than its object is left out.
    let read_otherwise = |s: &EffectSummary| -> BTreeSet<String> {
        s.reads_self_attrs
            .iter()
            .filter(|r| !s.returns_self_attrs.contains(&r.attr))
            .map(|r| r.attr.clone())
            .collect()
    };
    let read = [read_otherwise(into), read_otherwise(&other)];
    // A must-fact: a cached container only when both definitions return one of the same kind.
    if into.cached_result != other.cached_result {
        into.cached_result = None;
    }
    into.add_self_reads(&other);
    into.returns_self_attrs.extend(other.returns_self_attrs);
    into.returns_self_attrs
        .retain(|a| !read.iter().any(|r| r.contains(a)));
    // The effects of either definition, with the guards of each occurrence.
    for e in &other.direct_exec_effects {
        let had = into.direct_exec_effects.contains(e);
        let occ = other.exec_guards.get(e).cloned().unwrap_or_default();
        guards::add_occurrence(&mut into.exec_guards, *e, had, &occ);
    }
    into.direct_exec_effects.extend(other.direct_exec_effects);
    into.transitive_exec_effects
        .extend(other.transitive_exec_effects);
    for (k, chain) in other.exec_chains {
        into.exec_chains.entry(k).or_insert(chain);
    }
    into.must_exec = Some(Box::new(MustExec::of_name_bound_twice()));
    into.mutates_params |= other.mutates_params;
    // Typed only where both definitions type it.
    into.typed_params &= other.typed_params;
    into.escapes_params |= other.escapes_params;
    into.consumes_params |= other.consumes_params;
    into.iterates_params |= other.iterates_params;
    // A must-fact: propagate only through parameters both definitions forward unconditionally.
    into.unconditional_forwards &= other.unconditional_forwards;
    into.entry_change_params |= other.entry_change_params;
    for (i, paths) in other.mutated_param_paths {
        let memos = other.memo_param_paths.as_deref().and_then(|m| m.get(&i));
        for path in paths {
            let memo = memos.is_some_and(|m| m.contains(&path));
            into.note_param_change(i, path, memo);
        }
    }
    for (i, attrs) in other.changed_param_attrs {
        into.changed_param_attrs.entry(i).or_default().extend(attrs);
    }
    for (i, spans) in other.param_change_sites {
        into.param_change_sites.entry(i).or_default().extend(spans);
    }
    for (i, spans) in other.param_inplace_sites {
        into.param_inplace_sites.entry(i).or_default().extend(spans);
    }
    for (i, spans) in other.retyped_param_changes.into_iter().flat_map(|c| *c) {
        let into = into.retyped_param_changes.get_or_insert_with(Box::default);
        into.entry(i).or_default().extend(spans);
    }
    for (i, slots) in other.inplace_param_slots {
        into.inplace_param_slots.entry(i).or_default().extend(slots);
    }
    into.mutates_self_attrs.extend(other.mutates_self_attrs);
    if let Some(facts) = other.iterations {
        into.iteration_facts_mut().merge(*facts);
    }
    into.invalidates_cached.extend(other.invalidates_cached);
    into.merge_kept(other.kept.as_deref());
    into.typed_field_write_sites
        .extend(other.typed_field_write_sites);
    into.typed_field_read_sites
        .extend(other.typed_field_read_sites);
    extend_boxed(&mut into.sentinel_tests, other.sentinel_tests);
    extend_boxed(&mut into.deep_copies, other.deep_copies);
    into.private_calls.extend(other.private_calls);
    into.offloads_blocking_params |= other.offloads_blocking_params;
    into.unknown_calls |= other.unknown_calls;
    add_state_changes(into, other.shared_state.as_deref());
    // A must-fact: new in every definition.
    if !into.new_cells().is_empty() {
        let other_new = other.shared_state.as_ref().map(|f| &f.new_cells);
        into.change_shared_state(|f| {
            f.new_cells
                .retain(|c| other_new.is_some_and(|o| o.contains(c)));
        });
    }
    if let Some(o) = other.returns_protected {
        ProtectedReturn::add(&mut into.returns_protected, *o, ReturnStep::ITSELF);
    }
    // A must-fact: a new object only when both definitions return one.
    into.returns_new = match (std::mem::take(&mut into.returns_new), other.returns_new) {
        (
            ReturnsNew::New {
                kind: a,
                cond: c,
                elements_of: x,
                kept: mut k,
                linked: i,
            },
            ReturnsNew::New {
                kind: b,
                cond: d,
                elements_of: y,
                kept: l,
                linked: j,
            },
        ) => ReturnsNew::New {
            kind: a.min(b),
            cond: c.and(&d),
            elements_of: x | y,
            kept: {
                k.join(&l);
                k
            },
            linked: i || j,
        },
        _ => ReturnsNew::No,
    };
    if let Some(other) = other.return_origins {
        let facts = into.return_origins.get_or_insert_with(Box::default);
        facts.returned.known.join(other.returned.known);
        facts.returned.results.extend(other.returned.results);
        facts.returned.nodes.extend(other.returned.nodes);
        facts.nodes.extend(other.nodes);
    }
    into.unresolved_write_sites
        .extend(other.unresolved_write_sites);
    into.unresolved_access_sites
        .extend(other.unresolved_access_sites);
    into.capacity_waits.extend(other.capacity_waits);
    into.absorb_lock_facts(other.locks);
    into.absorb_exclusive_uses(other.exclusive.into_iter().flat_map(|uses| *uses));
    into.refresh_typed_field_writes(func);
}

/// Who owns an object being written: the method's `self`, a parameter path, or a typed object.
#[derive(Debug, Clone, Default)]
struct Owner {
    is_self: bool,
    param: Option<(u8, Option<String>)>,
    /// The project classes the object may be an instance of.
    typed_classes: Vec<String>,
    /// Parameters the object may be, as itself.
    may_params: u64,
    /// An instance the function constructs (`TypedFieldWriteSite::fresh`).
    fresh: bool,
    /// Where the object comes from among the objects that existed before the call (`MUT208`).
    reach: Reach,
    /// The variable holding the object (`TypedFieldWriteSite::construction`).
    var: Option<VarId>,
    /// Heki knows nothing of the object's class ([`Extractor::unknown_class`] of `var`): a write
    /// heki cannot name is unresolved.
    unknown_class: bool,
}

/// Which classes a write under a name heki cannot read may add an attribute to
/// ([`Unresolved::Name`] rather than [`Unresolved::Existing`]).
#[derive(Debug, Clone, Copy)]
enum Adds<'s> {
    /// Every class: the name is computed at run time.
    Any,
    /// Those that not every one of these classes is or derives from: the name is a key of the
    /// namespace of an instance of one of them, or a field one declares
    /// ([`AttrName::KeyOf`]).
    UnlessKeysOf(&'s [String]),
    /// None: the write removes the attribute.
    Nothing,
}

impl Adds<'_> {
    fn to(self, project: &resolver::ProjectDecls, class: &str) -> bool {
        match self {
            Self::Any => true,
            Self::UnlessKeysOf(sources) => {
                !(sources.iter()).all(|s| project.keys_exist_in(s, class))
            }
            Self::Nothing => false,
        }
    }
}

/// `obj.<attr>` with project-class `obj` (or `obj["key"]` with a `TypedDict` `obj`).
#[derive(Debug, Clone)]
struct ObjAttr {
    /// The project classes `obj` may be an instance of.
    classes: Vec<String>,
    /// The attribute (the key of a `TypedDict`).
    attr: String,
    /// The variable is the load itself (not a copy / phi of it).
    direct: bool,
    /// The parameters `obj` may be (as itself).
    obj_params: u64,
    /// `obj` is an instance the function constructs (`TypedFieldWriteSite::fresh`).
    fresh: bool,
    /// Where `obj` comes from (`MUT208`).
    reach: Reach,
    /// `obj` itself, unless phi inputs load the attribute from different objects.
    holder: Option<VarId>,
}

/// An entry (at any depth) of the container `obj.<attr>` holds: `obj.attr["k"]`,
/// `obj.attr["k"][i]`, through copies and agreeing phis.
#[derive(Debug, Clone, PartialEq)]
struct ItemOf {
    /// The project classes of the object holding the container.
    classes: Vec<String>,
    attr: String,
    /// The first key below the attribute (`TypedFieldWriteSite::key`).
    key: Option<String>,
    obj_params: u64,
    fresh: bool,
    reach: Reach,
    holder: Option<VarId>,
}

/// What every value a function binds to one of its closure cells is, as its alias maps join
/// them (a read of the cell in a nested scope is a phi of the bindings): the container an
/// attribute of an object holds, of a project class or of unknown class, an entry of one, or the
/// namespace of an object (`obj.__dict__`). The object is none of the nested scope's own: no
/// parameter of it, constructed nowhere in it.
struct CellAlias {
    attr: Option<ObjAttr>,
    item: Option<ItemOf>,
    dict: Option<Owner>,
    /// The attribute of an object of unknown class whose container (or an entry of it, `true`)
    /// every binding holds ([`Extractor::untyped_container`]).
    untyped: Option<(String, bool)>,
}

/// The [`CellAlias`] of the cells the functions of one module summarized so far bind. The
/// builder numbers a function before the scopes nested in it, so the functions enclosing a
/// scope are summarized before it.
type CellAliases = FxHashMap<CellKey, CellAlias>;

/// The object a typed write changes, as its write site records it.
#[derive(Debug, Clone, Copy)]
struct Written {
    /// `TypedFieldWriteSite::obj_params`.
    params: u64,
    /// `TypedFieldWriteSite::fresh`.
    fresh: bool,
    /// Where it comes from (`MUT208`).
    reach: Reach,
    /// The variable holding it, for its construction query (`None`: no single variable).
    holder: Option<VarId>,
}

impl Owner {
    fn written(&self) -> Written {
        Written {
            params: self.may_params,
            fresh: self.fresh,
            reach: self.reach,
            holder: self.var,
        }
    }
}

impl ObjAttr {
    fn written(&self) -> Written {
        Written {
            params: self.obj_params,
            fresh: self.fresh,
            reach: self.reach,
            holder: self.holder,
        }
    }
}

impl ItemOf {
    fn written(&self) -> Written {
        Written {
            params: self.obj_params,
            fresh: self.fresh,
            reach: self.reach,
            holder: self.holder,
        }
    }
}

/// The local facts of one function, or of a class body it runs ([`Extractor::run`]).
struct Extracted {
    summary: EffectSummary,
    sites: Vec<CallSite>,
    flow: Option<FuncFlow>,
    /// [`FileSummary::reference_sites`].
    references: Vec<CallSite>,
    /// [`FileSummary::external_accesses`].
    external: Vec<ExternalAccess>,
    /// The [`CellAlias`] of the closure cells the function binds.
    cells: Vec<(CellKey, CellAlias)>,
    /// The instructions that may remove a kept value ([`OptInFacts::kept_values`]).
    kept_points: Vec<cache_flow::KeptPoint>,
    /// The values of the function's variables ([`TypeCtx::infer`]).
    types: FxHashMap<VarId, Val>,
    /// [`FileSummary::blind_calls`].
    blind: Vec<BlindCall>,
    /// [`FileSummary::unbound_calls`].
    unbound: Vec<UnboundCall>,
    /// The calls importing a module by name ([`FileSummary::imports`]).
    imports: Vec<ImportSite>,
    /// [`FileSummary::unawaited_handoffs`].
    unawaited: Vec<UnawaitedHandoff>,
    /// The local fact of [`crate::OptInTables::fresh_returns`].
    fresh_return: Option<fresh::FreshReturn>,
    /// [`Extractor::class_runs`].
    class_runs: FxHashMap<FunctionId, OnEveryReturn>,
}

/// The points of a function that read one `self` attribute ([`Extractor::self_read_points`]).
#[derive(Default)]
struct SelfReadPoints<'a> {
    /// The loads whose value is read as a whole.
    whole: Vec<Position>,
    /// Per constant key, the item loads of a value read only through such loads.
    keyed: BTreeMap<&'a str, Vec<Position>>,
}

/// A typed write site waiting for the answer of its construction query.
struct PendingSite {
    site: TypedFieldWriteSite,
    query: usize,
    rebinds: bool,
}

struct Extractor<'a> {
    f: &'a IrFunction,
    shape: &'a FnShape<'a>,
    key: &'a FuncKey,
    cx: &'a TypeCtx<'a>,
    path: &'a Path,
    types: FxHashMap<VarId, Val>,
    /// The types of the function's values with their arguments (`list[Model]`, `set[str]`).
    inferred: std::rc::Rc<crate::infer::Inferred>,
    /// The values `isinstance` checks give variables in the blocks they guard.
    narrowed: crate::infer::NarrowedVals,
    /// The classes each `isinstance` check names, by its second argument.
    isinstance_classes: FxHashMap<VarId, Vec<String>>,
    /// Defining instruction of each variable.
    defs: FxHashMap<VarId, &'a Inst>,
    /// The blocks a run can reach and the edges it can take.
    live: ownership::Live,
    /// Instructions using each variable as an operand.
    uses: FxHashMap<VarId, Vec<&'a InstKind>>,
    /// The variables a phi or a block's terminator reads (`return v`, a branch on `v`).
    flow_uses: OnceCell<FxHashSet<VarId>>,
    /// The names the function binds itself ([`resolver::TypeCtx::local_names`]).
    local_names: OnceCell<resolver::LocalNames>,
    self_vars: FxHashSet<VarId>,
    param_alias: FxHashMap<VarId, (u8, Option<String>)>,
    /// One-level slots of parameters: `p.a` / `p["k"]` of parameter `p` itself, through copies
    /// and agreeing phis (`EffectSummary::inplace_param_slots`).
    param_slot_alias: FxHashMap<VarId, (u8, Slot)>,
    /// Shallow copies of a parameter, or of the object `p.a` holds (`copy.copy(p)`, `dict(p)`,
    /// `p.copy()`, `{**p}`), through copies and agreeing phis: a new object holding what the
    /// original holds, so a change below its top level changes the parameter, as one below
    /// `param_alias`'s entry would, and a change of its own top level does not.
    param_copy_alias: FxHashMap<VarId, (u8, Option<String>)>,
    /// The instructions (block, index) that write an attribute of a parameter (or of a shallow
    /// copy of one) only to fill a memo or to initialize it lazily (`memo_param_paths`).
    memo_fills: FxHashSet<(BlockId, usize)>,
    /// `self.<attr>` aliases: `var -> attr`.
    self_attr_alias: FxHashMap<VarId, String>,
    /// `obj.<attr>` with project-class `obj`.
    obj_attr_alias: FxHashMap<VarId, ObjAttr>,
    /// What the closure cells of the functions enclosing this one hold.
    cell_aliases: &'a CellAliases,
    /// `x.__dict__` -> owner of `x`.
    dict_of: FxHashMap<VarId, Owner>,
    /// Entries of the containers that project-class attributes hold (`WriteKind::ItemMut`),
    /// decided by [`Self::build_entry_aliases`].
    item_alias: FxHashMap<VarId, ItemOf>,
    /// Parameters (bit `i`: parameter `i < 64`) each variable may be, through copies and phis
    /// (any input): the may-alias counterpart of `param_alias`'s `(i, None)` entries.
    may_params: FxHashMap<VarId, u64>,
    /// The value each instruction's result is the object of, as itself ([`Self::same_object`]).
    same_objects: FxHashMap<VarId, VarId>,
    /// The values a call handed back as itself (`typing.cast`, a project function returning its
    /// parameter), directly or through the same objects and phis (any input): a type checker
    /// types them by that call, not by the annotation of what was handed in.
    retyped: FxHashSet<VarId>,
    /// Instances the function constructs by calling a project class (`Model(...)`,
    /// `cls(...)`), through copies and phis whose every input is one (`a = A(x) if c else
    /// A(y)`): `TypedFieldWriteSite::fresh`.
    fresh: FxHashSet<VarId>,
    /// Attributes the function assigns or deletes on any object.
    stored_attrs: Vec<&'a str>,
    /// The inputs (`GuardSubject` as `(param, attr)`) whose object the function changes in
    /// place somewhere, so its state at a later test is not the caller's ([`Self::changed_inputs`]).
    changed_inputs: FxHashSet<(u8, Option<String>)>,
    /// The block and phi defining each phi variable (`GuardInputs::phis`).
    phis: OnceCell<FxHashMap<VarId, (BlockId, &'a PhiNode)>>,
    /// The function's integer constants (`capacity`), built on first use.
    ints: OnceCell<heki_ir::FnIndex<'a>>,
    /// What the queue capacity walks of the function's values found so far.
    capacity_walks: RefCell<capacity::Walks>,
    /// The loops around each block.
    around: OnceCell<Vec<Vec<&'a NaturalLoop>>>,
    /// The positions where the function certainly holds a lock ([`Self::lock_held_at`]).
    lock_held: OnceCell<FxHashSet<Position>>,
    /// Where the function certainly holds thread locks (`EFF117`, [`Self::thread_lock_points`]).
    thread_locks: OnceCell<guarded::LockPoints>,
    /// Where the function runs shielded from cancellation (`EFF118`, [`Self::shield_here`]).
    shields: OnceCell<cancellation::ShieldPoints>,
    /// The budget of the walks after the function's re-delivery checkpoints (`EFF118`, its own:
    /// they never spend the budget of the walks other rules read).
    redelivery_budget: OnceCell<Budget>,
    /// The blocks acquiring each lock (`locks::BlockTests::common`).
    lock_blocks: std::collections::BTreeMap<ObjRef, BTreeSet<BlockId>>,
    /// The points that may take a lock (a `with`, an `acquire()`) every normal return has
    /// passed (filled with [`Self::must_run`], `LockAcquire::on_every_return`).
    takes_on_every_return: FxHashSet<(BlockId, usize)>,
    /// The tests of state on every path to each block (`EFF109`).
    state_tests: OnceCell<locks::BlockTests>,
    guards: LocalGuards,
    summary: EffectSummary,
    sites: Vec<CallSite>,
    /// Resolved callee of every `Call` / `MethodCall` by position (for the iteration flow).
    callees: FxHashMap<(BlockId, usize), Callee>,
    /// Whether each call of a held callable (a bound method, `m = obj.method; m(...)`, or a
    /// `functools.partial` object made here), which resolves past the type of the value it
    /// calls, has as its targets every implementation it can run, by position
    /// ([`Self::targets_complete_at`]).
    held_calls_complete: FxHashMap<Position, bool>,
    /// The project calls (`Call` / `MethodCall`), for the sites of the objects they return.
    project_calls: Vec<ProjectCall>,
    contracts: Option<&'a dyn Contracts>,
    /// Call results and attribute reads the function may return, with the step from each to the
    /// returned value (`MUT202` return flow).
    returned: FxHashMap<VarId, ReturnStep>,
    /// Position (block, instruction index) of the instruction being visited.
    pos: (BlockId, usize),
    /// The class line of the method's own class (`Self::own_line`).
    own_line: OnceCell<Option<SharedLine>>,
    /// How many lambdas the function makes (`Self::shares_its_name`).
    lambdas: OnceCell<usize>,
    /// The points `MUT208` describes (`Some` when an `invalidation` contract is configured).
    events: Option<Events>,
    /// Contracts naming this function an exempt writer (`MUT208`), matched at its first write
    /// to a contract target.
    exempt: Option<u64>,
    /// Where each variable's object comes from among the objects that existed before the call
    /// (`MUT208`; empty unless an `invalidation` contract is configured).
    reach: FxHashMap<VarId, Reach>,
    /// The module globals and closure cells each value may be reached from
    /// ([`Self::global_roots`]), on the function's tables and budget.
    roots: Closure<VarId, KeySet<Root<'a>>>,
    /// The ways each mapping a call reads the entries of may be built, by its copy root
    /// ([`Self::mapping`]).
    mappings: RefCell<FxHashMap<VarId, keywords::Ways>>,
    /// The budget of the walks over the function's values ([`Self::global_roots`],
    /// [`Self::mapping`], [`Self::element_paths`], [`Self::entry_params`]) and of the mapping
    /// entries calls bind ([`Self::keyword_ways`]).
    walk_budget: Budget,
    /// What the function may rebind (set by `build_aliases`).
    rebound: OnceCell<Rebound<'a>>,
    /// The parameter and copy aliases of the values whose items the function stores, deletes,
    /// or adds ([`Self::keeps_items`]).
    items_changed: OnceCell<FxHashSet<(u8, Option<String>)>>,
    /// The parameters whose items or elements each value may be, or be below
    /// ([`Self::element_paths`]), on the function's tables and budget.
    elements: ElementWalks<KeySet<(u8, String)>>,
    /// The parameters whose entries each value may be as part of the container
    /// ([`Self::entry_params`]), on the function's tables and budget.
    entries: ElementWalks<Or>,
    /// Allocations, escapes, and construction queries (`construction`).
    rec: Recorder,
    pending_sites: Vec<PendingSite>,
    /// `(call site, callee, parameter, query)`: arguments waiting for `ArgOrigin::new_object`.
    pending_args: Vec<(usize, usize, usize, usize)>,
    /// The origins of a callee's parameters while a call binds them, kept from call to call:
    /// each bound callee takes a copy of its exact length, so no call allocates room it then
    /// gives back (`release_spare_capacity`).
    param_origins: Vec<ArgOrigin>,
    /// Unresolved writes waiting for the answer of their construction query.
    pending_unresolved: Vec<(UnresolvedWriteSite, usize)>,
    /// Whether the construction queries can be answered with anything but "no": the function
    /// allocates (a call returning a project-class instance, a copy), or it is a private helper
    /// (its writes to its parameters are reported at callers that may hand it new objects), or
    /// has parameters an effect boundary judges by what the calls reaching it hand them
    /// ([`Contracts::judges_handed_objects`]).
    track: bool,
    /// Variables that may hold what a call returns that may be a new object (a superset of the
    /// allocations `Recorder` learns while visiting), through copies and phis: only they and the
    /// parameters are asked about.
    maybe_new: FxHashSet<VarId>,
    /// The items of the tuple and list displays an assignment only unpacks (`a, b = X(), Y()`),
    /// each with the element of the display it is, and those displays: what binds the targets
    /// is the elements, the display is never seen ([`Self::build_unpacked_displays`]).
    unpacked: FxHashMap<VarId, VarId>,
    unpacked_displays: FxHashSet<VarId>,
    /// The stores into containers that may be ones a constructor made ([`Escape::Added`]), by
    /// position, for the check after the visit of whether they hand the value to code of the
    /// project.
    added: FxHashMap<crate::construction::Pos, AddedBy>,
    /// What a construction of each project class keeps of its instance, decided once per class
    /// for the function ([`Self::constructor`]).
    constructors: RefCell<FxHashMap<Box<str>, Constructor<'a>>>,
    /// [`Contracts::ownership`]: the facts only ownership contracts read are computed.
    ownership: bool,
    /// The opt-in facts the enabled rules read.
    opt_in: OptInFacts,
    /// Per call instruction, whether the function has made it whenever it returns normally
    /// (filled before the visit when an enabled rule reads [`EffectSummary::must_exec`]).
    must_run: FxHashMap<(BlockId, usize), OnEveryReturn>,
    /// Per class body a `class` statement of the function runs, [`Self::must_run`] of that
    /// statement as a call of the body (filled with it).
    class_runs: FxHashMap<FunctionId, OnEveryReturn>,
    /// [`KeyFacts`], built on first use.
    key_facts: OnceCell<KeyFacts<'a>>,
    /// The changes of checked containers of `self` the visit finds, recorded after it
    /// ([`Self::self_container_changes`]).
    self_changes: Vec<SelfChange>,
    /// [`Extractor::builtin_method_runs`]'s answers.
    builtin_methods: FxHashMap<(String, String, bool), bool>,
    /// [`Contracts::references`]: the references handing a target on are extracted.
    references: bool,
    /// `(call, argument)`: a callable a library API runs (`Self::handed_callable`), and the one
    /// each `functools.partial` object it is holds, by the call handing it over.
    handed: FxHashSet<(VarId, VarId)>,
    /// The accesses outside the project that effect boundaries forbid
    /// (`FileSummary::external_accesses`).
    external: Vec<ExternalAccess>,
    /// [`Extracted::kept_points`].
    kept_points: Vec<cache_flow::KeptPoint>,
    /// Per variable, what the function establishes of its value's class
    /// ([`Self::established_values`]), built on first use.
    established: OnceCell<Vec<Built>>,
    /// The span and argument position of each callable a call is handed that has a project site
    /// of its own ([`Self::as_handed`]), built on first use.
    followed_callables: OnceCell<FxHashSet<(Span, u8)>>,
    /// The calls heki is blind to (`OptInTables::blind_spots`), with
    /// [`OptInFacts::blind_spots`].
    blind: Vec<BlindCall>,
    /// The calls heki binds to no project code ([`FileSummary::unbound_calls`]), with
    /// [`Contracts::any_callee_arguments`].
    unbound: Vec<UnboundCall>,
    /// The calls importing a module by name, with [`OptInFacts::imports`].
    imports: Vec<ImportSite>,
    /// The failures of the by-name imports the function's handlers take, built on first use.
    import_handlers: OnceCell<crate::imports::ImportHandlers<'a, 'a>>,
    /// The project functions handed to a library call that calls them without awaiting what
    /// they return (`OptInTables::unawaited_handoffs`), with
    /// [`OptInFacts::unawaited_handoffs`].
    unawaited: Vec<UnawaitedHandoff>,
}

/// A key of a container as [`Extractor::key_present`] compares keys: the same variable through
/// copies, or the same constant (`self.items["k"]`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum KeyOf<'a> {
    Var(VarId),
    Const(&'a ConstLiteral),
}

/// The tests and reads of keys of the containers attributes of `self` hold, by attribute
/// ([`Extractor::key_present`]).
struct KeyFacts<'a> {
    /// Per key, the blocks dominated by a side of a branch on which the container holds the key,
    /// when the branch alone enters that side.
    sides: FxHashMap<String, FxHashMap<KeyOf<'a>, heki_ir::Dominated>>,
    /// The index of the first read of each key in each block.
    reads: FxHashMap<String, FxHashMap<(BlockId, KeyOf<'a>), usize>>,
}

/// A change of the checked container (`MUT113`) that `var` holds, `self.<attr>`.
struct SelfChange {
    var: VarId,
    attr: String,
    change: ContainerChange,
    span: Span,
}

/// The stores of a function to the attributes of `self` its changes change, and its reads of
/// attributes of `self` ([`Extractor::rebound_before`]).
struct Rebindings<'a> {
    stores: FxHashMap<&'a str, Rebinding>,
    /// The attribute each read reads, and where.
    reads: FxHashMap<VarId, (&'a str, Position)>,
}

/// The stores of a function to one attribute of `self`.
struct Rebinding {
    /// Whether every store is of a new object.
    all_new: bool,
    /// The index of the first store in each block.
    first_in_block: FxHashMap<BlockId, usize>,
    /// The blocks the stores' blocks dominate.
    dominated: heki_ir::Dominated,
}

/// A project call, for the sites of the object it returns (`SiteRole`).
/// A point of a function: (block, instruction index).
type Position = (BlockId, usize);

struct ProjectCall {
    /// Index of its `Call` site in `Extractor::sites`.
    site: usize,
    /// Where the call is.
    at: Position,
    /// The variable it defines.
    dst: VarId,
    /// Each callee with the parameter each argument variable binds.
    params: Vec<(FuncKey, Vec<(u8, VarId)>)>,
    /// The receiver its callees bind to their first parameter (a method's `self`, a property's
    /// object), which `params` leaves out.
    recv: Option<VarId>,
}

/// The object a value holds ([`CallObjects::object_of`]): a value, or an attribute read of one.
type Object<'s> = (VarId, Option<&'s str>);

/// The project calls of a function by span, and the object each of its values holds, for the
/// objects of a `ResultIteration`: built once per function, each value's object found once.
struct CallObjects<'x> {
    calls_at: FxHashMap<Span, Vec<&'x ProjectCall>>,
    /// The values a project call returns: a property read among them runs a getter, which may
    /// return a new object at each read.
    results: FxHashSet<VarId>,
    defs: &'x FxHashMap<VarId, &'x Inst>,
    shape: &'x FnShape<'x>,
    types: &'x FxHashMap<VarId, Val>,
    /// The method's own `self`, which `super()` is.
    self_var: Option<VarId>,
}

impl<'x> CallObjects<'x> {
    fn new(ex: &'x Extractor<'_>) -> Self {
        let mut calls_at: FxHashMap<Span, Vec<&'x ProjectCall>> = FxHashMap::default();
        for call in &ex.project_calls {
            calls_at
                .entry(ex.sites[call.site].span)
                .or_default()
                .push(call);
        }
        Self {
            calls_at,
            results: ex.project_calls.iter().map(|c| c.dst).collect(),
            defs: &ex.defs,
            shape: ex.shape,
            types: &ex.types,
            self_var: ex.f.params.first().map(|p| p.var),
        }
    }

    /// The iteration `between` steps the results of `calls`, with the objects of the loop's calls
    /// that the iterated calls bind to a parameter (numbered by their first loop call).
    fn iteration(&self, between: BetweenSteps, calls: &[(Span, &ProjectCall)]) -> ResultIteration {
        let arguments: Vec<Vec<(u8, Object<'_>)>> = calls
            .iter()
            .map(|(_, call)| {
                let bound = call
                    .params
                    .iter()
                    .flat_map(|(_, bound)| bound.iter().copied());
                let all = call.recv.map(|r| (0, r)).into_iter().chain(bound);
                all.filter(|(p, _)| *p < 64)
                    .map(|(p, v)| (p, self.object_of(v)))
                    .collect()
            })
            .collect();
        let bound: FxHashSet<Object<'_>> = arguments.iter().flatten().map(|(_, o)| *o).collect();
        let mut numbers: FxHashMap<Object<'_>, u32> = FxHashMap::default();
        let receivers: Vec<Option<u32>> = between
            .calls
            .iter()
            .map(|span| {
                let at = self.calls_at.get(span).into_iter().flatten();
                let firsts = at.filter_map(|c| {
                    c.recv.or_else(|| {
                        c.params
                            .iter()
                            .flat_map(|(_, bound)| bound)
                            .find(|(p, _)| *p == 0)
                            .map(|(_, v)| *v)
                    })
                });
                let object = firsts
                    .map(|v| self.object_of(v))
                    .find(|o| bound.contains(o))?;
                let next = u32::try_from(numbers.len()).unwrap_or(u32::MAX);
                Some(*numbers.entry(object).or_insert(next))
            })
            .collect();
        let mut results: Vec<IteratedResult> = calls
            .iter()
            .zip(arguments)
            .map(|((call, _), arguments)| IteratedResult {
                call: *call,
                arguments: arguments
                    .into_iter()
                    .filter_map(|(p, o)| Some((p, *numbers.get(&o)?)))
                    .collect(),
            })
            .collect();
        results.sort();
        let receivers = if receivers.iter().all(Option::is_none) {
            Vec::new()
        } else {
            receivers
        };
        ResultIteration {
            between,
            receivers,
            results,
        }
    }

    /// The object `var` holds, as far as the function's code tells: the value it copies, or an
    /// attribute read of one that runs no property getter (`self.registry` read twice is one
    /// object, as the containers `MUT113` checks are attributes of `self`).
    fn object_of(&self, var: VarId) -> Object<'x> {
        let value = self.copied_value(var);
        match self.defs.get(&value).map(|inst| &inst.kind) {
            Some(InstKind::AttrLoad { obj, attr, .. }) if !self.results.contains(&value) => {
                (self.copied_value(*obj), Some(attr.as_str()))
            }
            _ => (value, None),
        }
    }

    /// The value `var` copies, through `Copy`; `super()` is the method's own `self`. A phi merges
    /// different values.
    fn copied_value(&self, var: VarId) -> VarId {
        let root = self.shape.copy_root(var);
        match (self.types.get(&root), self.self_var) {
            (Some(Val::Super(_)), Some(first)) => first,
            _ => root,
        }
    }
}

/// The executor class that runs what it is handed in other processes.
const PROCESS_POOL: &str = "concurrent.futures.ProcessPoolExecutor";

/// Whether a decorator of `f` is one of `names` ([`ModuleDecls::decorator_is`]).
/// The builtin and standard library containers whose own methods and item stores change them in
/// place (`EffectSummary::param_inplace_sites`).
const BUILTIN_CONTAINERS: &[&str] = &[
    "builtins.list",
    "builtins.dict",
    "builtins.set",
    "builtins.bytearray",
    "collections.deque",
    "collections.OrderedDict",
    "collections.defaultdict",
    "collections.Counter",
];

fn decorated_as(module: &ModuleDecls, f: &IrFunction, names: &[&str]) -> bool {
    f.decorators.iter().any(|d| module.decorator_is(d, names))
}

/// Whether `text` is a dotted name (`get`, `handler`, `self.on_done`).
fn is_dotted_name(text: &str) -> bool {
    text.split('.').all(|part| {
        part.chars()
            .next()
            .is_some_and(|c| c == '_' || c.is_alphabetic())
            && part.chars().all(|c| c == '_' || c.is_alphanumeric())
    })
}

impl<'a> Extractor<'a> {
    fn new(
        shape: &'a FnShape<'a>,
        key: &'a FuncKey,
        cx: &'a TypeCtx<'a>,
        path: &'a Path,
        contracts: Option<&'a dyn Contracts>,
        opt_in: OptInFacts,
        cell_aliases: &'a CellAliases,
    ) -> Self {
        let f = shape.f;
        let (types, narrowed, isinstance_classes, inferred) = cx.infer_flow(f);
        let mut defs = FxHashMap::default();
        let mut uses: FxHashMap<VarId, Vec<&InstKind>> = FxHashMap::default();
        let mut stored_attrs = Vec::new();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(d) = inst.kind.dst() {
                defs.insert(d, inst);
            }
            if let InstKind::AttrStore { attr, .. } | InstKind::AttrDelete { attr, .. } = &inst.kind
            {
                stored_attrs.push(attr.as_str());
            }
            for op in inst.kind.operands() {
                uses.entry(op).or_default().push(&inst.kind);
            }
        }
        let is_method =
            f.owner_class_qualname.is_some() && !f.has_decorator(&["staticmethod", "classmethod"]);
        let self_vars = match (is_method, f.params.first()) {
            (true, Some(p)) => self_aliases(f, p.var),
            _ => FxHashSet::default(),
        };
        let summary = EffectSummary {
            is_async: matches!(f.kind, FunctionKind::Async | FunctionKind::AsyncGenerator),
            is_generator: is_generator_function(f, cx.module),
            is_cached_property: decorated_as(cx.module, f, &["cached_property"]),
            kept: opt_in
                .kept_values
                .then(|| {
                    f.decorators.iter().find_map(|d| {
                        let symbol = cx.module.decorator_symbol(d)?;
                        let listed = cx.kb.cached_property(&symbol)?;
                        Some(ListedCache {
                            in_slot: listed.slot.is_some(),
                        })
                    })
                })
                .flatten()
                .map(|listed| {
                    Box::new(KeptFacts {
                        listed_cache: Some(listed),
                        ..KeptFacts::default()
                    })
                }),
            is_lru_cache: decorated_as(cx.module, f, &["lru_cache", "cache"]),
            owner_class_qualname: f.owner_class_qualname.clone(),
            decorators: f.decorators.clone(),
            decorator_may_replace: !keeps_returned_value(&f.decorators, cx.module),
            // `eval`, `exec`, `locals()`, frame introspection (Assumption A1): code heki cannot
            // follow runs here, or reads and changes this frame's state.
            unknown_calls: !f.a1_hazards.is_empty(),
            override_roles: contracts
                .map(|c| c.override_roles(key, cx.project))
                .filter(|roles| *roles != OverrideRoles::default())
                .map(Box::new),
            ..EffectSummary::default()
        };
        let live = ownership::Live::of(f, &defs);
        let scopes = narrowed.scopes();
        let mut ex = Self {
            f,
            shape,
            key,
            cx,
            path,
            types,
            narrowed,
            isinstance_classes,
            inferred,
            defs,
            live,
            uses,
            flow_uses: OnceCell::new(),
            local_names: OnceCell::new(),
            self_vars,
            param_alias: FxHashMap::default(),
            param_slot_alias: FxHashMap::default(),
            param_copy_alias: FxHashMap::default(),
            memo_fills: FxHashSet::default(),
            self_attr_alias: FxHashMap::default(),
            obj_attr_alias: FxHashMap::default(),
            cell_aliases,
            dict_of: FxHashMap::default(),
            item_alias: FxHashMap::default(),
            may_params: FxHashMap::default(),
            same_objects: FxHashMap::default(),
            retyped: FxHashSet::default(),
            fresh: FxHashSet::default(),
            stored_attrs,
            changed_inputs: FxHashSet::default(),
            phis: OnceCell::new(),
            ints: OnceCell::new(),
            capacity_walks: RefCell::default(),
            around: OnceCell::new(),
            lock_blocks: std::collections::BTreeMap::new(),
            takes_on_every_return: FxHashSet::default(),
            lock_held: OnceCell::new(),
            thread_locks: OnceCell::new(),
            shields: OnceCell::new(),
            redelivery_budget: OnceCell::new(),
            state_tests: OnceCell::new(),
            guards: LocalGuards::default(),
            summary,
            sites: Vec::new(),
            callees: FxHashMap::default(),
            held_calls_complete: FxHashMap::default(),
            project_calls: Vec::new(),
            own_line: OnceCell::new(),
            lambdas: OnceCell::new(),
            contracts,
            returned: FxHashMap::default(),
            pos: (BlockId(0), 0),
            events: contracts
                .filter(|c| c.invalidations() > 0)
                .map(|_| Events::default()),
            exempt: None,
            reach: FxHashMap::default(),
            roots: Closure::new(),
            mappings: RefCell::default(),
            walk_budget: Budget::of_function("summary::walks", f),
            rebound: OnceCell::new(),
            items_changed: OnceCell::new(),
            elements: ElementWalks::new(scopes),
            entries: ElementWalks::new(scopes),
            rec: Recorder::default(),
            pending_sites: Vec::new(),
            pending_args: Vec::new(),
            param_origins: Vec::new(),
            pending_unresolved: Vec::new(),
            track: false,
            maybe_new: FxHashSet::default(),
            unpacked: FxHashMap::default(),
            unpacked_displays: FxHashSet::default(),
            added: FxHashMap::default(),
            constructors: RefCell::default(),
            ownership: false,
            opt_in,
            must_run: FxHashMap::default(),
            class_runs: FxHashMap::default(),
            key_facts: OnceCell::new(),
            self_changes: Vec::new(),
            builtin_methods: FxHashMap::default(),
            references: opt_in.suggestions || contracts.is_some_and(|c| c.references()),
            handed: FxHashSet::default(),
            external: Vec::new(),
            kept_points: Vec::new(),
            established: OnceCell::new(),
            followed_callables: OnceCell::new(),
            blind: Vec::new(),
            unbound: Vec::new(),
            imports: Vec::new(),
            import_handlers: OnceCell::new(),
            unawaited: Vec::new(),
        };
        ex.ownership = contracts.is_some_and(|c| c.ownership());
        let handed = contracts.is_some_and(|c| c.judges_handed_objects()) && !f.params.is_empty();
        ex.track = ex.ownership && (ex.may_construct() || handed);
        ex.build_same_objects();
        ex.build_may_params();
        ex.build_fresh();
        if ex.events.is_some() {
            ex.build_reach();
        }
        ex.build_aliases();
        ex.changed_inputs = ex.changed_inputs();
        ex.find_new_cells();
        ex.memo_fills = ex.memo_fills();
        let class_of = |v: VarId| match ex.types.get(&v) {
            Some(Val::Inst(class, _)) => Some(class.as_str()),
            _ => None,
        };
        if ex.summary.is_lru_cache {
            let module = ex.cx.module;
            ex.summary.cached_result =
                readonly::cached_result(f, module, &ex.defs, &ex.live, class_of);
        }
        let returned = readonly::returned_values(
            shape,
            &ex.defs,
            &ex.live,
            |kind| ex.derivations(kind),
            class_of,
        );
        ex.returned = returned;
        ex.summary.typed_params = (0..64u32)
            .zip(&f.params)
            .filter(|(_, p)| ex.project_class(p.var).is_some())
            .fold(0, |acc, (i, _)| acc | 1u64 << i);
        ex
    }

    fn guard_inputs(&self) -> GuardInputs<'_, 'a> {
        GuardInputs {
            f: self.f,
            shape: self.shape,
            defs: &self.defs,
            uses: &self.uses,
            types: &self.types,
            isinstance_classes: &self.isinstance_classes,
            param_alias: &self.param_alias,
            param_slot_alias: &self.param_slot_alias,
            stored_attrs: &self.stored_attrs,
            changed_inputs: &self.changed_inputs,
            fresh: &self.fresh,
            phis: &self.phis,
            kb: self.cx.kb,
            project: self.cx.project,
        }
    }

    /// The inputs whose items the function changes in place: an item store or delete, an
    /// in-place operator, or a call of a method that adds or removes items, on the parameter or
    /// an attribute of it. A truthiness test of one of them may find at the test another state
    /// than the caller's; a `None`, `isinstance`, or `hasattr` test is decided otherwise
    /// (`GuardInputs::subject_of`).
    fn changed_inputs(&self) -> FxHashSet<(u8, Option<String>)> {
        let input = |v: &VarId| match (self.param_alias.get(v), self.param_slot_alias.get(v)) {
            (Some((i, None)), _) => Some((*i, None)),
            (_, Some((i, Slot::Attr(a)))) => Some((*i, Some(a.clone()))),
            _ => None,
        };
        let insts = self.f.blocks.iter().flat_map(|b| &b.insts);
        insts
            .filter_map(|inst| match &inst.kind {
                InstKind::SubscriptStore { obj, .. } | InstKind::SubscriptDelete { obj, .. } => {
                    input(obj)
                }
                InstKind::BinOp {
                    lhs,
                    in_place: true,
                    ..
                } => input(lhs),
                InstKind::MethodCall { recv, method, .. }
                    if guards::CHANGING_METHODS.contains(&method.as_str()) =>
                {
                    input(recv)
                }
                _ => None,
            })
            .collect()
    }

    /// The loops around `bb`, from a table built on first use.
    fn loops_around(&self, bb: BlockId) -> &[&'a NaturalLoop] {
        let f = self.f;
        let table = self.around.get_or_init(|| {
            let mut table: Vec<Vec<&'a NaturalLoop>> = vec![Vec::new(); f.blocks.len()];
            for lp in &f.loops {
                for b in &lp.body {
                    table[b.0 as usize].push(lp);
                }
            }
            table
        });
        &table[bb.0 as usize]
    }

    /// [`ArgBinding`] of the argument `var` of a call inside `loops`.
    fn binding(&self, var: VarId, loops: &[&NaturalLoop]) -> ArgBinding {
        let cx = self.guard_inputs();
        let mut b = self.guards.binding(&cx, var, loops);
        let mut fact = self.arg_fact(var);
        // The string constants of classes are resolved for the keys ownership contracts read;
        // looking them up for every argument would make every call depend on the classes of
        // its arguments.
        if self.ownership && fact.literal.is_none() {
            fact.literal = self.resolved_str(var).map(heki_kb::LiteralValue::Str);
        }
        let kb = self.cx.kb;
        if let Some(c) = self.guards.constant(&cx, var) {
            fact.class = Some(c.class.name().to_owned());
            fact.exact = true;
            fact.truthy = c.truthy;
        } else if fact.class.as_deref().is_some_and(|c| !kb.decides_class(c)) {
            fact.class = None;
        }
        fact.flags.retain(|f| kb.decides_flag(f));
        b.fact = (fact != heki_kb::ArgFact::default()).then(|| Box::new(fact));
        b.from_params = self.opt_in.must_effects
            && resolver::from_parameter(
                &|d| self.defs.get(&d).copied(),
                &|x, next| self.phi_inputs(x, next),
                &|x| self.is_param(x),
                var,
            );
        b
    }

    /// What the function knows of `var` as an argument of a catalogued call
    /// ([`resolver::arg_fact`]).
    fn arg_fact(&self, var: VarId) -> heki_kb::ArgFact {
        let name_of = |n| match self.val(n)? {
            Val::Ref(fqn) => Some(fqn.clone()),
            _ => None,
        };
        resolver::arg_fact(
            |d| self.defs.get(&d).copied(),
            |c| self.instance_of(c),
            name_of,
            &|inst| self.cx.module_constant(inst, &name_of),
            &|inst| self.cx.names_file_constant(inst, &name_of),
            var,
        )
    }

    /// `(callee, parameter, capacity)` of the argument `var` a call binds exactly to parameter
    /// `slot` of `callee`, when it is a queue (`SiteQueues::handed`).
    fn handed_queue(
        &self,
        callee: &FuncKey,
        slot: usize,
        var: VarId,
    ) -> Option<(FuncKey, u8, CapacityOf)> {
        let queue = matches!(self.types.get(&var), Some(Val::Inst(c, _)) if capacity::is_queue_class(self.cx.kb, c));
        let slot = u8::try_from(slot).ok().filter(|_| queue)?;
        Some((callee.clone(), slot, self.capacity_of(var).capacity))
    }

    /// What `var` gives a queue (`capacity`): attributes and globals through the project's
    /// declarations, the parameters as keys.
    fn capacity_of(&self, var: VarId) -> capacity::Walked {
        let owner = self.cx.owner_fqn(self.f);
        let class_of = |v: VarId| {
            if self.self_vars.contains(&v) {
                return owner.clone();
            }
            match self.types.get(&v) {
                Some(Val::Inst(c, _)) if self.cx.project.is_project_class(c) => Some(c.clone()),
                _ => None,
            }
        };
        capacity::ValueCapacity {
            cx: self.cx,
            f: self.f,
            key: self.key,
            defs: &self.defs,
            phis: guards::phi_table(self.f, &self.phis),
            ints: &self.ints,
            class_of: &class_of,
            queue_params: u64::MAX,
            in_decls: false,
            walks: &self.capacity_walks,
        }
        .walk(var)
    }

    /// The constant key of a subscript: its string literal, or the module-level `Final`
    /// string constant its index names.
    fn key_of(&self, key_const: Option<&str>, index: VarId) -> Option<String> {
        match key_const {
            Some(k) => Some(k.to_owned()),
            None => self.resolved_str(index),
        }
    }

    /// The key of an item write of a typed attribute's container: its constant (`key_of`), or,
    /// with an ownership contract, where a key that is no constant comes from.
    fn entry_key(&self, key_const: Option<&str>, index: VarId) -> (Option<String>, EntryKey) {
        let key = self.key_of(key_const, index);
        let from = match self.param_alias.get(&index) {
            _ if key.is_some() || !self.ownership => EntryKey::Written,
            Some((p, None)) => EntryKey::Param(*p),
            _ => EntryKey::Unknown,
        };
        (key, from)
    }

    /// [`Self::may_params`]: a monotone fixpoint over copies, in-place container operators
    /// ([`Self::same_object`]), the items of displays an assignment only unpacks, each its
    /// element ([`Self::unpacked`]), and phis, from the parameters and the reads of the
    /// enclosing functions' parameters ([`Self::captured_bit`]).
    fn build_may_params(&mut self) {
        for (i, p) in self.f.params.iter().enumerate().take(64) {
            *self.may_params.entry(p.var).or_default() |= 1u64 << i;
        }
        for inst in self.f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::LoadDeref { dst, name } = &inst.kind {
                let bit = self.captured_bit(name);
                if bit != 0 {
                    *self.may_params.entry(*dst).or_default() |= bit;
                }
            }
        }
        loop {
            let mut changed = false;
            for bb in &self.f.blocks {
                for phi in &bb.phis {
                    let bits = phi.incoming.iter().fold(0, |acc, (_, v)| {
                        acc | self.may_params.get(v).copied().unwrap_or(0)
                    });
                    changed |= or_bits(&mut self.may_params, phi.dst, bits);
                }
                self.pos = (bb.id, 0);
                for inst in &bb.insts {
                    let unpacked = |dst| self.unpacked.get(&dst).map(|elem| (dst, *elem));
                    let same = self.same_object(&inst.kind);
                    if let Some((dst, src)) = same.or_else(|| inst.kind.dst().and_then(unpacked)) {
                        let bits = self.may_params.get(&src).copied().unwrap_or(0);
                        changed |= or_bits(&mut self.may_params, dst, bits);
                    }
                }
            }
            if !changed {
                return;
            }
        }
    }

    /// `(dst, src)` of an instruction whose result is the object it is handed
    /// ([`Self::build_same_objects`]).
    fn same_object(&self, kind: &InstKind) -> Option<(VarId, VarId)> {
        let dst = kind.dst()?;
        self.same_objects.get(&dst).map(|src| (dst, *src))
    }

    /// [`Self::same_objects`] and [`Self::retyped`]. An instruction's result is the object it is
    /// handed on every path for a copy, an augmented assignment changing a built-in container in
    /// place (`items += more`, `extras |= other`), which gives the container itself (on any
    /// other value, `count += 1`, it binds a new object), and a call handing an argument back as
    /// itself: `typing.cast(T, value)` ([`ownership::handed_back`]), or a call of project
    /// functions that each return the parameter it binds there ([`Self::returned_param`]).
    fn build_same_objects(&mut self) {
        let mut handed_back = Vec::new();
        for bb in &self.f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                self.pos = (bb.id, index);
                let same = match &inst.kind {
                    InstKind::Copy { dst, src } => Some((*dst, *src)),
                    InstKind::BinOp {
                        dst,
                        op,
                        lhs,
                        in_place: true,
                        ..
                    } => self
                        .has_in_place_operator(*lhs, *op)
                        .then_some((*dst, *lhs)),
                    kind @ (InstKind::Call {
                        dst,
                        args,
                        star_from,
                        ..
                    }
                    | InstKind::MethodCall {
                        dst,
                        args,
                        star_from,
                        ..
                    }) => {
                        let symbol = match self.callee_of(kind) {
                            Callee::External(symbol) => symbol,
                            _ => String::new(),
                        };
                        let returned = || match self.returned_param(kind) {
                            Some((dst, holder, None)) => Some((dst, holder)),
                            _ => None,
                        };
                        let call = ownership::handed_back(&symbol, args, *star_from)
                            .map(|v| (*dst, v))
                            .or_else(returned);
                        handed_back.extend(call.map(|(dst, _)| dst));
                        call
                    }
                    _ => None,
                };
                if let Some((dst, src)) = same {
                    self.same_objects.insert(dst, src);
                }
            }
        }
        if handed_back.is_empty() {
            return;
        }
        // Forward from the calls' results, each value once. Past the budget nothing is retyped:
        // the type checker's exclusions of `MUT110` stand.
        let mut onward: FxHashMap<VarId, Vec<VarId>> = FxHashMap::default();
        for (dst, src) in &self.same_objects {
            onward.entry(*src).or_default().push(*dst);
        }
        for phi in self.f.blocks.iter().flat_map(|b| &b.phis) {
            for (_, v) in &phi.incoming {
                onward.entry(*v).or_default().push(phi.dst);
            }
        }
        let budget = Budget::of_function("summary::build_same_objects", self.f);
        let mut retyped = FxHashSet::default();
        let walked = heki_ir::walk::reach(
            &budget,
            handed_back,
            |v, next| next.extend(onward.get(&v).into_iter().flatten()),
            |v| {
                retyped.insert(v);
                heki_ir::walk::Visit::<()>::Expand
            },
        );
        if matches!(walked, Answer::Known(_)) {
            self.retyped = retyped;
        }
    }

    /// Whether the function's parameter `i` is annotated read-only
    /// ([`crate::FunctionDecl::readonly_params`]).
    fn readonly_param(&self, i: u8) -> bool {
        (self.cx.project.function_decl(self.key))
            .is_some_and(|d| d.readonly_params & (1u64 << i) != 0)
    }

    /// The mask bit of the enclosing function's parameter that the variable `name` the function
    /// reads from it is on every path (`0`: none). A class body's code is the code of the
    /// function running it, which binds the cell itself.
    fn captured_bit(&self, name: &str) -> u64 {
        self.cx
            .captured_param(self.f, name)
            .map_or(0, |(owner, i)| {
                self.enclosing_param_bit(func_qualname(owner), i)
            })
    }

    /// The mask bit of parameter `param` of `owner`, a function of the module enclosing the
    /// summarized function or the function itself.
    fn enclosing_param_bit(&self, owner: &str, param: u8) -> u64 {
        if owner == self.key.qualname {
            return 1u64 << param;
        }
        self.cx
            .project
            .function_decl(self.key)
            .and_then(|d| d.captured_params.as_deref())
            .map_or(0, |c| c.bit(owner, param))
    }

    /// The parameter `var` is on every path, as a mask (`0`: none or not known): its own, or an
    /// enclosing function's that it reads through a closure cell, directly or through copies.
    fn exact_param(&self, var: VarId) -> u64 {
        if let Some((i, None)) = self.param_alias.get(&var) {
            return 1u64 << i;
        }
        let root = self.shape.copy_root(var);
        match self.shape.inst(root).map(|inst| &inst.kind) {
            Some(InstKind::LoadDeref { name, .. }) => self.captured_bit(name),
            _ => 0,
        }
    }

    /// Bind the parameters of enclosing functions that `callee`, declared by `decl`, has
    /// ([`resolver::CapturedParams`]) past its own `params`, to what this function holds there:
    /// its own parameter, or its position of the same one when it is nested in that function
    /// too. A call away from that function binds nothing.
    fn bind_captured(&self, callee: &FuncKey, decl: &FunctionDecl, params: &mut Vec<ArgOrigin>) {
        let Some(captured) = decl
            .captured_params
            .as_deref()
            .filter(|_| callee.module == self.key.module)
        else {
            return;
        };
        let bits = (captured.params.iter().enumerate())
            .map(|(j, (owner, param))| (j, self.enclosing_param_bit(owner, *param)));
        for (j, bit) in bits.filter(|(_, bit)| *bit != 0) {
            let at = usize::from(captured.first) + j;
            if params.len() <= at {
                params.resize(at + 1, ArgOrigin::default());
            }
            params[at].may_params = bit;
        }
    }

    /// The value of `var` in the block being read (`pos`).
    fn val(&self, var: VarId) -> Option<&Val> {
        self.val_in(self.pos.0, var)
    }

    /// [`Self::val`] in block `bb`.
    ///
    /// A check narrows the value, so a variable whose own class is already within the narrowed
    /// ones keeps it (a copy of the value typed `a: A = x`), as `Engine::need` does.
    fn val_in(&self, bb: BlockId, var: VarId) -> Option<&Val> {
        let own = self.types.get(&var);
        match (self.narrowed.get(bb, var), own) {
            (Some(narrowed), Some(own)) if own.within(narrowed) => Some(own),
            (Some(narrowed), _) => Some(narrowed),
            (None, own) => own,
        }
    }

    /// The class of `var` when it is an instance of one class.
    fn instance_of(&self, var: VarId) -> Option<String> {
        match self.val(var)? {
            Val::Inst(class, _) => Some(class.clone()),
            _ => None,
        }
    }

    /// The project class of `var`, when it is an instance of one class.
    fn project_class(&self, var: VarId) -> Option<String> {
        match self.val(var)? {
            Val::Inst(c, _) if self.cx.project.is_project_class(c) => Some(c.clone()),
            _ => None,
        }
    }

    /// The project classes `var` may be an instance of: one, the members of a union, or a
    /// protocol with the classes implementing it structurally.
    fn project_classes(&self, var: VarId) -> Vec<String> {
        let project = self.cx.project;
        match self.val(var) {
            Some(Val::Inst(c, _)) if project.is_project_class(c) => {
                let mut out = vec![c.clone()];
                out.extend(
                    project
                        .protocol_implementers(c)
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
                out
            }
            Some(Val::Union(cs)) => cs
                .iter()
                .filter(|c| project.is_project_class(c))
                .cloned()
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Alias maps through `AttrLoad`, copies and in-place container operators
    /// ([`Self::same_object`]), agreeing phis, the calls and property reads that return an
    /// attribute of what they are handed, and the closure cells of enclosing functions (iterated
    /// to a fixpoint); then the entries of the containers the attributes hold, which none of
    /// these maps reads ([`Self::build_entry_aliases`]).
    fn build_aliases(&mut self) {
        for (i, p) in self.f.params.iter().enumerate() {
            if let Ok(i) = u8::try_from(i)
                && i < 64
            {
                self.param_alias.insert(p.var, (i, None));
            }
        }
        let rebound = Rebound::of(self.f, |kind| self.namespace_load(kind));
        let mut returned_attrs: FxHashMap<VarId, (VarId, String)> = FxHashMap::default();
        for bb in &self.f.blocks {
            self.pos = (bb.id, 0);
            for inst in &bb.insts {
                if let Some((dst, holder, Some(attr))) = self.returned_param(&inst.kind) {
                    returned_attrs.insert(dst, (holder, attr));
                }
                if let InstKind::LoadDeref { dst, name } = &inst.kind {
                    self.alias_cell_read(*dst, name);
                }
            }
        }
        loop {
            let before = (
                self.param_alias.len(),
                self.param_slot_alias.len(),
                self.param_copy_alias.len(),
                self.self_attr_alias.len(),
                self.obj_attr_alias.len(),
                self.dict_of.len(),
            );
            for bb in &self.f.blocks {
                self.pos = (bb.id, 0);
                for phi in &bb.phis {
                    let inputs: Vec<VarId> = phi
                        .incoming
                        .iter()
                        .map(|(_, v)| *v)
                        .filter(|v| *v != phi.dst)
                        .collect();
                    merge_phi(&mut self.param_alias, phi.dst, &inputs, |a| a.clone());
                    merge_phi(&mut self.param_slot_alias, phi.dst, &inputs, |a| a.clone());
                    merge_phi(&mut self.param_copy_alias, phi.dst, &inputs, |a| a.clone());
                    merge_phi(&mut self.self_attr_alias, phi.dst, &inputs, |a| a.clone());
                    merge_obj_attr_phi(&mut self.obj_attr_alias, phi.dst, &inputs);
                }
                for inst in &bb.insts {
                    if let Some((dst, src)) = self.same_object(&inst.kind) {
                        self.alias_same_object(dst, src);
                    }
                    match &inst.kind {
                        kind @ (InstKind::AttrLoad { .. }
                        | InstKind::Call { .. }
                        | InstKind::MethodCall { .. }
                        | InstKind::DictLiteral { .. }
                        | InstKind::AllocBuiltin { .. }) => {
                            if let Some((dst, obj, attr)) = self.attr_load(kind) {
                                self.alias_attr_load(dst, obj, &attr, &rebound);
                            } else if let Some(original) = self.shallow_copied_param(kind)
                                && let Some(dst) = kind.dst()
                            {
                                self.param_copy_alias.insert(dst, original);
                            }
                        }
                        InstKind::SubscriptLoad {
                            dst,
                            obj,
                            key_const,
                            is_slice,
                            ..
                        } => {
                            // A slice of a builtin sequence is a shallow copy of it.
                            if *is_slice
                                && let Some(original) = self
                                    .param_alias
                                    .get(obj)
                                    .or_else(|| self.param_copy_alias.get(obj))
                                && ownership::slice_copies(match self.val(*obj) {
                                    Some(Val::Inst(class, _)) => Some(class.as_str()),
                                    _ => None,
                                })
                            {
                                self.param_copy_alias.insert(*dst, original.clone());
                            }
                            // `p["key"]` of a parameter, `td["key"]` of a `TypedDict`: a key of a
                            // typed dict is its field.
                            if let Some(key) = key_const {
                                let copied = || {
                                    rebound
                                        .keeps_key(key)
                                        .then(|| self.param_copy_alias.get(obj))
                                        .flatten()
                                };
                                if let Some((i, None)) = self.param_alias.get(obj).or_else(copied) {
                                    self.param_slot_alias
                                        .insert(*dst, (*i, Slot::Key(key.clone())));
                                }
                                if let Some(class) = self.project_class(*obj)
                                    && self.cx.project.is_typed_dict(&class)
                                {
                                    let load = self.obj_attr(vec![class], *obj, key);
                                    self.obj_attr_alias.insert(*dst, load);
                                }
                            }
                        }
                        _ => {}
                    }
                    // What a getter returns is the attribute it reads (after the property's own
                    // load above, which it replaces).
                    if let Some(dst) = inst.kind.dst()
                        && let Some((holder, attr)) = returned_attrs.get(&dst)
                    {
                        self.alias_attr_load(dst, *holder, attr, &rebound);
                    }
                }
            }
            let after = (
                self.param_alias.len(),
                self.param_slot_alias.len(),
                self.param_copy_alias.len(),
                self.self_attr_alias.len(),
                self.obj_attr_alias.len(),
                self.dict_of.len(),
            );
            if after == before {
                break;
            }
        }
        let _ = self.rebound.set(rebound);
        self.build_entry_aliases();
    }

    /// The parameters whose items or elements the object `var` holds may be, or be below, each
    /// with the path a change of it is to the parameter ([`crate::ELEMENTS_PATH`], or the
    /// attribute it is below: `p.a[0]`): an item (`p["k"]`, `p[i]`), a step of iterating it
    /// (`for x in p`), an element that a method outside the project returns or views
    /// (`p.get(k)`, `p.values()`), an attribute or item of one, through copies and phis, of a
    /// parameter, of the object `p.a` holds, or of a shallow copy of one, while the function
    /// leaves those items as the original's ([`Rebound::keeps_items`]). Empty past the function's
    /// budget.
    fn element_paths(&self, var: VarId) -> Vec<(u8, String)> {
        if self.param_alias.is_empty() {
            return Vec::new();
        }
        let own = |(v, step): (VarId, ItemStep)| {
            self.items_of_container(v, step)
                .map(|(i, path)| {
                    let path = path.clone();
                    (*i, path.unwrap_or_else(|| crate::ELEMENTS_PATH.to_owned()))
                })
                .into_iter()
                .collect()
        };
        let next = |at, out: &mut Vec<_>| self.element_steps(at, out, |_| true);
        match self.elements.of(self.narrowing_scope()).ask(
            &self.walk_budget,
            (var, ItemStep::Below),
            own,
            next,
        ) {
            Answer::Known(paths) => paths.iter().cloned().collect(),
            Answer::Open => Vec::new(),
        }
    }

    /// The parameters (bit `i`) whose entries the object `var` holds may be as part of the
    /// container ([`crate::EffectSummary::entry_change_params`]): the [`Self::element_paths`] at
    /// [`crate::ELEMENTS_PATH`] that no attribute of an object of a project class leads to (the
    /// object `field.tags` holds is the field's, not an entry of the container holding the
    /// field), asked only when `elements`, the element paths of `var`, hold one. Only with an
    /// ownership contract; none past the function's budget.
    fn entry_params(&self, var: VarId, elements: &[(u8, String)]) -> u64 {
        if !self.ownership
            || elements
                .iter()
                .all(|(_, path)| path != crate::ELEMENTS_PATH)
        {
            return 0;
        }
        let own = |(v, step): (VarId, ItemStep)| {
            Or(match self.items_of_container(v, step) {
                Some((i, None)) => 1u64.checked_shl(u32::from(*i)).unwrap_or(0),
                _ => 0,
            })
        };
        let next = |at, out: &mut Vec<_>| {
            self.element_steps(at, out, |obj| self.project_classes(obj).is_empty());
        };
        self.entries
            .of(self.narrowing_scope())
            .ask(&self.walk_budget, (var, ItemStep::Below), own, next)
            .map(|mask| mask.0)
            .open_means(0)
    }

    /// The narrowing scope of the block being read ([`crate::infer::NarrowedVals::scope`]).
    fn narrowing_scope(&self) -> usize {
        self.narrowed.scope(self.pos.0)
    }

    /// The parameter or the shallow copy of one (`param_alias`, `param_copy_alias`) whose items
    /// the step `(v, step)` of [`Self::element_steps`] takes, while the function leaves them as
    /// the original's ([`Rebound::keeps_items`]).
    fn items_of_container(&self, v: VarId, step: ItemStep) -> Option<&(u8, Option<String>)> {
        let ItemStep::Items(index) = step else {
            return None;
        };
        let key = index.and_then(|i| self.shape.str_const(i));
        self.param_alias
            .get(&v)
            .or_else(|| self.param_copy_alias.get(&v))
            .filter(|alias| self.keeps_items(alias, key))
    }

    /// The values one step on from `(v, step)` in [`Self::element_paths`]: those whose items
    /// `v`'s object may be, or whose objects it may be below, through copies, phis, items,
    /// iteration, the elements and views methods outside the project hand out, and the
    /// attributes of the objects `through_attr_of` accepts.
    fn element_steps(
        &self,
        (v, step): (VarId, ItemStep),
        out: &mut Vec<(VarId, ItemStep)>,
        through_attr_of: impl Fn(VarId) -> bool,
    ) {
        let (f, shape) = (self.f, self.shape);
        let container = |v: &VarId| {
            self.param_alias
                .get(v)
                .or_else(|| self.param_copy_alias.get(v))
        };
        let below = (v, ItemStep::Below);
        match shape.def(v) {
            Def::Phi(b, i) => {
                out.extend((f.block(b).phis[i as usize].incoming.iter()).map(|(_, w)| (*w, step)))
            }
            Def::Inst(b, i) => match (&f.block(b).insts[i as usize].kind, step) {
                (InstKind::Copy { src, .. }, _) => out.push((*src, step)),
                // The items of a parameter or of its copy are its own (`items_of_container`);
                // those of any other value are below it.
                (_, ItemStep::Items(_)) if container(&v).is_none() => out.push(below),
                (_, ItemStep::Items(_)) => {}
                (
                    InstKind::SubscriptLoad {
                        obj,
                        index,
                        is_slice: false,
                        ..
                    },
                    ItemStep::Below,
                ) => out.push((*obj, ItemStep::Items(Some(*index)))),
                (InstKind::IterNext { iter, .. }, ItemStep::Below) => {
                    if let Some(InstKind::MakeIter { iterable, .. }) =
                        shape.inst(*iter).map(|i| &i.kind)
                    {
                        out.push((*iterable, ItemStep::Items(None)));
                    }
                }
                (kind, ItemStep::Below) => match self.attr_load(kind) {
                    // An attribute of a parameter or of its copy is `param_alias`'s.
                    Some((_, obj, _)) if container(&obj).is_none() && through_attr_of(obj) => {
                        out.push((obj, step));
                    }
                    Some(_) => {}
                    None => out.extend(self.element_view(kind)),
                },
            },
            Def::Param | Def::None => {}
        }
    }

    /// Whether the function leaves as the original's the item under the constant `key` of the
    /// parameters and copies with the alias `container` (`param_alias`, `param_copy_alias`): it
    /// stores or deletes that key on no object ([`Rebound::keeps_key`]); and for `None`, every
    /// item at every position: it stores, deletes, or adds no item of a value with that alias
    /// (`c[i] = v`, `c.append(v)`, `c.insert(0, v)`, `c.appendleft(v)`, `c.update(...)`).
    fn keeps_items(&self, container: &(u8, Option<String>), key: Option<&str>) -> bool {
        let rebound = self
            .rebound
            .get()
            .expect("build_aliases sets what the function rebinds");
        if let Some(key) = key {
            return rebound.keeps_key(key);
        }
        let changed = self.items_changed.get_or_init(|| {
            let alias = |v: &VarId| {
                self.param_alias
                    .get(v)
                    .or_else(|| self.param_copy_alias.get(v))
                    .cloned()
            };
            let insts = self.f.blocks.iter().flat_map(|b| &b.insts);
            insts
                .filter_map(|inst| match &inst.kind {
                    InstKind::SubscriptStore { obj, .. }
                    | InstKind::SubscriptDelete { obj, .. } => alias(obj),
                    InstKind::MethodCall { recv, method, .. }
                        if matches!(
                            method.as_str(),
                            "append"
                                | "appendleft"
                                | "extend"
                                | "extendleft"
                                | "insert"
                                | "update"
                                | "setdefault"
                                | "__setitem__"
                                | "__delitem__"
                        ) =>
                    {
                        alias(recv)
                    }
                    _ => None,
                })
                .collect()
        });
        !changed.contains(container)
    }

    /// The receiver whose element (under the key the index names, `None`: any) the result of
    /// `kind` is, when it is a method of a class outside the project that returns an element of
    /// its receiver or a view of its elements (`p.get("k")`, `p.values()`,
    /// [`readonly::ELEMENT_ACCESSORS`]).
    fn element_view(&self, kind: &InstKind) -> Option<(VarId, ItemStep)> {
        let InstKind::MethodCall {
            recv, method, args, ..
        } = kind
        else {
            return None;
        };
        if matches!(self.method_callee(*recv, method), Callee::Project(..)) {
            return None;
        }
        let views = (self.derivations(kind).into_iter())
            .any(|(v, d)| v == *recv && d == Derivation::Step(ReturnStep::ELEMENT));
        let index = match method.as_str() {
            "get" | "__getitem__" => args.first().copied(),
            _ => None,
        };
        views.then_some((*recv, ItemStep::Items(index)))
    }

    /// The aliases of `src` for `dst`, the same object ([`Self::same_object`]).
    fn alias_same_object(&mut self, dst: VarId, src: VarId) {
        if let Some(a) = self.param_alias.get(&src).cloned() {
            self.param_alias.insert(dst, a);
        }
        if let Some(a) = self.param_slot_alias.get(&src).cloned() {
            self.param_slot_alias.insert(dst, a);
        }
        if let Some(a) = self.param_copy_alias.get(&src).cloned() {
            self.param_copy_alias.insert(dst, a);
        }
        if let Some(a) = self.self_attr_alias.get(&src).cloned() {
            self.self_attr_alias.insert(dst, a);
        }
        if let Some(a) = self.obj_attr_alias.get(&src).cloned() {
            let copy = ObjAttr { direct: false, ..a };
            self.obj_attr_alias.insert(dst, copy);
        }
        if let Some(o) = self.dict_of.get(&src).cloned() {
            self.dict_of.insert(dst, o);
        }
    }

    /// The alias maps of `dst`, a read of the closure cell `name` of an enclosing function: the
    /// [`CellAlias`] of the values that function binds to it, of an object reached from the cell.
    fn alias_cell_read(&mut self, dst: VarId, name: &str) {
        let Some(alias) = self
            .cx
            .bound_cell(self.f, name)
            .and_then(|cell| self.cell_aliases.get(cell))
        else {
            return;
        };
        let reach = self.reach_of(dst);
        if let Some(a) = &alias.attr {
            self.obj_attr_alias
                .insert(dst, ObjAttr { reach, ..a.clone() });
        }
        if let Some(i) = &alias.item {
            self.item_alias.insert(dst, ItemOf { reach, ..i.clone() });
        }
        if let Some(o) = &alias.dict {
            self.dict_of.insert(dst, Owner { reach, ..o.clone() });
        }
    }

    /// `(dst, obj, attr)` of an instruction loading attribute `attr` of `obj`: `obj.attr`,
    /// `getattr(obj, "attr"[, default])` with a constant name (a default only stands in for a
    /// missing attribute), or `vars(obj)`, which loads `obj.__dict__`.
    fn attr_load(&self, kind: &InstKind) -> Option<(VarId, VarId, String)> {
        match kind {
            InstKind::AttrLoad { dst, obj, attr } => Some((*dst, *obj, attr.clone())),
            InstKind::Call {
                dst,
                callee_var: Some(f),
                args,
                star_from: None,
                ..
            } if matches!(self.val(*f), Some(Val::Ref(s)) if s == "builtins.getattr")
                && (2..=3).contains(&args.len()) =>
            {
                Some((*dst, args[0], self.resolved_str(args[1])?))
            }
            _ => self
                .namespace_load(kind)
                .map(|(dst, obj)| (dst, obj, "__dict__".to_owned())),
        }
    }

    /// [`resolver::namespace_load`] in the function.
    fn namespace_load(&self, kind: &InstKind) -> Option<(VarId, VarId)> {
        resolver::namespace_load(kind, |v| self.val(v))
    }

    /// `(dst, holder, attr)` of a call or property read that returns an object it is handed
    /// (`same(node)` returning `node`), or attribute `attr` of one (`node.members()`,
    /// `node.view`, `items_of(node)` returning `node.items`): every project function it may run
    /// returns that object of the same parameter ([`crate::FunctionDecl::returns_param`]), bound
    /// here to `holder`, and every class the value it dispatches on may be runs one of them
    /// ([`resolver::TypeCtx::runs_on_every_class`]).
    fn returned_param(&self, kind: &InstKind) -> Option<(VarId, VarId, Option<String>)> {
        let project = &self.cx.project;
        let (dst, callee, recv, (args, kwargs), star_from, dispatch) = match kind {
            InstKind::AttrLoad { dst, obj, attr } if project.may_return_param(attr) => {
                let getters = self.cx.resolve_attr_load(self.val(*obj), attr)?;
                (
                    *dst,
                    getters,
                    Some(*obj),
                    (&[][..], &[][..]),
                    None,
                    (*obj, attr.as_str()),
                )
            }
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                kwargs,
                star_from,
                ..
            } if project.may_return_param(method) => {
                let callee = self.method_callee(*recv, method);
                let dispatch = (*recv, method.as_str());
                (
                    *dst,
                    callee,
                    Some(*recv),
                    (args.as_slice(), kwargs.as_slice()),
                    *star_from,
                    dispatch,
                )
            }
            InstKind::Call {
                dst,
                callee_var: Some(c),
                args,
                kwargs,
                star_from,
                ..
            } => {
                // Calling a class makes a new object; a named function is the one declared.
                let returns_attr = match self.val(*c)? {
                    Val::Ref(fqn) => matches!(
                        project.symbol(fqn),
                        resolver::Symbol::Function(_, decl) if decl.returns_param.is_some()
                    ),
                    Val::Inst(_, _) | Val::Union(_) => project.may_return_param("__call__"),
                    Val::Class(_) | Val::Super(_) | Val::OneOf(_) => false,
                };
                if !returns_attr {
                    return None;
                }
                let callee = self.cx.resolve_call(self.val(*c));
                (
                    *dst,
                    callee,
                    None,
                    (args.as_slice(), kwargs.as_slice()),
                    *star_from,
                    (*c, "__call__"),
                )
            }
            _ => return None,
        };
        let Callee::Project(keys, binds_recv) = &callee else {
            return None;
        };
        // The argument each callee returns the attribute of: `None` for the receiver, else
        // the position among the call's arguments (a static method binds no receiver).
        let mut returned: Option<(Option<usize>, Option<&str>)> = None;
        let mut decls = Vec::with_capacity(keys.len());
        for key in keys {
            let decl = self.cx.project.function_decl(key)?;
            let (param, attr) = decl.returns_param.as_ref()?;
            let binds = *binds_recv && !decl.is_staticmethod;
            let this = (
                usize::from(*param).checked_sub(usize::from(binds)),
                attr.as_deref(),
            );
            if *returned.get_or_insert(this) != this {
                return None;
            }
            decls.push((decl, *param));
        }
        let (position, attr) = returned?;
        let (value, name) = dispatch;
        if !self.cx.runs_on_every_class(self.val(value), name, &callee) {
            return None;
        }
        // A keyword binds the parameter where no position does, if it names it in every callee
        // (a `**` mapping could bind it only where no written keyword does).
        let keyword = || {
            kwargs.iter().find_map(|(name, v)| {
                let name = name.as_deref()?;
                (decls.iter())
                    .all(|(decl, param)| decl.keyword_param(name) == Some(usize::from(*param)))
                    .then_some(*v)
            })
        };
        let holder = match position {
            None => recv?,
            Some(i) if i < args.len() && star_from.is_none_or(|s| i < usize::from(s)) => args[i],
            Some(_) if star_from.is_none() => keyword()?,
            Some(_) => return None,
        };
        Some((dst, holder, attr.map(str::to_owned)))
    }

    /// The alias maps of `dst = obj.<attr>` ([`Self::build_aliases`]).
    fn alias_attr_load(&mut self, dst: VarId, obj: VarId, attr: &str, rebound: &Rebound<'_>) {
        if attr == "__dict__" {
            let owner = self.owner_of(obj);
            self.dict_of.insert(dst, owner);
            return;
        }
        // An attribute of a shallow copy holds what the original's does, unless the function
        // may have rebound it on the copy.
        let copied = || {
            rebound
                .keeps_attr(attr)
                .then(|| self.param_copy_alias.get(&obj))
                .flatten()
        };
        if let Some((i, path)) = self.param_alias.get(&obj).or_else(copied).cloned() {
            if path.is_none() {
                self.param_slot_alias
                    .insert(dst, (i, Slot::Attr(attr.to_owned())));
            }
            self.param_alias
                .insert(dst, (i, Some(path.unwrap_or_else(|| attr.to_owned()))));
        }
        if self.self_vars.contains(&obj) {
            self.self_attr_alias.insert(dst, attr.to_owned());
        }
        let classes = self.project_classes(obj);
        if !classes.is_empty() {
            let load = self.obj_attr(classes, obj, attr);
            self.obj_attr_alias.insert(dst, load);
        }
    }

    /// The load `obj.<attr>` (`obj["<attr>"]`) of the project-class object `obj`.
    fn obj_attr(&self, classes: Vec<String>, obj: VarId, attr: &str) -> ObjAttr {
        ObjAttr {
            classes,
            attr: attr.to_owned(),
            direct: true,
            obj_params: self.may_params.get(&obj).copied().unwrap_or(0),
            fresh: self.is_fresh(obj),
            reach: self.reach_of(obj),
            holder: Some(obj),
        }
    }

    /// [`Self::reach`]: the parameters and module / closure state (a summarized function reads
    /// globals with `LoadGlobal`, closure cells with `LoadDeref`), then a monotone fixpoint over
    /// copies, phis (any input), attribute and item loads, iteration, and the methods that return
    /// an element of their receiver. Calls reach nothing: their result is new or of unknown origin.
    fn build_reach(&mut self) {
        for (i, p) in self.f.params.iter().enumerate().take(64) {
            self.reach.insert(
                p.var,
                Reach {
                    params: 1u64 << i,
                    globals: false,
                },
            );
        }
        for inst in self.f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::LoadGlobal { dst, .. } | InstKind::LoadDeref { dst, .. } = &inst.kind {
                self.reach.insert(
                    *dst,
                    Reach {
                        params: 0,
                        globals: true,
                    },
                );
            }
        }
        loop {
            let mut changed = false;
            for bb in &self.f.blocks {
                for phi in &bb.phis {
                    let r = phi
                        .incoming
                        .iter()
                        .fold(Reach::default(), |acc, (_, v)| acc.join(self.reach_of(*v)));
                    changed |= join_reach(&mut self.reach, phi.dst, r);
                }
                for inst in &bb.insts {
                    if let Some((dst, src)) = reached_through(&inst.kind) {
                        let r = self.reach_of(src);
                        changed |= join_reach(&mut self.reach, dst, r);
                    }
                }
            }
            if !changed {
                return;
            }
        }
    }

    /// [`Self::reach`] of `var`.
    fn reach_of(&self, var: VarId) -> Reach {
        self.reach.get(&var).copied().unwrap_or_default()
    }

    /// The module globals and closure cells the object `var` holds may be reached from: the
    /// loads of globals and of the enclosing functions' variables (in the module body, the values
    /// it binds to the module's names) it is reached from along the steps of [`Self::reach`];
    /// open past the function's budget.
    fn global_roots(&self, var: VarId) -> Answer<std::rc::Rc<KeySet<Root<'a>>>> {
        let f = self.f;
        let shape = self.shape;
        let own = |v: VarId| {
            let mut roots = KeySet::new();
            match shape.inst(v).map(|inst| &inst.kind) {
                Some(InstKind::LoadGlobal { name, .. }) => {
                    roots.insert(Root::Global(name));
                }
                Some(InstKind::LoadDeref { name, .. }) => {
                    roots.insert(Root::Cell(name));
                }
                _ => {}
            }
            if f.is_module_body {
                for u in shape.readers(v) {
                    if let Use::Inst(b, i) = *u
                        && let InstKind::StoreGlobal { name, value } =
                            &f.block(b).insts[i as usize].kind
                        && *value == v
                    {
                        roots.insert(Root::Global(name));
                    }
                }
            }
            roots
        };
        let sources = |v: VarId, out: &mut Vec<VarId>| match shape.def(v) {
            Def::Inst(b, i) => out.extend(
                reached_through(&f.block(b).insts[i as usize].kind)
                    .filter(|(dst, _)| *dst == v)
                    .map(|(_, src)| src),
            ),
            Def::Phi(b, i) => {
                out.extend(f.block(b).phis[i as usize].incoming.iter().map(|(_, v)| *v))
            }
            Def::Param | Def::None => {}
        };
        self.roots.ask(&self.walk_budget, var, own, sources)
    }

    /// A change, at `span`, of state that outlives the call.
    fn change_state(&mut self, state: SharedState, span: Span) {
        if !self.opt_in.shared_state {
            return;
        }
        let f = self.summary.shared_state_mut();
        match &state {
            SharedState::Global(_) => f.globals = true,
            SharedState::Cell(cell) => f.cells.push(cell.clone()),
        }
        f.changed.push((state, span));
    }

    /// A change of the module global `name` or of its object.
    fn change_global(&mut self, name: &str, span: Span) {
        self.change_state(SharedState::Global(name.to_owned()), span);
    }

    /// Rebinding or deleting the module global `name`: a change of shared state, except in the
    /// module body, whose bindings define the module's names.
    fn bind_global(&mut self, name: &str, span: Span) {
        if !self.f.is_module_body {
            self.change_global(name, span);
        }
    }

    /// A change of the closure cell of the enclosing functions' variable `name`: rebinding it
    /// (`rebinds`) or changing its object in place. A cell whose owner heki cannot find counts
    /// as a module global.
    fn change_cell(&mut self, name: &str, rebinds: bool, span: Span) {
        let state = self.cell_state(name, rebinds);
        self.change_state(state, span);
    }

    /// The shared state of the enclosing functions' variable `name` ([`Self::change_cell`]).
    fn cell_state(&self, name: &str, rebinds: bool) -> SharedState {
        self.cell_owner(name)
            .map_or(SharedState::Global(name.to_owned()), |owner| {
                SharedState::Cell(CellChange {
                    owner,
                    name: name.to_owned(),
                    rebinds,
                })
            })
    }

    /// The shared state the object `var` holds may be reached from: each module global and
    /// closure cell, or, when the walk is open past its budget, a module global whose name is not
    /// known (an empty name).
    fn shared_states(&self, var: VarId) -> Vec<SharedState> {
        match self.global_roots(var) {
            Answer::Known(roots) => roots
                .iter()
                .map(|root| match root {
                    Root::Global(name) => SharedState::Global((*name).to_owned()),
                    Root::Cell(name) => self.cell_state(name, false),
                })
                .collect(),
            Answer::Open => vec![SharedState::Global(String::new())],
        }
    }

    /// The enclosing function whose variable `name` is (the nearest one whose nested functions
    /// capture it), by qualname.
    fn cell_owner(&self, name: &str) -> Option<String> {
        self.cx
            .ir()
            .enclosing_functions(self.f)
            .map(func_qualname)
            .find(|outer| self.cx.captures(outer, name))
            .map(str::to_owned)
    }

    /// The shared state the call at `site` hands to its project callee `callee`: the receiver
    /// `recv` (parameter 0) and the arguments `bound` (parameter, variable).
    fn handed(
        &self,
        site: Span,
        callee: usize,
        recv: Option<VarId>,
        bound: &[(u8, VarId)],
    ) -> Vec<HandedState> {
        let args = recv
            .map(|r| (0, r))
            .into_iter()
            .chain(bound.iter().copied());
        args.flat_map(|(param, var)| {
            self.shared_states(var)
                .into_iter()
                .map(move |state| HandedState {
                    site,
                    callee,
                    param,
                    state,
                })
        })
        .collect()
    }

    /// A change of the object `var` holds, at `span`: a change of the global or closure cell it
    /// is reached from, if any.
    fn change_object(&mut self, var: VarId, span: Span) {
        if !self.opt_in.shared_state {
            return;
        }
        for state in self.shared_states(var) {
            self.change_state(state, span);
        }
    }

    /// [`EffectSummary::new_cells`]: the captured variables every assignment of which binds a
    /// new container (a display, or a copy of one).
    fn find_new_cells(&mut self) {
        let f = self.f;
        if f.captured_vars.is_empty() || !self.opt_in.shared_state {
            return;
        }
        let new_container = |v: VarId| {
            matches!(
                self.defs.get(&v).map(|i| &i.kind),
                Some(InstKind::AllocBuiltin { .. } | InstKind::DictLiteral { .. })
            )
        };
        let mut bound_otherwise: FxHashSet<&str> =
            f.params.iter().map(|p| p.name.as_str()).collect();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            let Some(dst) = inst.kind.dst() else { continue };
            let Some(name) = f.var_name(dst) else {
                continue;
            };
            let new = match &inst.kind {
                InstKind::Copy { src, .. } => new_container(*src),
                kind => matches!(
                    kind,
                    InstKind::AllocBuiltin { .. } | InstKind::DictLiteral { .. }
                ),
            };
            if !new {
                bound_otherwise.insert(name);
            }
        }
        let new_cells: Vec<String> = f
            .captured_vars
            .iter()
            .filter(|c| !bound_otherwise.contains(c.as_str()))
            .cloned()
            .collect();
        if !new_cells.is_empty() {
            self.summary.shared_state_mut().new_cells = new_cells;
        }
    }

    /// The project class a call constructs: a call of a class object (`Model(...)`,
    /// `cls(...)`, a property's or an attribute's value `self.model_type(...)`), or of a class
    /// reached through a module or class (`models.Model(...)`), or an allocation by
    /// `object.__new__` ([`Self::bare_allocation`]).
    fn construction(&self, kind: &InstKind) -> Option<String> {
        let project = self.cx.project;
        match kind {
            InstKind::Call {
                callee_var: Some(c),
                args,
                ..
            } => match self.val(*c) {
                Some(Val::Ref(fqn)) if fqn == "builtins.object.__new__" => {
                    self.bare_allocation(args)
                }
                Some(_) => self.class_called(*c),
                None => self
                    .typed_partial(*c)?
                    .0
                    .class_object(project)
                    .map(str::to_owned),
            },
            InstKind::MethodCall { recv, method, .. }
                if let Some(class) = self.held_class(*recv, method) =>
            {
                Some(class)
            }
            InstKind::MethodCall {
                recv, method, args, ..
            } => match self.val(*recv)? {
                Val::Ref(owner) => {
                    let fqn = project.object_named(&format!("{owner}.{method}"));
                    if fqn == "builtins.object.__new__"
                        || method == "__new__"
                            && project.is_project_class(owner)
                            && project.new_is_object_new(owner, false, self.cx.kb)
                    {
                        return self.bare_allocation(args);
                    }
                    project.is_project_class(&fqn).then_some(fqn)
                }
                Val::Class(owner)
                    if method == "__new__"
                        && project.new_is_object_new(owner, false, self.cx.kb) =>
                {
                    self.bare_allocation(args)
                }
                Val::Super(owner)
                    if method == "__new__"
                        && project.new_is_object_new(owner, true, self.cx.kb) =>
                {
                    self.bare_allocation(args)
                }
                // `self.factory(...)` of a class body binding `factory` to a class.
                Val::Inst(class, _) | Val::Class(class)
                    if let Some((named, _)) = project.class_alias(class, method) =>
                {
                    let fqn = project.object_named(named);
                    project.is_project_class(&fqn).then_some(fqn)
                }
                recv => self
                    .cx
                    .called_member(recv.recv()?, method)??
                    .class_object(project)
                    .map(str::to_owned),
            },
            _ => None,
        }
    }

    /// The project class `object.__new__(cls, ...)` allocates an instance of, without running
    /// its `__init__`: the class object passed first.
    fn bare_allocation(&self, args: &[VarId]) -> Option<String> {
        self.class_called(*args.first()?)
    }

    /// The callable a call of `var` calls with the call's own positions, and what the object
    /// adds, when `var` holds a `functools.partial` object the function does not make (an item
    /// of a table, `POLICY[kind]`) of a named object, or of one of several, holding no
    /// positions: its type says so, and calling it runs what a call of the callable written out
    /// runs. One the function makes is [`Self::partial`]'s.
    fn typed_partial(&self, var: VarId) -> Option<(Val, Held)> {
        if self.partial(var).is_some() {
            return None;
        }
        match self.inferred.ty(var)? {
            Ty::Partial(callable, held) if *held != Held::Positions => {
                Some((callable.val()?, *held))
            }
            _ => None,
        }
    }

    /// The project class calling `callee` constructs (`Model`, `cls` in a class method).
    fn class_called(&self, callee: VarId) -> Option<String> {
        self.val(callee)?
            .class_object(self.cx.project)
            .map(str::to_owned)
    }

    /// [`Self::fresh`]: the results of constructing a project class
    /// ([`Self::construction`]), then a monotone fixpoint over copies and phis.
    fn build_fresh(&mut self) {
        for inst in self.f.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(dst) = inst.kind.dst()
                && self.construction(&inst.kind).is_some()
            {
                self.fresh.insert(dst);
            }
        }
        loop {
            let before = self.fresh.len();
            for bb in &self.f.blocks {
                for phi in &bb.phis {
                    let mut inputs = phi
                        .incoming
                        .iter()
                        .map(|(_, v)| *v)
                        .filter(|v| *v != phi.dst);
                    let first = inputs.next();
                    if first.is_some_and(|v| self.fresh.contains(&v))
                        && inputs.all(|v| self.fresh.contains(&v))
                    {
                        self.fresh.insert(phi.dst);
                    }
                }
                for inst in &bb.insts {
                    if let InstKind::Copy { dst, src } = &inst.kind
                        && self.fresh.contains(src)
                    {
                        self.fresh.insert(*dst);
                    }
                }
            }
            if self.fresh.len() == before {
                return;
            }
        }
    }

    /// Whether `var` is an instance the function constructs (`Self::fresh`).
    fn is_fresh(&self, var: VarId) -> bool {
        self.fresh.contains(&var)
    }

    /// The class line of the class `var` is exactly an instance of ([`ArgOrigin::exact`]).
    fn exact_line(&self, var: VarId) -> Option<SharedLine> {
        match self.val(var)? {
            Val::Inst(class, true) => Some(self.cx.project.lineage(class, false)),
            _ => None,
        }
    }

    fn owner_of(&self, var: VarId) -> Owner {
        Owner {
            is_self: self.self_vars.contains(&var),
            param: self.param_alias.get(&var).cloned(),
            typed_classes: self.project_classes(var),
            may_params: self.may_params.get(&var).copied().unwrap_or(0),
            fresh: self.is_fresh(var),
            reach: self.reach_of(var),
            var: Some(var),
            unknown_class: self.unknown_class(var),
        }
    }

    fn origin(&self, var: VarId) -> ArgOrigin {
        let field = self.obj_attr_alias.get(&var);
        let typed_field = field
            .filter(|a| a.classes.len() == 1)
            .map(|a| (a.classes[0].clone(), a.attr.clone()));
        let typed_instance = self.project_class(var);
        let implementers = typed_instance
            .as_deref()
            .filter(|_| self.contracts.is_some())
            .and_then(|class| self.cx.project.protocol(class))
            .map(|listed| match listed {
                Some(classes) => Implementers::Listed(classes.to_vec()),
                None => Implementers::Unlisted,
            });
        let param_elements = self.element_paths(var);
        let param_entries = self.entry_params(var, &param_elements);
        let mut origin = ArgOrigin {
            param: self.param_alias.get(&var).cloned(),
            param_slot: self.param_slot_alias.get(&var).cloned(),
            param_elements,
            param_entries,
            param_copy: self.param_copy_alias.get(&var).cloned(),
            self_attr: self.self_attr_alias.get(&var).cloned(),
            targets: None,
            typed_field,
            typed_instance,
            implementers,
            may_params: self.may_params.get(&var).copied().unwrap_or(0),
            retyped: self.retyped.contains(&var),
            fresh: self.is_fresh(var) || field.is_some_and(|a| a.fresh),
            exact: self.exact_line(var),
            reach: self.reach_of(var),
            binding: None,
            new_object: NewArg::Old,
            origins: None,
        };
        let instances: Vec<&String> = origin.instance_classes().collect();
        origin.targets = self.arg_targets(origin.typed_field.as_ref(), &instances);
        origin
    }

    /// [`ArgOrigin::targets`] of an argument that is the field `field` and an instance of one
    /// of `instances` (`None` without an `invalidation` contract naming either).
    fn arg_targets(
        &self,
        field: Option<&(String, String)>,
        instances: &[&String],
    ) -> Option<Box<ArgTargets>> {
        let c = self.contracts.filter(|_| self.events.is_some())?;
        let with_bases = |class: &str| ContractClass::lineage(c, self.cx.project, class);
        let field = field.map_or(0, |(class, attr)| c.target_of(&with_bases(class), attr));
        let classes: Vec<Vec<ContractClass>> = instances.iter().map(|c| with_bases(c)).collect();
        let classes = if classes.iter().any(|line| c.names_class(line)) {
            classes
        } else {
            Vec::new()
        };
        (field != 0 || !classes.is_empty()).then(|| Box::new(ArgTargets { field, classes }))
    }

    /// The string constant behind `var` ([`TypeCtx::str_constant_with`]).
    fn resolved_str(&self, var: VarId) -> Option<String> {
        self.cx
            .str_constant_with(var, &|v| self.defs.get(&v).copied(), &self.types)
    }

    /// The name the variable `key` holding an attribute name names ([`stores::key_name`]).
    fn key_name(&self, key: VarId) -> AttrName<'static> {
        let def = |v: VarId| self.defs.get(&v).copied();
        stores::key_name(self.cx, self.f, key, &def, &self.types)
    }

    /// The names the keys of `other` name, stored by `ns.update(other)` ([`stores::update_keys`]).
    fn update_keys(&self, other: VarId) -> AttrName<'static> {
        let def = |v: VarId| self.defs.get(&v).copied();
        stores::update_keys(self.cx, self.f, other, &def, &self.types)
    }

    /// A store (`removes`: a deletion) of the attribute `name` names on `owner`'s object through
    /// its namespace or `setattr`: a write of each name a constant gives, or of a name heki
    /// cannot read ([`Self::computed_name`]).
    fn write_named(
        &mut self,
        owner: Owner,
        name: &AttrName<'_>,
        removes: bool,
        kind: WriteKind,
        span: Span,
    ) {
        let attrs: Vec<&str> = match name {
            AttrName::Named(attr) => vec![attr],
            AttrName::OneOf(attrs) => attrs.iter().map(String::as_str).collect(),
            _ if removes => return self.computed_name(&owner, Adds::Nothing, kind, span),
            AttrName::KeyOf(sources) => {
                return self.computed_name(&owner, Adds::UnlessKeysOf(sources), kind, span);
            }
            AttrName::Built { .. } | AttrName::Unknown => {
                return self.computed_name(&owner, Adds::Any, kind, span);
            }
        };
        for attr in attrs {
            if removes && owner.is_self {
                self.summary.invalidates_cached.insert(attr.to_owned());
            }
            self.write_owner_attr(owner.clone(), attr, kind, span);
        }
    }

    /// [`Self::memo_fills`]: the writes of the parameters' own attributes that fill a memo or
    /// initialize lazily ([`cache_flow::attr_writes`], each parameter an object of its own), and
    /// the fills of a container a parameter has as its default (`def f(x, _memo={})`).
    fn memo_fills(&self) -> FxHashSet<(BlockId, usize)> {
        if self.f.params.is_empty() {
            return FxHashSet::default();
        }
        let objects: FxHashMap<VarId, u32> = self
            .param_alias
            .iter()
            .filter(|(_, (_, path))| path.is_none())
            .map(|(v, (i, _))| (*v, u32::from(*i)))
            .collect();
        let defaults: Vec<(VarId, cache_flow::Held<'_>)> = objects
            .iter()
            .filter_map(|(v, i)| {
                let p = &self.f.params[*i as usize];
                p.container_default.then_some((*v, (*i, p.name.as_str())))
            })
            .collect();
        cache_flow::container_writes(self.shape, &objects, &defaults)
            .into_iter()
            .filter(|(attr, w)| w.fills_memo(attr) || w.lazy_init)
            .map(|(_, w)| (w.block, w.index as usize))
            .collect()
    }

    /// The parameter (or the object `p.a` holds) the result of `kind` is a shallow copy of
    /// (`EffectSummary::changed_param_attrs`), holding nothing else ([`readonly::copy_of`]: a copy
    /// call of the catalog or a copy method given it alone, `copy.copy(p)`, `dict(p)`,
    /// `p.copy()`, or a display that only unpacks it, `{**p}`, `[*p]`), of the parameter itself,
    /// of `p.a`, or of such a copy.
    fn shallow_copied_param(&self, kind: &InstKind) -> Option<(u8, Option<String>)> {
        let callee = self.callee_of(kind);
        let copy = readonly::copy_of(self.f, kind, &callee, self.cx.kb).filter(|c| !c.deep)?;
        let mut originals = copy.operands.iter().filter_map(|v| {
            self.param_alias
                .get(v)
                .or_else(|| self.param_copy_alias.get(v))
        });
        let first = originals.next()?;
        originals.all(|o| o == first).then(|| first.clone())
    }

    /// What the call instruction `kind` calls (`Unresolved` for any other instruction).
    fn callee_of(&self, kind: &InstKind) -> Callee {
        match kind {
            InstKind::Call { callee_var, .. } => {
                self.cx.resolve_call(callee_var.and_then(|v| self.val(v)))
            }
            InstKind::MethodCall { recv, method, .. } => self.method_callee(*recv, method),
            _ => Callee::Unresolved,
        }
    }

    /// Whether calling `var`, a bound method (`m = obj.method`), passes keywords the call does
    /// not show: those of a `functools.partial` class attribute ([`Call::unseen_keywords`]).
    fn calls_unseen_keywords(&self, var: VarId) -> bool {
        self.bound_method(var).is_some_and(|(recv, method)| {
            self.cx.calls_partial_with_keywords(self.val(recv), &method)
        })
    }

    /// The project class `recv.method(...)` constructs when `method` holds it: a field of an
    /// instance holding the class ([`TypeCtx::field_held`]), or a class attribute bound to a
    /// `functools.partial` object of it (`ClassAlias::partial`).
    fn held_class(&self, recv: VarId, method: &str) -> Option<String> {
        let project = self.cx.project;
        let class = match self
            .inferred
            .ty(recv)
            .and_then(|t| self.cx.field_held(t, method))
        {
            Some(crate::types::Elem::Of(crate::types::Ty::Ref(fqn))) => fqn.clone(),
            Some(_) => return None,
            None => match self.val(recv)? {
                Val::Ref(owner) | Val::Inst(owner, _) | Val::Class(owner) => {
                    let partial = project.class_aliases(owner, method)?.partial?;
                    project.object_named(&partial.callable)
                }
                _ => return None,
            },
        };
        project.is_project_class(&class).then_some(class)
    }

    /// What `recv.method(...)` calls ([`TypeCtx::resolve_method_of`]).
    fn method_callee(&self, recv: VarId, method: &str) -> Callee {
        self.cx
            .resolve_method_of(self.val(recv), self.inferred.ty(recv), method)
    }

    /// `MUT202` derivations of a call's result from its receiver and from its first argument
    /// (copies and element views; see [`readonly::call_derivation`]).
    fn derivations(&self, kind: &InstKind) -> Vec<(VarId, Derivation)> {
        let (recv, method, args, kwargs, callee) = match kind {
            InstKind::Call {
                callee_var,
                args,
                kwargs,
                ..
            } => (
                None,
                None,
                args,
                kwargs,
                self.cx.resolve_call(callee_var.and_then(|v| self.val(v))),
            ),
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } => (
                Some(*recv),
                Some(method.as_str()),
                args,
                kwargs,
                self.method_callee(*recv, method),
            ),
            _ => return Vec::new(),
        };
        let deep =
            readonly::deep_copy_requested(kwargs, |v| self.shape.bool_const(v) == Some(true));
        let own_items = args.len() > 1 || !kwargs.is_empty();
        let mut out = Vec::new();
        if let Some(r) = recv
            && let Some(d) = readonly::call_derivation(method, &callee, self.cx.kb, deep, own_items)
        {
            out.push((r, d));
        }
        if let Some(a) = args.first()
            && let Some(d) = readonly::call_derivation(None, &callee, self.cx.kb, deep, own_items)
        {
            out.push((*a, d));
        }
        out
    }

    /// [`Self::pos`] as the construction recorder counts positions.
    fn at(&self) -> Pos {
        let (bb, index) = self.pos;
        (bb.0, u32::try_from(index).unwrap_or(u32::MAX))
    }

    fn run(mut self) -> Extracted {
        let f = self.f;
        if self.opt_in.site_paths() {
            self.path_facts();
        }
        let reads_external = self.contracts.is_some_and(|c| c.reads_external());
        for bb in &f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                self.pos = (bb.id, index);
                self.visit(bb.id, index, &inst.kind, inst.span);
                if reads_external {
                    self.external_read(&inst.kind, inst.span);
                }
            }
        }
        if self.contracts.is_some_and(|c| c.import_probes()) {
            self.import_probes();
        }
        let mut references = if self.references {
            self.reference_sites()
        } else {
            Vec::new()
        };
        if self.contracts.is_some_and(|c| c.reference_bases()) {
            references.extend(self.base_sites());
        }
        if self.opt_in.kept_values {
            self.kept_value_points();
        }
        self.self_container_changes();
        if self.opt_in.locks {
            self.lock_gives_back();
        }
        self.lock_owner_tests();
        if self.opt_in.guarded {
            self.guarded_facts();
        }
        self.resolve_construction();
        if self.opt_in.only_read {
            self.only_read_facts();
        }
        if self.opt_in.return_origins {
            self.returned_origins();
        }
        let fresh_return = if self.opt_in.fresh_returns {
            self.fresh_return()
        } else {
            None
        };
        if self.opt_in.set_order {
            let calls: Vec<order_flow::OrderCall<'_>> = self
                .project_calls
                .iter()
                .map(|c| order_flow::OrderCall {
                    at: c.at,
                    dst: c.dst,
                    span: self.sites[c.site].span,
                    candidates: &c.params,
                    recv: c.recv,
                })
                .collect();
            self.summary.set_order = order_flow::local_facts(
                order_flow::LocalInput {
                    f,
                    shape: self.shape,
                    cx: self.cx,
                    types: &self.types,
                    inferred: &self.inferred,
                    callees: &self.callees,
                },
                &calls,
            );
        }
        self.iterations();
        if self.opt_in.iterations {
            let site_spans: FxHashSet<Span> = self.sites.iter().map(|s| s.span).collect();
            let container_iterations = iterations::container_iterations(
                f,
                self.cx,
                &self.types,
                &self.self_attr_alias,
                &site_spans,
                self.summary.is_generator,
            );
            if !container_iterations.is_empty() {
                self.summary.iteration_facts_mut().container_iterations = container_iterations;
            }
        }
        self.apply_declaration();
        if self.summary.is_cached_property && f.kind == FunctionKind::Sync {
            self.summary.returns_self_attrs = self.returned_self_attrs();
        }
        if f.owner_class_qualname.is_some()
            && self.key.qualname.rsplit('.').next() == Some("__hash__")
        {
            let hashed = self.hashed_whole_self_attrs();
            self.summary.hashed_whole_self_attrs = (!hashed.is_empty()).then(|| Box::new(hashed));
        }
        self.summary.refresh_typed_field_writes(self.key);
        if let Some(state) = &mut self.summary.shared_state {
            Arc::make_mut(state).normalize();
        }
        let flow = self.events.take().map(|events| {
            let n = self.contracts.map_or(0, |c| c.invalidations());
            let (entry, calls, writes) = invalidation::analyze(self.shape, events, n);
            for (site, flow) in calls {
                self.sites[site].flow = Some(flow);
            }
            FuncFlow {
                entries: vec![entry],
                writes,
            }
        });
        let cells = self.own_cell_aliases();
        Extracted {
            summary: self.summary,
            sites: self.sites,
            flow,
            references,
            external: self.external,
            cells,
            kept_points: self.kept_points,
            types: self.types,
            blind: self.blind,
            unbound: self.unbound,
            imports: self.imports,
            unawaited: self.unawaited,
            fresh_return,
            class_runs: self.class_runs,
        }
    }

    /// The instructions that may remove or replace a value kept in some object's `__dict__`
    /// ([`cache_flow::KeptPoint`]), and their union into [`KeptFacts::removes_kept`]. A
    /// project call's own removals are its callees' (propagation adds them), unless its targets
    /// are not every implementation it can run.
    fn kept_value_points(&mut self) {
        let f = self.f;
        if !f.a1_hazards.is_empty() {
            // Code heki cannot follow (`eval`, `exec`, frame introspection) may run in its
            // callers; its own calls are points of their own below.
            self.summary.kept_mut().removes_kept.set_any();
        }
        let mut points = Vec::new();
        let results = self.callees_by_result();
        for bb in &f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                let at = u32::try_from(index).unwrap_or(u32::MAX);
                let kind = self.kept_removal_at((bb.id, index), &inst.kind, &results);
                // A deletion by name that may also run other code still removes the value kept
                // under the name (`FileSummary::kept_removals`).
                if let (InstKind::AttrDelete { obj, attr }, Some(KeptRemovalKind::Any)) =
                    (&inst.kind, &kind)
                    && !NAMESPACE_ATTRS.contains(&attr.as_str())
                    && self.may_keep_under(*obj, attr)
                {
                    points.push((bb.id, at, KeptRemovalKind::Named(attr.clone())));
                }
                if let Some(kind) = kind {
                    points.push((bb.id, at, kind));
                }
            }
            if let Some(kind) = self.kept_removal_at_end(bb, &results) {
                points.push((
                    bb.id,
                    u32::try_from(bb.insts.len()).unwrap_or(u32::MAX),
                    kind,
                ));
            }
        }
        for (block, index, kind) in points {
            match &kind {
                KeptRemovalKind::Named(name) | KeptRemovalKind::Replaces(name) => {
                    self.summary.kept_mut().removes_kept.add(name);
                }
                KeptRemovalKind::Any => self.summary.kept_mut().removes_kept.set_any(),
                // The consumer is the caller; a generator's removals are its summary's, which
                // propagation joins.
                KeptRemovalKind::Suspends | KeptRemovalKind::Runs(_) => {}
            }
            self.kept_points
                .push(cache_flow::KeptPoint { block, index, kind });
        }
    }

    /// What the instruction `kind` at `(bb, index)` may do to the values kept in an object's
    /// `__dict__`. Every instruction kind has an answer: code of the project it may run that no
    /// event stands for (a special method of an operand's class, attribute access through a
    /// hook or a descriptor heki does not resolve, a class statement, an import) may remove
    /// anything.
    fn kept_removal_at(
        &self,
        at: Position,
        kind: &InstKind,
        results: &FxHashMap<VarId, &Callee>,
    ) -> Option<KeptRemovalKind> {
        let any = Some(KeptRemovalKind::Any);
        if self.specials_run_project_code(&self.f.special_methods(kind)) {
            return any;
        }
        if let Some(call) = call_operands(kind) {
            return self.kept_removal_of_call(at, &call);
        }
        // A value kept under a name no property getter of the project has is none a claim is
        // about.
        let kept_name = |name: &str| self.cx.project.names_property(name);
        let attribute_runs = |obj: VarId, attr: &str, access| {
            invokes::attribute_access_runs_project_code(
                self.cx.project,
                self.cx.kb,
                &self.as_handed(obj, None),
                attr,
                access,
            )
        };
        match kind {
            // A property getter the read runs is a project call of its own: what it removes is
            // its own, unless the read's targets are not every implementation it can run.
            InstKind::AttrLoad { obj, attr, .. } => match self.callees.get(&at) {
                Some(Callee::Project(..)) => {
                    (!self.targets_complete_at(at)).then_some(KeptRemovalKind::Any)
                }
                _ => attribute_runs(*obj, attr, invokes::AttrAccess::Read)
                    .then_some(KeptRemovalKind::Any),
            },
            // Rebinding or deleting an object's namespace or class replaces every value it
            // keeps, or what reading one finds.
            InstKind::AttrDelete { attr, .. } | InstKind::AttrStore { attr, .. }
                if NAMESPACE_ATTRS.contains(&attr.as_str()) =>
            {
                any
            }
            // A store or a deletion through any reference may replace or remove the value the
            // object keeps under the name, unless the object is established as one that keeps
            // none ([`invokes::may_keep_under`]).
            InstKind::AttrDelete { obj, attr } => {
                if attribute_runs(*obj, attr, invokes::AttrAccess::Write) {
                    any
                } else {
                    self.may_keep_under(*obj, attr)
                        .then(|| KeptRemovalKind::Named(attr.clone()))
                }
            }
            InstKind::AttrStore { obj, attr, .. } => {
                if attribute_runs(*obj, attr, invokes::AttrAccess::Write) {
                    any
                } else {
                    self.may_keep_under(*obj, attr)
                        .then(|| KeptRemovalKind::Replaces(attr.clone()))
                }
            }
            InstKind::SubscriptStore {
                obj,
                index,
                key_const,
                ..
            } if self.dict_of.contains_key(obj) => {
                match self.key_of(key_const.as_deref(), *index) {
                    Some(k) => kept_name(&k).then_some(KeptRemovalKind::Replaces(k)),
                    None => any,
                }
            }
            InstKind::SubscriptDelete {
                obj,
                index,
                key_const,
            } if self.dict_of.contains_key(obj) => {
                match self.key_of(key_const.as_deref(), *index) {
                    Some(k) => kept_name(&k).then_some(KeptRemovalKind::Named(k)),
                    None => any,
                }
            }
            // Another coroutine or the generator's consumer runs while the function waits: at
            // an `await`, the implicit awaits of `async for` and `async with` (`__anext__`,
            // `__aenter__`, `__aexit__`), and `yield from`.
            kind if kind.suspends() => any,
            InstKind::YieldFrom { .. } => any,
            InstKind::Yield { .. } => Some(KeptRemovalKind::Suspends),
            // Each step of a loop runs the iteration of what it iterates: at the iterator's
            // creation and at every step.
            InstKind::MakeIter { iterable, .. } => (self.f.var_name(*iterable) != Some(".0"))
                .then(|| self.iteration_removal(*iterable, results))
                .flatten(),
            InstKind::IterNext { iter, .. } => {
                let iterable = match self.defs.get(&self.shape.copy_root(*iter)).map(|i| &i.kind) {
                    Some(InstKind::MakeIter { iterable, .. })
                        if self.f.var_name(*iterable) != Some(".0") =>
                    {
                        *iterable
                    }
                    _ => return None,
                };
                self.iteration_removal(iterable, results)
            }
            // A class statement runs the metaclass, the bases' `__init_subclass__`, and the
            // attributes' `__set_name__`.
            kind if class_body_made(self.cx.ir(), kind).is_some() => any,
            // The first import of a project module runs its body.
            InstKind::Import { .. } => {
                let target = self
                    .f
                    .block(at.0)
                    .insts
                    .get(at.1)
                    .and_then(|i| self.cx.import_target(i.span));
                // Whatever the name is bound to, `from pkg import name` loads `pkg`.
                match target {
                    Some(t) if !self.cx.project.in_project_module(t) => None,
                    _ => any,
                }
            }
            // What these run is a special method of an operand or a call (decided above), or
            // nothing.
            InstKind::Call { .. }
            | InstKind::MethodCall { .. }
            | InstKind::Const { .. }
            | InstKind::AllocBuiltin { .. }
            | InstKind::DictLiteral { .. }
            | InstKind::LoadGlobal { .. }
            | InstKind::LoadDeref { .. }
            | InstKind::LoadName { .. }
            | InstKind::StoreGlobal { .. }
            | InstKind::StoreDeref { .. }
            | InstKind::Copy { .. }
            | InstKind::Delete { .. }
            | InstKind::BinOp { .. }
            | InstKind::UnaryOp { .. }
            | InstKind::Compare { .. }
            | InstKind::Convert { .. }
            | InstKind::Format { .. }
            | InstKind::BuildString { .. }
            | InstKind::SubscriptLoad { .. }
            | InstKind::SubscriptStore { .. }
            | InstKind::SubscriptDelete { .. }
            | InstKind::MakeGenExpr { .. }
            | InstKind::MakeClosure { .. }
            | InstKind::Await { .. }
            | InstKind::EnterWith { .. }
            | InstKind::ExitWith { .. }
            | InstKind::CatchException { .. } => None,
        }
    }

    /// What the end of `bb` may do to the values kept in an object's `__dict__`: a branch's
    /// truth test may run its condition's `__bool__` or `__len__`, and a `raise` constructs the
    /// exception and its cause (`raise ... from`) when they are classes.
    fn kept_removal_at_end(
        &self,
        bb: &BasicBlock,
        results: &FxHashMap<VarId, &Callee>,
    ) -> Option<KeptRemovalKind> {
        let runs = match &bb.terminator {
            Terminator::Raise(exc) => {
                let span = bb.term_span;
                let cause = self
                    .cx
                    .ir()
                    .statement_uses_of(self.f.id)
                    .filter(|(_, at)| span.is_some_and(|s| s.start <= at.start && at.end <= s.end))
                    .map(|(v, _)| v);
                exc.iter()
                    .copied()
                    .chain(cause)
                    .any(|v| self.raising_runs_project_code(v, results))
            }
            _ => bb
                .end_special_methods()
                .is_some_and(|s| self.specials_run_project_code(&[s])),
        };
        runs.then_some(KeptRemovalKind::Any)
    }

    /// Whether raising `v` (or making it the cause) may run code of the project: Python
    /// constructs a class it is handed. An instance runs nothing: one heki infers, a value built
    /// here (`from None`), and what calling a class returns (a class of the project, or an
    /// exception class of the builtins, whose names alone are capitalized there); a class
    /// outside the project is a library call handed nothing (R3); anything else may be a class
    /// of the project.
    fn raising_runs_project_code(&self, v: VarId, results: &FxHashMap<VarId, &Callee>) -> bool {
        let root = self.shape.copy_root(v);
        let builtin_exception = |symbol: &str| {
            symbol
                .strip_prefix("builtins.")
                .is_some_and(|name| name.starts_with(char::is_uppercase))
        };
        let constructed = match self.defs.get(&root).map(|i| &i.kind) {
            Some(InstKind::Call {
                callee_var: Some(class),
                ..
            }) => {
                self.val(*class)
                    .is_some_and(|c| c.class_object(self.cx.project).is_some())
                    || matches!(results.get(&root), Some(Callee::External(s)) if builtin_exception(s))
            }
            _ => false,
        };
        if constructed || self.built(v) != Built::Unknown {
            return false;
        }
        match self.val(v) {
            Some(Val::Inst(_, _) | Val::Union(_)) => false,
            Some(Val::Ref(class) | Val::Class(class))
                if !self.cx.project.is_project_class(class) =>
            {
                self.library_call_runs_project_code(class, None, None, &[], &[], false, false)
            }
            _ => true,
        }
    }

    /// Whether one of the special methods `specials` (an instruction's, or a block end's) may
    /// run code of the project, on the class of its value or of its items, the instruction's
    /// other operands handed to a library's implementation too
    /// ([`invokes::special_methods_run_project_code`]).
    fn specials_run_project_code(&self, specials: &[heki_ir::SpecialMethods]) -> bool {
        let operands: Vec<invokes::Handed<'_>> = specials
            .iter()
            .map(|s| self.as_handed(s.value, None))
            .collect();
        let live: Vec<bool> = operands
            .iter()
            .map(|h| !invokes::inert(self.cx.project, h))
            .collect();
        let lives = live.iter().filter(|l| **l).count();
        specials.iter().enumerate().any(|(own, s)| {
            // An object's `__dict__` holds the names of its attributes, strings: looking a key up
            // in it compares the key with strings, whose comparison runs only the key's own
            // methods (the key's operand).
            let keyed = s.keyed && !self.dict_of.contains_key(&s.value);
            invokes::special_methods_run_project_code(
                self.cx.project,
                self.cx.kb,
                &operands[own],
                lives == usize::from(live[own]),
                &heki_ir::SpecialMethods { keyed, ..*s },
            )
        })
    }

    /// Whether the object `obj` holds may keep a value under `attr` that a property getter of
    /// the project computed ([`invokes::may_keep_under`]), a name such a getter has.
    fn may_keep_under(&self, obj: VarId, attr: &str) -> bool {
        self.cx.project.names_property(attr)
            && invokes::may_keep_under(self.cx.project, &self.as_handed(obj, None), attr)
    }

    /// What the call `call` at `(bb, index)` may do to the values kept in an object's
    /// `__dict__`: a project call whose targets are every implementation it can run removes what
    /// they remove (propagated); a call of a library the catalog covers removes nothing unless
    /// it is handed a value whose type is not established as a library class that is not
    /// callable (a callable, an instance of a project class, a `__dict__`, a value of unknown
    /// type), or it runs code (`eval`, `exec`, an import); a method of a `__dict__` removes what
    /// it takes out of it; any other call may remove anything.
    fn kept_removal_of_call(
        &self,
        at: Position,
        call: &CallOperands<'_>,
    ) -> Option<KeptRemovalKind> {
        let any = Some(KeptRemovalKind::Any);
        let (args, kwargs) = (call.args, call.kwargs);
        let recv = call.method.map(|(r, _)| r);
        if let Some((r, method)) = call.method
            && self.dict_of.contains_key(&r)
        {
            return match method {
                "get" | "keys" | "values" | "items" | "copy" | "__contains__" | "__getitem__"
                | "__len__" | "__iter__" | "setdefault" => None,
                "pop" | "__delitem__" => match args.first().and_then(|a| self.resolved_str(*a)) {
                    Some(k) => Some(KeptRemovalKind::Named(k)),
                    None => any,
                },
                _ => any,
            };
        }
        if args
            .iter()
            .chain(kwargs.iter().map(|(_, v)| v))
            .any(|v| self.dict_of.contains_key(v))
        {
            return any;
        }
        // A comprehension's own scope is the one function it calls.
        let child_scope = call.method.is_none() && call.callee_var.is_none();
        let span = self.f.block(at.0).insts.get(at.1).map(|i| i.span);
        let library = |symbol: &str, recv| {
            self.library_call_runs_project_code(symbol, span, recv, args, kwargs, call.star, false)
        };
        match self.callees.get(&at)? {
            Callee::Project(..) => {
                (!child_scope && !self.targets_complete_at(at)).then_some(KeptRemovalKind::Any)
            }
            Callee::External(symbol) => {
                // Calling an instance runs its class's `__call__`: the instance is its receiver.
                let recv = recv.or(call.callee_var.filter(|_| symbol.ends_with(".__call__")));
                library(symbol, recv).then_some(KeptRemovalKind::Any)
            }
            Callee::TooMany | Callee::Unresolved => any,
        }
    }

    /// [`invokes::runs_project_code`] of the library call of `symbol` at `span` with these
    /// operands (`iterated`: the iterator it returned is iterated).
    #[allow(clippy::too_many_arguments)]
    fn library_call_runs_project_code(
        &self,
        symbol: &str,
        span: Option<Span>,
        recv: Option<VarId>,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        star: bool,
        iterated: bool,
    ) -> bool {
        let positional: Vec<invokes::Handed<'_>> = args
            .iter()
            .enumerate()
            .map(|(i, v)| self.as_handed(*v, span.map(|s| (s, u8::try_from(i).ok()))))
            .collect();
        let keywords: Vec<(Option<&str>, invokes::Handed<'_>)> = kwargs
            .iter()
            .map(|(k, v)| (k.as_deref(), self.as_handed(*v, span.map(|s| (s, None)))))
            .collect();
        invokes::runs_project_code(
            self.cx.project,
            self.cx.kb,
            symbol,
            recv.map(|r| self.as_handed(r, None)).as_ref(),
            &positional,
            &keywords,
            star,
            iterated,
        )
    }

    /// What iterating `iterable` may remove: a generator a project call returns runs its body
    /// here, which removes what the generator's summary says (`Runs`, at the call's span); a
    /// builtin container, string, or range runs its builtin iteration
    /// ([`invokes::iterates_builtin`]); the iterator a library call returned runs, while it is
    /// iterated, what its catalog entry says it runs then (`lazy`); anything else, the value
    /// another project call returns included (its class's iteration has no site), may run
    /// anything.
    fn iteration_removal(
        &self,
        iterable: VarId,
        results: &FxHashMap<VarId, &Callee>,
    ) -> Option<KeptRemovalKind> {
        let root = self.shape.copy_root(iterable);
        let any = Some(KeptRemovalKind::Any);
        match results.get(&root) {
            Some(Callee::Project(keys, _))
                if keys.iter().all(|k| {
                    self.cx
                        .project
                        .function_decl(k)
                        .is_some_and(|d| d.is_generator)
                }) =>
            {
                return self.defs.get(&root).map(|i| KeptRemovalKind::Runs(i.span));
            }
            Some(Callee::External(symbol)) => {
                let lazy = self.cx.kb.invoked(symbol).is_some_and(|e| e.lazy);
                let call = self.defs.get(&root).and_then(|i| call_operands(&i.kind));
                if let (true, Some(call)) = (lazy, call) {
                    let recv = call.method.map(|(r, _)| r);
                    let runs = self.library_call_runs_project_code(
                        symbol,
                        None,
                        recv,
                        call.args,
                        call.kwargs,
                        call.star,
                        true,
                    );
                    return runs.then_some(KeptRemovalKind::Any);
                }
            }
            _ => {}
        }
        if invokes::iterates_builtin(self.cx.project, self.cx.kb, &self.as_handed(iterable, None)) {
            None
        } else {
            any
        }
    }

    /// The builtin function or method the call at `(bb, i)` calls, as the catalog names it.
    fn builtin_result_at(&self, (bb, i): Position) -> Option<String> {
        match self.callees.get(&(bb, i)) {
            Some(Callee::External(symbol)) => Some(symbol.clone()),
            _ => None,
        }
    }

    /// [`invokes::reads_own_namespace`] of `obj`.
    fn reads_own_namespace(&self, obj: VarId) -> bool {
        invokes::reads_own_namespace(self.cx.project, self.cx.kb, self.types.get(&obj))
    }

    /// What the function establishes of the class of the value `v` holds ([`Built`]).
    fn built(&self, v: VarId) -> Built {
        let established = self.established.get_or_init(|| self.established_values());
        established
            .get(v.0 as usize)
            .copied()
            .unwrap_or(Built::Unknown)
    }

    /// Per variable, [`Built`] of the values it may hold: a least fixpoint over the instructions
    /// that make a value from their operands and over phis (a loop's counter), each variable
    /// visited once and again each time an operand's fact grows (at most twice each). A
    /// variable no instruction or phi defines is a parameter or a name bound elsewhere. An
    /// operand whose inferred class runs `object`'s or a builtin type's own special methods for
    /// the instruction ([`invokes::interpreter_own`]) counts whatever the function built.
    fn established_values(&self) -> Vec<Built> {
        let f = self.f;
        let n = f.var_names.len();
        let project = self.cx.project;
        // The operands of `kind` whose inferred classes do not decide its result's class.
        let undecided = |kind: &InstKind, scalar: bool| -> Vec<VarId> {
            f.special_methods(kind)
                .into_iter()
                .filter(|s| {
                    !invokes::interpreter_own(project, self.types.get(&s.value), s.methods, scalar)
                })
                .map(|s| s.value)
                .collect()
        };
        let mut operands: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut readers: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut rules: Vec<BuiltRule> = vec![BuiltRule::Is(Built::Unknown); n];
        let mut set = |dst: VarId, rule: BuiltRule, srcs: &mut dyn Iterator<Item = VarId>| {
            let d = dst.0 as usize;
            rules[d] = rule;
            for src in srcs.map(|s| s.0 as usize) {
                operands[d].push(src);
                readers[src].push(d);
            }
        };
        for bb in &f.blocks {
            for phi in &bb.phis {
                set(
                    phi.dst,
                    BuiltRule::Joined,
                    &mut phi.incoming.iter().map(|(_, v)| *v),
                );
            }
            for (i, inst) in bb.insts.iter().enumerate() {
                let Some(dst) = inst.kind.dst() else { continue };
                let none = &mut std::iter::empty();
                match &inst.kind {
                    InstKind::Const { .. }
                    | InstKind::BuildString { .. }
                    | InstKind::UnaryOp {
                        op: UnaryOpKind::Not,
                        ..
                    }
                    | InstKind::Compare {
                        op:
                            CompareOpKind::Is
                            | CompareOpKind::IsNot
                            | CompareOpKind::In
                            | CompareOpKind::NotIn,
                        ..
                    } => set(dst, BuiltRule::Is(Built::Atomic), none),
                    // A list, set, or dict display may be changed after it is made.
                    InstKind::AllocBuiltin {
                        kind: BuiltinKind::Tuple,
                        elems,
                        ..
                    } => set(dst, BuiltRule::Tuple, &mut elems.iter().copied()),
                    InstKind::AllocBuiltin { .. } | InstKind::DictLiteral { .. } => {
                        set(dst, BuiltRule::Is(Built::Exact), none);
                    }
                    // An instance's own namespace (`obj.__dict__`, `vars(obj)`), read without
                    // running code, is a builtin dict.
                    kind @ (InstKind::AttrLoad { .. } | InstKind::Call { .. })
                        if self
                            .namespace_load(kind)
                            .is_some_and(|(_, obj)| self.reads_own_namespace(obj)) =>
                    {
                        set(dst, BuiltRule::Is(Built::Exact), none);
                    }
                    InstKind::Copy { src, .. } => {
                        set(dst, BuiltRule::Joined, &mut std::iter::once(*src));
                    }
                    kind @ InstKind::BinOp { .. } => {
                        set(
                            dst,
                            BuiltRule::Joined,
                            &mut undecided(kind, true).into_iter(),
                        );
                    }
                    kind @ InstKind::UnaryOp { .. } => {
                        set(
                            dst,
                            BuiltRule::OfAtomic,
                            &mut undecided(kind, true).into_iter(),
                        );
                    }
                    // `==` and `!=` of builtin values give a `bool`; an ordering of two
                    // sequences gives what comparing their first differing items gives.
                    kind @ InstKind::Compare {
                        op: CompareOpKind::Eq | CompareOpKind::NotEq,
                        ..
                    } => set(
                        dst,
                        BuiltRule::OfExact,
                        &mut undecided(kind, false).into_iter(),
                    ),
                    kind @ InstKind::Compare { .. } => {
                        set(
                            dst,
                            BuiltRule::OfAtomic,
                            &mut undecided(kind, true).into_iter(),
                        );
                    }
                    // A conversion gives what the value's `__str__`, `__repr__`, or `__format__`
                    // returns, a subclass's instance too.
                    kind @ (InstKind::Format { .. } | InstKind::Convert { .. }) => {
                        set(
                            dst,
                            BuiltRule::OfExact,
                            &mut undecided(kind, false).into_iter(),
                        );
                    }
                    InstKind::Call { args, .. } | InstKind::MethodCall { args, .. } => {
                        let symbol = self.builtin_result_at((bb.id, i));
                        let listed =
                            |list: &[&str]| symbol.as_deref().is_some_and(|s| list.contains(&s));
                        if listed(invokes::BUILT_SCALARS) {
                            set(dst, BuiltRule::Is(Built::Atomic), none);
                        } else if listed(invokes::BUILT_CONTAINERS) {
                            set(dst, BuiltRule::Is(Built::Exact), none);
                        } else if listed(invokes::CONVERSIONS) {
                            let converted = args.first().copied().filter(|a| {
                                !invokes::interpreter_own(
                                    project,
                                    self.types.get(a),
                                    &["__str__", "__repr__", "__format__"],
                                    false,
                                )
                            });
                            set(dst, BuiltRule::OfExact, &mut converted.into_iter());
                        }
                    }
                    _ => {}
                }
            }
        }
        let max_visits = 2 * operands.iter().map(Vec::len).max().unwrap_or(0) + 1;
        let read = |v: usize, out: &mut Vec<usize>| out.extend(&readers[v]);
        let budget = Budget::unmetered("heki_effect::summary::Extractor::established_values");
        let facts = heki_ir::walk::fixpoint(
            &budget,
            heki_ir::walk::Sparse {
                nodes: n,
                readers: &read,
                max_visits: u32::try_from(max_visits).unwrap_or(u32::MAX),
            },
            |v, facts: &heki_ir::walk::Facts<(bool, bool)>| {
                let least = operands[v]
                    .iter()
                    .map(|o| Built::of_fact(facts.get(*o)))
                    .max()
                    .unwrap_or(Built::Atomic);
                rules[v].apply(least).fact()
            },
        );
        facts
            .map(|facts| {
                facts
                    .into_vec()
                    .iter()
                    .map(|f| Built::of_fact(f.as_ref()))
                    .collect()
            })
            .open_means(vec![Built::Unknown; n])
    }

    /// `v` as a library call is handed it ([`invokes::Handed`]); `at`: the call's position and
    /// the argument's (`None` for a keyword), where a callable it is handed has its own site.
    fn as_handed(&self, v: VarId, at: Option<(Span, Option<u8>)>) -> invokes::Handed<'_> {
        let root = self.shape.copy_root(v);
        let def = self.defs.get(&root).map(|i| &i.kind);
        let built = self.built(root);
        // A list, dict, or set display used anywhere else may be changed before the call; a
        // starred element and a `**` unpacking add items of other values.
        let used_here_only = v == root && self.uses.get(&root).is_some_and(|u| u.len() == 1);
        let parts: Option<Vec<VarId>> = match def {
            Some(InstKind::AllocBuiltin { kind, elems, .. })
                if (*kind == BuiltinKind::Tuple || used_here_only)
                    && !elems.iter().any(|e| self.f.is_unpacked(*e)) =>
            {
                Some(elems.clone())
            }
            Some(kind @ InstKind::DictLiteral { entries, .. })
                if used_here_only
                    && !entries
                        .iter()
                        .any(|e| matches!(e, DictEntry::Unpack { .. })) =>
            {
                Some(kind.operands())
            }
            // The items (if any) of a value whose items are established are builtin values.
            _ if built == Built::Atomic => Some(Vec::new()),
            _ => None,
        };
        let elems = parts.map(|parts| {
            parts
                .iter()
                .map(|e| {
                    let built = self.built(*e);
                    invokes::Handed {
                        val: self.val(*e),
                        built: built != Built::Unknown,
                        elems: (built == Built::Atomic).then(Vec::new),
                        ..invokes::Handed::default()
                    }
                })
                .collect()
        });
        let followed_callables = self.followed_callables.get_or_init(|| {
            self.sites
                .iter()
                .filter(|site| !site.project_callees().is_empty())
                .filter_map(|site| Some((site.span, site.handed_callable?)))
                .collect()
        });
        let followed = at.is_some_and(|(span, position)| {
            position.is_some_and(|i| followed_callables.contains(&(span, i)))
        });
        invokes::Handed {
            val: self.val(v),
            built: built != Built::Unknown,
            elems,
            followed,
        }
    }

    /// The [`CellAlias`] of each closure cell the function binds, when its bindings have one.
    fn own_cell_aliases(&mut self) -> Vec<(CellKey, CellAlias)> {
        let mut out = Vec::new();
        for (name, vars) in crate::infer::cell_bindings(self.f) {
            let mut untyped = vars.iter().map(|&v| {
                if let Def::Inst(b, i) = self.shape.def(v) {
                    self.pos = (b, i as usize);
                }
                self.untyped_container(v)
                    .map(|(attr, _, nested)| (attr, nested))
            });
            let first = untyped.next().flatten();
            let untyped = first.filter(|f| untyped.all(|u| u.as_ref() == Some(f)));
            let attr = joined_obj_attr(vars.iter().map(|v| self.obj_attr_alias.get(v)));
            let attr = attr.map(|a| ObjAttr {
                obj_params: 0,
                fresh: false,
                holder: None,
                ..a
            });
            let item = joined_item(vars.iter().map(|v| self.item_alias.get(v)));
            let item = item.map(|i| ItemOf {
                obj_params: 0,
                fresh: false,
                holder: None,
                ..i
            });
            let dict = joined_owner(vars.iter().map(|v| self.dict_of.get(v)));
            let dict = dict.map(|o| Owner {
                is_self: false,
                param: None,
                may_params: 0,
                fresh: false,
                var: None,
                ..o
            });
            if attr.is_some() || item.is_some() || dict.is_some() || untyped.is_some() {
                let alias = CellAlias {
                    attr,
                    item,
                    dict,
                    untyped,
                };
                out.push(((self.f.id, name.to_owned()), alias));
            }
        }
        out
    }

    /// The site at `span` of what calling `var`, loaded in block `bb`, calls: a project class it
    /// constructs, or the project functions it calls (a bound method's included, with its
    /// receiver's origin bound to `self`). An instance of a class defining `__call__` is no
    /// function or class handed on.
    fn reference_site(&self, var: VarId, bb: BlockId, span: Span) -> Option<CallSite> {
        let constructs = self.class_called(var);
        let (callee, recv) = self.callable_callee(var);
        if constructs.is_none() && recv.is_none() && !matches!(self.val(var), Some(Val::Ref(_))) {
            return None;
        }
        // A bound method binds its receiver to the method's `self`, as calling it does: the
        // ownership exemptions (a class's own initialization) read where that object comes from.
        let receiver = recv.map(|r| self.receiver(r, bb).0);
        let binds_self = |key: &FuncKey| {
            let decl = self.cx.project.function_decl(key);
            decl.is_some_and(|d| !d.is_staticmethod && !d.is_classmethod)
        };
        let callees: Vec<BoundCallee> = match callee {
            Callee::Project(keys, binds) => keys
                .into_iter()
                .map(|callee| BoundCallee {
                    params: receiver
                        .iter()
                        .filter(|_| binds && binds_self(&callee))
                        .cloned()
                        .collect(),
                    callee,
                })
                .collect(),
            _ => Vec::new(),
        };
        if constructs.is_none() && callees.is_empty() {
            return None;
        }
        let member = match self.defs.get(&var).map(|d| &d.kind) {
            Some(InstKind::AttrLoad { obj, attr, .. }) => {
                let target = constructs
                    .clone()
                    .or_else(|| (callees.len() == 1).then(|| callees[0].callee.fqn()));
                target.and_then(|t| self.member_of_module(*obj, attr, &t))
            }
            _ => None,
        };
        Some(CallSite {
            module_member: member,
            ..self.handed_site(span, self.f.block_loop_depth(bb), callees, constructs)
        })
    }

    /// The bases of the class statements in the function ([`Contracts::reference_bases`]), each
    /// as a reference site of the project class it is (`Base`, `core.Base`, `Base[int]`). A base
    /// is the value the statement passes ([`heki_ir::ModuleIr::class_base_values`]), typed in
    /// the statement's block as any value is, so a name it reads is what the statement's scope
    /// binds (a parameter, a local, a closure cell, a class-body name), whatever the module
    /// imports under that name. A site is at the base when the statement loads it, else at the
    /// statement (a name an import or the module body binds is read without a load of its own).
    fn base_sites(&mut self) -> Vec<CallSite> {
        let (cx, shape) = (self.cx, self.shape);
        let bases = cx
            .ir()
            .class_base_values(shape)
            .map(|b| (b.block, b.statement.span, b.value));
        let mut out = Vec::new();
        for (bb, statement, base) in bases {
            self.pos = (bb, 0);
            let named = self
                .class_called(base)
                .or_else(|| match &self.defs.get(&base)?.kind {
                    InstKind::SubscriptLoad { obj, .. } => self.class_called(*obj),
                    _ => None,
                });
            let Some(named) = named else {
                continue;
            };
            let span = self
                .defs
                .get(&base)
                .map(|def| def.span)
                .filter(|at| statement.start <= at.start && at.end <= statement.end)
                .unwrap_or(statement);
            let loop_depth = self.loops_around(bb).iter().map(|l| l.depth).max();
            out.push(CallSite {
                is_base: true,
                ..self.handed_site(span, loop_depth.unwrap_or(0), Vec::new(), Some(named))
            });
        }
        out
    }

    /// The site of a value handed on at `span`, inside `loop_depth` loops, calling `callees` or
    /// constructing `constructs` when it is called.
    fn handed_site(
        &self,
        span: Span,
        loop_depth: u16,
        callees: Vec<BoundCallee>,
        constructs: Option<String>,
    ) -> CallSite {
        CallSite {
            caller: self.key.clone(),
            target: CallTarget::Project(callees),
            is_awaited: false,
            span,
            loop_depth,
            recv_is_self: false,
            drives_coroutine_manually: false,
            constructs,
            role: SiteRole::Call,
            returned: None,
            flow: None,
            guards: Vec::new(),
            once_per: None,
            once_loops: 0,
            runner: Runner::Caller,
            binds_recv: false,
            handed_callable: None,
            per_item: None,
            dispatch: None,
            result_discarded: false,
            result_held: false,
            is_base: false,
            is_read: false,
            arguments: None,
            queues: None,
            state_tests: None,
            locks: crate::LockHeld::None,
            shield: crate::ShieldAt::Unshielded,
            // Handing a function on does not run it: no must-fact goes through the reference.
            complete: false,
            on_every_return: OnEveryReturn::default(),
            made_by: None,
            module_member: None,
        }
    }

    /// Record `inst` when it reads an attribute of a module outside the project that a contract
    /// names (`sys.platform`, or `platform` bound by `from sys import platform`). The value
    /// read must be that object by name: a value of unknown origin reads nothing, nor does a
    /// closure cell (`LoadDeref`), whose value is not typed. A name loaded only to be called
    /// there (`getframe(1)` of an imported function) is the call's, as `sys._getframe(1)` is.
    fn external_read(&mut self, inst: &InstKind, span: Span) {
        let (InstKind::AttrLoad { dst, .. }
        | InstKind::LoadGlobal { dst, .. }
        | InstKind::LoadName { dst, .. }
        | InstKind::Import { dst, .. }) = inst
        else {
            return;
        };
        if let Some(Val::Ref(fqn)) = self.types.get(dst)
            && self.contracts.is_some_and(|c| c.external_read(fqn))
            && matches!(self.cx.project.symbol(fqn), resolver::Symbol::External)
            && (matches!(inst, InstKind::Import { .. }) || !self.only_called(*dst))
        {
            self.external.push(ExternalAccess {
                kind: AccessKind::Read,
                name: fqn.clone(),
                span,
                imports_project_module: false,
            });
        }
    }

    /// Whether every use of `var` calls it, passing it nowhere.
    fn only_called(&self, var: VarId) -> bool {
        let readers = self.shape.readers(var);
        !readers.is_empty()
            && readers.iter().all(|u| {
                let Use::Inst(bb, i) = *u else {
                    return false;
                };
                matches!(
                    &self.f.block(bb).insts[i as usize].kind,
                    InstKind::Call { callee_var: Some(callee), args, kwargs, .. }
                        if *callee == var
                            && !args.contains(&var)
                            && kwargs.iter().all(|(_, k)| *k != var)
                )
            })
    }

    /// Record the imports that probe whether their module is installed
    /// ([`environment::import_probes`]): the module a plain import names (`black.mode`), or
    /// what a `from` import binds (`yaml.CSafeLoader`, a relative one resolved).
    fn import_probes(&mut self) {
        for (span, module, name) in environment::import_probes(self.f) {
            let written = match module {
                Some(m) => format!("{m}.{name}"),
                None => name.to_owned(),
            };
            // `import a.b` binds `a`, but probes `a.b`.
            let name = match module {
                None if name.contains('.') => written,
                _ => self.cx.import_target(span).map_or(written, str::to_owned),
            };
            self.external.push(ExternalAccess {
                kind: AccessKind::ImportProbe,
                name,
                span,
                imports_project_module: false,
            });
        }
    }

    /// `IterationFacts::result_iterations`: what runs between the steps of each iteration (by
    /// position, `iterated`, per project call) of the object a project call returns (a
    /// generator, or one a plain function hands on).
    fn result_iterations(&mut self, iterated: &[Vec<(Span, u16, Position)>]) {
        if iterated.iter().all(Vec::is_empty) {
            return;
        }
        let site_spans: FxHashSet<Span> = self.sites.iter().map(|s| s.span).collect();
        let objects = CallObjects::new(self);
        // The calls whose results each iteration steps, by its position: one fact per iteration.
        let mut stepping: BTreeMap<Position, Vec<(Span, &ProjectCall)>> = BTreeMap::new();
        for (call, uses) in self.project_calls.iter().zip(iterated) {
            for &(_, _, at) in uses {
                let span = self.sites[call.site].span;
                stepping.entry(at).or_default().push((span, call));
            }
        }
        let mut found = Vec::new();
        for ((block, index), calls) in stepping {
            let inst = &self.f.block(block).insts[index];
            if let Some(between) =
                BetweenSteps::of(self.f, inst, &site_spans, self.summary.is_generator)
            {
                found.push(objects.iteration(between, &calls));
            }
        }
        self.summary
            .iteration_facts_mut()
            .result_iterations
            .extend(found);
    }

    /// Where the function iterates its parameters and the objects its project calls return
    /// (`consumption::IterFlow`): the parameter facts (`consumes_params`,
    /// `unconditional_forwards`, `iterates_params`), and one more site of a call's callees
    /// wherever the object it returns is iterated, returned by this (plain) function, or passed
    /// to a project call (`SiteRole`). Any other use (stored, passed to an external or
    /// unresolved call, yielded, stepped with `next()`) runs nothing here; a result that outlives
    /// one of its uses is `CallSite::result_held`.
    fn iterations(&mut self) {
        let f = self.f;
        if f.params.is_empty() && self.project_calls.is_empty() {
            return;
        }
        let callees = std::mem::take(&mut self.callees);
        let flow =
            IterFlow::with_callees(self.shape, self.cx, &self.types, Roots::Summary, callees);
        let it = param_iteration(self.shape, &flow);
        self.summary.consumes_params = it.consumes;
        self.summary.unconditional_forwards = it.unconditional;
        self.summary.iterates_params = it.iterates;
        if self.project_calls.is_empty() {
            return;
        }
        let by_dst: FxHashMap<VarId, usize> = self
            .project_calls
            .iter()
            .enumerate()
            .map(|(i, c)| (c.dst, i))
            .collect();
        let calls_in = |roots: &[VarId]| -> Vec<usize> {
            roots
                .iter()
                .filter_map(|r| by_dst.get(r).copied())
                .collect()
        };
        // Where each call's result is iterated.
        // With the position of the iteration (for `MUT208`'s flow at the site).
        let mut iterated: Vec<Vec<(Span, u16, Position)>> =
            vec![Vec::new(); self.project_calls.len()];
        for e in &flow.events {
            let (span, depth) = (e.span, self.shape.loop_depth(e.block));
            for c in calls_in(&e.stepped) {
                if !iterated[c].iter().any(|i| (i.0, i.1) == (span, depth)) {
                    iterated[c].push((span, depth, (e.block, e.index)));
                }
            }
        }
        if self.opt_in.iterations {
            self.result_iterations(&iterated);
        }
        // Which call results a plain function returns (a generator's `return` value ends its
        // iteration and is no iterator handed out).
        let mut returned = vec![false; self.project_calls.len()];
        let plain = f.kind == FunctionKind::Sync && !f.is_module_body;
        if plain {
            for bb in &f.blocks {
                if let Terminator::Return(Some(v)) = bb.terminator {
                    for c in calls_in(&flow.expand_steps(&[v])) {
                        returned[c] = true;
                    }
                }
            }
        }
        if self.opt_in.iterations {
            let results: FxHashSet<VarId> = by_dst.keys().copied().collect();
            let held = iterations::held_results(f, &flow, &results, plain);
            for call in &self.project_calls {
                if held.contains(&call.dst) {
                    self.sites[call.site].result_held = true;
                }
            }
        }
        // Each site with the position where the callee's body runs into this function's body
        // (`None`: it runs where this function's result is iterated).
        let mut extra = Vec::new();
        for (c, call) in self.project_calls.iter().enumerate() {
            let site = &self.sites[call.site];
            for &(span, depth, at) in &iterated[c] {
                extra.push((
                    role_site(site, SiteRole::Iterate, span, depth, None),
                    Some(at),
                ));
            }
            if returned[c] {
                extra.push((
                    role_site(site, SiteRole::Return, site.span, site.loop_depth, None),
                    None,
                ));
            }
        }
        // Coroutines awaited after they are made (`pending = work(); ...; await pending`): the
        // body runs at the `await`, as for `await work()`.
        for bb in &f.blocks {
            for (index, inst) in bb.insts.iter().enumerate() {
                let InstKind::Await { value, .. } = &inst.kind else {
                    continue;
                };
                for c in calls_in(&flow.expand_steps(&[*value])) {
                    let site = &self.sites[self.project_calls[c].site];
                    if !site.is_awaited {
                        let depth = self.shape.loop_depth(bb.id);
                        extra.push((
                            role_site(site, SiteRole::Call, inst.span, depth, Some((true, false))),
                            Some((bb.id, index)),
                        ));
                    }
                }
            }
        }
        // Results passed to a project call (`consume(rows())`, `chunked(rows(), 100)`).
        for (x, call) in self.project_calls.iter().enumerate() {
            let mut via_by_arg: Vec<(VarId, Vec<(FuncKey, u8)>)> = Vec::new();
            for (key, bound) in &call.params {
                for &(param, arg) in bound {
                    match via_by_arg.iter_mut().find(|(a, _)| *a == arg) {
                        Some((_, via)) => via.push((key.clone(), param)),
                        None => via_by_arg.push((arg, vec![(key.clone(), param)])),
                    }
                }
            }
            let x_site = &self.sites[call.site];
            for (arg, via) in via_by_arg {
                for y in calls_in(&flow.expand_steps(&[arg]))
                    .into_iter()
                    .filter(|&y| y != x)
                {
                    let y_site = &self.sites[self.project_calls[y].site];
                    // Already iterated right there (a 3.11 comprehension frame `__comp_<id>(it)`).
                    if !iterated[y].iter().any(|(s, _, _)| *s == x_site.span) {
                        let role = SiteRole::Through {
                            via: via.clone(),
                            iterated: false,
                            returned: false,
                        };
                        let flags = Some((x_site.is_awaited, x_site.drives_coroutine_manually));
                        extra.push((
                            role_site(y_site, role, x_site.span, x_site.loop_depth, flags),
                            Some(call.at),
                        ));
                    }
                    for &(span, depth, at) in &iterated[x] {
                        let role = SiteRole::Through {
                            via: via.clone(),
                            iterated: true,
                            returned: false,
                        };
                        extra.push((role_site(y_site, role, span, depth, None), Some(at)));
                    }
                    // A returned adapter iterates its input where the caller's result is
                    // iterated.
                    if returned[x] {
                        let role = SiteRole::Through {
                            via: via.clone(),
                            iterated: true,
                            returned: true,
                        };
                        extra.push((
                            role_site(y_site, role, x_site.span, x_site.loop_depth, None),
                            None,
                        ));
                    }
                }
            }
        }
        let base = self.sites.len();
        for (k, (site, at)) in extra.into_iter().enumerate() {
            if let (Some(events), Some((b, i))) = (self.events.as_mut(), at) {
                events.calls.push((b, i, base + k));
            }
            self.sites.push(site);
        }
    }

    /// [`EffectSummary::returns_self_attrs`] of a `@cached_property` getter, from its own body
    /// (the reads of the `self` methods and properties it calls are taken away when its final
    /// summary is computed, `callgraph::apply_edge`).
    fn returned_self_attrs(&self) -> BTreeSet<String> {
        let f = self.f;
        let Some(loads) = self.self_attr_loads().filter(|l| !l.is_empty()) else {
            return BTreeSet::new();
        };
        let mut phi_uses: FxHashMap<VarId, Vec<VarId>> = FxHashMap::default();
        let mut tested: FxHashSet<VarId> = FxHashSet::default();
        for bb in &f.blocks {
            for phi in &bb.phis {
                for (_, v) in &phi.incoming {
                    phi_uses.entry(*v).or_default().push(phi.dst);
                }
            }
            if let Terminator::Branch { cond: v, .. } | Terminator::Raise(Some(v)) = bb.terminator {
                tested.insert(v);
            }
        }
        // The attributes each value may be or hold, from the loads forward: through copies and
        // phis, into displays (not as a starred element), dict values, and containers filled by
        // item assignment or `append` / `add` / `insert`. Any other use reads the attributes.
        let mut held: FxHashMap<VarId, BTreeSet<&str>> = FxHashMap::default();
        let mut work: Vec<VarId> = Vec::new();
        for &(v, attr) in &loads {
            held.entry(v).or_default().insert(attr);
            work.push(v);
        }
        let mut read: BTreeSet<&str> = BTreeSet::new();
        while let Some(v) = work.pop() {
            let attrs = held[&v].clone();
            // The value `v` is put into `dst`.
            let mut into: Vec<VarId> = phi_uses.get(&v).cloned().unwrap_or_default();
            let mut read_here = tested.contains(&v);
            for kind in self.uses.get(&v).into_iter().flatten() {
                match kind {
                    InstKind::Copy { dst, .. } if !f.is_unpacked(*dst) => into.push(*dst),
                    InstKind::AllocBuiltin {
                        dst,
                        kind: BuiltinKind::List | BuiltinKind::Tuple | BuiltinKind::Set,
                        ..
                    } => into.push(*dst),
                    InstKind::DictLiteral { dst, entries }
                        if entries.iter().all(|e| match e {
                            DictEntry::KeyValue { key, .. } => *key != v,
                            DictEntry::Unpack { dict } => *dict != v,
                        }) =>
                    {
                        into.push(*dst);
                    }
                    // Exactly one of the container and the stored value is `v`.
                    InstKind::SubscriptStore {
                        obj, index, value, ..
                    } if *index != v && (*obj == v) != (*value == v) => {
                        if *value == v {
                            into.push(*obj);
                        }
                    }
                    InstKind::MethodCall {
                        recv,
                        method,
                        args,
                        kwargs,
                        ..
                    } if kwargs.is_empty() => {
                        let value_at = match method.as_str() {
                            "append" | "add" => Some(0),
                            "insert" => Some(1),
                            _ => None,
                        };
                        let as_arg: Vec<usize> =
                            (0..args.len()).filter(|&i| args[i] == v).collect();
                        match (*recv == v, as_arg.as_slice()) {
                            // Filling the container with other values.
                            (true, [])
                                if value_at.is_some()
                                    || method == "extend"
                                    || method == "update" => {}
                            // Put into the receiver, as itself.
                            (false, [i]) if value_at == Some(*i) => into.push(*recv),
                            _ => read_here = true,
                        }
                    }
                    _ => read_here = true,
                }
            }
            if read_here {
                read.extend(attrs.iter().copied());
            }
            for dst in into {
                let to = held.entry(dst).or_default();
                let before = to.len();
                to.extend(attrs.iter().copied());
                if to.len() != before {
                    work.push(dst);
                }
            }
        }
        loads
            .iter()
            .map(|(_, attr)| *attr)
            .filter(|attr| !read.contains(attr))
            .map(str::to_owned)
            .collect()
    }

    /// The reads `self.<attr>` of a method that hands `self` to no other code, each with the
    /// value it defines: `self` is only read from, written to, and called on (project methods,
    /// whose reads count), never merged with another value, tested, raised, or returned. `None`
    /// when `self` goes anywhere else (code that gets it may read any attribute) or its
    /// `__dict__` is read.
    fn self_attr_loads(&self) -> Option<Vec<(VarId, &'a str)>> {
        let f = self.f;
        let is_self = |v: &VarId| self.self_vars.contains(v);
        let mut loads = Vec::new();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            let allowed: Option<VarId> = match &inst.kind {
                InstKind::AttrLoad { obj, .. } | InstKind::AttrDelete { obj, .. } => Some(*obj),
                InstKind::AttrStore { obj, value, .. } if !is_self(value) => Some(*obj),
                InstKind::Copy { dst, src } if is_self(dst) => Some(*src),
                InstKind::MethodCall { recv, method, .. }
                    if is_self(recv) || matches!(self.val(*recv), Some(Val::Super(_))) =>
                {
                    if !matches!(self.method_callee(*recv, method), Callee::Project(..)) {
                        return None;
                    }
                    Some(*recv)
                }
                _ => None,
            };
            if inst
                .kind
                .operands()
                .iter()
                .any(|v| is_self(v) && Some(*v) != allowed)
            {
                return None;
            }
            if let InstKind::AttrLoad { dst, obj, attr } = &inst.kind
                && is_self(obj)
            {
                if attr == "__dict__" {
                    return None;
                }
                loads.push((*dst, attr.as_str()));
            }
        }
        for bb in &f.blocks {
            let merged = bb
                .phis
                .iter()
                .any(|phi| !is_self(&phi.dst) && phi.incoming.iter().any(|(_, v)| is_self(v)));
            let escapes = match bb.terminator {
                Terminator::Return(Some(v))
                | Terminator::Branch { cond: v, .. }
                | Terminator::Raise(Some(v)) => is_self(&v),
                _ => false,
            };
            if merged || escapes {
                return None;
            }
        }
        Some(loads)
    }

    /// [`EffectSummary::hashed_whole_self_attrs`] of a `__hash__` method, from its own body
    /// (the reads of the `self` methods and properties it calls are taken away when its final
    /// summary is computed, `EffectSummary::add_self_reads`).
    fn hashed_whole_self_attrs(&self) -> BTreeSet<String> {
        let f = self.f;
        let Some(loads) = self.self_attr_loads() else {
            return BTreeSet::new();
        };
        // Values a phi merges or a terminator tests, raises, or returns.
        let mut escaping: FxHashSet<VarId> = FxHashSet::default();
        for bb in &f.blocks {
            escaping.extend(
                bb.phis
                    .iter()
                    .flat_map(|p| p.incoming.iter().map(|(_, v)| *v)),
            );
            if let Terminator::Return(Some(v))
            | Terminator::Branch { cond: v, .. }
            | Terminator::Raise(Some(v)) = bb.terminator
            {
                escaping.insert(v);
            }
        }
        let uses = |v: VarId| self.uses.get(&v).into_iter().flatten().copied();
        let hashes = |kind: &InstKind, v: VarId| match kind {
            InstKind::Call {
                callee_var: Some(c),
                args,
                kwargs,
                ..
            } => {
                args.as_slice() == [v]
                    && kwargs.is_empty()
                    && matches!(self.val(*c), Some(Val::Ref(s)) if s == "builtins.hash")
            }
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            } => *recv == v && method == "__hash__" && args.is_empty() && kwargs.is_empty(),
            _ => false,
        };
        // Every use of the value read (through copies) hashes it, or puts it into a tuple
        // display that is only hashed.
        let hashed_whole = |load: VarId| {
            let mut work = vec![load];
            let mut seen: FxHashSet<VarId> = std::iter::once(load).collect();
            while let Some(v) = work.pop() {
                if escaping.contains(&v) {
                    return false;
                }
                for kind in uses(v) {
                    match kind {
                        InstKind::Copy { dst, .. } if !f.is_unpacked(*dst) => {
                            // Copies in dead code can close a cycle.
                            if seen.insert(*dst) {
                                work.push(*dst);
                            }
                        }
                        InstKind::AllocBuiltin {
                            dst,
                            kind: BuiltinKind::Tuple,
                            ..
                        } if !escaping.contains(dst) && uses(*dst).all(|k| hashes(k, *dst)) => {}
                        kind if hashes(kind, v) => {}
                        _ => return false,
                    }
                }
            }
            true
        };
        let mut whole: BTreeSet<&str> = BTreeSet::new();
        let mut read_otherwise: BTreeSet<&str> = BTreeSet::new();
        for &(v, attr) in &loads {
            if hashed_whole(v) {
                whole.insert(attr);
            } else {
                read_otherwise.insert(attr);
            }
        }
        whole
            .difference(&read_otherwise)
            .map(|a| (*a).to_owned())
            .collect()
    }

    /// `MUT112`: what the object the function returns may be and the project calls whose results
    /// it may be ([`EffectSummary::return_origins`]), and what each argument of those calls may
    /// be, in turn through the calls whose results the arguments may be ([`ArgOrigin::origins`]:
    /// a callee returning its parameter returns the argument). A memoizing decorator returns
    /// state it keeps; another decorator may return anything.
    fn returned_origins(&mut self) {
        let f = self.f;
        if f.kind != FunctionKind::Sync || f.is_module_body {
            return;
        }
        if self.summary.is_lru_cache {
            self.summary.return_origins = Some(Box::new(ReturnOrigins {
                returned: LocalOrigins {
                    known: Origins {
                        held: true,
                        ..Origins::default()
                    },
                    ..LocalOrigins::default()
                },
                nodes: Vec::new(),
            }));
            return;
        }
        let live = &self.live;
        let returned: Vec<VarId> = f
            .blocks
            .iter()
            .filter(|b| live.block(b.id))
            .filter_map(|b| match b.terminator {
                Terminator::Return(Some(v)) => Some(v),
                _ => None,
            })
            .collect();
        if returned.is_empty() || self.summary.decorator_may_replace {
            return;
        }
        let callees = self.callees_by_result();
        let phis: FxHashMap<VarId, (BlockId, &PhiNode)> = f
            .blocks
            .iter()
            .flat_map(|b| b.phis.iter().map(move |p| (p.dst, (b.id, p))))
            .collect();
        let params: FxHashMap<VarId, ParamRole> = f
            .params
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let role = if p.is_vararg || p.is_kwarg {
                    ParamRole::Packed
                } else {
                    ParamRole::Plain(u8::try_from(i).ok()?)
                };
                Some((p.var, role))
            })
            .collect();
        let source_of = |inst: &'a Inst| self.ownership_source(inst, &callees);
        let slice_copies = |v: VarId| {
            ownership::slice_copies(match self.types.get(&v) {
                Some(Val::Inst(c, _)) => Some(c.as_str()),
                _ => None,
            })
        };
        // The objects the function stores somewhere, and where: a stored value, and each value
        // it copies through copies and phis, is kept there. One walk serves every store and
        // takes each (value, place) pair once; past a budget of pairs proportional to the
        // function's source, what it returns is unknown.
        let budget = KEPT_PAIRS_PER_SOURCE_BYTE * f.span.end.saturating_sub(f.span.start) as usize
            + MIN_KEPT_PAIRS;
        let mut kept: FxHashMap<VarId, Vec<ownership::Place<'_>>> = FxHashMap::default();
        let mut seen: FxHashSet<(VarId, ownership::Place<'_>)> = FxHashSet::default();
        let mut work: Vec<(VarId, ownership::Place<'_>)> = f
            .blocks
            .iter()
            .filter(|b| live.block(b.id))
            .flat_map(|b| &b.insts)
            .flat_map(|inst| ownership::stores(&inst.kind))
            .collect();
        while let Some((v, place)) = work.pop() {
            if !seen.insert((v, place)) {
                continue;
            }
            if seen.len() > budget {
                return;
            }
            kept.entry(v).or_default().push(place);
            match (self.defs.get(&v).map(|i| &i.kind), phis.get(&v)) {
                (Some(InstKind::Copy { src, .. }), _) => work.push((*src, place)),
                (None, Some((_, phi))) => {
                    work.extend(phi.incoming.iter().map(|(_, x)| (*x, place)));
                }
                _ => {}
            }
        }
        let mut nodes = ownership::OriginNodes::default();
        let origins = |roots: &[(VarId, ownership::Part)], nodes: &mut ownership::OriginNodes| {
            ownership::origins_of(
                &self.defs,
                &phis,
                live,
                roots,
                source_of,
                |v| params.get(&v).copied(),
                slice_copies,
                &kept,
                nodes,
            )
        };
        let roots: Vec<(VarId, ownership::Part)> = returned
            .iter()
            .map(|v| (*v, ownership::Part::Object))
            .collect();
        let returns = origins(&roots, &mut nodes);
        // The arguments of the project calls whose results the function may return, and in
        // turn of the calls whose results those arguments or the phis the walks meet may be.
        let mut calls_at: FxHashMap<Span, Vec<&ProjectCall>> = FxHashMap::default();
        for call in &self.project_calls {
            calls_at
                .entry(self.sites[call.site].span)
                .or_default()
                .push(call);
        }
        let budget = ORIGIN_STEPS_PER_SOURCE_BYTE
            * f.span.end.saturating_sub(f.span.start) as usize
            + MIN_ORIGIN_STEPS;
        let mut queue: Vec<Span> = returns.results.iter().map(|r| r.span).collect();
        let mut done: FxHashSet<Span> = FxHashSet::default();
        let mut args: Vec<(usize, usize, usize, LocalOrigins)> = Vec::new();
        loop {
            if nodes.steps > budget {
                return;
            }
            if let Some(span) = queue.pop() {
                if !done.insert(span) {
                    continue;
                }
                for call in calls_at.get(&span).into_iter().flatten() {
                    for (ci, (_, bound)) in call.params.iter().enumerate() {
                        for (slot, var) in bound {
                            let o = origins(&[(*var, ownership::Part::Object)], &mut nodes);
                            queue.extend(o.results.iter().map(|r| r.span));
                            if !o.is_empty() {
                                args.push((call.site, ci, usize::from(*slot), o));
                            }
                        }
                    }
                }
                continue;
            }
            let Some((id, root)) = nodes.next_pending() else {
                break;
            };
            let o = origins(&[root], &mut nodes);
            queue.extend(o.results.iter().map(|r| r.span));
            nodes.set(id, o);
        }
        let facts = ReturnOrigins {
            returned: returns,
            nodes: nodes.into_origins(),
        };
        self.summary.return_origins = (!facts.is_empty()).then(|| Box::new(facts));
        for (site, ci, slot, o) in args {
            let callees = self.sites[site]
                .project_callees_mut()
                .expect("a project call's site has project callees");
            // Every bound slot carries its binding, so the binder kept it in `params`.
            callees[ci].params[slot].origins = Some(Box::new(o));
        }
    }

    /// [`ownership::source`] of `inst`, with the project callees of the function's calls by
    /// their results (`callees`).
    fn ownership_source(&self, inst: &'a Inst, callees: &FxHashMap<VarId, &Callee>) -> Source<'a> {
        self.ownership_source_given(inst, callees, &|v| self.made_here(v, callees))
    }

    /// [`ownership::made_here`] of the object `var` holds.
    fn made_here(&self, var: VarId, callees: &FxHashMap<VarId, &Callee>) -> bool {
        ownership::made_here(self.shape.copy_root(var), &self.defs, &|i, made_here| {
            self.ownership_source_given(i, callees, made_here)
        })
    }

    /// [`Self::ownership_source`], where `made_here` tells the values that hold an object the
    /// function makes itself.
    fn ownership_source_given(
        &self,
        inst: &'a Inst,
        callees: &FxHashMap<VarId, &Callee>,
        made_here: &dyn Fn(VarId) -> bool,
    ) -> Source<'a> {
        let callee = inst.kind.dst().and_then(|d| callees.get(&d).copied());
        match (&inst.kind, callee) {
            // A property read calls its getter.
            (InstKind::AttrLoad { .. }, Some(Callee::Project(..))) => Source::Project,
            (kind, callee) => {
                let constructs = self.construction(kind).is_some();
                ownership::source(self.f, kind, callee, constructs, self.cx.kb, made_here)
            }
        }
    }

    /// The local fact of [`crate::OptInTables::fresh_returns`] of the visited function, from
    /// the callees its calls and property reads resolve to.
    fn fresh_return(&self) -> Option<fresh::FreshReturn> {
        let shape = self.shape;
        let class_of = |v: VarId| match self.types.get(&v) {
            Some(Val::Inst(class, _)) => Some(class.as_str()),
            _ => None,
        };
        let source_of = |v: VarId| {
            let Def::Inst(b, i) = shape.def(v) else {
                return None;
            };
            let at = (b, i as usize);
            let callee = self.callees.get(&at);
            let complete =
                matches!(callee, Some(Callee::Project(..))) && self.targets_complete_at(at);
            fresh::source(
                &shape.inst(v)?.kind,
                callee,
                complete,
                self.types.get(&v),
                class_of,
                self.cx.kb,
            )
        };
        let inspects = |b: BlockId, i: u32| {
            matches!(
                self.callees.get(&(b, i as usize)),
                Some(Callee::External(s)) if inspecting_builtin(s, true)
            )
        };
        fresh::fresh_return(shape, &self.summary, &self.live, source_of, inspects)
    }

    /// Answer the construction queries of the visited function (`construction`): the
    /// construction of its typed writes, `ArgOrigin::new_object` of its call arguments, and
    /// `EffectSummary::returns_new`.
    fn resolve_construction(&mut self) {
        let mut rec = std::mem::take(&mut self.rec);
        for (item, elem) in &self.unpacked {
            rec.handed_back(*item, *elem);
        }
        let param_alias = &self.param_alias;
        let param_of = |v: VarId| match param_alias.get(&v) {
            Some((i, None)) => Some(*i),
            _ => None,
        };
        // Calling a generator or an `async def` returns a generator or a coroutine.
        let returns = self.f.kind == FunctionKind::Sync
            && !self.f.is_module_body
            && !self.summary.decorator_may_replace;
        let sites = &self.sites;
        // Hand-overs are to project callees of the function's own call sites.
        let handed_to = |site: u32, callee: u16, known: &[InputGuard]| {
            let bc = &sites[site as usize].project_callees()[usize::from(callee)];
            (bc.callee.clone(), CallTests::of_call(&bc.params, known))
        };
        let element_of = |v: VarId| self.element_of_params(v);
        let shape = self.shape;
        let unpacked = &self.unpacked;
        // An unpacked item is its element (`Recorder::handed_back`), no part of the display.
        let reads_of = |v: VarId, out: &mut Vec<VarId>| {
            if let Some(inst) = shape.inst(v)
                && !unpacked.contains_key(&v)
                && !self.holds_no_operand_part(v)
            {
                match self.read_out_of(&inst.kind) {
                    Some(read) => out.push(read),
                    None => out.extend(inst.kind.operands()),
                }
            }
        };
        // A store into a container hands the value to code of the project when the call does,
        // on the receiver's class, the items', or the values handed; an item store, only when
        // the container's `__setitem__` does, and for the key also when hashing it or comparing
        // it with the keys held does.
        rec.adds_running_code(|pos, var| match &self.added[&pos] {
            AddedBy::Call {
                symbol,
                recv,
                args,
                kwargs,
                star,
            } => self.library_call_runs_project_code(
                symbol,
                None,
                Some(*recv),
                args,
                kwargs,
                *star,
                false,
            ),
            AddedBy::Item { obj, value } => {
                let kind = &self.f.block(BlockId(pos.0)).insts[pos.1 as usize].kind;
                let mut specials = self.f.special_methods(kind);
                if var == *value {
                    specials.retain(|s| s.value == *obj);
                    for s in &mut specials {
                        s.keyed = false;
                    }
                }
                self.specials_run_project_code(&specials)
            }
        });
        let made = |class: &str, attr: &str| {
            let Constructor::KeepsNothing { containers } = self.constructor(class) else {
                return None;
            };
            let kind = containers
                .iter()
                .find_map(|(a, k)| (&**a == attr).then_some(*k))?;
            (self.cx.project)
                .attr_read_finds_stored(class, attr, |base| self.catalog_stores(base))
                .then_some(kind)
        };
        let (answers, returned) = rec.resolve(
            self.shape,
            &self.may_params,
            &param_of,
            (&element_of, &reads_of),
            &handed_to,
            &made,
            returns,
        );
        for p in std::mem::take(&mut self.pending_sites) {
            let mut site = p.site;
            site.construction = construction_of(&answers[p.query], p.rebinds);
            self.summary.typed_field_write_sites.insert(site);
        }
        for (mut site, q) in std::mem::take(&mut self.pending_unresolved) {
            site.construction = construction_of(&answers[q], site.kind.rebinds());
            self.summary.unresolved_write_sites.insert(site);
        }
        for (si, ci, pi, q) in std::mem::take(&mut self.pending_args) {
            if let NewArg::New { .. } | NewArg::Param { .. } = &answers[q] {
                let callees = self.sites[si]
                    .project_callees_mut()
                    .expect("only the arguments of project callees are pending");
                let params = &mut callees[ci].params;
                if params.len() <= pi {
                    params.resize(pi + 1, ArgOrigin::default());
                }
                params[pi].new_object = answers[q].clone();
            }
        }
        if returned != ReturnsNew::No {
            self.summary.returns_new = returned;
        }
    }

    /// The parameters the object `var` holds is an element of by its definition (`0`: none): an
    /// item, `get`, `pop`, `setdefault`, or an iteration step ([`Source::Element`]) of a
    /// parameter itself, or of an iterator over one.
    fn element_of_params(&self, var: VarId) -> u64 {
        let shape = self.shape;
        let (Def::Inst(b, i), Some(inst)) = (shape.def(var), shape.inst(var)) else {
            return 0;
        };
        let callee = self.callees.get(&(b, i as usize));
        let constructs = self.construction(&inst.kind).is_some();
        let made_here = |_: VarId| false;
        let source = ownership::source(
            self.f, &inst.kind, callee, constructs, self.cx.kb, &made_here,
        );
        let Source::Element(of) = source else {
            return 0;
        };
        let holder = match shape.inst(of).map(|i| &i.kind) {
            Some(InstKind::MakeIter { iterable, .. }) => *iterable,
            _ => of,
        };
        match self.param_alias.get(&holder) {
            Some((p, None)) if *p < 64 => 1u64 << p,
            _ => 0,
        }
    }

    /// Record an escape of `var`'s object at the current instruction; a definite escape of a
    /// parameter, or its store in another object, keeps it (`EffectSummary::retains_params`).
    /// Only reached when an ownership contract is declared ([`Self::store_escapes`],
    /// [`Self::call_escapes`]).
    fn escape(&mut self, var: VarId, how: Escape) {
        let params = self.may_params.get(&var).copied().unwrap_or(0);
        let recorded = self.track && (params != 0 || self.may_hold_or_read_new(var));
        let keeps_params = !matches!(how, Escape::Handed(..)) && params != 0;
        if !recorded && !keeps_params {
            return;
        }
        let known = self.guards.facts_at(&self.guard_inputs(), self.pos.0);
        if keeps_params {
            self.summary
                .keep_params(params, &MayGiven::given(known.clone()));
        }
        if recorded {
            self.rec.escape(self.at(), var, how, known);
        }
    }

    /// [`Self::unpacked`]: a tuple or list display without `*` whose every use is a read of one
    /// of its items by a target unpacking ([`crate::consumption::is_unpacking`]).
    fn build_unpacked_displays(&mut self) {
        let shape = self.shape;
        let mut reads: FxHashMap<VarId, Vec<(i64, VarId)>> = FxHashMap::default();
        for inst in self.f.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::SubscriptLoad {
                dst,
                obj,
                index,
                is_slice: false,
                ..
            } = &inst.kind
                && crate::consumption::is_unpacking(shape, *index, inst.span)
                && let Some(InstKind::Const {
                    literal: ConstLiteral::Int(i),
                    ..
                }) = shape.inst(*index).map(|i| &i.kind)
            {
                reads.entry(*obj).or_default().push((*i, *dst));
            }
        }
        if reads.is_empty() {
            return;
        }
        let in_phi: FxHashSet<VarId> = (self.f.blocks.iter().flat_map(|b| &b.phis))
            .flat_map(|p| p.incoming.iter().map(|(_, v)| *v))
            .collect();
        for inst in self.f.blocks.iter().flat_map(|b| &b.insts) {
            let InstKind::AllocBuiltin {
                dst,
                kind: BuiltinKind::Tuple | BuiltinKind::List,
                elems,
            } = &inst.kind
            else {
                continue;
            };
            let Some(items) = reads.get(dst) else {
                continue;
            };
            // A starred element (`*xs, Node()`) adds as many items as `xs` holds, so a target's
            // position is no element's.
            if in_phi.contains(dst)
                || shape.first_starred(*dst).is_some()
                || self.uses.get(dst).map_or(0, Vec::len) != items.len()
            {
                continue;
            }
            // An item past the display's end raises before any target is bound.
            let element = |i: i64| usize::try_from(i).ok().and_then(|i| elems.get(i));
            self.unpacked_displays.insert(*dst);
            self.unpacked
                .extend((items.iter()).filter_map(|(i, item)| Some((*item, *element(*i)?))));
        }
    }

    /// [`Self::track`], and [`Self::maybe_new`] when it is set.
    fn may_construct(&mut self) -> bool {
        self.build_unpacked_displays();
        let mut seeds: FxHashSet<VarId> = FxHashSet::default();
        for i in self.f.blocks.iter().flat_map(|b| &b.insts) {
            // Any instance result (a superset: the recorder learns the allocations).
            let instance = |v: &VarId| matches!(self.types.get(v), Some(Val::Inst(_, _)));
            let maybe = match &i.kind {
                InstKind::Call { dst, callee_var, .. } => {
                    (instance(dst)
                        || callee_var.is_some_and(|c| {
                            matches!(self.types.get(&c), Some(Val::Ref(s)) if matches!(s.as_str(), "copy.copy" | "copy.deepcopy" | "dataclasses.replace"))
                        }))
                    .then_some(*dst)
                }
                InstKind::MethodCall { dst, method, .. } => (instance(dst)
                    || matches!(
                        method.as_str(),
                        "copy" | "deepcopy" | "model_copy" | "__copy__" | "__deepcopy__" | "replace"
                    ))
                .then_some(*dst),
                _ => None,
            };
            seeds.extend(maybe);
        }
        let name = func_qualname(self.f);
        let synthetic = name.rsplit('.').next().is_some_and(|n| n.starts_with('<'));
        let private = crate::callgraph::is_private_name(name) || synthetic && name != "<module>";
        self.maybe_new = seeds;
        private || !self.maybe_new.is_empty() && self.constructs_something()
    }

    /// Whether a public function can construct what its contracts see: an object that may be
    /// new is written (an attribute, `setattr`, a container one of its attributes holds),
    /// handed to a private helper (whose writes are reported here), or returned (a factory).
    fn constructs_something(&self) -> bool {
        let private = |name: &str| crate::callgraph::is_private_name(name);
        let mutated = |var: &VarId| {
            self.uses.get(var).into_iter().flatten().any(|u| match u {
                InstKind::MethodCall { recv, method, .. } => {
                    recv == var && container_mutator(method)
                }
                InstKind::SubscriptStore { obj, .. }
                | InstKind::SubscriptDelete { obj, .. }
                | InstKind::SubscriptLoad { obj, .. } => obj == var,
                _ => false,
            })
        };
        let returned = self.f.blocks.iter().any(
            |b| matches!(b.terminator, heki_ir::Terminator::Return(Some(v)) if self.may_be_new(v)),
        );
        returned
            || self
                .f
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(|i| match &i.kind {
                    InstKind::AttrStore { obj, .. } | InstKind::AttrDelete { obj, .. } => {
                        self.may_be_new(*obj)
                    }
                    InstKind::AttrLoad { dst, obj, .. } => self.may_be_new(*obj) && mutated(dst),
                    InstKind::MethodCall {
                        recv, method, args, ..
                    } => {
                        private(method)
                            && (self.may_be_new(*recv) || args.iter().any(|a| self.may_be_new(*a)))
                    }
                    InstKind::Call {
                        callee,
                        args,
                        kwargs,
                        ..
                    } => {
                        let last = callee.rsplit('.').next().unwrap_or(callee);
                        (private(last) || last == "setattr")
                            && args
                                .iter()
                                .chain(kwargs.iter().map(|(_, v)| v))
                                .any(|a| self.may_be_new(*a))
                    }
                    _ => false,
                })
    }

    /// Whether `var` may hold what a call in [`Self::maybe_new`] returned: through copies; a
    /// phi (no defining instruction) may.
    fn may_be_new(&self, var: VarId) -> bool {
        let mut cur = var;
        for _ in 0..16 {
            if let Some(elem) = self.unpacked.get(&cur) {
                cur = *elem;
                continue;
            }
            match self.defs.get(&cur).map(|i| &i.kind) {
                Some(InstKind::Copy { src, .. }) => cur = *src,
                Some(_) => return self.maybe_new.contains(&cur),
                None => return !self.maybe_new.is_empty(),
            }
        }
        false
    }

    /// Whether the value `v` holds no part of the operands of its definition: it is established
    /// to be a builtin value of its exact class that holds only such values ([`Built::Atomic`]),
    /// such as a constant, the `bool` of `not`, `is`, or `in`, an f-string's new `str`, or an
    /// operator or a conversion whose operands' classes run only `object`'s or a builtin type's
    /// special methods for it. A project class's operator may return anything, an object read
    /// out of its operand too (`__neg__` returning `self.parent`).
    fn holds_no_operand_part(&self, v: VarId) -> bool {
        self.built(v) == Built::Atomic
    }

    /// The one operand the result of `kind` is read out of when no other operand can be part of
    /// it: the object of an attribute read, the iterable or the iterator of an iteration step,
    /// and the object of an item looked up by a builtin type's own lookup, which gives an item
    /// the object holds and never the key: every class the object may be (R2) runs `object`'s
    /// or a builtin type's `__getitem__` and `__missing__` ([`invokes::interpreter_own`]). A
    /// project class's `__getitem__` may return its key.
    fn read_out_of(&self, kind: &InstKind) -> Option<VarId> {
        match kind {
            InstKind::AttrLoad { obj, .. }
            | InstKind::MakeIter { iterable: obj, .. }
            | InstKind::IterNext { iter: obj, .. } => Some(*obj),
            InstKind::SubscriptLoad { obj, .. } => invokes::interpreter_own(
                self.cx.project,
                self.types.get(obj),
                &["__getitem__", "__missing__"],
                false,
            )
            .then_some(*obj),
            _ => None,
        }
    }

    /// Whether `var` may hold what a call in [`Self::maybe_new`] returned, or a part of it:
    /// through copies and the value an attribute read, an item, or an iteration step reads its
    /// result out of ([`Self::read_out_of`]); anything else past the first step, a phi, or a
    /// definition with operands may. The resolution leaves out the operands of a value that
    /// holds no part of them ([`Self::holds_no_operand_part`]), which is asked only after the
    /// visit: it reads the callees the visit records.
    fn may_hold_or_read_new(&self, var: VarId) -> bool {
        if self.maybe_new.is_empty() {
            return false;
        }
        let mut cur = var;
        for _ in 0..16 {
            if let Some(elem) = self.unpacked.get(&cur) {
                cur = *elem;
                continue;
            }
            match self.defs.get(&cur).map(|i| &i.kind) {
                Some(InstKind::Copy { src, .. }) => cur = *src,
                Some(kind) => match self.read_out_of(kind) {
                    Some(next) => cur = next,
                    None => return self.maybe_new.contains(&cur) || !kind.operands().is_empty(),
                },
                None => return true,
            }
        }
        true
    }

    /// What a construction of the project class `class` keeps of its instance
    /// ([`resolver::ProjectDecls::constructor`]).
    fn constructor(&self, class: &str) -> Constructor<'a> {
        if let Some(known) = self.constructors.borrow().get(class) {
            return *known;
        }
        let found = (self.cx.project).constructor(class, |base| self.catalog_stores(base));
        self.constructors.borrow_mut().insert(class.into(), found);
        found
    }

    /// Whether the catalog says storing an attribute on an instance of the library class `base`
    /// stores it in the instance (R3, `pydantic.BaseModel`).
    fn catalog_stores(&self, base: &str) -> bool {
        (self.cx.kb.lookup(&format!("{base}.__setattr__"))).is_some_and(|e| e.stores_on_instance)
    }

    /// The holder of an attribute store of `attr` into the object `obj` holds, when that store
    /// may link a new object into another one ([`Escape::Into`]): `obj` may be new and is an
    /// instance of a project class whose write of `attr` stores the value itself and runs no
    /// code (`ProjectDecls::attr_write_stores_value`).
    fn link_holder(&self, obj: VarId, attr: &str) -> Option<VarId> {
        let Some(Val::Inst(class, _)) = self.types.get(&obj) else {
            return None;
        };
        (self.may_be_new(obj)
            && (self.cx.project)
                .attr_write_stores_value(class, &[attr], |base| self.catalog_stores(base)))
        .then_some(obj)
    }

    /// Record, when `var` may hold a container the constructor of a new object made for it (what
    /// a read of an attribute of an object that may be new gives, which `Recorder::resolve`
    /// decides), the read for the store `how` ([`Escape::Added`]) into it at the current
    /// instruction names; `how` otherwise.
    fn added_to(&mut self, var: VarId, how: (ContainerKind, AddedBy)) -> Option<Escape> {
        let shape = self.shape;
        let read = shape.copy_root(var);
        let Some(InstKind::AttrLoad { obj, attr, .. }) = shape.inst(read).map(|i| &i.kind) else {
            return None;
        };
        if NAMESPACE_ATTRS.contains(&attr.as_str()) || !self.may_be_new(*obj) {
            return None;
        }
        self.rec.container_read(read, *obj, attr);
        self.added.insert(self.at(), how.1);
        Some(Escape::Added(var, how.0))
    }

    /// Record a write of the attribute `attr` (`None`: of any, also its namespace or class) of
    /// the object `obj` holds, which may rebind a container its constructor made.
    fn attr_written(&mut self, obj: VarId, attr: Option<&str>) {
        if self.track && self.may_hold_or_read_new(obj) {
            let attr = attr.filter(|a| !NAMESPACE_ATTRS.contains(a));
            self.rec.write(self.at(), obj, attr);
        }
    }

    /// Whether a query about `var` can be answered with anything but "no".
    fn trackable(&self, var: VarId) -> bool {
        matches!(self.param_alias.get(&var), Some((_, None))) || self.may_be_new(var)
    }

    /// The operands an instruction other than a call stores, yields, or captures.
    fn store_escapes(&mut self, kind: &InstKind) {
        if !self.ownership {
            return;
        }
        match kind {
            InstKind::AttrStore { obj, attr, value } => {
                let how = self
                    .link_holder(*obj, attr)
                    .map_or(Escape::Definite, Escape::Into);
                self.escape(*value, how);
                self.attr_written(*obj, Some(attr));
            }
            InstKind::AttrDelete { obj, attr } => self.attr_written(*obj, Some(attr)),
            InstKind::AttrLoad { obj, attr, .. } if NAMESPACE_ATTRS.contains(&attr.as_str()) => {
                self.attr_written(*obj, None);
            }
            InstKind::StoreGlobal { value, .. }
            | InstKind::StoreDeref { value, .. }
            | InstKind::YieldFrom { value, .. }
            | InstKind::Await { value, .. }
            | InstKind::EnterWith { mgr: value, .. }
            | InstKind::Yield {
                value: Some(value), ..
            } => self.escape(*value, Escape::Definite),
            // A slice store iterates the value, which may run code of the project that keeps it.
            InstKind::SubscriptStore {
                obj,
                index,
                value,
                is_slice,
                ..
            } => {
                let kind = (self.types.get(obj).map(Val::instance_classes))
                    .filter(|_| !*is_slice)
                    .and_then(|classes| classes.first().filter(|_| classes.len() == 1))
                    .and_then(|class| ContainerKind::of_class(class))
                    .filter(|k| matches!(k, ContainerKind::List | ContainerKind::Dict));
                let item = AddedBy::Item {
                    obj: *obj,
                    value: *value,
                };
                let how = kind.and_then(|kind| self.added_to(*obj, (kind, item)));
                let how = how.unwrap_or(Escape::Definite);
                self.escape(*index, how.clone());
                self.escape(*value, how);
            }
            // The targets an unpacking binds hold the elements; the display is never seen.
            InstKind::AllocBuiltin { dst, .. } if self.unpacked_displays.contains(dst) => {}
            InstKind::MakeClosure { captured, .. }
            | InstKind::AllocBuiltin {
                elems: captured, ..
            } => {
                for v in captured {
                    self.escape(*v, Escape::Definite);
                }
            }
            InstKind::MakeGenExpr {
                captured,
                outer_iter,
                ..
            } => {
                for v in captured.iter().chain([outer_iter]) {
                    self.escape(*v, Escape::Definite);
                }
            }
            InstKind::DictLiteral { .. } => {
                for v in kind.operands() {
                    self.escape(v, Escape::Definite);
                }
            }
            _ => {}
        }
    }

    fn visit(&mut self, bb: BlockId, index: usize, kind: &'a InstKind, span: Span) {
        // (The subscript instructions bind `index` to their key.)
        let index_in_block = index;
        self.store_escapes(kind);
        if self.opt_in.locks {
            self.lock_acquired((bb, index), kind, span);
        }
        if self.opt_in.locks || self.opt_in.exclusive {
            self.name_rebound(kind);
        }
        if self.opt_in.exclusive {
            self.exclusive_use(bb, index, kind, span);
        }
        match kind {
            InstKind::AttrLoad { dst, obj, attr } if attr != "__dict__" => {
                if self.self_vars.contains(obj) {
                    self.record_self_read(*dst, attr);
                }
                let owner = self.owner_of(*obj);
                self.typed_read(owner, attr, ReadKind::DirectAttr, span);
                // A returned attribute that `readonly-returns` names (`Class.attr`).
                if let Some(step) = self.returned.get(dst).copied()
                    && let Some(Val::Inst(class, _)) = self.val(*obj)
                    && let Some(c) = self.contracts
                {
                    let class = ContractClass::with_qualname(self.cx.project, class.clone());
                    let (fqn, qualname) = class.member(attr);
                    if let Some(produced) = c.readonly_producer(&fqn, &qualname) {
                        self.seed_protected(fqn, produced, step, span);
                    }
                }
                self.property_read(bb, index, Some(*dst), *obj, attr, span);
            }
            InstKind::Compare {
                op: CompareOpKind::Is | CompareOpKind::IsNot,
                lhs,
                rhs,
                ..
            } => {
                self.sentinel_test(*lhs, *rhs, span);
                self.sentinel_test(*rhs, *lhs, span);
            }
            InstKind::SubscriptLoad {
                obj,
                index,
                key_const,
                is_slice,
                ..
            } => {
                // `obj.__dict__["attr"]`, `obj.__dict__[KEY]`.
                if let Some(owner) = self.dict_of.get(obj).cloned()
                    && let Some(k) = self.key_of(key_const.as_deref(), *index)
                {
                    self.typed_read(owner, &k, ReadKind::DictDirect, span);
                }
                // Reading a missing key of a `defaultdict` inserts it.
                if self.opt_in.iterations
                    && !is_slice
                    && !crate::consumption::is_unpacking(self.shape, *index, span)
                    && matches!(self.val(*obj), Some(Val::Inst(c, _)) if c == "collections.defaultdict")
                    && !self.key_present(bb, index_in_block, *obj, *index)
                {
                    self.container_change(*obj, ContainerChange::Resizes, span);
                }
            }
            // `obj.__dict__ |= other` updates the namespace in place, whatever the result is
            // stored to.
            InstKind::BinOp {
                op: BinOpKind::BitOr,
                lhs,
                in_place: true,
                ..
            } if self.dict_of.contains_key(lhs) => {
                let method = BinOpKind::BitOr.in_place_method();
                self.container_method(*lhs, method, &[], false, span);
            }
            InstKind::BinOp {
                dst,
                op,
                lhs,
                in_place: true,
                ..
            } if self.has_in_place_operator(*lhs, *op) => {
                if self.changes_in_place(*dst, *lhs, *op) {
                    let key = (None, EntryKey::Written);
                    self.container_mut(*lhs, op.in_place_method(), false, None, key, span);
                }
                // `self.items |= other` changes the container before storing it back.
                self.container_change(*lhs, ContainerChange::Resizes, span);
            }
            // A value stored in a module global or a closure variable outlives the call.
            InstKind::StoreGlobal { name, value } => {
                self.note_escape(*value);
                self.bind_global(name, span);
            }
            InstKind::StoreDeref { name, value } => {
                self.note_escape(*value);
                self.change_cell(name, true, span);
            }
            InstKind::Delete {
                name, global: true, ..
            } => self.bind_global(name, span),
            InstKind::Delete {
                name,
                nonlocal: true,
                ..
            } => self.change_cell(name, true, span),
            InstKind::AttrStore { obj, attr, value } => {
                self.write_attr(*obj, attr, WriteKind::DirectAttr, span);
                self.note_escape(*value);
            }
            InstKind::AttrDelete { obj, attr } => {
                if self.self_vars.contains(obj) {
                    self.summary.invalidates_cached.insert(attr.clone());
                }
                self.write_attr(*obj, attr, WriteKind::AttrDelete, span);
            }
            InstKind::SubscriptStore {
                obj,
                index,
                key_const,
                value,
                ..
            } => {
                self.subscript_write(*obj, key_const.as_deref(), *index, false, span);
                // A store under a key the container holds changes no iterator's step.
                if !self.key_present(bb, index_in_block, *obj, *index) {
                    self.container_change(*obj, ContainerChange::StoresItems, span);
                }
                self.note_escape(*value);
            }
            InstKind::SubscriptDelete {
                obj,
                index,
                key_const,
            } => {
                self.subscript_write(*obj, key_const.as_deref(), *index, true, span);
                self.container_change(*obj, ContainerChange::Resizes, span);
            }
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                kwargs,
                is_awaited,
                star_from,
            } => {
                if crate::callgraph::is_private_name(method) {
                    self.summary.private_calls.insert(method.clone());
                }
                let callee = self.method_callee(*recv, method);
                self.unresolved_access(method, Access::Call, *recv, span);
                self.deep_copy(&callee, args, kwargs, *star_from, span);
                // `obj.prop(...)` reads the property and calls the value read, which binds no
                // receiver and changes no container `obj` is.
                let calls_property = self.cx.calls_property(self.val(*recv), method);
                if calls_property {
                    self.property_read(bb, index, None, *recv, method, span);
                }
                // A container method changes its receiver in place, unless the receiver's
                // class defines the method itself: then what that method does is in its
                // summary (`Stats.clear()` may change nothing).
                let changes_recv = !calls_property && !matches!(callee, Callee::Project(..));
                if container_mutator(method) && changes_recv {
                    let plain = kwargs.is_empty() && star_from.is_none();
                    self.container_method(*recv, method, args, plain, span);
                    for a in args {
                        if matches!(method.as_str(), "append" | "insert" | "add" | "setdefault") {
                            self.note_escape(*a);
                        }
                    }
                }
                if changes_recv && let Some(change) = iterations::method_change(method) {
                    self.container_change(*recv, change, span);
                }
                // `obj.__dict__.get("attr"[, default])`.
                if method == "get"
                    && let Some(owner) = self.dict_of.get(recv).cloned()
                    && let Some(k) = args.first().and_then(|a| self.resolved_str(*a))
                {
                    self.typed_read(owner, &k, ReadKind::DictDirect, span);
                }
                let (recv_origin, recv_is_self) = match calls_property {
                    true => (None, false),
                    false => {
                        let (origin, is_self) = self.receiver(*recv, bb);
                        (Some(origin), is_self)
                    }
                };
                let constructs = self.construction(kind);
                let call = Call {
                    dst: Some(*dst),
                    value: None,
                    recv_var: recv_origin.as_ref().map(|_| *recv),
                    recv: recv_origin,
                    args,
                    kwargs,
                    star_from: *star_from,
                    args_at: Some(0),
                    is_awaited: *is_awaited,
                    recv_is_self,
                    constructs,
                    per_item: None,
                    unseen_keywords: matches!(callee, Callee::Project(..))
                        && self.cx.calls_partial_with_keywords(self.val(*recv), method),
                    span,
                };
                self.call_inst((bb, index), method, callee, &call);
            }
            InstKind::Call {
                dst,
                callee_var,
                callee,
                args,
                kwargs,
                is_awaited,
                star_from,
            } => {
                // A non-inlined comprehension iterates its first argument.
                if callee_var.is_none()
                    && let Some(id) = callee
                        .strip_prefix("__comp_")
                        .and_then(|n| n.parse::<u32>().ok())
                    && let Some(iterable) = args.first()
                {
                    self.child_iteration(bb, FunctionId(id), *iterable, span);
                }
                // `make = functools.partial(f, ...); make(...)` calls `f` here.
                let passed = Passed {
                    args,
                    kwargs,
                    star_from: *star_from,
                    recv: None,
                };
                if let Some(v) = callee_var.filter(|v| self.val(*v).is_none())
                    && let Some(partial) = self
                        .partial(v)
                        .and_then(|p| p.called_with(passed.args, passed.kwargs, passed.star_from))
                {
                    let (target, recv_var) = self.callable_callee(partial.callable);
                    let complete = self.callable_targets_complete(partial.callable, &target);
                    self.held_calls_complete.insert((bb, index), complete);
                    let (recv, recv_is_self) = match recv_var {
                        Some(r) => {
                            let (origin, is_self) = self.receiver(r, bb);
                            (Some(origin), is_self)
                        }
                        None => (None, false),
                    };
                    if self.references {
                        // The callable the object holds is called here: its use there is a
                        // call, not a reference.
                        self.handed.extend(partial.calls.iter().copied());
                    }
                    let call = Call {
                        dst: Some(*dst),
                        value: Some(partial.callable),
                        recv,
                        recv_var,
                        args: &partial.args,
                        kwargs: &partial.kwargs,
                        star_from: partial.star_from,
                        args_at: None,
                        is_awaited: *is_awaited,
                        recv_is_self,
                        constructs: self.class_called(partial.callable),
                        per_item: None,
                        unseen_keywords: self.calls_unseen_keywords(partial.callable),
                        span,
                    };
                    self.call_inst((bb, index), callee, target, &call);
                    return;
                }
                let callee_val = callee_var.and_then(|v| self.val(v));
                // `m = obj.method; m(...)` and `getattr(obj, "method")(...)` call a method of
                // the receiver, as `obj.method(...)` does.
                if callee_val.is_none()
                    && let Some(held) = *callee_var
                    && let Some((recv, method)) = self.bound_method(held)
                {
                    let target = self.method_callee(recv, &method);
                    let complete = self.callable_targets_complete(held, &target);
                    self.held_calls_complete.insert((bb, index), complete);
                    self.unresolved_access(&method, Access::Call, recv, span);
                    let (recv_origin, recv_is_self) = self.receiver(recv, bb);
                    let call = Call {
                        dst: Some(*dst),
                        value: *callee_var,
                        recv: Some(recv_origin),
                        recv_var: Some(recv),
                        args,
                        kwargs,
                        star_from: *star_from,
                        args_at: Some(0),
                        is_awaited: *is_awaited,
                        recv_is_self,
                        constructs: None,
                        per_item: None,
                        unseen_keywords: self.calls_unseen_keywords(held),
                        span,
                    };
                    self.call_inst((bb, index), &method, target, &call);
                    return;
                }
                // A partial object read here (`TABLE[kind](x)`) calls its callable.
                let partial = match (callee_val, callee_var) {
                    (None, Some(v)) => self.typed_partial(*v),
                    _ => None,
                };
                let callable = callee_val.or(partial.as_ref().map(|(c, _)| c));
                let target = match callee_var {
                    Some(_) => match self.cx.resolve_call(callable) {
                        // The keywords the object holds may decide what a library call does,
                        // which the catalog reads from the arguments the call shows.
                        Callee::External(_)
                            if partial
                                .as_ref()
                                .is_some_and(|(_, held)| *held == Held::Keywords) =>
                        {
                            Callee::Unresolved
                        }
                        target => target,
                    },
                    // Non-inlined comprehension (Python 3.11, `__comp_<function id>`) runs
                    // immediately.
                    None => callee
                        .strip_prefix("__comp_")
                        .and_then(|n| n.parse::<u32>().ok())
                        .map_or(Callee::Unresolved, |id| self.child_callee(FunctionId(id))),
                };
                if partial.is_some() {
                    let complete = self.cx.call_targets_complete(callable, &target);
                    self.held_calls_complete.insert((bb, index), complete);
                }
                self.deep_copy(&target, args, kwargs, *star_from, span);
                let constructs = self.construction(kind);
                let call = Call {
                    dst: Some(*dst),
                    value: *callee_var,
                    recv: None,
                    recv_var: None,
                    args,
                    kwargs,
                    star_from: *star_from,
                    args_at: Some(0),
                    is_awaited: *is_awaited,
                    recv_is_self: false,
                    constructs,
                    per_item: None,
                    unseen_keywords: partial.is_some_and(|(_, held)| held == Held::Keywords),
                    span,
                };
                self.call_inst((bb, index), callee, target, &call);
            }
            // A generator expression's body runs where it is consumed — almost always
            // immediately in the creating frame (`sum(f(x) for x in xs)`).
            InstKind::MakeGenExpr {
                dst,
                func_id: Some(id),
                outer_iter,
                ..
            } => {
                // Creating it iterates the outermost iterable at once.
                self.child_iteration(bb, *id, *outer_iter, span);
                let target = self.child_callee(*id);
                let call = Call {
                    dst: Some(*dst),
                    value: None,
                    recv: None,
                    recv_var: None,
                    args: &[],
                    kwargs: &[],
                    star_from: None,
                    args_at: Some(0),
                    is_awaited: false,
                    recv_is_self: false,
                    constructs: None,
                    per_item: None,
                    unseen_keywords: false,
                    span,
                };
                self.call(bb, &target, &call);
            }
            // A comprehension scope's `.0` is the iterator its creator built, where the
            // iteration starts (`child_iteration`).
            InstKind::MakeIter {
                iterable, is_async, ..
            } if self.f.var_name(*iterable) != Some(".0") => {
                self.protocol_call(bb, *iterable, iteration_protocol(*is_async), false, span)
            }
            InstKind::EnterWith {
                mgr,
                is_async: false,
                ..
            } => self.protocol_call(bb, *mgr, "__enter__", false, span),
            // Entering an async context manager awaits its `__aenter__` (a lock's acquisition).
            InstKind::EnterWith {
                mgr,
                is_async: true,
                ..
            } if self.opt_in.checkpoints => {
                self.protocol_call(bb, *mgr, "__aenter__", true, span);
            }
            _ => {}
        }
    }

    /// A non-reentrant lock `kind` takes (`LockFacts::acquires`), with the guards on the
    /// function's inputs (the tests of state on every path to it are added once every
    /// acquisition is known, [`Self::lock_owner_tests`]).
    fn lock_acquired(&mut self, (bb, index): (BlockId, usize), kind: &InstKind, span: Span) {
        // A branch on a constant never takes the lock its dead side would.
        if !matches!(
            kind,
            InstKind::EnterWith { .. } | InstKind::MethodCall { .. }
        ) || !self.live.block(bb)
        {
            return;
        }
        let def = |v: VarId| self.defs.get(&v).copied();
        let class_of = |v: VarId| locks::lock_class(self.cx, self.f, &self.types, &def, v);
        let Some(LockStep::Acquire(var)) = locks::lock_step(self.cx, kind, &def, &class_of) else {
            return;
        };
        let (Some(class), Some(lock)) = (
            class_of(var),
            locks::obj_ref(self.cx, self.f, self.key, &self.types, &def, var),
        ) else {
            return;
        };
        let guards = self.guards.occurrence(&self.guard_inputs(), bb);
        self.lock_blocks.entry(lock.clone()).or_default().insert(bb);
        let every = self.takes_on_every_return.contains(&(bb, index));
        let acq = locks::own_acquisition(self.key, self.path, (class, span), guards, every);
        locks::add_acquisition(&mut self.summary.lock_facts_mut().acquires, lock, acq);
    }

    /// The locks the function gives back where its own acquisitions do not hold them
    /// (`LockFacts::gives_back`), which it then takes on no return.
    fn lock_gives_back(&mut self) {
        if !locks::calls_release(self.f) {
            return;
        }
        let def = |v| self.defs.get(&v).copied();
        let steps = locks::lock_steps(
            self.cx,
            self.f,
            self.key,
            &self.types,
            &def,
            &self.live,
            &|_| Vec::new(),
        );
        let given = locks::unheld_releases(self.shape, &steps);
        if given.is_empty() {
            return;
        }
        let facts = self.summary.lock_facts_mut();
        for lock in given {
            if let Some(acq) = facts.acquires.get_mut(&lock) {
                acq.on_every_return = false;
            }
            facts.gives_back.insert(lock);
        }
    }

    /// A module global or a closure variable `kind` binds or deletes (`LockFacts::rebinds`;
    /// the attributes of parameters are in `mutated_param_paths`).
    fn name_rebound(&mut self, kind: &InstKind) {
        if self.f.is_module_body {
            return;
        }
        let def = |v: VarId| self.defs.get(&v).copied();
        let name = locks::rebound_name(self.cx, self.f, self.key, &self.types, &def, kind);
        if let Some(name @ (ObjRef::Global(_) | ObjRef::Cell { .. })) = name {
            self.summary.lock_facts_mut().rebinds.insert(name);
        }
    }

    /// The tests of state on every path to each block a run reaches, over the edges it can
    /// take.
    fn state_tests(&self) -> &locks::BlockTests {
        self.state_tests.get_or_init(|| {
            let dom = heki_ir::DominatorTree::over(self.shape, |p, b| self.live.edge(p, b));
            let def = |v: VarId| self.defs.get(&v).copied();
            let resolve = |recv: VarId, method: &str| match self.method_callee(recv, method) {
                Callee::Project(keys, _) => keys,
                _ => Vec::new(),
            };
            let global = |name: &str| self.cx.global_fqn(name);
            locks::BlockTests::of(self.shape, dom, &def, &resolve, &global)
        })
    }

    /// An object `kind` uses one operation at a time (`EffectSummary::exclusive_uses`), unless
    /// the function holds a lock there.
    fn exclusive_use(&mut self, bb: BlockId, index: usize, kind: &InstKind, span: Span) {
        if !matches!(
            kind,
            InstKind::MethodCall { .. } | InstKind::MakeIter { is_async: true, .. }
        ) || !self.live.block(bb)
        {
            return;
        }
        let class_of = |v: VarId| one_at_a_time::instance_class(&self.types, v);
        let Some((obj, method, one)) = one_at_a_time::use_step(self.cx, kind, &class_of) else {
            return;
        };
        let def = |v: VarId| self.defs.get(&v).copied();
        let Some(obj) = locks::obj_ref(self.cx, self.f, self.key, &self.types, &def, obj) else {
            return;
        };
        if self.lock_held_at((bb, index)) {
            return;
        }
        let u = one_at_a_time::own_use(self.key, self.path, method, one, span);
        one_at_a_time::add_use(self.summary.exclusive_uses_mut(), obj, u);
    }

    /// The tests of state on every path to every acquisition of each lock
    /// (`LockAcquire::tests`).
    fn lock_owner_tests(&mut self) {
        if self.lock_blocks.is_empty() {
            return;
        }
        let blocks = std::mem::take(&mut self.lock_blocks);
        let tests = self.state_tests();
        let found: Vec<_> = blocks
            .into_iter()
            .map(|(lock, blocks)| {
                let t = tests
                    .common(&blocks)
                    .expect("a lock with acquiring blocks a run reaches");
                let owner = locks::owner_tests(&lock, t);
                (lock, owner)
            })
            .collect();
        let acquires = &mut self.summary.lock_facts_mut().acquires;
        for (lock, tests) in found {
            let acq = acquires
                .get_mut(&lock)
                .expect("a lock with acquiring blocks was taken");
            acq.tests = tests;
        }
    }

    /// The comprehension or generator expression `child` created here iterates `iterable`
    /// (its outermost one) at once, as its first clause does (`__iter__`, or `__aiter__` for an
    /// `async for`): [`Self::protocol_call`]. A later `async for` clause does not change how the
    /// first one starts.
    fn child_iteration(&mut self, bb: BlockId, child: FunctionId, iterable: VarId, span: Span) {
        let first_is_async = self
            .cx
            .ir()
            .function(child)
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .find_map(|i| match i.kind {
                InstKind::MakeIter { is_async, .. } => Some(is_async),
                _ => None,
            });
        if let Some(is_async) = first_is_async {
            self.protocol_call(bb, iterable, iteration_protocol(is_async), false, span);
        }
    }

    /// The special method `protocol` of the class of `value`, run by the statement at `span`
    /// (`__iter__`, `__aiter__` when a `for` loop, a comprehension, or a consuming call such as
    /// `list(...)` iterates it; `__enter__` when a `with` enters it): the catalog's entry for
    /// it is a call at `span` (`for row in session.query(User):` runs the query,
    /// `with fsspec.open(path):` opens the file; `__aenter__`, awaited, when an `async with`
    /// enters it). Project classes' special methods are not followed.
    fn protocol_call(
        &mut self,
        bb: BlockId,
        value: VarId,
        protocol: &str,
        is_awaited: bool,
        span: Span,
    ) {
        let Some(Val::Inst(class, _)) = self.val(value) else {
            return;
        };
        if class.starts_with("builtins.") || self.cx.project.is_project_class(class) {
            return;
        }
        let symbol = format!("{class}.{protocol}");
        if self.cx.catalog(&symbol).is_some() {
            self.external_call(bb, symbol, Passed::NOTHING, is_awaited, span);
        }
    }

    /// The origin of a receiver bound to a method's first parameter, and whether it is the
    /// calling method's own `self`: `super()` binds the calling method's `self` (parameter 0).
    /// The receiver binds the parameter exactly (`ArgOrigin::binding`) at a call in `bb`.
    fn receiver(&self, recv: VarId, bb: BlockId) -> (ArgOrigin, bool) {
        let mut origin = self.origin(recv);
        let mut binding = self.binding(recv, self.loops_around(bb));
        let is_super = matches!(self.val(recv), Some(Val::Super(_)));
        // `super()` is the method's own `self`, found along the MRO past its class.
        if is_super {
            origin.param = Some((0, None));
            origin.may_params = 1;
            origin.reach = origin.reach.join(Reach {
                params: 1,
                globals: false,
            });
            binding.input = Some(GuardSubject {
                param: 0,
                attr: None,
            });
            binding.derived_params = 1;
        }
        origin.binding = Some(binding);
        (origin, is_super || self.self_vars.contains(&recv))
    }

    /// Reading `obj.attr` runs the property getters it resolves to: a project call binding the
    /// receiver, at the read, awaited when the read value is awaited directly (an `async`
    /// getter returns a coroutine). The read's own facts (`reads_self_attrs`, typed read
    /// sites) stay as they are.
    /// A read of `obj.attr` at `(bb, index)` that runs property getters: a call of them, whose
    /// result (`dst`) is a project call's result like any other (a generator a getter returns
    /// runs where the caller iterates it). Without `dst`, the read is the one a call of the
    /// property's value makes (`obj.prop(...)`), and the instruction's result is that call's.
    fn property_read(
        &mut self,
        bb: BlockId,
        index: usize,
        dst: Option<VarId>,
        obj: VarId,
        attr: &str,
        span: Span,
    ) {
        let Some(callee) = self.cx.resolve_attr_load(self.val(obj), attr) else {
            return;
        };
        let (recv, recv_is_self) = self.receiver(obj, bb);
        let is_awaited = dst.is_some_and(|dst| {
            self.uses.get(&dst).is_some_and(|uses| {
                uses.iter()
                    .any(|u| matches!(u, InstKind::Await { value, .. } if *value == dst))
            })
        });
        let call = Call {
            dst,
            value: None,
            recv: Some(recv),
            recv_var: Some(obj),
            args: &[],
            kwargs: &[],
            star_from: None,
            args_at: Some(0),
            is_awaited,
            recv_is_self,
            constructs: None,
            per_item: None,
            unseen_keywords: false,
            span,
        };
        match dst {
            Some(_) => self.call_inst((bb, index), attr, callee, &call),
            None => {
                self.call(bb, &callee, &call);
            }
        }
    }

    /// The receiver and method name of the bound method behind `var` (through copies): an
    /// attribute load that runs no property getter (`m = obj.method`), or `getattr(obj, "method"[,
    /// default])` with a constant name.
    fn bound_method(&self, var: VarId) -> Option<(VarId, String)> {
        let kind = &self.defs.get(&self.shape.copy_root(var))?.kind;
        let (_, obj, attr) = self.attr_load(kind)?;
        match kind {
            InstKind::AttrLoad { .. } => {
                let getter = self.cx.resolve_attr_load(self.val(obj), &attr).is_some();
                (!getter).then_some((obj, attr))
            }
            _ => Some((obj, attr)),
        }
    }

    /// What calling `var` calls, and the receiver a bound method binds (`obj.method`, a
    /// method alias, `getattr(obj, "method")`).
    fn callable_callee(&self, var: VarId) -> (Callee, Option<VarId>) {
        match self.bound_method(var) {
            Some((recv, method)) => (self.method_callee(recv, &method), Some(recv)),
            None => (self.cx.resolve_call(self.val(var)), None),
        }
    }

    /// Whether `callee`, what [`Self::callable_callee`] resolves calling `var` to, is every
    /// implementation that call can run ([`crate::TypeCtx::method_targets_complete`] for a
    /// bound method that `getattr` gives with no default, [`crate::TypeCtx::call_targets_complete`]
    /// otherwise).
    fn callable_targets_complete(&self, var: VarId, callee: &Callee) -> bool {
        match self.bound_method(var) {
            Some((recv, method)) => {
                // `getattr(obj, "method", default)` gives the default where the object lacks
                // the method.
                let root = self.defs.get(&self.shape.copy_root(var)).map(|i| &i.kind);
                let default = matches!(root, Some(InstKind::Call { args, .. }) if args.len() == 3);
                !default
                    && self
                        .cx
                        .method_targets_complete(self.val(recv), &method, callee)
            }
            None => self.cx.call_targets_complete(self.val(var), callee),
        }
    }

    fn child_callee(&self, id: FunctionId) -> Callee {
        let child = self.cx.ir().function(id);
        Callee::Project(
            vec![FuncKey {
                module: self.key.module.clone(),
                qualname: func_qualname(child).to_owned(),
            }],
            false,
        )
    }

    fn record_self_read(&mut self, dst: VarId, attr: &str) {
        let uses = self.uses.get(&dst).map(Vec::as_slice).unwrap_or(&[]);
        let keys: Vec<Option<String>> = uses
            .iter()
            .map(|u| match u {
                InstKind::SubscriptLoad {
                    obj,
                    key_const: Some(k),
                    ..
                } if *obj == dst => Some(k.clone()),
                _ => None,
            })
            .collect();
        if !keys.is_empty() && keys.iter().all(Option::is_some) {
            for k in keys {
                self.summary.reads_self_attrs.insert(AttrKeyPath {
                    attr: attr.to_owned(),
                    const_key: k,
                });
            }
        } else {
            self.summary.reads_self_attrs.insert(AttrKeyPath {
                attr: attr.to_owned(),
                const_key: None,
            });
        }
    }

    fn note_escape(&mut self, value: VarId) {
        if let Some((i, None)) = self.param_alias.get(&value) {
            self.summary.escapes_params |= 1u64 << i;
        }
    }

    /// A returned result of the quadratic reallocation `symbol` taking a parameter, as itself or
    /// in a display (`np.append(acc, x)`, `pd.concat([df, part])`): `reallocates_params`.
    fn note_returned_realloc(&mut self, symbol: &str, call: &Call<'_>) {
        let reallocates = self
            .cx
            .kb
            .lookup(symbol)
            .is_some_and(|e| e.effects.contains(&ExecEffectKind::QuadraticRealloc));
        if !reallocates || !call.dst.is_some_and(|d| self.returned.contains_key(&d)) {
            return;
        }
        for &arg in call.args {
            let held: Vec<VarId> = match self.defs.get(&arg).map(|i| &i.kind) {
                Some(InstKind::AllocBuiltin { elems, .. }) => elems.clone(),
                _ => vec![arg],
            };
            for v in held {
                if let Some((i, None)) = self.param_alias.get(&v) {
                    self.summary.reallocates_params |= 1u64 << i;
                }
            }
        }
    }

    /// A change below the attribute `attr` of parameter `i` (of the object `p.attr` holds).
    fn change_below_param_attr(&mut self, i: u8, attr: String, span: Span) {
        self.summary
            .changed_param_attrs
            .entry(i)
            .or_default()
            .insert(attr.clone());
        self.mutate_param(i, attr, span);
    }

    /// A change of the object `var` holds, when it may be one a parameter holds as an item or
    /// an element, or one below it ([`Self::element_paths`]); `of_entry`: a change of an entry
    /// as part of the container too, when it is one ([`Self::entry_params`]).
    fn change_param_element(&mut self, var: Option<VarId>, of_entry: bool, span: Span) {
        let Some(var) = var else { return };
        let paths = self.element_paths(var);
        if of_entry {
            self.summary.entry_change_params |= self.entry_params(var, &paths);
        }
        for (i, path) in paths {
            self.change_below_param_attr(i, path, span);
        }
    }

    /// The function's own change, at `span`, of parameter `i` at `path`.
    fn mutate_param(&mut self, i: u8, path: String, span: Span) {
        let memo = self.memo_fills.contains(&self.pos);
        self.summary.note_param_change(i, path, memo);
        self.summary
            .param_change_sites
            .entry(i)
            .or_default()
            .insert(span);
    }

    /// Record a typed write of `attr` on the object `obj`; its construction is answered once the
    /// function is visited.
    fn typed_site(
        &mut self,
        class: String,
        attr: &str,
        key: (Option<String>, EntryKey),
        kind: WriteKind,
        span: Span,
        obj: Written,
    ) {
        let (key, key_from) = key;
        // `MUT208`: a write to a contract target of an object that existed before the call,
        // unless it is the class's own initialization of `self`.
        let reach = obj.reach;
        if let (Some(c), Some(events)) = (self.contracts, self.events.as_mut())
            && reach.any()
        {
            let initializer = invalidation::is_initializer(self.key);
            let own_init = invalidation::builds_written_object(initializer, reach, obj.params);
            // The class and its project bases: a target class covers its subclasses.
            let mut mask = c.target_of(&ContractClass::lineage(c, self.cx.project, &class), attr);
            if mask != 0 {
                let key = self.key;
                let overriding =
                    (self.summary.override_roles.as_deref()).map_or(0, |roles| roles.exempt_writer);
                mask &= !*self
                    .exempt
                    .get_or_insert_with(|| c.exempt_writer(&key.fqn(), &key.qualname) | overriding);
            }
            if mask != 0 && !own_init {
                events.writes.push((
                    self.pos.0,
                    self.pos.1,
                    FlowWrite {
                        contracts: mask,
                        class_qualname: class.clone(),
                        attr: attr.to_owned(),
                        kind,
                        span,
                        reach,
                        obj_params: obj.params,
                        flow: SiteFlow::default(),
                    },
                ));
            }
        }
        let through_protocol = self.contracts.is_some_and(|c| c.write_protocols())
            && obj.holder.and_then(|v| self.val(v)).is_some_and(
                |v| matches!(v, Val::Inst(p, _) if self.cx.project.protocol_declares(p, attr)),
            );
        let site = TypedFieldWriteSite {
            class_qualname: class,
            attr: attr.to_owned(),
            kind,
            span,
            via_chain: Vec::new(),
            obj_params: obj.params,
            fresh: obj.fresh,
            key,
            construction: Construction::No,
            contexts: 0,
            through_protocol,
            key_from,
            passed: false,
        };
        // A site waiting for its answer joins the summary once the function is visited.
        if let Some(holder) = obj.holder.filter(|h| self.track && self.trackable(*h)) {
            let query = self.rec.query(self.at(), holder);
            self.pending_sites.push(PendingSite {
                site,
                query,
                rebinds: kind.rebinds(),
            });
            return;
        }
        self.summary.typed_field_write_sites.insert(site);
    }

    /// Read of attribute `attr` on the object `owner`, when it has a project class or is the
    /// class object of one (`Model.KIND`, `cls.KIND`, `kind.KIND` of a `kind: type[Model]`).
    fn typed_read(&mut self, owner: Owner, attr: &str, kind: ReadKind, span: Span) {
        if let Some(var) = owner.var {
            self.unresolved_access(attr, Access::Read(kind), var, span);
        }
        let val = owner.var.and_then(|v| self.val(v));
        // A read through a protocol declaring the attribute is one of every class implementing
        // it; the read contracts judge it by the protocol, the type the code reads it through.
        let protocol = match val {
            Some(Val::Inst(p, _)) if self.cx.project.protocol_declares(p, attr) => Some(p.clone()),
            _ => None,
        };
        let class_object = val
            .and_then(|v| v.class_object(self.cx.project))
            .map(str::to_owned);
        let classes = owner.typed_classes.into_iter().map(|c| (c, false));
        for (class, class_object) in classes.chain(class_object.map(|c| (c, true))) {
            let through_protocol = protocol.as_ref().is_some_and(|p| *p != class);
            self.summary
                .typed_field_read_sites
                .insert(TypedFieldReadSite {
                    class_qualname: class,
                    attr: attr.to_owned(),
                    kind,
                    obj_params: owner.may_params,
                    span,
                    through_protocol,
                    class_object,
                });
        }
    }

    /// Write of attribute `attr` on the object `owner` (AttrStore / AttrDelete / setattr / __dict__).
    fn write_owner_attr(&mut self, owner: Owner, attr: &str, kind: WriteKind, span: Span) {
        if let Some(var) = owner.var {
            self.change_object(var, span);
        }
        let written = owner.written();
        if owner.is_self {
            self.summary.mutates_self_attrs.insert(AttrKeyPath {
                attr: attr.to_owned(),
                const_key: None,
            });
        }
        match owner.param {
            Some((i, None)) => self.mutate_param(i, attr.to_owned(), span),
            Some((i, Some(path))) => self.change_below_param_attr(i, path, span),
            // An attribute of an entry of a project class is that class's write
            // (`typed_site`), not one of the container holding the entry.
            None => {
                let untied = owner.typed_classes.is_empty();
                self.change_param_element(owner.var, untied, span);
            }
        }
        for class in owner.typed_classes {
            self.typed_site(class, attr, (None, EntryKey::Written), kind, span, written);
        }
        if owner.unknown_class {
            self.unresolved(Unresolved::Receiver(attr.to_owned()), owner.var, kind, span);
        }
    }

    /// Whether heki knows nothing of the class of `var`'s object here: no inferred value, an
    /// annotation that says nothing (`Any`, `object`) that no `isinstance` test narrows, or a
    /// project protocol whose implementations heki cannot list.
    fn unknown_class(&self, var: VarId) -> bool {
        match self.val(var) {
            None => true,
            Some(Val::Inst(c, _)) => {
                resolver::of_any_class(c) || self.cx.project.protocol(c) == Some(None)
            }
            Some(_) => false,
        }
    }

    /// Record a read or a call of `name` heki cannot tie to a class (`MUT206`, `MUT207`) on the
    /// object `obj` holds, when a strict contract names it: the object counts as a parameter
    /// only when it is one on every path.
    fn unresolved_access(&mut self, name: &str, access: Access, obj: VarId, span: Span) {
        let Some(c) = self.contracts else {
            return;
        };
        let named = match access {
            Access::Read(_) => c.unresolved_read(name),
            Access::Call => c.unresolved_call(name),
        };
        if !named || !self.unknown_class(obj) {
            return;
        }
        let obj_params = self.exact_param(obj);
        let protocol = self.unknown_protocol(obj);
        self.summary
            .unresolved_access_sites
            .insert(UnresolvedAccessSite {
                name: name.to_owned(),
                access,
                span,
                obj_params,
                protocol,
            });
    }

    /// The project protocol `var`'s object is typed with although heki cannot tie it to a class
    /// ([`UnresolvedAccessSite::protocol`]): an instance of a protocol whose implementations
    /// heki cannot list, or a class object whose annotation is `type[P]`
    /// ([`ProjectDecls::protocol_class`]).
    fn unknown_protocol(&self, var: VarId) -> Option<String> {
        match self.val(var) {
            Some(Val::Inst(c, _)) if self.cx.project.protocol(c) == Some(None) => Some(c.clone()),
            Some(_) => None,
            None => self.cx.project.protocol_class(&self.declared_type(var)?),
        }
    }

    /// What the annotation of the place `var`'s value comes from declares, as its module
    /// resolves it: the parameter it is on every path, the local it is bound to, or the
    /// attribute it is read from (along the MRO of the object's class).
    fn declared_type(&self, var: VarId) -> Option<crate::types::Ty> {
        if let Some((i, None)) = self.param_alias.get(&var) {
            return self
                .cx
                .module
                .resolve_annotation_ty(self.f.params.get(usize::from(*i))?.annotation.as_deref()?);
        }
        // The annotated local is any name along the copies the value went through.
        let mut cur = var;
        loop {
            let name = self
                .f
                .var_names
                .get(cur.0 as usize)
                .and_then(Option::as_deref);
            let annotated =
                name.and_then(|name| self.f.local_annotations.iter().find(|a| a.target == name));
            if let Some(local) = annotated {
                let locals = self.local_names.get_or_init(|| self.cx.local_names(self.f));
                return self
                    .cx
                    .module
                    .resolve_annotation_ty_in(&local.annotation, locals);
            }
            match self.shape.inst(cur).map(|i| &i.kind) {
                Some(InstKind::Copy { src, .. }) => cur = *src,
                Some(InstKind::AttrLoad { obj, attr, .. }) => {
                    let class = self.instance_of(*obj)?;
                    return self.cx.project.member_type(&class, attr, false);
                }
                _ => return None,
            }
        }
    }

    /// Record a write heki cannot tie to a class (`MUT205`) of the object `obj` holds (`None`:
    /// an object of an enclosing function, reached from a closure cell): the object counts as a
    /// parameter only when it is one on every path.
    fn unresolved(&mut self, what: Unresolved, obj: Option<VarId>, kind: WriteKind, span: Span) {
        if !self.ownership {
            return;
        }
        let obj_params = obj.map_or(0, |o| self.exact_param(o));
        let protocol = match (&what, obj) {
            (Unresolved::Receiver(_), Some(obj)) => self.unknown_protocol(obj),
            _ => None,
        };
        let site = UnresolvedWriteSite {
            what,
            kind,
            span,
            obj_params,
            construction: Construction::No,
            protocol,
        };
        if let Some(obj) = obj.filter(|o| self.track && self.trackable(*o)) {
            let query = self.rec.query(self.at(), obj);
            self.pending_unresolved.push((site, query));
            return;
        }
        self.summary.unresolved_write_sites.insert(site);
    }

    /// A computed attribute name written on `owner`'s object (`setattr(obj, name, v)`,
    /// `obj.__dict__[name] = v`): unresolved when the object has a project class, or when heki
    /// knows nothing of its class. `adds` tells which of the classes it may add an attribute
    /// to.
    fn computed_name(&mut self, owner: &Owner, adds: Adds<'_>, kind: WriteKind, span: Span) {
        if let Some(var) = owner.var {
            self.change_object(var, span);
        }
        for class in &owner.typed_classes {
            let what = if adds.to(self.cx.project, class) {
                Unresolved::Name(class.clone())
            } else {
                Unresolved::Existing(class.clone())
            };
            self.unresolved(what, owner.var, kind, span);
        }
        if owner.typed_classes.is_empty() && owner.unknown_class {
            self.unresolved(Unresolved::Anything, owner.var, kind, span);
        }
    }

    /// The attribute of an object of unknown class whose container (or an entry of it, `true`)
    /// `var` holds, and the object (`None`: one of an enclosing function, reached from a closure
    /// cell): `obj.attr`, `obj.attr["k"]`, through copies and closure cells.
    fn untyped_container(&self, var: VarId) -> Option<(String, Option<VarId>, bool)> {
        let mut cur = var;
        let mut nested = false;
        for _ in 0..16 {
            match &self.defs.get(&cur)?.kind {
                InstKind::Copy { src, .. } => cur = *src,
                InstKind::LoadDeref { name, .. } => {
                    let cell = self.cx.bound_cell(self.f, name)?;
                    let (attr, entry) = self.cell_aliases.get(cell)?.untyped.clone()?;
                    return Some((attr, None, nested || entry));
                }
                InstKind::SubscriptLoad {
                    obj,
                    is_slice: false,
                    ..
                } => {
                    nested = true;
                    cur = *obj;
                }
                InstKind::AttrLoad { obj, attr, .. }
                    if attr != "__dict__" && self.unknown_class(*obj) =>
                {
                    return Some((attr.clone(), Some(*obj), nested));
                }
                _ => return None,
            }
        }
        None
    }

    fn write_attr(&mut self, obj: VarId, attr: &str, kind: WriteKind, span: Span) {
        let owner = self.owner_of(obj);
        self.write_owner_attr(owner, attr, kind, span);
    }

    /// Whether the augmented assignment `lhs op= ...` (defining `dst`) changes the container
    /// `lhs` holds in place: a built-in container whose class does so for `op`, bound to a name
    /// (a store of the result to an attribute or item is a write of its own).
    fn changes_in_place(&self, dst: VarId, lhs: VarId, op: BinOpKind) -> bool {
        let stored = self.uses.get(&dst).into_iter().flatten().any(|u| {
            matches!(u, InstKind::AttrStore { value, .. } | InstKind::SubscriptStore { value, .. } if *value == dst)
        });
        !stored && self.has_in_place_operator(lhs, op)
    }

    /// Whether `lhs op= ...` changes the built-in container `lhs` holds in place, whatever the
    /// result is stored to.
    fn has_in_place_operator(&self, lhs: VarId, op: BinOpKind) -> bool {
        matches!(self.val(lhs), Some(Val::Inst(class, _)) if crate::types::changes_in_place(class, op))
    }

    /// Whether the checked container of `self` that `obj` holds has the key `key` at instruction
    /// `index` of `bb` (`MUT113`): the instruction is on the side of a test where the container
    /// holds the key (inside `if key in <container>:`, past `if key not in <container>: return`
    /// or `if <container>.get(key) is None: raise KeyError(key)`), or past a read of
    /// `<container>[key]` in its block (not for a `Counter`). The container is `obj`, a copy of
    /// it, or another read of the same `self.<attr>`.
    fn key_present(&self, bb: BlockId, index: usize, obj: VarId, key: VarId) -> bool {
        let Some(attr) = self.checked_self_container(obj) else {
            return false;
        };
        let key = self.key_identity(key);
        let facts = self.key_facts();
        // A read of a missing key raises, or inserts it into a `defaultdict`; a `Counter` returns
        // 0 without inserting it.
        let counter = |c: &String| matches!(c.as_str(), "collections.Counter" | "typing.Counter");
        let may_be_counter = matches!(self.val(obj), Some(Val::Inst(c, _)) if counter(c))
            || matches!(self.val(obj), Some(Val::Union(cs)) if cs.iter().any(counter));
        let read_before = !may_be_counter
            && facts
                .reads
                .get(attr.as_str())
                .and_then(|reads| reads.get(&(bb, key)))
                .is_some_and(|&first| first < index);
        read_before
            || facts
                .sides
                .get(attr.as_str())
                .and_then(|sides| sides.get(&key))
                .is_some_and(|dominated| self.shape.dominators().contains(dominated, bb))
    }

    /// [`KeyFacts`] of the function, from one walk on first use.
    fn key_facts(&self) -> &KeyFacts<'a> {
        self.key_facts.get_or_init(|| {
            let mut reads: FxHashMap<String, FxHashMap<(BlockId, KeyOf<'a>), usize>> =
                FxHashMap::default();
            let mut sides: FxHashMap<String, FxHashMap<KeyOf<'a>, Vec<BlockId>>> =
                FxHashMap::default();
            for b in &self.f.blocks {
                for (i, inst) in b.insts.iter().enumerate() {
                    if let InstKind::SubscriptLoad {
                        obj,
                        index,
                        is_slice: false,
                        ..
                    } = &inst.kind
                        && let Some(attr) = self.self_attr_alias.get(obj)
                    {
                        let key = self.key_identity(*index);
                        let first = reads.entry(attr.clone()).or_default();
                        first.entry((b.id, key)).or_insert(i);
                    }
                }
                if let Terminator::Branch {
                    cond,
                    then_bb,
                    else_bb,
                } = b.terminator
                    && let Some((attr, key, on_then)) = self.key_test(cond)
                {
                    let side = if on_then { then_bb } else { else_bb };
                    if self.shape.preds(side).len() == 1 {
                        let by_key = sides.entry(attr.clone()).or_default();
                        by_key.entry(key).or_default().push(side);
                    }
                }
            }
            let sides = sides
                .into_iter()
                .map(|(attr, by_key)| {
                    let by_key = by_key
                        .into_iter()
                        .map(|(key, blocks)| (key, self.shape.dominators().dominated_by(blocks)))
                        .collect();
                    (attr, by_key)
                })
                .collect();
            KeyFacts { sides, reads }
        })
    }

    /// The attribute of `self` whose container a branch on `cond` tests, the key, and the side
    /// on which the container holds the key (`true`: the `then` side): `key in c`,
    /// `key not in c`, `c.get(key)`, `c.get(key) is None`, `c.get(key) is not None`, through
    /// `not`.
    fn key_test(&self, cond: VarId) -> Option<(&String, KeyOf<'a>, bool)> {
        let none = |v: VarId| self.constant(v) == Some(&ConstLiteral::None);
        // `c.get(key)` (without a default other than `None`): `None` only for a missing key or a
        // stored `None`.
        let lookup = |v: VarId| match self.defs.get(&self.shape.copy_root(v)).map(|i| &i.kind) {
            Some(InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                ..
            }) if method == "get"
                && kwargs.is_empty()
                && (args.len() == 1 || args.len() == 2 && none(args[1])) =>
            {
                Some((*recv, args[0]))
            }
            _ => None,
        };
        // `not` and copies (acyclic: neither reads a phi).
        let (mut cur, mut negated) = (self.shape.copy_root(cond), false);
        while let Some(InstKind::UnaryOp {
            op: UnaryOpKind::Not,
            operand,
            ..
        }) = self.defs.get(&cur).map(|i| &i.kind)
        {
            negated = !negated;
            cur = self.shape.copy_root(*operand);
        }
        let (container, key, on_then) = match self.defs.get(&cur).map(|i| &i.kind) {
            Some(InstKind::Compare {
                op: op @ (CompareOpKind::In | CompareOpKind::NotIn),
                lhs,
                rhs,
                ..
            }) => (*rhs, *lhs, (*op == CompareOpKind::In) != negated),
            Some(InstKind::Compare {
                op: op @ (CompareOpKind::Is | CompareOpKind::IsNot),
                lhs,
                rhs,
                ..
            }) => {
                let tested = if none(*rhs) { *lhs } else { *rhs };
                let (container, key) = lookup(tested).filter(|_| none(*lhs) || none(*rhs))?;
                (container, key, (*op == CompareOpKind::Is) == negated)
            }
            _ => {
                let (container, key) = lookup(cur)?;
                (container, key, !negated)
            }
        };
        let attr = self.self_attr_alias.get(&container)?;
        Some((attr, self.key_identity(key), on_then))
    }

    /// The key `var` is, as the stores and the tests of keys compare them.
    fn key_identity(&self, var: VarId) -> KeyOf<'a> {
        match self.constant(var) {
            Some(literal) => KeyOf::Const(literal),
            None => KeyOf::Var(self.shape.copy_root(var)),
        }
    }

    /// The literal the constant `var` holds (through copies).
    fn constant(&self, var: VarId) -> Option<&'a ConstLiteral> {
        match self.defs.get(&self.shape.copy_root(var)).map(|i| &i.kind) {
            Some(InstKind::Const { literal, .. }) => Some(literal),
            _ => None,
        }
    }

    /// The attribute of `self` whose checked container (`MUT113`) `var` holds.
    fn checked_self_container(&self, var: VarId) -> Option<&String> {
        let checked = self.val(var).and_then(IteratedContainer::of_val).is_some();
        self.self_attr_alias.get(&var).filter(|_| checked)
    }

    /// Whether the container `var` holds is a new object the function bound to `self.<attr>`
    /// itself: `var` is a read of `self.<attr>` (through copies) that a store of a new object to
    /// `self.<attr>` comes before on every path (`self.xref = {}`, then `self.xref[k] = v`), and
    /// every store of the function to `self.<attr>` is of a new object. Its changes are that
    /// object's, not those of a container an iteration started before the call steps.
    fn rebound_before(&self, var: VarId, attr: &str, rebindings: &Rebindings<'_>) -> bool {
        let read = rebindings
            .reads
            .get(&self.shape.copy_root(var))
            .filter(|(read, _)| *read == attr);
        let (Some(&(_, (read_bb, read_index))), Some(stores)) = (read, rebindings.stores.get(attr))
        else {
            return false;
        };
        let tree = self.shape.dominators();
        stores.all_new
            && (stores
                .first_in_block
                .get(&read_bb)
                .is_some_and(|&first| first < read_index)
                || tree
                    .strict_dominator(read_bb)
                    .is_some_and(|d| tree.contains(&stores.dominated, d)))
    }

    /// [`Rebindings`] of the attributes `changes` change, from one walk of the function once
    /// the callees of all its calls are known.
    fn rebindings(&self, changes: &[SelfChange]) -> Rebindings<'a> {
        let changed: FxHashSet<&str> = changes.iter().map(|c| c.attr.as_str()).collect();
        let mut reads = FxHashMap::default();
        let mut stores: FxHashMap<&'a str, Vec<(Position, VarId)>> = FxHashMap::default();
        for b in &self.f.blocks {
            for (i, inst) in b.insts.iter().enumerate() {
                match &inst.kind {
                    InstKind::AttrLoad { dst, obj, attr } if self.self_vars.contains(obj) => {
                        reads.insert(*dst, (attr.as_str(), (b.id, i)));
                    }
                    InstKind::AttrStore { obj, attr, value }
                        if self.self_vars.contains(obj) && changed.contains(attr.as_str()) =>
                    {
                        stores
                            .entry(attr.as_str())
                            .or_default()
                            .push(((b.id, i), *value));
                    }
                    _ => {}
                }
            }
        }
        let callees = self.callees_by_result();
        let new_object = |v: VarId| self.made_here(v, &callees);
        let stores = stores
            .into_iter()
            .map(|(attr, at)| {
                let mut first_in_block = FxHashMap::default();
                for &((b, i), _) in &at {
                    first_in_block.entry(b).or_insert(i);
                }
                let rebinding = Rebinding {
                    all_new: at.iter().all(|&(_, value)| new_object(value)),
                    first_in_block,
                    dominated: self
                        .shape
                        .dominators()
                        .dominated_by(at.iter().map(|&((b, _), _)| b)),
                };
                (attr, rebinding)
            })
            .collect();
        Rebindings { stores, reads }
    }

    /// The project callees of the function's calls, by the variables their results go to.
    fn callees_by_result(&self) -> FxHashMap<VarId, &Callee> {
        let mut out: FxHashMap<VarId, &Callee> = FxHashMap::default();
        for bb in &self.f.blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                if let (Some(dst), Some(c)) = (inst.kind.dst(), self.callees.get(&(bb.id, i))) {
                    out.insert(dst, c);
                }
            }
        }
        out
    }

    /// An in-place change of the container `var` holds (`MUT113`): of a parameter itself, and of
    /// `self.<attr>` of a class whose iterators check for changes ([`Self::self_changes`]).
    fn container_change(&mut self, var: VarId, change: ContainerChange, span: Span) {
        if !self.opt_in.iterations {
            return;
        }
        if let Some((i, None)) = self.param_alias.get(&var).cloned() {
            let facts = self.summary.iteration_facts_mut();
            iterations::merge_changes(&mut facts.changes_param_containers, [(i, change)]);
        }
        if let Some(attr) = self.checked_self_container(var).cloned() {
            self.self_changes.push(SelfChange {
                var,
                attr,
                change,
                span,
            });
        }
    }

    /// Record the changes of checked containers of `self` the visit found, but those of a new
    /// object the function bound to the attribute before ([`Self::rebound_before`]): once the
    /// callees of all the calls are known, which tell the stores of new objects.
    fn self_container_changes(&mut self) {
        let changes = std::mem::take(&mut self.self_changes);
        if changes.is_empty() {
            return;
        }
        let rebindings = self.rebindings(&changes);
        let kept: Vec<SelfChange> = changes
            .into_iter()
            .filter(|c| !self.rebound_before(c.var, &c.attr, &rebindings))
            .collect();
        for SelfChange {
            attr, change, span, ..
        } in kept
        {
            let facts = self.summary.iteration_facts_mut();
            iterations::merge_changes(&mut facts.changes_self_containers, [(attr.clone(), change)]);
            facts
                .self_container_change_sites
                .entry(attr)
                .or_default()
                .insert(span);
        }
    }

    /// [`ProjectDecls::method_not_overridden`], asked once per function for each class, method,
    /// and lookup.
    fn builtin_method_runs(&mut self, class: String, method: &str, by_name: bool) -> bool {
        let key = (class, method.to_owned(), by_name);
        if let Some(&known) = self.builtin_methods.get(&key) {
            return known;
        }
        let runs = self
            .cx
            .project
            .method_not_overridden(&key.0, method, by_name);
        self.builtin_methods.insert(key, runs);
        runs
    }

    /// In-place mutation of the container held in `var` (mutator method or subscript write) by
    /// `method` (`append`, `__setitem__`, `__iadd__`, ...), looked up `by_name` (a method call)
    /// or called implicitly (an item store, an augmented assignment): `const_key` is a literal
    /// key (`self.<attr>` keys), `item_key` the key of the changed entry ([`Self::entry_key`]).
    fn container_mut(
        &mut self,
        var: VarId,
        method: &str,
        by_name: bool,
        const_key: Option<&str>,
        item_key: (Option<String>, EntryKey),
        span: Span,
    ) {
        self.change_object(var, span);
        if let Some(attr) = self.self_attr_alias.get(&var).cloned() {
            self.summary.mutates_self_attrs.insert(AttrKeyPath {
                attr,
                const_key: const_key.map(str::to_owned),
            });
        }
        match self.param_alias.get(&var).cloned() {
            Some((i, None)) => {
                // A builtin container's own method or item store changes it in place; another
                // class's may not (a `Sequence` whose `append` returns a new one, or, under R2, a
                // project subclass of the builtin whose `reverse` does nothing).
                let class = match self.val(var) {
                    Some(Val::Inst(class, _)) if BUILTIN_CONTAINERS.contains(&class.as_str()) => {
                        Some(class.clone())
                    }
                    _ => None,
                };
                let builtin =
                    class.is_some_and(|class| self.builtin_method_runs(class, method, by_name));
                if builtin {
                    self.summary
                        .param_inplace_sites
                        .entry(i)
                        .or_default()
                        .insert(span);
                    if self.retyped.contains(&var) && self.readonly_param(i) {
                        let changes = self.summary.retyped_param_changes.get_or_insert_default();
                        changes.entry(i).or_default().insert(span);
                    }
                }
                self.mutate_param(i, String::new(), span);
            }
            Some((i, Some(path))) => self.change_below_param_attr(i, path, span),
            None => self.change_param_element(Some(var), true, span),
        }
        if let Some((i, slot)) = self.param_slot_alias.get(&var).cloned() {
            self.summary
                .inplace_param_slots
                .entry(i)
                .or_default()
                .insert(slot);
        }
        if let Some(a) = self.obj_attr_alias.get(&var).cloned() {
            let kind = if a.direct {
                WriteKind::ContainerMut
            } else {
                WriteKind::AliasContainerMut
            };
            let written = a.written();
            for class in a.classes {
                self.typed_site(class, &a.attr, item_key.clone(), kind, span, written);
            }
        } else if let Some(i) = self.item_alias.get(&var).cloned() {
            // `obj.attr["k"][name] = v`: an entry of an entry of the attribute's container.
            let written = i.written();
            for class in i.classes {
                let key = (i.key.clone(), EntryKey::Written);
                self.typed_site(class, &i.attr, key, WriteKind::ItemMut, span, written);
            }
        } else if let Some((attr, obj, nested)) = self.untyped_container(var) {
            let kind = if nested {
                WriteKind::ItemMut
            } else {
                WriteKind::ContainerMut
            };
            self.unresolved(Unresolved::Receiver(attr), obj, kind, span);
        }
    }

    fn subscript_write(
        &mut self,
        obj: VarId,
        key_const: Option<&str>,
        index: VarId,
        is_delete: bool,
        span: Span,
    ) {
        if let Some(owner) = self.dict_of.get(&obj).cloned() {
            // `x.__dict__["a"] = v` / `del self.__dict__["prop"]` / `x.__dict__[KEY] = v`.
            let name = match key_const {
                Some(k) => AttrName::Named(k.to_owned().into()),
                None => self.key_name(index),
            };
            self.write_named(owner, &name, is_delete, WriteKind::DictDirect, span);
            return;
        }
        // The changed entry's key matters only for a typed attribute's container.
        let item_key = if self.obj_attr_alias.contains_key(&obj) {
            self.entry_key(key_const, index)
        } else {
            (None, EntryKey::Written)
        };
        let method = if is_delete {
            "__delitem__"
        } else {
            "__setitem__"
        };
        self.container_mut(obj, method, false, key_const, item_key, span);
    }

    /// A call of the container method `method` on `recv` with the positional arguments
    /// `args` (`plain`: and no keyword or `*` argument).
    fn container_method(
        &mut self,
        recv: VarId,
        method: &str,
        args: &[VarId],
        plain: bool,
        span: Span,
    ) {
        if let Some(owner) = self.dict_of.get(&recv).cloned() {
            let kind = WriteKind::DictDirect;
            match (method, args) {
                ("pop" | "__delitem__" | "setdefault" | "__setitem__", _) => {
                    let name = args
                        .first()
                        .map_or(AttrName::Unknown, |a| self.key_name(*a));
                    let removes = matches!(method, "pop" | "__delitem__");
                    self.write_named(owner, &name, removes, kind, span);
                }
                ("update", [other]) if plain => {
                    let names = self.update_keys(*other);
                    self.write_named(owner, &names, false, kind, span);
                }
                ("popitem" | "clear", _) => {
                    if method == "clear" && owner.is_self {
                        self.summary.invalidates_cached.insert("*".to_owned());
                    }
                    self.computed_name(&owner, Adds::Nothing, kind, span);
                }
                // `update` of keywords, `|=` (`__ior__`): attributes heki cannot name.
                _ => self.computed_name(&owner, Adds::Any, kind, span),
            }
            return;
        }
        // `obj.attr.pop("k")`, `obj.attr.setdefault(KEY, v)`: the entry the call changes, for a
        // typed attribute's container.
        let item_key = match args.first() {
            Some(a)
                if self.obj_attr_alias.contains_key(&recv)
                    && matches!(method, "pop" | "__delitem__" | "setdefault" | "__setitem__") =>
            {
                self.entry_key(None, *a)
            }
            _ => (None, EntryKey::Written),
        };
        self.container_mut(recv, method, true, None, item_key, span);
    }

    fn hop(&self, callee: String, span: Span) -> CallHop {
        CallHop {
            caller: self.key.clone(),
            callee,
            span,
            path: self.path.to_path_buf(),
        }
    }

    /// `drives_coroutine_manually`: the call result is `.send(...)`-driven or handed to a
    /// `run_sync`-style driver. Copies of an SSA variable form a tree (every variable has one
    /// definition), so the walk visits each copy once.
    fn drives_manually(&self, dst: VarId) -> bool {
        let mut vars = vec![dst];
        while let Some(v) = vars.pop() {
            for u in self.uses.get(&v).into_iter().flatten() {
                match u {
                    InstKind::Copy { dst, .. } => vars.push(*dst),
                    InstKind::MethodCall { recv, method, .. } if *recv == v && method == "send" => {
                        return true;
                    }
                    InstKind::Call { callee, args, .. } if args.contains(&v) => {
                        if callee
                            .rsplit('.')
                            .next()
                            .is_some_and(|n| n.ends_with("run_sync"))
                        {
                            return true;
                        }
                    }
                    InstKind::MethodCall { method, args, .. }
                        if args.contains(&v) && method.ends_with("run_sync") =>
                    {
                        return true;
                    }
                    _ => {}
                }
            }
        }
        false
    }

    /// The constant `var` holds ([`ArgValue`]): a literal through copies, a string constant
    /// bound to a name ([`TypeCtx::str_constant_with`]), or a module's number or boolean
    /// constant ([`TypeCtx::global_literal`]).
    fn arg_value(&self, var: VarId) -> Option<ArgValue> {
        let def = |v: VarId| self.defs.get(&v).copied();
        resolver::const_literal(def, var)
            .and_then(ArgValue::of_literal)
            .or_else(|| {
                let text = self.cx.str_constant_with(var, &def, &self.types)?;
                Some(ArgValue::Str(text))
            })
            .or_else(|| {
                let module_of = |v: VarId| match self.types.get(&v)? {
                    Val::Ref(fqn) => Some(fqn.clone()),
                    _ => None,
                };
                let globals = |inst: &Inst| self.cx.module_constant(inst, &module_of);
                match resolver::literal(def, &globals, var)? {
                    LiteralValue::Bool(b) => Some(ArgValue::Bool(b)),
                    LiteralValue::Int(n) => Some(ArgValue::Int(n)),
                    // A string is read above, and a float is no value an argument list names.
                    _ => None,
                }
            })
    }

    /// The positional arguments of `call` with the parameter slot each one binds, counted from
    /// `offset` (a bound receiver): an argument unpacked with `*` binds the items of a display
    /// of known length one by one (`*(a, b)`, `*()`); past one of unknown length, no argument
    /// has a known slot.
    fn positional_slots(&self, call: &Call<'_>, offset: usize) -> Vec<(usize, VarId)> {
        let mut out = Vec::new();
        let mut slot = offset;
        for (j, a) in call.args.iter().enumerate() {
            if !call
                .args_at
                .is_some_and(|at| call.dst.is_some_and(|dst| self.f.is_starred(dst, at + j)))
            {
                out.push((slot, *a));
                slot += 1;
                continue;
            }
            let Some(InstKind::AllocBuiltin {
                kind: BuiltinKind::Tuple | BuiltinKind::List,
                elems,
                dst,
            }) = self.defs.get(a).map(|inst| &inst.kind)
            else {
                break;
            };
            if (0..elems.len()).any(|k| self.f.is_starred(*dst, k)) {
                break;
            }
            for e in elems {
                out.push((slot, *e));
                slot += 1;
            }
        }
        out
    }

    /// [`Self::call`] for a `Call` / `MethodCall` instruction at `(bb, index)`, or a property
    /// read, of the method or attribute `name` or the value whose source text `name` is: its
    /// callee is kept for the iteration flow, and a project call is remembered for the sites of
    /// the object it returns.
    fn call_inst(
        &mut self,
        (bb, index): (BlockId, usize),
        name: &str,
        callee: Callee,
        call: &Call<'_>,
    ) {
        if self.opt_in.blind_spots {
            self.note_blind(bb, name, &callee, call);
        }
        if self.opt_in.imports && matches!(callee, Callee::Unresolved | Callee::TooMany) {
            self.maybe_import_by_name(bb, name, call);
        }
        let sites_before = self.sites.len();
        let made = self.call(bb, &callee, call);
        if self.sites.len() > sites_before {
            let member = self.module_member(&self.sites[sites_before], call, name);
            for site in &mut self.sites[sites_before..] {
                site.module_member.clone_from(&member);
            }
        }
        // A library call keeps its unbound record beside its Layer 2 site, which binds no
        // parameter; a construction of a project class binds them on its own site.
        let constructs = (self.sites[sites_before..].iter()).any(|s| s.constructs.is_some());
        if !matches!(callee, Callee::Project(..)) && !constructs {
            self.unbound_call(call);
        }
        if let Some((site, params)) = made
            && let Some(dst) = call.dst
        {
            let binds_recv = self.sites[site].binds_recv && call.constructs.is_none();
            self.project_calls.push(ProjectCall {
                site,
                at: (bb, index),
                dst,
                params,
                recv: call.recv_var.or(call.value).filter(|_| binds_recv),
            });
        }
        self.callees.insert((bb, index), callee);
    }

    /// [`FileSummary::unbound_calls`]: `call`, which heki binds to no project function or class,
    /// when it passes by keyword a constant for a marked parameter and a call target names
    /// every callee.
    fn unbound_call(&mut self, call: &Call<'_>) {
        if !self.contracts.is_some_and(|c| c.any_callee_arguments()) {
            return;
        }
        if let Some(arguments) = self.keyword_marks(call) {
            self.unbound.push(UnboundCall {
                span: call.span,
                arguments: *arguments,
            });
        }
    }

    /// [`CallSite::module_member`] of `site`, of a call of method `name` on `call`'s receiver.
    fn module_member(&self, site: &CallSite, call: &Call<'_>, name: &str) -> Option<Box<FuncKey>> {
        let target = site.constructs.clone().or_else(|| {
            let callees = site.project_callees();
            (callees.len() == 1).then(|| callees[0].callee.fqn())
        })?;
        self.member_of_module(call.recv_var?, name, &target)
    }

    /// The name `name` of the project module `obj` holds, when a call target has a module part
    /// and the module binds it to `target`, a definition of another module (a name it imports):
    /// a load of it from the module object reads it by that name ([`CallSite::module_member`]).
    fn member_of_module(&self, obj: VarId, name: &str, target: &str) -> Option<Box<FuncKey>> {
        if !self.contracts.is_some_and(|c| c.module_members()) {
            return None;
        }
        let Some(Val::Ref(module)) = self.val(obj) else {
            return None;
        };
        self.cx.project.module(module)?;
        let own = target
            .strip_prefix(module.as_str())
            .and_then(|rest| rest.strip_prefix('.'))
            == Some(name);
        (!own).then(|| {
            Box::new(FuncKey {
                module: module.clone(),
                qualname: name.to_owned(),
            })
        })
    }

    /// [`FileSummary::blind_calls`]: the call `call` (of `name`, as for [`Self::call_inst`]) in
    /// block `bb` when heki resolves it to no function, or to a library name the catalog has no
    /// entry for.
    fn note_blind(&mut self, bb: BlockId, name: &str, callee: &Callee, call: &Call<'_>) {
        // The method called, or the value called when it is a dotted name (`handler`), not an
        // expression computing it (`make()`, `xs[0]`).
        let name = || is_dotted_name(name).then(|| name.to_owned());
        let blind = match callee {
            Callee::Project(..) => return,
            Callee::External(symbol) => {
                let Some(registered) = blind_spots::uncatalogued(self.cx, symbol) else {
                    return;
                };
                Blind::Uncatalogued {
                    symbol: symbol.clone(),
                    registered,
                }
            }
            Callee::TooMany | Callee::Unresolved => Blind::Unresolved {
                name: name(),
                reason: call
                    .recv_var
                    .or(call.value)
                    .map_or(UnresolvedReason::UntypedValue, |subject| {
                        self.unresolved_reason(callee, subject)
                    }),
            },
        };
        self.blind.push(BlindCall {
            span: call.span,
            loop_depth: self.shape.loop_depth(bb),
            is_awaited: call.is_awaited,
            blind,
        });
    }

    /// Why heki resolves a call on `subject` (its receiver, or the value it calls) to no
    /// function (`callee`).
    fn unresolved_reason(&self, callee: &Callee, subject: VarId) -> UnresolvedReason {
        if *callee == Callee::TooMany {
            return UnresolvedReason::TooManyImplementations;
        }
        if let Some(reason) = self.opaque_target(subject) {
            return reason;
        }
        match self.val(subject) {
            Some(Val::Inst(c, _)) if self.cx.project.protocol(c) == Some(None) => {
                UnresolvedReason::UnlistedProtocol
            }
            Some(_) => UnresolvedReason::UnknownMember,
            None => self.untyped_origin(subject),
        }
    }

    /// Why a call on `var` reaches no function heki knows although `var` has a type: the type
    /// (of the instance or class object, or of a member of the union) says nothing of its
    /// methods (`Any`, `object`) or of the function it holds (`Callable`), or `var` is a name
    /// nothing heki sees binds, which heki looks up in `builtins`.
    fn opaque_target(&self, var: VarId) -> Option<UnresolvedReason> {
        let of_class = |class: &String| {
            if resolver::of_any_class(class) {
                Some(UnresolvedReason::AnyValue)
            } else {
                resolver::is_any_callable(class).then_some(UnresolvedReason::CallableValue)
            }
        };
        match self.val(var)? {
            Val::Ref(name) => {
                resolver::is_unbound_name(name).then_some(UnresolvedReason::UnboundName)
            }
            // `super()` is that of a project class, which says what it has.
            Val::Inst(class, _) | Val::Class(class) | Val::Super(class) => of_class(class),
            Val::Union(classes) => classes.iter().find_map(of_class),
            Val::OneOf(_) => None,
        }
    }

    /// Where `var`, a value heki gives no class, comes from.
    fn untyped_origin(&self, var: VarId) -> UnresolvedReason {
        if let Some(&(param, None)) = self.param_alias.get(&var) {
            let p = &self.f.params[usize::from(param)];
            let name = p.name.clone();
            return match p.annotation {
                None => UnresolvedReason::UnannotatedParameter { param, name },
                Some(_) => UnresolvedReason::UnresolvedAnnotation { param, name },
            };
        }
        match self.defs.get(&self.shape.copy_root(var)).map(|i| &i.kind) {
            Some(InstKind::Call {
                callee_var: Some(f),
                ..
            }) if matches!(self.val(*f), Some(Val::Ref(s)) if s == "builtins.getattr") => {
                UnresolvedReason::ComputedName
            }
            Some(InstKind::Call { .. } | InstKind::MethodCall { .. }) => {
                UnresolvedReason::CallResult
            }
            Some(InstKind::AttrLoad { .. }) => UnresolvedReason::Attribute,
            _ => UnresolvedReason::UntypedValue,
        }
    }

    /// Record one call. A project callee decorated with an offloading decorator
    /// (`FunctionDecl::offloaded_by`) is called as `d(f)(...)`: a call of the catalog's `d` with
    /// the function as its offloaded argument, so its body does not run here. Returns the index
    /// of the project call site, with each kept callee and the parameter each argument
    /// variable binds.
    #[allow(clippy::type_complexity)]
    fn call(
        &mut self,
        bb: BlockId,
        callee: &Callee,
        call: &Call<'_>,
    ) -> Option<(usize, Vec<(FuncKey, Vec<(u8, VarId)>)>)> {
        let loop_depth = self.shape.loop_depth(bb);
        self.call_escapes(callee, call);
        match callee {
            Callee::External(symbol) => {
                if self.opt_in.imports {
                    self.import_by_name(bb, symbol, call);
                }
                if let Some(c) = self.contracts {
                    if c.external_call(symbol) {
                        let imports = self.cx.catalog(symbol).and_then(|e| e.environment_access())
                            == Some(EnvironmentAccess::Import);
                        self.external.push(ExternalAccess {
                            kind: AccessKind::Call,
                            name: symbol.clone(),
                            span: call.span,
                            imports_project_module: imports
                                && !environment::guarded_against_import_errors(self.f, bb)
                                && self.imports_project_module(&call.passed()),
                        });
                    }
                    if let Some(step) = call.dst.and_then(|d| self.returned.get(&d).copied())
                        && let Some(produced) = c.readonly_producer(symbol, symbol)
                    {
                        self.seed_protected(symbol.clone(), produced, step, call.span);
                    }
                    let mask = c.invalidator_of(symbol, symbol);
                    if let Some(events) = self.events.as_mut()
                        && mask != 0
                    {
                        events.invalidators.push((self.pos.0, self.pos.1, mask));
                    }
                }
                self.external_call(
                    bb,
                    symbol.clone(),
                    call.passed(),
                    call.is_awaited,
                    call.span,
                );
                // `list(query)` iterates the query as a `for` loop does.
                for iterable in crate::consumption::iterated_args(symbol, call.args) {
                    self.protocol_call(bb, iterable, iteration_protocol(false), false, call.span);
                }
                self.note_returned_realloc(symbol, call);
                // A project class without a project `__init__` (a dataclass, a pydantic model),
                // and a library API building an instance of its receiver's class
                // (`Model.model_construct(...)`): the construction is still a call site, with no
                // project callees.
                let built = match &call.constructs {
                    Some(class) => Some((class.clone(), self.keyword_marks(call))),
                    None => self
                        .built_receiver(symbol, call)
                        .map(|(class, fields)| (class, self.field_marks(call, fields))),
                };
                if let Some((class, arguments)) = built {
                    self.sites.push(CallSite {
                        caller: self.key.clone(),
                        target: CallTarget::Project(Vec::new()),
                        is_awaited: call.is_awaited,
                        span: call.span,
                        loop_depth,
                        recv_is_self: call.recv_is_self,
                        drives_coroutine_manually: false,
                        constructs: Some(class),
                        role: SiteRole::Call,
                        returned: None,
                        flow: None,
                        guards: Vec::new(),
                        once_per: None,
                        once_loops: 0,
                        runner: Runner::Caller,
                        binds_recv: false,
                        handed_callable: None,
                        per_item: None,
                        dispatch: None,
                        result_discarded: false,
                        queues: None,
                        result_held: false,
                        is_base: false,
                        is_read: false,
                        arguments,
                        state_tests: None,
                        locks: crate::LockHeld::None,
                        shield: crate::ShieldAt::Unshielded,
                        complete: false,
                        on_every_return: crate::OnEveryReturn::default(),
                        made_by: None,
                        module_member: None,
                    });
                }
                if self.opt_in.unawaited_handoffs {
                    self.unawaited_handoff(symbol, call);
                }
                self.handed_callable(bb, symbol, call);
                None
            }
            Callee::Project(keys, binds_recv) => {
                // A constructor call returns the new instance, not what `__init__` returns.
                let constructor = call.constructs.is_some();
                // The contracts the call announces: those every callee whose body may run at
                // the call is an invalidator of (an abstract method never runs).
                let mut invalidates: Option<u64> = None;
                let mut callees = Vec::new();
                let mut bindings = Vec::new();
                // The queues the call binds exactly (`SiteQueues::handed`).
                let mut handed = Vec::new();
                let mut offloaders: Vec<String> = Vec::new();
                // The bodies an offloading decorator runs on a worker thread.
                let mut on_worker = Vec::new();
                let mut any_async = false;
                let unknown = FunctionDecl::default();
                let mut buffer = std::mem::take(&mut self.param_origins);
                let loops = self.loops_around(bb);
                // `(callee, its index, what it is handed)` for `Self::hand_over`.
                let mut handovers = Vec::new();
                // What each callee returns at the call, for `Self::project_alloc`.
                let mut returns = Vec::with_capacity(keys.len());
                // What each callee running here is passed for the marked parameters, for
                // `agreed_marks`, on each way the mappings the call unpacks may be built when
                // there are several.
                let mut marks = Vec::new();
                let keyword_ways = self
                    .marked_params(call)
                    .map(|_| self.keyword_ways(call))
                    .filter(|ways| ways.len() > 1);
                let mut handed_state: Vec<HandedState> = Vec::new();
                // A constructor call binds the object it creates, unless the class may return
                // an existing instance.
                let new_recv = call
                    .constructs
                    .as_ref()
                    .map(|c| !self.cx.project.may_reuse_instances(c));
                for key in keys {
                    let cx = self.cx;
                    let decl = cx.project.function_decl(key).unwrap_or(&unknown);
                    let offloaded = decl.offloaded_by.is_some();
                    if let Some(symbol) = &decl.offloaded_by
                        && !offloaders.contains(symbol)
                    {
                        offloaders.push(symbol.clone());
                    }
                    any_async |= decl.is_async && !offloaded;
                    if let (Some(c), Some(_)) = (self.contracts, &self.events)
                        && !decl.is_abstract
                    {
                        // A generator's body runs only where it is iterated, an offloaded
                        // one on another thread.
                        let here = if decl.is_generator || offloaded {
                            0
                        } else {
                            c.invalidator_of(&key.fqn(), &key.qualname)
                                | c.overriding_invalidator(key, cx.project)
                        };
                        invalidates = Some(invalidates.map_or(here, |i| i & here));
                    }
                    let offset = usize::from(*binds_recv && !decl.is_staticmethod);
                    let mut params = std::mem::take(&mut buffer);
                    // The receiver the call hands over (a constructor's is its new object).
                    let mut handed_recv = None;
                    // The receiver is the first positional argument: it binds the first
                    // parameter where a position binds that one, else `*args` holds it.
                    let receiver_binds = offset == 1 && decl.positional_param(0).is_some();
                    if offset == 1 {
                        // A class method binds the class (no value of the caller), a
                        // constructor the object it creates; calling an instance (`p()`) binds
                        // it to the `self` of its `__call__`.
                        let receiver = if decl.is_classmethod {
                            ArgOrigin::default()
                        } else if let Some(new) = new_recv {
                            ArgOrigin {
                                new_object: if new {
                                    NewArg::New {
                                        kind: NewKind::Own,
                                        cond: NewCond::default(),
                                    }
                                } else {
                                    NewArg::Old
                                },
                                exact: call.dst.and_then(|made| self.exact_line(made)),
                                ..ArgOrigin::default()
                            }
                        } else {
                            handed_recv = call.recv_var.or(call.value);
                            call.recv
                                .clone()
                                .or_else(|| call.value.map(|v| self.origin(v)))
                                .unwrap_or_default()
                        };
                        params.push(if receiver_binds {
                            receiver
                        } else {
                            ArgOrigin::default()
                        });
                    }
                    // A position binds its parameter, and a keyword too unless it lands in
                    // `**kwargs`.
                    let passing = self.bindings(call, decl, offset);
                    let mut bound: Vec<(u8, VarId)> = Vec::new();
                    // What the call hands the callee, by parameter: the arguments bound exactly
                    // and, at the position of `*args`, those the tuple holds; the callee keeps
                    // one of those only by keeping `*args`, and no parameter is it. A position
                    // from `*` unpacking on (slot `unpacked`) is not taken as handed exactly.
                    let mut handed_args: Vec<(u8, VarId)> = Vec::new();
                    let held_at = decl.varargs_param().and_then(|p| u8::try_from(p).ok());
                    let unpacked = call
                        .star_from
                        .map_or(usize::MAX, |s| offset + usize::from(s));
                    if let Some(recv) = handed_recv {
                        let at = if receiver_binds { Some(0) } else { held_at };
                        handed_args.extend(at.map(|p| (p, recv)));
                    }
                    for &(slot, a) in &passing.slots {
                        let exact = slot < unpacked;
                        let Some(slot) = decl.positional_param(slot) else {
                            handed_args.extend(held_at.filter(|_| exact).map(|p| (p, a)));
                            continue;
                        };
                        let mut o = self.origin(a);
                        o.binding = Some(self.binding(a, loops));
                        handed.extend(self.handed_queue(key, slot, a));
                        if params.len() <= slot {
                            params.resize(slot + 1, ArgOrigin::default());
                        }
                        params[slot] = o;
                        if let Ok(slot) = u8::try_from(slot) {
                            bound.push((slot, a));
                            handed_args.extend(exact.then_some((slot, a)));
                        }
                    }
                    for &(_, v, pos) in &passing.keywords {
                        let Some(pos) = pos else { continue };
                        if params.len() <= pos {
                            params.resize(pos + 1, ArgOrigin::default());
                        }
                        params[pos] = self.origin(v);
                        params[pos].binding = Some(self.binding(v, loops));
                        handed.extend(self.handed_queue(key, pos, v));
                        if let Ok(pos) = u8::try_from(pos) {
                            bound.push((pos, v));
                            handed_args.push((pos, v));
                        }
                    }
                    for (pos, c) in passing.defaults(call, decl, offset) {
                        if params.len() <= pos {
                            params.resize(pos + 1, ArgOrigin::default());
                        }
                        params[pos].binding = Some(ArgBinding {
                            fact: Some(Box::new(c.clone())),
                            ..ArgBinding::default()
                        });
                    }
                    if let Some(items) = call.per_item {
                        items.bind(
                            &mut params,
                            offset + call.args.len(),
                            usize::from(decl.positional_params),
                        );
                    }
                    while params.last().is_some_and(ArgOrigin::is_empty) {
                        params.pop();
                    }
                    self.bind_captured(key, decl, &mut params);
                    if self.ownership && !offloaded {
                        handovers.push((decl, callees.len(), handed_args));
                    }
                    if !offloaded && let Some(marked) = self.marked_params(call) {
                        match &keyword_ways {
                            None => {
                                marks.push(self.callee_marks(call, decl, offset, &passing, marked));
                            }
                            Some(ways) => {
                                for way in ways {
                                    let passing = self.bind(call, decl, offset, way.clone());
                                    marks.push(
                                        self.callee_marks(call, decl, offset, &passing, marked),
                                    );
                                }
                            }
                        }
                    }
                    if !offloaded && self.opt_in.shared_state {
                        handed_state.extend(self.handed(
                            call.span,
                            callees.len(),
                            handed_recv.filter(|_| receiver_binds),
                            &bound,
                        ));
                    }
                    if self.ownership {
                        let known = self.guards.facts_at(&self.guard_inputs(), bb);
                        returns.push(NewFrom {
                            callee: key.clone(),
                            empty_params: params_without_elements(&params),
                            at: CallTests::of_call(&params, &known),
                        });
                    }
                    // A copy of the exact length; the buffer keeps its room for the next call.
                    let mut exact = Vec::with_capacity(params.len());
                    exact.append(&mut params);
                    let bc = BoundCallee {
                        callee: key.clone(),
                        params: exact,
                    };
                    buffer = params;
                    if offloaded {
                        on_worker.push(bc);
                    } else {
                        callees.push(bc);
                        bindings.push((key.clone(), bound));
                    }
                }
                self.param_origins = buffer;
                let inits: Vec<usize> = handovers.iter().map(|(_, i, _)| *i).collect();
                self.project_alloc(call, returns, &inits);
                if !handed_state.is_empty() {
                    self.summary.shared_state_mut().handed.extend(handed_state);
                }
                for (decl, i, handed_args) in handovers {
                    self.hand_over(call, decl, i, &handed_args);
                }
                // `@d def f` is `f = d(f)`: the call runs `d(f)`, which runs `f` on a worker.
                for symbol in offloaders {
                    let offloaded: Vec<VarId> = call.value.into_iter().collect();
                    let passed = Passed {
                        args: &offloaded,
                        kwargs: &[],
                        star_from: None,
                        recv: None,
                    };
                    self.external_call(bb, symbol, passed, call.is_awaited, call.span);
                }
                // The site the hand-overs above name comes first; the worker's follows it.
                if callees.is_empty() {
                    self.worker_site(bb, on_worker, call);
                    return None;
                }
                let drives = any_async
                    && !call.is_awaited
                    && call.dst.is_some_and(|d| self.drives_manually(d));
                if let Some(events) = self.events.as_mut() {
                    events
                        .calls
                        .push((self.pos.0, self.pos.1, self.sites.len()));
                    // An `async` invalidator only runs when it is awaited or driven, and a
                    // generator's body only where it is iterated.
                    let invalidates = invalidates.unwrap_or(0);
                    if invalidates != 0 && (!any_async || call.is_awaited || drives) {
                        events
                            .invalidators
                            .push((self.pos.0, self.pos.1, invalidates));
                    }
                }
                let arguments = agreed_marks(marks);
                let site = self.site(bb, CallTarget::Project(callees), call.is_awaited, call.span);
                let mut dispatch = self.dispatch(call, site.project_callees());
                if let Some(recv) = call.recv.as_ref().filter(|r| r.may_params != 0)
                    && self.contracts.is_some_and(|c| c.own_receiver_calls())
                    && site.project_callees().iter().any(|bc| {
                        self.cx
                            .project
                            .function_decl(&bc.callee)
                            .is_some_and(|d| d.is_classmethod || d.is_staticmethod)
                    })
                {
                    dispatch.get_or_insert_with(Box::default).recv_params = recv.may_params;
                }
                self.sites.push(CallSite {
                    arguments,
                    recv_is_self: call.recv_is_self,
                    // A builtin calling the callees per item uses what they return.
                    result_discarded: call.recv_is_self
                        && call.per_item.is_none()
                        && call.dst.is_some_and(|d| self.discards(d)),
                    drives_coroutine_manually: drives,
                    binds_recv: *binds_recv,
                    dispatch,
                    queues: (!handed.is_empty()).then(|| {
                        Box::new(SiteQueues {
                            while_full: None,
                            handed,
                        })
                    }),
                    constructs: call.constructs.clone(),
                    returned: if constructor || call.per_item.is_some() {
                        None
                    } else {
                        call.dst.and_then(|d| self.returned.get(&d).copied())
                    },
                    // The instruction's completeness is its own call's (`Self::targets_complete`):
                    // the getters a call of a property's value runs, as a read of the property,
                    // never are complete.
                    complete: site.complete && call.dst.is_some(),
                    ..site
                });
                let at = self.sites.len() - 1;
                self.worker_site(bb, on_worker, call);
                Some((at, bindings))
            }
            Callee::TooMany | Callee::Unresolved => {
                self.summary.unknown_calls = true;
                None
            }
        }
    }

    /// The class line of the method's own class: what its `self` is an instance of.
    fn own_line(&self) -> Option<SharedLine> {
        self.own_line
            .get_or_init(|| {
                let owner = self.cx.owner_fqn(self.f)?;
                self.cx
                    .project
                    .is_project_class(&owner)
                    .then(|| self.cx.project.lineage(&owner, false))
            })
            .clone()
    }

    /// [`Dispatch::declared`]: the definition of the called method in the project protocol the
    /// receiver is typed with, when the protocol declares it.
    fn declared_method(&self, call: &Call<'_>, callees: &[BoundCallee]) -> Option<FuncKey> {
        let Some(Val::Inst(protocol, _)) = self.val(call.recv_var?) else {
            return None;
        };
        let name = heki_semantic::mangling::member_name(&callees.first()?.callee.qualname);
        let project = self.cx.project;
        project
            .protocol_declares(protocol, &name)
            .then(|| project.mro_method(protocol, &name))?
    }

    /// What the receiver of `call` tells about the implementations `callees` run
    /// ([`Dispatch`]): the receiver's class (the method's own `self`, a receiver of one project
    /// class, or the class a constructor call makes) when a callee is inherited from a proper
    /// base of it, and, on a parameter of the caller as itself (`self` too), the callees that
    /// are overrides of subclasses.
    fn dispatch(&self, call: &Call<'_>, callees: &[BoundCallee]) -> Option<Box<Dispatch>> {
        let project = self.cx.project;
        let own;
        let class: &str = if call.recv_is_self {
            own = self.cx.owner_fqn(self.f)?;
            &own
        } else {
            call.constructs
                .as_deref()
                .or_else(|| call.recv.as_ref()?.typed_instance.as_deref())?
        };
        // The class of a method among the callees, as `(module, class qualname)`.
        fn class_of<'b>(project: &ProjectDecls, bc: &'b BoundCallee) -> Option<(&'b str, &'b str)> {
            let (owner, _) = bc.callee.qualname.rsplit_once('.')?;
            project
                .function_decl(&bc.callee)
                .is_some_and(|d| d.is_method)
                .then_some((bc.callee.module.as_str(), owner))
        }
        let class_of = |bc| class_of(project, bc);
        let is = |(module, owner): (&str, &str), fqn: &str| {
            fqn.strip_prefix(module)
                .and_then(|r| r.strip_prefix('.'))
                .is_some_and(|r| r == owner)
        };
        if callees.iter().filter_map(class_of).all(|c| is(c, class)) {
            return None;
        }
        let line = if call.recv_is_self {
            self.own_line()?
        } else {
            project.lineage(class, true)
        };
        let in_line = |c: (&str, &str)| line.mro.iter().any(|m| is(c, m));
        let receiver = callees
            .iter()
            .filter_map(class_of)
            .any(|c| !is(c, class) && in_line(c))
            .then(|| Arc::clone(&line));
        let mut providers: Vec<(u16, SharedLine)> = Vec::new();
        let receiver_param = match call.recv.as_ref().and_then(|r| r.param.as_ref()) {
            _ if call.recv_is_self => Some(0),
            Some(&(param, None)) => Some(param),
            _ => None,
        };
        if receiver_param.is_some() {
            for (i, bc) in callees.iter().enumerate() {
                if let Some(c) = class_of(bc).filter(|c| !in_line(*c))
                    && let Ok(i) = u16::try_from(i)
                {
                    providers.push((i, project.lineage(&format!("{}.{}", c.0, c.1), false)));
                }
            }
        }
        let declared = self.declared_method(call, callees);
        (receiver.is_some() || !providers.is_empty() || declared.is_some()).then(|| {
            Box::new(Dispatch {
                receiver,
                receiver_param: receiver_param
                    .filter(|_| !providers.is_empty())
                    .unwrap_or(0),
                providers,
                declared,
                recv_params: 0,
            })
        })
    }

    /// The site of `callees` an offloading API runs on a worker thread for `call`
    /// (`Runner::Worker`).
    fn worker_site(&mut self, bb: BlockId, callees: Vec<BoundCallee>, call: &Call<'_>) {
        if callees.is_empty() {
            return;
        }
        if let Some(events) = self.events.as_mut() {
            events
                .calls
                .push((self.pos.0, self.pos.1, self.sites.len()));
        }
        let site = self.site(bb, CallTarget::Project(callees), false, call.span);
        self.sites.push(CallSite {
            recv_is_self: call.recv_is_self,
            runner: Runner::Worker,
            ..site
        });
    }

    /// A callable a library API runs at the call, when it is a project function (or, but for an
    /// offloading API, a `functools.partial` of one, which passes the arguments it holds
    /// first): an offloading
    /// API's (`asyncio.to_thread(f, *args)`, `loop.run_in_executor(None, f, *args)`) on a worker
    /// thread, with the positional arguments after it (keyword arguments are left unbound: some
    /// APIs forward them, others take their own); a sync-facade API's
    /// (`AsyncSession.run_sync(f, *args)`) on the caller's thread, with the facade first and
    /// the arguments left unbound; and the one a builtin calls once per item (`map(f, xs)`,
    /// `sorted(xs, key=f)`) on the caller's thread, with the items as its arguments
    /// (`Self::passed_items`).
    fn handed_callable(&mut self, bb: BlockId, symbol: &str, call: &Call<'_>) {
        let facade = || {
            let k = usize::from(self.cx.catalog(symbol)?.sync_facade_arg?);
            if call.star_from.is_some_and(|s| usize::from(s) <= k) {
                return None;
            }
            Some(*call.args.get(k)?)
        };
        let per_item_call = || {
            crate::consumption::per_item_callable(symbol, call.args, call.kwargs, call.star_from)
        };
        let (callable, passed, runner, per_item) =
            match offloaded_callable(self.cx.kb, symbol, call.args, call.star_from) {
                Some((callable, first)) => (callable, &call.args[first..], Runner::Worker, None),
                None => match facade() {
                    Some(callable) => (callable, &[][..], Runner::SyncFacade, None),
                    None => match per_item_call() {
                        Some(p) => (p.callable, &[][..], Runner::PerItem, Some(p)),
                        None => return,
                    },
                },
            };
        let position = call
            .args
            .iter()
            .position(|a| *a == callable)
            .and_then(|i| u8::try_from(i).ok());
        // A worker's site is matched again to the call's own arguments after the callable
        // (`MUT202`, `heki_rules::mut_rules::site_call`), which the arguments a partial object
        // holds would shift: a partial is followed for the other APIs only.
        let partial = (runner != Runner::Worker)
            .then(|| self.partial(callable))
            .flatten();
        let target = partial.as_ref().map_or(callable, |p| p.callable);
        // A class a builtin is handed is constructed once per item: what the builtin returns
        // is not the new object, and that construction is not modeled. The facts of a lambda
        // that shares its name are those of every lambda of the name.
        if runner == Runner::PerItem
            && (self.class_called(target).is_some() || self.shares_its_name(target))
        {
            return;
        }
        let (callee, recv_var) = self.callable_callee(target);
        let Callee::Project(..) = &callee else {
            return;
        };
        let (recv, recv_is_self) = match recv_var {
            Some(r) => {
                let (origin, is_self) = self.receiver(r, bb);
                (Some(origin), is_self)
            }
            None => (None, false),
        };
        if self.references {
            // The API calls the callable, and a partial object the one it holds: their sites
            // are calls, not references (`Self::reference_sites`).
            self.handed.extend(call.dst.map(|dst| (dst, callable)));
            self.handed
                .extend(partial.iter().flat_map(|p| p.calls.iter().copied()));
        }
        let (held, kwargs, open) = partial.map_or((Vec::new(), Vec::new(), false), |p| {
            (p.args, p.kwargs, p.open)
        });
        let after = call.args.len() - passed.len();
        let items = per_item
            .as_ref()
            .map(|p| self.passed_items(bb, call, p, open));
        // Where the positions the call binds end: at the first argument `*` unpacks (in the
        // partial object or among the worker's arguments), or after the held arguments where
        // the API passes the facade or items of a size not known.
        let star_from = match runner {
            _ if open => Some(held.len()),
            Runner::Worker => call
                .star_from
                .filter(|_| !passed.is_empty())
                .map(|s| usize::from(s) - after + held.len()),
            Runner::SyncFacade if held.is_empty() => None,
            Runner::PerItem if items.as_ref().is_some_and(|i| i.positions.is_some()) => None,
            _ => Some(held.len()),
        }
        .map(|s| u8::try_from(s).unwrap_or(u8::MAX));
        let args: Vec<VarId> = if open {
            held
        } else {
            held.into_iter().chain(passed.iter().copied()).collect()
        };
        let handed = Call {
            dst: call.dst,
            value: Some(target),
            recv,
            recv_var,
            args: &args,
            kwargs: &kwargs,
            star_from,
            // A worker's arguments are the call's own after the callable; the others are the
            // arguments a partial holds.
            args_at: (runner == Runner::Worker).then_some(after),
            is_awaited: false,
            recv_is_self,
            constructs: None,
            per_item: items.as_ref(),
            unseen_keywords: self.calls_unseen_keywords(target),
            span: call.span,
        };
        let per_item = per_item.map(|p| {
            Box::new(PerItem {
                api: symbol.to_owned(),
                items: p.items,
            })
        });
        if let Some((site, _)) = self.call(bb, &callee, &handed) {
            self.sites[site].runner = runner;
            self.sites[site].handed_callable = position;
            self.sites[site].per_item = per_item;
        }
    }

    /// A project function handed to the library call `call` of `symbol` that calls it without
    /// awaiting what it returns (`OptInTables::unawaited_handoffs`): the callable the catalog
    /// names (`heki_kb::EffectiveEcosystemKb::unawaited_callable`), through a
    /// `functools.partial` object and a bound method. A class constructs an object instead.
    fn unawaited_handoff(&mut self, symbol: &str, call: &Call<'_>) {
        let catalog = self.cx.catalog(symbol);
        let Some(slot) = catalog.and_then(|entry| self.cx.kb.unawaited_callable(entry)) else {
            return;
        };
        let Some(callable) = resolver::passed_var(
            call.args,
            call.kwargs,
            call.star_from,
            (slot.arg, slot.param.as_deref()),
        ) else {
            return;
        };
        let target = self.partial(callable).map_or(callable, |p| p.callable);
        if self.class_called(target).is_some() {
            return;
        }
        let (callee, _) = self.callable_callee(target);
        let complete = self.callable_targets_complete(target, &callee);
        let Callee::Project(callees, _) = callee else {
            return;
        };
        self.unawaited.push(UnawaitedHandoff {
            span: call.span,
            api: symbol.to_owned(),
            callees,
            tests_result: slot.tests_result,
            hands_back: slot.hands_back,
            runs_on: slot
                .runs_on
                .filter(|_| !self.runs_in_process_pool(call, &slot)),
            complete,
        });
    }

    /// Whether the executor the call `call` names (`UnawaitedCall::executor_arg`) is a process
    /// pool: it runs the callable in another process, not on a thread of this one.
    fn runs_in_process_pool(&self, call: &Call<'_>, slot: &heki_kb::UnawaitedCall) -> bool {
        let Some(executor) = slot.executor_arg.and_then(|arg| {
            resolver::passed_var(call.args, call.kwargs, call.star_from, (Some(arg), None))
        }) else {
            return false;
        };
        match self.val(executor) {
            Some(Val::Inst(class, _)) => {
                class == PROCESS_POOL
                    || self
                        .cx
                        .project
                        .mro_of(class)
                        .iter()
                        .any(|c| c == PROCESS_POOL)
            }
            _ => false,
        }
    }

    /// What a builtin passes the callable it calls once per item (`Runner::PerItem`): items
    /// of the iterables, which it does not construct. After a partial object's `*` unpacking,
    /// where they land is not known.
    fn passed_items(
        &self,
        bb: BlockId,
        call: &Call<'_>,
        per_item: &PerItemCall,
        open: bool,
    ) -> PassedItems {
        let loops = self.loops_around(bb);
        let iterables: Vec<VarId> = match &per_item.items {
            Items::Args(positions) => positions
                .iter()
                .filter_map(|i| call.args.get(usize::from(*i)).copied())
                .collect(),
            Items::Receiver => call.recv_var.into_iter().collect(),
            Items::Unpacked => Vec::new(),
        };
        PassedItems {
            bit: per_item_bit(self.shape.loop_depth(bb)),
            passed: per_item.passed,
            positions: match (per_item.passed, &per_item.items) {
                (ItemArgs::Elements, _) | (_, Items::Unpacked) => None,
                (ItemArgs::AfterAccumulator, _) => Some(2),
                // `max(a, b, key=f)` passes one value at a time.
                (ItemArgs::OnePerIterable, _) => Some(iterables.len().max(1)),
            },
            // An item is not its iterable: neither a constant nor an input of the caller.
            items: iterables
                .iter()
                .filter(|_| !open)
                .map(|it| ArgBinding {
                    fact: None,
                    input: None,
                    ..self.binding(*it, loops)
                })
                .collect(),
        }
    }

    /// `var` is a lambda this function makes next to another one: every lambda of a function
    /// is `<function>.<locals>.<lambda>`, one key whose summary holds the facts of all of
    /// them.
    fn shares_its_name(&self, var: VarId) -> bool {
        let ir = self.cx.ir();
        let lambda = |kind: &InstKind| {
            matches!(kind, InstKind::MakeClosure { func_id: Some(id), .. }
                if ir.function(*id).name == "<lambda>")
        };
        let made = || {
            *self.lambdas.get_or_init(|| {
                self.f
                    .blocks
                    .iter()
                    .flat_map(|b| &b.insts)
                    .filter(|i| lambda(&i.kind))
                    .count()
            })
        };
        self.defs
            .get(&self.copied(var))
            .is_some_and(|i| lambda(&i.kind) && made() > 1)
    }

    /// The value `var` holds a copy of, through at most 16 copies: the walk runs at every call
    /// handing a callable, so a longer chain is not followed.
    fn copied(&self, var: VarId) -> VarId {
        let mut cur = var;
        for _ in 0..16 {
            match self.defs.get(&cur).map(|i| &i.kind) {
                Some(InstKind::Copy { src, .. }) => cur = *src,
                _ => break,
            }
        }
        cur
    }

    /// What calling `var` calls when it holds a `functools.partial` object ([`Partial::of`]).
    fn partial(&self, var: VarId) -> Option<Partial> {
        Partial::of(
            var,
            |v| self.copied(v),
            |object| {
                self.defs
                    .get(&object)
                    .and_then(|i| self.partial_call(&i.kind))
            },
        )
    }

    /// The parts of a `functools.partial(f, ...)` call ([`TypeCtx::partial_parts`]).
    #[allow(clippy::type_complexity)]
    fn partial_call(
        &self,
        kind: &'a InstKind,
    ) -> Option<(VarId, &'a [VarId], &'a [(Option<String>, VarId)], bool)> {
        self.cx.partial_parts(kind, &|v| self.val(v))
    }

    /// A call site in block `bb` with the caller's guards there.
    /// Nothing reads `v`: no instruction, phi, or terminator (a call made as a statement).
    fn discards(&self, v: VarId) -> bool {
        !self.uses.contains_key(&v) && !self.flow_uses().contains(&v)
    }

    /// The variables a phi or a block's terminator reads.
    fn flow_uses(&self) -> &FxHashSet<VarId> {
        self.flow_uses.get_or_init(|| {
            let mut out = FxHashSet::default();
            for bb in &self.f.blocks {
                out.extend(
                    bb.phis
                        .iter()
                        .flat_map(|phi| phi.incoming.iter().map(|(_, v)| *v)),
                );
                match bb.terminator {
                    Terminator::Branch { cond: v, .. }
                    | Terminator::Return(Some(v))
                    | Terminator::Raise(Some(v)) => {
                        out.insert(v);
                    }
                    _ => {}
                }
            }
            out
        })
    }

    /// The points that read each `self` attribute, for [`KeptFacts::must_read_self_attrs`]:
    /// per attribute, the loads whose value is read as a whole, and per constant key the item
    /// loads of a value every use of which is such an item load (the keys `reads_self_attrs`
    /// records).
    fn self_read_points(&self) -> BTreeMap<&'a str, SelfReadPoints<'a>> {
        let f = self.f;
        // The constant-key item loads of each value.
        let mut items: FxHashMap<VarId, Vec<(&'a str, Position)>> = FxHashMap::default();
        for bb in &f.blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                if let InstKind::SubscriptLoad {
                    obj,
                    key_const: Some(k),
                    ..
                } = &inst.kind
                {
                    items
                        .entry(*obj)
                        .or_default()
                        .push((k.as_str(), (bb.id, i)));
                }
            }
        }
        let mut out: BTreeMap<&'a str, SelfReadPoints<'a>> = BTreeMap::new();
        for bb in &f.blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                let InstKind::AttrLoad { dst, obj, attr } = &inst.kind else {
                    continue;
                };
                if attr == "__dict__" || !self.self_vars.contains(obj) {
                    continue;
                }
                let uses = self.uses.get(dst).map(Vec::as_slice).unwrap_or(&[]);
                let keyed = !uses.is_empty()
                    && !self.flow_uses().contains(dst)
                    && uses.iter().all(|u| {
                        matches!(u, InstKind::SubscriptLoad { obj, key_const: Some(_), .. } if obj == dst)
                    });
                let points = out.entry(attr.as_str()).or_default();
                if keyed {
                    for (k, at) in items.get(dst).into_iter().flatten() {
                        points.keyed.entry(k).or_default().push(*at);
                    }
                } else {
                    points.whole.push((bb.id, i));
                }
            }
        }
        out
    }

    fn site(&self, bb: BlockId, target: CallTarget, is_awaited: bool, span: Span) -> CallSite {
        let cx = self.guard_inputs();
        let (complete, on_every_return) = if self.opt_in.site_paths() {
            (
                self.targets_complete(),
                self.must_run.get(&self.pos).copied().unwrap_or_default(),
            )
        } else {
            (false, OnEveryReturn::default())
        };
        CallSite {
            caller: self.key.clone(),
            target,
            is_awaited,
            span,
            loop_depth: self.shape.loop_depth(bb),
            recv_is_self: false,
            drives_coroutine_manually: false,
            constructs: None,
            role: SiteRole::Call,
            returned: None,
            flow: None,
            guards: self.guards.facts_at(&cx, bb),
            once_per: self.guards.once_per_at(&cx, bb),
            once_loops: self.guards.once_loops_at(&cx, bb, self.loops_around(bb)),
            runner: Runner::Caller,
            binds_recv: false,
            handed_callable: None,
            per_item: None,
            dispatch: None,
            result_discarded: false,
            queues: None,
            result_held: false,
            is_base: false,
            is_read: false,
            arguments: None,
            state_tests: if self.opt_in.locks {
                self.state_tests().of_site(bb)
            } else {
                None
            },
            locks: crate::LockHeld::of(
                self.opt_in.exclusive && self.lock_held_at(self.pos),
                self.opt_in.guarded
                    && !self
                        .thread_lock_points()
                        .held_at(self.pos.0, self.pos.1)
                        .is_empty(),
            ),
            shield: self.shield_here(),
            complete,
            on_every_return,
            made_by: None,
            module_member: None,
        }
    }

    /// Whether the instruction at [`Self::pos`] runs shielded from cancellation (`EFF118`;
    /// unshielded while the rule does not run).
    fn shield_here(&self) -> crate::ShieldAt {
        if !self.opt_in.checkpoints {
            return crate::ShieldAt::Unshielded;
        }
        self.shields
            .get_or_init(|| {
                cancellation::ShieldPoints::of(self.cx, self.shape, &self.types, &self.live)
            })
            .at(self.pos.0, self.pos.1)
    }

    /// Whether an awaited catalog checkpoint `symbol` at [`Self::pos`] lets a cancellation in
    /// here: only while `EFF118` runs, outside a shield, and, for one doing no work of its own,
    /// where something runs after it ([`cancellation::REDELIVERIES`]).
    fn checkpoint_counts(&self, symbol: &str, is_awaited: bool) -> bool {
        is_awaited
            && self.opt_in.checkpoints
            && self.shield_here().is_unshielded()
            && !(cancellation::REDELIVERIES.contains(&symbol)
                && cancellation::runs_nothing_after(
                    self.shape,
                    self.redelivery_budget
                        .get_or_init(|| Budget::of_function("cancellation::redelivery", self.f)),
                    self.pos.0,
                    self.pos.1,
                ))
    }

    /// Whether the call at [`Self::pos`] has as its targets every implementation it can run
    /// (`CallSite::complete`).
    fn targets_complete(&self) -> bool {
        self.targets_complete_at(self.pos)
    }

    /// [`Self::targets_complete`] of the call at `(bb, index)`, with the types its block narrows
    /// (those its callee was resolved with); for a call of a held callable, those of the call
    /// it was resolved to ([`Self::held_calls_complete`]).
    fn targets_complete_at(&self, (bb, index): Position) -> bool {
        if let Some(&complete) = self.held_calls_complete.get(&(bb, index)) {
            return complete;
        }
        // A read that runs property getters ([`Self::property_read`]) runs the getter each class
        // the object may be finds for the name (more than CHA follows are none).
        self.f.block(bb).insts.get(index).is_some_and(|i| {
            self.cx
                .inst_targets_complete(&i.kind, |v| self.val_in(bb, v))
        })
    }

    /// Whether `v` is a phi, with its inputs pushed on `next`.
    fn phi_inputs(&self, v: VarId, next: &mut Vec<VarId>) -> bool {
        match self.guard_inputs().phi(v) {
            Some((_, phi)) => {
                next.extend(phi.incoming.iter().map(|(_, input)| *input));
                true
            }
            None => false,
        }
    }

    /// Whether `v` is one of the function's parameters.
    fn is_param(&self, v: VarId) -> bool {
        self.f.params.iter().any(|p| p.var == v)
    }

    /// [`Self::must_run`], [`Self::class_runs`], [`Self::takes_on_every_return`], and
    /// [`MustExec::never_returns`], from the path facts of the function with what the catalog
    /// and the project's declarations know: the members of modules and the context managers
    /// whose exit never suppresses an exception.
    fn path_facts(&mut self) {
        let f = self.f;
        let ir = self.cx.ir();
        let every = |a: Answer<Must>| a == Answer::Known(Must::Every);
        let owner = "heki_effect::summary::Extractor::path_facts";
        let read_points = if self.opt_in.kept_values {
            self.self_read_points()
        } else {
            BTreeMap::new()
        };
        let locks = self.opt_in.locks;
        let mut takes = FxHashSet::default();
        let (must_run, class_runs, never_returns, must_reads) =
            crate::with_paths(self.cx, f, &self.types, owner, |paths, _| {
                let mut must_run = FxHashMap::default();
                let mut class_runs: FxHashMap<FunctionId, OnEveryReturn> = FxHashMap::default();
                for bb in &f.blocks {
                    for (index, inst) in bb.insts.iter().enumerate() {
                        let at = (bb.id, index);
                        if locks
                            && may_take_lock(&inst.kind)
                            && every(paths.on_every_return(at, Progress::Called))
                        {
                            takes.insert(at);
                        }
                        let class_body = class_body_made(ir, &inst.kind);
                        if class_body.is_none() && !crate::anchors_call_site(&inst.kind) {
                            continue;
                        }
                        let called = paths.on_every_return(at, Progress::Called);
                        let run = OnEveryReturn {
                            called: every(called),
                            returned: every(paths.on_every_return(at, Progress::Returned)),
                            live: called.is_open() || called != Answer::Known(Must::Dead),
                        };
                        match class_body {
                            // A `class` statement in a `finally` body is made once per way out
                            // of its `try`: the body has run when one of them has.
                            Some(id) => {
                                let any = class_runs.entry(id).or_default();
                                any.called |= run.called;
                                any.returned |= run.returned;
                                any.live |= run.live;
                            }
                            None => {
                                must_run.insert(at, run);
                            }
                        }
                    }
                }
                let never_returns = f.blocks.first().is_some_and(|entry| {
                    paths.on_every_return((entry.id, 0), Progress::Called)
                        == Answer::Known(Must::NeverCompletes)
                });
                let mut must_reads = BTreeSet::new();
                for (attr, points) in &read_points {
                    let read = |at: &[Position]| {
                        paths.on_every_return_passes_one_of(at, Progress::Returned)
                            == Answer::Known(true)
                    };
                    if read(&points.whole) {
                        must_reads.insert(AttrKeyPath {
                            attr: (*attr).to_owned(),
                            const_key: None,
                        });
                        continue;
                    }
                    for (key, at) in &points.keyed {
                        let either: Vec<Position> =
                            points.whole.iter().chain(at).copied().collect();
                        if read(&either) {
                            must_reads.insert(AttrKeyPath {
                                attr: (*attr).to_owned(),
                                const_key: Some((*key).to_owned()),
                            });
                        }
                    }
                }
                (must_run, class_runs, never_returns, must_reads)
            });
        if !must_reads.is_empty() {
            self.summary.kept_mut().must_read_self_attrs = must_reads;
        }
        self.must_run = must_run;
        self.class_runs = class_runs;
        self.takes_on_every_return = takes;
        if never_returns && self.opt_in.must_effects {
            self.summary.must_exec = Some(Box::new(MustExec {
                never_returns,
                ..MustExec::default()
            }));
        }
    }

    /// The thread locks the function holds, by its own acquisitions and releases: the facts of
    /// its callees are not known yet ([`guarded::LockPoints::of`]).
    fn thread_lock_points(&self) -> &guarded::LockPoints {
        self.thread_locks.get_or_init(|| {
            guarded::LockPoints::of(self.cx, self.shape, &self.types, &self.live, &|_, _, _| {
                Vec::new()
            })
        })
    }

    /// `EFF117`: the thread locks the function returns holding, and its writes of shared state
    /// made holding a named one.
    fn guarded_facts(&mut self) {
        let points = self.thread_lock_points();
        if points.is_empty() {
            return;
        }
        let locked_writes: Vec<guarded::LockedWrite> =
            guarded::state_writes(self.cx, self.shape, &self.types, &self.live)
                .into_iter()
                .filter_map(|w| {
                    let lock = (points.held_at(w.at.0, w.at.1).iter())
                        .find(|k| k.name().is_some())?
                        .clone();
                    Some(guarded::LockedWrite {
                        state: w.state,
                        lock,
                        kind: w.kind,
                        span: w.span,
                    })
                })
                .collect();
        let facts = guarded::GuardFacts {
            returns_holding: points.returns_holding().clone(),
            locked_writes,
        };
        if !facts.is_empty() {
            self.summary.guarded = Some(Box::new(facts));
        }
    }

    /// Whether the function certainly holds a lock before the instruction at `at`: a `with` or
    /// an `acquire()` of a catalog lock (`locks::lock_steps`), computed once per function.
    fn lock_held_at(&self, at: Position) -> bool {
        self.lock_held
            .get_or_init(|| {
                let def = |v: VarId| self.defs.get(&v).copied();
                let steps =
                    // A name bound again still leaves its lock held: only whether one is matters.
                    locks::lock_steps(
                        self.cx,
                        self.f,
                        self.key,
                        &self.types,
                        &def,
                        &self.live,
                        &|_| Vec::new(),
                    );
                let mut out = FxHashSet::default();
                if steps.taken.is_empty() {
                    return out;
                }
                let entry = held::at_entry(self.shape, steps.taken.len(), |b, i| {
                    steps.changes.get(&(b, i))
                });
                let reached = (entry.into_iter().enumerate())
                    .filter_map(|(b, state)| Some((BlockId(b as u32), state?)));
                for (bb, mut state) in reached {
                    for i in 0..self.f.block(bb).insts.len() {
                        if !state.is_empty() {
                            out.insert((bb, i));
                        }
                        state.apply(steps.changes.get(&(bb, i)));
                    }
                }
                out
            })
            .contains(&at)
    }

    /// The allocation a call outside the project makes and the escapes of what a call is
    /// handed, before its callees are bound: a copy or a constructor call allocates its object,
    /// and a call heki cannot resolve may keep everything it is handed. A project call's are
    /// [`Self::project_alloc`] and [`Self::hand_over`].
    fn call_escapes(&mut self, callee: &Callee, call: &Call<'_>) {
        if !self.ownership {
            return;
        }
        match callee {
            Callee::External(symbol) if self.adds_to_container(symbol, call) => {}
            Callee::External(symbol) => {
                // A bare allocation runs no `__init__`: it stays new whatever one keeps.
                let bare = symbol.ends_with(".__new__");
                let new_instance = (call.constructs.as_deref()).is_some_and(|c| {
                    bare || matches!(self.constructor(c), Constructor::KeepsNothing { .. })
                });
                self.external_escapes(
                    symbol,
                    call.dst,
                    call.recv_var,
                    call.args,
                    call.kwargs,
                    new_instance,
                );
            }
            Callee::Project(..) => {}
            Callee::TooMany | Callee::Unresolved => {
                for v in call
                    .recv_var
                    .iter()
                    .chain(call.args)
                    .chain(call.kwargs.iter().map(|(_, v)| v))
                {
                    self.escape(*v, Escape::Definite);
                }
            }
        }
    }

    /// Whether the call of the library method `symbol` stores what it is handed in a builtin
    /// container that may be one a constructor made (`kids.append(part)`,
    /// [`stored_into_container`]), recording its escapes: the receiver's and each argument's
    /// [`Escape::Added`].
    fn adds_to_container(&mut self, symbol: &str, call: &Call<'_>) -> bool {
        let (Some(recv), Some(kind)) = (call.recv_var, stored_into_container(symbol)) else {
            return false;
        };
        let site = AddedBy::Call {
            symbol: symbol.to_owned(),
            recv,
            args: call.args.to_vec(),
            kwargs: call.kwargs.to_vec(),
            star: call.star_from.is_some(),
        };
        let Some(how) = (call.per_item.is_none() && call.args_at.is_some())
            .then(|| self.added_to(recv, (kind, site)))
            .flatten()
        else {
            return false;
        };
        // A keyword argument makes these methods raise before they store anything.
        let keywords = call.kwargs.iter().map(|(_, v)| v);
        for v in std::iter::once(&recv).chain(call.args).chain(keywords) {
            self.escape(*v, how.clone());
        }
        true
    }

    /// The allocation of a project call, its callees bound (`returns`: each with the parameters
    /// the call binds to values without elements): a constructor call allocates its object, a
    /// call handing back an argument as itself ([`Self::same_objects`]) gives that argument's
    /// object, and a project function returning an instance of a project class a new object
    /// when every candidate returns one.
    fn project_alloc(&mut self, call: &Call<'_>, returns: Vec<NewFrom>, inits: &[usize]) {
        if !self.ownership {
            return;
        }
        let Some(dst) = call.dst.filter(|_| self.track && call.per_item.is_none()) else {
            return;
        };
        if let Some(class) = &call.constructs {
            let hands = match self.constructor(class) {
                Constructor::KeepsNothing { .. } => false,
                Constructor::HandsToInit if !inits.is_empty() => true,
                Constructor::HandsToInit | Constructor::Unknown => return,
            };
            // The `__init__`s the call may run, each handed the object as its `self`.
            let site = u32::try_from(self.sites.len()).ok();
            let handed: Option<Vec<Escape>> = (inits.iter().filter(|_| hands))
                .map(|&init| Some(Escape::Initialized(site?, u16::try_from(init).ok()?)))
                .collect();
            let Some(handed) = handed else { return };
            let made = (NewKind::Own, Some(class.as_str()));
            self.rec.alloc(self.at(), dst, made, Vec::new());
            for how in handed {
                self.escape(dst, how);
            }
        } else if let Some(&arg) = self.same_objects.get(&dst) {
            self.rec.handed_back(dst, arg);
        } else if self.may_be_new(dst) {
            self.rec
                .alloc(self.at(), dst, (NewKind::Own, None), returns);
        }
    }

    /// The objects a call hands to the parameters of its project callee `decl` (the
    /// `callee`-th of the site the call adds). Each object `handed` to a parameter (bound to it
    /// exactly, or held by `*args`) is asked about for `ArgOrigin::new_object` and is kept only
    /// if the callee keeps that parameter; an argument after `*` unpacking, or one no parameter
    /// takes, may be kept. A generator or a
    /// coroutine that is created, not run, keeps its arguments. Only called when an ownership
    /// contract is declared.
    fn hand_over(
        &mut self,
        call: &Call<'_>,
        decl: &FunctionDecl,
        callee: usize,
        handed: &[(u8, VarId)],
    ) {
        let exact: FxHashSet<VarId> = handed.iter().map(|(_, v)| *v).collect();
        for v in call
            .args
            .iter()
            .chain(call.kwargs.iter().map(|(_, v)| v))
            .filter(|v| !exact.contains(v))
        {
            self.escape(*v, Escape::Definite);
        }
        let site = self.sites.len();
        let deferred = decl.is_generator || decl.is_async && !call.is_awaited;
        for &(pos, var) in handed {
            if self.track && self.trackable(var) {
                let query = self.rec.query(self.at(), var);
                self.pending_args
                    .push((site, callee, usize::from(pos), query));
            }
            let how = match (u32::try_from(site), u16::try_from(callee)) {
                (Ok(s), Ok(c)) if pos < 64 && !deferred => Escape::Handed(s, c, pos),
                _ => Escape::Definite,
            };
            self.escape(var, how);
        }
    }

    /// The escapes and the allocation of a call of a symbol outside the project (`symbol`):
    /// builtins that only inspect their arguments keep nothing, a copy keeps nothing of the
    /// object it copies and returns a new one, anything else may keep what it is handed.
    fn external_escapes(
        &mut self,
        symbol: &str,
        dst: Option<VarId>,
        recv: Option<VarId>,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        constructs: bool,
    ) {
        // The object an attribute write by name writes (the receiver of a bound
        // `obj.__setattr__`, the first argument of `object.__setattr__(obj, ...)` called on the
        // class), and the name.
        let bound = recv.filter(|r| !matches!(self.val(*r), Some(Val::Ref(_))));
        let written = match bound {
            Some(obj) => Some((obj, args.first())),
            None => args.first().map(|obj| (*obj, args.get(1))),
        };
        if let Some((obj, name)) = written {
            let name = || name.and_then(|a| self.shape.str_const(*a));
            let attr = match symbol {
                "builtins.setattr"
                | "builtins.object.__setattr__"
                | "builtins.delattr"
                | "builtins.object.__delattr__" => Some(name()),
                "builtins.vars" => Some(None),
                // Reading the namespace by name, as `obj.__dict__` does, or under a name heki
                // cannot read.
                "builtins.getattr" | "builtins.object.__getattribute__" => name()
                    .is_none_or(|a| NAMESPACE_ATTRS.contains(&a))
                    .then_some(None),
                _ => None,
            };
            if let Some(attr) = attr {
                self.attr_written(obj, attr);
            }
        }
        let method = symbol.rsplit('.').next().unwrap_or(symbol);
        let deep_kwarg = || {
            kwargs.iter().any(|(k, v)| {
                k.as_deref() == Some("deep") && self.shape.bool_const(*v) == Some(true)
            })
        };
        // The new object it returns, whether the receiver is only read, and how many leading
        // positional arguments are only read.
        let (alloc, recv_read, args_read) = match symbol {
            "copy.copy" | "dataclasses.replace" => (Some(NewKind::Copy), true, 1),
            "copy.deepcopy" => (Some(NewKind::Own), true, 1),
            _ if recv.is_some() && matches!(method, "copy" | "__copy__") => {
                (Some(NewKind::Copy), true, 0)
            }
            _ if recv.is_some() && method == "__deepcopy__" => (Some(NewKind::Own), true, 0),
            _ if recv.is_some() && method == "model_copy" => {
                let kind = if deep_kwarg() {
                    NewKind::Own
                } else {
                    NewKind::Copy
                };
                (Some(kind), true, 0)
            }
            // A project class without a project `__init__`: its fields keep the arguments.
            _ if constructs => (Some(NewKind::Own), true, 0),
            _ if inspecting_builtin(symbol, false) => (None, true, usize::MAX),
            "builtins.setattr" | "builtins.object.__setattr__" | "builtins.delattr" => {
                (None, true, 2)
            }
            _ => (None, false, 0),
        };
        if let (Some(kind), Some(dst)) = (alloc.filter(|_| self.track), dst) {
            self.rec.alloc(self.at(), dst, (kind, None), Vec::new());
        }
        if let Some(v) = recv.filter(|_| !recv_read) {
            self.escape(v, Escape::Definite);
        }
        for v in args
            .iter()
            .skip(args_read)
            .chain(kwargs.iter().map(|(_, v)| v))
        {
            self.escape(*v, Escape::Definite);
        }
    }

    fn external_call(
        &mut self,
        bb: BlockId,
        symbol: String,
        passed: Passed<'_>,
        is_awaited: bool,
        span: Span,
    ) {
        let args = passed.args;
        if symbol == "builtins.setattr" || symbol == "builtins.object.__setattr__" {
            let Some(obj) = args.first() else { return };
            let name = args.get(1).map_or(AttrName::Unknown, |a| self.key_name(*a));
            let owner = self.owner_of(*obj);
            self.write_named(owner, &name, false, WriteKind::SetAttr, span);
            return;
        }
        if symbol == "builtins.getattr" || symbol == "builtins.object.__getattribute__" {
            if let (Some(obj), Some(attr)) = (
                args.first(),
                args.get(1).and_then(|a| self.resolved_str(*a)),
            ) {
                let owner = self.owner_of(*obj);
                self.typed_read(owner, &attr, ReadKind::GetAttr, span);
            }
            return;
        }
        if symbol == "builtins.hasattr" {
            if let (Some(obj), Some(attr)) = (
                args.first(),
                args.get(1).and_then(|a| self.resolved_str(*a)),
            ) {
                let owner = self.owner_of(*obj);
                self.typed_read(owner, &attr, ReadKind::HasAttr, span);
            }
            return;
        }
        let Some(entry) = self.cx.catalog(&symbol) else {
            return;
        };
        if entry.excluded {
            return;
        }
        let case = entry.case_for(|i, k| {
            resolver::passed_arg(passed.args, passed.kwargs, passed.star_from, (i, k), |v| {
                self.arg_fact(v)
            })
        });
        // Stage 0-A filters: an awaited call never blocks the loop itself; loop-sensitive
        // effects (`DbQuery`, `UnpooledClientAlloc`, ...) are kept for EFF102 / EFF103. A schema
        // or setting statement is no data query, though running it still blocks.
        let ddl = self.runs_ddl(&symbol, args.first().copied());
        // A connection option does not wait on the database (`PRAGMA busy_timeout = 5000`).
        let setting = ddl && self.runs_connection_setting(&symbol, args.first().copied());
        // A wait on a future known to be done returns at once.
        let finished = entry.waits_until_done
            && passed.recv.is_some_and(|r| {
                self.guards
                    .finished_at(&self.guard_inputs(), bb, self.pos.1, r)
            });
        let mut effects: std::collections::BTreeSet<ExecEffectKind> = entry
            .effects_in(case)
            .iter()
            .copied()
            .filter(|e| !((is_awaited || finished) && BLOCKING_EFFECTS.contains(e)))
            .filter(|e| {
                *e != ExecEffectKind::Checkpoint || self.checkpoint_counts(&symbol, is_awaited)
            })
            .filter(|e| !(ddl && *e == ExecEffectKind::DbQuery))
            .filter(|e| !(setting && *e == ExecEffectKind::BlockingDb))
            .collect();
        // A `put` waits only on a bounded queue: the code may decide its capacity here, or leave
        // it to the project (what the callers bind to a parameter).
        let mut while_full = None;
        if entry.waits_while_full {
            let waits: BTreeSet<ExecEffectKind> = effects
                .iter()
                .copied()
                .filter(|e| BLOCKING_EFFECTS.contains(e))
                .collect();
            if !waits.is_empty() {
                let capacity = passed
                    .recv
                    .map_or_else(CapacityOf::unknown, |r| self.capacity_of(r).capacity);
                effects.retain(|e| !waits.contains(e));
                match capacity.decided() {
                    Some(Some(Answer::Known(Bound::Bounded))) => effects.extend(waits),
                    Some(_) => {}
                    None => {
                        while_full = Some(WhileFull {
                            effects: waits,
                            capacity,
                        });
                    }
                }
            }
        }
        if let Some(k) = entry.offloads_blocking_arg
            && let Some((p, None)) = args
                .get(usize::from(k))
                .and_then(|a| self.param_alias.get(a))
        {
            self.summary.offloads_blocking_params |= 1u64 << p;
        }
        if effects.is_empty() && entry.offloads_blocking_arg.is_none() && while_full.is_none() {
            return;
        }
        let occurrence = self.guards.occurrence(&self.guard_inputs(), bb);
        // Reports and contracts name the library API a catalog class's method is.
        let api = entry.api.clone().unwrap_or_else(|| symbol.clone());
        let flagged = match case {
            Some(_) => None,
            None => self.forwarded_flag(entry, passed),
        };
        let occurrence_of = |e: &ExecEffectKind| match flagged {
            Some((off, param)) if !off.contains(e) => {
                let mut needs_flag = occurrence.clone();
                needs_flag.inputs.insert(crate::InputGuard {
                    subject: GuardSubject { param, attr: None },
                    test: crate::GuardTest::Truthy,
                    holds: true,
                });
                needs_flag
            }
            _ => occurrence.clone(),
        };
        // A queue that only the function's parameter can make bounded: each call decides, so
        // the wait is an effect needing a bounded queue there. Any other is resolved over the
        // project before propagation (`capacity::ParamCapacities::realize`).
        let own_param = while_full.as_ref().and_then(|w| {
            let c = &w.capacity;
            match (c.fixed, c.params.iter().collect::<Vec<_>>().as_slice()) {
                (None | Some(Answer::Known(Bound::Unbounded)), [(f, p)]) if f == self.key => {
                    Some(*p)
                }
                _ => None,
            }
        });
        let guarded = own_param.and(while_full.as_ref()).map(|w| &w.effects);
        let by_value = match case {
            Some(_) => Vec::new(),
            None => self.value_guards(&symbol, entry, passed, &effects),
        };
        for e in effects.iter().chain(guarded.into_iter().flatten()) {
            let mut guards = crate::EffectGuards {
                bounded_queue: own_param.filter(|_| !effects.contains(e)),
                ..occurrence_of(e)
            };
            guards.inputs.extend(
                by_value
                    .iter()
                    .filter(|(of, _)| of == e)
                    .map(|(_, g)| g.clone()),
            );
            let had = !self.summary.direct_exec_effects.insert(*e);
            guards::add_occurrence(&mut self.summary.exec_guards, *e, had, &guards);
            let chain = vec![Arc::new(self.hop(api.clone(), span))];
            self.summary.exec_chains.insert(*e, chain);
        }
        for w in while_full.iter().filter(|_| own_param.is_none()) {
            for e in &w.effects {
                self.summary.capacity_waits.push(CapacityWait {
                    wait: WhileFull {
                        effects: BTreeSet::from([*e]),
                        capacity: w.capacity.clone(),
                    },
                    guards: occurrence_of(e),
                    hop: self.hop(api.clone(), span),
                });
            }
        }
        // The catalog describes the library's class: a project subclass may override the
        // method, and then the call runs no catalogued implementation.
        let unconditional = if self.opt_in.must_effects && self.targets_complete() {
            self.unconditional(entry, case, passed, &effects)
        } else {
            0
        };
        let must: Vec<ExecEffectKind> = effects
            .iter()
            .copied()
            .filter(|e| unconditional & crate::effect_bit(*e) != 0)
            .collect();
        let given = if self.opt_in.must_effects && self.targets_complete() && case.is_none() {
            self.given_waits(&symbol, entry, passed, &effects, unconditional)
        } else {
            Vec::new()
        };
        let hop =
            (!must.is_empty() || !given.is_empty()).then(|| Arc::new(self.hop(api.clone(), span)));
        let target = CallTarget::Layer2 {
            symbol: api,
            effects,
            offloads_blocking_arg: entry.offloads_blocking_arg,
            async_safe: entry.async_safe,
            unconditional,
        };
        let site = self.site(bb, target, is_awaited, span);
        if let Some(hop) = hop.filter(|_| site.made_on_every_return()) {
            let facts = self.summary.must_exec.get_or_insert_with(Box::default);
            for e in must {
                facts.effects.insert_new(e, || vec![Arc::clone(&hop)]);
            }
            for (e, param, test) in given {
                facts
                    .given
                    .insert(e, vec![(param, test)], vec![Arc::clone(&hop)], Vec::new());
            }
        }
        let queues = while_full.map(|w| {
            Box::new(SiteQueues {
                while_full: Some(w),
                handed: Vec::new(),
            })
        });
        self.sites.push(CallSite { queues, ..site });
    }

    /// The `effects` of a call of `entry` it has whatever the state and the arguments it does
    /// not show (`CallTarget::Layer2::unconditional`, for a call whose only target is the
    /// catalogued one): a database effect only where the entry
    /// `emits` it on every call, and none when a case of the entry tests an argument that may be
    /// computed from a parameter of the function (`resolver::from_parameter`), which its callers
    /// may pass a value selecting the case. An argument heki cannot evaluate otherwise is one the
    /// call waits on (R6).
    fn unconditional(
        &self,
        entry: &heki_kb::EcosystemEntry,
        case: Option<&heki_kb::ArgCase>,
        passed: Passed<'_>,
        effects: &BTreeSet<ExecEffectKind>,
    ) -> u16 {
        let from_caller = entry.cases.iter().any(|c| {
            resolver::passed_var(
                passed.args,
                passed.kwargs,
                passed.star_from,
                (c.arg, c.param.as_deref()),
            )
            .is_some_and(|v| {
                resolver::from_parameter(
                    &|d| self.defs.get(&d).copied(),
                    &|x, next| self.phi_inputs(x, next),
                    &|x| self.is_param(x),
                    v,
                )
            })
        });
        if from_caller {
            return 0;
        }
        effects
            .iter()
            .filter(|e| on_every_call(entry.emits_in(case), **e))
            .fold(0, |mask, e| mask | crate::effect_bit(*e))
    }

    /// The effects of a call of `entry` (`symbol`) that selects no case which the call has on
    /// every call when its callers pass a value that passes a test of the function's parameter
    /// (`MustExec::given`): the parameter is the argument every case tests, and a case, or
    /// passing none, has the effect on every call ([`on_every_call`]); or the call is made on the
    /// parameter, or on what catalog methods return of it, and a state of its library class has
    /// the effect on every call. Each with the parameter and the test; none of `unconditional`.
    fn given_waits(
        &self,
        symbol: &str,
        entry: &heki_kb::EcosystemEntry,
        passed: Passed<'_>,
        effects: &BTreeSet<ExecEffectKind>,
        unconditional: u16,
    ) -> Vec<(ExecEffectKind, u8, crate::ValueTest)> {
        let open = effects
            .iter()
            .copied()
            .filter(|e| unconditional & crate::effect_bit(*e) == 0);
        let argument = entry
            .case_slot()
            .and_then(|at| resolver::passed_var(passed.args, passed.kwargs, passed.star_from, at))
            .and_then(|v| self.own_param(v));
        let method = symbol.rsplit('.').next().unwrap_or(symbol);
        let receiver = passed
            .recv
            .filter(|_| entry.cases.is_empty())
            .and_then(|r| self.receiver_root(r, method));
        let mut out = Vec::new();
        for e in open {
            let waits = |case: Option<&heki_kb::ArgCase>| {
                entry.effects_in(case).contains(&e) && on_every_call(entry.emits_in(case), e)
            };
            let test = if let Some(param) = argument {
                let otherwise = waits(None);
                let mut cases: Vec<(heki_kb::ArgCase, bool)> = entry
                    .cases
                    .iter()
                    .map(|c| (case_tests(c), waits(Some(c))))
                    .collect();
                // Cases after the last one deciding otherwise than passing none give the same.
                while cases.last().is_some_and(|(_, w)| *w == otherwise) {
                    cases.pop();
                }
                let some = otherwise || cases.iter().any(|(_, w)| *w);
                some.then_some((param, crate::ValueTest::Cases { cases, otherwise }))
            } else if let Some((param, library, methods)) = &receiver {
                let kb = self.cx.kb;
                let states: Option<Vec<(String, bool)>> = kb
                    .states_of(library)
                    .map(|state| {
                        let waits = match state_call(kb, state, methods)? {
                            StateCall::Entry(s) => {
                                s.cases.is_empty()
                                    && s.effects.contains(&e)
                                    && on_every_call(s.emits, e)
                            }
                            StateCall::Uncatalogued => false,
                        };
                        Some((state.to_owned(), waits))
                    })
                    .collect();
                states
                    .filter(|s| s.iter().any(|(_, w)| *w))
                    .map(|s| (*param, crate::ValueTest::States(s)))
            } else {
                None
            };
            out.extend(test.map(|(param, test)| (e, param, test)));
        }
        out
    }

    /// A `[[tool.heki.ecosystem]]` entry naming the function declares what calling it does, as
    /// for an external function (the built-in catalog does not apply to project code): its
    /// `effects` run in the body, its `offloads_blocking_arg` (the receiver not counted) is a
    /// parameter the body offloads, and `excluded` / `async_safe` keep every effect / the
    /// blocking ones from its callers.
    fn apply_declaration(&mut self) {
        let fqn = self.key.fqn();
        let Some(entry) = self.cx.kb.declared(&fqn) else {
            return;
        };
        let hidden: BTreeSet<ExecEffectKind> = if entry.excluded {
            let loop_sensitive = [
                ExecEffectKind::UnpooledClientAlloc,
                ExecEffectKind::DbQuery,
                ExecEffectKind::QuadraticRealloc,
            ];
            BLOCKING_EFFECTS
                .iter()
                .copied()
                .chain(loop_sensitive)
                .collect()
        } else if entry.async_safe {
            BLOCKING_EFFECTS.iter().copied().collect()
        } else {
            BTreeSet::new()
        };
        for &e in &entry.effects {
            let had = !self.summary.direct_exec_effects.insert(e);
            guards::add_occurrence(
                &mut self.summary.exec_guards,
                e,
                had,
                &crate::EffectGuards::default(),
            );
            let chain = vec![Arc::new(self.hop(fqn.clone(), self.f.span))];
            self.summary.exec_chains.insert(e, chain);
        }
        if let Some(k) = entry.offloads_blocking_arg {
            let bound_first =
                self.f.owner_class_qualname.is_some() && !self.f.has_decorator(&["staticmethod"]);
            let p = u32::from(k) + u32::from(bound_first);
            if p < 64 {
                self.summary.offloads_blocking_params |= 1u64 << p;
            }
        }
        self.summary.hide_exec_effects(&hidden);
        self.summary.declared_hidden = hidden;
    }

    /// A flag of the catalog call that only `False` switches off (`Executor.shutdown(wait=...)`,
    /// `Lock.acquire(blocking=...)`), passed on from a parameter of this function: the effects
    /// besides the case's (`.0`) run only when the parameter (`.1`) is true, a guard its callers
    /// decide (`close(wait=False)`). The flags are read for their truth, as the APIs do.
    fn forwarded_flag<'e>(
        &self,
        entry: &'e heki_kb::EcosystemEntry,
        passed: Passed<'_>,
    ) -> Option<(&'e [ExecEffectKind], u8)> {
        entry.cases.iter().find_map(|c| {
            if c.equals != Some(heki_kb::ArgLiteral::Bool(false)) {
                return None;
            }
            let at = (c.arg, c.param.as_deref());
            let v = resolver::passed_var(passed.args, passed.kwargs, passed.star_from, at)?;
            let (p, None) = self.param_alias.get(&v)? else {
                return None;
            };
            Some((c.effects.as_deref()?, *p))
        })
    }

    /// The tests of a parameter of the function the catalog sets on `effects` of a call of
    /// `entry` (`symbol`) that selects no case ([`GuardTest::Value`]): the parameter is the
    /// argument every case of the entry tests, or the receiver the call (or the catalog calls
    /// whose results it is made on) is made on, and some case, or some state of the receiver's
    /// library class, lacks the effect. A caller decides each from the value it binds there.
    fn value_guards(
        &self,
        symbol: &str,
        entry: &heki_kb::EcosystemEntry,
        passed: Passed<'_>,
        effects: &BTreeSet<ExecEffectKind>,
    ) -> Vec<(ExecEffectKind, crate::InputGuard)> {
        let guard = |param, test| crate::InputGuard {
            subject: GuardSubject { param, attr: None },
            test: crate::GuardTest::Value(Box::new(test)),
            holds: true,
        };
        let mut out = Vec::new();
        if let Some(param) = entry
            .case_slot()
            .and_then(|at| resolver::passed_var(passed.args, passed.kwargs, passed.star_from, at))
            .and_then(|v| self.own_param(v))
        {
            for &e in effects {
                let mut cases: Vec<(heki_kb::ArgCase, bool)> = entry
                    .cases
                    .iter()
                    .map(|c| (case_tests(c), entry.effects_in(Some(c)).contains(&e)))
                    .collect();
                // Cases after the last one without the effect give what passing none does.
                while cases.last().is_some_and(|(_, keeps)| *keeps) {
                    cases.pop();
                }
                if !cases.is_empty() {
                    let test = crate::ValueTest::Cases {
                        cases,
                        otherwise: true,
                    };
                    out.push((e, guard(param, test)));
                }
            }
        }
        let method = symbol.rsplit('.').next().unwrap_or(symbol);
        if let Some((param, library, methods)) =
            passed.recv.and_then(|r| self.receiver_root(r, method))
        {
            let kb = self.cx.kb;
            for &e in effects {
                let states: Option<Vec<(String, bool)>> = kb
                    .states_of(&library)
                    .map(|state| {
                        Some((
                            state.to_owned(),
                            state_call(kb, state, &methods)?.may_have(e),
                        ))
                    })
                    .collect();
                if let Some(states) = states.filter(|s| s.iter().any(|(_, keeps)| !keeps)) {
                    out.push((e, guard(param, crate::ValueTest::States(states))));
                }
            }
        }
        out
    }

    /// The function's own parameter `var` is on every path, itself (through copies).
    fn own_param(&self, var: VarId) -> Option<u8> {
        let alias = self.param_alias.get(&var);
        match alias.or_else(|| self.param_alias.get(&self.shape.copy_root(var)))? {
            (p, None) => Some(*p),
            _ => None,
        }
    }

    /// The parameter a method call's receiver `recv` is made from by catalog method calls, the
    /// library class it is an instance of, and the methods called in turn from it, `method`
    /// last (`c.execute(sql).fetchone()`: `c`, `execute`, `fetchone`), when the class has
    /// states the catalog knows.
    fn receiver_root(&self, recv: VarId, method: &str) -> Option<(u8, String, Vec<String>)> {
        let mut methods = vec![method.to_owned()];
        let mut cur = recv;
        for _ in 0..MAX_RECEIVER_CALLS {
            if let Some(p) = self.own_param(cur) {
                let class = self.instance_of(cur)?;
                self.cx.kb.states_of(&class).next()?;
                methods.reverse();
                return Some((p, class, methods));
            }
            let root = self.shape.copy_root(cur);
            let Some(InstKind::MethodCall { recv, method, .. }) =
                self.defs.get(&root).map(|i| &i.kind)
            else {
                return None;
            };
            methods.push(method.clone());
            cur = *recv;
        }
        None
    }

    /// The function returns (through `step`) the result of the read-only producer `producer`
    /// it calls or reads at `span` (`MUT202` return flow; `produced` as the contracts naming
    /// the producer protect its result).
    fn seed_protected(
        &mut self,
        producer: String,
        produced: ContractSharing,
        step: ReturnStep,
        span: Span,
    ) {
        let chain = vec![Arc::new(self.hop(producer.clone(), span))];
        let produced = ProtectedReturn {
            producer,
            shared: produced.shared(),
            chain,
            declared: true,
            contracts: produced,
        };
        ProtectedReturn::add(&mut self.summary.returns_protected, produced, step);
    }
}

/// The method calls [`Extractor::receiver_root`] follows back from a receiver, at most.
const MAX_RECEIVER_CALLS: usize = 8;

/// Whether a catalog call that does its effects as `emits` says has effect `e` on every call:
/// a database effect only where it is done on every call, except that a SQLite connection that
/// may be in memory sends its statements without waiting; file system I/O only where the call
/// cannot do its work without it; nothing a configuration can switch off.
fn on_every_call(emits: heki_kb::Emits, e: ExecEffectKind) -> bool {
    match emits {
        heki_kb::Emits::Always => true,
        heki_kb::Emits::IfOnDisk => e != ExecEffectKind::BlockingDb,
        heki_kb::Emits::MayAvoidTheFileSystem => e != ExecEffectKind::BlockingFs,
        heki_kb::Emits::UnlessConfiguredOtherwise => false,
        _ => !matches!(e, ExecEffectKind::BlockingDb | ExecEffectKind::DbQuery),
    }
}

/// A case of a catalog entry with its tests only ([`crate::ValueTest::Cases`]).
fn case_tests(case: &heki_kb::ArgCase) -> heki_kb::ArgCase {
    heki_kb::ArgCase {
        effects: None,
        return_type: None,
        emits: None,
        ..case.clone()
    }
}

/// What calling the catalog methods `methods` in turn on an instance of the catalog class
/// `class` ends in, as the calls written on such an instance resolve: the entry of the last
/// one, or [`StateCall::Uncatalogued`] when a step names no entry or the class of no result.
/// `None` when a step's result depends on its arguments (a case changes the class).
pub(crate) fn state_call<'k>(
    kb: &'k heki_kb::EffectiveEcosystemKb,
    class: &str,
    methods: &[String],
) -> Option<StateCall<'k>> {
    let mut class = class.to_owned();
    let (last, steps) = methods.split_last()?;
    for m in steps {
        let Some(entry) = kb.lookup(&format!("{class}.{m}")) else {
            return Some(StateCall::Uncatalogued);
        };
        if entry.cases.iter().any(|c| c.return_type.is_some()) {
            return None;
        }
        if !entry.returns_receiver_in(None) {
            match entry.return_type_in(None) {
                Some(returned) => class = returned.to_owned(),
                None => return Some(StateCall::Uncatalogued),
            }
        }
    }
    Some(
        kb.lookup(&format!("{class}.{last}"))
            .map_or(StateCall::Uncatalogued, StateCall::Entry),
    )
}

/// [`state_call`].
pub(crate) enum StateCall<'k> {
    Entry(&'k heki_kb::EcosystemEntry),
    /// The calls run nothing the catalog knows of.
    Uncatalogued,
}

impl StateCall<'_> {
    /// Whether the call may have effect `e`, in some case.
    pub(crate) fn may_have(&self, e: ExecEffectKind) -> bool {
        match self {
            Self::Entry(entry) => {
                entry.effects.contains(&e)
                    || entry
                        .cases
                        .iter()
                        .any(|c| entry.effects_in(Some(c)).contains(&e))
            }
            Self::Uncatalogued => false,
        }
    }
}

/// The arguments a call passes, as written.
#[derive(Clone, Copy)]
struct Passed<'c> {
    args: &'c [VarId],
    kwargs: &'c [(Option<String>, VarId)],
    /// `InstKind::Call::star_from`.
    star_from: Option<u8>,
    /// The bound receiver of a method call.
    recv: Option<VarId>,
}

impl Passed<'_> {
    const NOTHING: Passed<'static> = Passed {
        args: &[],
        kwargs: &[],
        star_from: None,
        recv: None,
    };
}

/// One call instruction as [`Extractor::call`] sees it.
struct Call<'c> {
    /// The variable the call defines: none for the getter a call of a property runs
    /// (`obj.prop(...)`), whose value only that call uses.
    dst: Option<VarId>,
    /// The called value (`f` of `f(...)`), when it is a variable.
    value: Option<VarId>,
    /// The origin of a bound receiver (`obj` of `obj.m(...)`).
    recv: Option<ArgOrigin>,
    /// The variable holding the bound receiver.
    recv_var: Option<VarId>,
    args: &'c [VarId],
    kwargs: &'c [(Option<String>, VarId)],
    /// `InstKind::Call::star_from`.
    star_from: Option<u8>,
    /// Where `args` start among the instruction's arguments, whose `*` unpacking marks them
    /// (`IrFunction::is_starred` of `dst`); `None` for arguments a partial object holds, which
    /// are never unpacked.
    args_at: Option<usize>,
    is_awaited: bool,
    recv_is_self: bool,
    /// The project class the call constructs (`CallSite::constructs`).
    constructs: Option<String>,
    /// A builtin calls the callees once per item (`Runner::PerItem`), passing them these
    /// items: `dst` holds what the builtin returns, never what they return.
    per_item: Option<&'c PassedItems>,
    /// The call passes keywords the code does not show, which may bind any parameter: those a
    /// `functools.partial` object of a class attribute holds
    /// ([`TypeCtx::calls_partial_with_keywords`]).
    unseen_keywords: bool,
    span: Span,
}

impl<'c> Call<'c> {
    fn passed(&self) -> Passed<'c> {
        Passed {
            args: self.args,
            kwargs: self.kwargs,
            star_from: self.star_from,
            recv: self.recv_var,
        }
    }
}

/// How a call binds the parameters of one callee ([`Extractor::bindings`]), as Python binds
/// them.
struct Bindings<'c> {
    /// Each positional argument with its slot ([`Extractor::positional_slots`]): the parameter
    /// it binds, if any, is [`FunctionDecl::positional_param`].
    slots: Vec<(usize, VarId)>,
    /// Each keyword the call passes by name ([`Keywords`]) with the parameter it binds
    /// ([`FunctionDecl::keyword_param`]); one binding none lands in `**kwargs`, or the call
    /// raises `TypeError`.
    keywords: Vec<(Cow<'c, str>, VarId, Option<usize>)>,
    /// `*` unpacking, or `**` unpacking of anything but a constant dict display, may pass any
    /// parameter an argument binds.
    unpacks: bool,
    /// Two keywords share a name: the call raises `TypeError`.
    repeated: bool,
}

impl Bindings<'_> {
    /// Whether the call raises `TypeError` instead of running `decl` (whose first positional
    /// argument binds parameter `offset`, the receiver taking the slots before it): two keywords
    /// share a name, a keyword binds a parameter a position binds too, a keyword binds no
    /// parameter and `decl` takes no `**kwargs`, a position is past the positional parameters
    /// and `decl` takes no `*args`, or a parameter without a default is left unbound where no
    /// unpacking may bind it ([`Self::unpacks`]). The work is that of the call's arguments: the
    /// parameters without a default are counted in the declaration
    /// ([`FunctionDecl::required_positional`]).
    fn raises(&self, decl: &FunctionDecl, offset: usize) -> bool {
        let passed = offset + self.slots.len();
        let by_position = |p: usize| decl.positional_param(p).is_some() && p < passed;
        let doubled = self.repeated
            || self
                .keywords
                .iter()
                .any(|&(_, _, p)| p.map_or(!decl.has_kwargs, by_position));
        let past_positionals = passed
            .checked_sub(1)
            .is_some_and(|last| decl.positional_param(last).is_none());
        if doubled || (past_positionals && !decl.has_varargs) {
            return true;
        }
        let required = usize::from(decl.required_positional);
        let first_keyword_only = usize::from(decl.positional_params);
        let keyword_only = |p: usize| {
            p.checked_sub(first_keyword_only)
                .is_some_and(|j| j < 8 && decl.required_keyword_only >> j & 1 == 1)
        };
        // The receiver and the positions bind the leading parameters, the keywords the others.
        let (mut bound, mut bound_keyword_only) = (passed.min(required), 0);
        for p in self.keywords.iter().filter_map(|k| k.2) {
            bound += usize::from(p < required);
            bound_keyword_only += usize::from(keyword_only(p));
        }
        !self.unpacks
            && (bound < required
                || bound_keyword_only < decl.required_keyword_only.count_ones() as usize)
    }

    /// The parameters of `decl` that take their literal defaults: no position or keyword binds
    /// them, and no unpacking may ([`Self::unpacks`]).
    fn defaults<'d>(
        &self,
        call: &Call<'_>,
        decl: &'d FunctionDecl,
        offset: usize,
    ) -> impl Iterator<Item = (usize, &'d heki_kb::ArgFact)> {
        let unpacks = self.unpacks;
        let passed =
            offset + call.args.len() + call.per_item.and_then(|p| p.positions).unwrap_or(0);
        let by_position = move |p: usize| decl.positional_param(p).is_some() && p < passed;
        let by_keyword: Vec<usize> = self.keywords.iter().filter_map(|k| k.2).collect();
        decl.defaults
            .iter()
            .filter(move |_| !unpacks)
            .map(|(p, c)| (usize::from(*p), c))
            .filter(move |(p, _)| !by_position(*p) && !by_keyword.contains(p))
    }
}

/// The parameters a call binds to a value without elements ([`crate::ConstArg::has_no_elements`]),
/// by what it knows of each bound argument: a literal default, or a constant the call passes.
fn params_without_elements(params: &[ArgOrigin]) -> u64 {
    (params.iter().enumerate().take(64))
        .filter(|(_, o)| {
            (o.binding.as_ref().and_then(ArgBinding::constant)).is_some_and(|c| c.has_no_elements())
        })
        .fold(0, |mask, (k, _)| mask | 1u64 << k)
}

/// What a builtin calling a callable once per item passes it ([`Extractor::passed_items`]).
struct PassedItems {
    /// The builtin's loop over the items (`guards::per_item_bit`).
    bit: u64,
    passed: ItemArgs,
    /// How many positional arguments each call passes after the held ones, when the builtin
    /// fixes it: the parameters after them get their defaults.
    positions: Option<usize>,
    /// What is known of an item of each iterable.
    items: Vec<ArgBinding>,
}

impl PassedItems {
    /// Bind the items to the parameters below `positional` from `first` on (after the receiver
    /// and the arguments a partial object holds) that no keyword binds. The items and the objects bound before them
    /// are the same objects through the builtin's loop (`ArgBinding::stays_in_loops`), as the
    /// objects a `for` loop takes from its iterable are.
    fn bind(&self, params: &mut Vec<ArgOrigin>, first: usize, positional: usize) {
        for b in params.iter_mut().filter_map(|o| o.binding.as_mut()) {
            b.stays_in_loops |= self.bit;
        }
        let slots: Vec<(usize, &ArgBinding)> = match self.passed {
            ItemArgs::OnePerIterable => self
                .items
                .iter()
                .enumerate()
                .map(|(i, b)| (first + i, b))
                .collect(),
            ItemArgs::Elements => self
                .items
                .first()
                .map(|b| (first..positional).map(|slot| (slot, b)).collect())
                .unwrap_or_default(),
            ItemArgs::AfterAccumulator => self
                .items
                .first()
                .map(|b| (first + 1, b))
                .into_iter()
                .collect(),
        };
        // A parameter a keyword binds already takes no item (Python raises if one also comes
        // by position).
        let free = |slot: usize, params: &[ArgOrigin]| {
            slot < positional && params.get(slot).is_none_or(|o| o.binding.is_none())
        };
        let slots: Vec<(usize, &ArgBinding)> = slots
            .into_iter()
            .filter(|(slot, _)| free(*slot, params))
            .collect();
        for (slot, b) in slots {
            if params.len() <= slot {
                params.resize(slot + 1, ArgOrigin::default());
            }
            params[slot].binding = Some(ArgBinding {
                stays_in_loops: b.stays_in_loops | self.bit,
                ..b.clone()
            });
        }
    }
}

/// Another site of the callees of the call site `site`, for a use of the object the call
/// returns (`SiteRole`): at `span` and loop depth `depth`, awaited and driven as `flags` says
/// (the use is no call of the callees themselves).
fn role_site(
    site: &CallSite,
    role: SiteRole,
    span: Span,
    depth: u16,
    flags: Option<(bool, bool)>,
) -> CallSite {
    let (is_awaited, drives) = flags.unwrap_or((false, false));
    CallSite {
        caller: site.caller.clone(),
        target: site.target.clone(),
        is_awaited,
        span,
        loop_depth: depth,
        recv_is_self: site.recv_is_self,
        drives_coroutine_manually: drives,
        constructs: None,
        role,
        returned: None,
        flow: None,
        // The callees' facts run only where the object was made: under the guards of its call.
        // A lazy initialization of the caller around the call bounds them per call, not per
        // iteration of a loop around the other use.
        guards: site.guards.clone(),
        once_per: site.once_per,
        once_loops: 0,
        runner: site.runner,
        binds_recv: site.binds_recv,
        handed_callable: site.handed_callable,
        per_item: site.per_item.clone(),
        dispatch: site.dispatch.clone(),
        result_discarded: false,
        queues: site.queues.clone(),
        result_held: false,
        is_base: false,
        is_read: false,
        arguments: None,
        state_tests: site.state_tests.clone(),
        locks: site.locks,
        shield: site.shield,
        complete: false,
        on_every_return: crate::OnEveryReturn::default(),
        made_by: Some(site.span),
        module_member: None,
    }
}

/// Builtins that only inspect their arguments: calling one keeps none of them, and what it
/// returns holds none of them.
const INSPECTING_BUILTINS: &[&str] = &[
    "len",
    "isinstance",
    "issubclass",
    "id",
    "type",
    "hasattr",
    "vars",
    "callable",
    "repr",
    "str",
    "hash",
    "bool",
    "print",
    "format",
];

/// Builtins that read an attribute of their first argument: calling one keeps none of its
/// arguments, but what it returns may hold one (a bound method holds its object, and `getattr`
/// returns its default as it is).
const ATTRIBUTE_READING_BUILTINS: &[&str] = &["getattr", "object.__getattribute__"];

/// Whether calling the builtin `symbol` keeps none of its arguments ([`INSPECTING_BUILTINS`],
/// [`ATTRIBUTE_READING_BUILTINS`]); with `detached`, also whether what it returns holds none of
/// them.
fn inspecting_builtin(symbol: &str, detached: bool) -> bool {
    symbol.strip_prefix("builtins.").is_some_and(|n| {
        INSPECTING_BUILTINS.contains(&n) || !detached && ATTRIBUTE_READING_BUILTINS.contains(&n)
    })
}

/// Calls that only name what they are handed: typing forms, `super`, and the call form of an
/// `except` clause. (`typing.cast` hands its value back: [`ownership::handed_back`].)
const NAMING_CALLS: &[&str] = &[
    "builtins.super",
    "contextlib.suppress",
    "typing.NewType",
    "typing.TypeVar",
    "typing_extensions.NewType",
    "typing_extensions.TypeVar",
];

/// Whether calling `callee` hands none of its arguments on: a builtin that only inspects them
/// ([`inspecting_builtin`]), or a call of [`NAMING_CALLS`].
fn names_only(callee: &Callee) -> bool {
    let Callee::External(symbol) = callee else {
        return false;
    };
    NAMING_CALLS.contains(&symbol.as_str()) || inspecting_builtin(symbol, false)
}

/// The construction of a write (`rebinds`: it rebinds an attribute) of an object the answer
/// describes: a shallow copy's containers are the original's.
fn construction_of(answer: &NewArg, rebinds: bool) -> Construction {
    match answer {
        NewArg::New { kind, .. } if !rebinds && *kind == NewKind::Copy => Construction::No,
        NewArg::New { cond, .. } if cond.is_empty() => Construction::Yes,
        NewArg::New { cond, .. } => Construction::Pending {
            cond: cond.clone(),
            own: !rebinds,
        },
        NewArg::Param { param, cond } => Construction::Param {
            param: *param,
            own: !rebinds,
            cond: cond.clone(),
        },
        NewArg::Old => Construction::No,
    }
}

/// Give `dst` the alias shared by every aliased phi input (if they all agree).
/// A step of [`Extractor::element_paths`]: the object a value holds is below another value's
/// object, or is one of its items (under the constant key the index names; any item for
/// `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ItemStep {
    Below,
    Items(Option<VarId>),
}

/// The walks of [`Extractor::element_paths`] or [`Extractor::entry_params`], one per narrowing
/// scope ([`crate::infer::NarrowedVals::scope`]): their steps read the values of the block being
/// read (the classes of an attribute's object, the views of a receiver's methods), so an answer
/// holds only in the blocks where the checks give every value the same types.
struct ElementWalks<S>(Vec<Closure<(VarId, ItemStep), S>>);

impl<S: Join + Default> ElementWalks<S> {
    fn new(scopes: usize) -> Self {
        Self((0..scopes).map(|_| Closure::new()).collect())
    }

    /// The walk of the narrowing scope `scope`.
    fn of(&self, scope: usize) -> &Closure<(VarId, ItemStep), S> {
        &self.0[scope]
    }
}

/// What a function may rebind on some object: the attribute names it stores or deletes (also
/// through `setattr` / `delattr` and `__dict__`), `None` when it may set one it does not name;
/// and the constant keys it stores or deletes, `None` when it stores under a key it does not name
/// (`d[k] = v`, `d.update(...)`). An attribute or item of a shallow copy that the function may
/// rebind may hold a value of its own rather than the original's (`param_copy_alias`), and so
/// may an item of a parameter ([`Extractor::keeps_items`]).
struct Rebound<'f> {
    attrs: Option<FxHashSet<&'f str>>,
    keys: Option<FxHashSet<&'f str>>,
}

impl<'f> Rebound<'f> {
    /// The names `f` may rebind, `namespace` telling the namespaces its instructions load
    /// ([`resolver::namespace_load`]).
    fn of(f: &'f IrFunction, namespace: impl Fn(&InstKind) -> Option<(VarId, VarId)>) -> Self {
        let insts = || f.blocks.iter().flat_map(|b| &b.insts);
        let mut strings: FxHashMap<VarId, &'f str> = FxHashMap::default();
        let mut dicts: FxHashSet<VarId> = FxHashSet::default();
        for inst in insts() {
            if let InstKind::Const {
                dst,
                literal: ConstLiteral::Str(text),
            } = &inst.kind
            {
                strings.insert(*dst, text.as_str());
            } else if let Some((dst, _)) = namespace(&inst.kind) {
                dicts.insert(dst);
            }
        }
        let mut attrs = Some(FxHashSet::default());
        let mut keys = Some(FxHashSet::default());
        let add = |set: &mut Option<FxHashSet<&'f str>>, name: Option<&'f str>| match (name, set) {
            (Some(n), Some(named)) => {
                named.insert(n);
            }
            (_, set) => *set = None,
        };
        for inst in insts() {
            match &inst.kind {
                InstKind::AttrStore { attr, .. } | InstKind::AttrDelete { attr, .. } => {
                    add(&mut attrs, Some(attr.as_str()));
                }
                InstKind::SubscriptStore { obj, key_const, .. }
                | InstKind::SubscriptDelete { obj, key_const, .. } => {
                    let key = key_const.as_deref();
                    if dicts.contains(obj) {
                        add(&mut attrs, key);
                    }
                    add(&mut keys, key);
                }
                InstKind::Call { callee, args, .. }
                    if matches!(
                        callee.as_str(),
                        "setattr" | "delattr" | "object.__setattr__" | "object.__delattr__"
                    ) =>
                {
                    add(
                        &mut attrs,
                        args.get(1).and_then(|a| strings.get(a)).copied(),
                    );
                }
                InstKind::MethodCall { recv, method, .. }
                    if matches!(method.as_str(), "update" | "setdefault" | "__setitem__") =>
                {
                    if dicts.contains(recv) {
                        attrs = None;
                    }
                    keys = None;
                }
                InstKind::BinOp {
                    lhs,
                    op: BinOpKind::BitOr,
                    in_place: true,
                    ..
                } if dicts.contains(lhs) => attrs = None,
                _ => {}
            }
        }
        Self { attrs, keys }
    }

    /// Whether the function leaves attribute `attr` of every object it holds as it was.
    fn keeps_attr(&self, attr: &str) -> bool {
        self.attrs.as_ref().is_some_and(|a| !a.contains(attr))
    }

    /// Whether the function leaves the item under `key` of every object it holds as it was.
    fn keeps_key(&self, key: &str) -> bool {
        self.keys.as_ref().is_some_and(|k| !k.contains(key))
    }
}

fn merge_phi<T: Clone + PartialEq>(
    map: &mut FxHashMap<VarId, T>,
    dst: VarId,
    inputs: &[VarId],
    conv: impl Fn(&T) -> T,
) {
    if map.contains_key(&dst) {
        return;
    }
    let mut found: Option<T> = None;
    for v in inputs {
        match (map.get(v), &found) {
            (Some(a), None) => found = Some(conv(a)),
            (Some(a), Some(f)) if conv(a) != *f => return,
            (None, _) => return,
            _ => {}
        }
    }
    if let Some(a) = found {
        map.insert(dst, a);
    }
}

/// [`merge_phi`] for `obj.<attr>` aliases: the inputs must name one class attribute; the
/// objects' parameters and origins (`Reach`) are united (the merged object may be any of them). Inputs are inserted
/// with their final parameters (`may_params` is complete before the alias pass), so the first
/// insertion is final too.
fn merge_obj_attr_phi(map: &mut FxHashMap<VarId, ObjAttr>, dst: VarId, inputs: &[VarId]) {
    if map.contains_key(&dst) {
        return;
    }
    if let Some(a) = joined_obj_attr(inputs.iter().map(|v| map.get(v))) {
        map.insert(dst, a);
    }
}

/// The `obj.<attr>` alias of a value that is one of `inputs` ([`merge_obj_attr_phi`]): `None`
/// when one has none, or when they name different class attributes.
fn joined_obj_attr<'m>(inputs: impl IntoIterator<Item = Option<&'m ObjAttr>>) -> Option<ObjAttr> {
    let mut found: Option<ObjAttr> = None;
    for a in inputs {
        let a = a?;
        match &mut found {
            None => {
                found = Some(ObjAttr {
                    direct: false,
                    ..a.clone()
                });
            }
            Some(f) => {
                if f.classes != a.classes || f.attr != a.attr {
                    return None;
                }
                f.obj_params |= a.obj_params;
                f.fresh &= a.fresh;
                f.reach = f.reach.join(a.reach);
                if f.holder != a.holder {
                    f.holder = None;
                }
            }
        }
    }
    found
}

/// The object every input is the namespace of (`obj.__dict__`), when all are one of the same
/// classes; `None` when one is no namespace.
fn joined_owner<'m>(inputs: impl IntoIterator<Item = Option<&'m Owner>>) -> Option<Owner> {
    let mut found: Option<Owner> = None;
    for o in inputs {
        let o = o?;
        match &mut found {
            None => found = Some(o.clone()),
            Some(f) => {
                if f.typed_classes != o.typed_classes || f.unknown_class != o.unknown_class {
                    return None;
                }
                f.reach = f.reach.join(o.reach);
            }
        }
    }
    found
}

/// The entry alias of a value that is one of `inputs`: `None` when one has none, or when they
/// name entries of different class attributes (of differing keys: an entry heki cannot name);
/// the objects' parameters are united, as in [`merge_obj_attr_phi`].
fn joined_item<'m>(inputs: impl IntoIterator<Item = Option<&'m ItemOf>>) -> Option<ItemOf> {
    let mut found: Option<ItemOf> = None;
    for a in inputs {
        let a = a?;
        match &mut found {
            None => found = Some(a.clone()),
            Some(f) => {
                if f.classes != a.classes || f.attr != a.attr {
                    return None;
                }
                if f.key != a.key {
                    f.key = None;
                }
                f.obj_params |= a.obj_params;
                f.fresh &= a.fresh;
                f.reach = f.reach.join(a.reach);
                if f.holder != a.holder {
                    f.holder = None;
                }
            }
        }
    }
    found
}

/// Where an object that outlives the call is held ([`Extractor::global_roots`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Root<'a> {
    /// A module global, by name.
    Global(&'a str),
    /// The closure cell of an enclosing function's variable, by name.
    Cell(&'a str),
}

/// What a function establishes of the class of a value ([`Extractor::built`]), from the most
/// to the least.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Built {
    /// A builtin value of its exact class, never a subclass's, whose items (if any) are such
    /// values too and never change: a constant, a string, a number, a `bool`, a tuple of them.
    Atomic,
    /// A builtin value of its exact class whose items heki does not establish: a list, set, or
    /// dict display, a new container a builtin returns.
    Exact,
    /// A value whose class heki does not establish.
    Unknown,
}

impl Built {
    /// The fact [`Extractor::established_values`] keeps (no fact for `Atomic`, the least).
    fn fact(self) -> Option<(bool, bool)> {
        match self {
            Self::Atomic => None,
            Self::Exact => Some((false, true)),
            Self::Unknown => Some((true, true)),
        }
    }

    fn of_fact(fact: Option<&(bool, bool)>) -> Self {
        match fact {
            None | Some((false, false)) => Self::Atomic,
            Some((false, true)) => Self::Exact,
            Some((true, _)) => Self::Unknown,
        }
    }
}

/// How an instruction's result gets its [`Built`] from the least established of its operands.
#[derive(Debug, Clone, Copy)]
enum BuiltRule {
    /// Whatever its operands are.
    Is(Built),
    /// As its operands: a copy, a phi, a builtin operator (whose result is of a builtin class
    /// when both operands are builtin values of their exact classes).
    Joined,
    /// A tuple display: of its exact class, holding its elements.
    Tuple,
    /// A string or a `bool` when the operands are builtin values of their exact classes.
    OfExact,
    /// A builtin value whose items are established when the operands' are.
    OfAtomic,
}

impl BuiltRule {
    fn apply(self, least: Built) -> Built {
        match self {
            Self::Is(built) => built,
            Self::Joined => least,
            Self::Tuple => least.min(Built::Exact),
            Self::OfExact if least <= Built::Exact => Built::Atomic,
            Self::OfAtomic if least == Built::Atomic => Built::Atomic,
            Self::OfExact | Self::OfAtomic => Built::Unknown,
        }
    }
}

/// The attributes whose store or deletion replaces an object's whole namespace or its class.
pub(crate) const NAMESPACE_ATTRS: &[&str] = &["__dict__", "__class__"];

/// A store into a container that may be one a constructor made ([`Escape::Added`]).
enum AddedBy {
    /// A call of the library method `symbol` storing what it is handed in its receiver.
    Call {
        symbol: String,
        recv: VarId,
        args: Vec<VarId>,
        kwargs: Vec<(Option<String>, VarId)>,
        star: bool,
    },
    /// An item store, `obj[key] = value`.
    Item { obj: VarId, value: VarId },
}

/// The kind of the builtin container whose method `symbol` stores what it is handed in it
/// (`ownership::STORING_METHODS`: `list.append`, `set.add`, `dict.setdefault`, ...).
fn stored_into_container(symbol: &str) -> Option<ContainerKind> {
    let (class, method) = symbol.rsplit_once('.')?;
    ownership::STORING_METHODS
        .contains(&method)
        .then(|| ContainerKind::of_class(class))
        .flatten()
}

/// The operands of a call or method call instruction ([`call_operands`]).
struct CallOperands<'i> {
    /// A method call's receiver and method.
    method: Option<(VarId, &'i str)>,
    /// What a plain call calls (`None` also for a comprehension's own scope).
    callee_var: Option<VarId>,
    args: &'i [VarId],
    kwargs: &'i [(Option<String>, VarId)],
    /// Some arguments are unpacked (`*args`, `**kwargs`).
    star: bool,
}

fn call_operands(kind: &InstKind) -> Option<CallOperands<'_>> {
    match kind {
        InstKind::Call {
            callee_var,
            args,
            kwargs,
            star_from,
            ..
        } => Some(CallOperands {
            method: None,
            callee_var: *callee_var,
            args,
            kwargs,
            star: star_from.is_some(),
        }),
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            star_from,
            ..
        } => Some(CallOperands {
            method: Some((*recv, method)),
            callee_var: None,
            args,
            kwargs,
            star: star_from.is_some(),
        }),
        _ => None,
    }
}

/// The step by which an instruction's result is reached from an operand's object (`dst`, `src`):
/// a copy, an attribute or item of it, an iterator over it or an item it yields, an element its
/// accessor returns (`get`, `pop`, `setdefault`, ...).
fn reached_through(kind: &InstKind) -> Option<(VarId, VarId)> {
    match kind {
        InstKind::Copy { dst, src }
        | InstKind::AttrLoad { dst, obj: src, .. }
        | InstKind::SubscriptLoad { dst, obj: src, .. }
        | InstKind::MakeIter {
            dst, iterable: src, ..
        }
        | InstKind::IterNext { dst, iter: src, .. } => Some((*dst, *src)),
        InstKind::MethodCall {
            dst, recv, method, ..
        } if readonly::ELEMENT_ACCESSORS.contains(&method.as_str())
            || matches!(method.as_str(), "pop" | "popitem" | "setdefault") =>
        {
            Some((*dst, *recv))
        }
        _ => None,
    }
}

/// `map[var] = map[var] ∪ r`; whether it grew.
fn join_reach(map: &mut FxHashMap<VarId, Reach>, var: VarId, r: Reach) -> bool {
    if !r.any() {
        return false;
    }
    let slot = map.entry(var).or_default();
    let joined = slot.join(r);
    let grown = joined != *slot;
    *slot = joined;
    grown
}

/// `map[var] |= bits`; whether it grew.
fn or_bits(map: &mut FxHashMap<VarId, u64>, var: VarId, bits: u64) -> bool {
    if bits == 0 {
        return false;
    }
    let slot = map.entry(var).or_default();
    let grown = *slot | bits != *slot;
    *slot |= bits;
    grown
}
