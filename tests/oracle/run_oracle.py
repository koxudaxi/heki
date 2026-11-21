#!/usr/bin/env python3
"""Run-time oracle for heki's exact claims.

Generates projects (`generator.py`), checks heki's exact reports on each against what the program
does under `sys.monitoring` (`driver.py`), and writes every refutation with its project to the
output directory, optionally shrunk to a small repro (`minimize.py`).

```console
$ cargo build --release -p heki
$ python3.15 tests/oracle/run_oracle.py --heki target/release/heki --seeds 2000 --time-limit 1800 --out /tmp/oracle --minimize
```

The output directory holds `summary.json` and `refutations/<RULE>-<kind>/seed-<n>/` (the project,
`finding.json`, and with `--minimize` the shrunk project in `min/`). Assumption violations go to
`violations/` the same way. The exit code is 1 when a refutation was found and
`--fail-on-refutation` is given, 2 when the oracle itself failed (heki failed on a project, a
child process crashed or timed out, the recorder failed, a generated program did not import),
and 0 otherwise.
"""

from __future__ import annotations

import argparse
import collections
import concurrent.futures
import json
import os
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import driver  # noqa: E402
import generator  # noqa: E402
import minimize  # noqa: E402


def check_seed(seed, args):
    tools = driver.Tools(args.heki, args.python, run_timeout=args.run_timeout)
    work = os.path.join(args.out, "work", f"seed-{seed}")
    if os.path.exists(work):
        shutil.rmtree(work)
    project = generator.generate(seed, families=args.families)
    project.write(work)
    started = time.monotonic()
    result = driver.check_project(os.path.realpath(work), tools)
    result["seed"] = seed
    result["families"] = project.families
    result["seconds"] = round(time.monotonic() - started, 3)
    kept = []
    for kind, findings in (("refutations", result["refutations"]), ("violations", result["violations"])):
        for finding in findings:
            target = os.path.join(args.out, kind, f"{finding['rule']}-{finding['kind']}", f"seed-{seed}")
            if os.path.exists(target):
                shutil.rmtree(target)
            shutil.copytree(work, target, ignore=shutil.ignore_patterns("__pycache__"))
            with open(os.path.join(target, "finding.json"), "w", encoding="utf-8") as f:
                json.dump(finding, f, indent=1)
            kept.append(target)
    shutil.rmtree(work, ignore_errors=True)
    return result


def summarize(results, started, args):
    summary = {
        "seeds": len(results),
        "first_seed": args.start,
        "seconds": round(time.monotonic() - started, 1),
        "exact_reports": collections.Counter(),
        "instances_checked": collections.Counter(),
        "refutations": collections.Counter(),
        "violations": collections.Counter(),
        "unmapped": collections.Counter(),
        "runs": collections.Counter(),
        "errors": collections.Counter(),
        "groups": {},
    }
    for result in results:
        summary["exact_reports"].update(result["reports"])
        summary["instances_checked"].update(result["checked"])
        summary["runs"].update(result["runs"])
        for rule, _path, _line in result["unmapped"]:
            summary["unmapped"][rule] += 1
        for error in result["errors"]:
            summary["errors"][error[0]] += 1
        for kind, findings in (("refutations", result["refutations"]), ("violations", result["violations"])):
            for finding in findings:
                summary[kind][finding["rule"]] += 1
                key = f"{kind}/{finding['rule']}-{finding['kind']}"
                group = summary["groups"].setdefault(
                    key,
                    {"premises": collections.Counter(), "seeds": [], "assumption": finding.get("assumption")},
                )
                group["premises"][finding["premise"]] += 1
                group["seeds"].append(result["seed"])
    return summary


def print_summary(summary):
    print(f"seeds: {summary['seeds']} (from {summary['first_seed']}), {summary['seconds']} s")
    print("exact reports per rule:", dict(sorted(summary["exact_reports"].items())))
    print("claim instances checked:", dict(sorted(summary["instances_checked"].items())))
    print("program runs:", dict(summary["runs"]))
    if summary["unmapped"]:
        print("reports the plan could not map:", dict(summary["unmapped"]))
    if summary["errors"]:
        print("oracle errors:", dict(summary["errors"]))
    print("refutations per rule:", dict(sorted(summary["refutations"].items())) or "none")
    print("assumption violations per rule:", dict(sorted(summary["violations"].items())) or "none")
    for key, group in sorted(summary["groups"].items()):
        seeds = group["seeds"]
        note = f" (assumption {group['assumption']})" if group.get("assumption") else ""
        print(f"  {key}{note}: {len(seeds)} in seeds {seeds[:8]}{' ...' if len(seeds) > 8 else ''}")
        for premise, count in group["premises"].most_common():
            print(f"      {count} x {premise}")


def minimize_groups(summary, args):
    tools = driver.Tools(args.heki, args.python, run_timeout=args.run_timeout)
    done = {}
    for key, group in sorted(summary["groups"].items()):
        if not key.startswith("refutations/"):
            continue
        for seed in group["seeds"][: args.minimize_per_group]:
            folder = os.path.join(args.out, key, f"seed-{seed}")
            with open(os.path.join(folder, "finding.json"), encoding="utf-8") as f:
                finding = json.load(f)
            outcome = minimize.minimize(
                folder, finding, tools, os.path.join(folder, "min"), args.minimize_budget
            )
            done[f"{key}/seed-{seed}"] = outcome
            print(f"minimized {key}/seed-{seed}: {outcome}")
    return done


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--heki", required=True, help="the heki binary to check")
    parser.add_argument(
        "--python", default=sys.executable, help="the interpreter that runs the programs (3.12+)"
    )
    parser.add_argument("--seeds", type=int, default=100, help="how many projects to generate")
    parser.add_argument("--start", type=int, default=0, help="the first seed")
    parser.add_argument(
        "--time-limit", type=float, default=600.0, help="stop starting new seeds after this many seconds"
    )
    parser.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 2) // 2))
    parser.add_argument("--out", required=True, help="output directory")
    parser.add_argument(
        "--families", nargs="*", choices=sorted(generator.FAMILIES), help="generate only these families"
    )
    parser.add_argument("--run-timeout", type=float, default=2.0, help="seconds per program run")
    parser.add_argument("--minimize", action="store_true", help="shrink refutations to small repros")
    parser.add_argument("--minimize-per-group", type=int, default=1)
    parser.add_argument("--minimize-budget", type=float, default=180.0, help="seconds per repro")
    parser.add_argument("--fail-on-refutation", action="store_true")
    args = parser.parse_args()
    args.heki = os.path.realpath(args.heki)
    args.out = os.path.realpath(args.out)
    os.makedirs(args.out, exist_ok=True)
    started = time.monotonic()
    results = []
    seeds = iter(range(args.start, args.start + args.seeds))
    with concurrent.futures.ProcessPoolExecutor(max_workers=args.jobs) as pool:
        pending = set()
        for seed in seeds:
            pending.add(pool.submit(check_seed, seed, args))
            if len(pending) >= args.jobs * 2:
                break
        while pending:
            finished, pending = concurrent.futures.wait(
                pending, return_when=concurrent.futures.FIRST_COMPLETED
            )
            for future in finished:
                results.append(future.result())
            if time.monotonic() - started < args.time_limit:
                for seed in seeds:
                    pending.add(pool.submit(check_seed, seed, args))
                    if len(pending) >= args.jobs * 2:
                        break
    shutil.rmtree(os.path.join(args.out, "work"), ignore_errors=True)
    results.sort(key=lambda r: r["seed"])
    summary = summarize(results, started, args)
    if args.minimize:
        summary["minimized"] = minimize_groups(summary, args)
    with open(os.path.join(args.out, "summary.json"), "w", encoding="utf-8") as f:
        json.dump(summary, f, indent=1, default=dict)
    with open(os.path.join(args.out, "results.jsonl"), "w", encoding="utf-8") as f:
        for result in results:
            f.write(
                json.dumps(
                    {k: result[k] for k in ("seed", "families", "reports", "checked", "seconds", "errors")}
                )
                + "\n"
            )
    print_summary(summary)
    # heki failing on a project, a child process that crashed or timed out, the recorder failing,
    # a generated program that does not import: the run checked less than it says.
    if summary["errors"]:
        return 2
    if args.fail_on_refutation and summary["refutations"]:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
