"""Tests of `.github/workflows/release-draft.yml`: triggers, write gating, pins, and the trusted checkout."""

from __future__ import annotations

import os
import re
import shlex
import subprocess
from pathlib import Path
from typing import Any

import pytest
from helpers import ROOT, Repository
from release_draft_common import ANALYZED_LABEL, BREAKING_LABEL

yaml = pytest.importorskip("yaml")

WORKFLOW_PATH = ROOT / ".github/workflows/release-draft.yml"
WRITE_GUARD = "github.ref == 'refs/heads/main' && !inputs.dry_run"
PINNED = re.compile(r"[\w.-]+/[\w./-]+@[0-9a-f]{40}")


@pytest.fixture(scope="module")
def workflow() -> dict[str, Any]:
    return yaml.safe_load(WORKFLOW_PATH.read_text(encoding="utf-8"))


def all_steps(workflow: dict[str, Any]) -> list[tuple[str, dict[str, Any]]]:
    return [(name, step) for name, job in workflow["jobs"].items() for step in job["steps"]]


def test_triggers(workflow: dict[str, Any]) -> None:
    # PyYAML reads the key `on` as True.
    triggers = workflow[True]
    assert triggers["push"] == {"branches": ["main"]}
    assert "pull_request_target" not in triggers
    inputs = triggers["workflow_dispatch"]["inputs"]
    assert inputs["pr_number"] == {
        "description": "Merged pull request to analyze",
        "type": "string",
        "required": True,
    }
    assert inputs["dry_run"]["type"] == "boolean"
    assert inputs["dry_run"]["default"] is False
    assert workflow["concurrency"]["cancel-in-progress"] is False
    assert workflow["concurrency"]["queue"] == "max"


def test_every_write_is_skipped_off_main_and_in_a_dry_run(workflow: dict[str, Any]) -> None:
    writes = [
        (job, step["name"])
        for job, step in all_steps(workflow)
        if re.search(r"--method\s+(POST|PATCH|PUT|DELETE)", step.get("run", ""))
    ]
    assert writes == [
        ("update-draft", "Create or update the draft release"),
        ("update-draft", "Label breaking pull requests"),
        ("update-draft", "Comment the analysis on each pull request"),
        ("update-draft", "Mark the pull requests analyzed"),
    ]
    for job, step in all_steps(workflow):
        if (job, step.get("name")) in writes:
            assert step["if"].startswith(WRITE_GUARD), step["name"]
        else:
            assert not re.search(r"\bgh\s+(release|pr|issue|label)\b", step.get("run", "")), (
                step.get("name")
            )


def test_only_the_update_job_may_write(workflow: dict[str, Any]) -> None:
    assert workflow["permissions"] == {}
    permissions = {name: job["permissions"] for name, job in workflow["jobs"].items()}
    assert permissions == {
        "resolve-pr": {"contents": "read", "pull-requests": "read"},
        "analyze": {"contents": "read", "pull-requests": "read"},
        "update-draft": {"contents": "write", "pull-requests": "write"},
    }


def test_labels_match_the_scripts(workflow: dict[str, Any]) -> None:
    steps = {step["name"]: step for _, step in all_steps(workflow)}
    assert f"'labels[]={BREAKING_LABEL}'" in steps["Label breaking pull requests"]["run"]
    assert f"'labels[]={ANALYZED_LABEL}'" in steps["Mark the pull requests analyzed"]["run"]


def test_every_action_is_pinned_by_commit(workflow: dict[str, Any]) -> None:
    uses = [step["uses"] for _, step in all_steps(workflow) if "uses" in step]
    assert uses and all(PINNED.fullmatch(use) for use in uses), uses


def test_the_token_reaches_only_the_claude_step(workflow: dict[str, Any]) -> None:
    text = WORKFLOW_PATH.read_text(encoding="utf-8")
    assert text.count("secrets.") == 1
    claude = next(step for _, step in all_steps(workflow) if step.get("id") == "claude")
    assert claude["with"]["claude_code_oauth_token"] == "${{ secrets.CLAUDE_CODE_OAUTH_TOKEN }}"
    assert claude["uses"].startswith("anthropics/claude-code-action/base-action@")
    assert claude["with"]["show_full_output"] is False


def test_claude_can_only_read_the_prepared_artifacts(workflow: dict[str, Any]) -> None:
    claude = next(step for _, step in all_steps(workflow) if step.get("id") == "claude")
    arguments = shlex.split(claude["with"]["claude_args"])
    option = {
        arguments[i]: arguments[i + 1]
        for i in range(0, len(arguments) - 1)
        if arguments[i].startswith("--")
    }
    assert option["--model"] == "claude-opus-5-5"
    assert option["--tools"] == "Read"
    assert option["--allowedTools"] == "Read(/${{ steps.pr-diff.outputs.claude_working_dir }}/**)"
    assert option["--permission-mode"] == "dontAsk"
    assert option["--setting-sources"] == "user"


def test_every_job_checks_out_and_verifies_the_workflow_commit(workflow: dict[str, Any]) -> None:
    verifications = set()
    for name, job in workflow["jobs"].items():
        checkout, verify = job["steps"][0], job["steps"][1]
        assert checkout["uses"].startswith("actions/checkout@"), name
        assert checkout["with"]["persist-credentials"] is False, name
        assert "ref" not in checkout["with"], name
        assert verify["name"] == "Verify trusted checkout", name
        assert verify["env"] == {"TRUSTED_SHA": "${{ github.workflow_sha }}"}, name
        verifications.add(verify["run"])
    assert len(verifications) == 1, "every job runs the same verification"


def test_untrusted_values_reach_scripts_only_through_the_environment(
    workflow: dict[str, Any],
) -> None:
    for _, step in all_steps(workflow):
        expressions = re.findall(r"\$\{\{(.*?)\}\}", step.get("run", ""))
        assert expressions == [], (step["name"], expressions)


@pytest.mark.parametrize(
    ("case", "expected"),
    [("workflow", 0), ("other", 1), ("missing", 1), ("not a repository", 1)],
)
def test_the_verification_rejects_other_commits(
    tmp_path: Path, workflow: dict[str, Any], case: str, expected: int
) -> None:
    repository = Repository(tmp_path / "repository")
    other = repository.commit("other")
    head = repository.commit("workflow")
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    trusted, cwd = {
        "workflow": (head, repository.path),
        "other": (other, repository.path),
        "missing": ("", repository.path),
        "not a repository": (head, elsewhere),
    }[case]
    step = workflow["jobs"]["analyze"]["steps"][1]

    result = subprocess.run(
        ["bash", "-c", step["run"]],
        cwd=cwd,
        env={**os.environ, "TRUSTED_SHA": trusted, "GIT_CEILING_DIRECTORIES": str(tmp_path)},
        capture_output=True,
        check=False,
        text=True,
    )

    assert result.returncode == expected, result.stderr
