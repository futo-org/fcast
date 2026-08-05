//! `to_toml` is the crash-replay mechanism: the fuzz driver writes a failing
//! case out with it so the case becomes a file that replays. That only holds if
//! the document it writes parses AND describes the same media, so this file
//! feeds it the shapes the builder can build but the document nearly cannot.

use fcasttest::{
    scenario::{
        ScenarioBuilder,
        toml::{load_str, to_toml},
    },
    spec::{
        BufferingDip, BufferingRecovery, BufferingSpec, CueSpec, Fault, StreamKind, StreamSpec,
    },
};

fn round_trip(key: &str, build: impl FnOnce(ScenarioBuilder) -> ScenarioBuilder) {
    fcasttest::register_for_tests();
    let original = build(ScenarioBuilder::new(key)).register();
    let document = to_toml(&original);
    let replayed = load_str(&document)
        .unwrap_or_else(|err| panic!("the dumped document does not parse: {err}\n{document}"));

    let (left, right) = (original.spec(), replayed.spec());
    assert_eq!(left.seed, right.seed, "seed\n{document}");
    assert_eq!(left.buffering, right.buffering, "buffering\n{document}");
    assert_eq!(
        left.streams.len(),
        right.streams.len(),
        "stream count\n{document}"
    );
    for (left, right) in left.streams.iter().zip(&right.streams) {
        assert_eq!(left.id, right.id, "id\n{document}");
        assert_eq!(left.duration, right.duration, "duration\n{document}");
        assert_eq!(
            left.bytes_per_buffer, right.bytes_per_buffer,
            "bytes_per_buffer\n{document}"
        );
        assert_eq!(left.pacing, right.pacing, "pacing\n{document}");
        assert_eq!(left.faults, right.faults, "faults\n{document}");
        assert_eq!(left.decoder, right.decoder, "decoder\n{document}");
        assert_eq!(
            format!("{:?}", left.kind),
            format!("{:?}", right.kind),
            "the stream kind changed shape\n{document}"
        );
    }
    replayed.unregister();
}

/// A framerate that is not a whole number of frames per second is exactly what
/// real media has (30000/1001, 24000/1001). The builder takes a `gst::Fraction`,
/// so a scenario can carry one, and the dumped document has to carry it back.
#[test]
fn a_fractional_framerate_survives_the_dump() {
    for (key, num, den) in [
        ("rtfps30", 30000, 1001),
        ("rtfps24", 24000, 1001),
        ("rtfps50", 50, 1),
    ] {
        round_trip(key, |builder| {
            builder.stream(StreamSpec::new(
                "video_0",
                StreamKind::Video {
                    width: 16,
                    height: 16,
                    fps: gst::Fraction::new(num, den),
                    keyframe_interval: 7,
                },
            ))
        });
    }
}

/// Cue text and sync-point names are free-form strings that come from a
/// generator, so the dump has to quote them the way TOML reads them back.
#[test]
fn awkward_strings_survive_the_dump() {
    let awkward = [
        "plain",
        "with \"quotes\"",
        "with \\ backslash",
        "with\ttab",
        "with\nnewline",
        "with \u{7} bell",
        "with \u{1b} escape",
        "unicode ✓ é",
    ];

    for (index, text) in awkward.iter().enumerate() {
        round_trip(&format!("rtstr{index}"), |builder| {
            builder
                .stream(
                    StreamSpec::video("video_0").with_fault(Fault::StallAt {
                        buffer_index: 1,
                        sync_point: (*text).to_owned(),
                    }),
                )
                .text(
                    "text_0",
                    vec![CueSpec::new(
                        gst::ClockTime::from_mseconds(100),
                        gst::ClockTime::from_mseconds(200),
                        *text,
                    )],
                )
                // A buffering recovery gate is a free-form name too.
                .buffering(BufferingSpec::new(20).with_dip(BufferingDip {
                    stream: "video_0".to_owned(),
                    buffer_index: 2,
                    recovery: BufferingRecovery::OnSyncPoint((*text).to_owned()),
                }))
        });
    }
}

/// The everyday shape, so a regression in the awkward cases cannot be "fixed" by
/// breaking the normal one.
#[test]
fn an_ordinary_scenario_survives_the_dump() {
    round_trip("rtplain", |builder| {
        builder
            .video("video_0")
            .audio("audio_0")
            .text(
                "text_0",
                vec![
                    CueSpec::new(
                        gst::ClockTime::from_mseconds(100),
                        gst::ClockTime::from_mseconds(400),
                        "CUE00",
                    ),
                    CueSpec::new(
                        gst::ClockTime::from_mseconds(500),
                        gst::ClockTime::from_mseconds(800),
                        "CUE01",
                    ),
                ],
            )
            .duration(gst::ClockTime::from_mseconds(900))
            .bytes_per_buffer(64)
            // Every buffering shape the dump can carry, in one document.
            .buffering(
                BufferingSpec::new(35)
                    .with_initial_ms(120)
                    .with_periodic(700, 90)
                    .with_dip(BufferingDip {
                        stream: "video_0".to_owned(),
                        buffer_index: 4,
                        recovery: BufferingRecovery::AfterMs(60),
                    }),
            )
    });
}
