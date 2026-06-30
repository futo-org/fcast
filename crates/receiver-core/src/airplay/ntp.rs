//! AirPlay NTP timing service.
//!
//! During `SETUP` the client advertises a `timingPort`; the receiver is the
//! *active* NTP peer, periodically polling that port to estimate the offset
//! between the client's ("remote") wall clock and our local wall clock. This is
//! a direct port of UxPlay's `raop_ntp.c`.
//!
//! # Wire format
//!
//! Each poll sends a fixed 32-byte request and reads a 32-byte response:
//!
//! ```text
//! request  [0]=0x80 [1]=0xd2 [2..4]=0x0007
//!          [8..16]  client reference time (echo of the previous t2), big-endian
//!          [16..24] our previous receive time (NTP timestamp)
//!          [24..32] our send time t0 (NTP timestamp)
//! response [8..16]  t0 echoed back (originate timestamp)
//!          [16..24] t1, client receive time
//!          [24..32] t2, client transmit time
//! ```
//!
//! With `t3` = our local receive time, the standard NTP estimators are
//! `offset = ((t1 - t0) + (t2 - t3)) / 2` and `delay = (t3 - t0) - (t2 - t1)`.
//! We keep the last [`NTP_DATA_COUNT`] samples and adopt the offset of the
//! lowest-delay one (the least network-jittered estimate).
//!
//! # Scope
//!
//! This maintains the remote↔local clock and is wired into `SETUP` (we advertise
//! a real `timingPort` and answer the client). It is deliberately *additive*:
//! it does not yet drive buffer PTS. Tight lip-sync additionally needs the audio
//! RTCP sync packets (mapping the audio RTP timestamp onto this clock) and a
//! mapping from the resulting `CLOCK_REALTIME` domain onto GStreamer
//! running-time; both are the remaining follow-on work.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tracing::{debug, info, instrument, warn};

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
const SECONDS_FROM_1900_TO_1970: u64 = 2_208_988_800;
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Number of recent samples kept for the lowest-delay offset selection.
const NTP_DATA_COUNT: usize = 8;

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const RECV_TIMEOUT: Duration = Duration::from_millis(300);

/// A shared, drift-corrected mapping between the client's ("remote") wall clock
/// and our local wall clock, both in nanoseconds. Cloneable; clones share state.
///
/// `offset` is `remote - local`; it is `None` until the first successful poll.
#[derive(Clone, Default)]
pub struct NtpClock {
    offset_ns: Arc<Mutex<Option<i64>>>,
}

impl NtpClock {
    pub fn new() -> Self {
        Self::default()
    }

    fn set_offset(&self, offset_ns: i64) {
        *self.offset_ns.lock() = Some(offset_ns);
    }

    /// Convert a remote timestamp (ns on the client's clock) to our local wall
    /// clock (ns since the Unix epoch), or `None` if not yet synced.
    pub fn remote_to_local_ns(&self, remote_ns: u64) -> Option<u64> {
        let offset = (*self.offset_ns.lock())?;
        Some((remote_ns as i64 - offset).max(0) as u64)
    }
}

/// One round-trip sample.
#[derive(Clone, Copy)]
struct Sample {
    offset: i64,
    delay: i64,
}

/// The local wall clock in nanoseconds since the Unix epoch.
fn local_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Write an NTP timestamp (BE seconds-since-1900 . fraction) at `offset`.
fn put_ntp_timestamp(buf: &mut [u8], offset: usize, ns_since_1970: u64) {
    let seconds = ns_since_1970 / NANOS_PER_SEC + SECONDS_FROM_1900_TO_1970;
    let nanos = ns_since_1970 % NANOS_PER_SEC;
    let fraction = (nanos << 32) / NANOS_PER_SEC;
    buf[offset..offset + 4].copy_from_slice(&(seconds as u32).to_be_bytes());
    buf[offset + 4..offset + 8].copy_from_slice(&(fraction as u32).to_be_bytes());
}

/// Read an NTP timestamp written by [`put_ntp_timestamp`] back to ns since 1970.
fn get_ntp_timestamp(buf: &[u8], offset: usize) -> u64 {
    let seconds = u32::from_be_bytes(buf[offset..offset + 4].try_into().unwrap()) as u64
        - SECONDS_FROM_1900_TO_1970;
    let fraction = u32::from_be_bytes(buf[offset + 4..offset + 8].try_into().unwrap()) as u64;
    seconds * NANOS_PER_SEC + ((fraction * NANOS_PER_SEC) >> 32)
}

/// Read a big-endian u64 at `offset`.
fn get_long_be(buf: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(buf[offset..offset + 8].try_into().unwrap())
}

/// Convert a remote timestamp (an NTP-format u64: high 32 = seconds, low 32 =
/// fraction) to nanoseconds. When `ntp_epoch` is set the seconds field is
/// relative to 1900 and the Unix-epoch difference is removed; otherwise it is a
/// bare monotonic (since-boot) counter, matching the mirror video timestamps.
fn remote_timestamp_to_ns(timestamp: u64, ntp_epoch: bool) -> u64 {
    let mut seconds = (timestamp >> 32) & 0xffff_ffff;
    if ntp_epoch {
        seconds = seconds.saturating_sub(SECONDS_FROM_1900_TO_1970);
    }
    let fraction = timestamp & 0xffff_ffff;
    seconds * NANOS_PER_SEC + ((fraction * NANOS_PER_SEC) >> 32)
}

/// Compute the offset/delay sample for one exchange. Split out for testing.
fn sample(t0: i64, t1: i64, t2: i64, t3: i64) -> Sample {
    Sample {
        offset: ((t1 - t0) + (t2 - t3)) / 2,
        delay: (t3 - t0) - (t2 - t1),
    }
}

/// Select the offset of the lowest-delay sample among those collected so far.
fn best_offset(samples: &[Sample]) -> Option<i64> {
    samples.iter().min_by_key(|s| s.delay).map(|s| s.offset)
}

/// Run the NTP timing client against the client's `timing_addr` until the task
/// is aborted. Updates `clock` after every successful poll.
#[instrument(skip_all, fields(device = %device_name, %timing_addr))]
pub async fn run(socket: UdpSocket, timing_addr: SocketAddr, clock: NtpClock, device_name: String) {
    info!("mirror NTP timing client started");
    let mut samples: Vec<Sample> = Vec::with_capacity(NTP_DATA_COUNT);
    // The previous exchange's receive time and the client's reference time,
    // echoed back to the client on the next request (0 until the first reply).
    let mut prev_recv_time: u64 = 0;
    let mut client_ref_time: u64 = 0;
    let mut ticker = tokio::time::interval(POLL_INTERVAL);

    loop {
        ticker.tick().await;

        let send_time = local_time_ns();
        let mut request = [0u8; 32];
        request[0] = 0x80;
        request[1] = 0xd2;
        request[3] = 0x07;
        put_ntp_timestamp(&mut request, 24, send_time);
        if prev_recv_time != 0 {
            request[8..16].copy_from_slice(&client_ref_time.to_be_bytes());
            put_ntp_timestamp(&mut request, 16, prev_recv_time);
        }

        if let Err(err) = socket.send_to(&request, timing_addr).await {
            warn!(?err, "mirror NTP request send failed");
            continue;
        }

        let mut response = [0u8; 64];
        let n = match tokio::time::timeout(RECV_TIMEOUT, socket.recv(&mut response)).await {
            Ok(Ok(n)) => n,
            Ok(Err(err)) => {
                debug!(?err, "mirror NTP socket error");
                continue;
            }
            Err(_) => {
                debug!("mirror NTP receive timeout");
                continue;
            }
        };
        if n < 32 {
            debug!(n, "short mirror NTP response");
            continue;
        }

        let recv_time = local_time_ns();
        client_ref_time = get_long_be(&response, 24);
        prev_recv_time = recv_time;

        // t0: our send time echoed back; t1/t2: client receive/transmit times;
        // t3: our receive time. The mirror timing clock carries the 1900 epoch.
        let t0 = get_ntp_timestamp(&response, 8) as i64;
        let t1 = remote_timestamp_to_ns(get_long_be(&response, 16), true) as i64;
        let t2 = remote_timestamp_to_ns(get_long_be(&response, 24), true) as i64;
        let t3 = recv_time as i64;

        let s = sample(t0, t1, t2, t3);
        if samples.len() == NTP_DATA_COUNT {
            samples.remove(0);
        }
        samples.push(s);

        if let Some(offset) = best_offset(&samples) {
            clock.set_offset(offset);
            debug!(
                offset_ms = offset / 1_000_000,
                delay_ms = s.delay / 1_000_000,
                samples = samples.len(),
                "mirror NTP sync"
            );
        }
    }
}

/// Build the client timing address from the connection peer IP and the
/// `timingPort` advertised in `SETUP`.
pub fn timing_addr(peer_ip: IpAddr, timing_rport: u16) -> SocketAddr {
    SocketAddr::new(peer_ip, timing_rport)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntp_timestamp_round_trips() {
        let mut buf = [0u8; 8];
        // 1234.5 seconds since the Unix epoch.
        let ns = 1234 * NANOS_PER_SEC + NANOS_PER_SEC / 2;
        put_ntp_timestamp(&mut buf, 0, ns);
        let back = get_ntp_timestamp(&buf, 0);
        // The 32-bit fraction has ~0.23 ns resolution; allow a tiny rounding gap.
        assert!((back as i64 - ns as i64).abs() < 2, "got {back}, want {ns}");
    }

    #[test]
    fn remote_timestamp_epoch_handling() {
        // 5.5 seconds, since-boot (no epoch): high 32 = 5, low 32 = 2^31.
        let ts = (5u64 << 32) | (1u64 << 31);
        assert_eq!(remote_timestamp_to_ns(ts, false), 5_500_000_000);
        // With the 1900 epoch, the seconds field must exceed the epoch constant.
        let ts_epoch = ((5 + SECONDS_FROM_1900_TO_1970) << 32) | (1u64 << 31);
        assert_eq!(remote_timestamp_to_ns(ts_epoch, true), 5_500_000_000);
    }

    #[test]
    fn offset_and_delay_are_symmetric() {
        // Client clock exactly 100 s ahead, symmetric 10 ms one-way delay.
        let t0 = 1_000_000_000_000; // local send
        let t1 = t0 + 100 * NANOS_PER_SEC as i64 + 10_000_000; // remote recv
        let t2 = t1 + 5_000_000; // remote processing
        let t3 = t2 - 100 * NANOS_PER_SEC as i64 + 10_000_000; // local recv
        let s = sample(t0, t1, t2, t3);
        assert_eq!(s.offset, 100 * NANOS_PER_SEC as i64, "offset ~ +100 s");
        assert_eq!(s.delay, 20_000_000, "round-trip ~ 20 ms");
    }

    #[test]
    fn best_offset_picks_lowest_delay() {
        let samples = [
            Sample {
                offset: 10,
                delay: 50,
            },
            Sample {
                offset: 20,
                delay: 5,
            },
            Sample {
                offset: 30,
                delay: 90,
            },
        ];
        assert_eq!(best_offset(&samples), Some(20));
        assert_eq!(best_offset(&[]), None);
    }

    #[test]
    fn clock_converts_when_synced() {
        let clock = NtpClock::new();
        // Unsynced: no conversion.
        assert_eq!(clock.remote_to_local_ns(1_000), None);
        // remote is 500 ns ahead of local.
        clock.set_offset(500);
        assert_eq!(clock.remote_to_local_ns(1_000), Some(500));
    }
}
