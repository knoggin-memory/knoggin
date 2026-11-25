// SPDX-License-Identifier: AGPL-3.0-only
//! The embedding provider interface, the model identity, and the exact texts that are
//! embedded: the document recipe `doc-v1` and the query recipe `query-v1`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::Memory;
use crate::time::Timestamp;

/// Turns one text into one unit vector. The real model and the test fake implement it.
pub trait EmbeddingProvider: Send + Sync {
    /// The identity string and the dimension of the vectors it makes.
    fn identity(&self) -> &ModelIdentity;
    /// One text, already in its recipe, in; one unit vector out.
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
}

/// Names everything that can change a vector. A store has exactly one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelIdentity {
    /// The identity string, for example `embeddinggemma-2-q4-768-80bc2e03328dae2f`.
    pub id: String,
    /// Values per vector.
    pub dimensions: u32,
}

/// Why a text could not be embedded.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmbedError {
    /// The model or the tokenizer could not be loaded.
    #[error("could not load the model: {0}")]
    Load(String),
    /// A run of the model failed.
    #[error("the model run failed: {0}")]
    Run(String),
    /// The model returned an output of the wrong shape or a zero vector.
    #[error("the model returned an unexpected output")]
    BadOutput,
    /// ONNX Runtime was not initialized before a provider was loaded.
    #[error("ONNX Runtime is not initialized")]
    NotInitialized,
    /// ONNX Runtime was initialized twice.
    #[error("ONNX Runtime is already initialized")]
    AlreadyInitialized,
}

impl EmbedError {
    /// A load failure, keeping the cause's text.
    pub fn load(cause: impl fmt::Display) -> Self {
        Self::Load(cause.to_string())
    }

    /// A run failure, keeping the cause's text.
    pub fn run(cause: impl fmt::Display) -> Self {
        Self::Run(cause.to_string())
    }

    /// The model returned an unexpected output.
    pub fn bad_output() -> Self {
        Self::BadOutput
    }

    /// ONNX Runtime is not initialized.
    pub fn not_initialized() -> Self {
        Self::NotInitialized
    }

    /// ONNX Runtime is already initialized.
    pub fn already_initialized() -> Self {
        Self::AlreadyInitialized
    }
}

impl fmt::Debug for EmbedError {
    /// A cause's text could quote the input, so `Debug` names the variant only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Load(_) => "EmbedError::Load",
            Self::Run(_) => "EmbedError::Run",
            Self::BadOutput => "EmbedError::BadOutput",
            Self::NotInitialized => "EmbedError::NotInitialized",
            Self::AlreadyInitialized => "EmbedError::AlreadyInitialized",
        })
    }
}

/// The 34 bytes put before a query: recipe `query-v1`. It ends with exactly one space.
pub const QUERY_PREFIX: &str = "task: question answering | query: ";

/// A query in its recipe.
pub fn query_text(query: &str) -> String {
    format!("{QUERY_PREFIX}{query}")
}

/// A memory in the document recipe `doc-v1`:
/// `title: {entities} · {date} | text: {summary}{full}{tags}`.
pub fn document_text(memory: &Memory) -> String {
    document_text_from_parts(
        &memory.labels.entities,
        memory.created_at,
        &memory.summary,
        memory.full_text.as_deref(),
        &memory.tag_terms(),
    )
}

/// The document recipe for content that is not a memory yet, as the duplicate check
/// needs it. `tag_terms` is the list of [`crate::model::tag_terms`].
pub fn document_text_from_parts(
    entities: &[String],
    created_at: Timestamp,
    summary: &str,
    full_text: Option<&str>,
    tag_terms: &[String],
) -> String {
    let entities = if entities.is_empty() {
        "none".to_owned()
    } else {
        entities.join(", ")
    };
    let date = created_at.utc_date().unwrap_or_default();
    let full = full_text.map(|t| format!("\n\n{t}")).unwrap_or_default();
    let tags = if tag_terms.is_empty() {
        String::new()
    } else {
        format!(" {}", tag_terms.join(" "))
    };
    format!("title: {entities} \u{b7} {date} | text: {summary}{full}{tags}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Labels, Tag, tag_terms};

    #[test]
    fn query_prefix_is_34_bytes() {
        assert_eq!(QUERY_PREFIX.len(), 34);
        assert_eq!(
            query_text("where?"),
            "task: question answering | query: where?"
        );
    }

    #[test]
    fn document_recipe_matches_the_spec_example() {
        let labels = Labels {
            entities: vec!["Maya".into()],
            ..Labels::default()
        };
        let tags = vec![Tag::new("travel").unwrap()];
        let created = Timestamp::parse_rfc3339("2026-11-23T10:15:30Z").unwrap();
        let text = document_text_from_parts(
            &labels.entities,
            created,
            "Maya prefers window seats",
            None,
            &tag_terms(&tags, &labels),
        );
        assert_eq!(
            text,
            "title: Maya · 2026-11-23 | text: Maya prefers window seats travel entity/maya"
        );
    }

    #[test]
    fn document_recipe_without_entities_or_tags_and_with_full_text() {
        let created = Timestamp::parse_rfc3339("2026-01-02T00:00:00Z").unwrap();
        let text = document_text_from_parts(&[], created, "s", Some("long"), &[]);
        assert_eq!(text, "title: none · 2026-01-02 | text: s\n\nlong");
        let two = vec!["A".to_owned(), "B".to_owned()];
        let text = document_text_from_parts(&two, created, "s", None, &[]);
        assert!(text.starts_with("title: A, B · "));
    }

    #[test]
    fn embed_error_debug_hides_the_cause() {
        let e = EmbedError::run("input was marker-secret");
        assert_eq!(format!("{e:?}"), "EmbedError::Run");
        assert!(e.to_string().contains("marker-secret"));
    }
}
