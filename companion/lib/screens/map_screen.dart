import 'dart:io';

import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';
import 'package:flutter_map/flutter_map.dart';
import 'package:latlong2/latlong.dart';

import '../models/gps_fix.dart';
import '../models/lora_event.dart';
import '../models/merged_point.dart';

class MapScreen extends StatefulWidget {
  const MapScreen({super.key});

  @override
  State<MapScreen> createState() => _MapScreenState();
}

class _MapScreenState extends State<MapScreen> {
  List<LoRaEvent> _events = const [];
  List<GpsFix> _fixes = const [];
  List<MergedPoint> _merged = const [];
  String? _loraPath;
  String? _gpsPath;
  String? _error;
  Duration _maxDelta = const Duration(seconds: 5);

  Future<void> _pickLora() async {
    final f = await openFile(acceptedTypeGroups: [
      const XTypeGroup(label: 'CSV', extensions: ['csv']),
    ]);
    if (f == null) return;
    try {
      final events = await _loadLora(File(f.path));
      setState(() {
        _loraPath = f.path;
        _events = events;
        _error = null;
        _recompute();
      });
    } catch (e) {
      setState(() => _error = e.toString());
    }
  }

  Future<void> _pickGps() async {
    final f = await openFile(acceptedTypeGroups: [
      const XTypeGroup(label: 'CSV', extensions: ['csv']),
    ]);
    if (f == null) return;
    try {
      final fixes = await _loadGps(File(f.path));
      setState(() {
        _gpsPath = f.path;
        _fixes = fixes;
        _error = null;
        _recompute();
      });
    } catch (e) {
      setState(() => _error = e.toString());
    }
  }

  void _recompute() {
    _merged = mergeByTimestamp(_events, _fixes, maxDelta: _maxDelta);
  }

  Future<void> _saveMerged() async {
    if (_merged.isEmpty) return;
    final loc = await getSaveLocation(
      suggestedName: 'merged.csv',
      acceptedTypeGroups: [
        const XTypeGroup(label: 'CSV', extensions: ['csv']),
      ],
    );
    if (loc == null) return;
    final f = File(loc.path);
    final sink = f.openWrite();
    sink.writeln(MergedPoint.csvHeader);
    for (final m in _merged) {
      sink.writeln(m.toCsvRow());
    }
    await sink.flush();
    await sink.close();
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(content: Text('Saved ${_merged.length} rows → ${f.path}')),
    );
  }

  @override
  Widget build(BuildContext context) {
    final initialCenter = _merged.isNotEmpty
        ? LatLng(_merged.first.fix.lat, _merged.first.fix.lon)
        : _fixes.isNotEmpty
            ? LatLng(_fixes.first.lat, _fixes.first.lon)
            : const LatLng(0, 0);

    final routePoints = _fixes.map((f) => LatLng(f.lat, f.lon)).toList();

    return Scaffold(
      appBar: AppBar(title: const Text('Merge & map')),
      body: Row(
        children: [
          SizedBox(
            width: 320,
            child: _SidePanel(
              loraPath: _loraPath,
              gpsPath: _gpsPath,
              eventsCount: _events.length,
              fixesCount: _fixes.length,
              mergedCount: _merged.length,
              maxDelta: _maxDelta,
              error: _error,
              onPickLora: _pickLora,
              onPickGps: _pickGps,
              onDeltaChanged: (d) => setState(() {
                _maxDelta = d;
                _recompute();
              }),
              onExport: _merged.isEmpty ? null : _saveMerged,
            ),
          ),
          const VerticalDivider(width: 1),
          Expanded(
            child: FlutterMap(
              options: MapOptions(
                initialCenter: initialCenter,
                initialZoom: routePoints.isEmpty ? 2 : 15,
                interactionOptions: const InteractionOptions(
                  flags: InteractiveFlag.all & ~InteractiveFlag.rotate,
                ),
              ),
              children: [
                TileLayer(
                  urlTemplate:
                      'https://tile.openstreetmap.org/{z}/{x}/{y}.png',
                  userAgentPackageName: 'com.lr30.lora_companion',
                ),
                if (routePoints.length >= 2)
                  PolylineLayer(polylines: [
                    Polyline(
                      points: routePoints,
                      strokeWidth: 3,
                      color: Colors.blueGrey.withOpacity(0.6),
                    ),
                  ]),
                if (_merged.isNotEmpty)
                  MarkerLayer(
                    markers: _merged.map((m) {
                      return Marker(
                        point: LatLng(m.fix.lat, m.fix.lon),
                        width: 14,
                        height: 14,
                        child: _RssiDot(rssi: m.event.rxRssi ?? -130),
                      );
                    }).toList(),
                  ),
                const RichAttributionWidget(attributions: [
                  TextSourceAttribution('© OpenStreetMap contributors'),
                ]),
              ],
            ),
          ),
        ],
      ),
    );
  }

  Future<List<LoRaEvent>> _loadLora(File f) async {
    final lines = await f.readAsLines();
    final out = <LoRaEvent>[];
    for (var i = 0; i < lines.length; i++) {
      if (i == 0 && lines[i].startsWith('timestamp_iso')) continue;
      final ev = LoRaEvent.fromCsvRow(lines[i]);
      if (ev != null) out.add(ev);
    }
    return out;
  }

  Future<List<GpsFix>> _loadGps(File f) async {
    final lines = await f.readAsLines();
    final out = <GpsFix>[];
    for (var i = 0; i < lines.length; i++) {
      if (i == 0 && lines[i].startsWith('timestamp_iso')) continue;
      final fix = GpsFix.fromCsvRow(lines[i]);
      if (fix != null) out.add(fix);
    }
    return out;
  }
}

class _SidePanel extends StatelessWidget {
  const _SidePanel({
    required this.loraPath,
    required this.gpsPath,
    required this.eventsCount,
    required this.fixesCount,
    required this.mergedCount,
    required this.maxDelta,
    required this.error,
    required this.onPickLora,
    required this.onPickGps,
    required this.onDeltaChanged,
    required this.onExport,
  });

  final String? loraPath;
  final String? gpsPath;
  final int eventsCount;
  final int fixesCount;
  final int mergedCount;
  final Duration maxDelta;
  final String? error;
  final VoidCallback onPickLora;
  final VoidCallback onPickGps;
  final ValueChanged<Duration> onDeltaChanged;
  final VoidCallback? onExport;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.all(16),
      child: ListView(
        children: [
          OutlinedButton.icon(
            onPressed: onPickLora,
            icon: const Icon(Icons.usb),
            label: const Text('Load lora.csv'),
          ),
          if (loraPath != null) ...[
            const SizedBox(height: 4),
            Text('$eventsCount events',
                style: Theme.of(context).textTheme.bodySmall),
          ],
          const SizedBox(height: 12),
          OutlinedButton.icon(
            onPressed: onPickGps,
            icon: const Icon(Icons.location_on_outlined),
            label: const Text('Load gps.csv'),
          ),
          if (gpsPath != null) ...[
            const SizedBox(height: 4),
            Text('$fixesCount fixes',
                style: Theme.of(context).textTheme.bodySmall),
          ],
          const Divider(height: 32),
          Text('Join tolerance', style: Theme.of(context).textTheme.titleSmall),
          const SizedBox(height: 4),
          Slider(
            min: 1,
            max: 30,
            divisions: 29,
            label: '${maxDelta.inSeconds} s',
            value: maxDelta.inSeconds.toDouble(),
            onChanged: (v) => onDeltaChanged(Duration(seconds: v.round())),
          ),
          Text(
            'A LoRa hit is dropped if the nearest GPS fix is more than '
            '${maxDelta.inSeconds} s away.',
            style: Theme.of(context).textTheme.bodySmall,
          ),
          const Divider(height: 32),
          Text('Merged', style: Theme.of(context).textTheme.titleMedium),
          const SizedBox(height: 4),
          Text('$mergedCount mapped points'),
          const SizedBox(height: 12),
          FilledButton.icon(
            onPressed: onExport,
            icon: const Icon(Icons.save_alt),
            label: const Text('Export merged.csv'),
          ),
          if (error != null) ...[
            const SizedBox(height: 12),
            Text(error!,
                style: TextStyle(color: Theme.of(context).colorScheme.error)),
          ],
          const Divider(height: 32),
          _LegendRow(rssi: -60, label: '≥ −60 dBm (strong)'),
          _LegendRow(rssi: -90, label: '−60 … −100 dBm'),
          _LegendRow(rssi: -115, label: '−100 … −120 dBm'),
          _LegendRow(rssi: -130, label: '< −120 dBm (limit)'),
        ],
      ),
    );
  }
}

class _LegendRow extends StatelessWidget {
  const _LegendRow({required this.rssi, required this.label});
  final int rssi;
  final String label;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 4),
      child: Row(
        children: [
          _RssiDot(rssi: rssi),
          const SizedBox(width: 8),
          Text(label, style: const TextStyle(fontSize: 12)),
        ],
      ),
    );
  }
}

class _RssiDot extends StatelessWidget {
  const _RssiDot({required this.rssi});
  final int rssi;

  static Color colorFor(int rssi) {
    // -50 dBm → strong green; -120 dBm → red.
    final t = ((rssi + 120) / 70).clamp(0.0, 1.0);
    return Color.lerp(Colors.red.shade700, Colors.green.shade400, t)!;
  }

  @override
  Widget build(BuildContext context) {
    return Container(
      width: 14,
      height: 14,
      decoration: BoxDecoration(
        color: colorFor(rssi),
        shape: BoxShape.circle,
        border: Border.all(color: Colors.black54, width: 1),
      ),
    );
  }
}
