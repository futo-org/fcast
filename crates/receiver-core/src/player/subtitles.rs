//! The subtitle text-restore dance: every video load prerolls with the text
//! playbin flag off (a text branch built or reconfigured outside steady
//! PLAYING can wedge playsink/subtitleoverlay), and this module owns the
//! sequencing that restores text once the pipeline settles — including the
//! external-subtitle (`suburi`) variant, which additionally holds the
//! start/restore seek and enforces the requested selection.
//!
//! All of it runs on the application event loop: the timers below re-enter
//! through `Message::PlayerTimer`, never on their own task/thread, so every
//! dance step stays serialized with the rest of the player calls. The pump
//! entry points (`pump_subtitle_dance`, `subtitle_dance_streams_selected`)
//! are invoked by the application's bus-event handlers at the exact points
//! the sequencing was stress-validated at — dance actions deliberately run
//! AFTER the sender relays.

use std::time::{Duration, Instant};

use tracing::{debug, error, warn};

use super::{Player, PlayerState};
use crate::message::Message;

/// What the text-restore sequence of a load must end on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextIntent {
    /// Plain (no-suburi) load: once the pipeline settles, restore the
    /// subtitle stream decodebin3's suppressed auto-selection would have
    /// picked (or a change parked during the restore).
    Plain,
    /// Suburi load: select the external text stream once it appears and the
    /// pipeline settles.
    ExternalSelect,
    /// Suburi load with the external attached but not shown: restore the
    /// embedded stream with this id (stable across reloads), or none.
    ExternalAttached { restore_sid: Option<String> },
    /// No text handling at all (e.g. mirror streams): the text flag simply
    /// stays off for the whole load.
    Untracked,
}

/// The playback snapshot a load returns to once it prerolls.
#[derive(Debug, Clone, Copy)]
pub struct RestorePoint {
    pub position: gst::ClockTime,
    pub rate: f32,
}

/// How a load relates to what was playing before.
#[derive(Debug)]
pub enum LoadKind {
    /// A fresh media item; `start` is the play message's start position and
    /// rate (`None` for live sources — no post-preroll seek at all).
    Fresh { start: Option<RestorePoint> },
    /// An external-subtitle reload of the same item: return to the snapshot
    /// and stamp the reload-error grace window. A reload that must end
    /// paused additionally calls `set_pause_after_restore` (the reload
    /// itself always goes through Playing; see `pause_after_restore`).
    Reload { restore: RestorePoint },
}

/// Where the current load's text-restore sequence stands; the application
/// maps this to its protocol decisions (reject vs park a subtitle change,
/// hold a parked seek).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitlePhase {
    Idle,
    /// An external subtitle load is applying its own selection; competing
    /// subtitle changes would race it and seeks must stay held.
    ExternalInFlight,
    /// A plain load still owes its text restore; subtitle changes are parked
    /// as the restore target instead of applied.
    RestorePending,
}

/// A dance timer fired (routed back through the application loop so the
/// handling stays serialized with everything else).
#[derive(Debug)]
pub enum TimerEvent {
    /// Fired a few seconds after a suburi load: if the external text stream
    /// still hasn't appeared, the subtitle source failed.
    ExternalSubtitleCheck { generation: u64 },
    /// Stability poll for the settle sequence (playsink's text-branch build
    /// has no bus signal); re-scheduled while the pipeline reconfigures.
    Settle { generation: u64, attempt: u8 },
    /// Verify timer for a mid-preroll text deselect: if decodebin3 posted no
    /// `StreamsSelected` at all since the deselect went out, the event was
    /// silently swallowed by a collection-announcement race — re-send it.
    PrerollDeselectVerify {
        generation: u64,
        epoch: u64,
        attempt: u8,
    },
}

/// What the application must do in response to a dance timer.
#[derive(Debug, PartialEq, Eq)]
pub enum TimerAction {
    None,
    /// The external subtitle's text stream never appeared: remove it from
    /// the catalog, error the requesting sender and run the recovery reload
    /// (`Application::fail_external_subtitle`).
    ExternalSubtitleFailed,
}

/// The per-load state of the text-restore dance. Reset by `Player::load`
/// and whenever the player stops.
#[derive(Debug)]
pub(super) struct SubtitleDance {
    /// Invalidation token for all dance timers: bumped on every load and
    /// stop, so timers armed for a superseded load never act.
    generation: u64,
    /// The suburi of the current load, if any; also the error-attribution
    /// key for a failing subtitle source.
    suburi: Option<String>,
    /// What the restore sequence must end on (see `TextIntent`).
    intent: TextIntent,
    /// A suburi load is in flight and its text stream has not been handled
    /// yet (select/deselect on appearance). Still set once preroll finishes
    /// ⇒ the subtitle source failed silently.
    external_sub_pending: bool,
    /// The in-flight reload started from a paused pipeline: pause again once
    /// playback restarts. Holding the pipeline at Paused through the reload
    /// instead is not an option — selecting the fresh text stream during a
    /// preroll that stops at Paused hangs the preroll (observed with
    /// playbin3; going through Playing works).
    pause_after_restore: bool,
    /// The text playbin flag has been restored after the load disabled it
    /// (see `Job::SetUri`/`Job::EnableText`).
    text_enabled: bool,
    /// The stability poll confirmed the restore seek fully settled; the text
    /// flag may be enabled now (never right after a flush).
    ready_for_text: bool,
    /// A stability poll is already scheduled for this load.
    settle_scheduled: bool,
    /// Invalidation token for the mid-preroll deselect verify timer: bumped
    /// whenever a new verify is armed, so only the newest one may act.
    preroll_deselect_epoch: u64,
    /// Counts `StreamsSelected` confirmations seen during the flag-off
    /// external-subtitle preroll; compared against
    /// `preroll_confirm_seq_at_send` to detect a mid-preroll deselect that
    /// decodebin3 silently dropped (no confirmation at all).
    preroll_confirm_seq: u64,
    /// `preroll_confirm_seq` at the moment the last mid-preroll deselect was
    /// sent.
    preroll_confirm_seq_at_send: u64,
    /// A plain load still owes its text restore (see `TextIntent::Plain`
    /// and `pump_plain_text_restore`).
    plain_restore_pending: bool,
    /// A subtitle change parked while the plain-load restore was pending:
    /// `Some(None)` is a parked deselect, `Some(Some(sid))` a parked switch
    /// (stream IDs are stable across the collections of one load, indices
    /// are not). `None` means no override — the restore keeps whatever
    /// decodebin3 would have auto-selected.
    plain_subtitle_override: Option<Option<String>>,
    /// A subtitle deselect arrived while paused and was parked: playsink's
    /// text-chain teardown deadlocks without flowing data. Applied at
    /// resume.
    parked_paused_deselect: bool,
    /// The external subtitle selection has been confirmed and a final settle
    /// poll is running; the `pause_after_restore` re-pause must wait for
    /// that settle rather than fire from the transiently-"stable" confirm
    /// window (pausing while playsink is still building the text branch
    /// wedges the pause into a preroll that blocks on the sparse stream).
    repause_via_settle: bool,
    /// The post-preroll seek held until the pipeline can run it: a fresh
    /// load's start seek, or a reload's restore snapshot. External loads
    /// hold it further, until their text stream is handled.
    held_restore: Option<RestorePoint>,
    /// When the last external-subtitle-related reload started; errors
    /// blaming the (re-used) main URI shortly after are attributed to the
    /// superseded pipeline instance's teardown, not the fresh load.
    last_external_reload: Option<Instant>,
}

impl SubtitleDance {
    pub(super) fn new() -> Self {
        Self {
            generation: 0,
            suburi: None,
            intent: TextIntent::Untracked,
            external_sub_pending: false,
            pause_after_restore: false,
            text_enabled: true,
            ready_for_text: true,
            settle_scheduled: false,
            preroll_deselect_epoch: 0,
            preroll_confirm_seq: 0,
            preroll_confirm_seq_at_send: 0,
            plain_restore_pending: false,
            plain_subtitle_override: None,
            parked_paused_deselect: false,
            repause_via_settle: false,
            held_restore: None,
            last_external_reload: None,
        }
    }

    /// Back to the at-rest state (media stopped / about to be replaced).
    /// Bumps the generation so armed timers die.
    pub(super) fn reset(&mut self) {
        let generation = self.generation + 1;
        *self = Self::new();
        self.generation = generation;
    }
}

/// How long an external subtitle's text stream may take to appear after a
/// load before the requesting sender is told `ResourceNotFound`.
const EXTERNAL_SUB_TIMEOUT: Duration = Duration::from_secs(5);
/// How long after an external reload same-URI/no-URI pipeline errors are
/// attributed to the superseded instance's teardown (see
/// `Player::in_external_reload_grace`).
const EXTERNAL_RELOAD_ERROR_GRACE: Duration = Duration::from_secs(3);
/// Poll interval for the pipeline to finish rebuilding after the subtitle
/// selection (playsink's text-branch construction has no bus signal and can
/// block until the stream's next cue).
const SETTLE_POLL: Duration = Duration::from_millis(500);
/// Give up waiting for stability after this many polls and run the seek
/// anyway (best effort; the pipeline is likely stuck regardless).
const SETTLE_MAX_ATTEMPTS: u8 = 20;
/// How long to wait for decodebin3 to confirm a mid-preroll text deselect
/// before re-sending it. Stream collections arrive in a sub-millisecond
/// burst, so by the time this fires the burst that swallowed the original
/// event is long over.
const PREROLL_DESELECT_RETRY: Duration = Duration::from_millis(250);
/// Give up re-sending after this many attempts and let the
/// `ExternalSubtitleCheck` timeout fail the subtitle source instead.
const PREROLL_DESELECT_MAX_ATTEMPTS: u8 = 8;

impl Player {
    /// Load a new main URI with the text-restore sequencing this module
    /// implements: `intent` says what the sequence must end on, `kind`
    /// carries the post-preroll seek (a fresh item's start position or a
    /// reload's snapshot to return to).
    pub fn load(&mut self, uri: &str, suburi: Option<&str>, intent: TextIntent, kind: LoadKind) {
        self.subtitles.reset();
        self.subtitles.suburi = suburi.map(str::to_string);
        self.subtitles.external_sub_pending = suburi.is_some();
        // EVERY tracked load starts with the text flag disabled (see
        // Job::SetUri); it is restored once the pipeline settles. Suburi
        // loads run the external restore sequence (which also holds the
        // restore seek); plain loads run the lighter plain restore,
        // re-selecting whatever subtitle stream decodebin3 would have
        // auto-picked during preroll.
        self.subtitles.text_enabled = false;
        self.subtitles.ready_for_text = false;
        self.subtitles.plain_restore_pending = matches!(intent, TextIntent::Plain);
        match kind {
            LoadKind::Fresh { start } => {
                self.subtitles.held_restore = start;
            }
            LoadKind::Reload { restore } => {
                self.subtitles.held_restore = Some(restore);
                self.subtitles.last_external_reload = Some(Instant::now());
            }
        }
        self.subtitles.intent = intent;

        // The sub source can fail without any bus error (uridecodebin3 only
        // logs sub-item activation failures), so a bad subtitle URL simply
        // never produces a text stream. Check back after a timeout and let
        // the application error the requesting sender if it never
        // materialized.
        if suburi.is_some() {
            self.arm_timer(EXTERNAL_SUB_TIMEOUT, |generation| {
                TimerEvent::ExternalSubtitleCheck { generation }
            });
        }

        self.set_uri(uri, suburi);
    }

    fn arm_timer(&self, delay: Duration, event: impl FnOnce(u64) -> TimerEvent) {
        let event = event(self.subtitles.generation);
        let msg_tx = self.msg_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            msg_tx.send(Message::PlayerTimer(event));
        });
    }

    /// Handle a fired dance timer. Returns what the application must do in
    /// response (timers for superseded loads return `None`).
    pub fn handle_timer(&mut self, event: TimerEvent) -> TimerAction {
        match event {
            TimerEvent::ExternalSubtitleCheck { generation } => {
                if generation == self.subtitles.generation && self.subtitles.external_sub_pending {
                    // The text stream never showed up and no pipeline error
                    // pinpointed the subtitle URL: treat the subtitle source
                    // as failed and let the application recover.
                    warn!("External subtitle track never appeared within the timeout");
                    return TimerAction::ExternalSubtitleFailed;
                }
            }
            TimerEvent::Settle {
                generation,
                attempt,
            } => {
                if generation == self.subtitles.generation {
                    // Touching the pipeline while it is still transitioning
                    // (seek dance, text-branch build) freezes playback, so
                    // keep polling until it reports no in-flight transition.
                    if !self.is_pipeline_stable() && attempt < SETTLE_MAX_ATTEMPTS {
                        debug!(attempt, "Pipeline still reconfiguring; re-polling");
                        self.schedule_settle(attempt + 1);
                    } else {
                        debug!("Pipeline settled; continuing the text restore sequence");
                        self.subtitles.ready_for_text = true;
                        // The settle confirmed genuine stability, so it is
                        // now safe to re-pause (the transiently-"stable"
                        // confirm window is over).
                        self.subtitles.repause_via_settle = false;
                        self.pump_subtitle_dance(false);
                        self.maybe_pause_after_restore();
                    }
                }
            }
            TimerEvent::PrerollDeselectVerify {
                generation,
                epoch,
                attempt,
            } => {
                // Only the newest verify may act, and only while the
                // flag-off preroll is still in flight. A confirmation
                // having arrived since the send means the deselect was
                // applied (a confirm showing text again would have
                // triggered a fresh send of its own); none at all means
                // decodebin3 swallowed the event — send it again.
                if generation == self.subtitles.generation
                    && epoch == self.subtitles.preroll_deselect_epoch
                    && self.subtitles.external_sub_pending
                    && !self.subtitles.text_enabled
                    && !matches!(
                        self.player_state(),
                        PlayerState::Paused | PlayerState::Playing
                    )
                    && self.subtitles.preroll_confirm_seq
                        == self.subtitles.preroll_confirm_seq_at_send
                {
                    if attempt < PREROLL_DESELECT_MAX_ATTEMPTS {
                        debug!(attempt, "Mid-preroll text deselect unconfirmed; re-sending");
                        self.send_preroll_deselect(attempt + 1);
                    } else {
                        warn!("Mid-preroll text deselect never confirmed; giving up");
                    }
                }
            }
        }
        TimerAction::None
    }

    fn schedule_settle(&mut self, attempt: u8) {
        // A settle is in flight; the `pause_after_restore` re-pause must wait
        // for it to confirm genuine stability, not fire from the transiently-
        // "stable" window right after enabling text / selecting the stream.
        self.subtitles.repause_via_settle = true;
        self.arm_timer(SETTLE_POLL, |generation| TimerEvent::Settle {
            generation,
            attempt,
        });
    }

    /// Send the mid-preroll text deselect and arm a verify timer for it.
    ///
    /// The deselect must go out IMMEDIATELY when a confirmation shows text
    /// selected: a text stream that stays selected while playsink won't
    /// consume it (flag off) hard-wedges the preroll in well under 200ms
    /// (a deliberately delayed deselect was confirmed applied by decodebin3
    /// yet the preroll stayed dead). But an immediate send can also race
    /// the NEXT stream collection of the announcement burst (~1/100 under
    /// stress: the event is silently dropped, no confirmation ever posts,
    /// preroll wedges). Hence the verify: if nothing got confirmed within
    /// the retry window, send it again — the burst is over by then.
    fn send_preroll_deselect(&mut self, attempt: u8) {
        if let Err(err) = self.deselect_text_mid_preroll() {
            error!(?err, "Failed to deselect text during preroll");
        }
        self.subtitles.preroll_confirm_seq_at_send = self.subtitles.preroll_confirm_seq;
        self.subtitles.preroll_deselect_epoch += 1;
        let epoch = self.subtitles.preroll_deselect_epoch;
        self.arm_timer(PREROLL_DESELECT_RETRY, |generation| {
            TimerEvent::PrerollDeselectVerify {
                generation,
                epoch,
                attempt,
            }
        });
    }

    /// Identify the text track created by the external `suburi` source, if
    /// any. uridecodebin3 gives the subtitle source its own stream-id
    /// namespace, so the external text stream's id does not share the main
    /// streams' prefix. The last-text-stream fallback applies ONLY when
    /// prefix detection is structurally impossible (no non-text stream to
    /// derive the main prefix from): before the sub source activates, a
    /// MAIN text stream must never be mistaken for the external one —
    /// the dance acting on it releases the restore seek into the sub item's
    /// activation window, and that flush wedges the pipeline (observed
    /// under stress: seek never completes → 5s timeout → spurious
    /// ResourceNotFound; collections can also arrive incomplete, so "a text
    /// stream exists" says nothing about the external).
    ///
    /// Only meaningful while a suburi is loaded; callers must gate on that.
    pub fn external_subtitle_track_idx(&self) -> Option<u32> {
        let main_prefix = self
            .streams
            .iter()
            .filter(|s| !s.inner.stream_type().contains(gst::StreamType::TEXT))
            .find_map(|s| s.inner.stream_id())
            .and_then(|id| id.split('/').next().map(str::to_owned));

        let mut last_text = None;
        let mut foreign_text = None;
        for (idx, stream) in self.streams.iter().enumerate() {
            if !stream.inner.stream_type().contains(gst::StreamType::TEXT) {
                continue;
            }
            last_text = Some(idx as u32);
            if let (Some(prefix), Some(id)) = (main_prefix.as_deref(), stream.inner.stream_id())
                && id.split('/').next() != Some(prefix)
            {
                foreign_text = Some(idx as u32);
            }
        }

        if main_prefix.is_some() {
            foreign_text
        } else {
            foreign_text.or(last_text)
        }
    }

    /// Where the current load's text-restore sequence stands (see
    /// `SubtitlePhase`).
    pub fn subtitle_phase(&self) -> SubtitlePhase {
        if self.subtitles.external_sub_pending {
            SubtitlePhase::ExternalInFlight
        } else if self.subtitles.plain_restore_pending {
            SubtitlePhase::RestorePending
        } else {
            SubtitlePhase::Idle
        }
    }

    /// Whether an external subtitle (`suburi`) is attached to the current
    /// load. While one is, ANY flush races the text-branch reconfiguration
    /// and errors the suburi source, so subtitle changes must suppress the
    /// re-emit refresh.
    fn external_attached(&self) -> bool {
        self.subtitles.suburi.is_some()
    }

    /// Apply a plain (non-reload) subtitle stream selection through
    /// `TrackOps`, suppressing the re-emit flush while an external subtitle
    /// is attached (see `external_attached`). Same return as
    /// `request_track_change`.
    pub fn change_subtitle_stream(&mut self, stream_idx: Option<u32>) -> bool {
        if self.external_attached() {
            self.request_subtitle_change_no_refresh(stream_idx)
        } else {
            self.request_track_change(super::TrackKind::Subtitle, stream_idx)
        }
    }

    /// Park a subtitle change as the plain-load restore target (see
    /// `SubtitlePhase::RestorePending`): `None` keeps decodebin3's
    /// auto-pick, `Some(None)` is a parked deselect, `Some(Some(sid))` a
    /// parked switch.
    pub fn park_restore_subtitle_override(&mut self, target: Option<String>) {
        self.subtitles.plain_subtitle_override = Some(target);
    }

    /// Park a subtitle deselect that arrived while paused (playsink's
    /// text-chain teardown deadlocks without flowing data); applied once
    /// the pipeline actually runs again.
    pub fn park_paused_subtitle_deselect(&mut self) {
        self.subtitles.parked_paused_deselect = true;
    }

    /// A newer selection supersedes a parked paused deselect.
    pub fn clear_parked_paused_subtitle_deselect(&mut self) {
        self.subtitles.parked_paused_deselect = false;
    }

    /// Consume the parked paused deselect at the started-playing edge; the
    /// caller applies it as an ordinary subtitle change.
    pub fn take_parked_paused_subtitle_deselect(&mut self) -> bool {
        std::mem::take(&mut self.subtitles.parked_paused_deselect)
    }

    /// A pause landed while a load was in flight and would be stomped by the
    /// collection-time auto-play (loads always go through Playing): record
    /// the intent so the restore path returns to Paused once the load
    /// settles; an explicit resume or the next load clears it.
    pub fn set_pause_after_restore(&mut self) {
        self.subtitles.pause_after_restore = true;
    }

    /// An explicit resume overrides a pending return-to-Paused.
    pub fn clear_pause_after_restore(&mut self) {
        self.subtitles.pause_after_restore = false;
    }

    /// Run the held start/restore seek once the pipeline can answer the
    /// seekability query. The external dance calls this itself at its
    /// release point; for everything else the application drives it from
    /// its media-info updates (gated on `subtitle_phase`, so an external
    /// load keeps holding the seek).
    pub fn maybe_run_start_seek(&mut self) {
        if self.seekable
            && let Some(restore) = self.subtitles.held_restore.take()
        {
            self.seek_and_set_rate(restore.position, restore.rate);
        }
    }

    /// Whether `failed_uri` blames the in-flight external subtitle source:
    /// the subtitle failed, not the media itself, so playback degrades to
    /// running without it instead of stopping.
    pub fn error_is_external_subtitle(&self, failed_uri: Option<&str>) -> bool {
        self.subtitles.external_sub_pending
            && failed_uri.is_some_and(|uri| self.subtitles.suburi.as_deref() == Some(uri))
    }

    /// Whether an external-subtitle reload started recently: the reload
    /// re-uses the item's URI, so the superseded-load check cannot tell the
    /// OLD pipeline instance's teardown errors (a wedged suburi pipeline
    /// dies noisily, sometimes blaming the main URI or nothing at all)
    /// apart from a genuine failure of the fresh load. Errors in this
    /// window are tolerated; a genuine failure degrades to a hung load
    /// rather than a spurious fatal error.
    pub fn in_external_reload_grace(&self) -> bool {
        self.subtitles
            .last_external_reload
            .is_some_and(|t| t.elapsed() < EXTERNAL_RELOAD_ERROR_GRACE)
    }

    /// The active external subtitle failed: take the playback snapshot the
    /// recovery reload must return to (the held restore seek if it never
    /// ran, else the live position), clearing the dance's pending state.
    pub fn take_failed_external_snapshot(&mut self) -> (Option<gst::ClockTime>, f32, bool) {
        self.subtitles.external_sub_pending = false;
        let (position, rate) = match self.subtitles.held_restore.take() {
            Some(restore) => (Some(restore.position), restore.rate),
            None => (self.get_position(), self.rate() as f32),
        };
        let was_paused = std::mem::take(&mut self.subtitles.pause_after_restore);
        (position, rate, was_paused)
    }

    /// The external subtitle failed but its catalog entry was already gone;
    /// just stop waiting for its text stream.
    pub fn clear_external_sub_pending(&mut self) {
        self.subtitles.external_sub_pending = false;
    }

    /// A reload that started from a paused pipeline returns to Paused only
    /// once the whole restore sequence is done (position seek, subtitle
    /// selection, refresh): pausing earlier would re-introduce
    /// pause/preroll-time reconfiguration, which is exactly what the
    /// sequencing avoids.
    pub fn maybe_pause_after_restore(&mut self) {
        if self.subtitles.pause_after_restore
            && !self.subtitles.external_sub_pending
            && !self.subtitles.plain_restore_pending
            && !self.subtitles.repause_via_settle
            && self.subtitles.held_restore.is_none()
            && !self.has_pending_track_work()
            && self.player_state() == PlayerState::Playing
            && self.is_pipeline_stable()
        {
            debug!("Reload restore complete; returning to Paused");
            self.subtitles.pause_after_restore = false;
            self.pause();
        }
    }

    /// The post-relay `StreamsSelected` hook: count the confirmation for the
    /// mid-preroll deselect verify, undo a text selection confirmed during
    /// the flag-off external preroll, and reconcile the dance against the
    /// confirmation. Runs AFTER the application relayed the selection to
    /// senders — that ordering is stress-validated.
    pub fn subtitle_dance_streams_selected(&mut self) {
        // While the text-flag-off external-subtitle load is still
        // prerolling, playbin3 re-runs its auto-selection for EVERY stream
        // collection (media can advertise streams across several), and a
        // selected text stream stalls the sub source's play-item
        // activation, wedging the preroll within well under 200ms. Undo any
        // text selection the moment it is confirmed; the verify timer armed
        // by the send catches the event getting swallowed by a
        // collection-announcement race (see `send_preroll_deselect`).
        //
        // Deliberately EXTERNAL-ONLY: embedded text arrives in the FIRST
        // collection, where this deselect races the initial branch wiring
        // (observed preroll wedge) — and it is not needed there: an
        // unrouted embedded stream is decoupled by decodebin3's multiqueue
        // and cannot stall the preroll. The plain-load restore leaves the
        // auto-selection alone and builds its branch with the settle-time
        // flag flip instead.
        if self.subtitles.external_sub_pending
            && !self.subtitles.text_enabled
            && !matches!(
                self.player_state(),
                PlayerState::Paused | PlayerState::Playing
            )
        {
            self.subtitles.preroll_confirm_seq += 1;
            if self.current_subtitle_stream.is_some() {
                debug!("Deselecting text confirmed during the external-subtitle preroll");
                self.send_preroll_deselect(0);
            }
        }

        // Reconcile a pending selection against this confirmation, and let
        // a paused-reload restore that was waiting on it pause again.
        self.pump_subtitle_dance(true);
        self.maybe_pause_after_restore();
    }

    /// Once the external text stream shows up in a stream collection,
    /// enforce the requested selection state: select it
    /// (`TextIntent::ExternalSelect`), or deselect it
    /// (`TextIntent::ExternalAttached` — decodebin3 auto-selects a lone
    /// text stream, which would make "add but don't show" show it).
    pub fn pump_subtitle_dance(&mut self, selection_confirmed: bool) {
        if !self.subtitles.external_sub_pending {
            // No external in flight — but a plain flag-off load may still
            // owe its text restore (same triggers drive both sequences).
            self.pump_plain_text_restore(selection_confirmed);
            return;
        }
        let (select_on_load, restore_subtitle_sid) = match &self.subtitles.intent {
            TextIntent::ExternalSelect => (true, None),
            TextIntent::ExternalAttached { restore_sid } => (false, restore_sid.clone()),
            // `external_sub_pending` is only set for suburi loads, which
            // always carry an external intent.
            TextIntent::Plain | TextIntent::Untracked => {
                self.subtitles.external_sub_pending = false;
                return;
            }
        };
        let Some(idx) = self.external_subtitle_track_idx() else {
            // The external text stream always arrives in a *second* stream
            // collection (the sub source only activates after the main
            // source links); keep waiting.
            return;
        };
        // Touching the pipeline while the (re)loaded pipeline is still
        // prerolling wedges the preroll (observed: the second collection can
        // arrive before preroll completes). Stay pending; this is retried on
        // every relayed state change until the pipeline settles.
        if !matches!(
            self.player_state(),
            PlayerState::Paused | PlayerState::Playing
        ) {
            debug!("Deferring external subtitle selection until the pipeline settles");
            return;
        }

        // The requested end state; the text-without-video constraint stays
        // in force: never select a subtitle while video is deselected.
        let want = if self.current_video_stream.is_none() {
            None
        } else if select_on_load {
            Some(idx)
        } else {
            // The external stays attached as the suburi but an embedded/no
            // track is shown (e.g. after switching subtitles off it); select
            // that embedded track instead. Embedded stream ids are stable
            // across reloads of the same media.
            restore_subtitle_sid
                .as_deref()
                .and_then(|sid| self.stream_idx_by_id(sid))
        };

        // Restore the playback position FIRST, while the text playbin flag
        // is still off: the pipeline is settled and has NO text branch, so
        // the flushing seek is an ordinary, well-tested video+audio seek.
        // Once playsink starts building a text branch (which has no bus
        // signal and can block until the stream's next sparse cue), ANY
        // flush freezes the play item — so after this point we never flush
        // again. Text is enabled only once the stability poll confirms the
        // seek dance fully settled: flipping the flag right after a flush is
        // the other observed freeze trigger.
        if !self.subtitles.ready_for_text {
            self.maybe_run_start_seek();
            if !self.subtitles.settle_scheduled {
                self.subtitles.settle_scheduled = true;
                self.schedule_settle(0);
            }
            return;
        }

        // The text playbin flag was disabled for this load (see Job::SetUri)
        // so no text branch could wedge the preroll; restore it now and send
        // the selection along with it (a bare flag change posts no
        // confirmation of its own).
        //
        // Deliberately NO re-emit flush on the selection — it goes through
        // the no-refresh request (see above: no flushes once text is enabled;
        // the first cue renders at its next boundary). With the text flag
        // off, decodebin3 still auto-selects the external text stream during
        // preroll (the flag only keeps playsink from building a branch for
        // it), so `current_*` often already matches a select=true request and
        // no selection event would be confirmed at all. Only an actual change
        // gets sent — and only an actual change produces a StreamsSelected.
        if !self.subtitles.text_enabled {
            debug!("Re-enabling the text flag for the external subtitle");
            self.subtitles.text_enabled = true;
            self.enable_text_flag();
            if want == self.current_subtitle_stream {
                // Nothing to change; the flag flip alone makes playsink
                // build (or not) the text branch.
                debug!(?want, "External subtitle selection already as wanted");
                self.subtitles.external_sub_pending = false;
                self.schedule_settle(0);
            } else {
                debug!(?want, "Sending the external subtitle selection");
                // Serialized through TrackOps like any other selection: the
                // pipeline was just confirmed settled, so this dispatches
                // right away in practice; a not-quite-quiet pipeline parks it
                // briefly instead of racing a reconfiguration.
                self.request_subtitle_change_no_refresh(want);
                // The confirming StreamsSelected finishes the job below.
            }
            return;
        }

        // From here on, act only on confirmations of the selection we sent.
        if !selection_confirmed {
            return;
        }

        if want == self.current_subtitle_stream {
            debug!(?want, "External subtitle selection confirmed");
            self.subtitles.external_sub_pending = false;
            self.schedule_settle(0);
        } else {
            // A racing selection overrode ours; re-enforce. Bounded: each
            // confirmation triggers at most one correction.
            debug!(?want, "Re-enforcing the external subtitle selection");
            self.request_subtitle_change_no_refresh(want);
        }
    }

    /// The plain-load counterpart of the external dance: every video load
    /// prerolls with the text flag off (see `Job::SetUri`), so no text
    /// branch can be built (or reconfigured) outside steady PLAYING — the
    /// subtitleoverlay livelock window. decodebin3's text auto-selection is
    /// suppressed during the preroll (unrouted embedded text is harmlessly
    /// decoupled by the multiqueue); once a stability poll confirms the
    /// load settled, the flag flip alone makes playsink build the branch
    /// for it, in steady PLAYING with data flowing — the ordering the dance
    /// reliably survives. Subtitle changes parked during the restore
    /// (`plain_subtitle_override`) are applied BEFORE the flag flip, while
    /// no branch exists to reconfigure. Driven by the same triggers as the
    /// external variant: state changes, `StreamsSelected` confirmations and
    /// the settle poll.
    fn pump_plain_text_restore(&mut self, selection_confirmed: bool) {
        if !self.subtitles.plain_restore_pending {
            return;
        }
        // Touching the pipeline while it is still prerolling is what this
        // sequencing exists to avoid; wait for a steady state first.
        if !matches!(
            self.player_state(),
            PlayerState::Paused | PlayerState::Playing
        ) {
            return;
        }

        // Unlike the external path there is no restore seek to hold — a
        // plain load's start seek runs from the media-info updates as
        // before, while the text flag is still off (the safe ordering).
        // Text is enabled only once the stability poll confirms the load
        // fully settled: flipping the flag during a transition is an
        // observed freeze trigger.
        if !self.subtitles.ready_for_text {
            if !self.subtitles.settle_scheduled {
                self.subtitles.settle_scheduled = true;
                self.schedule_settle(0);
            }
            return;
        }

        // The target: a parked explicit change if there is one, otherwise
        // the first text stream — the same default decodebin3's
        // auto-selection would have picked (it was suppressed for this
        // load via `auto-select-text`, so nothing is selected yet). The
        // text-without-video constraint stays in force; recomputed on
        // every pass — a video disable can land between confirmations.
        let want = if self.current_video_stream.is_none() {
            None
        } else {
            match &self.subtitles.plain_subtitle_override {
                Some(target) => target.as_deref().and_then(|sid| self.stream_idx_by_id(sid)),
                None => self.first_text_stream_idx(),
            }
        };

        // The two mutating steps below build or reconfigure the text branch
        // and need FLOWING data: a branch built in a paused pipeline is the
        // original subtitleoverlay deadlock, and a paused text teardown is
        // the known worker-poisoning one. A user pause inside the restore
        // window parks the restore; the resume's state change re-runs it.
        // (Reload-restores hold Playing until the restore completes —
        // `maybe_pause_after_restore` waits on `plain_restore_pending` —
        // so only explicit user pauses land here.)
        if self.player_state() != PlayerState::Playing {
            return;
        }

        if !self.subtitles.text_enabled {
            // Restore the flag and send the selection along with it — the
            // exact ordering the external path stress-validated: with the
            // flag already on, the selection activates the stream and
            // builds its playsink branch in one flow, in steady PLAYING
            // with data flowing. (Selecting BEFORE the flip would park the
            // stream, and activating a parked stream is its own known
            // wedge.) No re-emit flush: the branch build is signal-less
            // and a flush racing it freezes the play item; the first cue
            // renders at its next boundary instead.
            debug!("Re-enabling the text flag after the plain load");
            self.subtitles.text_enabled = true;
            self.enable_text_flag();
            if want == self.current_subtitle_stream {
                // Nothing to select (no text streams, or video-less media);
                // the flag flip alone completes the restore.
                debug!(?want, "No subtitle selection to restore");
            } else {
                debug!(?want, "Selecting the default subtitle after the settle");
                self.request_subtitle_change_no_refresh(want);
            }
            // Completion is decided by the stability poll: a track switch
            // hitting the pipeline while the fresh branch is still wiring
            // up wedges it (observed: a composed audio+subtitle switch
            // ~450ms after the build froze playback). The restore — and
            // with it the window that parks competing subtitle changes —
            // stays pending until a poll confirms genuine stability.
            self.schedule_settle(0);
            return;
        }

        // Flag restored. Only act between stability polls: while one is in
        // flight it re-runs this when it confirms (or gives up), so any
        // late parked change below only ever hits a settled pipeline.
        if self.subtitles.repause_via_settle {
            return;
        }
        if want != self.current_subtitle_stream {
            // A change parked after the flag flip; now that the branch
            // settled this is an ordinary subtitle switch.
            if !self.has_pending_track_work() {
                debug!(?want, "Applying the late-parked subtitle target");
                self.request_subtitle_change_no_refresh(want);
                self.schedule_settle(0);
            }
            return;
        }
        if selection_confirmed {
            // Converged on a fresh confirmation; verify stability once more
            // before releasing the window (the branch build is signal-less).
            self.schedule_settle(0);
            return;
        }
        debug!("Plain-load text restore complete");
        self.subtitles.plain_restore_pending = false;
    }
}
