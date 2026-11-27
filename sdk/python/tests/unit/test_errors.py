# SPDX-License-Identifier: AGPL-3.0-only
"""Error mapping, retries with `Retry-After`, idempotency-key reuse, the key-expiry warning, and the two local rules."""

from __future__ import annotations

import asyncio
import json
import os
import socket
import warnings
from pathlib import Path
from typing import Any

import httpx
import pytest
import respx

from knoggin import (
    AsyncClient,
    AuthenticationError,
    Client,
    ConfigurationError,
    ConflictError,
    InvalidRequestError,
    KnogginConnectionError,
    KnogginError,
    KnogginTimeoutError,
    NotFoundError,
    PermissionDeniedError,
    RateLimitedError,
    ServerError,
    StoreUnavailableError,
    WriteOutcomeUnknown,
    _transport,
)
from knoggin.errors import CODE_CLASSES

from .conftest import BASE, KEY, STORED, error, requests

OPENAPI = Path(__file__).resolve().parents[4] / "docs" / "openapi.json"

EXPECTED = {
    "invalid_argument": InvalidRequestError,
    "payload_too_large": InvalidRequestError,
    "unauthenticated": AuthenticationError,
    "permission_denied": PermissionDeniedError,
    "not_found": NotFoundError,
    "already_exists": ConflictError,
    "conflict": ConflictError,
    "idempotency_conflict": ConflictError,
    "namespace_not_empty": ConflictError,
    "rate_limited": RateLimitedError,
    "store_locked": StoreUnavailableError,
    "store_deleting": StoreUnavailableError,
    "resource_exhausted": StoreUnavailableError,
    "internal": ServerError,
    "unavailable": ServerError,
    "upgrading": ServerError,
    "timeout": ServerError,
}


def test_every_code_of_the_server_maps_to_a_class() -> None:
    codes = json.loads(OPENAPI.read_text())["components"]["schemas"]["Code"]["enum"]
    assert sorted(codes) == sorted(CODE_CLASSES)
    assert CODE_CLASSES == EXPECTED


@pytest.mark.parametrize("code", sorted(EXPECTED))
def test_each_code_raises_its_class_with_the_fields(api: respx.MockRouter, code: str) -> None:
    api.get("/v1/namespaces").respond(400, json=error(code, "bad", field="name", reason="why"))
    with pytest.raises(EXPECTED[code]) as caught:
        Client(base_url=BASE, max_retries=0).namespaces.list()
    e = caught.value
    assert (e.code, e.message, e.field, e.reason, e.status) == (code, "bad", "name", "why", 400)
    assert e.request_id == "0193a1f2-0000-7000-8000-000000000000"


def test_an_answer_that_is_not_the_error_shape(api: respx.MockRouter) -> None:
    api.get("/v1/namespaces").respond(502, text="<html>bad gateway</html>")
    with pytest.raises(ServerError) as caught:
        Client(base_url=BASE).namespaces.list()
    assert caught.value.code is None and caught.value.status == 502


@pytest.mark.parametrize("code", ["rate_limited", "unavailable", "upgrading", "timeout"])
def test_retryable_codes_are_retried_honouring_retry_after(
    api: respx.MockRouter, no_waiting: list[float], code: str
) -> None:
    route = api.get("/v1/namespaces")
    route.side_effect = [
        httpx.Response(503, json=error(code), headers={"Retry-After": "7"}),
        httpx.Response(503, json=error(code)),
        httpx.Response(200, json={"items": []}),
    ]
    assert Client(base_url=BASE).namespaces.list() == []
    assert route.call_count == 3
    assert no_waiting[0] == 7.0
    assert 0.5 <= no_waiting[1] <= 1.5  # second wait: jittered 1 s


def test_other_codes_are_not_retried(api: respx.MockRouter) -> None:
    route = api.get("/v1/namespaces").respond(409, json=error("conflict"))
    with pytest.raises(ConflictError):
        Client(base_url=BASE).namespaces.list()
    assert route.call_count == 1


def test_a_read_that_keeps_failing_raises_the_last_error(api: respx.MockRouter) -> None:
    route = api.get("/v1/namespaces").respond(429, json=error("rate_limited"), headers={"Retry-After": "3"})
    with pytest.raises(RateLimitedError) as caught:
        Client(base_url=BASE, max_retries=2).namespaces.list()
    assert route.call_count == 3
    assert caught.value.retry_after == 3.0


@pytest.mark.parametrize(
    ("failure", "cls"),
    [(httpx.ConnectError("refused"), KnogginConnectionError), (httpx.ReadTimeout("slow"), KnogginTimeoutError)],
)
def test_no_answer_on_a_read(api: respx.MockRouter, failure: Exception, cls: type[KnogginError]) -> None:
    route = api.get("/v1/namespaces").mock(side_effect=failure)
    with pytest.raises(cls):
        Client(base_url=BASE, max_retries=1).namespaces.list()
    assert route.call_count == 2


@pytest.mark.parametrize("kind", ["sync", "async"])
def test_a_retried_write_reuses_its_idempotency_key(api: respx.MockRouter, kind: str) -> None:
    route = api.post("/v1/memories")
    route.side_effect = [
        httpx.ReadTimeout("lost"),
        httpx.Response(503, json=error("unavailable")),
        httpx.Response(201, json=STORED),
    ]
    if kind == "sync":
        Client(base_url=BASE).memories.store("x")
    else:
        asyncio.run(AsyncClient(base_url=BASE).memories.store("x"))
    keys = {r.headers["idempotency-key"] for r in requests(route)}
    assert route.call_count == 3 and len(keys) == 1


def test_each_call_makes_its_own_key(api: respx.MockRouter) -> None:
    route = api.post("/v1/memories").respond(201, json=STORED)
    c = Client(base_url=BASE)
    c.memories.store("x")
    c.memories.store("x")
    first, second = requests(route)
    assert first.headers["idempotency-key"] != second.headers["idempotency-key"]


@pytest.mark.parametrize(
    "answers",
    [
        [httpx.ConnectError("refused")] * 3,
        [httpx.ReadTimeout("lost")] * 3,
        [httpx.Response(503, json=error("unavailable"))] * 3,
    ],
)
def test_a_write_without_an_outcome(api: respx.MockRouter, answers: list[Any]) -> None:
    api.post("/v1/memories").side_effect = answers
    with pytest.raises(WriteOutcomeUnknown) as caught:
        Client(base_url=BASE).memories.store("x")
    assert isinstance(caught.value.__cause__, KnogginError)


@pytest.mark.parametrize("kind", ["sync", "async"])
@pytest.mark.parametrize(
    "answers",
    [
        [httpx.ReadTimeout("lost"), httpx.Response(423, json=error("store_locked"))],
        [httpx.Response(503, json=error("unavailable")), httpx.Response(400, json=error("invalid_argument"))],
    ],
)
def test_a_write_refused_after_an_unanswered_attempt_is_unknown(
    api: respx.MockRouter, answers: list[Any], kind: str
) -> None:
    api.post("/v1/memories").side_effect = answers
    with pytest.raises(WriteOutcomeUnknown) as caught:
        if kind == "sync":
            Client(base_url=BASE).memories.store("x")
        else:
            asyncio.run(AsyncClient(base_url=BASE).memories.store("x"))
    assert isinstance(caught.value.__cause__, StoreUnavailableError | InvalidRequestError)


def test_a_write_whose_retry_finds_no_daemon_is_unknown() -> None:
    attempt = _transport.Attempt(_transport.Op("POST", "/v1/memories", lambda d: None, write=True), max_retries=3)
    assert attempt.outcome(None, KnogginTimeoutError("lost"))[0]
    assert not attempt.outcome(None, ConfigurationError("not running"))[0]
    failure = attempt.failure()
    assert isinstance(failure, WriteOutcomeUnknown) and isinstance(failure.__cause__, ConfigurationError)


def test_a_write_refused_for_good_is_not_unknown(api: respx.MockRouter) -> None:
    api.post("/v1/memories").respond(400, json=error("invalid_argument", field="summary"))
    with pytest.raises(InvalidRequestError):
        Client(base_url=BASE).memories.store("x")


def test_key_expiry_warns_once_per_process(api: respx.MockRouter, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(_transport, "_expiry_warned", False)
    api.get("/v1/namespaces").respond(json={"items": []}, headers={"Knoggin-Key-Expires": "2026-12-01T00:00:00.000Z"})
    c = Client(base_url=BASE, api_key=KEY)
    with warnings.catch_warnings(record=True) as seen:
        warnings.simplefilter("always")
        c.namespaces.list()
        c.namespaces.list()
    assert [str(w.message) for w in seen] == [
        "The Knoggin API key expires at 2026-12-01T00:00:00.000Z. Rotate it before then."
    ]


# --- the key rule -------------------------------------------------------------------------------


@pytest.mark.parametrize(
    "url", ["http://memory.example.com", "http://10.0.0.5:7690", "http://[::2]:80", "http://localhost.example.com"]
)
def test_a_key_is_refused_over_plain_http_to_another_host(url: str) -> None:
    with pytest.raises(ConfigurationError, match="plain http"):
        Client(base_url=url, api_key=KEY)


@pytest.mark.parametrize(
    "url",
    [
        "http://localhost:7690",
        "http://127.0.0.1:7690",
        "http://127.9.8.7",
        "http://[::1]:7690",
        "https://memory.example.com",
    ],
)
def test_a_key_may_go_to_loopback_or_https(url: str) -> None:
    Client(base_url=url, api_key=KEY)


def test_plain_http_without_a_key_is_fine_and_the_environment_is_read(monkeypatch: pytest.MonkeyPatch) -> None:
    Client(base_url="http://memory.example.com")
    monkeypatch.setenv("KNOGGIN_BASE_URL", "http://memory.example.com")
    monkeypatch.setenv("KNOGGIN_API_KEY", KEY)
    with pytest.raises(ConfigurationError):
        Client()
    Client(base_url="https://memory.example.com")  # an argument wins over the environment


# --- the local rule -----------------------------------------------------------------------------


def _private_home(tmp_path: Path) -> Path:
    home = tmp_path / "k"
    (home / "run").mkdir(parents=True, mode=0o700)
    home.chmod(0o700)
    return home


@pytest.mark.parametrize("kind", ["sync", "async"])
def test_no_daemon_raises_configuration_error_and_opens_no_tcp_connection(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, kind: str
) -> None:
    monkeypatch.setenv("KNOGGIN_HOME", str(_private_home(tmp_path)))
    try:  # another program on the daemon's usual port, or on any port when that one is taken
        listener = socket.create_server(("127.0.0.1", 7690))
    except OSError:
        listener = socket.create_server(("127.0.0.1", 0))
    listener.settimeout(0.2)
    connects: list[Any] = []
    real = socket.socket.connect

    def connect(sock: socket.socket, address: Any) -> None:
        connects.append(address)
        real(sock, address)

    monkeypatch.setattr(socket.socket, "connect", connect)
    try:
        with pytest.raises(ConfigurationError, match="knoggin start"):
            if kind == "sync":
                Client().namespaces.list()
            else:
                asyncio.run(AsyncClient().namespaces.list())
        with pytest.raises(TimeoutError):
            listener.accept()
    finally:
        listener.close()
    assert connects == []


def test_a_socket_nobody_listens_on_is_not_running(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    short = Path(os.environ.get("TMPDIR", "/tmp")) / f"kgsdk{os.getpid()}"
    home = _private_home(short)
    try:
        dead = socket.socket(socket.AF_UNIX)
        dead.bind(str(home / "run" / "daemon.sock"))
        dead.close()  # the file stays, nothing accepts
        monkeypatch.setenv("KNOGGIN_HOME", str(home))
        with pytest.raises(ConfigurationError, match="knoggin start"):
            Client().namespaces.list()
    finally:
        import shutil

        shutil.rmtree(short, ignore_errors=True)


def test_a_home_that_others_can_write_is_refused(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    home = _private_home(tmp_path)
    (home / "run" / "daemon.sock").touch()
    home.chmod(0o777)
    monkeypatch.setenv("KNOGGIN_HOME", str(home))
    with pytest.raises(ConfigurationError, match="cannot write"):
        Client().namespaces.list()
    home.chmod(0o700)
    (home / "run").chmod(0o770)
    with pytest.raises(ConfigurationError, match="cannot write"):
        Client().namespaces.list()


def test_windows_needs_a_base_url(monkeypatch: pytest.MonkeyPatch) -> None:
    import sys

    monkeypatch.setattr(sys, "platform", "win32")
    with pytest.raises(ConfigurationError, match="Windows"):
        Client()
