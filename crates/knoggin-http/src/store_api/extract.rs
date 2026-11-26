// SPDX-License-Identifier: AGPL-3.0-only
//! Strict request parsing: JSON bodies with their content type, query parameters where an
//! unknown one is an error, path IDs, the idempotency key and the request context.

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use http::request::Parts;
use http::{HeaderMap, header};
use knoggin_core::{MemoryId, Timestamp};
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::ctx::{RequestContext, headers};
use crate::error::ApiError;

/// The request context the edge layer put in the extensions.
#[derive(Debug, Clone)]
pub(crate) struct Ctx(pub RequestContext);

impl<S: Send + Sync> FromRequestParts<S> for Ctx {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        match parts.extensions.get::<RequestContext>() {
            Some(ctx) => Ok(Self(ctx.clone())),
            None => {
                // A wiring bug: the store layer is never reached without a context.
                tracing::error!(
                    kind = "no_context",
                    "a store route ran without a request context"
                );
                Err(ApiError::internal())
            }
        }
    }
}

/// The `Idempotency-Key` header, if sent once. The engine checks its form.
#[derive(Debug, Clone)]
pub(crate) struct IdempotencyKey(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for IdempotencyKey {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        idempotency_key(&parts.headers).map(Self)
    }
}

fn idempotency_key(map: &HeaderMap) -> Result<Option<String>, ApiError> {
    let mut values = map.get_all(&headers::IDEMPOTENCY_KEY).iter();
    let bad = || {
        ApiError::invalid_argument(
            "Idempotency-Key",
            "the Idempotency-Key header must be sent once, as 1 to 128 printable ASCII characters",
        )
    };
    match (values.next(), values.next()) {
        (None, _) => Ok(None),
        (Some(v), None) => v.to_str().map(|s| Some(s.to_owned())).map_err(|_| bad()),
        (Some(_), Some(_)) => Err(bad()),
    }
}

/// Whether the request's content type is `expected` (parameters such as `charset` aside).
pub(crate) fn has_content_type(map: &HeaderMap, expected: &str) -> bool {
    map.get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case(expected))
}

/// A JSON body of type `T`. Unknown fields, wrong types and a wrong content type are
/// `invalid_argument`; an empty body counts as `{}`.
#[derive(Debug)]
pub(crate) struct JsonBody<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let typed = has_content_type(req.headers(), "application/json");
        let bytes = Bytes::from_request(req, state).await.map_err(|r| {
            if r.status() == http::StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::payload_too_large()
            } else {
                ApiError::invalid_argument("body", "the body could not be read")
            }
        })?;
        if bytes.is_empty() {
            return parse(b"{}").map(Self);
        }
        if !typed {
            return Err(ApiError::invalid_argument(
                "Content-Type",
                "a request body must be sent as application/json",
            ));
        }
        parse(&bytes).map(Self)
    }
}

fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes).map_err(|e| {
        let message = e.to_string();
        // serde names the field in backquotes for an unknown or a missing field.
        let field = message
            .split('`')
            .nth(1)
            .filter(|_| message.contains("unknown field") || message.contains("missing field"))
            .unwrap_or("body")
            .to_owned();
        ApiError::invalid_argument(field, message)
    })
}

/// The query parameters of a request, taken one by one; [`QueryMap::finish`] refuses any
/// that is left.
#[derive(Debug, Default)]
pub(crate) struct QueryMap(BTreeMap<String, Vec<String>>);

impl<S: Send + Sync> FromRequestParts<S> for QueryMap {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        let Query(pairs) = Query::<Vec<(String, String)>>::try_from_uri(&parts.uri)
            .map_err(|_| ApiError::invalid_argument("query", "the query string is malformed"))?;
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in pairs {
            map.entry(k).or_default().push(v);
        }
        Ok(Self(map))
    }
}

impl QueryMap {
    /// One value; sent twice is an error.
    pub(crate) fn text(&mut self, name: &str) -> Result<Option<String>, ApiError> {
        match self.0.remove(name) {
            None => Ok(None),
            Some(mut values) if values.len() == 1 => Ok(values.pop()),
            Some(_) => Err(ApiError::invalid_argument(
                name,
                format!("the query parameter {name} may be given once"),
            )),
        }
    }

    /// Every value of a repeatable parameter.
    pub(crate) fn all(&mut self, name: &str) -> Vec<String> {
        self.0.remove(name).unwrap_or_default()
    }

    /// `true` or `false`.
    pub(crate) fn flag(&mut self, name: &str, default: bool) -> Result<bool, ApiError> {
        match self.text(name)?.as_deref() {
            None => Ok(default),
            Some("true") => Ok(true),
            Some("false") => Ok(false),
            Some(_) => Err(ApiError::invalid_argument(
                name,
                format!("{name} must be true or false"),
            )),
        }
    }

    /// A whole number.
    pub(crate) fn number(&mut self, name: &str) -> Result<Option<u32>, ApiError> {
        self.text(name)?
            .map(|v| {
                v.parse().map_err(|_| {
                    ApiError::invalid_argument(name, format!("{name} must be a whole number"))
                })
            })
            .transpose()
    }

    /// An RFC 3339 time.
    pub(crate) fn time(&mut self, name: &str) -> Result<Option<Timestamp>, ApiError> {
        self.text(name)?
            .map(|v| {
                Timestamp::parse_rfc3339(&v)
                    .map_err(|e| ApiError::invalid_argument(name, format!("{name}: {e}")))
            })
            .transpose()
    }

    /// A value of a `snake_case` enum, as its JSON string.
    pub(crate) fn word<T: DeserializeOwned>(&mut self, name: &str) -> Result<Option<T>, ApiError> {
        self.text(name)?
            .map(|v| {
                serde_json::from_value(serde_json::Value::String(v)).map_err(|_| {
                    ApiError::invalid_argument(name, format!("{name} has an unknown value"))
                })
            })
            .transpose()
    }

    /// Refuses any parameter that was not taken.
    pub(crate) fn finish(self) -> Result<(), ApiError> {
        match self.0.into_keys().next() {
            None => Ok(()),
            Some(name) => Err(ApiError::invalid_argument(
                name.clone(),
                format!("unknown query parameter {name}"),
            )),
        }
    }
}

/// A memory ID from the path.
pub(crate) fn memory_id(text: &str) -> Result<MemoryId, ApiError> {
    Uuid::parse_str(text)
        .map(MemoryId::from_uuid)
        .map_err(|_| ApiError::invalid_argument("id", "the memory ID must be a UUID"))
}
