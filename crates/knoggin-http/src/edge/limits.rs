// SPDX-License-Identifier: AGPL-3.0-only
//! Rate limits and per-user caps of the edge layer: token buckets in memory, and counters
//! of open exports, imports and requests in flight.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use knoggin_core::UserId;
use tokio::time::Instant;
use uuid::Uuid;

use crate::error::{ApiError, Code};

/// A rate and a burst, in requests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    /// Requests per second.
    pub per_second: f64,
    /// The bucket's size.
    pub burst: f64,
}

/// An entry at rest this long with a full bucket is removed.
const IDLE: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

/// Token buckets keyed by `K`.
#[derive(Debug)]
pub(crate) struct Buckets<K> {
    rate: Rate,
    map: Mutex<HashMap<K, Bucket>>,
}

impl<K: Hash + Eq + Clone> Buckets<K> {
    pub(crate) fn new(rate: Rate) -> Self {
        Self {
            rate,
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one token, or answers `rate_limited` with the seconds until one is back.
    pub(crate) fn take(&self, key: &K, kind: &'static str) -> Result<(), ApiError> {
        let now = Instant::now();
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let Rate { per_second, burst } = self.rate;
        let full = |b: &Bucket| {
            (b.tokens + now.duration_since(b.at).as_secs_f64() * per_second).min(burst)
        };
        if map.len() > 1024 {
            // ponytail: a sweep over the whole map when it grows; fine for the few
            // thousand callers one process sees.
            map.retain(|_, b| full(b) < burst || now.duration_since(b.at) < IDLE);
        }
        let bucket = map.entry(key.clone()).or_insert(Bucket {
            tokens: burst,
            at: now,
        });
        bucket.tokens = full(bucket);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        let wait = ((1.0 - bucket.tokens) / per_second.max(f64::MIN_POSITIVE)).ceil();
        drop(map);
        metrics::counter!("knoggin_rate_limited_total", "kind" => kind).increment(1);
        Err(rate_limited(wait.clamp(1.0, 3600.0) as u32))
    }
}

impl<K: Hash + Eq> Buckets<K> {
    /// Gives back one token that `take` took, for a request that turned out not to count.
    pub(crate) fn give_back(&self, key: &K) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(b) = map.get_mut(key) {
            b.tokens = (b.tokens + 1.0).min(self.rate.burst);
        }
    }
}

/// The bucket an address is counted in: an IPv4 address (an IPv4-mapped IPv6 address counts
/// as that IPv4 address) is its own bucket; IPv6 addresses share one per /64.
pub fn address_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
        v4 => v4,
    }
}

/// `rate_limited` with `Retry-After`.
pub(crate) fn rate_limited(seconds: u32) -> ApiError {
    ApiError::new(Code::RateLimited, "Too many requests; slow down").with_retry_after(seconds)
}

/// What a per-user cap counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Cap {
    /// Requests in flight over all stores: 128.
    InFlight,
    /// Open exports: 2.
    Export,
    /// Open imports: 2.
    Import,
    /// Open event streams: 10.
    Events,
}

impl Cap {
    fn max(self) -> u32 {
        match self {
            Self::InFlight => 128,
            Self::Export | Self::Import => 2,
            Self::Events => 10,
        }
    }
}

/// Counters of what each user has open.
#[derive(Debug, Default)]
pub(crate) struct Caps {
    open: Arc<Mutex<HashMap<(UserId, Cap), u32>>>,
}

/// Holds one slot of a cap until dropped.
#[derive(Debug)]
pub(crate) struct CapGuard {
    open: Arc<Mutex<HashMap<(UserId, Cap), u32>>>,
    key: (UserId, Cap),
}

impl Caps {
    pub(crate) fn take(&self, user: UserId, cap: Cap) -> Result<CapGuard, ApiError> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let n = open.entry((user, cap)).or_insert(0);
        if *n >= cap.max() {
            return Err(rate_limited(1));
        }
        *n += 1;
        Ok(CapGuard {
            open: self.open.clone(),
            key: (user, cap),
        })
    }
}

impl Drop for CapGuard {
    fn drop(&mut self) {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = open.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                open.remove(&self.key);
            }
        }
    }
}

/// The key of the bucket per credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CredentialKey {
    /// An API key, by ID.
    Key(Uuid),
    /// A local web session, by the SHA-256 of its token.
    LocalSession([u8; 32]),
    /// An OAuth access token, by its SHA-256.
    Token([u8; 32]),
    /// A browser session, by the SHA-256 of its cookie value.
    Cookie([u8; 32]),
    /// The one user of a server with sign-in off.
    None,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_bucket_refills_at_its_rate() {
        let b = Buckets::new(Rate {
            per_second: 2.0,
            burst: 3.0,
        });
        for _ in 0..3 {
            b.take(&1, "credential").unwrap();
        }
        let e = b.take(&1, "credential").unwrap_err();
        assert_eq!((e.code, e.retry_after), (Code::RateLimited, Some(1)));
        b.take(&2, "credential").unwrap();
        tokio::time::advance(Duration::from_millis(500)).await;
        b.take(&1, "credential").unwrap();
        assert!(b.take(&1, "credential").is_err());
    }

    #[test]
    fn addresses_are_keyed_canonically_and_ipv6_by_its_64() {
        let v4: IpAddr = "192.0.2.7".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.0.2.7".parse().unwrap();
        assert_eq!(address_key(mapped), v4);
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(address_key(a), address_key(b));
        assert_ne!(address_key(a), address_key(c));
    }

    #[test]
    fn caps_free_their_slot_on_drop() {
        let caps = Caps::default();
        let user = UserId::from_uuid(Uuid::now_v7());
        let a = caps.take(user, Cap::Export).unwrap();
        let _b = caps.take(user, Cap::Export).unwrap();
        assert_eq!(
            caps.take(user, Cap::Export).unwrap_err().code,
            Code::RateLimited
        );
        caps.take(user, Cap::Import).unwrap();
        drop(a);
        caps.take(user, Cap::Export).unwrap();
    }
}
