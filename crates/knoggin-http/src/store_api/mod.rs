// SPDX-License-Identifier: AGPL-3.0-only
//! The store layer's axum router: the REST memory routes, export and import (tab "REST
//! API", Store routes), and the web UI's read models and event stream (`ui`). It expects
//! a `RequestContext` in the request extensions; the edge layer (or a container's
//! middleware) puts it there.

pub(crate) mod extract;
pub(crate) mod ui;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use http::{HeaderValue, StatusCode, header};
use knoggin_core::model::NamespacePolicy;
use knoggin_core::ops::{
    CreateNamespace, DeleteNamespace, FindSimilar, ForgetMemories, ForgetMemory, GetContext,
    GetMemory, ListMemories, ListTags, MoveMemories, NamespaceStats, Recall, ReinforceMemory,
    StoreMemories, StoreMemory, SupersedeMemory, UpdatePolicy,
};
use knoggin_core::transfer::OnConflict;
use serde::Serialize;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use self::extract::{Ctx, IdempotencyKey, JsonBody, QueryMap, has_content_type, memory_id};
use crate::ctx::headers;
use crate::error::{ApiError, shape_answer};
use crate::host::{Reply, StoreHost};
use crate::settings::DEFAULT_BODY_BYTES;
use crate::trace;

type Host = State<Arc<StoreHost>>;
type Answer = Result<Response, ApiError>;

/// The store routes over `host`. Expects `RequestContext` in the request extensions.
pub fn store_layer(host: Arc<StoreHost>) -> Router {
    let limits = host.config().limits.clone();
    let timeout = || {
        TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            Duration::from_millis(limits.request_timeout_ms),
        )
    };
    let bounded = |router: Router<Arc<StoreHost>>, bytes: usize| {
        router
            .layer(DefaultBodyLimit::max(bytes))
            .layer(RequestBodyLimitLayer::new(bytes))
    };
    let ordinary = Router::new()
        .route("/v1/memories", post(save).get(list))
        .route("/v1/memories/forget", post(forget_many))
        .route("/v1/memories/move", post(move_memories))
        .route("/v1/memories/{id}", get(get_memory).delete(forget))
        .route("/v1/memories/{id}/reinforce", post(reinforce))
        .route("/v1/memories/{id}/supersede", post(supersede))
        .route("/v1/memories/{id}/similar", post(similar))
        .route("/v1/recall", post(recall))
        .route("/v1/context", get(context))
        .route("/v1/namespaces", get(namespaces).post(create_namespace))
        .route(
            "/v1/namespaces/{name}",
            axum::routing::delete(delete_namespace),
        )
        .route("/v1/namespaces/{name}/stats", get(namespace_stats))
        .route("/v1/namespaces/{name}/policy", put(update_policy))
        .route("/v1/tags", get(tags))
        .merge(ui::routes());
    let ordinary = bounded(ordinary, DEFAULT_BODY_BYTES).layer(timeout());
    let batch = bounded(
        Router::new().route("/v1/memories/batch", post(save_batch)),
        limits.body_bytes,
    )
    .layer(timeout());
    // The three long-running routes have no time limit; the import streams, without a
    // limit on its whole body.
    let export = bounded(
        Router::new()
            .route("/v1/export", get(export))
            .merge(ui::stream_routes()),
        DEFAULT_BODY_BYTES,
    );
    let import = Router::new()
        .route("/v1/import", post(import))
        .layer(DefaultBodyLimit::disable());
    // `store_memories` can carry 100 memories, so the MCP endpoint takes the batch limit.
    let mcp = crate::mcp::routes(host.clone())
        .layer(DefaultBodyLimit::max(limits.body_bytes))
        .layer(RequestBodyLimitLayer::new(limits.body_bytes))
        .layer(timeout());
    ordinary
        .merge(batch)
        .merge(export)
        .merge(import)
        .with_state(host)
        .merge(mcp)
        .layer(middleware::from_fn(normalize))
        .layer(middleware::from_fn(trace::observe))
}

/// The store layer's way out (see [`shape_answer`]).
async fn normalize(request: Request, next: Next) -> Response {
    let id = trace::request_id(&request);
    shape_answer(next.run(request).await, id)
}

fn json(status: StatusCode, body: Vec<u8>) -> Response {
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn ok<T: Serialize>(value: &T) -> Answer {
    let body = serde_json::to_vec(value).map_err(|_| ApiError::internal())?;
    Ok(json(StatusCode::OK, body))
}

fn reply<T: Serialize>(status: StatusCode, reply: Reply<T>) -> Answer {
    let replayed = reply.is_replayed();
    let mut response = json(status, reply.into_json()?);
    if replayed {
        response.headers_mut().insert(
            headers::IDEMPOTENT_REPLAYED,
            HeaderValue::from_static("true"),
        );
    }
    Ok(response)
}

async fn save(
    State(host): Host,
    Ctx(ctx): Ctx,
    q: QueryMap,
    IdempotencyKey(key): IdempotencyKey,
    JsonBody(req): JsonBody<StoreMemory>,
) -> Answer {
    q.finish()?;
    reply(
        StatusCode::CREATED,
        host.store_memory(&ctx, req, key).await?,
    )
}

async fn save_batch(
    State(host): Host,
    Ctx(ctx): Ctx,
    q: QueryMap,
    IdempotencyKey(key): IdempotencyKey,
    JsonBody(req): JsonBody<StoreMemories>,
) -> Answer {
    q.finish()?;
    reply(StatusCode::OK, host.store_memories(&ctx, req, key).await?)
}

async fn list(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap) -> Answer {
    let req = ListMemories {
        namespace: q.text("namespace")?,
        tags: q.all("tag"),
        phase: q.word("phase")?,
        created_after: q.time("createdAfter")?,
        created_before: q.time("createdBefore")?,
        include_full_text: q.flag("includeFullText", false)?,
        limit: q
            .number("limit")?
            .unwrap_or(knoggin_core::model::limits::DEFAULT_PAGE_LIMIT),
        cursor: q.text("cursor")?,
    };
    q.finish()?;
    ok(&host.list_memories(&ctx, req).await?)
}

async fn get_memory(
    State(host): Host,
    Ctx(ctx): Ctx,
    Path(id): Path<String>,
    mut q: QueryMap,
) -> Answer {
    let req = GetMemory {
        id: memory_id(&id)?,
        namespace: q.text("namespace")?,
        record_access: q.flag("recordAccess", true)?,
        include_history: q.flag("includeHistory", false)?,
    };
    q.finish()?;
    ok(&host.get_memory(&ctx, req).await?)
}

async fn forget(
    State(host): Host,
    Ctx(ctx): Ctx,
    Path(id): Path<String>,
    mut q: QueryMap,
    IdempotencyKey(key): IdempotencyKey,
) -> Answer {
    let req = ForgetMemory {
        id: memory_id(&id)?,
        namespace: q.text("namespace")?,
    };
    q.finish()?;
    reply(StatusCode::OK, host.forget_memory(&ctx, req, key).await?)
}

async fn forget_many(
    State(host): Host,
    Ctx(ctx): Ctx,
    q: QueryMap,
    IdempotencyKey(key): IdempotencyKey,
    JsonBody(req): JsonBody<ForgetMemories>,
) -> Answer {
    q.finish()?;
    reply(StatusCode::OK, host.forget_memories(&ctx, req, key).await?)
}

async fn reinforce(
    State(host): Host,
    Ctx(ctx): Ctx,
    Path(id): Path<String>,
    q: QueryMap,
    IdempotencyKey(key): IdempotencyKey,
    JsonBody(mut req): JsonBody<ReinforceMemory>,
) -> Answer {
    q.finish()?;
    req.id = memory_id(&id)?;
    reply(StatusCode::OK, host.reinforce_memory(&ctx, req, key).await?)
}

async fn supersede(
    State(host): Host,
    Ctx(ctx): Ctx,
    Path(id): Path<String>,
    q: QueryMap,
    IdempotencyKey(key): IdempotencyKey,
    JsonBody(mut req): JsonBody<SupersedeMemory>,
) -> Answer {
    q.finish()?;
    req.id = memory_id(&id)?;
    reply(
        StatusCode::CREATED,
        host.supersede_memory(&ctx, req, key).await?,
    )
}

async fn move_memories(
    State(host): Host,
    Ctx(ctx): Ctx,
    q: QueryMap,
    IdempotencyKey(key): IdempotencyKey,
    JsonBody(req): JsonBody<MoveMemories>,
) -> Answer {
    q.finish()?;
    reply(StatusCode::OK, host.move_memories(&ctx, req, key).await?)
}

async fn similar(
    State(host): Host,
    Ctx(ctx): Ctx,
    Path(id): Path<String>,
    q: QueryMap,
    JsonBody(mut req): JsonBody<FindSimilar>,
) -> Answer {
    q.finish()?;
    req.id = memory_id(&id)?;
    ok(&host.find_similar(&ctx, req).await?)
}

async fn recall(
    State(host): Host,
    Ctx(ctx): Ctx,
    q: QueryMap,
    JsonBody(req): JsonBody<Recall>,
) -> Answer {
    q.finish()?;
    ok(&host.recall(&ctx, req).await?)
}

async fn context(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap) -> Answer {
    let req = GetContext {
        topic: q.text("topic")?,
        namespace: q.text("namespace")?,
        limit: q
            .number("limit")?
            .unwrap_or(knoggin_core::model::limits::DEFAULT_CONTEXT_LIMIT),
    };
    q.finish()?;
    ok(&host.context(&ctx, req).await?)
}

async fn namespaces(State(host): Host, Ctx(ctx): Ctx, q: QueryMap) -> Answer {
    q.finish()?;
    ok(&host.list_namespaces(&ctx).await?)
}

async fn create_namespace(
    State(host): Host,
    Ctx(ctx): Ctx,
    q: QueryMap,
    IdempotencyKey(key): IdempotencyKey,
    JsonBody(req): JsonBody<CreateNamespace>,
) -> Answer {
    q.finish()?;
    reply(
        StatusCode::CREATED,
        host.create_namespace(&ctx, req, key).await?,
    )
}

async fn namespace_stats(
    State(host): Host,
    Ctx(ctx): Ctx,
    Path(name): Path<String>,
    q: QueryMap,
) -> Answer {
    q.finish()?;
    let req = NamespaceStats {
        namespace: Some(name),
    };
    ok(&host.namespace_stats(&ctx, req).await?)
}

async fn update_policy(
    State(host): Host,
    Ctx(ctx): Ctx,
    Path(name): Path<String>,
    mut q: QueryMap,
    IdempotencyKey(key): IdempotencyKey,
    JsonBody(policy): JsonBody<NamespacePolicy>,
) -> Answer {
    let req = UpdatePolicy {
        name,
        policy,
        dry_run: q.flag("dryRun", false)?,
    };
    q.finish()?;
    reply(
        StatusCode::OK,
        host.update_namespace_policy(&ctx, req, key).await?,
    )
}

async fn delete_namespace(
    State(host): Host,
    Ctx(ctx): Ctx,
    Path(name): Path<String>,
    mut q: QueryMap,
    IdempotencyKey(key): IdempotencyKey,
) -> Answer {
    let req = DeleteNamespace {
        name,
        force: q.flag("force", false)?,
        dry_run: q.flag("dryRun", false)?,
    };
    q.finish()?;
    reply(StatusCode::OK, host.delete_namespace(&ctx, req, key).await?)
}

async fn tags(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap) -> Answer {
    let req = ListTags {
        namespace: q.text("namespace")?,
        prefix: q.text("prefix")?,
        limit: q
            .number("limit")?
            .unwrap_or(knoggin_core::model::limits::DEFAULT_PAGE_LIMIT),
        cursor: q.text("cursor")?,
    };
    q.finish()?;
    ok(&host.list_tags(&ctx, req).await?)
}

/// `GET /v1/export`. The `store` parameter was resolved and checked by the edge layer;
/// the edge layer also sets `Content-Disposition` with [`export_file_name`].
async fn export(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap) -> Answer {
    let _ = q.text("store")?;
    q.finish()?;
    let stream = host.export(&ctx).await?;
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    Ok(response)
}

/// `knoggin-<name>-<date>.jsonl`, with every character of the store's name outside
/// `A-Z a-z 0-9 _ -` turned into `_`, and `date` as `YYYY-MM-DD`.
pub fn export_file_name(store_name: &str, date: &str) -> String {
    let name: String = store_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("knoggin-{name}-{date}.jsonl")
}

async fn import(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap, request: Request) -> Answer {
    let on_conflict = q.word::<OnConflict>("onConflict")?.unwrap_or_default();
    q.finish()?;
    if !has_content_type(request.headers(), "application/x-ndjson") {
        return Err(ApiError::invalid_argument(
            "Content-Type",
            "an import must be sent as application/x-ndjson",
        ));
    }
    ok(&host.import(&ctx, on_conflict, request.into_body()).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_file_names_keep_only_safe_characters() {
        assert_eq!(
            export_file_name("Work notes/ä", "2026-11-25"),
            "knoggin-Work_notes__-2026-11-25.jsonl"
        );
    }
}
