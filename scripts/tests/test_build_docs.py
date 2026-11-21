"""Tests of `build_docs.py`'s check that every exact rule has its measurement in `docs/precision.md`.

The other checks run end to end in `crates/heki/tests/e2e_experimental.rs`; no rule is exact
yet, so this one is tested on rules given here.
"""

from __future__ import annotations

import build_docs
import pytest

ROW = "| `{code}` | {certainty} | round 4, 2026-12-01 | {n} | 0 / 10 / {b} / {f} | 99% | 95-100% | 10% |"


def measurement(code: str, certainty: str, n: int, f: int) -> dict:
    match = build_docs.MEASUREMENT.match(
        ROW.format(code=code, certainty=certainty, n=n, b=n - 10 - f, f=f)
    )
    assert match, code
    return match.groupdict()


@pytest.mark.parametrize(
    ("n", "f", "stale"),
    [
        (100, 0, []),
        (250, 0, []),
        (99, 0, ["OPT110: 99 sampled reports with 0 wrong (exact needs at least 100 and none wrong)"]),
        (100, 1, ["OPT110: 100 sampled reports with 1 wrong (exact needs at least 100 and none wrong)"]),
    ],
)
def test_an_exact_rule_needs_100_sampled_reports_and_none_wrong(
    n: int, f: int, stale: list[str]
) -> None:
    rules = [{"code": "OPT110", "certainty": "exact"}]
    measurements = {"OPT110": measurement("OPT110", "exact", n, f)}
    assert build_docs.unmeasured_exact_rules(rules, measurements) == stale


def test_an_exact_rule_without_a_row_fails_and_a_heuristic_one_needs_none() -> None:
    rules = [
        {"code": "OPT110", "certainty": "exact"},
        {"code": "EFF102", "certainty": "heuristic"},
        {"code": "EFF103", "certainty": "heuristic"},
    ]
    measurements = {"EFF103": measurement("EFF103", "heuristic", 50, 13)}
    assert build_docs.unmeasured_exact_rules(rules, measurements) == ["OPT110: no row"]


def test_the_measurements_of_the_real_page_are_read() -> None:
    rows = build_docs.precision_measurements()
    assert rows["EFF101"]["sample"] == "50" and rows["EFF101"]["f"] == "9"
    assert rows["EFF101"]["right"] == "82%"
