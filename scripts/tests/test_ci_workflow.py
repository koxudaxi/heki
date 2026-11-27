"""Tests of `.github/workflows/ci.yml`: the class of a change, the jobs it runs, and the gate."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest
from helpers import ROOT, Repository

yaml = pytest.importorskip("yaml")

WORKFLOW_PATH = ROOT / ".github/workflows/ci.yml"
DOCUMENTATION = [
    "docs/index.md",
    "docs/rules/index.md",
    "docs/cli-reference.md",
    "docs/llms.txt",
    "docs/llms-full.txt",
    "docs/assets/heki-icon.png",
    "docs/stylesheets/heki.css",
    "README.md",
    "ARCHITECTURE.md",
    "AGENTS.md",
    "CODE_OF_CONDUCT.md",
    "SECURITY.md",
    "zensical.toml",
    ".github/CONTRIBUTING.md",
    ".github/pull_request_template.md",
    ".github/ISSUE_TEMPLATE/bug_report.md",
    ".github/ISSUE_TEMPLATE/config.yml",
]
CODE = [
    # `e2e_json_schemas` compares the schemas with what the binary writes.
    "docs/schemas/check.json",
    "crates/heki/src/main.rs",
    # The binary embeds the rule documentation and the Agent Skill.
    "crates/heki_rules/rule-docs/MUT101.md",
    "skills/heki/SKILL.md",
    ".claude-plugin/marketplace.json",
    "pyproject.toml",
    "Cargo.lock",
    "scripts/build_docs.py",
    ".github/workflows/docs.yml",
    ".github/dependabot.yml",
    "LICENSE",
    "tests/corpus/loop_facts.py",
    "README.md.orig",
    "READMEXmd",
    "crates/heki/docs/index.md",
]


@pytest.fixture(scope="module")
def workflow() -> dict[str, Any]:
    return yaml.safe_load(WORKFLOW_PATH.read_text(encoding="utf-8"))


@pytest.fixture
def base(repository: Repository) -> Repository:
    """A repository whose last commit is the base branch of the change the test commits."""
    repository.commit(
        "base", {"README.md": "base\n", "skills/heki/references/adopt.md": "skill\n" * 20}
    )
    return repository


def literal_env(step: dict[str, Any]) -> dict[str, str]:
    """The step's environment without the values GitHub fills in from expressions."""
    return {name: value for name, value in step["env"].items() if not value.startswith("${{")}


def classify(
    workflow: dict[str, Any],
    repository: Repository,
    tmp_path: Path,
    *,
    event: str = "pull_request",
    base_ref: str = "main",
    stack_base: str = "",
) -> str:
    """Run the `changes` job's step on the repository's last commit as GitHub's bash runs it."""
    step = next(s for s in workflow["jobs"]["changes"]["steps"] if s.get("id") == "files")
    output = tmp_path / "github-output"
    output.write_text("", encoding="utf-8")
    env = {
        **os.environ,
        **literal_env(step),
        "EVENT": event,
        "BASE": base_ref,
        "STACK_BASE": stack_base,
        "GITHUB_OUTPUT": str(output),
    }
    subprocess.run(
        ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", step["run"]],
        cwd=repository.path,
        env=env,
        check=True,
        capture_output=True,
        text=True,
    )
    outputs = dict(line.split("=", 1) for line in output.read_text(encoding="utf-8").splitlines())
    return outputs["class"]


def change(repository: Repository, *paths: str) -> None:
    repository.commit("change", {path: f"changed {path}\n" for path in paths})


@pytest.mark.parametrize("path", DOCUMENTATION)
def test_documentation_alone_is_docs(
    workflow: dict[str, Any], base: Repository, tmp_path: Path, path: str
) -> None:
    change(base, path)
    assert classify(workflow, base, tmp_path) == "docs"


@pytest.mark.parametrize("path", CODE)
def test_any_other_file_is_code(
    workflow: dict[str, Any], base: Repository, tmp_path: Path, path: str
) -> None:
    change(base, "docs/index.md", path)
    assert classify(workflow, base, tmp_path) == "code"


@pytest.mark.parametrize(
    ("paths", "expected"),
    [
        pytest.param(["CHANGELOG.md"], "changelog", id="changelog alone"),
        pytest.param(
            ["CHANGELOG.md", "docs/index.md", "README.md"], "docs", id="changelog and docs"
        ),
        pytest.param(["CHANGELOG.md", "crates/heki/src/main.rs"], "code", id="changelog and code"),
        pytest.param(["docs/index.md", "docs/schemas/check.json"], "code", id="docs and a schema"),
        pytest.param(["docs/schemas/check.json"], "code", id="a schema alone"),
    ],
)
def test_mixed_changes(
    workflow: dict[str, Any], base: Repository, tmp_path: Path, paths: list[str], expected: str
) -> None:
    change(base, *paths)
    assert classify(workflow, base, tmp_path) == expected


def test_a_file_moved_from_the_code_into_the_docs_is_code(
    workflow: dict[str, Any], base: Repository, tmp_path: Path
) -> None:
    moved = base.path / "skills/heki/references/adopt.md"
    text = moved.read_text(encoding="utf-8")
    moved.unlink()
    base.commit("move", {"docs/adopt.md": text})
    assert base.git("diff", "--name-only", "HEAD^1", "HEAD") == "docs/adopt.md"
    assert classify(workflow, base, tmp_path) == "code"


@pytest.mark.parametrize("event", ["push", "release", "workflow_dispatch"])
def test_every_run_but_a_pull_request_is_code(
    workflow: dict[str, Any], base: Repository, tmp_path: Path, event: str
) -> None:
    change(base, "docs/index.md")
    assert classify(workflow, base, tmp_path, event=event) == "code"


@pytest.mark.parametrize("path", ["docs/index.md", "CHANGELOG.md"])
def test_a_pull_request_above_the_bottom_of_a_stack_is_code(
    workflow: dict[str, Any], base: Repository, tmp_path: Path, path: str
) -> None:
    change(base, path)
    assert classify(workflow, base, tmp_path, base_ref="main", stack_base="main") != "code"
    assert classify(workflow, base, tmp_path, base_ref="lower", stack_base="main") == "code"


def gate_groups(workflow: dict[str, Any]) -> dict[str, set[str]]:
    env = workflow["jobs"]["gate"]["steps"][0]["env"]
    return {group: set(env[group].split()) for group in ("ALWAYS_ON", "STACK_ONLY", "DOCS_ONLY")}


def test_each_job_runs_as_the_gate_expects(workflow: dict[str, Any]) -> None:
    jobs = workflow["jobs"]
    groups = gate_groups(workflow)
    stack_full = jobs["release-scripts"]["if"]
    assert set(jobs["gate"]["needs"]) == set(jobs) - {"gate"}
    for name in jobs["gate"]["needs"]:
        condition = jobs[name].get("if")
        if name in groups["ALWAYS_ON"]:
            assert condition is None, name
        elif name in groups["STACK_ONLY"]:
            assert condition == stack_full, name
        elif name in groups["DOCS_ONLY"]:
            assert condition == f"needs.changes.outputs.class == 'docs' && ({stack_full})", name
        else:
            assert condition == f"needs.changes.outputs.class == 'code' && ({stack_full})", name


def run_gate(workflow: dict[str, Any], ran: set[str], *, stack_full: bool, change: str) -> int:
    """The gate's exit code when the jobs in `ran` succeeded and every other job was skipped."""
    job = workflow["jobs"]["gate"]
    step = job["steps"][0]
    needs = {
        name: {"result": "success" if name in ran else "skipped", "outputs": {}}
        for name in job["needs"]
    }
    env = {
        **os.environ,
        **literal_env(step),
        "NEEDS": json.dumps(needs),
        "STACK_FULL": "true" if stack_full else "false",
        "CLASS": change,
    }
    return subprocess.run(
        [sys.executable, "-c", step["run"]], env=env, capture_output=True, text=True, check=False
    ).returncode


def test_the_gate(workflow: dict[str, Any]) -> None:
    groups = gate_groups(workflow)
    every = set(workflow["jobs"]["gate"]["needs"])
    always = groups["ALWAYS_ON"]
    stack = always | groups["STACK_ONLY"]
    docs = stack | groups["DOCS_ONLY"]
    code = every - groups["DOCS_ONLY"]

    assert run_gate(workflow, code, stack_full=True, change="code") == 0
    assert run_gate(workflow, every, stack_full=True, change="code") == 1
    assert run_gate(workflow, code - {"clippy"}, stack_full=True, change="code") == 1

    assert run_gate(workflow, docs, stack_full=True, change="docs") == 0
    assert run_gate(workflow, stack, stack_full=True, change="docs") == 1
    assert run_gate(workflow, docs | {"test-linux"}, stack_full=True, change="docs") == 1

    assert run_gate(workflow, stack, stack_full=True, change="changelog") == 0
    assert run_gate(workflow, docs, stack_full=True, change="changelog") == 1

    for change in ("code", "docs", "changelog"):
        assert run_gate(workflow, always, stack_full=False, change=change) == 0
        assert run_gate(workflow, stack, stack_full=False, change=change) == 1

    # A failed `changes` job leaves no class: nothing passes.
    assert run_gate(workflow, always - {"changes"}, stack_full=True, change="") == 1
