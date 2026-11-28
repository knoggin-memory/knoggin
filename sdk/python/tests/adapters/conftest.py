# SPDX-License-Identifier: Apache-2.0
"""Shared by the adapter tests, which run one framework at a time, each with its own dependency group.

The tests reach no network: a TCP connection to anything but a loopback address is refused, and any
such attempt fails the test, also when the framework swallowed the refusal. UDP, and so DNS lookups,
are left alone. Each framework's own switch for remote fetches and telemetry is set here, before the
framework is imported. The fixtures of the unit and contract tests are shared here too.
"""

from __future__ import annotations

import errno
import ipaddress
import os
import socket
from collections.abc import Iterator
from typing import Any

import pytest
import respx

from knoggin import _transport

from ..contract.conftest import client as client
from ..contract.conftest import live as live
from ..contract.conftest import via as via
from ..unit.conftest import BASE
from ..unit.conftest import clean_env as clean_env

# LiteLLM: the model cost map that ships with it, not the one on GitHub.
os.environ["LITELLM_LOCAL_MODEL_COST_MAP"] = "True"
# OpenAI Agents SDK: no trace export.
os.environ["OPENAI_AGENTS_DISABLE_TRACING"] = "1"


def _local(sock: socket.socket, address: Any) -> bool:
    if sock.family == socket.AF_UNIX or sock.type != socket.SOCK_STREAM:
        return True
    host = str(address[0])
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return host == "localhost"


@pytest.fixture(autouse=True)
def no_network(monkeypatch: pytest.MonkeyPatch) -> Iterator[list[str]]:
    """Every refused address, in order. A test that expects a refusal empties the list."""
    refused: list[str] = []
    connect, connect_ex = socket.socket.connect, socket.socket.connect_ex

    def guarded(sock: socket.socket, address: Any) -> None:
        if not _local(sock, address):
            refused.append(repr(address))
            raise ConnectionRefusedError(f"adapter tests reach no network, not even {address!r}")
        connect(sock, address)

    def guarded_ex(sock: socket.socket, address: Any) -> int:
        if not _local(sock, address):
            refused.append(repr(address))
            return errno.ECONNREFUSED
        return connect_ex(sock, address)

    monkeypatch.setattr(socket.socket, "connect", guarded)
    monkeypatch.setattr(socket.socket, "connect_ex", guarded_ex)
    yield refused
    if refused:
        pytest.fail(f"the test tried to reach the network: {', '.join(refused)}")


@pytest.fixture
def api(monkeypatch: pytest.MonkeyPatch) -> Iterator[respx.MockRouter]:
    """A mocked server at the unit tests' address. Retries against it never really wait."""

    def no_sleep(seconds: float) -> None:
        pass

    async def async_no_sleep(seconds: float) -> None:
        pass

    monkeypatch.setattr(_transport, "sleep", no_sleep)
    monkeypatch.setattr(_transport, "async_sleep", async_no_sleep)
    with respx.mock(base_url=BASE, assert_all_called=False) as router:
        yield router
