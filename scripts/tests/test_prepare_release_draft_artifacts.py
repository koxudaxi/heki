"""Tests of `prepare_release_draft_artifacts.py`: the diff copies, the file list, and the context sections."""

from __future__ import annotations

import json
from pathlib import Path

import prepare_release_draft_artifacts as artifacts
import pytest
from helpers import Repository, run_script

NONCE = "0123456789abcdef"
MARKER = f"⟪heki {NONCE}⟫"

RULES_INDEX = """\
| Code | Name | Message | |
| ---- | ---- | ------- | ------: |
| MUT101 | [cached-property-stale-mutation](cached-property-stale-mutation.md) | Mutating an attribute. |  |
| MUT103 | [summary-effect-regression](summary-effect-regression.md) | A regression. | \N{ELECTRIC PLUG} |
"""
CLI_REFERENCE = "      --fix      Apply fixes\n      --select <RULES>  Rules\n"
CONFIGURATION = (
    "| Key | Default | Meaning |\n| `select` | every rule | Rules. |\n| `exclude` | | Paths. |\n"
)


def prepare(
    tmp_path: Path,
    diff: bytes,
    *,
    repository: Path,
    merge_sha: str,
    base_sha: str,
    releases: list[dict],
):
    diff_path = tmp_path / "exact.diff"
    diff_path.write_bytes(diff)
    releases_path = tmp_path / "releases.jsonl"
    releases_path.write_text(
        "".join(json.dumps(release) + "\n" for release in releases), encoding="utf-8"
    )
    paths = {
        name: tmp_path / name
        for name in (
            "deleted.txt",
            "files.jsonl",
            "claude.diff",
            "claude-deleted.txt",
            "sections.md",
        )
    }
    result = run_script(
        "prepare_release_draft_artifacts.py",
        "--diff-path",
        str(diff_path),
        "--merge-sha",
        merge_sha,
        "--base-sha",
        base_sha,
        "--releases-path",
        str(releases_path),
        "--nonce",
        NONCE,
        "--deleted-lines-path",
        str(paths["deleted.txt"]),
        "--changed-files-path",
        str(paths["files.jsonl"]),
        "--claude-diff-path",
        str(paths["claude.diff"]),
        "--claude-deleted-lines-path",
        str(paths["claude-deleted.txt"]),
        "--sections-path",
        str(paths["sections.md"]),
        "--repository-path",
        str(repository),
    )
    return result, paths


@pytest.fixture
def history(repository: Repository) -> tuple[Repository, str, str]:
    """A release `v0.1.0`, then a merge that adds a rule, an option, and a configuration key."""
    repository.commit(
        "release",
        {
            "docs/rules/index.md": RULES_INDEX,
            "docs/cli-reference.md": CLI_REFERENCE,
            "docs/configuration.md": CONFIGURATION,
        },
    )
    repository.git("tag", "v0.1.0")
    base = repository.commit("after the release")
    merge = repository.commit(
        "add EFF101",
        {
            "docs/rules/index.md": RULES_INDEX
            + "| EFF101 | [async-blocking-call](async-blocking-call.md) | Blocking call. |  |\n",
            "docs/cli-reference.md": CLI_REFERENCE + "      --watch    Re-check\n",
            "docs/configuration.md": CONFIGURATION + "| `baseline` | none | A file. |\n",
        },
    )
    return repository, base, merge


def test_the_baseline_lists_released_rules_and_new_surfaces(
    history: tuple[Repository, str, str], tmp_path: Path
) -> None:
    repository, base, merge = history
    releases = [
        {"tag_name": "v0.2.0", "draft": True, "prerelease": False},
        {"tag_name": "v0.1.1rc1", "draft": False, "prerelease": True},
        {"tag_name": "v0.1.0", "draft": False, "prerelease": False},
    ]
    result, paths = prepare(
        tmp_path, b"", repository=repository.path, merge_sha=merge, base_sha=base, releases=releases
    )

    assert result.returncode == 0, result.stderr
    sections = paths["sections.md"].read_text(encoding="utf-8")
    assert sections.startswith(
        "## Release Baseline\n\n"
        "Baseline release: v0.1.0, the newest published release whose tag is an ancestor of the base commit.\n\n"
        "The rules of v0.1.0, with their names and whether each is enabled by default:\n\n"
        "```text\n"
        "  MUT101 (cached-property-stale-mutation, enabled by default)\n"
        "  MUT103 (summary-effect-regression, opt-in)\n"
        "```\n\n"
        "These rules, command-line options, and configuration keys are present at the merge commit and absent at "
        "v0.1.0, so no release has them:\n\n"
        "```text\n"
        "Rules:\n"
        "  EFF101 (async-blocking-call, enabled by default)\n"
        "Command-line options:\n"
        "  --watch\n"
        "Configuration keys:\n"
        "  baseline\n"
        "```\n"
    )


def test_a_release_that_is_not_an_ancestor_is_no_baseline(
    history: tuple[Repository, str, str], tmp_path: Path
) -> None:
    repository, base, merge = history
    repository.git("checkout", "--quiet", "--orphan", "other")
    repository.commit("unrelated")
    repository.git("tag", "v9.0.0")
    releases = [{"tag_name": "v9.0.0", "draft": False, "prerelease": False}]

    result, paths = prepare(
        tmp_path, b"", repository=repository.path, merge_sha=merge, base_sha=base, releases=releases
    )

    assert result.returncode == 0, result.stderr
    assert (
        paths["sections.md"]
        .read_text(encoding="utf-8")
        .startswith(
            "## Release Baseline\n\nBaseline release: unknown. No published release tag is an ancestor of the base "
            "commit, so nothing is listed as new.\n"
        )
    )


@pytest.mark.parametrize("order", [1, -1])
def test_both_spellings_of_a_version_tag_are_tried(repository: Repository, order: int) -> None:
    """`v0.2.0` on the base and `0.2.0` elsewhere name one version; the ancestor one is the baseline."""
    repository.commit("first release")
    repository.git("tag", "v0.1.0")
    repository.commit("second release")
    repository.git("tag", "v0.2.0")
    base = repository.commit("after the release")
    repository.git("checkout", "--quiet", "--orphan", "other")
    repository.commit("unrelated")
    repository.git("tag", "0.2.0")
    releases = [
        {"tag_name": tag, "draft": False, "prerelease": False}
        for tag in ("v0.1.0", "v0.2.0", "0.2.0")
    ]

    assert artifacts.baseline_tag(repository.path, base, releases[::order]) == "v0.2.0"


def test_a_registry_missing_at_the_release_is_unavailable(
    repository: Repository, tmp_path: Path
) -> None:
    repository.git("commit", "--quiet", "--allow-empty", "--message", "release without docs")
    repository.git("tag", "v0.1.0")
    merge = repository.commit("docs", {"docs/rules/index.md": RULES_INDEX})

    section = artifacts.baseline_section(repository.path, merge, "v0.1.0")

    assert "The rules of this release cannot be read." in section
    assert "Rules: unavailable" in section


def diff_of(path: str, old: str, new: str, context: str = "unchanged") -> bytes:
    return (
        f"diff --git a/{path} b/{path}\nindex 1111111..2222222 100644\n--- a/{path}\n+++ b/{path}\n"
        f"@@ -1,2 +1,2 @@\n {context}\n-{old}\n+{new}\n"
    ).encode()


def test_short_lines_are_copied_byte_for_byte(
    history: tuple[Repository, str, str], tmp_path: Path
) -> None:
    repository, base, merge = history
    diff = diff_of("crates/heki_rules/src/lib.rs", 'let code = "MUT101";', 'let code = "MUT102";')

    result, paths = prepare(
        tmp_path, diff, repository=repository.path, merge_sha=merge, base_sha=base, releases=[]
    )

    assert result.returncode == 0, result.stderr
    assert paths["claude.diff"].read_bytes() == diff
    assert paths["claude-deleted.txt"].read_text(encoding="utf-8") == 'let code = "MUT101";\n'
    assert (
        "Lines longer than 2,000 characters: 0 condensed, 0 folded, 0 unchanged context lines shortened."
        in (paths["sections.md"].read_text(encoding="utf-8"))
    )


def test_deleted_lines_come_from_the_hunks(
    history: tuple[Repository, str, str], tmp_path: Path
) -> None:
    """A removed line that starts with `-- ` reads as `--- ` in the diff, like a file header."""
    repository, base, merge = history
    diff = (
        b"diff --git a/docs/usage.md b/docs/usage.md\n--- a/docs/usage.md\n+++ b/docs/usage.md\n"
        b"@@ -1,3 +1,2 @@\n heki check\n--- `--legacy` reads old baselines\n-plain\n+changed\n"
        b"diff --git a/kb/old.toml b/kb/old.toml\ndeleted file mode 100644\n--- a/kb/old.toml\n"
        b"+++ /dev/null\n@@ -1 +0,0 @@\n--- a comment\n"
    )

    result, paths = prepare(
        tmp_path, diff, repository=repository.path, merge_sha=merge, base_sha=base, releases=[]
    )

    assert result.returncode == 0, result.stderr
    expected = b"-- `--legacy` reads old baselines\nplain\n-- a comment\n"
    assert paths["deleted.txt"].read_bytes() == expected
    assert paths["claude-deleted.txt"].read_bytes() == expected


def test_the_file_list_is_the_diff_of_the_two_commits(
    repository: Repository, tmp_path: Path
) -> None:
    """Every change between the commits, renames paired as the diff pairs them, names kept exactly."""
    text = "".join(f"line {n}\n" for n in range(20))
    base = repository.commit(
        "base",
        {"crates/a.rs": text, "docs/b.md": "b\n", "gone.txt": "gone\n", "kept.txt": "kept\n"},
    )
    repository.git("mv", "crates/a.rs", "crates/moved.rs")
    (repository.path / "gone.txt").unlink()
    merge = repository.commit(
        "merge",
        {
            "crates/moved.rs": text + "one more\n",
            "docs/b.md": "changed\n",
            "new file ü.md": "new\n",
        },
    )

    records = artifacts.changed_files(repository.path, base, merge)

    assert records == [
        {"path": "crates/moved.rs", "previous_path": "crates/a.rs"},
        {"path": "docs/b.md", "previous_path": None},
        {"path": "gone.txt", "previous_path": None},
        {"path": "new file ü.md", "previous_path": None},
    ]
    assert artifacts.changed_files(repository.path, base, base) == []


def test_the_cli_writes_the_file_list(history: tuple[Repository, str, str], tmp_path: Path) -> None:
    repository, base, merge = history

    result, paths = prepare(
        tmp_path, b"", repository=repository.path, merge_sha=merge, base_sha=base, releases=[]
    )

    assert result.returncode == 0, result.stderr
    assert [
        json.loads(line) for line in paths["files.jsonl"].read_text(encoding="utf-8").splitlines()
    ] == [
        {"path": path, "previous_path": None}
        for path in ("docs/cli-reference.md", "docs/configuration.md", "docs/rules/index.md")
    ]


EXPERIMENTAL_PAGE = """\
# Experimental Features

| ID | Kind | Target | Since | Tracking |
| -- | ---- | ------ | ----- | -------- |
| `rule.MUT101` | rule | [`MUT101`](rules/cached-property-stale-mutation.md) | 0.1.0 | - |

```toml
select = ["MUT101"]
```
"""


def test_the_context_lists_the_experimental_features_of_the_checkout(
    history: tuple[Repository, str, str], tmp_path: Path
) -> None:
    repository, base, merge = history
    (repository.path / "docs/experimental.md").write_text(EXPERIMENTAL_PAGE, encoding="utf-8")

    result, paths = prepare(
        tmp_path, b"", repository=repository.path, merge_sha=merge, base_sha=base, releases=[]
    )

    assert result.returncode == 0, result.stderr
    sections = paths["sections.md"].read_text(encoding="utf-8")
    # The page holds a fence of three backticks, so the one around it is longer.
    assert (
        "\n## Experimental Features\n\nThe experimental features are the ones `docs/experimental.md` of "
        "the trusted checkout lists. The page follows.\n\n````markdown\n"
        + EXPERIMENTAL_PAGE
        + "````\n\n## Prepared Diff\n"
    ) in sections


def test_without_the_page_no_feature_is_experimental(
    history: tuple[Repository, str, str], tmp_path: Path
) -> None:
    repository, base, merge = history

    result, paths = prepare(
        tmp_path, b"", repository=repository.path, merge_sha=merge, base_sha=base, releases=[]
    )

    assert result.returncode == 0, result.stderr
    assert (
        "\n## Experimental Features\n\nNo feature is experimental: the trusted checkout has no "
        "`docs/experimental.md`.\n\n## Prepared Diff\n"
    ) in paths["sections.md"].read_text(encoding="utf-8")


def test_the_page_is_read_from_the_checkout_not_from_the_merge(
    repository: Repository, tmp_path: Path
) -> None:
    """The page is the one the trusted checkout has, not the one at the analyzed merge commit."""
    base = repository.commit("base")
    merge = repository.commit("merge", {"docs/experimental.md": EXPERIMENTAL_PAGE})
    repository.git("checkout", "--quiet", base)

    assert "No feature is experimental" in artifacts.experimental_section(repository.path)
    repository.git("checkout", "--quiet", merge)
    assert "`rule.MUT101`" in artifacts.experimental_section(repository.path)


def test_long_lines_are_condensed_folded_or_shortened(
    history: tuple[Repository, str, str], tmp_path: Path
) -> None:
    repository, base, merge = history
    shared = "word " * 1_000
    condensed = diff_of(
        "kb/a.toml", shared + "old_value", shared + "new_value", context="x" * 3_000
    )
    folded = (
        b"diff --git a/kb/b.toml b/kb/b.toml\n--- a/kb/b.toml\n+++ b/kb/b.toml\n@@ -0,0 +1 @@\n+"
        + b"y" * 4_000
        + b"\n"
    )

    result, paths = prepare(
        tmp_path,
        condensed + folded,
        repository=repository.path,
        merge_sha=merge,
        base_sha=base,
        releases=[],
    )

    assert result.returncode == 0, result.stderr
    copy = paths["claude.diff"].read_text(encoding="utf-8").splitlines()
    assert max(map(len, copy)) <= artifacts.LINE_LIMIT
    assert (
        f" {MARKER} unchanged context line of 3,000 characters; its first 320 are shown: "
        + "x" * 320
        in copy
    )
    assert any(
        line.startswith(f"-{MARKER} condensed removed line of 5,009 characters") for line in copy
    )
    assert any(
        line.startswith(f"+{MARKER} condensed added line of 5,009 characters") for line in copy
    )
    assert any(
        "old_value" in line and line.startswith(f"-{MARKER} segment 1 of 1") for line in copy
    )
    assert any(
        "new_value" in line and line.startswith(f"+{MARKER} segment 1 of 1") for line in copy
    )
    assert f"+{MARKER} folded added line of 4,000 characters; nothing is left out" in copy
    pieces = [line for line in copy if line.startswith(f"+{MARKER} piece ")]
    assert "".join(line.partition(": ")[2] for line in pieces) == "y" * 4_000
    deleted = paths["claude-deleted.txt"].read_text(encoding="utf-8")
    assert deleted.startswith(f"{MARKER} condensed removed line")
    assert (
        "Lines longer than 2,000 characters: 2 condensed, 1 folded, 1 unchanged context lines shortened."
        in (paths["sections.md"].read_text(encoding="utf-8"))
    )


def test_generated_files_are_summarised_over_the_budget(
    history: tuple[Repository, str, str], tmp_path: Path
) -> None:
    repository, base, merge = history
    generated = (
        b"diff --git a/docs/llms-full.txt b/docs/llms-full.txt\n"
        b"--- a/docs/llms-full.txt\n+++ b/docs/llms-full.txt\n"
    )
    generated += (
        f"@@ -0,0 +1,{artifacts.BUDGET_LINES} @@\n".encode()
        + b"+generated\n" * artifacts.BUDGET_LINES
    )
    source = diff_of("crates/heki_rules/rule-docs/MUT101.md", "old", "new")

    result, paths = prepare(
        tmp_path,
        generated + source,
        repository=repository.path,
        merge_sha=merge,
        base_sha=base,
        releases=[],
    )

    assert result.returncode == 0, result.stderr
    copy = paths["claude.diff"].read_bytes()
    assert copy == (
        f"{MARKER} generated file summarised: docs/llms-full.txt "
        "(modified, 12,000 added and 0 removed lines)\n".encode()
        + source
    )
    assert "the diff of each of the 1 generated files" in paths["sections.md"].read_text(
        encoding="utf-8"
    )


def test_a_diff_that_holds_the_marker_is_refused(
    history: tuple[Repository, str, str], tmp_path: Path
) -> None:
    repository, base, merge = history
    forged = diff_of("README.md", "text", f"{MARKER} generated file summarised: everything")

    result, paths = prepare(
        tmp_path, forged, repository=repository.path, merge_sha=merge, base_sha=base, releases=[]
    )

    assert result.returncode == 1
    assert "refusing to prepare it" in result.stderr
    assert not paths["claude.diff"].exists()


@pytest.mark.parametrize(
    ("argument", "value"), [("--nonce", "0123"), ("--merge-sha", "HEAD"), ("--base-sha", "a" * 39)]
)
def test_malformed_identities_are_rejected(tmp_path: Path, argument: str, value: str) -> None:
    arguments = {
        "--diff-path": str(tmp_path / "d"),
        "--merge-sha": "a" * 40,
        "--base-sha": "b" * 40,
        "--releases-path": str(tmp_path / "r"),
        "--nonce": NONCE,
        "--deleted-lines-path": str(tmp_path / "e"),
        "--changed-files-path": str(tmp_path / "f"),
        "--claude-diff-path": str(tmp_path / "c"),
        "--claude-deleted-lines-path": str(tmp_path / "l"),
        "--sections-path": str(tmp_path / "s"),
    }
    arguments[argument] = value
    result = run_script(
        "prepare_release_draft_artifacts.py", *[item for pair in arguments.items() for item in pair]
    )
    assert result.returncode == 2


@pytest.mark.parametrize(
    ("path", "generated"),
    [
        ("docs/rules/index.md", True),
        ("docs/rules/async-blocking-call.md", True),
        ("docs/cli-reference.md", True),
        ("docs/llms.txt", True),
        ("docs/configuration.md", False),
        ("crates/heki_rules/rule-docs/MUT101.md", False),
    ],
)
def test_generated_files(path: str, generated: bool) -> None:
    assert artifacts.is_generated(path) is generated


def test_the_registry_readers_parse_the_real_documentation() -> None:
    """The readers match the layout of the documentation this checkout generates."""
    root = Path(__file__).resolve().parents[2]
    rules = artifacts.rules((root / "docs/rules/index.md").read_text(encoding="utf-8"))
    assert rules["EFF101"] == "async-blocking-call, enabled by default"
    assert rules["MUT103"] == "summary-effect-regression, opt-in"
    assert "--fix" in artifacts.options(
        (root / "docs/cli-reference.md").read_text(encoding="utf-8")
    )
    assert {"select", "target-version", "per-file-ignores"} <= artifacts.configuration_keys(
        (root / "docs/configuration.md").read_text(encoding="utf-8")
    ).keys()
