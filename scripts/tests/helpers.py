"""Helpers for the tests of the release workflows' scripts: git repositories, a fake `gh`, script runs."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any

SCRIPTS = Path(__file__).resolve().parent.parent
ROOT = SCRIPTS.parent

GIT_ENVIRONMENT = {
    "GIT_AUTHOR_NAME": "Test",
    "GIT_AUTHOR_EMAIL": "test@example.com",
    "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z",
    "GIT_COMMITTER_NAME": "Test",
    "GIT_COMMITTER_EMAIL": "test@example.com",
    "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
}

FAKE_GH = """\
import json, os, sys

args = sys.argv[1:]
endpoint = next(arg for arg in args[1:] if arg.startswith("/"))
with open(os.environ["FAKE_GH_LOG"], "a", encoding="utf-8") as log:
    log.write(endpoint + "\\n")
with open(os.environ["FAKE_GH_RESPONSES"], encoding="utf-8") as responses:
    value = json.load(responses).get(endpoint)
if value is None:
    print("HTTP 404: Not Found", file=sys.stderr)
    sys.exit(1)
if "--jq" in args and isinstance(value, list):
    # The fixtures hold the records already projected, as `--jq '.[] | {...}'` prints them.
    for record in value:
        print(json.dumps(record))
else:
    print(json.dumps(value))
"""


class Repository:
    """A git repository in a temporary directory, with commits whose files the test chooses."""

    def __init__(self, path: Path) -> None:
        self.path = path
        path.mkdir(parents=True, exist_ok=True)
        self.git("init", "--quiet", "--initial-branch=main")

    def git(self, *args: str) -> str:
        """Run git in the repository and return its output."""
        return subprocess.run(
            ["git", *args],
            cwd=self.path,
            env={**os.environ, **GIT_ENVIRONMENT},
            capture_output=True,
            check=True,
            text=True,
        ).stdout.strip()

    def commit(self, message: str, files: dict[str, str] | None = None) -> str:
        """Write the files, commit everything, and return the commit's SHA."""
        for name, text in (files or {}).items():
            (self.path / name).parent.mkdir(parents=True, exist_ok=True)
            (self.path / name).write_text(text, encoding="utf-8")
        self.git("add", "--all")
        self.git("commit", "--quiet", "--allow-empty", "--message", message)
        return self.git("rev-parse", "HEAD")


class FakeGitHub:
    """A `gh` on PATH that answers GET requests from a map of endpoints, and logs each request."""

    def __init__(self, directory: Path) -> None:
        self.directory = directory
        directory.mkdir(parents=True, exist_ok=True)
        executable = directory / "gh"
        executable.write_text(f"#!{sys.executable}\n{FAKE_GH}", encoding="utf-8")
        executable.chmod(0o755)
        self.responses_path = directory / "responses.json"
        self.log_path = directory / "requests.log"
        self.responses: dict[str, Any] = {}

    def respond(self, endpoint: str, value: Any) -> None:
        """Answer requests for an endpoint (with its query) with a JSON value."""
        self.responses[endpoint] = value
        self.responses_path.write_text(json.dumps(self.responses), encoding="utf-8")

    def requests(self) -> list[str]:
        """Return the endpoints requested so far."""
        return (
            self.log_path.read_text(encoding="utf-8").splitlines() if self.log_path.exists() else []
        )

    def environment(self) -> dict[str, str]:
        """Return the environment that puts this `gh` first on PATH."""
        self.responses_path.write_text(json.dumps(self.responses), encoding="utf-8")
        return {
            **os.environ,
            "PATH": f"{self.directory}{os.pathsep}{os.environ['PATH']}",
            "FAKE_GH_RESPONSES": str(self.responses_path),
            "FAKE_GH_LOG": str(self.log_path),
        }


def run_script(
    name: str, *args: str, cwd: Path | None = None, env: dict[str, str] | None = None
) -> subprocess.CompletedProcess[str]:
    """Run one of the workflow's scripts as the workflow does, from a checkout's working directory."""
    return subprocess.run(
        [sys.executable, str(SCRIPTS / name), *args],
        cwd=cwd or ROOT,
        env=env or dict(os.environ),
        capture_output=True,
        check=False,
        text=True,
    )


def outputs(path: Path) -> dict[str, str]:
    """Parse a `GITHUB_OUTPUT` file of `key=value` lines."""
    return (
        dict(line.split("=", 1) for line in path.read_text(encoding="utf-8").splitlines())
        if path.exists()
        else {}
    )
