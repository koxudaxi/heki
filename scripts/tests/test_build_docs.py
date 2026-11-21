"""Tests of `build_docs.py`'s checks of the rule certainties: every exact rule has a
certification in `docs/precision.md`, and every rule says what a report states.

The other checks run end to end in `crates/heki/tests/e2e_experimental.rs`; no rule is exact
yet, so these are tested on rules given here.
"""

from __future__ import annotations

import build_docs
import pytest

ROW = "| `{code}` | 0.3.0 | {census} / {census_false} | {holdout} / {holdout_false} |"


def certification(code: str, census_false: int, holdout_false: int) -> dict:
    match = build_docs.CERTIFICATION.match(
        ROW.format(
            code=code, census=240, census_false=census_false, holdout=31, holdout_false=holdout_false
        )
    )
    assert match, code
    return match.groupdict()


@pytest.mark.parametrize(
    ("census_false", "holdout_false", "stale"),
    [
        (0, 0, []),
        (1, 0, ["OPT110: 1 false claim(s) in the census and 0 in the holdout run (exact needs none)"]),
        (0, 2, ["OPT110: 0 false claim(s) in the census and 2 in the holdout run (exact needs none)"]),
    ],
)
def test_an_exact_rule_needs_no_false_claim_in_its_census_and_holdout_run(
    census_false: int, holdout_false: int, stale: list[str]
) -> None:
    rules = [{"code": "OPT110", "certainty": "exact"}]
    certifications = {"OPT110": certification("OPT110", census_false, holdout_false)}
    assert build_docs.uncertified_exact_rules(rules, certifications) == stale


def test_an_exact_rule_without_a_row_fails_and_a_heuristic_one_needs_none() -> None:
    rules = [
        {"code": "OPT110", "certainty": "exact"},
        {"code": "EFF102", "certainty": "heuristic"},
    ]
    assert build_docs.uncertified_exact_rules(rules, {}) == ["OPT110: no row"]


def test_every_rule_says_what_a_report_states() -> None:
    rules = [
        {"code": "EFF101", "doc": "## What it does\nx\n\n## What a report states\ny\n"},
        {"code": "EFF102", "doc": "## What it does\nx\n"},
    ]
    assert build_docs.rules_without_claims(rules) == [
        "EFF102: no `## What a report states` section"
    ]


def test_the_tables_of_the_real_page_are_read() -> None:
    rows = build_docs.precision_rows(build_docs.MEASUREMENT)
    assert rows["EFF101"]["sample"] == "50" and rows["EFF101"]["f"] == "9"
    assert rows["EFF101"]["right"] == "82%"
    certified = build_docs.precision_rows(build_docs.CERTIFICATION)
    assert certified["OPT103"]["census"] == "128" and certified["OPT103"]["holdout_false"] == "0"
    assert certified["EFF101"]["census"] == "287" and certified["EFF101"]["holdout_false"] == "0"
