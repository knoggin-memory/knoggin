<!-- Checked against support.claude.com/en/articles/11175166-get-started-with-custom-connectors-using-remote-mcp on 2026-11-27. -->

# claude.ai

claude.ai connects to Knoggin by its address, from Anthropic's servers. So Knoggin needs an `https` address on the standard port 443 that the internet can reach, with sign-in on: a server, or a local install behind a tunnel ([sign-in guide](../../../docs/sign-in.md)).

1. Open **Customize**, then **Connectors**. Choose **+ Add**, then add a custom connector.
2. Name it `Knoggin` and give the address `https://memory.example.com/mcp`, with your own host in place of `memory.example.com`. Choose **Continue**, check the sign-in settings claude.ai found, and choose **Continue** again.
3. Under **Authentication**, choose **Sign in now**, then **Add**. claude.ai opens your identity provider's sign-in page. When you are done, you are back in claude.ai. Knoggin shows no page of its own.
4. In a chat, choose **+** at the lower left, then **Connectors**, and turn Knoggin on.

A free plan allows one custom connector.

**Optional last step.** You may add this line to your own personal preferences in claude.ai's settings:

> At the start of each conversation, use Knoggin to load relevant context.

That is your own instruction to your assistant. Knoggin does not send it.
