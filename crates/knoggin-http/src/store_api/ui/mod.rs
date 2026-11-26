// SPDX-License-Identifier: AGPL-3.0-only
//! The web UI's read models, `/v1/ui/*` (tab "Web UI", Read-model routes and Live events).
//! They are store routes that need `memory:read`; none records access or changes
//! anything. A route that takes `namespace` covers every namespace without it, and none
//! reads the `Knoggin-Namespace` header. The shaping is in [`shape`]; the engine only
//! supplies the data.

pub(crate) mod shape;
pub(crate) mod wire;

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use http::StatusCode;
use knoggin_core::model::Phase;
use knoggin_core::ops::ModelStatus;
use knoggin_core::time::MILLIS_PER_DAY;
use knoggin_core::{Store, Timestamp};
use tokio::sync::broadcast::error::RecvError;

use self::shape::{RowQuery, Sort};
use self::wire::*;
use super::extract::{Ctx, QueryMap, memory_id};
use super::{Answer, Host, ok};
use crate::ctx::RequestContext;
use crate::error::ApiError;
use crate::host::live::{LaggedEvent, UiEvent};
use crate::host::{OpenStore, StoreHost};

/// A comment line goes out this often on an event stream.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The read-model routes, under the request timeout.
pub(crate) fn routes() -> Router<Arc<StoreHost>> {
    Router::new()
        .route("/v1/ui/graph", get(graph))
        .route("/v1/ui/memories", get(memories))
        .route("/v1/ui/facets", get(facets))
        .route("/v1/ui/memories/{id}/history", get(history))
        .route("/v1/ui/memories/{id}/neighbors", get(neighbors))
        .route("/v1/ui/search", get(search))
        .route("/v1/ui/activity", get(activity))
        .route("/v1/ui/health", get(health))
        .route("/v1/ui/system", get(system))
}

/// The event stream, which has no time limit.
pub(crate) fn stream_routes() -> Router<Arc<StoreHost>> {
    Router::new().route("/v1/ui/events", get(events))
}

/// A whole number from `min` to `max`, `default` when not sent.
fn bounded(
    q: &mut QueryMap,
    name: &str,
    default: u32,
    min: u32,
    max: u32,
) -> Result<u32, ApiError> {
    let v = q.number(name)?.unwrap_or(default);
    if (min..=max).contains(&v) {
        Ok(v)
    } else {
        Err(ApiError::invalid_argument(
            name,
            format!("{name} must be between {min} and {max}"),
        ))
    }
}

/// One read of the request's store with its open state, on a blocking thread.
async fn read<R: Send + 'static>(
    host: &StoreHost,
    ctx: &RequestContext,
    f: impl FnOnce(&Store, &OpenStore, Timestamp) -> Result<R, ApiError> + Send + 'static,
) -> Result<R, ApiError> {
    host.read_with(ctx, f).await
}

async fn graph(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap) -> Answer {
    let namespace = q.text("namespace")?;
    let offset = q.number("edgeOffset")?.unwrap_or(0) as usize;
    q.finish()?;
    ok(&read(&host, &ctx, move |s, _, now| {
        shape::graph(s, namespace.as_deref(), offset, now)
    })
    .await?)
}

async fn memories(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap) -> Answer {
    let phases = q
        .all("phase")
        .into_iter()
        .map(|p| {
            serde_json::from_value::<Phase>(serde_json::Value::String(p))
                .map_err(|_| ApiError::invalid_argument("phase", "phase has an unknown value"))
        })
        .collect::<Result<_, _>>()?;
    let ids = q
        .all("id")
        .iter()
        .map(|id| memory_id(id))
        .collect::<Result<_, _>>()?;
    let desc = match q.text("desc")?.as_deref() {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => {
            return Err(ApiError::invalid_argument(
                "desc",
                "desc must be true or false",
            ));
        }
    };
    let query = RowQuery {
        namespace: q.text("namespace")?,
        phases,
        tags: q.all("tag"),
        entities: q.all("entity"),
        clients: q.all("client"),
        ids,
        sort: q.word::<Sort>("sort")?.unwrap_or_default(),
        desc,
        offset: q.number("offset")?.unwrap_or(0) as usize,
        limit: bounded(&mut q, "limit", 200, 1, 200)? as usize,
    };
    q.finish()?;
    ok(&read(&host, &ctx, move |s, _, now| {
        shape::memory_page(s, &query, now)
    })
    .await?)
}

async fn facets(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap) -> Answer {
    let namespace = q.text("namespace")?;
    q.finish()?;
    ok(&read(&host, &ctx, move |s, _, _| {
        shape::facets(s, namespace.as_deref())
    })
    .await?)
}

async fn history(
    State(host): Host,
    Ctx(ctx): Ctx,
    Path(id): Path<String>,
    mut q: QueryMap,
) -> Answer {
    let id = memory_id(&id)?;
    let limit = bounded(&mut q, "limit", 20, 1, 100)? as usize;
    q.finish()?;
    ok(&read(&host, &ctx, move |s, _, _| shape::history(s, id, limit)).await?)
}

async fn neighbors(
    State(host): Host,
    Ctx(ctx): Ctx,
    Path(id): Path<String>,
    mut q: QueryMap,
) -> Answer {
    let id = memory_id(&id)?;
    let depth = bounded(&mut q, "depth", 1, 1, 2)? as u8;
    q.finish()?;
    ok(&read(&host, &ctx, move |s, _, _| shape::neighbors(s, id, depth)).await?)
}

async fn search(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap) -> Answer {
    let query = q
        .text("q")?
        .ok_or_else(|| ApiError::invalid_argument("q", "q is required"))?;
    let namespace = q.text("namespace")?;
    let limit = bounded(&mut q, "limit", 10, 1, 50)?;
    q.finish()?;
    ctx.require_read()?;
    let vector = host.query_vector(&query).await;
    ok(&read(&host, &ctx, move |s, _, now| {
        shape::search(
            s,
            &query,
            namespace.as_deref(),
            limit,
            vector.as_deref(),
            now,
        )
    })
    .await?)
}

async fn activity(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap) -> Answer {
    let namespace = q.text("namespace")?;
    let since = q.time("since")?;
    let limit = bounded(&mut q, "limit", 100, 1, 500)? as usize;
    q.finish()?;
    ok(&read(&host, &ctx, move |s, open, now| {
        let events = open.events().recent();
        shape::activity(s, namespace.as_deref(), since, limit, &events, now)
    })
    .await?)
}

async fn health(State(host): Host, Ctx(ctx): Ctx, mut q: QueryMap) -> Answer {
    let namespace = q.text("namespace")?;
    q.finish()?;
    ok(&read(&host, &ctx, move |s, open, now| {
        shape::health(s, namespace.as_deref(), open.snapshot_bytes(), now)
    })
    .await?)
}

/// No sweep for this long in a store that stayed open is a problem.
const SWEEP_LATE: i64 = 2 * MILLIS_PER_DAY;

async fn system(State(host): Host, Ctx(ctx): Ctx, q: QueryMap) -> Answer {
    q.finish()?;
    let (last_sweep, store_problem) = read(&host, &ctx, |_, open, _| {
        Ok((open.last_sweep(), open.degraded()))
    })
    .await?;
    let now = crate::host::now();
    let model = host.model_state();
    let embedding_problem =
        (!matches!(model.state, ModelStatus::Ready)).then_some("The model is not ready");
    let sweeper_problem = (now.millis() - last_sweep.millis() > SWEEP_LATE)
        .then_some("No sweep has run for more than 48 hours");
    let subsystems: Vec<SubsystemHealth> = [
        ("store", store_problem),
        ("embedding", embedding_problem),
        ("sweeper", sweeper_problem),
    ]
    .into_iter()
    .map(|(name, problem)| SubsystemHealth {
        name,
        status: if problem.is_some() { "degraded" } else { "ok" },
        message: problem,
    })
    .collect();
    let degraded = subsystems.iter().any(|s| s.message.is_some());
    let (started_at, uptime) = host.started();
    ok(&SystemStatus {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at,
        uptime_secs: uptime.as_secs(),
        process: ProcessStatus {
            rss_bytes: rss_bytes().await,
        },
        health: HealthStatus {
            status: if degraded { "degraded" } else { "ok" },
            subsystems,
        },
        model,
        recall_latency_ms: host.recall_ms.median(),
        embed_latency_ms: host.embed_ms.median(),
        sweep: SweepStatus {
            last_at: Some(last_sweep),
            next_at: Some(Timestamp::from_millis(
                last_sweep.millis().saturating_add(MILLIS_PER_DAY),
            )),
        },
        clients: host
            .clients(ctx.store_id)
            .into_iter()
            .map(|(name, last_seen_at)| ClientInfo { name, last_seen_at })
            .collect(),
    })
}

/// The process's resident memory: `VmRSS` on Linux, unknown elsewhere (reading it on
/// macOS needs `unsafe` or a child process).
async fn rss_bytes() -> Option<u64> {
    let status = tokio::fs::read_to_string("/proc/self/status").await.ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb.saturating_mul(1024))
}

/// `GET /v1/ui/events`: the store's live events as server-sent events, named by kind.
/// It never opens a store: 204 when the store is not open. It ends when the store closes
/// and when the process stops or parks ([`StoreHost::end_event_streams`]).
async fn events(State(host): Host, Ctx(ctx): Ctx, q: QueryMap) -> Answer {
    q.finish()?;
    ctx.require_read()?;
    let Some(subscription) = host.subscribe(ctx.store_id) else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let stream = futures_util::stream::unfold(subscription, |(mut rx, mut end)| async move {
        loop {
            let received = tokio::select! {
                received = rx.recv() => received,
                _ = end.wait_for(|ended| *ended) => return None,
            };
            let event = match received {
                Ok(event) => event,
                Err(RecvError::Lagged(missed)) => Arc::new(UiEvent::Lagged(LaggedEvent { missed })),
                Err(RecvError::Closed) => return None,
            };
            let Ok(data) = serde_json::to_string(&*event) else {
                continue;
            };
            let sse = Event::default().event(event.kind()).data(data);
            return Some((Ok::<_, Infallible>(sse), (rx, end)));
        }
    });
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response())
}

#[cfg(test)]
mod tests;
