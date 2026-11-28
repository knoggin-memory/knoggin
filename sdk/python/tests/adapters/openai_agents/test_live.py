# SPDX-License-Identifier: Apache-2.0
"""The OpenAI Agents SDK adapter against a live Knoggin."""

from __future__ import annotations

import asyncio
import uuid

import pytest

from ...contract.conftest import Live


def test_openai_agents_server_lists_knoggin_tools(live: Live, via: str) -> None:
    if via != "url":
        pytest.skip("an MCP client reaches Knoggin by address")
    from knoggin.openai_agents import READING_TOOLS, WRITING_TOOLS, mcp_server

    async def tools() -> set[str]:
        async with mcp_server(live.base_url, live.api_key, f"ct-{uuid.uuid4().hex[:8]}") as server:
            return {t.name for t in await server.list_tools()}

    assert asyncio.run(tools()) == set(READING_TOOLS) | set(WRITING_TOOLS)
