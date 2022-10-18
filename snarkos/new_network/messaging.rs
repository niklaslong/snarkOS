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

use std::{io, net::SocketAddr};

use bytes::BytesMut;
use kadmium::message::Message;
use tokio_util::codec::Decoder;
use tracing::*;

use crate::{
    new_network::{
        core::{
            codec::{MessageOrBytes, NoiseCodec},
            connections::ConnectionSide,
            protocols::{Disconnect, Reading, Writing},
        },
        node::{CurrentNetwork, Node},
    },
    Message as SnarkOSMessage,
    MessageCodec as SnarkOSCodec,
};

impl Node {
    async fn process_message(&self, source: SocketAddr, message: Message) -> io::Result<()> {
        if let Message::Chunk(chunk) = message {
            // Decode the snarkOS messages.
            let mut data = BytesMut::zeroed(chunk.data.len());
            data.copy_from_slice(&chunk.data);

            let mut codec = SnarkOSCodec::<CurrentNetwork>::default();
            let message = codec.decode(&mut data)?;

            match message {
                Some(SnarkOSMessage::Ping) => {
                    info!(parent: self.span(), "got PING from {source}, sending PONG")
                }

                Some(SnarkOSMessage::Pong(pong_data)) => {
                    info!(parent: self.span(), "got PONG from {source}")
                }

                _ => {
                    // TODO
                }
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl Reading for Node {
    type Codec = NoiseCodec;

    fn codec(&self, addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
        let noise_state = self.noise_state(addr).unwrap();
        NoiseCodec::new(noise_state)
    }

    async fn process_message(&self, source: SocketAddr, message: MessageOrBytes) -> io::Result<()> {
        let message = match message {
            MessageOrBytes::Message(message) => message,
            // Ignore plain bytes after the handshake.
            MessageOrBytes::Bytes(_) => return Ok(()),
        };

        info!(parent: self.span(), "processing {:?} from {}", message.variant_as_str(), source);

        self.process_message(source, message).await
    }
}

impl Writing for Node {
    type Codec = NoiseCodec;

    fn codec(&self, addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
        let noise_state = self.noise_state(addr).unwrap();
        NoiseCodec::new(noise_state)
    }
}

#[async_trait::async_trait]
impl Disconnect for Node {
    async fn handle_disconnect(&self, addr: SocketAddr) {
        self.router().set_disconnected(addr);
        self.remove_meta(addr);
    }
}
