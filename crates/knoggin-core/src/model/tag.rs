// SPDX-License-Identifier: AGPL-3.0-only
//! The validated, lower-cased tag. Ported from recalld `src/model/tag.rs`.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::limits::TAG_MAX_BYTES;

/// A tag: lower-cased, 1 to 128 bytes, starting with an ASCII letter or digit,
/// and using only `a-z 0-9 / _ . : -`.
#[derive(
    Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema, ts_rs::TS,
)]
#[serde(try_from = "String", into = "String")]
#[schema(value_type = String)]
#[ts(type = "string")]
pub struct Tag(String);

/// Why a text is not a tag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    /// The text is empty.
    #[error("tag must not be empty")]
    Empty,
    /// The text is longer than the limit.
    #[error("tag is {len} bytes, max is {max}")]
    TooLong {
        /// Bytes after lower-casing.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The first character is not an ASCII letter or digit.
    #[error("tag must start with an alphanumeric character, got '{0}'")]
    InvalidStartChar(char),
    /// A character outside the allowed set.
    #[error("tag contains disallowed character '{0}'")]
    InvalidChar(char),
}

impl Tag {
    /// Lower-cases and validates a tag.
    pub fn new(text: impl Into<String>) -> Result<Self, TagError> {
        let s = text.into().to_lowercase();
        let Some(&first) = s.as_bytes().first() else {
            return Err(TagError::Empty);
        };
        if s.len() > TAG_MAX_BYTES {
            return Err(TagError::TooLong {
                len: s.len(),
                max: TAG_MAX_BYTES,
            });
        }
        if !first.is_ascii_alphanumeric() {
            return Err(TagError::InvalidStartChar(char::from(first)));
        }
        if let Some(bad) = s
            .chars()
            .find(|c| !matches!(c, 'a'..='z' | '0'..='9' | '-' | '_' | '/' | '.' | ':'))
        {
            return Err(TagError::InvalidChar(bad));
        }
        Ok(Self(s))
    }

    /// The tag text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Tag {
    /// Tags are user text, so `Debug` prints only the length.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tag({} bytes)", self.0.len())
    }
}

impl TryFrom<String> for Tag {
    type Error = TagError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Tag::new(s)
    }
}

impl From<Tag> for String {
    fn from(tag: Tag) -> Self {
        tag.0
    }
}

impl AsRef<str> for Tag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_lower_cased_and_checked() {
        assert_eq!(Tag::new("Topic/Rust").unwrap().as_str(), "topic/rust");
        assert_eq!(Tag::new(""), Err(TagError::Empty));
        assert_eq!(Tag::new("-x"), Err(TagError::InvalidStartChar('-')));
        assert_eq!(Tag::new("a b"), Err(TagError::InvalidChar(' ')));
        assert_eq!(Tag::new("josé"), Err(TagError::InvalidChar('é')));
        assert!(Tag::new("a".repeat(128)).is_ok());
        assert_eq!(
            Tag::new("a".repeat(129)),
            Err(TagError::TooLong { len: 129, max: 128 })
        );
        assert!(Tag::new("v1.2:x_y-z/w").is_ok());
    }

    #[test]
    fn serde_validates() {
        let t: Tag = serde_json::from_str("\"Travel\"").unwrap();
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"travel\"");
        assert!(serde_json::from_str::<Tag>("\"no spaces\"").is_err());
        assert_eq!(format!("{t:?}"), "Tag(6 bytes)");
    }
}
