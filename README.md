# lora-dx-lr30

End-to-end LoRa **range-test rig** built around two DX-SMART **DX-LR30** modules
(SX1262 silicon, 433 МГц) on STM32F103C8T6 BluePill dev-boards. One node walks
around with a phone; the other sits at base. Distance vs. RSSI is logged,
GPS-tagged, and plotted on a map — either **live on an Android phone** in the
field, or **offline on a Mac** by AirDropping the iPhone's GPS trace.

```
                ┌────────────────────────┐
   walking →  │ node_a (PING initiator) │
                │ DX-LR30 + BluePill     │
                └───────────┬────────────┘
                            │  LoRa 433 МГц, +22 dBm
                            │
            ┌───────────────┴────────────────────────────────────┐
            ▼                                                    ▼
┌────────────────────────┐                ┌─────────────────────────────┐
│ node_b (PONG responder)│ ── USB-C ───→ │ Android phone — live map    │
│ DX-LR30 + BluePill     │  (CH340 UART)  │ Flutter companion           │
└───────────┬────────────┘                │  • usb_serial host          │
            │                             │  • phone GPS geotags hits   │
            │ USB-C   iPhone ─ GPS ──┐   │  • RSSI dots on OSM, live   │
            ▼ (CH340)                 │   └─────────────────────────────┘
┌────────────────────────┐            │     ↑ walking-end host, all on one device
│ macOS Flutter app      │ ←─ AirDrop ┘
│ • USB capture (node_b) │   gps_*.csv
│ • Merge & map          │
└────────────────────────┘
```

Two walking-end options:
- **Android phone** plugged into **node_a** over USB-C/OTG — live map, phone GPS, everything on one device.
- **iPhone** records the GPS trace, **Mac** captures node_b's USB log, the two CSVs are merged into a map afterwards.

## Repo layout

| Path           | What it is                                              |
|----------------|---------------------------------------------------------|
| `firmware/`    | Rust embedded firmware (Embassy + lora-phy). Two `[[bin]]`s: `node_a` (PING/SF sweep) and `node_b` (PONG/follow-SF). |
| `companion/`   | Flutter app — three targets: **Android** does live USB-host capture from node_a + phone-GPS-tagged map; **macOS** captures node_b's UART log and merges CSVs offline; **iOS** records the GPS trace. |
| `docs/architecture.md` | End-to-end architecture: packet format, SF handoff, tasks, data flow, hardware quirks. |
| `docs/speed-vs-range.md` | Sensitivity / ToA / energy / typical range per SF — pick which SF to use for which test. |

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
flutter run -d ios       # on a paired, unlocked iPhone
flutter run -d android   # or `flutter install -d <id>` if Android-debugging
                         # over the same USB port that hosts node_a
```

On Android the app boots straight into the live map; plug node_a into the
phone's USB-C with an OTG/data cable, tap **Connect**, allow USB access and
location, and hits start dropping on the map at the phone's GPS position.

## Hardware

DX-SMART DX-LR30 dev-board (USB-C, integrated CH340 USB↔UART bridge, on-board
DX-LR30 module wired to PA0..PA7 / PA3 / PC15 of an STM32F103C8T6). Pinout and
wiring constraints — see `firmware/README.md`.

You also need: 433 МГц quarter-wave antenna on **each** board (never run TX
without one; +22 dBm into open feed kills the PA), ST-Link V2 (clone is fine)
for flashing, USB-C cable for the macOS link. For the GPS half pick one — an
**Android phone + USB-C OTG cable** for the live-map flow, or an **iPhone**
for the AirDrop-and-merge-on-Mac flow.

## Status

- [x] Both nodes communicate, SF sweep works (with `next_sf_index` handoff)
- [x] UART log on `node_b` over CH340 → `/dev/cu.usbserial-XXX`
- [x] Hardware IWDG + 60 s app-level rx-timeout on `node_b`
- [x] Field LED indicator (PB11, active-low) for compute-less operation
- [x] Flutter companion: USB capture on macOS, GPS recorder on iOS, map merge
- [x] Flutter companion: Android live-capture map (node_a over USB-C/OTG, hits dropped at the phone's GPS position in real time)
- [ ] BLE bridge variant so iPhone can read radio directly (planned)

## License

[MIT](LICENSE) © 2026 Sergej Lepin.
