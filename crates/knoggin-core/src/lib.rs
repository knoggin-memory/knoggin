// SPDX-License-Identifier: AGPL-3.0-only
//! The Knoggin engine: one store of memories in memory, with search, decay and graph.
//!
//! Every operation is a plain, synchronous, deterministic function. The engine reads no
//! clock and no random source: the caller passes the current time and any new IDs.
