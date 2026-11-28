# SPDX-License-Identifier: Apache-2.0
"""The `knoggin` command of the Python package.

`knoggin init` (and `init --uninstall`) is handled here. Every other argument goes straight to the
real binary at `~/.knoggin/bin/knoggin`, which is installed first if it is missing. That makes
`uvx knoggin mcp` a working stdio MCP server on a fresh machine.
"""

from __future__ import annotations

import os
import subprocess
import sys
from importlib import resources
from pathlib import Path


def install_dir() -> Path:
    return Path.home() / ".knoggin"


def binary() -> Path:
    return install_dir() / "bin" / "knoggin"


def installer() -> Path | None:
    """The release's stamped `install.sh`, which the release build copies into the package."""
    path = Path(str(resources.files(__package__ or "knoggin.cli").joinpath("install.sh")))
    return path if path.is_file() else None


def install_binary() -> bool:
    """Runs the bundled installer. Its output goes to standard error, so that it never mixes with a bridge's stdout."""
    script = installer()
    if script is None:
        print(
            "This build of the knoggin package has no installer. Install Knoggin from its release page,"
            " then run this command again.",
            file=sys.stderr,
        )
        return False
    return subprocess.run(["/bin/sh", str(script)], stdout=2, check=False).returncode == 0


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if args[:1] == ["init"]:
        from .init import run

        return run(args[1:])
    exe = binary()
    if not exe.is_file():
        print(f"Knoggin is not installed at {exe}; installing it first.", file=sys.stderr)
        if not install_binary() or not exe.is_file():
            return 1
    os.execv(exe, [str(exe), *args])  # noqa: S606 - replaces this process with the real binary
