#![warn(
    missing_debug_implementations,
    rust_2018_idioms,
    unreachable_pub,
    non_snake_case,
    non_upper_case_globals
)]
#![deny(rustdoc::broken_intra_doc_links)]

//! DHCPv6 stateful address assignment (IA_NA) — RFC 8415.
//!
//! Runs after [`message_type::MsgType`] on the v6 server. `MsgType` sets the
//! response message type (Advertise for a plain Solicit, Reply for Request or a
//! Rapid-Commit Solicit); this plugin allocates an address for each IA_NA the
//! client requested and fills in the IA_NA / IAADDR options.

use std::{
    fmt,
    net::{IpAddr, Ipv6Addr},
    time::{Duration, SystemTime},
};

use dora_core::{
    dhcproto::v6::{
        self, DhcpOption, DhcpOptions, IAAddr, IANA, IAPD, IAPrefix, MessageType, OptionCode,
        Status, StatusCode,
    },
    prelude::*,
    tracing::warn,
};

use config::{DhcpConfig, v6::Network};
use ip_manager::{IpManager, IpState, Storage};
use message_type::MsgType;
use register_derive::Register;

/// how long an offered-but-uncommitted (Advertise) binding is held before it can
/// be reclaimed, so Solicit-only clients can't exhaust the pool by never sending
/// a Request. Committed leases persist for their full valid lifetime.
const OFFER_WINDOW: Duration = Duration::from_secs(60);

#[derive(Register)]
#[register(msg(v6::Message))]
#[register(plugin(MsgType))]
pub struct LeasesV6<S>
where
    S: Storage,
{
    cfg: Arc<DhcpConfig>,
    ip_mgr: Arc<IpManager<S>>,
}

impl<S: Storage> fmt::Debug for LeasesV6<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeasesV6").finish()
    }
}

impl<S: Storage> LeasesV6<S> {
    pub fn new(cfg: Arc<DhcpConfig>, ip_mgr: Arc<IpManager<S>>) -> Self {
        Self { cfg, ip_mgr }
    }
}

#[async_trait]
impl<S> Plugin<v6::Message> for LeasesV6<S>
where
    S: Storage + Send + Sync + 'static,
{
    #[instrument(level = "debug", skip_all)]
    async fn handle(&self, ctx: &mut MsgContext<v6::Message>) -> Result<Action> {
        let meta = ctx.meta();
        let msg_type = ctx.msg().msg_type();

        // only the stateful IA_NA exchanges are ours; anything else (e.g.
        // InformationRequest, answered by MsgType) continues down the chain.
        if !matches!(
            msg_type,
            MessageType::Solicit
                | MessageType::Request
                | MessageType::Renew
                | MessageType::Rebind
                | MessageType::Confirm
                | MessageType::Release
                | MessageType::Decline
        ) {
            return Ok(Action::Continue);
        }

        // client DUID (opt 1, mandatory), requested IA_NAs (opt 3) and IA_PDs (opt 25)
        let (duid, ianas, iapds) = match extract_client(ctx.msg()) {
            Some(v) => v,
            None => {
                debug!("v6 message has no Client Identifier; not responding");
                return Ok(Action::NoResponse);
            }
        };

        // relayed messages select the subnet by the relay's link-address; direct
        // messages use the receiving interface (RFC 8415 §13.1).
        let client_link = ctx.relay().and_then(|c| c.client_link());
        let network = match client_link {
            Some(link) => self.cfg.v6().get_network_by_addr(link),
            None => self.cfg.v6().get_network(meta.ifindex),
        };
        let network = match network {
            Some(net) => net,
            None => {
                warn!(
                    ifindex = meta.ifindex,
                    ?client_link,
                    "no v6 network for interface / relay link"
                );
                return Ok(Action::NoResponse);
            }
        };

        match msg_type {
            MessageType::Solicit | MessageType::Request => {
                self.assign(ctx, msg_type, &duid, &ianas, &iapds, network).await
            }
            MessageType::Renew | MessageType::Rebind => {
                self.renew(ctx, msg_type, &duid, &ianas, &iapds, network).await
            }
            // Confirm/Decline apply to addresses, not delegated prefixes
            MessageType::Confirm => self.confirm(ctx, &ianas, network).await,
            MessageType::Release => self.release(ctx, &duid, &ianas, &iapds, network).await,
            MessageType::Decline => self.decline(ctx, &duid, &ianas, network).await,
            _ => Ok(Action::Continue),
        }
    }
}

impl<S> LeasesV6<S>
where
    S: Storage + Send + Sync + 'static,
{
    /// Solicit -> Advertise (offer) / Request or Rapid-Commit Solicit -> Reply
    /// (commit): assign an address for each requested IA_NA.
    async fn assign(
        &self,
        ctx: &mut MsgContext<v6::Message>,
        msg_type: MessageType,
        duid: &[u8],
        ianas: &[IANA],
        iapds: &[IAPD],
        network: &Network,
    ) -> Result<Action> {
        // MsgType already set the response type: a Reply means commit (Request,
        // or a Rapid-Commit Solicit); a plain Solicit gives an Advertise (offer).
        let commit = match msg_type {
            MessageType::Solicit => {
                matches!(ctx.resp_msg().map(|m| m.msg_type()), Some(MessageType::Reply))
            }
            _ => true,
        };
        let state = if commit {
            IpState::Lease
        } else {
            IpState::Reserve
        };

        let mut ia_opts = Vec::with_capacity(ianas.len() + iapds.len());
        // IA_NA: assign an address per IA (with that pool's lifetimes)
        for iana in ianas {
            let id = binding_id(duid, iana.id);
            match self.allocate(network, &id, state).await {
                Some((addr, preferred, valid)) => {
                    debug!(?addr, iaid = iana.id, commit, "assigned v6 address");
                    ia_opts.push(iana_with_addr(iana.id, addr, preferred, valid));
                }
                None => {
                    warn!(iaid = iana.id, "no v6 address available for IA_NA");
                    ia_opts.push(iana_no_addrs(iana.id));
                }
            }
        }
        // IA_PD: delegate a prefix per IA (with that pool's lifetimes)
        for iapd in iapds {
            let id = binding_id(duid, iapd.id);
            match self.allocate_prefix(network, &id, state).await {
                Some((prefix, plen, preferred, valid)) => {
                    debug!(?prefix, plen, iaid = iapd.id, commit, "delegated v6 prefix");
                    ia_opts.push(iapd_with_prefix(iapd.id, prefix, plen, preferred, valid));
                }
                None => {
                    warn!(iaid = iapd.id, "no v6 prefix available for IA_PD");
                    ia_opts.push(iapd_no_prefix(iapd.id));
                }
            }
        }
        self.write_response(ctx, network, ia_opts)
    }

    /// Renew (unicast) / Rebind (any server): extend the client's existing
    /// bindings. RFC 8415 §18.3.4 / §18.3.5. Unknown bindings get NoBinding;
    /// a Rebind that matches nothing stays silent.
    async fn renew(
        &self,
        ctx: &mut MsgContext<v6::Message>,
        msg_type: MessageType,
        duid: &[u8],
        ianas: &[IANA],
        iapds: &[IAPD],
        network: &Network,
    ) -> Result<Action> {
        let mut ia_opts = Vec::with_capacity(ianas.len() + iapds.len());
        let mut any_extended = false;
        let is_renew = msg_type == MessageType::Renew;

        // IA_NA: extend addresses (with that pool's lifetimes)
        for iana in ianas {
            let id = binding_id(duid, iana.id);
            let mut extended = None;
            for addr in iana_addrs(iana) {
                let (preferred, valid) = na_lifetimes(network, addr);
                let expires_at = SystemTime::now() + valid;
                if let Ok(Some(IpAddr::V6(ip))) =
                    self.ip_mgr.renew(IpAddr::V6(addr), &id, expires_at).await
                {
                    extended = Some((ip, preferred, valid));
                    break;
                }
            }
            match extended {
                Some((addr, preferred, valid)) => {
                    any_extended = true;
                    debug!(?addr, iaid = iana.id, "extended v6 lease");
                    ia_opts.push(iana_with_addr(iana.id, addr, preferred, valid));
                }
                // Renew: reply NoBinding for an IA we don't have (§18.3.4).
                // Rebind: omit the IA entirely — NoBinding is not valid there
                // (§18.3.5); the client keeps trying other servers.
                None if is_renew => ia_opts.push(iana_status(
                    iana.id,
                    Status::NoBinding,
                    "no binding for this IA",
                )),
                None => {}
            }
        }

        // IA_PD: extend delegated prefixes (with that pool's lifetimes)
        for iapd in iapds {
            let id = binding_id(duid, iapd.id);
            let mut extended = None;
            for (prefix, plen) in iapd_prefixes(iapd) {
                let (preferred, valid) = pd_lifetimes(network, prefix);
                let expires_at = SystemTime::now() + valid;
                if let Ok(Some(IpAddr::V6(ip))) =
                    self.ip_mgr.renew_pd(IpAddr::V6(prefix), plen, &id, expires_at).await
                {
                    extended = Some((ip, plen, preferred, valid));
                    break;
                }
            }
            match extended {
                Some((prefix, plen, preferred, valid)) => {
                    any_extended = true;
                    debug!(?prefix, plen, iaid = iapd.id, "extended v6 prefix");
                    ia_opts.push(iapd_with_prefix(iapd.id, prefix, plen, preferred, valid));
                }
                None if is_renew => ia_opts.push(iapd_status(
                    iapd.id,
                    Status::NoBinding,
                    "no binding for this IA_PD",
                )),
                None => {}
            }
        }

        // A Rebind that matches no binding must not be answered (let another
        // server reply). Renew always answers.
        if !is_renew && !any_extended {
            debug!("Rebind matched no bindings; not responding");
            return Ok(Action::NoResponse);
        }
        self.write_response(ctx, network, ia_opts)
    }

    /// Confirm: tell the client whether its addresses are still on-link.
    /// RFC 8415 §18.3.3 — no leases are changed.
    async fn confirm(
        &self,
        ctx: &mut MsgContext<v6::Message>,
        ianas: &[IANA],
        network: &Network,
    ) -> Result<Action> {
        let addrs: Vec<Ipv6Addr> = ianas.iter().flat_map(iana_addrs).collect();
        // MUST NOT respond if the client sent no addresses to check
        if addrs.is_empty() {
            debug!("Confirm with no addresses; not responding");
            return Ok(Action::NoResponse);
        }
        let subnet = network.full_subnet();
        let (status, msg) = if addrs.iter().all(|a| subnet.contains(a)) {
            (Status::Success, "all addresses on-link")
        } else {
            (Status::NotOnLink, "address not on-link")
        };
        self.write_response(ctx, network, vec![status_code(status, msg)])
    }

    /// Release: free the client's addresses. RFC 8415 §18.3.7.
    async fn release(
        &self,
        ctx: &mut MsgContext<v6::Message>,
        duid: &[u8],
        ianas: &[IANA],
        iapds: &[IAPD],
        network: &Network,
    ) -> Result<Action> {
        // free addresses
        for iana in ianas {
            let id = binding_id(duid, iana.id);
            for addr in iana_addrs(iana) {
                if let Err(err) = self.ip_mgr.release_ip(IpAddr::V6(addr), &id).await {
                    debug!(?err, ?addr, "error releasing v6 address");
                }
            }
        }
        // free delegated prefixes
        for iapd in iapds {
            let id = binding_id(duid, iapd.id);
            for (prefix, plen) in iapd_prefixes(iapd) {
                if let Err(err) = self.ip_mgr.release_pd(IpAddr::V6(prefix), plen, &id).await {
                    debug!(?err, ?prefix, "error releasing v6 prefix");
                }
            }
        }
        self.write_response(ctx, network, vec![status_code(Status::Success, "released")])
    }

    /// Decline: the client found an address already in use; put it on probation.
    /// RFC 8415 §18.3.8.
    async fn decline(
        &self,
        ctx: &mut MsgContext<v6::Message>,
        duid: &[u8],
        ianas: &[IANA],
        network: &Network,
    ) -> Result<Action> {
        let expires_at = SystemTime::now() + network.probation_period();
        for iana in ianas {
            let id = binding_id(duid, iana.id);
            for addr in iana_addrs(iana) {
                if let Err(err) = self.ip_mgr.probate_ip(IpAddr::V6(addr), &id, expires_at).await {
                    debug!(?err, ?addr, "error probating declined v6 address");
                }
            }
        }
        self.write_response(ctx, network, vec![status_code(Status::Success, "declined")])
    }

    /// allocate the first available address across the network's IA_NA pools for
    /// this binding id, reusing the client's existing address if it already has
    /// one (reserve_first matches on the id). Returns the address with the
    /// allocating range's (preferred, valid) lifetimes.
    async fn allocate(
        &self,
        network: &Network,
        id: &[u8],
        state: IpState,
    ) -> Option<(Ipv6Addr, Duration, Duration)> {
        for range in network.ranges() {
            let preferred = range.preferred().determine_lease(None).0;
            let valid = range.valid().determine_lease(None).0;
            let expires_at = SystemTime::now() + db_ttl(state, valid);
            match self
                .ip_mgr
                .reserve_first(range, network, id, expires_at, Some(state))
                .await
            {
                Ok(IpAddr::V6(ip)) => return Some((ip, preferred, valid)),
                Ok(IpAddr::V4(_)) => continue, // never for a v6 range
                Err(err) => {
                    debug!(?err, "v6 range could not allocate an address, trying next");
                    continue;
                }
            }
        }
        None
    }

    /// delegate a prefix from the network's pd_pools for this binding id. Returns
    /// the prefix with the allocating pool's (preferred, valid) lifetimes.
    async fn allocate_prefix(
        &self,
        network: &Network,
        id: &[u8],
        state: IpState,
    ) -> Option<(Ipv6Addr, u8, Duration, Duration)> {
        let subnet = IpAddr::V6(network.subnet());
        for pool in network.pd_pools() {
            let preferred = pool.preferred().determine_lease(None).0;
            let valid = pool.valid().determine_lease(None).0;
            let expires_at = SystemTime::now() + db_ttl(state, valid);
            match self
                .ip_mgr
                .allocate_pd(pool, subnet, id, expires_at, state)
                .await
            {
                Ok(Some((prefix, plen))) => return Some((prefix, plen, preferred, valid)),
                Ok(None) => continue,
                Err(err) => {
                    debug!(?err, "pd pool could not delegate a prefix, trying next");
                    continue;
                }
            }
        }
        None
    }

    /// write the built options onto the MsgType-provided response, copy the
    /// client id + ORO-requested options, and respond.
    fn write_response(
        &self,
        ctx: &mut MsgContext<v6::Message>,
        network: &Network,
        opts: Vec<DhcpOption>,
    ) -> Result<Action> {
        let resp = ctx
            .resp_msg_mut()
            .context("v6 response must be set by MsgType before leases-v6 runs")?;
        for opt in opts {
            resp.opts_mut().insert(opt);
        }
        ctx.populate_opts(network.opts());
        Ok(Action::Respond)
    }
}

/// binding identity for a DHCPv6 IA: the client DUID followed by the 4-byte IAID.
fn binding_id(duid: &[u8], iaid: u32) -> Vec<u8> {
    let mut id = Vec::with_capacity(duid.len() + 4);
    id.extend_from_slice(duid);
    id.extend_from_slice(&iaid.to_be_bytes());
    id
}

/// build an IA_NA (opt 3) carrying a single assigned address (IAADDR, opt 5).
fn iana_with_addr(iaid: u32, addr: Ipv6Addr, preferred: Duration, valid: Duration) -> DhcpOption {
    let mut opts = DhcpOptions::new();
    opts.insert(DhcpOption::IAAddr(IAAddr {
        addr,
        preferred_life: preferred.as_secs() as u32,
        valid_life: valid.as_secs() as u32,
        opts: DhcpOptions::new(),
    }));
    DhcpOption::IANA(IANA {
        id: iaid,
        // T1/T2 are based on the PREFERRED lifetime so the client renews before
        // the address is deprecated (RFC 8415 §21.4), not the valid lifetime.
        t1: config::renew(preferred).as_secs() as u32,
        t2: config::rebind(preferred).as_secs() as u32,
        opts,
    })
}

/// build an IA_NA (opt 3) carrying only a status code (e.g. NoAddrsAvail,
/// NoBinding). RFC 8415 §21.13.
fn iana_status(iaid: u32, status: Status, msg: &str) -> DhcpOption {
    let mut opts = DhcpOptions::new();
    opts.insert(status_code(status, msg));
    DhcpOption::IANA(IANA {
        id: iaid,
        t1: 0,
        t2: 0,
        opts,
    })
}

/// build an IA_NA (opt 3) carrying a NoAddrsAvail status (RFC 8415 §18.3.1).
fn iana_no_addrs(iaid: u32) -> DhcpOption {
    iana_status(iaid, Status::NoAddrsAvail, "no addresses available")
}

/// a top-level Status Code option (opt 13).
fn status_code(status: Status, msg: &str) -> DhcpOption {
    DhcpOption::StatusCode(StatusCode {
        status,
        msg: msg.to_owned(),
    })
}

/// pull the mandatory client DUID (opt 1), the requested IA_NAs (opt 3) and
/// IA_PDs (opt 25) from a request. Returns `None` if there is no Client
/// Identifier.
fn extract_client(msg: &v6::Message) -> Option<(Vec<u8>, Vec<IANA>, Vec<IAPD>)> {
    let opts = msg.opts();
    let duid = match opts.get(OptionCode::ClientId) {
        Some(DhcpOption::ClientId(d)) => d.clone(),
        _ => return None,
    };
    let ianas = opts
        .get_all(OptionCode::IANA)
        .map(|os| {
            os.iter()
                .filter_map(|o| match o {
                    DhcpOption::IANA(iana) => Some(iana.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let iapds = opts
        .get_all(OptionCode::IAPD)
        .map(|os| {
            os.iter()
                .filter_map(|o| match o {
                    DhcpOption::IAPD(iapd) => Some(iapd.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Some((duid, ianas, iapds))
}

/// the (preferred, valid) lifetimes configured at the network level.
fn lifetimes(network: &Network) -> (Duration, Duration) {
    (
        network.preferred().determine_lease(None).0,
        network.valid().determine_lease(None).0,
    )
}

/// DB reservation time-to-live: the full valid lifetime for a committed lease,
/// but only a short offer window (capped at valid) for an offered reservation so
/// an un-Requested address is reclaimed quickly.
fn db_ttl(state: IpState, valid: Duration) -> Duration {
    if state == IpState::Lease {
        valid
    } else {
        OFFER_WINDOW.min(valid)
    }
}

/// (preferred, valid) for an IA_NA address: the containing range's lifetimes if
/// the address falls in a configured range, else the network default.
fn na_lifetimes(network: &Network, addr: Ipv6Addr) -> (Duration, Duration) {
    match network.range(addr) {
        Some(r) => (
            r.preferred().determine_lease(None).0,
            r.valid().determine_lease(None).0,
        ),
        None => lifetimes(network),
    }
}

/// (preferred, valid) for a delegated prefix: the containing pd_pool's lifetimes
/// if the prefix falls in a configured pool, else the network default.
fn pd_lifetimes(network: &Network, prefix: Ipv6Addr) -> (Duration, Duration) {
    for pool in network.pd_pools() {
        if pool.prefix().contains(&prefix) {
            return (
                pool.preferred().determine_lease(None).0,
                pool.valid().determine_lease(None).0,
            );
        }
    }
    lifetimes(network)
}

/// the addresses a client listed inside an IA_NA (its IAADDR sub-options).
fn iana_addrs(iana: &IANA) -> Vec<Ipv6Addr> {
    iana.opts
        .get_all(OptionCode::IAAddr)
        .map(|os| {
            os.iter()
                .filter_map(|o| match o {
                    DhcpOption::IAAddr(a) => Some(a.addr),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// build an IA_PD (opt 25) carrying a single delegated prefix (IAPREFIX, opt 26).
fn iapd_with_prefix(
    iaid: u32,
    prefix: Ipv6Addr,
    prefix_len: u8,
    preferred: Duration,
    valid: Duration,
) -> DhcpOption {
    let mut opts = DhcpOptions::new();
    opts.insert(DhcpOption::IAPrefix(IAPrefix {
        prefix_ip: prefix,
        prefix_len,
        preferred_lifetime: preferred.as_secs() as u32,
        valid_lifetime: valid.as_secs() as u32,
        opts: DhcpOptions::new(),
    }));
    DhcpOption::IAPD(IAPD {
        id: iaid,
        // T1/T2 from the PREFERRED lifetime (RFC 8415 §21.4)
        t1: config::renew(preferred).as_secs() as u32,
        t2: config::rebind(preferred).as_secs() as u32,
        opts,
    })
}

/// build an IA_PD (opt 25) carrying only a status code (NoPrefixAvail, NoBinding).
fn iapd_status(iaid: u32, status: Status, msg: &str) -> DhcpOption {
    let mut opts = DhcpOptions::new();
    opts.insert(status_code(status, msg));
    DhcpOption::IAPD(IAPD {
        id: iaid,
        t1: 0,
        t2: 0,
        opts,
    })
}

/// build an IA_PD (opt 25) carrying a NoPrefixAvail status (RFC 8415 §18.3.1).
fn iapd_no_prefix(iaid: u32) -> DhcpOption {
    iapd_status(iaid, Status::NoPrefixAvail, "no prefixes available")
}

/// the prefixes a client listed inside an IA_PD (its IAPREFIX sub-options).
fn iapd_prefixes(iapd: &IAPD) -> Vec<(Ipv6Addr, u8)> {
    iapd.opts
        .get_all(OptionCode::IAPrefix)
        .map(|os| {
            os.iter()
                .filter_map(|o| match o {
                    DhcpOption::IAPrefix(p) => Some((p.prefix_ip, p.prefix_len)),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binding_id_is_duid_plus_iaid() {
        // DUID bytes followed by the 4-byte big-endian IAID
        let id = binding_id(&[0xaa, 0xbb], 0x0102_0304);
        assert_eq!(id, vec![0xaa, 0xbb, 0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn test_iana_with_addr_carries_iaaddr_and_times() {
        let addr: Ipv6Addr = "2001:db8:1::100".parse().unwrap();
        let opt = iana_with_addr(
            42,
            addr,
            Duration::from_secs(1800), // preferred
            Duration::from_secs(3600), // valid
        );
        let DhcpOption::IANA(iana) = opt else {
            panic!("expected IANA");
        };
        assert_eq!(iana.id, 42);
        // T1/T2 derive from the PREFERRED lifetime (1800), not valid (3600)
        assert_eq!(iana.t1, 900); // renew  = preferred / 2
        assert_eq!(iana.t2, 1575); // rebind = preferred * 7/8

        let Some(DhcpOption::IAAddr(a)) = iana.opts.get(OptionCode::IAAddr) else {
            panic!("IA_NA must contain an IAADDR");
        };
        assert_eq!(a.addr, addr);
        assert_eq!(a.preferred_life, 1800);
        assert_eq!(a.valid_life, 3600);
    }

    #[test]
    fn test_iana_no_addrs_has_status_code() {
        let DhcpOption::IANA(iana) = iana_no_addrs(7) else {
            panic!("expected IANA");
        };
        assert_eq!(iana.id, 7);
        let Some(DhcpOption::StatusCode(sc)) = iana.opts.get(OptionCode::StatusCode) else {
            panic!("empty IA_NA must carry a StatusCode");
        };
        assert_eq!(sc.status, Status::NoAddrsAvail);
    }

    #[test]
    fn test_iana_addrs_round_trips() {
        // the addresses inside an IA_NA are read back out by iana_addrs
        let addr: Ipv6Addr = "2001:db8::9".parse().unwrap();
        let DhcpOption::IANA(iana) =
            iana_with_addr(1, addr, Duration::from_secs(10), Duration::from_secs(20))
        else {
            panic!("expected IANA");
        };
        assert_eq!(iana_addrs(&iana), vec![addr]);
    }

    #[test]
    fn test_iana_status_carries_nobinding() {
        let DhcpOption::IANA(iana) = iana_status(3, Status::NoBinding, "gone") else {
            panic!("expected IANA");
        };
        let Some(DhcpOption::StatusCode(sc)) = iana.opts.get(OptionCode::StatusCode) else {
            panic!("expected StatusCode");
        };
        assert_eq!(sc.status, Status::NoBinding);
    }

    #[test]
    fn test_status_code_is_top_level() {
        let DhcpOption::StatusCode(sc) = status_code(Status::Success, "ok") else {
            panic!("expected StatusCode");
        };
        assert_eq!(sc.status, Status::Success);
    }

    #[test]
    fn test_iapd_with_prefix_round_trips() {
        let prefix: Ipv6Addr = "2001:db8:100::".parse().unwrap();
        let DhcpOption::IAPD(iapd) = iapd_with_prefix(
            5,
            prefix,
            64,
            Duration::from_secs(1800),
            Duration::from_secs(3600),
        ) else {
            panic!("expected IA_PD");
        };
        assert_eq!(iapd.id, 5);
        // T1/T2 from the PREFERRED lifetime (1800), not valid (3600)
        assert_eq!(iapd.t1, 900);
        assert_eq!(iapd.t2, 1575);
        let Some(DhcpOption::IAPrefix(p)) = iapd.opts.get(OptionCode::IAPrefix) else {
            panic!("IA_PD must contain an IAPREFIX");
        };
        assert_eq!(p.prefix_ip, prefix);
        assert_eq!(p.prefix_len, 64);
        assert_eq!(p.valid_lifetime, 3600);
        // and iapd_prefixes reads it back
        assert_eq!(iapd_prefixes(&iapd), vec![(prefix, 64)]);
    }

    #[test]
    fn test_iapd_no_prefix_has_status() {
        let DhcpOption::IAPD(iapd) = iapd_no_prefix(9) else {
            panic!("expected IA_PD");
        };
        let Some(DhcpOption::StatusCode(sc)) = iapd.opts.get(OptionCode::StatusCode) else {
            panic!("empty IA_PD must carry a StatusCode");
        };
        assert_eq!(sc.status, Status::NoPrefixAvail);
    }
}
