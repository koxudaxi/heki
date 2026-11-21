//! Queue capacities (docs/ANALYSIS_AND_KB_SPEC.md section 2.2): a `put` waits only while its
//! queue is full (`heki_kb::EcosystemEntry::waits_while_full`), which only a queue that its
//! `maxsize` bounds can be (`heki_kb::EcosystemEntry::bounded_by`).
//!
//! What a value gives a queue ([`CapacityOf`]) is its own capacity for a queue, and that of a
//! queue built with it as `maxsize` for an integer. The code of one function fixes part of it:
//! a construction, an integer constant, `None` (no queue at all). It takes the rest from the
//! project: what the own bodies and methods of a class family store in an attribute
//! (`ClassDecl::attr_capacities`) and what a module binds a global to
//! (`ModuleDecls::global_capacities`), which the declarations resolve
//! (`ProjectDecls::attr_capacity`, `ProjectDecls::global_capacity`), and what the project callers
//! bind to a parameter, resolved over every call site ([`ParamCapacities`]).
//!
//! A queue is known to be bounded when every value that may reach it is known and one of them
//! is bounded. A value heki cannot see (a call result, a parameter some caller binds to an
//! unknown object, a function without project callers) makes it unknown, and a wait on it does
//! not count.

use crate::resolver::{
    DeclKeys, ModuleDecls, ProjectDecls, TypeCtx, func_qualname, is_summarized_function,
    passed_var, self_aliases,
};
use crate::types::Ty;
use crate::{
    BoundCallee, CallHop, CallSite, ConstClass, EffectGuards, EffectSummary, FuncKey, ModuleInput,
    ProjectIndex, SiteRole, guards,
};
use heki_ir::walk::Answer;
use heki_ir::{BlockId, ConstLiteral, FnIndex, Inst, InstKind, IrFunction, PhiNode, VarId};
use heki_kb::{ArgSlot, EffectiveEcosystemKb, ExecEffectKind};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// What the code establishes about how many items a queue holds. The variants are ordered by
/// the join: a queue that may be either is bounded on some path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bound {
    /// No limit: no `maxsize`, or one that is not above zero.
    Unbounded,
    /// A limit on some path.
    Bounded,
}

impl Bound {
    /// The bound of a queue built with a `maxsize` that is above zero, or not.
    fn of_positive(positive: bool) -> Self {
        if positive {
            Self::Bounded
        } else {
            Self::Unbounded
        }
    }
}

/// The capacity of a queue: established over every path that may reach it, or open when some
/// path is not known.
pub type Capacity = Answer<Bound>;

/// What the paths of `a` and `b` together give a queue (`None`: no queue on them): open when
/// either is, since one unknown path leaves the queue unknown, otherwise bounded when either is.
fn join(a: Option<Capacity>, b: Option<Capacity>) -> Option<Capacity> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.join_with(b, Bound::max)),
        (one, None) | (None, one) => one,
    }
}

/// A parameter of a function, by position.
pub type ParamOf = (FuncKey, u8);

/// What a value gives a queue, path by path: what the code fixes, and the project facts it
/// takes the rest from. A summary's values name parameters only; the declarations' name the
/// attributes and globals they store too, which the declarations resolve.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CapacityOf {
    /// The join of what the code fixes (`None`: nothing, such as only `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed: Option<Capacity>,
    /// What the project callers bind to these parameters ([`ParamCapacities`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub params: BTreeSet<ParamOf>,
    /// What the class family of each class stores in the attribute of an instance.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub attrs: BTreeSet<(String, String)>,
    /// What the module of each global binds it to, by fqn.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub globals: BTreeSet<String>,
}

impl CapacityOf {
    pub fn fixed(c: Capacity) -> Self {
        Self {
            fixed: Some(c),
            ..Self::default()
        }
    }

    pub fn unknown() -> Self {
        Self::fixed(Answer::Open)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self.fixed, Some(Answer::Open))
    }

    /// What either value gives. An unknown path makes it unknown, whatever the others are.
    pub fn join(&mut self, other: &Self) {
        self.fixed = join(self.fixed, other.fixed);
        self.params.extend(other.params.iter().cloned());
        self.attrs.extend(other.attrs.iter().cloned());
        self.globals.extend(other.globals.iter().cloned());
        if self.is_unknown() {
            *self = Self::unknown();
        }
    }

    /// What the code alone decides, when it depends on no project fact.
    pub fn decided(&self) -> Option<Option<Capacity>> {
        let open = !(self.params.is_empty() && self.attrs.is_empty() && self.globals.is_empty());
        (self.is_unknown() || !open).then_some(self.fixed)
    }
}

/// The blocking effects of a call that waits only while its receiver, a queue, is full
/// (`heki_kb::EcosystemEntry::waits_while_full`), with the capacity of the receiver, which the
/// project decides ([`SiteQueues::while_full`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhileFull {
    pub effects: BTreeSet<ExecEffectKind>,
    pub capacity: CapacityOf,
}

/// What a call knows of queues (`CallSite::queues`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteQueues {
    /// A Layer 2 call waiting only on a bounded queue, whose capacity the project decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub while_full: Option<WhileFull>,
    /// What the queues the call binds exactly to its project callees' parameters give them:
    /// `(callee, parameter, capacity)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handed: Vec<(FuncKey, u8, CapacityOf)>,
}

/// A function's own [`WhileFull`] call: the guards of the occurrence and the call's hop, for
/// the effects that run when the project shows the queue bounded ([`ParamCapacities::realize`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityWait {
    pub wait: WhileFull,
    pub guards: EffectGuards,
    pub hop: CallHop,
}

// -------------------------------------------------------------------------------- one function

/// What a walk over a value found ([`ValueCapacity::walk`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct Walked {
    pub capacity: CapacityOf,
    /// It reached what may be a queue: a construction of a catalogued queue class, a parameter
    /// annotated with one, or (in the declarations) an attribute or a global, which the
    /// project resolves.
    pub queue: bool,
    /// It reached an integer constant.
    pub int: bool,
}

impl Walked {
    fn of(capacity: CapacityOf) -> Self {
        Self {
            capacity,
            ..Self::default()
        }
    }

    fn join(&mut self, other: &Self) {
        self.capacity.join(&other.capacity);
        self.queue |= other.queue;
        self.int |= other.int;
    }
}

/// The values of one function walked so far ([`ValueCapacity::walk`]): every value is walked
/// once.
pub(crate) type Walks = FxHashMap<VarId, Walked>;

/// The capacities the values of one function give queues, read where the function is read:
/// in its declarations (`in_decls`: attributes and globals stay named) or in its summary
/// (resolved through the project's declarations).
pub(crate) struct ValueCapacity<'c, 'a> {
    pub cx: &'c TypeCtx<'a>,
    pub f: &'a IrFunction,
    pub key: &'c FuncKey,
    pub defs: &'c FxHashMap<VarId, &'a Inst>,
    pub phis: &'c FxHashMap<VarId, (BlockId, &'a PhiNode)>,
    /// The function's integer constants (`FnIndex::const_int`), built on first use.
    pub ints: &'c OnceCell<FnIndex<'a>>,
    /// The project class of the object a value holds, when the function knows it (`self` in a
    /// method).
    pub class_of: &'c dyn Fn(VarId) -> Option<String>,
    /// The parameters annotated with a catalogued queue class (bit `i`: parameter `i < 64`).
    pub queue_params: u64,
    pub in_decls: bool,
    pub walks: &'c RefCell<Walks>,
}

/// One value of a walk in progress (Tarjan's algorithm over what values come from): its
/// position, the least position it reaches back to, whether it is on the stack, what it is
/// itself, and the values it comes from.
struct Visit {
    index: usize,
    low: usize,
    on_stack: bool,
    own: Walked,
    sources: Vec<VarId>,
}

impl<'c, 'a> ValueCapacity<'c, 'a> {
    /// What `v` gives a queue: the join over every value it may be through copies and phis, and
    /// through the `maxsize` of a construction. The values on one cycle share one answer, and
    /// every value is walked once per function.
    pub(crate) fn walk(&self, root: VarId) -> Walked {
        if let Some(done) = self.walks.borrow().get(&root) {
            return done.clone();
        }
        let mut visits: FxHashMap<VarId, Visit> = FxHashMap::default();
        let mut stack: Vec<VarId> = Vec::new();
        // `(value, next source to visit)`.
        let mut path: Vec<(VarId, usize)> = vec![(root, 0)];
        self.enter(root, &mut visits, &mut stack);
        while let Some(&(v, next)) = path.last() {
            if let Some(&w) = visits[&v].sources.get(next) {
                path.last_mut().expect("a value on the path").1 += 1;
                match visits.get(&w) {
                    _ if self.walks.borrow().contains_key(&w) => {}
                    None => {
                        self.enter(w, &mut visits, &mut stack);
                        path.push((w, 0));
                    }
                    Some(seen) => {
                        let reached = seen.on_stack.then_some(seen.index);
                        let visit = visits.get_mut(&v).expect("a visited value");
                        visit.low = visit.low.min(reached.unwrap_or(visit.low));
                    }
                }
                continue;
            }
            path.pop();
            let (index, low) = (visits[&v].index, visits[&v].low);
            if let Some(&(parent, _)) = path.last() {
                let visit = visits.get_mut(&parent).expect("a visited value");
                visit.low = visit.low.min(low);
            }
            if low == index {
                // `v` starts a cycle, or is alone: its members share the join.
                let at = stack
                    .iter()
                    .rposition(|m| *m == v)
                    .expect("a value starting a cycle is on the stack");
                let members = stack.split_off(at);
                let mut walks = self.walks.borrow_mut();
                let mut out = Walked::default();
                for m in &members {
                    let visit = &visits[m];
                    out.join(&visit.own);
                    for w in visit.sources.iter().filter_map(|w| walks.get(w)) {
                        out.join(w);
                    }
                }
                for m in members {
                    visits.get_mut(&m).expect("a visited value").on_stack = false;
                    walks.insert(m, out.clone());
                }
            }
        }
        self.walks.borrow()[&root].clone()
    }

    /// Start visiting `v`: what it is itself and the values it comes from.
    fn enter(&self, v: VarId, visits: &mut FxHashMap<VarId, Visit>, stack: &mut Vec<VarId>) {
        let ints = self
            .ints
            .get_or_init(|| FnIndex::new(self.f, &self.cx.ir().constants));
        let (own, sources) = match (ints.const_int(v), self.defs.get(&v)) {
            (Some(n), _) => {
                let own = Walked {
                    int: true,
                    ..Walked::of(CapacityOf::fixed(Answer::Known(Bound::of_positive(n > 0))))
                };
                (own, Vec::new())
            }
            (None, Some(inst)) => self.defined(inst),
            (None, None) => match self.phis.get(&v) {
                Some((_, phi)) => (
                    Walked::default(),
                    phi.incoming.iter().map(|(_, w)| *w).collect(),
                ),
                None => (self.param(v), Vec::new()),
            },
        };
        let index = visits.len();
        let visit = Visit {
            index,
            low: index,
            on_stack: true,
            own,
            sources,
        };
        visits.insert(v, visit);
        stack.push(v);
    }

    /// What the value an instruction defines is itself, and the values it comes from (the
    /// source of a copy, the `maxsize` of a construction).
    fn defined(&self, inst: &Inst) -> (Walked, Vec<VarId>) {
        let unknown = || (Walked::of(CapacityOf::unknown()), Vec::new());
        if let Some(fqn) = self.global_read(inst) {
            return (self.global_fact(fqn), Vec::new());
        }
        match &inst.kind {
            InstKind::Copy { src, .. } => (Walked::default(), vec![*src]),
            InstKind::Const {
                literal: ConstLiteral::None,
                ..
            } => (Walked::default(), Vec::new()),
            InstKind::Call {
                callee_var,
                args,
                kwargs,
                star_from,
                ..
            } => match callee_var.and_then(|c| self.queue_slot(self.named(c))) {
                Some(slot) => construction(slot, args, kwargs, *star_from),
                None => unknown(),
            },
            InstKind::MethodCall {
                recv,
                method,
                args,
                kwargs,
                star_from,
                ..
            } => match self.queue_slot(self.named(*recv).map(|r| format!("{r}.{method}"))) {
                Some(slot) => construction(slot, args, kwargs, *star_from),
                None => unknown(),
            },
            InstKind::AttrLoad { obj, attr, .. } => {
                let fact = match ((self.class_of)(*obj), self.named(*obj)) {
                    (Some(class), _) => self.attr_fact(class, attr),
                    // An attribute of a module (or class) object: its global.
                    (None, Some(base)) => self.global_fact(format!("{base}.{attr}")),
                    (None, None) => Walked::of(CapacityOf::unknown()),
                };
                (fact, Vec::new())
            }
            _ => unknown(),
        }
    }

    /// The fqn of the global object a read of a name gives: a read of a module global, or an
    /// import statement's binding (wherever it is, also inside a function), which names what it
    /// imports.
    fn global_read(&self, inst: &Inst) -> Option<String> {
        match &inst.kind {
            InstKind::LoadGlobal {
                name,
                is_lazy_import: false,
                ..
            } => {
                let fqn = self.cx.module.imports.get(name).cloned();
                Some(fqn.unwrap_or_else(|| format!("{}.{name}", self.cx.module_name())))
            }
            InstKind::Import { is_lazy: false, .. } => {
                self.cx.import_target(inst.span).map(str::to_owned)
            }
            _ => None,
        }
    }

    /// A parameter (`v` defined by no instruction and no phi): what its callers bind, unless it
    /// collects arguments (`*args`, `**kwargs`).
    fn param(&self, v: VarId) -> Walked {
        let position = self
            .f
            .params
            .iter()
            .position(|p| p.var == v && !p.is_vararg && !p.is_kwarg)
            .and_then(|i| u8::try_from(i).ok());
        let capacity = position.map_or_else(CapacityOf::unknown, |i| CapacityOf {
            params: BTreeSet::from([(self.key.clone(), i)]),
            ..CapacityOf::default()
        });
        Walked {
            capacity,
            queue: position.is_some_and(|i| i < 64 && self.queue_params & (1u64 << i) != 0),
            int: false,
        }
    }

    fn attr_fact(&self, class: String, attr: &str) -> Walked {
        if self.in_decls {
            let named = CapacityOf {
                attrs: BTreeSet::from([(class, attr.to_owned())]),
                ..CapacityOf::default()
            };
            Walked {
                queue: true,
                ..Walked::of(named)
            }
        } else {
            Walked::of(self.cx.project.attr_capacity(&class, attr))
        }
    }

    fn global_fact(&self, fqn: String) -> Walked {
        if self.in_decls {
            let named = CapacityOf {
                globals: BTreeSet::from([fqn]),
                ..CapacityOf::default()
            };
            Walked {
                queue: true,
                ..Walked::of(named)
            }
        } else {
            Walked::of(self.cx.project.global_capacity(&fqn))
        }
    }

    /// The `bounded_by` argument of the catalogued queue class named `fqn`.
    fn queue_slot(&self, fqn: Option<String>) -> Option<&'a ArgSlot> {
        let kb: &'a EffectiveEcosystemKb = self.cx.kb;
        kb.lookup(&self.cx.project.canonical(&fqn?))?
            .bounded_by
            .as_ref()
    }

    /// The fqn a value names through the module's globals and imports (`queue`,
    /// `queue.Queue`), through copies and attribute loads, which read values defined before
    /// them, so the walk ends.
    fn named(&self, v: VarId) -> Option<String> {
        let mut v = v;
        let mut attrs: Vec<&str> = Vec::new();
        let ty = loop {
            let inst = self.defs.get(&v)?;
            match &inst.kind {
                InstKind::Copy { src, .. } => v = *src,
                InstKind::AttrLoad { obj, attr, .. } => {
                    attrs.push(attr);
                    v = *obj;
                }
                InstKind::LoadGlobal {
                    name,
                    is_lazy_import: false,
                    ..
                }
                | InstKind::LoadName { name, .. } => break self.cx.global(name),
                // What the import statement binds, also one inside a function, which the
                // module's own imports do not hold.
                InstKind::Import { is_lazy: false, .. } => break self.cx.import_type(inst.span)?,
                _ => return None,
            }
        };
        let (Ty::Ref(mut fqn) | Ty::Class(mut fqn)) = ty else {
            return None;
        };
        for a in attrs.iter().rev() {
            fqn.push('.');
            fqn.push_str(a);
        }
        Some(fqn)
    }
}

/// A construction of a catalogued queue class: unbounded without a `maxsize`, unknown when
/// unpacking may pass one, otherwise what its `maxsize` gives.
fn construction(
    slot: &ArgSlot,
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    star_from: Option<u8>,
) -> (Walked, Vec<VarId>) {
    let maxsize = passed_var(args, kwargs, star_from, (slot.arg, slot.param.as_deref()));
    let unpacked = star_from.is_some() || kwargs.iter().any(|(k, _)| k.is_none());
    let fixed = match (maxsize, unpacked) {
        (Some(_), _) => None,
        (None, true) => Some(Answer::Open),
        (None, false) => Some(Answer::Known(Bound::Unbounded)),
    };
    let own = Walked {
        queue: true,
        ..Walked::of(CapacityOf {
            fixed,
            ..CapacityOf::default()
        })
    };
    (own, maxsize.into_iter().collect())
}

/// Whether `class` is a catalogued queue class.
pub(crate) fn is_queue_class(kb: &EffectiveEcosystemKb, class: &str) -> bool {
    kb.lookup(class).is_some_and(|e| e.bounded_by.is_some())
}

// ----------------------------------------------------------------------------- the project

/// What the project callers bind to the parameters the index's waits name, over every call
/// site: the join of the arguments, unknown when a call binds the parameter to something heki
/// cannot see or when the function has no project caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParamCapacities {
    values: FxHashMap<ParamOf, Capacity>,
    /// The functions a parameter of which is asked about: what a call of one binds counts.
    asked: FxHashSet<FuncKey>,
}

/// What `site` binds to parameter `p` of its callee `bc`: unknown unless it binds it exactly
/// (`ArgOrigin::binding`, not through unpacking), then the queue its caller found
/// (`SiteQueues::handed`), or its constant.
pub(crate) fn bound_capacity(site: &CallSite, bc: &BoundCallee, p: usize) -> CapacityOf {
    let Some(b) = bc.params.get(p).and_then(|o| o.binding.as_ref()) else {
        return CapacityOf::unknown();
    };
    let handed = site.queues.iter().flat_map(|q| &q.handed);
    if let Some((_, _, c)) = handed
        .into_iter()
        .find(|(f, i, _)| *f == bc.callee && usize::from(*i) == p)
    {
        return c.clone();
    }
    match b.constant.map(|c| (c.class, c.positive)) {
        Some((ConstClass::None, _)) => CapacityOf::default(),
        Some((ConstClass::Int, Some(p))) => CapacityOf::fixed(Answer::Known(Bound::of_positive(p))),
        _ => CapacityOf::unknown(),
    }
}

/// The join of what `c` fixes and what `value` gives its parameters (`None`: no queue). A value
/// still naming an attribute or a global, which only declarations hold, is not known.
fn resolve_with(c: &CapacityOf, value: impl Fn(&ParamOf) -> Option<Capacity>) -> Option<Capacity> {
    let named = (!c.attrs.is_empty() || !c.globals.is_empty()).then_some(Answer::Open);
    c.params.iter().map(value).fold(join(c.fixed, named), join)
}

impl ParamCapacities {
    /// The capacities of every parameter the index's waits name, and of the parameters the
    /// queues calls hand on name: the latter are all the parameters a call's binding can name,
    /// so one pass over the calls finds every source.
    pub(crate) fn of(index: &ProjectIndex) -> Self {
        let mut keys: FxHashSet<ParamOf> = FxHashSet::default();
        for file in index.files.values() {
            for s in file.local_functions.values() {
                keys.extend(
                    s.capacity_waits
                        .iter()
                        .flat_map(|w| w.wait.capacity.params.iter().cloned()),
                );
            }
            for c in file.call_sites.iter().flat_map(site_capacities) {
                keys.extend(c.params.iter().cloned());
            }
        }
        let mut wanted: FxHashMap<&FuncKey, Vec<u8>> = FxHashMap::default();
        for (f, i) in &keys {
            wanted.entry(f).or_default().push(*i);
        }
        // The arguments of every call binding them, and which parameters each one's value
        // depends on.
        let mut sources: FxHashMap<ParamOf, Vec<CapacityOf>> = FxHashMap::default();
        let calls = index.files.values().flat_map(|f| &f.call_sites);
        for site in calls.filter(|s| s.role == SiteRole::Call) {
            for bc in site.project_callees() {
                for &i in wanted.get(&bc.callee).into_iter().flatten() {
                    let c = bound_capacity(site, bc, usize::from(i));
                    sources.entry((bc.callee.clone(), i)).or_default().push(c);
                }
            }
        }
        let mut dependents: FxHashMap<&ParamOf, Vec<&ParamOf>> = FxHashMap::default();
        for (k, srcs) in &sources {
            for p in srcs.iter().flat_map(|c| &c.params) {
                dependents.entry(p).or_default().push(k);
            }
        }
        // A join fixpoint over a worklist: a value only grows, three times at most, and each
        // change queues the parameters that depend on it.
        let mut values: FxHashMap<&ParamOf, Option<Capacity>> = FxHashMap::default();
        let mut queue: Vec<&ParamOf> = keys.iter().collect();
        queue.sort();
        let mut queued: FxHashSet<&ParamOf> = queue.iter().copied().collect();
        while let Some(k) = queue.pop() {
            queued.remove(k);
            let now = sources.get(k).map_or(Some(Answer::Open), |srcs| {
                let value = |p: &ParamOf| values.get(p).copied().flatten();
                srcs.iter().map(|c| resolve_with(c, value)).fold(None, join)
            });
            if values.insert(k, now) == Some(now) {
                continue;
            }
            for d in dependents.get(k).into_iter().flatten() {
                if queued.insert(d) {
                    queue.push(d);
                }
            }
        }
        Self {
            asked: keys.iter().map(|(f, _)| f.clone()).collect(),
            values: values
                .into_iter()
                .filter_map(|(k, v)| Some((k.clone(), v?)))
                .collect(),
        }
    }

    /// Whether replacing a file's summary, `files` before and after, can change the
    /// capacities: one of them has queue facts of its own (waits, calls waiting on a queue or
    /// handing one on), or calls a function a parameter of which is asked about.
    pub(crate) fn touched_by<'f>(
        &self,
        files: impl IntoIterator<Item = &'f crate::FileSummary>,
    ) -> bool {
        files.into_iter().any(|f| {
            let waits = f
                .local_functions
                .values()
                .any(|s| !s.capacity_waits.is_empty());
            waits
                || f.call_sites.iter().any(|c| {
                    c.queues.is_some()
                        || c.project_callees()
                            .iter()
                            .any(|bc| self.asked.contains(&bc.callee))
                })
        })
    }

    /// What `c` resolves to: `None` when no queue reaches it.
    pub(crate) fn resolve(&self, c: &CapacityOf) -> Option<Capacity> {
        resolve_with(c, |p| self.values.get(p).copied())
    }

    pub(crate) fn bounded(&self, c: &CapacityOf) -> bool {
        self.resolve(c) == Some(Answer::Known(Bound::Bounded))
    }

    /// The parameters whose capacities differ between `self` and `other`.
    pub(crate) fn changed(&self, other: &Self) -> FxHashSet<ParamOf> {
        let differ = |a: &Self, b: &Self| -> Vec<ParamOf> {
            a.values
                .iter()
                .filter(|(k, v)| b.values.get(*k) != Some(*v))
                .map(|(k, _)| k.clone())
                .collect()
        };
        differ(self, other)
            .into_iter()
            .chain(differ(other, self))
            .collect()
    }

    /// Add to `s`, the summary being computed from `local`, the effects of the waits of `local`
    /// on queues the project shows bounded, as an occurrence each.
    pub(crate) fn realize(&self, local: &EffectSummary, s: &mut EffectSummary) {
        for w in local
            .capacity_waits
            .iter()
            .filter(|w| self.bounded(&w.wait.capacity))
        {
            for &e in &w.wait.effects {
                let had = !s.direct_exec_effects.insert(e);
                guards::add_occurrence(&mut s.exec_guards, e, had, &w.guards);
                s.exec_chains
                    .entry(e)
                    .or_insert_with(|| vec![Arc::new(w.hop.clone())]);
            }
        }
    }

    /// The blocking effects a call waiting while its queue is full has: those of `site` when the
    /// project shows the queue bounded.
    pub(crate) fn while_full<'s>(
        &self,
        site: &'s CallSite,
    ) -> Option<&'s BTreeSet<ExecEffectKind>> {
        let w = site.queues.as_deref()?.while_full.as_ref()?;
        self.bounded(&w.capacity).then_some(&w.effects)
    }
}

/// What a call names of the project's facts: the queue it waits on when it waits only on a
/// bounded one, and the queues it hands on (`CallSite::queues`).
fn site_capacities(site: &CallSite) -> impl Iterator<Item = &CapacityOf> {
    site.queues.iter().flat_map(|q| {
        let while_full = q.while_full.iter().map(|w| &w.capacity);
        while_full.chain(q.handed.iter().map(|(_, _, c)| c))
    })
}

/// The functions of `file` whose facts name one of `params`: their waits, or their calls.
pub(crate) fn asking<'f>(
    file: &'f crate::FileSummary,
    params: &FxHashSet<ParamOf>,
) -> FxHashSet<&'f FuncKey> {
    let names = |c: &CapacityOf| c.params.iter().any(|p| params.contains(p));
    let waiting = file
        .local_functions
        .iter()
        .filter(|(_, s)| s.capacity_waits.iter().any(|w| names(&w.wait.capacity)));
    let calling = file
        .call_sites
        .iter()
        .filter(|c| site_capacities(c).any(names))
        .map(|c| &c.caller);
    waiting.map(|(k, _)| k).chain(calling).collect()
}

/// What `start` gives a queue through the declarations, in terms of parameters only: what it
/// fixes, joined with the answer of each attribute and global it names ([`NameWalk`]).
pub(crate) fn resolve_in_decls(project: &ProjectDecls, start: CapacityOf) -> CapacityOf {
    let mut out = CapacityOf {
        fixed: start.fixed,
        params: start.params,
        ..CapacityOf::default()
    };
    for name in names(start.attrs, start.globals) {
        let (value, reads) = NameWalk { project }.answer(name);
        if let Some(reads) = reads {
            project.replay(reads);
        }
        out.join(&value);
    }
    out
}

/// Each attribute and global of `attrs` and `globals` as the `CapacityOf` naming only it, the
/// key its answer is kept under.
fn names(
    attrs: BTreeSet<(String, String)>,
    globals: BTreeSet<String>,
) -> impl Iterator<Item = CapacityOf> {
    let attrs = attrs.into_iter().map(|a| CapacityOf {
        attrs: BTreeSet::from([a]),
        ..CapacityOf::default()
    });
    let globals = globals.into_iter().map(|g| CapacityOf {
        globals: BTreeSet::from([g]),
        ..CapacityOf::default()
    });
    attrs.chain(globals)
}

/// An answer of the declarations (`ProjectDecls::known_capacity`): the capacity, in terms of
/// parameters only, and what computing it read while recording.
type NameAnswer = (CapacityOf, Option<Arc<DeclKeys>>);

/// The answers of attributes and globals: what the declarations record of each one's stores
/// (`ProjectDecls::stored_attr_capacities`, `ProjectDecls::stored_global_capacity`), joined
/// with the answers of the names they read. Every name is resolved once while the indexes stay
/// (`ProjectDecls::keep_capacity`), and the names on one cycle share one answer.
struct NameWalk<'p> {
    project: &'p ProjectDecls,
}

/// One name of a walk in progress (Tarjan's algorithm over the names the stores of each name
/// read): its position, the least position it reaches back to, whether it is on the stack,
/// what its stores fix with the parameters they name, what reading them read, and the names
/// they read.
struct NameVisit {
    index: usize,
    low: usize,
    on_stack: bool,
    own: CapacityOf,
    reads: Option<DeclKeys>,
    names: Vec<CapacityOf>,
}

impl NameWalk<'_> {
    fn answer(&self, root: CapacityOf) -> NameAnswer {
        if let Some(known) = self.project.known_capacity(&root) {
            return known;
        }
        let mut done: FxHashMap<CapacityOf, NameAnswer> = FxHashMap::default();
        let mut visits: FxHashMap<CapacityOf, NameVisit> = FxHashMap::default();
        let mut stack: Vec<CapacityOf> = Vec::new();
        // `(name, next name its stores read to visit)`.
        let mut path: Vec<(CapacityOf, usize)> = vec![(root.clone(), 0)];
        self.enter(root.clone(), &mut visits, &mut stack);
        while let Some((v, next)) = path.last().cloned() {
            if let Some(w) = visits[&v].names.get(next).cloned() {
                path.last_mut().expect("a name on the path").1 += 1;
                if done.contains_key(&w) {
                    continue;
                }
                if let Some(known) = self.project.known_capacity(&w) {
                    done.insert(w, known);
                    continue;
                }
                match visits.get(&w) {
                    None => {
                        self.enter(w.clone(), &mut visits, &mut stack);
                        path.push((w, 0));
                    }
                    Some(seen) => {
                        let reached = seen.on_stack.then_some(seen.index);
                        let visit = visits.get_mut(&v).expect("a visited name");
                        visit.low = visit.low.min(reached.unwrap_or(visit.low));
                    }
                }
                continue;
            }
            path.pop();
            let (index, low) = (visits[&v].index, visits[&v].low);
            if let Some((parent, _)) = path.last() {
                let visit = visits.get_mut(parent).expect("a visited name");
                visit.low = visit.low.min(low);
            }
            if low == index {
                // `v` starts a cycle, or is alone: its members share the answer.
                let at = stack
                    .iter()
                    .rposition(|m| *m == v)
                    .expect("a name starting a cycle is on the stack");
                let members = stack.split_off(at);
                let answer = self.settle(&members, &mut visits, &done);
                for m in members {
                    self.project
                        .keep_capacity(m.clone(), answer.0.clone(), answer.1.clone());
                    done.insert(m, answer.clone());
                }
            }
        }
        done.remove(&root).expect("the root's answer is settled")
    }

    /// Start visiting `name`: what its stores fix, and the names they read.
    fn enter(
        &self,
        name: CapacityOf,
        visits: &mut FxHashMap<CapacityOf, NameVisit>,
        stack: &mut Vec<CapacityOf>,
    ) {
        let (stored, reads) = self.project.reads_of(|| self.stored(&name));
        let index = visits.len();
        let visit = NameVisit {
            index,
            low: index,
            on_stack: true,
            own: CapacityOf {
                fixed: stored.fixed,
                params: stored.params,
                ..CapacityOf::default()
            },
            reads,
            names: names(stored.attrs, stored.globals).collect(),
        };
        visits.insert(name.clone(), visit);
        stack.push(name);
    }

    /// The join of what the declarations record of the stores of `name`.
    fn stored(&self, name: &CapacityOf) -> CapacityOf {
        let mut out = CapacityOf::default();
        for (class, attr) in &name.attrs {
            for c in self.project.stored_attr_capacities(class, attr) {
                out.join(&c);
            }
        }
        for fqn in &name.globals {
            out.join(&self.project.stored_global_capacity(fqn));
        }
        out
    }

    /// The answer of the cycle `members`: what their stores fix, joined with the answers of
    /// the names outside it they read (settled before it), and what all of that read.
    fn settle(
        &self,
        members: &[CapacityOf],
        visits: &mut FxHashMap<CapacityOf, NameVisit>,
        done: &FxHashMap<CapacityOf, NameAnswer>,
    ) -> NameAnswer {
        let mut value = CapacityOf::default();
        let mut reads: Option<DeclKeys> = None;
        let mut add_reads = |r: DeclKeys| reads.get_or_insert_with(DeclKeys::default).extend(r);
        for m in members {
            let visit = visits.get_mut(m).expect("a visited name");
            visit.on_stack = false;
            value.join(&visit.own);
            if let Some(r) = visit.reads.take() {
                add_reads(r);
            }
            for (answer, r) in visit.names.iter().filter_map(|w| done.get(w)) {
                value.join(answer);
                if let Some(r) = r {
                    add_reads(DeclKeys::clone(r));
                }
            }
        }
        (value, reads.map(Arc::new))
    }
}

// ------------------------------------------------------------------------ the declarations

/// `ClassDecl::attr_capacities` and `ModuleDecls::global_capacities` of a module: what its class
/// bodies and methods store in attributes, and what it binds its globals to. A module that
/// neither imports a catalogued queue class (nor its module, at any scope) nor types an
/// attribute or a global as one builds no queue and holds none a `put` can be resolved on, and
/// records nothing.
pub(crate) fn record(input: &ModuleInput<'_>, cx: &TypeCtx<'_>, decls: &mut ModuleDecls) {
    let holds_a_queue = || imports_a_queue(input, cx, decls) || types_a_queue(cx, decls);
    if input.ir.constants.dynamic || !holds_a_queue() {
        return;
    }
    let mut attrs: BTreeMap<(String, String), Walked> = BTreeMap::new();
    let mut globals: BTreeMap<String, Walked> = BTreeMap::new();
    // The attributes and globals a queue's `maxsize` reads: an integer stored there counts.
    let mut maxsize_names: FxHashSet<String> = FxHashSet::default();
    for f in &input.ir.functions {
        let owner = if f.is_class_body {
            Some((f.qualname.clone(), FxHashSet::default()))
        } else {
            match (&f.owner_class_qualname, f.params.first()) {
                (Some(owner), Some(p))
                    if is_summarized_function(f)
                        && !f.has_decorator(&["staticmethod", "classmethod"]) =>
                {
                    Some((owner.clone(), self_aliases(f, p.var)))
                }
                _ => None,
            }
        };
        let mut defs: FxHashMap<VarId, &Inst> = FxHashMap::default();
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            if let Some(d) = inst.kind.dst() {
                defs.insert(d, inst);
            }
        }
        let phis = OnceCell::new();
        let ints = OnceCell::new();
        let key = FuncKey {
            module: input.module_name.clone(),
            qualname: func_qualname(f).to_owned(),
        };
        let owner_fqn = owner
            .as_ref()
            .map(|(q, _)| format!("{}.{q}", input.module_name));
        let selfs = owner.as_ref().map(|(_, s)| s);
        let class_of = |v: VarId| {
            let own = selfs.is_some_and(|s| s.contains(&v));
            own.then(|| owner_fqn.clone()).flatten()
        };
        let queue_params = f
            .params
            .iter()
            .take(64)
            .enumerate()
            .filter(|(_, p)| annotates_a_queue(cx, p.annotation.as_deref()))
            .fold(0u64, |m, (i, _)| m | 1u64 << i);
        let walks = RefCell::default();
        let values = ValueCapacity {
            cx,
            f,
            key: &key,
            defs: &defs,
            phis: guards::phi_table(f, &phis),
            ints: &ints,
            class_of: &class_of,
            queue_params,
            in_decls: true,
            walks: &walks,
        };
        let attr_of = |name: &str| {
            let class = &owner.as_ref().expect("a class's own store").0;
            (class.clone(), name.to_owned())
        };
        // A class body binds a class attribute to a named copy.
        let class_body_store = |kind: &InstKind| match kind {
            InstKind::Copy { dst, src } if f.is_class_body => {
                let name = f.var_name(*dst).filter(|n| !n.starts_with('.'));
                name.map(|n| (n, *src))
            }
            _ => None,
        };
        for inst in f.blocks.iter().flat_map(|b| &b.insts) {
            let (stores, value) = match (&inst.kind, class_body_store(&inst.kind)) {
                (_, Some((name, src))) => (attrs.entry(attr_of(name)).or_default(), src),
                (InstKind::AttrStore { obj, attr, value }, None) if class_of(*obj).is_some() => {
                    (attrs.entry(attr_of(attr)).or_default(), *value)
                }
                (InstKind::StoreGlobal { name, value }, None) => {
                    (globals.entry(name.clone()).or_default(), *value)
                }
                (
                    InstKind::Call {
                        callee_var: Some(c),
                        args,
                        kwargs,
                        star_from,
                        ..
                    },
                    None,
                ) => {
                    let slot = values.queue_slot(values.named(*c));
                    note_maxsize_name(&defs, slot, args, kwargs, *star_from, &mut maxsize_names);
                    continue;
                }
                (
                    InstKind::MethodCall {
                        recv,
                        method,
                        args,
                        kwargs,
                        star_from,
                        ..
                    },
                    None,
                ) => {
                    let fqn = values.named(*recv).map(|r| format!("{r}.{method}"));
                    let slot = values.queue_slot(fqn);
                    note_maxsize_name(&defs, slot, args, kwargs, *star_from, &mut maxsize_names);
                    continue;
                }
                _ => continue,
            };
            stores.join(&values.walk(value));
        }
    }
    let kept = |name: &str, s: &Walked| s.queue || (s.int && maxsize_names.contains(name));
    for ((owner, attr), s) in attrs {
        if kept(&attr, &s)
            && let Some(class) = decls.classes.get_mut(&owner)
        {
            class.attr_capacities.insert(attr, s.capacity);
        }
    }
    for (name, s) in globals {
        if kept(&name, &s) {
            decls.global_capacities.insert(name, s.capacity);
        }
    }
}

/// Note the attribute or global a construction of a catalogued queue class (`slot`: its
/// `maxsize` argument) reads as its `maxsize`, through copies.
fn note_maxsize_name(
    defs: &FxHashMap<VarId, &Inst>,
    slot: Option<&ArgSlot>,
    args: &[VarId],
    kwargs: &[(Option<String>, VarId)],
    star_from: Option<u8>,
    names: &mut FxHashSet<String>,
) {
    let at = slot.map(|s| (s.arg, s.param.as_deref()));
    let Some(mut v) = at.and_then(|at| passed_var(args, kwargs, star_from, at)) else {
        return;
    };
    while let Some(InstKind::Copy { src, .. }) = defs.get(&v).map(|i| &i.kind) {
        v = *src;
    }
    if let Some(InstKind::AttrLoad { attr: name, .. } | InstKind::LoadGlobal { name, .. }) =
        defs.get(&v).map(|i| &i.kind)
    {
        names.insert(name.clone());
    }
}

/// Whether a parameter annotation names a catalogued queue class (also in a union).
fn annotates_a_queue(cx: &TypeCtx<'_>, annotation: Option<&str>) -> bool {
    let ty = annotation.and_then(|a| cx.module.resolve_annotation_ty(a));
    ty.is_some_and(|t| names_a_queue(cx, &t))
}

/// Whether an instance of `ty` may be one of a catalogued queue class.
fn names_a_queue(cx: &TypeCtx<'_>, ty: &Ty) -> bool {
    ty.members()
        .iter()
        .any(|o| is_queue_class(cx.kb, &cx.project.canonical(&o.class)))
}

/// Whether the module's declarations type an attribute of one of its classes, or one of its
/// globals, as a queue (`self.events: Queue = EVENTS` with `Queue` imported from a project
/// module).
fn types_a_queue(cx: &TypeCtx<'_>, decls: &ModuleDecls) -> bool {
    let attrs = decls.classes.values().flat_map(|c| c.attr_types.values());
    attrs
        .chain(decls.global_types.values())
        .any(|t| names_a_queue(cx, t))
}

/// Whether the module imports (also for annotations only, or inside a function) a catalogued
/// queue class or a module holding one.
fn imports_a_queue(input: &ModuleInput<'_>, cx: &TypeCtx<'_>, decls: &ModuleDecls) -> bool {
    let queues: Vec<&str> = cx
        .kb
        .entries
        .values()
        .filter(|e| e.bounded_by.is_some())
        .map(|e| e.symbol.as_str())
        .collect();
    let anywhere = input
        .semantic
        .imports
        .iter()
        .filter_map(|imp| cx.import_target(imp.span));
    decls
        .imports
        .values()
        .chain(decls.type_checking_imports.values())
        .map(String::as_str)
        .chain(anywhere)
        .any(|fqn| {
            let fqn = cx.project.canonical(fqn);
            queues.iter().any(|q| {
                *q == fqn
                    || q.strip_prefix(fqn.as_str())
                        .is_some_and(|rest| rest.starts_with('.'))
            })
        })
}
