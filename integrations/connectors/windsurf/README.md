<!-- UNVERIFIED 2026-11-27: the `mcpServers` entries were checked against docs.devin.ai/desktop/cascade/mcp and docs.devin.ai/cli/extensibility/mcp/configuration, but the file's place was not: Windsurf is now Devin Desktop, and its docs name both `~/.config/devin/mcp_config.json` (that MCP page) and `~/.codeium/mcp_config.json` (docs.devin.ai/desktop/devin-desktop-faq). -->

# Windsurf (Devin Desktop)

Windsurf is now called Devin Desktop. Open its MCP config file from the app (its docs name `~/.config/devin/mcp_config.json`), merge one snippet into it, then refresh the MCP servers.

- **Local install:** `local.json`. Windsurf starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.json`, with your server's address in place of `memory.example.com`. The bridge `knoggin mcp --url` sends the key in `KNOGGIN_API_KEY` and refuses an address that is not `https`, so the key never travels in the clear. Set the variable in the environment Windsurf starts in.

**Saving.** This agent runs tools without asking you first, so it saves whenever it decides to. If you do not want that, give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
