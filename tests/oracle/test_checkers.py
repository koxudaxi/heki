#!/usr/bin/env python3
"""Unit tests of the oracle's checkers: for each claim kind, a synthetic trace that confirms the
claim and one that refutes it, in the event format `monitor.py` writes; and of the recorder on
real loops of the running interpreter.

`python3.15 tests/oracle/test_checkers.py`
"""

from __future__ import annotations

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import checkers  # noqa: E402
import monitor  # noqa: E402
from plan import Source, normalize_api  # noqa: E402


def run(*events, status="ok"):
    return {"entry": 0, "input": 0, "status": status, "events": list(events)}


def fake(seq, api, kind, waits, detail=None, at=("app/m.py", "f", 3, 4, 1), inputs=(), output=None):
    return ["fake", seq, api, kind, waits, detail, [list(at)], list(inputs), output]


def call(site, start, end, outcome="ok", captures=None, act=0):
    return ["call", site, act, start, end, outcome, captures]


def iteration(
    loop,
    start,
    end,
    completed=True,
    start_caps=None,
    body_caps=None,
    end_caps=None,
    lines=None,
    fetch=None,
    act=0,
    entry=1,
):
    return [
        "iter",
        loop,
        act,
        entry,
        start,
        end,
        completed,
        start_caps,
        body_caps,
        end_caps,
        lines,
        start - 1 if fetch is None else fetch,
    ]


def line(
    line_id,
    start,
    end,
    before=(),
    after=(),
    muts=(),
    types=None,
    chain=None,
    names=None,
    names_after=None,
    outcome="ok",
    act=1,
):
    return [
        "line",
        line_id,
        act,
        start,
        end,
        outcome,
        chain,
        list(before),
        list(after),
        list(muts),
        types or {},
        names or {},
        names_after or {},
    ]


class Eff101(unittest.TestCase):
    claim = {"id": "k0", "rule": "EFF101", "site": "c0", "witnesses": ["time.sleep"]}

    def test_a_wait_inside_every_returning_call_confirms(self):
        trace = run(fake(2, "time.sleep", "sleep", True, "1"), call("c0", 1, 3))
        result = checkers.check(self.claim, [trace])
        self.assertEqual((result["checked"], result["refutations"]), (1, []))

    def test_a_call_that_raised_is_not_an_instance(self):
        trace = run(call("c0", 1, 3, outcome="exc"))
        self.assertEqual(checkers.check(self.claim, [trace])["checked"], 0)

    def test_a_return_without_a_catalog_call_refutes_every_return(self):
        trace = run(fake(5, "time.sleep", "sleep", True), call("c0", 1, 3))
        refutation = checkers.check(self.claim, [trace])["refutations"][0]
        self.assertEqual(refutation["kind"], "no-wait")
        self.assertTrue(refutation["premise"].startswith("Every return"))

    def test_a_call_that_returned_at_once_refutes_waits(self):
        trace = run(fake(2, "time.sleep", "sleep", False, "0"), call("c0", 1, 3))
        refutation = checkers.check(self.claim, [trace])["refutations"][0]
        self.assertTrue(refutation["premise"].startswith("Waits"))

    def test_another_waiting_call_than_the_named_one_refutes_the_witness(self):
        trace = run(fake(2, "requests.get", "net", True), call("c0", 1, 3))
        self.assertEqual(checkers.check(self.claim, [trace])["refutations"][0]["kind"], "witness")

    def test_a_computed_argument_that_returns_at_once_is_an_r6_violation(self):
        source = _source("import time\n\n\ndef f():\n    time.sleep(compute())\n")
        trace = run(
            fake(2, "time.sleep", "sleep", False, "0", at=("app/m.py", "f", 5, 4, 1)), call("c0", 1, 3)
        )
        result = checkers.check(self.claim, [trace], {"app/m.py": source})
        self.assertEqual(result["refutations"], [])
        self.assertEqual(result["violations"][0]["assumption"], "R6")

    def test_a_module_constant_that_returns_at_once_is_heki_s(self):
        source = _source("import time\nPAUSE = 0\n\n\ndef f():\n    time.sleep(PAUSE)\n")
        trace = run(
            fake(2, "time.sleep", "sleep", False, "0", at=("app/m.py", "f", 6, 4, 1)), call("c0", 1, 3)
        )
        result = checkers.check(self.claim, [trace], {"app/m.py": source})
        self.assertEqual(len(result["refutations"]), 1)


class Eff103(unittest.TestCase):
    claim = {
        "id": "k0",
        "rule": "EFF103",
        "site": "c0",
        "loop": "L0",
        "witnesses": ["sqlite3.cursor.execute"],
    }

    def test_a_statement_on_every_completed_iteration_confirms(self):
        trace = run(
            fake(3, "sqlite3.Cursor.execute", "db_stmt", True),
            iteration("L0", 1, 5),
            iteration("L0", 6, 9, completed=False),
        )
        result = checkers.check(self.claim, [trace])
        self.assertEqual((result["checked"], result["refutations"]), (1, []))

    def test_a_completed_iteration_without_a_statement_refutes(self):
        trace = run(
            fake(3, "sqlite3.Cursor.execute", "db_stmt", True), iteration("L0", 1, 5), iteration("L0", 6, 9)
        )
        refutation = checkers.check(self.claim, [trace])["refutations"][0]
        self.assertEqual(refutation["kind"], "no-statement")
        self.assertTrue(refutation["premise"].startswith("Every iteration"))

    def test_another_statement_than_the_named_one_refutes_the_witness(self):
        trace = run(fake(3, "psycopg2.extensions.Cursor.execute", "db_stmt", True), iteration("L0", 1, 5))
        self.assertEqual(checkers.check(self.claim, [trace])["refutations"][0]["kind"], "witness")


class Data101(unittest.TestCase):
    claim = {"id": "k0", "rule": "DATA101", "loop": "L0", "target": "acc"}

    def test_a_value_rebuilt_from_the_previous_one_confirms(self):
        trace = run(
            fake(3, "numpy.append", "realloc", False, inputs=[10, 11], output=12),
            iteration(
                "L0", 1, 5, start_caps={"acc": [10, "FakeArray", {}]}, end_caps={"acc": [12, "FakeArray", {}]}
            ),
        )
        result = checkers.check(self.claim, [trace])
        self.assertEqual((result["checked"], result["refutations"]), (1, []))

    def test_a_value_built_from_another_one_refutes(self):
        trace = run(
            fake(3, "numpy.append", "realloc", False, inputs=[20, 11], output=12),
            iteration(
                "L0", 1, 5, start_caps={"acc": [10, "FakeArray", {}]}, end_caps={"acc": [12, "FakeArray", {}]}
            ),
        )
        self.assertEqual(checkers.check(self.claim, [trace])["refutations"][0]["kind"], "not-rebuilt")

    def test_an_iteration_without_a_reallocation_refutes(self):
        trace = run(
            iteration(
                "L0", 1, 5, start_caps={"acc": [10, "FakeArray", {}]}, end_caps={"acc": [10, "FakeArray", {}]}
            )
        )
        refutation = checkers.check(self.claim, [trace])["refutations"][0]
        self.assertTrue(refutation["premise"].startswith("On every iteration"))


class Mut110(unittest.TestCase):
    claim = {
        "id": "k0",
        "rule": "MUT110",
        "site": "c0",
        "line_id": "N0",
        "target": "values",
        "hops": [["app/m.py", 10]],
        "change": ["app/m.py", 4],
        "receivers": ["xs"],
        "stores": [],
        "augs": [],
    }
    chain = [["app/m.py", "helper", 4, 8, 2], ["app/m.py", "entry", 10, 4, 1]]

    def test_the_caller_s_object_changed_at_the_line_confirms(self):
        trace = run(
            line(
                "N0",
                3,
                5,
                before=[[7, "list", {"list": [1]}]],
                after=[[7, "list", {"list": [1, 2]}]],
                muts=[[7, "append", "list"]],
                chain=self.chain,
                act=2,
            ),
            call("c0", 2, 6, captures={"values": [7, "list", {}]}, act=1),
        )
        result = checkers.check(self.claim, [trace])
        self.assertEqual((result["checked"], result["refutations"]), (1, []))

    def test_a_line_reached_through_other_calls_is_not_an_instance(self):
        other = [["app/m.py", "helper", 4, 8, 2], ["app/m.py", "elsewhere", 30, 4, 1]]
        trace = run(
            line("N0", 3, 5, chain=other, act=2),
            call("c0", 2, 6, captures={"values": [7, "list", {}]}, act=1),
        )
        self.assertEqual(checkers.check(self.claim, [trace])["checked"], 0)

    def test_the_line_changing_another_object_refutes(self):
        trace = run(
            line(
                "N0",
                3,
                5,
                before=[[7, "list", {"list": [1]}], [8, "list", {"list": []}]],
                after=[[7, "list", {"list": [1]}], [8, "list", {"list": [2]}]],
                muts=[[8, "append", "list"]],
                chain=self.chain,
                act=2,
            ),
            call("c0", 2, 6, captures={"values": [7, "list", {}]}, act=1),
        )
        refutation = checkers.check(self.claim, [trace])["refutations"][0]
        self.assertEqual(refutation["kind"], "not-changed")
        self.assertTrue(refutation["premise"].startswith("Passed as itself"))

    def test_a_subscript_store_that_leaves_the_same_value_still_changes_in_place(self):
        claim = dict(self.claim, receivers=[], stores=["xs"])
        trace = run(
            line(
                "N0",
                3,
                5,
                before=[[7, "list", {"list": [1]}]],
                after=[[7, "list", {"list": [1]}]],
                chain=self.chain,
                names={"xs": 7},
                names_after={"xs": 7},
                act=2,
            ),
            call("c0", 2, 6, captures={"values": [7, "list", {}]}, act=1),
        )
        self.assertEqual(checkers.check(claim, [trace])["refutations"], [])


class Mut114(unittest.TestCase):
    claim = {
        "id": "k0",
        "rule": "MUT114",
        "ret": "R0",
        "line_id": "N0",
        "receivers": ["result"],
        "stores": [],
        "augs": ["result"],
    }

    def test_changing_what_the_cached_function_returned_confirms(self):
        trace = run(
            ["ret", "R0", 3, 2, 50, "list"],
            line(
                "N0",
                4,
                6,
                before=[[50, "list", {}]],
                after=[[50, "list", {"x": 1}]],
                muts=[[50, "append", "list"]],
            ),
        )
        result = checkers.check(self.claim, [trace])
        self.assertEqual((result["checked"], result["refutations"]), (1, []))

    def test_an_augmented_assignment_on_the_same_object_confirms_without_a_visible_change(self):
        trace = run(
            ["ret", "R0", 3, 2, 50, "set", "set"],
            line(
                "N0",
                4,
                6,
                before=[[50, "set", {}]],
                after=[[50, "set", {}]],
                names={"result": 50},
                names_after={"result": 50},
            ),
        )
        self.assertEqual(checkers.check(self.claim, [trace])["refutations"], [])

    def test_a_subclass_override_that_changes_nothing_refutes_changed_in_place(self):
        trace = run(
            ["ret", "R0", 3, 2, 50, "Frozen", "list"],
            line(
                "N0",
                4,
                6,
                before=[[50, "Frozen", {}]],
                after=[[50, "Frozen", {}]],
                names={"result": 50},
                names_after={"result": 50},
            ),
        )
        refutation = checkers.check(self.claim, [trace])["refutations"][0]
        self.assertTrue(refutation["premise"].startswith("Changed in place"))

    def test_changing_a_fresh_object_refutes_same_object(self):
        trace = run(
            ["ret", "R0", 3, 2, 50, "list"],
            line(
                "N0",
                4,
                6,
                before=[[60, "list", {}]],
                after=[[60, "list", {"x": 1}]],
                muts=[[60, "append", "list"]],
            ),
        )
        refutation = checkers.check(self.claim, [trace])["refutations"][0]
        self.assertEqual(refutation["kind"], "not-cached-object")
        self.assertTrue(refutation["premise"].startswith("Same object"))

    def test_changing_a_cached_object_of_another_kind_refutes_container_kind(self):
        trace = run(
            ["ret", "R0", 3, 2, 50, "deque", "deque"], line("N0", 4, 6, muts=[[50, "append", "deque"]])
        )
        self.assertEqual(checkers.check(self.claim, [trace])["refutations"][0]["kind"], "not-container")


class OptType(unittest.TestCase):
    claim = {"id": "k0", "rule": "OPT101", "line_id": "N0", "type_name": "x", "expected_types": ["list"]}

    def test_a_builtin_list_confirms(self):
        result = checkers.check(self.claim, [run(line("N0", 1, 2, types={"x": "list"}))])
        self.assertEqual((result["checked"], result["refutations"]), (1, []))

    def test_a_list_subclass_refutes(self):
        result = checkers.check(self.claim, [run(line("N0", 1, 2, types={"x": "MyList@app.m"}))])
        self.assertEqual(result["refutations"][0]["kind"], "not-exact-type")


class Opt113(unittest.TestCase):
    claim = {
        "id": "k0",
        "rule": "OPT113",
        "inner": "L0i",
        "outer": "L0o",
        "allowed_lines": [5, 6],
        "test_lines": [6],
        "keys": ["a.k", "b.k"],
        "inner_names": ["b"],
    }

    def body(self, item, outer_key, inner_key):
        return {"key:a.k": [outer_key, True], "key:b.k": [inner_key, True], "b": [item, "B", {}]}

    def test_an_idle_mismatch_confirms(self):
        trace = run(
            iteration("L0i", 2, 4, body_caps=self.body(100, 1, 2), lines=[5, 6]),
            iteration("L0i", 5, 9, body_caps=self.body(101, 1, 1), lines=[5, 6, 7]),
            ["exhaust", "L0i", 0, 1, 10],
        )
        result = checkers.check(self.claim, [trace])
        self.assertEqual((result["checked"], result["refutations"]), (1, []))

    def test_a_statement_on_a_mismatch_refutes(self):
        trace = run(iteration("L0i", 2, 4, body_caps=self.body(100, 1, 2), lines=[4, 5, 6]))
        self.assertEqual(checkers.check(self.claim, [trace])["refutations"][0]["kind"], "busy-on-mismatch")

    def test_project_code_advancing_the_inner_loop_refutes(self):
        trace = run(
            ["resume", 3, 5, "gen", "app/m.py"],
            iteration("L0i", 4, 6, fetch=2, body_caps=self.body(100, 1, 2), lines=[5, 6]),
        )
        self.assertEqual(checkers.check(self.claim, [trace])["refutations"][0]["kind"], "code-on-mismatch")

    def test_a_key_load_running_project_code_refutes(self):
        trace = run(
            ["start", 3, 5, "B.k", "app/m.py", 0, 6],
            iteration("L0i", 2, 4, body_caps={"b": [100, "B", {}]}, lines=[5, 6]),
        )
        self.assertEqual(checkers.check(self.claim, [trace])["refutations"][0]["kind"], "key-runs-code")

    def test_a_different_inner_collection_on_a_later_outer_iteration_refutes(self):
        trace = run(
            iteration("L0i", 2, 3, entry=1, body_caps=self.body(100, 1, 1), lines=[5, 6, 7]),
            ["exhaust", "L0i", 0, 1, 4],
            iteration("L0i", 5, 6, entry=2, body_caps=self.body(100, 2, 1), lines=[5, 6]),
            iteration("L0i", 7, 8, entry=2, body_caps=self.body(101, 2, 3), lines=[5, 6]),
            ["exhaust", "L0i", 0, 2, 9],
        )
        kinds = [r["kind"] for r in checkers.check(self.claim, [trace])["refutations"]]
        self.assertIn("collection-changed", kinds)


class Rewrites(unittest.TestCase):
    def outcome(self, value, status="ok", calls=()):
        return {
            "entry": 0,
            "input": 0,
            "status": status,
            "value": value,
            "events": [fake(1, api, "net", True, d) for api, d in calls],
        }

    def test_the_same_behaviour_confirms(self):
        self.assertEqual(checkers.compare_outcomes([self.outcome(1)], [self.outcome(1)]), [])

    def test_another_result_refutes(self):
        self.assertEqual(checkers.compare_outcomes([self.outcome(1)], [self.outcome(2)])[0][2], "value")

    def test_another_library_call_refutes(self):
        diff = checkers.compare_outcomes([self.outcome(1, calls=[("requests.get", "a")])], [self.outcome(1)])
        self.assertEqual(diff[0][2], "calls")

    def test_a_rewrite_that_never_ends_refutes(self):
        diff = checkers.compare_outcomes([self.outcome(1)], [self.outcome(None, status="timeout")])
        self.assertEqual(diff[0][2:], ("status", "ok", "timeout"))


class Helpers(unittest.TestCase):
    def test_catalog_names_compare_across_constructor_forms(self):
        self.assertEqual(normalize_api("sqlite3.connect(file).cursor().execute"), "sqlite3.cursor.execute")
        self.assertEqual(normalize_api('sqlite3.connect(":memory:").execute'), "sqlite3.connection.execute")
        self.assertEqual(
            normalize_api("psycopg2.extensions.cursor.execute"), "psycopg2.extensions.cursor.execute"
        )

    def test_static_attribute_loads_run_no_code(self):
        class Plain:
            def __init__(self):
                self.k = 1

        class WithProperty:
            @property
            def k(self):
                raise AssertionError("ran")

        self.assertEqual(monitor.static_eval("a.k", {"a": Plain()}), 1)
        self.assertIs(monitor.static_eval("a.k", {"a": WithProperty()}), monitor.UNKNOWN)
        self.assertEqual(monitor.static_eval('a["k"]', {"a": {"k": 2}}), 2)

    def test_snapshots_see_in_place_changes(self):
        items = [1, {2}]
        before = monitor.snapshot(items)
        items[1].add(3)
        self.assertNotEqual(before, monitor.snapshot(items))


class Ins:
    """An instruction as `dis` gives it, for the loop matcher."""

    def __init__(self, offset, opname):
        self.offset = offset
        self.opname = opname


class LoopMatching(unittest.TestCase):
    loop = {"id": "L0", "pos": [4, 13, 4, 18], "alt_pos": [[4, 4, 5, 20]]}
    code = [Ins(8, "GET_ITER"), Ins(10, "FOR_ITER"), Ins(14, "UNPACK_SEQUENCE"), Ins(16, "STORE_FAST"), Ins(18, "STORE_FAST"), Ins(20, "LOAD_FAST")]

    def test_the_iterable_s_position_finds_the_loop(self):
        pos_of = {8: (4, 13, 4, 18), 10: (4, 13, 4, 18), 20: (5, 8, 5, 9)}
        self.assertEqual(monitor.match_loop(self.code, pos_of, self.loop), (8, 10, 20))

    def test_the_statement_s_position_finds_it_on_earlier_cpython_3_12(self):
        pos_of = {8: (4, 4, 5, 20), 10: (4, 4, 5, 20), 20: (5, 8, 5, 9)}
        self.assertEqual(monitor.match_loop(self.code, pos_of, self.loop), (8, 10, 20))

    def test_another_loop_is_not_found(self):
        self.assertIsNone(monitor.match_loop(self.code, {10: (9, 1, 9, 2)}, self.loop))


class Recorder(unittest.TestCase):
    """The recorder on loops of the running interpreter: iterations of a `for` statement, and
    the items of a comprehension whose target and body share one line."""

    SOURCE = (
        "def run(items, rights):\n"
        "    out = []\n"
        "    for x in items:\n"
        "        out.append(x)\n"
        "    for a in items:\n"
        "        out.extend([(a, b) for b in rights if a == b])\n"
        "    return out\n"
    )

    def test_iterations_and_comprehension_items_are_recorded(self):
        import ast
        import importlib
        import tempfile

        import plan

        root = os.path.realpath(tempfile.mkdtemp(prefix="oracle-recorder-"))
        with open(os.path.join(root, "loops_mod.py"), "w", encoding="utf-8") as f:
            f.write(self.SOURCE)
        tree = ast.parse(self.SOURCE)
        fors = [n for n in ast.walk(tree) if isinstance(n, ast.For)]
        comp = next(n for n in ast.walk(tree) if isinstance(n, ast.ListComp))
        loops = [
            plan._loop_entry("Lfor", fors[0], "loops_mod.py"),
            plan._loop_entry("Lcomp", (comp.generators[0], comp), "loops_mod.py", capture_body=["b"], keys=["a", "b"]),
        ]
        recorder = monitor.Recorder(root, {"loops": loops})
        sys.path.insert(0, root)
        try:
            module = importlib.import_module("loops_mod")
            recorder.install()
            recorder.begin_run()
            module.run([1, 2, 3], [2, 5])
            events = recorder.end_run()
        finally:
            recorder.uninstall()
            sys.path.remove(root)
        self.assertEqual(recorder.errors, [])
        plain = [e for e in events if e[0] == "iter" and e[1] == "Lfor"]
        self.assertEqual([e[6] for e in plain], [True, True, True])
        items = [e[8]["b"][2] for e in events if e[0] == "iter" and e[1] == "Lcomp"]
        self.assertEqual(items, [2, 5] * 3)
        keys = [(e[8]["key:a"][0], e[8]["key:b"][0]) for e in events if e[0] == "iter" and e[1] == "Lcomp"]
        self.assertEqual(keys[:2], [(1, 2), (1, 5)])


def _source(text):
    import tempfile

    folder = tempfile.mkdtemp(prefix="oracle-test-")
    os.makedirs(os.path.join(folder, "app"))
    with open(os.path.join(folder, "app", "m.py"), "w", encoding="utf-8") as f:
        f.write(text)
    return Source(folder, "app/m.py")


if __name__ == "__main__":
    unittest.main()
