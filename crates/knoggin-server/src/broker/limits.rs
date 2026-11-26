// SPDX-License-Identifier: AGPL-3.0-only
//! The broker's counts, all in the router's memory (tab "Server", limits per container
//! and per user, per peer address): calls of one container, bytes one user moves in an
//! hour, and connections and failed tokens of one peer address. With several routers each
//! keeps its own; that is accepted.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use knoggin_core::UserId;
use knoggin_state::Clock;
use uuid::Uuid;

use crate::lock;

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
/// An entry at rest this long is dropped, so the maps stay small.
const REST_MS: i64 = 10 * MINUTE_MS;

/// Broker calls one container may have in flight.
pub(crate) const CALLS_IN_FLIGHT: u32 = 4;
/// Broker calls one container may start in a minute.
pub(crate) const CALLS_PER_MINUTE: u32 = 600;
/// Open connections of one peer address.
pub(crate) const OPEN_PER_ADDRESS: u32 = 8;
/// New connections of one peer address in a minute.
pub(crate) const NEW_PER_MINUTE: u32 = 60;
/// Failed token checks in a minute after which an address is refused for a minute.
pub(crate) const FAILED_TOKENS: u32 = 5;

/// The clock, printable.
#[derive(Clone)]
struct Now(Clock);

impl std::fmt::Debug for Now {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Clock")
    }
}

impl Now {
    fn ms(&self) -> i64 {
        (self.0)()
    }
}

/// A fixed one-minute window: the count and when it began.
#[derive(Debug, Clone, Copy, Default)]
struct Window {
    start: i64,
    count: u32,
}

impl Window {
    /// Counts one more at `now` unless `max` were passed.
    fn take(&mut self, now: i64, max: u32) -> bool {
        if now - self.start >= MINUTE_MS {
            *self = Window {
                start: now,
                count: 0,
            };
        }
        if self.count >= max {
            return false;
        }
        self.count += 1;
        true
    }
}

#[derive(Debug, Default)]
struct Calls {
    in_flight: u32,
    window: Window,
}

/// Broker calls per container: 4 in flight and 600 a minute.
#[derive(Debug)]
pub(crate) struct CallLimits {
    clock: Now,
    map: Mutex<HashMap<Uuid, Calls>>,
}

/// One call in flight; the slot frees when this is dropped. A blob transfer keeps it for
/// as long as its body moves.
#[derive(Debug)]
pub(crate) struct CallSlot {
    limits: Arc<CallLimits>,
    container: Uuid,
}

impl Drop for CallSlot {
    fn drop(&mut self) {
        if let Some(c) = lock(&self.limits.map).get_mut(&self.container) {
            c.in_flight = c.in_flight.saturating_sub(1);
        }
    }
}

impl CallLimits {
    pub(crate) fn new(clock: Clock) -> Arc<Self> {
        Arc::new(Self {
            clock: Now(clock),
            map: Mutex::new(HashMap::new()),
        })
    }

    /// A slot for one call of `container`, or `None` over a limit.
    pub(crate) fn enter(self: &Arc<Self>, container: Uuid) -> Option<CallSlot> {
        let now = self.clock.ms();
        let mut map = lock(&self.map);
        // ponytail: a scan per call; a timer-driven sweep if the map ever gets large.
        map.retain(|_, c| c.in_flight > 0 || now - c.window.start < REST_MS);
        let c = map.entry(container).or_default();
        if c.in_flight >= CALLS_IN_FLIGHT || !c.window.take(now, CALLS_PER_MINUTE) {
            return None;
        }
        c.in_flight += 1;
        Some(CallSlot {
            limits: self.clone(),
            container,
        })
    }
}

/// The bytes each user's containers moved in the last hour, uploads or downloads.
#[derive(Debug)]
pub(crate) struct HourlyBytes {
    clock: Now,
    limit: u64,
    map: Mutex<HashMap<UserId, VecDeque<(i64, u64)>>>,
}

impl HourlyBytes {
    pub(crate) fn new(clock: Clock, limit: u64) -> Self {
        Self {
            clock: Now(clock),
            limit,
            map: Mutex::new(HashMap::new()),
        }
    }

    fn used(entries: &mut VecDeque<(i64, u64)>, now: i64) -> u64 {
        while entries.front().is_some_and(|(at, _)| now - at >= HOUR_MS) {
            entries.pop_front();
        }
        entries.iter().map(|e| e.1).fold(0, u64::saturating_add)
    }

    /// Counts `bytes` for `user` unless that would pass the hourly limit.
    pub(crate) fn take(&self, user: UserId, bytes: u64) -> bool {
        let now = self.clock.ms();
        let mut map = lock(&self.map);
        map.retain(|_, e| e.back().is_some_and(|(at, _)| now - at < HOUR_MS));
        let entries = map.entry(user).or_default();
        if Self::used(entries, now).saturating_add(bytes) > self.limit {
            return false;
        }
        entries.push_back((now, bytes));
        true
    }

    /// Whether `user` still has budget left; the bytes are counted with `add` as they move.
    pub(crate) fn has_room(&self, user: UserId) -> bool {
        let now = self.clock.ms();
        let mut map = lock(&self.map);
        map.get_mut(&user)
            .is_none_or(|e| Self::used(e, now) < self.limit)
    }

    /// Counts bytes that moved.
    pub(crate) fn add(&self, user: UserId, bytes: u64) {
        let now = self.clock.ms();
        lock(&self.map)
            .entry(user)
            .or_default()
            .push_back((now, bytes));
    }
}

#[derive(Debug, Default)]
struct Peer {
    open: u32,
    new: Window,
    fails: Window,
    refused_until: i64,
    last_seen: i64,
}

/// The bounds of one peer address on the internal listener, before any token is checked:
/// 8 open connections, 60 new ones a minute, and a minute's refusal after 5 failed token
/// checks within a minute. The router's `admit` for `serve_tls` calls [`PeerLimits::admit`].
#[derive(Debug)]
pub struct PeerLimits {
    clock: Now,
    map: Mutex<HashMap<IpAddr, Peer>>,
}

/// Held for as long as an admitted connection lives.
#[derive(Debug)]
pub struct PeerGuard {
    limits: Arc<PeerLimits>,
    ip: IpAddr,
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        if let Some(p) = lock(&self.limits.map).get_mut(&self.ip) {
            p.open = p.open.saturating_sub(1);
        }
    }
}

impl PeerLimits {
    /// Empty counts, reading the time from `clock`.
    pub fn new(clock: Clock) -> Arc<Self> {
        Arc::new(Self {
            clock: Now(clock),
            map: Mutex::new(HashMap::new()),
        })
    }

    /// Admits a new connection from `peer`, or `None`: close it without an answer.
    pub fn admit(self: &Arc<Self>, peer: SocketAddr) -> Option<PeerGuard> {
        let ip = peer.ip().to_canonical();
        let now = self.clock.ms();
        let mut map = lock(&self.map);
        map.retain(|_, p| p.open > 0 || now - p.last_seen < REST_MS);
        let p = map.entry(ip).or_default();
        p.last_seen = now;
        if now < p.refused_until || p.open >= OPEN_PER_ADDRESS || !p.new.take(now, NEW_PER_MINUTE) {
            return None;
        }
        p.open += 1;
        Some(PeerGuard {
            limits: self.clone(),
            ip,
        })
    }

    /// Notes a failed token check from `ip`; the fifth within a minute refuses the address
    /// for the next minute.
    pub fn token_failed(&self, ip: IpAddr) {
        let now = self.clock.ms();
        let mut map = lock(&self.map);
        let p = map.entry(ip.to_canonical()).or_default();
        p.last_seen = now;
        if !p.fails.take(now, FAILED_TOKENS - 1) {
            p.refused_until = now + MINUTE_MS;
            p.fails = Window::default();
        }
    }

    /// Whether `ip` is refused for failed tokens: then a connection it already holds is
    /// served nothing more either.
    pub fn is_refused(&self, ip: IpAddr) -> bool {
        let now = self.clock.ms();
        lock(&self.map)
            .get(&ip.to_canonical())
            .is_some_and(|p| now < p.refused_until)
    }

    /// Clears the counts and any refusal of `ip`, when a new container is recorded there.
    /// Its open connections stay counted until they close.
    pub fn clear(&self, ip: IpAddr) {
        if let Some(p) = lock(&self.map).get_mut(&ip.to_canonical()) {
            let open = p.open;
            *p = Peer {
                open,
                ..Peer::default()
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use knoggin_state::testing::TestClock;

    use super::*;

    #[test]
    fn calls_are_bounded_in_flight_and_per_minute() {
        let clock = TestClock::new();
        let limits = CallLimits::new(clock.clock());
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let held: Vec<_> = (0..4).map(|_| limits.enter(a).unwrap()).collect();
        assert!(limits.enter(a).is_none(), "a fifth in flight");
        assert!(
            limits.enter(b).is_some(),
            "another container is not affected"
        );
        drop(held);
        for _ in 0..596 {
            drop(limits.enter(a).unwrap());
        }
        assert!(limits.enter(a).is_none(), "601st in a minute");
        clock.advance(MINUTE_MS);
        assert!(limits.enter(a).is_some());
    }

    #[test]
    fn hourly_bytes_slide() {
        let clock = TestClock::new();
        let budget = HourlyBytes::new(clock.clock(), 100);
        let (u, v) = (
            UserId::from_uuid(Uuid::now_v7()),
            UserId::from_uuid(Uuid::now_v7()),
        );
        assert!(budget.take(u, 60));
        assert!(!budget.take(u, 41));
        assert!(budget.take(v, 100), "per user");
        clock.advance(HOUR_MS / 2);
        assert!(budget.take(u, 40));
        assert!(!budget.has_room(u));
        clock.advance(HOUR_MS / 2);
        assert!(budget.has_room(u), "the first 60 are an hour old");
        budget.add(u, 60);
        assert!(!budget.has_room(u));
    }

    #[test]
    fn a_peer_is_bounded_and_refused_after_failed_tokens() {
        let clock = TestClock::new();
        let limits = PeerLimits::new(clock.clock());
        let a: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let open: Vec<_> = (0..8).map(|_| limits.admit(a).unwrap()).collect();
        assert!(limits.admit(a).is_none(), "a ninth open");
        assert!(limits.admit(b).is_some());
        drop(open);
        for _ in 0..52 {
            drop(limits.admit(a).unwrap());
        }
        assert!(limits.admit(a).is_none(), "61st new in a minute");
        clock.advance(MINUTE_MS);
        for _ in 0..4 {
            limits.token_failed(a.ip());
        }
        assert!(limits.admit(a).is_some(), "four failures are allowed");
        limits.token_failed(a.ip());
        assert!(limits.admit(a).is_none(), "refused after the fifth");
        assert!(limits.is_refused(a.ip()) && !limits.is_refused(b.ip()));
        // A new container at that address is served at once.
        limits.clear(a.ip());
        assert!(limits.admit(a).is_some());
        // An IPv4-mapped IPv6 address is the same peer.
        let mapped: SocketAddr = "[::ffff:10.0.0.2]:1".parse().unwrap();
        for _ in 0..5 {
            limits.token_failed(mapped.ip());
        }
        assert!(limits.admit(b).is_none());
    }
}
