use cfg_aliases::cfg_aliases;
use std::io::Result;

fn main() -> Result<()> {
    #[cfg(feature = "chromecast")]
    prost_build::compile_protos(&["src/googlecast.proto"], &["src"])?;

    cfg_aliases! {
        any_protocol: { any(feature = "fcast", feature = "chromecast") },
    }

    Ok(())
}
