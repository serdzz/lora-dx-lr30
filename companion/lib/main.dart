import 'dart:io' show Platform;

import 'package:flutter/material.dart';

import 'screens/gps_recorder_screen.dart';
import 'screens/macos_home_screen.dart';

void main() {
  runApp(const LoRaCompanionApp());
}

class LoRaCompanionApp extends StatelessWidget {
  const LoRaCompanionApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'LoRa Companion',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(
        useMaterial3: true,
        colorScheme: ColorScheme.fromSeed(
          seedColor: const Color(0xFF1E88E5),
          brightness: Brightness.dark,
        ),
      ),
      home: Platform.isIOS
          ? const GpsRecorderScreen()
          : const MacosHomeScreen(),
    );
  }
}
