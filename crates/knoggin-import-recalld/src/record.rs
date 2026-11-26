// SPDX-License-Identifier: AGPL-3.0-only
//! recalld's `DiskRecord` (the value of the table `memories` in `meta.db`), ported from
//! recalld's `src/model/record.rs` and `RecordSchemaV2` in `src/model/schema.rs` at the
//! reference commit. Both record versions decode, as recalld's `from_bytes` does.

use serde::Deserialize;

/// The fixed part of a version-1 record, version byte included.
const V1_FIXED_SIZE: usize = 73;

/// recalld's phase byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecalldPhase {
    Full,
    Summary,
    Ghost,
    /// A forgotten memory.
    Tombstone,
}

/// recalld's access kinds 1 to 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecalldAccess {
    Direct,
    Associative,
    DecaySweep,
    Reinforcement,
}

/// One decoded record. Only the fields the import maps are kept.
#[derive(Clone, PartialEq)]
pub(crate) struct DiskRecord {
    pub id: [u8; 16],
    pub namespace_id: u32,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub phase: RecalldPhase,
    pub stability: f32,
    pub difficulty: f32,
    pub is_permastore: bool,
    pub summary: String,
    pub tags: Vec<String>,
    pub access_history: Vec<(i64, RecalldAccess)>,
    pub text_offset: u64,
    pub text_length: u32,
    /// `None` for a version-1 record.
    pub schema: Option<RecordSchema>,
}

impl std::fmt::Debug for DiskRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskRecord")
            .field("phase", &self.phase)
            .field("schema", &self.schema.is_some())
            .finish_non_exhaustive()
    }
}

/// The part of `RecordSchemaV2` that the import reads. Every other field is ignored.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordSchema {
    pub lifecycle: Lifecycle,
    pub labels: SchemaLabels,
    #[serde(default)]
    pub required_features: Vec<String>,
}

/// `LifecycleFields`.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Lifecycle {
    #[serde(default)]
    pub retrieved_at: Option<i64>,
    #[serde(default)]
    pub reinforced_at: Option<i64>,
    pub decay_anchor_at: i64,
    /// `legacy_access_v1` or `explicit_feedback_v2`.
    pub lifecycle_version: String,
    pub phase_changed_at: i64,
}

/// `StructuredLabels`, without the provenance.
#[derive(Clone, PartialEq, Deserialize)]
pub(crate) struct SchemaLabels {
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub emotions: Vec<String>,
}

/// Why one record does not decode. The text names the field, never a value.
pub(crate) type DecodeError = &'static str;

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.data.len())
            .ok_or("truncated record")?;
        let bytes = self.data.get(self.pos..end).ok_or("truncated record")?;
        self.pos = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0_u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn f32(&mut self) -> Result<f32, DecodeError> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    fn string(&mut self) -> Result<String, DecodeError> {
        let len = usize::from(self.u16()?);
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| "a string is not UTF-8")
    }
}

impl DiskRecord {
    /// Decodes version 1 and version 2, as recalld's `DiskRecord::from_bytes`.
    pub(crate) fn from_bytes(data: &[u8]) -> Result<Self, DecodeError> {
        match data.first() {
            Some(1) => decode_v1(data),
            Some(2) => decode_v2(data),
            Some(_) => Err("unknown record version"),
            None => Err("empty record"),
        }
    }
}

fn decode_v2(data: &[u8]) -> Result<DiskRecord, DecodeError> {
    let mut r = Reader { data, pos: 1 };
    let base_len = usize::try_from(r.u32()?).map_err(|_| "truncated record")?;
    let base = r.take(base_len)?;
    if base.first() != Some(&1) {
        return Err("the base of a version-2 record is not version 1");
    }
    let mut record = decode_v1(base)?;
    let schema_len = usize::try_from(r.u32()?).map_err(|_| "truncated record")?;
    let schema: RecordSchema = serde_json::from_slice(r.take(schema_len)?)
        .map_err(|_| "the record schema is not valid")?;
    if r.pos != data.len() {
        return Err("trailing bytes after the record schema");
    }
    if !schema.required_features.is_empty() {
        return Err("the record needs a feature this importer does not know");
    }
    record.schema = Some(schema);
    Ok(record)
}

fn decode_v1(data: &[u8]) -> Result<DiskRecord, DecodeError> {
    if data.len() < V1_FIXED_SIZE {
        return Err("truncated record");
    }
    let mut r = Reader { data, pos: 1 };
    let id = r.array::<16>()?;
    let namespace_id = r.u32()?;
    let created_at = r.i64()?;
    let last_accessed_at = r.i64()?;
    let phase = match r.u8()? {
        1 => RecalldPhase::Full,
        2 => RecalldPhase::Summary,
        3 => RecalldPhase::Ghost,
        4 => RecalldPhase::Tombstone,
        _ => return Err("unknown phase"),
    };
    let _strength = r.f32()?;
    let _decay_strength = r.f32()?;
    let stability = r.f32()?;
    let difficulty = r.f32()?;
    let is_permastore = match r.u8()? {
        0 => false,
        1 => true,
        _ => return Err("is_permastore is neither 0 nor 1"),
    };
    let _vector_slot = r.u32()?;
    let _edge_count = r.u16()?;
    let text_offset = r.u64()?;
    let text_length = r.u32()?;
    let summary = r.string()?;
    let tag_count = r.u16()?;
    let mut tags = Vec::with_capacity(usize::from(tag_count));
    for _ in 0..tag_count {
        tags.push(r.string()?);
    }
    let access_count = r.u16()?;
    let mut access_history = Vec::with_capacity(usize::from(access_count));
    for _ in 0..access_count {
        let at = r.i64()?;
        let kind = match r.u8()? {
            1 => RecalldAccess::Direct,
            2 => RecalldAccess::Associative,
            3 => RecalldAccess::DecaySweep,
            4 => RecalldAccess::Reinforcement,
            _ => return Err("unknown access kind"),
        };
        access_history.push((at, kind));
    }
    if r.pos != data.len() {
        return Err("trailing record bytes");
    }
    if (text_offset == 0) != (text_length == 0) {
        return Err("text offset and length disagree");
    }
    Ok(DiskRecord {
        id,
        namespace_id,
        created_at,
        last_accessed_at,
        phase,
        stability,
        difficulty,
        is_permastore,
        summary,
        tags,
        access_history,
        text_offset,
        text_length,
        schema: None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// recalld's `to_bytes` for version 1, as in `src/model/record.rs`.
    pub(crate) fn v1_bytes(r: &DiskRecord, phase: u8, history: &[(i64, u8)]) -> Vec<u8> {
        let mut b = vec![1];
        b.extend_from_slice(&r.id);
        b.extend_from_slice(&r.namespace_id.to_le_bytes());
        b.extend_from_slice(&r.created_at.to_le_bytes());
        b.extend_from_slice(&r.last_accessed_at.to_le_bytes());
        b.push(phase);
        b.extend_from_slice(&(-0.0_f32).to_le_bytes());
        b.extend_from_slice(&0.375_f32.to_le_bytes());
        b.extend_from_slice(&r.stability.to_le_bytes());
        b.extend_from_slice(&r.difficulty.to_le_bytes());
        b.push(u8::from(r.is_permastore));
        b.extend_from_slice(&0_u32.to_le_bytes());
        b.extend_from_slice(&0_u16.to_le_bytes());
        b.extend_from_slice(&r.text_offset.to_le_bytes());
        b.extend_from_slice(&r.text_length.to_le_bytes());
        b.extend_from_slice(&(r.summary.len() as u16).to_le_bytes());
        b.extend_from_slice(r.summary.as_bytes());
        b.extend_from_slice(&(r.tags.len() as u16).to_le_bytes());
        for t in &r.tags {
            b.extend_from_slice(&(t.len() as u16).to_le_bytes());
            b.extend_from_slice(t.as_bytes());
        }
        b.extend_from_slice(&(history.len() as u16).to_le_bytes());
        for (at, kind) in history {
            b.extend_from_slice(&at.to_le_bytes());
            b.push(*kind);
        }
        b
    }

    /// recalld's `to_bytes` for version 2: the version-1 bytes, then the schema JSON.
    pub(crate) fn v2_bytes(v1: &[u8], schema: &serde_json::Value) -> Vec<u8> {
        let extra = serde_json::to_vec(schema).unwrap();
        let mut b = vec![2];
        b.extend_from_slice(&(v1.len() as u32).to_le_bytes());
        b.extend_from_slice(v1);
        b.extend_from_slice(&(extra.len() as u32).to_le_bytes());
        b.extend_from_slice(&extra);
        b
    }

    /// The record of recalld's `schema.rs` tests (`fn record()`).
    pub(crate) fn sample() -> DiskRecord {
        DiskRecord {
            id: [7; 16],
            namespace_id: 1,
            created_at: 100,
            last_accessed_at: 400,
            phase: RecalldPhase::Full,
            stability: 3.7145,
            difficulty: 5.0,
            is_permastore: false,
            summary: "exact résumé".into(),
            tags: vec!["entity/john-smith".into()],
            access_history: vec![
                (200, RecalldAccess::Direct),
                (300, RecalldAccess::Reinforcement),
            ],
            text_offset: 0,
            text_length: 0,
            schema: None,
        }
    }

    pub(crate) fn sample_schema() -> serde_json::Value {
        serde_json::json!({
            "lifecycle": {
                "observedAt": 100, "retrievedAt": 200, "reinforcedAt": 300,
                "decayAnchorAt": 400, "lifecycleVersion": "legacy_access_v1",
                "lifecycleUpdatedAt": 400, "timestampOrigin": "legacy_inferred",
                "phaseChangedAt": 500, "policyRevision": "0", "recordGeneration": "0"
            },
            "labels": {
                "entities": ["John Smith", "José"], "topics": ["日本語"],
                "emotions": ["mixed feelings"], "provenance": "native_v2"
            },
            "vector": {"slot": 0, "allocationEpoch": "0", "sha256": ([7_u8; 32].to_vec())},
            "requiredFeatures": [],
            "extensions": {"future.optional.v7": {"exact": "Keep Case"}}
        })
    }

    #[test]
    fn decodes_version_1_with_every_field() {
        let want = sample();
        let bytes = v1_bytes(&want, 1, &[(200, 1), (300, 4)]);
        assert_eq!(DiskRecord::from_bytes(&bytes).unwrap(), want);
    }

    #[test]
    fn decodes_version_2_with_its_schema() {
        let base = sample();
        let bytes = v2_bytes(&v1_bytes(&base, 1, &[(200, 1), (300, 4)]), &sample_schema());
        let got = DiskRecord::from_bytes(&bytes).unwrap();
        let schema = got.schema.as_ref().unwrap();
        assert_eq!(schema.labels.entities, ["John Smith", "José"]);
        assert_eq!(schema.labels.topics, ["日本語"]);
        assert_eq!(schema.lifecycle.decay_anchor_at, 400);
        assert_eq!(schema.lifecycle.phase_changed_at, 500);
        assert_eq!(schema.lifecycle.retrieved_at, Some(200));
        assert_eq!(got.summary, "exact résumé");
    }

    #[test]
    fn every_truncation_and_trailing_byte_is_refused() {
        // As recalld's `schema_codec_rejects_trailing_truncated_unknown_and_lossy_legacy_data`.
        let v1 = v1_bytes(&sample(), 1, &[(200, 1)]);
        let v2 = v2_bytes(&v1, &sample_schema());
        for bytes in [&v1, &v2] {
            for end in 0..bytes.len() {
                assert!(DiskRecord::from_bytes(&bytes[..end]).is_err(), "{end}");
            }
            let mut trailing = bytes.clone();
            trailing.push(0);
            assert!(DiskRecord::from_bytes(&trailing).is_err());
        }
    }

    #[test]
    fn unknown_values_are_refused() {
        let mut bad_kind = v1_bytes(&sample(), 1, &[(200, 1)]);
        *bad_kind.last_mut().unwrap() = 255;
        assert_eq!(
            DiskRecord::from_bytes(&bad_kind),
            Err("unknown access kind")
        );
        let bad_phase = v1_bytes(&sample(), 5, &[]);
        assert_eq!(DiskRecord::from_bytes(&bad_phase), Err("unknown phase"));
        let mut version = v1_bytes(&sample(), 1, &[]);
        version[0] = 3;
        assert_eq!(
            DiskRecord::from_bytes(&version),
            Err("unknown record version")
        );
        let mut features = sample_schema();
        features["requiredFeatures"] = serde_json::json!(["future-required"]);
        let bytes = v2_bytes(&v1_bytes(&sample(), 1, &[]), &features);
        assert_eq!(
            DiskRecord::from_bytes(&bytes),
            Err("the record needs a feature this importer does not know")
        );
    }
}
