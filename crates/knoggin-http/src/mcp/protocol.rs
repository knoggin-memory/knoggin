// SPDX-License-Identifier: AGPL-3.0-only
//! JSON-RPC 2.0 and MCP wire types, ported from recalld's `src/mcp/protocol.rs`.
//!
//! Two revisions share this module. The current one (`2026-07-28`) is stateless: every
//! request carries its protocol version, client info and capabilities in `params._meta`,
//! and every result carries `resultType`, caching hints and the server info. The older
//! ones (`2025-06-18` and before) keep the `initialize` handshake and, over HTTP, the
//! `Mcp-Session-Id` session. The revision is decided per message by [`era_hint`].
//!
//! Types that can hold the client's name have no `Debug`: client names are sensitive.

use serde::{Deserialize, Serialize};

/// The current, stateless revision.
pub(crate) const PROTOCOL_VERSION: &str = "2026-07-28";
/// What `server/discover` advertises and `-32022` lists: the current revision only, so a
/// current client never retries with an older version on the stateless path.
pub(crate) const SUPPORTED_MODERN_VERSIONS: &[&str] = &[PROTOCOL_VERSION];
/// The older revisions that `initialize` accepts and echoes.
pub(crate) const SUPPORTED_LEGACY_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// What `initialize` answers for a version it does not know.
pub(crate) const LEGACY_PROTOCOL_VERSION: &str = "2025-06-18";
pub(crate) const JSONRPC_VERSION: &str = "2.0";
pub(crate) const SERVER_NAME: &str = "knoggin";
pub(crate) const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub(crate) const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

/// The freshness hint of `server/discover` and `tools/list`.
pub(crate) const CATALOG_TTL_MS: u64 = 3_600_000;

pub(crate) const PARSE_ERROR: i32 = -32700;
pub(crate) const INVALID_REQUEST: i32 = -32600;
pub(crate) const METHOD_NOT_FOUND: i32 = -32601;
pub(crate) const INVALID_PARAMS: i32 = -32602;
pub(crate) const INTERNAL_ERROR: i32 = -32603;
pub(crate) const HEADER_MISMATCH: i32 = -32020;
pub(crate) const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

/// A JSON-RPC request (`id` set) or notification (`id` absent or `null`).
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct JsonRpcMessage {
    pub(crate) jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<JsonRpcId>,
    pub(crate) method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) params: Option<serde_json::Value>,
}

/// A request ID. `Null` answers a request whose ID could not be read, rather than
/// inventing one that could collide with a real request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum JsonRpcId {
    Number(i64),
    String(String),
    Null,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct JsonRpcResponse {
    pub(crate) jsonrpc: String,
    pub(crate) id: JsonRpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<JsonRpcError>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct JsonRpcError {
    pub(crate) code: i32,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    pub(crate) fn success(id: JsonRpcId, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub(crate) fn error(
        id: JsonRpcId,
        code: i32,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

/// `initialize` parameters (older revisions).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeParams {
    pub(crate) protocol_version: String,
    #[allow(dead_code, reason = "accepted, not inspected")]
    pub(crate) capabilities: serde_json::Map<String, serde_json::Value>,
    pub(crate) client_info: Implementation,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeResult {
    pub(crate) protocol_version: String,
    pub(crate) capabilities: ServerCapabilities,
    pub(crate) server_info: Implementation,
    pub(crate) instructions: String,
}

/// A client's or the server's name and version.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Implementation {
    pub(crate) name: String,
    pub(crate) version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerCapabilities {
    pub(crate) tools: ToolsCapability,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolsCapability {
    pub(crate) list_changed: bool,
}

/// `{ "tools": { "listChanged": false } }`, shared by `initialize` and `server/discover`.
pub(crate) fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        tools: ToolsCapability {
            list_changed: false,
        },
    }
}

/// One entry of `tools/list`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolInfo {
    pub(crate) name: &'static str,
    pub(crate) title: &'static str,
    pub(crate) description: String,
    pub(crate) input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output_schema: Option<serde_json::Value>,
    pub(crate) annotations: ToolAnnotations,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub(crate) meta: Option<serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolAnnotations {
    pub(crate) read_only_hint: bool,
    pub(crate) destructive_hint: bool,
    pub(crate) idempotent_hint: bool,
    pub(crate) open_world_hint: bool,
}

/// `tools/call` parameters. A missing `arguments` means `{}`.
#[derive(Deserialize)]
pub(crate) struct ToolCallParams {
    pub(crate) name: String,
    #[serde(default = "empty_object")]
    pub(crate) arguments: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}

/// A tool result: one compact JSON text block, the same object in `structuredContent`
/// on the current revision, and `isError` on a failure.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolCallResult {
    pub(crate) content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) structured_content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) is_error: Option<bool>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum ContentBlock {
    Text { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResultType {
    Complete,
}

/// Who may reuse a cached result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CacheScope {
    Public,
    Private,
}

/// The fields every result carries, added once by the dispatcher. The envelope is sent
/// on both revisions: the older schema allows extra top-level keys in a result.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResultEnvelope {
    #[serde(flatten)]
    pub(crate) payload: serde_json::Value,
    pub(crate) result_type: ResultType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ttl_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cache_scope: Option<CacheScope>,
    #[serde(rename = "_meta")]
    pub(crate) meta: ResultMeta,
}

#[derive(Serialize)]
pub(crate) struct ResultMeta {
    #[serde(rename = "io.modelcontextprotocol/serverInfo")]
    pub(crate) server_info: Implementation,
}

impl ResultEnvelope {
    pub(crate) fn new(payload: serde_json::Value, cache: Option<CacheScope>) -> Self {
        Self {
            payload,
            result_type: ResultType::Complete,
            ttl_ms: cache.map(|_| CATALOG_TTL_MS),
            cache_scope: cache,
            meta: ResultMeta {
                server_info: server_info(),
            },
        }
    }
}

pub(crate) fn server_info() -> Implementation {
    Implementation {
        name: SERVER_NAME.to_owned(),
        version: SERVER_VERSION.to_owned(),
    }
}

/// The result of `server/discover`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoverResult {
    pub(crate) supported_versions: &'static [&'static str],
    pub(crate) capabilities: ServerCapabilities,
    pub(crate) instructions: String,
}

/// A permissive view of `params._meta`, so a missing key can be named in the error.
#[derive(Default, Deserialize)]
struct RawRequestMeta {
    #[serde(rename = "io.modelcontextprotocol/protocolVersion")]
    protocol_version: Option<String>,
    #[serde(rename = "io.modelcontextprotocol/clientInfo")]
    client_info: Option<Implementation>,
    #[serde(rename = "io.modelcontextprotocol/clientCapabilities")]
    client_capabilities: Option<serde_json::Map<String, serde_json::Value>>,
}

/// The checked `_meta` of a current-revision request.
pub(crate) struct RequestMeta {
    pub(crate) protocol_version: String,
    pub(crate) client_info: Option<Implementation>,
}

/// Why `params._meta` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MetaError {
    MissingField(&'static str),
    Malformed,
    UnsupportedVersion(String),
}

impl MetaError {
    pub(crate) fn code(&self) -> i32 {
        match self {
            Self::MissingField(_) | Self::Malformed => INVALID_PARAMS,
            Self::UnsupportedVersion(_) => UNSUPPORTED_PROTOCOL_VERSION,
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            // Older clients cannot discover the `_meta` rule, so the message also names
            // their way in.
            Self::MissingField(field) => {
                let hint = if *field == META_PROTOCOL_VERSION {
                    " (2026-07-28 clients); legacy clients must send `initialize` first"
                } else {
                    ""
                };
                format!("Invalid params: missing required `_meta` field \"{field}\"{hint}")
            }
            Self::Malformed => "Invalid params: malformed `_meta`".to_owned(),
            Self::UnsupportedVersion(v) => format!("Unsupported protocol version \"{v}\""),
        }
    }

    pub(crate) fn data(&self) -> Option<serde_json::Value> {
        match self {
            Self::UnsupportedVersion(requested) => Some(serde_json::json!({
                "supported": SUPPORTED_MODERN_VERSIONS,
                "requested": requested,
            })),
            _ => None,
        }
    }
}

/// Parses and checks a current-revision request's `params._meta`.
pub(crate) fn parse_request_meta(
    params: Option<&serde_json::Value>,
) -> Result<RequestMeta, MetaError> {
    let raw = params
        .and_then(|p| p.get("_meta"))
        .ok_or(MetaError::MissingField(META_PROTOCOL_VERSION))?;
    if !raw.is_object() {
        return Err(MetaError::Malformed);
    }
    let raw: RawRequestMeta =
        serde_json::from_value(raw.clone()).map_err(|_| MetaError::Malformed)?;
    let protocol_version = raw
        .protocol_version
        .ok_or(MetaError::MissingField(META_PROTOCOL_VERSION))?;
    if !SUPPORTED_MODERN_VERSIONS.contains(&protocol_version.as_str()) {
        return Err(MetaError::UnsupportedVersion(protocol_version));
    }
    if raw.client_capabilities.is_none() {
        return Err(MetaError::MissingField(META_CLIENT_CAPABILITIES));
    }
    Ok(RequestMeta {
        protocol_version,
        client_info: raw.client_info,
    })
}

/// What one message says about its revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EraHint {
    /// Carries the current `_meta` protocol version.
    Modern,
    /// `initialize` or `notifications/initialized`.
    LegacyHandshake,
    /// Neither: the transport decides from the session.
    Ambiguous,
}

/// The revision a request is served under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Era {
    Modern,
    Legacy,
}

/// The `_meta` protocol-version key wins over the method name: an older client may send
/// `_meta` (for `progressToken`) but never that key, and an `initialize` that carries it
/// is a current request for a method the current revision removed (`-32601`).
pub(crate) fn era_hint(msg: &JsonRpcMessage) -> EraHint {
    let modern = msg
        .params
        .as_ref()
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get(META_PROTOCOL_VERSION))
        .is_some();
    if modern {
        EraHint::Modern
    } else if msg.method == "initialize" || msg.method == "notifications/initialized" {
        EraHint::LegacyHandshake
    } else {
        EraHint::Ambiguous
    }
}

/// Methods served without `_meta` and without a session: `server/discover` (asking for a
/// version in order to learn the versions would be circular) and `ping`.
pub(crate) fn is_handshake_exempt_method(method: &str) -> bool {
    matches!(method, "server/discover" | "ping")
}

/// The version an `initialize` answers: the one asked for if it is an older revision we
/// know, else `2025-06-18`. Never the current revision, which has no `initialize`.
pub(crate) fn negotiate_legacy_version(requested: &str) -> &'static str {
    SUPPORTED_LEGACY_VERSIONS
        .iter()
        .copied()
        .find(|v| *v == requested)
        .unwrap_or(LEGACY_PROTOCOL_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
    const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

    fn msg(method: &str, params: Option<serde_json::Value>) -> JsonRpcMessage {
        JsonRpcMessage {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: Some(JsonRpcId::Number(1)),
            method: method.to_owned(),
            params,
        }
    }

    fn modern_meta() -> serde_json::Value {
        serde_json::json!({ "_meta": {
            META_PROTOCOL_VERSION: PROTOCOL_VERSION,
            META_CLIENT_CAPABILITIES: {},
        } })
    }

    #[test]
    fn envelope_adds_result_type_cache_hints_and_server_info() {
        let json = serde_json::to_value(ResultEnvelope::new(
            serde_json::json!({ "tools": [] }),
            Some(CacheScope::Private),
        ))
        .unwrap();
        assert_eq!(json["resultType"], "complete");
        assert_eq!(json["tools"], serde_json::json!([]));
        assert_eq!(json["ttlMs"], CATALOG_TTL_MS);
        assert_eq!(json["cacheScope"], "private");
        assert!(json["_meta"].get("ttlMs").is_none());
        assert_eq!(json["_meta"][META_SERVER_INFO]["name"], "knoggin");
        assert_eq!(json["_meta"][META_SERVER_INFO]["version"], SERVER_VERSION);
        let plain = serde_json::to_value(ResultEnvelope::new(serde_json::json!({}), None)).unwrap();
        assert!(plain.get("ttlMs").is_none() && plain.get("cacheScope").is_none());
    }

    #[test]
    fn era_hints() {
        assert_eq!(
            era_hint(&msg("tools/list", Some(modern_meta()))),
            EraHint::Modern
        );
        assert_eq!(era_hint(&msg("initialize", None)), EraHint::LegacyHandshake);
        assert_eq!(
            era_hint(&msg("notifications/initialized", None)),
            EraHint::LegacyHandshake
        );
        assert_eq!(era_hint(&msg("tools/list", None)), EraHint::Ambiguous);
        let progress = serde_json::json!({ "_meta": { "progressToken": 7 } });
        assert_eq!(
            era_hint(&msg("tools/list", Some(progress))),
            EraHint::Ambiguous
        );
        assert_eq!(
            era_hint(&msg("initialize", Some(modern_meta()))),
            EraHint::Modern
        );
    }

    #[test]
    fn meta_errors_name_the_key_and_the_versions() {
        let e = parse_request_meta(None).err().unwrap();
        assert_eq!(e.code(), INVALID_PARAMS);
        assert!(e.message().contains(META_PROTOCOL_VERSION));
        assert!(e.message().contains("initialize"));
        let p = serde_json::json!({ "_meta": { META_PROTOCOL_VERSION: PROTOCOL_VERSION } });
        let e = parse_request_meta(Some(&p)).err().unwrap();
        assert_eq!(e, MetaError::MissingField(META_CLIENT_CAPABILITIES));
        assert!(e.data().is_none());
        let p = serde_json::json!({ "_meta": {
            META_PROTOCOL_VERSION: "2027-01-01", META_CLIENT_CAPABILITIES: {} } });
        let e = parse_request_meta(Some(&p)).err().unwrap();
        assert_eq!(e.code(), UNSUPPORTED_PROTOCOL_VERSION);
        let data = e.data().unwrap();
        assert_eq!(data["requested"], "2027-01-01");
        assert_eq!(data["supported"], serde_json::json!([PROTOCOL_VERSION]));
        let p = serde_json::json!({ "_meta": "x" });
        assert_eq!(
            parse_request_meta(Some(&p)).err(),
            Some(MetaError::Malformed)
        );
    }

    #[test]
    fn a_full_meta_block_parses() {
        let p = serde_json::json!({ "_meta": {
            META_PROTOCOL_VERSION: PROTOCOL_VERSION,
            META_CLIENT_INFO: { "name": "ExampleClient", "version": "1.0.0" },
            META_CLIENT_CAPABILITIES: {},
            "io.modelcontextprotocol/logLevel": "info",
            "progressToken": "tok-1",
        } });
        let meta = parse_request_meta(Some(&p)).ok().unwrap();
        assert_eq!(meta.protocol_version, PROTOCOL_VERSION);
        assert_eq!(meta.client_info.unwrap().name, "ExampleClient");
    }

    #[test]
    fn ids() {
        assert_eq!(
            serde_json::to_value(JsonRpcId::Null).unwrap(),
            serde_json::Value::Null
        );
        let m: JsonRpcMessage =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).unwrap();
        assert!(m.id.is_none(), "an explicit null id is a notification");
        assert_eq!(
            serde_json::from_str::<JsonRpcId>("42").unwrap(),
            JsonRpcId::Number(42)
        );
        assert_eq!(
            serde_json::from_str::<JsonRpcId>(r#""abc""#).unwrap(),
            JsonRpcId::String("abc".into())
        );
    }

    #[test]
    fn legacy_versions_are_echoed_and_never_the_current_one() {
        for v in SUPPORTED_LEGACY_VERSIONS {
            assert_eq!(negotiate_legacy_version(v), *v);
        }
        assert_eq!(
            negotiate_legacy_version("1999-01-01"),
            LEGACY_PROTOCOL_VERSION
        );
        assert_eq!(
            negotiate_legacy_version(PROTOCOL_VERSION),
            LEGACY_PROTOCOL_VERSION
        );
    }

    #[test]
    fn handshake_exempt_methods() {
        assert!(is_handshake_exempt_method("server/discover"));
        assert!(is_handshake_exempt_method("ping"));
        assert!(!is_handshake_exempt_method("tools/list"));
        assert!(!is_handshake_exempt_method("initialize"));
    }

    #[test]
    fn tool_results_are_compact() {
        let r = ToolCallResult {
            content: vec![ContentBlock::Text { text: "{}".into() }],
            structured_content: None,
            is_error: Some(true),
        };
        let json = serde_json::to_value(r).unwrap();
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["isError"], true);
        assert!(json.get("structuredContent").is_none());
    }
}
