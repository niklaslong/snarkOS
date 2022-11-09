// Copyright (C) 2019-2022 Aleo Systems Inc.
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

mod router;

use crate::traits::NodeInterface;
use snarkos_account::Account;
use snarkos_node_consensus::Consensus;
use snarkos_node_executor::{spawn_task_loop, Executor, NodeType, Status};
use snarkos_node_ledger::Ledger;
use snarkos_node_messages::{Data, Message, PuzzleResponse, UnconfirmedBlock, UnconfirmedSolution};
use snarkos_node_network::Network as NodeNetwork;
use snarkos_node_rest::Rest;
use snarkos_node_router::{Handshake, Inbound, Outbound, Router, RouterRequest};
use snarkos_node_store::ConsensusDB;
use snarkvm::prelude::{Address, Block, Network, PrivateKey, ViewKey};

use anyhow::{bail, Result};
use core::time::Duration;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use time::OffsetDateTime;
use tokio::time::timeout;

/// A beacon is a full node, capable of producing blocks.
#[derive(Clone)]
pub struct Beacon<N: Network> {
    /// The account of the node.
    account: Account<N>,
    /// The consensus module of the node.
    consensus: Consensus<N, ConsensusDB<N>>,
    /// The ledger of the node.
    ledger: Ledger<N, ConsensusDB<N>>,
    /// The router of the node.
    router: Router<N>,
    /// The REST server of the node.
    rest: Option<Arc<Rest<N, ConsensusDB<N>>>>,
    /// The time it to generate a block.
    block_generation_time: Arc<AtomicU64>,
    /// The shutdown signal.
    shutdown: Arc<AtomicBool>,

    // TODO(nkls): potentially encapsulate.
    network: NodeNetwork,
    connection_meta: Arc<RwLock<HashMap<SocketAddr, ConnectionMeta>>>,
    trusted_peers: Arc<HashSet<SocketAddr>>,
    candidate_peers: Arc<RwLock<HashSet<SocketAddr>>>,
    restricted_peers: Arc<RwLock<HashMap<SocketAddr, OffsetDateTime>>>,
}

impl<N: Network> Beacon<N> {
    /// Initializes a new beacon node.
    pub async fn new(
        node_ip: SocketAddr,
        rest_ip: Option<SocketAddr>,
        private_key: PrivateKey<N>,
        trusted_peers: &[SocketAddr],
        genesis: Option<Block<N>>,
        dev: Option<u16>,
    ) -> Result<Self> {
        // Initialize the node account.
        let account = Account::from(private_key)?;
        // Initialize the ledger.
        let ledger = Ledger::load(genesis, dev)?;
        // Initialize the consensus.
        let consensus = Consensus::new(ledger.clone())?;
        // Initialize the node router.
        let (router, router_receiver) = Router::new::<Self>(node_ip, account.address(), trusted_peers).await?;
        // Initialize the REST server.
        let rest = match rest_ip {
            Some(rest_ip) => Some(Arc::new(Rest::start(
                rest_ip,
                account.address(),
                Some(consensus.clone()),
                ledger.clone(),
                router.clone(),
            )?)),
            None => None,
        };
        // Initialize the block generation time.
        let block_generation_time = Arc::new(AtomicU64::new(2));
        // Initialize the node.
        let node = Self {
            account,
            consensus,
            ledger,
            router: router.clone(),
            rest,
            block_generation_time,
            shutdown: Default::default(),
            // TODO(nkls), wire up configuration.
            network: NodeNetwork::new(Default::default()).await?,
            connection_meta: Default::default(),
            trusted_peers: Default::default(),
            candidate_peers: Default::default(),
            restricted_peers: Default::default(),
        };

        // Enable the node's protocols.
        node.enable_handshake().await;
        node.enable_reading().await;
        node.enable_writing().await;
        node.enable_disconnect().await;

        // Initialize the router handler.
        router.initialize_handler(node.clone(), router_receiver).await;

        // Initialize the block production.
        node.initialize_block_production().await;
        // Initialize the signal handler.
        node.handle_signals();
        // Return the node.
        Ok(node)
    }

    /// Returns the ledger.
    pub fn ledger(&self) -> &Ledger<N, ConsensusDB<N>> {
        &self.ledger
    }

    /// Returns the REST server.
    pub fn rest(&self) -> &Option<Arc<Rest<N, ConsensusDB<N>>>> {
        &self.rest
    }
}

#[async_trait]
impl<N: Network> Executor for Beacon<N> {
    /// The node type.
    const NODE_TYPE: NodeType = NodeType::Beacon;

    /// Disconnects from peers and shuts down the node.
    async fn shut_down(&self) {
        info!("Shutting down...");
        // Update the node status.
        Self::status().update(Status::ShuttingDown);

        // Shut down the ledger.
        trace!("Proceeding to shut down the ledger...");
        self.shutdown.store(true, Ordering::SeqCst);

        // Flush the tasks.
        Self::resources().shut_down();
        trace!("Node has shut down.");
    }
}

impl<N: Network> NodeInterface<N> for Beacon<N> {
    /// Returns the node type.
    fn node_type(&self) -> NodeType {
        Self::NODE_TYPE
    }

    /// Returns the node router.
    fn router(&self) -> &Router<N> {
        &self.router
    }

    /// Returns the account private key of the node.
    fn private_key(&self) -> &PrivateKey<N> {
        self.account.private_key()
    }

    /// Returns the account view key of the node.
    fn view_key(&self) -> &ViewKey<N> {
        self.account.view_key()
    }

    /// Returns the account address of the node.
    fn address(&self) -> Address<N> {
        self.account.address()
    }
}

/// A helper method to check if the coinbase target has been met.
async fn check_for_coinbase<N: Network>(consensus: Consensus<N, ConsensusDB<N>>) {
    loop {
        // Check if the coinbase target has been met.
        match consensus.is_coinbase_target_met() {
            Ok(true) => break,
            Ok(false) => (),
            Err(error) => error!("Failed to check if coinbase target is met: {error}"),
        }
        // Sleep for one second.
        tokio::time::sleep(Duration::from_secs(1)).await
    }
}

impl<N: Network> Beacon<N> {
    /// Initialize a new instance of block production.
    async fn initialize_block_production(&self) {
        let beacon = self.clone();
        spawn_task_loop!(Self, {
            // Expected time per block.
            const ROUND_TIME: u64 = 15; // 15 seconds per block

            // Produce blocks.
            loop {
                // Fetch the current timestamp.
                let current_timestamp = OffsetDateTime::now_utc().unix_timestamp();
                // Compute the elapsed time.
                let elapsed_time = current_timestamp.saturating_sub(beacon.ledger.latest_timestamp()) as u64;

                // Do not produce a block if the elapsed time has not exceeded `ROUND_TIME - block_generation_time`.
                // This will ensure a block is produced at intervals of approximately `ROUND_TIME`.
                let time_to_wait = ROUND_TIME.saturating_sub(beacon.block_generation_time.load(Ordering::SeqCst));
                trace!("Waiting for {time_to_wait} seconds before producing a block...");
                if elapsed_time < time_to_wait {
                    if let Err(error) = timeout(
                        Duration::from_secs(time_to_wait.saturating_sub(elapsed_time)),
                        check_for_coinbase(beacon.consensus.clone()),
                    )
                    .await
                    {
                        trace!("Check for coinbase - {error}");
                    }
                }

                // Start a timer.
                let timer = std::time::Instant::now();
                // Produce the next block and propagate it to all peers.
                match beacon.produce_next_block().await {
                    // Update the block generation time.
                    Ok(()) => beacon.block_generation_time.store(timer.elapsed().as_secs(), Ordering::SeqCst),
                    Err(error) => error!("{error}"),
                }

                // If the Ctrl-C handler registered the signal, stop the node once the current block is complete.
                if beacon.shutdown.load(Ordering::Relaxed) {
                    info!("Shutting down block production");
                    break;
                }
            }
        });
    }

    /// Produces the next block and propagates it to all peers.
    async fn produce_next_block(&self) -> Result<()> {
        // Produce a transaction if the mempool is empty.
        if self.consensus.memory_pool().num_unconfirmed_transactions() == 0 {
            // Create a transfer transaction.
            let beacon = self.clone();
            let transaction = match tokio::task::spawn_blocking(move || {
                beacon.ledger.create_transfer(beacon.private_key(), beacon.address(), 1)
            })
            .await
            {
                Ok(Ok(transaction)) => transaction,
                Ok(Err(error)) => bail!("Failed to create a transfer transaction for the next block: {error}"),
                Err(error) => bail!("Failed to create a transfer transaction for the next block: {error}"),
            };
            // Add the transaction to the memory pool.
            let beacon = self.clone();
            match tokio::task::spawn_blocking(move || beacon.consensus.add_unconfirmed_transaction(transaction)).await {
                Ok(Ok(())) => (),
                Ok(Err(error)) => bail!("Failed to add the transaction to the memory pool: {error}"),
                Err(error) => bail!("Failed to add the transaction to the memory pool: {error}"),
            }
        }

        // Propose the next block.
        let beacon = self.clone();
        let next_block = match tokio::task::spawn_blocking(move || {
            let next_block = beacon.consensus.propose_next_block(beacon.private_key(), &mut rand::thread_rng())?;

            // Ensure the block is a valid next block.
            if let Err(error) = beacon.consensus.check_next_block(&next_block) {
                // Clear the memory pool of all solutions and transactions.
                trace!("Clearing the memory pool...");
                beacon.consensus.clear_memory_pool()?;
                trace!("Cleared the memory pool");
                bail!("Proposed an invalid block: {error}")
            }

            // Advance to the next block.
            match beacon.consensus.advance_to_next_block(&next_block) {
                Ok(()) => match serde_json::to_string_pretty(&next_block.header()) {
                    Ok(header) => info!("Block {}: {header}", next_block.height()),
                    Err(error) => info!("Block {}: (serde failed: {error})", next_block.height()),
                },
                Err(error) => {
                    // Clear the memory pool of all solutions and transactions.
                    trace!("Clearing the memory pool...");
                    beacon.consensus.clear_memory_pool()?;
                    trace!("Cleared the memory pool");
                    bail!("Failed to advance to the next block: {error}")
                }
            }

            Ok(next_block)
        })
        .await
        {
            Ok(Ok(next_block)) => next_block,
            Ok(Err(error)) => {
                // Sleep for one second.
                tokio::time::sleep(Duration::from_secs(1)).await;
                bail!("Failed to propose the next block: {error}")
            }
            Err(error) => {
                // Sleep for one second.
                tokio::time::sleep(Duration::from_secs(1)).await;
                bail!("Failed to propose the next block (JoinError): {error}")
            }
        };
        let next_block_height = next_block.height();
        let next_block_hash = next_block.hash();

        // // Ensure the block is a valid next block.
        // if let Err(error) = self.consensus.check_next_block(&next_block) {
        //     // Clear the memory pool of all solutions and transactions.
        //     trace!("Clearing the memory pool...");
        //     self.consensus.clear_memory_pool()?;
        //     trace!("Cleared the memory pool");
        //     // Sleep for one second.
        //     tokio::time::sleep(Duration::from_secs(1)).await;
        //     bail!("Proposed an invalid block: {error}")
        // }
        //
        // // Advance to the next block.
        // match self.consensus.advance_to_next_block(&next_block) {
        //     Ok(()) => match serde_json::to_string_pretty(&next_block.header()) {
        //         Ok(header) => info!("Block {next_block_height}: {header}"),
        //         Err(error) => info!("Block {next_block_height}: (serde failed: {error})"),
        //     },
        //     Err(error) => {
        //         // Clear the memory pool of all solutions and transactions.
        //         trace!("Clearing the memory pool...");
        //         self.consensus.clear_memory_pool()?;
        //         trace!("Cleared the memory pool");
        //         // Sleep for one second.
        //         tokio::time::sleep(Duration::from_secs(1)).await;
        //         bail!("Failed to advance to the next block: {error}")
        //     }
        // }

        // Serialize the block ahead of time to not do it for each peer.
        let serialized_block = match Data::Object(next_block).serialize().await {
            Ok(serialized_block) => serialized_block,
            Err(error) => bail!("Failed to serialize the next block for propagation: {error}"),
        };

        // Prepare the block to be sent to all peers.
        let message = Message::<N>::UnconfirmedBlock(UnconfirmedBlock {
            block_height: next_block_height,
            block_hash: next_block_hash,
            block: Data::Buffer(serialized_block),
        });

        // Propagate the block to all peers.
        if let Err(error) = self.router.process(RouterRequest::MessagePropagate(message, vec![])).await {
            trace!("Failed to broadcast the next block: {error}");
        }

        Ok(())
    }
}

/* Network traits */

use snarkos_node_executor::RawStatus;
use snarkos_node_messages::{MessageOrBytes, NoiseCodec, NoiseState, PeerRequest};
use snarkos_node_network::{
    protocols::{Disconnect, Handshake as Handshaking, Reading, Writing},
    Connection,
    ConnectionSide,
    P2P,
};

use std::{
    collections::HashSet,
    io,
    time::{Instant, SystemTime},
};

use rand::{
    prelude::{IteratorRandom, SliceRandom},
    rngs::OsRng,
};

impl<N: Network> Beacon<N> {
    pub fn noise_state(&self, addr: SocketAddr) -> Option<NoiseState> {
        self.connection_meta.read().get(&addr).map(|meta| meta.noise_state.clone())
    }

    pub fn trusted_peers(&self) -> &HashSet<SocketAddr> {
        &self.trusted_peers
    }

    pub fn candidate_peers(&self) -> Vec<SocketAddr> {
        self.candidate_peers.read().iter().copied().collect()
    }

    pub fn insert_candidate_peer(&self, addr: SocketAddr) {
        self.candidate_peers.write().insert(addr);
    }

    pub fn remove_candidate_peer(&self, addr: SocketAddr) {
        self.candidate_peers.write().remove(&addr);
    }

    pub fn insert_restricted_peer(&self, addr: SocketAddr) {
        self.restricted_peers.write().insert(addr, OffsetDateTime::now_utc());
    }

    pub fn remove_restricted_peer(&self, addr: SocketAddr) {
        self.restricted_peers.write().remove(&addr);
    }

    pub fn connected_beacons(&self) -> Vec<SocketAddr> {
        self.connection_meta
            .read()
            .iter()
            .filter(|(addr, meta)| meta.node_type == NodeType::Beacon)
            .map(|(addr, meta)| addr)
            .copied()
            .collect()
    }

    pub async fn start_periodic_tasks(&self) {
        let node = self.clone();
        // TODO(nkls): task accounting.
        tokio::spawn(async move {
            loop {
                node.heartbeat().await;
                // Sleep for `Self::HEARTBEAT_IN_SECS` seconds.
                tokio::time::sleep(Duration::from_secs(Router::<N>::HEARTBEAT_IN_SECS)).await;
            }
        });
    }

    pub async fn heartbeat(&self) {
        // tl;dr:
        // 1. ensure min-max peers (disconnect, peer requests to trusted peers, attempting
        //    connections).
        // 2. ensure trusted peers are connected.
        // 3. ensure only one beacon is connected.

        // Ensure the node has less than MAX PEERS. This shouldn't be necessary as this is checked
        // in the network upon connection but might as well sanity check it here.
        let num_excess_peers = self.network.num_connected().saturating_sub(Router::<N>::MAXIMUM_NUMBER_OF_PEERS);
        if num_excess_peers > 0 {
            debug!("Exceeded maximum number of connected peers, disconnecting from {num_excess_peers} peers");

            for peer_addr in self
                .network
                .connected_addrs()
                .into_iter()
                .filter(|peer_addr| !self.trusted_peers().contains(peer_addr))
                .take(num_excess_peers)
            {
                info!("Disconnecting from 'peer' {peer_addr}");

                let _disconnected = self.network.disconnect(peer_addr).await;
                debug_assert!(_disconnected);
            }
        }

        // Ensure the node is only connected to one beacon.
        let connected_beacons = self.connected_beacons();
        let num_excess_beacons = connected_beacons.len().saturating_sub(1);
        if num_excess_beacons > 0 {
            debug!("Exceeded maximum number of connected beacons by {num_excess_beacons}");

            for beacon_addr in connected_beacons.into_iter().choose_multiple(&mut OsRng::default(), num_excess_beacons)
            {
                info!("Disconnecting from 'beacon' {beacon_addr}");

                let _disconnected = self.network.disconnect(beacon_addr).await;
                debug_assert!(_disconnected);
            }
        }

        // Ensure the trusted peers are connected.
        for trusted_peer_addr in self.trusted_peers().into_iter() {
            if !self.network.is_connected(*trusted_peer_addr) {
                info!("Connecting to 'trusted peer' {trusted_peer_addr}");

                // Silence the error if there is any, this isn't a halting case.
                let _connected = self.network.connect(*trusted_peer_addr).await;
                debug_assert!(_connected.is_ok());
            }
        }

        // Ensure the node has more peers than MIN PEERS.
        let num_connected = self.network.num_connected();
        let num_missing_peers = Router::<N>::MINIMUM_NUMBER_OF_PEERS.saturating_sub(num_connected);

        if num_missing_peers > 0 {
            for candidate_addr in self.candidate_peers().into_iter().take(num_missing_peers) {
                let connection_succesful = self.network.connect(candidate_addr).await.is_ok();
                self.remove_candidate_peer(candidate_addr);

                if !connection_succesful {
                    self.insert_restricted_peer(candidate_addr)
                }
            }

            // If we have existing peers, request more addresses from them.
            if num_connected > 0 {
                for peer_addr in self.network.connected_addrs().choose_multiple(&mut OsRng::default(), 3) {
                    // Let the error through for now.
                    let _res =
                        self.unicast(*peer_addr, MessageOrBytes::Message(Box::new(Message::PeerRequest(PeerRequest))));
                    debug_assert!(_res.expect("writing protocol should be enabled").await.is_ok());
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ConnectionMeta {
    side: ConnectionSide,
    noise_state: NoiseState,

    // TODO(nkls): potentially split this out.
    // Peer Meta:
    version: u32,
    node_type: NodeType,
    status: RawStatus,
    block_height: Arc<RwLock<u32>>,  // TODO(nkls): this could probably be an atomic.
    last_seen: Arc<RwLock<Instant>>, // TODO(nkls): consider the time crate here.
    seen_messages: Arc<RwLock<HashMap<(u16, u32), SystemTime>>>,
}

impl ConnectionMeta {
    fn new(side: ConnectionSide, noise_state: NoiseState, version: u32, node_type: NodeType) -> Self {
        Self {
            side,
            noise_state,
            version,
            node_type,
            status: RawStatus::new(),
            block_height: Arc::new(RwLock::new(0)),
            last_seen: Arc::new(RwLock::new(Instant::now())),
            seen_messages: Default::default(),
        }
    }
}

impl<N: Network> P2P for Beacon<N> {
    fn network(&self) -> &NodeNetwork {
        &self.network
    }
}

#[async_trait::async_trait]
impl<N: Network> Handshaking for Beacon<N> {
    async fn perform_handshake(&self, conn: Connection) -> io::Result<Connection> {
        let peer_side = conn.side();

        match peer_side {
            // The peer initiated the connection.
            ConnectionSide::Initiator => {}

            // The relay initiated the connection.
            ConnectionSide::Responder => {}
        }

        Ok(conn)
    }
}

#[async_trait::async_trait]
impl<N: Network> Reading for Beacon<N> {
    type Codec = NoiseCodec;
    type Message = MessageOrBytes;

    fn codec(&self, addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
        let noise_state = self.noise_state(addr).unwrap();
        NoiseCodec::new(noise_state)
    }

    async fn process_message(&self, source: SocketAddr, message: Self::Message) -> io::Result<()> {
        todo!()
    }
}

#[async_trait::async_trait]
impl<N: Network> Writing for Beacon<N> {
    type Codec = NoiseCodec;
    type Message = MessageOrBytes;

    fn codec(&self, addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
        let noise_state = self.noise_state(addr).unwrap();
        NoiseCodec::new(noise_state)
    }
}

#[async_trait::async_trait]
impl<N: Network> Disconnect for Beacon<N> {
    async fn handle_disconnect(&self, _addr: SocketAddr) {}
}
