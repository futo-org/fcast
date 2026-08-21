use super::{
    ExternalSubId, MediaInput, Seek, StartPoint, SubtitleFeedItem, decisions::*, selection,
};
use crate::{
    api::{AfterCancel, PlaybinEvent},
    gapless::SwapState,
    hands::Outcome,
    jobs::{
        Callback, Job, QueuedAt, Refusal, SnapshotCallback, StalePolicy, poison_refusal,
        stale_policy,
    },
    routing::StreamKind,
};

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// Which queued jobs the supersession gate may DROP, pinned.
///
/// A wrongly dropped job is the one new failure class the gate can
/// introduce, so the drop set is a deliberate decision rather than a
/// default. `stale_policy`'s match is exhaustive, which forces a future
/// variant to make that decision; this test makes the decision visible,
/// so nothing lands in `Drop` (or leaves `Run`) unnoticed.
///
/// The `Run` list is the never-drop guarantee: every one of those either
/// carries a sharper token of its own (the seqnum and per-input epoch
/// families), is idempotent against whatever world it finds, or has a
/// caller blocked on its completion.
#[test]
fn stale_policy_drop_set_is_exactly_load_recoverclock_attachsub_dispatchselection() {
    // Only for `gst::Seqnum::next` below; no pipeline is built here.
    gst::init().unwrap();
    let id = ExternalSubId(1);
    let uri = || MediaInput::Uri("file:///item".to_string());
    let pinned = |job: Job, policy: StalePolicy| {
        assert_eq!(stale_policy(&job), policy, "{job:?}");
    };

    // Supersession by a later load or stop is precisely what makes these
    // four wrong to carry out. Nothing else may join them without a
    // named field bug it prevents and a nobody-is-stranded argument.
    let load = Job::Load {
        input: uri(),
        start: StartPoint::Live,
        generation: 1,
    };
    pinned(load, StalePolicy::Drop);
    pinned(Job::RecoverClock, StalePolicy::Drop);
    let attach = Job::AttachSub {
        id,
        url: "file:///subs.srt".to_string(),
    };
    pinned(attach, StalePolicy::Drop);
    // The fourth: a selection names the stream ids of the collection it
    // was formed against, and it does the outgoing item's text-branch
    // surgery on the way out. Unlike the three above, this drop is
    // REPORTED (`dispatch_failed` in `run_job`'s drop path), because the
    // engine recorded the wait before the job was ever queued.
    let dispatch = Job::DispatchSelection {
        target: selection::TrackSelection {
            video: Some("v".to_string()),
            audio: Some("a".to_string()),
            subtitle: None,
        },
        seqnum: gst::Seqnum::next(),
        replacing: true,
        generation: 1,
        queued: QueuedAt(std::time::Instant::now()),
    };
    pinned(dispatch, StalePolicy::Drop);

    // Intent that legitimately outlives an item change, or a guard of its
    // own the epoch must not second-guess. Observability only.
    let set_state = Job::SetState {
        target: gst::State::Paused,
    };
    pinned(set_state, StalePolicy::LogAndRun);
    pinned(
        Job::Seek(Seek::new(None, Some(1.0))),
        StalePolicy::LogAndRun,
    );
    pinned(Job::RecalculateLatency, StalePolicy::LogAndRun);
    let prepare = Job::PrepareNext {
        input: uri(),
        generation: 2,
    };
    pinned(prepare, StalePolicy::LogAndRun);
    pinned(Job::VideoChainGone, StalePolicy::LogAndRun);

    // The never-drop set.
    let stop = Job::Stop {
        target: gst::State::Ready,
        done: None,
    };
    pinned(stop, StalePolicy::Run);
    let refresh = Job::RefreshSeek {
        seqnum: gst::Seqnum::next(),
    };
    pinned(refresh, StalePolicy::Run);
    pinned(Job::DetachSub { id }, StalePolicy::Run);
    pinned(Job::FailSub { id, epoch: 0 }, StalePolicy::Run);
    pinned(Job::CheckSub { id, epoch: 0 }, StalePolicy::Run);
    pinned(Job::RetrySub { id, epoch: 0 }, StalePolicy::Run);
    pinned(Job::AdoptSubState { id, epoch: 0 }, StalePolicy::Run);
    let verify = Job::VerifyReplay {
        id,
        epoch: 0,
        attempt: 0,
    };
    pinned(verify, StalePolicy::Run);
    let replay = Job::ReplaySub {
        id,
        attempt: 0,
        epoch: 0,
    };
    pinned(replay, StalePolicy::Run);
    let dump = Job::DumpGraph {
        done: SnapshotCallback(Box::new(|_| {})),
    };
    pinned(dump, StalePolicy::Run);
    // Touches nothing, and a caller is parked on it. Dropping a barrier
    // strands that caller for the length of its own bound.
    let barrier = Job::Barrier {
        done: Callback(Box::new(|| {})),
    };
    pinned(barrier, StalePolicy::Run);
    pinned(
        Job::CancelPrepared {
            notify: true,
            after: AfterCancel::Nothing,
        },
        StalePolicy::Run,
    );
    pinned(Job::FinishActivation, StalePolicy::Run);
    pinned(Job::SyncTextRunningTime, StalePolicy::Run);
    pinned(Job::DrainTextWork, StalePolicy::Run);
    // Same shape as the drain: a poke whose whole payload is re-read at
    // execution, and whose loss is a text branch that joins on nobody's
    // schedule.
    pinned(Job::PollTextPolicy, StalePolicy::Run);
    pinned(Job::ClearStateFailure, StalePolicy::Run);
    // The deadline fires re-validate against the engine, which a load or
    // a stop has RESET: a stale fire finds nothing in flight for its
    // seqnum and returns. Dropping them would be the worse error, because
    // a deadline that does not fire never fires again.
    let selection_deadline = Job::SelectionDeadline {
        seqnum: gst::Seqnum::next(),
    };
    pinned(selection_deadline, StalePolicy::Run);
    let refresh_deadline = Job::RefreshDeadline {
        seqnum: gst::Seqnum::next(),
    };
    pinned(refresh_deadline, StalePolicy::Run);
    // Table hygiene is unconditional: an in-flight entry a dropped `Done`
    // left behind reads as a wedged lane to the tick and as an unsent
    // selection to the deadline, both for the rest of the instance.
    let done = Job::EffectDone {
        id: 0,
        outcome: Outcome::JoinFinished {
            kind: StreamKind::Text,
        },
    };
    pinned(done, StalePolicy::Run);
    // And the replay's `Done` carries an owed hold release, which a
    // superseded item's external needs exactly as much as a current
    // one's.
    let replay_done = Job::EffectDone {
        id: 1,
        outcome: Outcome::ReplaySent {
            sub_id: id,
            epoch: 0,
            attempt: 0,
            accepted: 1,
            total: 1,
        },
    };
    pinned(replay_done, StalePolicy::Run);
}

/// What a poisoned crate owes each job kind's caller, pinned.
///
/// The gate's contract is that refusing is not ignoring: a refusal that
/// settles nothing moves the wedge from the worker to the callers, which is
/// the silently-never-plays-again shape a field teardown produced. Every
/// variant with a caller parked on an outcome must therefore name a
/// settlement here, and `poison_refusal`'s match is exhaustive so a future
/// variant has to make that decision.
#[test]
fn the_poison_gate_settles_every_job_a_caller_waits_on() {
    // Only for `gst::Seqnum::next`; no pipeline is built here.
    gst::init().unwrap();
    let id = ExternalSubId(1);
    let uri = || MediaInput::Uri("file:///item".to_string());
    let settles = |job: Job, want: &str| {
        let name = format!("{job:?}");
        let got = match poison_refusal(job) {
            Refusal::Nothing => "nothing",
            Refusal::Barrier(_) => "barrier",
            Refusal::Snapshot(_) => "snapshot",
            Refusal::Event { .. } => "event",
            Refusal::DispatchFailed(_) => "dispatch-failed",
            Refusal::RefreshFailed(_) => "refresh-failed",
        };
        assert_eq!(got, want, "{name}");
    };

    // Blocking callers. A shutdown barrier nobody signals is the hang the
    // driver reports; the inspector's snapshot is answered EMPTY rather
    // than by walking a pipeline mid-descent on another thread.
    settles(
        Job::Stop {
            target: gst::State::Null,
            done: Some(Callback(Box::new(|| {}))),
        },
        "barrier",
    );
    // `stop_async` waits on nothing.
    settles(
        Job::Stop {
            target: gst::State::Ready,
            done: None,
        },
        "nothing",
    );
    settles(
        Job::DumpGraph {
            done: SnapshotCallback(Box::new(|_| {})),
        },
        "snapshot",
    );
    // The worker-liveness barrier the suites queue. Nothing else releases it.
    settles(
        Job::Barrier {
            done: Callback(Box::new(|| {})),
        },
        "barrier",
    );

    // Event-awaiting callers, each parked on exactly one outcome.
    settles(
        Job::Load {
            input: uri(),
            start: StartPoint::Live,
            generation: 3,
        },
        "event",
    );
    settles(Job::Seek(Seek::new(None, Some(1.0))), "event");
    settles(
        Job::RefreshSeek {
            seqnum: gst::Seqnum::next(),
        },
        "event",
    );
    settles(
        Job::CancelPrepared {
            notify: true,
            after: AfterCancel::Nothing,
        },
        "event",
    );
    // A self-cancel follows a `PreparedFailed` that already reported.
    settles(
        Job::CancelPrepared {
            notify: false,
            after: AfterCancel::Nothing,
        },
        "nothing",
    );
    settles(
        Job::PrepareNext {
            input: uri(),
            generation: 4,
        },
        "event",
    );
    settles(
        Job::AttachSub {
            id,
            url: "file:///subs.srt".to_string(),
        },
        "event",
    );

    // The selection engine's own waits. The deadline is refused too, so it
    // cannot be what notices the dispatch never left.
    let dispatch = Job::DispatchSelection {
        target: selection::TrackSelection {
            video: Some("v".to_string()),
            audio: Some("a".to_string()),
            subtitle: None,
        },
        seqnum: gst::Seqnum::next(),
        replacing: true,
        generation: 1,
        queued: QueuedAt(std::time::Instant::now()),
    };
    settles(dispatch, "dispatch-failed");
    settles(
        Job::SelectionDeadline {
            seqnum: gst::Seqnum::next(),
        },
        "dispatch-failed",
    );
    settles(
        Job::RefreshDeadline {
            seqnum: gst::Seqnum::next(),
        },
        "refresh-failed",
    );

    // Nobody waits: transport intent the caller re-drives, id/epoch-keyed
    // work whose input dies with the pipeline, and internal hygiene.
    settles(
        Job::SetState {
            target: gst::State::Paused,
        },
        "nothing",
    );
    settles(Job::RecoverClock, "nothing");
    settles(Job::RecalculateLatency, "nothing");
    settles(Job::DetachSub { id }, "nothing");
    settles(Job::FailSub { id, epoch: 0 }, "nothing");
    settles(Job::CheckSub { id, epoch: 0 }, "nothing");
    settles(Job::RetrySub { id, epoch: 0 }, "nothing");
    settles(Job::AdoptSubState { id, epoch: 0 }, "nothing");
    settles(
        Job::VerifyReplay {
            id,
            epoch: 0,
            attempt: 0,
        },
        "nothing",
    );
    settles(
        Job::ReplaySub {
            id,
            attempt: 0,
            epoch: 0,
        },
        "nothing",
    );
    settles(Job::FinishActivation, "nothing");
    settles(Job::SyncTextRunningTime, "nothing");
    settles(Job::DrainTextWork, "nothing");
    settles(Job::VideoChainGone, "nothing");
    settles(Job::ClearStateFailure, "nothing");
    settles(Job::PollTextPolicy, "nothing");
    settles(
        Job::EffectDone {
            id: 0,
            outcome: Outcome::JoinFinished {
                kind: StreamKind::Text,
            },
        },
        "nothing",
    );
}

/// The load-failure report must name the URI that failed, whichever shape
/// the input arrived in: without it the caller's error is unactionable.
#[test]
fn a_refused_load_reports_the_uri_it_would_have_played() {
    gst::init().unwrap();
    let Refusal::Event { event, generation } = poison_refusal(Job::Load {
        input: MediaInput::Uri("file:///item.mkv".to_string()),
        start: StartPoint::Live,
        generation: 9,
    }) else {
        panic!("a refused load must settle with an event");
    };
    // Stamped with the JOB's generation: the current one still names the
    // item being replaced, and the caller's load-scope gate would drop it.
    assert_eq!(generation, Some(9));
    let PlaybinEvent::Error {
        origin, failed_uri, ..
    } = event
    else {
        panic!("a refused load must settle with an Error");
    };
    assert!(matches!(origin, crate::ErrorOrigin::Main));
    assert_eq!(failed_uri.as_deref(), Some("file:///item.mkv"));
}

/// The eager park's whole decision table, which is two bits wide since
/// (`decisions::park_text_before_dispatch`).
///
/// Turning subtitles off always parks -- that IS the handover -- and a
/// REPLACE always parks, because the outgoing branch has to give the one
/// live text slot up before the incoming one can take it. A selection that
/// replaces nothing has no outgoing branch to hand over.
///
/// What this test USED to pin, and what it means that it no longer can:
/// the answer was a three-way `Option<DeferredTextWork>` and two levers
/// could still reach `Flush`, one from an undecidable upstream-selection
/// mode and one from a db3-owned replace. Both arms, and the levers that
/// reached them, are gone with subtitleoverlay, whose sticky re-push
/// cascade the flush deadlocked against; the mode argument went with them,
/// because every remaining arm answers the same thing.
#[test]
fn the_eager_park_covers_off_and_replace_and_nothing_else() {
    assert!(park_text_before_dispatch(true, false), "subtitle-off");
    assert!(
        park_text_before_dispatch(true, true),
        "subtitle-off replace"
    );
    assert!(park_text_before_dispatch(false, true), "replace");
    assert!(
        !park_text_before_dispatch(false, false),
        "a selection that replaces no text track has no branch to hand over"
    );
}

/// The gapless activation window is `swapped` AND `pending`, nothing
/// looser. Two callers depend on exactly this shape: the cancel refusal
/// (`cancel_prepared`) and the duration-refresh gate in
/// `translate_message`, which must not let a successor item's duration
/// reach the caller.
#[test]
fn activation_pending_needs_both_swapped_and_pending() {
    assert_eq!(SwapState::default().activation_pending(), None);
    // Armed but not yet performed: upstream is still the playing item.
    assert_eq!(
        SwapState {
            pending: Some(7),
            ..Default::default()
        }
        .activation_pending(),
        None
    );
    // A long-completed activation leaves `swapped` set, `pending` cleared.
    assert_eq!(
        SwapState {
            swapped: true,
            ..Default::default()
        }
        .activation_pending(),
        None
    );
    assert_eq!(
        SwapState {
            pending: Some(7),
            swapped: true,
            ..Default::default()
        }
        .activation_pending(),
        Some(7)
    );
}

#[test]
fn deselects_video_only_when_video_leaves_the_selection_entirely() {
    let collection = ids(&["vid-a", "vid-b"]);
    // Dropping video from the selection deactivates the chain.
    assert!(deselects_video(true, &collection, &ids(&["aud-1"])));
    // A video-to-video switch keeps the chain (decodebin3 reuses the pad).
    assert!(!deselects_video(
        true,
        &collection,
        &ids(&["vid-b", "aud-1"])
    ));
    // Nothing linked, nothing to deactivate.
    assert!(!deselects_video(false, &collection, &ids(&["aud-1"])));
    // Unknown kinds (no cached collection): never deactivate.
    assert!(!deselects_video(true, &[], &ids(&["aud-1"])));
}

#[test]
fn join_state_caps_at_paused_during_transitions() {
    use gst::State::*;
    // Settled: match the pipeline exactly.
    assert_eq!(join_state(Playing, VoidPending), Playing);
    assert_eq!(join_state(Paused, VoidPending), Paused);
    assert_eq!(join_state(Null, VoidPending), Null);
    // In flight: park at PAUSED so the commit walk finishes the climb
    // with the fresh base_time.
    assert_eq!(join_state(Paused, Playing), Paused);
    assert_eq!(join_state(Ready, Paused), Paused);
    // Downward transitions join below PAUSED.
    assert_eq!(join_state(Paused, Ready), Ready);
}

#[test]
fn pad_accepting_rejects_teardown_stragglers() {
    use gst::State::*;
    // Prerolling or settled at/above PAUSED: accept.
    assert!(pad_accepting(Ready, Paused));
    assert!(pad_accepting(Paused, Playing));
    assert!(pad_accepting(Paused, VoidPending));
    assert!(pad_accepting(Playing, VoidPending));
    // At or heading to READY/NULL: straggler.
    assert!(!pad_accepting(Ready, VoidPending));
    assert!(!pad_accepting(Paused, Ready));
    assert!(!pad_accepting(Playing, Null));
}

#[test]
fn text_links_only_into_a_settled_pipeline() {
    use gst::State::*;
    assert!(text_may_link(Paused, VoidPending));
    assert!(text_may_link(Playing, VoidPending));
    // Mid-transition (the async preroll in particular): never.
    assert!(!text_may_link(Ready, Paused));
    assert!(!text_may_link(Paused, Playing));
    assert!(!text_may_link(Ready, VoidPending));
}

const SECOND: i64 = 1_000_000_000;

/// The cases `decisions::text_pad_offset`'s doc enumerates, as arithmetic.
///
/// Chosen to SEPARATE this formula from the base-only one it replaced, which
/// was right exactly when both segments share a start (every load, seek and
/// gapless swap) and computed -22 s on the one shape that does not. The gapless
/// case agrees under both, the re-add and the lagging external do not.
#[test]
fn the_text_pad_offset_is_the_origin_difference() {
    // Aligned already, a plain load with both segments at zero.
    assert_eq!(text_pad_offset(0, 0, 1.0, 0, 0), 0);

    // THE GAPLESS SWAP, measured at subtitleoverlay's own input pads. Video
    // carries streamsynchronizer's per-group base, text bypasses ssync and
    // carries none, both start at 0. Text has to move forward by the whole base
    // or every cue of the new item lands ~8 s in the past.
    let ssync_base = 8 * SECOND + 189_219_955;
    assert_eq!(text_pad_offset(0, ssync_base, 1.0, 0, 0), ssync_base);

    // THE ADAPTIVE MID-STREAM RE-ADD. dashdemux2 restarts a re-selected track
    // at its global output position, emitting `start == base == that position`,
    // a segment already ON the pipeline's running-time line. The base-only
    // formula read that base as drift and applied -22.066 s. The origin
    // difference computes zero.
    let readd = 22 * SECOND + 66_000_000;
    assert_eq!(text_pad_offset(0, 0, 1.0, readd, readd), 0);

    // AN EXTERNAL LAGGING A SOUGHT VIDEO. The video was seeked to P and its
    // segment starts there, the external's own timeline still starts at 0, so
    // the offset is the -P that aligns them (the base-only formula computed 0
    // and left the repair to the replay seek).
    let p = 30 * SECOND;
    assert_eq!(text_pad_offset(p, 0, 1.0, 0, 0), -p);

    // A REPLAYED EXTERNAL, the same case after its replay seek re-issued the
    // pipeline seek at the video's origin. Same start, base 0 on both.
    assert_eq!(text_pad_offset(p, 0, 1.0, p, 0), 0);
}

#[test]
fn the_text_pad_offset_scales_the_start_term_by_the_rate() {
    // Only the START term is a stream-time distance and needs scaling, the
    // bases are already running time. At 2x, a text segment starting two
    // seconds of MEDIA after the video's start is one second of RUNNING TIME
    // after it.
    assert_eq!(text_pad_offset(0, 0, 2.0, 2 * SECOND, 0), SECOND);
    assert_eq!(text_pad_offset(0, 0, 0.5, SECOND, 0), 2 * SECOND);
    // The base difference is untouched by the rate.
    assert_eq!(text_pad_offset(0, 3 * SECOND, 2.0, 0, 0), 3 * SECOND);
    // Rate 1.0 stays on the integer path, so an odd nanosecond count is
    // exact rather than rounded through f64.
    let odd = 1_234_567_891_011_121;
    assert_eq!(text_pad_offset(0, 0, 1.0, odd, 0), odd);
}

#[test]
fn the_text_pad_offset_is_idempotent_against_its_own_output() {
    // `gst_pad_set_offset` applies on the way OUT to the peer, so the pad's own
    // sticky segment keeps the raw values and a second poll recomputes the SAME
    // answer. That is what makes the caller's `if pad.offset() != offset` a
    // stable no-op rather than a per-poll rewrite.
    let (vs, vb, ts, tb) = (5 * SECOND, 2 * SECOND, 0, 0);
    let first = text_pad_offset(vs, vb, 1.0, ts, tb);
    assert_eq!(text_pad_offset(vs, vb, 1.0, ts, tb), first);
}

#[test]
fn caps_name_kind_fallback() {
    assert_eq!(kind_from_caps_name("video/x-h264"), Some(StreamKind::Video));
    assert_eq!(kind_from_caps_name("audio/mpeg"), Some(StreamKind::Audio));
    assert_eq!(kind_from_caps_name("text/x-raw"), Some(StreamKind::Text));
    assert_eq!(
        kind_from_caps_name("subpicture/x-dvd"),
        Some(StreamKind::Text)
    );
    assert_eq!(kind_from_caps_name("application/x-id3"), None);
}

/// The consumer caps gate, pinned. It was the single decision point for
/// format acceptance and had no test of its own; cue-IR adds a variant that
/// must NOT appear here, which is exactly the kind of claim a test has to
/// hold down.
///
/// Every bitmap format is carried, so a `None` for a subpicture caps means
/// one thing only: no decoder exists for it yet. Each vertical moves its
/// line here as it lands (P10).
#[test]
fn consumer_caps_gate() {
    gst::init().unwrap();
    let format_of = |caps: gst::Caps| consumer_stream_format(&caps);
    let text = |fmt: &str| {
        gst::Caps::builder("text/x-raw")
            .field("format", fmt)
            .build()
    };

    assert_eq!(
        format_of(text("utf8")),
        Some(ConsumerStreamFormat::Text(super::SubtitleTextFormat::Utf8))
    );
    assert_eq!(
        format_of(text("pango-markup")),
        Some(ConsumerStreamFormat::Text(
            super::SubtitleTextFormat::PangoMarkup
        ))
    );
    // No `format` field: renderable, taken as utf8 rather than refused.
    assert_eq!(
        format_of(gst::Caps::builder("text/x-raw").build()),
        Some(ConsumerStreamFormat::Text(super::SubtitleTextFormat::Utf8))
    );
    // A format we cannot draw, and media that is not text at all: refused,
    // which is what raises `SubtitleTrackUnsupported`.
    assert_eq!(format_of(text("cue-ir")), None);
    assert_eq!(format_of(text("utf16")), None);
    assert_eq!(
        format_of(gst::Caps::builder("application/x-ssa").build()),
        None
    );
    // THE WHOLE IMPLEMENTED SET, driven off `BitmapSubFormat::ALL` rather
    // than a list written out here: every format is carried now, so what
    // this has to prove is that each one has a caps name and that the name
    // passes the gate, and that a fourth variant breaks it rather than slips
    // past.
    for format in super::BitmapSubFormat::ALL {
        let name = match format {
            super::BitmapSubFormat::Pgs => "subpicture/x-pgs",
            super::BitmapSubFormat::Vobsub => "subpicture/x-dvd",
            super::BitmapSubFormat::Dvb => "subpicture/x-dvb",
        };
        assert_eq!(
            bitmap_sub_format(name),
            Some(format),
            "{name} does not map back to {format:?}"
        );
        assert_eq!(
            format_of(gst::Caps::builder(name).build()),
            Some(ConsumerStreamFormat::Bitmap(format)),
            "{name} has a decoder behind it and must pass the gate"
        );
    }
    // xsub is not named at all: no decoder is planned, so it stays in the
    // loud set permanently rather than waiting for one.
    assert_eq!(bitmap_sub_format("subpicture/x-xsub"), None);
    assert_eq!(
        format_of(gst::Caps::builder("subpicture/x-xsub").build()),
        None
    );
}

/// The cue-IR mode is chosen by a buffer META, never by the caps: a parser
/// in `text-format=cue-ir` negotiates plain `format=utf8`, so this gate's
/// answer for a cue-IR stream is, and must stay, `Utf8`. Cue-IR is a payload
/// difference only; there is no negotiation difference at all.
#[test]
fn cue_ir_streams_are_indistinguishable_at_the_caps_gate() {
    gst::init().unwrap();
    let caps = gst::Caps::builder("text/x-raw")
        .field("format", "utf8")
        .build();
    assert_eq!(
        consumer_stream_format(&caps),
        Some(ConsumerStreamFormat::Text(super::SubtitleTextFormat::Utf8)),
        "cue-ir must not be reachable from caps alone"
    );
}

// --- external subtitle error policy --------------------------------------

#[test]
fn transport_race_deaths_recover_in_place() {
    // A deselect or one of our own flushes caught the source
    // mid-push: the source is fine, it stays attached, and the
    // join-time replay (or the never-linked retry) restarts it.
    assert_eq!(
        external_error_action(Some("streaming stopped, reason not-linked (-1)")),
        ExternalErrorAction::Recover
    );
    assert_eq!(
        external_error_action(Some("streaming stopped, reason flushing (-2)")),
        ExternalErrorAction::Recover
    );
}

#[test]
fn genuine_errors_fail_fast() {
    // Resource/decode/network errors are real failures, failed fast.
    assert_eq!(
        external_error_action(Some("Could not open resource for reading.")),
        ExternalErrorAction::Fail
    );
    assert_eq!(external_error_action(None), ExternalErrorAction::Fail);
}

// ------------------------------------- the transport's segment clip (P12(b))

/// One appsink sample, assembled the way the text branch's appsink hands it
/// over: buffer, caps and SEGMENT together.
///
/// `segment_start` is where a flushing ACCURATE seek left the segment, so
/// running time reads straight off it: a position at `segment_start` is zero.
fn sample_at(
    caps: &str,
    format: Option<&str>,
    pts: gst::ClockTime,
    duration: Option<gst::ClockTime>,
    segment_start: gst::ClockTime,
) -> gst::Sample {
    let mut buffer = gst::Buffer::from_slice(b"SPANNING".as_slice());
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(pts);
        buffer.set_duration(duration);
    }
    let mut caps = gst::Caps::builder(caps);
    if let Some(format) = format {
        caps = caps.field("format", format);
    }
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_start(segment_start);
    segment.set_time(segment_start);
    segment.set_position(segment_start);
    gst::Sample::builder()
        .buffer(&buffer)
        .caps(&caps.build())
        .segment(&segment)
        .build()
}

fn cue_bounds(item: Option<SubtitleFeedItem>) -> Option<(gst::ClockTime, Option<gst::ClockTime>)> {
    match item {
        Some(SubtitleFeedItem::Cue {
            start_rt, end_rt, ..
        }) => Some((start_rt, end_rt)),
        _ => None,
    }
}

/// A cue delivered under a REVERSE segment must still name a forward window.
///
/// `pipeline::rate_seek_event` builds a real reverse seek for a negative rate,
/// where running time runs the other way (`(stop - t)/|rate| + base`), so
/// mapping `[pts, pts+duration)` bound-for-bound hands the consumer an `end_rt`
/// below `start_rt`. A renderer expires on `now >= end_rt`, so an inverted
/// window is expired at birth and every cue is dropped for the whole reverse
/// play.
#[test]
fn a_cue_under_a_reverse_segment_still_names_a_forward_window() {
    gst::init().unwrap();
    let mut buffer = gst::Buffer::from_slice(b"REVERSE".as_slice());
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(gst::ClockTime::from_seconds(4));
        buffer.set_duration(gst::ClockTime::from_seconds(2));
    }
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_rate(-1.0);
    segment.set_start(gst::ClockTime::ZERO);
    segment.set_stop(gst::ClockTime::from_seconds(10));
    let sample = gst::Sample::builder()
        .buffer(&buffer)
        .caps(
            &gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build(),
        )
        .segment(&segment)
        .build();

    let (start_rt, end_rt) = cue_bounds(crate::Inner::item_from_sample(&sample))
        .expect("a cue inside a reverse segment is still a cue");
    let end_rt = end_rt.expect("the unit carried a duration, so the window is closed");
    assert!(
        end_rt > start_rt,
        "a reverse segment inverted the cue window ({start_rt} .. {end_rt}); every cue \
         reaches the renderer already expired"
    );
    // The interval itself is unchanged, two seconds of media at 1x.
    assert_eq!(end_rt - start_rt, gst::ClockTime::from_seconds(2));
}

/// A seek that lands INSIDE a cue: the demuxer resends the covering unit with
/// its ORIGINAL, pre-target pts (measured on matroskademux and on
/// qtdemux/tx3g alike). The transport must CLIP it onto the new segment, not
/// drop it -- dropping it is what left a paused seek with a bare frame and a
/// playing seek with no subtitle until the next cue began.
#[test]
fn a_cue_spanning_the_segment_start_is_clipped_onto_it() {
    gst::init().unwrap();
    // The mkv measurement, to scale: cue 1s..3s, seek to 2s.
    let sample = sample_at(
        "text/x-raw",
        Some("utf8"),
        gst::ClockTime::from_seconds(1),
        Some(gst::ClockTime::from_seconds(2)),
        gst::ClockTime::from_seconds(2),
    );
    assert_eq!(
        cue_bounds(crate::Inner::item_from_sample(&sample)),
        Some((gst::ClockTime::ZERO, Some(gst::ClockTime::from_seconds(1)))),
        "the covering cue must start at the segment and keep its real end"
    );
}

/// A cue with no duration is OPEN-ENDED, so a segment starting after its pts
/// still sits inside it. `gst_segment_clip` says so, and a renderer showing
/// the last cue until the next one needs it to.
#[test]
fn an_open_ended_cue_before_the_segment_is_clamped_rather_than_dropped() {
    gst::init().unwrap();
    let sample = sample_at(
        "text/x-raw",
        Some("utf8"),
        gst::ClockTime::from_seconds(1),
        None,
        gst::ClockTime::from_seconds(2),
    );
    assert_eq!(
        cue_bounds(crate::Inner::item_from_sample(&sample)),
        Some((gst::ClockTime::ZERO, None))
    );
}

/// A unit lying WHOLLY before the segment is still dropped: it describes a
/// stretch of timeline the pipeline has left, and clipping it forward would
/// paint a stale cue over the seek target.
#[test]
fn a_cue_wholly_before_the_segment_is_still_dropped() {
    gst::init().unwrap();
    let sample = sample_at(
        "text/x-raw",
        Some("utf8"),
        gst::ClockTime::ZERO,
        Some(gst::ClockTime::from_seconds(1)),
        gst::ClockTime::from_seconds(2),
    );
    assert!(
        crate::Inner::item_from_sample(&sample).is_none(),
        "a cue that ends before the segment begins has nothing to show"
    );
}

/// A cue wholly inside the segment is untouched -- the clip is only ever
/// reached by the seam case.
#[test]
fn a_cue_inside_the_segment_keeps_both_of_its_bounds() {
    gst::init().unwrap();
    let sample = sample_at(
        "text/x-raw",
        Some("utf8"),
        gst::ClockTime::from_seconds(3),
        Some(gst::ClockTime::from_seconds(1)),
        gst::ClockTime::from_seconds(2),
    );
    assert_eq!(
        cue_bounds(crate::Inner::item_from_sample(&sample)),
        Some((
            gst::ClockTime::from_seconds(1),
            Some(gst::ClockTime::from_seconds(2))
        ))
    );
}

/// The BITMAP arm shares the computation, so it shares the fix: a VOBSUB unit
/// the seek landed inside arrives clamped, and reports the time it has LEFT
/// rather than the time it was authored with (the ledger's P12(b) residual).
#[test]
fn a_bitmap_unit_spanning_the_segment_start_is_clipped_and_shortened() {
    gst::init().unwrap();
    let sample = sample_at(
        "subpicture/x-dvd",
        None,
        gst::ClockTime::from_seconds(1),
        Some(gst::ClockTime::from_seconds(2)),
        gst::ClockTime::from_seconds(2),
    );
    match crate::Inner::item_from_sample(&sample) {
        Some(SubtitleFeedItem::Bitmap { rt, duration, .. }) => {
            assert_eq!(rt, gst::ClockTime::ZERO);
            assert_eq!(duration, Some(gst::ClockTime::from_seconds(1)));
        }
        other => panic!("expected a clipped bitmap unit, got {other:?}"),
    }
}

/// And the bitmap arm still drops what lies wholly behind the segment.
#[test]
fn a_bitmap_unit_wholly_before_the_segment_is_still_dropped() {
    gst::init().unwrap();
    let sample = sample_at(
        "subpicture/x-dvd",
        None,
        gst::ClockTime::ZERO,
        Some(gst::ClockTime::from_seconds(1)),
        gst::ClockTime::from_seconds(2),
    );
    assert!(crate::Inner::item_from_sample(&sample).is_none());
}

/// A record that occupies NO time is not a cue. Some tracks carry a
/// zero-length twin in front of every real one (same start, same text); the
/// branch drops those before the sink, because while PAUSED they would spend
/// the single preroll slot, and this is the same judgement where the window is
/// computed -- so a clip that lands degenerate for any other reason is refused
/// rather than fed to a renderer as an already-expired cue.
#[test]
fn a_zero_length_record_is_not_a_cue() {
    gst::init().unwrap();
    // The field's shape: the twin sits AFTER the segment start, so nothing is
    // clipped and the degenerate window is the record's own.
    let sample = sample_at(
        "text/x-raw",
        Some("utf8"),
        gst::ClockTime::from_seconds(3),
        Some(gst::ClockTime::ZERO),
        gst::ClockTime::from_seconds(2),
    );
    assert!(
        crate::Inner::item_from_sample(&sample).is_none(),
        "a record with start == end can never be shown"
    );
}

/// And the same for a window the CLIP makes degenerate: a unit ending exactly
/// where the segment begins has nothing left of it.
#[test]
fn a_record_ending_exactly_at_the_segment_start_is_refused() {
    gst::init().unwrap();
    let sample = sample_at(
        "text/x-raw",
        Some("utf8"),
        gst::ClockTime::from_seconds(1),
        Some(gst::ClockTime::from_seconds(1)),
        gst::ClockTime::from_seconds(2),
    );
    assert!(crate::Inner::item_from_sample(&sample).is_none());
}

/// Field crash: a sender's Load carried `speed: 0.0`, which reached
/// `gst_event_new_seek` as the start seek's rate. gst asserts `rate != 0.0`
/// and returns NULL, the binding panics on the NULL event, and the panic
/// kills the worker thread for good (every later job logs "worker is gone").
/// The load entry coerces such rates to 1.0x instead.
#[test]
fn a_zero_or_non_finite_start_rate_is_coerced_to_1x() {
    use crate::decisions::sanitize_start_rate;
    for bad in [0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(sanitize_start_rate(bad), 1.0, "rate {bad}");
    }
    for ok in [1.0, 2.0, 0.5, -1.0] {
        assert_eq!(sanitize_start_rate(ok), ok, "rate {ok}");
    }
}

/// TRICKMODE exactly for reverse or above 2.0x. The 2.0 boundary is
/// INCLUSIVE on the full-quality side: a 2x "watch faster" keeps every
/// frame, and only just above it starts dropping.
#[test]
fn trickmode_is_reverse_or_strictly_above_two_x() {
    let full_quality = [0.5, 1.0, 1.25, 1.5, 2.0];
    for rate in full_quality {
        let flags = seek_flags_for(rate);
        assert!(
            !flags.contains(gst::SeekFlags::TRICKMODE),
            "rate {rate} must stay full quality"
        );
        assert!(flags.contains(gst::SeekFlags::FLUSH));
    }
    let trick = [2.0000001, 2.5, 4.0, -0.5, -1.0, -2.0];
    for rate in trick {
        let flags = seek_flags_for(rate);
        assert!(
            flags.contains(gst::SeekFlags::TRICKMODE),
            "rate {rate} must enable trickmode"
        );
        assert!(flags.contains(gst::SeekFlags::FLUSH));
    }
}

/// THE LEVERS ARE GONE: no `FCAST_<NAME>` token may appear anywhere in `src/`
/// (plus `build.rs`) except the names on [`ALLOWED`].
///
/// The predecessor of this gate asked the weaker question (is every name
/// mentioned also READ somewhere?), which was right while the levers existed.
/// They do not: every levered arm is deleted and every arm now runs
/// unconditionally, so a reintroduced `FCAST_*` name is either a resurrected
/// lever or prose describing a mechanism that is not there. Both are the
/// failure this gate exists to stop, and neither is something the compiler
/// can see, since the mentions live in comments.
///
/// The allow-list is for `FCAST_*` names that are NOT levers: build- and
/// test-harness knobs that gate no shipping behaviour. Adding to it needs
/// that argument written next to the entry.
///
/// SCOPE: `src/` and `build.rs`. `tests/` is deliberately out, because the
/// suites read their own harness knobs through local helpers (`env_u64`,
/// `env_secs`) that this rule cannot tell from prose.
#[test]
fn no_fcast_lever_name_survives_anywhere_in_src() {
    use std::collections::{BTreeMap, BTreeSet};

    /// `FCAST_LOOM` selects `cfg(loom)` for this package alone (`build.rs`),
    /// which swaps the `hands` primitives for the model checker's in a TEST
    /// build. It gates no shipping code path, so it is not a lever.
    const ALLOWED: &[&str] = &["FCAST_LOOM"];

    // Every `FCAST_<NAME>` token on one line, with its byte offset.
    fn levers_in(line: &str) -> Vec<(usize, String)> {
        const PREFIX: &str = "FCAST_";
        let bytes = line.as_bytes();
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(rel) = line[from..].find(PREFIX) {
            let at = from + rel;
            let mut end = at + PREFIX.len();
            while end < bytes.len()
                && (bytes[end].is_ascii_uppercase()
                    || bytes[end].is_ascii_digit()
                    || bytes[end] == b'_')
            {
                end += 1;
            }
            // `FCAST_*` in prose is the family, not a name.
            if end > at + PREFIX.len() {
                out.push((at, line[at..end].to_string()));
            }
            from = at + PREFIX.len();
        }
        out
    }

    // The scanner must actually find tokens, or an empty answer below would
    // read as a pass. The probe is ASSEMBLED rather than written out: a
    // literal here would be a real mention in a real src/ file and this very
    // test would report itself.
    let p = "FCAST_";
    assert_eq!(
        levers_in(&format!("read {p}NO_TICK and {p}X2, not {p}* or {p}lower")),
        vec![(5, format!("{p}NO_TICK")), (23, format!("{p}X2")),],
        "the token scanner is broken"
    );

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("build.rs")];
    let mut dirs = vec![root.join("src")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("the crate's src/ is readable") {
            let path = entry.expect("a readable directory entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                // `.rs` only, which also keeps merge leftovers (`lib.rs.orig`)
                // out of the answer.
                files.push(path);
            }
        }
    }
    assert!(
        files.len() > 10,
        "the src/ walk found {} files",
        files.len()
    );

    let mut scanned = 0usize;
    let mut mentions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        scanned += 1;
        let file = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        for (no, line) in text.lines().enumerate() {
            for (_, name) in levers_in(line) {
                if ALLOWED.contains(&name.as_str()) {
                    continue;
                }
                mentions
                    .entry(name)
                    .or_default()
                    .insert(format!("{file}:{}", no + 1));
            }
        }
    }
    assert!(
        scanned > 10,
        "only {scanned} files were readable, so the scan itself is broken"
    );

    let found: Vec<String> = mentions
        .iter()
        .map(|(name, sites)| {
            let sites: Vec<&str> = sites.iter().map(String::as_str).collect();
            format!("{name} ({})", sites.join(", "))
        })
        .collect();
    assert!(
        found.is_empty(),
        "the levers are gone; these `FCAST_*` names are back in src/. Delete the lever and \
         its prose (keeping WHY it existed), or add a non-lever knob to ALLOWED with the \
         argument for it:\n  {}",
        found.join("\n  ")
    );
}
