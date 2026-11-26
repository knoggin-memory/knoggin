<!-- UNVERIFIED 2026-11-26: ChatGPT's settings were not checked (the build contacts no outside service). The menu names are as known on that date and may differ by plan. -->

# ChatGPT

ChatGPT connects to Knoggin by its address, from OpenAI's servers. So Knoggin needs an `https` address that the internet can reach, with sign-in on: a server, or a local install behind a tunnel ([sign-in guide](../../../docs/sign-in.md)).

1. Open **Settings**, then **Apps & Connectors**. Under **Advanced settings**, turn on developer mode if your plan requires it for custom connectors.
2. Choose **Create**. Name it `Knoggin`, give the MCP server address `https://memory.example.com/mcp` with your own host, and choose OAuth as the authentication.
3. Save. ChatGPT opens your identity provider's sign-in page. When you are done, you are back in ChatGPT. Knoggin shows no page of its own.
4. In a chat, turn Knoggin on from the tools menu.

ChatGPT asks before it runs a tool that changes data. Knoggin's tools say which ones only read.
