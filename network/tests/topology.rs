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

use snarkos_network::Node;
use snarkos_testing::{
    network::{
        test_environment,
        test_node,
        topology::{connect_nodes, Topology},
        TestSetup,
    },
    wait_until,
};

const N: usize = 5;

async fn test_nodes(n: usize, setup: TestSetup) -> Vec<Node> {
    let mut nodes = vec![];

    for _ in 0..n {
        let environment = test_environment(setup.clone());
        let mut node = Node::new(environment).await.unwrap();

        node.establish_address().await.unwrap();
        nodes.push(node);
    }

    nodes
}

async fn start_nodes(nodes: &Vec<Node>) {
    for node in nodes {
        node.start_services().await;
    }
}

#[tokio::test]
async fn line() {
    let setup = TestSetup {
        consensus_setup: None,
        peer_sync_interval: 2,
        ..Default::default()
    };
    let mut nodes = test_nodes(N, setup).await;
    connect_nodes(&mut nodes, Topology::Line).await;
    start_nodes(&nodes).await;

    // First and Last nodes should have 1 connected peer.
    wait_until!(
        5,
        nodes.first().unwrap().peer_book.read().number_of_connected_peers() == 1
    );
    wait_until!(
        5,
        nodes.last().unwrap().peer_book.read().number_of_connected_peers() == 1
    );

    // All other nodes should have two.
    for i in 1..(nodes.len() - 1) {
        wait_until!(5, nodes[i].peer_book.read().number_of_connected_peers() == 2);
    }
}

#[tokio::test]
#[ignore]
async fn line_degeneration() {
    let setup = TestSetup {
        consensus_setup: None,
        peer_sync_interval: 1,
        min_peers: (N / 2) as u16,
        ..Default::default()
    };
    let mut nodes = test_nodes(N, setup).await;
    connect_nodes(&mut nodes, Topology::Line).await;
    start_nodes(&nodes).await;

    let density = || {
        let connections = total_connection_count(&nodes);
        network_density(N as f64, connections as f64)
    };
    wait_until!(5, density() >= 0.5);
    assert!(degree_centrality_delta(&nodes) as f64 <= 0.3 * N as f64);
}

#[tokio::test]
async fn ring() {
    let setup = TestSetup {
        consensus_setup: None,
        peer_sync_interval: 2,
        ..Default::default()
    };
    let mut nodes = test_nodes(N, setup).await;
    connect_nodes(&mut nodes, Topology::Ring).await;
    start_nodes(&nodes).await;

    for node in &nodes {
        wait_until!(5, node.peer_book.read().number_of_connected_peers() == 2);
    }
}

#[tokio::test]
#[ignore]
async fn ring_degeneration() {
    let setup = TestSetup {
        consensus_setup: None,
        peer_sync_interval: 1,
        min_peers: (N / 2) as u16,
        ..Default::default()
    };
    let mut nodes = test_nodes(N, setup).await;
    connect_nodes(&mut nodes, Topology::Ring).await;
    start_nodes(&nodes).await;

    let density = || {
        let connections = total_connection_count(&nodes);
        network_density(N as f64, connections as f64)
    };
    wait_until!(5, density() >= 0.5);
    assert!(degree_centrality_delta(&nodes) as f64 <= 0.3 * N as f64);
}

#[tokio::test]
async fn mesh() {
    let setup = TestSetup {
        consensus_setup: None,
        peer_sync_interval: 2,
        ..Default::default()
    };
    let mut nodes = test_nodes(N, setup).await;
    connect_nodes(&mut nodes, Topology::Mesh).await;
    start_nodes(&nodes).await;

    for node in &nodes {
        wait_until!(5, node.peer_book.read().number_of_connected_peers() as usize == N - 1);
    }
}

#[tokio::test]
async fn star() {
    let setup = TestSetup {
        consensus_setup: None,
        peer_sync_interval: 2,
        ..Default::default()
    };
    let mut nodes = test_nodes(N, setup).await;
    connect_nodes(&mut nodes, Topology::Star).await;
    start_nodes(&nodes).await;

    let hub = nodes.first().unwrap();
    wait_until!(5, hub.peer_book.read().number_of_connected_peers() as usize == N - 1);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn star_degeneration() {
    let setup = TestSetup {
        consensus_setup: None,
        peer_sync_interval: 1,
        min_peers: (N / 2) as u16,
        ..Default::default()
    };
    let mut nodes = test_nodes(N, setup).await;
    connect_nodes(&mut nodes, Topology::Star).await;
    start_nodes(&nodes).await;

    let density = || {
        let connections = total_connection_count(&nodes);
        network_density(N as f64, connections as f64)
    };
    wait_until!(5, density() >= 0.5);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn binary_star_contact() {
    let filter = tracing_subscriber::EnvFilter::from_default_env().add_directive("tokio_reactor=off".parse().unwrap());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // 1. pass N nodes through a bootstrapper
    // 2. pass K nodes through a different bootstrapper (totally separate networks)
    // 3. introduce a node that is "fed" the list of random nodes from the N and K sets
    // 4. check out the end topology

    // Setup the bootnodes for each star topology.
    let bootnode_setup = TestSetup {
        consensus_setup: None,
        peer_sync_interval: 2,
        min_peers: N as u16,
        is_bootnode: true,
        ..Default::default()
    };
    let environment_a = test_environment(bootnode_setup.clone());
    let environment_b = test_environment(bootnode_setup);
    let mut bootnode_a = Node::new(environment_a).await.unwrap();
    let mut bootnode_b = Node::new(environment_b).await.unwrap();

    bootnode_a.establish_address().await.unwrap();
    bootnode_b.establish_address().await.unwrap();

    let ba = bootnode_a.local_address().unwrap().to_string();
    let bb = bootnode_b.local_address().unwrap().to_string();

    // Create the nodes to be used as the leafs in the stars.
    let setup = TestSetup {
        consensus_setup: None,
        peer_sync_interval: 2,
        min_peers: (N / 2) as u16,
        ..Default::default()
    };
    let mut star_a_nodes = test_nodes(N - 1, setup.clone()).await;
    let mut star_b_nodes = test_nodes(N - 1, setup).await;

    // Insert the bootnodes at the begining of the node lists.
    star_a_nodes.insert(0, bootnode_a);
    star_b_nodes.insert(0, bootnode_b);

    // Create the star topologies.
    connect_nodes(&mut star_a_nodes, Topology::Star).await;
    connect_nodes(&mut star_b_nodes, Topology::Star).await;

    // Start the services.
    start_nodes(&star_a_nodes).await;
    start_nodes(&star_b_nodes).await;

    // Measure the initial density once the topologies are established. The two star topologies
    // should still be disconnected.
    let hub_a = star_a_nodes.first().unwrap();
    wait_until!(5, hub_a.peer_book.read().number_of_connected_peers() as usize == N - 1);
    let hub_b = star_b_nodes.first().unwrap();
    wait_until!(5, hub_b.peer_book.read().number_of_connected_peers() as usize == N - 1);

    // Setting up a list of nodes as we will consider them as a whole graph from this point
    // forwards.
    star_a_nodes.append(&mut star_b_nodes);
    let mut nodes = star_a_nodes;

    // Single node to connect to a subset of N and K.
    let bootnodes = vec![ba, bb];

    let solo_setup = TestSetup {
        consensus_setup: None,
        peer_sync_interval: 2,
        min_peers: N as u16,
        bootnodes,
        ..Default::default()
    };
    let solo = test_node(solo_setup).await;
    nodes.push(solo);

    let density = || {
        let connections = total_connection_count(&nodes);
        network_density(N as f64, connections as f64)
    };
    wait_until!(5, density() >= 0.5);
}

fn total_connection_count(nodes: &Vec<Node>) -> usize {
    let mut count = 0;

    for node in nodes {
        count += node.peer_book.read().number_of_connected_peers()
    }

    (count / 2).into()
}

// Topology metrics
//
// 1. node count
// 2. density
// 3. centrality measurements:
//
//      - degree centrality (covered by the number of connected peers)
//      - eigenvector centrality

fn network_density(n: f64, ac: f64) -> f64 {
    // Calculate the total number of possible connections given a node count.
    let pc = n * (n - 1.0) / 2.0;
    // Actual connections divided by the possbile connections gives the density.
    ac / pc
}

fn degree_centrality_delta(nodes: &Vec<Node>) -> u16 {
    let dc = nodes
        .iter()
        .map(|node| node.peer_book.read().number_of_connected_peers());
    let min = dc.clone().min().unwrap();
    let max = dc.max().unwrap();

    max - min
}
