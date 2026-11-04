//! Battery voltage of the repeater's supply, sampled on PB0 (ADC1_IN8)
//! through an external resistor divider.
//!
//! Mirrors the divider ratio used on the Feather nRF52832 BLE firmware
//! (`../nrf52832-ble/src/battery.rs` in the `embassy` repo): **806 kΩ over
//! 2 MΩ**, i.e. the ADC pin sees `VBAT * 806k / (806k + 2M)`. Wire the same
//! divider from the repeater's supply rail to PB0 — PA0..PA7 are all
//! claimed by the DX-LR30 radio (see README's wiring table), PB11 is the
//! status LED, PA9/PA10 are the UART, so PB0/PB1 (ADC1_IN8/IN9) are the
//! free ADC-capable pins on this BluePill.
//!
//! STM32F103's ADC1 has no internal VREF trim on this silicon revision —
//! `embassy_stm32::adc::VDDA_CALIB_MV = 3300` is the fixed assumption
//! this module relies on (board must actually run off a clean ~3.3 V rail;
//! if yours sags under radio TX current bursts, expect a few % of jitter
//! in the reading — average several samples if that matters for your use).
//!
//! # Low-power note (embassy-stm32 0.6 / STOP mode)
//! In embassy-stm32 0.6, `Adc::new()` calls `rcc::enable_and_reset::<ADC1>()`,
//! which increments the STOP-mode refcount for as long as the `Adc` is alive —
//! a long-lived `Adc` would permanently prevent the low-power executor from
//! ever entering STOP. `BatteryMonitor` therefore keeps only the `Peri`
//! tokens and constructs (and drops) the `Adc` inside each `read_mv()` call.
//! The cost is a re-calibration per reading (sub-millisecond at ~12 MHz ADC
//! clock), paid once per telemetry interval — negligible against the STOP
//! savings. `Adc::drop` disables ADON and releases the RCC clock/refcount.

use embassy_stm32::adc::{Adc, SampleTime};
use embassy_stm32::bind_interrupts;
use embassy_stm32::peripherals::{ADC1, PB0};
use embassy_stm32::Peri;

bind_interrupts!(struct Irqs {
    ADC1_2 => embassy_stm32::adc::InterruptHandler<ADC1>;
});

/// Divider ratio ×1_000_000 for integer math: `VBAT_mV = adc_mV * DIVIDER_X1M / 1_000_000`.
/// 2_000_000 / 806_000 = 2.481389... — stored pre-multiplied to keep this a
/// single integer multiply+divide at the call site, same trick as the
/// Feather firmware's `DIVIDER_X1000` (there scaled by 1000, here by 1e6
/// since the ratio itself needs more precision: 806k/2M diverges from a
/// round number at the 4th significant digit).
const DIVIDER_X1M: u32 = 2_481_389;

pub struct BatteryMonitor<'d> {
    adc1: Peri<'d, ADC1>,
    pin: Peri<'d, PB0>,
}

impl<'d> BatteryMonitor<'d> {
    pub fn new(adc1: Peri<'d, ADC1>, pin: Peri<'d, PB0>) -> Self {
        // Reference `Irqs` so `bind_interrupts!` above registers the real
        // ADC1_2 vector — `Adc::new()` (0.6) unpends/enables the NVIC line
        // itself, but the handler body only exists via this binding.
        // (In 0.1 we had to unpend/enable manually here — see git history.)
        let _ = Irqs;
        Self { adc1, pin }
    }

    /// One-shot battery voltage reading, in millivolts.
    ///
    /// Constructs the ADC driver afresh (incl. calibration) and drops it
    /// before returning, so no STOP-blocking RCC refcount outlives the call —
    /// see the module doc comment's low-power note.
    pub async fn read_mv(&mut self) -> u16 {
        // Large divider resistors (806k/2M) need a long sample window for
        // the ADC's sample-and-hold cap to actually charge to the real
        // voltage. 239.5 cycles (the longest available) at the default
        // ~12 MHz ADC clock is ~20 µs, the safe choice here.
        let mut adc = Adc::new(self.adc1.reborrow());
        let raw = adc.read(&mut self.pin, SampleTime::CYCLES239_5).await as u32;
        let adc_mv = raw * embassy_stm32::adc::VDDA_CALIB_MV / embassy_stm32::adc::ADC_MAX;
        ((adc_mv * DIVIDER_X1M) / 1_000_000) as u16
    }
}
