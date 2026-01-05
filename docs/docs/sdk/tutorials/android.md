# Android

#### Project setup

Create a new kotlin android project using your tool of choice. This tutorial assumens that kotlin script is used for gradle configurations. This tutorial does not do anything with UI, see the section [Other examples](#other-examples) for more samples.

Add jitpack as a repository to `settings.gradle.kts`:

```kotlin
dependencyResolutionManagement {
    repositories {
        ...
        maven("https://jitpack.io")
    }
}
```

And add the SDK and [JNA] as dependencies to `app/build.gradle.kts`:

```kotlin
dependencies {
    ...
    implementation("org.futo.gitlab.videostreaming.fcast-sdk-jitpack:sender-sdk-minimal:0.4.0") {
        exclude(group = "net.java.dev.jna")
    }
    implementation("net.java.dev.jna:jna:5.13.0@aar")
}
```

Add the internet permission to `app/src/main/AndroidManifest.xml` to be able to discover receivers:

```xml
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android" xmlns:tools="http://schemas.android.com/tools">
    ...
    <uses-permission android:name="android.permission.INTERNET" />
    ...
</manifest>
```

Enable logging:

```kotlin
import org.fcast.sender_sdk.LogLevelFilter
import org.fcast.sender_sdk.initLogger

class MainActivity : ComponentActivity() {
    init {
        initLogger(LogLevelFilter.DEBUG)
    }

    ...
}
```

#### Receiver discovery

The next step will be to find and connect to a receiver device. The receiver documentation introduces [Automatic Discovery] which is what we'll use. To do this we need to use the NsdDeviceDiscoverer provided by the SDK. It expects a type which implements a callback interface:

```kotlin
...
import org.fcast.sender_sdk.NsdDeviceDiscoverer
import org.fcast.sender_sdk.DeviceDiscovererEventHandler
import org.fcast.sender_sdk.DeviceInfo

class DiscoveryEventHandler() : DeviceDiscovererEventHandler {
    override fun deviceAvailable(deviceInfo: DeviceInfo) {
        Log.d("DiscoveryEventHandler", "Device available: $deviceInfo")
    }

    override fun deviceChanged(deviceInfo: DeviceInfo) {
        Log.d("DiscoveryEventHandler", "Device changed: $deviceInfo")
    }

    override fun deviceRemoved(deviceName: String) { }
}

class MainActivity : ComponentActivity() {
    lateinit var deviceDiscoverer: NsdDeviceDiscoverer

    ...

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        deviceDiscoverer = NsdDeviceDiscoverer(this, DiscoveryEventHandler())

        ...
    }
}
```

If you run the app and have a receiver running you'll get a log output that looks something like this:

```console
NsdDeviceDiscoverer   Service discovery started for _googlecast._tcp
NsdDeviceDiscoverer   Service discovery started for _fcast._tcp
DiscoveryEventHandler Device available: DeviceInfo(name=MyFCast, protocol=F_CAST, addresses=[], port=0)
DiscoveryEventHandler Device changed: DeviceInfo(name=MyFCast, protocol=F_CAST, addresses=[V4(o1=192, o2=168, o3=50, o4=119)], port=46899)
```

#### Connecting

Now that we have a device we can initiate a connection which requires some boilerplate. Similar to the discoverer we need to create a type that implements a callback interface:

```kotlin
import org.fcast.sender_sdk.DeviceConnectionState
import org.fcast.sender_sdk.DeviceDiscovererEventHandler
import org.fcast.sender_sdk.DeviceEventHandler
import org.fcast.sender_sdk.DeviceInfo
import org.fcast.sender_sdk.KeyEvent
import org.fcast.sender_sdk.LogLevelFilter
import org.fcast.sender_sdk.MediaEvent
import org.fcast.sender_sdk.PlaybackState
import org.fcast.sender_sdk.Source

class DevEventHandler() : DeviceEventHandler {
    override fun connectionStateChanged(state: DeviceConnectionState) {
        Log.d("DevEventHandler", "Connection state changed: $state")
    }

    override fun volumeChanged(volume: Double) { }

    override fun timeChanged(time: Double) { }

    override fun playbackStateChanged(state: PlaybackState) { }

    override fun durationChanged(duration: Double) { }

    override fun speedChanged(speed: Double) { }

    override fun sourceChanged(source: Source) { }

    override fun keyEvent(event: KeyEvent) {}

    override fun mediaEvent(event: MediaEvent) {}

    override fun playbackError(message: String) { }
}

class DiscoveryEventHandler() : DeviceDiscovererEventHandler {
    val castContext = CastContext()
    var device: CastingDevice? = null

    fun maybeConnect(deviceInfo: DeviceInfo) {
        if (device == null && deviceInfo.port != 0.toUShort() && !deviceInfo.addresses.isEmpty()) {
            val newDevice = castContext.createDeviceFromInfo(deviceInfo)
            newDevice.connect(null, DevEventHandler(), 1000u)
            device = newDevice
        }
    }

    override fun deviceAvailable(deviceInfo: DeviceInfo) {
        maybeConnect(deviceInfo)
    }

    override fun deviceChanged(deviceInfo: DeviceInfo) {
        maybeConnect(deviceInfo)
    }

    override fun deviceRemoved(deviceName: String) { }
}
```

Running this will connect to the first valid receiver discovered and print some state updates:

```console
DevEventHandler Connection state changed: org.fcast.sender_sdk.DeviceConnectionState$Connecting@a2b612a
DevEventHandler Connection state changed: Connected(usedRemoteAddr=V4(o1=192, o2=168, o3=50, o4=119), localAddr=V4(o1=192, o2=168, o3=50, o4=8))
```

#### Cast

Let's try to cast something. We need add some more boilerplate:

```kotlin
class DevEventHandler(
    val device: CastingDevice
) : DeviceEventHandler {
    override fun connectionStateChanged(state: DeviceConnectionState) {
        Log.d("DevEventHandler", "Connection state changed: $state")
        if (state is DeviceConnectionState.Connected) {
            device.load(LoadRequest.Url(
                contentType = "video/mp4",
                url = "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4",
                resumePosition = 0.0,
                speed = null,
                volume = null,
                metadata = null,
                requestHeaders = null
            ))
        }
    }

    override fun volumeChanged(volume: Double) {
        Log.d("DevEventHandler", "Volume changed: $volume")
    }

    override fun timeChanged(time: Double) {
        Log.d("DevEventHandler", "Time changed: $time")
    }

    override fun playbackStateChanged(state: PlaybackState) {
        Log.d("DevEventHandler", "Playback state changed: $state")
    }

    ...
}

class DiscoveryEventHandler() : DeviceDiscovererEventHandler {
    val castContext = CastContext()
    var device: CastingDevice? = null

    fun maybeConnect(deviceInfo: DeviceInfo) {
        if (device == null && deviceInfo.port != 0.toUShort() && !deviceInfo.addresses.isEmpty()) {
            val newDevice = castContext.createDeviceFromInfo(deviceInfo)
            newDevice.connect(null, DevEventHandler(newDevice), 1000u)
            device = newDevice
        }
    }

    ...
}
```

If we run the program now we see that [Big Buck Bunny] is started playing on the receiver and our program is receiving updates of the playback state!

```console
DevEventHandler Connection state changed: Connected(usedRemoteAddr=V4(o1=192, o2=168, o3=50, o4=119), localAddr=V4(o1=192, o2=168, o3=50, o4=8))
DevEventHandler Volume changed: 1.0
DevEventHandler Playback state changed: PLAYING
DevEventHandler Time changed: 1.233566
DevEventHandler Time changed: 2.296937
DevEventHandler Time changed: 3.357214
DevEventHandler Time changed: 4.410581
DevEventHandler Time changed: 5.472523
DevEventHandler Time changed: 6.537779
DevEventHandler Time changed: 7.598685
```

#### Other examples

A more in depth example can be found [here](https://github.com/futo-org/fcast/tree/master/sdk/sender/examples/android).

#### Complete code

```kotlin
package org.fcast.sdk.example.fcastsdktutorial

import android.os.Bundle
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.tooling.preview.Preview
import org.fcast.sdk.example.fcastsdktutorial.ui.theme.FCastSDKTutorialTheme
import org.fcast.sender_sdk.CastContext
import org.fcast.sender_sdk.CastingDevice
import org.fcast.sender_sdk.DeviceConnectionState
import org.fcast.sender_sdk.DeviceDiscovererEventHandler
import org.fcast.sender_sdk.DeviceEventHandler
import org.fcast.sender_sdk.DeviceInfo
import org.fcast.sender_sdk.KeyEvent
import org.fcast.sender_sdk.LoadRequest
import org.fcast.sender_sdk.LogLevelFilter
import org.fcast.sender_sdk.MediaEvent
import org.fcast.sender_sdk.NsdDeviceDiscoverer
import org.fcast.sender_sdk.PlaybackState
import org.fcast.sender_sdk.Source
import org.fcast.sender_sdk.initLogger

class DevEventHandler(
    val device: CastingDevice
) : DeviceEventHandler {
    override fun connectionStateChanged(state: DeviceConnectionState) {
        Log.d("DevEventHandler", "Connection state changed: $state")
        if (state is DeviceConnectionState.Connected) {
            device.load(LoadRequest.Url(
                contentType = "video/mp4",
                url = "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4",
                resumePosition = 0.0,
                speed = null,
                volume = null,
                metadata = null,
                requestHeaders = null
            ))
        }
    }

    override fun volumeChanged(volume: Double) {
        Log.d("DevEventHandler", "Volume changed: $volume")
    }

    override fun timeChanged(time: Double) {
        Log.d("DevEventHandler", "Time changed: $time")
    }

    override fun playbackStateChanged(state: PlaybackState) {
        Log.d("DevEventHandler", "Playback state changed: $state")
    }

    override fun durationChanged(duration: Double) { }

    override fun speedChanged(speed: Double) { }

    override fun sourceChanged(source: Source) { }

    override fun keyEvent(event: KeyEvent) {}

    override fun mediaEvent(event: MediaEvent) {}

    override fun playbackError(message: String) { }
}

class DiscoveryEventHandler() : DeviceDiscovererEventHandler {
    val castContext = CastContext()
    var device: CastingDevice? = null

    fun maybeConnect(deviceInfo: DeviceInfo) {
        if (device == null && deviceInfo.port != 0.toUShort() && !deviceInfo.addresses.isEmpty()) {
            val newDevice = castContext.createDeviceFromInfo(deviceInfo)
            newDevice.connect(null, DevEventHandler(newDevice), 1000u)
            device = newDevice
        }
    }

    override fun deviceAvailable(deviceInfo: DeviceInfo) {
        maybeConnect(deviceInfo)
    }

    override fun deviceChanged(deviceInfo: DeviceInfo) {
        maybeConnect(deviceInfo)
    }

    override fun deviceRemoved(deviceName: String) { }
}

class MainActivity : ComponentActivity() {
    lateinit var deviceDiscoverer: NsdDeviceDiscoverer

    init {
        initLogger(LogLevelFilter.DEBUG)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        deviceDiscoverer = NsdDeviceDiscoverer(this, DiscoveryEventHandler())

        enableEdgeToEdge()
        setContent {
            FCastSDKTutorialTheme {
                Scaffold(modifier = Modifier.fillMaxSize()) { innerPadding ->
                    Greeting(
                        name = "Android",
                        modifier = Modifier.padding(innerPadding)
                    )
                }
            }
        }
    }
}

@Composable
fun Greeting(name: String, modifier: Modifier = Modifier) {
    Text(
        text = "Hello $name!",
        modifier = modifier
    )
}

@Preview(showBackground = true)
@Composable
fun GreetingPreview() {
    FCastSDKTutorialTheme {
        Greeting("Android")
    }
}
```

[JNA]: https://github.com/java-native-access/jna
[Automatic Discovery]: ../../receiver/#automatic-discovery-mdns
[Big Buck Bunny]: https://en.wikipedia.org/wiki/Big_Buck_Bunny
