// SPDX-License-Identifier: AGPL-3.0-only
//! Shared helpers of the store-layer tests: a host in a temporary folder, requests with
//! an inserted context, and a backend that counts and fails saves.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use knoggin_core::StoreId;
use knoggin_core::snapshot::SnapshotName;
use knoggin_http::RequestContext;
use knoggin_http::error::Code;
use knoggin_http::host::{
    BackendError, EmbedWaits, HostConfig, LocalBackend, OpenedStore, SealedSnapshot, StoreBackend,
};
use knoggin_http::store_layer;
use knoggin_http::testing::{TestHost, fake_config};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

/// Short waits for tests that cross the embedding threads.
pub const WAITS: EmbedWaits = EmbedWaits {
    query: Duration::from_secs(2),
    duplicate_check: Duration::from_secs(2),
};

/// A host with its folder and its router.
pub struct Env {
    pub t: TestHost,
    pub app: Router,
    pub counting: Arc<Counting>,
    _dir: TempDir,
}

impl Env {
    /// A local host with the fake model ready (or not).
    pub async fn new(ready: bool) -> Self {
        Self::with_config(fake_config(), ready).await
    }

    /// A host with `config`.
    pub async fn with_config(config: HostConfig, ready: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut counting = None;
        let t = TestHost::with_backend(dir.path(), config, ready, WAITS, |inner| {
            let c = Arc::new(Counting::new(inner));
            counting = Some(c.clone());
            c
        })
        .await;
        let app = store_layer(t.host.clone());
        Self {
            t,
            app,
            counting: counting.unwrap(),
            _dir: dir,
        }
    }

    /// A request with a context, an optional JSON body and extra headers.
    pub async fn call(
        &self,
        ctx: Option<&RequestContext>,
        method: Method,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Resp {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        let mut request = builder.body(body).unwrap();
        if let Some(ctx) = ctx {
            request.extensions_mut().insert(ctx.clone());
        }
        self.send(request).await
    }

    /// Sends a prepared request.
    pub async fn send(&self, request: Request<Body>) -> Resp {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Resp {
            status,
            headers,
            json,
            bytes: bytes.to_vec(),
        }
    }
}

/// One answer.
#[derive(Debug)]
pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub json: Value,
    pub bytes: Vec<u8>,
}

impl Resp {
    /// Asserts an error answer of the shape, with a code from the list, and returns it.
    pub fn error(&self) -> Code {
        let e = &self.json["error"];
        let code: Code = serde_json::from_value(e["code"].clone()).unwrap_or_else(|_| {
            panic!(
                "not an error body: {}",
                String::from_utf8_lossy(&self.bytes)
            )
        });
        assert!(e["message"].is_string(), "{}", self.json);
        assert!(e["requestId"].is_string(), "{}", self.json);
        let expected = if self.status == StatusCode::METHOD_NOT_ALLOWED {
            StatusCode::METHOD_NOT_ALLOWED
        } else {
            code.status()
        };
        assert_eq!(self.status, expected, "{}", self.json);
        assert_eq!(self.headers["cache-control"], "no-store");
        code
    }
}

/// A backend over `LocalBackend` that counts saves and can fail the next few.
#[derive(Debug)]
pub struct Counting {
    pub inner: Arc<LocalBackend>,
    pub saves: Mutex<Vec<(tokio::time::Instant, StoreId, SnapshotName)>>,
    pub fail_next: AtomicU32,
    pub failures: AtomicU32,
}

impl Counting {
    fn new(inner: Arc<LocalBackend>) -> Self {
        Self {
            inner,
            saves: Mutex::new(Vec::new()),
            fail_next: AtomicU32::new(0),
            failures: AtomicU32::new(0),
        }
    }

    /// How many saves went through.
    pub fn count(&self) -> usize {
        self.saves.lock().unwrap().len()
    }

    /// The times of the saves that went through.
    pub fn times(&self) -> Vec<tokio::time::Instant> {
        self.saves.lock().unwrap().iter().map(|s| s.0).collect()
    }
}

#[async_trait]
impl StoreBackend for Counting {
    async fn open(
        &self,
        store: StoreId,
        after: Option<SnapshotName>,
    ) -> Result<OpenedStore, BackendError> {
        self.inner.open(store, after).await
    }

    async fn save(&self, store: StoreId, snapshot: SealedSnapshot) -> Result<(), BackendError> {
        if self
            .fail_next
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            self.failures.fetch_add(1, Ordering::SeqCst);
            return Err(BackendError::Unavailable);
        }
        let name = snapshot.name;
        self.inner.save(store, snapshot).await?;
        self.saves
            .lock()
            .unwrap()
            .push((tokio::time::Instant::now(), store, name));
        Ok(())
    }
}

/// A minimal valid save body.
pub fn memory(summary: &str) -> Value {
    serde_json::json!({ "summary": summary })
}
