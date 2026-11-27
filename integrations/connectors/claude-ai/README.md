<!-- UNVERIFIED 2026-11-26: claude.ai's settings were not checked (the build contacts no outside service). The menu names are as known on that date. -->

# claude.ai

claude.ai connects to Knoggin by its address, from Anthropic's servers. So Knoggin needs an `https` address that the internet can reach, with sign-in on: a server, or a local install behind a tunnel ([sign-in guide](../../../docs/sign-in.md)).

1. Open **Settings**, then **Connectors**, and choose **Add custom connector**.
2. Name it `Knoggin` and give the address `https://memory.example.com/mcp`, with your own host in place of `memory.example.com`.
3. Choose **Connect**. claude.ai opens your identity provider's sign-in page. When you are done, you are back in claude.ai. Knoggin shows no page of its own.
4. In a chat, turn the connector on in the tools menu.

**Optional last step.** You may add this line to your own personal preferences in claude.ai's settings:

> At the start of each conversation, use Knoggin to load relevant context.

That is your own instruction to your assistant. Knoggin does not send it.
