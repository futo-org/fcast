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

/// Stable key for deduplicating discovered devices and matching removals.
///
/// Chromecast receivers use the TXT `id` field (hardware id). FCast receivers
/// use the display / mDNS name. A protocol prefix prevents collisions when the
/// same friendly name appears on different stacks.
String deviceStorageKey(DeviceInfo info) {
  final txtId = info.txtRecords['id'];
  if (txtId != null && txtId.isNotEmpty) {
    return '${info.protocol.name}:$txtId';
  }
  return '${info.protocol.name}:${info.name}';
}

/// Resolves a Bonsoir service to IP addresses.
///
/// Bonsoir 7 populates [BonsoirService.hostAddresses] after a native resolve.
/// When that resolve fails partially (common on macOS under concurrent
/// resolves), fall back to [BonsoirService.hostname] + [InternetAddress.lookup]
/// — the approach used by fcast_sender_sdk 0.0.3.
Future<List<IpAddr>> _lookupHostAddresses(BonsoirService service) async {
  if (service.hostAddresses.isNotEmpty) {
    final addrs = _convertHostAddresses(service.hostAddresses);
    if (addrs.isNotEmpty) {
      return addrs;
    }
  }

  final hostname = service.hostname;
  if (hostname != null && hostname.isNotEmpty) {
    final lookedUp = await InternetAddress.lookup(hostname);
    return lookedUp.map(_internetAddressToIpAddr).whereType<IpAddr>().toList();
  }

  return const [];
}

/// One pending mDNS resolve job for the serialized resolve queue.
class _PendingResolve {
  _PendingResolve({
    required this.protocol,
    required this.service,
    required this.resolver,
    this.attempt = 1,
  });

  final String protocol;
  final BonsoirService service;
  final ServiceResolver resolver;
  final int attempt;
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
  /// Storage key from [deviceStorageKey], not the raw mDNS instance name.
  final String name;

  DiscoveryEventDeviceRemoved({required this.name});
}

class DeviceDiscoverer {
  static const _maxResolveAttempts = 4;

  final BonsoirDiscovery _fcastDiscovery = BonsoirDiscovery(
    type: '_fcast._tcp',
  );
  final BonsoirDiscovery _chromecastDiscovery = BonsoirDiscovery(
    type: '_googlecast._tcp',
  );
  final eventStreamController = StreamController();

  // Dedupe by stable storage key instead of Chromecast friendly name alone.
  final Set<String> _seenStorageKeys = {};

  // Map mDNS instance name -> storage key so ServiceLost matches ServiceFound.
  final Map<String, String> _mdnsNameToStorageKey = {};

  // Serialized resolve queue: macOS DNSServiceResolve is unreliable when
  // several Bonsoir resolves run concurrently across both browsers.
  final Map<String, BonsoirService> _unresolvedByMdns = {};
  final Map<String, int> _resolveAttemptsByMdns = {};
  final List<_PendingResolve> _resolveQueue = [];
  _PendingResolve? _inFlightResolve;
  Timer? _resolveRetryTimer;

  DeviceDiscoverer();

  void _deviceFoundOrUpdated(
    String mdnsServiceName,
    DeviceInfo deviceInfo,
    int? gcastCaps,
  ) {
    final storageKey = deviceStorageKey(deviceInfo);
    _mdnsNameToStorageKey[mdnsServiceName] = storageKey;
    _unresolvedByMdns.remove(mdnsServiceName);
    _resolveAttemptsByMdns.remove(mdnsServiceName);

    if (_seenStorageKeys.add(storageKey)) {
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

  void _deviceRemoved(String mdnsServiceName) {
    final storageKey = _mdnsNameToStorageKey.remove(mdnsServiceName);
    _unresolvedByMdns.remove(mdnsServiceName);
    _resolveAttemptsByMdns.remove(mdnsServiceName);
    if (storageKey == null) {
      return;
    }
    _seenStorageKeys.remove(storageKey);
    eventStreamController.sink.add(
      DiscoveryEventDeviceRemoved(name: storageKey),
    );
  }

  void _enqueueResolve({
    required String protocol,
    required BonsoirService service,
    required ServiceResolver resolver,
    int attempt = 1,
  }) {
    if (service.hostAddresses.isNotEmpty) {
      return;
    }
    if (_inFlightResolve?.service.name == service.name ||
        _resolveQueue.any((job) => job.service.name == service.name)) {
      return;
    }

    _unresolvedByMdns[service.name] = service;
    _resolveAttemptsByMdns[service.name] = attempt;
    _resolveQueue.add(
      _PendingResolve(
        protocol: protocol,
        service: service,
        resolver: resolver,
        attempt: attempt,
      ),
    );
    _pumpResolveQueue();
  }

  void _pumpResolveQueue() {
    if (_inFlightResolve != null || _resolveQueue.isEmpty) {
      return;
    }
    final job = _resolveQueue.removeAt(0);
    _inFlightResolve = job;
    unawaited(job.service.resolve(job.resolver));
  }

  void _completeInFlightResolve({String? mdnsName}) {
    final inFlight = _inFlightResolve;
    if (inFlight == null) {
      return;
    }
    if (mdnsName != null && inFlight.service.name != mdnsName) {
      return;
    }
    _inFlightResolve = null;
    _pumpResolveQueue();
  }

  void _retryResolve(String protocol, String mdnsName) {
    final service = _unresolvedByMdns[mdnsName];
    final resolver = protocol == 'chromecast'
        ? _chromecastDiscovery.serviceResolver
        : _fcastDiscovery.serviceResolver;
    if (service == null) {
      return;
    }

    final nextAttempt = (_resolveAttemptsByMdns[mdnsName] ?? 0) + 1;
    if (nextAttempt > _maxResolveAttempts) {
      _unresolvedByMdns.remove(mdnsName);
      _resolveAttemptsByMdns.remove(mdnsName);
      return;
    }

    _resolveRetryTimer?.cancel();
    _resolveRetryTimer = Timer(
      Duration(milliseconds: 250 * nextAttempt),
      () => _enqueueResolve(
        protocol: protocol,
        service: service,
        resolver: resolver,
        attempt: nextAttempt,
      ),
    );
  }

  void _handleResolveFailed(String protocol, String? mdnsName) {
    final targetName = mdnsName ?? _inFlightResolve?.service.name;
    _completeInFlightResolve(mdnsName: targetName);
    if (targetName != null) {
      _retryResolve(protocol, targetName);
    }
  }

  Future<void> _handleFcastEvent(BonsoirDiscoveryEvent event) async {
    switch (event) {
      case BonsoirDiscoveryServiceFoundEvent():
        _enqueueResolve(
          protocol: 'fCast',
          service: event.service,
          resolver: _fcastDiscovery.serviceResolver,
        );
      case BonsoirDiscoveryServiceResolvedEvent():
        _completeInFlightResolve(mdnsName: event.service.name);
        final deviceInfo = await _makeFcastDeviceInfo(event.service);
        if (deviceInfo != null) {
          _deviceFoundOrUpdated(event.service.name, deviceInfo, null);
        }
      case BonsoirDiscoveryServiceUpdatedEvent():
        final deviceInfo = await _makeFcastDeviceInfo(event.service);
        if (deviceInfo != null) {
          _deviceFoundOrUpdated(event.service.name, deviceInfo, null);
        }
      case BonsoirDiscoveryServiceResolveFailedEvent():
        _handleResolveFailed('fCast', event.service?.name);
      case BonsoirDiscoveryServiceLostEvent():
        _deviceRemoved(event.service.name);
      default:
        break;
    }
  }

  Future<void> _handleChromecastEvent(BonsoirDiscoveryEvent event) async {
    switch (event) {
      case BonsoirDiscoveryServiceFoundEvent():
        _enqueueResolve(
          protocol: 'chromecast',
          service: event.service,
          resolver: _chromecastDiscovery.serviceResolver,
        );
      case BonsoirDiscoveryServiceResolvedEvent():
        _completeInFlightResolve(mdnsName: event.service.name);
        final service = await _makeChromecastDeviceInfo(event.service);
        if (service != null) {
          _deviceFoundOrUpdated(event.service.name, service.$1, service.$2);
        }
      case BonsoirDiscoveryServiceUpdatedEvent():
        final service = await _makeChromecastDeviceInfo(event.service);
        if (service != null) {
          _deviceFoundOrUpdated(event.service.name, service.$1, service.$2);
        }
      case BonsoirDiscoveryServiceResolveFailedEvent():
        _handleResolveFailed('chromecast', event.service?.name);
      case BonsoirDiscoveryServiceLostEvent():
        _deviceRemoved(event.service.name);
      default:
        break;
    }
  }

  Future<DeviceInfo?> _makeFcastDeviceInfo(BonsoirService service) async {
    final addrs = await _lookupHostAddresses(service);
    if (addrs.isEmpty) {
      return null;
    }
    return DeviceInfo(
      name: service.name,
      protocol: ProtocolType.fCast,
      addresses: addrs,
      port: service.port,
      txtRecords: service.attributes,
    );
  }

  Future<(DeviceInfo, int?)?> _makeChromecastDeviceInfo(
    BonsoirService service,
  ) async {
    final addrs = await _lookupHostAddresses(service);
    if (addrs.isEmpty) {
      return null;
    }
    // fn = friendly display name advertised in Chromecast TXT records.
    final name = service.attributes['fn'] ?? service.name;
    final capsStr = service.attributes['ca'];
    final caps = capsStr != null ? int.tryParse(capsStr) : null;
    return (
      DeviceInfo(
        name: name,
        protocol: ProtocolType.chromecast,
        addresses: addrs,
        port: service.port,
        txtRecords: service.attributes,
      ),
      caps,
    );
  }

  Future<void> init() async {
    // Chromecast first: on macOS, FCast resolves were starving Chromecast
    // browse when both browsers started simultaneously.
    await _chromecastDiscovery.initialize();
    await _fcastDiscovery.initialize();

    _chromecastDiscovery.eventStream!.listen((event) async {
      await _handleChromecastEvent(event);
    });
    _fcastDiscovery.eventStream!.listen((event) async {
      await _handleFcastEvent(event);
    });

    await _chromecastDiscovery.start();
    await _fcastDiscovery.start();
  }
}
