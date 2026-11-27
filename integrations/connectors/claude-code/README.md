<!-- Checked against code.claude.com/docs/en/mcp and code.claude.com/docs/en/skills on 2026-11-27. -->

# Claude Code

`uvx knoggin init` makes this change for you. To do it by hand, run one command.

**Local install:**

```sh
claude mcp add --scope user knoggin -- knoggin mcp
```

**Server:**

```sh
claude mcp add --scope user --transport http knoggin https://memory.example.com/mcp
```

Use your server's address in place of `memory.example.com`. Then run `/mcp` in Claude Code and choose to sign in. Claude Code opens your identity provider's sign-in page; when you are done, you are back in Claude Code. Knoggin shows no page of its own.

The files `local.json` and `server.json` hold the same entries in the form of a project's `.mcp.json`.

**An app's store.** Set `KNOGGIN_API_KEY` to the app's key in the environment Claude Code runs in. The local entry sends it, and reaches that app instead of your personal store.

**The skill.** Copy `skills/knoggin-memory` to `~/.claude/skills/`. Or install the [Knoggin plugin](../../plugin/README.md), which brings the skill, the plugin tool set and two commands.
