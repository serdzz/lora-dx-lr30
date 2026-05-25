# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Rust embedded firmware for a **two-board LoRa range test**. Two STM32F103C8T6 BluePill
dev-boards, each with an integrated DX-SMART DX-LR30 module (433 MHz, **SX1262 silicon
— not SX127x despite the family naming**). `node_a` initiates PINGs and sweeps
SF7→SF12; `node_b` responds with PONGs and follows the SF embedded in each PING.
Built on Embassy + `lora-phy` 3.x; logs over defmt-RTT (probe-rs) and over USB CDC-ACM.

A Flutter macOS+iOS companion app in the sibling `../companion/` directory of
this monorepo consumes `node_b`'s UART/USB output and merges it with iPhone
GPS into a map.

## Build / flash / observe

Toolchain (one-time): `rustup target add thumbv7m-none-eabi && cargo install probe-rs --features cli --locked`.

```bash
cargo build --release --bin node_a              # ~46-54 KB text; binary must fit 64 KB Flash
cargo run   --release --bin node_a              # flashes via probe-rs, streams defmt RTT
cargo run   --release --bin node_b
cargo run   --release --bin find_led            # diagnostic — cycles plausible LED GPIOs

DEFMT_LOG=trace cargo run --release --bin node_a   # full SPI byte trace from lora-phy

probe-rs list                                   # confirm ST-Link is enumerated
/opt/homebrew/Cellar/llvm@20/*/bin/llvm-size \
  target/thumbv7m-none-eabi/release/node_a      # text+data must stay under 64 KB
```

`cargo test` does not work — `no_std` target. Logic-only checks live in the
Flutter companion's `test/widget_test.dart`.

### Recovering from a stuck chip

If `probe-rs run` is killed mid-stream (SIGTERM/SIGKILL), the SWD session leaves
the target **halted** and the next attach fails with `SwdApWdataError`. Press
the on-board RST button or power-cycle the board (yank the USB cable that feeds
3V3, wait 3 s, re-plug) before re-flashing. `--connect-under-reset` does not
work because the ST-Link probe wiring on this dev-board doesn't include NRST.

## Architecture

**Single Cargo package, two `[[bin]]`s** (`node_a`, `node_b`) plus `find_led`.
The shared library `lora_dx_lr30` exposes:

- `protocol.rs` — 12-byte ping/pong wire format (`Packet::encode`/`decode`), the
  `SF_TABLE: [SpreadingFactor; 6]` swept by `node_a`, and `FREQ_HZ` / `TX_POWER_DBM`
  constants. Both bins parse and emit the same packet shape; `node_b` echoes the
  RSSI/SNR it heard in the PONG body so `node_a` can log both directions.
- `radio.rs` — LoRa modem constants (`BANDWIDTH`, `CODING_RATE`, `PREAMBLE_LEN`,
  `RX_SYMBOL_TIMEOUT`) **and** the custom `DxLr30` zero-sized struct that
  implements `Sx126xVariant`. Its only job is to override
  `use_dio2_as_rfswitch() = false` because DX-LR30 wires the RF SPDT switch to
  PA0/PA1 (TXEN/RXEN), not DIO2.
- `host_log.rs` — USART1 (PA9 TX / PA10 RX, 115200 8N1) text logger. The
  DX-SMART board routes USART1 through an on-board CH340 USB-UART bridge to
  its USB-C connector, so the host sees the same stream as `/dev/cu.wchusbserial*`.
  `static Pipe<1024>` + `host_log!()` macro pushes formatted text
  non-blocking; the `run_host_log` async task drains the pipe via UART DMA.
  Drops bytes on overflow rather than blocking the LoRa loop. *Don't* try to
  use the STM32's native USB peripheral on PA11/PA12 — those pins aren't
  wired to anything on this board.

Both bins follow the same shape: configure RCC (HSE 8 MHz + PLL→72 MHz so USB
gets 48 MHz), boot LED blink, init SPI + GPIO + EXTI, build the `Sx126x` driver,
spawn `usb_task`, run the main TX/RX loop using `lora-phy`'s `LoRa::tx` /
`LoRa::rx`. `node_a` runs an `SF7→SF12` sweep with PER summary per SF; `node_b`
sits in continuous RX, sends PONG on each PING, follows the SF the initiator
just declared.

## Non-obvious gotchas

**MISO bug in embassy-stm32 0.1 on STM32F1.** `Spi::new(...)` configures MISO
as `AFType::Input`, then calls `miso.set_speed(VeryHigh)` which on `gpio_v1`
rewrites MODE→OUTPUT_50MHZ while leaving CNF at `01`. On F1 that bit pattern is
**GPIO open-drain output**, so PA6 is driven low and the SX1262's MISO can never
push it high — every `ReadRegister` returns `0x00` and `lora.tx().await` hangs
forever waiting on DIO1. Both `node_a.rs` and `node_b.rs` patch PA6 back to
INPUT+FLOATING via direct PAC writes **immediately after `Spi::new(...)`**.
Don't remove that block unless you've upgraded `embassy-stm32` past 0.1.

**Pin assignments are board-specific, not BluePill-standard.** The DX-SMART
dev-board's onboard LED is on **PB11** (active-low), not PC13. The DX-LR30
control pins (NRST=PA3, BUSY=PA2, DIO1=PC15) come from the vendor's
`LR_driver/UserConfig.h` (DX-SMART reference firmware, shipped with the
dev-boards, not redistributed here) and are not freely changeable. When in
doubt, the canonical pinout is in `README.md` (this repo).

**lora-phy types need same-type pins.** `GenericSx126xInterfaceVariant<CTRL, WAIT>`
takes ONE `CTRL` (output) and ONE `WAIT` (async input) type. We mix PA0/PA1/PA3
(reset + RXEN + TXEN) and PA2/PC15 (BUSY + DIO1) which live on different concrete
pin types, so both bins use `Output::degrade()` and `ExtiInput::new(Input::new(...).degrade(), exti.degrade())`
to collapse them to `Output<'_, AnyPin>` / `ExtiInput<'_, AnyPin>`. The
`Channel as _` trait import on `exti::Channel` is required for `EXTIn.degrade()`.

**Flash size is tight.** Release `node_a` is ~54 KB text + ~7.5 KB BSS, right
against `memory.x`'s `FLASH = 64K`. Adding a feature can push the binary over.
Keep `opt-level = "s"`, `lto = "fat"`, `codegen-units = 1` in the release
profile. Many BluePill clones physically have 128 KB Flash even though marked
64K — if you need headroom, bump `memory.x` to `LENGTH = 128K` after verifying
the actual silicon.

**Default HSI clock is fine.** UART works on the default HSI 8 MHz (no PLL),
so both bins call `embassy_stm32::init(Default::default())`. An earlier
revision configured HSE+PLL→72 MHz to satisfy the native USB peripheral, but
that whole branch was removed once we discovered the board uses CH340 +
UART — there's no need to spin up HSE for this firmware.

## Reference firmware (vendor, not in this repo)

DX-SMART ships a Keil / STM32-HAL C project for the same boards (commonly
distributed as `LR20&30-433.zip`). `LR_driver/UserConfig.c::LoraInit()` is the
canonical SX1262 init recipe (PA config, sync word, mod params, DCDC
regulator, soft RF switch); cross-check any radio-init changes against it.
`Driver/driver_gpio.c` confirms the LED pin scaffolding (though the actual pin
macros are undefined in the vendor source — `find_led` discovered PB11 by
GPIO sweep).

## Companion app

`../companion/` (sibling directory in this monorepo) is the Flutter app that
reads `node_b`'s UART output on macOS and merges it with an iPhone GPS trace
into a route map. See its own `README.md` for the field workflow.
