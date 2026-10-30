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

Repeaters additionally need a battery/supply voltage divider to PB0 (see
"Device addressing & OTA" below for the full wiring + rationale).

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

## Device addressing & OTA wire protocol (`device_id.rs`, `ota.rs`)

Scaffolding for a future multi-node deployment (a chain of repeaters, as
opposed to today's fixed `node_a`/`node_b` pair) — not wired into either
`[[bin]]` yet, but ready to build against.

- `device_id::DEVICE_ID` — each physical board's address, a `u8` baked in at
  compile time from the `DEVICE_ID` env var (`DEVICE_ID=3 cargo build
  --release --bin repeater`). Defaults to `0` if unset. `255`
  (`device_id::BROADCAST`) is reserved and rejected at compile time if you
  try to assign it to a device.
- `ota::Packet` — a separate, variable-length packet format (distinct
  `MAGIC` from `protocol.rs`'s fixed 12-byte ping/pong) for pushing a new
  firmware image over the same LoRa link: `Announce` (broadcast "I have an
  update") → `Accept` (a specific device opts in) → `Chunk`/`ChunkAck`
  (unicast, `CHUNK_DATA_LEN = 32` bytes at a time, resend-by-index on a bad
  chunk) → `Done`/`DoneAck` (final CRC check before the device commits and
  reboots). Chunk delivery is **unicast to one `target` device at a time on
  purpose** — see the module doc comment in `ota.rs` for why broadcasting
  flash writes to every repeater at once is the wrong failure mode on a
  shared channel.
- Pairs with `embassy-boot-stm32`'s swap-bank update scheme (not yet added
  to this repo): `ota.rs` only defines the wire format and chunking math,
  not flash writing or the bootloader handoff.
- `src/bin/repeater.rs` answers `ota::Packet` OTA traffic addressed to its
  own compile-time `DEVICE_ID`, staging incoming firmware chunks into an
  upper flash region with a final CRC check — but **does not implement
  the swap-and-boot handoff** (its `commit_update()` just logs and resets
  unchanged; see the "NOT YET IMPLEMENTED" note at the top of that file).
  For an OTA target that actually swaps and boots new firmware, use
  `../node_b_app/` instead (below) — repeater.rs stays receive-only/
  honestly-incomplete on purpose rather than silently gaining a different
  on-disk layout than what this README documents it as.
- `battery.rs` — battery/supply voltage via a resistor divider on PB0
  (ADC1_IN8), same 806 kΩ/2 MΩ ratio as the Feather nRF52832 BLE firmware
  in the sibling `embassy` repo. Wire `VBAT → 806k → PB0 → 2M → GND`.
  `repeater.rs` samples it on every forwarded PONG and stamps the result
  into `protocol::Packet::battery_mv` — so a base station sees each
  repeater's power-bank health riding on the same ping/pong traffic it's
  already relaying, with no separate telemetry packet or schedule. `0` in
  that field means "not measured" (node_a/node_b leave it at 0 — they're
  hand-held/wired-power and don't carry this divider).

## OTA updates that actually apply: `node_a` → `node_b_app` (`../node_b_app/`, `../bootloader/`)

`node_a` pushes a new firmware image to a `node_b_app` board over LoRa, and
`node_b_app` really does swap to it and reboot — unlike `repeater.rs`'s
receive-only OTA above. This needs two more crates (siblings of this one,
same monorepo):

- `../swap-common/` — flash layout + CRC-32 shared by the two below. No
  embassy/HAL dependency on purpose (see its doc comment) — the bootloader
  must never be coupled to whatever embassy version the app firmware
  happens to pin.
- `../bootloader/` — a ~3 KB hand-rolled bootloader, flashed ONCE per
  board at `0x0800_0000`. On every boot it checks the staging region for a
  pending update, copies it into the active-app region if present (with a
  CRC re-verify before AND after the copy — see its module doc comment for
  the crash-during-copy safety argument), then jumps to the active app.
- `../node_b_app/` — `node_b`'s ping/pong behaviour, linked to run from the
  active-app region (`0x0800_1000`, not `0x0800_0000` — see its
  `memory.x`) instead of the whole chip, plus a real OTA receiver (ported
  from `repeater.rs`'s `handle_ota`, with the STM32F1 "must erase before
  write" fix repeater.rs was missing) that stages the image and writes a
  swap header instead of just logging "not implemented."

**This flash layout needs 128 KB of physical flash**, not the 64 KB this
board is marketed as — see `swap-common/src/layout.rs`'s doc comment for
why many "C8" BluePill clones actually have it, and why both the
bootloader and `node_b_app` refuse to touch flash at all if the chip's
factory size register reports less.

### One-time setup per board

```bash
cd ../bootloader && cargo run --release           # flash the bootloader ONCE
cd ../node_b_app  && cargo run --release --bin node_b_app   # flash the app
```
After this, `probe-rs run` against `node_b_app` directly (bypassing the
bootloader) will NOT boot correctly — its vector table lives at
`0x0800_1000`, and only the bootloader's jump sets `VTOR` there. Reflash
the bootloader first on a chip that's been erased or came in blank.

### Pushing an update

```bash
# On node_a's host (same machine/terminal you'd use for `cargo run --bin node_a`'s UART log):
pip3 install pyserial
python3 ../tools/ota_push.py /dev/cu.wchusbserial1420 --target 2 \
    ../node_b_app/target/thumbv7m-none-eabi/release/node_b_app.bin
```
(`arm-none-eabi-objcopy -O binary ... node_b_app.bin` first if you need a
raw `.bin` rather than the ELF — `ota_push.py` just reads `image_len`
bytes from whatever file you give it, whole-file.) The script puts
`node_a` into OTA bridge mode over its own UART (sending `OTA\n`), relays
`Announce`/`Chunk`/`Done` to the target over LoRa, waits for each
`Accept`/`ChunkAck`/`DoneAck`, retries individual lost chunks, and sends
`EXIT\n` when done to put `node_a` back into its normal ping/pong sweep.
`--target` is the receiving board's compile-time `DEVICE_ID` (set when you
built/flashed its `node_b_app`, e.g. `DEVICE_ID=2 cargo build ...`).

Known gap: the bootloader's own 3 KB never gets updated by this scheme —
only the ~62 KB active-app region does. A bootloader bug needs physical
SWD re-flashing, same as the very first flash.

