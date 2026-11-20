//! `EFF107` (`unretained-async-task`, opt-in): the result of `asyncio.create_task(...)` /
//! `loop.create_task(...)` / `asyncio.ensure_future(...)` bound to a local variable that is never
//! retained. The event loop keeps only a weak reference to the task, so it can be
//! garbage-collected before it finishes.
//!
//! A call whose result is discarded (an expression statement) is Ruff's `RUF006` and is never
//! reported. `loop.create_task` needs a receiver typed as an event loop (`get_running_loop()`,
//! ...); `asyncio.TaskGroup.create_task` keeps its tasks and an unknown receiver is skipped.
//! Only a coroutine makes a new task: an argument known to be something else is skipped.

use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::Diagnostic;
use heki_effect::consumption::resolve_inst;
use heki_effect::{Callee, TypeCtx, Val};
use heki_ir::{Inst, InstKind, IrFunction, Terminator, VarId};
use heki_kb::FUTURE_CLASSES;
use rustc_hash::{FxHashMap, FxHashSet};

/// Calls returning a task the event loop references weakly, with the parameter that takes the
/// coroutine.
const TASK_FACTORIES: &[(&str, &str)] = &[
    ("asyncio.create_task", "coro"),
    ("asyncio.ensure_future", "coro_or_future"),
    ("asyncio.AbstractEventLoop.create_task", "coro"),
    ("asyncio.BaseEventLoop.create_task", "coro"),
];

/// Receiver methods that cancel the task or require it to be done: the frame keeps the task
/// alive up to that point (`add_done_callback`, `set_name`, ... do not).
const LIFETIME_METHODS: &[&str] = &["cancel", "result", "exception"];

/// The public methods of `asyncio.Task` and `asyncio.Future` (CPython 3.11 to 3.15): reading one
/// from a task binds a method that holds the task. Other attributes (`task._state`,
/// `task.__class__`) hold nothing of it.
const TASK_METHODS: &[&str] = &[
    "add_done_callback",
    "cancel",
    "cancelled",
    "cancelling",
    "done",
    "exception",
    "get_context",
    "get_coro",
    "get_loop",
    "get_name",
    "get_stack",
    "print_stack",
    "remove_done_callback",
    "result",
    "set_exception",
    "set_name",
    "set_result",
    "uncancel",
];

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("EFF107", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    // Names the module imports a factory under (`from asyncio import create_task as spawn`).
    let imported_factories: FxHashSet<&str> = tcx
        .module
        .imports
        .iter()
        .filter(|(_, target)| TASK_FACTORIES.iter().any(|(s, _)| s == target))
        .map(|(name, _)| name.as_str())
        .collect();
    // Module and class bodies bind globals / class attributes, which keep the task.
    for f in unit
        .input
        .ir
        .functions
        .iter()
        .filter(|f| !f.is_module_body && !f.is_class_body)
    {
        // Candidates before type inference; resolution decides.
        let named_like_factory = |inst: &&Inst| match &inst.kind {
            InstKind::Call { callee, .. } => {
                matches!(
                    callee.rsplit('.').next(),
                    Some("create_task" | "ensure_future")
                ) || imported_factories.contains(callee.split('.').next().unwrap_or(callee))
            }
            InstKind::MethodCall { method, .. } => {
                matches!(method.as_str(), "create_task" | "ensure_future")
            }
            _ => false,
        };
        let candidates: Vec<&Inst> = f
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(named_like_factory)
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let types = tcx.infer(f);
        let flow = TaskFlow::of(f);
        let qualname = CheckContext::key(unit, f).qualname;
        for inst in candidates {
            let factory = match resolve_inst(&tcx, &types, &inst.kind) {
                Callee::External(s) => TASK_FACTORIES.iter().find(|(f, _)| *f == s),
                _ => None,
            };
            let Some(&(symbol, keyword)) = factory else {
                continue;
            };
            if task_argument(&inst.kind, keyword)
                .is_some_and(|arg| makes_no_task(&flow, &tcx, &types, arg))
            {
                continue;
            }
            let dst = inst.kind.dst().expect("calls define their result");
            let aliases = flow.aliases([dst]);
            let Some(name) = flow.name_of(&aliases) else {
                continue;
            };
            if flow.keeps(&aliases) {
                continue;
            }
            cx.emit(
                out,
                "EFF107",
                unit,
                inst.span,
                &qualname,
                format!("{symbol}->{name}"),
                format!(
                    "the task from `{symbol}` is bound to `{name}` but never awaited, returned, or stored, so it can be garbage-collected before it finishes"
                ),
                Some("keep it in a set and discard it in a done callback, or await it".to_owned()),
                &[],
            );
        }
    }
}

/// The argument a factory call makes its task from: the first positional one, or the one passed
/// as `keyword` (`None` when `*` or `**` unpacking may supply it instead).
fn task_argument(kind: &InstKind, keyword: &str) -> Option<VarId> {
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
        } if *star_from != Some(0) => args.first().copied().or_else(|| {
            kwargs
                .iter()
                .find(|(name, _)| name.as_deref() == Some(keyword))
                .map(|(_, v)| *v)
        }),
        _ => None,
    }
}

/// Whether `arg` is known not to be a coroutine, so the factory makes no new task from it: a
/// value typed as a future or a task ([`FUTURE_CLASSES`], from an annotation or the catalog's
/// `return_type`), or the result of calling project functions that all create no coroutine
/// (`FunctionDecl::makes_no_coroutine`; `create_task` raises `TypeError` on what they return,
/// and `ensure_future` returns a future it is handed). An argument of unknown kind may be a
/// coroutine and is reported.
fn makes_no_task(
    flow: &TaskFlow<'_>,
    tcx: &TypeCtx<'_>,
    types: &FxHashMap<VarId, Val>,
    arg: VarId,
) -> bool {
    if matches!(types.get(&arg), Some(Val::Inst(class)) if FUTURE_CLASSES.contains(&class.as_str()))
    {
        return true;
    }
    let Some(call) = flow.calls.get(&arg) else {
        return false;
    };
    match resolve_inst(tcx, types, call) {
        Callee::Project(keys, _) => {
            !keys.is_empty()
                && keys.iter().all(|key| {
                    // Calling a class returns the instance its `__init__` sets up, of unknown
                    // kind.
                    key.qualname.rsplit('.').next() != Some("__init__")
                        && tcx
                            .project
                            .function_decl(key)
                            .is_some_and(|decl| decl.makes_no_coroutine())
                })
        }
        _ => false,
    }
}

/// What the rule reads of one function, indexed once for all its factory calls, so that each
/// call costs only the values it reaches.
#[derive(Default)]
struct TaskFlow<'f> {
    /// The values a `Copy` binds from each value.
    copies: FxHashMap<VarId, Vec<VarId>>,
    /// The phis each value flows into.
    phis: FxHashMap<VarId, Vec<VarId>>,
    /// The [`TASK_METHODS`] bound to each value (`task.cancel`).
    methods: FxHashMap<VarId, Vec<VarId>>,
    /// The local name each `Copy` binds, with its source position.
    names: FxHashMap<VarId, (u32, &'f str)>,
    /// The call each value is the result of, through copies (`pending = e.wait()`).
    calls: FxHashMap<VarId, &'f InstKind>,
    /// Values awaited, returned or yielded, stored (attribute value, subscript key or value,
    /// global, enclosing scope, container literal), passed to a call, or captured by a closure
    /// or generator expression.
    kept: FxHashSet<VarId>,
    /// Receivers of a [`LIFETIME_METHODS`] call.
    ended: FxHashSet<VarId>,
}

impl<'f> TaskFlow<'f> {
    fn of(f: &'f IrFunction) -> Self {
        let mut flow = Self::default();
        let mut calls = Vec::new();
        for bb in &f.blocks {
            for phi in &bb.phis {
                for (_, v) in &phi.incoming {
                    flow.phis.entry(*v).or_default().push(phi.dst);
                }
            }
            for inst in &bb.insts {
                match &inst.kind {
                    InstKind::Copy { dst, src } => {
                        flow.copies.entry(*src).or_default().push(*dst);
                        if let Some(name) = f.var_name(*dst).filter(|n| !n.starts_with('.')) {
                            flow.names.insert(*dst, (inst.span.start, name));
                        }
                    }
                    InstKind::AttrLoad { dst, obj, attr }
                        if TASK_METHODS.contains(&attr.as_str()) =>
                    {
                        flow.methods.entry(*obj).or_default().push(*dst);
                    }
                    kind @ (InstKind::Call { dst, .. } | InstKind::MethodCall { dst, .. }) => {
                        calls.push((*dst, kind));
                    }
                    _ => {}
                }
                if let InstKind::MethodCall { recv, method, .. } = &inst.kind
                    && LIFETIME_METHODS.contains(&method.as_str())
                {
                    flow.ended.insert(*recv);
                }
                flow.kept.extend(kept_operands(&inst.kind));
            }
            if let Terminator::Return(Some(v)) = &bb.terminator {
                flow.kept.insert(*v);
            }
        }
        // Each value has one `Copy` source, so every value is reached from at most one call.
        for (dst, kind) in calls {
            let mut pending = vec![dst];
            while let Some(v) = pending.pop() {
                if flow.calls.insert(v, kind).is_none() {
                    pending.extend(flow.copies.get(&v).into_iter().flatten());
                }
            }
        }
        flow
    }

    /// `roots` and every value copying one of them, through `Copy` and phis.
    fn aliases(&self, roots: impl IntoIterator<Item = VarId>) -> FxHashSet<VarId> {
        let mut set = FxHashSet::default();
        let mut pending: Vec<VarId> = roots.into_iter().collect();
        while let Some(v) = pending.pop() {
            if set.insert(v) {
                for edges in [&self.copies, &self.phis] {
                    pending.extend(edges.get(&v).into_iter().flatten());
                }
            }
        }
        set
    }

    /// The first local name the task is bound to (`None`: never bound to a name, e.g. a bare
    /// expression statement or a value passed on directly).
    fn name_of(&self, aliases: &FxHashSet<VarId>) -> Option<String> {
        aliases
            .iter()
            .filter_map(|v| self.names.get(v))
            .min_by_key(|(start, _)| *start)
            .map(|(_, name)| (*name).to_owned())
    }

    /// Whether an alias of the task, or a method bound to one (`task.cancel`, which holds the
    /// task as its `__self__`), is kept, or whether an alias is ended by a [`LIFETIME_METHODS`]
    /// call on it. What is read from a bound method (`task.cancel.__name__`) holds nothing.
    fn keeps(&self, aliases: &FxHashSet<VarId>) -> bool {
        let methods = self.aliases(
            aliases
                .iter()
                .flat_map(|a| self.methods.get(a).into_iter().flatten().copied()),
        );
        aliases
            .iter()
            .any(|v| self.kept.contains(v) || self.ended.contains(v))
            || methods.iter().any(|v| self.kept.contains(v))
    }
}

/// The operands an instruction keeps: see [`TaskFlow::kept`]. Only being the receiver of a
/// method call keeps nothing alive (`task.add_done_callback(...)`).
fn kept_operands(kind: &InstKind) -> Vec<VarId> {
    match kind {
        InstKind::Await { value, .. }
        | InstKind::YieldFrom { value, .. }
        | InstKind::AttrStore { value, .. }
        | InstKind::StoreGlobal { value, .. }
        | InstKind::StoreDeref { value, .. } => vec![*value],
        InstKind::SubscriptStore { index, value, .. } => vec![*index, *value],
        InstKind::MethodCall { args, kwargs, .. } => args
            .iter()
            .chain(kwargs.iter().map(|(_, v)| v))
            .copied()
            .collect(),
        InstKind::Yield { .. }
        | InstKind::AllocBuiltin { .. }
        | InstKind::DictLiteral { .. }
        | InstKind::MakeClosure { .. }
        | InstKind::MakeGenExpr { .. }
        | InstKind::Call { .. } => kind.operands(),
        _ => Vec::new(),
    }
}
