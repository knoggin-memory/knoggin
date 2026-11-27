<!-- Checked against docs.openwebui.com/features/extensibility/mcp on 2026-11-27. -->

# Open WebUI

Open WebUI connects to Knoggin by address, with an API key. There is no file to edit. It needs Open WebUI 0.6.31 or later.

1. As an admin, open **Settings**, then **Integrations**. Under **External Tool Servers**, choose **+ Add Connection**.
2. Set the type to **MCP (Streamable HTTP)** and give the address `https://memory.example.com/mcp`, with your server's host. A local install answers at `http://127.0.0.1:7690/mcp` on the same machine.
3. Set the authentication to **Bearer** and paste the key into the **Key** field. Keep the key in `KNOGGIN_API_KEY` where you keep secrets, and paste it from there.

**Read-only key.** This agent can run tools without asking you first. Give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
