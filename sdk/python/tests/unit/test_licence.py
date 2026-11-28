# SPDX-License-Identifier: Apache-2.0
"""The Python SDK is Apache-2.0: its licence file, its package metadata and the first line of its files."""

from __future__ import annotations

import hashlib
import subprocess
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPDX = "SPDX-License-Identifier: Apache-2.0"
# The SHA-256 of https://www.apache.org/licenses/LICENSE-2.0.txt
APACHE_2 = "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
# Files with no room for a comment: the licence itself, the lock file uv writes, and the empty py.typed marker.
NO_HEADER = {"LICENSE", "uv.lock", "src/knoggin/py.typed"}
FIRST_LINE = {".py": f"# {SPDX}", ".toml": f"# {SPDX}", ".gitignore": f"# {SPDX}", ".md": f"<!-- {SPDX} -->"}


def test_the_licence_file_is_the_apache_2_text_and_the_metadata_says_so() -> None:
    assert hashlib.sha256((ROOT / "LICENSE").read_bytes()).hexdigest() == APACHE_2
    project = tomllib.loads((ROOT / "pyproject.toml").read_text())["project"]
    assert (project["license"], project["license-files"]) == ("Apache-2.0", ["LICENSE"])


def test_every_file_starts_with_the_apache_2_header() -> None:
    listed = subprocess.run(["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.split()
    files = [f for f in listed if f not in NO_HEADER]
    assert len(files) > 30
    wrong: list[str] = []
    for f in files:
        first = (ROOT / f).read_text().split("\n", 1)[0]
        want = FIRST_LINE.get(Path(f).suffix or Path(f).name, f"// {SPDX}")
        if first != want:
            wrong.append(f"{f}: {first!r}")
    assert wrong == []
