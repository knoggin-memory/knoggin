// SPDX-License-Identifier: AGPL-3.0-only
//! The router's internal listener (tab "Server", the broker): the bounds per peer address
//! that apply before any token is checked (8 open connections, 60 new ones a minute, and
//! a minute's refusal after 5 failed token checks within a minute), the one-host allow
//! list, and the router that serves only `/internal/v1/` (a public route gets 404).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::response::IntoResponse;
use knoggin_http::ApiError;
use tokio::time::Instant;

use crate::settings::Cidr;

/// Open connections one address may hold.
const MAX_OPEN: u32 = 8;
/// New connections one address may make in a minute.
const MAX_NEW: u32 = 60;
/// Failed token checks within a minute before an address is refused for a minute.
const MAX_FAILED: u32 = 5;
const MINUTE: Duration = Duration::from_secs(60);
/// An entry at rest this long with nothing open is removed.
const IDLE: Duration = Duration::from_secs(600);

#[derive(Debug)]
struct Entry {
    open: u32,
    /// Start of the current minute of new connections, and their count.
    window: Instant,
    new: u32,
    /// Start of the current minute of failed tokens, and their count.
    failed_window: Instant,
    failed: u32,
    refused_until: Option<Instant>,
    seen: Instant,
}

impl Entry {
    fn new(now: Instant) -> Self {
        Self {
            open: 0,
            window: now,
            new: 0,
            failed_window: now,
            failed: 0,
            refused_until: None,
            seen: now,
        }
    }
}

/// The counts per peer address of the internal listener, kept in the router's memory.
#[derive(Debug, Default)]
pub struct PeerBounds(Arc<Mutex<HashMap<IpAddr, Entry>>>);

/// Holds one open connection of an address until dropped.
#[derive(Debug)]
pub struct Admitted {
    map: Arc<Mutex<HashMap<IpAddr, Entry>>>,
    ip: IpAddr,
}

impl Drop for Admitted {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(e) = map.get_mut(&self.ip) {
            e.open = e.open.saturating_sub(1);
        }
    }
}

impl PeerBounds {
    /// Admits a new connection from `ip`, or `None` to close it without an answer.
    pub fn admit(&self, ip: IpAddr) -> Option<Admitted> {
        let ip = ip.to_canonical();
        let now = Instant::now();
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if map.len() > 1024 {
            // ponytail: a sweep over the whole map when it grows; fine for the few
            // thousand containers one router sees.
            map.retain(|_, e| e.open > 0 || now.duration_since(e.seen) < IDLE);
        }
        let e = map.entry(ip).or_insert_with(|| Entry::new(now));
        e.seen = now;
        if e.refused_until.is_some_and(|t| now < t) {
            return None;
        }
        if now.duration_since(e.window) >= MINUTE {
            e.window = now;
            e.new = 0;
        }
        if e.open >= MAX_OPEN || e.new >= MAX_NEW {
            return None;
        }
        e.open += 1;
        e.new += 1;
        Some(Admitted {
            map: self.0.clone(),
            ip,
        })
    }

    /// Counts a failed token check from `ip`; the fifth within a minute refuses the
    /// address for the next minute.
    pub fn failed_token(&self, ip: IpAddr) {
        let ip = ip.to_canonical();
        let now = Instant::now();
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let e = map.entry(ip).or_insert_with(|| Entry::new(now));
        if now.duration_since(e.failed_window) >= MINUTE {
            e.failed_window = now;
            e.failed = 0;
        }
        e.failed += 1;
        if e.failed >= MAX_FAILED {
            e.refused_until = Some(now + MINUTE);
            tracing::warn!(
                kind = "peer_refused",
                "an internal peer address is refused for a minute"
            );
        }
    }

    /// Clears the counts and any refusal of an address that passes to a new container.
    /// Connections that are still open stay counted.
    pub fn clear(&self, ip: IpAddr) {
        let ip = ip.to_canonical();
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(e) = map.get_mut(&ip) {
            let open = e.open;
            *e = Entry::new(Instant::now());
            e.open = open;
        }
    }
}

/// The internal listener's `admit`: on one host only peers inside
/// `containers.user_net_cidr`, then the bounds per address.
pub(crate) fn admit(
    bounds: Arc<PeerBounds>,
    user_net: Option<Cidr>,
) -> impl Fn(SocketAddr) -> Option<Admitted> {
    move |peer| {
        let ip = peer.ip().to_canonical();
        if user_net.as_ref().is_some_and(|c| !c.contains(ip)) {
            return None;
        }
        bounds.admit(ip)
    }
}

/// The internal listener's routes: `broker` (whose routes are all under `/internal/v1/`),
/// and 404 for anything else.
pub fn internal_router(broker: Router) -> Router {
    broker.fallback(|| async { ApiError::not_found("No such route").into_response() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn open_new_and_failed_bounds_per_address() {
        let b = PeerBounds::default();
        let a: IpAddr = "10.201.0.3".parse().unwrap();
        let other: IpAddr = "10.201.0.11".parse().unwrap();
        let held: Vec<_> = (0..8).map(|_| b.admit(a).unwrap()).collect();
        assert!(b.admit(a).is_none(), "a ninth open connection is refused");
        assert!(b.admit(other).is_some());
        drop(held);
        // 60 new connections a minute.
        for _ in 0..52 {
            drop(b.admit(a).unwrap());
        }
        assert!(b.admit(a).is_none());
        tokio::time::advance(MINUTE).await;
        drop(b.admit(a).unwrap());
        // Five failed tokens refuse the address for a minute; another address goes on.
        for _ in 0..5 {
            b.failed_token(a);
        }
        assert!(b.admit(a).is_none());
        assert!(b.admit(other).is_some());
        // A new container at that address is served at once.
        b.clear(a);
        assert!(b.admit(a).is_some());
        for _ in 0..5 {
            b.failed_token(a);
        }
        tokio::time::advance(MINUTE).await;
        assert!(b.admit(a).is_some(), "the refusal ends after a minute");
    }

    #[test]
    fn one_host_admits_only_the_user_network() {
        let bounds = Arc::new(PeerBounds::default());
        let net: Cidr = "10.201.0.0/16".parse().unwrap();
        let f = admit(bounds, Some(net));
        assert!(f("10.201.4.2:5000".parse().unwrap()).is_some());
        assert!(f("10.202.4.2:5000".parse().unwrap()).is_none());
        assert!(f("[::ffff:10.201.4.2]:5000".parse().unwrap()).is_some());
    }
}
