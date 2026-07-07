//! End-to-end DHCPv6 integration tests.
//!
//! These drive real packets through a running `dora` binary. Unlike the v4
//! tests, they need no network namespace / veth / sudo: the server is bound to
//! loopback on a non-default port (so it unicasts replies), and the client
//! wraps each request in a Relay-forward. The relay path selects the subnet by
//! the relay link-address rather than the receiving interface, so it works over
//! loopback while exercising the full stack: UDP receive -> relay unwrap ->
//! MsgType -> leases-v6 allocation -> relay-reply wrap -> UDP send.

use std::{
    fs,
    net::{Ipv6Addr, UdpSocket},
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

use dora_core::dhcproto::{
    Decodable, Decoder, Encodable,
    v6::{
        self, DhcpOption, DhcpOptions, IAAddr, IANA, IAPD, MessageType, OptionCode, RelayMessage,
        Status,
    },
};

/// a running `dora` server bound to loopback, killed and cleaned up on drop
struct DoraV6 {
    child: Child,
    db: String,
}

impl DoraV6 {
    fn start(v6_port: u16, v4_port: u16) -> Self {
        let db = format!("/tmp/dora_v6_relay_{v6_port}.db");
        for suffix in ["", "-shm", "-wal"] {
            let _ = fs::remove_file(format!("{db}{suffix}"));
        }
        let config = format!(
            "{}/tests/test_configs/v6_relay.yaml",
            env!("CARGO_MANIFEST_DIR")
        );
        let child = Command::new(env!("CARGO_BIN_EXE_dora"))
            .args([
                "-c",
                &config,
                "-d",
                &db,
                // ephemeral-ish v4 port so we don't need privileged :67; v4 is unused here
                "--v4-addr",
                &format!("0.0.0.0:{v4_port}"),
                "--v6-addr",
                &format!("[::1]:{v6_port}"),
                "--dora-log",
                "warn",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start dora");
        // give it a moment to bind; the exchange helper also retries
        thread::sleep(Duration::from_millis(500));
        Self { child, db }
    }
}

impl Drop for DoraV6 {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for suffix in ["", "-shm", "-wal"] {
            let _ = fs::remove_file(format!("{}{suffix}", self.db));
        }
    }
}

fn client(v6_port: u16) -> UdpSocket {
    let sock = UdpSocket::bind("[::1]:0").expect("bind client socket");
    sock.connect(format!("[::1]:{v6_port}")).expect("connect");
    sock.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    sock
}

const LINK: &str = "2001:db8:1::1"; // relay link-address -> selects the network
const PEER: &str = "fe80::5"; // client address

/// Send `inner` (a client message) wrapped in a Relay-forward and return the
/// inner client message from the Relay-reply. Retries on timeout so the first
/// call also covers server startup latency.
fn relay_exchange(sock: &UdpSocket, inner: &v6::Message) -> v6::Message {
    let mut relay = RelayMessage::new(
        MessageType::RelayForw,
        0,
        LINK.parse().unwrap(),
        PEER.parse().unwrap(),
    );
    relay
        .opts_mut()
        .insert(DhcpOption::RelayMsg(inner.to_vec().unwrap()));
    let bytes = relay.to_vec().unwrap();

    let mut buf = [0u8; 2048];
    for _ in 0..40 {
        sock.send(&bytes).expect("send relay-forward");
        if let Ok(n) = sock.recv(&mut buf) {
            let reply = RelayMessage::decode(&mut Decoder::new(&buf[..n])).expect("decode reply");
            assert_eq!(reply.msg_type(), MessageType::RelayRepl, "expected Relay-reply");
            let inner = match reply.opts().get(OptionCode::RelayMsg) {
                Some(DhcpOption::RelayMsg(b)) => b.clone(),
                _ => panic!("relay-reply missing Relay-Message option"),
            };
            return v6::Message::decode(&mut Decoder::new(&inner)).expect("decode inner");
        }
        thread::sleep(Duration::from_millis(200));
    }
    panic!("no relay-reply after retries (server not responding)");
}

fn with_client_id(mut m: v6::Message, duid: &[u8]) -> v6::Message {
    m.opts_mut().insert(DhcpOption::ClientId(duid.to_vec()));
    m
}

/// the address inside the first IA_NA of a message, if any
fn iana_addr(msg: &v6::Message) -> Option<Ipv6Addr> {
    match msg.opts().get(OptionCode::IANA)? {
        DhcpOption::IANA(iana) => match iana.opts.get(OptionCode::IAAddr)? {
            DhcpOption::IAAddr(a) => Some(a.addr),
            _ => None,
        },
        _ => None,
    }
}

fn top_status(msg: &v6::Message) -> Option<Status> {
    match msg.opts().get(OptionCode::StatusCode)? {
        DhcpOption::StatusCode(sc) => Some(sc.status),
        _ => None,
    }
}

/// build an IA_NA carrying a specific address (for Request/Renew/Release echo)
fn iana_with(addr: Ipv6Addr, iaid: u32) -> DhcpOption {
    let mut opts = DhcpOptions::new();
    opts.insert(DhcpOption::IAAddr(IAAddr {
        addr,
        preferred_life: 0,
        valid_life: 0,
        opts: DhcpOptions::new(),
    }));
    DhcpOption::IANA(IANA {
        id: iaid,
        t1: 0,
        t2: 0,
        opts,
    })
}

/// Full IA_NA lifecycle over a relay: Solicit -> Advertise, Request -> Reply,
/// Renew -> Reply, Release -> Reply(Success).
#[test]
fn v6_relay_ia_na_lifecycle() {
    let _srv = DoraV6::start(15480, 15481);
    let sock = client(15480);
    let duid = b"\x00\x03\x00\x01\xaa\xbb\xcc\xdd\xee\x01";

    // Solicit -> Advertise with an address from the configured range
    let mut sol = with_client_id(v6::Message::new(MessageType::Solicit), duid);
    sol.opts_mut().insert(DhcpOption::IANA(IANA {
        id: 1,
        t1: 0,
        t2: 0,
        opts: DhcpOptions::new(),
    }));
    let adv = relay_exchange(&sock, &sol);
    assert_eq!(adv.msg_type(), MessageType::Advertise);
    let addr = iana_addr(&adv).expect("Advertise has an IAADDR");
    let (lo, hi): (Ipv6Addr, Ipv6Addr) =
        ("2001:db8:1::100".parse().unwrap(), "2001:db8:1::1ff".parse().unwrap());
    assert!(addr >= lo && addr <= hi, "advertised addr {addr} in range");

    // the server identifier the client must echo back on Request/Renew/Release
    let server_id = adv
        .opts()
        .get(OptionCode::ServerId)
        .expect("Advertise has a Server Identifier")
        .clone();

    let build = |ty: MessageType| {
        let mut m = with_client_id(v6::Message::new(ty), duid);
        m.opts_mut().insert(server_id.clone());
        m.opts_mut().insert(iana_with(addr, 1));
        m
    };

    // Request -> Reply committing the same address
    let reply = relay_exchange(&sock, &build(MessageType::Request));
    assert_eq!(reply.msg_type(), MessageType::Reply);
    assert_eq!(iana_addr(&reply), Some(addr), "Reply commits the requested addr");

    // Renew -> Reply, same address extended
    let renew = relay_exchange(&sock, &build(MessageType::Renew));
    assert_eq!(renew.msg_type(), MessageType::Reply);
    assert_eq!(iana_addr(&renew), Some(addr), "Renew keeps the address");

    // Release -> Reply with a top-level Success
    let rel = relay_exchange(&sock, &build(MessageType::Release));
    assert_eq!(rel.msg_type(), MessageType::Reply);
    assert_eq!(top_status(&rel), Some(Status::Success), "Release Success");
}

/// A repeat Solicit from the same client is offered the same address (a stable
/// binding), and a plain Solicit yields an Advertise (not a committing Reply,
/// since Rapid Commit is off in the test config).
#[test]
fn v6_relay_solicit_is_stable() {
    let _srv = DoraV6::start(15482, 15483);
    let sock = client(15482);
    let duid = b"\x00\x03\x00\x01\xaa\xbb\xcc\xdd\xee\x02";

    let mut sol = with_client_id(v6::Message::new(MessageType::Solicit), duid);
    sol.opts_mut().insert(DhcpOption::IANA(IANA {
        id: 7,
        t1: 0,
        t2: 0,
        opts: DhcpOptions::new(),
    }));

    let adv1 = relay_exchange(&sock, &sol);
    assert_eq!(adv1.msg_type(), MessageType::Advertise);
    let a1 = iana_addr(&adv1).expect("IAADDR");

    // a second Solicit from the same DUID+IAID gets the same address back
    let adv2 = relay_exchange(&sock, &sol);
    assert_eq!(iana_addr(&adv2), Some(a1), "same client keeps its offered address");
}

/// IA_PD: Solicit for a prefix -> Advertise carrying an IA_PD + IAPREFIX from
/// the configured pd_pool.
#[test]
fn v6_relay_prefix_delegation() {
    let _srv = DoraV6::start(15484, 15485);
    let sock = client(15484);
    let duid = b"\x00\x03\x00\x01\xaa\xbb\xcc\xdd\xee\x03";

    let mut sol = with_client_id(v6::Message::new(MessageType::Solicit), duid);
    sol.opts_mut().insert(DhcpOption::IAPD(IAPD {
        id: 1,
        t1: 0,
        t2: 0,
        opts: DhcpOptions::new(),
    }));
    let adv = relay_exchange(&sock, &sol);
    assert_eq!(adv.msg_type(), MessageType::Advertise);

    let prefix = match adv.opts().get(OptionCode::IAPD) {
        Some(DhcpOption::IAPD(iapd)) => match iapd.opts.get(OptionCode::IAPrefix) {
            Some(DhcpOption::IAPrefix(p)) => Some((p.prefix_ip, p.prefix_len)),
            _ => None,
        },
        _ => None,
    };
    let (base, len) = prefix.expect("Advertise carries an IA_PD with an IAPREFIX");
    assert_eq!(len, 64, "delegated /64");
    // the delegated prefix comes from the pd_pool 2001:db8:100::/56
    let (lo, hi): (Ipv6Addr, Ipv6Addr) = (
        "2001:db8:100::".parse().unwrap(),
        "2001:db8:100:ff::".parse().unwrap(),
    );
    assert!(base >= lo && base <= hi, "prefix {base} from the pd_pool");
}
