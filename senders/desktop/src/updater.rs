// Techniques from: https://github.com/tauri-apps/plugins-workspace/blob/v2/plugins/updater/src/updater.rs

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use mcore::Release;
use tracing::{debug, info};

const BASE_URL: &str = "http://127.0.0.1:8000";
#[cfg(target_os = "macos")]
const OS_SPECIFIC_PATH: &str = "/macos-aarch64";

type MainThreadClosure = Box<dyn FnOnce() + Send + Sync + 'static>;
type RunOnMainThread = Box<dyn Fn(MainThreadClosure) -> bool + Send + Sync + 'static>;

fn str_to_version(s: &str) -> Option<u32> {
    let mut segments = s.split('.');
    let major = segments.next()?.parse::<u8>().ok()?;
    let minor = segments.next()?.parse::<u8>().ok()?;
    let patch = segments.next()?.parse::<u8>().ok()?;
    Some(u32::from_be_bytes([0, major, minor, patch]))
}

pub async fn check_for_update() -> Result<Option<Release>> {
    let latest_release = reqwest::get(BASE_URL.to_owned() + OS_SPECIFIC_PATH + "/latest.json")
        .await?
        .json::<Release>()
        .await?;

    info!(?latest_release, "Got latest release info");

    let latest_version =
        str_to_version(&latest_release.version).ok_or(anyhow!("Invalid version"))?;
    let current_version =
        str_to_version(env!("CARGO_PKG_VERSION")).ok_or(anyhow!("Invalid version"))?;

    debug!(latest_version, current_version);

    Ok(if latest_version > current_version {
        Some(latest_release)
    } else {
        None
    })
}

pub async fn download_update<F>(update: &Release, on_progress_update: F) -> Result<Bytes>
where
    F: Fn(u64, u64) + 'static,
{
    let url = BASE_URL.to_owned() + OS_SPECIFIC_PATH + "/" + &update.file;

    let mut request = reqwest::get(url).await?;

    let length = request.content_length().unwrap_or(0);
    let mut content = bytes::BytesMut::with_capacity(length as usize);

    while let Some(chunk) = request.chunk().await? {
        content.extend(chunk);
        on_progress_update(content.len() as u64, length);
    }

    Ok(content.into())
}

pub async fn install_update(
    installer_file: Bytes,
    run_on_main_thread: RunOnMainThread,
) -> Result<()> {
    use flate2::read::GzDecoder;

    let cursor = std::io::Cursor::new(installer_file);
    let mut extracted_files: Vec<PathBuf> = Vec::new();

    let tmp_backup_dir = tempfile::Builder::new()
        .prefix("fcast_sender_current_app")
        .tempdir()?;

    let tmp_extract_dir = tempfile::Builder::new()
        .prefix("fcast_sender_updated_app")
        .tempdir()?;

    let decoder = GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(decoder);

    // Extract files to temporary directory
    for entry in archive.entries()? {
        let mut entry = entry?;
        let collected_path: PathBuf = entry.path()?.iter().skip(1).collect();
        let extraction_path = tmp_extract_dir.path().join(&collected_path);

        // Ensure parent directories exist
        if let Some(parent) = extraction_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if let Err(err) = entry.unpack(&extraction_path) {
            // Cleanup on error
            std::fs::remove_dir_all(tmp_extract_dir.path()).ok();
            return Err(err.into());
        }
        extracted_files.push(extraction_path);
    }

    debug!("Extracted update");

    let extract_path = "/Applications/FCast Sender.app";

    // Try to move the current app to backup
    let move_result = std::fs::rename(extract_path, tmp_backup_dir.path().join("current_app"));
    let need_authorization = if let Err(err) = move_result {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            true
        } else {
            std::fs::remove_dir_all(tmp_extract_dir.path()).ok();
            return Err(err.into());
        }
    } else {
        false
    };

    if need_authorization {
        debug!("App installation needs admin privileges");
        // Use AppleScript to perform moves with admin privileges
        let apple_script = format!(
            "do shell script \"rm -rf '{src}' && mv -f '{new}' '{src}'\" with administrator privileges",
            src = extract_path,
            new = tmp_extract_dir.path().display()
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let did_fail = (run_on_main_thread)(Box::new(move || {
            let mut script =
                osakit::Script::new_from_source(osakit::Language::AppleScript, &apple_script);
            script.compile().expect("invalid AppleScript");
            let r = script.execute();
            tx.send(r).unwrap();
        }));
        let result = rx.recv().unwrap();

        if did_fail || result.is_err() {
            std::fs::remove_dir_all(tmp_extract_dir.path()).ok();
            anyhow::bail!("Failed to move the new app into place");
        }
    } else {
        // Remove existing directory if it exists
        if PathBuf::from(extract_path).exists() {
            std::fs::remove_dir_all(extract_path)?;
        }
        // Move the new app to the target path
        std::fs::rename(tmp_extract_dir.path(), extract_path)?;
    }

    let _ = std::process::Command::new("touch")
        .arg(extract_path)
        .status();

    Ok(())
}
