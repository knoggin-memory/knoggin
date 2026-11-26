# SPDX-License-Identifier: AGPL-3.0-only
"""The SDK's part of the tests of the tab "Sign-in and credentials": API keys, the local socket, and "not running".

These need a daemon this run started, because they make, revoke and rotate keys and delete stores
through the control routes on its socket.
"""

from __future__ import annotations

import socket
import uuid

import httpx
import pytest

from knoggin import (
    AuthenticationError,
    Client,
    ConfigurationError,
    PermissionDeniedError,
    StoreUnavailableError,
)

from ..daemon import Daemon, binary
from .conftest import socket_client


def key_client(d: Daemon, secret: str) -> Client:
    return Client(base_url=d.base_url, api_key=secret)


def test_a_valid_key_works_and_bad_keys_are_unauthenticated(daemon: Daemon) -> None:
    _, _, secret = daemon.app_key(f"keys-{uuid.uuid4().hex[:8]}")
    assert key_client(daemon, secret).namespaces.list()[0].name == "default"
    for bad in ("knoggin_live_short", "knoggin_live_" + "A" * 43, "not-a-knoggin-key"):
        with pytest.raises(AuthenticationError) as caught:
            key_client(daemon, bad).namespaces.list()
        assert caught.value.code == "unauthenticated" and caught.value.status == 401


def test_a_tcp_request_without_a_credential_is_unauthenticated(daemon: Daemon) -> None:
    with pytest.raises(AuthenticationError):
        Client(base_url=daemon.base_url).namespaces.list()


def test_a_revoked_key_is_refused(daemon: Daemon) -> None:
    store, key_id, secret = daemon.app_key(f"revoke-{uuid.uuid4().hex[:8]}")
    c = key_client(daemon, secret)
    c.memories.store("before the revoke")
    assert daemon.control("DELETE", f"/v1/stores/{store}/keys/{key_id}").status_code == 204
    with pytest.raises(AuthenticationError):
        c.namespaces.list()


def test_a_rotated_key_replaces_the_old_one(daemon: Daemon) -> None:
    store, key_id, old = daemon.app_key(f"rotate-{uuid.uuid4().hex[:8]}")
    rotated = daemon.control("POST", f"/v1/stores/{store}/keys/{key_id}/rotate", {})
    assert rotated.status_code == 201, rotated.text
    with pytest.raises(AuthenticationError):
        key_client(daemon, old).namespaces.list()
    assert key_client(daemon, rotated.json()["secret"]).namespaces.list()


def test_the_key_of_a_deleted_store_is_refused_until_the_undo(daemon: Daemon) -> None:
    store, _, secret = daemon.app_key(f"deleted-{uuid.uuid4().hex[:8]}")
    c = key_client(daemon, secret)
    assert c.namespaces.list()
    assert daemon.control("DELETE", f"/v1/stores/{store}").status_code == 200
    with pytest.raises(StoreUnavailableError) as caught:
        c.namespaces.list()
    assert caught.value.code == "store_deleting"
    assert daemon.control("POST", f"/v1/stores/{store}/undelete").status_code == 200
    assert c.namespaces.list()[0].name == "default"


def test_a_read_only_key_reads_cannot_write_and_records_no_access(daemon: Daemon) -> None:
    store, _, writer = daemon.app_key(f"ro-{uuid.uuid4().hex[:8]}")
    m = key_client(daemon, writer).memories.store("read me")
    ro = daemon.control("POST", f"/v1/stores/{store}/keys", {"name": "ro", "permissions": ["memory:read"]})
    assert ro.status_code == 201, ro.text
    reader = key_client(daemon, ro.json()["secret"])
    before = reader.memories.get(m.id, record_access=True).access_count
    reader.memories.recall("read me", record_access=True)
    assert reader.memories.get(m.id).access_count == before
    with pytest.raises(PermissionDeniedError):
        reader.memories.store("not allowed")
    with pytest.raises(PermissionDeniedError):
        reader.memories.forget(m.id)


def test_the_socket_needs_no_credential_and_a_key_there_reaches_its_own_store_only(
    daemon: Daemon, monkeypatch: pytest.MonkeyPatch
) -> None:
    ns = f"ct-{uuid.uuid4().hex[:12]}"
    _, _, secret = daemon.app_key(f"sock-{uuid.uuid4().hex[:8]}")
    owner = socket_client(daemon, monkeypatch)  # no credential: the owner, the personal store
    app = socket_client(daemon, monkeypatch, api_key=secret)
    app.namespaces.create(ns)
    saved = app.memories.store("only in the app", namespace=ns)
    assert [m.id for m in key_client(daemon, secret).memories.list(namespace=ns)] == [saved.id]
    assert ns not in {n.name for n in owner.namespaces.list()}
    assert all(m.id != saved.id for m in owner.memories.list())


def test_a_stopped_daemon_is_reported_as_not_running_and_no_tcp_connection_is_opened(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    exe = binary()
    assert exe is not None
    d = Daemon(exe)
    try:
        address = d.base_url.removeprefix("http://")
        host, port = address.rsplit(":", 1)
        d.stop()
        # Another program takes the daemon's old port.
        listener = socket.create_server((host, int(port)))
        listener.settimeout(0.3)
        try:
            with pytest.raises(ConfigurationError, match="knoggin start"):
                socket_client(d, monkeypatch).namespaces.list()
            with pytest.raises(TimeoutError):
                listener.accept()
        finally:
            listener.close()
    finally:
        d.remove()


def test_the_daemon_socket_answers_the_sdk_like_any_client(daemon: Daemon) -> None:
    # The SDK speaks plain HTTP on the socket: the same routes and JSON as over TCP.
    with httpx.Client(transport=httpx.HTTPTransport(uds=str(daemon.socket)), base_url="http://localhost") as raw:
        assert raw.get("/v1/session").json()["credential"]["kind"] == "local"
