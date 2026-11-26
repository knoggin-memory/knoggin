// SPDX-License-Identifier: AGPL-3.0-only
//! The reads other than recall: get, similar, list, namespaces, tags, statistics and
//! the start-of-session context. None of them records access. With recording on, the
//! host calls `record_access` afterwards: for get with the memory as `direct` (unless
//! it is a ghost), for similar with the hits as `associative`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use uuid::Uuid;

use crate::error::EngineError;
use crate::id::MemoryId;
use crate::model::Phase;
use crate::model::limits::{
    CONTEXT_FULL_TEXT_COUNT, MAX_CONTEXT_LIMIT, MAX_PAGE_LIMIT, MAX_SIMILAR_LIMIT, validate_tags,
};
use crate::ops::{
    ContextResult, Detail, EmbeddingCounts, EmbeddingProgress, FindSimilar, GetContext, GetMemory,
    Items, ListMemories, ListTags, MemoryView, NamespaceCount, NamespaceStats,
    NamespaceStatsResult, NamespaceView, Page, PhaseCounts, Recall, RecallResponse, SimilarHit,
    SimilarResult, TagCount,
};
use crate::store::Store;
use crate::time::Timestamp;

fn not_found_memory(id: MemoryId) -> EngineError {
    EngineError::NotFound(format!("memory {id} not found"))
}

fn check_limit(field: &'static str, limit: u32, max: u32) -> Result<usize, EngineError> {
    if !(1..=max).contains(&limit) {
        return Err(EngineError::invalid(
            field,
            format!("{field} must be between 1 and {max}"),
        ));
    }
    Ok(limit as usize)
}

fn bad_cursor() -> EngineError {
    EngineError::invalid("cursor", "cursor is not one this server gave out")
}

/// A cursor is base64url text (tab "REST API"), so the tag it may name does not
/// travel in clear.
fn encode_cursor(plain: &str) -> String {
    URL_SAFE_NO_PAD.encode(plain)
}

fn decode_cursor(text: &str) -> Result<String, EngineError> {
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| bad_cursor())?;
    String::from_utf8(bytes).map_err(|_| bad_cursor())
}

/// The opaque cursor of a memory list: the last `(created_at, id)` seen.
fn memory_cursor(created_at: Timestamp, id: MemoryId) -> String {
    encode_cursor(&format!("{}.{id}", created_at.millis()))
}

fn parse_memory_cursor(text: &str) -> Result<(Timestamp, MemoryId), EngineError> {
    let text = decode_cursor(text)?;
    let (millis, id) = text.split_once('.').ok_or_else(bad_cursor)?;
    let millis: i64 = millis.parse().map_err(|_| bad_cursor())?;
    let id = Uuid::parse_str(id).map_err(|_| bad_cursor())?;
    Ok((Timestamp::from_millis(millis), MemoryId::from_uuid(id)))
}

/// The opaque cursor of a tag list: the last `(count, tag)` seen.
fn tag_cursor(count: u64, tag: &str) -> String {
    encode_cursor(&format!("{count}.{tag}"))
}

fn parse_tag_cursor(text: &str) -> Result<(u64, String), EngineError> {
    let text = decode_cursor(text)?;
    let (count, tag) = text.split_once('.').ok_or_else(bad_cursor)?;
    Ok((count.parse().map_err(|_| bad_cursor())?, tag.to_owned()))
}

/// The first `limit` of `items`, with the cursor of the last one when more follow.
fn page<T, C>(mut items: Vec<T>, limit: usize, cursor: impl Fn(&T) -> C) -> (Vec<T>, Option<C>) {
    let more = items.len() > limit;
    items.truncate(limit);
    let next = if more { items.last().map(cursor) } else { None };
    (items, next)
}

impl Store {
    /// One memory with `fullText`, its strength computed at `now`. A ghost comes
    /// without `summary`. A `namespace` in the request is only a check.
    pub fn get_memory(&self, req: &GetMemory, now: Timestamp) -> Result<MemoryView, EngineError> {
        let memory = self
            .memories
            .get(&req.id)
            .ok_or_else(|| not_found_memory(req.id))?;
        if let Some(name) = req.namespace.as_deref()
            && self.namespace_by_name(name).map(|n| n.id) != Some(memory.namespace)
        {
            return Err(not_found_memory(req.id));
        }
        Ok(self.memory_view(memory, now, true, req.include_history))
    }

    /// The memories nearest to one memory's own vector, in its namespace: exact top
    /// `limit` by cosine, without the memory itself, at least `minScore` when given.
    pub fn find_similar(
        &self,
        req: &FindSimilar,
        now: Timestamp,
    ) -> Result<SimilarResult, EngineError> {
        let limit = check_limit("limit", req.limit, MAX_SIMILAR_LIMIT)?;
        if let Some(min) = req.min_score
            && !(0.0..=1.0).contains(&min)
        {
            return Err(EngineError::invalid(
                "minScore",
                "minScore must be between 0 and 1",
            ));
        }
        let memory = self
            .memories
            .get(&req.id)
            .ok_or_else(|| not_found_memory(req.id))?;
        let vector = self
            .vectors
            .get(req.id)
            .ok_or_else(|| EngineError::Conflict("embedding pending".into()))?;
        let namespace = memory.namespace;
        let items = self
            .vectors
            .top_k(vector, limit, |id| {
                id != req.id
                    && self
                        .memories
                        .get(&id)
                        .is_some_and(|m| m.namespace == namespace && m.phase != Phase::Ghost)
            })
            .into_iter()
            .filter(|(_, score)| req.min_score.is_none_or(|min| *score >= min))
            .filter_map(|(id, score)| {
                let m = self.memories.get(&id)?;
                Some(SimilarHit {
                    memory: self.memory_view(m, now, false, false),
                    score,
                })
            })
            .collect();
        Ok(Items { items })
    }

    /// One page of a namespace's memories, newest first (`createdAt`, then ID, both
    /// descending). The cursor names the last memory seen, which need not exist any more.
    pub fn list_memories(
        &self,
        req: &ListMemories,
        now: Timestamp,
    ) -> Result<Page<MemoryView>, EngineError> {
        let namespace = self.resolve_namespace(req.namespace.as_deref())?.id;
        let tags = validate_tags("tags", &req.tags)?;
        let limit = check_limit("limit", req.limit, MAX_PAGE_LIMIT)?;
        let after = req.cursor.as_deref().map(parse_memory_cursor).transpose()?;
        // ponytail: a full scan and sort per page; an index by creation time if lists get slow.
        let mut found: Vec<_> = self
            .memories
            .values()
            .filter(|m| {
                m.namespace == namespace
                    && req.phase.is_none_or(|p| m.phase == p)
                    && req.created_after.is_none_or(|t| m.created_at > t)
                    && req.created_before.is_none_or(|t| m.created_at < t)
                    && tags.iter().all(|t| m.tags.contains(t))
                    && after.is_none_or(|key| (m.created_at, m.id) < key)
            })
            .collect();
        found.sort_by_key(|m| std::cmp::Reverse((m.created_at, m.id)));
        found.truncate(limit.saturating_add(1));
        let (found, next) = page(found, limit, |m| memory_cursor(m.created_at, m.id));
        Ok(Page {
            items: found
                .into_iter()
                .map(|m| self.memory_view(m, now, req.include_full_text, false))
                .collect(),
            next_cursor: next,
        })
    }

    /// Every namespace, by name.
    pub fn list_namespaces(&self) -> Vec<NamespaceView> {
        let mut out: Vec<NamespaceView> = self
            .namespaces
            .values()
            .map(|n| self.namespace_view(n))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// One page of a namespace's plain tags with their counts, by count descending,
    /// then tag. Labels are not counted, and a ghost has no tags.
    pub fn list_tags(&self, req: &ListTags) -> Result<Page<TagCount>, EngineError> {
        let namespace = self.resolve_namespace(req.namespace.as_deref())?.id;
        let limit = check_limit("limit", req.limit, MAX_PAGE_LIMIT)?;
        let after = req.cursor.as_deref().map(parse_tag_cursor).transpose()?;
        // Tags are lower-cased when made, so the prefix is too.
        let prefix = req.prefix.as_deref().unwrap_or("").to_lowercase();
        let mut counts: Vec<TagCount> = self
            .tags
            .counts(namespace)
            .filter(|(tag, _)| tag.as_str().starts_with(&prefix))
            .map(|(tag, count)| TagCount {
                tag: tag.as_str().to_owned(),
                count,
            })
            .filter(|t| {
                after.as_ref().is_none_or(|(count, tag)| {
                    t.count < *count || (t.count == *count && t.tag > *tag)
                })
            })
            .collect();
        counts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
        let (items, next) = page(counts, limit, |t| tag_cursor(t.count, &t.tag));
        Ok(Page {
            items,
            next_cursor: next,
        })
    }

    /// The statistics of one namespace; `averageStrength` is computed at `now`.
    pub fn namespace_stats(
        &self,
        req: &NamespaceStats,
        now: Timestamp,
    ) -> Result<NamespaceStatsResult, EngineError> {
        let ns = self.resolve_namespace(req.namespace.as_deref())?;
        let counts = self.namespace_counts(ns.id);
        let members = || self.memories.values().filter(|m| m.namespace == ns.id);
        let (mut sum, mut live) = (0.0_f64, 0_u64);
        for m in members().filter(|m| m.phase != Phase::Ghost) {
            sum += f64::from(self.raw_strength(m, now));
            live = live.saturating_add(1);
        }
        let in_ns = |id| self.memories.get(&id).is_some_and(|m| m.namespace == ns.id);
        let edge_count = self
            .graph
            .iter()
            .filter(|e| in_ns(e.source) && in_ns(e.target))
            .count() as u64;
        Ok(NamespaceStatsResult {
            name: ns.name.clone(),
            memory_count: counts.memories,
            phase_counts: PhaseCounts {
                full: counts.full,
                summary: counts.summary,
                ghost: counts.ghost,
            },
            permastore_count: counts.permastore,
            average_strength: if live == 0 {
                0.0
            } else {
                (sum / live as f64) as f32
            },
            edge_count,
            embedding: EmbeddingCounts {
                ready: counts.ready,
                pending: counts.pending,
                failed: counts.failed,
                none: counts.no_vector,
            },
            text_bytes: counts.text_bytes,
            oldest_created_at: members().map(|m| m.created_at).min(),
            newest_created_at: members().map(|m| m.created_at).max(),
            policy: ns.policy,
        })
    }

    /// The start-of-session call. With a `topic`, a recall for it (this call's limit,
    /// depth 0, no filters); without one, the namespace's newest memories that are not
    /// ghosts. The first 3 carry full text. `query_vector` is the embedded topic.
    pub fn context(
        &self,
        req: &GetContext,
        query_vector: Option<&[f32]>,
        now: Timestamp,
    ) -> Result<ContextResult, EngineError> {
        let limit = check_limit("limit", req.limit, MAX_CONTEXT_LIMIT)?;
        let ns = self.resolve_namespace(req.namespace.as_deref())?;
        let full_text_count = CONTEXT_FULL_TEXT_COUNT.min(limit);
        let topic = req.topic.as_deref().filter(|t| !t.trim().is_empty());
        let (memories, degraded) = match topic {
            Some(topic) => {
                let recall = Recall {
                    query: topic.to_owned(),
                    namespace: Some(ns.name.clone()),
                    limit: req.limit,
                    full_text_count: Some(full_text_count as u32),
                    tags: Vec::new(),
                    entities: Vec::new(),
                    topics: Vec::new(),
                    emotions: Vec::new(),
                    min_strength: None,
                    depth: 0,
                    time_range_start: None,
                    time_range_end: None,
                    detail: Detail::Compact,
                    record_access: false,
                };
                // The recall checks the topic as its query; the error names the
                // field that the client sent.
                let result =
                    self.recall(&recall, query_vector, false, now)
                        .map_err(|e| match e {
                            EngineError::InvalidArgument {
                                field: "query",
                                message,
                            } => EngineError::InvalidArgument {
                                field: "topic",
                                message: message.replacen("query", "topic", 1),
                            },
                            other => other,
                        })?;
                match result.response {
                    RecallResponse::Compact(c) => (c.memories, c.degraded),
                    RecallResponse::Full(_) => (Vec::new(), None),
                }
            }
            None => {
                let mut newest: Vec<_> = self
                    .memories
                    .values()
                    .filter(|m| m.namespace == ns.id && m.phase != Phase::Ghost)
                    .collect();
                newest.sort_by_key(|m| std::cmp::Reverse((m.created_at, m.id)));
                (
                    self.compact_hits(newest.iter().take(limit).map(|m| m.id), full_text_count),
                    None,
                )
            }
        };
        let summary = self.summary();
        Ok(ContextResult {
            default_namespace: ns.name.clone(),
            namespaces: self
                .list_namespaces()
                .into_iter()
                .map(|n| NamespaceCount {
                    name: n.name,
                    memory_count: n.memory_count,
                })
                .collect(),
            memories,
            embedding: EmbeddingProgress {
                pending: summary.pending_embeddings,
                total: summary.embeddable,
            },
            degraded,
        })
    }
}

#[cfg(test)]
mod tests;
