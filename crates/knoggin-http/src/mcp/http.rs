// SPDX-License-Identifier: AGPL-3.0-only
//! The streamable HTTP transport, ported from recalld's `src/mcp/http_transport.rs`.
//!
//! A current-revision request is stateless: any `Mcp-Session-Id` is ignored and none is
//! returned. An `initialize` makes a session of the older revisions, which later requests
//! name in `Mcp-Session-Id`. Credentials and the origin rule are the edge layer's; the
//! request context arrives in the extensions.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use knoggin_core::StoreId;
use serde_json::Value;
use uuid::Uuid;

use super::protocol::{
    Era, EraHint, HEADER_MISMATCH, INVALID_PARAMS, INVALID_REQUEST, InitializeParams,
    InitializeResult, JSONRPC_VERSION, JsonRpcId, JsonRpcMessage, JsonRpcResponse,
    META_PROTOCOL_VERSION, MetaError, PARSE_ERROR, RequestMeta, era_hint,
    is_handshake_exempt_method, negotiate_legacy_version, parse_request_meta, server_capabilities,
    server_info,
};
use super::server::{Call, dispatch, success};
use super::tools::ToolSet;
use super::{Answered, McpState};
use crate::ctx::{CredentialKind, RequestContext};
use crate::error::ApiError;

const HDR_SESSION_ID: &str = "mcp-session-id";
const HDR_MCP_METHOD: &str = "mcp-method";
const HDR_MCP_NAME: &str = "mcp-name";
const HDR_MCP_PROTOCOL_VER: &str = "mcp-protocol-version";
/// The SEP-2243 wrapper of a header value that is not plain ASCII.
const B64_PREFIX: &str = "=?base64?";
const B64_SUFFIX: &str = "?=";
const ALLOW_METHODS: &str = "POST, DELETE";

/// At most this many sessions per process.
pub(crate) const MAX_SESSIONS: usize = 1024;
/// A session idle this long has ended.
pub(crate) const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);
/// The longest client name kept, in bytes.
const CLIENT_MAX_BYTES: usize = 64;

/// Who made a session: the store, the API key if one was used, and the endpoint. A
/// request from anyone else gets the 404 of an unknown session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Owner {
    store: StoreId,
    key: Option<Uuid>,
    set: ToolSet,
}

impl Owner {
    fn of(ctx: &RequestContext, set: ToolSet) -> Self {
        let key = (ctx.credential.kind == CredentialKind::ApiKey)
            .then_some(ctx.credential.id)
            .flatten();
        Self {
            store: ctx.store_id,
            key,
            set,
        }
    }
}

/// One session of the older revisions: the client's name and who made it. It exists only
/// once `initialize` succeeded; the negotiated version is answered and not needed again.
struct Session {
    owner: Owner,
    client: Option<String>,
    last_seen: Instant,
}

/// The sessions of this process. Held only in memory: a restart or a parked container
/// ends them and clients send `initialize` again.
#[derive(Default)]
pub(crate) struct Sessions {
    map: HashMap<String, Session>,
}

impl Sessions {
    /// Adds a session, first dropping idle ones and, when the table is full, the one idle
    /// longest.
    fn insert(&mut self, id: String, session: Session, now: Instant) {
        self.map
            .retain(|_, s| now.saturating_duration_since(s.last_seen) < SESSION_IDLE);
        if self.map.len() >= MAX_SESSIONS
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, s)| s.last_seen)
                .map(|(k, _)| k.clone())
        {
            self.map.remove(&oldest);
        }
        self.map.insert(id, session);
    }

    /// The live session `id` of `owner`, touched.
    fn get(&mut self, id: &str, owner: Owner, now: Instant) -> Option<&mut Session> {
        if self
            .map
            .get(id)
            .is_some_and(|s| now.saturating_duration_since(s.last_seen) >= SESSION_IDLE)
        {
            self.map.remove(id);
        }
        let session = self.map.get_mut(id).filter(|s| s.owner == owner)?;
        session.last_seen = now;
        Some(session)
    }

    /// Ends the session `id` of `owner`; false when there is none.
    fn remove(&mut self, id: &str, owner: Owner, now: Instant) -> bool {
        self.get(id, owner, now).is_some() && self.map.remove(id).is_some()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

fn sessions(state: &McpState) -> std::sync::MutexGuard<'_, Sessions> {
    state
        .sessions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// A random session ID: 32 bytes from the system's generator, as hex.
fn new_session_id() -> Option<String> {
    let mut bytes = [0u8; 32];
    aws_lc_rs::rand::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The client's name cut at a character boundary at or below 64 bytes; none if empty.
pub(crate) fn cut_client(name: &str) -> Option<String> {
    let mut end = name.len().min(CLIENT_MAX_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &name[..end];
    (!cut.is_empty()).then(|| cut.to_owned())
}

fn rpc(status: StatusCode, response: JsonRpcResponse) -> Response {
    let body = serde_json::to_vec(&response).unwrap_or_default();
    let mut r = (status, body).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    // The store layer must not turn this answer into a REST error.
    r.extensions_mut().insert(Answered);
    r
}

fn rpc_error(status: StatusCode, id: JsonRpcId, code: i32, message: impl Into<String>) -> Response {
    rpc(status, JsonRpcResponse::error(id, code, message, None))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn unknown_session(id: JsonRpcId) -> Response {
    rpc_error(
        StatusCode::NOT_FOUND,
        id,
        INVALID_REQUEST,
        "Unknown or expired MCP session; send 'initialize' again",
    )
}

/// `POST /mcp` and `POST /mcp/plugin`.
pub(crate) async fn post(
    State(state): State<Arc<McpState>>,
    set: ToolSet,
    Extension(ctx): Extension<RequestContext>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                JsonRpcId::Null,
                PARSE_ERROR,
                "Parse error: the body is not JSON",
            );
        }
    };
    if value.is_array() {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            JsonRpcId::Null,
            INVALID_REQUEST,
            "Invalid request: JSON-RPC batches are not supported",
        );
    }
    let guessed = value
        .get("id")
        .and_then(|id| serde_json::from_value(id.clone()).ok())
        .unwrap_or(JsonRpcId::Null);
    let message: JsonRpcMessage = match serde_json::from_value(value) {
        Ok(m) => m,
        Err(_) => {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                guessed,
                INVALID_REQUEST,
                "Invalid request: not a JSON-RPC 2.0 request",
            );
        }
    };
    if message.jsonrpc != JSONRPC_VERSION {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            guessed,
            INVALID_REQUEST,
            "Invalid request: jsonrpc must be \"2.0\"",
        );
    }
    // A notification gets no answer and changes nothing.
    let Some(id) = message.id.clone() else {
        let mut r = StatusCode::ACCEPTED.into_response();
        r.extensions_mut().insert(Answered);
        return r;
    };
    match era_hint(&message) {
        EraHint::Modern => modern(&state, set, ctx, &headers, id, message).await,
        EraHint::LegacyHandshake => initialize(&state, set, ctx, &headers, id, message),
        EraHint::Ambiguous => {
            if let Some(sid) = header_str(&headers, HDR_SESSION_ID) {
                let owner = Owner::of(&ctx, set);
                let client = match sessions(&state).get(sid, owner, Instant::now()) {
                    Some(s) => s.client.clone(),
                    None => return unknown_session(id),
                };
                let ctx = RequestContext { client, ..ctx };
                let call = Call {
                    era: Era::Legacy,
                    set,
                    ctx: &ctx,
                };
                let r = dispatch(&state, call, id, &message.method, message.params).await;
                rpc(StatusCode::OK, r)
            } else if is_handshake_exempt_method(&message.method) {
                let call = Call {
                    era: Era::Legacy,
                    set,
                    ctx: &ctx,
                };
                let r = dispatch(&state, call, id, &message.method, message.params).await;
                rpc(StatusCode::OK, r)
            } else {
                rpc_error(
                    StatusCode::BAD_REQUEST,
                    id,
                    INVALID_PARAMS,
                    MetaError::MissingField(META_PROTOCOL_VERSION).message(),
                )
            }
        }
    }
}

/// A current-revision request: no session, the three headers checked against the body.
async fn modern(
    state: &McpState,
    set: ToolSet,
    ctx: RequestContext,
    headers: &HeaderMap,
    id: JsonRpcId,
    message: JsonRpcMessage,
) -> Response {
    let meta = match parse_request_meta(message.params.as_ref()) {
        Ok(meta) => meta,
        Err(e) => {
            return rpc(
                StatusCode::BAD_REQUEST,
                JsonRpcResponse::error(id, e.code(), e.message(), e.data()),
            );
        }
    };
    if let Err(detail) = validate_modern_headers(headers, &message, &meta) {
        return rpc_error(StatusCode::BAD_REQUEST, id, HEADER_MISMATCH, detail);
    }
    let client = meta.client_info.as_ref().and_then(|c| cut_client(&c.name));
    let ctx = RequestContext { client, ..ctx };
    let call = Call {
        era: Era::Modern,
        set,
        ctx: &ctx,
    };
    let r = dispatch(state, call, id, &message.method, message.params).await;
    rpc(StatusCode::OK, r)
}

/// `initialize` of the older revisions: makes a session, or renews the one the client
/// already names.
fn initialize(
    state: &McpState,
    set: ToolSet,
    ctx: RequestContext,
    headers: &HeaderMap,
    id: JsonRpcId,
    message: JsonRpcMessage,
) -> Response {
    if message.method != "initialize" {
        // `notifications/initialized` sent as a request.
        return rpc_error(
            StatusCode::BAD_REQUEST,
            id,
            INVALID_REQUEST,
            "Invalid request: notifications/initialized is a notification",
        );
    }
    let params: InitializeParams = match message.params.map(serde_json::from_value) {
        Some(Ok(p)) => p,
        _ => {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                id,
                INVALID_PARAMS,
                "Invalid params: initialize needs protocolVersion, capabilities and clientInfo",
            );
        }
    };
    let version = negotiate_legacy_version(&params.protocol_version);
    let client = cut_client(&params.client_info.name);
    let owner = Owner::of(&ctx, set);
    let now = Instant::now();
    let mut table = sessions(state);
    let existing = header_str(headers, HDR_SESSION_ID).and_then(|sid| {
        let s = table.get(sid, owner, now)?;
        s.client.clone_from(&client);
        Some(sid.to_owned())
    });
    let session_id = match existing {
        Some(sid) => sid,
        None => {
            let Some(sid) = new_session_id() else {
                return rpc_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    id,
                    super::protocol::INTERNAL_ERROR,
                    "Internal error",
                );
            };
            table.insert(
                sid.clone(),
                Session {
                    owner,
                    client,
                    last_seen: now,
                },
                now,
            );
            sid
        }
    };
    drop(table);
    let result = InitializeResult {
        protocol_version: version.to_owned(),
        capabilities: server_capabilities(),
        server_info: server_info(),
        instructions: state.instructions(),
    };
    let mut r = rpc(
        StatusCode::OK,
        success(id, serde_json::to_value(result).unwrap_or_default(), None),
    );
    if let Ok(v) = HeaderValue::from_str(&session_id) {
        r.headers_mut().insert(HDR_SESSION_ID, v);
    }
    r
}

/// The SEP-2243 headers: `Mcp-Method` and `MCP-Protocol-Version` always, `Mcp-Name` on
/// `tools/call`, each equal to the body.
fn validate_modern_headers(
    headers: &HeaderMap,
    message: &JsonRpcMessage,
    meta: &RequestMeta,
) -> Result<(), String> {
    let method = header_str(headers, HDR_MCP_METHOD)
        .ok_or_else(|| "Header mismatch: missing required Mcp-Method header".to_owned())?;
    let method = decode_header_value(method, "Mcp-Method")?;
    if method != message.method {
        return Err(format!(
            "Header mismatch: Mcp-Method header \"{method}\" does not match body method \"{}\"",
            message.method
        ));
    }
    let version = header_str(headers, HDR_MCP_PROTOCOL_VER).ok_or_else(|| {
        "Header mismatch: missing required MCP-Protocol-Version header".to_owned()
    })?;
    let version = decode_header_value(version, "MCP-Protocol-Version")?;
    if version != meta.protocol_version {
        return Err(format!(
            "Header mismatch: MCP-Protocol-Version header \"{version}\" does not match \
             _meta protocol version \"{}\"",
            meta.protocol_version
        ));
    }
    let expected = match message.method.as_str() {
        "tools/call" | "prompts/get" => Some(("name", "params.name")),
        "resources/read" => Some(("uri", "params.uri")),
        _ => None,
    };
    if let Some((field, label)) = expected {
        let want = message
            .params
            .as_ref()
            .and_then(|p| p.get(field))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = header_str(headers, HDR_MCP_NAME)
            .ok_or_else(|| "Header mismatch: missing required Mcp-Name header".to_owned())?;
        if decode_header_value(name, "Mcp-Name")? != want {
            return Err(format!(
                "Header mismatch: Mcp-Name header does not match {label}"
            ));
        }
    }
    Ok(())
}

/// Decodes the `=?base64?{value}?=` wrapper, if present (standard alphabet, padded or not).
fn decode_header_value(raw: &str, name: &str) -> Result<String, String> {
    let Some(inner) = raw
        .strip_prefix(B64_PREFIX)
        .and_then(|r| r.strip_suffix(B64_SUFFIX))
    else {
        return Ok(raw.to_owned());
    };
    let malformed = || format!("Header mismatch: malformed base64 sentinel in {name} header");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(inner)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(inner))
        .map_err(|_| malformed())?;
    String::from_utf8(bytes).map_err(|_| malformed())
}

/// `DELETE`: ends the session named in `Mcp-Session-Id`.
pub(crate) async fn delete(
    State(state): State<Arc<McpState>>,
    set: ToolSet,
    Extension(ctx): Extension<RequestContext>,
    headers: HeaderMap,
) -> Response {
    let Some(sid) = header_str(&headers, HDR_SESSION_ID) else {
        return method_not_allowed();
    };
    if sessions(&state).remove(sid, Owner::of(&ctx, set), Instant::now()) {
        StatusCode::OK.into_response()
    } else {
        ApiError::not_found("Unknown or expired MCP session").into_response()
    }
}

/// `GET`: there is no server-to-client stream.
pub(crate) async fn get() -> Response {
    method_not_allowed()
}

fn method_not_allowed() -> Response {
    let mut r = ApiError::method_not_allowed().into_response();
    r.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(ALLOW_METHODS));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(owner: Owner, at: Instant) -> Session {
        Session {
            owner,
            client: None,
            last_seen: at,
        }
    }

    fn owner() -> Owner {
        Owner {
            store: StoreId::from_uuid(Uuid::now_v7()),
            key: Some(Uuid::now_v7()),
            set: ToolSet::Full,
        }
    }

    #[test]
    fn idle_sessions_end_and_a_full_table_drops_the_longest_idle() {
        let t0 = Instant::now();
        let o = owner();
        let mut s = Sessions::default();
        s.insert("a".into(), session(o, t0), t0);
        assert!(
            s.get("a", o, t0 + SESSION_IDLE - Duration::from_secs(1))
                .is_some()
        );
        assert!(
            s.get("a", o, t0 + SESSION_IDLE * 2).is_none(),
            "idle 30 min after the touch"
        );
        let mut s = Sessions::default();
        for i in 0..MAX_SESSIONS {
            let at = t0 + Duration::from_millis(i as u64);
            s.insert(i.to_string(), session(o, at), at);
        }
        // Touching "0" makes "1" the longest idle.
        let later = t0 + Duration::from_secs(5);
        assert!(s.get("0", o, later).is_some());
        s.insert("new".into(), session(o, later), later);
        assert_eq!(s.len(), MAX_SESSIONS);
        assert!(s.get("1", o, later).is_none());
        assert!(s.get("0", o, later).is_some() && s.get("new", o, later).is_some());
    }

    #[test]
    fn another_owner_sees_no_session() {
        let t0 = Instant::now();
        let o = owner();
        let mut s = Sessions::default();
        s.insert("a".into(), session(o, t0), t0);
        for other in [
            Owner {
                key: Some(Uuid::now_v7()),
                ..o
            },
            Owner {
                store: StoreId::from_uuid(Uuid::now_v7()),
                ..o
            },
            Owner {
                set: ToolSet::Plugin,
                ..o
            },
        ] {
            assert!(s.get("a", other, t0).is_none());
            assert!(!s.remove("a", other, t0));
        }
        assert!(s.remove("a", o, t0));
        assert!(!s.remove("a", o, t0));
    }

    #[test]
    fn client_names_are_cut_at_a_character_boundary() {
        let name = format!("{}🦀tail", "a".repeat(62));
        assert_eq!(cut_client(&name).unwrap(), "a".repeat(62));
        assert_eq!(cut_client("claude-code").unwrap(), "claude-code");
        assert_eq!(cut_client(""), None);
        let exact = "b".repeat(64);
        assert_eq!(cut_client(&format!("{exact}c")).unwrap(), exact);
    }

    #[test]
    fn base64_sentinels_decode() {
        let enc = format!(
            "{B64_PREFIX}{}{B64_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode("store_memory")
        );
        assert_eq!(decode_header_value(&enc, "x").unwrap(), "store_memory");
        assert_eq!(decode_header_value("plain", "x").unwrap(), "plain");
        let bad = format!("{B64_PREFIX}!!!{B64_SUFFIX}");
        assert!(decode_header_value(&bad, "x").is_err());
    }
}
