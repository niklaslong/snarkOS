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

#[allow(dead_code)]
mod common;

use std::time::Duration;

use clap::Parser;
use common::Client;
use pea2pea::Pea2Pea;
use snarkos::{
    logger::initialize_logger,
    new_network::node::{CurrentNetwork, Node},
    Account,
    CLI,
};

#[tokio::test]
async fn handshake_responder_side() {
    let mut cli = CLI::try_parse_from(&["run"]).unwrap();
    cli.dev = Some(0);

    // initialize_logger(cli.verbosity);

    let account = Account::<CurrentNetwork>::sample().unwrap();
    let node = Node::new(&cli, account).await.unwrap();

    let client = Client::new().await;

    assert!(client.node().connect(cli.node).await.is_ok());
    assert_eq!(node.network.num_connected(), 1);
}

#[tokio::test]
async fn handshake_initiator_side() {
    let mut cli = CLI::try_parse_from(&["run"]).unwrap();
    cli.dev = Some(0);

    // initialize_logger(cli.verbosity);

    let account = Account::<CurrentNetwork>::sample().unwrap();
    let node = Node::new(&cli, account).await.unwrap();

    let client = Client::new().await;

    assert!(node.connect(client.node().listening_addr().unwrap()).await.is_ok());
    assert_eq!(client.node().num_connected(), 1);
}
