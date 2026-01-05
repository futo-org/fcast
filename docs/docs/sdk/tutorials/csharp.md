# C\#

#### Project setup

Add the SDK as a dependency to the csproj file:

```xml
<Project Sdk="Microsoft.NET.Sdk">

   ...

  <ItemGroup>
    <PackageReference Include="FCastSenderSDKDotnet" Version="0.0.5" />
  </ItemGroup>

</Project>
```

#### Receiver discovery

The next step will be to find and connect to a receiver device. The receiver documentation introduces [Automatic Discovery] which is what we'll use. To do this we need to use the discoverer provided by the SDK. It expects a type which implements a callback interface:

```csharp
using FCast.SenderSDK;

class DiscoveryEventHandler : DeviceDiscovererEventHandler
{
    public void DeviceAvailable(DeviceInfo deviceInfo) {
        System.Console.WriteLine(deviceInfo);
    }

    public void DeviceChanged(DeviceInfo deviceInfo) {}

    public void DeviceRemoved(string deviceName) {}
}

public class Program
{
    public static void Main(string[] args)
    {
        CastContext context = new CastContext();
        DiscoveryEventHandler eventHandler = new DiscoveryEventHandler();
        context.StartDiscovery(eventHandler);

        Thread.Sleep(10 * 1000);
    }
}
```

If you run the program and have a receiver running you'll get a log output that looks something like this:

```console
DeviceInfo { name = MyFCast, protocol = FCast, addresses = FCast.SenderSDK.IpAddr[], port = 46899 }
```

#### Connecting

Now that we have a device we can initiate a connection which requires some boilerplate. Similar to the discoverer we need to create a type that implements a callback interface:

```csharp
...

class EventHandler: DeviceEventHandler {
    private CastingDevice device;

    public EventHandler(CastingDevice device) {
        this.device = device;
    }

    public void ConnectionStateChanged(DeviceConnectionState state) {
        switch (state) {
        case DeviceConnectionState.Connected(
            IpAddr usedRemoteAddr,
            IpAddr localAddr
        ):
            System.Console.WriteLine("Connected");
            break;
        default:
            break;
        }
    }

    public void VolumeChanged(double volume) {}
    public void TimeChanged(double time) {}
    public void PlaybackStateChanged(PlaybackState state) {}
    public void DurationChanged(double duration) {}
    public void SpeedChanged(double speed) {}
    public void SourceChanged(Source @source) {}
    public void KeyEvent(KeyEvent @event) {}
    public void MediaEvent(MediaEvent @event) {}
    public void PlaybackError(string message) {}
}

class DiscoveryEventHandler : DeviceDiscovererEventHandler
{
    CastContext Context;

    public DiscoveryEventHandler(CastContext context)
    {
        Context = context;
    }

    public void DeviceAvailable(DeviceInfo deviceInfo) {
        CastingDevice device = Context.CreateDeviceFromInfo(deviceInfo);
        device.Connect(null, new EventHandler(device), 1000);
    }

    public void DeviceChanged(DeviceInfo deviceInfo) {}

    public void DeviceRemoved(string deviceName) {}
}

...
```

Running this will connect to the first valid receiver discovered and print a message:

```console
Connected
```

#### Cast

Let's try to cast something. We need add some more boilerplate:

```csharp
class EventHandler: DeviceEventHandler {
    ...

    public void ConnectionStateChanged(DeviceConnectionState state) {
        switch (state) {
        case DeviceConnectionState.Connected(
            IpAddr usedRemoteAddr,
            IpAddr localAddr
        ):
            System.Console.WriteLine("Connected");
            device.Load(
                new LoadRequest.Url(
                    "video/mp4",
                    "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4",
                    0.0,
                    null,
                    null,
                    null,
                    null
                )
            );
            break;
        default:
            break;
        }
    }

    public void VolumeChanged(double volume) {
        System.Console.WriteLine($"Volume changed: {volume}");
    }

    public void TimeChanged(double time) {
        System.Console.WriteLine($"Time changed: {time}");
    }

    public void PlaybackStateChanged(PlaybackState state) {
        System.Console.WriteLine($"Playback state changed: {state}");
    }

    ...
}
```

If we run the program now we see that [Big Buck Bunny] is started playing on the receiver and our program is receiving updates of the playback state!

```console
Volume changed: 1
Playback state changed: Playing
Time changed: 1.209802
Time changed: 2.270733
Time changed: 3.329788
Time changed: 4.394705
Time changed: 5.45521
Time changed: 6.521831
Time changed: 7.571996
```

#### Complete code

```csharp
using FCast.SenderSDK;

class EventHandler: DeviceEventHandler {
    private CastingDevice device;

    public EventHandler(CastingDevice device) {
        this.device = device;
    }

    public void ConnectionStateChanged(DeviceConnectionState state) {
        switch (state) {
        case DeviceConnectionState.Connected(
            IpAddr usedRemoteAddr,
            IpAddr localAddr
        ):
            System.Console.WriteLine("Connected");
            device.Load(
                new LoadRequest.Url(
                    "video/mp4",
                    "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4",
                    0.0,
                    null,
                    null,
                    null,
                    null
                )
            );
            break;
        default:
            break;
        }
    }

    public void VolumeChanged(double volume) {
        System.Console.WriteLine($"Volume changed: {volume}");
    }

    public void TimeChanged(double time) {
        System.Console.WriteLine($"Time changed: {time}");
    }

    public void PlaybackStateChanged(PlaybackState state) {
        System.Console.WriteLine($"Playback state changed: {state}");
    }

    public void DurationChanged(double duration) {}
    public void SpeedChanged(double speed) {}
    public void SourceChanged(Source @source) {}
    public void KeyEvent(KeyEvent @event) {}
    public void MediaEvent(MediaEvent @event) {}
    public void PlaybackError(string message) {}
}

class DiscoveryEventHandler : DeviceDiscovererEventHandler
{
    CastContext Context;

    public DiscoveryEventHandler(CastContext context)
    {
        Context = context;
    }

    public void DeviceAvailable(DeviceInfo deviceInfo) {
        CastingDevice device = Context.CreateDeviceFromInfo(deviceInfo);
        device.Connect(null, new EventHandler(device), 1000);
    }

    public void DeviceChanged(DeviceInfo deviceInfo) {}

    public void DeviceRemoved(string deviceName) {}
}

public class Program
{
    public static void Main(string[] args)
    {
        CastContext context = new CastContext();
        DiscoveryEventHandler eventHandler = new DiscoveryEventHandler(context);
        context.StartDiscovery(eventHandler);

        Thread.Sleep(10 * 1000);
    }
}
```

[Automatic Discovery]: ../../receiver/#automatic-discovery-mdns
[Big Buck Bunny]: https://en.wikipedia.org/wiki/Big_Buck_Bunny
