# lora-dx-lr30

End-to-end LoRa **range-test rig** built around two DX-SMART **DX-LR30** modules
(SX1262 silicon, 433 МГц) on STM32F103C8T6 BluePill dev-boards. One node walks
around with a phone; the other sits at base plugged into a Mac. Distance vs.
RSSI is logged, GPS-tagged, and plotted on a map afterwards.

```
            ┌───────────────────────┐
walking → │ node_a (PING initiator)│      iPhone ─ GPS trace ──┐
            │ DX-LR30 + BluePill    │                            │
            └───────────┬───────────┘                            │
                        │ LoRa 433 МГц, +22 dBm                  │
                        ▼                                        ▼
            ┌───────────────────────┐                ┌────────────────────┐
            │ node_b (PONG responder)│ ─── USB-C ─→ │ macOS Flutter app  │
            │ DX-LR30 + BluePill    │  (CH340 UART) │ • USB capture       │
            └───────────────────────┘                │ • Merge by ts       │
                                                     │ • Map + RSSI dots   │
                                                     └────────────────────┘
```

## Repo layout

| Path           | What it is                                              |
|----------------|---------------------------------------------------------|
| `firmware/`    | Rust embedded firmware (Embassy + lora-phy). Two `[[bin]]`s: `node_a` (PING/SF sweep) and `node_b` (PONG/follow-SF). |
| `companion/`   | Flutter app — macOS reads `node_b`'s UART log, iOS records GPS, both merged into a map. |
| `docs/architecture.md` | End-to-end architecture: packet format, SF handoff, tasks, data flow, hardware quirks. |
| `firmware/docs/speed-vs-range.md` | Sensitivity / ToA / energy / typical range per SF — pick which SF to use for which test. |

Each subdirectory has its own README + setup steps.

## Quick start

**Firmware** (needs Rust + `probe-rs` + ST-Link V2 clone):

```bash
cd firmware
rustup target add thumbv7m-none-eabi
cargo install probe-rs --features cli --locked
cargo run --release --bin node_a   # flash + stream defmt-RTT on one board
cargo run --release --bin node_b   # the other board
```

**Companion app** (needs Flutter + `automake libtool` for libserialport on macOS):

```bash
cd companion
brew install automake libtool
flutter pub get
flutter run -d macos
flutter run -d ios     # on a paired, unlocked iPhone
```

## Hardware

DX-SMART DX-LR30 dev-board (USB-C, integrated CH340 USB↔UART bridge, on-board
DX-LR30 module wired to PA0..PA7 / PA3 / PC15 of an STM32F103C8T6). Pinout and
wiring constraints — see `firmware/README.md`.

You also need: 433 МГц quarter-wave antenna on **each** board (never run TX
without one; +22 dBm into open feed kills the PA), ST-Link V2 (clone is fine)
for flashing, USB-C cable for the macOS link, an iPhone with the companion
app for the GPS half.

## Status

- [x] Both nodes communicate, SF sweep works (with `next_sf_index` handoff)
- [x] UART log on `node_b` over CH340 → `/dev/cu.usbserial-XXX`
- [x] Hardware IWDG + 60 s app-level rx-timeout on `node_b`
- [x] Field LED indicator (PB11, active-low) for compute-less operation
- [x] Flutter companion: USB capture on macOS, GPS recorder on iOS, map merge
- [ ] BLE bridge variant so iPhone can read radio directly (planned)

## License

[MIT](LICENSE) © 2026 Sergej Lepin.
