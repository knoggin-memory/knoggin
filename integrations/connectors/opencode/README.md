<!-- UNVERIFIED 2026-11-26: opencode's documentation was not read (the build contacts no outside service). The snippets follow its `opencode.json` format as known on that date; the skills folder is the least certain part. -->

# opencode

`uvx knoggin init` makes this change for you. To do it by hand, merge one snippet into `~/.config/opencode/opencode.json`.

- **Local install:** `local.json`. opencode starts `knoggin mcp`, which talks to the daemon over its Unix socket.
- **Server:** `server.json`, with your server's address in place of `memory.example.com`. opencode signs in when it first connects (or run `opencode mcp auth knoggin`) and opens your identity provider's page; when you are done, you are back in opencode. Knoggin shows no page of its own.

**The skill.** Copy `skills/knoggin-memory` to `~/.config/opencode/skill/`, if your opencode version reads Agent Skills.
