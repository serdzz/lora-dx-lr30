import 'package:flutter/material.dart';

import 'map_screen.dart';
import 'usb_capture_screen.dart';

class MacosHomeScreen extends StatelessWidget {
  const MacosHomeScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('LoRa Companion — macOS')),
      body: Center(
        child: ConstrainedBox(
          constraints: const BoxConstraints(maxWidth: 720),
          child: Padding(
            padding: const EdgeInsets.all(24),
            child: Column(
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                _HubCard(
                  icon: Icons.usb,
                  title: 'USB capture · node_b',
                  body:
                      'Read /dev/cu.usbmodem* from node_b (PONG responder). '
                      'Each `rx ping sf=… seq=… rssi=… snr=…` line is timestamped '
                      'and saved to lora.csv.',
                  onTap: () => Navigator.of(context).push(
                    MaterialPageRoute(builder: (_) => const UsbCaptureScreen()),
                  ),
                ),
                const SizedBox(height: 16),
                _HubCard(
                  icon: Icons.map_outlined,
                  title: 'Merge & map',
                  body:
                      'Load lora.csv (saved here) + gps.csv (AirDropped from '
                      'the iPhone) and draw the route coloured by RSSI.',
                  onTap: () => Navigator.of(context).push(
                    MaterialPageRoute(builder: (_) => const MapScreen()),
                  ),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

class _HubCard extends StatelessWidget {
  const _HubCard({
    required this.icon,
    required this.title,
    required this.body,
    required this.onTap,
  });

  final IconData icon;
  final String title;
  final String body;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Card(
      child: InkWell(
        onTap: onTap,
        borderRadius: BorderRadius.circular(12),
        child: Padding(
          padding: const EdgeInsets.all(20),
          child: Row(
            children: [
              Container(
                width: 56,
                height: 56,
                decoration: BoxDecoration(
                  color: scheme.primaryContainer,
                  borderRadius: BorderRadius.circular(12),
                ),
                child: Icon(icon, size: 32, color: scheme.onPrimaryContainer),
              ),
              const SizedBox(width: 16),
              Expanded(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Text(title, style: Theme.of(context).textTheme.titleLarge),
                    const SizedBox(height: 4),
                    Text(body,
                        style: Theme.of(context).textTheme.bodyMedium),
                  ],
                ),
              ),
              const Icon(Icons.chevron_right),
            ],
          ),
        ),
      ),
    );
  }
}
