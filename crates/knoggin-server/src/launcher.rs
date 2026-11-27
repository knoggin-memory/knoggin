// SPDX-License-Identifier: AGPL-3.0-only
//! The `Launcher` interface and its small types: how a router starts, finds and stops user
//! containers. `EcsLauncher` (ECS tasks on AWS) and `DockerLauncher` (a client of the
//! launcher service) implement it, and
//! the fleet tests' fake launcher runs `serve` in the test process. The router never sends
//! an image, a command or a mount: only the container ID, the build's spec and the five
//! environment values.

use std::cmp::Ordering;
use std::fmt;
use std::net::SocketAddrV4;

use async_trait::async_trait;
use knoggin_core::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::link::{CertPin, LaunchSecret};

pub mod docker;
pub mod ecs;

pub use docker::DockerLauncher;
pub use ecs::EcsLauncher;

/// Starts, describes, lists and stops user containers.
#[async_trait]
pub trait Launcher: Send + Sync {
    /// Starts one container; returns the launcher's own name for it.
    async fn launch(&self, spec: &ContainerSpec) -> Result<LaunchHandle, LaunchError>;
    /// The facts of these containers. A handle the launcher no longer knows is left out.
    async fn describe(&self, handles: &[LaunchHandle]) -> Result<Vec<ContainerInfo>, LaunchError>;
    /// Every Knoggin user container, each with the Knoggin container ID of its tag or label.
    async fn list(&self) -> Result<Vec<ContainerInfo>, LaunchError>;
    /// Asks for a stop. `reason` is the record's `stop_reason`, for the launcher's logs.
    async fn stop(&self, handle: &LaunchHandle, reason: &str) -> Result<(), LaunchError>;
}

/// What can go wrong with a launcher. "Mark gone" changes nothing on any of these.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// The launcher cannot be reached, or did not answer in time.
    #[error("the launcher cannot be reached")]
    Unavailable,
    /// The launcher refused the call, for example with no capacity or no free network.
    #[error("the launcher refused: {0}")]
    Refused(&'static str),
    /// No container has that handle.
    #[error("no such container")]
    NotFound,
}

/// What `launch` starts: the router's container ID, the build's spec and the environment.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    /// The Knoggin container ID, a UUID the router made.
    pub container_id: Uuid,
    /// What the launcher needs to start the build: the ECS task definition revision;
    /// empty on one host, where the launcher always uses its own image.
    pub spec: String,
    /// The five environment values.
    pub env: ContainerEnv,
}

/// The launcher's own name for a container: a task ARN or a Docker container ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LaunchHandle(pub String);

impl fmt::Display for LaunchHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether a container runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Accepted but not running yet (an ECS task being placed or pulled).
    Pending,
    /// Running.
    Running,
    /// Ended; see `ended`.
    Ended,
}

/// Why a container ended, as the launcher reports it. "Mark gone" decides by the exit code,
/// not by `Stopped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The process exited by itself.
    Exited,
    /// Killed for memory.
    Memory,
    /// Stopped by the launcher.
    Stopped,
}

impl EndReason {
    /// The launcher service's `endReason` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Memory => "memory",
            Self::Stopped => "stopped",
        }
    }
}

/// How an ended container ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    /// The launcher's reason.
    pub reason: EndReason,
    /// The exit code, when the launcher knows it.
    pub exit_code: Option<i64>,
}

/// The facts of one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    /// The launcher's own name.
    pub handle: LaunchHandle,
    /// The Knoggin container ID from its tag or label; `None` when it carries none.
    pub container_id: Option<Uuid>,
    /// Pending, running or ended.
    pub state: RunState,
    /// Its address, an IPv4 address and port such as `10.201.0.3:8443`, once it has one.
    pub address: Option<SocketAddrV4>,
    /// The host it runs on (the ECS container instance); `None` on one host.
    pub host: Option<String>,
    /// Whether that host is draining.
    pub draining: bool,
    /// When it started.
    pub started_at: Option<Timestamp>,
    /// For an ended container: why, and its exit code.
    pub ended: Option<Ended>,
}

/// The five values a launcher sets in a user container's environment, and no others.
#[derive(Debug, Clone)]
pub struct ContainerEnv {
    /// `KNOGGIN_CONTAINER_ID`.
    pub container_id: Uuid,
    /// `KNOGGIN_LAUNCH_SECRET`, hex of the 32 bytes.
    pub launch_secret: LaunchSecret,
    /// `KNOGGIN_ROUTER_URL`: `internal.router_url` on AWS; the launcher service sets
    /// `https://router:8443` itself on one host.
    pub router_url: String,
    /// `KNOGGIN_ROUTER_CERT_SHA256`: the pin of the routers' internal certificate.
    pub router_cert_sha256: CertPin,
    /// `KNOGGIN_LOG__LEVEL`.
    pub log_level: LogLevel,
}

/// The names of the five variables.
pub mod env_names {
    /// The container's UUID.
    pub const CONTAINER_ID: &str = "KNOGGIN_CONTAINER_ID";
    /// Hex of the launch secret.
    pub const LAUNCH_SECRET: &str = "KNOGGIN_LAUNCH_SECRET";
    /// The router's address.
    pub const ROUTER_URL: &str = "KNOGGIN_ROUTER_URL";
    /// The routers' certificate pin.
    pub const ROUTER_CERT_SHA256: &str = "KNOGGIN_ROUTER_CERT_SHA256";
    /// The log level.
    pub const LOG_LEVEL: &str = "KNOGGIN_LOG__LEVEL";
}

/// A container environment that is missing a value or has one in the wrong form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0} is missing or not in its form")]
pub struct EnvError(pub &'static str);

impl ContainerEnv {
    /// The five variables as name and value, for a launcher to set. The launch secret
    /// stays in a buffer that wipes itself.
    pub fn to_vars(&self) -> [(&'static str, Zeroizing<String>); 5] {
        [
            (
                env_names::CONTAINER_ID,
                Zeroizing::new(self.container_id.hyphenated().to_string()),
            ),
            (env_names::LAUNCH_SECRET, self.launch_secret.to_hex()),
            (
                env_names::ROUTER_URL,
                Zeroizing::new(self.router_url.clone()),
            ),
            (
                env_names::ROUTER_CERT_SHA256,
                Zeroizing::new(self.router_cert_sha256.to_hex()),
            ),
            (
                env_names::LOG_LEVEL,
                Zeroizing::new(self.log_level.as_str().to_owned()),
            ),
        ]
    }

    /// Reads the five values through `lookup` (`main` passes the process environment;
    /// no role function reads it).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, EnvError> {
        let get = |name: &'static str| lookup(name).map(Zeroizing::new).ok_or(EnvError(name));
        let container_id = Uuid::parse_str(&get(env_names::CONTAINER_ID)?)
            .map_err(|_| EnvError(env_names::CONTAINER_ID))?;
        let launch_secret = LaunchSecret::from_hex(&get(env_names::LAUNCH_SECRET)?)
            .map_err(|_| EnvError(env_names::LAUNCH_SECRET))?;
        let router_url = get(env_names::ROUTER_URL)?.to_string();
        if !router_url.starts_with("https://") {
            return Err(EnvError(env_names::ROUTER_URL));
        }
        let router_cert_sha256 = get(env_names::ROUTER_CERT_SHA256)?
            .parse()
            .map_err(|_| EnvError(env_names::ROUTER_CERT_SHA256))?;
        let log_level = get(env_names::LOG_LEVEL)?
            .parse()
            .map_err(|_| EnvError(env_names::LOG_LEVEL))?;
        Ok(Self {
            container_id,
            launch_secret,
            router_url,
            router_cert_sha256,
            log_level,
        })
    }
}

/// `log.level`: one of the five levels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// `error`.
    Error,
    /// `warn`.
    Warn,
    /// `info`, the default.
    #[default]
    Info,
    /// `debug`.
    Debug,
    /// `trace`.
    Trace,
}

impl LogLevel {
    /// The config and environment form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = EnvError;

    fn from_str(text: &str) -> Result<Self, EnvError> {
        Ok(match text {
            "error" => Self::Error,
            "warn" => Self::Warn,
            "info" => Self::Info,
            "debug" => Self::Debug,
            "trace" => Self::Trace,
            _ => return Err(EnvError(env_names::LOG_LEVEL)),
        })
    }
}

/// The build a process runs: its release version. `main` fills it from
/// `CARGO_PKG_VERSION`; a fleet test passes two values to run two builds side by side.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BuildInfo {
    /// The release version, a semantic version such as `0.1.0` or `0.2.0-rc.1`.
    pub version: String,
}

impl BuildInfo {
    /// This build, as `main` passes it.
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
        }
    }

    /// Compares two builds as semantic versions. `None` when either is not one; a router
    /// treats such a build as neither newer nor older.
    pub fn compare(&self, other: &BuildInfo) -> Option<Ordering> {
        Some(Version::parse(&self.version)?.cmp(&Version::parse(&other.version)?))
    }

    /// Whether this build is newer than `other`.
    pub fn is_newer_than(&self, other: &BuildInfo) -> bool {
        self.compare(other) == Some(Ordering::Greater)
    }
}

/// `MAJOR.MINOR.PATCH[-PRE][+BUILD]`, ordered as semantic versioning says.
#[derive(Debug, PartialEq, Eq)]
struct Version<'a> {
    core: [u64; 3],
    pre: Vec<&'a str>,
}

impl<'a> Version<'a> {
    fn parse(text: &'a str) -> Option<Self> {
        let text = text.split_once('+').map_or(text, |(v, _)| v);
        let (core_text, pre_text) = match text.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (text, None),
        };
        let mut parts = core_text.split('.');
        let mut core = [0u64; 3];
        for slot in &mut core {
            *slot = number(parts.next()?)?;
        }
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre_text {
            Some(p) => {
                let ids: Vec<&str> = p.split('.').collect();
                if ids.iter().any(|i| {
                    i.is_empty() || !i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                }) {
                    return None;
                }
                ids
            }
            None => Vec::new(),
        };
        Some(Self { core, pre })
    }
}

fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl Ord for Version<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A pre-release sorts before its release.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (number(a), number(b)) {
                            (Some(x), Some(y)) => x.cmp(&y),
                            (Some(_), None) => Ordering::Less,
                            (None, Some(_)) => Ordering::Greater,
                            (None, None) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            }
        })
    }
}

impl PartialOrd for Version<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::link::InternalSecret;

    #[test]
    fn environment_round_trip() {
        let id = Uuid::now_v7();
        let env = ContainerEnv {
            container_id: id,
            launch_secret: InternalSecret::from_bytes([1; 32]).launch_secret(id),
            router_url: "https://router:8443".into(),
            router_cert_sha256: CertPin::of(b"cert"),
            log_level: LogLevel::Debug,
        };
        let vars: HashMap<&str, String> = env
            .to_vars()
            .iter()
            .map(|(k, v)| (*k, v.to_string()))
            .collect();
        assert_eq!(vars.len(), 5);
        assert_eq!(vars["KNOGGIN_LOG__LEVEL"], "debug");
        assert_eq!(vars["KNOGGIN_CONTAINER_ID"], id.to_string());
        assert_eq!(vars["KNOGGIN_LAUNCH_SECRET"].len(), 64);
        let back = ContainerEnv::from_lookup(|k| vars.get(k).cloned()).unwrap();
        assert_eq!(back.container_id, id);
        assert_eq!(back.router_cert_sha256, env.router_cert_sha256);
        assert_eq!(
            back.launch_secret.router_token().expose(),
            env.launch_secret.router_token().expose()
        );
        assert!(!format!("{back:?}").contains(&vars["KNOGGIN_LAUNCH_SECRET"][..16]));
        for (name, bad) in [
            ("KNOGGIN_LOG__LEVEL", "loud"),
            ("KNOGGIN_ROUTER_URL", "http://router:8443"),
            ("KNOGGIN_LAUNCH_SECRET", "12"),
            ("KNOGGIN_CONTAINER_ID", "c"),
        ] {
            let mut v = vars.clone();
            v.insert(name, bad.into());
            assert_eq!(
                ContainerEnv::from_lookup(|k| v.get(k).cloned()).unwrap_err(),
                EnvError(name)
            );
            v.remove(name);
            assert!(ContainerEnv::from_lookup(|k| v.get(k).cloned()).is_err());
        }
    }

    #[test]
    fn builds_compare_as_semantic_versions() {
        let b = |v: &str| BuildInfo::new(v);
        let ordered = [
            "0.1.0-alpha",
            "0.1.0-alpha.1",
            "0.1.0-alpha.beta",
            "0.1.0-beta.2",
            "0.1.0-beta.11",
            "0.1.0-rc.1",
            "0.1.0",
            "0.1.1",
            "0.2.0",
            "0.10.0",
            "1.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(b(pair[1]).is_newer_than(&b(pair[0])), "{pair:?}");
            assert!(!b(pair[0]).is_newer_than(&b(pair[1])), "{pair:?}");
        }
        assert_eq!(b("1.0.0+abc").compare(&b("1.0.0")), Some(Ordering::Equal));
        for bad in ["1.0", "1.0.0.0", "v1.0.0", "1.0.0-", "1.0.0-a..b", "1.x.0"] {
            assert_eq!(b(bad).compare(&b("1.0.0")), None, "{bad}");
        }
    }
}
