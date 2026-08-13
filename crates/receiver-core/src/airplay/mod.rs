//! AirPlay screen-mirroring receiver: `_airplay._tcp`, served over a hybrid
//! HTTP/1.1 + RTSP/1.0 connection (see [`http`]). Distinct from the legacy
//! RAOP/AirTunes audio receiver in [`crate::raop`].

mod audio;
mod crypto;
mod h264;
mod http;
mod ntp;
pub(crate) mod source;
mod stream;

pub(crate) use source::AirPlayContext;

use std::collections::HashMap;

use anyhow::{Context, Result};
use mdns_sd::ServiceInfo;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tracing::{debug, instrument, warn};

use crate::MessageSender;
use apple_fairplay::FairPlay;
use crypto::MirrorCipher;
use http::{Connection, Request, Response};

/// Apple's default AirPlay port; iOS expects the HTTP/RTSP server here.
pub const AIRPLAY_TCP_PORT: u16 = 7000;

/// TXT-record features bitmask, low/high 32-bit words. Bit 27 ("legacy
/// pairing") is deliberately off, so the client skips the `/pair-setup`
/// ed25519/x25519 handshake and goes straight to `/fp-setup`, whose recovered
/// AES key is used directly (no ECDH hashing).
const FEATURES_LO: u32 = 0x527F_FEE6;
const FEATURES_HI: u32 = 0x0;

/// Combined 64-bit features bitmask, reported in the `GET /info` plist.
const FEATURES: u64 = ((FEATURES_HI as u64) << 32) | (FEATURES_LO as u64);

/// Mirroring geometry in `GET /info`; iOS picks the streamed resolution from
/// it.
const DISPLAY_WIDTH: u64 = 1920;
const DISPLAY_HEIGHT: u64 = 1080;

/// Model/source version matching UxPlay's, so iOS treats us as a known
/// receiver.
const MODEL: &str = "AppleTV3,2";
const SOURCE_VERSION: &str = "220.68";

/// Stable AirPlay "pairing identifier" advertised in the `pi` TXT record.
const PAIRING_ID: &str = "2e388006-13ba-4041-9a67-25dd4a43d536";

#[derive(Debug, Clone)]
pub struct Configuration {
    pub device_name: String,
    /// 6-byte hardware address, shared with RAOP so both report one device id.
    pub hw_addr: [u8; 6],
    /// Public key advertised in the `pk` TXT record (hex).
    pub pk: String,
}

impl Configuration {
    /// `deviceid` in AirPlay MAC form, e.g. `aa:bb:cc:dd:ee:ff`.
    fn device_id(&self) -> String {
        self.hw_addr
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Raw public key: `/info` sends `pk` as binary data, not the TXT hex
    /// string.
    fn pk_raw(&self) -> Vec<u8> {
        (0..self.pk.len() / 2)
            .filter_map(|i| u8::from_str_radix(&self.pk[i * 2..i * 2 + 2], 16).ok())
            .collect()
    }

    /// DNS-TXT wire encoding of the TXT record: each entry is a length byte
    /// followed by `key=value`.
    fn airplay_txt_record(&self) -> Vec<u8> {
        let props = txt_properties(self);
        let mut entries: Vec<(&String, &String)> = props.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = Vec::new();
        for (k, v) in entries {
            let s = format!("{k}={v}");
            out.push(s.len() as u8);
            out.extend_from_slice(s.as_bytes());
        }
        out
    }

    /// Build the `GET /info` capabilities plist, mirroring UxPlay's response.
    fn info_plist_dict(&self) -> plist::Dictionary {
        use plist::Value;
        let device_id = self.device_id();
        let mut d = plist::Dictionary::new();

        d.insert("deviceID".into(), Value::String(device_id.clone()));
        d.insert("macAddress".into(), Value::String(device_id));
        d.insert("pk".into(), Value::Data(self.pk_raw()));
        d.insert("features".into(), Value::Integer((FEATURES as i64).into()));
        d.insert("name".into(), Value::String(self.device_name.clone()));
        d.insert("pi".into(), Value::String(PAIRING_ID.to_owned()));
        d.insert("vv".into(), Value::Integer(2i64.into()));
        d.insert("statusFlags".into(), Value::Integer(68i64.into()));
        d.insert("keepAliveLowPower".into(), Value::Integer(1i64.into()));
        d.insert(
            "sourceVersion".into(),
            Value::String(SOURCE_VERSION.to_owned()),
        );
        d.insert("keepAliveSendStatsAsBody".into(), Value::Boolean(true));
        d.insert("model".into(), Value::String(MODEL.to_owned()));
        d.insert("initialVolume".into(), Value::Real(0.0));

        d.insert("audioLatencies".into(), Value::Array(audio_array(0)));
        d.insert("audioFormats".into(), Value::Array(audio_array(0x3ff_fffc)));

        let mut display = plist::Dictionary::new();
        display.insert(
            "uuid".into(),
            Value::String("e0ff8a27-6738-3d56-8a16-cc53aacee925".to_owned()),
        );
        display.insert("widthPhysical".into(), Value::Integer(0i64.into()));
        display.insert("heightPhysical".into(), Value::Integer(0i64.into()));
        display.insert(
            "width".into(),
            Value::Integer((DISPLAY_WIDTH as i64).into()),
        );
        display.insert(
            "height".into(),
            Value::Integer((DISPLAY_HEIGHT as i64).into()),
        );
        display.insert(
            "widthPixels".into(),
            Value::Integer((DISPLAY_WIDTH as i64).into()),
        );
        display.insert(
            "heightPixels".into(),
            Value::Integer((DISPLAY_HEIGHT as i64).into()),
        );
        display.insert("rotation".into(), Value::Boolean(false));
        display.insert("refreshRate".into(), Value::Real(1.0 / 60.0));
        display.insert("maxFPS".into(), Value::Integer(60i64.into()));
        display.insert("overscanned".into(), Value::Boolean(false));
        display.insert("features".into(), Value::Integer(14i64.into()));
        d.insert(
            "displays".into(),
            Value::Array(vec![Value::Dictionary(display)]),
        );

        d
    }
}

/// `(has_audio, has_video)` for a `TEARDOWN` body's `streams` array (audio is
/// `type 96`, video `type 110`). No valid array yields `(false, false)`, which
/// callers treat as a full-session teardown.
fn teardown_stream_types(body: &[u8]) -> (bool, bool) {
    let Ok(root) = plist::from_bytes::<plist::Value>(body) else {
        return (false, false);
    };
    let Some(streams) = root
        .as_dictionary()
        .and_then(|d| d.get("streams"))
        .and_then(|s| s.as_array())
    else {
        return (false, false);
    };
    let mut has_audio = false;
    let mut has_video = false;
    for stream in streams {
        match stream
            .as_dictionary()
            .and_then(|d| d.get("type"))
            .and_then(plist_u64)
        {
            Some(96) => has_audio = true,
            Some(110) => has_video = true,
            _ => {}
        }
    }
    (has_audio, has_video)
}

/// Plist `u64`: iOS sends large ids (e.g. `streamConnectionID`) as an unsigned
/// integer, a signed integer, or a decimal string.
fn plist_u64(v: &plist::Value) -> Option<u64> {
    v.as_unsigned_integer()
        .or_else(|| v.as_signed_integer().map(|i| i as u64))
        .or_else(|| v.as_string().and_then(|s| s.parse().ok()))
}

/// The `audioLatencies`/`audioFormats` two-element arrays (types 100 and 101);
/// `formats` is `0` for latencies and the format bitmask for formats.
fn audio_array(formats: u64) -> Vec<plist::Value> {
    use plist::Value;
    [100u64, 101]
        .into_iter()
        .map(|ty| {
            let mut e = plist::Dictionary::new();
            e.insert("type".into(), Value::Integer((ty as i64).into()));
            if formats == 0 {
                e.insert("audioType".into(), Value::String("default".to_owned()));
                e.insert("inputLatencyMicros".into(), Value::Integer(0i64.into()));
                e.insert("outputLatencyMicros".into(), Value::Integer(0i64.into()));
            } else {
                e.insert(
                    "audioInputFormats".into(),
                    Value::Integer((formats as i64).into()),
                );
                e.insert(
                    "audioOutputFormats".into(),
                    Value::Integer((formats as i64).into()),
                );
            }
            Value::Dictionary(e)
        })
        .collect()
}

/// Build the `_airplay._tcp` mDNS service and its connection-handler config.
pub fn service_info(device_name: String) -> Result<(ServiceInfo, Configuration)> {
    let hw_addr = crate::raop::device_name_hash(&device_name);

    // Discovery only needs a stable 64-char hex string here.
    let pk = Sha256::digest(device_name.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    let config = Configuration {
        device_name: device_name.clone(),
        hw_addr,
        pk,
    };

    let host_name = format!("{device_name}.local.");
    let props = txt_properties(&config);

    let service = ServiceInfo::new(
        "_airplay._tcp.local.",
        &device_name,
        &host_name,
        (), // Auto
        AIRPLAY_TCP_PORT,
        props,
    )?
    .enable_addr_auto();

    Ok((service, config))
}

fn txt_properties(config: &Configuration) -> HashMap<String, String> {
    HashMap::from([
        ("deviceid".to_owned(), config.device_id()),
        (
            "features".to_owned(),
            format!("0x{FEATURES_LO:X},0x{FEATURES_HI:X}"),
        ),
        ("flags".to_owned(), "0x4".to_owned()),
        ("model".to_owned(), MODEL.to_owned()),
        ("pk".to_owned(), config.pk.clone()),
        ("pi".to_owned(), PAIRING_ID.to_owned()),
        ("srcvers".to_owned(), SOURCE_VERSION.to_owned()),
        ("vv".to_owned(), "2".to_owned()),
        ("pw".to_owned(), "false".to_owned()),
    ])
}

/// Handle a single sender connection for its whole lifetime.
pub async fn handle_sender(
    stream: tokio::net::TcpStream,
    config: Configuration,
    msg_tx: MessageSender,
    airplay_context: AirPlayContext,
) {
    let peer_ip = stream
        .peer_addr()
        .map(|addr| addr.ip())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let mut handler = Handler {
        config,
        connection: Connection::new(stream),
        fairplay: FairPlay::new(),
        aeskey: None,
        aesiv: None,
        msg_tx,
        airplay_context,
        peer_ip,
        mirror_session: None,
        tasks: Vec::new(),
        audio_task: None,
        ntp_clock: ntp::NtpClock::new(),
    };
    if let Err(err) = handler.run().await {
        tracing::error!(cause = ?err, "airplay connection error");
    }
}

struct Handler {
    config: Configuration,
    connection: Connection,
    fairplay: FairPlay,
    /// 16-byte AES key recovered from the `SETUP` `ekey` via FairPlay; set in
    /// phase A, seeds the mirror cipher in phase B.
    aeskey: Option<[u8; 16]>,
    /// 16-byte AES IV from the `SETUP` `eiv`, for AES-CBC audio decryption.
    aesiv: Option<[u8; 16]>,
    msg_tx: MessageSender,
    airplay_context: AirPlayContext,
    /// Client IP, used as the destination for NTP timing polls.
    peer_ip: std::net::IpAddr,
    /// `streamConnectionID` of the mirror this connection started; `None` on
    /// connections that never start one (e.g. `/info` probes), so closing them
    /// doesn't tear down a session.
    mirror_session: Option<u64>,
    /// Session background tasks (video reader, audio receiver, NTP), aborted on
    /// teardown so they stop promptly rather than on timeout/EOF.
    tasks: Vec<tokio::task::AbortHandle>,
    /// The audio receiver task, so an audio-only `TEARDOWN` can stop just it.
    audio_task: Option<tokio::task::AbortHandle>,
    /// Drift-corrected remote↔local clock maintained by the NTP timing client.
    ntp_clock: ntp::NtpClock,
}

impl Drop for Handler {
    fn drop(&mut self) {
        // Idempotent with the explicit `TEARDOWN` handler.
        self.full_teardown();
    }
}

impl Handler {
    #[instrument(skip(self), fields(device = %self.config.device_name))]
    async fn run(&mut self) -> Result<()> {
        while let Some(request) = self.connection.read_request().await? {
            debug!(
                method = %request.method,
                url = %request.url,
                protocol = %request.protocol,
                "airplay request"
            );
            let response = self.respond(&request).await;
            self.connection.write_response(&response).await?;
        }
        Ok(())
    }

    async fn respond(&mut self, request: &Request) -> Response {
        let mut response = match (request.method.as_str(), request.path()) {
            ("GET", "/info") => self.info(request),
            ("OPTIONS", _) => Response::new(&request.protocol, 200, "OK").header(
                "Public",
                "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, \
                 GET_PARAMETER, SET_PARAMETER, POST, GET, PUT",
            ),
            ("POST", "/feedback") => Response::new(&request.protocol, 200, "OK"),
            ("POST", "/fp-setup") => self.fp_setup(request),
            ("SETUP", _) => self.setup(request).await,
            ("SET_PARAMETER", _) => self.set_parameter(request),
            ("TEARDOWN", _) => self.teardown(request),
            _ => {
                // Permissive default (as in UxPlay) keeps the connection healthy.
                debug!(method = %request.method, url = %request.url, "unhandled airplay request");
                Response::new(&request.protocol, 200, "OK")
            }
        };

        // Default headers, matching UxPlay.
        response = response.header("Server", &format!("AirTunes/{SOURCE_VERSION}"));
        if let Some(cseq) = request.header("CSeq") {
            response = response.header("CSeq", cseq);
        }
        response
    }

    /// `TEARDOWN`: `streams: [{type: 96}]` alone stops *just* audio (the client
    /// paused music) and the video mirror must keep playing; a teardown listing
    /// video (`type 110`) or with no `streams` array ends the whole session.
    fn teardown(&mut self, request: &Request) -> Response {
        let (has_audio, has_video) = teardown_stream_types(&request.body);
        if has_audio && !has_video {
            debug!("airplay audio-only teardown (video mirror continues)");
            self.teardown_audio();
        } else {
            self.full_teardown();
        }
        Response::new(&request.protocol, 200, "OK")
    }

    /// Stop only the audio receiver task; the audio channel stays registered so
    /// a later audio `SETUP` resumes feeding the existing pad.
    fn teardown_audio(&mut self) {
        if let Some(handle) = self.audio_task.take() {
            handle.abort();
        }
    }

    /// Stop this connection's mirror session: abort its background tasks and
    /// tell the app to stop the player. Idempotent - runs on `TEARDOWN` and
    /// on `Drop`; only the connection that started the mirror sends
    /// `MirrorStopped`.
    fn full_teardown(&mut self) {
        // Notify before aborting the video task, so `MirrorStopped` is queued
        // ahead of the player EOS that abort triggers (the app's generic EOS
        // path would otherwise clear state without resetting the GUI).
        if let Some(stream_connection_id) = self.mirror_session.take() {
            debug!(stream_connection_id, "airplay mirror teardown");
            self.airplay_context.end_session(stream_connection_id);
            self.msg_tx.airplay(crate::message::AirPlay::MirrorStopped {
                stream_connection_id,
            });
        }
        self.audio_task = None;
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }

    /// `GET /info`: capability exchange. The response protocol must echo the
    /// request's (`RTSP/1.0` from real devices, `HTTP/1.1` from curl); iOS
    /// first asks for just the TXT record via a plist body with a
    /// `qualifier` array (`["txtAirPlay"]`), and a plain `GET /info` gets
    /// the full plist.
    fn info(&self, request: &Request) -> Response {
        let is_plist_request = request
            .header("Content-Type")
            .is_some_and(|ct| ct.contains("apple-binary-plist"));

        if is_plist_request {
            let qualifier = plist::from_bytes::<plist::Value>(&request.body)
                .ok()
                .and_then(|v| {
                    v.as_dictionary()
                        .and_then(|d| d.get("qualifier"))
                        .and_then(|q| q.as_array())
                        .and_then(|a| a.first())
                        .and_then(|s| s.as_string())
                        .map(str::to_owned)
                });
            debug!(?qualifier, "GET /info qualifier request");

            let mut dict = plist::Dictionary::new();
            if qualifier.as_deref() == Some("txtAirPlay") {
                dict.insert(
                    "txtAirPlay".into(),
                    plist::Value::Data(self.config.airplay_txt_record()),
                );
            }
            return self.binary_plist_response(request, dict);
        }

        self.binary_plist_response(request, self.config.info_plist_dict())
    }

    /// Serialize `dict` as a binary plist response, echoing the request
    /// protocol.
    fn binary_plist_response(&self, request: &Request, dict: plist::Dictionary) -> Response {
        let mut body = Vec::new();
        match plist::to_writer_binary(&mut body, &plist::Value::Dictionary(dict)) {
            Ok(()) => Response::new(&request.protocol, 200, "OK")
                .body("application/x-apple-binary-plist", body),
            Err(err) => {
                warn!(?err, "failed to serialize binary plist response");
                Response::new(&request.protocol, 500, "Internal Server Error")
            }
        }
    }

    /// `POST /fp-setup`: FairPlay handshake; the stage is selected by body
    /// length (16 → 142-byte reply, 164 → 32-byte reply).
    fn fp_setup(&mut self, request: &Request) -> Response {
        let result = match request.body.len() {
            16 => self.fairplay.setup(&request.body).map(|r| r.to_vec()),
            164 => self.fairplay.handshake(&request.body).map(|r| r.to_vec()),
            other => {
                warn!(other, "unexpected fp-setup body length");
                return Response::new(&request.protocol, 400, "Bad Request");
            }
        };
        match result {
            Ok(reply) => {
                Response::new(&request.protocol, 200, "OK").body("application/octet-stream", reply)
            }
            Err(err) => {
                warn!(?err, "fp-setup failed");
                Response::new(&request.protocol, 400, "Bad Request")
            }
        }
    }

    /// `SET_PARAMETER`: maps the `text/parameters` `volume:` dB value onto
    /// linear volume; other parameters are accepted and ignored.
    fn set_parameter(&mut self, request: &Request) -> Response {
        let is_text = request
            .header("Content-Type")
            .is_some_and(|ct| ct.contains("text/parameters"));
        if is_text {
            let body = String::from_utf8_lossy(&request.body);
            for line in body.lines() {
                if let Some(value) = line.strip_prefix("volume:") {
                    if let Ok(db) = value.trim().parse::<f32>() {
                        let linear = crate::raop::airplay_volume_to_linear(db.into());
                        debug!(db, linear, "SET_PARAMETER volume");
                        if let Some(stream_connection_id) = self.mirror_session {
                            self.msg_tx.airplay(crate::message::AirPlay::VolumeChanged {
                                stream_connection_id,
                                volume: linear as f32,
                            });
                        }
                    } else {
                        warn!(value = value.trim(), "unparseable SET_PARAMETER volume");
                    }
                }
            }
        }
        Response::new(&request.protocol, 200, "OK")
    }

    /// `SETUP`: a binary-plist request with up to two phases, either or both of
    /// which may be present. Phase A carries `ekey`/`eiv`, the FairPlay-wrapped
    /// AES key; phase B carries the `streams` array and answers with each
    /// stream's port.
    async fn setup(&mut self, request: &Request) -> Response {
        let root: plist::Value = match plist::from_bytes(&request.body) {
            Ok(v) => v,
            Err(err) => {
                warn!(?err, "SETUP body is not a valid plist");
                return Response::new(&request.protocol, 400, "Bad Request");
            }
        };
        let Some(dict) = root.as_dictionary() else {
            warn!("SETUP plist is not a dictionary");
            return Response::new(&request.protocol, 400, "Bad Request");
        };

        if tracing::enabled!(tracing::Level::DEBUG) {
            let mut xml = Vec::new();
            if plist::to_writer_xml(&mut xml, &root).is_ok() {
                debug!(plist = %String::from_utf8_lossy(&xml), "SETUP body");
            }
        }

        let mut response = plist::Dictionary::new();

        // Phase A: session keys.
        if let (Some(ekey), Some(eiv)) = (
            dict.get("ekey").and_then(|v| v.as_data()),
            dict.get("eiv").and_then(|v| v.as_data()),
        ) {
            self.aesiv = eiv.try_into().ok();
            match self.fairplay.decrypt(ekey) {
                Ok(aeskey) => {
                    debug!("SETUP phase A: recovered AES key from ekey");
                    self.aeskey = Some(aeskey);
                }
                Err(err) => {
                    warn!(?err, "SETUP phase A: failed to decrypt ekey");
                    return Response::new(&request.protocol, 400, "Bad Request");
                }
            }
            // We run no event channel, so `eventPort` is reported as unused.
            let timing_port = self.start_ntp(dict).await;
            response.insert("eventPort".into(), 0u32.into());
            response.insert("timingPort".into(), u32::from(timing_port).into());
        }

        // Phase B: stream setup.
        if let Some(streams) = dict.get("streams").and_then(|v| v.as_array()) {
            match self.setup_streams(streams).await {
                Ok(res_streams) => {
                    response.insert("streams".into(), plist::Value::Array(res_streams));
                }
                Err(err) => {
                    warn!(?err, "SETUP phase B: stream setup failed");
                    return Response::new(&request.protocol, 400, "Bad Request");
                }
            }
        }

        let mut body = Vec::new();
        if let Err(err) = plist::to_writer_binary(&mut body, &plist::Value::Dictionary(response)) {
            warn!(?err, "failed to serialize SETUP response");
            return Response::new(&request.protocol, 500, "Internal Server Error");
        }
        Response::new(&request.protocol, 200, "OK").body("application/x-apple-binary-plist", body)
    }

    /// Start the NTP timing client against the client's `timingPort`, returning
    /// the local port to advertise (`0` if there is no timing port or no
    /// socket).
    async fn start_ntp(&mut self, dict: &plist::Dictionary) -> u16 {
        let Some(timing_rport) = dict.get("timingPort").and_then(plist_u64) else {
            debug!("SETUP without timingPort; skipping NTP timing client");
            return 0;
        };
        let socket = match tokio::net::UdpSocket::bind(("0.0.0.0", 0)).await {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "failed to bind NTP timing socket");
                return 0;
            }
        };
        let local_port = match socket.local_addr() {
            Ok(addr) => addr.port(),
            Err(err) => {
                warn!(?err, "failed to read NTP timing socket port");
                return 0;
            }
        };
        let timing_addr = ntp::timing_addr(self.peer_ip, timing_rport as u16);
        debug!(%timing_addr, local_port, "starting NTP timing client");
        let task = tokio::spawn(ntp::run(
            socket,
            timing_addr,
            self.ntp_clock.clone(),
            self.config.device_name.clone(),
        ));
        self.tasks.push(task.abort_handle());
        local_port
    }

    /// Process a phase-B `SETUP` `streams` array, returning the response
    /// descriptors in request order. Video (`type 110`) is set up before audio
    /// (`type 96`) so audio can attach to its session, and `MirrorStarted` is
    /// announced only once every stream is registered - otherwise the source
    /// Bin could start before the audio channel exists and miss its pad.
    async fn setup_streams(&mut self, streams: &[plist::Value]) -> Result<Vec<plist::Value>> {
        let dicts: Vec<&plist::Dictionary> =
            streams.iter().filter_map(|s| s.as_dictionary()).collect();
        let stream_type = |s: &plist::Dictionary| s.get("type").and_then(plist_u64);

        // Pass 1: establish the video session (claims the single mirror slot).
        let mut video_data_port: Option<u16> = None;
        for s in &dicts {
            if stream_type(s) == Some(110) {
                video_data_port = Some(self.setup_video_stream(s).await?);
            }
        }

        // Pass 2: respond in request order, wiring audio to pass 1's session.
        let mut out = Vec::new();
        for s in &dicts {
            match stream_type(s) {
                Some(110) => {
                    let data_port = video_data_port.context("video stream setup missing")?;
                    let mut res = plist::Dictionary::new();
                    res.insert("type".into(), 110u32.into());
                    res.insert("dataPort".into(), u32::from(data_port).into());
                    out.push(plist::Value::Dictionary(res));
                }
                Some(96) => {
                    let (data_port, control_port) = self.setup_audio_stream(s).await?;
                    let mut res = plist::Dictionary::new();
                    res.insert("type".into(), 96u32.into());
                    res.insert("dataPort".into(), u32::from(data_port).into());
                    res.insert("controlPort".into(), u32::from(control_port).into());
                    out.push(plist::Value::Dictionary(res));
                }
                other => debug!(?other, "ignoring unsupported SETUP stream type"),
            }
        }

        // A later audio-only SETUP must NOT re-announce, or the app re-sets the
        // player URI and playbin builds a second source element whose `prepare`
        // fails on the already-claimed channels.
        if video_data_port.is_some()
            && let Some(stream_connection_id) = self.mirror_session
        {
            self.msg_tx.airplay(crate::message::AirPlay::MirrorStarted {
                stream_connection_id,
            });
        }
        Ok(out)
    }

    /// Set up the mirror video stream (`type 110`), returning the data port to
    /// advertise.
    async fn setup_video_stream(&mut self, s: &plist::Dictionary) -> Result<u16> {
        let stream_connection_id = s
            .get("streamConnectionID")
            .and_then(plist_u64)
            .context("mirror stream missing streamConnectionID")?;
        let aeskey = self
            .aeskey
            .context("mirror SETUP before keys were established")?;

        // Registering claims the single mirror slot and creates the access-unit
        // channel the `airplaysrc` Bin takes when the app sets the player URI.
        let au_tx = self
            .airplay_context
            .try_register(stream_connection_id)
            .context("a mirror session is already active")?;

        let cipher = MirrorCipher::new(&aeskey, stream_connection_id);
        let listener = TcpListener::bind(("0.0.0.0", 0))
            .await
            .context("failed to bind mirror data port")?;
        let data_port = listener.local_addr()?.port();
        debug!(stream_connection_id, data_port, "mirror stream setup");

        let task = tokio::spawn(stream::run(
            listener,
            cipher,
            au_tx,
            stream_connection_id,
            self.msg_tx.clone(),
            self.ntp_clock.clone(),
            self.config.device_name.clone(),
        ));
        self.tasks.push(task.abort_handle());
        // Recorded against the session so the app can force-end the mirror.
        self.airplay_context
            .add_abort(stream_connection_id, task.abort_handle());
        self.mirror_session = Some(stream_connection_id);
        Ok(data_port)
    }

    /// Set up the mirror audio stream (`type 96`, AAC-ELD over RTP/UDP),
    /// returning `(data_port, control_port)`. Audio is skipped - ports still
    /// bound and reported - without an AAC decoder or a session.
    async fn setup_audio_stream(&mut self, s: &plist::Dictionary) -> Result<(u16, u16)> {
        let ct = s.get("ct").and_then(plist_u64).unwrap_or(0) as u8;
        let aeskey = self
            .aeskey
            .context("audio SETUP before keys were established")?;
        let aesiv = self.aesiv.context("audio SETUP missing eiv")?;

        // The control port carries sync/resend packets: bound and reported to
        // satisfy the client, but not yet read.
        let data_sock = tokio::net::UdpSocket::bind(("0.0.0.0", 0))
            .await
            .context("failed to bind audio data port")?;
        let control_sock = tokio::net::UdpSocket::bind(("0.0.0.0", 0))
            .await
            .context("failed to bind audio control port")?;
        let data_port = data_sock.local_addr()?.port();
        let control_port = control_sock.local_addr()?.port();
        debug!(ct, data_port, control_port, "mirror audio stream setup");

        let Some(stream_connection_id) = self.mirror_session else {
            warn!("audio SETUP with no mirror session; dropping audio");
            return Ok((data_port, control_port));
        };
        if gst::ElementFactory::find("avdec_aac").is_none() {
            warn!("avdec_aac missing (install the GStreamer libav plugin); mirror audio disabled");
            return Ok((data_port, control_port));
        }
        let Some(frame_tx) = self.airplay_context.audio_sender(stream_connection_id) else {
            warn!(stream_connection_id, "no audio channel for session");
            return Ok((data_port, control_port));
        };

        let task = tokio::spawn(audio::run(
            data_sock,
            aeskey,
            aesiv,
            ct,
            frame_tx,
            self.config.device_name.clone(),
        ));
        self.tasks.push(task.abort_handle());
        self.airplay_context
            .add_abort(stream_connection_id, task.abort_handle());
        self.audio_task = Some(task.abort_handle());
        Ok((data_port, control_port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Configuration {
        Configuration {
            device_name: "FCast-test".to_owned(),
            hw_addr: [0x01, 0x23, 0x45, 0x67, 0x89, 0xab],
            pk: "deadbeef".to_owned(),
        }
    }

    #[test]
    fn device_id_is_lowercase_mac() {
        assert_eq!(test_config().device_id(), "01:23:45:67:89:ab");
    }

    fn teardown_body(types: &[u64]) -> Vec<u8> {
        use plist::Value;
        let streams = types
            .iter()
            .map(|&t| {
                let mut d = plist::Dictionary::new();
                d.insert("type".into(), Value::Integer((t as i64).into()));
                Value::Dictionary(d)
            })
            .collect();
        let mut root = plist::Dictionary::new();
        root.insert("streams".into(), Value::Array(streams));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &Value::Dictionary(root)).unwrap();
        body
    }

    #[test]
    fn teardown_stream_types_classifies_body() {
        assert_eq!(teardown_stream_types(&teardown_body(&[96])), (true, false));
        assert_eq!(teardown_stream_types(&teardown_body(&[110])), (false, true));
        assert_eq!(
            teardown_stream_types(&teardown_body(&[96, 110])),
            (true, true)
        );
        assert_eq!(teardown_stream_types(&teardown_body(&[])), (false, false));
        assert_eq!(teardown_stream_types(b""), (false, false));
    }

    #[test]
    fn info_plist_parses_with_expected_keys() {
        let dict = test_config().info_plist_dict();

        assert_eq!(
            dict.get("deviceID").and_then(|v| v.as_string()),
            Some("01:23:45:67:89:ab")
        );
        assert_eq!(
            dict.get("macAddress").and_then(|v| v.as_string()),
            Some("01:23:45:67:89:ab")
        );
        assert_eq!(dict.get("model").and_then(|v| v.as_string()), Some(MODEL));
        assert_eq!(
            dict.get("sourceVersion").and_then(|v| v.as_string()),
            Some(SOURCE_VERSION)
        );
        assert_eq!(
            dict.get("features").and_then(|v| v.as_unsigned_integer()),
            Some(FEATURES)
        );
        assert_eq!(
            dict.get("pk").and_then(|v| v.as_data()),
            Some([0xde, 0xad, 0xbe, 0xef].as_slice())
        );
        let display = dict
            .get("displays")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_dictionary())
            .expect("displays[0]");
        assert_eq!(
            display.get("width").and_then(|v| v.as_unsigned_integer()),
            Some(DISPLAY_WIDTH)
        );
    }

    #[test]
    fn airplay_txt_record_is_dns_txt_encoded() {
        let txt = test_config().airplay_txt_record();
        let mut i = 0;
        let mut entries = Vec::new();
        while i < txt.len() {
            let len = txt[i] as usize;
            entries.push(String::from_utf8(txt[i + 1..i + 1 + len].to_vec()).unwrap());
            i += 1 + len;
        }
        assert_eq!(i, txt.len(), "entries must tile the buffer exactly");
        assert!(entries.iter().any(|e| e == "deviceid=01:23:45:67:89:ab"));
        assert!(entries.iter().any(|e| e == "model=AppleTV3,2"));
    }

    #[tokio::test]
    async fn serves_info_over_tcp() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let msg_tx = MessageSender::new(tx);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut handler = Handler {
                config: test_config(),
                connection: Connection::new(stream),
                fairplay: FairPlay::new(),
                aeskey: None,
                aesiv: None,
                msg_tx,
                airplay_context: AirPlayContext::new(),
                peer_ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                mirror_session: None,
                tasks: Vec::new(),
                audio_task: None,
                ntp_clock: ntp::NtpClock::new(),
            };
            let _ = handler.run().await;
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /info HTTP/1.1\r\nCSeq: 7\r\n\r\n")
            .await
            .unwrap();

        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let (status_line, body) = loop {
            let n = client.read(&mut tmp).await.unwrap();
            assert_ne!(n, 0, "connection closed before full response");
            buf.extend_from_slice(&tmp[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(idx) = text.find("\r\n\r\n") {
                let head = &text[..idx];
                let content_length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap();
                let body_start = idx + 4;
                if buf.len() >= body_start + content_length {
                    let status = head.lines().next().unwrap().to_owned();
                    break (
                        status,
                        buf[body_start..body_start + content_length].to_vec(),
                    );
                }
            }
        };

        assert!(
            status_line.starts_with("HTTP/1.1 200"),
            "got: {status_line}"
        );
        let value: plist::Value = plist::from_bytes(&body).expect("valid plist body");
        assert_eq!(
            value
                .as_dictionary()
                .and_then(|d| d.get("model"))
                .and_then(|v| v.as_string()),
            Some(MODEL)
        );
    }
}
