import 'dart:async';

import 'package:geolocator/geolocator.dart';

import '../models/gps_fix.dart';

class LocationService {
  StreamSubscription<Position>? _sub;
  final _fixCtrl = StreamController<GpsFix>.broadcast();

  Stream<GpsFix> get fixes => _fixCtrl.stream;

  Future<void> ensurePermission() async {
    if (!await Geolocator.isLocationServiceEnabled()) {
      throw StateError('Location services are disabled');
    }
    var perm = await Geolocator.checkPermission();
    if (perm == LocationPermission.denied) {
      perm = await Geolocator.requestPermission();
    }
    if (perm == LocationPermission.deniedForever ||
        perm == LocationPermission.denied) {
      throw StateError('Location permission was denied');
    }
  }

  Future<void> start() async {
    await ensurePermission();
    await _sub?.cancel();
    _sub = Geolocator.getPositionStream(
      locationSettings: const LocationSettings(
        accuracy: LocationAccuracy.bestForNavigation,
        distanceFilter: 0,
      ),
    ).listen((p) {
      _fixCtrl.add(
        GpsFix(
          timestamp: p.timestamp.toUtc(),
          lat: p.latitude,
          lon: p.longitude,
          accuracyM: p.accuracy,
          altitudeM: p.altitude,
          speedMps: p.speed,
          headingDeg: p.heading,
        ),
      );
    }, onError: (e) => _fixCtrl.addError(e));
  }

  Future<void> stop() async {
    await _sub?.cancel();
    _sub = null;
  }

  Future<void> dispose() async {
    await stop();
    await _fixCtrl.close();
  }
}
