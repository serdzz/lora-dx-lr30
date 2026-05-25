# Architecture

End-to-end view of the LoRa-DX-LR30 range-test rig. Two embedded firmware
images on identical hardware, one Flutter app that runs in two distinct UIs
depending on the host OS, all glued together by a simple text protocol over
LoRa on one side and a CSV-on-disk handoff on the other.

## System overview

```
                ┌───────────────────────┐
walking node →  │ node_a (PING initiator)│
                │ SX1262 + STM32F103C8T6 │
                └──────────┬─────────────┘
                           │
                           │  LoRa 433 МГц, BW=125, CR=4/5
                           │  +22 dBm, 12-byte packet, CRC16-on
                           │  ─────────── air gap ───────────
                           │
                ┌──────────▼─────────────┐
base node →    │ node_b (PONG responder)│
                │ SX1262 + STM32F103C8T6 │
                └──────────┬─────────────┘
                           │ USART1 PA9 (TX) @ 115200 8N1
                           ▼
                ┌───────────────────────┐
                │ on-board CH340 USB-UART│
                └──────────┬─────────────┘
                           │ USB-C  (/dev/cu.usbserial-N on macOS)
                           ▼
                ┌───────────────────────┐
                │ companion (Flutter)    │   ┌─────────────────┐
                │ macOS — usb_capture    │   │ iPhone (Flutter)│
                │   → lora_*.csv         │   │ gps_recorder    │
                │ macOS — map_screen     ├──←│ → gps_*.csv     │
                │   merge by timestamp   │   │ via AirDrop     │
                │   → OSM + RSSI dots    │   └─────────────────┘
                └───────────────────────┘
```

Two physical USB cables involved during a session:
- ST-Link V2 → BluePill SWD (only during flashing; disconnected before the
  field walk to free the USB-C port for the host link)
- USB-C cable → BluePill USB-C → CH340 → MCU USART1 (the data path during
  the actual range test)

## LoRa link layer

### Packet format (12 bytes, little-endian)

| Offset | Field            | Size | Meaning                                                      |
|--------|------------------|------|--------------------------------------------------------------|
| 0      | `magic`          | u8   | `0xA5` — sanity check, rejects random air noise              |
| 1      | `version`        | u8   | `1` — bumped on any incompatible packet-layout change        |
| 2      | `kind`           | u8   | `0` = PING, `1` = PONG                                       |
| 3      | `sf_index`       | u8   | SF this packet was modulated at; index into `SF_TABLE[0..5]` |
| 4..6   | `seq`            | u16  | Sequence within the current SF round (0..19)                 |
| 6..8   | `echo_rssi`      | i16  | RSSI node_b heard for the PING (only meaningful in PONG)     |
| 8      | `echo_snr`       | i8   | SNR node_b heard for the PING (only meaningful in PONG)      |
| 9      | `next_sf_index`  | u8   | SF the *next* packet will be modulated at — sweep handoff    |
| 10..12 | reserved         | 2 B  | Padding for future fields                                    |

The two reserved bytes are not currently checked, so future versions can use
them without bumping `version` as long as readers default-init them to 0.

### Modulation parameters (compile-time constants)

| Constant            | Value     | Source                          |
|---------------------|-----------|---------------------------------|
| `FREQ_HZ`           | 433 000 000 | `firmware/src/protocol.rs`    |
| `TX_POWER_DBM`      | +22 dBm   | `firmware/src/protocol.rs`      |
| `BANDWIDTH`         | 125 кГц   | `firmware/src/radio.rs`         |
| `CODING_RATE`       | 4/5       | `firmware/src/radio.rs`         |
| `PREAMBLE_LEN`      | 8 symbols | `firmware/src/radio.rs`         |
| `RX_SYMBOL_TIMEOUT` | 200 symbols | `firmware/src/radio.rs`       |
| Sync word           | 0x12 (private LoRa, set by lora-phy default `enable_public_network=false`) | — |
| Header              | Explicit                                              |
| CRC16-CCITT         | **on** (both TX and RX)                               |
| IQ inversion        | off                                                   |

Per-SF sensitivity, ToA, and expected real-world range are tabulated in
[`speed-vs-range.md`](speed-vs-range.md).

### SF coordination

Naive approach (broken): node_a switches SF unilaterally, embeds the new SF in
the next packet. Doesn't work — LoRa is SF-selective, so node_b can't decode a
packet at the SF it isn't currently tuned to. Chicken-and-egg.

Working approach: `next_sf_index` is **telegraphed one ping ahead**:

```
node_a (sf_index=K)  ──→ PING(sf_index=K, next_sf_index=K)        ─→ ...
                         (19 such pings, the round)

node_a (sf_index=K)  ──→ PING(sf_index=K, next_sf_index=K+1)      ─→ last ping
node_b receives, sends PONG at K, then sets current_sf_index = K+1

node_a (sf_index=K+1) ─→ PING(sf_index=K+1, next_sf_index=K+1)    ─→ next round
node_b is already on K+1, demodulates fine
```

Edge case: if the *last* ping of a round is lost, node_a switches SF but
node_b doesn't, and they desync until either:

- node_a wraps the sweep back to K (worst-case wait: full sweep, ~3 minutes
  for SF7..SF12), OR
- node_b's app-level 60 s rx-timeout fires and (when sweep is enabled)
  advances `current_sf_index` by one — within ≤6 timeouts node_b lands on
  the same SF as node_a

Pinned-SF mode (currently active for SF12-only tests) disables both halves of
this — both bins hardcode `sf_index = 5`, `next_sf_index = sf_index`, no
transition signal, no scan on timeout.

### Round timing

- One round = `PINGS_PER_SF = 20` pings.
- Per-ping cycle = TX-PING (ToA) + turnaround + RX-PONG (ToA) + 50 ms idle.
- After a full round, node_a sleeps 500 ms before the next round.

For SF12/BW125 that's ~2.5 s per ping → ~50 s per round, ~0.4 successful
pings/sec — the slowest end of the trade. For SF7 it's ~7 pings/sec.

## Firmware (Rust + Embassy)

Single Cargo package, two `[[bin]]`s sharing a small `lora_dx_lr30` library:

```
firmware/src/
├── lib.rs            // re-export of the modules below
├── protocol.rs       // Packet encode/decode, SF_TABLE, constants
├── radio.rs          // BANDWIDTH/CR/preamble + DxLr30 Sx126xVariant
├── host_log.rs       // USART1 text logger (Pipe → DMA TX)
└── bin/
    ├── node_a.rs     // PING initiator + per-SF summary
    ├── node_b.rs     // PONG responder + IWDG + 60 s rx-timeout
    └── find_led.rs   // diagnostic — sweeps GPIO pins to locate the LED
```

### Tasks per node

```
node_a executor:
  ├── main          : LoRa init, then the PING/PONG loop
  └── host_log_task : drain HOST_LOG pipe → USART1 DMA

node_b executor:
  ├── main           : LoRa init, then the RX(continuous)/PONG loop
  ├── host_log_task  : drain HOST_LOG pipe → USART1 DMA
  └── watchdog_task  : pet IWDG every 2 s (timeout 8 s)
```

`main` owns the `LoRa<Sx126x<…>, Delay>` driver instance (it carries the
SPI bus, NSS, reset, BUSY, DIO1, RXEN, TXEN). The other tasks own only
their own peripherals and the global `Pipe<1024>` for log forwarding.

### Logging path

The single canonical event source is each `info!` / `warn!` in the main loop.
Each gets mirrored to two sinks:

1. **defmt-RTT** via probe-rs (active only when ST-Link is connected; binary
   wire format, decoded on the host by `probe-rs run`)
2. **`host_log!()`** macro → static `Pipe<1024, CriticalSectionRawMutex>` →
   `host_log_task` drains it onto USART1 via DMA → CH340 → USB-C → host

The pipe drops bytes on overflow rather than blocking. This means a slow /
disconnected host never stalls the LoRa loop — at worst, you lose lines while
the buffer is congested.

### Reliability (node_b only)

```
                       ┌─────────────────────┐
                       │ HW watchdog (IWDG)  │  8 s timeout, pet every 2 s
                       │ catches: executor   │  Reset chip if executor dies
                       │  death, panic loop, │
                       │  deadlock           │
                       └─────────────────────┘
                       ┌─────────────────────┐
                       │ App rx-timeout      │  60 s on lora.rx()
                       │ catches: silent     │  enter_standby + re-arm
                       │  SX1262 wedge       │  (sweep mode advances SF too)
                       │  with live executor │
                       └─────────────────────┘
```

node_a doesn't need either — its loop has natural per-ping progress (TX → RX
single-mode with `RX_SYMBOL_TIMEOUT`), so a stuck radio just produces a stream
of `miss` log lines without freezing anything.

## Companion app (Flutter)

```
companion/lib/
├── main.dart                       // Platform.isIOS → GPS recorder, else macOS hub
├── models/
│   ├── lora_event.dart             // parser (node_a + node_b log line shapes)
│   ├── gps_fix.dart                // Core Location sample
│   └── merged_point.dart           // nearest-timestamp join (binary search)
├── services/
│   ├── serial_service.dart         // flutter_libserialport wrapper, line buffer
│   └── location_service.dart       // geolocator stream + permission gate
└── screens/
    ├── gps_recorder_screen.dart    // iOS: Start/Stop + Share→AirDrop
    ├── macos_home_screen.dart      // two-card hub
    ├── usb_capture_screen.dart     // port picker, live list, save CSV
    └── map_screen.dart             // flutter_map + OSM + RSSI-coloured dots
```

### Data flow

```
┌──────────────────┐  60 s            ┌─────────────┐
│ /dev/cu.usbserial│ ─ bytes ────────→│ SerialPort  │
└──────────────────┘ 115200 8N1       │ Reader      │
                                      └──────┬──────┘
                                             │ Uint8List
                                             ▼
                                      ┌─────────────┐
                                      │ UTF-8 decode│
                                      │ + line buf  │
                                      └──────┬──────┘
                                             │ String per \n
                                             ▼
                                      ┌─────────────┐
                                      │ LoRaEvent   │
                                      │ ::parse()   │  regex (3 shapes)
                                      └──────┬──────┘
                                             ▼
                          ┌──────────────────┴─────────────────┐
                          ▼                                    ▼
                   ┌────────────┐                      ┌──────────────┐
                   │ Live list  │                      │ lora_*.csv   │
                   │ (ListView) │                      │ (file_selector│
                   └────────────┘                      │  Save dialog) │
                                                       └──────────────┘
```

`lora_*.csv` + AirDropped `gps_*.csv` → `MapScreen` → `mergeByTimestamp()`
(binary-search nearest fix per LoRa hit, drop if `|delta| > maxDelta`,
default 5 s) → `flutter_map` Markers coloured by RSSI (green → red).

### Parser shapes

The serial stream from `node_b` is a mix of free-form log lines plus a few
structured shapes that the parser cares about:

| Source         | Regex template                                                                | LoRaEvent.kind |
|----------------|-------------------------------------------------------------------------------|----------------|
| `node_a` hit   | `sf=N seq=N rx_rssi=N rx_snr=N tx_rssi=N tx_snr=N`                            | `hit`          |
| `node_b` hit   | `rx ping sf=N seq=N rssi=N snr=N`                                             | `hit`          |
| miss / silent  | `miss sf=N seq=N` or `rx silent 60s on SFN — re-arming`                       | `miss`/`info`  |
| everything else | `=== SF7 round start (20 pings) ===`, `node_b ... booting`, summaries        | `info`         |

Only `hit` events are matched against GPS fixes in the merge step.

### Permissions / sandboxing

| Platform | What's wired                                                                          |
|----------|---------------------------------------------------------------------------------------|
| iOS      | `NSLocationWhenInUseUsageDescription` + always usage + `UIBackgroundModes=location`   |
| macOS    | App sandbox **disabled** — character-device opens on `/dev/cu.*` are blocked by sandbox even with the deprecated `temporary-exception.files.absolute-path.read-write` exception. Network client kept on for OSM tile fetching. |

## Hardware quirks worth knowing

1. **DX-LR30 module is SX1262, not SX127x.** The name is misleading; cross-
   checked against the vendor firmware which uses the SX126x command set.
2. **USB-C is a CH340 UART bridge, not native STM32 USB.** PA11/PA12 on the
   MCU aren't wired to the connector; PA9/PA10 (USART1) go through CH340.
   Host sees `/dev/cu.usbserial-N`, requires the WCH driver if it doesn't
   bind automatically.
3. **embassy-stm32 0.1 MISO bug on STM32F1.** `Spi::new(...)` ends with
   `miso.set_speed(VeryHigh)` which on `gpio_v1` rewrites MODE→OUTPUT_50MHZ
   while leaving CNF=01, turning MISO into a GPIO open-drain output. PA6
   is then pulled to 0 V by the MCU itself and SX1262's MISO can never push
   it high → every `ReadRegister` returns 0x00 → `lora.tx().await` spins
   forever waiting on DIO1 IRQ that the chip can't acknowledge through SPI.
   Workaround: PAC-level reset of PA6 to INPUT+FLOATING immediately after
   `Spi::new(...)` — see the block in both `node_*.rs`.
4. **RF SPDT switch is software-controlled** via TXEN (PA0) / RXEN (PA1) on
   the DX-LR30 module — *not* the SX1262's DIO2 line. Hence the custom
   `DxLr30` Sx126xVariant in `radio.rs` that returns
   `use_dio2_as_rfswitch() = false`.
5. **User LED on PB11**, not the BluePill-standard PC13. Discovered by GPIO
   sweep via `find_led` because the vendor `LedGpioInit` function has the
   pin macros undefined.
6. **Default HSI 8 MHz clock is fine** for both timing and USART; HSE+PLL
   only matters if you ever switch back to the native USB peripheral on
   PA11/PA12 (which you can't, see #2 above).

## CSV file formats

Three stable schemas the companion app uses:

- `lora_*.csv` — `timestamp_iso, kind, sf, seq, rx_rssi, rx_snr, tx_rssi, tx_snr, raw`
- `gps_*.csv`  — `timestamp_iso, lat, lon, accuracy_m, altitude_m, speed_mps, heading_deg`
- `merged.csv` — `timestamp_iso, lat, lon, sf, seq, rx_rssi, rx_snr, tx_rssi, tx_snr, delta_ms, accuracy_m`

Timestamps are ISO-8601 UTC. The merge step relies on independent NTP-synced
clocks on the Mac and the iPhone — they're typically within tens of
milliseconds of each other, which is well under the default `maxDelta = 5 s`
tolerance.

## Out of scope (intentionally)

- LoRaWAN — too much overhead for a point-to-point range test
- Encryption / authentication — open ISM band, hobbyist use
- On-flash log persistence — host-side capture is the source of truth
- Dynamic role switching at runtime — separate `node_a` / `node_b` bins are
  simpler and Flash is tight
- BLE bridge for direct radio→iPhone — would need extra hardware (HM-10 or
  similar) and a different transport layer. Listed as a future possibility
  in the top-level README.
