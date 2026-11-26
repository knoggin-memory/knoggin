# Knoggin plugin: privacy

The plugin connects your agent to a Knoggin that you run: the local install on your own machine, or a server you or your organization deploy. Memories go only there. The plugin sends nothing to the Knoggin project or to anyone else, and it collects no usage data.

Recall through the plugin is read-only: it changes nothing, not even the record of when a memory was last read. The plugin saves a memory only after it has shown you the exact text and you have said yes.

## When a forgotten memory can come back

1. **Forgetting erases a memory from the live store at once.**
2. **A crash before the next snapshot can bring a forgotten memory back** until it is forgotten again. That window is usually a few seconds. It is about 30 seconds while memories are being written without a pause, and longer while storage cannot be reached.
3. **A forgotten memory can also come back when a container that was asked to park, and still has not saved 2 hours later, is stopped.** This happens on a server only.
4. **Backups can hold a forgotten memory until they age out:** 7 days by default on a server, which the deployer can change. The backups are encrypted, and nothing reads them unless an admin restores one.

Whoever runs the Knoggin server can read every memory on it. On a local install that is you.
