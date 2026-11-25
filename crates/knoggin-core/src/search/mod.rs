// SPDX-License-Identifier: AGPL-3.0-only
//! Search: the vector scan, the full-text index, the entity index with entity
//! normalization, and the tag index. The recall pipeline and scoring go here too.

pub mod entity;
pub mod fts;
mod pipeline;
pub mod tags;
pub mod vector;
