//! Out-of-process tests for `dora-embedded`: the single-process binary on a
//! redb file. Same netns/veth harness as `basic.rs`, so the same DHCP client
//! drives it; the point here is the store, not the DHCP logic (which
//! `basic.rs` already covers against Postgres):
//!
//! - the embedded binary serves a DORA exchange end to end,
//! - leases survive a clean restart (SIGTERM) and a crash (SIGKILL),
//! - a Postgres URL is rejected up front.
// this binary uses a subset of the shared harness
#[allow(dead_code)]
mod common;

use std::{
    net::Ipv4Addr,
    process::{Command, Stdio},
};

use anyhow::Result;
use common::{builder::*, client::Client, env::DhcpServerEnv, utils};
use dora_core::{dhcproto::v4, tracing};
use integration_tests::bin_path;
use mac_address::MacAddress;
use tracing_test::traced_test;

fn client() -> Result<Client<v4::Message>> {
    let settings = ClientSettingsBuilder::default()
        .iface_name("dhcpcli")
        .target("192.168.2.1".parse::<std::net::IpAddr>().unwrap())
        .port(9900_u16)
        .build()?;
    Ok(Client::<v4::Message>::new(settings))
}

/// DISCOVER -> OFFER -> REQUEST -> ACK for `mac`; returns the leased address.
fn dora(client: &mut Client<v4::Message>, mac: MacAddress) -> Result<Ipv4Addr> {
    let msg_args = DiscoverBuilder::default()
        .giaddr([192, 168, 2, 1])
        .chaddr(mac)
        .build()?;
    let resp = client.run(MsgType::Discover(msg_args))?;
    assert_eq!(resp.opts().msg_type().unwrap(), v4::MessageType::Offer);
    let offered = resp.yiaddr();

    let msg_args = RequestBuilder::default()
        .giaddr([192, 168, 2, 1])
        .chaddr(mac)
        .opt_req_addr(offered)
        .build()?;
    let resp = client.run(MsgType::Request(msg_args))?;
    assert_eq!(resp.opts().msg_type().unwrap(), v4::MessageType::Ack);
    assert_eq!(resp.yiaddr(), offered);
    Ok(offered)
}

/// A DISCOVER alone; returns the offered address.
fn discover(client: &mut Client<v4::Message>, mac: MacAddress) -> Result<Ipv4Addr> {
    let msg_args = DiscoverBuilder::default()
        .giaddr([192, 168, 2, 1])
        .chaddr(mac)
        .build()?;
    let resp = client.run(MsgType::Discover(msg_args))?;
    assert_eq!(resp.opts().msg_type().unwrap(), v4::MessageType::Offer);
    Ok(resp.yiaddr())
}

#[test]
#[traced_test]
/// Discover / Offer / Request / Ack, then a renewing Request, through
/// `dora-embedded`.
fn embedded_dhcpv4_unicast() -> Result<()> {
    let _srv = DhcpServerEnv::start_embedded(
        "basic.yaml",
        "embedded_basic",
        "dora_test",
        "dhcpcli",
        "dhcpsrv",
        "192.168.2.1",
    );
    let mut client = client()?;
    let mac = utils::rand_mac();
    let ip = dora(&mut client, mac)?;

    // renew
    let msg_args = RequestBuilder::default()
        .giaddr([192, 168, 2, 1])
        .chaddr(mac)
        .opt_req_addr(ip)
        .build()?;
    let resp = client.run(MsgType::Request(msg_args))?;
    assert_eq!(resp.opts().msg_type().unwrap(), v4::MessageType::Ack);
    assert_eq!(resp.yiaddr(), ip);

    // the lease is visible through the (in-process) management API
    let leases = _srv.api_get("/v1/leases");
    assert!(
        leases.contains(&ip.to_string()),
        "lease {ip} missing from /v1/leases: {leases}"
    );
    Ok(())
}

/// Two clients lease consecutive addresses, the daemon is stopped with
/// `signal` and started again on the same file, and the second client must be
/// offered its own address back. With a lost store it would be handed the
/// first address of the range again, so this discriminates persistence from
/// mere re-allocation.
fn leases_survive(signal: &str, name: &str) -> Result<()> {
    let mut srv = DhcpServerEnv::start_embedded(
        "basic.yaml",
        name,
        "dora_test",
        "dhcpcli",
        "dhcpsrv",
        "192.168.2.1",
    );
    let mut client = client()?;
    let (mac_a, mac_b) = (utils::rand_mac(), utils::rand_mac());
    let ip_a = dora(&mut client, mac_a)?;
    let ip_b = dora(&mut client, mac_b)?;
    assert_ne!(ip_a, ip_b);

    srv.restart(signal);

    // B gets its own binding back, not A's (the lowest free) address
    let again = discover(&mut client, mac_b)?;
    assert_eq!(again, ip_b, "client B's lease did not survive {signal}");
    // and A's binding is still held, so a new client gets a third address
    let ip_c = dora(&mut client, utils::rand_mac())?;
    assert_ne!(ip_c, ip_a);
    assert_ne!(ip_c, ip_b);

    let leases = srv.api_get("/v1/leases");
    for ip in [ip_a, ip_b, ip_c] {
        assert!(
            leases.contains(&ip.to_string()),
            "{ip} missing after {signal}"
        );
    }
    Ok(())
}

#[test]
#[traced_test]
fn embedded_leases_survive_restart() -> Result<()> {
    leases_survive("TERM", "embedded_restart")
}

#[test]
#[traced_test]
/// Power loss: SIGKILL between commits. `--redb-sync immediate` (the default)
/// fsyncs every commit, so every ACKed lease must be there after the crash.
fn embedded_leases_survive_sigkill() -> Result<()> {
    leases_survive("KILL", "embedded_sigkill")
}

#[test]
/// `dora-embedded` is redb-only: a Postgres URL is a configuration error,
/// reported before anything else is opened. No namespace or root needed.
fn embedded_rejects_postgres_url() {
    let out = Command::new(bin_path("dora-embedded"))
        .args([
            "-d",
            "postgres://dora:dora@localhost/dora",
            "--dora-log",
            "warn",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("failed to run dora-embedded");
    // the binary logs the error and exits (0) from main like the other
    // services; the message is what we check
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("supports redb only"),
        "expected a wrong-backend error, got: {text}"
    );
}
