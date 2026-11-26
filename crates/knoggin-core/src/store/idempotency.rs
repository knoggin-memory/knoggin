// SPDX-License-Identifier: AGPL-3.0-only
//! Idempotent writes: `run_idempotent` and the input of the request hash.
//!
//! The engine does not hash: `knoggin-core` takes no cryptography crate (tab
//! "Workspace and crates"), and every SHA-256 in Knoggin comes from `aws-lc-rs` (tab
//! "Engineering rules"). [`request_hash_input`] builds the exact bytes, and the host
//! hashes them with `aws_lc_rs::digest::SHA256` before calling [`Store::run_idempotent`].

use serde::Serialize;
use serde_json::Value;

use crate::error::EngineError;
use crate::id::MemoryId;
use crate::model::limits::{IDEMPOTENCY_KEY_MAX_BYTES, IDEMPOTENCY_MAX_ENTRIES};
use crate::ops::Idempotent;
use crate::time::Timestamp;

use super::queue::is_expired;
use super::{IdempotencyEntry, Store};

/// The bytes whose SHA-256 is the request hash: the operation's name (the `Store`
/// method, `store_memory` for example), one zero byte, and the request as JSON with
/// the keys of every object sorted and no white space.
///
/// `request` is the resolved request value with its defaults filled in, so a call
/// that leaves a field out and one that sends its default give the same bytes.
pub fn request_hash_input<T: Serialize>(op: &str, request: &T) -> Result<Vec<u8>, EngineError> {
    let value = serde_json::to_value(request)
        .map_err(|_| EngineError::invalid("request", "the request cannot be written as JSON"))?;
    let mut out = Vec::with_capacity(op.len() + 64);
    out.extend_from_slice(op.as_bytes());
    out.push(0);
    write_canonical(&value, &mut out);
    Ok(out)
}

/// Writes `value` with sorted object keys, whatever order `serde_json` keeps (its
/// `preserve_order` feature changes that order).
fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push(b'{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(&Value::String(key.clone()), out);
                out.push(b':');
                write_canonical(item, out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        // A scalar has no keys to order; serde_json writes it compactly.
        scalar => out.extend_from_slice(scalar.to_string().as_bytes()),
    }
}

/// 1 to 128 bytes of printable ASCII.
fn check_key(key: &str) -> Result<(), EngineError> {
    let valid = (1..=IDEMPOTENCY_KEY_MAX_BYTES).contains(&key.len())
        && key.bytes().all(|b| (0x20..=0x7e).contains(&b));
    if valid {
        Ok(())
    } else {
        Err(EngineError::invalid(
            "idempotencyKey",
            format!(
                "the idempotency key must be 1 to {IDEMPOTENCY_KEY_MAX_BYTES} bytes of printable ASCII"
            ),
        ))
    }
}

impl Store {
    /// Runs `op` once per idempotency key. The same key with the same hash replays the
    /// stored JSON answer without running anything; with another hash it is
    /// `idempotency_conflict`. An entry older than 24 hours counts as absent. A failed
    /// operation is not recorded. `op` returns its answer and the IDs of the memories
    /// whose text that answer carries; forgetting one of them drops the entry.
    ///
    /// The host never calls this for a dry run.
    pub fn run_idempotent<T: Serialize>(
        &mut self,
        key: &str,
        request_hash: [u8; 32],
        now: Timestamp,
        op: impl FnOnce(&mut Store) -> Result<(T, Vec<MemoryId>), EngineError>,
    ) -> Result<Idempotent<T>, EngineError> {
        check_key(key)?;
        let mut expired = false;
        if let Some(entry) = self.idempotency.get(key) {
            if is_expired(entry.created_at, now) {
                expired = true;
            } else if entry.request_hash == request_hash {
                return Ok(Idempotent::Replayed(entry.response.clone()));
            } else {
                return Err(EngineError::IdempotencyConflict);
            }
        }
        // The expired entry goes only once the operation has succeeded, so that a
        // failed write leaves the store as it was.
        let (answer, text_ids) = op(self)?;
        if expired {
            self.remove_idempotency(key);
            self.changes.bookkeeping = self.changes.bookkeeping.saturating_add(1);
        }
        // An answer that cannot be written as JSON is returned but not kept: a retry
        // then runs again, which is what happens without a key.
        if let Ok(response) = serde_json::to_vec(&answer) {
            self.make_room_for_entry();
            self.insert_idempotency(
                key.to_owned(),
                IdempotencyEntry {
                    request_hash,
                    created_at: now,
                    response,
                    text_ids,
                },
            );
        }
        Ok(Idempotent::Ran(answer))
    }

    /// Drops the oldest entries until one more fits under the cap.
    fn make_room_for_entry(&mut self) {
        // ponytail: a linear scan per eviction; at 10,000 entries that is microseconds.
        while self.idempotency.len() >= IDEMPOTENCY_MAX_ENTRIES {
            let Some(oldest) = self
                .idempotency
                .iter()
                .min_by(|a, b| a.1.created_at.cmp(&b.1.created_at).then(a.0.cmp(b.0)))
                .map(|(k, _)| k.clone())
            else {
                return;
            };
            self.remove_idempotency(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ErrorCode;
    use crate::ops::{ReinforceMemory, StoreMemory};
    use crate::store::write::tests::{DAY, T0, ctx, mid, req, store, t};

    /// Pins the canonical bytes of one fixed request. Their SHA-256, the request hash,
    /// is `d2d25a61f99d2215c890a333bdc6a8afcce9a0e714bda2d4af5f6991123457f9`; the host's test pins that with aws-lc-rs.
    #[test]
    fn the_hash_input_of_a_fixed_request_is_pinned() {
        let req: StoreMemory = serde_json::from_value(json!({
            "summary": "Maya prefers window seats",
            "namespace": "default",
            "tags": ["travel"],
        }))
        .unwrap();
        let bytes = request_hash_input("store_memory", &req).unwrap();
        let expected = concat!(
            "store_memory\0",
            r#"{"checkDuplicates":false,"createdAt":null,"emotions":[],"entities":[],"#,
            r#""fullText":null,"namespace":"default","parentId":null,"#,
            r#""summary":"Maya prefers window seats","tags":["travel"],"topics":[]}"#
        );
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    }

    #[test]
    fn absent_and_default_fields_and_number_forms_hash_the_same() {
        let a: ReinforceMemory = serde_json::from_value(json!({})).unwrap();
        let b: ReinforceMemory = serde_json::from_value(json!({ "quality": 3 })).unwrap();
        let input = |r: &ReinforceMemory| request_hash_input("reinforce_memory", r).unwrap();
        assert_eq!(input(&a), input(&b));
        assert_ne!(
            request_hash_input("forget_memory", &b).unwrap(),
            input(&b),
            "the operation name is part of it"
        );
    }

    #[test]
    fn nested_keys_are_sorted_at_every_level() {
        let v = json!({ "b": [{ "z": 1, "a": null }], "a": { "y": "x", "c": true } });
        let mut out = Vec::new();
        write_canonical(&v, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"a":{"c":true,"y":"x"},"b":[{"a":null,"z":1}]}"#
        );
    }

    fn save_op(
        n: u16,
    ) -> impl FnOnce(&mut Store) -> Result<(crate::ops::StoredMemory, Vec<MemoryId>), EngineError>
    {
        move |s: &mut Store| {
            let r = s.store_memory(
                req(json!({ "summary": "alpha" })),
                mid(n),
                None,
                &ctx(),
                t(T0),
            )?;
            Ok((r, Vec::new()))
        }
    }

    #[test]
    fn same_hash_replays_and_another_conflicts() {
        let mut s = store();
        let first = s.run_idempotent("k1", [1; 32], t(T0), save_op(1)).unwrap();
        let Idempotent::Ran(stored) = first else {
            panic!("did not run")
        };
        let again = s
            .run_idempotent("k1", [1; 32], t(T0 + 5), save_op(2))
            .unwrap();
        assert_eq!(
            again,
            Idempotent::Replayed(serde_json::to_vec(&stored).unwrap())
        );
        assert!(s.memory(mid(2)).is_none(), "a replay runs nothing");
        let err = s
            .run_idempotent("k1", [2; 32], t(T0), save_op(3))
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::IdempotencyConflict);
        assert_eq!(s.memories().count(), 1);
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());
    }

    #[test]
    fn entries_expire_after_a_day_and_failures_are_not_recorded() {
        let mut s = store();
        s.run_idempotent("k", [1; 32], t(T0), save_op(1)).unwrap();
        let r = s
            .run_idempotent("k", [1; 32], t(T0 + DAY), save_op(2))
            .unwrap();
        assert!(
            matches!(r, Idempotent::Replayed(_)),
            "24 hours is still kept"
        );
        let r = s
            .run_idempotent("k", [2; 32], t(T0 + DAY + 1), save_op(2))
            .unwrap();
        assert!(
            matches!(r, Idempotent::Ran(_)),
            "an expired entry counts as absent"
        );
        assert_eq!(s.idempotency.get("k").unwrap().request_hash, [2; 32]);
        // A failure after an expired lookup leaves the expired entry and the counters.
        let changes = s.changes();
        let entry = s.idempotency.get("k").cloned();
        assert!(
            s.run_idempotent("k", [9; 32], t(T0 + 3 * DAY), save_op(1))
                .is_err()
        );
        assert_eq!(s.idempotency.get("k").cloned(), entry);
        assert_eq!(s.changes(), changes);
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());
        // A failure (the ID exists) is not recorded, so a retry runs again.
        assert!(s.run_idempotent("f", [3; 32], t(T0), save_op(1)).is_err());
        assert!(!s.idempotency.contains_key("f"));
        assert!(matches!(
            s.run_idempotent("f", [3; 32], t(T0), save_op(3)).unwrap(),
            Idempotent::Ran(_)
        ));
    }

    #[test]
    fn keys_are_checked() {
        let mut s = store();
        for key in ["", "caf\u{e9}", "tab\t", &"k".repeat(129)] {
            let err = s
                .run_idempotent(key, [0; 32], t(T0), save_op(1))
                .unwrap_err();
            assert_eq!(err.field(), Some("idempotencyKey"));
        }
        assert!(
            s.run_idempotent(&"k".repeat(128), [0; 32], t(T0), save_op(1))
                .is_ok()
        );
        assert!(
            s.run_idempotent("with space ~", [0; 32], t(T0), save_op(2))
                .is_ok()
        );
    }

    #[test]
    fn at_most_ten_thousand_entries_are_kept_and_the_oldest_go_first() {
        let mut s = store();
        let noop = |s: &mut Store| -> Result<((), Vec<MemoryId>), EngineError> {
            let _ = s;
            Ok(((), Vec::new()))
        };
        for i in 0..IDEMPOTENCY_MAX_ENTRIES {
            s.run_idempotent(&format!("k{i:05}"), [0; 32], t(T0 + i as i64), noop)
                .unwrap();
        }
        assert_eq!(s.idempotency.len(), IDEMPOTENCY_MAX_ENTRIES);
        s.run_idempotent("new", [0; 32], t(T0 + DAY), noop).unwrap();
        assert_eq!(s.idempotency.len(), IDEMPOTENCY_MAX_ENTRIES);
        assert!(!s.idempotency.contains_key("k00000") && s.idempotency.contains_key("k00001"));
        assert!(s.idempotency.contains_key("new"));
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());
    }

    #[test]
    fn a_duplicate_report_entry_is_purged_when_its_match_is_forgotten() {
        let mut s = store();
        let text = "maya likes window seats";
        let v = crate::testing::FakeProvider::new().vector(text);
        s.store_memory(
            req(json!({ "summary": text })),
            mid(1),
            Some(v.clone()),
            &ctx(),
            t(T0),
        )
        .unwrap();
        s.run_idempotent("dup", [1; 32], t(T0), |s: &mut Store| {
            let r = s.store_memory(
                req(json!({ "summary": text, "checkDuplicates": true })),
                mid(2),
                Some(v),
                &ctx(),
                t(T0),
            )?;
            let ids = r
                .near_duplicates
                .iter()
                .flat_map(|d| d.matches.iter().map(|m| m.id))
                .collect();
            Ok((r, ids))
        })
        .unwrap();
        s.run_idempotent("plain", [2; 32], t(T0), save_op(3))
            .unwrap();
        assert_eq!(s.idempotency.get("dup").unwrap().text_ids, [mid(1)]);
        s.forget_memory(
            crate::ops::ForgetMemory {
                id: mid(1),
                namespace: None,
            },
            &ctx(),
            t(T0),
        )
        .unwrap();
        assert!(!s.idempotency.contains_key("dup"));
        assert!(s.idempotency.contains_key("plain"));
    }
}
