use std::sync::Arc;

use fcast_sender_sdk::{
    context::CastContext,
    device::{
        DeviceConnectionState, DeviceEventHandler, DeviceInfo, GenericKeyEvent, GenericMediaEvent,
        PlaybackState, ProtocolType, Source,
    },
    IpAddr,
};
use log::info;

struct EventHandler {}

impl DeviceEventHandler for EventHandler {
    fn connection_state_changed(&self, state: DeviceConnectionState) {
        info!("Connection state changed: {state:?}");
    }

    fn volume_changed(&self, volume: f64) {
        info!("Volume changed: {volume}");
    }

    fn time_changed(&self, time: f64) {
        info!("Time changed: {time}");
    }

    fn playback_state_changed(&self, state: PlaybackState) {
        info!("Playback state changed: {state:?}");
    }

    fn duration_changed(&self, duration: f64) {
        info!("Duration changed: {duration}");
    }

    fn speed_changed(&self, speed: f64) {
        info!("Speed changed: {speed}");
    }

    fn source_changed(&self, source: Source) {
        info!("Source changed: {source:?}");
    }

    fn key_event(&self, event: GenericKeyEvent) {
        info!("Key event: {event:?}");
    }

    fn media_event(&self, event: GenericMediaEvent) {
        info!("Media event: {event:?}");
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    let ctx = CastContext::new().unwrap();

    let dev = ctx.create_device_from_info(DeviceInfo {
        name: "FCast testing device".to_owned(),
        r#type: ProtocolType::FCast,
        addresses: vec![IpAddr::v4(127, 0, 0, 1)],
        port: 46899,
    });

    dev.connect(Arc::new(EventHandler {})).unwrap();

    info!("Press enter load demo video");
    std::io::stdin().read_line(&mut String::new()).unwrap();

    dev.load_video(
        "video/mp4".to_string(),
        "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4"
            .to_string(),
        0.0,
        None,
        None,
    )
    .unwrap();

    info!("Press enter quit");
    std::io::stdin().read_line(&mut String::new()).unwrap();
}
