# Export files

Written by hand. No script makes them: edit the files themselves, and keep
`bad-expected.json` and the line table below in step with `bad.jsonl`. The tables cite these
import rules; "Engine" means the limits a save checks, and "format" the export file format:

- R1: the first line is the header, and only the first.
- R2: every line is one JSON object of a known type, with known fields of the right type and
  in range, times at most a day ahead, and the namespace or memories it names already there.
- R3: a memory's phase decides its fields: `full` needs a summary, `summary` has no full
  text, and a ghost has no summary, full text, tags or labels.
- R4: a namespace that exists and holds memories keeps its policy and creation time, and its
  line is skipped; an empty one takes the line's policy.
- R5: a memory whose ID is already there is `already_exists` with `onConflict=error`, and
  skipped with `onConflict=skip`.
- R6: `savedBy` and `client` do not travel: an imported memory is saved by `import`.
- R8: an automatic `associative` edge is skipped, an edge that is already there is skipped and
  keeps its weight, and both of an edge's memories must exist.
- R9: a server's store size cap and rate limits apply.

| File | What it is for |
| --- | --- |
| `good.jsonl` | A valid export in the exact form `GET /v1/export` writes: every field and line kind |
| `bad.jsonl` | A valid header, then one line for each kind of bad line, with good lines mixed in |
| `bad-expected.json` | What importing `bad.jsonl` with `onConflict=error` answers: counts, and each error line's number, code and a piece of its message |

Used by `the_good_export_fixture_imports_and_exports_back` and
`the_bad_export_fixture_reports_each_bad_line_and_applies_the_rest` in
`crates/knoggin-http/tests/store_routes.rs`, by `import_reads_the_export_fixtures` in
`crates/knoggin/tests/commands.rs`, and as seeds of the `export_line` fuzz target.

## `good.jsonl`

The header, three namespaces (`default` with the default policy, `personal` with every policy
value changed, `work` with decay off), seven memories and seven edges. The memories: one with
every field set (full text, tags, all three label kinds, lifecycle numbers, every time, an
access history of every kind, `savedBy` and `client`), its child, one in phase `summary`, one
superseded by another, a ghost, and a permanent one. The edges: one of each type, a manual
`associative` edge and an automatic one.

Imported into an empty store it gives 16 imported lines and 1 skipped (the automatic
`associative` edge, R8), and the store exports back to the same lines except for what the
format says does not travel: the header, that automatic edge, `savedBy` and `client` (R6), and
the creation time of `default`, which every store already has (R4).

## `bad.jsonl`

Imported with `onConflict=error`: 7 lines imported, 3 skipped, 66 error lines. The test adds one
more line of 4 MiB and one byte at the end (line 79); a file that size does not belong in git.

| Line | What is wrong | Rule | Code |
| --- | --- | --- | --- |
| 1 | Nothing: the header. The test also swaps it for bad ones (wrong version, wrong format, unknown field, missing version, not a header, not JSON), each of which refuses the whole file | R1 | |
| 2, 3, 4 | Good: a namespace, a memory in it, a memory with only the required fields | | |
| 5 | Blank: ignored, but numbered | | |
| 6 | Not JSON | R2 | `invalid_argument` |
| 7 | A JSON array, not an object | format | `invalid_argument` |
| 8 | No `type` | R2 | `invalid_argument` |
| 9 | Unknown line type | R2 | `invalid_argument` |
| 10 | A second header | R1 | `invalid_argument` |
| 11 | A field given twice | R2 | `invalid_argument` |
| 12 | Not UTF-8 (a Latin-1 byte) | format | `invalid_argument` |
| 13 | Namespace without `name` | R2 | `invalid_argument` |
| 14 | Namespace with an unknown field | R2 | `invalid_argument` |
| 15 | Namespace name with a space | R2, Engine | `invalid_argument` |
| 16 | Namespace name of 65 bytes | R2, Engine | `invalid_argument` |
| 17 | Policy without `permastoreThreshold` | R2 | `invalid_argument` |
| 18 | Policy thresholds out of order | R2, Engine | `invalid_argument` |
| 19 | Namespace `createdAt` more than a day ahead | R2 | `invalid_argument` |
| 20 | Good: `scratch` again; it holds a memory, so it keeps its policy and is skipped | R4 | |
| 21 | Memory with an unknown field (`parentId`, a save field) | R2 | `invalid_argument` |
| 22 to 25 | Memory without `id`, `namespace`, `createdAt`, `phase` | R2 | `invalid_argument` |
| 26 | `full` memory without a summary | R3 | `invalid_argument` |
| 27 | `summary` memory with a `null` summary | R3 | `invalid_argument` |
| 28 | Empty summary | R2, Engine | `invalid_argument` |
| 29 | Summary of 2,001 bytes | R2, Engine | `invalid_argument` |
| 30 | Summary with tool-call markup | R2, Engine | `invalid_argument` |
| 31 | Empty full text | R2, Engine | `invalid_argument` |
| 32 | `summary` memory with full text | R3 | `invalid_argument` |
| 33 | Ghost with a summary | R3 | `invalid_argument` |
| 34 | Ghost with full text | R3 | `invalid_argument` |
| 35 | Ghost with tags | R3 | `invalid_argument` |
| 36 | Ghost with labels | R3 | `invalid_argument` |
| 37 | Unknown phase | R2 | `invalid_argument` |
| 38 | ID that is not a UUID | R2 | `invalid_argument` |
| 39 | `tags` a string, not a list | R2 | `invalid_argument` |
| 40 | `isPermastore` a string | R2 | `invalid_argument` |
| 41 | Negative `accessCount` | R2 | `invalid_argument` |
| 42 | 65 tags | R2, Engine | `invalid_argument` |
| 43 | Tag of 129 bytes | R2, Engine | `invalid_argument` |
| 44 | Tag with a space | R2, Engine tag rules | `invalid_argument` |
| 45 | 33 entities | R2, Engine | `invalid_argument` |
| 46 | Entity of 122 bytes | R2, Engine | `invalid_argument` |
| 47 | Topic of 123 bytes | R2, Engine | `invalid_argument` |
| 48 | Emotion of 121 bytes | R2, Engine | `invalid_argument` |
| 49 | Empty topic | R2, Engine | `invalid_argument` |
| 50 | `createdAt` more than a day ahead | R2 | `invalid_argument` |
| 51 | `createdAt` before 1970 | R2 | `invalid_argument` |
| 52 | `createdAt` not a time | R2 | `invalid_argument` |
| 53 | `decayAnchorAt` more than a day ahead | R2 | `invalid_argument` |
| 54 | Stability below 0.01 | R2, Engine | `invalid_argument` |
| 55 | Difficulty above 10 | R2, Engine | `invalid_argument` |
| 56 | 33 access events | R2, Engine | `invalid_argument` |
| 57 | Unknown access kind | R2 | `invalid_argument` |
| 58 | Access event more than a day ahead | R2 | `invalid_argument` |
| 59 | Memory in namespace `later`, which only a later line makes | R2 | `not_found` |
| 60, 61 | Good: namespace `later`, then the same memory again | | |
| 62 | Memory whose ID line 3 already imported | R5 | `already_exists` |
| 63 | Edge with an unknown field | R2 | `invalid_argument` |
| 64 to 66 | Edge without `source`, `target`, `edgeType` | R2 | `invalid_argument` |
| 67 | Unknown edge type | R2 | `invalid_argument` |
| 68 | Edge to a memory that does not exist | R2, R8 | `not_found` |
| 69 | Edge from a memory that does not exist | R2, R8 | `not_found` |
| 70 | Edge to a memory that only line 75 makes | R2, R8 | `not_found` |
| 71 | Edge from a memory to itself | R2 | `invalid_argument` |
| 72, 73 | Edge weight above 1, below 0 | R2 | `invalid_argument` |
| 74 | Edge `createdAt` more than a day ahead | R2 | `invalid_argument` |
| 75, 76 | Good: a ghost with only some fields, then a `parent_child` edge to it | | |
| 77 | The same edge again with another weight: skipped, the first stays | R8 | |
| 78 | Automatic `associative` edge to a missing memory: skipped | R2, R8 | |
| 79 | Added by the test: longer than 4 MiB | format | `invalid_argument` |

With `onConflict=skip`, line 62 is skipped instead (R5). Two cases need a server and are not
here: `resource_exhausted` and `rate_limited` (R9).
