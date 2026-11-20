"""Tests of `release_draft_common.py`: the JSON records `gh api --jq` prints, and the published versions."""

from __future__ import annotations

import json

import pytest
from release_draft_common import json_lines, published_versions


def test_only_lf_ends_a_record() -> None:
    # `gh api --jq` leaves U+0085 unescaped inside a string; `str.splitlines` would split there.
    records = [
        {"body": "Line\u0085next"},
        {"body": "Form\x0cfeed and   separator"},
        {"title": "Fixes #3"},
    ]
    text = "".join(json.dumps(record, ensure_ascii=False) + "\n" for record in records)

    assert "\u0085" in text
    assert json_lines(text) == records
    assert json_lines(text.replace("\n", "\r\n")) == records
    assert json_lines("\n\n" + text + "\n") == records


def test_a_broken_record_stops_the_step() -> None:
    with pytest.raises(SystemExit):
        json_lines('{"body": "split\n here"}\n')


def release(tag: str, *, draft: bool = False, prerelease: bool = False) -> dict:
    return {"tag_name": tag, "draft": draft, "prerelease": prerelease}


def test_published_versions_keep_every_tag_newest_first() -> None:
    releases = [
        release("0.2.0"),
        release("v0.1.0"),
        release("v0.2.0"),
        release("v0.3.0", draft=True),
        release("v0.2.1rc1", prerelease=True),
        release("nightly"),
        {"tag_name": "v0.9.0", "draft": None, "prerelease": False},
    ]

    assert published_versions(releases) == [
        ((0, 2, 0), "v0.2.0"),
        ((0, 2, 0), "0.2.0"),
        ((0, 1, 0), "v0.1.0"),
    ]
    assert published_versions([]) == []
