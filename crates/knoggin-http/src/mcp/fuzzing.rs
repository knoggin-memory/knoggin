// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP request and argument parser, opened to `fuzz/`. Not an API: hidden from the
//! docs, and no optional feature switches it on (the workspace has none).

use knoggin_core::ops;
use serde_json::Value;

use super::args::{self, parse};
use super::http::cut_client;
use super::protocol::{
    InitializeParams, JsonRpcId, JsonRpcMessage, ToolCallParams, era_hint,
    negotiate_legacy_version, parse_request_meta,
};

/// Parses `body` as `POST /mcp` does, then the parameters of `initialize` and of
/// `tools/call`, and the call's arguments as every tool reads them.
pub fn parse_request(body: &[u8]) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return;
    };
    let _ = value
        .get("id")
        .and_then(|id| serde_json::from_value::<JsonRpcId>(id.clone()).ok());
    let Ok(message) = serde_json::from_value::<JsonRpcMessage>(value) else {
        return;
    };
    let _ = era_hint(&message);
    if let Ok(meta) = parse_request_meta(message.params.as_ref()) {
        let _ = meta.client_info.as_ref().and_then(|c| cut_client(&c.name));
    }
    let Some(params) = message.params else {
        return;
    };
    if let Ok(p) = serde_json::from_value::<InitializeParams>(params.clone()) {
        let _ = negotiate_legacy_version(&p.protocol_version);
        let _ = cut_client(&p.client_info.name);
    }
    if let Ok(call) = serde_json::from_value::<ToolCallParams>(params) {
        tool_arguments(&call.arguments);
    }
}

/// Every tool's argument parse and its conversion into the engine's request.
fn tool_arguments(a: &Value) {
    let _ = parse::<args::GetContextArgs>(a.clone()).map(ops::GetContext::from);
    let _ = parse::<args::RecallArgs>(a.clone()).map(ops::Recall::from);
    let _ = parse::<args::GetMemoryArgs>(a.clone()).map(ops::GetMemory::from);
    let _ = parse::<args::FindSimilarArgs>(a.clone()).map(ops::FindSimilar::from);
    let _ = parse::<args::ListMemoriesArgs>(a.clone()).map(ops::ListMemories::from);
    let _ = parse::<args::NoArgs>(a.clone());
    let _ = parse::<args::ListTagsArgs>(a.clone()).map(ops::ListTags::from);
    let _ = parse::<args::NamespaceStatsArgs>(a.clone()).map(ops::NamespaceStats::from);
    let _ = parse::<args::StoreMemoryArgs>(a.clone()).map(args::StoreMemoryArgs::split);
    let _ = parse::<args::StoreMemoriesArgs>(a.clone()).map(args::StoreMemoriesArgs::split);
    let _ = parse::<args::SupersedeArgs>(a.clone()).map(args::SupersedeArgs::split);
    let _ = parse::<args::ReinforceArgs>(a.clone()).map(args::ReinforceArgs::split);
    let _ = parse::<args::MoveArgs>(a.clone()).map(args::MoveArgs::split);
    let _ = parse::<args::CreateNamespaceArgs>(a.clone()).map(args::CreateNamespaceArgs::split);
    let _ = parse::<args::UpdatePolicyArgs>(a.clone()).map(args::UpdatePolicyArgs::split);
    let _ = parse::<args::ForgetArgs>(a.clone()).map(args::ForgetArgs::split);
    let _ = parse::<args::ForgetManyArgs>(a.clone()).map(args::ForgetManyArgs::split);
    let _ = parse::<args::DeleteNamespaceArgs>(a.clone()).map(args::DeleteNamespaceArgs::split);
}
