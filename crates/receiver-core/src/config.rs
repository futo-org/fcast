use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, TableLike};
use tracing::{debug, error, info, warn};

/// Root of the on-disk receiver configuration, mirrored 1:1 to `config.toml`.
///
/// Deserialization is lenient: missing sections/keys fall back to their
/// defaults and unknown keys are ignored, so older and newer config files both
/// load.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// `[discovery]` mDNS / network discovery settings.
    pub discovery: DiscoveryConfig,
    /// `[fcast]` the FCast protocol receiver.
    pub fcast: FcastConfig,
    /// `[raop]` the AirPlay audio (RAOP) receiver.
    pub raop: RaopConfig,
    /// `[chromecast]` the Google Cast receiver.
    pub chromecast: ChromecastConfig,
    /// `[airplay]` the AirPlay screen-mirroring receiver.
    pub airplay: AirplayConfig,
    /// `[interface]` window, tray and player presentation.
    pub interface: InterfaceConfig,
    /// `[video]` video output settings.
    pub video: VideoConfig,
    /// `[log]` logging settings.
    pub log: LogConfig,
}

/// `[discovery]` how the receiver advertises itself on the network.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Regex of network interface names to exclude; absent excludes none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_interfaces: Option<String>,
}

/// `[fcast]` the FCast protocol receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FcastConfig {
    /// Whether to bind and announce the `_fcast._tcp` service.
    pub enabled: bool,
    /// Broadcast name; `{hostname}` expands. Defaults to `FCast-{hostname}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Default for FcastConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            name: None,
        }
    }
}

/// `[raop]` the AirPlay audio (RAOP) receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RaopConfig {
    /// Whether to advertise and serve RAOP.
    pub enabled: bool,
    /// Broadcast name; `{hostname}` expands. Defaults to `FCast-{hostname}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Default for RaopConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            name: None,
        }
    }
}

/// `[chromecast]` the Google Cast receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChromecastConfig {
    /// Whether to advertise and serve Google Cast.
    pub enabled: bool,
    /// Broadcast name; `{hostname}` expands. Defaults to
    /// `Chromecast-{hostname}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Default for ChromecastConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            name: None,
        }
    }
}

/// `[airplay]` the AirPlay screen-mirroring receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AirplayConfig {
    /// Whether to advertise and serve AirPlay mirroring (needs the `airplay`
    /// feature).
    pub enabled: bool,
}

impl Default for AirplayConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// `[interface]` window, tray and player presentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InterfaceConfig {
    /// Show the main window on start. False starts hidden to the tray.
    pub show_window: bool,
    /// Show a system tray icon. False never opens a tray.
    pub tray: bool,
    /// Start the main window fullscreen.
    pub start_fullscreen: bool,
    /// Run the media player fullscreen. False uses a windowed player.
    pub fullscreen_player: bool,
    /// Run without a GUI at all.
    pub headless: bool,
}

impl Default for InterfaceConfig {
    fn default() -> Self {
        Self {
            show_window: true,
            tray: true,
            start_fullscreen: false,
            fullscreen_player: true,
            headless: false,
        }
    }
}

/// `[video]` video output settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    /// Allow HDR passthrough. False tone-maps HDR content to SDR.
    pub hdr_output: bool,
    /// Frame render profile: `fast`, `balanced` or `high-quality`. Stored as a
    /// string so an unrecognised value warns instead of discarding the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_profile: Option<String>,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            hdr_output: true,
            render_profile: None,
        }
    }
}

/// `[log]` logging settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Log verbosity: `off`, `error`, `warn`, `info`, `debug` or `trace`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

impl Config {
    /// Apply a boolean setting by dotted `section.key`. False for an unknown
    /// key.
    pub fn set_bool(&mut self, key: &str, value: bool) -> bool {
        match key {
            "fcast.enabled" => self.fcast.enabled = value,
            "raop.enabled" => self.raop.enabled = value,
            "chromecast.enabled" => self.chromecast.enabled = value,
            "airplay.enabled" => self.airplay.enabled = value,
            "interface.show_window" => self.interface.show_window = value,
            "interface.tray" => self.interface.tray = value,
            "interface.start_fullscreen" => self.interface.start_fullscreen = value,
            "interface.fullscreen_player" => self.interface.fullscreen_player = value,
            "interface.headless" => self.interface.headless = value,
            "video.hdr_output" => self.video.hdr_output = value,
            _ => return false,
        }
        true
    }

    /// Apply a string setting by dotted `section.key`. An empty value clears
    /// it; dropdowns also treat `"Default"` as unset while free-text names
    /// keep it literal. False for an unknown key.
    pub fn set_string(&mut self, key: &str, value: &str) -> bool {
        let trimmed = value.trim();
        let text = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        let choice = (!trimmed.is_empty() && trimmed != "Default").then(|| trimmed.to_owned());
        match key {
            "discovery.exclude_interfaces" => self.discovery.exclude_interfaces = text,
            "fcast.name" => self.fcast.name = text,
            "raop.name" => self.raop.name = text,
            "chromecast.name" => self.chromecast.name = text,
            "video.render_profile" => self.video.render_profile = choice,
            "log.level" => self.log.level = choice,
            _ => return false,
        }
        true
    }
}

/// Owns the receiver's persisted [`Config`]. The in-memory copy is the source
/// of truth for reads; on save it is merged into the parsed on-disk document so
/// untouched keys, comments and formatting survive round-trips.
#[derive(Debug)]
pub struct ConfigStore {
    /// `None` when no writable location resolved; the store then keeps edits in
    /// memory.
    path: Option<PathBuf>,
    /// Typed, in-memory view, the source of truth for reads.
    config: Config,
    /// Parsed write target, kept so saves preserve its comments and formatting.
    doc: DocumentMut,
}

impl ConfigStore {
    /// Load the receiver config from the first existing, parseable candidate
    /// (see [`read_candidate_paths`]). Never fails: errors log and fall
    /// through, down to [`Config::default`].
    pub fn load(explicit_path: Option<&str>) -> Self {
        let mut loaded: Option<(PathBuf, Config)> = None;
        for path in read_candidate_paths(explicit_path) {
            if !path.exists() {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => match toml_edit::de::from_str::<Config>(&text) {
                    Ok(config) => {
                        info!(?path, ?config, "Loaded receiver config");
                        loaded = Some((path, config));
                        break;
                    }
                    Err(err) => {
                        // Logging may not be initialised yet, so also use stderr.
                        eprintln!("fcast-receiver: ignoring invalid config at {path:?}: {err}");
                        error!(?err, ?path, "Failed to parse config, trying next candidate");
                    }
                },
                Err(err) => error!(?err, ?path, "Failed to read config file"),
            }
        }

        let (loaded_from, config) = match loaded {
            Some((path, config)) => (Some(path), config),
            None => {
                debug!("No receiver config file found, using defaults");
                (None, Config::default())
            }
        };

        let path = writable_path(explicit_path, loaded_from.as_deref());

        // Start from the write target's current content so its comments survive.
        let doc = match &path {
            Some(path) => load_document(path).unwrap_or_default(),
            None => {
                warn!("No writable config location, edits will not be persisted");
                DocumentMut::new()
            }
        };

        Self { path, config, doc }
    }

    /// Build a store around an explicit file path, bypassing path resolution.
    #[allow(dead_code)]
    pub fn open(path: PathBuf) -> Self {
        let doc = load_document(&path).unwrap_or_default();
        let config = toml_edit::de::from_str::<Config>(&doc.to_string()).unwrap_or_default();
        Self {
            path: Some(path),
            config,
            doc,
        }
    }

    /// The current, typed configuration.
    pub fn get(&self) -> &Config {
        &self.config
    }

    /// The file edits are persisted to, if a writable location was resolved.
    #[allow(dead_code)]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Apply an in-memory edit and persist it. The edit always applies; the
    /// `Result` only reflects the disk write, and no writable location is `Ok`.
    #[allow(dead_code)]
    pub fn update<F>(&mut self, edit: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut Config),
    {
        edit(&mut self.config);
        self.persist()
    }

    /// Write the current config to disk; prefer [`ConfigStore::update`].
    #[allow(dead_code)]
    pub fn save(&mut self) -> std::io::Result<()> {
        self.persist()
    }

    fn persist(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.clone() else {
            warn!("No writable config path, keeping changes in memory only");
            return Ok(());
        };

        let merged = apply(&self.doc, &self.config)
            .map_err(|err| std::io::Error::other(format!("failed to serialize config: {err}")))?;
        write_atomic(&path, merged.to_string().as_bytes())?;
        // Keep the cached document in sync with disk for the next merge.
        self.doc = merged;
        info!(?path, "Saved receiver config");
        Ok(())
    }
}

/// The ordered list of files to try when loading, highest precedence first.
fn read_candidate_paths(explicit_path: Option<&str>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(explicit) = explicit_path {
        paths.push(PathBuf::from(explicit));
    }
    if let Some(base) = directories::BaseDirs::new() {
        let config_dir = base.config_dir();
        paths.push(config_dir.join("fcast-receiver.toml"));
        paths.push(config_dir.join("fcast-receiver").join("config.toml"));
    }
    #[cfg(target_os = "linux")]
    {
        paths.push(PathBuf::from("/etc").join("fcast-receiver.toml"));
        paths.push(
            PathBuf::from("/etc")
                .join("fcast-receiver")
                .join("config.toml"),
        );
    }
    paths
}

/// Where edits are written back to: the explicit path, else the file it was
/// loaded from. A config loaded from a read-only system path (e.g. `/etc`) is
/// migrated to the per-user dir so edits don't need root.
fn writable_path(explicit_path: Option<&str>, loaded_from: Option<&Path>) -> Option<PathBuf> {
    if let Some(explicit) = explicit_path {
        return Some(PathBuf::from(explicit));
    }
    if let Some(loaded) = loaded_from
        && !is_system_path(loaded)
    {
        return Some(loaded.to_path_buf());
    }
    directories::BaseDirs::new()
        .map(|base| base.config_dir().join("fcast-receiver").join("config.toml"))
}

fn is_system_path(path: &Path) -> bool {
    path.starts_with("/etc")
}

fn load_document(path: &Path) -> Option<DocumentMut> {
    if !path.exists() {
        return None;
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            error!(?err, ?path, "Failed to read config document for editing");
            return None;
        }
    };
    match text.parse::<DocumentMut>() {
        Ok(doc) => Some(doc),
        Err(err) => {
            error!(?err, ?path, "Failed to parse config document for editing");
            None
        }
    }
}

/// The document to write: the serialized `config` merged into `doc`, preserving
/// `doc`'s comments and formatting.
fn apply(doc: &DocumentMut, config: &Config) -> Result<DocumentMut, toml_edit::ser::Error> {
    let serialized = toml_edit::ser::to_string(config)?;
    let mut src: DocumentMut = serialized
        .parse()
        .expect("serialized config is always valid TOML");
    // `toml_edit` serializes nested structs as inline tables; expand them to
    // `[section]` tables so fresh files read naturally. Existing files keep their
    // own style, since the merge below only rewrites leaves.
    for (_, item) in src.as_table_mut().iter_mut() {
        expand_inline_tables(item);
    }
    let mut out = doc.clone();
    merge_table_like(out.as_table_mut(), src.as_table());
    // The merge only writes keys present in `src`, and serde skips `None`, so a
    // cleared optional would otherwise linger on disk.
    prune_cleared_keys(out.as_table_mut(), src.as_table());
    Ok(out)
}

/// The optional settings that can be cleared: exactly the `Option` fields with
/// `skip_serializing_if` handled by [`Config::set_string`]. Keep both in sync.
const CLEARABLE_KEYS: &[&[&str]] = &[
    &["discovery", "exclude_interfaces"],
    &["fcast", "name"],
    &["raop", "name"],
    &["chromecast", "name"],
    &["video", "render_profile"],
    &["log", "level"],
];

/// Remove from `dst` every [`CLEARABLE_KEYS`] path absent from `src`; foreign
/// keys and still-set values are untouched.
fn prune_cleared_keys(dst: &mut dyn TableLike, src: &dyn TableLike) {
    for path in CLEARABLE_KEYS {
        if !path_present(src, path) {
            remove_path(dst, path);
        }
    }
}

/// Whether the nested `path` resolves to a present key in `table`.
fn path_present(table: &dyn TableLike, path: &[&str]) -> bool {
    match path {
        [] => true,
        [key] => table.contains_key(key),
        [key, rest @ ..] => table
            .get(key)
            .and_then(Item::as_table_like)
            .is_some_and(|child| path_present(child, rest)),
    }
}

/// Remove the leaf named by `path`, if present. Empty parent sections stay, so
/// their decor is preserved.
fn remove_path(table: &mut dyn TableLike, path: &[&str]) {
    match path {
        [] => {}
        [key] => {
            table.remove(key);
        }
        [key, rest @ ..] => {
            if let Some(child) = table.get_mut(key).and_then(Item::as_table_like_mut) {
                remove_path(child, rest);
            }
        }
    }
}

/// Recursively copy every key from `src` into `dst`, keeping `dst`'s decor.
/// Keys present only in `dst` are left untouched.
fn merge_table_like(dst: &mut dyn TableLike, src: &dyn TableLike) {
    for (key, src_item) in src.iter() {
        if let Some(dst_item) = dst.get_mut(key) {
            merge_item(dst_item, src_item);
        } else {
            dst.insert(key, src_item.clone());
        }
    }
}

fn merge_item(dst: &mut Item, src: &Item) {
    if dst.is_table_like() && src.is_table_like() {
        let dst_table = dst.as_table_like_mut().expect("dst is table-like");
        let src_table = src.as_table_like().expect("src is table-like");
        merge_table_like(dst_table, src_table);
    } else {
        replace_preserving_decor(dst, src);
    }
}

/// Convert inline-table values (`x = { .. }`) into standard `[table]` items.
fn expand_inline_tables(item: &mut Item) {
    if let Some(inline) = item.as_inline_table().cloned() {
        let mut table = inline.into_table();
        for (_, child) in table.iter_mut() {
            expand_inline_tables(child);
        }
        *item = Item::Table(table);
    } else if let Some(table) = item.as_table_mut() {
        for (_, child) in table.iter_mut() {
            expand_inline_tables(child);
        }
    }
}

/// Overwrite `dst` with `src`'s value, keeping `dst`'s decor (surrounding
/// whitespace and any inline comment) when both are scalar values.
fn replace_preserving_decor(dst: &mut Item, src: &Item) {
    let Some(src_value) = src.as_value() else {
        *dst = src.clone();
        return;
    };
    let existing_decor = dst.as_value().map(|value| value.decor().clone());
    let mut new_value = src_value.clone();
    if let Some(decor) = existing_decor {
        *new_value.decor_mut() = decor;
    }
    *dst = Item::Value(new_value);
}

/// Write `bytes` to `path` atomically (temp file plus rename), so readers never
/// observe a partial file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped example, embedded only so the test below can guard it.
    const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");

    fn parse_config(text: &str) -> Config {
        toml_edit::de::from_str(text).expect("valid config")
    }

    #[test]
    fn example_config_is_valid_and_all_defaults() {
        // The example must be valid TOML and, unedited, behave exactly like having
        // no config at all. Serializing canonicalises away comments and layout.
        let parsed: Config =
            toml_edit::de::from_str(EXAMPLE_CONFIG).expect("example is valid TOML");
        assert_eq!(
            toml_edit::ser::to_string(&parsed).expect("serialize parsed example"),
            toml_edit::ser::to_string(&Config::default()).expect("serialize default"),
            "config.example.toml drifted: keep settings commented out or at their default value",
        );
    }

    fn render(existing: &str, config: &Config) -> String {
        let doc: DocumentMut = existing.parse().expect("valid toml");
        apply(&doc, config).expect("apply").to_string()
    }

    #[test]
    fn missing_sections_and_keys_default() {
        let config: Config = parse_config("");
        assert!(config.discovery.exclude_interfaces.is_none());
        assert!(config.fcast.enabled);
        assert!(config.raop.enabled);
        assert!(config.chromecast.enabled);
        assert!(config.airplay.enabled);
        assert!(config.interface.show_window);
        assert!(config.interface.tray);
        assert!(!config.interface.start_fullscreen);
        assert!(config.interface.fullscreen_player);
        assert!(!config.interface.headless);
        assert!(config.video.hdr_output);
    }

    #[test]
    fn partial_section_keeps_other_defaults() {
        // Only one key set in a section: the rest come from the section default.
        let config = parse_config("[fcast]\nname = \"Living Room\"\n");
        assert!(config.fcast.enabled);
        assert_eq!(config.fcast.name.as_deref(), Some("Living Room"));
    }

    #[test]
    fn set_bool_dispatch() {
        let mut config = Config::default();
        assert!(config.set_bool("raop.enabled", false));
        assert!(!config.raop.enabled);
        assert!(config.set_bool("interface.tray", false));
        assert!(!config.interface.tray);
        assert!(config.set_bool("video.hdr_output", false));
        assert!(!config.video.hdr_output);
        assert!(
            !config.set_bool("bogus.key", true),
            "unknown key returns false"
        );
    }

    #[test]
    fn set_string_dispatch_and_clear() {
        let mut config = Config::default();

        assert!(config.set_string("fcast.name", "Living Room"));
        assert_eq!(config.fcast.name.as_deref(), Some("Living Room"));
        // Whitespace-only clears back to the default.
        assert!(config.set_string("fcast.name", "   "));
        assert!(config.fcast.name.is_none());

        // Dropdowns clear via the "Default" sentinel.
        assert!(config.set_string("video.render_profile", "balanced"));
        assert_eq!(config.video.render_profile.as_deref(), Some("balanced"));
        assert!(config.set_string("video.render_profile", "Default"));
        assert!(config.video.render_profile.is_none());

        // A free-text name of "Default" stays literal.
        assert!(config.set_string("chromecast.name", "Default"));
        assert_eq!(config.chromecast.name.as_deref(), Some("Default"));

        assert!(
            !config.set_string("bogus.key", "x"),
            "unknown key returns false"
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config =
            parse_config("some_future_key = 42\n[discovery]\nexclude_interfaces = \"a\"\n");
        assert_eq!(config.discovery.exclude_interfaces.as_deref(), Some("a"));
    }

    #[test]
    fn apply_inserts_missing_key() {
        let mut config = Config::default();
        config.discovery.exclude_interfaces = Some("eth.*".to_owned());

        let out = render("", &config);
        let reparsed = parse_config(&out);
        assert_eq!(
            reparsed.discovery.exclude_interfaces.as_deref(),
            Some("eth.*")
        );
    }

    #[test]
    fn fresh_render_uses_section_headers() {
        let out = render("", &Config::default());
        assert!(out.contains("[fcast]"), "expected section headers: {out}");
        assert!(!out.contains("fcast = {"), "should not be inline: {out}");
    }

    #[test]
    fn apply_preserves_comments_and_updates_value() {
        let existing = "\
# Receiver configuration
[discovery]
# exclude loopback and docker interfaces
exclude_interfaces = \"old\" # trailing note
";
        let mut config = Config::default();
        config.discovery.exclude_interfaces = Some("new".to_owned());

        let out = render(existing, &config);

        assert!(
            out.contains("# Receiver configuration"),
            "header comment: {out}"
        );
        assert!(
            out.contains("# exclude loopback and docker interfaces"),
            "section comment: {out}"
        );
        assert!(out.contains("# trailing note"), "inline comment: {out}");
        assert!(out.contains("\"new\""), "updated value: {out}");
        assert!(!out.contains("\"old\""), "old value removed: {out}");
    }

    #[test]
    fn foreign_keys_survive_a_save() {
        let existing = "custom = true\n[discovery]\nexclude_interfaces = \"a\"\n";
        let mut config = parse_config(existing);
        config.discovery.exclude_interfaces = Some("b".to_owned());

        let out = render(existing, &config);
        assert!(out.contains("custom = true"), "foreign key kept: {out}");
        assert!(out.contains("\"b\""), "value updated: {out}");
    }

    #[test]
    fn clearing_a_value_reverts_the_file_to_default() {
        // A cleared name must be removed from disk, not reappear on reload.
        let existing = "[raop]\nname = \"Old Name\"\n";
        let mut config = parse_config(existing);
        assert_eq!(config.raop.name.as_deref(), Some("Old Name"));

        config.raop.name = None;

        let out = render(existing, &config);
        assert!(!out.contains("Old Name"), "cleared value removed: {out}");
        assert!(
            parse_config(&out).raop.name.is_none(),
            "reloads at default: {out}"
        );
    }

    #[test]
    fn clearing_one_key_keeps_siblings_and_foreign_keys() {
        // Pruning a cleared key must not disturb other set values or foreign keys.
        let existing = "custom = true\n[raop]\nname = \"Old\"\n[fcast]\nname = \"Keep\"\n";
        let mut config = parse_config(existing);
        config.raop.name = None;

        let out = render(existing, &config);
        assert!(!out.contains("Old"), "cleared key removed: {out}");
        assert!(out.contains("custom = true"), "foreign key kept: {out}");
        assert_eq!(
            parse_config(&out).fcast.name.as_deref(),
            Some("Keep"),
            "sibling value kept: {out}"
        );
    }

    #[test]
    fn round_trip_through_disk() {
        let path = unique_temp_path();
        let mut store = ConfigStore::open(path.clone());
        store
            .update(|config| {
                config.discovery.exclude_interfaces = Some("veth.*".to_owned());
                config.raop.enabled = false;
                config.chromecast.name = Some("Den".to_owned());
            })
            .expect("persist");

        let reloaded = ConfigStore::open(path.clone());
        assert_eq!(
            reloaded.get().discovery.exclude_interfaces.as_deref(),
            Some("veth.*")
        );
        assert!(!reloaded.get().raop.enabled);
        assert_eq!(reloaded.get().chromecast.name.as_deref(), Some("Den"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_file_falls_back_to_default() {
        let path = unique_temp_path();
        std::fs::write(&path, b"this is = = not valid toml\n").expect("write");

        // Loading a broken document for editing yields defaults, not a panic.
        let store = ConfigStore::open(path.clone());
        assert!(store.get().discovery.exclude_interfaces.is_none());
        assert!(store.get().fcast.enabled);

        let _ = std::fs::remove_file(&path);
    }

    fn unique_temp_path() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "fcast-receiver-config-test-{}-{}.toml",
            std::process::id(),
            n
        ))
    }
}
