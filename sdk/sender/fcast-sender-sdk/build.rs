use std::io::Result;

use cfg_aliases::cfg_aliases;

fn main() -> Result<()> {
    #[cfg(feature = "chromecast")]
    prost_build::compile_protos(&["src/googlecast.proto"], &["src"])?;

    cfg_aliases! {
        any_protocol: { any(feature = "fcast", feature = "chromecast") },
    }

    Ok(())
}
