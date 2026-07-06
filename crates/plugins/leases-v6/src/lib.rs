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
        self, DhcpOption, DhcpOptions, IAAddr, IANA, MessageType, OptionCode, Status, StatusCode,
    },
    prelude::*,
    tracing::warn,
};

use config::{DhcpConfig, v6::Network};
use ip_manager::{IpManager, IpState, Storage};
use message_type::MsgType;
use register_derive::Register;

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

        // whether this exchange commits a lease vs. merely offers one. MsgType
        // has already set the response type, so a Reply means commit (Request, or
        // a Rapid-Commit Solicit); a plain Solicit yields an Advertise (offer).
        let commit = match msg_type {
            MessageType::Solicit => {
                matches!(ctx.resp_msg().map(|m| m.msg_type()), Some(MessageType::Reply))
            }
            MessageType::Request => true,
            // InformationRequest is answered by MsgType; Renew/Rebind/Confirm/
            // Release/Decline are a later phase.
            _ => return Ok(Action::Continue),
        };

        // pull the client DUID (opt 1, mandatory) and the requested IA_NAs
        let (duid, ianas) = {
            let opts = ctx.msg().opts();
            let duid = match opts.get(OptionCode::ClientId) {
                Some(DhcpOption::ClientId(d)) => d.clone(),
                _ => {
                    debug!("v6 message has no Client Identifier; not responding");
                    return Ok(Action::NoResponse);
                }
            };
            let ianas: Vec<IANA> = opts
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
            (duid, ianas)
        };

        let network = match self.cfg.v6().get_network(meta.ifindex) {
            Some(net) => net,
            None => {
                warn!(ifindex = meta.ifindex, "no v6 network configured for interface");
                return Ok(Action::NoResponse);
            }
        };

        let valid = network.valid().determine_lease(None).0;
        let preferred = network.preferred().determine_lease(None).0;
        let state = if commit {
            IpState::Lease
        } else {
            IpState::Reserve
        };
        let expires_at = SystemTime::now() + valid;

        // allocate an address for each requested IA_NA
        let mut ia_opts = Vec::with_capacity(ianas.len());
        for iana in &ianas {
            let id = binding_id(&duid, iana.id);
            match self.allocate(network, &id, expires_at, state).await {
                Some(addr) => {
                    debug!(?addr, iaid = iana.id, commit, "assigned v6 address");
                    ia_opts.push(iana_with_addr(iana.id, addr, preferred, valid));
                }
                None => {
                    warn!(iaid = iana.id, "no v6 address available for IA_NA");
                    ia_opts.push(iana_no_addrs(iana.id));
                }
            }
        }

        // write the IA_NA options onto the response set up by MsgType
        let resp = ctx
            .resp_msg_mut()
            .context("v6 response must be set by MsgType before leases-v6 runs")?;
        for opt in ia_opts {
            resp.opts_mut().insert(opt);
        }
        // copy the client id and any ORO-requested options (DNS, etc.)
        ctx.populate_opts(network.opts());
        Ok(Action::Respond)
    }
}

impl<S> LeasesV6<S>
where
    S: Storage + Send + Sync + 'static,
{
    /// allocate the first available address across the network's IA_NA pools for
    /// this binding id, reusing the client's existing address if it already has
    /// one (reserve_first matches on the id).
    async fn allocate(
        &self,
        network: &Network,
        id: &[u8],
        expires_at: SystemTime,
        state: IpState,
    ) -> Option<Ipv6Addr> {
        for range in network.ranges() {
            match self
                .ip_mgr
                .reserve_first(range, network, id, expires_at, Some(state))
                .await
            {
                Ok(IpAddr::V6(ip)) => return Some(ip),
                Ok(IpAddr::V4(_)) => continue, // never for a v6 range
                Err(err) => {
                    debug!(?err, "v6 range could not allocate an address, trying next");
                    continue;
                }
            }
        }
        None
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
        t1: config::renew(valid).as_secs() as u32,
        t2: config::rebind(valid).as_secs() as u32,
        opts,
    })
}

/// build an IA_NA (opt 3) carrying a NoAddrsAvail status (RFC 8415 §18.3.1).
fn iana_no_addrs(iaid: u32) -> DhcpOption {
    let mut opts = DhcpOptions::new();
    opts.insert(DhcpOption::StatusCode(StatusCode {
        status: Status::NoAddrsAvail,
        msg: "no addresses available".to_owned(),
    }));
    DhcpOption::IANA(IANA {
        id: iaid,
        t1: 0,
        t2: 0,
        opts,
    })
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
        assert_eq!(iana.t1, 1800); // renew  = valid / 2
        assert_eq!(iana.t2, 3150); // rebind = valid * 7/8

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
}
