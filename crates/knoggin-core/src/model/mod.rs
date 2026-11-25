// SPDX-License-Identifier: AGPL-3.0-only
//! The records a store holds: memories, namespaces and edges, with their limits and validation.

pub mod limits;
pub mod markup;
mod tag;

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::id::{MemoryId, NamespaceId};
use crate::time::Timestamp;

pub use tag::{Tag, TagError};

use limits::{
    DEFAULT_DIFFICULTY, DEFAULT_FULL_TO_SUMMARY, DEFAULT_GHOST_TO_DELETE,
    DEFAULT_INITIAL_STABILITY, DEFAULT_PERMASTORE_THRESHOLD, DEFAULT_SUMMARY_TO_GHOST,
    DIFFICULTY_MAX, DIFFICULTY_MIN, EMOTION_TERM_PREFIX, ENTITY_TERM_PREFIX, STABILITY_CEILING,
    STABILITY_FLOOR, TOPIC_TERM_PREFIX,
};

/// How much content a memory has left. It only moves down: Full, Summary, Ghost.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
    ts_rs::TS,
)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Summary and full text.
    Full,
    /// The full text has faded.
    Summary,
    /// Only the lifecycle numbers and the edges are left.
    Ghost,
}

/// Whether a memory has a vector.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS,
)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingState {
    /// The vector is in the vector table.
    Ready,
    /// Waiting in the embedding queue.
    Pending,
    /// Failed three times; queued again when the store is next opened.
    Failed,
    /// A ghost: it will never have a vector.
    None,
}

/// How a memory was accessed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS,
)]
#[serde(rename_all = "snake_case")]
pub enum AccessKind {
    /// An ordinary hit of a recall or a get.
    Direct,
    /// A hit found only by graph expansion or by similar search.
    Associative,
    /// A reinforcement.
    Reinforcement,
}

/// One entry of a memory's access history.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS,
)]
#[serde(rename_all = "camelCase")]
pub struct AccessEvent {
    /// When.
    pub at: Timestamp,
    /// How.
    pub kind: AccessKind,
}

/// The type of a directed edge. The numbers match recalld; 3 and 4 do not exist here.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
    ts_rs::TS,
)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum EdgeType {
    /// Source is the parent, target the child.
    ParentChild = 1,
    /// Semantic relation, made when a vector lands.
    Associative = 2,
    /// The two share a canonical entity.
    Entity = 5,
    /// Saved close together in time.
    Temporal = 6,
    /// Source replaces target.
    Supersedes = 7,
}

impl EdgeType {
    /// Every edge type, in number order.
    pub const ALL: [EdgeType; 5] = [
        Self::ParentChild,
        Self::Associative,
        Self::Entity,
        Self::Temporal,
        Self::Supersedes,
    ];

    /// The number recalld and the web UI's typed arrays use.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// The type with that number.
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::ParentChild),
            2 => Some(Self::Associative),
            5 => Some(Self::Entity),
            6 => Some(Self::Temporal),
            7 => Some(Self::Supersedes),
            _ => None,
        }
    }

    /// The name every JSON answer uses.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ParentChild => "parent_child",
            Self::Associative => "associative",
            Self::Entity => "entity",
            Self::Temporal => "temporal",
            Self::Supersedes => "supersedes",
        }
    }
}

/// A directed, weighted edge between two memories. Its key is `(source, edge_type, target)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    /// The parent, or the memory that replaces.
    pub source: MemoryId,
    /// The child, or the memory that is replaced.
    pub target: MemoryId,
    /// The type.
    pub edge_type: EdgeType,
    /// 0 to 1.
    pub weight: f32,
    /// True for automatic links.
    pub auto_created: bool,
    /// When it was made.
    pub created_at: Timestamp,
}

impl Edge {
    /// The other end of the edge, seen from `from`.
    pub fn other(&self, from: MemoryId) -> MemoryId {
        if self.source == from {
            self.target
        } else {
            self.source
        }
    }

    /// The edge's key.
    pub const fn key(&self) -> EdgeKey {
        EdgeKey {
            source: self.source,
            edge_type: self.edge_type,
            target: self.target,
        }
    }
}

/// The key of an edge. Ordered by source, type, target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EdgeKey {
    /// Source.
    pub source: MemoryId,
    /// Type.
    pub edge_type: EdgeType,
    /// Target.
    pub target: MemoryId,
}

/// Entities, topics and emotions, stored exactly as given.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Labels {
    /// Named people, places and things.
    pub entities: Vec<String>,
    /// Subjects.
    pub topics: Vec<String>,
    /// Feelings.
    pub emotions: Vec<String>,
}

impl fmt::Debug for Labels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Labels")
            .field("entities", &self.entities.len())
            .field("topics", &self.topics.len())
            .field("emotions", &self.emotions.len())
            .finish()
    }
}

/// One memory with its lifecycle numbers. Strength is never stored; it is computed
/// from `decay_anchor_at` and `stability` whenever it is read.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    /// Identity, made by the host.
    pub id: MemoryId,
    /// The namespace it belongs to.
    pub namespace: NamespaceId,
    /// When it was saved.
    pub created_at: Timestamp,
    /// The short text; empty only for a ghost.
    pub summary: String,
    /// The long text; `None` once it has faded.
    pub full_text: Option<String>,
    /// Free labels.
    pub tags: Vec<Tag>,
    /// Structured labels.
    pub labels: Labels,
    /// How much content is left.
    pub phase: Phase,
    /// Days; how slowly it fades.
    pub stability: f32,
    /// 1 to 10; input to the stability update.
    pub difficulty: f32,
    /// Exempt from the sweep. Set once, never cleared.
    pub is_permastore: bool,
    /// The time fading is measured from. Moves only on reinforcement.
    pub decay_anchor_at: Timestamp,
    /// The last recorded access.
    pub retrieved_at: Option<Timestamp>,
    /// The last reinforcement.
    pub reinforced_at: Option<Timestamp>,
    /// The last phase change; `created_at` for a new memory.
    pub phase_changed_at: Timestamp,
    /// Total recorded accesses.
    pub access_count: u64,
    /// The newest 32 accesses, oldest first.
    pub access_history: Vec<AccessEvent>,
    /// Whether it has a vector.
    pub embedding: EmbeddingState,
    /// Failed embedding attempts while `Pending`.
    pub embed_failures: u8,
    /// The credential that saved it: `api_key:<id>`, `oauth:<user>`, `session:<user>`,
    /// `local`, `none` or `import`.
    pub saved_by: String,
    /// The MCP client's name, when one was given.
    pub client: Option<String>,
}

impl Memory {
    /// The tag list that the full-text index and the document recipe use: the tags
    /// in order, then `entity/<lower>`, `topic/<lower>` and `emotion/<lower>` for
    /// each label, without repeats. recalld kept them as one list.
    pub fn tag_terms(&self) -> Vec<String> {
        tag_terms(&self.tags, &self.labels)
    }

    /// Bytes of all text: summary, full text, tags and labels.
    pub fn text_bytes(&self) -> u64 {
        let labels = self
            .labels
            .entities
            .iter()
            .chain(&self.labels.topics)
            .chain(&self.labels.emotions)
            .map(String::len);
        let tags = self.tags.iter().map(|t| t.as_str().len());
        let total: usize = self.summary.len()
            + self.full_text.as_ref().map_or(0, String::len)
            + tags.sum::<usize>()
            + labels.sum::<usize>();
        total as u64
    }
}

/// The tag terms of [`Memory::tag_terms`], for content that is not a memory yet.
pub fn tag_terms(tags: &[Tag], labels: &Labels) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |term: String| {
        if seen.insert(term.clone()) {
            out.push(term);
        }
    };
    for tag in tags {
        push(tag.as_str().to_owned());
    }
    for (prefix, items) in [
        (ENTITY_TERM_PREFIX, &labels.entities),
        (TOPIC_TERM_PREFIX, &labels.topics),
        (EMOTION_TERM_PREFIX, &labels.emotions),
    ] {
        for item in items {
            push(format!("{prefix}{}", item.to_lowercase()));
        }
    }
    out
}

impl fmt::Debug for Memory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Memory")
            .field("id", &self.id)
            .field("namespace", &self.namespace)
            .field("phase", &self.phase)
            .field("embedding", &self.embedding)
            .field("tags", &self.tags.len())
            .field("access_count", &self.access_count)
            .finish_non_exhaustive()
    }
}

/// The three phase thresholds: `0 < ghost_to_delete < summary_to_ghost < full_to_summary < 1`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PhaseThresholds {
    /// Effective strength at or below which the full text fades.
    pub full_to_summary: f32,
    /// Effective strength at or below which the memory becomes a ghost.
    pub summary_to_ghost: f32,
    /// Raw strength at or below which a ghost is deleted.
    pub ghost_to_delete: f32,
}

impl Default for PhaseThresholds {
    fn default() -> Self {
        Self {
            full_to_summary: DEFAULT_FULL_TO_SUMMARY,
            summary_to_ghost: DEFAULT_SUMMARY_TO_GHOST,
            ghost_to_delete: DEFAULT_GHOST_TO_DELETE,
        }
    }
}

/// A namespace's decay policy. Also the `policy` object of the API, where all five
/// keys are required and `decayRateMultiplier` may be `null`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NamespacePolicy {
    /// Stability of a new memory, 0.01 to 36,500 days.
    pub initial_stability: f32,
    /// Difficulty of a new memory, 1 to 10.
    pub default_difficulty: f32,
    /// When phases change.
    pub phase_thresholds: PhaseThresholds,
    /// Stability, in days, from which a memory is permanent. Greater than 0.
    pub permastore_threshold: f32,
    /// At least 0. `None` means 1.0; 0 turns decay off.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schema(required = true)]
    pub decay_rate_multiplier: Option<f32>,
}

impl Default for NamespacePolicy {
    fn default() -> Self {
        Self {
            initial_stability: DEFAULT_INITIAL_STABILITY,
            default_difficulty: DEFAULT_DIFFICULTY,
            phase_thresholds: PhaseThresholds::default(),
            permastore_threshold: DEFAULT_PERMASTORE_THRESHOLD,
            decay_rate_multiplier: None,
        }
    }
}

impl NamespacePolicy {
    /// The multiplier with `None` read as 1.0.
    pub fn multiplier(&self) -> f32 {
        self.decay_rate_multiplier.unwrap_or(1.0)
    }

    /// Checks every bound. Field names are those of the API's policy object.
    pub fn validate(&self) -> Result<(), EngineError> {
        let finite_in = |field: &'static str, value: f32, lo: f32, hi: f32| {
            if value.is_finite() && (lo..=hi).contains(&value) {
                Ok(())
            } else {
                Err(EngineError::invalid(
                    field,
                    format!("{field} must be between {lo} and {hi}"),
                ))
            }
        };
        finite_in(
            "initialStability",
            self.initial_stability,
            STABILITY_FLOOR,
            STABILITY_CEILING,
        )?;
        finite_in(
            "defaultDifficulty",
            self.default_difficulty,
            DIFFICULTY_MIN,
            DIFFICULTY_MAX,
        )?;
        let t = &self.phase_thresholds;
        let ordered = [t.ghost_to_delete, t.summary_to_ghost, t.full_to_summary]
            .iter()
            .all(|v| v.is_finite())
            && 0.0 < t.ghost_to_delete
            && t.ghost_to_delete < t.summary_to_ghost
            && t.summary_to_ghost < t.full_to_summary
            && t.full_to_summary < 1.0;
        if !ordered {
            return Err(EngineError::invalid(
                "phaseThresholds",
                "phaseThresholds must satisfy 0 < ghostToDelete < summaryToGhost < \
                 fullToSummary < 1",
            ));
        }
        if !(self.permastore_threshold.is_finite() && self.permastore_threshold > 0.0) {
            return Err(EngineError::invalid(
                "permastoreThreshold",
                "permastoreThreshold must be greater than 0",
            ));
        }
        if let Some(m) = self.decay_rate_multiplier
            && !(m.is_finite() && m >= 0.0)
        {
            return Err(EngineError::invalid(
                "decayRateMultiplier",
                "decayRateMultiplier must be at least 0",
            ));
        }
        Ok(())
    }
}

/// A namespace: a partition for search and for decay policy, not an access control.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Namespace {
    /// Sequential from 1, never reused.
    pub id: NamespaceId,
    /// 1 to 64 bytes of ASCII letters, digits, `-` and `_`; unique ignoring ASCII case.
    pub name: String,
    /// When it was made.
    pub created_at: Timestamp,
    /// Its decay policy.
    pub policy: NamespacePolicy,
}

impl fmt::Debug for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Namespace")
            .field("id", &self.id)
            .field("created_at", &self.created_at)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_types_keep_recalld_numbers_and_names() {
        for t in EdgeType::ALL {
            assert_eq!(EdgeType::from_u8(t.as_u8()), Some(t));
            assert_eq!(
                serde_json::to_string(&t).unwrap(),
                format!("\"{}\"", t.name())
            );
        }
        assert_eq!(EdgeType::from_u8(3), None);
        assert_eq!(EdgeType::from_u8(4), None);
        assert_eq!(EdgeType::Supersedes.as_u8(), 7);
    }

    #[test]
    fn phases_only_order_down() {
        assert!(Phase::Full < Phase::Summary && Phase::Summary < Phase::Ghost);
        assert_eq!(
            serde_json::to_string(&Phase::Summary).unwrap(),
            "\"summary\""
        );
    }

    #[test]
    fn tag_terms_merge_labels_without_repeats() {
        let tags = vec![
            Tag::new("travel").unwrap(),
            Tag::new("entity/maya").unwrap(),
        ];
        let labels = Labels {
            entities: vec!["Maya".into(), "Lisbon Trip".into()],
            topics: vec!["Seats".into()],
            emotions: vec!["happy".into(), "HAPPY".into()],
        };
        assert_eq!(
            tag_terms(&tags, &labels),
            [
                "travel",
                "entity/maya",
                "entity/lisbon trip",
                "topic/seats",
                "emotion/happy"
            ]
        );
    }

    #[test]
    fn policy_serializes_all_five_keys_and_requires_them() {
        let p = NamespacePolicy::default();
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"initialStability":3.7145,"defaultDifficulty":5.0,"phaseThresholds":{"fullToSummary":0.7,"summaryToGhost":0.3,"ghostToDelete":0.05},"permastoreThreshold":1500.0,"decayRateMultiplier":null}"#
        );
        let missing = r#"{"initialStability":3.7145,"defaultDifficulty":5.0,"phaseThresholds":{"fullToSummary":0.7,"summaryToGhost":0.3,"ghostToDelete":0.05},"permastoreThreshold":1500.0}"#;
        assert!(serde_json::from_str::<NamespacePolicy>(missing).is_err());
        let unknown = r#"{"initialStability":3.7145,"defaultDifficulty":5.0,"phaseThresholds":{"fullToSummary":0.7,"summaryToGhost":0.3,"ghostToDelete":0.05},"permastoreThreshold":1500.0,"decayRateMultiplier":2,"x":1}"#;
        assert!(serde_json::from_str::<NamespacePolicy>(unknown).is_err());
    }

    #[test]
    fn policy_bounds() {
        assert!(NamespacePolicy::default().validate().is_ok());
        let field = |p: NamespacePolicy| p.validate().unwrap_err().field().unwrap();
        let d = NamespacePolicy::default;
        let t = PhaseThresholds::default;
        let bad = [
            (
                NamespacePolicy {
                    initial_stability: 0.0,
                    ..d()
                },
                "initialStability",
            ),
            (
                NamespacePolicy {
                    default_difficulty: 11.0,
                    ..d()
                },
                "defaultDifficulty",
            ),
            (
                NamespacePolicy {
                    permastore_threshold: 0.0,
                    ..d()
                },
                "permastoreThreshold",
            ),
            (
                NamespacePolicy {
                    decay_rate_multiplier: Some(-1.0),
                    ..d()
                },
                "decayRateMultiplier",
            ),
            (
                NamespacePolicy {
                    phase_thresholds: PhaseThresholds {
                        ghost_to_delete: 0.3,
                        ..t()
                    },
                    ..d()
                },
                "phaseThresholds",
            ),
            (
                NamespacePolicy {
                    phase_thresholds: PhaseThresholds {
                        full_to_summary: f32::NAN,
                        ..t()
                    },
                    ..d()
                },
                "phaseThresholds",
            ),
        ];
        for (policy, name) in bad {
            assert_eq!(field(policy), name);
        }
        let off = NamespacePolicy {
            decay_rate_multiplier: Some(0.0),
            ..d()
        };
        assert!(off.validate().is_ok());
        assert_eq!(off.multiplier(), 0.0);
    }
}
