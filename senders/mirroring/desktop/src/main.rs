#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;
use clap::Parser;
use desktop_mirroring::FetchEvent;
#[cfg(target_os = "macos")]
use desktop_mirroring::macos;
use fcast_sender_sdk::{
    context::CastContext,
    device::{self, DeviceFeature, DeviceInfo},
};
use mcore::{AudioSource, Event, SourceConfig, VideoSource, transmission::WhepSink, ShouldQuit};
use std::{collections::HashMap, rc::Rc, sync::Arc};
use tokio::{
    runtime::Runtime,
    sync::mpsc::{Sender, channel},
};
use tracing::{debug, error, info, level_filters::LevelFilter};

slint::include_modules!();

pub type ProducerId = String;

struct Application {
    cast_ctx: CastContext,
    tx_sink: Option<WhepSink>,
    ui_weak: slint::Weak<MainWindow>,
    event_tx: Sender<Event>,
    devices: HashMap<String, DeviceInfo>,
    video_sources: Vec<(usize, VideoSource)>,
    audio_sources: Vec<(usize, AudioSource)>,
    video_source_fetcher_tx: Sender<FetchEvent>,
    current_device_id: usize,
    active_device: Option<Arc<dyn device::CastingDevice>>,
    local_address: Option<fcast_sender_sdk::IpAddr>,
    our_source_url: Option<String>,
}

impl Application {
    /// Must be called from a tokio runtime.
    pub fn new(ui_weak: slint::Weak<MainWindow>, event_tx: Sender<Event>) -> Result<Self> {
        let cast_ctx = CastContext::new()?;
        cast_ctx.start_discovery(Arc::new(mcore::Discoverer::new(
            event_tx.clone(),
            tokio::runtime::Handle::current(),
        )));

        #[allow(unused_mut)]
        let (video_source_fetcher_tx, mut video_source_fetcher_rx) = channel::<FetchEvent>(10);

        #[cfg(target_os = "linux")]
        {
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                desktop_mirroring::linux::video_source_fetch_worker(
                    video_source_fetcher_rx,
                    event_tx,
                )
                .await;
            });
        }

        #[cfg(target_os = "macos")]
        {
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                loop {
                    let Some(event) = video_source_fetcher_rx.recv().await else {
                        error!("Failed to receive new video source fetcher event");
                        break;
                    };

                    match event {
                        FetchEvent::Fetch => match macos::get_video_sources() {
                            Ok(sources) => {
                                event_tx
                                    .send(Event::VideosAvailable(
                                        sources
                                            .into_iter()
                                            .enumerate()
                                            .map(|(idx, src)| (idx, src))
                                            .collect(),
                                    ))
                                    .await
                                    .expect("event loop is not running");
                            }
                            Err(err) => {
                                error!("Failed to get video sources: {err}");
                            }
                        },
                        FetchEvent::Quit => break,
                        FetchEvent::ClearState => (),
                    }
                }

                debug!("Video source fetch loop quit");
            });
        }

        #[cfg(target_os = "windows")]
        {
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                loop {
                    let Some(event) = video_source_fetcher_rx.recv().await else {
                        error!("Failed to receive new video source fetcher event");
                        break;
                    };

                    match event {
                        FetchEvent::Fetch => {
                            use gst::prelude::*;
                            let Some(dev_provider) = gst::DeviceProviderFactory::by_name(
                                "d3d11screencapturedeviceprovider",
                            ) else {
                                error!("Failed to create `d3d11screencapturedeviceprovider`");
                                continue;
                            };

                            if let Err(err) = dev_provider.start() {
                                error!("Failed to start d3d11 device provider: {err}");
                                continue;
                            }
                            let devs = dev_provider.devices();
                            dev_provider.stop();

                            let mut converted_devs = Vec::new();

                            for (idx, dev) in devs.iter().enumerate() {
                                let Some(props) = dev.properties() else {
                                    error!("Could not get device properties");
                                    continue;
                                };
                                let name = dev.display_name().to_string();
                                let handle = match props.get::<u64>("device.hmonitor") {
                                    Ok(handle) => handle,
                                    Err(err) => {
                                        error!(
                                            "Failed to get the `device.hmonitor` property from the device: {err}"
                                        );
                                        continue;
                                    }
                                };
                                converted_devs
                                    .push((idx, VideoSource::D3d11Monitor { name, handle }));
                            }

                            event_tx
                                .send(Event::VideosAvailable(converted_devs))
                                .await
                                .expect("event loop is not running");
                        }
                        FetchEvent::Quit => break,
                        FetchEvent::ClearState => (),
                    }
                }

                debug!("Video source fetch loop quit");
            });
        }

        Ok(Self {
            cast_ctx,
            tx_sink: None,
            ui_weak,
            event_tx,
            devices: HashMap::new(),
            video_sources: Vec::new(),
            audio_sources: Vec::new(),
            video_source_fetcher_tx,
            current_device_id: 0,
            active_device: None,
            local_address: None,
            our_source_url: None,
        })
    }

    fn disconnect_active_device(&mut self, stop_playback: bool) {
        if let Some(device) = self.active_device.take() {
            tokio::spawn(async move {
                if stop_playback {
                    if let Err(err) = device.stop_playback() {
                        error!(?err, "Failed to stop playback");
                    }
                    // NOTE: Instead of waiting for the PlaybackState::Idle event in the main loop we just sleep here
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                if let Err(err) = device.disconnect() {
                    error!(?err, "Failed to disconnect from device");
                }
            });
        }
    }

    fn shutdown_sink(&mut self) {
        if let Some(mut tx_sink) = self.tx_sink.take() {
            tx_sink.shutdown();
        }
    }

    async fn stop_cast(&mut self, stop_playback: bool) -> Result<()> {
        self.disconnect_active_device(stop_playback);

        self.shutdown_sink();

        self.video_source_fetcher_tx
            .send(FetchEvent::ClearState)
            .await?;

        self.ui_weak.upgrade_in_event_loop(|ui| {
            ui.global::<Bridge>()
                .invoke_change_state(AppState::Disconnected);
        })?;

        self.our_source_url = None;

        Ok(())
    }

    fn update_receivers_in_ui(&mut self) -> Result<()> {
        let receivers = self
            .devices
            .keys()
            .map(slint::SharedString::from)
            .collect::<Vec<slint::SharedString>>();
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let model = Rc::new(slint::VecModel::<slint::SharedString>::from_iter(
                receivers.into_iter(),
            ));
            ui.global::<Bridge>().set_devices(model.into());
        })?;

        Ok(())
    }

    fn add_or_update_device(&mut self, device_info: DeviceInfo) -> Result<()> {
        self.devices.insert(device_info.name.clone(), device_info);
        self.update_receivers_in_ui()?;
        Ok(())
    }

    /// Returns `true` if the event loop should quit
    async fn handle_event(&mut self, event: Event) -> Result<ShouldQuit> {
        let span = tracing::span!(tracing::Level::DEBUG, "handle_event");
        let _enter = span.enter();

        debug!(?event, "Handling event");

        match event {
            Event::StartCast {
                video_uid,
                audio_uid,
            } => {
                debug!(?self.video_sources, "Video sources");

                let video_sources = std::mem::take(&mut self.video_sources);
                let video_src = match video_uid {
                    Some(uid) => video_sources
                        .into_iter()
                        .find(|(id, _)| uid == *id)
                        .map(|(_, dev)| dev),
                    None => None,
                };

                debug!(?self.audio_sources, "Audio sources");

                let audio_sources = std::mem::take(&mut self.audio_sources);
                let audio_src = match audio_uid {
                    Some(uid) => audio_sources
                        .into_iter()
                        .find(|(id, _)| uid == *id)
                        .map(|(_, dev)| dev),
                    None => None,
                };

                let source_config = match (video_src, audio_src) {
                    (Some(video), Some(audio)) => SourceConfig::AudioVideo { video, audio },
                    (Some(video), None) => SourceConfig::Video(video),
                    (None, Some(audio)) => SourceConfig::Audio(audio),
                    _ => unreachable!(),
                };

                debug!("Adding WHEP pipeline");
                self.tx_sink = Some(mcore::transmission::WhepSink::new(
                    source_config,
                    self.event_tx.clone(),
                    tokio::runtime::Handle::current(),
                )?);
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>()
                        .invoke_change_state(AppState::StartingCast);
                })?;
            }
            Event::StopCast => self.stop_cast(true).await?,
            Event::ConnectToDevice(device_name) => match self.devices.get(&device_name) {
                Some(device_info) => {
                    if device_info.addresses.is_empty() || device_info.port == 0 {
                        error!(?device_info, "Device is missing an address or port");
                        return Ok(ShouldQuit::No);
                    }

                    debug!(?device_info, "Trying to connect");
                    let device = self.cast_ctx.create_device_from_info(device_info.clone());
                    self.current_device_id += 1;
                    if let Err(err) = device.connect(
                        None,
                        Arc::new(mcore::DeviceHandler::new(
                            self.current_device_id,
                            self.event_tx.clone(),
                            tokio::runtime::Handle::current(),
                        )),
                        1000,
                    ) {
                        error!(?err);
                        self.ui_weak.upgrade_in_event_loop(|ui| {
                            ui.global::<Bridge>()
                                .invoke_change_state(AppState::Disconnected);
                        })?;
                        return Ok(ShouldQuit::No);
                    }
                    self.active_device = Some(device);
                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        let bridge = ui.global::<Bridge>();
                        bridge.set_device_name(slint::SharedString::from(device_name));
                        bridge.invoke_change_state(AppState::Connecting);
                    })?;
                }
                None => error!(device_name, "Device not found"),
            },
            Event::SignallerStarted { bound_port } => {
                let Some(addr) = self.local_address.as_ref() else {
                    error!("Local address is missing");
                    return Ok(ShouldQuit::No);
                };

                let (content_type, url) = self
                    .tx_sink
                    .as_ref()
                    .unwrap()
                    .get_play_msg(addr.into(), bound_port);

                debug!(content_type, url, "Sending play message");

                self.our_source_url = Some(url.clone());

                match self.active_device.as_ref() {
                    Some(device) => {
                        device.load(device::LoadRequest::Url {
                            content_type,
                            url,
                            resume_position: None,
                            speed: None,
                            volume: None,
                            metadata: None,
                            request_headers: None,
                        })?;
                    }
                    None => error!("Active device is missing, cannot send play message"),
                }

                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>().invoke_change_state(AppState::Casting);
                })?;

                // if let Some(tx_sink) = self.tx_sink.as_ref() {
                //     use gst::prelude::*;
                //     tx_sink.pipeline.debug_to_dot_file(gst::DebugGraphDetails::ALL, "sender-pipeline");
                // }
            }
            Event::Quit => return Ok(ShouldQuit::Yes),
            Event::VideosAvailable(sources) => {
                self.video_sources = sources;
                self.update_video_sources_in_ui()?;
            }
            Event::ReloadVideoSources => {
                self.video_source_fetcher_tx.send(FetchEvent::Fetch).await?
            }
            Event::DeviceAvailable(device_info) => self.add_or_update_device(device_info)?,
            Event::DeviceRemoved(device_name) => {
                self.devices.remove(&device_name);
            }
            Event::DeviceChanged(device_info) => self.add_or_update_device(device_info)?,
            Event::FromDevice { id, event } if id == self.current_device_id => match event {
                mcore::DeviceEvent::StateChanged(new_state) => match new_state {
                    device::DeviceConnectionState::Disconnected => (),
                    device::DeviceConnectionState::Connecting => (),
                    device::DeviceConnectionState::Reconnecting => self.shutdown_sink(),
                    device::DeviceConnectionState::Connected { local_addr, .. } => {
                        if let Some(active_device) = &self.active_device {
                            if !active_device.supports_feature(DeviceFeature::WhepStreaming) {
                                info!("Device does not support WHEP streaming");
                                self.disconnect_active_device(false);
                                self.ui_weak.upgrade_in_event_loop(|ui| {
                                    ui.global::<Bridge>()
                                        .invoke_change_state(AppState::UnsupportedReceiver);
                                })?;
                                return Ok(ShouldQuit::No);
                            }
                        }

                        debug!("Device connected");
                        self.local_address = Some(local_addr);

                        self.video_source_fetcher_tx.send(FetchEvent::Fetch).await?;

                        #[cfg(target_os = "linux")]
                        {
                            self.audio_sources = vec![(0, AudioSource::PulseVirtualSink)];
                            self.update_audio_sources_in_ui()?;
                        }

                        self.ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.global::<Bridge>()
                                .invoke_change_state(AppState::SelectingSource);
                        })?;
                    }
                },
                mcore::DeviceEvent::SourceChanged(new_source) => {
                    if self.tx_sink.is_some() {
                        match new_source {
                            fcast_sender_sdk::device::Source::Url { ref url, .. } => {
                                if Some(url) != self.our_source_url.as_ref() {
                                    // At this point the receiver has stopped playing our stream
                                    debug!(
                                        ?new_source,
                                        "The source on the receiver changed, disconnecting"
                                    );
                                    self.stop_cast(false).await?;
                                }
                            }
                            _ => (),
                        }
                    }
                }
            },
            Event::FromDevice { id, .. } => {
                debug!(
                    id,
                    current = self.current_device_id,
                    "Got event from old device",
                );
            }
            #[cfg(target_os = "linux")]
            Event::UnsupportedDisplaySystem => {
                error!("Unsupported display system");
                return Ok(ShouldQuit::Yes);
            }
        }

        Ok(ShouldQuit::No)
    }

    fn update_audio_sources_in_ui(&self) -> Result<()> {
        let audio_sources = self.audio_sources.clone();
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let audio_devs = slint::VecModel::<slint::ModelRc<AudioSourceModel>>::from_iter(
                audio_sources.chunks(3).map(|row| {
                    slint::ModelRc::<AudioSourceModel>::new(
                        slint::VecModel::<AudioSourceModel>::from_iter(row.iter().map(|dev| {
                            AudioSourceModel {
                                name: slint::SharedString::from(dev.1.display_name().as_str()),
                                uid: dev.0 as i32,
                            }
                        })),
                    )
                }),
            );

            ui.global::<Bridge>()
                .set_audio_sources(Rc::new(audio_devs).into());
        })?;

        Ok(())
    }

    fn update_video_sources_in_ui(&mut self) -> Result<()> {
        self.video_sources
            .sort_by(|a, b| a.1.display_name().cmp(&b.1.display_name()));

        let video_sources = self
            .video_sources
            .iter()
            .map(|(uid, s)| (*uid, s.display_name()))
            .collect::<Vec<(usize, String)>>();
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let video_devs = slint::VecModel::<slint::ModelRc<VideoSourceModel>>::from_iter(
                video_sources.chunks(3).map(|row| {
                    slint::ModelRc::<VideoSourceModel>::new(
                        slint::VecModel::<VideoSourceModel>::from_iter(row.iter().map(|dev| {
                            VideoSourceModel {
                                name: slint::SharedString::from(dev.1.as_str()),
                                uid: dev.0 as i32,
                            }
                        })),
                    )
                }),
            );

            ui.global::<Bridge>()
                .set_video_sources(Rc::new(video_devs).into());
        })?;

        Ok(())
    }

    pub async fn run_event_loop(
        mut self,
        mut event_rx: tokio::sync::mpsc::Receiver<Event>,
    ) -> Result<()> {
        tracing_gstreamer::integrate_events();
        gst::log::remove_default_log_function();
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        gst::init()?;

        loop {
            let Some(event) = event_rx.recv().await else {
                debug!("No more events");
                break;
            };

            if self.handle_event(event).await? == ShouldQuit::Yes {
                break;
            }
        }

        debug!("Quitting event loop");

        if let Some(mut tx_sink) = self.tx_sink.take() {
            tx_sink.shutdown();
        }

        self.video_source_fetcher_tx.send(FetchEvent::Quit).await?;

        let _ = slint::quit_event_loop();

        Ok(())
    }
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {}

fn log_level() -> LevelFilter {
    match std::env::var("FCAST_LOG") {
        Ok(level) => match level.to_ascii_lowercase().as_str() {
            "error" => LevelFilter::ERROR,
            "warn" => LevelFilter::WARN,
            "info" => LevelFilter::INFO,
            "debug" => LevelFilter::DEBUG,
            "trace" => LevelFilter::TRACE,
            _ => LevelFilter::OFF,
        },
        #[cfg(debug_assertions)]
        Err(_) => LevelFilter::DEBUG,
        #[cfg(not(debug_assertions))]
        Err(_) => LevelFilter::OFF,
    }
}

fn main() -> Result<()> {
    let init_start = std::time::Instant::now();

    let _cli = Cli::parse();

    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(log_level())
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;

    tracing_log::LogTracer::init()?;

    let runtime = Runtime::new()?;

    let (event_tx, event_rx) = channel::<Event>(100);

    let ui = MainWindow::new()?;

    let event_loop_jh = runtime.spawn({
        let ui_weak = ui.as_weak();
        let event_tx = event_tx.clone();
        async move {
            Application::new(ui_weak, event_tx)
                .unwrap()
                .run_event_loop(event_rx)
                .await
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_connect_to_device({
        let event_tx = event_tx.clone();
        move |device_name| {
            if let Err(err) =
                event_tx.blocking_send(Event::ConnectToDevice(device_name.to_string()))
            {
                error!("on_connect_to_device: failed to send event: {err}");
            }
        }
    });

    ui.global::<Bridge>().on_start_cast({
        let event_tx = event_tx.clone();
        move |video_uid, audio_uid| {
            event_tx
                .blocking_send(Event::StartCast {
                    video_uid: if video_uid >= 0 {
                        Some(video_uid as usize)
                    } else {
                        None
                    },
                    audio_uid: if audio_uid >= 0 {
                        Some(audio_uid as usize)
                    } else {
                        None
                    },
                })
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_stop_cast({
        let event_tx = event_tx.clone();
        move || {
            event_tx.blocking_send(Event::StopCast).unwrap();
        }
    });

    ui.global::<Bridge>().on_reload_video_sources({
        let event_tx = event_tx.clone();
        move || {
            event_tx.blocking_send(Event::ReloadVideoSources).unwrap();
        }
    });

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    ui.global::<Bridge>().set_is_audio_supported(false);

    #[cfg(target_os = "linux")]
    ui.global::<Bridge>().set_is_audio_supported(true);

    // event_handler!(event_tx, |event_tx: tokio::sync::mpsc::Sender<Event>| {
    //     ui.on_add_receiver_manually(move |name, addr, port| {
    //         let parsed_addr = match format!("{addr}:{port}").parse::<std::net::SocketAddr>() {
    //             Ok(a) => a,
    //             Err(err) => {
    //                 // TODO: show in UI
    //                 error!("Failed to parse manually added receiver socket address: {err}");
    //                 return;
    //             }
    //         };
    //         todo!();
    //         // event_tx
    //         //     .blocking_send(Event::ReceiverAvailable {
    //         //         name: name.to_string(),
    //         //         addresses: vec![parsed_addr],
    //         //     })
    //         //     .unwrap();
    //     });
    // });

    let init_finished_in = init_start.elapsed();
    debug!(?init_finished_in, "Initialization finished");

    ui.run()?;

    runtime.block_on(async move {
        event_tx.send(Event::Quit).await.unwrap();
        event_loop_jh.await.unwrap();
    });

    Ok(())
}
