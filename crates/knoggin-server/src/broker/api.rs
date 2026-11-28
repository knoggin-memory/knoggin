// SPDX-License-Identifier: AGPL-3.0-only
//! The bodies of the broker's routes, shared by the broker and
//! the container's `RouterBackend` so that the two ends cannot drift, and the broker's
//! error codes.

use std::collections::BTreeMap;
use std::fmt;

use knoggin_http::error::Code;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

/// Paths of the broker's routes.
pub mod paths {
    use knoggin_core::StoreId;

    /// `POST` [`super::OpenRequest`], answers [`super::OpenAnswer`].
    pub const OPEN: &str = "/internal/v1/stores/{store_id}/open";
    /// `POST` [`super::SnapshotsRequest`], answers [`super::UploadLink`].
    pub const SNAPSHOTS: &str = "/internal/v1/stores/{store_id}/snapshots";
    /// `POST` [`super::CommitRequest`].
    pub const COMMIT: &str = "/internal/v1/stores/{store_id}/snapshots/{sequence}/commit";
    /// `POST`, no body: the container has saved and leaves.
    pub const PARKED: &str = "/internal/v1/parked";
    /// `GET` and `PUT` `<prefix><object key>`, one host only.
    pub const BLOB_PREFIX: &str = "/internal/v1/blob/";

    /// The `open` path of one store.
    pub fn open(store: StoreId) -> String {
        format!("/internal/v1/stores/{store}/open")
    }

    /// The `snapshots` path of one store.
    pub fn snapshots(store: StoreId) -> String {
        format!("/internal/v1/stores/{store}/snapshots")
    }

    /// The `commit` path of one snapshot.
    pub fn commit(store: StoreId, sequence: u64) -> String {
        format!("/internal/v1/stores/{store}/snapshots/{sequence}/commit")
    }
}

/// A generation and a sequence: a snapshot name in JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameParts {
    /// The lease generation.
    pub generation: u64,
    /// The sequence within it.
    pub sequence: u64,
}

/// The body of `open`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRequest {
    /// The newest snapshot below this name, because this one could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<NameParts>,
}

/// A data key as base64url text, wiped on drop and never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyText(pub Zeroizing<String>);

impl fmt::Debug for KeyText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl Serialize for KeyText {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for KeyText {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(Zeroizing::new(String::deserialize(d)?)))
    }
}

/// The answer of `open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAnswer {
    /// The lease's generation: every snapshot of this open carries it.
    pub generation: u64,
    /// One more than the highest sequence already stored under that generation, or 1.
    pub next_sequence: u64,
    /// The store's data key, base64url without padding.
    pub data_key: KeyText,
    /// The wrapped data key, base64url without padding.
    pub wrapped_key: String,
    /// The master key that wrapped it.
    pub kek_id: String,
    /// The snapshot is the one an admin chose to restore.
    pub restore: bool,
    /// The snapshot to load; `None` for a new store.
    pub snapshot: Option<SnapshotLink>,
}

/// A snapshot to download and its signed link.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotLink {
    /// Its generation.
    pub generation: u64,
    /// Its sequence.
    pub sequence: u64,
    /// Its size in the bucket.
    pub size_bytes: u64,
    /// The signed address. Secret until it expires.
    pub url: String,
    /// Headers to send with it.
    pub headers: BTreeMap<String, String>,
}

impl fmt::Debug for SnapshotLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotLink")
            .field("generation", &self.generation)
            .field("sequence", &self.sequence)
            .field("size_bytes", &self.size_bytes)
            .finish_non_exhaustive()
    }
}

/// The body of `snapshots`: ask for a create-only upload link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotsRequest {
    /// The sequence; the generation is the lease's.
    pub sequence: u64,
    /// The exact length of the sealed file.
    pub size_bytes: u64,
}

/// The answer of `snapshots`: send it exactly as given.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadLink {
    /// `PUT`.
    pub method: String,
    /// The signed address. Secret until it expires.
    pub url: String,
    /// Signed headers, `Content-Length` and `If-None-Match: *` among them.
    pub headers: BTreeMap<String, String>,
    /// RFC 3339.
    pub expires_at: String,
}

impl fmt::Debug for UploadLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadLink")
            .field("method", &self.method)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// The body of `commit`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommitRequest {
    /// Memories in every phase.
    pub memory_count: u64,
    /// Namespaces.
    pub namespace_count: u64,
    /// RFC 3339; only when the store was written since this open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_write_at: Option<String>,
    /// The client of that write, at most 64 characters. Sensitive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_write_client: Option<String>,
    /// The file's length.
    pub size_bytes: u64,
    /// `<generation>-<sequence>` of the restored snapshot, until a commit succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored: Option<String>,
}

impl fmt::Debug for CommitRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommitRequest")
            .field("memory_count", &self.memory_count)
            .field("namespace_count", &self.namespace_count)
            .field("size_bytes", &self.size_bytes)
            .field("restored", &self.restored)
            .finish_non_exhaustive()
    }
}

/// The code of a broker error: the REST codes plus `lease_lost`, which exists only on
/// this link and never reaches a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerCode {
    /// A code of the REST API.
    Api(Code),
    /// The caller does not hold the lease: it stops at once and exits without writing.
    LeaseLost,
}

impl BrokerCode {
    /// The code as written on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api(code) => code.as_str(),
            Self::LeaseLost => "lease_lost",
        }
    }
}

/// The reason of a 429 when the hourly upload budget is used up.
pub const UPLOAD_BUDGET: &str = "upload_budget";

/// The error body of a broker answer, as the container reads it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ErrorEnvelope {
    /// The error.
    pub error: ErrorBody,
}

/// The fields of a broker error the container acts on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ErrorBody {
    /// The code.
    pub code: String,
    /// The reason, on `store_locked` and on `rate_limited` for the upload budget.
    #[serde(default)]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names_are_the_specified_ones() {
        let answer = OpenAnswer {
            generation: 3,
            next_sequence: 1,
            data_key: KeyText(Zeroizing::new("k".into())),
            wrapped_key: "w".into(),
            kek_id: "local:1".into(),
            restore: false,
            snapshot: Some(SnapshotLink {
                generation: 2,
                sequence: 5,
                size_bytes: 10,
                url: "https://x".into(),
                headers: BTreeMap::new(),
            }),
        };
        let json = serde_json::to_value(&answer).unwrap();
        assert_eq!(json["nextSequence"], 1);
        assert_eq!(json["dataKey"], "k");
        assert_eq!(json["wrappedKey"], "w");
        assert_eq!(json["kekId"], "local:1");
        assert_eq!(json["snapshot"]["sizeBytes"], 10);
        assert_eq!(serde_json::from_value::<OpenAnswer>(json).unwrap(), answer);
        assert!(!format!("{answer:?}").contains("https://x"));
        let commit: CommitRequest = serde_json::from_str(
            r#"{"memoryCount":1,"namespaceCount":2,"lastWriteAt":"2026-11-23T10:15:30.123Z",
                "lastWriteClient":"c","sizeBytes":3,"restored":"0000000001-0000000002"}"#,
        )
        .unwrap();
        assert_eq!(commit.restored.as_deref(), Some("0000000001-0000000002"));
        assert!(serde_json::from_str::<SnapshotsRequest>(r#"{"sequence":1}"#).is_err());
        assert_eq!(
            serde_json::to_value(OpenRequest {
                after: Some(NameParts {
                    generation: 1,
                    sequence: 2
                })
            })
            .unwrap(),
            serde_json::json!({"after": {"generation": 1, "sequence": 2}})
        );
        assert_eq!(BrokerCode::LeaseLost.as_str(), "lease_lost");
        assert_eq!(
            BrokerCode::Api(Code::StoreDeleting).as_str(),
            "store_deleting"
        );
    }
}
