# Ranking fixture

`fixture.json` holds what recalld (reference commit
`a1b8a245117d3972c87bb403d39bf86ab6862102`) stored and returned for 309
memories in two namespaces and 40 recalls. The test
`crates/knoggin-core/tests/ranking_fixture.rs` rebuilds the same store with
`knoggin_core::testing::StoreBuilder`, replays each recall at its recorded time
and checks that Knoggin returns the same IDs in the same order.

Made by `scripts/make-ranking-fixture.sh <recalld checkout> [work dir]`, which
builds recalld with `--features bench` and runs `scripts/make-ranking-fixture.py`.
Each run gives new IDs and times, so regenerate the file only on purpose.

## Units and conventions

- Times appear twice: `...Ms` is an integer of milliseconds since the Unix
  epoch, UTC; the field without the suffix is the same instant in RFC 3339,
  UTC, with exactly three fraction digits and `Z` (`2026-11-25T22:55:12.424Z`).
  The two always agree; `...Ms` is the one to use.
- IDs are recalld's UUIDv7 strings, lower case with hyphens.
- Vectors are 64 `f32` values written as the shortest decimal that reads back
  as the same `f32`. Parse them as `f32`.
- `stability` is in days. `difficulty` is on recalld's 1 to 10 scale.
- Edge `weight` is an `f32` in 0 to 1.
- Order is meaningful in `memories` (save order), `edges` (sorted by `source`,
  `typeNumber`, `target`), `queries` (the order they were asked) and
  `expectedIds` (rank order). Arrays of labels keep the order they were saved in.

## Top level

| Field | Meaning |
| --- | --- |
| `format` | `1` |
| `source` | Provenance: `recalldCommit`, `build`, `script`, and `embedding` (`stub`, `dimensions` = 64, `documentPrefix`, `queryPrefix`: recalld's default prefixes, which recalld put in front of every text it embedded) |
| `namespaces` | The two namespaces as recalld created them: `name`, `embeddingDim`, `initialStability` (days), `defaultDifficulty`, `permastoreThreshold` (days), `desiredRetention` (not used by Knoggin). Phase thresholds and the decay rate multiplier were recalld's defaults (0.7, 0.3, 0.05; 1.0) |
| `memories` | One object per memory, below |
| `edges` | Every edge recalld held, below |
| `queries` | The 40 recalls, below |

## `memories[]`

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | string | Memory ID |
| `namespace` | string | Namespace name (`atlas` or `harbor`) |
| `createdAtMs`, `createdAt` | time | Creation time that recalld gave it at save (no `createdAt` was sent) |
| `summary` | string | |
| `fullText` | string or null | |
| `tags` | string[] | The memory's own tags. recalld's label tags (`entity/`, `topic/`, `emotion/`) are not here |
| `entities`, `topics`, `emotions` | string[] | Labels exactly as saved and as recalld returned them |
| `parentId` | string or null | The parent named at save. The `parent_child` edge is in `edges` |
| `supersedes` | string or null | The memory this one replaced at save. The `supersedes` edge is in `edges` |
| `phase` | string | `full`, `summary` or `ghost`, from recalld's memory route |
| `stability`, `difficulty` | number | From recalld's memory route |
| `isPermastore` | bool | From recalld's memory route |
| `accessCount` | integer | Length of recalld's access history (recalld's `get_memory` `accessCount`) |
| `decayAnchorAtMs`, `decayAnchorAt` | time | recalld's `lastAccessedAt`; equal to `createdAt` for every memory (the script checks it) |
| `document` | string | The exact text sent to the stub for this memory's vector: the document prefix plus recalld's `document_v1` text (summary, then a blank line and the full text if any, then a space and the tags with `entity/`, `topic/`, `emotion/` label tags) |
| `vector` | f32[64] | The stub's vector for `document` |

recalld stores the stub's vector after normalizing it again, so its copy can
differ in the last bit (at most 1e-7 here). The script compared 200 of them.

## `edges[]`

| Field | Type | Meaning |
| --- | --- | --- |
| `source`, `target` | string | Memory IDs. For `parent_child` the source is the parent; for `supersedes` the source is the newer memory |
| `type` | string | `parent_child`, `associative`, `entity`, `temporal` or `supersedes` |
| `typeNumber` | integer | recalld's number for the type: 1, 2, 5, 6, 7 |
| `weight` | f32 | |

recalld's graph route gives neither the automatic flag nor the creation time
of an edge. Set the flag from the type (`associative`, `entity`, `temporal` are
automatic) and use the source memory's `createdAtMs`. Neither affects ranking.

## `queries[]`

| Field | Type | Meaning |
| --- | --- | --- |
| `name` | string | `q01` to `q40` |
| `request` | object | The exact body sent to recalld's `POST /search`: `query`, `namespace`, `limit`, `depth`, and when used `entities` (array of strings), `timeRangeStart` and `timeRangeEnd` (integers, milliseconds since the epoch, always both), plus `recordAccess: false`. Nothing else was sent; absent fields took recalld's defaults (no tags, topics or emotions filters, no `minStrength`) |
| `atMs`, `at` | time | When the call was sent. Use it as `now` |
| `vector` | f32[64] | The stub's vector for the query prefix plus `request.query`. Proven: the script sent each request again with this vector as `embedding` and got the same IDs and scores |
| `expectedIds` | string[] | The IDs recalld returned, in rank order. May be shorter than `limit` |

recalld's response also carries a per-hit `score`, which is not the final score
it sorts by, so the fixture leaves it out.

## How the data was made

recalld ran on an empty data directory with its defaults except the link caps
under `[graph]`: `auto_link_threshold = 0.95`, `max_auto_links = 1`,
`max_entity_links = 1`, `max_temporal_links = 1`. With the defaults, every
memory saved within an hour gets 20 temporal links and up to 25 more, every
degree is above 6, fan is 0, and neither spreading activation nor the
connection bonus would do anything. These settings only choose which edges
recalld makes; ranking does not read them.

The content is meant to make each channel count:

- Words shared within a theme drive the vector and full-text channels; code
  names (`zephyr`, `marigold`, ...) appear only in full texts.
- Four hub memories, worded apart from everything else, are each the parent of
  12 memories of one theme. Queries on those themes with `depth` reach the hub
  through the graph; at a limit of 12 or less the vector channel fetches 100
  memories, fewer than either namespace holds. In 7 of the 19 queries with
  `depth` (q06, q07, q19, q23, q26, q37, q38) the result differs from the same
  query at depth 0.
- `Atlas Crew` and `Harbor Club` are on every memory except the hub children;
  q20 and q40 filter on them with limit 5, so all their hits come from the
  entity channel alone.
- Entity names include honorifics (`Dr. Sarah Martinez`, `Coach Daniel Reyes`),
  possessives (`Petrov's`, `Rosa's`), several words, and bare last names.
  Filters in q14 (`martinez`) and q25 (`LISBON`) differ in case from the stored
  label.
- Each namespace has a superseded chain of five memories (four hops). recalld
  refuses a second successor for one memory (HTTP 503), so the chains are
  linear.
- Time ranges: around all creation times, two days before up to the last save,
  entirely in the past, entirely in the future, and one minute wide (the
  one-day sigma floor).

All memories were saved within about 11 seconds, and the first query came
about 11 seconds after the first save.
