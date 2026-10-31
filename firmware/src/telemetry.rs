//! Standalone battery/power telemetry broadcast, independent of
//! `protocol::Packet`'s ping/pong traffic.
//!
//! # Why this exists (and why stamping `protocol::Packet::battery_mv` on
//! forwarded PONGs isn't enough)
//! `repeater.rs`/`repeater_app` already stamp their own battery reading
//! into `protocol::Packet::battery_mv` on every PONG they forward — but
//! that field holds exactly ONE `u16`, and every repeater further down
//! the chain OVERWRITES it with its own reading before re-forwarding.
//! In a chain `node_b_app -> repeater(id=2) -> repeater(id=1) -> node_a`,
//! node_a only ever sees repeater id=1's battery; id=2's reading is
//! clobbered in transit and never reaches node_a. Extending the fixed
//! 12-byte packet with a per-hop array would need either a growing
//! packet (breaks `MAX_LORA_PAYLOAD`/airtime budget as the chain grows)
//! or a hard cap on chain length baked into the wire format — neither is
//! worth it when a separate, infrequent broadcast does the job cleanly.
//!
//! # Wire format
//! A fixed 5-byte packet, broadcast (no addressing/ack — telemetry is
//! fire-and-forget, a dropped reading just means node_a is slightly
//! stale on that one repeater's battery until the next broadcast, not a
//! retry-worthy failure): `[MAGIC][VERSION][device_id][battery_mv: u16
//! LE]`. Distinct `MAGIC` from both `protocol::MAGIC` (0xA5, ping/pong)
//! and `ota::MAGIC` (0x5A, firmware updates) so a receiver tells all
//! three packet kinds apart by their first byte alone, same pattern as
//! `ota.rs`'s doc comment describes for telling OTA apart from ping/pong.
//!
//! # Sending cadence
//! Each repeater broadcasts its own reading on a fixed interval
//! (`BROADCAST_INTERVAL`), independent of how much ping/pong traffic it's
//! forwarding — so a repeater that's currently seeing zero ping/pong
//! (e.g. node_a/node_b are between SF-sweep rounds, or out of range
//! temporarily) still gets its battery heard by node_a, which a
//! "piggyback on PONG forwarding" scheme could not guarantee.

use crate::device_id::BROADCAST;

pub const MAGIC: u8 = 0x7E;
pub const VERSION: u8 = 1;
pub const PACKET_LEN: usize = 5;

/// How often each repeater broadcasts its own `{device_id, battery_mv}` —
/// see the module doc comment's "Sending cadence". 30s balances airtime
/// (telemetry competes for the same channel as ping/pong forwarding) vs.
/// freshness (a power bank's voltage drifts on the order of minutes, not
/// seconds, so sub-minute cadence buys nothing).
pub const BROADCAST_INTERVAL_SECS: u64 = 30;

#[derive(Copy, Clone)]
pub struct Packet {
    pub device_id: u8,
    pub battery_mv: u16,
}

impl Packet {
    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let mut b = [0u8; PACKET_LEN];
        b[0] = MAGIC;
        b[1] = VERSION;
        b[2] = self.device_id;
        b[3..5].copy_from_slice(&self.battery_mv.to_le_bytes());
        b
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < PACKET_LEN || buf[0] != MAGIC || buf[1] != VERSION {
            return None;
        }
        let device_id = buf[2];
        if device_id == BROADCAST {
            // A repeater should never broadcast under the reserved
            // broadcast address — treat it as a corrupted/foreign packet
            // rather than a real reading from "device 255".
            return None;
        }
        Some(Packet { device_id, battery_mv: u16::from_le_bytes([buf[3], buf[4]]) })
    }
}
