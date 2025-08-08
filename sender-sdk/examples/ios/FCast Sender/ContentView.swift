import Network
import PhotosUI
import SwiftUI
import System
import CodeScanner

final class DevEventHandler: DeviceEventHandler {
    let onStateChanged: @Sendable (DeviceConnectionState) -> Void
    let dataModel: DataModel

    init(
        onStateChanged: @Sendable @escaping (DeviceConnectionState) -> Void,
        dataModel: DataModel
    ) {
        self.onStateChanged = onStateChanged
        self.dataModel = dataModel
    }

    func connectionStateChanged(state: DeviceConnectionState) {
        onStateChanged(state)
    }

    func volumeChanged(volume: Double) {
        DispatchQueue.main.async {
            self.dataModel.volume = volume
        }
    }

    func timeChanged(time: Double) {
        DispatchQueue.main.async {
            self.dataModel.time = time
        }
    }

    func playbackStateChanged(state: PlaybackState) {}

    func durationChanged(duration: Double) {
        DispatchQueue.main.async {
            self.dataModel.duration = duration
        }
    }

    func speedChanged(speed: Double) {
        DispatchQueue.main.async {
            self.dataModel.speed = speed
        }
    }

    func sourceChanged(source: Source) {}

    func keyEvent(event: GenericKeyEvent) {}

    func mediaEvent(event: GenericMediaEvent) {}
}

final class NWDeviceDiscoverer {
    private var ctx: CastContext
    private var fCastBrowser: NWBrowser
    private var chromecastBrowser: NWBrowser

    init(
        context: CastContext,
        onAdded: @escaping (FoundDevice) -> Void,
        onRemoved: @escaping (NWEndpoint) -> Void,
    ) {
        ctx = context
        fCastBrowser = NWBrowser(
            for: .bonjourWithTXTRecord(type: "_fcast._tcp", domain: nil),
            using: .tcp
        )
        chromecastBrowser = NWBrowser(
            for: .bonjourWithTXTRecord(type: "_googlecast._tcp", domain: nil),
            using: .tcp
        )

        fCastBrowser.browseResultsChangedHandler = { newResults, changes in
            for result in changes {
                switch result {
                case .added(let added):
                    if case .service(let name, _, _, _) = added.endpoint {
                        onAdded(
                            FoundDevice(
                                name: name,
                                endpoint: added.endpoint,
                                proto: ProtocolType.fCast
                            )
                        )
                    }
                case .removed(let removed):
                    onRemoved(removed.endpoint)
                default:
                    break
                }
            }
        }
        chromecastBrowser.browseResultsChangedHandler = { newResults, changes in
            for result in changes {
                switch result {
                case .added(let added):
                    if case .service(var name, _, _, _) = added.endpoint {
                        if case .bonjour(let txt) = added.metadata,
                            let maybeFriendlyNameData = txt.getEntry(for: "fn"),
                            let friendlyNameData = maybeFriendlyNameData.data,
                            let friendlyName = String(
                                data: friendlyNameData,
                                encoding: .utf8
                            )
                        {
                            name = friendlyName
                        }
                        onAdded(
                            FoundDevice(
                                name: name,
                                endpoint: added.endpoint,
                                proto: ProtocolType.chromecast
                            )
                        )
                    }
                case .removed(let removed):
                    onRemoved(removed.endpoint)
                default:
                    break
                }
            }
        }

        fCastBrowser.start(queue: .main)
        chromecastBrowser.start(queue: .main)
    }
}

struct ContentView: View {
    @ObservedObject var dataModel: DataModel
    var castContext: CastContext
    var discoverer: NWDeviceDiscoverer
    @State var activeDevice: CastingDevice? = nil
    var eventHandler: DevEventHandler
    @State var selectedMediaItem: PhotosPickerItem? = nil
    @State var isImportingFile = false
    @State var isShowingMediaPicker = false
    @State var activeFileHandle: FileHandle? = nil
    var fileServer: FileServer

    init(data: DataModel) throws {
        initLogger()
        dataModel = data
        castContext = try CastContext()
        fileServer = castContext.startFileServer()
        discoverer = NWDeviceDiscoverer(
            context: castContext,
            onAdded: { found in
                data.devices.append(found)
            },
            onRemoved: { endpoint in
                data.devices.removeAll { it in
                    it.endpoint == endpoint
                }
            }
        )
        eventHandler = DevEventHandler(
            onStateChanged: { state in
                switch state {
                case .connected(usedRemoteAddr: _, let localAddr):
                    DispatchQueue.main.async {
                        data.sheetState = SheetState.connected
                        data.usedLocalAddress = localAddr
                    }
                default:
                    break
                }
            },
            dataModel: data,
        )
    }

    var body: some View {
        NavigationStack {
            VStack {
                if activeDevice != nil {
                    Button("Cast local file") {
                        isShowingMediaPicker.toggle()
                    }
                }
            }
            .padding()
            .sheet(isPresented: $isShowingMediaPicker) {
                MediaPicker { contentType, localFileURL in
                    if let handle = try? FileHandle(
                        forReadingFrom: localFileURL
                    ) {
                        self.activeFileHandle = handle
                        Task {
                            if let activeDevice = self.activeDevice,
                                let usedLocalAddress = dataModel
                                    .usedLocalAddress
                            {
                                do {
                                    let entry = try self.fileServer.serveFile(
                                        fd: handle.fileDescriptor
                                    )
                                    let url =
                                        "http://\(urlFormatIpAddr(addr: usedLocalAddress)):\(entry.port)/\(entry.location)"
                                    try activeDevice.loadUrl(
                                        contentType: contentType,
                                        url: url,
                                        resumePosition: nil,
                                        speed: nil
                                    )
                                } catch {
                                    print("Failed to serve file")
                                }
                            }
                        }
                    }
                }
            }
            .toolbar {
                Button(action: {
                    dataModel.isShowingSheet.toggle()
                }) {
                    Image("chromecast-icon")
                        .renderingMode(.template)
                        .resizable()
                        .scaledToFit()
                        .frame(maxWidth: 64)
                }
            }
            .sheet(isPresented: $dataModel.isShowingSheet) {
                switch dataModel.sheetState {
                case .deviceList:
                    DeviceList(
                        devices: dataModel.devices,
                        onConnect: { device in
                            dataModel.sheetState = SheetState.connecting(
                                deviceName: device.name
                            )
                            Task {
                                let conn = NWConnection(
                                    to: device.endpoint,
                                    using: .tcp
                                )
                                conn.stateUpdateHandler = { state in
                                    switch state {
                                    case .ready:
                                        if let innerEndpoint = conn.currentPath?
                                            .remoteEndpoint,
                                           case .hostPort(let host, let port) =
                                            innerEndpoint
                                        {
                                            switch host {
                                            default:
                                                break
                                            }
                                            let address: IpAddr
                                            switch host {
                                            case .ipv4(let addr):
                                                let raw = addr.rawValue
                                                address = IpAddr.v4(
                                                    o1: raw[0],
                                                    o2: raw[1],
                                                    o3: raw[2],
                                                    o4: raw[3]
                                                )
                                            case .ipv6(let addr):
                                                let raw = addr.rawValue
                                                address = IpAddr.v6(
                                                    o1: raw[0],
                                                    o2: raw[1],
                                                    o3: raw[2],
                                                    o4: raw[3],
                                                    o5: raw[4],
                                                    o6: raw[5],
                                                    o7: raw[6],
                                                    o8: raw[7],
                                                    o9: raw[8],
                                                    o10: raw[9],
                                                    o11: raw[10],
                                                    o12: raw[11],
                                                    o13: raw[12],
                                                    o14: raw[13],
                                                    o15: raw[14],
                                                    o16: raw[15]
                                                )
                                            default:
                                                DispatchQueue.main.async {
                                                    dataModel.sheetState =
                                                    SheetState.failedToConnect(
                                                        deviceName: device.name,
                                                        reason:
                                                            "No address available"
                                                    )
                                                }
                                                return
                                            }
                                            let info = DeviceInfo(
                                                name: device.name,
                                                type: device.proto,
                                                addresses: [address],
                                                port: port.rawValue
                                            )
                                            activeDevice =
                                            castContext.createDeviceFromInfo(
                                                info: info
                                            )
                                            do {
                                                try activeDevice?.connect(
                                                    eventHandler: eventHandler
                                                )
                                            } catch {
                                                DispatchQueue.main.async {
                                                    dataModel.sheetState =
                                                    SheetState.failedToConnect(
                                                        deviceName: device.name,
                                                        reason: "Unknown"
                                                    )
                                                }
                                            }
                                        }
                                    default:
                                        break
                                    }
                                }
                                conn.start(queue: .global())
                            }
                        },
                         onConnectScanned: { scannedDeviceInfo in
                             activeDevice =
                             castContext.createDeviceFromInfo(
                                info: scannedDeviceInfo
                             )
                             do {
                                 try activeDevice?.connect(
                                    eventHandler: eventHandler
                                 )
                             } catch {
                                 DispatchQueue.main.async {
                                     dataModel.sheetState =
                                     SheetState.failedToConnect(
                                        deviceName: scannedDeviceInfo.name,
                                        reason: "Unknown"
                                     )
                                 }
                             }
                         }
                    )
                    .presentationDetents([.medium, .large])
                case .connecting(let deviceName):
                    VStack {
                        ProgressView("Connecting to \(deviceName)")
                            .progressViewStyle(CircularProgressViewStyle())
                        Button(action: {
                        }) {
                            Text("Cancel")
                        }
                    }
                    .presentationDetents([.medium, .large])
                case .failedToConnect(let deviceName, let reason):
                    VStack {
                        Text("Failed to connect to \(deviceName)")
                        Text("Reason: \(reason)")
                    }
                    .presentationDetents([.medium])
                    .onDisappear {
                        dataModel.sheetState = SheetState.deviceList
                    }
                case .connected:
                    VStack {
                        if let devName = activeDevice?.name() {
                            (Text("Connected to ") + Text(devName).bold())
                                .padding(.top)
                        }

                        Spacer()

                        Text("Position")
                        Slider(
                            value: $dataModel.time,
                            in: 0.0...dataModel.duration,
                            onEditingChanged: { editing in
                                if !editing {
                                    do {
                                        try activeDevice?.seek(
                                            timeSeconds: dataModel.time
                                        )
                                    } catch {
                                        print("Failed to seek")
                                    }
                                }
                            },
                        )

                        Text("Volume")
                        Slider(
                            value: $dataModel.volume,
                            in: 0.0...1.0,
                            onEditingChanged: { editing in
                                if !editing {
                                    do {
                                        try activeDevice?.changeVolume(
                                            volume: dataModel.volume
                                        )
                                    } catch {
                                        print("Failed to change volume")
                                    }
                                }
                            }
                        )

                        HStack {
                            Spacer()
                            
                            Button(action: {
                                do {
                                    try activeDevice?.pausePlayback()
                                } catch {
                                    print("Failed to pause playback")
                                }
                            }) {
                                Image(systemName: "pause").font(.system(size: 42))
                            }

                            Spacer()
                            
                            Button(action: {
                                do {
                                    try activeDevice?.resumePlayback()
                                } catch {
                                    print("Failed to resume playback")
                                }
                            }) {
                                Image(systemName: "play").font(.system(size: 42))
                            }

                            Spacer()
                            
                            Button(action: {
                                do {
                                    try activeDevice?.stopPlayback()
                                } catch {
                                    print("Failed to stop playback")
                                }
                            }) {
                                Image(systemName: "stop").font(.system(size: 42))
                            }
                            
                            Spacer()
                        }

                        Spacer()

                        Button("Disconnect") {
                            do {
                                try activeDevice?.disconnect()
                            } catch {
                                print("Failed to disconnect device")
                            }
                            activeDevice = nil
                            dataModel.sheetState = SheetState.deviceList
                        }
                        .padding()
                    }
                    .presentationDetents([.medium, .large])
                }
            }
        }
    }
}

struct DeviceList: View {
    var devices: [FoundDevice]
    var onConnect: (FoundDevice) -> Void
    var onConnectScanned: (DeviceInfo) -> Void
    @State var isPresentingQrScanner = false

    var body: some View {
        VStack {
            List(devices, id: \.name) { device in
                Button(action: {
                    onConnect(device)
                }) {
                    HStack {
                        // TODO: change these icons
                        switch device.proto {
                        case .chromecast:
                            Image("chromecast-icon")
                                .renderingMode(.template)
                                .resizable()
                                .scaledToFit()
                                .frame(maxWidth: 32)
                        default:
                            Image(systemName: "questionmark.app.dashed")
                        }
                        Text(device.name)
                    }
                }
            }

            Text("Not seeing your receiver?")

            Button("Scan QR", systemImage: "qrcode.viewfinder") {
                isPresentingQrScanner.toggle()
            }
        }
        .sheet(isPresented: $isPresentingQrScanner) {
            CodeScannerView(codeTypes: [.qr]) { response in
                if case let .success(result) = response {
                    isPresentingQrScanner = false
                    if let deviceInfo = deviceInfoFromUrl(url: result.string) {
                        onConnectScanned(deviceInfo)
                    }
                }
            }
        }
    }
}

struct MediaPicker: UIViewControllerRepresentable {
    var onComplete: (String, URL) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(onComplete: onComplete)
    }

    func makeUIViewController(context: Context) -> PHPickerViewController {
        var config = PHPickerConfiguration(photoLibrary: .shared())
        config.filter = .any(of: [.images, .videos])
        config.selectionLimit = 1

        let picker = PHPickerViewController(configuration: config)
        picker.delegate = context.coordinator
        return picker
    }

    func updateUIViewController(
        _ uiViewController: PHPickerViewController,
        context: Context
    ) {}

    class Coordinator: NSObject, PHPickerViewControllerDelegate {
        let onComplete: (String, URL) -> Void

        init(onComplete: @escaping (String, URL) -> Void) {
            self.onComplete = onComplete
        }

        func picker(
            _ picker: PHPickerViewController,
            didFinishPicking results: [PHPickerResult]
        ) {
            picker.dismiss(animated: true)
            guard let item = results.first?.itemProvider else { return }
            print(item.registeredContentTypes)
            guard
                var contentType = item
                    .registeredContentTypes
                    .makeIterator()
                    .map({ it in return it.preferredMIMEType })
                    .filter({ it in it != nil })
                    .first ?? "application/octet-stream"
            else {
                print("Unable to get content type")
                return
            }
            if contentType == "video/quicktime" {
                contentType = "video/mp4"
            }

            let matchingTypes = [
                UTType.image.identifier,
                UTType.movie.identifier,
            ]
            for typeId in matchingTypes {
                if item.hasItemConformingToTypeIdentifier(typeId) {
                    item.loadFileRepresentation(forTypeIdentifier: typeId) {
                        tempURL,
                        error in
                        if let error = error {
                            // TODO: error
                            return
                        }
                        guard let tempURL = tempURL else {
                            // TODO: error
                            return
                        }
                        self.onComplete(contentType, tempURL)
                    }
                    break
                }
            }
        }
    }
}
