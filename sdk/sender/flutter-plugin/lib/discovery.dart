import 'dart:async';
import 'dart:typed_data';
import 'dart:io' show InternetAddress, InternetAddressType;

import 'package:bonsoir/bonsoir.dart';
import 'package:fcast_sender_sdk/fcast_sender_sdk.dart';

IpAddr? _internetAddressToIpAddr(InternetAddress addr) {
  Uint8List octets = addr.rawAddress;
  switch (addr.type) {
    case InternetAddressType.IPv4:
      if (octets.length != 4) {
        return null;
      }
      return IpAddr.v4(
        o1: octets[0],
        o2: octets[1],
        o3: octets[2],
        o4: octets[3],
      );
    case InternetAddressType.IPv6:
      if (octets.length != 16) {
        return null;
      }
      return IpAddr.v6(
        o1: octets[0],
        o2: octets[1],
        o3: octets[2],
        o4: octets[3],
        o5: octets[4],
        o6: octets[5],
        o7: octets[6],
        o8: octets[7],
        o9: octets[8],
        o10: octets[9],
        o11: octets[10],
        o12: octets[11],
        o13: octets[12],
        o14: octets[13],
        o15: octets[14],
        o16: octets[15],
        scopeId: 0, // TODO: get this as well
      );
    default:
      return null;
  }
}

List<IpAddr> _convertHostAddresses(List<String> hostAddresses) {
  return hostAddresses
      .map(InternetAddress.tryParse)
      .whereType<InternetAddress>()
      .map(_internetAddressToIpAddr)
      .whereType<IpAddr>()
      .toList();
}

class DiscoveryEvent {}

class DiscoveryEventDeviceAdded extends DiscoveryEvent {
  final DeviceInfo deviceInfo;
  final int? gcastCaps;

  DiscoveryEventDeviceAdded({
    required this.deviceInfo,
    required this.gcastCaps,
  });
}

class DiscoveryEventDeviceUpdated extends DiscoveryEvent {
  final DeviceInfo deviceInfo;
  final int? gcastCaps;

  DiscoveryEventDeviceUpdated({
    required this.deviceInfo,
    required this.gcastCaps,
  });
}

class DiscoveryEventDeviceRemoved extends DiscoveryEvent {
  final String name;

  DiscoveryEventDeviceRemoved({required this.name});
}

class DeviceDiscoverer {
  final BonsoirDiscovery _fcastDiscovery = BonsoirDiscovery(
    type: '_fcast._tcp',
  );
  final BonsoirDiscovery _chromecastDiscovery = BonsoirDiscovery(
    type: '_googlecast._tcp',
  );
  final eventStreamController = StreamController();
  final Set<String> _seenDevices = {};

  DeviceDiscoverer();

  void _deviceFoundOrUpdated(DeviceInfo deviceInfo, int? gcastCaps) {
    if (_seenDevices.add(deviceInfo.name)) {
      // Not seen
      eventStreamController.sink.add(
        DiscoveryEventDeviceAdded(deviceInfo: deviceInfo, gcastCaps: gcastCaps),
      );
    } else {
      eventStreamController.sink.add(
        DiscoveryEventDeviceUpdated(
          deviceInfo: deviceInfo,
          gcastCaps: gcastCaps,
        ),
      );
    }
  }

  void _deviceRemoved(String name) {
    _seenDevices.remove(name);
    eventStreamController.sink.add(DiscoveryEventDeviceRemoved(name: name));
  }

  Future<DeviceInfo?> _makeFcastDeviceInfo(BonsoirService service) async {
    if (service.hostAddresses.isNotEmpty) {
      List<IpAddr> addrs = _convertHostAddresses(service.hostAddresses);
      DeviceInfo deviceInfo = DeviceInfo(
        name: service.name,
        protocol: ProtocolType.fCast,
        addresses: addrs,
        port: service.port,
        txtRecords: service.attributes,
      );
      return deviceInfo;
    }
    return null;
  }

  Future<(DeviceInfo, int?)?> _makeChromecastDeviceInfo(
    BonsoirService service,
  ) async {
    if (service.hostAddresses.isNotEmpty) {
      List<IpAddr> addrs = _convertHostAddresses(service.hostAddresses);
      // NOTE: fn = friendly name
      String name = service.attributes['fn'] ?? service.name;
      DeviceInfo deviceInfo = DeviceInfo(
        name: name,
        protocol: ProtocolType.chromecast,
        addresses: addrs,
        port: service.port,
        txtRecords: service.attributes,
      );
      String? capsStr = service.attributes["ca"];
      int? caps = capsStr != null ? int.tryParse(capsStr) : null;
      return (deviceInfo, caps);
    }
    return null;
  }

  Future<void> init() async {
    await _fcastDiscovery.initialize();
    await _chromecastDiscovery.initialize();

    _fcastDiscovery.eventStream!.listen((event) async {
      switch (event) {
        case BonsoirDiscoveryServiceFoundEvent():
          event.service.resolve(_fcastDiscovery.serviceResolver);
          break;
        case BonsoirDiscoveryServiceResolvedEvent():
          DeviceInfo? deviceInfo = await _makeFcastDeviceInfo(event.service);
          if (deviceInfo != null) {
            _deviceFoundOrUpdated(deviceInfo, null);
          }
          break;
        case BonsoirDiscoveryServiceUpdatedEvent():
          DeviceInfo? deviceInfo = await _makeFcastDeviceInfo(event.service);
          if (deviceInfo != null) {
            _deviceFoundOrUpdated(deviceInfo, null);
          }
          break;
        case BonsoirDiscoveryServiceLostEvent():
          _deviceRemoved(event.service.name);
          break;
        default:
          break;
      }
    });
    _chromecastDiscovery.eventStream!.listen((event) async {
      switch (event) {
        case BonsoirDiscoveryServiceFoundEvent():
          event.service.resolve(_chromecastDiscovery.serviceResolver);
          break;
        case BonsoirDiscoveryServiceResolvedEvent():
          (DeviceInfo, int?)? service = await _makeChromecastDeviceInfo(
            event.service,
          );
          if (service != null) {
            _deviceFoundOrUpdated(service.$1, service.$2);
          }
          break;
        case BonsoirDiscoveryServiceUpdatedEvent():
          (DeviceInfo, int?)? service = await _makeChromecastDeviceInfo(
            event.service,
          );
          if (service != null) {
            _deviceFoundOrUpdated(service.$1, service.$2);
          }
          break;
        case BonsoirDiscoveryServiceLostEvent():
          _deviceRemoved(event.service.name);
          break;
        default:
          break;
      }
    });

    await _fcastDiscovery.start();
    await _chromecastDiscovery.start();
  }
}
