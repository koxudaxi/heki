"""Checks the release workflow runs on the built distributions before they are published.

Usage:
    python scripts/release_check.py smoke dist/heki-*.whl [--install] [--version 0.1.0]
    python scripts/release_check.py versions dist/ [--version 0.1.0]  # needs `packaging`

The version is the release's: the tag without its `v`, which the build jobs wrote into the
manifests (`scripts/set_release_versions.py`). Without `--version`, a run that built the
committed manifests, it is `Cargo.toml`'s `workspace.package.version`.

`smoke` checks that a wheel carries the `heki` executable and that the executable analyzes a
project: `heki --version` prints the version, and `heki check` reports a known diagnostic with
exit code 1 and a clean project with 0. With `--install` it runs the executable installed into
a fresh virtual environment by uv, otherwise the one extracted from the wheel (for a platform
the runner's Python cannot install, such as musl on a glibc runner).

`versions` checks that the metadata of every wheel and of the source distribution declare the
version, so that a tag such as `v0.2.0-rc.1` cannot publish distributions that declare `0.2.0`.
"""

from __future__ import annotations

import argparse
import email.parser
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile
from pathlib import Path
from typing import NoReturn

ROOT = Path(__file__).resolve().parent.parent

BLOCKING = "import time\n\n\nasync def handler() -> None:\n    time.sleep(1)\n"
BLOCKING_REPORT = "blocking.py:5:5: EFF101 "
CLEAN = "def add(a: int, b: int) -> int:\n    return a + b\n"


def cargo_version() -> str:
    cargo = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    return cargo["workspace"]["package"]["version"]


def fail(message: str) -> NoReturn:
    sys.exit(f"release check failed: {message}")


# ------------------------------------------------------------------------------------- smoke


def executable_member(wheel: zipfile.ZipFile, name: str) -> str:
    """The one `heki` executable in the wheel's scripts directory."""
    exe = "heki.exe" if "-win" in name else "heki"
    members = [
        m
        for m in wheel.namelist()
        if Path(m).parent.name == "scripts" and Path(m).parent.parent.name.endswith(".data")
    ]
    if [Path(m).name for m in members] != [exe]:
        fail(f"{name}: expected exactly `{exe}` under `*.data/scripts/`, found {members}")
    if wheel.getinfo(members[0]).file_size == 0:
        fail(f"{name}: `{members[0]}` is empty")
    return members[0]


def installed(wheel_path: Path, tmp: Path) -> Path:
    venv = tmp / "venv"
    subprocess.run(["uv", "venv", "--quiet", str(venv)], check=True)
    subprocess.run(
        ["uv", "pip", "install", "--quiet", "--python", str(venv), str(wheel_path)], check=True
    )
    exe = venv / "Scripts" / "heki.exe" if sys.platform == "win32" else venv / "bin" / "heki"
    if not exe.is_file():
        fail(f"installing {wheel_path.name} did not create {exe}")
    return exe


def extracted(wheel: zipfile.ZipFile, member: str, tmp: Path) -> Path:
    exe = Path(wheel.extract(member, tmp))
    exe.chmod(exe.stat().st_mode | stat.S_IXUSR)
    return exe


def run(exe: Path, *args: str, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run([str(exe), *args], cwd=cwd, capture_output=True, text=True, check=False)


def smoke(wheel_path: Path, install: bool, expected: str) -> None:
    with zipfile.ZipFile(wheel_path) as wheel, tempfile.TemporaryDirectory() as tmp_name:
        tmp = Path(tmp_name)
        member = executable_member(wheel, wheel_path.name)
        exe = installed(wheel_path, tmp) if install else extracted(wheel, member, tmp)

        version = run(exe, "--version")
        if version.returncode != 0 or version.stdout.strip() != f"heki {expected}":
            fail(
                f"`heki --version` printed {version.stdout!r} (exit {version.returncode}), "
                f"not `heki {expected}`"
            )
        rules = run(exe, "rule", "--all")
        if rules.returncode != 0 or "EFF101" not in rules.stdout:
            fail(f"`heki rule --all` failed (exit {rules.returncode}): {rules.stderr}")

        project = tmp / "project"
        project.mkdir()
        (project / "pyproject.toml").write_text('[project]\nname = "smoke"\nversion = "0"\n')
        (project / "blocking.py").write_text(BLOCKING)
        # Named by its code, the rule runs whatever its certainty.
        found = run(exe, "check", "--select", "EFF101", "--output-format", "concise", cwd=project)
        if found.returncode != 1 or not any(
            line.startswith(BLOCKING_REPORT) for line in found.stdout.splitlines()
        ):
            fail(
                f"`heki check` on a blocking `async def` exited {found.returncode} without "
                f"`{BLOCKING_REPORT}...`:\n{found.stdout}{found.stderr}"
            )
        (project / "blocking.py").write_text(CLEAN)
        clean = run(exe, "check", cwd=project)
        if clean.returncode != 0:
            fail(f"`heki check` on a clean project exited {clean.returncode}:\n{clean.stderr}")
    how = "installed" if install else "extracted"
    print(f"{wheel_path.name}: `heki` runs and reports ({how}).")


# ---------------------------------------------------------------------------------- versions


def wheel_version(path: Path) -> str:
    with zipfile.ZipFile(path) as wheel:
        metadata = [m for m in wheel.namelist() if m.endswith(".dist-info/METADATA")]
        if len(metadata) != 1:
            fail(f"{path.name}: expected one `.dist-info/METADATA`, found {metadata}")
        return email.parser.HeaderParser().parsestr(wheel.read(metadata[0]).decode())["Version"]


def sdist_version(path: Path) -> str:
    with tarfile.open(path) as sdist:
        pkg_info = [m for m in sdist.getnames() if m.count("/") == 1 and m.endswith("/PKG-INFO")]
        if len(pkg_info) != 1:
            fail(f"{path.name}: expected one top-level `PKG-INFO`, found {pkg_info}")
        data = sdist.extractfile(pkg_info[0])
        assert data is not None
        return email.parser.HeaderParser().parsestr(data.read().decode())["Version"]


def versions(dist: Path, version: str) -> None:
    from packaging.version import InvalidVersion, Version

    try:
        expected = Version(version)
    except InvalidVersion:
        fail(f"{version!r} is not a version")
    wheels = sorted(dist.glob("*.whl"))
    sdists = sorted(dist.glob("*.tar.gz"))
    if not wheels or len(sdists) != 1:
        fail(f"expected wheels and one source distribution in {dist}, found {wheels + sdists}")
    declared = {w.name: wheel_version(w) for w in wheels}
    declared[sdists[0].name] = sdist_version(sdists[0])

    def same(text: str) -> bool:
        try:
            return Version(text) == expected
        except InvalidVersion:
            return False

    wrong = {where: v for where, v in declared.items() if not same(v)}
    if wrong:
        listing = "\n".join(f"  {where}: {v}" for where, v in wrong.items())
        fail(f"versions differ from {expected}:\n{listing}")
    print(f"Every distribution declares {expected} ({len(declared)} distributions).")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    smoke_args = commands.add_parser("smoke", help="check one wheel and run its executable")
    smoke_args.add_argument("wheel", type=Path)
    smoke_args.add_argument("--install", action="store_true", help="install it with uv first")
    version_args = commands.add_parser("versions", help="compare the distributions' versions")
    version_args.add_argument("dist", type=Path)
    for command in (smoke_args, version_args):
        command.add_argument("--version", help="the release version (the tag without its `v`)")
    args = parser.parse_args(argv)
    version = args.version or cargo_version()
    if args.command == "smoke":
        smoke(args.wheel.resolve(), args.install, version)
    else:
        versions(args.dist, version)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
