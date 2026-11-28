// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP endpoint: the protocol layer ported from recalld, the streamable HTTP transport,
//! and the two tool sets at `/mcp` and `/mcp/plugin`. Every tool calls the same `StoreHost`
//! method as its REST route. The store layer mounts `routes`.

mod args;
#[doc(hidden)]
pub mod fuzzing;
mod http;
mod protocol;
mod server;
mod tools;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::routing::post;

use self::tools::ToolSet;
use crate::host::StoreHost;

/// The settings the MCP endpoint reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    /// The setting `source_url`, named at the end of the server instructions.
    pub source_url: String,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            source_url: env!("CARGO_PKG_REPOSITORY").to_owned(),
        }
    }
}

/// Marks an answer of the MCP endpoint, which the store layer passes on as it is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Answered;

/// What both endpoints share: the host and the sessions of the older revisions.
pub(crate) struct McpState {
    host: Arc<StoreHost>,
    sessions: Mutex<http::Sessions>,
}

impl McpState {
    /// The server instructions, with the deployment's addresses.
    fn instructions(&self) -> String {
        let config = self.host.config();
        instructions(config.web_url.as_deref(), &config.mcp.source_url)
    }
}

/// The `instructions` of `initialize` and `server/discover`, the same on both tool sets. The
/// text tells an agent to save what the user asks it to keep, or what its own instructions say
/// to keep, and to say what it saved; it never asks the agent to wait for a yes. A test
/// compares it byte for byte.
pub(crate) fn instructions(web_url: Option<&str>, source_url: &str) -> String {
    let browse = match web_url {
        Some(url) => format!("The user can browse and manage memories at {url}."),
        None => "The user can browse and manage memories by running knoggin open.".to_owned(),
    };
    format!(
        "Knoggin is this user's long-term memory. When a request depends on earlier context, call get_context or recall_memories. Save something when the user asks you to remember it, or when your own instructions tell you to keep it, and tell the user what you saved. Never save passwords, keys or other secrets. Namespaces organize memories; they are not access controls. {browse} Source code: {source_url}."
    )
}

/// `/mcp` and `/mcp/plugin` over `host`. Expects `RequestContext` in the extensions; the
/// store layer adds the body limit, the timeout and the request log around it.
pub(crate) fn routes(host: Arc<StoreHost>) -> Router {
    let state = Arc::new(McpState {
        host,
        sessions: Mutex::new(http::Sessions::default()),
    });
    let endpoint = |set: ToolSet| {
        post(move |s, c, h, b| http::post(s, set, c, h, b))
            .delete(move |s, c, h| http::delete(s, set, c, h))
            .get(http::get)
    };
    Router::new()
        .route("/mcp", endpoint(ToolSet::Full))
        .route("/mcp/plugin", endpoint(ToolSet::Plugin))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server instructions, byte for byte, with and without a web address.
    #[test]
    fn instructions_are_the_fixed_text() {
        assert_eq!(
            instructions(
                Some("https://memory.example.com"),
                "https://github.com/knoggin/knoggin"
            ),
            "Knoggin is this user's long-term memory. When a request depends on earlier context, call get_context or recall_memories. Save something when the user asks you to remember it, or when your own instructions tell you to keep it, and tell the user what you saved. Never save passwords, keys or other secrets. Namespaces organize memories; they are not access controls. The user can browse and manage memories at https://memory.example.com. Source code: https://github.com/knoggin/knoggin."
        );
        assert_eq!(
            instructions(None, "https://x.example"),
            "Knoggin is this user's long-term memory. When a request depends on earlier context, call get_context or recall_memories. Save something when the user asks you to remember it, or when your own instructions tell you to keep it, and tell the user what you saved. Never save passwords, keys or other secrets. Namespaces organize memories; they are not access controls. The user can browse and manage memories by running knoggin open. Source code: https://x.example."
        );
    }
}
