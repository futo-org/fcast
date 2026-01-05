# Rust

This tutorial will show you basic usage of the [FCast sender SDK] from rust.

#### Project setup

First we need to create a new project:

```
cargo new --bin fcast-tutorial
```

And add the SDK as a dependency to your `Cargo.toml`:

```toml
[dependencies.fcast-sender-sdk]
version = "0.1.2"
default-features = false
features = ["chromecast", "discovery", "fcast"] # (1)!
```

1. The crates features can be found [here](https://docs.rs/crate/fcast-sender-sdk/0.1.2/features)

At the core of the SDK we have a [CastContext], let's create one:

```rust
use fcast_sender_sdk::context::CastContext;

fn main() {
    let ctx = CastContext::new().unwrap();
}
```

#### Receiver discovery

The next step will be to find and connect to a receiver device. The receiver documentation introduces [Automatic Discovery] which is what we'll use. To do this we need to use the [start_discovery()] helper function provided by the cast context. It expects a type which implements a callback trait:

```rust
...

use fcast_sender_sdk::{DeviceDiscovererEventHandler, device::DeviceInfo};
use std::sync::Arc;

struct DiscovererEventHandler {}

impl DeviceDiscovererEventHandler for DiscovererEventHandler {
    fn device_available(&self, device_info: DeviceInfo) {
        println!("Device available: {device_info:?}");
    }

    fn device_removed(&self, _device_name: String) {}

    fn device_changed(&self, _device_info: DeviceInfo) {}
}

fn main() {
    ...

    ctx.start_discovery(Arc::new(DiscovererEventHandler {}));

    std::thread::sleep(std::time::Duration::from_secs(5));
}
```

When running the program when there are receiver devices on your network the program output might look something like this:

```console
Device available: DeviceInfo { name: "MyFCast", protocol: FCast, addresses: [V4 { o1: 192, o2: 168, o3: 50, o4: 173 }], port: 46899 }
Device available: DeviceInfo { name: "MyChromecast", protocol: Chromecast, addresses: [V4 { o1: 192, o2: 168, o3: 50, o4: 36 }], port: 8009 }
```

Now let's actually connect to a device. We'll do something very simple for the sake of this tutorial:

```rust
...

use std::sync::mpsc::{Sender, channel};

struct DiscovererEventHandler {
    device_tx: Sender<DeviceInfo>,
}

impl DeviceDiscovererEventHandler for DiscovererEventHandler {
    fn device_available(&self, device_info: DeviceInfo) {
        self.device_tx.send(device_info).unwrap();
    }

    ...
}

fn main() {
    ...

    let (device_tx, device_rx) = channel();
    ctx.start_discovery(Arc::new(DiscovererEventHandler { device_tx }));
```

We get the first discovered device and create a [CastingDevice] with the information:

```rust
    let device_info = device_rx.recv().unwrap();
    println!("Device info: {device_info:?}");
    let device = ctx.create_device_from_info(device_info);
}
```

#### Connecting

Now that we have a device we can initiate a connection which requires some boilerplate. Similar to the discoverer we need to create a
type that implements a callback trait:

```rust
...

use fcast_sender_sdk::device::{
    DeviceConnectionState, DeviceEventHandler, DeviceInfo, KeyEvent,
    MediaEvent, PlaybackState, Source,
};

struct DevEventHandler {}

impl DeviceEventHandler for DevEventHandler {
    fn connection_state_changed(&self, state: DeviceConnectionState) {
        println!("Connection state changed: {state:?}");
    }

    fn volume_changed(&self, volume: f64) {
        println!("Volume changed: {volume}");
    }

    fn time_changed(&self, time: f64) {
        println!("Time changed: {time}");
    }

    fn playback_state_changed(&self, state: PlaybackState) {
        println!("Playback state changed: {state:?}");
    }

    fn duration_changed(&self, _duration: f64) {}

    fn speed_changed(&self, _speed: f64) {}

    fn source_changed(&self, _source: Source) {}

    fn key_event(&self, _event: KeyEvent) {}

    fn media_event(&self, _event: MediaEvent) {}

    fn playback_error(&self, _message: String) {}
}

fn main {
    ...

    device.connect(None, Arc::new(DevEventHandler {}), 1000).unwrap();
```

Sleep for a long time:

```rust
    std::thread::sleep(std::time::Duration::from_secs(600));
}
```

Running the program you might get a terminal output that looks like this:

```console
Device info: DeviceInfo { name: "MyFCast", protocol: FCast, addresses: [V4 { o1: 192, o2: 168, o3: 50, o4: 173 }], port: 46899 }
Connection state changed: Connecting
Connection state changed: Connected { used_remote_addr: V4 { o1: 192, o2: 168, o3: 50, o4: 173 }, local_addr: V4 { o1: 192, o2: 168, o3: 50, o4: 173 } }
```

#### Cast

Let's try to cast something. We need add some more boilerplate:

```rust
use std::sync::Weak;
use fcast_sender_sdk::device::CastingDevice;

struct DevEventHandler {
    device_weak: Weak<dyn CastingDevice>
}

impl DeviceEventHandler for DevEventHandler {
    fn connection_state_changed(&self, state: DeviceConnectionState) {
        println!("Connection state changed: {state:?}");

        if matches!(state, DeviceConnectionState::Connected { .. }) {
            if let Some(device) = self.device_weak.upgrade() {
```

We wait until we receive the `Connected` connection state change event before loading our video:

```rust
                device.load(LoadRequest::Video {
                    content_type: "video/mp4".to_owned(),
                    url: "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4".to_owned(),
                    resume_position: 0.0,
                    speed: None,
                    volume: None,
                    metadata: None,
                    request_headers: None,
                }).unwrap();
            }
        }
    }

    ...
}

fn main() {
    ...

    device
        .connect(
            None,
            Arc::new(DevEventHandler {
                device_weak: Arc::downgrade(&device),
            }),
            1000,
        )
        .unwrap();

    ...
}
```

If we run the program now we see that [Big Buck Bunny] is started playing on the receiver and our program is receiving updates of the playback state!

```console
Device info: DeviceInfo { name: "MyFCast", protocol: FCast, addresses: [V4 { o1: 192, o2: 168, o3: 50, o4: 173 }], port: 46899 }
Connection state changed: Connecting
Connection state changed: Connected { used_remote_addr: V4 { o1: 192, o2: 168, o3: 50, o4: 173 }, local_addr: V4 { o1: 192, o2: 168, o3: 50, o4: 173 } }
Playback state changed: Playing
Volume changed: 1
Time changed: 1.204268
Time changed: 2.278132
Time changed: 3.34539
Time changed: 4.409731
Time changed: 5.479233
Time changed: 6.533175
Time changed: 7.579753
```

#### Other examples

A more in depth example can be found [here](https://github.com/futo-org/fcast/tree/master/sdk/sender/examples/desktop).

#### Complete code

```rust
use std::sync::{Arc, Weak};

use fcast_sender_sdk::context::CastContext;
use fcast_sender_sdk::device::{
    CastingDevice, DeviceConnectionState, DeviceEventHandler, DeviceInfo,
    KeyEvent, LoadRequest, MediaEvent, PlaybackState, Source,
};
use fcast_sender_sdk::DeviceDiscovererEventHandler;

use std::sync::mpsc::{Sender, channel};

struct DevEventHandler {
    device_weak: Weak<dyn CastingDevice>,
}

impl DeviceEventHandler for DevEventHandler {
    fn connection_state_changed(&self, state: DeviceConnectionState) {
        println!("Connection state changed: {state:?}");

        if matches!(state, DeviceConnectionState::Connected { .. }) {
            if let Some(device) = self.device_weak.upgrade() {
                device.load(LoadRequest::Video {
                    content_type: "video/mp4".to_owned(),
                    url: "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4".to_owned(),
                    resume_position: 0.0,
                    speed: None,
                    volume: None,
                    metadata: None,
                    request_headers: None,
                }).unwrap();
            }
        }
    }

    fn volume_changed(&self, volume: f64) {
        println!("Volume changed: {volume}");
    }

    fn time_changed(&self, time: f64) {
        println!("Time changed: {time}");
    }

    fn playback_state_changed(&self, state: PlaybackState) {
        println!("Playback state changed: {state:?}");
    }

    fn duration_changed(&self, _duration: f64) {}

    fn speed_changed(&self, _speed: f64) {}

    fn source_changed(&self, _source: Source) {}

    fn key_event(&self, _event: KeyEvent) {}

    fn media_event(&self, _event: MediaEvent) {}

    fn playback_error(&self, _message: String) {}
}

struct DiscovererEventHandler {
    device_tx: Sender<DeviceInfo>,
}

impl DeviceDiscovererEventHandler for DiscovererEventHandler {
    fn device_available(&self, device_info: DeviceInfo) {
        self.device_tx.send(device_info).unwrap();
    }

    fn device_removed(&self, _device_name: String) {}

    fn device_changed(&self, _device_info: DeviceInfo) {}
}

fn main() {
    let ctx = CastContext::new().unwrap();

    let (device_tx, device_rx) = channel();

    ctx.start_discovery(Arc::new(DiscovererEventHandler { device_tx }));

    let device_info = device_rx.recv().unwrap();
    println!("Device info: {device_info:?}");
    let device = ctx.create_device_from_info(device_info);

    device
        .connect(
            None,
            Arc::new(DevEventHandler {
                device_weak: Arc::downgrade(&device),
            }),
            1000,
        )
        .unwrap();

    std::thread::sleep(std::time::Duration::from_secs(600));
}
```

[FCast Sender SDK]: https://crates.io/crates/fcast-sender-sdk
[CastContext]: https://docs.rs/fcast-sender-sdk/0.1.2/fcast_sender_sdk/context/struct.CastContext.html
[Automatic Discovery]: ../../receiver/#automatic-discovery-mdns
[start_discovery()]: https://docs.rs/fcast-sender-sdk/0.1.2/fcast_sender_sdk/context/struct.CastContext.html#method.start_discovery
[CastingDevice]: https://docs.rs/fcast-sender-sdk/0.1.2/fcast_sender_sdk/device/trait.CastingDevice.html
[Big Buck Bunny]: https://en.wikipedia.org/wiki/Big_Buck_Bunny
