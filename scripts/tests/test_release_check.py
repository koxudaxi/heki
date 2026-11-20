"""Tests of `release_check.py versions`: every distribution declares the release version."""

from __future__ import annotations

import io
import re
import tarfile
import zipfile
from pathlib import Path

import pytest
import release_check


def write_wheel(dist: Path, platform: str, version: str) -> None:
    with zipfile.ZipFile(dist / f"heki-{version}-py3-none-{platform}.whl", "w") as wheel:
        wheel.writestr(
            f"heki-{version}.dist-info/METADATA",
            f"Metadata-Version: 2.4\nName: heki\nVersion: {version}\n",
        )


def write_sdist(dist: Path, version: str) -> None:
    data = f"Metadata-Version: 2.4\nName: heki\nVersion: {version}\n".encode()
    with tarfile.open(dist / f"heki-{version}.tar.gz", "w:gz") as sdist:
        info = tarfile.TarInfo(f"heki-{version}/PKG-INFO")
        info.size = len(data)
        sdist.addfile(info, io.BytesIO(data))


@pytest.fixture
def dist(tmp_path: Path) -> Path:
    path = tmp_path / "dist"
    path.mkdir()
    return path


def test_the_distributions_declare_the_release_version_not_the_manifests(
    dist: Path, capsys
) -> None:
    # The build wrote the tag's version; the committed manifests still declare the last release.
    write_wheel(dist, "manylinux_2_28_x86_64", "0.2.0rc1")
    write_wheel(dist, "win_amd64", "0.2.0rc1")
    write_sdist(dist, "0.2.0rc1")
    assert release_check.cargo_version() != "0.2.0-rc.1"
    assert release_check.main(["versions", str(dist), "--version", "0.2.0-rc.1"]) == 0
    assert capsys.readouterr().out == "Every distribution declares 0.2.0rc1 (3 distributions).\n"


def test_a_distribution_with_another_version_fails(dist: Path) -> None:
    write_wheel(dist, "manylinux_2_28_x86_64", "0.1.2")
    write_wheel(dist, "win_amd64", "0.1.1")
    write_sdist(dist, "0.1.2")
    with pytest.raises(SystemExit, match=re.escape("heki-0.1.1-py3-none-win_amd64.whl: 0.1.1")):
        release_check.main(["versions", str(dist), "--version", "0.1.2"])


def test_without_a_version_the_committed_manifests_name_it(dist: Path) -> None:
    committed = release_check.cargo_version()
    write_wheel(dist, "manylinux_2_28_x86_64", committed)
    write_sdist(dist, committed)
    assert release_check.main(["versions", str(dist)]) == 0
    with pytest.raises(SystemExit, match="versions differ"):
        release_check.main(["versions", str(dist), "--version", "9.8.7"])


def test_a_release_version_that_is_not_a_version_fails(dist: Path) -> None:
    write_wheel(dist, "manylinux_2_28_x86_64", "0.1.2")
    write_sdist(dist, "0.1.2")
    with pytest.raises(SystemExit, match="'release-0.1.2' is not a version"):
        release_check.main(["versions", str(dist), "--version", "release-0.1.2"])
