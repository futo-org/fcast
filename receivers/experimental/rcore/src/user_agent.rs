use std::{collections::HashMap, sync::LazyLock};

use parking_lot::Mutex;
use rand::{SeedableRng, rngs::SmallRng, seq::IndexedRandom};

const SYSTEMS: &[&str] = &[
    "Macintosh; Intel Mac OS X 10_15_17",
    "X11; Linux x86_64",
    "Linux; Android 10",
    "Linux; Android 11",
    "Linux; Android 12",
];

const PLATFORMS: &[&str] = &[
    "Gecko/20100101",
    "AppleWebKit/537.36 (KHTML, like Gecho)",
    "AppleWebKit/605.1.15 (KHTML, like Gecho)",
];

const BROWSERS: &[&str] = &[
    "Firefox/147.0",
    "Firefox/146.0",
    "Firefox/145.0",
    "Firefox/144.0",
    "Chrome/144.0.0.0 Safari/537.36",
    "Version/26.2 Safari/605.1.15",
    "Chrome/138.0.0.0 Mobile Safari/537.36",
];

// Use one UA string for each domain, helps against bot detection for certain websites
static CACHE: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn random_browser_user_agent(domain: Option<&str>) -> String {
    if let Some(domain) = domain {
        let cache = CACHE.lock();
        if let Some(ua) = cache.get(domain) {
            return ua.clone();
        }
    }

    let mut rng = SmallRng::from_rng(&mut rand::rng());

    // NOTE: safe unwraps since the slices are never empty
    let system = SYSTEMS.choose(&mut rng).unwrap();
    let platform = PLATFORMS.choose(&mut rng).unwrap();
    let browser = BROWSERS.choose(&mut rng).unwrap();

    let ua = format!("Mozilla/5.0 {system} {platform} {browser}");

    if let Some(domain) = domain {
        CACHE.lock().insert(domain.to_owned(), ua.clone());
    }

    ua
}
