// SPDX-License-Identifier: AGPL-3.0-only
//! The launcher service's API against a fake Docker Engine (bollard over HTTP to a stub on
//! loopback), and `DockerLauncher` against the service in process. The fake holds other projects'
//! containers and networks beside the launcher's own, and the tests check that none of them is ever
//! changed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::{Method, StatusCode, Uri};
use knoggin_core::Timestamp;
use knoggin_server::launcher::{
    ContainerEnv, ContainerSpec, DockerLauncher, EndReason, LaunchError, LaunchHandle, Launcher,
    LogLevel, RunState,
};
use knoggin_server::launcher_service::{LauncherService, ServiceConfig};
use knoggin_server::link::{CertPin, InternalSecret};
use knoggin_server::settings::SecretText;
use serde_json::{Value, json};
use uuid::Uuid;

const PROJECT: &str = "kn-test";
const IMAGE: &str = "sha256:feedface";
const TOKEN: &str = "launcher-token-marker";
const MEMORY_MIB: u64 = 512;

#[derive(Debug, Clone)]
struct Ctr {
    id: String,
    name: String,
    labels: BTreeMap<String, String>,
    image: String,
    body: Value,
    networks: BTreeMap<String, String>,
    status: &'static str,
    exit_code: i64,
    started_at: String,
    finished_at: String,
}

#[derive(Debug, Clone)]
struct Net {
    id: String,
    name: String,
    labels: BTreeMap<String, String>,
    subnet: String,
    body: Value,
    members: BTreeMap<String, (String, Vec<String>)>,
}

#[derive(Debug, Default)]
struct Fake {
    containers: BTreeMap<String, Ctr>,
    networks: BTreeMap<String, Net>,
    /// Every changing call: method and path.
    changes: Vec<(String, String)>,
    next: u32,
    /// Every container start fails.
    fail_start: bool,
}

type Shared = Arc<Mutex<Fake>>;

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

impl Fake {
    fn add(&mut self, id: &str, labels: BTreeMap<String, String>, status: &'static str, code: i64) {
        self.containers.insert(
            id.to_owned(),
            Ctr {
                id: id.to_owned(),
                name: id.to_owned(),
                labels,
                image: "sha256:other".into(),
                body: json!({}),
                networks: BTreeMap::new(),
                status,
                exit_code: code,
                started_at: "2026-11-26T10:00:00.123456789Z".into(),
                finished_at: "2026-11-26T10:05:00Z".into(),
            },
        );
    }

    fn add_net(&mut self, name: &str, labels: BTreeMap<String, String>, subnet: &str) {
        self.networks.insert(
            name.to_owned(),
            Net {
                id: format!("net-{name}"),
                name: name.to_owned(),
                labels,
                subnet: subnet.to_owned(),
                body: json!({}),
                members: BTreeMap::new(),
            },
        );
    }

    fn find(&self, key: &str) -> Option<String> {
        self.containers
            .values()
            .find(|c| c.id == key || c.name == key || c.id.starts_with(key))
            .map(|c| c.id.clone())
    }

    fn find_net(&self, key: &str) -> Option<String> {
        self.networks
            .values()
            .find(|n| n.id == key || n.name == key)
            .map(|n| n.name.clone())
    }

    fn join(&mut self, net: &str, ctr: &str, aliases: Vec<String>) {
        let n = self.networks.get_mut(net).unwrap();
        let base: Ipv4Addr = n.subnet.split('/').next().unwrap().parse().unwrap();
        let ip = Ipv4Addr::from(u32::from(base) + 2 + n.members.len() as u32).to_string();
        n.members.insert(ctr.to_owned(), (ip.clone(), aliases));
        self.containers
            .get_mut(ctr)
            .unwrap()
            .networks
            .insert(net.to_owned(), ip);
    }

    fn inspect(&self, c: &Ctr) -> Value {
        let networks: serde_json::Map<String, Value> = c
            .networks
            .iter()
            .map(|(n, ip)| (n.clone(), json!({ "IPAddress": ip })))
            .collect();
        json!({
            "Id": c.id,
            "Name": format!("/{}", c.name),
            "Image": c.image,
            "Config": { "Labels": c.labels },
            "State": {
                "Status": c.status,
                "Running": c.status == "running",
                "OOMKilled": false,
                "ExitCode": c.exit_code,
                "StartedAt": c.started_at,
                "FinishedAt": c.finished_at,
            },
            "NetworkSettings": { "Networks": networks },
        })
    }
}

/// The label filters of a list call.
fn wanted(query: &HashMap<String, String>) -> Vec<(String, String)> {
    query
        .get("filters")
        .and_then(|f| serde_json::from_str::<HashMap<String, Vec<String>>>(f).ok())
        .and_then(|f| f.get("label").cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
        .collect()
}

fn matches(labels: &BTreeMap<String, String>, wanted: &[(String, String)]) -> bool {
    wanted.iter().all(|(k, v)| labels.get(k) == Some(v))
}

fn status(code: StatusCode, message: &str) -> Response {
    (code, axum::Json(json!({ "message": message }))).into_response()
}

async fn docker(
    State(fake): State<Shared>,
    method: Method,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let path = uri.path();
    // bollard writes `/v1.53/...`.
    let path = path
        .strip_prefix('/')
        .and_then(|p| p.split_once('/'))
        .filter(|(v, _)| v.starts_with('v'))
        .map_or(path, |(_, rest)| rest);
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut f = fake.lock().unwrap();
    if method != Method::GET {
        f.changes.push((method.to_string(), path.to_owned()));
    }
    let now = "2026-11-26T12:00:00Z".to_owned();
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["containers", "json"]) => {
            let all = query.get("all").is_some_and(|a| a == "true" || a == "1");
            let want = wanted(&query);
            let list: Vec<Value> = f
                .containers
                .values()
                .filter(|c| all || c.status == "running")
                .filter(|c| matches(&c.labels, &want))
                .map(|c| json!({ "Id": c.id, "Names": [format!("/{}", c.name)], "Labels": c.labels, "State": c.status }))
                .collect();
            axum::Json(list).into_response()
        }
        ("GET", ["containers", id, "json"]) => match f.find(id) {
            Some(id) => axum::Json(f.inspect(&f.containers[&id])).into_response(),
            None => status(StatusCode::NOT_FOUND, "no such container"),
        },
        ("POST", ["containers", "create"]) => {
            f.next += 1;
            let id = format!("{:064x}", f.next);
            let name = query.get("name").cloned().unwrap_or_else(|| id.clone());
            let labels: BTreeMap<String, String> =
                serde_json::from_value(body["Labels"].clone()).unwrap_or_default();
            f.containers.insert(
                id.clone(),
                Ctr {
                    id: id.clone(),
                    name,
                    labels,
                    image: body["Image"].as_str().unwrap_or_default().to_owned(),
                    body: body.clone(),
                    networks: BTreeMap::new(),
                    status: "created",
                    exit_code: 0,
                    started_at: "0001-01-01T00:00:00Z".into(),
                    finished_at: "0001-01-01T00:00:00Z".into(),
                },
            );
            (
                StatusCode::CREATED,
                axum::Json(json!({ "Id": id, "Warnings": [] })),
            )
                .into_response()
        }
        ("POST", ["containers", id, "start"]) => {
            let Some(id) = f.find(id) else {
                return status(StatusCode::NOT_FOUND, "no such container");
            };
            if f.fail_start {
                return status(StatusCode::INTERNAL_SERVER_ERROR, "cannot start");
            }
            let net = f.containers[&id].body["HostConfig"]["NetworkMode"]
                .as_str()
                .map(str::to_owned);
            if let Some(net) = net.and_then(|n| f.find_net(&n)) {
                f.join(&net, &id, Vec::new());
            }
            let c = f.containers.get_mut(&id).unwrap();
            c.status = "running";
            c.started_at = now;
            StatusCode::NO_CONTENT.into_response()
        }
        ("POST", ["containers", id, "stop"]) => {
            let Some(id) = f.find(id) else {
                return status(StatusCode::NOT_FOUND, "no such container");
            };
            let c = f.containers.get_mut(&id).unwrap();
            // A container role that gets a stop signal parks and exits 0.
            c.status = "exited";
            c.exit_code = 0;
            c.finished_at = now;
            StatusCode::NO_CONTENT.into_response()
        }
        ("DELETE", ["containers", id]) => {
            let Some(id) = f.find(id) else {
                return status(StatusCode::NOT_FOUND, "no such container");
            };
            f.containers.remove(&id);
            for n in f.networks.values_mut() {
                n.members.remove(&id);
            }
            StatusCode::NO_CONTENT.into_response()
        }
        ("GET", ["networks"]) => {
            let want = wanted(&query);
            let list: Vec<Value> = f
                .networks
                .values()
                .filter(|n| matches(&n.labels, &want))
                .map(|n| json!({ "Name": n.name, "Id": n.id, "Labels": n.labels, "IPAM": { "Config": [{ "Subnet": n.subnet }] } }))
                .collect();
            axum::Json(list).into_response()
        }
        ("POST", ["networks", "create"]) => {
            let name = body["Name"].as_str().unwrap_or_default().to_owned();
            let subnet = body["IPAM"]["Config"][0]["Subnet"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if f.networks.contains_key(&name) || f.networks.values().any(|n| n.subnet == subnet) {
                return status(StatusCode::FORBIDDEN, "Pool overlaps");
            }
            let labels: BTreeMap<String, String> =
                serde_json::from_value(body["Labels"].clone()).unwrap_or_default();
            f.add_net(&name, labels, &subnet);
            f.networks.get_mut(&name).unwrap().body = body;
            (
                StatusCode::CREATED,
                axum::Json(json!({ "Id": format!("net-{name}"), "Warning": "" })),
            )
                .into_response()
        }
        ("POST", ["networks", net, "connect"]) => {
            let (Some(net), Some(ctr)) = (
                f.find_net(net),
                body["Container"].as_str().and_then(|c| f.find(c)),
            ) else {
                return status(StatusCode::NOT_FOUND, "no such network or container");
            };
            let aliases = serde_json::from_value(body["EndpointConfig"]["Aliases"].clone())
                .unwrap_or_default();
            f.join(&net, &ctr, aliases);
            StatusCode::OK.into_response()
        }
        ("POST", ["networks", net, "disconnect"]) => {
            let (Some(net), Some(ctr)) = (
                f.find_net(net),
                body["Container"].as_str().and_then(|c| f.find(c)),
            ) else {
                return status(StatusCode::NOT_FOUND, "no such network or container");
            };
            f.networks.get_mut(&net).unwrap().members.remove(&ctr);
            f.containers.get_mut(&ctr).unwrap().networks.remove(&net);
            StatusCode::OK.into_response()
        }
        ("DELETE", ["networks", net]) => {
            let Some(net) = f.find_net(net) else {
                return status(StatusCode::NOT_FOUND, "no such network");
            };
            // As Docker: only a running container's endpoint keeps a network.
            if f.networks[&net]
                .members
                .keys()
                .any(|c| f.containers.get(c).is_some_and(|c| c.status == "running"))
            {
                return status(StatusCode::FORBIDDEN, "network has active endpoints");
            }
            // A stopped container keeps naming it, as on Docker.
            f.networks.remove(&net);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => status(StatusCode::NOT_FOUND, "not in the fake"),
    }
}

struct Rig {
    fake: Shared,
    service: Arc<LauncherService>,
    url: String,
    http: reqwest::Client,
}

async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    addr
}

async fn rig() -> Rig {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mut f = Fake::default();
    f.add(
        "launcher-host",
        labels(&[
            ("com.docker.compose.project", PROJECT),
            ("com.docker.compose.service", "launcher"),
        ]),
        "running",
        0,
    );
    f.containers.get_mut("launcher-host").unwrap().image = IMAGE.into();
    f.add(
        "router-ctr",
        labels(&[
            ("com.docker.compose.project", PROJECT),
            ("com.docker.compose.service", "router"),
        ]),
        "running",
        0,
    );
    // Another stack's user container and network, and a container that is no one's.
    f.add(
        "foreign-user",
        labels(&[
            ("knoggin.role", "user"),
            ("knoggin.deployment", "other"),
            ("knoggin.container-id", &Uuid::now_v7().to_string()),
        ]),
        "exited",
        0,
    );
    f.add_net(
        "kn-other-x",
        labels(&[("knoggin.role", "user"), ("knoggin.deployment", "other")]),
        "10.201.0.0/29",
    );
    f.add("owners-own", BTreeMap::new(), "running", 0);
    let fake: Shared = Arc::new(Mutex::new(f));
    let docker_addr = serve(Router::new().fallback(docker).with_state(fake.clone())).await;
    let docker = bollard::Docker::connect_with_http(
        &format!("http://{docker_addr}"),
        10,
        bollard::API_DEFAULT_VERSION,
    )
    .unwrap();
    let service = LauncherService::start(
        docker,
        "launcher-host\n",
        ServiceConfig {
            token: SecretText::new(TOKEN),
            user_net: "10.201.0.0/16".parse().unwrap(),
            memory_mib: MEMORY_MIB,
            test_mode: true,
        },
    )
    .await
    .unwrap();
    let addr = serve(service.router()).await;
    Rig {
        fake,
        service,
        url: format!("http://{addr}"),
        http: reqwest::Client::new(),
    }
}

impl Rig {
    async fn call(
        &self,
        method: Method,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let answer = req.send().await.unwrap();
        let code = answer.status().as_u16();
        let text = answer.text().await.unwrap();
        (code, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn foreign_untouched(&self) {
        let f = self.fake.lock().unwrap();
        for (method, path) in &f.changes {
            for foreign in ["foreign-user", "kn-other-x", "owners-own", "launcher-host"] {
                assert!(!path.contains(foreign), "{method} {path} touched {foreign}");
            }
        }
        assert!(f.containers.contains_key("foreign-user"));
        assert!(f.networks.contains_key("kn-other-x"));
    }
}

fn create_body(id: Uuid) -> Value {
    json!({
        "id": id.to_string(),
        "launchSecret": "ab".repeat(32),
        "routerCertSha256": "cd".repeat(32),
        "logLevel": "debug",
    })
}

#[tokio::test]
async fn the_launcher_service_creates_lists_stops_attaches_and_tidies_only_its_own() {
    let r = rig().await;
    let id = Uuid::now_v7();

    // The token.
    let (code, body) = r.call(Method::GET, "/v1/containers", "wrong", None).await;
    assert_eq!(code, 401);
    assert_eq!(body["error"]["code"], "unauthenticated");
    let (code, _) = r.call(Method::POST, "/v1/attach", "", None).await;
    assert_eq!(code, 401);

    // Every field is checked before it is used.
    for (field, value) in [
        ("id", json!("not-a-uuid")),
        ("launchSecret", json!("ab".repeat(31))),
        ("routerCertSha256", json!("zz".repeat(32))),
        ("logLevel", json!("loud")),
    ] {
        let mut body = create_body(id);
        body[field] = value;
        let (code, answer) = r
            .call(Method::POST, "/v1/containers", TOKEN, Some(body))
            .await;
        assert_eq!(code, 400, "{field}");
        assert_eq!(answer["error"]["code"], "invalid_argument");
    }
    let mut extra = create_body(id);
    extra["image"] = json!("evil:latest");
    let (code, _) = r
        .call(Method::POST, "/v1/containers", TOKEN, Some(extra))
        .await;
    assert_eq!(code, 400, "no other field reaches the description");
    assert!(r.fake.lock().unwrap().changes.is_empty());

    // Create.
    let (code, created) = r
        .call(Method::POST, "/v1/containers", TOKEN, Some(create_body(id)))
        .await;
    assert_eq!(code, 200, "{created}");
    let handle = created["handle"].as_str().unwrap().to_owned();
    let address: std::net::SocketAddrV4 = created["address"].as_str().unwrap().parse().unwrap();
    assert_eq!(address.port(), 8443);
    let net_name = format!("kn-{PROJECT}-{id}");
    {
        let f = r.fake.lock().unwrap();
        let net = &f.networks[&net_name];
        // The lowest free /29: the other stack holds 10.201.0.0/29.
        assert_eq!(net.subnet, "10.201.0.8/29");
        assert!(
            address.ip().octets()[..3] == [10, 201, 0]
                && (8..16).contains(&address.ip().octets()[3])
        );
        assert_eq!(net.body["Internal"], true);
        assert_eq!(
            net.body["Options"]["com.docker.network.bridge.gateway_mode_ipv4"],
            "isolated"
        );
        assert_eq!(
            net.labels,
            labels(&[("knoggin.role", "user"), ("knoggin.deployment", PROJECT)])
        );
        assert_eq!(net.members["router-ctr"].1, ["router"]);
        let c = &f.containers[&handle];
        let b = &c.body;
        assert_eq!(b["Image"], IMAGE, "the launcher's own image");
        assert_eq!(b["Cmd"], json!(["serve"]));
        assert_eq!(b["User"], "65532:65532");
        assert_eq!(b["StopTimeout"], 120);
        assert_eq!(
            c.labels,
            labels(&[
                ("knoggin.role", "user"),
                ("knoggin.deployment", PROJECT),
                ("knoggin.container-id", &id.to_string()),
            ])
        );
        let mut env: Vec<String> = serde_json::from_value(b["Env"].clone()).unwrap();
        env.sort();
        assert_eq!(
            env,
            [
                format!("KNOGGIN_CONTAINER_ID={id}"),
                format!("KNOGGIN_LAUNCH_SECRET={}", "ab".repeat(32)),
                "KNOGGIN_LOG__LEVEL=debug".to_owned(),
                format!("KNOGGIN_ROUTER_CERT_SHA256={}", "cd".repeat(32)),
                "KNOGGIN_ROUTER_URL=https://router:8443".to_owned(),
                "KNOGGIN_TEST_MODE=1".to_owned(),
            ]
        );
        let h = &b["HostConfig"];
        let memory = MEMORY_MIB * 1024 * 1024;
        assert_eq!(h["ReadonlyRootfs"], true);
        assert_eq!(h["CapDrop"], json!(["ALL"]));
        assert_eq!(h["SecurityOpt"], json!(["no-new-privileges"]));
        assert_eq!(h["PidsLimit"], 256);
        assert_eq!(h["Memory"], memory);
        assert_eq!(h["MemorySwap"], memory);
        assert_eq!(h["NanoCpus"], 1_000_000_000);
        assert_eq!(h["Init"], true);
        assert_eq!(h["RestartPolicy"]["Name"], "no");
        assert_eq!(
            h["Ulimits"],
            json!([{ "Name": "core", "Soft": 0, "Hard": 0 }])
        );
        assert_eq!(h["LogConfig"]["Type"], "json-file");
        assert_eq!(h["LogConfig"]["Config"]["max-size"], "10m");
        assert_eq!(h["LogConfig"]["Config"]["max-file"], "2");
        assert_eq!(h["AutoRemove"], false);
        assert_eq!(h["NetworkMode"], net_name);
        assert_ne!(h["Privileged"], true);
        for never in [
            "Binds",
            "Mounts",
            "PortBindings",
            "CapAdd",
            "Devices",
            "PidMode",
            "VolumesFrom",
        ] {
            assert!(h[never].is_null(), "{never} is set");
        }
        assert!(b["Volumes"].is_null() && b["ExposedPorts"].is_null());
    }

    // List: only this stack's user containers.
    let (code, listed) = r.call(Method::GET, "/v1/containers", TOKEN, None).await;
    assert_eq!(code, 200);
    let items = listed["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{listed}");
    let item = &items[0];
    assert_eq!(item["id"], id.to_string());
    assert_eq!(item["handle"], handle);
    assert_eq!(item["state"], "running");
    assert_eq!(item["address"], created["address"]);
    assert_eq!(item["startedAt"], "2026-11-26T12:00:00.000Z");
    assert!(item["endReason"].is_null() && item["exitCode"].is_null());

    // Stop: an unknown handle and another stack's container are both unknown.
    let (code, _) = r
        .call(Method::POST, "/v1/containers/nope/stop", TOKEN, None)
        .await;
    assert_eq!(code, 404);
    let (code, _) = r
        .call(
            Method::POST,
            "/v1/containers/foreign-user/stop",
            TOKEN,
            None,
        )
        .await;
    assert_eq!(code, 404);
    let (code, _) = r
        .call(Method::POST, "/v1/containers/owners-own/stop", TOKEN, None)
        .await;
    assert_eq!(code, 404);
    let (code, _) = r
        .call(
            Method::POST,
            &format!("/v1/containers/{handle}/stop"),
            TOKEN,
            None,
        )
        .await;
    assert_eq!(code, 204);
    let ended = async {
        loop {
            let (_, listed) = r.call(Method::GET, "/v1/containers", TOKEN, None).await;
            if listed["items"][0]["state"] == "ended" {
                return listed["items"][0].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    let item = tokio::time::timeout(std::time::Duration::from_secs(5), ended)
        .await
        .unwrap();
    assert_eq!(item["endReason"], "stopped");
    assert_eq!(item["exitCode"], 0);

    // Attach: a network of this stack that the router left (a router restart) is joined;
    // another stack's network is not.
    let orphan = format!("kn-{PROJECT}-{}", Uuid::now_v7());
    r.fake.lock().unwrap().add_net(
        &orphan,
        labels(&[("knoggin.role", "user"), ("knoggin.deployment", PROJECT)]),
        "10.201.0.16/29",
    );
    let (code, _) = r.call(Method::POST, "/v1/attach", TOKEN, None).await;
    assert_eq!(code, 204);
    {
        let f = r.fake.lock().unwrap();
        assert_eq!(f.networks[&orphan].members["router-ctr"].1, ["router"]);
        assert!(f.networks["kn-other-x"].members.is_empty());
    }

    // Tidy: the cleanly ended container goes with its network; the orphan network waits a
    // minute; the other stack's ended container and network stay.
    let t0 = Timestamp::from_millis(1_800_000_000_000);
    r.service.tidy(t0).await;
    {
        let f = r.fake.lock().unwrap();
        assert!(!f.containers.contains_key(&handle));
        assert!(!f.networks.contains_key(&net_name));
        assert!(
            f.networks.contains_key(&orphan),
            "a new empty network waits"
        );
    }
    r.service
        .tidy(Timestamp::from_millis(t0.millis() + 61_000))
        .await;
    assert!(!r.fake.lock().unwrap().networks.contains_key(&orphan));
    r.foreign_untouched();
}

#[tokio::test]
async fn a_failed_container_is_kept_ten_minutes_for_the_router_to_ask() {
    let r = rig().await;
    let id = Uuid::now_v7();
    let (_, created) = r
        .call(Method::POST, "/v1/containers", TOKEN, Some(create_body(id)))
        .await;
    let handle = created["handle"].as_str().unwrap().to_owned();
    {
        let mut f = r.fake.lock().unwrap();
        let c = f.containers.get_mut(&handle).unwrap();
        c.status = "exited";
        c.exit_code = 137;
        c.finished_at = "2027-01-15T08:00:00Z".into();
    }
    let finished = Timestamp::parse_rfc3339("2027-01-15T08:00:00Z").unwrap();
    let (_, listed) = r.call(Method::GET, "/v1/containers", TOKEN, None).await;
    assert_eq!(listed["items"][0]["endReason"], "exited");
    assert_eq!(listed["items"][0]["exitCode"], 137);
    r.service
        .tidy(Timestamp::from_millis(finished.millis() + 9 * 60_000))
        .await;
    assert!(r.fake.lock().unwrap().containers.contains_key(&handle));
    r.service
        .tidy(Timestamp::from_millis(finished.millis() + 11 * 60_000))
        .await;
    assert!(!r.fake.lock().unwrap().containers.contains_key(&handle));
    r.foreign_untouched();
}

#[tokio::test]
async fn a_container_that_cannot_start_is_removed_and_its_range_freed() {
    let r = rig().await;
    r.fake.lock().unwrap().fail_start = true;
    let id = Uuid::now_v7();
    let (code, _) = r
        .call(Method::POST, "/v1/containers", TOKEN, Some(create_body(id)))
        .await;
    assert_eq!(code, 503);
    {
        let f = r.fake.lock().unwrap();
        assert!(
            !f.containers
                .values()
                .any(|c| c.labels.get("knoggin.container-id") == Some(&id.to_string())),
            "the container that did not start is gone"
        );
    }
    // Its network is empty: after a minute the tidy-up removes it and frees its range.
    let net = |r: &Rig| {
        r.fake
            .lock()
            .unwrap()
            .networks
            .keys()
            .any(|n| n.ends_with(&id.to_string()))
    };
    assert!(net(&r));
    let now = Timestamp::parse_rfc3339("2027-01-15T08:00:00Z").unwrap();
    r.service.tidy(now).await;
    r.service
        .tidy(Timestamp::from_millis(now.millis() + 2 * 60_000))
        .await;
    assert!(!net(&r));
    r.fake.lock().unwrap().fail_start = false;
    let (code, created) = r
        .call(
            Method::POST,
            "/v1/containers",
            TOKEN,
            Some(create_body(Uuid::now_v7())),
        )
        .await;
    assert_eq!(code, 200, "{created}");
    r.foreign_untouched();
}

#[tokio::test]
async fn docker_launcher_drives_the_service() {
    let r = rig().await;
    let launcher = DockerLauncher::new(&r.url, SecretText::new(TOKEN)).unwrap();
    launcher.attach().await.unwrap();
    let id = Uuid::now_v7();
    let spec = ContainerSpec {
        container_id: id,
        spec: String::new(),
        env: ContainerEnv {
            container_id: id,
            launch_secret: InternalSecret::from_bytes([3; 32]).launch_secret(id),
            router_url: "https://ignored.example:8443".into(),
            router_cert_sha256: CertPin::of(b"internal cert"),
            log_level: LogLevel::Warn,
        },
    };
    let handle = launcher.launch(&spec).await.unwrap();
    let described = launcher
        .describe(std::slice::from_ref(&handle))
        .await
        .unwrap();
    assert_eq!(described.len(), 1);
    let info = &described[0];
    assert_eq!(info.container_id, Some(id));
    assert_eq!(info.state, RunState::Running);
    assert!(info.address.is_some());
    assert!(info.started_at.is_some() && info.ended.is_none());
    {
        // The service set the router's address itself; the `ContainerSpec`'s `router_url` never
        // travels.
        let f = r.fake.lock().unwrap();
        let env = f.containers[&handle.0].body["Env"].to_string();
        assert!(env.contains("KNOGGIN_ROUTER_URL=https://router:8443"));
        assert!(!env.contains("ignored.example"));
        assert!(env.contains("KNOGGIN_LOG__LEVEL=warn"));
    }
    assert_eq!(launcher.list().await.unwrap().len(), 1);
    assert!(
        launcher
            .describe(&[LaunchHandle("0123".into())])
            .await
            .unwrap()
            .is_empty()
    );
    launcher.stop(&handle, "rollout").await.unwrap();
    let ended = async {
        loop {
            let d = launcher
                .describe(std::slice::from_ref(&handle))
                .await
                .unwrap();
            if d[0].state == RunState::Ended {
                return d[0].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    let info = tokio::time::timeout(std::time::Duration::from_secs(5), ended)
        .await
        .unwrap();
    let end = info.ended.unwrap();
    assert_eq!((end.reason, end.exit_code), (EndReason::Stopped, Some(0)));
    assert_eq!(
        launcher.stop(&LaunchHandle("feed".into()), "x").await,
        Err(LaunchError::NotFound)
    );
    assert_eq!(
        launcher.stop(&LaunchHandle("../x".into()), "x").await,
        Err(LaunchError::NotFound)
    );
    // A wrong token is refused; a launcher that is not there is unavailable.
    let wrong = DockerLauncher::new(&r.url, SecretText::new("nope")).unwrap();
    assert!(matches!(wrong.list().await, Err(LaunchError::Refused(_))));
    let gone = DockerLauncher::new("http://127.0.0.1:9", SecretText::new(TOKEN)).unwrap();
    assert_eq!(gone.list().await, Err(LaunchError::Unavailable));
    r.foreign_untouched();
}

#[tokio::test]
async fn a_tidy_while_the_router_is_stopped_takes_it_off_the_network_first() {
    let r = rig().await;
    let (_, created) = r
        .call(
            Method::POST,
            "/v1/containers",
            TOKEN,
            Some(create_body(Uuid::now_v7())),
        )
        .await;
    let handle = created["handle"].as_str().unwrap().to_owned();
    let net = {
        let mut f = r.fake.lock().unwrap();
        let net = f.containers[&handle]
            .networks
            .keys()
            .next()
            .unwrap()
            .clone();
        assert!(f.containers["router-ctr"].networks.contains_key(&net));
        // The router stops (an upgrade), and its parked container exits cleanly.
        f.containers.get_mut("router-ctr").unwrap().status = "exited";
        f.containers.get_mut(&handle).unwrap().status = "exited";
        net
    };
    r.service
        .tidy(Timestamp::from_millis(1_800_000_000_000))
        .await;
    let f = r.fake.lock().unwrap();
    assert!(!f.networks.contains_key(&net), "the network is removed");
    assert!(
        !f.containers["router-ctr"].networks.contains_key(&net),
        "the stopped router still names the removed network, so it cannot start again"
    );
}
