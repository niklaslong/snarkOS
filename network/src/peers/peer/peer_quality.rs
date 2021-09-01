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

use snarkos_metrics as metrics;

use std::time::Instant;

use chrono::{DateTime, Utc};

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerQuality {
    /// The timestamp of the first connection with the peer.
    pub first_seen: Option<DateTime<Utc>>,
    /// The timestmap of the last change in connection state or message received from the peer.
    pub last_seen: Option<DateTime<Utc>>,
    /// The timestamp of the last connection with the peer.
    pub last_connected: Option<DateTime<Utc>>,
    /// The timestamp of the last disconnect with the peer.
    pub last_disconnected: Option<DateTime<Utc>>,
    /// The number of times we have connected to the peer.
    pub connected_count: u64,
    /// The number of times we have disconnected from the peer.
    pub disconnected_count: u64,

    /// The time it took to send a `Ping` to the peer and for it to respond with a `Pong`.
    pub rtt_ms: u64,
    /// The timestamp of the last sent `Ping` to the peer.
    #[serde(skip)]
    pub last_ping_sent: Option<Instant>,
    /// Set to `true` if the node has sent a `Ping` to the peer and hasn't yet received a `Pong` in
    /// response.
    #[serde(skip)]
    pub expecting_pong: bool,

    /// The number of failures associated with the peer; grounds for dismissal.
    pub failures: Vec<DateTime<Utc>>,
    /// The number of messages received from the peer.
    pub num_messages_received: u64,
}

impl PeerQuality {
    pub fn is_inactive(&self, now: DateTime<Utc>) -> bool {
        let last_seen = self.last_seen;
        if let Some(last_seen) = last_seen {
            now - last_seen > chrono::Duration::seconds(crate::MAX_PEER_INACTIVITY_SECS.into())
        } else {
            // in the peer book, but never been connected to before
            false
        }
    }

    pub fn register_seen(&mut self) {
        let now = chrono::Utc::now();
        if self.first_seen.is_none() {
            self.first_seen = Some(now);
        }
        self.last_seen = Some(now);
    }

    pub fn register_connected(&mut self) {
        self.register_seen();
        self.last_connected = Some(chrono::Utc::now());
        self.connected_count += 1;
    }

    pub fn register_disconnected(&mut self) {
        let disconnect_timestamp = chrono::Utc::now();

        self.register_seen();
        self.last_disconnected = Some(disconnect_timestamp);
        self.disconnected_count += 1;
        self.expecting_pong = false;

        if let Some(last_connected) = self.last_connected {
            if let Ok(elapsed) = disconnect_timestamp.signed_duration_since(last_connected).to_std() {
                metrics::histogram!(metrics::connections::DURATION, elapsed);
            }
        }
    }
}
