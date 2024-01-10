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

mod router;

use crate::traits::NodeInterface;
use indexmap::IndexMap;
use snarkos_account::Account;
use snarkos_node_bft::{helpers::init_primary_channels, ledger_service::CoreLedgerService};
use snarkos_node_consensus::Consensus;
use snarkos_node_rest::Rest;
use snarkos_node_router::{
    messages::{NodeType, PuzzleResponse, UnconfirmedSolution, UnconfirmedTransaction},
    Heartbeat,
    Inbound,
    Outbound,
    Router,
    Routing,
};
use snarkos_node_sync::{BlockSync, BlockSyncMode};
use snarkos_node_tcp::{
    protocols::{Disconnect, Handshake, OnConnect, Reading, Writing},
    P2P,
};
use snarkvm::{
    console::types::Address,
    ledger::{block_reward, coinbase_reward, committee::Committee},
    prelude::{
        block::{Block, Header},
        coinbase::ProverSolution,
        store::ConsensusStorage,
        Ledger,
        Network,
    },
    synthesizer::{ensure_stakers_matches, staking_rewards, to_next_committee},
};
use std::collections::HashMap;

use anyhow::Result;
use core::future::Future;
use parking_lot::Mutex;
use std::{
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};
use tokio::task::JoinHandle;

/// A validator is a full node, capable of validating blocks.
#[derive(Clone)]
pub struct Validator<N: Network, C: ConsensusStorage<N>> {
    /// The ledger of the node.
    ledger: Ledger<N, C>,
    /// The consensus module of the node.
    consensus: Consensus<N>,
    /// The router of the node.
    router: Router<N>,
    /// The REST server of the node.
    rest: Option<Rest<N, C, Self>>,
    /// The sync module.
    sync: BlockSync<N>,
    /// The spawned handles.
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// The shutdown signal.
    shutdown: Arc<AtomicBool>,
}

impl<N: Network, C: ConsensusStorage<N>> Validator<N, C> {
    /// Initializes a new validator node.
    pub async fn new(
        node_ip: SocketAddr,
        bft_ip: Option<SocketAddr>,
        rest_ip: Option<SocketAddr>,
        rest_rps: u32,
        account: Account<N>,
        trusted_peers: &[SocketAddr],
        trusted_validators: &[SocketAddr],
        genesis: Block<N>,
        cdn: Option<String>,
        dev: Option<u16>,
    ) -> Result<Self> {
        // Prepare the shutdown flag.
        let shutdown: Arc<AtomicBool> = Default::default();

        // Initialize the signal handler.
        let signal_node = Self::handle_signals(shutdown.clone());

        // Initialize the ledger.
        let ledger = Ledger::load(genesis, dev)?;
        // TODO: Remove me after Phase 3.
        let ledger = crate::phase_3_reset(ledger, dev)?;
        // Initialize the CDN.
        if let Some(base_url) = cdn {
            // Sync the ledger with the CDN.
            if let Err((_, error)) =
                snarkos_node_cdn::sync_ledger_with_cdn(&base_url, ledger.clone(), shutdown.clone()).await
            {
                crate::log_clean_error(dev);
                return Err(error);
            }
        }

        // Initialize the ledger service.
        let ledger_service = Arc::new(CoreLedgerService::new(ledger.clone(), shutdown.clone()));
        // Initialize the sync module.
        let sync = BlockSync::new(BlockSyncMode::Gateway, ledger_service.clone());

        // Initialize the consensus.
        let mut consensus = Consensus::new(account.clone(), ledger_service, bft_ip, trusted_validators, dev)?;
        // Initialize the primary channels.
        let (primary_sender, primary_receiver) = init_primary_channels::<N>();
        // Start the consensus.
        consensus.run(primary_sender, primary_receiver).await?;

        // Initialize the node router.
        let router = Router::new(
            node_ip,
            NodeType::Validator,
            account,
            trusted_peers,
            Self::MAXIMUM_NUMBER_OF_PEERS as u16,
            dev.is_some(),
        )
        .await?;

        // Initialize the node.
        let mut node = Self {
            ledger: ledger.clone(),
            consensus: consensus.clone(),
            router,
            rest: None,
            sync,
            handles: Default::default(),
            shutdown,
        };
        // Initialize the transaction pool.
        node.initialize_transaction_pool(dev)?;

        // Initialize the REST server.
        if let Some(rest_ip) = rest_ip {
            node.rest =
                Some(Rest::start(rest_ip, rest_rps, Some(consensus), ledger.clone(), Arc::new(node.clone())).await?);
        }
        // Initialize the routing.
        node.initialize_routing().await;
        // Initialize the notification message loop.
        node.handles.lock().push(crate::start_notification_message_loop());
        // Pass the node to the signal handler.
        let _ = signal_node.set(node.clone());
        // Return the node.
        Ok(node)
    }

    /// Returns the ledger.
    pub fn ledger(&self) -> &Ledger<N, C> {
        &self.ledger
    }

    /// Returns the REST server.
    pub fn rest(&self) -> &Option<Rest<N, C, Self>> {
        &self.rest
    }
}

impl<N: Network, C: ConsensusStorage<N>> Validator<N, C> {
    // /// Initialize the transaction pool.
    // fn initialize_transaction_pool(&self, dev: Option<u16>) -> Result<()> {
    //     use snarkvm::{
    //         console::{
    //             account::ViewKey,
    //             program::{Identifier, Literal, Plaintext, ProgramID, Record, Value},
    //             types::U64,
    //         },
    //         ledger::block::transition::Output,
    //     };
    //     use std::str::FromStr;
    //
    //     // Initialize the locator.
    //     let locator = (ProgramID::from_str("credits.aleo")?, Identifier::from_str("split")?);
    //     // Initialize the record name.
    //     let record_name = Identifier::from_str("credits")?;
    //
    //     /// Searches the genesis block for the mint record.
    //     fn search_genesis_for_mint<N: Network>(
    //         block: Block<N>,
    //         view_key: &ViewKey<N>,
    //     ) -> Option<Record<N, Plaintext<N>>> {
    //         for transition in block.transitions().filter(|t| t.is_mint()) {
    //             if let Output::Record(_, _, Some(ciphertext)) = &transition.outputs()[0] {
    //                 if ciphertext.is_owner(view_key) {
    //                     match ciphertext.decrypt(view_key) {
    //                         Ok(record) => return Some(record),
    //                         Err(error) => {
    //                             error!("Failed to decrypt the mint output record - {error}");
    //                             return None;
    //                         }
    //                     }
    //                 }
    //             }
    //         }
    //         None
    //     }
    //
    //     /// Searches the block for the split record.
    //     fn search_block_for_split<N: Network>(
    //         block: Block<N>,
    //         view_key: &ViewKey<N>,
    //     ) -> Option<Record<N, Plaintext<N>>> {
    //         let mut found = None;
    //         // TODO (howardwu): Switch to the iterator when DoubleEndedIterator is supported.
    //         // block.transitions().rev().for_each(|t| {
    //         let splits = block.transitions().filter(|t| t.is_split()).collect::<Vec<_>>();
    //         splits.iter().rev().for_each(|t| {
    //             if found.is_some() {
    //                 return;
    //             }
    //             let Output::Record(_, _, Some(ciphertext)) = &t.outputs()[1] else {
    //                 error!("Failed to find the split output record");
    //                 return;
    //             };
    //             if ciphertext.is_owner(view_key) {
    //                 match ciphertext.decrypt(view_key) {
    //                     Ok(record) => found = Some(record),
    //                     Err(error) => {
    //                         error!("Failed to decrypt the split output record - {error}");
    //                     }
    //                 }
    //             }
    //         });
    //         found
    //     }
    //
    //     let self_ = self.clone();
    //     self.spawn(async move {
    //         // Retrieve the view key.
    //         let view_key = self_.view_key();
    //         // Initialize the record.
    //         let mut record = {
    //             let mut found = None;
    //             let mut height = self_.ledger.latest_height();
    //             while found.is_none() && height > 0 {
    //                 // Retrieve the block.
    //                 let Ok(block) = self_.ledger.get_block(height) else {
    //                     error!("Failed to get block at height {}", height);
    //                     break;
    //                 };
    //                 // Search for the latest split record.
    //                 if let Some(record) = search_block_for_split(block, view_key) {
    //                     found = Some(record);
    //                 }
    //                 // Decrement the height.
    //                 height = height.saturating_sub(1);
    //             }
    //             match found {
    //                 Some(record) => record,
    //                 None => {
    //                     // Retrieve the genesis block.
    //                     let Ok(block) = self_.ledger.get_block(0) else {
    //                         error!("Failed to get the genesis block");
    //                         return;
    //                     };
    //                     // Search the genesis block for the mint record.
    //                     if let Some(record) = search_genesis_for_mint(block, view_key) {
    //                         found = Some(record);
    //                     }
    //                     found.expect("Failed to find the split output record")
    //                 }
    //             }
    //         };
    //         info!("Starting transaction pool...");
    //         // Start the transaction loop.
    //         loop {
    //             tokio::time::sleep(Duration::from_secs(1)).await;
    //             // If the node is running in development mode, only generate if you are allowed.
    //             if let Some(dev) = dev {
    //                 if dev != 0 {
    //                     continue;
    //                 }
    //             }
    //
    //             // Prepare the inputs.
    //             let inputs = [Value::from(record.clone()), Value::from(Literal::U64(U64::new(1)))].into_iter();
    //             // Execute the transaction.
    //             let transaction = match self_.ledger.vm().execute(
    //                 self_.private_key(),
    //                 locator,
    //                 inputs,
    //                 None,
    //                 None,
    //                 &mut rand::thread_rng(),
    //             ) {
    //                 Ok(transaction) => transaction,
    //                 Err(error) => {
    //                     error!("Transaction pool encountered an execution error - {error}");
    //                     continue;
    //                 }
    //             };
    //             // Retrieve the transition.
    //             let Some(transition) = transaction.transitions().next() else {
    //                 error!("Transaction pool encountered a missing transition");
    //                 continue;
    //             };
    //             // Retrieve the second output.
    //             let Output::Record(_, _, Some(ciphertext)) = &transition.outputs()[1] else {
    //                 error!("Transaction pool encountered a missing output");
    //                 continue;
    //             };
    //             // Save the second output record.
    //             let Ok(next_record) = ciphertext.decrypt(view_key) else {
    //                 error!("Transaction pool encountered a decryption error");
    //                 continue;
    //             };
    //             // Broadcast the transaction.
    //             if self_
    //                 .unconfirmed_transaction(
    //                     self_.router.local_ip(),
    //                     UnconfirmedTransaction::from(transaction.clone()),
    //                     transaction.clone(),
    //                 )
    //                 .await
    //             {
    //                 info!("Transaction pool broadcasted the transaction");
    //                 let commitment = next_record.to_commitment(&locator.0, &record_name).unwrap();
    //                 while !self_.ledger.contains_commitment(&commitment).unwrap_or(false) {
    //                     tokio::time::sleep(Duration::from_secs(1)).await;
    //                 }
    //                 info!("Transaction accepted by the ledger");
    //             }
    //             // Save the record.
    //             record = next_record;
    //         }
    //     });
    //     Ok(())
    // }

    /// Initialize the transaction pool.
    fn initialize_transaction_pool(&self, dev: Option<u16>) -> Result<()> {
        use snarkvm::console::{
            program::{Identifier, Literal, ProgramID, Value},
            types::U64,
        };
        use std::str::FromStr;

        // Initialize the locator.
        // let locator = (ProgramID::from_str("credits.aleo")?, Identifier::from_str("transfer_public")?);

        // Determine whether to start the loop.
        match dev {
            // If the node is running in development mode, only generate if you are allowed.
            Some(dev) => {
                // If the node is not the first node, do not start the loop.
                if dev != 0 {
                    return Ok(());
                }
            }
            None => {
                // Retrieve the genesis committee.
                let Ok(Some(committee)) = self.ledger.get_committee_for_round(0) else {
                    // If the genesis committee is not available, do not start the loop.
                    return Ok(());
                };
                // Retrieve the first member.
                // Note: It is guaranteed that the committee has at least one member.
                let first_member = committee.members().first().unwrap().0;
                // If the node is not the first member, do not start the loop.
                if self.address() != *first_member {
                    return Ok(());
                }
            }
        }

        let self_ = self.clone();
        let locator_bond = (ProgramID::from_str("credits.aleo")?, Identifier::from_str("bond_public")?);
        self.spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            info!("Starting transaction pool...");

                     let mut handled = HashMap::new();

             // Here, for every `round`, we try to find the parameters that make myself leader in `round + 4`.
             // We do this by manipulating the stake of myself in the committee. We just need O(committee_size) of tries to find the parameters.
             loop {
                 tokio::time::sleep(Duration::from_millis(10)).await;
                 let latest_round = self_.ledger.latest_round();
                 if handled.get(&latest_round).is_some() {
                     continue;
                 }
                 let current_committee = self_.ledger.latest_committee().unwrap();
                 let current_block = self_.ledger.latest_block();
                 let next_round = latest_round + 2;
                 if current_committee.starting_round() != latest_round {
                     continue;
                 }
                 info!("latest_round {latest_round}, next round {next_round} current_committee {current_committee}");

                 if latest_round % 2 == 1 {
                     continue;
                 }
                 handled.insert(latest_round, true);
                 info!("latest_round {latest_round}");

                 let mut bound_amount: u64 = 1_000_000;
                 {
                     const BASE_AMOUNT: u64 = 1_000_000;
                     // Calculate the coinbase reward.
                     let cumulative_proof_target = current_block.cumulative_proof_target();
                     info!("current_block cumulative_proof_target {cumulative_proof_target}");
                     let coinbase_reward = coinbase_reward(
                         current_block.height().saturating_add(1),
                         N::STARTING_SUPPLY,
                         N::ANCHOR_HEIGHT,
                         N::BLOCK_TIME,
                         0,
                         0,
                         current_block.coinbase_target(),
                     ).unwrap();
                     let reward = block_reward(N::STARTING_SUPPLY, N::BLOCK_TIME, coinbase_reward, 0);
                     info!("coinbase_reward {coinbase_reward}, reward {reward}");
                     loop {
                         // For every iteration, we increase the bound amount by `BASE_AMOUNT`.
                         bound_amount += BASE_AMOUNT;
                         let mut next_stakers: IndexMap<Address<N>, (Address<N>, u64)> = IndexMap::new();
                         let mut next_members = IndexMap::new();
                         current_committee.members().into_iter().for_each(|(address, (amount, _))| {
                             let next_amount = {
                                 if *address == self_.address() {
                                     *amount + bound_amount
                                 } else {
                                     *amount
                                 }
                             };
                             next_stakers.insert(*address, (*address, next_amount));
                             next_members.insert(*address, (next_amount, true));
                         });

                         let next_committee = Committee::new(next_round, next_members).unwrap();
                         ensure_stakers_matches(&next_committee, &next_stakers).unwrap();

                         let next_rewarded_stakers = staking_rewards(&next_stakers, &next_committee, reward);
                         let next_rewarded_committee = to_next_committee(&next_committee, next_round, &next_rewarded_stakers).unwrap();

                         // Check if I am the leader for the next committee in the `next_round + 2` round.
                         let next_next_round = next_round + 2;
                         if next_rewarded_committee.get_leader(next_round + 2).unwrap() == self_.address() {
                             // Found the parameters.
                             info!("I will be the leader for the next committee with new bound amount {bound_amount} at next_next_round {next_next_round} next_rewarded_committee {next_rewarded_committee}");
                             break;
                         }
                     }
                 }

                 // Here, we just send a transaction to bond the amount to myself.
                 let to_address = Literal::Address(self_.address());
                 let inputs = [Value::from(to_address), Value::from(Literal::U64(U64::new(bound_amount)))];
                 // Execute the transaction.
                 let transaction = match self_.ledger.vm().execute(
                     self_.private_key(),
                     locator_bond,
                     inputs.into_iter(),
                     None,
                     0, // set the fee to 0 here to avoid the effect of transaction fee reward
                     None,
                     &mut rand::thread_rng(),
                 ) {
                     Ok(transaction) => transaction,
                     Err(error) => {
                         error!("Transaction pool encountered an execution error - {error}");
                         continue;
                     }
                 };
                 let fee = transaction.fee_amount().unwrap();
                 info!("bond_public transaction fee {fee}");
                 // Broadcast the transaction.
                 if self_
                     .unconfirmed_transaction(
                         self_.router.local_ip(),
                         UnconfirmedTransaction::from(transaction.clone()),
                         transaction.clone(),
                     )
                     .await
                 {
                     info!("Transaction pool broadcasted the transaction");
                 }
             }
         });

        // Periodically send transaction just to avoid network stuck
        let self_ = self.clone();
        let locator_transfer = (ProgramID::from_str("credits.aleo")?, Identifier::from_str("transfer_public")?);
        self.spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            info!("Starting transaction pool...");

            // Start the transaction loop.
            loop {
                tokio::time::sleep(Duration::from_millis(2000)).await;
                let latest_round = self_.ledger.latest_round();
                info!("send transaction at round {latest_round}");

                // Prepare the inputs.
                let inputs = [Value::from(Literal::Address(self_.address())), Value::from(Literal::U64(U64::new(1)))];
                // Execute the transaction.
                let transaction = match self_.ledger.vm().execute(
                    self_.private_key(),
                    locator_transfer,
                    inputs.into_iter(),
                    None,
                    // 10_000,
                    0,
                    None,
                    &mut rand::thread_rng(),
                ) {
                    Ok(transaction) => transaction,
                    Err(error) => {
                        error!("Transaction pool encountered an execution error - {error}");
                        continue;
                    }
                };
                // Broadcast the transaction.
                if self_
                    .unconfirmed_transaction(
                        self_.router.local_ip(),
                        UnconfirmedTransaction::from(transaction.clone()),
                        transaction.clone(),
                    )
                    .await
                {
                    info!("Transaction pool broadcasted the transaction");
                }
            }
        });
        Ok(())
    }

    /// Spawns a task with the given future; it should only be used for long-running tasks.
    pub fn spawn<T: Future<Output = ()> + Send + 'static>(&self, future: T) {
        self.handles.lock().push(tokio::spawn(future));
    }
}

#[async_trait]
impl<N: Network, C: ConsensusStorage<N>> NodeInterface<N> for Validator<N, C> {
    /// Shuts down the node.
    async fn shut_down(&self) {
        info!("Shutting down...");

        // Shut down the node.
        trace!("Shutting down the node...");
        self.shutdown.store(true, std::sync::atomic::Ordering::Relaxed);

        // Abort the tasks.
        trace!("Shutting down the validator...");
        self.handles.lock().iter().for_each(|handle| handle.abort());

        // Shut down the router.
        self.router.shut_down().await;

        // Shut down consensus.
        trace!("Shutting down consensus...");
        self.consensus.shut_down().await;

        info!("Node has shut down.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snarkvm::prelude::{
        store::{helpers::memory::ConsensusMemory, ConsensusStore},
        Testnet3,
        VM,
    };

    use anyhow::bail;
    use rand::SeedableRng;
    use rand_chacha::ChaChaRng;
    use std::str::FromStr;

    type CurrentNetwork = Testnet3;

    /// Use `RUST_MIN_STACK=67108864 cargo test --release profiler --features timer` to run this test.
    #[ignore]
    #[tokio::test]
    async fn test_profiler() -> Result<()> {
        // Specify the node attributes.
        let node = SocketAddr::from_str("0.0.0.0:4133").unwrap();
        let rest = SocketAddr::from_str("0.0.0.0:3033").unwrap();
        let dev = Some(0);

        // Initialize an (insecure) fixed RNG.
        let mut rng = ChaChaRng::seed_from_u64(1234567890u64);
        // Initialize the account.
        let account = Account::<CurrentNetwork>::new(&mut rng).unwrap();
        // Initialize a new VM.
        let vm = VM::from(ConsensusStore::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::open(None)?)?;
        // Initialize the genesis block.
        let genesis = vm.genesis_beacon(account.private_key(), &mut rng)?;

        println!("Initializing validator node...");

        let validator = Validator::<CurrentNetwork, ConsensusMemory<CurrentNetwork>>::new(
            node,
            None,
            Some(rest),
            10,
            account,
            &[],
            &[],
            genesis,
            None,
            dev,
        )
        .await
        .unwrap();

        println!("Loaded validator node with {} blocks", validator.ledger.latest_height(),);

        bail!("\n\nRemember to #[ignore] this test!\n\n")
    }
}
