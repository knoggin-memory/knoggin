# Maintainers

What the maintainers have to decide, set or run by hand: the defaults to confirm, the repository settings, the repository name, the release steps and the measurements.

## Defaults to confirm

Knoggin ships with each of these defaults. Each one waits for the maintainers to confirm or change it.

1. **Parking with embedding work.** A container that still has embedding work waits up to 30 minutes before it parks, instead of 10 idle minutes.
2. **Bounds on apps and keys.** At most 1,000 apps per user and 1,000 key records per store, to protect the shared parts of a server.
3. **The container's memory is a ceiling.** No quota is set, but a user's open stores must fit in the container's memory, and writes are refused once they reach the container's budget. `containers.memory_mib` raises it.
4. **The size of one memory.** 2,000 bytes for a summary and 64 KiB for a full text saved through the API; an import may carry a full text up to its line limit of 4 MiB.
5. **Any outside identity provider.** Sign-in works with any provider outside Knoggin that follows the standards, including one the deployer runs.
6. **Sign-in off with users.** A server that already has users refuses to start with sign-in off, because every caller would then be an admin over them.
7. **The local state database.** The local install keeps the same records as the server's state database, in one encrypted file.
8. **Key prefix.** API keys are stored only as hashes, plus the first 8 characters of the key's random part beside the hash, so a person can tell keys apart.
9. **Snapshot timing and upload budget.** Under continuous writing a snapshot can wait up to 30 seconds; a container asked to park that has not saved after 2 hours is stopped; an hourly upload budget can pause a store's writes.
10. **`get_context` size.** The start-of-session call returns 10 memories by default, 3 of them with full text.
11. **Where access is not recorded.** Four places never record access: the plugin address `/mcp/plugin`, `get_context`, a credential without write permission, and the web UI's own reads. An agent connected through a plugin therefore records nothing when it recalls.
12. **Six interfaces.** Knoggin has six: object storage, the state database, the key provider, the container launcher, the embedding provider and the store backend. Sign-in has no provider interface, because one OpenID Connect client covers every provider.

## Repository settings

These are GitHub settings, not files. All of them were set on 2026-11-27, after the repository went public. Check them again if the repository moves.

- Require two-factor sign-in for the organization.
- Turn on immutable releases, private vulnerability reporting and Dependabot alerts.
- Make the default workflow token read-only.
- Require actions to be pinned to a full commit SHA.
- Require approval for workflows from outside contributors.
- Protect `main`: pull requests only, the one required check `ci-ok`, linear history, squash merges, no force pushes.
- Protect tags `v*`: the ruleset `protect-release-tags` blocks moving and deleting them, with no bypass; the ruleset `release-tags-created-by-the-release-app` lets only the release App and the organization's admins create them.
- Environments `release`, `release-please` and `pypi`, each deploying from `main` only, with a maintainer as required reviewer on `pypi`. The release App's two secrets, `RELEASE_APP_CLIENT_ID` and `RELEASE_APP_PRIVATE_KEY`, live in `release-please`, not in the repository's secrets.
- Secret scanning, push protection and validity checks on.
- Actions limited to GitHub's own actions plus `googleapis/release-please-action` and `Swatinem/rust-cache`. A new third-party action must be added to that list (Settings, Actions, General) before a workflow can use it.
- CodeQL default setup on.

The CI job `deps` (dependency review) runs only on a public repository: on a private one GitHub requires Advanced Security for it, and the job is skipped.

## The repository name

The repository is `knoggin-memory/knoggin`. Every file that names it writes exactly that string, alone or inside `github.com/knoggin-memory/knoggin` or `ghcr.io/knoggin-memory/knoggin`, so one search finds every place. Rust code reads it from one place, the `repository` field of the root `Cargo.toml`.

List the files that carry it:

```sh
git grep -l 'knoggin-memory/knoggin'
```

There are 27 such files today, among them the workflows, `install.sh`, `server.json`, the SDK's metadata and the docs.

To rename it, from the repository root (works with the BSD and GNU tools alike):

```sh
git grep -lz 'knoggin-memory/knoggin' | xargs -0 perl -pi -e 's#knoggin-memory/knoggin#NEW-ORG/NEW-REPO#g'
```

## Releasing

Nothing has been released yet. The release workflow, `release.yml`, has never run: at first GitHub Actions could not start jobs (billing), and a release needs the accounts below.

Before the first release:

1. Register the PyPI project `knoggin` with a trusted publisher: this repository, workflow `release.yml`, environment `pypi`.
2. Done on 2026-11-27: the switch to the patched graph (see "The faster compute path" below). `model-release.yml` published the release `model-embeddinggemma-2-q4-768-1d894cac0c7bb07b`, and the patched graph is pinned.
3. Done on 2026-11-27: the App is `knoggin-release` (bot `knoggin-release[bot]`, user ID 362108219), with both secrets set and the sign-off below pointed at it. To set it up again: create the release App, which release-please uses to open its pull requests (the workflow's own token may not open them, and its pull requests would start no CI):
   1. In the organization's settings, Developer settings, GitHub Apps, create an App named for example `knoggin-release`. Turn the webhook off. Repository permissions: Contents read and write, Pull requests read and write (Metadata read comes with them). Nothing else.
   2. Install it on `knoggin-memory/knoggin` only.
   3. Generate a private key, and copy the App's Client ID.
   4. Add two secrets to the environment `release-please` (deploying from `main` only), not to the repository: `RELEASE_APP_CLIENT_ID` (the Client ID) and `RELEASE_APP_PRIVATE_KEY` (the whole `.pem` file).
   5. Point release-please's sign-off at the App's bot account, because the `dco` check wants each commit signed off by its author, and the App authors release-please's commits. With the App named `knoggin-release`, `gh api 'users/knoggin-release%5Bbot%5D' --jq .id` gives the bot's ID; set `signoff` in `release-please-config.json` to `knoggin-release[bot] <ID+knoggin-release[bot]@users.noreply.github.com>` and merge that change.
   6. Re-run the latest Release workflow on `main`. release-please then opens the release pull request, and CI runs on it like on any other.

Each release:

1. Merge changes to `main` with Conventional Commit titles. release-please keeps one release pull request open that bumps every version file and writes the changelog. The same run refreshes `Cargo.lock` and `sdk/python/uv.lock` on that branch.
2. Merge the release pull request once `ci-ok` passes. release-please then makes the tag and a draft release.
3. The workflow builds the archives, runs the real-model tests, builds the image for both platforms, pushes it by digest, scans it with Grype in a job that cannot sign, attests and signs it, and only then tags `X.Y.Z`.
4. `assets` writes the SBOMs, the stamped `install.sh` and `SHA256SUMS` in a job that cannot sign or write. `publish` (environment `release`, no approval) checks each file against the hashes the build jobs handed over, attests every file, uploads them with the archives, adds the image digest to the notes and publishes the draft. `image-tags` then moves `X.Y` and `latest`, only forward and never for a pre-release.
5. Approve the `pypi` environment. The SDK goes to PyPI by trusted publishing.

To try the release on this machine without publishing anything, run `scripts/release-dry-run.sh` (it needs Node 26 with pnpm, cargo-auditable, cargo-about and syft). It builds the archive, serves it on a loopback port and installs it into a temporary home.

## Measurements

The numbers that set Knoggin's defaults, and the checks that still need an account or a device. "One machine" means anyone can run it locally.

| Measure | What | Needs | Status |
| --- | --- | --- | --- |
| LoCoMo benchmark | Full LoCoMo benchmark run with the shipped model and default recall | Model API key and budget | Not yet run; how to run it is below |
| Claim-to-first-answer time | Time from claim to first answer on the one-host stack at 1,500, 15,000 and 75,000 memories | One machine | Done 2026-11-26 at 1,500 and 15,000: median 0.67 s and 2.07 s, under the 3-second mark. 75,000 not yet run (below) |
| Peak memory of a user container | Peak memory of a user container at the same three sizes | One machine | Done 2026-11-26 at 1,500 and 15,000. The resulting changes are applied: `memory_mib` 1,536, a model reserve of 928 MiB, and an upload budget of about 171 GiB an hour (below). 75,000 not yet run |
| Snapshot size and duration | Snapshot size and duration at the three sizes, local disk and SeaweedFS | One machine | Done 2026-11-26 at 1,500 and 15,000 (below). 75,000 not yet run |
| The faster compute path | Retrieval with the patched graph | One machine and `locomo10.json` | Done 2026-11-25: 97.0% overlap, the patched graph is the one to ship. Switched on 2026-11-27: the patched graph ships (below) |
| AWS sizing | On AWS: usable memory, containers per host, launch to ready, embedding time on a full host | AWS account | Not yet run |
| Bucket self-test and long upload | Bucket self-test and a long upload | One machine for SeaweedFS; an AWS account for real S3 | SeaweedFS done 2026-11-26: self-test passes, a 15-second upload on a 5-second link completes, so `storage.link_ttl_seconds` stays 300. Real S3 not yet run |
| Linux first run | Linux first run with and without a desktop session | A Linux machine | Not yet run |
| Engine latency | Engine latency on a 4-core arm64 machine with the fake model | One machine | Done 2026-11-25: every budget met (below) |
| macOS keychain after an upgrade | macOS: keychain read after an upgrade replaced the binary | A Mac and two builds | Not yet run |
| ECS and no-new-privileges | ECS: start a task with `no-new-privileges` | AWS account | Not yet run |
| Isolation tests on AWS | `crates/knoggin-server/tests/stack_isolation.rs` against a deployed AWS stack (`KNOGGIN_STACK_URL`), including whether busybox `wget` in the probe task completes TLS with S3 | AWS account | Not yet run. Only the variable selection, the request signing (against AWS's published example) and the probe script's exit codes are tested locally |
| ONNX Runtime for Intel Macs | Whether ONNX Runtime 1.31.1 has a library for Intel Macs | A look-up | Done 2026-11-25: no. Intel Macs stay unsupported |
| AuthKit check | One pass against a real AuthKit environment | WorkOS account | Not yet run |
| Directory submission | Submit the plugin to the Claude and OpenAI directories | Accounts with both directories | Not yet run. The plugin needs an icon first: `claude plugin validate --profile directory --strict` fails only on `ICON_MISSING`, which wants a square PNG of 512 to 2048 px, and the repository holds the logos as SVG only |
| Server instructions in claude.ai | Whether claude.ai and Claude Desktop show MCP server instructions | A claude.ai account | Not yet run |
| Embedding time on x86_64 Linux | Embedding time on an x86_64 Linux CI runner | A GitHub-hosted x86_64 runner | Not yet run |

### LoCoMo benchmark: how to run it

The patched graph ships (see "The faster compute path"), so the benchmark measures the model that ships. Then:

1. Get `locomo10.json` (recalld keeps a copy at `src/bench/data/locomo10.json`).
2. Build the harness: `cargo build --release -p knoggin-bench --locked`.
3. Fill the model folder with `knoggin model fetch`, or pass `--model-dir`. Point `ORT_DYLIB_PATH` (or `--ort-lib`) at the ONNX Runtime library.
4. Have an OpenAI-compatible server that serves Qwen3 235B and DeepSeek V3.2 from Amazon Bedrock. Put its bearer token in `BENCH_LLM_API_KEY`.
5. Run:

```sh
BENCH_JUDGE_STYLE=mem0 BENCH_LLM_API_KEY=... target/release/knoggin-bench locomo \
  --data locomo10.json --llm-url https://your-server \
  --model <qwen id> --ingest-model <qwen id> --judge-model <deepseek id> \
  --out-dir locomo-run
```

The defaults: 25 memories per recall, full text for 5, all five question categories. The report gives accuracy, its 95% interval and mean tokens per recall; `bench_results.json` and `bench_debug.log` land in `--out-dir`.

If accuracy is below 82.3%, run once more with `--full-text-count 10` and publish both runs with their settings. The default stays 5. Open an issue in recalld that describes any gap.

### Claim-to-first-answer time, peak memory and snapshots: how they were run

On 2026-11-26 on an Apple M5 Max (OrbStack 29.8.2 with an 18-CPU Linux VM, kernel 7.2.9), with the harness `crates/knoggin-server/tests/measure.rs` and the image `knoggin:measure`, built from the working tree without `SKIP_MODEL`. Each user container has its one CPU. The stores come from `knoggin-bench gen-export --seed 1` and are filled through the import route; every number below was taken with the embedding queue empty. The stack runs without test mode and with sign-in off, because test mode gives user containers the fake model. `containers.idle_seconds` is 60 so that a container parks soon between runs, and `containers.max` is 4. The 15,000-memory store was filled with `containers.memory_mib = 2048`, because at 1,024 a container with that store can be killed for memory while it embeds, and then measured at 1,024 and 2,048.

Rerun one size at a time (`75000` with `KNOGGIN_MEASURE_MEMORY_MIB=4096`):

```sh
KNOGGIN_MEASURE_SIZE=15000 cargo test -p knoggin-server --test measure --locked -- \
  --ignored --nocapture --test-threads 2 server local_daemon
KNOGGIN_MEASURE_SIZE=1500 cargo test -p knoggin-server --test measure --locked -- \
  --ignored --nocapture --exact seaweedfs
```

`server` fills the store (once; the state stays in `target/measure/server-<size>`) and then measures peak memory, the first answer and snapshots against SeaweedFS; `local_daemon` fills a `knoggin daemon` in `target/measure/local-<size>` and measures snapshots on local disk. The doc comment of `measure.rs` lists the other variables, among them `KNOGGIN_MEASURE_FILL_ONLY=1`, which stops after the fill. A fill sends `GET /v1/context` every minute, so the container never parks while it embeds. Delete `target/measure` to start over.

**Fill rate.** 1.39 memories per second in one user container (0.72 s each), the same for the local daemon on one CPU. That projects 3.0 hours for 15,000 (the fill took about 3 hours 5 minutes) and 15 hours for 75,000. The 75,000 run was therefore not made, to keep the machine from running hot for more than about 3 hours. To make it, run the first command above with `KNOGGIN_MEASURE_SIZE=75000 KNOGGIN_MEASURE_MEMORY_MIB=4096` (about 15 hours, two CPUs busy), then repeat it once the queue is empty if the run was interrupted: the filled store is kept, and the second run measures at once.

### Claim-to-first-answer time

The container parks by itself, a pool container is ready, then `GET /v1/memories?limit=1` is timed from sending to the last byte of the answer.

| Store size | Median | Runs (s) |
| --- | --- | --- |
| 1,500 | 0.67 s | 0.69, 2.20, 0.44, 0.67, 0.43 |
| 15,000 | 2.07 s | 2.12, 2.07, 1.83, 2.19, 1.85 |
| 75,000 | not run | |

At 15,000 with `memory_mib = 2048` the runs were 2.10, 1.85, 2.07, 1.83 and 2.10 s, so the time is the load itself (decrypting, decoding, rebuilding the full-text index), not memory pressure. The median at 15,000 is under 3 seconds, so content format 2 is not needed, and the full-text index stays out of the snapshot.

### Peak memory of a user container

Read from the container's cgroup: "resident" is `memory.current` less the page cache, the highest of samples taken every 20 ms during the phase; `memory.peak` is the cgroup's own high mark since the container started, page cache included, so it carries the model load and earlier phases. The two-embeddings case saves a memory of 9,000 bytes (cut to 1,024 tokens) and 20 ms later sends a recall with a 4,096-byte query. The empty-store case runs in a fresh container whose only open store is a new, empty app store. In MiB:

| Phase | 1,500, limit 1,024: resident / `memory.peak` | 15,000, limit 1,024 | 15,000, limit 2,048 |
| --- | --- | --- | --- |
| Ready pool container (model loaded, no store) | 521 | 521 | 521 |
| Two embeddings, empty store | 896 / 952 | 896 / 1,024 | 895 / 1,060 |
| Load (claim to first answer) | 522 / 525 | 581 / 656 | 581 / 656 |
| 10 recalls | 530 / 654 | 589 / 656 | 589 / 706 |
| Snapshot | 543 / 668 | 728 / 749 | 719 / 846 |
| Two embeddings, store loaded | 914 / 1,024 | killed for memory (1,005 / 1,024 when last read) | 1,099 / 1,221 |

What follows (all of it applied; the changes are listed below):

- **Default `containers.memory_mib`: 1,536.** The peak of a 15,000-memory store is 1,099 MiB resident (1,221 MiB with page cache); with 25% to spare that needs 1,374 MiB (1,526), which 1,536 holds and 1,024 does not. At 1,024 the two-embeddings case killed the container.
- **Largest store each size holds with 25% to spare**, from the two points above (the two-embeddings peak grows by about 14 KiB per memory, from about 894 MiB at none): 768 holds none (the model's peak alone is above it); 1,024 holds none with the spare (1,500 memories ran, with 110 MiB left, and 15,000 were killed); 1,536 about 24,000 memories; 2,048 about 54,000.
- **Model reserve: 928 MiB**, the empty-store peak (897 MiB at most over four runs, resident) rounded up to the next 32 MiB, in place of 640. Counted with the page cache it would be 1,088 MiB; the cache is dropped before the kernel kills, so the resident figure is the one that keeps the model from being killed.
- **Size estimate: no change.** At 15,000 the engine's estimate is 127 MiB; the store adds 60 MiB to a ready container, and 1 MiB at 1,500, because the heap of a ready container holds about 6 MiB that the model load freed. Taken together, the store uses about 66 MiB (about 4.5 KiB per memory) at 15,000, half the estimate, so the estimate is never below the real use. The factor 0.4 also holds: a snapshot of the 15,000 store adds about 140 MiB at its peak, the second copy, which is within the 60% of the budget that the factor leaves.
- **Upload budget:** with 1,536 and 928, the store budget is (1,536 - 928) × 0.4 = 243.2 MiB, and the default `limits.upload_bytes_per_hour` becomes 720 times that, about 171 GiB (about 108 GiB before).

The changes, all made: `default_memory_mib` and its doc comments in `crates/knoggin-server/src/settings.rs` and `memory_mib: 1024` in `crates/knoggin/src/config.rs` become 1,536; `MODEL_RESERVE_BYTES` (and the `640 MiB` in its doc comment) becomes 928 MiB, together with `const MODEL` in `crates/knoggin-server/tests/container.rs`, the formula in `crates/knoggin-http/CONTRACT.md`, and the `budgets` test in `settings.rs` (153.6 MiB becomes 243.2 MiB, "About 108 GiB" becomes about 171 GiB). `deploy/docker/config.example.toml` gets `memory_mib = 1536` and "4 fits a host with 8 GiB"; `docs/deploy-one-host.md` the same; `deploy/aws/variables.tf` the default 1536, with the assertions on 1024 in `deploy/aws/tests/module.tftest.hcl`; `docs/deploy-aws.md` the default and its table (containers at 1,536 MiB: 4, 10 and 20 on the three hosts, $0.023, $0.018 and $0.018 per container-hour).

### Snapshot size and duration

Size is the newest snapshot object or file; duration is the mean over the snapshots of the window (encode, seal, upload and commit on the server; the daemon's `knoggin_snapshot_seconds` locally). The save wait is the longest of about 300 to 430 saves sent one after the other while snapshots ran, against the median of the same saves.

| Store size | Where | Size | Duration | Saves: median, longest |
| --- | --- | --- | --- | --- |
| 1,500 | SeaweedFS | 7.4 MiB | 56 ms | 1.4 ms, 7 ms |
| 1,500 | Local disk | 7.4 MiB | 14 ms | 1.0 ms, 5 ms |
| 15,000 | SeaweedFS | 74.3 MiB | 308 ms | 1.4 ms, 33 ms |
| 15,000 | Local disk | 74.3 MiB | 147 ms | 0.9 ms, 47 ms |
| 75,000 | both | not run | | |

About 5 KiB per memory, most of it the 768 floats of each vector (3 KiB). The longest save waited 47 ms at 15,000, while `encode` held the read lock. Local disk is the daemon in a Linux container with one CPU, its home bind-mounted from the Mac's disk, because there was no macOS ONNX Runtime library or model on that machine. The router's counters `knoggin_container_snapshot_*_total` (and the other `knoggin_container_*_total`) were set with `Counter::absolute`, which keeps the highest value seen: when a new container's totals were below an earlier one's, they stayed hidden until they passed it. The harness restarted the router before it read them. That is fixed: the router now adds each container's growth since its last report, so the counters keep rising when a container ends.

### The faster compute path

Measured on 2026-11-25 on an Apple Silicon Mac with `locomo10.json` from recalld's benchmark commit: all 5,882 turns and 1,986 questions embedded with the published graph and with the patched one. Mean top-10 overlap 0.9701 (minimum 0.80); mean cosine between the two graphs' vectors 0.99987. `scripts/patch_q4_graph.py` (onnx 1.23.2) set `accuracy_level = 4` on all 146 `MatMulNBits` nodes; the patched `model_q4.onnx` has SHA-256 `1afd951bf447f631786cc1302921f94d4bc30f602ee6a61e8e517d0a3203c8a8`. Both passes ran at background priority, so the speed ratio seen (1.19 times) is not a measurement.

With an overlap this high, the patched graph is the one to ship, and it ships. Only a release of the organization can host the patched file, so the switch came in two parts, before the first public release (a later switch would re-embed every store):

1. On 2026-11-27 `model-release.yml` ran from `main` (`gh workflow run model-release.yml --ref main -f graph=patched -f identity=embeddinggemma-2-q4-768-1d894cac0c7bb07b`). It patched the graph, checked every hash and published the three files and `NOTICE-model_q4.onnx.txt` under the release `model-embeddinggemma-2-q4-768-1d894cac0c7bb07b`. The published hashes match the pins, and `knoggin model fetch` completed against the release.
2. The pull request "feat(embed): ship the patched q4 graph" then pinned it:
   - `knoggin-embed`'s model module pins the patched graph (494,100 bytes, `graph_sha256` `1afd951b...`), which gives the identity string `embeddinggemma-2-q4-768-1d894cac0c7bb07b`; the identity test checks it.
   - The graph file's only download address, in the model module and in the image's model stage, is the asset `model_q4.onnx` of that release, because Hugging Face keeps serving the published graph; as a second source it would always fail and hide the release's own error. The weights and the tokenizer keep Hugging Face first and that release second.
   - `scripts/patch_q4_graph.py` fails unless its output has the pinned hash; `model-release.yml` defaults to the patched graph and accepts only its identity string for it; `third_party/model-licence.txt` says the graph was modified.

### Bucket self-test and long upload

On 2026-11-26 with `seaweedfs` in `crates/knoggin-server/tests/measure.rs` (SeaweedFS 4.54, the pin of `deploy/docker/compose.yaml`). `knoggin admin selftest` passes: state database, key provider and the five-step bucket test. An upload link signed with the router's own code for a 5-second lifetime, used for a 64 KiB upload streamed over 15 seconds, got 200: SeaweedFS checks the lifetime when a request starts and does not cut off an upload that outlasts its link. A link signed 10 seconds earlier with the same lifetime got 403 "Request has expired", so the check is on. On one host the router's blob route only streams the body and adds no deadline. No change is needed: `storage.link_ttl_seconds` stays 300. Real S3 is not yet checked.

### Engine latency

On GitHub's `ubuntu-24.04-arm` runner (4-core arm64 Linux), with `FakeProvider::with_dim(768)`, after the link-selection fix (CI run 45733605239). 95th percentiles in ms:

| Store size | Save | apply_embedding | Recall | Get | 8 recalls at once (no mark) |
| --- | --- | --- | --- | --- | --- |
| 15,000 | 0.92 (budget 20) | 3.39 (20) | 57.87 (100) | 0.05 (2) | 256.98 |
| 75,000 | 4.38 | 23.05 | 152.59 (400) | 0.09 | 577.67 |

The stores are built by the budget test itself: one namespace, summaries of 20 to 45 words, full text on 30%, about 20 edges per memory, all memories `Full`.

### ONNX Runtime for Intel Macs

No. The 1.31.1 NuGet package's `runtimes/` folder holds `android`, `ios`, `linux-arm64`, `linux-x64`, `osx-arm64`, `win-arm64` and `win-x64`, and PyPI's 1.31.1 macOS wheels are all `macosx_14_0_arm64`. (GitHub's release page for ONNX Runtime returned 404 on 2026-11-25, which is why the NuGet package is the source.) `x86_64-apple-darwin` stays out of the release matrix.
