// SPDX-License-Identifier: AGPL-3.0-only
//! The server-only settings: what the router,
//! the lifecycle task and the launcher service read beyond the edge and store layers'
//! settings in `knoggin_http`. The binary fills them from the file and the environment,
//! with secrets already read from their files. A user container reads none of these: it
//! gets five environment values (`launcher::ContainerEnv`) and the rest in its claim.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer};
use zeroize::Zeroizing;

use crate::launcher::LogLevel;
use crate::link::InternalSecret;

/// The memory the model may take at its peak, kept out of a container's store budget.
pub const MODEL_RESERVE_BYTES: u64 = 928 * 1024 * 1024;

/// How many store budgets one user's containers may upload in an hour by default.
pub const UPLOAD_BUDGETS_PER_HOUR: u64 = 720;

/// The bytes all open stores of a container may hold together:
/// `(memory_limit_bytes - 928 MiB) * 0.4`, the memory budget.
pub fn store_budget(memory_limit_bytes: u64) -> u64 {
    memory_limit_bytes.saturating_sub(MODEL_RESERVE_BYTES) / 5 * 2
}

/// The router's settings beyond the edge layer's.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterSettings {
    /// `server.trusted_proxies`.
    #[serde(default)]
    pub trusted_proxies: Vec<Cidr>,
    /// `server.image_digest`, shown in the admin status.
    #[serde(default)]
    pub image_digest: Option<String>,
    /// The router's rate limits and the upload budget.
    #[serde(default)]
    pub limits: RouterLimits,
    /// `storage.*`.
    pub storage: StorageSettings,
    /// `retention.*`.
    #[serde(default)]
    pub retention: RetentionSettings,
    /// `containers.*`.
    #[serde(default)]
    pub containers: ContainerSettings,
    /// `internal.*`.
    pub internal: InternalSettings,
    /// `rollout.*`.
    #[serde(default)]
    pub rollout: RolloutSettings,
    /// `log.level`, passed on to containers as `KNOGGIN_LOG__LEVEL`.
    #[serde(default)]
    pub log_level: LogLevel,
}

impl RouterSettings {
    /// `limits.upload_bytes_per_hour`, or its default: 720 store budgets of one container.
    pub fn upload_bytes_per_hour(&self) -> u64 {
        self.limits.upload_bytes_per_hour.unwrap_or_else(|| {
            UPLOAD_BUDGETS_PER_HOUR
                .saturating_mul(store_budget(self.containers.memory_limit_bytes()))
        })
    }
}

/// A token-bucket rate: requests per second and burst.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateSetting {
    /// Sustained requests per second.
    pub rps: f64,
    /// Bucket size.
    pub burst: f64,
}

/// The `limits` settings only a router reads.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterLimits {
    /// `limits.per_address_rps` and `_burst` (50, 100).
    pub per_address: RateSetting,
    /// `limits.per_user_rps` and `_burst` (100, 200).
    pub per_user: RateSetting,
    /// `limits.upload_bytes_per_hour`; unset means 720 store budgets.
    pub upload_bytes_per_hour: Option<u64>,
}

impl Default for RouterLimits {
    fn default() -> Self {
        Self {
            per_address: RateSetting {
                rps: 50.0,
                burst: 100.0,
            },
            per_user: RateSetting {
                rps: 100.0,
                burst: 200.0,
            },
            upload_bytes_per_hour: None,
        }
    }
}

/// `storage.*`: the snapshot bucket. Read by the router and the lifecycle task.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageSettings {
    /// `storage.bucket`.
    pub bucket: String,
    /// `storage.region`.
    #[serde(default)]
    pub region: Option<String>,
    /// `storage.endpoint_url`: an S3-compatible service that is not AWS.
    #[serde(default)]
    pub endpoint_url: Option<String>,
    /// `storage.force_path_style`.
    #[serde(default)]
    pub force_path_style: bool,
    /// `storage.credentials_file`: AWS shared-credentials format; the default chain
    /// without it.
    #[serde(default)]
    pub credentials_file: Option<PathBuf>,
    /// `storage.link_ttl_seconds` (300).
    #[serde(default = "default_link_ttl")]
    pub link_ttl_seconds: u64,
}

fn default_link_ttl() -> u64 {
    300
}

/// `retention.*`. Read by the router (to label snapshots) and the lifecycle task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionSettings {
    /// `retention.days` (7): how long daily snapshots are kept.
    pub days: u32,
    /// `retention.soft_delete_days` (7): how long deletions can be undone.
    pub soft_delete_days: u32,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            days: 7,
            soft_delete_days: 7,
        }
    }
}

/// `keys.*`: where the master key lives on a server. The binary builds the provider.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum KeySettings {
    /// `keys.provider = "kms"` with `keys.kms_key_id`.
    Kms {
        /// The KMS key ARN.
        kms_key_id: String,
    },
    /// `keys.provider = "key_file"` with `keys.master_key_file` (one host).
    KeyFile {
        /// The key set file.
        master_key_file: PathBuf,
    },
}

/// `launcher.*` as the router reads it. The binary builds the `Launcher` from it.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LauncherSettings {
    /// AWS: `EcsLauncher`.
    Ecs(EcsSettings),
    /// One host: `DockerLauncher`, a client of the launcher service. Sets `one_host`.
    Docker(DockerSettings),
}

/// `launcher.ecs.*`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcsSettings {
    /// The cluster.
    pub cluster: String,
    /// The user task definition; also the router's own build spec.
    pub task_definition: String,
    /// The user subnets.
    pub subnets: Vec<String>,
    /// The user security groups.
    pub security_groups: Vec<String>,
    /// The capacity provider.
    pub capacity_provider: String,
}

/// `launcher.docker.*`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerSettings {
    /// `launcher.docker.url` (`http://launcher:7070`).
    #[serde(default = "default_launcher_url")]
    pub url: String,
    /// `launcher.docker.token`, shared with the launcher service.
    pub token: SecretText,
}

fn default_launcher_url() -> String {
    "http://launcher:7070".into()
}

/// The launcher service's settings (`knoggin launcher`, one host).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LauncherServiceSettings {
    /// `launcher.bind` (`0.0.0.0:7070`).
    #[serde(default = "default_launcher_bind")]
    pub bind: SocketAddr,
    /// `launcher.docker.token`.
    pub token: SecretText,
    /// `containers.user_net_cidr`: the range each container's /29 is cut from.
    #[serde(default = "default_user_net")]
    pub user_net_cidr: Cidr,
    /// `containers.memory_mib`: the memory limit of every user container.
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u64,
}

fn default_launcher_bind() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 7070))
}

fn default_user_net() -> Cidr {
    Cidr {
        network: IpAddr::from([10, 201, 0, 0]),
        prefix: 16,
    }
}

fn default_memory_mib() -> u64 {
    1536
}

/// `containers.*` as the router reads them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContainerSettings {
    /// `containers.user_net_cidr` (`10.201.0.0/16`): one host only; the internal
    /// listener's allowed peers.
    pub user_net_cidr: Cidr,
    /// `containers.memory_mib` (1,536): the limit the claim tells a container.
    pub memory_mib: u64,
    /// `containers.pool_target` (2): ready containers to keep.
    pub pool_target: u32,
    /// `containers.max` (0 means no bound; must be set on one host).
    pub max: u32,
    /// `containers.idle_seconds` (600).
    pub idle_seconds: u64,
    /// `containers.max_lifetime_hours` (24).
    pub max_lifetime_hours: u64,
    /// `containers.claim_wait_ms` (20,000).
    pub claim_wait_ms: u64,
    /// `containers.max_per_user` (6; 0 means no bound): the most containers one user may
    /// have at once, their own included. The rest serve their isolated and shared apps.
    pub max_per_user: u32,
}

impl ContainerSettings {
    /// `memory_mib` in bytes, as the claim sends it.
    pub fn memory_limit_bytes(&self) -> u64 {
        self.memory_mib.saturating_mul(1024 * 1024)
    }
}

impl Default for ContainerSettings {
    fn default() -> Self {
        Self {
            user_net_cidr: default_user_net(),
            memory_mib: default_memory_mib(),
            pool_target: 2,
            max: 0,
            idle_seconds: 600,
            max_lifetime_hours: 24,
            claim_wait_ms: 20_000,
            max_per_user: 6,
        }
    }
}

/// `internal.*`: the router's side of the link.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InternalSettings {
    /// `internal.bind` (`0.0.0.0:8443`).
    #[serde(default = "default_internal_bind")]
    pub bind: SocketAddr,
    /// `internal.router_url`: how containers reach the router on AWS. On one host the
    /// launcher always gives `https://router:8443` and this is ignored.
    #[serde(default)]
    pub router_url: Option<String>,
    /// `internal.secret`.
    pub secret: InternalSecret,
    /// `internal.cert`: the routers' shared certificate, PEM.
    pub cert: String,
    /// `internal.key`: its key, PEM.
    pub key: SecretText,
}

fn default_internal_bind() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8443))
}

/// `rollout.*`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RolloutSettings {
    /// `rollout.allow_downgrade`: lets an older build take over.
    pub allow_downgrade: bool,
}

/// A secret given as text (a token, a key's PEM): wiped on drop, never printed.
#[derive(Clone)]
pub struct SecretText(Zeroizing<String>);

impl SecretText {
    /// Wraps a value read from a file or the environment, without surrounding whitespace.
    pub fn new(text: &str) -> Self {
        Self(Zeroizing::new(text.trim().to_owned()))
    }

    /// The value, at the point of use.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<'de> Deserialize<'de> for SecretText {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = Zeroizing::new(String::deserialize(d)?);
        Ok(Self::new(&text))
    }
}

/// An address range such as `10.201.0.0/16` or `fd00::/8`. Addresses are compared in
/// canonical form, so an IPv4-mapped IPv6 address falls in the IPv4 range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

/// A text that is not an address range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not an address range like 10.0.0.0/8")]
pub struct CidrError;

impl Cidr {
    /// Whether `ip` lies in this range.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                masked(u128::from(u32::from(ip)), self.prefix, 32) == u128::from(u32::from(net))
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                masked(u128::from(ip), self.prefix, 128) == u128::from(net)
            }
            _ => false,
        }
    }

    /// Whether the two ranges share an address.
    pub fn overlaps(&self, other: &Cidr) -> bool {
        self.contains(other.network) || other.contains(self.network)
    }
}

fn masked(value: u128, prefix: u8, width: u8) -> u128 {
    if prefix == 0 {
        return 0;
    }
    let host_bits = u32::from(width - prefix);
    value >> host_bits << host_bits
}

impl FromStr for Cidr {
    type Err = CidrError;

    /// `<address>/<prefix>`, or a bare address as a range of one. The address must be the
    /// range's first, so a typo such as `10.201.3.0/16` is not silently widened.
    fn from_str(text: &str) -> Result<Self, CidrError> {
        let (addr, prefix) = match text.trim().split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (text.trim(), None),
        };
        let network = addr
            .parse::<IpAddr>()
            .map_err(|_| CidrError)?
            .to_canonical();
        let width = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
                p.parse::<u8>().map_err(|_| CidrError)?
            }
            Some(_) => return Err(CidrError),
            None => width,
        };
        if prefix > width {
            return Err(CidrError);
        }
        let base = match network {
            IpAddr::V4(v) => u128::from(u32::from(v)),
            IpAddr::V6(v) => u128::from(v),
        };
        if masked(base, prefix, width) != base {
            return Err(CidrError);
        }
        Ok(Self { network, prefix })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

impl<'de> Deserialize<'de> for Cidr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let net: Cidr = "10.201.0.0/16".parse().unwrap();
        assert!(net.contains("10.201.255.7".parse().unwrap()));
        assert!(net.contains("::ffff:10.201.0.3".parse().unwrap()));
        assert!(!net.contains("10.202.0.1".parse().unwrap()));
        assert!(!net.contains("::1".parse().unwrap()));
        let one: Cidr = "192.0.2.5".parse().unwrap();
        assert_eq!(one.to_string(), "192.0.2.5/32");
        assert!(one.contains("192.0.2.5".parse().unwrap()));
        assert!(!one.contains("192.0.2.6".parse().unwrap()));
        let v6: Cidr = "fd00::/8".parse().unwrap();
        assert!(v6.contains("fdaa::1".parse().unwrap()));
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains("8.8.8.8".parse().unwrap()));
        assert!(all.overlaps(&net));
        assert!(!net.overlaps(&"192.0.2.0/24".parse().unwrap()));
        assert!("10.0.0.0/8".parse::<Cidr>().unwrap().overlaps(&net));
        for bad in [
            "10.201.3.0/16",
            "10.0.0.0/33",
            "10.0.0.0/",
            "x/8",
            "10.0.0.0/+8",
            "",
        ] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad}");
        }
    }

    #[test]
    fn budgets() {
        // 1,536 MiB: (1536 - 928) MiB * 0.4 = 243.2 MiB.
        let budget = store_budget(1536 * 1024 * 1024);
        assert_eq!(budget, (608 * 1024 * 1024) / 5 * 2);
        assert_eq!(store_budget(100), 0);
        let settings = RouterSettings {
            trusted_proxies: vec![],
            image_digest: None,
            limits: RouterLimits::default(),
            storage: serde_json::from_value(serde_json::json!({"bucket": "b"})).unwrap(),
            retention: RetentionSettings::default(),
            containers: ContainerSettings::default(),
            internal: serde_json::from_value(serde_json::json!({
                "secret": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                "cert": "c", "key": "k\n"
            }))
            .unwrap(),
            rollout: RolloutSettings::default(),
            log_level: LogLevel::Info,
        };
        // About 171 GiB.
        assert_eq!(settings.upload_bytes_per_hour(), 720 * budget);
        assert_eq!(settings.storage.link_ttl_seconds, 300);
        assert_eq!(settings.internal.bind.port(), 8443);
        assert_eq!(settings.internal.key.expose(), "k");
        assert!(!format!("{settings:?}").contains("\"k\""));
    }

    #[test]
    fn tagged_choices() {
        let keys: KeySettings =
            serde_json::from_value(serde_json::json!({"provider": "kms", "kms_key_id": "arn"}))
                .unwrap();
        assert_eq!(
            keys,
            KeySettings::Kms {
                kms_key_id: "arn".into()
            }
        );
        let launcher: LauncherSettings =
            serde_json::from_value(serde_json::json!({"kind": "docker", "token": "t"})).unwrap();
        let LauncherSettings::Docker(docker) = launcher else {
            panic!("docker")
        };
        assert_eq!(docker.url, "http://launcher:7070");
        assert_eq!(format!("{:?}", docker.token), "[redacted]");
    }
}
