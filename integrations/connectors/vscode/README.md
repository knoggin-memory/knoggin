<!-- Checked against code.visualstudio.com/docs/agents/reference/mcp-configuration and code.visualstudio.com/docs/copilot/customization/agent-skills on 2026-11-27. -->

# VS Code

`uvx knoggin init` makes this change for you. To do it by hand:

**Local install:**

```sh
code --add-mcp '{"name":"knoggin","type":"stdio","command":"knoggin","args":["mcp"]}'
```

**Server:**

```sh
code --add-mcp '{"name":"knoggin","type":"http","url":"https://memory.example.com/mcp"}'
```

Use your server's address in place of `memory.example.com`. VS Code asks you to sign in when it first starts the server, and opens your identity provider's page; when you are done, you are back in VS Code. Knoggin shows no page of its own.

The files `local.json` and `server.json` hold the same entries for a workspace's `.vscode/mcp.json` or your user `mcp.json`. VS Code now calls this format deprecated and plans to remove it in February 2027; `code --add-mcp` still writes it.

**The skill.** Copy `skills/knoggin-memory` to `~/.copilot/skills/`, or to `.github/skills/` in a workspace.
