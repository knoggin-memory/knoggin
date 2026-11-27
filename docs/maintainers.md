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
| M1 | Full LoCoMo benchmark run with the shipped model and default recall | Owner: model API key and budget | Not yet run; how to run it is below |
| M2 | Time from claim to first answer on the one-host stack at 1,500, 15,000 and 75,000 memories | Build | Not yet run |
| M3 | Peak memory of a user container at the same three sizes | Build | Not yet run |
| M4 | Snapshot size and duration at the three sizes, local disk and SeaweedFS | Build | Not yet run |
| M5 | Retrieval with the faster compute path (patched graph) | Build if `locomo10.json` is usable, otherwise owner | Done 2026-11-25: 97.0% overlap, the patched graph is the one to ship; switch waits for the owner (below) |
| M6 | On AWS: usable memory, containers per host, launch to ready, embedding time on a full host | Owner: AWS account | Not yet run |
| M7 | Bucket self-test and a long upload | Build for SeaweedFS; owner for real S3 | Not yet run |
| M8 | Linux first run with and without a desktop session | Build if a Linux machine is at hand, otherwise owner | Not yet run |
| M9 | Engine latency on a 4-core arm64 machine with the fake model | Build | Done 2026-11-25: every budget met (below) |
| M10 | macOS: keychain read after an upgrade replaced the binary | Owner: a Mac and two builds | Not yet run |
| M11 | ECS: start a task with `no-new-privileges` | Owner | Not yet run |
| M12 | Whether ONNX Runtime 1.31.1 has a library for Intel Macs | Build: a look-up | Done 2026-11-25: no. Intel Macs stay unsupported |
| M13 | One pass against a real AuthKit environment | Owner: WorkOS account | Not yet run |
| M14 | Submit the plugin to the Claude and OpenAI directories | Owner | Not yet run |
| M15 | Whether claude.ai and Claude Desktop show MCP server instructions | Owner | Not yet run |
| M16 | Embedding time on an x86_64 Linux CI runner | Build, once the organization exists (P3) | Not yet run |

### M1: how to run the benchmark

Run M5's switch first, so that M1 measures the model that ships. Then:

1. Get `locomo10.json` (recalld keeps a copy at `src/bench/data/locomo10.json`).
2. Build the harness: `cargo build --release -p knoggin-bench --locked`.
3. Fill the model folder with `knoggin model fetch`, or pass `--model-dir`. Point `ORT_DYLIB_PATH` (or `--ort-lib`) at the ONNX Runtime library.
4. Have an OpenAI-compatible server that serves Qwen3 235B and DeepSeek V3.2 from Amazon Bedrock. Put its bearer token in `BENCH_LLM_API_KEY`.
5. Run:

```sh
BENCH_JUDGE_STYLE=mem0 BENCH_LLM_API_KEY=... target/release/knoggin-bench locomo \
  --data locomo10.json --llm-url https://your-server \
  --model <qwen id> --ingest-model <qwen id> --judge-model <deepseek id> \
  --out-dir m1-run
```

The defaults match the spec: 25 memories per recall, full text for 5, all five question categories. The report gives accuracy, its 95% interval and mean tokens per recall; `bench_results.json` and `bench_debug.log` land in `--out-dir`.

If accuracy is below 82.3%, run once more with `--full-text-count 10` and publish both runs with their settings. The default stays 5. Open an issue in recalld that describes any gap.

### M5: the faster compute path

Measured on 2026-11-25 on the build machine (Apple Silicon, macOS) with `locomo10.json` from recalld's benchmark commit: all 5,882 turns and 1,986 questions embedded with the published graph and with the patched one. Mean top-10 overlap 0.9701 (minimum 0.80); mean cosine between the two graphs' vectors 0.99987. `scripts/patch_q4_graph.py` (onnx 1.23.2) set `accuracy_level = 4` on all 146 `MatMulNBits` nodes; the patched `model_q4.onnx` has SHA-256 `1afd951bf447f631786cc1302921f94d4bc30f602ee6a61e8e517d0a3203c8a8`. Both passes ran at background priority, so the speed ratio seen (1.19 times) is not a measurement.

The rule says the patched graph is the one to ship. The build keeps the published graph pinned, because only a release of the organization can host the patched file. The owner switches before the first public release (a later switch re-embeds every store):

1. Pin the new `graph_sha256` in `knoggin-embed`'s model module and take the new identity string that follows from it.
2. Update the identity test to the new string.
3. Make the graph file's first download address, and with it the image's model stage, the asset `model_q4.onnx` of the project's release tagged `model-<new identity string>`, because Hugging Face keeps serving the published graph.
4. Run `model-release.yml` with the patched graph and a notice that says it was modified, before the release that names the new identity.

### M9: engine latency

On GitHub's `ubuntu-24.04-arm` runner (4-core arm64 Linux), with `FakeProvider::with_dim(768)`, after the link-selection fix (CI run 45733605239). 95th percentiles in ms:

| Store size | Save | apply_embedding | Recall | Get | 8 recalls at once (no mark) |
| --- | --- | --- | --- | --- | --- |
| 15,000 | 0.92 (budget 20) | 3.39 (20) | 57.87 (100) | 0.05 (2) | 256.98 |
| 75,000 | 4.38 | 23.05 | 152.59 (400) | 0.09 | 577.67 |

The stores are built by the budget test itself: one namespace, summaries of 20 to 45 words, full text on 30%, about 20 edges per memory, all memories `Full`.

### M12: ONNX Runtime for Intel Macs

No. The 1.31.1 NuGet package's `runtimes/` folder holds `android`, `ios`, `linux-arm64`, `linux-x64`, `osx-arm64`, `win-arm64` and `win-x64`, and PyPI's 1.31.1 macOS wheels are all `macosx_14_0_arm64`. (GitHub's release page for ONNX Runtime returned 404 on 2026-11-25, which is why the NuGet package is the source; see `docs/spec-deviations.md`.) `x86_64-apple-darwin` stays out of the release matrix.
