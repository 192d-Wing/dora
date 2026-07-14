use std::fmt::Write;

use dora_core::{
    async_trait, chrono,
    dhcproto::{v4, v6},
    handler::PostResponse,
    server::context::MsgContext,
};
use leases::ExpiresAt;
use message_type::MatchedClasses;
use tracing::info;

pub struct ForensicLog;

fn format_mac(chaddr: &[u8]) -> String {
    let mut s = String::with_capacity(chaddr.len() * 3);
    for (i, b) in chaddr.iter().enumerate() {
        if i > 0 {
            s.push(':');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn format_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[async_trait]
impl PostResponse<v4::Message> for ForensicLog {
    async fn handle(&self, ctx: MsgContext<v4::Message>) {
        let timestamp = ctx.time().to_rfc3339();
        let msg_id = ctx.id();

        let req_type = ctx
            .msg()
            .opts()
            .msg_type()
            .map(|t| format!("{t:?}"))
            .unwrap_or_default();

        let chaddr = format_mac(ctx.msg().chaddr());

        let client_id = ctx
            .msg()
            .opts()
            .get(v4::OptionCode::ClientIdentifier)
            .map(|opt| {
                if let v4::DhcpOption::ClientIdentifier(id) = opt {
                    format_hex(id)
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();

        let hostname = ctx
            .msg()
            .opts()
            .get(v4::OptionCode::Hostname)
            .map(|opt| {
                if let v4::DhcpOption::Hostname(h) = opt {
                    String::from_utf8_lossy(h).into_owned()
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();

        let giaddr = ctx.msg().giaddr();

        let relay_agent_info = ctx
            .msg()
            .opts()
            .get(v4::OptionCode::RelayAgentInformation)
            .map(|opt| format!("{opt:?}"))
            .unwrap_or_default();

        let subnet = ctx.subnet().map(|s| s.to_string()).unwrap_or_default();
        let src_addr = ctx.src_addr().to_string();
        let dst_addr = ctx.dst_addr().map(|a| a.to_string()).unwrap_or_default();
        let interface = ctx.interface().map(|i| i.to_string()).unwrap_or_default();

        let matched_classes = ctx
            .get_local::<MatchedClasses>()
            .map(|mc| mc.0.join(","))
            .unwrap_or_default();

        let expires_at = ctx
            .get_local::<ExpiresAt>()
            .map(|e| {
                let dt: chrono::DateTime<chrono::Utc> = e.0.into();
                dt.to_rfc3339()
            })
            .unwrap_or_default();

        // Extract response fields (may be None for RELEASE/DECLINE/drops)
        let (resp_type, assigned_ip, lease_duration_secs) = match ctx.resp_msg() {
            Some(resp) => {
                let rt = resp
                    .opts()
                    .msg_type()
                    .map(|t| format!("{t:?}"))
                    .unwrap_or_default();
                let ip = resp.yiaddr().to_string();
                let lease = resp
                    .opts()
                    .get(v4::OptionCode::AddressLeaseTime)
                    .map(|opt| {
                        if let v4::DhcpOption::AddressLeaseTime(t) = opt {
                            *t
                        } else {
                            0
                        }
                    })
                    .unwrap_or(0);
                (rt, ip, lease)
            }
            None => (String::from("none"), String::new(), 0),
        };

        info!(
            target: "forensic_log",
            %timestamp,
            msg_id,
            req_type,
            resp_type,
            chaddr,
            client_id,
            assigned_ip,
            lease_duration_secs,
            expires_at,
            %giaddr,
            relay_agent_info,
            hostname,
            subnet,
            src_addr,
            dst_addr,
            interface,
            matched_classes,
        );
    }
}

#[async_trait]
impl PostResponse<v6::Message> for ForensicLog {
    async fn handle(&self, ctx: MsgContext<v6::Message>) {
        use v6::{DhcpOption, OptionCode};

        let timestamp = ctx.time().to_rfc3339();
        let msg_id = ctx.id();
        let req_type = format!("{:?}", ctx.msg().msg_type());

        let duid = ctx
            .msg()
            .opts()
            .get(OptionCode::ClientId)
            .map(|opt| {
                if let DhcpOption::ClientId(bytes) = opt {
                    format_hex(bytes)
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();

        let relay_link_addr = ctx
            .relay()
            .and_then(|chain| chain.0.last())
            .map(|hop| hop.link_addr.to_string())
            .unwrap_or_default();

        let src_addr = ctx.src_addr().to_string();
        let dst_addr = ctx.dst_addr().map(|a| a.to_string()).unwrap_or_default();
        let interface = ctx.interface().map(|i| i.to_string()).unwrap_or_default();

        let matched_classes = ctx
            .get_local::<MatchedClasses>()
            .map(|mc| mc.0.join(","))
            .unwrap_or_default();

        // Extract response fields
        let (resp_type, assigned_addrs, assigned_prefixes) = match ctx.resp_msg() {
            Some(resp) => {
                let rt = format!("{:?}", resp.msg_type());

                let mut addrs = Vec::new();
                if let Some(iana_opts) = resp.opts().get_all(OptionCode::IANA) {
                    for opt in iana_opts {
                        if let DhcpOption::IANA(iana) = opt
                            && let Some(ia_addrs) = iana.opts.get_all(OptionCode::IAAddr)
                        {
                            for addr_opt in ia_addrs {
                                if let DhcpOption::IAAddr(ia_addr) = addr_opt {
                                    addrs.push(format!(
                                        "{}(iaid={},preferred={},valid={})",
                                        ia_addr.addr,
                                        iana.id,
                                        ia_addr.preferred_life,
                                        ia_addr.valid_life,
                                    ));
                                }
                            }
                        }
                    }
                }

                let mut prefixes = Vec::new();
                if let Some(iapd_opts) = resp.opts().get_all(OptionCode::IAPD) {
                    for opt in iapd_opts {
                        if let DhcpOption::IAPD(iapd) = opt
                            && let Some(ia_pfxs) = iapd.opts.get_all(OptionCode::IAPrefix)
                        {
                            for pfx_opt in ia_pfxs {
                                if let DhcpOption::IAPrefix(ia_prefix) = pfx_opt {
                                    prefixes.push(format!(
                                        "{}/{}(iaid={},preferred={},valid={})",
                                        ia_prefix.prefix_ip,
                                        ia_prefix.prefix_len,
                                        iapd.id,
                                        ia_prefix.preferred_lifetime,
                                        ia_prefix.valid_lifetime,
                                    ));
                                }
                            }
                        }
                    }
                }

                (rt, addrs.join(";"), prefixes.join(";"))
            }
            None => (String::from("none"), String::new(), String::new()),
        };

        info!(
            target: "forensic_log",
            %timestamp,
            msg_id,
            req_type,
            resp_type,
            duid,
            assigned_addrs,
            assigned_prefixes,
            relay_link_addr,
            src_addr,
            dst_addr,
            interface,
            matched_classes,
        );
    }
}
