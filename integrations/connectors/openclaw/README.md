<!-- UNVERIFIED 2026-11-26: OpenClaw's documentation was not read (the build contacts no outside service), and its MCP config format was not known well enough to write a snippet. Only the steps and the skill ship. The skill folder layout follows OpenClaw skills seen on the build machine. -->

# OpenClaw

There is no snippet for OpenClaw: add Knoggin as an MCP server through OpenClaw's own MCP settings.

- **Local install:** a server that runs the command `knoggin mcp`. It talks to the daemon over its Unix socket and needs no address or key.
- **Server:** a server that runs `knoggin mcp --url https://memory.example.com/mcp`, with your server's address, and with your API key in the variable `KNOGGIN_API_KEY` of the environment OpenClaw starts it in. If your OpenClaw connects to MCP addresses itself, give it `https://memory.example.com/mcp` and the header `Authorization: Bearer` followed by the key from that variable.

OpenClaw can run tools without asking. If yours does, give it a read-only key (`knoggin keys create --read-only --name openclaw`, or a read-only key from the server's web UI).

**The skill.** Copy `skills/knoggin-memory` to your OpenClaw skills folder, for example `~/.openclaw/skills/`.
