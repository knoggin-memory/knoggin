// SPDX-License-Identifier: AGPL-3.0-only
//! The snapshot container, version 1: a header, then AES-256-GCM chunks under a key that
//! HKDF-SHA-256 derives from the data key, a per-snapshot salt and every header byte.

use std::fmt;
use std::io::{self, Read};

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::hkdf::{HKDF_SHA256, Salt};
use knoggin_core::StoreId;
use zeroize::{Zeroize, Zeroizing};

use crate::key::{DataKey, WrappedKey, fill_random};

/// The first 8 bytes of every snapshot.
pub const MAGIC: [u8; 8] = *b"KNOGSNAP";
/// The container format this build reads and writes.
pub const CONTAINER_FORMAT: u16 = 1;
/// `alg` 1: AES-256-GCM chunks with an HKDF-SHA-256 key.
pub const ALG: u8 = 1;
/// `chunk_log2` 16: 65,536 plaintext bytes per chunk.
pub const CHUNK_LOG2: u8 = 16;
/// Plaintext bytes in every chunk but the last.
pub const CHUNK_LEN: usize = 1 << CHUNK_LOG2;
/// The tag that follows each chunk's ciphertext.
pub const TAG_LEN: usize = 16;
/// Upper bound on `kek_id` and on the wrapped key, in bytes.
pub const MAX_FIELD_LEN: usize = 1024;
/// Length of a header with an empty `kek_id` and wrapped key.
pub const MIN_HEADER_LEN: usize = 100;

const SALT_LEN: usize = 32;
/// Bytes up to and including `kek_id_len`.
const FIXED_LEN: usize = 98;
const INFO: &[u8] = b"knoggin-snapshot-v1";

/// Errors of the container format.
#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    /// The bytes are not a valid snapshot under this key: a failed check or a chunk that does not decrypt.
    #[error("corrupt snapshot: {0}")]
    Corrupt(&'static str),
    /// An unknown `container_format`, `alg` or `chunk_log2`.
    #[error("unknown container format")]
    UnsupportedContainer,
    /// A `content_format` outside what the caller reads.
    #[error("unsupported content format {0}")]
    UnsupportedContentFormat(u32),
    /// The header names another snapshot than the caller asked for.
    #[error("the snapshot's {field} differs from the one expected")]
    Unexpected {
        /// `store_id`, `generation` or `sequence`.
        field: &'static str,
    },
    /// The content is longer than the caller's limit. Returned only after the first chunk verified the header.
    #[error("content of {content_len} bytes is over the limit of {max_len}")]
    TooLarge {
        /// `content_len` from the verified header.
        content_len: u64,
        /// The caller's limit.
        max_len: u64,
    },
    /// A header that cannot be written: `kek_id` or the wrapped key over 1,024 bytes, or a format other than 1.
    #[error("invalid header: {0}")]
    InvalidHeader(&'static str),
    /// Reading the body failed. A break in transit, not corruption.
    #[error("reading the snapshot failed")]
    Io(#[source] io::Error),
    /// The system random source failed.
    #[error("the random source failed")]
    Random,
    /// The cipher refused its input.
    #[error("the cipher failed")]
    Crypto,
}

/// A snapshot header, field for field. `seal` sets `salt` and `content_len` itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Always 1.
    pub container_format: u16,
    /// Always 1.
    pub alg: u8,
    /// Always 16.
    pub chunk_log2: u8,
    /// Version of the content inside.
    pub content_format: u32,
    /// The store this snapshot belongs to.
    pub store_id: StoreId,
    /// Lease generation on the server; open counter locally.
    pub generation: u64,
    /// Counts up within a generation.
    pub sequence: u64,
    /// When the snapshot was made, in milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// Length of the plaintext content.
    pub content_len: u64,
    /// Random, new for every snapshot.
    pub salt: [u8; SALT_LEN],
    /// Names the master key that wrapped the data key.
    pub kek_id: String,
    /// The store's data key, wrapped.
    pub wrapped_key: Vec<u8>,
}

impl Header {
    /// A version-1 header. `seal` fills in the salt and the content length.
    pub fn new(
        store_id: StoreId,
        generation: u64,
        sequence: u64,
        created_ms: u64,
        content_format: u32,
        wrapped: WrappedKey,
    ) -> Self {
        Self {
            container_format: CONTAINER_FORMAT,
            alg: ALG,
            chunk_log2: CHUNK_LOG2,
            content_format,
            store_id,
            generation,
            sequence,
            created_ms,
            content_len: 0,
            salt: [0; SALT_LEN],
            kek_id: wrapped.kek_id,
            wrapped_key: wrapped.blob,
        }
    }

    /// `header_len = 100 + K + W`.
    pub fn header_len(&self) -> usize {
        MIN_HEADER_LEN + self.kek_id.len() + self.wrapped_key.len()
    }

    /// The wrapped data key of the header, to hand to `KeyProvider::unwrap`.
    pub fn wrapped_key(&self) -> WrappedKey {
        WrappedKey {
            kek_id: self.kek_id.clone(),
            blob: self.wrapped_key.clone(),
        }
    }

    /// Checks that this is the snapshot the caller asked for. Recovery tools skip it.
    pub fn expect(
        &self,
        store: StoreId,
        generation: u64,
        sequence: u64,
    ) -> Result<(), ContainerError> {
        let field = if self.store_id != store {
            "store_id"
        } else if self.generation != generation {
            "generation"
        } else if self.sequence != sequence {
            "sequence"
        } else {
            return Ok(());
        };
        Err(ContainerError::Unexpected { field })
    }

    /// Refuses a `content_format` outside `min..=max`.
    pub fn check_content_format(&self, min: u32, max: u32) -> Result<(), ContainerError> {
        if (min..=max).contains(&self.content_format) {
            Ok(())
        } else {
            Err(ContainerError::UnsupportedContentFormat(
                self.content_format,
            ))
        }
    }

    fn encode(&self) -> Result<Vec<u8>, ContainerError> {
        if self.container_format != CONTAINER_FORMAT
            || self.alg != ALG
            || self.chunk_log2 != CHUNK_LOG2
        {
            return Err(ContainerError::InvalidHeader(
                "only container format 1 is written",
            ));
        }
        let k = field_len(self.kek_id.len(), "kek_id is over 1,024 bytes")?;
        let w = field_len(self.wrapped_key.len(), "wrapped key is over 1,024 bytes")?;
        let mut out = Vec::with_capacity(self.header_len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.container_format.to_le_bytes());
        out.push(self.alg);
        out.push(self.chunk_log2);
        out.extend_from_slice(&self.content_format.to_le_bytes());
        out.extend_from_slice(self.store_id.as_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&self.created_ms.to_le_bytes());
        out.extend_from_slice(&self.content_len.to_le_bytes());
        out.extend_from_slice(&self.salt);
        out.extend_from_slice(&k.to_le_bytes());
        out.extend_from_slice(self.kek_id.as_bytes());
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&self.wrapped_key);
        Ok(out)
    }
}

fn field_len(len: usize, too_long: &'static str) -> Result<u16, ContainerError> {
    if len > MAX_FIELD_LEN {
        return Err(ContainerError::InvalidHeader(too_long));
    }
    u16::try_from(len).map_err(|_| ContainerError::InvalidHeader(too_long))
}

fn chunk_count(content_len: u64) -> u64 {
    content_len.div_ceil(CHUNK_LEN as u64).max(1)
}

fn nonce(index: u64, last: bool) -> Nonce {
    let mut n = [0u8; NONCE_LEN];
    n[3..11].copy_from_slice(&index.to_be_bytes());
    n[11] = u8::from(last);
    Nonce::assume_unique_for_key(n)
}

fn snapshot_key(
    key: &DataKey,
    salt: &[u8; SALT_LEN],
    header: &[u8],
) -> Result<LessSafeKey, ContainerError> {
    let prk = Salt::new(HKDF_SHA256, salt).extract(key.expose());
    let info = [INFO, header];
    let okm = prk
        .expand(&info, &AES_256_GCM)
        .map_err(|_| ContainerError::Crypto)?;
    Ok(LessSafeKey::new(UnboundKey::from(okm)))
}

/// File length: `header_len + content_len + 16 * chunks`, so an upload can state it up front.
pub fn sealed_len(header: &Header, content_len: u64) -> u64 {
    (header.header_len() as u64)
        .saturating_add(content_len)
        .saturating_add((TAG_LEN as u64).saturating_mul(chunk_count(content_len)))
}

/// Seals `content`: the iterator yields the header bytes, then each sealed chunk.
///
/// Draws a new salt on every call, whatever `header.salt` holds, so a retry never repeats
/// a key or a nonce. The plaintext is wiped as soon as the last chunk is sealed.
pub fn seal(mut header: Header, key: DataKey, content: Vec<u8>) -> Sealer {
    let content = Zeroizing::new(content);
    header.content_len = content.len() as u64;
    let chunks = chunk_count(header.content_len);
    let start = fill_random(&mut header.salt)
        .map_err(|_| ContainerError::Random)
        .and_then(|()| header.encode())
        .and_then(|bytes| Ok((snapshot_key(&key, &header.salt, &bytes)?, bytes)));
    match start {
        Ok((sealing, bytes)) => Sealer {
            header: Some(bytes),
            error: None,
            key: Some(sealing),
            content,
            next: 0,
            chunks,
        },
        Err(e) => Sealer {
            header: None,
            error: Some(e),
            key: None,
            content: Zeroizing::default(),
            next: chunks,
            chunks,
        },
    }
}

/// The iterator that `seal` returns. Owns the plaintext until the last chunk is sealed.
pub struct Sealer {
    header: Option<Vec<u8>>,
    error: Option<ContainerError>,
    key: Option<LessSafeKey>,
    content: Zeroizing<Vec<u8>>,
    next: u64,
    chunks: u64,
}

impl fmt::Debug for Sealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealer")
            .field("next", &self.next)
            .field("chunks", &self.chunks)
            .finish()
    }
}

impl Sealer {
    fn finish(&mut self) {
        self.content.zeroize();
        *self.content = Vec::new();
        self.key = None;
        self.next = self.chunks;
    }
}

impl Iterator for Sealer {
    type Item = Result<Vec<u8>, ContainerError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(e) = self.error.take() {
            return Some(Err(e));
        }
        if let Some(h) = self.header.take() {
            return Some(Ok(h));
        }
        let key = self.key.as_ref()?;
        if self.next >= self.chunks {
            return None;
        }
        let index = self.next;
        let last = index + 1 == self.chunks;
        // `index * CHUNK_LEN` never passes the content length, which fits in memory.
        let start = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_mul(CHUNK_LEN))
            .unwrap_or(usize::MAX)
            .min(self.content.len());
        let end = start.saturating_add(CHUNK_LEN).min(self.content.len());
        let mut buf = Vec::with_capacity(end - start + TAG_LEN);
        buf.extend_from_slice(&self.content[start..end]);
        let sealed = key.seal_in_place_separate_tag(nonce(index, last), Aad::empty(), &mut buf);
        self.next += 1;
        if last {
            self.finish();
        }
        match sealed {
            Ok(tag) => {
                buf.extend_from_slice(tag.as_ref());
                Some(Ok(buf))
            }
            Err(_) => {
                buf.zeroize();
                self.finish();
                Some(Err(ContainerError::Crypto))
            }
        }
    }
}

/// Parses a header from the start of `bytes`. Returns it with its length.
///
/// Nothing in it is trusted until `open` has decrypted a chunk under it.
pub fn read_header(bytes: &[u8]) -> Result<(Header, usize), ContainerError> {
    let short = ContainerError::Corrupt("truncated header");
    let fixed = bytes.get(..FIXED_LEN).ok_or(short)?;
    if fixed[..8] != MAGIC {
        return Err(ContainerError::Corrupt("not a snapshot (magic)"));
    }
    let u16_at = |at: usize| u16::from_le_bytes([fixed[at], fixed[at + 1]]);
    let u64_at = |at: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&fixed[at..at + 8]);
        u64::from_le_bytes(b)
    };
    let container_format = u16_at(8);
    let (alg, chunk_log2) = (fixed[10], fixed[11]);
    if container_format != CONTAINER_FORMAT || alg != ALG || chunk_log2 != CHUNK_LOG2 {
        return Err(ContainerError::UnsupportedContainer);
    }
    let content_format = u32::from_le_bytes([fixed[12], fixed[13], fixed[14], fixed[15]]);
    let mut store = [0u8; 16];
    store.copy_from_slice(&fixed[16..32]);
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&fixed[64..96]);

    let k = usize::from(u16_at(96));
    if k > MAX_FIELD_LEN {
        return Err(ContainerError::Corrupt("kek_id_len over 1,024"));
    }
    let kek_end = FIXED_LEN + k;
    let kek = bytes
        .get(FIXED_LEN..kek_end)
        .ok_or(ContainerError::Corrupt("truncated header"))?;
    let kek_id = std::str::from_utf8(kek)
        .map_err(|_| ContainerError::Corrupt("kek_id is not UTF-8"))?
        .to_string();
    let w_bytes = bytes
        .get(kek_end..kek_end + 2)
        .ok_or(ContainerError::Corrupt("truncated header"))?;
    let w = usize::from(u16::from_le_bytes([w_bytes[0], w_bytes[1]]));
    if w > MAX_FIELD_LEN {
        return Err(ContainerError::Corrupt("wrapped_len over 1,024"));
    }
    let header_len = kek_end + 2 + w;
    let wrapped_key = bytes
        .get(kek_end + 2..header_len)
        .ok_or(ContainerError::Corrupt("truncated header"))?
        .to_vec();
    let header = Header {
        container_format,
        alg,
        chunk_log2,
        content_format,
        store_id: StoreId::from_bytes(store),
        generation: u64_at(32),
        sequence: u64_at(40),
        created_ms: u64_at(48),
        content_len: u64_at(56),
        salt,
        kek_id,
        wrapped_key,
    };
    Ok((header, header_len))
}

/// Reads exactly one header from a stream, leaving it at the first chunk.
pub fn read_header_from(r: &mut impl Read) -> Result<Header, ContainerError> {
    let mut buf = vec![0u8; FIXED_LEN];
    read_full(r, &mut buf)?;
    let k = usize::from(u16::from_le_bytes([buf[96], buf[97]]));
    if k > MAX_FIELD_LEN {
        // Let `read_header` give the precise error for the fixed part.
        read_header(&buf)?;
        return Err(ContainerError::Corrupt("kek_id_len over 1,024"));
    }
    buf.resize(FIXED_LEN + k + 2, 0);
    read_full(r, &mut buf[FIXED_LEN..])?;
    let w = usize::from(u16::from_le_bytes([
        buf[FIXED_LEN + k],
        buf[FIXED_LEN + k + 1],
    ]));
    if w > MAX_FIELD_LEN {
        read_header(&buf)?;
        return Err(ContainerError::Corrupt("wrapped_len over 1,024"));
    }
    let at = buf.len();
    buf.resize(at + w, 0);
    read_full(r, &mut buf[at..])?;
    read_header(&buf).map(|(h, _)| h)
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<(), ContainerError> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => ContainerError::Corrupt("truncated"),
        _ => ContainerError::Io(e),
    })
}

/// Decrypts the chunks that follow `header` in `body` and returns the content.
///
/// Reserves no memory from `content_len` before the first chunk has verified the header,
/// and refuses content longer than `max_len` (`TooLarge`, after that verification).
pub fn open(
    header: &Header,
    key: &DataKey,
    mut body: impl Read,
    max_len: u64,
) -> Result<Vec<u8>, ContainerError> {
    let header_bytes = header.encode()?;
    let opening = snapshot_key(key, &header.salt, &header_bytes)?;
    let len = header.content_len;
    let chunks = chunk_count(len);
    let mut out = Zeroizing::new(Vec::new());
    let mut done: u64 = 0;
    for index in 0..chunks {
        let last = index + 1 == chunks;
        // At most CHUNK_LEN: every chunk but the last is full, the last holds the rest.
        let size = usize::try_from((len - done).min(CHUNK_LEN as u64))
            .map_err(|_| ContainerError::Corrupt("chunk size"))?;
        let start = out.len();
        out.resize(start + size + TAG_LEN, 0);
        read_full(&mut body, &mut out[start..])?;
        let plain = opening
            .open_in_place(nonce(index, last), Aad::empty(), &mut out[start..])
            .map_err(|_| ContainerError::Corrupt("a chunk does not decrypt"))?
            .len();
        out.truncate(start + plain);
        done += plain as u64;
        if index == 0 {
            let too_large = ContainerError::TooLarge {
                content_len: len,
                max_len,
            };
            if len > max_len {
                return Err(too_large);
            }
            let total = usize::try_from(len).map_err(|_| too_large)?;
            let have = out.len();
            out.reserve_exact((total + TAG_LEN).saturating_sub(have));
        }
    }
    let mut one = [0u8; 1];
    loop {
        match body.read(&mut one) {
            Ok(0) => break,
            Ok(_) => return Err(ContainerError::Corrupt("bytes after the final chunk")),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ContainerError::Io(e)),
        }
    }
    Ok(std::mem::take(&mut *out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const NO_LIMIT: u64 = u64::MAX;

    fn store(n: u8) -> StoreId {
        StoreId::from_bytes([n; 16])
    }

    fn header(kek_id: &str, wrapped: Vec<u8>) -> Header {
        Header::new(
            store(7),
            3,
            9,
            1_764_000_000_000,
            1,
            WrappedKey {
                kek_id: kek_id.into(),
                blob: wrapped,
            },
        )
    }

    fn sealed(h: &Header, key: &DataKey, content: &[u8]) -> Vec<u8> {
        let parts: Vec<Vec<u8>> = seal(h.clone(), key.clone(), content.to_vec())
            .collect::<Result<_, _>>()
            .unwrap();
        parts.concat()
    }

    fn open_all(bytes: &[u8], key: &DataKey) -> Result<Vec<u8>, ContainerError> {
        let (h, n) = read_header(bytes)?;
        open(&h, key, &bytes[n..], NO_LIMIT)
    }

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn key() -> DataKey {
        DataKey::from_bytes([0x11; 32])
    }

    fn std_header() -> Header {
        header("local:3f9a1c04b2e87d55", vec![0xEE; 60])
    }

    #[test]
    fn round_trips_at_every_boundary() {
        let h = std_header();
        for len in [0, 1, CHUNK_LEN, CHUNK_LEN + 1, 1 << 20] {
            let data = content(len);
            let parts: Vec<Vec<u8>> = seal(h.clone(), key(), data.clone())
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(parts.len() as u64, 1 + chunk_count(len as u64), "len {len}");
            let bytes = parts.concat();
            assert_eq!(bytes.len() as u64, sealed_len(&h, len as u64), "len {len}");
            let (read, n) = read_header(&bytes).unwrap();
            assert_eq!(n, h.header_len());
            assert_eq!(read.content_len, len as u64);
            read.expect(store(7), 3, 9).unwrap();
            assert_eq!(open(&read, &key(), &bytes[n..], NO_LIMIT).unwrap(), data);
        }
    }

    #[test]
    fn layout_is_byte_exact() {
        let h = std_header();
        let bytes = sealed(&h, &key(), b"abc");
        assert_eq!(&bytes[0..8], b"KNOGSNAP");
        assert_eq!(&bytes[8..10], &[1, 0]);
        assert_eq!(bytes[10], 1);
        assert_eq!(bytes[11], 16);
        assert_eq!(&bytes[12..16], &[1, 0, 0, 0]);
        assert_eq!(&bytes[16..32], &[7; 16]);
        assert_eq!(&bytes[32..40], &3u64.to_le_bytes());
        assert_eq!(&bytes[40..48], &9u64.to_le_bytes());
        assert_eq!(&bytes[48..56], &1_764_000_000_000u64.to_le_bytes());
        assert_eq!(&bytes[56..64], &3u64.to_le_bytes());
        assert_eq!(&bytes[96..98], &22u16.to_le_bytes());
        assert_eq!(&bytes[98..120], b"local:3f9a1c04b2e87d55");
        assert_eq!(&bytes[120..122], &60u16.to_le_bytes());
        assert_eq!(&bytes[122..182], &[0xEE; 60]);
        assert_eq!(bytes.len(), 182 + 3 + 16);

        // The one chunk, checked against an independent derivation of the key and nonce.
        let mut info = b"knoggin-snapshot-v1".to_vec();
        info.extend_from_slice(&bytes[..182]);
        let mut salt = [0u8; 32];
        salt.copy_from_slice(&bytes[64..96]);
        let prk = Salt::new(HKDF_SHA256, &salt).extract(&[0x11; 32]);
        let info = [info.as_slice()];
        let okm = prk.expand(&info, &AES_256_GCM).unwrap();
        let k = LessSafeKey::new(UnboundKey::from(okm));
        let mut n = [0u8; 12];
        n[11] = 1;
        let mut chunk = bytes[182..].to_vec();
        let plain = k
            .open_in_place(Nonce::assume_unique_for_key(n), Aad::empty(), &mut chunk)
            .unwrap();
        assert_eq!(plain, b"abc");
    }

    #[test]
    fn every_seal_draws_a_new_salt() {
        let mut h = std_header();
        h.salt = [5; 32];
        let a = sealed(&h, &key(), b"same");
        let b = sealed(&h, &key(), b"same");
        assert_ne!(a[64..96], b[64..96]);
        assert_ne!(&a[64..96], &[5; 32]);
        assert_ne!(a[a.len() - 20..], b[b.len() - 20..]);
    }

    #[test]
    fn a_flipped_bit_in_any_header_field_fails() {
        let h = std_header();
        let good = sealed(&h, &key(), &content(CHUNK_LEN + 10));
        let k = h.kek_id.len();
        // Every field: (offset, size).
        let fields = [
            (0, 8),
            (8, 2),
            (10, 1),
            (11, 1),
            (12, 4),
            (16, 16),
            (32, 8),
            (40, 8),
            (48, 8),
            (56, 8),
            (64, 32),
            (96, 2),
            (98, k),
            (98 + k, 2),
            (100 + k, h.wrapped_key.len()),
        ];
        for (at, size) in fields {
            for byte in [at, at + size - 1] {
                for bit in [0, 7] {
                    let mut bad = good.clone();
                    bad[byte] ^= 1 << bit;
                    assert!(
                        open_all(&bad, &key()).is_err(),
                        "byte {byte} bit {bit} opened"
                    );
                }
            }
        }
        assert!(open_all(&good, &key()).is_ok());
    }

    #[test]
    fn damaged_chunks_fail() {
        let h = std_header();
        let data = content(3 * CHUNK_LEN);
        let good = sealed(&h, &key(), &data);
        let n = h.header_len();
        let c = CHUNK_LEN + TAG_LEN;
        let corrupt =
            |r: Result<Vec<u8>, ContainerError>| matches!(r, Err(ContainerError::Corrupt(_)));

        let mut flipped = good.clone();
        flipped[n + c + 100] ^= 1;
        assert!(corrupt(open_all(&flipped, &key())));

        let dropped = &good[..good.len() - c];
        assert!(corrupt(open_all(dropped, &key())));

        let mut swapped = good[..n].to_vec();
        swapped.extend_from_slice(&good[n + c..n + 2 * c]);
        swapped.extend_from_slice(&good[n..n + c]);
        swapped.extend_from_slice(&good[n + 2 * c..]);
        assert_eq!(swapped.len(), good.len());
        assert!(corrupt(open_all(&swapped, &key())));

        // A chunk from another snapshot of the same store, same header but for the salt.
        let other = sealed(&h, &key(), &data);
        let mut mixed = good.clone();
        mixed[n + c..n + 2 * c].copy_from_slice(&other[n + c..n + 2 * c]);
        assert!(corrupt(open_all(&mixed, &key())));

        let mut appended = good.clone();
        appended.push(0);
        assert!(corrupt(open_all(&appended, &key())));
        let mut extra_chunk = good.clone();
        extra_chunk.extend_from_slice(&good[n..n + c]);
        assert!(corrupt(open_all(&extra_chunk, &key())));

        // A short chunk that is not the last: cut the middle chunk by one byte.
        let mut short = good[..n + c + 10].to_vec();
        short.extend_from_slice(&good[n + c + 11..]);
        assert!(corrupt(open_all(&short, &key())));

        assert!(corrupt(open_all(&good, &DataKey::from_bytes([0x12; 32]))));
        assert_eq!(open_all(&good, &key()).unwrap(), data);
    }

    #[test]
    fn expect_checks_store_generation_and_sequence() {
        let h = std_header();
        let (read, _) = read_header(&sealed(&h, &key(), b"x")).unwrap();
        read.expect(store(7), 3, 9).unwrap();
        for (s, g, q, field) in [
            (store(8), 3, 9, "store_id"),
            (store(7), 4, 9, "generation"),
            (store(7), 3, 10, "sequence"),
        ] {
            match read.expect(s, g, q) {
                Err(ContainerError::Unexpected { field: f }) => assert_eq!(f, field),
                r => panic!("{r:?}"),
            }
        }
    }

    #[test]
    fn content_format_range() {
        let h = std_header();
        h.check_content_format(1, 1).unwrap();
        assert!(matches!(
            h.check_content_format(2, 3),
            Err(ContainerError::UnsupportedContentFormat(1))
        ));
    }

    #[test]
    fn unknown_container_fields_are_refused() {
        let good = sealed(&std_header(), &key(), b"x");
        for (at, value) in [(8, 2u8), (10, 2), (11, 15)] {
            let mut bad = good.clone();
            bad[at] = value;
            assert!(matches!(
                read_header(&bad),
                Err(ContainerError::UnsupportedContainer)
            ));
        }
        assert!(matches!(
            read_header(&good[..99]),
            Err(ContainerError::Corrupt(_))
        ));
    }

    #[test]
    fn largest_header_round_trips() {
        let h = header(&"k".repeat(1024), vec![0xAB; 1024]);
        assert_eq!(h.header_len(), 100 + 2048);
        let data = content(CHUNK_LEN + 5);
        let bytes = sealed(&h, &key(), &data);
        assert_eq!(open_all(&bytes, &key()).unwrap(), data);

        let too_long = header(&"k".repeat(1025), vec![]);
        let first = seal(too_long, key(), vec![1]).next().unwrap();
        assert!(matches!(first, Err(ContainerError::InvalidHeader(_))));
        let too_long = header("k", vec![0; 1025]);
        let mut it = seal(too_long, key(), vec![1]);
        assert!(matches!(
            it.next(),
            Some(Err(ContainerError::InvalidHeader(_)))
        ));
        assert!(it.next().is_none());
    }

    #[test]
    fn limit_applies_only_after_the_first_chunk_verified() {
        let h = std_header();
        let data = content(2 * CHUNK_LEN);
        let bytes = sealed(&h, &key(), &data);
        let (read, n) = read_header(&bytes).unwrap();
        assert!(matches!(
            open(&read, &key(), &bytes[n..], 1000),
            Err(ContainerError::TooLarge { content_len, max_len: 1000 }) if content_len == data.len() as u64
        ));
        assert_eq!(
            open(&read, &key(), &bytes[n..], data.len() as u64).unwrap(),
            data
        );

        // A forged huge content_len is reported as corrupt, not as too large, and reserves nothing.
        let mut forged = bytes.clone();
        forged[56..64].copy_from_slice(&u64::MAX.to_le_bytes());
        let (fh, fnn) = read_header(&forged).unwrap();
        assert!(matches!(
            open(&fh, &key(), &forged[fnn..], 1000),
            Err(ContainerError::Corrupt(_))
        ));
    }

    #[test]
    fn stream_header_reader_matches_and_leaves_the_body() {
        let h = header(&"z".repeat(300), vec![1; 700]);
        let data = content(1000);
        let bytes = sealed(&h, &key(), &data);
        let mut r = &bytes[..];
        let read = read_header_from(&mut r).unwrap();
        assert_eq!(read, read_header(&bytes).unwrap().0);
        assert_eq!(open(&read, &key(), r, NO_LIMIT).unwrap(), data);

        let mut r = &bytes[..50];
        assert!(matches!(
            read_header_from(&mut r),
            Err(ContainerError::Corrupt(_))
        ));
        let mut bad = bytes.clone();
        bad[96..98].copy_from_slice(&2000u16.to_le_bytes());
        assert!(matches!(
            read_header_from(&mut &bad[..]),
            Err(ContainerError::Corrupt(_))
        ));
    }

    #[test]
    fn io_errors_are_not_corruption() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::ConnectionReset))
            }
        }
        let h = std_header();
        let bytes = sealed(&h, &key(), b"abc");
        let (read, _) = read_header(&bytes).unwrap();
        assert!(matches!(
            open(&read, &key(), Broken, NO_LIMIT),
            Err(ContainerError::Io(_))
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn seal_open_round_trip(len in 0usize..200_000, seed in any::<u8>(), k in 0usize..64, w in 0usize..128) {
            let data: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31) ^ seed).collect();
            let h = header(&"q".repeat(k), vec![seed; w]);
            let bytes = sealed(&h, &key(), &data);
            prop_assert_eq!(bytes.len() as u64, sealed_len(&h, len as u64));
            prop_assert_eq!(open_all(&bytes, &key()).unwrap(), data);
        }
    }
}
