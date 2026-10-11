//! Helpers shared by every [`Storage`](crate::Storage) backend.
//!
//! Address stepping and timestamp conversion live here (rather than in a
//! backend module) so that every backend allocates identically: the
//! conformance suite in `storage_tests` relies on that.

use std::{
    collections::HashSet,
    net::IpAddr,
    time::{Duration, SystemTime},
};

use crate::{ClientInfo, State};

/// Seconds since the Unix epoch as an `i64`, which is how every backend stores
/// timestamps. Panics on a time before 1970.
pub fn systime_epoch(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .expect("failed to get system time")
        .as_secs() as i64
}

/// Inverse of [`systime_epoch`].
pub fn to_systime(time: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(time as u64)
}

/// The first address in `[start, end]` that is not excluded. Used when a range
/// is empty so the excluded start of a range is not handed out.
pub fn first_available(start: IpAddr, end: IpAddr, exclusions: &HashSet<IpAddr>) -> Option<IpAddr> {
    match (start, end) {
        (IpAddr::V4(start), IpAddr::V4(end)) => ipnet::Ipv4AddrRange::new(start, end)
            .map(IpAddr::V4)
            .find(|ip| !exclusions.contains(ip)),
        (IpAddr::V6(start), IpAddr::V6(end)) => ipnet::Ipv6AddrRange::new(start, end)
            .map(IpAddr::V6)
            .find(|ip| !exclusions.contains(ip)),
        _ => None,
    }
}

/// Get the next IP between start and end, skipping any exclusions. `start` is
/// the current max allocated address, so `nth(1)` returns the next address
/// after it (both v4 and v6).
///
/// Note: the exclusions filter is applied before `nth(1)`, so if `start` is
/// itself excluded the address immediately after it is skipped too. Every
/// backend shares this function so they allocate identically; fix it here
/// (with a conformance case) rather than in one backend.
pub fn inc_ip(start: IpAddr, end: IpAddr, exclusions: &HashSet<IpAddr>) -> Option<IpAddr> {
    match (start, end) {
        (IpAddr::V4(ip), IpAddr::V4(end)) => ipnet::Ipv4AddrRange::new(ip, end)
            .map(IpAddr::V4)
            .filter(|ip| !exclusions.contains(ip))
            .nth(1),
        (IpAddr::V6(ip), IpAddr::V6(end)) => ipnet::Ipv6AddrRange::new(ip, end)
            .map(IpAddr::V6)
            .filter(|ip| !exclusions.contains(ip))
            .nth(1),
        _ => None,
    }
}

/// Decode the stored `(leased, probation)` flags into a [`State`].
pub fn into_clientinfo(info: ClientInfo, leased: bool, probation: bool) -> State {
    if leased {
        State::Leased(info)
    } else if probation {
        State::Probated(info)
    } else {
        State::Reserved(info)
    }
}
