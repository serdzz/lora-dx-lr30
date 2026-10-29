//! Per-device identity for addressing in multi-node deployments (e.g. a
//! chain of repeaters, as opposed to the current fixed node_a/node_b pair).
//!
//! Compile-time constant, set at build time with an environment variable —
//! no persistent storage, no runtime provisioning step. Each physical board
//! gets its own build:
//!
//!   DEVICE_ID=3 cargo build --release --bin repeater
//!
//! Defaults to 0 if unset, so existing builds (and `node_a`/`node_b`, which
//! don't use this yet) keep working unchanged.
//!
//! `0xFF` is reserved as a broadcast address (see `ota.rs`) — never assign
//! it to a real device.

/// This board's address, baked in at compile time via the `DEVICE_ID` env var.
pub const DEVICE_ID: u8 = {
    match option_env!("DEVICE_ID") {
        Some(s) => parse_u8_or_panic(s),
        None => 0,
    }
};

/// Broadcast address: an OTA packet addressed to `BROADCAST` is accepted by
/// every device regardless of its own `DEVICE_ID`. Used for announce/discover,
/// not for chunk delivery (which must be unicast — see `ota.rs` doc comment).
pub const BROADCAST: u8 = 0xFF;

/// `str::parse` isn't `const fn` in core; this is a minimal decimal-only
/// const-context parser, good enough for a build-time env var. Panics (a
/// compile error, since this runs at const-eval time) on anything else.
const fn parse_u8_or_panic(s: &str) -> u8 {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        panic!("DEVICE_ID env var is set but empty");
    }
    let mut value: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !b.is_ascii_digit() {
            panic!("DEVICE_ID must be a decimal number 0..=254 (255 is reserved for broadcast)");
        }
        value = value * 10 + (b - b'0') as u32;
        if value > 254 {
            panic!("DEVICE_ID must be 0..=254 (255 is reserved for broadcast)");
        }
        i += 1;
    }
    value as u8
}
