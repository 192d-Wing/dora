//! # Client Classes

use serde::{Deserialize, Serialize};

use crate::wire::{v4::Options, v6::Options as OptionsV6};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClientClasses {
    pub(crate) v4: Vec<ClientClass>,
    /// DHCPv6 client classes. Only the protocol-agnostic subset of the assert
    /// expression language is supported (option access, `member`, substring,
    /// concat, hexstring, equality); v4-only header atoms are rejected.
    #[serde(default)]
    pub(crate) v6: Vec<ClientClassV6>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClientClass {
    pub(crate) name: String,
    pub(crate) assert: String,
    #[serde(default)]
    pub(crate) options: Options,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClientClassV6 {
    pub(crate) name: String,
    pub(crate) assert: String,
    #[serde(default)]
    pub(crate) options: OptionsV6,
}
