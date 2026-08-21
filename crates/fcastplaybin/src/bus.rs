//! The GStreamer bus: event emission, error attribution and the
//! translation of bus messages into [`crate::PlaybinEvent`]s.

use std::sync::{Arc, atomic::Ordering};

use gst::prelude::*;
use tracing::{debug, warn};

use crate::{
    FcastPlaybin, Inner,
    api::{AfterCancel, ErrorOrigin, ExternalSubId, MessageHook, PlaybinEvent},
    decisions,
    jobs::Job,
    routing::StreamKind,
    selection,
    selection::TrackSelection,
};

/// A bus error's source input, classified by the generation-tagged inputs.
/// The internal superset of [`ErrorOrigin`]. External-input errors are
/// consumed by the crate and need their id for the fail/re-arm decision.
enum ErrorSource {
    Main,
    External(ExternalSubId),
    Stale,
    /// A pre-armed next input that has not activated yet (its generation is
    /// ahead of the current one). Consumed internally. The prepare is
    /// abandoned and reported as [`PlaybinEvent::PreparedFailed`].
    Prepared(u64),
    Unknown,
}

/// The generation ladder of [`Inner::classify_error_src`], with the gst
/// ancestry walk supplied as a per-input bool so the attribution can be
/// pinned without a pipeline. `inputs` yields (the src is from this input,
/// the input's generation, its external id).
fn classify_matched_input(
    current: u64,
    inputs: impl IntoIterator<Item = (bool, u64, Option<ExternalSubId>)>,
) -> ErrorSource {
    for (is_from_input, generation, external) in inputs {
        if !is_from_input {
            continue;
        }
        if generation > current {
            return ErrorSource::Prepared(generation);
        }
        if generation != current {
            return ErrorSource::Stale;
        }
        return match external {
            Some(id) => ErrorSource::External(id),
            None => ErrorSource::Main,
        };
    }
    ErrorSource::Unknown
}

impl Inner {
    /// Deliver an event to the caller's handler, a no-op until
    /// [`FcastPlaybin::set_event_handler`] installs one. Stamped with the
    /// current load generation.
    pub(crate) fn emit(&self, event: PlaybinEvent) {
        self.emit_with_generation(event, self.current_generation());
    }

    /// [`Self::emit`] for an event that belongs to a load the pipeline has not
    /// adopted, where `current_generation` would misattribute it.
    ///
    /// The one caller is the async load's own failure report. A load that dies
    /// before storing its generation leaves the current generation naming the
    /// replaced item, and the caller gates events by the generation
    /// `load_async` returned, so a report stamped with the current one would
    /// be dropped as stale.
    ///
    /// Deliberately not fixed by storing the generation earlier. The pipeline
    /// genuinely is still the previous item until the new core is installed,
    /// and every other event stamped in between would then lie.
    pub(crate) fn emit_with_generation(&self, event: PlaybinEvent, generation: u64) {
        let callback = self.events.lock().clone();
        if let Some(callback) = callback {
            callback(event, generation);
        }
    }

    /// Queue [`Job::ClearStateFailure`] for an error message this crate
    /// consumes instead of surfacing. Called from the bus translation, i.e. a
    /// streaming thread, so it only queues. Reading and re-committing the
    /// pipeline's state belongs on the worker.
    ///
    /// Errors that do reach the caller are deliberately left alone. The caller
    /// decides what a real error means (a teardown, usually), and unlatching
    /// under it would hide a pipeline that genuinely cannot run.
    fn queue_state_unlatch(&self) {
        if std::env::var_os("FCAST_NO_ERROR_STATE_UNLATCH").is_some() {
            return;
        }
        self.queue_job(Job::ClearStateFailure);
    }

    /// A pipeline settle re-drives the text link, the way the two arms below
    /// re-drive the text drain.
    ///
    /// [`Inner::poll_text_policy`] refuses to link below a settled `>= PAUSED`
    /// ([`decisions::text_may_link`]), and the only asks (a decodebin3 pad
    /// appearing, a video chain join) fire during a load's async preroll where
    /// the gate refuses them. Without this, an item that selects a text track
    /// and never leaves PAUSED renders no cue at all. Reaching PLAYING hides
    /// the gap because that edge brings joins and drains with polls attached.
    ///
    /// Both callers matter. A PAUSED-to-PAUSED seek posts no state-changed
    /// (see the `AsyncDone` arm), and a load settles through state-changed.
    ///
    /// Narrow on purpose. Only when the seat is genuinely empty while a
    /// subtitle is applied and not explicitly off, so a settled pipeline whose
    /// text is already joined queues nothing. Asked for, not performed. This
    /// runs on a streaming thread and the surgery belongs to the decider.
    ///
    /// Lever: `FCAST_NO_SETTLE_TEXT_POLL`.
    fn request_text_policy_poll_on_settle(&self, current: gst::State, pending: gst::State) {
        if std::env::var_os("FCAST_NO_SETTLE_TEXT_POLL").is_some() {
            return;
        }
        if !decisions::text_may_link(current, pending) {
            return;
        }
        // Routing then selection, the crate's lock order (see `Inner::routing`).
        let (unseated, seated) = {
            let routing = self.routing.lock();
            let text = || routing.routed.iter().filter(|r| r.kind == StreamKind::Text);
            (
                text().any(|r| r.downstream.is_none()),
                text().any(|r| r.downstream.is_some()),
            )
        };
        if !unseated || seated {
            return;
        }
        {
            let selection = self.selection.lock();
            if selection.subtitle_sid().is_none() || selection.subtitle_explicitly_off() {
                return;
            }
        }
        // [`Inner::request_text_policy_poll`]'s coalescing, without its
        // `FCAST_INLINE_TEXT_POLL` arm. That lever runs the surgery on the
        // asking thread, and this thread is never allowed to.
        if self.poll_queued.swap(true, Ordering::SeqCst) {
            self.poll_policy_coalesced.fetch_add(1, Ordering::SeqCst);
            return;
        }
        debug!("a pipeline settle found a text stream routed with its seat empty; polling");
        if !self.queue_job(Job::PollTextPolicy) {
            // No decider left to clear the bit, and a bit left set silences
            // every later poll (see `Inner::request_text_policy_poll`).
            self.poll_queued.store(false, Ordering::SeqCst);
        }
    }

    /// Classify a bus message source by the generation-tagged inputs.
    fn classify_error_src(&self, src: Option<&gst::Object>) -> ErrorSource {
        let Some(src) = src else {
            return ErrorSource::Unknown;
        };
        let generation = self.current_generation();
        let routing = self.routing.lock();
        classify_matched_input(
            generation,
            routing.inputs.iter().map(|input| {
                let is_from_input = src == input.element.upcast_ref::<gst::Object>()
                    || src.has_as_ancestor(&input.element);
                (
                    is_from_input,
                    input.generation,
                    input.external.as_ref().map(|external| external.id),
                )
            }),
        )
    }

    /// Translate a bus message into its typed event, applying the crate's
    /// filters. Per-element state changes and foreign ASYNC_DONEs are
    /// dropped, external-input collections are swallowed, and errors from
    /// elements no longer in the pipeline are discarded.
    fn translate_message(&self, msg: &gst::Message) -> Option<PlaybinEvent> {
        use gst::MessageView;

        let pipeline_obj = self.pipeline.upcast_ref::<gst::Object>();
        let event = match msg.view() {
            MessageView::Eos(_) => {
                // With a prepared next input linked, no pipeline EOS should
                // exist between items. One arriving means the gapless handoff
                // missed. Surface it so the caller's ordinary end-of-stream
                // advance takes over. The next load's reset cleans the input.
                if self.prepared.lock().is_some() {
                    warn!("pipeline EOS with a prepared next input: gapless handoff missed");
                }
                PlaybinEvent::EndOfStream
            }
            MessageView::Error(error) => {
                if let Some(src) = msg.src()
                    && src != pipeline_obj
                    && !src.has_as_ancestor(&self.pipeline)
                {
                    debug!(
                        src = %src.name(),
                        "Dropping error from element no longer in the current pipeline"
                    );
                    // It was still a child when it posted, so the pipeline
                    // carries the latch even though the element has left.
                    self.queue_state_unlatch();
                    return None;
                }
                // External subtitle input errors are consumed here (re-arm or
                // a typed failure event), never surfaced as pipeline errors.
                let origin = match self.classify_error_src(msg.src()) {
                    ErrorSource::External(id) => {
                        self.handle_external_error(id, &error.error(), error.debug());
                        self.queue_state_unlatch();
                        return None;
                    }
                    ErrorSource::Prepared(generation) => {
                        // The pre-armed next input died before activating. The
                        // current item is unaffected. Drop the prepare, tell
                        // the caller, and let its ordinary end-of-stream
                        // advance load the item normally.
                        warn!(
                            generation,
                            error = %error.error(),
                            debug = ?error.debug(),
                            "prepared next input failed before activation"
                        );
                        self.queue_job(Job::CancelPrepared {
                            notify: false,
                            after: AfterCancel::Nothing,
                        });
                        self.queue_state_unlatch();
                        self.emit(PlaybinEvent::PreparedFailed { generation });
                        return None;
                    }
                    ErrorSource::Main => ErrorOrigin::Main,
                    ErrorSource::Stale => ErrorOrigin::Stale,
                    ErrorSource::Unknown => ErrorOrigin::Unknown,
                };
                // Diagnostic only. Supersession is decided by the event's
                // generation and attribution by `origin`, not by this URI.
                let failed_uri = msg
                    .src()
                    .and_then(|src| src.dynamic_cast_ref::<gst::URIHandler>())
                    .and_then(|handler| handler.uri())
                    .map(|uri| uri.to_string());
                PlaybinEvent::Error {
                    origin,
                    error: error.error(),
                    failed_uri,
                }
            }
            MessageView::Warning(warning) => PlaybinEvent::Warning {
                error: warning.error(),
                src: msg.src().map(|src| src.name().to_string()),
                debug: warning.debug().map(|debug| debug.to_string()),
            },
            MessageView::Tag(tag) => PlaybinEvent::Tags(tag.tags()),
            MessageView::Buffering(buffering) => {
                // The prepared next input buffers ahead while the CURRENT
                // item plays; its levels must not drive the caller's
                // buffering state machine. Once it activates it is the main
                // input and its messages flow normally.
                if self.message_from_prepared_input(msg) {
                    debug!(
                        percent = buffering.percent(),
                        "dropping buffering from the prepared next input"
                    );
                    return None;
                }
                PlaybinEvent::Buffering(buffering.percent())
            }
            MessageView::StateChanged(change) => {
                if !msg.src().map(|s| s == pipeline_obj).unwrap_or(false) {
                    return None;
                }
                // Every pipeline state edge re-attempts the postponed
                // text-branch work (see [`Job::DrainTextWork`]). On the bus
                // this only queues. The worker does the blocking part.
                if self.has_deferred_text_work() {
                    self.queue_job(Job::DrainTextWork);
                }
                // ... and the postponed text LINK, off the message's own
                // fields rather than a re-query: they are exactly what the
                // link gate reads, and they cannot race the commit that
                // posted them.
                self.request_text_policy_poll_on_settle(change.current(), change.pending());
                PlaybinEvent::StateChanged {
                    old: change.old(),
                    current: change.current(),
                    pending: change.pending(),
                }
            }
            MessageView::RequestState(state) => {
                let state = state.requested_state();
                debug!(?state, "State requested");
                PlaybinEvent::RequestState(state)
            }
            MessageView::StreamCollection(collection) => {
                if self.message_from_external_input(msg) {
                    debug!(
                        src = ?msg.src().map(|s| s.name()),
                        "Ignoring a partial stream collection from an external subtitle input"
                    );
                    return None;
                }
                let collection = collection.stream_collection();
                // The prepared next input's collection belongs to the next
                // item. Hold it back and deliver it at activation, after
                // PreparedActivated and stamped with the new generation
                // (the input-posted form is caught by ancestry, the
                // decodebin3-posted form by its stream ids).
                //
                // Must stay ahead of the decodebin3 filter below. The gapless
                // handoff needs the next item's collection as early as the
                // prepared input can post it.
                if self.message_from_prepared_input(msg) || self.collection_is_prepared(&collection)
                {
                    debug!("holding the prepared next input's stream collection");
                    if let Some(prepared) = self.prepared.lock().as_mut() {
                        prepared.pending_collection = Some(collection);
                    }
                    return None;
                }
                // Only decodebin3's collection is the merged one, and only
                // its stream ids may appear in a `SELECT_STREAMS` sent to
                // decodebin3. Several elements post partial collections, each
                // naming a single stream, interleaved with the merged ones.
                //
                // Feeding partials to the selection engine makes the
                // collection appear to shrink, so reconciliation reads empty
                // slots as deselection and actively deselects tracks the
                // caller never touched.
                //
                // Matching the current core also drops a collection from a
                // decodebin3 the load already superseded.
                // A/B lever for bisecting regressions without a rebuild.
                let from_db3 = std::env::var_os("FCAST_NO_DB3_COLLECTION_FILTER").is_some() || {
                    let core = self.core.lock();
                    match (core.as_ref(), msg.src()) {
                        (Some(core), Some(src)) => src == core.db3.upcast_ref::<gst::Object>(),
                        _ => false,
                    }
                };
                if !from_db3 {
                    debug!(
                        src = ?msg.src().map(|s| s.name()),
                        "ignoring a partial stream collection that is not decodebin3's merged one"
                    );
                    return None;
                }
                // Cache the collection's video ids before the caller can
                // react to the event. `select_streams` classifies a no-video
                // selection with them.
                {
                    let mut routing = self.routing.lock();
                    routing.collection_video_ids = collection
                        .iter()
                        .filter(|s| s.stream_type().contains(gst::StreamType::VIDEO))
                        .filter_map(|s| s.stream_id().map(|id| id.to_string()))
                        .collect();
                }
                // The selection engine reconciles against the new collection
                // before the caller can react to the event.
                let streams = collection
                    .iter()
                    .filter_map(|s| {
                        let sid = s.stream_id()?.to_string();
                        let typ = s.stream_type();
                        let kind = if typ.contains(gst::StreamType::VIDEO) {
                            StreamKind::Video
                        } else if typ.contains(gst::StreamType::AUDIO) {
                            StreamKind::Audio
                        } else if typ.contains(gst::StreamType::TEXT) {
                            StreamKind::Text
                        } else {
                            return None;
                        };
                        Some(selection::CollectionStream { sid, kind })
                    })
                    .collect();
                self.selection.lock().collection_changed(streams);
                PlaybinEvent::StreamCollection(collection)
            }
            MessageView::StreamsSelected(streams) => {
                // The prepared next input's report about itself must not be
                // read as the live pipeline's selection. A stream-aware
                // adaptive demuxer posts its own streams-selected the moment
                // it reaches PAUSED, ahead of any boundary. Consumers below
                // speak about the item that is decoding, matching the
                // treatment of its buffering, duration and collection.
                //
                // Untreated, the report names exactly the prepared input's
                // stream ids, `try_activate_prepared` reads it as the gapless
                // switch and runs the activation with nothing linked, the
                // still-playing input is removed, and the demuxer dies of
                // `not-linked`. Pull-driven parse chains never announce a
                // selection of their own, so only adaptive sources hit this.
                if self.adaptive_prepare_hold() && self.message_from_prepared_input(msg) {
                    debug!("dropping the prepared next input's own selection report");
                    return None;
                }
                let mut video = None;
                let mut audio = None;
                let mut subtitle = None;
                let mut all_ids = Vec::new();

                for stream in streams.streams() {
                    let typ = stream.stream_type();
                    let id = stream.stream_id().map(|id| id.to_string());
                    if let Some(id) = &id {
                        all_ids.push(id.clone());
                    }

                    if typ.contains(gst::StreamType::VIDEO) {
                        video = id;
                    } else if typ.contains(gst::StreamType::AUDIO) {
                        audio = id;
                    } else if typ.contains(gst::StreamType::TEXT) {
                        subtitle = id;
                    }
                }

                // An upstream-selection demuxer reports only its own streams,
                // so absence of an external input's text there must not
                // deselect it. Merge the crate-owned slot back in (cache-only
                // read, this runs on a streaming thread). Same lever as the
                // dispatch split.
                if subtitle.is_none() && *self.upstream_selection.lock() == Some(true) {
                    let kept = self.selection.lock().subtitle_sid();
                    if let Some(sid) = kept {
                        let is_external = self.routing.lock().inputs.iter().any(|input| {
                            input.external.is_some() && input.stream_ids().contains(&sid)
                        });
                        if is_external {
                            debug!(%sid, "keeping the external subtitle an upstream report cannot speak about");
                            all_ids.push(sid.clone());
                            subtitle = Some(sid);
                        }
                    }
                }

                // Track the upstream-owned active set (every report, minus
                // external-input sids) so the dispatch split can tell a real
                // upstream change from a no-op. An adaptive demuxer only
                // confirms an activation edge, so a no-op send would leave the
                // engine awaiting a confirmation that cannot come.
                {
                    let mut upstream_ids: Vec<String> = {
                        let routing = self.routing.lock();
                        all_ids
                            .iter()
                            .filter(|sid| {
                                !routing.inputs.iter().any(|input| {
                                    input.external.is_some() && input.stream_ids().contains(sid)
                                })
                            })
                            .cloned()
                            .collect()
                    };
                    upstream_ids.sort();
                    *self.last_upstream_ids.lock() = upstream_ids;
                }

                // A selection naming the prepared input's streams is the
                // gapless switch. Adopt the next item's generation and
                // deliver its held-back collection first, so this selection
                // event arrives in a fresh load's order and stamping.
                self.try_activate_prepared(&all_ids);

                // The hold release must follow the replay decision below so
                // it can wait for the realigning seek. Releasing here renders
                // a just-selected external against the wrong origin. See
                // `Inner::release_owed_hold`. The lever restores the old
                // (unconditional, earlier) position for A/B comparison.
                let inline_hold_release = std::env::var_os("FCAST_NO_OWED_HOLD_RELEASE").is_some();
                if inline_hold_release {
                    self.unblock_selected_externals(&all_ids, None);
                }

                let seqnum = msg.seqnum();
                // The previously applied subtitle, for the selection-time
                // replay below. Tracked here rather than read off the engine,
                // whose `applied` is optimistic (set at dispatch) and by
                // confirmation time already names the new target.
                let previous_subtitle =
                    std::mem::replace(&mut *self.last_applied_subtitle.lock(), subtitle.clone());
                // Record what applied (and settle/overtake the in-flight
                // dispatch) before the caller sees the event. The caller's
                // pump then dispatches any re-assertion or queued work.
                self.selection.lock().streams_selected(
                    seqnum,
                    &TrackSelection {
                        video: video.clone(),
                        audio: audio.clone(),
                        subtitle: subtitle.clone(),
                    },
                );

                // Selection-time replay, the pad-reuse counterpart of the
                // join-time one. Switching between text streams makes
                // decodebin3 swap the stream on the already-linked output
                // pad, so no join (and no join-time replay) ever fires. The
                // replay restarts an external whose task died deselected and
                // re-aligns its timeline, so a selection that moves onto an
                // external with the branch already live queues it here. A
                // fresh join sees an unlinked branch and keeps its join-time
                // replay. A same-sid re-assertion is skipped so a redundant
                // SELECT_STREAMS cannot blink the current cue.
                // The external whose hold release the replay below owes, if
                // any (see `Inner::release_owed_hold`).
                let mut owed_release: Option<(ExternalSubId, u32)> = None;
                if let Some(sid) = &subtitle
                    && previous_subtitle.as_deref() != Some(sid.as_str())
                    // A/B lever for diagnosing switch regressions without a
                    // rebuild.
                    && std::env::var_os("FCAST_NO_SELECTION_REPLAY").is_none()
                {
                    let (target, branch_live) = {
                        let routing = self.routing.lock();
                        let branch_live = routing
                            .routed
                            .iter()
                            .any(|r| r.kind == StreamKind::Text && r.downstream.is_some());
                        let target = routing.inputs.iter().find_map(|input| {
                            let external = input.external.as_ref()?;
                            input.stream_ids().contains(sid).then_some((
                                external.id,
                                external.epoch,
                                external.last_origin,
                                external.task_dead,
                            ))
                        });
                        (target, branch_live)
                    };
                    if let Some((id, epoch, last_origin, task_dead)) = target {
                        let (_, origin) = self.video_timeline();
                        // With every text branch parked there is no pad swap
                        // to wait on and no join to replay from, and a drained
                        // external whose multiqueue slot was reclaimed has no
                        // pad carrying its sid at all. Only this re-push
                        // brings it back.
                        // Lever: `FCAST_NO_PARKED_SELECTION_REPLAY`.
                        let parked_needs_push = !branch_live
                            && std::env::var_os("FCAST_NO_PARKED_SELECTION_REPLAY").is_none();
                        if task_dead || origin != last_origin || parked_needs_push {
                            // The input's cues would render shifted, so the
                            // destructive flush-replay must run before
                            // anything wrong reaches the screen.
                            debug!(
                                ?id,
                                sid,
                                %origin,
                                %last_origin,
                                task_dead,
                                branch_live,
                                "the selection moved onto a dead, differently-timed or slotless external; replaying it"
                            );
                            // The release of this input's hold belongs to the
                            // replay, and only if the job is really on its
                            // way. A failed send leaves nothing that could
                            // discharge it. The per-resource in-flight bit is
                            // read so a duplicate replay collapses here rather
                            // than in `replay_subtitle`.
                            let already = !self.replay_inflight.lock().insert((id, epoch));
                            let sent = if already {
                                debug!(
                                    ?id,
                                    epoch,
                                    "a replay for this input is already queued or in flight; the \
                                     selection does not add a second"
                                );
                                true
                            } else {
                                let sent = self.queue_job(Job::ReplaySub {
                                    id,
                                    epoch,
                                    attempt: 0,
                                });
                                if !sent {
                                    self.replay_inflight.lock().remove(&(id, epoch));
                                }
                                sent
                            };
                            // Owed either way. The hold is discharged by the
                            // outcome of a replay for this `(id, epoch)`
                            // (`Inner::release_owed_hold`), and a suppressed
                            // duplicate carries the same key. The bit is read
                            // HERE and the owing is written by the call below,
                            // so the rival's outcome can settle in between and
                            // find nothing owed; `unblock_selected_externals`
                            // re-reads the bit at its tail for exactly that
                            // window.
                            if sent && !inline_hold_release {
                                owed_release = Some((id, epoch));
                            }
                        } else {
                            // Same timeline. A still-alive input delivers on
                            // its own, and a flush now races the very swap
                            // this selection started. The verification
                            // replays only if nothing arrives.
                            debug!(
                                ?id,
                                sid,
                                "the selection moved onto an external with a live text branch; arming its replay check"
                            );
                            self.arm_replay_verification(id, epoch, 0);
                        }
                    }
                }

                // An external held blocked until selected may flow now (see
                // `ExternalInput::hold_until_selected`), except the one whose
                // realigning replay was just queued. That one waits for the
                // seek (see `Inner::release_owed_hold`).
                if !inline_hold_release {
                    self.unblock_selected_externals(&all_ids, owed_release);
                }

                PlaybinEvent::StreamsSelected {
                    video,
                    audio,
                    subtitle,
                    seqnum,
                }
            }
            MessageView::ClockLost(_) => PlaybinEvent::ClockLost,
            MessageView::AsyncDone(_) => {
                if !msg.src().map(|s| s == pipeline_obj).unwrap_or(false) {
                    return None;
                }
                // Settle an in-flight refresh flush (attribution by
                // exclusivity, see the selection module docs).
                self.selection.lock().refresh_done();
                // An async settle is a state edge too (a PAUSED-to-PAUSED
                // seek posts no state-changed), so it also re-attempts the
                // postponed text-branch work.
                if self.has_deferred_text_work() {
                    self.queue_job(Job::DrainTextWork);
                }
                // ... and the postponed text link. The state has to be READ
                // here (an async settle carries none), which is sound
                // precisely because this message says the transition is over.
                let (_, current, pending) = self.pipeline.state(gst::ClockTime::ZERO);
                self.request_text_policy_poll_on_settle(current, pending);
                PlaybinEvent::AsyncDone
            }
            MessageView::DurationChanged(_) => {
                // A prefetching prepared input refines the next item's
                // duration, which says nothing about the item playing now.
                // Like its buffering levels, dropped until it activates.
                if self.message_from_prepared_input(msg) {
                    debug!("dropping duration-changed from the prepared next input");
                    return None;
                }
                // Past a performed swap the prepared input is the only linked
                // upstream, so the re-query this event asks for would be
                // answered by the next item and latch its duration onto the
                // item still playing. Drop it. The activation resets the
                // caller's duration anyway, and the new item posts its own
                // duration-changed.
                //
                // Minimal lock scope on purpose. This runs on the posting
                // (streaming) thread and the guard is released before the log.
                let activating = self.swap_gate.state.lock().activation_pending();
                if let Some(generation) = activating {
                    debug!(
                        generation,
                        "dropping duration-changed inside the gapless activation window"
                    );
                    return None;
                }
                PlaybinEvent::DurationChanged
            }
            MessageView::Latency(_) => {
                // An element's latency changed. The pipeline must re-query and
                // redistribute latency or the change never takes effect. Runs
                // on the worker, not this posting (streaming) thread (see
                // `Job::RecalculateLatency`).
                self.queue_job(Job::RecalculateLatency);
                return None;
            }
            MessageView::Element(element) => {
                let s = element.structure()?;
                match s.name().as_str() {
                    // fimagedec announces what it is decoding (format,
                    // dimensions, animated or still) for load classification.
                    "fcast-image-stream" => PlaybinEvent::ImageStream(s.to_owned()),
                    // sabrumpsrc reports server-directed backoffs (0 = ended).
                    "sabrump-status" => PlaybinEvent::SourceBackoff {
                        remaining_ms: s.get::<i64>("backoff-ms").ok()?.max(0) as u64,
                    },
                    _ => return None,
                }
            }
            _ => return None,
        };
        Some(event)
    }
}

impl FcastPlaybin {
    /// Own the bus and deliver typed [`PlaybinEvent`]s through `events`
    /// instead. Call at most once, before driving playback. Translation runs
    /// as a bus SYNC handler on the posting (streaming) thread, so the
    /// callback must be cheap and non-blocking (forward into a channel).
    /// Worker feedback ([`PlaybinEvent::Loaded`], seek outcomes) arrives
    /// through the same callback. The callback's second argument is the
    /// generation of the load the event belongs to (see
    /// [`load_async`](Self::load_async)).
    ///
    /// `hook`, when given, gets first look at every raw message (also on the
    /// posting thread) for caller-specific traffic like `NeedContext`.
    /// Returning `true` consumes the message.
    pub fn set_event_handler(
        &self,
        hook: Option<MessageHook>,
        events: impl Fn(PlaybinEvent, u64) + Send + Sync + 'static,
    ) {
        *self.inner.events.lock() = Some(Arc::new(events));
        // A strong clone here would cycle pipeline -> bus -> handler.
        let weak = Arc::downgrade(&self.inner);
        self.bus().set_sync_handler(move |_, msg| {
            // This upgrade may turn out to be the LAST strong reference, and
            // it dies on the posting (streaming) thread. `Inner::drop` is
            // written for exactly that; see the comment above `impl Drop for
            // Inner`.
            if let Some(inner) = weak.upgrade() {
                if let Some(hook) = &hook
                    && hook(msg)
                {
                    return gst::BusSyncReply::Drop;
                }
                if let Some(event) = inner.translate_message(msg) {
                    inner.emit(event);
                }
            }
            gst::BusSyncReply::Drop
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorSource, ExternalSubId, classify_matched_input};

    const MAIN: Option<ExternalSubId> = None;

    /// The 5-way attribution ladder, pinned per rung. Misattribution is
    /// user-visible: a prepared or stale input's death surfaced as `Main`
    /// shows a spurious playback error for an item that is playing fine.
    #[test]
    fn error_attribution_names_each_generation_rung() {
        // Ahead of the current generation: a pre-armed next input, consumed
        // internally and reported as PreparedFailed with ITS generation.
        assert!(matches!(
            classify_matched_input(5, [(true, 6, MAIN)]),
            ErrorSource::Prepared(6)
        ));
        // Behind: a superseded input still leaving the pipeline.
        assert!(matches!(
            classify_matched_input(5, [(true, 4, MAIN)]),
            ErrorSource::Stale
        ));
        // Current generation with an external id: the fail/re-arm decision
        // consumes it, never the caller.
        assert!(matches!(
            classify_matched_input(5, [(true, 5, Some(ExternalSubId(3)))]),
            ErrorSource::External(ExternalSubId(3))
        ));
        // The current main input, the one origin a caller tears down over.
        assert!(matches!(
            classify_matched_input(5, [(true, 5, MAIN)]),
            ErrorSource::Main
        ));
        // No input claims the src at all: unattributable.
        assert!(matches!(
            classify_matched_input(5, [(false, 5, MAIN), (false, 6, MAIN)]),
            ErrorSource::Unknown
        ));
        assert!(matches!(
            classify_matched_input(5, std::iter::empty::<(bool, u64, Option<ExternalSubId>)>()),
            ErrorSource::Unknown
        ));
    }

    /// The ladder acts on the FIRST input that claims the src and never
    /// reads past it, so a later input cannot steal the attribution.
    #[test]
    fn error_attribution_stops_at_the_first_claiming_input() {
        assert!(matches!(
            classify_matched_input(5, [(false, 5, MAIN), (true, 4, MAIN), (true, 5, MAIN)]),
            ErrorSource::Stale
        ));
        // The ahead check outranks the behind check on the same input, so a
        // prepare is never misread as stale drainage.
        assert!(matches!(
            classify_matched_input(5, [(true, 9, Some(ExternalSubId(1)))]),
            ErrorSource::Prepared(9)
        ));
    }
}
