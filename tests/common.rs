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

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use bytes::Bytes;
use futures_util::{sink::SinkExt, TryStreamExt};
use kadmium::{
    message::{Init, Message, Nonce},
    tcp::SyncTcpRouter,
    Id,
    ProcessData,
};
use parking_lot::RwLock;
use pea2pea::{
    protocols::{Disconnect, Handshake, Reading, Writing},
    Config,
    Connection,
    ConnectionSide,
    Node,
    Pea2Pea,
};
use snarkos::new_network::core::codec::{MessageOrBytes, NoiseCodec, NoiseState};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_util::codec::{Framed, FramedParts};
use tracing::*;

#[derive(Clone)]
pub struct Client {
    node: Node,
    router: SyncTcpRouter,

    pub sent_message_counter: Arc<AtomicU64>,
    pub sent_messages: Arc<RwLock<HashMap<Nonce, Message>>>,

    pub received_message_counter: Arc<AtomicU64>,
    pub received_messages: Arc<RwLock<HashMap<Nonce, Message>>>,

    noise_states: Arc<RwLock<HashMap<SocketAddr, NoiseState>>>,
}

impl Client {
    pub async fn new() -> Self {
        let client = Self {
            node: Node::new(Config {
                listener_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                max_connections: 200,
                ..Default::default()
            })
            .await
            .unwrap(),

            router: SyncTcpRouter::new(Id::rand(), 100, 25),
            sent_message_counter: Arc::new(AtomicU64::new(0)),
            sent_messages: Arc::new(RwLock::new(HashMap::new())),
            received_message_counter: Arc::new(AtomicU64::new(0)),
            received_messages: Arc::new(RwLock::new(HashMap::new())),

            noise_states: Arc::new(RwLock::new(HashMap::new())),
        };

        client.enable_handshake().await;
        // client.enable_reading().await;
        // client.enable_writing().await;
        client.enable_disconnect().await;

        client
    }

    pub fn router(&self) -> &SyncTcpRouter {
        &self.router
    }

    pub fn sent_message_count(&self) -> u64 {
        self.sent_message_counter.load(Ordering::Relaxed)
    }

    pub fn received_message_count(&self) -> u64 {
        self.received_message_counter.load(Ordering::Relaxed)
    }

    pub fn register_sent_message(&self, message: Message) {
        self.sent_message_counter.fetch_add(1, Ordering::SeqCst);
        self.sent_messages.write().insert(message.nonce(), message);
    }

    pub fn register_received_message(&self, message: Message) {
        self.received_message_counter.fetch_add(1, Ordering::SeqCst);
        self.received_messages.write().insert(message.nonce(), message);
    }
}

impl Pea2Pea for Client {
    fn node(&self) -> &Node {
        &self.node
    }
}

#[async_trait::async_trait]
impl Handshake for Client {
    async fn perform_handshake(&self, mut conn: Connection) -> io::Result<Connection> {
        let noise_builder = snow::Builder::new("Noise_XX_25519_ChaChaPoly_BLAKE2s".parse().unwrap());
        let noise_keypair = noise_builder.generate_keypair().unwrap();
        let noise_builder = noise_builder.local_private_key(&noise_keypair.private);

        let local_id = self.router().local_id();
        let local_listening_port = self.node().listening_addr().unwrap().port();

        let peer_side = conn.side();
        let stream = self.borrow_stream(&mut conn);

        let (noise_state, _payload) = handshake_xx(stream, peer_side, noise_builder, MessageOrBytes::Bytes(Bytes::new())).await?;

        let (noise_state, peer_id, peer_addr, _conn_addr) =
            handshake_kadmium(stream, noise_state, local_id, local_listening_port, peer_side, self.node().span()).await?;

        self.router().insert(peer_id, peer_addr);
        self.router().set_connected(peer_id, peer_addr);

        // Save the noise state to be reused by Reading and Writing.
        self.noise_states.write().insert(conn.addr(), noise_state);

        Ok(conn)
    }
}

// #[async_trait::async_trait]
// impl Reading for Client {
//     type Codec = NoiseCodec;
//     type Message = MessageOrBytes;
//
//     fn codec(&self, addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
//         let noise_state = self.noise_states.read().get(&addr).cloned().unwrap();
//         NoiseCodec::new(noise_state)
//     }
//
//     async fn process_message(&self, source: SocketAddr, message: Self::Message) -> io::Result<()> {
//         dbg!("got message");
//         if let MessageOrBytes::Message(message) = message {
//             // Track the received messages to make test assertions easier.
//             self.register_received_message(message.clone());
//
//             let _response = self.router().process_message::<Client, ClientData>(self.clone(), message, source);
//         }
//
//         Ok(())
//     }
// }
//
// impl Writing for Client {
//     type Codec = NoiseCodec;
//     type Message = MessageOrBytes;
//
//     fn codec(&self, addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
//         let noise_state = self.noise_states.read().get(&addr).cloned().unwrap();
//         NoiseCodec::new(noise_state)
//     }
// }

#[async_trait::async_trait]
impl Disconnect for Client {
    async fn handle_disconnect(&self, addr: SocketAddr) {
        self.router().set_disconnected(addr);
    }
}

// impl ProcessData<Client> for ClientData {
//     fn verify_data(&self, _state: Client) -> bool {
//         // TODO: plug into snarkVM for tx and block verification? For now assume all data is valid.
//         // TODO: consider caching data for DoS protection if it is invalid, may need source
//         // accessible here in order to do so.
//         true
//     }
//
//     fn process_data(&self, _state: Client) {
//         // Likely not needed.
//     }
// }

pub async fn handshake_xx<T>(
    inner: &mut T,
    peer_side: ConnectionSide,
    noise_builder: snow::Builder<'_>,
    payload: MessageOrBytes,
) -> io::Result<(NoiseState, MessageOrBytes)>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    match peer_side {
        // The peer initiated the connection.
        ConnectionSide::Initiator => {
            let noise_state = noise_builder.build_responder().unwrap();
            let mut framed = Framed::new(inner, NoiseCodec::new(NoiseState::Handshake(Box::new(noise_state))));

            // <- e
            framed.try_next().await?;

            // -> e, ee, s, es
            framed.send(payload).await?;

            // <- s, se
            let secure_payload = framed.try_next().await?.unwrap();

            let FramedParts { codec, .. } = framed.into_parts();
            let NoiseCodec { noise_state, .. } = codec;

            Ok((noise_state.into_post_handshake_state(), secure_payload))
        }

        // The relay initiated the connection.
        ConnectionSide::Responder => {
            let noise_state = noise_builder.build_initiator().unwrap();
            let mut framed = Framed::new(inner, NoiseCodec::new(NoiseState::Handshake(Box::new(noise_state))));

            // -> e
            framed.send(MessageOrBytes::Bytes(Bytes::new())).await?;

            // <- e, ee, s, es
            let secure_payload = framed.try_next().await?.unwrap();

            // -> s, se
            framed.send(payload).await?;

            let FramedParts { codec, .. } = framed.into_parts();
            let NoiseCodec { noise_state, .. } = codec;

            Ok((noise_state.into_post_handshake_state(), secure_payload))
        }
    }
}

pub async fn handshake_kadmium(
    stream: &mut TcpStream,
    noise_state: NoiseState,
    local_id: Id,
    local_listening_port: u16,
    peer_side: ConnectionSide,
    span: &Span,
) -> io::Result<(NoiseState, Id, SocketAddr, SocketAddr)> {
    let conn_addr = stream.peer_addr().unwrap();
    let mut peer_addr = conn_addr;

    let mut framed = Framed::new(stream, NoiseCodec::new(noise_state));

    let peer_id = match peer_side {
        // The peer initiated the connection.
        ConnectionSide::Initiator => {
            // Receive the peer's local ID.
            // TODO: Handle errors better.
            let (peer_id, peer_port) = if let MessageOrBytes::KadmiumMessage(Message::Init(init)) = framed.try_next().await?.unwrap() {
                (init.id, init.port)
            } else {
                panic!("expected peer ID")
            };

            debug!(parent: span, "kadmium handshake (1/2): received peer ID and peer port");

            // Ports will be different since connection was opened by the peer (a new stream is
            // created per connection).
            debug_assert_ne!(peer_addr.port(), peer_port);
            peer_addr.set_port(peer_port);

            // Respond with our local ID and port.
            framed
                .send(MessageOrBytes::KadmiumMessage(Message::Init(Init {
                    nonce: 0,
                    id: local_id,
                    port: local_listening_port,
                })))
                .await?;

            debug!(parent: span, "kadmium handshake (2/2): sent ID and listening port");

            peer_id
        }

        // The relay initiated the connection.
        ConnectionSide::Responder => {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            // Send our local ID and port to the peer.
            framed
                .send(MessageOrBytes::KadmiumMessage(Message::Init(Init {
                    nonce: 0,
                    id: local_id,
                    port: local_listening_port,
                })))
                .await?;

            debug!(parent: span, "kadmium handshake (1/2): sent ID and listening port");

            // Receive the peer's local ID and port.
            let (peer_id, _peer_port) = if let MessageOrBytes::KadmiumMessage(Message::Init(init)) = framed.try_next().await?.unwrap() {
                (init.id, init.port)
            } else {
                panic!("expected peer ID")
            };

            // Ports should be the same since the connection was openend with the peer's listener.
            debug_assert_eq!(peer_addr.port(), _peer_port);

            debug!(parent: span, "kadmium handshake (2/2): received peer ID and peer listening port");

            peer_id
        }
    };

    let FramedParts { codec, .. } = framed.into_parts();
    let NoiseCodec { noise_state, .. } = codec;

    Ok((noise_state, peer_id, peer_addr, conn_addr))
}
