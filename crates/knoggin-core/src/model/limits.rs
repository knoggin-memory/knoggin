// SPDX-License-Identifier: AGPL-3.0-only
//! Limits, engine constants and the content validation of a save.
//!
//! Ported from recalld `src/model/constants.rs` and `src/model/validation.rs`,
//! with Knoggin's own `FULL_TEXT_MAX_BYTES` and `QUERY_MAX_BYTES`. Limits are bytes of UTF-8.

use crate::error::EngineError;
use crate::time::{MILLIS_PER_DAY, Timestamp};

use super::markup;
use super::tag::Tag;

// ── Content limits ──────────────────────────────────────────────────

/// Longest summary.
pub const SUMMARY_MAX_BYTES: usize = 2_000;
/// Longest full text of a new memory (recalld allows 1 MiB).
pub const FULL_TEXT_MAX_BYTES: usize = 65_536;
/// Most tags on one memory.
pub const MAX_TAGS: usize = 64;
/// Longest tag.
pub const TAG_MAX_BYTES: usize = 128;
/// Most entities on one memory.
pub const MAX_ENTITIES: usize = 32;
/// Most topics on one memory.
pub const MAX_TOPICS: usize = 32;
/// Most emotions on one memory.
pub const MAX_EMOTIONS: usize = 32;
/// Longest entity: 128 bytes less the `entity/` prefix.
pub const ENTITY_MAX_BYTES: usize = 121;
/// Longest topic: 128 bytes less the `topic/` prefix.
pub const TOPIC_MAX_BYTES: usize = 122;
/// Longest emotion: 128 bytes less the `emotion/` prefix.
pub const EMOTION_MAX_BYTES: usize = 120;
/// Longest recall or search query (recalld has no limit).
pub const QUERY_MAX_BYTES: usize = 4_096;
/// Longest namespace name.
pub const NAMESPACE_NAME_MAX_BYTES: usize = 64;
/// Most items in a batch save, a batch forget or a move.
pub const MAX_BATCH: usize = 100;
/// Access events kept per memory, newest last.
pub const ACCESS_HISTORY_MAX: usize = 32;
/// Name of the namespace every store has and that cannot be deleted.
pub const DEFAULT_NAMESPACE: &str = "default";

/// Prefix of the label term made from an entity.
pub const ENTITY_TERM_PREFIX: &str = "entity/";
/// Prefix of the label term made from a topic.
pub const TOPIC_TERM_PREFIX: &str = "topic/";
/// Prefix of the label term made from an emotion.
pub const EMOTION_TERM_PREFIX: &str = "emotion/";

// ── Request defaults and bounds ─────────────────────────────────────

/// Default page size of a list.
pub const DEFAULT_PAGE_LIMIT: u32 = 50;
/// Largest page size of a list.
pub const MAX_PAGE_LIMIT: u32 = 200;
/// Default number of recall hits.
pub const DEFAULT_RECALL_LIMIT: u32 = 25;
/// Largest number of recall hits.
pub const MAX_RECALL_LIMIT: u32 = 100;
/// Default number of recall hits that carry full text (or `limit`, when smaller).
pub const DEFAULT_FULL_TEXT_COUNT: u32 = 5;
/// Deepest graph expansion profile of a recall.
pub const MAX_RECALL_DEPTH: u8 = 3;
/// Default number of similar-search hits.
pub const DEFAULT_SIMILAR_LIMIT: u32 = 10;
/// Largest number of similar-search hits.
pub const MAX_SIMILAR_LIMIT: u32 = 100;
/// Default number of context memories.
pub const DEFAULT_CONTEXT_LIMIT: u32 = 10;
/// Largest number of context memories.
pub const MAX_CONTEXT_LIMIT: u32 = 25;
/// How many context memories carry full text.
pub const CONTEXT_FULL_TEXT_COUNT: usize = 3;
/// Default reinforcement quality.
pub const DEFAULT_QUALITY: u8 = 3;

// ── Decay ───────────────────────────────────────────────────────────

/// Lowest stability, in days.
pub const STABILITY_FLOOR: f32 = 0.01;
/// Highest stability, in days (about 100 years).
pub const STABILITY_CEILING: f32 = 36_500.0;
/// Default initial stability of a namespace (FSRS v4.5 w2, "Good").
pub const DEFAULT_INITIAL_STABILITY: f32 = 3.7145;
/// Lowest difficulty.
pub const DIFFICULTY_MIN: f32 = 1.0;
/// Highest difficulty.
pub const DIFFICULTY_MAX: f32 = 10.0;
/// Default difficulty of a namespace.
pub const DEFAULT_DIFFICULTY: f32 = 5.0;
/// Default `full_to_summary` threshold.
pub const DEFAULT_FULL_TO_SUMMARY: f32 = 0.7;
/// Default `summary_to_ghost` threshold.
pub const DEFAULT_SUMMARY_TO_GHOST: f32 = 0.3;
/// Default `ghost_to_delete` threshold.
pub const DEFAULT_GHOST_TO_DELETE: f32 = 0.05;
/// Default permastore threshold, in days.
pub const DEFAULT_PERMASTORE_THRESHOLD: f32 = 1_500.0;

// ── Engine constants ────────────────────────────────────────────────

/// Largest connection bonus.
pub const MAX_CONNECTION_BONUS: f32 = 0.15;
/// Lowest cosine of an automatic semantic link (never below 0.40).
pub const AUTO_LINK_THRESHOLD: f32 = 0.50;
/// Lowest cosine of a semantic link to a memory that shares a tag or label term.
pub const AUTO_LINK_SHARED_TAG_THRESHOLD: f32 = 0.45;
/// Most automatic semantic links per memory.
pub const MAX_AUTO_LINKS: usize = 15;
/// Most automatic entity links per memory.
pub const MAX_ENTITY_LINKS: usize = 10;
/// Most automatic temporal links per memory.
pub const MAX_TEMPORAL_LINKS: usize = 20;
/// Window of a temporal link.
pub const TEMPORAL_WINDOW_MS: i64 = 3_600_000;
/// Lowest weight of a temporal link.
pub const MIN_TEMPORAL_WEIGHT: f32 = 0.05;
/// Retrieval-induced forgetting: activation below which nothing is suppressed.
pub const RIF_ACTIVATION_LOW: f32 = 0.1;
/// Retrieval-induced forgetting: activation from which the multiplier rises again.
pub const RIF_ACTIVATION_HIGH: f32 = 0.45;
/// Retrieval-induced forgetting: deepest suppression.
pub const RIF_MAX_SUPPRESSION: f32 = 0.15;
/// Hours between sweeps of an open store.
pub const SWEEP_INTERVAL_HOURS: u32 = 24;
/// Most hops followed along a `Supersedes` chain.
pub const MAX_SUPERSEDE_HOPS: usize = 10;
/// Cosine from which a memory counts as a near duplicate.
pub const DUPLICATE_THRESHOLD: f32 = 0.85;
/// Most near duplicates reported.
pub const MAX_NEAR_DUPLICATES: usize = 3;
/// Longest near-duplicate summary, the ellipsis included.
pub const NEAR_DUPLICATE_SUMMARY_MAX_BYTES: usize = 240;
/// Failures after which a memory leaves the embedding queue.
pub const MAX_EMBED_FAILURES: u8 = 3;
/// Lowest norm of an accepted vector.
pub const VECTOR_NORM_MIN: f32 = 0.999;
/// Highest norm of an accepted vector.
pub const VECTOR_NORM_MAX: f32 = 1.001;
/// Longest idempotency key.
pub const IDEMPOTENCY_KEY_MAX_BYTES: usize = 128;
/// How long an idempotency entry is kept.
pub const IDEMPOTENCY_TTL_MS: i64 = 24 * 3_600_000;
/// Most idempotency entries kept; the oldest go first.
pub const IDEMPOTENCY_MAX_ENTRIES: usize = 10_000;

// ── Validation ──────────────────────────────────────────────────────

/// The client-supplied content of a new memory, borrowed from a request.
#[derive(Clone, Copy)]
pub struct ContentRef<'a> {
    /// The summary.
    pub summary: &'a str,
    /// The full text.
    pub full_text: Option<&'a str>,
    /// Tags as sent, before lower-casing.
    pub tags: &'a [String],
    /// Entities.
    pub entities: &'a [String],
    /// Topics.
    pub topics: &'a [String],
    /// Emotions.
    pub emotions: &'a [String],
    /// The requested creation time.
    pub created_at: Option<Timestamp>,
}

impl std::fmt::Debug for ContentRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentRef")
            .field("summary_bytes", &self.summary.len())
            .field("tags", &self.tags.len())
            .finish_non_exhaustive()
    }
}

/// Steps 1 to 6 of the save validation, in order. Returns the parsed tags.
///
/// Steps 7 and 8, the namespace and the parent, need the store.
pub fn validate_content(content: ContentRef<'_>, now: Timestamp) -> Result<Vec<Tag>, EngineError> {
    validate_content_within(content, now, FULL_TEXT_MAX_BYTES)
}

/// [`validate_content`] with another full-text limit: an import line may carry a
/// longer full text than a save, and is otherwise checked the same, in the same order.
pub fn validate_content_within(
    content: ContentRef<'_>,
    now: Timestamp,
    full_text_max_bytes: usize,
) -> Result<Vec<Tag>, EngineError> {
    if content.summary.is_empty() {
        return Err(EngineError::invalid("summary", "summary must not be empty"));
    }
    validate_text_len("summary", content.summary, SUMMARY_MAX_BYTES)?;
    if let Some(full_text) = content.full_text {
        if full_text.is_empty() {
            return Err(EngineError::invalid(
                "fullText",
                "fullText must not be empty; leave it out instead",
            ));
        }
        validate_text_len("fullText", full_text, full_text_max_bytes)?;
    }
    let tags = validate_tags("tags", content.tags)?;
    for (field, items, max_items, max_bytes) in [
        ("entities", content.entities, MAX_ENTITIES, ENTITY_MAX_BYTES),
        ("topics", content.topics, MAX_TOPICS, TOPIC_MAX_BYTES),
        (
            "emotions",
            content.emotions,
            MAX_EMOTIONS,
            EMOTION_MAX_BYTES,
        ),
    ] {
        validate_labels(field, items, max_items, max_bytes)?;
    }
    if let Some(created_at) = content.created_at {
        validate_created_at(created_at, now)?;
    }
    markup::reject_client_markup("summary", content.summary)?;
    if let Some(full_text) = content.full_text {
        markup::reject_client_markup("fullText", full_text)?;
    }
    Ok(tags)
}

/// A requested `createdAt` lies between 1970-01-01 and one day after `now`.
pub fn validate_created_at(created_at: Timestamp, now: Timestamp) -> Result<(), EngineError> {
    let latest = now.millis().checked_add(MILLIS_PER_DAY).ok_or_else(|| {
        EngineError::invalid("createdAt", "createdAt cannot be checked against this time")
    })?;
    if created_at < Timestamp::MIN_API || created_at.millis() > latest {
        return Err(EngineError::invalid(
            "createdAt",
            "createdAt must be between 1970-01-01 and one day from now",
        ));
    }
    Ok(())
}

/// At most `MAX_TAGS` tags, each a valid tag. Returns them lower-cased.
pub fn validate_tags(field: &'static str, raw: &[String]) -> Result<Vec<Tag>, EngineError> {
    validate_count(field, raw.len(), MAX_TAGS)?;
    validate_item_lens(field, raw, TAG_MAX_BYTES)?;
    raw.iter()
        .enumerate()
        .map(|(index, text)| {
            Tag::new(text.as_str()).map_err(|e| {
                EngineError::invalid(field, format!("{field}[{index}] is invalid: {e}"))
            })
        })
        .collect()
}

/// At most `max_items` labels, each 1 byte to `max_bytes`. Any text is a label.
pub fn validate_labels(
    field: &'static str,
    items: &[String],
    max_items: usize,
    max_bytes: usize,
) -> Result<(), EngineError> {
    validate_count(field, items.len(), max_items)?;
    validate_item_lens(field, items, max_bytes)?;
    if let Some(index) = items.iter().position(String::is_empty) {
        return Err(EngineError::invalid(
            field,
            format!("{field}[{index}] must not be empty"),
        ));
    }
    Ok(())
}

/// A clarifying sentence when bytes and characters differ (recalld `utf8_hint`).
fn utf8_hint(bytes: usize, chars: usize) -> &'static str {
    if bytes == chars {
        ""
    } else {
        " Limits are measured in UTF-8 bytes, not characters: non-ASCII characters such as \
         em dashes and curly quotes count as 2-4 bytes each."
    }
}

/// `value` fits in `max_bytes`.
pub fn validate_text_len(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), EngineError> {
    let bytes = value.len();
    if bytes > max_bytes {
        let chars = value.chars().count();
        return Err(EngineError::invalid(
            field,
            format!(
                "{field} is {bytes} bytes ({chars} characters), which exceeds the \
                 {max_bytes}-byte limit.{}",
                utf8_hint(bytes, chars)
            ),
        ));
    }
    Ok(())
}

/// An array field holds at most `max` items.
pub fn validate_count(field: &'static str, count: usize, max: usize) -> Result<(), EngineError> {
    if count > max {
        return Err(EngineError::invalid(
            field,
            format!("{field} has {count} items, which exceeds the maximum of {max}"),
        ));
    }
    Ok(())
}

/// Every element of an array field fits in `max_bytes`; the first that does not is named.
pub fn validate_item_lens(
    field: &'static str,
    items: &[String],
    max_bytes: usize,
) -> Result<(), EngineError> {
    for (index, item) in items.iter().enumerate() {
        let bytes = item.len();
        if bytes > max_bytes {
            let chars = item.chars().count();
            return Err(EngineError::invalid(
                field,
                format!(
                    "{field}[{index}] is {bytes} bytes ({chars} characters), which exceeds the \
                     {max_bytes}-byte limit.{}",
                    utf8_hint(bytes, chars)
                ),
            ));
        }
    }
    Ok(())
}

/// A namespace name: 1 to 64 bytes of ASCII letters, digits, `-` and `_`.
pub fn validate_namespace_name(field: &'static str, name: &str) -> Result<(), EngineError> {
    let valid = !name.is_empty()
        && name.len() <= NAMESPACE_NAME_MAX_BYTES
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_');
    if valid {
        Ok(())
    } else {
        Err(EngineError::invalid(
            field,
            format!(
                "{field} must be 1-{NAMESPACE_NAME_MAX_BYTES} bytes of ASCII letters, digits, \
                 hyphens or underscores"
            ),
        ))
    }
}

/// The longest prefix of `text` of at most `max_bytes` that ends on a character boundary.
pub fn truncate_on_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

/// A near-duplicate summary: whole when it fits, else cut so that text plus `…` fit
/// (recalld `truncate_summary`).
pub fn truncate_summary(summary: &str, max_bytes: usize) -> String {
    const ELLIPSIS: &str = "\u{2026}";
    if summary.len() <= max_bytes {
        return summary.to_owned();
    }
    let cut = truncate_on_char_boundary(summary, max_bytes.saturating_sub(ELLIPSIS.len()));
    format!("{cut}{ELLIPSIS}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from recalld `src/model/validation.rs`, `src/model/duplicates.rs`.

    fn input(summary: &str) -> ContentRef<'_> {
        ContentRef {
            summary,
            full_text: None,
            tags: &[],
            entities: &[],
            topics: &[],
            emotions: &[],
            created_at: None,
        }
    }

    fn check(c: ContentRef<'_>) -> Result<Vec<Tag>, EngineError> {
        validate_content(c, Timestamp::from_millis(1_700_000_000_000))
    }

    fn strings(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("t{i}")).collect()
    }

    const HINT: &str = "Limits are measured in UTF-8 bytes, not characters";

    fn msg(e: EngineError) -> String {
        e.to_string()
    }

    #[test]
    fn em_dash_summary_reports_both_units_and_hint() {
        let summary = "—".repeat(1900);
        assert_eq!(summary.len(), 5700);
        let err = check(input(&summary)).unwrap_err();
        assert_eq!(err.field(), Some("summary"));
        let m = msg(err);
        assert!(m.contains("5700 bytes (1900 characters)"), "{m}");
        assert!(m.contains("2000-byte limit"), "{m}");
        assert!(m.contains(HINT), "{m}");
    }

    #[test]
    fn ascii_limits() {
        assert!(check(input(&"a".repeat(1900))).is_ok());
        let m = msg(check(input(&"a".repeat(2100))).unwrap_err());
        assert!(m.contains("2100 bytes (2100 characters)"), "{m}");
        assert!(!m.contains(HINT), "{m}");
        assert!(check(input(&"a".repeat(2000))).is_ok());
        assert!(check(input(&"a".repeat(2001))).is_err());
    }

    #[test]
    fn em_dash_boundary_counts_bytes_not_chars() {
        assert!(check(input(&"—".repeat(666))).is_ok());
        let m = msg(check(input(&"—".repeat(667))).unwrap_err());
        assert!(m.contains("2001 bytes (667 characters)"), "{m}");
        assert!(check(input(&"🧠".repeat(500))).is_ok());
    }

    #[test]
    fn empty_summary_and_full_text_are_rejected() {
        let err = check(input("")).unwrap_err();
        assert_eq!(err.field(), Some("summary"));
        let mut c = input("ok");
        c.full_text = Some("");
        assert_eq!(check(c).unwrap_err().field(), Some("fullText"));
    }

    #[test]
    fn full_text_boundary() {
        let at = "a".repeat(FULL_TEXT_MAX_BYTES);
        let mut c = input("ok");
        c.full_text = Some(&at);
        assert!(check(c).is_ok());
        let over = "a".repeat(FULL_TEXT_MAX_BYTES + 1);
        c.full_text = Some(&over);
        let err = check(c).unwrap_err();
        assert_eq!(err.field(), Some("fullText"));
        assert!(msg(err).contains("65536-byte limit"));
    }

    fn with_field<'a>(summary: &'a str, field: &str, items: &'a [String]) -> ContentRef<'a> {
        let mut c = input(summary);
        match field {
            "tags" => c.tags = items,
            "entities" => c.entities = items,
            "topics" => c.topics = items,
            _ => c.emotions = items,
        }
        c
    }

    #[test]
    fn array_count_limits() {
        for (field, max) in [
            ("tags", MAX_TAGS),
            ("entities", MAX_ENTITIES),
            ("topics", MAX_TOPICS),
            ("emotions", MAX_EMOTIONS),
        ] {
            let at = strings(max);
            assert!(check(with_field("ok", field, &at)).is_ok(), "{field}");
            let over = strings(max + 1);
            let err = check(with_field("ok", field, &over)).unwrap_err();
            assert_eq!(err.field(), Some(field));
            let m = msg(err);
            assert!(
                m.contains(&format!(
                    "has {} items, which exceeds the maximum of {max}",
                    max + 1
                )),
                "{m}"
            );
        }
    }

    const ITEM_LIMITS: [(&str, usize); 4] = [
        ("tags", TAG_MAX_BYTES),
        ("entities", ENTITY_MAX_BYTES),
        ("topics", TOPIC_MAX_BYTES),
        ("emotions", EMOTION_MAX_BYTES),
    ];

    #[test]
    fn item_length_boundary_is_inclusive() {
        for (field, max) in ITEM_LIMITS {
            let at = vec!["a".repeat(max)];
            assert!(check(with_field("ok", field, &at)).is_ok(), "{field}");
            let over = vec!["a".repeat(max + 1)];
            let err = check(with_field("ok", field, &over)).unwrap_err();
            assert_eq!(err.field(), Some(field));
            let m = msg(err);
            assert!(
                m.contains(&format!("{field}[0] is {} bytes", max + 1)),
                "{m}"
            );
            assert!(m.contains(&format!("{max}-byte limit")), "{m}");
        }
    }

    #[test]
    fn item_error_names_the_offending_index() {
        let items = vec![
            "fine".into(),
            "also-fine".into(),
            "x".repeat(TAG_MAX_BYTES + 1),
        ];
        let m = msg(check(with_field("ok", "tags", &items)).unwrap_err());
        assert!(m.contains("tags[2] is 129 bytes"), "{m}");
        assert!(m.contains("128-byte limit"), "{m}");
    }

    #[test]
    fn item_limit_counts_bytes_not_characters() {
        let items = vec!["—".repeat(61)];
        let m = msg(check(with_field("ok", "entities", &items)).unwrap_err());
        assert!(
            m.contains("entities[0] is 183 bytes (61 characters)"),
            "{m}"
        );
        assert!(m.contains(HINT), "{m}");
    }

    #[test]
    fn labels_at_limit_in_multibyte_characters_are_accepted() {
        let items = vec!["—".repeat(40)];
        for field in ["entities", "topics", "emotions"] {
            assert!(check(with_field("ok", field, &items)).is_ok(), "{field}");
        }
    }

    #[test]
    fn labels_are_any_text_but_not_empty() {
        let items = vec!["Maya Smith".into(), "José".into()];
        assert!(check(with_field("ok", "entities", &items)).is_ok());
        let items = vec!["ok".into(), String::new()];
        let err = check(with_field("ok", "topics", &items)).unwrap_err();
        assert_eq!(msg(err), "topics[1] must not be empty");
    }

    #[test]
    fn invalid_tags_are_rejected_and_valid_ones_lower_cased() {
        let items = vec!["Travel".into()];
        assert_eq!(
            check(with_field("ok", "tags", &items)).unwrap()[0].as_str(),
            "travel"
        );
        let items = vec!["ok".into(), "no spaces".into()];
        let m = msg(check(with_field("ok", "tags", &items)).unwrap_err());
        assert!(m.starts_with("tags[1] is invalid"), "{m}");
    }

    #[test]
    fn count_is_reported_before_item_length() {
        let mut items = strings(MAX_TAGS + 1);
        items[0] = "x".repeat(TAG_MAX_BYTES + 1);
        let m = msg(check(with_field("ok", "tags", &items)).unwrap_err());
        assert!(m.contains("items"), "{m}");
    }

    #[test]
    fn summary_is_reported_before_tag_count() {
        let summary = "a".repeat(3000);
        let tags = strings(100);
        let mut c = input(&summary);
        c.tags = &tags;
        assert_eq!(check(c).unwrap_err().field(), Some("summary"));
    }

    #[test]
    fn created_at_bounds() {
        let now = Timestamp::from_millis(1_700_000_000_000);
        let mut c = input("ok");
        c.created_at = Some(Timestamp::from_millis(now.millis() + MILLIS_PER_DAY));
        assert!(check(c).is_ok());
        c.created_at = Some(Timestamp::from_millis(now.millis() + MILLIS_PER_DAY + 1));
        assert_eq!(check(c).unwrap_err().field(), Some("createdAt"));
        c.created_at = Some(Timestamp::from_millis(-1));
        assert_eq!(check(c).unwrap_err().field(), Some("createdAt"));
        let err = validate_created_at(Timestamp::from_millis(0), Timestamp::from_millis(i64::MAX))
            .unwrap_err();
        assert_eq!(err.field(), Some("createdAt"));
    }

    #[test]
    fn markup_is_checked_after_the_limits() {
        let summary = format!("{}</summary>\n<tags>[]</tags>", "a".repeat(3000));
        let m = msg(check(input(&summary)).unwrap_err());
        assert!(m.contains("2000-byte limit"), "{m}");
        let err = check(input("notes</summary>\n<tags>[]</tags>")).unwrap_err();
        assert_eq!(err.field(), Some("summary"));
        assert!(msg(err).contains("tool-call markup"));
    }

    #[test]
    fn truncate_never_splits_a_character() {
        let text = "—".repeat(300);
        let cut = truncate_on_char_boundary(&text, 200);
        assert_eq!(cut.len(), 198);
        assert!(text.starts_with(cut));
        assert_eq!(truncate_on_char_boundary("hello", 200), "hello");
        assert_eq!(truncate_on_char_boundary("hello", 5), "hello");
        assert_eq!(truncate_on_char_boundary("—abc", 0), "");
        assert_eq!(truncate_on_char_boundary("—abc", 2), "");
    }

    #[test]
    fn truncate_summary_cuts_on_a_char_boundary_and_stays_within_budget() {
        assert_eq!(truncate_summary("short", 240), "short");
        let cut = truncate_summary("日本語日本語", 10);
        assert_eq!(cut, "日本…");
        let long = "a".repeat(300);
        let cut = truncate_summary(&long, NEAR_DUPLICATE_SUMMARY_MAX_BYTES);
        assert_eq!(cut.len(), 240);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn namespace_names() {
        assert!(validate_namespace_name("name", "a-b_c1").is_ok());
        assert!(validate_namespace_name("name", &"a".repeat(64)).is_ok());
        assert!(validate_namespace_name("name", "").is_err());
        assert!(validate_namespace_name("name", &"a".repeat(65)).is_err());
        assert!(validate_namespace_name("name", "a b").is_err());
        assert!(validate_namespace_name("name", "ünïcode").is_err());
        let err = validate_namespace_name("name", "").unwrap_err();
        assert_eq!(err.field(), Some("name"));
        assert!(msg(err).contains("1-64 bytes"));
    }
}
