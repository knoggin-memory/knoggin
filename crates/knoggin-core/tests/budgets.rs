// SPDX-License-Identifier: AGPL-3.0-only
//! Latency budgets (measurement M9, tab "Engine", "Latency budgets"). Ignored by
//! default; run with
//! `cargo nextest run -p knoggin-core --release --locked --run-ignored only -E 'binary(budgets)'`.
//!
//! The starting stores are built with `StoreBuilder` from generated memories, and their
//! automatic edges are computed here by the engine's rules (entity, temporal, semantic),
//! so the store is the one 15,000 or 75,000 saves and embeddings would have produced,
//! without timing those saves. One test runs both sizes in turn, so nothing else runs
//! while it measures.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use knoggin_core::embed::{document_text, query_text};
use knoggin_core::model::{Edge, EdgeType, Labels, Memory, Tag};
use knoggin_core::ops::{ApplyOutcome, Detail, GetMemory, Recall, StoreMemory, WriteContext};
use knoggin_core::search::entity::canonicalize_entity;
use knoggin_core::search::vector::dot;
use knoggin_core::testing::{FakeProvider, StoreBuilder, memory};
use knoggin_core::{MemoryId, NamespaceId, Store, Timestamp};

const DIM: u32 = 768;
const OPS: usize = 1_000;
const THREADS: usize = 8;
/// 2025-11-01T00:00:00Z.
const START_MS: i64 = 1_761_955_200_000;
const MINUTE: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
/// Memories per topic on average: memories of one topic share words.
const CLUSTER: usize = 20;

const STOP_WORDS: [&str; 30] = [
    "the", "a", "to", "and", "of", "in", "is", "for", "on", "that", "with", "it", "was", "at",
    "as", "by", "we", "he", "she", "they", "this", "be", "from", "or", "an", "are", "not", "but",
    "have", "has",
];
const SYLLABLES: [&str; 20] = [
    "ba", "ce", "di", "fo", "gu", "ha", "je", "ki", "lo", "mu", "na", "pe", "ri", "so", "tu", "va",
    "we", "xi", "yo", "zu",
];
const FIRST: [&str; 20] = [
    "Maya", "Liam", "Ana", "Omar", "Chen", "Sofia", "Ravi", "Elena", "Tom", "Ines", "Kenji",
    "Nora", "Pablo", "Zara", "Ivan", "Lucy", "Amir", "Greta", "Hugo", "Mei",
];
const LAST: [&str; 15] = [
    "Smith", "Garcia", "Okafor", "Tanaka", "Silva", "Novak", "Patel", "Berg", "Rossi", "Kim",
    "Moreau", "Haddad", "Larsen", "Costa", "Weber",
];
const PLACES: [&str; 20] = [
    "Lisbon", "Berlin", "Osaka", "Denver", "Nairobi", "Lyon", "Austin", "Porto", "Seoul", "Quito",
    "Oslo", "Perth", "Cairo", "Dublin", "Hanoi", "Lima", "Zurich", "Accra", "Riga", "Boise",
];
const TAGS: [&str; 24] = [
    "work",
    "travel",
    "family",
    "health",
    "project/alpha",
    "project/beta",
    "finance",
    "home",
    "music",
    "food",
    "meeting",
    "decision",
    "todo",
    "idea",
    "bug",
    "release",
    "school",
    "garden",
    "sport",
    "reading",
    "car",
    "pets",
    "birthday",
    "client",
];
const TOPICS: [&str; 16] = [
    "planning",
    "preferences",
    "schedule",
    "budget",
    "deployment",
    "hiring",
    "recipes",
    "trips",
    "fitness",
    "reviews",
    "contracts",
    "design",
    "research",
    "support",
    "events",
    "learning",
];
const EMOTIONS: [&str; 6] = [
    "happy",
    "anxious",
    "grateful",
    "frustrated",
    "excited",
    "calm",
];

/// splitmix64: deterministic on every platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

/// Picks rank `r` with weight `1 / (r + 1)`.
struct Zipf(Vec<f64>);

impl Zipf {
    fn new(n: usize) -> Self {
        let mut total = 0.0;
        Self(
            (0..n)
                .map(|r| {
                    total += 1.0 / (r + 1) as f64;
                    total
                })
                .collect(),
        )
    }

    fn sample(&self, rng: &mut Rng) -> usize {
        let u = (rng.next() >> 11) as f64 / (1u64 << 53) as f64 * self.0[self.0.len() - 1];
        self.0.partition_point(|&c| c <= u).min(self.0.len() - 1)
    }
}

/// The word lists, the entity pool and the topic clusters of one generated store.
struct Corpus {
    words: Vec<String>,
    word_zipf: Zipf,
    stop_zipf: Zipf,
    entities: Vec<String>,
    entity_zipf: Zipf,
    topic_words: Vec<[usize; 8]>,
}

/// One generated memory, before it becomes a `Memory` or a `StoreMemory`.
struct Draft {
    created_at: i64,
    summary: String,
    full_text: Option<String>,
    tags: Vec<String>,
    labels: Labels,
}

impl Corpus {
    fn new(n: usize, rng: &mut Rng) -> Self {
        // 4,000 pseudo-words of three syllables, in a shuffled order so that frequent
        // ranks are not alphabetical neighbours.
        let mut words: Vec<String> = Vec::with_capacity(8_000);
        for a in SYLLABLES {
            for b in SYLLABLES {
                for c in SYLLABLES {
                    words.push(format!("{a}{b}{c}"));
                }
            }
        }
        for i in (1..words.len()).rev() {
            words.swap(i, rng.below(i + 1));
        }
        words.truncate(4_000);
        let mut entities: Vec<String> = Vec::new();
        for f in FIRST {
            for l in LAST {
                entities.push(format!("{f} {l}"));
            }
        }
        entities.extend(PLACES.iter().map(|p| (*p).to_owned()));
        for i in (1..entities.len()).rev() {
            entities.swap(i, rng.below(i + 1));
        }
        let topics = (n / CLUSTER).max(1);
        let topic_words = (0..topics)
            .map(|_| std::array::from_fn(|_| rng.below(words.len())))
            .collect();
        Self {
            word_zipf: Zipf::new(words.len()),
            words,
            stop_zipf: Zipf::new(STOP_WORDS.len()),
            entity_zipf: Zipf::new(entities.len()),
            entities,
            topic_words,
        }
    }

    /// `count` words: a fifth stop words, a third from the topic, the rest general.
    fn text(&self, rng: &mut Rng, topic: usize, count: usize) -> String {
        let mut out: Vec<&str> = Vec::with_capacity(count);
        for _ in 0..count {
            let roll = rng.below(100);
            out.push(if roll < 20 {
                STOP_WORDS[self.stop_zipf.sample(rng)]
            } else if roll < 52 {
                &self.words[self.topic_words[topic][rng.below(8)]]
            } else {
                &self.words[self.word_zipf.sample(rng)]
            });
        }
        out.join(" ")
    }

    fn draft(&self, rng: &mut Rng, clock: &mut Clock) -> Draft {
        let created_at = clock.tick(rng);
        let topic = rng.below(self.topic_words.len());
        let len = rng.range(20, 45);
        let summary = self.text(rng, topic, len);
        let full_text = rng.chance(30).then(|| {
            let len = rng.range(80, 300);
            self.text(rng, topic, len)
        });
        let pick = |rng: &mut Rng, list: &[&str], max: usize| -> Vec<String> {
            let k = rng.range(0, max);
            let set: BTreeSet<&str> = (0..k).map(|_| list[rng.below(list.len())]).collect();
            set.into_iter().map(str::to_owned).collect()
        };
        let tags = pick(rng, &TAGS, 3);
        let entity_count = [0, 1, 1, 1, 2, 2, 3, 0][rng.below(8)];
        let entities: BTreeSet<String> = (0..entity_count)
            .map(|_| self.entities[self.entity_zipf.sample(rng)].clone())
            .collect();
        Draft {
            created_at,
            summary,
            full_text,
            tags,
            labels: Labels {
                entities: entities.into_iter().collect(),
                topics: pick(rng, &TOPICS, 2),
                emotions: pick(rng, &EMOTIONS, 1),
            },
        }
    }

    /// A question an agent might ask about one topic.
    fn query(&self, rng: &mut Rng) -> String {
        let topic = rng.below(self.topic_words.len());
        let t = |rng: &mut Rng| self.words[self.topic_words[topic][rng.below(8)]].as_str();
        let (a, b) = (t(rng), t(rng));
        let c = &self.words[self.word_zipf.sample(rng)];
        let who = &self.entities[self.entity_zipf.sample(rng)];
        match rng.below(4) {
            0 => format!("what did {who} say about the {a} {b}"),
            1 => format!("{a} {b} {c}"),
            2 => format!("when was the {a} and {c} decided"),
            _ => format!("notes on {a} {b} for {who}"),
        }
    }
}

/// A UUIDv7-shaped ID: the creation millisecond first, so IDs sort by time.
fn id_for(created_at: i64, index: usize) -> MemoryId {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&created_at.to_be_bytes()[2..]);
    b[6] = 0x70;
    b[8..16].copy_from_slice(&(index as u64 | 0x8000_0000_0000_0000).to_be_bytes());
    MemoryId::from_bytes(b)
}

/// Memories come in sessions: 1 to 15 saves 1 to 5 minutes apart, then 30 minutes
/// to 3 hours of quiet. Times strictly increase.
struct Clock {
    at: i64,
    left: usize,
}

impl Clock {
    fn tick(&mut self, rng: &mut Rng) -> i64 {
        if self.left == 0 {
            self.left = rng.range(1, 15);
            self.at += rng.range(30, 180) as i64 * MINUTE;
        } else {
            self.at += rng.range(1, 5) as i64 * MINUTE;
        }
        self.left -= 1;
        self.at
    }
}

fn to_memory(d: &Draft, id: MemoryId, ns: NamespaceId) -> Memory {
    let at = Timestamp::from_millis(d.created_at);
    let mut m = memory(id, ns, &d.summary, at);
    m.full_text.clone_from(&d.full_text);
    m.tags = d
        .tags
        .iter()
        .map(|t| Tag::new(t.as_str()).unwrap())
        .collect();
    m.labels = d.labels.clone();
    m
}

fn auto_edge(
    source: MemoryId,
    target: MemoryId,
    edge_type: EdgeType,
    weight: f32,
    at: i64,
) -> Edge {
    Edge {
        source,
        target,
        edge_type,
        weight,
        auto_created: true,
        created_at: Timestamp::from_millis(at),
    }
}

/// Everything a test phase needs from the build.
struct Built {
    store: Store,
    corpus: Corpus,
    rng: Rng,
    clock: Clock,
    ids: Vec<MemoryId>,
    edges: [usize; 3],
    seconds: f64,
}

/// Runs `f` on chunks of `0..n` on every core and returns the results in order.
fn par_chunks<T: Send>(
    n: usize,
    chunk: usize,
    f: impl Fn(Range<usize>) -> Vec<T> + Sync,
) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let mut parts: Vec<(usize, Vec<T>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let start = next.fetch_add(chunk, Ordering::Relaxed);
                        if start >= n {
                            return out;
                        }
                        out.push((start, f(start..(start + chunk).min(n))));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    parts.sort_by_key(|p| p.0);
    parts.into_iter().flat_map(|p| p.1).collect()
}

/// Builds a store of `n` memories in `default` with vectors and the automatic edges
/// that saving them one by one, each vector landing before the next save, would have
/// made: every earlier memory is a candidate of a later one, with the engine's rules.
fn build(n: usize, seed: u64) -> Built {
    let started = Instant::now();
    let provider = FakeProvider::with_dim(DIM);
    let dim = DIM as usize;
    let mut rng = Rng(seed);
    let corpus = Corpus::new(n, &mut rng);
    let mut clock = Clock {
        at: START_MS,
        left: 0,
    };
    let ns = NamespaceId::new(1);
    let memories: Vec<Memory> = (0..n)
        .map(|i| {
            let d = corpus.draft(&mut rng, &mut clock);
            to_memory(&d, id_for(d.created_at, i), ns)
        })
        .collect();
    let vectors: Vec<f32> = par_chunks(n, 256, |r| {
        r.flat_map(|i| provider.vector(&document_text(&memories[i])))
            .collect()
    });
    let vector = |i: usize| &vectors[i * dim..(i + 1) * dim];
    let terms: Vec<BTreeSet<String>> = memories
        .iter()
        .map(|m| m.tag_terms().into_iter().collect())
        .collect();

    // Semantic links, made when the vector lands: cosine at least 0.50, or 0.45 with a
    // shared tag term; highest first, then ID (IDs grow with the index); 15. Rows go
    // in blocks so that each earlier vector is read once per block.
    let semantic: Vec<Vec<(usize, f32)>> = par_chunks(n, 64, |rows| {
        let mut found: Vec<Vec<(usize, f32)>> = vec![Vec::new(); rows.len()];
        for j in 0..rows.end.saturating_sub(1) {
            let other = vector(j);
            for i in rows.start.max(j + 1)..rows.end {
                let cos = dot(vector(i), other).clamp(-1.0, 1.0);
                if cos >= 0.50 || (cos >= 0.45 && terms[i].iter().any(|t| terms[j].contains(t))) {
                    found[i - rows.start].push((j, cos));
                }
            }
        }
        for f in &mut found {
            f.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            f.truncate(15);
        }
        found
    });

    let mut builder = StoreBuilder::new(
        knoggin_core::embed::EmbeddingProvider::identity(&provider).clone(),
        Timestamp::from_millis(START_MS),
    );
    let ids: Vec<MemoryId> = memories.iter().map(|m| m.id).collect();
    let times: Vec<i64> = memories.iter().map(|m| m.created_at.millis()).collect();
    // Canonical entity keys as small integers; each key lists its memories in order.
    let mut key_ids: BTreeMap<String, usize> = BTreeMap::new();
    let mut holders: Vec<Vec<usize>> = Vec::new();
    let mut keys_of: Vec<Vec<usize>> = Vec::with_capacity(n);
    let mut shared = vec![0u8; n];
    let mut touched: Vec<usize> = Vec::new();
    let mut edges = [0; 3];

    for (i, m) in memories.into_iter().enumerate() {
        let (id, at) = (ids[i], times[i]);
        let my_keys: Vec<usize> = m
            .labels
            .entities
            .iter()
            .flat_map(|e| canonicalize_entity(e))
            .collect::<BTreeSet<String>>()
            .into_iter()
            .map(|k| {
                let next = key_ids.len();
                let kid = *key_ids.entry(k).or_insert(next);
                if kid == holders.len() {
                    holders.push(Vec::new());
                }
                kid
            })
            .collect();
        builder.memory(m, Some(vector(i)));

        // Entity links: shared keys descending, then ID; weight shared / union; 10.
        for &k in &my_keys {
            for &j in &holders[k] {
                if shared[j] == 0 {
                    touched.push(j);
                }
                shared[j] += 1;
            }
        }
        let by_rank = |a: &usize, b: &usize| shared[*b].cmp(&shared[*a]).then(a.cmp(b));
        if touched.len() > 10 {
            touched.select_nth_unstable_by(9, by_rank);
            touched.truncate(10);
        }
        touched.sort_by(by_rank);
        for &j in &touched {
            let s = usize::from(shared[j]);
            let union = my_keys.len() + keys_of[j].len() - s;
            builder.edge(auto_edge(
                id,
                ids[j],
                EdgeType::Entity,
                s as f32 / union as f32,
                at,
            ));
            edges[0] += 1;
        }
        for &k in &my_keys {
            for &j in &holders[k] {
                shared[j] = 0;
            }
            holders[k].push(i);
        }
        touched.clear();
        keys_of.push(my_keys);
        // Temporal links: within the hour, nearest first; 20.
        for j in (0..i).rev().take(20) {
            let delta = at - times[j];
            if delta > HOUR_MS {
                break;
            }
            let weight = (1.0 - delta as f64 / HOUR_MS as f64).max(0.05) as f32;
            builder.edge(auto_edge(id, ids[j], EdgeType::Temporal, weight, at));
            edges[1] += 1;
        }
        for &(j, cos) in &semantic[i] {
            builder.edge(auto_edge(id, ids[j], EdgeType::Associative, cos, at));
            edges[2] += 1;
        }
    }

    Built {
        store: builder.build(),
        corpus,
        rng,
        clock,
        ids,
        edges,
        seconds: started.elapsed().as_secs_f64(),
    }
}

/// p50, p95 and max in milliseconds, nearest rank.
fn percentiles(mut samples: Vec<Duration>) -> [f64; 3] {
    samples.sort();
    let at = |p: usize| samples[(samples.len() * p).div_ceil(100).max(1) - 1].as_secs_f64() * 1e3;
    [at(50), at(95), at(100)]
}

fn recall_request(query: &str, depth: u8) -> Recall {
    Recall {
        query: query.to_owned(),
        namespace: None,
        limit: 25,
        full_text_count: None,
        tags: Vec::new(),
        entities: Vec::new(),
        topics: Vec::new(),
        emotions: Vec::new(),
        min_strength: None,
        depth,
        time_range_start: None,
        time_range_end: None,
        detail: Detail::Compact,
        record_access: true,
    }
}

fn timed(mut f: impl FnMut()) -> Duration {
    let t = Instant::now();
    f();
    t.elapsed()
}

/// One row of the table: name, p50, p95, max, budget.
type Row = (&'static str, [f64; 3], Option<f64>);

/// Builds a store of `n` memories, times every operation and returns the rows.
fn measure(n: usize, seed: u64, budgets: [Option<f64>; 4]) -> Vec<Row> {
    let provider = FakeProvider::with_dim(DIM);
    let model = knoggin_core::embed::EmbeddingProvider::identity(&provider).clone();
    let Built {
        mut store,
        corpus,
        mut rng,
        mut clock,
        mut ids,
        edges: [entity, temporal, semantic],
        seconds,
    } = build(n, seed);
    println!(
        "\n{n} memories in one namespace, {DIM} dimensions, built in {seconds:.1} s. Edges: {entity} entity, {temporal} temporal, {semantic} associative ({:.1} per memory)",
        (entity + temporal + semantic) as f64 / n as f64
    );
    let ctx = WriteContext {
        saved_by: "local".into(),
        client: None,
    };

    // Saves, without a vector.
    let drafts: Vec<Draft> = (0..OPS)
        .map(|_| corpus.draft(&mut rng, &mut clock))
        .collect();
    let mut save = Vec::with_capacity(OPS);
    let mut now = Timestamp::from_millis(0);
    for (k, d) in drafts.into_iter().enumerate() {
        let id = id_for(d.created_at, n + k);
        now = Timestamp::from_millis(d.created_at);
        let req = StoreMemory {
            summary: d.summary,
            full_text: d.full_text,
            namespace: None,
            tags: d.tags,
            entities: d.labels.entities,
            topics: d.labels.topics,
            emotions: d.labels.emotions,
            parent_id: None,
            check_duplicates: false,
            created_at: None,
        };
        save.push(timed(|| {
            store
                .store_memory(req.clone(), id, None, &ctx, now)
                .unwrap();
        }));
        ids.push(id);
    }

    // The 1,000 new memories' vectors land, each with its link scan.
    let jobs = store.pending_embeddings(OPS);
    assert_eq!(jobs.len(), OPS);
    let mut apply = Vec::with_capacity(OPS);
    for job in jobs {
        let vector = provider.vector(&job.text);
        apply.push(timed(|| {
            let outcome = store.apply_embedding(job.id, &model, vector.clone(), now);
            assert!(matches!(outcome, ApplyOutcome::Applied { .. }));
        }));
    }

    // Recalls with depth 0 and 1 in turn, and gets.
    let now = Timestamp::from_millis(now.millis() + MINUTE);
    let queries: Vec<(Recall, Vec<f32>)> = (0..OPS)
        .map(|k| {
            let q = corpus.query(&mut rng);
            let v = provider.vector(&query_text(&q));
            (recall_request(&q, (k % 2) as u8), v)
        })
        .collect();
    let mut recall = Vec::with_capacity(OPS);
    let mut returned = 0;
    for (req, v) in &queries {
        recall.push(timed(|| {
            returned += store.recall(req, Some(v), false, now).unwrap().hits.len();
        }));
    }
    let mut get = Vec::with_capacity(OPS);
    for _ in 0..OPS {
        let req = GetMemory {
            id: ids[rng.below(ids.len())],
            namespace: None,
            record_access: true,
            include_history: false,
        };
        get.push(timed(|| {
            store.get_memory(&req, now).unwrap();
        }));
    }
    assert!(
        returned > OPS * 10,
        "recalls returned too little: {returned}"
    );

    // Eight recalls at once behind the host's read lock.
    let lock = RwLock::new(store);
    let parallel: Vec<Duration> = std::thread::scope(|s| {
        let handles: Vec<_> = queries
            .chunks(OPS / THREADS)
            .map(|chunk| {
                let lock = &lock;
                s.spawn(move || {
                    chunk
                        .iter()
                        .map(|(req, v)| {
                            timed(|| {
                                let store = lock.read().unwrap();
                                store.recall(req, Some(v), false, now).unwrap();
                            })
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });

    vec![
        ("save", percentiles(save), budgets[0]),
        ("apply_embedding", percentiles(apply), budgets[1]),
        ("recall", percentiles(recall), budgets[2]),
        ("get", percentiles(get), budgets[3]),
        ("recall, 8 at once", percentiles(parallel), None),
    ]
}

#[test]
#[ignore = "timing"]
fn latency_budgets() {
    let mut misses = Vec::new();
    for (n, budgets) in [
        (15_000, [Some(20.0), Some(20.0), Some(100.0), Some(2.0)]),
        (75_000, [None, None, Some(400.0), None]),
    ] {
        let rows = measure(n, 0x6b6e_6f67_6769_6e00 ^ n as u64, budgets);
        println!(
            "{:<18} {:>9} {:>9} {:>9} {:>11}",
            "operation", "p50 ms", "p95 ms", "max ms", "p95 budget"
        );
        for (name, [p50, p95, max], budget) in rows {
            let mark = match budget {
                Some(b) if p95 > b => {
                    misses.push(format!("{n} memories: {name} p95 {p95:.2} ms > {b} ms"));
                    format!("{b} MISS")
                }
                Some(b) => format!("{b} ok"),
                None => "-".to_owned(),
            };
            println!("{name:<18} {p50:>9.2} {p95:>9.2} {max:>9.2} {mark:>11}");
        }
    }
    assert!(misses.is_empty(), "budgets missed:\n{}", misses.join("\n"));
}
