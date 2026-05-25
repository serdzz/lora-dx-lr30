import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:flutter_libserialport/flutter_libserialport.dart';

import '../models/lora_event.dart';

class SerialPortInfo {
  SerialPortInfo({required this.name, required this.description});
  final String name;
  final String description;
}

/// Lists, opens and reads a USB CDC-ACM serial port. Decodes lines as
/// `LoRaEvent` and streams them out. macOS only — flutter_libserialport
/// doesn't ship an iOS implementation (Apple restricts USB serial access).
class SerialService {
  SerialPort? _port;
  SerialPortReader? _reader;
  StreamSubscription<Uint8List>? _sub;
  final _eventCtrl = StreamController<LoRaEvent>.broadcast();
  final _buf = StringBuffer();

  Stream<LoRaEvent> get events => _eventCtrl.stream;

  bool get isOpen => _port?.isOpen ?? false;

  static List<SerialPortInfo> list() {
    final names = SerialPort.availablePorts;
    return names.map((n) {
      String desc = n;
      try {
        final p = SerialPort(n);
        final product = p.productName;
        final manufacturer = p.manufacturer;
        p.dispose();
        if (product != null && product.isNotEmpty) {
          desc = manufacturer != null && manufacturer.isNotEmpty
              ? '$manufacturer · $product'
              : product;
        }
      } catch (_) {
        // ignore — keep the raw name
      }
      return SerialPortInfo(name: n, description: desc);
    }).toList();
  }

  Future<void> open(String name) async {
    await close();
    final p = SerialPort(name);
    // No hardware flow control: the DX-SMART board's CH340 doesn't have RTS/CTS
    // or DTR/DSR routed anywhere. Leaving them at `flowControl` makes
    // libserialport stall reads forever waiting on CTS/DSR to assert. Keep
    // DTR ON so CH340-based boards that level-shift DTR for "auto-reset"
    // (Arduino-style) at least don't sit in reset.
    final cfg = SerialPortConfig()
      ..baudRate = 115200
      ..bits = 8
      ..stopBits = 1
      ..parity = SerialPortParity.none
      ..xonXoff = SerialPortXonXoff.disabled
      ..rts = SerialPortRts.off
      ..cts = SerialPortCts.ignore
      ..dtr = SerialPortDtr.on
      ..dsr = SerialPortDsr.ignore;
    if (!p.openReadWrite()) {
      cfg.dispose();
      throw StateError(SerialPort.lastError?.toString() ?? 'openReadWrite failed');
    }
    p.config = cfg;
    cfg.dispose();
    _port = p;

    final reader = SerialPortReader(p, timeout: 0);
    _reader = reader;
    _sub = reader.stream.listen(
      _onData,
      onError: (e) {
        _eventCtrl.addError(e);
      },
    );
  }

  void _onData(Uint8List data) {
    _buf.write(utf8.decode(data, allowMalformed: true));
    var s = _buf.toString();
    while (true) {
      final idx = s.indexOf('\n');
      if (idx < 0) break;
      final line = s.substring(0, idx).replaceAll('\r', '');
      s = s.substring(idx + 1);
      final ev = LoRaEvent.parse(line);
      if (ev != null) _eventCtrl.add(ev);
    }
    _buf
      ..clear()
      ..write(s);
  }

  Future<void> close() async {
    await _sub?.cancel();
    _sub = null;
    _reader?.close();
    _reader = null;
    if (_port?.isOpen ?? false) {
      _port!.close();
    }
    _port?.dispose();
    _port = null;
    _buf.clear();
  }

  Future<void> dispose() async {
    await close();
    await _eventCtrl.close();
  }
}
