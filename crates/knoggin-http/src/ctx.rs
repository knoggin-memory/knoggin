// SPDX-License-Identifier: AGPL-3.0-only
//! `RequestContext` and the internal headers, the one place that parses or builds them.
//!
//! The edge layer checks the credential and builds a `RequestContext`; the store layer
//! reads it from the request extensions and never a header from the client. Between a
//! router and a container the context travels as the internal headers below.

use std::fmt;

use http::{HeaderMap, HeaderName, HeaderValue};
use knoggin_core::model::limits::validate_namespace_name;
use knoggin_core::ops::WriteContext;
use knoggin_core::{StoreId, UserId};
use uuid::Uuid;

use crate::error::ApiError;

/// Header names, lower case as `http` wants them.
pub mod headers {
    use http::HeaderName;

    /// The request ID the server makes, on every response and on forwarded requests.
    pub const REQUEST_ID: HeaderName = HeaderName::from_static("knoggin-request-id");
    /// The request's default namespace, from the client.
    pub const NAMESPACE: HeaderName = HeaderName::from_static("knoggin-namespace");
    /// The store a browser session names, from the client.
    pub const STORE: HeaderName = HeaderName::from_static("knoggin-store");
    /// The client's idempotency key.
    pub const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
    /// `true` on a stored answer.
    pub const IDEMPOTENT_REPLAYED: HeaderName = HeaderName::from_static("idempotent-replayed");
    /// Internal: the user.
    pub const USER_ID: HeaderName = HeaderName::from_static("knoggin-user-id");
    /// Internal: the store.
    pub const STORE_ID: HeaderName = HeaderName::from_static("knoggin-store-id");
    /// Internal: `memory:read` or `memory:read,memory:write`.
    pub const PERMISSIONS: HeaderName = HeaderName::from_static("knoggin-permissions");
    /// Internal: `api_key`, `oauth`, `session` or `none`.
    pub const CREDENTIAL_KIND: HeaderName = HeaderName::from_static("knoggin-credential-kind");
    /// Internal: the key's ID, the user's ID, or empty for `none`.
    pub const CREDENTIAL_ID: HeaderName = HeaderName::from_static("knoggin-credential-id");
    /// Internal: the protocol version of the link.
    pub const PROTOCOL: HeaderName = HeaderName::from_static("knoggin-protocol");
    /// Internal: the container that calls a router.
    pub const CONTAINER_ID: HeaderName = HeaderName::from_static("knoggin-container-id");
    /// Internal: `parking` on a container's 503 while it parks.
    pub const INTERNAL_STATUS: HeaderName = HeaderName::from_static("knoggin-internal-status");
}

/// The protocol version of the internal link that this build speaks.
pub const PROTOCOL_VERSION: &str = "1";

/// What the credential may do with memories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// `memory:read`.
    pub read: bool,
    /// `memory:write`.
    pub write: bool,
}

impl Permissions {
    /// `memory:read` only.
    pub const READ: Self = Self {
        read: true,
        write: false,
    };
    /// `memory:read` and `memory:write`.
    pub const READ_WRITE: Self = Self {
        read: true,
        write: true,
    };

    /// The header form: `memory:read` or `memory:read,memory:write`.
    pub fn header_value(self) -> &'static str {
        match (self.read, self.write) {
            (_, true) => "memory:read,memory:write",
            _ => "memory:read",
        }
    }

    /// Parses the header form; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "memory:read" => Some(Self::READ),
            "memory:read,memory:write" => Some(Self::READ_WRITE),
            _ => None,
        }
    }
}

/// The kind of credential a request carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// An API key of one store.
    ApiKey,
    /// An OAuth access token.
    Oauth,
    /// A browser session or a local web session.
    Session,
    /// The daemon's Unix socket. Never travels on the internal link.
    Local,
    /// A server with sign-in off.
    None,
}

impl CredentialKind {
    /// The header and `savedBy` form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Oauth => "oauth",
            Self::Session => "session",
            Self::Local => "local",
            Self::None => "none",
        }
    }

    fn parse_internal(text: &str) -> Option<Self> {
        match text {
            "api_key" => Some(Self::ApiKey),
            "oauth" => Some(Self::Oauth),
            "session" => Some(Self::Session),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// The credential of a request: its kind and ID, for logs, rate limits and `savedBy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialRef {
    /// The kind.
    pub kind: CredentialKind,
    /// The key's ID for an API key, the user's ID for `oauth` and `session`, none otherwise.
    pub id: Option<Uuid>,
}

impl CredentialRef {
    /// The memory's `savedBy`: `api_key:<id>`, `oauth:<id>`, `session:<id>`, `local` or `none`.
    pub fn saved_by(&self) -> String {
        match self.id {
            Some(id) => format!("{}:{id}", self.kind.as_str()),
            None => self.kind.as_str().to_owned(),
        }
    }
}

/// Who asks, for which store, with what rights. Built by the edge layer (or rebuilt from
/// the internal headers in a container) and put in the request extensions.
#[derive(Clone, PartialEq, Eq)]
pub struct RequestContext {
    /// The UUIDv7 the server made for this request.
    pub request_id: Uuid,
    /// The user.
    pub user_id: UserId,
    /// The store the credential selects.
    pub store_id: StoreId,
    /// Read, write.
    pub permissions: Permissions,
    /// Kind and ID of the credential.
    pub credential: CredentialRef,
    /// From the `Knoggin-Namespace` header, already checked as a namespace name.
    pub default_namespace: Option<String>,
    /// The MCP client's name, when the request came over MCP.
    pub client: Option<String>,
}

// Namespace and client names are sensitive (tab "Engineering rules", logging).
impl fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestContext")
            .field("request_id", &self.request_id)
            .field("user_id", &self.user_id)
            .field("store_id", &self.store_id)
            .field("permissions", &self.permissions)
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

impl RequestContext {
    /// The engine's `WriteContext` for this request.
    pub fn write_context(&self) -> WriteContext {
        WriteContext {
            saved_by: self.credential.saved_by(),
            client: self.client.clone(),
        }
    }

    /// `permission_denied` unless the credential may write.
    pub fn require_write(&self) -> Result<(), ApiError> {
        if self.permissions.write {
            Ok(())
        } else {
            Err(ApiError::permission_denied(
                "this credential may not change memories",
            ))
        }
    }

    /// `permission_denied` unless the credential may read.
    pub fn require_read(&self) -> Result<(), ApiError> {
        if self.permissions.read {
            Ok(())
        } else {
            Err(ApiError::permission_denied(
                "this credential may not read memories",
            ))
        }
    }

    /// Writes the internal headers of a forwarded request, after removing any that are
    /// there already, so nothing the client sent can repeat or replace one.
    pub fn write_internal_headers(&self, map: &mut HeaderMap) -> Result<(), ApiError> {
        for name in [
            headers::USER_ID,
            headers::STORE_ID,
            headers::PERMISSIONS,
            headers::CREDENTIAL_KIND,
            headers::CREDENTIAL_ID,
            headers::REQUEST_ID,
            headers::PROTOCOL,
            headers::NAMESPACE,
        ] {
            map.remove(name);
        }
        if self.credential.kind == CredentialKind::Local {
            return Err(ApiError::internal());
        }
        let value = |text: &str| HeaderValue::from_str(text).map_err(|_| ApiError::internal());
        map.insert(headers::USER_ID, value(&self.user_id.to_string())?);
        map.insert(headers::STORE_ID, value(&self.store_id.to_string())?);
        map.insert(
            headers::PERMISSIONS,
            HeaderValue::from_static(self.permissions.header_value()),
        );
        map.insert(
            headers::CREDENTIAL_KIND,
            HeaderValue::from_static(self.credential.kind.as_str()),
        );
        let id = self
            .credential
            .id
            .map(|id| id.to_string())
            .unwrap_or_default();
        map.insert(headers::CREDENTIAL_ID, value(&id)?);
        map.insert(headers::REQUEST_ID, value(&self.request_id.to_string())?);
        map.insert(
            headers::PROTOCOL,
            HeaderValue::from_static(PROTOCOL_VERSION),
        );
        if let Some(ns) = &self.default_namespace {
            map.insert(headers::NAMESPACE, value(ns)?);
        }
        Ok(())
    }

    /// Rebuilds the context from the internal headers of a forwarded request. A required
    /// header that is missing, appears twice or does not parse is `internal`; a bad
    /// `Knoggin-Namespace` is the client's own error (`invalid_argument`).
    pub fn from_internal_headers(map: &HeaderMap) -> Result<Self, ApiError> {
        if one(map, &headers::PROTOCOL)? != PROTOCOL_VERSION {
            return Err(ApiError::internal());
        }
        let uuid =
            |name: &HeaderName| Uuid::parse_str(one(map, name)?).map_err(|_| ApiError::internal());
        let kind = CredentialKind::parse_internal(one(map, &headers::CREDENTIAL_KIND)?)
            .ok_or_else(ApiError::internal)?;
        let id_text = one(map, &headers::CREDENTIAL_ID)?;
        let id = match (kind, id_text.is_empty()) {
            (CredentialKind::None, true) => None,
            (CredentialKind::None, false) | (_, true) => return Err(ApiError::internal()),
            (_, false) => Some(Uuid::parse_str(id_text).map_err(|_| ApiError::internal())?),
        };
        Ok(Self {
            request_id: uuid(&headers::REQUEST_ID)?,
            user_id: UserId::from_uuid(uuid(&headers::USER_ID)?),
            store_id: StoreId::from_uuid(uuid(&headers::STORE_ID)?),
            permissions: Permissions::parse(one(map, &headers::PERMISSIONS)?)
                .ok_or_else(ApiError::internal)?,
            credential: CredentialRef { kind, id },
            default_namespace: namespace_header(map)?,
            client: None,
        })
    }
}

/// The client's `Knoggin-Namespace` header: absent, or one valid namespace name.
/// Sent twice, not UTF-8 or not a valid name is `invalid_argument`.
pub fn namespace_header(map: &HeaderMap) -> Result<Option<String>, ApiError> {
    let mut values = map.get_all(&headers::NAMESPACE).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let bad = || {
        ApiError::invalid_argument(
            "Knoggin-Namespace",
            "the Knoggin-Namespace header must name one valid namespace",
        )
    };
    if values.next().is_some() {
        return Err(bad());
    }
    let text = value.to_str().map_err(|_| bad())?;
    validate_namespace_name("Knoggin-Namespace", text).map_err(|_| bad())?;
    Ok(Some(text.to_owned()))
}

/// Whether a call on the internal link carries exactly one `Knoggin-Protocol` that this
/// build speaks. A call without it, or with another version, is refused with `internal`.
pub fn link_protocol_ok(map: &HeaderMap) -> bool {
    one(map, &headers::PROTOCOL).is_ok_and(|v| v == PROTOCOL_VERSION)
}

/// The token of a call on the internal link: exactly one `Authorization: Bearer <token>`.
pub fn link_bearer(map: &HeaderMap) -> Option<&str> {
    let value = one(map, &http::header::AUTHORIZATION).ok()?;
    value.strip_prefix("Bearer ").filter(|t| !t.is_empty())
}

/// The identity headers of a container's call to a router: `Knoggin-User-Id` and
/// `Knoggin-Container-Id`, each exactly once and a UUID.
pub fn link_identity(map: &HeaderMap) -> Option<(UserId, Uuid)> {
    let uuid = |name: &HeaderName| Uuid::parse_str(one(map, name).ok()?).ok();
    Some((
        UserId::from_uuid(uuid(&headers::USER_ID)?),
        uuid(&headers::CONTAINER_ID)?,
    ))
}

/// Writes a container's identity on a call to a router: its user, its ID, its token
/// (`bearer` is the whole `Authorization` value) and `Knoggin-Protocol`, replacing any
/// that are there.
pub fn write_link_identity(
    map: &mut HeaderMap,
    user: UserId,
    container: Uuid,
    bearer: HeaderValue,
) {
    write_link_bearer(map, bearer);
    map.insert(headers::USER_ID, uuid_value(&user.uuid()));
    map.insert(headers::CONTAINER_ID, uuid_value(&container));
}

/// Writes a router's token (`bearer` is the whole `Authorization` value) and
/// `Knoggin-Protocol` on a call to a container, replacing any that are there.
pub fn write_link_bearer(map: &mut HeaderMap, bearer: HeaderValue) {
    map.insert(http::header::AUTHORIZATION, bearer);
    map.insert(
        headers::PROTOCOL,
        HeaderValue::from_static(PROTOCOL_VERSION),
    );
}

fn uuid_value(id: &Uuid) -> HeaderValue {
    let mut buf = Uuid::encode_buffer();
    #[expect(
        clippy::expect_used,
        reason = "a hyphenated UUID is always a valid header value"
    )]
    let value = HeaderValue::from_str(id.hyphenated().encode_lower(&mut buf))
        .expect("a UUID is a valid header value");
    value
}

fn one<'a>(map: &'a HeaderMap, name: &HeaderName) -> Result<&'a str, ApiError> {
    let mut values = map.get_all(name).iter();
    match (values.next(), values.next()) {
        (Some(v), None) => v.to_str().map_err(|_| ApiError::internal()),
        _ => Err(ApiError::internal()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Code;

    fn ctx(kind: CredentialKind, id: Option<Uuid>) -> RequestContext {
        RequestContext {
            request_id: Uuid::now_v7(),
            user_id: UserId::from_uuid(Uuid::now_v7()),
            store_id: StoreId::from_uuid(Uuid::now_v7()),
            permissions: Permissions::READ,
            credential: CredentialRef { kind, id },
            default_namespace: Some("work".into()),
            client: Some("marker-client".into()),
        }
    }

    #[test]
    fn internal_headers_round_trip() {
        for (kind, id) in [
            (CredentialKind::ApiKey, Some(Uuid::now_v7())),
            (CredentialKind::Oauth, Some(Uuid::now_v7())),
            (CredentialKind::Session, Some(Uuid::now_v7())),
            (CredentialKind::None, None),
        ] {
            let c = ctx(kind, id);
            let mut map = HeaderMap::new();
            map.append(
                headers::PERMISSIONS,
                HeaderValue::from_static("memory:read,memory:write"),
            );
            c.write_internal_headers(&mut map).unwrap();
            let back = RequestContext::from_internal_headers(&map).unwrap();
            assert_eq!(back.permissions, Permissions::READ, "client value replaced");
            assert_eq!(back.client, None);
            assert_eq!(
                RequestContext {
                    client: c.client.clone(),
                    ..back
                },
                c
            );
        }
    }

    #[test]
    fn local_never_travels() {
        let mut map = HeaderMap::new();
        let err = ctx(CredentialKind::Local, None)
            .write_internal_headers(&mut map)
            .unwrap_err();
        assert_eq!(err.code, Code::Internal);
    }

    #[test]
    fn a_missing_or_repeated_header_is_internal() {
        let mut map = HeaderMap::new();
        ctx(CredentialKind::ApiKey, Some(Uuid::now_v7()))
            .write_internal_headers(&mut map)
            .unwrap();
        let mut twice = map.clone();
        twice.append(
            headers::PERMISSIONS,
            HeaderValue::from_static("memory:read"),
        );
        let mut missing = map.clone();
        missing.remove(headers::USER_ID);
        let mut old = map.clone();
        old.insert(headers::PROTOCOL, HeaderValue::from_static("0"));
        for bad in [twice, missing, old] {
            assert_eq!(
                RequestContext::from_internal_headers(&bad)
                    .unwrap_err()
                    .code,
                Code::Internal
            );
        }
        let mut bad_ns = map;
        bad_ns.insert(headers::NAMESPACE, HeaderValue::from_static("no spaces"));
        assert_eq!(
            RequestContext::from_internal_headers(&bad_ns)
                .unwrap_err()
                .code,
            Code::InvalidArgument
        );
    }

    #[test]
    fn link_headers() {
        let user = UserId::from_uuid(Uuid::now_v7());
        let container = Uuid::now_v7();
        let mut map = HeaderMap::new();
        assert!(!link_protocol_ok(&map));
        assert_eq!(link_bearer(&map), None);
        map.append(headers::USER_ID, HeaderValue::from_static("client-sent"));
        write_link_identity(
            &mut map,
            user,
            container,
            HeaderValue::from_static("Bearer abc"),
        );
        assert!(link_protocol_ok(&map));
        assert_eq!(link_bearer(&map), Some("abc"));
        assert_eq!(link_identity(&map), Some((user, container)));
        let mut twice = map.clone();
        twice.append(headers::CONTAINER_ID, HeaderValue::from_static("x"));
        assert_eq!(link_identity(&twice), None);
        twice.append(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(link_bearer(&twice), None);
        for bad in ["Bearer ", "bearer abc", "Basic abc"] {
            let mut m = HeaderMap::new();
            m.insert(http::header::AUTHORIZATION, HeaderValue::from_static(bad));
            assert_eq!(link_bearer(&m), None, "{bad}");
        }
        map.insert(headers::PROTOCOL, HeaderValue::from_static("2"));
        assert!(!link_protocol_ok(&map));
    }

    #[test]
    fn saved_by_and_debug() {
        let id = Uuid::now_v7();
        let c = ctx(CredentialKind::ApiKey, Some(id));
        assert_eq!(c.write_context().saved_by, format!("api_key:{id}"));
        assert_eq!(
            ctx(CredentialKind::Local, None).credential.saved_by(),
            "local"
        );
        let d = format!("{c:?}");
        assert!(!d.contains("marker") && !d.contains("work"), "{d}");
    }
}
