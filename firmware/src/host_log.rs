//! USART1 (PA9 TX / PA10 RX) text logger.
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

/// Drain `HOST_LOG` into USART1 forever. Spawn this once at boot.
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
