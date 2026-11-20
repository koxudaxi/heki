//! `EFF107` (`unretained-async-task`, opt-in): the result of `asyncio.create_task(...)` /
//! `loop.create_task(...)` / `asyncio.ensure_future(...)` bound to a local variable that is never
//! retained. The event loop keeps only a weak reference to the task, so it can be
//! garbage-collected before it finishes.
//!
//! A call whose result is discarded (an expression statement) is Ruff's `RUF006` and is never
//! reported. `loop.create_task` needs a receiver typed as an event loop (`get_running_loop()`,
//! ...); `asyncio.TaskGroup.create_task` keeps its tasks and an unknown receiver is skipped.

use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::Diagnostic;
use heki_effect::consumption::resolve_inst;
use heki_effect::{Callee, TypeCtx};
use heki_ir::{Inst, InstKind, IrFunction, Terminator, VarId};
use rustc_hash::FxHashSet;

/// Calls returning a task the event loop references weakly.
const TASK_FACTORIES: &[&str] = &[
    "asyncio.create_task",
    "asyncio.ensure_future",
    "asyncio.AbstractEventLoop.create_task",
    "asyncio.BaseEventLoop.create_task",
];

/// Receiver methods that cancel the task or require it to be done: the frame keeps the task
/// alive up to that point (`add_done_callback`, `set_name`, ... do not).
const LIFETIME_METHODS: &[&str] = &["cancel", "result", "exception"];

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("EFF107", unit) {
        return;
    }
    let tcx = TypeCtx::new(&unit.input, cx.decls, cx.kb);
    // Module and class bodies bind globals / class attributes, which keep the task.
    for f in unit
        .input
        .ir
        .functions
        .iter()
        .filter(|f| !f.is_module_body && !f.is_class_body)
    {
        let named_like_factory = |inst: &&Inst| match &inst.kind {
            InstKind::Call { callee, .. } => {
                matches!(
                    callee.rsplit('.').next(),
                    Some("create_task" | "ensure_future")
                )
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
        let qualname = CheckContext::key(unit, f).qualname;
        for inst in candidates {
            let symbol = match resolve_inst(&tcx, &types, &inst.kind) {
                Callee::External(s) if TASK_FACTORIES.contains(&s.as_str()) => s,
                _ => continue,
            };
            let dst = inst.kind.dst().expect("calls define their result");
            let aliases = aliases_of(f, dst);
            let Some(name) = name_of(f, &aliases) else {
                continue;
            };
            if retained(f, &aliases) {
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
                    "the task from `{symbol}` is bound to `{name}` but never awaited, returned, or stored; the event loop keeps only a weak reference, so it can be garbage-collected before it finishes (keep it in a set and discard it in a done callback, or await it)"
                ),
                &[],
            );
        }
    }
}

/// `var` and every `Copy` / phi alias of it.
fn aliases_of(f: &IrFunction, var: VarId) -> FxHashSet<VarId> {
    let mut set = FxHashSet::default();
    set.insert(var);
    loop {
        let before = set.len();
        for bb in &f.blocks {
            for phi in &bb.phis {
                if phi.incoming.iter().any(|(_, v)| set.contains(v)) {
                    set.insert(phi.dst);
                }
            }
            for inst in &bb.insts {
                if let InstKind::Copy { dst, src } = &inst.kind
                    && set.contains(src)
                {
                    set.insert(*dst);
                }
            }
        }
        if set.len() == before {
            return set;
        }
    }
}

/// The first local name the task is bound to (`None`: never bound to a name, e.g. a bare
/// expression statement or a value passed on directly).
fn name_of(f: &IrFunction, aliases: &FxHashSet<VarId>) -> Option<String> {
    f.blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|i| match i.kind {
            InstKind::Copy { dst, .. } if aliases.contains(&dst) => f
                .var_name(dst)
                .filter(|n| !n.starts_with('.'))
                .map(|n| (i.span.start, n)),
            _ => None,
        })
        .min_by_key(|(start, _)| *start)
        .map(|(_, n)| n.to_owned())
}

/// Whether any alias is awaited, returned or yielded, stored (attribute value, subscript key or
/// value, global, enclosing scope, container literal), passed to a call, captured by a closure
/// or generator expression, or ended by a [`LIFETIME_METHODS`] call on it.
fn retained(f: &IrFunction, aliases: &FxHashSet<VarId>) -> bool {
    let has = |v: &VarId| aliases.contains(v);
    let retains = |kind: &InstKind| match kind {
        InstKind::Await { value, .. }
        | InstKind::YieldFrom { value, .. }
        | InstKind::AttrStore { value, .. }
        | InstKind::StoreGlobal { value, .. }
        | InstKind::StoreDeref { value, .. } => has(value),
        InstKind::SubscriptStore { index, value, .. } => has(index) || has(value),
        // Only being the receiver keeps nothing alive (`task.add_done_callback(...)`).
        InstKind::MethodCall {
            recv,
            method,
            args,
            kwargs,
            ..
        } => {
            args.iter().chain(kwargs.iter().map(|(_, v)| v)).any(has)
                || has(recv) && LIFETIME_METHODS.contains(&method.as_str())
        }
        InstKind::Yield { .. }
        | InstKind::AllocBuiltin { .. }
        | InstKind::DictLiteral { .. }
        | InstKind::MakeClosure { .. }
        | InstKind::MakeGenExpr { .. }
        | InstKind::Call { .. } => kind.operands().iter().any(has),
        _ => false,
    };
    f.blocks.iter().any(|bb| {
        matches!(&bb.terminator, Terminator::Return(Some(v)) if has(v))
            || bb.insts.iter().any(|inst| retains(&inst.kind))
    })
}
