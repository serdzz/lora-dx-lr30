import 'dart:async';
import 'dart:io';

import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';
import 'package:intl/intl.dart';

import '../models/lora_event.dart';
import '../services/serial_service.dart';

class UsbCaptureScreen extends StatefulWidget {
  const UsbCaptureScreen({super.key});

  @override
  State<UsbCaptureScreen> createState() => _UsbCaptureScreenState();
}

class _UsbCaptureScreenState extends State<UsbCaptureScreen> {
  final _svc = SerialService();
  final _events = <LoRaEvent>[];
  StreamSubscription<LoRaEvent>? _sub;
  List<SerialPortInfo> _ports = const [];
  SerialPortInfo? _selected;
  String? _error;
  bool _capturing = false;

  @override
  void initState() {
    super.initState();
    _refresh();
  }

  @override
  void dispose() {
    _sub?.cancel();
    _svc.dispose();
    super.dispose();
  }

  void _refresh() {
    final ports = SerialService.list();
    setState(() {
      _ports = ports;
      // The DX-SMART board enumerates as `wchusbserial*` via its on-board CH340.
      // Fall back to native CDC (`usbmodem*`) and to the first port as a last resort.
      _selected ??= ports.firstWhere(
        (p) => p.name.contains('wchusbserial') || p.name.contains('usbserial'),
        orElse: () => ports.firstWhere(
          (p) => p.name.contains('usbmodem'),
          orElse: () => ports.isNotEmpty
              ? ports.first
              : SerialPortInfo(name: '', description: ''),
        ),
      );
      if (_selected?.name.isEmpty ?? true) _selected = null;
    });
  }

  Future<void> _toggle() async {
    if (_capturing) {
      await _svc.close();
      await _sub?.cancel();
      _sub = null;
      setState(() => _capturing = false);
      return;
    }
    final sel = _selected;
    if (sel == null) return;
    try {
      setState(() => _error = null);
      await _svc.open(sel.name);
      _sub = _svc.events.listen(
        (ev) {
          if (!mounted) return;
          setState(() => _events.add(ev));
        },
        onError: (e) {
          if (!mounted) return;
          setState(() => _error = e.toString());
        },
      );
      setState(() => _capturing = true);
    } catch (e) {
      setState(() => _error = e.toString());
    }
  }

  Future<void> _save() async {
    if (_events.isEmpty) return;
    final ts = DateFormat('yyyyMMdd_HHmmss').format(DateTime.now().toUtc());
    final location = await getSaveLocation(
      suggestedName: 'lora_$ts.csv',
      acceptedTypeGroups: [
        const XTypeGroup(label: 'CSV', extensions: ['csv']),
      ],
    );
    if (location == null) return;
    final f = File(location.path);
    final sink = f.openWrite();
    sink.writeln(LoRaEvent.csvHeader);
    for (final ev in _events) {
      sink.writeln(ev.toCsvRow());
    }
    await sink.flush();
    await sink.close();
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(content: Text('Saved ${_events.length} events → ${f.path}')),
    );
  }

  void _clear() => setState(() => _events.clear());

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final hits = _events.where((e) => e.kind == LoRaEventKind.hit).length;
    final misses = _events.where((e) => e.kind == LoRaEventKind.miss).length;

    return Scaffold(
      appBar: AppBar(title: const Text('USB capture · node_b')),
      body: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Row(
              children: [
                Expanded(
                  child: DropdownButtonFormField<SerialPortInfo>(
                    decoration: const InputDecoration(
                      labelText: 'Serial port',
                      border: OutlineInputBorder(),
                    ),
                    value: _selected,
                    items: _ports
                        .map((p) => DropdownMenuItem(
                              value: p,
                              child: Text('${p.name}  ·  ${p.description}'),
                            ))
                        .toList(),
                    onChanged: _capturing
                        ? null
                        : (v) => setState(() => _selected = v),
                  ),
                ),
                const SizedBox(width: 8),
                IconButton.filled(
                  onPressed: _capturing ? null : _refresh,
                  icon: const Icon(Icons.refresh),
                  tooltip: 'Re-scan ports',
                ),
              ],
            ),
            const SizedBox(height: 12),
            Row(
              children: [
                Expanded(
                  child: FilledButton.icon(
                    onPressed: _selected == null ? null : _toggle,
                    icon: Icon(_capturing ? Icons.stop : Icons.play_arrow),
                    label: Text(_capturing ? 'Stop' : 'Open & capture'),
                    style: FilledButton.styleFrom(
                      backgroundColor: _capturing
                          ? scheme.errorContainer
                          : scheme.primaryContainer,
                      foregroundColor: _capturing
                          ? scheme.onErrorContainer
                          : scheme.onPrimaryContainer,
                      padding: const EdgeInsets.symmetric(vertical: 16),
                    ),
                  ),
                ),
                const SizedBox(width: 8),
                OutlinedButton.icon(
                  onPressed: _events.isEmpty ? null : _save,
                  icon: const Icon(Icons.save_alt),
                  label: const Text('Save CSV'),
                ),
                const SizedBox(width: 8),
                OutlinedButton.icon(
                  onPressed: _events.isEmpty ? null : _clear,
                  icon: const Icon(Icons.delete_outline),
                  label: const Text('Clear'),
                ),
              ],
            ),
            const SizedBox(height: 12),
            Row(
              children: [
                _Pill(label: 'total', value: _events.length.toString()),
                const SizedBox(width: 8),
                _Pill(label: 'hits', value: hits.toString(),
                    color: Colors.green.shade700),
                const SizedBox(width: 8),
                _Pill(label: 'miss', value: misses.toString(),
                    color: Colors.orange.shade700),
              ],
            ),
            if (_error != null) ...[
              const SizedBox(height: 8),
              Text(_error!, style: TextStyle(color: scheme.error)),
            ],
            const SizedBox(height: 12),
            Expanded(
              child: Card(
                child: ListView.builder(
                  reverse: true,
                  itemCount: _events.length,
                  itemBuilder: (ctx, i) {
                    final e = _events[_events.length - 1 - i];
                    final color = switch (e.kind) {
                      LoRaEventKind.hit => Colors.green.shade400,
                      LoRaEventKind.miss => Colors.orange.shade400,
                      LoRaEventKind.info => scheme.onSurfaceVariant,
                    };
                    return ListTile(
                      dense: true,
                      leading: CircleAvatar(
                        radius: 8,
                        backgroundColor: color,
                      ),
                      title: Text(
                        e.raw,
                        style: const TextStyle(
                            fontFamily: 'monospace', fontSize: 13),
                      ),
                      subtitle: Text(
                        DateFormat('HH:mm:ss.SSS')
                            .format(e.timestamp.toLocal()),
                        style: const TextStyle(fontSize: 11),
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

class _Pill extends StatelessWidget {
  const _Pill({required this.label, required this.value, this.color});
  final String label;
  final String value;
  final Color? color;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 4),
      decoration: BoxDecoration(
        color: (color ?? scheme.surfaceContainerHighest).withOpacity(0.35),
        borderRadius: BorderRadius.circular(20),
        border: Border.all(color: color ?? scheme.outline),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          Text(label,
              style: TextStyle(
                  color: color ?? scheme.onSurfaceVariant,
                  fontSize: 12)),
          const SizedBox(width: 6),
          Text(value,
              style: const TextStyle(
                  fontWeight: FontWeight.bold, fontSize: 13)),
        ],
      ),
    );
  }
}
