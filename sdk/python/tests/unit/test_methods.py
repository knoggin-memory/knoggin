# SPDX-License-Identifier: Apache-2.0
"""Every method of `Client` and `AsyncClient` against a mocked server: the request it sends and the model it returns."""

from __future__ import annotations

import asyncio
import inspect
import json
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

import httpx
import pytest
import respx

from knoggin import AsyncClient, Client, NamespacePolicy, NewMemory, models

from .conftest import BASE, COMPACT, CONTEXT, ID, KEY, MEMORY, NAMESPACE, POLICY, STATS, STORED, requests

START = datetime(2026, 1, 1, tzinfo=UTC)
END = datetime(2026, 2, 1, tzinfo=UTC)


@dataclass
class Case:
    name: str
    call: Callable[[Any], Any]
    method: str
    path: str
    answer: Any
    returns: Callable[[Any], bool]
    status: int = 200
    body: Any = None
    params: list[tuple[str, str]] = field(default_factory=lambda: [])
    write: bool = False


CASES = [
    Case(
        "context",
        lambda c: c.context(topic="trip"),
        "GET",
        "/v1/context",
        CONTEXT,
        lambda r: isinstance(r, models.Context) and r.model.name == "Fake model",
        params=[("topic", "trip"), ("limit", "10")],
    ),
    Case(
        "store",
        lambda c: c.memories.store("Maya prefers window seats", full_text="x", tags=["travel"], check_duplicates=True),
        "POST",
        "/v1/memories",
        STORED,
        lambda r: isinstance(r, models.StoredMemory) and r.id == ID,
        status=201,
        body={"summary": "Maya prefers window seats", "fullText": "x", "tags": ["travel"], "checkDuplicates": True},
        write=True,
    ),
    Case(
        "store_many",
        lambda c: c.memories.store_many([NewMemory(summary="a", tags=["x"]), NewMemory(summary="b", namespace="w")]),
        "POST",
        "/v1/memories/batch",
        {"results": [{"index": 0, "status": "ok", "memory": STORED}], "stored": 1, "errors": 0},
        lambda r: isinstance(r, models.BatchResult) and r.results[0].memory is not None,
        body={
            "memories": [
                {"summary": "a", "tags": ["x"], "entities": [], "topics": [], "emotions": []},
                {"summary": "b", "namespace": "w", "tags": [], "entities": [], "topics": [], "emotions": []},
            ]
        },
        write=True,
    ),
    Case(
        "supersede",
        lambda c: c.memories.supersede(ID, "Maya prefers aisle seats", entities=["Maya"]),
        "POST",
        f"/v1/memories/{ID}/supersede",
        {**STORED, "supersedes": ID},
        lambda r: isinstance(r, models.StoredMemory) and r.supersedes == ID,
        status=201,
        body={"summary": "Maya prefers aisle seats", "entities": ["Maya"]},
        write=True,
    ),
    Case(
        "recall",
        lambda c: c.memories.recall("seats", tags=["travel"], time_range=(START, END), record_access=False),
        "POST",
        "/v1/recall",
        {"memories": [{**COMPACT, "fullText": "x"}], "count": 1},
        lambda r: isinstance(r, models.RecallResult) and isinstance(r.memories[0], models.CompactHit),
        body={
            "query": "seats",
            "limit": 25,
            "tags": ["travel"],
            "depth": 0,
            "timeRangeStart": "2026-01-01T00:00:00+00:00",
            "timeRangeEnd": "2026-02-01T00:00:00+00:00",
            "detail": "compact",
            "recordAccess": False,
        },
    ),
    Case(
        "recall_full",
        lambda c: c.memories.recall("seats", detail="full", limit=5, full_text_count=2),
        "POST",
        "/v1/recall",
        {
            "memories": [{**MEMORY, "score": 0.5, "related": [{"id": ID, "edgeType": "entity", "weight": 0.4}]}],
            "count": 1,
        },
        lambda r: isinstance(r.memories[0], models.FullHit) and r.memories[0].related[0].edge_type == "entity",
        body={"query": "seats", "limit": 5, "fullTextCount": 2, "depth": 0, "detail": "full"},
    ),
    Case(
        "get",
        lambda c: c.memories.get(ID, record_access=False, include_history=True),
        "GET",
        f"/v1/memories/{ID}",
        {**MEMORY, "accessHistory": [{"at": "2026-11-23T11:00:00.000Z", "kind": "direct"}]},
        lambda r: isinstance(r, models.Memory) and r.access_history is not None and r.saved_by.startswith("api_key"),
        params=[("recordAccess", "false"), ("includeHistory", "true")],
    ),
    Case(
        "reinforce",
        lambda c: c.memories.reinforce(ID, quality=4),
        "POST",
        f"/v1/memories/{ID}/reinforce",
        {"id": ID, "strength": 1.0, "stability": 9.0, "phase": "full", "isPermastore": False},
        lambda r: isinstance(r, models.Reinforced) and r.stability == 9.0,
        body={"quality": 4},
        write=True,
    ),
    Case(
        "forget",
        lambda c: c.memories.forget(ID),
        "DELETE",
        f"/v1/memories/{ID}",
        {"id": ID, "deleted": True},
        lambda r: isinstance(r, models.Forgotten) and r.deleted,
        write=True,
    ),
    Case(
        "forget_many",
        lambda c: c.memories.forget_many([ID], dry_run=True),
        "POST",
        "/v1/memories/forget",
        {"results": [{"index": 0, "id": ID, "deleted": True}], "deleted": 1},
        lambda r: isinstance(r, models.ForgetManyResult) and r.deleted == 1,
        body={"ids": [ID], "dryRun": True},
        write=True,
    ),
    Case(
        "move",
        lambda c: c.memories.move([ID], "work", source_namespace="default"),
        "POST",
        "/v1/memories/move",
        {"moved": 1, "destinationNamespace": "work"},
        lambda r: isinstance(r, models.Moved) and r.destination_namespace == "work",
        body={"ids": [ID], "destinationNamespace": "work", "sourceNamespace": "default"},
        write=True,
    ),
    Case(
        "similar",
        lambda c: c.memories.similar(ID, limit=3, min_score=0.2),
        "POST",
        f"/v1/memories/{ID}/similar",
        {"items": [{"memory": {k: v for k, v in MEMORY.items() if k != "fullText"}, "score": 0.8}]},
        lambda r: isinstance(r, list) and isinstance(r[0], models.SimilarHit),
        body={"limit": 3, "minScore": 0.2},
    ),
    Case(
        "namespaces.list",
        lambda c: c.namespaces.list(),
        "GET",
        "/v1/namespaces",
        {"items": [NAMESPACE]},
        lambda r: isinstance(r[0], models.Namespace) and r[0].policy.phase_thresholds.ghost_to_delete == 0.05,
    ),
    Case(
        "namespaces.create",
        lambda c: c.namespaces.create("work", initial_stability=5.0),
        "POST",
        "/v1/namespaces",
        NAMESPACE,
        lambda r: isinstance(r, models.Namespace),
        status=201,
        body={"name": "work", "initialStability": 5.0},
        write=True,
    ),
    Case(
        "namespaces.stats",
        lambda c: c.namespaces.stats("a b/c"),
        "GET",
        "/v1/namespaces/a%20b%2Fc/stats",
        STATS,
        lambda r: isinstance(r, models.NamespaceStats) and r.phase_counts.full == 3,
    ),
    Case(
        "namespaces.update_policy",
        lambda c: c.namespaces.update_policy("work", NamespacePolicy.model_validate(POLICY), dry_run=True),
        "PUT",
        "/v1/namespaces/work/policy",
        {**NAMESPACE, "wouldChange": {"fullToSummary": 1, "summaryToGhost": 0, "deleted": 0}},
        lambda r: isinstance(r, models.Namespace) and r.would_change is not None,
        body=POLICY,
        params=[("dryRun", "true")],
        write=True,
    ),
    Case(
        "namespaces.delete",
        lambda c: c.namespaces.delete("work", force=True),
        "DELETE",
        "/v1/namespaces/work",
        {"name": "work", "deleted": True, "memoriesDeleted": 3, "edgesRemoved": 1},
        lambda r: isinstance(r, models.NamespaceDeleted) and r.memories_deleted == 3,
        params=[("force", "true")],
        write=True,
    ),
]


def run(result: Any) -> Any:
    return asyncio.run(result) if inspect.iscoroutine(result) else result


def make(kind: str, **kwargs: Any) -> Any:
    return (Client if kind == "sync" else AsyncClient)(base_url=BASE, api_key=KEY, **kwargs)


@pytest.mark.parametrize("kind", ["sync", "async"])
@pytest.mark.parametrize("case", CASES, ids=[c.name for c in CASES])
def test_method_sends_its_request_and_reads_the_answer(api: respx.MockRouter, case: Case, kind: str) -> None:
    route = api.request(case.method, case.path).respond(case.status, json=case.answer)
    result = run(case.call(make(kind, namespace="work")))
    assert case.returns(result), result
    req = route.calls.last.request
    assert req.headers["authorization"] == f"Bearer {KEY}"
    assert req.headers["knoggin-namespace"] == "work"
    assert list(req.url.params.multi_items()) == case.params
    assert json.loads(req.content) == case.body if case.body is not None else req.content == b""
    if case.write:
        assert len(req.headers["idempotency-key"]) == 36
    else:
        assert "idempotency-key" not in req.headers
    assert not any(h.lower().startswith("knoggin-") and h.lower() != "knoggin-namespace" for h in req.headers)


def test_an_argument_left_at_none_is_not_sent(api: respx.MockRouter) -> None:
    route = api.post("/v1/recall").respond(json={"memories": [], "count": 0})
    Client(base_url=BASE).memories.recall("q", limit=None, depth=None)
    assert json.loads(route.calls.last.request.content) == {"query": "q", "detail": "compact"}
    assert "authorization" not in route.calls.last.request.headers


@pytest.mark.parametrize("kind", ["sync", "async"])
def test_list_follows_cursors(api: respx.MockRouter, kind: str) -> None:
    second = {**MEMORY, "id": "0193a1f2-7c3e-7b1a-9f00-2b6f3c1d9e12"}
    api.get("/v1/memories", params={"cursor": "c2"}).respond(json={"items": [second], "nextCursor": None})
    first = api.get("/v1/memories").respond(json={"items": [MEMORY], "nextCursor": "c2"})
    client = make(kind)
    if kind == "sync":
        got = [m.id for m in client.memories.list(tags=["a", "b"], limit=1)]
    else:

        async def collect() -> list[str]:
            return [m.id async for m in client.memories.list(tags=["a", "b"], limit=1)]

        got = asyncio.run(collect())
    assert got == [ID, second["id"]]
    assert requests(first)[0].url.params.get_list("tag") == ["a", "b"]
    assert first.call_count == 1


@pytest.mark.parametrize("kind", ["sync", "async"])
def test_tags_follow_cursors(api: respx.MockRouter, kind: str) -> None:
    api.get("/v1/tags", params={"cursor": "n"}).respond(json={"items": [{"tag": "b", "count": 1}], "nextCursor": None})
    api.get("/v1/tags").respond(json={"items": [{"tag": "a", "count": 2}], "nextCursor": "n"})
    client = make(kind)
    if kind == "sync":
        got = [(t.tag, t.count) for t in client.tags.list(prefix="x")]
    else:

        async def collect() -> list[tuple[str, int]]:
            return [(t.tag, t.count) async for t in client.tags.list(prefix="x")]

        got = asyncio.run(collect())
    assert got == [("a", 2), ("b", 1)]


def test_time_range_needs_both_ends_and_a_time_zone() -> None:
    client = Client(base_url=BASE)
    with pytest.raises(ValueError, match="both"):
        client.memories.recall("q", time_range=(START, None))  # type: ignore[arg-type]  # pyright: ignore[reportArgumentType]
    with pytest.raises(ValueError, match="time zone"):
        client.memories.recall("q", time_range=(datetime(2026, 1, 1), END))


@pytest.mark.parametrize("kind", ["sync", "async"])
def test_export_streams_to_a_new_private_file(api: respx.MockRouter, kind: str, tmp_path: Any) -> None:
    api.get("/v1/export").respond(
        content=b'{"header":1}\n{"memory":2}\n', headers={"content-type": "application/x-ndjson"}
    )
    out = tmp_path / "x.jsonl"
    run(make(kind).export(out))
    assert out.read_bytes() == b'{"header":1}\n{"memory":2}\n'
    assert out.stat().st_mode & 0o777 == 0o600
    with pytest.raises(FileExistsError):
        run(make(kind).export(out))


def test_a_failed_export_leaves_no_file(api: respx.MockRouter, tmp_path: Any) -> None:
    from .conftest import error

    api.get("/v1/export").respond(404, json=error("not_found"))
    out = tmp_path / "x.jsonl"
    with pytest.raises(Exception, match="nope"):
        Client(base_url=BASE).export(out)
    assert not out.exists()


@pytest.mark.parametrize("kind", ["sync", "async"])
def test_import_streams_the_file_once(api: respx.MockRouter, kind: str, tmp_path: Any) -> None:
    route = api.post("/v1/import").respond(json={"imported": 2, "skipped": 0, "errors": []})
    f = tmp_path / "in.jsonl"
    f.write_bytes(b'{"a":1}\n{"b":2}\n')
    result = run(make(kind).import_(f, on_conflict="error"))
    assert isinstance(result, models.ImportResult) and result.imported == 2
    req = route.calls.last.request
    assert req.headers["content-type"] == "application/x-ndjson"
    assert req.url.params["onConflict"] == "error"
    assert "idempotency-key" not in req.headers
    assert req.read() == f.read_bytes()


@pytest.mark.parametrize("kind", ["sync", "async"])
def test_an_import_that_cannot_connect_sent_nothing(api: respx.MockRouter, kind: str, tmp_path: Any) -> None:
    from knoggin import KnogginConnectionError, WriteOutcomeUnknown

    route = api.post("/v1/import").mock(side_effect=httpx.ConnectError("refused"))
    f = tmp_path / "in.jsonl"
    f.write_bytes(b"{}\n")
    with pytest.raises(KnogginConnectionError) as caught:
        run(make(kind).import_(f))
    assert not isinstance(caught.value, WriteOutcomeUnknown)
    assert route.call_count == 1


@pytest.mark.parametrize("kind", ["sync", "async"])
def test_an_import_without_an_answer_is_not_retried(api: respx.MockRouter, kind: str, tmp_path: Any) -> None:
    from knoggin import WriteOutcomeUnknown

    route = api.post("/v1/import").mock(side_effect=httpx.ReadTimeout("slow"))
    f = tmp_path / "in.jsonl"
    f.write_bytes(b"{}\n")
    with pytest.raises(WriteOutcomeUnknown):
        run(make(kind).import_(f))
    assert route.call_count == 1
