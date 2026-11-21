"""Runs the entry points of a generated project, each on its inputs, and writes what happened.

Usage: `python child.py SPEC.json OUT.json`. The spec names the project root, the entry points
with their inputs, a per-run timeout, and optionally a watch plan: with a plan, `sys.monitoring`
records the plan's call instances, iterations, and lines (see `monitor.py`); without one only
the fakes' events and the outcomes are recorded, which is what comparing a program with its
rewritten copy needs.

This file runs generated programs only. It imports nothing from the project until the fakes are
installed, so the program never reaches the network, a database, or a child process.
"""

from __future__ import annotations

import contextlib
import importlib
import inspect
import io
import json
import os
import re
import signal
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import fakes  # noqa: E402
import monitor  # noqa: E402

_HEX = re.compile(r"0x[0-9a-fA-F]+")


class RunTimeout(BaseException):
    """Raised by the timer: the run is discarded (assumption R7 excludes such exceptions)."""


def _on_alarm(signum, frame):
    raise RunTimeout()


class Values:
    """Builds argument values from their JSON specs."""

    def __init__(self, root):
        self.root = root
        self.shared = {}

    def build(self, spec):
        if isinstance(spec, list):
            return [self.build(item) for item in spec]
        if not isinstance(spec, dict):
            return spec
        ((tag, value),) = spec.items()
        if tag == "$tuple":
            return tuple(self.build(item) for item in value)
        if tag == "$set":
            return {self.build(item) for item in value}
        if tag == "$frozenset":
            return frozenset(self.build(item) for item in value)
        if tag == "$dict":
            return {self.build(k): self.build(v) for k, v in value}
        if tag == "$bytes":
            return value.encode()
        if tag == "$float":
            return float(value)
        if tag == "$obj":
            target, args, kwargs = value
            klass = self.resolve(target)
            return klass(*self.build(args), **{k: self.build(v) for k, v in kwargs.items()})
        if tag == "$shared":
            name, inner = value
            if name not in self.shared:
                self.shared[name] = self.build(inner)
            return self.shared[name]
        if tag == "$ref":
            return self.resolve(value)
        if tag == "$conn":
            return _sqlite_connect(value)
        if tag == "$cursor":
            return _sqlite_connect(value).cursor()
        if tag == "$pgcursor":
            return importlib.import_module("psycopg2").connect(value).cursor()
        if tag == "$queue":
            q = fakes.FakeQueue()
            for item in value:
                q.queue.append(self.build(item))
            return q
        if tag == "$array":
            return fakes.FakeArray(self.build(value))
        if tag == "$frame":
            return fakes.FakeFrame(self.build(value))
        if tag == "$path":
            # Relative to the project root, the working directory of the run, so that a program
            # and its rewritten copy see the same value.
            return value
        if tag == "$session":
            return fakes.FakeSession()
        raise ValueError(f"unknown value tag {tag}")

    def resolve(self, target):
        module_name, _, qualname = target.partition(":")
        obj = importlib.import_module(module_name)
        for part in qualname.split("."):
            obj = getattr(obj, part)
        return obj


def _sqlite_connect(database):
    import sqlite3

    return sqlite3.connect(database)


def _drive(result):
    if inspect.iscoroutine(result):
        try:
            while True:
                yielded = result.send(None)
                if yielded is not None:
                    raise RuntimeError("the coroutine awaited a future the oracle does not drive")
        except StopIteration as stop:
            return stop.value
    if inspect.isgenerator(result):
        out = []
        for item in result:
            out.append(item)
            if len(out) > 1000:
                break
        return out
    return result


def _exception(exc):
    return [type(exc).__name__, _HEX.sub("0x?", str(exc))]


def run(spec):
    root = os.path.realpath(spec["root"])
    sys.path.insert(0, root)
    os.chdir(root)
    plan = spec.get("plan")
    recorder = monitor.Recorder(root, plan or {})
    fakes.install(recorder)
    if plan is not None:
        recorder.install()
    values = Values(root)
    signal.signal(signal.SIGALRM, _on_alarm)
    timeout = float(spec.get("timeout", 2.0))
    out = {"runs": [], "error": None}
    for entry_index, entry in enumerate(spec["entries"]):
        try:
            function = values.resolve(f"{entry['module']}:{entry['func']}")
        except BaseException as exc:  # noqa: BLE001 - an import error is reported, not raised
            out["error"] = ["import"] + _exception(exc)
            break
        for input_index, given in enumerate(entry["inputs"]):
            fakes.ROWS = given.get("rows", 3)
            record = {"entry": entry_index, "input": input_index}
            try:
                args = values.build(given.get("args", []))
                kwargs = {k: values.build(v) for k, v in given.get("kwargs", {}).items()}
            except BaseException as exc:  # noqa: BLE001
                record["status"] = "bad-input"
                record["exc"] = _exception(exc)
                out["runs"].append(record)
                continue
            stdout = io.StringIO()
            recorder.begin_run()
            signal.setitimer(signal.ITIMER_REAL, timeout)
            try:
                with contextlib.redirect_stdout(stdout):
                    value = _drive(function(*args, **kwargs))
                record["status"] = "ok"
                record["value"] = monitor.snapshot(value)
            except RunTimeout:
                record["status"] = "timeout"
            except RecursionError as exc:
                record["status"] = "timeout"
                record["exc"] = _exception(exc)
            except BaseException as exc:  # noqa: BLE001 - the program's exception is its outcome
                record["status"] = "exc"
                record["exc"] = _exception(exc)
            finally:
                signal.setitimer(signal.ITIMER_REAL, 0)
                record["events"] = recorder.end_run()
                if recorder.errors:
                    record["monitor_errors"] = list(recorder.errors)
            record["args_after"] = [monitor.snapshot(a) for a in args]
            record["kwargs_after"] = {k: monitor.snapshot(v) for k, v in kwargs.items()}
            record["stdout"] = stdout.getvalue()
            out["runs"].append(record)
    if plan is not None:
        recorder.uninstall()
    return out


def main():
    spec_path, out_path = sys.argv[1], sys.argv[2]
    with open(spec_path, encoding="utf-8") as f:
        spec = json.load(f)
    result = run(spec)
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(result, f, default=repr)


if __name__ == "__main__":
    main()
