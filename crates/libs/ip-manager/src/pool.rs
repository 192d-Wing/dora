//! Family-neutral views of the config pool/network types so `IpManager`'s
//! allocation logic serves both DHCPv4 and DHCPv6 without duplicating the
//! reserve/next-available algorithm.
//!
//! The config crate owns concrete v4/v6 `NetRange`/`Network` types; these
//! traits (local to `ip-manager`) let the allocator operate on `IpAddr` and a
//! small set of network parameters regardless of address family.
use std::{collections::HashSet, net::IpAddr, time::Duration};

/// A range of addresses the allocator can hand out.
pub trait Pool {
    /// first address of the range
    fn start(&self) -> IpAddr;
    /// last address of the range (inclusive)
    fn end(&self) -> IpAddr;
    /// true if `ip` is assignable from this pool (in range and not excluded)
    fn contains(&self, ip: IpAddr) -> bool;
    /// addresses to skip, as a family-neutral set
    fn exclusions(&self) -> HashSet<IpAddr>;
}

/// The network/subnet parameters the allocator needs.
pub trait NetworkParams {
    /// subnet/network address bindings are recorded against
    fn subnet(&self) -> IpAddr;
    /// whether the server is authoritative for this network
    fn authoritative(&self) -> bool;
    /// whether to probe (DAD) an address before handing it out
    fn ping_check(&self) -> bool;
    /// how long to wait for a DAD probe reply
    fn ping_timeout(&self) -> Duration;
    /// how long a declined / in-use address is kept out of rotation
    fn probation_period(&self) -> Duration;
}

impl Pool for config::v4::NetRange {
    fn start(&self) -> IpAddr {
        IpAddr::V4(config::v4::NetRange::start(self))
    }
    fn end(&self) -> IpAddr {
        IpAddr::V4(config::v4::NetRange::end(self))
    }
    fn contains(&self, ip: IpAddr) -> bool {
        matches!(ip, IpAddr::V4(v4) if config::v4::NetRange::contains(self, &v4))
    }
    fn exclusions(&self) -> HashSet<IpAddr> {
        config::v4::NetRange::exclusions(self)
            .iter()
            .map(|ip| IpAddr::V4(*ip))
            .collect()
    }
}

impl Pool for config::v6::NetRange {
    fn start(&self) -> IpAddr {
        IpAddr::V6(config::v6::NetRange::start(self))
    }
    fn end(&self) -> IpAddr {
        IpAddr::V6(config::v6::NetRange::end(self))
    }
    fn contains(&self, ip: IpAddr) -> bool {
        matches!(ip, IpAddr::V6(v6) if config::v6::NetRange::contains(self, &v6))
    }
    fn exclusions(&self) -> HashSet<IpAddr> {
        config::v6::NetRange::exclusions(self)
            .iter()
            .map(|ip| IpAddr::V6(*ip))
            .collect()
    }
}

impl NetworkParams for config::v4::Network {
    fn subnet(&self) -> IpAddr {
        IpAddr::V4(config::v4::Network::subnet(self))
    }
    fn authoritative(&self) -> bool {
        config::v4::Network::authoritative(self)
    }
    fn ping_check(&self) -> bool {
        config::v4::Network::ping_check(self)
    }
    fn ping_timeout(&self) -> Duration {
        config::v4::Network::ping_timeout(self)
    }
    fn probation_period(&self) -> Duration {
        config::v4::Network::probation_period(self)
    }
}

impl NetworkParams for config::v6::Network {
    fn subnet(&self) -> IpAddr {
        IpAddr::V6(config::v6::Network::subnet(self))
    }
    fn authoritative(&self) -> bool {
        config::v6::Network::authoritative(self)
    }
    fn ping_check(&self) -> bool {
        config::v6::Network::ping_check(self)
    }
    fn ping_timeout(&self) -> Duration {
        config::v6::Network::ping_timeout(self)
    }
    fn probation_period(&self) -> Duration {
        config::v6::Network::probation_period(self)
    }
}
