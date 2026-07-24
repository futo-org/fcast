import 'package:fcast_sender_sdk/fcast_sender_sdk.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('deviceStorageKey', () {
    test('chromecast uses TXT id when present', () {
      final info = DeviceInfo(
        name: 'Our Bedroom',
        protocol: ProtocolType.chromecast,
        addresses: const [],
        port: 8009,
        txtRecords: const {'id': 'abc123', 'fn': 'Our Bedroom'},
      );

      expect(deviceStorageKey(info), 'chromecast:abc123');
    });

    test('fCast falls back to display name', () {
      final info = DeviceInfo(
        name: 'FCast-NVIDIA-SHIELD Android TV',
        protocol: ProtocolType.fCast,
        addresses: const [],
        port: 46899,
        txtRecords: const {'appVersion': '2.1.5'},
      );

      expect(
        deviceStorageKey(info),
        'fCast:FCast-NVIDIA-SHIELD Android TV',
      );
    });
  });
}
