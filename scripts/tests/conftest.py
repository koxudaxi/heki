"""Fixtures for the tests of the release workflows' scripts.

Run them from the repository root with:

    uv run --only-group scripts-tests --isolated --python 3.12 pytest scripts/tests
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

# The scripts import each other as top-level modules, as `python3 scripts/<name>.py` does.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from helpers import FakeGitHub, Repository


@pytest.fixture
def repository(tmp_path: Path) -> Repository:
    """An empty git repository on `main`."""
    return Repository(tmp_path / "repository")


@pytest.fixture
def github(tmp_path: Path) -> FakeGitHub:
    """A fake `gh` for the scripts that call the GitHub API."""
    return FakeGitHub(tmp_path / "fake-gh")
