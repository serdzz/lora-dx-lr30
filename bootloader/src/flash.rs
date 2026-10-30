//! Minimal unsafe STM32F1 internal-flash driver: unlock, page-erase,
//! half-word write. Hand-rolled register pokes instead of `embassy-stm32`
//! on purpose — see `main.rs`'s module doc comment for why this
//! bootloader stays off embassy entirely. Register addresses/bit layout
//! are architectural for the whole STM32F1 line (RM0008 §3.4 "Flash
//! memory interface registers"), not board-specific.

const FLASH_KEYR: *mut u32 = 0x4002_2004 as *mut u32;
const FLASH_SR: *mut u32 = 0x4002_200C as *mut u32;
const FLASH_CR: *mut u32 = 0x4002_2010 as *mut u32;
const FLASH_AR: *mut u32 = 0x4002_2014 as *mut u32;

const KEY1: u32 = 0x4567_0123;
const KEY2: u32 = 0xCDEF_89AB;

const CR_PG: u32 = 1 << 0;
const CR_PER: u32 = 1 << 1;
const CR_STRT: u32 = 1 << 6;
const CR_LOCK: u32 = 1 << 7;
const SR_BSY: u32 = 1 << 0;
const SR_EOP: u32 = 1 << 5;

unsafe fn wait_ready() {
    while core::ptr::read_volatile(FLASH_SR) & SR_BSY != 0 {}
}

unsafe fn unlock() {
    core::ptr::write_volatile(FLASH_KEYR, KEY1);
    core::ptr::write_volatile(FLASH_KEYR, KEY2);
}

unsafe fn lock() {
    let cr = core::ptr::read_volatile(FLASH_CR);
    core::ptr::write_volatile(FLASH_CR, cr | CR_LOCK);
}

/// Erases one 1 KB page at `addr` (must be page-aligned — caller's job,
/// this doesn't check). Blocks until the erase completes.
pub unsafe fn erase_page(addr: u32) {
    unlock();
    wait_ready();
    let cr = core::ptr::read_volatile(FLASH_CR);
    core::ptr::write_volatile(FLASH_CR, cr | CR_PER);
    core::ptr::write_volatile(FLASH_AR, addr);
    let cr = core::ptr::read_volatile(FLASH_CR);
    core::ptr::write_volatile(FLASH_CR, cr | CR_STRT);
    wait_ready();
    core::ptr::write_volatile(FLASH_SR, SR_EOP); // clear EOP, write-1-to-clear
    let cr = core::ptr::read_volatile(FLASH_CR);
    core::ptr::write_volatile(FLASH_CR, cr & !CR_PER);
    lock();
}

/// Writes `data` (any length, but STM32F1 only ever programs 16 bits at a
/// time — odd trailing byte is NOT written; caller must pad to an even
/// length, which the bootloader's caller always does because every image
/// chunk from `ota.rs`/`host`-side tooling is written whole-halfword) to
/// `addr`. `addr` and `data.len()` must both be even.
pub unsafe fn write(addr: u32, data: &[u8]) {
    debug_assert_eq!(addr % 2, 0);
    debug_assert_eq!(data.len() % 2, 0);
    unlock();
    for (i, chunk) in data.chunks_exact(2).enumerate() {
        wait_ready();
        let cr = core::ptr::read_volatile(FLASH_CR);
        core::ptr::write_volatile(FLASH_CR, cr | CR_PG);
        let halfword = u16::from_le_bytes([chunk[0], chunk[1]]);
        let dst = (addr + (i as u32) * 2) as *mut u16;
        core::ptr::write_volatile(dst, halfword);
        wait_ready();
        core::ptr::write_volatile(FLASH_SR, SR_EOP);
        let cr = core::ptr::read_volatile(FLASH_CR);
        core::ptr::write_volatile(FLASH_CR, cr & !CR_PG);
    }
    lock();
}
