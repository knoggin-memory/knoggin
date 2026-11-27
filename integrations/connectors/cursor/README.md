<!-- Checked against cursor.com/docs/context/mcp and cursor.com/docs/skills on 2026-11-27. -->

# Cursor

`uvx knoggin init` makes this change for you. To do it by hand, merge one snippet into `~/.cursor/mcp.json`, or into `.cursor/mcp.json` in a project.

- **Local install:** `local.json`. Cursor starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.json`, with your server's address in place of `memory.example.com`. Cursor offers to sign in when it first connects, and opens your identity provider's page; when you are done, you are back in Cursor. Knoggin shows no page of its own.

**An app's store.** Set `KNOGGIN_API_KEY` to the app's key in the environment Cursor runs in, and use the local entry.

**The skill.** Copy `skills/knoggin-memory` to `~/.cursor/skills/` or `~/.agents/skills/`.
