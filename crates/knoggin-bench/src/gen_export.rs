// SPDX-License-Identifier: AGPL-3.0-only
//! `gen-export`: an export-format file of synthetic memories for the measurements that need
//! large stores (M2, M3, M4). The distribution is fixed so that measurements compare: one
//! namespace; a summary of 80 to 300 bytes on every memory; a full text of 300 to 3,000
//! bytes on 70% of them; 3 tags from a pool of 200, 2 entities from a pool of 500 and 1
//! topic from a pool of 50; `createdAt` is the time of writing; no edges. Lengths and picks
//! are uniform, from a generator seeded with `--seed`. The text uses [`WORDS_FILE`].

use std::io::{self, Write};

use knoggin_core::id::MemoryId;
use knoggin_core::model::Phase;
use knoggin_core::time::Timestamp;
use knoggin_core::transfer::{
    EXPORT_FORMAT, EXPORT_VERSION, ExportCounts, ExportHeader, MemoryRecord, NamespaceRecord,
    TransferRecord,
};

/// The fixed list of 2,000 common English words, one per line.
const WORDS_FILE: &str = include_str!("words.txt");

/// The words of [`WORDS_FILE`]; the pools are slices of it: tags are words 0 to 199,
/// entities the capitalised words 200 to 699, topics words 700 to 749.
fn words() -> Vec<&'static str> {
    WORDS_FILE.lines().collect()
}

const TAG_POOL: std::ops::Range<usize> = 0..200;
const ENTITY_POOL: std::ops::Range<usize> = 200..700;
const TOPIC_POOL: std::ops::Range<usize> = 700..750;

/// SplitMix64: small, seeded, and the same on every platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` by a widening multiply; the bias is far below 2^-50 here.
    fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next()) * n as u128) >> 64) as usize
    }

    /// Uniform in `lo..=hi`.
    fn between(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    /// `k` distinct picks from `pool`.
    fn distinct(&mut self, pool: std::ops::Range<usize>, k: usize) -> Vec<usize> {
        let mut picks = Vec::with_capacity(k);
        while picks.len() < k {
            let i = pool.start + self.below(pool.len());
            if !picks.contains(&i) {
                picks.push(i);
            }
        }
        picks
    }
}

/// Words separated by spaces, exactly `len` bytes; it never ends in a space.
fn text(rng: &mut Rng, words: &[&str], len: usize) -> String {
    let mut out = String::with_capacity(len + 8);
    while out.len() < len {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(words[rng.below(words.len())]);
    }
    out.truncate(len);
    if out.ends_with(' ') {
        out.pop();
        out.push('s');
    }
    out
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

/// Writes the file: the header, the namespace `default`, then `count` memories by ID.
/// The same `count`, `seed` and `now` give the same bytes.
pub(crate) fn generate(
    count: usize,
    seed: u64,
    now: Timestamp,
    out: &mut impl Write,
) -> io::Result<()> {
    let words = words();
    let mut rng = Rng(seed);
    let mut line = |record: &TransferRecord| -> io::Result<()> {
        serde_json::to_writer(&mut *out, record)?;
        out.write_all(b"\n")
    };
    line(&TransferRecord::Header(ExportHeader {
        format: EXPORT_FORMAT.to_owned(),
        version: EXPORT_VERSION,
        exported_at: Some(now),
        model: None,
        counts: Some(ExportCounts {
            namespaces: 1,
            memories: count as u64,
            edges: 0,
        }),
    }))?;
    line(&TransferRecord::Namespace(NamespaceRecord {
        name: "default".to_owned(),
        created_at: Some(now),
        policy: None,
    }))?;

    // IDs first, sorted, so memories stream out in ID order without holding them all.
    let millis = u64::try_from(now.millis()).unwrap_or(0);
    let mut ids: Vec<MemoryId> = (0..count)
        .map(|_| {
            let mut random = [0_u8; 10];
            random[..8].copy_from_slice(&rng.next().to_le_bytes());
            random[8..].copy_from_slice(&rng.next().to_le_bytes()[..2]);
            MemoryId::from_uuid(
                uuid::Builder::from_unix_timestamp_millis(millis, &random).into_uuid(),
            )
        })
        .collect();
    ids.sort();
    for id in ids {
        let len = rng.between(80, 300);
        let summary = text(&mut rng, &words, len);
        let full_text = (rng.below(10) < 7).then(|| {
            let len = rng.between(300, 3_000);
            text(&mut rng, &words, len)
        });
        let tags = rng
            .distinct(TAG_POOL, 3)
            .into_iter()
            .map(|i| words[i].to_owned())
            .collect();
        let entities = rng
            .distinct(ENTITY_POOL, 2)
            .into_iter()
            .map(|i| capitalise(words[i]))
            .collect();
        let topics = rng
            .distinct(TOPIC_POOL, 1)
            .into_iter()
            .map(|i| words[i].to_owned())
            .collect();
        line(&TransferRecord::Memory(Box::new(MemoryRecord {
            id,
            namespace: "default".to_owned(),
            created_at: now,
            summary: Some(summary),
            full_text,
            tags: Some(tags),
            entities: Some(entities),
            topics: Some(topics),
            emotions: Some(Vec::new()),
            phase: Phase::Full,
            stability: None,
            difficulty: None,
            is_permastore: None,
            decay_anchor_at: None,
            retrieved_at: None,
            reinforced_at: None,
            phase_changed_at: None,
            access_count: None,
            saved_by: None,
            client: None,
            access_history: None,
        })))?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use knoggin_core::embed::EmbeddingProvider as _;
    use knoggin_core::id::StoreId;
    use knoggin_core::testing::FakeProvider;
    use knoggin_core::transfer::{ImportOutcome, OnConflict};

    const NOW: i64 = 1_764_000_000_000;

    fn file(count: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::new();
        generate(count, seed, Timestamp::from_millis(NOW), &mut out).unwrap();
        out
    }

    fn records(bytes: &[u8]) -> Vec<TransferRecord> {
        std::str::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn word_list_is_two_thousand_distinct_words() {
        let words = words();
        assert_eq!(words.len(), 2_000);
        let unique: std::collections::BTreeSet<_> = words.iter().collect();
        assert_eq!(unique.len(), 2_000);
        assert!(
            words
                .iter()
                .all(|w| w.bytes().all(|b| b.is_ascii_lowercase()))
        );
    }

    #[test]
    fn same_seed_same_bytes() {
        assert_eq!(file(300, 7), file(300, 7));
        assert_ne!(file(300, 7), file(300, 8));
    }

    #[test]
    fn distribution_is_the_fixed_one() {
        let count = 2_000;
        let recs = records(&file(count, 1));
        let words: std::collections::HashSet<&str> = words().into_iter().collect();
        let TransferRecord::Header(header) = &recs[0] else {
            panic!("first line is not the header")
        };
        assert_eq!(header.counts.as_ref().unwrap().memories, count as u64);
        assert!(matches!(&recs[1], TransferRecord::Namespace(n) if n.name == "default"));
        let memories: Vec<&MemoryRecord> = recs[2..]
            .iter()
            .map(|r| match r {
                TransferRecord::Memory(m) => &**m,
                _ => panic!("only memories after the namespace"),
            })
            .collect();
        assert_eq!(memories.len(), count);
        assert!(
            memories.windows(2).all(|w| w[0].id < w[1].id),
            "sorted by ID"
        );
        let mut with_full = 0;
        let (mut shortest, mut longest) = (usize::MAX, 0);
        for m in &memories {
            let summary = m.summary.as_deref().unwrap();
            assert!((80..=300).contains(&summary.len()), "{}", summary.len());
            shortest = shortest.min(summary.len());
            longest = longest.max(summary.len());
            assert!(!summary.ends_with(' '));
            if let Some(full) = &m.full_text {
                with_full += 1;
                assert!((300..=3_000).contains(&full.len()));
            }
            // Every whole word (all but a cut last one) is from the list.
            let ws: Vec<&str> = summary.split(' ').collect();
            assert!(ws[..ws.len() - 1].iter().all(|w| words.contains(w)));
            let tags = m.tags.as_ref().unwrap();
            assert_eq!(tags.len(), 3);
            assert!(tags[0] != tags[1] && tags[1] != tags[2] && tags[0] != tags[2]);
            assert_eq!(m.entities.as_ref().unwrap().len(), 2);
            assert_eq!(m.topics.as_ref().unwrap().len(), 1);
            assert_eq!(m.created_at.millis(), NOW);
        }
        // 70% with full text: 1,400 expected, 3 sigma is about 62.
        assert!((1_300..=1_500).contains(&with_full), "{with_full}");
        assert!(
            shortest < 90 && longest > 290,
            "lengths spread over the range"
        );
    }

    #[test]
    fn output_imports_into_a_store() {
        let count = 500;
        let recs = records(&file(count, 3));
        let model = FakeProvider::new().identity().clone();
        let now = Timestamp::from_millis(NOW + 1_000);
        let mut store = knoggin_core::store::Store::create(
            StoreId::from_uuid(uuid::Uuid::now_v7()),
            model,
            now,
        );
        let mut recs = recs.into_iter();
        let Some(TransferRecord::Header(header)) = recs.next() else {
            panic!("no header")
        };
        header.check().unwrap();
        let mut imported = 0;
        for r in recs {
            if store.import_record(r, OnConflict::Skip, now).unwrap() == ImportOutcome::Imported {
                imported += 1;
            }
        }
        // `default` exists already and is empty, so its line counts as imported too.
        assert!(imported == count || imported == count + 1, "{imported}");
        assert_eq!(store.pending_embeddings(usize::MAX).len(), count);
    }
}
