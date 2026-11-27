<!-- Checked on 2026-11-27 against docs.n8n.io/integrations/builtin/cluster-nodes/sub-nodes/n8n-nodes-langchain.toolmcp, which still shows the older SSE-only fields, and against the node's source in github.com/n8n-io/n8n (McpClientTool.node.ts), which has the fields below. -->

# n8n

Use the **MCP Client Tool** node in an AI Agent workflow. There is no file to edit.

1. Add the node to the agent's tools.
2. Set **Endpoint** to `https://memory.example.com/mcp`, with your server's host, and **Server Transport** to **HTTP Streamable**.
3. Set **Authentication** to **Bearer Auth**, and make a credential that holds the key from `KNOGGIN_API_KEY`.
4. Set **Tools to Include** to **Selected**, and pick only the reading tools: `get_context`, `recall_memories`, `get_memory`, `find_similar_memories`, `list_memories`, `list_namespaces`, `list_tags` and `namespace_stats`.

**Read-only key.** This agent can run tools without asking you first. Give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
