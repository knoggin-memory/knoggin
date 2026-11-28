// SPDX-License-Identifier: AGPL-3.0-only
//! The router's three mode routes: the
//! store status (from the lease and the container's `status`, which does not wake it),
//! the admin status (from the container records, the rollout record and the users), and
//! the admin snapshot list (from the bucket, labelled by the same `thin` the lifecycle
//! task uses). Their shapes are the daemon's.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use http::{HeaderValue, StatusCode, header};
use knoggin_core::{StoreId, Timestamp};
use knoggin_http::ApiError;
use knoggin_http::edge::Caller;
use knoggin_state::record::{ContainerState, LeaseState};
use knoggin_state::{Stores, Users};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::lease::{Fleet, db_error, peer_of};

/// The `status` limit of the store status route.
const STATUS_LIMIT: Duration = Duration::from_secs(2);

/// What the admin status reports about this deployment.
#[derive(Debug, Clone)]
pub struct StatusInfo {
    /// `server.image_digest`.
    pub image_digest: Option<String>,
    /// `local_file`, `postgres` or `dynamodb`.
    pub state_db: &'static str,
    /// `keychain`, `key_file` or `kms`.
    pub key_provider: &'static str,
    /// `docker` or `ecs`.
    pub launcher: &'static str,
    /// `storage.bucket`.
    pub bucket: String,
    /// `storage.region`.
    pub region: Option<String>,
    /// `retention.days`, for the snapshot labels.
    pub retention_days: u32,
}

#[derive(Clone)]
struct ModeState {
    fleet: Arc<Fleet>,
    stores: Stores,
    users: Users,
    info: Arc<StatusInfo>,
}

/// The router's mode routes for `edge_layer`.
pub fn mode_routes(fleet: Arc<Fleet>, stores: Stores, users: Users, info: StatusInfo) -> Router {
    Router::new()
        .route("/v1/stores/{id}/status", get(store_status))
        .route("/v1/admin/status", get(admin_status))
        .route("/v1/admin/stores/{id}/snapshots", get(snapshots))
        .with_state(ModeState {
            fleet,
            stores,
            users,
            info: Arc::new(info),
        })
}

type Answer = Result<Response, ApiError>;

fn ok<T: Serialize>(value: &T) -> Answer {
    let body = serde_json::to_vec(value).map_err(|_| ApiError::internal())?;
    let mut r = (StatusCode::OK, body).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(r)
}

fn store_id(text: &str) -> Result<StoreId, ApiError> {
    Uuid::parse_str(text)
        .map(StoreId::from_uuid)
        .map_err(|_| ApiError::invalid_argument("id", "the ID must be a UUID"))
}

async fn store_status(
    State(s): State<ModeState>,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
) -> Answer {
    let id = store_id(&id)?;
    let record = s
        .stores
        .get_of(c.user.value.id, id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::not_found("No such store"))?;
    let lease = s
        .fleet
        .leases
        .get(record.value.slot())
        .await
        .map_err(db_error)?;
    let parked = json!({ "state": "parked", "lockedReason": null, "full": false, "model": null, "embedding": null });
    let waking = json!({ "state": "waking", "lockedReason": null, "full": false, "model": null, "embedding": null });
    let Some(lease) = lease else {
        return ok(&parked);
    };
    match lease.value.state {
        LeaseState::Parked => return ok(&parked),
        LeaseState::Claiming => return ok(&waking),
        LeaseState::Warm => {}
    }
    let (Some(cid), Some(peer)) = (
        lease.value.container_id,
        peer_of(
            lease.value.address.as_deref(),
            lease.value.cert_sha256.as_deref(),
        ),
    ) else {
        return ok(&waking);
    };
    let Some(answer) = s.fleet.status(cid, peer, Some(id), STATUS_LIMIT).await else {
        return ok(&waking);
    };
    let model = serde_json::to_value(&answer.model).unwrap_or(Value::Null);
    let view = if let Some(r) = answer.stores.iter().find(|r| r.store_id == id) {
        json!({
            "state": "ready", "lockedReason": null, "full": r.full, "model": model,
            "embedding": { "pending": r.pending_embeddings, "total": r.total_embeddable },
        })
    } else if answer.opening.contains(&id) {
        json!({ "state": "waking", "lockedReason": null, "full": false, "model": model, "embedding": null })
    } else if let Some(l) = answer.locks.iter().find(|l| l.store_id == id) {
        json!({ "state": "locked", "lockedReason": l.reason.as_str(), "full": false, "model": model, "embedding": null })
    } else {
        json!({ "state": "parked", "lockedReason": null, "full": false, "model": model, "embedding": null })
    };
    ok(&view)
}

async fn admin_status(State(s): State<ModeState>) -> Answer {
    let records = s.fleet.containers.list().await.map_err(db_error)?;
    let count = |state| records.iter().filter(|c| c.value.state == state).count();
    let rollout = s.fleet.meta.rollout().await.map_err(db_error)?;
    let mut total = 0u64;
    let mut deleting = 0u64;
    let mut after = None;
    loop {
        let page = s.users.list(100, after).await.map_err(db_error)?;
        for u in &page {
            total += 1;
            deleting += u64::from(u.value.deleted_at_ms.is_some());
        }
        match page.last() {
            Some(last) if page.len() == 100 => after = Some(last.value.id),
            _ => break,
        }
    }
    let config = s.fleet.config();
    let rollout_view = if config.one_host {
        Value::Null
    } else {
        rollout.as_ref().map_or(
            Value::Null,
            |r| json!({ "claimBuild": r.value.claim.build, "desiredBuild": r.value.desired.build }),
        )
    };
    let retire = rollout
        .as_ref()
        .and_then(|r| r.value.retire_before_ms)
        .and_then(|t| Timestamp::from_millis(t).to_rfc3339());
    ok(&json!({
        "version": config.build.version,
        "imageDigest": s.info.image_digest,
        "mode": "server",
        "backends": {
            "stateDb": s.info.state_db,
            "objectStorage": "s3",
            "keyProvider": s.info.key_provider,
            "launcher": s.info.launcher,
        },
        "storage": { "bucket": s.info.bucket, "region": s.info.region },
        "dataDir": null,
        "containers": {
            "claimed": count(ContainerState::Claimed),
            "ready": count(ContainerState::Ready),
            "starting": count(ContainerState::Starting),
        },
        "pool": { "target": config.containers.pool_target },
        "rollout": rollout_view,
        "users": { "total": total, "deleting": deleting },
        "retireBefore": retire,
    }))
}

async fn snapshots(State(s): State<ModeState>, Path(id): Path<String>) -> Answer {
    let id = store_id(&id)?;
    let record = s
        .stores
        .get(id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::not_found("No such store"))?;
    let list = crate::admin::snapshot_list(
        &*s.fleet.storage,
        &record.value,
        s.info.retention_days,
        Timestamp::from_millis(s.fleet.now()),
    )
    .await
    .map_err(|e| {
        tracing::warn!(kind = "bucket", error = %e, "the snapshot list failed");
        ApiError::unavailable(2)
    })?;
    ok(&list)
}
