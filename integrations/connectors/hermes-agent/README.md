<!-- UNVERIFIED 2026-11-26: Hermes Agent's documentation was not read (the build contacts no outside service). The snippets follow its `mcp_servers` setting and skills folder as known on that date. -->

# Hermes Agent

Merge one snippet into `~/.hermes/config.yaml`.

- **Local install:** `local.yaml`. Hermes starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.yaml`, with your server's address in place of `memory.example.com`. Hermes starts the bridge `knoggin mcp --url`, which sends the API key in `KNOGGIN_API_KEY`. Set that variable in the environment Hermes starts in. The bridge refuses an address that is not `https`.

If your Hermes runs tools without asking, give it a read-only key (`knoggin keys create --read-only --name hermes`, or a read-only key from the server's web UI).

**The skill.** Copy `skills/knoggin-memory` to `~/.hermes/skills/`.
