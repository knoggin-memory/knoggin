# SPDX-License-Identifier: Apache-2.0
"""The OpenAI Agents SDK adapter, without a server."""

from __future__ import annotations

import pytest

from knoggin import ConfigurationError

from ...unit.conftest import BASE, KEY


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
