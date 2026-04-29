//! Bounded, sequenced replay buffer for outbound JSON-RPC frames.
//!
//! Every frame produced by the bridge is `push`ed here under a strictly
//! increasing `seq`. When a client reattaches with a `last_seen` cursor we
//! replay the slice `(last_seen, current_seq]`. If the cursor predates the
//! ring's floor (because eviction has already happened) we surface
//! [`ReplayError::Drift`] and the client is expected to fall back to
//! authoritative state (e.g. `thread/read`).

use std::collections::VecDeque;

use serde_json::Value;
use thiserror::Error;

/// A single frame stored in the ring with its assigned sequence number and
/// cached serialized byte length (used for the byte-cap eviction policy).
///
/// Frames are stored as raw `serde_json::Value` so the session layer is
/// transport-only — bridges keep their own typed envelopes (codex-proto,
/// bridge-core::envelope) and only serialize at the boundary.
#[derive(Debug, Clone)]
pub struct Sequenced {
    pub seq: u64,
    pub payload: Value,
    pub bytes: usize,
}

#[derive(Debug, Error)]
pub enum ReplayError {
    /// The client's `last_seen` cursor is below the ring's `floor_seq` — the
    /// frames it needs have already been evicted. The client must reload state
    /// authoritatively before reattaching.
    #[error("client cursor {last_seen} predates ring floor {floor_seq}")]
    Drift {
        last_seen: u64,
        floor_seq: u64,
        current_seq: u64,
    },
}

/// Bounded sequenced replay buffer.
///
/// Two limits apply: maximum message count and maximum total serialized bytes.
/// Either limit triggers front-eviction, which advances `floor_seq` past the
/// dropped frames.
#[derive(Debug)]
pub struct ReplayRing {
    buf: VecDeque<Sequenced>,
    max_msgs: usize,
    max_bytes: usize,
    bytes: usize,
    /// Lowest `seq` currently resident, or `next_seq` if the buffer is empty.
    floor_seq: u64,
    /// The next seq to assign on `push`.
    next_seq: u64,
}

impl ReplayRing {
    pub fn new(max_msgs: usize, max_bytes: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            max_msgs: max_msgs.max(1),
            max_bytes: max_bytes.max(1),
            bytes: 0,
            floor_seq: 1,
            next_seq: 1,
        }
    }

    /// The seq of the most-recently-pushed frame, or 0 if the ring is empty
    /// and nothing has ever been pushed.
    pub fn current_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// The seq the *next* `push` will assign. Used by `Session::enqueue` to
    /// stamp the payload with `_alleycat_seq` before it lands in the ring.
    pub fn next_seq_peek(&self) -> u64 {
        self.next_seq
    }

    /// The smallest seq still resident; equal to `next_seq` when empty.
    pub fn floor_seq(&self) -> u64 {
        self.floor_seq
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Append a frame. Returns the assigned seq. Evicts from the front until
    /// both `max_msgs` and `max_bytes` are satisfied; eviction bumps
    /// `floor_seq`.
    pub fn push(&mut self, payload: Value) -> u64 {
        let bytes = serde_json::to_vec(&payload).map(|v| v.len()).unwrap_or(0);
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.buf.push_back(Sequenced {
            seq,
            payload,
            bytes,
        });
        self.bytes = self.bytes.saturating_add(bytes);
        self.evict();
        seq
    }

    fn evict(&mut self) {
        while self.buf.len() > self.max_msgs || self.bytes > self.max_bytes {
            let Some(front) = self.buf.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(front.bytes);
            self.floor_seq = front.seq.saturating_add(1);
        }
        if self.buf.is_empty() {
            self.floor_seq = self.next_seq;
        }
    }

    /// Replay frames with `seq > last_seen`. Returns an empty vec when the
    /// client is caught up. Returns [`ReplayError::Drift`] when the client's
    /// cursor predates the ring floor.
    pub fn replay_from(&self, last_seen: u64) -> Result<Vec<Sequenced>, ReplayError> {
        let needed = last_seen.saturating_add(1);
        if needed >= self.next_seq {
            return Ok(Vec::new());
        }
        if needed < self.floor_seq {
            return Err(ReplayError::Drift {
                last_seen,
                floor_seq: self.floor_seq,
                current_seq: self.current_seq(),
            });
        }
        Ok(self
            .buf
            .iter()
            .filter(|f| f.seq > last_seen)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notif(method: &str, payload_size_hint: usize) -> Value {
        let filler = "x".repeat(payload_size_hint);
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": { "filler": filler },
        })
    }

    #[test]
    fn push_assigns_increasing_seqs_starting_at_one() {
        let mut ring = ReplayRing::new(8, 1024);
        assert_eq!(ring.current_seq(), 0);
        assert_eq!(ring.floor_seq(), 1);
        assert_eq!(ring.push(notif("a", 1)), 1);
        assert_eq!(ring.push(notif("b", 1)), 2);
        assert_eq!(ring.push(notif("c", 1)), 3);
        assert_eq!(ring.current_seq(), 3);
        assert_eq!(ring.floor_seq(), 1);
    }

    #[test]
    fn evicts_by_msg_count_and_advances_floor() {
        let mut ring = ReplayRing::new(2, 1 << 20);
        ring.push(notif("a", 1));
        ring.push(notif("b", 1));
        ring.push(notif("c", 1));
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.floor_seq(), 2);
        assert_eq!(ring.current_seq(), 3);
    }

    #[test]
    fn evicts_by_byte_cap() {
        // Each notif serializes to roughly 100+ bytes (filler size 100 plus
        // method/params overhead). Cap at ~250 bytes -> only 2 fit.
        let mut ring = ReplayRing::new(100, 250);
        ring.push(notif("alpha", 100));
        ring.push(notif("beta", 100));
        ring.push(notif("gamma", 100));
        assert!(ring.len() <= 2);
        assert!(ring.floor_seq() >= 2);
    }

    #[test]
    fn replay_from_caught_up_returns_empty() {
        let mut ring = ReplayRing::new(8, 1 << 20);
        ring.push(notif("a", 1));
        ring.push(notif("b", 1));
        let frames = ring.replay_from(2).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn replay_from_within_buffer_returns_tail() {
        let mut ring = ReplayRing::new(8, 1 << 20);
        ring.push(notif("a", 1));
        ring.push(notif("b", 1));
        ring.push(notif("c", 1));
        let frames = ring.replay_from(1).unwrap();
        assert_eq!(frames.iter().map(|f| f.seq).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn replay_from_below_floor_returns_drift() {
        let mut ring = ReplayRing::new(2, 1 << 20);
        ring.push(notif("a", 1));
        ring.push(notif("b", 1));
        ring.push(notif("c", 1));
        // floor is now 2; cursor 0 is below floor (needs seq 1 which was evicted)
        let err = ring.replay_from(0).unwrap_err();
        let ReplayError::Drift {
            last_seen,
            floor_seq,
            current_seq,
        } = err;
        assert_eq!(last_seen, 0);
        assert_eq!(floor_seq, 2);
        assert_eq!(current_seq, 3);
    }

    #[test]
    fn replay_from_exact_floor_minus_one_is_servable() {
        let mut ring = ReplayRing::new(2, 1 << 20);
        ring.push(notif("a", 1));
        ring.push(notif("b", 1));
        ring.push(notif("c", 1));
        // floor=2, current=3. Cursor=1 needs seq 2 onward, which is in buf.
        let frames = ring.replay_from(1).unwrap();
        assert_eq!(frames.iter().map(|f| f.seq).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn empty_ring_replay_at_zero_is_empty() {
        let ring = ReplayRing::new(8, 1 << 20);
        assert!(ring.replay_from(0).unwrap().is_empty());
    }

    #[test]
    fn cursor_above_current_is_empty_not_error() {
        // Server restart edge: client claims to have seen seq 100, server's
        // ring is fresh with current_seq=0. Don't error; replay nothing.
        // The client will reconcile from SessionInfo.current_seq.
        let ring = ReplayRing::new(8, 1 << 20);
        assert!(ring.replay_from(100).unwrap().is_empty());
    }
}
