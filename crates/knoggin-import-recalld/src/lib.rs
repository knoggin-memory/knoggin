// SPDX-License-Identifier: AGPL-3.0-only
//! Reads a stopped recalld 2.x data directory and yields
//! [`knoggin_core::transfer::TransferRecord`] lines in export-file order, for `knoggin
//! import recalld`.
//!
//! Before reading, three steps: 1. find the store, and with it the lock files; 2. take a
//! shared lock on each lock file that exists; 3. check the row `state` with
//! `controlSchemaVersion` 2.
//!
//! The recalld directory is never changed. `meta.db` and `edges.db` are redb 2 files,
//! which redb cannot open read-only, so they are copied into a private folder under the
//! run directory that the caller names (`~/.knoggin/run/`), opened there, and the folder
//! is deleted before [`read`] returns, after a success and after a failure. The text file
//! is read where it is.
//!
//! Everything is read into memory before the copies go, so plaintext stays on disk for
//! the shortest time. ponytail: the whole store is held in memory, as the daemon holds it
//! anyway; stream the memories in a second pass if stores outgrow that.

mod files;
mod record;

/// The recalld record decoder, opened to `fuzz/`. Not an API.
#[doc(hidden)]
pub mod fuzzing {
    /// Decodes `data` as a value of recalld's `memories` table, and as an edge whose
    /// key is the first 33 bytes and whose value is the rest.
    pub fn decode(data: &[u8]) {
        let _ = super::record::DiskRecord::from_bytes(data);
        let (key, value) = data.split_at(data.len().min(33));
        let _ = super::edge_record(key, value);
    }
}

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use knoggin_core::model::limits::{
    ACCESS_HISTORY_MAX, EMOTION_TERM_PREFIX, ENTITY_TERM_PREFIX, NAMESPACE_NAME_MAX_BYTES,
    TOPIC_TERM_PREFIX,
};
use knoggin_core::model::{
    AccessEvent, AccessKind, EdgeType, NamespacePolicy, Phase, PhaseThresholds,
};
use knoggin_core::transfer::{
    EXPORT_FORMAT, EXPORT_VERSION, EdgeRecord, ExportCounts, ExportHeader, MemoryRecord,
    NamespaceRecord, TransferRecord,
};
use knoggin_core::{MemoryId, Timestamp};
use redb::{ReadableTable, TableDefinition, TableError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::files::{Copies, TextFile};
use crate::record::{DiskRecord, RecalldAccess, RecalldPhase};

const CONTROL: TableDefinition<&str, &[u8]> = TableDefinition::new("canonical_control");
const NAMESPACES: TableDefinition<u32, &[u8]> = TableDefinition::new("namespaces");
const MEMORIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("memories");
const TEXT_GENERATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("text_generations_v1");
const EDGES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("edges_forward");

/// Why a recalld directory cannot be imported at all.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// A container volume whose layout journal is not ready.
    #[error(
        "This recalld volume is in the middle of a layout change. Start recalld once, stop it, and try again"
    )]
    LayoutChanging,
    /// A lock file is held.
    #[error("Stop recalld first: recalld daemon stop")]
    Running,
    /// Not recalld's current layout, or a text file in format 1.
    #[error("Upgrade recalld to 2.2 and start it once, then stop it and try again")]
    TooOld,
    /// The store has no active text file.
    #[error(
        "This recalld directory has no active text file. Start recalld 2.2 once, stop it, and try again"
    )]
    NoActiveText,
    /// A file or folder that must be there is not.
    #[error("{} is missing; is this a recalld data directory?", .0.display())]
    Missing(PathBuf),
    /// A symbolic link where a file or folder was expected.
    #[error("{} is a symbolic link; the importer does not follow links", .0.display())]
    SymbolicLink(PathBuf),
    /// Not a folder.
    #[error("{} is not a directory", .0.display())]
    NotADirectory(PathBuf),
    /// Not a regular file.
    #[error("{} is not a regular file", .0.display())]
    NotAFile(PathBuf),
    /// The run directory is in or beside the recalld directory.
    #[error("the import's temporary folder must not be in or beside the recalld directory")]
    RunDirInside,
    /// The store's own structure is damaged.
    #[error("the recalld data is damaged: {0}")]
    Corrupt(&'static str),
    /// A redb file could not be read.
    #[error("a recalld database could not be read: {0}")]
    Database(String),
    /// A file could not be read or the copies could not be made.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn db_error(e: impl fmt::Display) -> ImportError {
    ImportError::Database(e.to_string())
}

/// A recalld record that is left out, with the reason. Neither field holds memory text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRecord {
    /// `memories` or `edges_forward`.
    pub table: &'static str,
    /// The memory ID, or `source/type/target` of an edge, or the key in hex.
    pub key: String,
    /// Why.
    pub reason: &'static str,
}

/// Memories by phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PhaseCounts {
    /// `full`.
    pub full: u64,
    /// `summary`.
    pub summary: u64,
    /// `ghost`.
    pub ghost: u64,
}

/// A namespace that took a suffix because its name differs from an earlier one only in
/// ASCII case.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Renamed {
    /// recalld's name.
    pub from: String,
    /// Knoggin's.
    pub to: String,
}

impl fmt::Debug for Renamed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Renamed { .. }")
    }
}

/// The counts of `knoggin import recalld --json`, in its shape. The reader fills every
/// field; the command replaces `edges` by the number of edge lines the import route
/// accepted and adds the route's error lines to `errors`.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    /// Namespace lines.
    pub namespaces: u64,
    /// Memory lines by phase.
    pub memories: PhaseCounts,
    /// Forgotten memories (recalld's phase 4), not imported.
    pub forgotten_skipped: u64,
    /// Edge lines.
    pub edges: u64,
    /// Edges left out: types 3 and 4, automatic associative edges, and edges with a
    /// forgotten end (recalld keeps a forgotten memory's edges).
    pub edges_skipped: u64,
    /// Memory lines that are not ghosts: they wait for embedding once imported.
    pub pending_embedding: u64,
    /// Namespaces that took a suffix.
    pub renamed: Vec<Renamed>,
    /// The reader's own error lines ([`Import::skipped`]).
    pub errors: u64,
}

impl fmt::Debug for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Summary")
            .field("namespaces", &self.namespaces)
            .field("memories", &self.memories)
            .field("forgotten_skipped", &self.forgotten_skipped)
            .field("edges", &self.edges)
            .field("edges_skipped", &self.edges_skipped)
            .field("pending_embedding", &self.pending_embedding)
            .field("renamed", &self.renamed.len())
            .field("errors", &self.errors)
            .finish()
    }
}

/// What [`read`] found.
#[derive(Debug, Clone)]
pub struct Import {
    /// The lines in file order: the header, namespaces by name, memories by ID, edges by
    /// `(source, edgeType, target)`.
    pub records: Vec<TransferRecord>,
    /// The counts.
    pub summary: Summary,
    /// Records that are not in `records`, with the reason.
    pub skipped: Vec<SkippedRecord>,
}

/// Reads the stopped recalld directory `data_dir`. The copies go into a new folder in
/// `run_dir`, which is created with mode 0700 if missing and must not be in or beside
/// the recalld directory. `now` becomes the header's `exportedAt`.
pub fn read(data_dir: &Path, run_dir: &Path, now: Timestamp) -> Result<Import, ImportError> {
    let located = files::locate(data_dir)?;
    let _locks = files::hold_locks(&located.locks)?;
    let copies = Copies::create(run_dir, &located)?;
    read_store(&located.store, &copies, now)
    // `copies` drops here: the folder goes, whatever the result.
}

/// What `meta.db` holds, read out of the copy.
struct Meta {
    store_id: Uuid,
    namespaces: Vec<(u32, RecalldNamespace)>,
    memories: Vec<(Vec<u8>, Vec<u8>)>,
    text: Option<(Uuid, String, u16)>,
}

fn read_store(store: &Path, copies: &Copies, now: Timestamp) -> Result<Import, ImportError> {
    let meta_copy = copies.copy(&store.join("meta.db"))?;
    let edges_copy = copies.copy(&store.join("edges.db"))?;
    let meta = read_meta(&meta_copy)?;
    let raw_edges = read_edges(&edges_copy)?;
    let (generation, file_name, format) = meta.text.clone().ok_or(ImportError::NoActiveText)?;
    if format != 2 {
        return Err(ImportError::TooOld);
    }
    let text = TextFile::open(&store.join(file_name), meta.store_id, generation)?;
    Ok(build(meta, raw_edges, &text, now))
}

fn open_db(path: &Path) -> Result<redb::Database, ImportError> {
    redb::Database::open(path).map_err(db_error)
}

/// Rows of a table, or none when the table does not exist.
fn rows<K: redb::Key + 'static, V: redb::Value + 'static, T>(
    tx: &redb::ReadTransaction,
    def: TableDefinition<K, V>,
    mut each: impl FnMut(K::SelfType<'_>, V::SelfType<'_>) -> Result<T, ImportError>,
) -> Result<Vec<T>, ImportError> {
    let table = match tx.open_table(def) {
        Ok(t) => t,
        Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(db_error(e)),
    };
    let mut out = Vec::new();
    for row in table.iter().map_err(db_error)? {
        let (k, v) = row.map_err(db_error)?;
        out.push(each(k.value(), v.value())?);
    }
    Ok(out)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ControlState {
    control_schema_version: u32,
    store_id: Uuid,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextGeneration {
    id: Uuid,
    filename: String,
    format_version: u16,
}

/// The fields of recalld's `NamespaceConfig` that the import takes. Every other field
/// is ignored, including `desiredRetention`, `requiredFeatures` and
/// `vectorCertification`.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecalldNamespace {
    name: String,
    initial_stability: f32,
    default_difficulty: f32,
    phase_thresholds: RecalldThresholds,
    permastore_threshold: f32,
    created_at: i64,
    #[serde(default)]
    decay_rate_multiplier: Option<f32>,
    #[serde(default)]
    schema: Option<NamespaceSchema>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecalldThresholds {
    full_to_summary: f32,
    summary_to_ghost: f32,
    ghost_to_delete: f32,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NamespaceSchema {
    #[serde(default)]
    inherited_decay: Option<InheritedDecay>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InheritedDecay {
    multiplier_bits: u32,
}

fn read_meta(path: &Path) -> Result<Meta, ImportError> {
    let db = open_db(path)?;
    let tx = db.begin_read().map_err(db_error)?;
    // Step 3 before reading: the row `state` with `controlSchemaVersion` 2.
    let control = match tx.open_table(CONTROL) {
        Ok(table) => table
            .get("state")
            .map_err(db_error)?
            .and_then(|v| serde_json::from_slice::<ControlState>(v.value()).ok()),
        Err(TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(db_error(e)),
    };
    let store_id = match control {
        Some(c) if c.control_schema_version == 2 => c.store_id,
        _ => return Err(ImportError::TooOld),
    };
    let namespaces = rows(&tx, NAMESPACES, |id, value| {
        serde_json::from_slice::<RecalldNamespace>(value)
            .map(|n| (id, n))
            .map_err(|_| ImportError::Corrupt("a namespace record does not decode"))
    })?;
    let memories = rows(&tx, MEMORIES, |k, v| Ok((k.to_vec(), v.to_vec())))?;
    let generations = rows(&tx, TEXT_GENERATIONS, |k, v| Ok((k.to_owned(), v.to_vec())))?;
    let text = active_text(&generations)?;
    Ok(Meta {
        store_id,
        namespaces,
        memories,
        text,
    })
}

/// The row `active` names a generation; that generation's row names the file.
fn active_text(rows: &[(String, Vec<u8>)]) -> Result<Option<(Uuid, String, u16)>, ImportError> {
    let Some((_, active)) = rows.iter().find(|(k, _)| k == "active") else {
        return Ok(None);
    };
    let active: Uuid = serde_json::from_slice(active)
        .map_err(|_| ImportError::Corrupt("the active text generation does not decode"))?;
    let Some((_, row)) = rows.iter().find(|(k, _)| *k == active.to_string()) else {
        return Ok(None);
    };
    let generation: TextGeneration = serde_json::from_slice(row)
        .map_err(|_| ImportError::Corrupt("a text generation does not decode"))?;
    // The name comes from the database: accept only `fulltext.<uuid>.dat` of this
    // generation, so it cannot point anywhere else.
    let middle = generation
        .filename
        .strip_prefix("fulltext.")
        .and_then(|s| s.strip_suffix(".dat"))
        .and_then(|s| Uuid::parse_str(s).ok());
    if generation.id != active
        || middle != Some(active)
        || generation.filename != format!("fulltext.{active}.dat")
    {
        return Err(ImportError::Corrupt(
            "the active text file name is not valid",
        ));
    }
    Ok(Some((
        active,
        generation.filename,
        generation.format_version,
    )))
}

/// One raw row of `edges_forward`.
type RawEdge = (Vec<u8>, Vec<u8>);

fn read_edges(path: &Path) -> Result<Vec<RawEdge>, ImportError> {
    let db = open_db(path)?;
    let tx = db.begin_read().map_err(db_error)?;
    rows(&tx, EDGES, |k, v| Ok((k.to_vec(), v.to_vec())))
}

fn time(ms: i64) -> Result<Timestamp, &'static str> {
    Timestamp::from_api_millis(ms).map_err(|_| "a time is outside the range Knoggin accepts")
}

/// Knoggin's names: recalld compares names byte for byte, Knoggin ignores ASCII case.
/// The second of two names that differ only in case gets `-2`, then `-3`, cut so that
/// the result fits in 64 bytes.
fn knoggin_names(namespaces: &[(u32, RecalldNamespace)]) -> (HashMap<u32, String>, Vec<Renamed>) {
    let mut taken = HashSet::new();
    let mut names = HashMap::new();
    let mut renamed = Vec::new();
    for (id, ns) in namespaces {
        let mut name = ns.name.clone();
        let mut n = 2_u32;
        while !taken.insert(name.to_ascii_lowercase()) {
            let suffix = format!("-{n}");
            let mut cut = NAMESPACE_NAME_MAX_BYTES
                .saturating_sub(suffix.len())
                .min(ns.name.len());
            while !ns.name.is_char_boundary(cut) {
                cut = cut.saturating_sub(1);
            }
            name = format!("{}{suffix}", &ns.name[..cut]);
            n = n.saturating_add(1);
        }
        if name != ns.name {
            renamed.push(Renamed {
                from: ns.name.clone(),
                to: name.clone(),
            });
        }
        names.insert(*id, name);
    }
    (names, renamed)
}

fn namespace_record(ns: &RecalldNamespace, name: String) -> Result<NamespaceRecord, &'static str> {
    // A multiplier the namespace adopted is data; recalld's global one is config.
    let multiplier = ns.decay_rate_multiplier.or_else(|| {
        ns.schema
            .as_ref()
            .and_then(|s| s.inherited_decay)
            .map(|d| f32::from_bits(d.multiplier_bits))
    });
    Ok(NamespaceRecord {
        name,
        created_at: Some(time(ns.created_at)?),
        policy: Some(NamespacePolicy {
            initial_stability: ns.initial_stability,
            default_difficulty: ns.default_difficulty,
            phase_thresholds: PhaseThresholds {
                full_to_summary: ns.phase_thresholds.full_to_summary,
                summary_to_ghost: ns.phase_thresholds.summary_to_ghost,
                ghost_to_delete: ns.phase_thresholds.ghost_to_delete,
            },
            permastore_threshold: ns.permastore_threshold,
            decay_rate_multiplier: multiplier,
        }),
    })
}

/// Tags and labels apart. Tags that recalld derived from a label (`entity/`, `topic/`,
/// `emotion/` plus the label in lower case) are dropped when the label is there; when
/// the record has no labels at all they become labels again.
fn split_labels(
    tags: Vec<String>,
    labels: Option<[Vec<String>; 3]>,
) -> (Vec<String>, [Vec<String>; 3]) {
    const PREFIXES: [&str; 3] = [ENTITY_TERM_PREFIX, TOPIC_TERM_PREFIX, EMOTION_TERM_PREFIX];
    let labels = labels.filter(|l| l.iter().any(|v| !v.is_empty()));
    match labels {
        Some(labels) => {
            let derived: HashSet<String> = PREFIXES
                .iter()
                .zip(&labels)
                .flat_map(|(p, values)| {
                    values
                        .iter()
                        .map(move |v| format!("{p}{}", v.to_lowercase()))
                })
                .collect();
            let tags = tags.into_iter().filter(|t| !derived.contains(t)).collect();
            (tags, labels)
        }
        None => {
            let mut labels: [Vec<String>; 3] = Default::default();
            let mut plain = Vec::new();
            'tags: for tag in tags {
                for (prefix, values) in PREFIXES.iter().zip(labels.iter_mut()) {
                    if let Some(label) = tag.strip_prefix(prefix) {
                        values.push(label.to_owned());
                        continue 'tags;
                    }
                }
                plain.push(tag);
            }
            (plain, labels)
        }
    }
}

/// A memory line, or `None` for a forgotten memory.
fn memory_record(
    r: DiskRecord,
    namespace: &str,
    text: &TextFile,
) -> Result<Option<MemoryRecord>, &'static str> {
    let phase = match r.phase {
        RecalldPhase::Full => Phase::Full,
        RecalldPhase::Summary => Phase::Summary,
        RecalldPhase::Ghost => Phase::Ghost,
        RecalldPhase::Tombstone => return Ok(None),
    };
    let ghost = phase == Phase::Ghost;
    // Only a full memory keeps its text; a summary that still has text loses it.
    let full_text = if phase == Phase::Full && r.text_length > 0 {
        Some(text.read(r.text_offset, r.text_length)?)
    } else {
        None
    };
    let (anchor, retrieved, reinforced, phase_changed, labels) = match &r.schema {
        Some(s) => {
            let l = &s.lifecycle;
            let anchor = if l.lifecycle_version == "legacy_access_v1" {
                r.last_accessed_at
            } else {
                l.decay_anchor_at
            };
            let labels = [
                s.labels.entities.clone(),
                s.labels.topics.clone(),
                s.labels.emotions.clone(),
            ];
            (
                anchor,
                l.retrieved_at,
                l.reinforced_at,
                l.phase_changed_at,
                Some(labels),
            )
        }
        None => (r.last_accessed_at, None, None, r.created_at, None),
    };
    let (tags, [entities, topics, emotions]) = if ghost {
        (Vec::new(), Default::default())
    } else {
        split_labels(r.tags, labels)
    };
    let mut history = Vec::new();
    for (at, kind) in &r.access_history {
        let kind = match kind {
            RecalldAccess::Direct => AccessKind::Direct,
            RecalldAccess::Associative => AccessKind::Associative,
            RecalldAccess::Reinforcement => AccessKind::Reinforcement,
            RecalldAccess::DecaySweep => continue,
        };
        history.push(AccessEvent {
            at: time(*at)?,
            kind,
        });
    }
    // Knoggin keeps the newest 32, as recalld does.
    let history = history.split_off(history.len().saturating_sub(ACCESS_HISTORY_MAX));
    Ok(Some(MemoryRecord {
        id: MemoryId::from_bytes(r.id),
        namespace: namespace.to_owned(),
        created_at: time(r.created_at)?,
        summary: (!ghost).then_some(r.summary),
        full_text,
        tags: Some(tags),
        entities: Some(entities),
        topics: Some(topics),
        emotions: Some(emotions),
        phase,
        stability: Some(r.stability),
        difficulty: Some(r.difficulty),
        is_permastore: Some(r.is_permastore),
        decay_anchor_at: Some(time(anchor)?),
        retrieved_at: retrieved.map(time).transpose()?,
        reinforced_at: reinforced.map(time).transpose()?,
        phase_changed_at: Some(time(phase_changed)?),
        access_count: Some(history.len() as u64),
        saved_by: None,
        client: None,
        access_history: Some(history),
    }))
}

enum EdgeLine {
    Line(EdgeRecord),
    Skipped,
}

/// Key 33 bytes (source, type, target), value 13 bytes (`f32` weight, `u8` automatic,
/// `u64` created), little-endian.
fn edge_record(key: &[u8], value: &[u8]) -> Result<EdgeLine, &'static str> {
    let (Ok(key), Some(value)) = (<&[u8; 33]>::try_from(key), value.get(..13)) else {
        return Err("the edge key or value has the wrong length");
    };
    let mut source = [0_u8; 16];
    let mut target = [0_u8; 16];
    source.copy_from_slice(&key[..16]);
    target.copy_from_slice(&key[17..]);
    let mut weight = [0_u8; 4];
    weight.copy_from_slice(&value[..4]);
    let mut created = [0_u8; 8];
    created.copy_from_slice(&value[5..13]);
    let auto_created = value[4] != 0;
    let edge_type = match key[16] {
        3 | 4 => return Ok(EdgeLine::Skipped),
        2 if auto_created => return Ok(EdgeLine::Skipped),
        n => EdgeType::from_u8(n).ok_or("unknown edge type")?,
    };
    let created = i64::try_from(u64::from_le_bytes(created))
        .map_err(|_| "a time is outside the range Knoggin accepts")?;
    Ok(EdgeLine::Line(EdgeRecord {
        source: MemoryId::from_bytes(source),
        target: MemoryId::from_bytes(target),
        edge_type,
        weight: Some(f32::from_le_bytes(weight)),
        auto_created: Some(auto_created),
        created_at: Some(time(created)?),
    }))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn build(meta: Meta, raw_edges: Vec<RawEdge>, text: &TextFile, now: Timestamp) -> Import {
    let mut summary = Summary::default();
    let mut skipped = Vec::new();
    let mut skip = |table, key: String, reason| {
        skipped.push(SkippedRecord { table, key, reason });
    };

    let (names, renamed) = knoggin_names(&meta.namespaces);
    summary.renamed = renamed;
    let mut namespaces = Vec::new();
    for (id, ns) in &meta.namespaces {
        let name = names.get(id).cloned().unwrap_or_default();
        match namespace_record(ns, name) {
            Ok(r) => namespaces.push(r),
            Err(reason) => skip("namespaces", id.to_string(), reason),
        }
    }
    namespaces.sort_by(|a, b| a.name.cmp(&b.name));

    let mut memories = BTreeMap::new();
    let mut forgotten = HashSet::new();
    for (key, value) in meta.memories {
        let id = <[u8; 16]>::try_from(key.as_slice())
            .map(|k| MemoryId::from_bytes(k).to_string())
            .unwrap_or_else(|_| hex(&key));
        let decoded = DiskRecord::from_bytes(&value).and_then(|r| {
            if r.id.as_slice() != key {
                return Err("the record's ID differs from its key");
            }
            let ns = names
                .get(&r.namespace_id)
                .ok_or("the record's namespace does not exist")?;
            memory_record(r, ns, text)
        });
        match decoded {
            Ok(Some(m)) => {
                match m.phase {
                    Phase::Full => summary.memories.full += 1,
                    Phase::Summary => summary.memories.summary += 1,
                    Phase::Ghost => summary.memories.ghost += 1,
                }
                memories.insert(m.id, m);
            }
            Ok(None) => {
                summary.forgotten_skipped += 1;
                if let Ok(k) = <[u8; 16]>::try_from(key.as_slice()) {
                    forgotten.insert(MemoryId::from_bytes(k));
                }
            }
            Err(reason) => skip("memories", id, reason),
        }
    }

    let mut edges = Vec::new();
    for (key, value) in raw_edges {
        match edge_record(&key, &value) {
            Ok(EdgeLine::Line(e))
                if forgotten.contains(&e.source) || forgotten.contains(&e.target) =>
            {
                summary.edges_skipped += 1;
            }
            Ok(EdgeLine::Line(e)) => edges.push(e),
            Ok(EdgeLine::Skipped) => summary.edges_skipped += 1,
            Err(reason) => skip("edges_forward", hex(&key), reason),
        }
    }
    edges.sort_by_key(|e| (e.source, e.edge_type, e.target));

    summary.namespaces = namespaces.len() as u64;
    summary.edges = edges.len() as u64;
    summary.pending_embedding = summary.memories.full + summary.memories.summary;
    summary.errors = skipped.len() as u64;

    let mut records = Vec::with_capacity(1 + namespaces.len() + memories.len() + edges.len());
    records.push(TransferRecord::Header(ExportHeader {
        format: EXPORT_FORMAT.to_owned(),
        version: EXPORT_VERSION,
        exported_at: Some(now),
        model: None,
        counts: Some(ExportCounts {
            namespaces: namespaces.len() as u64,
            memories: memories.len() as u64,
            edges: edges.len() as u64,
        }),
    }));
    records.extend(namespaces.into_iter().map(TransferRecord::Namespace));
    records.extend(
        memories
            .into_values()
            .map(|m| TransferRecord::Memory(Box::new(m))),
    );
    records.extend(edges.into_iter().map(TransferRecord::Edge));
    Import {
        records,
        summary,
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;

    use super::*;
    use crate::record::tests::{sample, sample_schema, v1_bytes, v2_bytes};

    const STORE: Uuid = Uuid::from_u128(0x0001);
    const GENERATION: Uuid = Uuid::from_u128(0x0002);

    /// A text file in format 2 with these entries; their (offset, length).
    fn text_file(entries: &[&str]) -> (tempfile::TempDir, TextFile, Vec<(u64, u32)>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("fulltext.{GENERATION}.dat"));
        let mut bytes = vec![0_u8; 48];
        bytes[..4].copy_from_slice(b"MEMT");
        bytes[4..6].copy_from_slice(&2_u16.to_le_bytes());
        bytes[16..32].copy_from_slice(STORE.as_bytes());
        bytes[32..48].copy_from_slice(GENERATION.as_bytes());
        let mut refs = Vec::new();
        for text in entries {
            refs.push((bytes.len() as u64, text.len() as u32));
            bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&crc32fast::hash(text.as_bytes()).to_le_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let file = TextFile::open(&path, STORE, GENERATION).unwrap();
        (dir, file, refs)
    }

    fn namespace(name: &str, extra: serde_json::Value) -> RecalldNamespace {
        let mut v = json!({
            "id": 1, "name": name, "embeddingDim": 16, "initialStability": 3.7145,
            "defaultDifficulty": 5.0, "permastoreThreshold": 1500.0, "createdAt": 1000,
            "desiredRetention": 0.9,
            "phaseThresholds": {"fullToSummary": 0.7, "summaryToGhost": 0.3, "ghostToDelete": 0.05}
        });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        serde_json::from_value(v).unwrap()
    }

    fn id(n: u8) -> [u8; 16] {
        [n; 16]
    }

    fn meta(memories: Vec<Vec<u8>>) -> Meta {
        Meta {
            store_id: STORE,
            namespaces: vec![(1, namespace("default", json!({})))],
            memories: memories
                .into_iter()
                .map(|v| (v[if v[0] == 1 { 1 } else { 6 }..][..16].to_vec(), v))
                .collect(),
            text: None,
        }
    }

    fn memory(import: &Import, n: u8) -> MemoryRecord {
        import
            .records
            .iter()
            .find_map(|r| match r {
                TransferRecord::Memory(m) if m.id == MemoryId::from_bytes(id(n)) => {
                    Some((**m).clone())
                }
                _ => None,
            })
            .unwrap()
    }

    fn at(ms: i64) -> Option<Timestamp> {
        Some(Timestamp::from_millis(ms))
    }

    #[test]
    fn version_1_record_takes_its_anchor_and_labels_from_legacy_fields() {
        let (_d, text, _) = text_file(&[]);
        let mut r = sample();
        r.id = id(1);
        r.tags = vec![
            "entity/john-smith".into(),
            "topic/rust".into(),
            "kind/x".into(),
        ];
        let bytes = v1_bytes(&r, 1, &[(200, 1), (250, 3), (300, 4), (310, 2)]);
        let import = build(meta(vec![bytes]), vec![], &text, Timestamp::from_millis(9));
        let m = memory(&import, 1);
        assert_eq!(m.decay_anchor_at, at(400), "last_accessed_at");
        assert_eq!(m.phase_changed_at, at(100), "created_at");
        assert_eq!((m.retrieved_at, m.reinforced_at), (None, None));
        assert_eq!(m.tags, Some(vec!["kind/x".to_owned()]));
        assert_eq!(m.entities, Some(vec!["john-smith".to_owned()]));
        assert_eq!(m.topics, Some(vec!["rust".to_owned()]));
        let kinds: Vec<_> = m.access_history.unwrap().iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [
                AccessKind::Direct,
                AccessKind::Reinforcement,
                AccessKind::Associative
            ],
            "kind 3 is dropped"
        );
        assert_eq!(m.access_count, Some(3));
        assert_eq!(m.stability, Some(3.7145));
        assert_eq!((m.saved_by, m.client), (None, None));
    }

    #[test]
    fn version_2_record_keeps_labels_apart_from_tags() {
        let (_d, text, _) = text_file(&[]);
        let mut r = sample();
        r.id = id(1);
        r.tags = vec![
            "entity/john smith".into(),
            "entity/josé".into(),
            "topic/日本語".into(),
            "emotion/other".into(),
            "Plain".into(),
        ];
        let bytes = v2_bytes(&v1_bytes(&r, 1, &[]), &sample_schema());
        let import = build(meta(vec![bytes]), vec![], &text, Timestamp::from_millis(9));
        let m = memory(&import, 1);
        assert_eq!(
            m.entities,
            Some(vec!["John Smith".to_owned(), "José".to_owned()])
        );
        assert_eq!(m.topics, Some(vec!["日本語".to_owned()]));
        assert_eq!(m.emotions, Some(vec!["mixed feelings".to_owned()]));
        // Derived tags go; one whose label is not there stays a tag.
        assert_eq!(
            m.tags,
            Some(vec!["emotion/other".to_owned(), "Plain".to_owned()])
        );
        assert_eq!(m.retrieved_at, at(200));
        assert_eq!(m.reinforced_at, at(300));
        assert_eq!(m.phase_changed_at, at(500));
    }

    #[test]
    fn lifecycle_version_decides_the_anchor() {
        let (_d, text, _) = text_file(&[]);
        let mut r = sample();
        r.id = id(1);
        let mut legacy = sample_schema();
        legacy["lifecycle"]["decayAnchorAt"] = json!(350);
        let mut explicit = legacy.clone();
        explicit["lifecycle"]["lifecycleVersion"] = json!("explicit_feedback_v2");
        let a = v2_bytes(&v1_bytes(&r, 1, &[]), &legacy);
        r.id = id(2);
        let b = v2_bytes(&v1_bytes(&r, 1, &[]), &explicit);
        let import = build(meta(vec![a, b]), vec![], &text, Timestamp::from_millis(9));
        assert_eq!(
            memory(&import, 1).decay_anchor_at,
            at(400),
            "legacy: last_accessed_at"
        );
        assert_eq!(
            memory(&import, 2).decay_anchor_at,
            at(350),
            "explicit: the schema's"
        );
    }

    #[test]
    fn phases_text_and_forgotten_memories() {
        let (_d, text, refs) = text_file(&["kept text", "faded text"]);
        let mut records = Vec::new();
        for (n, phase, text_ref) in [
            (1_u8, 1_u8, Some(refs[0])),
            (2, 2, Some(refs[1])),
            (3, 3, None),
            (4, 4, None),
        ] {
            let mut r = sample();
            r.id = id(n);
            if let Some((offset, length)) = text_ref {
                r.text_offset = offset;
                r.text_length = length;
            }
            records.push(v1_bytes(&r, phase, &[(200, 1)]));
        }
        let import = build(meta(records), vec![], &text, Timestamp::from_millis(9));
        let full = memory(&import, 1);
        assert_eq!(
            (full.phase, full.full_text.as_deref()),
            (Phase::Full, Some("kept text"))
        );
        let summary = memory(&import, 2);
        assert_eq!((summary.phase, summary.full_text), (Phase::Summary, None));
        let ghost = memory(&import, 3);
        assert_eq!(ghost.phase, Phase::Ghost);
        assert_eq!(ghost.summary, None);
        assert_eq!(ghost.tags, Some(vec![]));
        assert_eq!(ghost.entities, Some(vec![]));
        assert_eq!(ghost.access_count, Some(1), "a ghost keeps its history");
        assert_eq!(
            import.records.len(),
            1 + 1 + 3,
            "the forgotten one is not a line"
        );
        assert_eq!(import.summary.forgotten_skipped, 1);
        assert_eq!(
            import.summary.memories,
            PhaseCounts {
                full: 1,
                summary: 1,
                ghost: 1
            }
        );
        assert_eq!(import.summary.pending_embedding, 2);
    }

    #[test]
    fn a_bad_record_is_an_error_line_and_the_rest_goes_on() {
        let (_d, text, refs) = text_file(&["some text"]);
        let mut good = sample();
        good.id = id(1);
        let mut crc = sample();
        crc.id = id(2);
        crc.text_offset = refs[0].0;
        crc.text_length = refs[0].1 - 1;
        let mut features = sample_schema();
        features["requiredFeatures"] = json!(["future"]);
        let mut feature = sample();
        feature.id = id(3);
        let mut other_ns = sample();
        other_ns.id = id(4);
        other_ns.namespace_id = 9;
        let mut too_late = sample();
        too_late.id = id(5);
        too_late.created_at = i64::MAX;
        let mut history = sample();
        history.id = id(6);
        let events: Vec<_> = (0..40).map(|i| (1000 + i, 1)).collect();
        let records = vec![
            v1_bytes(&good, 1, &[]),
            v1_bytes(&crc, 1, &[]),
            v2_bytes(&v1_bytes(&feature, 1, &[]), &features),
            v1_bytes(&other_ns, 1, &[]),
            v1_bytes(&too_late, 1, &[]),
            v1_bytes(&history, 1, &events),
        ];
        let import = build(meta(records), vec![], &text, Timestamp::from_millis(9));
        let reasons: Vec<_> = import.skipped.iter().map(|s| s.reason).collect();
        assert_eq!(
            reasons,
            [
                "the text entry's length does not match the record",
                "the record needs a feature this importer does not know",
                "the record's namespace does not exist",
                "a time is outside the range Knoggin accepts",
            ]
        );
        assert_eq!(
            import.skipped[0].key,
            MemoryId::from_bytes(id(2)).to_string()
        );
        assert_eq!(import.summary.errors, 4);
        let kept = memory(&import, 6).access_history.unwrap();
        assert_eq!(kept.len(), ACCESS_HISTORY_MAX, "the newest 32");
        assert_eq!(kept[0].at, Timestamp::from_millis(1008));
        memory(&import, 1);
    }

    #[test]
    fn a_crc_mismatch_is_refused() {
        let (_d, text, refs) = text_file(&["abc"]);
        assert_eq!(text.read(refs[0].0, refs[0].1).unwrap(), "abc");
        let path = _d.path().join(format!("fulltext.{GENERATION}.dat"));
        let mut bytes = std::fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() = b'x';
        std::fs::write(&path, bytes).unwrap();
        let text = TextFile::open(&path, STORE, GENERATION).unwrap();
        assert_eq!(
            text.read(refs[0].0, refs[0].1),
            Err("the text entry's CRC32 does not match")
        );
        assert!(text.read(0, 3).is_err(), "inside the header");
        assert!(text.read(refs[0].0, 4).is_err(), "past the end");
    }

    #[test]
    fn the_text_header_must_name_this_store_and_generation() {
        let (d, _, _) = text_file(&[]);
        let path = d.path().join(format!("fulltext.{GENERATION}.dat"));
        assert!(matches!(
            TextFile::open(&path, Uuid::from_u128(9), GENERATION),
            Err(ImportError::Corrupt(_))
        ));
        assert!(matches!(
            TextFile::open(&path, STORE, Uuid::from_u128(9)),
            Err(ImportError::Corrupt(_))
        ));
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4] = 1;
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            TextFile::open(&path, STORE, GENERATION),
            Err(ImportError::TooOld)
        ));
    }

    #[test]
    fn the_text_file_name_must_be_this_generation() {
        let row = |name: &str| {
            vec![
                ("active".to_owned(), serde_json::to_vec(&GENERATION).unwrap()),
                (
                    GENERATION.to_string(),
                    serde_json::to_vec(&json!({
                        "id": GENERATION, "storeId": STORE, "filename": name,
                        "formatVersion": 2, "sealedLength": 48, "sealedSha256": ([0_u8; 32].to_vec())
                    }))
                    .unwrap(),
                ),
            ]
        };
        let good = format!("fulltext.{GENERATION}.dat");
        assert_eq!(
            active_text(&row(&good)).unwrap(),
            Some((GENERATION, good.clone(), 2))
        );
        for bad in [
            "../meta.db".to_owned(),
            format!("fulltext.{}.dat", Uuid::from_u128(5)),
            format!("fulltext.{}.dat", GENERATION.simple()),
            format!("../fulltext.{GENERATION}.dat"),
        ] {
            assert!(
                matches!(active_text(&row(&bad)), Err(ImportError::Corrupt(_))),
                "{bad}"
            );
        }
        assert_eq!(
            active_text(&row(&good)[1..]).unwrap(),
            None,
            "no active row"
        );
        assert_eq!(
            active_text(&row(&good)[..1]).unwrap(),
            None,
            "no generation row"
        );
    }

    #[test]
    fn namespace_policy_takes_an_adopted_multiplier_only() {
        let own = namespace(
            "a",
            json!({"decayRateMultiplier": 0.5,
            "schema": {"inheritedDecay": {"multiplierBits": 0.75_f32.to_bits()}}}),
        );
        let adopted = namespace(
            "b",
            json!({"schema": {"inheritedDecay": {
            "version": 1, "multiplierBits": 0.75_f32.to_bits()}, "vectorCertification": {},
            "requiredFeatures": ["x"]}}),
        );
        let neither = namespace(
            "c",
            json!({"schema": {"lifecycleVersion": "legacy_access_v1"}}),
        );
        let policy =
            |n: &RecalldNamespace| namespace_record(n, n.name.clone()).unwrap().policy.unwrap();
        assert_eq!(policy(&own).decay_rate_multiplier, Some(0.5));
        assert_eq!(policy(&adopted).decay_rate_multiplier, Some(0.75));
        assert_eq!(policy(&neither).decay_rate_multiplier, None);
        let r = namespace_record(&neither, "c".into()).unwrap();
        assert_eq!(r.created_at, at(1000));
        assert_eq!(r.policy.unwrap().initial_stability, 3.7145);
    }

    #[test]
    fn names_that_differ_only_in_case_get_a_suffix() {
        let long = "x".repeat(64);
        let list: Vec<_> = [
            "Work",
            "work",
            "WORK",
            "work-2",
            long.as_str(),
            &long.to_uppercase(),
        ]
        .iter()
        .enumerate()
        .map(|(i, n)| (i as u32 + 1, namespace(n, json!({}))))
        .collect();
        let (names, renamed) = knoggin_names(&list);
        assert_eq!(names[&1], "Work");
        assert_eq!(names[&2], "work-2");
        assert_eq!(names[&3], "WORK-3");
        assert_eq!(names[&4], "work-2-2", "work-2 was taken by the rename");
        assert_eq!(names[&5], long);
        assert_eq!(names[&6], format!("{}-2", "X".repeat(62)));
        assert_eq!(renamed.len(), 4);
        assert_eq!(
            renamed[0],
            Renamed {
                from: "work".into(),
                to: "work-2".into()
            }
        );
    }

    fn edge(source: u8, kind: u8, target: u8, weight: f32, auto: bool, created: u64) -> RawEdge {
        let mut k = vec![source; 16];
        k.push(kind);
        k.extend_from_slice(&[target; 16]);
        let mut v = weight.to_le_bytes().to_vec();
        v.push(u8::from(auto));
        v.extend_from_slice(&created.to_le_bytes());
        (k, v)
    }

    #[test]
    fn edges_are_mapped_skipped_and_ordered() {
        let (_d, text, _) = text_file(&[]);
        let mut a = sample();
        a.id = id(1);
        let mut b = sample();
        b.id = id(2);
        let mut gone = sample();
        gone.id = id(3);
        let records = vec![
            v1_bytes(&a, 1, &[]),
            v1_bytes(&b, 1, &[]),
            v1_bytes(&gone, 4, &[]),
        ];
        let edges = vec![
            edge(2, 7, 1, 1.0, false, 50),
            edge(1, 6, 2, 0.9, true, 40),
            edge(1, 1, 2, 1.0, false, 30),
            edge(1, 2, 2, 0.4, false, 31),
            edge(2, 2, 1, 0.5, true, 32),
            edge(1, 3, 2, 1.0, false, 33),
            edge(1, 4, 2, 1.0, false, 34),
            edge(1, 5, 3, 1.0, true, 35),
            edge(3, 5, 1, 1.0, true, 36),
            edge(1, 9, 2, 1.0, false, 37),
            (vec![1; 32], vec![0; 13]),
        ];
        let import = build(meta(records), edges, &text, Timestamp::from_millis(9));
        let lines: Vec<_> = import
            .records
            .iter()
            .filter_map(|r| match r {
                TransferRecord::Edge(e) => Some(e),
                _ => None,
            })
            .collect();
        let keys: Vec<_> = lines
            .iter()
            .map(|e| (e.source.as_bytes()[0], e.edge_type, e.target.as_bytes()[0]))
            .collect();
        assert_eq!(
            keys,
            [
                (1, EdgeType::ParentChild, 2),
                (1, EdgeType::Associative, 2),
                (1, EdgeType::Temporal, 2),
                (2, EdgeType::Supersedes, 1),
            ]
        );
        assert_eq!(
            lines[1].auto_created,
            Some(false),
            "a hand-made associative edge stays"
        );
        assert_eq!(lines[2].weight, Some(0.9));
        assert_eq!(lines[2].created_at, at(40));
        assert_eq!(import.summary.edges, 4);
        assert_eq!(
            import.summary.edges_skipped, 5,
            "types 3 and 4, automatic 2, forgotten ends"
        );
        let reasons: Vec<_> = import.skipped.iter().map(|s| s.reason).collect();
        assert_eq!(
            reasons,
            [
                "unknown edge type",
                "the edge key or value has the wrong length"
            ]
        );
    }

    #[test]
    fn lines_come_in_export_order() {
        let (_d, text, _) = text_file(&[]);
        let mut m = meta(vec![]);
        m.namespaces = vec![
            (1, namespace("zeta", json!({}))),
            (2, namespace("Alpha", json!({}))),
            (3, namespace("default", json!({}))),
        ];
        for n in [9_u8, 3, 5] {
            let mut r = sample();
            r.id = id(n);
            r.namespace_id = 3;
            let bytes = v1_bytes(&r, 1, &[]);
            m.memories.push((id(n).to_vec(), bytes));
        }
        let import = build(m, vec![], &text, Timestamp::from_millis(77));
        let TransferRecord::Header(h) = &import.records[0] else {
            panic!("header first");
        };
        assert_eq!(h.exported_at, Some(Timestamp::from_millis(77)));
        assert_eq!(h.model, None);
        assert_eq!(
            h.counts,
            Some(ExportCounts {
                namespaces: 3,
                memories: 3,
                edges: 0
            })
        );
        let order: Vec<String> = import.records[1..]
            .iter()
            .map(|r| match r {
                TransferRecord::Namespace(n) => n.name.clone(),
                TransferRecord::Memory(m) => m.id.as_bytes()[0].to_string(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(order, ["Alpha", "default", "zeta", "3", "5", "9"]);
    }

    #[test]
    fn a_record_whose_id_differs_from_its_key_is_an_error_line() {
        let (_d, text, _) = text_file(&[]);
        let mut m = meta(vec![]);
        let mut r = sample();
        r.id = id(1);
        m.memories.push((id(2).to_vec(), v1_bytes(&r, 1, &[])));
        let import = build(m, vec![], &text, Timestamp::from_millis(9));
        assert_eq!(
            import.skipped[0].reason,
            "the record's ID differs from its key"
        );
    }

    /// The explicit step that writes the seed corpus of the fuzz target `recalld_record`
    /// from the recalld data fixture: each value of its `memories` table, and each edge
    /// as its 33-byte key followed by its value, as the target splits them.
    #[test]
    #[ignore = "writes fuzz/seeds/recalld_record"]
    fn write_fuzz_seeds() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let out = root.join("fuzz/seeds/recalld_record");
        // redb may write to a database it opens; the fixture is read from copies.
        let tmp = tempfile::tempdir().unwrap();
        for db in ["meta.db", "edges.db"] {
            std::fs::copy(
                root.join("tests/fixtures/recalld-data").join(db),
                tmp.path().join(db),
            )
            .unwrap();
        }
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        for (key, value) in read_meta(&tmp.path().join("meta.db")).unwrap().memories {
            std::fs::write(out.join(format!("memory-{}", hex(&key))), value).unwrap();
        }
        for (key, value) in read_edges(&tmp.path().join("edges.db")).unwrap() {
            std::fs::write(
                out.join(format!("edge-{}", hex(&key))),
                [key, value].concat(),
            )
            .unwrap();
        }
    }
}
