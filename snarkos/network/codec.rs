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

use std::{io, sync::Arc};

use bytes::{Bytes, BytesMut};
use kadmium::{codec::MessageCodec, message::Message};
use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, ParallelIterator};
use snow::{HandshakeState, StatelessTransportState};
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

const MAX_MESSAGE_LEN: usize = 65535;

#[derive(Clone, Debug)]
pub enum MessageOrBytes {
    Message(Message),
    Bytes(Bytes),
}

#[derive(Clone)]
pub struct PostHandshakeState {
    state: Arc<StatelessTransportState>,
    tx_nonce: u64,
    rx_nonce: u64,
}

pub enum NoiseState {
    Handshake(Box<HandshakeState>),
    PostHandshake(PostHandshakeState),
}

impl Clone for NoiseState {
    fn clone(&self) -> Self {
        match self {
            Self::Handshake(..) => unimplemented!(),
            Self::PostHandshake(ph_state) => Self::PostHandshake(ph_state.clone()),
        }
    }
}

impl NoiseState {
    pub fn into_post_handshake_state(self) -> Self {
        if let Self::Handshake(noise_state) = self {
            let noise_state = noise_state.into_stateless_transport_mode().unwrap();
            Self::PostHandshake(PostHandshakeState {
                state: Arc::new(noise_state),
                tx_nonce: 0,
                rx_nonce: 0,
            })
        } else {
            panic!()
        }
    }
}

pub struct NoiseCodec {
    codec: LengthDelimitedCodec,
    kadmium_codec: MessageCodec,
    // snarkos_codec: SnarkOSCodec<Testnet3>,
    pub noise_state: NoiseState,
}

impl NoiseCodec {
    pub fn new(noise_state: NoiseState) -> Self {
        Self {
            codec: LengthDelimitedCodec::new(),
            kadmium_codec: MessageCodec::new(),
            noise_state,
        }
    }
}

impl Encoder<MessageOrBytes> for NoiseCodec {
    type Error = io::Error;

    fn encode(&mut self, message: MessageOrBytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let ciphertext = match (&mut self.noise_state, message) {
            (NoiseState::Handshake(ref mut noise), MessageOrBytes::Bytes(bytes)) => {
                let mut buffer = [0u8; MAX_MESSAGE_LEN];
                let len = noise.write_message(&bytes, &mut buffer).unwrap();

                buffer[..len].into()
            }

            (NoiseState::PostHandshake(ref mut noise), MessageOrBytes::Message(message)) => {
                // Encode the kadmium message using its codec.
                let mut bytes = BytesMut::new();
                self.kadmium_codec.encode(message, &mut bytes).unwrap();

                let chunked_plaintext_msg: Vec<_> = bytes.chunks(MAX_MESSAGE_LEN - 16).collect();
                let num_chunks = chunked_plaintext_msg.len() as u64;

                // Encrypt the resulting bytes with Noise.
                let encrypted_chunks: Vec<Vec<u8>> = chunked_plaintext_msg
                    .into_par_iter()
                    .enumerate()
                    .map(|(nonce_offset, plaintext_chunk)| {
                        let mut buffer = vec![0u8; MAX_MESSAGE_LEN];

                        let len = noise
                            .state
                            .write_message(noise.tx_nonce + nonce_offset as u64, plaintext_chunk, &mut buffer)
                            .unwrap();

                        buffer.truncate(len);
                        buffer
                    })
                    .collect();

                let mut buffer = BytesMut::new();
                for chunk in encrypted_chunks {
                    buffer.extend_from_slice(&chunk)
                }

                noise.tx_nonce += num_chunks;

                buffer
            }

            _ => unimplemented!(),
        };

        // Encode the resulting ciphertext using the length-delimited codec.
        self.codec.encode(ciphertext.freeze(), dst)
    }
}

impl Decoder for NoiseCodec {
    type Error = io::Error;
    type Item = MessageOrBytes;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Decode the ciphertext with the length-delimited codec.
        let bytes = if let Some(bytes) = self.codec.decode(src)? {
            bytes
        } else {
            return Ok(None);
        };

        let msg = match self.noise_state {
            NoiseState::Handshake(ref mut noise) => {
                let mut buffer = [0u8; MAX_MESSAGE_LEN];

                // Decrypt the ciphertext in handshake mode.
                let len = noise.read_message(&bytes, &mut buffer).map_err(|_| io::ErrorKind::InvalidData)?;

                Some(MessageOrBytes::Bytes(Bytes::copy_from_slice(&buffer[..len])))
            }

            NoiseState::PostHandshake(ref mut noise) => {
                let chunked_encrypted_msg: Vec<_> = bytes.chunks(MAX_MESSAGE_LEN).collect();
                let num_chunks = chunked_encrypted_msg.len() as u64;

                let decrypted_chunks: Vec<io::Result<Vec<u8>>> = chunked_encrypted_msg
                    .into_par_iter()
                    .enumerate()
                    .map(|(nonce_offset, encrypted_chunk)| {
                        let mut buffer = vec![0u8; MAX_MESSAGE_LEN];

                        // Decrypt the ciphertext in post-handshake mode.
                        let len = noise
                            .state
                            .read_message(noise.rx_nonce + nonce_offset as u64, encrypted_chunk, &mut buffer)
                            .map_err(|_| io::ErrorKind::InvalidData)?;

                        buffer.truncate(len);
                        Ok(buffer)
                    })
                    .collect();

                noise.rx_nonce += num_chunks;

                // Decode the plaintext into a kadmium message.
                let mut plaintext = BytesMut::new();
                for chunk in decrypted_chunks {
                    plaintext.extend_from_slice(&chunk?);
                }

                self.kadmium_codec.decode(&mut plaintext)?.map(MessageOrBytes::Message)
            }
        };

        Ok(msg)
    }
}
