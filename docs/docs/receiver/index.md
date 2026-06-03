
# FCast Receiver

The FCast Receiver is a receiver implementation that supports using the FCast protocol for displaying multimedia content from FCast sender applications.

![The main window of the FCast Receiver showing the connection information and a loading spinner.](/images/receiver_main.png)

Currently supported receiver platforms include:

* Android (Native)
* Linux, Windows, MacOS

Receivers can be downloaded from https://fcast.org/#downloads

## Connecting to the Receiver

After starting an FCast sender and a receiver application, you have the following options to connect your devices:

#### Automatic Discovery (mDNS)

!!! note

    Automatic discovery may not work in all network configurations.

The receiver should be visible in the casting dialog under `Discovered Devices` in the sender application. If you cannot find your receiver device, try connecting via QR code or manually connecting to it.

#### QR Code Connection

If you are using a sender application on a mobile device, you will have the ability to connect to the receiver by scanning the receiver QR Code from the sender application.

#### Manual Connection

1. Find the IP of the device running the receiver. The receiver will display all IP address you may be able to connect to under the `Connection Details` section.
1. In the sender application, navigate to the menu where you can manually connect to the receiver.
1. Enter the IP and port (if required, default is `46899`) information and connect.

The receiver will indicate once you have successfully connected to the sender application.
![The main window of the FCast Receiver showing a device has been connected.](/images/receiver_main_connected.png)
