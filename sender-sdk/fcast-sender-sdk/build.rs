use cfg_aliases::cfg_aliases;

fn main() {
    cfg_aliases! {
        any_protocol: { any(feature = "airplay1", feature = "airplay2", feature = "fcast", feature = "chromecast") },
    }
}
