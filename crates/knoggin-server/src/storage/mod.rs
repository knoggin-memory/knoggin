// SPDX-License-Identifier: AGPL-3.0-only
//! Object storage for snapshots (tab "Workspace and crates"): `ObjectStorage`, with
//! `S3Storage` for any S3-compatible service and `MemoryStorage`, the fast tests' stand-in
//! that serves real signed links on a loopback port. The router and the admin commands
//! never read or write an object directly: they hand out signed requests, and whoever
//! moves the bytes sends them as given. Only the lifecycle task deletes.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use knoggin_core::Timestamp;

pub mod memory;
pub mod names;
mod s3;

pub use memory::{Fault, MemoryStorage};
pub use s3::S3Storage;

/// The bucket.
#[async_trait]
pub trait ObjectStorage: Send + Sync {
    /// Every object whose key starts with `prefix`: key, size, last modified.
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>, StorageError>;
    /// A download link for `key`. `now` is the signing time: callers pass the current
    /// time; the bucket self-test passes an earlier one.
    async fn presign_get(
        &self,
        key: &str,
        ttl: Duration,
        now: Timestamp,
    ) -> Result<SignedRequest, StorageError>;
    /// A create-only upload link for exactly `len` bytes: `Content-Length` and
    /// `If-None-Match: *` are signed, so the bucket refuses another length and an existing
    /// name (412).
    async fn presign_put_new(
        &self,
        key: &str,
        len: u64,
        ttl: Duration,
        now: Timestamp,
    ) -> Result<SignedRequest, StorageError>;
    /// Deletes these objects; a key that is already gone is no error. The lifecycle task
    /// alone calls it.
    async fn delete(&self, keys: &[String]) -> Result<(), StorageError>;
}

/// One object of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Its key.
    pub key: String,
    /// Its size in bytes.
    pub size: u64,
    /// When the bucket last wrote it.
    pub last_modified: Timestamp,
}

/// A request signed for the bucket: send it exactly as given, with the method, the URL
/// and every header. The URL is a secret until it expires and is never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedRequest {
    /// `GET` or `PUT`.
    pub method: String,
    /// The whole URL, signature in its query.
    pub url: String,
    /// Headers that are signed and must be sent as they are (`Content-Length`,
    /// `If-None-Match` and the like). `Host` is not among them: it follows from the URL.
    pub headers: Vec<(String, String)>,
    /// When the link stops working, in milliseconds since the epoch.
    pub expires_ms: i64,
}

impl fmt::Debug for SignedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedRequest")
            .field("method", &self.method)
            .field("headers", &self.headers.len())
            .field("expires_ms", &self.expires_ms)
            .finish_non_exhaustive()
    }
}

/// What can go wrong with the bucket. No variant carries a URL, a key or a credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// The bucket cannot be reached or failed: 503 to whoever waits.
    #[error("the bucket cannot be reached")]
    Unavailable,
    /// The bucket refused the call; the service's error code when it gave one.
    #[error("the bucket refused the call ({0})")]
    Refused(String),
    /// A key, a length or a lifetime the bucket cannot sign.
    #[error("cannot sign this request: {0}")]
    Invalid(&'static str),
}
