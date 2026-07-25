import 'package:fcast_sender_sdk/fcast_sender_sdk.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('deviceStorageKey', () {
    test('chromecast uses TXT id when present', () {
      final info = DeviceInfo(
        name: 'Cast Receiver 1',
        protocol: ProtocolType.chromecast,
        addresses: const [],
        port: 8009,
        txtRecords: const {'id': 'device-id-001', 'fn': 'Cast Receiver 1'},
      );

      expect(deviceStorageKey(info), 'chromecast:device-id-001');
    });

    test('fCast falls back to display name', () {
      final info = DeviceInfo(
        name: 'FCast Receiver 1',
        protocol: ProtocolType.fCast,
        addresses: const [],
        port: 46899,
        txtRecords: const {'appVersion': '2.1.5'},
      );

      expect(
        deviceStorageKey(info),
        'fCast:FCast Receiver 1',
      );
    });
  });
}
