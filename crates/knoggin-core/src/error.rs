// SPDX-License-Identifier: AGPL-3.0-only
//! `EngineError`: one variant per error code the engine can cause.

use std::fmt;

use serde::Serialize;

/// The error codes the engine can cause, a subset of the API's closed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, utoipa::ToSchema, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A value is malformed or out of range.
    InvalidArgument,
    /// No such memory or namespace.
    NotFound,
    /// A namespace with that name exists.
    AlreadyExists,
    /// The request cannot be done in the current state.
    Conflict,
    /// Deleting a namespace that has memories, without `force`.
    NamespaceNotEmpty,
    /// The idempotency key was used with a different request.
    IdempotencyConflict,
}

impl ErrorCode {
    /// The code as the API writes it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::NotFound => "not_found",
            Self::AlreadyExists => "already_exists",
            Self::Conflict => "conflict",
            Self::NamespaceNotEmpty => "namespace_not_empty",
            Self::IdempotencyConflict => "idempotency_conflict",
        }
    }
}

/// An operation that failed. A failed write leaves the store unchanged.
///
/// `Display` is the message for the client. It can name what the client sent,
/// a namespace name for example, so `Debug` prints the code and field only:
/// log the code, never the message.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// A value is malformed or out of range. `field` is the camelCase request field.
    #[error("{message}")]
    InvalidArgument {
        /// The request field at fault.
        field: &'static str,
        /// What is wrong, for people.
        message: String,
    },
    /// No such memory or namespace.
    #[error("{0}")]
    NotFound(String),
    /// A namespace with that name exists.
    #[error("{0}")]
    AlreadyExists(String),
    /// The request cannot be done in the current state.
    #[error("{0}")]
    Conflict(String),
    /// The namespace still has memories and `force` was not set.
    #[error("{0}")]
    NamespaceNotEmpty(String),
    /// The idempotency key was used with a different request.
    #[error("the idempotency key was used with a different request")]
    IdempotencyConflict,
}

impl EngineError {
    /// Shorthand for `InvalidArgument`.
    pub fn invalid(field: &'static str, message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            field,
            message: message.into(),
        }
    }

    /// The API error code.
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidArgument { .. } => ErrorCode::InvalidArgument,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::AlreadyExists(_) => ErrorCode::AlreadyExists,
            Self::Conflict(_) => ErrorCode::Conflict,
            Self::NamespaceNotEmpty(_) => ErrorCode::NamespaceNotEmpty,
            Self::IdempotencyConflict => ErrorCode::IdempotencyConflict,
        }
    }

    /// The request field at fault, when one is.
    pub const fn field(&self) -> Option<&'static str> {
        match self {
            Self::InvalidArgument { field, .. } => Some(field),
            _ => None,
        }
    }
}

impl fmt::Debug for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("EngineError");
        d.field("code", &self.code().as_str());
        if let Some(field) = self.field() {
            d.field("field", &field);
        }
        d.finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_message() {
        let e = EngineError::NotFound("namespace 'marker-secret' not found".into());
        assert_eq!(e.to_string(), "namespace 'marker-secret' not found");
        assert!(!format!("{e:?}").contains("marker-secret"));
        let e = EngineError::invalid("summary", "marker-secret");
        assert_eq!(
            format!("{e:?}"),
            r#"EngineError { code: "invalid_argument", field: "summary", .. }"#
        );
        assert_eq!(e.code(), ErrorCode::InvalidArgument);
        assert_eq!(
            serde_json::to_string(&ErrorCode::NamespaceNotEmpty).unwrap(),
            "\"namespace_not_empty\""
        );
    }
}
