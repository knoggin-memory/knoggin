// SPDX-License-Identifier: AGPL-3.0-only
//! An independent MCP client (`rmcp`) connects over streamable HTTP, lists the tools,
//! stores a memory and recalls it, once over the older revisions and once over the current
//! one.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use axum::extract::Request;
use axum::middleware::{self, Next};
use knoggin_http::Permissions;
use rmcp::model::{CallToolRequestParams, ProtocolVersion};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::{Value, json};

use support::Env;

/// Serves the store layer on a loopback port, with a context put in every request as
/// the edge layer would.
async fn serve(env: &Env) -> String {
    let ctx = env.t.ctx(Permissions::READ_WRITE);
    let app = env
        .app
        .clone()
        .layer(middleware::from_fn(move |mut req: Request, next: Next| {
            let ctx = ctx.clone();
            async move {
                req.extensions_mut().insert(ctx);
                next.run(req).await
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn text(result: &rmcp::model::CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let block = result.content[0].as_text().unwrap();
    serde_json::from_str(&block.text).unwrap()
}

#[tokio::test]
async fn rmcp_lists_stores_and_recalls_on_both_revisions() {
    let env = Env::new(true).await;
    let url = serve(&env).await;
    for (lifecycle, summary) in [
        (ClientLifecycleMode::Initialize, "Maya prefers window seats"),
        (
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
            "Maya takes the night train to Porto",
        ),
    ] {
        let transport = StreamableHttpClientTransport::from_uri(url.clone());
        let client = ().serve_with_lifecycle(transport, lifecycle).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 18);
        assert!(tools.iter().any(|t| t.name == "recall_memories"));
        let saved = client
            .call_tool(
                CallToolRequestParams::new("store_memory")
                    .with_arguments(args(json!({ "summary": summary }))),
            )
            .await
            .unwrap();
        let id = text(&saved)["id"].as_str().unwrap().to_owned();
        let recalled = client
            .call_tool(
                CallToolRequestParams::new("recall_memories")
                    .with_arguments(args(json!({ "query": summary }))),
            )
            .await
            .unwrap();
        let hits = text(&recalled);
        assert!(
            hits["memories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == id.as_str()),
            "{hits}"
        );
        client.cancel().await.unwrap();
    }
}
