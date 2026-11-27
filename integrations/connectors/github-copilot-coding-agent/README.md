<!-- UNVERIFIED 2026-11-26: GitHub's documentation was not read (the build contacts no outside service). The snippet follows the coding agent's MCP configuration as known on that date. -->

# GitHub Copilot coding agent

The coding agent runs in GitHub's cloud, so Knoggin needs an `https` address that GitHub can reach: a server.

1. In the repository's settings, open **Copilot**, **Coding agent**, and paste `mcp.json` into the MCP configuration, with your server's address in place of `memory.example.com`.
2. In the repository's `copilot` environment, add the secret `COPILOT_MCP_KNOGGIN_API_KEY` with the key. GitHub passes only secrets whose names start with `COPILOT_MCP_`, so this is the one place the variable is not called `KNOGGIN_API_KEY`.

The snippet lists only the reading tools.

**Read-only key.** This agent can run tools without asking you first. Give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
