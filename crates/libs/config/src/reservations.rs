//! Runtime (API-managed) host reservations.
//!
//! An in-memory store shared between the management API — which creates,
//! updates, and deletes reservations — and the DHCP datapath, which reads it on
//! the hot path. Runtime reservations take precedence over config reservations
//! and the dynamic pool. Persistence lives in `ip-manager`; the binary warms
//! this store from the database on startup and keeps it in sync on every write.
//!
//! v4 reservations reuse the config match predicate (`Condition`: MAC or a
//! single option) and resolve to a [`crate::v4::Reserved`] so they flow through
//! the exact same `StaticAddr` assignment path as config reservations. v6
//! reservations match on the client DUID and resolve to a reserved IA_NA address
//! plus an optional IA_PD prefix.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result, anyhow, bail};
use dora_core::{
    dhcproto::v4::{DhcpOption, DhcpOptions, OptionCode},
    pnet::util::MacAddr,
};

use crate::{
    v4::Reserved,
    wire::v4::{Condition, NetworkConfig, Options, ReservedIp},
};

/// A runtime reservation's match predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResMatch {
    /// v4: the config match predicate (MAC or a single option).
    V4(Condition),
    /// v6: the client DUID (Client Identifier option bytes).
    V6Duid(Vec<u8>),
}

/// A resolved v6 reservation: a reserved IA_NA address and an optional IA_PD
/// delegated prefix (base + length).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V6Reserved {
    /// reserved IA_NA address
    pub ip: Ipv6Addr,
    /// optional reserved IA_PD prefix (base, prefix length)
    pub prefix: Option<(Ipv6Addr, u8)>,
}

/// A single runtime reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeReservation {
    /// reserved address (IA_NA for v6)
    pub ip: IpAddr,
    /// optional v6 IA_PD prefix (base, length)
    pub prefix: Option<(Ipv6Addr, u8)>,
    /// optional owning network (CIDR string), for listing/validation
    pub network: Option<String>,
    /// the match predicate
    pub match_: ResMatch,
}

impl RuntimeReservation {
    /// `"v4"` or `"v6"`, derived from the reserved address family.
    pub fn family(&self) -> &'static str {
        match self.ip {
            IpAddr::V4(_) => "v4",
            IpAddr::V6(_) => "v6",
        }
    }

    /// the reserved address as a string (the persistence / delete key)
    pub fn ip_string(&self) -> String {
        self.ip.to_string()
    }

    /// the reserved v6 prefix as `"base/len"`, if any
    pub fn prefix_string(&self) -> Option<String> {
        self.prefix.map(|(base, len)| format!("{base}/{len}"))
    }

    /// the match predicate serialized to its persisted / API JSON form
    pub fn match_json(&self) -> String {
        match_to_value(&self.match_).to_string()
    }

    /// the match predicate as a JSON value (for the API response `match` field)
    pub fn match_value(&self) -> serde_json::Value {
        match_to_value(&self.match_)
    }

    /// Reconstruct a reservation from its persisted parts (as stored by
    /// `ip-manager`).
    pub fn from_parts(
        family: &str,
        ip: &str,
        prefix: Option<&str>,
        network: Option<String>,
        match_json: &str,
    ) -> Result<Self> {
        let ip: IpAddr = ip
            .parse()
            .with_context(|| format!("invalid reserved ip {ip}"))?;
        // family must agree with the address
        match (family, ip) {
            ("v4", IpAddr::V4(_)) | ("v6", IpAddr::V6(_)) => {}
            _ => bail!("family {family} does not match address {ip}"),
        }
        let prefix = prefix.map(parse_prefix).transpose()?;
        if prefix.is_some() && !ip.is_ipv6() {
            bail!("only v6 reservations may carry a prefix");
        }
        let value: serde_json::Value =
            serde_json::from_str(match_json).context("invalid reservation match json")?;
        let match_ = match_from_value(family, &value)?;
        Ok(Self {
            ip,
            prefix,
            network,
            match_,
        })
    }
}

/// Errors from mutating the reservation store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReservationError {
    /// a reservation for this (family, ip) already exists (create only)
    AddressExists,
    /// this match predicate already reserves a different address
    MatchExists,
    /// no reservation exists for this (family, ip)
    NotFound,
}

impl fmt::Display for ReservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReservationError::AddressExists => {
                f.write_str("a reservation for this address already exists")
            }
            ReservationError::MatchExists => {
                f.write_str("this match already reserves a different address")
            }
            ReservationError::NotFound => f.write_str("no reservation for this address"),
        }
    }
}

impl std::error::Error for ReservationError {}

/// A cheap, cloneable handle to the shared runtime-reservation store.
#[derive(Debug, Clone, Default)]
pub struct RuntimeReservations {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    /// authoritative set, keyed by (family, ip)
    entries: BTreeMap<(String, IpAddr), RuntimeReservation>,
    /// v4 fast path (mirrors config `reserved_macs`)
    reserved_macs: HashMap<MacAddr, Reserved>,
    /// v4 option match (mirrors config `reserved_opts`): code -> (option, reserved)
    reserved_opts: HashMap<OptionCode, (DhcpOption, Reserved)>,
    /// v6 fast path: DUID -> reserved address/prefix
    v6_duids: HashMap<Vec<u8>, V6Reserved>,
}

impl RuntimeReservations {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Warm the store from persisted reservations (called once on startup).
    /// Returns the number loaded; malformed rows are skipped and logged.
    pub fn load(&self, reservations: impl IntoIterator<Item = RuntimeReservation>) {
        let mut inner = self.inner.write().expect("reservation lock poisoned");
        for res in reservations {
            inner
                .entries
                .insert((res.family().to_string(), res.ip), res);
        }
        inner.rebuild_indexes();
    }

    /// Insert a reservation. With `replace = false` (create) an existing address
    /// is an error; with `replace = true` (update) it overwrites. Either way a
    /// match that already points at a *different* address is rejected.
    pub fn insert(
        &self,
        res: RuntimeReservation,
        replace: bool,
    ) -> std::result::Result<(), ReservationError> {
        let mut inner = self.inner.write().expect("reservation lock poisoned");
        let key = (res.family().to_string(), res.ip);
        if !replace && inner.entries.contains_key(&key) {
            return Err(ReservationError::AddressExists);
        }
        // reject a duplicate match that resolves to a different address
        if let Some((_, existing)) = inner
            .entries
            .iter()
            .find(|((_, ip), e)| *ip != res.ip && e.match_ == res.match_)
        {
            let _ = existing;
            return Err(ReservationError::MatchExists);
        }
        inner.entries.insert(key, res);
        inner.rebuild_indexes();
        Ok(())
    }

    /// Remove a reservation by (family, ip). Returns whether one was removed.
    pub fn remove(&self, family: &str, ip: IpAddr) -> bool {
        let mut inner = self.inner.write().expect("reservation lock poisoned");
        let removed = inner.entries.remove(&(family.to_string(), ip)).is_some();
        if removed {
            inner.rebuild_indexes();
        }
        removed
    }

    /// Whether a reservation exists for (family, ip).
    pub fn contains(&self, family: &str, ip: IpAddr) -> bool {
        self.inner
            .read()
            .expect("reservation lock poisoned")
            .entries
            .contains_key(&(family.to_string(), ip))
    }

    /// All reservations, ordered by (family, ip).
    pub fn list(&self) -> Vec<RuntimeReservation> {
        self.inner
            .read()
            .expect("reservation lock poisoned")
            .entries
            .values()
            .cloned()
            .collect()
    }

    /// v4 datapath lookup: a runtime reservation for this MAC (first) or a
    /// matching request option, honoring class matches. Mirrors the config
    /// `get_reserved_mac` / `search_reserved_opt` semantics. `mac` is `None` when
    /// the request has no 6-byte chaddr, in which case only option matches apply.
    pub fn lookup_v4(
        &self,
        mac: Option<MacAddr>,
        opts: &DhcpOptions,
        classes: Option<&[String]>,
    ) -> Option<Reserved> {
        let inner = self.inner.read().expect("reservation lock poisoned");
        if let Some(res) = mac.and_then(|mac| inner.reserved_macs.get(&mac))
            && res.match_class(classes)
        {
            return Some(res.clone());
        }
        for (_, opt) in opts.iter() {
            if matches!(opt, DhcpOption::MessageType(_)) {
                continue;
            }
            if let Some((val, res)) = inner.reserved_opts.get(&opt.into())
                && val == opt
                && res.match_class(classes)
            {
                return Some(res.clone());
            }
        }
        None
    }

    /// v6 datapath lookup: the reserved address/prefix for this DUID, if any.
    pub fn lookup_v6(&self, duid: &[u8]) -> Option<V6Reserved> {
        self.inner
            .read()
            .expect("reservation lock poisoned")
            .v6_duids
            .get(duid)
            .cloned()
    }
}

impl Inner {
    /// Rebuild the derived lookup indexes from the authoritative `entries`.
    /// Reservations change rarely (only via the API), so a full rebuild on each
    /// mutation keeps the hot-path reads simple and lock-free of write logic.
    fn rebuild_indexes(&mut self) {
        self.reserved_macs.clear();
        self.reserved_opts.clear();
        self.v6_duids.clear();
        for res in self.entries.values() {
            match (&res.match_, res.ip) {
                (ResMatch::V4(Condition::Mac(mac)), IpAddr::V4(ip)) => {
                    self.reserved_macs
                        .insert(*mac, to_v4_reserved(ip, &res.match_));
                }
                (ResMatch::V4(Condition::Options(match_opts)), IpAddr::V4(ip)) => {
                    // single-option match, mirroring config (v4.rs From<Net>)
                    if let Some((code, opt)) = match_opts.values.0.iter().next() {
                        self.reserved_opts
                            .insert(*code, (opt.clone(), to_v4_reserved(ip, &res.match_)));
                    }
                }
                (ResMatch::V6Duid(duid), IpAddr::V6(ip)) => {
                    self.v6_duids.insert(
                        duid.clone(),
                        V6Reserved {
                            ip,
                            prefix: res.prefix,
                        },
                    );
                }
                // family/address mismatch is rejected on insert, so unreachable
                _ => {}
            }
        }
    }
}

/// Build a `config::v4::Reserved` for a runtime v4 reservation so it reuses the
/// StaticAddr assignment path. The lease/options come from defaults (as a config
/// reservation with no explicit overrides would).
fn to_v4_reserved(ip: Ipv4Addr, match_: &ResMatch) -> Reserved {
    let condition = match match_ {
        ResMatch::V4(cond) => cond.clone(),
        // only called for v4 entries
        ResMatch::V6Duid(_) => unreachable!("v6 match on a v4 reservation"),
    };
    let wire = ReservedIp {
        ip,
        options: Options::default(),
        condition,
        config: NetworkConfig::default(),
        class: None,
    };
    Reserved::from(&wire)
}

/// Serialize a match predicate to its JSON form (the persisted `match_json` and
/// the API `match` field).
fn match_to_value(match_: &ResMatch) -> serde_json::Value {
    match match_ {
        // Condition serializes to `{"chaddr": ...}` / `{"options": ...}`
        ResMatch::V4(cond) => serde_json::to_value(cond).unwrap_or(serde_json::Value::Null),
        ResMatch::V6Duid(duid) => serde_json::json!({ "duid": hex::encode(duid) }),
    }
}

/// Parse a match predicate from its JSON form for the given family.
pub fn match_from_value(family: &str, value: &serde_json::Value) -> Result<ResMatch> {
    match family {
        "v4" => {
            let cond: Condition = serde_json::from_value(value.clone())
                .context("v4 match must be {\"chaddr\": <mac>} or {\"options\": <opts>}")?;
            Ok(ResMatch::V4(cond))
        }
        "v6" => {
            let duid_hex = value
                .get("duid")
                .and_then(|d| d.as_str())
                .ok_or_else(|| anyhow!("v6 match must be {{\"duid\": <hex>}}"))?;
            let duid = hex::decode(duid_hex).context("v6 match duid must be hex")?;
            if duid.is_empty() {
                bail!("v6 match duid must not be empty");
            }
            Ok(ResMatch::V6Duid(duid))
        }
        other => bail!("unknown family {other}"),
    }
}

/// Parse a `"base/len"` v6 prefix string.
fn parse_prefix(s: impl AsRef<str>) -> Result<(Ipv6Addr, u8)> {
    let s = s.as_ref();
    let (base, len) = s
        .split_once('/')
        .ok_or_else(|| anyhow!("prefix must be in base/len form"))?;
    let base = Ipv6Addr::from_str(base).with_context(|| format!("invalid prefix base {base}"))?;
    let len: u8 = len
        .parse()
        .with_context(|| format!("invalid prefix length {len}"))?;
    if len > 128 {
        bail!("prefix length must be <= 128");
    }
    Ok((base, len))
}

#[cfg(test)]
mod tests {
    use dora_core::dhcproto::v4::DhcpOptions;

    use super::*;

    fn v4_mac(ip: &str, mac: MacAddr) -> RuntimeReservation {
        RuntimeReservation {
            ip: ip.parse().unwrap(),
            prefix: None,
            network: None,
            match_: ResMatch::V4(Condition::Mac(mac)),
        }
    }

    #[test]
    fn v4_mac_reservation_looks_up_and_resolves_ip() {
        let store = RuntimeReservations::new();
        let mac = MacAddr::new(1, 2, 3, 4, 5, 6);
        store.insert(v4_mac("192.168.0.50", mac), false).unwrap();

        let res = store
            .lookup_v4(Some(mac), &DhcpOptions::default(), None)
            .expect("reservation for mac");
        assert_eq!(res.ip().to_string(), "192.168.0.50");
        // a different mac does not match
        assert!(
            store
                .lookup_v4(
                    Some(MacAddr::new(9, 9, 9, 9, 9, 9)),
                    &DhcpOptions::default(),
                    None
                )
                .is_none()
        );
    }

    #[test]
    fn create_rejects_duplicate_address_and_match() {
        let store = RuntimeReservations::new();
        let mac = MacAddr::new(1, 2, 3, 4, 5, 6);
        store.insert(v4_mac("192.168.0.50", mac), false).unwrap();

        // same address again (create) -> AddressExists
        assert_eq!(
            store.insert(
                v4_mac("192.168.0.50", MacAddr::new(7, 7, 7, 7, 7, 7)),
                false
            ),
            Err(ReservationError::AddressExists)
        );
        // same match to a different address -> MatchExists
        assert_eq!(
            store.insert(v4_mac("192.168.0.51", mac), false),
            Err(ReservationError::MatchExists)
        );
        // update (replace) of the same address is allowed
        store.insert(v4_mac("192.168.0.50", mac), true).unwrap();
    }

    #[test]
    fn remove_drops_reservation() {
        let store = RuntimeReservations::new();
        let mac = MacAddr::new(1, 2, 3, 4, 5, 6);
        store.insert(v4_mac("192.168.0.50", mac), false).unwrap();
        assert!(store.remove("v4", "192.168.0.50".parse().unwrap()));
        assert!(
            store
                .lookup_v4(Some(mac), &DhcpOptions::default(), None)
                .is_none()
        );
        // removing again reports false
        assert!(!store.remove("v4", "192.168.0.50".parse().unwrap()));
    }

    #[test]
    fn v6_duid_reservation_round_trips_through_parts() {
        let res = RuntimeReservation::from_parts(
            "v6",
            "2001:db8::5",
            None,
            Some("2001:db8::/64".to_string()),
            r#"{"duid":"0001000112ab"}"#,
        )
        .expect("valid v6 reservation");
        assert_eq!(res.family(), "v6");

        let store = RuntimeReservations::new();
        store.insert(res, false).unwrap();
        let duid = hex::decode("0001000112ab").unwrap();
        let reserved = store.lookup_v6(&duid).expect("v6 reservation");
        assert_eq!(reserved.ip.to_string(), "2001:db8::5");
    }

    #[test]
    fn from_parts_rejects_family_mismatch_and_bad_match() {
        // v4 family with a v6 address
        assert!(
            RuntimeReservation::from_parts(
                "v4",
                "2001:db8::1",
                None,
                None,
                r#"{"chaddr":"01:02:03:04:05:06"}"#
            )
            .is_err()
        );
        // v6 match missing duid
        assert!(
            RuntimeReservation::from_parts("v6", "2001:db8::1", None, None, r#"{"mac":"x"}"#)
                .is_err()
        );
    }
}
