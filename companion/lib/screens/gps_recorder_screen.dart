import 'dart:async';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:intl/intl.dart';
import 'package:path_provider/path_provider.dart';
import 'package:share_plus/share_plus.dart';

import '../models/gps_fix.dart';
import '../services/location_service.dart';

class GpsRecorderScreen extends StatefulWidget {
  const GpsRecorderScreen({super.key});

  @override
  State<GpsRecorderScreen> createState() => _GpsRecorderScreenState();
}

class _GpsRecorderScreenState extends State<GpsRecorderScreen> {
  final _svc = LocationService();
  final _fixes = <GpsFix>[];
  StreamSubscription<GpsFix>? _sub;
  GpsFix? _last;
  String? _error;
  bool _recording = false;

  @override
  void dispose() {
    _sub?.cancel();
    _svc.dispose();
    super.dispose();
  }

  Future<void> _toggle() async {
    if (_recording) {
      await _svc.stop();
      await _sub?.cancel();
      _sub = null;
      setState(() => _recording = false);
      return;
    }
    setState(() => _error = null);
    try {
      await _svc.start();
      _sub = _svc.fixes.listen((f) {
        if (!mounted) return;
        setState(() {
          _fixes.add(f);
          _last = f;
        });
      });
      setState(() => _recording = true);
    } on StateError catch (e) {
      setState(() => _error = e.message);
    }
  }

  Future<void> _share() async {
    if (_fixes.isEmpty) return;
    final dir = await getTemporaryDirectory();
    final ts = DateFormat('yyyyMMdd_HHmmss').format(DateTime.now().toUtc());
    final f = File('${dir.path}/gps_$ts.csv');
    final sink = f.openWrite();
    sink.writeln(GpsFix.csvHeader);
    for (final fix in _fixes) {
      sink.writeln(fix.toCsvRow());
    }
    await sink.flush();
    await sink.close();
    await SharePlus.instance.share(
      ShareParams(
        files: [XFile(f.path, mimeType: 'text/csv')],
        text: 'LoRa GPS trace · ${_fixes.length} fixes · $ts UTC',
        subject: 'gps_$ts.csv',
      ),
    );
  }

  void _clear() => setState(() {
        _fixes.clear();
        _last = null;
      });

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Scaffold(
      appBar: AppBar(
        title: const Text('LoRa GPS Recorder'),
        backgroundColor: scheme.surface,
      ),
      body: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            _StatusCard(
              recording: _recording,
              count: _fixes.length,
              last: _last,
              error: _error,
            ),
            const SizedBox(height: 16),
            Row(
              children: [
                Expanded(
                  child: FilledButton.icon(
                    onPressed: _toggle,
                    icon: Icon(_recording ? Icons.stop : Icons.play_arrow),
                    label: Text(_recording ? 'Stop' : 'Start tracking'),
                    style: FilledButton.styleFrom(
                      padding: const EdgeInsets.symmetric(vertical: 18),
                      backgroundColor: _recording
                          ? scheme.errorContainer
                          : scheme.primaryContainer,
                      foregroundColor: _recording
                          ? scheme.onErrorContainer
                          : scheme.onPrimaryContainer,
                    ),
                  ),
                ),
              ],
            ),
            const SizedBox(height: 8),
            Row(
              children: [
                Expanded(
                  child: OutlinedButton.icon(
                    onPressed: _fixes.isEmpty ? null : _share,
                    icon: const Icon(Icons.ios_share),
                    label: const Text('Share (AirDrop)'),
                  ),
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: OutlinedButton.icon(
                    onPressed: _fixes.isEmpty ? null : _clear,
                    icon: const Icon(Icons.delete_outline),
                    label: const Text('Clear'),
                  ),
                ),
              ],
            ),
            const SizedBox(height: 16),
            const Text('Last 30 fixes (UTC)'),
            const SizedBox(height: 8),
            Expanded(
              child: Card(
                child: ListView.builder(
                  reverse: true,
                  itemCount: _fixes.length,
                  itemBuilder: (ctx, i) {
                    final f = _fixes[_fixes.length - 1 - i];
                    return ListTile(
                      dense: true,
                      title: Text(
                        '${f.lat.toStringAsFixed(5)}, ${f.lon.toStringAsFixed(5)}',
                        style: const TextStyle(fontFamily: 'monospace'),
                      ),
                      subtitle: Text(
                        '${DateFormat.Hms().format(f.timestamp.toLocal())} '
                        '· ±${(f.accuracyM ?? 0).toStringAsFixed(0)}m'
                        '${f.speedMps != null ? " · ${f.speedMps!.toStringAsFixed(1)} m/s" : ""}',
                      ),
                    );
                  },
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _StatusCard extends StatelessWidget {
  const _StatusCard({
    required this.recording,
    required this.count,
    required this.last,
    required this.error,
  });

  final bool recording;
  final int count;
  final GpsFix? last;
  final String? error;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Card(
      color: scheme.surfaceContainerHighest,
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Icon(
                  recording ? Icons.fiber_manual_record : Icons.radio_button_unchecked,
                  color: recording ? Colors.red : scheme.onSurfaceVariant,
                ),
                const SizedBox(width: 8),
                Text(
                  recording ? 'Recording' : 'Idle',
                  style: Theme.of(context).textTheme.titleMedium,
                ),
                const Spacer(),
                Text('$count fixes',
                    style: Theme.of(context).textTheme.bodyMedium),
              ],
            ),
            if (last != null) ...[
              const SizedBox(height: 12),
              Text(
                '${last!.lat.toStringAsFixed(6)}, ${last!.lon.toStringAsFixed(6)}',
                style: const TextStyle(
                    fontSize: 18, fontFamily: 'monospace'),
              ),
              const SizedBox(height: 4),
              Text(
                '±${(last!.accuracyM ?? 0).toStringAsFixed(1)} m · '
                '${last!.altitudeM?.toStringAsFixed(1) ?? "?"} m alt · '
                '${last!.speedMps?.toStringAsFixed(1) ?? "?"} m/s',
                style: TextStyle(color: scheme.onSurfaceVariant),
              ),
            ],
            if (error != null) ...[
              const SizedBox(height: 12),
              Text(error!,
                  style: TextStyle(color: scheme.error)),
            ],
            if (!Platform.isIOS) ...[
              const SizedBox(height: 12),
              Text(
                'Note: this screen is intended for iOS — geolocator may not '
                'be available on this platform.',
                style: TextStyle(color: scheme.onSurfaceVariant),
              ),
            ],
          ],
        ),
      ),
    );
  }
}
