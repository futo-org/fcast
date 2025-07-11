import SwiftUI
import Synchronization
import Combine

@MainActor
class DataModel: ObservableObject {
    @Published var playbackState = PlaybackState.idle
    @Published var volume = 1.0
    @Published var time = 0.0
    @Published var duration = 0.0
}

final class EventHandler: CastingDeviceEventHandler {
    private let dataModel: Mutex<DataModel>
    
    init(data: DataModel) {
        dataModel = Mutex(data)
    }
    
    func connectionStateChanged(state: CastConnectionState) {
        print(state)
    }
    
    func volumeChanged(volume: Double) {
        dataModel.withLock { dataModel in
            DispatchQueue.main.async { [dataModel] in
                dataModel.volume = volume
            }
        }
    }
    
    func timeChanged(time: Double) {
        dataModel.withLock { dataModel in
            DispatchQueue.main.async { [dataModel] in
                dataModel.time = time
            }
        }
    }
    
    func playbackStateChanged(state: PlaybackState) {
        dataModel.withLock { dataModel in
            DispatchQueue.main.async { [dataModel] in
                dataModel.playbackState = state
            }
        }
    }
    
    func durationChanged(duration: Double) {
        dataModel.withLock { dataModel in
            DispatchQueue.main.async { [dataModel] in
                dataModel.duration = duration
            }
        }
    }
    
    func speedChanged(speed: Double) {
        print(speed)
    }
}

final class SwiftCastingDevice {
    var eventHandler: CastingDeviceEventHandler
    var castingDevice: CastingDevice
    
    init(dataModel: DataModel) {
        initLogger()
        eventHandler = EventHandler(data: dataModel)
        castingDevice = FCastCastingDevice
            .newWithEventHandler(
                deviceInfo: CastingDeviceInfo(
                    name: "Test",
                    type: CastProtocolType.fCast,
                    addresses: [IpAddr.v4(q1: 127, q2: 0, q3: 0, q4: 1)],
                    port: 46899
                ),
                eventHandler: eventHandler
            )
    }
}

@main
struct FCast_SenderApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView(data: DataModel())
        }
    }
}
