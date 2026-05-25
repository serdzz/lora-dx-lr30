# LoRa DX-LR30 Range Test (BluePill + Embassy + lora-phy)

Two STM32F103C8T6 BluePill boards driving DX-SMART DX-LR30 modules (**SX1262**,
not SX127x) at 433 MHz. One board pings, the other pongs; both log RSSI/SNR.
The initiator sweeps SF7→SF12 and the responder follows the SF embedded in
each PING. Pin layout matches the DX-SMART dev-board's manufacturer reference
firmware (Keil/STM32-HAL C project shipped with the boards, not redistributed
in this repo), so the existing wiring is reused as-is.

## Wiring (per board)

| DX-LR30 | STM32F103C8T6 (BluePill) | Notes                                |
|---------|--------------------------|--------------------------------------|
| VCC     | 3V3                      | 3.3 V only, add 10–100 µF bulk cap   |
| GND     | GND                      |                                      |
| MOSI    | PA7                      | SPI1_MOSI                            |
| MISO    | PA6                      | SPI1_MISO                            |
| SCK     | PA5                      | SPI1_SCK                             |
| NSS     | PA4                      | GPIO CS, idle high                   |
| NRST    | PA3                      | active-low                           |
| BUSY    | PA2                      | input pulled-up, EXTI2               |
| DIO1    | PC15                     | RxDone/TxDone/Timeout IRQ, EXTI15    |
| TXEN    | PA0                      | drives onboard RF antenna switch (TX)|
| RXEN    | PA1                      | drives onboard RF antenna switch (RX)|

The DX-LR30 uses a **software-controlled RF SPDT switch** (RXEN/TXEN), not DIO2 —
this is why `radio.rs` defines a custom `DxLr30` variant that overrides
`use_dio2_as_rfswitch()` to `false`, and the IV is fed `rf_switch_rx`/`rf_switch_tx`
GPIOs.

A 433 MHz quarter-wave antenna (~17.3 cm) is required. Never run TX without one —
SX1262 high-power PA at +22 dBm draws ~120 mA spikes and can be damaged.

## Toolchain

```bash
rustup target add thumbv7m-none-eabi
cargo install probe-rs --features cli --locked
```

ST-Link V2 (clone is fine):

| ST-Link | BluePill |
|---------|----------|
| SWCLK   | PA14 (labeled SWCLK) |
| SWDIO   | PA13 (labeled SWDIO) |
| GND     | GND      |
| 3V3     | 3V3 (skip if board USB-powered) |

## Build & flash

On board A (initiator):
```bash
cargo run --release --bin node_a
```

On board B (responder):
```bash
cargo run --release --bin node_b
```

`probe-rs run` streams defmt logs from RTT in the terminal.

The same log lines are *also* emitted over USART1 (PA9 TX) at 115200 8N1. The
DX-SMART board routes that USART through an on-board CH340 USB-UART bridge to
the USB-C connector — plug a cable into the Mac and the board shows up as
`/dev/cu.wchusbserial*` (install [the WCH macOS driver][wch] if the device
appears in `ioreg` but not under `/dev/cu.*`).

[wch]: https://www.wch.cn/downloads/CH34XSER_MAC_ZIP.html

## What you see

`node_a` logs each ping/pong round:
```
sf=7 seq=3 rx_rssi=-42 rx_snr=10 tx_rssi=-44 tx_snr=11
```
- `rx_*` — what A heard when receiving the PONG.
- `tx_*` — what B reported it heard for the PING (echoed in the PONG body).

After 20 pings per SF, A prints a summary:
```
=== SF7 summary: hits=20/20 per=0% rx_rssi_avg=-43 rx_snr_avg=10 tx_rssi_avg=-44 tx_snr_avg=11 ===
```
Then steps to SF8, SF9, …, SF12, wraps back to SF7.

`node_b` follows SF automatically from each PING; you'll see `follow SF: 7 -> 8` etc.

## Knobs

- `protocol.rs::FREQ_HZ` — drop the center if your antenna prefers it.
- `protocol.rs::TX_POWER_DBM` — default +22 dBm (SX1262 HighPower PA, matches
  reference firmware). Cap to local regulation.
- `protocol.rs::PINGS_PER_SF` — bump for tighter PER stats per sweep.
- `radio.rs::RX_SYMBOL_TIMEOUT` — increase if SF12 misses on weak links.
- `radio.rs::DxLr30::use_dio2_as_rfswitch()` — leave as `false` for DX-LR30. If
  porting to a board that does wire DIO2 → RF switch and *doesn't* expose RXEN/TXEN,
  flip to `true` and pass `None`s for `rf_switch_rx`/`rf_switch_tx`.

## Bench-test (before going outdoors)

1. Place boards ~30 cm apart, antennas attached.
2. Start `node_b` first, then `node_a`. SF7 PER should be 0%, `rx_rssi` ≈ −30..−50 dBm.
3. Watch the SF sweep finish; the SF12 cycle takes ~30 s.
4. If `LoRa init failed` panics, recheck SPI + RESET wiring; SX1262 must respond on
   `sx126x_get_status` after reset. The first thing to check is BUSY (PA2): it
   must go low within ~10 ms of releasing RESET, otherwise the chip is stuck.

## Reference

The manufacturer's STM32 HAL/Keil reference firmware (C, shipped with DX-SMART
dev-boards under the `LR20&30-433/` archive — vendor-provided, not in this
repo) contains the canonical chip-init recipe in `LR_driver/UserConfig.c`.
Settings cross-checked against it: 433 MHz, +22 dBm, BW125, DCDC regulator,
soft RF switch via PA0/PA1, BUSY/DIO1 on PA2/PC15.

## Out of scope

LoRaWAN, encryption, on-flash logging, dynamic role switching. Pure raw-PHY range test.
