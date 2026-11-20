"""Plan the draft release, labels, and comments for the analyzed pull requests; write nothing to GitHub.

The draft accumulates: its ``## Breaking Changes`` section keeps the items of every analyzed pull
request under their headings, and ``## Changes`` lists every merged pull request as
``* <title> (#<number>)``, except those labeled `maintenance` or `dependencies`. The items and the
line of a pull request analyzed again replace the earlier ones. Other text in the draft is kept.

The next version follows 0.x semantics from the newest published release: a breaking change bumps
the minor version (0.1.x -> 0.2.0) and anything else the patch. When the manifests already name a
later version, the draft takes it. The draft's tag is ``v<version>``, and its body starts with a
line asking for the manifests to be bumped to that version while they declare another.

The plan directory gets ``release.json`` (the body of the create or update request),
``comment-<number>.json`` for each pull request, and the step outputs (``release_id``, empty for
a new draft; ``tag``; ``breaking_prs``; ``pr_numbers``). The workflow's write steps send them.
"""

from __future__ import annotations

import argparse
import json
import re
import unicodedata
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from release_check import manifest_versions
from release_draft_common import (
    BREAKING_CHANGE_HEADINGS,
    BREAKING_LABEL,
    UNLISTED_LABELS,
    Version,
    fail,
    format_version,
    json_lines,
    parse_version,
    published_versions,
)
from validate_release_draft_analysis import (
    EM_DASH,
    validate_content_format,
    validate_reasoning,
    validate_structured_output,
)

ROOT = Path(__file__).resolve().parent.parent
BREAKING_SECTION = "## Breaking Changes"
CHANGES_SECTION = "## Changes"
NOTICE_PREFIX = "Before publishing this draft, bump the version to "
MARKDOWN_PUNCTUATION = re.compile(r"([\\`*_\[\]<>|~@])")
INVISIBLE_CATEGORIES = frozenset({"Cc", "Cf", "Cs", "Zl", "Zp"})


@dataclass(frozen=True)
class Analysis:
    """A validated analysis of one pull request."""

    has_breaking_changes: bool
    breaking_changes_content: str
    reasoning: str


@dataclass(frozen=True)
class MergedPullRequest:
    """An analyzed pull request with the title and labels the draft lists it by."""

    number: int
    title: str
    labels: frozenset[str]
    analysis: Analysis


@dataclass
class DraftBody:
    """The parts of a draft release body the merge reads and writes."""

    preamble: list[str] = field(default_factory=list)
    breaking: dict[str, list[str]] = field(default_factory=dict)
    changes: list[str] = field(default_factory=list)
    other_sections: list[list[str]] = field(default_factory=list)

    def has_breaking_changes(self) -> bool:
        return any(self.breaking.values())


@dataclass(frozen=True)
class Plan:
    """What the write steps send: the draft release and one comment per pull request."""

    release_id: int | None
    release: dict[str, Any]
    comments: dict[int, str]
    breaking_prs: list[int]


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--pr-numbers", required=True, nargs="+", type=int, help="in merge order")
    parser.add_argument("--analyses-dir", required=True, type=Path)
    parser.add_argument("--prs-path", required=True, type=Path)
    parser.add_argument("--releases-path", required=True, type=Path)
    parser.add_argument("--plan-dir", required=True, type=Path)
    parser.add_argument("--github-output-path", required=True, type=Path)
    parser.add_argument(
        "--root", default=ROOT, type=Path, help="the checkout whose manifests are read"
    )
    return parser.parse_args(argv)


def plain_title(title: str) -> str:
    """Return a pull request title as one line of inert Markdown that renders as the title's text."""
    text = "".join(
        " " if unicodedata.category(character) in INVISIBLE_CATEGORIES else character
        for character in title
    )
    text = " ".join(text.replace(EM_DASH, "-").split())
    return MARKDOWN_PUNCTUATION.sub(r"\\\1", text)


def _ends_with_pr(line: str, number: int) -> bool:
    return line.rstrip().endswith(f"(#{number})")


def parse_body(body: str) -> DraftBody:
    """Split a draft body into its sections; the version notice is dropped, since it is written anew."""
    draft = DraftBody()
    section: list[str] | None = draft.preamble
    breaking_heading: str | None = None
    for line in body.replace("\r\n", "\n").split("\n"):
        if line.startswith("## "):
            breaking_heading = None
            if line.rstrip() == BREAKING_SECTION:
                section = None
            elif line.rstrip() == CHANGES_SECTION:
                section = draft.changes
            else:
                draft.other_sections.append([line])
                section = draft.other_sections[-1]
            continue
        if section is None:
            if line.startswith("### "):
                breaking_heading = line.removeprefix("### ").strip()
                draft.breaking.setdefault(breaking_heading, [])
            elif line.strip():
                draft.breaking.setdefault(breaking_heading or "", []).append(line)
        elif section is draft.preamble:
            if not line.startswith(NOTICE_PREFIX):
                section.append(line)
        elif section is draft.changes:
            if line.strip():
                section.append(line)
        else:
            section.append(line)
    return draft


def breaking_items(content: str) -> dict[str, list[str]]:
    """Return the validated items of one analysis by heading."""
    items: dict[str, list[str]] = {}
    heading = ""
    for line in content.splitlines():
        if line.startswith("### "):
            heading = line.removeprefix("### ")
        elif line.strip():
            items.setdefault(heading, []).append(line)
    return items


def merge_pull_requests(draft: DraftBody, pulls: list[MergedPullRequest]) -> DraftBody:
    """Add each pull request's breaking items and change line, replacing those of an earlier analysis."""
    for pull in pulls:
        for heading, lines in draft.breaking.items():
            draft.breaking[heading] = [
                line for line in lines if not _ends_with_pr(line, pull.number)
            ]
        draft.changes = [line for line in draft.changes if not _ends_with_pr(line, pull.number)]
        if pull.analysis.has_breaking_changes:
            for heading, items in breaking_items(pull.analysis.breaking_changes_content).items():
                draft.breaking.setdefault(heading, []).extend(items)
        if not pull.labels & UNLISTED_LABELS:
            draft.changes.append(f"* {plain_title(pull.title)} (#{pull.number})")
    return draft


def _heading_order(heading: str, position: int) -> tuple[int, int]:
    """Known headings in their fixed order, then any other heading where the draft had it."""
    if heading in BREAKING_CHANGE_HEADINGS:
        return 0, BREAKING_CHANGE_HEADINGS.index(heading)
    return 1, position


def render_body(draft: DraftBody, notice: str | None) -> str:
    """Return the draft body: the notice, other leading text, the two sections, then any other section."""
    parts = [notice] if notice else []
    if preamble := "\n".join(draft.preamble).strip():
        parts.append(preamble)
    if draft.has_breaking_changes():
        parts.append(BREAKING_SECTION)
        ordered = sorted(
            enumerate(draft.breaking.items()), key=lambda item: _heading_order(item[1][0], item[0])
        )
        for _, (heading, lines) in ordered:
            if lines:
                parts.append("\n".join(([f"### {heading}"] if heading else []) + lines))
    if draft.changes:
        parts += [CHANGES_SECTION, "\n".join(draft.changes)]
    parts += [text for section in draft.other_sections if (text := "\n".join(section).strip())]
    return "\n\n".join(parts) + "\n" if parts else ""


def next_version(latest: Version, *, breaking: bool, manifest: Version | None) -> Version:
    """Return the version after the newest published release, or the manifests' version when later."""
    major, minor, patch = latest
    if breaking:
        computed = (major, minor + 1, 0) if major == 0 else (major + 1, 0, 0)
    else:
        computed = (major, minor, patch + 1)
    return max(computed, manifest) if manifest else computed


def manifest_version(declared: dict[str, str]) -> Version | None:
    """Return the version the manifests agree on, or None when they differ or name a pre-release."""
    values = set(declared.values())
    return parse_version(values.pop()) if len(values) == 1 else None


def version_notice(version: Version, declared: dict[str, str]) -> str | None:
    """Return the body's first line while a manifest declares another version than the draft's."""
    if all(value == format_version(version) for value in declared.values()):
        return None
    return (
        f"{NOTICE_PREFIX}{format_version(version)} in a pull request and merge it: "
        "`pyproject.toml`, `Cargo.toml` (`workspace.package.version` and the internal crates under "
        "`workspace.dependencies`), and `Cargo.lock`."
    )


def _validated_release(record: Any) -> dict[str, Any]:
    if (
        isinstance(record, dict)
        and type(record.get("id")) is int
        and isinstance(record.get("tag_name"), str)
        and isinstance(record.get("draft"), bool)
        and isinstance(record.get("prerelease"), bool)
        and (record.get("body") is None or isinstance(record["body"], str))
    ):
        return record
    fail("Unexpected release record.")


def draft_release(releases: list[dict[str, Any]]) -> dict[str, Any] | None:
    """Return the one draft release named by a version tag, None when there is none."""
    drafts = [
        release for release in releases if release["draft"] and parse_version(release["tag_name"])
    ]
    if len(drafts) > 1:
        fail("More than one draft release is named by a version; delete all but one.")
    return drafts[0] if drafts else None


def comment_body(pull: MergedPullRequest, tag: str) -> str:
    """Return the comment that reports the analysis on the pull request."""
    analysis = pull.analysis
    lines = ["## Breaking change analysis", ""]
    if analysis.has_breaking_changes:
        lines += [
            f"Result: breaking changes found. The pull request is labeled `{BREAKING_LABEL}`.",
            "",
        ]
    else:
        lines += ["Result: no breaking changes found.", ""]
    lines += [f"Reasoning: {analysis.reasoning}", ""]
    if analysis.has_breaking_changes:
        lines += [
            f"These items are in the Breaking Changes section of the draft release {tag}:",
            "",
            analysis.breaking_changes_content.strip(),
            "",
        ]
    lines += ["---", "Written by the Release Draft workflow."]
    return "\n".join(lines) + "\n"


def plan(
    pulls: list[MergedPullRequest], releases: list[dict[str, Any]], declared: dict[str, str]
) -> tuple[Plan, Version]:
    """Return the requests that update the draft and comment on each pull request."""
    if not (published := published_versions(releases)):
        fail("No release is published, so the draft has no version to follow.")
    existing = draft_release(releases)
    draft = merge_pull_requests(parse_body((existing or {}).get("body") or ""), pulls)
    version = next_version(
        max(published), breaking=draft.has_breaking_changes(), manifest=manifest_version(declared)
    )
    tag = f"v{format_version(version)}"
    release: dict[str, Any] = {
        "tag_name": tag,
        "name": tag,
        "body": render_body(draft, version_notice(version, declared)),
        "draft": True,
    }
    if existing is None:
        release["target_commitish"] = "main"
    return (
        Plan(
            release_id=existing["id"] if existing else None,
            release=release,
            comments={pull.number: comment_body(pull, tag) for pull in pulls},
            breaking_prs=[pull.number for pull in pulls if pull.analysis.has_breaking_changes],
        ),
        version,
    )


def load_analysis(path: Path, number: int) -> Analysis:
    """Load an analysis the analyze job validated, and check its structure again."""
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        fail(f"The analysis of pull request #{number} is unavailable.")
    if not isinstance(raw, dict):
        fail(f"The analysis of pull request #{number} is invalid.")
    has_breaking_changes, content, reasoning = validate_structured_output(raw)
    validate_reasoning(reasoning)
    # The analyze job checked the removal claims against the exact diff, which this job does not have.
    validate_content_format(
        has_breaking_changes=has_breaking_changes,
        breaking_changes_content=content,
        pr_number=number,
    )
    return Analysis(has_breaking_changes, content, reasoning)


def load_pull_requests(
    numbers: list[int], prs_path: Path, analyses_dir: Path
) -> list[MergedPullRequest]:
    """Pair each pull request's title and labels with its analysis, in merge order."""
    records = json_lines(prs_path.read_text(encoding="utf-8"))
    if [
        record.get("number") if isinstance(record, dict) else None for record in records
    ] != numbers:
        fail("The pull request records do not match the resolved pull requests.")
    pulls = []
    for record in records:
        match record:
            case {"number": int(number), "title": str(title), "labels": list(labels)} if all(
                isinstance(label, str) for label in labels
            ):
                analysis = load_analysis(analyses_dir / f"analysis-{number}.json", number)
                pulls.append(MergedPullRequest(number, title, frozenset(labels), analysis))
            case _:
                fail("Unexpected pull request record.")
    return pulls


def write_plan(plan_dir: Path, result: Plan) -> None:
    """Write the request bodies the write steps send."""
    plan_dir.mkdir(parents=True, exist_ok=True)
    (plan_dir / "release.json").write_text(
        json.dumps(result.release, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    for number, body in result.comments.items():
        (plan_dir / f"comment-{number}.json").write_text(
            json.dumps({"body": body}, ensure_ascii=False) + "\n", encoding="utf-8"
        )


def main(argv: list[str] | None = None) -> None:
    """Plan the draft update, print it, and write the request bodies and step outputs."""
    args = _parse_args(argv)
    if len(set(args.pr_numbers)) != len(args.pr_numbers) or min(args.pr_numbers) < 1:
        fail("Expected distinct positive pull request numbers.")
    pulls = load_pull_requests(args.pr_numbers, args.prs_path, args.analyses_dir)
    releases = [
        _validated_release(record)
        for record in json_lines(args.releases_path.read_text(encoding="utf-8"))
    ]
    result, version = plan(pulls, releases, manifest_versions(args.root))
    write_plan(args.plan_dir, result)
    action = "create" if result.release_id is None else f"update release {result.release_id} as"
    print(
        f"Draft release: {action} {result.release['tag_name']} (version {format_version(version)})."
    )
    print(
        f"Breaking changes in: {', '.join(f'#{n}' for n in result.breaking_prs) or 'none of these'}."
    )
    print("----- draft body -----", result.release["body"], sep="\n")
    for number, body in result.comments.items():
        print(f"----- comment on #{number} -----", body, sep="\n")
    with args.github_output_path.open("a", encoding="utf-8") as output:
        output.write(f"release_id={'' if result.release_id is None else result.release_id}\n")
        output.write(f"tag={result.release['tag_name']}\n")
        output.write(f"breaking_prs={' '.join(map(str, result.breaking_prs))}\n")
        output.write(f"pr_numbers={' '.join(map(str, args.pr_numbers))}\n")


if __name__ == "__main__":
    main()
