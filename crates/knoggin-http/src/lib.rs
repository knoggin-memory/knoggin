// SPDX-License-Identifier: AGPL-3.0-only
//! Everything Knoggin serves over HTTP, in two layers: the edge layer checks credentials
//! and builds a `RequestContext`; the store layer holds the open stores and runs the
//! memory operations.

pub mod ctx;
pub mod error;
pub mod panic;
pub mod settings;
pub mod trace;

pub use ctx::{CredentialKind, CredentialRef, Permissions, RequestContext};
pub use error::{ApiError, Code, LockReason};
