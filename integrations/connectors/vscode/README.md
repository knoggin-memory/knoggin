<!-- Checked 2026-11-26: `code --help` on the build machine lists `--add-mcp`. The `mcp.json` format and the skills folder are UNVERIFIED: the vendor's documentation was not read (the build contacts no outside service). -->

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

The files `local.json` and `server.json` hold the same entries for a workspace's `.vscode/mcp.json` or your user `mcp.json`.

**The skill.** Copy `skills/knoggin-memory` to `.github/skills/` in a workspace, if your VS Code version reads Agent Skills.
