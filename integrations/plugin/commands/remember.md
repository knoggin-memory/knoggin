---
description: Propose a memory for Knoggin and save it only after you say yes
argument-hint: [what to remember]
---

Propose one memory for the user's long-term memory in Knoggin.

- If text follows the command, base the memory on it: $ARGUMENTS
- If nothing follows the command, propose what is worth keeping from the conversation so far.

Write a short summary that stands on its own. Put detail in the full text. Add entities and a few tags. Never include passwords, keys, tokens or other secrets.

Show the user the exact summary, full text, entities and tags, and ask whether to save them. Do not save anything until the user says yes. If the user asks for changes, show the new text and ask again.

After a yes, save it with Knoggin's `store_memory` tool and tell the user the memory's ID.
