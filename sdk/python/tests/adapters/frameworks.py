# SPDX-License-Identifier: Apache-2.0
"""Checks every adapter job makes of its framework's dependency group."""

from __future__ import annotations

import errno
import os
import socket
import subprocess
import sys
from importlib.metadata import requires, version

import pytest
from packaging.requirements import Requirement
from packaging.version import Version


def check_group(extra: str, module: str) -> None:
    """The group installs what the extra asks for, and `import knoggin` still imports none of it."""
    wanted = [
        r for r in map(Requirement, requires("knoggin") or []) if r.marker and r.marker.evaluate({"extra": extra})
    ]
    assert wanted, f"the package has no extra {extra!r}"
    for r in wanted:
        assert r.specifier.contains(version(r.name), prereleases=True), f"{r.name} {version(r.name)} is not {r}"
    floor = os.environ.get("KNOGGIN_ADAPTER_FLOOR")
    if floor:  # CI's floor run pins exactly the lowest version the extra allows
        pin = Requirement(floor)
        (r,) = [r for r in wanted if r.name == pin.name]
        assert [Version(s.version) for s in r.specifier if s.operator == ">="] == [
            Version(s.version) for s in pin.specifier
        ], f"CI's floor run pins {pin}, the extra asks for {r}"
    __import__(module)
    probe = f"import sys, knoggin; sys.exit({module!r} in sys.modules)"
    assert subprocess.run([sys.executable, "-c", probe], check=False).returncode == 0, (
        f"`import knoggin` imports {module}"
    )


def check_no_network(refused: list[str]) -> None:
    """The guard of `conftest.py` refuses an outside address, blocking or not, and records both attempts."""
    with pytest.raises(ConnectionRefusedError, match="reach no network"):
        socket.create_connection(("192.0.2.1", 443), timeout=1)
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.setblocking(False)
        assert sock.connect_ex(("192.0.2.1", 443)) == errno.ECONNREFUSED
    assert refused == [repr(("192.0.2.1", 443))] * 2
    refused.clear()  # expected: the test does not fail at teardown
