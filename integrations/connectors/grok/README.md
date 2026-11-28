<!-- Checked against docs.x.ai/build/features/mcp-servers and docs.x.ai/grok/connectors on 2026-11-27. -->

# Grok

**Grok Build (the `grok` command).** Run one command, or merge a snippet into `~/.grok/config.toml`.

Local install (`local.toml`):

```sh
grok mcp add knoggin -- knoggin mcp
```

Server (`server.toml`), with your server's address in place of `memory.example.com`:

```sh
grok mcp add --transport http knoggin https://memory.example.com/mcp
```

Grok opens your identity provider's sign-in page when it first connects; when you are done, you are back in Grok. Knoggin shows no page of its own. To use an API key instead, use the commented `headers` line in `server.toml`, and keep the key in `KNOGGIN_API_KEY`.

**The Grok app.** Add a connector: **New Connector**, then **Custom**, and give the address `https://memory.example.com/mcp`. The app connects from xAI's servers, so Knoggin needs an `https` address the internet can reach, with sign-in on ([sign-in guide](../../../docs/sign-in.md)).

**Saving.** This agent runs tools without asking you first, so it saves whenever it decides to. If you do not want that, give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
