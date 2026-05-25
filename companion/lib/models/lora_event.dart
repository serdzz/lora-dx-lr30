/// One parsed line of the LoRa CDC output stream.
///
/// Three flavours:
/// - HIT  (`sf=7 seq=3 rx_rssi=-44 rx_snr=10 tx_rssi=-43 tx_snr=11`)
/// - MISS (`miss sf=7 seq=3`)
/// - INFO (anything else — boot strings, `=== SF... summary ===` etc.)
enum LoRaEventKind { hit, miss, info }

class LoRaEvent {
  LoRaEvent({
    required this.timestamp,
    required this.kind,
    required this.raw,
    this.sf,
    this.seq,
    this.rxRssi,
    this.rxSnr,
    this.txRssi,
    this.txSnr,
  });

  final DateTime timestamp;
  final LoRaEventKind kind;
  final String raw;

  final int? sf;
  final int? seq;
  final int? rxRssi;
  final int? rxSnr;
  final int? txRssi;
  final int? txSnr;

  /// node_a format: `sf=7 seq=3 rx_rssi=-44 rx_snr=10 tx_rssi=-43 tx_snr=11`
  static final _hitNodeARe = RegExp(
    r'sf=(\d+)\s+seq=(\d+)\s+rx_rssi=(-?\d+)\s+rx_snr=(-?\d+)\s+tx_rssi=(-?\d+)\s+tx_snr=(-?\d+)',
  );

  /// node_b format: `rx ping sf=7 seq=3 rssi=-95 snr=4` — RSSI/SNR of the
  /// incoming PING (only one side of the round-trip).
  static final _hitNodeBRe = RegExp(
    r'rx\s+ping\s+sf=(\d+)\s+seq=(\d+)\s+rssi=(-?\d+)\s+snr=(-?\d+)',
  );

  static final _missRe = RegExp(r'miss\s+sf=(\d+)\s+seq=(\d+)');

  static LoRaEvent? parse(String line, {DateTime? at}) {
    final ts = at ?? DateTime.now().toUtc();
    final clean = line.trim();
    if (clean.isEmpty) return null;

    final hitA = _hitNodeARe.firstMatch(clean);
    if (hitA != null) {
      return LoRaEvent(
        timestamp: ts,
        kind: LoRaEventKind.hit,
        raw: clean,
        sf: int.parse(hitA.group(1)!),
        seq: int.parse(hitA.group(2)!),
        rxRssi: int.parse(hitA.group(3)!),
        rxSnr: int.parse(hitA.group(4)!),
        txRssi: int.parse(hitA.group(5)!),
        txSnr: int.parse(hitA.group(6)!),
      );
    }

    final hitB = _hitNodeBRe.firstMatch(clean);
    if (hitB != null) {
      return LoRaEvent(
        timestamp: ts,
        kind: LoRaEventKind.hit,
        raw: clean,
        sf: int.parse(hitB.group(1)!),
        seq: int.parse(hitB.group(2)!),
        rxRssi: int.parse(hitB.group(3)!),
        rxSnr: int.parse(hitB.group(4)!),
      );
    }

    final miss = _missRe.firstMatch(clean);
    if (miss != null) {
      return LoRaEvent(
        timestamp: ts,
        kind: LoRaEventKind.miss,
        raw: clean,
        sf: int.parse(miss.group(1)!),
        seq: int.parse(miss.group(2)!),
      );
    }

    return LoRaEvent(
      timestamp: ts,
      kind: LoRaEventKind.info,
      raw: clean,
    );
  }

  /// CSV header for the lora.csv file.
  static const csvHeader =
      'timestamp_iso,kind,sf,seq,rx_rssi,rx_snr,tx_rssi,tx_snr,raw';

  String toCsvRow() {
    String esc(String s) =>
        s.contains(',') || s.contains('"') ? '"${s.replaceAll('"', '""')}"' : s;
    final cols = <String>[
      timestamp.toIso8601String(),
      kind.name,
      sf?.toString() ?? '',
      seq?.toString() ?? '',
      rxRssi?.toString() ?? '',
      rxSnr?.toString() ?? '',
      txRssi?.toString() ?? '',
      txSnr?.toString() ?? '',
      esc(raw),
    ];
    return cols.join(',');
  }

  static LoRaEvent? fromCsvRow(String row) {
    // Lightweight CSV split that respects double-quoted fields.
    final cols = _splitCsv(row);
    if (cols.length < 9) return null;
    int? intOrNull(String s) => s.isEmpty ? null : int.tryParse(s);
    final ts = DateTime.tryParse(cols[0]);
    if (ts == null) return null;
    return LoRaEvent(
      timestamp: ts,
      kind: LoRaEventKind.values.firstWhere(
        (k) => k.name == cols[1],
        orElse: () => LoRaEventKind.info,
      ),
      sf: intOrNull(cols[2]),
      seq: intOrNull(cols[3]),
      rxRssi: intOrNull(cols[4]),
      rxSnr: intOrNull(cols[5]),
      txRssi: intOrNull(cols[6]),
      txSnr: intOrNull(cols[7]),
      raw: cols[8],
    );
  }
}

List<String> _splitCsv(String row) {
  final out = <String>[];
  final sb = StringBuffer();
  var inQuotes = false;
  var i = 0;
  while (i < row.length) {
    final c = row[i];
    if (inQuotes) {
      if (c == '"' && i + 1 < row.length && row[i + 1] == '"') {
        sb.write('"');
        i += 2;
        continue;
      }
      if (c == '"') {
        inQuotes = false;
        i++;
        continue;
      }
      sb.write(c);
      i++;
    } else {
      if (c == ',') {
        out.add(sb.toString());
        sb.clear();
        i++;
        continue;
      }
      if (c == '"') {
        inQuotes = true;
        i++;
        continue;
      }
      sb.write(c);
      i++;
    }
  }
  out.add(sb.toString());
  return out;
}
