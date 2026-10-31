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
//! `embassy_stm32::adc::f1::VDDA_CALIB_MV = 3300` is the fixed assumption
//! this module relies on (board must actually run off a clean ~3.3 V rail;
//! if yours sags under radio TX current bursts, expect a few % of jitter
//! in the reading — average several samples if that matters for your use).

use embassy_stm32::adc::{Adc, SampleTime};
use embassy_stm32::bind_interrupts;
use embassy_stm32::peripherals::{ADC1, PB0};
use embassy_time::Delay;

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
    adc: Adc<'d, ADC1>,
    pin: PB0,
}

impl<'d> BatteryMonitor<'d> {
    pub fn new(adc1: ADC1, pin: PB0) -> Self {
        let mut delay = Delay;
        let mut adc = Adc::new(adc1, &mut delay);
        // Large divider resistors (806k/2M) need a long sample window for
        // the ADC's sample-and-hold cap to actually charge to the real
        // voltage — same reasoning as the Feather firmware's `Time::_40US`
        // for its divider. STM32F1 ADC sample time is in ADC clock cycles,
        // not microseconds; 239.5 cycles (the longest available) at the
        // default ~12 MHz ADC clock is ~20 µs, the safe choice here.
        adc.set_sample_time(SampleTime::Cycles239_5);
        // `Adc::new()` does NOT enable the ADC1_2 interrupt in the NVIC —
        // it only sets up the peripheral registers. `read()`'s internal
        // `poll_fn` waits on a waker that only `InterruptHandler::
        // on_interrupt()` (bound above via `bind_interrupts!`) ever wakes,
        // and that handler is only CALLED if this interrupt is actually
        // unmasked in the NVIC — without this, `read_mv().await` hangs
        // forever on the very first conversion, with no watchdog reset
        // (the hang is a `Pending` future, not a blocking spin loop, so
        // every OTHER task — including the watchdog petter — keeps
        // running normally while this one never completes). Found the
        // hard way: field test showed exactly one log line + 3 LED
        // blinks, then total silence, no reset-loop even after 15s.
        unsafe {
            use embassy_stm32::interrupt::typelevel::Interrupt;
            embassy_stm32::interrupt::typelevel::ADC1_2::unpend();
            embassy_stm32::interrupt::typelevel::ADC1_2::enable();
        }
        Self { adc, pin }
    }

    /// One-shot battery voltage reading, in millivolts.
    pub async fn read_mv(&mut self) -> u16 {
        let raw = self.adc.read(&mut self.pin).await as u32;
        let adc_mv = raw * embassy_stm32::adc::VDDA_CALIB_MV / embassy_stm32::adc::ADC_MAX;
        ((adc_mv * DIVIDER_X1M) / 1_000_000) as u16
    }
}
