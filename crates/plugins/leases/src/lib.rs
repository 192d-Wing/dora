#![warn(
    missing_debug_implementations,
    // missing_docs, // we shall remove thee, someday!
    rust_2018_idioms,
    unreachable_pub,
    non_snake_case,
    non_upper_case_globals
)]
#![deny(rustdoc::broken_intra_doc_links)]
#![allow(clippy::cognitive_complexity, clippy::too_many_arguments)]

const OFFER_TIME: Duration = Duration::from_secs(60);

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr},
    time::{Duration, SystemTime},
};

use client_protection::RenewThreshold;
use ddns::{DdnsUpdate, dhcid::DhcId};
use dora_core::{
    anyhow::anyhow,
    chrono::{DateTime, SecondsFormat, Utc},
    dhcproto::v4::{DhcpOption, Message, MessageType, OptionCode},
    metrics,
    prelude::*,
    tracing::warn,
};
use message_type::MatchedClasses;
use register_derive::Register;
use static_addr::StaticAddr;

use config::{
    DhcpConfig,
    v4::{NetRange, Network},
};
use ip_manager::{IpError, IpManager, IpState, Storage};

#[derive(Register)]
#[register(msg(Message))]
#[register(plugin(StaticAddr))]
pub struct Leases<S>
where
    S: Storage,
{
    cfg: Arc<DhcpConfig>,
    ddns: DdnsUpdate,
    ip_mgr: Arc<IpManager<S>>,
    renew_cache: Option<RenewThreshold<Vec<u8>>>,
}

impl<S> fmt::Debug for Leases<S>
where
    S: Storage,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Leases").field("cfg", &self.cfg).finish()
    }
}

impl<S> Leases<S>
where
    S: Storage,
{
    pub fn new(cfg: Arc<DhcpConfig>, ip_mgr: Arc<IpManager<S>>) -> Self {
        Self {
            renew_cache: cfg.v4().cache_threshold().map(RenewThreshold::new),
            ip_mgr,
            cfg,
            ddns: DdnsUpdate::new(),
        }
    }

    pub fn cache_threshold(&self, id: &[u8]) -> Option<Duration> {
        self.renew_cache
            .as_ref()
            .and_then(|cache| cache.threshold(id))
    }

    pub fn cache_remove(&self, id: &[u8]) {
        self.renew_cache
            .as_ref()
            .and_then(|cache| cache.remove(&id.to_vec()));
    }
    pub fn cache_insert(&self, id: &[u8], lease_time: Duration) {
        self.renew_cache
            .as_ref()
            // TODO: try to remove to_vec?
            .and_then(|cache| {
                let old = cache.insert(id.to_vec(), lease_time);
                trace!(?old, ?id, "replacing old renewal time");
                old
            });
    }

    fn set_lease(
        &self,
        ctx: &mut MsgContext<Message>,
        (lease, t1, t2): (Duration, Duration, Duration),
        ip: Ipv4Addr,
        expires_at: SystemTime,
        classes: Option<&[String]>,
        range: &NetRange,
    ) -> Result<()> {
        ctx.resp_msg_mut()
            .context("response message must be set before leases is run")?
            .set_yiaddr(ip);
        ctx.populate_opts_lease(
            &self.cfg.v4().collect_opts(range.opts(), classes),
            lease,
            t1,
            t2,
        );
        ctx.set_local(ExpiresAt(expires_at));
        Ok(())
    }
}

#[async_trait]
impl<S> Plugin<Message> for Leases<S>
where
    S: Storage + Send + Sync + 'static,
{
    #[instrument(level = "debug", skip_all)]
    async fn handle(&self, ctx: &mut MsgContext<Message>) -> Result<Action> {
        let req = ctx.msg();

        let client_id = self.cfg.v4().client_id(req).to_vec(); // to_vec required b/c of borrowck error
        let subnet = ctx.subnet()?;
        // look up that subnet from our config
        let network = self.cfg.v4().network(subnet);
        let classes = ctx.get_local::<MatchedClasses>().map(|c| c.0.to_owned());
        let resp_has_yiaddr = matches!(ctx.resp_msg(), Some(msg) if !msg.yiaddr().is_unspecified());
        let rapid_commit =
            ctx.msg().opts().get(OptionCode::RapidCommit).is_some() && self.cfg.v4().rapid_commit();
        let bootp = self.cfg.v4().bootp_enabled();

        match (req.opts().msg_type(), network) {
            // if yiaddr is set, then a previous plugin has already given the message an IP (like static)
            (Some(MessageType::Discover), _) if resp_has_yiaddr => {
                return Ok(Action::Continue);
            }
            // giaddr has matched one of our configured subnets
            (Some(MessageType::Discover), Some(net)) => {
                self.discover(ctx, &client_id, net, classes, rapid_commit)
                    .await
            }
            // a previous plugin (e.g. static-addr) already assigned an address;
            // leave that ACK alone rather than run it through the lease state
            // machine (static addresses live outside the dynamic ranges).
            (Some(MessageType::Request), _) if resp_has_yiaddr => Ok(Action::Continue),
            (Some(MessageType::Request), Some(net)) => {
                self.request(ctx, &client_id, net, classes).await
            }
            (Some(MessageType::Release), _) => self.release(ctx, &client_id).await,
            (Some(MessageType::Decline), Some(net)) => self.decline(ctx, &client_id, net).await,
            // if BOOTP enabled and no msg type
            // getting here means no static address has been assigned either
            (_, Some(net)) if bootp => self.bootp(ctx, &client_id, net, classes).await,
            _ => {
                debug!(?subnet, giaddr = ?req.giaddr(), "message type or subnet did not match");
                // NoResponse means no other plugin gets to try to send a message
                Ok(Action::NoResponse)
            }
        }
    }
}

impl<S> Leases<S>
where
    S: Storage,
{
    async fn bootp(
        &self,
        ctx: &mut MsgContext<Message>,
        client_id: &[u8],
        network: &Network,
        classes: Option<Vec<String>>,
    ) -> Result<Action> {
        // BOOTP addresses are forever
        // TODO: we should probably set the expiry time to NULL but for now, 40 years in the future
        let expires_at = SystemTime::now() + Duration::from_secs(60 * 60 * 24 * 7 * 12 * 40);
        let state = Some(IpState::Lease);
        let resp = self
            .first_available(ctx, client_id, network, classes, expires_at, state)
            .await;
        ctx.filter_dhcp_opts();

        resp
    }

    /// uses requested ip from client, or the first available IP in the range
    async fn first_available(
        &self,
        ctx: &mut MsgContext<Message>,
        client_id: &[u8],
        network: &Network,
        classes: Option<Vec<String>>,
        expires_at: SystemTime,
        state: Option<IpState>,
    ) -> Result<Action> {
        let classes = classes.as_deref();
        // requested ip included in message, try to reserve
        if let Some(ip) = ctx.requested_ip() {
            // within our range. `range` makes sure IP is not in exclude list
            if let Some(range) = network.range(ip, classes) {
                match self
                    .ip_mgr
                    .try_ip(
                        ip.into(),
                        network.subnet().into(),
                        client_id,
                        expires_at,
                        network,
                        state,
                    )
                    .await
                {
                    Ok(_) => {
                        debug!(
                            ?ip,
                            ?client_id,
                            expires_at = %print_time(expires_at),
                            range = ?range.addrs(),
                            subnet = ?network.subnet(),
                           "reserved IP for client-- sending offer"
                        );
                        let lease = range.lease().determine_lease(ctx.requested_lease_time());
                        self.set_lease(ctx, lease, ip, expires_at, classes, range)?;
                        return Ok(Action::Continue);
                    }
                    // address in use from ping or cannot reserve this ip
                    // try to assign an IP
                    Err(err) => {
                        debug!(
                            ?err,
                            "could not assign requested IP, attempting to get new one"
                        );
                    }
                }
            }
        }
        // no requested IP, so find the next available
        for range in network.ranges_with_class(classes) {
            match self
                .ip_mgr
                .reserve_first(range, network, client_id, expires_at, state)
                .await
            {
                Ok(IpAddr::V4(ip)) => {
                    debug!(
                        ?ip,
                        ?client_id,
                        expires_at = %print_time(expires_at),
                        range = ?range.addrs(),
                        subnet = ?network.subnet(),
                        "reserved IP for client-- sending offer"
                    );
                    let lease = range.lease().determine_lease(ctx.requested_lease_time());
                    self.set_lease(ctx, lease, ip, expires_at, classes, range)?;
                    return Ok(Action::Continue);
                }
                Err(IpError::DbError(err)) => {
                    // log database error and try next IP
                    error!(?err);
                }
                _ => {
                    // all other errors try next
                }
            }
        }
        warn!(
            "leases plugin did not assign ip, check configuration or try clearing leases table. submit bugs to: github.com/bluecatengineering/dora"
        );
        Ok(Action::NoResponse)
    }

    async fn discover(
        &self,
        ctx: &mut MsgContext<Message>,
        client_id: &[u8],
        network: &Network,
        classes: Option<Vec<String>>,
        rapid_commit: bool,
    ) -> Result<Action> {
        // give 60 seconds between discover & request, TODO: configurable?
        let expires_at = SystemTime::now() + OFFER_TIME;
        let state = if rapid_commit {
            Some(IpState::Lease)
        } else {
            None
        };
        self.first_available(ctx, client_id, network, classes, expires_at, state)
            .await
    }

    /// Handle a DHCPREQUEST, dispatching on the RFC 2131 §4.3.2 client state.
    /// Each state has distinct response rules; most importantly an INIT-REBOOT
    /// for a client we have no record of MUST be answered with silence, not a
    /// NAK, so non-communicating servers can coexist on one wire.
    async fn request(
        &self,
        ctx: &mut MsgContext<Message>,
        client_id: &[u8],
        network: &Network,
        classes: Option<Vec<String>>,
    ) -> Result<Action> {
        let classes = classes.as_deref();
        match ctx.request_state() {
            // Responding to our DHCPOFFER: commit the offered address (creating
            // the lease if needed), or NAK if we can't honor it.
            RequestState::Selecting { requested } => {
                self.request_lease(ctx, client_id, network, classes, requested, true)
                    .await
            }
            // Extending a lease (unicast renew or broadcast rebind): extend an
            // existing binding, or NAK/stay-silent if we can't. Authoritative
            // servers still (re)create the lease so clients survive a server
            // restart, matching prior behavior.
            RequestState::Renewing { ciaddr } | RequestState::Rebinding { ciaddr } => {
                self.request_lease(ctx, client_id, network, classes, ciaddr, true)
                    .await
            }
            // Verifying a cached address after reboot. Wrong network -> NAK;
            // right network but unknown client -> stay silent (never create).
            RequestState::InitReboot { requested } => {
                self.request_init_reboot(ctx, client_id, network, classes, requested)
                    .await
            }
            // Malformed: no determinable requested address.
            RequestState::Unknown => {
                self.nak_or_silent(ctx, network, "DHCPREQUEST with no requested IP")
            }
        }
    }

    /// SELECTING / RENEWING / REBINDING: commit or extend the lease for `ip`.
    /// `create` allows inserting a fresh lease when none exists (authoritative).
    /// NAKs (or stays silent, per `authoritative`) when the address is out of
    /// range or cannot be leased.
    async fn request_lease(
        &self,
        ctx: &mut MsgContext<Message>,
        client_id: &[u8],
        network: &Network,
        classes: Option<&[String]>,
        ip: Ipv4Addr,
        create: bool,
    ) -> Result<Action> {
        let Some(range) = network.range(ip, classes) else {
            return self.nak_or_silent(ctx, network, "requested IP is not in any range");
        };
        if self
            .commit_lease(ctx, client_id, network, classes, ip, range, create)
            .await?
        {
            Ok(Action::Continue)
        } else {
            self.nak_or_silent(ctx, network, "requested IP could not be leased")
        }
    }

    /// INIT-REBOOT: verify a client's cached address (RFC 2131 §4.3.2).
    /// - address on the wrong network -> NAK (we positively know it's wrong)
    /// - right network, existing binding -> ACK (extend)
    /// - right network, no record of the client -> stay silent (MUST NOT NAK)
    ///
    /// Never creates a new lease: INIT-REBOOT only verifies an existing one.
    async fn request_init_reboot(
        &self,
        ctx: &mut MsgContext<Message>,
        client_id: &[u8],
        network: &Network,
        classes: Option<&[String]>,
        requested: Ipv4Addr,
    ) -> Result<Action> {
        if !network.full_subnet().contains(&requested) {
            // client applied its old address to a different link
            return self.nak_or_silent(ctx, network, "INIT-REBOOT address is on the wrong network");
        }
        match network.range(requested, classes) {
            Some(range)
                if self
                    .commit_lease(ctx, client_id, network, classes, requested, range, false)
                    .await? =>
            {
                Ok(Action::Continue)
            }
            // on-link but we have no binding for this client: RFC 2131 requires
            // silence so non-communicating servers can share a wire.
            _ => {
                debug!(
                    ?requested,
                    "INIT-REBOOT with no matching lease; staying silent"
                );
                Ok(Action::NoResponse)
            }
        }
    }

    /// Commit (or extend) the lease for `ip`, which is known to be in `range`,
    /// and populate the response. `create` selects create-or-extend
    /// ([`IpManager::try_lease`]) vs extend-only ([`IpManager::renew_existing`]).
    /// Returns whether the lease was granted; `false` means the caller should
    /// NAK or stay silent.
    ///
    /// [`IpManager::try_lease`]: ip_manager::IpManager::try_lease
    /// [`IpManager::renew_existing`]: ip_manager::IpManager::renew_existing
    async fn commit_lease(
        &self,
        ctx: &mut MsgContext<Message>,
        client_id: &[u8],
        network: &Network,
        classes: Option<&[String]>,
        ip: Ipv4Addr,
        range: &NetRange,
        create: bool,
    ) -> Result<bool> {
        // renew-threshold fast path: reuse the outstanding lease as-is
        if let Some(remaining) = self.cache_threshold(client_id) {
            metrics::RENEW_CACHE_HIT.inc();
            let lease = (
                remaining,
                config::renew(remaining),
                config::rebind(remaining),
            );
            let expires_at = SystemTime::now() + lease.0;
            debug!(
                ?ip,
                ?client_id,
                range = ?range.addrs(),
                subnet = ?network.subnet(),
                "reusing LEASE. client is renewing inside of the renew threshold"
            );
            self.set_lease(ctx, lease, ip, expires_at, classes, range)?;
            return Ok(true);
        }

        let lease = range.lease().determine_lease(ctx.requested_lease_time());
        let expires_at = SystemTime::now() + lease.0;

        let granted = if create {
            match self
                .ip_mgr
                .try_lease(ip.into(), client_id, expires_at, network)
                .await
            {
                Ok(_) => true,
                Err(err) => {
                    debug!(?err, "can't give out lease");
                    false
                }
            }
        } else {
            // extend-only: never insert a new binding
            self.ip_mgr
                .renew_existing(ip.into(), client_id, expires_at)
                .await
                .context("failed to look up lease for renewal")?
        };
        if !granted {
            return Ok(false);
        }

        debug!(
            ?ip,
            ?client_id,
            expires_at = %print_time(expires_at),
            range = ?range.addrs(),
            subnet = ?network.subnet(),
            "sending LEASE"
        );
        self.set_lease(ctx, lease, ip, expires_at, classes, range)?;
        // insert lease into cache
        self.cache_insert(client_id, lease.0);

        // do ddns update. Consider this as a plugin?
        let dhcid = dhcid(self.cfg.v4(), ctx.msg());
        if let Err(err) = self
            .ddns
            .update(ctx, dhcid, self.cfg.v4().ddns(), range, ip, lease.0)
            .await
        {
            error!(?err, "error during ddns update");
        }
        Ok(true)
    }

    /// NAK when authoritative (tell the client to restart), otherwise stay
    /// silent so we don't interfere with another server on the wire.
    fn nak_or_silent(
        &self,
        ctx: &mut MsgContext<Message>,
        network: &Network,
        reason: &str,
    ) -> Result<Action> {
        if network.authoritative() {
            debug!(%reason, "authoritative -> NAK");
            ctx.update_resp_msg(MessageType::Nak)
                .context("failed to set msg type")?;
            Ok(Action::Respond)
        } else {
            debug!(%reason, "not authoritative -> no response");
            Ok(Action::NoResponse)
        }
    }

    async fn release(&self, ctx: &mut MsgContext<Message>, client_id: &[u8]) -> Result<Action> {
        let ip = ctx.msg().ciaddr().into();
        if let Some(info) = self.ip_mgr.release_ip(ip, client_id).await? {
            self.cache_remove(client_id);
            debug!(?info, "released ip");
        } else {
            debug!(?ip, ?client_id, "ip not found in storage");
        }
        // release has no response
        Ok(Action::NoResponse)
    }

    async fn decline(
        &self,
        ctx: &mut MsgContext<Message>,
        client_id: &[u8],
        network: &Network,
    ) -> Result<Action> {
        let declined_ip = if let Some(DhcpOption::RequestedIpAddress(ip)) =
            ctx.msg().opts().get(OptionCode::RequestedIpAddress)
        {
            Ok(ip)
        } else {
            Err(anyhow!("decline has no option 50 (requested IP)"))
        }?;
        let expires_at = SystemTime::now() + network.probation_period();
        self.ip_mgr
            .probate_ip((*declined_ip).into(), client_id, expires_at)
            .await?;
        // IP is decline, remove from cache
        self.cache_remove(ctx.msg().chaddr());
        debug!(
            ?declined_ip,
            expires_at = %print_time(expires_at),
            "added declined IP with probation set"
        );
        Ok(Action::Continue)
    }
}

/// When the lease will expire at
#[derive(Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct ExpiresAt(pub SystemTime);

fn print_time(expires_at: SystemTime) -> String {
    DateTime::<Utc>::from(expires_at).to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// If opt 61 (client id) exists return that, otherwise return `chaddr` from the message
/// header.
pub fn dhcid(cfg: &config::v4::Config, msg: &Message) -> DhcId {
    if cfg.chaddr_only() {
        DhcId::chaddr(msg.chaddr())
    } else if let Some(DhcpOption::ClientIdentifier(id)) =
        msg.opts().get(OptionCode::ClientIdentifier)
    {
        DhcId::client_id(&id[..])
    } else {
        DhcId::chaddr(msg.chaddr())
    }
}

#[cfg(test)]
mod tests {
    use dora_core::dhcproto::v4;
    use ip_manager::sqlite::SqliteDb;
    use tracing_test::traced_test;

    use super::*;

    #[test]
    fn test_time_print() {
        assert_eq!(
            print_time(SystemTime::UNIX_EPOCH),
            "1970-01-01T00:00:00Z".to_owned()
        );
    }

    static SAMPLE_YAML: &str = include_str!("../../../libs/config/sample/config.yaml");

    #[tokio::test]
    #[traced_test]
    async fn test_request() -> Result<()> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        // println!("{cfg:#?}");
        let mgr = Arc::new(IpManager::new(SqliteDb::new("sqlite::memory:").await?)?);
        let leases = Leases::new(Arc::new(cfg.clone()), mgr);
        let mut ctx = message_type::util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Request,
        )?;
        leases.handle(&mut ctx).await?;

        // no requested IP put in message, NAK
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Nak)
        );
        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_discover() -> Result<()> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let mgr = Arc::new(IpManager::new(SqliteDb::new("sqlite::memory:").await?)?);
        let leases = Leases::new(Arc::new(cfg.clone()), mgr);
        let mut ctx = message_type::util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Discover,
        )?;
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("192.168.0.100".parse()?));
        ctx.resp_msg_mut()
            .unwrap()
            .opts_mut()
            .insert(v4::DhcpOption::MessageType(v4::MessageType::Offer)); // ack is set in msg type plugin

        leases.handle(&mut ctx).await?;
        debug!(?ctx);
        // requested IP, OFFER
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Offer)
        );
        assert_eq!(
            ctx.resp_msg().unwrap().yiaddr(),
            Ipv4Addr::new(192, 168, 0, 100)
        );
        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_release() -> Result<()> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let mgr = IpManager::new(SqliteDb::new("sqlite::memory:").await?)?;
        let leases = Leases::new(Arc::new(cfg.clone()), Arc::new(mgr));
        let mut ctx = message_type::util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Discover,
        )?;
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("192.168.0.100".parse()?));
        ctx.resp_msg_mut()
            .unwrap()
            .opts_mut()
            .insert(v4::DhcpOption::MessageType(v4::MessageType::Offer)); // ack is set in msg type plugin

        leases.handle(&mut ctx).await?;
        debug!(?ctx);
        // requested IP, OFFER
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Offer)
        );
        assert_eq!(
            ctx.resp_msg().unwrap().yiaddr(),
            Ipv4Addr::new(192, 168, 0, 100)
        );

        let mut ctx = message_type::util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Request,
        )?;
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("192.168.0.100".parse()?));
        ctx.resp_msg_mut()
            .unwrap()
            .opts_mut()
            .insert(v4::DhcpOption::MessageType(v4::MessageType::Ack)); // ack is set in msg type plugin

        leases.handle(&mut ctx).await?;
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Ack)
        );
        assert_eq!(
            ctx.resp_msg().unwrap().yiaddr(),
            Ipv4Addr::new(192, 168, 0, 100)
        );

        let mut ctx = message_type::util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Release, // set release
        )?;
        ctx.msg_mut().set_ciaddr(Ipv4Addr::new(192, 168, 0, 100));
        ctx.resp_msg_mut()
            .unwrap()
            .opts_mut()
            .insert(v4::DhcpOption::MessageType(v4::MessageType::Ack)); // ack is set in msg type plugin

        leases.handle(&mut ctx).await?;
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Ack)
        );

        Ok(())
    }

    /// build a DHCPREQUEST context on the 192.168.0.0/24 link (selected via the
    /// giaddr/subnet-selection blank_ctx sets) with a preset Ack response.
    fn request_ctx() -> Result<MsgContext<Message>> {
        let mut ctx = message_type::util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Request,
        )?;
        ctx.resp_msg_mut()
            .unwrap()
            .opts_mut()
            .insert(v4::DhcpOption::MessageType(v4::MessageType::Ack));
        Ok(ctx)
    }

    async fn new_leases() -> Result<Leases<SqliteDb>> {
        let cfg = DhcpConfig::parse_str(SAMPLE_YAML).unwrap();
        let mgr = Arc::new(IpManager::new(SqliteDb::new("sqlite::memory:").await?)?);
        Ok(Leases::new(Arc::new(cfg), mgr))
    }

    /// INIT-REBOOT (opt 50, no server-id, ciaddr 0) for an in-range address we
    /// have no record of MUST be answered with silence, not a NAK (RFC 2131
    /// §4.3.2 — coexistence of non-communicating servers).
    #[tokio::test]
    #[traced_test]
    async fn test_request_init_reboot_unknown_is_silent() -> Result<()> {
        let leases = new_leases().await?;
        let mut ctx = request_ctx()?;
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("192.168.0.100".parse()?));

        let action = leases.handle(&mut ctx).await?;
        assert!(
            matches!(action, Action::NoResponse),
            "unknown INIT-REBOOT client must get no response"
        );
        // the pre-set Ack must not have been turned into a NAK
        assert!(
            !ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Nak)
        );
        Ok(())
    }

    /// INIT-REBOOT for an address that isn't on this link -> the client moved
    /// networks, so NAK (we positively know the address is wrong).
    #[tokio::test]
    #[traced_test]
    async fn test_request_init_reboot_wrong_network_naks() -> Result<()> {
        let leases = new_leases().await?;
        let mut ctx = request_ctx()?;
        // 172.16.0.5 is not within the selected 192.168.0.0/24 subnet
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("172.16.0.5".parse()?));

        let action = leases.handle(&mut ctx).await?;
        assert!(matches!(action, Action::Respond));
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Nak),
            "wrong-network INIT-REBOOT must NAK"
        );
        Ok(())
    }

    /// INIT-REBOOT for an address we DO hold a binding for -> ACK (extend). Here
    /// the binding is established by a prior DISCOVER reservation.
    #[tokio::test]
    #[traced_test]
    async fn test_request_init_reboot_known_acks() -> Result<()> {
        let leases = new_leases().await?;
        // DISCOVER reserves 192.168.0.100 for this client
        let mut ctx = message_type::util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Discover,
        )?;
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("192.168.0.100".parse()?));
        ctx.resp_msg_mut()
            .unwrap()
            .opts_mut()
            .insert(v4::DhcpOption::MessageType(v4::MessageType::Offer));
        leases.handle(&mut ctx).await?;

        // INIT-REBOOT for the same address -> extend -> ACK
        let mut ctx = request_ctx()?;
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("192.168.0.100".parse()?));
        leases.handle(&mut ctx).await?;
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Ack)
        );
        assert_eq!(
            ctx.resp_msg().unwrap().yiaddr(),
            Ipv4Addr::new(192, 168, 0, 100)
        );
        Ok(())
    }

    /// RENEWING (ciaddr set, no server-id) extends an existing lease -> ACK.
    #[tokio::test]
    #[traced_test]
    async fn test_request_renewing_acks() -> Result<()> {
        let leases = new_leases().await?;
        // establish a lease via DISCOVER reservation
        let mut ctx = message_type::util::blank_ctx(
            "192.168.0.1:67".parse()?,
            "192.168.0.1".parse()?,
            "192.168.0.1".parse()?,
            v4::MessageType::Discover,
        )?;
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("192.168.0.100".parse()?));
        ctx.resp_msg_mut()
            .unwrap()
            .opts_mut()
            .insert(v4::DhcpOption::MessageType(v4::MessageType::Offer));
        leases.handle(&mut ctx).await?;

        // RENEW: ciaddr carries the client's current address (no opt 50/server-id)
        let mut ctx = request_ctx()?;
        ctx.msg_mut().set_ciaddr(Ipv4Addr::new(192, 168, 0, 100));
        leases.handle(&mut ctx).await?;
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Ack)
        );
        assert_eq!(
            ctx.resp_msg().unwrap().yiaddr(),
            Ipv4Addr::new(192, 168, 0, 100)
        );
        Ok(())
    }

    /// A REQUEST whose response already carries a yiaddr (assigned by an earlier
    /// plugin such as static-addr) is left untouched: static addresses live
    /// outside the dynamic ranges and must not run through the lease state
    /// machine, even though they look like SELECTING for an out-of-range IP.
    #[tokio::test]
    #[traced_test]
    async fn test_request_preassigned_yiaddr_is_kept() -> Result<()> {
        let leases = new_leases().await?;
        let mut ctx = request_ctx()?;
        // simulate static-addr having already assigned the address
        ctx.resp_msg_mut()
            .unwrap()
            .set_yiaddr(Ipv4Addr::new(192, 168, 2, 170));
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("192.168.2.170".parse()?));
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::ServerIdentifier("192.168.0.1".parse()?));

        let action = leases.handle(&mut ctx).await?;
        assert!(matches!(action, Action::Continue));
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Ack)
        );
        assert_eq!(
            ctx.resp_msg().unwrap().yiaddr(),
            Ipv4Addr::new(192, 168, 2, 170)
        );
        Ok(())
    }

    /// SELECTING (server-id present) commits the offered address, creating the
    /// lease when authoritative even without a prior reservation -> ACK.
    #[tokio::test]
    #[traced_test]
    async fn test_request_selecting_acks() -> Result<()> {
        let leases = new_leases().await?;
        let mut ctx = request_ctx()?;
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::RequestedIpAddress("192.168.0.100".parse()?));
        // server-id present -> SELECTING
        ctx.msg_mut()
            .opts_mut()
            .insert(v4::DhcpOption::ServerIdentifier("192.168.0.1".parse()?));
        leases.handle(&mut ctx).await?;
        assert!(
            ctx.resp_msg()
                .unwrap()
                .opts()
                .has_msg_type(v4::MessageType::Ack)
        );
        assert_eq!(
            ctx.resp_msg().unwrap().yiaddr(),
            Ipv4Addr::new(192, 168, 0, 100)
        );
        Ok(())
    }
}
