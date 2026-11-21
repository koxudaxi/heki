"""Checks one generated project: heki's exact reports against what the program does.

`check_project(root, heki, python)` runs `heki check` on the project, turns its exact reports into
claims (`plan.py`), runs every entry point on its inputs under the monitor (`child.py`), and
checks each claim (`checkers.py`). For the `OPT` reports with a rewrite it also applies
`heki check --fix --include-size-dependent` to a copy and compares what the original and the
rewritten program do on the same inputs; a difference is attributed to the report whose rewrite
alone reproduces it.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile

import checkers
import plan as planner

HERE = os.path.dirname(os.path.abspath(__file__))
CHILD = os.path.join(HERE, "child.py")
OPT_FIX_RULES = ("OPT101", "OPT102", "OPT110", "OPT111", "OPT120", "OPT121", "OPT130")


class Tools:
    def __init__(self, heki, python, run_timeout=2.0, heki_timeout=60.0):
        self.heki = heki
        self.python = python
        self.run_timeout = run_timeout
        self.heki_timeout = heki_timeout


def run_heki(tools, root, *extra):
    command = [tools.heki, "check", "--output-format", "json", "--config", "heki.toml", *extra, "app"]
    proc = subprocess.run(
        command, cwd=root, capture_output=True, text=True, timeout=tools.heki_timeout, env=_env()
    )
    if proc.returncode not in (0, 1):
        raise HekiError(f"heki exited with {proc.returncode}: {proc.stderr.strip()[:2000]}")
    return json.loads(proc.stdout or "[]")


class HekiError(Exception):
    pass


def _env():
    env = {k: v for k, v in os.environ.items() if not k.startswith("PYTHON")}
    env["PYTHONHASHSEED"] = "0"
    env["NO_COLOR"] = "1"
    return env


def run_child(tools, root, entries, watch=None):
    spec = {"root": root, "plan": watch, "entries": entries, "timeout": tools.run_timeout}
    with tempfile.TemporaryDirectory(prefix="oracle-run-") as tmp:
        spec_path = os.path.join(tmp, "spec.json")
        out_path = os.path.join(tmp, "out.json")
        with open(spec_path, "w", encoding="utf-8") as f:
            json.dump(spec, f)
        budget = 30 + tools.run_timeout * sum(len(e["inputs"]) for e in entries)
        try:
            proc = subprocess.run(
                # `-I` would also drop PYTHONHASHSEED; the environment is already cleaned.
                [tools.python, "-P", "-s", CHILD, spec_path, out_path],
                cwd=tmp,
                capture_output=True,
                text=True,
                timeout=budget,
                env=_env(),
            )
        except subprocess.TimeoutExpired:
            return {"runs": [], "error": ["child-timeout", "", ""]}
        if proc.returncode != 0 or not os.path.exists(out_path):
            return {"runs": [], "error": ["child-crash", str(proc.returncode), proc.stderr[-2000:]]}
        with open(out_path, encoding="utf-8") as f:
            return json.load(f)


def load_entries(root):
    with open(os.path.join(root, "oracle.json"), encoding="utf-8") as f:
        return json.load(f)["entries"]


def check_project(root, tools, entries=None):
    """Everything the oracle finds in one project, as a JSON-ready dict."""
    entries = entries if entries is not None else load_entries(root)
    out = {
        "reports": {},
        "checked": {},
        "refutations": [],
        "violations": [],
        "unmapped": [],
        "errors": [],
        "runs": {"total": 0, "timeout": 0, "bad-input": 0, "monitor-error": 0},
    }
    try:
        reports = run_heki(tools, root)
    except (HekiError, subprocess.TimeoutExpired, json.JSONDecodeError) as exc:
        out["errors"].append(["heki", str(exc)[:2000]])
        return out
    exact = [r for r in reports if r.get("certainty") == "exact" and r["rule_code"] in planner.EXACT_RULES]
    for report in exact:
        out["reports"][report["rule_code"]] = out["reports"].get(report["rule_code"], 0) + 1
    if not exact:
        return out
    built = planner.build(root, exact)
    out["unmapped"] = [[c["rule"], c["path"], c["line"]] for c in built.unmapped]
    by_id = built.reports
    trace = run_child(tools, root, entries, built.plan)
    if trace.get("error"):
        out["errors"].append(trace["error"])
    runs = trace.get("runs", [])
    _count_runs(out, runs)
    for claim in built.claims:
        result = checkers.check(claim, runs, built.sources)
        out["checked"][claim["rule"]] = out["checked"].get(claim["rule"], 0) + result["checked"]
        for finding in result["refutations"]:
            out["refutations"].append(_with_report(finding, claim, by_id))
        for finding in result["violations"]:
            out["violations"].append(_with_report(finding, claim, by_id))
    fixable = [c for c in built.claims if c["rule"] in OPT_FIX_RULES and c["has_fix"]]
    if fixable:
        _check_rewrites(root, tools, entries, fixable, by_id, out)
    return out


def _count_runs(out, runs):
    for run in runs:
        out["runs"]["total"] += 1
        if run.get("status") in ("timeout", "bad-input"):
            out["runs"][run["status"]] += 1
        if run.get("monitor_errors"):
            out["runs"]["monitor-error"] += 1
            if len(out["errors"]) < 3:
                out["errors"].append(["monitor", run["monitor_errors"][0][-1500:]])


def _with_report(finding, claim, by_id):
    report = by_id.get(claim["id"], {})
    finding = dict(finding)
    finding["report"] = {
        "path": report.get("path"),
        "line": report.get("line"),
        "column": report.get("column"),
        "message": report.get("message"),
        "help": report.get("help"),
        "target": report.get("target"),
        "related": [[r["path"], r["line"], r["message"]] for r in report.get("related", [])],
    }
    return finding


def _check_rewrites(root, tools, entries, fixable, by_id, out):
    original = run_child(tools, root, entries)
    if original.get("error"):
        out["errors"].append(original["error"])
        return
    with tempfile.TemporaryDirectory(prefix="oracle-fix-") as tmp:
        fixed_root = os.path.join(tmp, "fixed")
        shutil.copytree(root, fixed_root)
        proc = subprocess.run(
            [tools.heki, "check", "--fix", "--include-size-dependent", "--config", "heki.toml", "app"],
            cwd=fixed_root,
            capture_output=True,
            text=True,
            timeout=tools.heki_timeout,
            env=_env(),
        )
        if proc.returncode == 2:
            out["errors"].append(["heki-fix", proc.stderr[-2000:]])
            return
        rewritten = run_child(tools, fixed_root, entries)
        differences = checkers.compare_outcomes(original["runs"], rewritten.get("runs", []))
        if not differences:
            return
        blamed = []
        for claim in fixable:
            alone_root = os.path.join(tmp, f"alone-{claim['id']}")
            shutil.copytree(root, alone_root)
            _apply_edits(alone_root, by_id[claim["id"]])
            alone = run_child(tools, alone_root, entries)
            alone_diff = checkers.compare_outcomes(original["runs"], alone.get("runs", []))
            if alone_diff:
                blamed.append((claim, alone_diff))
        if not blamed:
            blamed = [(claim, differences) for claim in fixable]
            kind = "rewrite-changes-behaviour-together"
        else:
            kind = "rewrite-changes-behaviour"
        for claim, diff in blamed:
            entry, index, field, before, after = diff[0]
            finding = {
                "rule": claim["rule"],
                "claim": claim["id"],
                "kind": kind,
                "premise": "Rewrite proven safe: the rewritten program behaves differently",
                "entry": entry,
                "input": index,
                "detail": {"field": field, "original": before, "rewritten": after, "differences": len(diff)},
            }
            out["refutations"].append(_with_report(finding, claim, by_id))


def _apply_edits(root, report):
    path = os.path.join(root, report["path"])
    with open(path, "rb") as f:
        data = f.read()
    for edit in sorted(report.get("fix") or [], key=lambda e: e["span"]["start"], reverse=True):
        start, end = edit["span"]["start"], edit["span"]["end"]
        data = data[:start] + edit["replacement"].encode() + data[end:]
    with open(path, "wb") as f:
        f.write(data)


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Check one generated project directory.")
    parser.add_argument("root")
    parser.add_argument("--heki", required=True)
    parser.add_argument("--python", default=sys.executable)
    args = parser.parse_args()
    result = check_project(os.path.realpath(args.root), Tools(args.heki, args.python))
    json.dump(result, sys.stdout, indent=1)


if __name__ == "__main__":
    main()
