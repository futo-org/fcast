//! Characterization tests for subtitle charset handling.
//!
//! These pin down what a subtitle parser actually does with the bytes of an
//! external subtitle file, which is the only place in the external
//! subtitle chain where a charset is guessed. The chain under test is the
//! tail of the real one:
//!
//!   filesrc -> <parser> -> (text/x-raw,format=pango-markup)
//!
//! In production those bytes arrive from `fcasthttpsrc` inside
//! `urisourcebin parse-streams=true` and the parser is instantiated by
//! `parsebin`, so the buffer boundaries are network sized instead of
//! 4096, but the decoding code path is identical.
//!
//! # WHICH parser, and why this file has two halves
//!
//! Everything below the "green"/"ignored" banners characterizes the **C
//! `subparse`**, which is what the receiver used to autoplug. It no longer
//! does: `receiver-core/src/gstreamer.rs` registers `gstrssubparse` and then
//! SWAPS THE RANKS, demoting `subparse`/`ssaparse` to `Rank::NONE` and
//! promoting `rssubparse`/`rsssaparse` to `Rank::PRIMARY`, "to route every
//! subtitle stream through the Rust elements while keeping the C ones around
//! as a gst-launch escape hatch".
//!
//! So every C-`subparse` assertion in this file, including all four
//! `#[ignore]`d ones, describes an element PRODUCTION NEVER PLUGS. They are
//! still worth keeping (the C element remains reachable by name, and the
//! findings note is written against it), but on their own they say nothing
//! about what a viewer sees. The [`mod production_parser`] section at the
//! bottom re-runs the load-bearing cases against `rssubparse`, the element
//! that actually decodes subtitles in the shipped receiver.
//!
//! Findings, the reasoning and the ranked mitigations live in
//! subtitle-encoding-findings.md at the repo root. Tests marked
//! `#[ignore]` assert the behavior we WANT and fail today.
//!
//! The receiver's shipped mitigation is `GST_SUBTITLE_ENCODING=UTF-8`, set
//! before `gst::init` in crates/receiver-core/src/gstreamer.rs. It is
//! exercised here through [`parse_cues_with_env`], which is why every
//! pipeline in this file runs under one lock: the variable is process global
//! and both parsers read it per converted block, so a test that sets it must
//! be the only pipeline running while it does.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use gst::prelude::*;

/// Generous bound for parsing a few kilobytes of text.
const EOS_TIMEOUT: Duration = Duration::from_secs(10);

/// The default `filesrc` read size, and therefore the offset of the first
/// buffer boundary in every one of these files.
const READ_BOUNDARY: usize = 4096;

/// ZERO WIDTH SPACE. Three bytes (0xE2 0x80 0x8B) in UTF-8, and very
/// common in auto generated captions, which makes it the character most
/// likely to straddle a read boundary in the field.
const ZWSP: char = '\u{200b}';

/// The environment variable `subparse` consults for its fallback encoding,
/// one line before it would guess (`gstsubparse.c:442-444`).
const ENCODING_VAR: &str = "GST_SUBTITLE_ENCODING";

/// Serializes every pipeline run in this binary, so the one test that sets
/// [`ENCODING_VAR`] cannot change the decoding under another test's parser.
static PIPELINE: Mutex<()> = Mutex::new(());

fn lock_pipeline() -> std::sync::MutexGuard<'static, ()> {
    // A failing test poisons the lock, and the tests after it are still
    // meaningful, so the poison is ignored rather than cascaded.
    PIPELINE.lock().unwrap_or_else(|err| err.into_inner())
}

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // The tests that pin today's unconfigured behavior have to see an
        // unset variable even when the suite is run with one exported. Every
        // test calls this before it locks the pipeline, and `Once` blocks the
        // others until the first caller is through, so nothing can be parsing
        // while this runs.
        // SAFETY: no other thread of this binary is running a pipeline or
        // reading the environment at this point.
        unsafe {
            std::env::remove_var(ENCODING_VAR);
        }
        gst::init().unwrap();
    });
}

/// Sets [`ENCODING_VAR`] for as long as it is alive and restores the previous
/// value, so a panicking test cannot leak it into the next one.
struct EncodingEnv(Option<std::ffi::OsString>);

impl EncodingEnv {
    fn set(value: &str) -> Self {
        let previous = std::env::var_os(ENCODING_VAR);
        // SAFETY: the caller holds the pipeline lock, so no other pipeline in
        // this binary is running.
        unsafe {
            std::env::set_var(ENCODING_VAR, value);
        }
        Self(previous)
    }
}

impl Drop for EncodingEnv {
    fn drop(&mut self) {
        // SAFETY: as in `set`, the pipeline lock is still held.
        unsafe {
            match self.0.take() {
                Some(previous) => std::env::set_var(ENCODING_VAR, previous),
                None => std::env::remove_var(ENCODING_VAR),
            }
        }
    }
}

/// The C parser. Registered at `Rank::PRIMARY` by default, but demoted to
/// `Rank::NONE` by the receiver, so nothing autoplugs it in production.
const PARSER_C: &str = "subparse";

/// The Rust parser the receiver promotes to `Rank::PRIMARY`, i.e. the one
/// `parsebin` actually builds for every subtitle stream in the shipped
/// receiver. Registered by [`register_production_parser`].
const PARSER_RS: &str = "rssubparse";

/// Registers `gstrssubparse` into the process registry, exactly as
/// receiver-core does. Idempotent. The rank swap is deliberately NOT done
/// here: these tests instantiate a parser BY NAME, so ranks are irrelevant,
/// and leaving them alone keeps this binary's other suites (which autoplug
/// through `parsebin`) on whatever they were already exercising.
fn register_production_parser() {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        gstrssubparse::plugin_register_static().expect("registering rssubparse");
    });
}

/// Whether the needed plugins are present. Tests skip when missing.
fn plugins_available() -> bool {
    ["filesrc", PARSER_C, "fakesink"]
        .iter()
        .all(|f| gst::ElementFactory::find(f).is_some())
}

/// Whether the production parser is available.
fn production_parser_available() -> bool {
    register_production_parser();
    ["filesrc", PARSER_RS, "fakesink"]
        .iter()
        .all(|f| gst::ElementFactory::find(f).is_some())
}

fn tmp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fcastplaybin-subenc-{}-{}",
        std::process::id(),
        name
    ))
}

/// `hh:mm:ss,mmm` for a whole number of seconds.
fn ts(seconds: u32) -> String {
    format!(
        "{:02}:{:02}:{:02},000",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

/// One SubRip cue, numbered `index`, one second long, starting at
/// `index` seconds.
fn cue(index: u32, text: &str) -> String {
    format!("{index}\n{} --> {}\n{text}\n\n", ts(index), ts(index + 1))
}

/// A small SRT with one cue per entry of `texts`.
fn srt(texts: &[&str]) -> String {
    texts
        .iter()
        .enumerate()
        .map(|(i, t)| cue(i as u32 + 1, t))
        .collect()
}

/// A clean UTF-8 SRT whose byte at [`READ_BOUNDARY`] minus one starts a
/// ZWSP, so the character straddles the first buffer boundary. Nothing
/// about the file is invalid: this is the case that mojibakes a
/// completely well formed subtitle file.
fn srt_straddling_the_read_boundary() -> String {
    let mut out = String::new();
    let mut index = 0;
    while out.len() < READ_BOUNDARY - 300 {
        index += 1;
        out.push_str(&cue(index, &format!("filler line number {index} padding")));
    }

    // Build the straddling cue by hand so the padding length can be
    // computed against the real header length.
    index += 1;
    let head = format!("{index}\n{} --> {}\n", ts(index), ts(index + 1));
    let pad = READ_BOUNDARY - 1 - out.len() - head.len();
    out.push_str(&head);
    out.push_str(&"A".repeat(pad));
    out.push(ZWSP);
    out.push_str("TAILWORD\n\n");

    // Cues after the boundary. A latched parser decodes these as
    // ISO-8859-15 even though they are plain valid UTF-8.
    for k in index + 1..index + 4 {
        out.push_str(&cue(k, &format!("after{ZWSP}boundary café {k}")));
    }

    let bytes = out.as_bytes();
    assert_eq!(
        &bytes[READ_BOUNDARY - 1..READ_BOUNDARY + 2],
        &[0xE2, 0x80, 0x8B],
        "the generated file must put a ZWSP across the read boundary"
    );
    out
}

fn write(name: &str, bytes: &[u8]) -> PathBuf {
    let path = tmp_path(name);
    std::fs::write(&path, bytes).expect("writing the subtitle file");
    path
}

/// [`parse_cues_at`] with the read size that produced the field bug.
fn parse_cues(path: &Path, encoding: Option<&str>) -> Result<Vec<Vec<u8>>, String> {
    parse_cues_at(path, encoding, READ_BOUNDARY as u32)
}

/// [`parse_cues`] through the parser the RECEIVER autoplugs (see
/// [`PARSER_RS`]), at the read size that produced the field bug.
fn parse_cues_rs(path: &Path, encoding: Option<&str>) -> Result<Vec<Vec<u8>>, String> {
    let _pipeline = lock_pipeline();
    run_pipeline_with(PARSER_RS, path, encoding, READ_BOUNDARY as u32)
}

/// [`parse_cues_rs`] with [`ENCODING_VAR`] set for the run, which is what the
/// receiver does at startup. Held under the pipeline lock because the
/// variable is process global.
fn parse_cues_rs_with_env(path: &Path, encoding: &str) -> Result<Vec<Vec<u8>>, String> {
    let _pipeline = lock_pipeline();
    let _env = EncodingEnv::set(encoding);
    run_pipeline_with(PARSER_RS, path, None, READ_BOUNDARY as u32)
}

/// [`parse_cues`] with [`ENCODING_VAR`] set for the run, which is what the
/// receiver does at startup (crates/receiver-core/src/gstreamer.rs). Held
/// under the pipeline lock because the variable is process global.
fn parse_cues_with_env(path: &Path, encoding: &str) -> Result<Vec<Vec<u8>>, String> {
    let _pipeline = lock_pipeline();
    let _env = EncodingEnv::set(encoding);
    run_pipeline(path, None, READ_BOUNDARY as u32)
}

/// Run `filesrc ! subparse ! fakesink` over `path` and return the bytes
/// of every cue buffer `subparse` emitted, in order. `encoding` sets the
/// parser's `subtitle-encoding` property, which is what mitigation 3 in
/// the findings note would do through `parsebin`. `blocksize` fixes the
/// source read size, which is what decides where the buffer boundaries
/// fall and therefore whether a multi byte character gets split.
fn parse_cues_at(
    path: &Path,
    encoding: Option<&str>,
    blocksize: u32,
) -> Result<Vec<Vec<u8>>, String> {
    let _pipeline = lock_pipeline();
    run_pipeline(path, encoding, blocksize)
}

fn run_pipeline(
    path: &Path,
    encoding: Option<&str>,
    blocksize: u32,
) -> Result<Vec<Vec<u8>>, String> {
    run_pipeline_with(PARSER_C, path, encoding, blocksize)
}

/// `filesrc ! <factory> ! fakesink`, see [`run_pipeline`]. The charset
/// override is set through whichever property the parser actually exposes:
/// the C element names it `subtitle-encoding`, the Rust one names it
/// `encoding`. That difference is not cosmetic — `parsebin`/`decodebin3`
/// forward a `subtitle-encoding` property down to the parsers they build,
/// so mitigation 3 from the findings note reaches only the C element.
fn run_pipeline_with(
    factory: &str,
    path: &Path,
    encoding: Option<&str>,
    blocksize: u32,
) -> Result<Vec<Vec<u8>>, String> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().unwrap())
        .property("blocksize", blocksize)
        .build()
        .expect("filesrc");
    let parse = gst::ElementFactory::make(factory)
        .build()
        .unwrap_or_else(|_| panic!("building {factory}"));
    if let Some(encoding) = encoding {
        let name = ["subtitle-encoding", "encoding"]
            .into_iter()
            .find(|name| parse.find_property(name).is_some())
            .unwrap_or_else(|| panic!("{factory} exposes no charset property"));
        parse.set_property(name, encoding);
    }
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink");
    pipeline
        .add_many([&src, &parse, &sink])
        .expect("adding elements");
    gst::Element::link_many([&src, &parse, &sink]).expect("linking elements");

    let cues: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sunk = Arc::clone(&cues);
    parse
        .static_pad("src")
        .expect("subparse src pad")
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data
                && let Ok(map) = buffer.map_readable()
            {
                sunk.lock().unwrap().push(map.to_vec());
            }
            gst::PadProbeReturn::Ok
        })
        .expect("adding the collecting probe");

    pipeline.set_state(gst::State::Playing).expect("to PLAYING");
    let bus = pipeline.bus().expect("pipeline bus");
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_mseconds(EOS_TIMEOUT.as_millis() as u64),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let outcome = match msg.as_deref().map(gst::MessageRef::view) {
        Some(gst::MessageView::Error(err)) => Err(err.error().to_string()),
        Some(gst::MessageView::Eos(_)) => Ok(()),
        _ => Err("timed out waiting for EOS".to_owned()),
    };
    pipeline.set_state(gst::State::Null).expect("to NULL");

    outcome.map(|()| std::mem::take(&mut *cues.lock().unwrap()))
}

/// The cue bytes as a string, so assertions read like the rendered text.
/// Cue buffers are pango markup, so control characters appear as numeric
/// character references such as `&#x80;` rather than as raw bytes.
fn text(cues: &[Vec<u8>], index: usize) -> String {
    String::from_utf8_lossy(&cues[index]).into_owned()
}

// ---------------------------------------------------------------- green

/// Case 1. Clean UTF-8 survives untouched, including the characters most
/// likely to be mangled by a charset fallback.
#[test]
fn clean_utf8_passes_through_unchanged() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let content = srt(&[
        "What\u{200b} is\u{200b} this??",
        "emoji \u{1f600} here",
        "CJK 你好世界",
        "accents café naïve éèü",
    ]);
    let path = write("clean.srt", content.as_bytes());
    let cues = parse_cues(&path, None).expect("the file parses");

    assert_eq!(cues.len(), 4, "one buffer per cue");
    assert_eq!(text(&cues, 0), "What\u{200b} is\u{200b} this??");
    assert_eq!(text(&cues, 1), "emoji \u{1f600} here");
    assert_eq!(text(&cues, 2), "CJK 你好世界");
    assert_eq!(text(&cues, 3), "accents café naïve éèü");
}

/// Case 3. A UTF-8 BOM is consumed rather than rendered, and it pins the
/// encoding for the whole stream. That BOM path is also the only one that
/// handles a split multi byte character correctly, see
/// `bom_immunizes_a_file_against_the_read_boundary_split`.
#[test]
fn utf8_bom_is_consumed() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let mut content = vec![0xEF, 0xBB, 0xBF];
    content.extend_from_slice(srt(&["café\u{200b}ok"]).as_bytes());
    let path = write("bom.srt", &content);
    let cues = parse_cues(&path, None).expect("the file parses");

    assert_eq!(cues.len(), 1);
    assert_eq!(text(&cues, 0), "café\u{200b}ok");
}

/// Case 5. UTF-16LE is detected from its BOM and converted.
#[test]
fn utf16le_with_bom_is_converted() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let mut content = vec![0xFF, 0xFE];
    for unit in srt(&["café\u{200b}ok"]).encode_utf16() {
        content.extend_from_slice(&unit.to_le_bytes());
    }
    let path = write("utf16le.srt", &content);
    let cues = parse_cues(&path, None).expect("the file parses");

    assert_eq!(cues.len(), 1);
    assert_eq!(text(&cues, 0), "café\u{200b}ok");
}

/// Case 7, the parts that are correct. Cue buffers are pango markup, so
/// the whitelisted SubRip tags stay live and everything else is escaped.
#[test]
fn markup_specials_are_escaped_and_allowed_tags_survive() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let content = srt(&[
        "<i>italics</i> here",
        "5 < 6 and 7 > 6",
        "raw & ampersand",
        "unclosed <i>italic",
    ]);
    let path = write("markup.srt", content.as_bytes());
    let cues = parse_cues(&path, None).expect("the file parses");

    assert_eq!(text(&cues, 0), "<i>italics</i> here");
    assert_eq!(text(&cues, 1), "5 &lt; 6 and 7 &gt; 6");
    assert_eq!(text(&cues, 2), "raw &amp; ampersand");
    // The parser balances the markup so pango does not reject the cue.
    assert_eq!(text(&cues, 3), "unclosed <i>italic</i>");
}

/// Case 8, the field bug. A clean, entirely valid UTF-8 file mojibakes
/// from the first read boundary that splits a multi byte character all
/// the way to EOF, because `subparse` validates per buffer and latches
/// the failure. This test pins the CURRENT behavior so a fix is visible
/// as a failure here, and so the `#[ignore]`d twin below documents the
/// intent.
#[test]
fn read_boundary_split_mojibakes_the_rest_of_the_file_today() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let content = srt_straddling_the_read_boundary();
    let path = write("boundary.srt", content.as_bytes());
    let cues = parse_cues(&path, None).expect("the file parses");

    // Everything decoded before the boundary is fine.
    assert_eq!(text(&cues, 0), "filler line number 1 padding");

    // The straddling cue and every later cue are decoded as ISO-8859-15.
    // 0xE2 0x80 0x8B becomes U+00E2 U+0080 U+008B, and the two C1
    // controls reach pango as numeric references, which it draws as hex
    // boxes. That is the exact field artifact.
    let straddling = cues
        .iter()
        .position(|c| c.ends_with(b"TAILWORD"))
        .expect("the straddling cue is emitted");
    assert!(
        text(&cues, straddling).ends_with("â&#x80;&#x8b;TAILWORD"),
        "expected the ISO-8859-15 reading, got {:?}",
        text(&cues, straddling)
    );
    assert!(
        text(&cues, straddling + 1).starts_with("afterâ&#x80;&#x8b;boundary cafÃ©"),
        "the latch keeps every later cue broken, got {:?}",
        text(&cues, straddling + 1)
    );
}

/// The same file, under the mitigation the receiver ships:
/// `GST_SUBTITLE_ENCODING=UTF-8` in the environment, set before `gst::init`
/// in crates/receiver-core/src/gstreamer.rs. A truncated trailing sequence
/// then converts as a partial read instead of tripping the ISO-8859-15
/// fallback, so the file decodes as the valid UTF-8 it is. The green twin
/// above pins what happens with nothing configured, which is what a
/// `gst-launch` reproduction still shows.
#[test]
fn read_boundary_split_should_not_change_the_decoding() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let content = srt_straddling_the_read_boundary();
    let path = write("boundary-want.srt", content.as_bytes());
    let cues = parse_cues_with_env(&path, "UTF-8").expect("the file parses");

    let straddling = cues
        .iter()
        .position(|c| c.ends_with(b"TAILWORD"))
        .expect("the straddling cue is emitted");
    assert!(
        text(&cues, straddling).ends_with("\u{200b}TAILWORD"),
        "got {:?}",
        text(&cues, straddling)
    );
    // The latch is what spread the damage, so the cues after the boundary
    // matter as much as the split one.
    assert!(
        text(&cues, straddling + 1).starts_with("after\u{200b}boundary café"),
        "got {:?}",
        text(&cues, straddling + 1)
    );
}

/// The other half of shipping that variable: it must not change anything
/// else. Measured byte identical to leaving it unset on clean UTF-8 and on a
/// legacy cp1252 file (the findings note, case 10).
#[test]
fn forcing_utf8_in_the_environment_regresses_nothing_else() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let content = srt(&["What\u{200b} is\u{200b} this??", "accents café naïve éèü"]);
    let path = write("noregress-clean.srt", content.as_bytes());
    assert_eq!(
        parse_cues_with_env(&path, "UTF-8").expect("the file parses"),
        parse_cues(&path, None).expect("the file parses"),
    );

    let mut legacy = Vec::new();
    legacy.extend_from_slice(b"1\n00:00:01,000 --> 00:00:02,000\n");
    legacy.extend_from_slice(&[0x93, b'C', b'a', b'f', 0xE9, 0x94]);
    legacy.extend_from_slice(b"\n\n");
    let path = write("noregress-cp1252.srt", &legacy);
    assert_eq!(
        parse_cues_with_env(&path, "UTF-8").expect("the file parses"),
        parse_cues(&path, None).expect("the file parses"),
    );
}

/// Mitigation 3 from the findings note, measured. Setting
/// `subtitle-encoding=UTF-8` on the parser fixes the read boundary split
/// completely, because a truncated trailing sequence converts as a
/// partial read (no error, `consumed` short) instead of tripping the
/// ISO-8859-15 fallback. In production the property would be set on the
/// `parsebin` instance, which forwards it to every parser it builds.
#[test]
fn forcing_subtitle_encoding_utf8_fixes_the_boundary_split() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let content = srt_straddling_the_read_boundary();
    let path = write("boundary-forced.srt", content.as_bytes());
    let cues = parse_cues(&path, Some("UTF-8")).expect("the file parses");

    let straddling = cues
        .iter()
        .position(|c| c.ends_with(b"TAILWORD"))
        .expect("the straddling cue is emitted");
    assert!(
        text(&cues, straddling).ends_with("\u{200b}TAILWORD"),
        "the ZWSP must survive, got {:?}",
        text(&cues, straddling)
    );
    assert!(
        text(&cues, straddling + 1).starts_with("after\u{200b}boundary café"),
        "later cues must stay clean, got {:?}",
        text(&cues, straddling + 1)
    );
}

/// A UTF-8 BOM also immunizes the file, for a different reason: the BOM
/// pins `detected_encoding`, and that code path tracks how many bytes it
/// consumed. This is why some caption files never show the bug.
#[test]
fn bom_immunizes_a_file_against_the_read_boundary_split() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let mut content = vec![0xEF, 0xBB, 0xBF];
    content.extend_from_slice(srt_straddling_the_read_boundary().as_bytes());
    let path = write("boundary-bom.srt", &content);
    // The BOM pushes every offset along by three, so read three bytes
    // more to land the boundary on the same character as the unBOM'd run.
    let cues = parse_cues_at(&path, None, READ_BOUNDARY as u32 + 3).expect("the file parses");

    let straddling = cues
        .iter()
        .position(|c| c.ends_with(b"TAILWORD"))
        .expect("the straddling cue is emitted");
    assert!(
        text(&cues, straddling).ends_with("\u{200b}TAILWORD"),
        "the ZWSP must survive, got {:?}",
        text(&cues, straddling)
    );
}

/// Case 4. A legacy 8 bit file is read as ISO-8859-15, which is right for
/// the Latin range and wrong for the Windows-1252 punctuation block at
/// 0x80 to 0x9F. Pinned because any mitigation must not make this worse.
#[test]
fn legacy_windows_1252_keeps_accents_and_boxes_the_punctuation() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    // 0x93 and 0x94 are cp1252 curly quotes, 0xE9 is e acute.
    let mut content = Vec::new();
    content.extend_from_slice(b"1\n00:00:01,000 --> 00:00:02,000\n");
    content.extend_from_slice(&[0x93, b'C', b'a', b'f', 0xE9, 0x94]);
    content.extend_from_slice(b"\n\n");
    let path = write("cp1252.srt", &content);
    let cues = parse_cues(&path, None).expect("the file parses");

    // The accent is right, the quotes became C1 controls.
    assert_eq!(text(&cues, 0), "&#x93;Café&#x94;");

    // And naming the real charset renders it correctly, which is what
    // the property exists for.
    let cues = parse_cues(&path, Some("windows-1252")).expect("the file parses");
    assert_eq!(text(&cues, 0), "“Café”");
}

// -------------------------------------------------------------- ignored

/// Still broken at this layer: one invalid byte destroys the whole buffer it
/// lands in, including cues that come before it. `GST_SUBTITLE_ENCODING`
/// cannot help, because naming UTF-8 makes `g_convert` fail with EILSEQ and
/// `subparse` then retries the block as hardcoded ISO-8859-15 anyway
/// (findings note, case 10). External subtitles are covered ABOVE this layer
/// instead: the receiver decodes the complete file itself and repairs the one
/// byte (`receiver_core::subtitle_transcode`, `decode` case
/// `one_invalid_byte_in_utf8_damages_only_itself`). Embedded text is still
/// exposed, which is what the upstream `subparse` patch in the findings note
/// would fix.
///
/// STILL ACCURATE on the element production actually plugs: see
/// `production_parser::one_invalid_byte_still_damages_the_whole_read`.
#[test]
#[ignore = "upstream: one invalid byte mojibakes every cue in the same read, unfixable from the env var"]
fn one_invalid_byte_damages_only_itself() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let mut content = srt(&["What\u{200b} is\u{200b} this??", "second cue"]).into_bytes();
    let at = content
        .windows(6)
        .position(|w| w == b"second")
        .expect("the marker is present")
        + 6;
    content.insert(at, 0xFF);
    let path = write("invalid-byte.srt", &content);
    let cues = parse_cues(&path, None).expect("the file parses");

    // The cue before the bad byte has nothing wrong with it.
    assert_eq!(text(&cues, 0), "What\u{200b} is\u{200b} this??");
    // And the damage should be confined to the one byte.
    assert!(
        text(&cues, 1).starts_with("second"),
        "got {:?}",
        text(&cues, 1)
    );
}

/// Still broken, and not a charset problem: WebVTT defines `&amp;`, `&lt;`,
/// `&gt;` and `&nbsp;` as its escapes, and `parse_webvtt` never decodes them.
/// The cue is escaped a second time, so the viewer reads the entity itself.
/// Neither mitigation touches this, since the bytes reaching `subparse` are
/// already correct UTF-8. Upstream fix only.
///
/// STILL ACCURATE on the element production actually plugs: see
/// `production_parser::webvtt_entities_are_still_re_escaped`.
#[test]
#[ignore = "upstream: WebVTT character entities are re-escaped instead of decoded"]
fn webvtt_entities_are_decoded() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let content = "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.000\nTom &amp; Jerry\n\n";
    let path = write("entities.vtt", content.as_bytes());
    let cues = parse_cues(&path, None).expect("the file parses");

    // Wanted: the ampersand decoded once, then escaped once for pango.
    assert_eq!(text(&cues, 0), "Tom &amp; Jerry");
}

/// Still broken, and not a charset problem: prose containing an angle
/// bracketed word is deleted by the unhandled tag stripper, whatever the
/// bytes were. Upstream fix only.
///
/// STILL ACCURATE on the element production actually plugs: see
/// `production_parser::tag_shaped_prose_is_still_deleted`.
#[test]
#[ignore = "upstream: subrip_remove_unhandled_tags deletes tag shaped prose"]
fn tag_shaped_prose_is_preserved() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let content = srt(&["use the <div> tag"]);
    let path = write("tagish.srt", content.as_bytes());
    let cues = parse_cues(&path, None).expect("the file parses");

    assert_eq!(text(&cues, 0), "use the &lt;div&gt; tag");
}

/// Still broken at this layer: a BOM plus one invalid byte fails the whole
/// track. The ISO-8859-15 retry rewrites the BOM into visible text, so format
/// autodetection no longer recognises SubRip and the element errors out with
/// WRONG_TYPE instead of showing slightly broken cues. Naming UTF-8 changes
/// nothing (the conversion still fails with EILSEQ and falls back the same
/// way). Covered above this layer for external subtitles: the receiver
/// repairs the byte and writes a file whose BOM is honest
/// (`receiver_core::subtitle_transcode`).
///
/// The WANT is still unmet on the element production actually plugs, but the
/// FAILURE MODE described above is not what happens there: `rssubparse`
/// reaches EOS cleanly and emits no cues at all rather than erroring the
/// track out. See `production_parser::bom_plus_invalid_byte_still_fails_the_track`.
#[test]
#[ignore = "upstream: BOM plus an invalid byte fails the track instead of degrading"]
fn bom_plus_invalid_byte_still_loads() {
    init();
    if !plugins_available() {
        eprintln!("skipping: plugins missing");
        return;
    }
    let mut content = vec![0xEF, 0xBB, 0xBF];
    content.extend_from_slice(srt(&["first cue", "second cue"]).as_bytes());
    let at = content
        .windows(6)
        .position(|w| w == b"second")
        .expect("the marker is present")
        + 6;
    content.insert(at, 0xFF);
    let path = write("bom-invalid.srt", &content);

    let cues = parse_cues(&path, None).expect("the track must still load");
    assert_eq!(text(&cues, 0), "first cue");
}

// --------------------------------------------------- the production parser

/// The same cases, against the parser the receiver ACTUALLY autoplugs.
///
/// Everything above characterizes the C `subparse`.
/// `receiver-core/src/gstreamer.rs` demotes that element to `Rank::NONE` and
/// promotes `rssubparse` to `Rank::PRIMARY`, so in the shipped receiver
/// `parsebin` never builds the C one at all. Re-running the load-bearing
/// cases here is what keeps the file honest about the viewer's experience:
/// each test states which of the two elements' behaviours it found, so a
/// change on either side shows up as a failure rather than as a silently
/// stale document.
mod production_parser {
    use super::*;

    /// Clean UTF-8 must survive the production parser untouched too. The
    /// baseline: if this ever fails, nothing else in this module means
    /// anything.
    #[test]
    fn clean_utf8_passes_through_unchanged() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let content = srt(&[
            "What\u{200b} is\u{200b} this??",
            "emoji \u{1f600} here",
            "CJK 你好世界",
            "accents café naïve éèü",
        ]);
        let path = write("rs-clean.srt", content.as_bytes());
        let cues = parse_cues_rs(&path, None).expect("the file parses");

        assert_eq!(cues.len(), 4, "one buffer per cue");
        assert_eq!(text(&cues, 0), "What\u{200b} is\u{200b} this??");
        assert_eq!(text(&cues, 1), "emoji \u{1f600} here");
        assert_eq!(text(&cues, 2), "CJK 你好世界");
        assert_eq!(text(&cues, 3), "accents café naïve éèü");
    }

    /// THE FIELD BUG, on the production parser: a clean, entirely valid UTF-8
    /// file whose multi byte character straddles a read boundary.
    ///
    /// On the C element this mojibakes from the boundary to EOF and needs
    /// `GST_SUBTITLE_ENCODING=UTF-8` to avoid it (see
    /// `read_boundary_split_mojibakes_the_rest_of_the_file_today` and its
    /// mitigated twin). The Rust element holds the partial trailing sequence
    /// and prepends it to the next chunk, so the split is invisible with
    /// NOTHING configured.
    ///
    /// This is the one case where knowing which element runs changes the
    /// answer completely, which is why the file cannot be left describing
    /// only the C one.
    #[test]
    fn read_boundary_split_is_not_a_problem_for_the_production_parser() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let content = srt_straddling_the_read_boundary();
        let path = write("rs-boundary.srt", content.as_bytes());
        let cues = parse_cues_rs(&path, None).expect("the file parses");

        let straddling = cues
            .iter()
            .position(|c| c.ends_with(b"TAILWORD"))
            .expect("the straddling cue is emitted");
        assert!(
            text(&cues, straddling).ends_with("\u{200b}TAILWORD"),
            "the ZWSP must survive the split, got {:?}",
            text(&cues, straddling)
        );
        assert!(
            text(&cues, straddling + 1).starts_with("after\u{200b}boundary café"),
            "no latch may spread the damage to later cues, got {:?}",
            text(&cues, straddling + 1)
        );
    }

    /// Pango markup handling on the production parser: whitelisted SubRip
    /// tags stay live, everything else is escaped, unbalanced markup is
    /// closed. Same as the C element (`markup_specials_are_escaped_...`).
    #[test]
    fn markup_specials_are_escaped_and_allowed_tags_survive() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let content = srt(&[
            "<i>italics</i> here",
            "5 < 6 and 7 > 6",
            "raw & ampersand",
            "unclosed <i>italic",
        ]);
        let path = write("rs-markup.srt", content.as_bytes());
        let cues = parse_cues_rs(&path, None).expect("the file parses");

        assert_eq!(text(&cues, 0), "<i>italics</i> here");
        assert_eq!(text(&cues, 1), "5 &lt; 6 and 7 &gt; 6");
        assert_eq!(text(&cues, 2), "raw &amp; ampersand");
        assert_eq!(text(&cues, 3), "unclosed <i>italic</i>");
    }

    /// UTF-16LE end to end on the production parser. `subtitle_transcode` used
    /// to convert this before GStreamer ever saw it; now the parser owns it, so
    /// it needs proving here rather than only in the plugin's own suite.
    #[test]
    fn utf16le_with_bom_is_decoded() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let mut content = vec![0xFF, 0xFE];
        for unit in srt(&["café\u{200b}ok"]).encode_utf16() {
            content.extend_from_slice(&unit.to_le_bytes());
        }
        let path = write("rs-utf16le.srt", &content);
        let cues = parse_cues_rs(&path, None).expect("the file parses");

        assert_eq!(cues.len(), 1);
        assert_eq!(text(&cues, 0), "café\u{200b}ok");
    }

    /// UTF-32LE, the trap case. Its BOM (`FF FE 00 00`) opens with the UTF-16LE
    /// BOM, so a detector testing UTF-16 first misreads the whole file - which
    /// is exactly what the C element does. The transcoder used to cover for
    /// that; the parser now tests UTF-32 first and gets it right.
    #[test]
    fn utf32le_with_bom_is_not_mistaken_for_utf16() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let mut content = vec![0xFF, 0xFE, 0x00, 0x00];
        for ch in srt(&["café\u{200b}ok"]).chars() {
            content.extend_from_slice(&(ch as u32).to_le_bytes());
        }
        let path = write("rs-utf32le.srt", &content);
        let cues = parse_cues_rs(&path, None).expect("the file parses");

        assert_eq!(cues.len(), 1);
        assert_eq!(text(&cues, 0), "café\u{200b}ok");
    }

    /// A legacy 8 bit file on the production parser. `rssubparse` decides the
    /// charset from whole-stream evidence rather than guessing ISO-8859-15 per
    /// buffer, so cp1252's 0x80-0x9F punctuation now decodes correctly with
    /// nothing named. This is the case `subtitle_transcode` used to exist for.
    #[test]
    fn legacy_windows_1252_decodes_without_being_told() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let mut content = Vec::new();
        content.extend_from_slice(b"1\n00:00:01,000 --> 00:00:02,000\n");
        content.extend_from_slice(&[0x93, b'C', b'a', b'f', 0xE9, 0x94]);
        content.extend_from_slice(b"\n\n");
        let path = write("rs-cp1252.srt", &content);

        // ISO-8859-15 would render 0x93/0x94 as C1 control pictures; cp1252
        // renders the curly quotes the file actually meant.
        let cues = parse_cues_rs(&path, None).expect("the file parses");
        assert_eq!(text(&cues, 0), "“Café”");

        // Naming the charset explicitly must agree rather than fight it.
        let cues = parse_cues_rs(&path, Some("windows-1252")).expect("the file parses");
        assert_eq!(text(&cues, 0), "“Café”");
    }

    /// Was `one_invalid_byte_damages_only_itself`, long `#[ignore]`d as a want.
    /// `rssubparse` now delivers it: one stray byte in a valid UTF-8 file no
    /// longer latches the whole stream onto a legacy fallback, so cues before
    /// the damage survive intact and the damage itself becomes U+FFFD rather
    /// than mojibaking everything around it.
    #[test]
    fn one_invalid_byte_damages_only_itself() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let mut content = srt(&["What\u{200b} is\u{200b} this??", "second cue"]).into_bytes();
        let at = content
            .windows(6)
            .position(|w| w == b"second")
            .expect("the marker is present")
            + 6;
        content.insert(at, 0xFF);
        let path = write("rs-invalid-byte.srt", &content);
        let cues = parse_cues_rs(&path, None).expect("the file parses");

        // The cue BEFORE the stray byte is untouched, zero-width spaces and all.
        assert_eq!(
            text(&cues, 0),
            "What\u{200b} is\u{200b} this??",
            "a later stray byte must not reach back and damage an earlier cue"
        );
        // And the damage is confined to one replacement character.
        let damaged = text(&cues, 1);
        assert!(
            damaged.starts_with("second") && damaged.contains('\u{fffd}'),
            "the stray byte should survive as U+FFFD inside its own cue, got {damaged:?}"
        );
    }

    /// The `#[ignore]`d `webvtt_entities_are_decoded`, re-measured on the
    /// production parser. Still broken: `&amp;` is re-escaped rather than
    /// decoded, so the viewer reads the entity itself.
    #[test]
    fn webvtt_entities_are_still_re_escaped() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let content = "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.000\nTom &amp; Jerry\n\n";
        let path = write("rs-entities.vtt", content.as_bytes());
        let cues = parse_cues_rs(&path, None).expect("the file parses");
        assert_eq!(text(&cues, 0), "Tom &amp;amp; Jerry");
    }

    /// The `#[ignore]`d `tag_shaped_prose_is_preserved`, re-measured on the
    /// production parser. Still broken: an angle bracketed word in prose is
    /// deleted by the unhandled tag stripper.
    #[test]
    fn tag_shaped_prose_is_still_deleted() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let content = srt(&["use the <div> tag"]);
        let path = write("rs-tagish.srt", content.as_bytes());
        let cues = parse_cues_rs(&path, None).expect("the file parses");
        assert_eq!(text(&cues, 0), "use the  tag");
    }

    /// Was `bom_plus_invalid_byte_still_loads`, long `#[ignore]`d as a want.
    /// Both older behaviours lost the whole file over one stray byte: C
    /// `subparse` errored the track out with WRONG_TYPE (its ISO-8859-15 retry
    /// rewrote the BOM into visible text, so autodetection stopped recognising
    /// SubRip), and the first `rssubparse` reached EOS cleanly but emitted no
    /// cues at all. The want is now met, and it is the strongest single case
    /// for doing charset work in the parser: a BOM plus one damaged byte still
    /// renders every readable cue.
    #[test]
    fn bom_plus_invalid_byte_still_loads() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let mut content = vec![0xEF, 0xBB, 0xBF];
        content.extend_from_slice(srt(&["first cue", "second cue"]).as_bytes());
        let at = content
            .windows(6)
            .position(|w| w == b"second")
            .expect("the marker is present")
            + 6;
        content.insert(at, 0xFF);
        let path = write("rs-bom-invalid.srt", &content);

        let cues = parse_cues_rs(&path, None).expect("the track loads without erroring");
        assert!(
            !cues.is_empty(),
            "a single damaged byte must not cost the whole track its cues"
        );
        assert_eq!(
            text(&cues, 0),
            "first cue",
            "the cue before the damage is readable and must render, got {:?}",
            cues.iter()
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect::<Vec<_>>()
        );
    }

    /// The receiver's shipped mitigation (`GST_SUBTITLE_ENCODING=UTF-8`) must
    /// not REGRESS the production parser, which does not need it. Measured
    /// byte identical to leaving it unset, on clean UTF-8 and on the
    /// boundary-split file the variable exists for.
    #[test]
    fn forcing_utf8_in_the_environment_regresses_nothing() {
        init();
        if !production_parser_available() {
            eprintln!("skipping: rssubparse missing");
            return;
        }
        let content = srt(&["What\u{200b} is\u{200b} this??", "accents café naïve éèü"]);
        let path = write("rs-noregress-clean.srt", content.as_bytes());
        assert_eq!(
            parse_cues_rs_with_env(&path, "UTF-8").expect("the file parses"),
            parse_cues_rs(&path, None).expect("the file parses"),
        );

        let path = write(
            "rs-noregress-boundary.srt",
            srt_straddling_the_read_boundary().as_bytes(),
        );
        assert_eq!(
            parse_cues_rs_with_env(&path, "UTF-8").expect("the file parses"),
            parse_cues_rs(&path, None).expect("the file parses"),
        );
    }
}
