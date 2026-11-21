//! `MUT113` facts (docs/ANALYSIS_AND_KB_SPEC.md section 2.3): where a method iterates a
//! container of its own instance (`for k in self.items:`, `yield from self.items.values()`), and
//! how a function changes such containers in place.
//!
//! Python checks a dict, a set, and a deque at every step of an iterator over it: an iterator
//! whose dict or set changed size since it was created raises `RuntimeError` at its next step
//! ("dictionary changed size during iteration"), and so does a deque iterator after any change
//! but an item store or `reverse()`. A list iterator walks indices: it sees appended items and
//! skips or repeats items when others move, without an error, so a list is no such container.
//!
//! The iterations are read from the IR when the method is summarized (local facts, IR-free
//! afterwards); the changes are propagated through the methods a function calls on `self` and
//! the helpers it hands a container to (`callgraph`).

use crate::consumption::{Consumption, IterFlow, resolve_inst};
use crate::resolver::{Callee, TypeCtx, Val};
use crate::{BoundCallee, CallSite, EffectSummary, Facts, FuncKey, ProjectIndex};
use heki_ast::Span;
use heki_ir::{BlockId, Inst, InstKind, IrFunction, PhiNode, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A function's `MUT113` facts (`EffectSummary::iterations`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IterationFacts {
    /// The `self` attributes holding a container of a class [`IteratedContainer`] names that the
    /// function changes in place (rebinding one changes no container), directly, through local
    /// copies, through the methods and properties it calls on `self`, and through the helpers it
    /// hands them to, with the strongest change. Propagated to callers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub changes_self_containers: BTreeMap<String, ContainerChange>,
    /// The parameters (by position) whose own container the function changes in place, with the
    /// strongest change, counted as for `changes_self_containers` (whatever the parameter's
    /// class: its callers' arguments decide whether their iterators check): directly, through
    /// local copies, and through the helpers it hands them to. Propagated to callers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub changes_param_containers: BTreeMap<u8, ContainerChange>,
    /// The spans of the function's own changes of the containers in `changes_self_containers`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub self_container_change_sites: BTreeMap<String, BTreeSet<Span>>,
    /// A method's iterations over the containers of its own instance that a generator suspends
    /// in or whose body makes calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub container_iterations: Vec<ContainerIteration>,
    /// Where the function iterates the objects its project calls return (a generator, or one a
    /// plain function hands on), with what runs between their steps: what can run while such a
    /// generator is suspended. One per iteration, with every call whose result it steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub result_iterations: Vec<ResultIteration>,
}

/// The facts of a function with none.
pub(crate) static NO_ITERATION_FACTS: IterationFacts = IterationFacts {
    changes_self_containers: BTreeMap::new(),
    changes_param_containers: BTreeMap::new(),
    self_container_change_sites: BTreeMap::new(),
    container_iterations: Vec::new(),
    result_iterations: Vec::new(),
};

impl IterationFacts {
    /// The facts without source positions (Early Cutoff).
    pub(crate) fn without_locations(&self) -> Self {
        let zero = Span { start: 0, end: 0 };
        let mut out = Self {
            self_container_change_sites: BTreeMap::new(),
            ..self.clone()
        };
        for it in &mut out.container_iterations {
            it.span = zero;
            it.between.calls.fill(zero);
        }
        for used in &mut out.result_iterations {
            used.between.calls.fill(zero);
            for result in &mut used.results {
                result.call = zero;
            }
        }
        out
    }

    /// The facts callers copy (`changes_self_containers`, `changes_param_containers`), `None`
    /// without any.
    pub(crate) fn propagated(&self) -> Option<Box<Self>> {
        let none =
            self.changes_self_containers.is_empty() && self.changes_param_containers.is_empty();
        (!none).then(|| {
            Box::new(Self {
                changes_self_containers: self.changes_self_containers.clone(),
                changes_param_containers: self.changes_param_containers.clone(),
                ..Self::default()
            })
        })
    }

    /// Add the facts of another definition of the function.
    pub(crate) fn merge(&mut self, other: Self) {
        merge_changes(
            &mut self.changes_self_containers,
            other.changes_self_containers,
        );
        merge_changes(
            &mut self.changes_param_containers,
            other.changes_param_containers,
        );
        for (attr, spans) in other.self_container_change_sites {
            self.self_container_change_sites
                .entry(attr)
                .or_default()
                .extend(spans);
        }
        self.container_iterations.extend(other.container_iterations);
        self.container_iterations.sort();
        self.container_iterations.dedup();
        self.result_iterations.extend(other.result_iterations);
        self.result_iterations.sort();
        self.result_iterations.dedup();
    }
}

/// A container class whose iterators raise `RuntimeError` after it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IteratedContainer {
    /// `dict`, `OrderedDict`, `defaultdict`, `Counter`, and their `keys()` / `values()` /
    /// `items()` views.
    Mapping,
    Set,
    Deque,
}

impl IteratedContainer {
    /// The checked container an instance of `class` is (`typing` aliases of builtins resolve to
    /// the builtin). Abstract types (`MutableMapping`, `MutableSet`) promise no such check.
    pub fn of_class(class: &str) -> Option<Self> {
        match class {
            "builtins.dict"
            | "collections.OrderedDict"
            | "collections.defaultdict"
            | "collections.Counter"
            | "typing.Counter" => Some(Self::Mapping),
            "builtins.set" => Some(Self::Set),
            "collections.deque" | "typing.Deque" => Some(Self::Deque),
            _ => None,
        }
    }

    /// The checked container a value of type `val` is: one class, or a union whose classes
    /// besides `None` agree.
    pub(crate) fn of_val(val: &Val) -> Option<Self> {
        match val {
            Val::Inst(class) => Self::of_class(class),
            Val::Union(classes) => {
                let mut kinds = classes
                    .iter()
                    .filter(|c| c.as_str() != "builtins.NoneType")
                    .map(|c| Self::of_class(c));
                let first = kinds.next()??;
                kinds.all(|k| k == Some(first)).then_some(first)
            }
            _ => None,
        }
    }

    /// Whether a change makes the next step of an iterator over the container raise: any change
    /// of a mapping (a stored item may be a new key), and any but an item store of a set or a
    /// deque.
    pub fn broken_by(self, change: ContainerChange) -> bool {
        match change {
            ContainerChange::StoresItems => self == Self::Mapping,
            ContainerChange::Resizes => true,
        }
    }
}

/// What a function's in-place changes of a container can do to the iterators over it, the
/// weaker first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerChange {
    /// Only items stored under a key or an index (`x[k] = v`): a mapping gains the key when it
    /// is new.
    StoresItems,
    /// Items inserted, removed, or moved (`append`, `add`, `update`, `setdefault`, `pop`,
    /// `del x[k]`, `clear`, `rotate`, `move_to_end`, the in-place operators).
    Resizes,
}

/// Merge the changes `from` into `into`, keeping the stronger one per container.
pub(crate) fn merge_changes<K: Ord>(
    into: &mut BTreeMap<K, ContainerChange>,
    from: impl IntoIterator<Item = (K, ContainerChange)>,
) {
    for (container, change) in from {
        let slot = into.entry(container).or_insert(change);
        *slot = (*slot).max(change);
    }
}

/// The containers of the calling method's own instance that the project callee `bc` (summarized
/// by `callee`) changes in place when `facts` run at `site`: those it changes on its own `self`
/// when the call is on the caller's `self` (`self.helper()`, `super().m()`, a property read), and
/// the caller's `self.<attr>` handed to a parameter whose container it changes, with that change
/// (a helper storing an item of a deque changes no more than the store would here).
pub(crate) fn call_changes(
    site: &CallSite,
    bc: &BoundCallee,
    callee: &EffectSummary,
    facts: &EffectSummary,
) -> Vec<(String, ContainerChange)> {
    let mut out = Vec::new();
    if site.recv_is_self && callee.owner_class_qualname.is_some() {
        out.extend(
            facts
                .iteration_facts()
                .changes_self_containers
                .iter()
                .map(|(attr, change)| (attr.clone(), *change)),
        );
    }
    for (&i, &change) in &facts.iteration_facts().changes_param_containers {
        if let Some(attr) = bc
            .params
            .get(usize::from(i))
            .and_then(|o| o.self_attr.as_ref())
        {
            out.push((attr.clone(), change));
        }
    }
    out
}

/// What the project callees of `site` change in place of the calling method's own containers
/// while the caller's body runs them, by callee ([`ProjectIndex::delivery`]).
pub fn changed_at<'s>(
    index: &ProjectIndex,
    site: &'s CallSite,
) -> Vec<(&'s FuncKey, String, ContainerChange)> {
    let running = site.project_callees().iter().filter_map(|bc| {
        let callee = index.summary(&bc.callee)?;
        let d = index
            .delivery(site, callee)
            .filter(|d| d.into == Facts::Body)?;
        Some((bc, callee, callee.facts(d.from)?))
    });
    running
        .flat_map(|(bc, callee, facts)| {
            call_changes(site, bc, callee, facts)
                .into_iter()
                .map(|(attr, change)| (&bc.callee, attr, change))
        })
        .collect()
}

/// The change the container method `method` makes in place (`None`: it changes nothing an
/// iterator checks, as `sort()` and `reverse()` of a list or a deque, or is none). A method of a
/// project class is its own summary's.
pub(crate) fn method_change(method: &str) -> Option<ContainerChange> {
    match method {
        "__setitem__" => Some(ContainerChange::StoresItems),
        "append"
        | "appendleft"
        | "extend"
        | "extendleft"
        | "insert"
        | "add"
        | "update"
        | "setdefault"
        | "pop"
        | "popleft"
        | "popitem"
        | "remove"
        | "discard"
        | "clear"
        | "difference_update"
        | "intersection_update"
        | "symmetric_difference_update"
        | "rotate"
        | "move_to_end"
        | "__delitem__" => Some(ContainerChange::Resizes),
        _ => None,
    }
}

/// A `for` loop or a `yield from` of a method over the containers its instance's attributes
/// hold, directly or through a view or a lazy wrapper (`self.items.values()`,
/// `enumerate(self.items)`, `zip(self.a, self.b)`); a snapshot (`list(self.items)`,
/// `self.items.copy()`) is a container of its own.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContainerIteration {
    /// The attributes and their containers the iteration may step, in order, at least one. The
    /// loop keeps one record of what runs between its steps for all of them.
    pub iterated: Vec<(String, IteratedContainer)>,
    /// The `for` loop's iterable, or the `yield from` expression.
    pub span: Span,
    pub between: BetweenSteps,
}

/// An iteration of the objects project calls return, in the calling function: a `for` loop, a
/// `yield from`, or a call consuming them (`list(...)`, `"".join(...)`), with the calls whose
/// results it steps (`zip(a(), b())` steps both).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ResultIteration {
    pub between: BetweenSteps,
    /// For each call in `between.calls`, the object it binds to its own callees' first
    /// parameter (a method's `self`, a property's object), numbered as the objects of
    /// `results`' arguments are (`None`: none of them). Empty when no call is on such an object.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receivers: Vec<Option<u32>>,
    /// The calls whose results it steps, by span.
    pub results: Vec<IteratedResult>,
}

/// A call whose result a [`ResultIteration`] steps.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IteratedResult {
    /// The call.
    pub call: Span,
    /// The parameters of the call's callees bound to an object one of the iteration's
    /// `receivers` is, with its number: the same value through copies (`super()` is `self`), or
    /// the same attribute of one. A generator iterating a container of that parameter's object
    /// runs those calls on its own instance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<(u8, u32)>,
}

/// What runs between two steps of an iteration.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BetweenSteps {
    /// The iterating function is a generator that suspends inside the iteration and resumes it
    /// later: at a `yield` in the loop's body from which the loop goes on, or inside the
    /// `yield from` itself.
    pub suspends: bool,
    /// The call sites in the loop's body from which the loop goes on (calls, reads of
    /// properties, which run their getters, and iterations running a generator's body), by span.
    pub calls: Vec<Span>,
}

impl BetweenSteps {
    /// What runs between the steps of the iteration at instruction `inst` of `f` (a `for` loop's
    /// `MakeIter`, a `yield from`, or a consuming call, which runs nothing); `None` for a loop
    /// without a natural loop. `site_spans` are the spans of `f`'s call sites.
    pub(crate) fn of(
        f: &IrFunction,
        inst: &Inst,
        site_spans: &FxHashSet<Span>,
        is_generator: bool,
    ) -> Option<Self> {
        let iter = match &inst.kind {
            InstKind::YieldFrom { .. } => {
                return Some(Self {
                    suspends: true,
                    calls: Vec::new(),
                });
            }
            InstKind::MakeIter { dst, .. } => *dst,
            _ => return Some(Self::default()),
        };
        let body = continuing_body(f, iter)?;
        let in_body = body.iter().flat_map(|b| &f.block(*b).insts);
        let suspends = is_generator
            && in_body
                .clone()
                .any(|i| matches!(i.kind, InstKind::Yield { .. } | InstKind::YieldFrom { .. }));
        let mut calls: Vec<Span> = in_body
            .map(|i| i.span)
            .filter(|s| site_spans.contains(s))
            .collect();
        calls.sort();
        calls.dedup();
        Some(Self { suspends, calls })
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.suspends && self.calls.is_empty()
    }
}

/// What a use of a project call's result does with the object.
enum ResultUse {
    /// Nothing that steps it or lets it out: a copy, a comparison, a lazy wrapper made of it
    /// (the wrapper's own uses are the object's).
    Inert,
    /// It escapes: stored, yielded, captured, handed to code heki does not follow, or returned
    /// by a generator or a coroutine. Any later code may step it.
    Escapes,
    /// It is stepped to exhaustion, returned by a plain function, or passed to a project call.
    Uses,
    /// A step that may leave a generator suspended, resumed by a use reachable from `Resume`.
    Steps(Resume),
}

/// Where the code after a step that may leave a generator suspended starts.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Resume {
    /// After the instruction at `(block, index)`.
    After(BlockId, usize),
    /// A `for` loop: on leaving its body other than through its `header` (the loop's next step).
    /// `exhausted`: the header's edge taken once the iterator ends, which exhausts the object.
    Loop {
        header: BlockId,
        exhausted: Option<BlockId>,
    },
}

/// The project call results among `results` (`flow` roots) whose object outlives one of the
/// function's uses of it: it escapes, or a step that may leave it suspended (`next()`, a method of
/// it, a `for` loop that leaves early or over a wrapper that may stop first, `any()`) is followed
/// by another use. The code between them runs while a generator is suspended. A `plain` function
/// (neither a generator nor a coroutine) hands a returned result on to its caller.
pub(crate) fn held_results(
    f: &IrFunction,
    flow: &IterFlow,
    results: &FxHashSet<VarId>,
    plain: bool,
) -> FxHashSet<VarId> {
    let carried = |v: VarId| -> Vec<VarId> {
        if !flow.root_of.contains_key(&v) && !flow.may_roots.contains_key(&v) {
            return Vec::new();
        }
        let mut roots = flow.expand_steps(&[v]);
        roots.retain(|r| results.contains(r));
        roots
    };
    let events: FxHashMap<(BlockId, usize), Vec<&Consumption>> =
        flow.events.iter().fold(FxHashMap::default(), |mut by, e| {
            by.entry((e.block, e.index)).or_default().push(e);
            by
        });
    let mut held: FxHashSet<VarId> = FxHashSet::default();
    let mut uses: FxHashMap<VarId, Vec<(BlockId, usize)>> = FxHashMap::default();
    let mut steps: FxHashMap<VarId, Vec<Resume>> = FxHashMap::default();
    for bb in &f.blocks {
        for (index, inst) in bb.insts.iter().enumerate() {
            let at = (bb.id, index);
            let mut facts: Option<InstFacts> = None;
            for v in inst.kind.operands() {
                for r in carried(v) {
                    let facts = facts.get_or_insert_with(|| {
                        InstFacts::of(
                            f,
                            flow,
                            inst,
                            events.get(&at).map_or(&[][..], Vec::as_slice),
                        )
                    });
                    match result_use(flow, at, inst, facts, v, r) {
                        ResultUse::Inert => {}
                        ResultUse::Escapes => {
                            held.insert(r);
                        }
                        ResultUse::Uses => uses.entry(r).or_default().push(at),
                        ResultUse::Steps(resume) => {
                            uses.entry(r).or_default().push(at);
                            steps.entry(r).or_default().push(resume);
                        }
                    }
                }
            }
        }
        if let Terminator::Return(Some(v)) = bb.terminator {
            for r in carried(v) {
                if plain {
                    uses.entry(r).or_default().push((bb.id, bb.insts.len()));
                } else {
                    held.insert(r);
                }
            }
        }
    }
    // Results stepped by one instruction share their steps and uses: each walk is made once.
    let mut walked: FxHashMap<Walk<'_>, bool> = FxHashMap::default();
    for (r, resumes) in &steps {
        let later = uses[r].as_slice();
        let resumes_later = |resume: &Resume| {
            *walked
                .entry((*resume, later))
                .or_insert_with(|| resumed(f, *resume, later))
        };
        if resumes.iter().any(resumes_later) {
            held.insert(*r);
        }
    }
    held
}

/// A walk of [`resumed`]: from a step, to the later uses of a result.
type Walk<'u> = (Resume, &'u [(BlockId, usize)]);

/// A `for` loop's header, with the edge it takes once the iterator ends.
type LoopHeader = (BlockId, Option<BlockId>);

/// What one instruction does to the project call results its operands carry, found once for all
/// of them: a `zip(...)` of many results is one instruction.
struct InstFacts {
    /// Its iterations (`flow.events` there): the results each steps, those it exhausts, and whether
    /// it runs to exhaustion.
    events: Vec<(FxHashSet<VarId>, FxHashSet<VarId>, bool)>,
    /// The results the value it defines steps when that is iterated (a lazy wrapper of them).
    wrapped: FxHashSet<VarId>,
    /// A `for` loop's: the results its end exhausts, and its header with the edge taken at the
    /// end (`None` without a natural loop).
    loop_end: Option<(FxHashSet<VarId>, Option<LoopHeader>)>,
}

impl InstFacts {
    fn of(f: &IrFunction, flow: &IterFlow, inst: &Inst, events: &[&Consumption]) -> Self {
        let set = |roots: Vec<VarId>| -> FxHashSet<VarId> { roots.into_iter().collect() };
        let loop_end = match &inst.kind {
            InstKind::MakeIter { dst, iterable, .. } => {
                Some((set(flow.expand(&[*iterable])), loop_header(f, *dst)))
            }
            _ => None,
        };
        Self {
            events: events
                .iter()
                .map(|e| (set(e.stepped.clone()), set(e.roots.clone()), e.full))
                .collect(),
            wrapped: inst
                .kind
                .dst()
                .map(|d| set(flow.expand_steps(&[d])))
                .unwrap_or_default(),
            loop_end,
        }
    }
}

/// What the instruction `inst` at `(block, index)` does with the project call result `r`, which
/// its operand `v` carries (`facts`: what it does to all of them).
fn result_use(
    flow: &IterFlow,
    (block, index): (BlockId, usize),
    inst: &Inst,
    facts: &InstFacts,
    v: VarId,
    r: VarId,
) -> ResultUse {
    let after = Resume::After(block, index);
    if let Some((exhausted, header)) = &facts.loop_end {
        let resume = header.map(|(header, end)| Resume::Loop {
            header,
            exhausted: end.filter(|_| exhausted.contains(&r)),
        });
        return ResultUse::Steps(resume.unwrap_or(after));
    }
    if let Some((_, exhausted, full)) = facts.events.iter().find(|(s, _, _)| s.contains(&r)) {
        return if *full && exhausted.contains(&r) {
            ResultUse::Uses
        } else {
            ResultUse::Steps(after)
        };
    }
    match &inst.kind {
        InstKind::Copy { .. }
        | InstKind::Compare { .. }
        | InstKind::UnaryOp { .. }
        | InstKind::Delete { .. } => ResultUse::Inert,
        _ if facts.wrapped.contains(&r) => ResultUse::Inert,
        // A generator's own methods step it (`send()`, `throw()`).
        InstKind::MethodCall { recv, .. } if *recv == v => ResultUse::Steps(after),
        InstKind::Call { .. } | InstKind::MethodCall { .. } => {
            match flow.callees.get(&(block, index)) {
                Some(Callee::External(s)) if s == "builtins.next" => ResultUse::Steps(after),
                Some(Callee::Project(..)) => ResultUse::Uses,
                _ => ResultUse::Escapes,
            }
        }
        _ => ResultUse::Escapes,
    }
}

/// The header of the `for` loop stepping the iterator `iter`, with the edge it takes once the
/// iterator ends (`None` without such a loop).
fn loop_header(f: &IrFunction, iter: VarId) -> Option<LoopHeader> {
    let header = f.blocks.iter().find(|b| {
        b.insts
            .iter()
            .any(|i| matches!(i.kind, InstKind::IterNext { iter: it, .. } if it == iter))
    })?;
    // The header branches to the loop's body, else to where the loop ends.
    Some((header.id, header.terminator.successors().get(1).copied()))
}

/// Whether one of `uses` (positions; a block's terminator is at its instruction count) can run
/// after the step that `resume` continues, on normal or exception edges.
fn resumed(f: &IrFunction, resume: Resume, uses: &[(BlockId, usize)]) -> bool {
    let next = |b: BlockId| {
        let block = f.block(b);
        let mut succs = block.terminator.successors();
        succs.extend(block.exc_target);
        succs
    };
    let mut seen: FxHashSet<BlockId> = FxHashSet::default();
    let mut stack = match resume {
        Resume::After(block, index) => {
            if uses.iter().any(|&(b, i)| b == block && i > index) {
                return true;
            }
            next(block)
        }
        Resume::Loop { header, exhausted } => {
            seen.insert(header);
            let mut succs = next(header);
            succs.retain(|s| Some(*s) != exhausted && Some(*s) != f.block(header).exc_target);
            succs
        }
    };
    while let Some(b) = stack.pop() {
        if !seen.insert(b) {
            continue;
        }
        if uses.iter().any(|&(u, _)| u == b) {
            return true;
        }
        stack.extend(next(b));
    }
    false
}

/// The iterations of the method `f` over the checked containers of its own instance
/// (`self_attrs` maps the variables holding `self.<attr>` to the attribute) that a generator
/// suspends in or whose body makes calls. `site_spans` are the spans of `f`'s call sites. The
/// `(iteration, container)` facts share the budget of [`live_containers`]' facts.
pub(crate) fn container_iterations(
    f: &IrFunction,
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    self_attrs: &FxHashMap<VarId, String>,
    site_spans: &FxHashSet<Span>,
    is_generator: bool,
) -> Vec<ContainerIteration> {
    if self_attrs.is_empty() {
        return Vec::new();
    }
    let live = live_containers(f, tcx, types, self_attrs);
    if live.is_empty() {
        return Vec::new();
    }
    // One `zip` of many containers stepped by many `yield from`s gives each iteration every
    // container: past the budget of facts the method has no iterations, as past the live ones.
    let mut budget = live_facts_budget(f);
    let mut out = Vec::new();
    for inst in f.blocks.iter().flat_map(|b| &b.insts) {
        let iterated = match &inst.kind {
            InstKind::YieldFrom { value, .. } => live.get(value),
            InstKind::MakeIter {
                iterable,
                is_async: false,
                ..
            } => live.get(iterable),
            _ => None,
        };
        let between = iterated.and_then(|_| BetweenSteps::of(f, inst, site_spans, is_generator));
        let (Some(iterated), Some(between)) = (iterated, between.filter(|b| !b.is_empty())) else {
            continue;
        };
        let Some(left) = budget.checked_sub(iterated.len()) else {
            return Vec::new();
        };
        budget = left;
        out.push(ContainerIteration {
            iterated: iterated.iter().cloned().collect(),
            span: inst.span,
            between,
        });
    }
    out.sort();
    out.dedup();
    out
}

/// The checked containers of `self` (attribute and class) a value may step an iterator over.
type Live = FxHashMap<VarId, BTreeSet<(String, IteratedContainer)>>;

/// The passes [`live_containers`] takes at most: a container a loop passes back through a chain
/// of copies takes one pass per link, and real code takes a few.
const MAX_LIVE_PASSES: usize = 24;

/// The `(value, container)` facts [`live_containers`] keeps at most, and the
/// `(iteration, container)` facts [`container_iterations`] keeps at most: a multiple of the
/// function's source size, within bounds, far above what real code keeps. A loop rotating
/// aliases of many containers, or a chain of merges of them, gives each value every container.
const LIVE_FACTS_PER_SOURCE_BYTE: usize = 1;
const MIN_LIVE_FACTS: usize = 4096;
const MAX_LIVE_FACTS: usize = 100_000;

fn live_facts_budget(f: &IrFunction) -> usize {
    let source_bytes = f.span.end.saturating_sub(f.span.start) as usize;
    (source_bytes * LIVE_FACTS_PER_SOURCE_BYTE).clamp(MIN_LIVE_FACTS, MAX_LIVE_FACTS)
}

/// The values whose iteration steps an iterator over a checked container of `self`: the
/// container itself (through copies), its `keys()` / `values()` / `items()` views, lazy wrappers
/// of either (`enumerate`, `zip`, `reversed`, `itertools.chain`, a generator expression), and
/// the iterators made from them. A merged value may be any of its inputs. Past the pass limit or
/// the budget of facts the facts are not final: none is kept, and the method has no iterations.
fn live_containers(
    f: &IrFunction,
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    self_attrs: &FxHashMap<VarId, String>,
) -> Live {
    let mut live: Live = FxHashMap::default();
    for (var, attr) in self_attrs {
        if let Some(container) = types.get(var).and_then(IteratedContainer::of_val) {
            live.entry(*var)
                .or_default()
                .insert((attr.clone(), container));
        }
    }
    if live.is_empty() {
        return live;
    }
    let budget = live_facts_budget(f);
    let mut facts: usize = live.values().map(BTreeSet::len).sum();
    for _ in 0..MAX_LIVE_PASSES {
        let before = facts;
        for bb in &f.blocks {
            let links = bb.phis.iter().map(Link::Phi);
            for link in links.chain(bb.insts.iter().map(Link::Inst)) {
                let Some((dst, found)) = flows(tcx, types, &live, link) else {
                    continue;
                };
                facts += join(&mut live, dst, found);
                if facts > budget {
                    return Live::default();
                }
            }
        }
        if facts == before {
            return live;
        }
    }
    Live::default()
}

/// A phi or an instruction, through which values flow in [`live_containers`].
enum Link<'f> {
    Phi(&'f PhiNode),
    Inst(&'f Inst),
}

/// The value `link` defines and the containers it may iterate from its inputs (`None`: it
/// iterates none of theirs).
fn flows(
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    live: &Live,
    link: Link<'_>,
) -> Option<(VarId, BTreeSet<(String, IteratedContainer)>)> {
    let union = |sources: &[VarId]| -> BTreeSet<(String, IteratedContainer)> {
        sources
            .iter()
            .filter_map(|v| live.get(v))
            .flatten()
            .cloned()
            .collect()
    };
    let inst = match link {
        Link::Phi(phi) => {
            let inputs: Vec<VarId> = phi.incoming.iter().map(|(_, v)| *v).collect();
            return Some((phi.dst, union(&inputs)));
        }
        Link::Inst(inst) => inst,
    };
    match &inst.kind {
        InstKind::Copy { dst, src } => Some((*dst, union(&[*src]))),
        InstKind::MakeIter { dst, iterable, .. } => Some((*dst, union(&[*iterable]))),
        InstKind::MakeGenExpr {
            dst, outer_iter, ..
        } => Some((*dst, union(&[*outer_iter]))),
        InstKind::MethodCall {
            dst,
            recv,
            method,
            args,
            kwargs,
            ..
        } if args.is_empty()
            && kwargs.is_empty()
            && matches!(method.as_str(), "keys" | "values" | "items") =>
        {
            let mut views = union(&[*recv]);
            views.retain(|(_, c)| *c == IteratedContainer::Mapping);
            Some((*dst, views))
        }
        // `itertools.chain(...)` is a method call on the module.
        InstKind::Call { dst, args, .. } | InstKind::MethodCall { dst, args, .. }
            if args.iter().any(|a| live.contains_key(a)) =>
        {
            match resolve_inst(tcx, types, &inst.kind) {
                Callee::External(symbol) => Some((*dst, union(&lazily_iterated(&symbol, args)))),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Add the containers `found` to those `dst` may iterate; the number of new facts.
fn join(live: &mut Live, dst: VarId, found: BTreeSet<(String, IteratedContainer)>) -> usize {
    if found.is_empty() {
        return 0;
    }
    let slot = live.entry(dst).or_default();
    let before = slot.len();
    slot.extend(found);
    slot.len() - before
}

/// The arguments iterating the result of the builtin or `itertools` call `symbol` steps while
/// they stay live: `iter(x)`, `reversed(x)`, and the lazy wrappers. A consuming call (`list(x)`,
/// `sorted(x)`) makes a snapshot.
fn lazily_iterated(symbol: &str, args: &[VarId]) -> Vec<VarId> {
    let from = match symbol {
        "builtins.iter" if args.len() == 1 => 0,
        "builtins.reversed"
        | "builtins.enumerate"
        | "builtins.zip"
        | "itertools.chain"
        | "itertools.islice"
        | "itertools.zip_longest"
        | "itertools.pairwise"
        | "itertools.batched"
        | "itertools.accumulate"
        | "itertools.groupby"
        | "itertools.compress" => 0,
        "builtins.map"
        | "builtins.filter"
        | "itertools.starmap"
        | "itertools.takewhile"
        | "itertools.dropwhile"
        | "itertools.filterfalse" => 1,
        _ => return Vec::new(),
    };
    args.get(from..).unwrap_or_default().to_vec()
}

/// The blocks of the `for` loop stepping the iterator `iter` from which the loop goes on to its
/// next step: those reaching the loop's header without leaving the loop, on normal or exception
/// edges.
fn continuing_body(f: &IrFunction, iter: VarId) -> Option<Vec<BlockId>> {
    let lp = f.loops.iter().find(|lp| {
        f.block(lp.header)
            .insts
            .iter()
            .any(|i| matches!(i.kind, InstKind::IterNext { iter: it, .. } if it == iter))
    })?;
    let in_loop: FxHashSet<BlockId> = lp.body.iter().copied().collect();
    // The loop's own edges reversed: one walk back from the header finds every block reaching it.
    let mut preds: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
    for &b in &lp.body {
        let block = f.block(b);
        let mut succs = block.terminator.successors();
        succs.extend(block.exc_target);
        for s in succs {
            if s == lp.header || in_loop.contains(&s) {
                preds.entry(s).or_default().push(b);
            }
        }
    }
    let mut reaching: FxHashSet<BlockId> = FxHashSet::default();
    let mut stack = vec![lp.header];
    while let Some(b) = stack.pop() {
        for &p in preds.get(&b).into_iter().flatten() {
            if p != lp.header && reaching.insert(p) {
                stack.push(p);
            }
        }
    }
    Some(
        lp.body
            .iter()
            .copied()
            .filter(|b| reaching.contains(b))
            .collect(),
    )
}
