"""Write a release's version into the manifests, and date its CHANGELOG section.

Usage:
    python scripts/set_release_versions.py 0.1.2
    python scripts/set_release_versions.py 0.1.2 --changelog 2026-11-25

The release tag (``v`` + the version) is the only source of the version. `release.yml` runs the
script with it right after checking out the tag, so that every distribution declares it, and the
Release Sync workflow runs it on `main` with ``--changelog <publish date>`` after the release is
published, in the pull request that brings `main` to the release. It writes:

- `Cargo.toml`: ``workspace.package.version``, and the ``version`` next to the ``path`` of every
  internal crate under ``workspace.dependencies`` (crates.io needs both to agree);
- `pyproject.toml`: ``project.version``;
- `Cargo.lock`: the version of every workspace member, without registry access, so that a build
  with ``--locked`` accepts the lock file;
- with ``--changelog DATE``, `CHANGELOG.md`: the first ``## [VERSION] - Unreleased`` heading
  becomes ``## [VERSION] - DATE``, and the ``[VERSION]`` link, a compare that ends at ``HEAD``,
  ends at the release tag instead (a missing one compares the previous release with the tag, or
  shows the tag of a first release). When a section is headed ``[Unreleased]``, the
  ``[Unreleased]`` link compares the tag with ``HEAD``. The hand-written entries and links stay as
  they are. A release older than the version the manifests declare
  changes nothing: `main` has moved past it (a backport published late, or an old release synced
  again).

Each edit rewrites one value found by an exact pattern: a pattern that does not match exactly once
stops the script before it writes any file, and so does a manifest that the edits leave with
another version. A second run with the same version changes nothing.
"""

from __future__ import annotations

import argparse
import datetime
import re
import sys
import tomllib
from pathlib import Path
from typing import Any, NoReturn

ROOT = Path(__file__).resolve().parent.parent

RELEASE_VERSION = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(alpha|beta|rc)\.(0|[1-9][0-9]*))?"
)
"""A version that Cargo (SemVer) and PyPI (PEP 440) both accept, and order alike."""
PRE_RELEASES = ("alpha", "beta", "rc")
ISO_DATE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}")

TOML_TABLE_HEADER = re.compile(r"^\[", re.MULTILINE)
VERSION_LINE = re.compile(r'^version = "(?P<value>[^"]*)"$', re.MULTILINE)
UNRELEASED_HEADING = re.compile(r"^## \[(?P<version>[^\]]+)\] - Unreleased$", re.MULTILINE)
RELEASED_HEADING = re.compile(
    r"^## \[(?P<version>[^\]]+)\] - [0-9]{4}-[0-9]{2}-[0-9]{2}$", re.MULTILINE
)
UNRELEASED_SECTION = re.compile(r"^## \[Unreleased\]", re.MULTILINE | re.IGNORECASE)
LINK_DEFINITION = re.compile(r"^\[(?P<label>[^\]]+)\]: (?P<url>\S+)$", re.MULTILINE)

WORKSPACE_VERSION = "Cargo.toml `workspace.package.version`"


def fail(message: str) -> NoReturn:
    sys.exit(f"cannot set the release version: {message}")


def version_order(version: str, where: str) -> tuple[int, ...]:
    """The sort key of a release version: a pre-release comes before its final release."""
    match = RELEASE_VERSION.fullmatch(version)
    if match is None:
        fail(f"{where}: {version!r} is not a release version")
    major, minor, patch, _, kind, number = match.groups()
    pre = (PRE_RELEASES.index(kind), int(number)) if kind else (len(PRE_RELEASES), 0)
    return int(major), int(minor), int(patch), *pre


def declared_versions(pyproject: dict[str, Any], cargo: dict[str, Any]) -> dict[str, str]:
    """The version each manifest entry declares, by entry."""
    declared: dict[str, str] = {
        "pyproject.toml `project.version`": pyproject["project"]["version"],
        WORKSPACE_VERSION: cargo["workspace"]["package"]["version"],
    }
    for name, dep in cargo["workspace"]["dependencies"].items():
        if isinstance(dep, dict) and "path" in dep:
            declared[f"Cargo.toml `workspace.dependencies.{name}`"] = dep.get("version", "(none)")
    return declared


def manifest_versions(root: Path = ROOT) -> dict[str, str]:
    """The version each manifest entry of a checkout declares; the release draft reads it."""
    return declared_versions(
        tomllib.loads((root / "pyproject.toml").read_text(encoding="utf-8")),
        tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8")),
    )


def replace_value(
    text: str, pattern: re.Pattern[str], value: str, where: str, span: tuple[int, int] | None = None
) -> str:
    """Replace the ``value`` group of the one match of `pattern` (within `span`)."""
    start, end = span or (0, len(text))
    matches = list(pattern.finditer(text, start, end))
    if len(matches) != 1:
        fail(f"{where}: expected one line matching `{pattern.pattern}`, found {len(matches)}")
    match = matches[0]
    return text[: match.start("value")] + value + text[match.end("value") :]


def table_span(text: str, header: str, file: str) -> tuple[int, int]:
    """The lines of a TOML table: from the end of its header to the next table's header."""
    headers = list(re.finditer(rf"^\[{re.escape(header)}\]$", text, re.MULTILINE))
    if len(headers) != 1:
        fail(f"{file}: expected one `[{header}]` table, found {len(headers)}")
    start = headers[0].end()
    following = TOML_TABLE_HEADER.search(text, start)
    return start, following.start() if following else len(text)


def workspace_members(root: Path, cargo: dict[str, Any]) -> list[str]:
    """The package names of the workspace members, each of which must inherit the version."""
    names = []
    for member in cargo["workspace"]["members"]:
        manifest = f"{member}/Cargo.toml"
        package = tomllib.loads((root / manifest).read_text(encoding="utf-8"))["package"]
        if package.get("version") != {"workspace": True}:
            fail(
                f"{manifest}: the version must come from the workspace (`version.workspace = true`)"
            )
        names.append(package["name"])
    return names


def cargo_toml(text: str, version: str, internal: list[str]) -> str:
    text = replace_value(
        text,
        VERSION_LINE,
        version,
        "Cargo.toml `[workspace.package]`",
        table_span(text, "workspace.package", "Cargo.toml"),
    )
    for name in internal:
        dependency = re.compile(
            rf'^{re.escape(name)} = \{{ path = "[^"]*", version = "(?P<value>[^"]*)" \}}$',
            re.MULTILINE,
        )
        span = table_span(text, "workspace.dependencies", "Cargo.toml")
        text = replace_value(text, dependency, version, f"Cargo.toml `{name}`", span)
    return text


def pyproject_toml(text: str, version: str) -> str:
    span = table_span(text, "project", "pyproject.toml")
    return replace_value(text, VERSION_LINE, version, "pyproject.toml `[project]`", span)


def cargo_lock(text: str, version: str, members: list[str]) -> str:
    """Set the version of each workspace member's entry, the one without a `source`."""
    for name in members:
        entry = re.compile(
            rf'^\[\[package\]\]\nname = "{re.escape(name)}"\nversion = "(?P<value>[^"]*)"\n'
            r"(?!source = )",
            re.MULTILINE,
        )
        text = replace_value(text, entry, version, f"Cargo.lock `{name}`")
    return text


def check_written(pyproject: str, cargo: str, lock: str, version: str, members: list[str]) -> None:
    """Fail unless every version a build reads is `version`: the edits missed no entry."""
    declared = declared_versions(tomllib.loads(pyproject), tomllib.loads(cargo))
    local = {
        package["name"]: package["version"]
        for package in tomllib.loads(lock)["package"]
        if "source" not in package
    }
    if sorted(local) != sorted(members):
        fail(
            f"Cargo.lock: the entries without a source are {sorted(local)}, not the workspace "
            f"members {sorted(members)}"
        )
    declared |= {f"Cargo.lock `{name}`": value for name, value in local.items()}
    if wrong := {where: value for where, value in declared.items() if value != version}:
        listing = "\n".join(f"  {where}: {value}" for where, value in wrong.items())
        fail(f"these entries do not declare {version}:\n{listing}")


def link_definition(text: str, label: str) -> re.Match[str] | None:
    """The `[label]: <url>` line, if the CHANGELOG has one."""
    named = [m for m in LINK_DEFINITION.finditer(text) if m["label"].lower() == label.lower()]
    if len(named) > 1:
        fail(f"CHANGELOG.md: [{label}] is defined {len(named)} times")
    return named[0] if named else None


def with_url(text: str, definition: re.Match[str], url: str) -> str:
    return text[: definition.start("url")] + url + text[definition.end("url") :]


def with_new_link(text: str, label: str, url: str) -> str:
    """Add a link definition: `[Unreleased]` first in the list, a version first after it."""
    definitions = list(LINK_DEFINITION.finditer(text))
    line = f"[{label}]: {url}\n"
    before = [m for m in definitions if label == "Unreleased" or m["label"].lower() != "unreleased"]
    if before:
        return text[: before[0].start()] + line + text[before[0].start() :]
    if definitions:
        end = definitions[-1].end()
        return text[:end] + "\n" + line.rstrip("\n") + text[end:]
    return text.rstrip("\n") + "\n\n" + line


def released_link(text: str, version: str, heading: re.Match[str], repository: str) -> str:
    """Make the version's link end at its tag; a link that ends elsewhere is the author's."""
    tag = f"v{version}"
    definition = link_definition(text, version)
    if definition is None:
        previous = RELEASED_HEADING.search(text, heading.end())
        if previous is None:
            return with_new_link(text, version, f"{repository}/releases/tag/{tag}")
        return with_new_link(text, version, f"{repository}/compare/v{previous['version']}...{tag}")
    if definition["url"].endswith("...HEAD"):
        return with_url(text, definition, definition["url"].removesuffix("HEAD") + tag)
    return text


def dated_changelog(text: str, version: str, date: str, repository: str) -> str:
    """Date the version's section and point its links at the release."""
    released = [m for m in RELEASED_HEADING.finditer(text) if m["version"] == version]
    if not released:
        first = UNRELEASED_HEADING.search(text)
        if first is None:
            fail(f"CHANGELOG.md: no `## [{version}] - Unreleased` heading to date")
        if first["version"] != version:
            found = first["version"]
            fail(f"CHANGELOG.md: the first unreleased section is [{found}], not [{version}]")
        text = text[: first.start()] + f"## [{version}] - {date}" + text[first.end() :]
        released = [m for m in RELEASED_HEADING.finditer(text) if m["version"] == version]
    text = released_link(text, version, released[0], repository)
    if UNRELEASED_SECTION.search(text):
        url = f"{repository}/compare/v{version}...HEAD"
        unreleased = link_definition(text, "Unreleased")
        text = (
            with_new_link(text, "Unreleased", url)
            if unreleased is None
            else with_url(text, unreleased, url)
        )
    return text


def release_files(root: Path, version: str, changelog_date: str | None) -> dict[Path, str]:
    """The new text of every file the release changes, checked before anything is written."""
    texts = {
        name: (root / name).read_text(encoding="utf-8")
        for name in ("Cargo.toml", "pyproject.toml", "Cargo.lock")
    }
    cargo = tomllib.loads(texts["Cargo.toml"])
    internal = [
        name
        for name, dep in cargo["workspace"]["dependencies"].items()
        if isinstance(dep, dict) and "path" in dep
    ]
    members = workspace_members(root, cargo)
    written = {
        "Cargo.toml": cargo_toml(texts["Cargo.toml"], version, internal),
        "pyproject.toml": pyproject_toml(texts["pyproject.toml"], version),
        "Cargo.lock": cargo_lock(texts["Cargo.lock"], version, members),
    }
    check_written(
        written["pyproject.toml"], written["Cargo.toml"], written["Cargo.lock"], version, members
    )
    if changelog_date is not None:
        repository = cargo["workspace"]["package"]["repository"].rstrip("/")
        changelog = (root / "CHANGELOG.md").read_text(encoding="utf-8")
        written["CHANGELOG.md"] = dated_changelog(changelog, version, changelog_date, repository)
    return {root / name: text for name, text in written.items()}


def release_date(text: str) -> str:
    try:
        if ISO_DATE.fullmatch(text):
            return datetime.date.fromisoformat(text).isoformat()
    except ValueError:
        pass
    raise argparse.ArgumentTypeError(f"not a date in the form YYYY-MM-DD: {text!r}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("version", help="the release version: the tag without its `v`")
    parser.add_argument(
        "--changelog",
        metavar="DATE",
        type=release_date,
        help="also date the version's CHANGELOG.md section (the publish date, YYYY-MM-DD)",
    )
    parser.add_argument("--root", type=Path, default=ROOT, help="the checkout to write")
    args = parser.parse_args(argv)
    if not RELEASE_VERSION.fullmatch(args.version):
        fail(
            f"{args.version!r} is not a version in the form MAJOR.MINOR.PATCH, optionally "
            "followed by -alpha.N, -beta.N, or -rc.N"
        )
    if args.changelog is not None:
        declared = manifest_versions(args.root)[WORKSPACE_VERSION]
        if version_order(declared, WORKSPACE_VERSION) > version_order(args.version, "the release"):
            print(f"{args.version}: the manifests declare the later {declared}; nothing to change.")
            return 0
    changed = []
    for path, text in release_files(args.root, args.version, args.changelog).items():
        if path.read_text(encoding="utf-8") != text:
            path.write_text(text, encoding="utf-8")
            changed.append(path.name)
    print(f"{args.version}: {'wrote ' + ', '.join(changed) if changed else 'nothing to change'}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
