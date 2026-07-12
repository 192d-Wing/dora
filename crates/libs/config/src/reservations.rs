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
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use dora_core::{
    dhcproto::v4::{DhcpOption, DhcpOptions, OptionCode},
    pnet::util::MacAddr,
};
use parking_lot::RwLock;

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
    /// DHCPv4 options handed to the matched client (v4 reservations only;
    /// empty for v6, whose reservations only pin an address/prefix)
    pub options: Options,
    /// restrict this reservation to a matched client class (v4 only)
    pub class: Option<String>,
    /// lease-time override in seconds (v4 only; `None` uses the range/default)
    pub lease_time: Option<u32>,
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

    /// For an option-match v4 reservation, the option code it is indexed by in
    /// the lookup table (only the first option is used, mirroring config).
    fn first_option_code(&self) -> Option<OptionCode> {
        match &self.match_ {
            ResMatch::V4(Condition::Options(opts)) => {
                opts.values.0.iter().next().map(|(code, _)| *code)
            }
            _ => None,
        }
    }

    /// Reconstruct a reservation from its persisted parts (as stored by
    /// `ip-manager`). `options_json` is the serialized v4 [`Options`]
    /// (`{"values": {...}}`); `class` and `lease_time` (seconds) are v4-only
    /// overrides. All three are ignored for v6 reservations, which only pin an
    /// address/prefix.
    // one arg per persisted column; grouping them into a struct would just move
    // the same fields around for no clarity gain
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        family: &str,
        ip: &str,
        prefix: Option<&str>,
        network: Option<String>,
        match_json: &str,
        options_json: Option<&str>,
        class: Option<String>,
        lease_time: Option<u32>,
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
        // options/class/lease only apply to v4 reservations
        let is_v4 = ip.is_ipv4();
        let options = match options_json {
            Some(json) if is_v4 => {
                serde_json::from_str(json).context("invalid reservation options json")?
            }
            _ => Options::default(),
        };
        Ok(Self {
            ip,
            prefix,
            network,
            match_,
            options,
            class: if is_v4 { class } else { None },
            lease_time: if is_v4 { lease_time } else { None },
        })
    }

    /// `true` when this reservation carries no options.
    fn options_empty(&self) -> bool {
        self.options.as_ref().iter().next().is_none()
    }

    /// The v4 options serialized to their persisted JSON form (`{"values":
    /// {...}}`), or `None` when there are no options to store.
    pub fn options_json(&self) -> Option<String> {
        if self.options_empty() {
            None
        } else {
            serde_json::to_string(&self.options).ok()
        }
    }

    /// The v4 options as a JSON value (`{"values": {...}}`) for API responses,
    /// or `None` when there are no options.
    pub fn options_value(&self) -> Option<serde_json::Value> {
        if self.options_empty() {
            None
        } else {
            serde_json::to_value(&self.options).ok()
        }
    }

    /// the configured class restriction, if any
    pub fn class(&self) -> Option<&str> {
        self.class.as_deref()
    }

    /// the lease-time override in seconds, if any
    pub fn lease_time(&self) -> Option<u32> {
        self.lease_time
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

    /// Replace the store's contents with `reservations` (used to warm the store
    /// on startup and to re-sync it from the database).
    ///
    /// This is a full replace, not a merge: `entries` is cleared first, so a
    /// reservation that was deleted from the source (e.g. via the API) is also
    /// dropped here. Without the clear, a periodic re-sync would only ever add
    /// rows and never remove deleted ones. `rebuild_indexes` then rebuilds the
    /// fast-path maps from the new `entries`.
    pub fn load(&self, reservations: impl IntoIterator<Item = RuntimeReservation>) {
        let mut inner = self.inner.write();
        inner.entries.clear();
        for res in reservations {
            inner
                .entries
                .insert((res.family().to_string(), res.ip), res);
        }
        inner.rebuild_indexes();
    }

    /// Insert a reservation, returning the entry it replaced (if any) so the
    /// caller can roll back on a later persistence failure. With `replace = false`
    /// (create) an existing address is [`ReservationError::AddressExists`]; with
    /// `replace = true` (update) a *missing* address is
    /// [`ReservationError::NotFound`]. Either way a match that already points at a
    /// different address — or, for an option match, reuses another reservation's
    /// option code — is [`ReservationError::MatchExists`].
    pub fn insert(
        &self,
        res: RuntimeReservation,
        replace: bool,
    ) -> std::result::Result<Option<RuntimeReservation>, ReservationError> {
        let mut inner = self.inner.write();
        let key = (res.family().to_string(), res.ip);
        let exists = inner.entries.contains_key(&key);
        if !replace && exists {
            return Err(ReservationError::AddressExists);
        }
        if replace && !exists {
            return Err(ReservationError::NotFound);
        }
        // reject a duplicate match that resolves to a different address, including
        // an option match that would collide on the same option code (the lookup
        // index is keyed by code, so two would silently shadow each other)
        let new_opt_code = res.first_option_code();
        for ((_, ip), e) in inner.entries.iter() {
            if *ip == res.ip {
                continue;
            }
            let same_match = e.match_ == res.match_;
            let same_opt_code = new_opt_code.is_some() && e.first_option_code() == new_opt_code;
            if same_match || same_opt_code {
                return Err(ReservationError::MatchExists);
            }
        }
        let replaced = inner.entries.insert(key, res);
        inner.rebuild_indexes();
        Ok(replaced)
    }

    /// Restore a previously-removed / replaced entry without conflict checks —
    /// used to roll back after a persistence failure.
    pub fn restore(&self, res: RuntimeReservation) {
        let mut inner = self.inner.write();
        inner
            .entries
            .insert((res.family().to_string(), res.ip), res);
        inner.rebuild_indexes();
    }

    /// Remove a reservation by (family, ip). Returns whether one was removed.
    pub fn remove(&self, family: &str, ip: IpAddr) -> bool {
        let mut inner = self.inner.write();
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
            .entries
            .contains_key(&(family.to_string(), ip))
    }

    /// All reservations, ordered by (family, ip).
    pub fn list(&self) -> Vec<RuntimeReservation> {
        self.inner.read().entries.values().cloned().collect()
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
        let inner = self.inner.read();
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
        self.inner.read().v6_duids.get(duid).cloned()
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
                    self.reserved_macs.insert(*mac, to_v4_reserved(ip, res));
                }
                (ResMatch::V4(Condition::Options(match_opts)), IpAddr::V4(ip)) => {
                    // single-option match, mirroring config (v4.rs From<Net>)
                    if let Some((code, opt)) = match_opts.values.0.iter().next() {
                        self.reserved_opts
                            .insert(*code, (opt.clone(), to_v4_reserved(ip, res)));
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
/// StaticAddr assignment path, carrying the reservation's options / class /
/// lease-time override (defaults where unset, as a config reservation would).
fn to_v4_reserved(ip: Ipv4Addr, res: &RuntimeReservation) -> Reserved {
    let condition = match &res.match_ {
        ResMatch::V4(cond) => cond.clone(),
        // only called for v4 entries
        ResMatch::V6Duid(_) => unreachable!("v6 match on a v4 reservation"),
    };
    // a lease-time override pins the lease to that single value (min == max ==
    // default, so a client-requested time clamps to it), matching a config
    // reservation that sets only `lease_time.default`.
    let config = match res.lease_time.and_then(std::num::NonZeroU32::new) {
        Some(default) => NetworkConfig {
            lease_time: crate::wire::MinMax {
                default,
                min: None,
                max: None,
            },
        },
        None => NetworkConfig::default(),
    };
    let wire = ReservedIp {
        ip,
        options: res.options.clone(),
        policy: None,
        condition,
        config,
        class: res.class.clone(),
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
            options: Options::default(),
            class: None,
            lease_time: None,
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
            None,
            None,
            None,
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
    fn v4_reservation_carries_options_class_lease() {
        let res = RuntimeReservation::from_parts(
            "v4",
            "192.168.0.50",
            None,
            None,
            r#"{"chaddr":"00:11:22:33:44:55"}"#,
            Some(r#"{"values":{"6":{"type":"ip","value":["1.2.3.4"]}}}"#),
            Some("foo".to_string()),
            Some(1800),
        )
        .expect("valid v4 reservation");
        // accessors + options round-trip through the persisted JSON form
        assert_eq!(res.class(), Some("foo"));
        assert_eq!(res.lease_time(), Some(1800));
        assert!(res.options_json().unwrap().contains("1.2.3.4"));

        let store = RuntimeReservations::new();
        store.insert(res, false).unwrap();
        let mac: MacAddr = "00:11:22:33:44:55".parse().unwrap();
        // class-gated: no match without the class, match with it
        assert!(
            store
                .lookup_v4(Some(mac), &DhcpOptions::new(), None)
                .is_none()
        );
        let reserved = store
            .lookup_v4(Some(mac), &DhcpOptions::new(), Some(&["foo".to_string()]))
            .expect("reserved");
        assert_eq!(reserved.ip().to_string(), "192.168.0.50");
        assert_eq!(
            reserved.opts().get(OptionCode::DomainNameServer),
            Some(&DhcpOption::DomainNameServer(vec![[1, 2, 3, 4].into()]))
        );
        assert_eq!(reserved.class(), Some("foo"));
        assert_eq!(reserved.lease().get_default().as_secs(), 1800);
    }

    /// v6 reservations ignore v4-only options/class/lease
    #[test]
    fn v6_reservation_ignores_v4_attrs() {
        let res = RuntimeReservation::from_parts(
            "v6",
            "2001:db8::9",
            None,
            None,
            r#"{"duid":"0001000112ab"}"#,
            Some(r#"{"values":{"6":{"type":"ip","value":["1.2.3.4"]}}}"#),
            Some("foo".to_string()),
            Some(1800),
        )
        .expect("valid v6 reservation");
        assert_eq!(res.class(), None);
        assert_eq!(res.lease_time(), None);
        assert_eq!(res.options_json(), None);
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
                r#"{"chaddr":"01:02:03:04:05:06"}"#,
                None,
                None,
                None,
            )
            .is_err()
        );
        // v6 match missing duid
        assert!(
            RuntimeReservation::from_parts(
                "v6",
                "2001:db8::1",
                None,
                None,
                r#"{"mac":"x"}"#,
                None,
                None,
                None,
            )
            .is_err()
        );
    }
}
