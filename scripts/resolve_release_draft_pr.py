"""Resolve the merged pull requests the Release Draft workflow analyzes, and the base of each diff.

`push` looks up every first-parent commit a push to `main` added with
`commits/{sha}/pulls` and keeps the pull requests merged at that very commit. One push can merge
several (a stack merges from the bottom up, each pull request as its own commit), and the
outputs keep their merge order. `replay` takes one merged pull request whose merge commit is in
the trusted history.

The diff of a pull request starts at the first parent of its merge commit. A rebase merge puts
several commits on `main`, and GitHub links each of them to the pull request: the base is then
the commit before that run. A pull request already labeled `breaking-change-analyzed` is
skipped, except by a dry run.

Nothing is resolved while the repository has no published release: there is no release to
compare with, and the first release's notes come from `CHANGELOG.md`.

The outputs (`prs`, `pr_numbers`) hold only validated numbers and commit SHAs.
"""

from __future__ import annotations

import argparse
import json
import subprocess
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

from release_draft_common import (
    ANALYZED_LABEL,
    COMMIT_SHA,
    PR_NUMBER,
    fail,
    gh_api,
    gh_api_json,
    json_lines,
    published_versions,
)

MAIN_REF = "refs/heads/main"
MAIN_REMOTE_REF = "refs/remotes/origin/main"
MAX_PUSHED_COMMITS = 100
MAX_REBASED_COMMITS = 250
"""Each step back through a rebase merge costs one API request."""


@dataclass(frozen=True)
class MergedPullRequest:
    """A merged pull request and the commits its diff spans."""

    number: int
    merge_sha: str
    base_sha: str


@dataclass(frozen=True)
class PullRequest:
    """The validated fields of a `pulls/{number}` response."""

    number: int
    merge_sha: str
    commits: int
    analyzed: bool


def _sha_argument(value: str) -> str:
    if not COMMIT_SHA.fullmatch(value):
        msg = "expected a full commit SHA"
        raise argparse.ArgumentTypeError(msg)
    return value


def _pr_number_argument(value: str) -> int:
    if not PR_NUMBER.fullmatch(value):
        msg = "expected a positive pull request number"
        raise argparse.ArgumentTypeError(msg)
    return int(value)


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repository", required=True)
    parser.add_argument("--github-output-path", required=True, type=Path)
    modes = parser.add_subparsers(dest="mode", required=True)
    push = modes.add_parser("push", help="the pull requests a push to main merged")
    push.add_argument("--before", required=True, type=_sha_argument)
    push.add_argument("--after", required=True, type=_sha_argument)
    replay = modes.add_parser("replay", help="one merged pull request")
    replay.add_argument("--pr-number", required=True, type=_pr_number_argument)
    replay.add_argument("--ref", required=True, help="the ref the workflow runs on")
    replay.add_argument("--trusted-sha", required=True, type=_sha_argument)
    replay.add_argument("--dry-run", action="store_true")
    return parser.parse_args(argv)


def _git(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["git", *args], capture_output=True, check=False, text=True)


def _parents(commit: str) -> list[str]:
    result = _git("rev-list", "--parents", "-n", "1", commit)
    if result.returncode:
        fail("A commit of the pull request is missing from the checkout.")
    return result.stdout.split()[1:]


def _pushed_commits(before: str, after: str) -> list[str]:
    """Return the first-parent commits `before..after`, oldest first."""
    if set(before) == {"0"}:
        fail("The push created the branch; replay its pull requests instead.")
    result = _git("rev-list", "--first-parent", "--reverse", f"{before}..{after}")
    if result.returncode:
        fail(
            "The commits of this push cannot be listed (a force push?); replay its pull requests instead."
        )
    commits = result.stdout.split()
    if len(commits) > MAX_PUSHED_COMMITS:
        fail(
            f"The push added more than {MAX_PUSHED_COMMITS} commits; replay its pull requests instead."
        )
    return commits


def _commit_pulls(repository: str, commit: str) -> list[dict[str, Any]]:
    """Return the merged pull requests of this repository that GitHub links to a commit."""
    pulls = gh_api_json(f"/repos/{repository}/commits/{commit}/pulls?per_page=100")
    if not isinstance(pulls, list):
        fail("Unexpected response for the pull requests of a commit.")
    return [
        pull
        for pull in pulls
        if isinstance(pull, dict)
        and type(pull.get("number")) is int
        and isinstance(pull.get("merged_at"), str)
        and pull["merged_at"]
        and isinstance(base := pull.get("base"), dict)
        and isinstance(base_repository := base.get("repo"), dict)
        and base_repository.get("full_name") == repository
    ]


def pull_merged_at(commit: str, pulls: list[dict[str, Any]]) -> int | None:
    """Return the number of the pull request merged at exactly this commit, if any.

    Its base branch name is not checked: a pull request of a stack can still name the branch
    below it when the stack merges.
    """
    numbers = {pull["number"] for pull in pulls if pull.get("merge_commit_sha") == commit}
    if len(numbers) > 1:
        fail(f"Several pull requests name {commit} as their merge commit.")
    return numbers.pop() if numbers else None


def validate_pull_request(payload: Any, number: int, repository: str) -> PullRequest:
    """Admit only a merged pull request of this repository with well-formed metadata."""
    match payload:
        case {
            "number": int(payload_number),
            "merged": True,
            "merge_commit_sha": str(merge_sha),
            "commits": int(commits),
            "base": {"repo": {"full_name": str(base_repository)}},
            "labels": list(labels),
        } if (
            type(payload_number) is int
            and payload_number == number
            and base_repository == repository
            and COMMIT_SHA.fullmatch(merge_sha)
            and type(commits) is int
            and commits >= 1
            and all(
                isinstance(label, dict) and isinstance(label.get("name"), str) for label in labels
            )
        ):
            return PullRequest(
                number=number,
                merge_sha=merge_sha,
                commits=commits,
                analyzed=any(label["name"] == ANALYZED_LABEL for label in labels),
            )
    fail(f"Expected pull request #{number} to be merged into this repository, with valid metadata.")


def diff_base(repository: str, pull: PullRequest) -> str:
    """Return the commit the diff of a merged pull request starts from."""
    parents = _parents(pull.merge_sha)
    if not parents:
        fail("A merge commit without a parent has no diff base.")
    base, steps = parents[0], 1
    if len(parents) > 1:
        return base
    while steps < pull.commits and pull.number in {
        p["number"] for p in _commit_pulls(repository, base)
    }:
        if steps >= MAX_REBASED_COMMITS:
            fail(
                f"Pull request #{pull.number} was rebased onto more than {MAX_REBASED_COMMITS} commits."
            )
        parents = _parents(base)
        if len(parents) != 1:
            fail(f"A rebased commit of pull request #{pull.number} is a merge commit.")
        base, steps = parents[0], steps + 1
    return base


def _merged_pull_request(repository: str, pull: PullRequest) -> MergedPullRequest:
    return MergedPullRequest(pull.number, pull.merge_sha, diff_base(repository, pull))


def _fetch_pull_request(repository: str, number: int) -> PullRequest:
    return validate_pull_request(
        gh_api_json(f"/repos/{repository}/pulls/{number}"), number, repository
    )


def _has_published_release(repository: str) -> bool:
    releases = json_lines(
        gh_api(
            f"/repos/{repository}/releases?per_page=100",
            paginate=True,
            jq=".[] | {tag_name, draft, prerelease}",
        )
    )
    return bool(published_versions(release for release in releases if isinstance(release, dict)))


def resolve_push(repository: str, before: str, after: str) -> list[MergedPullRequest]:
    """Return the pull requests a push merged, in merge order, without those already analyzed."""
    resolved = []
    for commit in _pushed_commits(before, after):
        if (number := pull_merged_at(commit, _commit_pulls(repository, commit))) is None:
            continue
        pull = _fetch_pull_request(repository, number)
        if pull.merge_sha != commit:
            fail(f"Pull request #{number} names another merge commit than the one it was found at.")
        if pull.analyzed:
            print(f"Pull request #{number} is labeled {ANALYZED_LABEL}; skipping it.")
            continue
        resolved.append(_merged_pull_request(repository, pull))
    return resolved


def _history_sha(ref: str, trusted_sha: str, *, dry_run: bool) -> str:
    """Return the commit whose history must contain a replayed pull request's merge commit."""
    if ref == MAIN_REF:
        return trusted_sha
    if not dry_run:
        fail(
            "A replay that changes the draft runs on main only; set dry_run to analyze from another branch."
        )
    result = _git("rev-parse", "--verify", f"{MAIN_REMOTE_REF}^{{commit}}")
    if result.returncode:
        fail("The checkout has no copy of main.")
    return result.stdout.strip()


def resolve_replay(
    repository: str, number: int, ref: str, trusted_sha: str, *, dry_run: bool
) -> list[MergedPullRequest]:
    """Return the requested pull request when its merge commit is in the trusted history."""
    history_sha = _history_sha(ref, trusted_sha, dry_run=dry_run)
    pull = _fetch_pull_request(repository, number)
    if _git("merge-base", "--is-ancestor", pull.merge_sha, history_sha).returncode:
        fail(f"The merge commit of pull request #{number} is not in the history of main.")
    if pull.analyzed and not dry_run:
        print(
            f"Pull request #{number} is labeled {ANALYZED_LABEL}; remove the label to analyze it again."
        )
        return []
    return [_merged_pull_request(repository, pull)]


def _write_outputs(path: Path, resolved: list[MergedPullRequest]) -> None:
    prs = json.dumps([asdict(pull) for pull in resolved], separators=(",", ":"))
    with path.open("a", encoding="utf-8") as output:
        output.write(f"prs={prs}\n")
        output.write(f"pr_numbers={' '.join(str(pull.number) for pull in resolved)}\n")


def main(argv: list[str] | None = None) -> None:
    """Validate every input before writing the workflow outputs."""
    args = _parse_args(argv)
    if not _has_published_release(args.repository):
        print(
            "::notice title=Release Draft::No release is published yet, so there is nothing to compare "
            "with; the first release's notes come from CHANGELOG.md. Skipping the analysis and the draft."
        )
        resolved = []
    elif args.mode == "push":
        resolved = resolve_push(args.repository, args.before, args.after)
    else:
        resolved = resolve_replay(
            args.repository, args.pr_number, args.ref, args.trusted_sha, dry_run=args.dry_run
        )
    for pull in resolved:
        print(
            f"Pull request #{pull.number}: merged at {pull.merge_sha}, diff from {pull.base_sha}."
        )
    if not resolved:
        print("No pull request to analyze.")
    _write_outputs(args.github_output_path, resolved)


if __name__ == "__main__":
    main()
