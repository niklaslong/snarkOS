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

use crate::{message::*, stats, ConnReader, ConnWriter, NetworkError, Node, SerializedPeerBook, Version};
use snarkvm_dpc::Storage;

use std::{
    cmp,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use parking_lot::Mutex;
use rand::seq::IteratorRandom;
use snow::HandshakeState;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    sync::mpsc::channel,
    task,
};

impl<S: Storage> Node<S> {
    /// Obtain a list of addresses of connected peers for this node.
    pub(crate) fn connected_peers(&self) -> Vec<SocketAddr> {
        self.peer_book.connected_peers().keys().copied().collect()
    }
}

impl<S: Storage + Send + Sync + 'static> Node<S> {
    ///
    /// Broadcasts updates with connected peers and maintains a permitted number of connected peers.
    ///
    pub(crate) fn update_peers(&self) {
        let min_peers = self.config.minimum_number_of_connected_peers() as usize;
        let max_peers = self.config.maximum_number_of_connected_peers() as usize;

        // Fetch the bootnodes.
        let bootnodes = self.config.bootnodes();

        // Disconnect from peers whose RTT is too high or have too many failures.
        let now = chrono::Utc::now();
        for (addr, peer_quality) in self
            .peer_book
            .connected_peers()
            .iter()
            .filter(|(addr, _)| !bootnodes.contains(addr)) // Skip this check if the peer is a bootnode.
            .map(|(addr, info)| (*addr, &info.quality))
        {
            if peer_quality.rtt_ms.load(Ordering::Relaxed) > 1500
                || peer_quality.failures.load(Ordering::Relaxed) >= 3
                || peer_quality.is_inactive(now)
            {
                warn!("Peer {} has a low quality score; disconnecting.", addr);
                self.disconnect_from_peer(addr);
            }
        }

        // Fetch the number of connected and connecting peers after the RTT disconnects.
        let number_of_connected_peers = self.peer_book.number_of_connected_peers() as usize;
        let number_of_connecting_peers = self.peer_book.number_of_connecting_peers() as usize;
        let number_of_peers = number_of_connected_peers + number_of_connecting_peers;

        trace!(
            "Connected to {} peer{}, connecting to {}",
            number_of_connected_peers,
            if number_of_connected_peers == 1 { "" } else { "s" },
            number_of_connecting_peers
        );

        // Calculate the peer counts to disconnect and connect based on the node type and current
        // peer counts.
        let (number_to_disconnect, number_to_connect) = match self.config.is_bootnode() {
            true => (
                // Bootnodes disconnect down to the min peer count, this to free up room for
                // the next crawled peers...
                number_of_connected_peers.saturating_sub(min_peers),
                // ...then they connect to peers up to min peers below the max, so as to keep room for
                // new incoming connections.
                max_peers.saturating_sub(2 * min_peers),
            ),
            false => (
                // Non-bootnodes disconnect if above the max peer count...
                number_of_connected_peers.saturating_sub(max_peers),
                // ...and connect if below the min peer count.
                min_peers.saturating_sub(number_of_peers),
            ),
        };

        if number_to_disconnect != 0 {
            trace!(
                "Disconnecting from {} peers to maintain their permitted number",
                number_to_disconnect
            );

            let mut connected = self
                .peer_book
                .connected_peers()
                .iter()
                .map(|(addr, info)| (*addr, info.last_connected()))
                .collect::<Vec<_>>();

            // Bootnodes will disconnect from random peers...
            if !self.config.is_bootnode() {
                // ...while regular peers from the most recently connected.
                connected.sort_unstable_by_key(|(_, last_connected)| *last_connected);
            }

            for _ in 0..number_to_disconnect {
                if let Some((addr, _)) = connected.pop() {
                    self.disconnect_from_peer(addr);
                }
            }
        }

        // Attempt to connect to the default bootnodes of the network if the node has no active
        // connections.
        if number_of_connected_peers == 0 {
            self.connect_to_bootnodes();
        }

        if number_to_connect != 0 {
            self.connect_to_disconnected_peers(number_to_connect);
        }

        // Read the peer counts again, since they may have changed.
        if self.peer_book.number_of_connected_peers() != 0 {
            // Broadcast a `GetPeers` message to request for more peers.
            self.broadcast_getpeers_requests();

            // Send a `Ping` to every connected peer.
            self.broadcast_pings();
        }
    }

    pub async fn initiate_connection(&self, remote_address: SocketAddr) -> Result<(), NetworkError> {
        // Local address must be known by now.
        let own_address = self.local_address().unwrap();

        // Don't connect if maximum number of connections has been reached.
        if !self.can_connect() {
            return Err(NetworkError::TooManyConnections);
        }

        if remote_address == own_address
            || ((remote_address.ip().is_unspecified() || remote_address.ip().is_loopback())
                && remote_address.port() == own_address.port())
        {
            return Err(NetworkError::SelfConnectAttempt);
        }
        if self.peer_book.is_connecting(remote_address) {
            return Err(NetworkError::PeerAlreadyConnecting);
        }
        if self.peer_book.is_connected(remote_address) {
            return Err(NetworkError::PeerAlreadyConnected);
        }

        metrics::increment_counter!(stats::CONNECTIONS_ALL_INITIATED);

        self.peer_book.set_connecting(remote_address)?;

        debug!("Connecting to {}...", remote_address);

        // Spawn a task that will be subject to a deadline.
        let node = self.clone();
        let handshake_task = task::spawn(async move {
            // open the connection
            let stream = TcpStream::connect(remote_address).await?;
            let (mut reader, mut writer) = stream.into_split();

            let builder = snow::Builder::with_resolver(
                crate::HANDSHAKE_PATTERN
                    .parse()
                    .expect("Invalid noise handshake pattern!"),
                Box::new(snow::resolvers::SodiumResolver),
            );
            let static_key = builder.generate_keypair()?.private;
            let noise_builder = builder.local_private_key(&static_key).psk(3, crate::HANDSHAKE_PSK);
            let mut noise = noise_builder.build_initiator()?;
            let mut buffer: Box<[u8]> = vec![0u8; crate::MAX_MESSAGE_SIZE].into();
            let mut buf = [0u8; crate::NOISE_BUF_LEN]; // a temporary intermediate buffer to decrypt from

            // -> e
            let len = noise.write_message(&[], &mut buffer)?;
            writer.write_all(&[len as u8]).await?;
            writer.write_all(&buffer[..len]).await?;
            trace!("sent e (XX handshake part 1/3) to {}", remote_address);

            // <- e, ee, s, es
            reader.read_exact(&mut buf[..1]).await?;
            let len = buf[0] as usize;
            if len == 0 {
                return Err(NetworkError::InvalidHandshake);
            }
            let len = reader.read_exact(&mut buf[..len]).await?;
            let len = noise.read_message(&buf[..len], &mut buffer)?;
            let peer_version = Version::deserialize(&buffer[..len])?;
            trace!("received e, ee, s, es (XX handshake part 2/3) from {}", remote_address);

            if peer_version.node_id == node.id {
                return Err(NetworkError::SelfConnectAttempt);
            }
            if peer_version.version != crate::PROTOCOL_VERSION {
                return Err(NetworkError::InvalidHandshake);
            }

            // -> s, se, psk
            let own_version =
                Version::serialize(&Version::new(crate::PROTOCOL_VERSION, own_address.port(), node.id)).unwrap();
            let len = noise.write_message(&own_version, &mut buffer)?;
            writer.write_all(&[len as u8]).await?;
            writer.write_all(&buffer[..len]).await?;
            trace!("sent s, se, psk (XX handshake part 3/3) to {}", remote_address);

            // The remote_listener is the same as remote_address when initiating a connection.
            node.set_connected(remote_address, remote_address, noise, buffer, reader, writer)?;

            metrics::increment_counter!(stats::HANDSHAKES_SUCCESSES_INIT);

            Ok(())
        });

        // Fetch the bootnodes and determine if address is a bootnode.
        let bootnodes = self.config.bootnodes();
        let is_connecting_peer_bootnode = bootnodes.contains(&remote_address);

        // Check if the handshake doesn't time out.
        let timeout = match is_connecting_peer_bootnode {
            true => Duration::from_secs(crate::HANDSHAKE_BOOTNODE_TIMEOUT_SECS as u64),
            false => Duration::from_secs(crate::HANDSHAKE_PEER_TIMEOUT_SECS as u64),
        };
        match tokio::time::timeout(timeout, handshake_task).await {
            // the Result layers are: <timeout result>(<task join result>(<block result>)); JoinHandleError is returned
            // as NetworkError::InvalidHandshake, since there's not much to salvage there
            Ok(Ok(Ok(_))) => {}
            Ok(Ok(e)) => {
                metrics::increment_counter!(stats::HANDSHAKES_FAILURES_INIT);
                return e;
            }
            Ok(Err(_)) => {
                metrics::increment_counter!(stats::HANDSHAKES_FAILURES_INIT);
                return Err(NetworkError::InvalidHandshake);
            }
            Err(_) => {
                metrics::increment_counter!(stats::HANDSHAKES_TIMEOUTS_INIT);
                return Err(NetworkError::HandshakeTimeout);
            }
        }

        match is_connecting_peer_bootnode {
            true => info!("Connected to bootnode {}", remote_address),
            false => info!("Connected to peer {}", remote_address),
        };

        Ok(())
    }

    ///
    /// Broadcasts a connection request to all default bootnodes of the network.
    ///
    /// This function attempts to reconnect this node server with any bootnode peer
    /// that this node may have failed to connect to.
    ///
    /// This function filters out any bootnode peers the node server is
    /// either connnecting to or already connected to.
    ///
    fn connect_to_bootnodes(&self) {
        // Local address must be known by now.
        let own_address = self.local_address().unwrap();

        // Iterate through each bootnode address and attempt a connection request.
        for bootnode_address in self
            .config
            .bootnodes()
            .iter()
            .filter(|peer| **peer != own_address)
            .copied()
        {
            let node = self.clone();
            task::spawn(async move {
                match node.initiate_connection(bootnode_address).await {
                    Err(NetworkError::PeerAlreadyConnecting) | Err(NetworkError::PeerAlreadyConnected) => {
                        // no issue here, already connecting
                    }
                    Err(e @ NetworkError::TooManyConnections) => {
                        warn!("Couldn't connect to bootnode {}: {}", bootnode_address, e);
                        // the connection hasn't been established, no need to disconnect
                    }
                    Err(e) => {
                        warn!("Couldn't connect to bootnode {}: {}", bootnode_address, e);
                        node.disconnect_from_peer(bootnode_address);
                        node.peer_book.set_unroutable(bootnode_address);
                    }
                    Ok(_) => {}
                }
            });
        }
    }

    // pub(crate) fn crawl_peers(&self, count: usize, duration: std::time::Duration) {
    //     // Should be known at this point.
    //     let own_address = self.local_address().unwrap();
    //     let bootnodes = self.config.bootnodes();

    //     // returns at most `count` addrs
    //     let addrs = self
    //         .expect_network_topology()
    //         .never_crawled
    //         .read()
    //         .iter()
    //         .filter(|peer| **peer != own_address && !bootnodes.contains(peer))
    //         .copied()
    //         .choose_multiple(&mut rand::thread_rng(), count);

    //     // FIXME: also choose some routable and unroutable addrs to retry?

    //     for remote_address in addrs {
    //         let node_clone = self.clone();
    //         tokio::spawn(async move {
    //             match node_clone.initiate_connection(remote_address).await {
    //                 Err(NetworkError::PeerAlreadyConnecting) | Err(NetworkError::PeerAlreadyConnected) => {
    //                     // no issue here, already connecting
    //                 }
    //                 Err(e @ NetworkError::TooManyConnections) | Err(e @ NetworkError::SelfConnectAttempt) => {
    //                     warn!("Couldn't connect to peer {}: {}", remote_address, e);
    //                     // the connection hasn't been established, no need to disconnect
    //                 }
    //                 Err(e) => {
    //                     warn!("Couldn't connect to peer {}: {}", remote_address, e);
    //                     node_clone.disconnect_from_peer(remote_address);

    //                     // mark the peer as unroutable
    //                     node_clone.expect_network_topology().set_unroutable(remote_address);
    //                 }
    //                 Ok(_) => {
    //                     // keep track of the crawled peer
    //                     node_clone.expect_network_topology().set_routable(remote_address);

    //                     // request peer data
    //                     node_clone.send_request(Message::new(Direction::Outbound(remote_address), Payload::GetPeers));

    //                     // disconnect from the peer at the end of the interval.
    //                     tokio::time::sleep(duration).await;

    //                     node_clone.disconnect_from_peer(remote_address);
    //                 }
    //             }
    //         });
    //     }
    // }

    ///
    /// Broadcasts a connection request to all disconnected peers.
    ///
    fn connect_to_disconnected_peers(&self, count: usize) {
        // Local address must be known by now.
        let own_address = self.local_address().unwrap();

        // If this node is not a bootnode, attempt to satisfy the minimum number of peer connections.
        let random_peers = {
            trace!(
                "Connecting to {} disconnected peers",
                cmp::min(count, self.peer_book.disconnected_peers().len())
            );

            let bootnodes = self.config.bootnodes();

            // Iterate through a selection of random peers and attempt to connect.
            self.peer_book
                .disconnected_peers()
                .iter()
                .map(|(k, _)| k)
                .filter(|peer| **peer != own_address && !bootnodes.contains(peer))
                .copied()
                .choose_multiple(&mut rand::thread_rng(), count)
        };

        for remote_address in random_peers {
            let node = self.clone();
            task::spawn(async move {
                match node.initiate_connection(remote_address).await {
                    Err(NetworkError::PeerAlreadyConnecting) | Err(NetworkError::PeerAlreadyConnected) => {
                        // no issue here, already connecting
                    }
                    Err(e @ NetworkError::TooManyConnections) | Err(e @ NetworkError::SelfConnectAttempt) => {
                        warn!("Couldn't connect to peer {}: {}", remote_address, e);
                        // the connection hasn't been established, no need to disconnect
                    }
                    Err(e) => {
                        warn!("Couldn't connect to peer {}: {}", remote_address, e);
                        node.disconnect_from_peer(remote_address);
                        node.peer_book.set_unroutable(remote_address);
                    }
                    Ok(_) => {}
                }
            });
        }
    }

    /// Broadcasts a `GetPeers` message to all connected peers to request for more peers.
    fn broadcast_getpeers_requests(&self) {
        // Check that this node is not a bootnode.
        if !self.config.is_bootnode() {
            // Fetch the number of connected and connecting peers.
            let number_of_connected_peers = self.peer_book.number_of_connected_peers() as usize;
            let number_of_connecting_peers = self.peer_book.number_of_connecting_peers() as usize;

            // Check if this node server is below the permitted number of connected peers.
            let min_peers = self.config.minimum_number_of_connected_peers() as usize;
            if number_of_connected_peers + number_of_connecting_peers >= min_peers {
                return;
            }
        }

        trace!("Sending `GetPeers` requests to connected peers");

        for remote_address in self.connected_peers() {
            self.send_request(Message::new(Direction::Outbound(remote_address), Payload::GetPeers));
        }
    }

    /// Broadcasts a `Ping` message to all connected peers.
    fn broadcast_pings(&self) {
        trace!("Broadcasting `Ping` messages");

        // Consider peering tests that don't use the sync layer.
        let current_block_height = if let Some(ref sync) = self.sync() {
            sync.current_block_height()
        } else {
            0
        };

        for remote_address in self.connected_peers() {
            self.peer_book.sending_ping(remote_address);

            self.send_request(Message::new(
                Direction::Outbound(remote_address),
                Payload::Ping(current_block_height),
            ));
        }
    }

    /// TODO (howardwu): Implement manual serializers and deserializers to prevent forward breakage
    ///  when the PeerBook or PeerInfo struct fields change.
    ///
    /// Stores the current peer book to the given storage object.
    ///
    /// This function serializes the peer book into a byte vector for storage.
    ///
    #[inline]
    pub(crate) fn save_peer_book_to_storage(&self) -> Result<(), NetworkError> {
        // Serialize the peer book.
        let serialized_peer_book = bincode::serialize(&SerializedPeerBook::from(&self.peer_book))?;

        // TODO: the peer book should be stored outside of sync
        if let Some(ref sync) = self.sync() {
            // Save the serialized peer book to storage.
            sync.storage().save_peer_book_to_storage(serialized_peer_book)?;
        }

        Ok(())
    }

    ///
    /// Removes the given remote address channel and sets the peer in the peer book
    /// as disconnected from this node server.
    ///
    #[inline]
    pub fn disconnect_from_peer(&self, remote_address: SocketAddr) {
        // Set the peer as disconnected in the peer book.
        let was_connected = self.peer_book.set_disconnected(remote_address);

        // If the peer was truly disconnected, remove its channel and advise.
        if was_connected {
            self.outbound.channels.write().remove(&remote_address);
            trace!("Disconnected from {}", remote_address);
        }
    }

    #[inline]
    pub fn set_connected(
        &self,
        remote_address: SocketAddr,
        remote_listener: SocketAddr,
        noise: HandshakeState,
        buffer: Box<[u8]>,
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
    ) -> Result<(), NetworkError> {
        let noise = Arc::new(Mutex::new(noise.into_transport_mode()?));
        let mut reader = ConnReader::new(remote_listener, reader, buffer.clone(), Arc::clone(&noise));
        let mut writer = ConnWriter::new(remote_listener, writer, buffer, noise);

        // Create a channel dedicated to sending messages to the connection.
        let (sender, receiver) = channel(crate::OUTBOUND_CHANNEL_DEPTH);

        // The node might already have connected to this peer in the meantime; double-check it.
        if self.connected_peers().contains(&remote_listener) {
            return Err(NetworkError::PeerAlreadyConnected);
        }

        // Save the channel under the provided remote address.
        if self.outbound.channels.write().insert(remote_listener, sender).is_some() {
            error!("The outbound channel for {} had already existed!", remote_listener);
        }

        // Listen for inbound messages.
        let node = self.clone();
        let peer_reading_task = tokio::spawn(async move {
            node.listen_for_inbound_messages(&mut reader).await;
        });

        // Listen for outbound messages.
        let node = self.clone();
        let peer_writing_task = tokio::spawn(async move {
            node.listen_for_outbound_messages(receiver, &mut writer).await;
        });

        // Mark the peer as connected.
        self.peer_book.set_connected(remote_address, Some(remote_listener));

        if let Some(peer) = self.peer_book.get_peer(remote_listener, true) {
            peer.register_task(peer_reading_task, true);
            peer.register_task(peer_writing_task, false);
        } else {
            peer_reading_task.abort();
            peer_writing_task.abort();

            return Err(NetworkError::PeerIsDisconnected);
        }

        trace!("Connected to {} (listener: {})", remote_address, remote_listener);

        Ok(())
    }

    pub(crate) fn send_peers(&self, remote_address: SocketAddr) {
        // Broadcast the sanitized list of connected peers back to the requesting peer.
        let peers = self
            .peer_book
            .connected_peers()
            .iter()
            .map(|(k, _)| k)
            .filter(|&addr| *addr != remote_address)
            .copied()
            .choose_multiple(&mut rand::thread_rng(), crate::SHARED_PEER_COUNT);

        self.send_request(Message::new(Direction::Outbound(remote_address), Payload::Peers(peers)));
    }

    /// A node has sent their list of peer addresses.
    /// Add all new/updated addresses to our disconnected.
    /// The connection handler will be responsible for sending out handshake requests to them.
    pub(crate) fn process_inbound_peers(&self, source: SocketAddr, peers: Vec<SocketAddr>) {
        let local_address = self.local_address().unwrap(); // the address must be known by now

        for peer_address in peers.iter().filter(|&peer_addr| *peer_addr != local_address).copied() {
            // Inform the peer book that we found a peer.
            // The peer book will determine if we have seen the peer before,
            // and include the peer if it is new.
            self.peer_book.add_peer(peer_address);
        }

        if let Some(topology) = self.network_topology.get() {
            // If this node is tracking the network topology, record the connections. This can
            // then be used to construct the graph and query peer info from the peerbook.

            topology.update(source, peers);
        }
    }

    pub fn can_connect(&self) -> bool {
        let num_connected = self.peer_book.number_of_connected_peers() as usize;
        let num_connecting = self.peer_book.number_of_connecting_peers() as usize;

        let max_peers = self.config.maximum_number_of_connected_peers() as usize;

        if num_connected >= max_peers || num_connected + num_connecting >= max_peers {
            warn!(
                "Max number of connections ({} connecting, {} connected; max: {}) reached",
                num_connecting, num_connected, max_peers
            );
            false
        } else {
            true
        }
    }
}
