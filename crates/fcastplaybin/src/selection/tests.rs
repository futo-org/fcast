use super::*;

fn sel(video: Option<&str>, audio: Option<&str>, subtitle: Option<&str>) -> TrackSelection {
    TrackSelection {
        video: video.map(str::to_string),
        audio: audio.map(str::to_string),
        subtitle: subtitle.map(str::to_string),
    }
}

fn collection(streams: &[(&str, StreamKind)]) -> Vec<CollectionStream> {
    streams
        .iter()
        .map(|(sid, kind)| CollectionStream {
            sid: sid.to_string(),
            kind: *kind,
        })
        .collect()
}

/// The standard A/V+text collection most tests run against.
fn avt_collection() -> Vec<CollectionStream> {
    collection(&[
        ("v0", StreamKind::Video),
        ("a0", StreamKind::Audio),
        ("a1", StreamKind::Audio),
        ("t0", StreamKind::Text),
        ("t1", StreamKind::Text),
    ])
}

fn ctx(quiet: bool, paused: bool) -> PumpCtx {
    PumpCtx {
        gate: SelectionGate {
            quiet,
            paused,
            seekable: true,
        },
        externals_attached: false,
        externals: Vec::new(),
        // These cases predate the split and model db3-owned mode.
        upstream_owns: false,
        now: Instant::now(),
    }
}

/// [`ctx`] with the pump's clock pinned, for the cases that drive the
/// bounded subtitle holdback.
fn ctx_at(quiet: bool, paused: bool, now: Instant) -> PumpCtx {
    PumpCtx {
        now,
        ..ctx(quiet, paused)
    }
}

/// Upstream-selection mode, where decodebin3 posts no confirmations of its
/// own and the crate owes the caller every answer.
fn ctx_upstream(quiet: bool, paused: bool) -> PumpCtx {
    PumpCtx {
        upstream_owns: true,
        ..ctx(quiet, paused)
    }
}

/// A request for what is ALREADY applied still has to be answered in
/// upstream-selection mode: nothing else ever will, and the caller keeps
/// showing the previous track until it hears back.
#[test]
fn an_already_satisfied_request_is_confirmed_once_in_upstream_mode() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
    );

    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
    assert_eq!(
        engine.pump(&ctx_upstream(true, false)),
        Some(Command::ConfirmApplied(sel(
            Some("v0"),
            Some("a0"),
            Some("t0")
        ))),
    );
    // Exactly once per request.
    assert_eq!(engine.pump(&ctx_upstream(true, false)), None);
}

/// A request naming an external that has NOT materialized is NOT satisfied
/// by whatever happens to be on screen.
#[test]
fn a_request_for_an_unmaterialized_external_is_not_confirmed() {
    const EXT: ExternalSubId = ExternalSubId(9);
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
    );

    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(EXT));
    // Nothing advertised for it yet: no answer, and the request survives.
    assert_eq!(engine.pump(&ctx_upstream(true, false)), None);
    assert_eq!(engine.pump(&ctx_upstream(true, false)), None);

    // It materializes, and the REAL dispatch answers the request.
    let mut collection = avt_collection();
    collection.push(CollectionStream {
        sid: "ext0".into(),
        kind: StreamKind::Text,
    });
    engine.collection_changed(collection);
    let ctx = PumpCtx {
        upstream_owns: true,
        ..ctx_ext(true, false, &[(EXT, &["ext0"])])
    };
    assert_eq!(
        engine.pump(&ctx),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a0"),
            Some("ext0")
        ))),
    );
    // Consumed by that dispatch: no confirmation follows it.
    let seqnum = gst::Seqnum::next();
    engine.selection_dispatched(seqnum, sel(Some("v0"), Some("a0"), Some("ext0")));
    engine.streams_selected(seqnum, &sel(Some("v0"), Some("a0"), Some("ext0")));
    assert_eq!(engine.pump(&ctx), None);
}

/// A request for the external that IS already applied stays confirmable:
/// the acceptance case, and the reason the gate tests resolvability rather
/// than "is an external".
#[test]
fn a_request_for_the_already_applied_external_is_confirmed() {
    const EXT: ExternalSubId = ExternalSubId(9);
    let mut engine = SelectionEngine::default();
    let mut collection = avt_collection();
    collection.push(CollectionStream {
        sid: "ext0".into(),
        kind: StreamKind::Text,
    });
    engine.collection_changed(collection);
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("ext0")),
    );

    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(EXT));
    let ctx = PumpCtx {
        upstream_owns: true,
        ..ctx_ext(true, false, &[(EXT, &["ext0"])])
    };
    assert_eq!(
        engine.pump(&ctx),
        Some(Command::ConfirmApplied(sel(
            Some("v0"),
            Some("a0"),
            Some("ext0")
        ))),
    );
}

/// A request for a sid the collection does not carry is the same shape as
/// the unmaterialized external: a fallback, not an answer.
#[test]
fn a_request_for_an_absent_sid_is_not_confirmed() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
    );

    engine.request(
        TrackSlot::Subtitle,
        TrackTarget::Stream(Some("gone".into())),
    );
    assert_eq!(engine.pump(&ctx_upstream(true, false)), None);
}

/// The same request in db3-owned mode is answered by decodebin3, so the
/// engine must not manufacture a second confirmation there.
#[test]
fn an_already_satisfied_request_is_not_confirmed_in_db3_owned_mode() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
    );

    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
    assert_eq!(engine.pump(&ctx(true, false)), None);
}

/// `dirty` set by anything OTHER than a user request must never confirm, or
/// the receiver relays unsolicited track changes.
#[test]
fn a_dirty_engine_without_a_request_confirms_nothing() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
    );
    // Collection churn, no request.
    engine.collection_changed(avt_collection());
    assert_eq!(engine.pump(&ctx_upstream(true, false)), None);
}

/// A request that lands while the transport is not quiet is ANSWERED LATER,
/// not dropped: the bookkeeping outlives every None-returning gate.
#[test]
fn a_request_made_while_not_quiet_is_answered_at_the_next_quiet_pump() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
    );

    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
    assert_eq!(engine.pump(&ctx_upstream(false, false)), None);
    assert_eq!(
        engine.pump(&ctx_upstream(true, false)),
        Some(Command::ConfirmApplied(sel(
            Some("v0"),
            Some("a0"),
            Some("t0")
        ))),
    );
}

fn ctx_ext(quiet: bool, paused: bool, externals: &[(ExternalSubId, &[&str])]) -> PumpCtx {
    PumpCtx {
        gate: SelectionGate {
            quiet,
            paused,
            seekable: true,
        },
        upstream_owns: false,
        externals_attached: true,
        externals: externals
            .iter()
            .map(|(id, sids)| (*id, sids.iter().map(|s| s.to_string()).collect()))
            .collect(),
        now: Instant::now(),
    }
}

/// Engine with the standard collection adopted and its defaults confirmed
/// as applied (the steady state most flows start from).
fn settled_engine() -> SelectionEngine {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
    );
    // The collection change marked the engine dirty and the auto-select
    // matches every (unset) desire, so this pump converges without a
    // dispatch.
    assert_eq!(engine.pump(&ctx(true, false)), None);
    engine
}

#[test]
fn collection_seeds_applied_with_defaults() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    assert_eq!(engine.applied(), &sel(Some("v0"), Some("a0"), Some("t0")),);
}

#[test]
fn selection_dispatches_immediately_when_quiet() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a1"),
            Some("t0")
        )))
    );
}

#[test]
fn selection_waits_until_quiet() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    assert_eq!(engine.pump(&ctx(false, false)), None);
    assert!(engine.has_dispatchable_work());
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a1"),
            Some("t0")
        )))
    );
}

#[test]
fn noop_selection_is_not_dispatched() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
    assert_eq!(engine.pump(&ctx(true, false)), None);
    assert!(!engine.has_dispatchable_work());
}

#[test]
fn playing_switch_serializes_and_coalesces_latest_wins() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(target.clone()))
    );
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());

    // Unconfirmed selection blocks everything while playing, including
    // the refresh the subtitle switch scheduled.
    assert_eq!(engine.pump(&ctx(true, false)), None);

    // Spammed changes compose latest-wins against the optimistic
    // applied state.
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
    assert_eq!(engine.pump(&ctx(true, false)), None);

    // The dispatched switch confirms and the composed latest goes out.
    engine.streams_selected(sn, &target);
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(Some("v0"), Some("a0"), None)))
    );
}

#[test]
fn selection_confirms_by_content_when_seqnum_is_lost() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    assert!(engine.pump(&ctx(true, false)).is_some());
    engine.selection_dispatched(gst::Seqnum::next(), target.clone());

    // A confirmation under a *foreign* seqnum, but reporting exactly
    // the requested selection, settles it.
    engine.streams_selected(gst::Seqnum::next(), &target);
    assert!(engine.selecting.is_none());
    // No re-dispatch (converged). Only the switch's scheduled flush
    // remains, exactly once.
    assert_eq!(engine.pump(&ctx(true, false)), Some(Command::RefreshSeek));
    assert_eq!(engine.pump(&ctx(true, false)), None);
}

#[test]
fn overtaken_selection_is_redispatched() {
    // decodebin3's own collection-default auto-select lands after ours and
    // stomps it. The overtaking STREAMS_SELECTED must re-assert the desire
    // instead of waiting forever on a confirmation that never comes.
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
    let target = sel(Some("v0"), Some("a0"), None);
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(target.clone()))
    );
    engine.selection_dispatched(gst::Seqnum::next(), target.clone());

    // decodebin3's own auto-select arrives instead of our confirmation:
    // foreign seqnum, foreign content.
    let adopted = sel(Some("v0"), Some("a0"), Some("t0"));
    engine.streams_selected(gst::Seqnum::next(), &adopted);

    // The desire re-asserts with a fresh dispatch.
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(target.clone()))
    );
    engine.selection_dispatched(gst::Seqnum::next(), target.clone());

    // This time it applies (content match settles under any seqnum) and
    // the engine converges.
    engine.streams_selected(gst::Seqnum::next(), &target);
    assert!(engine.selecting.is_none());
    assert_eq!(engine.pump(&ctx(true, false)), None);
    assert!(!engine.has_dispatchable_work());
}

#[test]
fn overtaken_selection_yields_to_a_newer_request() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    assert!(engine.pump(&ctx(true, false)).is_some());
    engine.selection_dispatched(gst::Seqnum::next(), sel(Some("v0"), Some("a0"), Some("t1")));

    // A newer request lands while the first is unconfirmed.
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));

    // The overtaking event must not resurrect the old request over it:
    // the latest desire wins.
    let adopted = sel(Some("v0"), Some("a0"), Some("t0"));
    engine.streams_selected(gst::Seqnum::next(), &adopted);
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(Some("v0"), Some("a0"), None)))
    );
}

#[test]
fn foreign_autoselect_with_nothing_in_flight_is_reasserted() {
    // A fresh collection's auto-select stomping the applied state with NO
    // request in flight is detected and corrected.
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
    let target = sel(Some("v0"), Some("a0"), None);
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(target.clone()))
    );
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());
    engine.streams_selected(sn, &target);
    assert_eq!(engine.pump(&ctx(true, false)), None);

    // Later, decodebin3 auto-selects the text stream on its own (fresh
    // collection default). The explicit "no subtitle" desire re-asserts.
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
    );
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(target))
    );
}

#[test]
fn unset_slots_follow_the_pipeline() {
    // Without an explicit desire the engine never fights the pipeline's
    // own choices.
    let mut engine = settled_engine();
    engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a1"), None));
    assert_eq!(engine.pump(&ctx(true, false)), None);
    assert_eq!(engine.applied(), &sel(Some("v0"), Some("a1"), None));
}

/// WHICH switches schedule a re-emit flush is a table over
/// [`select::schedule_refresh`] (audio/subtitle/video, the disable hazards,
/// upstream mode, externals, unseekable). What stays here is the engine
/// behaviour around a scheduled flush that the pure rule cannot see: the
/// ordering gates, the per-pump re-decision, and `cancel_refresh`.
#[test]
fn refresh_dispatches_after_selection_settles_and_pipeline_quiets() {
    let mut engine = settled_engine();
    // Enabling a subtitle schedules a refresh.
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(target.clone()))
    );
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());
    engine.streams_selected(sn, &target);
    // Re-preroll in progress: refresh must hold.
    assert_eq!(engine.pump(&ctx(false, false)), None);
    // Settled: flush.
    assert_eq!(engine.pump(&ctx(true, false)), Some(Command::RefreshSeek));
    // One flush only.
    assert_eq!(engine.pump(&ctx(true, false)), None);
}

#[test]
fn external_attaching_before_the_flush_drops_it() {
    // A plain switch schedules its flush, an external attaches while
    // the selection is confirming: the dispatch-time ctx check drops
    // the now-hazardous flush.
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    assert!(engine.pump(&ctx(true, false)).is_some());
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());
    engine.streams_selected(sn, &target);

    let ext = crate::ExternalSubId(3);
    assert_eq!(engine.pump(&ctx_ext(true, false, &[(ext, &[])])), None);
    assert!(!engine.has_dispatchable_work());
    // Detaching later must not resurrect the dropped flush either.
    assert_eq!(engine.pump(&ctx(true, false)), None);
}

#[test]
fn user_seek_cancels_refresh() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    assert!(engine.pump(&ctx(true, false)).is_some());
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());
    engine.streams_selected(sn, &target);

    // The user's own flushing seek re-emits the cue already.
    engine.cancel_refresh();
    assert_eq!(engine.pump(&ctx(true, false)), None);
}

#[test]
fn paused_selection_parks_and_the_refresh_waits_for_it() {
    // RENAMED, and the old name is the point. It was
    // `paused_selection_parks_and_refresh_flushes_past_it`, and flushing
    // past a parked selection is the defect: the refresh is a FLUSHING
    // seek, `SELECT_STREAMS` is sent from the select lane, and a flush
    // landing mid-send makes that `send_event` return false. urisourcebin
    // refuses the event, the stream is never selected upstream, and the
    // crate is left with a linked text branch no data reaches. Pinned
    // end-to-end by
    // `dash_segmented_embedded_text_shows_on_a_first_select_while_paused`.
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(target.clone()))
    );
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());

    // While paused the selection is parked (no STREAMS_SELECTED until data
    // flows), and the refresh waits behind it. DEFERRED, not dropped: the
    // want survives every pump that refuses it, so an arbitrarily long
    // parked window cannot lose the re-emit.
    for _ in 0..5 {
        assert_eq!(engine.pump(&ctx(true, true)), None);
    }
    assert!(
        engine.has_dispatchable_work(),
        "the deferred refresh was dropped"
    );

    // Confirmation releases it, once.
    engine.streams_selected(sn, &target);
    assert_eq!(engine.pump(&ctx(true, true)), Some(Command::RefreshSeek));
    engine.refresh_dispatched(gst::Seqnum::next());

    // Flush in flight: nothing else dispatches even though paused.
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    assert_eq!(engine.pump(&ctx(false, true)), None);
}

#[test]
fn a_refused_paused_selection_drops_its_deferred_refresh() {
    // The other half of the deferral: the refresh exists to re-emit a cue
    // for a switch that HAPPENED. A refused dispatch never switched
    // anything, so releasing the deferred flush would put an orphan
    // flushing seek into the pipeline for no reason.
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    assert!(engine.pump(&ctx(true, true)).is_some());
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target);
    assert_eq!(engine.pump(&ctx(true, true)), None);

    engine.dispatch_failed(sn);
    assert_eq!(engine.pump(&ctx(true, true)), None);
    // And the refusal rolled the optimistic `applied` back, so the desire
    // is divergent again and a later fresh event still asks.
    assert_ne!(engine.applied.subtitle, Some("t1".to_string()));
}

#[test]
fn paused_selection_can_be_superseded() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    assert!(engine.pump(&ctx(true, true)).is_some());
    let sn1 = gst::Seqnum::next();
    let first = sel(Some("v0"), Some("a1"), Some("t0"));
    engine.selection_dispatched(sn1, first.clone());

    // A parked selection has no re-preroll to overlap with, so the next
    // request replaces it instead of queueing behind it forever.
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a0".into())));
    let second = sel(Some("v0"), Some("a0"), Some("t0"));
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(second.clone()))
    );
    let sn2 = gst::Seqnum::next();
    engine.selection_dispatched(sn2, second.clone());

    // The stale confirmation (sn1, reporting the superseded audio) must
    // settle neither by its seqnum nor by content.
    engine.streams_selected(sn1, &first);
    assert!(engine.selecting.is_some());
    // The superseding one settles on its own seqnum.
    engine.streams_selected(sn2, &second);
    assert!(engine.selecting.is_none());
}

#[test]
fn paused_switch_refreshes_exactly_once() {
    // A paused subtitle switch dispatches the selection, then ONE re-emit
    // flush: the flushing seek re-prerolls, so the cue composites before
    // ASYNC_DONE and no retry is needed.
    //
    // COUNTED across the whole window, not asserted at one chosen pump,
    // and that is the point of the rewrite. The deferral moved the refresh
    // from before the confirmation to after it, and "fires once, later" and
    // "fires once on the deferral AND again on the confirmation" are
    // indistinguishable to any test that only looks at a single pump - or
    // to the e2e reproduction, which asserts cue DELIVERY and would pass
    // just as well with a redundant flush in it.
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    assert!(engine.pump(&ctx(true, true)).is_some());
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());

    let mut refreshes = 0;
    let pump = |engine: &mut SelectionEngine, refreshes: &mut usize| {
        if engine.pump(&ctx(true, true)) == Some(Command::RefreshSeek) {
            *refreshes += 1;
            engine.refresh_dispatched(gst::Seqnum::next());
            assert!(engine.refresh_done());
        }
    };
    // Parked: pumped repeatedly, released by nothing.
    for _ in 0..3 {
        pump(&mut engine, &mut refreshes);
    }
    assert_eq!(
        refreshes, 0,
        "a refresh flushed past an unconfirmed selection"
    );
    // Confirmed: released, and only once however often the pump runs.
    engine.streams_selected(sn, &target);
    for _ in 0..3 {
        pump(&mut engine, &mut refreshes);
    }
    assert_eq!(refreshes, 1, "the paused switch re-emitted more than once");
    assert_eq!(engine.pump(&ctx(true, true)), None);
    assert!(!engine.has_dispatchable_work());
}

#[test]
fn async_done_settles_refresh_by_exclusivity() {
    let mut engine = settled_engine();
    // No refresh out: an unrelated ASYNC_DONE is not a refresh
    // completion.
    assert!(!engine.refresh_done());
    engine.refresh_dispatched(gst::Seqnum::next());
    assert!(engine.refresh_done());
    assert!(engine.refreshing.is_none());
}

#[test]
fn new_collection_invalidates_in_flight_work() {
    // A new collection means an in-flight selection targeted stream ids
    // that may be gone, so the waits are abandoned deterministically.
    let mut engine = settled_engine();
    engine.selection_dispatched(gst::Seqnum::next(), sel(Some("v0"), Some("a1"), None));
    engine.refresh_dispatched(gst::Seqnum::next());
    assert!(engine.selecting.is_some());

    engine.collection_changed(avt_collection());
    assert!(engine.selecting.is_none());
    assert!(engine.refreshing.is_none());
}

#[test]
fn collection_reconciles_applied_and_keeps_explicit_desires() {
    let mut engine = settled_engine();
    // Explicitly select t1, confirmed.
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    assert!(engine.pump(&ctx(true, false)).is_some());
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());
    engine.streams_selected(sn, &target);
    engine.cancel_refresh();

    // t1 leaves the collection: applied falls back to the default text
    // stream, and the desire (t1) is unresolvable, so the engine
    // converges on what remains rather than fighting.
    engine.collection_changed(collection(&[
        ("v0", StreamKind::Video),
        ("a0", StreamKind::Audio),
        ("t0", StreamKind::Text),
    ]));
    assert_eq!(engine.applied(), &sel(Some("v0"), Some("a0"), Some("t0")));
    assert_eq!(engine.pump(&ctx(true, false)), None);
}

#[test]
fn external_desire_parks_until_materialized() {
    let mut engine = settled_engine();
    let ext = crate::ExternalSubId(1);
    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

    // Attached but no stream in the collection yet: keep showing what
    // shows now (no dispatch, the current selection already matches).
    assert_eq!(engine.pump(&ctx_ext(true, false, &[(ext, &[])])), None);

    // The external's stream materializes in a new collection. The
    // desire resolves and dispatches.
    let mut streams = avt_collection();
    streams.push(CollectionStream {
        sid: "ext-t".into(),
        kind: StreamKind::Text,
    });
    engine.collection_changed(streams);
    assert_eq!(
        engine.pump(&ctx_ext(true, false, &[(ext, &["ext-t"])])),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a0"),
            Some("ext-t")
        )))
    );
}

#[test]
fn an_external_that_materializes_before_its_input_is_visible_is_not_stranded() {
    // Resolving the desire needs two independent things to line up:
    // decodebin3 must advertise the input's stream, and the routing table
    // must have seen that input produce the id. Only the first re-dirties
    // the engine, so in that order the dirty flag is spent on a resolution
    // that cannot see the input yet and only `awaiting_external` re-arms.
    let mut engine = settled_engine();
    let ext = crate::ExternalSubId(1);
    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

    let mut streams = avt_collection();
    streams.push(CollectionStream {
        sid: "ext-t".into(),
        kind: StreamKind::Text,
    });
    engine.collection_changed(streams);

    // The pump runs before the routing table lists the input's ids.
    assert_eq!(engine.pump(&ctx_ext(true, false, &[(ext, &[])])), None);

    // They appear. Nothing else happens, so this pump is the only chance
    // the desire gets.
    let externals: &[(ExternalSubId, &[&str])] = &[(ext, &["ext-t"])];
    assert_eq!(
        engine.pump(&ctx_ext(true, false, externals)),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a0"),
            Some("ext-t")
        )))
    );
}

#[test]
fn external_desire_survives_the_autoselect_stomp() {
    // The desire dispatches after materialization, decodebin3's
    // auto-select for the new collection stomps it, the engine re-asserts.
    let mut engine = settled_engine();
    let ext = crate::ExternalSubId(1);
    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

    let mut streams = avt_collection();
    streams.push(CollectionStream {
        sid: "ext-t".into(),
        kind: StreamKind::Text,
    });
    engine.collection_changed(streams);
    let externals: &[(ExternalSubId, &[&str])] = &[(ext, &["ext-t"])];
    let target = sel(Some("v0"), Some("a0"), Some("ext-t"));
    assert_eq!(
        engine.pump(&ctx_ext(true, false, externals)),
        Some(Command::SelectStreams(target.clone()))
    );
    engine.selection_dispatched(gst::Seqnum::next(), target.clone());

    // Auto-select stomps with the embedded default.
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
    );
    assert_eq!(
        engine.pump(&ctx_ext(true, false, externals)),
        Some(Command::SelectStreams(target.clone()))
    );
    engine.selection_dispatched(gst::Seqnum::next(), target.clone());
    engine.streams_selected(gst::Seqnum::next(), &target);
    assert_eq!(engine.pump(&ctx_ext(true, false, externals)), None);
    assert!(!engine.has_dispatchable_work());
}

#[test]
fn external_switch_never_schedules_the_refresh() {
    // No refresh flush while an external input is attached; the
    // crate's join-time input replay covers externals instead.
    let mut engine = settled_engine();
    let ext = crate::ExternalSubId(1);
    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

    let mut streams = avt_collection();
    streams.push(CollectionStream {
        sid: "ext-t".into(),
        kind: StreamKind::Text,
    });
    engine.collection_changed(streams);
    let externals: &[(ExternalSubId, &[&str])] = &[(ext, &["ext-t"])];

    let target = sel(Some("v0"), Some("a0"), Some("ext-t"));
    assert_eq!(
        engine.pump(&ctx_ext(true, false, externals)),
        Some(Command::SelectStreams(target.clone()))
    );
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());
    engine.streams_selected(sn, &target);
    assert_eq!(engine.pump(&ctx_ext(true, false, externals)), None);
    assert!(!engine.has_dispatchable_work());
}

#[test]
fn failed_external_desire_keeps_the_current_subtitle() {
    let mut engine = settled_engine();
    let ext = crate::ExternalSubId(1);
    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));
    assert_eq!(engine.pump(&ctx_ext(true, false, &[(ext, &[])])), None);

    // The input fails (watchdog/bus error): the parked desire must not
    // park forever, and whatever was showing before the request keeps
    // showing (no dispatch at all).
    engine.external_gone(ext);
    assert_eq!(engine.pump(&ctx(true, false)), None);
    assert!(!engine.has_dispatchable_work());
    assert_eq!(engine.applied(), &sel(Some("v0"), Some("a0"), Some("t0")));
}

#[test]
fn external_desire_is_reasserted_after_a_foreign_autoselect() {
    // The external counterpart of
    // `foreign_autoselect_with_nothing_in_flight_is_reasserted`: after the
    // external's stream is confirmed and the engine converged, decodebin3
    // auto-selects the embedded text default and stomps it. The desire
    // must re-assert - no collection change follows such a stomp, so
    // nothing else would ever re-dirty the engine.
    let mut engine = settled_engine();
    let ext = crate::ExternalSubId(1);
    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

    let mut streams = avt_collection();
    streams.push(CollectionStream {
        sid: "ext-t".into(),
        kind: StreamKind::Text,
    });
    engine.collection_changed(streams);
    let externals: &[(ExternalSubId, &[&str])] = &[(ext, &["ext-t"])];
    let target = sel(Some("v0"), Some("a0"), Some("ext-t"));
    assert_eq!(
        engine.pump(&ctx_ext(true, false, externals)),
        Some(Command::SelectStreams(target.clone()))
    );
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());
    engine.streams_selected(sn, &target);
    // Converged: nothing in flight, nothing dirty.
    assert_eq!(engine.pump(&ctx_ext(true, false, externals)), None);

    // decodebin3 selects the embedded text default on its own.
    engine.streams_selected(
        gst::Seqnum::next(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
    );
    assert_eq!(
        engine.pump(&ctx_ext(true, false, externals)),
        Some(Command::SelectStreams(target))
    );
}

#[test]
fn a_stale_superseded_echo_does_not_revert_applied() {
    // The paused supersede path: a dispatch's late confirmation is our own
    // stale echo, and must not be adopted as applied either - `applied`
    // (and with it `subtitle_sid`, which gates what may join the overlay)
    // would name the subtitle the superseding dispatch is turning OFF.
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let first = sel(Some("v0"), Some("a0"), Some("t1"));
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(first.clone()))
    );
    let sn1 = gst::Seqnum::next();
    engine.selection_dispatched(sn1, first.clone());

    // Paused: the next request supersedes the parked dispatch.
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
    let second = sel(Some("v0"), Some("a0"), None);
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(second.clone()))
    );
    let sn2 = gst::Seqnum::next();
    engine.selection_dispatched(sn2, second.clone());

    // The superseded dispatch's late echo arrives.
    engine.streams_selected(sn1, &first);
    assert!(engine.selecting.is_some(), "the live wait must survive");
    assert_eq!(
        engine.applied(),
        &second,
        "a stale echo must not revert the applied selection"
    );
    assert_eq!(
        engine.subtitle_sid(),
        None,
        "a stale echo must not re-authorize the subtitle being turned off"
    );
}

#[test]
fn external_resolution_picks_the_inputs_text_stream() {
    // An external input can advertise more than one stream, in source-pad
    // order, so resolving to its FIRST advertised stream would drop a
    // non-text id into the subtitle slot and deselect the text one.
    let mut engine = settled_engine();
    let ext = crate::ExternalSubId(1);
    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));

    let mut streams = avt_collection();
    streams.push(CollectionStream {
        sid: "ext-a".into(),
        kind: StreamKind::Audio,
    });
    streams.push(CollectionStream {
        sid: "ext-t".into(),
        kind: StreamKind::Text,
    });
    engine.collection_changed(streams);

    let externals: &[(ExternalSubId, &[&str])] = &[(ext, &["ext-a", "ext-t"])];
    assert_eq!(
        engine.pump(&ctx_ext(true, false, externals)),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a0"),
            Some("ext-t")
        )))
    );
}

#[test]
fn video_disable_implicitly_drops_subtitles() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Video, TrackTarget::Stream(None));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(None, Some("a0"), None)))
    );
}

/// decodebin3 reports its selection while its merged collection is still
/// growing, so a report can name audio alone with the video stream
/// already advertised. Composing against that empty video slot asks
/// decodebin3 to turn video OFF, which nobody requested, and the rule
/// above then strips the subtitle the request was about.
#[test]
fn an_unset_slot_left_empty_by_a_partial_report_resolves_to_the_default() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    // decodebin3's own first report, before its video input reported.
    engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), Some("t0")));
    assert_eq!(engine.applied().video, None);

    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a0"),
            Some("t1")
        ))),
        "the unset video slot must follow the collection default, not compose a disable"
    );
}

/// The same growth window, one post earlier, when the collection has no
/// video stream at all yet. Nothing can be composed that honours the
/// request, so nothing is dispatched. The collection change that brings
/// video in re-dirties the desire.
#[test]
fn a_subtitle_request_waits_for_a_collection_that_has_video() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(collection(&[
        ("a0", StreamKind::Audio),
        ("t0", StreamKind::Text),
        ("t1", StreamKind::Text),
    ]));
    // decodebin3 auto-selected audio and the first text before its video
    // input reported, so a subtitle IS showing. Without that the stripped
    // target equals `applied` and the no-op check hides the defect.
    engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), Some("t0")));

    // Switching to the other text stream must not be answered by turning
    // subtitles off, which is what stripping t1 and dispatching the
    // remainder amounts to.
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        None,
        "a selection that strips the requested subtitle must not be dispatched"
    );

    // Video arrives. Now the request is expressible, and the collection
    // change re-dirtied it.
    engine.collection_changed(avt_collection());
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a0"),
            Some("t1")
        )))
    );
}

/// On a media with NO video at all the holdback's premise never holds: the
/// collection change that brings video in is not coming, and `pump` clears
/// `dirty` before `resolve` answers `None`. Nothing was dispatched, no
/// `ConfirmApplied` was produced and `unanswered_request` stayed set for
/// the life of the item, so the request was swallowed whole.
///
/// The wait is bounded now: while it lasts the engine counts as
/// unconverged (which is what keeps the pump poked at all), and past
/// [`select::SUBTITLE_HOLDBACK_GRACE`] the switch goes out with the subtitle
/// KEPT, exactly as the sibling arm sends it when another slot has work.
#[test]
fn a_video_less_item_eventually_dispatches_a_held_back_subtitle() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(collection(&[
        ("a0", StreamKind::Audio),
        ("t0", StreamKind::Text),
        ("t1", StreamKind::Text),
    ]));
    engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), Some("t0")));

    let t0 = Instant::now();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    // Held back, and `dirty` is consumed by the attempt: only the wait
    // itself brings the pump back.
    assert_eq!(engine.pump(&ctx_at(true, false, t0)), None);
    assert!(!engine.has_dispatchable_work());
    assert!(
        engine.unconverged(),
        "a held-back desire must keep the engine poked"
    );
    // Inside the grace the collection could still be growing a video
    // stream, so every pump keeps waiting.
    for step in 1..5 {
        let now = t0 + Duration::from_millis(100 * step);
        assert_eq!(engine.pump(&ctx_at(true, false, now)), None);
        assert_eq!(engine.pump(&ctx_at(true, true, now)), None);
    }
    assert_eq!(
        engine.applied().subtitle.as_deref(),
        Some("t0"),
        "the switch went out before the collection had its chance"
    );

    // No video ever came. The switch dispatches with the subtitle kept.
    let late = t0 + select::SUBTITLE_HOLDBACK_GRACE;
    let target = sel(None, Some("a0"), Some("t1"));
    assert_eq!(
        engine.pump(&ctx_at(true, false, late)),
        Some(Command::SelectStreams(target.clone()))
    );
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target.clone());
    engine.streams_selected(sn, &target);
    assert!(
        engine.subtitle_held_since.is_none(),
        "the wait outlived the dispatch that ended it"
    );
    assert!(
        !engine.pending.has(Pending::REQUEST),
        "the dispatch's own confirmation answers the request"
    );
}

/// The same wait, on the request the field actually loses: an external
/// subtitle attached to an audio-only item with `select=true`.
///
/// `collection_changed` seeds the text slot with the external's stream, so
/// the resolution EQUALS `applied` and the only thing owed is the answer -
/// which the holdback swallowed with the resolution, leaving the input held
/// and the caller waiting for a confirmation that never came.
#[test]
fn a_video_less_item_answers_a_held_back_external_request() {
    let ext = ExternalSubId(3);
    let mut engine = SelectionEngine::default();
    engine.collection_changed(collection(&[("a0", StreamKind::Audio)]));
    engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), None));
    // The external input's stream is merged in.
    engine.collection_changed(collection(&[
        ("a0", StreamKind::Audio),
        ("ext-t", StreamKind::Text),
    ]));

    let t0 = Instant::now();
    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(ext));
    let held = PumpCtx {
        now: t0,
        upstream_owns: true,
        ..ctx_ext(true, false, &[(ext, &["ext-t"])])
    };
    assert_eq!(engine.pump(&held), None);
    assert!(
        engine.unconverged(),
        "the request is owed an answer only this crate can give"
    );

    let late = PumpCtx {
        now: t0 + select::SUBTITLE_HOLDBACK_GRACE,
        ..held
    };
    assert_eq!(
        engine.pump(&late),
        Some(Command::ConfirmApplied(sel(
            None,
            Some("a0"),
            Some("ext-t")
        ))),
        "the held-back request was never answered"
    );
    assert!(!engine.unconverged());
}

/// A report that PREDATES half the collection must not leave the engine
/// silent. `empty_text_stream.toml`'s wedge in twenty lines instead of a
/// 52-second scenario sweep.
///
/// `collection_changed` seeds still-empty slots with the collection
/// default, mirroring decodebin3's own auto-select, and that premise fails
/// when the auto-select ran EARLY against a partial merged collection and
/// was never revisited. With `parse-streams=true` every elementary stream
/// is its own input, so the merged collection arrives in increments.
///
/// The sequence below is the one the scenario produced verbatim. The
/// unguarded seed wrote the grown-in audio and video into `applied`, the
/// pump found `target == applied` and dispatched NOTHING, so decodebin3
/// kept playing text alone with no A/V chain built, no real sink joined,
/// the preroll token holding the pipeline ASYNC and the load never reaching
/// settled PLAYING.
#[test]
fn a_report_that_predates_half_the_collection_still_dispatches() {
    let mut engine = SelectionEngine::default();
    // decodebin3's text input reports first, and it auto-selects on what it
    // has.
    engine.collection_changed(collection(&[("t0", StreamKind::Text)]));
    engine.streams_selected(gst::Seqnum::next(), &sel(None, None, Some("t0")));
    assert_eq!(engine.applied(), &sel(None, None, Some("t0")));

    // The audio and video inputs report afterwards. No second
    // STREAMS_SELECTED follows, so nothing but this engine can notice.
    engine.collection_changed(collection(&[
        ("a0", StreamKind::Audio),
        ("t0", StreamKind::Text),
    ]));
    engine.collection_changed(collection(&[
        ("v0", StreamKind::Video),
        ("a0", StreamKind::Audio),
        ("t0", StreamKind::Text),
    ]));

    // A kind that is new since the report is ASKED about, not assumed, so
    // `applied` still says what decodebin3 actually confirmed...
    assert_eq!(
        engine.applied().video,
        None,
        "a kind the report predates must not be seeded as applied"
    );
    // ...and the pump therefore has real work, the full default selection,
    // which is what gets decodebin3 to build the A/V chains at all.
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a0"),
            Some("t0")
        )))
    );
}

/// The guard above must stay OFF before decodebin3 has committed to
/// anything, or every load dispatches an explicit `SELECT_STREAMS` for its
/// own defaults. Not merely noise, an explicit selection takes decodebin3
/// out of auto-select for the item, and this crate depends on that
/// auto-select for text (`collection_changed`'s `text_state_known` note).
#[test]
fn a_collection_before_any_report_is_still_seeded_whole() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(collection(&[("t0", StreamKind::Text)]));
    engine.collection_changed(collection(&[
        ("v0", StreamKind::Video),
        ("a0", StreamKind::Audio),
        ("t0", StreamKind::Text),
    ]));
    assert_eq!(
        engine.applied(),
        &sel(Some("v0"), Some("a0"), Some("t0")),
        "with no report yet, decodebin3's own auto-select still covers this"
    );
    assert_eq!(
        engine.pump(&ctx(true, false)),
        None,
        "and there is nothing to tell it"
    );
}

#[test]
fn empty_selection_is_refused() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Video, TrackTarget::Stream(None));
    engine.request(TrackSlot::Audio, TrackTarget::Stream(None));
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
    // All-off resolves to the empty selection, which decodebin3 asserts
    // on: nothing is dispatched.
    assert_eq!(engine.pump(&ctx(true, false)), None);
    assert!(!engine.has_dispatchable_work());
}

#[test]
fn reset_forgets_everything() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    engine.reset();
    assert!(!engine.has_dispatchable_work());
    assert_eq!(engine.applied(), &TrackSelection::default());
    assert_eq!(engine.pump(&ctx(true, false)), None);
}

#[test]
fn dispatch_failure_clears_the_wait_and_the_flush() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    assert!(engine.pump(&ctx(true, false)).is_some());
    let sn = gst::Seqnum::next();
    engine.selection_dispatched(sn, target);
    engine.dispatch_failed(sn);
    assert!(engine.selecting.is_none());
    // The failed switch's flush must not fire as an orphan.
    assert_eq!(engine.pump(&ctx(true, false)), None);
}

/// A failure report for a dispatch the engine is NOT waiting on must
/// change nothing at all - the flush least of all. Every select the hands
/// skip (a superseded core, a stale queue epoch) reports a failure for a
/// seqnum a load or a swap has already moved past.
#[test]
fn a_failure_for_a_superseded_dispatch_leaves_the_live_one_alone() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    // The pump schedules the switch's re-emit flush.
    assert!(engine.pump(&ctx(true, false)).is_some());
    let stale = gst::Seqnum::next();
    engine.selection_dispatched(stale, sel(Some("v0"), Some("a0"), Some("t1")));
    assert!(
        engine.pending.has(Pending::REFRESH),
        "the switch schedules its re-emit"
    );

    // A newer dispatch replaces it (a caller's newer switch, or the
    // deadline's re-assertion): the old record is superseded, and only
    // the new one is being waited on.
    let live = gst::Seqnum::next();
    engine.selection_dispatched(live, sel(Some("v0"), Some("a0"), Some("t2")));

    engine.dispatch_failed(stale);
    assert!(
        engine.selection_in_flight(live),
        "a stale failure cleared the live wait"
    );
    assert!(
        engine.pending.has(Pending::REFRESH),
        "a stale failure cancelled the live switch's re-emit flush"
    );

    // And the live one's own failure still does both.
    engine.dispatch_failed(live);
    assert!(engine.selecting.is_none());
    assert!(!engine.pending.has(Pending::REFRESH));
}

#[test]
fn a_superseded_echo_after_a_foreign_overtake_does_not_revert_applied() {
    // The overtake path drops the live tracking (`selecting` is taken and
    // never restored) while the superseded records stay, so the same echo
    // lands with nothing in flight and would be mistaken for a foreign
    // selection - re-arming `subtitle_sid` with the very subtitle the
    // newest dispatch is turning off.
    let mut engine = settled_engine();
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let first = sel(Some("v0"), Some("a0"), Some("t1"));
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(first.clone()))
    );
    let sn1 = gst::Seqnum::next();
    engine.selection_dispatched(sn1, first.clone());

    // Paused, so the next request supersedes the parked dispatch.
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
    let second = sel(Some("v0"), Some("a0"), None);
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(second.clone()))
    );
    let sn2 = gst::Seqnum::next();
    engine.selection_dispatched(sn2, second.clone());

    // decodebin3's own auto-select lands before either confirmation:
    // neither seqnum nor content matches, so the live wait is abandoned
    // and the desire re-asserts. The superseded record for sn1 outlives it.
    let foreign = sel(Some("v0"), Some("a1"), None);
    engine.streams_selected(gst::Seqnum::next(), &foreign);
    assert_eq!(engine.subtitle_sid(), None);

    // Only now does the superseded dispatch's confirmation arrive.
    engine.streams_selected(sn1, &first);
    assert_eq!(
        engine.subtitle_sid(),
        None,
        "a superseded dispatch's late echo must not re-authorize its subtitle"
    );
    assert_eq!(
        engine.applied().subtitle,
        None,
        "a superseded dispatch's late echo must not revert the applied selection"
    );
}

/// A supersede chain whose SENT head confirms and whose superseding head
/// is then refused: the engine must come to rest on what upstream really
/// applied, not on the refused target and not on the pre-chain guess.
///
/// The chain: paused dispatch sn1 (really sent, parked at decodebin3),
/// superseded by sn2 (`applied` optimistic, anchor still the pre-chain
/// state). The resume's flushing seek lands mid-send of sn2, so sn1
/// confirms and sn2 is refused. Clearing the anchor on the way through the
/// echo left `dispatch_failed` nothing to roll back to: `applied` kept the
/// refused target, `desired == applied`, nothing dirty, no wait, no
/// deadline, and `poll_text_policy` linking a subtitle upstream never
/// selected.
#[test]
fn a_refusal_after_a_superseded_echo_rolls_back_to_the_report() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    let first = sel(Some("v0"), Some("a1"), Some("t0"));
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(first.clone()))
    );
    let sn1 = gst::Seqnum::next();
    engine.selection_dispatched(sn1, first.clone());

    // Paused, so the next request supersedes the parked dispatch.
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let second = sel(Some("v0"), Some("a1"), Some("t1"));
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(second.clone()))
    );
    let sn2 = gst::Seqnum::next();
    engine.selection_dispatched(sn2, second.clone());

    // The resume makes data flow and the parked sn1 confirms. The live
    // wait is restored, and nothing is adopted.
    engine.streams_selected(sn1, &first);
    assert!(engine.selection_in_flight(sn2), "the live wait was settled");

    // The same flush refused sn2 mid-send: it never left.
    engine.dispatch_failed(sn2);
    assert_eq!(
        engine.applied(),
        &first,
        "the refusal must put back what upstream last confirmed"
    );
}

/// The other order of the same chain: the refusal first, the sent head's
/// confirmation after it.
///
/// `dispatch_failed` rolls back to the pre-chain snapshot, which the sent
/// sibling has already moved upstream past, and that sibling's own
/// confirmation is then drained as a stale echo - skipping adoption AND
/// the divergence check. The desire has to survive that, or the engine
/// rests with `applied`, the desire and decodebin3 all disagreeing.
#[test]
fn a_refusal_inside_a_supersede_chain_keeps_the_desire_dispatchable() {
    let mut engine = settled_engine();
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    let first = sel(Some("v0"), Some("a1"), Some("t0"));
    assert!(engine.pump(&ctx(true, true)).is_some());
    let sn1 = gst::Seqnum::next();
    engine.selection_dispatched(sn1, first.clone());

    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let second = sel(Some("v0"), Some("a1"), Some("t1"));
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(second.clone()))
    );
    let sn2 = gst::Seqnum::next();
    engine.selection_dispatched(sn2, second.clone());

    engine.dispatch_failed(sn2);
    assert!(
        engine.has_dispatchable_work(),
        "a refusal with a sent sibling outstanding left nothing to re-assert"
    );

    // The sibling's late confirmation is ours but stale, so it is drained
    // rather than adopted, and it must not take the desire with it.
    engine.streams_selected(sn1, &first);
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(second)),
        "the superseding desire was never asked for again"
    );
}

#[test]
fn two_superseded_dispatches_are_recognized_and_then_forgotten() {
    // Three paused dispatches, each superseding the last, so two records
    // are outstanding. decodebin3 folds the first into the second and
    // confirms only the second, which must retire BOTH: a record that
    // outlives its dispatch goes on swallowing genuine foreign selections
    // that happen to name the same streams.
    let mut engine = settled_engine();

    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    let first = sel(Some("v0"), Some("a1"), Some("t0"));
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(first.clone()))
    );
    engine.selection_dispatched(gst::Seqnum::next(), first.clone());

    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let second = sel(Some("v0"), Some("a1"), Some("t1"));
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(second.clone()))
    );
    let sn_second = gst::Seqnum::next();
    engine.selection_dispatched(sn_second, second.clone());

    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
    let third = sel(Some("v0"), Some("a1"), None);
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(third.clone()))
    );
    engine.selection_dispatched(gst::Seqnum::next(), third.clone());

    engine.streams_selected(sn_second, &second);
    assert!(
        engine.selecting.is_some(),
        "a stale echo must not settle the live wait"
    );
    assert_eq!(
        engine.applied(),
        &third,
        "a stale echo must not revert the applied selection"
    );

    // The first dispatch's streams are no longer ours to recognize. A
    // selection naming them now is decodebin3's own and must re-assert
    // the desire instead of being swallowed as an echo that can never
    // come.
    engine.streams_selected(gst::Seqnum::next(), &first);
    assert_eq!(
        engine.pump(&ctx(true, true)),
        Some(Command::SelectStreams(third)),
        "a retired superseded record must not swallow a foreign selection"
    );
}

/// A record for the set to hold, distinguishable by its audio sid.
fn record(n: usize) -> (gst::Seqnum, TrackSelection) {
    (
        gst::Seqnum::next(),
        sel(Some("v0"), Some(&format!("a{n}")), None),
    )
}

/// The inline set's contract, straight at the type. A `Vec` gave these for
/// free; a fixed array has to be shown to.
///
/// The load-bearing one is the PREFIX drain: decodebin3 confirms in dispatch
/// order, so matching a record retires every older one, which is what keeps a
/// stale record from swallowing a genuinely foreign selection much later.
#[test]
fn the_superseded_set_drains_as_a_prefix() {
    let mut set = SupersededSet::default();
    assert!(set.is_empty());

    let records: Vec<_> = (0..4).map(record).collect();
    for entry in &records {
        set.push(entry.clone());
    }

    // Matching by seqnum and by content are the same lookup: the seqnum is
    // lost when decodebin3 coalesces or no-ops a request.
    assert_eq!(
        set.position(records[2].0, &TrackSelection::default()),
        Some(2)
    );
    assert_eq!(set.position(gst::Seqnum::next(), &records[1].1), Some(1));
    assert_eq!(
        set.position(gst::Seqnum::next(), &TrackSelection::default()),
        None
    );

    // Draining the middle record takes the older one with it and leaves the
    // newer ones in order.
    set.drain_through(1);
    assert_eq!(set.len, 2);
    assert_eq!(set.position(records[0].0, &TrackSelection::default()), None);
    assert_eq!(set.position(records[1].0, &TrackSelection::default()), None);
    assert_eq!(
        set.position(records[2].0, &TrackSelection::default()),
        Some(0)
    );
    assert_eq!(
        set.position(records[3].0, &TrackSelection::default()),
        Some(1)
    );

    set.drain_through(1);
    assert!(set.is_empty());
    assert_eq!(set.position(records[3].0, &TrackSelection::default()), None);
}

/// The one behaviour a `Vec` never had. Overflow forgets the OLDEST record,
/// which is the safe direction: the set is drained head-first anyway, and a
/// record dropped too early costs at most one convergent re-assert, while a
/// record kept too long is the swallowed foreign selection the drain exists
/// to prevent.
#[test]
fn superseded_overflow_forgets_the_oldest_record() {
    let mut set = SupersededSet::default();
    let records: Vec<_> = (0..SUPERSEDED_CAP + 2).map(record).collect();
    for entry in &records {
        set.push(entry.clone());
    }

    assert_eq!(set.len, SUPERSEDED_CAP, "the set never grows past its cap");
    for gone in &records[..2] {
        assert_eq!(
            set.position(gone.0, &TrackSelection::default()),
            None,
            "an overflowed record must be forgotten, not kept"
        );
    }
    // The survivors kept their order, newest last.
    for (index, kept) in records[2..].iter().enumerate() {
        assert_eq!(
            set.position(kept.0, &TrackSelection::default()),
            Some(index)
        );
    }
}

/// What a gapless boundary carries. A stream id names a stream of the item
/// that just ended, so only an explicit DISABLE is item-independent and
/// survives. An external desire goes with the rest: the activation removes
/// the previous generation's inputs (`Job::FinishActivation`).
#[test]
fn a_gapless_boundary_carries_explicit_disables_only() {
    struct Case {
        name: &'static str,
        setup: fn(&mut SelectionEngine),
        applied: TrackSelection,
        dispatch: Option<TrackSelection>,
    }
    let cases = [
        Case {
            name: "nothing requested",
            setup: |_| {},
            applied: sel(Some("v0"), Some("a0"), Some("t0")),
            dispatch: None,
        },
        Case {
            name: "an explicit subtitle stream is dropped",
            setup: |engine| {
                engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())))
            },
            applied: sel(Some("v0"), Some("a0"), Some("t0")),
            dispatch: None,
        },
        Case {
            name: "an external subtitle desire is dropped with its input",
            setup: |engine| {
                engine.request(
                    TrackSlot::Subtitle,
                    TrackTarget::ExternalSubtitle(crate::ExternalSubId(4)),
                )
            },
            applied: sel(Some("v0"), Some("a0"), Some("t0")),
            dispatch: None,
        },
        Case {
            name: "subtitles off survive",
            setup: |engine| engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None)),
            applied: sel(Some("v0"), Some("a0"), None),
            dispatch: None,
        },
        Case {
            name: "video off survives and re-asserts",
            setup: |engine| engine.request(TrackSlot::Video, TrackTarget::Stream(None)),
            applied: sel(None, Some("a0"), Some("t0")),
            dispatch: Some(sel(None, Some("a0"), None)),
        },
        Case {
            name: "audio off survives",
            setup: |engine| engine.request(TrackSlot::Audio, TrackTarget::Stream(None)),
            applied: sel(Some("v0"), None, Some("t0")),
            dispatch: None,
        },
    ];

    for case in cases {
        let mut engine = settled_engine();
        (case.setup)(&mut engine);
        engine.reset_across_gapless();
        // The activation feeds the next item's collection right after.
        engine.collection_changed(avt_collection());
        assert_eq!(
            engine.applied(),
            &case.applied,
            "applied state after the boundary, case: {}",
            case.name
        );
        assert_eq!(
            engine.pump(&ctx(true, false)),
            case.dispatch.map(Command::SelectStreams),
            "dispatch after the boundary, case: {}",
            case.name
        );
    }
}

#[test]
fn a_report_made_before_text_was_advertised_does_not_turn_text_off() {
    // A report made while the collection held no text stream says NOTHING
    // about the text slot. If it still suppressed seeding, the next
    // dispatch would compose an event that omits text - decodebin3's way
    // of being told to turn it off - and its auto-select never runs again.
    let mut engine = SelectionEngine::default();
    engine.collection_changed(collection(&[("a0", StreamKind::Audio)]));
    engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), None));

    // The video and text inputs report and the merged collection grows.
    engine.collection_changed(avt_collection());
    assert_eq!(
        engine.applied().subtitle,
        Some("t0".into()),
        "an empty text slot from a report made before text existed is not \
         a decision to keep text off"
    );

    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a1"),
            Some("t0")
        ))),
        "an audio switch must not deselect the text stream nobody touched"
    );
}

/// The report ladder only ever climbs. Its rungs are what an empty applied
/// text slot MEANS, and a later report that happens to have no text to speak
/// about must not un-know a text state already learned, or the seeding guard
/// in `collection_changed` re-seeds a slot decodebin3 has really turned off.
///
/// The fourth combination the two bools this replaces could express - text
/// known with nothing reported - is now unrepresentable rather than merely
/// unreached.
#[test]
fn the_report_ladder_only_climbs() {
    let mut engine = SelectionEngine::default();
    assert_eq!(engine.report, ReportProgress::Unreported);

    // A report against a text-less collection commits without knowing.
    engine.collection_changed(collection(&[("a0", StreamKind::Audio)]));
    engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), None));
    assert_eq!(engine.report, ReportProgress::Committed);

    // Text is advertised, so the next report speaks about the slot.
    engine.collection_changed(avt_collection());
    engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), None));
    assert_eq!(engine.report, ReportProgress::TextKnown);

    // A later report naming no text, against a collection that carries none
    // (an adaptive stream dropping its text period), must not demote.
    engine.collection_changed(collection(&[("a0", StreamKind::Audio)]));
    engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), None));
    assert_eq!(engine.report, ReportProgress::TextKnown);

    // Only a load reset starts the ladder over.
    engine.reset();
    assert_eq!(engine.report, ReportProgress::Unreported);
}

/// A decodebin3 stand-in for the exhaustive ordering search below: it
/// applies verbatim what it is told and confirms with the dispatch's own
/// seqnum, so any divergence the search reports is the engine's.
struct Model {
    engine: SelectionEngine,
    /// Dispatched, not yet confirmed, oldest first.
    inflight: std::collections::VecDeque<(gst::Seqnum, TrackSelection)>,
    /// What the ops asked for, mirrored here so the oracle does not read
    /// the state it is checking.
    want_audio: Option<Option<&'static str>>,
    want_subtitle: SubWant,
    /// Whether any subtitle op ran at all, which is what makes an UNSET
    /// desire mean "never touched" rather than "reset by a failed
    /// external".
    subtitle_ever_requested: bool,
    want_video: Option<Option<&'static str>>,
    collection: Vec<CollectionStream>,
    /// Stream ids each attached external input has produced, exactly as
    /// `PumpCtx` carries them.
    externals: Vec<(ExternalSubId, Vec<String>)>,
}

/// The subtitle slot's desire as the ops set it, mirrored so the oracle
/// does not read the state it is checking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubWant {
    Unset,
    Off,
    Stream(&'static str),
    External,
}

/// The one external input the searches use.
const EXT: ExternalSubId = ExternalSubId(9);
/// The text stream that external input advertises once it materializes.
const EXT_SID: &str = "ext-t";

/// The ops the search permutes. Each is something the crate really does
/// to the engine.
#[derive(Clone, Copy, Debug)]
enum Op {
    Pump {
        paused: bool,
    },
    /// Deliver the oldest pending confirmation.
    Echo,
    /// decodebin3 auto-selects the collection defaults on its own.
    Foreign,
    WantSubtitle(Option<&'static str>),
    WantAudio(Option<&'static str>),
    WantVideo(Option<&'static str>),
    /// The merged collection grows to the full A/V+text one.
    Grow,
    /// The external input attaches: its pads exist and carry their
    /// stream ids, but decodebin3 has not merged them yet.
    AttachExternal,
    /// decodebin3 merges the external's stream into the collection.
    MaterializeExternal,
    WantExternal,
    /// The input failed or was detached.
    ExternalGone,
}

impl Model {
    fn new(collection: Vec<CollectionStream>) -> Self {
        let mut engine = SelectionEngine::default();
        engine.collection_changed(collection.clone());
        Self {
            engine,
            inflight: std::collections::VecDeque::new(),
            want_audio: None,
            want_subtitle: SubWant::Unset,
            subtitle_ever_requested: false,
            want_video: None,
            collection,
            externals: Vec::new(),
        }
    }

    fn ctx(&self, paused: bool) -> PumpCtx {
        PumpCtx {
            gate: SelectionGate {
                quiet: true,
                paused,
                seekable: true,
            },
            externals_attached: !self.externals.is_empty(),
            externals: self.externals.clone(),
            upstream_owns: false,
            now: Instant::now(),
        }
    }

    /// Whether the external desire has everything it needs to resolve:
    /// the input has produced the id AND decodebin3 advertises it as
    /// text.
    fn external_resolves(&self) -> bool {
        self.externals
            .iter()
            .any(|(id, sids)| *id == EXT && sids.iter().any(|s| s == EXT_SID))
            && self
                .collection
                .iter()
                .any(|s| s.sid == EXT_SID && s.kind == StreamKind::Text)
    }

    fn default_of(&self, kind: StreamKind) -> Option<String> {
        self.collection
            .iter()
            .find(|s| s.kind == kind)
            .map(|s| s.sid.clone())
    }

    fn pump_once(&mut self, paused: bool) -> bool {
        let ctx = self.ctx(paused);
        match self.engine.pump(&ctx) {
            None => false,
            // Unreachable in db3-owned mode, which is what this driver
            // models (`ctx` sets `upstream_owns: false`).
            Some(Command::ConfirmApplied(_)) => {
                unreachable!("ConfirmApplied is scoped to upstream-selection mode")
            }
            Some(Command::SelectStreams(target)) => {
                assert!(
                    target.video.is_some() || target.audio.is_some() || target.subtitle.is_some(),
                    "an empty selection was dispatched"
                );
                let seqnum = gst::Seqnum::next();
                self.engine.selection_dispatched(seqnum, target.clone());
                self.inflight.push_back((seqnum, target));
                true
            }
            Some(Command::RefreshSeek) => {
                self.engine.refresh_dispatched(gst::Seqnum::next());
                self.engine.refresh_done();
                true
            }
        }
    }

    fn echo(&mut self) -> bool {
        match self.inflight.pop_front() {
            Some((seqnum, target)) => {
                self.engine.streams_selected(seqnum, &target);
                true
            }
            None => false,
        }
    }

    fn apply(&mut self, op: Op) {
        match op {
            Op::Pump { paused } => {
                self.pump_once(paused);
            }
            Op::Echo => {
                self.echo();
            }
            Op::Foreign => {
                let foreign = TrackSelection {
                    video: self.default_of(StreamKind::Video),
                    audio: self.default_of(StreamKind::Audio),
                    subtitle: self.default_of(StreamKind::Text),
                };
                self.engine.streams_selected(gst::Seqnum::next(), &foreign);
            }
            Op::WantSubtitle(sid) => {
                self.want_subtitle = match sid {
                    Some(sid) => SubWant::Stream(sid),
                    None => SubWant::Off,
                };
                self.subtitle_ever_requested = true;
                self.engine.request(
                    TrackSlot::Subtitle,
                    TrackTarget::Stream(sid.map(str::to_string)),
                );
            }
            Op::WantAudio(sid) => {
                self.want_audio = Some(sid);
                self.engine.request(
                    TrackSlot::Audio,
                    TrackTarget::Stream(sid.map(str::to_string)),
                );
            }
            Op::WantVideo(sid) => {
                self.want_video = Some(sid);
                self.engine.request(
                    TrackSlot::Video,
                    TrackTarget::Stream(sid.map(str::to_string)),
                );
            }
            Op::Grow => {
                // The external's stream, once merged, stays merged.
                let ext =
                    self.collection
                        .iter()
                        .any(|s| s.sid == EXT_SID)
                        .then(|| CollectionStream {
                            sid: EXT_SID.into(),
                            kind: StreamKind::Text,
                        });
                self.collection = avt_collection();
                self.collection.extend(ext);
                self.engine.collection_changed(self.collection.clone());
            }
            Op::AttachExternal => {
                if !self.externals.iter().any(|(id, _)| *id == EXT) {
                    self.externals.push((EXT, vec![EXT_SID.to_string()]));
                }
            }
            Op::MaterializeExternal => {
                if !self.collection.iter().any(|s| s.sid == EXT_SID) {
                    self.collection.push(CollectionStream {
                        sid: EXT_SID.into(),
                        kind: StreamKind::Text,
                    });
                    self.engine.collection_changed(self.collection.clone());
                }
            }
            Op::WantExternal => {
                self.want_subtitle = SubWant::External;
                self.subtitle_ever_requested = true;
                self.engine
                    .request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(EXT));
            }
            Op::ExternalGone => {
                self.externals.retain(|(id, _)| *id != EXT);
                self.collection.retain(|s| s.sid != EXT_SID);
                if self.want_subtitle == SubWant::External {
                    // `external_gone` resets the desire to UNSET, and
                    // whatever was showing keeps showing.
                    self.want_subtitle = SubWant::Unset;
                }
                self.engine.external_gone(EXT);
            }
        }
    }

    /// Run the pipeline to a standstill: every confirmation delivered,
    /// every dispatch the engine still wants made and confirmed. Bounded,
    /// so a re-assertion that ping-pongs fails here rather than hanging.
    fn settle(&mut self, trace: &[Op]) {
        for _ in 0..64 {
            while self.echo() {}
            if !self.pump_once(false) && self.inflight.is_empty() {
                assert!(
                    !self.engine.has_dispatchable_work(),
                    "settled with work still pending, trace {trace:?}"
                );
                return;
            }
        }
        panic!("the engine never converged, trace {trace:?}");
    }

    fn has(&self, sid: &str) -> bool {
        self.collection.iter().any(|s| s.sid == sid)
    }

    fn has_video(&self) -> bool {
        self.collection.iter().any(|s| s.kind == StreamKind::Video)
    }

    /// Every explicit desire the current collection CAN express must be
    /// the applied state once everything has settled.
    fn check_desires(&self, trace: &[Op]) {
        let applied = self.engine.applied();
        match self.want_video {
            Some(Some(sid)) if self.has(sid) => assert_eq!(
                applied.video.as_deref(),
                Some(sid),
                "the video desire was not applied, trace {trace:?}, applied {applied:?}"
            ),
            Some(None) => assert_eq!(
                applied.video, None,
                "the video disable was not applied, trace {trace:?}, applied {applied:?}"
            ),
            _ => {}
        }
        match self.want_audio {
            Some(Some(sid)) if self.has(sid) => assert_eq!(
                applied.audio.as_deref(),
                Some(sid),
                "the audio desire was not applied, trace {trace:?}, applied {applied:?}"
            ),
            Some(None) => assert_eq!(
                applied.audio, None,
                "the audio disable was not applied, trace {trace:?}, applied {applied:?}"
            ),
            _ => {}
        }
        // Text cannot be presented without video, so neither a collection
        // without a video stream nor a deselected video slot is something
        // a subtitle desire can be held to.
        let text_presentable = self.has_video() && applied.video.is_some();
        match self.want_subtitle {
            SubWant::Stream(sid) if self.has(sid) && text_presentable => assert_eq!(
                applied.subtitle.as_deref(),
                Some(sid),
                "the subtitle desire was not applied, trace {trace:?}, applied {applied:?}"
            ),
            SubWant::External if self.external_resolves() && text_presentable => assert_eq!(
                applied.subtitle.as_deref(),
                Some(EXT_SID),
                "the external subtitle desire was not applied, trace {trace:?}, \
                 applied {applied:?}"
            ),
            SubWant::Off => assert_eq!(
                applied.subtitle, None,
                "the subtitle disable was not applied, trace {trace:?}, applied {applied:?}"
            ),
            // An UNSET desire follows the pipeline, which auto-selects the
            // first stream of each kind, so ending with the text slot off
            // means a dispatch composed it away without anyone asking.
            // Only checked when no subtitle op ran at all: a desire RESET
            // by a failed external legitimately leaves an earlier disable.
            SubWant::Unset
                if !self.subtitle_ever_requested
                    && text_presentable
                    && self.default_of(StreamKind::Text).is_some() =>
            {
                assert!(
                    applied.subtitle.is_some(),
                    "text was deselected without a request, trace {trace:?}, \
                     applied {applied:?}"
                )
            }
            _ => {}
        }
    }
}

/// Exhaustive over every ordering of a small op alphabet: whatever the
/// interleaving of requests, confirmations, foreign auto-selects and
/// collection growth, the engine must converge and apply every desire the
/// collection can express. Catches lost desires, stale echoes overwriting
/// the applied state, and re-assertion loops.
#[test]
fn every_ordering_converges_on_the_desired_state() {
    const OPS: &[Op] = &[
        Op::Pump { paused: false },
        Op::Pump { paused: true },
        Op::Echo,
        Op::Foreign,
        Op::WantSubtitle(Some("t1")),
        Op::WantSubtitle(None),
        Op::WantAudio(Some("a1")),
        Op::WantVideo(None),
        Op::Grow,
    ];
    // Three starting points: a collection still growing (audio only,
    // decodebin3 has merged one input), a media that genuinely has no
    // video stream and never will, and the full one.
    let starts = [
        collection(&[("a0", StreamKind::Audio)]),
        collection(&[
            ("a0", StreamKind::Audio),
            ("a1", StreamKind::Audio),
            ("t0", StreamKind::Text),
            ("t1", StreamKind::Text),
        ]),
        avt_collection(),
    ];

    for start in starts {
        search(OPS, &start, 6);
    }
}

/// The same exhaustive treatment for the external-subtitle desire, whose
/// resolution needs TWO independent things to line up (the input producing
/// its stream id, decodebin3 merging that id in as text) plus a reset path
/// when the input dies.
#[test]
fn every_external_ordering_converges_on_the_desired_state() {
    const OPS: &[Op] = &[
        Op::Pump { paused: false },
        Op::Pump { paused: true },
        Op::Echo,
        Op::Foreign,
        Op::AttachExternal,
        Op::MaterializeExternal,
        Op::WantExternal,
        Op::ExternalGone,
        Op::WantSubtitle(None),
    ];
    search(OPS, &avt_collection(), 5);
}

/// Every op sequence up to `max_len`, each run against a fresh engine and
/// then settled and checked.
fn search(ops: &[Op], start: &[CollectionStream], max_len: usize) {
    for len in 1..=max_len {
        let total = ops.len().pow(len as u32);
        for n in 0..total {
            let mut trace = Vec::with_capacity(len);
            let mut rest = n;
            for _ in 0..len {
                trace.push(ops[rest % ops.len()]);
                rest /= ops.len();
            }
            let mut model = Model::new(start.to_vec());
            for op in &trace {
                model.apply(*op);
            }
            model.settle(&trace);
            model.check_desires(&trace);
        }
    }
}

#[test]
fn a_held_back_subtitle_must_not_starve_the_other_slots() {
    // The holdback keys off "the collection has no video", which is also
    // true for the whole life of a media that simply has none: every later
    // resolve would hit it and drop every other slot's request too.
    let mut engine = SelectionEngine::default();
    engine.collection_changed(collection(&[
        ("a0", StreamKind::Audio),
        ("a1", StreamKind::Audio),
        ("t0", StreamKind::Text),
        ("t1", StreamKind::Text),
    ]));
    engine.streams_selected(gst::Seqnum::next(), &sel(None, Some("a0"), Some("t0")));

    // Nothing can honour this while the collection has no video, so it
    // is held back rather than answered by an event that turns the
    // showing subtitle off.
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    assert_eq!(engine.pump(&ctx(true, false)), None);

    // An audio switch must go out, carrying the subtitle: a collection
    // with no video id cannot express "video on" either way, and stripping
    // the text stream would be a second deselect nobody asked for.
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(None, Some("a1"), Some("t1")))),
        "the holdback must defer only the subtitle, not every other slot"
    );
}

// The engine holds only the ADVISORY half of a deadline: when a wait ran
// out, never what to do about it. The cases below inject both the clock
// and the worker's decisions.

/// The deadline every case below arms with, and the re-arm period handed
/// to `due_deadlines`. Production values: `crate::SELECTION_DEADLINE` /
/// `crate::REFRESH_DEADLINE`.
const DEADLINE: Duration = Duration::from_secs(10);
/// Comfortably past [`DEADLINE`].
const LATE: Duration = Duration::from_secs(11);
/// What the worker's probe finds a never-applied selection left playing:
/// the state `settled_engine` starts from.
fn reality() -> TrackSelection {
    sel(Some("v0"), Some("a0"), Some("t0"))
}

/// An advisory deadline is exactly as alive as the dispatch it names, and
/// nothing disarms it: the next look drops an advisory whose seqnum no
/// longer names a live wait.
#[test]
fn a_deadline_fires_only_while_its_dispatch_is_in_flight() {
    let t0 = Instant::now();
    let mut engine = settled_engine();
    let seqnum = gst::Seqnum::next();
    let target = sel(Some("v0"), Some("a1"), Some("t0"));
    engine.selection_dispatched(seqnum, target.clone());
    engine.arm_selection_deadline(seqnum, t0 + DEADLINE, 2);

    // Before the due time, nothing.
    assert!(
        engine
            .due_deadlines(t0 + Duration::from_secs(1), DEADLINE)
            .is_empty()
    );
    // Still in flight when it comes due: the positive control.
    assert_eq!(
        engine.due_deadlines(t0 + LATE, DEADLINE),
        vec![DeadlineFire::Selection(seqnum)]
    );

    // The confirmation arrives after all. Nobody tells the advisory.
    engine.streams_selected(seqnum, &target);
    assert!(
        engine
            .due_deadlines(t0 + Duration::from_secs(60), DEADLINE)
            .is_empty(),
        "a confirmed dispatch still had a live deadline"
    );
    assert!(
        engine
            .due_deadlines(t0 + Duration::from_secs(600), DEADLINE)
            .is_empty(),
        "the dead advisory was re-armed instead of dropped"
    );
}

/// The two paths that abandon in-flight work without any confirmation -
/// a fresh collection and a load reset - take the deadlines with them.
/// The sibling of `new_collection_invalidates_in_flight_work`.
#[test]
fn collection_change_and_reset_disarm_deadlines() {
    let arm = |engine: &mut SelectionEngine, t0: Instant| {
        let select = gst::Seqnum::next();
        engine.selection_dispatched(select, sel(Some("v0"), Some("a1"), Some("t0")));
        engine.arm_selection_deadline(select, t0 + DEADLINE, 2);
        let refresh = gst::Seqnum::next();
        engine.refresh_dispatched(refresh);
        engine.arm_refresh_deadline(refresh, t0 + DEADLINE);
    };

    let t0 = Instant::now();
    let mut engine = settled_engine();
    arm(&mut engine, t0);
    engine.collection_changed(avt_collection());
    assert!(
        engine.due_deadlines(t0 + LATE, DEADLINE).is_empty(),
        "a new collection left deadlines armed against waits it abandoned"
    );

    let mut engine = settled_engine();
    arm(&mut engine, t0);
    engine.reset();
    assert!(
        engine.due_deadlines(t0 + LATE, DEADLINE).is_empty(),
        "a load reset left the previous item's deadlines armed"
    );
}

/// A fire re-arms rather than clears, so a wait that is still there gets
/// looked at again - but at most once per re-arm period, so a worker
/// parked behind a long job never returns to a queue of fires.
#[test]
fn a_fired_deadline_rearms_and_does_not_storm() {
    let t0 = Instant::now();
    let mut engine = settled_engine();
    let seqnum = gst::Seqnum::next();
    engine.selection_dispatched(seqnum, sel(Some("v0"), Some("a1"), Some("t0")));
    engine.arm_selection_deadline(seqnum, t0 + DEADLINE, 2);

    assert_eq!(
        engine.due_deadlines(t0 + LATE, DEADLINE),
        vec![DeadlineFire::Selection(seqnum)]
    );
    // Inside the re-arm window, however often the tick looks.
    for at in [
        t0 + LATE + Duration::from_millis(1),
        t0 + LATE + Duration::from_secs(5),
        t0 + LATE + DEADLINE - Duration::from_millis(1),
    ] {
        assert!(
            engine.due_deadlines(at, DEADLINE).is_empty(),
            "a second fire inside one re-arm period"
        );
    }
    // And once more past it.
    assert_eq!(
        engine.due_deadlines(t0 + LATE + DEADLINE, DEADLINE),
        vec![DeadlineFire::Selection(seqnum)]
    );
}

/// A timed-out dispatch becomes a SUPERSEDED record, not a forgotten one:
/// a confirmation that turns up late is then ours-but-stale, and neither
/// settles the dispatch that replaced it nor rewinds `applied`.
#[test]
fn timed_out_dispatch_becomes_a_superseded_echo() {
    let t0 = Instant::now();
    let mut engine = settled_engine();
    let first = gst::Seqnum::next();
    let first_target = sel(Some("v0"), Some("a1"), Some("t0"));
    engine.selection_dispatched(first, first_target.clone());
    engine.arm_selection_deadline(first, t0 + DEADLINE, 2);
    assert_eq!(
        engine.due_deadlines(t0 + LATE, DEADLINE),
        vec![DeadlineFire::Selection(first)]
    );
    assert_eq!(
        engine.selection_timed_out(first),
        TimeoutOutcome::Retry {
            target: first_target.clone(),
            retries_left: 1,
        }
    );

    // The next dispatch. A user request composed onto the same slot set
    // in the meantime, so the two targets differ and a rewind would show.
    let second = gst::Seqnum::next();
    let second_target = sel(Some("v0"), Some("a1"), Some("t1"));
    engine.selection_dispatched(second, second_target.clone());
    engine.arm_selection_deadline(second, t0 + LATE + DEADLINE, 1);

    // And NOW the first dispatch's confirmation arrives.
    engine.streams_selected(first, &first_target);
    assert_eq!(
        engine.selecting_target(second),
        Some(second_target.clone()),
        "the stale echo settled the dispatch that superseded it"
    );
    assert_eq!(
        engine.applied(),
        &second_target,
        "the stale echo rewound the applied selection"
    );
}

/// The end of the retry ladder: reporting reality rather than the request
/// must leave the engine CONVERGED, or it re-dispatches a selection the
/// pipeline has already refused three times, forever.
#[test]
fn retries_exhaust_into_a_converged_give_up() {
    let t0 = Instant::now();
    let mut engine = settled_engine();
    // Two slots, so the give-up's neutralization is pinned on both the
    // plain-stream and the subtitle desire.
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t1".into())));
    let target = sel(Some("v0"), Some("a1"), Some("t1"));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(target.clone()))
    );

    let mut now = t0;
    let mut seqnum = gst::Seqnum::next();
    engine.selection_dispatched(seqnum, target.clone());
    engine.arm_selection_deadline(seqnum, now + DEADLINE, 2);
    // Two retries, modelling the worker: probe says not applied,
    // re-dispatch under a fresh seqnum and re-arm with what is left.
    for expected in [1, 0] {
        now += LATE;
        assert_eq!(
            engine.due_deadlines(now, DEADLINE),
            vec![DeadlineFire::Selection(seqnum)]
        );
        let TimeoutOutcome::Retry {
            target,
            retries_left,
        } = engine.selection_timed_out(seqnum)
        else {
            panic!("the retries were exhausted early");
        };
        assert_eq!(retries_left, expected);
        seqnum = gst::Seqnum::next();
        engine.selection_dispatched(seqnum, target);
        engine.arm_selection_deadline(seqnum, now + DEADLINE, retries_left);
    }

    now += LATE;
    assert_eq!(
        engine.due_deadlines(now, DEADLINE),
        vec![DeadlineFire::Selection(seqnum)]
    );
    assert_eq!(
        engine.selection_timed_out(seqnum),
        TimeoutOutcome::Exhausted {
            target: target.clone()
        }
    );

    assert!(
        engine.selection_gave_up(seqnum, &reality()),
        "nothing overtook this give-up, so it must have been adopted"
    );
    assert_eq!(engine.applied(), &reality());
    // Converged: the unhonourable desire is UNSET, not rewritten and not
    // turned off, so nothing re-asserts and nothing was torn down.
    assert_eq!(engine.pump(&ctx(true, false)), None);
    assert!(!engine.has_dispatchable_work());
    // Convergence has to survive the next event: a cleared `dirty` alone
    // would be re-set by any foreign report, and a desire still pointing
    // at the refused track would re-assert there.
    engine.streams_selected(gst::Seqnum::next(), &reality());
    assert_eq!(engine.pump(&ctx(true, false)), None);

    // Still a live engine: the next request dispatches normally, against
    // the adopted reality rather than against the abandoned request.
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(sel(
            Some("v0"),
            Some("a1"),
            Some("t0")
        )))
    );
}

/// The refreshing latch, and its cure. `refreshing` blocks EVERY dispatch
/// and is cleared only by a top-level ASYNC_DONE, so a swallowed one
/// freezes the whole selection channel for the rest of the item.
#[test]
fn refresh_deadline_unlatches_the_pump() {
    let t0 = Instant::now();
    let mut engine = settled_engine();
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a1".into())));
    let target = sel(Some("v0"), Some("a1"), Some("t0"));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(target.clone()))
    );
    let select = gst::Seqnum::next();
    engine.selection_dispatched(select, target.clone());
    engine.streams_selected(select, &target);

    // The switch schedules the re-emit flush, which dispatches next.
    assert_eq!(engine.pump(&ctx(true, false)), Some(Command::RefreshSeek));
    let refresh = gst::Seqnum::next();
    engine.refresh_dispatched(refresh);
    engine.arm_refresh_deadline(refresh, t0 + DEADLINE);

    // Its ASYNC_DONE never comes. Every later request is now blocked.
    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some("a0".into())));
    assert_eq!(
        engine.pump(&ctx(true, false)),
        None,
        "an in-flight refresh must block dispatch; this test proves nothing otherwise"
    );

    // The deadline fires and the worker (modelled here) fails the refresh.
    assert_eq!(
        engine.due_deadlines(t0 + LATE, DEADLINE),
        vec![DeadlineFire::Refresh(refresh)]
    );
    engine.refresh_failed(refresh);
    assert_eq!(
        engine.pump(&ctx(true, false)),
        Some(Command::SelectStreams(reality())),
        "the request stranded behind the swallowed ASYNC_DONE never dispatched"
    );
}

/// Seeded splitmix64, mirroring `fcasttest::prng`: a seed fully
/// determines a run, and no test here ever draws from a thread-local RNG.
struct Prng(u64);

impl Prng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Liveness, randomized: no reachable interleaving leaves a wait that
/// nothing will ever look at again.
///
/// 1. INVARIANT, after every step: an in-flight selection or refresh has a
///    matching advisory. A wait without one is a latch.
/// 2. LIVENESS: from wherever the random ops left it, a BOUNDED number of fires
///    (worker decisions modelled) leaves nothing in flight and the engine still
///    dispatching for a fresh request.
///
/// The op alphabet includes the swallow this all exists for: a
/// confirmation, or an ASYNC_DONE, that simply never arrives.
#[test]
fn no_reachable_state_leaves_a_wait_without_a_deadline() {
    const RUNS: u64 = 400;
    const OPS_PER_RUN: u64 = 14;
    /// Generous: a retry ladder is three fires and a refresh is one.
    const FIRE_BOUND: usize = 12;

    for seed in 0..RUNS {
        let mut prng = Prng(seed);
        let mut engine = settled_engine();
        let mut now = Instant::now();
        let mut trace: Vec<&str> = Vec::new();

        let check = |engine: &SelectionEngine, trace: &[&str]| {
            if let Some((seqnum, _)) = &engine.selecting {
                let armed = engine
                    .advisory(DeadlineKind::Selection)
                    .is_some_and(|advisory| advisory.seqnum == *seqnum);
                assert!(
                    armed,
                    "a selection is in flight with no deadline: {trace:?}"
                );
            }
            if let Some(seqnum) = engine.refreshing {
                let armed = engine
                    .advisory(DeadlineKind::Refresh)
                    .is_some_and(|advisory| advisory.seqnum == seqnum);
                assert!(armed, "a refresh is in flight with no deadline: {trace:?}");
            }
        };

        for _ in 0..OPS_PER_RUN {
            match prng.below(8) {
                0 => {
                    trace.push("want-audio");
                    let sid = if prng.next() & 1 == 0 { "a0" } else { "a1" };
                    engine.request(TrackSlot::Audio, TrackTarget::Stream(Some(sid.into())));
                }
                1 => {
                    trace.push("want-subtitle");
                    let sid = match prng.below(3) {
                        0 => Some("t0".to_string()),
                        1 => Some("t1".to_string()),
                        _ => None,
                    };
                    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(sid));
                }
                2 => {
                    trace.push("pump");
                    // The pump and the arming that follows it are ONE
                    // step, exactly as `pump_selection` does them under
                    // one engine lock.
                    let paused = prng.next() & 1 == 0;
                    match engine.pump(&ctx(true, paused)) {
                        Some(Command::SelectStreams(target)) => {
                            let seqnum = gst::Seqnum::next();
                            engine.selection_dispatched(seqnum, target);
                            engine.arm_selection_deadline(seqnum, now + DEADLINE, 2);
                        }
                        Some(Command::RefreshSeek) => {
                            let seqnum = gst::Seqnum::next();
                            engine.refresh_dispatched(seqnum);
                            engine.arm_refresh_deadline(seqnum, now + DEADLINE);
                        }
                        // Never reached in db3-owned mode, and asserting
                        // that here would pin an unrelated contract.
                        Some(Command::ConfirmApplied(_)) | None => {}
                    }
                }
                3 => {
                    trace.push("confirm");
                    if let Some((seqnum, target)) = engine.selecting.clone() {
                        engine.streams_selected(seqnum, &target);
                    }
                }
                4 => {
                    // The whole point of the phase: the message dies.
                    trace.push("swallow");
                }
                5 => {
                    trace.push("collection");
                    engine.collection_changed(avt_collection());
                }
                6 => {
                    trace.push("reset");
                    engine.reset();
                    engine.collection_changed(avt_collection());
                }
                _ => {
                    trace.push("fire");
                    now += LATE;
                    for fire in engine.due_deadlines(now, DEADLINE) {
                        worker(&mut engine, fire, now);
                    }
                }
            }
            check(&engine, &trace);
        }

        // Liveness. Nothing but time and the modelled worker from here.
        let mut fires = 0;
        while engine.selecting.is_some() || engine.refreshing.is_some() {
            assert!(
                fires < FIRE_BOUND,
                "a wait survived {FIRE_BOUND} deadline fires: {trace:?}"
            );
            fires += 1;
            now += LATE;
            let due = engine.due_deadlines(now, DEADLINE);
            assert!(
                !due.is_empty(),
                "something is in flight but no deadline came due: {trace:?}"
            );
            for fire in due {
                worker(&mut engine, fire, now);
            }
            check(&engine, &trace);
        }

        // And the engine still works. A fresh collection puts it on known
        // ground, then a request for the audio track that is NOT applied
        // must dispatch.
        engine.collection_changed(avt_collection());
        let wanted = if engine.applied().audio.as_deref() == Some("a0") {
            "a1"
        } else {
            "a0"
        };
        engine.request(TrackSlot::Audio, TrackTarget::Stream(Some(wanted.into())));
        let command = engine.pump(&ctx(true, false));
        let Some(Command::SelectStreams(target)) = command else {
            panic!("the engine stopped dispatching after {trace:?}: {command:?}");
        };
        assert_eq!(target.audio.as_deref(), Some(wanted));
    }
}

/// The worker's half of a deadline fire, as the unit tests model it: the
/// probe never finds the selection applied (the pessimistic case, which
/// is the one that has to terminate), and a refresh that ran out failed.
fn worker(engine: &mut SelectionEngine, fire: DeadlineFire, now: Instant) {
    match fire {
        DeadlineFire::Selection(seqnum) => match engine.selection_timed_out(seqnum) {
            TimeoutOutcome::NotInFlight => {}
            TimeoutOutcome::Retry {
                target,
                retries_left,
            } => {
                let retry = gst::Seqnum::next();
                engine.selection_dispatched(retry, target);
                engine.arm_selection_deadline(retry, now + DEADLINE, retries_left);
            }
            TimeoutOutcome::Exhausted { .. } => {
                // Nothing here can overtake the give-up, so the adopted
                // answer is not interesting to this model.
                let _ = engine.selection_gave_up(seqnum, &reality());
            }
        },
        DeadlineFire::Refresh(seqnum) => engine.refresh_failed(seqnum),
    }
}

/// With NOTHING refreshing, `refresh_superseded` is false (a caller may
/// force a refresh the engine never tracked, and that job must run) while
/// its `==` sibling `refresh_in_flight` is also false. The two differ
/// ONLY on the None case, and collapsing them flips one of these.
#[test]
fn no_refresh_in_flight_is_not_supersession() {
    let engine = SelectionEngine::default();
    let seqnum = gst::Seqnum::next();
    assert!(!engine.refresh_superseded(seqnum));
    assert!(!engine.refresh_in_flight(seqnum));
}

/// With a refresh TRACKED, the two split exactly on the seqnum: the
/// tracked one is in flight and not superseded, any other is superseded
/// and not in flight.
#[test]
fn a_tracked_refresh_splits_in_flight_from_superseded_by_seqnum() {
    let mut engine = SelectionEngine::default();
    let ours = gst::Seqnum::next();
    let other = gst::Seqnum::next();
    engine.refresh_dispatched(ours);
    assert!(engine.refresh_in_flight(ours));
    assert!(!engine.refresh_superseded(ours));
    assert!(!engine.refresh_in_flight(other));
    assert!(engine.refresh_superseded(other));
    // A failure report for a foreign seqnum clears nothing.
    engine.refresh_failed(other);
    assert!(engine.refresh_in_flight(ours));
    // Ours does.
    engine.refresh_failed(ours);
    assert!(!engine.refresh_in_flight(ours));
    assert!(!engine.refresh_superseded(ours));
}

/// `selection_pending` and `selecting_seqnum` read the same live wait:
/// empty until a dispatch, naming it while it waits, empty again once its
/// confirmation settles it.
#[test]
fn the_live_wait_is_visible_from_dispatch_to_confirmation() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    assert!(!engine.selection_pending());
    assert_eq!(engine.selecting_seqnum(), None);

    let seqnum = gst::Seqnum::next();
    let target = sel(Some("v0"), Some("a0"), Some("t1"));
    engine.selection_dispatched(seqnum, target.clone());
    assert!(engine.selection_pending());
    assert_eq!(engine.selecting_seqnum(), Some(seqnum));

    engine.streams_selected(seqnum, &target);
    assert!(!engine.selection_pending());
    assert_eq!(engine.selecting_seqnum(), None);
}

/// An explicit subtitle OFF is a desire, not an applied state: an applied
/// text slot that happens to be empty must not read as one.
#[test]
fn an_empty_applied_text_slot_is_not_an_explicit_off() {
    let mut engine = SelectionEngine::default();
    engine.collection_changed(avt_collection());
    engine.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), None));
    assert_eq!(engine.subtitle_sid(), None, "applied text is empty");
    assert!(
        !engine.subtitle_explicitly_off(),
        "nobody asked for off; decodebin3 just reported none"
    );

    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(None));
    assert!(engine.subtitle_explicitly_off());

    // Any other desire ends the explicit off.
    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
    assert!(!engine.subtitle_explicitly_off());
    engine.request(
        TrackSlot::Subtitle,
        TrackTarget::ExternalSubtitle(ExternalSubId(4)),
    );
    assert!(!engine.subtitle_explicitly_off());
}

/// `desires_external` answers for the DESIRED external only: the right
/// id, not another id, not a stream desire, and not after the external
/// failed and the desire was dropped.
#[test]
fn desires_external_names_exactly_the_desired_id() {
    const EXT: ExternalSubId = ExternalSubId(7);
    const OTHER: ExternalSubId = ExternalSubId(8);
    let mut engine = SelectionEngine::default();
    assert!(!engine.desires_external(EXT));

    engine.request(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(EXT));
    assert!(engine.desires_external(EXT));
    assert!(!engine.desires_external(OTHER));

    engine.external_gone(EXT);
    assert!(
        !engine.desires_external(EXT),
        "the failed desire is dropped"
    );

    engine.request(TrackSlot::Subtitle, TrackTarget::Stream(Some("t0".into())));
    assert!(!engine.desires_external(EXT));
}

/// `video_ids` is the derived replacement for the deleted routing mirror,
/// so it must answer exactly what that mirror held: the collection's video
/// ids in collection order, and EMPTY whenever nothing is advertised (the
/// "kinds are unknowable" state a reader must not read as "no video").
#[test]
fn video_ids_names_the_collection_video_streams_and_is_empty_when_unknowable() {
    let mut engine = SelectionEngine::default();
    assert!(
        engine.video_ids().is_empty(),
        "nothing advertised yet is unknowable, not no-video"
    );

    engine.collection_changed(collection(&[
        ("a0", StreamKind::Audio),
        ("v1", StreamKind::Video),
        ("t0", StreamKind::Text),
        ("v0", StreamKind::Video),
    ]));
    assert_eq!(engine.video_ids(), vec!["v1".to_string(), "v0".to_string()]);

    engine.collection_changed(collection(&[("a0", StreamKind::Audio)]));
    assert!(
        engine.video_ids().is_empty(),
        "an audio-only collection has no video ids"
    );

    engine.collection_changed(avt_collection());
    assert_eq!(engine.video_ids(), vec!["v0".to_string()]);

    // Both per-item resets drop the collection with everything else, so the
    // gap before the incoming item's collection reads as unknowable.
    engine.reset_across_gapless();
    assert!(engine.video_ids().is_empty());
    engine.collection_changed(avt_collection());
    engine.reset();
    assert!(engine.video_ids().is_empty());
}
