<!-- Checked against zed.dev/docs/ai/mcp and zed.dev/docs/ai/skills on 2026-11-27. -->

# Zed

`uvx knoggin init` makes this change for you. To do it by hand, merge one snippet into Zed's `settings.json`.

- **Local install:** `local.json`. Zed starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.json`, with your server's address in place of `memory.example.com`. The entry has no `Authorization` header, so Zed signs you in through your identity provider when it first connects; when you are done, you are back in Zed. Knoggin shows no page of its own.

**An API key instead of signing in.** Zed's documentation describes no way to read a variable in a header, so a key there would sit in the file as text. Run the bridge, which sends the key in `KNOGGIN_API_KEY` and refuses an address that is not `https`:

```json
{
  "context_servers": {
    "knoggin": {
      "command": "knoggin",
      "args": ["mcp", "--url", "https://memory.example.com/mcp"]
    }
  }
}
```

Set `KNOGGIN_API_KEY` in the environment Zed starts in. The same goes for an app's key with the local entry.

**The skill.** Copy `skills/knoggin-memory` to `~/.agents/skills/`. Zed's own agent reads it.
