// SPDX-License-Identifier: AGPL-3.0-only
//! The JSON shapes of the control routes (tab "REST API", Shared shapes and Control
//! routes), with their OpenAPI schemas under the public names.

use knoggin_core::ops::{EmbeddingProgress, ModelState};
use knoggin_core::time::{MILLIS_PER_DAY, Timestamp};
use knoggin_core::{StoreId, UserId};
use knoggin_state::record::{ApiKey, Permission, StoreKind, StoreRecord};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::LockReason;
use crate::host::{StoreState, StoreStatus};

/// `personal` or `app`.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename = "StoreKind")]
pub(crate) enum Kind {
    Personal,
    App,
}

/// `active` or `deleted`.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename = "StoreLifecycle")]
pub(crate) enum Status {
    Active,
    Deleted,
}

/// An account-level store record, not its memories.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = Store)]
#[ts(rename = "Store")]
pub(crate) struct StoreView {
    pub(crate) id: StoreId,
    pub(crate) kind: Kind,
    pub(crate) name: String,
    pub(crate) created_at: Timestamp,
    pub(crate) memory_count: u64,
    pub(crate) last_write_at: Option<Timestamp>,
    pub(crate) last_write_client: Option<String>,
    pub(crate) namespace_count: u64,
    pub(crate) status: Status,
    pub(crate) deleted_at: Option<Timestamp>,
    pub(crate) delete_after: Option<Timestamp>,
}

impl StoreView {
    /// The record's view. `live_count` is the open store's memory count on the daemon.
    pub(crate) fn new(r: &StoreRecord, soft_delete_days: u32, live_count: Option<u64>) -> Self {
        Self {
            id: r.id,
            kind: match r.kind {
                StoreKind::Personal => Kind::Personal,
                StoreKind::App => Kind::App,
            },
            name: r.name.expose().clone(),
            created_at: Timestamp::from_millis(r.created_ms),
            memory_count: live_count.unwrap_or(r.memory_count),
            last_write_at: r.last_write_ms.map(Timestamp::from_millis),
            last_write_client: r.last_write_client.as_ref().map(|c| c.expose().clone()),
            namespace_count: r.namespace_count,
            status: if r.deleted_at_ms.is_some() {
                Status::Deleted
            } else {
                Status::Active
            },
            deleted_at: r.deleted_at_ms.map(Timestamp::from_millis),
            delete_after: r.deleted_at_ms.map(|t| delete_after(t, soft_delete_days)),
        }
    }
}

/// The end of the undo period of a deletion at `at_ms`.
pub(crate) fn delete_after(at_ms: i64, soft_delete_days: u32) -> Timestamp {
    Timestamp::from_millis(at_ms.saturating_add(i64::from(soft_delete_days) * MILLIS_PER_DAY))
}

/// An API key, without its secret.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = ApiKey)]
#[ts(rename = "ApiKey")]
pub(crate) struct KeyView {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) prefix: String,
    #[schema(value_type = Vec<String>, example = json!(["memory:read", "memory:write"]))]
    pub(crate) permissions: Vec<&'static str>,
    pub(crate) created_at: Timestamp,
    pub(crate) expires_at: Timestamp,
    /// `YYYY-MM-DD`, or `null`.
    pub(crate) last_used_on: Option<String>,
    pub(crate) revoked_at: Option<Timestamp>,
}

impl KeyView {
    pub(crate) fn new(k: &ApiKey) -> Self {
        Self {
            id: k.id,
            name: k.name.expose().clone(),
            prefix: k.prefix.clone(),
            permissions: k
                .permissions
                .iter()
                .map(|p| match p {
                    Permission::Read => "memory:read",
                    Permission::Write => "memory:write",
                })
                .collect(),
            created_at: Timestamp::from_millis(k.created_ms),
            expires_at: Timestamp::from_millis(k.key_expires_ms),
            last_used_on: k.last_used_day.clone(),
            revoked_at: k.revoked_ms.map(Timestamp::from_millis),
        }
    }
}

/// A new key and its secret, which only this answer carries.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = CreatedApiKey)]
#[ts(rename = "CreatedApiKey")]
pub(crate) struct CreatedKey {
    pub(crate) key: KeyView,
    /// `knoggin_live_...`.
    pub(crate) secret: String,
}

/// The body of `POST /v1/stores` and `PATCH /v1/stores/{id}`.
#[derive(Deserialize, ToSchema, ts_rs::TS)]
#[serde(deny_unknown_fields)]
#[schema(as = StoreName)]
#[ts(rename = "StoreName")]
pub(crate) struct NameBody {
    pub(crate) name: String,
}

/// The body of `POST /v1/stores/{id}/keys`.
#[derive(Deserialize, ToSchema, ts_rs::TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schema(as = CreateApiKey)]
#[ts(rename = "CreateApiKey")]
pub(crate) struct CreateKey {
    pub(crate) name: String,
    /// `["memory:read"]` or both (the default).
    pub(crate) permissions: Option<Vec<String>>,
    /// 30, 90 (the default), 180 or 365.
    pub(crate) expires_in_days: Option<u32>,
}

/// The optional body of a rotation.
#[derive(Deserialize, ToSchema, ts_rs::TS)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schema(as = RotateApiKey)]
#[ts(rename = "RotateApiKey")]
pub(crate) struct RotateKey {
    pub(crate) expires_in_days: Option<u32>,
}

/// The body of `DELETE /v1/account`.
#[derive(Deserialize, ToSchema, ts_rs::TS)]
#[serde(deny_unknown_fields)]
#[schema(as = DeleteAccount)]
#[ts(rename = "DeleteAccount")]
pub(crate) struct DeleteAccount {
    /// Exactly `delete my data`.
    pub(crate) confirm: String,
}

/// The answer of `DELETE /v1/account`.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = AccountDeleted)]
#[ts(rename = "AccountDeleted")]
pub(crate) struct AccountDeleted {
    pub(crate) deleted_at: Timestamp,
    pub(crate) delete_after: Timestamp,
}

/// `GET /healthz`.
#[derive(Serialize, ToSchema, ts_rs::TS)]
pub(crate) struct Health {
    pub(crate) status: &'static str,
    pub(crate) version: String,
}

/// `GET /readyz`.
#[derive(Serialize, ToSchema, ts_rs::TS)]
pub(crate) struct Ready {
    pub(crate) status: &'static str,
}

/// The identity provider, when sign-in is on.
#[derive(Serialize, ToSchema, ts_rs::TS)]
pub(crate) struct Provider {
    pub(crate) name: String,
}

/// The user of a session answer.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = SessionUser)]
#[ts(rename = "SessionUser")]
pub(crate) struct SessionUser {
    pub(crate) id: UserId,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub(crate) name: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub(crate) email: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub(crate) is_admin: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub(crate) deleted_at: Option<Option<Timestamp>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub(crate) delete_after: Option<Option<Timestamp>>,
}

/// The credential of a session answer.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = SessionCredential)]
#[ts(rename = "SessionCredential")]
pub(crate) struct SessionCredential {
    /// `session`, `oauth`, `api_key`, `local` or `none`.
    pub(crate) kind: &'static str,
    #[schema(value_type = Vec<String>)]
    pub(crate) permissions: Vec<&'static str>,
    pub(crate) store_id: Option<StoreId>,
}

/// `GET /v1/session`.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = Session)]
#[ts(rename = "Session")]
pub(crate) struct SessionView {
    /// `local` or `server`.
    pub(crate) mode: &'static str,
    /// `none` or `oauth`.
    pub(crate) auth: &'static str,
    pub(crate) version: String,
    pub(crate) source_url: String,
    pub(crate) docs_url: String,
    pub(crate) public_url: String,
    pub(crate) mcp_url: String,
    pub(crate) signed_in: bool,
    /// `expired`, `account_removed` or `not_allowed`, when a credential was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub(crate) signed_out_reason: Option<&'static str>,
    pub(crate) provider: Option<Provider>,
    pub(crate) user: Option<SessionUser>,
    pub(crate) credential: Option<SessionCredential>,
    pub(crate) personal_store_id: Option<StoreId>,
    pub(crate) soft_delete_days: u32,
    pub(crate) retention_days: u32,
    pub(crate) idle_seconds: Option<u64>,
    pub(crate) web_sign_in: bool,
}

/// `parked`, `waking`, `ready` or `locked`.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename = "StoreState")]
pub(crate) enum StoreStateView {
    Parked,
    Waking,
    Ready,
    Locked,
}

/// `GET /v1/stores/{id}/status`.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = StoreStatus)]
#[ts(rename = "StoreStatus")]
pub(crate) struct StoreStatusView {
    pub(crate) state: StoreStateView,
    #[schema(value_type = Option<String>)]
    pub(crate) locked_reason: Option<&'static str>,
    pub(crate) full: bool,
    pub(crate) model: Option<ModelState>,
    pub(crate) embedding: Option<EmbeddingProgress>,
}

impl StoreStatusView {
    pub(crate) fn new(status: &StoreStatus, model: Option<ModelState>) -> Self {
        let (state, locked_reason) = match status.state {
            StoreState::Parked => (StoreStateView::Parked, None),
            StoreState::Waking => (StoreStateView::Waking, None),
            StoreState::Ready => (StoreStateView::Ready, None),
            StoreState::Locked(r) => (StoreStateView::Locked, Some(LockReason::as_str(r))),
        };
        Self {
            state,
            locked_reason,
            full: status.full,
            model,
            embedding: status.embedding,
        }
    }
}

/// `{ state, since }` of a user's container.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContainerView {
    /// `active` or `parked`.
    pub(crate) state: &'static str,
    pub(crate) since: Option<Timestamp>,
}

/// One user of `GET /v1/users`.
#[derive(Serialize, ToSchema, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[schema(as = User)]
#[ts(rename = "User")]
pub(crate) struct UserView {
    pub(crate) id: UserId,
    pub(crate) name: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) is_admin: bool,
    pub(crate) created_at: Timestamp,
    pub(crate) last_active_on: Option<String>,
    pub(crate) app_count: u64,
    pub(crate) container: ContainerView,
    pub(crate) deleted_at: Option<Timestamp>,
    /// `self`, `admin` or `null`.
    pub(crate) deleted_by: Option<&'static str>,
    pub(crate) delete_after: Option<Timestamp>,
}

/// One snapshot of `GET /v1/admin/stores/{id}/snapshots`, which a server serves
/// (milestone 6); its shape is here for the web UI's types.
#[derive(Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "Snapshot")]
#[allow(dead_code, reason = "only the web UI's type export uses it")]
pub(crate) struct SnapshotView {
    /// `<generation>-<sequence>`, what `knoggin admin restore` takes.
    pub(crate) name: String,
    pub(crate) generation: u64,
    pub(crate) sequence: u64,
    pub(crate) size_bytes: u64,
    pub(crate) last_modified: Timestamp,
    /// `newest`, `restore`, `last_hour`, `hourly`, `daily`, or `null`.
    pub(crate) kept_because: Option<String>,
}
