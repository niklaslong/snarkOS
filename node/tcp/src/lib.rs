// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![deny(missing_docs)]
#![deny(unsafe_code)]

//! **Tcp** is a simple, low-level, and customizable implementation of a TCP stack.

mod helpers;
pub use helpers::*;

pub mod protocols;

mod tcp;
pub use tcp::Tcp;

use std::net::IpAddr;

/// A trait for objects containing a [`Tcp`]; it is required to implement protocols.
pub trait P2P {
    /// Returns a reference to the TCP instance.
    fn tcp(&self) -> &Tcp;
}

/// Checks if the given IP address is a bogon address.
///
/// A bogon address is an IP address that should not appear on the public Internet.
/// This includes private addresses, loopback addresses, and link-local addresses.
pub fn is_bogon_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => ipv4.is_loopback() || ipv4.is_private() || ipv4.is_link_local(),
        IpAddr::V6(ipv6) => ipv6.is_loopback(),
    }
}

/////////////////////////////////////////////

use anyhow::{bail, Result};
use std::net::SocketAddr;
use tokio::task::JoinHandle;
use tracing::*;

// TODO(nkls): move somewhere else?
/// Common and overloadable network behaviour.
pub trait TcpExt {
    /// Attempts to connect to the given peer IP.
    fn connect(&self, peer_ip: SocketAddr) -> Option<JoinHandle<()>>
    where
        Self: P2P + Clone + Sync + Send + 'static,
    {
        // Return early if the attempt is against the protocol rules.
        if let Err(forbidden_error) = self.check_connection_attempt(peer_ip) {
            warn!("{forbidden_error}");
            return None;
        }

        let self_clone = self.clone();
        Some(tokio::spawn(async move {
            // Attempt to connect to the candidate peer.
            match self_clone.tcp().connect(peer_ip).await {
                // // Remove the peer from the candidate peers.
                Ok(()) => {
                    self_clone.remove_candidate_peer(peer_ip);
                }
                // If the connection was not allowed, log the error.
                Err(error) => {
                    self_clone.remove_connecting_peer(peer_ip);
                    warn!("Unable to connect to '{peer_ip}' - {error}")
                }
            }
        }))
    }

    /// Disconnects from the given peer IP, if the peer is connected.
    fn disconnect(&self, peer_ip: SocketAddr) -> JoinHandle<()>
    where
        Self: P2P + Clone + Sync + Send + 'static,
    {
        let router = self.clone();
        tokio::spawn(async move {
            if let Some(peer_addr) = router.resolve_to_ambiguous(&peer_ip) {
                // Disconnect from this peer.
                let _disconnected = router.tcp().disconnect(peer_addr).await;
                debug_assert!(_disconnected);
            }
        })
    }

    /// Ensure we are allowed to connect to the given peer.
    fn check_connection_attempt(&self, peer_ip: SocketAddr) -> Result<()> {
        // Ensure the peer IP is not this node.
        if self.is_local_ip(peer_ip) {
            bail!("Dropping connection attempt to '{peer_ip}' (attempted to self-connect)")
        }
        // Ensure the node does not surpass the maximum number of peer connections.
        if self.number_of_connected_peers() >= self.max_connected_peers() {
            bail!("Dropping connection attempt to '{peer_ip}' (maximum peers reached)")
        }
        // Ensure the node is not already connected to this peer.
        if self.is_connected(peer_ip) {
            bail!("Dropping connection attempt to '{peer_ip}' (already connected)")
        }
        // Ensure the peer is not restricted.
        if self.is_restricted(peer_ip) {
            bail!("Dropping connection attempt to '{peer_ip}' (restricted)")
        }
        // Ensure the node is not already connecting to this peer.
        if !self.insert_connecting_peer(peer_ip) {
            bail!("Dropping connection attempt to '{peer_ip}' (already shaking hands as the initiator)")
        }
        Ok(())
    }

    /// Returns the local IP address.
    fn local_ip(&self) -> SocketAddr;

    /// Returns `true` if the given IP address is a local IP address.
    fn is_local_ip(&self, ip: SocketAddr) -> bool {
        ip == self.local_ip()
            || (ip.ip().is_unspecified() || ip.ip().is_loopback()) && ip.port() == self.local_ip().port()
    }

    /// Returns the (ambiguous) peer address from the listener IP address.
    fn resolve_to_ambiguous(&self, peer_ip: &SocketAddr) -> Option<SocketAddr>;

    /// Returns the number of connected peers.
    fn number_of_connected_peers(&self) -> usize;

    /// Returns the maximum number of connected peers.
    fn max_connected_peers(&self) -> usize;

    /// Returns `true` if the given IP address is a connected peer.
    fn is_connected(&self, peer_ip: SocketAddr) -> bool;

    /// Returns `true` if the given IP address is a restricted peer.
    fn is_restricted(&self, peer_ip: SocketAddr) -> bool;

    /// Removes the peer from the candidate peers and returns `true` if it was present.
    fn remove_candidate_peer(&self, peer_ip: SocketAddr) -> bool;

    /// Inserts the peer into the connecting peers and returns `true` if it was not present.
    fn insert_connecting_peer(&self, peer_ip: SocketAddr) -> bool;

    /// Removes the peer from the connecting peers and returns `true` if it was present.
    fn remove_connecting_peer(&self, peer_ip: SocketAddr) -> bool;
}
