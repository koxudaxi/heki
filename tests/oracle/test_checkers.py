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
    evals=None,
    evals_after=None,
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
        evals or {},
        evals_after or {},
    ]


class Mut115(unittest.TestCase):
    claim = {
        "id": "k0",
        "rule": "MUT115",
        "line_id": "N0",
        "copy_line_id": "Nc0",
        "changes": [
            {"held": '(options)["bases"]', "copy": 'result["bases"]', "changed": 'result["bases"]', "kind": "call"}
        ],
    }

    def copy(self, start, held=7, in_copy=7, act=1):
        return line(
            "Nc0",
            start,
            start + 1,
            act=act,
            evals={'(options)["bases"]': [held, "list", {"list": []}]},
            evals_after={'result["bases"]': [in_copy, "list", {"list": []}]},
        )

    def change(self, start, changed=7, before=(), after=("x",), muts=(), act=1, outcome="ok"):
        snap = lambda items: {"list": list(items)}  # noqa: E731
        return line(
            "N0",
            start,
            start + 1,
            act=act,
            outcome=outcome,
            muts=muts,
            evals={'result["bases"]': [changed, "list", snap(before)]},
            evals_after={'result["bases"]': [changed, "list", snap(after)]},
        )

    def test_a_change_of_the_object_the_original_held_confirms(self):
        result = checkers.check(self.claim, [run(self.copy(1), self.change(3))])
        self.assertEqual((result["checked"], result["refutations"]), (1, []))

    def test_a_method_changing_it_to_equal_contents_confirms(self):
        trace = run(self.copy(1), self.change(3, before=("x",), after=("x",), muts=[[7, "sort", "list"]]))
        self.assertEqual(checkers.check(self.claim, [trace])["refutations"], [])

    def test_another_object_refutes_still_shared(self):
        refutation = checkers.check(self.claim, [run(self.copy(1), self.change(3, changed=9))])["refutations"][0]
        self.assertEqual(refutation["kind"], "not-the-copied-object")
        self.assertTrue(refutation["premise"].startswith("Still shared"))

    def test_a_copy_holding_another_object_refutes_shallow_copy(self):
        refutation = checkers.check(self.claim, [run(self.copy(1, in_copy=8), self.change(3))])["refutations"][0]
        self.assertTrue(refutation["premise"].startswith("Shallow copy"))

    def test_a_line_changing_nothing_refutes_changed_below_the_copy(self):
        trace = run(self.copy(1), self.change(3, before=("x",), after=("x",)))
        refutation = checkers.check(self.claim, [trace])["refutations"][0]
        self.assertTrue(refutation["premise"].startswith("Changed below the copy"))

    def test_only_completed_lines_after_a_copy_in_the_same_activation_count(self):
        trace = run(
            self.change(1),
            self.copy(3, act=2),
            self.change(5),
            self.change(7, changed=9, outcome="exc"),
        )
        self.assertEqual(checkers.check(self.claim, [trace])["checked"], 0)

    def test_a_store_leaving_equal_contents_confirms(self):
        claim = dict(self.claim, changes=[dict(self.claim["changes"][0], kind="store")])
        trace = run(self.copy(1), self.change(3, before=("x",), after=("x",)))
        self.assertEqual(checkers.check(claim, [trace])["refutations"], [])

    def test_an_augmented_assignment_changes_a_container_target_or_stores_into_its_holder(self):
        claim = dict(
            self.claim,
            changes=[
                {"held": '(o)["k"]["n"]', "copy": 'c["k"]["n"]', "changed": 'c["k"]["n"]', "kind": "inplace"},
                {"held": '(o)["k"]', "copy": 'c["k"]', "changed": 'c["k"]', "kind": "store"},
            ],
        )
        copy = line(
            "Nc0",
            1,
            2,
            evals={'(o)["k"]["n"]': [5, "int", 1], '(o)["k"]': [7, "dict", {}]},
            evals_after={'c["k"]["n"]': [5, "int", 1], 'c["k"]': [7, "dict", {}]},
        )
        stored = line("N0", 3, 4, evals={'c["k"]["n"]': [5, "int", 1], 'c["k"]': [7, "dict", {}]})
        self.assertEqual(checkers.check(claim, [run(copy, stored)])["refutations"], [])
        elsewhere = line("N0", 3, 4, evals={'c["k"]["n"]': [5, "int", 1], 'c["k"]': [9, "dict", {}]})
        refutation = checkers.check(claim, [run(copy, elsewhere)])["refutations"][0]
        self.assertEqual(refutation["kind"], "not-the-copied-object")

    def test_paths_heki_cannot_load_without_running_code_are_not_checked(self):
        copy = line("Nc0", 1, 2, evals={'(options)["bases"]': None})
        self.assertEqual(checkers.check(self.claim, [run(copy, self.change(3))])["checked"], 0)


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
        self.assertTrue(refutation["premise"].startswith("Every iteration"))


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


def kept_line(line_id, start, end, before, after, muts=(), act=1, outcome="ok"):
    """A `line` event of a kept-value claim's write, with the facts before and after it."""
    event = line(line_id, start, end, muts=muts, act=act, outcome=outcome)
    return [*event, {"before": before, "after": after}]


def kept_state(kept=70, attr=(80, "a"), cached=True, plain=True, has=True, klass="Parser"):
    return {
        "self": 60,
        "kept": kept,
        "attr": list(attr) if attr is not None else None,
        "class": klass,
        "mro": [klass, "object"],
        "has": has,
        "cached": cached,
        "plain": plain,
    }


class Mut101(unittest.TestCase):
    claim = {
        "id": "k0",
        "rule": "MUT101",
        "line_id": "N0",
        "ret": "R0",
        "writer_ret": "W0",
        "prop": "features",
        "attr": "raw",
        "getter": "Parser.features",
        "rebinds": True,
    }

    def trace(self, before=None, after=None, loads=True, kept_at_return=70, muts=()):
        events = [
            ["start", 1, 2, "Parser.features", "app/m.py", 0, 3],
            ["load", 2, 2, 60, "raw"] if loads else ["load", 2, 2, 60, "other"],
            ["ret", "R0", 2, 3, 70, "str", "str", 60],
            kept_line("N0", 5, 6, before or kept_state(), after or kept_state(attr=(81, "b")), muts=muts),
        ]
        if kept_at_return is not False:
            events.append(["kept_ret", "W0", 1, 9, 60, kept_at_return])
        return run(*events)

    def premises(self, trace):
        return [r["premise"].split(":")[0] for r in checkers.check(self.claim, [trace])["refutations"]]

    def test_a_kept_value_the_getter_computed_from_the_attribute_confirms(self):
        result = checkers.check(self.claim, [self.trace()])
        self.assertEqual((result["checked"], result["refutations"]), (1, []))

    def test_an_instance_holding_no_value_is_no_instance(self):
        result = checkers.check(self.claim, [self.trace(before=kept_state(kept=None))])
        self.assertEqual(result["checked"], 0)

    def test_a_class_without_the_property_is_no_instance(self):
        result = checkers.check(self.claim, [self.trace(before=kept_state(has=False, cached=False))])
        self.assertEqual(result["checked"], 0)

    def test_another_descriptor_under_the_name_refutes_cached_property(self):
        self.assertEqual(self.premises(self.trace(before=kept_state(cached=False))), ["Cached property"])

    def test_a_value_the_getter_did_not_return_refutes_cached_property(self):
        self.assertEqual(self.premises(self.trace(before=kept_state(kept=71))), ["Cached property"])

    def test_a_run_of_the_getter_without_the_load_refutes_reads_on_every_path(self):
        self.assertEqual(self.premises(self.trace(loads=False)), ["Reads on every path"])

    def test_a_store_that_runs_code_refutes_plain_write(self):
        self.assertEqual(self.premises(self.trace(before=kept_state(plain=False))), ["Plain write"])

    def test_an_in_place_change_that_changes_nothing_refutes_plain_write(self):
        claim = dict(self.claim, rebinds=False)
        trace = self.trace(after=kept_state())
        refutations = checkers.check(claim, [trace])["refutations"]
        self.assertEqual([r["premise"].split(":")[0] for r in refutations], ["Plain write"])
        changed = self.trace(after=kept_state(), muts=[[80, "append", "list"]])
        self.assertEqual(checkers.check(claim, [changed])["refutations"], [])

    def test_a_run_of_the_getter_on_another_instance_returning_the_same_object_is_not_its_run(self):
        # `a.features`, then `b.features` returning the same object without loading `a.raw`.
        trace = self.trace()
        trace["events"][3:3] = [
            ["start", 4, 3, "Parser.features", "app/m.py", 0, 4],
            ["load", 3, 4, 61, "raw"],
            ["ret", "R0", 3, 4, 70, "str", "str", 61],
        ]
        self.assertEqual(self.premises(trace), [])

    def test_a_value_gone_at_the_normal_return_refutes_kept_to_the_return(self):
        self.assertEqual(self.premises(self.trace(kept_at_return=None)), ["Kept to the return"])

    def test_a_writer_that_raised_makes_no_claim_about_the_return(self):
        self.assertEqual(self.premises(self.trace(kept_at_return=False)), [])


class Mut111(unittest.TestCase):
    claim = {"id": "k0", "rule": "MUT111", "line_id": "N0", "prop": "features", "getter": "Parser.features"}

    def test_a_read_of_the_getters_property_confirms(self):
        trace = run(kept_line("N0", 1, 2, kept_state(), kept_state()))
        result = checkers.check(self.claim, [trace])
        self.assertEqual((result["checked"], result["refutations"]), (1, []))

    def test_an_object_without_the_cached_property_refutes(self):
        trace = run(kept_line("N0", 1, 2, kept_state(cached=False), kept_state(cached=False)))
        refutation = checkers.check(self.claim, [trace])["refutations"][0]
        self.assertTrue(refutation["premise"].startswith("Runs the writer"))


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


class PremiseTable(unittest.TestCase):
    table = {
        "EFF101": ["Every return", "Every implementation", "Waits"],
        "MUT114": ["Cached", "Keyed", "Container kind", "Same object", "Changed in place"],
    }

    def test_the_generator_strips_every_premise_of_the_table(self):
        import premise_generator

        self.assertEqual(premise_generator.check_table(self.table), ["EFF101", "MUT114"])
        with self.assertRaises(premise_generator.TableError):
            premise_generator.check_table({"EFF101": ["Every return", "A premise no builder strips"]})

    def test_every_program_keeps_all_premises_or_strips_one(self):
        import premise_generator

        seen = set()
        for seed in range(200):
            project = premise_generator.generate(seed, self.table)
            premise = project.premise
            self.assertIn(premise["stripped"], [None, *self.table[premise["rule"]]])
            seen.add((premise["rule"], premise["stripped"]))
        self.assertEqual(len(seen), 4 + 6)

    def report(self, rule):
        return {"rule_code": rule, "certainty": "exact", "path": "app/s0.py", "line": 3, "related": []}

    def test_an_exact_report_with_a_premise_stripped_refutes_it(self):
        import driver

        out = {"refutations": [], "departures": [], "premise": None}
        driver._check_stripped_premise({"rule": "EFF101", "stripped": "Waits"}, [self.report("EFF101")], out)
        self.assertEqual(out["refutations"][0]["kind"], "stripped-premise")
        self.assertEqual(out["premise"], {"rule": "EFF101", "stripped": "Waits", "exact": 1})

    def test_a_relevance_premise_is_a_departure_and_a_base_program_refutes_nothing(self):
        import driver

        out = {"refutations": [], "departures": [], "premise": None}
        driver._check_stripped_premise({"rule": "MUT114", "stripped": "Keyed"}, [self.report("MUT114")], out)
        self.assertEqual((out["refutations"], out["departures"][0]["kind"]), ([], "relevance-premise"))
        out = {"refutations": [], "departures": [], "premise": None}
        driver._check_stripped_premise({"rule": "EFF101", "stripped": None}, [self.report("EFF101")], out)
        self.assertEqual(out["refutations"], [])

    def dumped(self, rule, certainty, *premises, qualname="handle_s0_1"):
        """A report as `heki dump-open-premises` prints it."""
        open_premises = [
            {"premise": p, "cause": "c", "closer": "none", "at": None, "detail": None} for p in premises
        ]
        return {
            "rule": rule,
            "certainty": certainty,
            "path": "app/s0.py",
            "line": 3,
            "column": 5,
            "qualname": qualname,
            "open_premises": open_premises,
        }

    def test_a_heuristic_report_records_the_stripped_premise_among_its_open_ones(self):
        import driver

        out = {"unrecorded": [], "recorded_checked": 0}
        reports = [
            self.dumped("EFF111", "heuristic", "Every implementation", "Waits"),
            # Exact reports, and reports of other rules, record nothing to check.
            self.dumped("EFF101", "exact"),
            self.dumped("EFF113", "heuristic", "Origin"),
            # A report in a helper is about another claim.
            self.dumped("EFF111", "heuristic", "Origin", qualname="step_s0_2"),
        ]
        stripped = {"rule": "EFF101", "stripped": "Waits"}
        driver._check_open_premises(stripped, "EFF111", reports, {"handle_s0_1"}, out)
        self.assertEqual((out["unrecorded"], out["recorded_checked"]), ([], 1))

    def test_a_heuristic_report_without_the_stripped_premise_is_unrecorded(self):
        import driver

        out = {"unrecorded": [], "recorded_checked": 0}
        reports = [self.dumped("EFF111", "heuristic", "Every implementation")]
        stripped = {"rule": "EFF101", "stripped": "Every return"}
        driver._check_open_premises(stripped, "EFF111", reports, {"handle_s0_1"}, out)
        finding = out["unrecorded"][0]
        self.assertEqual((finding["rule"], finding["kind"]), ("EFF101", "stripped-premise-not-recorded"))
        self.assertEqual(finding["detail"]["stripped"], "Every return")

    def test_a_rule_whose_heuristic_tier_keeps_its_code_checks_its_own_reports(self):
        import driver
        import premise_generator

        code = premise_generator.OPEN_PREMISE_CODES["OPT101"]
        self.assertEqual(code, "OPT101")
        out = {"unrecorded": [], "recorded_checked": 0}
        reports = [
            self.dumped("OPT101", "heuristic", "Rewrite proven safe"),
            self.dumped("OPT101", "exact"),
            self.dumped("OPT101", "heuristic"),
        ]
        stripped = {"rule": "OPT101", "stripped": "Rewrite proven safe"}
        driver._check_open_premises(stripped, code, reports, {"handle_s0_1"}, out)
        self.assertEqual(out["recorded_checked"], 2)
        self.assertEqual([f["detail"]["recorded"] for f in out["unrecorded"]], [[]])

    def test_every_rule_recording_open_premises_has_programs_with_a_premise_stripped(self):
        import premise_generator

        for rule in premise_generator.OPEN_PREMISE_CODES:
            self.assertIn(rule, premise_generator.STRIPS, rule)


class Minimizing(unittest.TestCase):
    """A refutation that stands on how the program was built keeps its program text."""

    files = {
        "app/__init__.py": "",
        "app/s0.py": "def f(flag):\n    if flag:\n        return None\n    return 1\n",
    }
    meta = {"entries": [{"module": "app.s0", "func": "f", "inputs": [{"args": [True]}, {"args": [False]}]}]}

    def minimizer(self):
        import minimize

        class Always(minimize.Minimizer):
            def holds(self, files, meta):
                return True

        return Always(None, "EFF101", "x", budget=30)

    def test_a_stripped_premise_keeps_the_program_and_narrows_the_inputs(self):
        finding = {"kind": "stripped-premise", "entry": None, "input": None}
        files, meta = self.minimizer().run(dict(self.files), self.meta, finding)
        self.assertEqual(files, self.files)
        self.assertEqual(meta["entries"][0]["inputs"], [{"args": [True]}])

    def test_a_run_time_refutation_is_reduced(self):
        finding = {"kind": "no-wait", "entry": 0, "input": 1}
        files, _meta = self.minimizer().run(dict(self.files), self.meta, finding)
        self.assertNotEqual(files, self.files)


class Ins:
    """An instruction as `dis` gives it, for the loop matcher."""

    def __init__(self, offset, opname):
        self.offset = offset
        self.opname = opname


class LoopMatching(unittest.TestCase):
    loop = {"id": "L0", "pos": [4, 13, 4, 18], "alt_pos": [[4, 4, 5, 20]]}
    code = [
        Ins(8, "GET_ITER"),
        Ins(10, "FOR_ITER"),
        Ins(14, "UNPACK_SEQUENCE"),
        Ins(16, "STORE_FAST"),
        Ins(18, "STORE_FAST"),
        Ins(20, "LOAD_FAST"),
    ]

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
            plan._loop_entry(
                "Lcomp", (comp.generators[0], comp), "loops_mod.py", capture_body=["b"], keys=["a", "b"]
            ),
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


class RecordedCopies(unittest.TestCase):
    """The plan of a `MUT115` claim and the recorder on a real copy: the change of what the
    copied value holds confirms the claim, a rebinding in the copy refutes it."""

    SOURCE = (
        "def with_base(options, base, rebind):\n"
        "    result = options.copy()\n"
        "    if rebind:\n"
        "        result[\"bases\"] = []\n"
        "    result[\"bases\"].append(base)\n"
        "    result[\"bases\"] += [base]\n"
        "    return result\n"
    )

    def test_a_change_below_the_copy_is_checked_against_the_original(self):
        import importlib
        import tempfile

        import plan

        root = os.path.realpath(tempfile.mkdtemp(prefix="oracle-copies-"))
        with open(os.path.join(root, "copies_mod.py"), "w", encoding="utf-8") as f:
            f.write(self.SOURCE)
        def report(text, line):
            start = self.SOURCE.index(text)
            return {
                "rule_code": "MUT115",
                "certainty": "exact",
                "path": "copies_mod.py",
                "line": line,
                "column": 5,
                "span": {"start": start, "end": self.SOURCE.index("\n", start)},
                "message": "shares",
                "target": "result",
                "related": [{"path": "copies_mod.py", "line": 2, "column": 14, "message": "copy"}],
            }

        planner = plan.build(
            root, [report("result[\"bases\"].append", 5), report("result[\"bases\"] +=", 6)]
        )
        self.assertEqual(len(planner.claims), 2, planner.unmapped)
        claim, augmented = planner.claims
        self.assertEqual(claim["changes"][0]["changed"], 'result["bases"]')
        self.assertEqual(claim["changes"][0]["held"], '(options)["bases"]')
        self.assertEqual(augmented["changes"][0]["kind"], "inplace")
        recorder = monitor.Recorder(root, planner.plan)
        sys.path.insert(0, root)
        try:
            module = importlib.import_module("copies_mod")
            recorder.install()
            runs = []
            for rebind in (False, True):
                recorder.begin_run()
                module.with_base({"bases": ["a"]}, "b", rebind)
                runs.append(run(*recorder.end_run()))
        finally:
            recorder.uninstall()
            sys.path.remove(root)
        self.assertEqual(recorder.errors, [])
        kept = checkers.check(claim, runs[:1])
        self.assertEqual((kept["checked"], kept["refutations"]), (1, []))
        rebound = checkers.check(claim, runs[1:])
        self.assertEqual(rebound["refutations"][0]["kind"], "not-the-copied-object")
        added = checkers.check(augmented, runs[:1])
        self.assertEqual((added["checked"], added["refutations"]), (1, []))


def _source(text):
    import tempfile

    folder = tempfile.mkdtemp(prefix="oracle-test-")
    os.makedirs(os.path.join(folder, "app"))
    with open(os.path.join(folder, "app", "m.py"), "w", encoding="utf-8") as f:
        f.write(text)
    return Source(folder, "app/m.py")


if __name__ == "__main__":
    unittest.main()
