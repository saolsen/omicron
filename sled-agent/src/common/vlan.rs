//! VLAN ID wrapper.

use omicron_common::api::external::Error;
use std::fmt;
use std::str::FromStr;

/// The maximum VLAN value (inclusive), as specified by IEEE 802.1Q.
pub const VLAN_MAX: u16 = 4094;

/// Wrapper around a VLAN ID, ensuring it is valid.
#[derive(Debug, Clone, Copy)]
pub struct VlanID(u16);

impl VlanID {
    /// Creates a new VLAN ID, returning an error if it is out of range.
    pub fn new(id: u16) -> Result<Self, Error> {
        if VLAN_MAX < id {
            return Err(Error::InvalidValue {
                label: id.to_string(),
                message: "Invalid VLAN value".to_string(),
            });
        }
        Ok(Self(id))
    }
}

impl fmt::Display for VlanID {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0.to_string())
    }
}

impl FromStr for VlanID {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s.parse().map_err(|e| Error::InvalidValue {
            label: s.to_string(),
            message: format!("{}", e),
        })?)
    }
}