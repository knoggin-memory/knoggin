# SPDX-License-Identifier: AGPL-3.0-only
"""Knoggin as an MCP server for the OpenAI Agents SDK (extra `knoggin[openai-agents]`).

```python
from agents import Agent
from knoggin.openai_agents import mcp_server
async with mcp_server() as knoggin:            # reads KNOGGIN_BASE_URL and KNOGGIN_API_KEY
    agent = Agent(name="assistant", mcp_servers=[knoggin])
```

Checked against openai-agents 0.25: `agents.mcp.MCPServerStreamableHttp` with `params` (url, headers,
timeout) and `require_approval` as an object of `always` and `never` tool lists. Every tool that writes
needs approval; the reading tools do not.
"""

from __future__ import annotations

from typing import Any

from agents.mcp import MCPServerStreamableHttp

from ._config import resolve
from .errors import ConfigurationError

__all__ = ["READING_TOOLS", "WRITING_TOOLS", "mcp_server"]

READING_TOOLS = [
    "get_context",
    "recall_memories",
    "get_memory",
    "find_similar_memories",
    "list_memories",
    "list_namespaces",
    "list_tags",
    "namespace_stats",
]
WRITING_TOOLS = [
    "store_memory",
    "store_memories",
    "supersede_memory",
    "reinforce_memory",
    "move_memories",
    "create_namespace",
    "update_namespace_policy",
    "forget_memory",
    "forget_memories",
    "delete_namespace",
]


def mcp_server(
    base_url: str | None = None,
    api_key: str | None = None,
    namespace: str | None = None,
    *,
    timeout: float = 30.0,
    name: str = "knoggin",
    **kwargs: Any,
) -> MCPServerStreamableHttp:
    """The Agents SDK's streamable-HTTP MCP server object for Knoggin's `/mcp` address, with approval required for writes.

    Arguments win over `KNOGGIN_BASE_URL`, `KNOGGIN_API_KEY` and `KNOGGIN_NAMESPACE`. An address is
    required: an MCP client cannot use the daemon's socket, so a local install is reached at its
    loopback address with an API key (or use the stdio command `knoggin mcp`). Other keyword arguments
    go to `MCPServerStreamableHttp`.
    """
    settings = resolve(base_url, api_key, namespace, timeout, 0)
    if settings.base_url is None:
        raise ConfigurationError("mcp_server needs the server's address: give base_url or set KNOGGIN_BASE_URL")
    headers: dict[str, str] = {}
    if settings.api_key:
        headers["Authorization"] = f"Bearer {settings.api_key}"
    if settings.namespace:
        headers["Knoggin-Namespace"] = settings.namespace
    return MCPServerStreamableHttp(
        params={"url": f"{settings.base_url}/mcp", "headers": headers, "timeout": timeout},
        name=name,
        require_approval={"always": {"tool_names": WRITING_TOOLS}, "never": {"tool_names": READING_TOOLS}},
        **kwargs,
    )
