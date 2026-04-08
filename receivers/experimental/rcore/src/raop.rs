// Loosely based on [Airguitar](https://github.com/MSNexploder/airguitar)
// TODO: do propper synchronization
// TODO: notify sender about missing packets

use aes::{
    Aes128,
    cipher::{
        BlockDecryptMut, InnerIvInit, KeyInit, block_padding::ZeroPadding,
        generic_array::GenericArray,
    },
};
use anyhow::{Result, anyhow};
use base64::Engine;
use bytes::{Buf, BytesMut};
use gst::{glib, prelude::*};
use mdns_sd::ServiceInfo;
use nom::{
    IResult,
    branch::permutation,
    bytes::complete::tag_no_case,
    character::complete::{char, digit1, space0},
    combinator::{map_res, opt},
    sequence::{delimited, preceded, terminated, tuple},
};
use rsa::{RsaPrivateKey, pkcs1::DecodeRsaPrivateKey};
use rtsp_types::{
    HeaderName, Message, Method, ParseError, Request, Response, ResponseBuilder, StatusCode,
    Version,
    headers::{
        self, RtpLowerTransport, RtpProfile, RtpTransport, RtpTransportParameters, Transport,
        TransportMode, Transports,
    },
};
use sha1::Sha1;
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    net::{IpAddr, SocketAddr},
    str,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufWriter},
    net::{TcpStream, UdpSocket},
    sync::{
        broadcast::{self, Sender},
        mpsc, oneshot,
    },
};
use tracing::{debug, error, instrument, trace, warn};

use crate::MessageSender;

lazy_static::lazy_static! {
    static ref PRIVATE_KEY: RsaPrivateKey = RsaPrivateKey::from_pkcs1_pem(
        r#"-----BEGIN RSA PRIVATE KEY-----
MIIEpQIBAAKCAQEA59dE8qLieItsH1WgjrcFRKj6eUWqi+bGLOX1HL3U3GhC/j0Q
g90u3sG/1CUtwC5vOYvfDmFI6oSFXi5ELabWJmT2dKHzBJKa3k9ok+8t9ucRqMd6
DZHJ2YCCLlDRKSKv6kDqnw4UwPdpOMXziC/AMj3Z/lUVX1G7WSHCAWKf1zNS1eLv
qr+boEjXuBOitnZ/bDzPHrTOZz0Dew0uowxf/+sG+NCK3eQJVxqcaJ/vEHKIVd2M
+5qL71yJQ+87X6oV3eaYvt3zWZYD6z5vYTcrtij2VZ9Zmni/UAaHqn9JdsBWLUEp
VviYnhimNVvYFZeCXg/IdTQ+x4IRdiXNv5hEewIDAQABAoIBAQDl8Axy9XfWBLmk
zkEiqoSwF0PsmVrPzH9KsnwLGH+QZlvjWd8SWYGN7u1507HvhF5N3drJoVU3O14n
DY4TFQAaLlJ9VM35AApXaLyY1ERrN7u9ALKd2LUwYhM7Km539O4yUFYikE2nIPsc
EsA5ltpxOgUGCY7b7ez5NtD6nL1ZKauw7aNXmVAvmJTcuPxWmoktF3gDJKK2wxZu
NGcJE0uFQEG4Z3BrWP7yoNuSK3dii2jmlpPHr0O/KnPQtzI3eguhe0TwUem/eYSd
yzMyVx/YpwkzwtYL3sR5k0o9rKQLtvLzfAqdBxBurcizaaA/L0HIgAmOit1GJA2s
aMxTVPNhAoGBAPfgv1oeZxgxmotiCcMXFEQEWflzhWYTsXrhUIuz5jFua39GLS99
ZEErhLdrwj8rDDViRVJ5skOp9zFvlYAHs0xh92ji1E7V/ysnKBfsMrPkk5KSKPrn
jndMoPdevWnVkgJ5jxFuNgxkOLMuG9i53B4yMvDTCRiIPMQ++N2iLDaRAoGBAO9v
//mU8eVkQaoANf0ZoMjW8CN4xwWA2cSEIHkd9AfFkftuv8oyLDCG3ZAf0vrhrrtk
rfa7ef+AUb69DNggq4mHQAYBp7L+k5DKzJrKuO0r+R0YbY9pZD1+/g9dVt91d6LQ
NepUE/yY2PP5CNoFmjedpLHMOPFdVgqDzDFxU8hLAoGBANDrr7xAJbqBjHVwIzQ4
To9pb4BNeqDndk5Qe7fT3+/H1njGaC0/rXE0Qb7q5ySgnsCb3DvAcJyRM9SJ7OKl
Gt0FMSdJD5KG0XPIpAVNwgpXXH5MDJg09KHeh0kXo+QA6viFBi21y340NonnEfdf
54PX4ZGS/Xac1UK+pLkBB+zRAoGAf0AY3H3qKS2lMEI4bzEFoHeK3G895pDaK3TF
BVmD7fV0Zhov17fegFPMwOII8MisYm9ZfT2Z0s5Ro3s5rkt+nvLAdfC/PYPKzTLa
lpGSwomSNYJcB9HNMlmhkGzc1JnLYT4iyUyx6pcZBmCd8bD0iwY/FzcgNDaUmbX9
+XDvRA0CgYEAkE7pIPlE71qvfJQgoA9em0gILAuE4Pu13aKiJnfft7hIjbK+5kyb
3TysZvoyDnb3HOKvInK7vXbKuU4ISgxB2bB3HcYzQMGsz1qJ2gG0N5hvJpzwwhbh
XqFKA4zaaSrw622wDniAK5MlIE0tIAKKP4yxNGjoD2QYjhBGuhvkWKY=
-----END RSA PRIVATE KEY-----"#).unwrap();
    static ref APPLE_CHALLENGE: HeaderName = HeaderName::from_static_str("Apple-Challenge").unwrap();
    static ref APPLE_RESPONSE: HeaderName = HeaderName::from_static_str("Apple-Response").unwrap();
    static ref AUDIO_LATENCY: HeaderName = HeaderName::from_static_str("Audio-Latency").unwrap();
}

const TXT_VERSION: &str = "txtvers";
const TXT_AUDIO_CODECS: &str = "cn";
const TXT_AUDIO_CHANNELS: &str = "ch";
const TXT_ENCRYPTION_TYPE: &str = "et";
const TXT_DEVICE_MODEL: &str = "am";
const TXT_SERVER_VERSION: &str = "vs";
const TXT_SUPPORTED_TRANSPORT: &str = "tp";
const TXT_SUPPORTED_METADATA: &str = "md";
const TXT_AUDIO_SAMPLE_SIZE: &str = "ss";
const TXT_AUDIO_SAMPLING_RATE: &str = "sr";
const TXT_PASSWORD_REQUIRED: &str = "pw";

const RTP_SYNC_PAYLOAD_TYPE: u8 = 84;
const RTP_RESENT_DATA_PAYLOAD_TYPE: u8 = 86;
const RTP_TIMING_PAYLOAD_TYPE: u8 = 83;

const SAMPLING_RATE: u64 = 44100;
const LATENCY_IN_SAMPLES: u64 = SAMPLING_RATE;

// NTP epoch - UNIX epoch
const NTP_OFFSET: Duration = Duration::from_secs(2208988800);

mod alacdec_imp {
    use std::sync::LazyLock;

    use gst::{glib, subclass::prelude::*};
    use gst_audio::{prelude::*, subclass::prelude::*};
    use parking_lot::Mutex;

    use crate::raop::SAMPLING_RATE;

    static CAT: LazyLock<gst::DebugCategory> =
        LazyLock::new(|| gst::DebugCategory::new("fcalacdec", gst::DebugColorFlags::empty(), None));

    #[derive(Default)]
    struct State {
        decoder: Option<alac::Decoder>,
        output_buffer: Vec<i16>,
    }

    #[derive(Default)]
    pub struct FcAlacDec {
        state: Mutex<State>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FcAlacDec {
        const NAME: &'static str = "FcAlacDec";
        type Type = super::FcAlacDec;
        type ParentType = gst_audio::AudioDecoder;
    }

    impl ObjectImpl for FcAlacDec {}

    impl GstObjectImpl for FcAlacDec {}

    impl ElementImpl for FcAlacDec {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let caps = gst_audio::AudioCapsBuilder::new()
                    .format(gst_audio::AudioFormat::S16le)
                    .channels(2)
                    .rate(SAMPLING_RATE as i32)
                    .layout(gst_audio::AudioLayout::Interleaved)
                    .build();
                let src_pad_template = gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap();

                let caps = gst::Caps::builder("audio/x-alac")
                    .field("channels", 2i32)
                    .field("rate", SAMPLING_RATE as i32)
                    .build();
                let sink_pad_template = gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap();

                vec![src_pad_template, sink_pad_template]
            });

            PAD_TEMPLATES.as_ref()
        }
    }

    impl AudioDecoderImpl for FcAlacDec {
        fn set_format(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
            let s = caps.structure(0).unwrap();
            if let Ok(Some(sdp_fmtp)) = s.get_optional::<gst::Buffer>("sdp-fmtp") {
                let map = sdp_fmtp.map_readable().unwrap();
                let buf = map.as_slice();
                let fmtp = str::from_utf8(buf).unwrap();
                let stream_info = alac::StreamInfo::from_sdp_format_parameters(fmtp).unwrap();
                let max_samples = stream_info.max_samples_per_packet() as usize;
                let decoder = alac::Decoder::new(stream_info);

                {
                    let mut state = self.state.lock();
                    state.decoder = Some(decoder);
                    state.output_buffer = vec![0; max_samples];
                }

                let audio_info = gst_audio::AudioInfo::builder(
                    gst_audio::AudioFormat::S16le,
                    SAMPLING_RATE as u32,
                    2,
                )
                .build()
                .unwrap();

                let element = self.obj();
                if element.set_output_format(&audio_info).is_err() || element.negotiate().is_err() {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Error to negotiate output from based on in-caps streaminfo"
                    );
                }
            } else {
                todo!();
            }

            Ok(())
        }

        fn handle_frame(
            &self,
            buffer: Option<&gst::Buffer>,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let inbuf = match buffer {
                Some(b) => b,
                None => return Ok(gst::FlowSuccess::Ok),
            };
            let in_pts = inbuf.pts();

            let inbuf_map = inbuf.map_readable().map_err(|_| {
                gst::error!(CAT, imp = self, "Failed to map buffer as readable");
                gst::FlowError::Error
            })?;

            let mut state = self.state.lock();
            let State {
                decoder,
                output_buffer,
            } = &mut *state;
            match decoder {
                Some(decoder) => {
                    let samples = decoder
                        .decode_packet::<i16>(&inbuf_map, output_buffer)
                        .unwrap();
                    let mut buffer = gst::Buffer::with_size(samples.len() * 2).unwrap();
                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_pts(in_pts);
                        let mut map = buffer.map_writable().unwrap();
                        let data = map.as_mut_slice();
                        use byte_slice_cast::*;
                        let samples_bytes = samples.as_byte_slice();
                        data.copy_from_slice(samples_bytes);
                    }

                    self.obj().finish_frame(Some(buffer), 1)
                }
                None => {
                    todo!()
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct FcAlacDec(ObjectSubclass<alacdec_imp::FcAlacDec>) @extends gst_audio::AudioDecoder, gst::Element, gst::Object;
}

impl Default for FcAlacDec {
    fn default() -> Self {
        glib::Object::new()
    }
}

fn decode_base64(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if input.ends_with('=') {
        base64::engine::general_purpose::STANDARD.decode_vec(input, &mut out)?;
    } else {
        base64::engine::general_purpose::STANDARD_NO_PAD.decode_vec(input, &mut out)?;
    };

    Ok(out)
}

fn encode_base64(input: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(input)
}

#[allow(unused)]
#[derive(Debug, Clone)]
struct RtpInfo {
    /// Sequence number of the first packet that is a direct result of the request.
    seq: u16,
    /// RTP timestamp corresponding to the start time.
    rtptime: u32,
}

impl RtpInfo {
    fn parse(input: &str) -> IResult<&str, RtpInfo> {
        let (input, (seq, rtptime)) = permutation((
            parameter(tag_no_case("seq")),
            parameter(tag_no_case("rtptime")),
        ))(input)?;
        Ok((input, RtpInfo { seq, rtptime }))
    }
}

fn parameter<'a, O1, O2, E, F>(seq_parser: F) -> impl FnMut(&'a str) -> IResult<&'a str, O2, E>
where
    F: nom::Parser<&'a str, O1, E>,
    O2: std::str::FromStr,
    E: nom::error::ParseError<&'a str> + nom::error::FromExternalError<&'a str, <O2>::Err>,
{
    terminated(
        preceded(
            tuple((trim(seq_parser), char('='))),
            trim(map_res(digit1, |s: &str| s.parse::<O2>())),
        ),
        opt(char(';')),
    )
}

fn trim<I, O, E: nom::error::ParseError<I>, F>(parser: F) -> impl FnMut(I) -> IResult<I, O, E>
where
    F: nom::Parser<I, O, E>,
    I: nom::InputTakeAtPosition,
    <I as nom::InputTakeAtPosition>::Item: nom::AsChar + Clone,
{
    delimited(space0, parser, space0)
}

#[derive(Debug)]
struct Connection {
    stream: BufWriter<TcpStream>,
    buffer: BytesMut,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}

impl Connection {
    pub fn new(socket: TcpStream) -> Result<Connection> {
        let local_addr = socket.local_addr()?;
        let peer_addr = socket.peer_addr()?;

        Ok(Connection {
            stream: BufWriter::new(socket),
            buffer: BytesMut::with_capacity(1024),

            local_addr,
            peer_addr,
        })
    }

    #[instrument(skip(self))]
    pub async fn read_message(&mut self) -> Result<Option<Message<Vec<u8>>>> {
        loop {
            if let Some(message) = self.parse_message()? {
                return Ok(Some(message));
            }

            if 0 == self.stream.read_buf(&mut self.buffer).await? {
                if self.buffer.is_empty() {
                    return Ok(None);
                } else {
                    return Err(anyhow::anyhow!("connection reset by peer"));
                }
            }
        }
    }

    #[instrument(skip(self))]
    pub async fn write_response<B: AsRef<[u8]> + Debug>(
        &mut self,
        response: &Response<B>,
    ) -> Result<()> {
        let mut buffer = Vec::new();
        response.write(&mut buffer)?;
        self.stream.write_all(&buffer).await?;

        self.stream.flush().await?;
        Ok(())
    }

    fn parse_message(&mut self) -> Result<Option<Message<Vec<u8>>>> {
        match Message::parse(&self.buffer[..]) {
            Ok((message, consumed)) => {
                self.buffer.advance(consumed);
                Ok(Some(message))
            }
            Err(ParseError::Incomplete(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

const NO_SSRC_HEADER_SIZE: usize = 8;

#[allow(unused)]
struct NoSsrcRtpHeader {
    payload_type: u8,
    sequence_number: u16,
    timestamp: u32,
}

impl NoSsrcRtpHeader {
    fn parse(buf: &[u8]) -> Self {
        assert!(buf.len() >= NO_SSRC_HEADER_SIZE);

        let payload_type = buf[1] & 0b01111111;
        let sequence_number = u16::from_be_bytes([buf[2], buf[3]]);
        let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

        Self {
            payload_type,
            sequence_number,
            timestamp,
        }
    }
}

#[allow(unused)]
#[derive(Debug)]
struct ControlReceiver {
    player_tx: mpsc::Sender<Command>,
    socket: Arc<UdpSocket>,
    shutdown: Shutdown,
}

impl ControlReceiver {
    #[instrument(name = "control_receiver", skip(self))]
    async fn run(&mut self) -> Result<()> {
        let mut buf = [0; 4 * 1024];
        while !self.shutdown.is_shutdown() {
            let length = tokio::select! {
                result = self.socket.recv_from(&mut buf) => {
                  match result {
                      Ok((length, _)) => {
                        if length == 0 {
                          return Ok(());
                        } else {
                          length
                        }
                      },
                      Err(e) => {
                        return Err(e.into());
                      },
                  }
                },
                _ = self.shutdown.recv() => {
                    return Ok(());
                }
            };

            if length < NO_SSRC_HEADER_SIZE + 8 + 4 {
                anyhow::bail!("Received invalid packet");
            }

            let header = NoSsrcRtpHeader::parse(&buf[..length]);

            match header.payload_type {
                RTP_SYNC_PAYLOAD_TYPE => {
                    // let time = duration_from_ntp(&buf[8..16]);
                    // let timestamp = u32::from_be_bytes(buf[16..20].try_into().unwrap());
                    // debug!("{:?} - {:?}-{:?}", header.sequence_number, time, timestamp);
                }
                RTP_RESENT_DATA_PAYLOAD_TYPE => (),
                payload_type => trace!(payload_type, "received packet with unknown payload type"),
            }
        }

        Ok(())
    }
}

fn duration_from_ntp(buf: &[u8]) -> Duration {
    assert_eq!(buf.len(), 8);

    macro_rules! u32_from_be {
        ($buf:expr, $start_idx:expr) => {
            u32::from_be_bytes([
                $buf[$start_idx],
                $buf[$start_idx + 1],
                $buf[$start_idx + 2],
                $buf[$start_idx + 3],
            ])
        };
    }

    Duration::new(u32_from_be!(buf, 0) as u64, u32_from_be!(buf, 4))
}

fn ntp_duration_now() -> Duration {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        + NTP_OFFSET
}

fn ntp_time_now() -> [u8; 8] {
    let now = ntp_duration_now();
    let secs = (now.as_secs() as u32).to_be_bytes();
    let nanos = now.subsec_nanos().to_be_bytes();

    [
        secs[0], secs[1], secs[2], secs[3], nanos[0], nanos[1], nanos[2], nanos[3],
    ]
}

#[derive(Debug)]
struct ServerReceiver {
    player_tx: mpsc::Sender<Command>,
    socket: Arc<UdpSocket>,
    shutdown: Shutdown,
}

impl ServerReceiver {
    #[instrument(skip(self))]
    async fn run(&mut self) -> Result<()> {
        let mut buf = [0; 4 * 1024];
        while !self.shutdown.is_shutdown() {
            let length = tokio::select! {
                result = self.socket.recv_from(&mut buf) => {
                  match result {
                      Ok((length, _)) => {
                        if length == 0 {
                          return Ok(());
                        } else {
                          length
                        }
                      },
                      Err(e) => {
                        return Err(e.into());
                      },
                  }
                },
                _ = self.shutdown.recv() => {
                    return Ok(());
                }
            };

            match rtp_types::RtpPacket::parse(&buf[..length]) {
                Ok(packet) => {
                    let seq = packet.sequence_number();
                    let payload = packet.payload().to_vec();

                    self.player_tx
                        .send(Command::PutPacket {
                            seq,
                            packet: payload,
                            timestamp: packet.timestamp(),
                        })
                        .await?
                }
                Err(err) => {
                    debug!(?err);
                }
            }
        }

        Ok(())
    }
}

#[allow(unused)]
#[derive(Debug)]
struct TimingManager {
    player_tx: mpsc::Sender<Command>,
    socket: Arc<UdpSocket>,
    shutdown: Shutdown,
}

impl TimingManager {
    #[instrument(name = "timing_manager", skip(self))]
    async fn run(&mut self) -> Result<()> {
        let mut recv_buf = [0; 32];
        let mut send_interval = tokio::time::interval(Duration::from_secs(3));

        // let mut base_local = None;
        // let mut base_remote = None;

        while !self.shutdown.is_shutdown() {
            tokio::select! {
                result = self.socket.recv_from(&mut recv_buf) => {
                    let (length, _) = result?;
                    if length == 0 {
                        return Ok(());
                    }

                    if length != 32 {
                        continue;
                    }

                    let header = NoSsrcRtpHeader::parse(&recv_buf[..32]);
                    if header.payload_type == RTP_TIMING_PAYLOAD_TYPE {
                        let _origin = duration_from_ntp(&recv_buf[8..16]);
                        let _receive = duration_from_ntp(&recv_buf[16..24]);
                        let _transmit = duration_from_ntp(&recv_buf[24..32]);
                        let _now = ntp_duration_now();

                        // let (_base_local, _base_remote) = {
                        //     let l = base_local.unwrap_or(origin);
                        //     base_local = Some(l);
                        //     let r = base_remote.unwrap_or(receive);
                        //     base_remote = Some(r);
                        //     (l, r)
                        // };

                        // debug!(?origin, ?receive, ?transmit, diff = ?(now - origin));
                    }
                }
                _ = send_interval.tick() => {
                    let now = ntp_time_now();

                    let message = [
                        0x80, 0xd2, 0x0, 0x07, 0x0, 0x0, 0x0, 0x0,
                        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                        // 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
                        now[0], now[1], now[2], now[3], now[4], now[5], now[6], now[7],
                    ];

                  let _ = self.socket.send(&message).await;
                }
                _ = self.shutdown.recv() => {
                    return Ok(());
                }
            }
        }

        Ok(())
    }
}

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

#[derive(Debug)]
struct Encryption {
    aesiv: Vec<u8>,
    aeskey: Vec<u8>,
}

#[allow(unused)]
#[derive(Debug)]
struct Announce {
    fmtp: String,
    minimum_latency: u32,
    maximum_latency: u32,
    encryption: Option<Encryption>,
}

#[derive(Debug)]
struct Setup {
    ip: IpAddr,
    control_port: u16,
    timing_port: u16,
}

#[derive(Debug)]
struct SetupResponse {
    control_port: u16,
    timing_port: u16,
    server_port: u16,
}

#[derive(Debug)]
struct GetParameterResponse {
    volume: f64,
}

#[derive(Debug)]
enum Command {
    // RTSP
    Announce {
        payload: Announce,
        resp: oneshot::Sender<Result<()>>,
    },
    Setup {
        payload: Setup,
        resp: oneshot::Sender<Result<SetupResponse>>,
    },
    Record {
        resp: oneshot::Sender<Result<()>>,
    },
    Teardown {
        resp: oneshot::Sender<Result<()>>,
    },
    SetParameter {
        volume: f64,
        resp: oneshot::Sender<Result<()>>,
    },
    GetParameter {
        resp: oneshot::Sender<GetParameterResponse>,
    },
    Flush {
        payload: RtpInfo,
        resp: oneshot::Sender<Result<()>>,
    },
    SetProgress {
        start: u64,
        curr: u64,
        end: u64,
    },

    // Internal
    PutPacket {
        seq: u16,
        packet: Vec<u8>,
        timestamp: u32,
    },
}

struct Player {
    player_tx: mpsc::Sender<Command>,
    player_rx: mpsc::Receiver<Command>,
    shutdown: Shutdown,
    _shutdown_complete: mpsc::Sender<()>,
    msg_tx: MessageSender,
}

impl Player {
    async fn run(&mut self) -> Result<()> {
        let mut _notify_shutdown: Option<Sender<()>> = None;
        let mut encryption: Option<Encryption> = None;
        let mut cipher: Option<Aes128> = None;
        // let control_tx: Option<mpsc::Sender<ControlSenderCommand>> = None;
        let mut current_volume = 1.0;
        let mut time_start = 0;
        let mut position = 0;
        let mut duration = 0;
        let mut packet_tx = None;

        let pipeline = gst::Pipeline::new();

        let appsrc = gst_app::AppSrc::builder()
            .stream_type(gst_app::AppStreamType::Stream)
            .is_live(true)
            .format(gst::Format::Time)
            .caps(
                &gst::Caps::builder("audio/x-alac")
                    .field("channels", 2i32)
                    .field("rate", SAMPLING_RATE as i32)
                    .field("stream-format", "raw")
                    .build(),
            )
            .build();
        let queue = gst::ElementFactory::make("queue").build()?;
        let alacdec = FcAlacDec::default();
        let convert = gst::ElementFactory::make("audioconvert").build()?;
        let resample = gst::ElementFactory::make("audioresample").build()?;
        let volume_elem = gst::ElementFactory::make("volume").build()?;
        let sink = gst::ElementFactory::make("autoaudiosink").build()?;

        let elems = [
            appsrc.upcast_ref(),
            &queue,
            alacdec.upcast_ref(),
            &convert,
            &resample,
            &volume_elem,
            &sink,
        ];
        pipeline.add_many(elems)?;
        gst::Element::link_many(elems)?;

        fn send_progress_update(msg_tx: &MessageSender, curr: u64, end: u64) {
            let position_sec = curr / SAMPLING_RATE;
            let duration_sec = end / SAMPLING_RATE;
            msg_tx.raop(crate::Raop::ProgressUpdate {
                position_sec,
                duration_sec,
            });
        }

        while !self.shutdown.is_shutdown() {
            let maybe_request = tokio::select! {
                res = self.player_rx.recv() => {
                  res
                },
                _ = self.shutdown.recv() => return Ok(()),
            };

            let request = match maybe_request {
                Some(request) => request,
                None => return Ok(()),
            };

            match request {
                Command::Announce { payload, resp } => {
                    debug!(?payload, "announce");

                    encryption = payload.encryption;
                    if let Some(ref encryption) = encryption {
                        let key = GenericArray::from_slice(&encryption.aeskey);
                        cipher = Some(Aes128::new(key));
                    }

                    appsrc.set_caps(Some(
                        &gst::Caps::builder("audio/x-alac")
                            .field("channels", 2i32)
                            .field("rate", SAMPLING_RATE as i32)
                            .field("stream-format", "raw")
                            .field(
                                "sdp-fmtp",
                                gst::Buffer::from_slice(payload.fmtp.as_bytes().to_vec()),
                            )
                            .build(),
                    ));

                    let _ = resp.send(Ok(()));
                }
                Command::Setup { payload, resp } => {
                    let control_sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
                    let timing_sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
                    let server_sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

                    let control_addr = SocketAddr::new(payload.ip, payload.control_port);
                    let timing_addr = SocketAddr::new(payload.ip, payload.timing_port);

                    control_sock.connect(control_addr).await?;
                    timing_sock.connect(timing_addr).await?;

                    let control_port = control_sock.local_addr()?.port();
                    let timing_port = timing_sock.local_addr()?.port();
                    let s_port = server_sock.local_addr()?.port();

                    let (notify_shutdown_sender, _) = broadcast::channel(1);
                    _notify_shutdown = Some(notify_shutdown_sender.clone());

                    let mut timing_server = TimingManager {
                        socket: timing_sock.clone(),
                        player_tx: self.player_tx.clone(),
                        shutdown: Shutdown::new(notify_shutdown_sender.subscribe()),
                    };

                    // TODO:
                    // let (control_server_tx, control_server_rx) = mpsc::channel(4);
                    // let mut control_sender = ControlSender {
                    //     control_server_rx,
                    //     socket: c_sock.clone(),
                    //     shutdown: Shutdown::new(notify_shutdown_sender.subscribe()),
                    // };

                    // control_tx = Some(control_server_tx);

                    let mut control_receiver = ControlReceiver {
                        socket: control_sock.clone(),
                        player_tx: self.player_tx.clone(),
                        shutdown: Shutdown::new(notify_shutdown_sender.subscribe()),
                    };

                    let mut server_receiver = ServerReceiver {
                        socket: server_sock.clone(),
                        player_tx: self.player_tx.clone(),
                        shutdown: Shutdown::new(notify_shutdown_sender.subscribe()),
                    };

                    tokio::spawn(async move {
                        if let Err(err) = timing_server.run().await {
                            error!(cause = ?err, "connection error");
                        }
                    });

                    // TODO:
                    // tokio::spawn(async move {
                    //     // Process the connection. If an error is encountered, log it.
                    //     if let Err(err) = control_sender.run().await {
                    //         error!(cause = ?err, "connection error");
                    //     }
                    // });

                    tokio::spawn(async move {
                        if let Err(err) = control_receiver.run().await {
                            error!(cause = ?err, "connection error");
                        }
                    });

                    tokio::spawn(async move {
                        if let Err(err) = server_receiver.run().await {
                            error!(cause = ?err, "connection error");
                        }
                    });

                    let _ = resp.send(Ok(SetupResponse {
                        control_port,
                        timing_port,
                        server_port: s_port,
                    }));
                }
                Command::Record { resp } => {
                    tracing::debug!("Record");

                    let mut rtp_base_time = None;
                    let (new_samples_tx, samples_rx) =
                        std::sync::mpsc::sync_channel::<(u32, Vec<u8>)>(64);
                    appsrc.set_callbacks(
                        gst_app::AppSrcCallbacks::builder()
                            .need_data(move |appsrc, _| {
                                let Ok((timestamp, packet)) = samples_rx.recv() else {
                                    let _ = appsrc.end_of_stream();
                                    return;
                                };

                                let rtp_base_time = if let Some(t) = rtp_base_time {
                                    t
                                } else {
                                    rtp_base_time = Some(timestamp);
                                    timestamp
                                };

                                let rtp_time = timestamp - rtp_base_time;
                                let real_rtp_time = gst::ClockTime::from_seconds_f64(
                                    rtp_time as f64 / SAMPLING_RATE as f64,
                                );

                                let pts = real_rtp_time
                                    + gst::ClockTime::from_seconds_f64(
                                        LATENCY_IN_SAMPLES as f64 / SAMPLING_RATE as f64,
                                    );

                                let mut buffer = gst::Buffer::with_size(packet.len()).unwrap();
                                {
                                    let buffer = buffer.get_mut().unwrap();
                                    buffer.set_pts(pts);

                                    let mut map = buffer.map_writable().unwrap();
                                    let data = map.as_mut_slice();
                                    data.copy_from_slice(&packet);
                                }
                                appsrc.push_buffer(buffer).unwrap();
                            })
                            .build(),
                    );
                    packet_tx = Some(new_samples_tx);

                    pipeline.set_state(gst::State::Playing)?;

                    let _ = resp.send(Ok(()));
                }
                Command::Teardown { resp } => {
                    _notify_shutdown = None;
                    encryption = None;
                    cipher = None;
                    let _ = packet_tx.take();
                    pipeline.set_state(gst::State::Null)?;
                    let _ = resp.send(Ok(()));
                }
                Command::SetParameter { volume, resp } => {
                    current_volume = volume;

                    // https://openairplay.github.io/airplay-spec/audio/volume_control.html
                    let percentage = if volume < -30.0 {
                        0.0
                    } else {
                        1.0 - (volume / -30.0)
                    };

                    volume_elem.set_property("volume", percentage.clamp(0.0, 10.0));
                    let _ = resp.send(Ok(()));
                }
                Command::GetParameter { resp } => {
                    let _ = resp.send(GetParameterResponse {
                        volume: current_volume,
                    });
                }
                Command::Flush { payload, resp } => {
                    debug!(?payload, "Flushing");
                    let _ = resp.send(Ok(()));
                }
                Command::SetProgress { start, curr, end } => {
                    send_progress_update(&self.msg_tx, curr - start, end - start);
                    time_start = start;
                    position = curr;
                    duration = end;
                }
                // TODO: can the decryption be optimized?
                Command::PutPacket {
                    seq: _seq,
                    packet,
                    timestamp,
                } => match (&encryption, &cipher) {
                    (Some(enc), Some(cipher)) => {
                        fn is_no_data_packet(packet: &[u8]) -> bool {
                            packet.len() == 16 && packet[12..16] == [0x00, 0x68, 0x34, 0x00]
                        }

                        let len = packet.len();
                        if packet.len() <= 12 || is_no_data_packet(&packet) {
                            continue;
                        }

                        let iv = GenericArray::from_slice(&enc.aesiv);
                        let mut buffer = packet.clone();
                        buffer.extend_from_slice(&[0; 16]);
                        let aeslen = len & !0xf;

                        let buffer_end = (16 * (len / 16)) + 16;
                        let decryptor = Aes128CbcDec::inner_iv_init(cipher.clone(), iv);
                        let mut result = decryptor
                            .decrypt_padded_vec_mut::<ZeroPadding>(&buffer[..buffer_end])
                            .unwrap();

                        result[aeslen..len].copy_from_slice(&packet[aeslen..len]);

                        if let Some(tx) = packet_tx.as_ref() {
                            tx.send((timestamp, result))?;
                        }

                        let diff = (timestamp as u64).saturating_sub(position);
                        if duration > 0 && diff >= SAMPLING_RATE {
                            position += diff;
                            send_progress_update(
                                &self.msg_tx,
                                position - time_start,
                                duration - time_start,
                            );
                        }
                    }
                    _ => {
                        warn!("Cannot decrypt packet because crypto state is missing");
                    }
                },
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
struct Shutdown {
    shutdown: bool,
    notify: broadcast::Receiver<()>,
}

impl Shutdown {
    fn new(notify: broadcast::Receiver<()>) -> Shutdown {
        Shutdown {
            shutdown: false,
            notify,
        }
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    async fn recv(&mut self) {
        if self.shutdown {
            return;
        }

        let _ = self.notify.recv().await;

        self.shutdown = true;
    }
}

#[derive(Debug)]
struct Handler {
    config: Arc<Configuration>,
    connection: Connection,
    player_tx: mpsc::Sender<Command>,
    shutdown: Shutdown,
    _shutdown_complete: mpsc::Sender<()>,
    msg_tx: MessageSender,
}

impl Handler {
    #[instrument(skip(self))]
    async fn run(&mut self) -> Result<()> {
        while !self.shutdown.is_shutdown() {
            let maybe_request = tokio::select! {
                res = self.connection.read_message() => res?,
                _ = self.shutdown.recv() => {
                    return Ok(());
                }
            };

            let request = match maybe_request {
                Some(Message::Request(request)) => request,
                Some(_) => unreachable!(),
                None => return Ok(()),
            };

            self.execute(&request).await?
        }

        Ok(())
    }

    async fn execute(&mut self, request: &Request<Vec<u8>>) -> Result<()> {
        match request.method() {
            Method::Options => {
                let response_builder = Response::builder(Version::V1_0, StatusCode::Ok);
                let response = self.add_default_headers(request, response_builder)?
                .header(headers::PUBLIC, "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER")
                .empty();

                self.connection.write_response(&response).await?;
                Ok(())
            }
            Method::Setup => {
                let transports = request
                    .header(&headers::TRANSPORT)
                    .map(|x| x.as_str().replace("mode=record", "mode=\"RECORD\""))
                    .map(|x| {
                        Request::builder(Method::Setup, Version::V1_0)
                            .header(headers::TRANSPORT, x)
                            .empty()
                    })
                    .and_then(|x| x.typed_header::<Transports>().ok().flatten());
                let transport = transports.as_ref().and_then(|x| x.first());

                let ports = match transport {
                    Some(Transport::Rtp(rtp)) => {
                        let params = &rtp.params.others;
                        let maybe_control_port = params
                            .get("control_port")
                            .and_then(|x| x.as_ref())
                            .and_then(|x| x.parse().ok());
                        let maybe_timing_port = params
                            .get("timing_port")
                            .and_then(|x| x.as_ref())
                            .and_then(|x| x.parse().ok());

                        if let (Some(control_port), Some(timing_port)) =
                            (maybe_control_port, maybe_timing_port)
                        {
                            Some((control_port, timing_port))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                if let Some((control_port, timing_port)) = ports {
                    let setup = Setup {
                        ip: self.connection.peer_addr.ip(),
                        control_port,
                        timing_port,
                    };

                    let (tx, rx) = oneshot::channel();
                    self.player_tx
                        .send(Command::Setup {
                            payload: setup,
                            resp: tx,
                        })
                        .await?;
                    let success = rx.await?;

                    let response_builder = match success {
                        Ok(res) => {
                            let mut others = BTreeMap::new();
                            others.insert(
                                "control_port".into(),
                                Some(format!("{}", res.control_port)),
                            );
                            others
                                .insert("timing_port".into(), Some(format!("{}", res.timing_port)));

                            let transport = Transport::Rtp(RtpTransport {
                                profile: RtpProfile::Avp,
                                lower_transport: Some(RtpLowerTransport::Udp),
                                params: RtpTransportParameters {
                                    unicast: true,
                                    multicast: false,
                                    server_port: Some((res.server_port, None)),
                                    interleaved: Some((0, Some(1))),
                                    mode: vec![TransportMode::Record],
                                    others,
                                    ..Default::default()
                                },
                            });
                            let transports: Transports = vec![transport].into();

                            Response::builder(Version::V1_0, StatusCode::Ok)
                                .header(headers::SESSION, "1")
                                .typed_header(&transports)
                        }
                        Err(_) => {
                            Response::builder(Version::V1_0, StatusCode::ParameterNotUnderstood)
                        }
                    };
                    let response = self.add_default_headers(request, response_builder)?.empty();

                    self.connection.write_response(&response).await?;
                }

                Ok(())
            }
            Method::GetParameter => {
                let response_builder = self.add_default_headers(
                    request,
                    Response::builder(Version::V1_0, StatusCode::Ok),
                )?;

                let (tx, rx) = oneshot::channel();
                self.player_tx
                    .send(Command::GetParameter { resp: tx })
                    .await?;
                let parameters = rx.await?;

                let body = str::from_utf8(request.body())?
                    .lines()
                    .filter_map({
                        |line| match line {
                            "volume" => Some(format!("volume: {:.6}", parameters.volume)),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\r\n");

                if body.is_empty() {
                    let response = response_builder.empty();
                    self.connection.write_response(&response).await?;
                } else {
                    let response = response_builder.build(body);
                    self.connection.write_response(&response).await?;
                }

                Ok(())
            }
            Method::SetParameter => {
                let response = match request.header(&headers::CONTENT_TYPE).map(|x| x.as_str()) {
                    Some("text/parameters") => {
                        for line in str::from_utf8(request.body())?.lines() {
                            match line.split_once(":") {
                                Some(("volume", volume)) => {
                                    let vol = volume.trim().parse::<f64>()?;
                                    let (tx, rx) = oneshot::channel();
                                    self.player_tx
                                        .send(Command::SetParameter {
                                            volume: vol,
                                            resp: tx,
                                        })
                                        .await?;
                                    let _ = rx.await?;
                                }
                                Some(("progress", prog_str)) => {
                                    let mut split = prog_str.trim().split('/');
                                    let start = split.next().map(|v| v.parse::<u64>());
                                    let curr = split.next().map(|v| v.parse::<u64>());
                                    let end = split.next().map(|v| v.parse::<u64>());
                                    if let (Some(Ok(start)), Some(Ok(curr)), Some(Ok(end))) =
                                        (start, curr, end)
                                    {
                                        self.player_tx
                                            .send(Command::SetProgress { start, curr, end })
                                            .await?;
                                    }
                                }
                                _ => (),
                            }
                        }

                        self.add_default_headers(
                            request,
                            Response::builder(Version::V1_0, StatusCode::Ok),
                        )?
                        .empty()
                    }
                    Some("application/x-dmap-tagged") => {
                        if let Ok(metadata) = RaopMetadata::parse_from_dmap(request.body()) {
                            self.msg_tx.raop(crate::Raop::MetadataSet(metadata));
                        }

                        self.add_default_headers(
                            request,
                            Response::builder(Version::V1_0, StatusCode::Ok),
                        )?
                        .empty()
                    }
                    Some(ctype) if ctype.starts_with("image") => {
                        match ctype {
                            "image/none" => {
                                self.msg_tx.raop(crate::Raop::CoverArtRemoved);
                            }
                            _ => {
                                let data = request.body().to_vec();
                                self.msg_tx.raop(crate::Raop::CoverArtSet(data));
                            }
                        }

                        self.add_default_headers(
                            request,
                            Response::builder(Version::V1_0, StatusCode::Ok),
                        )?
                        .empty()
                    }
                    _ => {
                        Response::builder(Version::V1_0, StatusCode::ParameterNotUnderstood).empty()
                    }
                };

                self.connection.write_response(&response).await?;
                Ok(())
            }
            Method::Announce => {
                let sdp = sdp_types::Session::parse(request.body())?;
                let media = sdp
                    .medias
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("missing media description"))?;

                // fmtp = format parameter
                let fmtp = media
                    .get_first_attribute_value("fmtp")?
                    .map({
                        |x| match x.find(char::is_whitespace) {
                            Some(index) => x[index..].into(),
                            None => x.into(),
                        }
                    })
                    .ok_or_else(|| anyhow!("missing fmtp"))?;

                let minimum_latency = media
                    .get_first_attribute_value("min-latency")
                    .unwrap_or(None)
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(0);

                let maximum_latency = media
                    .get_first_attribute_value("max-latency")
                    .unwrap_or(None)
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(0);

                let aesiv = media
                    .get_first_attribute_value("aesiv")
                    .unwrap_or(None)
                    .and_then(|x| decode_base64(x).ok());

                let aeskey = media
                    .get_first_attribute_value("rsaaeskey")
                    .unwrap_or(None)
                    .and_then(|x| decode_base64(x).ok())
                    .and_then(|x| {
                        let padding = rsa::Oaep::new::<Sha1>();
                        PRIVATE_KEY.decrypt(padding, &x).ok()
                    });

                let encryption = if let (Some(aesiv), Some(aeskey)) = (aesiv, aeskey) {
                    Some(Encryption { aesiv, aeskey })
                } else {
                    None
                };

                let announce = Announce {
                    fmtp,
                    minimum_latency,
                    maximum_latency,
                    encryption,
                };

                let (tx, rx) = oneshot::channel();
                self.player_tx
                    .send(Command::Announce {
                        payload: announce,
                        resp: tx,
                    })
                    .await?;
                let success = rx.await?;

                let response_builder = if success.is_ok() {
                    Response::builder(Version::V1_0, StatusCode::Ok)
                } else {
                    Response::builder(Version::V1_0, StatusCode::NotEnoughBandwidth)
                };
                let response = self.add_default_headers(request, response_builder)?.empty();

                self.connection.write_response(&response).await?;
                Ok(())
            }
            Method::Record => {
                let response_builder = Response::builder(Version::V1_0, StatusCode::Ok)
                    .header(AUDIO_LATENCY.clone(), LATENCY_IN_SAMPLES.to_string());
                let response = self.add_default_headers(request, response_builder)?.empty();

                {
                    let (tx, rx) = oneshot::channel();
                    self.player_tx.send(Command::Record { resp: tx }).await?;
                    let _ = rx.await?;
                }

                self.connection.write_response(&response).await?;
                Ok(())
            }
            Method::Teardown => {
                let response_builder = Response::builder(Version::V1_0, StatusCode::Ok)
                    .header(headers::CONNECTION, "close");
                let response = self.add_default_headers(request, response_builder)?.empty();

                let (tx, rx) = oneshot::channel();
                self.player_tx.send(Command::Teardown { resp: tx }).await?;
                let _ = rx.await?;

                self.connection.write_response(&response).await?;
                Ok(())
            }
            Method::Extension(extension) => match extension.as_str() {
                "FLUSH" | "flush" => {
                    let rtp_header = request.header(&headers::RTP_INFO);
                    let response_builder = Response::builder(Version::V1_0, StatusCode::Ok);
                    let response = self.add_default_headers(request, response_builder)?.empty();

                    if let Some(value) = rtp_header
                        && let Ok((_, info)) = RtpInfo::parse(value.as_str())
                    {
                        let (tx, rx) = oneshot::channel();
                        self.player_tx
                            .send(Command::Flush {
                                resp: tx,
                                payload: info,
                            })
                            .await?;
                        let _ = rx.await?;
                    }

                    self.connection.write_response(&response).await?;
                    Ok(())
                }
                _ => todo!(),
            },

            Method::Describe
            | Method::Pause
            | Method::Play
            | Method::PlayNotify
            | Method::Redirect => {
                let response =
                    Response::builder(Version::V1_0, StatusCode::MethodNotAllowed).empty();

                self.connection.write_response(&response).await?;
                Ok(())
            }
        }
    }

    fn add_default_headers(
        &self,
        request: &Request<Vec<u8>>,
        mut response_builder: ResponseBuilder,
    ) -> Result<ResponseBuilder> {
        response_builder = response_builder.header(headers::SERVER, "AirTunes/105.1");

        if let Some(c_seq) = request.header(&headers::CSEQ) {
            response_builder = response_builder.header(headers::CSEQ, c_seq.as_str());
        }

        if let Some(challenge) = request.header(&APPLE_CHALLENGE) {
            let challenge = challenge.as_str();
            let response = self.calculate_challenge(challenge)?;
            response_builder = response_builder.header(APPLE_RESPONSE.clone(), response);
        }

        Ok(response_builder)
    }

    fn calculate_challenge(&self, challenge: &str) -> Result<String> {
        let chall = decode_base64(challenge)?;
        let addr = match self.connection.local_addr.ip() {
            IpAddr::V4(ip) => ip.octets().to_vec(),
            IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        let hw_addr = self.config.hw_addr.to_vec();

        let buf = [chall, addr, hw_addr].concat();
        let challresp =
            encode_base64(&PRIVATE_KEY.sign(rsa::Pkcs1v15Sign::new_unprefixed(), &buf)?);

        Ok(challresp)
    }
}

#[derive(Debug, Clone)]
pub struct Configuration {
    pub hw_addr: [u8; 6],
}

pub fn device_name_hash(name: &str) -> [u8; 6] {
    use md5::Digest;
    let mut hasher = md5::Md5::new();
    hasher.update(name.as_bytes());
    let h = hasher.finalize();
    [h[0], h[1], h[2], h[3], h[4], h[5]]
}

pub fn hash_to_string(hash: &[u8; 6]) -> String {
    hash.iter()
        .map(|v| format!("{:02X}", v))
        .collect::<String>()
}

pub fn txt_properties() -> HashMap<String, String> {
    macro_rules! s {
        ($s:expr) => {
            $s.to_owned()
        };
    }

    HashMap::from([
        (s!(TXT_VERSION), s!("1")),
        (s!(TXT_DEVICE_MODEL), s!("FCast")),
        (s!(TXT_SERVER_VERSION), s!("105.1")),
        (s!(TXT_SUPPORTED_TRANSPORT), s!("UDP")),
        // 0 = text
        // 1 = artwork
        // 2 = progress
        (s!(TXT_SUPPORTED_METADATA), s!("0,1,2")),
        (s!(TXT_AUDIO_SAMPLE_SIZE), s!("16")),
        (s!(TXT_AUDIO_SAMPLING_RATE), SAMPLING_RATE.to_string()),
        // 0 = no encryption
        // 1 = RSA
        (s!(TXT_ENCRYPTION_TYPE), s!("0,1")),
        // 1 = ALAC
        (s!(TXT_AUDIO_CODECS), s!("1")),
        (s!(TXT_AUDIO_CHANNELS), s!("2")),
        (s!(TXT_PASSWORD_REQUIRED), s!("false")),
        // Required fields with unknown description
        (s!("sf"), s!("0x4")),
        (s!("ek"), s!("1")),
        (s!("da"), s!("true")),
        (s!("sv"), s!("false")),
        (s!("vn"), s!("65537")),
    ])
}

pub fn service_info(device_name: String) -> Result<(ServiceInfo, Configuration)> {
    let hash = device_name_hash(&device_name);
    let fmt_device_name = format!("{}@{device_name}", hash_to_string(&hash),);
    let host_name = format!("{fmt_device_name}.local.");

    let config = Configuration {
        hw_addr: [hash[0], hash[1], hash[2], hash[3], hash[4], hash[5]],
    };

    let props = txt_properties();

    let service = mdns_sd::ServiceInfo::new(
        "_raop._tcp.local.",
        &fmt_device_name,
        &host_name,
        (), // Auto
        33505,
        props,
    )?
    .enable_addr_auto();

    Ok((service, config))
}

pub async fn handle_sender(
    stream: tokio::net::TcpStream,
    config: Configuration,
    msg_tx: MessageSender,
) {
    use tokio::sync::{broadcast, mpsc};

    let (notify_shutdown, _) = broadcast::channel(1);
    let (shutdown_complete_tx, _shutdown_complete_rx) = mpsc::channel(1);
    let (player_tx, player_rx) = mpsc::channel(4);

    let mut player = Player {
        player_tx: player_tx.clone(),
        player_rx,
        shutdown: Shutdown::new(notify_shutdown.subscribe()),
        _shutdown_complete: shutdown_complete_tx.clone(),
        msg_tx: msg_tx.clone(),
    };

    tokio::spawn(async move {
        player.run().await.unwrap();
    });

    let mut handler = Handler {
        config: std::sync::Arc::new(config),
        connection: Connection::new(stream).unwrap(),
        player_tx: player_tx.clone(),
        shutdown: Shutdown::new(notify_shutdown.subscribe()),
        _shutdown_complete: shutdown_complete_tx.clone(),
        msg_tx,
    };

    if let Err(err) = handler.run().await {
        tracing::error!(cause = ?err, "connection error");
    }
}

#[derive(Debug)]
enum DmapTag {
    AlbumArtist,
    Album,
    Artist,
    Comment,
    ContentDescription,
    Composer,
    Category,
    SortArtist,
    SortComposer,
    SortAlbumArtist,
    SortName,
    SortSeries,
    SortAlbum,
    Description,
    Format,
    Genre,
    Keywords,
    LongContentDescription,
    Title,
}

impl DmapTag {
    fn parse(tag: &[u8]) -> Option<Self> {
        Some(match tag {
            b"asaa" => Self::AlbumArtist,
            b"asal" => Self::Album,
            b"asar" => Self::Artist,
            b"ascm" => Self::Comment,
            b"ascn" => Self::ContentDescription,
            b"ascp" => Self::Composer,
            b"asct" => Self::Category,
            b"assa" => Self::SortArtist,
            b"assc" => Self::SortComposer,
            b"assl" => Self::SortAlbumArtist,
            b"assn" => Self::SortName,
            b"asss" => Self::SortSeries,
            b"assu" => Self::SortAlbum,
            b"asdt" => Self::Description,
            b"asfm" => Self::Format,
            b"asgn" => Self::Genre,
            b"asky" => Self::Keywords,
            b"aslc" => Self::LongContentDescription,
            b"minm" => Self::Title,
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub struct RaopMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
}

impl RaopMetadata {
    fn parse_from_dmap(dmap: &[u8]) -> Result<Self> {
        let mut metadata = RaopMetadata {
            title: None,
            artist: None,
        };

        let mut i = 8;
        while i < dmap.len() - 8 {
            let tag = &dmap[i..i + 4];
            i += 4;
            let l = &dmap[i..i + 4];
            let len = u32::from_be_bytes([l[0], l[1], l[2], l[3]]) as usize;
            i += 4;

            let Some(tag) = DmapTag::parse(tag) else {
                continue;
            };

            if i + len >= dmap.len() {
                anyhow::bail!("Out of bounds");
            }

            let val = &dmap[i..i + len];
            i += len;

            match tag {
                DmapTag::Title | DmapTag::Artist => (),
                _ => continue,
            }

            let val_str = String::from_utf8(val.to_vec())?;
            match tag {
                DmapTag::Title => metadata.title = Some(val_str),
                DmapTag::Artist => metadata.artist = Some(val_str),
                _ => (),
            }
        }

        Ok(metadata)
    }
}
