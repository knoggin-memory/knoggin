// SPDX-License-Identifier: AGPL-3.0-only
//! The router's public API end to end, with stub containers: the edge layer over the
//! forwarder, the browser checks on the router, the rate limits per address (with trusted
//! proxies) and per user, the three mode routes, the start-up guards, and the internal
//! listener's 404 for public routes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use http::{Method, StatusCode, header};
use http_body_util::BodyExt;
use knoggin_core::{Sensitive, StoreId, Timestamp, UserId};
use knoggin_crypto::KeyProvider;
use knoggin_crypto::testing::test_provider;
use knoggin_http::edge::listen::Peer;
use knoggin_http::edge::{Arrival, AuthMode, EdgeConfig, EdgeParts, EdgeState, Rate, Role, guard};
use knoggin_server::launcher::{BuildInfo, LogLevel};
use knoggin_server::lease::{Fleet, FleetConfig};
use knoggin_server::link::api::{ClaimLimits, ClaimSettings};
use knoggin_server::link::{InternalSecret, ServerIdentity};
use knoggin_server::router::forward::Timing;
use knoggin_server::router::status::{StatusInfo, mode_routes};
use knoggin_server::router::{guards, public_router};
use knoggin_server::settings::{ContainerSettings, RouterSettings};
use knoggin_server::storage::{MemoryStorage, names};
use knoggin_server::testing::{FakeLauncher, StubControl, stub_runner};
use knoggin_state::record::Permission;
use knoggin_state::testing::{TestClock, test_field_keys};
use knoggin_state::{ApiKeys, LocalFileDb, NewApiKey, NewStore, NewUser, StateDb, Stores, Users};
use tower::ServiceExt;
use uuid::Uuid;

const HOST: &str = "127.0.0.1:8080";

struct App {
    router: Router,
    fleet: Arc<Fleet>,
    launcher: Arc<FakeLauncher>,
    storage: Arc<MemoryStorage>,
    db: Arc<dyn StateDb>,
    user: UserId,
    store: StoreId,
    keys: ApiKeys,
    clock: TestClock,
}

async fn app(per_address: Rate, per_user: Rate) -> App {
    let clock = TestClock::new();
    clock.advance(1_800_000_000_000);
    let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(clock.clock()));
    let fields = Arc::new(test_field_keys());
    let provider: Arc<dyn KeyProvider> = Arc::new(test_provider());
    let users = Users::new(db.clone(), fields.clone());
    let user = knoggin_http::local::ensure_local_user(&users, &*provider, clock.now())
        .await
        .unwrap();
    let store = users
        .get(user)
        .await
        .unwrap()
        .unwrap()
        .value
        .personal_store_id;
    let mut config = EdgeConfig::local("http://127.0.0.1:1".into(), "0.1.0".into());
    config.role = Role::Server;
    config.auth_mode = AuthMode::None;
    config.public_url = Some(format!("http://{HOST}"));
    config.test_mode = true;
    config.idle_seconds = Some(600);
    config.per_address = Some(per_address);
    config.per_user = Some(per_user);
    let edge = EdgeState::new(
        config,
        EdgeParts {
            db: db.clone(),
            field_keys: fields.clone(),
            keys: provider,
            clock: clock.clock(),
            host: None,
            local: None,
        },
    )
    .await
    .unwrap();
    let control = Arc::new(StubControl::default());
    let launcher = FakeLauncher::new(stub_runner("0.1.0", true, control), clock.clock());
    let storage = Arc::new(MemoryStorage::start(clock.clock()).await.unwrap());
    let fleet = Fleet::new(
        FleetConfig {
            router_id: Uuid::now_v7(),
            build: BuildInfo::new("0.1.0"),
            spec: String::new(),
            one_host: true,
            test_mode: true,
            containers: ContainerSettings {
                pool_target: 0,
                claim_wait_ms: 8_000,
                ..ContainerSettings::default()
            },
            claim: ClaimSettings {
                idle_seconds: 600,
                memory_limit_bytes: 1 << 30,
                public_url: format!("http://{HOST}"),
                source_url: "https://example.invalid".into(),
                limits: ClaimLimits {
                    body_bytes: 10 << 20,
                    request_timeout_ms: 30_000,
                    store_concurrency: 64,
                },
            },
            secret: InternalSecret::from_bytes([9u8; 32]),
            router_url: "https://127.0.0.1:1".into(),
            router_pin: ServerIdentity::for_container(Uuid::now_v7()).unwrap().pin(),
            log_level: LogLevel::Info,
            allow_downgrade: false,
        },
        db.clone(),
        fields.clone(),
        launcher.clone(),
        storage.clone(),
        clock.clock(),
    )
    .unwrap();
    let modes = mode_routes(
        fleet.clone(),
        Stores::new(db.clone(), fields.clone()),
        users,
        StatusInfo {
            image_digest: None,
            state_db: "local_file",
            key_provider: "key_file",
            launcher: "docker",
            bucket: "knoggin-test".into(),
            region: None,
            retention_days: 7,
        },
    );
    let router = public_router(
        edge,
        fleet.clone(),
        modes,
        Duration::from_secs(20),
        vec!["192.0.2.10/32".parse().unwrap()],
        Timing::default(),
    );
    App {
        router,
        fleet,
        launcher,
        storage,
        keys: ApiKeys::new(db.clone(), fields),
        db,
        user,
        store,
        clock,
    }
}

fn wide() -> Rate {
    Rate {
        per_second: 1000.0,
        burst: 1000.0,
    }
}

fn req(method: Method, path: &str) -> http::request::Builder {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST)
}

async fn call(app: &App, peer: &str, request: Request) -> Response {
    let mut request = request;
    request.extensions_mut().insert(Arrival::Public);
    request
        .extensions_mut()
        .insert(Peer(peer.parse::<SocketAddr>().unwrap()));
    app.router.clone().oneshot(request).await.unwrap()
}

async fn json(r: Response) -> serde_json::Value {
    let b = r.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null)
}

const CLIENT: &str = "198.51.100.7:4000";

#[tokio::test]
async fn a_store_request_reaches_the_users_container_through_the_edge() {
    let a = app(wide(), wide()).await;
    let r = call(
        &a,
        CLIENT,
        req(Method::GET, "/v1/tags").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers().contains_key("knoggin-request-id"));
    assert_eq!(r.headers().get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    let v = json(r).await;
    assert_eq!(v["path"], "/v1/tags");
}

#[tokio::test]
async fn the_routers_browser_checks_refuse_internal_headers_and_foreign_origins() {
    let a = app(wide(), wide()).await;
    for name in [
        "knoggin-permissions",
        "Knoggin-User-Id",
        "KNOGGIN-STORE-ID",
        "knoggin-credential-kind",
    ] {
        let r = call(
            &a,
            CLIENT,
            req(Method::GET, "/v1/tags")
                .header(name, "x")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(json(r).await["error"]["code"], "invalid_argument", "{name}");
    }
    let twice = req(Method::GET, "/v1/tags")
        .header("knoggin-user-id", "a")
        .header("knoggin-user-id", "b")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        json(call(&a, CLIENT, twice).await).await["error"]["code"],
        "invalid_argument"
    );
    let foreign = req(Method::GET, "/v1/tags")
        .header(header::ORIGIN, "https://evil.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        json(call(&a, CLIENT, foreign).await).await["error"]["code"],
        "permission_denied"
    );
    let site = req(Method::GET, "/v1/tags")
        .header("sec-fetch-site", "cross-site")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        json(call(&a, CLIENT, site).await).await["error"]["code"],
        "permission_denied"
    );
    let host = Request::get("/v1/tags")
        .header(header::HOST, "evil.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        json(call(&a, CLIENT, host).await).await["error"]["code"],
        "permission_denied"
    );
    let health = Request::get("/healthz")
        .header(header::HOST, "10.0.0.9")
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&a, CLIENT, health).await.status(), StatusCode::OK);
    let options = req(Method::OPTIONS, "/v1/tags")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        call(&a, CLIENT, options).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        a.launcher.handles().len(),
        0,
        "no refused request woke a container"
    );
}

#[tokio::test]
async fn the_forwarding_header_counts_only_from_a_trusted_proxy() {
    let a = app(
        Rate {
            per_second: 0.001,
            burst: 1.0,
        },
        wide(),
    )
    .await;
    let health = |xff: &str| {
        Request::get("/v1/session")
            .header(header::HOST, HOST)
            .header("x-forwarded-for", xff)
            .body(Body::empty())
            .unwrap()
    };
    // From an untrusted peer the header is ignored: both share the peer's bucket.
    assert_eq!(
        call(&a, CLIENT, health("203.0.113.1")).await.status(),
        StatusCode::OK
    );
    let r = call(&a, CLIENT, health("203.0.113.2")).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    // From the trusted proxy each forwarded client has its own bucket.
    let proxy = "192.0.2.10:5000";
    assert_eq!(
        call(&a, proxy, health("203.0.113.1")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        call(&a, proxy, health("203.0.113.2")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        call(&a, proxy, health("203.0.113.2")).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn a_user_with_many_keys_stays_inside_the_per_user_limit() {
    let a = app(
        wide(),
        Rate {
            per_second: 0.001,
            burst: 3.0,
        },
    )
    .await;
    let mut secrets = Vec::new();
    for i in 0..4 {
        let (secret, hash, prefix) = knoggin_crypto::token::new_api_key().unwrap();
        a.keys
            .create(
                a.user,
                a.store,
                NewApiKey {
                    id: Uuid::now_v7(),
                    hash,
                    prefix,
                    key_expires_ms: a.clock.now() + 86_400_000,
                },
                Sensitive::new(format!("k{i}")),
                vec![Permission::Read],
                a.clock.now(),
            )
            .await
            .unwrap();
        secrets.push(secret);
    }
    let mut codes = Vec::new();
    for s in &secrets {
        let r = call(
            &a,
            CLIENT,
            req(Method::GET, "/v1/tags")
                .header(header::AUTHORIZATION, format!("Bearer {s}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        codes.push(r.status());
    }
    assert_eq!(
        codes,
        vec![
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );
}

#[tokio::test]
async fn the_store_status_does_not_wake_a_container_and_the_admin_status_counts() {
    let a = app(wide(), wide()).await;
    let r = call(
        &a,
        CLIENT,
        req(Method::GET, &format!("/v1/stores/{}/status", a.store))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(json(r).await["state"], "parked");
    assert_eq!(a.launcher.handles().len(), 0);
    // Warm: the container's status says the store is not open, so it is parked.
    a.fleet
        .acquire(a.user, Duration::from_secs(8))
        .await
        .unwrap();
    let r = call(
        &a,
        CLIENT,
        req(Method::GET, &format!("/v1/stores/{}/status", a.store))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let v = json(r).await;
    assert_eq!(v["state"], "parked");
    assert_eq!(v["model"]["state"], "ready");
    let r = call(
        &a,
        CLIENT,
        req(Method::GET, "/v1/admin/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let v = json(r).await;
    assert_eq!(v["mode"], "server");
    assert_eq!(v["containers"]["claimed"], 1);
    assert_eq!(v["rollout"], serde_json::Value::Null, "one host");
    assert_eq!(v["users"]["total"], 1);
}

#[tokio::test]
async fn the_snapshot_list_labels_with_thin() {
    let a = app(wide(), wide()).await;
    let now = a.clock.now();
    for (g, s, ago) in [(5u64, 1u64, 7_200_000i64), (5, 2, 60_000), (5, 3, 1_000)] {
        let name = knoggin_core::snapshot::SnapshotName::new(g, s).unwrap();
        a.storage.insert(
            &names::snapshot_key(a.store, name),
            vec![0u8; 10],
            Timestamp::from_millis(now - ago),
        );
    }
    let r = call(
        &a,
        CLIENT,
        req(
            Method::GET,
            &format!("/v1/admin/stores/{}/snapshots", a.store),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    let v = json(r).await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["name"], "0000000005-0000000003");
    assert_eq!(items[0]["keptBecause"], "newest");
    assert_eq!(items[0]["sizeBytes"], 10);
    let r = call(
        &a,
        CLIENT,
        req(
            Method::GET,
            &format!("/v1/admin/stores/{}/snapshots", Uuid::now_v7()),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_public_route_sent_to_the_internal_listener_gets_404() {
    let a = app(wide(), wide()).await;
    let settings = settings(&["192.0.2.10/32"], "10.201.0.0/16");
    let broker = knoggin_server::router::broker(
        &a.fleet,
        Arc::new(test_provider()),
        &settings,
        Some("https://127.0.0.1:1".into()),
    )
    .unwrap();
    for path in ["/v1/tags", "/v1/%2e%2e/internal/v1/park", "/healthz"] {
        let r = broker
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn an_internal_path_sent_to_the_public_listener_is_never_forwarded() {
    let a = app(wide(), wide()).await;
    for path in [
        "/internal/v1/park",
        "/v1/%2e%2e/internal/v1/park",
        "/v1/../internal/v1/status",
        "/internal/v1/stores/x/open",
    ] {
        let r = call(
            &a,
            CLIENT,
            req(Method::POST, path).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(a.launcher.handles().len(), 0);
}

fn settings(trusted: &[&str], one_host_net: &str) -> RouterSettings {
    let mut v: serde_json::Value = serde_json::json!({
        "trusted_proxies": trusted,
        "storage": { "bucket": "b" },
        "containers": { "user_net_cidr": one_host_net },
        "internal": {
            "secret": format!("{}=", "A".repeat(43)),
            "cert": "x",
            "key": "y"
        }
    });
    v["containers"]["user_net_cidr"] = serde_json::json!(one_host_net);
    serde_json::from_value(v).unwrap()
}

#[test]
fn the_router_refuses_to_start_on_one_host_without_proxies_or_with_an_overlap() {
    let mut edge = EdgeConfig::local("http://127.0.0.1:1".into(), "0.1.0".into());
    edge.role = Role::Server;
    edge.public_url = Some("https://memory.example".into());
    assert!(guards(&edge, &settings(&[], "10.201.0.0/16"), true).is_err());
    assert!(guards(&edge, &settings(&["10.201.3.4/32"], "10.201.0.0/16"), true).is_err());
    assert!(guards(&edge, &settings(&["10.200.0.2/32"], "10.201.0.0/16"), true).is_ok());
    assert!(guards(&edge, &settings(&[], "10.201.0.0/16"), false).is_ok());
    // The sign-in guards run too: an http address outside test mode.
    edge.public_url = Some("http://memory.example".into());
    assert!(guards(&edge, &settings(&["10.200.0.2/32"], "10.201.0.0/16"), true).is_err());
}

#[tokio::test]
async fn in_mode_none_the_router_refuses_a_state_database_with_another_user() {
    let a = app(wide(), wide()).await;
    let fields = Arc::new(test_field_keys());
    let users = Users::new(a.db.clone(), fields);
    let mut edge = EdgeConfig::local("http://127.0.0.1:1".into(), "0.1.0".into());
    edge.role = Role::Server;
    edge.auth_mode = AuthMode::None;
    assert!(guard::router_users(&edge, &users).await.is_ok());
    let (_, wrapped) = test_provider()
        .generate(knoggin_crypto::KeyScope {
            store: StoreId::from_uuid(Uuid::now_v7()),
            generation: 0,
        })
        .await
        .unwrap();
    users
        .create(
            NewUser {
                id: UserId::from_uuid(Uuid::now_v7()),
                issuer: "https://idp.example".into(),
                subject: Sensitive::new("someone".into()),
                email: None,
                email_verified: false,
                name: None,
            },
            NewStore {
                id: StoreId::from_uuid(Uuid::now_v7()),
                name: Sensitive::new("Personal".into()),
                wrapped,
            },
            a.clock.now(),
        )
        .await
        .unwrap();
    assert!(guard::router_users(&edge, &users).await.is_err());
}
