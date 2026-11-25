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
use heki_ir::walk::{Budget, FnShape};
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

/// Where `f` (or the method a scope nested in it reads `self` from) runs on an instance: the
/// class, fully qualified, when `var` is that method's first parameter.
fn receiver<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    var: VarId,
) -> Option<Receiver> {
    let var = through_copies(def, var);
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
    match def(var).map(|i| &i.kind) {
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
fn state_of<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    var: VarId,
) -> Option<StateKey> {
    match &def(through_copies(def, var))?.kind {
        InstKind::AttrLoad { obj, attr, .. } => attr_state(tcx, f, types, def, *obj, attr),
        InstKind::LoadGlobal { name, .. } => Some(StateKey::Global(tcx.global_fqn(name))),
        _ => None,
    }
}

/// The attribute `attr` of `obj` as shared state.
fn attr_state<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    obj: VarId,
    attr: &str,
) -> Option<StateKey> {
    match receiver(tcx, f, types, def, obj)? {
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
        Some(Val::Inst(class)) => ContainerKind::of_class(class),
        _ => None,
    }
}

/// The write of shared state `kind` makes in `f`: an assignment or deletion of an attribute of
/// the instance, a class, or a module, or of a global the function declares `global`; an item
/// store or delete, or a mutating method call, on a builtin container such state holds.
pub fn state_written<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    kind: &InstKind,
) -> Option<(StateKey, WriteKind)> {
    let changed = |obj: VarId| {
        container(types, through_copies(def, obj))?;
        Some((state_of(tcx, f, types, def, obj)?, WriteKind::Changes))
    };
    match kind {
        InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr } => {
            if matches!(types.get(&through_copies(def, *obj)), Some(Val::Inst(c)) if thread_local_class(tcx, c))
            {
                return None;
            }
            Some((
                attr_state(tcx, f, types, def, *obj, attr)?,
                WriteKind::Assigns,
            ))
        }
        InstKind::StoreGlobal { name, .. }
        | InstKind::Delete {
            name, global: true, ..
        } if !f.is_module_body => {
            Some((StateKey::Global(tcx.global_fqn(name)), WriteKind::Assigns))
        }
        InstKind::SubscriptStore { obj, .. } | InstKind::SubscriptDelete { obj, .. } => {
            changed(*obj)
        }
        InstKind::MethodCall { recv, method, .. } => {
            let kind = container(types, through_copies(def, *recv))?;
            if !kind.mutators().contains(&method.as_str()) {
                return None;
            }
            changed(*recv)
        }
        _ => None,
    }
}

/// The own writes of shared state of `f` in the blocks a run reaches (`live`).
pub fn state_writes<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    live: &crate::ownership::Live,
) -> Vec<StateWrite> {
    if f.is_module_body || f.is_class_body {
        return Vec::new();
    }
    let mut out = Vec::new();
    for bb in f.blocks.iter().filter(|b| live.block(b.id)) {
        for (i, inst) in bb.insts.iter().enumerate() {
            if let Some((state, kind)) = state_written(tcx, f, types, def, &inst.kind) {
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
/// `sync_lock_or_tx`, or, for a variable a closure reads, bound to one by the enclosing function.
fn is_thread_lock<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    var: VarId,
) -> bool {
    let class = match types.get(&var) {
        Some(Val::Inst(c)) => Some(c.clone()),
        _ => match def(through_copies(def, var)).map(|i| &i.kind) {
            Some(InstKind::LoadDeref { name, .. }) => tcx.captured(f, name).and_then(|c| c.1),
            _ => None,
        },
    };
    class.is_some_and(|c| tcx.catalog(&c).is_some_and(|e| e.sync_lock_or_tx))
}

/// How `f` names the lock `var`.
fn lock_key<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    var: VarId,
) -> LockKey {
    let state = state_of(tcx, f, types, def, var);
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

fn step<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    kind: &InstKind,
) -> Option<Step> {
    match kind {
        InstKind::EnterWith {
            mgr,
            is_async: false,
            ..
        } if is_thread_lock(tcx, f, types, def, *mgr) => Some(Step::Take(*mgr, true)),
        InstKind::ExitWith { mgr, .. } => Some(Step::Exit(*mgr)),
        InstKind::MethodCall {
            recv,
            method,
            is_awaited: false,
            ..
        } if is_thread_lock(tcx, f, types, def, *recv) => match method.as_str() {
            "acquire" => Some(Step::Take(*recv, false)),
            "release" => Some(Step::Release(lock_key(tcx, f, types, def, *recv))),
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
}

impl LockPoints {
    /// The thread lock steps of `f`, with the locks the calls at each instruction take for it
    /// (`taken_by_call`: the callees' [`GuardFacts::returns_holding`] in `f`'s names; nothing
    /// while the callees' facts are not known). One pass over the instructions after the
    /// dataflows, which keep a bit per acquisition and per lock released.
    pub fn of<'i>(
        tcx: &TypeCtx<'_>,
        shape: &FnShape<'_>,
        types: &FxHashMap<VarId, Val>,
        def: &dyn Fn(VarId) -> Option<&'i Inst>,
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
                if let Some(s) = step(tcx, f, types, def, &inst.kind) {
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
                let key = lock_key(tcx, f, types, def, *lock);
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
            released_later(f, &taken, &changes, &releases, &steps)
        };
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

    /// The named locks held on every normal return that the function took itself.
    pub fn returns_holding(&self) -> &BTreeSet<LockKey> {
        &self.returns_holding
    }
}

/// The locks given back by `release()` on every path from each instruction of `f` before they
/// are taken again (a backward must-dataflow, exception edges included, over at most 63 locks).
/// A block from which no path leaves the function gives back nothing. A worklist: a block is
/// visited again only when a successor lost a bit, so the visits are bounded by the blocks times
/// the bits (counted by `budget`).
fn released_later(
    f: &IrFunction,
    taken: &Acquisitions<LockKey>,
    changes: &FxHashMap<(BlockId, usize), Change>,
    releases: &[((BlockId, usize), LockKey)],
    budget: &Budget,
) -> FxHashMap<(BlockId, usize), Vec<LockKey>> {
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
    let n = f.blocks.len();
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for bb in &f.blocks {
        for s in bb.terminator.successors().into_iter().chain(bb.exc_target) {
            preds[s.0 as usize].push(bb.id.0 as usize);
        }
    }
    // The blocks from which a path leaves the function.
    let mut leaves = vec![false; n];
    let mut queue: Vec<usize> = (f.blocks.iter())
        .filter(|b| matches!(b.terminator, Terminator::Return(_) | Terminator::Raise(_)))
        .map(|b| b.id.0 as usize)
        .collect();
    while let Some(b) = queue.pop() {
        if !std::mem::replace(&mut leaves[b], true) {
            queue.extend(&preds[b]);
        }
    }
    // What block `b` gives back after its last instruction, from its successors' entries, and
    // what its exception edge does after each instruction.
    let exit_of = |b: usize, ins: &[u64]| -> (u64, u64) {
        let bb = &f.blocks[b];
        let exc = bb.exc_target.map_or(full, |e| ins[e.0 as usize]);
        let succs = bb.terminator.successors();
        let after = succs.iter().fold(full, |acc, s| acc & ins[s.0 as usize]);
        (if succs.is_empty() { 0 } else { after & exc }, exc)
    };
    // The state before each instruction of block `b`.
    let before = |b: usize, ins: &[u64]| -> Vec<u64> {
        let bb = &f.blocks[b];
        let (mut state, exc) = exit_of(b, ins);
        let mut out = vec![0; bb.insts.len()];
        for i in (0..bb.insts.len()).rev() {
            let at = (bb.id, i);
            state = ((state & !retaken(at)) | released.get(&at).copied().unwrap_or(0)) & exc;
            out[i] = state;
        }
        out
    };
    let entry_of = |b: usize, ins: &[u64]| {
        let states = before(b, ins);
        states.first().copied().unwrap_or_else(|| exit_of(b, ins).0)
    };
    let mut ins: Vec<u64> = (0..n).map(|b| if leaves[b] { full } else { 0 }).collect();
    let mut work: Vec<usize> = (0..n).filter(|b| leaves[*b]).collect();
    let mut queued = leaves.clone();
    while let Some(b) = work.pop() {
        queued[b] = false;
        budget.take(1 + f.blocks[b].insts.len());
        let now = entry_of(b, &ins);
        if now != ins[b] {
            ins[b] = now;
            for &p in &preds[b] {
                if leaves[p] && !std::mem::replace(&mut queued[p], true) {
                    work.push(p);
                }
            }
        }
    }
    let mut out = FxHashMap::default();
    for b in (0..n).filter(|b| leaves[*b]) {
        let id = f.blocks[b].id;
        for (i, state) in before(b, &ins).into_iter().enumerate() {
            let held: Vec<LockKey> = (0..keys.len())
                .filter(|k| state & (1u64 << k) != 0)
                .map(|k| keys[k].clone())
                .collect();
            if !held.is_empty() {
                out.insert((id, i), held);
            }
        }
    }
    out
}

/// `var` before the copies that produced it.
fn through_copies<'i>(def: &dyn Fn(VarId) -> Option<&'i Inst>, mut var: VarId) -> VarId {
    let mut seen = FxHashSet::default();
    while seen.insert(var) {
        match def(var).map(|i| &i.kind) {
            Some(InstKind::Copy { src, .. }) => var = *src,
            _ => break,
        }
    }
    var
}
