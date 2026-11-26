"""Tests of `set_release_versions.py`: the manifests, `Cargo.lock`, and the CHANGELOG."""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

import pytest
import set_release_versions as release
from conftest import CHECKOUT_VERSION
from helpers import ROOT

DATA = Path(__file__).parent / "data" / "set_release_versions"
MANIFESTS = ("Cargo.toml", "pyproject.toml", "Cargo.lock", "uv.lock")
REPOSITORY = "https://github.com/koxudaxi/heki"


def texts(root: Path) -> dict[str, str]:
    return {
        str(path.relative_to(root)): path.read_text(encoding="utf-8")
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def lock_versions(root: Path) -> dict[tuple[str, str | None], str]:
    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    return {(p["name"], p.get("source")): p["version"] for p in lock["package"]}


def test_every_version_a_build_reads_is_written(checkout: Path, capsys) -> None:
    before = texts(checkout)
    members = {
        tomllib.loads(text)["package"]["name"]
        for name, text in before.items()
        if name.startswith("crates/")
    }

    assert release.main(["9.8.7", "--root", str(checkout)]) == 0

    assert set(release.manifest_versions(checkout).values()) == {"9.8.7"}
    locked = lock_versions(checkout)
    assert {name for (name, source), v in locked.items() if v == "9.8.7"} == members
    assert {key: v for key, v in locked.items() if key[0] not in members} == {
        key: v for key, v in lock_versions(ROOT).items() if key[0] not in members
    }
    after = texts(checkout)
    for name in MANIFESTS:
        changed = [
            (old, new)
            for old, new in zip(before[name].splitlines(), after[name].splitlines(), strict=True)
            if old != new
        ]
        assert changed, name
        for old, new in changed:
            written = re.fullmatch(r'(.*\bversion = ")9\.8\.7(".*)', new)
            assert written, new
            assert re.fullmatch(r'(.*\bversion = ")[^"]*(".*)', old).groups() == written.groups()
    assert after["CHANGELOG.md"] == before["CHANGELOG.md"]
    assert {name: text for name, text in after.items() if name.startswith("crates/")} == {
        name: text for name, text in before.items() if name.startswith("crates/")
    }
    written = "Cargo.toml, pyproject.toml, Cargo.lock, uv.lock, README.md, adoption.md, index.md"
    assert capsys.readouterr().out == f"9.8.7: wrote {written}.\n"


def test_the_precommit_examples_pin_the_release_tag(checkout: Path) -> None:
    pinned = release.precommit_revs(checkout)
    assert set(pinned) == set(release.PRECOMMIT_DOCS)
    assert all(pinned.values())
    assert {v for revs in pinned.values() for v in revs} == {CHECKOUT_VERSION}
    release.main(["0.1.2", "--root", str(checkout)])
    assert release.precommit_revs(checkout) == {
        name: ["0.1.2"] * len(revs) for name, revs in pinned.items()
    }


def test_the_repository_pins_its_own_release_in_the_precommit_examples() -> None:
    version = release.manifest_versions(ROOT)[release.WORKSPACE_VERSION]
    assert {v for revs in release.precommit_revs(ROOT).values() for v in revs} == {version}


def test_the_commands_pin_the_release(checkout: Path) -> None:
    pinned = release.package_pins(checkout)
    assert set(pinned) == set(release.PACKAGE_PIN_DOCS)
    assert all(pinned.values())
    assert {v for pins in pinned.values() for v in pins} == {CHECKOUT_VERSION}
    release.main(["0.2.0-rc.1", "--root", str(checkout)])
    assert release.package_pins(checkout) == {
        name: ["0.2.0-rc.1"] * len(pins) for name, pins in pinned.items()
    }


def test_the_repository_pins_its_own_release_in_the_commands() -> None:
    version = release.manifest_versions(ROOT)[release.WORKSPACE_VERSION]
    assert {v for pins in release.package_pins(ROOT).values() for v in pins} == {version}


@pytest.mark.parametrize(
    ("text", "pins"),
    [
        ("$ uvx heki==0.6.0 check", ["0.6.0"]),
        ("additional_dependencies: [heki==0.2.0-rc.1]", ["0.2.0-rc.1"]),
        ("pin it (`heki==0.6.0`).", ["0.6.0"]),
        ("ends a sentence: heki==0.6.0.", ["0.6.0"]),
        ("pyheki==1.0 and my.heki==1.0 and heki-pre-commit==1.0", []),
    ],
)
def test_a_pin_is_heki_and_a_version(text: str, pins: list[str]) -> None:
    assert [m["value"] for m in release.PACKAGE_PIN.finditer(text)] == pins


def test_another_hooks_rev_stays(checkout: Path) -> None:
    other = "  - repo: https://github.com/astral-sh/ruff-pre-commit\n    rev: v0.8.0\n"
    readme = checkout / "README.md"
    readme.write_text(readme.read_text(encoding="utf-8") + other, encoding="utf-8")
    release.main(["0.1.2", "--root", str(checkout)])
    assert readme.read_text(encoding="utf-8").endswith(other)
    assert release.precommit_revs(checkout)["README.md"] == ["0.1.2"]


def test_a_page_without_its_pin_fails_before_anything_is_written(checkout: Path) -> None:
    index = checkout / "docs" / "index.md"
    index.write_text(index.read_text(encoding="utf-8").replace("heki==", "heki "), encoding="utf-8")
    before = texts(checkout)
    with pytest.raises(SystemExit, match=re.escape("docs/index.md `heki==` pin: expected a line")):
        release.main(["9.8.7", "--root", str(checkout)])
    assert texts(checkout) == before


def test_a_pre_release_version_is_written(checkout: Path) -> None:
    release.main(["0.2.0-rc.1", "--root", str(checkout)])
    assert set(release.manifest_versions(checkout).values()) == {"0.2.0-rc.1"}


def test_the_changelog_section_is_dated_and_its_link_points_to_the_tag(checkout: Path) -> None:
    release.main(["0.1.2", "--changelog", "2026-11-25", "--root", str(checkout)])
    assert (checkout / "CHANGELOG.md").read_text(encoding="utf-8") == (
        DATA / "dated-CHANGELOG.md"
    ).read_text(encoding="utf-8")


def test_a_release_build_dates_the_changelog_as_the_sync_does(
    checkout: Path, tmp_path: Path
) -> None:
    """The step that writes the release into a build's tree and the Release Sync step leave the
    same CHANGELOG, so `heki version --changelog` of the released binary prints what `main`
    gets; without a date (a pre-release, a manual build) the section stays as it is."""
    yaml = pytest.importorskip("yaml")
    workflows = ROOT / ".github" / "workflows"
    build = yaml.safe_load((workflows / "release.yml").read_text(encoding="utf-8"))
    sync = yaml.safe_load((workflows / "release-sync.yml").read_text(encoding="utf-8"))
    build_step = next(
        s for s in build["jobs"]["sdist"]["steps"] if "set_release_versions.py" in s.get("run", "")
    )
    sync_step = next(
        s for s in sync["jobs"]["patch"]["steps"] if "set_release_versions.py" in s.get("run", "")
    )
    script = f'"{sys.executable}" "{ROOT / "scripts" / "set_release_versions.py"}"'

    def run(step: dict, root: Path, env: dict[str, str]) -> str:
        command = step["run"].replace(
            "uv run --no-project --python 3.14 scripts/set_release_versions.py", script
        )
        result = subprocess.run(
            ["bash", "-c", f'{command} --root "{root}"'],
            env={**os.environ, **env},
            capture_output=True,
            check=False,
            text=True,
        )
        assert result.returncode == 0, result.stderr
        return (root / "CHANGELOG.md").read_text(encoding="utf-8")

    synced = tmp_path / "main"
    shutil.copytree(checkout, synced)
    built = run(build_step, checkout, {"RELEASE_VERSION": "0.1.2", "RELEASE_DATE": "2026-11-25"})
    assert built == run(sync_step, synced, {"VERSION": "0.1.2", "DATE": "2026-11-25"})
    assert "## [0.1.2] - 2026-11-25" in built
    undated = tmp_path / "undated"
    shutil.copytree(synced, undated)
    shutil.copy(DATA / "CHANGELOG.md", undated / "CHANGELOG.md")
    unchanged = run(build_step, undated, {"RELEASE_VERSION": "0.1.2", "RELEASE_DATE": ""})
    assert unchanged == (DATA / "CHANGELOG.md").read_text(encoding="utf-8")


def test_a_second_run_changes_nothing(checkout: Path, capsys) -> None:
    release.main(["0.1.2", "--changelog", "2026-11-25", "--root", str(checkout)])
    first = texts(checkout)
    capsys.readouterr()

    # A later run, on another day, keeps the date of the first.
    release.main(["0.1.2", "--changelog", "2026-11-26", "--root", str(checkout)])
    assert texts(checkout) == first
    assert capsys.readouterr().out == "0.1.2: nothing to change.\n"


def test_main_never_goes_back_to_an_older_release(checkout: Path, capsys) -> None:
    release.main(["0.2.0", "--root", str(checkout)])
    (checkout / "CHANGELOG.md").write_text(
        "## [0.2.1] - Unreleased\n\n## [0.1.2] - 2026-11-25\n", encoding="utf-8"
    )
    before = texts(checkout)
    capsys.readouterr()

    release.main(["0.1.2", "--changelog", "2026-11-25", "--root", str(checkout)])

    assert texts(checkout) == before
    assert capsys.readouterr().out == (
        "0.1.2: the manifests declare the later 0.2.0; nothing to change.\n"
    )
    # A build writes any version it is given.
    release.main(["0.1.2", "--root", str(checkout)])
    assert set(release.manifest_versions(checkout).values()) == {"0.1.2"}


def test_release_versions_are_ordered_as_cargo_and_pypi_order_them() -> None:
    ordered = [
        "0.2.0-alpha.1",
        "0.2.0-beta.1",
        "0.2.0-rc.1",
        "0.2.0-rc.2",
        "0.2.0",
        "0.2.1",
        "0.10.0",
    ]
    keys = [release.version_order(version, "test") for version in ordered]
    assert keys == sorted(keys) and len(set(keys)) == len(keys)


@pytest.mark.parametrize(
    ("changelog", "expected"),
    [
        (
            # A link that does not end at `HEAD` is the author's.
            "## [Unreleased]\n\n## [0.1.2] - Unreleased\n\n- A fix.\n\n"
            "[0.1.2]: https://example.com/notes\n",
            "## [Unreleased]\n\n## [0.1.2] - 2026-11-25\n\n- A fix.\n\n"
            f"[Unreleased]: {REPOSITORY}/compare/v0.1.2...HEAD\n"
            "[0.1.2]: https://example.com/notes\n",
        ),
        (
            # A missing link compares the previous release with the tag.
            "## [Unreleased]\n\n## [0.1.2] - Unreleased\n\n## [0.1.1] - 2026-11-22\n\n"
            f"[unreleased]: {REPOSITORY}/compare/v0.1.1...HEAD\n"
            f"[0.1.1]: {REPOSITORY}/releases/tag/v0.1.1\n",
            "## [Unreleased]\n\n## [0.1.2] - 2026-11-25\n\n## [0.1.1] - 2026-11-22\n\n"
            f"[unreleased]: {REPOSITORY}/compare/v0.1.2...HEAD\n"
            f"[0.1.2]: {REPOSITORY}/compare/v0.1.1...v0.1.2\n"
            f"[0.1.1]: {REPOSITORY}/releases/tag/v0.1.1\n",
        ),
        (
            # A first release without a link shows its tag, and without an `[Unreleased]`
            # section no `[Unreleased]` link is written.
            "## [0.1.2] - Unreleased\n\n- A fix.\n",
            f"## [0.1.2] - 2026-11-25\n\n- A fix.\n\n[0.1.2]: {REPOSITORY}/releases/tag/v0.1.2\n",
        ),
        (
            "## [0.1.2] - Unreleased\n\n[Unreleased]: https://example.com\n",
            "## [0.1.2] - 2026-11-25\n\n[Unreleased]: https://example.com\n"
            f"[0.1.2]: {REPOSITORY}/releases/tag/v0.1.2\n",
        ),
        (
            # Only the first unreleased section is the release's.
            "## [0.1.2] - Unreleased\n\n## [0.1.1] - Unreleased\n\n"
            f"[0.1.2]: {REPOSITORY}/compare/v0.1.0...HEAD\n",
            "## [0.1.2] - 2026-11-25\n\n## [0.1.1] - Unreleased\n\n"
            f"[0.1.2]: {REPOSITORY}/compare/v0.1.0...v0.1.2\n",
        ),
        (
            # A section dated and linked by hand stays as it is.
            f"## [0.1.2] - 2026-11-21\n\n[0.1.2]: {REPOSITORY}/compare/v0.1.1...v0.1.2\n",
            f"## [0.1.2] - 2026-11-21\n\n[0.1.2]: {REPOSITORY}/compare/v0.1.1...v0.1.2\n",
        ),
    ],
)
def test_the_changelog_links(checkout: Path, changelog: str, expected: str) -> None:
    (checkout / "CHANGELOG.md").write_text(changelog, encoding="utf-8")
    release.main(["0.1.2", "--changelog", "2026-11-25", "--root", str(checkout)])
    assert (checkout / "CHANGELOG.md").read_text(encoding="utf-8") == expected
    release.main(["0.1.2", "--changelog", "2026-11-25", "--root", str(checkout)])
    assert (checkout / "CHANGELOG.md").read_text(encoding="utf-8") == expected


def edit(root: Path, name: str, old: str, new: str) -> None:
    text = (root / name).read_text(encoding="utf-8")
    assert text.count(old) == 1, (name, old)
    (root / name).write_text(text.replace(old, new), encoding="utf-8")


@pytest.mark.parametrize(
    ("name", "old", "new", "message"),
    [
        (
            "pyproject.toml",
            'version = "{v}"',
            'version="{v}"',
            "pyproject.toml `[project]`: expected one line matching",
        ),
        (
            "Cargo.toml",
            '[workspace.package]\nversion = "{v}"',
            '[workspace.package]\nversion="{v}"',
            "Cargo.toml `[workspace.package]`: expected one line matching",
        ),
        (
            "Cargo.toml",
            'heki_kb = { path = "crates/heki_kb", version = "{v}" }',
            'heki_kb = { version = "{v}", path = "crates/heki_kb" }',
            "Cargo.toml `heki_kb`: expected one line matching",
        ),
        (
            "Cargo.lock",
            '[[package]]\nname = "heki_ast"\n',
            '[[package]]\nname = "heki_ast"\nversion = "{v}"\n\n[[package]]\nname = "heki_ast"\n',
            "Cargo.lock `heki_ast`: expected one line matching",
        ),
        (
            "Cargo.lock",
            '[[package]]\nname = "heki_ast"\n',
            '[[package]]\nname = "heki_outside"\nversion = "0.0.1"\n\n'
            '[[package]]\nname = "heki_ast"\n',
            "Cargo.lock: the entries without a source are",
        ),
        (
            "uv.lock",
            'version = "{v}"\nsource = { virtual = "." }',
            'version = "{v}"\nsource = { registry = "https://pypi.org/simple" }',
            "uv.lock `heki`: expected one line matching",
        ),
        (
            "README.md",
            "    rev: v{v}",
            "    rev:  v{v}",
            "README.md pre-commit example: expected a line matching",
        ),
        (
            "crates/heki_kb/Cargo.toml",
            "\nversion.workspace = true",
            '\nversion = "{v}"',
            "crates/heki_kb/Cargo.toml: the version must come from the workspace",
        ),
    ],
)
def test_a_pattern_that_does_not_match_once_fails_before_anything_is_written(
    checkout: Path, name: str, old: str, new: str, message: str
) -> None:
    version = CHECKOUT_VERSION
    edit(checkout, name, old.replace("{v}", version), new.replace("{v}", version))
    before = texts(checkout)
    with pytest.raises(SystemExit, match=re.escape(message)):
        release.main(["9.8.7", "--changelog", "2026-11-25", "--root", str(checkout)])
    assert texts(checkout) == before


@pytest.mark.parametrize(
    ("changelog", "message"),
    [
        ("## [0.1.1] - 2026-11-22\n", "no `## [0.1.2] - Unreleased` heading to date"),
        (
            "## [0.2.0] - Unreleased\n\n## [0.1.2] - Unreleased\n",
            "the first unreleased section is [0.2.0], not [0.1.2]",
        ),
        (
            "## [0.1.2] - Unreleased\n\n[0.1.2]: https://a\n[0.1.2]: https://b\n",
            "[0.1.2] is defined 2 times",
        ),
    ],
)
def test_a_changelog_without_the_release_section_fails(
    checkout: Path, changelog: str, message: str
) -> None:
    (checkout / "CHANGELOG.md").write_text(changelog, encoding="utf-8")
    before = texts(checkout)
    with pytest.raises(SystemExit, match=re.escape(message)):
        release.main(["0.1.2", "--changelog", "2026-11-25", "--root", str(checkout)])
    assert texts(checkout) == before


@pytest.mark.parametrize(
    "version", ["v0.1.2", "0.1", "0.1.2rc1", "01.1.2", "0.1.2+local", "0.1.2-dev.1"]
)
def test_a_version_that_is_not_a_release_version_fails(checkout: Path, version: str) -> None:
    before = texts(checkout)
    with pytest.raises(SystemExit, match="is not a version in the form MAJOR.MINOR.PATCH"):
        release.main([version, "--root", str(checkout)])
    assert texts(checkout) == before


@pytest.mark.parametrize("date", ["2026-02-30", "20261125", "2026-11-25T00:00:00Z"])
def test_a_changelog_date_must_be_a_calendar_date(checkout: Path, date: str, capsys) -> None:
    with pytest.raises(SystemExit) as stopped:
        release.main(["0.1.2", "--changelog", date, "--root", str(checkout)])
    assert stopped.value.code == 2
    assert "not a date in the form YYYY-MM-DD" in capsys.readouterr().err
