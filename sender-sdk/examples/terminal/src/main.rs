use std::sync::Arc;

use fcast_sender_sdk::{
    /* airplay2::AirPlay2CastingDevice, */ casting_device::{
        DeviceConnectionState, DeviceEventHandler, DeviceInfo, GenericKeyEvent,
        GenericMediaEvent, PlaybackState, Source,
    }, context::CastContext, IpAddr
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
    #[cfg(debug_assertions)]
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    #[cfg(not(debug_assertions))]
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // let dev = FCastCastingDevice::new(
    //     CastingDeviceInfo {
    //         name: "Testing".to_owned(),
    //         r#type: CastProtocolType::FCast,
    //         addresses: vec![IpAddr::v4(127, 0, 0, 1)],
    //         port: 46899,
    //     },
    // );

    // let dev = ChromecastCastingDevice::new(
    //     CastingDeviceInfo {
    //         name: "Chromecast Testing".to_owned(),
    //         r#type: CastProtocolType::Chromecast,
    //         // avahi-browse --all --resolve
    //         addresses: vec![IpAddr::v4(192, 168, 1, 37)],
    //         port: 8009,
    //     },
    // );

    // dev.start(Arc::new(EventHandler {}));

    // info!("Sleeping for 5s...");
    // std::thread::sleep(std::time::Duration::from_secs(5));

    // info!("Loading video...");
    // dev.load_video(
    //     "".to_string(),
    //     "video/mp4".to_string(),
    //     "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4"
    //         .to_string(),
    //     0.0,
    //     0.0,
    //     Some(1.0),
    // );
    // dev.subscribe_event(GenericEventSubscriptionGroup::Keys);
    // dev.subscribe_event(GenericEventSubscriptionGroup::Media);

    // let manager = CastingManager::new(Arc::new(ManagerEventHandler {}));

    let ctx = CastContext::new().unwrap();

    // let dev = ctx.create_device_from_info(DeviceInfo::fcast(
    //     "AirPlay2".to_owned(),
    //     vec![IpAddr::v4(127, 0, 0, 1)],
    //     46899,
    // ));


    // sonos: vec![IpAddr::v4(192, 168, 1, 203)],

    let dev = ctx.create_device_from_info(DeviceInfo::airplay2(
        "AirPlay2".to_owned(),
        // vec![IpAddr::v4(192, 168, 1, 133)],
        // 7000,

        // Sonos
        vec![IpAddr::v4(192, 168, 1, 203)],
        7000,

        // Mac
        // vec![IpAddr::v4(192, 168, 1, 64)],
        // 7000,

        // uxplay
        // vec![IpAddr::v4(192, 168, 1, 133)],
        // 1338,

        // shairport sync
        // vec![IpAddr::v4(192, 168, 1, 133)],
        // 7000,
    ));

    dev.connect(Arc::new(EventHandler {})).unwrap();

    info!("Press enter to quit");
    std::io::stdin().read_line(&mut String::new()).unwrap();

    // dev.stop().unwrap();

    // dev.stop_casting();

    // info!("Sleeping for 1s...");
    // tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}
