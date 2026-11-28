# Frozen snapshots, content format 1

**These files are frozen. Never regenerate, edit or replace them.** They prove that every
build still opens what content format 1 wrote. If a change makes the test fail, the change
broke the format: fix the change, not the files. The only reason to write new fixtures is a
new content format number, and those go in a new directory (`snapshots/v2/`) beside this one,
which stays as it is for as long as format 1 can be read.

| File | Store ID | Name | What it holds |
| --- | --- | --- | --- |
| `empty.kgs` | `018bcfe5-6800-7000-8000-000000000001` | `0000000001-0000000001` | A new store: the `default` namespace only |
| `small.kgs` | `018bcfe5-6800-7000-8000-000000000002` | `0000000001-0000000002` | Two memories, one with a vector, one pending, and their temporal link |
| `full.kgs` | `018bcfe5-6800-7000-8000-000000000003` | `0000000002-0000000007` | Every field in use: two namespaces (one with a custom policy, a deleted one leaving a gap in the IDs), all three phases, every edge type, vectors, pending, failed and ghost embedding states, labels, full texts, access history of every kind, a reinforcement, a permanent memory, a key-saved memory without a client, two idempotency entries (one carrying memory text), and a forget that removed a memory (the forget counter is 1) |

Each is container format 1, sealed with the test key set
(`knoggin_crypto::testing::TEST_MASTER_KEY`, `kek_id` `local:dd369de2a0cd2406`), with a
data key made when they were written, wrapped for the file's store and generation, and
`created_ms` 1700086400000. The content was written
by knoggin-core 0.1.0 (`written_by` in the meta section).

- **The test that opens them:** `crates/knoggin-crypto/tests/snapshot_fixtures.rs` (`empty_v1`,
  `small_v1`, `full_v1`) runs on every build. It checks the name and the format, decrypts,
  decodes, compares the store with values written out in the test, and requires that encoding
  the decoded store gives the same content bytes.
- **The helper that wrote them:** the ignored test `write_v1_fixtures` in the same file. It
  refuses to overwrite a file. Do not run it for format 1 again.
