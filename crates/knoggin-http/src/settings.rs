// SPDX-License-Identifier: AGPL-3.0-only
//! The settings that the store layer reads. The binary fills them from the file and the
//! environment; the edge layer adds its own here.

use serde::Deserialize;

/// The `limits` settings the store layer uses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct StoreLimits {
    /// `limits.body_bytes`: the body limit of `POST /v1/memories/batch` (and the MCP routes).
    pub body_bytes: usize,
    /// `limits.request_timeout_ms`: the request time limit; export and import are exempt.
    pub request_timeout_ms: u64,
    /// `limits.store_concurrency`: concurrent requests per open store.
    pub store_concurrency: usize,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            body_bytes: 10_485_760,
            request_timeout_ms: 30_000,
            store_concurrency: 64,
        }
    }
}

/// The body limit of every route but the batch save, the MCP routes and the import.
pub const DEFAULT_BODY_BYTES: usize = 256 * 1024;

/// The longest line of an import.
pub const IMPORT_LINE_BYTES: usize = 4 * 1024 * 1024;
