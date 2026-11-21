//! Local (one function) effect extraction (docs/ANALYSIS_AND_KB_SPEC.md section 2.2).
//!
//! Produces the function's local `EffectSummary` plus the `CallSite`s the interprocedural
//! fixpoint (`callgraph`) maps callee facts through. No IR survives this module.

use crate::cache_flow;
use crate::capacity::{self, Bound, CapacityOf, CapacityWait, SiteQueues, WhileFull};
use crate::construction::{Escape, Pos, Recorder};
use crate::consumption::{ItemArgs, IterFlow, PerItemCall, Roots, param_iteration};
use crate::environment::{self, AccessKind, ExternalAccess};
use crate::guards::{self, GuardInputs, LocalGuards, per_item_bit};
use crate::invalidation::{self, Events, FlowWrite, FuncFlow, Reach, SiteFlow};
use crate::iterations::{
    self, BetweenSteps, ContainerChange, IteratedContainer, IteratedResult, ResultIteration,
};
use crate::ownership::{self, LocalOrigins, Origins, ParamRole, ReturnOrigins, Source};
use crate::readonly::{self, Derivation, ProtectedReturn, ReturnStep, Shared};
use crate::resolver::{
    self, Callee, DeclKeys, FunctionDecl, ModuleDecls, TypeCtx, Val, func_qualname,
    is_generator_function, self_aliases,
};
use crate::{
    Access, ArgBinding, ArgOrigin, ArgTargets, AttrKeyPath, BoundCallee, CallHop, CallSite,
    CallTarget, Construction, ContractClass, Contracts, Dispatch, EffectSummary, FileSummary,
    FuncKey, GuardSubject, Implementers, Items, ModuleInput, MustExec, NewArg, NewCond, NewKind,
    OnEveryReturn, OptInFacts, PerItem, ProjectDecls, ReadKind, ReturnsNew, Runner, SharedLine,
    SiteRole, Slot, TypedFieldReadSite, TypedFieldWriteSite, Unresolved, UnresolvedAccessSite,
    UnresolvedWriteSite, WriteKind,
};
use heki_ast::Span;
use heki_ir::walk::{Answer, FnShape};
use heki_ir::{
    BinOpKind, BlockId, BuiltinKind, CompareOpKind, ConstLiteral, DictEntry, FunctionId,
    FunctionKind, Inst, InstKind, IrFunction, ModuleIr, Must, NaturalLoop, PhiNode, Progress,
    Terminator, UnaryOpKind, VarId,
};
use heki_kb::{EffectiveEcosystemKb, ExecEffectKind};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

mod statements;

/// Methods that mutate their receiver in place (section 2.4; shared by MUT101/201/202).
pub const CONTAINER_MUTATORS: &[&str] = &[
    "append",
    "extend",
    "insert",
    "pop",
    "remove",
    "clear",
    "sort",
    "reverse",
    "add",
    "discard",
    "update",
    "popitem",
    "setdefault",
    "difference_update",
    "intersection_update",
    "symmetric_difference_update",
    "__setitem__",
    "__delitem__",
];

/// The (value, place) pairs the store walk of `return_origins` may take per byte of a function's
/// source, above a floor: a function past it returns objects of unknown origin.
const KEPT_PAIRS_PER_SOURCE_BYTE: usize = 1;
const MIN_KEPT_PAIRS: usize = 4096;

/// The values the origin walks of `return_origins` may take per byte of a function's source,
/// above a floor: a function past it returns objects of unknown origin.
const ORIGIN_STEPS_PER_SOURCE_BYTE: usize = 1;
const MIN_ORIGIN_STEPS: usize = 4096;

/// Augmented assignments changing a container in place, by the container's class: `xs += ys`
/// extends a list, `s |= t` updates a set (`__iadd__`, `__ior__`, ...).
const IN_PLACE_OPERATORS: &[(&str, &[BinOpKind])] = &[
    ("builtins.list", &[BinOpKind::Add, BinOpKind::Mult]),
    ("builtins.bytearray", &[BinOpKind::Add, BinOpKind::Mult]),
    (
        "builtins.set",
        &[
            BinOpKind::BitOr,
            BinOpKind::BitAnd,
            BinOpKind::Sub,
            BinOpKind::BitXor,
        ],
    ),
    ("builtins.dict", &[BinOpKind::BitOr]),
    ("collections.deque", &[BinOpKind::Add, BinOpKind::Mult]),
    ("collections.OrderedDict", &[BinOpKind::BitOr]),
    ("collections.defaultdict", &[BinOpKind::BitOr]),
    (
        "collections.Counter",
        &[
            BinOpKind::Add,
            BinOpKind::Sub,
            BinOpKind::BitOr,
            BinOpKind::BitAnd,
        ],
    ),
];

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
pub(crate) fn summarize_module(
    input: &ModuleInput<'_>,
    decls: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    own: (Arc<ModuleDecls>, DeclKeys),
    track: bool,
    contracts: Option<&dyn Contracts>,
    opt_in: OptInFacts,
) -> FileSummary {
    let (own_decls, completion_reads) = own;
    let (mut file, reads) = if track {
        decls.record(|| extract_module(input, decls, kb, contracts, opt_in))
    } else {
        (
            extract_module(input, decls, kb, contracts, opt_in),
            DeclKeys::default(),
        )
    };
    file.decls = own_decls;
    file.decl_reads = reads;
    file.decl_reads.extend(completion_reads);
    file
}

fn extract_module(
    input: &ModuleInput<'_>,
    decls: &ProjectDecls,
    kb: &EffectiveEcosystemKb,
    contracts: Option<&dyn Contracts>,
    opt_in: OptInFacts,
) -> FileSummary {
    let cx = TypeCtx::new(input, decls, kb);
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
        } = Extractor::new(&shape, &key, &cx, &input.path, contracts, opt_in).run();
        // A `class` statement in a function runs its body there: the body's effects and calls
        // are the function's (its methods run only when they are called).
        if !f.is_module_body {
            for body in class_bodies_run_by(input.ir, f) {
                let body = FnShape::new(body);
                let facts = Extractor::new(&body, &key, &cx, &input.path, contracts, opt_in).run();
                absorb_class_body(&mut summary, facts.summary);
                external.extend(facts.external);
                sites.extend(
                    facts
                        .sites
                        .into_iter()
                        .map(|site| CallSite { flow: None, ..site }),
                );
                references.extend(facts.references);
            }
        } else if contracts.is_some_and(|c| c.references()) {
            // The module body runs the bodies of its classes: what they hand on (a dataclass
            // field's `default_factory`) is handed on by the module. Their other facts are not
            // the module's.
            for body in class_bodies_run_by(input.ir, f) {
                let body = FnShape::new(body);
                let facts = Extractor::new(&body, &key, &cx, &input.path, contracts, opt_in).run();
                references.extend(facts.references);
            }
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
        if let Some(flow) = cache_flow::cache_flow(&shape, &key, &sites, &cx) {
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
        // Same qualname defined twice (conditional definitions): keep the union of facts.
        match file.local_functions.get_mut(&key) {
            Some(existing) => merge_duplicate(Arc::make_mut(existing), summary),
            None => {
                file.local_functions.insert(key, Arc::new(summary));
            }
        }
    }
    release_spare_capacity(&mut file);
    file
}

/// The class bodies the `class` statements of `f` run, nested classes included, each once (a
/// `class` statement in a `finally` body is made once per way out of its `try`).
fn class_bodies_run_by<'m>(ir: &'m ModuleIr, f: &'m IrFunction) -> Vec<&'m IrFunction> {
    let mut out: Vec<&IrFunction> = Vec::new();
    let mut stack = vec![f];
    while let Some(g) = stack.pop() {
        for inst in g.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::MakeClosure {
                func_id: Some(id), ..
            } = &inst.kind
                && ir.function(*id).is_class_body
                && out.iter().all(|c| c.id != *id)
            {
                out.push(ir.function(*id));
                stack.push(ir.function(*id));
            }
        }
    }
    out
}

/// Add the facts of a class body the function runs (`body`) to the function's own: its
/// execution effects and the project attributes it writes and reads.
fn absorb_class_body(into: &mut EffectSummary, body: EffectSummary) {
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
    into.private_calls.extend(body.private_calls);
    into.unknown_calls |= body.unknown_calls;
    into.capacity_waits.extend(body.capacity_waits);
    into.refresh_typed_field_writes();
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

fn merge_duplicate(into: &mut EffectSummary, other: EffectSummary) {
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
    for (i, slots) in other.inplace_param_slots {
        into.inplace_param_slots.entry(i).or_default().extend(slots);
    }
    into.mutates_self_attrs.extend(other.mutates_self_attrs);
    if let Some(facts) = other.iterations {
        into.iteration_facts_mut().merge(*facts);
    }
    into.invalidates_cached.extend(other.invalidates_cached);
    into.typed_field_write_sites
        .extend(other.typed_field_write_sites);
    into.typed_field_read_sites
        .extend(other.typed_field_read_sites);
    into.private_calls.extend(other.private_calls);
    into.offloads_blocking_params |= other.offloads_blocking_params;
    into.unknown_calls |= other.unknown_calls;
    if let Some(o) = other.returns_protected {
        let ProtectedReturn {
            producer,
            shared,
            chain,
            declared,
        } = o;
        ProtectedReturn::add(
            &mut into.returns_protected,
            producer,
            declared,
            shared,
            ReturnStep::ITSELF,
            chain,
        );
    }
    into.retains_params |= other.retains_params;
    // A must-fact: a new object only when both definitions return one.
    into.returns_new = match (std::mem::take(&mut into.returns_new), other.returns_new) {
        (ReturnsNew::New { kind: a, cond: c }, ReturnsNew::New { kind: b, cond: d }) => {
            ReturnsNew::New {
                kind: a.min(b),
                cond: c.and(&d),
            }
        }
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
    into.refresh_typed_field_writes();
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
    /// Where the object comes from among the objects that existed before the call (`MUT109`).
    reach: Reach,
    /// The variable holding the object (`TypedFieldWriteSite::construction`).
    var: Option<VarId>,
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
    /// Where `obj` comes from (`MUT109`).
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

/// The object a typed write changes, as its write site records it.
#[derive(Debug, Clone, Copy)]
struct Written {
    /// `TypedFieldWriteSite::obj_params`.
    params: u64,
    /// `TypedFieldWriteSite::fresh`.
    fresh: bool,
    /// Where it comes from (`MUT109`).
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
    /// The values `isinstance` checks give variables in the blocks they guard.
    narrowed: FxHashMap<(BlockId, VarId), Val>,
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
    /// `x.__dict__` -> owner of `x`.
    dict_of: FxHashMap<VarId, Owner>,
    /// Entries of the containers that project-class attributes hold (`WriteKind::ItemMut`).
    item_alias: FxHashMap<VarId, ItemOf>,
    /// Parameters (bit `i`: parameter `i < 64`) each variable may be, through copies and phis
    /// (any input): the may-alias counterpart of `param_alias`'s `(i, None)` entries.
    may_params: FxHashMap<VarId, u64>,
    /// Instances the function constructs by calling a project class (`Model(...)`,
    /// `cls(...)`), through copies and phis whose every input is one (`a = A(x) if c else
    /// A(y)`): `TypedFieldWriteSite::fresh`.
    fresh: FxHashSet<VarId>,
    /// Attributes the function assigns or deletes on any object.
    stored_attrs: Vec<&'a str>,
    /// The block and phi defining each phi variable (`GuardInputs::phis`).
    phis: OnceCell<FxHashMap<VarId, (BlockId, &'a PhiNode)>>,
    /// The function's integer constants (`capacity`), built on first use.
    ints: OnceCell<heki_ir::FnIndex<'a>>,
    /// What the queue capacity walks of the function's values found so far.
    capacity_walks: RefCell<capacity::Walks>,
    /// Buffers of the guard analysis's backward walks (`GuardInputs::scratch`).
    scratch: RefCell<(FxHashSet<VarId>, Vec<VarId>)>,
    /// The loops around each block.
    around: OnceCell<Vec<Vec<&'a NaturalLoop>>>,
    guards: LocalGuards,
    summary: EffectSummary,
    sites: Vec<CallSite>,
    /// Resolved callee of every `Call` / `MethodCall` by position (for the iteration flow).
    callees: FxHashMap<(BlockId, usize), Callee>,
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
    /// The points `MUT109` describes (`Some` when an `invalidation` contract is configured).
    events: Option<Events>,
    /// Contracts naming this function an exempt writer (`MUT109`), matched at its first write
    /// to a contract target.
    exempt: Option<u64>,
    /// Where each variable's object comes from among the objects that existed before the call
    /// (`MUT109`; empty unless an `invalidation` contract is configured).
    reach: FxHashMap<VarId, Reach>,
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
    /// (its writes to its parameters are reported at callers that may hand it new objects).
    track: bool,
    /// Variables that may hold what a call returns that may be a new object (a superset of the
    /// allocations `Recorder` learns while visiting), through copies and phis: only they and the
    /// parameters are asked about.
    maybe_new: FxHashSet<VarId>,
    /// [`Contracts::ownership`]: the facts only ownership contracts read are computed.
    ownership: bool,
    /// The opt-in facts the enabled rules read.
    opt_in: OptInFacts,
    /// Per call instruction, whether the function has made it whenever it returns normally
    /// (filled before the visit when an enabled rule reads [`EffectSummary::must_exec`]).
    must_run: FxHashMap<(BlockId, usize), OnEveryReturn>,
    /// [`KeyFacts`], built on first use.
    key_facts: OnceCell<KeyFacts<'a>>,
    /// The changes of checked containers of `self` the visit finds, recorded after it
    /// ([`Self::self_container_changes`]).
    self_changes: Vec<SelfChange>,
    /// [`Contracts::references`]: the references handing a target on are extracted.
    references: bool,
    /// `(call, argument)`: a callable a library API runs (`Self::handed_callable`), and the one
    /// each `functools.partial` object it is holds, by the call handing it over.
    handed: FxHashSet<(VarId, VarId)>,
    /// The accesses outside the project that effect boundaries forbid
    /// (`FileSummary::external_accesses`).
    external: Vec<ExternalAccess>,
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

impl<'a> Extractor<'a> {
    fn new(
        shape: &'a FnShape<'a>,
        key: &'a FuncKey,
        cx: &'a TypeCtx<'a>,
        path: &'a Path,
        contracts: Option<&'a dyn Contracts>,
        opt_in: OptInFacts,
    ) -> Self {
        let f = shape.f;
        let (types, narrowed, isinstance_classes) = cx.infer_flow(f);
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
            is_generator: is_generator_function(f),
            is_cached_property: decorated_as(cx.module, f, &["cached_property"]),
            is_lru_cache: decorated_as(cx.module, f, &["lru_cache", "cache"]),
            owner_class_qualname: f.owner_class_qualname.clone(),
            decorators: f.decorators.clone(),
            ..EffectSummary::default()
        };
        let live = ownership::Live::of(f, &defs);
        let mut ex = Self {
            f,
            shape,
            key,
            cx,
            path,
            types,
            narrowed,
            isinstance_classes,
            defs,
            live,
            uses,
            flow_uses: OnceCell::new(),
            self_vars,
            param_alias: FxHashMap::default(),
            param_slot_alias: FxHashMap::default(),
            param_copy_alias: FxHashMap::default(),
            memo_fills: FxHashSet::default(),
            self_attr_alias: FxHashMap::default(),
            obj_attr_alias: FxHashMap::default(),
            dict_of: FxHashMap::default(),
            item_alias: FxHashMap::default(),
            may_params: FxHashMap::default(),
            fresh: FxHashSet::default(),
            stored_attrs,
            phis: OnceCell::new(),
            ints: OnceCell::new(),
            capacity_walks: RefCell::default(),
            scratch: RefCell::default(),
            around: OnceCell::new(),
            guards: LocalGuards::default(),
            summary,
            sites: Vec::new(),
            callees: FxHashMap::default(),
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
            rec: Recorder::default(),
            pending_sites: Vec::new(),
            pending_args: Vec::new(),
            param_origins: Vec::new(),
            pending_unresolved: Vec::new(),
            track: false,
            maybe_new: FxHashSet::default(),
            ownership: false,
            opt_in,
            must_run: FxHashMap::default(),
            key_facts: OnceCell::new(),
            self_changes: Vec::new(),
            references: contracts.is_some_and(|c| c.references()),
            handed: FxHashSet::default(),
            external: Vec::new(),
        };
        ex.ownership = contracts.is_some_and(|c| c.ownership());
        ex.track = ex.ownership && ex.may_construct();
        ex.build_may_params();
        ex.build_fresh();
        if ex.events.is_some() {
            ex.build_reach();
        }
        ex.build_aliases();
        ex.memo_fills = ex.memo_fills();
        let class_of = |v: VarId| match ex.types.get(&v) {
            Some(Val::Inst(class)) => Some(class.as_str()),
            _ => None,
        };
        if ex.summary.is_lru_cache {
            let module = ex.cx.module;
            ex.summary.cached_result = readonly::cached_result(
                f,
                &ex.defs,
                &ex.live,
                |t| module.resolve_dotted(t, false),
                class_of,
            );
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
            fresh: &self.fresh,
            phis: &self.phis,
            scratch: &self.scratch,
            kb: self.cx.kb,
            project: self.cx.project,
        }
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
        self.guards.binding(&self.guard_inputs(), var, loops)
    }

    /// `(callee, parameter, capacity)` of the argument `var` a call binds exactly to parameter
    /// `slot` of `callee`, when it is a queue (`SiteQueues::handed`).
    fn handed_queue(
        &self,
        callee: &FuncKey,
        slot: usize,
        var: VarId,
    ) -> Option<(FuncKey, u8, CapacityOf)> {
        let queue = matches!(self.types.get(&var), Some(Val::Inst(c)) if capacity::is_queue_class(self.cx.kb, c));
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
                Some(Val::Inst(c)) if self.cx.project.is_project_class(c) => Some(c.clone()),
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

    /// [`Self::may_params`]: a monotone fixpoint over copies and phis.
    fn build_may_params(&mut self) {
        for (i, p) in self.f.params.iter().enumerate().take(64) {
            *self.may_params.entry(p.var).or_default() |= 1u64 << i;
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
                for inst in &bb.insts {
                    if let InstKind::Copy { dst, src } = &inst.kind {
                        let bits = self.may_params.get(src).copied().unwrap_or(0);
                        changed |= or_bits(&mut self.may_params, *dst, bits);
                    }
                }
            }
            if !changed {
                return;
            }
        }
    }

    /// The value of `var` in the block being read (`pos`).
    fn val(&self, var: VarId) -> Option<&Val> {
        self.narrowed
            .get(&(self.pos.0, var))
            .or_else(|| self.types.get(&var))
    }

    /// The class of `var` when it is an instance of one class.
    fn instance_of(&self, var: VarId) -> Option<String> {
        match self.val(var)? {
            Val::Inst(class) => Some(class.clone()),
            _ => None,
        }
    }

    /// The project class of `var`, when it is an instance of one class.
    fn project_class(&self, var: VarId) -> Option<String> {
        match self.val(var)? {
            Val::Inst(c) if self.cx.project.is_project_class(c) => Some(c.clone()),
            _ => None,
        }
    }

    /// The project classes `var` may be an instance of: one, the members of a union, or a
    /// protocol with the classes implementing it structurally.
    fn project_classes(&self, var: VarId) -> Vec<String> {
        let project = self.cx.project;
        match self.val(var) {
            Some(Val::Inst(c)) if project.is_project_class(c) => {
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

    /// Alias maps through `AttrLoad`, `Copy`, agreeing phis, and the calls and property reads
    /// that return an attribute of what they are handed (iterated to a fixpoint).
    fn build_aliases(&mut self) {
        for (i, p) in self.f.params.iter().enumerate() {
            if let Ok(i) = u8::try_from(i)
                && i < 64
            {
                self.param_alias.insert(p.var, (i, None));
            }
        }
        let rebound = Rebound::of(self.f);
        let mut returned_attrs: FxHashMap<VarId, (VarId, String)> = FxHashMap::default();
        for bb in &self.f.blocks {
            self.pos = (bb.id, 0);
            for inst in &bb.insts {
                if let Some((dst, holder, attr)) = self.returned_attr(&inst.kind) {
                    returned_attrs.insert(dst, (holder, attr));
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
                self.item_alias.len(),
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
                    if !self.item_alias.is_empty() {
                        merge_item_phi(&mut self.item_alias, phi.dst, &inputs);
                    }
                }
                for inst in &bb.insts {
                    match &inst.kind {
                        InstKind::Copy { dst, src } => {
                            if let Some(a) = self.param_alias.get(src).cloned() {
                                self.param_alias.insert(*dst, a);
                            }
                            if let Some(a) = self.param_slot_alias.get(src).cloned() {
                                self.param_slot_alias.insert(*dst, a);
                            }
                            if let Some(a) = self.param_copy_alias.get(src).cloned() {
                                self.param_copy_alias.insert(*dst, a);
                            }
                            if let Some(a) = self.self_attr_alias.get(src).cloned() {
                                self.self_attr_alias.insert(*dst, a);
                            }
                            if let Some(a) = self.obj_attr_alias.get(src).cloned() {
                                let copy = ObjAttr { direct: false, ..a };
                                self.obj_attr_alias.insert(*dst, copy);
                            }
                            if let Some(o) = self.dict_of.get(src).cloned() {
                                self.dict_of.insert(*dst, o);
                            }
                            if !self.item_alias.is_empty()
                                && let Some(i) = self.item_alias.get(src).cloned()
                            {
                                self.item_alias.insert(*dst, i);
                            }
                        }
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
                            index,
                            key_const,
                            is_slice,
                            ..
                        } => {
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
                            // An entry of a container a project-class attribute holds (a slice
                            // is a new container).
                            // (Only below a typed attribute: most functions have none.)
                            if !is_slice
                                && self.ownership
                                && !(self.obj_attr_alias.is_empty() && self.item_alias.is_empty())
                                && !self.item_alias.contains_key(dst)
                            {
                                let item = match (
                                    self.obj_attr_alias.get(obj),
                                    self.item_alias.get(obj),
                                ) {
                                    (_, Some(outer)) => Some(outer.clone()),
                                    (Some(a), None) => Some(ItemOf {
                                        classes: a.classes.clone(),
                                        attr: a.attr.clone(),
                                        key: self.key_of(key_const.as_deref(), *index),
                                        obj_params: a.obj_params,
                                        fresh: a.fresh,
                                        reach: a.reach,
                                        holder: a.holder,
                                    }),
                                    (None, None) => None,
                                };
                                if let Some(item) = item {
                                    self.item_alias.insert(*dst, item);
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
                self.item_alias.len(),
            );
            if after == before {
                break;
            }
        }
    }

    /// `(dst, obj, attr)` of an instruction loading attribute `attr` of `obj`: `obj.attr`, or
    /// `getattr(obj, "attr"[, default])` with a constant name (a default only stands in for a
    /// missing attribute).
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
            _ => None,
        }
    }

    /// `(dst, holder, attr)` of a call or property read that returns attribute `attr` of an
    /// object it is handed (`node.members()`, `node.view`, `items_of(node)` returning
    /// `node.items`): every project function it may run returns that attribute of the same
    /// parameter ([`crate::FunctionDecl::returns_param_attr`]), bound here to `holder`, and
    /// every class the value it dispatches on may be runs one of them
    /// ([`resolver::TypeCtx::runs_on_every_class`]).
    fn returned_attr(&self, kind: &InstKind) -> Option<(VarId, VarId, String)> {
        let project = &self.cx.project;
        let (dst, callee, recv, args, star_from, dispatch) = match kind {
            InstKind::AttrLoad { dst, obj, attr } if project.may_return_param_attr(attr) => {
                let getters = self.cx.resolve_attr_load(self.val(*obj), attr)?;
                (
                    *dst,
                    getters,
                    Some(*obj),
                    &[][..],
                    None,
                    (*obj, attr.as_str()),
                )
            }
            InstKind::MethodCall {
                dst,
                recv,
                method,
                args,
                star_from,
                ..
            } if project.may_return_param_attr(method) => {
                let callee = self.cx.resolve_method(self.val(*recv), method);
                let dispatch = (*recv, method.as_str());
                (
                    *dst,
                    callee,
                    Some(*recv),
                    args.as_slice(),
                    *star_from,
                    dispatch,
                )
            }
            InstKind::Call {
                dst,
                callee_var: Some(c),
                args,
                star_from,
                ..
            } => {
                // Calling a class makes a new object; a named function is the one declared.
                let returns_attr = match self.val(*c)? {
                    Val::Ref(fqn) => matches!(
                        project.symbol(fqn),
                        resolver::Symbol::Function(_, decl) if decl.returns_param_attr.is_some()
                    ),
                    Val::Inst(_) | Val::Union(_) => project.may_return_param_attr("__call__"),
                    Val::Class(_) | Val::Super(_) => false,
                };
                if !returns_attr {
                    return None;
                }
                let callee = self.cx.resolve_call(self.val(*c));
                (
                    *dst,
                    callee,
                    None,
                    args.as_slice(),
                    *star_from,
                    (*c, "__call__"),
                )
            }
            _ => return None,
        };
        let Callee::Project(keys, binds_recv) = callee else {
            return None;
        };
        // The argument each callee returns the attribute of: `None` for the receiver, else
        // the position among the call's arguments (a static method binds no receiver).
        let mut returned: Option<(Option<usize>, &str)> = None;
        for key in &keys {
            let decl = self.cx.project.function_decl(key)?;
            let (param, attr) = decl.returns_param_attr.as_ref()?;
            let binds = binds_recv && !decl.is_staticmethod;
            let this = (
                usize::from(*param).checked_sub(usize::from(binds)),
                attr.as_str(),
            );
            if *returned.get_or_insert(this) != this {
                return None;
            }
        }
        let (position, attr) = returned?;
        let (value, name) = dispatch;
        if !self.cx.runs_on_every_class(self.val(value), name, &keys) {
            return None;
        }
        let holder = match position {
            None => recv?,
            Some(i) if star_from.is_none_or(|s| i < usize::from(s)) => *args.get(i)?,
            Some(_) => return None,
        };
        Some((dst, holder, attr.to_owned()))
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
                    let from = match &inst.kind {
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
                    };
                    if let Some((dst, src)) = from {
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

    /// The project class a call constructs: a call of a class object (`Model(...)`,
    /// `cls(...)`), or of a class reached through a module or class (`models.Model(...)`), or
    /// an allocation by `object.__new__` ([`Self::bare_allocation`]).
    fn construction(&self, kind: &InstKind) -> Option<String> {
        let project = self.cx.project;
        match kind {
            InstKind::Call {
                callee_var: Some(c),
                args,
                ..
            } => match self.val(*c)? {
                Val::Ref(fqn) if fqn == "builtins.object.__new__" => self.bare_allocation(args),
                _ => self.class_called(*c),
            },
            InstKind::MethodCall {
                recv, method, args, ..
            } => match self.val(*recv)? {
                Val::Ref(owner) => {
                    let fqn = project.canonical(&format!("{owner}.{method}"));
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
                _ => None,
            },
            _ => None,
        }
    }

    /// The project class `object.__new__(cls, ...)` allocates an instance of, without running
    /// its `__init__`: the class object passed first.
    fn bare_allocation(&self, args: &[VarId]) -> Option<String> {
        self.class_called(*args.first()?)
    }

    /// The project class calling `callee` constructs (`Model`, `cls` in a class method).
    fn class_called(&self, callee: VarId) -> Option<String> {
        match self.val(callee)? {
            Val::Ref(fqn) | Val::Class(fqn) if self.cx.project.is_project_class(fqn) => {
                Some(fqn.clone())
            }
            _ => None,
        }
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

    fn owner_of(&self, var: VarId) -> Owner {
        Owner {
            is_self: self.self_vars.contains(&var),
            param: self.param_alias.get(&var).cloned(),
            typed_classes: self.project_classes(var),
            may_params: self.may_params.get(&var).copied().unwrap_or(0),
            fresh: self.is_fresh(var),
            reach: self.reach_of(var),
            var: Some(var),
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
        let mut origin = ArgOrigin {
            param: self.param_alias.get(&var).cloned(),
            param_slot: self.param_slot_alias.get(&var).cloned(),
            self_attr: self.self_attr_alias.get(&var).cloned(),
            targets: None,
            typed_field,
            typed_instance,
            implementers,
            may_params: self.may_params.get(&var).copied().unwrap_or(0),
            fresh: self.is_fresh(var) || field.is_some_and(|a| a.fresh),
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

    /// [`Self::memo_fills`]: the writes of the parameters' own attributes that fill a memo or
    /// initialize lazily ([`cache_flow::attr_writes`], each parameter an object of its own).
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
        cache_flow::attr_writes(self.shape, &objects)
            .into_iter()
            .filter(|(attr, w)| w.fills_memo(attr) || w.lazy_init)
            .map(|(_, w)| (w.block, w.index as usize))
            .collect()
    }

    /// The parameter (or the object `p.a` holds) the result of `kind` is a shallow copy of
    /// (`EffectSummary::changed_param_attrs`), holding nothing else: a copy call of the catalog
    /// or a copy method without other arguments (`copy.copy(p)`, `dict(p)`, `p.copy()`), or a
    /// display that only unpacks it (`{**p}`, `[*p]`, [`readonly::display_copies`]), of the
    /// parameter itself, of `p.a`, or of such a copy.
    fn shallow_copied_param(&self, kind: &InstKind) -> Option<(u8, Option<String>)> {
        let copied: Vec<VarId> = match kind {
            InstKind::Call { args, kwargs, .. } | InstKind::MethodCall { args, kwargs, .. }
                if kwargs.is_empty() && args.len() <= 1 =>
            {
                self.derivations(kind)
                    .into_iter()
                    .filter(|(_, d)| *d == Derivation::Step(ReturnStep::SHALLOW_COPY))
                    .map(|(v, _)| v)
                    .collect()
            }
            InstKind::DictLiteral { .. } | InstKind::AllocBuiltin { .. } => {
                readonly::display_copies(self.f, kind)
            }
            _ => return None,
        };
        let mut originals = copied.iter().filter_map(|v| {
            self.param_alias
                .get(v)
                .or_else(|| self.param_copy_alias.get(v))
        });
        let first = originals.next()?;
        originals.all(|o| o == first).then(|| first.clone())
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
                self.cx.resolve_method(self.val(*recv), method),
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
        if self.opt_in.must_effects {
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
        let references = if self.references {
            self.reference_sites()
        } else {
            Vec::new()
        };
        self.self_container_changes();
        self.resolve_construction();
        if self.opt_in.return_origins {
            self.returned_origins();
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
        self.summary.refresh_typed_field_writes();
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
        Extracted {
            summary: self.summary,
            sites: self.sites,
            flow,
            references,
            external: self.external,
        }
    }

    /// The function's references handing a project function, method, or class on
    /// ([`FileSummary::reference_sites`]): a load of one whose value reaches, through copies,
    /// phis, and the displays holding it, an argument of a call, a stored attribute, item, or
    /// global, a `return`, a `yield`, or a `raise`. The value as a callee is a call (its own
    /// site), and so is a callable a library API runs (`Self::handed_callable`). Not handed on
    /// either: an argument of a call that only inspects or names it ([`names_only`]), a
    /// receiver, an operand, a subscript, a dict key, a closure's captures (its defaults and
    /// its annotations are not told apart, nor a class's bases and keywords), and the name a
    /// module binds to the object (an import, a module alias), which is the object. A site is
    /// at the load; a name an import or the module body binds is read without a load of its own
    /// (the bound value), so its sites are at the uses handing it on.
    fn reference_sites(&mut self) -> Vec<CallSite> {
        let flow = HandedValues::of(
            self.f,
            |at| self.callees.get(&at).is_some_and(names_only),
            &self.handed,
        );
        let reaching = flow.reaching();
        let mut out = Vec::new();
        for &(at, var, span, imported) in &flow.loads {
            if !reaching.contains(&var) {
                continue;
            }
            self.pos = at;
            let Some(site) = self.reference_site(var, at.0, span) else {
                continue;
            };
            let spans = if imported || flow.bound_by_module(var) {
                flow.handed_at(var)
            } else {
                vec![span]
            };
            out.extend(spans.into_iter().map(|span| CallSite {
                span,
                ..site.clone()
            }));
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
        Some(CallSite {
            caller: self.key.clone(),
            target: CallTarget::Project(callees),
            is_awaited: false,
            span,
            loop_depth: self.f.block_loop_depth(bb),
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
            queues: None,
            // Handing a function on does not run it: no must-fact goes through the reference.
            complete: false,
            on_every_return: OnEveryReturn::default(),
        })
    }

    /// Record `inst` when it reads an attribute of a module outside the project that an effect
    /// boundary forbids reading (`sys.platform`, or `platform` bound by `from sys import
    /// platform`). The value read must be that object by name: a value of unknown origin
    /// reads nothing, nor does a closure cell (`LoadDeref`), whose value is not typed.
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
        {
            self.external.push(ExternalAccess {
                kind: AccessKind::Read,
                name: fqn.clone(),
                span,
            });
        }
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
        let it = param_iteration(f, &flow);
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
        // With the position of the iteration (for `MUT109`'s flow at the site).
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
                    if !matches!(
                        self.cx.resolve_method(self.val(*recv), method),
                        Callee::Project(..)
                    ) {
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
        if returned.is_empty() || !keeps_returned_value(&f.decorators) {
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
                Some(Val::Inst(c)) => Some(c.as_str()),
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
            let params = &mut callees[ci].params;
            if params.len() <= slot {
                params.resize(slot + 1, ArgOrigin::default());
            }
            params[slot].origins = Some(Box::new(o));
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
                ownership::source(kind, callee, constructs, self.cx.kb, made_here)
            }
        }
    }

    /// Answer the construction queries of the visited function (`construction`): the
    /// construction of its typed writes, `ArgOrigin::new_object` of its call arguments, and
    /// `EffectSummary::returns_new`.
    fn resolve_construction(&mut self) {
        let rec = std::mem::take(&mut self.rec);
        let param_alias = &self.param_alias;
        let param_of = |v: VarId| match param_alias.get(&v) {
            Some((i, None)) => Some(*i),
            _ => None,
        };
        // Calling a generator or an `async def` returns a generator or a coroutine.
        let returns = self.f.kind == FunctionKind::Sync
            && !self.f.is_module_body
            && keeps_returned_value(&self.f.decorators);
        let sites = &self.sites;
        // Hand-overs are to project callees of the function's own call sites.
        let callee_of = |site: u32, callee: u16| {
            sites[site as usize].project_callees()[usize::from(callee)]
                .callee
                .clone()
        };
        let (answers, returned) =
            rec.resolve(self.shape, &self.may_params, &param_of, &callee_of, returns);
        for p in std::mem::take(&mut self.pending_sites) {
            let mut site = p.site;
            site.construction = construction_of(&answers[p.query], p.rebinds);
            self.summary.typed_field_write_sites.insert(site);
        }
        for (mut site, q) in std::mem::take(&mut self.pending_unresolved) {
            // A parameter is left to the callers (`MUT205` walks them).
            site.construction = match construction_of(&answers[q], site.kind.rebinds()) {
                Construction::Param { .. } => Construction::No,
                other => other,
            };
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
        if let Some(NewArg::New { kind, cond }) = returned {
            self.summary.returns_new = ReturnsNew::New { kind, cond };
        }
    }

    /// Record an escape of `var`'s object at the current instruction; a definite escape of a
    /// parameter keeps it (`EffectSummary::retains_params`). Only reached when an ownership
    /// contract is declared ([`Self::store_escapes`], [`Self::call_escapes`]).
    fn escape(&mut self, var: VarId, how: Escape) {
        if how == Escape::Definite {
            self.summary.retains_params |= self.may_params.get(&var).copied().unwrap_or(0);
        }
        if self.track
            && (self.may_params.get(&var).is_some_and(|p| *p != 0) || self.may_be_new(var))
        {
            self.rec.escape(self.at(), var, how);
        }
    }

    /// [`Self::track`], and [`Self::maybe_new`] when it is set.
    fn may_construct(&mut self) -> bool {
        let mut seeds: FxHashSet<VarId> = FxHashSet::default();
        for i in self.f.blocks.iter().flat_map(|b| &b.insts) {
            // Any instance result (a superset: the recorder learns the allocations).
            let instance = |v: &VarId| matches!(self.types.get(v), Some(Val::Inst(_)));
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
                    recv == var && CONTAINER_MUTATORS.contains(&method.as_str())
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
            match self.defs.get(&cur).map(|i| &i.kind) {
                Some(InstKind::Copy { src, .. }) => cur = *src,
                Some(_) => return self.maybe_new.contains(&cur),
                None => return !self.maybe_new.is_empty(),
            }
        }
        false
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
            InstKind::AttrStore { value, .. }
            | InstKind::StoreGlobal { value, .. }
            | InstKind::StoreDeref { value, .. }
            | InstKind::YieldFrom { value, .. }
            | InstKind::Await { value, .. }
            | InstKind::EnterWith { mgr: value, .. }
            | InstKind::Yield {
                value: Some(value), ..
            } => self.escape(*value, Escape::Definite),
            InstKind::SubscriptStore { index, value, .. } => {
                self.escape(*index, Escape::Definite);
                self.escape(*value, Escape::Definite);
            }
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
        match kind {
            InstKind::AttrLoad { dst, obj, attr } if attr != "__dict__" => {
                if self.self_vars.contains(obj) {
                    self.record_self_read(*dst, attr);
                }
                let owner = self.owner_of(*obj);
                self.typed_read(owner, attr, ReadKind::DirectAttr, span);
                // A returned attribute that `readonly-returns` names (`Class.attr`).
                if let Some(step) = self.returned.get(dst).copied()
                    && let Some(Val::Inst(class)) = self.val(*obj)
                    && let Some(c) = self.contracts
                {
                    let class = ContractClass::new(c, self.cx.project, class.clone());
                    let (fqn, qualname) = class.member(attr);
                    if let Some(deep) = c.readonly_producer(&fqn, &qualname) {
                        self.seed_protected(fqn, deep, step, span);
                    }
                }
                self.property_read(bb, index, *dst, *obj, attr, span);
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
                    && matches!(self.val(*obj), Some(Val::Inst(c)) if c == "collections.defaultdict")
                    && !self.key_present(bb, index_in_block, *obj, *index)
                {
                    self.container_change(*obj, ContainerChange::Resizes, span);
                }
            }
            InstKind::BinOp {
                dst,
                op,
                lhs,
                in_place: true,
                ..
            } if self.has_in_place_operator(*lhs, *op) => {
                if self.changes_in_place(*dst, *lhs, *op) {
                    self.container_mut(*lhs, None, None, span);
                }
                // `self.items |= other` changes the container before storing it back.
                self.container_change(*lhs, ContainerChange::Resizes, span);
            }
            // A value stored in a module global or a closure variable outlives the call.
            InstKind::StoreGlobal { value, .. } | InstKind::StoreDeref { value, .. } => {
                self.note_escape(*value);
            }
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
                let callee = self.cx.resolve_method(self.val(*recv), method);
                self.unresolved_access(method, Access::Call, *recv, span);
                // A container method changes its receiver in place, unless the receiver's
                // class defines the method itself: then what that method does is in its
                // summary (`Stats.clear()` may change nothing).
                if CONTAINER_MUTATORS.contains(&method.as_str())
                    && !matches!(callee, Callee::Project(..))
                {
                    self.container_method(*recv, method, args, span);
                    for a in args {
                        if matches!(method.as_str(), "append" | "insert" | "add" | "setdefault") {
                            self.note_escape(*a);
                        }
                    }
                }
                if !matches!(callee, Callee::Project(..))
                    && let Some(change) = iterations::method_change(method)
                {
                    self.container_change(*recv, change, span);
                }
                // `obj.__dict__.get("attr"[, default])`.
                if method == "get"
                    && let Some(owner) = self.dict_of.get(recv).cloned()
                    && let Some(k) = args.first().and_then(|a| self.resolved_str(*a))
                {
                    self.typed_read(owner, &k, ReadKind::DictDirect, span);
                }
                let (recv_origin, recv_is_self) = self.receiver(*recv, bb);
                let constructs = self.construction(kind);
                let call = Call {
                    dst: *dst,
                    value: None,
                    recv: Some(recv_origin),
                    recv_var: Some(*recv),
                    args,
                    kwargs,
                    star_from: *star_from,
                    args_at: Some(0),
                    is_awaited: *is_awaited,
                    recv_is_self,
                    constructs,
                    per_item: None,
                    span,
                };
                self.call_inst(bb, index, callee, &call);
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
                let callee_val = callee_var.and_then(|v| self.val(v));
                // `m = obj.method; m(...)` and `getattr(obj, "method")(...)` call a method of
                // the receiver, as `obj.method(...)` does.
                if callee_val.is_none()
                    && let Some((recv, method)) = callee_var.and_then(|v| self.bound_method(v))
                {
                    let target = self.cx.resolve_method(self.val(recv), &method);
                    self.unresolved_access(&method, Access::Call, recv, span);
                    let (recv_origin, recv_is_self) = self.receiver(recv, bb);
                    let call = Call {
                        dst: *dst,
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
                        span,
                    };
                    self.call_inst(bb, index, target, &call);
                    return;
                }
                let target = match callee_var {
                    Some(_) => self.cx.resolve_call(callee_val),
                    // Non-inlined comprehension (Python 3.11, `__comp_<function id>`) runs
                    // immediately.
                    None => callee
                        .strip_prefix("__comp_")
                        .and_then(|n| n.parse::<u32>().ok())
                        .map_or(Callee::Unresolved, |id| self.child_callee(FunctionId(id))),
                };
                let constructs = self.construction(kind);
                let call = Call {
                    dst: *dst,
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
                    span,
                };
                self.call_inst(bb, index, target, &call);
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
                    dst: *dst,
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
                    span,
                };
                self.call(bb, &target, &call);
            }
            // A comprehension scope's `.0` is the iterator its creator built, where the
            // iteration starts (`child_iteration`).
            InstKind::MakeIter {
                iterable, is_async, ..
            } if self.f.var_name(*iterable) != Some(".0") => {
                self.iteration(bb, *iterable, *is_async, span)
            }
            _ => {}
        }
    }

    /// The comprehension or generator expression `child` created here iterates `iterable`
    /// (its outermost one) at once, as its first clause does (`__iter__`, or `__aiter__` for an
    /// `async for`): [`Self::iteration`]. A later `async for` clause does not change how the
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
            self.iteration(bb, iterable, is_async, span);
        }
    }

    /// Iterating `iterable` (a `for` loop, a comprehension, a consuming call such as `list(...)`)
    /// runs `__iter__` of its class, `__aiter__` for an `async for`: the catalog's entry for it
    /// is a call at `span` (`for row in session.query(User):` runs the query). Project classes'
    /// `__iter__` are not followed.
    fn iteration(&mut self, bb: BlockId, iterable: VarId, is_async: bool, span: Span) {
        let Some(Val::Inst(class)) = self.val(iterable) else {
            return;
        };
        if class.starts_with("builtins.") || self.cx.project.is_project_class(class) {
            return;
        }
        let protocol = if is_async { "__aiter__" } else { "__iter__" };
        let symbol = format!("{class}.{protocol}");
        if self.cx.catalog(&symbol).is_some() {
            self.external_call(bb, symbol, Passed::NOTHING, false, span);
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
    /// runs where the caller iterates it).
    fn property_read(
        &mut self,
        bb: BlockId,
        index: usize,
        dst: VarId,
        obj: VarId,
        attr: &str,
        span: Span,
    ) {
        let Some(callee) = self.cx.resolve_attr_load(self.val(obj), attr) else {
            return;
        };
        let (recv, recv_is_self) = self.receiver(obj, bb);
        let is_awaited = self.uses.get(&dst).is_some_and(|uses| {
            uses.iter()
                .any(|u| matches!(u, InstKind::Await { value, .. } if *value == dst))
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
            span,
        };
        self.call_inst(bb, index, callee, &call);
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
            Some((recv, method)) => (self.cx.resolve_method(self.val(recv), &method), Some(recv)),
            None => (self.cx.resolve_call(self.val(var)), None),
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
        if !reallocates || !self.returned.contains_key(&call.dst) {
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
        key: Option<String>,
        kind: WriteKind,
        span: Span,
        obj: Written,
    ) {
        // `MUT109`: a write to a contract target of an object that existed before the call,
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
                mask &= !*self
                    .exempt
                    .get_or_insert_with(|| c.exempt_writer(&key.fqn(), &key.qualname));
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

    /// Read of attribute `attr` on the object `owner`, when it has a project class.
    fn typed_read(&mut self, owner: Owner, attr: &str, kind: ReadKind, span: Span) {
        if let Some(var) = owner.var {
            self.unresolved_access(attr, Access::Read(kind), var, span);
        }
        // A read through a protocol declaring the attribute is one of every class implementing
        // it; the read contracts judge it by the protocol, the type the code reads it through.
        let protocol = match owner.var.and_then(|v| self.val(v)) {
            Some(Val::Inst(p)) if self.cx.project.protocol_declares(p, attr) => Some(p.clone()),
            _ => None,
        };
        for class in owner.typed_classes {
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
                });
        }
    }

    /// Write of attribute `attr` on the object `owner` (AttrStore / AttrDelete / setattr / __dict__).
    fn write_owner_attr(&mut self, owner: Owner, attr: &str, kind: WriteKind, span: Span) {
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
            None => {}
        }
        for class in owner.typed_classes {
            self.typed_site(class, attr, None, kind, span, written);
        }
        if let Some(var) = owner.var.filter(|v| self.unknown_class(*v)) {
            self.unresolved(Unresolved::Receiver(attr.to_owned()), var, kind, span);
        }
    }

    /// Whether heki knows nothing of the class of `var`'s object here: no inferred value, an
    /// annotation that says nothing (`Any`, `object`) that no `isinstance` test narrows, or a
    /// project protocol whose implementations heki cannot list.
    fn unknown_class(&self, var: VarId) -> bool {
        match self.val(var) {
            None => true,
            Some(Val::Inst(c)) => {
                matches!(
                    c.as_str(),
                    "typing.Any" | "typing_extensions.Any" | "builtins.object"
                ) || self.cx.project.protocol(c) == Some(None)
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
        let obj_params = match self.param_alias.get(&obj) {
            Some((i, None)) => 1u64 << i,
            _ => 0,
        };
        self.summary
            .unresolved_access_sites
            .insert(UnresolvedAccessSite {
                name: name.to_owned(),
                access,
                span,
                obj_params,
            });
    }

    /// Record a write heki cannot tie to a class (`MUT205`) of the object `obj` holds: the
    /// object counts as a parameter only when it is one on every path.
    fn unresolved(&mut self, what: Unresolved, obj: VarId, kind: WriteKind, span: Span) {
        if !self.ownership {
            return;
        }
        let obj_params = match self.param_alias.get(&obj) {
            Some((i, None)) => 1u64 << i,
            _ => 0,
        };
        let site = UnresolvedWriteSite {
            what,
            kind,
            span,
            obj_params,
            construction: Construction::No,
        };
        if self.track && self.trackable(obj) {
            let query = self.rec.query(self.at(), obj);
            self.pending_unresolved.push((site, query));
            return;
        }
        self.summary.unresolved_write_sites.insert(site);
    }

    /// A computed attribute name written on `owner`'s object (`setattr(obj, name, v)`,
    /// `obj.__dict__[name] = v`): unresolved when the object has a project class.
    fn computed_name(&mut self, owner: &Owner, kind: WriteKind, span: Span) {
        let Some(var) = owner.var else { return };
        for class in &owner.typed_classes {
            self.unresolved(Unresolved::Name(class.clone()), var, kind, span);
        }
    }

    /// The attribute of an object of unknown class whose container (or an entry of it, `true`)
    /// `var` holds: `obj.attr`, `obj.attr["k"]`, through copies.
    fn untyped_container(&self, var: VarId) -> Option<(String, VarId, bool)> {
        let mut cur = var;
        let mut nested = false;
        for _ in 0..16 {
            match &self.defs.get(&cur)?.kind {
                InstKind::Copy { src, .. } => cur = *src,
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
                    return Some((attr.clone(), *obj, nested));
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
        matches!(self.val(lhs), Some(Val::Inst(class)) if IN_PLACE_OPERATORS
            .iter()
            .any(|(c, ops)| c == class && ops.contains(&op)))
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
        let may_be_counter = matches!(self.val(obj), Some(Val::Inst(c)) if counter(c))
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

    /// In-place mutation of the container held in `var` (mutator method or subscript write):
    /// `const_key` is a literal key (`self.<attr>` keys), `item_key` the constant key of the
    /// changed entry (also a `Final` constant).
    fn container_mut(
        &mut self,
        var: VarId,
        const_key: Option<&str>,
        item_key: Option<String>,
        span: Span,
    ) {
        if let Some(attr) = self.self_attr_alias.get(&var).cloned() {
            self.summary.mutates_self_attrs.insert(AttrKeyPath {
                attr,
                const_key: const_key.map(str::to_owned),
            });
        }
        match self.param_alias.get(&var).cloned() {
            Some((i, None)) => {
                // A builtin container's own method or item store changes it in place; another
                // class's (a `Sequence` whose `append` returns a new one) may not.
                let builtin = matches!(
                    self.val(var),
                    Some(Val::Inst(class)) if BUILTIN_CONTAINERS.contains(&class.as_str())
                );
                if builtin {
                    self.summary
                        .param_inplace_sites
                        .entry(i)
                        .or_default()
                        .insert(span);
                }
                self.mutate_param(i, String::new(), span);
            }
            Some((i, Some(path))) => self.change_below_param_attr(i, path, span),
            None => {}
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
                let key = i.key.clone();
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
            if let Some(k) = self.key_of(key_const, index) {
                if is_delete && owner.is_self {
                    self.summary.invalidates_cached.insert(k.clone());
                }
                self.write_owner_attr(owner, &k, WriteKind::DictDirect, span);
            } else {
                self.computed_name(&owner, WriteKind::DictDirect, span);
            }
            return;
        }
        // The changed entry's key matters only for a typed attribute's container.
        let item_key = if self.obj_attr_alias.contains_key(&obj) {
            self.key_of(key_const, index)
        } else {
            None
        };
        self.container_mut(obj, key_const, item_key, span);
    }

    fn container_method(&mut self, recv: VarId, method: &str, args: &[VarId], span: Span) {
        if let Some(owner) = self.dict_of.get(&recv).cloned() {
            match method {
                "pop" | "__delitem__" | "setdefault" | "__setitem__" => {
                    if let Some(k) = args.first().and_then(|a| self.resolved_str(*a)) {
                        if owner.is_self && matches!(method, "pop" | "__delitem__") {
                            self.summary.invalidates_cached.insert(k.clone());
                        }
                        self.write_owner_attr(owner, &k, WriteKind::DictDirect, span);
                    } else {
                        self.computed_name(&owner, WriteKind::DictDirect, span);
                    }
                }
                // `update`, `popitem`, `clear`: attributes heki cannot name.
                _ => {
                    if method == "clear" && owner.is_self {
                        self.summary.invalidates_cached.insert("*".to_owned());
                    }
                    self.computed_name(&owner, WriteKind::DictDirect, span);
                }
            }
            return;
        }
        // `obj.attr.pop("k")`, `obj.attr.setdefault(KEY, v)`: the entry the call changes, for a
        // typed attribute's container.
        let item_key = if self.obj_attr_alias.contains_key(&recv)
            && matches!(method, "pop" | "__delitem__" | "setdefault" | "__setitem__")
        {
            args.first().and_then(|a| self.resolved_str(*a))
        } else {
            None
        };
        self.container_mut(recv, None, item_key, span);
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
                .is_some_and(|at| self.f.is_starred(call.dst, at + j))
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

    /// [`Self::call`] for a `Call` / `MethodCall` instruction at `(bb, index)`: its callee is
    /// kept for the iteration flow, and a project call is remembered for the sites of the
    /// object it returns.
    fn call_inst(&mut self, bb: BlockId, index: usize, callee: Callee, call: &Call<'_>) {
        if let Some((site, params)) = self.call(bb, &callee, call) {
            let binds_recv = self.sites[site].binds_recv && call.constructs.is_none();
            self.project_calls.push(ProjectCall {
                site,
                at: (bb, index),
                dst: call.dst,
                params,
                recv: call.recv_var.or(call.value).filter(|_| binds_recv),
            });
        }
        self.callees.insert((bb, index), callee);
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
                if let Some(c) = self.contracts {
                    if c.external_call(symbol) {
                        self.external.push(ExternalAccess {
                            kind: AccessKind::Call,
                            name: symbol.clone(),
                            span: call.span,
                        });
                    }
                    if let Some(step) = self.returned.get(&call.dst).copied()
                        && let Some(deep) = c.readonly_producer(symbol, symbol)
                    {
                        self.seed_protected(symbol.clone(), deep, step, call.span);
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
                    self.iteration(bb, iterable, false, call.span);
                }
                self.note_returned_realloc(symbol, call);
                // A project class without a project `__init__` (a dataclass, a pydantic model):
                // its construction is still a call site, with no project callees.
                if let Some(class) = &call.constructs {
                    self.sites.push(CallSite {
                        caller: self.key.clone(),
                        target: CallTarget::Project(Vec::new()),
                        is_awaited: call.is_awaited,
                        span: call.span,
                        loop_depth,
                        recv_is_self: call.recv_is_self,
                        drives_coroutine_manually: false,
                        constructs: Some(class.clone()),
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
                        complete: false,
                        on_every_return: crate::OnEveryReturn::default(),
                    });
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
                // `(callee, its index, receiver, bound arguments, offset)` for `Self::hand_over`.
                let mut handovers = Vec::new();
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
                        };
                        invalidates = Some(invalidates.map_or(here, |i| i & here));
                    }
                    let offset = usize::from(*binds_recv && !decl.is_staticmethod);
                    let mut params = std::mem::take(&mut buffer);
                    // The receiver the call hands over (a constructor's is its new object).
                    let mut handed_recv = None;
                    if offset == 1 {
                        // A class method binds the class (no value of the caller), a
                        // constructor the object it creates; calling an instance (`p()`) binds
                        // it to the `self` of its `__call__`.
                        params.push(if decl.is_classmethod {
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
                                ..ArgOrigin::default()
                            }
                        } else {
                            handed_recv = call.recv_var.or(call.value);
                            call.recv
                                .clone()
                                .or_else(|| call.value.map(|v| self.origin(v)))
                                .unwrap_or_default()
                        });
                    }
                    // A position binds its parameter, and a keyword too unless it lands in
                    // `**kwargs`.
                    let mut bound: Vec<(u8, VarId)> = Vec::new();
                    for (slot, a) in self.positional_slots(call, offset) {
                        let mut o = self.origin(a);
                        if slot < usize::from(decl.positional_params) {
                            o.binding = Some(self.binding(a, loops));
                            handed.extend(self.handed_queue(key, slot, a));
                        }
                        if params.len() <= slot {
                            params.resize(slot + 1, ArgOrigin::default());
                        }
                        params[slot] = o;
                        if let Ok(slot) = u8::try_from(slot) {
                            bound.push((slot, a));
                        }
                    }
                    for (name, v) in call.kwargs {
                        let Some(name) = name else { continue };
                        if let Some(pos) = decl.param_names.iter().position(|p| p == name) {
                            if params.len() <= pos {
                                params.resize(pos + 1, ArgOrigin::default());
                            }
                            params[pos] = self.origin(*v);
                            if pos < usize::from(decl.named_params) {
                                params[pos].binding = Some(self.binding(*v, loops));
                                handed.extend(self.handed_queue(key, pos, *v));
                            }
                            if let Ok(pos) = u8::try_from(pos) {
                                bound.push((pos, *v));
                            }
                        }
                    }
                    // An omitted parameter has its literal default, unless `*` / `**`
                    // unpacking may pass it.
                    if call.star_from.is_none() && call.kwargs.iter().all(|(n, _)| n.is_some()) {
                        for (pos, c) in &decl.defaults {
                            let pos = usize::from(*pos);
                            let by_position = pos < usize::from(decl.positional_params)
                                && pos
                                    < offset
                                        + call.args.len()
                                        + call.per_item.and_then(|p| p.positions).unwrap_or(0);
                            let by_name = decl.param_names.get(pos).is_some_and(|n| {
                                call.kwargs
                                    .iter()
                                    .any(|(k, _)| k.as_deref() == Some(n.as_str()))
                            });
                            if by_position || by_name {
                                continue;
                            }
                            if params.len() <= pos {
                                params.resize(pos + 1, ArgOrigin::default());
                            }
                            params[pos].binding = Some(ArgBinding {
                                constant: Some(*c),
                                ..ArgBinding::default()
                            });
                        }
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
                    if self.ownership && !offloaded {
                        handovers.push((decl, callees.len(), handed_recv, bound.clone(), offset));
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
                for (decl, i, recv, bound, offset) in handovers {
                    let bound = &bound;
                    self.hand_over(
                        call,
                        decl,
                        i,
                        &Handed {
                            recv,
                            bound,
                            offset,
                        },
                    );
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
                let drives = any_async && !call.is_awaited && self.drives_manually(call.dst);
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
                let site = self.site(bb, CallTarget::Project(callees), call.is_awaited, call.span);
                self.sites.push(CallSite {
                    recv_is_self: call.recv_is_self,
                    // A builtin calling the callees per item uses what they return.
                    result_discarded: call.recv_is_self
                        && call.per_item.is_none()
                        && self.discards(call.dst),
                    drives_coroutine_manually: drives,
                    binds_recv: *binds_recv,
                    dispatch: self.dispatch(call, site.project_callees()),
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
                        self.returned.get(&call.dst).copied()
                    },
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
        let Some(Val::Inst(protocol)) = self.val(call.recv_var?) else {
            return None;
        };
        let name = callees.first()?.callee.qualname.rsplit('.').next()?;
        let project = self.cx.project;
        project
            .protocol_declares(protocol, name)
            .then(|| project.mro_method(protocol, name))?
    }

    /// What the receiver of `call` tells about the implementations `callees` run
    /// ([`Dispatch`]): the receiver's class (the method's own `self`, a receiver of one project
    /// class, or the class a constructor call makes) when a callee is inherited from a proper
    /// base of it, and, on `self`, the callees that are overrides of subclasses.
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
        if call.recv_is_self {
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
                providers,
                declared,
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
            self.handed.insert((call.dst, callable));
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
                    constant: None,
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

    /// What calling `var` calls when it holds a `functools.partial` object (`partial(f, a,
    /// k=b)` calls `f(a, ..., k=b)`), through copies and at most 16 nested partials (the inner
    /// one's arguments go first; past the bound, the call is not resolved).
    fn partial(&self, var: VarId) -> Option<Partial> {
        let mut found: Option<Partial> = None;
        let mut cur = var;
        for _ in 0..16 {
            let made = self.copied(cur);
            let Some((inner, held, kwargs, open)) = self
                .defs
                .get(&made)
                .and_then(|i| self.partial_call(&i.kind))
            else {
                break;
            };
            let p = found.get_or_insert_with(|| Partial {
                callable: inner,
                calls: Vec::new(),
                args: Vec::new(),
                kwargs: Vec::new(),
                open: false,
            });
            p.callable = inner;
            p.calls.push((made, inner));
            // The outer partials' arguments come after what this one unpacks with `*`.
            if open {
                p.args.clear();
            }
            p.args.splice(0..0, held.iter().copied());
            // An outer partial's `**` unpacking may override any keyword this one holds.
            let overridden = p.kwargs.iter().any(|(k, _)| k.is_none());
            p.kwargs.splice(
                0..0,
                kwargs
                    .iter()
                    .filter(|(k, _)| !overridden || k.is_none())
                    .cloned(),
            );
            p.open |= open;
            cur = inner;
        }
        found
    }

    /// The parts of a `functools.partial(f, ...)` call: `f`, the positional arguments after it
    /// up to the first one `*` unpacks, the keywords, and whether one is unpacked.
    #[allow(clippy::type_complexity)]
    fn partial_call(
        &self,
        kind: &'a InstKind,
    ) -> Option<(VarId, &'a [VarId], &'a [(Option<String>, VarId)], bool)> {
        let (callee, args, kwargs, star_from) = match kind {
            InstKind::Call {
                callee_var: Some(c),
                args,
                kwargs,
                star_from,
                ..
            } => (self.cx.resolve_call(self.val(*c)), args, kwargs, star_from),
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                star_from,
                ..
            } => (
                self.cx.resolve_method(self.val(*recv), method),
                args,
                kwargs,
                star_from,
            ),
            _ => return None,
        };
        matches!(callee, Callee::External(s) if s == "functools.partial").then_some(())?;
        let known = star_from.map_or(args.len(), |s| usize::from(s).min(args.len()));
        let (&inner, held) = args[..known].split_first()?;
        Some((inner, held, kwargs, known < args.len()))
    }

    /// A call site in block `bb` with the caller's guards there.
    /// Nothing reads `v`: no instruction, phi, or terminator (a call made as a statement).
    fn discards(&self, v: VarId) -> bool {
        let flow_uses = self.flow_uses.get_or_init(|| {
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
        });
        !self.uses.contains_key(&v) && !flow_uses.contains(&v)
    }

    fn site(&self, bb: BlockId, target: CallTarget, is_awaited: bool, span: Span) -> CallSite {
        let cx = self.guard_inputs();
        let (complete, on_every_return) = if self.opt_in.must_effects {
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
            complete,
            on_every_return,
        }
    }

    /// Whether the call at [`Self::pos`] has as its targets every implementation it can run
    /// (`CallSite::complete`).
    fn targets_complete(&self) -> bool {
        let (bb, index) = self.pos;
        match self.f.block(bb).insts.get(index).map(|i| &i.kind) {
            Some(InstKind::MethodCall { recv, method, .. }) => {
                let recv = self.val(*recv);
                let callee = self.cx.resolve_method(recv, method);
                self.cx.method_targets_complete(recv, method, &callee)
            }
            Some(InstKind::Call { callee_var, .. }) => {
                let value = callee_var.and_then(|v| self.val(v));
                let callee = self.cx.resolve_call(value);
                self.cx.call_targets_complete(value, &callee)
            }
            _ => false,
        }
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

    /// [`Self::must_run`] and [`MustExec::never_returns`], from the path facts of the function
    /// with what the catalog and the project's declarations know: the members of modules and
    /// the context managers whose exit never suppresses an exception.
    fn path_facts(&mut self) {
        let f = self.f;
        let every = |a: Answer<Must>| a == Answer::Known(Must::Every);
        let owner = "heki_effect::summary::Extractor::path_facts";
        let (must_run, never_returns) =
            crate::with_paths(self.cx, f, &self.types, owner, |paths, _| {
                let mut must_run = FxHashMap::default();
                for bb in &f.blocks {
                    for (index, inst) in bb.insts.iter().enumerate() {
                        if !crate::anchors_call_site(&inst.kind) {
                            continue;
                        }
                        let at = (bb.id, index);
                        let called = paths.on_every_return(at, Progress::Called);
                        must_run.insert(
                            at,
                            OnEveryReturn {
                                called: every(called),
                                returned: every(paths.on_every_return(at, Progress::Returned)),
                                live: called.is_open() || called != Answer::Known(Must::Dead),
                            },
                        );
                    }
                }
                let never_returns = f.blocks.first().is_some_and(|entry| {
                    paths.on_every_return((entry.id, 0), Progress::Called)
                        == Answer::Known(Must::NeverCompletes)
                });
                (must_run, never_returns)
            });
        self.must_run = must_run;
        if never_returns {
            self.summary.must_exec = Some(Box::new(MustExec {
                never_returns,
                ..MustExec::default()
            }));
        }
    }

    /// The allocation a call makes and the escapes of what it is handed, before its callees
    /// are bound: a constructor call allocates its object, a project function may return a new
    /// one, and a call heki cannot resolve may keep everything it is handed.
    fn call_escapes(&mut self, callee: &Callee, call: &Call<'_>) {
        if !self.ownership {
            return;
        }
        match callee {
            Callee::External(symbol) => {
                let new_instance = call
                    .constructs
                    .as_ref()
                    .is_some_and(|c| !self.cx.project.may_reuse_instances(c));
                self.external_escapes(
                    symbol,
                    call.dst,
                    call.recv_var,
                    call.args,
                    call.kwargs,
                    new_instance,
                );
            }
            Callee::Project(keys, _) => {
                if !self.track || call.per_item.is_some() {
                } else if let Some(class) = &call.constructs {
                    if !self.cx.project.may_reuse_instances(class) {
                        self.rec
                            .alloc(self.at(), call.dst, NewKind::Own, Vec::new());
                    }
                } else if self.may_be_new(call.dst) {
                    // A project function returning an instance of a project class: a new object
                    // when every candidate returns one.
                    self.rec
                        .alloc(self.at(), call.dst, NewKind::Own, keys.clone());
                }
            }
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

    /// The objects a call hands to the parameters of its project callee `decl` (the
    /// `callee`-th of the site the call adds). Each object bound to a parameter exactly is asked
    /// about for `ArgOrigin::new_object` and is kept only if the callee keeps that parameter; an
    /// argument after `*` unpacking, or one no parameter takes, may be kept. A generator or a
    /// coroutine that is created, not run, keeps its arguments. Only called when an ownership
    /// contract is declared.
    fn hand_over(&mut self, call: &Call<'_>, decl: &FunctionDecl, callee: usize, handed: &Handed) {
        let limit = call
            .star_from
            .map_or(usize::MAX, |s| handed.offset + usize::from(s));
        let handed: Vec<(u8, VarId)> = handed
            .recv
            .map(|v| (0, v))
            .into_iter()
            .chain(
                handed
                    .bound
                    .iter()
                    .copied()
                    .filter(|(p, _)| usize::from(*p) < limit),
            )
            .collect();
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
        for (pos, var) in handed {
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
        dst: VarId,
        recv: Option<VarId>,
        args: &[VarId],
        kwargs: &[(Option<String>, VarId)],
        constructs: bool,
    ) {
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
            _ if symbol
                .strip_prefix("builtins.")
                .is_some_and(|n| INSPECTING_BUILTINS.contains(&n)) =>
            {
                (None, true, usize::MAX)
            }
            "builtins.setattr" | "builtins.object.__setattr__" | "builtins.delattr" => {
                (None, true, 2)
            }
            _ => (None, false, 0),
        };
        if let Some(kind) = alloc.filter(|_| self.track) {
            self.rec.alloc(self.at(), dst, kind, Vec::new());
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
            match args.get(1).and_then(|a| self.resolved_str(*a)) {
                Some(attr) => self.write_attr(*obj, &attr, WriteKind::SetAttr, span),
                None => {
                    let owner = self.owner_of(*obj);
                    self.computed_name(&owner, WriteKind::SetAttr, span);
                }
            }
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
        let Some(entry) = self.cx.catalog(&symbol) else {
            return;
        };
        if entry.excluded {
            return;
        }
        let case = entry.case_for(|i, k| {
            resolver::passed_arg(passed.args, passed.kwargs, passed.star_from, (i, k), |v| {
                resolver::arg_fact(
                    |d| self.defs.get(&d).copied(),
                    |c| self.instance_of(c),
                    |n| match self.val(n)? {
                        Val::Ref(fqn) => Some(fqn.clone()),
                        _ => None,
                    },
                    &|n| self.cx.global_str(n).map(str::to_owned),
                    v,
                )
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
        for e in effects.iter().chain(guarded.into_iter().flatten()) {
            let guards = crate::EffectGuards {
                bounded_queue: own_param.filter(|_| !effects.contains(e)),
                ..occurrence_of(e)
            };
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
        if !must.is_empty() && self.must_run.get(&self.pos).is_some_and(|r| r.called) {
            let hop = Arc::new(self.hop(api.clone(), span));
            let facts = self.summary.must_exec.get_or_insert_with(Box::default);
            for e in must {
                facts
                    .effects
                    .entry(e)
                    .or_insert_with(|| vec![Arc::clone(&hop)]);
            }
        }
        let target = CallTarget::Layer2 {
            symbol: api,
            effects,
            offloads_blocking_arg: entry.offloads_blocking_arg,
            async_safe: entry.async_safe,
            unconditional,
        };
        let site = self.site(bb, target, is_awaited, span);
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
            .filter(|e| match entry.emits_in(case) {
                heki_kb::Emits::Always => true,
                // A connection that may be in memory sends its statements without waiting.
                heki_kb::Emits::IfOnDisk => **e != ExecEffectKind::BlockingDb,
                _ => !matches!(e, ExecEffectKind::BlockingDb | ExecEffectKind::DbQuery),
            })
            .fold(0, |mask, e| mask | crate::effect_bit(*e))
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

    /// The function returns (through `step`) the result of the read-only producer `producer`
    /// it calls or reads at `span` (`MUT202` return flow; `deep` as the producer is declared).
    fn seed_protected(&mut self, producer: String, deep: bool, step: ReturnStep, span: Span) {
        let chain = vec![Arc::new(self.hop(producer.clone(), span))];
        ProtectedReturn::add(
            &mut self.summary.returns_protected,
            producer,
            true,
            Shared::produced(deep),
            step,
            chain,
        );
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
    /// The variable the call defines.
    dst: VarId,
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

/// What calling a `functools.partial` object calls ([`Extractor::partial`]).
struct Partial {
    callable: VarId,
    /// Each nested `functools.partial` call: the object it makes and the callable it holds.
    calls: Vec<(VarId, VarId)>,
    /// The positional arguments it passes first, up to the first one `*` unpacks.
    args: Vec<VarId>,
    /// Its keywords (`**` unpacking is a `None` keyword).
    kwargs: Vec<(Option<String>, VarId)>,
    /// It unpacks positional arguments with `*`: the positions after `args` are not known.
    open: bool,
}

/// How the values of a function reach the uses handing them on ([`Extractor::reference_sites`]).
struct HandedValues {
    /// Where each value comes from: the values it copies, or the phi inputs it merges.
    from: FxHashMap<VarId, Vec<VarId>>,
    /// The values each display holds (a dict display's values, not its keys).
    held: FxHashMap<VarId, Vec<VarId>>,
    /// The uses handing each value on, and whether only a display is handed on there: a module
    /// binding a name to a function or class binds the name to the object, and binding it to
    /// a display stores what the display holds.
    uses: FxHashMap<VarId, Vec<(Span, bool)>>,
    /// The loads: where, the value, its span, and whether it is an import.
    loads: Vec<(Position, VarId, Span, bool)>,
    /// What each value flows to (a copy, a phi, or a display holding it: `true`), built on
    /// first use.
    onward: OnceCell<FxHashMap<VarId, Vec<(VarId, bool)>>>,
}

impl HandedValues {
    /// The flow of `f`'s values; `names_only_at` tells the calls that hand nothing on, and
    /// `handed` the callables library APIs run (`(call, argument)`).
    fn of(
        f: &IrFunction,
        names_only_at: impl Fn(Position) -> bool,
        handed: &FxHashSet<(VarId, VarId)>,
    ) -> Self {
        let mut flow = Self {
            from: FxHashMap::default(),
            held: FxHashMap::default(),
            uses: FxHashMap::default(),
            loads: Vec::new(),
            onward: OnceCell::new(),
        };
        for bb in &f.blocks {
            for phi in &bb.phis {
                let inputs = phi.incoming.iter().map(|(_, v)| *v);
                flow.from.entry(phi.dst).or_default().extend(inputs);
            }
            for (index, inst) in bb.insts.iter().enumerate() {
                let (at, span) = ((bb.id, index), inst.span);
                match &inst.kind {
                    InstKind::LoadGlobal { dst, .. }
                    | InstKind::LoadName { dst, .. }
                    | InstKind::AttrLoad { dst, .. } => flow.loads.push((at, *dst, span, false)),
                    InstKind::Import { dst, .. } => flow.loads.push((at, *dst, span, true)),
                    InstKind::Copy { dst, src } => flow.from.entry(*dst).or_default().push(*src),
                    InstKind::AllocBuiltin { dst, elems, .. } => {
                        flow.held.entry(*dst).or_default().extend(elems);
                    }
                    InstKind::DictLiteral { dst, entries } => {
                        let values = entries.iter().filter_map(|e| match e {
                            DictEntry::KeyValue { value, .. } => Some(*value),
                            DictEntry::Unpack { .. } => None,
                        });
                        flow.held.entry(*dst).or_default().extend(values);
                    }
                    // A comprehension frame's arguments are its captures.
                    InstKind::Call {
                        dst,
                        callee_var: Some(_),
                        args,
                        kwargs,
                        ..
                    }
                    | InstKind::MethodCall {
                        dst, args, kwargs, ..
                    } if !names_only_at(at) => {
                        let passed = args.iter().chain(kwargs.iter().map(|(_, v)| v));
                        for v in passed.filter(|v| !handed.contains(&(*dst, **v))) {
                            flow.hand_on(*v, span, false);
                        }
                    }
                    InstKind::AttrStore { value, .. }
                    | InstKind::SubscriptStore { value, .. }
                    | InstKind::Yield {
                        value: Some(value), ..
                    } => flow.hand_on(*value, span, false),
                    InstKind::StoreGlobal { value, .. } => {
                        flow.hand_on(*value, span, f.is_module_body);
                    }
                    _ => {}
                }
            }
            // The builder gives every `return` and `raise` of a value its statement's span.
            if let (Terminator::Return(Some(v)) | Terminator::Raise(Some(v)), Some(span)) =
                (&bb.terminator, bb.term_span)
            {
                flow.hand_on(*v, span, false);
            }
        }
        flow
    }

    fn hand_on(&mut self, var: VarId, span: Span, display_only: bool) {
        self.uses.entry(var).or_default().push((span, display_only));
    }

    /// Whether the module body binds a name to `var` itself.
    fn bound_by_module(&self, var: VarId) -> bool {
        self.uses
            .get(&var)
            .is_some_and(|uses| uses.iter().any(|(_, display_only)| *display_only))
    }

    /// The values whose objects reach a use handing them on: through copies and phis, and as
    /// what a display reaching one holds.
    fn reaching(&self) -> FxHashSet<VarId> {
        let walk = |start: Vec<VarId>, displays: bool| {
            let mut seen: FxHashSet<VarId> = start.iter().copied().collect();
            let mut stack = start;
            while let Some(v) = stack.pop() {
                let inside = displays.then(|| self.held.get(&v)).flatten();
                for &w in self.from.get(&v).into_iter().chain(inside).flatten() {
                    if seen.insert(w) {
                        stack.push(w);
                    }
                }
            }
            seen
        };
        let in_display = walk(self.uses.keys().copied().collect(), true);
        let mut start: Vec<VarId> = self
            .uses
            .iter()
            .filter(|(_, uses)| uses.iter().any(|(_, display_only)| !display_only))
            .map(|(v, _)| *v)
            .collect();
        start.extend(in_display.iter().filter_map(|d| self.held.get(d)).flatten());
        walk(start, false)
    }

    /// Where `var`'s object is handed on: the spans of the uses it reaches, in order.
    fn handed_at(&self, var: VarId) -> Vec<Span> {
        let onward = self.onward.get_or_init(|| {
            let mut onward: FxHashMap<VarId, Vec<(VarId, bool)>> = FxHashMap::default();
            for (to, sources) in &self.from {
                for s in sources {
                    onward.entry(*s).or_default().push((*to, false));
                }
            }
            for (display, values) in &self.held {
                for v in values {
                    onward.entry(*v).or_default().push((*display, true));
                }
            }
            onward
        });
        let mut seen: FxHashSet<(VarId, bool)> = FxHashSet::from_iter([(var, false)]);
        let mut stack = vec![(var, false)];
        let mut out = Vec::new();
        while let Some((v, in_display)) = stack.pop() {
            let uses = self.uses.get(&v).into_iter().flatten();
            out.extend(
                uses.filter(|(_, only)| in_display || !only)
                    .map(|(s, _)| *s),
            );
            for &(w, into) in onward.get(&v).into_iter().flatten() {
                if seen.insert((w, in_display || into)) {
                    stack.push((w, in_display || into));
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
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

/// What a call binds to one project callee's parameters ([`Extractor::hand_over`]).
struct Handed<'b> {
    /// The receiver, unless it is the object a constructor call creates.
    recv: Option<VarId>,
    /// `(parameter, argument)` of the arguments (`*` unpacking binds the ones from
    /// `Call::star_from` on inexactly) and of the keywords naming a parameter.
    bound: &'b [(u8, VarId)],
    /// The parameter of the first positional argument.
    offset: usize,
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
        complete: false,
        on_every_return: crate::OnEveryReturn::default(),
    }
}

/// Decorators that call the function's body on every call (its result stays a new object).
const RETURN_KEEPING_DECORATORS: &[&str] = &[
    "staticmethod",
    "classmethod",
    "property",
    "abstractmethod",
    "override",
];

/// Whether calling a function decorated with `decorators` returns what its body returns: a
/// decorator may hand out one cached object (`@lru_cache`, `@cached_property`), so only the
/// decorators that call the body each time keep its result.
pub(crate) fn keeps_returned_value(decorators: &[String]) -> bool {
    decorators.iter().all(|d| {
        let base = d.split('(').next().unwrap_or(d).trim();
        let last = base.rsplit('.').next().unwrap_or(base);
        RETURN_KEEPING_DECORATORS.contains(&last)
    })
}

/// Builtins that only inspect their arguments: calling one keeps none of them.
const INSPECTING_BUILTINS: &[&str] = &[
    "len",
    "isinstance",
    "issubclass",
    "id",
    "type",
    "hasattr",
    "getattr",
    "object.__getattribute__",
    "callable",
    "repr",
    "str",
    "hash",
    "bool",
    "print",
    "format",
];

/// Calls that only name what they are handed: typing forms, `super`, and the call form of an
/// `except` clause.
const NAMING_CALLS: &[&str] = &[
    "builtins.super",
    "contextlib.suppress",
    "typing.NewType",
    "typing.TypeVar",
    "typing.cast",
    "typing_extensions.NewType",
    "typing_extensions.TypeVar",
    "typing_extensions.cast",
];

/// Whether calling `callee` hands none of its arguments on: a builtin that only inspects them
/// ([`INSPECTING_BUILTINS`]), or a call of [`NAMING_CALLS`].
fn names_only(callee: &Callee) -> bool {
    let Callee::External(symbol) = callee else {
        return false;
    };
    NAMING_CALLS.contains(&symbol.as_str())
        || symbol
            .strip_prefix("builtins.")
            .is_some_and(|n| INSPECTING_BUILTINS.contains(&n))
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
/// What a function may rebind on some object: the attribute names it stores or deletes (also
/// through `setattr` / `delattr` and `__dict__`), `None` when it may set one it does not name;
/// and the constant keys it stores or deletes, `None` when it stores under a key it does not name
/// (`d[k] = v`, `d.update(...)`). An attribute or item of a shallow copy that the function may
/// rebind may hold a value of its own rather than the original's (`param_copy_alias`).
struct Rebound<'f> {
    attrs: Option<FxHashSet<&'f str>>,
    keys: Option<FxHashSet<&'f str>>,
}

impl<'f> Rebound<'f> {
    fn of(f: &'f IrFunction) -> Self {
        let insts = || f.blocks.iter().flat_map(|b| &b.insts);
        let mut strings: FxHashMap<VarId, &'f str> = FxHashMap::default();
        let mut dicts: FxHashSet<VarId> = FxHashSet::default();
        for inst in insts() {
            match &inst.kind {
                InstKind::Const {
                    dst,
                    literal: ConstLiteral::Str(text),
                } => {
                    strings.insert(*dst, text.as_str());
                }
                InstKind::AttrLoad { dst, attr, .. } if attr == "__dict__" => {
                    dicts.insert(*dst);
                }
                _ => {}
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
    let mut found: Option<ObjAttr> = None;
    for v in inputs {
        let Some(a) = map.get(v) else {
            return;
        };
        match &mut found {
            None => {
                found = Some(ObjAttr {
                    direct: false,
                    ..a.clone()
                });
            }
            Some(f) => {
                if f.classes != a.classes || f.attr != a.attr {
                    return;
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
    if let Some(a) = found {
        map.insert(dst, a);
    }
}

/// [`merge_phi`] for container entries: the inputs must name entries of one class attribute
/// (of differing keys: an entry heki cannot name); the objects' parameters are united, as in
/// [`merge_obj_attr_phi`].
fn merge_item_phi(map: &mut FxHashMap<VarId, ItemOf>, dst: VarId, inputs: &[VarId]) {
    if map.contains_key(&dst) {
        return;
    }
    let mut found: Option<ItemOf> = None;
    for v in inputs {
        let Some(a) = map.get(v) else {
            return;
        };
        match &mut found {
            None => found = Some(a.clone()),
            Some(f) => {
                if f.classes != a.classes || f.attr != a.attr {
                    return;
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
    if let Some(a) = found {
        map.insert(dst, a);
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
