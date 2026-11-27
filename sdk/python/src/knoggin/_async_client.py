# SPDX-License-Identifier: AGPL-3.0-only
"""The asynchronous client: the same methods as `Client`, as coroutines."""

from __future__ import annotations

import asyncio
import builtins
import functools
import os
from collections.abc import AsyncIterator, Callable, Coroutine, Sequence
from datetime import datetime
from pathlib import Path
from types import TracebackType
from typing import Any, BinaryIO, Concatenate, Literal, ParamSpec, Self, TypeVar

import httpx
from pydantic import BaseModel

from . import _ops, _transport, typed
from ._config import resolve
from ._transport import Attempt, Op, api_error, before_send, client_args, no_answer, note_expiry, parse, query
from .errors import WriteOutcomeUnknown
from .models import ImportResult, Memory, TagCount

P = ParamSpec("P")
T = TypeVar("T")
M = TypeVar("M", bound=BaseModel)


class _Runner:
    async def _run(self, op: Op[T]) -> T:
        raise NotImplementedError


def _call(build: Callable[P, Op[T]]) -> Callable[Concatenate[_Runner, P], Coroutine[Any, Any, T]]:
    """Turns a request builder of `_ops` into a coroutine method that sends it."""

    @functools.wraps(build)
    async def method(self: _Runner, /, *args: P.args, **kwargs: P.kwargs) -> T:
        return await self._run(build(*args, **kwargs))

    return method


class _Part(_Runner):
    def __init__(self, client: AsyncClient) -> None:
        self._client = client

    async def _run(self, op: Op[T]) -> T:
        return await self._client._run(op)  # pyright: ignore[reportPrivateUsage]


class AsyncMemories(_Part):
    """`client.memories`: save, recall, read, change and forget memories."""

    store = _call(_ops.store)
    store_many = _call(_ops.store_many)
    supersede = _call(_ops.supersede)
    recall = _call(_ops.recall)
    get = _call(_ops.get)
    reinforce = _call(_ops.reinforce)
    forget = _call(_ops.forget)
    forget_many = _call(_ops.forget_many)
    move = _call(_ops.move)
    similar = _call(_ops.similar)
    store_model = _call(typed.store_model)

    async def recall_models(self, query: str, as_: type[M], **kwargs: Any) -> builtins.list[M]:
        """Recalls memories saved with `store_model` from class `as_`, parsed into it."""
        return typed.parse_models(await self._run(typed.recall_models_op(query, as_, **kwargs)), as_)

    async def list(
        self,
        *,
        namespace: str | None = None,
        tags: Sequence[str] = (),
        phase: Literal["full", "summary", "ghost"] | None = None,
        created_after: datetime | None = None,
        created_before: datetime | None = None,
        include_full_text: bool = False,
        limit: int | None = 50,
    ) -> AsyncIterator[Memory]:
        """Every memory that matches, newest first, `limit` per request; follows the cursors."""
        cursor = None
        while True:
            page, cursor = await self._run(
                _ops.list_page(
                    namespace=namespace,
                    tags=tags,
                    phase=phase,
                    created_after=created_after,
                    created_before=created_before,
                    include_full_text=include_full_text,
                    limit=limit,
                    cursor=cursor,
                )
            )
            for m in page:
                yield m
            if not cursor:
                return


class AsyncNamespaces(_Part):
    """`client.namespaces`."""

    list = _call(_ops.namespaces_list)
    create = _call(_ops.namespaces_create)
    stats = _call(_ops.namespaces_stats)
    update_policy = _call(_ops.namespaces_update_policy)
    delete = _call(_ops.namespaces_delete)


class AsyncTags(_Part):
    """`client.tags`."""

    async def list(self, *, namespace: str | None = None, prefix: str | None = None) -> AsyncIterator[TagCount]:
        """Every plain tag with its count, by count then tag; follows the cursors."""
        cursor = None
        while True:
            data = await self._run(_ops.tags_page(namespace=namespace, prefix=prefix, cursor=cursor))
            for t in data["items"]:
                yield TagCount.model_validate(t)
            cursor = data.get("nextCursor")
            if not cursor:
                return


async def _chunks(f: BinaryIO) -> AsyncIterator[bytes]:
    while chunk := await asyncio.to_thread(f.read, 65536):
        yield chunk


class AsyncClient(_Runner):
    """The asynchronous Knoggin client. Without `base_url` it talks to the local daemon over its Unix socket.

    Arguments win over the environment variables `KNOGGIN_BASE_URL`, `KNOGGIN_API_KEY` and
    `KNOGGIN_NAMESPACE`. `namespace` is the default namespace of every request.
    """

    def __init__(
        self,
        base_url: str | None = None,
        api_key: str | None = None,
        namespace: str | None = None,
        timeout: float = 30.0,
        max_retries: int = 2,
    ) -> None:
        self._settings = resolve(base_url, api_key, namespace, timeout, max_retries)
        uds = None if self._settings.base_url else str(self._settings.socket)
        self._http = httpx.AsyncClient(transport=httpx.AsyncHTTPTransport(uds=uds), **client_args(self._settings))
        self.memories = AsyncMemories(self)
        self.namespaces = AsyncNamespaces(self)
        self.tags = AsyncTags(self)

    context = _call(_ops.context)

    async def export(self, path: str | os.PathLike[str]) -> None:
        """Streams the whole store to a new file at `path`, with mode 0600. An existing file is never overwritten."""
        target = Path(path)
        fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            with os.fdopen(fd, "wb") as f:
                attempt = Attempt(Op("GET", "/v1/export", lambda d: None), self._settings.max_retries)
                while True:
                    before_send(self._settings)
                    try:
                        async with self._http.stream("GET", "/v1/export", timeout=self._long_timeout()) as r:
                            if r.is_success:
                                note_expiry(r)
                                f.seek(0)
                                f.truncate()
                                async for chunk in r.aiter_bytes():
                                    f.write(chunk)
                                return
                            await r.aread()
                            again, wait = attempt.outcome(r, None)
                    except httpx.TransportError as e:
                        again, wait = attempt.outcome(None, no_answer(e, self._settings))
                    if not again:
                        raise attempt.failure()
                    await _transport.async_sleep(wait)
        except BaseException:
            target.unlink(missing_ok=True)
            raise

    async def import_(
        self, path: str | os.PathLike[str], *, on_conflict: Literal["skip", "error"] = "skip"
    ) -> ImportResult:
        """Streams an export file into the store. Sent once and never retried; sending the same file again skips what is there."""
        before_send(self._settings)
        with open(path, "rb") as f:
            try:
                r = await self._http.post(
                    "/v1/import",
                    params=query(onConflict=on_conflict),
                    content=_chunks(f),
                    headers={"Content-Type": "application/x-ndjson"},
                    timeout=self._long_timeout(),
                )
            except httpx.TransportError as e:
                err = no_answer(e, self._settings)
                if isinstance(e, httpx.ConnectError | httpx.ConnectTimeout):
                    raise err from None  # no connection was made, so nothing was sent
                raise WriteOutcomeUnknown(f"The import may or may not have been applied: {err.message}") from err
        if r.is_success:
            return parse(Op("POST", "/v1/import", ImportResult.model_validate), r)
        note_expiry(r)
        raise api_error(r)

    async def aclose(self) -> None:
        await self._http.aclose()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc: type[BaseException] | BaseException | TracebackType | None) -> None:
        await self.aclose()

    def _long_timeout(self) -> httpx.Timeout:
        return httpx.Timeout(self._settings.timeout, read=None)

    async def _run(self, op: Op[T]) -> T:
        attempt = Attempt(op, self._settings.max_retries)
        while True:
            before_send(self._settings)
            headers = attempt.request_headers()
            try:
                r = await self._http.request(op.method, op.path, params=op.params, json=op.json, headers=headers)
            except httpx.TransportError as e:
                again, wait = attempt.outcome(None, no_answer(e, self._settings))
            else:
                if r.is_success:
                    return parse(op, r)
                again, wait = attempt.outcome(r, None)
            if not again:
                raise attempt.failure()
            await _transport.async_sleep(wait)
