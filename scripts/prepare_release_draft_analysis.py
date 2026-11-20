"""Skip the Claude analysis for pull requests whose changes cannot reach a release's behavior.

A path is safe when it is not part of what heki ships or how a release is built: tests, the
documentation, development scripts, and the CI and documentation workflows. `release.yml` is not
safe, since its build matrix decides the platforms a release supports. Anything that does not
parse, or a file list whose length differs from the count GitHub reports (the files API stops
at 3,000), goes to Claude.
"""

from __future__ import annotations

import argparse
import json
from collections.abc import Iterable
from pathlib import Path

FILE_CHANGE_KEYS = frozenset({"path", "previous_path"})
SAFE_EXACT_PATHS = frozenset(
    {
        ".github/workflows/ci.yml",
        ".github/workflows/docs.yml",
        ".github/workflows/release-draft.yml",
        ".gitignore",
        "AGENTS.md",
        "ARCHITECTURE.md",
        "CHANGELOG.md",
        "README.md",
        "scripts/build_docs.py",
        "scripts/build_llms_txt.py",
        "scripts/check_memory.py",
        "scripts/prepare_release_draft_analysis.py",
        "scripts/prepare_release_draft_artifacts.py",
        "scripts/release_draft_common.py",
        "scripts/report_release_analysis_failure.py",
        "scripts/resolve_release_draft_pr.py",
        "scripts/update_release_draft.py",
        "scripts/validate_release_draft_analysis.py",
        "zensical.toml",
    }
)
SAFE_PREFIXES = ("tests/", "docs/", "scripts/tests/", "crates/heki/tests/")
SAFE_REASONING = (
    "Only tests, documentation, development scripts, or CI workflows changed, which cannot change the behavior of "
    "a release."
)


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--changed-files-path", required=True, type=Path)
    parser.add_argument("--expected-changed-files", required=True, type=int)
    parser.add_argument("--analysis-path", required=True, type=Path)
    parser.add_argument("--github-output-path", required=True, type=Path)
    return parser.parse_args(argv)


def is_safe_path(path: str) -> bool:
    """Return whether one exact changed path cannot change the behavior of a release."""
    if not path or path != path.strip():
        return False
    return path in SAFE_EXACT_PATHS or any(
        path.startswith(prefix) and path != prefix for prefix in SAFE_PREFIXES
    )


def _paths_from_record(raw_record: str) -> tuple[str, ...] | None:
    """Parse one GitHub API file record; None when it is not exactly the expected shape."""
    try:
        file_change = json.loads(raw_record)
    except json.JSONDecodeError:
        return None
    if not isinstance(file_change, dict) or file_change.keys() != FILE_CHANGE_KEYS:
        return None
    match file_change:
        case {"path": str(path), "previous_path": None}:
            return (path,)
        case {"path": str(path), "previous_path": str(previous_path)}:
            return path, previous_path
    return None


def requires_claude(records: Iterable[str], expected_changed_files: int) -> bool:
    """Return whether the file records leave any doubt that the changes miss every release."""
    if expected_changed_files <= 0:
        return True
    count = 0
    for raw_record in records:
        if not raw_record.strip():
            continue
        if not (paths := _paths_from_record(raw_record)) or not all(map(is_safe_path, paths)):
            return True
        count += 1
    return count != expected_changed_files


def main(argv: list[str] | None = None) -> None:
    """Write the routing decision, and the analysis itself when Claude is not needed."""
    args = _parse_args(argv)
    try:
        with args.changed_files_path.open(encoding="utf-8") as records:
            needs_claude = requires_claude(records, args.expected_changed_files)
    except (OSError, UnicodeError):
        needs_claude = True
    if not needs_claude:
        args.analysis_path.write_text(
            json.dumps(
                {
                    "has_breaking_changes": False,
                    "breaking_changes_content": "",
                    "reasoning": SAFE_REASONING,
                }
            )
            + "\n",
            encoding="utf-8",
        )
    with args.github_output_path.open("a", encoding="utf-8") as github_output:
        github_output.write(f"requires_claude={str(needs_claude).lower()}\n")


if __name__ == "__main__":
    main()
