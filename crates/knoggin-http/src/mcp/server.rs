// SPDX-License-Identifier: AGPL-3.0-only
//! The stateless method dispatcher, ported from recalld's `src/mcp/server.rs`. The
//! transport decides the revision and the request context; this answers the method.

use serde_json::Value;

use super::McpState;
use super::protocol::{
    CacheScope, ContentBlock, DiscoverResult, Era, INVALID_PARAMS, JsonRpcId, JsonRpcResponse,
    METHOD_NOT_FOUND, ResultEnvelope, SUPPORTED_MODERN_VERSIONS, ToolCallParams, ToolCallResult,
    server_capabilities,
};
use super::tools::{self, ToolSet};
use crate::ctx::RequestContext;

/// One request as the transport resolved it.
pub(crate) struct Call<'a> {
    pub(crate) era: Era,
    pub(crate) set: ToolSet,
    pub(crate) ctx: &'a RequestContext,
}

/// A success wrapped in the result envelope.
pub(crate) fn success(id: JsonRpcId, payload: Value, cache: Option<CacheScope>) -> JsonRpcResponse {
    match serde_json::to_value(ResultEnvelope::new(payload, cache)) {
        Ok(v) => JsonRpcResponse::success(id, v),
        Err(_) => {
            JsonRpcResponse::error(id, super::protocol::INTERNAL_ERROR, "Internal error", None)
        }
    }
}

/// Answers every method except `initialize`, which the transport handles because it
/// makes a session.
pub(crate) async fn dispatch(
    state: &McpState,
    call: Call<'_>,
    id: JsonRpcId,
    method: &str,
    params: Option<Value>,
) -> JsonRpcResponse {
    match (call.era, method) {
        (_, "server/discover") => {
            let result = DiscoverResult {
                supported_versions: SUPPORTED_MODERN_VERSIONS,
                capabilities: server_capabilities(),
                instructions: state.instructions(),
            };
            success(
                id,
                serde_json::to_value(result).unwrap_or_default(),
                Some(CacheScope::Public),
            )
        }
        (Era::Legacy, "ping") => success(id, serde_json::json!({}), None),
        (_, "tools/list") => {
            let tools = tools::list(call.set, call.ctx, call.era == Era::Modern);
            // The list depends on the credential, so only that credential may reuse it.
            success(
                id,
                serde_json::json!({ "tools": tools }),
                Some(CacheScope::Private),
            )
        }
        (_, "tools/call") => {
            let params: ToolCallParams = match params.map(serde_json::from_value) {
                Some(Ok(p)) => p,
                _ => {
                    return JsonRpcResponse::error(
                        id,
                        INVALID_PARAMS,
                        "Invalid params: tools/call needs a tool name",
                        None,
                    );
                }
            };
            let out = tools::call(
                &state.host,
                call.ctx,
                call.set,
                &params.name,
                params.arguments,
            )
            .await;
            if let Some((tool, code)) = out.logged {
                tracing::info!(tool, code, "tool call");
            }
            let structured = (call.era == Era::Modern).then_some(out.value);
            let result = ToolCallResult {
                content: vec![ContentBlock::Text { text: out.text }],
                structured_content: structured,
                is_error: out.is_error.then_some(true),
            };
            success(id, serde_json::to_value(result).unwrap_or_default(), None)
        }
        // The current revision removed `initialize` and `ping`; resources, prompts and
        // everything else are not served.
        (_, other) => JsonRpcResponse::error(
            id,
            METHOD_NOT_FOUND,
            format!("Method not found: {other}"),
            None,
        ),
    }
}
