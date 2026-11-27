# SPDX-License-Identifier: AGPL-3.0-only
"""The typed-memory helpers and the two framework adapters, against a mocked server."""

from __future__ import annotations

import asyncio
import json
import logging

import pytest
import respx
from pydantic import BaseModel

from knoggin import AsyncClient, Client, ConfigurationError

from .conftest import BASE, COMPACT, KEY, STORED


class TripPlan(BaseModel):
    city: str
    days: int


def test_store_model_saves_json_with_a_type_tag(api: respx.MockRouter) -> None:
    route = api.post("/v1/memories").respond(201, json=STORED)
    Client(base_url=BASE).memories.store_model(TripPlan(city="Lisbon", days=3), tags=["travel"], namespace="w")
    sent = json.loads(route.calls.last.request.content)
    assert sent == {
        "summary": 'TripPlan {"city":"Lisbon","days":3}',
        "fullText": '{"city":"Lisbon","days":3}',
        "namespace": "w",
        "tags": ["travel", "type/tripplan"],
    }


def test_store_model_summary_is_cut_at_200_characters_of_json(api: respx.MockRouter) -> None:
    route = api.post("/v1/memories").respond(201, json=STORED)
    Client(base_url=BASE).memories.store_model(TripPlan(city="x" * 500, days=1))
    sent = json.loads(route.calls.last.request.content)
    assert sent["summary"] == "TripPlan " + sent["fullText"][:200]
    route2 = api.post("/v1/memories").respond(201, json=STORED)
    Client(base_url=BASE).memories.store_model(TripPlan(city="x", days=1), summary="my plan")
    assert json.loads(route2.calls.last.request.content)["summary"] == "my plan"


@pytest.mark.parametrize("kind", ["sync", "async"])
def test_recall_models_filters_by_type_and_skips_what_does_not_parse(
    api: respx.MockRouter, kind: str, caplog: pytest.LogCaptureFixture
) -> None:
    hits = [
        {**COMPACT, "fullText": '{"city":"Lisbon","days":3}'},
        {**COMPACT, "fullText": "not json"},
        COMPACT,
    ]
    route = api.post("/v1/recall").respond(json={"memories": hits, "count": 3})
    caplog.set_level(logging.DEBUG, logger="knoggin")
    if kind == "sync":
        got = Client(base_url=BASE).memories.recall_models("trip", TripPlan, limit=7)
    else:
        got = asyncio.run(AsyncClient(base_url=BASE).memories.recall_models("trip", TripPlan, limit=7))
    assert got == [TripPlan(city="Lisbon", days=3)]
    sent = json.loads(route.calls.last.request.content)
    assert (sent["tags"], sent["limit"], sent["fullTextCount"]) == (["type/tripplan"], 7, 7)
    assert sum("is not a valid TripPlan" in r.getMessage() for r in caplog.records) == 2


# --- Pydantic AI --------------------------------------------------------------------------------


def test_pydantic_ai_memory_gives_recall_and_an_approved_remember(api: respx.MockRouter) -> None:
    from pydantic_ai import Agent
    from pydantic_ai.models.test import TestModel
    from pydantic_ai.tools import DeferredToolRequests

    from knoggin.pydantic_ai import Memory

    recall = api.post("/v1/recall").respond(json={"memories": [COMPACT], "count": 1})
    store = api.post("/v1/memories").respond(201, json=STORED)
    memory = Memory(client=AsyncClient(base_url=BASE, api_key=KEY), namespace="w")
    tools = memory.get_toolset().tools
    assert sorted(tools) == ["recall_memories", "remember"]
    assert not tools["recall_memories"].requires_approval
    assert tools["remember"].requires_approval

    agent = Agent(TestModel(), capabilities=[memory], output_type=[str, DeferredToolRequests])
    result = agent.run_sync("What seat does Maya like?")
    assert recall.called and not store.called  # the save waits for approval
    assert isinstance(result.output, DeferredToolRequests)
    assert [c.tool_name for c in result.output.approvals] == ["remember"]
    assert json.loads(recall.calls.last.request.content)["namespace"] == "w"


def test_pydantic_ai_memory_reads_the_environment(monkeypatch: pytest.MonkeyPatch) -> None:
    from knoggin.pydantic_ai import Memory

    monkeypatch.setenv("KNOGGIN_BASE_URL", "http://memory.example.com")
    monkeypatch.setenv("KNOGGIN_API_KEY", KEY)
    with pytest.raises(ConfigurationError):  # the key rule holds for the adapter's own client
        Memory()._client()


# --- OpenAI Agents SDK --------------------------------------------------------------------------


def test_openai_agents_server_points_at_mcp_and_requires_approval_for_writes() -> None:
    from knoggin.openai_agents import READING_TOOLS, WRITING_TOOLS, mcp_server

    server = mcp_server(BASE, KEY, "w")
    assert server.params["url"] == f"{BASE}/mcp"
    assert server.params.get("headers") == {"Authorization": f"Bearer {KEY}", "Knoggin-Namespace": "w"}
    assert server.name == "knoggin"
    assert server._needs_approval_policy == {
        **dict.fromkeys(WRITING_TOOLS, True),
        **dict.fromkeys(READING_TOOLS, False),
    }
    assert len(READING_TOOLS) + len(WRITING_TOOLS) == 18
    assert not set(READING_TOOLS) & set(WRITING_TOOLS)


def test_openai_agents_server_needs_an_address_and_keeps_the_key_rule() -> None:
    from knoggin.openai_agents import mcp_server

    with pytest.raises(ConfigurationError, match="address"):
        mcp_server()
    with pytest.raises(ConfigurationError, match="plain http"):
        mcp_server("http://memory.example.com", KEY)
