//! `EFF110` (`exclusive-object-fanout`, opt-in): an object that allows one operation at a time,
//! used by tasks a function starts together.
//!
//! A fan-out starts tasks that run at the same time: `asyncio.gather(...)`, `asyncio.wait(...)`
//! over the tasks of a display, the `create_task` calls of one `asyncio.TaskGroup`, and the
//! `start_soon` calls of one anyio task group. Each task (a sibling) is a coroutine a project
//! call creates, a `create_task` of one, a call a task group starts, or a one-at-a-time call
//! itself; one in a loop or a comprehension starts several. The objects a sibling uses come from
//! its callee's summary (`EffectSummary::exclusive`), mapped through the call's arguments to
//! what the fan-out function names: a parameter or its attribute, a global, a closure variable,
//! or one of its local variables. Two siblings, or one started several times, using the same
//! object is reported, unless the object is the loop's own item there (a handle per task).
//!
//! An object allowing one operation only while it holds no connection (SQLAlchemy's
//! `AsyncSession`) is reported only where the fan-out can be its first operation: unless every
//! path to the fan-out uses it after it was made or last gave its connection back (`commit()`,
//! `rollback()`, `close()`).

use crate::{CheckContext, ModuleUnit};
use heki_ast::Span;
use heki_diagnostics::{Diagnostic, RelatedLocation};
use heki_effect::consumption::resolve_inst;
use heki_effect::held::{self, Change};
use heki_effect::locks::{self, ObjRef};
use heki_effect::one_at_a_time::{self, ExclusiveUse};
use heki_effect::ownership::Live;
use heki_effect::{CallHop, Callee, FuncKey, TypeCtx, Val, owned_chain};
use heki_ir::walk::FnShape;
use heki_ir::{BlockId, FunctionId, Inst, InstKind, IrFunction, VarId};
use heki_kb::OneAtATime;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;

/// The fan-out APIs, by the catalog symbol their call resolves to.
const GATHER: &str = "asyncio.gather";
const WAIT: &str = "asyncio.wait";
const TASK_GROUP_CREATE: &str = "asyncio.TaskGroup.create_task";
const ANYIO_START_SOON: &str = "anyio.abc.TaskGroup.start_soon";
/// Calls wrapping a coroutine in a task that starts it (their first argument).
const TASK_FACTORIES: &[&str] = &[
    "asyncio.create_task",
    "asyncio.ensure_future",
    "asyncio.AbstractEventLoop.create_task",
    "asyncio.BaseEventLoop.create_task",
];

/// What an object a sibling uses is to the fan-out function.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Holder {
    /// A name outside the function's frame: a parameter or its attribute, a global, a closure
    /// variable.
    Obj(ObjRef),
    /// A local variable (its value before copies), or an attribute of one.
    Local { root: VarId, attr: Option<String> },
}

impl Holder {
    /// The attribute `attr` of the object, one level deep.
    fn attr(self, attr: &str) -> Option<Holder> {
        match self {
            Holder::Obj(ObjRef::Param { param, attr: None }) => Some(Holder::Obj(ObjRef::Param {
                param,
                attr: Some(attr.to_owned()),
            })),
            Holder::Local { root, attr: None } => Some(Holder::Local {
                root,
                attr: Some(attr.to_owned()),
            }),
            _ => None,
        }
    }
}

/// A function's IR with its definitions and inferred values.
struct Frame<'f> {
    f: &'f IrFunction,
    shape: FnShape<'f>,
    key: FuncKey,
    types: FxHashMap<VarId, Val>,
    defs: FxHashMap<VarId, &'f Inst>,
    /// Where each variable an instruction defines is defined.
    at: FxHashMap<VarId, Position>,
}

/// A point of a function: (block, instruction index).
type Position = (BlockId, usize);

impl<'f> Frame<'f> {
    fn new(tcx: &TypeCtx<'_>, unit: &ModuleUnit<'_>, f: &'f IrFunction) -> Self {
        let mut defs = FxHashMap::default();
        let mut at = FxHashMap::default();
        for b in &f.blocks {
            for (i, inst) in b.insts.iter().enumerate() {
                if let Some(d) = inst.kind.dst() {
                    defs.insert(d, inst);
                    at.insert(d, (b.id, i));
                }
            }
        }
        Self {
            f,
            shape: FnShape::new(f),
            key: CheckContext::key(unit, f),
            types: tcx.infer(f),
            defs,
            at,
        }
    }

    fn def(&self, v: VarId) -> Option<&'f Inst> {
        self.defs.get(&v).copied()
    }

    /// The function's parameter named `name`, or a variable of that name an instruction
    /// defines.
    fn var_named(&self, name: &str) -> Option<VarId> {
        if let Some(p) = self.f.params.iter().find(|p| p.name == name) {
            return Some(p.var);
        }
        (0..self.f.var_names.len())
            .map(|i| VarId(i as u32))
            .find(|v| self.f.var_name(*v) == Some(name) && self.def(*v).is_some())
    }

    /// `v` before the copies that produced it.
    fn root(&self, mut v: VarId) -> VarId {
        let mut seen = FxHashSet::default();
        while seen.insert(v) {
            match self.def(v).map(|i| &i.kind) {
                Some(InstKind::Copy { src, .. }) => v = *src,
                _ => break,
            }
        }
        v
    }
}

/// One task a fan-out starts.
struct Sibling {
    /// The function holding the call that creates it (`Frames` index: the fan-out function, or
    /// a generator expression of it).
    frame: usize,
    /// The call creating the coroutine, the one-at-a-time call itself, or (`start_soon`) the
    /// callable and its arguments.
    start: Start,
    /// It is started more than once: in a loop of the fan-out function or by a comprehension.
    many: bool,
    /// The loop whose iterations start it (a block of its body), for the values each iteration
    /// makes anew.
    looped: Option<BlockId>,
    span: Span,
}

enum Start {
    /// A call instruction (`Call` / `MethodCall`).
    Call(VarId),
    /// `start_soon(func, *args)`: the callable and the arguments it is called with.
    Deferred { func: VarId, args: Vec<VarId> },
}

/// One object a sibling uses, as the fan-out function names it.
struct Used {
    holder: Holder,
    sibling: usize,
    kind: OneAtATime,
    method: String,
    chain: Vec<CallHop>,
}

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("EFF110", unit) {
        return;
    }
    debug_assert!(
        cx.index.opt_in_facts().exclusive,
        "`opt_in_facts` decides for EFF110"
    );
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    let functions = &unit.input.ir.functions;
    for f in functions.iter().filter(|f| !f.is_class_body) {
        // Fan-outs are named so; resolution decides.
        let candidate = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| match &i.kind {
                InstKind::Call { callee, .. } => {
                    matches!(callee.rsplit('.').next(), Some("gather" | "wait"))
                }
                InstKind::MethodCall { method, .. } => {
                    matches!(
                        method.as_str(),
                        "gather" | "wait" | "create_task" | "start_soon"
                    )
                }
                _ => false,
            });
        if !candidate {
            continue;
        }
        let mut check = Check {
            cx,
            unit,
            tcx: &tcx,
            frames: vec![Frame::new(&tcx, unit, f)],
            appends: OnceCell::new(),
            loop_defs: RefCell::default(),
            bound: RefCell::default(),
            fanned: FxHashMap::default(),
            out,
        };
        check.function();
    }
}

struct Check<'c, 'a> {
    cx: &'c CheckContext<'a>,
    unit: &'c ModuleUnit<'a>,
    tcx: &'c TypeCtx<'a>,
    /// The fan-out function first, then the generator expressions it starts tasks from.
    frames: Vec<Frame<'a>>,
    /// What the fan-out function appends to each of its lists (`list root -> (block, item)`).
    appends: OnceCell<FxHashMap<VarId, Vec<(BlockId, VarId)>>>,
    /// The variables each loop of the fan-out function defines, by header.
    loop_defs: RefCell<FxHashMap<BlockId, FxHashSet<VarId>>>,
    /// Where each object holding a connection only while used is bound
    /// ([`Self::bound_before`]).
    bound: RefCell<FxHashMap<Holder, Bound>>,
    /// The objects the tasks of each fan-out use, by its position.
    fanned: FxHashMap<Position, FxHashSet<Holder>>,
    out: &'c mut Vec<Diagnostic>,
}

/// A fan-out: where it is reported, where it starts its tasks, its API, and its tasks.
type Fanout = (Span, Position, &'static str, Vec<Sibling>);

/// Where an object is certainly bound: on entry to each block, and what each instruction does.
struct Bound {
    entry: Vec<Option<held::HeldSet>>,
    changes: FxHashMap<Position, Change>,
}

impl<'a> Check<'_, 'a> {
    fn top(&self) -> &Frame<'a> {
        &self.frames[0]
    }

    /// The catalog symbol an instruction of frame `k` calls.
    fn external(&self, k: usize, kind: &InstKind) -> Option<String> {
        match resolve_inst(self.tcx, &self.frames[k].types, kind) {
            Callee::External(s) => Some(s),
            _ => None,
        }
    }

    fn function(&mut self) {
        let f = self.top().f;
        let mut groups: BTreeMap<VarId, Fanout> = BTreeMap::new();
        let mut fanouts: Vec<Fanout> = Vec::new();
        // A fan-out a run never reaches starts nothing.
        let live = Live::of(f, &self.top().defs);
        for bb in f.blocks.iter().filter(|b| live.block(b.id)) {
            for (index, inst) in bb.insts.iter().enumerate() {
                let Some((args, _, _)) = call_parts(&inst.kind) else {
                    continue;
                };
                let Some(symbol) = self.external(0, &inst.kind) else {
                    continue;
                };
                match symbol.as_str() {
                    GATHER => {
                        let mut siblings = Vec::new();
                        let dst = inst.kind.dst().expect("calls define their result");
                        for (i, a) in args.iter().enumerate() {
                            if f.is_starred(dst, i) {
                                self.elements(*a, bb.id, &mut siblings);
                            } else {
                                self.element(0, *a, None, &mut siblings);
                            }
                        }
                        fanouts.push((inst.span, (bb.id, index), "asyncio.gather", siblings));
                    }
                    WAIT => {
                        let mut siblings = Vec::new();
                        if let Some(a) = args.first() {
                            self.elements(*a, bb.id, &mut siblings);
                        }
                        fanouts.push((inst.span, (bb.id, index), "asyncio.wait", siblings));
                    }
                    TASK_GROUP_CREATE | ANYIO_START_SOON => {
                        let looped = self.loop_of(bb.id);
                        let deferred = symbol == ANYIO_START_SOON;
                        let (InstKind::MethodCall { recv, .. }, Some(start)) =
                            (&inst.kind, self.group_start(deferred, args, looped))
                        else {
                            continue;
                        };
                        let group = self.top().root(*recv);
                        // The group starts its tasks where it is entered.
                        let top = self.top();
                        let entered = top.def(group).map_or(inst.span, |d| d.span);
                        let at = top.at.get(&group).copied().unwrap_or((bb.id, index));
                        let api = if symbol == ANYIO_START_SOON {
                            "anyio task group"
                        } else {
                            "asyncio.TaskGroup"
                        };
                        let slot = groups
                            .entry(group)
                            .or_insert((entered, at, api, Vec::new()));
                        slot.3.push(Sibling {
                            frame: 0,
                            start,
                            many: looped.is_some(),
                            looped,
                            span: inst.span,
                        });
                    }
                    _ => {}
                }
            }
        }
        fanouts.extend(groups.into_values());
        // What the tasks of each fan-out use: after it, the objects have been used.
        let fanouts: Vec<_> = fanouts
            .into_iter()
            .filter(|(_, _, _, s)| s.len() > 1 || s.first().is_some_and(|s| s.many))
            .map(|(span, at, api, siblings)| {
                let used: Vec<Used> = (siblings.iter().enumerate())
                    .flat_map(|(i, s)| self.uses_of(i, s))
                    .collect();
                for u in &used {
                    self.fanned.entry(at).or_default().insert(u.holder.clone());
                }
                (span, at, api, siblings, used)
            })
            .collect();
        for (span, at, api, siblings, used) in fanouts {
            self.fanout(span, at, api, &siblings, &used);
        }
    }

    /// A block of the innermost loop of the fan-out function around `bb`.
    fn loop_of(&self, bb: BlockId) -> Option<BlockId> {
        self.top().shape.innermost(bb).map(|lp| lp.header)
    }

    /// The tasks the iterable `v` holds for the fan-out in block `at`: the items of a display,
    /// the element of a generator expression or of a comprehension (started once per item), or
    /// of a variable bound to one, and what is appended to a list (once per iteration of a loop
    /// the fan-out is not in).
    fn elements(&mut self, v: VarId, at: BlockId, out: &mut Vec<Sibling>) {
        let top = self.top();
        let v = top.root(v);
        match top.def(v).map(|i| &i.kind) {
            Some(InstKind::AllocBuiltin { elems, .. }) if !elems.is_empty() => {
                for e in elems {
                    self.element(0, *e, None, out);
                }
            }
            // A list the function fills (also a comprehension built in it, Python 3.12+).
            Some(InstKind::AllocBuiltin { .. }) => {
                let appended = self.appends.get_or_init(|| {
                    let mut out: FxHashMap<VarId, Vec<(BlockId, VarId)>> = FxHashMap::default();
                    for b in &top.f.blocks {
                        for inst in &b.insts {
                            if let InstKind::MethodCall {
                                recv, method, args, ..
                            } = &inst.kind
                                && method == "append"
                                && let [item] = args.as_slice()
                            {
                                out.entry(top.root(*recv)).or_default().push((b.id, *item));
                            }
                        }
                    }
                    out
                });
                for &(b, item) in appended.get(&v).into_iter().flatten() {
                    let looped = (top.shape.innermost(b))
                        .filter(|lp| !top.shape.in_body(lp, at))
                        .map(|lp| lp.header);
                    self.element(0, item, looped, out);
                }
            }
            Some(InstKind::MakeGenExpr {
                func_id: Some(id), ..
            }) => self.child_elements(*id, out),
            // A comprehension of its own scope (before Python 3.12).
            Some(InstKind::Call {
                callee_var: None,
                callee,
                ..
            }) => {
                if let Some(id) = callee
                    .strip_prefix("__comp_")
                    .and_then(|n| n.parse::<u32>().ok())
                {
                    self.child_elements(FunctionId(id), out);
                }
            }
            _ => {}
        }
    }

    /// What a task group's `create_task(coro)` (or anyio's `start_soon(func, *args)`, `deferred`)
    /// starts, inside the loop `looped`.
    fn group_start(
        &self,
        deferred: bool,
        args: &[VarId],
        looped: Option<BlockId>,
    ) -> Option<Start> {
        let (first, rest) = args.split_first()?;
        if deferred {
            return Some(Start::Deferred {
                func: *first,
                args: rest.to_vec(),
            });
        }
        let mut one = Vec::new();
        self.element(0, *first, looped, &mut one);
        one.pop().map(|s| s.start)
    }

    /// The tasks a generator expression or a comprehension of the fan-out function yields or
    /// appends, each started once per item.
    fn child_elements(&mut self, id: FunctionId, out: &mut Vec<Sibling>) {
        let g = self.unit.input.ir.function(id);
        let mut frame = Frame::new(self.tcx, self.unit, g);
        // The variables it reads from the fan-out function have that function's values.
        let top = self.top();
        for inst in g.blocks.iter().flat_map(|b| &b.insts) {
            if let InstKind::LoadDeref { dst, name } = &inst.kind
                && let Some(v) = top.var_named(name)
                && let Some(val) = top.types.get(&v)
            {
                frame.types.entry(*dst).or_insert_with(|| val.clone());
            }
        }
        self.frames.push(frame);
        let k = self.frames.len() - 1;
        for b in &g.blocks {
            for inst in &b.insts {
                let item = match &inst.kind {
                    InstKind::Yield { value: Some(v), .. } => *v,
                    InstKind::MethodCall { method, args, .. }
                        if method == "append" && args.len() == 1 =>
                    {
                        args[0]
                    }
                    _ => continue,
                };
                let mut one = Vec::new();
                self.element(k, item, Some(b.id), &mut one);
                out.extend(one.into_iter().map(|s| Sibling { many: true, ..s }));
            }
        }
    }

    /// The task the value `v` of frame `k` is: a coroutine a call creates, a task a factory
    /// makes of one, or a one-at-a-time call.
    fn element(&self, k: usize, v: VarId, looped: Option<BlockId>, out: &mut Vec<Sibling>) {
        let frame = &self.frames[k];
        let v = frame.root(v);
        let Some(inst) = frame.def(v) else {
            return;
        };
        if !matches!(
            inst.kind,
            InstKind::Call { .. } | InstKind::MethodCall { .. }
        ) {
            return;
        }
        if let Some(symbol) = self.external(k, &inst.kind)
            && TASK_FACTORIES.contains(&symbol.as_str())
        {
            if let Some(coro) = call_parts(&inst.kind).and_then(|(args, _, _)| args.first()) {
                self.element(k, *coro, looped, out);
            }
            return;
        }
        out.push(Sibling {
            frame: k,
            start: Start::Call(v),
            many: looped.is_some(),
            looped,
            span: inst.span,
        });
    }

    /// Report the objects two tasks of one fan-out (or one started several times) use.
    fn fanout(&mut self, span: Span, at: Position, api: &str, siblings: &[Sibling], used: &[Used]) {
        let mut by_holder: BTreeMap<&Holder, Vec<&Used>> = BTreeMap::new();
        for u in used {
            by_holder.entry(&u.holder).or_default().push(u);
        }
        for (holder, uses) in by_holder {
            let distinct: FxHashSet<usize> = uses.iter().map(|u| u.sibling).collect();
            let repeated = uses.iter().any(|u| siblings[u.sibling].many);
            if distinct.len() < 2 && !repeated {
                continue;
            }
            let always = uses.iter().any(|u| u.kind == OneAtATime::Always);
            if !always && self.bound_before(holder, at) {
                continue;
            }
            let first = uses[0];
            let second = uses.iter().find(|u| u.sibling != first.sibling).copied();
            self.report(span, api, holder, first, second);
        }
    }

    /// The objects sibling `i` uses, as the fan-out function names them.
    fn uses_of(&self, i: usize, s: &Sibling) -> Vec<Used> {
        let frame = &self.frames[s.frame];
        let mut out = Vec::new();
        let (callees, bound) = match &s.start {
            Start::Call(v) => {
                let inst = frame.def(*v).expect("a sibling's call defines it");
                // A one-at-a-time call started as a task itself.
                let class_of = |x: VarId| one_at_a_time::instance_class(&frame.types, x);
                if let Some((obj, method, kind)) =
                    one_at_a_time::use_step(self.tcx, &inst.kind, &class_of)
                    && let Some(holder) = self.holder(s, obj)
                {
                    let hop = CallHop {
                        caller: frame.key.clone(),
                        callee: method.clone(),
                        span: inst.span,
                        path: self.unit.input.path.clone(),
                    };
                    out.push(Used {
                        holder,
                        sibling: i,
                        kind,
                        method,
                        chain: vec![hop],
                    });
                    return out;
                }
                let (callee, recv) = match &inst.kind {
                    InstKind::MethodCall { recv, .. } => (
                        resolve_inst(self.tcx, &frame.types, &inst.kind),
                        Some(*recv),
                    ),
                    _ => (resolve_inst(self.tcx, &frame.types, &inst.kind), None),
                };
                let Callee::Project(keys, binds_recv) = callee else {
                    return out;
                };
                let (args, kwargs, star_from) = call_parts(&inst.kind).unwrap_or_default();
                let recv = recv.filter(|_| binds_recv);
                (keys, Binding::new(recv, args, kwargs, star_from))
            }
            Start::Deferred { func, args } => {
                let (callee, recv) = match frame.def(frame.root(*func)).map(|i| &i.kind) {
                    Some(InstKind::AttrLoad { obj, attr, .. }) => (
                        self.tcx.resolve_method(frame.types.get(obj), attr),
                        Some(*obj),
                    ),
                    _ => (self.tcx.resolve_call(frame.types.get(func)), None),
                };
                let Callee::Project(keys, binds_recv) = callee else {
                    return out;
                };
                let recv = recv.filter(|_| binds_recv);
                (keys, Binding::new(recv, args, &[], None))
            }
        };
        let found = callees.iter().filter_map(|key| {
            Some((
                key,
                self.cx.index.summary(key)?,
                self.cx.decls.function_decl(key)?,
            ))
        });
        for (key, summary, decl) in found {
            // The task runs the coroutine's body.
            if !summary.is_async || summary.is_generator {
                continue;
            }
            for (obj, u) in summary.exclusive_uses() {
                // A task that may bind the name first may use another object under it.
                if locks::may_rebind(summary, obj) {
                    continue;
                }
                let Some(holder) = self.through(s, &bound, decl, obj) else {
                    continue;
                };
                out.push(self.used(i, s, key, holder, u));
            }
        }
        out
    }

    fn used(&self, i: usize, s: &Sibling, key: &FuncKey, holder: Holder, u: &ExclusiveUse) -> Used {
        let frame = &self.frames[s.frame];
        let hop = CallHop {
            caller: frame.key.clone(),
            callee: key.fqn(),
            span: s.span,
            path: self.unit.input.path.clone(),
        };
        let mut chain = vec![hop];
        chain.extend(owned_chain(&u.chain));
        Used {
            holder,
            sibling: i,
            kind: u.kind,
            method: u.method.clone(),
            chain,
        }
    }

    /// The object a callee's `obj` is to the fan-out function at a sibling's call.
    fn through(
        &self,
        s: &Sibling,
        bound: &Binding<'_>,
        decl: &heki_effect::FunctionDecl,
        obj: &ObjRef,
    ) -> Option<Holder> {
        match obj {
            ObjRef::Param { param, attr } => {
                let holder = self.holder(s, bound.arg(decl, *param)?)?;
                match attr {
                    Some(a) => holder.attr(a),
                    None => Some(holder),
                }
            }
            ObjRef::Global(_) | ObjRef::Cell { .. } => Some(Holder::Obj(obj.clone())),
        }
    }

    /// What value `v` of a sibling's frame is to the fan-out function, `None` for a value each
    /// start of the sibling makes anew (the item of its loop or comprehension).
    fn holder(&self, s: &Sibling, v: VarId) -> Option<Holder> {
        let frame = &self.frames[s.frame];
        let v = frame.root(v);
        if s.frame != 0 {
            // A generator expression's own values are per item; a variable it captures is the
            // fan-out function's.
            return match frame.def(v).map(|i| &i.kind) {
                Some(InstKind::LoadDeref { name, .. }) => self.named(name),
                Some(InstKind::LoadGlobal { name, .. }) => {
                    Some(Holder::Obj(ObjRef::Global(self.tcx.global_fqn(name))))
                }
                Some(InstKind::AttrLoad { obj, attr, .. }) => self.holder(s, *obj)?.attr(attr),
                _ => None,
            };
        }
        self.top_holder(v, s.looped)
    }

    /// What value `v` of the fan-out function is, `None` when the loop around the start
    /// (`looped`) makes it anew in each iteration.
    fn top_holder(&self, v: VarId, looped: Option<BlockId>) -> Option<Holder> {
        let top = self.top();
        let def = |x: VarId| top.def(x);
        if let Some(obj) = locks::obj_ref(self.tcx, top.f, &top.key, &top.types, &def, v) {
            return Some(Holder::Obj(obj));
        }
        let root = top.root(v);
        if let Some(head) = looped
            && self.made_in_loop(root, head)
        {
            return None;
        }
        match top.def(root).map(|i| &i.kind) {
            Some(InstKind::AttrLoad { obj, attr, .. }) => self.top_holder(*obj, looped)?.attr(attr),
            _ => Some(Holder::Local { root, attr: None }),
        }
    }

    /// Whether the loop with header `head` defines `v` (an instruction or a phi in its body).
    fn made_in_loop(&self, v: VarId, head: BlockId) -> bool {
        let f = self.top().f;
        let mut cache = self.loop_defs.borrow_mut();
        let defined = cache.entry(head).or_insert_with(|| {
            f.loops
                .iter()
                .filter(|lp| lp.header == head)
                .flat_map(|lp| &lp.body)
                .flat_map(|b| {
                    let block = f.block(*b);
                    let phis = block.phis.iter().map(|p| p.dst);
                    phis.chain(block.insts.iter().filter_map(|i| i.kind.dst()))
                })
                .collect()
        });
        defined.contains(&v)
    }

    /// The fan-out function's variable `name` (a closure of it reads it): its parameter, or
    /// its local variable of that name.
    fn named(&self, name: &str) -> Option<Holder> {
        let var = self.top().var_named(name)?;
        self.top_holder(var, None)
    }

    /// Whether every path to the fan-out at `at` uses the object after it was made or last gave
    /// its connection back: it holds a connection there. One must-dataflow per object and
    /// function, a bit for "used since".
    fn bound_before(&self, holder: &Holder, at: Position) -> bool {
        let mut cache = self.bound.borrow_mut();
        let bound = cache
            .entry(holder.clone())
            .or_insert_with(|| self.bound_flow(holder));
        let (b, i) = at;
        let mut state = (bound.entry[b.0 as usize].clone()).expect("a run reaches a fan-out");
        for j in 0..i {
            state.apply(bound.changes.get(&(b, j)));
        }
        !state.is_empty()
    }

    fn bound_flow(&self, holder: &Holder) -> Bound {
        let top = self.top();
        let f = top.f;
        let probe = Sibling {
            frame: 0,
            start: Start::Call(VarId(0)),
            many: false,
            looped: None,
            span: f.span,
        };
        let class_of = |x: VarId| one_at_a_time::instance_class(&top.types, x);
        let mut changes: FxHashMap<Position, Change> = FxHashMap::default();
        for bb in &f.blocks {
            for (i, inst) in bb.insts.iter().enumerate() {
                if let Some((obj, _, kind)) =
                    one_at_a_time::use_step(self.tcx, &inst.kind, &class_of)
                    && self.holder(&probe, obj).as_ref() == Some(holder)
                {
                    let change = match kind {
                        OneAtATime::Unbinds => Change::Give(vec![0]),
                        _ => Change::Take(0),
                    };
                    changes.insert((bb.id, i), change);
                } else if self.awaited_use(&probe, inst, holder)
                    || self
                        .fanned
                        .get(&(bb.id, i))
                        .is_some_and(|h| h.contains(holder))
                {
                    changes.insert((bb.id, i), Change::Take(0));
                }
            }
        }
        let entry = held::at_entry(&top.shape, 1, |b, i| changes.get(&(b, i)));
        Bound { entry, changes }
    }

    /// Whether `inst`, an awaited call of a project coroutine (or a plain call), uses `holder`.
    fn awaited_use(&self, probe: &Sibling, inst: &Inst, holder: &Holder) -> bool {
        let (InstKind::Call { is_awaited, .. } | InstKind::MethodCall { is_awaited, .. }) =
            inst.kind
        else {
            return false;
        };
        let top = self.top();
        let Callee::Project(keys, binds_recv) = resolve_inst(self.tcx, &top.types, &inst.kind)
        else {
            return false;
        };
        let recv = match inst.kind {
            InstKind::MethodCall { recv, .. } => binds_recv.then_some(recv),
            _ => None,
        };
        let (args, kwargs, star_from) = call_parts(&inst.kind).unwrap_or_default();
        let bound = Binding::new(recv, args, kwargs, star_from);
        let found = keys.iter().filter_map(|key| {
            Some((
                self.cx.index.summary(key)?,
                self.cx.decls.function_decl(key)?,
            ))
        });
        found.into_iter().any(|(summary, decl)| {
            // A coroutine runs where it is awaited; a plain function where it is called.
            (is_awaited || !summary.is_async)
                && !summary.is_generator
                && summary
                    .exclusive_uses()
                    .keys()
                    .any(|obj| self.through(probe, &bound, decl, obj).as_ref() == Some(holder))
        })
    }

    fn report(
        &mut self,
        span: Span,
        api: &str,
        holder: &Holder,
        first: &Used,
        second: Option<&Used>,
    ) {
        let top = self.top();
        let name = holder_name(top, holder);
        let class = first
            .method
            .rsplit_once('.')
            .map_or(first.method.as_str(), |(c, _)| c);
        let tasks = match second {
            Some(_) => "tasks together that both use",
            None => "a task for every item, each using",
        };
        let message = format!(
            "`{}` starts {tasks} `{name}` (`{api}`); `{class}` allows one operation at a time, so \
             the second task's call raises",
            top.key.qualname
        );
        let target = format!("{name}->{}", first.method);
        let help = "give each task its own object, or hold a lock around each use";
        let qualname = top.key.qualname.clone();
        let mut related: Vec<RelatedLocation> = Vec::new();
        for u in std::iter::once(first).chain(second) {
            let (last, calls) = u.chain.split_last().expect("a use's chain ends at the use");
            for hop in calls {
                related.push(self.cx.related_at(
                    &hop.path,
                    hop.span,
                    format!("`{}` calls `{}`", hop.caller.qualname, hop.callee),
                ));
            }
            related.push(self.cx.related_at(
                &last.path,
                last.span,
                format!("`{}` calls `{}` here", last.caller.qualname, last.callee),
            ));
        }
        let d = self
            .cx
            .emit(
                self.out,
                "EFF110",
                self.unit,
                span,
                &qualname,
                target,
                message,
                Some(help.to_owned()),
                &[],
            )
            .expect("`run` checked that EFF110 runs on the unit");
        d.related = related;
    }
}

/// How a call binds a callee's parameters: the receiver, positions before any `*` unpacking,
/// and keywords.
struct Binding<'v> {
    recv: Option<VarId>,
    args: &'v [VarId],
    kwargs: &'v [(Option<String>, VarId)],
    star_from: Option<u8>,
}

impl<'v> Binding<'v> {
    fn new(
        recv: Option<VarId>,
        args: &'v [VarId],
        kwargs: &'v [(Option<String>, VarId)],
        star_from: Option<u8>,
    ) -> Self {
        Self {
            recv,
            args,
            kwargs,
            star_from,
        }
    }

    /// The argument bound to the callee's parameter `param`.
    fn arg(&self, decl: &heki_effect::FunctionDecl, param: u8) -> Option<VarId> {
        let p = usize::from(param);
        let offset = usize::from(self.recv.is_some());
        if self.recv.is_some() && p == 0 {
            return self.recv;
        }
        let name = decl.param_names.get(p)?;
        if decl.keyword_param(name) == Some(p)
            && let Some((_, v)) = self
                .kwargs
                .iter()
                .find(|(k, _)| k.as_deref() == Some(name.as_str()))
        {
            return Some(*v);
        }
        let i = p.checked_sub(offset)?;
        // Past the positional parameters, or from a `*` unpacking on, nothing is known.
        let positional =
            decl.positional_param(p).is_some() && self.star_from.is_none_or(|s| i < usize::from(s));
        positional.then(|| self.args.get(i).copied()).flatten()
    }
}

/// A call's positional arguments, keyword arguments, and where `*` unpacking starts.
type CallParts<'k> = (&'k [VarId], &'k [(Option<String>, VarId)], Option<u8>);

fn call_parts(kind: &InstKind) -> Option<CallParts<'_>> {
    match kind {
        InstKind::Call {
            args,
            kwargs,
            star_from,
            ..
        }
        | InstKind::MethodCall {
            args,
            kwargs,
            star_from,
            ..
        } => Some((args, kwargs, *star_from)),
        _ => None,
    }
}

/// How the fan-out function names an object: a local variable by the first name a copy of its
/// value has.
fn holder_name(top: &Frame<'_>, holder: &Holder) -> String {
    match holder {
        Holder::Obj(obj) => obj.name_in(top.f),
        Holder::Local { root, attr } => {
            let named = top.f.var_name(*root).or_else(|| {
                top.f
                    .blocks
                    .iter()
                    .flat_map(|b| &b.insts)
                    .find_map(|i| match i.kind {
                        InstKind::Copy { dst, src } if src == *root => top.f.var_name(dst),
                        _ => None,
                    })
            });
            let base = named.unwrap_or("the value");
            match attr {
                Some(a) => format!("{base}.{a}"),
                None => base.to_owned(),
            }
        }
    }
}
