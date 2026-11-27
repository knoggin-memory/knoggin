# SPDX-License-Identifier: AGPL-3.0-only
"""The contract tests run against a live Knoggin.

With `KNOGGIN_TEST_URL` (and `KNOGGIN_TEST_API_KEY`) they use that address, which is how `stack_sdk.rs`
runs them against the one-host stack. Otherwise they start a daemon in test mode in a temporary home,
from `KNOGGIN_BIN` or this repository's debug build, and run each test both over its Unix socket and
over its loopback address with an app's API key.
"""

from __future__ import annotations

import os
import uuid
from collections.abc import Iterator
from dataclasses import dataclass
from typing import Any

import pytest

from knoggin import Client

from ..daemon import Daemon, binary

TEST_URL = os.environ.get("KNOGGIN_TEST_URL")
TEST_KEY = os.environ.get("KNOGGIN_TEST_API_KEY")


@dataclass
class Live:
    """Where the tests talk to: a daemon this run owns, or the address given."""

    daemon: Daemon | None
    base_url: str
    api_key: str | None
    store_id: str | None = None


@pytest.fixture(scope="session")
def live() -> Iterator[Live]:
    if TEST_URL:
        yield Live(None, TEST_URL.rstrip("/"), TEST_KEY)
        return
    exe = binary()
    if exe is None:
        if os.environ.get("KNOGGIN_BIN"):
            pytest.fail(f"KNOGGIN_BIN={os.environ['KNOGGIN_BIN']} is not a file")
        pytest.skip("no knoggin binary: run `cargo build -p knoggin`, or set KNOGGIN_BIN or KNOGGIN_TEST_URL")
    d = Daemon(exe)
    try:
        store_id, _, key = d.app_key("sdk-contract")
        yield Live(d, d.base_url, key, store_id)
    finally:
        d.remove()


@pytest.fixture
def daemon(live: Live) -> Daemon:
    """The daemon itself, for the tests that need its socket or its control routes."""
    if live.daemon is None:
        pytest.skip("needs a daemon this run started")
    return live.daemon


def socket_client(d: Daemon, monkeypatch: pytest.MonkeyPatch, **kwargs: Any) -> Client:
    with monkeypatch.context() as m:
        m.setenv("KNOGGIN_HOME", str(d.home))
        return Client(**kwargs)


@pytest.fixture(params=["socket", "url"])
def via(request: pytest.FixtureRequest, live: Live) -> str:
    if request.param == "socket" and live.daemon is None:
        pytest.skip("the socket needs a daemon this run started")
    return str(request.param)


@pytest.fixture
def client(via: str, live: Live, monkeypatch: pytest.MonkeyPatch) -> Iterator[Client]:
    """A client whose default namespace is new for each test, so tests never see each other's memories."""
    ns = f"ct-{uuid.uuid4().hex[:12]}"
    if via == "socket":
        assert live.daemon is not None
        c = socket_client(live.daemon, monkeypatch, namespace=ns)
    else:
        c = Client(base_url=live.base_url, api_key=live.api_key, namespace=ns)
    c.namespaces.create(ns)
    yield c
    c.namespaces.delete(ns, force=True)
    c.close()
