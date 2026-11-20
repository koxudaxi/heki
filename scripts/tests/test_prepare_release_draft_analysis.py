"""Tests of `prepare_release_draft_analysis.py`: which pull requests skip the Claude analysis."""

from __future__ import annotations

import json
from pathlib import Path

import prepare_release_draft_analysis as route
import pytest
from helpers import outputs, run_script
from validate_release_draft_analysis import validate_reasoning


def record(path: str, previous_path: str | None = None) -> str:
    return json.dumps({"path": path, "previous_path": previous_path})


@pytest.mark.parametrize(
    ("path", "safe"),
    [
        ("tests/difftest/counterexamples.py", True),
        ("crates/heki/tests/cli_check.rs", True),
        ("scripts/tests/test_update_release_draft.py", True),
        ("docs/configuration.md", True),
        ("README.md", True),
        (".github/workflows/ci.yml", True),
        (".github/workflows/release-draft.yml", True),
        (".github/workflows/release-sync.yml", True),
        (".github/workflows/release.yml", False),
        ("scripts/release_check.py", False),
        ("scripts/set_release_versions.py", False),
        ("crates/heki_rules/rule-docs/MUT101.md", False),
        ("crates/heki_effect/src/tests.rs", False),
        ("kb/soundness/builtins.toml", False),
        ("Cargo.lock", False),
        ("pyproject.toml", False),
        ("tests/", False),
        (" tests/x.py", False),
        ("", False),
    ],
)
def test_safe_paths(path: str, safe: bool) -> None:
    assert route.is_safe_path(path) is safe


@pytest.mark.parametrize(
    ("records", "needs_claude"),
    [
        pytest.param([record("tests/a.py"), record("docs/b.md")], False, id="only safe paths"),
        pytest.param(
            [record("docs/new.md", "crates/heki/src/main.rs")],
            True,
            id="renamed from shipped code",
        ),
        pytest.param(
            [record("tests/a.py"), record("crates/heki/src/check.rs")], True, id="one shipped file"
        ),
        pytest.param([], True, id="no records"),
        pytest.param([""], True, id="only a blank line"),
        pytest.param(['{"path": "tests/a.py"}'], True, id="record without previous_path"),
        pytest.param(
            ['{"path": "tests/a.py", "previous_path": null, "x": 1}'], True, id="extra key"
        ),
        pytest.param(["not json"], True, id="malformed record"),
        pytest.param([record("tests/a.py"), ""], False, id="blank lines are not records"),
    ],
)
def test_routing(records: list[str], needs_claude: bool) -> None:
    assert route.requires_claude(records) is needs_claude


def test_a_safe_pull_request_gets_a_valid_analysis_without_claude(tmp_path: Path) -> None:
    files = tmp_path / "files.jsonl"
    files.write_text(record("tests/a.py") + "\n" + record("docs/b.md") + "\n", encoding="utf-8")
    analysis = tmp_path / "analysis.json"
    output = tmp_path / "output"

    result = run_script(
        "prepare_release_draft_analysis.py",
        "--changed-files-path",
        str(files),
        "--analysis-path",
        str(analysis),
        "--github-output-path",
        str(output),
    )

    assert result.returncode == 0, result.stderr
    assert outputs(output) == {"requires_claude": "false"}
    written = json.loads(analysis.read_text(encoding="utf-8"))
    assert written == {
        "has_breaking_changes": False,
        "breaking_changes_content": "",
        "experimental_changes_content": "",
        "reasoning": route.SAFE_REASONING,
    }
    validate_reasoning(written["reasoning"])


def test_an_unreadable_file_list_goes_to_claude(tmp_path: Path) -> None:
    output = tmp_path / "output"
    result = run_script(
        "prepare_release_draft_analysis.py",
        "--changed-files-path",
        str(tmp_path / "missing.jsonl"),
        "--analysis-path",
        str(tmp_path / "analysis.json"),
        "--github-output-path",
        str(output),
    )
    assert result.returncode == 0, result.stderr
    assert outputs(output) == {"requires_claude": "true"}
    assert not (tmp_path / "analysis.json").exists()
