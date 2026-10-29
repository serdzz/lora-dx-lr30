//! OTA (over-the-air) firmware update wire protocol, addressed by
//! [`crate::device_id::DEVICE_ID`].
//!
//! Separate packet kind/format from `protocol.rs`'s fixed 12-byte ping/pong
//! — OTA needs a variable-length chunk payload and this repo's main packet
//! format has no room (and no reason) to carry one. Both share the channel
//! (frequency/SF/bandwidth); a receiver tells OTA and ping/pong packets
//! apart by `MAGIC` (different from `protocol::MAGIC`).
//!
//! # Addressing
//! Every OTA packet after ANNOUNCE carries a `target: u8` matching a
//! specific device's [`crate::device_id::DEVICE_ID`] — **not**
//! [`crate::device_id::BROADCAST`]. Only ANNOUNCE (an updater offering a new
//! image to the whole link) is broadcast; a device that wants the update
//! replies with ACCEPT carrying its own id, and from then on every CHUNK/ACK
//! exchange is unicast target↔device. This is deliberate: on a shared LoRa
//! channel, broadcasting the chunk stream to every device at once would
//! mean every device in range writes flash from every chunk — one bad CRC
//! or RF collision corrupts ALL of them simultaneously instead of just the
//! one node currently being updated. Addressing confines a failed/aborted
//! update to one device; the others keep running their current firmware and
//! can be updated one at a time afterwards (see module docs in
//! `embassy-boot-stm32`'s swap scheme for why a half-written image is safe
//! to retry — the active bank is untouched until the new one verifies).
//!
//! # Packet kinds
//! - `Announce { updater, image_len, image_crc }` — broadcast. "I have a new
//!   image, this long, this checksum." No device acts on this unprompted;
//!   it's informational until a human/host picks a target.
//! - `Accept { target }` — unicast reply from `target` back to `updater`,
//!   meaning "go ahead, start sending me chunks."
//! - `Chunk { target, index, total, data }` — unicast `updater → target`.
//!   `data` is up to `CHUNK_DATA_LEN` bytes of the new image at
//!   `index * CHUNK_DATA_LEN`.
//! - `ChunkAck { target, index, ok }` — unicast `target → updater`. `ok`
//!   false means "CRC/magic mismatch, resend this index" — not a NAK for
//!   the whole transfer, just this one chunk, so a flaky link only costs a
//!   retransmit of the bytes that were actually lost, not a restart from
//!   index 0.
//! - `Done { target, final_crc }` — unicast `updater → target` once all
//!   chunks are acked, carrying the CRC of the *reassembled* image for a
//!   final end-to-end check before `target` commits it to the update
//!   partition and reboots into the bootloader.
//! - `DoneAck { target, ok }` — unicast `target → updater`. `ok` true means
//!   "CRC matched, committing and rebooting now"; false means "mismatch,
//!   don't reboot, send Chunk/Done again."

use crate::device_id::BROADCAST;

pub const MAGIC: u8 = 0x5A;
pub const VERSION: u8 = 1;

/// Conservative for SF12 airtime and this radio's payload ceiling; leaves
/// headroom in a 12-byte-ish header for `Chunk`'s fixed fields. Tune down
/// further (e.g. 24) if testing shows the margin is too tight at SF12 —
/// smaller chunks cost more round-trips but each is cheaper to resend.
pub const CHUNK_DATA_LEN: usize = 32;

#[derive(Copy, Clone, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
    Announce = 0,
    Accept = 1,
    Chunk = 2,
    ChunkAck = 3,
    Done = 4,
    DoneAck = 5,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Kind::Announce),
            1 => Some(Kind::Accept),
            2 => Some(Kind::Chunk),
            3 => Some(Kind::ChunkAck),
            4 => Some(Kind::Done),
            5 => Some(Kind::DoneAck),
            _ => None,
        }
    }
}

/// Max encoded size across all variants — callers size their LoRa TX/RX
/// buffers to this, not to any one variant's `encode()` length. Matches
/// `Chunk`'s layout exactly: 3 (header) + 1 (target) + 2 (index) + 2
/// (total) + `CHUNK_DATA_LEN` (data) = 8 + `CHUNK_DATA_LEN`.
pub const MAX_PACKET_LEN: usize = 8 + CHUNK_DATA_LEN;

const HEADER_LEN: usize = 3; // MAGIC, VERSION, Kind

#[derive(Clone)]
pub enum Packet<'a> {
    Announce {
        updater: u8,
        image_len: u32,
        image_crc: u32,
    },
    Accept {
        target: u8,
    },
    Chunk {
        target: u8,
        index: u16,
        total: u16,
        data: &'a [u8],
    },
    ChunkAck {
        target: u8,
        index: u16,
        ok: bool,
    },
    Done {
        target: u8,
        final_crc: u32,
    },
    DoneAck {
        target: u8,
        ok: bool,
    },
}

impl<'a> Packet<'a> {
    /// `target`/`updater` this packet is addressed to or from, for the
    /// caller's filtering: accept if it equals your own
    /// [`crate::device_id::DEVICE_ID`], or (Announce only) if it's
    /// [`crate::device_id::BROADCAST`].
    pub fn addressed_to(&self) -> u8 {
        match self {
            Packet::Announce { .. } => BROADCAST,
            Packet::Accept { target }
            | Packet::Chunk { target, .. }
            | Packet::ChunkAck { target, .. }
            | Packet::Done { target, .. }
            | Packet::DoneAck { target, .. } => *target,
        }
    }

    /// Encodes into `buf`, returns the number of bytes written. `buf` must
    /// be at least [`MAX_PACKET_LEN`] long.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        buf[0] = MAGIC;
        buf[1] = VERSION;
        match self {
            Packet::Announce {
                updater,
                image_len,
                image_crc,
            } => {
                buf[2] = Kind::Announce as u8;
                buf[3] = *updater;
                buf[4..8].copy_from_slice(&image_len.to_le_bytes());
                buf[8..12].copy_from_slice(&image_crc.to_le_bytes());
                12
            }
            Packet::Accept { target } => {
                buf[2] = Kind::Accept as u8;
                buf[3] = *target;
                4
            }
            Packet::Chunk {
                target,
                index,
                total,
                data,
            } => {
                buf[2] = Kind::Chunk as u8;
                buf[3] = *target;
                buf[4..6].copy_from_slice(&index.to_le_bytes());
                buf[6..8].copy_from_slice(&total.to_le_bytes());
                let n = data.len().min(CHUNK_DATA_LEN);
                buf[8..8 + n].copy_from_slice(&data[..n]);
                8 + n
            }
            Packet::ChunkAck { target, index, ok } => {
                buf[2] = Kind::ChunkAck as u8;
                buf[3] = *target;
                buf[4..6].copy_from_slice(&index.to_le_bytes());
                buf[6] = *ok as u8;
                7
            }
            Packet::Done { target, final_crc } => {
                buf[2] = Kind::Done as u8;
                buf[3] = *target;
                buf[4..8].copy_from_slice(&final_crc.to_le_bytes());
                8
            }
            Packet::DoneAck { target, ok } => {
                buf[2] = Kind::DoneAck as u8;
                buf[3] = *target;
                buf[4] = *ok as u8;
                5
            }
        }
    }

    /// Decodes from `buf` (a received LoRa payload). Borrows `Chunk`'s data
    /// straight out of `buf`, so the returned `Packet` can't outlive it.
    pub fn decode(buf: &'a [u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN + 1 || buf[0] != MAGIC || buf[1] != VERSION {
            return None;
        }
        let kind = Kind::from_u8(buf[2])?;
        match kind {
            Kind::Announce => {
                if buf.len() < 12 {
                    return None;
                }
                Some(Packet::Announce {
                    updater: buf[3],
                    image_len: u32::from_le_bytes(buf[4..8].try_into().ok()?),
                    image_crc: u32::from_le_bytes(buf[8..12].try_into().ok()?),
                })
            }
            Kind::Accept => {
                if buf.len() < 4 {
                    return None;
                }
                Some(Packet::Accept { target: buf[3] })
            }
            Kind::Chunk => {
                if buf.len() < 8 {
                    return None;
                }
                let index = u16::from_le_bytes(buf[4..6].try_into().ok()?);
                let total = u16::from_le_bytes(buf[6..8].try_into().ok()?);
                Some(Packet::Chunk {
                    target: buf[3],
                    index,
                    total,
                    data: &buf[8..],
                })
            }
            Kind::ChunkAck => {
                if buf.len() < 7 {
                    return None;
                }
                Some(Packet::ChunkAck {
                    target: buf[3],
                    index: u16::from_le_bytes(buf[4..6].try_into().ok()?),
                    ok: buf[6] != 0,
                })
            }
            Kind::Done => {
                if buf.len() < 8 {
                    return None;
                }
                Some(Packet::Done {
                    target: buf[3],
                    final_crc: u32::from_le_bytes(buf[4..8].try_into().ok()?),
                })
            }
            Kind::DoneAck => {
                if buf.len() < 5 {
                    return None;
                }
                Some(Packet::DoneAck {
                    target: buf[3],
                    ok: buf[4] != 0,
                })
            }
        }
    }
}

/// How many [`CHUNK_DATA_LEN`]-byte chunks an image of `image_len` bytes
/// splits into — both ends compute this the same way so `Chunk::total`
/// matches what the receiver expects.
pub const fn chunk_count(image_len: u32) -> u16 {
    ((image_len as usize + CHUNK_DATA_LEN - 1) / CHUNK_DATA_LEN) as u16
}
