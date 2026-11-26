<!-- UNVERIFIED 2026-11-26: Open WebUI's documentation was not read (the build contacts no outside service). It is set up in its admin settings, so there is no snippet; the menu names are as known on that date. -->

# Open WebUI

Open WebUI connects to Knoggin by address, with an API key. There is no file to edit.

1. Open **Admin Panel**, **Settings**, **External Tools**, and add a server.
2. Choose the type **MCP (Streamable HTTP)** and give the address `https://memory.example.com/mcp`, with your server's host. A local install answers at `http://127.0.0.1:7690/mcp` on the same machine.
3. Choose bearer authentication and paste the key. Keep the key in `KNOGGIN_API_KEY` where you keep secrets, and paste it from there.

**Read-only key.** This agent can run tools without asking you first. Give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
