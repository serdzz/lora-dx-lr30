use lora_phy::mod_params::{Bandwidth, CodingRate};
use lora_phy::sx126x::{DeviceSel, Sx126xVariant};

pub const BANDWIDTH: Bandwidth = Bandwidth::_125KHz;
pub const CODING_RATE: CodingRate = CodingRate::_4_5;

/// Preamble length, in LoRa symbols, used by BOTH `node_a` (TX) and
/// `node_b_app` (RX) — must match since the receiver's configured
/// preamble length governs its hardware duty-cycle math below.
///
/// Bumped from the original `8` to give `node_b_app`'s RX duty-cycle
/// room to actually sleep between listen windows. See
/// `duty_cycle_for_sf`'s doc comment for why a short preamble leaves
/// (almost) no slack: the receiver's single listen window itself must
/// be >= ~8-10 symbols to reliably latch a preamble (SX1262 datasheet
/// §6.1.1.1 + confirmed independently by RadioLib's
/// `startReceiveDutyCycleAuto` minSymbols default), so an 8-symbol
/// preamble forces rxPeriod ≈ the whole preamble — no sleep fits.
/// 32 symbols costs node_a (battery-handheld, less power-constrained)
/// a longer TX-side preamble, in exchange for real sleep headroom on
/// node_b_app (field-deployed, battery-critical).
pub const PREAMBLE_LEN: u16 = 32;
pub const MAX_LORA_PAYLOAD: usize = 40;

/// RX timeout for `RxMode::Single(u16)`. On SX126x this is in 15.625 µs units when
/// using `set_rx`; lora-phy abstracts it as a symbol-based timeout (RegSymbTimeout).
/// 200 symbols at BW125 ≈ 200 ms @ SF7 … 6.5 s @ SF12 — matches our ping/pong slot.
pub const RX_SYMBOL_TIMEOUT: u16 = 200;

/// DX-LR30 LR30-specific variant: SX1262 silicon, HighPower PA, RF switch driven by
/// MCU GPIOs PA0/PA1 (TXEN/RXEN) — NOT by DIO2. This overrides the lora-phy default
/// `Sx1262::use_dio2_as_rfswitch() = true`.
pub struct DxLr30;

impl Sx126xVariant for DxLr30 {
    fn get_device_sel(&self) -> DeviceSel {
        DeviceSel::HighPowerPA
    }
    fn use_dio2_as_rfswitch(&self) -> bool {
        false
    }
}

use lora_phy::mod_params::{DutyCycleParams, SpreadingFactor};

/// SX126x RX timeout/duty-cycle registers tick in units of 15.625 µs
/// (1/64 ms). `ticks = round(us * 64 / 1000)`.
fn us_to_ticks(us: u32) -> u32 {
    (us * 64) / 1000
}

/// LoRa symbol duration in microseconds at the given SF, fixed at this
/// project's BANDWIDTH (125 kHz): `Tsym = 2^SF / BW`.
fn symbol_duration_us(sf: SpreadingFactor) -> u32 {
    let sf_num: u32 = match sf {
        SpreadingFactor::_5 => 5,
        SpreadingFactor::_6 => 6,
        SpreadingFactor::_7 => 7,
        SpreadingFactor::_8 => 8,
        SpreadingFactor::_9 => 9,
        SpreadingFactor::_10 => 10,
        SpreadingFactor::_11 => 11,
        SpreadingFactor::_12 => 12,
    };
    ((1u32 << sf_num) * 1_000_000) / 125_000
}

/// Minimum number of LoRa symbols the SX1262 needs to examine to
/// reliably LATCH a preamble (not just "the preamble exists" but "this
/// receiver, waking up at an arbitrary phase, will notice it before it
/// ends"). SX1262 datasheet §6.1.1.1: 8 symbols for SF7-SF12 (this
/// project's whole `SF_TABLE`), 12 for SF5/SF6 (unused here, but kept
/// correct). Independently corroborated by RadioLib's
/// `startReceiveDutyCycleAuto` doc comment, which hard-codes the same
/// 8/12 split. This is NOT the datasheet's `rxPeriod`/`sleepPeriod`
/// relationship (see below) — it's a separate, stricter floor on
/// `rxPeriod` alone that the first (naive) version of this function
/// missed, which is why it collapsed the link in the field.
fn min_latch_symbols(sf: SpreadingFactor) -> u32 {
    match sf {
        SpreadingFactor::_5 | SpreadingFactor::_6 => 12,
        _ => 8,
    }
}

/// Computes SX1262 hardware `SetRxDutyCycle` parameters (`rxPeriod` /
/// `sleepPeriod`, in 15.625 µs ticks) for the given SF, using
/// `PREAMBLE_LEN` (shared with node_a's TX config — see its doc
/// comment) as the preamble length to budget against.
///
/// # The two constraints, both from the SX1262 datasheet (§13.1.7 +
/// §6.1.1.1), that the EARLIER version of this function got wrong by
/// only honouring the first one:
///
/// 1. **Latch floor**: `rxPeriod` itself must be >= `min_latch_symbols`
///    worth of time — a listen window shorter than that can structurally
///    miss a preamble it's awake for, because the SX1262 needs that many
///    consecutive symbols to detect one. The original version set
///    `rx_us` to 2 symbols' worth, which is below this floor — the real
///    bug, not a rounding/sizing nit.
/// 2. **Coverage**: `T_preamble + T_header <= 2*rxPeriod + sleepPeriod`
///    — guarantees that no matter what phase the duty-cycle happens to
///    be in when a packet starts transmitting, at least one full
///    `rxPeriod` window falls entirely within the preamble+header, so
///    the chip gets a clean, uninterrupted look at it.
///
/// `rx_symbols` here is `min_latch_symbols + 2` (small margin over the
/// bare floor, cheap insurance against clock jitter / SPI scheduling
/// delay eating into the window). `sleep_us` is solved from constraint
/// 2 using an approximate header duration of 2 symbols, then derated by
/// 15% (`* 85 / 100`) as a safety margin — reduced from the original
/// 30% derating after a field test (2026-11-03, PREAMBLE_LEN=32,
/// 1s ping interval) confirmed the link held with ZERO packet loss at
/// the 30%-derated setting, meaning that margin had slack to give back
/// for more power savings. If a future field test shows desyncs
/// increase, revert this to `* 7 / 10`.
///
/// Returns `None` if the preamble isn't long enough to leave ANY sleep
/// margin after satisfying both constraints above — the caller should
/// treat that as "duty-cycling isn't viable at this SF with the current
/// `PREAMBLE_LEN`" and fall back to `RxMode::Continuous` rather than
/// ship a window so tight it risks repeating the SF12 field failure.
pub fn duty_cycle_for_sf(sf: SpreadingFactor) -> Option<DutyCycleParams> {
    let t_sym_us = symbol_duration_us(sf);
    let preamble_us = t_sym_us.saturating_mul(PREAMBLE_LEN as u32);
    let header_us = t_sym_us.saturating_mul(2); // explicit header, approx.

    let rx_symbols = min_latch_symbols(sf) + 2;
    let rx_us = t_sym_us.saturating_mul(rx_symbols);

    let coverage_total = preamble_us.saturating_add(header_us);
    let max_sleep_us = coverage_total.saturating_sub(rx_us.saturating_mul(2));
    if max_sleep_us == 0 {
        return None;
    }
    let sleep_us = (max_sleep_us.saturating_mul(85)) / 100;
    if sleep_us < 500 {
        // Not enough margin left to bother — the 500 µs floor is well
        // below one SX1262 wake-up latency's worth of benefit.
        return None;
    }

    Some(DutyCycleParams {
        rx_time: us_to_ticks(rx_us),
        sleep_time: us_to_ticks(sleep_us),
    })
}
