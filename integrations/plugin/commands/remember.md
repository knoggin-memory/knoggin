---
description: Save a memory to Knoggin
argument-hint: [what to remember]
---

Save one memory to the user's long-term memory in Knoggin.

- If text follows the command, base the memory on it: $ARGUMENTS
- If nothing follows the command, save what is most worth keeping from the conversation so far.

Write a short summary that stands on its own. Put detail in the full text. Add entities and a few tags. Never include passwords, keys, tokens or other secrets.

Save it with Knoggin's `store_memory` tool. Do not ask first. Then show the user the summary you saved and the memory's ID. If there is no `store_memory` tool, say that this connection is read-only and nothing was saved.
