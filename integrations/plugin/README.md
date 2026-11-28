<!--
Checked on 2026-11-27 against code.claude.com/docs/en/plugins-reference, code.claude.com/docs/en/plugin-marketplaces,
developers.openai.com/codex/plugins/build and learn.chatgpt.com/docs/developer-commands. Codex now documents a portable
format (`plugin.json` and `mcp.json` at the plugin's root, with a `type` on every server); this plugin uses the
`.codex-plugin/plugin.json` manifest, which those docs keep as a compatibility fallback, in the form used by installed Codex
plugins. `claude plugin validate` passes on this folder and on the marketplace file.
-->

# Knoggin plugin

Long-term memory for Claude Code and Codex, from your own Knoggin. The plugin connects the agent to Knoggin's plugin tool set, where recall is read-only. Its memory skill tells the agent when to recall, to save directly, and to tell you what it saved.

## What is in it

| File | For | What it does |
| --- | --- | --- |
| `.claude-plugin/plugin.json` | Claude Code | The manifest |
| `.codex-plugin/plugin.json` | Codex | The manifest. It names `mcp.json` and `skills/` |
| `.mcp.json` | Claude Code | The MCP server: `knoggin mcp --tool-set plugin` |
| `mcp.json` | Codex | The same server, in Codex's form |
| `skills/knoggin-memory/SKILL.md` | Both | When to recall, when to save, how to change a memory |
| `commands/remember.md`, `commands/recall.md` | Claude Code | `/remember <text>` saves a memory and shows what it saved. `/recall <topic>` shows what Knoggin finds |
| `hooks/hooks.json` | Claude Code | At session start, prints one fixed line: "Knoggin memory is connected. Use it when earlier context matters." It reads nothing, sends nothing and calls no tool |
| `PRIVACY.md` | Both | What happens to your memories, and when a forgotten one can come back |

The Codex build has no commands and no hook.

## Saving

The agent saves without asking you. If your agent asks before it runs a tool, that is the agent's own setting: nothing reaches Knoggin until you allow the call. To stop the agent from saving, use a read-only key.

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

Use your server's address in place of `memory.example.com`. Claude Code can ask for a value when a plugin is enabled (`userConfig`), but neither format lets an entry switch between a command and an address on whether a value was given, so the plugin ships the local entry.

## More

- [Connecting agents](../../docs/connect-agents.md)
- [Sign-in](../../docs/sign-in.md)
