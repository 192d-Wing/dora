//! Client protection
//!
//!
use config::v4::FloodThreshold;
// TODO: consider switching both to Mutex<Hashmap<>>.
// the caches are all locked immediately and written to, so dashmap is probably overkill
// (governor uses dashmap internally by default by we can turn off the "dashmap" feature)
use dashmap::DashMap;
use governor::{Quota, RateLimiter, clock::DefaultClock, state::keyed::DefaultKeyedStateStore};
use tracing::{debug, trace};

use std::{
    borrow::Borrow,
    fmt,
    hash::Hash,
    net::IpAddr,
    num::NonZeroU32,
    time::{Duration, Instant},
};

pub struct RenewThreshold<K> {
    percentage: u64,
    cache: DashMap<K, RenewExpiry>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RenewExpiry {
    // when entry was created
    pub created: Instant,
    // % * lease_time
    pub percentage: Duration,
    // full lease time
    pub lease_time: Duration,
    // the address that was actually granted to this client. The renew fast path
    // reuses a cached lease only when the client re-requests THIS address, so a
    // client can't use its own valid cache entry to be handed an address it
    // doesn't hold.
    pub addr: IpAddr,
}

impl RenewExpiry {
    /// if the elapsed time is less than the fraction of lease time configured
    /// return the lease time remaining
    pub fn get_remaining(&self) -> Option<Duration> {
        let elapsed = self.created.elapsed();
        if elapsed <= self.percentage {
            // saturating: guards against a percentage > lease_time
            // (misconfigured threshold > 100%) underflowing here.
            Some(self.lease_time.saturating_sub(elapsed))
        } else {
            None
        }
    }
}

impl RenewExpiry {
    pub fn new(now: Instant, addr: IpAddr, lease_time: Duration, percentage: u64) -> Self {
        Self {
            // clamp the fast-path window to the lease length so it can never
            // outlive the DB binding (which would let the cache hand back an
            // address that has since been reassigned). Also saturate the
            // multiply against absurd configured percentages.
            percentage: Duration::from_secs(lease_time.as_secs().saturating_mul(percentage) / 100)
                .min(lease_time),
            created: now,
            lease_time,
            addr,
        }
    }
}

impl<K: Eq + Hash + Clone> RenewThreshold<K> {
    pub fn new(percentage: u32) -> Self {
        Self {
            percentage: percentage as u64,
            cache: DashMap::new(),
        }
    }
    // insert id into cache with the granted address and lease time, replacing
    // any existing entry
    pub fn insert(&self, id: K, addr: IpAddr, lease_time: Duration) -> Option<RenewExpiry> {
        let now = Instant::now();
        self.cache
            .insert(id, RenewExpiry::new(now, addr, lease_time, self.percentage))
    }
    /// Test whether the renew threshold is met for `id` renewing `addr`.
    /// Returns the remaining lease time only when the cached entry is still
    /// within threshold AND was granted for the same `addr`. Expired entries are
    /// evicted on access so the cache stays bounded by the active client set.
    pub fn threshold<Q>(&self, id: &Q, addr: IpAddr) -> Option<Duration>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        // copy the entry out (RenewExpiry: Copy) so the read guard is released
        // before any eviction write on the same shard.
        let entry = self.cache.get(id).map(|e| *e)?;
        match entry.get_remaining() {
            // past the threshold window: evict and fall through to the slow path
            None => {
                self.cache.remove(id);
                None
            }
            // within threshold but for a different address than we granted:
            // do NOT reuse (the caller must verify ownership via the DB).
            Some(_) if entry.addr != addr => None,
            Some(remaining) => Some(remaining),
        }
    }
    pub fn remove(&self, id: &K) -> Option<(K, RenewExpiry)> {
        self.cache.remove(id)
    }
}

pub struct FloodCache<K: Hash + Eq + Clone> {
    rl: RateLimiter<K, DefaultKeyedStateStore<K>, DefaultClock>,
}

impl<K> FloodCache<K>
where
    K: Eq + Hash + Clone + fmt::Debug,
{
    pub fn new(cfg: FloodThreshold) -> Self {
        debug!(
            packets = cfg.packets(),
            period = cfg.period().as_secs(),
            "creating flood cache with following settings"
        );
        // let rate = cfg.packets() / cfg.period().as_secs() as u32;
        // debug!("creating flood cache threshold {:?} packets/sec", rate);

        Self {
            #[allow(deprecated)]
            rl: RateLimiter::keyed(
                Quota::new(
                    NonZeroU32::new(cfg.packets()).expect("conversion will not fail"),
                    cfg.period(),
                )
                .expect("don't pass Duration of 0"),
            ),
        }
    }
    pub fn is_allowed(&self, id: &K) -> bool {
        let res = self.rl.check_key(id);
        if let Err(not_until) = &res {
            trace!(?not_until, ?id, "reached threshold for client")
        }
        res.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flood_threshold_packets() {
        let cache = FloodCache::new(FloodThreshold::new(2, Duration::from_secs(1)));
        assert!(cache.is_allowed(&[1, 2, 3, 4]));
        assert!(cache.is_allowed(&[1, 2, 3, 4]));

        // too many packets
        assert!(!cache.is_allowed(&[1, 2, 3, 4]));

        // wait for duration
        std::thread::sleep(Duration::from_millis(1_100));
        // should be true now
        assert!(cache.is_allowed(&[1, 2, 3, 4]));
        assert!(cache.is_allowed(&[1, 2, 3, 4]));
    }

    #[test]
    fn test_flood_threshold_large_period() {
        let cache = FloodCache::new(FloodThreshold::new(2, Duration::from_secs(5)));
        assert!(cache.is_allowed(&[1, 2, 3, 4]));
        assert!(cache.is_allowed(&[1, 2, 3, 4]));

        // // too many packets
        // assert!(!cache.is_allowed(&[1, 2, 3, 4]));

        // // wait for duration
        // std::thread::sleep(Duration::from_millis(1_100));
        // // should be true now
        // assert!(cache.is_allowed(&[1, 2, 3, 4]));
        // assert!(cache.is_allowed(&[1, 2, 3, 4]));
    }

    #[test]
    fn test_flood_threshold_multi() {
        let cache = FloodCache::new(FloodThreshold::new(2, Duration::from_secs(1)));
        assert!(cache.is_allowed(&[1, 2, 3, 4]));
        assert!(cache.is_allowed(&[1, 2, 3, 4]));
        assert!(!cache.is_allowed(&[1, 2, 3, 4]));

        // another client, independent threshold
        assert!(cache.is_allowed(&[4, 3, 2, 1]));
        assert!(cache.is_allowed(&[4, 3, 2, 1]));
        assert!(!cache.is_allowed(&[4, 3, 2, 1]));
    }

    #[test]
    fn test_renew_remaining() {
        let addr = IpAddr::from([1, 2, 3, 4]);
        let renew = RenewExpiry::new(Instant::now(), addr, Duration::from_secs(5), 50);
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(
            renew
                .get_remaining()
                .unwrap()
                .as_secs_f32()
                // round up
                .round(),
            4.
        );
        std::thread::sleep(Duration::from_secs(5));
        assert!(renew.get_remaining().is_none());
    }

    #[test]
    fn test_cache_threshold() {
        let cache = RenewThreshold::new(50);
        let a = IpAddr::from([10, 0, 0, 1]);
        let b = IpAddr::from([10, 0, 0, 2]);
        let lease_time = Duration::from_secs(2);
        let lease_time_b = Duration::from_secs(6);
        assert!(cache.insert([1, 2, 3, 4], a, lease_time).is_none());

        // another client, independent threshold
        assert!(cache.insert([4, 3, 2, 1], b, lease_time_b).is_none());

        // half of lease time passes
        std::thread::sleep(Duration::from_secs(1));

        assert!(cache.threshold(&[1, 2, 3, 4], a).is_none());
        assert!(cache.threshold(&[1, 2, 3, 4], a).is_none());
        assert_eq!(
            cache
                .threshold(&[4, 3, 2, 1], b)
                .unwrap()
                .as_secs_f32()
                .round(),
            5.
        );

        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(
            cache
                .threshold(&[4, 3, 2, 1], b)
                .unwrap()
                .as_secs_f32()
                .round(),
            4.
        );

        std::thread::sleep(Duration::from_secs(2));
        assert!(cache.threshold(&[4, 3, 2, 1], b).is_none());
    }

    // the fast path must only fire for the exact address the client was granted;
    // requesting a different address falls through to the slow (DB) path even
    // while the client has a valid cache entry.
    #[test]
    fn test_cache_threshold_wrong_addr() {
        let cache = RenewThreshold::new(50);
        let granted = IpAddr::from([10, 0, 0, 1]);
        let other = IpAddr::from([10, 0, 0, 99]);
        cache.insert([1, 2, 3, 4], granted, Duration::from_secs(6));

        // same client, same (in-threshold) window, but a different requested IP
        assert!(
            cache.threshold(&[1, 2, 3, 4], other).is_none(),
            "must not reuse a cached lease for a different address"
        );
        // the address it actually holds still fast-paths
        assert!(cache.threshold(&[1, 2, 3, 4], granted).is_some());
    }

    #[test]
    fn test_cache_renew_0() {
        // threshold set to 0 means the cache will never return a cached lease
        let cache = RenewThreshold::new(0);
        let a = IpAddr::from([10, 0, 0, 1]);
        let b = IpAddr::from([10, 0, 0, 2]);
        let lease_time = Duration::from_secs(2);
        let lease_time_b = Duration::from_secs(6);
        assert!(cache.insert([1, 2, 3, 4], a, lease_time).is_none());

        // another client, independent threshold
        assert!(cache.insert([4, 3, 2, 1], b, lease_time_b).is_none());

        // half of lease time passes
        std::thread::sleep(Duration::from_secs(1));

        assert!(cache.threshold(&[1, 2, 3, 4], a).is_none());
        assert!(cache.threshold(&[4, 3, 2, 1], b).is_none());
        std::thread::sleep(Duration::from_secs(3));
        assert!(cache.threshold(&[4, 3, 2, 1], b).is_none());
    }
}
