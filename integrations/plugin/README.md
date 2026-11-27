<!--
UNVERIFIED 2026-11-26. The vendors' current plugin documentation could not be read from
the build machine (the build contacts no outside service). The files follow the formats
of the plugins installed on that machine: Claude Code 2.1.332 (its official marketplace)
and Codex CLI 0.172.0 (its curated and bundled plugins). `claude plugin validate` passes
on this folder and on the marketplace file. Check both formats against the vendors' docs
before submitting to a directory.
-->

# Knoggin plugin

Long-term memory for Claude Code and Codex, from your own Knoggin. The plugin connects the agent to Knoggin's plugin tool set, where recall is read-only. Its memory skill makes the agent show you the exact text of a memory and wait for your yes before it saves.

## What is in it

| File | For | What it does |
| --- | --- | --- |
| `.claude-plugin/plugin.json` | Claude Code | The manifest |
| `.codex-plugin/plugin.json` | Codex | The manifest. It names `mcp.json` and `skills/` |
| `.mcp.json` | Claude Code | The MCP server: `knoggin mcp --tool-set plugin` |
| `mcp.json` | Codex | The same server, in Codex's form |
| `skills/knoggin-memory/SKILL.md` | Both | When to recall, how to propose a save, how to change a memory |
| `commands/remember.md`, `commands/recall.md` | Claude Code | `/remember <text>` proposes a memory and waits for a yes. `/recall <topic>` shows what Knoggin finds |
| `hooks/hooks.json` | Claude Code | At session start, prints one fixed line: "Knoggin memory is connected. Use it when earlier context matters, and ask before saving." It reads nothing, sends nothing and calls no tool |
| `PRIVACY.md` | Both | What happens to your memories, and when a forgotten one can come back |

The Codex build has no commands and no hook.

## Install

You need Knoggin itself first: `uvx knoggin init`, or the installer from the release. The command `knoggin` must be on your `PATH` (the installer links `~/.local/bin/knoggin`).

**Claude Code:**

```sh
claude plugin marketplace add knoggin-memory/knoggin
claude plugin install knoggin@knoggin
```

**Codex:**

```sh
codex plugin marketplace add knoggin-memory/knoggin
codex plugin add knoggin@knoggin
```

## Use a server instead of the local install

The plugin ships with the local entry. It starts `knoggin mcp --tool-set plugin`, which talks to the daemon over its Unix socket. No address and no secret is in the file.

To use a Knoggin server, change the entry to the server's plugin address, `<address>/mcp/plugin`. The agent then signs in through the server's identity provider by itself.

- **Claude Code:** in the installed plugin's `.mcp.json`, replace the `knoggin` entry with `{ "type": "http", "url": "https://memory.example.com/mcp/plugin" }`.
- **Codex:** in the installed plugin's `mcp.json`, replace it with `{ "type": "http", "url": "https://memory.example.com/mcp/plugin" }`.

Use your server's address in place of `memory.example.com`. Neither format lets an entry switch between a command and an address, so the plugin cannot ask for the address at install.

## More

- [Connecting agents](../../docs/connect-agents.md)
- [Sign-in](../../docs/sign-in.md)
