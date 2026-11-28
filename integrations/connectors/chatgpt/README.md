<!-- Checked against help.openai.com/en/articles/12584461-developer-mode-and-mcp-apps-in-chatgpt on 2026-11-27. -->

# ChatGPT

ChatGPT connects to Knoggin by its address, from OpenAI's servers. So Knoggin needs an `https` address that the internet can reach, with sign-in on: a server, or a local install behind a tunnel ([sign-in guide](../../../docs/sign-in.md)).

Custom MCP servers work on the web, on Business and Enterprise/Edu plans. On Pro, a custom server gets read and fetch permissions only. You do not need to turn on developer mode.

1. Open **Plugins**. Choose the plus button, then **Add custom MCP server**.
2. Name it `Knoggin`, give the address `https://memory.example.com/mcp` with your own host, and choose OAuth as the authentication.
3. Tick the box that you understand the risk, and choose **Create as a plugin**. When ChatGPT asks you to sign in, it opens your identity provider's page. When you are done, you are back in ChatGPT. Knoggin shows no page of its own.
4. Install the plugin. In a chat, type `@` and choose Knoggin.

ChatGPT asks before it runs a tool that changes data. Knoggin's tools say which ones only read. Nothing is saved until you allow the call.
