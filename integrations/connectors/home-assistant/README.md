<!-- UNVERIFIED 2026-11-26: Home Assistant's documentation was not read (the build contacts no outside service). It is set up in its integrations page, so there is no snippet. Whether your version speaks streamable HTTP, which Knoggin needs, could not be checked. -->

# Home Assistant

Use the **Model Context Protocol** integration. There is no file to edit.

1. Open **Settings**, **Devices & services**, and add the integration.
2. Give the address `https://memory.example.com/mcp`, with your server's host. Knoggin speaks streamable HTTP only, not the older SSE transport.
3. Sign in through your identity provider when Home Assistant asks, or give it an API key if it asks for a token. Keep the key in `KNOGGIN_API_KEY` where you keep secrets.
4. Pick the integration as a tool source for your conversation agent.

**Read-only key.** This agent can run tools without asking you first. Give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
