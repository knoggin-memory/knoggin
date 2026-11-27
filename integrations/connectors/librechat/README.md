<!-- UNVERIFIED 2026-11-26: LibreChat's documentation was not read (the build contacts no outside service). The snippet follows its `mcpServers` setting as known on that date. -->

# LibreChat

Merge `librechat.yaml` into your `librechat.yaml`, with your server's address in place of `memory.example.com`. Put the API key in the variable `KNOGGIN_API_KEY` of LibreChat's environment (its `.env` file). Restart LibreChat.

**Read-only key.** This agent can run tools without asking you first. Give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
