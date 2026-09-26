//! Announced byte transfers (BRG-06).
//!
//! `transferStart{transferId, name, mediaType, byteLength, sha256}` is followed
//! by chunks whose plaintext is `u32le transferId ‖ u32le seq ‖ payload`. The
//! receiver checks the sequence, length and SHA-256 before handing the bytes
//! over, holds at most [`MAX_IN_FLIGHT`] transfers, never accepts a reused id,
//! and fails a transfer after [`NO_PROGRESS_TIMEOUT`] without progress.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use sha2_011::{Digest, Sha256};

use super::protocol::{TransferStart, JSON_PAYLOAD_MEDIA_TYPES, MAX_JSON_PAYLOAD_BYTES, MAX_TRANSFER_BYTES};
use super::redact::Redacted;

pub const MAX_CHUNK_PAYLOAD: usize = 1024 * 1024;
pub const MAX_IN_FLIGHT: usize = 3;
pub const NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);
const HEADER_LEN: usize = 8;

/// Why a transfer failed. The session closes with `transferFailed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferError {
    TooLarge,
    TooManyInFlight,
    ReusedId,
    UnknownTransfer,
    BadChunk,
    OutOfSequence,
    Overrun,
    ChecksumMismatch,
    Stalled,
}

/// A transfer whose bytes arrived complete and verified.
#[derive(Clone)]
pub struct Completed {
    pub transfer_id: u32,
    pub name: Redacted<String>,
    pub media_type: String,
    pub bytes: Redacted<Vec<u8>>,
}

impl std::fmt::Debug for Completed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completed")
            .field("transfer_id", &self.transfer_id)
            .field("media_type", &self.media_type)
            .field("len", &self.bytes.expose().len())
            .finish()
    }
}

struct Partial {
    start: TransferStart,
    bytes: Vec<u8>,
    next_seq: u32,
    last_progress: Instant,
}

/// Receives the transfers of one direction of one session.
#[derive(Default)]
pub struct Incoming {
    active: HashMap<u32, Partial>,
    seen: HashSet<u32>,
}

impl std::fmt::Debug for Incoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Incoming").field("in_flight", &self.active.len()).finish()
    }
}

fn finish(partial: Partial) -> Result<Completed, TransferError> {
    if Sha256::digest(&partial.bytes).as_slice() != partial.start.sha256 {
        return Err(TransferError::ChecksumMismatch);
    }
    Ok(Completed {
        transfer_id: partial.start.transfer_id,
        name: partial.start.name,
        media_type: partial.start.media_type,
        bytes: partial.bytes.into(),
    })
}

impl Incoming {
    /// Announce a transfer. A zero-length transfer completes at once.
    pub fn start(&mut self, start: TransferStart, now: Instant) -> Result<Option<Completed>, TransferError> {
        let limit = if JSON_PAYLOAD_MEDIA_TYPES.contains(&start.media_type.as_str()) {
            MAX_JSON_PAYLOAD_BYTES
        } else {
            MAX_TRANSFER_BYTES
        };
        if start.byte_length > limit {
            return Err(TransferError::TooLarge);
        }
        if !self.seen.insert(start.transfer_id) {
            return Err(TransferError::ReusedId);
        }
        if self.active.len() >= MAX_IN_FLIGHT {
            return Err(TransferError::TooManyInFlight);
        }
        let partial = Partial {
            bytes: Vec::with_capacity(start.byte_length.min(MAX_CHUNK_PAYLOAD as u64) as usize),
            start,
            next_seq: 0,
            last_progress: now,
        };
        if partial.start.byte_length == 0 {
            return finish(partial).map(Some);
        }
        self.active.insert(partial.start.transfer_id, partial);
        Ok(None)
    }

    /// Accept one chunk plaintext.
    pub fn chunk(&mut self, plaintext: &[u8], now: Instant) -> Result<Option<Completed>, TransferError> {
        if plaintext.len() <= HEADER_LEN || plaintext.len() > HEADER_LEN + MAX_CHUNK_PAYLOAD {
            return Err(TransferError::BadChunk);
        }
        let transfer_id = u32::from_le_bytes(plaintext[0..4].try_into().expect("4 bytes"));
        let seq = u32::from_le_bytes(plaintext[4..8].try_into().expect("4 bytes"));
        let payload = &plaintext[HEADER_LEN..];
        let partial = self.active.get_mut(&transfer_id).ok_or(TransferError::UnknownTransfer)?;
        if seq != partial.next_seq {
            return Err(TransferError::OutOfSequence);
        }
        if partial.bytes.len() as u64 + payload.len() as u64 > partial.start.byte_length {
            return Err(TransferError::Overrun);
        }
        partial.bytes.extend_from_slice(payload);
        partial.next_seq += 1;
        partial.last_progress = now;
        if partial.bytes.len() as u64 == partial.start.byte_length {
            let partial = self.active.remove(&transfer_id).expect("active transfer");
            return finish(partial).map(Some);
        }
        Ok(None)
    }

    /// `Err(Stalled)` when an active transfer made no progress for 30 s.
    pub fn check_progress(&self, now: Instant) -> Result<(), TransferError> {
        let stalled = self
            .active
            .values()
            .any(|partial| now.saturating_duration_since(partial.last_progress) > NO_PROGRESS_TIMEOUT);
        if stalled {
            Err(TransferError::Stalled)
        } else {
            Ok(())
        }
    }

    /// Drop every partial buffer (on failure or close).
    pub fn dispose(&mut self) {
        self.active.clear();
    }
}

/// The chunk plaintexts that carry `bytes` as transfer `transfer_id`.
pub fn chunks(transfer_id: u32, bytes: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    bytes.chunks(MAX_CHUNK_PAYLOAD).enumerate().map(move |(seq, payload)| {
        let mut chunk = Vec::with_capacity(HEADER_LEN + payload.len());
        chunk.extend_from_slice(&transfer_id.to_le_bytes());
        chunk.extend_from_slice(&(seq as u32).to_le_bytes());
        chunk.extend_from_slice(payload);
        chunk
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::{protocol, vectors};
    use serde_json::Value;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    /// Replay one event sequence from transfers.json; `Err` at the first
    /// failure, otherwise the completed transfers in order.
    fn replay(events: &[Value]) -> Result<Vec<Completed>, String> {
        let start = Instant::now();
        let mut now = start;
        let mut incoming = Incoming::default();
        let mut done = Vec::new();
        for event in events {
            let result = match event["event"].as_str().unwrap() {
                "transferStart" => {
                    let message = &event["message"];
                    // The schema layer refuses what it can before the receiver sees it.
                    if let Err(error) = protocol::validate_message(protocol::Direction::WebToDesktop, message) {
                        return Err(error.to_string());
                    }
                    let protocol::Inbound::TransferStart(start) = protocol::parse_inbound(&message.to_string()).unwrap() else {
                        unreachable!()
                    };
                    incoming.start(start, now)
                }
                "chunk" => {
                    let payload = match event.get("payloadHex") {
                        Some(hex_text) => hex(hex_text.as_str().unwrap()),
                        None => vec![event["payloadFill"].as_u64().unwrap() as u8; event["payloadLength"].as_u64().unwrap() as usize],
                    };
                    let mut plaintext = (event["transferId"].as_u64().unwrap() as u32).to_le_bytes().to_vec();
                    plaintext.extend_from_slice(&(event["seq"].as_u64().unwrap() as u32).to_le_bytes());
                    plaintext.extend_from_slice(&payload);
                    incoming.chunk(&plaintext, now)
                }
                "advanceMs" => {
                    now += Duration::from_millis(event["ms"].as_u64().unwrap());
                    incoming.check_progress(now).map(|()| None)
                }
                other => panic!("unknown event {other}"),
            };
            match result {
                Ok(Some(completed)) => done.push(completed),
                Ok(None) => {}
                Err(error) => return Err(format!("{error:?}")),
            }
        }
        Ok(done)
    }

    #[test]
    fn transfer_vectors() {
        let transfers = vectors::json("transfers.json");
        for case in transfers["valid"].as_array().unwrap() {
            let done = replay(case["events"].as_array().unwrap()).unwrap_or_else(|e| panic!("{}: {e}", case["name"]));
            let ids: Vec<u64> = done.iter().map(|c| c.transfer_id as u64).collect();
            let expected: Vec<u64> = case["expect"]["complete"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
            assert_eq!(ids, expected, "{}", case["name"]);
            if let Some(bytes) = case["expect"]["bytesHex"].as_str() {
                assert_eq!(done[0].bytes.expose(), &hex(bytes), "{}", case["name"]);
            }
        }
        for case in transfers["invalid"].as_array().unwrap() {
            assert!(replay(case["events"].as_array().unwrap()).is_err(), "{}", case["name"]);
        }
    }

    #[test]
    fn outgoing_chunks_reassemble() {
        let bytes: Vec<u8> = (0..(MAX_CHUNK_PAYLOAD * 2 + 5)).map(|i| (i % 251) as u8).collect();
        let message = protocol::transfer_start("t1", 3, "synthetic.pdf", "application/pdf", &bytes);
        let protocol::Inbound::TransferStart(start) = protocol::parse_inbound(&message.to_string()).unwrap() else {
            unreachable!()
        };
        let now = Instant::now();
        let mut incoming = Incoming::default();
        assert!(incoming.start(start, now).unwrap().is_none());
        let pieces: Vec<Vec<u8>> = chunks(3, &bytes).collect();
        assert_eq!(pieces.len(), 3);
        let mut done = None;
        for piece in &pieces {
            done = incoming.chunk(piece, now).unwrap();
        }
        assert_eq!(done.unwrap().bytes.expose(), &bytes);
        assert!(!format!("{incoming:?}").contains("251"));
    }
}
