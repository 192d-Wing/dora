#![warn(
    missing_debug_implementations,
    // missing_docs, // we shall remove thee, someday!
    rust_2018_idioms,
    unreachable_pub,
    non_snake_case,
    non_upper_case_globals
)]
#![deny(rustdoc::broken_intra_doc_links)]
#![allow(clippy::cognitive_complexity)]

use client_protection::FloodCache;
use dora_core::{
    dhcproto::{
        v4::{DhcpOption, Message, MessageType, Opcode, OptionCode},
        v6,
    },
    metrics,
    mode::SharedMode,
    prelude::*,
    tracing::warn,
};
use register_derive::Register;
use std::{fmt::Debug, net::Ipv4Addr};

use config::{DhcpConfig, client_classes};

#[derive(Register)]
#[register(msg(Message))]
#[register(msg(v6::Message))]
#[register(plugin())]
pub struct MsgType {
    cfg: Arc<DhcpConfig>,
    flood: Option<FloodCache<Vec<u8>>>,
    /// shared server mode; when the server is draining / in maintenance /
    /// shutting down, new-lease (and, for maintenance, renewal) requests are
    /// dropped here. Defaults to `Normal`; the binary wires in the live handle
    /// via [`MsgType::with_mode`].
    mode: SharedMode,
}

impl Debug for MsgType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsgType")
            .field("cfg", &self.cfg)
            .field("mode", &self.mode.get())
            .finish()
    }
}

impl MsgType {
    pub fn new(cfg: Arc<DhcpConfig>) -> Result<Self> {
        Ok(Self {
            flood: cfg.v4().flood_threshold().map(FloodCache::new),
            cfg,
            mode: SharedMode::default(),
        })
    }

    /// Attach the shared server-mode handle so the datapath honors
    /// drain / maintenance / shutting-down modes set via the management API.
    pub fn with_mode(mut self, mode: SharedMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn flood_check(&self, id: &Vec<u8>) -> bool {
        self.flood
            .as_ref()
            .map(|flood| flood.is_allowed(id))
            .unwrap_or(true)
    }
}

#[async_trait]
impl Plugin<Message> for MsgType {
    #[instrument(level = "debug", skip_all)]
    async fn handle(&self, ctx: &mut MsgContext<Message>) -> Result<Action> {
        // set the interface, using data from config
        // MsgType plugin must run first because future plugins use this data
        let meta = ctx.meta();
        let interface = self
            .cfg
            .v4()
            .find_network(meta.ifindex)
            .context("interface message was received on does not exist?")?;
        ctx.set_interface(interface);

        let req = ctx.msg();
        let msg_type = req.opts().msg_type();

        let subnet = ctx.subnet()?;
        debug!(
            opcode = ?req.opcode(),
            msg_type = ?msg_type,
            src_addr = %ctx.src_addr(),
            ?subnet,
            req = %ctx.msg(),
        );

        // Honor the server mode before doing any work. Drain / maintenance /
        // shutting-down suppress new lease acquisition; maintenance additionally
        // suppresses renewals, taking the server fully out of service. New
        // acquisition is a DISCOVER or a SELECTING / INIT-REBOOT REQUEST;
        // renewals are RENEWING / REBINDING REQUESTs (classified by
        // `request_state`). Other message types (RELEASE, DECLINE, INFORM) are
        // always processed.
        let mode = self.mode.get();
        match msg_type {
            Some(MessageType::Discover) if mode.suppresses_new_leases() => {
                debug!(
                    ?mode,
                    "server mode suppresses new leases; dropping DISCOVER"
                );
                return Ok(Action::NoResponse);
            }
            Some(MessageType::Request) => {
                // RENEWING / REBINDING extend an existing lease; everything else
                // (SELECTING, INIT-REBOOT, and malformed/Unknown) is treated as a
                // new acquisition for the purpose of mode enforcement.
                let is_renewal = matches!(
                    ctx.request_state(),
                    RequestState::Renewing { .. } | RequestState::Rebinding { .. }
                );
                let suppress = if is_renewal {
                    mode.suppresses_renewals()
                } else {
                    mode.suppresses_new_leases()
                };
                if suppress {
                    debug!(
                        ?mode,
                        is_renewal, "server mode suppresses REQUEST; dropping"
                    );
                    return Ok(Action::NoResponse);
                }
            }
            _ => {}
        }

        let client_id = self.cfg.v4().client_id(req).to_vec(); // to_vec required b/c of borrowck error
        if !self.flood_check(&client_id) {
            metrics::FLOOD_THRESHOLD_COUNT.inc();
            debug!(
                ?client_id,
                "client is chatty, engaging rate limit and not responding"
            );
            return Ok(Action::NoResponse);
        }
        // otherwise our interface IP as the id
        let cfg_server_id = self
            .cfg
            .v4()
            .server_id(meta.ifindex, subnet)
            .context("cannot find server_id")?;
        // look up which network the message belongs to
        let network = self.cfg.v4().network(subnet);
        let sname = network.and_then(|net| net.server_name());
        let fname = network.and_then(|net| net.file_name());
        // message that will be returned
        let mut resp = util::new_msg(req, cfg_server_id, sname, fname);

        // determine the server id to use in the response message
        let resp_server_id = RespServerId::new(cfg_server_id, req);

        if let Some(server_id) = resp_server_id.get() {
            // add the correct server identifier to response
            resp.opts_mut()
                .insert(DhcpOption::ServerIdentifier(server_id));
        } else {
            debug!(
                ?cfg_server_id,
                "server identifier in msg doesn't match server address or server id override"
            );
            return Ok(Action::NoResponse);
        }
        if req.opcode() == Opcode::BootReply {
            debug!("BootReply not supported");
            return Ok(Action::NoResponse);
        }

        // evaluate client classes
        let matched = util::client_classes(self.cfg.v4(), ctx)?;
        let addr = {
            let ciaddr = ctx.msg().ciaddr();
            if !ciaddr.is_unspecified() {
                ciaddr
            } else {
                // TODO: when `subnet` is used to select a range, it probably doesn't exist.
                subnet
            }
        };
        let rapid_commit =
            ctx.msg().opts().get(OptionCode::RapidCommit).is_some() && self.cfg.v4().rapid_commit();

        match msg_type {
            Some(MessageType::Discover) if rapid_commit => {
                resp.opts_mut()
                    .insert(DhcpOption::MessageType(MessageType::Ack));
            }
            Some(MessageType::Discover) => {
                resp.opts_mut()
                    .insert(DhcpOption::MessageType(MessageType::Offer));
            }
            Some(MessageType::Request) => {
                if req.giaddr().is_unspecified() {
                    resp.set_flags(req.flags().set_broadcast());
                }
                resp.opts_mut()
                    .insert(DhcpOption::MessageType(MessageType::Ack));
            }
            Some(MessageType::Release) => {
                resp.opts_mut()
                    .insert(DhcpOption::MessageType(MessageType::Ack));
            }
            // INFORM & we are authoritative: answer with the client's local
            // configuration regardless of pool coverage (RFC 2131 §4.3.5). The
            // client already holds an address, so yiaddr stays 0 and no lease
            // time is added (populate_opts, not populate_opts_lease). Options
            // come from the range containing the client's address if there is
            // one, otherwise from class / interface-derived local config.
            Some(MessageType::Inform) if matches!(network, Some(net) if net.authoritative()) => {
                // a DROP class silences the client entirely -- INFORM included.
                // (The shared tail below runs this check for other message types,
                // but INFORM returns early, so it must be repeated here.)
                if matches!(&matched, Some(classes) if classes.iter().any(|c| c == client_classes::client_classification::DROP_CLASS))
                {
                    debug!("DROP class matched");
                    return Ok(Action::NoResponse);
                }
                resp.opts_mut()
                    .insert(DhcpOption::MessageType(MessageType::Ack));
                ctx.set_resp_msg(resp);
                let opts = match self.cfg.v4().range(addr, addr, matched.as_deref()) {
                    Some(range) => self.cfg.v4().collect_opts(range.opts(), matched.as_deref()),
                    None => {
                        debug!(
                            "INFORM address not in any configured range; answering with local config"
                        );
                        // no range to draw from, but global options still apply
                        self.cfg
                            .v4()
                            .collect_opts(self.cfg.v4().global_opts(), matched.as_deref())
                    }
                };
                ctx.populate_opts(&opts);
                if let Some(classes) = matched {
                    ctx.set_local(MatchedClasses(classes));
                }
                return Ok(Action::Respond);
            }
            Some(MessageType::Decline) => {
                if let Some(DhcpOption::RequestedIpAddress(ip)) =
                    req.opts().get(OptionCode::RequestedIpAddress)
                {
                    debug!(declined_ip = ?ip, "got DECLINE");
                    return Ok(Action::Continue);
                } else {
                    // TODO: is this a real case? AFAIK all declines must include the IP
                    error!("got DECLINE with no option 50 (requested IP)");
                    return Ok(Action::NoResponse);
                }
            }
            None if req.opcode() == Opcode::BootRequest && self.cfg.v4().bootp_enabled() => {
                // No message type but BOOTREQUEST, this is a BOOTP message
                ctx.set_resp_msg(resp);
                return Ok(Action::Continue);
            }
            _ => {
                debug!("unsupported message type");
                return Ok(Action::NoResponse);
            }
        }

        if let Some(classes) = matched {
            if classes
                .iter()
                .any(|class| class == client_classes::client_classification::DROP_CLASS)
            {
                // contains DROP class, drop packet
                debug!("DROP class matched");
                return Ok(Action::NoResponse);
            }
            ctx.set_local(MatchedClasses(classes));
        }
        ctx.set_resp_msg(resp);
        Ok(Action::Continue)
    }
}

/// supports 3 variants:
/// CfgServerId - the server id retrieved from the config
/// ServerIdOverride - the server id override retrieved from the RAI in the message
/// None - no valid server id, we should not process the message
enum RespServerId {
    CfgServerId(Ipv4Addr),
    ServerIdOverride(Ipv4Addr),
    None,
}

impl RespServerId {
    /// returns either the server id override or the server id from the config (RFC 5107)
    fn new(cfg_server_id: Ipv4Addr, req: &Message) -> Self {
        // get the server id and server id override from the message
        let server_id_override = util::get_server_id_override(req.opts());
        let msg_server_id_opt = req.opts().get(OptionCode::ServerIdentifier);

        if let Some(&DhcpOption::ServerIdentifier(msg_id)) = msg_server_id_opt {
            // if the server override matches the msg server id, we should respond
            if let Some(override_id) = server_id_override
                && override_id == msg_id
            {
                return Self::ServerIdOverride(override_id);
            }
            // we should not respond if the server id from the config does not match the msg server id and
            // the msg server id is not unspecified
            if cfg_server_id != msg_id && !msg_id.is_unspecified() {
                return Self::None;
            }
        }
        Self::CfgServerId(cfg_server_id)
    }

    fn get(&self) -> Option<Ipv4Addr> {
        match self {
            Self::CfgServerId(addr) => Some(*addr),
            Self::ServerIdOverride(addr) => Some(*addr),
            Self::None => None,
        }
    }
}

pub mod util {
    use config::{client_classes::client_classification::PacketDetails, v4::Config};

    use super::*;

    pub fn new_msg(
        req: &Message,
        siaddr: Ipv4Addr,
        sname: Option<&str>,
        fname: Option<&str>,
    ) -> Message {
        let mut msg = Message::new_with_id(
            req.xid(),
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            siaddr,
            req.giaddr(),
            req.chaddr(),
        );
        msg.set_opcode(Opcode::BootReply)
            .set_htype(req.htype())
            .set_flags(req.flags())
            .set_hops(req.hops());
        // set the sname & fname header fields
        if let Some(sname) = sname {
            msg.set_sname_str(sname);
        }
        if let Some(fname) = fname {
            msg.set_fname_str(fname);
        }
        msg
    }

    pub fn packet_details(cfg: &Config, meta: RecvMeta) -> Result<PacketDetails<'_>> {
        Ok(PacketDetails {
            iface: cfg
                .find_interface(meta.ifindex)
                .context("could not find interface")?
                .name
                .as_str(),
            src: match meta.addr.ip() {
                IpAddr::V4(ip) => ip,
                IpAddr::V6(_ip) => {
                    // this error shouldn't happen but we'll cover it anyway
                    return Err(anyhow::anyhow!(
                        "addr recvd an ipv6 address for ipv4 message"
                    ));
                }
            },
            dst: match meta.dst_ip.context("no destination ip on recvd message")? {
                IpAddr::V4(ip) => ip,
                IpAddr::V6(_ip) => {
                    return Err(anyhow::anyhow!(
                        "dst_ip recvd an ipv6 address for ipv4 message"
                    ));
                }
            },
            len: meta.len,
        })
    }

    pub fn client_classes(cfg: &Config, ctx: &MsgContext<Message>) -> Result<Option<Vec<String>>> {
        // TODO: what should we do if there is an error processing client classes?
        Ok(cfg
            .eval_client_classes(ctx.msg(), util::packet_details(cfg, ctx.meta())?)
            .and_then(|classes| match classes {
                Ok(classes) => {
                    debug!(matched_classes = ?classes, "matched classes");
                    Some(classes)
                }
                Err(err) => {
                    error!(?err, "error processing client classes");
                    None
                }
            }))
    }

    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use anyhow::Result;
    use dhcproto::{Encodable, v4};
    use dora_core::server::msg::SerialMsg;
    use unix_udp_sock::RecvMeta;

    /// for testing
    pub fn blank_ctx(
        recv_addr: SocketAddr,
        siaddr: Ipv4Addr,
        giaddr: Ipv4Addr,
        msg_type: v4::MessageType,
    ) -> Result<MsgContext<dhcproto::v4::Message>> {
        let uns = Ipv4Addr::UNSPECIFIED;
        let mut msg = dhcproto::v4::Message::new(uns, uns, siaddr, giaddr, &[1, 2, 3, 4, 5, 6]);
        msg.opts_mut().insert(v4::DhcpOption::MessageType(msg_type));
        msg.opts_mut()
            .insert(v4::DhcpOption::SubnetSelection(giaddr));
        msg.opts_mut()
            .insert(v4::DhcpOption::ParameterRequestList(vec![
                v4::OptionCode::SubnetMask,
                v4::OptionCode::Router,
                v4::OptionCode::DomainNameServer,
                v4::OptionCode::DomainName,
            ]));
        let buf = msg.to_vec().unwrap();
        let meta = RecvMeta {
            addr: recv_addr,
            len: buf.len(),
            ifindex: 1,
            // recv addr copied here
            dst_ip: Some(recv_addr.ip()),
            ..RecvMeta::default()
        };
        let resp = crate::util::new_msg(&msg, siaddr, None, None);
        let mut ctx: MsgContext<dhcproto::v4::Message> = MsgContext::new(
            SerialMsg::new(buf.into(), recv_addr),
            meta,
            Arc::new(State::new(10)),
        )?;
        ctx.set_resp_msg(resp);
        Ok(ctx)
    }

    /// Convenience for RFC 5107 compliance. Fetches the ServerIdentifierOverride suboption (11) from
    /// RelayAgentInformation (82) to use in comparisons between the server id and override id.
    pub fn get_server_id_override(opts: &v4::DhcpOptions) -> Option<Ipv4Addr> {
        // fetch the RelayAgentInformation option (option 82)
        if let Some(DhcpOption::RelayAgentInformation(relay_info)) =
            opts.get(OptionCode::RelayAgentInformation)
        {
            // fetch the ServerIdentifierOverride suboption (suboption 11) from the relay information
            let override_info = relay_info.get(v4::relay::RelayCode::ServerIdentifierOverride);
            if let Some(v4::relay::RelayInfo::ServerIdentifierOverride(addr)) = override_info {
                return Some(*addr);
            }
        }
        None
    }
}

#[async_trait]
impl Plugin<v6::Message> for MsgType {
    #[instrument(level = "debug", skip_all)]
    async fn handle(&self, ctx: &mut MsgContext<v6::Message>) -> Result<Action> {
        // message type variants (v6::MessageType is a newtype with associated
        // consts in usg-dhcproto 0.16, so they must be referenced by path)
        use v6::MessageType;
        // set the interface, using data from config
        // MsgType plugin must run first because future plugins use this data
        let meta = ctx.meta();
        // A relayed message identifies the client link via the relay's
        // link-address, not the receiving interface, so its interface link-local
        // is optional. A directly-received message still requires one.
        let interface = self.cfg.v6().get_interface_link_local(meta.ifindex);
        match interface {
            Some(iface) => {
                ctx.set_interface(iface);
            }
            None if ctx.relay().is_none() => {
                return Err(anyhow::anyhow!(
                    "no link-local address on interface {}",
                    meta.ifindex
                ));
            }
            None => {}
        }

        if let Some(global_unicast) = self.cfg.v6().get_interface_global(meta.ifindex) {
            ctx.set_global(global_unicast);
        }

        // evaluate v6 client classes once, up front, and stash the matched names
        // so leases-v6 (and the INFORMATION-REQUEST path below) can merge their
        // options. Done before `req` is bound so it doesn't hold a borrow of ctx
        // across the following `set_local`.
        let matched_v6 = match self.cfg.v4().eval_client_classes_v6(ctx.msg()) {
            Some(Ok(classes)) => Some(classes),
            Some(Err(err)) => {
                warn!(?err, "error evaluating v6 client classes");
                None
            }
            None => None,
        };
        if let Some(classes) = &matched_v6 {
            // a DROP class silences the client entirely
            if classes
                .iter()
                .any(|c| c == client_classes::client_classification::DROP_CLASS)
            {
                debug!("v6 DROP class matched");
                return Ok(Action::NoResponse);
            }
            ctx.set_local(MatchedClasses(classes.clone()));
        }

        let req = ctx.msg();
        let msg_type = req.msg_type();
        // honor Rapid Commit only if the client asked and we are configured for it
        let rapid_commit =
            req.opts().get(v6::OptionCode::RapidCommit).is_some() && self.cfg.v6().rapid_commit();

        debug!(
            ?msg_type,
            ?interface,
            global = ?ctx.global(),
            src_addr = %ctx.src_addr(),
            req = %ctx.msg(),
        );

        // let network = self.cfg.v6().get_network(meta.ifindex);

        // Honor the server mode (mirrors the v4 path): drain / maintenance /
        // shutting-down suppress new lease acquisition; maintenance additionally
        // suppresses renewals. SOLICIT and the REQUEST that completes it are new
        // acquisition; RENEW / REBIND are renewals. Other message types (RELEASE,
        // DECLINE, CONFIRM, INFORMATION-REQUEST) are always processed.
        let mode = self.mode.get();
        match msg_type {
            MessageType::Solicit | MessageType::Request if mode.suppresses_new_leases() => {
                debug!(
                    ?mode,
                    "server mode suppresses new leases; dropping SOLICIT/REQUEST"
                );
                return Ok(Action::NoResponse);
            }
            MessageType::Renew | MessageType::Rebind if mode.suppresses_renewals() => {
                debug!(
                    ?mode,
                    "server mode suppresses renewals; dropping RENEW/REBIND"
                );
                return Ok(Action::NoResponse);
            }
            _ => {}
        }

        // create initial response with reply type
        let mut resp = v6::Message::new_with_id(MessageType::Reply, req.xid());

        let server_id = self.cfg.v6().server_id();
        // TODO RelayForw type
        // TODO: make sure we handle client ids as specified - https://www.rfc-editor.org/rfc/rfc8415#section-16.1
        let req_sid = req.opts().get(v6::OptionCode::ServerId);
        // if the request includes a server id, it must match our server id
        if matches!(req_sid, Some(v6::DhcpOption::ServerId(id)) if *id != server_id) {
            debug!(?server_id, "server identifier in msg doesn't match");
            return Ok(Action::NoResponse);
        }
        // Confirm and Rebind MUST NOT carry a Server Identifier; discard if they
        // do (RFC 8415 §16.5 / §16.9)
        if matches!(msg_type, MessageType::Confirm | MessageType::Rebind) && req_sid.is_some() {
            debug!(
                ?msg_type,
                "discarding Confirm/Rebind that carries a Server Identifier"
            );
            return Ok(Action::NoResponse);
        }
        // add server id to response
        resp.opts_mut()
            .insert(v6::DhcpOption::ServerId(server_id.to_vec()));

        match msg_type {
            // discard if it has these types but NO server id
            // https://www.rfc-editor.org/rfc/rfc8415#section-16.6
            MessageType::Request
            | MessageType::Renew
            | MessageType::Decline
            | MessageType::Release
                if req_sid.is_none() =>
            {
                return Ok(Action::NoResponse);
            }
            MessageType::InformationRequest => {
                if let Some(opts) = self.cfg.v6().get_opts(meta.ifindex) {
                    // matched-class options fill in codes not set by config opts
                    let opts = self.cfg.v4().collect_opts_v6(opts, matched_v6.as_deref());
                    ctx.set_resp_msg(resp);
                    ctx.populate_opts(&opts);
                    return Ok(Action::Respond);
                }

                warn!(
                    ?msg_type,
                    "couldn't match any options with INFORMATION-REQUEST message"
                );
            }
            // Solicit: Advertise an address, unless Rapid Commit turns this into a
            // committing Reply. The leases-v6 plugin fills the IA_NA.
            MessageType::Solicit => {
                if rapid_commit {
                    resp.opts_mut().insert(v6::DhcpOption::RapidCommit);
                } else {
                    resp.set_msg_type(MessageType::Advertise);
                }
            }
            // Reply-type exchanges: response stays a Reply; leases-v6 processes
            // the binding (commit / renew / confirm / release / decline).
            MessageType::Request
            | MessageType::Renew
            | MessageType::Rebind
            | MessageType::Confirm
            | MessageType::Release
            | MessageType::Decline => {}
            _ => {
                debug!("currently unsupported message type");
                return Ok(Action::NoResponse);
            }
        }

        ctx.set_resp_msg(resp);
        Ok(Action::Continue)
    }
}

/// a list of matching client classes for this message
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedClasses(pub Vec<String>);

#[cfg(test)]
mod tests {
    use util::get_server_id_override;

    use dora_core::dhcproto::v4::{self, relay};
    use tracing_test::traced_test;

    use super::*;

    static SAMPLE_YAML: &str = include_str!("../../../libs/config/sample/config.yaml");

    #[tokio::test]
    #[traced_test]
    async fn test_request() -> Result<()> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg.clone()))?;
        let mut ctx = util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Request,
        )?;
        plugin.handle(&mut ctx).await?;

        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Ack)
        );
        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_discover() -> Result<()> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg.clone()))?;
        let mut ctx = util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Discover,
        )?;
        plugin.handle(&mut ctx).await?;

        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Offer)
        );
        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_bootp() -> Result<()> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg.clone()))?;
        let mut ctx = util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Request,
        )?;
        // remove msg type so we're bootp
        ctx.msg_mut().opts_mut().remove(v4::OptionCode::MessageType);
        plugin.handle(&mut ctx).await?;

        assert!(ctx.resp_msg().unwrap().opts().msg_type().is_none());
        Ok(())
    }

    /// Drain suppresses new leases: a DISCOVER gets no response.
    #[tokio::test]
    #[traced_test]
    async fn test_drain_suppresses_discover() -> Result<()> {
        use dora_core::mode::{ServerMode, SharedMode};
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg))?.with_mode(SharedMode::new(ServerMode::Drain));
        let mut ctx = util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Discover,
        )?;
        // NoResponse tells the server runner to send nothing back to the client
        let action = plugin.handle(&mut ctx).await?;
        assert!(matches!(action, Action::NoResponse));
        Ok(())
    }

    /// Drain still allows renewals: a RENEWING REQUEST (ciaddr set, no
    /// server-id) is answered (ACK).
    #[tokio::test]
    #[traced_test]
    async fn test_drain_allows_renewal_request() -> Result<()> {
        use dora_core::mode::{ServerMode, SharedMode};
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg))?.with_mode(SharedMode::new(ServerMode::Drain));
        let mut ctx = util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Request,
        )?;
        // ciaddr set + no server-id => RENEWING, which drain must still answer
        ctx.msg_mut().set_ciaddr("192.168.0.2".parse::<Ipv4Addr>()?);
        plugin.handle(&mut ctx).await?;
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Ack)
        );
        Ok(())
    }

    /// Drain suppresses a SELECTING REQUEST (new acquisition completing an
    /// offer): no response.
    #[tokio::test]
    #[traced_test]
    async fn test_drain_suppresses_selecting_request() -> Result<()> {
        use dora_core::mode::{ServerMode, SharedMode};
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg))?.with_mode(SharedMode::new(ServerMode::Drain));
        let mut ctx = util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Request,
        )?;
        // server-id (names us) + requested-ip + ciaddr 0 => SELECTING, a new
        // acquisition that drain must suppress
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::ServerIdentifier("192.168.0.1".parse()?));
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("192.168.0.2".parse()?));
        let action = plugin.handle(&mut ctx).await?;
        assert!(matches!(action, Action::NoResponse));
        Ok(())
    }

    /// Maintenance takes the server fully out of service: even a REQUEST
    /// (renewal) gets no response.
    #[tokio::test]
    #[traced_test]
    async fn test_maintenance_suppresses_request() -> Result<()> {
        use dora_core::mode::{ServerMode, SharedMode};
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin =
            MsgType::new(Arc::new(cfg))?.with_mode(SharedMode::new(ServerMode::Maintenance));
        let mut ctx = util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Request,
        )?;
        let action = plugin.handle(&mut ctx).await?;
        assert!(matches!(action, Action::NoResponse));
        Ok(())
    }

    /// build an INFORM context: the client already holds `ciaddr` and asks for
    /// local config on the 192.168.0.0/24 link.
    fn inform_ctx(ciaddr: &str) -> Result<MsgContext<Message>> {
        let mut ctx = util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Inform,
        )?;
        ctx.msg_mut().set_ciaddr(ciaddr.parse::<Ipv4Addr>()?);
        Ok(ctx)
    }

    /// INFORM whose address falls in a configured range -> ACK with local
    /// config, yiaddr 0, and no lease time (RFC 2131 §4.3.5).
    #[tokio::test]
    #[traced_test]
    async fn test_inform_in_range_acks() -> Result<()> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg))?;
        let mut ctx = inform_ctx("192.168.0.100")?;

        let action = plugin.handle(&mut ctx).await?;
        assert!(matches!(action, Action::Respond));
        let resp = ctx.resp_msg().unwrap();
        assert!(resp.opts().has_msg_type(v4::MessageType::Ack));
        assert_eq!(resp.yiaddr(), Ipv4Addr::UNSPECIFIED, "INFORM sets yiaddr 0");
        assert!(
            resp.opts().get(v4::OptionCode::AddressLeaseTime).is_none(),
            "INFORM must not return a lease time"
        );
        Ok(())
    }

    /// INFORM whose address is on-link but not in any pool is still answered
    /// with local config (the relaxed gating -- previously suppressed).
    #[tokio::test]
    #[traced_test]
    async fn test_inform_out_of_range_still_acks() -> Result<()> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg))?;
        // 192.168.0.60 is inside 192.168.0.0/24 but outside the .100-.150 range
        let mut ctx = inform_ctx("192.168.0.60")?;

        let action = plugin.handle(&mut ctx).await?;
        assert!(
            matches!(action, Action::Respond),
            "authoritative INFORM must answer regardless of pools"
        );
        let resp = ctx.resp_msg().unwrap();
        assert!(resp.opts().has_msg_type(v4::MessageType::Ack));
        assert_eq!(resp.yiaddr(), Ipv4Addr::UNSPECIFIED);
        assert!(resp.opts().get(v4::OptionCode::AddressLeaseTime).is_none());
        Ok(())
    }

    /// A non-authoritative network still ignores INFORM (unchanged gating).
    #[tokio::test]
    #[traced_test]
    async fn test_inform_non_authoritative_ignored() -> Result<()> {
        static NONAUTH_YAML: &str = r#"
v4:
    networks:
        192.168.0.0/24:
            authoritative: false
            ranges:
                - start: 192.168.0.100
                  end: 192.168.0.150
                  config:
                      lease_time:
                          default: 3600
                  options:
                      values: {}
"#;
        let cfg = DhcpConfig::parse_str(NONAUTH_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg))?;
        let mut ctx = inform_ctx("192.168.0.100")?;

        let action = plugin.handle(&mut ctx).await?;
        assert!(
            matches!(action, Action::NoResponse),
            "non-authoritative network must ignore INFORM"
        );
        Ok(())
    }

    /// A client matched to the DROP class is silenced, INFORM included -- the
    /// relaxed INFORM path must still honor DROP.
    #[tokio::test]
    #[traced_test]
    async fn test_inform_drop_class_ignored() -> Result<()> {
        // blank_ctx uses chaddr 01:02:03:04:05:06, so this DROP assert matches
        static DROP_YAML: &str = r#"
v4:
    networks:
        192.168.0.0/24:
            ranges:
                - start: 192.168.0.100
                  end: 192.168.0.150
                  config:
                      lease_time:
                          default: 3600
                  options:
                      values: {}
    client_classes:
        v4:
            - name: DROP
              assert: "pkt4.mac == 0x010203040506"
              options:
                  values: {}
"#;
        let cfg = DhcpConfig::parse_str(DROP_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg))?;
        let mut ctx = inform_ctx("192.168.0.100")?;

        let action = plugin.handle(&mut ctx).await?;
        assert!(
            matches!(action, Action::NoResponse),
            "DROP-classed client must not get an INFORM ACK"
        );
        Ok(())
    }

    // ensure the server identifier override is written to the response server identifier when they match
    #[tokio::test]
    #[traced_test]
    async fn test_server_id_eq_override() -> Result<()> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg.clone()))?;
        let mut ctx = util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Discover,
        )?;

        let mut relay_info = relay::RelayAgentInformation::default();
        relay_info.insert(relay::RelayInfo::ServerIdentifierOverride(
            "10.0.0.1".parse()?,
        ));
        // assign suboption 11 of DHCP relay info (opt 82)
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RelayAgentInformation(relay_info));
        // assign the same address to the server identifier
        ctx.msg_mut()
            .opts_mut()
            .insert(DhcpOption::ServerIdentifier("10.0.0.1".parse()?));
        plugin.handle(&mut ctx).await?;

        let resp_server_id = ctx
            .resp_msg()
            .unwrap()
            .opts()
            .get(OptionCode::ServerIdentifier);
        let msg_server_id_override = get_server_id_override(ctx.msg().opts());

        // get and compare the Ipv4Addrs from resp_server_id and resp_server_id_override
        if let (Some(&DhcpOption::ServerIdentifier(addr1)), Some(addr2)) =
            (resp_server_id, msg_server_id_override)
        {
            assert_eq!(addr1, addr2);
        } else {
            panic!(
                "Server identifier and server identifier override are not both Ipv4Addrs:\n\nOpt 54 = {:?}\nOpt 82 Subopt 11 = {:?}\n\n",
                resp_server_id, msg_server_id_override
            );
        }
        // ensure we respond with an offer
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Offer)
        );
        Ok(())
    }

    // ensure the server identifier override is not written to the response server identifier when they don't match
    #[tokio::test]
    #[traced_test]
    async fn test_server_id_ne_override() -> Result<()> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let plugin = MsgType::new(Arc::new(cfg.clone()))?;
        let mut ctx = util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Discover,
        )?;

        let mut relay_info = relay::RelayAgentInformation::default();
        relay_info.insert(relay::RelayInfo::ServerIdentifierOverride(
            "10.0.0.2".parse()?,
        ));
        // assign suboption 11 of DHCP relay info (opt 82)
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RelayAgentInformation(relay_info));
        // assign an address to the server identifier that does not match the override or our address
        ctx.msg_mut()
            .opts_mut()
            .insert(DhcpOption::ServerIdentifier("10.0.0.10".parse()?));
        let res = plugin.handle(&mut ctx).await?;
        // when the the server id in the message matches neither the server id override nor our server
        // id, we must not respond
        assert_eq!(res, Action::NoResponse);
        Ok(())
    }
}
