import 'gps_fix.dart';
import 'lora_event.dart';

/// LoRa hit + GPS fix joined by nearest-timestamp.
class MergedPoint {
  MergedPoint({
    required this.event,
    required this.fix,
    required this.deltaMs,
  });

  final LoRaEvent event;
  final GpsFix fix;
  final int deltaMs;

  static const csvHeader =
      'timestamp_iso,lat,lon,sf,seq,rx_rssi,rx_snr,tx_rssi,tx_snr,delta_ms,accuracy_m';

  String toCsvRow() {
    return [
      event.timestamp.toIso8601String(),
      fix.lat.toStringAsFixed(7),
      fix.lon.toStringAsFixed(7),
      event.sf?.toString() ?? '',
      event.seq?.toString() ?? '',
      event.rxRssi?.toString() ?? '',
      event.rxSnr?.toString() ?? '',
      event.txRssi?.toString() ?? '',
      event.txSnr?.toString() ?? '',
      deltaMs.toString(),
      fix.accuracyM?.toStringAsFixed(1) ?? '',
    ].join(',');
  }
}

/// Join each HIT LoRa event with the nearest GPS fix (by timestamp).
/// Skip if the closest fix is more than [maxDelta] away — the route segment
/// is then a true gap, not a synthetic interpolation.
List<MergedPoint> mergeByTimestamp(
  List<LoRaEvent> events,
  List<GpsFix> fixes, {
  Duration maxDelta = const Duration(seconds: 5),
}) {
  if (fixes.isEmpty) return const [];
  final sortedFixes = [...fixes]..sort((a, b) => a.timestamp.compareTo(b.timestamp));

  final out = <MergedPoint>[];
  for (final ev in events.where((e) => e.kind == LoRaEventKind.hit)) {
    final fix = _nearest(sortedFixes, ev.timestamp);
    if (fix == null) continue;
    final delta = ev.timestamp.difference(fix.timestamp).inMilliseconds.abs();
    if (delta > maxDelta.inMilliseconds) continue;
    out.add(MergedPoint(event: ev, fix: fix, deltaMs: delta));
  }
  return out;
}

GpsFix? _nearest(List<GpsFix> sorted, DateTime when) {
  // Binary search for insertion index.
  var lo = 0;
  var hi = sorted.length - 1;
  while (lo < hi) {
    final mid = (lo + hi) >> 1;
    if (sorted[mid].timestamp.isBefore(when)) {
      lo = mid + 1;
    } else {
      hi = mid;
    }
  }
  final candidates = <GpsFix>[];
  if (lo > 0) candidates.add(sorted[lo - 1]);
  if (lo < sorted.length) candidates.add(sorted[lo]);
  if (candidates.isEmpty) return null;
  candidates.sort((a, b) {
    final da = a.timestamp.difference(when).inMilliseconds.abs();
    final db = b.timestamp.difference(when).inMilliseconds.abs();
    return da.compareTo(db);
  });
  return candidates.first;
}
