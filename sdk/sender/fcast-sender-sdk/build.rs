use cfg_aliases::cfg_aliases;

fn main() {
    cfg_aliases! {
        any_protocol: { any(feature = "fcast", feature = "chromecast") },
    }
}
