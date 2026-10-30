//! `no_std`, dependency-free crate shared by the OTA-capable application
//! firmware (`node_b_app`, in `../firmware/`) and the swap-and-boot
//! bootloader (`../bootloader/`). Holds exactly the things both sides must
//! agree on bit-for-bit: the flash layout, the "update pending" header
//! format, and a CRC-32 implementation — nothing HAL/embassy-specific, so
//! the bootloader doesn't have to pull in whatever embassy version the
//! application is pinned to (see `../bootloader/src/main.rs`'s doc comment
//! for why that independence matters).
#![no_std]

pub mod crc32;
pub mod layout;
