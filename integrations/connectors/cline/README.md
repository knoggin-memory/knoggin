<!-- Checked against docs.cline.bot/mcp/configuring-mcp-servers on 2026-11-27. -->

# Cline

Open Cline's MCP servers settings and merge one snippet into `cline_mcp_settings.json`.

- **Local install:** `local.json`. Cline starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.json`, with your server's address in place of `memory.example.com`. The bridge `knoggin mcp --url` sends the key in `KNOGGIN_API_KEY` and refuses an address that is not `https`, so the key never travels in the clear. Set the variable in the environment your editor starts in.

`autoApprove` lists the tools this agent runs without asking you. Leave it empty to be asked before each call. Add `store_memory` to let it save without asking.

**Saving.** This agent runs tools without asking you first, so it saves whenever it decides to. If you do not want that, give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
