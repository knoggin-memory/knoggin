// SPDX-License-Identifier: AGPL-3.0-only
//! `ApiError` and the closed list of error codes (tab "REST API", Errors).
//!
//! Every error a client sees is one `ApiError`. Its response carries the error itself in
//! the response extensions, so that an outer layer can fill in the request ID
//! ([`fill_request_id`]) and log or count the code without parsing the body.

use std::fmt;

use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use knoggin_core::{EngineError, ErrorCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The 17 error codes. This list is closed; the SDK mirrors it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// 400 (405 for a wrong method).
    InvalidArgument,
    /// 401.
    Unauthenticated,
    /// 403.
    PermissionDenied,
    /// 404.
    NotFound,
    /// 409.
    AlreadyExists,
    /// 409.
    Conflict,
    /// 409.
    IdempotencyConflict,
    /// 409.
    NamespaceNotEmpty,
    /// 410.
    StoreDeleting,
    /// 413.
    PayloadTooLarge,
    /// 423, with a `reason`.
    StoreLocked,
    /// 429.
    RateLimited,
    /// 500, always "Internal error".
    Internal,
    /// 503.
    Unavailable,
    /// 503.
    Upgrading,
    /// 504.
    Timeout,
    /// 507.
    ResourceExhausted,
}

impl Code {
    /// Every code, in the order of the table.
    pub const ALL: [Code; 17] = [
        Self::InvalidArgument,
        Self::Unauthenticated,
        Self::PermissionDenied,
        Self::NotFound,
        Self::AlreadyExists,
        Self::Conflict,
        Self::IdempotencyConflict,
        Self::NamespaceNotEmpty,
        Self::StoreDeleting,
        Self::PayloadTooLarge,
        Self::StoreLocked,
        Self::RateLimited,
        Self::Internal,
        Self::Unavailable,
        Self::Upgrading,
        Self::Timeout,
        Self::ResourceExhausted,
    ];

    /// The code as the API writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::Unauthenticated => "unauthenticated",
            Self::PermissionDenied => "permission_denied",
            Self::NotFound => "not_found",
            Self::AlreadyExists => "already_exists",
            Self::Conflict => "conflict",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::NamespaceNotEmpty => "namespace_not_empty",
            Self::StoreDeleting => "store_deleting",
            Self::PayloadTooLarge => "payload_too_large",
            Self::StoreLocked => "store_locked",
            Self::RateLimited => "rate_limited",
            Self::Internal => "internal",
            Self::Unavailable => "unavailable",
            Self::Upgrading => "upgrading",
            Self::Timeout => "timeout",
            Self::ResourceExhausted => "resource_exhausted",
        }
    }

    /// The HTTP status of the code.
    pub fn status(self) -> StatusCode {
        match self {
            Self::InvalidArgument => StatusCode::BAD_REQUEST,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::PermissionDenied => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::AlreadyExists
            | Self::Conflict
            | Self::IdempotencyConflict
            | Self::NamespaceNotEmpty => StatusCode::CONFLICT,
            Self::StoreDeleting => StatusCode::GONE,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::StoreLocked => StatusCode::LOCKED,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Unavailable | Self::Upgrading => StatusCode::SERVICE_UNAVAILABLE,
            Self::Timeout => StatusCode::GATEWAY_TIMEOUT,
            Self::ResourceExhausted => StatusCode::INSUFFICIENT_STORAGE,
        }
    }
}

impl From<ErrorCode> for Code {
    fn from(code: ErrorCode) -> Self {
        match code {
            ErrorCode::InvalidArgument => Self::InvalidArgument,
            ErrorCode::NotFound => Self::NotFound,
            ErrorCode::AlreadyExists => Self::AlreadyExists,
            ErrorCode::Conflict => Self::Conflict,
            ErrorCode::NamespaceNotEmpty => Self::NamespaceNotEmpty,
            ErrorCode::IdempotencyConflict => Self::IdempotencyConflict,
        }
    }
}

/// Why a store cannot be opened: the `reason` of `store_locked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockReason {
    /// The master key that wrapped the data key is not available.
    MasterKeyMissing,
    /// No snapshot of the store can be read.
    SnapshotUnreadable,
    /// The snapshot was written by a newer build.
    FormatTooNew,
    /// The store does not fit in its container.
    OutOfMemory,
    /// Local install: the daemon's state file cannot be read.
    StateUnreadable,
    /// Local install: snapshot files exist without a state file.
    StateMissing,
}

impl LockReason {
    /// The reason as the API writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MasterKeyMissing => "master_key_missing",
            Self::SnapshotUnreadable => "snapshot_unreadable",
            Self::FormatTooNew => "format_too_new",
            Self::OutOfMemory => "out_of_memory",
            Self::StateUnreadable => "state_unreadable",
            Self::StateMissing => "state_missing",
        }
    }
}

/// The message of every `internal` error.
pub const INTERNAL_MESSAGE: &str = "Internal error";

/// One error as a client sees it.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiError {
    /// The code clients switch on.
    pub code: Code,
    /// For people. May quote the client's own input, never another memory's content.
    pub message: String,
    /// The one request field at fault.
    pub field: Option<String>,
    /// A fixed word, only where the table gives one.
    pub reason: Option<&'static str>,
    /// Only on `store_locked` from `GET /v1/session` on a locked local install.
    pub docs_url: Option<String>,
    /// Whole seconds for `Retry-After`.
    pub retry_after: Option<u32>,
    /// Filled in by the layer that knows it.
    pub request_id: Option<Uuid>,
    /// A wrong HTTP method: `invalid_argument` with status 405.
    pub method_not_allowed: bool,
}

// The message can quote what the client sent, a namespace name for example.
impl fmt::Debug for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiError")
            .field("code", &self.code.as_str())
            .field("field", &self.field)
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Body<'a> {
    code: Code,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    docs_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<Uuid>,
}

#[derive(Serialize)]
struct Envelope<'a> {
    error: Body<'a>,
}

impl ApiError {
    /// An error with a code and a message.
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            field: None,
            reason: None,
            docs_url: None,
            retry_after: None,
            request_id: None,
            method_not_allowed: false,
        }
    }

    /// `invalid_argument` on one field.
    pub fn invalid_argument(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Code::InvalidArgument, message).with_field(field)
    }

    /// `permission_denied`.
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(Code::PermissionDenied, message)
    }

    /// `not_found`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Code::NotFound, message)
    }

    /// `internal`, with the one fixed message. Log the cause at the call site, by kind.
    pub fn internal() -> Self {
        Self::new(Code::Internal, INTERNAL_MESSAGE)
    }

    /// `unavailable` with `Retry-After`.
    pub fn unavailable(retry_after: u32) -> Self {
        Self::new(Code::Unavailable, "The service is busy; try again shortly")
            .with_retry_after(retry_after)
    }

    /// `store_locked` with its reason.
    pub fn store_locked(reason: LockReason) -> Self {
        let mut e = Self::new(Code::StoreLocked, "The store cannot be opened");
        e.reason = Some(reason.as_str());
        e
    }

    /// `store_deleting`.
    pub fn store_deleting() -> Self {
        Self::new(Code::StoreDeleting, "The store is scheduled for deletion")
    }

    /// `resource_exhausted`: the store has reached the memory available to it.
    pub fn resource_exhausted() -> Self {
        Self::new(
            Code::ResourceExhausted,
            "The store has reached the memory available to it; forget memories to make room",
        )
    }

    /// `payload_too_large`.
    pub fn payload_too_large() -> Self {
        Self::new(Code::PayloadTooLarge, "The request body is over the limit")
    }

    /// `timeout`.
    pub fn timeout() -> Self {
        Self::new(Code::Timeout, "The request ran past its time limit")
    }

    /// A wrong HTTP method: `invalid_argument` with status 405.
    pub fn method_not_allowed() -> Self {
        let mut e = Self::new(
            Code::InvalidArgument,
            "This method is not allowed on this route",
        );
        e.method_not_allowed = true;
        e
    }

    /// Sets `field`.
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Sets `Retry-After`.
    pub fn with_retry_after(mut self, seconds: u32) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    /// Sets `reason`.
    pub fn with_reason(mut self, reason: &'static str) -> Self {
        self.reason = Some(reason);
        self
    }

    /// Sets the request ID.
    pub fn with_request_id(mut self, id: Uuid) -> Self {
        self.request_id = Some(id);
        self
    }

    /// The HTTP status.
    pub fn status(&self) -> StatusCode {
        if self.method_not_allowed {
            StatusCode::METHOD_NOT_ALLOWED
        } else {
            self.code.status()
        }
    }

    /// The JSON body.
    pub fn body(&self) -> Vec<u8> {
        let envelope = Envelope {
            error: Body {
                code: self.code,
                message: if self.code == Code::Internal {
                    INTERNAL_MESSAGE
                } else {
                    &self.message
                },
                field: self.field.as_deref(),
                reason: self.reason,
                docs_url: self.docs_url.as_deref(),
                request_id: self.request_id,
            },
        };
        // A struct of strings and enums always serializes.
        serde_json::to_vec(&envelope).unwrap_or_default()
    }

    /// The JSON error for a JoinError of a blocking task or a spawned task: a panic is
    /// `internal`, anything else a cancel at shutdown (`unavailable`). The JoinError itself
    /// is never formatted: its text holds the panic message.
    pub fn from_join(error: &tokio::task::JoinError) -> Self {
        if error.is_panic() {
            tracing::error!(kind = "panic", "a store task panicked");
            Self::internal()
        } else {
            Self::unavailable(1)
        }
    }

    /// The error for a response that a library layer produced without one: a body limit
    /// (413), the request timeout (504 or 408), a wrong method (405) or a panic (500).
    pub fn from_bare_status(status: StatusCode) -> Option<Self> {
        match status {
            StatusCode::PAYLOAD_TOO_LARGE => Some(Self::payload_too_large()),
            StatusCode::GATEWAY_TIMEOUT | StatusCode::REQUEST_TIMEOUT => Some(Self::timeout()),
            StatusCode::METHOD_NOT_ALLOWED => Some(Self::method_not_allowed()),
            StatusCode::NOT_FOUND => Some(Self::not_found("No such route")),
            StatusCode::INTERNAL_SERVER_ERROR => Some(Self::internal()),
            _ => None,
        }
    }
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        let mut out = Self::new(e.code().into(), e.to_string());
        out.field = e.field().map(str::to_owned);
        out
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status(), self.body()).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Some(seconds) = self.retry_after {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response.extensions_mut().insert(self);
        response
    }
}

/// Re-renders an error response with the request ID when its error has none yet. Every
/// response that carries an `ApiError` (any made by its `IntoResponse`) qualifies.
pub fn fill_request_id(response: Response, id: Uuid) -> Response {
    match response.extensions().get::<ApiError>() {
        Some(e) if e.request_id.is_none() => {
            let e = e.clone().with_request_id(id);
            let mut fresh = e.into_response();
            // Keep headers a layer below set, such as `Allow` on a 405.
            for (name, value) in response.headers() {
                if !fresh.headers().contains_key(name) {
                    fresh.headers_mut().insert(name, value.clone());
                }
            }
            fresh
        }
        _ => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_statuses() {
        assert_eq!(Code::ALL.len(), 17);
        for code in Code::ALL {
            let json = serde_json::to_value(code).unwrap();
            assert_eq!(json, code.as_str());
        }
        assert_eq!(
            ApiError::method_not_allowed().status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(Code::ResourceExhausted.status().as_u16(), 507);
        assert_eq!(Code::StoreLocked.status().as_u16(), 423);
    }

    #[test]
    fn the_body_has_the_shape_and_internal_hides_its_message() {
        let e = ApiError::invalid_argument("summary", "too long").with_request_id(Uuid::nil());
        let v: serde_json::Value = serde_json::from_slice(&e.body()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "error": { "code": "invalid_argument", "message": "too long",
                "field": "summary", "requestId": "00000000-0000-0000-0000-000000000000" } })
        );
        let mut e = ApiError::internal();
        e.message = "marker".into();
        assert!(!String::from_utf8(e.body()).unwrap().contains("marker"));
        let locked = ApiError::store_locked(LockReason::FormatTooNew);
        let v: serde_json::Value = serde_json::from_slice(&locked.body()).unwrap();
        assert_eq!(v["error"]["reason"], "format_too_new");
    }

    #[test]
    fn engine_errors_map_with_their_field() {
        let e: ApiError = EngineError::invalid("query", "marker").into();
        assert_eq!(
            (e.code, e.field.as_deref()),
            (Code::InvalidArgument, Some("query"))
        );
        assert!(!format!("{e:?}").contains("marker"));
        let e: ApiError = EngineError::IdempotencyConflict.into();
        assert_eq!(e.code, Code::IdempotencyConflict);
    }

    #[test]
    fn fill_request_id_keeps_headers() {
        let mut r = ApiError::method_not_allowed().into_response();
        r.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET"));
        let id = Uuid::now_v7();
        let r = fill_request_id(r, id);
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(r.headers()[header::ALLOW], "GET");
        assert_eq!(
            r.extensions().get::<ApiError>().unwrap().request_id,
            Some(id)
        );
    }
}
