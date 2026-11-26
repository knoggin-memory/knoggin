// SPDX-License-Identifier: AGPL-3.0-only
//! `RouterBackend`: the container's `StoreBackend`. It opens and saves stores through the
//! router's broker and moves snapshot bytes with the signed links the broker hands out:
//! over the pinned link to the router's blob route on one host, straight to the bucket
//! on AWS. Once the broker answers `lease_lost`, it calls the broker never again.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use http::{HeaderName, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Limited};
use knoggin_core::snapshot::SnapshotName;
use knoggin_core::{StoreId, UserId};
use knoggin_crypto::{DataKey, WrappedKey};
use knoggin_http::error::LockReason;
use knoggin_http::host::{BackendError, OpenedStore, SealedSnapshot, SnapshotSource, StoreBackend};
use tokio::sync::watch;
use tokio_util::io::{StreamReader, SyncIoBridge};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::broker::api::{
    CommitRequest, ErrorEnvelope, NameParts, OpenAnswer, OpenRequest, SnapshotsRequest,
    UPLOAD_BUDGET, UploadLink, paths,
};
use crate::link::{LaunchSecret, MAX_ANSWER_BYTES, RouterClient, write_container_call};

/// The time limit of one broker call.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// A transfer of snapshot bytes ends when nothing moves for this long.
const TRANSFER_IDLE: Duration = Duration::from_secs(60);
/// Tries of a broker call that is answered 429 without a reason (too many calls).
const BUSY_TRIES: u32 = 5;

/// The container's side of the link to the router: its client with the routers'
/// certificate pinned, its identity, and the mark that the lease is lost.
pub(crate) struct Link {
    pub(crate) client: RouterClient<Body>,
    pub(crate) bucket: reqwest::Client,
    /// `KNOGGIN_ROUTER_URL` without a trailing slash.
    pub(crate) router_url: String,
    pub(crate) launch: LaunchSecret,
    pub(crate) container_id: Uuid,
    /// Becomes true on the first `lease_lost`; from then on nothing is sent.
    pub(crate) lost: watch::Sender<bool>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("container_id", &self.container_id)
            .finish_non_exhaustive()
    }
}

/// How the broker answered a call it refused.
fn refusal(link: &Link, status: StatusCode, reason: Option<&str>) -> BackendError {
    match status.as_u16() {
        409 => {
            link.lost.send_replace(true);
            BackendError::LeaseLost
        }
        404 | 410 => BackendError::Deleted,
        423 => BackendError::Locked(
            reason
                .and_then(|r| serde_json::from_value(serde_json::Value::from(r)).ok())
                .unwrap_or(LockReason::SnapshotUnreadable),
        ),
        429 if reason == Some(UPLOAD_BUDGET) => BackendError::OverBudget,
        _ => BackendError::Unavailable,
    }
}

impl Link {
    fn is_lost(&self) -> bool {
        *self.lost.borrow()
    }

    /// A request to the router with the container's identity.
    fn request(
        &self,
        user: UserId,
        method: Method,
        url: &str,
        body: Body,
    ) -> Option<Request<Body>> {
        let mut req = Request::builder().method(method).uri(url).body(body).ok()?;
        write_container_call(req.headers_mut(), &self.launch, user, self.container_id);
        Some(req)
    }

    /// One broker call with a JSON body; the answer's body on success, read into a
    /// buffer that wipes itself. A 429 without a reason is tried again after a pause.
    pub(crate) async fn call(
        &self,
        user: UserId,
        path: &str,
        body: Vec<u8>,
    ) -> Result<Zeroizing<Vec<u8>>, BackendError> {
        let mut pause = Duration::from_millis(250);
        for _ in 0..BUSY_TRIES {
            if self.is_lost() {
                return Err(BackendError::LeaseLost);
            }
            let url = format!("{}{path}", self.router_url);
            let mut req = self
                .request(user, Method::POST, &url, Body::from(body.clone()))
                .ok_or(BackendError::Internal)?;
            req.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            let read = async {
                let answer = self.client.request(req).await.ok()?;
                let status = answer.status();
                let bytes = Limited::new(answer.into_body(), MAX_ANSWER_BYTES)
                    .collect()
                    .await
                    .ok()?
                    .to_bytes();
                Some((status, Zeroizing::new(bytes.to_vec())))
            };
            let Ok(Some((status, bytes))) = tokio::time::timeout(CALL_TIMEOUT, read).await else {
                tracing::warn!(kind = "broker", "a broker call failed");
                return Err(BackendError::Unavailable);
            };
            if status.is_success() {
                return Ok(bytes);
            }
            let reason = serde_json::from_slice::<ErrorEnvelope>(&bytes)
                .ok()
                .and_then(|e| e.error.reason);
            if status == StatusCode::TOO_MANY_REQUESTS && reason.is_none() {
                tokio::time::sleep(pause).await;
                pause *= 2;
                continue;
            }
            return Err(refusal(self, status, reason.as_deref()));
        }
        Err(BackendError::Unavailable)
    }

    /// `parked`: tried 3 times within about 10 seconds. Returns whether it went through.
    pub(crate) async fn parked(&self, user: UserId) -> bool {
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            if self.is_lost() {
                return false;
            }
            let url = format!("{}{}", self.router_url, paths::PARKED);
            let Some(req) = self.request(user, Method::POST, &url, Body::empty()) else {
                return false;
            };
            let sent = tokio::time::timeout(Duration::from_secs(3), self.client.request(req));
            if let Ok(Ok(answer)) = sent.await
                && answer.status().is_success()
            {
                return true;
            }
        }
        false
    }

    /// Whether a link points at the router (one host): then it goes over the pinned link
    /// with the container's identity, else straight to the bucket.
    fn via_router(&self, url: &str) -> bool {
        url.strip_prefix(&self.router_url)
            .is_some_and(|rest| rest.starts_with(paths::BLOB_PREFIX))
    }

    /// Sends a signed request exactly as given, with `body`. Returns the status and the
    /// answer's bytes as a stream.
    async fn send_signed(
        &self,
        user: UserId,
        method: Method,
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        body: Option<BodyStream>,
    ) -> Result<(StatusCode, BodyStream), BackendError> {
        if self.is_lost() {
            return Err(BackendError::LeaseLost);
        }
        let unavailable = || {
            // The address is a secret: it is never logged.
            tracing::warn!(kind = "transfer", "a snapshot transfer failed");
            BackendError::Unavailable
        };
        if self.via_router(url) {
            let body = body.map_or_else(Body::empty, Body::from_stream);
            let mut req = self
                .request(user, method, url, body)
                .ok_or(BackendError::Internal)?;
            for (name, value) in headers {
                let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| unavailable())?;
                let value = HeaderValue::from_str(value).map_err(|_| unavailable())?;
                req.headers_mut().insert(name, value);
            }
            let answer = tokio::time::timeout(TRANSFER_IDLE, self.client.request(req))
                .await
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())?;
            let status = answer.status();
            let stream = answer
                .into_body()
                .into_data_stream()
                .map_err(io::Error::other);
            return Ok((status, Box::pin(stream)));
        }
        let mut req = self.bucket.request(method, url);
        for (name, value) in headers {
            req = req.header(name.as_str(), value.as_str());
        }
        if let Some(body) = body {
            req = req.body(reqwest::Body::wrap_stream(body));
        }
        let answer = tokio::time::timeout(TRANSFER_IDLE, req.send())
            .await
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())?;
        let status = StatusCode::from_u16(answer.status().as_u16()).map_err(|_| unavailable())?;
        Ok((
            status,
            Box::pin(answer.bytes_stream().map_err(io::Error::other)),
        ))
    }
}

/// A stream of bytes that ends with an error when nothing arrives for a minute.
type BodyStream = std::pin::Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

fn idle_limited(stream: BodyStream) -> BodyStream {
    Box::pin(futures_util::stream::unfold(
        Some(stream),
        |state| async move {
            let mut stream = state?;
            match tokio::time::timeout(TRANSFER_IDLE, stream.next()).await {
                Ok(Some(Ok(bytes))) => Some((Ok(bytes), Some(stream))),
                Ok(Some(Err(e))) => Some((Err(e), None)),
                Ok(None) => None,
                Err(_) => Some((Err(io::Error::from(io::ErrorKind::TimedOut)), None)),
            }
        },
    ))
}

/// A commit whose upload went through but whose commit failed: it is tried again, for
/// the same name and without a new upload, before the store's next save.
struct Pending {
    name: SnapshotName,
    body: Vec<u8>,
}

/// The container's `StoreBackend`: every open and save goes through the broker.
pub struct RouterBackend {
    link: Arc<Link>,
    user: UserId,
    pending: Mutex<HashMap<StoreId, Pending>>,
}

impl std::fmt::Debug for RouterBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterBackend")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl RouterBackend {
    pub(crate) fn new(link: Arc<Link>, user: UserId) -> Self {
        Self {
            link,
            user,
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<StoreId, Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn commit(
        &self,
        store: StoreId,
        name: SnapshotName,
        body: Vec<u8>,
    ) -> Result<(), BackendError> {
        let path = paths::commit(store, name.sequence());
        match self.link.call(self.user, &path, body).await {
            Ok(_) => {
                self.pending().remove(&store);
                Ok(())
            }
            Err(e) => {
                if matches!(e, BackendError::Deleted | BackendError::LeaseLost) {
                    self.pending().remove(&store);
                }
                Err(e)
            }
        }
    }
}

fn to_json(value: &impl serde::Serialize) -> Result<Vec<u8>, BackendError> {
    serde_json::to_vec(value).map_err(|_| BackendError::Internal)
}

#[async_trait]
impl StoreBackend for RouterBackend {
    async fn open(
        &self,
        store: StoreId,
        after: Option<SnapshotName>,
    ) -> Result<OpenedStore, BackendError> {
        let request = OpenRequest {
            after: after.map(|n| NameParts {
                generation: n.generation(),
                sequence: n.sequence(),
            }),
        };
        let bytes = self
            .link
            .call(self.user, &paths::open(store), to_json(&request)?)
            .await?;
        let answer: OpenAnswer =
            serde_json::from_slice(&bytes).map_err(|_| BackendError::Unavailable)?;
        let key =
            DataKey::from_base64url(&answer.data_key.0).map_err(|_| BackendError::Unavailable)?;
        let wrapped = WrappedKey {
            kek_id: answer.kek_id.clone(),
            blob: base64::Engine::decode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                &answer.wrapped_key,
            )
            .map_err(|_| BackendError::Unavailable)?,
        };
        let snapshot = match &answer.snapshot {
            None => None,
            Some(link) => {
                let name = SnapshotName::new(link.generation, link.sequence)
                    .map_err(|_| BackendError::Unavailable)?;
                let (status, stream) = self
                    .link
                    .send_signed(self.user, Method::GET, &link.url, &link.headers, None)
                    .await?;
                if !status.is_success() {
                    tracing::warn!(store_id = %store, status = status.as_u16(), "a snapshot download was refused");
                    return Err(match status.as_u16() {
                        409 | 404 | 410 if self.link.via_router(&link.url) => {
                            refusal(&self.link, status, None)
                        }
                        _ => BackendError::Unavailable,
                    });
                }
                // The host reads it on a blocking thread; the bridge gets this runtime.
                let reader = SyncIoBridge::new_with_handle(
                    StreamReader::new(idle_limited(stream)),
                    tokio::runtime::Handle::current(),
                );
                Some(SnapshotSource {
                    name,
                    reader: Box::new(reader),
                })
            }
        };
        Ok(OpenedStore {
            key,
            wrapped,
            generation: answer.generation,
            next_sequence: answer.next_sequence,
            restore: answer.restore,
            snapshot,
        })
    }

    async fn save(&self, store: StoreId, snapshot: SealedSnapshot) -> Result<(), BackendError> {
        let leftover = self.pending().get(&store).map(|p| (p.name, p.body.clone()));
        if let Some((pending, body)) = leftover {
            self.commit(store, pending, body).await?;
            if pending == snapshot.name {
                // That name holds the older upload, now committed; the newer changes take
                // the next sequence.
                return Err(BackendError::NameTaken);
            }
        }
        let SealedSnapshot {
            name,
            chunks,
            len,
            memory_count,
            namespace_count,
            last_write_at,
            last_write_client,
            restored,
        } = snapshot;
        let request = SnapshotsRequest {
            sequence: name.sequence(),
            size_bytes: len,
        };
        let bytes = self
            .link
            .call(self.user, &paths::snapshots(store), to_json(&request)?)
            .await?;
        let link: UploadLink =
            serde_json::from_slice(&bytes).map_err(|_| BackendError::Unavailable)?;
        let method =
            Method::from_bytes(link.method.as_bytes()).map_err(|_| BackendError::Unavailable)?;
        // Sealing happens as the chunks are taken, on a blocking thread; the body takes
        // them from a small channel, so at most two chunks wait in memory.
        let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(2);
        // Once the body is dropped the channel closes and the task ends by itself.
        drop(tokio::task::spawn_blocking(move || {
            for chunk in chunks {
                let item = chunk
                    .map(Bytes::from)
                    .map_err(|_| io::Error::other("sealing failed"));
                let failed = item.is_err();
                if tx.blocking_send(item).is_err() || failed {
                    return;
                }
            }
        }));
        let body: BodyStream = Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }));
        let sent = self
            .link
            .send_signed(
                self.user,
                method,
                &link.url,
                &link.headers,
                Some(idle_limited(body)),
            )
            .await;
        let (status, answer) = sent?;
        // Read the rest of the answer so the connection can be used again.
        let _ = tokio::time::timeout(TRANSFER_IDLE, answer.take(64).count()).await;
        match status.as_u16() {
            200..=299 => {}
            412 => return Err(BackendError::NameTaken),
            409 | 404 | 410 if self.link.via_router(&link.url) => {
                return Err(refusal(&self.link, status, None));
            }
            code => {
                tracing::warn!(store_id = %store, status = code, "a snapshot upload was refused");
                return Err(BackendError::Unavailable);
            }
        }
        let commit = CommitRequest {
            memory_count,
            namespace_count,
            last_write_at: last_write_at.and_then(|t| t.to_rfc3339()),
            last_write_client,
            size_bytes: len,
            restored: restored.map(|n| n.to_string()),
        };
        let body = to_json(&commit)?;
        self.pending().insert(
            store,
            Pending {
                name,
                body: body.clone(),
            },
        );
        self.commit(store, name, body).await
    }
}
