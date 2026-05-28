import 'dart:io';

import 'package:flutter/services.dart';

/// Thin wrapper over the native `FLAG_KEEP_SCREEN_ON` toggle on Android.
///
/// We rolled our own instead of using `wakelock_plus` because on the test
/// device (Galaxy S23 FE, Android 16) the package's `addFlags()` call had no
/// observable effect — `dumpsys window` showed the window's flags unchanged
/// after `WakelockPlus.enable()`. Going through our own MethodChannel
/// straight to `getWindow().addFlags(...)` works and gives us a single place
/// to debug if it ever stops working again.
///
/// No-op on non-Android platforms.
class ScreenLock {
  static const _channel =
      MethodChannel('com.lr30.lora_companion/screen_lock');

  /// Keep the screen on (sets `FLAG_KEEP_SCREEN_ON` on the activity window).
  static Future<void> enable() async {
    if (!Platform.isAndroid) return;
    await _channel.invokeMethod<void>('enable');
  }

  /// Release the keep-screen-on flag.
  static Future<void> disable() async {
    if (!Platform.isAndroid) return;
    await _channel.invokeMethod<void>('disable');
  }
}
