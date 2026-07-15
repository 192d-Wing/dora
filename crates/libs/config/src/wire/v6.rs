use base64::Engine;
use dora_core::{
    dhcproto::{
        Decodable, Decoder, Encodable, Encoder,
        v6::{DhcpOption, DhcpOptions, EncodeResult, NtpSuboption, OptionCode},
    },
    hickory_proto::{
        rr::Name,
        serialize::binary::{BinEncodable, BinEncoder, NameEncoding},
    },
};
use ipnet::Ipv6Net;
use serde::{Deserialize, Deserializer, Serialize, de};
use tracing::warn;

use std::{collections::HashMap, net::Ipv6Addr, ops::RangeInclusive};

use crate::{
    v6::DEFAULT_SERVER_ID_FILE_PATH,
    wire::{Interface, MaybeList, MinMax},
};

/// top-level config type
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct Config {
    pub interfaces: Option<Vec<Interface>>,
    pub server_id: Option<ServerDuid>,
    pub networks: HashMap<Ipv6Net, Net>,
    // TODO: better defaults than blank? pull information from the system
    /// global DHCPv6 options: applied to every network unless the network (or
    /// its referenced `policy`) sets the same option code.
    #[serde(default)]
    pub options: Option<Options>,
    /// named, reusable option-sets ("policies"). Reference one by name via the
    /// `policy` key on a network to apply its options.
    #[serde(default)]
    pub policies: HashMap<String, Options>,
    /// DHCPv6 client classes. Matched-class options are merged into responses
    /// below the explicitly-configured options.
    #[serde(default)]
    pub client_classes: Vec<crate::wire::client_classes::ClientClassV6>,
    /// honor the Rapid Commit option (opt 14): answer a Solicit that carries it
    /// with a committing Reply instead of Advertise. RFC 8415 §18.3.1.
    #[serde(default = "super::default_rapid_commit")]
    pub rapid_commit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Net {
    pub config: NetworkConfig,
    #[serde(default)]
    pub options: Options,
    /// name of a policy (see [`Config::policies`]) whose options apply to this
    /// network. Overridden by `options` set at any level within the network.
    #[serde(default)]
    pub policy: Option<String>,
    pub interfaces: Option<Vec<String>>,
    /// address pools used for IA_NA (non-temporary address) assignment.
    /// RFC 8415 stateful DHCPv6.
    #[serde(default)]
    pub ranges: Vec<IpRange>,
    /// prefix pools used for IA_PD (prefix delegation). RFC 8415 §6.3.
    #[serde(default)]
    pub pd_pools: Vec<PdPool>,
    /// ping check is an optional value, when turned on an ICMP echo request will be sent
    /// before OFFER for this network
    #[serde(default)]
    pub ping_check: bool,
    /// default ping timeout in ms
    #[serde(default = "super::default_ping_to")]
    pub ping_timeout_ms: u64,
    /// probation period in seconds
    #[serde(default = "super::default_probation")]
    pub probation_period: u64,
    /// Whether we are authoritative for this network (default: true)
    #[serde(default = "super::default_authoritative")]
    pub authoritative: bool,
}

/// A prefix delegation pool: a parent prefix carved into equal-length
/// delegated prefixes handed out via IA_PD. e.g. delegate /64s out of a /48.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PdPool {
    /// the parent prefix to delegate from, e.g. `2001:db8:1::/48`
    pub prefix: Ipv6Net,
    /// length of the prefixes delegated to clients, e.g. `64`.
    /// must be greater than the parent `prefix` length and <= 128.
    pub delegated_len: u8,
    #[serde(default)]
    pub options: Options,
    #[serde(default)]
    pub config: Option<NetworkConfig>,
    /// delegated prefixes to skip (never hand out)
    #[serde(default)]
    pub except: Vec<Ipv6Net>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Default)]
pub enum DuidType {
    #[default]
    LLT,
    LL,
    EN,
    UUID,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(tag = "type")]
pub enum ServerDuidInfo {
    LLT {
        #[serde(default)]
        htype: u16,
        #[serde(default)]
        time: u32,
        #[serde(default)]
        identifier: String,
    },
    LL {
        #[serde(default)]
        htype: u16,
        #[serde(default)]
        identifier: String,
    },
    EN {
        #[serde(default)]
        enterprise_id: u32,
        #[serde(default)]
        identifier: String,
    },
    UUID {
        // identifier must be supplied for UUID
        identifier: String,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(tag = "type")]
pub struct ServerDuid {
    #[serde(flatten)]
    pub info: ServerDuidInfo,
    #[serde(default = "default_persist")]
    pub persist: bool,
    #[serde(default = "default_path")]
    pub path: String,
}

fn default_persist() -> bool {
    true
}

fn default_path() -> String {
    DEFAULT_SERVER_ID_FILE_PATH.to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct NetworkConfig {
    pub lease_time: MinMax,
    pub preferred_time: MinMax,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct IpRange {
    // RangeInclusive includes `start`/`end` so flatten will parse those fields
    #[serde(flatten)]
    pub range: RangeInclusive<Ipv6Addr>,
    #[serde(default)]
    pub options: Options,
    #[serde(default)]
    pub config: Option<NetworkConfig>,
    #[serde(default)]
    pub except: Vec<Ipv6Addr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct Options {
    pub values: Opts,
}

impl Options {
    pub fn get(self) -> DhcpOptions {
        self.values.0
    }
}

impl AsRef<DhcpOptions> for Options {
    fn as_ref(&self) -> &DhcpOptions {
        &self.values.0
    }
}

impl From<Options> for DhcpOptions {
    fn from(o: Options) -> Self {
        o.values.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Opts(pub DhcpOptions);

/// this type is only used as an intermediate representation
/// Opts are received as essentially a HashMap<u8, Opt>
/// and transformed into DhcpOptions
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum Opt {
    Ip(MaybeList<Ipv6Addr>),
    IpList(Vec<Ipv6Addr>),
    U8(MaybeList<u8>),
    U32(MaybeList<u32>),
    U16(MaybeList<u16>),
    Str(MaybeList<String>),
    /// DNS wire-format domain names (e.g. option 24 Domain Search List).
    /// Accepts a single name or list: `"example.com"` or `["a.com", "b.com"]`
    Domain(MaybeList<String>),
    /// NTP server as an IPv6 address (option 56, suboption 1).
    NtpAddr(MaybeList<Ipv6Addr>),
    /// NTP server as an FQDN (option 56, suboption 3).
    NtpFqdn(MaybeList<String>),
    B64(String),
    Hex(String),
}

impl<'de> serde::Deserialize<'de> for Opts {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // decode what was on the wire to a map
        let map: HashMap<u16, Opt> = Deserialize::deserialize(de)?;
        // we'll encode the map to buf so we can use DhcpOptions::decode
        let mut buf = vec![];
        let mut enc = Encoder::new(&mut buf);
        for (code, opt) in map {
            write_opt(&mut enc, code, opt).map_err(de::Error::custom)?;
        }

        // buffer now has binary data for DhcpOptions -- decode it
        let opts = DhcpOptions::decode(&mut Decoder::new(&buf)).map_err(de::Error::custom)?;
        Ok(Self(opts))
    }
}

fn encode_opt<'a, T, F>(data: &[T], f: F, e: &mut Encoder<'a>) -> EncodeResult<()>
where
    F: Fn(&T, &mut Encoder<'a>) -> EncodeResult<()>,
{
    // size_of_val removes data.len() * mem::size_of::<T>()
    e.write_u16((std::mem::size_of_val(data)) as u16)?;
    for thing in data {
        f(thing, e)?;
    }
    Ok(())
}

fn write_opt(enc: &mut Encoder<'_>, code: u16, opt: Opt) -> anyhow::Result<()> {
    enc.write_u16(code)?;
    match opt {
        Opt::Ip(MaybeList::Val(ip)) => {
            enc.write_u16(16)?;
            enc.write_u128(ip.into())?;
        }
        Opt::IpList(list) | Opt::Ip(MaybeList::List(list)) => {
            enc.write_u16(list.len() as u16 * 16)?;
            for ip in list {
                enc.write_u128(ip.into())?;
            }
        }
        Opt::U8(MaybeList::Val(n)) => {
            enc.write_u16(1)?;
            enc.write_u8(n)?;
        }
        Opt::U8(MaybeList::List(list)) => {
            enc.write_u16(list.len() as u16)?;
            enc.write_slice(&list)?;
        }
        Opt::U32(MaybeList::Val(n)) => {
            enc.write_u16(4)?;
            enc.write_u32(n)?;
        }
        Opt::U32(MaybeList::List(list)) => {
            encode_opt(&list, |n, e| e.write_u32(*n), enc)?;
        }
        Opt::U16(MaybeList::Val(n)) => {
            enc.write_u16(2)?;
            enc.write_u16(n)?;
        }
        Opt::U16(MaybeList::List(list)) => {
            encode_opt(&list, |n, e| e.write_u16(*n), enc)?;
        }
        Opt::Str(MaybeList::Val(s)) => {
            enc.write_u16(s.len() as u16)?;
            enc.write_slice(s.as_bytes())?;
        }
        Opt::Str(MaybeList::List(list)) => {
            encode_opt(&list, |n, e| e.write_slice(n.as_bytes()), enc)?;
        }
        Opt::Domain(MaybeList::Val(domain)) => {
            let mut buf = Vec::new();
            let mut name_encoder = BinEncoder::new(&mut buf);
            name_encoder.set_name_encoding(NameEncoding::Uncompressed);
            let name = domain.parse::<Name>()?;
            name.emit(&mut name_encoder)?;
            enc.write_u16(buf.len() as u16)?;
            enc.write_slice(&buf)?;
        }
        Opt::Domain(MaybeList::List(list)) => {
            let mut buf = Vec::new();
            let mut name_encoder = BinEncoder::new(&mut buf);
            name_encoder.set_name_encoding(NameEncoding::Uncompressed);
            for name in list {
                let name = name.parse::<Name>()?;
                name.emit(&mut name_encoder)?;
            }
            enc.write_u16(buf.len() as u16)?;
            enc.write_slice(&buf)?;
        }
        Opt::NtpAddr(MaybeList::Val(ip)) => {
            let mut buf = Vec::new();
            NtpSuboption::ServerAddress(ip).encode(&mut Encoder::new(&mut buf))?;
            enc.write_u16(buf.len() as u16)?;
            enc.write_slice(&buf)?;
        }
        Opt::NtpAddr(MaybeList::List(list)) => {
            let mut buf = Vec::new();
            let mut subopt_enc = Encoder::new(&mut buf);
            for ip in list {
                NtpSuboption::ServerAddress(ip).encode(&mut subopt_enc)?;
            }
            enc.write_u16(buf.len() as u16)?;
            enc.write_slice(&buf)?;
        }
        Opt::NtpFqdn(MaybeList::Val(fqdn)) => {
            let name = fqdn.parse::<Name>()?;
            let mut buf = Vec::new();
            NtpSuboption::FQDN(name).encode(&mut Encoder::new(&mut buf))?;
            enc.write_u16(buf.len() as u16)?;
            enc.write_slice(&buf)?;
        }
        Opt::NtpFqdn(MaybeList::List(list)) => {
            let mut buf = Vec::new();
            let mut subopt_enc = Encoder::new(&mut buf);
            for fqdn in list {
                let name = fqdn.parse::<Name>()?;
                NtpSuboption::FQDN(name).encode(&mut subopt_enc)?;
            }
            enc.write_u16(buf.len() as u16)?;
            enc.write_slice(&buf)?;
        }
        Opt::B64(s) => {
            let bytes = base64::engine::general_purpose::STANDARD_NO_PAD.decode(s)?;
            enc.write_u16(bytes.len() as u16)?;
            enc.write_slice(&bytes)?;
        }
        Opt::Hex(s) => {
            let bytes = hex::decode(s)?;
            enc.write_u16(bytes.len() as u16)?;
            enc.write_slice(&bytes)?;
        }
    }
    Ok(())
}

// NOTE: this will be used in tests, so a complete mapping of different
// opt types is not necessary. Using B64, everything will still be decoded
// to it's proper type
impl Serialize for Opts {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let map = self
            .0
            .iter()
            .filter_map(decode_opt)
            .collect::<HashMap<u16, Opt>>();
        ser.collect_map(&map)
    }
}

fn decode_opt(opt: &DhcpOption) -> Option<(u16, Opt)> {
    use dora_core::dhcproto::v6::DhcpOption::*;
    let code: OptionCode = opt.into();
    match opt {
        // inspiration: https://kea.readthedocs.io/en/kea-2.2.0/arm/dhcp6-srv.html?highlight=router%20advertisement#dhcp6-std-options-list
        Preference(n) => Some((code.into(), Opt::U8(MaybeList::Val(*n)))),
        ServerUnicast(ip) => Some((code.into(), Opt::Ip(MaybeList::Val(*ip)))),
        DomainNameServers(addrs) => Some((code.into(), Opt::Ip(MaybeList::List(addrs.clone())))),
        DomainSearchList(names) => Some((
            code.into(),
            Opt::Domain(MaybeList::List(
                names.iter().map(|n| n.to_string()).collect(),
            )),
        )),
        NtpServer(subopts) => {
            let mut fqdns = Vec::new();
            let mut addrs = Vec::new();
            for s in subopts {
                match s {
                    NtpSuboption::FQDN(name) => fqdns.push(name.to_string()),
                    NtpSuboption::ServerAddress(ip) | NtpSuboption::MulticastAddress(ip) => {
                        addrs.push(*ip)
                    }
                }
            }
            if !fqdns.is_empty() && addrs.is_empty() {
                Some((code.into(), Opt::NtpFqdn(MaybeList::List(fqdns))))
            } else if !addrs.is_empty() && fqdns.is_empty() {
                Some((code.into(), Opt::NtpAddr(MaybeList::List(addrs))))
            } else if !subopts.is_empty() {
                let mut buf = Vec::new();
                let mut sub_enc = Encoder::new(&mut buf);
                for s in subopts {
                    if let Err(err) = s.encode(&mut sub_enc) {
                        warn!(?err, "failed to encode NTP suboption");
                        return None;
                    }
                }
                Some((code.into(), Opt::Hex(hex::encode(&buf))))
            } else {
                None
            }
        }
        Unknown(opt) => Some((code.into(), Opt::Hex(hex::encode(opt.data())))),
        _ => {
            // the data includes the code value, let's slice that off
            match opt.to_vec() {
                Ok(buf) => Some((
                    code.into(),
                    Opt::Hex(if buf.is_empty() {
                        "".into()
                    } else {
                        hex::encode(&buf[1..])
                    }),
                )),
                Err(err) => {
                    warn!(?err);
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode() {
        let mut buf = Vec::new();
        let mut e = Encoder::new(&mut buf);
        let opt = Opt::Ip(MaybeList::List(vec![
            Ipv6Addr::UNSPECIFIED,
            Ipv6Addr::LOCALHOST,
        ]));
        write_opt(&mut e, 23, opt).unwrap();
        dbg!(std::mem::size_of::<Ipv6Addr>());
        assert_eq!(
            // [<2 byte code><2 byte len><data>]
            &[
                0, 23, // code
                0, 32, // len in bytes
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // first addr
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1 // second addr
            ],
            &buf[..]
        );
    }
}
