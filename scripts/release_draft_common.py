"""Definitions the scripts of the Release Draft workflow share.

The workflow (`.github/workflows/release-draft.yml`) runs these scripts from a trusted checkout of
`main`. They read GitHub API responses and git objects as data and never import or run code of
the pull request they analyze.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from collections.abc import Iterable, Mapping
from typing import Any, NoReturn

COMMIT_SHA = re.compile(r"[0-9a-f]{40}")
PR_NUMBER = re.compile(r"[1-9][0-9]{0,9}")
VERSION_TAG = re.compile(r"v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
MARKER_NAME = "heki"
"""The word in ``⟪heki <nonce>⟫``, which starts every line the workflow writes into Claude's diff."""

BREAKING_LABEL = "breaking-change"
ANALYZED_LABEL = "breaking-change-analyzed"
UNLISTED_LABELS = frozenset({"maintenance", "dependencies"})
"""Pull requests with one of these labels are left out of the draft's list of changes."""

BREAKING_CHANGE_HEADINGS = (
    "Default Rule Changes",
    "Rule Code and Name Changes",
    "Configuration and CLI Changes",
    "Output and Editor Integration Changes",
    "Python and Platform Support Changes",
)
"""The ``###`` headings of the ``## Breaking Changes`` section, in the order the draft lists them."""

Version = tuple[int, int, int]


def fail(message: str) -> NoReturn:
    """Print a message that holds no API or pull-request text and stop the step."""
    print(message, file=sys.stderr)
    raise SystemExit(1)


def parse_version(tag: object) -> Version | None:
    """Return the version a release tag names (``v0.1.0`` or ``0.1.0``), or None for any other text."""
    if isinstance(tag, str) and (match := VERSION_TAG.fullmatch(tag)):
        return int(match[1]), int(match[2]), int(match[3])
    return None


def format_version(version: Version) -> str:
    """Return a version as ``MAJOR.MINOR.PATCH``."""
    return ".".join(map(str, version))


def published_versions(releases: Iterable[Mapping[str, Any]]) -> list[tuple[Version, str]]:
    """Return the version and tag of every published, final release, the newest version first.

    Drafts and pre-releases do not count: neither is a release users received as final. Two tags
    can name one version (``v0.2.0`` and ``0.2.0``), and both are kept: either may be the one an
    ancestry check finds.
    """
    return sorted(
        (
            (version, release["tag_name"])
            for release in releases
            if release.get("draft") is False
            and release.get("prerelease") is False
            and (version := parse_version(release.get("tag_name")))
        ),
        reverse=True,
    )


def gh_api(endpoint: str, *, paginate: bool = False, jq: str | None = None) -> str:
    """Return the body `gh api` prints for a GET request; fail without echoing it."""
    command = [
        "gh",
        "api",
        *(["--paginate"] if paginate else []),
        endpoint,
        *(["--jq", jq] if jq else []),
    ]
    result = subprocess.run(command, capture_output=True, check=False, text=True, encoding="utf-8")
    if result.returncode:
        fail(f"The GitHub API request for {endpoint.partition('?')[0]} failed.")
    return result.stdout


def gh_api_json(endpoint: str) -> Any:
    """Return the JSON document of a GET request."""
    try:
        return json.loads(gh_api(endpoint))
    except ValueError:
        fail(f"The GitHub API returned invalid JSON for {endpoint.partition('?')[0]}.")


def json_lines(text: str) -> list[Any]:
    """Parse one JSON value per non-empty line, as `gh api --jq '.[] | ...'` prints them.

    Only LF ends a record: the encoder leaves characters such as U+0085 unescaped inside strings,
    and `str.splitlines` would split there.
    """
    try:
        return [json.loads(line) for line in text.split("\n") if line.strip()]
    except ValueError:
        fail("Expected one JSON value per line.")
