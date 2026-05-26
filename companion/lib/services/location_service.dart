import 'dart:async';

import 'package:flutter/foundation.dart';
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
    // iOS first grants only "When In Use"; calling requestPermission again
    // nudges CoreLocation to show the "Always" upgrade prompt, which keeps
    // background recording alive once the screen locks. Best-effort: if the
    // user keeps "When In Use", background updates still work while the blue
    // indicator is showing, so we don't fail here.
    if (perm == LocationPermission.whileInUse) {
      await Geolocator.requestPermission();
    }
  }

  /// On Apple platforms we must use [AppleSettings] rather than the base
  /// [LocationSettings]: only there can we disable CoreLocation's automatic
  /// pause (which otherwise stops updates whenever iOS decides we're stationary)
  /// and enable background delivery (so recording survives the screen locking
  /// during a range test). The base class leaves both at their stingy defaults.
  LocationSettings _locationSettings() {
    final isApple = defaultTargetPlatform == TargetPlatform.iOS ||
        defaultTargetPlatform == TargetPlatform.macOS;
    if (isApple) {
      return AppleSettings(
        accuracy: LocationAccuracy.bestForNavigation,
        distanceFilter: 0,
        pauseLocationUpdatesAutomatically: false,
        allowBackgroundLocationUpdates: true,
        showBackgroundLocationIndicator: true,
        activityType: ActivityType.otherNavigation,
      );
    }
    return const LocationSettings(
      accuracy: LocationAccuracy.bestForNavigation,
      distanceFilter: 0,
    );
  }

  Future<void> start() async {
    await ensurePermission();
    await _sub?.cancel();
    _sub = Geolocator.getPositionStream(
      locationSettings: _locationSettings(),
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
