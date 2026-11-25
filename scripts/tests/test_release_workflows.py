"""Tests of `release.yml` and `release-sync.yml`: the tag is the version, and `main` follows it."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest
from helpers import ROOT, FakeGitHub, Repository, outputs

yaml = pytest.importorskip("yaml")

PINNED = re.compile(r"[\w.-]+/[\w./-]+@[0-9a-f]{40}")
SET_VERSION = (
    "uv run --no-project --python 3.14 scripts/set_release_versions.py "
    '"$RELEASE_VERSION" ${RELEASE_DATE:+--changelog "$RELEASE_DATE"}'
)
RELEASE_VERSION = "${{ needs.version.outputs.version }}"
RELEASE_DATE = "${{ needs.version.outputs.date }}"
OWNER = "Koudai Aono <630670+koxudaxi@users.noreply.github.com>"
GIT_WITHOUT_USER_CONFIG = {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}

FAKE_GH_CLI = """\
import json, os, sys

args = sys.argv[1:]
with open(os.environ["FAKE_GH_LOG"], "a", encoding="utf-8") as log:
    log.write(json.dumps(args) + "\\n")
if args[:2] == ["pr", "list"]:
    print(os.environ.get("FAKE_PR_NUMBER", ""))
elif args[:2] == ["pr", "create"]:
    print("https://github.com/o/r/pull/42")
elif args[0] == "api" and "--method" not in args and not os.environ.get("FAKE_LABEL_EXISTS"):
    print("HTTP 404: Not Found", file=sys.stderr)
    sys.exit(1)
"""


def load(name: str) -> dict[str, Any]:
    return yaml.safe_load((ROOT / ".github/workflows" / name).read_text(encoding="utf-8"))


@pytest.fixture(scope="module")
def release() -> dict[str, Any]:
    return load("release.yml")


@pytest.fixture(scope="module")
def sync() -> dict[str, Any]:
    return load("release-sync.yml")


def step(job: dict[str, Any], name: str) -> dict[str, Any]:
    return next(step for step in job["steps"] if step.get("name") == name)


def run_step(
    script: str, env: dict[str, str], cwd: Path | None = None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", "-c", script],
        cwd=cwd,
        env=env,
        capture_output=True,
        check=False,
        text=True,
    )


@pytest.mark.parametrize("name", ["release.yml", "release-sync.yml"])
def test_every_action_is_pinned_and_values_reach_scripts_through_the_environment(
    name: str,
) -> None:
    workflow = load(name)
    steps = [step for job in workflow["jobs"].values() for step in job.get("steps", [])]
    uses = [step["uses"] for step in steps if "uses" in step]
    assert uses and all(PINNED.fullmatch(use) for use in uses), uses
    for step in steps:
        assert re.findall(r"\$\{\{(.*?)\}\}", step.get("run", "")) == [], step.get("name")
        if step.get("uses", "").startswith("actions/checkout@"):
            assert step["with"]["persist-credentials"] is False


def test_every_job_that_builds_writes_the_release_version_first(release: dict[str, Any]) -> None:
    builders = {}
    for name, job in release["jobs"].items():
        steps = job.get("steps", [])
        builds = [
            i
            for i, step in enumerate(steps)
            if step.get("uses", "").startswith("PyO3/maturin-action@")
            or "cargo publish" in step.get("run", "")
        ]
        if builds:
            writes = [i for i, step in enumerate(steps) if step.get("run") == SET_VERSION]
            builders[name] = writes
            assert len(writes) == 1 and writes[0] < builds[0], name
            assert steps[writes[0]]["if"] == "env.RELEASE_VERSION != ''", name
            assert all(
                step.get("uses", "").startswith(("actions/checkout@", "astral-sh/setup-uv@"))
                for step in steps[: writes[0]]
            ), name
            assert job["env"]["RELEASE_VERSION"] == RELEASE_VERSION, name
            assert job["env"]["RELEASE_DATE"] == RELEASE_DATE, name
            assert "version" in job["needs"], name
    assert sorted(builders) == ["crates-io", "sdist", "wheels"]


def test_the_checks_compare_the_distributions_with_the_release_version(
    release: dict[str, Any],
) -> None:
    for job, name in [
        ("smoke", "heki runs from the wheel"),
        ("versions", "Every distribution declares the release version"),
    ]:
        checked = step(release["jobs"][job], name)
        assert checked["env"]["RELEASE_VERSION"] == RELEASE_VERSION
        assert 'set -- "$@" --version "$RELEASE_VERSION"' in checked["run"] or (
            'set -- --version "$RELEASE_VERSION"' in checked["run"]
        )
        assert "version" in release["jobs"][job]["needs"]


@pytest.mark.parametrize(
    ("tag", "version_input", "publish", "expected"),
    [
        ("v0.1.2", "", "", "0.1.2"),
        ("", "0.1.3", "false", "0.1.3"),
        ("", "v0.1.3", "false", "0.1.3"),
        ("", "", "true", ""),
        ("", "", "false", ""),
    ],
)
def test_the_release_version_comes_from_the_tag_or_the_input(
    release: dict[str, Any],
    tmp_path: Path,
    tag: str,
    version_input: str,
    publish: str,
    expected: str,
) -> None:
    output = tmp_path / "output"
    script = release["jobs"]["version"]["steps"][0]["run"]
    env = {
        "GITHUB_EVENT_NAME": "release" if tag else "workflow_dispatch",
        "TAG": tag,
        "INPUT": version_input,
        "PUBLISH": publish,
        "GITHUB_OUTPUT": str(output),
    }
    result = run_step(script, {**os.environ, **env})
    assert result.returncode == 0, result.stderr
    assert outputs(output) == {"version": expected, "date": ""}


@pytest.mark.parametrize(
    ("prerelease", "expected"),
    [("false", "2026-11-25"), ("true", "")],
)
def test_a_final_release_dates_its_build_with_the_publish_date(
    release: dict[str, Any], tmp_path: Path, prerelease: str, expected: str
) -> None:
    """The UTC date of `published_at`, as the Release Sync workflow reads it; a pre-release has
    no CHANGELOG section of its own to date."""
    output = tmp_path / "output"
    script = release["jobs"]["version"]["steps"][0]["run"]
    env = {
        "GITHUB_EVENT_NAME": "release",
        "TAG": "v0.1.2",
        "PUBLISHED_AT": "2026-11-25T23:59:59Z",
        "PRERELEASE": prerelease,
        "INPUT": "",
        "PUBLISH": "",
        "GITHUB_OUTPUT": str(output),
    }
    result = run_step(script, {**os.environ, **env})
    assert result.returncode == 0, result.stderr
    assert outputs(output) == {"version": "0.1.2", "date": expected}


def test_a_release_tag_without_a_version_fails(release: dict[str, Any], tmp_path: Path) -> None:
    script = release["jobs"]["version"]["steps"][0]["run"]
    env = {
        "GITHUB_EVENT_NAME": "release",
        "TAG": "v",
        "INPUT": "",
        "PUBLISH": "",
        "GITHUB_OUTPUT": str(tmp_path / "output"),
    }
    result = run_step(script, {**os.environ, **env})
    assert result.returncode == 1
    assert "The release tag `v` names no version." in result.stdout
    assert outputs(tmp_path / "output") == {}


def test_a_version_input_cannot_publish(release: dict[str, Any], tmp_path: Path) -> None:
    script = release["jobs"]["version"]["steps"][0]["run"]
    env = {
        "GITHUB_EVENT_NAME": "workflow_dispatch",
        "TAG": "",
        "INPUT": "0.1.3",
        "PUBLISH": "true",
        "GITHUB_OUTPUT": str(tmp_path / "o"),
    }
    result = run_step(script, {**os.environ, **env})
    assert result.returncode == 1
    assert "builds and checks only" in result.stdout


def test_the_dispatch_inputs(release: dict[str, Any], sync: dict[str, Any]) -> None:
    # PyYAML reads the key `on` as True.
    assert release[True]["workflow_dispatch"]["inputs"]["version"]["type"] == "string"
    assert sync[True]["release"] == {"types": ["published"]}
    assert sync[True]["workflow_dispatch"]["inputs"]["tag"]["required"] is True


def test_only_the_pull_request_job_may_write(sync: dict[str, Any]) -> None:
    assert sync["permissions"] == {}
    assert {name: job["permissions"] for name, job in sync["jobs"].items()} == {
        "patch": {"contents": "read"},
        "pull-request": {"contents": "write", "pull-requests": "write"},
    }
    assert "secrets." not in (ROOT / ".github/workflows/release-sync.yml").read_text(
        encoding="utf-8"
    )
    # A pre-release leaves main as it is.
    assert "!github.event.release.prerelease" in sync["jobs"]["patch"]["if"]


@pytest.mark.parametrize(
    ("release_record", "code", "expected"),
    [
        (
            {"draft": False, "prerelease": False, "published_at": "2026-11-25T03:04:05Z"},
            0,
            {"version": "0.1.2", "date": "2026-11-25"},
        ),
        ({"draft": False, "prerelease": True, "published_at": "2026-11-25T03:04:05Z"}, 1, {}),
    ],
)
def test_the_release_gives_the_version_and_the_date(
    sync: dict[str, Any],
    tmp_path: Path,
    release_record: dict[str, Any],
    code: int,
    expected: dict[str, str],
) -> None:
    github = FakeGitHub(tmp_path / "fake-gh")
    github.respond("/repos/o/r/releases/tags/v0.1.2", release_record)
    output = tmp_path / "output"
    env = {"GITHUB_REPOSITORY": "o/r", "TAG": "v0.1.2", "GITHUB_OUTPUT": str(output)}
    result = run_step(
        step(sync["jobs"]["patch"], "Read the release")["run"], {**github.environment(), **env}
    )
    assert result.returncode == code, result.stderr
    assert outputs(output) == expected
    if code:
        assert "is not a published final release" in result.stdout


@pytest.mark.parametrize("changed", [True, False])
def test_the_patch_holds_the_changes_to_main(
    sync: dict[str, Any], repository: Repository, tmp_path: Path, changed: bool
) -> None:
    head = repository.commit("main", {"Cargo.toml": 'version = "0.1.1"\n'})
    if changed:
        (repository.path / "Cargo.toml").write_text('version = "0.1.2"\n', encoding="utf-8")
    output = tmp_path / "output"
    patch = tmp_path / "release-sync.patch"
    env = {"PATCH_PATH": str(patch), "GITHUB_OUTPUT": str(output), **GIT_WITHOUT_USER_CONFIG}
    result = run_step(
        step(sync["jobs"]["patch"], "Create the patch")["run"],
        {**os.environ, **env},
        cwd=repository.path,
    )
    assert result.returncode == 0, result.stderr
    assert outputs(output) == {"base": head, "changed": str(changed).lower()}
    assert patch.exists() == changed


class SyncRemote:
    """A bare repository that stands in for GitHub, a checkout of its `main`, and a fake `gh`."""

    def __init__(self, tmp_path: Path) -> None:
        origin = Repository(tmp_path / "origin")
        self.main = origin.commit("main", {"Cargo.toml": 'version = "0.1.1"\n'})
        self.bare = tmp_path / "remote.git"
        self.checkout = tmp_path / "checkout"
        self.git("clone", "--quiet", "--bare", str(origin.path), str(self.bare))
        self.git("clone", "--quiet", str(self.bare), str(self.checkout))
        (self.checkout / "Cargo.toml").write_text('version = "0.1.2"\n', encoding="utf-8")
        self.patch = tmp_path / "release-sync.patch"
        self.patch.write_text(self.git("-C", str(self.checkout), "diff", "--binary"))
        self.git("-C", str(self.checkout), "checkout", "--quiet", "--", ".")
        self.gh = tmp_path / "fake-gh"
        self.gh.mkdir()
        (self.gh / "gh").write_text(f"#!{sys.executable}\n{FAKE_GH_CLI}", encoding="utf-8")
        (self.gh / "gh").chmod(0o755)
        self.log = tmp_path / "gh.log"
        self.temp = tmp_path / "runner-temp"
        self.temp.mkdir()

    @staticmethod
    def git(*args: str) -> str:
        return subprocess.run(
            ["git", *args],
            env={**os.environ, **GIT_WITHOUT_USER_CONFIG},
            capture_output=True,
            check=True,
            text=True,
        ).stdout

    def run(self, script: str, **fake: str) -> subprocess.CompletedProcess[str]:
        """Run the step as the job does; the identity of its commit is the step's own."""
        env = {
            key: value for key, value in os.environ.items() if not key.startswith(("GIT_", "FAKE_"))
        } | {
            "PATH": f"{self.gh}{os.pathsep}{os.environ['PATH']}",
            "FAKE_GH_LOG": str(self.log),
            "GH_TOKEN": "token",
            "GITHUB_REPOSITORY": "o/r",
            "PATCH_PATH": str(self.patch),
            "RUNNER_TEMP": str(self.temp),
            "VERSION": "0.1.2",
            **GIT_WITHOUT_USER_CONFIG,
            # The step pushes to GitHub with the token; here the bare repository answers.
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": f"url.{self.bare}.insteadOf",
            "GIT_CONFIG_VALUE_0": "https://x-access-token:token@github.com/o/r.git",
            **fake,
        }
        return run_step(script, env, cwd=self.checkout)

    def gh_calls(self) -> list[list[str]]:
        return [json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines()]

    def branch(self, *args: str) -> str:
        return self.git("--git-dir", str(self.bare), *args).strip()


def test_the_pull_request_is_opened_from_the_sync_branch(
    sync: dict[str, Any], tmp_path: Path
) -> None:
    remote = SyncRemote(tmp_path)
    script = step(
        sync["jobs"]["pull-request"], "Push the branch and open or update the pull request"
    )["run"]

    result = remote.run(script)

    assert result.returncode == 0, result.stderr
    branch = "refs/heads/release/sync-0.1.2"
    assert remote.branch("log", "-1", "--format=%an <%ae>|%cn <%ce>|%s|%P", branch) == (
        f"{OWNER}|{OWNER}|chore: sync release metadata for 0.1.2|{remote.main}"
    )
    assert remote.branch("show", f"{branch}:Cargo.toml") == 'version = "0.1.2"'
    calls = remote.gh_calls()
    create = next(call for call in calls if call[:2] == ["pr", "create"])
    assert create[create.index("--base") + 1] == "main"
    assert create[create.index("--head") + 1] == "release/sync-0.1.2"
    assert create[create.index("--title") + 1] == "chore: sync release metadata for 0.1.2"
    body = (remote.temp / "release-sync-body.md").read_text(encoding="utf-8")
    assert "close and reopen it to start them" in body
    assert body.startswith("Brings `main` to the release v0.1.2:")
    # The label is created once, then added.
    assert calls[-2][:4] == ["api", "--method", "POST", "/repos/o/r/labels"]
    assert calls[-1][:4] == ["api", "--method", "POST", "/repos/o/r/issues/42/labels"]
    assert "labels[]=maintenance" in calls[-1]


def test_a_second_run_updates_the_branch_and_the_pull_request(
    sync: dict[str, Any], tmp_path: Path
) -> None:
    remote = SyncRemote(tmp_path)
    script = step(
        sync["jobs"]["pull-request"], "Push the branch and open or update the pull request"
    )["run"]
    assert remote.run(script, GIT_COMMITTER_DATE="2026-11-25T00:00:00Z").returncode == 0
    first = remote.branch("rev-parse", "refs/heads/release/sync-0.1.2")
    remote.git("-C", str(remote.checkout), "reset", "--quiet", "--hard", remote.main)
    remote.log.unlink()

    result = remote.run(
        script,
        GIT_COMMITTER_DATE="2026-11-26T00:00:00Z",
        FAKE_PR_NUMBER="42",
        FAKE_LABEL_EXISTS="1",
    )

    assert result.returncode == 0, result.stderr
    second = remote.branch("rev-parse", "refs/heads/release/sync-0.1.2")
    assert second != first
    assert remote.branch("rev-parse", f"{second}^") == remote.main
    calls = remote.gh_calls()
    assert not any(call[:2] == ["pr", "create"] for call in calls)
    assert ["pr", "edit", "42"] == next(call for call in calls if call[:2] == ["pr", "edit"])[:3]
    assert not any("/repos/o/r/labels" in call for call in calls if "--method" in call)
    assert calls[-1][:4] == ["api", "--method", "POST", "/repos/o/r/issues/42/labels"]


@pytest.mark.parametrize(
    ("prerelease", "date_flag"),
    [("false", True), ("true", False)],
)
def test_the_docs_a_release_deploys_show_the_release_tag(
    checkout: Path, prerelease: str, date_flag: bool
) -> None:
    """The docs job writes the release the way the build jobs do, with the script the Release
    Sync pull request runs, so the pre-commit `rev:` lines of the deployed pages are the tag."""
    docs = load("docs.yml")["jobs"]["build"]
    write = step(
        docs, "Write the release into the manifests and the pre-commit examples"
    )
    assert write["if"] == "github.event_name == 'release'"
    assert [s.get("name") for s in docs["steps"]].index(write["name"]) < [
        s.get("run") for s in docs["steps"]
    ].index("cargo build --release -p heki")
    scripts = checkout / "scripts"
    scripts.mkdir()
    for name in ("set_release_versions.py",):
        (scripts / name).write_text(
            (ROOT / "scripts" / name).read_text(encoding="utf-8")
        )
    env = {
        **os.environ,
        "TAG": "v0.1.2",
        "PUBLISHED_AT": "2026-11-25T23:59:59Z",
        "PRERELEASE": prerelease,
        "PATH": f"{Path(sys.executable).parent}{os.pathsep}{os.environ['PATH']}",
    }
    fake_python = checkout / "bin"
    fake_python.mkdir()
    (fake_python / "python").write_text(f'#!/bin/sh\nexec "{sys.executable}" "$@"\n')
    (fake_python / "python").chmod(0o755)
    env["PATH"] = f"{fake_python}{os.pathsep}{env['PATH']}"
    result = run_step(write["run"], env, cwd=checkout)
    assert result.returncode == 0, result.stderr
    for page in ("README.md", "docs/adoption.md"):
        assert re.findall(
            r"rev: (v\S+)", (checkout / page).read_text(encoding="utf-8")
        ) == ["v0.1.2"]
    changelog = (checkout / "CHANGELOG.md").read_text(encoding="utf-8")
    assert ("2026-11-25" in changelog) is date_flag


def test_a_docs_release_tag_without_a_version_fails() -> None:
    docs = load("docs.yml")["jobs"]["build"]
    write = step(
        docs, "Write the release into the manifests and the pre-commit examples"
    )
    result = run_step(write["run"], {**os.environ, "TAG": "v"})
    assert result.returncode == 1
    assert "The release tag `v` names no version." in result.stdout
