"""Tests of `update_release_draft.py`: the next version, the accumulated draft body, the comments, and the labels."""

from __future__ import annotations

import json
import shutil
from pathlib import Path
from typing import Any

import pytest
import update_release_draft as update
from helpers import outputs, run_script

DATA = Path(__file__).parent / "data" / "update_release_draft"
NOTICE_010 = (
    "Before publishing this draft, bump the version to 0.1.1 in a pull request and merge it: `pyproject.toml`, "
    "`Cargo.toml` (`workspace.package.version` and the internal crates under `workspace.dependencies`), and "
    "`Cargo.lock`."
)


def write_manifests(root: Path, version: str, *, internal: str | None = None) -> Path:
    root.mkdir(parents=True, exist_ok=True)
    (root / "pyproject.toml").write_text(
        f'[project]\nname = "heki"\nversion = "{version}"\n', encoding="utf-8"
    )
    (root / "Cargo.toml").write_text(
        f'[workspace.package]\nversion = "{version}"\n\n[workspace.dependencies]\n'
        f'heki_ast = {{ path = "crates/heki_ast", version = "{internal or version}" }}\nanyhow = "1.0"\n',
        encoding="utf-8",
    )
    return root


def released(
    tag: str, *, draft: bool = False, prerelease: bool = False, body: str | None = "", id: int = 1
) -> dict:
    return {
        "id": id,
        "tag_name": tag,
        "name": tag,
        "draft": draft,
        "prerelease": prerelease,
        "body": body,
    }


def merged(
    number: int,
    *,
    title: str = "A change",
    labels: tuple[str, ...] = (),
    content: str = "",
    experimental: str = "",
    comment_id: int | None = None,
) -> update.MergedPullRequest:
    analysis = update.Analysis(bool(content), content, experimental, "Reasoning.")
    return update.MergedPullRequest(number, title, frozenset(labels), analysis, comment_id)


BREAKING_CONTENT = (
    "### Configuration and CLI Changes\n* Option renamed - The option `--a` is now `--b` (#{n})\n"
)


@pytest.mark.parametrize(
    ("latest", "breaking", "manifest", "expected"),
    [
        ((0, 1, 0), False, (0, 1, 0), (0, 1, 1)),
        ((0, 1, 0), True, (0, 1, 0), (0, 2, 0)),
        ((0, 1, 4), True, None, (0, 2, 0)),
        ((0, 9, 3), False, None, (0, 9, 4)),
        ((1, 2, 3), True, None, (2, 0, 0)),
        ((1, 2, 3), False, None, (1, 2, 4)),
        # The manifests already name a later version: the owner chose a larger bump.
        ((0, 1, 0), False, (0, 2, 0), (0, 2, 0)),
        ((0, 1, 0), True, (0, 1, 1), (0, 2, 0)),
    ],
)
def test_next_version(latest, breaking, manifest, expected) -> None:
    assert update.next_version(latest, breaking=breaking, manifest=manifest) == expected


def test_the_notice_appears_while_a_manifest_names_another_version(tmp_path: Path) -> None:
    declared = update.manifest_versions(write_manifests(tmp_path / "a", "0.1.0"))
    assert update.manifest_version(declared) == (0, 1, 0)
    assert update.version_notice((0, 1, 1), declared) == NOTICE_010
    assert update.version_notice((0, 1, 0), declared) is None

    mixed = update.manifest_versions(write_manifests(tmp_path / "b", "0.1.1", internal="0.1.0"))
    assert update.manifest_version(mixed) is None, "the manifests disagree"
    assert update.version_notice((0, 1, 1), mixed) is not None

    assert update.manifest_version({"x": "0.2.0rc1"}) is None


@pytest.mark.parametrize(
    ("title", "line"),
    [
        ("Plain title", "Plain title"),
        ("Fix `select` for setup_* methods", r"Fix \`select\` for setup\_\* methods"),
        ("See [docs](https://example.com)", r"See \[docs\](https://example.com)"),
        ("<script>alert(1)</script>", r"\<script\>alert(1)\</script\>"),
        ("Thanks @someone | ~strike~", r"Thanks \@someone \| \~strike\~"),
        ("Two\nlines​and\ttabs", "Two lines and tabs"),
        ("Dash — here", "Dash - here"),
        ("Fixes #12", "Fixes #12"),
    ],
)
def test_titles_become_inert_lines(title: str, line: str) -> None:
    assert update.plain_title(title) == line


def test_a_first_draft_lists_the_change_and_its_breaking_items() -> None:
    draft = update.merge_pull_requests(
        update.parse_body(""),
        [merged(5, title="Rename an option", content=BREAKING_CONTENT.format(n=5))],
    )
    assert update.render_body(draft, None) == (
        "## Breaking Changes\n\n"
        "### Configuration and CLI Changes\n"
        "* Option renamed - The option `--a` is now `--b` (#5)\n\n"
        "## Changes\n\n"
        "* Rename an option (#5)\n"
    )


def test_a_draft_without_breaking_changes_has_no_breaking_section() -> None:
    draft = update.merge_pull_requests(update.parse_body(""), [merged(6, title="Faster checks")])
    assert (
        update.render_body(draft, NOTICE_010)
        == f"{NOTICE_010}\n\n## Changes\n\n* Faster checks (#6)\n"
    )


def test_maintenance_and_dependency_updates_are_not_listed() -> None:
    draft = update.merge_pull_requests(
        update.parse_body(""),
        [
            merged(7, labels=("maintenance",)),
            merged(8, labels=("dependencies", "bug")),
            merged(9, labels=("bug",)),
        ],
    )
    assert update.render_body(draft, None) == "## Changes\n\n* A change (#9)\n"


def test_a_draft_with_only_unlisted_changes_is_empty() -> None:
    draft = update.merge_pull_requests(update.parse_body(""), [merged(7, labels=("maintenance",))])
    assert update.render_body(draft, None) == ""


def test_items_accumulate_across_pull_requests_in_heading_order() -> None:
    body = update.render_body(
        update.merge_pull_requests(
            update.parse_body(""), [merged(5, content=BREAKING_CONTENT.format(n=5))]
        ),
        None,
    )
    later = "### Default Rule Changes\n* Rule reports more - `MUT101` reports setup methods (#6)\n"
    body = update.render_body(
        update.merge_pull_requests(update.parse_body(body), [merged(6, content=later)]), None
    )
    assert body == (
        "## Breaking Changes\n\n"
        "### Default Rule Changes\n"
        "* Rule reports more - `MUT101` reports setup methods (#6)\n\n"
        "### Configuration and CLI Changes\n"
        "* Option renamed - The option `--a` is now `--b` (#5)\n\n"
        "## Changes\n\n"
        "* A change (#5)\n"
        "* A change (#6)\n"
    )


def test_a_pull_request_analyzed_again_replaces_its_items() -> None:
    first = update.render_body(
        update.merge_pull_requests(
            update.parse_body(""),
            [merged(5, title="Old title", content=BREAKING_CONTENT.format(n=5)), merged(6)],
        ),
        None,
    )
    again = update.merge_pull_requests(update.parse_body(first), [merged(5, title="New title")])
    assert update.render_body(again, None) == "## Changes\n\n* A change (#6)\n* New title (#5)\n"


def test_changes_to_experimental_features_accumulate_in_their_own_section() -> None:
    first = update.render_body(
        update.merge_pull_requests(
            update.parse_body(""),
            [merged(5, experimental="* `EFF101` is new - A rule (#5)\n")],
        ),
        None,
    )
    assert first == (
        "## Experimental\n\n* `EFF101` is new - A rule (#5)\n\n## Changes\n\n* A change (#5)\n"
    )
    later = update.merge_pull_requests(
        update.parse_body(first),
        [
            merged(
                6,
                content=BREAKING_CONTENT.format(n=6),
                experimental="* `EFF101` reports less - Fewer reports (#6)\n",
            )
        ],
    )
    assert update.render_body(later, None) == (
        "## Breaking Changes\n\n"
        "### Configuration and CLI Changes\n"
        "* Option renamed - The option `--a` is now `--b` (#6)\n\n"
        "## Experimental\n\n"
        "* `EFF101` is new - A rule (#5)\n"
        "* `EFF101` reports less - Fewer reports (#6)\n\n"
        "## Changes\n\n"
        "* A change (#5)\n"
        "* A change (#6)\n"
    )
    again = update.merge_pull_requests(update.parse_body(first), [merged(5)])
    assert update.render_body(again, None) == "## Changes\n\n* A change (#5)\n"


def test_experimental_changes_do_not_bump_the_minor_version(tmp_path: Path) -> None:
    declared = update.manifest_versions(write_manifests(tmp_path, "0.1.0"))
    plan, version = update.plan(
        [merged(5, experimental="* `EFF101` reports less - Fewer reports (#5)\n")],
        [released("v0.1.0")],
        declared,
    )
    assert version == (0, 1, 1)
    assert plan.breaking_prs == []
    assert plan.comments[5] == (
        f"{update.COMMENT_MARKER}\n## Breaking change analysis\n\nResult: no breaking changes found.\n\n"
        "Reasoning: Reasoning.\n\n"
        "These changes to experimental features are in the Experimental section of the draft release v0.1.1:\n\n"
        "* `EFF101` reports less - Fewer reports (#5)\n\n"
        "---\nWritten by the Release Draft workflow.\n"
    )


def test_the_body_merge_keeps_what_the_owner_wrote() -> None:
    analysis_31 = json.loads((DATA / "analysis-31.json").read_text(encoding="utf-8"))
    pulls = [
        merged(
            31,
            title=json.loads((DATA / "prs.jsonl").read_text(encoding="utf-8").splitlines()[0])[
                "title"
            ],
            content=analysis_31["breaking_changes_content"],
            experimental=analysis_31["experimental_changes_content"],
        ),
        merged(32, labels=("maintenance",)),
    ]
    draft = update.merge_pull_requests(
        update.parse_body((DATA / "draft-body.md").read_text(encoding="utf-8")), pulls
    )
    notice = update.version_notice((0, 2, 0), {"x": "0.1.0"})
    assert update.render_body(draft, notice) == (DATA / "expected-body.md").read_text(
        encoding="utf-8"
    )


def test_the_first_draft_is_created_from_main(tmp_path: Path) -> None:
    declared = update.manifest_versions(write_manifests(tmp_path, "0.1.0"))
    plan, version = update.plan([merged(5)], [released("v0.1.0")], declared)
    assert version == (0, 1, 1)
    assert plan.release_id is None
    assert plan.release == {
        "tag_name": "v0.1.1",
        "name": "v0.1.1",
        "body": f"{NOTICE_010}\n\n## Changes\n\n* A change (#5)\n",
        "draft": True,
        "target_commitish": "main",
    }
    assert plan.breaking_prs == []


def test_a_breaking_change_moves_the_draft_to_the_next_minor_version(tmp_path: Path) -> None:
    declared = update.manifest_versions(write_manifests(tmp_path, "0.1.1"))
    existing = released("v0.1.1", draft=True, body="## Changes\n\n* A change (#5)\n", id=77)
    plan, version = update.plan(
        [merged(6, title="Rename", content=BREAKING_CONTENT.format(n=6))],
        [existing, released("v0.1.0", id=1), released("v0.0.9", id=2)],
        declared,
    )
    assert version == (0, 2, 0)
    assert plan.release_id == 77
    assert plan.release["tag_name"] == plan.release["name"] == "v0.2.0"
    # An update that lands after the owner published the release must not unpublish it.
    assert "draft" not in plan.release
    assert "target_commitish" not in plan.release
    assert plan.release["body"].startswith(
        "Before publishing this draft, bump the version to 0.2.0 "
    )
    assert plan.breaking_prs == [6]
    assert plan.comments[6] == (
        f"{update.COMMENT_MARKER}\n"
        "## Breaking change analysis\n\n"
        "Result: breaking changes found. The pull request is labeled `breaking-change`.\n\n"
        "Reasoning: Reasoning.\n\n"
        "These items are in the Breaking Changes section of the draft release v0.2.0:\n\n"
        "### Configuration and CLI Changes\n"
        "* Option renamed - The option `--a` is now `--b` (#6)\n\n"
        "---\n"
        "Written by the Release Draft workflow.\n"
    )


def test_bumped_manifests_drop_the_notice(tmp_path: Path) -> None:
    declared = update.manifest_versions(write_manifests(tmp_path, "0.1.1"))
    plan, _ = update.plan([merged(5)], [released("v0.1.0")], declared)
    assert plan.release["body"] == "## Changes\n\n* A change (#5)\n"


def test_a_non_breaking_comment(tmp_path: Path) -> None:
    declared = update.manifest_versions(write_manifests(tmp_path, "0.1.0"))
    plan, _ = update.plan([merged(5)], [released("v0.1.0")], declared)
    assert plan.comments[5] == (
        f"{update.COMMENT_MARKER}\n"
        "## Breaking change analysis\n\nResult: no breaking changes found.\n\nReasoning: Reasoning.\n\n"
        "---\nWritten by the Release Draft workflow.\n"
    )


def test_a_pull_request_found_not_breaking_loses_the_label(tmp_path: Path) -> None:
    declared = update.manifest_versions(write_manifests(tmp_path, "0.1.0"))
    plan, _ = update.plan(
        [
            merged(5, labels=("breaking-change", "bug")),
            merged(6, labels=("breaking-change",), content=BREAKING_CONTENT.format(n=6)),
            merged(7, labels=("bug",)),
        ],
        [released("v0.1.0")],
        declared,
    )
    assert plan.breaking_prs == [6]
    assert plan.stale_breaking_prs == [5]


COMMENT = {"id": 900, "author": update.WORKFLOW_AUTHOR, "body": f"{update.COMMENT_MARKER}\nOld."}


@pytest.mark.parametrize(
    ("records", "expected"),
    [
        pytest.param([], None, id="no comments"),
        pytest.param([COMMENT], 900, id="the workflow's comment"),
        pytest.param([COMMENT | {"author": "someone"}], None, id="another author"),
        pytest.param([COMMENT | {"author": None}], None, id="a deleted account"),
        pytest.param(
            [COMMENT | {"body": f"Quoted: {update.COMMENT_MARKER}"}], None, id="marker not first"
        ),
        pytest.param(
            [COMMENT | {"body": "## Breaking change analysis"}], None, id="without the marker"
        ),
        pytest.param([COMMENT | {"id": 950}, COMMENT], 900, id="the oldest of two"),
    ],
)
def test_the_earlier_analysis_comment_is_found(records: list[dict], expected: int | None) -> None:
    assert update.analysis_comment_id(records) == expected


@pytest.mark.parametrize(
    "record",
    [COMMENT | {"id": True}, COMMENT | {"id": "900"}, COMMENT | {"body": None}, {"id": 900}],
)
def test_an_unexpected_comment_record_stops_the_plan(record: dict) -> None:
    with pytest.raises(SystemExit):
        update.analysis_comment_id([record])


def test_drafts_and_pre_releases_are_no_baseline(tmp_path: Path) -> None:
    declared = update.manifest_versions(write_manifests(tmp_path, "0.1.0"))
    releases = [
        released("v0.3.0", prerelease=True, id=3),
        released("v0.1.0", id=1),
        released("v9.9.9", draft=True, id=9),
    ]
    plan, version = update.plan([merged(5)], releases, declared)
    assert version == (0, 1, 1)
    assert plan.release_id == 9, "the existing draft is updated and retagged"


def test_two_version_drafts_stop_the_plan(tmp_path: Path) -> None:
    declared = update.manifest_versions(write_manifests(tmp_path, "0.1.0"))
    releases = [
        released("v0.1.0"),
        released("v0.1.1", draft=True, id=2),
        released("v0.2.0", draft=True, id=3),
    ]
    with pytest.raises(SystemExit):
        update.plan([merged(5)], releases, declared)


def test_without_a_published_release_there_is_no_plan(tmp_path: Path) -> None:
    declared = update.manifest_versions(write_manifests(tmp_path, "0.1.0"))
    with pytest.raises(SystemExit):
        update.plan([merged(5)], [released("v0.1.0", draft=True)], declared)


def run_cli(
    tmp_path: Path,
    numbers: list[str],
    releases: list[dict[str, Any]],
    *,
    prs: str | None = None,
    comments: dict[str, list[dict[str, Any]]] | None = None,
):
    analyses = tmp_path / "analyses"
    analyses.mkdir(exist_ok=True)
    for path in DATA.glob("analysis-*.json"):
        if not (analyses / path.name).exists():
            shutil.copy(path, analyses)
    comments_dir = tmp_path / "comments"
    comments_dir.mkdir(exist_ok=True)
    for number in numbers:
        (comments_dir / f"comments-{number}.jsonl").write_text(
            "".join(json.dumps(record) + "\n" for record in (comments or {}).get(number, [])),
            encoding="utf-8",
        )
    prs_path = tmp_path / "prs.jsonl"
    prs_path.write_text(
        prs if prs is not None else (DATA / "prs.jsonl").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    releases_path = tmp_path / "releases.jsonl"
    releases_path.write_text(
        "".join(json.dumps(release) + "\n" for release in releases), encoding="utf-8"
    )
    output = tmp_path / "output"
    result = run_script(
        "update_release_draft.py",
        "--pr-numbers",
        *numbers,
        "--analyses-dir",
        str(analyses),
        "--prs-path",
        str(prs_path),
        "--comments-dir",
        str(comments_dir),
        "--releases-path",
        str(releases_path),
        "--plan-dir",
        str(tmp_path / "plan"),
        "--github-output-path",
        str(output),
        "--root",
        str(write_manifests(tmp_path / "root", "0.1.0")),
    )
    return result, outputs(output)


def test_the_cli_writes_the_planned_requests(tmp_path: Path) -> None:
    body = (DATA / "draft-body.md").read_text(encoding="utf-8")
    releases = [released("v0.1.1", draft=True, body=body, id=500), released("v0.1.0", id=400)]
    # The comment bodies are untrusted text: U+0085 inside one must not split its record.
    comments = {
        "32": [
            {"id": 7, "author": "someone", "body": "Looks good\u0085to me"},
            COMMENT | {"id": 8},
        ]
    }

    result, values = run_cli(tmp_path, ["31", "32", "33"], releases, comments=comments)

    assert result.returncode == 0, result.stderr
    assert values == {
        "release_id": "500",
        "tag": "v0.2.0",
        "breaking_prs": "31",
        "stale_breaking_prs": "",
        "pr_numbers": "31 32 33",
    }
    release = json.loads((tmp_path / "plan/release.json").read_text(encoding="utf-8"))
    assert release == {
        "tag_name": "v0.2.0",
        "name": "v0.2.0",
        "body": (DATA / "expected-body.md").read_text(encoding="utf-8"),
    }
    assert {
        n: (tmp_path / f"plan/comment-{n}.id").read_text(encoding="utf-8") for n in (31, 32, 33)
    } == {31: "\n", 32: "8\n", 33: "\n"}
    comments = {
        n: json.loads((tmp_path / f"plan/comment-{n}.json").read_text(encoding="utf-8"))
        for n in (31, 32, 33)
    }
    assert comments[32]["body"].startswith(
        f"{update.COMMENT_MARKER}\n## Breaking change analysis\n\nResult: no breaking changes found."
    )
    assert "Draft release: update release 500 as v0.2.0 (version 0.2.0)." in result.stdout
    assert "----- comment on #32 (update of comment 8) -----" in result.stdout
    assert "----- comment on #33 (new comment) -----" in result.stdout
    assert "Breaking changes in: #31." in result.stdout


@pytest.mark.parametrize(
    ("numbers", "prs", "message"),
    [
        (["31", "32"], None, "do not match the resolved pull requests"),
        (["31", "31", "32"], None, "distinct positive"),
        (["31"], '{"number": 31, "title": 7, "labels": []}\n', "Unexpected pull request record"),
        (
            ["34"],
            '{"number": 34, "title": "No analysis", "labels": []}\n',
            "analysis of pull request #34 is unavailable",
        ),
    ],
)
def test_the_cli_refuses_inconsistent_inputs(
    tmp_path: Path, numbers: list[str], prs: str | None, message: str
) -> None:
    result, values = run_cli(tmp_path, numbers, [released("v0.1.0")], prs=prs)
    assert result.returncode == 1
    assert message in result.stderr
    assert values == {}
    assert not (tmp_path / "plan").exists()


def test_the_cli_validates_the_analyses_again(tmp_path: Path) -> None:
    (tmp_path / "analyses").mkdir()
    (tmp_path / "analyses/analysis-31.json").write_text(
        json.dumps(
            {
                "has_breaking_changes": True,
                "breaking_changes_content": "* Not under a heading (#31)",
                "experimental_changes_content": "",
                "reasoning": "x",
            }
        ),
        encoding="utf-8",
    )
    prs = '{"number": 31, "title": "t", "labels": []}\n'
    result, values = run_cli(tmp_path, ["31"], [released("v0.1.0")], prs=prs)
    assert result.returncode == 1
    assert "invalid breaking-change item" in result.stderr
    assert values == {}
