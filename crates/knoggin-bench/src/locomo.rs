// SPDX-License-Identifier: AGPL-3.0-only
//! The LoCoMo run, ported from recalld's `src/bench/locomo.rs`: ingestion, the search
//! step, answering, judging and the report. Memories live in an in-process
//! `knoggin_core::Store`; the harness does the host's part (embedding, access recording).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Instant;

use anyhow::Context as _;
use knoggin_core::embed::{EmbeddingProvider, query_text};
use knoggin_core::id::{MemoryId, StoreId};
use knoggin_core::ops::{
    ApplyOutcome, Detail, GetMemory, Recall, RecallResponse, StoreMemory, SupersedeMemory,
    TimeValue, WriteContext,
};
use knoggin_core::store::Store;
use knoggin_core::time::Timestamp;
use serde::Serialize;
use uuid::Uuid;

use crate::data::{Conversation, category_name};
use crate::llm::{JudgeStyle, LlmClient, MemoryToStore, SearchParams, SearchQuery};

/// Replay clock start (2020-01-01), before every LoCoMo session date.
const BENCH_CLOCK_START_MS: i64 = 1_577_836_800_000;
/// Turns of context the ingestion model sees before the latest one.
const CONTEXT_WINDOW: usize = 20;
/// Recently saved memories the ingestion model sees, so it does not save them again.
const MEMORY_WINDOW: usize = 30;

/// Settings of one run.
#[derive(Debug)]
pub(crate) struct RunConfig {
    pub(crate) data: PathBuf,
    pub(crate) out_dir: PathBuf,
    pub(crate) top_k: usize,
    pub(crate) full_text_count: usize,
    pub(crate) skip_adversarial: bool,
    pub(crate) parallel: usize,
    pub(crate) qa_parallel: usize,
    pub(crate) json: bool,
}

/// The three models of a run.
#[derive(Debug, Clone)]
pub(crate) struct Models {
    pub(crate) answer: LlmClient,
    pub(crate) ingest: LlmClient,
    pub(crate) judge: LlmClient,
}

/// The embedding providers: documents (the queue) and queries.
#[derive(Clone)]
pub(crate) struct Embedders {
    pub(crate) document: Arc<dyn EmbeddingProvider>,
    pub(crate) query: Arc<dyn EmbeddingProvider>,
}

impl std::fmt::Debug for Embedders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedders")
            .field("model", &self.document.identity().id)
            .finish()
    }
}

// ── The store and the host's part ─────────────────────────────────

/// One conversation's store, with the replay clock and the host's duties.
pub(crate) struct Harness {
    store: RwLock<Store>,
    /// Replay clock in ms: moved forward to each turn's time, never back.
    clock: AtomicI64,
    embedders: Embedders,
}

impl std::fmt::Debug for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Harness").finish_non_exhaustive()
    }
}

/// One memory of a search result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScoredMemory {
    pub(crate) id: MemoryId,
    pub(crate) summary: String,
    /// Set only once the list is final, for the first `fullTextCount` memories.
    pub(crate) full_text: Option<String>,
    pub(crate) score: f32,
    pub(crate) created_at: i64,
    pub(crate) entities: Vec<String>,
    pub(crate) topics: Vec<String>,
    pub(crate) emotions: Vec<String>,
}

/// One filtered search: one `recall`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RecallSpec {
    pub(crate) query: String,
    pub(crate) entities: Vec<String>,
    pub(crate) topics: Vec<String>,
    pub(crate) emotions: Vec<String>,
    pub(crate) depth: u8,
    pub(crate) time_range: Option<(Option<i64>, Option<i64>)>,
    pub(crate) limit: usize,
}

impl Harness {
    pub(crate) fn new(embedders: Embedders) -> Self {
        let start = Timestamp::from_millis(BENCH_CLOCK_START_MS);
        let model = embedders.document.identity().clone();
        Self {
            store: RwLock::new(Store::create(
                StoreId::from_uuid(Uuid::now_v7()),
                model,
                start,
            )),
            clock: AtomicI64::new(BENCH_CLOCK_START_MS),
            embedders,
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Store> {
        self.store.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Store> {
        self.store.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.clock.load(Ordering::SeqCst))
    }

    /// Moves the replay clock to `ms` unless it is already later, as recalld's `ManualClock`.
    fn advance_to(&self, ms: i64) -> Timestamp {
        let before = self.clock.fetch_max(ms, Ordering::SeqCst);
        Timestamp::from_millis(before.max(ms))
    }

    /// Saves one memory at the turn's time, as recalld's harness did through the
    /// `store_memory` tool. A `supersedes` that names no usable memory is dropped and the
    /// memory saved on its own, as an agent would.
    pub(crate) fn save(&self, mem: &MemoryToStore, timestamp_ms: i64) -> anyhow::Result<MemoryId> {
        let now = self.advance_to(timestamp_ms);
        let ctx = WriteContext {
            saved_by: "local".to_owned(),
            client: Some("knoggin-bench".to_owned()),
        };
        let id = MemoryId::from_uuid(Uuid::now_v7());
        // As recalld's harness: entities keep their case, topics and emotions are
        // lower-cased. Knoggin takes any non-empty label, so none needs cleaning.
        let entities = labels(&mem.entities, false);
        let topics = labels(&mem.topics, true);
        let emotions = labels(&mem.emotions, true);
        let mut store = self.write();
        if let Some(old) = mem
            .supersedes
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            let req = SupersedeMemory {
                id: MemoryId::from_uuid(old),
                summary: mem.summary.clone(),
                full_text: mem.full_text.clone(),
                tags: Vec::new(),
                entities: entities.clone(),
                topics: topics.clone(),
                emotions: emotions.clone(),
                check_duplicates: false,
                created_at: None,
            };
            if let Ok(saved) = store.supersede_memory(req, id, None, &ctx, now) {
                return Ok(saved.id);
            }
        }
        let req = StoreMemory {
            summary: mem.summary.clone(),
            full_text: mem.full_text.clone(),
            namespace: None,
            tags: Vec::new(),
            entities,
            topics,
            emotions,
            parent_id: None,
            check_duplicates: false,
            created_at: None,
        };
        let saved = store
            .store_memory(req, id, None, &ctx, now)
            .map_err(|e| anyhow::anyhow!("store_memory refused: {e}"))?;
        Ok(saved.id)
    }

    /// The host's embedding queue: embeds every pending memory until none is left.
    /// Returns how many vectors landed.
    pub(crate) fn drain_embeddings(&self) -> usize {
        let model = self.embedders.document.identity().clone();
        let mut applied = 0;
        loop {
            let jobs = self.read().pending_embeddings(32);
            if jobs.is_empty() {
                return applied;
            }
            for job in jobs {
                // The model runs outside the lock, as the host's embedding threads do.
                let vector = self.embedders.document.embed(&job.text);
                let now = self.now();
                let mut store = self.write();
                match vector {
                    Ok(vector) => {
                        if let ApplyOutcome::Applied { .. } =
                            store.apply_embedding(job.id, &model, vector, now)
                        {
                            applied += 1;
                        }
                    }
                    Err(_) => store.fail_embedding(job.id),
                }
            }
        }
    }

    /// One recall with the query's own text and filters, then `record_access`, as the
    /// host does with access recording on.
    pub(crate) fn recall(&self, spec: &RecallSpec) -> anyhow::Result<Vec<ScoredMemory>> {
        let (time_range_start, time_range_end) = match spec.time_range {
            // Knoggin wants both bounds or neither; recalld took one alone.
            Some((start, end)) if start.is_some() || end.is_some() => (
                Some(TimeValue::Millis(
                    start.unwrap_or(Timestamp::MIN_API.millis()),
                )),
                Some(TimeValue::Millis(
                    end.unwrap_or(Timestamp::MAX_API.millis()),
                )),
            ),
            _ => (None, None),
        };
        let req = Recall {
            query: spec.query.clone(),
            namespace: None,
            limit: u32::try_from(spec.limit).unwrap_or(u32::MAX),
            full_text_count: Some(0),
            tags: Vec::new(),
            entities: spec.entities.clone(),
            topics: spec.topics.clone(),
            emotions: spec.emotions.clone(),
            min_strength: None,
            depth: spec.depth,
            time_range_start,
            time_range_end,
            detail: Detail::Full,
            record_access: true,
        };
        let vector = self.embedders.query.embed(&query_text(&spec.query)).ok();
        let now = self.now();
        let result = self
            .read()
            .recall(&req, vector.as_deref(), false, now)
            .map_err(|e| anyhow::anyhow!("recall refused: {e}"))?;
        self.write().record_access(&result.hits, now);
        let RecallResponse::Full(full) = result.response else {
            anyhow::bail!("recall answered compact although full was asked for");
        };
        Ok(full
            .memories
            .into_iter()
            .filter_map(|hit| {
                Some(ScoredMemory {
                    id: hit.memory.id,
                    summary: hit.memory.summary?,
                    full_text: None,
                    score: hit.score,
                    created_at: hit.memory.created_at.millis(),
                    entities: hit.memory.entities,
                    topics: hit.memory.topics,
                    emotions: hit.memory.emotions,
                })
            })
            .collect())
    }

    /// Knoggin's default recall shape on the final list: full text for the first `count`
    /// memories, taken from the store, and summaries only for the rest.
    pub(crate) fn fill_full_texts(&self, list: &mut [ScoredMemory], count: usize) {
        let now = self.now();
        let store = self.read();
        for (i, m) in list.iter_mut().enumerate() {
            m.full_text = if i < count {
                let req = GetMemory {
                    id: m.id,
                    namespace: None,
                    record_access: false,
                    include_history: false,
                };
                store.get_memory(&req, now).ok().and_then(|v| v.full_text)
            } else {
                None
            };
        }
    }
}

/// The non-empty labels of `items`, trimmed, lower-cased when `lower`.
fn labels(items: &[String], lower: bool) -> Vec<String> {
    items
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| {
            if lower {
                l.to_lowercase()
            } else {
                l.to_owned()
            }
        })
        .collect()
}

/// Runs `f` on a blocking thread: the model and the engine are CPU work.
async fn blocking<T: Send + 'static>(
    harness: &Arc<Harness>,
    f: impl FnOnce(&Harness) -> T + Send + 'static,
) -> anyhow::Result<T> {
    let harness = Arc::clone(harness);
    tokio::task::spawn_blocking(move || f(&harness))
        .await
        .map_err(|e| {
            if e.is_panic() {
                anyhow::anyhow!("a blocking task panicked")
            } else {
                anyhow::anyhow!("a blocking task was cancelled")
            }
        })
}

// ── Ingestion ─────────────────────────────────────────────────────

/// The text of one turn as the ingestion model sees it.
fn turn_text(turn: &crate::data::Turn) -> String {
    let mut text = format!("{}: {}", turn.speaker, turn.text);
    if let Some(caption) = &turn.blip_caption {
        let query = turn.image_query.as_deref().unwrap_or("shared image");
        let _ = write!(
            text,
            "\n[{} shared an image \u{2014} \"{}\". The image shows: {}]",
            turn.speaker, query, caption
        );
    }
    text
}

/// Feeds every turn to the ingestion model and saves what it returns. A refused save
/// is skipped and reported, as an agent would move on.
async fn ingest_conversation(
    harness: &Arc<Harness>,
    conv: &Conversation,
    llm: &LlmClient,
    label: &str,
) -> anyhow::Result<usize> {
    let mut stored_memories: Vec<(String, String)> = Vec::new();
    let mut recent_turns: Vec<String> = Vec::new();
    let total_turns = conv.turns.len();

    for (turn_idx, turn) in conv.turns.iter().enumerate() {
        if turn_idx % 50 == 0 && turn_idx > 0 {
            eprintln!("  {label}: ingesting... {turn_idx}/{total_turns} turns");
        }
        let text = turn_text(turn);
        let context = &recent_turns[recent_turns.len().saturating_sub(CONTEXT_WINDOW)..];
        let recent_mems = &stored_memories[stored_memories.len().saturating_sub(MEMORY_WINDOW)..];
        let memories = llm
            .process_turn(context, &text, recent_mems, turn.timestamp_ms)
            .await
            .unwrap_or_default();

        for mem in memories {
            let ts = turn.timestamp_ms;
            let summary = mem.summary.clone();
            let saved = blocking(harness, move |h| {
                let id = h.save(&mem, ts);
                // Vectors land before the next turn, as recalld embedded on save.
                h.drain_embeddings();
                id
            })
            .await?;
            match saved {
                Ok(id) => stored_memories.push((id.to_string(), summary)),
                Err(error) => eprintln!("  {label}: store rejected: {error}"),
            }
        }
        recent_turns.push(text);
    }
    Ok(stored_memories.len())
}

// ── Search ────────────────────────────────────────────────────────

/// Keeps the higher score per memory.
pub(crate) fn merge(into: &mut HashMap<MemoryId, ScoredMemory>, hits: Vec<ScoredMemory>) {
    for hit in hits {
        match into.get(&hit.id) {
            Some(existing) if existing.score >= hit.score => {}
            _ => {
                into.insert(hit.id, hit);
            }
        }
    }
}

/// Best score first (ties by ID, so runs repeat), cut to `top_k`.
pub(crate) fn ranked(merged: HashMap<MemoryId, ScoredMemory>, top_k: usize) -> Vec<ScoredMemory> {
    let mut list: Vec<ScoredMemory> = merged.into_values().collect();
    list.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    list.truncate(top_k);
    list
}

/// The recalls of one round of the search step.
pub(crate) fn recall_specs(params: &SearchParams, top_k: usize, followup: bool) -> Vec<RecallSpec> {
    params
        .queries
        .iter()
        .filter(|q| !q.query.trim().is_empty())
        .map(|q| RecallSpec {
            query: q.query.clone(),
            entities: labels(&params.entities, false),
            topics: labels(&params.topics, false),
            emotions: labels(&params.emotions, false),
            depth: u8::try_from(params.depth.min(2)).unwrap_or(2),
            time_range: (!followup).then_some((params.time_range_start, params.time_range_end)),
            limit: top_k,
        })
        .collect()
}

/// Runs one recall; a request the store refuses is tried once without its time range,
/// then skipped. The model writes these filters, so a refusal is not the store's fault.
async fn run_recall(
    harness: &Arc<Harness>,
    spec: RecallSpec,
    debug: &mut String,
) -> anyhow::Result<Vec<ScoredMemory>> {
    let first = spec.clone();
    match blocking(harness, move |h| h.recall(&first)).await? {
        Ok(hits) => Ok(hits),
        Err(error) if spec.time_range.is_some() => {
            let _ = writeln!(
                debug,
                "  Recall refused ({error}); retrying without time range"
            );
            let retry = RecallSpec {
                time_range: None,
                ..spec
            };
            Ok(blocking(harness, move |h| h.recall(&retry))
                .await?
                .unwrap_or_else(|error| {
                    let _ = writeln!(debug, "  Recall refused again ({error}); skipped");
                    Vec::new()
                }))
        }
        Err(error) => {
            let _ = writeln!(debug, "  Recall refused ({error}); skipped");
            Ok(Vec::new())
        }
    }
}

/// The search step: the model writes one to three queries, each is one recall, the
/// results merge; then one follow-up round may add more.
pub(crate) async fn search_memories(
    harness: &Arc<Harness>,
    question: &str,
    top_k: usize,
    llm: &LlmClient,
    debug: &mut String,
) -> anyhow::Result<Vec<ScoredMemory>> {
    let params = match llm.construct_search_query(question).await {
        Ok(params) => params,
        Err(e) => {
            eprintln!("      Warning: search query construction failed: {e}");
            SearchParams {
                queries: vec![SearchQuery {
                    query: question.to_owned(),
                    fts_query: None,
                }],
                entities: Vec::new(),
                topics: Vec::new(),
                emotions: Vec::new(),
                depth: 1,
                time_range_start: None,
                time_range_end: None,
            }
        }
    };
    let _ = writeln!(
        debug,
        "  Depth: {}, TimeRange: {:?}..{:?}",
        params.depth, params.time_range_start, params.time_range_end
    );
    if !(params.entities.is_empty() && params.topics.is_empty() && params.emotions.is_empty()) {
        let _ = writeln!(
            debug,
            "  Filters: entities {:?} topics {:?} emotions {:?}",
            params.entities, params.topics, params.emotions
        );
    }

    let mut merged = HashMap::new();
    for (qi, (q, spec)) in params
        .queries
        .iter()
        .filter(|q| !q.query.trim().is_empty())
        .zip(recall_specs(&params, top_k, false))
        .enumerate()
    {
        let _ = writeln!(debug, "  Query[{}]: {:?}", qi + 1, q.query);
        if let Some(fq) = &q.fts_query {
            let _ = writeln!(debug, "  FTS[{}]:   {:?} (unused)", qi + 1, fq);
        }
        merge(&mut merged, run_recall(harness, spec, debug).await?);
    }
    let mut results = ranked(merged, top_k);
    for (i, r) in results.iter().enumerate().take(5) {
        let snippet: String = r.summary.chars().take(120).collect();
        let _ = writeln!(debug, "  [{}] score={:.3} | {}", i + 1, r.score, snippet);
    }

    // One follow-up round, as the published harness.
    let summaries = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let snippet: String = r.summary.chars().take(150).collect();
            format!("[{}] {}", i + 1, snippet)
        })
        .collect::<Vec<_>>()
        .join("\n");
    match llm.construct_followup_query(question, &summaries).await {
        Ok(Some(followup)) => {
            let _ = writeln!(debug, "  -- Follow-up round 1 --");
            let mut merged: HashMap<MemoryId, ScoredMemory> =
                results.iter().map(|r| (r.id, r.clone())).collect();
            let before = merged.len();
            for (qi, spec) in recall_specs(&followup, top_k, true).into_iter().enumerate() {
                let _ = writeln!(debug, "  FU1-Query[{}]: {:?}", qi + 1, spec.query);
                merge(&mut merged, run_recall(harness, spec, debug).await?);
            }
            let new_unique = merged.len() - before;
            results = ranked(merged, top_k);
            let _ = writeln!(
                debug,
                "  Follow-up round 1: {new_unique} new, {} total after merge",
                results.len()
            );
        }
        Ok(None) => {
            let _ = writeln!(debug, "  Follow-up round 1: not needed");
        }
        Err(e) => {
            let _ = writeln!(debug, "  Follow-up round 1: error: {e}");
        }
    }
    Ok(results)
}

// ── What the answering model sees ─────────────────────────────────

/// One memory as the answering model sees it.
#[derive(Debug, Clone)]
pub(crate) struct MemoryContext {
    pub(crate) text: String,
    pub(crate) score: f32,
    pub(crate) created_at: i64,
    pub(crate) entities: Vec<String>,
    pub(crate) topics: Vec<String>,
    pub(crate) emotions: Vec<String>,
}

impl From<&ScoredMemory> for MemoryContext {
    fn from(m: &ScoredMemory) -> Self {
        let text = match &m.full_text {
            Some(ft) => format!("{}\nOriginal: {}", m.summary, ft),
            None => m.summary.clone(),
        };
        Self {
            text,
            score: m.score,
            created_at: m.created_at,
            entities: m.entities.clone(),
            topics: m.topics.clone(),
            emotions: m.emotions.clone(),
        }
    }
}

fn utc(millis: i64) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).ok()
}

/// `2024-06-24 10:00:00 UTC`, as recalld's `format_timestamp` in UTC.
pub(crate) fn format_timestamp(millis: i64) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    utc(millis).and_then(|t| t.format(format).ok()).map_or_else(
        || format!("<invalid timestamp: {millis}>"),
        |s| format!("{s} UTC"),
    )
}

/// The memory block exactly as the answering model sees it: oldest first, with the time
/// since the one before. recalld's graph-neighbour block is never present.
pub(crate) fn render_memory_context(memories: &[MemoryContext]) -> String {
    let mut sorted: Vec<&MemoryContext> = memories.iter().collect();
    sorted.sort_by_key(|m| m.created_at);
    let mut lines = Vec::with_capacity(sorted.len());
    for (i, mem) in sorted.iter().enumerate() {
        let day_of_week = utc(mem.created_at)
            .map(|t| t.weekday().to_string())
            .unwrap_or_default();
        let mut line = format!(
            "[{}] (score: {:.2}, {} {}) {}",
            i + 1,
            mem.score,
            day_of_week,
            format_timestamp(mem.created_at),
            mem.text
        );
        if let Some(prev) = i.checked_sub(1).and_then(|p| sorted.get(p)) {
            let delta_days = (mem.created_at - prev.created_at) / 86_400_000;
            if delta_days > 0 {
                let delta = if delta_days == 1 {
                    "1 day".to_owned()
                } else if delta_days < 30 {
                    format!("{delta_days} days")
                } else if delta_days < 365 {
                    match delta_days / 30 {
                        1 => "~1 month".to_owned(),
                        months => format!("~{months} months"),
                    }
                } else {
                    let (years, months) = (delta_days / 365, (delta_days % 365) / 30);
                    if months > 0 {
                        format!("~{years} year(s) {months} month(s)")
                    } else {
                        format!("~{years} year(s)")
                    }
                };
                let _ = write!(line, "\n    \u{23f1} {delta} after [{i}]");
            }
        }
        let mut meta = Vec::new();
        if !mem.entities.is_empty() {
            meta.push(format!("Entities: {}", mem.entities.join(", ")));
        }
        if !mem.topics.is_empty() {
            meta.push(format!("Topics: {}", mem.topics.join(", ")));
        }
        if !mem.emotions.is_empty() {
            meta.push(format!("Mood: {}", mem.emotions.join(", ")));
        }
        if !meta.is_empty() {
            let _ = write!(line, "\n    {}", meta.join(" | "));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Mem0's 2026 protocol judges open-domain questions against the gold answer up to its
/// first semicolon.
fn judge_gold(style: JudgeStyle, category: &str, gold: &str) -> String {
    if style == JudgeStyle::Mem0V3 && category == "open-domain" {
        gold.split(';').next().unwrap_or(gold).trim().to_owned()
    } else {
        gold.to_owned()
    }
}

// ── Report ────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize)]
pub(crate) struct CategoryStats {
    pub(crate) total: usize,
    pub(crate) correct: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConversationResult {
    id: String,
    turns: usize,
    memories_stored: usize,
    qa_total: usize,
    qa_correct: usize,
    accuracy: f64,
    ingestion_secs: f64,
    questions: Vec<QuestionResult>,
}

#[derive(Debug, Serialize)]
pub(crate) struct QuestionResult {
    question: String,
    gold_answer: String,
    generated_answer: String,
    correct: bool,
    category: String,
}

/// The report: recalld's `bench_results.json` fields, plus the 95% interval, the
/// full-text count and the size of the memory block.
#[derive(Debug, Default, Serialize)]
pub(crate) struct LocomoReport {
    pub(crate) total_conversations: usize,
    pub(crate) total_qa: usize,
    pub(crate) total_turns: usize,
    pub(crate) total_correct: usize,
    pub(crate) accuracy: f64,
    /// Wilson score interval, 95%, in percent.
    pub(crate) accuracy_ci95: (f64, f64),
    pub(crate) model: String,
    pub(crate) ingest_model: String,
    pub(crate) judge_model: String,
    pub(crate) judge_style: String,
    pub(crate) skip_adversarial: bool,
    pub(crate) stress_test: bool,
    pub(crate) top_k: usize,
    pub(crate) full_text_count: usize,
    pub(crate) categories: BTreeMap<String, CategoryStats>,
    pub(crate) avg_retrieval_ms: f64,
    pub(crate) avg_answer_ms: f64,
    pub(crate) avg_judge_ms: f64,
    /// Mean characters of the memory block per question; tokens come from the debug log.
    pub(crate) avg_context_chars: f64,
    pub(crate) per_conversation: Vec<ConversationResult>,
}

/// The Wilson score interval at 95% for `correct` of `total`, in percent.
pub(crate) fn wilson95(correct: usize, total: usize) -> (f64, f64) {
    if total == 0 {
        return (0.0, 0.0);
    }
    #[expect(clippy::cast_precision_loss, reason = "counts far below 2^52")]
    let (k, n) = (correct as f64, total as f64);
    let z = 1.959_963_985_f64;
    let p = k / n;
    let denom = 1.0 + z * z / n;
    let centre = p + z * z / (2.0 * n);
    let margin = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt();
    (
        (centre - margin) / denom * 100.0,
        (centre + margin) / denom * 100.0,
    )
}

#[expect(clippy::cast_precision_loss, reason = "counts far below 2^52")]
fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

#[expect(clippy::cast_precision_loss, reason = "counts far below 2^52")]
fn percent(correct: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        correct as f64 / total as f64 * 100.0
    }
}

/// The report for people.
pub(crate) fn format_report(report: &LocomoReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n=== LoCoMo Benchmark ({} conversations, {} QA pairs) ===",
        report.total_conversations, report.total_qa
    );
    let _ = writeln!(
        out,
        "  Model: {}    Ingest: {}    Judge: {} ({})    Top-k: {}    Full text: {}    Turns: {}\n",
        report.model,
        report.ingest_model,
        report.judge_model,
        report.judge_style,
        report.top_k,
        report.full_text_count,
        report.total_turns,
    );
    let _ = writeln!(
        out,
        "  Overall accuracy:  {:.1}% ({}/{}), 95% CI [{:.1}%, {:.1}%]\n",
        report.accuracy,
        report.total_correct,
        report.total_qa,
        report.accuracy_ci95.0,
        report.accuracy_ci95.1,
    );
    let _ = writeln!(
        out,
        "  {:<12} {:>6} {:>8} {:>9}",
        "Category", "Count", "Correct", "Accuracy"
    );
    for cat in [
        "single-hop",
        "multi-hop",
        "temporal",
        "open-domain",
        "adversarial",
    ] {
        if let Some(stats) = report.categories.get(cat) {
            let _ = writeln!(
                out,
                "  {:<12} {:>6} {:>8} {:>8.1}%",
                cat,
                stats.total,
                stats.correct,
                percent(stats.correct, stats.total)
            );
        }
    }
    let _ = writeln!(
        out,
        "\n  Latency (avg):  retrieval: {:.0}ms    answer: {:.0}ms    judge: {:.0}ms",
        report.avg_retrieval_ms, report.avg_answer_ms, report.avg_judge_ms,
    );
    let _ = writeln!(
        out,
        "  Memory block (avg): {:.0} characters",
        report.avg_context_chars
    );
    out
}

// ── The run ───────────────────────────────────────────────────────

struct QaOutcome {
    idx: usize,
    result: QuestionResult,
    debug: String,
    retrieval_ms: f64,
    answer_ms: f64,
    judge_ms: f64,
    context_chars: f64,
}

struct ConversationOutcome {
    result: ConversationResult,
    debug: String,
    retrieval: Vec<f64>,
    answer: Vec<f64>,
    judge: Vec<f64>,
    context_chars: Vec<f64>,
}

#[derive(Debug, Clone, Copy)]
struct QaSettings {
    top_k: usize,
    full_text_count: usize,
}

/// One question: search, answer, judge.
async fn run_question(
    harness: Arc<Harness>,
    models: Models,
    question: String,
    gold_answer: String,
    category: u32,
    is_adversarial: bool,
    settings: QaSettings,
) -> anyhow::Result<(QuestionResult, String, [f64; 4])> {
    let cat_name = category_name(category).to_owned();
    let mut debug = String::new();

    let start = Instant::now();
    let mut memories = search_memories(
        &harness,
        &question,
        settings.top_k,
        &models.answer,
        &mut debug,
    )
    .await?;
    let count = settings.full_text_count;
    memories = blocking(&harness, move |h| {
        h.fill_full_texts(&mut memories, count);
        memories
    })
    .await?;
    let retrieval_ms = start.elapsed().as_secs_f64() * 1000.0;

    let contexts: Vec<MemoryContext> = memories.iter().map(MemoryContext::from).collect();
    let start = Instant::now();
    let generated = models.answer.generate_answer(&question, &contexts).await?;
    let answer_ms = start.elapsed().as_secs_f64() * 1000.0;
    let rendered = render_memory_context(&contexts);
    let _ = writeln!(
        debug,
        "  MemCtx: {}",
        serde_json::json!({
            "question": question,
            "category": cat_name,
            "memories": contexts.len(),
            "fullTexts": memories.iter().filter(|m| m.full_text.is_some()).count(),
            "neighbors": 0,
            "context": rendered,
        })
    );

    let start = Instant::now();
    let gold = judge_gold(models.judge.judge_style(), &cat_name, &gold_answer);
    let verdict = models
        .judge
        .judge_answer(&question, &gold, &generated, is_adversarial)
        .await?;
    let judge_ms = start.elapsed().as_secs_f64() * 1000.0;

    let _ = writeln!(debug, "  Q: {question}");
    let _ = writeln!(debug, "  Gold: {gold_answer}");
    let _ = writeln!(debug, "  Gen:  {generated}");
    let _ = writeln!(
        debug,
        "  -> {} ({cat_name})\n",
        if verdict.correct { "CORRECT" } else { "WRONG" }
    );
    #[expect(clippy::cast_precision_loss, reason = "a few thousand characters")]
    let chars = rendered.chars().count() as f64;
    Ok((
        QuestionResult {
            question,
            gold_answer,
            generated_answer: generated,
            correct: verdict.correct,
            category: cat_name,
        },
        debug,
        [retrieval_ms, answer_ms, judge_ms, chars],
    ))
}

async fn run_conversation(
    conv: Arc<Conversation>,
    pos: String,
    models: Models,
    embedders: Embedders,
    settings: QaSettings,
    skip_adversarial: bool,
    qa_parallel: usize,
) -> anyhow::Result<ConversationOutcome> {
    let tag = format!("{pos} {}", conv.id);
    let active: Vec<usize> = (0..conv.qa_pairs.len())
        .filter(|&i| !(skip_adversarial && conv.qa_pairs[i].is_adversarial))
        .collect();
    eprintln!("  {tag}: {} turns, {} QA", conv.turns.len(), active.len());

    let harness = Arc::new(Harness::new(embedders));
    eprintln!("  {tag}: ingesting...");
    let start = Instant::now();
    let stored = ingest_conversation(&harness, &conv, &models.ingest, &tag)
        .await
        .with_context(|| format!("{tag}: ingestion failed"))?;
    let ingestion_secs = start.elapsed().as_secs_f64();
    eprintln!("  {tag}: ingested {stored} memories in {ingestion_secs:.1}s");

    let qa_total = active.len();
    let sem = Arc::new(tokio::sync::Semaphore::new(qa_parallel.max(1)));
    let done = Arc::new(AtomicUsize::new(0));
    let correct = Arc::new(AtomicUsize::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    for (idx, qa_idx) in active.into_iter().enumerate() {
        let (harness, models, conv, sem) = (
            Arc::clone(&harness),
            models.clone(),
            Arc::clone(&conv),
            Arc::clone(&sem),
        );
        let (done, correct, tag) = (Arc::clone(&done), Arc::clone(&correct), tag.clone());
        tasks.spawn(async move {
            let _permit = sem.acquire().await?;
            let qa = &conv.qa_pairs[qa_idx];
            let (result, debug, [retrieval_ms, answer_ms, judge_ms, context_chars]) = run_question(
                harness,
                models,
                qa.question.clone(),
                qa.gold_answer.clone(),
                qa.category,
                qa.is_adversarial,
                settings,
            )
            .await?;
            if result.correct {
                correct.fetch_add(1, Ordering::Relaxed);
            }
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 20 == 0 || n == qa_total {
                let acc = percent(correct.load(Ordering::Relaxed), n);
                eprintln!("  {tag}: QA {n}/{qa_total} ({acc:.1}%)");
            }
            anyhow::Ok(QaOutcome {
                idx,
                result,
                debug,
                retrieval_ms,
                answer_ms,
                judge_ms,
                context_chars,
            })
        });
    }
    let mut outcomes = Vec::with_capacity(qa_total);
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(outcome)) => outcomes.push(outcome),
            Ok(Err(e)) => anyhow::bail!("{tag}: QA task failed: {e:#}"),
            Err(_) => anyhow::bail!("{tag}: QA task panicked"),
        }
    }
    outcomes.sort_by_key(|o| o.idx);

    let mut out = ConversationOutcome {
        result: ConversationResult {
            id: conv.id.clone(),
            turns: conv.turns.len(),
            memories_stored: stored,
            qa_total,
            qa_correct: outcomes.iter().filter(|o| o.result.correct).count(),
            accuracy: 0.0,
            ingestion_secs,
            questions: Vec::new(),
        },
        debug: String::new(),
        retrieval: Vec::new(),
        answer: Vec::new(),
        judge: Vec::new(),
        context_chars: Vec::new(),
    };
    out.result.accuracy = percent(out.result.qa_correct, qa_total);
    for o in outcomes {
        out.debug.push_str(&o.debug);
        out.retrieval.push(o.retrieval_ms);
        out.answer.push(o.answer_ms);
        out.judge.push(o.judge_ms);
        out.context_chars.push(o.context_chars);
        out.result.questions.push(o.result);
    }
    Ok(out)
}

fn style_name(style: JudgeStyle) -> &'static str {
    match style {
        JudgeStyle::Recalld => "recalld",
        JudgeStyle::Mem0 => "mem0",
        JudgeStyle::Mem0V3 => "mem0-v3",
    }
}

/// The whole run: every conversation in its own store, then the report.
pub(crate) async fn run(
    config: RunConfig,
    models: Models,
    embedders: Embedders,
) -> anyhow::Result<()> {
    eprintln!("  Loading dataset from {}...", config.data.display());
    let now_ms = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
    let conversations = crate::data::load(&config.data, now_ms)?;
    let total_qa: usize = conversations
        .iter()
        .flat_map(|c| &c.qa_pairs)
        .filter(|q| !(config.skip_adversarial && q.is_adversarial))
        .count();
    let total_turns: usize = conversations.iter().map(|c| c.turns.len()).sum();
    eprintln!(
        "  {} conversations, {total_qa} QA pairs, {total_turns} turns, parallelism: {}",
        conversations.len(),
        config.parallel
    );

    let mut report = LocomoReport {
        total_conversations: conversations.len(),
        total_qa,
        total_turns,
        model: models.answer.model().to_owned(),
        ingest_model: models.ingest.model().to_owned(),
        judge_model: models.judge.model().to_owned(),
        judge_style: style_name(models.judge.judge_style()).to_owned(),
        skip_adversarial: config.skip_adversarial,
        top_k: config.top_k,
        full_text_count: config.full_text_count,
        ..LocomoReport::default()
    };

    std::fs::create_dir_all(&config.out_dir)?;
    let debug_path = config.out_dir.join("bench_debug.log");
    let mut debug_log = std::io::BufWriter::new(std::fs::File::create(&debug_path)?);
    eprintln!("  Debug log: {}", debug_path.display());

    let settings = QaSettings {
        top_k: config.top_k,
        full_text_count: config.full_text_count,
    };
    let total = conversations.len();
    let sem = Arc::new(tokio::sync::Semaphore::new(config.parallel.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    for (idx, conv) in conversations.into_iter().enumerate() {
        let (sem, models, embedders) = (Arc::clone(&sem), models.clone(), embedders.clone());
        let (skip, qa_parallel) = (config.skip_adversarial, config.qa_parallel);
        tasks.spawn(async move {
            let _permit = sem.acquire().await?;
            run_conversation(
                Arc::new(conv),
                format!("[{}/{total}]", idx + 1),
                models,
                embedders,
                settings,
                skip,
                qa_parallel,
            )
            .await
        });
    }

    let (mut retrieval, mut answer, mut judge, mut chars) = (vec![], vec![], vec![], vec![]);
    while let Some(joined) = tasks.join_next().await {
        let outcome = match joined {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(e)) => {
                eprintln!("  ERROR: conversation task failed: {e:#}");
                continue;
            }
            Err(_) => {
                eprintln!("  ERROR: conversation task panicked");
                continue;
            }
        };
        std::io::Write::write_all(&mut debug_log, outcome.debug.as_bytes())?;
        std::io::Write::flush(&mut debug_log)?;
        retrieval.extend(outcome.retrieval);
        answer.extend(outcome.answer);
        judge.extend(outcome.judge);
        chars.extend(outcome.context_chars);
        for q in &outcome.result.questions {
            let stats = report.categories.entry(q.category.clone()).or_default();
            stats.total += 1;
            if q.correct {
                stats.correct += 1;
                report.total_correct += 1;
            }
        }
        report.per_conversation.push(outcome.result);
    }
    report.per_conversation.sort_by(|a, b| a.id.cmp(&b.id));
    report.accuracy = percent(report.total_correct, report.total_qa);
    report.accuracy_ci95 = wilson95(report.total_correct, report.total_qa);
    report.avg_retrieval_ms = mean(&retrieval);
    report.avg_answer_ms = mean(&answer);
    report.avg_judge_ms = mean(&judge);
    report.avg_context_chars = mean(&chars);

    let json = serde_json::to_string_pretty(&report)?;
    let results_path = config.out_dir.join("bench_results.json");
    std::fs::write(&results_path, &json)?;
    eprintln!("  Results written to {}", results_path.display());
    if config.json {
        println!("{json}");
    } else {
        println!("{}", format_report(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use knoggin_core::testing::FakeProvider;

    fn embedders() -> Embedders {
        let fake: Arc<dyn EmbeddingProvider> = Arc::new(FakeProvider::with_dim(768));
        Embedders {
            document: Arc::clone(&fake),
            query: fake,
        }
    }

    fn mem(summary: &str, full: Option<&str>, entities: &[&str]) -> MemoryToStore {
        MemoryToStore {
            summary: summary.to_owned(),
            full_text: full.map(str::to_owned),
            entities: entities.iter().map(|s| (*s).to_owned()).collect(),
            topics: vec!["pets".to_owned()],
            emotions: Vec::new(),
            supersedes: None,
        }
    }

    fn scored(n: u128, score: f32) -> ScoredMemory {
        ScoredMemory {
            id: MemoryId::from_uuid(Uuid::from_u128(n)),
            summary: format!("m{n}"),
            full_text: None,
            score,
            created_at: 0,
            entities: Vec::new(),
            topics: Vec::new(),
            emotions: Vec::new(),
        }
    }

    #[test]
    fn merge_keeps_the_higher_score_and_ranks() {
        let mut merged = HashMap::new();
        merge(
            &mut merged,
            vec![scored(1, 0.5), scored(2, 0.9), scored(3, 0.1)],
        );
        merge(
            &mut merged,
            vec![scored(1, 0.7), scored(2, 0.2), scored(4, 0.7)],
        );
        assert_eq!(merged.len(), 4);
        let list = ranked(merged, 3);
        let got: Vec<(u128, f32)> = list
            .iter()
            .map(|m| (m.id.uuid().as_u128(), m.score))
            .collect();
        // 1 and 4 tie at 0.7: the lower ID first.
        assert_eq!(got, [(2, 0.9), (1, 0.7), (4, 0.7)]);
    }

    #[test]
    fn recall_specs_carry_filters_and_limits() {
        let params = SearchParams {
            queries: vec![
                SearchQuery {
                    query: "a".into(),
                    fts_query: Some("k".into()),
                },
                SearchQuery {
                    query: "  ".into(),
                    fts_query: None,
                },
                SearchQuery {
                    query: "b".into(),
                    fts_query: None,
                },
            ],
            entities: vec!["Ann".into()],
            topics: vec!["pets".into()],
            emotions: vec!["joy".into()],
            depth: 2,
            time_range_start: Some(1),
            time_range_end: None,
        };
        let specs = recall_specs(&params, 25, false);
        assert_eq!(specs.len(), 2, "an empty query is skipped");
        assert_eq!(specs[0].query, "a");
        assert_eq!(specs[1].query, "b");
        assert!(specs.iter().all(|s| s.limit == 25 && s.depth == 2));
        assert_eq!(specs[0].entities, ["Ann"]);
        assert_eq!(specs[0].topics, ["pets"]);
        assert_eq!(specs[0].emotions, ["joy"]);
        assert_eq!(specs[0].time_range, Some((Some(1), None)));
        let follow = recall_specs(&params, 25, true);
        assert!(follow.iter().all(|s| s.time_range.is_none()));
    }

    /// Saves, embeds until nothing is pending, recalls with label filters, records access,
    /// and gives full text for the first `fullTextCount` of the final list only.
    #[test]
    fn store_recall_and_default_shape() {
        let h = Harness::new(embedders());
        let base = 1_683_554_160_000;
        for i in 0..8_i64 {
            let who = if i % 2 == 0 { "Ann" } else { "Bo" };
            let summary = format!("{who} walked the dog number {i}");
            let full = format!("full text {i}");
            h.save(&mem(&summary, Some(&full), &[who]), base + i * 1000)
                .unwrap();
        }
        assert_eq!(h.read().summary().pending_embeddings, 8);
        assert_eq!(h.drain_embeddings(), 8);
        assert_eq!(h.read().summary().pending_embeddings, 0);
        assert_eq!(
            h.now().millis(),
            base + 7000,
            "the clock is at the last turn"
        );

        let spec = RecallSpec {
            query: "dog walk".into(),
            entities: vec!["ann".into()],
            topics: vec!["pets".into()],
            depth: 0,
            limit: 25,
            ..RecallSpec::default()
        };
        let hits = h.recall(&spec).unwrap();
        assert_eq!(hits.len(), 4, "the entity filter keeps Ann's memories only");
        assert!(
            hits.iter()
                .all(|m| m.entities == ["Ann"] && m.full_text.is_none())
        );
        let accessed = h
            .read()
            .get_memory(
                &GetMemory {
                    id: hits[0].id,
                    namespace: None,
                    record_access: false,
                    include_history: false,
                },
                h.now(),
            )
            .unwrap()
            .access_count;
        assert_eq!(accessed, 1, "the recall was recorded");

        // A list whose order no single recall gave: full text still comes from the store.
        let mut list: Vec<ScoredMemory> = hits.into_iter().rev().collect();
        h.fill_full_texts(&mut list, 2);
        assert!(
            list[0]
                .full_text
                .as_deref()
                .unwrap()
                .starts_with("full text")
        );
        assert!(list[1].full_text.is_some());
        assert!(list[2..].iter().all(|m| m.full_text.is_none()));
        let ctx = MemoryContext::from(&list[0]);
        assert!(ctx.text.contains("\nOriginal: full text"));

        // One-sided time range is filled in, not refused.
        let ranged = RecallSpec {
            time_range: Some((Some(base), None)),
            ..spec
        };
        assert!(!h.recall(&ranged).unwrap().is_empty());
    }

    #[test]
    fn bad_supersedes_still_saves() {
        let h = Harness::new(embedders());
        let first = h
            .save(&mem("Ann has a cat", None, &["Ann"]), 1_683_554_160_000)
            .unwrap();
        let mut next = mem("Ann now has two cats", None, &["Ann"]);
        next.supersedes = Some(first.to_string());
        h.save(&next, 1_683_554_161_000).unwrap();
        next.supersedes = Some(Uuid::from_u128(7).to_string());
        h.save(&next, 1_683_554_162_000).unwrap();
        next.supersedes = Some("not an id".into());
        h.save(&next, 1_683_554_163_000).unwrap();
        assert_eq!(h.read().summary().memory_count, 4);
        // An earlier turn time does not move the clock back.
        h.save(&mem("x", None, &[]), 1_000).unwrap();
        assert_eq!(h.now().millis(), 1_683_554_163_000);
    }

    #[test]
    fn memory_block_matches_recalld() {
        let day = 86_400_000;
        let base = 1_683_554_160_000; // Monday 2023-05-08 13:56 UTC
        let m = |created_at: i64, text: &str, entities: Vec<String>| MemoryContext {
            text: text.to_owned(),
            score: 0.456,
            created_at,
            entities,
            topics: Vec::new(),
            emotions: vec!["happy".into()],
        };
        let block = render_memory_context(&[
            m(base + 40 * day, "later", Vec::new()),
            m(
                base,
                "first\nOriginal: long",
                vec!["Ann".into(), "Bo".into()],
            ),
            m(base + day, "next", Vec::new()),
        ]);
        let expected = "[1] (score: 0.46, Monday 2023-05-08 13:56:00 UTC) first\nOriginal: long\n    \
                        Entities: Ann, Bo | Mood: happy\n\
                        [2] (score: 0.46, Tuesday 2023-05-09 13:56:00 UTC) next\n    \u{23f1} 1 day after [1]\n    Mood: happy\n\
                        [3] (score: 0.46, Saturday 2023-06-17 13:56:00 UTC) later\n    \u{23f1} ~1 month after [2]\n    Mood: happy";
        assert_eq!(block, expected);
        assert_eq!(render_memory_context(&[]), "");
    }

    #[test]
    fn report_has_interval_and_categories() {
        let (lo, hi) = wilson95(1667, 1986);
        assert!(
            (lo - 82.3).abs() < 0.05 && (hi - 85.5).abs() < 0.05,
            "{lo} {hi}"
        );
        let (lo, hi) = wilson95(1305, 1540);
        assert!(
            (lo - 82.9).abs() < 0.05 && (hi - 86.4).abs() < 0.05,
            "{lo} {hi}"
        );
        assert_eq!(wilson95(0, 0), (0.0, 0.0));

        let mut report = LocomoReport {
            total_conversations: 1,
            total_qa: 3,
            total_correct: 2,
            accuracy: percent(2, 3),
            accuracy_ci95: wilson95(2, 3),
            model: "qwen3-235b".into(),
            judge_style: "mem0".into(),
            top_k: 25,
            full_text_count: 5,
            avg_context_chars: 1234.4,
            ..LocomoReport::default()
        };
        report.categories.insert(
            "temporal".into(),
            CategoryStats {
                total: 2,
                correct: 1,
            },
        );
        report.categories.insert(
            "adversarial".into(),
            CategoryStats {
                total: 1,
                correct: 1,
            },
        );
        let text = format_report(&report);
        assert!(
            text.contains("Overall accuracy:  66.7% (2/3), 95% CI [20.8%, 93.9%]"),
            "{text}"
        );
        assert!(text.contains("Top-k: 25    Full text: 5"));
        let temporal = text.find("temporal").unwrap();
        let adversarial = text.find("adversarial").unwrap();
        assert!(temporal < adversarial, "categories in the published order");
        assert!(text.contains("     2        1     50.0%"));
        assert!(text.contains("Memory block (avg): 1234 characters"));
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(json["total_correct"], 2);
        assert_eq!(json["full_text_count"], 5);
        assert_eq!(json["categories"]["temporal"]["correct"], 1);
    }

    #[test]
    fn mem0_v3_trims_open_domain_gold() {
        assert_eq!(judge_gold(JudgeStyle::Mem0V3, "open-domain", "a; b"), "a");
        assert_eq!(judge_gold(JudgeStyle::Mem0, "open-domain", "a; b"), "a; b");
        assert_eq!(judge_gold(JudgeStyle::Mem0V3, "temporal", "a; b"), "a; b");
    }

    #[test]
    fn turn_text_includes_the_image() {
        let turn = crate::data::Turn {
            speaker: "Ann".into(),
            text: "look".into(),
            blip_caption: Some("a dog".into()),
            image_query: None,
            timestamp_ms: 0,
        };
        assert_eq!(
            turn_text(&turn),
            "Ann: look\n[Ann shared an image \u{2014} \"shared image\". The image shows: a dog]"
        );
    }

    const DATA: &str = r#"[{"sample_id": "conv-t",
        "conversation": {
            "session_1_date_time": "1:56 pm on 8 May, 2023",
            "session_1": [
                {"speaker": "Ann", "text": "I adopted a dog named Rex"},
                {"speaker": "Bo", "text": "Nice, I paint landscapes"}],
            "session_2_date_time": "10:00 am on 9 June, 2023",
            "session_2": [
                {"speaker": "Ann", "text": "Rex loves the beach"},
                {"speaker": "Bo", "text": "I sold a painting"}]},
        "qa": [
            {"question": "What is Ann's dog called?", "answer": "Rex", "category": 4},
            {"question": "When did Ann adopt Rex?", "answer": "May 2023", "category": 2},
            {"question": "What is Bo's dog called?", "adversarial_answer": "Rex", "category": 5}]}]"#;

    /// The whole run against a stub model server: ingestion saves through the store,
    /// the search step recalls, the answer sees the recalled memories, the judge grades,
    /// and the report and the debug log are written.
    #[tokio::test]
    async fn full_run_against_a_stub_model() {
        use crate::llm::prompts;
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(|req: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                let system = body["messages"][0]["content"].as_str().unwrap().to_owned();
                let user = body["messages"][1]["content"].as_str().unwrap().to_owned();
                let content = if system == prompts::INGEST_SYSTEM {
                    let turn = user.rsplit('\n').next().unwrap();
                    json!({"store_memory": [{"summary": turn, "entities": ["Ann"],
                        "topics": ["Pets"]}]})
                    .to_string()
                } else if system == prompts::SEARCH_SYSTEM {
                    json!({"queries": [{"query": "dog name"}], "entities": ["ann"]}).to_string()
                } else if system == prompts::FOLLOWUP_SYSTEM {
                    json!({"sufficient": true}).to_string()
                } else if system == prompts::ANSWER_SYSTEM {
                    "Rex".to_owned()
                } else {
                    json!({"label": "CORRECT"}).to_string()
                };
                ResponseTemplate::new(200).set_body_json(json!({
                    "choices": [{"message": {"role": "assistant", "content": content}}]
                }))
            })
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("locomo.json");
        std::fs::write(&data, DATA).unwrap();
        let client = |style| {
            LlmClient::new("stub", &server.uri(), None, style)
                .unwrap()
                .without_backoff()
        };
        let models = Models {
            answer: client(JudgeStyle::Recalld),
            ingest: client(JudgeStyle::Recalld),
            judge: client(JudgeStyle::Mem0),
        };
        let config = RunConfig {
            data,
            out_dir: dir.path().to_owned(),
            top_k: 25,
            full_text_count: 5,
            skip_adversarial: false,
            parallel: 1,
            qa_parallel: 2,
            json: true,
        };
        run(config, models, embedders()).await.unwrap();

        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("bench_results.json")).unwrap())
                .unwrap();
        assert_eq!(report["total_qa"], 3);
        assert_eq!(report["total_correct"], 3);
        assert_eq!(report["judge_style"], "mem0");
        assert_eq!(report["per_conversation"][0]["memories_stored"], 4);
        assert_eq!(report["categories"]["adversarial"]["total"], 1);

        let requests = server.received_requests().await.unwrap();
        let bodies: Vec<serde_json::Value> = requests
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        let answers: Vec<&str> = bodies
            .iter()
            .filter(|b| b["messages"][0]["content"] == prompts::ANSWER_SYSTEM)
            .map(|b| b["messages"][1]["content"].as_str().unwrap())
            .collect();
        assert_eq!(answers.len(), 3);
        // The entity filter `ann` matches every memory (saved as `Ann`); all four are shown.
        assert!(
            answers
                .iter()
                .all(|u| u.contains("Ann: I adopted a dog named Rex"))
        );
        assert!(
            answers.iter().all(|u| u.contains("Topics: pets")),
            "topics lower-cased"
        );
        let judged_by_mem0 = bodies
            .iter()
            .filter(|b| b["messages"][0]["content"] == prompts::JUDGE_MEM0_SYSTEM)
            .count();
        assert_eq!(
            judged_by_mem0, 2,
            "the adversarial question keeps recalld's judge"
        );

        let log = std::fs::read_to_string(dir.path().join("bench_debug.log")).unwrap();
        assert_eq!(log.matches("MemCtx: ").count(), 3);
        assert!(log.contains("\"memories\":4"));
    }
}
