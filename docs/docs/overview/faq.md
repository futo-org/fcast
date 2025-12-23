
# FAQ

## Why did you develop FCast?
We developed FCast to democratize the field of media streaming. By creating an open-source protocol, we aim to encourage innovation and offer developers the freedom to create their own custom receiver implementations. This way, we're not just providing a tool for streaming media content, but also creating a dynamic platform for technological growth and creativity.

## How does FCast differ from other streaming protocols like Chromecast or AirPlay?
Unlike Chromecast or AirPlay, which are closed protocols, FCast is entirely open-source. This allows developers to create custom receiver implementations and integrate FCast into their existing applications, making it a more flexible and adaptable option.

## How can I implement FCast into my own application?
FCast has provided example implementations of both senders and receivers in Android, Rust, and TypeScript. You can use these as a reference while building your own application. Additionally, you can consult the protocol documentation for specific details on implementing the FCast protocol. Despite its powerful capabilities, the protocol is designed to be simple and straightforward to implement, making it accessible for developers at all levels.

## How does FCast handle device discovery?
FCast uses mDNS (Multicast Domain Name System) to discover available receivers on the network, simplifying the process of establishing a connection between the sender and receiver.

## Can I contribute to FCast's development?
Absolutely! As an open-source project, FCast thrives on community contributions. You can contribute to FCast by enhancing the codebase, implementing new features, fixing bugs, and more. You can find more details in our CONTRIBUTING.md file on our repository.

## Why isn't the FCast Receiver on F-Droid repository?
The Android receiver is optimized for Android TV devices (although it can be used on other mobile devices). F-Droid is not designed to work well as an Android TV app store currently, and installed applications typically are not added to the launcher.
However, we do provide a standalone APK available for download which also supports self-updates.
