//! Flash layout and the "swap pending" header shared between the
//! bootloader (`../bootloader/`, a separate crate — see its own doc
//! comment for why) and `node_b_app`.
//!
//! # Layout (requires 128 KB physical flash — see the safety check below)
//! ```text
//! 0x0800_0000  bootloader        4 KB  (pages 0-3)
//! 0x0800_1000  active app       62 KB  (pages 4-65)   <- node_b_app runs here
//! 0x0801_0800  staging region   62 KB  (pages 66-127) <- OTA writes here
//! 0x0802_0000  (end of 128 KB)
//! ```
//! Chosen so the active app region is exactly as big as the staging
//! region: any image that fits as the ACTIVE app also fits as a RECEIVED
//! update, with no separate size limit to track. 1 KB granularity matches
//! this chip's erase page size (medium-density STM32F1).
//!
//! **This needs 128 KB of physical flash, not the 64 KB this board is
//! marketed as.** STM32F103C8T6 clones commonly ship with the CB die (128
//! KB) under the C8 marking — see CLAUDE.md's note on this — but it is NOT
//! guaranteed. The factory size register ([`real_flash_size_kb`]) is not
//! reliable either — our boards report 64 yet have working upper flash —
//! so both `bootloader` and `node_b_app` instead check
//! [`upper_flash_present`] (rejects a die whose upper half mirrors the
//! lower one) and `node_b_app` additionally verifies every staging erase
//! and write by reading it back, rather than silently writing past the
//! end of physical flash (which STM32F1 doesn't fault on — it just does
//! nothing or wraps, either of which corrupts something silently).
//!
//! # Why a bootloader instead of `embassy-boot-stm32`
//! `embassy-boot-stm32` needs `embassy-stm32` 0.6+; this repo is pinned to
//! 0.1 for `lora-phy` compatibility (see CLAUDE.md). This hand-rolled
//! bootloader only does ONE thing — copy staging→active on a flagged
//! reboot — not embassy-boot's full A/B anti-rollback scheme. In
//! particular: **a power loss during the staging→active copy leaves the
//! active region in an inconsistent state**; the bootloader's own
//! mitigation is to only clear the pending-swap flag AFTER a successful
//! copy+reverify, so a reset mid-copy retries the same copy from the start
//! on the next boot (idempotent — staging is never modified during copy,
//! only read) rather than leaving the flag cleared on a half-finished
//! active region. This is NOT bank-swap-atomic the way embassy-boot is;
//! treat it as best-effort, not a guarantee against bricking on a badly
//! timed power cut.

pub const FLASH_BASE: u32 = 0x0800_0000;
pub const PAGE_SIZE: u32 = 1024;

pub const BOOTLOADER_LEN: u32 = 4 * 1024;
pub const ACTIVE_OFFSET: u32 = BOOTLOADER_LEN; // 0x1000
pub const ACTIVE_LEN: u32 = 62 * 1024; // 0xF800
pub const STAGING_OFFSET: u32 = ACTIVE_OFFSET + ACTIVE_LEN; // 0x10800
pub const STAGING_LEN: u32 = 62 * 1024; // 0xF800

pub const ACTIVE_BASE: u32 = FLASH_BASE + ACTIVE_OFFSET;

/// Minimum physical flash size this scheme requires. See the module doc
/// comment — boards must be verified to actually have this much before
/// trusting OTA swaps on them.
pub const REQUIRED_FLASH_KB: u16 = 128;

/// Reads the STM32F1 factory-programmed flash-size register (a 16-bit
/// halfword at a fixed address, independent of what any datasheet/vendor
/// marking on the chip package says) — the only authoritative source for
/// "how much flash does THIS specific die actually have." Documented in
/// RM0008 as "Flash size register", present on all STM32F1 lines.
pub fn real_flash_size_kb() -> u16 {
    const F_SIZE_REG: *const u16 = 0x1FFF_F7E0 as *const u16;
    unsafe { core::ptr::read_volatile(F_SIZE_REG) }
}

/// Whether flash above 64 KB is real, independent of [`real_flash_size_kb`].
/// Our C8-marked boards report 64 in that register yet have working upper
/// flash (verified with `stm32flash -w -v` at 0x0801_0800), so the register
/// alone rejects good chips. The failure we must avoid is a true 64 KB die
/// whose upper addresses mirror the lower 64 KB: detect that by comparing
/// the bootloader's vector table at FLASH_BASE with FLASH_BASE + 64 KB
/// (active-region tail — never identical to the bootloader's vectors).
pub fn upper_flash_present() -> bool {
    let lo = FLASH_BASE as *const u32;
    let hi = (FLASH_BASE + 64 * 1024) as *const u32;
    (0..16).any(|i| unsafe { core::ptr::read_volatile(lo.add(i)) != core::ptr::read_volatile(hi.add(i)) })
}

/// Header written at the very start of the staging region, AFTER all image
/// chunks, with `valid` written last (see `encode`) — so a power loss
/// partway through writing this header is indistinguishable from "no
/// header written yet" (both read back as `valid != VALID_MARKER`),
/// never as a corrupted-but-apparently-valid header.
pub const HEADER_RESERVED: u32 = 64; // image data starts at this offset within staging
pub const HEADER_MAGIC: u32 = 0x4F54_4142; // "BATO" (OTA backwards-ish), arbitrary
pub const VALID_MARKER: u32 = 0xA55A_A55A;

#[derive(Clone, Copy)]
pub struct SwapHeader {
    pub image_len: u32,
    pub image_crc: u32,
}

impl SwapHeader {
    /// Encodes the header for writing. The caller MUST write this as two
    /// separate `blocking_write` calls in this exact order — see
    /// `encode_pending` / `encode_fields` below — so `valid` really does
    /// land last on the physical flash, not just last in this buffer.
    fn encode_fields(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&HEADER_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&self.image_len.to_le_bytes());
        b[8..12].copy_from_slice(&self.image_crc.to_le_bytes());
        b
    }

    /// Byte offset (within the staging region) of the `magic`+`image_len`+
    /// `image_crc` fields. Write this FIRST.
    pub const FIELDS_OFFSET: u32 = 0;
    /// Byte offset of the `valid` marker. Write this LAST, once the image
    /// data and the fields above are already durably written — see the
    /// struct doc comment.
    pub const VALID_OFFSET: u32 = 12;

    pub fn encode_fields_bytes(&self) -> [u8; 12] {
        self.encode_fields()
    }

    pub fn encode_valid_bytes() -> [u8; 4] {
        VALID_MARKER.to_le_bytes()
    }

    /// Decodes a header read back from staging. `None` if the magic or
    /// valid marker doesn't match (either never written, or a prior update
    /// was already consumed and the bootloader invalidated it — see
    /// `bootloader`'s copy routine).
    pub fn decode(buf: &[u8; 16]) -> Option<Self> {
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let image_len = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let image_crc = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let valid = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        if magic != HEADER_MAGIC || valid != VALID_MARKER {
            return None;
        }
        Some(SwapHeader { image_len, image_crc })
    }
}
