# SPDX-License-Identifier: Apache-2.0
"""The Pydantic AI adapter against a mocked server."""

from __future__ import annotations

import json

import pytest
import respx

from knoggin import AsyncClient, ConfigurationError

from ...unit.conftest import BASE, COMPACT, KEY, STORED


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
