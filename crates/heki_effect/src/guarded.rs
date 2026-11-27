//! Shared state and the thread locks held where it is written (`EFF117`).
//!
//! A thread lock is a value typed as an instance of a catalog class flagged `sync_lock_or_tx`
//! (`threading.Lock`, `RLock`, `Condition`, `Semaphore`). A function holds one where every path
//! to the point took it and has not given it back ([`held::at_entry`]): by a `with` over it, by
//! an `acquire()` (with any arguments: code that goes on after a failed timed acquisition is
//! not told apart), or by a call of a function that returns holding it
//! ([`GuardFacts::returns_holding`], joined at the call through the binding of `self`). It
//! also holds one where every path from the point on gives it back by `release()` before taking
//! it again: code that releases a lock it did not take itself took it through a helper, or was
//! called holding it.
//!
//! Shared state is an attribute of the instance a method runs on, a module global, or an
//! attribute of a class or module, assigned, or changed in place where heki knows it holds a
//! builtin container (an item store or delete, a mutating method of the container's class).
//! Attributes of a `threading.local` are no shared state: every thread sees its own.

use crate::held::{self, Acquisitions, Change};
use crate::readonly::ContainerKind;
use crate::resolver::{TypeCtx, Val};
use heki_ast::Span;
use heki_ir::walk::{And, Budget, Facts, FnShape, Sparse, Visit, fixpoint, reach};
use heki_ir::{BlockId, Inst, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A thread lock as the code names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockKey {
    /// `self.<attr>` of the instance a method runs on (also read in a scope nested in it).
    SelfAttr(String),
    /// A module global, or an attribute of a class or module, fully qualified.
    Global(String),
    /// A lock heki cannot name the same way in another function (a local, another object's).
    Unnamed,
}

impl LockKey {
    /// How a report names the lock; `None` for one heki cannot name.
    pub fn name(&self) -> Option<String> {
        match self {
            Self::SelfAttr(attr) => Some(format!("self.{attr}")),
            Self::Global(fqn) => Some(fqn.clone()),
            Self::Unnamed => None,
        }
    }
}

/// State that outlives the calls of the functions writing it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateKey {
    /// The attribute `attr` of an instance, written by a method of `class` (fully qualified) on
    /// the instance it runs on.
    Attr { class: String, attr: String },
    /// A module global, or an attribute of a class or module, fully qualified.
    Global(String),
}

impl StateKey {
    /// How a report names the state.
    pub fn display(&self) -> String {
        match self {
            Self::Attr { attr, .. } => format!("self.{attr}"),
            Self::Global(fqn) => fqn.clone(),
        }
    }
}

/// How a write changes the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteKind {
    /// Binds or deletes the name or attribute (`self.n += 1`, `self.cache = {}`).
    Assigns,
    /// Changes the container it holds in place (`self.d[k] = v`, `self.items.append(x)`).
    Changes,
}

impl WriteKind {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Assigns => "assigns",
            Self::Changes => "changes",
        }
    }
}

/// One write of shared state by a function's own body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateWrite {
    pub state: StateKey,
    pub kind: WriteKind,
    pub span: Span,
    /// The writing instruction: its block and index.
    pub at: (BlockId, usize),
}

/// A write of shared state made holding a lock heki names ([`GuardFacts::locked_writes`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LockedWrite {
    pub state: StateKey,
    pub lock: LockKey,
    pub kind: WriteKind,
    pub span: Span,
}

/// A function's `EFF117` facts (`EffectSummary::guarded`; computed only while the rule runs,
/// `OptInFacts::guarded`). Local facts: callers read them at their calls of the function.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardFacts {
    /// The named thread locks the function holds on every normal return, which it took itself:
    /// a helper taking a lock for its caller (`def _acquire(self): self._lock.acquire()`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub returns_holding: BTreeSet<LockKey>,
    /// The function's own writes of shared state made holding a named thread lock, each with
    /// the outermost such lock.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locked_writes: Vec<LockedWrite>,
}

impl GuardFacts {
    pub fn is_empty(&self) -> bool {
        self.returns_holding.is_empty() && self.locked_writes.is_empty()
    }

    /// Copy with every source location zeroed (Early Cutoff).
    pub(crate) fn without_locations(&self) -> Self {
        let mut out = self.clone();
        for w in &mut out.locked_writes {
            w.span = Span { start: 0, end: 0 };
        }
        out
    }
}

/// What the receiver of a function's code is, for a name it reads.
enum Receiver {
    /// The instance a method of the class (fully qualified) runs on.
    Instance(String),
    /// A class or module, fully qualified.
    Named(String),
}

/// Where the function of `shape` (or the method a scope nested in it reads `self` from) runs on
/// an instance: the class, fully qualified, when `var` copies that method's first parameter.
fn receiver(
    tcx: &TypeCtx<'_>,
    shape: &FnShape<'_>,
    types: &FxHashMap<VarId, Val>,
    var: VarId,
) -> Option<Receiver> {
    let (f, var) = (shape.f, shape.copy_root(var));
    let method = |g: &IrFunction, param: u8| {
        let class = g.owner_class_qualname.as_ref()?;
        let fqn = format!("{}.{class}", tcx.module_name());
        let named = g
            .decorators
            .iter()
            .any(|d| d == "classmethod" || d == "staticmethod");
        match (param, named) {
            (0, false) => Some(Receiver::Instance(fqn)),
            (0, true) if g.decorators.iter().any(|d| d == "classmethod") => {
                Some(Receiver::Named(fqn))
            }
            _ => None,
        }
    };
    if f.params.first().is_some_and(|p| p.var == var) {
        return method(f, 0);
    }
    match shape.inst(var).map(|i| &i.kind) {
        Some(InstKind::LoadDeref { name, .. }) => {
            let (g, param) = tcx.captured_param(f, name)?;
            method(g, param)
        }
        _ => match types.get(&var) {
            Some(Val::Ref(fqn)) => Some(Receiver::Named(tcx.project.canonical(fqn))),
            _ => None,
        },
    }
}

/// Whether the class (fully qualified) keeps its instances' attributes per thread.
fn thread_local_class(tcx: &TypeCtx<'_>, class: &str) -> bool {
    let local = |c: &str| heki_kb::THREAD_LOCAL_CLASSES.contains(&c);
    local(class) || tcx.project.mro_of(class).iter().any(|c| local(c))
}

/// The object `var` reaches as shared state: an attribute of the method's instance, of a class,
/// or of a module, or a module global (through copies).
fn state_of(
    tcx: &TypeCtx<'_>,
    shape: &FnShape<'_>,
    types: &FxHashMap<VarId, Val>,
    var: VarId,
) -> Option<StateKey> {
    match &shape.inst(shape.copy_root(var))?.kind {
        InstKind::AttrLoad { obj, attr, .. } => attr_state(tcx, shape, types, *obj, attr),
        InstKind::LoadGlobal { name, .. } => Some(StateKey::Global(tcx.global_fqn(name))),
        _ => None,
    }
}

/// The attribute `attr` of `obj` as shared state.
fn attr_state(
    tcx: &TypeCtx<'_>,
    shape: &FnShape<'_>,
    types: &FxHashMap<VarId, Val>,
    obj: VarId,
    attr: &str,
) -> Option<StateKey> {
    match receiver(tcx, shape, types, obj)? {
        Receiver::Instance(class) if !thread_local_class(tcx, &class) => Some(StateKey::Attr {
            class,
            attr: attr.to_owned(),
        }),
        Receiver::Instance(_) => None,
        Receiver::Named(fqn) => Some(StateKey::Global(
            tcx.project.canonical(&format!("{fqn}.{attr}")),
        )),
    }
}

/// The builtin container kind of a value heki types as one.
fn container(types: &FxHashMap<VarId, Val>, var: VarId) -> Option<ContainerKind> {
    match types.get(&var) {
        Some(Val::Inst(class, _)) => ContainerKind::of_class(class),
        _ => None,
    }
}

/// The write of shared state `kind` makes in the function of `shape`: an assignment or deletion
/// of an attribute of the instance, a class, or a module, or of a global the function declares
/// `global`; an item store or delete, or a mutating method call, on a builtin container such
/// state holds.
pub fn state_written(
    tcx: &TypeCtx<'_>,
    shape: &FnShape<'_>,
    types: &FxHashMap<VarId, Val>,
    kind: &InstKind,
) -> Option<(StateKey, WriteKind)> {
    let changed = |obj: VarId| {
        container(types, shape.copy_root(obj))?;
        Some((state_of(tcx, shape, types, obj)?, WriteKind::Changes))
    };
    match kind {
        InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr } => {
            if matches!(types.get(&shape.copy_root(*obj)), Some(Val::Inst(c, _)) if thread_local_class(tcx, c))
            {
                return None;
            }
            Some((
                attr_state(tcx, shape, types, *obj, attr)?,
                WriteKind::Assigns,
            ))
        }
        InstKind::StoreGlobal { name, .. }
        | InstKind::Delete {
            name, global: true, ..
        } if !shape.f.is_module_body => {
            Some((StateKey::Global(tcx.global_fqn(name)), WriteKind::Assigns))
        }
        InstKind::SubscriptStore { obj, .. } | InstKind::SubscriptDelete { obj, .. } => {
            changed(*obj)
        }
        InstKind::MethodCall { recv, method, .. } => {
            let kind = container(types, shape.copy_root(*recv))?;
            if !kind.mutators().contains(&method.as_str()) {
                return None;
            }
            changed(*recv)
        }
        _ => None,
    }
}

/// The own writes of shared state of the function of `shape` in the blocks a run reaches
/// (`live`).
pub fn state_writes(
    tcx: &TypeCtx<'_>,
    shape: &FnShape<'_>,
    types: &FxHashMap<VarId, Val>,
    live: &crate::ownership::Live,
) -> Vec<StateWrite> {
    let f = shape.f;
    if f.is_module_body || f.is_class_body {
        return Vec::new();
    }
    let mut out = Vec::new();
    for bb in f.blocks.iter().filter(|b| live.block(b.id)) {
        for (i, inst) in bb.insts.iter().enumerate() {
            if let Some((state, kind)) = state_written(tcx, shape, types, &inst.kind) {
                out.push(StateWrite {
                    state,
                    kind,
                    span: inst.span,
                    at: (bb.id, i),
                });
            }
        }
    }
    out
}

/// Whether `var` is a thread lock: typed as an instance of a catalog class flagged
/// `sync_lock_or_tx`. A variable a closure reads is typed so when the enclosing function binds it
/// to one and no nested scope rebinds it.
fn is_thread_lock(tcx: &TypeCtx<'_>, types: &FxHashMap<VarId, Val>, var: VarId) -> bool {
    match types.get(&var) {
        Some(Val::Inst(class, _)) => tcx.catalog(class).is_some_and(|e| e.sync_lock_or_tx),
        _ => false,
    }
}

/// How the function of `shape` names the lock `var`.
fn lock_key(
    tcx: &TypeCtx<'_>,
    shape: &FnShape<'_>,
    types: &FxHashMap<VarId, Val>,
    var: VarId,
) -> LockKey {
    let state = state_of(tcx, shape, types, var);
    match state {
        Some(StateKey::Attr { attr, .. }) => LockKey::SelfAttr(attr),
        Some(StateKey::Global(fqn)) => LockKey::Global(fqn),
        None => LockKey::Unnamed,
    }
}

/// How an instruction takes or gives back a thread lock.
enum Step {
    Take(VarId, bool),
    Release(LockKey),
    Exit(VarId),
}

fn step(
    tcx: &TypeCtx<'_>,
    shape: &FnShape<'_>,
    types: &FxHashMap<VarId, Val>,
    kind: &InstKind,
) -> Option<Step> {
    match kind {
        InstKind::EnterWith {
            mgr,
            is_async: false,
            ..
        } if is_thread_lock(tcx, types, *mgr) => Some(Step::Take(*mgr, true)),
        InstKind::ExitWith { mgr, .. } => Some(Step::Exit(*mgr)),
        InstKind::MethodCall {
            recv,
            method,
            is_awaited: false,
            ..
        } if is_thread_lock(tcx, types, *recv) => match method.as_str() {
            "acquire" => Some(Step::Take(*recv, false)),
            "release" => Some(Step::Release(lock_key(tcx, shape, types, *recv))),
            _ => None,
        },
        _ => None,
    }
}

/// Where a function certainly holds thread locks, and which ([`Self::held_at`]).
pub struct LockPoints {
    /// The locks held while each instruction runs, the outermost first, where one is.
    held: FxHashMap<(BlockId, usize), Vec<LockKey>>,
    /// The named locks the function took itself and holds on every normal return.
    returns_holding: BTreeSet<LockKey>,
    /// The instructions that run after a `release()` of a lock the function does not take
    /// itself on every path (one its callers hold), where one is.
    released_entry: FxHashSet<(BlockId, usize)>,
}

impl LockPoints {
    /// The thread lock steps of `f`, with the locks the calls at each instruction take for it
    /// (`taken_by_call`: the callees' [`GuardFacts::returns_holding`] in `f`'s names; nothing
    /// while the callees' facts are not known). One pass over the instructions after the
    /// dataflows, which keep a bit per acquisition and per lock released.
    pub fn of(
        tcx: &TypeCtx<'_>,
        shape: &FnShape<'_>,
        types: &FxHashMap<VarId, Val>,
        live: &crate::ownership::Live,
        taken_by_call: &dyn Fn(BlockId, usize, &Inst) -> Vec<LockKey>,
    ) -> Self {
        let f = shape.f;
        let mut taken: Acquisitions<LockKey> = Acquisitions::default();
        let mut changes = FxHashMap::default();
        let mut releases: Vec<((BlockId, usize), LockKey)> = Vec::new();
        let mut steps: Vec<((BlockId, usize), Step)> = Vec::new();
        for bb in f.blocks.iter().filter(|b| live.block(b.id)) {
            for (i, inst) in bb.insts.iter().enumerate() {
                if let Some(s) = step(tcx, shape, types, &inst.kind) {
                    steps.push(((bb.id, i), s));
                    continue;
                }
                // One acquisition per instruction: the first lock its calls take.
                if let Some(lock) = taken_by_call(bb.id, i, inst).into_iter().next() {
                    let k = taken.add(inst.span, None, lock);
                    changes.insert((bb.id, i), Change::Take(k));
                }
            }
        }
        for (at, s) in &steps {
            if let Step::Take(lock, with) = s {
                let key = lock_key(tcx, shape, types, *lock);
                let span = f.block(at.0).insts[at.1].span;
                let k = taken.add(span, with.then_some(*lock), key);
                changes.insert(*at, Change::Take(k));
            }
        }
        for (at, s) in &steps {
            match s {
                Step::Exit(mgr) => {
                    let span = f.block(at.0).insts[at.1].span;
                    changes.insert(*at, Change::Give(taken.exited(*mgr, span)));
                }
                Step::Release(key) => {
                    changes.insert(*at, Change::Give(taken.released(|k| k == key)));
                    releases.push((*at, key.clone()));
                }
                Step::Take(..) => {}
            }
        }
        let mut held: FxHashMap<(BlockId, usize), Vec<LockKey>> = if releases.is_empty() {
            FxHashMap::default()
        } else {
            let steps = Budget::unmetered("heki_effect::guarded::released_later");
            released_later(shape, &taken, &changes, &releases, &steps)
        };
        let released_entry = released_entry(shape, &taken, &releases);
        let mut returns_holding: Option<BTreeSet<LockKey>> = None;
        if !taken.is_empty() {
            let entry = held::at_entry(shape, taken.len(), |b, i| changes.get(&(b, i)));
            for (b, state) in entry.into_iter().enumerate() {
                let Some(mut state) = state else { continue };
                let bb = &f.blocks[b];
                for i in 0..bb.insts.len() {
                    let mut here: Vec<(u32, LockKey)> = (state.ones())
                        .map(|k| (taken.span(k).start, taken.item(k).clone()))
                        .collect();
                    if !here.is_empty() {
                        here.sort();
                        let keys = held.entry((bb.id, i)).or_default();
                        let mut ordered: Vec<LockKey> = here.into_iter().map(|(_, k)| k).collect();
                        for k in std::mem::take(keys) {
                            if !ordered.contains(&k) {
                                ordered.push(k);
                            }
                        }
                        *keys = ordered;
                    }
                    state.apply(changes.get(&(bb.id, i)));
                }
                if matches!(bb.terminator, Terminator::Return(_)) {
                    let named: BTreeSet<LockKey> = (state.ones())
                        .map(|k| taken.item(k).clone())
                        .filter(|k| *k != LockKey::Unnamed)
                        .collect();
                    returns_holding = Some(match returns_holding {
                        Some(prev) => prev.intersection(&named).cloned().collect(),
                        None => named,
                    });
                }
            }
        }
        Self {
            held,
            returns_holding: returns_holding.unwrap_or_default(),
            released_entry,
        }
    }

    /// Whether the function holds no thread lock anywhere, nor when it returns.
    pub fn is_empty(&self) -> bool {
        self.held.is_empty() && self.returns_holding.is_empty()
    }

    /// The thread locks certainly held while instruction `i` of block `b` runs, the outermost
    /// first; empty when none is.
    pub fn held_at(&self, b: BlockId, i: usize) -> &[LockKey] {
        self.held.get(&(b, i)).map_or(&[], Vec::as_slice)
    }

    /// Whether instruction `i` of block `b` runs after every path gave back a lock the function
    /// does not take itself: one its callers hold on entry.
    pub fn released_entry_at(&self, b: BlockId, i: usize) -> bool {
        self.released_entry.contains(&(b, i))
    }

    /// The named locks held on every normal return that the function took itself.
    pub fn returns_holding(&self) -> &BTreeSet<LockKey> {
        &self.returns_holding
    }
}

/// The instructions every path to which passed a `release()` of a lock the function takes
/// nowhere itself (a forward must-dataflow with one bit, set by those releases).
fn released_entry(
    shape: &FnShape<'_>,
    taken: &Acquisitions<LockKey>,
    releases: &[((BlockId, usize), LockKey)],
) -> FxHashSet<(BlockId, usize)> {
    let foreign: FxHashMap<(BlockId, usize), Change> = (releases.iter())
        .filter(|(_, key)| taken.released(|k| k == key).is_empty())
        .map(|(at, _)| (*at, Change::Take(0)))
        .collect();
    let mut out = FxHashSet::default();
    if foreign.is_empty() {
        return out;
    }
    let entry = held::at_entry(shape, 1, |b, i| foreign.get(&(b, i)));
    for (b, state) in entry.into_iter().enumerate() {
        let Some(mut state) = state else { continue };
        let bb = &shape.f.blocks[b];
        for i in 0..bb.insts.len() {
            if !state.is_empty() {
                out.insert((bb.id, i));
            }
            state.apply(foreign.get(&(bb.id, i)));
        }
    }
    out
}

/// The locks given back by `release()` on every path from each instruction of the function
/// before they are taken again: a backward must-dataflow over the blocks, exception edges
/// included, with a bit per lock released (at most 63). A block from which no path leaves the
/// function gives back nothing.
fn released_later(
    shape: &FnShape<'_>,
    taken: &Acquisitions<LockKey>,
    changes: &FxHashMap<(BlockId, usize), Change>,
    releases: &[((BlockId, usize), LockKey)],
    budget: &Budget,
) -> FxHashMap<(BlockId, usize), Vec<LockKey>> {
    let f = shape.f;
    let mut keys: Vec<&LockKey> = releases.iter().map(|(_, k)| k).collect();
    keys.sort();
    keys.dedup();
    keys.truncate(63);
    let bit = |k: &LockKey| keys.iter().position(|x| *x == k).map(|b| 1u64 << b);
    let full: u64 = (1u64 << keys.len()) - 1;
    let released: FxHashMap<(BlockId, usize), u64> = (releases.iter())
        .filter_map(|(at, k)| Some((*at, bit(k)?)))
        .collect();
    // Taking a lock again ends what a later release gives back of it.
    let retaken = |at: (BlockId, usize)| match changes.get(&at) {
        Some(Change::Take(k)) => bit(taken.item(*k)).unwrap_or(0),
        _ => 0,
    };
    // The nodes: the blocks from which a path leaves the function, each listed after a block it
    // has an edge to, so that a first visit mostly finds its successors' facts.
    let exits = (f.blocks.iter())
        .filter(|b| matches!(b.terminator, Terminator::Return(_) | Terminator::Raise(_)))
        .map(|b| b.id);
    let mut leaving: Vec<BlockId> = Vec::new();
    // Unmetered: the walk ends, visiting every such block.
    let _ = reach(
        budget,
        exits,
        |b, out| out.extend(shape.preds(b)),
        |b| {
            leaving.push(b);
            Visit::<()>::Expand
        },
    );
    let mut node: Vec<Option<usize>> = vec![None; f.blocks.len()];
    for (i, b) in leaving.iter().enumerate() {
        node[b.0 as usize] = Some(i);
    }
    // What a block gives back from its entry on: every lock while its fact is not known yet,
    // nothing for a block from which no path leaves.
    let entry = |facts: &Facts<And>, b: BlockId| {
        node[b.0 as usize].map_or(0, |i| facts.get(i).map_or(full, |a| a.0))
    };
    // The state before each instruction of block `b`, then after its last one.
    let before = |b: BlockId, facts: &Facts<And>| -> Vec<u64> {
        let bb = f.block(b);
        let exc = bb.exc_target.map_or(full, |e| entry(facts, e));
        let succs = bb.terminator.successors();
        let after = succs.iter().fold(full, |acc, s| acc & entry(facts, *s));
        let mut state = if succs.is_empty() { 0 } else { after & exc };
        let mut out = vec![state; bb.insts.len() + 1];
        for i in (0..bb.insts.len()).rev() {
            let at = (bb.id, i);
            state = ((state & !retaken(at)) | released.get(&at).copied().unwrap_or(0)) & exc;
            out[i] = state;
        }
        out
    };
    let readers = |i: usize, out: &mut Vec<usize>| {
        out.extend((shape.preds(leaving[i]).iter()).filter_map(|p| node[p.0 as usize]));
    };
    let transfer = |i: usize, facts: &Facts<And>| {
        let b = leaving[i];
        budget.take(f.block(b).insts.len());
        Some(And(before(b, facts)[0]))
    };
    // A block is visited once, then once per fact one of the three blocks it reads (two
    // successors and the exception target) gets or bit it loses.
    let spec = Sparse {
        nodes: leaving.len(),
        readers: &readers,
        max_visits: 1 + 3 * (1 + 63),
    };
    // Unmetered and within the visit limit, the dataflow is never open.
    fixpoint(budget, spec, transfer)
        .map(|facts| {
            let mut out = FxHashMap::default();
            for &b in &leaving {
                let states = before(b, &facts);
                for (i, state) in states[..states.len() - 1].iter().enumerate() {
                    let held: Vec<LockKey> = (0..keys.len())
                        .filter(|k| state & (1u64 << k) != 0)
                        .map(|k| keys[k].clone())
                        .collect();
                    if !held.is_empty() {
                        out.insert((b, i), held);
                    }
                }
            }
            out
        })
        .open_means(FxHashMap::default())
}
