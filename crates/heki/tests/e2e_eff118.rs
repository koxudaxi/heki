//! `EFF118` (`unshielded-cleanup-checkpoint`) end to end: a cleanup that awaits a call reaching an
//! AnyIO or Trio cancellation checkpoint with no shield on the way. The shapes reported (a
//! worker-thread call one and several calls down from a `finally`, a shutdown loop in an
//! `__aexit__`, a pool `finally`, an `except BaseException`, Trio), the shapes that are not (a
//! shield on the path, a shielding project wrapper, every call shielding, also through an
//! override, a lock made with `fast_acquire=True`, a branch a constant argument never takes, a
//! plain await, an `except` that only re-raises the cancellation, the re-delivery idiom,
//! `except Exception`, a clause after one taking the cancellation, a scope that may shield), and
//! the selection.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, qualname, target)` of every `EFF118` diagnostic, sorted.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    let mut out: Vec<_> = diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF118")
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn d(path: &str, line: u64, qualname: &str, target: &str) -> (String, u64, String, String) {
    (path.into(), line, qualname.into(), target.into())
}

const SELECT: &[&str] = &["--select", "EFF118"];

/// A worker whose `finally` deletes a billed resource through a helper running a worker-thread
/// call: in a cancelled scope the call raises before it starts.
const WORKER: &str = r#"import anyio


def _delete(name: str) -> None:
    print("deleting", name)


async def delete_group(name: str) -> None:
    await anyio.to_thread.run_sync(_delete, name)


async def wait_for_deletion(name: str) -> None:
    await delete_group(name)


async def run(name: str, keep: bool = False) -> None:
    try:
        await anyio.sleep(10)
    finally:
        if not keep:
            await wait_for_deletion(name)
        print("cleaned up")
"#;

/// A process manager whose `__aexit__` stops every process inside `try: ... except Exception:`:
/// the cancellation is no `Exception`, so the first one ends the loop.
const PROCESSES: &str = r#"import signal

import anyio


class ProcessManager:
    def __init__(self) -> None:
        self.pids: list[int] = []

    async def kill(self, pid: int) -> None:
        print("signal", pid, signal.SIGTERM)
        await anyio.sleep(5)
        print("signal", pid, signal.SIGKILL)

    async def __aenter__(self) -> "ProcessManager":
        return self

    async def __aexit__(self, *exc: object) -> None:
        for pid in self.pids:
            try:
                await self.kill(pid)
            except Exception:
                pass
"#;

/// A pool `finally` that pops a stale worker into a call argument and then awaits: the popped
/// worker is neither destroyed nor kept. A handler taking `BaseException` sends a result.
const POOL: &str = r#"from collections import deque

import anyio


class Worker:
    def destroy(self) -> None:
        pass


idle: deque[Worker] = deque()


async def run_sync(worker: Worker) -> None:
    try:
        await anyio.sleep(1)
    finally:
        if idle:
            await anyio.to_thread.run_sync(idle.popleft().destroy)
        idle.append(worker)


async def forward(send: anyio.streams.memory.MemoryObjectSendStream[int]) -> None:
    try:
        await anyio.sleep(1)
    except BaseException:
        await send.send(0)
        raise
"#;

/// Trio: a `finally` sleeping, and a handler of `trio.Cancelled`.
const TRIO: &str = r#"import trio


async def close(stream: trio.MemorySendChannel[bytes]) -> None:
    try:
        await trio.sleep(1)
    finally:
        await trio.sleep(0.1)
        print("closed")


async def stop() -> None:
    try:
        await trio.sleep(1)
    except trio.Cancelled:
        await trio.sleep(0.1)
        raise
"#;

#[test]
fn reports_a_cleanup_reaching_a_checkpoint_with_the_calls_down() {
    let p = Project::new("eff118_harm");
    p.write("pkg/__init__.py", "")
        .write("pkg/worker.py", WORKER)
        .write("pkg/processes.py", PROCESSES)
        .write("pkg/pool.py", POOL)
        .write("pkg/trio_close.py", TRIO);
    let diags = diagnostics(&p, SELECT);
    assert_eq!(
        found(&diags),
        vec![
            d(
                "pkg/pool.py",
                19,
                "run_sync",
                "anyio.to_thread.run_sync->anyio.to_thread.run_sync"
            ),
            d(
                "pkg/pool.py",
                27,
                "forward",
                "anyio.streams.memory.MemoryObjectSendStream.send->anyio.streams.memory.MemoryObjectSendStream.send"
            ),
            d(
                "pkg/processes.py",
                21,
                "ProcessManager.__aexit__",
                "pkg.processes.ProcessManager.kill->anyio.sleep"
            ),
            d("pkg/trio_close.py", 8, "close", "trio.sleep->trio.sleep"),
            d("pkg/trio_close.py", 16, "stop", "trio.sleep->trio.sleep"),
            d(
                "pkg/worker.py",
                21,
                "run",
                "pkg.worker.wait_for_deletion->anyio.to_thread.run_sync"
            ),
        ]
    );
    let run = diags
        .iter()
        .find(|d| d["qualname"] == "run")
        .expect("the worker's `finally` is reported");
    assert_eq!(run["certainty"], "heuristic");
    assert_eq!(
        run["message"],
        "`run` awaits `pkg.worker.wait_for_deletion` in a `finally` body, which reaches the \
         cancellation checkpoint `anyio.to_thread.run_sync` 3 calls down with no shield on the \
         way: in a cancelled scope the await raises the cancellation and the rest of the cleanup \
         does not run"
    );
    assert_eq!(
        run["help"],
        "run the cleanup inside `with anyio.CancelScope(shield=True):`"
    );
    // The hops down to the checkpoint.
    let related: Vec<&str> = run["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        related,
        [
            "`run` calls `pkg.worker.wait_for_deletion`",
            "`wait_for_deletion` calls `pkg.worker.delete_group`",
            "`delete_group` calls `anyio.to_thread.run_sync`",
        ]
    );
    let stop = diags.iter().find(|d| d["qualname"] == "stop").unwrap();
    assert_eq!(
        stop["help"],
        "run the cleanup inside `with trio.CancelScope(shield=True):`"
    );
    assert!(
        stop["message"]
            .as_str()
            .unwrap()
            .contains("in an `except` clause taking the cancellation"),
        "{stop}"
    );
}

/// Cleanups the rule leaves alone.
const QUIET: &str = r#"import anyio


async def helper() -> int:
    return 1


async def stop_session(force: bool = False) -> None:
    lock = anyio.Lock()
    if force:
        async with lock:
            pass


async def _shielded(coro) -> None:
    with anyio.CancelScope(shield=True):
        await coro


async def stop_quietly() -> None:
    with anyio.CancelScope(shield=True):
        await anyio.sleep(1)


class Session:
    def __init__(self) -> None:
        self._lock = anyio.Lock(fast_acquire=True)

    async def _close(self) -> None:
        try:
            pass
        finally:
            await anyio.sleep(0.1)
            print("closed")

    async def shutdown(self) -> None:
        with anyio.CancelScope(shield=True):
            await self._close()

    async def shutdown_later(self) -> None:
        await _shielded(self._close())

    async def disconnect(self) -> None:
        try:
            pass
        finally:
            await stop_session()
            await stop_quietly()
            await helper()
            async with self._lock:
                pass
            with anyio.move_on_after(5, shield=True):
                await anyio.sleep(1)


async def reraise() -> None:
    try:
        await anyio.sleep(1)
    except anyio.get_cancelled_exc_class():
        raise
    except BaseException:
        await anyio.sleep(1)


async def only_exceptions() -> None:
    try:
        await anyio.sleep(1)
    except Exception:
        await anyio.sleep(1)


async def redeliver() -> None:
    try:
        pass
    finally:
        with anyio.CancelScope(shield=True):
            await anyio.sleep(1)
        await anyio.lowlevel.checkpoint()


class Host:
    async def _close(self) -> None:
        pass

    async def stop(self) -> None:
        with anyio.CancelScope(shield=True):
            await self._close()


class WindowsHost(Host):
    async def _close(self) -> None:
        try:
            pass
        finally:
            await anyio.to_thread.run_sync(print, "removing")
            print("removed")


async def maybe_shielded(shield: bool) -> None:
    try:
        pass
    finally:
        with anyio.CancelScope(shield=shield):
            await anyio.sleep(1)
"#;

#[test]
fn leaves_shielded_and_unreached_checkpoints() {
    let p = Project::new("eff118_quiet");
    p.write("pkg/__init__.py", "").write("pkg/quiet.py", QUIET);
    assert_eq!(found(&diagnostics(&p, SELECT)), vec![]);
}

/// The shields by the module that defines them, as code importing them there (or AnyIO's own
/// modules) spells them: the same shield as the public name, and a scope without `shield` is
/// none.
const DEFINING_PATHS: &str = r#"import anyio
from anyio._core import _tasks
from anyio._core._tasks import CancelScope, move_on_after


async def shielded_class() -> None:
    try:
        await anyio.sleep(1)
    finally:
        with CancelScope(shield=True):
            await anyio.sleep(1)


async def shielded_deadline() -> None:
    try:
        await anyio.sleep(1)
    finally:
        with move_on_after(5, shield=True):
            await anyio.sleep(1)


async def shielded_through_the_module() -> None:
    try:
        await anyio.sleep(1)
    finally:
        with _tasks.CancelScope(shield=True):
            await anyio.sleep(1)


async def not_shielded() -> None:
    try:
        await anyio.sleep(1)
    finally:
        with CancelScope():
            await anyio.sleep(1)
"#;

#[test]
fn a_shield_named_by_its_defining_module_shields() {
    let p = Project::new("eff118_defining_paths");
    p.write("pkg/__init__.py", "")
        .write("pkg/paths.py", DEFINING_PATHS);
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![d(
            "pkg/paths.py",
            35,
            "not_shielded",
            "anyio.sleep->anyio.sleep"
        )]
    );
}

#[test]
fn a_call_awaiting_the_function_unshielded_keeps_the_report() {
    let p = Project::new("eff118_callers");
    p.write("pkg/__init__.py", "")
        .write("pkg/quiet.py", QUIET)
        .write(
            "pkg/caller.py",
            "from pkg.quiet import Session\n\n\nasync def close(s: Session) -> None:\n    \
             await s._close()\n",
        );
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![d(
            "pkg/quiet.py",
            33,
            "Session._close",
            "anyio.sleep->anyio.sleep"
        )]
    );
}

/// The base class's call of the method runs the subclass's override: when it stops shielding
/// it, the override's cleanup is reported.
#[test]
fn a_call_through_an_override_decides_the_report() {
    let p = Project::new("eff118_override");
    let unshielded = QUIET.replace(
        "    async def stop(self) -> None:\n        with anyio.CancelScope(shield=True):\n            \
         await self._close()\n",
        "    async def stop(self) -> None:\n        await self._close()\n",
    );
    p.write("pkg/__init__.py", "")
        .write("pkg/quiet.py", &unshielded);
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![d(
            "pkg/quiet.py",
            94,
            "WindowsHost._close",
            "anyio.to_thread.run_sync->anyio.to_thread.run_sync"
        )]
    );
}

/// The cancellation's classes named by a call, by an imported call, in a tuple, or by a bare
/// `except`; a timeout
/// scope that does not shield; a re-delivery that something runs after.
const HANDLERS: &str = r#"import anyio
from anyio import get_cancelled_exc_class


async def by_method() -> None:
    try:
        await anyio.sleep(1)
    except anyio.get_cancelled_exc_class():
        await anyio.sleep(0.1)
        raise


async def by_name() -> None:
    try:
        await anyio.sleep(1)
    except get_cancelled_exc_class():
        await anyio.sleep(0.1)
        raise


async def in_a_tuple() -> None:
    try:
        await anyio.sleep(1)
    except (KeyboardInterrupt, BaseException):
        await anyio.sleep(0.1)
        raise


async def bare() -> None:
    try:
        await anyio.sleep(1)
    except:
        await anyio.sleep(0.1)
        raise


async def under_a_timeout() -> None:
    try:
        pass
    finally:
        with anyio.move_on_after(1):
            await anyio.sleep(0.1)
        print("done")


async def redelivered_too_early() -> None:
    try:
        pass
    finally:
        await anyio.lowlevel.checkpoint()
    print("after")
"#;

/// Scopes that shield (a constructor imported by name) or may shield (a `shield` set on the
/// scope, options unpacked, a scope made elsewhere), and clauses whose classes heki cannot tell
/// (a parameter, an attribute, `except*`).
const SCOPES: &str = r#"import anyio
from anyio import CancelScope


class Holder:
    def __init__(self) -> None:
        self._scope = anyio.CancelScope()
        self.errors: type[BaseException] = BaseException

    async def close(self, options: dict[str, bool], errors: type[BaseException]) -> None:
        try:
            pass
        finally:
            with CancelScope(shield=True):
                await anyio.sleep(0.1)
            with anyio.CancelScope() as scope:
                scope.shield = True
                await anyio.sleep(0.1)
            with anyio.CancelScope(**options):
                await anyio.sleep(0.1)
            with self._scope:
                await anyio.sleep(0.1)
        try:
            await anyio.sleep(1)
        except errors:
            await anyio.sleep(0.1)
        try:
            await anyio.sleep(1)
        except self.errors:
            await anyio.sleep(0.1)


async def star() -> None:
    try:
        await anyio.sleep(1)
    except* BaseException:
        await anyio.sleep(0.1)
"#;

#[test]
fn reads_the_cancellation_classes_and_the_scopes_that_may_shield() {
    let p = Project::new("eff118_classes");
    p.write("pkg/__init__.py", "")
        .write("pkg/handlers.py", HANDLERS)
        .write("pkg/scopes.py", SCOPES);
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![
            d(
                "pkg/handlers.py",
                9,
                "by_method",
                "anyio.sleep->anyio.sleep"
            ),
            d("pkg/handlers.py", 17, "by_name", "anyio.sleep->anyio.sleep"),
            d(
                "pkg/handlers.py",
                25,
                "in_a_tuple",
                "anyio.sleep->anyio.sleep"
            ),
            d("pkg/handlers.py", 33, "bare", "anyio.sleep->anyio.sleep"),
            d(
                "pkg/handlers.py",
                42,
                "under_a_timeout",
                "anyio.sleep->anyio.sleep"
            ),
            d(
                "pkg/handlers.py",
                50,
                "redelivered_too_early",
                "anyio.lowlevel.checkpoint->anyio.lowlevel.checkpoint"
            ),
        ]
    );
}

#[test]
fn runs_only_when_selected_or_heuristic() {
    let p = Project::new("eff118_selection");
    p.write("pkg/__init__.py", "")
        .write("pkg/worker.py", WORKER);
    assert_eq!(found(&diagnostics(&p, &["--no-heuristic"])), vec![]);
    assert_eq!(found(&diagnostics(&p, &[])).len(), 1);
    let (code, stdout, _) = p.heki(&["rule", "EFF118"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("## What a report states"), "{stdout}");
    assert!(stdout.contains("This rule is heuristic"), "{stdout}");
}
