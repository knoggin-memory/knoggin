<!-- Checked against goose-docs.ai/docs/getting-started/using-extensions and goose-docs.ai/docs/guides/config-files on 2026-11-27. -->

# Goose

Merge one snippet into `~/.config/goose/config.yaml`, or add the same command with `goose configure`.

- **Local install:** `local.yaml`. Goose starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.yaml`, with your server's address in place of `memory.example.com`. The bridge `knoggin mcp --url` sends the key in `KNOGGIN_API_KEY` and refuses an address that is not `https`, so the key never travels in the clear. Set the variable in the environment Goose starts in.

**Saving.** This agent runs tools without asking you first, so it saves whenever it decides to. If you do not want that, give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
