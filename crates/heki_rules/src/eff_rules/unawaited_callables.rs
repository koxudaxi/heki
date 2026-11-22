//! `EFF115` (`async-def-as-sync-callback`, opt-in): an `async def` handed to a library call
//! that calls it as a plain function and never awaits what it returns (a thread's target, an
//! executor's function, an `atexit` or signal handler, a `filter` predicate; the catalog's
//! `calls_unawaited` and offloading APIs). Calling it only creates a coroutine, so its body does
//! not run there.
//!
//! The summary records each project function handed to such a call
//! (`FileSummary::unawaited_handoffs`, through a `functools.partial` object or a bound method);
//! the rule reads the declarations of the functions it calls, so a change of one between `def`
//! and `async def` re-checks the file. A project function that takes a callable is never such a
//! call, whatever it does with it (`inspect.iscoroutinefunction` tests included).

use crate::{CheckContext, ModuleUnit};
use heki_diagnostics::Diagnostic;
use heki_effect::{FuncKey, FunctionDecl, UnawaitedHandoff};

pub(super) fn run(cx: &CheckContext<'_>, unit: &ModuleUnit<'_>, out: &mut Vec<Diagnostic>) {
    if !cx.enabled("EFF115", unit) {
        return;
    }
    debug_assert!(
        cx.index.opt_in_facts().unawaited_handoffs,
        "`opt_in_facts` decides for EFF115"
    );
    let Some(by_function) = cx
        .index
        .files
        .get(&unit.input.path)
        .and_then(|file| file.unawaited_handoffs.as_deref())
    else {
        return;
    };
    for (func, handoffs) in by_function {
        for handoff in handoffs {
            report(cx, unit, func, handoff, out);
        }
    }
}

fn report(
    cx: &CheckContext<'_>,
    unit: &ModuleUnit<'_>,
    func: &FuncKey,
    handoff: &UnawaitedHandoff,
    out: &mut Vec<Diagnostic>,
) {
    let decls: Vec<(&FuncKey, &FunctionDecl)> = handoff
        .callees
        .iter()
        .filter_map(|key| Some((key, cx.decls.function_decl(key)?)))
        .collect();
    let Some(&(async_def, decl)) = decls.iter().find(|(_, d)| is_async_def(d)) else {
        return;
    };
    let api = handoff
        .api
        .strip_prefix("builtins.")
        .unwrap_or(&handoff.api);
    let name = &async_def.qualname;
    // Worded as certain when every function the value calls makes a coroutine as defined.
    let what = if !decl.called_as_defined {
        format!("`{name}` is an `async def` under a decorator, which may make it a plain function")
    } else if decls.len() < handoff.callees.len()
        || !decls
            .iter()
            .all(|(_, d)| is_async_def(d) && d.called_as_defined)
    {
        format!("`{name}`, one implementation the call may run, is an `async def`")
    } else {
        format!("`{name}` is an `async def`")
    };
    let (message, help) = if handoff.tests_result {
        (
            format!(
                "`{api}` tests what `{name}` returns for truth, and {what}: the result is a coroutine, which is always true, and the body does not run"
            ),
            "pass a plain function, or await the results before testing them",
        )
    } else {
        (
            format!(
                "`{api}` never awaits what `{name}` returns, and {what}: calling it only creates a coroutine, so the body does not run"
            ),
            "pass a plain function that runs the coroutine (`asyncio.run`, `asyncio.run_coroutine_threadsafe`), or schedule it on the event loop as a task",
        )
    };
    cx.emit(
        out,
        "EFF115",
        unit,
        handoff.span,
        &func.qualname,
        format!("{api}->{name}"),
        message,
        Some(help.to_owned()),
        &[],
    );
}

/// An `async def` whose call returns a coroutine as defined: not an async generator.
fn is_async_def(decl: &FunctionDecl) -> bool {
    decl.is_async && !decl.is_generator
}
