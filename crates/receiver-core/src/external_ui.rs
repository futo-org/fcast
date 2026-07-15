use std::sync::Mutex;

use tokio::sync::mpsc::UnboundedSender;

use crate::image::{AnimationFrame, DecodedImage};

pub struct ImageAnimationFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub delay_ms: i64,
}

pub enum ImageCommand {
    Set {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    },
    SetAnimation {
        frames: Vec<ImageAnimationFrame>,
    },
    AudioPlaceholder,
    Clear,
}

pub enum TitleCommand {
    Show {
        title: String,
        artist: String,
        album: String,
        persistent: bool,
    },
    Clear,
}

pub enum PlaybackCommand {
    Progress { elapsed_s: f64, duration_s: f64 },
    Paused(bool),
}

#[derive(Default)]
struct State {
    playing: bool,
    audio_variant: bool,
    has_cover: bool,
    title: String,
    artist: String,
    album: String,
}

pub struct ExternalUi {
    image_tx: UnboundedSender<ImageCommand>,
    title_tx: UnboundedSender<TitleCommand>,
    playback_tx: UnboundedSender<PlaybackCommand>,
    state: Mutex<State>,
}

impl ExternalUi {
    pub fn new(
        image_tx: UnboundedSender<ImageCommand>,
        title_tx: UnboundedSender<TitleCommand>,
        playback_tx: UnboundedSender<PlaybackCommand>,
    ) -> Self {
        Self {
            image_tx,
            title_tx,
            playback_tx,
            state: Mutex::new(State::default()),
        }
    }

    pub fn set_progress(&self, elapsed_s: f64, duration_s: f64) {
        let _ = self.playback_tx.send(PlaybackCommand::Progress {
            elapsed_s,
            duration_s,
        });
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.playback_tx.send(PlaybackCommand::Paused(paused));
    }

    pub fn set_playing(&self, playing: bool) {
        self.state.lock().unwrap().playing = playing;
        self.maybe_show_placeholder();
        self.maybe_show_title();
    }

    pub fn set_audio_variant(&self, is_audio: bool) {
        self.state.lock().unwrap().audio_variant = is_audio;
        if is_audio {
            self.maybe_show_placeholder();
        } else {
            let _ = self.image_tx.send(ImageCommand::Clear);
        }
        self.maybe_show_title();
    }

    pub fn set_title(&self, title: &str) {
        self.state.lock().unwrap().title = title.to_owned();
        self.maybe_show_placeholder();
        self.maybe_show_title();
    }

    pub fn set_artist(&self, artist: &str) {
        self.state.lock().unwrap().artist = artist.to_owned();
        self.maybe_show_title();
    }

    pub fn set_album(&self, album: &str) {
        self.state.lock().unwrap().album = album.to_owned();
        self.maybe_show_title();
    }

    pub fn set_preview(&self, img: DecodedImage) {
        self.send_image(img);
    }

    pub fn set_cover(&self, img: DecodedImage) {
        self.state.lock().unwrap().has_cover = true;
        self.send_image(img);
    }

    fn send_image(&self, img: DecodedImage) {
        let (rgba, width, height) = img.into_upright_rgba();
        let _ = self.image_tx.send(ImageCommand::Set {
            rgba,
            width,
            height,
        });
    }

    pub fn set_animation(&self, frames: Vec<AnimationFrame>) {
        let frames = frames
            .into_iter()
            .map(|f| ImageAnimationFrame {
                width: f.image.width(),
                height: f.image.height(),
                rgba: f.image.as_bytes().to_vec(),
                delay_ms: f.delay_ms,
            })
            .collect();
        let _ = self.image_tx.send(ImageCommand::SetAnimation { frames });
    }

    pub fn clear_audio_covers(&self) {
        self.state.lock().unwrap().has_cover = false;
        self.maybe_show_placeholder();
    }

    pub fn clear(&self) {
        self.state.lock().unwrap().has_cover = false;
        let _ = self.image_tx.send(ImageCommand::Clear);
        let _ = self.title_tx.send(TitleCommand::Clear);
    }

    fn maybe_show_placeholder(&self) {
        let state = self.state.lock().unwrap();
        if state.playing && state.audio_variant && !state.has_cover {
            let _ = self.image_tx.send(ImageCommand::AudioPlaceholder);
        }
    }

    fn maybe_show_title(&self) {
        let state = self.state.lock().unwrap();
        if !state.playing {
            return;
        }
        let _ = self.title_tx.send(TitleCommand::Show {
            title: state.title.clone(),
            artist: state.artist.clone(),
            album: state.album.clone(),
            persistent: state.audio_variant,
        });
    }
}
