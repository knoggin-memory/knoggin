<!-- Checked against docs.github.com/en/copilot/how-tos/copilot-on-github/customize-copilot/configure-mcp-servers and docs.github.com/en/copilot/how-tos/copilot-on-github/customize-copilot/customize-cloud-agent/configure-secrets-and-variables on 2026-11-27. -->

# GitHub Copilot cloud agent

The cloud agent (formerly the coding agent) runs in GitHub's cloud, so Knoggin needs an `https` address that GitHub can reach: a server. It does not sign in with OAuth, so it uses an API key.

1. In the repository's **Settings**, under **Code, planning, and automation**, open **Copilot**, then **MCP servers**. Paste `mcp.json` into the MCP configuration, with your server's address in place of `memory.example.com`, and choose **Save MCP configuration**.
2. Under **Security**, open **Secrets and variables**, then **Agents**, and add the secret `COPILOT_MCP_KNOGGIN_API_KEY` with the key. GitHub passes only secrets whose names start with `COPILOT_MCP_`, so this is the one place the variable is not called `KNOGGIN_API_KEY`.

The snippet lists only the reading tools.

**Saving.** This agent runs tools without asking you first, so it saves whenever it decides to. If you do not want that, give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
