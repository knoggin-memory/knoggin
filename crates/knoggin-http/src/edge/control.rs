// SPDX-License-Identifier: AGPL-3.0-only
//! The control routes that are the same code in both modes: health, the session, stores,
//! API keys, the account and the admin's users. The credential check ran before every one
//! of them and left a [`Caller`].

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use knoggin_core::ops::{Items, Page};
use knoggin_core::time::Timestamp;
use knoggin_core::{Sensitive, StoreId, UserId};
use knoggin_crypto::token::new_api_key;
use knoggin_state::record::{
    DeletedBy, LeaseState, ParkReason, Permission, Slot, StoreKind, StoreRecord, User,
};
use knoggin_state::users::LOCAL_IDENTITY;
use knoggin_state::{NewApiKey, NewStore, StateError, Versioned};
use serde::Serialize;
use uuid::Uuid;

use super::check::{Caller, Credential, SessionSeen, state_error};
use super::shapes::{
    AccountDeleted, ContainerView, CreateKey, CreatedKey, DeleteAccount, Health, KeyView, NameBody,
    Provider, Ready, RotateKey, SessionCredential, SessionUser, SessionView, StoreView, UserView,
    delete_after,
};
use super::{Arrival, EdgeState, Role};
use crate::error::{ApiError, Code};
use crate::store_api::extract::{JsonBody, QueryMap};

type Answer = Result<Response, ApiError>;
type Edge = State<EdgeState>;

/// The routes, over `edge`.
pub(crate) fn routes(edge: EdgeState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/session", get(session))
        .route("/v1/openapi.json", get(openapi))
        .route("/v1/stores", get(list_stores).post(create_store))
        .route(
            "/v1/stores/{id}",
            get(get_store).patch(rename_store).delete(delete_store),
        )
        .route("/v1/stores/{id}/undelete", post(undelete_store))
        .route("/v1/stores/{id}/keys", get(list_keys).post(create_key))
        .route("/v1/stores/{id}/keys/{keyId}/rotate", post(rotate_key))
        .route(
            "/v1/stores/{id}/keys/{keyId}",
            axum::routing::delete(revoke_key),
        )
        .route("/v1/account", axum::routing::delete(delete_account))
        .route("/v1/account/undelete", post(undelete_account))
        .route("/v1/users", get(list_users))
        .route("/v1/users/{id}", axum::routing::delete(remove_user))
        .route("/v1/users/{id}/stores", get(user_stores))
        .route("/v1/users/{id}/undelete", post(restore_user))
        .with_state(edge)
}

fn json<T: Serialize>(status: StatusCode, value: &T) -> Answer {
    let body = serde_json::to_vec(value).map_err(|_| ApiError::internal())?;
    let mut r = (status, body).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(r)
}

fn ok<T: Serialize>(value: &T) -> Answer {
    json(StatusCode::OK, value)
}

const ALREADY_EXISTS: &str = "An app with this name already exists.";

/// A store or key name: 1 to 64 characters after trimming, no control characters.
fn checked_name(field: &str, name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    let count = name.chars().count();
    if count == 0 || count > 64 {
        return Err(ApiError::invalid_argument(
            field,
            format!("{field} must be 1 to 64 characters"),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(ApiError::invalid_argument(
            field,
            format!("{field} may not hold control characters"),
        ));
    }
    Ok(name.to_owned())
}

fn path_id(text: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(text).map_err(|_| ApiError::invalid_argument("id", "the ID must be a UUID"))
}

/// On a local install, keys, store deletion and data deletion belong to the owner of the
/// machine: a cookie session from the public address may not do them, so that an
/// identity later taken off the allow-list leaves no key behind.
fn owner_only(edge: &EdgeState, c: &Caller) -> Result<(), ApiError> {
    if edge.inner().config.role == Role::Local && matches!(c.credential, Credential::Cookie(..)) {
        return Err(ApiError::permission_denied(
            "Keys are managed on the machine that runs Knoggin. Run knoggin open there.",
        ));
    }
    Ok(())
}

fn not_found_store() -> ApiError {
    ApiError::not_found("No such store")
}

impl EdgeState {
    /// A store of the caller, deleted or not; another user's store is `not_found`.
    async fn own_store(&self, user: UserId, id: &str) -> Result<Versioned<StoreRecord>, ApiError> {
        let id = StoreId::from_uuid(path_id(id)?);
        self.inner()
            .stores
            .get_of(user, id)
            .await
            .map_err(state_error)?
            .ok_or_else(not_found_store)
    }

    /// Memory counts of the stores open in this process.
    fn live_counts(&self) -> HashMap<StoreId, u64> {
        self.inner()
            .host
            .as_ref()
            .map(|h| {
                h.report()
                    .stores
                    .into_iter()
                    .map(|s| (s.store_id, s.memory_count))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn store_view(&self, r: &StoreRecord) -> StoreView {
        let live = self.live_counts().get(&r.id).copied();
        StoreView::new(r, self.inner().config.soft_delete_days, live)
    }

    /// A store marked deleted is dropped from memory at once, without a save.
    fn drop_open(&self, store: StoreId) {
        if let Some(host) = &self.inner().host {
            host.drop_store(store);
        }
    }

    fn undo_window_passed(&self, at_ms: i64) -> bool {
        delete_after(at_ms, self.inner().config.soft_delete_days).millis() <= self.now()
    }

    /// Sets the user's deletion mark (keeping an earlier one), writes that time into every
    /// store that has none, ends the sessions, and asks a running container to park.
    /// `keep` is the cookie session that asked for its own account's deletion, which stays
    /// valid so that the owner can undo.
    async fn mark_user_deleted(
        &self,
        user: UserId,
        by: DeletedBy,
        keep: Option<&[u8; 32]>,
    ) -> Result<i64, ApiError> {
        let inner = self.inner();
        let now = self.now();
        let (at, _) = inner
            .users
            .update(user, |u: &mut User| {
                let at = *u.deleted_at_ms.get_or_insert(now);
                if by == DeletedBy::Admin || u.deleted_by.is_none() {
                    u.deleted_by = Some(by);
                }
                Ok::<_, StateError>(at)
            })
            .await
            .map_err(state_error)?;
        // The user's own container parks, and the container of every store this call
        // marked that has one of its own. Nothing else: a shared app of someone else's
        // that the user only used keeps its container.
        let mut park = vec![Slot::User(user)];
        for store in inner.stores.list(user).await.map_err(state_error)? {
            if store.value.deleted_at_ms.is_none() {
                inner
                    .stores
                    .update(user, store.value.id, |s| {
                        s.deleted_at_ms.get_or_insert(at);
                        Ok::<_, StateError>(())
                    })
                    .await
                    .map_err(state_error)?;
                if let slot @ Slot::Store(_) = store.value.slot() {
                    park.push(slot);
                }
            }
            self.drop_open(store.value.id);
        }
        inner
            .sessions
            .delete_all(user, keep)
            .await
            .map_err(state_error)?;
        for slot in park {
            self.request_deleted_park(slot, now).await?;
        }
        Ok(at)
    }

    /// Records a park request `deleted` on a slot's lease that is not parked.
    async fn request_deleted_park(&self, slot: Slot, now: i64) -> Result<(), ApiError> {
        let leases = &self.inner().leases;
        if let Some(lease) = leases.get(slot).await.map_err(state_error)?
            && lease.value.state != LeaseState::Parked
        {
            let mut value = lease.value.clone();
            if value.request_park(ParkReason::Deleted, now) {
                // A conflict means the lease moved on; the reconcile loop parks it then.
                let _ = leases.put(slot, &value, lease.cond()).await;
            }
        }
        Ok(())
    }

    /// Clears the user's mark and brings back the stores deleted with it.
    async fn unmark_user(&self, user: &Versioned<User>) -> Result<Versioned<User>, ApiError> {
        let inner = self.inner();
        let at = user.value.deleted_at_ms.ok_or_else(|| {
            ApiError::new(Code::Conflict, "The account is not scheduled for deletion")
        })?;
        if self.undo_window_passed(at) {
            return Err(ApiError::new(Code::Conflict, "The undo period has passed"));
        }
        let (_, after) = inner
            .users
            .update(user.value.id, |u: &mut User| {
                u.deleted_at_ms = None;
                u.deleted_by = None;
                Ok::<_, StateError>(())
            })
            .await
            .map_err(state_error)?;
        for store in inner
            .stores
            .list(user.value.id)
            .await
            .map_err(state_error)?
        {
            if store.value.deleted_at_ms == Some(at) {
                inner
                    .stores
                    .update(user.value.id, store.value.id, |s| {
                        if s.deleted_at_ms == Some(at) {
                            s.deleted_at_ms = None;
                        }
                        Ok::<_, StateError>(())
                    })
                    .await
                    .map_err(state_error)?;
            }
        }
        Ok(after)
    }

    /// The session object for what the credential check saw.
    pub(crate) fn session_view(&self, seen: &SessionSeen, web_sign_in: bool) -> SessionView {
        let c = &self.inner().config;
        let public = c.public_url.clone().unwrap_or_else(|| c.own_url.clone());
        let mut view = SessionView {
            mode: match c.role {
                Role::Local => "local",
                Role::Server => "server",
            },
            auth: c.auth_mode.as_str(),
            version: c.version.clone(),
            source_url: c.source_url.clone(),
            docs_url: format!("{}/blob/v{}/docs", c.source_url, c.version),
            mcp_url: format!("{public}/mcp"),
            public_url: public,
            signed_in: false,
            signed_out_reason: None,
            provider: self.inner().oauth.as_ref().map(|o| Provider {
                name: o.config.provider_name.clone(),
            }),
            user: None,
            credential: None,
            personal_store_id: None,
            soft_delete_days: c.soft_delete_days,
            retention_days: c.retention_days,
            idle_seconds: c.idle_seconds,
            web_sign_in,
        };
        match seen {
            SessionSeen::Nobody => {}
            SessionSeen::SignedOut(reason) => view.signed_out_reason = Some(reason),
            SessionSeen::Caller(caller) => {
                let u = &caller.user.value;
                view.signed_in = true;
                let (kind, permissions, store_id) = match &caller.credential {
                    Credential::ApiKey(k, _) => (
                        "api_key",
                        k.value
                            .permissions
                            .iter()
                            .map(|p| match p {
                                Permission::Read => "memory:read",
                                Permission::Write => "memory:write",
                            })
                            .collect(),
                        Some(k.value.store_id),
                    ),
                    Credential::Local => ("local", vec!["memory:read", "memory:write"], None),
                    Credential::LocalSession(_) | Credential::Cookie(..) => {
                        ("session", vec!["memory:read", "memory:write"], None)
                    }
                    Credential::Oauth(_) => ("oauth", vec!["memory:read", "memory:write"], None),
                    Credential::None => ("none", vec!["memory:read", "memory:write"], None),
                };
                view.credential = Some(SessionCredential {
                    kind,
                    permissions,
                    store_id,
                });
                view.user = Some(if store_id.is_some() {
                    SessionUser {
                        id: u.id,
                        name: None,
                        email: None,
                        is_admin: None,
                        deleted_at: None,
                        delete_after: None,
                    }
                } else {
                    view.personal_store_id = Some(u.personal_store_id);
                    // The one local user has no name and no email: a cookie session there
                    // reports those of the identity that signed in.
                    let (name, email) = match (&caller.credential, u.issuer == LOCAL_IDENTITY) {
                        (Credential::Cookie(_, s), true) => (&s.value.name, &s.value.email),
                        (_, true) => (&None, &None),
                        (_, false) => (&u.name, &u.email),
                    };
                    SessionUser {
                        id: u.id,
                        name: Some(name.as_ref().map(|n| n.expose().clone())),
                        email: Some(email.as_ref().map(|e| e.expose().clone())),
                        is_admin: Some(caller.is_admin),
                        deleted_at: Some(u.deleted_at_ms.map(Timestamp::from_millis)),
                        delete_after: Some(
                            u.deleted_at_ms.map(|t| delete_after(t, c.soft_delete_days)),
                        ),
                    }
                });
            }
        }
        view
    }
}

#[utoipa::path(get, path = "/healthz", tag = "status",
    responses((status = 200, body = Health)))]
pub(crate) async fn healthz(State(edge): Edge) -> Answer {
    ok(&Health {
        status: "ok",
        version: edge.inner().config.version.clone(),
    })
}

#[utoipa::path(get, path = "/readyz", tag = "status",
    responses((status = 200, body = Ready), (status = 503, body = Ready)))]
pub(crate) async fn readyz(State(edge): Edge) -> Answer {
    if edge
        .inner()
        .ready
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        ok(&Ready { status: "ready" })
    } else {
        json(
            StatusCode::SERVICE_UNAVAILABLE,
            &Ready { status: "starting" },
        )
    }
}

#[utoipa::path(get, path = "/v1/session", tag = "session",
    responses((status = 200, body = SessionView), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn session(
    State(edge): Edge,
    Extension(seen): Extension<SessionSeen>,
    Extension(arrival): Extension<Arrival>,
    headers: HeaderMap,
) -> Answer {
    ok(&edge.session_view(&seen, edge.web_sign_in(&arrival, &headers)))
}

#[utoipa::path(get, path = "/v1/openapi.json", tag = "session",
    responses((status = 200, description = "The OpenAPI 3.1 document of `/v1`")))]
pub(crate) async fn openapi() -> Answer {
    let mut r = super::openapi::document_json().into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(r)
}

#[utoipa::path(get, path = "/v1/stores", tag = "stores",
    params(("includeDeleted" = Option<bool>, Query, description = "Default true")),
    responses((status = 200, body = Items<StoreView>), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn list_stores(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    mut q: QueryMap,
) -> Answer {
    let include_deleted = q.flag("includeDeleted", true)?;
    q.finish()?;
    let mut stores: Vec<StoreRecord> = edge
        .inner()
        .stores
        .list(c.user.value.id)
        .await
        .map_err(state_error)?
        .into_iter()
        .map(|s| s.value)
        .filter(|s| include_deleted || s.deleted_at_ms.is_none())
        .collect();
    stores.sort_by_cached_key(|s| {
        (
            s.kind != StoreKind::Personal,
            s.name.expose().to_lowercase(),
        )
    });
    let live = edge.live_counts();
    let days = edge.inner().config.soft_delete_days;
    let items = stores
        .iter()
        .map(|s| StoreView::new(s, days, live.get(&s.id).copied()))
        .collect();
    ok(&Items { items })
}

#[utoipa::path(post, path = "/v1/stores", tag = "stores", request_body = NameBody,
    responses((status = 201, body = StoreView), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn create_store(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    q: QueryMap,
    JsonBody(body): JsonBody<NameBody>,
) -> Answer {
    q.finish()?;
    let name = checked_name("name", &body.name)?;
    let id = StoreId::from_uuid(Uuid::now_v7());
    let (_, wrapped) = edge
        .inner()
        .keys
        .generate(knoggin_crypto::KeyScope {
            store: id,
            generation: 0,
        })
        .await
        .map_err(|_| {
            tracing::error!(kind = "key", "could not make a data key");
            ApiError::internal()
        })?;
    let store = NewStore {
        id,
        name: Sensitive::new(name),
        wrapped,
    };
    match edge
        .inner()
        .stores
        .create_app(c.user.value.id, store, edge.now())
        .await
    {
        Ok(r) => json(StatusCode::CREATED, &edge.store_view(&r.value)),
        Err(StateError::AlreadyExists) => Err(ApiError::new(Code::AlreadyExists, ALREADY_EXISTS)),
        Err(StateError::LimitReached) => Err(ApiError::new(
            Code::Conflict,
            "A user has at most 1,000 apps",
        )),
        Err(e) => Err(state_error(e)),
    }
}

#[utoipa::path(get, path = "/v1/stores/{id}", tag = "stores",
    responses((status = 200, body = StoreView), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn get_store(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
    q: QueryMap,
) -> Answer {
    q.finish()?;
    let store = edge.own_store(c.user.value.id, &id).await?;
    ok(&edge.store_view(&store.value))
}

#[utoipa::path(patch, path = "/v1/stores/{id}", tag = "stores", request_body = NameBody,
    responses((status = 200, body = StoreView), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn rename_store(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
    q: QueryMap,
    JsonBody(body): JsonBody<NameBody>,
) -> Answer {
    q.finish()?;
    let store = edge.own_store(c.user.value.id, &id).await?;
    let name = checked_name("name", &body.name)?;
    match edge
        .inner()
        .stores
        .rename(c.user.value.id, store.value.id, Sensitive::new(name))
        .await
    {
        Ok(r) => ok(&edge.store_view(&r.value)),
        Err(StateError::NotFound) => Err(not_found_store()),
        Err(StateError::AlreadyExists) => Err(ApiError::new(Code::AlreadyExists, ALREADY_EXISTS)),
        Err(e) => Err(state_error(e)),
    }
}

#[utoipa::path(delete, path = "/v1/stores/{id}", tag = "stores",
    responses((status = 200, body = StoreView), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn delete_store(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
    q: QueryMap,
) -> Answer {
    q.finish()?;
    owner_only(&edge, &c)?;
    let store = edge.own_store(c.user.value.id, &id).await?;
    if store.value.kind == StoreKind::Personal {
        return Err(ApiError::invalid_argument(
            "id",
            "The personal store cannot be deleted",
        ));
    }
    let now = edge.now();
    let (_, after) = edge
        .inner()
        .stores
        .update(c.user.value.id, store.value.id, |s| {
            s.deleted_at_ms.get_or_insert(now);
            Ok::<_, StateError>(())
        })
        .await
        .map_err(state_error)?;
    edge.drop_open(store.value.id);
    // ponytail: no "close one store" call to the container; add one if a deleted normal
    // app's key must leave the shared container sooner than its next save or park.
    if let slot @ Slot::Store(_) = after.value.slot() {
        edge.request_deleted_park(slot, now).await?;
    }
    ok(&edge.store_view(&after.value))
}

#[utoipa::path(post, path = "/v1/stores/{id}/undelete", tag = "stores",
    responses((status = 200, body = StoreView), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn undelete_store(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
    q: QueryMap,
) -> Answer {
    q.finish()?;
    let store = edge.own_store(c.user.value.id, &id).await?;
    let (_, after) = edge
        .inner()
        .stores
        .update(c.user.value.id, store.value.id, |s| match s.deleted_at_ms {
            None => Err(Refused(ApiError::new(
                Code::Conflict,
                "The store is not deleted",
            ))),
            Some(at) if edge.undo_window_passed(at) => Err(Refused(ApiError::new(
                Code::Conflict,
                "The undo period has passed",
            ))),
            Some(_) => {
                s.deleted_at_ms = None;
                Ok(())
            }
        })
        .await
        .map_err(|r| r.0)?;
    ok(&edge.store_view(&after.value))
}

/// A refusal decided inside a record update, which then writes nothing.
struct Refused(ApiError);

impl From<StateError> for Refused {
    fn from(e: StateError) -> Self {
        Self(state_error(e))
    }
}

#[utoipa::path(get, path = "/v1/stores/{id}/keys", tag = "keys",
    responses((status = 200, body = Items<KeyView>), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn list_keys(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
    q: QueryMap,
) -> Answer {
    q.finish()?;
    let store = edge.own_store(c.user.value.id, &id).await?;
    let mut keys: Vec<_> = edge
        .inner()
        .api_keys
        .list(store.value.id)
        .await
        .map_err(state_error)?
        .into_iter()
        .map(|k| k.value.key)
        .collect();
    keys.sort_by_key(|k| std::cmp::Reverse((k.created_ms, k.id)));
    ok(&Items {
        items: keys.iter().map(KeyView::new).collect(),
    })
}

fn lifetime_days(days: Option<u32>) -> Result<i64, ApiError> {
    match days.unwrap_or(90) {
        d @ (30 | 90 | 180 | 365) => Ok(i64::from(d)),
        _ => Err(ApiError::invalid_argument(
            "expiresInDays",
            "expiresInDays must be 30, 90, 180 or 365",
        )),
    }
}

fn new_key(now: i64, days: i64) -> Result<(String, NewApiKey), ApiError> {
    let (secret, hash, prefix) = new_api_key().map_err(|_| {
        tracing::error!(kind = "random", "could not make an API key");
        ApiError::internal()
    })?;
    let key = NewApiKey {
        id: Uuid::now_v7(),
        hash,
        prefix,
        key_expires_ms: now.saturating_add(days * knoggin_core::time::MILLIS_PER_DAY),
    };
    Ok((secret, key))
}

fn key_error(e: StateError) -> ApiError {
    match e {
        StateError::StoreDeleted => ApiError::store_deleting(),
        StateError::LimitReached => {
            ApiError::new(Code::Conflict, "A store has at most 1,000 key records")
        }
        StateError::NotFound => ApiError::not_found("No such store or key"),
        other => state_error(other),
    }
}

#[utoipa::path(post, path = "/v1/stores/{id}/keys", tag = "keys", request_body = CreateKey,
    responses((status = 201, body = CreatedKey), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn create_key(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
    q: QueryMap,
    JsonBody(body): JsonBody<CreateKey>,
) -> Answer {
    q.finish()?;
    owner_only(&edge, &c)?;
    let store = edge.own_store(c.user.value.id, &id).await?;
    let name = checked_name("name", &body.name)?;
    let permissions = match body.permissions.as_deref() {
        None => vec![Permission::Read, Permission::Write],
        Some([r]) if r == "memory:read" => vec![Permission::Read],
        Some([r, w]) if r == "memory:read" && w == "memory:write" => {
            vec![Permission::Read, Permission::Write]
        }
        Some([w, r]) if r == "memory:read" && w == "memory:write" => {
            vec![Permission::Read, Permission::Write]
        }
        Some(_) => {
            return Err(ApiError::invalid_argument(
                "permissions",
                "permissions must be [\"memory:read\"] or [\"memory:read\", \"memory:write\"]",
            ));
        }
    };
    let days = lifetime_days(body.expires_in_days)?;
    if store.value.deleted_at_ms.is_some() {
        return Err(ApiError::store_deleting());
    }
    let now = edge.now();
    let (secret, new) = new_key(now, days)?;
    let key = edge
        .inner()
        .api_keys
        .create(
            c.user.value.id,
            store.value.id,
            new,
            Sensitive::new(name),
            permissions,
            now,
        )
        .await
        .map_err(key_error)?;
    json(
        StatusCode::CREATED,
        &CreatedKey {
            key: KeyView::new(&key),
            secret,
        },
    )
}

#[utoipa::path(post, path = "/v1/stores/{id}/keys/{keyId}/rotate", tag = "keys", request_body(content = Option<RotateKey>),
    responses((status = 201, body = CreatedKey), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn rotate_key(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    Path((id, key_id)): Path<(String, String)>,
    q: QueryMap,
    JsonBody(body): JsonBody<RotateKey>,
) -> Answer {
    q.finish()?;
    owner_only(&edge, &c)?;
    let store = edge.own_store(c.user.value.id, &id).await?;
    let key_id = path_id(&key_id)?;
    let days = lifetime_days(body.expires_in_days)?;
    let old = edge
        .inner()
        .api_keys
        .list(store.value.id)
        .await
        .map_err(state_error)?
        .into_iter()
        .find(|k| k.value.key.id == key_id)
        .ok_or_else(|| ApiError::not_found("No such key"))?;
    if old.value.key.revoked_ms.is_some() {
        return Err(ApiError::new(Code::Conflict, "The key is already revoked"));
    }
    if store.value.deleted_at_ms.is_some() {
        return Err(ApiError::store_deleting());
    }
    let now = edge.now();
    let (secret, new) = new_key(now, days)?;
    let key = match edge
        .inner()
        .api_keys
        .rotate(c.user.value.id, store.value.id, key_id, new, now)
        .await
    {
        // Revoked in between.
        Err(StateError::NotFound) => {
            return Err(ApiError::new(Code::Conflict, "The key is already revoked"));
        }
        other => other.map_err(key_error)?,
    };
    json(
        StatusCode::CREATED,
        &CreatedKey {
            key: KeyView::new(&key),
            secret,
        },
    )
}

#[utoipa::path(delete, path = "/v1/stores/{id}/keys/{keyId}", tag = "keys",
    responses((status = 204, description = "Revoked"), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn revoke_key(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    Path((id, key_id)): Path<(String, String)>,
    q: QueryMap,
) -> Answer {
    q.finish()?;
    owner_only(&edge, &c)?;
    let store = edge.own_store(c.user.value.id, &id).await?;
    let key_id = path_id(&key_id)?;
    edge.inner()
        .api_keys
        .revoke(store.value.id, key_id, edge.now())
        .await
        .map_err(key_error)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[utoipa::path(delete, path = "/v1/account", tag = "account", request_body = DeleteAccount,
    responses((status = 200, body = AccountDeleted), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn delete_account(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    q: QueryMap,
    JsonBody(body): JsonBody<DeleteAccount>,
) -> Answer {
    q.finish()?;
    if body.confirm != "delete my data" {
        return Err(ApiError::invalid_argument(
            "confirm",
            "confirm must be exactly \"delete my data\"",
        ));
    }
    owner_only(&edge, &c)?;
    let cookie = match &c.credential {
        Credential::Cookie(h, _) => Some(h),
        _ => None,
    };
    let at = edge
        .mark_user_deleted(c.user.value.id, DeletedBy::Owner, cookie)
        .await?;
    if let Some(local) = &edge.inner().local {
        let keep = match &c.credential {
            Credential::LocalSession(h) => Some(*h),
            Credential::Local
            | Credential::ApiKey(..)
            | Credential::Oauth(_)
            | Credential::Cookie(..)
            | Credential::None => None,
        };
        local.forget_all_but(keep);
    }
    ok(&AccountDeleted {
        deleted_at: Timestamp::from_millis(at),
        delete_after: delete_after(at, edge.inner().config.soft_delete_days),
    })
}

#[utoipa::path(post, path = "/v1/account/undelete", tag = "account",
    responses((status = 200, body = SessionView), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn undelete_account(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    Extension(arrival): Extension<Arrival>,
    headers: HeaderMap,
    q: QueryMap,
) -> Answer {
    q.finish()?;
    let user = edge.unmark_user(&c.user).await?;
    let caller = Caller { user, ..c };
    let web = edge.web_sign_in(&arrival, &headers);
    ok(&edge.session_view(&SessionSeen::Caller(Box::new(caller)), web))
}

impl EdgeState {
    async fn user_view(&self, user: &User) -> Result<UserView, ApiError> {
        let inner = self.inner();
        let app_count = inner
            .stores
            .list(user.id)
            .await
            .map_err(state_error)?
            .iter()
            .filter(|s| s.value.kind == StoreKind::App && s.value.deleted_at_ms.is_none())
            .count() as u64;
        let container = match inner
            .leases
            .get(Slot::User(user.id))
            .await
            .map_err(state_error)?
        {
            Some(l) if l.value.state != LeaseState::Parked => ContainerView {
                state: "active",
                since: Some(Timestamp::from_millis(l.value.state_since_ms)),
            },
            Some(l) => ContainerView {
                state: "parked",
                since: Some(Timestamp::from_millis(l.value.state_since_ms)),
            },
            None => ContainerView {
                state: "parked",
                since: None,
            },
        };
        Ok(UserView {
            id: user.id,
            name: user.name.as_ref().map(|n| n.expose().clone()),
            email: user.email.as_ref().map(|e| e.expose().clone()),
            is_admin: self.is_admin(user),
            created_at: Timestamp::from_millis(user.created_ms),
            last_active_on: user.last_active_day.clone(),
            app_count,
            container,
            deleted_at: user.deleted_at_ms.map(Timestamp::from_millis),
            deleted_by: user.deleted_by.map(|b| match b {
                DeletedBy::Owner => "self",
                DeletedBy::Admin => "admin",
            }),
            delete_after: user
                .deleted_at_ms
                .map(|t| delete_after(t, inner.config.soft_delete_days)),
        })
    }

    async fn other_user(&self, id: &str) -> Result<Versioned<User>, ApiError> {
        let id = UserId::from_uuid(path_id(id)?);
        self.inner()
            .users
            .get(id)
            .await
            .map_err(state_error)?
            .ok_or_else(|| ApiError::not_found("No such user"))
    }
}

#[utoipa::path(get, path = "/v1/users", tag = "admin",
    params(("limit" = Option<u32>, Query), ("cursor" = Option<String>, Query)),
    responses((status = 200, body = Page<UserView>), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn list_users(State(edge): Edge, mut q: QueryMap) -> Answer {
    let limit = q.number("limit")?.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::invalid_argument(
            "limit",
            "limit must be 1 to 200",
        ));
    }
    // The cursor is the last user's ID in its simple form, which is base64url text.
    let after = q
        .text("cursor")?
        .map(|c| {
            Uuid::try_parse(&c)
                .map(UserId::from_uuid)
                .map_err(|_| ApiError::invalid_argument("cursor", "the cursor is not valid"))
        })
        .transpose()?;
    q.finish()?;
    let page = edge
        .inner()
        .users
        .list(limit as usize, after)
        .await
        .map_err(state_error)?;
    let next_cursor = (page.len() == limit as usize)
        .then(|| page.last().map(|u| u.value.id.uuid().simple().to_string()))
        .flatten();
    let mut items = Vec::with_capacity(page.len());
    for u in &page {
        items.push(edge.user_view(&u.value).await?);
    }
    ok(&Page { items, next_cursor })
}

#[utoipa::path(get, path = "/v1/users/{id}/stores", tag = "admin",
    responses((status = 200, body = Items<StoreView>), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn user_stores(State(edge): Edge, Path(id): Path<String>, q: QueryMap) -> Answer {
    q.finish()?;
    let user = edge.other_user(&id).await?;
    let mut stores: Vec<StoreRecord> = edge
        .inner()
        .stores
        .list(user.value.id)
        .await
        .map_err(state_error)?
        .into_iter()
        .map(|s| s.value)
        .collect();
    stores.sort_by_cached_key(|s| {
        (
            s.kind != StoreKind::Personal,
            s.name.expose().to_lowercase(),
        )
    });
    let items = stores.iter().map(|s| edge.store_view(s)).collect();
    ok(&Items { items })
}

#[utoipa::path(delete, path = "/v1/users/{id}", tag = "admin",
    responses((status = 200, body = UserView), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn remove_user(
    State(edge): Edge,
    Extension(c): Extension<Caller>,
    Path(id): Path<String>,
    q: QueryMap,
) -> Answer {
    q.finish()?;
    let user = edge.other_user(&id).await?;
    if user.value.id == c.user.value.id {
        return Err(ApiError::invalid_argument(
            "id",
            "You cannot remove yourself",
        ));
    }
    edge.mark_user_deleted(user.value.id, DeletedBy::Admin, None)
        .await?;
    let user = edge.other_user(&id).await?;
    ok(&edge.user_view(&user.value).await?)
}

#[utoipa::path(post, path = "/v1/users/{id}/undelete", tag = "admin",
    responses((status = 200, body = UserView), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn restore_user(State(edge): Edge, Path(id): Path<String>, q: QueryMap) -> Answer {
    q.finish()?;
    let user = edge.other_user(&id).await?;
    let after = edge.unmark_user(&user).await?;
    ok(&edge.user_view(&after.value).await?)
}
