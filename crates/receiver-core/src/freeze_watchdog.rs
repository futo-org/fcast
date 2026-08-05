//! Receiver-level freeze watchdog: detection and the staged recovery decision.
//!
//! A silent pipeline wedge (FREEZE-DIAGN.md section 1: adaptivedemux2's single
//! output task permanently paused by a stray `GST_FLOW_FLUSHING`, one
//! decodebin3-multiqueue single-queue latched with it) leaves the receiver
//! believing playback is fine. The state stays PLAYING, nothing errors,
//! buffering never reports, downloads keep running, and the only observable is
//! that the playback position stops moving. Track selection is inert at the
//! demuxer from then on, so nothing the user does recovers it either.
//!
//! Only a FLUSHING seek fully heals that wedge (FREEZE-DIAGN.md section 6: it
//! is the one path that both resets the demuxer's flow combiner and clears the
//! multiqueue latch), so the ladder is: dump diagnostics and send a flushing
//! seek to the current position, then reload the item at that position, then
//! give up and report a media error. Capped per item so a permanently broken
//! stream cannot loop.
//!
//! The decision lives here as a pure state transition over sampled facts, with
//! no access to the pipeline, so every branch is unit-testable. The caller
//! samples once per progress tick and performs the action it gets back.

use std::time::{Duration, Instant};

use crate::MediaItemId;

/// How long the position must stay EXACTLY pinned before the watchdog acts,
/// and the length of each later stage's window. Everything ordinary that stops
/// the position (pause, rebuffer, preroll, seek) is excluded by state rather
/// than by waiting it out, so this only has to outlast sampling noise. It is
/// also the user's patience budget times three, since the ladder needs three
/// windows to reach the error.
pub(crate) const FREEZE_WINDOW: Duration = Duration::from_secs(5);

/// A position this close to the duration is treated as "at the end", where a
/// pinned position is the ordinary wait for EOS. A known upstream oggdemux
/// shape parks exactly AT the duration, which must not be recovered from.
const END_MARGIN: gst::ClockTime = gst::ClockTime::from_seconds(1);

/// Everything the decision needs, sampled by the caller once per tick.
///
/// Every field is an exclusion the detector owns, so the caller does no gating
/// of its own beyond skipping the position query when it would be ignored.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FreezeSample {
    pub now: Instant,
    /// Identity of the item the application considers current. A change resets
    /// the per-item recovery cap.
    pub item: MediaItemId,
    /// The receiver believes playback is running: the state machine is settled
    /// at PLAYING, which by construction also means no load, no buffering, no
    /// requested transition and no parked or in-flight seek.
    pub playing: bool,
    /// No async state change is in progress. A re-preroll legitimately does
    /// not advance the position.
    pub pipeline_settled: bool,
    /// A stream collection has been advertised for this load.
    pub have_media_info: bool,
    /// The application still has a load in flight (it clears this later than
    /// the state machine leaves its loading phase).
    pub loading: bool,
    /// An image decoded through the player pipeline. Stills park by design and
    /// animations loop, neither has an advancing position.
    pub image: bool,
    /// A live source (WHEP, fwebrtc, AirPlay mirror). Position semantics
    /// differ and a seek is refused outright.
    pub live: bool,
    /// The pipeline answered the seeking query with "seekable". A flushing
    /// seek is only available here.
    pub seekable: bool,
    /// Any seek-shaped operation the application is waiting on: an in-flight
    /// or parked pipeline seek, the optimistic scrub hold, a seek parked until
    /// seekability resolves, or an operation parked on a gapless cancel.
    pub seek_pending: bool,
    /// Current playback rate. Reverse playback counts down, so a pinned
    /// position means something else there.
    pub rate: f64,
    /// The position the user sees, `None` when the pipeline cannot answer.
    pub position: Option<gst::ClockTime>,
    pub duration: Option<gst::ClockTime>,
}

/// What the caller should do about the sample it just took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FreezeAction {
    None,
    /// Dump diagnostics and send a flushing seek to the current position.
    Seek {
        pinned_for: Duration,
    },
    /// The seek did not help. Reload the item at the current position.
    Reload {
        pinned_for: Duration,
    },
    /// Recovery is exhausted for this item. Report a media error.
    GiveUp {
        pinned_for: Duration,
    },
}

/// How far up the ladder this item has already been taken. The cap is per
/// ITEM, not per freeze: a stream that wedges every few seconds must reach the
/// error instead of alternating seek and reload forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Fresh,
    Sought,
    Reloaded,
    Spent,
}

#[derive(Debug, Clone, Copy)]
struct Pinned {
    position: gst::ClockTime,
    since: Instant,
}

#[derive(Debug)]
pub(crate) struct FreezeWatchdog {
    /// Lever: `FCAST_NO_FREEZE_WATCHDOG` (set = no detection, no recovery,
    /// old behavior).
    enabled: bool,
    item: MediaItemId,
    pinned: Option<Pinned>,
    stage: Stage,
    /// Seqnum of the dispatched recovery seek, so its failure report can be
    /// recognized in the log.
    recovery_seqnum: Option<gst::Seqnum>,
}

impl FreezeWatchdog {
    pub(crate) fn new() -> Self {
        Self {
            enabled: std::env::var_os("FCAST_NO_FREEZE_WATCHDOG").is_none(),
            item: 0,
            pinned: None,
            stage: Stage::Fresh,
            recovery_seqnum: None,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Fold one sample in and say what to do.
    pub(crate) fn poll(&mut self, sample: &FreezeSample) -> FreezeAction {
        if !self.enabled {
            return FreezeAction::None;
        }

        if sample.item != self.item {
            self.item = sample.item;
            self.stage = Stage::Fresh;
            self.pinned = None;
            self.recovery_seqnum = None;
        }

        let Some(position) = self.judgeable(sample) else {
            // Not judgeable: forget any pin so the time spent excluded (a
            // pause, a re-preroll) can never count as pinned playback.
            self.pinned = None;
            return FreezeAction::None;
        };

        // EXACTLY pinned, never merely slow: a starved sink clamps its
        // reported position to the last rendered buffer's stop time
        // (gstbasesink.c:5337-5341), so the wedge repeats one value forever
        // while anything still running moves at least a nanosecond.
        let pinned = match self.pinned {
            Some(pinned) if pinned.position == position => pinned,
            _ => {
                self.pinned = Some(Pinned {
                    position,
                    since: sample.now,
                });
                return FreezeAction::None;
            }
        };

        let pinned_for = sample.now.saturating_duration_since(pinned.since);
        if pinned_for < FREEZE_WINDOW {
            return FreezeAction::None;
        }

        // Restart the window: the next rung needs another full one, whether or
        // not the recovery just dispatched is ever confirmed.
        self.pinned = Some(Pinned {
            position,
            since: sample.now,
        });

        match self.stage {
            Stage::Fresh if sample.seekable => {
                self.stage = Stage::Sought;
                FreezeAction::Seek { pinned_for }
            }
            // No flushing seek is available on a stream the pipeline calls
            // unseekable, so that rung is skipped rather than wasted.
            Stage::Fresh | Stage::Sought => {
                self.stage = Stage::Reloaded;
                FreezeAction::Reload { pinned_for }
            }
            Stage::Reloaded => {
                self.stage = Stage::Spent;
                FreezeAction::GiveUp { pinned_for }
            }
            // Guarded in `judgeable`, kept total rather than panicking.
            Stage::Spent => FreezeAction::None,
        }
    }

    /// The position to judge, or `None` when this sample says nothing about a
    /// freeze.
    fn judgeable(&self, sample: &FreezeSample) -> Option<gst::ClockTime> {
        if self.stage == Stage::Spent {
            return None;
        }
        if !sample.playing || !sample.have_media_info {
            return None;
        }
        if sample.loading || sample.image || sample.live || sample.seek_pending {
            return None;
        }
        // Reverse playback counts the position DOWN, and a legitimate pin at 0
        // is how it ends. A high forward rate still advances, so it is judged
        // like any other. (Rate 0 is refused upstream of here, this also
        // covers it.)
        if sample.rate <= 0.0 {
            return None;
        }
        // An unsettled pipeline is not judgeable while nothing has been tried.
        // Once a recovery is in flight the gate has to lift: our own flushing
        // seek makes the pipeline unsettled, and a wedge that survives it
        // (the seek was refused, or its preroll can never complete) would
        // otherwise never escalate.
        if !sample.pipeline_settled && self.stage == Stage::Fresh {
            return None;
        }
        let position = sample.position?;
        // At or past the end, a pinned position is the wait for EOS.
        if let Some(duration) = sample.duration
            && position.nseconds().saturating_add(END_MARGIN.nseconds()) >= duration.nseconds()
        {
            return None;
        }
        Some(position)
    }

    /// The recovery reload starts a new load, which bumps the application's
    /// item id. Adopt it WITHOUT resetting the ladder: the cap is what stops a
    /// permanently wedging stream from looping through the recovery forever.
    pub(crate) fn note_recovery_reload(&mut self, item: MediaItemId) {
        self.item = item;
        self.pinned = None;
    }

    /// Remember which seek is ours, so a failure report for it can be logged
    /// as the refused recovery it is (the demuxer refuses a seek whose
    /// scheduler loop is stopped, FREEZE-DIAGN.md section 6).
    pub(crate) fn note_recovery_seek(&mut self, seqnum: gst::Seqnum) {
        self.recovery_seqnum = Some(seqnum);
    }

    pub(crate) fn is_recovery_seek(&self, seqnum: gst::Seqnum) -> bool {
        self.recovery_seqnum == Some(seqnum)
    }

    /// Diagnostic name of the rung this item is on.
    pub(crate) fn stage_name(&self) -> &'static str {
        match self.stage {
            Stage::Fresh => "fresh",
            Stage::Sought => "sought",
            Stage::Reloaded => "reloaded",
            Stage::Spent => "spent",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A position well clear of both ends of the test item.
    const MID: gst::ClockTime = gst::ClockTime::from_seconds(10);

    /// A healthy mid-item playing sample. Tests mutate the one field they are
    /// about, so a new exclusion cannot silently stop being exercised.
    fn playing(now: Instant, position: gst::ClockTime) -> FreezeSample {
        FreezeSample {
            now,
            item: 1,
            playing: true,
            pipeline_settled: true,
            have_media_info: true,
            loading: false,
            image: false,
            live: false,
            seekable: true,
            seek_pending: false,
            rate: 1.0,
            position: Some(position),
            duration: Some(gst::ClockTime::from_seconds(600)),
        }
    }

    /// Feed samples 100 ms apart over `span`, all reporting the same position,
    /// and return the first action that is not `None`.
    fn pin_for(
        watchdog: &mut FreezeWatchdog,
        start: Instant,
        position: gst::ClockTime,
        span: Duration,
    ) -> FreezeAction {
        pin_for_with(watchdog, start, position, span, |_| {})
    }

    /// [`pin_for`] with each sample adjusted, for the exclusion and item-id
    /// cases.
    fn pin_for_with(
        watchdog: &mut FreezeWatchdog,
        start: Instant,
        position: gst::ClockTime,
        span: Duration,
        adjust: impl Fn(&mut FreezeSample),
    ) -> FreezeAction {
        let mut elapsed = Duration::ZERO;
        while elapsed <= span {
            let mut sample = playing(start + elapsed, position);
            adjust(&mut sample);
            let action = watchdog.poll(&sample);
            if action != FreezeAction::None {
                return action;
            }
            elapsed += Duration::from_millis(100);
        }
        FreezeAction::None
    }

    #[test]
    fn advancing_playback_never_fires() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        for tick in 0..200u64 {
            let sample = playing(
                start + Duration::from_millis(100 * tick),
                gst::ClockTime::from_mseconds(100 * tick),
            );
            assert_eq!(watchdog.poll(&sample), FreezeAction::None, "tick {tick}");
        }
    }

    /// A single nanosecond of progress is progress: the predicate is exact
    /// pinning, not slowness, because a slow-but-alive pipeline must never be
    /// flushed out from under the user.
    #[test]
    fn a_barely_advancing_position_never_fires() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        for tick in 0..200u64 {
            let sample = playing(
                start + Duration::from_millis(100 * tick),
                gst::ClockTime::from_nseconds(MID.nseconds() + tick),
            );
            assert_eq!(watchdog.poll(&sample), FreezeAction::None, "tick {tick}");
        }
    }

    #[test]
    fn an_exact_pin_past_the_window_asks_for_the_flushing_seek() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        // Just under the window: still nothing.
        assert_eq!(
            pin_for(
                &mut watchdog,
                start,
                MID,
                FREEZE_WINDOW - Duration::from_millis(200)
            ),
            FreezeAction::None
        );
        let action = watchdog.poll(&playing(start + FREEZE_WINDOW, MID));
        assert!(
            matches!(action, FreezeAction::Seek { pinned_for } if pinned_for >= FREEZE_WINDOW),
            "{action:?}"
        );
        // And it does not re-fire on the very next tick.
        assert_eq!(
            watchdog.poll(&playing(
                start + FREEZE_WINDOW + Duration::from_millis(100),
                MID
            )),
            FreezeAction::None
        );
    }

    #[test]
    fn a_pin_that_survives_the_seek_escalates_then_gives_up() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        assert!(matches!(
            pin_for(&mut watchdog, start, MID, FREEZE_WINDOW),
            FreezeAction::Seek { .. }
        ));
        let second = start + FREEZE_WINDOW + Duration::from_millis(100);
        assert!(matches!(
            pin_for(&mut watchdog, second, MID, FREEZE_WINDOW),
            FreezeAction::Reload { .. }
        ));
        let third = second + FREEZE_WINDOW + Duration::from_millis(100);
        assert!(matches!(
            pin_for(&mut watchdog, third, MID, FREEZE_WINDOW),
            FreezeAction::GiveUp { .. }
        ));
        // Spent for good: no further action for this item, ever.
        let fourth = third + FREEZE_WINDOW + Duration::from_millis(100);
        assert_eq!(
            pin_for(&mut watchdog, fourth, MID, FREEZE_WINDOW * 4),
            FreezeAction::None
        );
    }

    /// The recovery reload bumps the application's item id. If that reset the
    /// ladder, a permanently wedging stream would alternate seek and reload
    /// forever instead of reaching the error.
    #[test]
    fn a_recovery_reload_keeps_the_per_item_cap() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        assert!(matches!(
            pin_for(&mut watchdog, start, MID, FREEZE_WINDOW),
            FreezeAction::Seek { .. }
        ));
        let second = start + FREEZE_WINDOW + Duration::from_millis(100);
        assert!(matches!(
            pin_for(&mut watchdog, second, MID, FREEZE_WINDOW),
            FreezeAction::Reload { .. }
        ));
        // The reload's own load bumps the item id.
        watchdog.note_recovery_reload(2);
        let third = second + FREEZE_WINDOW + Duration::from_millis(100);
        let action = pin_for_with(&mut watchdog, third, MID, FREEZE_WINDOW, |s| s.item = 2);
        assert!(matches!(action, FreezeAction::GiveUp { .. }), "{action:?}");
    }

    /// A genuinely new item does reset it (the freeze belonged to the media
    /// that is gone).
    #[test]
    fn a_new_item_rearms_the_ladder() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        assert!(matches!(
            pin_for(&mut watchdog, start, MID, FREEZE_WINDOW),
            FreezeAction::Seek { .. }
        ));
        let next = start + FREEZE_WINDOW + Duration::from_millis(100);
        let action = pin_for_with(&mut watchdog, next, MID, FREEZE_WINDOW, |s| s.item = 7);
        assert!(matches!(action, FreezeAction::Seek { .. }), "{action:?}");
    }

    /// Every exclusion, driven the same way: a pin held well past the window
    /// with that one field flipped must produce nothing.
    #[test]
    fn excluded_states_never_fire() {
        let cases: [(&str, fn(&mut FreezeSample)); 9] = [
            ("paused or transitioning", |s| s.playing = false),
            ("no media info", |s| s.have_media_info = false),
            ("load in flight", |s| s.loading = true),
            ("pipeline image", |s| s.image = true),
            ("live source", |s| s.live = true),
            ("seek pending", |s| s.seek_pending = true),
            ("unsettled pipeline", |s| s.pipeline_settled = false),
            ("reverse playback", |s| s.rate = -1.0),
            ("position unknown", |s| s.position = None),
        ];
        for (name, adjust) in cases {
            let mut watchdog = FreezeWatchdog::new();
            let start = Instant::now();
            assert_eq!(
                pin_for_with(&mut watchdog, start, MID, FREEZE_WINDOW * 3, adjust),
                FreezeAction::None,
                "{name}"
            );
        }
    }

    /// A buffering dip is a state exclusion, not a timeout: the state machine
    /// leaves settled PLAYING for its whole duration, and the pinned time it
    /// spans must not carry over into the resumed playback.
    #[test]
    fn a_buffering_dip_does_not_accumulate_pinned_time() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        // Playing and pinned for most of a window.
        pin_for(
            &mut watchdog,
            start,
            MID,
            FREEZE_WINDOW - Duration::from_secs(1),
        );
        // A long rebuffer at the same position (state machine not running).
        let dip = start + FREEZE_WINDOW;
        let action = pin_for_with(&mut watchdog, dip, MID, FREEZE_WINDOW * 4, |s| {
            s.playing = false;
        });
        assert_eq!(action, FreezeAction::None);
        // Playback resumes and reports the same position for a while. The
        // window must start over from the resume, not from before the dip.
        let resumed = dip + FREEZE_WINDOW * 4 + Duration::from_millis(100);
        assert_eq!(
            pin_for(
                &mut watchdog,
                resumed,
                MID,
                FREEZE_WINDOW - Duration::from_millis(200)
            ),
            FreezeAction::None
        );
    }

    /// The end of an item pins legitimately while it waits for EOS (and a
    /// known upstream oggdemux shape parks exactly AT the duration).
    #[test]
    fn a_pin_at_the_end_of_the_item_never_fires() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        let end = gst::ClockTime::from_seconds(600);
        let action = pin_for_with(&mut watchdog, start, end, FREEZE_WINDOW * 3, |s| {
            s.duration = Some(end);
        });
        assert_eq!(action, FreezeAction::None);
    }

    /// An unknown duration must not disable detection (a push-mode demuxer
    /// answers late), and a pin far from the end is judged normally.
    #[test]
    fn an_unknown_duration_still_detects() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        let action = pin_for_with(&mut watchdog, start, MID, FREEZE_WINDOW, |s| {
            s.duration = None
        });
        assert!(matches!(action, FreezeAction::Seek { .. }), "{action:?}");
    }

    /// A stream the pipeline calls unseekable cannot take the flushing seek,
    /// so it skips straight to the reload rung.
    #[test]
    fn an_unseekable_stream_goes_straight_to_the_reload() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        let action = pin_for_with(&mut watchdog, start, MID, FREEZE_WINDOW, |s| {
            s.seekable = false
        });
        assert!(matches!(action, FreezeAction::Reload { .. }), "{action:?}");
    }

    /// Once a recovery is in flight, an unsettled pipeline must NOT block the
    /// escalation: our own flushing seek unsettles the pipeline, and a seek
    /// whose preroll can never complete (the wedge survived) is exactly the
    /// case the reload exists for.
    #[test]
    fn an_unsettled_pipeline_after_the_seek_still_escalates() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        assert!(matches!(
            pin_for(&mut watchdog, start, MID, FREEZE_WINDOW),
            FreezeAction::Seek { .. }
        ));
        let after = start + FREEZE_WINDOW + Duration::from_millis(100);
        let action = pin_for_with(&mut watchdog, after, MID, FREEZE_WINDOW, |s| {
            s.pipeline_settled = false
        });
        assert!(matches!(action, FreezeAction::Reload { .. }), "{action:?}");
    }

    /// A successful recovery is invisible to the ladder beyond the spent rung:
    /// the position advances again, so nothing else fires.
    #[test]
    fn a_recovered_pipeline_stops_firing() {
        let mut watchdog = FreezeWatchdog::new();
        let start = Instant::now();
        assert!(matches!(
            pin_for(&mut watchdog, start, MID, FREEZE_WINDOW),
            FreezeAction::Seek { .. }
        ));
        let resumed = start + FREEZE_WINDOW + Duration::from_millis(100);
        for tick in 0..200u64 {
            let sample = playing(
                resumed + Duration::from_millis(100 * tick),
                gst::ClockTime::from_mseconds(10_000 + 100 * tick),
            );
            assert_eq!(watchdog.poll(&sample), FreezeAction::None, "tick {tick}");
        }
    }

    #[test]
    fn the_lever_disables_everything() {
        // The constructor reads the environment, so build a disabled one the
        // same way `new` would with the lever set.
        let mut watchdog = FreezeWatchdog {
            enabled: false,
            item: 0,
            pinned: None,
            stage: Stage::Fresh,
            recovery_seqnum: None,
        };
        assert!(!watchdog.enabled());
        let start = Instant::now();
        assert_eq!(
            pin_for(&mut watchdog, start, MID, FREEZE_WINDOW * 4),
            FreezeAction::None
        );
    }

    #[test]
    fn recovery_seek_seqnums_are_recognized() {
        let mut watchdog = FreezeWatchdog::new();
        let ours = gst::Seqnum::next();
        let foreign = gst::Seqnum::next();
        assert!(!watchdog.is_recovery_seek(ours));
        watchdog.note_recovery_seek(ours);
        assert!(watchdog.is_recovery_seek(ours));
        assert!(!watchdog.is_recovery_seek(foreign));
    }
}
