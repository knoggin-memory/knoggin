// SPDX-License-Identifier: AGPL-3.0-only
//! The full-text index: SQLite FTS5 in memory, one database per open store, behind a
//! `Mutex`. Schema, scoring and query string are recalld's (`src/search/fts.rs`).
//!
//! It is rebuilt from the memories when a store is loaded and is never saved.

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, PoisonError};

use rusqlite::{Connection, OptionalExtension, params};

use crate::id::{MemoryId, NamespaceId};

const SCHEMA: &str = "
CREATE TABLE id_map (
  rowid        INTEGER PRIMARY KEY AUTOINCREMENT,
  namespace_id INTEGER NOT NULL,
  memory_id    BLOB    NOT NULL UNIQUE
);
CREATE VIRTUAL TABLE fts_content USING fts5(
  summary, full_text, tags,
  content='', contentless_delete=1, tokenize='porter unicode61'
);
";

const SEARCH: &str = "
SELECT m.memory_id, -bm25(fts_content, 5.0, 1.0, 2.0) AS score
FROM fts_content JOIN id_map m ON m.rowid = fts_content.rowid
WHERE fts_content MATCH ?1 AND m.namespace_id = ?2
ORDER BY score DESC, m.memory_id ASC
LIMIT ?3";

/// A full-text failure. The recall pipeline logs it and treats it as no hits.
#[derive(Debug, thiserror::Error)]
pub enum FtsError {
    /// SQLite failed.
    #[error("full-text index error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// An earlier write failed, so the index no longer matches the memories. It is
    /// whole again after a rebuild.
    #[error("the full-text index is out of date until it is rebuilt")]
    Broken,
    /// A row held a malformed ID or a score that is not finite.
    #[error("the full-text index returned a malformed row")]
    BadRow,
}

/// What is indexed for one memory: its namespace and a hash of the indexed text.
/// Kept so that a test can compare the live index with a rebuild; the text itself
/// is not kept twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FtsDoc {
    /// The namespace.
    pub namespace: NamespaceId,
    /// Hash of summary, full text and tags column, as indexed.
    pub text_hash: u64,
}

/// The index. `None` inside the mutex means the database could not be opened.
pub struct FtsIndex {
    conn: Mutex<Option<Connection>>,
    docs: BTreeMap<MemoryId, FtsDoc>,
    broken: bool,
}

impl std::fmt::Debug for FtsIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtsIndex")
            .field("docs", &self.docs.len())
            .field("broken", &self.broken)
            .finish()
    }
}

impl Default for FtsIndex {
    fn default() -> Self {
        Self::new()
    }
}

fn open() -> Option<Connection> {
    let conn = Connection::open_in_memory().ok()?;
    conn.execute_batch(SCHEMA).ok()?;
    Some(conn)
}

/// The FTS5 query for a user query: split on white space, drop double quotes, drop
/// empty tokens, quote each token and join with `OR`.
pub fn match_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn text_hash(summary: &str, full_text: Option<&str>, tags: &str) -> u64 {
    let mut h = DefaultHasher::new();
    (summary, full_text, tags).hash(&mut h);
    h.finish()
}

impl FtsIndex {
    /// An empty index in a new in-memory database.
    pub fn new() -> Self {
        let conn = open();
        let broken = conn.is_none();
        Self {
            conn: Mutex::new(conn),
            docs: BTreeMap::new(),
            broken,
        }
    }

    /// Drops every row and starts a new database: the first step of a rebuild.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// False after a write failed; a rebuild makes it true again.
    pub fn is_healthy(&self) -> bool {
        !self.broken
    }

    /// Adds or replaces the row of one memory. `tags` is the tags column: the
    /// memory's plain tags joined by spaces.
    pub fn upsert(
        &mut self,
        namespace: NamespaceId,
        memory: MemoryId,
        summary: &str,
        full_text: Option<&str>,
        tags: &str,
    ) {
        let result = self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            delete_row(&tx, memory)?;
            tx.execute(
                "INSERT INTO id_map (namespace_id, memory_id) VALUES (?1, ?2)",
                params![namespace.get(), memory.as_bytes().as_slice()],
            )?;
            let rowid = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO fts_content (rowid, summary, full_text, tags) VALUES (?1, ?2, ?3, ?4)",
                params![rowid, summary, full_text.unwrap_or(""), tags],
            )?;
            tx.commit()
        });
        self.note(result);
        self.docs.insert(
            memory,
            FtsDoc {
                namespace,
                text_hash: text_hash(summary, full_text, tags),
            },
        );
    }

    /// Removes the row of one memory. Returns whether it had one.
    pub fn remove(&mut self, memory: MemoryId) -> bool {
        if self.docs.remove(&memory).is_none() {
            return false;
        }
        let result = self.with_conn(|conn| delete_row(conn, memory).map(|_| ()));
        self.note(result);
        true
    }

    /// Moves the row of one memory to another namespace. Scores do not change.
    pub fn set_namespace(&mut self, memory: MemoryId, namespace: NamespaceId) {
        let Some(doc) = self.docs.get_mut(&memory) else {
            return;
        };
        doc.namespace = namespace;
        let result = self.with_conn(|conn| {
            conn.execute(
                "UPDATE id_map SET namespace_id = ?1 WHERE memory_id = ?2",
                params![namespace.get(), memory.as_bytes().as_slice()],
            )
            .map(|_| ())
        });
        self.note(result);
    }

    /// The top `limit` memories of `namespace` for `query`, by score descending, then ID.
    /// An empty query string gives no hits.
    pub fn search(
        &self,
        namespace: NamespaceId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryId, f32)>, FtsError> {
        if self.broken {
            return Err(FtsError::Broken);
        }
        let fts_query = match_query(query);
        if fts_query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let conn = guard.as_ref().ok_or(FtsError::Broken)?;
        let mut stmt = conn.prepare_cached(SEARCH)?;
        let rows = stmt.query_map(params![fts_query, namespace.get(), limit], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, f64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (bytes, score) = row?;
            let bytes: [u8; 16] = bytes.try_into().map_err(|_| FtsError::BadRow)?;
            let score = score as f32;
            if !score.is_finite() {
                return Err(FtsError::BadRow);
            }
            out.push((MemoryId::from_bytes(bytes), score));
        }
        Ok(out)
    }

    /// Whether `memory` has a row.
    pub fn contains(&self, memory: MemoryId) -> bool {
        self.docs.contains_key(&memory)
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// No rows.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// What is indexed per memory, in ID order. For index comparisons.
    pub fn docs(&self) -> &BTreeMap<MemoryId, FtsDoc> {
        &self.docs
    }

    fn with_conn<T>(
        &mut self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Option<rusqlite::Result<T>> {
        let conn = self
            .conn
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()?;
        Some(f(conn))
    }

    /// A failed or impossible write leaves the index out of date until the next rebuild.
    fn note<T>(&mut self, result: Option<rusqlite::Result<T>>) {
        if !matches!(result, Some(Ok(_))) {
            self.broken = true;
        }
    }

    #[cfg(test)]
    fn row_counts(&self) -> (i64, i64) {
        let guard = self.conn.lock().unwrap();
        let conn = guard.as_ref().unwrap();
        let count = |sql| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        (
            count("SELECT count(*) FROM id_map"),
            count("SELECT count(*) FROM fts_content"),
        )
    }
}

/// Deletes the FTS row first (a contentless table is reachable only by rowid), then
/// the `id_map` row.
fn delete_row(conn: &Connection, memory: MemoryId) -> rusqlite::Result<bool> {
    let rowid: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM id_map WHERE memory_id = ?1",
            params![memory.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(rowid) = rowid else {
        return Ok(false);
    };
    conn.execute("DELETE FROM fts_content WHERE rowid = ?1", params![rowid])?;
    conn.execute("DELETE FROM id_map WHERE rowid = ?1", params![rowid])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from recalld `src/search/fts.rs`, on the in-memory database.

    fn ns1() -> NamespaceId {
        NamespaceId::new(1)
    }

    fn ns2() -> NamespaceId {
        NamespaceId::new(2)
    }

    fn id(n: u128) -> MemoryId {
        MemoryId::from_bytes(n.to_be_bytes())
    }

    fn ids(results: &[(MemoryId, f32)]) -> Vec<MemoryId> {
        results.iter().map(|r| r.0).collect()
    }

    #[test]
    fn add_and_search() {
        let mut index = FtsIndex::new();
        index.upsert(ns1(), id(1), "Rust programming language systems", None, "");
        index.upsert(
            ns1(),
            id(2),
            "Python programming language scripting",
            None,
            "",
        );
        index.upsert(ns1(), id(3), "Rust memory safety ownership", None, "");
        let results = index.search(ns1(), "Rust programming", 10).unwrap();
        assert_eq!(results[0].0, id(1));
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn remove_document_deletes_the_content_row_too() {
        let mut index = FtsIndex::new();
        index.upsert(ns1(), id(1), "test document content", None, "");
        assert_eq!(index.row_counts(), (1, 1));
        assert!(index.remove(id(1)));
        assert_eq!(index.row_counts(), (0, 0));
        assert!(index.is_empty());
        assert!(index.search(ns1(), "test", 10).unwrap().is_empty());
        assert!(!index.remove(id(1)));
    }

    #[test]
    fn search_empty_index_and_no_matching_terms() {
        let mut index = FtsIndex::new();
        assert!(index.search(ns1(), "anything", 10).unwrap().is_empty());
        index.upsert(ns1(), id(1), "alpha beta gamma", None, "");
        assert!(index.search(ns1(), "delta epsilon", 10).unwrap().is_empty());
    }

    #[test]
    fn update_existing_document() {
        let mut index = FtsIndex::new();
        index.upsert(ns1(), id(1), "original content", None, "");
        index.upsert(ns1(), id(1), "updated replacement", None, "");
        assert!(index.search(ns1(), "original", 10).unwrap().is_empty());
        assert_eq!(ids(&index.search(ns1(), "updated", 10).unwrap()), [id(1)]);
        assert_eq!(index.row_counts(), (1, 1));
    }

    #[test]
    fn namespace_isolation_and_moves() {
        let mut index = FtsIndex::new();
        index.upsert(ns1(), id(1), "shared keyword alpha", None, "");
        index.upsert(ns2(), id(2), "shared keyword alpha", None, "");
        assert_eq!(ids(&index.search(ns1(), "alpha", 10).unwrap()), [id(1)]);
        assert_eq!(ids(&index.search(ns2(), "alpha", 10).unwrap()), [id(2)]);
        index.set_namespace(id(1), ns2());
        assert!(index.search(ns1(), "alpha", 10).unwrap().is_empty());
        assert_eq!(
            ids(&index.search(ns2(), "alpha", 10).unwrap()),
            [id(1), id(2)]
        );
        assert_eq!(index.docs()[&id(1)].namespace, ns2());
    }

    #[test]
    fn query_syntax_is_neutralized() {
        let mut index = FtsIndex::new();
        index.upsert(ns1(), id(1), "some test document", None, "");
        for q in [
            "AND OR NOT",
            "NEAR(test, doc)",
            "test*",
            "^test",
            "\"quoted phrase\"",
            "",
            "   ",
            "\"",
        ] {
            index.search(ns1(), q, 10).unwrap();
        }
    }

    #[test]
    fn match_query_quotes_and_ors() {
        assert_eq!(
            match_query("Sarah trip Japan"),
            "\"Sarah\" OR \"trip\" OR \"Japan\""
        );
        assert_eq!(match_query(" a\"b  \"\" c "), "\"ab\" OR \"c\"");
        assert_eq!(match_query("   "), "");
    }

    #[test]
    fn columns_are_weighted_summary_over_tags_over_full_text() {
        let mut index = FtsIndex::new();
        index.upsert(ns1(), id(1), "x", Some("kiwi"), "");
        index.upsert(ns1(), id(2), "x", None, "kiwi");
        index.upsert(ns1(), id(3), "kiwi", None, "");
        assert_eq!(
            ids(&index.search(ns1(), "kiwi", 10).unwrap()),
            [id(3), id(2), id(1)]
        );
    }

    #[test]
    fn stemming_and_limit() {
        let mut index = FtsIndex::new();
        for n in 1..=5 {
            index.upsert(ns1(), id(n), "running runners", None, "");
        }
        let hits = index.search(ns1(), "run", 3).unwrap();
        assert_eq!(
            ids(&hits),
            [id(1), id(2), id(3)],
            "equal scores fall back to ID"
        );
        assert!(index.search(ns1(), "run", 0).unwrap().is_empty());
    }

    /// recalld `repeated_clear_rehydrate_preserves_exact_fts_scores`, extended: after
    /// deletes, the live index scores exactly as a rebuild with the remaining rows. The
    /// index-equivalence property test relies on this.
    #[test]
    fn deletes_leave_the_same_scores_as_a_rebuild() {
        let docs = [
            (id(1), "alpha alpha beta", ""),
            (id(2), "beta", "alpha beta gamma delta epsilon zeta"),
            (id(3), "gamma delta", "beta"),
            (id(4), "alpha gamma gamma", "delta"),
            (id(5), "epsilon beta", "alpha"),
        ];
        let mut live = FtsIndex::new();
        for (i, s, b) in docs {
            live.upsert(ns1(), i, s, Some(b), "tag/x");
        }
        live.remove(id(2));
        live.upsert(ns1(), id(4), "alpha gamma", None, "");
        live.remove(id(5));
        let mut rebuilt = FtsIndex::new();
        rebuilt.upsert(ns1(), id(1), "alpha alpha beta", Some(""), "tag/x");
        rebuilt.upsert(ns1(), id(3), "gamma delta", Some("beta"), "tag/x");
        rebuilt.upsert(ns1(), id(4), "alpha gamma", None, "");
        for q in ["alpha", "beta", "gamma", "alpha gamma", "delta beta", "tag"] {
            assert_eq!(
                live.search(ns1(), q, 10).unwrap(),
                rebuilt.search(ns1(), q, 10).unwrap(),
                "query {q}"
            );
        }
        assert_eq!(live.docs(), rebuilt.docs());
    }

    #[test]
    fn reset_empties_and_heals() {
        let mut index = FtsIndex::new();
        index.upsert(ns1(), id(1), "alpha", None, "");
        index.broken = true;
        assert!(matches!(
            index.search(ns1(), "alpha", 1),
            Err(FtsError::Broken)
        ));
        index.reset();
        assert!(index.is_healthy());
        assert!(index.is_empty());
        assert!(index.search(ns1(), "alpha", 1).unwrap().is_empty());
    }
}
