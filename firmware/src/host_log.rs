//! USART1 (PA9 TX / PA10 RX) text logger AND host command/OTA-frame
//! receiver.
//!
//! The DX-SMART dev-board routes its USB-C connector through an on-board CH340
//! USB-to-UART bridge into the MCU's USART1 — same pinout the manufacturer
//! firmware uses (DX-SMART vendor SDK). Native USB-CDC on PA11/PA12 is *not*
//! wired to anything on this board, so for "logs over USB" we have to go via
//! UART → CH340. On the host the device shows up as `/dev/cu.wchusbserial*`
//! (once the WCH driver is installed and the macOS kext is approved).
//!
//! `host_log!(...)` pushes text into a non-blocking Pipe; one async task drains
//! the pipe and writes to USART1 via DMA. The pipe drops bytes on overflow so
//! a disconnected / slow host never blocks the LoRa loop.
//!
//! # Host → device command/OTA channel (node_a only)
//! `node_a` is the only binary that reads this UART back (`node_b`/`repeater`/
//! `node_b_app` only ever write log text to it) — see `src/bin/node_a.rs`'s
//! OTA bridge mode and `tools/ota_push.py` for the host side. Two input
//! shapes share the same RX stream, told apart by their first byte:
//! - A line of plain ASCII text ending in `\n` (`OTA\n`, `EXIT\n`) — simple
//!   mode-switch commands, human-typeable for manual testing.
//! - A binary frame `[0xA5][len: u8][len bytes of ota::Packet-encoded data]`
//!   — one `ota::Packet` for `node_a` to transmit over LoRa as-is. `0xA5`
//!   doubles as `ota::MAGIC`'s first byte (deliberately, so the frame and
//!   the LoRa payload it carries start with the same byte — `len` is this
//!   framing's only addition, `ota::Packet` needs no length prefix of its
//!   own since it's raw LoRa payload with no host-facing framing need).
//!
//! Parsed results land in [`HOST_CMD`] for `node_a`'s main loop to drain
//! with `try_receive` — never blocks the LoRa ping/pong path waiting on the
//! host.

use core::fmt::Write;

use embassy_stm32::bind_interrupts;
use embassy_stm32::peripherals;
use embassy_stm32::usart::{Config as UartConfig, Uart};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pipe::Pipe;

bind_interrupts!(pub struct Irqs {
    USART1 => embassy_stm32::usart::InterruptHandler<peripherals::USART1>;
});

pub static HOST_LOG: Pipe<CriticalSectionRawMutex, 1024> = Pipe::new();

/// `core::fmt::Write` sink that pushes UTF-8 into `HOST_LOG` without blocking.
pub struct HostLogWriter;

impl Write for HostLogWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let _ = HOST_LOG.try_write(s.as_bytes());
        Ok(())
    }
}

/// Format a line and append CRLF — the CRLF helps line-based terminals
/// (`screen`, `minicom`, Arduino Serial Monitor) display each event on its
/// own row.
#[macro_export]
macro_rules! host_log {
    ($($arg:tt)*) => {{
        use ::core::fmt::Write as _;
        let _ = ::core::write!($crate::host_log::HostLogWriter, $($arg)*);
        let _ = ::core::write!($crate::host_log::HostLogWriter, "\r\n");
    }};
}

/// Drain `HOST_LOG` into USART1 forever. Spawn this once at boot. Used by
/// `node_b`/`repeater`/`node_b_app`, which only ever write log text to the
/// host and never read commands back.
pub async fn run_host_log(
    usart: peripherals::USART1,
    tx_pin: peripherals::PA9,
    rx_pin: peripherals::PA10,
    tx_dma: peripherals::DMA1_CH4,
    rx_dma: peripherals::DMA1_CH5,
) {
    let mut cfg = UartConfig::default();
    cfg.baudrate = 115_200;
    let uart = Uart::new(usart, rx_pin, tx_pin, Irqs, tx_dma, rx_dma, cfg)
        .expect("USART1 init");
    let (mut tx, _rx) = uart.split();

    let mut buf = [0u8; 64];
    loop {
        let n = HOST_LOG.read(&mut buf).await;
        let _ = tx.write(&buf[..n]).await;
    }
}

/// Pushes raw bytes (not text) into the same outbound pipe `host_log!`
/// uses — for forwarding an `ota::Packet` reply straight back to the host
/// unparsed, framed the same way `run_host_log_and_cmd`'s RX side expects
/// an outbound frame to look (`[0xA5][len][data]`) so the host tool can
/// use one framing in both directions.
pub fn push_ota_frame(data: &[u8]) {
    let len = data.len().min(255) as u8;
    let _ = HOST_LOG.try_write(&[0xA5, len]);
    let _ = HOST_LOG.try_write(&data[..len as usize]);
}

/// Same idea as [`push_ota_frame`] but for `io::Packet` relay/sensor
/// traffic, framed `[0xA6][len][data]` — a distinct leading byte from
/// the OTA framing's `0xA5` so the host-side parser can tell the two
/// binary frame families apart on byte 0 alone (`io::MAGIC` is `0x3C`,
/// unrelated to this UART framing byte — `ota::MAGIC` happening to equal
/// the OTA framing byte is a coincidence, not a rule this follows).
pub fn push_io_frame(data: &[u8]) {
    let len = data.len().min(255) as u8;
    let _ = HOST_LOG.try_write(&[0xA6, len]);
    let _ = HOST_LOG.try_write(&data[..len as usize]);
}

/// A command/frame parsed off the host UART — see the module doc comment's
/// "Host → device command/OTA channel" section for the wire shapes.
pub enum HostCmd {
    /// Plain-text line (sans trailing `\n`/`\r\n`), e.g. `b"OTA"`, `b"EXIT"`.
    /// Fixed-capacity because this is `no_std`/no-alloc — a line longer than
    /// this is truncated, which is fine for the short mode-switch keywords
    /// this channel actually carries.
    Line(heapless::Vec<u8, 32>),
    /// One raw `ota::Packet`-encoded frame, ready to hand straight to
    /// `lora.prepare_for_tx`/`tx()` unmodified.
    OtaFrame(heapless::Vec<u8, { crate::ota::MAX_PACKET_LEN }>),
    /// One raw `io::Packet`-encoded frame (relay set / sensor query),
    /// ready to transmit over LoRa unmodified — same "host drives the
    /// state machine, firmware is a dumb relay" split as `OtaFrame`.
    IoFrame(heapless::Vec<u8, { crate::io::MAX_PACKET_LEN }>),
}

/// Queue of parsed host commands for `node_a`'s main loop to drain with
/// `try_receive` (never blocks the LoRa ping/pong path). Depth 4 is enough
/// slack for a burst of OTA frames arriving faster than node_a can transmit
/// them one at a time over the air — the host side (`tools/ota_push.py`)
/// is expected to wait for an ack/response per frame rather than blindly
/// flooding this queue, so depth 4 is "a little slack", not "the real
/// flow-control mechanism".
pub static HOST_CMD: embassy_sync::channel::Channel<CriticalSectionRawMutex, HostCmd, 4> =
    embassy_sync::channel::Channel::new();

/// Like [`run_host_log`], but ALSO reads the host UART back and parses
/// [`HostCmd`]s into [`HOST_CMD`] — for `node_a`'s OTA bridge mode only.
/// Runs the TX-drain and RX-parse loops concurrently in one task via
/// `select`, since both sides share the one `Uart` peripheral instance
/// (can't be split across two separately-spawned tasks without `'static`
/// lifetime gymnastics this no_std/no-alloc setup doesn't need).
pub async fn run_host_log_and_cmd(
    usart: peripherals::USART1,
    tx_pin: peripherals::PA9,
    rx_pin: peripherals::PA10,
    tx_dma: peripherals::DMA1_CH4,
    rx_dma: peripherals::DMA1_CH5,
) {
    let mut cfg = UartConfig::default();
    cfg.baudrate = 115_200;
    let uart = Uart::new(usart, rx_pin, tx_pin, Irqs, tx_dma, rx_dma, cfg)
        .expect("USART1 init");
    let (mut tx, mut rx) = uart.split();

    let tx_fut = async {
        let mut buf = [0u8; 64];
        loop {
            let n = HOST_LOG.read(&mut buf).await;
            let _ = tx.write(&buf[..n]).await;
        }
    };

    let rx_fut = async {
        // Framing state machine: either collecting a text line (until
        // '\n'), or — on seeing the binary frame's leading 0xA5 byte
        // where a text line's first byte never legally is, since '\n'/
        // printable ASCII command keywords don't start with 0xA5 —
        // collecting `len` bytes of a binary ota::Packet frame.
        let mut line = heapless::Vec::<u8, 32>::new();
        let mut byte = [0u8; 1];
        loop {
            if rx.read(&mut byte).await.is_err() {
                continue; // framing/overrun error — resync by just reading the next byte
            }
            let b = byte[0];

            if line.is_empty() && (b == 0xA5 || b == 0xA6) {
                // Binary frame: next byte is length, then that many
                // payload bytes. 0xA5 = ota::Packet frame, 0xA6 =
                // io::Packet frame — see HostCmd's doc comment.
                let is_io_frame = b == 0xA6;
                let mut len_byte = [0u8; 1];
                if rx.read(&mut len_byte).await.is_err() {
                    continue;
                }
                let max_len = if is_io_frame {
                    crate::io::MAX_PACKET_LEN
                } else {
                    crate::ota::MAX_PACKET_LEN
                };
                let len = (len_byte[0] as usize).min(max_len);
                if is_io_frame {
                    let mut frame = heapless::Vec::<u8, { crate::io::MAX_PACKET_LEN }>::new();
                    let mut ok = true;
                    for _ in 0..len {
                        let mut b = [0u8; 1];
                        if rx.read(&mut b).await.is_err() {
                            ok = false;
                            break;
                        }
                        let _ = frame.push(b[0]);
                    }
                    if ok {
                        HOST_CMD.send(HostCmd::IoFrame(frame)).await;
                    }
                } else {
                    // `len` counts only the payload bytes after [0xA5][len]; the
                    // payload itself starts with ota::MAGIC (0x5A), so the UART
                    // framing byte 0xA5 must NOT be pushed into the frame.
                    let mut frame = heapless::Vec::<u8, { crate::ota::MAX_PACKET_LEN }>::new();
                    let mut ok = true;
                    for _ in 0..len {
                        let mut b = [0u8; 1];
                        if rx.read(&mut b).await.is_err() {
                            ok = false;
                            break;
                        }
                        let _ = frame.push(b[0]);
                    }
                    if ok {
                        HOST_CMD.send(HostCmd::OtaFrame(frame)).await;
                    }
                }
                continue;
            }

            if b == b'\n' {
                let trimmed_len = if line.last() == Some(&b'\r') { line.len() - 1 } else { line.len() };
                let mut out = heapless::Vec::<u8, 32>::new();
                let _ = out.extend_from_slice(&line[..trimmed_len]);
                HOST_CMD.send(HostCmd::Line(out)).await;
                line.clear();
                continue;
            }

            // Drop silently on overflow rather than erroring — a too-long
            // line just gets truncated, matching HostCmd::Line's doc
            // comment.
            let _ = line.push(b);
        }
    };

    embassy_futures::join::join(tx_fut, rx_fut).await;
}
