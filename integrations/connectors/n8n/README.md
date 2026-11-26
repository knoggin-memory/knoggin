<!-- UNVERIFIED 2026-11-26: n8n's documentation was not read (the build contacts no outside service). n8n is set up in its editor, so there is no snippet; the node and field names are as known on that date. -->

# n8n

Use the **MCP Client Tool** node in an AI Agent workflow. There is no file to edit.

1. Add the node to the agent's tools.
2. Set the endpoint to `https://memory.example.com/mcp`, with your server's host, and the transport to HTTP Streamable.
3. Choose bearer authentication, and make a credential that holds the key from `KNOGGIN_API_KEY`.
4. Under the tools to include, pick only the reading tools: `get_context`, `recall_memories`, `get_memory`, `find_similar_memories`, `list_memories`, `list_namespaces`, `list_tags` and `namespace_stats`.

**Read-only key.** This agent can run tools without asking you first. Give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
