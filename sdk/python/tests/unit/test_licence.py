# SPDX-License-Identifier: Apache-2.0
"""The Python SDK is Apache-2.0: its licence file, its package metadata and the first line of every Python file."""

from __future__ import annotations

import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HEADER = "# SPDX-License-Identifier: Apache-2.0"


def test_the_licence_file_and_the_metadata_say_apache_2() -> None:
    assert (ROOT / "LICENSE").read_text().split()[:4] == ["Apache", "License", "Version", "2.0,"]
    project = tomllib.loads((ROOT / "pyproject.toml").read_text())["project"]
    assert (project["license"], project["license-files"]) == ("Apache-2.0", ["LICENSE"])


def test_every_python_file_starts_with_the_apache_2_header() -> None:
    files = sorted(p for d in ("src", "tests") for p in (ROOT / d).rglob("*.py"))
    assert len(files) > 20
    wrong = [str(p.relative_to(ROOT)) for p in files if p.read_text().split("\n", 1)[0] != HEADER]
    assert wrong == []
