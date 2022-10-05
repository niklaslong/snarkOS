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

use std::{collections::HashMap, net::SocketAddr, ops::Deref, sync::Arc};

use kadmium::tcp::SyncTcpRouter;
use parking_lot::RwLock;

use crate::{
    new_network::core::{
        codec::NoiseState,
        connections::ConnectionSide,
        network::{ConnectionMeta, Network},
        P2P,
    },
    Ledger,
};

type CurrentNetwork = snarkvm::prelude::Testnet3;

/// The central object responsible for handling connections.
#[derive(Clone)]
pub struct Node {
    pub network: Arc<Network>,
    ledger: Arc<Ledger<CurrentNetwork>>,

    // TODO: consolidate into network.
    pub router: SyncTcpRouter,
    pub connection_meta: Arc<RwLock<HashMap<SocketAddr, ConnectionMeta>>>,
}

// TODO: should probably be removed but current vendored code depends on this.
impl Deref for Node {
    type Target = Arc<Network>;

    fn deref(&self) -> &Self::Target {
        &self.network
    }
}

impl P2P for Node {
    fn node(&self) -> &Node {
        self
    }
}

impl Node {
    pub fn router(&self) -> &SyncTcpRouter {
        &self.router
    }

    pub fn noise_state(&self, addr: SocketAddr) -> Option<NoiseState> {
        self.connection_meta.read().get(&addr).map(|meta| meta.noise_state.clone())
    }

    pub fn insert_meta(&self, addr: SocketAddr, side: ConnectionSide, noise_state: NoiseState) {
        self.connection_meta.write().insert(addr, ConnectionMeta::new(side, noise_state));
    }

    pub fn remove_meta(&self, addr: SocketAddr) {
        self.connection_meta.write().remove(&addr);
    }
}
