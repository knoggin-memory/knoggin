<!-- Checked against opencode.ai/docs/mcp-servers and opencode.ai/docs/skills on 2026-11-27. -->

# opencode

Merge one snippet into `~/.config/opencode/opencode.json`. The entry goes under `mcp.servers`, not directly under `mcp`.

- **Local install:** `local.json`. opencode starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.json`, with your server's address in place of `memory.example.com`. opencode signs in when it first connects (or run `opencode mcp auth knoggin`) and opens your identity provider's page; when you are done, you are back in opencode. Knoggin shows no page of its own.

**The skill.** Copy `skills/knoggin-memory` to `~/.config/opencode/skills/` or `~/.agents/skills/`.
