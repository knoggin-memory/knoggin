# SPDX-License-Identifier: Apache-2.0
"""One function per API call: each builds the request and says how to read the answer.

`Client` and `AsyncClient` expose these as their methods, so the two have the same signatures.
An argument left at None is not sent, so the server's default applies.
"""

from __future__ import annotations

from collections.abc import Sequence
from datetime import datetime
from typing import Any, Literal
from urllib.parse import quote

from ._transport import Op, body, items, query
from .models import (
    BatchResult,
    Context,
    ForgetManyResult,
    Forgotten,
    Memory,
    Moved,
    Namespace,
    NamespaceDeleted,
    NamespacePolicy,
    NamespaceStats,
    NewMemory,
    RecallResult,
    Reinforced,
    SimilarHit,
    StoredMemory,
)

Strings = Sequence[str]


def _seg(value: str) -> str:
    return quote(value, safe="")


def _time(value: datetime | None, name: str) -> str | None:
    if value is None:
        return None
    if value.tzinfo is None or value.utcoffset() is None:
        raise ValueError(f"{name} needs a time zone")
    return value.isoformat()


def context(topic: str | None = None, namespace: str | None = None, limit: int | None = 10) -> Op[Context]:
    """The start-of-session call: the namespaces, the newest memories (or a recall for `topic`) and the model's state."""
    return Op("GET", "/v1/context", Context.model_validate, query(topic=topic, namespace=namespace, limit=limit))


def store(
    summary: str,
    *,
    full_text: str | None = None,
    namespace: str | None = None,
    tags: Strings = (),
    entities: Strings = (),
    topics: Strings = (),
    emotions: Strings = (),
    parent_id: str | None = None,
    check_duplicates: bool = False,
) -> Op[StoredMemory]:
    """Saves one memory."""
    return Op(
        "POST",
        "/v1/memories",
        StoredMemory.model_validate,
        json=body(
            summary=summary,
            fullText=full_text,
            namespace=namespace,
            tags=tuple(tags),
            entities=tuple(entities),
            topics=tuple(topics),
            emotions=tuple(emotions),
            parentId=parent_id,
            checkDuplicates=check_duplicates or None,
        ),
        write=True,
    )


def store_many(items: Sequence[NewMemory]) -> Op[BatchResult]:
    """Saves 1 to 100 memories in one call. Each item succeeds or fails on its own."""
    memories = [m.model_dump(mode="json", by_alias=True, exclude_none=True) for m in items]
    return Op("POST", "/v1/memories/batch", BatchResult.model_validate, json={"memories": memories}, write=True)


def supersede(
    id: str,
    summary: str,
    *,
    full_text: str | None = None,
    tags: Strings = (),
    entities: Strings = (),
    topics: Strings = (),
    emotions: Strings = (),
    check_duplicates: bool = False,
) -> Op[StoredMemory]:
    """Saves a corrected memory that replaces `id` in recall."""
    return Op(
        "POST",
        f"/v1/memories/{_seg(id)}/supersede",
        StoredMemory.model_validate,
        json=body(
            summary=summary,
            fullText=full_text,
            tags=tuple(tags),
            entities=tuple(entities),
            topics=tuple(topics),
            emotions=tuple(emotions),
            checkDuplicates=check_duplicates or None,
        ),
        write=True,
    )


def recall(
    query: str,
    *,
    namespace: str | None = None,
    limit: int | None = 25,
    full_text_count: int | None = None,
    tags: Strings = (),
    entities: Strings = (),
    topics: Strings = (),
    emotions: Strings = (),
    min_strength: float | None = None,
    depth: int | None = 0,
    time_range: tuple[datetime, datetime] | None = None,
    detail: Literal["compact", "full"] = "compact",
    record_access: bool | None = None,
) -> Op[RecallResult]:
    """Finds memories by meaning. `time_range` is a pair of time-zone-aware datetimes (start, end)."""
    start = end = None
    if time_range is not None:
        if time_range[0] is None or time_range[1] is None:  # pyright: ignore[reportUnnecessaryComparison]
            raise ValueError("time_range needs both a start and an end")
        start, end = _time(time_range[0], "time_range start"), _time(time_range[1], "time_range end")
    return Op(
        "POST",
        "/v1/recall",
        RecallResult.model_validate,
        json=body(
            query=query,
            namespace=namespace,
            limit=limit,
            fullTextCount=full_text_count,
            tags=tuple(tags),
            entities=tuple(entities),
            topics=tuple(topics),
            emotions=tuple(emotions),
            minStrength=min_strength,
            depth=depth,
            timeRangeStart=start,
            timeRangeEnd=end,
            detail=detail,
            recordAccess=record_access,
        ),
    )


def get(id: str, *, record_access: bool | None = None, include_history: bool = False) -> Op[Memory]:
    """One memory with its full text."""
    return Op(
        "GET",
        f"/v1/memories/{_seg(id)}",
        Memory.model_validate,
        query(recordAccess=record_access, includeHistory=include_history or None),
    )


def list_page(
    *,
    namespace: str | None = None,
    tags: Strings = (),
    phase: Literal["full", "summary", "ghost"] | None = None,
    created_after: datetime | None = None,
    created_before: datetime | None = None,
    include_full_text: bool = False,
    limit: int | None = 50,
    cursor: str | None = None,
) -> Op[tuple[list[Memory], str | None]]:
    """One page of `GET /v1/memories`, newest first; `memories.list` follows the cursors."""
    return Op(
        "GET",
        "/v1/memories",
        lambda d: ([Memory.model_validate(m) for m in items(d)], d.get("nextCursor")),
        query(
            namespace=namespace,
            tag=tuple(tags),
            phase=phase,
            createdAfter=_time(created_after, "created_after"),
            createdBefore=_time(created_before, "created_before"),
            includeFullText=include_full_text or None,
            limit=limit,
            cursor=cursor,
        ),
    )


def reinforce(id: str, quality: int = 3) -> Op[Reinforced]:
    """Strengthens a memory: quality 1 (forgot) to 4 (easy)."""
    return Op(
        "POST", f"/v1/memories/{_seg(id)}/reinforce", Reinforced.model_validate, json={"quality": quality}, write=True
    )


def forget(id: str) -> Op[Forgotten]:
    """Erases one memory from the live store. `deleted` is False when there was no such memory."""
    return Op("DELETE", f"/v1/memories/{_seg(id)}", Forgotten.model_validate, write=True)


def forget_many(ids: Strings, *, dry_run: bool = False) -> Op[ForgetManyResult]:
    """Erases 1 to 100 memories. A dry run removes nothing and reports which IDs exist."""
    return Op(
        "POST",
        "/v1/memories/forget",
        ForgetManyResult.model_validate,
        json=body(ids=tuple(ids), dryRun=dry_run or None),
        write=True,
    )


def move(ids: Strings, destination_namespace: str, *, source_namespace: str | None = None) -> Op[Moved]:
    """Moves 1 to 100 memories to another namespace."""
    return Op(
        "POST",
        "/v1/memories/move",
        Moved.model_validate,
        json=body(ids=tuple(ids), destinationNamespace=destination_namespace, sourceNamespace=source_namespace),
        write=True,
    )


def similar(id: str, *, limit: int | None = 10, min_score: float | None = None) -> Op[list[SimilarHit]]:
    """The memories closest in meaning to `id`, without their full text."""
    return Op(
        "POST",
        f"/v1/memories/{_seg(id)}/similar",
        lambda d: [SimilarHit.model_validate(h) for h in items(d)],
        json=body(limit=limit, minScore=min_score),
    )


def namespaces_list() -> Op[list[Namespace]]:
    """Every namespace of the store, by name."""
    return Op("GET", "/v1/namespaces", lambda d: [Namespace.model_validate(n) for n in items(d)])


def namespaces_create(
    name: str, *, initial_stability: float | None = None, decay_rate_multiplier: float | None = None
) -> Op[Namespace]:
    """Creates a namespace."""
    return Op(
        "POST",
        "/v1/namespaces",
        Namespace.model_validate,
        json=body(name=name, initialStability=initial_stability, decayRateMultiplier=decay_rate_multiplier),
        write=True,
    )


def namespaces_stats(name: str) -> Op[NamespaceStats]:
    """Counts, strength and sizes of one namespace."""
    return Op("GET", f"/v1/namespaces/{_seg(name)}/stats", NamespaceStats.model_validate)


def namespaces_update_policy(name: str, policy: NamespacePolicy, *, dry_run: bool = False) -> Op[Namespace]:
    """Replaces a namespace's decay policy. A dry run changes nothing and reports `would_change`."""
    return Op(
        "PUT",
        f"/v1/namespaces/{_seg(name)}/policy",
        Namespace.model_validate,
        query(dryRun=dry_run or None),
        json=policy.model_dump(mode="json", by_alias=True),
        write=True,
    )


def namespaces_delete(name: str, *, force: bool = False, dry_run: bool = False) -> Op[NamespaceDeleted]:
    """Deletes a namespace; one that holds memories needs `force`."""
    return Op(
        "DELETE",
        f"/v1/namespaces/{_seg(name)}",
        NamespaceDeleted.model_validate,
        query(force=force or None, dryRun=dry_run or None),
        write=True,
    )


def tags_page(*, namespace: str | None = None, prefix: str | None = None, cursor: str | None = None) -> Op[Any]:
    """One page of `GET /v1/tags`; `tags.list` follows the cursors."""
    return Op("GET", "/v1/tags", lambda d: d, query(namespace=namespace, prefix=prefix, limit=200, cursor=cursor))
