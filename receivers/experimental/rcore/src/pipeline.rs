use fcast_protocol::PlaybackState;
use futures::StreamExt;
use gst::prelude::*;

use anyhow::{Result, anyhow, bail};

use tokio::sync::mpsc::Sender;
use tracing::{debug, error};

use crate::log_if_err;

#[derive(thiserror::Error, Debug)]
pub enum SetPlaybackUriError {
    #[error("unsupported resource scheme")]
    UnsupportedResourceScheme,
    #[error("invalid URI")]
    InvalidUri,
    #[error("{0}")]
    PipelineStateChange(gst::StateChangeError),
}

#[derive(Debug)]
pub struct PipelinePlaybackState {
    pub time: f64,
    pub duration: f64,
    pub state: PlaybackState,
    pub speed: f64,
}

pub struct Pipeline {
    inner: gst::Pipeline,
    playbin: gst::Element,
}

impl Pipeline {
    pub async fn new(appsink: gst::Element, event_tx: Sender<crate::Event>) -> Result<Self> {
        let pipeline = gst::Pipeline::new();

        // TODO: handle `BUFFERING` messages as described in the playbin3 docs:
        // https://gstreamer.freedesktop.org/documentation/playback/playbin3.html?gi-language=c#Buffering
        let playbin = gst::ElementFactory::make("playbin3").build()?;

        playbin.connect("element-setup", false, |vals| {
            let Ok(elem) = vals[1].get::<gst::Element>() else {
                return None;
            };

            if let Some(factory) = elem.factory()
                && factory.name() == "rtspsrc"
            {
                elem.set_property("latency", 25u32);
            }

            if let Some(factory) = elem.factory()
                && factory.name() == "webrtcbin"
            {
                elem.set_property("latency", 1u32);
            }

            None
        });

        pipeline.add(&playbin)?;

        tokio::spawn({
            let bus = pipeline.bus().ok_or(anyhow!("Pipeline without bus"))?;
            let event_tx = event_tx.clone();
            let playbin_weak = playbin.downgrade();

            async move {
                let mut messages = bus.stream();

                while let Some(msg) = messages.next().await {
                    use gst::MessageView;

                    match msg.view() {
                        MessageView::Eos(..) => {
                            log_if_err!(event_tx.send(crate::Event::PipelineEos).await);
                        }
                        MessageView::StateChanged(state_change) => {
                            let Some(playbin) = playbin_weak.upgrade() else {
                                continue;
                            };
                            if state_change.src().map(|s| s == &playbin).unwrap_or(false) {
                                let current = state_change.current();
                                // if current == gst::State::Playing {
                                //     let current_url: String = playbin.property("uri");
                                //     // TODO: get the rest...
                                // }
                                log_if_err!(
                                    event_tx
                                        .send(crate::Event::PipelineStateChanged(current))
                                        .await
                                );
                            }
                        }
                        MessageView::Error(err) => {
                            error!(
                                "Error from {:?}: {} ({:?})",
                                err.src().map(|s| s.path_string()),
                                err.error(),
                                err.debug()
                            );
                            log_if_err!(event_tx.send(crate::Event::PipelineError).await);
                        }
                        // MessageView::Tag(tag) => {
                        //     debug!("Tag bus message: {:?}", tag);
                        // }
                        MessageView::Buffering(buffering) => {
                            // TODO: buffering.percent() buffering.buffering_stats()
                        }
                        MessageView::DurationChanged(_) => {
                            // TODO: need to query the pipeline for the new duration
                        }
                        // MessageView::Toc(toc) => {}
                        MessageView::StreamCollection(collection) => {
                            let stream_collection = collection.stream_collection();
                            for stream in &stream_collection {
                                debug!(
                                    "Stream : caps={:?} type={:?} tags={:?}",
                                    stream.caps(),
                                    stream.stream_type(),
                                    stream.tags()
                                );

                                // TODO: get all tag info stuff

                                // caps=Some(Caps(audio/mpeg(memory:SystemMemory) { mpegversion: (gint) 4, framed: (gboolean) TRUE, stream-format: (gchararray) "raw", level: (gchararray) "2", base-profile: (gchararray) "lc", profile: (gchararray) "lc", codec_data: (GstBuffer) ((GstBuffer*) 0x7fff80131ee0), rate: (gint) 44100, channels: (gint) 2 })) type=StreamType(AUDIO) tags=Some(TagList { audio-codec: (gchararray) "MPEG-4 AAC", maximum-bitrate: (guint) 169368, bitrate: (guint) 125488, container-specific-track-id: (gchararray) "1" })

                                // Caps=Some(Caps(video/x-h264(memory:SystemMemory) { stream-format: (gchararray) "avc", alignment: (gchararray) "au", level: (gchararray) "3.1", profile: (gchararray) "high", codec_data: (GstBuffer) ((GstBuffer*) 0x7fff8014cf50), width: (gint) 1280, height: (gint) 720, framerate: (GstFraction) 24/1, pixel-aspect-ratio: (GstFraction) 1/1 })) type=StreamType(VIDEO) tags=None

                                // caps=Some(Caps(video/x-vp9(memory:SystemMemory) { width: (gint) 3840, height: (gint) 2160, framerate: (GstFraction) 30000/1001, colorimetry: (gchararray) "bt709", chroma-format: (gchararray) "4:2:0", bit-depth-luma: (guint) 8, bit-depth-chroma: (guint) 8, parsed: (gboolean) TRUE, alignment: (gchararray) "frame", profile: (gchararray) "0", codec-alpha: (gboolean) FALSE })) type=StreamType(VIDEO) tags=Some(TagList { video-codec: (gchararray) "VP9", language-code: (gchararray) "en", container-specific-track-id: (gchararray) "1", extended-comment: (gchararray) "DURATION=00:02:45.932000000", minimum-bitrate: (guint) 28531, maximum-bitrate: (guint) 662697, bitrate: (guint) 197454 })

                                // caps=Some(Caps(audio/x-opus(memory:SystemMemory) { rate: (gint) 48000, channels: (gint) 2, channel-mapping-family: (gint) 0, stream-count: (gint) 1, coupled-count: (gint) 1, streamheader: Array([(GstBuffer) ((GstBuffer*) 0x7fff740200a0), (GstBuffer) ((GstBuffer*) 0x7fff74020370)]) })) type=StreamType(AUDIO) tags=Some(TagList { audio-codec: (gchararray) "Opus", language-code: (gchararray) "en", container-specific-track-id: (gchararray) "2", extended-comment: (gchararray) "DURATION=00:02:45.968000000" })
                            }
                        }
                        MessageView::StreamsSelected(streams) => {}
                        _ => (),
                    }
                }
            }
        });

        playbin.set_property("video-sink", appsink);

        // TODO: can get latest frame after a seek with https://github.com/GNOME/totem/blob/b962a406da6d9e25b572e07fb8af41beb1a284dd/src/gst/totem-gst-pixbuf-helpers.c#L31 ?

        let audio_pitch = gst::ElementFactory::make("pitch").build()?;
        playbin.set_property("audio-filter", audio_pitch);

        pipeline.set_state(gst::State::Ready)?;

        // id = stream.stream_id
        // pipeline.send_event(gst::event::SelectStreams);

        Ok(Self {
            inner: pipeline,
            playbin,
        })
    }

    pub fn is_live(&self) -> bool {
        self.inner.is_live()
    }

    pub fn get_duration(&self) -> Option<gst::ClockTime> {
        self.inner.query_duration()
    }

    pub fn get_playback_state(&self) -> Result<PipelinePlaybackState> {
        let position: Option<gst::ClockTime> = self.inner.query_position();
        let duration = self.get_duration();

        let speed = {
            let mut query = gst::query::Segment::new(gst::Format::Time);
            if self.inner.query(&mut query) {
                query
                    .get_mut()
                    .unwrap() // We know the query succeeded
                    .result()
                    .0
            } else {
                1.0f64
            }
        };

        let state = {
            let state = self.inner.state(gst::ClockTime::from_mseconds(250));
            match state.0 {
                Ok(s) => {
                    if s != gst::StateChangeSuccess::Success {
                        bail!("timeout");
                    }
                }
                Err(err) => {
                    bail!("{err}");
                }
            }

            match state.1 {
                gst::State::Paused => PlaybackState::Paused,
                gst::State::Playing => PlaybackState::Playing,
                _ => PlaybackState::Idle,
            }
        };

        Ok(PipelinePlaybackState {
            time: position.unwrap_or_default().seconds_f64(),
            duration: duration.unwrap_or_default().seconds_f64(),
            state,
            speed,
        })
    }

    pub fn set_playback_uri(&self, uri: &str) -> std::result::Result<(), SetPlaybackUriError> {
        // Parse the `uri` to make sure it's valid and ensure it's not a `file://` because that's
        // potentially a security concern.
        // NOTE: removed for debugging
        // match url::Url::parse(uri) {
        //     Ok(url) => {
        //         if url.scheme() == "file" {
        //             error!("Received URI is a `file`");
        //             return Err(SetPlaybackUriError::UnsupportedResourceScheme);
        //         }
        //     }
        //     Err(err) => {
        //         error!("Failed to parse provided URI: {err}");
        //         return Err(SetPlaybackUriError::InvalidUri);
        //     }
        // }

        self.inner
            .set_state(gst::State::Ready)
            .map_err(SetPlaybackUriError::PipelineStateChange)?;
        self.playbin.set_property("uri", uri);

        debug!("Playback URI set to: {uri}");

        Ok(())
    }

    pub fn is_playing(&self) -> Option<bool> {
        let (_, state, _) = self.inner.state(None);
        match state {
            gst::State::Playing => Some(true),
            gst::State::Paused => Some(false),
            _ => None,
        }
    }

    pub fn pause(&self) -> Result<()> {
        self.inner.set_state(gst::State::Paused)?;

        debug!("Playback paused");

        Ok(())
    }

    pub fn play_or_resume(&self) -> Result<()> {
        self.inner.set_state(gst::State::Playing)?;

        debug!("Playback resumed");

        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.inner.set_state(gst::State::Null)?;
        self.playbin.set_property("uri", "");

        debug!("Playback stopped");

        Ok(())
    }

    pub fn set_volume(&self, new_volume: f64) {
        self.playbin
            .set_property("volume", new_volume.clamp(0.0, 1.0));

        debug!("Volume set to {}", new_volume.clamp(0.0, 1.0));
    }

    pub fn seek(&self, seek_to: f64) -> Result<()> {
        self.inner.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::from_seconds_f64(seek_to),
        )?;

        // TODO: should use gst::event::Seek

        debug!("Seeked to: {seek_to}");

        Ok(())
    }

    pub fn set_speed(&self, new_speed: f64) -> Result<()> {
        let Some(position) = self.inner.query_position::<gst::ClockTime>() else {
            bail!("Failed to query playback position");
        };

        if new_speed > 0.0 {
            self.inner.seek(
                new_speed,
                gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH,
                gst::SeekType::Set,
                position,
                gst::SeekType::End,
                gst::ClockTime::ZERO,
            )?;
        } else {
            self.inner.seek(
                new_speed,
                gst::SeekFlags::ACCURATE | gst::SeekFlags::FLUSH,
                gst::SeekType::Set,
                gst::ClockTime::ZERO,
                gst::SeekType::End,
                position,
            )?;
        }

        debug!("Playback speed set to: {new_speed}");

        Ok(())
    }

    pub fn get_connection_speed(&self) -> u64 {
        self.playbin.property("connection-speed")
    }
}
