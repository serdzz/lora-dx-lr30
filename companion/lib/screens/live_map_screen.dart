import 'dart:async';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_map/flutter_map.dart';
import 'package:intl/intl.dart';
import 'package:latlong2/latlong.dart';
import 'package:path_provider/path_provider.dart';
import 'package:share_plus/share_plus.dart';

import '../models/gps_fix.dart';
import '../models/lora_event.dart';
import '../models/merged_point.dart';
import '../services/android_serial_service.dart';
import '../services/location_service.dart';
import '../widgets/rssi_dot.dart';

/// Live range-test screen for Android: node_a is plugged into the phone's
/// USB-C port, the phone's own GPS supplies position, and every LoRa hit is
/// dropped on the map at the phone's current location, coloured by RSSI.
class LiveMapScreen extends StatefulWidget {
  const LiveMapScreen({super.key});

  @override
  State<LiveMapScreen> createState() => _LiveMapScreenState();
}

class _LiveMapScreenState extends State<LiveMapScreen> {
  final _serial = AndroidSerialService();
  final _location = LocationService();
  final _map = MapController();

  StreamSubscription<LoRaEvent>? _evSub;
  StreamSubscription<GpsFix>? _fixSub;

  /// Hits paired with the phone's position at the moment they arrived.
  final _points = <MergedPoint>[];

  /// The full GPS breadcrumb track (drawn as a polyline behind the dots).
  final _track = <GpsFix>[];

  List<UsbDeviceInfo> _devices = const [];
  UsbDeviceInfo? _selected;
  GpsFix? _lastFix;

  bool _connected = false;
  bool _follow = true;
  String? _error;
  int _missCount = 0;

  @override
  void initState() {
    super.initState();
    _scan();
  }

  @override
  void dispose() {
    _evSub?.cancel();
    _fixSub?.cancel();
    _serial.dispose();
    _location.dispose();
    super.dispose();
  }

  Future<void> _scan() async {
    try {
      final devices = await AndroidSerialService.list();
      if (!mounted) return;
      setState(() {
        _devices = devices;
        if (_selected == null ||
            !devices.any((d) => d.deviceId == _selected!.deviceId)) {
          _selected = devices.isNotEmpty ? devices.first : null;
        }
      });
    } catch (e) {
      if (!mounted) return;
      setState(() => _error = e.toString());
    }
  }

  Future<void> _toggle() async {
    if (_connected) {
      await _stop();
      return;
    }
    final sel = _selected;
    if (sel == null) return;
    setState(() => _error = null);
    try {
      // GPS first: a hit without a fix can't be placed on the map.
      await _location.start();
      _fixSub = _location.fixes.listen(_onFix, onError: _onStreamError);

      await _serial.open(sel.deviceId);
      _evSub = _serial.events.listen(_onEvent, onError: _onStreamError);

      setState(() => _connected = true);
    } catch (e) {
      await _stop();
      if (!mounted) return;
      setState(() => _error = e.toString());
    }
  }

  Future<void> _stop() async {
    await _evSub?.cancel();
    await _fixSub?.cancel();
    _evSub = null;
    _fixSub = null;
    await _serial.close();
    await _location.stop();
    if (!mounted) return;
    setState(() => _connected = false);
  }

  void _onStreamError(Object e) {
    if (!mounted) return;
    setState(() => _error = e.toString());
  }

  void _onFix(GpsFix fix) {
    if (!mounted) return;
    setState(() {
      _lastFix = fix;
      _track.add(fix);
    });
    if (_follow) {
      _map.move(LatLng(fix.lat, fix.lon), _map.camera.zoom);
    }
  }

  void _onEvent(LoRaEvent ev) {
    if (!mounted) return;
    if (ev.kind == LoRaEventKind.miss) {
      setState(() => _missCount++);
      return;
    }
    if (ev.kind != LoRaEventKind.hit) return;

    final fix = _lastFix;
    if (fix == null) return; // No position yet — can't map it.
    final delta = ev.timestamp.difference(fix.timestamp).inMilliseconds.abs();
    setState(() {
      _points.add(MergedPoint(event: ev, fix: fix, deltaMs: delta));
    });
  }

  Future<void> _export() async {
    if (_points.isEmpty) return;
    final ts = DateFormat('yyyyMMdd_HHmmss').format(DateTime.now().toUtc());
    final dir = await getTemporaryDirectory();
    final file = File('${dir.path}/merged_$ts.csv');
    final sink = file.openWrite();
    sink.writeln(MergedPoint.csvHeader);
    for (final m in _points) {
      sink.writeln(m.toCsvRow());
    }
    await sink.flush();
    await sink.close();
    await SharePlus.instance.share(
      ShareParams(
        files: [XFile(file.path, mimeType: 'text/csv')],
        text: 'LoRa range test · ${_points.length} mapped hits · $ts UTC',
        subject: 'merged_$ts.csv',
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final center = _lastFix != null
        ? LatLng(_lastFix!.lat, _lastFix!.lon)
        : _points.isNotEmpty
            ? LatLng(_points.first.fix.lat, _points.first.fix.lon)
            : const LatLng(56.95, 24.10); // Riga-ish fallback before first fix.
    final trackPts = _track.map((f) => LatLng(f.lat, f.lon)).toList();

    return Scaffold(
      appBar: AppBar(
        title: const Text('Live map · node_a'),
        actions: [
          IconButton(
            tooltip: 'Re-scan USB',
            onPressed: _connected ? null : _scan,
            icon: const Icon(Icons.usb),
          ),
        ],
      ),
      body: Column(
        children: [
          _ConnectionBar(
            devices: _devices,
            selected: _selected,
            connected: _connected,
            onSelect: (d) => setState(() => _selected = d),
            onToggle: _selected == null ? null : _toggle,
          ),
          if (_error != null)
            Container(
              width: double.infinity,
              color: scheme.errorContainer,
              padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
              child: Text(_error!,
                  style: TextStyle(color: scheme.onErrorContainer)),
            ),
          Expanded(
            child: Stack(
              children: [
                FlutterMap(
                  mapController: _map,
                  options: MapOptions(
                    initialCenter: center,
                    initialZoom: 15,
                    // A manual drag means the user wants to look around;
                    // stop yanking the camera back to their position.
                    onPointerDown: (_, _) {
                      if (_follow) setState(() => _follow = false);
                    },
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
                    if (trackPts.length >= 2)
                      PolylineLayer(polylines: [
                        Polyline(
                          points: trackPts,
                          strokeWidth: 3,
                          color: Colors.blueGrey.withValues(alpha: 0.6),
                        ),
                      ]),
                    MarkerLayer(
                      markers: [
                        for (final m in _points)
                          Marker(
                            point: LatLng(m.fix.lat, m.fix.lon),
                            width: 14,
                            height: 14,
                            child: RssiDot(rssi: m.event.rxRssi ?? -130),
                          ),
                        if (_lastFix != null)
                          Marker(
                            point: LatLng(_lastFix!.lat, _lastFix!.lon),
                            width: 22,
                            height: 22,
                            child: const _MeMarker(),
                          ),
                      ],
                    ),
                    const RichAttributionWidget(attributions: [
                      TextSourceAttribution('© OpenStreetMap contributors'),
                    ]),
                  ],
                ),
                Positioned(
                  right: 12,
                  bottom: 12,
                  child: FloatingActionButton.small(
                    heroTag: 'recenter',
                    onPressed: _lastFix == null
                        ? null
                        : () {
                            setState(() => _follow = true);
                            _map.move(
                              LatLng(_lastFix!.lat, _lastFix!.lon),
                              _map.camera.zoom,
                            );
                          },
                    child: Icon(_follow ? Icons.my_location : Icons.location_searching),
                  ),
                ),
              ],
            ),
          ),
          _StatusBar(
            hits: _points.length,
            misses: _missCount,
            lastRssi: _points.isEmpty ? null : _points.last.event.rxRssi,
            hasFix: _lastFix != null,
            onExport: _points.isEmpty ? null : _export,
          ),
        ],
      ),
    );
  }
}

class _ConnectionBar extends StatelessWidget {
  const _ConnectionBar({
    required this.devices,
    required this.selected,
    required this.connected,
    required this.onSelect,
    required this.onToggle,
  });

  final List<UsbDeviceInfo> devices;
  final UsbDeviceInfo? selected;
  final bool connected;
  final ValueChanged<UsbDeviceInfo?> onSelect;
  final VoidCallback? onToggle;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 12, 12, 4),
      child: Row(
        children: [
          Expanded(
            child: devices.isEmpty
                ? const Text('No USB device. Plug node_a into the phone, '
                    'then tap the USB icon to re-scan.')
                : DropdownButtonFormField<UsbDeviceInfo>(
                    isExpanded: true,
                    decoration: const InputDecoration(
                      labelText: 'USB device',
                      border: OutlineInputBorder(),
                      isDense: true,
                    ),
                    value: selected,
                    items: devices
                        .map((d) => DropdownMenuItem(
                              value: d,
                              child: Text(d.description,
                                  overflow: TextOverflow.ellipsis),
                            ))
                        .toList(),
                    onChanged: connected ? null : onSelect,
                  ),
          ),
          const SizedBox(width: 8),
          FilledButton.icon(
            onPressed: onToggle,
            icon: Icon(connected ? Icons.stop : Icons.play_arrow),
            label: Text(connected ? 'Stop' : 'Connect'),
            style: FilledButton.styleFrom(
              backgroundColor:
                  connected ? scheme.errorContainer : scheme.primaryContainer,
              foregroundColor: connected
                  ? scheme.onErrorContainer
                  : scheme.onPrimaryContainer,
              padding: const EdgeInsets.symmetric(vertical: 18, horizontal: 16),
            ),
          ),
        ],
      ),
    );
  }
}

class _StatusBar extends StatelessWidget {
  const _StatusBar({
    required this.hits,
    required this.misses,
    required this.lastRssi,
    required this.hasFix,
    required this.onExport,
  });

  final int hits;
  final int misses;
  final int? lastRssi;
  final bool hasFix;
  final VoidCallback? onExport;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Material(
      color: scheme.surfaceContainerHighest,
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
        child: Row(
          children: [
            Icon(hasFix ? Icons.gps_fixed : Icons.gps_off,
                size: 18,
                color: hasFix ? scheme.primary : scheme.onSurfaceVariant),
            const SizedBox(width: 12),
            Text('hits $hits',
                style: const TextStyle(fontWeight: FontWeight.bold)),
            const SizedBox(width: 12),
            Text('miss $misses',
                style: TextStyle(color: scheme.onSurfaceVariant)),
            if (lastRssi != null) ...[
              const SizedBox(width: 12),
              RssiDot(rssi: lastRssi!, size: 12),
              const SizedBox(width: 4),
              Text('$lastRssi dBm'),
            ],
            const Spacer(),
            OutlinedButton.icon(
              onPressed: onExport,
              icon: const Icon(Icons.save_alt, size: 18),
              label: const Text('CSV'),
            ),
          ],
        ),
      ),
    );
  }
}

class _MeMarker extends StatelessWidget {
  const _MeMarker();

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Container(
      decoration: BoxDecoration(
        color: scheme.primary,
        shape: BoxShape.circle,
        border: Border.all(color: Colors.white, width: 2.5),
        boxShadow: const [BoxShadow(color: Colors.black45, blurRadius: 4)],
      ),
    );
  }
}
