//! Minimal swap-and-boot stub. On every reset:
//! 1. Check the staging region ([`swap_common::layout::STAGING_OFFSET`])
//!    for a valid [`swap_common::layout::SwapHeader`] (written by
//!    `node_b_app`'s OTA receiver once it's fully CRC-verified an
//!    incoming image).
//! 2. If present: re-verify the image's CRC-32 one more time (paranoia —
//!    catches flash bitrot between "app wrote it" and "bootloader reads
//!    it", which on this chip is a reset, so effectively immediate; still
//!    cheap insurance), then page-erase the active region and copy the
//!    image across, halfword by halfword, then re-verify the ACTIVE
//!    region's CRC before clearing the staging header's `valid` marker.
//!    Only after that erase-of-the-marker succeeds do we consider the
//!    swap done — see the "why this ordering" note below.
//! 3. Either way (swap just happened, or nothing was pending), jump to
//!    the active app at [`swap_common::layout::ACTIVE_BASE`].
//!
//! # Why this ordering (crash-during-copy safety)
//! The staging region is NEVER modified by this bootloader until AFTER
//! the active region's copy is verified — so a reset at any point during
//! the copy (power cut, watchdog, whatever) leaves staging exactly as it
//! was, and the next boot just redoes the same copy from scratch. The
//! active region can be in a transiently-corrupt (half old / half new)
//! state during the copy, but the bootloader doesn't jump into it until
//! the copy is complete and reverified — if reset happens mid-copy, the
//! very fact we're back in the bootloader means we never jumped to the
//! half-written active region in the first place. The one edge case NOT
//! covered: physically failing flash that writes successfully but reads
//! back wrong later — out of scope for a 1 KB bootloader.
//!
//! # Why hand-rolled instead of embassy-stm32
//! `embassy-boot-stm32` needs embassy-stm32 0.6+; `node_b_app` is pinned
//! to 0.1 for `lora-phy` compatibility (see firmware/CLAUDE.md). Rather
//! than make the bootloader depend on whatever embassy version the app
//! happens to use (coupling two things that should be independent — a
//! bootloader that only works with ONE specific app embassy version is a
//! trap waiting to brick a board on the next app-side dependency bump),
//! this bootloader has ZERO embassy dependency: just `cortex-m`/
//! `cortex-m-rt` for the vector table and a hand-rolled flash driver
//! (`flash.rs`) using raw RM0008 register pokes. It will keep working no
//! matter what embassy version `node_b_app` moves to in the future.
//!
//! # Flashing this ONCE per board
//! Unlike `node_a`/`node_b`/`repeater`, this is flashed to address
//! `0x0800_0000` a single time per physical board (it IS the vector table
//! / reset entry point) — OTA updates only ever replace the ACTIVE region
//! this bootloader jumps to, never the bootloader itself. Re-flashing the
//! bootloader (e.g. after a bug fix in this crate) requires physical SWD
//! access again, same as the very first flash.

#![no_std]
#![no_main]

use swap_common::flash;

use cortex_m::peripheral::SCB;
use cortex_m_rt::entry;
use swap_common::crc32::crc32;
use swap_common::layout::{
    SwapHeader, ACTIVE_BASE, ACTIVE_LEN, FLASH_BASE, HEADER_RESERVED, PAGE_SIZE, upper_flash_present,
    STAGING_OFFSET,
};

use panic_halt as _;

fn read_header() -> Option<SwapHeader> {
    let base = (FLASH_BASE + STAGING_OFFSET) as *const u8;
    let mut buf = [0u8; 16];
    unsafe {
        core::ptr::copy_nonoverlapping(base, buf.as_mut_ptr(), 16);
    }
    SwapHeader::decode(&buf)
}

/// Clears just the `valid` marker (4 bytes at `VALID_OFFSET`) by erasing
/// the page it lives in and rewriting everything else in that page back
/// unchanged, minus the marker. The header lives in the FIRST page of
/// staging, which also holds the first `HEADER_RESERVED - 16` bytes of
/// image data — erase-then-rewrite-the-rest is required because STM32F1
/// flash bits only go 1→0 without an erase, and the marker's bits need to
/// go 0→1 (any value != VALID_MARKER works; this writes all-zero) to stop
/// being valid.
fn invalidate_header() {
    let page_base = FLASH_BASE + STAGING_OFFSET;
    let mut saved = [0u8; PAGE_SIZE as usize];
    unsafe {
        core::ptr::copy_nonoverlapping(page_base as *const u8, saved.as_mut_ptr(), PAGE_SIZE as usize);
    }
    // Zero the valid-marker field (bytes 12..16) in our RAM copy before
    // rewriting the page — this is what makes decode() reject it on the
    // next boot.
    saved[12..16].copy_from_slice(&0u32.to_le_bytes());
    unsafe {
        flash::erase_page(page_base);
        flash::write(page_base, &saved);
    }
}

/// Copies `image_len` bytes from staging (just past the header) into the
/// active region, page-erasing the active region first. Returns true if
/// the copied active region's CRC matches `image_crc`.
fn apply_update(image_len: u32, image_crc: u32) -> bool {
    if image_len > ACTIVE_LEN {
        return false; // shouldn't happen — node_b_app's OTA receiver already
                       // rejects an Announce this large, but double-check
                       // before writing past the active region's bounds.
    }

    let active_base = ACTIVE_BASE;
    let pages = ACTIVE_LEN / PAGE_SIZE;
    unsafe {
        for p in 0..pages {
            flash::erase_page(active_base + p * PAGE_SIZE);
        }
    }

    let src_base = (FLASH_BASE + STAGING_OFFSET + HEADER_RESERVED) as *const u8;
    // Copy in PAGE_SIZE chunks straight from flash-to-flash via a RAM
    // scratch buffer — reading directly out of staging while writing
    // active is fine (different flash banks' worth of address space, no
    // overlap, and STM32F1 flash reads are always allowed even while
    // another region is mid-program per RM0008).
    let mut remaining = image_len;
    let mut offset = 0u32;
    let mut scratch = [0u8; PAGE_SIZE as usize];
    while remaining > 0 {
        let n = remaining.min(PAGE_SIZE);
        unsafe {
            core::ptr::copy_nonoverlapping(src_base.add(offset as usize), scratch.as_mut_ptr(), n as usize);
            // Pad the tail chunk's unused bytes with 0xFF (erased-flash
            // value) so write() always sees an even length without
            // writing garbage past image_len into the active region.
            for b in scratch[n as usize..].iter_mut() {
                *b = 0xFF;
            }
            let write_len = if n % 2 == 0 { n } else { n + 1 };
            flash::write(active_base + offset, &scratch[..write_len as usize]);
        }
        offset += n;
        remaining -= n;
    }

    let active_slice = unsafe { core::slice::from_raw_parts(active_base as *const u8, image_len as usize) };
    crc32(active_slice) == image_crc
}

/// Jumps to the active app's reset vector. Never returns.
fn boot_active() -> ! {
    unsafe {
        let vector_table = ACTIVE_BASE as *const u32;
        let stack_ptr = core::ptr::read_volatile(vector_table);
        let reset_vector = core::ptr::read_volatile(vector_table.add(1));

        // Relocate the vector table before jumping — the active app's own
        // vector table is at ACTIVE_BASE, not FLASH_BASE, and the CPU
        // otherwise keeps using whatever VTOR this bootloader itself set
        // up (effectively FLASH_BASE), misrouting every interrupt/fault
        // the app enables to the bootloader's (nonexistent) handlers.
        let scb = &*cortex_m::peripheral::SCB::PTR;
        scb.vtor.write(ACTIVE_BASE);

        cortex_m::asm::bootstrap(stack_ptr as *const u32, reset_vector as *const u32);
    }
}

#[entry]
fn main() -> ! {
    if !upper_flash_present() {
        // This board's silicon doesn't have the flash this scheme needs —
        // checked by mirroring, not the factory size register, which reads
        // 64 on our boards despite working upper flash (see layout.rs) —
        // see layout.rs's module doc comment. Refuse to touch flash at
        // all (an update attempted on a true-64K chip would write past
        // physical flash); just boot whatever's at ACTIVE_BASE as-is, so
        // a board flashed with this bootloader by mistake isn't bricked,
        // it just can't receive OTA swaps.
        boot_active();
    }

    if let Some(header) = read_header() {
        let staging_image = unsafe {
            core::slice::from_raw_parts(
                (FLASH_BASE + STAGING_OFFSET + HEADER_RESERVED) as *const u8,
                header.image_len as usize,
            )
        };
        if crc32(staging_image) == header.image_crc && apply_update(header.image_len, header.image_crc) {
            invalidate_header();
        } else {
            // CRC mismatch (staging corrupted) or the copy's post-write
            // reverify failed — do NOT invalidate the header, so the next
            // boot retries the same copy rather than silently giving up
            // and booting a half-updated active region. The board will
            // keep retrying this copy on every reset until either it
            // succeeds or a new, good image overwrites staging.
        }
    }

    boot_active();
}

/// `cortex-m-rt` requires exactly one `#[exception] fn DefaultHandler` or
/// equivalent fallback for unhandled exceptions when not using embassy's
/// bundled vector table — a bare, do-nothing handler is fine here since
/// this bootloader has no peripherals/interrupts enabled at any point
/// before `boot_active()` hands off.
#[cortex_m_rt::exception]
unsafe fn DefaultHandler(_irqn: i16) {}

#[cortex_m_rt::exception]
unsafe fn HardFault(_frame: &cortex_m_rt::ExceptionFrame) -> ! {
    SCB::sys_reset();
}
