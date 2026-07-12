//! # Client Classes

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use client_classification::{Args, ArgsV6, Expr, PacketDetails, Val, ast, eval_v6};
use dora_core::dhcproto::{
    self, Decodable, Decoder, Encodable,
    v4::{self, OptionCode, UnknownOption},
    v6,
};
use topo_sort::DependencyTree;
use tracing::{error, trace, warn};

use crate::wire;
pub use client_classification;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientClasses {
    /// list of classes, order is topologically sorted based on use of `member` dependencies in the expression
    pub(crate) classes: HashMap<String, ClientClass>,
    pub(crate) original_order: Vec<String>,
    pub(crate) topo_order: Vec<String>,
    /// DHCPv6 classes, indexed and ordered independently of the v4 ones
    pub(crate) v6_classes: HashMap<String, ClientClassV6>,
    pub(crate) v6_original_order: Vec<String>,
    pub(crate) v6_topo_order: Vec<String>,
}

impl ClientClasses {
    pub fn find(&self, name: &str) -> Option<&ClientClass> {
        self.classes.get(name)
    }
    pub fn find_v6(&self, name: &str) -> Option<&ClientClassV6> {
        self.v6_classes.get(name)
    }
    /// whether any v6 client classes are configured
    pub fn has_v6(&self) -> bool {
        !self.v6_classes.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientClass {
    pub(crate) name: String,
    // TODO: client classes assertion won't work with sub-options right now
    pub(crate) assert: Expr,
    pub(crate) options: v4::DhcpOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientClassV6 {
    pub(crate) name: String,
    pub(crate) assert: Expr,
    pub(crate) options: v6::DhcpOptions,
}

impl TryFrom<wire::client_classes::ClientClasses> for ClientClasses {
    type Error = anyhow::Error;

    fn try_from(cfg: wire::client_classes::ClientClasses) -> Result<Self, Self::Error> {
        // save original order for option precedence
        let original_order = cfg.v4.iter().map(|c| c.name.clone()).collect();
        let mut dep_tree = DependencyTree::new();
        let mut classes = HashMap::new();
        for class in cfg.v4.into_iter() {
            let assert = ast::parse(&class.assert)
                .with_context(|| format!("failed to parse client class {}", class.name))?;
            let deps = client_classification::get_class_dependencies(&assert);
            let name = class.name.clone();
            dep_tree.add(name.clone(), name, deps);
            classes.insert(
                class.name.clone(),
                ClientClass {
                    name: class.name,
                    assert,
                    options: class.options.get(),
                },
            );
        }

        // v6 classes, built the same way but stored & ordered separately
        let v6_original_order = cfg.v6.iter().map(|c| c.name.clone()).collect();
        let mut v6_dep_tree = DependencyTree::new();
        let mut v6_classes = HashMap::new();
        for class in cfg.v6.into_iter() {
            let assert = ast::parse(&class.assert)
                .with_context(|| format!("failed to parse v6 client class {}", class.name))?;
            let deps = client_classification::get_class_dependencies(&assert);
            let name = class.name.clone();
            v6_dep_tree.add(name.clone(), name, deps);
            v6_classes.insert(
                class.name.clone(),
                ClientClassV6 {
                    name: class.name,
                    assert,
                    options: class.options.get(),
                },
            );
        }

        Ok(Self {
            classes,
            original_order,
            topo_order: dep_tree.topological_sort()?,
            v6_classes,
            v6_original_order,
            v6_topo_order: v6_dep_tree.topological_sort()?,
        })
    }
}

impl ClientClasses {
    /// evaluate all client classes, returning a list of classes that match
    pub fn eval(
        &self,
        req: &dhcproto::v4::Message,
        pkt: PacketDetails,
        bootp_enabled: bool,
    ) -> Result<Vec<String>> {
        let (chaddr, opts) = to_unknown_opts(req)?;
        let vendor_builtin = client_classification::create_builtin_vendor(req);
        // if msg-type is not Discover/Offer/Request/Inform/etc then the msg is BOOTP
        let is_bootp = bootp_enabled
            && req.opts().msg_type().is_none()
            && req.opcode() == v4::Opcode::BootRequest;

        if let Err(err) = vendor_builtin {
            // log error but don't stop evaluation
            warn!(
                ?err,
                "error converting opt 60 (vendor class) to string for VENDOR_CLASS_"
            );
        }
        let mut args = Args {
            chaddr,
            member: {
                // all packets are member of "ALL"
                let mut set = HashSet::new();
                set.insert(client_classification::ALL_CLASS.to_owned());
                // add "VENDOR_CLASS_*" built-in
                if let Ok(Some(vendor)) = vendor_builtin {
                    set.insert(vendor);
                }
                // add "BOOTP"
                if is_bootp {
                    set.insert(client_classification::BOOTP_CLASS.to_owned());
                }
                set
            },
            msg: req,
            opts,
            pkt,
        };
        // eval all client classes in topological order
        for name in &self.topo_order {
            // this should never fail
            let class = self.classes.get(name).context("class not found")?;
            // eval class, passing args
            if class.eval(&args) {
                // add class name to dependencies set, for future evals
                // classes are always eval'd in topological order, so
                // future evals know what prior evals were
                args.member.insert(class.name.to_owned());
            }
        }

        Ok(args.member.into_iter().collect())
    }
    /// take matched client classes, return merge DhcpOptions that contains all classes options merged
    /// together with precedence given based on original position in client_classes list (lower index == higher precedence)
    pub fn collect_opts(&self, matched_classes: Option<&[String]>) -> Option<v4::DhcpOptions> {
        self.original_order
            .iter()
            .filter(|name| matched_classes.map(|m| m.contains(name)).unwrap_or(false))
            .fold(None, |ret, name| {
                let class = self.find(name)?;
                merge_opts(&class.options, ret)
            })
    }

    /// evaluate all v6 client classes, returning the names of those that match.
    /// Every message is a member of `ALL`.
    pub fn eval_v6(&self, req: &dhcproto::v6::Message) -> Result<Vec<String>> {
        let opts = to_v6_unknown_opts(req)?;
        let mut member = HashSet::new();
        member.insert(client_classification::ALL_CLASS.to_owned());
        let mut args = ArgsV6 {
            opts,
            msg: req,
            member,
        };
        // eval in topological order so `member(..)` dependencies are resolved
        for name in &self.v6_topo_order {
            let class = self.v6_classes.get(name).context("v6 class not found")?;
            if class.eval(&args) {
                args.member.insert(class.name.to_owned());
            }
        }
        Ok(args.member.into_iter().collect())
    }

    /// merge the options of all matched v6 classes, precedence following the
    /// original config order (earlier == higher priority).
    pub fn collect_opts_v6(&self, matched_classes: Option<&[String]>) -> Option<v6::DhcpOptions> {
        self.v6_original_order
            .iter()
            .filter(|name| matched_classes.map(|m| m.contains(name)).unwrap_or(false))
            .fold(None, |ret, name| {
                let class = self.find_v6(name)?;
                merge_opts_v6(&class.options, ret)
            })
    }
}

impl ClientClass {
    pub fn eval(&self, args: &Args) -> bool {
        trace!(name = ?self.name, expr = ?self.assert, chaddr = ?args.chaddr, "evaluating expression");
        match client_classification::eval(&self.assert, args) {
            Ok(Val::Bool(true)) => true,
            Ok(Val::Bool(false)) => false,
            res => {
                error!(name = ?self.name, ?res, "expression didn't evaluate to true/false");
                false
            }
        }
    }
}

impl ClientClassV6 {
    pub fn eval(&self, args: &ArgsV6) -> bool {
        trace!(name = ?self.name, expr = ?self.assert, "evaluating v6 expression");
        match eval_v6(&self.assert, args) {
            Ok(Val::Bool(true)) => true,
            Ok(Val::Bool(false)) => false,
            res => {
                error!(name = ?self.name, ?res, "v6 expression didn't evaluate to true/false");
                false
            }
        }
    }
}

/// build a map of v6 option code -> raw option data (the value section, without
/// the 4-byte code/len header) for use by the client-class evaluator.
fn to_v6_unknown_opts(req: &dhcproto::v6::Message) -> Result<HashMap<v6::OptionCode, Vec<u8>>> {
    req.opts()
        .iter()
        .map(|opt| {
            let code: v6::OptionCode = opt.into();
            // wire form is [code: u16][len: u16][data..]; slice off the header
            let buf = opt.to_vec().context("failed to encode v6 option")?;
            let data = buf.get(4..).unwrap_or(&[]).to_vec();
            Ok((code, data))
        })
        .collect::<Result<HashMap<_, _>>>()
        .context("failed to convert options in v6 client_classes")
}

fn to_unknown_opts(
    req: &dhcproto::v4::Message,
) -> Result<(&[u8], HashMap<OptionCode, UnknownOption>)> {
    // TODO: find a better way to do this so we don't have to convert to Unknown on every eval
    // possibly, add better methods to dhcproto so we can pull the data section out?
    Ok((
        req.chaddr(),
        req.opts()
            .iter()
            .map(|(k, v)| {
                Ok((*k, {
                    // using UnknownOption here so that the data section is easy to get
                    let opt = v.to_vec()?;
                    let mut d = Decoder::new(&opt);
                    UnknownOption::decode(&mut d)?
                }))
            })
            .collect::<Result<HashMap<_, _>>>()
            .context("failed to convert options in client_classes")?,
    ))
}

/// merge `b` into `a`, favoring `b` where there are duplicates
fn merge_opts(a: &v4::DhcpOptions, b: Option<v4::DhcpOptions>) -> Option<v4::DhcpOptions> {
    match b {
        Some(mut b) => {
            for (code, opt) in a.iter() {
                if b.get(*code).is_none() {
                    b.insert(opt.clone());
                }
            }
            Some(b)
        }
        None => Some(a.clone()),
    }
}

/// merge `b` into `a`, favoring `b` where there are duplicates (v6 variant)
fn merge_opts_v6(a: &v6::DhcpOptions, b: Option<v6::DhcpOptions>) -> Option<v6::DhcpOptions> {
    match b {
        Some(mut b) => {
            for opt in a.iter() {
                if b.get(opt.into()).is_none() {
                    b.insert(opt.clone());
                }
            }
            Some(b)
        }
        None => Some(a.clone()),
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn v6_client_class_eval_and_opts() {
        use dora_core::dhcproto::v6::{
            DhcpOption as O, Message, MessageType, OptionCode as C, VendorClass,
        };

        let wire: wire::client_classes::ClientClasses = serde_json::from_str(
            r#"{
                "v4": [],
                "v6": [
                    {"name":"has-vendor","assert":"option[16].exists",
                     "options":{"values":{"23":{"type":"ip_list","value":["2001:db8::abcd"]}}}},
                    {"name":"dep","assert":"member('has-vendor')",
                     "options":{"values":{"24":{"type":"hex","value":"00"}}}}
                ]
            }"#,
        )
        .unwrap();
        let classes = ClientClasses::try_from(wire).unwrap();

        // a message carrying a Vendor Class option (16) matches `has-vendor`,
        // and `dep` matches transitively via member()
        let mut msg = Message::new(MessageType::Solicit);
        msg.opts_mut().insert(O::VendorClass(VendorClass {
            num: 42,
            data: vec![b"docsis".to_vec()],
        }));
        let matched = classes.eval_v6(&msg).unwrap();
        assert!(matched.contains(&"has-vendor".to_owned()));
        assert!(matched.contains(&"dep".to_owned()));
        assert!(matched.contains(&"ALL".to_owned()));

        // collected options: opt 23 (from has-vendor) and opt 24 (from dep)
        let opts = classes.collect_opts_v6(Some(&matched)).unwrap();
        assert!(opts.get(C::DomainNameServers).is_some());
        assert!(opts.get(C::from(24u16)).is_some());

        // a message without a Vendor Class option matches neither
        let plain = Message::new(MessageType::Solicit);
        let matched = classes.eval_v6(&plain).unwrap();
        assert!(!matched.contains(&"has-vendor".to_owned()));
        assert!(!matched.contains(&"dep".to_owned()));
        assert!(matched.contains(&"ALL".to_owned()));
    }

    #[test]
    fn v6_client_class_rejects_v4_atom() {
        use dora_core::dhcproto::v6::{Message, MessageType};
        // a v4-only header atom is unsupported in v6: the expression fails to
        // evaluate to a bool, so the class simply doesn't match (logged error).
        let wire: wire::client_classes::ClientClasses = serde_json::from_str(
            r#"{
                "v4": [],
                "v6": [{"name":"bad","assert":"pkt4.mac == 0x001122334455",
                        "options":{"values":{}}}]
            }"#,
        )
        .unwrap();
        let classes = ClientClasses::try_from(wire).unwrap();
        let matched = classes
            .eval_v6(&Message::new(MessageType::Solicit))
            .unwrap();
        assert!(!matched.iter().any(|c| c == "bad"));
        assert!(matched.contains(&"ALL".to_owned()));
    }

    #[test]
    fn merge_opts() {
        let classes = ClientClasses {
            original_order: ["foo", "bar", "baz"]
                .iter()
                .map(|&n| n.to_owned())
                .collect(),
            topo_order: ["bar", "baz", "foo"]
                .iter()
                .map(|&n| n.to_owned())
                .collect(),
            classes: [
                (
                    "foo".to_owned(),
                    ClientClass {
                        name: "foo".to_owned(),
                        assert: client_classification::Expr::Bool(true),
                        options: {
                            let mut opts = v4::DhcpOptions::new();
                            opts.insert(v4::DhcpOption::Router(vec![[8, 8, 8, 8].into()]));
                            opts.insert(v4::DhcpOption::AddressLeaseTime(10));
                            opts
                        },
                    },
                ),
                (
                    "bar".to_owned(),
                    ClientClass {
                        name: "bar".to_owned(),
                        assert: client_classification::Expr::Bool(true),
                        options: {
                            let mut opts = v4::DhcpOptions::new();
                            opts.insert(v4::DhcpOption::Router(vec![[1, 1, 1, 1].into()]));
                            opts.insert(v4::DhcpOption::SubnetMask([1, 1, 1, 1].into()));
                            opts.insert(v4::DhcpOption::TimeOffset(50));
                            opts
                        },
                    },
                ),
                (
                    "baz".to_owned(),
                    ClientClass {
                        name: "baz".to_owned(),
                        assert: client_classification::Expr::Bool(true),
                        options: {
                            let mut opts = v4::DhcpOptions::new();
                            opts.insert(v4::DhcpOption::ServerIdentifier([1, 1, 1, 1].into()));
                            opts.insert(v4::DhcpOption::ArpCacheTimeout(1));
                            opts
                        },
                    },
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let opts = classes.collect_opts(Some(&["foo".to_owned(), "bar".to_owned()]));
        // includes opts from "foo" and "bar", favouring "foo" for duplicates because it shows up earlier in the `client_classes` list
        assert_eq!(opts.unwrap(), {
            let mut opts = v4::DhcpOptions::new();
            opts.insert(v4::DhcpOption::Router(vec![[8, 8, 8, 8].into()]));
            opts.insert(v4::DhcpOption::AddressLeaseTime(10));
            opts.insert(v4::DhcpOption::SubnetMask([1, 1, 1, 1].into()));
            opts.insert(v4::DhcpOption::TimeOffset(50));
            opts
        });
    }

    #[test]
    fn eval_bootp() {
        use std::collections::HashSet;
        let classes = ClientClasses {
            original_order: ["foo"].iter().map(|&n| n.to_owned()).collect(),
            topo_order: ["foo"].iter().map(|&n| n.to_owned()).collect(),
            classes: [(
                "foo".to_owned(),
                ClientClass {
                    name: "foo".to_owned(),
                    assert: client_classification::Expr::Bool(true),
                    options: {
                        let mut opts = v4::DhcpOptions::new();
                        opts.insert(v4::DhcpOption::Router(vec![[8, 8, 8, 8].into()]));
                        opts.insert(v4::DhcpOption::AddressLeaseTime(10));
                        opts
                    },
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let uns = Ipv4Addr::UNSPECIFIED;
        let bootp = v4::Message::new(uns, uns, uns, uns, &[1, 2, 3, 4, 5, 6]);
        // msg is a bootp message because it has empty opts
        let res = classes
            .eval(&bootp, PacketDetails::default(), true)
            .unwrap();
        assert_eq!(
            res.iter().collect::<HashSet<_>>(),
            ["foo".to_owned(), "BOOTP".to_owned(), "ALL".to_owned()]
                .iter()
                .collect::<HashSet<_>>()
        );

        let uns = Ipv4Addr::UNSPECIFIED;
        let mut msg = v4::Message::new(uns, uns, uns, uns, &[1, 2, 3, 4, 5, 6]);
        msg.opts_mut()
            .insert(v4::DhcpOption::MessageType(v4::MessageType::Discover));
        msg.opts_mut()
            .insert(v4::DhcpOption::ClassIdentifier(b"docsis3.0".to_vec()));
        // msg is a bootp message because it has empty opts
        let res = classes.eval(&msg, PacketDetails::default(), true).unwrap();
        assert_eq!(
            res.iter().collect::<HashSet<_>>(),
            [
                "foo".to_owned(),
                "VENDOR_CLASS_docsis3.0".to_owned(),
                "ALL".to_owned()
            ]
            .iter()
            .collect::<HashSet<_>>()
        );
    }
}
