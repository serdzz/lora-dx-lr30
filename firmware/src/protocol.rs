use lora_phy::mod_params::SpreadingFactor;

pub const MAGIC: u8 = 0xA5;
pub const VERSION: u8 = 1;
pub const PACKET_LEN: usize = 12;

pub const FREQ_HZ: u32 = 433_000_000;
/// SX1262 high-power PA tops at +22 dBm. Reference firmware also uses 22.
pub const TX_POWER_DBM: i32 = 22;

pub const PINGS_PER_SF: u16 = 20;

pub const SF_TABLE: [SpreadingFactor; 6] = [
    SpreadingFactor::_7,
    SpreadingFactor::_8,
    SpreadingFactor::_9,
    SpreadingFactor::_10,
    SpreadingFactor::_11,
    SpreadingFactor::_12,
];

pub fn sf_from_index(i: u8) -> SpreadingFactor {
    SF_TABLE[(i as usize) % SF_TABLE.len()]
}

#[derive(Copy, Clone, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
    Ping = 0,
    Pong = 1,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Kind::Ping),
            1 => Some(Kind::Pong),
            _ => None,
        }
    }
}

#[derive(Copy, Clone)]
pub struct Packet {
    pub kind: Kind,
    /// SF this packet was modulated at — receiver must already be tuned to it.
    pub sf_index: u8,
    /// SF to switch to AFTER this round-trip. On every ping except the last
    /// of a round equals `sf_index`. On the last, equals the next SF in the
    /// sweep table — telegraphs the transition before node_a actually changes.
    /// node_b updates its `current_sf_index = next_sf_index` after sending the
    /// pong, keeping both ends synchronised across SF boundaries.
    pub next_sf_index: u8,
    pub seq: u16,
    pub echo_rssi: i16,
    pub echo_snr: i8,
}

impl Packet {
    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let mut b = [0u8; PACKET_LEN];
        b[0] = MAGIC;
        b[1] = VERSION;
        b[2] = self.kind as u8;
        b[3] = self.sf_index;
        b[4..6].copy_from_slice(&self.seq.to_le_bytes());
        b[6..8].copy_from_slice(&self.echo_rssi.to_le_bytes());
        b[8] = self.echo_snr as u8;
        b[9] = self.next_sf_index;
        b
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < PACKET_LEN {
            return None;
        }
        if buf[0] != MAGIC || buf[1] != VERSION {
            return None;
        }
        Some(Packet {
            kind: Kind::from_u8(buf[2])?,
            sf_index: buf[3],
            seq: u16::from_le_bytes([buf[4], buf[5]]),
            echo_rssi: i16::from_le_bytes([buf[6], buf[7]]),
            echo_snr: buf[8] as i8,
            next_sf_index: buf[9],
        })
    }
}
