# SPDX-License-Identifier: AGPL-3.0-only
"""Every SDK call against a live Knoggin, over the socket and over an address with an API key."""

from __future__ import annotations

import asyncio
import json
import time
import uuid
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from pydantic import BaseModel

from knoggin import (
    AsyncClient,
    Client,
    ConflictError,
    InvalidRequestError,
    NewMemory,
    NotFoundError,
    models,
)

from .conftest import Live


def wait_embedded(client: Client, memory_id: str) -> None:
    deadline = time.monotonic() + 30
    while client.memories.get(memory_id, record_access=False).embedding != "ready":
        assert time.monotonic() < deadline, "the embedding did not land"
        time.sleep(0.1)


def test_store_get_list_recall_context_similar(client: Client) -> None:
    a = client.memories.store(
        "Maya prefers window seats",
        full_text="Said so while booking the Lisbon trip.",
        tags=["travel"],
        entities=["Maya"],
        topics=["seating"],
        emotions=["calm"],
    )
    b = client.memories.store(
        "Maya books trains to Porto", tags=["travel", "rail"], parent_id=a.id, check_duplicates=True
    )
    assert isinstance(a, models.StoredMemory) and a.phase == "full"
    wait_embedded(client, a.id)
    wait_embedded(client, b.id)

    got = client.memories.get(a.id, include_history=True)
    assert (got.summary, got.full_text, got.tags, got.entities) == (
        "Maya prefers window seats",
        "Said so while booking the Lisbon trip.",
        ["travel"],
        ["Maya"],
    )
    assert got.access_history is not None

    listed = list(client.memories.list(limit=1))  # one per page: the cursor is followed
    assert [m.id for m in listed] == [b.id, a.id]
    assert [m.id for m in client.memories.list(tags=["rail"])] == [b.id]
    assert all(m.full_text is None for m in listed)
    assert next(iter(client.memories.list(include_full_text=True, phase="full"))).id == b.id
    now = datetime.now(UTC)
    assert (
        len(list(client.memories.list(created_after=now - timedelta(hours=1), created_before=now + timedelta(hours=1))))
        == 2
    )

    compact = client.memories.recall("window seat", tags=["travel"], record_access=False)
    assert compact.memories and isinstance(compact.memories[0], models.CompactHit)
    assert a.id in {h.id for h in compact.memories}
    full = client.memories.recall(
        "window seat",
        detail="full",
        limit=5,
        full_text_count=1,
        entities=["Maya"],
        min_strength=0.1,
        depth=1,
        time_range=(now - timedelta(days=1), now + timedelta(days=1)),
    )
    assert full.memories and all(isinstance(h, models.FullHit) for h in full.memories)

    ctx = client.context(topic="Maya", limit=5)
    assert isinstance(ctx, models.Context) and ctx.model.state == "ready"
    assert {m.id for m in client.context().memories} == {a.id, b.id}

    similar = client.memories.similar(a.id, limit=5)
    assert similar and similar[0].memory.id == b.id and similar[0].memory.full_text is None


def test_store_many_supersede_reinforce_forget(client: Client) -> None:
    batch = client.memories.store_many(
        [NewMemory(summary="one", tags=["n"]), NewMemory(summary="two"), NewMemory(summary="x" * 3000)]
    )
    assert (batch.stored, batch.errors) == (2, 1)
    assert batch.results[2].status == "error" and batch.results[2].error is not None
    one, two = (r.memory.id for r in batch.results[:2] if r.memory)

    newer = client.memories.supersede(one, "one, corrected", tags=["n"])
    assert newer.supersedes == one
    assert one not in {h.id for h in client.memories.recall("one", record_access=False).memories}

    r = client.memories.reinforce(two, quality=4)
    assert isinstance(r, models.Reinforced) and r.id == two

    dry = client.memories.forget_many([two, str(uuid.uuid4())], dry_run=True)
    assert [i.deleted for i in dry.results] == [True, False]
    assert client.memories.get(two).id == two
    assert client.memories.forget(two).deleted
    assert not client.memories.forget(two).deleted
    assert client.memories.forget_many([newer.id]).deleted == 1
    with pytest.raises(NotFoundError):
        client.memories.get(two)


def test_move_between_namespaces(client: Client) -> None:
    m = client.memories.store("to be moved")
    target = f"ct-{uuid.uuid4().hex[:12]}"
    client.namespaces.create(target)
    try:
        moved = client.memories.move([m.id], target, source_namespace=None)
        assert (moved.moved, moved.destination_namespace) == (1, target)
        assert client.memories.get(m.id).namespace == target
    finally:
        client.namespaces.delete(target, force=True)


def test_namespaces(client: Client) -> None:
    name = f"ct-{uuid.uuid4().hex[:12]}"
    created = client.namespaces.create(name, initial_stability=5.0, decay_rate_multiplier=2.0)
    assert created.name == name and created.policy.initial_stability == 5.0
    with pytest.raises(ConflictError) as caught:
        client.namespaces.create(name)
    assert caught.value.code == "already_exists"
    assert name in {n.name for n in client.namespaces.list()}

    client.memories.store("in the namespace", namespace=name, tags=["ns"])
    stats = client.namespaces.stats(name)
    assert (stats.memory_count, stats.phase_counts.full) == (1, 1)

    policy = created.policy.model_copy(update={"decay_rate_multiplier": None, "initial_stability": 4.0})
    dry = client.namespaces.update_policy(name, policy, dry_run=True)
    assert dry.would_change is not None
    assert client.namespaces.stats(name).policy.initial_stability == 5.0  # a dry run changes nothing
    assert client.namespaces.update_policy(name, policy).policy.decay_rate_multiplier is None

    with pytest.raises(ConflictError) as caught:
        client.namespaces.delete(name)
    assert caught.value.code == "namespace_not_empty"
    preview = client.namespaces.delete(name, force=True, dry_run=True)
    assert (preview.deleted, preview.memories_deleted) == (False, 1)
    assert client.namespaces.delete(name, force=True).deleted
    with pytest.raises(NotFoundError):
        client.namespaces.stats(name)
    with pytest.raises(InvalidRequestError):
        client.namespaces.delete("default")


def test_tags(client: Client) -> None:
    client.memories.store("a", tags=["ct/alpha", "ct/beta"])
    client.memories.store("b", tags=["ct/alpha"])
    assert [(t.tag, t.count) for t in client.tags.list(prefix="ct/")] == [("ct/alpha", 2), ("ct/beta", 1)]


class Trip(BaseModel):
    city: str
    days: int


def test_typed_memories(client: Client) -> None:
    client.memories.store_model(Trip(city="Lisbon", days=3))
    client.memories.store("Trip to nowhere", tags=["type/trip"], full_text="not json")
    assert client.memories.recall_models("Lisbon trip", Trip) == [Trip(city="Lisbon", days=3)]


def test_export_and_import(client: Client, tmp_path: Path) -> None:
    client.memories.store("exported memory", tags=["exp"])
    out = tmp_path / "store.jsonl"
    client.export(out)
    assert out.stat().st_mode & 0o777 == 0o600
    lines = out.read_text().splitlines()
    assert lines and "exported memory" in out.read_text()
    json.loads(lines[0])
    # The same file again: the memory and its namespace are there, so they are skipped. (An empty namespace such as
    # `default` takes the file's policy and counts as imported.)
    time.sleep(2)  # an import is never retried, so let the key's rate bucket (20 a second) fill up first
    again = client.import_(out)
    assert again.errors == [] and again.skipped >= 2
    assert [m.summary for m in client.memories.list()] == ["exported memory"]


def test_errors_carry_the_api_fields(client: Client) -> None:
    with pytest.raises(InvalidRequestError) as caught:
        client.memories.store("x" * 2001)
    e = caught.value
    assert (e.code, e.field, e.status) == ("invalid_argument", "summary", 400)
    assert e.request_id
    with pytest.raises(NotFoundError):
        client.memories.get(str(uuid.uuid4()))


def test_async_client(live: Live, via: str, monkeypatch: pytest.MonkeyPatch) -> None:
    ns = f"ct-{uuid.uuid4().hex[:12]}"

    async def flow() -> None:
        if via == "socket":
            assert live.daemon is not None
            monkeypatch.setenv("KNOGGIN_HOME", str(live.daemon.home))
            c = AsyncClient(namespace=ns)
        else:
            c = AsyncClient(base_url=live.base_url, api_key=live.api_key, namespace=ns)
        async with c:
            await c.namespaces.create(ns)
            m = await c.memories.store("async memory", tags=["async"])
            assert (await c.memories.get(m.id)).summary == "async memory"
            assert [x.id async for x in c.memories.list()] == [m.id]
            assert [t.tag async for t in c.tags.list()] == ["async"]
            assert (await c.context()).memories[0].id == m.id
            assert (await c.namespaces.delete(ns, force=True)).deleted

    asyncio.run(flow())
