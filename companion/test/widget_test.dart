import 'package:flutter_test/flutter_test.dart';

import 'package:lora_companion/models/gps_fix.dart';
import 'package:lora_companion/models/lora_event.dart';
import 'package:lora_companion/models/merged_point.dart';

void main() {
  test('parses node_a hit line', () {
    final ev = LoRaEvent.parse(
      'sf=7 seq=3 rx_rssi=-44 rx_snr=10 tx_rssi=-43 tx_snr=11',
    )!;
    expect(ev.kind, LoRaEventKind.hit);
    expect(ev.sf, 7);
    expect(ev.rxRssi, -44);
    expect(ev.txSnr, 11);
  });

  test('parses node_b hit line', () {
    final ev = LoRaEvent.parse('rx ping sf=9 seq=42 rssi=-95 snr=4')!;
    expect(ev.kind, LoRaEventKind.hit);
    expect(ev.sf, 9);
    expect(ev.seq, 42);
    expect(ev.rxRssi, -95);
    expect(ev.rxSnr, 4);
    expect(ev.txRssi, isNull);
  });

  test('parses miss line', () {
    final ev = LoRaEvent.parse('miss sf=7 seq=0')!;
    expect(ev.kind, LoRaEventKind.miss);
    expect(ev.seq, 0);
  });

  test('csv round-trip', () {
    final ev = LoRaEvent.parse('rx ping sf=8 seq=1 rssi=-72 snr=8')!;
    final row = ev.toCsvRow();
    final back = LoRaEvent.fromCsvRow(row)!;
    expect(back.sf, 8);
    expect(back.rxRssi, -72);
  });

  test('merge picks nearest GPS within delta', () {
    final base = DateTime.utc(2026, 5, 24, 12);
    final events = [
      LoRaEvent.parse('rx ping sf=7 seq=0 rssi=-50 snr=10',
          at: base.add(const Duration(seconds: 2)))!,
      LoRaEvent.parse('rx ping sf=7 seq=1 rssi=-60 snr=8',
          at: base.add(const Duration(seconds: 12)))!,
    ];
    final fixes = [
      GpsFix(timestamp: base.add(const Duration(seconds: 1)), lat: 1, lon: 1),
      GpsFix(timestamp: base.add(const Duration(seconds: 2)), lat: 2, lon: 2),
      GpsFix(timestamp: base.add(const Duration(seconds: 20)), lat: 3, lon: 3),
    ];
    final merged =
        mergeByTimestamp(events, fixes, maxDelta: const Duration(seconds: 5));
    expect(merged.length, 1);
    // seq=0 event at +2s matches fix at +2s exactly (delta 0); seq=1 at +12s
    // is 8s away from the nearest fix (+20s) which is > maxDelta=5s → dropped.
    expect(merged.first.fix.lat, 2);
    expect(merged.first.deltaMs, 0);
  });
}
