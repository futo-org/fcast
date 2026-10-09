//! Self-update for the sideloaded android build. The channel is a
//! `latest.json` next to the apks, ordered by versionCode since that is what
//! the platform orders installs by. One apk per ABI, zstd-compressed for the
//! transfer and unpacked while it downloads. This side checks and downloads,
//! the activity verifies the apk and hands it to PackageInstaller.

use std::{collections::BTreeMap, path::Path, time::Duration};

use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use zstd::stream::raw::{Decoder, Operation};

pub const CHANNEL_URL: &str = "https://dl.fcast.org/receiver/android/";
const MANIFEST: &str = "latest.json";
const MAX_MANIFEST: usize = 64 * 1024;
/// A chunk gap this long fails the download instead of leaving the dialog at
/// a frozen percentage.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Unpacked size cap, the apk is ~75 MB.
const MAX_APK: u64 = 512 * 1024 * 1024;
/// zstd -19 uses an 8 MB window, refusing larger keeps decoder memory low on
/// 1 GB boxes.
const WINDOW_LOG_MAX: u32 = 23;
const DECODE_BUF: usize = 128 * 1024;

/// The channel's manifest, `files` maps an android ABI name to its apk.
#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    version_code: i64,
    files: BTreeMap<String, String>,
}

/// The release resolved for this device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The versionName, for display.
    pub version: String,
    pub version_code: i64,
    /// A plain file name in the channel directory.
    pub file: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("server answered {0}")]
    Status(reqwest::StatusCode),
    #[error("release manifest is too large")]
    ManifestTooLarge,
    #[error("release manifest is malformed: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("release names an invalid file {0:?}")]
    BadFileName(String),
    #[error("no build for this device's ABIs {0:?}")]
    NoBuildForDevice(Vec<String>),
    #[error("writing the download failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the download does not unpack: {0}")]
    Decompress(std::io::Error),
    #[error("the download unpacks past {MAX_APK} bytes")]
    TooLarge,
    #[error("the download ended mid-stream")]
    Incomplete,
    #[error("download stalled")]
    Stalled,
    #[error("download ended at {got} of {expected} bytes")]
    Truncated { got: u64, expected: u64 },
}

/// A plain `*.apk.zst` name, never a path or a URL, so the manifest can only
/// point inside the channel directory.
fn validate_file(f: &str) -> Result<(), Error> {
    let plain = f.len() > ".apk.zst".len()
        && f.ends_with(".apk.zst")
        && !f.starts_with('.')
        && f.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if plain {
        Ok(())
    } else {
        Err(Error::BadFileName(f.to_owned()))
    }
}

impl Manifest {
    /// The first of `abis`, in the device's preference order, the channel
    /// has a build for.
    fn pick(self, abis: &[String]) -> Result<Release, Error> {
        for f in self.files.values() {
            validate_file(f)?;
        }
        let mut files = self.files;
        let file = abis
            .iter()
            .find_map(|abi| files.remove(abi))
            .ok_or_else(|| Error::NoBuildForDevice(abis.to_vec()))?;
        Ok(Release {
            version: self.version,
            version_code: self.version_code,
            file,
        })
    }
}

impl Release {
    pub fn is_newer_than(&self, installed_version_code: i64) -> bool {
        self.version_code > installed_version_code
    }
}

pub async fn fetch_release(
    client: &reqwest::Client,
    channel: &str,
    abis: &[String],
) -> Result<Release, Error> {
    let resp = client
        .get(format!("{channel}{MANIFEST}"))
        .timeout(MANIFEST_TIMEOUT)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::Status(resp.status()));
    }
    let body = crate::utils::read_body_capped(resp, MAX_MANIFEST)
        .await
        .map_err(|err| match err {
            crate::utils::BodyError::Request(err) => Error::Http(err),
            crate::utils::BodyError::TooLarge(_) => Error::ManifestTooLarge,
        })?;
    serde_json::from_slice::<Manifest>(&body)?.pick(abis)
}

/// Streaming zstd decode in bounded steps, so neither a large apk nor a
/// decompression bomb is ever held in memory.
struct Unzstd {
    dec: Decoder<'static>,
    out: Box<[u8]>,
    produced: u64,
    /// The last step filled `out`, the decoder may hold more.
    full: bool,
    /// The last step ended a frame.
    frame_done: bool,
}

impl Unzstd {
    fn new() -> Result<Self, Error> {
        let mut dec = Decoder::new().map_err(Error::Decompress)?;
        dec.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(WINDOW_LOG_MAX))
            .map_err(Error::Decompress)?;
        Ok(Self {
            dec,
            out: vec![0; DECODE_BUF].into_boxed_slice(),
            produced: 0,
            full: false,
            frame_done: false,
        })
    }

    /// Decodes up to one buffer, returns the input consumed and the output.
    fn step(&mut self, input: &[u8]) -> Result<(usize, &[u8]), Error> {
        let st = self
            .dec
            .run_on_buffers(input, &mut self.out)
            .map_err(Error::Decompress)?;
        self.produced += st.bytes_written as u64;
        if self.produced > MAX_APK {
            return Err(Error::TooLarge);
        }
        self.full = st.bytes_written == self.out.len();
        if st.bytes_read > 0 || st.bytes_written > 0 {
            self.frame_done = st.remaining == 0;
        }
        Ok((st.bytes_read, &self.out[..st.bytes_written]))
    }
}

/// Streams the release to `dest`, unpacked. `progress` gets the compressed
/// (received, total), total is 0 when the server sends no length.
pub async fn download(
    client: &reqwest::Client,
    channel: &str,
    release: &Release,
    dest: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<(), Error> {
    validate_file(&release.file)?;
    let mut resp = client
        .get(format!("{channel}{}", release.file))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::Status(resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    // Written beside the target and renamed when whole: the activity clears
    // the target when it is recreated, which mid-download cut the file.
    let part = dest.with_extension("apk.part");
    let res = download_to(&mut resp, &part, total, &mut progress).await;
    match res {
        Ok(()) => Ok(tokio::fs::rename(&part, dest).await?),
        Err(err) => {
            let _ = tokio::fs::remove_file(&part).await;
            Err(err)
        }
    }
}

async fn download_to(
    resp: &mut reqwest::Response,
    path: &Path,
    total: u64,
    progress: &mut impl FnMut(u64, u64),
) -> Result<(), Error> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut unz = Unzstd::new()?;
    let mut got = 0u64;
    while let Some(chunk) = tokio::time::timeout(STALL_TIMEOUT, resp.chunk())
        .await
        .map_err(|_| Error::Stalled)??
    {
        let mut input = &chunk[..];
        while !input.is_empty() || unz.full {
            let (used, out) = unz.step(input)?;
            file.write_all(out).await?;
            input = &input[used..];
            if used == 0 && !unz.full {
                break;
            }
        }
        got += chunk.len() as u64;
        progress(got, total);
    }
    // tokio's File finishes its last write in the background until flushed
    file.flush().await?;
    if total != 0 && got != total {
        return Err(Error::Truncated {
            got,
            expected: total,
        });
    }
    if !unz.frame_done {
        return Err(Error::Incomplete);
    }
    Ok(())
}

/// Integer percent, 0 while the total is unknown.
pub fn percent(got: u64, total: u64) -> i32 {
    if total == 0 {
        0
    } else {
        (got.min(total) * 100 / total) as i32
    }
}

#[cfg(target_os = "android")]
pub use android::*;

#[cfg(target_os = "android")]
mod android {
    use std::path::{Path, PathBuf};

    use jni::objects::{JObjectArray, JString, JValue};
    use tracing::{debug, info, warn};

    use super::{CHANNEL_URL, fetch_release};
    use crate::android_jni::with_core;
    use crate::message::{AppUpdate, MessageSender};

    /// A receiver stays up for weeks, so a check at startup alone, or once
    /// a day, leaves a release unseen for long. The manifest is a few
    /// hundred bytes.
    const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3 * 60 * 60);
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
    /// Debug builds only: the channel to test against, e.g.
    /// `adb shell setprop debug.fcast.update_url http://192.168.1.2:8000/`.
    const DEBUG_CHANNEL_PROP: &std::ffi::CStr = c"debug.fcast.update_url";

    // Mirrors Updater.java's MODE_* values.
    const MODE_RELEASE: i32 = 1;
    const MODE_DEBUG: i32 = 2;

    /// The channel this build updates from with its installed versionCode,
    /// None when it must not self-update.
    fn channel() -> Option<(String, i64)> {
        let (mode, code) = with_core("updaterMode", |env, core| {
            let mode = env.call_static_method(core, "updaterMode", "()I", &[])?.i()?;
            let code = env.call_static_method(core, "installedVersionCode", "()J", &[])?.j()?;
            Ok((mode, code))
        })?;
        match mode {
            MODE_RELEASE => Some((CHANNEL_URL.to_owned(), code)),
            MODE_DEBUG => {
                let url = crate::bug_report::system_property(DEBUG_CHANNEL_PROP);
                (!url.is_empty()).then(|| {
                    let url = if url.ends_with('/') { url } else { url + "/" };
                    (url, code)
                })
            }
            _ => None,
        }
    }

    /// Build.SUPPORTED_ABIS, most preferred first.
    fn supported_abis() -> Vec<String> {
        with_core("SUPPORTED_ABIS", |env, _| {
            let arr: JObjectArray = env
                .get_static_field("android/os/Build", "SUPPORTED_ABIS", "[Ljava/lang/String;")?
                .l()?
                .into();
            let mut abis = Vec::new();
            for i in 0..env.get_array_length(&arr)? {
                let abi = JString::from(env.get_object_array_element(&arr, i)?);
                abis.push(env.get_string(&abi)?.into());
                env.delete_local_ref(abi)?;
            }
            Ok(abis)
        })
        .unwrap_or_default()
    }

    /// Checks at startup and then every three hours, an hour after a failed
    /// check.
    pub async fn run_checker(client: reqwest::Client, msg_tx: MessageSender) {
        let Some((channel, installed)) = channel() else {
            info!("self-update is off for this build");
            return;
        };
        let abis = supported_abis();
        info!(channel, installed, ?abis, "self-update channel");
        loop {
            let wait = match fetch_release(&client, &channel, &abis).await {
                Ok(release) => {
                    debug!(?release, "latest release");
                    if release.is_newer_than(installed) {
                        msg_tx.app_update(AppUpdate::UpdateAvailable { channel: channel.clone(), release });
                    }
                    CHECK_INTERVAL
                }
                Err(err) => {
                    warn!(%err, "update check failed");
                    RETRY_INTERVAL
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Where the apk downloads to, cleared of any earlier attempt.
    pub fn prepare_download() -> Option<PathBuf> {
        with_core("prepareUpdateDownload", |env, core| {
            let path = env
                .call_static_method(core, "prepareUpdateDownload", "()Ljava/lang/String;", &[])?
                .l()?;
            let path: String = env.get_string(&JString::from(path))?.into();
            let path = PathBuf::from(path);
            // a download a killed process left half written
            let _ = std::fs::remove_file(path.with_extension("apk.part"));
            Ok(path)
        })
    }

    /// Hands the downloaded apk to the installer. Asynchronous, a failure
    /// comes back as `AppUpdate::InstallFailed`.
    pub fn install(apk: &Path, version_code: i64) -> bool {
        with_core("installUpdate", |env, core| {
            let path = env.new_string(apk.to_string_lossy())?;
            env.call_static_method(
                core,
                "installUpdate",
                "(Ljava/lang/String;J)V",
                &[JValue::Object(&path), JValue::Long(version_code)],
            )?;
            Ok(())
        })
        .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn release(file: &str) -> Release {
        Release {
            version: "3.1.0".into(),
            version_code: 60,
            file: file.into(),
        }
    }

    fn abis(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn file_names_stay_inside_the_channel() {
        for ok in ["fcast-receiver-android-60-arm64-v8a.apk.zst", "a.apk.zst", "x_1.2.3.apk.zst"] {
            assert!(validate_file(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".apk.zst",
            "a.apk",
            "../x.apk.zst",
            "a/b.apk.zst",
            "https://evil.example/x.apk.zst",
            "x.apk.zst?y",
            ".hidden.apk.zst",
            "x.exe",
            "x%2f.apk.zst",
        ] {
            assert!(matches!(validate_file(bad), Err(Error::BadFileName(_))), "{bad}");
        }
    }

    fn manifest() -> Manifest {
        serde_json::from_str(
            r#"{"version":"3.1.0","version_code":60,"files":{
                "arm64-v8a":"r-60-arm64-v8a.apk.zst","armeabi-v7a":"r-60-armeabi-v7a.apk.zst"}}"#,
        )
        .unwrap()
    }

    #[test]
    fn the_device_gets_its_most_preferred_build() {
        let r = manifest().pick(&abis(&["arm64-v8a", "armeabi-v7a", "armeabi"])).unwrap();
        assert_eq!(r, release("r-60-arm64-v8a.apk.zst"));
        // an arm64 box with a 32-bit userland lists only the 32-bit ABIs
        let r = manifest().pick(&abis(&["armeabi-v7a", "armeabi"])).unwrap();
        assert_eq!(r.file, "r-60-armeabi-v7a.apk.zst");
        let r = manifest().pick(&abis(&["x86_64", "armeabi-v7a"])).unwrap();
        assert_eq!(r.file, "r-60-armeabi-v7a.apk.zst");
        assert!(matches!(
            manifest().pick(&abis(&["x86_64"])),
            Err(Error::NoBuildForDevice(_))
        ));
        assert!(matches!(manifest().pick(&[]), Err(Error::NoBuildForDevice(_))));
    }

    #[test]
    fn a_bad_name_for_another_abi_still_rejects_the_manifest() {
        let m: Manifest = serde_json::from_str(
            r#"{"version":"1","version_code":2,"files":{"arm64-v8a":"a.apk.zst","x86":"../x"}}"#,
        )
        .unwrap();
        assert!(matches!(m.pick(&abis(&["arm64-v8a"])), Err(Error::BadFileName(_))));
    }

    #[test]
    fn newer_is_strictly_greater() {
        let r = release("a.apk.zst");
        assert!(r.is_newer_than(59));
        assert!(!r.is_newer_than(60));
        assert!(!r.is_newer_than(61));
    }

    #[test]
    fn percent_is_clamped_and_zero_without_total() {
        assert_eq!(percent(50, 0), 0);
        assert_eq!(percent(0, 200), 0);
        assert_eq!(percent(100, 200), 50);
        assert_eq!(percent(200, 200), 100);
        assert_eq!(percent(300, 200), 100);
    }

    /// Serves `respond(path)` for every request until the test ends. The
    /// response is raw bytes, so a test can lie about Content-Length.
    async fn serve(respond: fn(&str) -> Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head);
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let _ = sock.write_all(&respond(&path)).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}/")
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    fn client() -> reqwest::Client {
        // the rustls-no-provider build panics without a process provider
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        reqwest::Client::new()
    }

    /// Several decode buffers' worth, varied so it does not shrink to nothing.
    fn apk() -> Vec<u8> {
        (0..1_000_000u32).map(|i| (i * 7 + i / 1000) as u8).collect()
    }

    fn packed() -> Vec<u8> {
        zstd::encode_all(&apk()[..], 19).unwrap()
    }

    fn channel_server(path: &str) -> Vec<u8> {
        match path {
            "/latest.json" => ok(br#"{"version":"3.1.0","version_code":60,"files":{"arm64-v8a":"r-60.apk.zst"}}"#),
            "/r-60.apk.zst" => ok(&packed()),
            "/half.apk.zst" => {
                let p = packed();
                ok(&p[..p.len() / 2])
            }
            "/plain.apk.zst" => ok(&apk()),
            "/short.apk.zst" => {
                let p = packed();
                let mut r = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    p.len()
                )
                .into_bytes();
                r.extend_from_slice(&p[..p.len() / 2]);
                r
            }
            "/huge.apk.zst" => {
                // a few hundred KB that unpacks past the cap
                let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
                let zeros = vec![0u8; 1 << 20];
                for _ in 0..(MAX_APK >> 20) + 1 {
                    std::io::Write::write_all(&mut enc, &zeros).unwrap();
                }
                ok(&enc.finish().unwrap())
            }
            _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        }
    }

    async fn fetch(file: &str) -> (Result<(), Error>, Vec<u8>) {
        let base = serve(channel_server).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("update.apk");
        let res = download(&client(), &base, &release(file), &dest, |_, _| {}).await;
        (res, std::fs::read(&dest).unwrap_or_default())
    }

    #[tokio::test]
    async fn fetches_and_unpacks_a_release() {
        let base = serve(channel_server).await;
        let client = client();
        let release = fetch_release(&client, &base, &abis(&["arm64-v8a"])).await.unwrap();
        assert_eq!(release, Release {
            version: "3.1.0".into(),
            version_code: 60,
            file: "r-60.apk.zst".into()
        });

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("update.apk");
        let mut last = (0, 0);
        download(&client, &base, &release, &dest, |got, total| last = (got, total))
            .await
            .unwrap();
        let n = packed().len() as u64;
        assert_eq!(last, (n, n), "progress counts the compressed bytes");
        assert!(std::fs::read(&dest).unwrap() == apk());
    }

    #[tokio::test]
    async fn a_short_body_is_an_error() {
        let (res, _) = fetch("short.apk.zst").await;
        // hyper usually reports the early close itself, either way it fails
        assert!(
            matches!(res, Err(Error::Http(_) | Error::Truncated { .. })),
            "{res:?}"
        );
    }

    #[tokio::test]
    async fn a_cut_stream_is_incomplete() {
        let (res, _) = fetch("half.apk.zst").await;
        assert!(matches!(res, Err(Error::Incomplete)), "{res:?}");
    }

    /// The target only ever appears whole, a failure leaves nothing behind.
    #[tokio::test]
    async fn the_target_appears_only_when_whole() {
        let base = serve(channel_server).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("update.apk");
        let res = download(&client(), &base, &release("half.apk.zst"), &dest, |_, _| {}).await;
        assert!(res.is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "a partial file was left");

        let mut saw_target_early = false;
        let dest2 = dest.clone();
        download(&client(), &base, &release("r-60.apk.zst"), &dest, |_, _| {
            saw_target_early |= dest2.exists();
        })
        .await
        .unwrap();
        assert!(!saw_target_early, "the target existed mid-download");
        assert!(std::fs::read(&dest).unwrap() == apk());
        assert!(!dest.with_extension("apk.part").exists());
    }

    #[tokio::test]
    async fn an_uncompressed_apk_does_not_unpack() {
        let (res, _) = fetch("plain.apk.zst").await;
        assert!(matches!(res, Err(Error::Decompress(_))), "{res:?}");
    }

    #[tokio::test]
    async fn a_bomb_stops_at_the_cap() {
        let (res, out) = fetch("huge.apk.zst").await;
        assert!(matches!(res, Err(Error::TooLarge)), "{res:?}");
        assert!(out.len() as u64 <= MAX_APK);
    }

    #[tokio::test]
    async fn missing_files_and_bad_manifests_are_errors() {
        let (res, _) = fetch("gone.apk.zst").await;
        assert!(matches!(res, Err(Error::Status(s)) if s == 404), "{res:?}");

        let arm64 = abis(&["arm64-v8a"]);
        let base = serve(|_| ok(br#"{"version":"1","version_code":2,"files":{"arm64-v8a":"../x.apk.zst"}}"#)).await;
        let res = fetch_release(&client(), &base, &arm64).await;
        assert!(matches!(res, Err(Error::BadFileName(_))), "{res:?}");

        let base = serve(|_| ok(b"<html>not found</html>")).await;
        let res = fetch_release(&client(), &base, &arm64).await;
        assert!(matches!(res, Err(Error::Manifest(_))), "{res:?}");

        let base = serve(|_| ok(&vec![b' '; MAX_MANIFEST + 1])).await;
        let res = fetch_release(&client(), &base, &arm64).await;
        assert!(matches!(res, Err(Error::ManifestTooLarge)), "{res:?}");
    }
}
