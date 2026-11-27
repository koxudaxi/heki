"""Tests of `.github/workflows/ci.yml`: the class of a change, the jobs it runs, and the gate."""

from __future__ import annotations

import json
import os
import re
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


DOCS_WORKFLOW_PATH = ROOT / ".github/workflows/docs.yml"


@pytest.fixture(scope="module")
def docs_workflow() -> dict[str, Any]:
    return yaml.safe_load(DOCS_WORKFLOW_PATH.read_text(encoding="utf-8"))


def uses(step: dict[str, Any], action: str) -> bool:
    return step.get("uses", "").split("@")[0] == action


def flat(expression: str) -> str:
    return " ".join(expression.replace("(", " ( ").replace(")", " ) ").split())


def test_a_production_deploy_builds_and_checks_nothing(docs_workflow: dict[str, Any]) -> None:
    """A release or a manual run on main deploys pages the pull request and main checked: no
    toolchain, no cache, no build, no generated-pages check."""
    build = docs_workflow["jobs"]["build"]
    production = flat(build["env"]["PRODUCTION"].removeprefix("${{").removesuffix("}}"))
    assert production == flat(
        "github.event_name == 'release'"
        " || (github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main')"
    )
    assert production in flat(docs_workflow["jobs"]["production"]["if"])
    rust_steps = [
        step
        for step in build["steps"]
        if any(
            uses(step, action)
            for action in (
                "dtolnay/rust-toolchain",
                "Swatinem/rust-cache",
                "actions/cache/restore",
                "actions/cache/save",
            )
        )
        or "cargo" in step.get("run", "")
        or "--check" in step.get("run", "")
    ]
    assert len(rust_steps) == 7
    for step in rust_steps:
        condition = step["if"]
        # A push is never a production deploy.
        assert condition.startswith(("env.PRODUCTION != 'true'", "github.event_name == 'push'"))
    check = next(s for s in build["steps"] if "build_docs.py" in s.get("run", ""))
    restore = next(s for s in build["steps"] if uses(s, "actions/cache/restore"))
    assert check["run"].split("--heki ")[1].split()[0] == restore["with"]["path"]


def cache_steps(workflow: dict[str, Any]) -> list[dict[str, Any]]:
    return [
        step
        for job in workflow["jobs"].values()
        for step in job.get("steps", [])
        if step.get("uses", "").startswith(("actions/cache", "Swatinem/rust-cache"))
    ]


def test_only_a_push_to_main_writes_the_heki_caches(
    workflow: dict[str, Any], docs_workflow: dict[str, Any]
) -> None:
    for each in (workflow, docs_workflow):
        # `on:` is YAML 1.1's `True`.
        assert each[True]["push"] == {"branches": ["main"]}
        for step in cache_steps(each):
            # `actions/cache` itself saves in its post step, on every event.
            assert not uses(step, "actions/cache"), step
            if uses(step, "actions/cache/save"):
                assert "github.event_name == 'push'" in step["if"], step
    for step in cache_steps(docs_workflow):
        if uses(step, "Swatinem/rust-cache"):
            assert step["with"]["save-if"] == "${{ github.event_name == 'push' }}"


def cache_key_files(step: dict[str, Any]) -> list[str]:
    calls = re.findall(r"hashFiles\(([^)]*)\)", step["with"]["key"])
    assert len(calls) == 1, step
    return re.findall(r"'([^']*)'", calls[0])


def heki_cache_keys(workflow: dict[str, Any], docs_workflow: dict[str, Any]) -> dict[str, list]:
    """The files each cache of heki's build hashes, by the workflow that saves it."""
    def steps(job: dict[str, Any], action: str) -> list[dict[str, Any]]:
        return [step for step in job["steps"] if uses(step, action)]

    archive = steps(workflow["jobs"]["test-build-linux"], "actions/cache/save")
    restore = steps(workflow["jobs"]["docs-tests"], "actions/cache/restore")
    binary = steps(docs_workflow["jobs"]["build"], "actions/cache/restore")
    assert len(archive) == len(restore) == len(binary) == 1
    assert archive[0]["with"]["key"] == restore[0]["with"]["key"]
    return {"ci.yml": cache_key_files(archive[0]), "docs.yml": cache_key_files(binary[0])}


def matches(path: str, patterns: list[str]) -> bool:
    return any(
        path.startswith(pattern.removesuffix("**")) if pattern.endswith("/**") else path == pattern
        for pattern in patterns
    )


def tracked_files() -> list[str]:
    out = subprocess.run(
        ["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout
    return [path for path in out.split("\0") if path]


def embedded_files() -> set[str]:
    """Every file a crate embeds with `include_str!` or `include_bytes!`."""
    found = set()
    for path in tracked_files():
        if not (path.startswith("crates/") and path.endswith(".rs")):
            continue
        text = (ROOT / path).read_text(encoding="utf-8")
        literal = re.findall(r'include_(?:str|bytes)!\(\s*"([^"]+)"\s*\)', text)
        # A path built from `concat!` or `env!` would escape this check.
        assert len(literal) == len(re.findall(r"include_(?:str|bytes)!\(", text)), path
        found |= {os.path.normpath(os.path.join(os.path.dirname(path), name)) for name in literal}
    return found


def test_the_heki_cache_keys_cover_every_build_input(
    workflow: dict[str, Any], docs_workflow: dict[str, Any]
) -> None:
    """A cached build is reused only for the same code: its key hashes every file cargo reads
    (manifests, sources, build scripts, embedded files, cargo and toolchain configuration) and the
    workflow that builds it (the toolchain pin, the profile, the tools)."""
    keys = heki_cache_keys(workflow, docs_workflow)
    for name, patterns in keys.items():
        assert all("*" not in p or (p.endswith("/**") and "*" not in p[:-3]) for p in patterns)
        assert f".github/workflows/{name}" in patterns
    assert {p for p in keys["ci.yml"] if not p.startswith(".github/")} == {
        p for p in keys["docs.yml"] if not p.startswith(".github/")
    }
    patterns = keys["ci.yml"]
    files = tracked_files()
    build_inputs = (
        {"Cargo.toml", "Cargo.lock", "pyproject.toml"}
        | {path for path in files if path.startswith("kb/")}
        | {path for path in files if path.rsplit("/", 1)[-1] in ("Cargo.toml", "build.rs")}
        | {path for path in files if path.startswith((".cargo/", "rust-toolchain"))}
        | embedded_files()
    )
    assert {"kb/soundness/builtins.toml", "CHANGELOG.md", "skills/heki/SKILL.md"} <= build_inputs
    assert {path for path in build_inputs if not matches(path, patterns)} == set()


def test_a_documentation_change_keeps_the_heki_cache_keys(
    workflow: dict[str, Any], docs_workflow: dict[str, Any]
) -> None:
    """No page is hashed but CHANGELOG.md, which `heki version --changelog` embeds."""
    step = next(s for s in workflow["jobs"]["changes"]["steps"] if s.get("id") == "files")
    documentation = [re.compile(line) for line in step["env"]["DOCS"].splitlines()]
    for patterns in heki_cache_keys(workflow, docs_workflow).values():
        hashed = {
            path
            for path in tracked_files()
            if any(regex.fullmatch(path) for regex in documentation) and matches(path, patterns)
        }
        assert hashed == {"CHANGELOG.md"}


def test_the_docs_tests_archive_holds_the_tests_it_runs(workflow: dict[str, Any]) -> None:
    archive = next(
        s
        for s in workflow["jobs"]["test-build-linux"]["steps"]
        if s.get("name") == "Archive the tests that read the documentation"
    )
    steps = {s.get("name"): s for s in workflow["jobs"]["docs-tests"]["steps"]}
    build = steps["Build and archive the tests that read the documentation"]["run"]
    run = steps["Run the tests that read the documentation"]["run"]
    binaries = set(re.findall(r"binary\((\w+)\)", archive["env"]["BINARIES"]))
    assert binaries == set(re.findall(r"--test (\w+)", build))
    assert binaries == set(re.findall(r"binary\((\w+)\)", run))
    assert "package(heki_rules) & kind(lib)" in archive["env"]["BINARIES"]
    assert "-p heki_rules --lib" in build
    assert "--archive-file docs-tests.tar.zst" in archive["run"]
    assert "--archive-file docs-tests.tar.zst" in build
    assert "--archive-file docs-tests.tar.zst" in run
