# lora_companion

Flutter companion app for the LoRa-DX-LR30 range-test firmware (sibling
`../firmware/` in this monorepo).
One codebase, two platforms:

- **iOS** — records a GPS trace while you walk around carrying `node_a`. Share
  the resulting `gps_*.csv` over AirDrop when you're back.
- **macOS** — reads `node_b`'s USB CDC log (`/dev/cu.usbmodem*`) at the
  stationary end, saves `lora_*.csv`, then merges the two files and plots the
  route on an OpenStreetMap with each LoRa hit coloured by RSSI.

The app picks the right UI for the host platform at startup (`Platform.isIOS` →
GPS recorder, otherwise → macOS hub).

## Field workflow

1. Flash both LoRa boards (see `../firmware/README.md`). `node_a` carries
   with you; `node_b` stays at the home/base point plugged into the Mac via USB-C.
2. **iPhone** — open the app, tap *Start tracking*, walk around. When done,
   tap *Share (AirDrop)* and send `gps_YYYYMMDD_HHMMSS.csv` to the Mac.
3. **Mac** — open the app, hit *USB capture · node_b*, pick the
   `wchusbserialXXXX` port (DX-SMART boards expose their UART through an
   on-board CH340 — install [the WCH macOS driver][wch] if no such port shows
   up), *Open & capture*. Save `lora_*.csv` after the walk.

[wch]: https://www.wch.cn/downloads/CH34XSER_MAC_ZIP.html
4. Back on the macOS hub, *Merge & map* → load both CSVs → see the route. The
   slider on the side panel controls the max-allowed timestamp gap when joining
   a LoRa hit to its nearest GPS fix (default 5 s).
5. *Export merged.csv* if you need the joined dataset for further analysis.

## Setup

```bash
flutter pub get
```

macOS additionally needs the native build deps for `libserialport`:

```bash
brew install automake libtool
```

iOS pods are fetched automatically on first build.

## Run

```bash
flutter run -d macos     # opens the hub (USB capture + map)
flutter run -d ios       # opens the GPS recorder on a tethered iPhone
flutter test             # runs the parser / merge unit tests
```

`flutter analyze` is clean except for a few `withOpacity` / `value` deprecation
notices in Material 3.

## What gets parsed

`LoRaEvent.parse(line)` recognises three flavours of input from the firmware's
USB-CDC output:

| Source           | Example                                                                   | `kind` |
|------------------|---------------------------------------------------------------------------|--------|
| `node_a` hit     | `sf=7 seq=3 rx_rssi=-44 rx_snr=10 tx_rssi=-43 tx_snr=11`                  | `hit`  |
| `node_b` hit     | `rx ping sf=7 seq=3 rssi=-95 snr=4`                                       | `hit`  |
| miss             | `miss sf=7 seq=4`                                                          | `miss` |
| anything else    | `=== SF7 summary: ... ===`, boot strings, …                               | `info` |

`node_b` is the canonical macOS-side source — it gives one RSSI/SNR per
received PING, which is exactly what we want to map to the iPhone's GPS at the
matching timestamp.

## Architecture

```
lib/
├── main.dart                       Platform-switch entry
├── models/
│   ├── lora_event.dart             Regex parsers + CSV round-trip
│   ├── gps_fix.dart                One Core Location sample
│   └── merged_point.dart           Nearest-timestamp join (binary search)
├── services/
│   ├── serial_service.dart         flutter_libserialport wrapper; line buffer
│   └── location_service.dart       geolocator stream + permission gate
└── screens/
    ├── gps_recorder_screen.dart    iOS UI
    ├── macos_home_screen.dart      Two-card hub
    ├── usb_capture_screen.dart     Port picker, live event list, save CSV
    └── map_screen.dart             flutter_map + OSM tiles + RSSI-coloured dots
```

The merge uses **timestamp-only** matching — both sides clock-stamp samples on
their own host (Mac and iPhone are independently NTP-synced, so the absolute
times line up within tens of milliseconds). LoRa events that don't have a GPS
fix within `maxDelta` (default 5 s) are dropped; this keeps the route honest
when the iPhone briefly loses GPS.

## Permissions

Already wired in:

- `ios/Runner/Info.plist` — `NSLocationWhenInUseUsageDescription`,
  `NSLocationAlwaysAndWhenInUseUsageDescription`, plus the `location`
  background mode so the trace continues when the screen locks.
- `macos/Runner/{Debug,Release}.entitlements` — **sandbox intentionally OFF**.
  macOS blocks character-device opens on `/dev/cu.usbmodem*` from sandboxed
  apps and the `temporary-exception.files.absolute-path.read-write` workaround
  is deprecated and silently ignored. Since this is a local dev tool (not an
  App Store build) disabling sandbox is the right call. `network.client` stays
  enabled for OSM tile fetching.

## CSV formats

Stable across the app's three writers:

- `lora_*.csv` — `timestamp_iso,kind,sf,seq,rx_rssi,rx_snr,tx_rssi,tx_snr,raw`
- `gps_*.csv` — `timestamp_iso,lat,lon,accuracy_m,altitude_m,speed_mps,heading_deg`
- `merged.csv` — `timestamp_iso,lat,lon,sf,seq,rx_rssi,rx_snr,tx_rssi,tx_snr,delta_ms,accuracy_m`

`delta_ms` is the absolute time gap between the LoRa hit and its matched GPS
fix — useful for filtering when post-processing in a spreadsheet.

## Why not direct USB on iOS?

Apple does not expose USB CDC-ACM to third-party iOS apps without MFi
certification, regardless of whether the iPhone uses Lightning or USB-C. The
practical workaround for "field walk with iPhone" is to keep the LoRa-to-host
link on macOS only and have the iPhone contribute the GPS half via its own
file, transferred over AirDrop. The two halves are joined later by timestamp.

If you genuinely need direct radio→iPhone in the field, the right answer is to
add a BLE bridge (e.g. HM-10 on the BluePill's UART) and switch the iOS app to
`flutter_blue_plus`. That would be a separate project, not this app.
