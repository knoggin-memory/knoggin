# Connecting agents

An agent reaches Knoggin over MCP in one of two ways:

- **Local install:** the agent runs the command `knoggin mcp`. It talks to the daemon over its Unix socket, starts the daemon if needed, and needs no address and no key.
- **Server:** the agent connects to `https://<your server>/mcp` and signs in through your identity provider by itself ([Sign-in](sign-in.md)). Agents that can only run a command use `knoggin mcp --url https://<your server>/mcp`, with an API key in `KNOGGIN_API_KEY`.

`uvx knoggin init` connects Claude Code, Codex, Cursor, VS Code, Zed and opencode for you. `uvx knoggin init --server https://<your server>` connects them to a server instead. Each agent's folder under `integrations/connectors/` holds its config snippet, with the placeholder `memory.example.com` for your server's host.

## Three things that apply to every agent

**Two tool sets.** `/mcp` (and `knoggin mcp`) offers all 18 tools, and records each recall so that memories you use fade more slowly. `/mcp/plugin` (and `knoggin mcp --tool-set plugin`) offers 10 tools, and its recall tools change nothing; the plugins use it.

**An app's store.** To give an agent its own store, make an app and a key for it ([Apps and API keys](apps-and-keys.md)), and set `KNOGGIN_API_KEY` to that key in the agent's environment. The key selects the app's store.

**A project's namespace.** A file `.knoggin.toml` with the line `namespace = "<name>"`, in a project folder or above it, makes `knoggin mcp` save and recall in that namespace for agents started there.

**When to save.** Knoggin's instructions tell the agent to recall when earlier context matters, and to save when you ask it to, then say what it saved. Nothing tells an agent to save on its own. The memory skill and the plugins go further: the agent shows you the exact text and waits for your yes before every save.

## Claude Code

**Plugin (recommended).** It brings the memory skill, `/remember` and `/recall`, and the read-only plugin tool set:

```sh
claude plugin marketplace add knoggin-memory/knoggin
claude plugin install knoggin@knoggin
```

It connects to the local install. To point it at a server, see the [plugin's README](../integrations/plugin/README.md).

**Or the MCP server alone:**

```sh
claude mcp add --scope user knoggin -- knoggin mcp                                        # local
claude mcp add --scope user --transport http knoggin https://memory.example.com/mcp        # server
```

For a server, run `/mcp` in Claude Code and sign in. Copy the skill from [`integrations/connectors/claude-code`](../integrations/connectors/claude-code/README.md) to `~/.claude/skills/` if you want it without the plugin.

## Codex

**Plugin (recommended).** It brings the memory skill and the read-only plugin tool set:

```sh
codex plugin marketplace add knoggin-memory/knoggin
codex plugin add knoggin@knoggin
```

**Or the MCP server alone:**

```sh
codex mcp add knoggin -- knoggin mcp                                     # local
codex mcp add knoggin --url https://memory.example.com/mcp               # server
codex mcp login knoggin
```

Snippets and the skill: [`integrations/connectors/codex`](../integrations/connectors/codex/README.md).

## Cursor

Merge a snippet into `~/.cursor/mcp.json`: the command `knoggin mcp` for a local install, or the address `https://<your server>/mcp`, where Cursor signs you in. See [`integrations/connectors/cursor`](../integrations/connectors/cursor/README.md).

## VS Code

```sh
code --add-mcp '{"name":"knoggin","type":"stdio","command":"knoggin","args":["mcp"]}'
```

For a server, use `"type":"http","url":"https://<your server>/mcp"` instead; VS Code signs you in. See [`integrations/connectors/vscode`](../integrations/connectors/vscode/README.md).

## Zed

Add `knoggin` under `context_servers` in Zed's `settings.json`, with the command `knoggin mcp`, or for a server the address `https://<your server>/mcp`, where Zed signs you in. See [`integrations/connectors/zed`](../integrations/connectors/zed/README.md).

## opencode

Add `knoggin` under `mcp.servers` in `~/.config/opencode/opencode.json`: a `local` entry with the command `["knoggin", "mcp"]`, or a `remote` entry with `https://<your server>/mcp`. See [`integrations/connectors/opencode`](../integrations/connectors/opencode/README.md).

## OpenClaw

Add `knoggin` under `mcp.servers` in OpenClaw's config, or with `openclaw mcp set`, and copy the memory skill to `~/.openclaw/skills/`. See [`integrations/connectors/openclaw`](../integrations/connectors/openclaw/README.md).

## Hermes Agent

Add `knoggin` under `mcp_servers` in `~/.hermes/config.yaml`, and copy the memory skill to `~/.hermes/skills/`. See [`integrations/connectors/hermes-agent`](../integrations/connectors/hermes-agent/README.md).

## claude.ai and ChatGPT

Both connect from the vendor's servers, so Knoggin needs an `https` address that the internet can reach, with sign-in on: a server, or a local install behind a tunnel ([Sign-in](sign-in.md)). Add a custom connector with the address `https://<your server>/mcp` and sign in. Steps: [claude.ai](../integrations/connectors/claude-ai/README.md), [ChatGPT](../integrations/connectors/chatgpt/README.md).

Whether claude.ai and Claude Desktop show Knoggin's server instructions to the model is not yet known. If you want each conversation to start with your context, add this line to your own personal preferences in claude.ai: "At the start of each conversation, use Knoggin to load relevant context."

## Other agents

Snippets and steps for Open WebUI, LibreChat, n8n, Antigravity CLI, Home Assistant, the GitHub Copilot coding agent, Goose, Cline, Kiro, Windsurf and Grok are in their folders under [`integrations/connectors/`](../integrations/connectors). Several of these run tools without asking you first. Give those a **read-only key**, so they can recall but never save, change or forget.

## Your own code

Use the [Python SDK](python-sdk.md). It has adapters for Pydantic AI and the OpenAI Agents SDK.
