// SPDX-License-Identifier: AGPL-3.0-only
//! `DockerLauncher`: the router's client of the launcher service on one host. The router holds no
//! Docker socket; it sends the container ID and three environment values, and the service does the
//! rest.

use std::net::SocketAddrV4;
use std::time::Duration;

use async_trait::async_trait;
use knoggin_core::Timestamp;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::{
    ContainerInfo, ContainerSpec, EndReason, Ended, LaunchError, LaunchHandle, Launcher, RunState,
};
use crate::settings::SecretText;

/// Every call's time limit. A create starts a container; a stop only asks for one.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The launcher service's client.
pub struct DockerLauncher {
    http: reqwest::Client,
    base: String,
    token: SecretText,
}

impl std::fmt::Debug for DockerLauncher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerLauncher")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Created {
    handle: String,
}

#[derive(Deserialize)]
struct Listing {
    items: Vec<Item>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    id: Option<String>,
    handle: String,
    state: String,
    address: Option<String>,
    started_at: Option<String>,
    end_reason: Option<String>,
    exit_code: Option<i64>,
}

impl Item {
    fn into_info(self) -> Option<ContainerInfo> {
        let state = match self.state.as_str() {
            "running" => RunState::Running,
            "ended" => RunState::Ended,
            _ => return None,
        };
        let ended = (state == RunState::Ended).then_some(Ended {
            reason: match self.end_reason.as_deref() {
                Some("memory") => EndReason::Memory,
                Some("stopped") => EndReason::Stopped,
                _ => EndReason::Exited,
            },
            exit_code: self.exit_code,
        });
        Some(ContainerInfo {
            handle: LaunchHandle(self.handle),
            container_id: self.id.as_deref().and_then(|i| Uuid::parse_str(i).ok()),
            state,
            address: self
                .address
                .as_deref()
                .and_then(|a| a.parse::<SocketAddrV4>().ok()),
            host: None,
            draining: false,
            started_at: self
                .started_at
                .as_deref()
                .and_then(|t| Timestamp::parse_rfc3339(t).ok()),
            ended,
        })
    }
}

impl DockerLauncher {
    /// A client of the service at `url` (`launcher.docker.url`) with the shared token.
    pub fn new(url: &str, token: SecretText) -> Result<Self, LaunchError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(CALL_TIMEOUT)
            .build()
            .map_err(|_| LaunchError::Refused("no HTTP client"))?;
        Ok(Self {
            http,
            base: url.trim_end_matches('/').to_owned(),
            token,
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Response, LaunchError> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(self.token.expose());
        if let Some(body) = body {
            req = req.json(&body);
        }
        let answer = req.send().await.map_err(|_| LaunchError::Unavailable)?;
        match answer.status().as_u16() {
            200..=299 => Ok(answer),
            404 => Err(LaunchError::NotFound),
            400 => Err(LaunchError::Refused("the launcher refused a field")),
            401 => Err(LaunchError::Refused("the launcher refused the token")),
            503 => Err(LaunchError::Refused(
                "the launcher cannot create a container",
            )),
            _ => Err(LaunchError::Unavailable),
        }
    }

    /// `POST /v1/attach`: connects this router's container to every existing per-user
    /// network. The router calls it at start, before the launcher is boxed; it is not part
    /// of the `Launcher` trait.
    pub async fn attach(&self) -> Result<(), LaunchError> {
        self.call(reqwest::Method::POST, "/v1/attach", None)
            .await
            .map(drop)
    }
}

#[async_trait]
impl Launcher for DockerLauncher {
    async fn launch(&self, spec: &ContainerSpec) -> Result<LaunchHandle, LaunchError> {
        let env = &spec.env;
        // The service sets the router's address itself; the spec is empty on one host.
        let body = json!({
            "id": spec.container_id.to_string(),
            "launchSecret": env.launch_secret.to_hex().as_str(),
            "routerCertSha256": env.router_cert_sha256.to_hex(),
            "logLevel": env.log_level.as_str(),
        });
        let answer = self
            .call(reqwest::Method::POST, "/v1/containers", Some(body))
            .await?;
        let created: Created = answer.json().await.map_err(|_| LaunchError::Unavailable)?;
        Ok(LaunchHandle(created.handle))
    }

    async fn describe(&self, handles: &[LaunchHandle]) -> Result<Vec<ContainerInfo>, LaunchError> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .filter(|c| handles.contains(&c.handle))
            .collect())
    }

    async fn list(&self) -> Result<Vec<ContainerInfo>, LaunchError> {
        let answer = self
            .call(reqwest::Method::GET, "/v1/containers", None)
            .await?;
        let listing: Listing = answer.json().await.map_err(|_| LaunchError::Unavailable)?;
        Ok(listing
            .items
            .into_iter()
            .filter_map(Item::into_info)
            .collect())
    }

    async fn stop(&self, handle: &LaunchHandle, _reason: &str) -> Result<(), LaunchError> {
        // A handle is a Docker container ID: hex only, so it is safe in the path.
        if handle.0.is_empty() || !handle.0.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(LaunchError::NotFound);
        }
        self.call(
            reqwest::Method::POST,
            &format!("/v1/containers/{}/stop", handle.0),
            None,
        )
        .await
        .map(drop)
    }
}
