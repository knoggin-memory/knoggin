# Getting started

Knoggin gives your AI agents a long-term memory on your own machine. This guide installs it, starts it, connects an agent and opens the web UI. It takes a few minutes.

To run Knoggin for several people, see [Deploy on one host](deploy-one-host.md) or [Deploy on AWS](deploy-aws.md) instead.

## Install

With Python's `uv`:

```sh
uvx knoggin init
```

This installs the `knoggin` program and connects the agents it finds: Claude Code, Codex, Cursor, VS Code, Zed and opencode. It shows every change first and asks before it makes it. Add `--dry-run` to only see the plan, or `--yes` to accept every change. `knoggin init --uninstall` takes the changes back; it never removes memories.

Without Python, use the installer from the latest release:

```sh
curl -fsSL https://github.com/knoggin-memory/knoggin/releases/latest/download/install.sh | sh
```

Both put the program in `~/.knoggin/` and link `~/.local/bin/knoggin`. Both check the SHA-256 of the download, and check its build attestation when `gh` or `cosign` is installed. Knoggin runs on Linux (x86_64 and arm64) and on Macs with Apple silicon.

## First run

```sh
knoggin start
```

The first start does three things:

- It makes a **master key**, which encrypts everything Knoggin stores. On macOS it goes in your login keychain; on Linux with a desktop it goes in the Secret Service; elsewhere it is a key file. **Back it up now:** run `knoggin stop`, then `knoggin key export --out <file>`, then `knoggin start` again, and keep that file somewhere safe. Without the key your memories cannot be read. See [Local install](local-install.md).
- It downloads the embedding model, about 207 MB, once. Until it is ready, memories are saved and are found again by their words; finding them by meaning starts when the model has loaded.
- It starts the daemon in the background. `knoggin status` shows what it is doing, and `knoggin stop` stops it.

Agents that use `knoggin mcp` start the daemon themselves when it is not running.

## Connect an agent

If `knoggin init` found your agent, it is already connected. Otherwise add Knoggin by hand. For Claude Code:

```sh
claude mcp add --scope user knoggin -- knoggin mcp
```

Every other agent works the same way: it runs the command `knoggin mcp`. [Connecting agents](connect-agents.md) has a section for each agent, and how to install the Knoggin plugin for Claude Code and Codex.

Then ask your agent to remember something, and later ask about it in a new session.

## The web UI

```sh
knoggin open
```

This opens Knoggin in your browser with a one-time code. No password is needed, because only your own account on this machine can run the command. The UI shows:

- **Memory map:** your memories and the links between them.
- **Explore** and **Search:** find, read, reinforce and delete memories.
- **Timeline:** what your agents saved and recalled, as it happens.
- **Health:** memories that are fading, and how big the store is.
- **Apps** and **API keys:** separate stores for agents, and their keys. See [Apps and API keys](apps-and-keys.md).
- **Settings:** export your data, and delete it.

## Next

- [Local install](local-install.md): where the files are, backups, export and recovery.
- [Python SDK](python-sdk.md): use Knoggin from your own code.
- [Security](security.md): what Knoggin protects, and what it does not.
- Moving from recalld? See [Migrate from recalld](migrate-from-recalld.md).
