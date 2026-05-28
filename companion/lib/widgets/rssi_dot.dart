import 'package:flutter/material.dart';

/// A small dot whose colour maps RSSI to a red→green scale.
/// −120 dBm (link limit) is red, −50 dBm (strong) is green.
class RssiDot extends StatelessWidget {
  const RssiDot({super.key, required this.rssi, this.size = 14});

  final int rssi;
  final double size;

  static Color colorFor(int rssi) {
    final t = ((rssi + 120) / 70).clamp(0.0, 1.0);
    return Color.lerp(Colors.red.shade700, Colors.green.shade400, t)!;
  }

  @override
  Widget build(BuildContext context) {
    return Container(
      width: size,
      height: size,
      decoration: BoxDecoration(
        color: colorFor(rssi),
        shape: BoxShape.circle,
        border: Border.all(color: Colors.black54, width: 1),
      ),
    );
  }
}
