// SPDX-License-Identifier: AGPL-3.0-only
//! `knoggin launcher` (tab "Deployment", The launcher service): the one process on one host
//! that holds the Docker socket. It accepts four calls from the router, each with the shared
//! token, and starts every user container from one description fixed in this file: no field
//! of a request reaches a container's host settings. It acts only on containers and
//! networks that carry both labels it sets itself, `knoggin.role=user` and
//! `knoggin.deployment=<its own Compose project>`, and tidies up what has ended.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bollard::Docker;
use bollard::models::{
    ContainerCreateBody, ContainerInspectResponse, EndpointSettings, HostConfig,
    HostConfigLogConfig, Ipam, IpamConfig, NetworkConnectRequest, NetworkCreateRequest,
    NetworkDisconnectRequest, NetworkingConfig, ResourcesUlimits, RestartPolicy,
    RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptions, ListContainersOptions, ListNetworksOptions, RemoveContainerOptions,
    StopContainerOptions,
};
use bytes::Bytes;
use http::StatusCode;
use knoggin_core::Timestamp;
use knoggin_http::error::{ApiError, Code};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::launcher::{ContainerEnv, LogLevel};
use crate::link::{CertPin, LaunchSecret};
use crate::settings::{Cidr, LauncherServiceSettings, SecretText};

/// The label every user container and network carries, with the value `user`.
pub const ROLE_LABEL: &str = "knoggin.role";
/// The label that names the Compose project the launcher belongs to.
pub const DEPLOYMENT_LABEL: &str = "knoggin.deployment";
/// The label that carries a user container's Knoggin container ID.
pub const CONTAINER_ID_LABEL: &str = "knoggin.container-id";
/// Docker Compose's project label, read from the launcher's own container.
pub const COMPOSE_PROJECT: &str = "com.docker.compose.project";
/// Docker Compose's service label, which finds the router's container.
pub const COMPOSE_SERVICE: &str = "com.docker.compose.service";

/// The address a container reaches the router at on its own network.
const ROUTER_URL: &str = "https://router:8443";
/// The port a user container listens on.
const CONTAINER_PORT: u16 = 8443;
/// The stop timeout, here and in the container's own setting.
const STOP_TIMEOUT_S: i32 = 120;
/// How long a container that did not end cleanly is kept for the router to ask why.
const KEEP_FAILED: Duration = Duration::from_secs(600);
/// How long a labelled network may stand without its user container.
const EMPTY_NETWORK: Duration = Duration::from_secs(60);
/// How often the launcher tidies up.
const TIDY_EVERY: Duration = Duration::from_secs(5);
/// A request body is a few short fields.
const BODY_LIMIT: usize = 16 * 1024;
/// Every Docker call that is not a stop.
const DOCKER_TIMEOUT: Duration = Duration::from_secs(30);

/// Why the launcher could not start.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// Its own container could not be inspected, or carries no Compose project.
    #[error("the launcher cannot read its own container: {0}")]
    OwnContainer(String),
}

/// The launcher's settings as the service runs with them.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// `launcher.docker.token`.
    pub token: SecretText,
    /// `containers.user_net_cidr`.
    pub user_net: Cidr,
    /// `containers.memory_mib`.
    pub memory_mib: u64,
    /// Test mode: passed on to the containers as `KNOGGIN_TEST_MODE=1`.
    pub test_mode: bool,
}

/// The running service.
pub struct LauncherService {
    docker: Docker,
    config: ServiceConfig,
    /// The launcher's own Compose project.
    project: String,
    /// The launcher's own image ID: user containers run the same build.
    image: String,
    /// Creates are one at a time, so two never pick the same network range.
    create: tokio::sync::Mutex<()>,
    /// Handles the launcher was asked to stop: their end reason is `stopped`.
    stopped: Mutex<HashSet<String>>,
    /// Labelled networks without their container, and since when.
    empty_since: Mutex<HashMap<String, i64>>,
    /// The stops in progress (each waits up to 120 seconds).
    stops: Mutex<JoinSet<()>>,
}

impl std::fmt::Debug for LauncherService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LauncherService")
            .field("project", &self.project)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unavailable(what: &str) -> ApiError {
    tracing::warn!(kind = "docker", "{what}");
    ApiError::new(Code::Unavailable, "The container could not be created")
}

/// A Docker answer of 404.
fn is_not_found(e: &bollard::errors::Error) -> bool {
    matches!(
        e,
        bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

async fn timed<T>(
    f: impl Future<Output = Result<T, bollard::errors::Error>>,
) -> Result<T, bollard::errors::Error> {
    tokio::time::timeout(DOCKER_TIMEOUT, f)
        .await
        .unwrap_or(Err(bollard::errors::Error::RequestTimeoutError))
}

fn labels_of(inspect: &ContainerInspectResponse) -> HashMap<String, String> {
    inspect
        .config
        .as_ref()
        .and_then(|c| c.labels.clone())
        .unwrap_or_default()
}

/// The body of `POST /v1/containers`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateBody {
    id: String,
    launch_secret: String,
    router_cert_sha256: String,
    log_level: String,
}

fn is_hex64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

impl LauncherService {
    /// Starts the service: inspects its own container (`own`, the ID Docker set as its
    /// hostname) for its Compose project and image.
    pub async fn start(
        docker: Docker,
        own: &str,
        config: ServiceConfig,
    ) -> Result<Arc<Self>, ServiceError> {
        let me = timed(docker.inspect_container(own.trim(), None))
            .await
            .map_err(|e| ServiceError::OwnContainer(e.to_string()))?;
        let project = labels_of(&me)
            .get(COMPOSE_PROJECT)
            .filter(|p| !p.is_empty())
            .cloned()
            .ok_or_else(|| ServiceError::OwnContainer(format!("no label {COMPOSE_PROJECT}")))?;
        let image = me
            .image
            .filter(|i| !i.is_empty())
            .ok_or_else(|| ServiceError::OwnContainer("no image ID".into()))?;
        Ok(Arc::new(Self {
            docker,
            config,
            project,
            image,
            create: tokio::sync::Mutex::new(()),
            stopped: Mutex::new(HashSet::new()),
            empty_since: Mutex::new(HashMap::new()),
            stops: Mutex::new(JoinSet::new()),
        }))
    }

    /// The four calls, behind the token.
    pub fn router(self: &Arc<Self>) -> Router {
        let token = self.config.token.clone();
        Router::new()
            .route("/v1/containers", post(create).get(list))
            .route("/v1/containers/{handle}/stop", post(stop))
            .route("/v1/attach", post(attach))
            .fallback(|| async { ApiError::not_found("No such route") })
            .layer(middleware::from_fn(move |req: Request, next: Next| {
                let token = token.clone();
                async move {
                    let given = req
                        .headers()
                        .get(http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .unwrap_or("");
                    if !knoggin_crypto::token::eq(given.as_bytes(), token.expose().as_bytes()) {
                        return ApiError::new(Code::Unauthenticated, "A valid token is required")
                            .into_response();
                    }
                    next.run(req).await
                }
            }))
            .with_state(self.clone())
    }

    fn our_labels(&self) -> [(String, String); 2] {
        [
            (ROLE_LABEL.into(), "user".into()),
            (DEPLOYMENT_LABEL.into(), self.project.clone()),
        ]
    }

    /// Whether a container or network carries both of the launcher's own labels.
    fn is_ours(&self, labels: &HashMap<String, String>) -> bool {
        self.our_labels()
            .iter()
            .all(|(k, v)| labels.get(k) == Some(v))
    }

    fn label_filter(&self) -> HashMap<String, Vec<String>> {
        let labels = self
            .our_labels()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        HashMap::from([("label".to_owned(), labels)])
    }

    fn network_name(&self, id: &str) -> String {
        format!("kn-{}-{id}", self.project)
    }

    /// The running router container of this project.
    async fn router_container(&self) -> Result<String, ApiError> {
        self.find_router(false).await
    }

    /// The router container of this project; with `stopped`, also one that is not running.
    async fn find_router(&self, stopped: bool) -> Result<String, ApiError> {
        let filters = HashMap::from([(
            "label".to_owned(),
            vec![
                format!("{COMPOSE_PROJECT}={}", self.project),
                format!("{COMPOSE_SERVICE}=router"),
            ],
        )]);
        let found = timed(self.docker.list_containers(Some(ListContainersOptions {
            all: stopped,
            filters: Some(filters),
            ..Default::default()
        })))
        .await
        .map_err(|_| unavailable("the router's container could not be listed"))?;
        found
            .into_iter()
            .find(|c| {
                let l = c.labels.clone().unwrap_or_default();
                l.get(COMPOSE_PROJECT) == Some(&self.project)
                    && l.get(COMPOSE_SERVICE).map(String::as_str) == Some("router")
            })
            .and_then(|c| c.id)
            .ok_or_else(|| unavailable("the router's container is not running"))
    }

    /// The lowest /29 of `containers.user_net_cidr` that no network on the host uses.
    async fn free_range(&self) -> Result<Cidr, ApiError> {
        let networks = timed(self.docker.list_networks(None::<ListNetworksOptions>))
            .await
            .map_err(|_| unavailable("the networks could not be listed"))?;
        let used: Vec<Cidr> = networks
            .iter()
            .filter_map(|n| n.ipam.as_ref()?.config.as_ref())
            .flatten()
            .filter_map(|c| c.subnet.as_deref()?.parse().ok())
            .collect();
        free_range(&self.config.user_net, &used)
            .ok_or_else(|| unavailable("no network range is free"))
    }

    /// The fixed description of a user container.
    fn container_body(&self, id: Uuid, env: &ContainerEnv, network: &str) -> ContainerCreateBody {
        let mut vars: Vec<String> = env
            .to_vars()
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_str()))
            .collect();
        if self.config.test_mode {
            vars.push("KNOGGIN_TEST_MODE=1".into());
        }
        let memory =
            i64::try_from(self.config.memory_mib.saturating_mul(1024 * 1024)).unwrap_or(i64::MAX);
        let mut labels: HashMap<String, String> = self.our_labels().into_iter().collect();
        labels.insert(CONTAINER_ID_LABEL.into(), id.to_string());
        ContainerCreateBody {
            image: Some(self.image.clone()),
            cmd: Some(vec!["serve".into()]),
            user: Some("65532:65532".into()),
            env: Some(vars),
            labels: Some(labels),
            stop_timeout: Some(i64::from(STOP_TIMEOUT_S)),
            host_config: Some(HostConfig {
                readonly_rootfs: Some(true),
                cap_drop: Some(vec!["ALL".into()]),
                security_opt: Some(vec!["no-new-privileges".into()]),
                pids_limit: Some(256),
                memory: Some(memory),
                memory_swap: Some(memory),
                nano_cpus: Some(1_000_000_000),
                init: Some(true),
                restart_policy: Some(RestartPolicy {
                    name: Some(RestartPolicyNameEnum::NO),
                    maximum_retry_count: None,
                }),
                ulimits: Some(vec![ResourcesUlimits {
                    name: Some("core".into()),
                    soft: Some(0),
                    hard: Some(0),
                }]),
                log_config: Some(HostConfigLogConfig {
                    typ: Some("json-file".into()),
                    config: Some(HashMap::from([
                        ("max-size".into(), "10m".into()),
                        ("max-file".into(), "2".into()),
                    ])),
                }),
                network_mode: Some(network.to_owned()),
                auto_remove: Some(false),
                privileged: Some(false),
                ..Default::default()
            }),
            networking_config: Some(NetworkingConfig {
                endpoints_config: Some(HashMap::from([(
                    network.to_owned(),
                    EndpointSettings::default(),
                )])),
            }),
            ..Default::default()
        }
    }

    async fn create(&self, body: CreateBody) -> Result<Value, ApiError> {
        let id = Uuid::parse_str(&body.id)
            .ok()
            .filter(|u| u.hyphenated().to_string() == body.id)
            .ok_or_else(|| ApiError::invalid_argument("id", "id must be a UUID"))?;
        if !is_hex64(&body.launch_secret) {
            return Err(ApiError::invalid_argument(
                "launchSecret",
                "launchSecret must be 64 hex characters",
            ));
        }
        if !is_hex64(&body.router_cert_sha256) {
            return Err(ApiError::invalid_argument(
                "routerCertSha256",
                "routerCertSha256 must be 64 hex characters",
            ));
        }
        let log_level: LogLevel = body.log_level.parse().map_err(|_| {
            ApiError::invalid_argument("logLevel", "logLevel must be one of the five levels")
        })?;
        let launch_secret = LaunchSecret::from_hex(&body.launch_secret)
            .map_err(|_| ApiError::invalid_argument("launchSecret", "not a launch secret"))?;
        let router_cert_sha256: CertPin = body
            .router_cert_sha256
            .parse()
            .map_err(|_| ApiError::invalid_argument("routerCertSha256", "not a pin"))?;
        let env = ContainerEnv {
            container_id: id,
            launch_secret,
            router_url: ROUTER_URL.into(),
            router_cert_sha256,
            log_level,
        };

        let _one = self.create.lock().await;
        let range = self.free_range().await?;
        let router = self.router_container().await?;
        let name = self.network_name(&id.to_string());
        let network = NetworkCreateRequest {
            name: name.clone(),
            driver: Some("bridge".into()),
            internal: Some(true),
            ipam: Some(Ipam {
                config: Some(vec![IpamConfig {
                    subnet: Some(range.to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            options: Some(HashMap::from([(
                "com.docker.network.bridge.gateway_mode_ipv4".into(),
                "isolated".into(),
            )])),
            labels: Some(self.our_labels().into_iter().collect()),
            ..Default::default()
        };
        timed(self.docker.create_network(network))
            .await
            .map_err(|_| unavailable("a network could not be created"))?;
        // A failure from here on leaves a labelled network without a container, which the
        // tidy-up removes after a minute.
        timed(self.docker.connect_network(
            &name,
            NetworkConnectRequest {
                container: router.clone(),
                endpoint_config: Some(EndpointSettings {
                    aliases: Some(vec!["router".into()]),
                    ..Default::default()
                }),
            },
        ))
        .await
        .map_err(|_| unavailable("the router could not join a network"))?;
        let created = timed(self.docker.create_container(
            Some(CreateContainerOptions {
                name: Some(name.clone()),
                ..Default::default()
            }),
            self.container_body(id, &env, &name),
        ))
        .await
        .map_err(|_| unavailable("a container could not be created"))?;
        timed(self.docker.start_container(&created.id, None))
            .await
            .map_err(|_| unavailable("a container could not be started"))?;
        let inspect = timed(self.docker.inspect_container(&created.id, None))
            .await
            .map_err(|_| unavailable("a container could not be inspected"))?;
        let ip = address_on(&inspect, &name)
            .ok_or_else(|| unavailable("a container has no address on its network"))?;
        Ok(json!({
            "handle": created.id,
            "address": SocketAddrV4::new(ip, CONTAINER_PORT).to_string(),
        }))
    }

    async fn list(&self) -> Result<Value, ApiError> {
        let found = timed(self.docker.list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(self.label_filter()),
            ..Default::default()
        })))
        .await
        .map_err(|_| unavailable("the containers could not be listed"))?;
        let mut items = Vec::new();
        for summary in found {
            let Some(handle) = summary.id else { continue };
            if !self.is_ours(&summary.labels.unwrap_or_default()) {
                continue;
            }
            let inspect = match timed(self.docker.inspect_container(&handle, None)).await {
                Ok(i) => i,
                // Removed in between.
                Err(e) if is_not_found(&e) => continue,
                Err(_) => return Err(unavailable("a container could not be inspected")),
            };
            let labels = labels_of(&inspect);
            if !self.is_ours(&labels) {
                continue;
            }
            let id = labels.get(CONTAINER_ID_LABEL).cloned();
            let state = inspect.state.clone().unwrap_or_default();
            let ended = !state.running.unwrap_or(false)
                && matches!(
                    state.status,
                    Some(
                        bollard::models::ContainerStateStatusEnum::EXITED
                            | bollard::models::ContainerStateStatusEnum::DEAD
                    )
                );
            let address = id
                .as_deref()
                .and_then(|id| address_on(&inspect, &self.network_name(id)))
                .map(|ip| SocketAddrV4::new(ip, CONTAINER_PORT).to_string());
            let started_at = state
                .started_at
                .as_deref()
                .and_then(|t| Timestamp::parse_rfc3339(t).ok())
                .and_then(Timestamp::to_rfc3339);
            let (end_reason, exit_code) = if ended {
                let reason = if state.oom_killed.unwrap_or(false) {
                    "memory"
                } else if lock(&self.stopped).contains(&handle) {
                    "stopped"
                } else {
                    "exited"
                };
                (json!(reason), json!(state.exit_code.unwrap_or(-1)))
            } else {
                (Value::Null, Value::Null)
            };
            items.push(json!({
                "id": id,
                "handle": handle,
                "state": if ended { "ended" } else { "running" },
                "address": address,
                "startedAt": started_at,
                "endReason": end_reason,
                "exitCode": exit_code,
            }));
        }
        Ok(json!({ "items": items }))
    }

    async fn stop(self: &Arc<Self>, handle: &str) -> Result<(), ApiError> {
        let not_found = || ApiError::not_found("No such container");
        let inspect = match timed(self.docker.inspect_container(handle, None)).await {
            Ok(i) => i,
            Err(e) if is_not_found(&e) => return Err(not_found()),
            Err(_) => return Err(unavailable("a container could not be inspected")),
        };
        // Anything the launcher did not start is unknown to it.
        if !self.is_ours(&labels_of(&inspect)) {
            return Err(not_found());
        }
        let id = inspect.id.unwrap_or_else(|| handle.to_owned());
        lock(&self.stopped).insert(id.clone());
        let docker = self.docker.clone();
        let mut stops = lock(&self.stops);
        while stops.try_join_next().is_some() {}
        stops.spawn(async move {
            let options = StopContainerOptions {
                t: Some(STOP_TIMEOUT_S),
                signal: None,
            };
            if docker.stop_container(&id, Some(options)).await.is_err() {
                tracing::warn!(kind = "docker", "a container could not be stopped");
            }
        });
        Ok(())
    }

    async fn attach(&self) -> Result<(), ApiError> {
        let router = self.router_container().await?;
        let inspect = timed(self.docker.inspect_container(&router, None))
            .await
            .map_err(|_| unavailable("the router's container could not be inspected"))?;
        let joined: HashSet<String> = inspect
            .network_settings
            .and_then(|n| n.networks)
            .map(|n| n.into_keys().collect())
            .unwrap_or_default();
        let networks = timed(self.docker.list_networks(Some(ListNetworksOptions {
            filters: Some(self.label_filter()),
        })))
        .await
        .map_err(|_| unavailable("the networks could not be listed"))?;
        for n in networks {
            let Some(name) = n.name else { continue };
            if !self.is_ours(&n.labels.unwrap_or_default()) || joined.contains(&name) {
                continue;
            }
            timed(self.docker.connect_network(
                &name,
                NetworkConnectRequest {
                    container: router.clone(),
                    endpoint_config: Some(EndpointSettings {
                        aliases: Some(vec!["router".into()]),
                        ..Default::default()
                    }),
                },
            ))
            .await
            .map_err(|_| unavailable("the router could not join a network"))?;
        }
        Ok(())
    }

    /// One tidy-up pass: removes user containers that ended with code 0 at once, and
    /// those that ended otherwise after 10 minutes, each with its network; and labelled
    /// networks that have had no user container for a minute. The router is disconnected
    /// from a network first, because Docker refuses to remove one that still has an
    /// endpoint.
    pub async fn tidy(&self, now: Timestamp) {
        let Ok(found) = timed(self.docker.list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(self.label_filter()),
            ..Default::default()
        })))
        .await
        else {
            return;
        };
        let mut alive_networks = HashSet::new();
        for summary in found {
            let Some(handle) = summary.id else { continue };
            let Ok(inspect) = timed(self.docker.inspect_container(&handle, None)).await else {
                continue;
            };
            let labels = labels_of(&inspect);
            if !self.is_ours(&labels) {
                continue;
            }
            let Some(network) = labels
                .get(CONTAINER_ID_LABEL)
                .map(|id| self.network_name(id))
            else {
                continue;
            };
            let state = inspect.state.unwrap_or_default();
            let ended = matches!(
                state.status,
                Some(
                    bollard::models::ContainerStateStatusEnum::EXITED
                        | bollard::models::ContainerStateStatusEnum::DEAD
                )
            );
            let finished = state
                .finished_at
                .as_deref()
                .and_then(|t| Timestamp::parse_rfc3339(t).ok());
            let keep_ms = i64::try_from(KEEP_FAILED.as_millis()).unwrap_or(i64::MAX);
            let due = ended
                && (state.exit_code == Some(0)
                    || finished.is_some_and(|f| now.millis().saturating_sub(f.millis()) > keep_ms));
            if !due {
                alive_networks.insert(network);
                continue;
            }
            let options = RemoveContainerOptions {
                force: true,
                ..Default::default()
            };
            if timed(self.docker.remove_container(&handle, Some(options)))
                .await
                .is_ok()
            {
                lock(&self.stopped).remove(&handle);
                self.remove_network(&network).await;
            }
        }
        let Ok(networks) = timed(self.docker.list_networks(Some(ListNetworksOptions {
            filters: Some(self.label_filter()),
        })))
        .await
        else {
            return;
        };
        let at = now.millis();
        let empty_ms = i64::try_from(EMPTY_NETWORK.as_millis()).unwrap_or(i64::MAX);
        let mut doomed = Vec::new();
        {
            let mut empty = lock(&self.empty_since);
            let mut seen = HashSet::new();
            for n in networks {
                let Some(name) = n.name else { continue };
                if !self.is_ours(&n.labels.unwrap_or_default()) || alive_networks.contains(&name) {
                    continue;
                }
                seen.insert(name.clone());
                let since = *empty.entry(name.clone()).or_insert(at);
                if at.saturating_sub(since) > empty_ms {
                    doomed.push(name);
                }
            }
            empty.retain(|name, _| seen.contains(name));
        }
        for name in doomed {
            self.remove_network(&name).await;
            lock(&self.empty_since).remove(&name);
        }
    }

    async fn remove_network(&self, name: &str) {
        // A stopped router too: Docker removes a network that only a stopped container
        // names, and that container then cannot start again ("network not found").
        if let Ok(router) = self.find_router(true).await {
            let disconnected = timed(self.docker.disconnect_network(
                name,
                NetworkDisconnectRequest {
                    container: router.clone(),
                    force: Some(true),
                },
            ))
            .await;
            if disconnected.is_err() && self.joined(&router, name).await != Some(false) {
                tracing::warn!(
                    kind = "docker",
                    "the router could not leave a network; it is kept"
                );
                return;
            }
        }
        if timed(self.docker.remove_network(name)).await.is_err() {
            tracing::warn!(kind = "docker", "a network could not be removed");
        }
    }

    /// Whether `container` is on the network `name`; `None` when Docker cannot say.
    async fn joined(&self, container: &str, name: &str) -> Option<bool> {
        let inspect = timed(self.docker.inspect_container(container, None))
            .await
            .ok()?;
        Some(
            inspect
                .network_settings
                .and_then(|n| n.networks)
                .is_some_and(|n| n.contains_key(name)),
        )
    }

    /// Stops the stops in progress (at shutdown).
    fn abort_stops(&self) {
        lock(&self.stops).abort_all();
    }
}

/// The IPv4 address of a container on `network`.
fn address_on(inspect: &ContainerInspectResponse, network: &str) -> Option<Ipv4Addr> {
    inspect
        .network_settings
        .as_ref()?
        .networks
        .as_ref()?
        .get(network)?
        .ip_address
        .as_deref()?
        .parse()
        .ok()
}

/// The lowest /29 inside `within` that overlaps none of `used`.
fn free_range(within: &Cidr, used: &[Cidr]) -> Option<Cidr> {
    let base = match within.to_string().split_once('/') {
        Some((a, p)) => (a.parse::<Ipv4Addr>().ok()?, p.parse::<u32>().ok()?),
        None => return None,
    };
    let (start, prefix) = (u32::from(base.0), base.1);
    if prefix > 29 {
        return None;
    }
    let count = 1u64 << (29 - prefix);
    (0..count).find_map(|i| {
        let net = start.checked_add(u32::try_from(i * 8).ok()?)?;
        let candidate: Cidr = format!("{}/29", Ipv4Addr::from(net)).parse().ok()?;
        (!used.iter().any(|u| u.overlaps(&candidate))).then_some(candidate)
    })
}

type Answer = Result<Response, ApiError>;

async fn create(State(s): State<Arc<LauncherService>>, body: Bytes) -> Answer {
    if body.len() > BODY_LIMIT {
        return Err(ApiError::new(
            Code::PayloadTooLarge,
            "The body is too large",
        ));
    }
    let body: CreateBody = serde_json::from_slice(&body)
        .map_err(|_| ApiError::invalid_argument("body", "the body is not the expected JSON"))?;
    let answer = s.create(body).await?;
    Ok((StatusCode::OK, axum::Json(answer)).into_response())
}

async fn list(State(s): State<Arc<LauncherService>>) -> Answer {
    Ok((StatusCode::OK, axum::Json(s.list().await?)).into_response())
}

async fn stop(State(s): State<Arc<LauncherService>>, Path(handle): Path<String>) -> Answer {
    s.stop(&handle).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn attach(State(s): State<Arc<LauncherService>>) -> Answer {
    s.attach().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `knoggin launcher`: serves the four calls on `launcher.bind` and tidies up every few
/// seconds until `stop`. `own` is the launcher's own container ID (its hostname). Returns
/// the exit code: 1 when it cannot read its own container or bind its port.
pub async fn run(
    settings: LauncherServiceSettings,
    test_mode: bool,
    docker: Docker,
    own: &str,
    mut stop: watch::Receiver<bool>,
) -> i32 {
    let config = ServiceConfig {
        token: settings.token,
        user_net: settings.user_net_cidr,
        memory_mib: settings.memory_mib,
        test_mode,
    };
    let service = match LauncherService::start(docker, own, config).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(kind = "launcher", error = %e, "the launcher cannot start");
            return 1;
        }
    };
    let listener = match tokio::net::TcpListener::bind(settings.bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(kind = "launcher", error = %e, "the launcher cannot listen");
            return 1;
        }
    };
    let mut serve_stop = stop.clone();
    let server = tokio::spawn(knoggin_http::edge::listen::serve_tcp(
        listener,
        service.router(),
        knoggin_http::edge::Arrival::Public,
        knoggin_http::edge::listen::ListenConfig::DEFAULT,
        async move {
            let _ = serve_stop.wait_for(|s| *s).await;
        },
    ));
    tracing::info!(kind = "launcher", "the launcher is serving");
    loop {
        tokio::select! {
            () = tokio::time::sleep(TIDY_EVERY) => {
                service.tidy(Timestamp::from_millis(knoggin_state::system_clock()())).await;
            }
            _ = stop.wait_for(|s| *s) => break,
        }
    }
    service.abort_stops();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lowest_free_range_is_taken() {
        let within: Cidr = "10.201.0.0/16".parse().unwrap();
        assert_eq!(
            free_range(&within, &[]).unwrap().to_string(),
            "10.201.0.0/29"
        );
        let used: Vec<Cidr> = vec![
            "10.201.0.0/29".parse().unwrap(),
            "10.201.0.16/28".parse().unwrap(),
            "172.17.0.0/16".parse().unwrap(),
        ];
        assert_eq!(
            free_range(&within, &used).unwrap().to_string(),
            "10.201.0.8/29"
        );
        let all: Vec<Cidr> = vec!["10.0.0.0/8".parse().unwrap()];
        assert!(free_range(&within, &all).is_none());
        let tiny: Cidr = "10.201.0.0/29".parse().unwrap();
        assert_eq!(free_range(&tiny, &[]).unwrap(), tiny);
    }
}
