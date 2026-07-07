//! DHCPv6 relay agent support (RFC 8415 §19).
//!
//! A relay agent wraps a client message in a Relay-forward (type 12) message
//! carrying the client's link address (used to pick the subnet) and peer
//! address, and forwards it to the server. Relays can be chained (a Relay-forward
//! nested inside another). The server processes the innermost client message and
//! returns its answer wrapped in a matching Relay-reply (type 13) chain.
//!
//! The vendored `dhcproto` models the Relay-Message option (opt 9) as raw bytes,
//! so this module decodes the encapsulated message itself: a nested Relay-forward
//! or, at the innermost hop, the client [`v6::Message`].
use std::net::Ipv6Addr;

use dhcproto::{
    Decodable, Decoder, Encodable,
    v6::{self, DhcpOption, MessageType, OptionCode, RelayMessage},
};

/// Guards against a malicious/looping relay chain (RFC 8415 caps hop-count at 32).
const MAX_HOPS: usize = 32;

/// One relay agent's addressing info, needed to rebuild the Relay-reply chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayHop {
    /// the relay's address on the client-facing link (used to select the subnet)
    pub link_addr: Ipv6Addr,
    /// the client (or downstream relay) address this hop forwarded for
    pub peer_addr: Ipv6Addr,
    /// the relay's hop count
    pub hop_count: u8,
    /// Interface-ID (opt 18), echoed verbatim in the Relay-reply if present.
    pub interface_id: Option<Vec<u8>>,
}

/// The chain of relay agents a message passed through, outermost hop first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayChain(pub Vec<RelayHop>);

impl RelayChain {
    /// true if the message was not relayed (no hops)
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// The innermost link address — the client's link — used for subnet selection.
    pub fn client_link(&self) -> Option<Ipv6Addr> {
        self.0.last().map(|h| h.link_addr)
    }
}

/// If `bytes` is a Relay-forward, unwrap the (possibly nested) relay chain and
/// return the innermost client message plus the chain (outermost hop first).
/// Returns `None` if `bytes` is not a relay message or is malformed.
pub fn unwrap(bytes: &[u8]) -> Option<(v6::Message, RelayChain)> {
    if bytes.first().copied() != Some(MessageType::RelayForw.into()) {
        return None;
    }
    let mut chain = Vec::new();
    let mut cur = bytes.to_vec();
    for _ in 0..MAX_HOPS {
        let relay = RelayMessage::decode(&mut Decoder::new(&cur)).ok()?;
        let interface_id = match relay.opts().get(OptionCode::InterfaceId) {
            Some(DhcpOption::InterfaceId(id)) => Some(id.clone()),
            _ => None,
        };
        chain.push(RelayHop {
            link_addr: relay.link_addr(),
            peer_addr: relay.peer_addr(),
            hop_count: relay.hop_count(),
            interface_id,
        });
        // the encapsulated message (a nested Relay-forward or the client message)
        let inner = match relay.opts().get(OptionCode::RelayMsg) {
            Some(DhcpOption::RelayMsg(inner)) => inner.clone(),
            // a Relay-forward with no Relay-Message option is malformed
            _ => return None,
        };
        if inner.first().copied() == Some(MessageType::RelayForw.into()) {
            cur = inner; // nested relay: unwrap the next hop
            continue;
        }
        // innermost hop: decode the client message
        let msg = v6::Message::decode(&mut Decoder::new(&inner)).ok()?;
        return Some((msg, RelayChain(chain)));
    }
    None // exceeded MAX_HOPS
}

/// Wrap a server response into a Relay-reply chain matching `chain`, returning
/// the outermost Relay-reply bytes to send back to the relay agent. Each hop
/// echoes its link/peer address and Interface-ID (RFC 8415 §19.3).
pub fn wrap(resp: &v6::Message, chain: &RelayChain) -> Option<Vec<u8>> {
    let mut inner = resp.to_vec().ok()?;
    // build from the innermost hop outward
    for hop in chain.0.iter().rev() {
        let mut relay = RelayMessage::new(
            MessageType::RelayRepl,
            hop.hop_count,
            hop.link_addr,
            hop.peer_addr,
        );
        relay.opts_mut().insert(DhcpOption::RelayMsg(inner));
        if let Some(id) = &hop.interface_id {
            relay.opts_mut().insert(DhcpOption::InterfaceId(id.clone()));
        }
        inner = relay.to_vec().ok()?;
    }
    Some(inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhcproto::v6::{DhcpOptions, IANA};

    fn client_solicit() -> v6::Message {
        let mut m = v6::Message::new(MessageType::Solicit);
        m.opts_mut().insert(DhcpOption::ClientId(vec![0xaa, 0xbb]));
        m.opts_mut().insert(DhcpOption::IANA(IANA {
            id: 1,
            t1: 0,
            t2: 0,
            opts: DhcpOptions::new(),
        }));
        m
    }

    /// build a single Relay-forward around an inner message
    fn relay_forward(
        link: Ipv6Addr,
        peer: Ipv6Addr,
        hop: u8,
        iface: Option<&[u8]>,
        inner: &[u8],
    ) -> Vec<u8> {
        let mut r = RelayMessage::new(MessageType::RelayForw, hop, link, peer);
        r.opts_mut().insert(DhcpOption::RelayMsg(inner.to_vec()));
        if let Some(id) = iface {
            r.opts_mut().insert(DhcpOption::InterfaceId(id.to_vec()));
        }
        r.to_vec().unwrap()
    }

    #[test]
    fn non_relay_returns_none() {
        assert!(unwrap(&client_solicit().to_vec().unwrap()).is_none());
    }

    #[test]
    fn single_hop_unwrap() {
        let solicit = client_solicit();
        let link: Ipv6Addr = "2001:db8:1::1".parse().unwrap();
        let peer: Ipv6Addr = "fe80::5".parse().unwrap();
        let fwd = relay_forward(link, peer, 0, Some(b"eth0"), &solicit.to_vec().unwrap());

        let (inner, chain) = unwrap(&fwd).expect("should unwrap");
        assert_eq!(inner.msg_type(), MessageType::Solicit);
        assert_eq!(chain.0.len(), 1);
        assert_eq!(chain.client_link(), Some(link));
        assert_eq!(chain.0[0].peer_addr, peer);
        assert_eq!(chain.0[0].interface_id.as_deref(), Some(&b"eth0"[..]));
    }

    #[test]
    fn nested_hops_unwrap_outermost_first() {
        let solicit = client_solicit();
        let inner_link: Ipv6Addr = "2001:db8:1::1".parse().unwrap();
        let outer_link: Ipv6Addr = "2001:db8:2::1".parse().unwrap();
        let peer: Ipv6Addr = "fe80::5".parse().unwrap();

        let hop1 = relay_forward(inner_link, peer, 0, None, &solicit.to_vec().unwrap());
        let hop2 = relay_forward(outer_link, peer, 1, None, &hop1);

        let (inner, chain) = unwrap(&hop2).expect("should unwrap nested");
        assert_eq!(inner.msg_type(), MessageType::Solicit);
        assert_eq!(chain.0.len(), 2);
        // outermost first, so client link is the innermost (last) hop
        assert_eq!(chain.0[0].link_addr, outer_link);
        assert_eq!(chain.client_link(), Some(inner_link));
    }

    #[test]
    fn wrap_roundtrips_and_echoes_interface_id() {
        let solicit = client_solicit();
        let link: Ipv6Addr = "2001:db8:1::1".parse().unwrap();
        let peer: Ipv6Addr = "fe80::5".parse().unwrap();
        let fwd = relay_forward(link, peer, 3, Some(b"if42"), &solicit.to_vec().unwrap());
        let (_inner, chain) = unwrap(&fwd).unwrap();

        // server answers with a Reply
        let reply = v6::Message::new(MessageType::Reply);
        let wrapped = wrap(&reply, &chain).expect("should wrap");

        // the wrapped bytes are a Relay-reply echoing link/peer/iface, carrying Reply
        let outer = RelayMessage::decode(&mut Decoder::new(&wrapped)).unwrap();
        assert_eq!(outer.msg_type(), MessageType::RelayRepl);
        assert_eq!(outer.link_addr(), link);
        assert_eq!(outer.peer_addr(), peer);
        assert_eq!(outer.hop_count(), 3);
        match outer.opts().get(OptionCode::InterfaceId) {
            Some(DhcpOption::InterfaceId(id)) => assert_eq!(id, b"if42"),
            _ => panic!("interface-id must be echoed"),
        }
        match outer.opts().get(OptionCode::RelayMsg) {
            Some(DhcpOption::RelayMsg(bytes)) => {
                let inner = v6::Message::decode(&mut Decoder::new(bytes)).unwrap();
                assert_eq!(inner.msg_type(), MessageType::Reply);
            }
            _ => panic!("relay-reply must carry the response"),
        }
    }
}
