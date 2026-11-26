"""Fixtures for the tests of the release workflows' scripts.

Run them from the repository root with:

    uv run --only-group scripts-tests --isolated --python 3.12 pytest scripts/tests
"""

from __future__ import annotations

import shutil
import sys
import tomllib
from pathlib import Path

import pytest

# The scripts import each other as top-level modules, as `python3 scripts/<name>.py` does.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import set_release_versions
from helpers import ROOT, FakeGitHub, Repository

DATA = Path(__file__).parent / "data" / "set_release_versions"
MANIFESTS = ("Cargo.toml", "pyproject.toml", "Cargo.lock", "uv.lock")
CHECKOUT_VERSION = "0.1.1"
"""The version of the copies, older than the releases the tests sync, whatever `main` declares."""


@pytest.fixture
def repository(tmp_path: Path) -> Repository:
    """An empty git repository on `main`."""
    return Repository(tmp_path / "repository")


@pytest.fixture
def github(tmp_path: Path) -> FakeGitHub:
    """A fake `gh` for the scripts that call the GitHub API."""
    return FakeGitHub(tmp_path / "fake-gh")


@pytest.fixture
def checkout(tmp_path: Path) -> Path:
    """Copies of the repository's manifests, lock files, and the pages that pin the release, at
    `CHECKOUT_VERSION`, and a CHANGELOG."""
    for name in MANIFESTS:
        shutil.copy(ROOT / name, tmp_path / name)
    pinned = {*set_release_versions.PRECOMMIT_DOCS, *set_release_versions.PACKAGE_PIN_DOCS}
    for name in sorted(pinned):
        (tmp_path / name).parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(ROOT / name, tmp_path / name)
    for member in tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"][
        "members"
    ]:
        (tmp_path / member).mkdir(parents=True)
        shutil.copy(ROOT / member / "Cargo.toml", tmp_path / member / "Cargo.toml")
    for path, text in set_release_versions.release_files(tmp_path, CHECKOUT_VERSION, None).items():
        path.write_text(text, encoding="utf-8")
    shutil.copy(DATA / "CHANGELOG.md", tmp_path / "CHANGELOG.md")
    return tmp_path
