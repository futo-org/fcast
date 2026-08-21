//! Plain mirrors of the types the `.slint` compiler generates.
//!
//! The UI layer (`receiver-ui`) owns the generated types; this crate must not,
//! or every test in it would pay for compiling slint. Commands crossing the
//! `UpdateGuiCommand` channel therefore carry these instead, and the UI maps
//! them one-for-one when it applies a command.

/// Mirrors the generated `AppState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppState {
    #[default]
    Idle,
    LoadingMedia,
    Playing,
}

/// Mirrors the generated `GuiPlaybackState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GuiPlaybackState {
    #[default]
    Idle,
    Playing,
    Paused,
    Loading,
}

/// Mirrors the generated `UiPlayerVariant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiPlayerVariant {
    #[default]
    Unknown,
    Image,
    Audio,
    Video,
    Raop,
}

/// Mirrors the generated `UiUpdaterState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiUpdaterState {
    #[default]
    None,
    ShowingDialog,
    Downloading,
    DownloadFailed,
    InstallFailed,
    InstallSuccessful,
}

/// Mirrors the generated `UiMediaTrack` (whose `name` is a `SharedString`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UiMediaTrack {
    pub id: i32,
    pub name: String,
}

/// Mirrors the generated `UiToastKind`. The wording lives in slint (`@tr`,
/// so it localizes), severity (error vs warning styling) derives from the
/// kind there too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiToastKind {
    MediaNotFound,
    AccessDenied,
    NetworkFailure,
    UnsupportedFormat,
    MissingCodec,
    DecodeFailed,
    DrmProtected,
    OutputFailure,
    ImageDownloadFailed,
    MissingCodecForTrack,
    StuckStream,
    SubtitleFormatUnsupported,
    GenericWarning,
}

/// A rendered QR code as a module grid, `size * size`, row-major; `true` is a
/// dark module. The UI turns this into a pixel buffer. Building one here would
/// need slint's image types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrCode {
    pub size: u32,
    pub dark: Vec<bool>,
}

/// An 8-bit RGBA colour, mirroring `slint::Color`, for the inspector's scene
/// description (see [`crate::inspector_graph`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub alpha: u8,
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Color {
    pub const fn from_rgb_u8(red: u8, green: u8, blue: u8) -> Self {
        Self {
            alpha: 0xff,
            red,
            green,
            blue,
        }
    }

    pub const fn from_argb_u8(alpha: u8, red: u8, green: u8, blue: u8) -> Self {
        Self {
            alpha,
            red,
            green,
            blue,
        }
    }
}
