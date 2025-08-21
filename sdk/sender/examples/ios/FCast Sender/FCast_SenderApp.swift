import SwiftUI
import Synchronization
import Combine
import Network

struct FoundDevice {
    var name: String
    var endpoint: NWEndpoint
    var proto: ProtocolType
}

enum SheetState {
    case deviceList
    case connecting(deviceName: String)
    case failedToConnect(deviceName: String, reason: String)
    case connected
}

@MainActor
class DataModel: ObservableObject {
    @Published var playbackState = PlaybackState.idle
    @Published var volume = 1.0
    @Published var time = 0.0
    @Published var duration = 0.0
    @Published var speed = 1.0
    @Published var devices: Array<FoundDevice> = Array()
    @Published var showingDeviceList = false
    @Published var showingConnectingToDevice = false
    @Published var showingFailedToConnect = false
    @Published var isShowingSheet = false
    @Published var sheetState = SheetState.deviceList
    @Published var usedLocalAddress: IpAddr? = nil
}

@main
struct FCast_SenderApp: App {
    var body: some Scene {
        WindowGroup {
            try! ContentView(data: DataModel())
        }
    }
}
