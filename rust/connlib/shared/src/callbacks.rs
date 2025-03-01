use ip_network::{IpNetwork, Ipv4Network, Ipv6Network};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt::Debug;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::messages::client::Site;
use crate::messages::ResourceId;

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Status {
    Unknown,
    Online,
    Offline,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResourceDescription {
    Dns(ResourceDescriptionDns),
    Cidr(ResourceDescriptionCidr),
}

impl ResourceDescription {
    pub fn address_description(&self) -> Option<&str> {
        match self {
            ResourceDescription::Dns(r) => r.address_description.as_deref(),
            ResourceDescription::Cidr(r) => r.address_description.as_deref(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            ResourceDescription::Dns(r) => &r.name,
            ResourceDescription::Cidr(r) => &r.name,
        }
    }

    pub fn status(&self) -> Status {
        match self {
            ResourceDescription::Dns(r) => r.status,
            ResourceDescription::Cidr(r) => r.status,
        }
    }

    pub fn id(&self) -> ResourceId {
        match self {
            ResourceDescription::Dns(r) => r.id,
            ResourceDescription::Cidr(r) => r.id,
        }
    }

    /// What the GUI clients should paste to the clipboard, e.g. `https://github.com/firezone`
    pub fn pastable(&self) -> Cow<'_, str> {
        match self {
            ResourceDescription::Dns(r) => Cow::from(&r.address),
            ResourceDescription::Cidr(r) => Cow::from(r.address.to_string()),
        }
    }

    pub fn sites(&self) -> &[Site] {
        match self {
            ResourceDescription::Dns(r) => &r.sites,
            ResourceDescription::Cidr(r) => &r.sites,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
pub struct ResourceDescriptionDns {
    /// Resource's id.
    pub id: ResourceId,
    /// Internal resource's domain name.
    pub address: String,
    /// Name of the resource.
    ///
    /// Used only for display.
    pub name: String,

    pub address_description: Option<String>,
    pub sites: Vec<Site>,

    pub status: Status,
}

/// Description of a resource that maps to a CIDR.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResourceDescriptionCidr {
    /// Resource's id.
    pub id: ResourceId,
    /// CIDR that this resource points to.
    pub address: IpNetwork,
    /// Name of the resource.
    ///
    /// Used only for display.
    pub name: String,

    pub address_description: Option<String>,
    pub sites: Vec<Site>,

    pub status: Status,
}

/// Traits that will be used by connlib to callback the client upper layers.
pub trait Callbacks: Clone + Send + Sync {
    /// Called when the tunnel address is set.
    fn on_set_interface_config(&self, _: Ipv4Addr, _: Ipv6Addr, _: Vec<IpAddr>) {}

    /// Called when the route list changes.
    fn on_update_routes(&self, _: Vec<Ipv4Network>, _: Vec<Ipv6Network>) {}

    /// Called when the resource list changes.
    fn on_update_resources(&self, _: Vec<ResourceDescription>) {}

    /// Called when the tunnel is disconnected.
    ///
    /// If the tunnel disconnected due to a fatal error, `error` is the error
    /// that caused the disconnect.
    fn on_disconnect(&self, error: &crate::Error) {
        tracing::error!(error = ?error, "tunnel_disconnected");
        // Note that we can't panic here, since we already hooked the panic to this function.
        std::process::exit(0);
    }
}
