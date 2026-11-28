<!-- Checked against home-assistant.io/integrations/mcp on 2026-11-27. -->

# Home Assistant

Use the **Model Context Protocol Client** integration. There is no file to edit. It signs in with OAuth only, so Knoggin needs sign-in on ([sign-in guide](../../../docs/sign-in.md)); it has no field for an API key.

1. Open **Settings**, **Devices & services**, and add the integration.
2. Give the address `https://memory.example.com/mcp`, with your server's host. Home Assistant tries streamable HTTP first, which is what Knoggin speaks.
3. When Home Assistant sees that the server needs sign-in, it asks for an OAuth client ID and secret. Knoggin issues none: make an OAuth client for Home Assistant in your identity provider and give its ID and secret. Home Assistant then opens your identity provider's sign-in page. When you are done, you are back in Home Assistant. Knoggin shows no page of its own.
4. In **Settings**, **Devices & services**, open the settings (the cogwheel) of your conversation agent and pick Knoggin under its AI tools.

**Saving.** A conversation agent runs tools without asking you first, so it saves whenever it decides to. Home Assistant has no place for a read-only key: it signs in as you and gets your full access.
