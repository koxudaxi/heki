//! The non-reentrant locks a function takes (`EFF109`, docs/ANALYSIS_AND_KB_SPEC.md section 2.2).
//!
//! A lock is named by where the function reaches it ([`ObjRef`]): an attribute of a parameter
//! (`self._lock`), a module global (`_LOCK`, `locks._LOCK`), or a variable of an enclosing
//! function that closures capture. An
//! acquisition is a `with` or `async with` over an instance of a catalog class flagged
//! `non_reentrant_lock`, an awaited `acquire()` of one, or a blocking `acquire()` the catalog
//! lists. `EffectSummary::acquires_locks` holds the function's own acquisitions and those of the
//! calls that run in its body on its own thread or task, in the function's names: a callee's
//! parameter becomes the caller's argument; a global and a cell stay what they are. A call that
//! only creates a coroutine or a generator, a function run on a worker thread, and a callable
//! handed to code heki does not follow take nothing at the call.

use crate::guards;
use crate::held::{Acquisitions, Change};
use crate::ownership::Live;
use crate::resolver::{TypeCtx, Val};
use crate::{
    BoundCallee, CallHop, CallSite, EffectGuards, FuncKey, InputGuard, Runner, SharedChain,
};
use crate::{callgraph, owned_chain};
use heki_ast::Span;
use heki_ir::{BlockId, ConstLiteral, Inst, InstKind, IrFunction, Terminator, VarId};
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

/// One lock a function takes (`EffectSummary::acquires_locks`).
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
    /// For a lock held by a parameter (`self._lock`): the attributes of that object tested on
    /// every path to every acquisition (`if self._closed: return`). Code changing one of them
    /// before it gets there may keep it from taking the lock.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tested_attrs: BTreeSet<String>,
    /// For a lock held by a parameter: the project methods called on that object whose result
    /// is tested on every path to every acquisition (`if self._owns_lock(): return`); what they
    /// read counts as tested.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tested_calls: BTreeSet<FuncKey>,
    /// The module globals (fully qualified) tested on every path to every acquisition
    /// (`if _LIB is not None: return _LIB`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tested_globals: BTreeSet<String>,
    /// `tested_attrs` and `tested_calls` are every test. False past the most tests kept: every
    /// test may be one, and the acquisition counts as guarded.
    pub tests_known: bool,
}

impl LockAcquire {
    /// Copy with every source location zeroed (Early Cutoff).
    pub(crate) fn without_locations(&self) -> Self {
        let mut out = self.clone();
        crate::zero_hop_spans(&mut out.chain);
        out
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
            // Tests that are not known stand for every test.
            if !cur.tests_known {
                cur.tested_attrs.clone_from(&acq.tested_attrs);
                cur.tested_calls.clone_from(&acq.tested_calls);
                cur.tested_globals.clone_from(&acq.tested_globals);
                cur.tests_known = acq.tests_known;
            } else if acq.tests_known {
                cur.tested_attrs.retain(|a| acq.tested_attrs.contains(a));
                cur.tested_calls.retain(|k| acq.tested_calls.contains(k));
                cur.tested_globals
                    .retain(|g| acq.tested_globals.contains(g));
            }
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
            tested_attrs: acq.tested_attrs.clone(),
            tested_calls: acq.tested_calls.clone(),
            tested_globals: acq.tested_globals.clone(),
            tests_known: acq.tests_known,
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
    pub tested_attrs: BTreeSet<String>,
    pub tested_calls: BTreeSet<FuncKey>,
    pub tested_globals: BTreeSet<String>,
    /// [`LockAcquire::tests_known`].
    pub tests_known: bool,
}

impl crate::ProjectIndex {
    /// The locks the project callees of `site` (in file `path`) take in its caller's body, on
    /// the caller's thread or task ([`Self::delivery`]), in the caller's names.
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
            for (lock, acq) in &facts.acquires_locks {
                let Some((lock, acq)) =
                    through_call(site, bc, lock, acq, &lead, rewrite, &BTreeMap::new())
                else {
                    continue;
                };
                out.push(SiteLock {
                    lock,
                    chain: owned_chain(&acq.chain),
                    class: acq.class,
                    tested_attrs: acq.tested_attrs,
                    tested_calls: acq.tested_calls,
                    tested_globals: acq.tested_globals,
                    tests_known: acq.tests_known,
                });
            }
        }
        out
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
/// back the `acquire()`s of its lock, and an `ExitWith` the acquisitions of its `with`
/// statement.
pub fn lock_steps<'i>(
    tcx: &TypeCtx<'_>,
    f: &IrFunction,
    key: &FuncKey,
    types: &FxHashMap<VarId, Val>,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    live: &Live,
) -> LockSteps {
    let class_of = |v: VarId| lock_class(tcx, f, types, def, v);
    let mut steps = LockSteps {
        taken: Acquisitions::default(),
        changes: FxHashMap::default(),
        takes: FxHashMap::default(),
        releases: FxHashMap::default(),
    };
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
                None => {}
            }
        }
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

/// The most tests of a lock owner's state an acquisition keeps; past it, the acquisition counts
/// as guarded ([`LockAcquire::tests_known`] is false).
const MAX_OWNER_TESTS: usize = 16;

/// The dominator tree of a function's reachable blocks: each block's immediate dominator, its
/// depth, and its position in a preorder walk.
pub struct DomTree {
    idom: Vec<Option<BlockId>>,
    depth: Vec<u32>,
    pre: Vec<u32>,
}

impl DomTree {
    /// From the immediate dominators (`None` for a block a run does not reach).
    pub fn new(f: &IrFunction, idom: Vec<Option<BlockId>>) -> Self {
        let n = f.blocks.len();
        let mut depth = vec![0u32; n];
        let mut children: Vec<Vec<BlockId>> = vec![Vec::new(); n];
        let mut root = None;
        // An immediate dominator comes before its blocks in reverse postorder.
        for b in f.rpo_blocks() {
            match idom[b.0 as usize] {
                Some(d) if d == b => root = Some(b),
                Some(d) => {
                    depth[b.0 as usize] = depth[d.0 as usize] + 1;
                    children[d.0 as usize].push(b);
                }
                None => {}
            }
        }
        let mut pre = vec![u32::MAX; n];
        let mut next = 0u32;
        let mut stack: Vec<BlockId> = root.into_iter().collect();
        while let Some(b) = stack.pop() {
            pre[b.0 as usize] = next;
            next += 1;
            stack.extend(children[b.0 as usize].iter().rev().copied());
        }
        Self { idom, depth, pre }
    }

    fn up(&self, b: BlockId) -> Option<BlockId> {
        self.idom[b.0 as usize].filter(|d| *d != b)
    }

    /// The nearest block dominating both (blocks a run reaches).
    fn common(&self, mut a: BlockId, mut b: BlockId) -> BlockId {
        let up = |x: BlockId| self.up(x).expect("two blocks of one tree meet at its root");
        while a != b {
            let (da, db) = (self.depth[a.0 as usize], self.depth[b.0 as usize]);
            if da >= db {
                a = up(a);
            }
            if db >= da {
                b = up(b);
            }
        }
        a
    }
}

/// The tests of state on every path to every block of `blocks` (the acquisitions of one lock,
/// in blocks a run reaches) ([`LockAcquire::tested_attrs`], `tested_calls`, `tested_globals`).
#[derive(Debug, Default)]
pub struct StateTests {
    pub attrs: BTreeSet<String>,
    pub calls: BTreeSet<FuncKey>,
    pub globals: BTreeSet<String>,
}

/// The tests of state on every path to every block of `blocks`: the attributes the branch
/// conditions read on parameter `owner` (the lock's owner, when a parameter holds the lock), the
/// project methods called on it whose result they test (`resolve` gives a method's
/// definitions), and the module globals they read (`global` gives a name's fully qualified
/// one). `None` past [`MAX_OWNER_TESTS`]. One walk up the dominator tree from the blocks'
/// common dominator: a block dominating them all ends in a branch before each of them, unless
/// it is one of them.
pub fn owner_tests<'i>(
    f: &IrFunction,
    dom: &DomTree,
    def: &dyn Fn(VarId) -> Option<&'i Inst>,
    owner: Option<u8>,
    blocks: &BTreeSet<BlockId>,
    resolve: &dyn Fn(VarId, &str) -> Vec<FuncKey>,
    global: &dyn Fn(&str) -> String,
) -> Option<StateTests> {
    let mut tests = StateTests::default();
    let owner = owner
        .and_then(|p| f.params.get(usize::from(p)))
        .map(|p| p.var);
    let by_pre = |b: &&BlockId| dom.pre[b.0 as usize];
    let lo = blocks.iter().min_by_key(by_pre)?;
    let hi = blocks.iter().max_by_key(by_pre)?;
    let common = dom.common(*lo, *hi);
    let mut cur = if blocks.contains(&common) {
        dom.up(common)
    } else {
        Some(common)
    };
    let is_owner = |v: VarId| Some(through_copies(def, v)) == owner;
    let mut seen = FxHashSet::default();
    while let Some(d) = cur {
        cur = dom.up(d);
        let Terminator::Branch { cond, .. } = f.block(d).terminator else {
            continue;
        };
        let mut stack = vec![cond];
        while let Some(v) = stack.pop() {
            if !seen.insert(v) {
                continue;
            }
            match def(v).map(|i| &i.kind) {
                Some(InstKind::Copy { src, .. }) => stack.push(*src),
                Some(InstKind::UnaryOp { operand, .. }) => stack.push(*operand),
                Some(InstKind::Compare { lhs, rhs, .. }) => stack.extend([*lhs, *rhs]),
                Some(InstKind::AttrLoad { obj, attr, .. }) if is_owner(*obj) => {
                    tests.attrs.insert(attr.clone());
                }
                Some(InstKind::MethodCall { recv, method, .. }) if is_owner(*recv) => {
                    tests.calls.extend(resolve(*recv, method));
                }
                Some(InstKind::LoadGlobal { name, .. }) => {
                    tests.globals.insert(global(name));
                }
                _ => {}
            }
        }
        if tests.attrs.len() + tests.calls.len() + tests.globals.len() > MAX_OWNER_TESTS {
            return None;
        }
    }
    Some(tests)
}

/// The acquisition [`LockAcquire`] for a lock of class `class` taken by the function `caller`
/// at `span`, with its guards (the tests of the lock owner's state are added once every
/// acquisition is known, [`owner_tests`]).
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
        tested_attrs: BTreeSet::new(),
        tested_calls: BTreeSet::new(),
        tested_globals: BTreeSet::new(),
        tests_known: true,
    }
}
