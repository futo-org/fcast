use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, TableLike};
use tracing::{debug, error, info, warn};

/// Root of the on-disk receiver configuration, mirrored 1:1 to `config.toml`.
///
/// Every field maps to a TOML table. Sections are their own structs so the file
/// stays organised as it grows. Deserialization is lenient: missing
/// sections/keys fall back to their defaults and unknown keys are ignored, so
/// older and newer config files both load without error.
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
    /// A regex for excluding network interface names from being broadcast to.
    ///
    /// `None` (the key being absent) means no interfaces are excluded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_interfaces: Option<String>,
}

/// `[fcast]` the FCast protocol receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FcastConfig {
    /// Whether to advertise and serve FCast. When false the receiver does not
    /// bind or announce the `_fcast._tcp` service.
    pub enabled: bool,
    /// Broadcast name shown to senders. The `{hostname}` variable is replaced
    /// with the local hostname. Defaults to `FCast-{hostname}`.
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
    /// Broadcast name. The `{hostname}` variable is replaced with the local
    /// hostname. Defaults to `FCast-{hostname}`.
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
    /// Broadcast name. The `{hostname}` variable is replaced with the local
    /// hostname. Defaults to `Chromecast-{hostname}`.
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
    /// Whether to advertise and serve AirPlay mirroring. Only has an effect on
    /// builds compiled with the `airplay` feature.
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
    /// Frame render profile: `fast`, `balanced` or `high-quality`. Absent uses
    /// the built-in default. Stored as a string so an unrecognised value warns
    /// and falls back rather than discarding the rest of the file.
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
    /// Absent uses the built-in default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

impl Config {
    /// Apply a boolean setting identified by a dotted `section.key`, as sent from
    /// the settings UI. Returns false for an unknown key so the caller can log it
    /// instead of silently doing nothing.
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

    /// Apply a string setting identified by a dotted `section.key`, as sent from
    /// the settings UI. An empty value clears the setting back to its default.
    /// Dropdown settings additionally treat the `"Default"` sentinel as unset,
    /// while free-text names keep it as a literal value. Returns false for an
    /// unknown key.
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

/// Owns the receiver's persisted [`Config`]: loads it, hands out a typed view,
/// applies programmatic edits, and writes them back while preserving the
/// existing file's comments and layout.
///
/// The in-memory [`Config`] is the source of truth for reads. On save it is
/// merged into the parsed on-disk document ([`apply`]) so untouched keys,
/// comments and formatting survive round-trips.
#[derive(Debug)]
pub struct ConfigStore {
    /// Where edits are persisted. `None` when no writable location could be
    /// resolved (e.g. no home directory). The store then behaves read-only and
    /// keeps edits in memory.
    path: Option<PathBuf>,
    /// Typed, in-memory view, the source of truth for reads.
    config: Config,
    /// The parsed on-disk document for the write target, kept so saves preserve
    /// its comments and formatting. Empty when the target file does not exist.
    doc: DocumentMut,
}

impl ConfigStore {
    /// Load the receiver config.
    ///
    /// `explicit_path` is the optional `--settings-file-path` override. The read
    /// precedence (first existing, parseable file wins) is:
    ///
    /// 1. `explicit_path`, if given.
    /// 2. `$XDG_CONFIG_HOME/fcast-receiver.toml`
    /// 3. `$XDG_CONFIG_HOME/fcast-receiver/config.toml`
    /// 4. (Linux) `/etc/fcast-receiver.toml`
    /// 5. (Linux) `/etc/fcast-receiver/config.toml`
    ///
    /// This never fails: any read/parse error is logged and the next candidate
    /// (or, ultimately, [`Config::default`]) is used.
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
                        // Logging may not be initialised yet at startup, so also
                        // surface a broken config on stderr rather than silently
                        // falling back to defaults.
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

        // Merge edits into the write target's current content so its comments
        // are preserved. When writing to a fresh location (no file yet, or
        // migrating away from a read-only system path) we start from an empty
        // document.
        let doc = match &path {
            Some(path) => load_document(path).unwrap_or_default(),
            None => {
                warn!("No writable config location, edits will not be persisted");
                DocumentMut::new()
            }
        };

        Self { path, config, doc }
    }

    /// Build an in-memory store around an explicit file path. Intended for tests
    /// and callers that manage their own path resolution.
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

    /// The current, typed configuration. This is the read path for all settings.
    pub fn get(&self) -> &Config {
        &self.config
    }

    /// The file edits are persisted to, if a writable location was resolved.
    #[allow(dead_code)]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Apply an in-memory edit and persist it, preserving the file's existing
    /// comments and formatting.
    ///
    /// The edit is applied to the in-memory config unconditionally. The returned
    /// `Result` only reflects whether persisting to disk succeeded. If no
    /// writable location exists the edit stays in memory and `Ok(())` is
    /// returned.
    #[allow(dead_code)]
    pub fn update<F>(&mut self, edit: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut Config),
    {
        edit(&mut self.config);
        self.persist()
    }

    /// Write the current config to disk. Rarely needed directly. Prefer
    /// [`ConfigStore::update`], which edits and persists together.
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
        // Keep the cached document in sync with what is now on disk so the next
        // save merges into the latest content.
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

/// Where edits should be written back to.
///
/// An explicit `--settings-file-path` always wins. Otherwise, if the config was
/// loaded from a user-writable file we keep writing there. A config loaded from
/// a read-only system path (e.g. `/etc`) is migrated to the per-user config dir
/// so user edits don't need root and take precedence on the next load.
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

/// Produce the document to write: the serialized `config` merged into `doc`,
/// preserving `doc`'s comments and formatting.
///
/// This is the piece that makes adding a setting a one-field change: the config
/// is serialized generically and merged key-by-key, so there is no hand-written
/// per-key write logic to maintain.
fn apply(doc: &DocumentMut, config: &Config) -> Result<DocumentMut, toml_edit::ser::Error> {
    let serialized = toml_edit::ser::to_string(config)?;
    // A freshly serialized `Config` is always valid TOML.
    let mut src: DocumentMut = serialized
        .parse()
        .expect("serialized config is always valid TOML");
    // `toml_edit` serializes nested structs as inline tables (`x = { .. }`).
    // Expand them to standard `[section]` tables so freshly written files read
    // naturally. Existing files keep whatever style they already use (the merge
    // below never rewrites a table's container, only its leaves).
    for (_, item) in src.as_table_mut().iter_mut() {
        expand_inline_tables(item);
    }
    let mut out = doc.clone();
    merge_table_like(out.as_table_mut(), src.as_table());
    Ok(out)
}

/// Recursively copy every key from `src` into `dst`. Table-like values (standard
/// or inline tables) are merged in place. Leaf values are replaced while keeping
/// `dst`'s surrounding decor (comments/whitespace). Keys present only in `dst`
/// (foreign keys, or fields serialized as absent) are left untouched.
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

/// Convert inline-table values (`x = { .. }`) into standard `[table]` items,
/// recursively. Used to normalize freshly serialized config before merging.
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

/// Overwrite `dst` with `src`'s value, keeping `dst`'s existing decor (the
/// surrounding whitespace and any inline comment) when both are scalar values.
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

/// Write `bytes` to `path` atomically: create parent dirs, write a sibling temp
/// file, then rename it over the target so readers never observe a partial file.
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

    /// The shipped, fully-commented example. Kept next to this crate so packagers
    /// can install it, and embedded here only so the test below can guard it.
    const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");

    fn parse_config(text: &str) -> Config {
        toml_edit::de::from_str(text).expect("valid config")
    }

    #[test]
    fn example_config_is_valid_and_all_defaults() {
        // Guards two properties of the shipped example:
        //   1. It is valid TOML that deserializes into `Config`.
        //   2. A copied-but-unedited copy behaves exactly like having no config,
        //      i.e. every setting is commented out or left at its default.
        // Comparing serialized forms canonicalises away comments and layout.
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

        // A free-text name of "Default" stays literal (only dropdowns treat it
        // as unset).
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
