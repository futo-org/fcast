//! Self-update for the sideloaded android build. The channel is a
//! `latest.json` next to the apks, ordered by versionCode since that is what
//! the platform orders installs by. This side checks and downloads, the
//! activity verifies the apk and hands it to PackageInstaller.

use std::{path::Path, time::Duration};

use serde::Deserialize;
use tokio::io::AsyncWriteExt;

pub const CHANNEL_URL: &str = "https://dl.fcast.org/receiver/android/";
const MANIFEST: &str = "latest.json";
/// A chunk gap this long fails the download instead of leaving the dialog at
/// a frozen percentage.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
    #[error("release manifest is malformed: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("release names an invalid file {0:?}")]
    BadFileName(String),
    #[error("writing the download failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("download stalled")]
    Stalled,
    #[error("download ended at {got} of {expected} bytes")]
    Truncated { got: u64, expected: u64 },
}

impl Release {
    /// A plain `*.apk` name, never a path or a URL, so the manifest can only
    /// point inside the channel directory.
    fn validate(&self) -> Result<(), Error> {
        let f = self.file.as_str();
        let plain = f.len() > ".apk".len()
            && f.ends_with(".apk")
            && !f.starts_with('.')
            && f.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if plain {
            Ok(())
        } else {
            Err(Error::BadFileName(self.file.clone()))
        }
    }

    pub fn is_newer_than(&self, installed_version_code: i64) -> bool {
        self.version_code > installed_version_code
    }
}

pub async fn fetch_release(client: &reqwest::Client, channel: &str) -> Result<Release, Error> {
    let resp = client
        .get(format!("{channel}{MANIFEST}"))
        .timeout(MANIFEST_TIMEOUT)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::Status(resp.status()));
    }
    let release: Release = serde_json::from_slice(&resp.bytes().await?)?;
    release.validate()?;
    Ok(release)
}

/// Streams the release apk to `dest`. `progress` gets (received, total),
/// total is 0 when the server sends no length.
pub async fn download(
    client: &reqwest::Client,
    channel: &str,
    release: &Release,
    dest: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<(), Error> {
    release.validate()?;
    let mut resp = client
        .get(format!("{channel}{}", release.file))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::Status(resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(dest).await?;
    let mut got = 0u64;
    while let Some(chunk) = tokio::time::timeout(STALL_TIMEOUT, resp.chunk())
        .await
        .map_err(|_| Error::Stalled)??
    {
        file.write_all(&chunk).await?;
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

    use jni::objects::{JObject, JString, JValue};
    use tracing::{debug, info, warn};

    use super::{CHANNEL_URL, fetch_release};
    use crate::message::{AppUpdate, MessageSender};

    /// The (vm, activity) pair the Application keeps.
    pub type Jni = (usize, usize);

    const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
    /// Debug builds only: the channel to test against, e.g.
    /// `adb shell setprop debug.fcast.update_url http://192.168.1.2:8000/`.
    const DEBUG_CHANNEL_PROP: &std::ffi::CStr = c"debug.fcast.update_url";

    // Mirrors Updater.java's MODE_* values.
    const MODE_RELEASE: i32 = 1;
    const MODE_DEBUG: i32 = 2;

    /// Runs `f` against the activity in a local frame. The callers sit on
    /// permanently attached threads, where local refs are never freed
    /// otherwise.
    fn with_activity<R>(
        jni: Jni,
        what: &str,
        f: impl FnOnce(&mut jni::JNIEnv, &JObject) -> jni::errors::Result<R>,
    ) -> Option<R> {
        let vm = unsafe { jni::JavaVM::from_raw(jni.0 as *mut _) }.ok()?;
        let mut env = vm.attach_current_thread_permanently().ok()?;
        let activity = unsafe { JObject::from_raw(jni.1 as jni::sys::jobject) };
        match env.with_local_frame(8, |env| f(env, &activity)) {
            Ok(v) => Some(v),
            Err(err) => {
                let _ = env.exception_clear();
                warn!(?err, what, "updater call into the activity failed");
                None
            }
        }
    }

    /// The channel this build updates from with its installed versionCode,
    /// None when it must not self-update.
    fn channel(jni: Jni) -> Option<(String, i64)> {
        let (mode, code) = with_activity(jni, "updaterMode", |env, a| {
            let mode = env.call_method(a, "updaterMode", "()I", &[])?.i()?;
            let code = env.call_method(a, "installedVersionCode", "()J", &[])?.j()?;
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

    /// Checks at startup and then daily, an hour after a failed check.
    pub async fn run_checker(jni: Jni, client: reqwest::Client, msg_tx: MessageSender) {
        let Some((channel, installed)) = channel(jni) else {
            info!("self-update is off for this build");
            return;
        };
        info!(channel, installed, "self-update channel");
        loop {
            let wait = match fetch_release(&client, &channel).await {
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
    pub fn prepare_download(jni: Jni) -> Option<PathBuf> {
        with_activity(jni, "prepareUpdateDownload", |env, a| {
            let path = env
                .call_method(a, "prepareUpdateDownload", "()Ljava/lang/String;", &[])?
                .l()?;
            let path: String = env.get_string(&JString::from(path))?.into();
            Ok(PathBuf::from(path))
        })
    }

    /// Hands the downloaded apk to the installer. Asynchronous, a failure
    /// comes back as `AppUpdate::InstallFailed`.
    pub fn install(jni: Jni, apk: &Path, version_code: i64) -> bool {
        with_activity(jni, "installUpdate", |env, a| {
            let path = env.new_string(apk.to_string_lossy())?;
            env.call_method(
                a,
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

    #[test]
    fn file_names_stay_inside_the_channel() {
        for ok in ["fcast-receiver-android-60.apk", "a.apk", "x_1.2.3.apk"] {
            assert!(release(ok).validate().is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".apk",
            "../x.apk",
            "a/b.apk",
            "https://evil.example/x.apk",
            "x.apk?y",
            ".hidden.apk",
            "x.exe",
            "x%2f.apk",
        ] {
            assert!(
                matches!(release(bad).validate(), Err(Error::BadFileName(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn newer_is_strictly_greater() {
        let r = release("a.apk");
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

    const APK: &[u8] = &[0x5a; 100_000];

    fn channel_server(path: &str) -> Vec<u8> {
        match path {
            "/latest.json" => ok(br#"{"version":"3.1.0","version_code":60,"file":"r-60.apk"}"#),
            "/r-60.apk" => ok(APK),
            "/short.apk" => {
                let mut r = b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n"
                    .to_vec();
                r.extend_from_slice(&APK[..10_000]);
                r
            }
            _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        }
    }

    #[tokio::test]
    async fn fetches_and_downloads_a_release() {
        let base = serve(channel_server).await;
        let client = client();
        let release = fetch_release(&client, &base).await.unwrap();
        assert_eq!(release, Release {
            version: "3.1.0".into(),
            version_code: 60,
            file: "r-60.apk".into()
        });

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("update.apk");
        let mut last = (0, 0);
        download(&client, &base, &release, &dest, |got, total| last = (got, total))
            .await
            .unwrap();
        assert_eq!(last, (APK.len() as u64, APK.len() as u64));
        assert_eq!(std::fs::read(&dest).unwrap(), APK);
    }

    #[tokio::test]
    async fn a_short_body_is_an_error() {
        let base = serve(channel_server).await;
        let dir = tempfile::tempdir().unwrap();
        let res = download(
            &client(),
            &base,
            &release("short.apk"),
            &dir.path().join("update.apk"),
            |_, _| {},
        )
        .await;
        // hyper usually reports the early close itself, either way it fails
        assert!(
            matches!(res, Err(Error::Http(_) | Error::Truncated { .. })),
            "{res:?}"
        );
    }

    #[tokio::test]
    async fn missing_files_and_bad_manifests_are_errors() {
        let base = serve(channel_server).await;
        let dir = tempfile::tempdir().unwrap();
        let res = download(
            &client(),
            &base,
            &release("gone.apk"),
            &dir.path().join("update.apk"),
            |_, _| {},
        )
        .await;
        assert!(matches!(res, Err(Error::Status(s)) if s == 404), "{res:?}");

        let base = serve(|_| ok(br#"{"version":"1","version_code":2,"file":"../x.apk"}"#)).await;
        let res = fetch_release(&client(), &base).await;
        assert!(matches!(res, Err(Error::BadFileName(_))), "{res:?}");

        let base = serve(|_| ok(b"<html>not found</html>")).await;
        let res = fetch_release(&client(), &base).await;
        assert!(matches!(res, Err(Error::Manifest(_))), "{res:?}");
    }
}
