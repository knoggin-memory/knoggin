<!-- Checked against hermes-agent.nousresearch.com/docs/user-guide/features/mcp, hermes-agent.nousresearch.com/docs/reference/mcp-config-reference and hermes-agent.nousresearch.com/docs/user-guide/features/skills on 2026-11-27. -->

# Hermes Agent

Merge one snippet into `~/.hermes/config.yaml`.

- **Local install:** `local.yaml`. Hermes starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.yaml`, with your server's address in place of `memory.example.com`. Hermes starts the bridge `knoggin mcp --url`, which sends the API key in `KNOGGIN_API_KEY`. Hermes passes a server only the variables its entry lists under `env`, so the snippet lists this one; put the key in `~/.hermes/.env`. The bridge refuses an address that is not `https`.

If your Hermes runs tools without asking, give it a read-only key (`knoggin keys create --read-only --name hermes`, or a read-only key from the server's web UI).

**The skill.** Copy `skills/knoggin-memory` to `~/.hermes/skills/`.
