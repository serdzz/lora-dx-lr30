//! GPIO relay-control and digital-sensor wire protocol, addressed by
//! [`crate::device_id::DEVICE_ID`] — same addressing model as `ota.rs`
//! (unicast target/device, `BROADCAST` reserved for queries that don't
//! know the answering device's id up front).
//!
//! Separate `MAGIC` from both `protocol.rs` (ping/pong, fixed 12 bytes)
//! and `ota.rs` (0x5A) so a receiver can tell all three packet families
//! apart from byte 0 alone, same dispatch pattern `repeater.rs` already
//! uses for protocol vs ota.
//!
//! # Packet kinds
//! - `SetRelay { target, relay_index, state }` — unicast, controller ->
//!   node. "Set relay `relay_index` (0 or 1) to `state`." Sent by node_a
//!   on a host command (see `node_a.rs`'s `run_io_bridge`).
//! - `RelayAck { target, relay_index, state, ok }` — unicast reply,
//!   node -> controller. `ok=false` means the relay_index was out of
//!   range for this device; `state` always echoes the *actual* resulting
//!   output level so the controller can tell a stale/duplicate ack from
//!   a fresh one.
//! - `SensorQuery { target }` — unicast, controller -> node. "Report your
//!   current digital sensor states."
//! - `SensorReport { target, digital_mask }` — unicast reply, node ->
//!   controller. Bit 0 = sensor 1, bit 1 = sensor 2 (1 = line high).
//!   `target` here is the *replying* device's own id (mirrors how
//!   `ota::Packet::Accept`/`ChunkAck` echo the responder's identity), not
//!   a third party's.
//!
//! No `seq` field (unlike `protocol::Packet`) — these are low-rate,
//! on-demand request/response exchanges, not a continuous measurement
//! stream, so there's no round to desync from. A repeater forwarding
//! these dedupes on raw payload bytes instead of (kind, seq) — see
//! `repeater.rs`'s `IO_SEEN_LEN` ring.

use crate::device_id::BROADCAST;

pub const MAGIC: u8 = 0x3C;
pub const VERSION: u8 = 1;

/// Max encoded size across all variants. Matches `RelayAck`'s layout:
/// 3 (header) + 1 (target) + 1 (relay_index) + 1 (state) + 1 (ok) = 7.
/// `SensorReport` is smaller (6); sized to the larger of the two so
/// callers can share one buffer.
pub const MAX_PACKET_LEN: usize = 7;

const HEADER_LEN: usize = 3; // MAGIC, VERSION, Kind

#[derive(Copy, Clone, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
    SetRelay = 0,
    RelayAck = 1,
    SensorQuery = 2,
    SensorReport = 3,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Kind::SetRelay),
            1 => Some(Kind::RelayAck),
            2 => Some(Kind::SensorQuery),
            3 => Some(Kind::SensorReport),
            _ => None,
        }
    }
}

#[derive(Copy, Clone)]
pub enum Packet {
    SetRelay { target: u8, relay_index: u8, state: bool },
    RelayAck { target: u8, relay_index: u8, state: bool, ok: bool },
    SensorQuery { target: u8 },
    SensorReport { target: u8, digital_mask: u8 },
}

impl Packet {
    /// Device this packet concerns — the caller's filter is "equals my
    /// own `DEVICE_ID`, or (`SensorQuery`/`SetRelay` only) `BROADCAST`
    /// if this deployment ever needs to address every node at once.
    /// Currently all senders address a specific `DEVICE_ID`, never
    /// `BROADCAST` — the constant is exposed for forward-compatibility,
    /// same reasoning as `ota::Packet::addressed_to`.
    pub fn addressed_to(&self) -> u8 {
        match self {
            Packet::SetRelay { target, .. }
            | Packet::RelayAck { target, .. }
            | Packet::SensorQuery { target }
            | Packet::SensorReport { target, .. } => *target,
        }
    }

    #[allow(dead_code)]
    pub const BROADCAST_ADDR: u8 = BROADCAST;

    /// Encodes into `buf`, returns the number of bytes written. `buf`
    /// must be at least [`MAX_PACKET_LEN`] long.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        buf[0] = MAGIC;
        buf[1] = VERSION;
        match self {
            Packet::SetRelay { target, relay_index, state } => {
                buf[2] = Kind::SetRelay as u8;
                buf[3] = *target;
                buf[4] = *relay_index;
                buf[5] = *state as u8;
                6
            }
            Packet::RelayAck { target, relay_index, state, ok } => {
                buf[2] = Kind::RelayAck as u8;
                buf[3] = *target;
                buf[4] = *relay_index;
                buf[5] = *state as u8;
                buf[6] = *ok as u8;
                7
            }
            Packet::SensorQuery { target } => {
                buf[2] = Kind::SensorQuery as u8;
                buf[3] = *target;
                4
            }
            Packet::SensorReport { target, digital_mask } => {
                buf[2] = Kind::SensorReport as u8;
                buf[3] = *target;
                buf[4] = *digital_mask;
                5
            }
        }
    }

    /// Decodes a received LoRa payload.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN + 1 || buf[0] != MAGIC || buf[1] != VERSION {
            return None;
        }
        let kind = Kind::from_u8(buf[2])?;
        match kind {
            Kind::SetRelay => {
                if buf.len() < 6 {
                    return None;
                }
                Some(Packet::SetRelay {
                    target: buf[3],
                    relay_index: buf[4],
                    state: buf[5] != 0,
                })
            }
            Kind::RelayAck => {
                if buf.len() < 7 {
                    return None;
                }
                Some(Packet::RelayAck {
                    target: buf[3],
                    relay_index: buf[4],
                    state: buf[5] != 0,
                    ok: buf[6] != 0,
                })
            }
            Kind::SensorQuery => {
                if buf.len() < 4 {
                    return None;
                }
                Some(Packet::SensorQuery { target: buf[3] })
            }
            Kind::SensorReport => {
                if buf.len() < 5 {
                    return None;
                }
                Some(Packet::SensorReport { target: buf[3], digital_mask: buf[4] })
            }
        }
    }
}
