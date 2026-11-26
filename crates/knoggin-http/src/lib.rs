// SPDX-License-Identifier: AGPL-3.0-only
//! Everything Knoggin serves over HTTP, in two layers: the edge layer checks credentials
//! and builds a `RequestContext`; the store layer ([`host`], [`store_api`]) holds the open
//! stores and runs the memory operations. See `CONTRACT.md` for what each module offers.

pub mod ctx;
pub mod edge;
pub mod error;
pub mod host;
pub mod local;
pub mod mcp;
pub mod panic;
pub mod settings;
pub mod store_api;
pub mod testing;
pub mod trace;

pub use ctx::{CredentialKind, CredentialRef, Permissions, RequestContext};
pub use edge::edge_layer;
pub use error::{ApiError, Code, LockReason};
pub use host::{StoreBackend, StoreHost};
pub use store_api::store_layer;
