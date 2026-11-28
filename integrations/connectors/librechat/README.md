<!-- Checked against librechat.ai/docs/configuration/librechat_yaml/object_structure/mcp_servers and librechat.ai/docs/features/mcp on 2026-11-27. -->

# LibreChat

Merge `librechat.yaml` into your `librechat.yaml`, with your server's address in place of `memory.example.com`. Put the API key in the variable `KNOGGIN_API_KEY` of LibreChat's environment. Restart LibreChat. Keep `type: streamable-http`: without it LibreChat uses SSE for an `https` address, which Knoggin does not speak. The variable works only in `librechat.yaml`, not in a server added in LibreChat's own settings.

**Saving.** This agent runs tools without asking you first, so it saves whenever it decides to. If you do not want that, give it a read-only key, so that it can recall but never save, change or forget: on a local install `knoggin keys create --read-only --name <agent>`, on a server a read-only key from the web UI's API keys page.
