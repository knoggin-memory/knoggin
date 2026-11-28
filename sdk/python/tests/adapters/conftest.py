# SPDX-License-Identifier: Apache-2.0
"""Shared by the adapter tests, which run one framework at a time, each with its own dependency group.

The tests reach no network: a connection to anything but a Unix socket or a loopback address fails.
The fixtures of the unit and contract tests are shared here too.
"""

from __future__ import annotations

import ipaddress
import socket
from typing import Any

import pytest

from ..contract.conftest import client as client
from ..contract.conftest import live as live
from ..contract.conftest import via as via
from ..unit.conftest import api as api
from ..unit.conftest import clean_env as clean_env


def _local(sock: socket.socket, address: Any) -> bool:
    if sock.family == socket.AF_UNIX:
        return True
    host = str(address[0])
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return host == "localhost"


@pytest.fixture(autouse=True)
def no_network(monkeypatch: pytest.MonkeyPatch) -> None:
    connect = socket.socket.connect

    def guarded(sock: socket.socket, address: Any) -> None:
        if not _local(sock, address):
            raise ConnectionRefusedError(f"adapter tests reach no network, not even {address!r}")
        connect(sock, address)

    monkeypatch.setattr(socket.socket, "connect", guarded)
