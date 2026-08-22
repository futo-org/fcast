//! The track-selection rules, separated from the engine state they read and
//! from the pipeline the dispatch acts on.
//!
//! Three decisions live here, one per hazard the selection path spent field
//! captures learning about:
//!
//! * [`subtitle_holdback`] - what a resolution without video does with the
//!   subtitle it was asked for
//!   ([`crate::selection::SelectionEngine::resolve`]).
//! * [`schedule_refresh`] - whether a dispatch owes a re-emit flush
//!   ([`crate::selection::SelectionEngine::pump`]).
//! * [`upstream_split`] - what the upstream-selection split does with the
//!   upstream-owned part of a selection
//!   ([`crate::FcastPlaybin::dispatch_selection`]).
//!
//! Every one of them used to be reachable only by driving a live pipeline into
//! the shape that provokes it; each is a table row here.

use std::time::Duration;

use crate::selection::TrackSelection;

/// How long the subtitle holdback waits for the collection to announce video
/// before dispatching anyway (see [`subtitle_holdback`]). Long enough to cover
/// decodebin3 merging its inputs one collection at a time, short enough that a
/// media which never announces video still answers the request.
pub(crate) const SUBTITLE_HOLDBACK_GRACE: Duration = Duration::from_secs(1);

/// What a resolution that came out with NO video does with the subtitle stream
/// it also came out with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubtitleHoldback {
    /// Dispatch nothing at all and come back later. The collection may still
    /// be growing a video stream, and an event whose whole content is dropping
    /// the text stream turns a request to ENABLE a subtitle into one that
    /// disables it, which decodebin3 never undoes by auto-selecting text again.
    Defer,
    /// Send the selection with the subtitle KEPT. The video pinning the event
    /// cannot carry is unavoidable while the collection has no video id to
    /// name, and dropping the text stream on top of that would be a second
    /// deselect nobody asked for.
    KeepSubtitle,
    /// Send the selection with the subtitle dropped. Video is really being
    /// turned off, and a text stream cannot be presented without one.
    DropSubtitle,
}

/// Everything [`subtitle_holdback`] reads. Asked ONLY of a resolution whose
/// video slot is empty and whose subtitle slot is not, which is the whole
/// precondition of the rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HoldbackFacts {
    /// The advertised collection carries a video stream. "The selection has no
    /// video" and "the collection has no video" differ, and only the first is a
    /// deselect: the second is decodebin3's merged collection still growing, or
    /// a media with no video at all, where `poll_text_policy` never links text
    /// into the overlay anyway.
    pub(crate) collection_has_video: bool,
    /// The video slot was explicitly asked OFF (`desired_video == Some(None)`),
    /// which is a deselect whatever the collection says.
    pub(crate) video_explicitly_off: bool,
    /// This dispatch would carry nothing but the loss of the subtitle: the
    /// other two slots resolve to what is already applied. Another slot with
    /// real work cannot be made to wait.
    pub(crate) only_the_subtitle_moves: bool,
    /// How long the wait has already lasted, zero at the first deferral. The
    /// engine reads no clock of its own, so this is measured against the
    /// pump's.
    pub(crate) held_for: Duration,
}

/// The subtitle holdback, pure over everything `resolve` reads for it.
///
/// The wait is BOUNDED because its premise fails on media that never announces
/// video at all: no collection change is coming, the pump has already consumed
/// `dirty`, and the desire (plus the request it answers) would be swallowed for
/// the life of the item. Past the grace the dispatch goes out with the subtitle
/// kept, exactly as the busy-slot arm sends it.
pub(crate) fn subtitle_holdback(facts: HoldbackFacts) -> SubtitleHoldback {
    if facts.collection_has_video || facts.video_explicitly_off {
        return SubtitleHoldback::DropSubtitle;
    }
    if facts.only_the_subtitle_moves && facts.held_for < SUBTITLE_HOLDBACK_GRACE {
        return SubtitleHoldback::Defer;
    }
    SubtitleHoldback::KeepSubtitle
}

/// The transport conditions the re-emit flush is decided against, projected
/// from the pump's `PumpCtx`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RefreshTransport {
    /// An adaptive demuxer owns stream selection.
    pub(crate) upstream_owns: bool,
    /// An external subtitle input is attached.
    pub(crate) externals_attached: bool,
    /// The media is known seekable.
    pub(crate) seekable: bool,
}

/// Whether a scheduled re-emit flush may still be dispatched at all.
///
/// Hazardous once an external subtitle input attaches (it races the external
/// inputs' reconfiguration and can freeze the play item) and pointless on an
/// unseekable stream, since the flush IS a seek. Re-decided at every pump, so a
/// flush scheduled before an attach is dropped rather than sent, and a detach
/// does not resurrect it (the flag is cleared, not masked).
pub(crate) fn refresh_still_safe(transport: RefreshTransport) -> bool {
    !transport.externals_attached && transport.seekable
}

/// Whether dispatching `target` over `applied` should schedule a re-emit flush.
///
/// A flushing seek to the current position drops the deeply-buffered old track
/// so a switch takes effect at once. Scheduled only for a switch TO a real
/// audio/subtitle track, and never when a slot is being DISABLED (`Some` ->
/// `None`): flushing across a sink/branch teardown wedges (audio-off drops the
/// pipeline clock, video-off freezes the audio clock, subtitle-off fails vaapi
/// renegotiation). Video switches never flush either, since that re-prerolls
/// the video chain.
///
/// A subtitle switch in UPSTREAM mode does not flush: the adaptive demuxer
/// restarts a re-selected text track from the current position itself (and the
/// join replays the park), while a flushing seek to "the current position"
/// snaps back to the fragment boundary. Measured on DASH: refresh at 5.633 s
/// landed the segment at 4.0 s, a user-visible position jump on every subtitle
/// enable. Audio keeps the flush: its switch lag is the deep aqueue downstream,
/// which no demuxer restart empties.
pub(crate) fn schedule_refresh(
    applied: &TrackSelection,
    target: &TrackSelection,
    transport: RefreshTransport,
) -> bool {
    let switching_to_track = (target.subtitle != applied.subtitle
        && target.subtitle.is_some()
        && !transport.upstream_owns)
        || (target.audio != applied.audio && target.audio.is_some());
    let disabling = (applied.audio.is_some() && target.audio.is_none())
        || (applied.video.is_some() && target.video.is_none())
        || (applied.subtitle.is_some() && target.subtitle.is_none());
    switching_to_track && !disabling && refresh_still_safe(transport)
}

/// What the upstream-selection split does with the upstream-owned part of a
/// dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpstreamSplit {
    /// The part changed and names at least one stream: send it to the main
    /// input and wait for the demuxer's activation edge.
    Send,
    /// The part did not change. A no-op send has no activation edge to confirm
    /// it and decodebin3 posts nothing in this mode, so the crate answers.
    ConfirmLocally,
    /// The part changed to NOTHING: a deselect of every upstream-owned stream.
    /// No `SELECT_STREAMS` can carry it (an empty event is refused, and the
    /// demuxer has no "select nothing"), and confirming it locally would tell
    /// the caller the slot is off while the demuxer keeps playing it. Refused,
    /// so the rollback keeps `applied` naming what is really playing and the
    /// desire stays divergent.
    RefuseEmptyDeselect,
}

/// The upstream split, over the ids that would go out and the mirror of what
/// last did.
///
/// `last_sent` is stored sorted and the comparison is set-shaped, so the
/// candidate is sorted here too. The mirror answers "what did this crate last
/// PUT upstream": it is compared here and recorded only when the event really
/// sends, because a refused send, a lane skip or a superseded core otherwise
/// left it claiming ids that never went out, and the next dispatch of the same
/// target read that as no-change and confirmed locally a selection the demuxer
/// had never been told about.
pub(crate) fn upstream_split(last_sent: &[String], upstream_ids: &[&str]) -> UpstreamSplit {
    let mut sorted: Vec<&str> = upstream_ids.to_vec();
    sorted.sort_unstable();
    let changed = sorted.len() != last_sent.len()
        || sorted
            .iter()
            .zip(last_sent)
            .any(|(id, sent)| *id != sent.as_str());
    if !changed {
        return UpstreamSplit::ConfirmLocally;
    }
    if upstream_ids.is_empty() {
        return UpstreamSplit::RefuseEmptyDeselect;
    }
    UpstreamSplit::Send
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(video: Option<&str>, audio: Option<&str>, subtitle: Option<&str>) -> TrackSelection {
        TrackSelection {
            video: video.map(str::to_string),
            audio: audio.map(str::to_string),
            subtitle: subtitle.map(str::to_string),
        }
    }

    /// A collection still growing, the dispatch carrying nothing else, at the
    /// first deferral.
    const GROWING: HoldbackFacts = HoldbackFacts {
        collection_has_video: false,
        video_explicitly_off: false,
        only_the_subtitle_moves: true,
        held_for: Duration::ZERO,
    };

    #[test]
    fn subtitle_holdback_table() {
        let past = SUBTITLE_HOLDBACK_GRACE;
        let inside = SUBTITLE_HOLDBACK_GRACE - Duration::from_millis(1);
        let rows: &[(HoldbackFacts, SubtitleHoldback, &str)] = &[
            (
                GROWING,
                SubtitleHoldback::Defer,
                "the collection may still announce video",
            ),
            (
                HoldbackFacts {
                    held_for: inside,
                    ..GROWING
                },
                SubtitleHoldback::Defer,
                "one tick short of the grace still waits",
            ),
            (
                HoldbackFacts {
                    held_for: past,
                    ..GROWING
                },
                SubtitleHoldback::KeepSubtitle,
                "the collection never announced video; the switch goes out kept",
            ),
            (
                HoldbackFacts {
                    only_the_subtitle_moves: false,
                    ..GROWING
                },
                SubtitleHoldback::KeepSubtitle,
                "another slot has real work and cannot wait",
            ),
            (
                HoldbackFacts {
                    collection_has_video: true,
                    ..GROWING
                },
                SubtitleHoldback::DropSubtitle,
                "the collection has video, so an empty video slot is a deselect",
            ),
            (
                HoldbackFacts {
                    video_explicitly_off: true,
                    ..GROWING
                },
                SubtitleHoldback::DropSubtitle,
                "video was explicitly asked off",
            ),
            (
                HoldbackFacts {
                    collection_has_video: true,
                    only_the_subtitle_moves: false,
                    held_for: past,
                    ..GROWING
                },
                SubtitleHoldback::DropSubtitle,
                "a real deselect outranks both wait terms",
            ),
        ];
        for (facts, want, why) in rows {
            assert_eq!(subtitle_holdback(*facts), *want, "{why}: {facts:?}");
        }
    }

    /// Seekable, no externals, decodebin3 owning selection.
    const PLAIN: RefreshTransport = RefreshTransport {
        upstream_owns: false,
        externals_attached: false,
        seekable: true,
    };

    #[test]
    fn schedule_refresh_table() {
        let avt = sel(Some("v0"), Some("a0"), Some("t0"));
        let rows: &[(TrackSelection, TrackSelection, RefreshTransport, bool, &str)] = &[
            (
                avt.clone(),
                sel(Some("v0"), Some("a1"), Some("t0")),
                PLAIN,
                true,
                "an audio switch flushes the deep aqueue",
            ),
            (
                sel(Some("v0"), Some("a0"), None),
                sel(Some("v0"), Some("a0"), Some("t1")),
                PLAIN,
                true,
                "enabling a subtitle re-emits the cue on screen",
            ),
            (
                avt.clone(),
                sel(Some("v0"), Some("a0"), Some("t1")),
                PLAIN,
                true,
                "a subtitle switch re-emits too",
            ),
            (
                avt.clone(),
                sel(Some("v0"), Some("a0"), Some("t1")),
                RefreshTransport {
                    upstream_owns: true,
                    ..PLAIN
                },
                false,
                "an upstream subtitle switch would snap back to the fragment boundary",
            ),
            (
                avt.clone(),
                sel(Some("v0"), Some("a1"), Some("t1")),
                RefreshTransport {
                    upstream_owns: true,
                    ..PLAIN
                },
                true,
                "the audio half of the same switch keeps its flush upstream",
            ),
            (
                avt.clone(),
                sel(Some("v1"), Some("a0"), Some("t0")),
                PLAIN,
                false,
                "a video switch re-prerolls the video chain instead",
            ),
            (
                avt.clone(),
                avt.clone(),
                PLAIN,
                false,
                "a no-op moves nothing",
            ),
            (
                avt.clone(),
                sel(Some("v0"), Some("a0"), None),
                PLAIN,
                false,
                "subtitle-off fails vaapi renegotiation across a flush",
            ),
            (
                avt.clone(),
                sel(Some("v0"), None, Some("t0")),
                PLAIN,
                false,
                "audio-off drops the pipeline clock",
            ),
            (
                avt.clone(),
                sel(None, Some("a1"), Some("t0")),
                PLAIN,
                false,
                "video-off freezes the audio clock, even under an audio switch",
            ),
            (
                avt.clone(),
                sel(Some("v0"), Some("a1"), None),
                PLAIN,
                false,
                "the subtitle-disable hazard wins over the audio switch",
            ),
            (
                avt.clone(),
                sel(Some("v0"), Some("a1"), Some("t0")),
                RefreshTransport {
                    externals_attached: true,
                    ..PLAIN
                },
                false,
                "any flush races an attached external input's reconfiguration",
            ),
            (
                avt.clone(),
                sel(Some("v0"), Some("a1"), Some("t0")),
                RefreshTransport {
                    seekable: false,
                    ..PLAIN
                },
                false,
                "the flush is a seek, and this stream has none",
            ),
        ];
        for (applied, target, transport, want, why) in rows {
            assert_eq!(
                schedule_refresh(applied, target, *transport),
                *want,
                "{why}: {applied:?} -> {target:?} {transport:?}"
            );
        }
    }

    #[test]
    fn refresh_safety_is_re_decided_from_the_transport_alone() {
        assert!(refresh_still_safe(PLAIN));
        assert!(!refresh_still_safe(RefreshTransport {
            externals_attached: true,
            ..PLAIN
        }));
        assert!(!refresh_still_safe(RefreshTransport {
            seekable: false,
            ..PLAIN
        }));
        // Upstream mode gates the subtitle SWITCH, never the flush itself.
        assert!(refresh_still_safe(RefreshTransport {
            upstream_owns: true,
            ..PLAIN
        }));
    }

    #[test]
    fn upstream_split_table() {
        let sent = |ids: &[&str]| -> Vec<String> { ids.iter().map(|s| s.to_string()).collect() };
        let rows: &[(Vec<String>, &[&str], UpstreamSplit, &str)] = &[
            (
                sent(&["a0", "v0"]),
                &["v0", "a1"],
                UpstreamSplit::Send,
                "a changed part with streams in it goes out",
            ),
            (
                sent(&[]),
                &["v0", "a0"],
                UpstreamSplit::Send,
                "the first send of a load",
            ),
            (
                sent(&["a0", "v0"]),
                &["v0", "a0"],
                UpstreamSplit::ConfirmLocally,
                "same set, other order: no activation edge to wait for",
            ),
            (
                sent(&["a0", "v0"]),
                &["a0"],
                UpstreamSplit::Send,
                "a shrinking part still names a stream",
            ),
            (
                sent(&["a0", "v0"]),
                &[],
                UpstreamSplit::RefuseEmptyDeselect,
                "no event can carry a deselect of everything upstream owns",
            ),
            (
                sent(&["t-ext"]),
                &[],
                UpstreamSplit::RefuseEmptyDeselect,
                "an external-only mirror still refuses an empty change",
            ),
            (
                sent(&[]),
                &[],
                UpstreamSplit::ConfirmLocally,
                "nothing upstream before, nothing now: unchanged, not a deselect",
            ),
            (
                sent(&["a0"]),
                &["a0", "v0"],
                UpstreamSplit::Send,
                "a growing part goes out",
            ),
        ];
        for (last_sent, ids, want, why) in rows {
            assert_eq!(upstream_split(last_sent, ids), *want, "{why}: {ids:?}");
        }
    }
}
