<!-- UNVERIFIED 2026-11-26: Grok's documentation was not read (the build contacts no outside service), and its MCP setup was not known well enough to write a snippet. Only the steps ship. -->

# Grok

There is no snippet for Grok. Where your Grok client lets you add an MCP server:

- **By command,** on the machine that runs Knoggin: `knoggin mcp`. It talks to the daemon over its Unix socket and needs no address or key.
- **By address:** `https://memory.example.com/mcp`, with your server's host, signing in through your identity provider or sending `Authorization: Bearer` with the key from `KNOGGIN_API_KEY`. A client that runs in the cloud needs an address the internet can reach ([sign-in guide](../../../docs/sign-in.md)).

**Read-only key.** This agent can run tools without asking you first. Give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
