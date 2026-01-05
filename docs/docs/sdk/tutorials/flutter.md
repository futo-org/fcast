# Flutter

#### Project setup

Make sure you've installed [rustup] and [protoc].

Create a new flutter project:

```
flutter create --project-name fcast_tutorial_flutter .
```

Add the library as a dependency:

```
flutter pub add fcast_sender_sdk
```

Import the SDK and initialize it:

```dart
import 'package:flutter/material.dart';
import 'package:fcast_sender_sdk/fcast_sender_sdk.dart';


Future<void> main() async {
  await FCastSenderSdkLib.init();
  initLogger();
  runApp(const MyApp());
}

...
```

#### Receiver discovery

The next step will be to find and connect to a receiver device. The receiver documentation introduces [Automatic Discovery] which is what we'll use. To do this we need to use the DeviceDiscoverer provided by the SDK. :

```dart
Future<void> main() async {
  ...

  final DeviceDiscoverer discoverer = DeviceDiscoverer();
  await discoverer.init();
  discoverer.eventStreamController.stream.listen((event) {
    switch (event) {
      case DiscoveryEventDeviceAdded():
        print('Device added: name=${event.deviceInfo.name} protocol=${event.deviceInfo.protocol} addresses=${event.deviceInfo.addresses} port=${event.deviceInfo.port}');
        break;
      default:
        break;
    }
  });

  ...
}
```

If you run the app and have a receiver running you'll get a log output that looks something like this:

```console
Device added: name=MyFCast protocol=ProtocolType.fCast addresses=[IpAddr.v4(o1: 100, o2: 102, o3: 222, o4: 4)] port=46899
```

#### Connecting

```dart
Future<void> main() async {
  ...

  final CastContext castContext = CastContext();
  final DeviceDiscoverer discoverer = DeviceDiscoverer();
  await discoverer.init();
  discoverer.eventStreamController.stream.listen((event) {
    switch (event) {
      case DiscoveryEventDeviceAdded():
        final CastingDevice device = castContext.createDeviceFromInfo(
          info: event.deviceInfo,
        );
        device.connect(eventHandler: DeviceEventHandler(onEvent: (event) {
          print('Device event: $event');
        }), reconnectIntervalMillis: 1000);
        break;
      default:
        break;
    }
  });

  ...
}
```

```console
Device event: DeviceEvent.connectionStateChanged(newState: DeviceConnectionState.connecting())
Device event: DeviceEvent.connectionStateChanged(newState: DeviceConnectionState.connected(usedRemoteAddr: IpAddr.v4(o1: 100, o2: 102, o3: 222, o4: 4), localAddr: IpAddr.v4(o1: 100, o2: 102, o3: 222, o4: 4)))
```

#### Cast

Let's try to cast something. We need add some more boilerplate:

```dart
Future<void> main() async {
  ...

  discoverer.eventStreamController.stream.listen((event) {
    switch (event) {
      case DiscoveryEventDeviceAdded():
        final CastingDevice device = castContext.createDeviceFromInfo(
          info: event.deviceInfo,
        );
        device.connect(
          eventHandler: DeviceEventHandler(
            onEvent: (event) {
              switch (event) {
                case DeviceEvent_ConnectionStateChanged():
                  if (event.newState case DeviceConnectionState_Connected()) {
                    device.load(
                      request: LoadRequest.url(
                        contentType: 'video/mp4',
                        url: 'http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4',
                      ),
                    );
                  }
                  break;
                case DeviceEvent_VolumeChanged():
                  print("Volume changed: ${event.newVolume}");
                  break;
                case DeviceEvent_TimeChanged():
                  print("Time changed: ${event.newTime}");
                  break;
                case DeviceEvent_PlaybackStateChanged():
                  print("Playback state changed: ${event.newPlaybackState}");
                  break;
                default:
                  break;
              }
            },
          ),
          reconnectIntervalMillis: 1000,
        );
        break;
      default:
        break;
    }
  });

  ...
}
```

If we run the program now we see that [Big Buck Bunny] is started playing on the receiver and our program is receiving updates of the playback state!

```console
Volume changed: 1.0
Playback state changed: PlaybackState.playing
Time changed: 1.254372
Time changed: 2.318126
Time changed: 3.36574
Time changed: 4.43104
Time changed: 5.492113
Time changed: 6.557984
```

#### Other examples

A more in depth example can be found [here](https://gitlab.futo.org/videostreaming/fcast-sender-sdk-flutter-plugin/-/tree/master/example?ref_type=heads).

#### Complete code

```dart
import 'package:flutter/material.dart';
import 'package:fcast_sender_sdk/fcast_sender_sdk.dart';

Future<void> main() async {
  await FCastSenderSdkLib.init();
  initLogger();

  final CastContext castContext = CastContext();
  final DeviceDiscoverer discoverer = DeviceDiscoverer();
  await discoverer.init();
  discoverer.eventStreamController.stream.listen((event) {
    switch (event) {
      case DiscoveryEventDeviceAdded():
        print(
          'Device added: name=${event.deviceInfo.name} protocol=${event.deviceInfo.protocol} addresses=${event.deviceInfo.addresses} port=${event.deviceInfo.port}',
        );
        final CastingDevice device = castContext.createDeviceFromInfo(
          info: event.deviceInfo,
        );
        device.connect(
          eventHandler: DeviceEventHandler(
            onEvent: (event) {
              switch (event) {
                case DeviceEvent_ConnectionStateChanged():
                  if (event.newState case DeviceConnectionState_Connected()) {
                    device.load(
                      request: LoadRequest.url(
                        contentType: 'video/mp4',
                        url: 'http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4',
                      ),
                    );
                  }
                  break;
                case DeviceEvent_VolumeChanged():
                  print("Volume changed: ${event.newVolume}");
                  break;
                case DeviceEvent_TimeChanged():
                  print("Time changed: ${event.newTime}");
                  break;
                case DeviceEvent_PlaybackStateChanged():
                  print("Playback state changed: ${event.newPlaybackState}");
                  break;
                default:
                  break;
              }
            },
          ),
          reconnectIntervalMillis: 1000,
        );
        break;
      default:
        break;
    }
  });

  runApp(const MyApp());
}

class MyApp extends StatelessWidget {
  const MyApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Flutter Demo',
      theme: ThemeData(
        colorScheme: .fromSeed(seedColor: Colors.deepPurple),
      ),
      home: const MyHomePage(title: 'Flutter Demo Home Page'),
    );
  }
}

class MyHomePage extends StatefulWidget {
  const MyHomePage({super.key, required this.title});
  final String title;
  @override
  State<MyHomePage> createState() => _MyHomePageState();
}

class _MyHomePageState extends State<MyHomePage> {
  int _counter = 0;

  void _incrementCounter() {
    setState(() {
      _counter++;
    });
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        backgroundColor: Theme.of(context).colorScheme.inversePrimary,
        title: Text(widget.title),
      ),
      body: Center(
        child: Column(
          mainAxisAlignment: .center,
          children: [
            const Text('You have pushed the button this many times:'),
            Text(
              '$_counter',
              style: Theme.of(context).textTheme.headlineMedium,
            ),
          ],
        ),
      ),
      floatingActionButton: FloatingActionButton(
        onPressed: _incrementCounter,
        tooltip: 'Increment',
        child: const Icon(Icons.add),
      ),
    );
  }
}
```

[rustup]: https://rustup.rs/
[protoc]: https://protobuf.dev/installation/
[Automatic Discovery]: ../../receiver/#automatic-discovery-mdns
[Big Buck Bunny]: https://en.wikipedia.org/wiki/Big_Buck_Bunny
