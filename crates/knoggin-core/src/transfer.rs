// SPDX-License-Identifier: AGPL-3.0-only
//! Export and import records: format `knoggin-export` version 1 (tab "Config, CLI,
//! local install", "Export file format"). One [`TransferRecord`] is one JSON line.
//!
//! An export holds everything except vectors and the idempotency table. An import
//! applies one record at a time: a record that fails a check is reported and the rest
//! goes on.

use std::fmt;
use std::ops::Bound;

use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::id::MemoryId;
use crate::model::limits::{
    ACCESS_HISTORY_MAX, ContentRef, DIFFICULTY_MAX, DIFFICULTY_MIN, STABILITY_CEILING,
    STABILITY_FLOOR, validate_content_within, validate_created_at, validate_namespace_name,
};
use crate::model::{
    AccessEvent, Edge, EdgeKey, EdgeType, EmbeddingState, Labels, Memory, Namespace,
    NamespacePolicy, Phase,
};
use crate::store::Store;
use crate::time::Timestamp;

/// The format name in the header.
pub const EXPORT_FORMAT: &str = "knoggin-export";
/// The format version in the header.
pub const EXPORT_VERSION: u32 = 1;
/// Who saved an imported memory: a file must not be able to say.
pub const IMPORT_SAVED_BY: &str = "import";

/// One line of an export file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransferRecord {
    /// The first line.
    Header(ExportHeader),
    /// A namespace with its policy.
    Namespace(NamespaceRecord),
    /// A memory without its vector.
    Memory(Box<MemoryRecord>),
    /// An edge.
    Edge(EdgeRecord),
}

/// The first line of an export file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportHeader {
    /// `knoggin-export`.
    pub format: String,
    /// 1.
    pub version: u32,
    /// When the export was made.
    #[serde(default)]
    pub exported_at: Option<Timestamp>,
    /// The model identity of the store; information only.
    #[serde(default)]
    pub model: Option<String>,
    /// Counts for people; an import does not check them.
    #[serde(default)]
    pub counts: Option<ExportCounts>,
}

impl ExportHeader {
    /// Refuses a header of another format or version.
    pub fn check(&self) -> Result<(), EngineError> {
        if self.format == EXPORT_FORMAT && self.version == EXPORT_VERSION {
            Ok(())
        } else {
            Err(EngineError::invalid(
                "header",
                format!("the first line must be a {EXPORT_FORMAT} version {EXPORT_VERSION} header"),
            ))
        }
    }
}

/// The header's counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportCounts {
    /// Namespaces.
    pub namespaces: u64,
    /// Memories.
    pub memories: u64,
    /// Edges.
    pub edges: u64,
}

/// A namespace line. Only `name` is required.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NamespaceRecord {
    /// Name.
    pub name: String,
    /// When it was made.
    #[serde(default)]
    pub created_at: Option<Timestamp>,
    /// Its decay policy; the default policy when absent.
    #[serde(default)]
    pub policy: Option<NamespacePolicy>,
}

impl fmt::Debug for NamespaceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NamespaceRecord")
            .field("created_at", &self.created_at)
            .finish_non_exhaustive()
    }
}

/// A memory line. `id`, `namespace`, `createdAt` and `phase` are required, and a
/// `summary` unless the phase is `ghost`; every other field takes the value of a newly
/// saved memory when absent or `null`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryRecord {
    /// Identity, kept.
    pub id: MemoryId,
    /// The namespace's name.
    pub namespace: String,
    /// When it was saved.
    pub created_at: Timestamp,
    /// `null` for a ghost.
    #[serde(default)]
    pub summary: Option<String>,
    /// The long text.
    #[serde(default)]
    pub full_text: Option<String>,
    /// Tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Entities.
    #[serde(default)]
    pub entities: Option<Vec<String>>,
    /// Topics.
    #[serde(default)]
    pub topics: Option<Vec<String>>,
    /// Emotions.
    #[serde(default)]
    pub emotions: Option<Vec<String>>,
    /// Phase.
    pub phase: Phase,
    /// Days.
    #[serde(default)]
    pub stability: Option<f32>,
    /// 1 to 10.
    #[serde(default)]
    pub difficulty: Option<f32>,
    /// Exempt from decay.
    #[serde(default)]
    pub is_permastore: Option<bool>,
    /// The time fading is measured from.
    #[serde(default)]
    pub decay_anchor_at: Option<Timestamp>,
    /// Last recorded access.
    #[serde(default)]
    pub retrieved_at: Option<Timestamp>,
    /// Last reinforcement.
    #[serde(default)]
    pub reinforced_at: Option<Timestamp>,
    /// Last phase change.
    #[serde(default)]
    pub phase_changed_at: Option<Timestamp>,
    /// Total recorded accesses.
    #[serde(default)]
    pub access_count: Option<u64>,
    /// Written on export; ignored on import.
    #[serde(default)]
    pub saved_by: Option<String>,
    /// Written on export; ignored on import.
    #[serde(default)]
    pub client: Option<String>,
    /// Oldest first, at most 32.
    #[serde(default)]
    pub access_history: Option<Vec<AccessEvent>>,
}

impl fmt::Debug for MemoryRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryRecord")
            .field("id", &self.id)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

/// An edge line. `source`, `target` and `edgeType` are required.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeRecord {
    /// Source.
    pub source: MemoryId,
    /// Target.
    pub target: MemoryId,
    /// Type.
    pub edge_type: EdgeType,
    /// 0 to 1; 1 when absent.
    #[serde(default)]
    pub weight: Option<f32>,
    /// False when absent.
    #[serde(default)]
    pub auto_created: Option<bool>,
    /// The source memory's `createdAt` when absent.
    #[serde(default)]
    pub created_at: Option<Timestamp>,
}

/// Where the next export page starts: after this memory, or after this edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportCursor {
    /// After the memory with this ID.
    Memory(MemoryId),
    /// After the edge with this key.
    Edge(EdgeKey),
}

/// One page of an export.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportPage {
    /// The lines, in file order.
    pub records: Vec<TransferRecord>,
    /// `None` on the last page.
    pub next: Option<ExportCursor>,
}

/// What an import does with a memory whose ID exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Count the line as skipped.
    #[default]
    Skip,
    /// Report the line as an error.
    Error,
}

/// What one imported line did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    /// The record changed the store.
    Imported,
    /// Nothing to do: it exists already, or it is an automatic semantic link.
    Skipped,
}

fn memory_record(memory: &Memory, namespace: String) -> MemoryRecord {
    let ghost = memory.phase == Phase::Ghost;
    MemoryRecord {
        id: memory.id,
        namespace,
        created_at: memory.created_at,
        summary: (!ghost).then(|| memory.summary.clone()),
        full_text: memory.full_text.clone(),
        tags: Some(memory.tags.iter().map(|t| t.as_str().to_owned()).collect()),
        entities: Some(memory.labels.entities.clone()),
        topics: Some(memory.labels.topics.clone()),
        emotions: Some(memory.labels.emotions.clone()),
        phase: memory.phase,
        stability: Some(memory.stability),
        difficulty: Some(memory.difficulty),
        is_permastore: Some(memory.is_permastore),
        decay_anchor_at: Some(memory.decay_anchor_at),
        retrieved_at: memory.retrieved_at,
        reinforced_at: memory.reinforced_at,
        phase_changed_at: Some(memory.phase_changed_at),
        access_count: Some(memory.access_count),
        saved_by: Some(memory.saved_by.clone()),
        client: memory.client.clone(),
        access_history: Some(memory.access_history.clone()),
    }
}

fn edge_record(edge: &Edge) -> EdgeRecord {
    EdgeRecord {
        source: edge.source,
        target: edge.target,
        edge_type: edge.edge_type,
        weight: Some(edge.weight),
        auto_created: Some(edge.auto_created),
        created_at: Some(edge.created_at),
    }
}

/// A time in an imported line: from 1970 to one day after `now`, as for a save.
fn check_time(
    field: &'static str,
    at: Option<Timestamp>,
    now: Timestamp,
) -> Result<(), EngineError> {
    match at {
        Some(at) => validate_created_at(at, now).map_err(|_| {
            EngineError::invalid(
                field,
                format!("{field} must be between 1970-01-01 and one day from now"),
            )
        }),
        None => Ok(()),
    }
}

fn check_range(field: &'static str, value: f32, lo: f32, hi: f32) -> Result<f32, EngineError> {
    if value.is_finite() && (lo..=hi).contains(&value) {
        Ok(value)
    } else {
        Err(EngineError::invalid(
            field,
            format!("{field} must be between {lo} and {hi}"),
        ))
    }
}

impl Store {
    /// One page of the export: the header and every namespace on the first page, then
    /// up to `max` memories and edges after `after`.
    pub fn export_page(
        &self,
        after: Option<ExportCursor>,
        max: usize,
        now: Timestamp,
    ) -> ExportPage {
        let max = max.max(1);
        let mut records = Vec::new();
        if after.is_none() {
            records.push(TransferRecord::Header(ExportHeader {
                format: EXPORT_FORMAT.to_owned(),
                version: EXPORT_VERSION,
                exported_at: Some(now),
                model: Some(self.model.id.clone()),
                counts: Some(ExportCounts {
                    namespaces: self.namespaces.len() as u64,
                    memories: self.memories.len() as u64,
                    edges: self.graph.len() as u64,
                }),
            }));
            let mut namespaces: Vec<&Namespace> = self.namespaces.values().collect();
            namespaces.sort_by(|a, b| a.name.cmp(&b.name));
            records.extend(namespaces.into_iter().map(|n| {
                TransferRecord::Namespace(NamespaceRecord {
                    name: n.name.clone(),
                    created_at: Some(n.created_at),
                    policy: Some(n.policy),
                })
            }));
        }
        let mut taken = 0_usize;
        let mut next = None;
        if !matches!(after, Some(ExportCursor::Edge(_))) {
            let start = match after {
                Some(ExportCursor::Memory(id)) => Bound::Excluded(id),
                _ => Bound::Unbounded,
            };
            for memory in self
                .memories
                .range((start, Bound::Unbounded))
                .map(|(_, m)| m)
            {
                if taken == max {
                    return ExportPage { records, next };
                }
                let namespace = self
                    .namespaces
                    .get(&memory.namespace)
                    .map(|n| n.name.clone())
                    .unwrap_or_default();
                records.push(TransferRecord::Memory(Box::new(memory_record(
                    memory, namespace,
                ))));
                taken += 1;
                next = Some(ExportCursor::Memory(memory.id));
            }
        }
        let edge_after = match after {
            Some(ExportCursor::Edge(key)) => Some(key),
            _ => None,
        };
        // ponytail: skips the edges before the cursor on every page; add a range
        // lookup to `Graph` if exports of large graphs get slow.
        for edge in self
            .graph
            .iter()
            .skip_while(|e| edge_after.is_some_and(|k| e.key() <= k))
        {
            if taken == max {
                return ExportPage { records, next };
            }
            records.push(TransferRecord::Edge(edge_record(edge)));
            taken += 1;
            next = Some(ExportCursor::Edge(edge.key()));
        }
        ExportPage {
            records,
            next: None,
        }
    }

    /// Applies one line of an import file (rules 2 to 10 of "Import rules"). The host
    /// checks the header with [`ExportHeader::check`]; a header here is an error line.
    pub fn import_record(
        &mut self,
        record: TransferRecord,
        on_conflict: OnConflict,
        now: Timestamp,
    ) -> Result<ImportOutcome, EngineError> {
        let outcome = match record {
            TransferRecord::Header(_) => Err(EngineError::invalid(
                "type",
                "a header is allowed only as the first line",
            )),
            TransferRecord::Namespace(r) => self.import_namespace(r, now),
            TransferRecord::Memory(r) => self.import_memory(*r, on_conflict, now),
            TransferRecord::Edge(r) => self.import_edge(r, now),
        }?;
        if outcome == ImportOutcome::Imported {
            self.changes.content = self.changes.content.saturating_add(1);
        }
        Ok(outcome)
    }

    fn import_namespace(
        &mut self,
        r: NamespaceRecord,
        now: Timestamp,
    ) -> Result<ImportOutcome, EngineError> {
        validate_namespace_name("name", &r.name)?;
        check_time("createdAt", r.created_at, now)?;
        let policy = r.policy.unwrap_or_default();
        policy.validate()?;
        if let Some(existing) = self.namespace_by_name(&r.name) {
            let id = existing.id;
            if self.namespace_counts(id).memories > 0 {
                return Ok(ImportOutcome::Skipped);
            }
            // The policy feeds no index, so it is set in place.
            if let Some(n) = self.namespaces.get_mut(&id) {
                n.policy = policy;
            }
            return Ok(ImportOutcome::Imported);
        }
        let id = self
            .allocate_namespace_id()
            .ok_or_else(|| EngineError::invalid("name", "no namespace IDs are left"))?;
        self.insert_namespace(Namespace {
            id,
            name: r.name,
            created_at: r.created_at.unwrap_or(now),
            policy,
        });
        Ok(ImportOutcome::Imported)
    }

    fn import_memory(
        &mut self,
        r: MemoryRecord,
        on_conflict: OnConflict,
        now: Timestamp,
    ) -> Result<ImportOutcome, EngineError> {
        // The phase rules first (rule 3 of "Import rules"), then the checks of a save in
        // the save's order, so a line reports the same first error as the request would.
        let ghost = r.phase == Phase::Ghost;
        match (ghost, &r.summary) {
            (false, None) => return Err(EngineError::invalid("summary", "summary is required")),
            (true, Some(_)) => {
                return Err(EngineError::invalid("summary", "a ghost has no summary"));
            }
            _ => {}
        }
        if r.full_text.is_some() && r.phase != Phase::Full {
            return Err(EngineError::invalid(
                "fullText",
                "only a memory in the phase 'full' has full text",
            ));
        }
        let raw_tags = r.tags.unwrap_or_default();
        let labels = Labels {
            entities: r.entities.unwrap_or_default(),
            topics: r.topics.unwrap_or_default(),
            emotions: r.emotions.unwrap_or_default(),
        };
        let summary = r.summary.unwrap_or_default();
        let tags = if ghost {
            if !raw_tags.is_empty() || labels != Labels::default() {
                return Err(EngineError::invalid(
                    "tags",
                    "a ghost has no tags or labels",
                ));
            }
            validate_created_at(r.created_at, now)?;
            Vec::new()
        } else {
            let content = ContentRef {
                summary: &summary,
                full_text: r.full_text.as_deref(),
                tags: &raw_tags,
                entities: &labels.entities,
                topics: &labels.topics,
                emotions: &labels.emotions,
                created_at: Some(r.created_at),
            };
            // Full text may be as long as the line limit allows.
            validate_content_within(content, now, usize::MAX)?
        };
        for (field, at) in [
            ("decayAnchorAt", r.decay_anchor_at),
            ("retrievedAt", r.retrieved_at),
            ("reinforcedAt", r.reinforced_at),
            ("phaseChangedAt", r.phase_changed_at),
        ] {
            check_time(field, at, now)?;
        }
        let history = r.access_history.unwrap_or_default();
        if history.len() > ACCESS_HISTORY_MAX {
            return Err(EngineError::invalid(
                "accessHistory",
                format!("accessHistory holds at most {ACCESS_HISTORY_MAX} events"),
            ));
        }
        for event in &history {
            check_time("accessHistory", Some(event.at), now)?;
        }
        let namespace = self.resolve_namespace(Some(&r.namespace))?.id;
        let policy = self
            .namespaces
            .get(&namespace)
            .map(|n| n.policy)
            .unwrap_or_default();
        let stability = check_range(
            "stability",
            r.stability.unwrap_or(policy.initial_stability),
            STABILITY_FLOOR,
            STABILITY_CEILING,
        )?;
        let difficulty = check_range(
            "difficulty",
            r.difficulty.unwrap_or(policy.default_difficulty),
            DIFFICULTY_MIN,
            DIFFICULTY_MAX,
        )?;
        if self.memories.contains_key(&r.id) {
            return match on_conflict {
                OnConflict::Skip => Ok(ImportOutcome::Skipped),
                OnConflict::Error => Err(EngineError::AlreadyExists(format!(
                    "memory {} already exists",
                    r.id
                ))),
            };
        }
        let memory = Memory {
            id: r.id,
            namespace,
            created_at: r.created_at,
            summary,
            full_text: r.full_text,
            tags,
            labels,
            phase: r.phase,
            stability,
            difficulty,
            is_permastore: r.is_permastore.unwrap_or(false),
            decay_anchor_at: r.decay_anchor_at.unwrap_or(r.created_at),
            retrieved_at: r.retrieved_at,
            reinforced_at: r.reinforced_at,
            phase_changed_at: r.phase_changed_at.unwrap_or(r.created_at),
            access_count: r.access_count.unwrap_or(0),
            access_history: history,
            embedding: if ghost {
                EmbeddingState::None
            } else {
                EmbeddingState::Pending
            },
            embed_failures: 0,
            saved_by: IMPORT_SAVED_BY.to_owned(),
            client: None,
        };
        if self.insert_memory(memory, None) {
            Ok(ImportOutcome::Imported)
        } else {
            Err(EngineError::Conflict(format!(
                "memory {} could not be imported",
                r.id
            )))
        }
    }

    fn import_edge(&mut self, r: EdgeRecord, now: Timestamp) -> Result<ImportOutcome, EngineError> {
        let auto_created = r.auto_created.unwrap_or(false);
        if auto_created && r.edge_type == EdgeType::Associative {
            return Ok(ImportOutcome::Skipped);
        }
        if r.source == r.target {
            return Err(EngineError::invalid(
                "target",
                "an edge must join two memories",
            ));
        }
        let weight = check_range("weight", r.weight.unwrap_or(1.0), 0.0, 1.0)?;
        check_time("createdAt", r.created_at, now)?;
        let source = self
            .memories
            .get(&r.source)
            .ok_or_else(|| EngineError::NotFound(format!("memory {} not found", r.source)))?;
        let created_at = r.created_at.unwrap_or(source.created_at);
        if !self.memories.contains_key(&r.target) {
            return Err(EngineError::NotFound(format!(
                "memory {} not found",
                r.target
            )));
        }
        let edge = Edge {
            source: r.source,
            target: r.target,
            edge_type: r.edge_type,
            weight,
            auto_created,
            created_at,
        };
        Ok(if self.insert_edge(edge) {
            ImportOutcome::Imported
        } else {
            ImportOutcome::Skipped
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ErrorCode;
    use crate::store::write::tests::{
        DAY, T0, assert_matches_rebuild, ctx, mid, namespace, req, store, t, vector,
    };

    fn export_all(s: &Store, max: usize) -> Vec<TransferRecord> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = s.export_page(after, max, t(T0));
            out.extend(page.records);
            match page.next {
                Some(c) => after = Some(c),
                None => return out,
            }
        }
    }

    fn sample() -> Store {
        let mut s = store();
        namespace(&mut s, "work");
        let text = "maya likes window seats";
        s.store_memory(
            req(json!({ "summary": text, "fullText": "on the Lisbon trip", "tags": ["travel"], "entities": ["Maya"] })),
            mid(1),
            Some(vector(text)),
            &ctx(),
            t(T0),
        )
        .unwrap();
        s.store_memory(
            req(json!({ "summary": text, "entities": ["Maya"], "parentId": mid(1) })),
            mid(2),
            Some(vector(text)),
            &ctx(),
            t(T0),
        )
        .unwrap();
        s.store_memory(
            req(json!({ "summary": "work note", "namespace": "work" })),
            mid(3),
            None,
            &ctx(),
            t(T0),
        )
        .unwrap();
        s.store_memory(
            req(json!({ "summary": "to fade" })),
            mid(4),
            None,
            &ctx(),
            t(T0),
        )
        .unwrap();
        s.fade_to_ghost(mid(4), t(T0 + DAY));
        s.record_access(
            &[crate::ops::AccessHit {
                id: mid(1),
                kind: crate::model::AccessKind::Direct,
            }],
            t(T0 + 5),
        );
        s
    }

    #[test]
    fn export_is_ordered_and_pages_by_cursor() {
        let s = sample();
        let all = export_all(&s, 1_000);
        let kinds: Vec<&str> = all
            .iter()
            .map(|r| match r {
                TransferRecord::Header(_) => "h",
                TransferRecord::Namespace(_) => "n",
                TransferRecord::Memory(_) => "m",
                TransferRecord::Edge(_) => "e",
            })
            .collect();
        let edges = s.graph().len();
        let mut expected = vec!["h", "n", "n", "m", "m", "m", "m"];
        expected.extend(std::iter::repeat_n("e", edges));
        assert_eq!(kinds, expected);
        assert_eq!(export_all(&s, 1), all, "pages of one give the same lines");
        assert_eq!(export_all(&s, 3), all);
        let TransferRecord::Header(h) = &all[0] else {
            panic!()
        };
        assert_eq!(
            (h.format.as_str(), h.version, h.model.as_deref()),
            ("knoggin-export", 1, Some("fake-64"))
        );
        assert_eq!(h.counts.unwrap().memories, 4);
        let line = serde_json::to_value(&all[3]).unwrap();
        assert_eq!(line["type"], "memory");
        assert_eq!(line["retrievedAt"], "2023-11-14T22:13:20.005Z");
        assert_eq!(
            line["reinforcedAt"],
            serde_json::Value::Null,
            "every field is written"
        );
        assert_eq!(line["accessHistory"][0]["kind"], "direct");
        let ghost = serde_json::to_value(&all[6]).unwrap();
        assert_eq!(ghost["summary"], serde_json::Value::Null);
        let edge = serde_json::to_value(all.last().unwrap()).unwrap();
        assert!(edge["edgeType"].is_string());
    }

    #[test]
    fn export_then_import_round_trips() {
        let s = sample();
        let lines: Vec<String> = export_all(&s, 2)
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        let mut fresh = store();
        let header: TransferRecord = serde_json::from_str(&lines[0]).unwrap();
        let TransferRecord::Header(h) = header else {
            panic!()
        };
        h.check().unwrap();
        let mut skipped = 0;
        for line in &lines[1..] {
            let record: TransferRecord = serde_json::from_str(line).unwrap();
            if fresh
                .import_record(record, OnConflict::Skip, t(T0 + DAY))
                .unwrap()
                == ImportOutcome::Skipped
            {
                skipped += 1;
            }
        }
        let auto_semantic = s
            .graph()
            .iter()
            .filter(|e| e.auto_created && e.edge_type == EdgeType::Associative)
            .count();
        assert!(auto_semantic > 0);
        assert_eq!(skipped, auto_semantic);
        let normalize = |records: Vec<TransferRecord>| -> Vec<TransferRecord> {
            records
                .into_iter()
                .filter_map(|r| match r {
                    TransferRecord::Header(_) => None,
                    TransferRecord::Memory(mut m) => {
                        m.saved_by = Some(IMPORT_SAVED_BY.into());
                        m.client = None;
                        Some(TransferRecord::Memory(m))
                    }
                    TransferRecord::Edge(e)
                        if e.auto_created == Some(true) && e.edge_type == EdgeType::Associative =>
                    {
                        None
                    }
                    other => Some(other),
                })
                .collect()
        };
        assert_eq!(
            normalize(export_all(&fresh, 1_000)),
            normalize(export_all(&s, 1_000))
        );
        let m = fresh.memory(mid(1)).unwrap();
        assert_eq!(
            (m.embedding, m.saved_by.as_str(), m.client.as_deref()),
            (EmbeddingState::Pending, "import", None)
        );
        assert_eq!(
            fresh.memory(mid(4)).unwrap().embedding,
            EmbeddingState::None
        );
        assert_eq!(fresh.changes().content, (lines.len() - 1 - skipped) as u64);
        // The same file again changes nothing.
        let content = fresh.changes().content;
        for line in &lines[1..] {
            let record: TransferRecord = serde_json::from_str(line).unwrap();
            fresh
                .import_record(record, OnConflict::Skip, t(T0 + DAY))
                .unwrap();
        }
        assert_eq!(fresh.changes().content, content);
        assert_matches_rebuild(&mut fresh);
    }

    fn import(s: &mut Store, v: serde_json::Value) -> Result<ImportOutcome, EngineError> {
        s.import_record(serde_json::from_value(v).unwrap(), OnConflict::Skip, t(T0))
    }

    fn memory_line(extra: serde_json::Value) -> serde_json::Value {
        let mut v = json!({ "type": "memory", "id": mid(1), "namespace": "default", "createdAt": "2023-01-01T00:00:00Z", "phase": "full", "summary": "s" });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }

    #[test]
    fn memory_lines_are_checked_like_requests() {
        let mut s = store();
        let field = |s: &mut Store, extra| import(s, memory_line(extra)).unwrap_err().field();
        assert_eq!(field(&mut s, json!({ "summary": null })), Some("summary"));
        assert_eq!(field(&mut s, json!({ "summary": "" })), Some("summary"));
        assert_eq!(
            field(&mut s, json!({ "phase": "summary", "fullText": "f" })),
            Some("fullText")
        );
        assert_eq!(field(&mut s, json!({ "phase": "ghost" })), Some("summary"));
        assert_eq!(
            field(
                &mut s,
                json!({ "phase": "ghost", "summary": null, "tags": ["a"] })
            ),
            Some("tags")
        );
        assert_eq!(field(&mut s, json!({ "tags": ["Bad Tag"] })), Some("tags"));
        // The first error of a save with the same content: the summary before the tags.
        assert_eq!(
            field(&mut s, json!({ "summary": "", "tags": ["Bad Tag"] })),
            Some("summary")
        );
        assert_eq!(
            field(&mut s, json!({ "createdAt": "2999-01-01T00:00:00Z" })),
            Some("createdAt")
        );
        assert_eq!(
            field(&mut s, json!({ "stability": 0.0 })),
            Some("stability")
        );
        assert_eq!(
            field(&mut s, json!({ "difficulty": 11.0 })),
            Some("difficulty")
        );
        let err = import(&mut s, memory_line(json!({ "namespace": "nope" }))).unwrap_err();
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(
            serde_json::from_value::<TransferRecord>(memory_line(json!({ "extra": 1 }))).is_err(),
            "unknown fields"
        );
        assert!(
            serde_json::from_value::<TransferRecord>(json!({ "type": "memory", "id": mid(1) }))
                .is_err()
        );
        assert_eq!(s.memories().count(), 0);
        assert_eq!(s.changes().content, 0);
        // Full text may be longer than a save allows; absent fields take a new memory's values.
        let long = "x".repeat(100_000);
        assert_eq!(
            import(
                &mut s,
                memory_line(json!({ "fullText": long, "savedBy": "api_key:x", "client": "evil" }))
            )
            .unwrap(),
            ImportOutcome::Imported
        );
        let m = s.memory(mid(1)).unwrap();
        assert_eq!(
            (m.stability, m.difficulty, m.decay_anchor_at, m.access_count),
            (3.7145, 5.0, m.created_at, 0)
        );
        assert_eq!((m.saved_by.as_str(), m.client.as_deref()), ("import", None));
        assert_eq!(
            import(&mut s, memory_line(json!({}))).unwrap(),
            ImportOutcome::Skipped
        );
        let err = s
            .import_record(
                serde_json::from_value(memory_line(json!({}))).unwrap(),
                OnConflict::Error,
                t(T0),
            )
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::AlreadyExists);
        assert_eq!(s.changes().content, 1);
    }

    #[test]
    fn namespace_lines_keep_a_used_policy_and_fill_an_empty_one() {
        let mut s = store();
        let policy = json!({ "initialStability": 9.0, "defaultDifficulty": 5.0, "phaseThresholds": { "fullToSummary": 0.7, "summaryToGhost": 0.3, "ghostToDelete": 0.05 }, "permastoreThreshold": 1500.0, "decayRateMultiplier": null });
        assert_eq!(
            import(
                &mut s,
                json!({ "type": "namespace", "name": "default", "policy": policy })
            )
            .unwrap(),
            ImportOutcome::Imported
        );
        assert_eq!(
            s.namespace_by_name("default")
                .unwrap()
                .policy
                .initial_stability,
            9.0
        );
        assert_eq!(
            import(&mut s, json!({ "type": "namespace", "name": "work" })).unwrap(),
            ImportOutcome::Imported
        );
        assert_eq!(s.namespace_by_name("work").unwrap().created_at, t(T0));
        s.store_memory(
            req(json!({ "summary": "a", "namespace": "work" })),
            mid(1),
            None,
            &ctx(),
            t(T0),
        )
        .unwrap();
        assert_eq!(
            import(
                &mut s,
                json!({ "type": "namespace", "name": "WORK", "policy": policy })
            )
            .unwrap(),
            ImportOutcome::Skipped
        );
        assert_eq!(
            s.namespace_by_name("work")
                .unwrap()
                .policy
                .initial_stability,
            3.7145
        );
        assert_eq!(
            import(&mut s, json!({ "type": "namespace", "name": "a b" }))
                .unwrap_err()
                .field(),
            Some("name")
        );
    }

    #[test]
    fn edge_lines_need_both_ends_and_skip_automatic_semantic_links() {
        let mut s = store();
        s.store_memory(req(json!({ "summary": "a" })), mid(1), None, &ctx(), t(T0))
            .unwrap();
        s.store_memory(
            req(json!({ "summary": "b" })),
            mid(2),
            None,
            &ctx(),
            t(T0 + 10 * DAY),
        )
        .unwrap();
        let edge = |source, target, ty: &str, auto| json!({ "type": "edge", "source": mid(source), "target": mid(target), "edgeType": ty, "autoCreated": auto });
        assert_eq!(
            import(&mut s, edge(1, 9, "associative", true)).unwrap(),
            ImportOutcome::Skipped
        );
        assert_eq!(
            import(&mut s, edge(1, 9, "entity", true))
                .unwrap_err()
                .code(),
            ErrorCode::NotFound
        );
        assert_eq!(
            import(&mut s, edge(1, 1, "entity", true))
                .unwrap_err()
                .code(),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            import(&mut s, edge(2, 1, "parent_child", false)).unwrap(),
            ImportOutcome::Imported
        );
        let e = s.graph().iter().next().unwrap();
        assert_eq!(
            (e.weight, e.created_at),
            (1.0, t(T0 + 10 * DAY)),
            "defaults: weight 1, the source's time"
        );
        assert_eq!(
            import(&mut s, edge(2, 1, "parent_child", false)).unwrap(),
            ImportOutcome::Skipped
        );
        let mut heavy = edge(1, 2, "entity", false);
        heavy["weight"] = json!(1.5);
        assert_eq!(import(&mut s, heavy).unwrap_err().field(), Some("weight"));
        let header = json!({ "type": "header", "format": "knoggin-export", "version": 1 });
        assert!(
            import(&mut s, header).is_err(),
            "a header past the first line"
        );
        let TransferRecord::Header(bad) = serde_json::from_value(
            json!({ "type": "header", "format": "knoggin-export", "version": 2, "model": null }),
        )
        .unwrap() else {
            panic!()
        };
        assert!(bad.check().is_err());
    }
}
