//! The receiver's bug report as it travels in the issue link a QR carries.
//!
//! A QR a phone reads from a couch holds a few hundred bytes, so the report
//! is a binary record, brotli-compressed against a dictionary both ends
//! ship, and base64url'd, behind a two-byte prefix a decoder can find in an
//! issue body. Nothing travels in clear but the issue title.
//! [`Report::render`] is the readable block, the same text the receiver
//! shows before sending and the decoder prints after.
//!
//! The dictionary holds what a report can say, the error texts, codec
//! descriptions, decoder names and message templates, so a typical report
//! halves against plain compression (`dict/generate.sh`). Encoder and
//! decoder must share it byte for byte, and the same goes for the record
//! layout: a blob is read by the decoder built from the same tree.

use std::fmt;

use base64::Engine;

/// Leads every blob, so a decoder can find one in an issue body. No
/// base64url char is a dot, so it also ends a search.
pub const BLOB_PREFIX: &str = "fc.";
const DICTIONARY: &[u8] = include_bytes!("../dict/dictionary.bin");
/// The window must hold the dictionary and the record; 64 KiB is plenty.
const LGWIN: i32 = 16;

pub const ISSUE_TRACKER_URL: &str = "https://github.com/futo-org/fcast/issues";
const NEW_ISSUE_URL: &str = "https://github.com/futo-org/fcast/issues/new";

/// The most of the error message a fitted link keeps once the report is over
/// budget with no warnings left to drop.
const MESSAGE_CUT: usize = 300;

/// An FC-Exx or FC-Wxx code, one byte on the wire with the class in the high
/// bit. The names are the receiver's kinds, append-only there and here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Code {
    pub warning: bool,
    pub number: u8,
}

impl Code {
    pub fn error(number: u8) -> Self {
        Self {
            warning: false,
            number,
        }
    }

    pub fn warning(number: u8) -> Self {
        Self {
            warning: true,
            number,
        }
    }

    /// "FC-E99" and friends, as the receiver prints them.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("FC-")?;
        let (class, digits) = rest.split_at(1);
        let number: u8 = digits.parse().ok()?;
        match class {
            "E" => Some(Self::error(number)),
            "W" => Some(Self::warning(number)),
            _ => None,
        }
    }

    /// The receiver's kind name, "?" for one this decoder predates.
    pub fn name(self) -> &'static str {
        match (self.warning, self.number) {
            (false, 1) => "NotFound",
            (false, 2) => "AccessDenied",
            (false, 3) => "NetworkFailure",
            (false, 4) => "UnsupportedFormat",
            (false, 5) => "MissingCodec",
            (false, 6) => "DecodeFailed",
            (false, 7) => "DrmProtected",
            (false, 8) => "OutputFailure",
            (false, 9) => "ImageDownloadFailed",
            (false, 10) => "Frozen",
            (false, 99) => "Unexpected",
            (true, 1) => "MissingCodecForTrack",
            (true, 3) => "SubtitleFormatUnsupported",
            (true, 99) => "Unknown",
            _ => "?",
        }
    }

    fn to_byte(self) -> u8 {
        (self.number & 0x7f) | if self.warning { 0x80 } else { 0 }
    }

    fn from_byte(byte: u8) -> Self {
        Self {
            warning: byte & 0x80 != 0,
            number: byte & 0x7f,
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = if self.warning { 'W' } else { 'E' };
        write!(f, "FC-{class}{:02}", self.number)
    }
}

macro_rules! byte_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $value:expr => $text:expr),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        #[repr(u8)]
        pub enum $name {
            #[default]
            Unknown = 0,
            $($variant = $value,)*
        }

        impl $name {
            fn from_byte(byte: u8) -> Self {
                match byte {
                    $($value => Self::$variant,)*
                    _ => Self::Unknown,
                }
            }

            pub fn as_str(self) -> &'static str {
                match self {
                    Self::Unknown => "unknown",
                    $(Self::$variant => $text,)*
                }
            }
        }
    };
}

byte_enum!(Os { Linux = 1 => "linux", Android = 2 => "android", Macos = 3 => "macos", Windows = 4 => "windows" });
byte_enum!(Arch { X86_64 = 1 => "x86_64", Aarch64 = 2 => "aarch64", X86 = 3 => "x86", Arm = 4 => "arm" });
byte_enum!(
    /// The display server, where the OS has more than one.
    Display { Wayland = 1 => "wayland", X11 = 2 => "x11", Native = 3 => "" }
);
byte_enum!(
    /// What kind of item was playing.
    SourceKind { Single = 1 => "single", Playlist = 2 => "playlist", Queue = 3 => "queue", Raop = 4 => "raop", AirplayMirror = 5 => "airplay-mirror" }
);
byte_enum!(
    /// How the item reached the receiver.
    Protocol {
        FcastV1 = 1 => "fcast v1",
        FcastV2 = 2 => "fcast v2",
        FcastV3 = 3 => "fcast v3",
        FcastV4 = 4 => "fcast v4",
        FcastUnintroduced = 5 => "fcast, no introduction (v1 or v2)",
        Chromecast = 6 => "chromecast",
        Raop = 7 => "raop",
        Airplay = 8 => "airplay",
        Gui = 9 => "local gui",
        Autoplay = 10 => "queue autoplay",
    }
);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Device {
    pub os: Os,
    pub arch: Arch,
    pub display: Display,
    /// The hardware model where the platform names one, android's
    /// `ro.product.model`. Empty elsewhere.
    pub model: String,
    /// The OS release where the platform names one, android's
    /// `ro.build.version.release`. Empty elsewhere.
    pub os_version: String,
}

impl Device {
    pub fn is_unknown(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Source {
    pub kind: SourceKind,
    /// The declared container or content type.
    pub container: String,
    /// The media host, or the address with its query stripped when the user
    /// allowed the path. Empty for inline content.
    pub location: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sender {
    pub app: String,
    pub version: String,
    pub protocol: Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Warning {
    /// Seconds before the error, of its latest repeat.
    pub secs_ago: u32,
    pub code: Code,
    /// Consecutive repeats folded into this line, 1 when none.
    pub repeats: u32,
    pub message: String,
}

/// The report. `code` and `message` are the error itself and every report
/// carries them, the rest is what the user allowed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub code: Code,
    /// The kind's interpolable scrap, the codec for a missing one. Empty
    /// when the kind has none.
    pub detail: String,
    /// The error line, the pipeline's message with its diagnostic.
    pub message: String,
    pub receiver_version: String,
    pub device: Device,
    pub source: Option<Source>,
    pub sender: Option<Sender>,
    /// Oldest first.
    pub warnings: Vec<Warning>,
    /// The video decoder elements the pipeline had built when it failed,
    /// comma separated. Empty when none was, a missing codec for one.
    pub decoders: String,
}

/// What fitting a report to a budget left out, for the UI to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    pub warnings_kept: usize,
    pub warnings_total: usize,
    pub message_cut: bool,
}

impl Report {
    /// The issue title, and the first line of the block.
    pub fn title(&self) -> String {
        if self.detail.is_empty() {
            format!("{} {}", self.code, self.code.name())
        } else {
            format!("{} {} ({})", self.code, self.code.name(), self.detail)
        }
    }

    /// The readable block, what the receiver previews and the decoder prints.
    pub fn render(&self) -> String {
        let mut out = self.title();
        out.push('\n');
        out.push_str(&self.message);
        out.push_str("\nreceiver ");
        out.push_str(&self.receiver_version);
        if !self.device.is_unknown() {
            out.push_str("\ndevice ");
            out.push_str(self.device.os.as_str());
            if !self.device.os_version.is_empty() {
                out.push(' ');
                out.push_str(&self.device.os_version);
            }
            out.push(' ');
            out.push_str(self.device.arch.as_str());
            let display = self.device.display.as_str();
            if !display.is_empty() && self.device.display != Display::Unknown {
                out.push(' ');
                out.push_str(display);
            }
            if !self.device.model.is_empty() {
                out.push(' ');
                out.push_str(&self.device.model);
            }
        }
        if let Some(source) = &self.source {
            out.push_str("\nsource ");
            out.push_str(source.kind.as_str());
            if !source.container.is_empty() {
                out.push(' ');
                out.push_str(&source.container);
            }
            if source.location.is_empty() {
                out.push_str(" inline content");
            } else {
                out.push(' ');
                out.push_str(&source.location);
            }
        }
        if !self.decoders.is_empty() {
            out.push_str("\ndecoders ");
            out.push_str(&self.decoders);
        }
        if let Some(sender) = &self.sender {
            out.push_str("\nsender ");
            let mut parts = Vec::with_capacity(3);
            // Some senders put their version in the app name too.
            let app = if sender.version.is_empty() || sender.app.contains(&sender.version) {
                sender.app.clone()
            } else {
                format!("{} {}", sender.app, sender.version)
            };
            let app = app.trim();
            if !app.is_empty() {
                parts.push(app.to_owned());
            }
            parts.push(sender.protocol.as_str().to_owned());
            out.push_str(&parts.join(", "));
        }
        if !self.warnings.is_empty() {
            out.push_str("\nrecent warnings");
            for warning in &self.warnings {
                out.push_str(&format!(
                    "\n{}s ago {} {}",
                    warning.secs_ago, warning.code, warning.message
                ));
                if warning.repeats > 1 {
                    out.push_str(&format!(" (x{})", warning.repeats));
                }
            }
        }
        out
    }

    /// The new-issue link: the title in clear, the report as the body blob.
    pub fn issue_url(&self) -> String {
        let title = form_encode(&self.title());
        format!("{NEW_ISSUE_URL}?title={title}&body={}", encode(self))
    }

    /// The largest report whose link fits `budget` bytes: the oldest warnings
    /// go first, then the message is cut. The error and the sections the
    /// user chose always stay.
    pub fn fitted(&self, budget: usize) -> (Report, Fit) {
        let total = self.warnings.len();
        let mut report = self.clone();
        let mut message_cut = false;
        while report.issue_url().len() > budget {
            if !report.warnings.is_empty() {
                report.warnings.remove(0);
                continue;
            }
            if message_cut || report.message.len() <= MESSAGE_CUT {
                break;
            }
            let cut = (0..=MESSAGE_CUT)
                .rev()
                .find(|i| report.message.is_char_boundary(*i))
                .unwrap_or(0);
            report.message.truncate(cut);
            report.message.push_str("...");
            message_cut = true;
        }
        let fit = Fit {
            warnings_kept: report.warnings.len(),
            warnings_total: total,
            message_cut,
        };
        (report, fit)
    }
}

/// The query encoding a browser expects: spaces as `+`, the rest escaped.
fn form_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// The wire format. Varints are LEB128, strings a varint length and the bytes.

fn put_varint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    put_varint(out, text.len() as u32);
    out.extend_from_slice(text.as_bytes());
}

fn to_bytes(report: &Report) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.push(report.code.to_byte());
    put_str(&mut out, &report.detail);
    put_str(&mut out, &report.message);
    put_str(&mut out, &report.receiver_version);
    out.push(report.device.os as u8);
    out.push(report.device.arch as u8);
    out.push(report.device.display as u8);
    put_str(&mut out, &report.device.model);
    put_str(&mut out, &report.device.os_version);
    match &report.source {
        None => out.push(0),
        Some(source) => {
            out.push(1);
            out.push(source.kind as u8);
            put_str(&mut out, &source.container);
            put_str(&mut out, &source.location);
        }
    }
    match &report.sender {
        None => out.push(0),
        Some(sender) => {
            out.push(1);
            put_str(&mut out, &sender.app);
            put_str(&mut out, &sender.version);
            out.push(sender.protocol as u8);
        }
    }
    put_varint(&mut out, report.warnings.len() as u32);
    for warning in &report.warnings {
        put_varint(&mut out, warning.secs_ago);
        out.push(warning.code.to_byte());
        put_varint(&mut out, warning.repeats);
        put_str(&mut out, &warning.message);
    }
    put_str(&mut out, &report.decoders);
    out
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("no {BLOB_PREFIX} prefix, not a bug report blob for this dictionary")]
    Prefix,
    #[error("base64: {0}")]
    Base64(String),
    #[error("brotli: {0}")]
    Compression(String),
    #[error("the record ends early")]
    Truncated,
    #[error("a string is not utf-8")]
    Utf8,
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, DecodeError> {
        let byte = *self.bytes.get(self.at).ok_or(DecodeError::Truncated)?;
        self.at += 1;
        Ok(byte)
    }

    fn varint(&mut self) -> Result<u32, DecodeError> {
        let mut value = 0u32;
        for shift in (0..35).step_by(7) {
            let byte = self.byte()?;
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(DecodeError::Truncated)
    }

    fn str(&mut self) -> Result<String, DecodeError> {
        let len = self.varint()? as usize;
        let end = self.at.checked_add(len).ok_or(DecodeError::Truncated)?;
        let bytes = self.bytes.get(self.at..end).ok_or(DecodeError::Truncated)?;
        self.at = end;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::Utf8)
    }
}

fn from_bytes(bytes: &[u8]) -> Result<Report, DecodeError> {
    let mut r = Reader { bytes, at: 0 };
    let mut report = Report {
        code: Code::from_byte(r.byte()?),
        detail: r.str()?,
        message: r.str()?,
        receiver_version: r.str()?,
        device: Device {
            os: Os::from_byte(r.byte()?),
            arch: Arch::from_byte(r.byte()?),
            display: Display::from_byte(r.byte()?),
            model: r.str()?,
            os_version: r.str()?,
        },
        ..Report::default()
    };
    if r.byte()? == 1 {
        report.source = Some(Source {
            kind: SourceKind::from_byte(r.byte()?),
            container: r.str()?,
            location: r.str()?,
        });
    }
    if r.byte()? == 1 {
        report.sender = Some(Sender {
            app: r.str()?,
            version: r.str()?,
            protocol: Protocol::from_byte(r.byte()?),
        });
    }
    let count = r.varint()? as usize;
    for _ in 0..count {
        report.warnings.push(Warning {
            secs_ago: r.varint()?,
            code: Code::from_byte(r.byte()?),
            repeats: r.varint()?,
            message: r.str()?,
        });
    }
    report.decoders = r.str()?;
    Ok(report)
}

/// The blob: the record compressed at brotli's best quality against
/// [`DICTIONARY`], base64url without padding, behind [`BLOB_PREFIX`].
pub fn encode(report: &Report) -> String {
    let record = to_bytes(report);
    let mut params = brotli::enc::BrotliEncoderParams::default();
    params.quality = 11;
    params.lgwin = LGWIN;
    params.size_hint = record.len();
    let mut input = record.as_slice();
    let mut compressed = Vec::with_capacity(record.len());
    let mut input_buffer = [0u8; 4096];
    let mut output_buffer = [0u8; 4096];
    // A slice in and a Vec out: neither side can fail.
    let _ = brotli::enc::BrotliCompressCustomIoCustomDict(
        &mut brotli::IoReaderWrapper(&mut input),
        &mut brotli::IoWriterWrapper(&mut compressed),
        &mut input_buffer,
        &mut output_buffer,
        &params,
        brotli::enc::StandardAlloc::default(),
        &mut |_, _, _, _| (),
        DICTIONARY,
        std::io::Error::other("unexpected end of input"),
    );
    let mut out = String::from(BLOB_PREFIX);
    out.push_str(&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(compressed));
    out
}

/// The inverse of [`encode`]. Whitespace around the blob is tolerated.
pub fn decode(blob: &str) -> Result<Report, DecodeError> {
    let body = blob
        .trim()
        .strip_prefix(BLOB_PREFIX)
        .ok_or(DecodeError::Prefix)?;
    let compressed = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|e| DecodeError::Base64(e.to_string()))?;
    let mut input = compressed.as_slice();
    let mut bytes = Vec::new();
    let mut input_buffer = [0u8; 4096];
    let mut output_buffer = [0u8; 4096];
    brotli_decompressor::BrotliDecompressCustomDict(
        &mut input,
        &mut bytes,
        &mut input_buffer,
        &mut output_buffer,
        DICTIONARY.to_vec(),
    )
    .map_err(|e| DecodeError::Compression(e.to_string()))?;
    from_bytes(&bytes)
}

/// The first blob in `text` that decodes: an issue body, a link, or the blob
/// itself, with anything a user typed around it.
pub fn find_and_decode(text: &str) -> Result<Report, DecodeError> {
    let mut last_err = DecodeError::Prefix;
    for (start, _) in text.match_indices(BLOB_PREFIX) {
        let body = &text[start + BLOB_PREFIX.len()..];
        let end = body
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(body.len());
        if end == 0 {
            continue;
        }
        match decode(&text[start..start + BLOB_PREFIX.len() + end]) {
            Ok(report) => return Ok(report),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Letters deflate cannot squeeze, so byte counts mean something.
    pub(crate) fn noise(len: usize, seed: u64) -> String {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                char::from(b'a' + (x % 26) as u8)
            })
            .collect()
    }

    fn report() -> Report {
        Report {
            code: Code::error(99),
            detail: String::new(),
            message: "simulated from the inspector (uri https://example.com/private/movie.mp4)"
                .to_owned(),
            receiver_version: "3.0.4".to_owned(),
            device: Device {
                os: Os::Linux,
                arch: Arch::X86_64,
                display: Display::Wayland,
                model: String::new(),
                os_version: String::new(),
            },
            source: Some(Source {
                kind: SourceKind::Single,
                container: "video/mp4".to_owned(),
                location: "example.com".to_owned(),
            }),
            sender: Some(Sender {
                app: "Grayjay".to_owned(),
                version: "2.0.1".to_owned(),
                protocol: Protocol::FcastV4,
            }),
            warnings: vec![
                Warning {
                    secs_ago: 12,
                    code: Code::warning(1),
                    repeats: 1,
                    message: "no decoder for audio/x-dts".to_owned(),
                },
                Warning {
                    secs_ago: 3,
                    code: Code::warning(99),
                    repeats: 250,
                    message: "stalled".to_owned(),
                },
            ],
            decoders: "avdec_h264".to_owned(),
        }
    }

    #[test]
    fn codes_print_and_parse_as_the_receiver_writes_them() {
        assert_eq!(Code::error(99).to_string(), "FC-E99");
        assert_eq!(Code::warning(1).to_string(), "FC-W01");
        assert_eq!(Code::parse("FC-E05"), Some(Code::error(5)));
        assert_eq!(Code::parse("FC-W99"), Some(Code::warning(99)));
        assert_eq!(Code::parse("FC-X01"), None);
        assert_eq!(Code::parse("E01"), None);
        assert_eq!(Code::error(5).name(), "MissingCodec");
        assert_eq!(Code::warning(3).name(), "SubtitleFormatUnsupported");
        assert_eq!(Code::error(42).name(), "?");
        assert_eq!(Code::from_byte(Code::warning(99).to_byte()), Code::warning(99));
    }

    #[test]
    fn a_full_report_round_trips() {
        let report = report();
        let blob = encode(&report);
        assert!(blob.starts_with(BLOB_PREFIX));
        assert!(!blob[BLOB_PREFIX.len()..].contains(['=', '+', '/']), "{blob}");
        assert_eq!(decode(&blob).unwrap(), report);
        assert_eq!(decode(&format!("  {blob}\n")).unwrap(), report);
    }

    #[test]
    fn an_empty_report_round_trips() {
        let report = Report::default();
        assert_eq!(decode(&encode(&report)).unwrap(), report);
    }

    #[test]
    fn the_render_reads_as_the_receiver_shows_it() {
        let text = report().render();
        assert_eq!(
            text,
            "FC-E99 Unexpected\n\
             simulated from the inspector (uri https://example.com/private/movie.mp4)\n\
             receiver 3.0.4\n\
             device linux x86_64 wayland\n\
             source single video/mp4 example.com\n\
             decoders avdec_h264\n\
             sender Grayjay 2.0.1, fcast v4\n\
             recent warnings\n\
             12s ago FC-W01 no decoder for audio/x-dts\n\
             3s ago FC-W99 stalled (x250)"
        );
        let mut on_a_box = report();
        on_a_box.device = Device {
            os: Os::Android,
            arch: Arch::Arm,
            display: Display::Native,
            model: "LEAP-S1".to_owned(),
            os_version: "14".to_owned(),
        };
        assert!(on_a_box.render().contains("\ndevice android 14 arm LEAP-S1\n"), "{}", on_a_box.render());
        assert_eq!(decode(&encode(&on_a_box)).unwrap(), on_a_box);
        let mut with_version_in_name = report();
        with_version_in_name.sender = Some(Sender {
            app: "FCast Sender SDK v0.3.0".to_owned(),
            version: "0.3.0".to_owned(),
            protocol: Protocol::FcastV4,
        });
        assert!(
            with_version_in_name.render().contains("\nsender FCast Sender SDK v0.3.0, fcast v4"),
            "{}",
            with_version_in_name.render()
        );
        let mut bare = Report::default();
        bare.code = Code::error(5);
        bare.detail = "audio/x-dts".to_owned();
        bare.message = "m".to_owned();
        assert_eq!(bare.render(), "FC-E05 MissingCodec (audio/x-dts)\nm\nreceiver ");
    }

    #[test]
    fn the_link_has_the_title_in_clear_and_the_blob_as_body() {
        let url = report().issue_url();
        assert!(
            url.starts_with("https://github.com/futo-org/fcast/issues/new?title=FC-E99+Unexpected&body=fc."),
            "{url}"
        );
        let body = url.split_once("&body=").unwrap().1;
        assert!(!body.contains('%'));
        assert_eq!(decode(body).unwrap(), report());
        assert_eq!(find_and_decode(&url).unwrap(), report());
        assert_eq!(
            find_and_decode(&format!("It broke while I was watching.\n\n{}\n\nthanks", encode(&report()))).unwrap(),
            report()
        );
    }

    #[test]
    fn decode_errors_are_named() {
        assert_eq!(decode("1.abc"), Err(DecodeError::Prefix));
        assert!(matches!(decode("fc.***"), Err(DecodeError::Base64(_))));
        assert!(matches!(decode("fc.AAAA"), Err(DecodeError::Compression(_)) | Err(DecodeError::Truncated)));
        assert_eq!(from_bytes(&[0x63, 5]), Err(DecodeError::Truncated));
        assert_eq!(find_and_decode("nothing here"), Err(DecodeError::Prefix));
    }

    /// The dictionary's whole point. A report made of the phrases it holds
    /// must stay well under what plain compression gave (160 bytes deflated
    /// for this shape), or the dictionary silently stopped matching.
    #[test]
    fn the_dictionary_halves_a_typical_report() {
        let mut typical = report();
        typical.code = Code::error(5);
        typical.detail = "Digital Video (DV) decoder".to_owned();
        typical.message = "no stream of this item could be decoded [stream CodecNotFound]".to_owned();
        typical.warnings.clear();
        typical.decoders = String::new();
        let blob = encode(&typical);
        let chars = blob.len() - BLOB_PREFIX.len();
        assert!(chars <= 110, "{chars} chars of base64: {blob}");
        assert_eq!(decode(&blob).unwrap(), typical);
    }

    #[test]
    fn a_realistic_report_leaves_margin_in_a_couch_budget() {
        let mut report = report();
        for i in 0..4u32 {
            report.warnings.push(Warning {
                secs_ago: i * 3,
                code: Code::warning(99),
                repeats: 1,
                message: format!("the decoder dropped frames while the queue ran dry, item {i}"),
            });
        }
        let url = report.issue_url();
        assert!(url.len() < 650 * 2 / 3, "{} bytes", url.len());
        let (fitted, fit) = report.fitted(650);
        assert_eq!(fitted, report);
        assert_eq!(fit, Fit { warnings_kept: 6, warnings_total: 6, message_cut: false });
    }

    #[test]
    fn fitting_drops_the_oldest_warnings_first() {
        let mut report = report();
        report.warnings = (0..10u64)
            .map(|i| Warning {
                secs_ago: (10 - i as u32) * 3,
                code: Code::warning(99),
                repeats: 1,
                message: noise(120, i),
            })
            .collect();
        let (fitted, fit) = report.fitted(650);
        assert!(fitted.issue_url().len() <= 650);
        assert!(fit.warnings_kept > 0 && fit.warnings_kept < 10, "{fit:?}");
        assert!(!fit.message_cut);
        assert_eq!(fitted.warnings.last(), report.warnings.last(), "the newest stays");
        assert_eq!(fitted.warnings[0], report.warnings[10 - fit.warnings_kept]);
    }

    #[test]
    fn a_full_ring_of_long_warnings_still_fits_something() {
        let mut report = report();
        report.warnings = (0..10u64)
            .map(|i| Warning {
                secs_ago: 1,
                code: Code::warning(99),
                repeats: 1,
                message: noise(200, i + 50),
            })
            .collect();
        let (fitted, fit) = report.fitted(650);
        assert!(fitted.issue_url().len() <= 650, "{}", fitted.issue_url().len());
        assert!(fit.warnings_kept >= 1, "{fit:?}");
        assert_eq!(fit.warnings_total, 10);
    }

    #[test]
    fn the_message_is_cut_last_and_only_when_incompressible() {
        let mut report = report();
        report.warnings.clear();
        report.message = "x".repeat(2000);
        let (fitted, fit) = report.fitted(650);
        assert!(!fit.message_cut, "repetition deflates to nothing");
        assert_eq!(fitted.message.len(), 2000);

        report.message = noise(2000, 99);
        let (fitted, fit) = report.fitted(650);
        assert!(fit.message_cut);
        assert!(fitted.message.ends_with("..."));
        assert!(fitted.message.len() <= MESSAGE_CUT + 3);
        assert_eq!(decode(&encode(&fitted)).unwrap(), fitted);
        // Cutting is the end of the road: a report the budget still refuses
        // is handed back as is, not emptied.
        let (fitted, _) = fitted.fitted(50);
        assert_eq!(fitted.code, Code::error(99));
    }

    #[test]
    fn the_message_cut_lands_on_a_char_boundary() {
        let mut report = report();
        report.warnings.clear();
        report.message = "ø".repeat(400);
        let (fitted, fit) = report.fitted(10);
        assert!(fit.message_cut);
        assert!(fitted.message.ends_with("ø..."));
    }
}
