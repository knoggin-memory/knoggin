<!-- UNVERIFIED 2026-11-26: Zed's documentation was not read (the build contacts no outside service). The snippets follow its `context_servers` setting as known on that date. -->

# Zed

`uvx knoggin init` makes the local change for you. To do it by hand, merge one snippet into Zed's `settings.json`.

- **Local install:** `local.json`. Zed starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.json`, with your server's address in place of `memory.example.com`. Zed starts the bridge `knoggin mcp --url`, which sends the API key in `KNOGGIN_API_KEY`. Set that variable in the environment Zed starts in. The bridge refuses an address that is not `https`, so the key never travels in the clear.

Zed has no skills; the server's own instructions tell the agent how to use Knoggin.
