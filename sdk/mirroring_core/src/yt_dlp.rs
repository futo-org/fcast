use anyhow::Result;
use serde::Deserialize;
use smallvec::SmallVec;
use smol_str::SmolStr;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, error};

#[derive(Debug, Deserialize)]
pub struct Thumbnail {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub id: SmolStr,
}

#[derive(Debug, Deserialize)]
pub struct Format {
    #[serde(rename = "format_id")]
    pub id: SmolStr,
    pub url: String,
    pub manifest_url: Option<String>,
    pub protocol: SmolStr,
    pub container: Option<SmolStr>,
}

impl Format {
    pub fn content_type(&self) -> Option<&'static str> {
        Some(match self.protocol.as_str() {
            "m3u8_native" => "application/vnd.apple.mpegurl",
            "http_dash_segments" => "application/dash+xml",
            _ => match &self.container {
                Some(container) => match container.as_str() {
                    "m4a_dash" | "m4v_dash" => "application/dash+xml",
                    _ => return None,
                },
                None => match self.id.as_str() {
                    "mp4" => "video/mp4",
                    "x-matroska" => "video/x-matroska",
                    _ => return None,
                }
            },
        })
    }

    pub fn src_url(&self) -> String {
        self.manifest_url.clone().unwrap_or(self.url.clone())
    }
}

#[derive(Debug, Deserialize)]
pub struct YtDlpSource {
    pub id: SmolStr,
    pub title: Option<String>,
    pub thumbnails: Option<SmallVec<[Thumbnail; 4]>>,
    pub formats: Option<Vec<Format>>,
    pub duration: Option<f64>,
}

fn yt_dlp_command() -> tokio::process::Command {
    tokio::process::Command::new("yt-dlp")
}

impl YtDlpSource {
    pub async fn try_get(
        url: &str,
        event_tx: &tokio::sync::mpsc::UnboundedSender<crate::Event>,
        mut quit_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<()> {
        let mut cmd = yt_dlp_command();
        cmd.args(["--dump-json", url]);
        cmd.stdout(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or(anyhow::anyhow!("child is missing stdout"))?;

        let mut reader = BufReader::new(stdout).lines();

        loop {
            tokio::select! {
                line = reader.next_line() => {
                    let Some(line) = line? else {
                        break;
                    };

                    let source = match serde_json::from_str::<YtDlpSource>(&line) {
                        Ok(src) => src,
                        Err(err) => {
                            error!(?err, "Invalid YtDlpSource");
                            continue;
                        }
                    };
                    event_tx.send(crate::Event::YtDlp(crate::YtDlpEvent::SourceAvailable(Box::new(source))))?;
                }
                _ = &mut quit_rx => {
                    debug!("Got quit signal");
                    child.kill().await?;
                    return Ok(());
                }
            }
        }

        event_tx.send(crate::Event::YtDlp(crate::YtDlpEvent::Finished))?;

        if let Err(err) = child.wait().await {
            error!(?err, "yt-dlp failed");
        }

        debug!("yt-dlp finished");

        Ok(())
    }
}

pub async fn is_yt_dlp_available() -> Result<bool> {
    let output = yt_dlp_command().arg("--version").output();

    let output = output.await?;

    if let Ok(stdout) = str::from_utf8(&output.stdout) {
        debug!(stdout, "Yt-dlp stdout output");
    }

    Ok(output.status.success())
}
