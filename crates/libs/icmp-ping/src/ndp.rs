//! IPv6 Neighbor Discovery (RFC 4861) duplicate-address detection.
//!
//! Unlike the ICMPv6 *echo* probe (which a host may filter), a Neighbor
//! Solicitation is the mechanism IPv6 stacks use for address resolution, so a
//! host that owns the target address will reliably answer with a Neighbor
//! Advertisement. We send an NS for a candidate address to its solicited-node
//! multicast group and treat a matching NA (within a timeout) as "in use".
//!
//! This needs a RAW ICMPv6 socket (the unprivileged DGRAM "ping" socket is
//! echo-only) and, because the destination is a link-local multicast, an
//! interface scope id on the send. The kernel fills the ICMPv6 checksum for
//! `IPPROTO_ICMPV6` raw sockets and picks a link-local source on the scoped
//! interface, so we only hand-build the small NS body.

use std::{
    collections::HashMap,
    io,
    net::{Ipv6Addr, SocketAddr, SocketAddrV6},
    sync::Arc,
    time::Duration,
};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::{
    net::UdpSocket,
    sync::{Mutex, oneshot},
    time,
};
use tracing::warn;

/// ICMPv6 Neighbor Solicitation message type (RFC 4861 §4.3).
const ND_NEIGHBOR_SOLICIT: u8 = 135;
/// ICMPv6 Neighbor Advertisement message type (RFC 4861 §4.4).
const ND_NEIGHBOR_ADVERT: u8 = 136;
/// ICMPv6 header (4) + reserved (4) + target address (16).
const NS_LEN: usize = 24;

/// The solicited-node multicast address for `target`: `ff02::1:ffXX:XXXX`,
/// formed from the low 24 bits of the address (RFC 4291 §2.7.1).
pub fn solicited_node_multicast(target: Ipv6Addr) -> Ipv6Addr {
    let o = target.octets();
    Ipv6Addr::new(
        0xff02,
        0,
        0,
        0,
        0,
        1,
        0xff00 | o[13] as u16,
        (o[14] as u16) << 8 | o[15] as u16,
    )
}

/// Build a Neighbor Solicitation querying `target`. The checksum is left zero:
/// the kernel computes it for `IPPROTO_ICMPV6` raw sockets. No source
/// link-layer-address option is included — the responder unicasts the NA back
/// to the packet's (kernel-chosen) IPv6 source.
pub fn build_neighbor_solicit(target: Ipv6Addr) -> [u8; NS_LEN] {
    let mut buf = [0u8; NS_LEN];
    buf[0] = ND_NEIGHBOR_SOLICIT;
    // bytes 1..8 stay zero (code, checksum, reserved)
    buf[8..24].copy_from_slice(&target.octets());
    buf
}

/// If `buf` is a Neighbor Advertisement, return the address it advertises (its
/// target field). `buf` starts at the ICMPv6 header — IPv6 raw sockets do not
/// deliver the IP header.
pub fn parse_neighbor_advert(buf: &[u8]) -> Option<Ipv6Addr> {
    if buf.len() >= NS_LEN && buf[0] == ND_NEIGHBOR_ADVERT {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&buf[8..24]);
        Some(Ipv6Addr::from(octets))
    } else {
        None
    }
}

type Waiters = Arc<Mutex<HashMap<Ipv6Addr, oneshot::Sender<()>>>>;

/// Sends Neighbor Solicitations and matches the Neighbor Advertisements that
/// come back, for duplicate-address detection.
#[derive(Debug)]
pub struct NeighborSolicitor {
    socket: Arc<UdpSocket>,
    waiters: Waiters,
}

impl NeighborSolicitor {
    /// Open the RAW ICMPv6 socket and spawn the receive loop. Fails (so callers
    /// can fall back / skip DAD) when a raw socket can't be created — e.g. the
    /// process lacks `CAP_NET_RAW`, or the environment has no ICMPv6.
    pub fn new() -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))?;
        socket.set_nonblocking(true)?;
        // RFC 4861 §7.1.2: ND messages MUST use hop limit 255; receivers discard
        // any with a lower value.
        socket.set_multicast_hops_v6(255)?;
        socket.set_unicast_hops_v6(255)?;

        let std_socket: std::net::UdpSocket = socket.into();
        let socket = Arc::new(UdpSocket::from_std(std_socket)?);
        let waiters: Waiters = Arc::default();

        // receive loop: dispatch each NA to a waiter keyed on its target address
        let (rx_socket, rx_waiters) = (Arc::clone(&socket), Arc::clone(&waiters));
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                match rx_socket.recv_from(&mut buf).await {
                    Ok((n, _src)) => {
                        if let Some(target) = parse_neighbor_advert(&buf[..n])
                            && let Some(tx) = rx_waiters.lock().await.remove(&target)
                        {
                            let _ = tx.send(());
                        }
                    }
                    Err(err) => {
                        // a transient recv error (e.g. ENOBUFS) must not
                        // permanently disable DAD — keep the loop alive, but
                        // throttle so a persistent error can't hot-spin.
                        warn!(?err, "ICMPv6 ND receive error; continuing");
                        time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });

        Ok(Self { socket, waiters })
    }

    /// Probe `target` on the interface `scope_id`. Returns `Ok(true)` if a
    /// Neighbor Advertisement for it arrives within `timeout` (the address is in
    /// use), `Ok(false)` otherwise.
    pub async fn probe(
        &self,
        target: Ipv6Addr,
        scope_id: u32,
        timeout: Duration,
    ) -> io::Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(target, tx);

        let ns = build_neighbor_solicit(target);
        let dst = SocketAddr::V6(SocketAddrV6::new(
            solicited_node_multicast(target),
            0,
            0,
            scope_id,
        ));
        if let Err(err) = self.socket.send_to(&ns, dst).await {
            self.waiters.lock().await.remove(&target);
            return Err(err);
        }

        let in_use = matches!(time::timeout(timeout, rx).await, Ok(Ok(())));
        // drop the waiter if it's still there (timeout / send race)
        self.waiters.lock().await.remove(&target);
        Ok(in_use)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solicited_node_from_low_24_bits() {
        // RFC 4291 example: solicited-node of 4037::01:800:200e:8c6c
        let target: Ipv6Addr = "2001:db8::11:2233:4455".parse().unwrap();
        assert_eq!(
            solicited_node_multicast(target),
            "ff02::1:ff33:4455".parse::<Ipv6Addr>().unwrap()
        );
    }

    #[test]
    fn ns_has_type_and_target() {
        let target: Ipv6Addr = "2001:db8:1::100".parse().unwrap();
        let ns = build_neighbor_solicit(target);
        assert_eq!(ns[0], ND_NEIGHBOR_SOLICIT);
        assert_eq!(&ns[8..24], &target.octets());
        // checksum + reserved left for the kernel / spec (zero)
        assert_eq!(&ns[1..8], &[0u8; 7]);
    }

    #[test]
    fn parses_matching_advert() {
        let target: Ipv6Addr = "2001:db8:1::100".parse().unwrap();
        // a minimal NA: type 136, then flags(4) + target(16)
        let mut na = [0u8; NS_LEN];
        na[0] = ND_NEIGHBOR_ADVERT;
        na[8..24].copy_from_slice(&target.octets());
        assert_eq!(parse_neighbor_advert(&na), Some(target));
    }

    #[test]
    fn ignores_non_advert_and_short() {
        let mut ns = [0u8; NS_LEN];
        ns[0] = ND_NEIGHBOR_SOLICIT; // not an advert
        assert_eq!(parse_neighbor_advert(&ns), None);
        assert_eq!(parse_neighbor_advert(&[136, 0, 0]), None); // too short
    }
}
