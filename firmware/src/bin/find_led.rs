#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{AnyPin, Level, Output, Pin, Speed};
use embassy_time::{Duration, Timer};

use defmt_rtt as _;
use panic_probe as _;

/// Cycle through every plausible LED GPIO on the DX-SMART dev-board (STM32F103C8T6 +
/// DX-LR30). For each pin: 4 quick blinks (120 ms ON / 120 ms OFF) + 600 ms solid ON
/// in active-LOW polarity. Log line shows which pin is driving right now.
///
/// Pins we skip:
/// - PA0..PA7, PA3, PC15: used by LoRa SPI / NRST / DIO1 / RXEN / TXEN
/// - PA13, PA14: SWD (don't touch — kills the debug probe)
///
/// On STM32F1 the JTAG-only pins PA15 (JTDI), PB3 (JTDO), PB4 (NJTRST) are reserved
/// at reset; we set AFIO_MAPR.SWJ_CFG = 0b010 to disable JTAG (keeping SWD) so those
/// pins become regular GPIO.
#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());

    // Free PA15 / PB3 / PB4 from JTAG. Requires AFIO clock to be enabled
    // (embassy_stm32::init does that) and writes the SWJ_CFG field.
    // SWJ_CFG bits: 010 = JTAG-DP disabled, SW-DP enabled (frees PA15/PB3/PB4 for GPIO).
    embassy_stm32::pac::AFIO.mapr().modify(|w| {
        w.set_swj_cfg(0b010);
    });

    info!("find_led v2: probing GPIO pins — watch the LED");

    let pins: [(&str, AnyPin); 23] = [
        ("PA8",  p.PA8.degrade()),
        ("PA9",  p.PA9.degrade()),
        ("PA10", p.PA10.degrade()),
        ("PA11", p.PA11.degrade()),
        ("PA12", p.PA12.degrade()),
        ("PA15", p.PA15.degrade()),
        ("PB0",  p.PB0.degrade()),
        ("PB1",  p.PB1.degrade()),
        ("PB2",  p.PB2.degrade()),
        ("PB3",  p.PB3.degrade()),
        ("PB4",  p.PB4.degrade()),
        ("PB5",  p.PB5.degrade()),
        ("PB6",  p.PB6.degrade()),
        ("PB7",  p.PB7.degrade()),
        ("PB8",  p.PB8.degrade()),
        ("PB9",  p.PB9.degrade()),
        ("PB10", p.PB10.degrade()),
        ("PB11", p.PB11.degrade()),
        ("PB12", p.PB12.degrade()),
        ("PB13", p.PB13.degrade()),
        ("PB14", p.PB14.degrade()),
        ("PB15", p.PB15.degrade()),
        ("PC13", p.PC13.degrade()),
    ];

    let mut iter = pins.into_iter();
    loop {
        let (label, pin) = match iter.next() {
            Some(x) => x,
            None => {
                info!("find_led: cycle complete — power-cycle to rerun");
                loop {
                    Timer::after(Duration::from_secs(60)).await;
                }
            }
        };

        info!("→ now driving {} (active-LOW, ~2 s)", label);
        let mut out = Output::new(pin, Level::High, Speed::Low);
        // 4 quick blinks
        for _ in 0..4 {
            out.set_low();
            Timer::after(Duration::from_millis(120)).await;
            out.set_high();
            Timer::after(Duration::from_millis(120)).await;
        }
        // 600 ms solid ON-low
        out.set_low();
        Timer::after(Duration::from_millis(600)).await;
        out.set_high();
        Timer::after(Duration::from_millis(400)).await;
        drop(out);
    }
}
