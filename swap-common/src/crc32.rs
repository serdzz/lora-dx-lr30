//! Minimal `no_std` CRC-32 (the "zlib"/ISO-HDLC variant: poly `0xEDB88320`
//! reflected, init `0xFFFFFFFF`, final XOR `0xFFFFFFFF`). Bit-at-a-time, not
//! table-based — this only ever runs over a whole firmware image once (at
//! `Done`, and once more after a bootloader copy — see `../../bootloader`),
//! not in any latency-sensitive path, so the ~8x slowdown vs. a 256-entry
//! lookup table doesn't matter and we skip the 1 KB table to save flash.
//!
//! Matches Python's `zlib.crc32`/`binascii.crc32` and `crc32fast`'s default
//! bit-for-bit — pick whichever is convenient on the host side (see
//! `ota_push.py`) and it will agree with what `node_b_app`/`bootloader`
//! compute on-device.

pub struct Crc32 {
    crc: u32,
}

impl Crc32 {
    pub fn new() -> Self {
        Self { crc: 0xFFFF_FFFF }
    }

    pub fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.crc ^= byte as u32;
            for _ in 0..8 {
                // Branchless: an all-1s or all-0s mask from the low bit,
                // instead of an `if`, keeps this free of a data-dependent
                // branch per bit — not a security concern here, just
                // marginally friendlier to the pipeline on a chip this
                // small.
                let mask = (self.crc & 1).wrapping_neg();
                self.crc = (self.crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }

    pub fn finalize(self) -> u32 {
        !self.crc
    }
}

/// One-shot convenience wrapper over [`Crc32`].
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_vector() {
        // "123456789" -> 0xCBF43926 is the textbook CRC-32/ISO-HDLC test vector.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
