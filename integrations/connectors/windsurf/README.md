<!-- UNVERIFIED 2026-11-26: Windsurf's documentation was not read (the build contacts no outside service). The snippets follow its `mcp_config.json` as known on that date. -->

# Windsurf

Merge one snippet into `~/.codeium/windsurf/mcp_config.json`, then refresh the MCP servers in Windsurf.

- **Local install:** `local.json`. Windsurf starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.json`, with your server's address in place of `memory.example.com`. The bridge `knoggin mcp --url` sends the key in `KNOGGIN_API_KEY` and refuses an address that is not `https`, so the key never travels in the clear. Set the variable in the environment Windsurf starts in.

**Read-only key.** This agent can run tools without asking you first. Give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
