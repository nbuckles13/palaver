//! What kind of network a node is on, and where others should be told to find it.
//!
//! A node with a public address of its own needs to ask nobody. Otherwise STUN servers
//! can say what its address looks like from outside ([`crate::stun`]), and whether its
//! router keeps that address for every destination. Whether to ask is the user's choice
//! ([`StunMode`]): asking tells the servers this node's address.

use crate::quic::{QuicEndpoint, Reflexive};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// Public STUN servers asked when the user has named none.
pub const DEFAULT_STUN_SERVERS: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];
/// How long to wait for STUN servers, by default.
pub const STUN_PATIENCE: Duration = Duration::from_secs(3);

/// How long to wait for a server's name to be looked up.
const RESOLVE_PATIENCE: Duration = Duration::from_secs(2);

/// When a node asks STUN servers for its address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StunMode {
    /// Never: the most private choice. Behind a router, others must find the node by
    /// what its neighbours see.
    Never,
    /// Only when the node has no public address of its own.
    #[default]
    WhenNeeded,
    /// Always, which also shows a firewall that rewrites nothing but still stands between.
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StunConfig {
    pub mode: StunMode,
    pub servers: Vec<SocketAddr>,
    pub patience: Duration,
}

/// What was found out about the node's network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// The node's own address is the one others see.
    Open(SocketAddr),
    /// A router stands between, and gives the node this address whoever it talks to:
    /// others can be told to send there, and a hole punch can work.
    BehindRouter(SocketAddr),
    /// A router gives the node a different address for each destination. No address can
    /// be handed out, and a hole punch will not work.
    PerDestination,
    /// Nothing was asked, or nobody answered.
    Unknown,
}

impl Network {
    /// The address to give others beyond the node's own, if there is one.
    pub fn public_address(&self) -> Option<SocketAddr> {
        match self {
            Self::Open(address) | Self::BehindRouter(address) => Some(*address),
            Self::PerDestination | Self::Unknown => None,
        }
    }
}

/// Whether `ip` can be reached from anywhere on the internet, as far as its number
/// tells: not private, not shared by a carrier, not this machine or this link.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            let carrier_shared = a == 100 && (64..128).contains(&b);
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || ip.is_documentation()
                || carrier_shared)
        }
        // Global unicast addresses are 2000::/3; all else is local in one way or another.
        IpAddr::V6(ip) => ip.segments()[0] & 0xe000 == 0x2000,
    }
}

/// Find out what network `endpoint` is on. `local` is its own address as it would tell
/// it to others.
pub async fn find(endpoint: &QuicEndpoint, local: SocketAddr, config: &StunConfig) -> Network {
    let ask = match config.mode {
        StunMode::Never => false,
        StunMode::WhenNeeded => !is_public(local.ip()),
        StunMode::Always => true,
    };
    if !ask {
        return if is_public(local.ip()) {
            Network::Open(local)
        } else {
            Network::Unknown
        };
    }
    match endpoint
        .public_addresses(&config.servers, config.patience)
        .await
    {
        Reflexive::Stable(seen) if seen == local => Network::Open(local),
        Reflexive::Stable(seen) => Network::BehindRouter(seen),
        Reflexive::PerDestination => Network::PerDestination,
        Reflexive::Unknown => Network::Unknown,
    }
}

/// The addresses of the servers named, one each, of the same family as `like` (a socket
/// of one family cannot send to the other). Names that do not resolve are left out.
pub async fn resolve(names: &[String], like: SocketAddr) -> Vec<SocketAddr> {
    let mut servers = Vec::new();
    for name in names {
        // A machine with no working resolver must not hold everything up.
        let looked = tokio::time::timeout(RESOLVE_PATIENCE, tokio::net::lookup_host(name.as_str()));
        if let Ok(Ok(mut found)) = looked.await
            && let Some(address) = found.find(|a| a.is_ipv4() == like.is_ipv4())
        {
            servers.push(address);
        }
    }
    servers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_addresses_count_as_public() {
        for public in ["8.8.8.8", "203.0.114.7", "100.128.0.1", "2001:4860::1"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
        for local in [
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.5",
            "100.64.0.1",
            "100.127.255.254",
            "127.0.0.1",
            "169.254.3.4",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "::",
        ] {
            assert!(!is_public(local.parse().unwrap()), "{local}");
        }
    }
}
