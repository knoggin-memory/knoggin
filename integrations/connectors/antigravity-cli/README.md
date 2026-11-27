<!-- Checked against antigravity.google/docs/mcp on 2026-11-27. -->

# Antigravity CLI

Merge one snippet into `~/.gemini/config/mcp_config.json`, or into `.agents/mcp_config.json` in a workspace.

- **Local install:** `local.json`. It starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.json`, with your server's address in place of `memory.example.com`. The bridge `knoggin mcp --url` sends the key in `KNOGGIN_API_KEY` and refuses an address that is not `https`, so the key never travels in the clear. Set the variable in the environment Antigravity starts in.

**Read-only key.** This agent can run tools without asking you first. Give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
