import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'package:usb_serial/usb_serial.dart';

import '../models/lora_event.dart';

/// A USB device the phone can see on its host port.
class UsbDeviceInfo {
  UsbDeviceInfo({
    required this.deviceId,
    required this.name,
    required this.description,
  });

  /// Stable per-attachment id used to re-open the device.
  final int deviceId;
  final String name;
  final String description;
}

/// Reads node_a's CDC stream over the Android USB Host API.
///
/// The DX-SMART board bridges USB-C through a CH340; `usb_serial` bundles
/// usb-serial-for-android, whose CH34x driver claims it automatically, so we
/// don't need to match VID/PID ourselves. Lines are decoded into [LoRaEvent]s
/// and streamed out — the same contract the desktop [SerialService] exposes,
/// so screens can treat the two interchangeably.
///
/// Android-only: on other platforms `usb_serial`'s platform channel is absent
/// and these calls will throw, so only construct this when `Platform.isAndroid`.
class AndroidSerialService {
  UsbPort? _port;
  StreamSubscription<Uint8List>? _sub;
  final _eventCtrl = StreamController<LoRaEvent>.broadcast();
  final _buf = StringBuffer();

  Stream<LoRaEvent> get events => _eventCtrl.stream;

  bool get isOpen => _port != null;

  /// Lists attached USB serial-capable devices. Listing does **not** prompt
  /// for permission — only [open] does, when Android shows its access dialog.
  static Future<List<UsbDeviceInfo>> list() async {
    final devices = await UsbSerial.listDevices();
    // Drop anything without a deviceId — we'd have no handle to re-open it.
    return devices.where((d) => d.deviceId != null).map((d) {
      final product = d.productName;
      final manufacturer = d.manufacturerName;
      String desc;
      if (product != null && product.isNotEmpty) {
        desc = manufacturer != null && manufacturer.isNotEmpty
            ? '$manufacturer · $product'
            : product;
      } else {
        // CH340 clones often report no strings; fall back to VID:PID.
        desc =
            'VID:${d.vid?.toRadixString(16)} PID:${d.pid?.toRadixString(16)}';
      }
      return UsbDeviceInfo(
        deviceId: d.deviceId!,
        // The analyzer resolves UsbDevice.deviceName as non-null, but the
        // build (CFE) treats it as nullable and won't compile without the
        // fallback — trust the compiler and keep it.
        // ignore: dead_null_aware_expression, dead_code
        name: d.deviceName ?? 'usb',
        description: desc,
      );
    }).toList();
  }

  /// Opens [deviceId] at 115200 8N1 and starts streaming events.
  ///
  /// Mirrors the desktop service's framing: no hardware flow control (the
  /// CH340 has no RTS/CTS routed), DTR asserted so auto-reset boards leave
  /// reset. Throws [StateError] if the device vanished or access was denied.
  Future<void> open(int deviceId) async {
    await close();

    final devices = await UsbSerial.listDevices();
    final device = devices.firstWhere(
      (d) => d.deviceId == deviceId,
      orElse: () => throw StateError('USB device $deviceId is no longer attached'),
    );

    // create() auto-detects the driver (CH34x for this board); open() triggers
    // the Android "Allow app to access the USB device?" dialog.
    final port = await device.create();
    if (port == null) {
      throw StateError('Could not create a serial port for the device');
    }
    final opened = await port.open();
    if (!opened) {
      throw StateError('USB permission denied or device busy');
    }

    await port.setDTR(true);
    await port.setRTS(false);
    await port.setPortParameters(
      115200,
      UsbPort.DATABITS_8,
      UsbPort.STOPBITS_1,
      UsbPort.PARITY_NONE,
    );

    _port = port;
    _sub = port.inputStream?.listen(
      _onData,
      onError: (Object e) => _eventCtrl.addError(e),
    );
    if (_sub == null) {
      await close();
      throw StateError('Device opened but exposed no input stream');
    }
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
    await _port?.close();
    _port = null;
    _buf.clear();
  }

  Future<void> dispose() async {
    await close();
    await _eventCtrl.close();
  }
}
