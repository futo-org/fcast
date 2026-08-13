//! Renderer settings. Plain data, no GPU.
//!
//! Kept outside `placebo.rs` because the config surface that carries them is
//! compiled without libplacebo, so these types must build with the `render`
//! feature off. `placebo` re-exports them.

#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum RenderProfile {
    Fast,
    Balanced,
    HighQuality,
}

#[derive(Clone, Copy)]
pub struct RenderingOptions {
    pub profile: RenderProfile,
    pub visualize_lut: bool,
    pub show_clipping: bool,
}
