class GpsFix {
  GpsFix({
    required this.timestamp,
    required this.lat,
    required this.lon,
    this.accuracyM,
    this.altitudeM,
    this.speedMps,
    this.headingDeg,
  });

  final DateTime timestamp;
  final double lat;
  final double lon;
  final double? accuracyM;
  final double? altitudeM;
  final double? speedMps;
  final double? headingDeg;

  static const csvHeader =
      'timestamp_iso,lat,lon,accuracy_m,altitude_m,speed_mps,heading_deg';

  String toCsvRow() {
    String s(double? d) => d?.toStringAsFixed(3) ?? '';
    return [
      timestamp.toIso8601String(),
      lat.toStringAsFixed(7),
      lon.toStringAsFixed(7),
      s(accuracyM),
      s(altitudeM),
      s(speedMps),
      s(headingDeg),
    ].join(',');
  }

  static GpsFix? fromCsvRow(String row) {
    final cols = row.split(',');
    if (cols.length < 3) return null;
    final ts = DateTime.tryParse(cols[0]);
    if (ts == null) return null;
    double? d(int i) =>
        cols.length > i && cols[i].isNotEmpty ? double.tryParse(cols[i]) : null;
    return GpsFix(
      timestamp: ts,
      lat: double.parse(cols[1]),
      lon: double.parse(cols[2]),
      accuracyM: d(3),
      altitudeM: d(4),
      speedMps: d(5),
      headingDeg: d(6),
    );
  }
}
