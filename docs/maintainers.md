# Maintainers

What the owner of the repository has to decide, set or run by hand. Later milestones add the release steps and the measurement results.

## Choices waiting for the owner

The build makes each of these as the spec says. Each is the spec's own choice, not one of the owner's decisions, and waits for the owner to confirm or change it.

1. **Parking with embedding work.** A container that still has embedding work waits up to 30 minutes before it parks, instead of 10 idle minutes (tab "Server").
2. **Bounds on apps and keys.** At most 1,000 apps per user and 1,000 key records per store, to protect the shared parts of a server (tab "Sign-in and credentials").
3. **The container's memory is a ceiling.** No quota is set, but a user's open stores must fit in the container's memory, and writes are refused once they reach the container's budget. `containers.memory_mib` raises it (tab "Server", memory budget).
4. **The size of one memory.** 2,000 bytes for a summary and 64 KiB for a full text saved through the API; an import may carry a full text up to its line limit of 4 MiB (tab "Engine").
5. **"Hosted" identity provider.** Read as "outside Knoggin": any provider that follows the standards works, including one the deployer runs (tab "Sign-in and credentials").
6. **Sign-in off with users.** A server that already has users refuses to start with sign-in off, because every caller would then be an admin over them (tab "Sign-in and credentials").
7. **The local state database.** "An embedded database in the local install" is read as one encrypted file that holds the same records as the server's state database (tab "State database").
8. **Key prefix.** API keys are stored only as hashes, plus the first 8 characters of the key's random part beside the hash, so a person can tell keys apart (tab "Sign-in and credentials").
9. **Snapshot timing and upload budget.** Under continuous writing a snapshot can wait up to 30 seconds; a container asked to park that has not saved after 2 hours is stopped; an hourly upload budget can pause a store's writes (tabs "Snapshots and keys" and "Server").
10. **`get_context` size.** The start-of-session call returns 10 memories by default, 3 of them with full text (tab "MCP").
11. **Where access is not recorded.** Four places never record access: the plugin address `/mcp/plugin`, `get_context`, a credential without write permission, and the web UI's own reads. An agent connected through a plugin therefore records nothing when it recalls (tabs "MCP", "REST API", "Web UI").
12. **Six interfaces, not five.** The owner's list named five interfaces, including an identity provider. The build has six: object storage, the state database, the key provider, the container launcher, the embedding provider and the store backend. Sign-in has no provider interface, because one OpenID Connect client covers every provider (tab "Engineering rules", Interfaces).

## Repository settings

These are GitHub settings, not files, so the build cannot apply them.

- Require two-factor sign-in for the organization.
- Turn on immutable releases, private vulnerability reporting and Dependabot alerts.
- Make the default workflow token read-only.
- Require actions to be pinned to a full commit SHA.
- Require approval for workflows from outside contributors.
- Protect `main`: pull requests only, the one required check `ci-ok`, linear history, squash merges, no force pushes.
- Protect tags `v*`.
- Create the environments `release` and `pypi`, with the owner as required reviewer on `pypi`.

The CI job `deps` (dependency review) runs only while the repository is public: on a private repository GitHub requires Advanced Security for it. It is skipped, not failed, until then.

## The repository name

The repository is `knoggin-memory/knoggin`. Every file that names it writes exactly that string, alone or inside `github.com/knoggin-memory/knoggin` or `ghcr.io/knoggin-memory/knoggin`, so one search finds every place. Rust code reads it from one place, the `repository` field of the root `Cargo.toml`.

List the files that carry it:

```sh
git grep -l 'knoggin-memory/knoggin'
```

Today these are `README.md`, `SECURITY.md`, this file and, once the workspace lands, `Cargo.toml`. Later milestones add the install script, `server.json`, the SDK's metadata, the workflows and the docs.

To rename it, from the repository root (works with the BSD and GNU tools alike):

```sh
git grep -lz 'knoggin-memory/knoggin' | xargs -0 perl -pi -e 's#knoggin-memory/knoggin#NEW-ORG/NEW-REPO#g'
```

## Measurements

The spec's build-time measurements (tab "Overview", Build-time measurements), each with its rule there. "Build" means the build agent can run it on one machine; "owner" means it needs an account or a device that only the owner has.

| ID | Measure | Who | Status |
| --- | --- | --- | --- |
| M1 | Full LoCoMo benchmark run with the shipped model and default recall | Owner: model API key and budget | Not yet run |
| M2 | Time from claim to first answer on the one-host stack at 1,500, 15,000 and 75,000 memories | Build | Not yet run |
| M3 | Peak memory of a user container at the same three sizes | Build | Not yet run |
| M4 | Snapshot size and duration at the three sizes, local disk and SeaweedFS | Build | Not yet run |
| M5 | Retrieval with the faster compute path (patched graph) | Build if `locomo10.json` is usable, otherwise owner | Not yet run |
| M6 | On AWS: usable memory, containers per host, launch to ready, embedding time on a full host | Owner: AWS account | Not yet run |
| M7 | Bucket self-test and a long upload | Build for SeaweedFS; owner for real S3 | Not yet run |
| M8 | Linux first run with and without a desktop session | Build if a Linux machine is at hand, otherwise owner | Not yet run |
| M9 | Engine latency on a 4-core arm64 machine with the fake model | Build | Not yet run |
| M10 | macOS: keychain read after an upgrade replaced the binary | Owner: a Mac and two builds | Not yet run |
| M11 | ECS: start a task with `no-new-privileges` | Owner | Not yet run |
| M12 | Whether ONNX Runtime 1.31.1 has a library for Intel Macs | Build: a look-up | Not yet run |
| M13 | One pass against a real AuthKit environment | Owner: WorkOS account | Not yet run |
| M14 | Submit the plugin to the Claude and OpenAI directories | Owner | Not yet run |
| M15 | Whether claude.ai and Claude Desktop show MCP server instructions | Owner | Not yet run |
| M16 | Embedding time on an x86_64 Linux CI runner | Build, once the organization exists (P3) | Not yet run |
