//! The `/stream` TCP data connection that carries the mirrored H.264 video.
//!
//! After `SETUP`, iOS opens a *second* TCP connection (distinct from the
//! HTTP/RTSP control connection) to the `dataPort` we returned. Each packet is a
//! fixed 128-byte header followed by `payload_size` bytes of body
//! (`raop_rtp_mirror.c`):
//!
//! | Bytes  | Field           | Endian | Notes                                                                               |
//! |--------|-----------------|--------|-------------------------------------------------------------------------------------|
//! | 0–3    | `payload_size`  | LE     | body length following the header                                                    |
//! | 4      | `packet_type`   | -      | 0x00 video, 0x10 video (IDR), 0x01 SPS/PPS config, 0x02 heartbeat, 0x05 perf report |
//! | 6      | `option`        | -      | 0x56/0x5e on a config packet = client sleeping (pause)                              |
//! | 8–15   | `ntp_timestamp` | LE     | raw client clock (nanoseconds since boot)                                           |
//! | 16–127 | metadata        | -      | for config packets: image dimensions as IEEE-754 floats                             |
//!
//! Config (`0x01`) bodies are unencrypted; video (`0x00`/`0x10`) bodies are
//! AES-128-CTR encrypted (see [`crate::airplay::crypto`]).

use std::sync::mpsc::Sender;

use anyhow::Result;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
};
use tracing::{debug, info, instrument, warn};

use super::{crypto::MirrorCipher, h264, ntp::NtpClock, source::AccessUnit};
use crate::{MessageSender, message::AirPlay};

const HEADER_LEN: usize = 128;

/// Parsed 128-byte packet header.
#[derive(Debug, Clone, Copy)]
struct Header {
    payload_size: usize,
    packet_type: u8,
    option: u8,
    ntp_timestamp: u64,
}

impl Header {
    fn parse(buf: &[u8; HEADER_LEN]) -> Self {
        Self {
            payload_size: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize,
            packet_type: buf[4],
            option: buf[6],
            ntp_timestamp: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
        }
    }

    /// The packet timestamp as nanoseconds on the client's clock. The raw value
    /// is NTP-format (high 32 bits seconds, low 32 bits fraction) with no fixed
    /// epoch (`raop_ntp_timestamp_to_nano_seconds`).
    fn remote_pts_ns(&self) -> u64 {
        let seconds = (self.ntp_timestamp >> 32) & 0xffff_ffff;
        let fraction = self.ntp_timestamp & 0xffff_ffff;
        seconds * 1_000_000_000 + ((fraction * 1_000_000_000) >> 32)
    }
}

/// Image dimensions carried in a `0x01` config packet header.
#[derive(Debug, Clone, Copy)]
struct ConfigDimensions {
    width: f32,
    height: f32,
}

impl ConfigDimensions {
    /// Width/height are little-endian IEEE-754 floats at offsets 56/60 (the
    /// values are integral, e.g. `1920.0`).
    fn parse(header: &[u8; HEADER_LEN]) -> Self {
        let f = |off: usize| f32::from_le_bytes(header[off..off + 4].try_into().unwrap());
        Self {
            width: f(56),
            height: f(60),
        }
    }
}

/// Accept the single `/stream` data connection on `listener`, decrypt and frame
/// the video into Annex-B access units, and forward them on `au_tx` to the
/// `airplaysrc` element. Returns when the client disconnects (dropping `au_tx`,
/// which signals EOS to the source).
#[instrument(skip_all, fields(device = %device_name))]
pub async fn run(
    listener: TcpListener,
    mut cipher: MirrorCipher,
    au_tx: Sender<AccessUnit>,
    stream_connection_id: u64,
    msg_tx: MessageSender,
    ntp_clock: NtpClock,
    device_name: String,
) {
    let stream = match listener.accept().await {
        Ok((stream, peer)) => {
            info!(%peer, "mirror data connection accepted");
            stream
        }
        Err(err) => {
            warn!(?err, "failed to accept mirror data connection");
            return;
        }
    };

    if let Err(err) = read_loop(
        stream,
        &mut cipher,
        &au_tx,
        stream_connection_id,
        &msg_tx,
        &ntp_clock,
    )
    .await
    {
        debug!(cause = ?err, "mirror data connection closed");
    }
}

async fn read_loop(
    mut stream: TcpStream,
    cipher: &mut MirrorCipher,
    au_tx: &Sender<AccessUnit>,
    stream_connection_id: u64,
    msg_tx: &MessageSender,
    ntp_clock: &NtpClock,
) -> Result<()> {
    let mut packets: u64 = 0;
    // SPS/PPS from the most recent config packet, pending prepend to the next
    // (IDR) video frame - they share a timestamp (`raop_rtp_mirror.c`).
    let mut pending_sps_pps: Option<Vec<u8>> = None;
    // Whether the client has signalled it stopped sending video (screen asleep).
    // Tracked so we notify the app only on transitions.
    let mut suspended = false;
    loop {
        let mut header_buf = [0u8; HEADER_LEN];
        // read_exact errors with UnexpectedEof when the client closes the
        // connection between packets - the normal end of a session.
        if let Err(err) = stream.read_exact(&mut header_buf).await {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                debug!(packets, "client closed mirror data connection");
                return Ok(());
            }
            return Err(err.into());
        }

        let header = Header::parse(&header_buf);
        let mut payload = vec![0u8; header.payload_size];
        stream.read_exact(&mut payload).await?;
        packets += 1;

        match header.packet_type {
            // SPS/PPS config - unencrypted; carries the image dimensions.
            0x01 => {
                let dims = ConfigDimensions::parse(&header_buf);
                // The option byte flags whether the video stream is stopping
                // (client sleeping) - 0x56/0x5e - or (re)starting - 0x16/0x1e
                // (`raop_rtp_mirror.c`). Notify the app only on transitions.
                let sleeping = matches!(header.option, 0x56 | 0x5e);
                info!(
                    width = dims.width,
                    height = dims.height,
                    payload = header.payload_size,
                    sleeping,
                    "mirror config packet (SPS/PPS)"
                );
                if sleeping && !suspended {
                    suspended = true;
                    msg_tx.airplay(AirPlay::MirrorPaused {
                        stream_connection_id,
                    });
                } else if !sleeping && suspended {
                    suspended = false;
                    msg_tx.airplay(AirPlay::MirrorResumed {
                        stream_connection_id,
                    });
                }
                match h264::config_to_annex_b(&payload) {
                    Some(sps_pps) => pending_sps_pps = Some(sps_pps),
                    None => warn!(payload = header.payload_size, "unparseable config packet"),
                }
            }
            // Video - encrypted. Decrypt, convert to Annex-B, prepend any pending
            // SPS/PPS, and forward the access unit.
            0x00 | 0x10 => {
                cipher.decrypt(&mut payload);
                if !h264::length_prefixed_to_annex_b(&mut payload) {
                    warn!(
                        size = header.payload_size,
                        "malformed video packet, dropping"
                    );
                    continue;
                }
                let with_config = pending_sps_pps.is_some();
                let data = match pending_sps_pps.take() {
                    Some(mut sps_pps) => {
                        sps_pps.extend_from_slice(&payload);
                        sps_pps
                    }
                    None => payload,
                };
                let remote_pts_ns = header.remote_pts_ns();
                // When the NTP clock has synced, log the end-to-end latency
                // (remote capture time -> now) as a diagnostic. This does not
                // drive PTS - tight A/V sync additionally needs the audio RTCP
                // mapping and a running-time conversion (see `ntp`).
                let latency_ms = ntp_clock.remote_to_local_ns(remote_pts_ns).map(|local| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos() as i64)
                        .unwrap_or(0);
                    (now - local as i64) / 1_000_000
                });
                let au = AccessUnit {
                    pts_ns: remote_pts_ns,
                    data,
                };
                debug!(
                    idr = header.packet_type == 0x10,
                    bytes = au.data.len(),
                    with_config,
                    pts_ns = au.pts_ns,
                    latency_ms,
                    "mirror access unit"
                );
                // A send error means the source element/pipeline is gone; end.
                if au_tx.send(au).is_err() {
                    debug!("access-unit receiver dropped, ending stream reader");
                    return Ok(());
                }
            }
            0x02 => debug!("mirror heartbeat packet"),
            0x05 => debug!(size = header.payload_size, "mirror perf report packet"),
            other => debug!(
                packet_type = other,
                size = header.payload_size,
                "mirror packet"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 128-byte header with the given fields, dimensions at 56/60.
    fn make_header(payload_size: u32, ptype: u8, option: u8, w: f32, h: f32) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(&payload_size.to_le_bytes());
        buf[4] = ptype;
        buf[6] = option;
        buf[8..16].copy_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
        buf[56..60].copy_from_slice(&w.to_le_bytes());
        buf[60..64].copy_from_slice(&h.to_le_bytes());
        buf
    }

    #[test]
    fn parses_header_fields() {
        let buf = make_header(4096, 0x10, 0x00, 1920.0, 1080.0);
        let h = Header::parse(&buf);
        assert_eq!(h.payload_size, 4096);
        assert_eq!(h.packet_type, 0x10);
        assert_eq!(h.option, 0x00);
        assert_eq!(h.ntp_timestamp, 0x0123_4567_89ab_cdef);
    }

    #[test]
    fn converts_ntp_timestamp_to_ns() {
        // 5 seconds + half a second (fraction = 2^31).
        let ntp = (5u64 << 32) | (1u64 << 31);
        let mut buf = [0u8; HEADER_LEN];
        buf[8..16].copy_from_slice(&ntp.to_le_bytes());
        assert_eq!(Header::parse(&buf).remote_pts_ns(), 5_500_000_000);
    }

    #[test]
    fn parses_config_dimensions() {
        let buf = make_header(64, 0x01, 0x56, 1280.0, 720.0);
        let dims = ConfigDimensions::parse(&buf);
        assert_eq!(dims.width, 1280.0);
        assert_eq!(dims.height, 720.0);
    }
}
