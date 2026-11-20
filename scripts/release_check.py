"""Checks the release workflow runs on the built distributions before they are published.

Usage:
    python scripts/release_check.py smoke dist/heki-*.whl [--install]

`smoke` checks that a wheel carries the `heki` executable and that the executable analyzes a
project: it reports a known diagnostic with exit code 1 and a clean project with 0. With
`--install` it runs the executable installed into a fresh virtual environment by uv, otherwise
the one extracted from the wheel (for a platform the runner's Python cannot install, such as
musl on a glibc runner).
"""

from __future__ import annotations

import argparse
import stat
import subprocess
import sys
import tempfile
import tomllib
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

BLOCKING = "import time\n\n\nasync def handler() -> None:\n    time.sleep(1)\n"
BLOCKING_REPORT = "blocking.py:5:5: EFF101 "
CLEAN = "def add(a: int, b: int) -> int:\n    return a + b\n"


def cargo_version() -> str:
    cargo = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    return cargo["workspace"]["package"]["version"]


def fail(message: str) -> None:
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


def smoke(wheel_path: Path, install: bool) -> None:
    with zipfile.ZipFile(wheel_path) as wheel, tempfile.TemporaryDirectory() as tmp_name:
        tmp = Path(tmp_name)
        member = executable_member(wheel, wheel_path.name)
        exe = installed(wheel_path, tmp) if install else extracted(wheel, member, tmp)

        version = run(exe, "--version")
        if version.returncode != 0 or version.stdout.strip() != f"heki {cargo_version()}":
            fail(f"`heki --version` printed {version.stdout!r} (exit {version.returncode})")
        rules = run(exe, "rule", "--all")
        if rules.returncode != 0 or "EFF101" not in rules.stdout:
            fail(f"`heki rule --all` failed (exit {rules.returncode}): {rules.stderr}")

        project = tmp / "project"
        project.mkdir()
        (project / "pyproject.toml").write_text('[project]\nname = "smoke"\nversion = "0"\n')
        (project / "blocking.py").write_text(BLOCKING)
        found = run(exe, "check", "--output-format", "concise", cwd=project)
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    smoke_args = commands.add_parser("smoke", help="check one wheel and run its executable")
    smoke_args.add_argument("wheel", type=Path)
    smoke_args.add_argument("--install", action="store_true", help="install it with uv first")
    args = parser.parse_args()
    smoke(args.wheel.resolve(), args.install)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
