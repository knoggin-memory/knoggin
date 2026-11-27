<!-- Checked against docs.openclaw.ai/tools/mcp, docs.openclaw.ai/cli/mcp/transports and docs.openclaw.ai/tools/skills on 2026-11-27. -->

# OpenClaw

Merge one snippet into OpenClaw's config, under `mcp.servers`, or set the same entry with `openclaw mcp set`.

**Local install:** `local.json`. OpenClaw starts `knoggin mcp`, which talks to the daemon over its Unix socket and needs no address or key.

```sh
openclaw mcp set knoggin '{"command":"knoggin","args":["mcp"]}'
```

**Server:** `server.json`, with your server's address in place of `memory.example.com`. Keep `"transport": "streamable-http"`: without it OpenClaw uses the older SSE transport, which Knoggin does not speak. Then sign in; OpenClaw opens your identity provider's page, and when you are done you are back in OpenClaw. Knoggin shows no page of its own.

```sh
openclaw mcp set knoggin '{"url":"https://memory.example.com/mcp","transport":"streamable-http","auth":"oauth"}'
openclaw mcp login knoggin
```

To use an API key instead of signing in, replace `"auth": "oauth"` with `"headers": {"Authorization": "Bearer ${KNOGGIN_API_KEY}"}` and set `KNOGGIN_API_KEY` in the environment OpenClaw runs in.

OpenClaw can run tools without asking. If yours does, give it a read-only key (`knoggin keys create --read-only --name openclaw`, or a read-only key from the server's web UI).

**The skill.** Copy `skills/knoggin-memory` to `~/.openclaw/skills/`.
