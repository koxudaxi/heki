"""Checks each exact claim against the runs `monitor.py` recorded.

One function per claim kind takes the claim (from `plan.py`), the runs of the program, and the
project's sources, and returns how many instances of the claim the runs exercised and the
refutations among them. A refutation names the premise it contradicts. It is a bug in heki unless
the program broke an assumption of `docs/precision.md`; such a case is returned as a violation
instead, with the assumption.

The claims, as the rule pages state them:

- `EFF101`: whenever the reported call returns, a waiting call has run during it, one the report
  names (a call that raised still ran).
- `EFF103`: every iteration of the loop that completes sends a database statement, one the report
  names.
- `DATA101`: on every iteration that completes, the variable's new value was built by the
  reallocating calls from its value at the start of the iteration.
- `MUT110`: whenever the named line runs during the reported call, reached through the calls the
  report shows, it changes the caller's object in place.
- `MUT114`: the reported line changes in place a list, dict, or set that the cached function
  returned.
- `OPT101` ... `OPT130` (the proven tier): the accumulator is of the builtin type the rule names
  when the statement runs; the rewrite keeps the program's behaviour (`compare_outcomes`).
- `OPT113` (the exact tier): an inner item whose key differs runs only the loads of the two keys
  and the comparison, no key load runs project code, and the inner loop steps through the same
  items with the same keys on every outer iteration.
"""

from __future__ import annotations

import ast

from plan import normalize_api

CATALOG_KINDS = ("sleep", "net", "subprocess", "db_stmt", "db_other", "thread")
CONTAINER_KINDS = ("list", "dict", "set")
BUILTIN_CONTAINERS = ("list", "dict", "set", "bytearray", "deque")


class Result:
    def __init__(self, claim):
        self.claim = claim
        self.checked = 0
        self.refutations = []
        self.violations = []

    def refute(self, kind, premise, run, detail, assumption=None):
        finding = {
            "rule": self.claim["rule"],
            "claim": self.claim["id"],
            "kind": kind,
            "premise": premise,
            "entry": run.get("entry"),
            "input": run.get("input"),
            "detail": detail,
        }
        if assumption is not None:
            finding["assumption"] = assumption
            target = self.violations
        else:
            target = self.refutations
        for known in target:
            if known["kind"] == kind:
                known["count"] = known.get("count", 1) + 1
                return
        target.append(finding)

    def as_dict(self):
        return {
            "claim": self.claim["id"],
            "rule": self.claim["rule"],
            "checked": self.checked,
            "refutations": self.refutations,
            "violations": self.violations,
        }


def valid_runs(runs):
    for run in runs:
        if run.get("status") in ("ok", "exc") and not run.get("monitor_errors"):
            yield run


def _of(events, tag, key=None):
    return [e for e in events if e[0] == tag and (key is None or e[1] == key)]


def _inside(fakes, start, end):
    return [f for f in fakes if start < f[1] < end]


def _brief(fake):
    stack = fake[6]
    where = f"{stack[0][0]}:{stack[0][2]}" if stack else "?"
    return {"api": fake[2], "kind": fake[3], "waits": fake[4], "arg": fake[5], "at": where}


# ---------------------------------------------------------------------------------------------
# EFF101
# ---------------------------------------------------------------------------------------------
def check_eff101(claim, runs, sources=None):
    result = Result(claim)
    named = set(claim.get("witnesses") or ())
    for run in valid_runs(runs):
        events = run["events"]
        fakes = _of(events, "fake")
        for call in _of(events, "call", claim["site"]):
            if call[5] != "ok":
                continue
            result.checked += 1
            inside = _inside(fakes, call[3], call[4])
            waits = [f for f in inside if f[4]]
            if waits:
                if named and not any(normalize_api(f[2]) in named for f in waits):
                    result.refute(
                        "witness",
                        "Waits: the waiting call that ran is not the one named",
                        run,
                        {"named": sorted(named), "ran": [_brief(f) for f in waits]},
                    )
                continue
            catalog = [f for f in inside if f[3] in CATALOG_KINDS]
            detail = {"ran": [_brief(f) for f in inside], "named": sorted(named)}
            if not catalog:
                result.refute(
                    "no-wait",
                    "Every return / Every implementation: the call returned without reaching a catalog call",
                    run,
                    detail,
                )
                continue
            if sources is not None and all(
                f[3] in ("sleep", "net") and r6_excused(f, sources) for f in catalog
            ):
                result.refute("no-wait", "Waits", run, detail, assumption="R6")
                continue
            result.refute(
                "no-wait",
                "Waits: the catalog call ran but did not wait for input or output",
                run,
                detail,
            )
    return result


def r6_excused(fake, sources):
    """Whether a call that returned at once got its deciding argument from an expression heki
    cannot evaluate (assumption R6): not a literal, a module constant, or a parameter."""
    stack = fake[6]
    if not stack:
        return False
    path, _qualname, line, col = stack[0][:4]
    source = sources.get(path)
    if source is None:
        return False
    tree = source.tree
    call = None
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and node.lineno == line and (col is None or node.col_offset == col):
            call = node
            break
    if call is None or not call.args:
        return False
    return not _evaluable(call.args[0], tree, line)


def _evaluable(expr, tree, line):
    """A literal, a name the module binds to a literal, or a parameter of the enclosing function:
    values heki is to evaluate (rule pages of `EFF101`; `docs/precision.md`, R6). A name imported
    from another module, or any other expression, is not."""
    if isinstance(expr, ast.Constant):
        return True
    if isinstance(expr, ast.Tuple):
        return all(_evaluable(e, tree, line) for e in expr.elts)
    if not isinstance(expr, ast.Name):
        return False
    for stmt in tree.body:
        if isinstance(stmt, ast.Assign) and any(
            isinstance(t, ast.Name) and t.id == expr.id for t in stmt.targets
        ):
            return isinstance(stmt.value, ast.Constant)
    for node in ast.walk(tree):
        if (
            isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
            and node.lineno <= line <= node.end_lineno
        ):
            params = node.args.posonlyargs + node.args.args + node.args.kwonlyargs
            if any(p.arg == expr.id for p in params):
                return True
    return False


# ---------------------------------------------------------------------------------------------
# EFF103
# ---------------------------------------------------------------------------------------------
def check_eff103(claim, runs, sources=None):
    result = Result(claim)
    named = set(claim.get("witnesses") or ())
    for run in valid_runs(runs):
        events = run["events"]
        fakes = _of(events, "fake")
        calls = _of(events, "call", claim["site"])
        for it in _of(events, "iter", claim["loop"]):
            if not it[6]:
                continue
            result.checked += 1
            start, end = it[4], it[5]
            inside = _inside(fakes, start, end)
            statements = [f for f in inside if f[3] == "db_stmt"]
            if statements:
                if named and not any(normalize_api(f[2]) in named for f in statements):
                    result.refute(
                        "witness",
                        "Sends: the statement sent is not the one named",
                        run,
                        {"named": sorted(named), "ran": [_brief(f) for f in statements]},
                    )
                continue
            ran = [c for c in calls if start < c[3] < end]
            if not ran:
                premise = "Every iteration: the reported call did not run on this iteration"
            elif any(c[5] == "ok" for c in ran):
                premise = "Every return / Every implementation / Sends: the call returned without a statement"
            else:
                premise = (
                    "Every iteration: the reported call raised before sending, and the iteration went on"
                )
            result.refute(
                "no-statement",
                premise,
                run,
                {"ran": [_brief(f) for f in inside], "named": sorted(named), "calls": [c[5] for c in ran]},
            )
    return result


# ---------------------------------------------------------------------------------------------
# DATA101
# ---------------------------------------------------------------------------------------------
def check_data101(claim, runs, sources=None):
    result = Result(claim)
    target = claim["target"]
    for run in valid_runs(runs):
        events = run["events"]
        fakes = [f for f in _of(events, "fake") if f[3] == "realloc"]
        for it in _of(events, "iter", claim["loop"]):
            if not it[6]:
                continue
            start_caps, end_caps = it[7] or {}, it[9] or {}
            first, last = start_caps.get(target), end_caps.get(target)
            if not first or not last:
                continue
            result.checked += 1
            built = {first[0]}
            reallocs = _inside(fakes, it[4], it[5])
            for f in reallocs:
                if any(i in built for i in f[7]) and f[8] is not None:
                    built.add(f[8])
            if last[0] != first[0] and last[0] in built:
                continue
            if not reallocs:
                premise = "Every iteration: no reallocating call returned on this iteration"
            elif last[0] == first[0]:
                premise = "Loop-carried on every path: the value was not rebuilt on this iteration"
            else:
                premise = "Loop-carried on every path / Reallocates: the new value was not built from the previous one"
            result.refute(
                "not-rebuilt",
                premise,
                run,
                {
                    "before": first[1:],
                    "after": last[1:],
                    "reallocations": [_brief(f) for f in reallocs],
                },
            )
    return result


# ---------------------------------------------------------------------------------------------
# MUT110
# ---------------------------------------------------------------------------------------------
def operated(claim, line):
    """The objects a line changed in place: a builtin container method called on them, a
    subscript store or `del` into what a name held, an augmented assignment that left the name
    on the same object, or a change of their contents."""
    before = {i: snap for i, _t, snap in line[7]}
    after = {i: snap for i, _t, snap in line[8]}
    exact = {i for i, t, _snap in line[7] if t in BUILTIN_CONTAINERS}
    objects = {i for i in before if before[i] != after.get(i)}
    objects.update(m[0] for m in line[9])
    names_before = line[11] if len(line) > 11 else {}
    names_after = line[12] if len(line) > 12 else {}
    # A store or an augmented assignment changes a builtin container even when it leaves the
    # same contents (`s |= {x}` with `x` already in `s`); a subclass's override may do nothing.
    for name in claim.get("stores", ()):
        if names_before.get(name) in exact:
            objects.add(names_before[name])
    for name in claim.get("augs", ()):
        if names_before.get(name) in exact and names_after.get(name) == names_before[name]:
            objects.add(names_before[name])
    return objects


def check_mut110(claim, runs, sources=None):
    result = Result(claim)
    param = claim["target"]
    hops = [list(h) for h in claim["hops"]]
    for run in valid_runs(runs):
        events = run["events"]
        lines = _of(events, "line", claim["line_id"])
        for call in _of(events, "call", claim["site"]):
            captured = (call[6] or {}).get(param)
            if not captured:
                continue
            obj = captured[0]
            for line in lines:
                if not call[3] < line[3] <= line[4] <= call[4]:
                    continue
                chain = line[6] or []
                cut = next((k for k, frame in enumerate(chain) if frame[4] == call[2]), None)
                if cut is None:
                    continue
                shown = [[frame[0], frame[2]] for frame in reversed(chain[1 : cut + 1])]
                if shown != hops:
                    continue
                if line[5] != "ok":
                    continue
                result.checked += 1
                changed = operated(claim, line)
                muts = line[9]
                if obj in changed:
                    continue
                others = sorted(changed)
                premise = (
                    "Passed as itself: the line changed another object"
                    if others
                    else "Changed at the line: the line ran and left the caller's object unchanged"
                )
                result.refute(
                    "not-changed",
                    premise,
                    run,
                    {"object": captured[1:], "line": claim["change"], "mutations": muts},
                )
    return result


# ---------------------------------------------------------------------------------------------
# MUT114
# ---------------------------------------------------------------------------------------------
def check_mut114(claim, runs, sources=None):
    result = Result(claim)
    cached = {}
    for run in runs:
        for ret in _of(run.get("events", []), "ret", claim["ret"]):
            cached[ret[4]] = ret[6] if len(ret) > 6 else ret[5]
    for run in valid_runs(runs):
        for line in _of(run["events"], "line", claim["line_id"]):
            if line[5] != "ok":
                continue
            result.checked += 1
            types = {i: t for i, t, _snap in line[7]}
            for m in line[9]:
                types.setdefault(m[0], m[2])
            changed = operated(claim, line)
            after_of = {i: snap for i, _t, snap in line[8]}
            hits = [i for i in changed if i in cached]
            if not hits:
                premise = (
                    "Same object: the line changed an object the cached function did not return"
                    if changed
                    else "Changed in place: the line ran and changed nothing it names"
                )
                result.refute(
                    "not-cached-object",
                    premise,
                    run,
                    {
                        "changed": sorted(types.get(i, "?") for i in changed),
                        "line_objects": [[t, snap, after_of.get(i)] for i, t, snap in line[7]],
                    },
                )
                continue
            if not any(cached[i] in CONTAINER_KINDS for i in hits):
                result.refute(
                    "not-container",
                    "Container kind: the cached object is not a list, dict, or set",
                    run,
                    {"types": [cached[i] for i in hits]},
                )
    return result


# ---------------------------------------------------------------------------------------------
# MUT115: an object below a shallow copy, which the copied value held there
# ---------------------------------------------------------------------------------------------
# The classes whose augmented assignment changes the object in place.
IN_PLACE_TYPES = ("list", "dict", "set", "bytearray", "deque")


def check_mut115(claim, runs, sources=None):
    """At each run of the change line that completes, after the copy line ran in the same
    activation, for the first of the claim's candidates that applies (an augmented assignment
    changes its target in place when the target is a builtin container, else the object it stores
    into): the object the line changes is the one the copied value held at the access path when
    the copy line started (else Still shared), the copy held that object there when the copy line
    ended (else Shallow copy), and the line changed it (else Changed below the copy): a store, a
    delete, and an in-place operator change it by running; a method call must show a builtin
    changing method or a change of its contents. A path that cannot be loaded without running
    code is not checked."""
    result = Result(claim)
    for run in valid_runs(runs):
        copies = {}
        events = sorted(
            (e for e in run["events"] if e[0] == "line" and e[1] in (claim["copy_line_id"], claim["line_id"])),
            key=lambda e: e[3],
        )
        for event in events:
            evals_before = event[13] if len(event) > 13 else {}
            evals_after = event[14] if len(event) > 14 else {}
            if event[1] == claim["copy_line_id"]:
                if event[5] == "ok":
                    copies[event[2]] = (evals_before, evals_after)
                continue
            if event[5] != "ok" or event[2] not in copies:
                continue
            change = _applying_change(claim["changes"], evals_before)
            if change is None:
                continue
            copy_before, copy_after = copies[event[2]]
            held = copy_before.get(change["held"])
            in_copy = copy_after.get(change["copy"])
            changed_before = evals_before.get(change["changed"])
            changed_after = evals_after.get(change["changed"])
            if held is None or changed_before is None:
                continue
            result.checked += 1
            if in_copy is not None and in_copy[0] != held[0]:
                result.refute(
                    "copy-holds-another-object",
                    "Shallow copy: the copy does not hold the object the copied value holds there",
                    run,
                    {"held": held[1], "in_copy": in_copy[1]},
                )
                continue
            if changed_before[0] != held[0]:
                result.refute(
                    "not-the-copied-object",
                    "Still shared: the line changes another object than the copied value held there",
                    run,
                    {"held": held[1:], "changed": changed_before[1:]},
                )
                continue
            if change["kind"] != "call":
                continue
            same_after = changed_after is not None and changed_after[0] == changed_before[0]
            mutated = any(m[0] == changed_before[0] for m in event[9])
            if same_after and changed_after[2] == changed_before[2] and not mutated:
                result.refute(
                    "unchanged",
                    "Changed below the copy: the line ran and left the object as it was",
                    run,
                    {"object": changed_before[1:]},
                )
    return result


def _applying_change(changes, evals_before):
    """The first candidate change that applies at a run of the line: an in-place one only where
    its target holds a builtin container when the line starts."""
    for change in changes:
        if change["kind"] == "inplace":
            target = evals_before.get(change["changed"])
            if target is None or target[1] not in IN_PLACE_TYPES:
                continue
        return change
    return None


# ---------------------------------------------------------------------------------------------
# OPT proven tier: the accumulator's type when the statement runs
# ---------------------------------------------------------------------------------------------
def check_opt_type(claim, runs, sources=None):
    result = Result(claim)
    name = claim.get("type_name")
    expected = set(claim.get("expected_types") or ())
    if not name or not expected:
        return result
    for run in valid_runs(runs):
        for line in _of(run["events"], "line", claim["line_id"]):
            seen = (line[10] or {}).get(name)
            if seen is None:
                continue
            result.checked += 1
            if seen not in expected:
                result.refute(
                    "not-exact-type",
                    "Builtin type by construction: the value is not of the builtin type the rule names",
                    run,
                    {"name": name, "type": seen, "expected": sorted(expected)},
                )
    return result


# ---------------------------------------------------------------------------------------------
# OPT113
# ---------------------------------------------------------------------------------------------
def check_opt113(claim, runs, sources=None):
    result = Result(claim)
    allowed = set(claim["allowed_lines"])
    test_lines = set(claim["test_lines"])
    keys = claim.get("keys") or []
    names = claim.get("inner_names") or []
    for run in valid_runs(runs):
        events = run["events"]
        code = [e for e in events if e[0] in ("start", "resume")]
        iters = _of(events, "iter", claim["inner"])
        exhausted = {(e[2], e[3]) for e in _of(events, "exhaust", claim["inner"])}
        traversals = {}
        for it in iters:
            act, start, end, fetch = it[2], it[4], it[5], it[11]
            body = it[8] or {}
            from_test = [
                c
                for c in code
                if c[0] == "start" and start < c[1] < end and c[5] == act and c[6] in test_lines
            ]
            if from_test:
                result.refute(
                    "key-runs-code",
                    "Equality of keys: loading a key ran code of the project",
                    run,
                    {"ran": sorted({c[3] for c in from_test})},
                )
            item = tuple((body.get(n) or [None])[0] for n in names)
            if names and all(v is not None for v in item):
                inner_key = body.get("key:" + keys[1]) if len(keys) == 2 else None
                traversals.setdefault((act, it[3]), []).append((item, inner_key))
            if len(keys) != 2:
                continue
            outer_value, inner_value = body.get("key:" + keys[0]), body.get("key:" + keys[1])
            if not isinstance(outer_value, list) or not isinstance(inner_value, list):
                continue
            if not (outer_value[1] and inner_value[1]) or outer_value[0] == inner_value[0]:
                continue
            result.checked += 1
            extra = sorted(set(it[10] or ()) - allowed)
            if extra:
                result.refute(
                    "busy-on-mismatch",
                    "Idle on a mismatch: an inner item whose key differs ran other statements",
                    run,
                    {"lines": extra, "keys": [outer_value[0], inner_value[0]]},
                )
            advanced = [c for c in code if fetch < c[1] < end]
            if advanced:
                result.refute(
                    "code-on-mismatch",
                    "Idle on a mismatch: an inner item whose key differs ran code of the project",
                    run,
                    {"ran": sorted({c[3] for c in advanced})},
                )
        _check_same_collection(result, run, traversals, exhausted)
    return result


def _check_same_collection(result, run, traversals, exhausted):
    by_act = {}
    for (act, entry), steps in traversals.items():
        by_act.setdefault(act, []).append((entry, steps))
    for act, entries in by_act.items():
        full = [steps for entry, steps in entries if (act, entry) in exhausted]
        reference = full[0] if full else max((steps for _e, steps in entries), key=len)
        items = [s[0] for s in reference]
        key_of = {s[0]: s[1] for s in reference}
        for entry, steps in entries:
            seen = [s[0] for s in steps]
            complete = (act, entry) in exhausted
            if (complete and seen != items) or (not complete and seen != items[: len(seen)]):
                result.refute(
                    "collection-changed",
                    "Same collection, same keys: the inner loop stepped through other items on a later outer iteration",
                    run,
                    {"first": len(items), "later": len(seen)},
                )
                break
            changed = [s for s in steps if s[0] in key_of and s[1] != key_of[s[0]]]
            if changed:
                result.refute(
                    "key-changed",
                    "Same collection, same keys: an inner item's key changed between outer iterations",
                    run,
                    {"before": key_of[changed[0][0]], "after": changed[0][1]},
                )
                break


CHECKERS = {
    "EFF101": check_eff101,
    "EFF103": check_eff103,
    "DATA101": check_data101,
    "MUT110": check_mut110,
    "MUT114": check_mut114,
    "MUT115": check_mut115,
    "OPT101": check_opt_type,
    "OPT102": check_opt_type,
    "OPT103": check_opt_type,
    "OPT110": check_opt_type,
    "OPT111": check_opt_type,
    "OPT120": check_opt_type,
    "OPT121": check_opt_type,
    "OPT130": check_opt_type,
    "OPT113": check_opt113,
}


def check(claim, runs, sources=None):
    return CHECKERS[claim["rule"]](claim, runs, sources).as_dict()


# ---------------------------------------------------------------------------------------------
# OPT rewrites: the original and the rewritten program on the same inputs
# ---------------------------------------------------------------------------------------------
OBSERVED = ("status", "value", "exc", "args_after", "kwargs_after", "stdout")


def observable(run):
    """What a run shows outside the function: its result or exception, the arguments' final
    state, its output, and the library calls it made, in order."""
    seen = {key: run.get(key) for key in OBSERVED}
    seen["calls"] = [[e[2], e[5]] for e in run.get("events", []) if e[0] == "fake"]
    return seen


def compare_outcomes(original, rewritten):
    """The runs whose observable behaviour differs, as `(entry, input, field, before, after)`."""
    differences = []
    after = {(r["entry"], r["input"]): r for r in rewritten}
    for run in original:
        if run.get("status") in ("timeout", "bad-input"):
            continue
        other = after.get((run["entry"], run["input"]))
        if other is None:
            continue
        if other.get("status") == "timeout":
            # `for v in x: x = x + [v]` ends; `x.append(v)` there never does.
            differences.append((run["entry"], run["input"], "status", run.get("status"), "timeout"))
            continue
        a, b = observable(run), observable(other)
        for key in a:
            if a[key] != b[key]:
                differences.append((run["entry"], run["input"], key, a[key], b[key]))
                break
    return differences
