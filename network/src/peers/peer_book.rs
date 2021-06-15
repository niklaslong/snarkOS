// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{
    peers::{PeerInfo, PeerQuality},
    stats,
    NetworkError,
};
use snarkos_storage::{BlockHeight, Ledger};
use snarkvm_algorithms::traits::LoadableMerkleParameters;
use snarkvm_dpc::{Storage, TransactionScheme};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Instant,
};

#[derive(Deserialize, Serialize)]
pub struct SerializedPeerBook(Vec<PeerInfo>);

impl From<&PeerBook> for SerializedPeerBook {
    fn from(book: &PeerBook) -> Self {
        let mut peers = book.connected_peers();
        peers.extend(book.disconnected_peers().into_iter());
        let peers = peers
            .into_iter()
            .map(|(_, info)| info)
            .filter(|info| !info.address().ip().is_loopback())
            .collect();

        SerializedPeerBook(peers)
    }
}

impl From<SerializedPeerBook> for PeerBook {
    fn from(book: SerializedPeerBook) -> Self {
        PeerBook {
            disconnected_peers: RwLock::new(
                book.0
                    .into_iter()
                    .filter(|info| !info.address().ip().is_loopback())
                    .map(|info| (info.address(), info))
                    .collect(),
            ),
            ..Default::default()
        }
    }
}

///
/// A data structure for storing the history of all peers with this node server.
///
#[derive(Debug, Default)]
pub struct PeerBook {
    /// The map of the addresses currently being handshaken with.
    connecting_peers: RwLock<HashSet<SocketAddr>>,
    /// The map of connected peers to their metadata.
    connected_peers: RwLock<HashMap<SocketAddr, PeerInfo>>,
    /// The map of disconnected peers to their metadata.
    disconnected_peers: RwLock<HashMap<SocketAddr, PeerInfo>>,
}

impl PeerBook {
    // TODO (howardwu): Implement manual serializers and deserializers to prevent forward breakage
    //  when the PeerBook or PeerInfo struct fields change.
    ///
    /// Returns an instance of `PeerBook` from the given storage object.
    ///
    /// This function fetches a serialized peer book from the given storage object,
    /// and attempts to deserialize it as an instance of `PeerBook`.
    ///
    /// If the peer book does not exist in storage or fails to deserialize properly,
    /// returns a new instance of PeerBook.
    ///
    #[inline]
    pub fn load<T: TransactionScheme, P: LoadableMerkleParameters, S: Storage>(storage: &Ledger<T, P, S>) -> Self {
        // Fetch the peer book from storage.
        match storage.get_peer_book() {
            // Attempt to deserialize it as a peer book.
            Ok(Some(serialized_peer_book)) => match bincode::deserialize::<SerializedPeerBook>(&serialized_peer_book) {
                Ok(peer_book) => PeerBook::from(peer_book),
                _ => Default::default(),
            },
            _ => Default::default(),
        }
    }

    ///
    /// Returns `true` if a given address is a connecting peer in the `PeerBook`.
    ///
    #[inline]
    pub fn is_connecting(&self, address: SocketAddr) -> bool {
        self.connecting_peers.read().contains(&address)
    }

    ///
    /// Returns `true` if a given address is a connected peer in the `PeerBook`.
    ///
    #[inline]
    pub fn is_connected(&self, address: SocketAddr) -> bool {
        self.connected_peers.read().contains_key(&address)
    }

    ///
    /// Returns `true` if a given address is a disconnected peer in the `PeerBook`.
    ///
    #[inline]
    pub fn is_disconnected(&self, address: SocketAddr) -> bool {
        self.disconnected_peers.read().contains_key(&address)
    }

    ///
    /// Returns the number of connecting peers.
    ///
    #[inline]
    pub fn number_of_connecting_peers(&self) -> u16 {
        self.connecting_peers.read().len() as u16
    }

    ///
    /// Returns the number of connected peers.
    ///
    #[inline]
    pub fn number_of_connected_peers(&self) -> u16 {
        self.connected_peers.read().len() as u16
    }

    ///
    /// Returns the number of disconnected peers.
    ///
    #[inline]
    pub fn number_of_disconnected_peers(&self) -> u16 {
        self.disconnected_peers.read().len() as u16
    }

    ///
    /// Returns a reference to the connecting peers in this peer book.
    ///
    #[inline]
    pub fn connecting_peers(&self) -> HashSet<SocketAddr> {
        self.connecting_peers.read().clone()
    }

    ///
    /// Returns a reference to the connected peers in this peer book.
    ///
    #[inline]
    pub fn connected_peers(&self) -> HashMap<SocketAddr, PeerInfo> {
        (*self.connected_peers.read()).clone()
    }

    ///
    /// Returns a reference to the disconnected peers in this peer book.
    ///
    #[inline]
    pub fn disconnected_peers(&self) -> HashMap<SocketAddr, PeerInfo> {
        self.disconnected_peers.read().clone()
    }

    ///
    /// Marks the given address as "connecting".
    ///
    pub fn set_connecting(&self, address: SocketAddr) -> Result<(), NetworkError> {
        if self.is_connected(address) {
            return Err(NetworkError::PeerAlreadyConnected);
        }

        if self.connecting_peers.write().insert(address) {
            metrics::increment_gauge!(stats::CONNECTIONS_CONNECTING, 1.0);
        }

        Ok(())
    }

    ///
    /// Adds the given address to the connected peers in the `PeerBook`.
    ///
    pub fn set_connected(&self, address: SocketAddr, listener: Option<SocketAddr>) {
        // If listener.is_some(), then it's different than the address; otherwise it's just the address param.
        let listener = if let Some(addr) = listener { addr } else { address };

        // Remove the peer info from the connecting peers, if it exists.
        let mut peer_info = match self.disconnected_peers.write().remove(&listener) {
            // Case 1 - A previously known peer.
            Some(peer_info) => {
                metrics::decrement_gauge!(stats::CONNECTIONS_DISCONNECTED, 1.0);
                peer_info
            }
            // Case 2 - A peer that was previously not known.
            None => PeerInfo::new(listener),
        };

        // Remove the peer's address from the list of connecting peers.
        if self.connecting_peers.write().remove(&address) {
            metrics::decrement_gauge!(stats::CONNECTIONS_CONNECTING, 1.0);
        }

        // Update the peer info to connected.
        peer_info.set_connected();

        // Add the address into the connected peers.
        if self.connected_peers.write().insert(listener, peer_info).is_none() {
            metrics::increment_gauge!(stats::CONNECTIONS_CONNECTED, 1.0);
        } else {
            error!("{} is already a connected peer!", listener);
        }
    }

    ///
    /// Removes the given address from the connecting and connected peers in this `PeerBook`,
    /// and adds the given address to the disconnected peers in this `PeerBook`.
    ///
    pub fn set_disconnected(&self, address: SocketAddr) -> bool {
        // Case 1 - The given address is a connecting peer, attempt to disconnect.
        if self.connecting_peers.write().remove(&address) {
            metrics::decrement_gauge!(stats::CONNECTIONS_CONNECTING, 1.0);
            return false;
        }

        // Case 2 - The given address is a connected peer, attempt to disconnect.
        if let Some(mut peer_info) = self.connected_peers.write().remove(&address) {
            // Update the peer info to disconnected.
            peer_info.set_disconnected();

            metrics::decrement_gauge!(stats::CONNECTIONS_CONNECTED, 1.0);

            // Add the address into the disconnected peers.
            if self.disconnected_peers.write().insert(address, peer_info).is_none() {
                metrics::increment_gauge!(stats::CONNECTIONS_DISCONNECTED, 1.0);
            } else {
                error!("Detected a double disconnect from {}!", address);
            }

            return true;
        }

        false
    }

    pub fn set_unroutable(&self, address: SocketAddr) {
        // An unroutable peer must be disconnected.
        if let Some(peer_info) = self.disconnected_peers.write().get_mut(&address) {
            peer_info.set_is_routable(false);
        }
    }

    ///
    /// Adds the given address to the disconnected peers in this `PeerBook`.
    ///
    pub fn add_peer(&self, address: SocketAddr) {
        if self.is_connected(address) || self.is_disconnected(address) || self.is_connecting(address) {
            return;
        }

        // Add the given address to the map of disconnected peers.
        self.disconnected_peers.write().insert(address, PeerInfo::new(address));

        metrics::increment_gauge!(stats::CONNECTIONS_DISCONNECTED, 1.0);

        debug!("Added {} to the peer book", address);
    }

    ///
    /// Returns the peer info of the given address, if it exists.
    ///
    pub fn get_peer(&self, address: SocketAddr, only_if_connected: bool) -> Option<PeerInfo> {
        // Check if the address is a connected peer.
        if self.is_connected(address) {
            // Fetch the peer info of the connected peer.
            return self.connected_peers().get(&address).cloned();
        }

        // Check if the address is a known disconnected peer.
        if !only_if_connected && self.is_disconnected(address) {
            // Fetch the peer info of the disconnected peer.
            return self.disconnected_peers().get(&address).cloned();
        }

        None
    }

    ///
    /// Removes the given address from this `PeerBook`.
    ///
    /// This function should only be used in the case that the peer
    /// should be forgotten about permanently.
    ///
    pub fn remove_peer(&self, address: SocketAddr) {
        // Disconnect from the peer if currently connected.
        let _ = self.set_disconnected(address);

        // Remove the peer from the list of known peers.
        self.disconnected_peers.write().remove(&address);
    }

    fn peer_quality(&self, addr: SocketAddr) -> Option<Arc<PeerQuality>> {
        self.connected_peers().get(&addr).map(|peer| Arc::clone(&peer.quality))
    }

    ///
    /// Returns the `SocketAddr` of the last seen peer to be used as a sync node, or `None`.
    ///
    pub fn last_seen(&self) -> Option<SocketAddr> {
        if let Some((&socket_address, _)) = self
            .connected_peers()
            .iter()
            .max_by(|a, b| a.1.last_seen().cmp(&b.1.last_seen()))
        {
            Some(socket_address)
        } else {
            None
        }
    }

    ///
    /// Updates the last seen timestamp of this peer to the current time.
    ///
    #[inline]
    pub fn register_message(&self, addr: SocketAddr) {
        if let Some(quality) = self.peer_quality(addr) {
            *quality.last_seen.write() = Some(chrono::Utc::now());
            quality.num_messages_received.fetch_add(1, Ordering::Relaxed);
        } else {
            trace!("Tried updating state of a peer that's not connected: {}", addr);
        }
    }

    pub fn sending_ping(&self, target: SocketAddr) {
        if let Some(quality) = self.peer_quality(target) {
            let timestamp = Instant::now();
            *quality.last_ping_sent.lock() = Some(timestamp);
            quality.expecting_pong.store(true, Ordering::SeqCst);
        } else {
            // shouldn't occur, but just in case
            warn!("Tried to send a Ping to an unknown peer: {}!", target);
        }
    }

    /// Handles an incoming `Ping` message.
    pub fn received_ping(&self, source: SocketAddr, block_height: BlockHeight) {
        if let Some(ref quality) = self.peer_quality(source) {
            quality.block_height.store(block_height, Ordering::SeqCst);
        } else {
            warn!("Tried updating block height of a peer that's not connected: {}", source);
        }
    }

    /// Handles an incoming `Pong` message.
    pub fn received_pong(&self, source: SocketAddr) {
        if let Some(quality) = self.peer_quality(source) {
            if quality.expecting_pong.load(Ordering::SeqCst) {
                let ping_sent = quality.last_ping_sent.lock().unwrap();
                let rtt = ping_sent.elapsed().as_millis() as u64;
                trace!("RTT for {} is {}ms", source, rtt);
                quality.rtt_ms.store(rtt, Ordering::SeqCst);
                quality.expecting_pong.store(false, Ordering::SeqCst);
            } else {
                quality.failures.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            // shouldn't occur, but just in case
            warn!("Received a Pong from an unknown peer: {}!", source);
        }
    }

    /// Registers that the given number of blocks is expected as part of syncing with a peer.
    pub fn expecting_sync_blocks(&self, addr: SocketAddr, count: usize) -> bool {
        if let Some(ref pq) = self.peer_quality(addr) {
            pq.remaining_sync_blocks.store(count as u32, Ordering::SeqCst);
            true
        } else {
            trace!("Peer for expecting_sync_blocks purposes not found! (probably disconnected)");
            false
        }
    }

    /// Registers the receipt of a sync block from a peer; returns `true` when finished syncing.
    pub fn got_sync_block(&self, addr: SocketAddr) -> bool {
        if let Some(ref pq) = self.peer_quality(addr) {
            pq.remaining_sync_blocks.fetch_sub(1, Ordering::SeqCst) == 1
        } else {
            trace!("Peer for got_sync_block purposes not found! (probably disconnected)");
            // We might still be processing queued sync blocks; the sync expiry mechanism
            // will handle going into the `Idle` state if the batch is incomplete.
            false
        }
    }

    /// Cancels any expected sync block counts from all peers.
    pub fn cancel_any_unfinished_syncing(&self) {
        for peer_info in self.connected_peers().values_mut() {
            let missing_sync_blocks = peer_info.quality.remaining_sync_blocks.swap(0, Ordering::SeqCst);
            if missing_sync_blocks != 0 {
                warn!(
                    "Was expecting {} more sync blocks from {}",
                    missing_sync_blocks,
                    peer_info.address(),
                );

                peer_info.quality.failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Registers a non-critical failure related to a peer.
    pub fn register_failure(&self, addr: SocketAddr) {
        if let Some(pq) = self.peer_quality(addr) {
            pq.failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn test_set_connecting_from_never_connected() {
        let peer_book = PeerBook::default();
        let remote_address = SocketAddr::from((IpAddr::V4(Ipv4Addr::LOCALHOST), 4031));

        peer_book.add_peer(remote_address);
        assert_eq!(false, peer_book.is_connecting(remote_address));
        assert_eq!(false, peer_book.is_connected(remote_address));
        assert_eq!(true, peer_book.is_disconnected(remote_address));

        peer_book.set_connecting(remote_address).unwrap();
        assert_eq!(true, peer_book.is_connecting(remote_address));
        assert_eq!(false, peer_book.is_connected(remote_address));
        assert_eq!(true, peer_book.is_disconnected(remote_address));
    }

    #[test]
    fn test_set_connected_from_connecting() {
        let peer_book = PeerBook::default();
        let remote_address = SocketAddr::from((IpAddr::V4(Ipv4Addr::LOCALHOST), 4031));

        peer_book.set_connecting(remote_address).unwrap();
        assert_eq!(true, peer_book.is_connecting(remote_address));
        assert_eq!(false, peer_book.is_connected(remote_address));
        assert_eq!(false, peer_book.is_disconnected(remote_address));

        peer_book.set_connected(remote_address, None);
        assert_eq!(false, peer_book.is_connecting(remote_address));
        assert_eq!(true, peer_book.is_connected(remote_address));
        assert_eq!(false, peer_book.is_disconnected(remote_address));
    }

    #[test]
    fn test_set_disconnected_from_connecting() {
        let peer_book = PeerBook::default();
        let remote_address = SocketAddr::from((IpAddr::V4(Ipv4Addr::LOCALHOST), 4031));

        peer_book.add_peer(remote_address);

        peer_book.set_connecting(remote_address).unwrap();
        assert_eq!(true, peer_book.is_connecting(remote_address));
        assert_eq!(false, peer_book.is_connected(remote_address));
        assert_eq!(true, peer_book.is_disconnected(remote_address));

        peer_book.set_disconnected(remote_address);
        assert_eq!(false, peer_book.is_connecting(remote_address));
        assert_eq!(false, peer_book.is_connected(remote_address));
        assert_eq!(true, peer_book.is_disconnected(remote_address));
    }

    #[test]
    fn test_set_disconnected_from_connected() {
        let peer_book = PeerBook::default();
        let remote_address = SocketAddr::from((IpAddr::V4(Ipv4Addr::LOCALHOST), 4031));

        peer_book.set_connecting(remote_address).unwrap();
        assert_eq!(true, peer_book.is_connecting(remote_address));
        assert_eq!(false, peer_book.is_connected(remote_address));
        assert_eq!(false, peer_book.is_disconnected(remote_address));

        peer_book.set_connected(remote_address, None);
        assert_eq!(false, peer_book.is_connecting(remote_address));
        assert_eq!(true, peer_book.is_connected(remote_address));
        assert_eq!(false, peer_book.is_disconnected(remote_address));

        peer_book.set_disconnected(remote_address);
        assert_eq!(false, peer_book.is_connecting(remote_address));
        assert_eq!(false, peer_book.is_connected(remote_address));
        assert_eq!(true, peer_book.is_disconnected(remote_address));
    }

    #[test]
    fn test_set_connected_from_disconnected() {
        let peer_book = PeerBook::default();
        let remote_address = SocketAddr::from((IpAddr::V4(Ipv4Addr::LOCALHOST), 4031));

        peer_book.set_connecting(remote_address).unwrap();
        peer_book.set_connected(remote_address, None);
        peer_book.set_disconnected(remote_address);
        assert_eq!(false, peer_book.is_connecting(remote_address));
        assert_eq!(false, peer_book.is_connected(remote_address));
        assert_eq!(true, peer_book.is_disconnected(remote_address));

        peer_book.set_connected(remote_address, None);

        assert_eq!(false, peer_book.is_connecting(remote_address));
        assert_eq!(true, peer_book.is_connected(remote_address));
    }
}
