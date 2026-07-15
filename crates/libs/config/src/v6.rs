use std::{
    collections::{HashMap, HashSet},
    net::Ipv6Addr,
    ops::RangeInclusive,
    path::Path,
    str::FromStr,
    time::{Duration, SystemTime},
};

use anyhow::{Context, bail};
use dora_core::{
    anyhow::Result,
    dhcproto::v6::{DhcpOptions, HType, Message, duid::Duid},
    pnet::ipnetwork::{IpNetwork, Ipv6Network},
    pnet::{self, datalink::NetworkInterface},
};
use ipnet::{Ipv6AddrRange, Ipv6Net};
use tracing::debug;

use crate::{
    LeaseTime, PersistIdentifier,
    client_classes::ClientClassesV6,
    generate_random_bytes,
    wire::{self, v6::ServerDuidInfo},
};
/// the default path to  server identifier file path
pub static DEFAULT_SERVER_ID_FILE_PATH: &str = "/var/lib/dora/server_id";
// const DEFAULT_VALID: Duration = Duration::from_secs(12 * 24 * 60 * 60); // 12 days
// const DEFAULT_PREFERRED: Duration = Duration::from_secs(8 * 24 * 60 * 60); // 8 days

/// server config for dhcpv6
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// interfaces that are either explicitly bound by the config or
    /// are up & ipv6
    interfaces: Vec<NetworkInterface>,
    /// global dhcp options
    opts: Option<DhcpOptions>,
    /// used to make a selection on which network or subnet to use
    networks: HashMap<Ipv6Net, Network>,
    server_id: Duid,
    /// whether to honor the Rapid Commit option (opt 14)
    rapid_commit: bool,
    /// v6 client classes (from `v6.client_classes`)
    client_classes: Option<ClientClassesV6>,
}

impl Config {
    /// return server id as a slice of bytes
    pub fn server_id(&self) -> &[u8] {
        self.server_id.as_ref()
    }
    /// whether the server honors the Rapid Commit option (opt 14)
    pub fn rapid_commit(&self) -> bool {
        self.rapid_commit
    }
    /// return the optional explicitly bound interfaces if there are any
    pub fn interfaces(&self) -> &[NetworkInterface] {
        self.interfaces.as_slice()
    }
    /// Returns:
    ///     - if the config has an interface, return that
    ///     - OR find iface_index and return that
    ///     - OR use default interface
    pub fn get_interface_global(&self, iface_index: u32) -> Option<Ipv6Network> {
        self.find_interface(iface_index).and_then(|int| {
            int.ips.iter().find_map(|ip| match ip {
                IpNetwork::V6(ip) if is_unicast_global(&ip.ip()) => Some(*ip),
                _ => None,
            })
        })
    }
    pub fn get_interface_link_local(&self, iface_index: u32) -> Option<Ipv6Network> {
        self.find_interface(iface_index).and_then(|int| {
            int.ips.iter().find_map(|ip| match ip {
                IpNetwork::V6(ip) if is_unicast_link_local(&ip.ip()) => Some(*ip),
                _ => None,
            })
        })
    }
    pub fn get_interface_ips(&self, iface_index: u32) -> Option<Vec<Ipv6Network>> {
        self.find_interface(iface_index).map(|int| {
            int.ips
                .iter()
                .filter_map(|ip| match ip {
                    IpNetwork::V6(ip) => Some(*ip),
                    _ => None,
                })
                .collect()
        })
    }
    // find the interface at the index `iface_index`
    fn find_interface(&self, iface_index: u32) -> Option<&NetworkInterface> {
        self.interfaces.iter().find(|e| e.index == iface_index)
    }

    /// get a `Network` configured for a given interface index. If the config doesn't specify
    /// an interface, use the IPs (local/global) of the receiving interface
    pub fn get_network(&self, iface_index: u32) -> Option<&Network> {
        let ifs = self.get_interface_ips(iface_index)?;
        self.networks.iter().find_map(|(subnet, network)| {
            // if the configured interface index matches the one we received the packet on
            if matches!(&network.interfaces, Some(ints) if ints.iter().any(|i| i.index == iface_index)) {
                return Some(network);
            }
            if ifs.iter().any(|ip| subnet.contains(&ip.ip())) { // or if no configured interfaces, one of the subnets matches (either global or link-local)
                return Some(network);
            }
            None
        })
    }

    /// find the network whose subnet contains `addr`. Used for relayed messages,
    /// where the relay's link-address identifies the client's link/subnet
    /// (RFC 8415 §13.1) rather than the interface the packet was received on.
    pub fn get_network_by_addr(&self, addr: Ipv6Addr) -> Option<&Network> {
        self.networks
            .iter()
            .find_map(|(subnet, net)| subnet.contains(&addr).then_some(net))
    }

    /// gets options (which have been already merged with global opts) for the network of `iface_index` or the global options
    pub fn get_opts(&self, iface_index: u32) -> Option<&DhcpOptions> {
        self.get_network(iface_index)
            .map(|n| n.opts())
            .or(self.opts.as_ref())
    }

    /// get the first `Network`
    pub fn get_first(&self) -> Option<(&Ipv6Net, &Network)> {
        self.networks.iter().next()
    }

    /// evaluate all v6 client classes, returning the names of those that match.
    /// Returns `None` when no v6 classes are configured (so callers can skip the
    /// per-packet option-map build on the common no-classes path).
    pub fn eval_client_classes(&self, req: &Message) -> Option<Result<Vec<String>>> {
        self.client_classes
            .as_ref()
            .map(|classes| classes.eval(req))
    }

    /// merge matched v6 client-class options over `opts` (client-class wins):
    /// class options override codes already present in `opts`.
    pub fn collect_opts(
        &self,
        opts: &DhcpOptions,
        matched_classes: Option<&[String]>,
    ) -> DhcpOptions {
        match self
            .client_classes
            .as_ref()
            .and_then(|classes| classes.collect_opts(matched_classes))
        {
            Some(class_opts) => merge_opts(&class_opts, opts.clone()),
            None => opts.clone(),
        }
    }
}

/// merge `b` into `a`, `a` takes priority where there are duplicates
fn merge_opts(a: &DhcpOptions, b: DhcpOptions) -> DhcpOptions {
    let mut opts = a.clone();
    for opt in b.iter() {
        if opts.get(opt.into()).is_none() {
            opts.insert(opt.clone());
        }
    }
    opts
}

/// Look up a named policy's options, erroring if the referenced name is unknown.
/// Returns empty options when no policy is referenced.
fn policy_opts(
    policies: &HashMap<String, wire::v6::Options>,
    name: Option<&str>,
) -> Result<DhcpOptions> {
    match name {
        None => Ok(DhcpOptions::default()),
        Some(name) => policies
            .get(name)
            .map(|o| o.as_ref().clone())
            .ok_or_else(|| anyhow::anyhow!("unknown policy `{name}` referenced in v6 config")),
    }
}

/// Fold broader-scoped option layers into `acc` (most-specific wins): any option
/// code already present in `acc` shadows the same code from a broader layer.
/// `broader` is ordered from more- to less-specific.
fn fold_opts(mut acc: DhcpOptions, broader: &[&DhcpOptions]) -> DhcpOptions {
    for layer in broader {
        acc = merge_opts(&acc, (*layer).clone());
    }
    acc
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    interfaces: Option<Vec<NetworkInterface>>,
    subnet: Ipv6Net,
    valid: LeaseTime,
    preferred: LeaseTime,
    options: DhcpOptions,
    /// address pools available for IA_NA assignment on this network
    ranges: Vec<NetRange>,
    /// prefix pools available for IA_PD delegation on this network
    pd_pools: Vec<PdPool>,
    ping_check: bool,
    /// default ping timeout in ms
    ping_timeout_ms: Duration,
    /// probation period in seconds
    probation_period: Duration,
    /// Whether we are authoritative for this network (default: true)
    authoritative: bool,
}

impl Network {
    pub fn subnet(&self) -> Ipv6Addr {
        self.subnet.network()
    }
    /// the full subnet (prefix + length) this network owns
    pub fn full_subnet(&self) -> Ipv6Net {
        self.subnet
    }
    pub fn authoritative(&self) -> bool {
        self.authoritative
    }
    /// index of the first interface explicitly bound to this network, if any.
    /// Used to scope a link-local Neighbor Solicitation for v6 DAD.
    pub fn iface_index(&self) -> Option<u32> {
        self.interfaces.as_ref()?.first().map(|i| i.index)
    }
    /// address pools available for IA_NA assignment
    pub fn ranges(&self) -> &[NetRange] {
        &self.ranges
    }
    /// prefix pools available for IA_PD delegation
    pub fn pd_pools(&self) -> &[PdPool] {
        &self.pd_pools
    }
    /// returns the range that contains `ip`, if any (not in its exclude set)
    pub fn range(&self, ip: Ipv6Addr) -> Option<&NetRange> {
        self.ranges.iter().find(|r| r.contains(&ip))
    }
    /// default valid lifetime for this network
    pub fn valid(&self) -> LeaseTime {
        self.valid
    }
    /// default preferred lifetime for this network
    pub fn preferred(&self) -> LeaseTime {
        self.preferred
    }
    /// is ping check enabled for this range? should we ping an IP before offering?
    pub fn ping_check(&self) -> bool {
        self.ping_check
    }
    /// get the ping timeout
    pub fn ping_timeout(&self) -> Duration {
        self.ping_timeout_ms
    }
    /// Returns the configured probation period for decline's received on this network
    pub fn probation_period(&self) -> Duration {
        self.probation_period
    }
    /// return options configured for this network
    pub fn opts(&self) -> &DhcpOptions {
        &self.options
    }
}

/// An address pool used for IA_NA assignment. Mirrors the v4 `NetRange` but
/// carries both a `valid` and a `preferred` lifetime as required by DHCPv6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetRange {
    addrs: RangeInclusive<Ipv6Addr>,
    /// valid lifetime for addresses in this range
    valid: LeaseTime,
    /// preferred lifetime for addresses in this range
    preferred: LeaseTime,
    opts: DhcpOptions,
    exclude: HashSet<Ipv6Addr>,
}

impl NetRange {
    /// the (inclusive) range of addresses this pool offers
    pub fn addrs(&self) -> RangeInclusive<Ipv6Addr> {
        self.addrs.clone()
    }
    pub fn start(&self) -> Ipv6Addr {
        *self.addrs.start()
    }
    pub fn end(&self) -> Ipv6Addr {
        *self.addrs.end()
    }
    /// options to include for addresses from this range
    pub fn opts(&self) -> &DhcpOptions {
        &self.opts
    }
    /// valid lifetime config for this range
    pub fn valid(&self) -> LeaseTime {
        self.valid
    }
    /// preferred lifetime config for this range
    pub fn preferred(&self) -> LeaseTime {
        self.preferred
    }
    /// true if `ip` is within the range and not excluded
    pub fn contains(&self, ip: &Ipv6Addr) -> bool {
        !self.exclude.contains(ip) && self.addrs.contains(ip)
    }
    /// the excluded addresses for this range
    pub fn exclusions(&self) -> &HashSet<Ipv6Addr> {
        &self.exclude
    }
    /// iterate the assignable addresses in the range, skipping exclusions
    pub fn iter(&self) -> impl Iterator<Item = Ipv6Addr> + '_ {
        Ipv6AddrRange::new(self.start(), self.end()).filter(move |ip| !self.exclude.contains(ip))
    }
}

impl NetRange {
    fn from_wire(r: wire::v6::IpRange, net_config: &wire::v6::NetworkConfig) -> Self {
        let cfg = r.config.unwrap_or_else(|| net_config.clone());
        Self {
            addrs: r.range,
            valid: cfg.lease_time.into(),
            preferred: cfg.preferred_time.into(),
            opts: r.options.get(),
            exclude: r.except.into_iter().collect(),
        }
    }
}

/// A prefix delegation pool used for IA_PD. RFC 8415 §6.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdPool {
    /// parent prefix delegated prefixes are carved from
    prefix: Ipv6Net,
    /// length of prefixes delegated to clients (> prefix length, <= 128)
    delegated_len: u8,
    valid: LeaseTime,
    preferred: LeaseTime,
    opts: DhcpOptions,
    /// delegated prefixes that should never be handed out
    except: Vec<Ipv6Net>,
}

impl PdPool {
    /// the parent prefix
    pub fn prefix(&self) -> Ipv6Net {
        self.prefix
    }
    /// length of prefixes delegated to clients
    pub fn delegated_len(&self) -> u8 {
        self.delegated_len
    }
    pub fn opts(&self) -> &DhcpOptions {
        &self.opts
    }
    pub fn valid(&self) -> LeaseTime {
        self.valid
    }
    pub fn preferred(&self) -> LeaseTime {
        self.preferred
    }
    /// prefixes excluded from delegation
    pub fn exclusions(&self) -> &[Ipv6Net] {
        &self.except
    }
    /// total number of prefixes this pool can delegate (before exclusions)
    pub fn total_prefixes(&self) -> u128 {
        let bits = self.delegated_len.saturating_sub(self.prefix.prefix_len());
        if bits >= 128 {
            u128::MAX
        } else {
            1u128 << bits
        }
    }
    /// lazily iterate the delegated prefix base addresses (skipping any in the
    /// `except` list). The iterator can be very long for wide pools, so callers
    /// should bound how many candidates they scan.
    pub fn iter_prefixes(&self) -> impl Iterator<Item = Ipv6Addr> + '_ {
        let dlen = self.delegated_len;
        let base = u128::from(self.prefix.network());
        // number of delegated blocks = 2^(dlen - plen); step between block bases
        // = 2^(128 - dlen)
        let count = self.total_prefixes();
        let step: u128 = if dlen >= 128 {
            1
        } else {
            1u128 << (128 - dlen)
        };
        (0..count)
            .map(move |i| Ipv6Addr::from(base.wrapping_add(i.wrapping_mul(step))))
            .filter(move |ip| !self.except.iter().any(|ex| ex.contains(ip)))
    }
}

impl PdPool {
    fn from_wire(p: wire::v6::PdPool, net_config: &wire::v6::NetworkConfig) -> Result<Self> {
        if p.delegated_len <= p.prefix.prefix_len() {
            bail!(
                "pd_pool delegated_len ({}) must be greater than the parent prefix length ({}) for prefix {}",
                p.delegated_len,
                p.prefix.prefix_len(),
                p.prefix
            );
        }
        if p.delegated_len >= 128 {
            bail!(
                "pd_pool delegated_len ({}) must be < 128 (a delegated prefix cannot be a single address)",
                p.delegated_len
            );
        }
        let cfg = p.config.unwrap_or_else(|| net_config.clone());
        let valid: LeaseTime = cfg.lease_time.into();
        let preferred: LeaseTime = cfg.preferred_time.into();
        check_lifetimes(&format!("pd_pool {}", p.prefix), preferred, valid)?;
        Ok(Self {
            prefix: p.prefix,
            delegated_len: p.delegated_len,
            valid,
            preferred,
            opts: p.options.get(),
            except: p.except,
        })
    }
}

/// A preferred lifetime greater than the valid lifetime produces a wire-invalid
/// IAADDR/IAPREFIX that clients MUST discard (RFC 8415 §21.6). Reject it in config.
fn check_lifetimes(what: &str, preferred: LeaseTime, valid: LeaseTime) -> Result<()> {
    // check the default and the max: a requested time is clamped to [min, max],
    // so preferred.max > valid.max could still put preferred > valid on the wire.
    if preferred.get_default() > valid.get_default() || preferred.get_max() > valid.get_max() {
        bail!(
            "{what}: preferred_time must be <= lease_time/valid (default {:?} vs {:?}, max {:?} vs {:?})",
            preferred.get_default(),
            valid.get_default(),
            preferred.get_max(),
            valid.get_max()
        );
    }
    Ok(())
}

// TODO: replace with is_unicast_global from std when released
pub const fn is_unicast_global(ip: &Ipv6Addr) -> bool {
    !(ip.is_multicast()
        || ip.is_loopback()
        || is_unicast_link_local(ip) // is_unicast_link_local
        || ((ip.segments()[0] & 0xfe00) == 0xfc00) // is_unique_local
        || ip.is_unspecified()
        || ((ip.segments()[0] == 0x2001) && (ip.segments()[1] == 0xdb8))) // is_documentation
}

// TODO: replace with is_unicast_link_local from std when released
pub const fn is_unicast_link_local(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

pub fn generate_duid_from_config(server_id: &ServerDuidInfo, link_layer: Ipv6Addr) -> Result<Duid> {
    fn parse_id(id: &str, link_layer: Ipv6Addr) -> Result<Ipv6Addr> {
        Ok(if id.is_empty() {
            link_layer
        } else {
            Ipv6Addr::from_str(id).context("identifier must be a valid ipv6 address")?
        })
    }
    fn parse_htype(htype: u16) -> HType {
        if htype == 0 {
            HType::Eth
        } else {
            HType::from(htype)
        }
    }
    match server_id {
        ServerDuidInfo::LLT {
            htype,
            identifier,
            time,
        } => {
            let htype = parse_htype(*htype);
            let identifier = parse_id(identifier, link_layer)?;
            let time = if *time == 0 {
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .context("unable to get system time")?
                    .as_secs() as u32
            } else {
                *time
            };
            Ok(Duid::link_layer_time(htype, time, identifier))
        }
        ServerDuidInfo::LL { htype, identifier } => {
            let htype = parse_htype(*htype);
            let identifier = parse_id(identifier, link_layer)?;
            Ok(Duid::link_layer(htype, identifier))
        }
        ServerDuidInfo::EN {
            enterprise_id,
            identifier,
        } => {
            let enterprise_id = if *enterprise_id == 0 {
                1 //TODO: harewire to 1 temporarily
            } else {
                *enterprise_id
            };
            let identifier = if identifier.is_empty() {
                generate_random_bytes(6)
            } else {
                hex::decode(identifier).context("identifier should be a valid hex string")?
            };
            Ok(Duid::enterprise(enterprise_id, &identifier[..]))
        }
        ServerDuidInfo::UUID { identifier } => {
            if identifier.is_empty() {
                bail!("identifier must be specified for UUID type DUID");
            }
            let identifier =
                hex::decode(identifier).context("identifier should be a valid hex string")?;
            Ok(Duid::uuid(&identifier[..]))
        }
    }
}

fn generate_duid_and_persist(
    server_id_info: &ServerDuidInfo,
    link_layer_address: Ipv6Addr,
    server_id_path: &Path,
) -> Result<Duid> {
    let duid = generate_duid_from_config(server_id_info, link_layer_address)
        .context("can not generate duid from config")?;
    PersistIdentifier {
        identifier: hex::encode(duid.as_ref()),
        duid_config: server_id_info.clone(),
    }
    .to_json(server_id_path)
    .context("can not write server identifier json")?;
    Ok(duid)
}

impl TryFrom<wire::v6::Config> for Config {
    type Error = anyhow::Error;

    fn try_from(cfg: wire::v6::Config) -> Result<Self> {
        let interfaces = crate::v6_find_interfaces(cfg.interfaces.as_deref())?;
        // DUID-LLT is the default, will need config options to do others
        let link_local = interfaces
            .iter()
            .find_map(|int| {
                int.ips.iter().find_map(|ip| match ip {
                    IpNetwork::V6(ip) if is_unicast_link_local(&ip.ip()) => Some(*ip),
                    _ => None,
                })
            })
            .context("unable to find a link local ip")?;
        let server_id = match cfg.server_id {
            None => {
                // if server id file exists, then use it
                let server_id_path = Path::new(DEFAULT_SERVER_ID_FILE_PATH);
                if server_id_path.exists() {
                    let identifier_file = PersistIdentifier::from_json(server_id_path)
                        .context("can not read server identifier json")?;
                    identifier_file
                        .duid()
                        .context("can not get duid from server identifier file")?
                } else {
                    // https://www.rfc-editor.org/rfc/rfc8415#section-11.2
                    Duid::link_layer_time(
                        HType::Eth,
                        SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .context("unable to get system time")?
                            .as_secs() as u32,
                        link_local.ip(),
                    )
                }
            }
            Some(server_id) => {
                let server_id_path = if server_id.path.is_empty() {
                    Path::new(DEFAULT_SERVER_ID_FILE_PATH)
                } else {
                    Path::new(&server_id.path)
                };
                if !server_id.persist {
                    generate_duid_from_config(&server_id.info, link_local.ip())
                        .context("can not generate duid from config")?
                } else if !server_id_path.exists() {
                    generate_duid_and_persist(&server_id.info, link_local.ip(), server_id_path)?
                } else {
                    let identifier_file = PersistIdentifier::from_json(server_id_path)
                        .context("can not read server identifier json")?;
                    if identifier_file.duid_config == server_id.info {
                        // Here, server_id.info is read from a YAML file and the fields like time, identifier, enterprise_id, etc. have not been processed yet (i.e., 0 has not been replaced with the corresponding default values). Therefore, a comparison can be made. For example, if the server_id type is set to LLT and all other values are empty, then both the persisted file and server_id.info will have all fields as 0 or empty string, making them equal. The difference in time or local link layer address due to changes in time or adapter will not affect the comparison.
                        identifier_file
                            .duid()
                            .context("can not get duid from server identifier file")?
                    } else {
                        generate_duid_and_persist(&server_id.info, link_local.ip(), server_id_path)?
                    }
                }
            }
        };
        let global_opts = cfg.options;
        // global options resolved once, folded into every network/range/pd_pool.
        let global = global_opts
            .as_ref()
            .map(|o| o.as_ref().clone())
            .unwrap_or_default();
        let policies = &cfg.policies;
        debug!(?interfaces, ?server_id, "v6 interfaces that will be used");
        let networks = cfg
            .networks
            .into_iter()
            .map(|(subnet, net)| {
                let wire::v6::Net {
                    ping_check,
                    probation_period,
                    authoritative,
                    ping_timeout_ms,
                    config,
                    options,
                    policy,
                    ranges,
                    pd_pools,
                    interfaces: net_interfaces,
                } = net;

                // network-scoped option context, most- to least-specific:
                // network options -> network policy -> global. This is what v6
                // responses actually draw from (`network.opts()`); per-range and
                // per-pd_pool options are not applied on the v6 response path, so
                // there is nothing to fold into them.
                let net_policy = policy_opts(policies, policy.as_deref())?;
                let net_ctx = fold_opts(options.get(), &[&net_policy, &global]);

                // convert address pools (IA_NA) and prefix pools (IA_PD)
                let ranges: Vec<NetRange> = ranges
                    .into_iter()
                    .map(|r| NetRange::from_wire(r, &config))
                    .collect();
                for r in &ranges {
                    check_lifetimes(
                        &format!("range {}-{}", r.start(), r.end()),
                        r.preferred(),
                        r.valid(),
                    )?;
                }
                let pd_pools: Vec<PdPool> = pd_pools
                    .into_iter()
                    .map(|p| PdPool::from_wire(p, &config))
                    .collect::<Result<_>>()?;

                // If any interfaces are explicitly set for the network,
                // find them. If the interface can't be found return an error.
                let net_interfaces = net_interfaces
                    .map(|net_interfaces| {
                        let found_interfaces = pnet::datalink::interfaces()
                            .into_iter()
                            .filter(|e| {
                                e.is_up() && !e.ips.is_empty() && e.ips.iter().any(|i| i.is_ipv6())
                            })
                            .collect::<Vec<_>>();

                        net_interfaces
                            .into_iter()
                            .map(|int| {
                                if let Some(interface) =
                                    found_interfaces.iter().find(|i| i.name == int)
                                {
                                    Ok(interface.clone())
                                } else {
                                    bail!("unable to find interface {} for network", int)
                                }
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()?;

                let (valid, preferred): (LeaseTime, LeaseTime) =
                    (config.lease_time.into(), config.preferred_time.into());
                check_lifetimes(&format!("network {subnet}"), preferred, valid)?;

                let network = Network {
                    interfaces: net_interfaces,
                    subnet,
                    valid,
                    preferred,
                    ranges,
                    pd_pools,
                    ping_check,
                    probation_period: Duration::from_secs(probation_period),
                    authoritative,
                    ping_timeout_ms: Duration::from_millis(ping_timeout_ms),
                    // network options merged with policy + global (network wins)
                    options: net_ctx,
                };
                Ok((subnet, network))
            })
            .collect::<Result<_, anyhow::Error>>()?;

        let client_classes = if cfg.client_classes.is_empty() {
            None
        } else {
            Some(
                ClientClassesV6::try_from(cfg.client_classes)
                    .context("unable to parse v6 client_classes")?,
            )
        };

        Ok(Self {
            interfaces,
            networks,
            opts: global_opts.map(|o| o.get()),
            server_id,
            rapid_commit: cfg.rapid_commit,
            client_classes,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{PersistIdentifier, v4::Config};
    use std::path::Path;

    pub static TEST_SERVER_ID_FILE_PATH: &str = "./server_id"; //can not use include_str because sometimes it doesn't exist.
    /// Serializes the tests that read/write the shared `./server_id` file so they
    /// don't race each other under a threaded test runner (e.g. cargo llvm-cov).
    static SERVER_ID_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    pub static CONFIG_V6_YAML: &str = include_str!("../sample/config_v6.yaml");
    pub static CONFIG_V6_LL_YAML: &str = include_str!("../sample/config_v6_LL.yaml");
    pub static CONFIG_V6_EN_YAML: &str = include_str!("../sample/config_v6_EN.yaml");
    pub static CONFIG_V6_UUID_YAML: &str = include_str!("../sample/config_v6_UUID.yaml");
    pub static CONFIG_V6_NO_PERSIST_YAML: &str =
        include_str!("../sample/config_v6_no_persist.yaml");
    pub static CONFIG_V6_POOLS_YAML: &str = include_str!("../sample/config_v6_pools.yaml");
    pub static CONFIG_V6_POLICIES_YAML: &str = include_str!("../sample/config_v6_policies.yaml");

    /// v6 global options and named policies resolve with most-specific-wins
    /// precedence into the network options that responses draw from.
    #[test]
    fn test_v6_global_and_policy_opts() {
        use dora_core::dhcproto::v6::{DhcpOption as O, OptionCode as C};
        let cfg = Config::new(CONFIG_V6_POLICIES_YAML).unwrap();
        let v6 = cfg.v6().expect("expected v6 config");
        let (_subnet, net) = v6.get_first().expect("expected a network");

        // network references policy `corp`; opt 23 (DNS) comes from the policy,
        // shadowing the global 2001:db8::9
        assert_eq!(
            net.opts().get(C::DomainNameServers),
            Some(&O::DomainNameServers(vec![
                "2001:db8::53".parse().unwrap(),
                "2001:db8::54".parse().unwrap(),
            ]))
        );
    }

    /// when the same option code is set both globally and on a network, the
    /// network's own value wins (most-specific-wins). This guards the v6
    /// precedence change noted in the CHANGELOG (previously global won).
    #[test]
    fn test_v6_network_overrides_global_opts() {
        use dora_core::dhcproto::v6::{DhcpOption as O, OptionCode as C};
        let yaml = r#"
v6:
    server_id:
        type: LL
        identifier: fe80::1
        persist: false
        path: ./server_id_net_over_global
    options:
        values:
            23:
                type: ip_list
                value: [2001:db8::1]
    networks:
        2001:db8:1::/64:
            config:
                lease_time:
                    default: 3600
                preferred_time:
                    default: 3600
            options:
                values:
                    23:
                        type: ip_list
                        value: [2001:db8::2]
"#;
        let cfg = Config::new(yaml).unwrap();
        let (_subnet, net) = cfg.v6().unwrap().get_first().unwrap();
        // network's opt 23 wins over the global opt 23
        assert_eq!(
            net.opts().get(C::DomainNameServers),
            Some(&O::DomainNameServers(vec!["2001:db8::2".parse().unwrap()]))
        );
    }

    /// referencing a policy name that isn't defined is a config error
    #[test]
    fn test_v6_unknown_policy_errors() {
        let yaml = r#"
v6:
    server_id:
        type: LL
        identifier: fe80::1
        persist: false
        path: ./server_id_unknown_policy
    networks:
        2001:db8:1::/64:
            policy: nope
            config:
                lease_time:
                    default: 3600
                preferred_time:
                    default: 3600
"#;
        let err = Config::new(yaml).expect_err("unknown v6 policy must fail");
        assert!(
            format!("{err:#}").contains("unknown policy"),
            "unexpected error: {err:#}"
        );
    }

    /// parse a v6 config with IA_NA `ranges` and IA_PD `pd_pools` and verify
    /// they are decoded into the parsed `Network`.
    #[test]
    fn test_v6_pools_parse() {
        use std::net::Ipv6Addr;
        use std::time::Duration;

        let cfg = Config::new(CONFIG_V6_POOLS_YAML).unwrap();
        let v6 = cfg.v6().expect("expected v6 config");
        let (_subnet, net) = v6.get_first().expect("expected a network");

        // --- IA_NA ranges ---
        assert_eq!(net.ranges().len(), 1, "expected one address pool");
        let range = &net.ranges()[0];
        assert_eq!(
            range.start(),
            "2001:db8:1::100".parse::<Ipv6Addr>().unwrap()
        );
        assert_eq!(range.end(), "2001:db8:1::1ff".parse::<Ipv6Addr>().unwrap());
        assert_eq!(range.valid().get_default(), Duration::from_secs(3600));
        assert_eq!(range.preferred().get_default(), Duration::from_secs(3600));

        // exclusion is honored by contains() and iter()
        let excluded = "2001:db8:1::150".parse::<Ipv6Addr>().unwrap();
        let in_range = "2001:db8:1::101".parse::<Ipv6Addr>().unwrap();
        let out_of_range = "2001:db8:1::200".parse::<Ipv6Addr>().unwrap();
        assert!(range.contains(&in_range));
        assert!(
            !range.contains(&excluded),
            "excluded addr must not be contained"
        );
        assert!(!range.contains(&out_of_range));
        assert!(!range.iter().any(|ip| ip == excluded));
        assert_eq!(net.range(in_range).map(|r| r.start()), Some(range.start()));

        // --- IA_PD pd_pools ---
        assert_eq!(net.pd_pools().len(), 1, "expected one pd pool");
        let pd = &net.pd_pools()[0];
        assert_eq!(pd.prefix(), "2001:db8:100::/56".parse().unwrap());
        assert_eq!(pd.delegated_len(), 64);
        // /56 parent delegating /64s -> 2^(64-56) = 256 prefixes
        assert_eq!(pd.total_prefixes(), 256);
        assert_eq!(pd.valid().get_default(), Duration::from_secs(3600));
    }

    /// get_network_by_addr selects the network whose subnet contains the link addr
    #[test]
    fn test_v6_get_network_by_addr() {
        use std::net::Ipv6Addr;
        let cfg = Config::new(CONFIG_V6_POOLS_YAML).unwrap();
        let v6 = cfg.v6().expect("expected v6 config");
        // network is 2001:db8:1::/64
        let inside: Ipv6Addr = "2001:db8:1::1".parse().unwrap();
        let outside: Ipv6Addr = "2001:db8:99::1".parse().unwrap();
        assert!(v6.get_network_by_addr(inside).is_some());
        assert!(v6.get_network_by_addr(outside).is_none());
    }

    /// an invalid pd_pool (delegated_len <= parent prefix length) must error
    #[test]
    fn test_v6_pd_pool_invalid_delegated_len() {
        let yaml = r#"
v6:
    server_id:
        type: LL
        identifier: fe80::1
        persist: false
        path: ./server_id_bad_pd
    networks:
        2001:db8:1::/64:
            config:
                lease_time:
                    default: 3600
                preferred_time:
                    default: 3600
            pd_pools:
                - prefix: 2001:db8:100::/64
                  delegated_len: 56
                  config:
                      lease_time:
                          default: 3600
                      preferred_time:
                          default: 3600
"#;
        let err = Config::new(yaml).expect_err("delegated_len < prefix len must fail");
        assert!(
            format!("{err:#}").contains("delegated_len"),
            "unexpected error: {err:#}"
        );
    }

    /// a pd_pool delegating full /128 prefixes must be rejected (collides with IA_NA)
    #[test]
    fn test_v6_pd_pool_delegated_len_128_rejected() {
        let yaml = r#"
v6:
    server_id:
        type: LL
        identifier: fe80::1
        persist: false
        path: ./server_id_pd128
    networks:
        2001:db8:1::/64:
            config:
                lease_time:
                    default: 3600
                preferred_time:
                    default: 3600
            pd_pools:
                - prefix: 2001:db8:100::/64
                  delegated_len: 128
                  config:
                      lease_time:
                          default: 3600
                      preferred_time:
                          default: 3600
"#;
        let err = Config::new(yaml).expect_err("delegated_len 128 must fail");
        assert!(
            format!("{err:#}").contains("delegated_len"),
            "unexpected error: {err:#}"
        );
    }

    /// a preferred_time greater than the valid lifetime must be rejected
    #[test]
    fn test_v6_preferred_gt_valid_rejected() {
        let yaml = r#"
v6:
    server_id:
        type: LL
        identifier: fe80::1
        persist: false
        path: ./server_id_badlife
    networks:
        2001:db8:1::/64:
            config:
                lease_time:
                    default: 1800
                preferred_time:
                    default: 3600
"#;
        let err = Config::new(yaml).expect_err("preferred > valid must fail");
        assert!(
            format!("{err:#}").contains("preferred_time"),
            "unexpected error: {err:#}"
        );
    }

    /// test if v6_config can generate a server_id; and if it can dump it to a file
    #[test]
    fn test_v6_config() {
        let _serial = SERVER_ID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = Path::new(TEST_SERVER_ID_FILE_PATH);
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }

        let cfg = Config::new(CONFIG_V6_YAML).unwrap();
        // test a range decoded properly
        match cfg.v6() {
            Some(v6_config) => {
                println!("{:?}", v6_config);
            }
            None => {
                panic!("expected v6 config")
            }
        };

        let identifier_file = PersistIdentifier::from_json(path).unwrap();
        let file_server_id = identifier_file.duid().unwrap();
        let file_server_id = file_server_id.as_ref();
        let server_id = cfg.v6().unwrap().server_id();
        assert_eq!(server_id, file_server_id);
    }

    /// test if we can generate a different server_id using different config rather than using the config file that exists
    #[test]
    fn test_v6_generate_different_server_id() {
        let _serial = SERVER_ID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg1 = Config::new(CONFIG_V6_YAML).unwrap();
        let cfg2 = Config::new(CONFIG_V6_LL_YAML).unwrap();
        let server_id1 = cfg1.v6().unwrap().server_id();
        let server_id2 = cfg2.v6().unwrap().server_id();
        println!("server_id1: {:?}", server_id1);
        println!("server_id2: {:?}", server_id2);
        assert_ne!(server_id1, server_id2);
    }
    /// test if we can generate EN type server_id
    #[test]
    fn test_v6_generate_en_server_id() {
        let _serial = SERVER_ID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = Config::new(CONFIG_V6_EN_YAML).unwrap();
        let server_id = cfg.v6().unwrap().server_id();
        println!("server_id: {:?}", server_id);
    }
    /// test if we can generate UUID type server_id
    #[test]
    fn test_v6_generate_uuid_server_id() {
        let _serial = SERVER_ID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = Config::new(CONFIG_V6_UUID_YAML).unwrap();
        let server_id = cfg.v6().unwrap().server_id();
        println!("server_id: {:?}", server_id);
    }
    /// test if wen can generate server_id without persisting it to a file
    #[test]
    fn test_v6_generate_server_id_without_persist() {
        let _serial = SERVER_ID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let server_id_path = Path::new(TEST_SERVER_ID_FILE_PATH);
        if server_id_path.exists() {
            std::fs::remove_file(server_id_path).unwrap();
        }
        let cfg = Config::new(CONFIG_V6_NO_PERSIST_YAML).unwrap();
        let server_id = cfg.v6().unwrap().server_id();
        println!("server_id: {:?}", server_id);
        assert!(!server_id_path.exists());
    }
}
