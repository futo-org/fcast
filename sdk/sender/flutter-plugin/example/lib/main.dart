import 'package:flutter/material.dart';
import 'package:fcast_sender_sdk/fcast_sender_sdk.dart';

Future<void> main() async {
  await FCastSenderSdkLib.init();
  initLogger(); // Intended only for debug builds
  runApp(const MyApp());
}

class MyApp extends StatelessWidget {
  const MyApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(home: _Scaffold());
  }
}

class _Scaffold extends StatefulWidget {
  @override
  State<StatefulWidget> createState() => _ScaffoldState();
}

class _ScaffoldState extends State<_Scaffold> {
  final CastContext castContext = CastContext();
  CastingDevice? activeDevice;
  int currentDeviceGeneration = 0;
  final Map<String, DeviceInfo> discoveredDevices = Map.fromEntries([
    // Debugging
    // MapEntry(
    //   "FCast-test",
    //   DeviceInfo(
    //     name: "FCast-test",
    //     protocol: ProtocolType.fCast,
    //     addresses: [IpAddr.v4(o1: 127, o2: 0, o3: 0, o4: 1)],
    //     port: 46899,
    //   ),
    // ),
  ]);
  final DeviceDiscoverer discoverer = DeviceDiscoverer();
  double currentVolume = 1.0;
  double currentTime = 0.0;
  double duration = 0.0;

  Future<void> startDiscovery() async {
    await discoverer.init();
    discoverer.eventStreamController.stream.listen((event) {
      print('discovery event: $event');
      switch (event) {
        case DiscoveryEventDeviceAdded():
          setState(() {
            discoveredDevices[event.deviceInfo.name] = event.deviceInfo;
          });
          break;
        case DiscoveryEventDeviceUpdated():
          setState(() {
            discoveredDevices[event.deviceInfo.name] = event.deviceInfo;
          });
          break;
        case DiscoveryEventDeviceRemoved():
          setState(() {
            discoveredDevices.remove(event.name);
          });
          break;
        default:
          break;
      }
    });
  }

  @override
  void initState() {
    super.initState();
    startDiscovery();
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: activeDevice == null
        ? AppBar(title: const Text('Connect to your receiver'))
        : null,
    body: SafeArea(
      child: activeDevice == null
          ? Center(
              child: ListView(
                children: discoveredDevices.entries
                    .map(
                      (entry) => InkWell(
                        onTap: () {
                          CastingDevice device = castContext
                              .createDeviceFromInfo(info: entry.value);
                          final thisDeviceGeneration =
                              ++currentDeviceGeneration;
                          device.connect(
                            eventHandler: DeviceEventHandler(
                              onEvent: (event) {
                                if (thisDeviceGeneration !=
                                    currentDeviceGeneration) {
                                  return;
                                }
                                print('Device event: $event');
                                switch (event) {
                                  case DeviceEvent_ConnectionStateChanged():
                                    break;
                                  case DeviceEvent_VolumeChanged():
                                    setState(() {
                                      currentVolume = event.newVolume;
                                    });
                                    break;
                                  case DeviceEvent_TimeChanged():
                                    setState(() {
                                      currentTime = event.newTime;
                                    });
                                    break;
                                  case DeviceEvent_PlaybackStateChanged():
                                    break;
                                  case DeviceEvent_DurationChanged():
                                    setState(() {
                                      duration = event.newDuration;
                                    });
                                    break;
                                  case DeviceEvent_SpeedChanged():
                                    break;
                                  case DeviceEvent_SourceChanged():
                                    break;
                                  case DeviceEvent_KeyEvent():
                                    break;
                                  case DeviceEvent_MediaEvent():
                                    break;
                                  case DeviceEvent_TracksAvailable():
                                    break;
                                  case DeviceEvent_TrackSelected():
                                    break;
                                  case DeviceEvent_PlaybackError():
                                    break;
                                }
                              },
                            ),
                            reconnectIntervalMillis: 1000,
                          );
                          setState(() {
                            activeDevice = device;
                          });
                        },
                        child: SizedBox(
                          height: 50,
                          child: Center(child: Text(entry.value.name)),
                        ),
                      ),
                    )
                    .toList(),
              ),
            )
          : Center(
              child: Column(
                children: [
                  ElevatedButton(
                    onPressed: () {
                      try {
                        activeDevice?.disconnect();
                      } catch (e) {
                        print('Failed to disconnect from device: $e');
                      }
                      setState(() {
                        activeDevice = null;
                      });
                    },
                    child: Text('Disconnect'),
                  ),
                  ElevatedButton(
                    onPressed: () {
                      try {
                        activeDevice?.load(
                          request: LoadRequest_Video(
                            contentType: 'video/mp4',
                            url:
                                'http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4',
                            resumePosition: 0.0,
                          ),
                        );
                      } catch (e) {
                        print('Failed to load video: $e');
                      }
                    },
                    child: Text('Play demo'),
                  ),
                  ElevatedButton(
                    onPressed: () {
                      try {
                        activeDevice?.pausePlayback();
                      } catch (e) {
                        print('Failed to pause playback: $e');
                      }
                    },
                    child: Text('Pause'),
                  ),
                  ElevatedButton(
                    onPressed: () {
                      try {
                        activeDevice?.resumePlayback();
                      } catch (e) {
                        print('Failed to resume playback: $e');
                      }
                    },
                    child: Text('Resume'),
                  ),
                  ElevatedButton(
                    onPressed: () {
                      try {
                        activeDevice?.stopPlayback();
                      } catch (e) {
                        print('Failed to stop playback: $e');
                      }
                    },
                    child: Text('Stop playback'),
                  ),
                  Text("Volume:"),
                  Slider(
                    value: currentVolume,
                    min: 0.0,
                    max: 1.0,
                    onChanged: (double newVolume) {
                      try {
                        activeDevice?.changeVolume(volume: newVolume);
                        setState(() {
                          currentVolume = newVolume;
                        });
                      } catch (e) {
                        print('Failed to change volume: $e');
                      }
                    },
                  ),
                  Text("Position:"),
                  Slider(
                    value: currentTime,
                    min: 0.0,
                    max: duration,
                    onChanged: (double newTime) {
                      try {
                        activeDevice?.seek(timeSeconds: newTime);
                        setState(() {
                          currentTime = newTime;
                        });
                      } catch (e) {
                        print('Failed to seek: $e');
                      }
                    },
                  ),
                ],
              ),
            ),
    ),
  );
}
