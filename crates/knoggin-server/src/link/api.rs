// SPDX-License-Identifier: AGPL-3.0-only
//! The bodies of the container's routes that a router calls (tab "Server", routes on the
//! container): `hello`, `claim`, `park` and `status`, and the route paths. The router
//! builds them, the container answers them; both sides use these types so that they
//! agree. The broker's bodies belong to the broker.

use knoggin_core::ops::ModelState;
use knoggin_core::{StoreId, UserId};
use knoggin_http::error::LockReason;
use knoggin_state::record::ParkReason;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The versions of the internal protocol this build speaks: the current one and, once
/// there is one, the one before.
pub const PROTOCOLS: &[u32] = &[1];

/// The highest version both sides speak, or `None`: a router stops such a container.
pub fn shared_protocol(theirs: &[u32]) -> Option<u32> {
    PROTOCOLS
        .iter()
        .copied()
        .filter(|v| theirs.contains(v))
        .max()
}

/// Paths of the container's internal routes.
pub mod paths {
    /// `GET`, with `?nonce=<64 hex>`; no `Authorization`.
    pub const HELLO: &str = "/internal/v1/hello";
    /// `POST` [`super::ClaimRequest`].
    pub const CLAIM: &str = "/internal/v1/claim";
    /// `POST` [`super::ParkRequest`]; answers 202.
    pub const PARK: &str = "/internal/v1/park";
    /// `GET`, optionally `?store=<store id>`; answers [`super::StatusAnswer`].
    pub const STATUS: &str = "/internal/v1/status";
    /// The value of `Knoggin-Internal-Status` on the 503 of a parking container.
    pub const PARKING: &str = "parking";
}

/// The answer of `hello`. The router derives the launch secret from the container ID in
/// its own record, refuses an answer whose `container_id` differs, and checks `proof`
/// against the certificate of the same connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloAnswer {
    /// The container's ID.
    pub container_id: Uuid,
    /// The release version of its build.
    pub build: String,
    /// The protocol versions it speaks.
    pub protocols: Vec<u32>,
    /// The model state.
    pub model: ModelState,
    /// Whether it runs in test mode; a router stops a container whose mode differs.
    pub test_mode: bool,
    /// Lower-case hex HMAC over `knoggin-hello-v1`, the nonce and the certificate's SHA-256.
    pub proof: String,
}

/// The body of `claim`. From then on the container serves this user only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimRequest {
    /// The user.
    pub user_id: UserId,
    /// The lease generation; every snapshot name carries it.
    pub generation: u64,
    /// What the container would otherwise read from a config file.
    pub settings: ClaimSettings,
}

/// The settings a container gets in its claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimSettings {
    /// `containers.idle_seconds`.
    pub idle_seconds: u64,
    /// `containers.memory_mib` in bytes; the memory budget follows from it.
    pub memory_limit_bytes: u64,
    /// `server.public_url`.
    pub public_url: String,
    /// `source_url`.
    pub source_url: String,
    /// The store layer's limits.
    pub limits: ClaimLimits,
}

/// The store layer's limits in a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimLimits {
    /// `limits.body_bytes`.
    pub body_bytes: u64,
    /// `limits.request_timeout_ms`.
    pub request_timeout_ms: u64,
    /// `limits.store_concurrency`.
    pub store_concurrency: u64,
}

/// The body of `park`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParkRequest {
    /// Why.
    pub reason: ParkReason,
}

/// Where a container stands, in its `status` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerPhase {
    /// In the pool: no user.
    Ready,
    /// Serves its user.
    Claimed,
    /// Running the park sequence.
    Parking,
}

/// The answer of `status`. Reading it never counts as activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusAnswer {
    /// Ready, claimed or parking.
    pub state: ContainerPhase,
    /// The user it was claimed for; `None` in the pool.
    pub user_id: Option<UserId>,
    /// The claim's generation; `None` in the pool.
    pub generation: Option<u64>,
    /// The release version of its build.
    pub build: String,
    /// Whole seconds since a request that counts as activity last started or finished;
    /// 0 while one is in flight.
    pub idle_seconds: u64,
    /// The model state.
    pub model: ModelState,
    /// The open stores.
    pub stores: Vec<StoreReport>,
    /// Stores being loaded.
    pub opening: Vec<StoreId>,
    /// Stores that failed to open.
    pub locks: Vec<StoreLock>,
    /// Snapshot writes that failed since the process started.
    pub snapshot_failures: u64,
    /// Running totals since the process started.
    pub counters: Counters,
}

/// One open store in a `status` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreReport {
    /// The store.
    pub store_id: StoreId,
    /// Memories in every phase.
    pub memory_count: u64,
    /// The engine's size estimate.
    pub estimated_bytes: u64,
    /// Has changes no snapshot holds yet.
    pub unsaved: bool,
    /// The sequence of the last snapshot written in this open.
    pub last_sequence: u64,
    /// Memories waiting to be embedded.
    pub pending_embeddings: u64,
    /// Memories that can hold a vector.
    pub total_embeddable: u64,
    /// Refused a write for the budget, or served for reading only.
    pub full: bool,
}

/// One store that failed to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreLock {
    /// The store.
    pub store_id: StoreId,
    /// Why.
    pub reason: LockReason,
}

/// Running totals of one container. The router adds them up with saturating arithmetic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counters {
    /// Store requests.
    pub requests: u64,
    /// Texts embedded.
    pub embed_count: u64,
    /// Time spent embedding.
    pub embed_millis: u64,
    /// Snapshots written.
    pub snapshot_count: u64,
    /// Time spent writing them.
    pub snapshot_millis: u64,
    /// Bytes written.
    pub snapshot_bytes: u64,
}

impl Counters {
    /// `self + other`, each field saturating.
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            requests: self.requests.saturating_add(other.requests),
            embed_count: self.embed_count.saturating_add(other.embed_count),
            embed_millis: self.embed_millis.saturating_add(other.embed_millis),
            snapshot_count: self.snapshot_count.saturating_add(other.snapshot_count),
            snapshot_millis: self.snapshot_millis.saturating_add(other.snapshot_millis),
            snapshot_bytes: self.snapshot_bytes.saturating_add(other.snapshot_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use knoggin_core::ops::ModelStatus;

    use super::*;

    fn model() -> ModelState {
        ModelState {
            state: ModelStatus::Ready,
            identity: "fake-64".into(),
            name: "Fake".into(),
            bytes_done: None,
            bytes_total: None,
            reason: None,
        }
    }

    #[test]
    fn field_names_are_the_specified_ones() {
        let claim = ClaimRequest {
            user_id: UserId::from_uuid(Uuid::nil()),
            generation: 7,
            settings: ClaimSettings {
                idle_seconds: 600,
                memory_limit_bytes: 1 << 30,
                public_url: "https://m.example".into(),
                source_url: "https://src.example".into(),
                limits: ClaimLimits {
                    body_bytes: 10,
                    request_timeout_ms: 20,
                    store_concurrency: 30,
                },
            },
        };
        let json = serde_json::to_value(&claim).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "userId": "00000000-0000-0000-0000-000000000000",
                "generation": 7,
                "settings": {
                    "idleSeconds": 600, "memoryLimitBytes": 1_073_741_824u64,
                    "publicUrl": "https://m.example", "sourceUrl": "https://src.example",
                    "limits": {"bodyBytes": 10, "requestTimeoutMs": 20, "storeConcurrency": 30}
                }
            })
        );
        assert_eq!(serde_json::from_value::<ClaimRequest>(json).unwrap(), claim);
        let park: ParkRequest = serde_json::from_str(r#"{"reason":"restore"}"#).unwrap();
        assert_eq!(park.reason, ParkReason::Restore);
        let status = StatusAnswer {
            state: ContainerPhase::Claimed,
            user_id: None,
            generation: Some(3),
            build: "0.1.0".into(),
            idle_seconds: 0,
            model: model(),
            stores: vec![],
            opening: vec![],
            locks: vec![StoreLock {
                store_id: StoreId::from_uuid(Uuid::nil()),
                reason: LockReason::OutOfMemory,
            }],
            snapshot_failures: 1,
            counters: Counters::default(),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["state"], "claimed");
        assert_eq!(json["locks"][0]["reason"], "out_of_memory");
        assert_eq!(json["model"]["state"], "ready");
        assert!(json["counters"].get("snapshotBytes").is_some());
        assert_eq!(
            serde_json::from_value::<StatusAnswer>(json).unwrap(),
            status
        );
        let hello = HelloAnswer {
            container_id: Uuid::nil(),
            build: "0.1.0".into(),
            protocols: vec![1],
            model: model(),
            test_mode: true,
            proof: "ab".into(),
        };
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(json["containerId"], "00000000-0000-0000-0000-000000000000");
        assert_eq!(json["testMode"], true);
        assert_eq!(serde_json::from_value::<HelloAnswer>(json).unwrap(), hello);
    }

    #[test]
    fn protocols_and_counters() {
        assert_eq!(shared_protocol(&[1, 2]), Some(1));
        assert_eq!(shared_protocol(&[2]), None);
        let max = Counters {
            requests: u64::MAX,
            ..Counters::default()
        };
        assert_eq!(max.saturating_add(max).requests, u64::MAX);
    }
}
