import SwiftUI

struct ContentView: View {
    var castingDevice: SwiftCastingDevice
    @ObservedObject var dataModel: DataModel
    
    init(data: DataModel) {
        dataModel = data
        self.castingDevice = SwiftCastingDevice(dataModel: data)
    }
    
    var body: some View {
        VStack {
            Button("Start", action: {
                castingDevice.castingDevice.start()
            })
            Button("Send Play", action: {
                castingDevice.castingDevice.loadVideo(
                    streamType: "",
                    contentType: "video/mp4",
                    contentId: "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4",
                    resumePosition: 0.0,
                    duration: 0.0,
                    speed: 1.0
                )
            })
            Slider(
                value: $dataModel.volume,
                in: 0.0...1.0,
                onEditingChanged: { editing in
                    if !editing {
                        castingDevice.castingDevice.changeVolume(volume: $dataModel.volume.wrappedValue)
                    }
                }
            )
            if dataModel.playbackState != PlaybackState.idle {
                Slider(
                    value: $dataModel.time,
                    in: 0.0...dataModel.duration,
                    onEditingChanged: { editing in
                        if !editing {
                            castingDevice.castingDevice.seek(timeSeconds: $dataModel.time.wrappedValue)
                        }
                    }
                )
            }
            if dataModel.playbackState == PlaybackState.playing {
                Button("Pause", action: {
                    castingDevice.castingDevice.pausePlayback()
                })
            } else if dataModel.playbackState == PlaybackState.paused {
                Button("Play", action: {
                    castingDevice.castingDevice.resumePlayback()
                })
            }
        }
        .padding()
    }
}
