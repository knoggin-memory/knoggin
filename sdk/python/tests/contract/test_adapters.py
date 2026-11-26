# SPDX-License-Identifier: AGPL-3.0-only
"""The two framework adapters against a live Knoggin. No model is called: Pydantic AI's `TestModel` stands in."""

from __future__ import annotations

import asyncio
import uuid

import pytest

from knoggin import AsyncClient, Client

from .conftest import Live


def test_pydantic_ai_memory_recalls_and_waits_for_approval_to_save(live: Live, client: Client, via: str) -> None:
    from pydantic_ai import Agent
    from pydantic_ai.models.test import TestModel
    from pydantic_ai.tools import DeferredToolRequests, DeferredToolResults, ToolApproved

    from knoggin.pydantic_ai import Memory

    client.memories.store("Maya prefers window seats")
    ns = client._settings.namespace
    if via == "socket":
        assert live.daemon is not None
        with pytest.MonkeyPatch.context() as m:
            m.setenv("KNOGGIN_HOME", str(live.daemon.home))
            async_client = AsyncClient(namespace=ns)
    else:
        async_client = AsyncClient(base_url=live.base_url, api_key=live.api_key, namespace=ns)
    agent = Agent(TestModel(), capabilities=[Memory(client=async_client)], output_type=[str, DeferredToolRequests])
    first = agent.run_sync("Which seat does Maya like?")
    assert isinstance(first.output, DeferredToolRequests)
    (call,) = first.output.approvals
    assert call.tool_name == "remember"
    recalled = [
        p
        for msg in first.all_messages()
        for p in getattr(msg, "parts", [])
        if getattr(p, "tool_name", "") == "recall_memories"
    ]
    assert any("window seats" in str(getattr(p, "content", "")) for p in recalled)
    assert len(list(client.memories.list())) == 1  # nothing saved before the approval

    approved = DeferredToolResults(approvals={call.tool_call_id: ToolApproved()})
    agent.run_sync(message_history=first.all_messages(), deferred_tool_results=approved)
    assert len(list(client.memories.list())) == 2


def test_openai_agents_server_lists_knoggin_tools(live: Live, via: str) -> None:
    if via != "url":
        pytest.skip("an MCP client reaches Knoggin by address")
    from knoggin.openai_agents import READING_TOOLS, WRITING_TOOLS, mcp_server

    async def tools() -> set[str]:
        async with mcp_server(live.base_url, live.api_key, f"ct-{uuid.uuid4().hex[:8]}") as server:
            return {t.name for t in await server.list_tools()}

    assert asyncio.run(tools()) == set(READING_TOOLS) | set(WRITING_TOOLS)
