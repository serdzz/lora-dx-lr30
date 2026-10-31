#![no_std]

pub mod battery;
pub mod device_id;
pub mod host_log;
pub mod ota;
pub mod protocol;
pub mod radio;
pub mod telemetry;

pub use swap_common::{crc32, layout};
