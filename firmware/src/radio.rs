use lora_phy::mod_params::{Bandwidth, CodingRate};
use lora_phy::sx126x::{DeviceSel, Sx126xVariant};

pub const BANDWIDTH: Bandwidth = Bandwidth::_125KHz;
pub const CODING_RATE: CodingRate = CodingRate::_4_5;
pub const PREAMBLE_LEN: u16 = 8;
pub const MAX_LORA_PAYLOAD: usize = 32;

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
