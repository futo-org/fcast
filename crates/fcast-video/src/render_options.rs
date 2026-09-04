//! Renderer settings. Plain data, no GPU. The config surface carries the
//! profile, the video lane turns it into a quality tier.

#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum RenderProfile {
    Fast,
    Balanced,
    HighQuality,
}
