//! The non-reentrant locks a function takes (`EFF109`, docs/ANALYSIS_AND_KB_SPEC.md section 2.2).
//!
//! A lock is named by where the function reaches it ([`ObjRef`]): an attribute of a parameter
//! (`self._lock`), a module global (`_LOCK`, `locks._LOCK`), or a variable of an enclosing
//! function that closures capture. An
//! acquisition is a `with` or `async with` over an instance of a catalog class flagged
//! `non_reentrant_lock`, an awaited `acquire()` of one, or a blocking `acquire()` the catalog
//! lists. [`LockFacts::acquires`] holds the function's own acquisitions and those of the calls
//! that run in its body on its own thread or task, in the function's names: a callee's
//! parameter becomes the caller's argument; a global and a cell stay what they are. A call that
//! only creates a coroutine or a generator, a function run on a worker thread, and a callable
//! handed to code heki does not follow take nothing at the call.
//!
//! A name is the same object on both sides only while nothing binds it again: a function that
//! may bind a name ([`LockFacts::rebinds`] for a global or a closure variable,
//! `EffectSummary::mutated_param_paths` for an attribute of a parameter), itself or through its
//! calls, takes no lock under that name for its callers, and a holder that binds it, or calls
//! such a function, no longer holds what the name reaches. The tests of state on every path to
//! an acquisition ([`StateTests`]) are those of the acquiring function and of every function on
//! the way from the holder's call: each call site keeps the tests on every path to it.

use crate::guards;
use crate::held::{Acquisitions, Change};
use crate::ownership::Live;
use crate::resolver::{TypeCtx, Val};
use crate::{
    BoundCallee, CallHop, CallSite, EffectGuards, FuncKey, InputGuard, Runner, SharedChain,
};
use crate::{callgraph, owned_chain};
use heki_ast::Span;
use heki_ir::walk::Answer;
use heki_ir::{
    BlockId, ConstLiteral, DominatorTree, Inst, InstKind, IrFunction, Terminator, VarId,
};
use heki_kb::ExecEffectKind;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

/// An object a function reaches from its inputs or from state that outlives its calls.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ObjRef {
    /// Parameter `param` itself (`None`), or its attribute `attr` (`self._lock`).
    Param { param: u8, attr: Option<String> },
    /// A module global, by its fully qualified name (`pkg.locks._LOCK`), read by its name or
    /// on its module.
    Global(String),
    /// The variable `name` of the function `owner` (fully qualified) that its closures capture.
    Cell { owner: String, name: String },
}

impl ObjRef {
    /// The same object in the names of a caller whose call binds the callee's parameters as
    /// `bc` says (`None`: the caller has no name for it).
    pub fn through(&self, bc: &BoundCallee) -> Option<ObjRef> {
        match self {
            Self::Param { param, attr } => {
                let (p, outer) = bc.params.get(usize::from(*param))?.param.as_ref()?;
                match (outer, attr) {
                    (None, attr) => Some(Self::Param {
                        param: *p,
                        attr: attr.clone(),
                    }),
                    (Some(a), None) => Some(Self::Param {
                        param: *p,
                        attr: Some(a.clone()),
                    }),
                    (Some(_), Some(_)) => None,
                }
            }
            Self::Global(_) | Self::Cell { .. } => Some(self.clone()),
        }
    }

    /// How the code of `f` names the object: `self._lock`, `_LOCK`, `lock`.
    pub fn name_in(&self, f: &IrFunction) -> String {
        match self {
            Self::Param { param, attr } => {
                let p = f
                    .params
                    .get(usize::from(*param))
                    .map_or("?", |p| p.name.as_str());
                match attr {
                    Some(a) => format!("{p}.{a}"),
                    None => p.to_owned(),
                }
            }
            Self::Global(fqn) => fqn.clone(),
            Self::Cell { name, .. } => name.clone(),
        }
    }
}

/// One lock a function takes ([`LockFacts::acquires`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockAcquire {
    /// The lock's catalog class (`threading.Lock`).
    pub class: String,
    /// The calls from the function to the acquisition, then the acquisition itself: a hop from
    /// the acquiring function to the lock's class, at the `with` item or the `acquire()` call.
    pub chain: SharedChain,
    /// Tests of the function's inputs that every acquisition needs (`guards`; `once_per` is
    /// never set): a call whose argument decides one against it takes nothing.
    pub guards: EffectGuards,
    /// The tests of state on every path to every acquisition, in the acquiring function and in
    /// the functions on the way to it; open past the most tests kept, where every test may be
    /// one.
    pub tests: Answer<OwnerTests>,
}

/// The tests of state on the way to the acquisitions of one lock. Code changing one of them
/// before it gets there may keep it from taking the lock.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerTests {
    /// For a lock held by a parameter (`self._lock`): the attributes of that object tested
    /// (`if self._closed: return`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub attrs: BTreeSet<String>,
    /// For a lock held by a parameter: the project methods called on that object whose result
    /// is tested (`if self._owns_lock(): return`); what they read counts as tested.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub calls: BTreeSet<FuncKey>,
    /// The module globals (fully qualified) tested (`if _LIB is not None: return _LIB`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub globals: BTreeSet<String>,
}

impl LockAcquire {
    /// Copy with every source location zeroed (Early Cutoff).
    pub(crate) fn without_locations(&self) -> Self {
        let mut out = self.clone();
        crate::zero_hop_spans(&mut out.chain);
        out
    }
}

/// A function's `EFF109` facts (`EffectSummary::locks`; computed only while the rule runs,
/// `OptInFacts::locks`, and `rebinds` while `EFF110` does too). Both are propagated to callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockFacts {
    /// The locks the function takes when its body runs, itself or through the calls that run
    /// there on its thread or task, in its own names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub acquires: BTreeMap<ObjRef, LockAcquire>,
    /// The module globals and closure variables the function may bind or delete, itself or
    /// through its calls. The attributes of parameters it may bind are in
    /// `EffectSummary::mutated_param_paths`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub rebinds: BTreeSet<ObjRef>,
}

pub(crate) static NO_LOCK_FACTS: LockFacts = LockFacts {
    acquires: BTreeMap::new(),
    rebinds: BTreeSet::new(),
};

impl LockFacts {
    /// Copy with every source location zeroed (Early Cutoff).
    pub(crate) fn without_locations(&self) -> Self {
        Self {
            acquires: (self.acquires.iter())
                .map(|(lock, acq)| (lock.clone(), acq.without_locations()))
                .collect(),
            rebinds: self.rebinds.clone(),
        }
    }

    /// Add the facts of `other`: its acquisitions ([`add_acquisition`]) and what it binds.
    pub(crate) fn absorb(&mut self, other: LockFacts) {
        for (lock, acq) in other.acquires {
            add_acquisition(&mut self.acquires, lock, acq);
        }
        self.rebinds.extend(other.rebinds);
    }
}

/// Whether the function with facts `facts` may bind the name `lock` (its own names): then the
/// lock its body takes (or the object it uses, `EFF110`) under that name may be another object
/// than the one the name reached at the call.
pub fn may_rebind(facts: &crate::EffectSummary, lock: &ObjRef) -> bool {
    match lock {
        ObjRef::Param {
            param,
            attr: Some(attr),
        } => facts
            .mutated_param_paths
            .get(param)
            .is_some_and(|paths| paths.contains(attr)),
        ObjRef::Param { attr: None, .. } => false,
        ObjRef::Global(_) | ObjRef::Cell { .. } => facts.lock_facts().rebinds.contains(lock),
    }
}

/// Add one more acquisition of `lock`: the guards, tests, and calls every acquisition needs,
/// and the shortest chain.
pub(crate) fn add_acquisition(
    acquires: &mut BTreeMap<ObjRef, LockAcquire>,
    lock: ObjRef,
    acq: LockAcquire,
) {
    match acquires.entry(lock) {
        Entry::Vacant(e) => {
            e.insert(acq);
        }
        Entry::Occupied(mut e) => {
            let cur = e.get_mut();
            cur.guards.inputs.retain(|g| acq.guards.inputs.contains(g));
            // The tests on the way to every acquisition: an open answer stands for every test,
            // so the other acquisition's tests are the common ones.
            cur.tests = match (std::mem::replace(&mut cur.tests, Answer::Open), acq.tests) {
                (Answer::Known(mut a), Answer::Known(b)) => {
                    a.attrs.retain(|x| b.attrs.contains(x));
                    a.calls.retain(|k| b.calls.contains(k));
                    a.globals.retain(|g| b.globals.contains(g));
                    Answer::Known(a)
                }
                (Answer::Open, other) | (other, Answer::Open) => other,
            };
            if acq.chain.len() < cur.chain.len() {
                cur.chain = acq.chain;
                cur.class = acq.class;
            }
        }
    }
}

/// What one acquisition of a callee becomes in the caller at `site` (bound by `bc`): the lock
/// in the caller's names, with the call in front of its chain and the guards mapped across the
/// call. `None` when the callee runs on a worker thread, the caller has no name for the lock,
/// or an argument decides a guard against the acquisition. `own_writes` are the caller's own
/// writes of its parameters' attributes: a test of one of those is no guard on its input.
#[allow(clippy::too_many_arguments)]
pub(crate) fn through_call(
    site: &CallSite,
    bc: &BoundCallee,
    lock: &ObjRef,
    acq: &LockAcquire,
    lead: &[Arc<CallHop>],
    rewrite: bool,
    own_writes: &BTreeMap<u8, BTreeSet<String>>,
) -> Option<(ObjRef, LockAcquire)> {
    if site.runner == Runner::Worker {
        return None;
    }
    let mapped = lock.through(bc)?;
    let mut through = guards::through_call(site, bc, Some(&acq.guards))?;
    through.guards.once_per = None;
    through.guards.inputs.retain(|g| !assigned(g, own_writes));
    Some((
        mapped,
        LockAcquire {
            class: acq.class.clone(),
            chain: callgraph::extend_chain(lead, &acq.chain, rewrite),
            guards: through.guards,
            tests: acq.tests.clone(),
        },
    ))
}

/// A guard on an attribute the function assigns itself.
fn assigned(g: &InputGuard, own_writes: &BTreeMap<u8, BTreeSet<String>>) -> bool {
    g.subject.attr.as_ref().is_some_and(|a| {
        own_writes
            .get(&g.subject.param)
            .is_some_and(|paths| paths.contains(a))
    })
}

/// One lock the callees of a call site take there ([`crate::ProjectIndex::site_locks`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteLock {
    /// The lock, in the names of the site's caller.
    pub lock: ObjRef,
    /// Starts with the hop of this call site; ends at the acquisition.
    pub chain: Vec<CallHop>,
    pub class: String,
    /// [`LockAcquire::tests`].
    pub tests: Answer<OwnerTests>,
}

impl crate::ProjectIndex {
    /// The locks the project callees of `site` (in file `path`) take in its caller's body, on
    /// the caller's thread or task ([`Self::delivery`]), in the caller's names; not one taken
    /// under a name the callee may bind first ([`may_rebind`]).
    pub fn site_locks(&self, path: &Path, site: &CallSite) -> Vec<SiteLock> {
        let mut out = Vec::new();
        for bc in site.project_callees() {
            let Some((callee, d, facts)) = self.summary(&bc.callee).and_then(|callee| {
                let d = self
                    .delivery(site, callee)
                    .filter(|d| d.into == crate::Facts::Body)?;
                Some((callee, d, callee.facts(d.from)?))
            }) else {
                continue;
            };
            let lead = callgraph::lead_hops(site, path, &bc.callee, d.via);
            let rewrite = callgraph::rewrites(callee, d.from);
            for (lock, acq) in &facts.lock_facts().acquires {
                if may_rebind(facts, lock) {
                    continue;
                }
                let Some((lock, acq)) =
                    through_call(site, bc, lock, acq, &lead, rewrite, &BTreeMap::new())
                else {
                    continue;
                };
                out.push(SiteLock {
                    lock,
                    chain: owned_chain(&acq.chain),
                    class: acq.class,
                    tests: acq.tests,
                });
            }
        }
        out
    }

    /// The names of the caller of `site` that its project callees may bind ([`may_rebind`]),
    /// in the caller's names: an attribute of a parameter, a module global, a closure variable.
    pub fn site_rebinds(&self, site: &CallSite) -> Vec<ObjRef> {
        let callees = site
            .project_callees()
            .iter()
            .filter_map(|bc| Some((bc, self.summary(&bc.callee)?)));
        callees
            .flat_map(|(bc, callee)| {
                let attrs = callee
                    .mutated_param_paths
                    .iter()
                    .flat_map(|(param, paths)| {
                        (paths.iter().filter(|a| !a.is_empty())).map(|a| ObjRef::Param {
                            param: *param,
                            attr: Some(a.clone()),
                        })
                    });
                let names = attrs.chain(callee.lock_facts().rebinds.iter().cloned());
                names
                    .filter_map(|name| name.through(bc))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

/// What an acquisition holds: the lock and its class.
#[derive(Debug, Clone, PartialEq)]
pub struct Taken {
    pub lock: ObjRef,
    pub class: String,
}

/// A function's lock acquisitions, what each instruction does to them, and the lock each
/// acquiring instruction takes (also one a `finally` copy takes again) and each `release()`
/// gives back.
pub struct LockSteps {
    pub taken: Acquisitions<Taken>,
    pub changes: FxHashMap<(BlockId, usize), Change>,
    pub takes: FxHashMap<(BlockId, usize), ObjRef>,
    pub releases: FxHashMap<(BlockId, usize), ObjRef>,
}

/// The lock acquisitions of `f` (the function `key`) in the blocks a run reaches, and what
/// each instruction does to them: a `with` or an `acquire()` takes one, a `release()` gives
/// back the acquisitions of its lock, an `ExitWith` those of its `with` statement, and an
/// instruction binding the name of a lock those of that name, which no longer reaches the held
/// object: `rebound` gives the names an instruction binds ([`rebound_name`]) and its calls may
/// bind (`ProjectIndex::site_rebinds`), empty where only whether a lock is held matters.
pub fn lock_steps<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    key: &FuncKey,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    live: &Live,
    rebound: &dyn Fn(&Inst) -> Vec<ObjRef>,
) -> LockSteps {
    let class_of = |v: VarId| lock_class(tcx, f, types, def, v);
    let mut steps = LockSteps {
        taken: Acquisitions::default(),
        changes: FxHashMap::default(),
        takes: FxHashMap::default(),
        releases: FxHashMap::default(),
    };
    let mut bound = Vec::new();
    for bb in f.blocks.iter().filter(|b| live.block(b.id)) {
        for (i, inst) in bb.insts.iter().enumerate() {
            match lock_step(tcx, &inst.kind, def, &class_of) {
                Some(LockStep::Acquire(v)) => {
                    let (Some(class), Some(lock)) =
                        (class_of(v), obj_ref(tcx, f, key, types, def, v))
                    else {
                        continue;
                    };
                    let mgr = matches!(inst.kind, InstKind::EnterWith { .. }).then_some(v);
                    let taken = Taken {
                        lock: lock.clone(),
                        class,
                    };
                    let k = steps.taken.add(inst.span, mgr, taken);
                    steps.changes.insert((bb.id, i), Change::Take(k));
                    steps.takes.insert((bb.id, i), lock);
                }
                Some(LockStep::Release(v)) => {
                    if let Some(lock) = obj_ref(tcx, f, key, types, def, v) {
                        steps.releases.insert((bb.id, i), lock);
                    }
                }
                None => {
                    let names = rebound(inst);
                    if !names.is_empty() {
                        bound.push(((bb.id, i), names));
                    }
                }
            }
        }
    }
    for (at, names) in bound {
        let given = steps.taken.released(|t| names.contains(&t.lock));
        steps.changes.insert(at, Change::Give(given));
    }
    for bb in &f.blocks {
        for (i, inst) in bb.insts.iter().enumerate() {
            if let InstKind::ExitWith { mgr, .. } = inst.kind {
                let given = steps.taken.exited(mgr, inst.span);
                steps.changes.insert((bb.id, i), Change::Give(given));
            }
        }
    }
    for (at, lock) in &steps.releases {
        let given = steps.taken.released(|t| t.lock == *lock);
        steps.changes.insert(*at, Change::Give(given));
    }
    steps
}

/// The name an instruction of `f` (the function `key`) binds or deletes, as [`obj_ref`] names
/// objects: an attribute of a parameter or of a module, a module global, or a variable that
/// closures capture (in the function binding it, or `nonlocal`).
pub fn rebound_name<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    key: &FuncKey,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    kind: &InstKind,
) -> Option<ObjRef> {
    let own_cell = |name: &str| {
        f.captured_vars
            .iter()
            .any(|c| c == name)
            .then(|| ObjRef::Cell {
                owner: key.fqn(),
                name: name.to_owned(),
            })
    };
    match kind {
        InstKind::AttrStore { obj, attr, .. } | InstKind::AttrDelete { obj, attr } => {
            let obj = through_copies(def, *obj);
            if let Some(param) = f.params.iter().take(64).position(|p| p.var == obj) {
                return Some(ObjRef::Param {
                    param: u8::try_from(param).ok()?,
                    attr: Some(attr.clone()),
                });
            }
            match types.get(&obj) {
                Some(Val::Ref(fqn)) => Some(ObjRef::Global(
                    tcx.project.canonical(&format!("{fqn}.{attr}")),
                )),
                _ => None,
            }
        }
        InstKind::StoreGlobal { name, .. }
        | InstKind::Delete {
            name, global: true, ..
        } => Some(ObjRef::Global(tcx.global_fqn(name))),
        InstKind::StoreDeref { name, .. } | InstKind::Delete { name, .. } => {
            match tcx.captured(f, name) {
                Some((owner, _)) => Some(ObjRef::Cell {
                    owner,
                    name: name.clone(),
                }),
                None => own_cell(name),
            }
        }
        // Reading a captured variable binds nothing.
        InstKind::LoadDeref { .. } => None,
        _ => own_cell(f.var_name(kind.dst()?)?),
    }
}

/// How an instruction takes or gives back a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStep {
    /// `with lock:` / `async with lock:`, `await lock.acquire()`, a blocking `lock.acquire()`.
    Acquire(VarId),
    /// `lock.release()`.
    Release(VarId),
}

/// The lock step of `kind`, when its value is a non-reentrant lock (`class_of` gives the
/// catalog class of a value, [`lock_class`]). A sync `acquire()` counts only where the catalog
/// says it blocks (`threading.Lock.acquire`), with no argument or `blocking=True`: a timeout or
/// `blocking=False` returns.
pub fn lock_step<'i>(
    tcx: &TypeCtx<'_>,
    kind: &InstKind,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    class_of: &dyn Fn(VarId) -> Option<String>,
) -> Option<LockStep> {
    match kind {
        InstKind::EnterWith { mgr, .. } => {
            class_of(*mgr)?;
            Some(LockStep::Acquire(*mgr))
        }
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            is_awaited,
            ..
        } => {
            let class = class_of(*recv)?;
            match method.as_str() {
                "release" => Some(LockStep::Release(*recv)),
                "acquire" if *is_awaited => {
                    (args.is_empty() && kwargs.is_empty()).then_some(LockStep::Acquire(*recv))
                }
                "acquire" => {
                    let blocks = tcx
                        .kb
                        .lookup(&format!("{class}.acquire"))
                        .is_some_and(|e| e.effects.contains(&ExecEffectKind::BlockingSync));
                    let is_true = |v: &VarId| {
                        matches!(
                            def(*v).map(|i| &i.kind),
                            Some(InstKind::Const {
                                literal: ConstLiteral::Bool(true),
                                ..
                            })
                        )
                    };
                    let waits = match (args.as_slice(), kwargs.as_slice()) {
                        ([], []) => true,
                        ([b], []) => is_true(b),
                        ([], [(Some(k), b)]) => k == "blocking" && is_true(b),
                        _ => false,
                    };
                    (blocks && waits).then_some(LockStep::Acquire(*recv))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// The catalog class of `var` when it is an instance of a non-reentrant lock: from its inferred
/// value (`types`), or, for a closure's variable of an enclosing function, from what that
/// function binds to it.
pub fn lock_class<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    var: VarId,
) -> Option<String> {
    let class = match types.get(&var) {
        Some(Val::Inst(c)) => c.clone(),
        _ => match def(through_copies(def, var)).map(|i| &i.kind) {
            Some(InstKind::LoadDeref { name, .. }) => tcx.captured(f, name)?.1?,
            _ => return None,
        },
    };
    tcx.kb
        .lookup(&class)
        .is_some_and(|e| e.non_reentrant_lock)
        .then_some(class)
}

/// The object `var` is, in the names of `f` (the function `key`): a parameter or its
/// attribute, a module global (read by name or on its module), or a variable of an enclosing
/// function (or of `f`) that closures capture, through copies.
pub fn obj_ref<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    key: &FuncKey,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    var: VarId,
) -> Option<ObjRef> {
    let param = |v: VarId| {
        let v = through_copies(def, v);
        f.params
            .iter()
            .take(64)
            .position(|p| p.var == v)
            .and_then(|i| u8::try_from(i).ok())
    };
    if let Some(p) = param(var) {
        return Some(ObjRef::Param {
            param: p,
            attr: None,
        });
    }
    // A variable of `f` its closures capture is a cell: the name, not the value it holds now.
    let mut cur = var;
    let mut seen = FxHashSet::default();
    while seen.insert(cur) {
        let kind = def(cur).map(|i| &i.kind);
        if let Some(name) = f.var_name(cur)
            && f.captured_vars.iter().any(|c| c == name)
            && !matches!(kind, Some(InstKind::LoadDeref { .. }))
        {
            return Some(ObjRef::Cell {
                owner: key.fqn(),
                name: name.to_owned(),
            });
        }
        match kind {
            Some(InstKind::Copy { src, .. }) => cur = *src,
            _ => break,
        }
    }
    match &def(cur)?.kind {
        InstKind::AttrLoad { obj, attr, .. } => {
            if let Some(p) = param(*obj) {
                return Some(ObjRef::Param {
                    param: p,
                    attr: Some(attr.clone()),
                });
            }
            match types.get(&through_copies(def, *obj)) {
                Some(Val::Ref(fqn)) => Some(ObjRef::Global(
                    tcx.project.canonical(&format!("{fqn}.{attr}")),
                )),
                _ => None,
            }
        }
        InstKind::LoadGlobal { name, .. } => Some(ObjRef::Global(tcx.global_fqn(name))),
        InstKind::LoadDeref { name, .. } => Some(ObjRef::Cell {
            owner: tcx.captured(f, name)?.0,
            name: name.clone(),
        }),
        _ => None,
    }
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

/// The most tests of state a block or an acquisition keeps; past it, every test may be one, and
/// an acquisition behind them counts as guarded ([`LockAcquire::tests`] is open).
const MAX_OWNER_TESTS: usize = 16;

/// The tests of state on every path to a block or a call site: the attributes of the
/// parameters that the branch conditions read, the project methods called on a parameter whose
/// result they test, and the module globals they read. Code changing one of them before it gets
/// there may keep it from going on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTests {
    /// `(parameter, attribute)`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub attrs: BTreeSet<(u8, String)>,
    /// `(parameter, method)`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub calls: BTreeSet<(u8, FuncKey)>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub globals: BTreeSet<String>,
}

impl StateTests {
    fn len(&self) -> usize {
        self.attrs.len() + self.calls.len() + self.globals.len()
    }
}

/// The tests of state on every path to each block of a function a run reaches
/// ([`StateTests`], open past [`MAX_OWNER_TESTS`]): those of its immediate dominator, and what
/// the condition of the branch ending that dominator reads. Blocks below no branch share their
/// dominator's tests, so the walk is one pass over the dominator tree.
pub struct BlockTests {
    dom: DominatorTree,
    /// Per block, in `sets`: the tests on every path to it, and those on every path past its
    /// terminator (its branch read too). The call sites below one branch share its set.
    at: Vec<Option<(usize, usize)>>,
    sets: Vec<Arc<Answer<StateTests>>>,
}

impl BlockTests {
    /// `dom`: the dominator tree over the edges a run can take. `resolve` gives the definitions
    /// of a method called on a value, `global` the fully qualified name of a module global.
    pub fn of<'i>(
        f: &IrFunction,
        dom: DominatorTree,
        def: &dyn Fn(VarId) -> Option<&'i Inst>,
        resolve: &dyn Fn(VarId, &str) -> Vec<FuncKey>,
        global: &dyn Fn(&str) -> String,
    ) -> Self {
        let param_of = |v: VarId| {
            let v = through_copies(def, v);
            (f.params.iter().take(64))
                .position(|p| p.var == v)
                .and_then(|i| u8::try_from(i).ok())
        };
        let mut sets = vec![Arc::new(Answer::Known(StateTests::default()))];
        let mut at: Vec<Option<(usize, usize)>> = vec![None; f.blocks.len()];
        // A dominator comes before the blocks it dominates in reverse postorder.
        for b in f.rpo_blocks() {
            let to = match dom.strict_dominator(b) {
                Some(d) => at[d.0 as usize].expect("a dominator comes first").1,
                None if dom.number(b).is_some() => 0,
                // Only an edge a run cannot take leads there.
                None => continue,
            };
            let past = match f.block(b).terminator {
                Terminator::Branch { cond, .. } => {
                    let tests = Answer::clone(&sets[to]).and_then(|mut tests| {
                        read_condition(def, cond, &param_of, resolve, global, &mut tests);
                        let count = tests.len();
                        kept(tests, count)
                    });
                    sets.push(Arc::new(tests));
                    sets.len() - 1
                }
                _ => to,
            };
            at[b.0 as usize] = Some((to, past));
        }
        Self { dom, at, sets }
    }

    /// The tests on every path to every block of `blocks` (blocks a run reaches): those of
    /// their nearest common dominator, and its branch when it is not one of them.
    pub fn common(&self, blocks: &BTreeSet<BlockId>) -> Option<&Answer<StateTests>> {
        let by_number = |b: &&BlockId| self.dom.number(**b);
        let lo = blocks.iter().min_by_key(by_number)?;
        let hi = blocks.iter().max_by_key(by_number)?;
        let common = self.dom.common(*lo, *hi);
        let (to, past) = self.at[common.0 as usize]?;
        Some(&self.sets[if blocks.contains(&common) { to } else { past }])
    }

    /// The tests on every path to a call site in block `b`, before it: `None` when there is
    /// none, or when no run reaches the block.
    pub fn of_site(&self, b: BlockId) -> Option<Arc<Answer<StateTests>>> {
        let tests = &self.sets[self.at[b.0 as usize]?.0];
        let none = matches!(**tests, Answer::Known(ref t) if t.len() == 0);
        (!none).then(|| Arc::clone(tests))
    }
}

/// What a branch condition `cond` reads into `tests`: the attributes of a parameter
/// (`param_of`), the project methods called on one, and the module globals, through copies,
/// `not`, and comparisons.
fn read_condition<'i>(
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    cond: VarId,
    param_of: &dyn Fn(VarId) -> Option<u8>,
    resolve: &dyn Fn(VarId, &str) -> Vec<FuncKey>,
    global: &dyn Fn(&str) -> String,
    tests: &mut StateTests,
) {
    let mut stack = vec![cond];
    // A value read twice (`x == x`) is read once.
    let mut seen: FxHashSet<VarId> = stack.iter().copied().collect();
    let mut operands = Vec::new();
    while let Some(v) = stack.pop() {
        match def(v).map(|i| &i.kind) {
            Some(InstKind::Copy { src, .. }) => operands.push(*src),
            Some(InstKind::UnaryOp { operand, .. }) => operands.push(*operand),
            Some(InstKind::Compare { lhs, rhs, .. }) => operands.extend([*lhs, *rhs]),
            Some(InstKind::AttrLoad { obj, attr, .. }) => {
                if let Some(p) = param_of(*obj) {
                    tests.attrs.insert((p, attr.clone()));
                }
            }
            Some(InstKind::MethodCall { recv, method, .. }) => {
                if let Some(p) = param_of(*recv) {
                    tests
                        .calls
                        .extend(resolve(*recv, method).into_iter().map(|k| (p, k)));
                }
            }
            Some(InstKind::LoadGlobal { name, .. }) => {
                tests.globals.insert(global(name));
            }
            _ => {}
        }
        stack.extend(operands.drain(..).filter(|w| seen.insert(*w)));
    }
}

/// The tests of state among `tests` on the way to an acquisition of `lock`: the attributes and
/// methods of the lock's owner (the parameter holding it) and the globals.
pub(crate) fn owner_tests(lock: &ObjRef, tests: &Answer<StateTests>) -> Answer<OwnerTests> {
    let owner = match lock {
        ObjRef::Param {
            param,
            attr: Some(_),
        } => Some(*param),
        _ => None,
    };
    let of_owner = |p: &u8| Some(*p) == owner;
    tests.as_ref().map(|t| OwnerTests {
        attrs: (t.attrs.iter())
            .filter(|(p, _)| of_owner(p))
            .map(|(_, a)| a.clone())
            .collect(),
        calls: (t.calls.iter())
            .filter(|(p, _)| of_owner(p))
            .map(|(_, k)| k.clone())
            .collect(),
        globals: t.globals.clone(),
    })
}

/// Add the tests of state `tests` of a call site on the way to an acquisition of `lock`
/// ([`crate::CallSite::state_tests`]): open when either is, or past [`MAX_OWNER_TESTS`].
pub(crate) fn add_owner_tests(acq: &mut LockAcquire, lock: &ObjRef, tests: &Answer<StateTests>) {
    let more = owner_tests(lock, tests);
    let tests = std::mem::replace(&mut acq.tests, Answer::Open);
    acq.tests = tests.zip(more).and_then(|(mut t, more)| {
        t.attrs.extend(more.attrs);
        t.calls.extend(more.calls);
        t.globals.extend(more.globals);
        let count = t.attrs.len() + t.calls.len() + t.globals.len();
        kept(t, count)
    });
}

/// `count` tests of state, open past [`MAX_OWNER_TESTS`].
fn kept<T>(tests: T, count: usize) -> Answer<T> {
    if count > MAX_OWNER_TESTS {
        Answer::Open
    } else {
        Answer::Known(tests)
    }
}

/// The acquisition [`LockAcquire`] for a lock of class `class` taken by the function `caller`
/// at `span`, with its guards (the tests of state on the way are set once every acquisition is
/// known, [`owner_tests`]).
pub(crate) fn own_acquisition(
    caller: &FuncKey,
    path: &Path,
    class: String,
    span: Span,
    guards: EffectGuards,
) -> LockAcquire {
    let hop = CallHop {
        caller: caller.clone(),
        callee: class.clone(),
        span,
        path: path.to_path_buf(),
    };
    LockAcquire {
        class,
        chain: vec![Arc::new(hop)],
        guards: EffectGuards {
            once_per: None,
            ..guards
        },
        tests: Answer::Known(OwnerTests::default()),
    }
}
