// SPDX-License-Identifier: AGPL-3.0-only
//! The edge layer over a real store layer, a local state database on a clock the test
//! moves, the fake model, and requests tagged with the listener they arrive on.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Method, Request, StatusCode};
use knoggin_core::embed::EmbeddingProvider;
use knoggin_core::ops::ModelStatus;
use knoggin_core::testing::FakeProvider;
use knoggin_core::{Sensitive, StoreId, UserId};
use knoggin_crypto::KeyProvider;
use knoggin_http::edge::{Arrival, EdgeConfig, EdgeParts, EdgeState, Rate, edge_layer};
use knoggin_http::host::{Embedder, HostConfig, LocalBackend, Providers, StoreHost};
use knoggin_http::local::{LocalSessions, LocalState, ensure_local_user, mode_routes};
use knoggin_http::store_layer;
use knoggin_http::testing::fake_model_state;
use knoggin_state::testing::{TestClock, test_field_keys};
use knoggin_state::{FieldKeys, LocalFileDb, NewStore, NewUser, StateDb, Stores, Users};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{OnceCell, watch};
use tower::ServiceExt;
use uuid::Uuid;

use super::{Resp, WAITS};

/// The fixed listener's port in these tests.
pub const PORT: u16 = 7690;
/// The local web listener's authority in these tests.
pub const WEB: &str = "127.0.0.1:50123";

/// The fixed listener.
pub fn fixed() -> Arrival {
    Arrival::Fixed { port: PORT }
}

/// The local web listener.
pub fn web() -> Arrival {
    Arrival::Web {
        authority: WEB.into(),
    }
}

/// A daemon's edge in one process.
pub struct EdgeEnv {
    pub clock: TestClock,
    pub db: Arc<dyn StateDb>,
    pub fields: Arc<FieldKeys>,
    pub keys: Arc<dyn KeyProvider>,
    pub users: Users,
    pub stores: Stores,
    pub host: Arc<StoreHost>,
    pub backend: Arc<LocalBackend>,
    pub local: Arc<LocalSessions>,
    pub edge: EdgeState,
    pub app: Router,
    pub user: UserId,
    pub personal: StoreId,
    pub dir: TempDir,
    _model: watch::Sender<knoggin_core::ops::ModelState>,
}

impl EdgeEnv {
    /// The daemon's defaults.
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    /// With the settings changed by `f`.
    pub async fn with(f: impl FnOnce(&mut EdgeConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = TestClock::new();
        let keys: Arc<dyn KeyProvider> = Arc::new(knoggin_crypto::testing::test_provider());
        let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(clock.clock()));
        let fields = Arc::new(test_field_keys());
        let users = Users::new(db.clone(), fields.clone());
        let stores = Stores::new(db.clone(), fields.clone());
        let user = ensure_local_user(&users, &*keys, clock.now())
            .await
            .unwrap();
        let personal = users
            .get(user)
            .await
            .unwrap()
            .unwrap()
            .value
            .personal_store_id;
        let backend = Arc::new(LocalBackend::new(
            &dir.path().join("data"),
            keys.clone(),
            stores.clone(),
            7,
        ));
        let (model, rx) = watch::channel(fake_model_state(ModelStatus::Ready));
        let providers = Arc::new(OnceCell::new());
        let _ = providers.set(Providers {
            query: Arc::new(FakeProvider::new()),
            document: Arc::new(FakeProvider::new()),
        });
        let config = HostConfig::local(FakeProvider::new().identity().clone());
        let host = StoreHost::new(config, backend.clone(), Embedder::new(rx, providers, WAITS));
        let local = Arc::new(LocalSessions::new(format!("http://{WEB}")));
        let mut config = EdgeConfig::local(format!("http://127.0.0.1:{PORT}"), "0.1.0".into());
        f(&mut config);
        let parts = EdgeParts {
            db: db.clone(),
            field_keys: fields.clone(),
            keys: keys.clone(),
            clock: clock.clock(),
            host: Some(host.clone()),
            local: Some(local.clone()),
        };
        let edge = EdgeState::new(config, parts).await.unwrap();
        let modes = mode_routes(LocalState {
            host: host.clone(),
            stores: stores.clone(),
            sessions: local.clone(),
            data_dir: dir.path().join("data"),
            key_provider: "key_file",
            version: "0.1.0".into(),
            clock: clock.clock(),
        });
        let app = edge_layer(edge.clone(), store_layer(host.clone()), modes);
        Self {
            clock,
            db,
            fields,
            keys,
            users,
            stores,
            host,
            backend,
            local,
            edge,
            app,
            user,
            personal,
            dir,
            _model: model,
        }
    }

    /// A request on `arrival`, with the `Host` of that listener unless `headers` names one.
    pub async fn call(
        &self,
        arrival: Arrival,
        method: Method,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Resp {
        let mut builder = Request::builder().method(method).uri(uri);
        let host = match &arrival {
            Arrival::Web { authority } => authority.clone(),
            _ => format!("127.0.0.1:{PORT}"),
        };
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            builder = builder.header("host", host);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        let mut request = builder.body(body).unwrap();
        request.extensions_mut().insert(arrival);
        self.send(request).await
    }

    /// Sends a prepared request.
    pub async fn send(&self, request: Request<Body>) -> Resp {
        send(&self.app, request).await
    }

    /// A request on the socket.
    pub async fn local(&self, method: Method, uri: &str, body: Option<Value>) -> Resp {
        self.call(Arrival::Socket, method, uri, body, &[]).await
    }

    /// A request with a bearer value on `arrival`.
    pub async fn bearer(
        &self,
        arrival: Arrival,
        token: &str,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> Resp {
        let auth = format!("Bearer {token}");
        self.call(arrival, method, uri, body, &[("authorization", &auth)])
            .await
    }

    /// Makes an API key over the socket; returns the secret and the key's JSON.
    pub async fn key(&self, store: StoreId, permissions: Value, days: u32) -> (String, Value) {
        let r = self
            .local(
                Method::POST,
                &format!("/v1/stores/{store}/keys"),
                Some(
                    json!({ "name": "laptop", "permissions": permissions, "expiresInDays": days }),
                ),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
        (
            r.json["secret"].as_str().unwrap().to_owned(),
            r.json["key"].clone(),
        )
    }

    /// Makes an app over the socket.
    pub async fn app(&self, name: &str) -> StoreId {
        let r = self
            .local(Method::POST, "/v1/stores", Some(json!({ "name": name })))
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
        r.json["id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .map(StoreId::from_uuid)
            .unwrap()
    }

    /// A second user, made directly in the database, with its personal store.
    pub async fn other_user(&self, subject: &str) -> (UserId, StoreId) {
        let store = StoreId::from_uuid(Uuid::now_v7());
        let (_, wrapped) = self
            .keys
            .generate(knoggin_crypto::KeyScope {
                store,
                generation: 0,
            })
            .await
            .unwrap();
        let (u, _) = self
            .users
            .create(
                NewUser {
                    id: UserId::from_uuid(Uuid::now_v7()),
                    issuer: "https://issuer.test".into(),
                    subject: Sensitive::new(subject.into()),
                    email: None,
                    email_verified: false,
                    name: None,
                },
                NewStore {
                    id: store,
                    name: Sensitive::new("Personal".into()),
                    wrapped,
                },
                self.clock.now(),
            )
            .await
            .unwrap();
        (u.value.id, store)
    }
}

/// A tight bucket per credential, for the rate-limit tests.
pub fn tight(config: &mut EdgeConfig) {
    config.per_credential = Rate {
        per_second: 0.001,
        burst: 2.0,
    };
    config.unauthenticated = Rate {
        per_second: 0.001,
        burst: 3.0,
    };
}

/// Sends `request` through `app`.
pub async fn send(app: &Router, request: Request<Body>) -> Resp {
    use http_body_util::BodyExt;
    let response = app.clone().oneshot(request).await.unwrap();
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
