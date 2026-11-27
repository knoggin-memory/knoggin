# SPDX-License-Identifier: AGPL-3.0-only
"""What `Client` and `AsyncClient` share: one request description, the retry rule, and the error mapping."""

from __future__ import annotations

import asyncio
import random
import time
import uuid
import warnings
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from importlib import metadata
from typing import Any, Generic, TypeVar, cast

import httpx

from ._config import NOT_RUNNING, Settings, check_socket
from .errors import (
    CODE_CLASSES,
    ConfigurationError,
    KnogginConnectionError,
    KnogginError,
    KnogginTimeoutError,
    RateLimitedError,
    ServerError,
    WriteOutcomeUnknown,
)

T = TypeVar("T")

VERSION = metadata.version("knoggin")

RETRIED_CODES = frozenset({"rate_limited", "unavailable", "upgrading", "timeout"})
BACKOFF_START = 0.5

# Replaced in tests, so that retries do not wait.
sleep: Callable[[float], None] = time.sleep
async_sleep: Callable[[float], Any] = asyncio.sleep


@dataclass
class Op(Generic[T]):
    """One API call: the request, whether it is a write, and how to read a success."""

    method: str
    path: str
    parse: Callable[[Any], T]
    params: httpx.QueryParams | None = None
    json: Any = None
    write: bool = False
    idempotency_key: str = field(default_factory=lambda: str(uuid.uuid4()))


def query(**values: Any) -> httpx.QueryParams:
    """Query parameters from camel-case names; None is left out, booleans are `true`/`false`, sequences repeat."""
    out: list[tuple[str, str | int | float | bool | None]] = []
    for name, value in values.items():
        if value is None:
            continue
        seq = cast(Sequence[object], value) if isinstance(value, list | tuple) else (value,)
        out.extend((name, ("true" if v else "false") if isinstance(v, bool) else str(v)) for v in seq)
    return httpx.QueryParams(out)


def body(**values: Any) -> dict[str, Any]:
    """A JSON body from camel-case names; None and empty tuples are left out, so the server's default applies."""
    return {k: list(v) if isinstance(v, tuple) else v for k, v in values.items() if v is not None and v != ()}  # pyright: ignore[reportUnknownArgumentType]


def headers(settings: Settings) -> dict[str, str]:
    h = {"User-Agent": f"knoggin-python/{VERSION}", "Accept": "application/json"}
    if settings.api_key:
        h["Authorization"] = f"Bearer {settings.api_key}"
    if settings.namespace:
        h["Knoggin-Namespace"] = settings.namespace
    return h


def client_args(settings: Settings) -> dict[str, Any]:
    """The arguments of `httpx.Client` and `httpx.AsyncClient`, without the transport."""
    return {
        "base_url": settings.base_url or "http://localhost",
        "headers": headers(settings),
        "timeout": settings.timeout,
        "follow_redirects": False,
    }


def before_send(settings: Settings) -> None:
    if settings.base_url is None:
        check_socket(settings)


def api_error(response: httpx.Response) -> KnogginError:
    """The error of an answer that is not a success."""
    err: dict[str, Any] = {}
    try:
        data = response.json()
        if isinstance(data, dict) and isinstance(data.get("error"), dict):  # pyright: ignore[reportUnknownMemberType]
            err = cast(dict[str, Any], data["error"])
    except ValueError:
        pass
    code = err.get("code") if isinstance(err.get("code"), str) else None
    message = str(err.get("message") or f"HTTP {response.status_code}")
    cls = CODE_CLASSES.get(code or "", ServerError if response.status_code >= 500 else KnogginError)
    e = cls(
        message,
        code=code,
        field=err.get("field"),
        reason=err.get("reason"),
        request_id=err.get("requestId") or response.headers.get("Knoggin-Request-Id"),
        status=response.status_code,
    )
    if isinstance(e, RateLimitedError):
        e.retry_after = retry_after(response)
    return e


def retry_after(response: httpx.Response) -> float | None:
    try:
        return max(0.0, float(response.headers["Retry-After"]))
    except (KeyError, ValueError):
        return None


def no_answer(e: httpx.TransportError, settings: Settings) -> KnogginError:
    if settings.base_url is None and isinstance(e, httpx.ConnectError):
        # The socket file is there, but nothing accepts on it.
        return ConfigurationError(NOT_RUNNING)
    if isinstance(e, httpx.TimeoutException):
        return KnogginTimeoutError(f"no answer in time: {type(e).__name__}")
    return KnogginConnectionError(f"no answer: {e}")


_expiry_warned = False


def note_expiry(response: httpx.Response) -> None:
    """Warn once per process when the server says the API key expires soon."""
    global _expiry_warned
    expires = response.headers.get("Knoggin-Key-Expires")
    if expires and not _expiry_warned:
        _expiry_warned = True
        warnings.warn(f"The Knoggin API key expires at {expires}. Rotate it before then.", stacklevel=4)


@dataclass
class Attempt:
    """The state of one call across its attempts."""

    op: Op[Any]
    max_retries: int
    number: int = 0
    last: KnogginError | None = None
    unanswered: bool = False
    """Whether an attempt got no answer, or only a retryable error, so a write may have been applied."""

    def request_headers(self) -> dict[str, str]:
        h: dict[str, str] = {}
        if self.op.write:
            h["Idempotency-Key"] = self.op.idempotency_key
        return h

    def outcome(self, response: httpx.Response | None, error: KnogginError | None) -> tuple[bool, float]:
        """After one attempt: whether to try again, and how long to wait first."""
        if response is not None:
            note_expiry(response)
            error = api_error(response)
        assert error is not None
        self.last = error
        retryable = isinstance(error, KnogginConnectionError | KnogginTimeoutError) or error.code in RETRIED_CODES
        self.unanswered |= retryable
        if not retryable or self.number >= self.max_retries:
            return False, 0.0
        self.number += 1
        wait = BACKOFF_START * 2 ** (self.number - 1) * random.uniform(0.5, 1.5)  # noqa: S311 - jitter, not a secret
        if response is not None and (after := retry_after(response)) is not None:
            wait = after
        return True, wait

    def failure(self) -> KnogginError:
        """The error to raise once no attempt is left.

        A write is unknown when any attempt went unanswered, even if a later one was refused for good: the
        earlier one may have been applied. The last error stays the cause.
        """
        last = self.last
        assert last is not None
        if self.op.write and self.unanswered:
            e = WriteOutcomeUnknown(
                f"The write may or may not have been applied: {last.message}",
                code=last.code,
                field=last.field,
                reason=last.reason,
                request_id=last.request_id,
                status=last.status,
            )
            e.__cause__ = last
            return e
        return last


def parse(op: Op[T], response: httpx.Response) -> T:
    note_expiry(response)
    return op.parse(response.json() if response.content else None)


def items(data: Mapping[str, Any]) -> list[Any]:
    return list(data["items"])
