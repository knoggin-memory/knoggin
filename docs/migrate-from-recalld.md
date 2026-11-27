# Migrate from recalld

`knoggin import recalld` copies the memories of a recalld 2.2 data directory into Knoggin. It never changes the recalld directory, so you can keep recalld until you are happy.

## Before you start

1. **Upgrade recalld to 2.2** and start it once, so its data is in the current layout. Older layouts are refused with "Upgrade recalld to 2.2 and start it once, then stop it and try again".
2. **Stop recalld:** `recalld daemon stop`. The import refuses a directory that recalld still holds: "Stop recalld first: recalld daemon stop".
3. **Install Knoggin** and run `knoggin start` ([Getting started](getting-started.md)).

## Import

```sh
knoggin import recalld ~/.recalld/data
```

Use the path of your recalld data directory; `~/.recalld/data` is an example. For a recalld container volume, give the volume's folder; the importer finds the store inside it. To put the memories into an app instead of your personal store, add `--app <name>`.

When it is done it prints what it imported:

```text
Namespaces: 3
Memories: 8 full, 3 summary, 1 ghost
Forgotten memories skipped: 1
Edges: 10
Edges skipped: 19
Waiting for embedding: 11
Errors: 0
The daemon embeds them in the background; `knoggin reembed` shows progress.
```

`--json` prints the same counts as one object. The command exits with code 1 if any line was an error; the rest is still imported.

## What carries over

- Every namespace with its decay policy.
- Every memory with its ID, summary, full text, tags, entities, topics and emotions, its phase (full, summary or ghost), its decay state and its access history.
- Links between memories. The ones recalld made automatically from vectors, and kinds of link Knoggin does not keep, are left out and counted under "Edges skipped"; Knoggin makes its own automatic links again.

What does not:

- **Vectors.** recalld's came from another model. Knoggin embeds each memory again in the background, which takes about 0.8 seconds per memory on one core. Until then a memory is found by its words, not yet by its meaning. `knoggin reembed` waits and shows progress.
- **Forgotten memories.** They are skipped and counted.
- **Who saved each memory.** Every imported memory is recorded as saved by `import`.
- recalld's global settings, such as its own decay multiplier.

Namespace names that differ only in letter case are one name in Knoggin. The second one gets the suffix `-2`, and the output lists it under "Renamed".

## Is it safe to run twice?

Yes. A memory whose ID is already there is skipped, so a second run changes nothing. If the import stopped part-way, run it again.

## One thing to know

recalld's database files cannot be opened read-only, so the importer copies the two of them into a private folder under `~/.knoggin/run/` and reads the copies. They hold memory text in plain text while the import runs, and are deleted when it ends, also when it fails. The daemon removes any copy left behind by a crash at its next start. This is the one place Knoggin writes memory text unencrypted to disk.

## Afterwards

Point your agents at Knoggin instead of recalld ([Connecting agents](connect-agents.md)). Knoggin's tools have the same purpose but simpler arguments; the agent learns them from the tool list.
