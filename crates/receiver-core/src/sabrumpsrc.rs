// URI handler and source bin for the SABR/UMP protocol (`sabrump://` scheme).
//
// URI format: `sabrump://<videoId>?spec=<base64url(JSON SabrStreamSpec)>`.

use gst::glib::{self, types::StaticType};

mod imp {
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, LazyLock};
    use std::time::Duration;

    use bytes::Bytes;
    use gst::{glib, prelude::*, subclass::prelude::*};
    use sabrump::spec::Role;
    use sabrump::{SabrFormat, SabrSession, SabrStreamSpec, SabrTransport};
    use tokio::task::JoinHandle;
    use parking_lot::Mutex;

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "sabrumpsrc",
            gst::DebugColorFlags::empty(),
            Some("SABR/UMP source bin"),
        )
    });

    const AWAIT_TIMEOUT: Duration = Duration::from_millis(500);
    /// Feeder loops at 20ms. Give up on a never-arriving init after ~10s.
    const INIT_WAIT_LIMIT: u32 = 500;

    struct Branch {
        appsrc: gst_app::AppSrc,
        role: Role,
        alternates: Vec<SabrFormat>,
        /// Set by the appsrc `enough-data` signal and cleared by `need-data`.
        /// The feeder paces on this in its own loop, not by blocking inside
        /// `push_buffer`, so it stays responsive to seeks while paused.
        enough: Arc<AtomicBool>,
    }

    #[derive(Default)]
    struct State {
        uri: Option<String>,
        session: Option<SabrSession>,
        branches: Vec<Branch>,
        running: Option<Arc<AtomicBool>>,
        /// The session pump task plus one feeder task per branch, all running on
        /// the shared tokio runtime. Aborted on teardown.
        tasks: Vec<JoinHandle<()>>,
        /// Bumped on each seek so feeders abandon their current position and
        /// re-feed from the session's new one.
        seek_gen: Arc<AtomicU64>,
        /// Seqnum of the last seek forwarded, to dedupe the per-branch probes.
        last_seek_seqnum: Option<gst::Seqnum>,
        duration_us: i64,
        is_live: bool,
    }

    #[derive(Default)]
    pub struct SabrumpSrc {
        state: Mutex<State>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SabrumpSrc {
        const NAME: &str = "SabrumpSrc";
        type Type = super::SabrumpSrc;
        type ParentType = gst::Bin;
        type Interfaces = (gst::URIHandler,);
    }

    impl ObjectImpl for SabrumpSrc {}
    impl GstObjectImpl for SabrumpSrc {}

    impl ElementImpl for SabrumpSrc {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let src = gst::PadTemplate::new(
                    "src_%u",
                    gst::PadDirection::Src,
                    gst::PadPresence::Sometimes,
                    &gst::Caps::new_any(),
                )
                .unwrap();
                vec![src]
            });
            PAD_TEMPLATES.as_ref()
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            gst::debug!(CAT, "change_state {transition:?}");
            match transition {
                gst::StateChange::ReadyToPaused => {
                    let ret = self.parent_change_state(transition)?;
                    self.start_streaming();
                    Ok(ret)
                }
                gst::StateChange::PausedToReady => {
                    if let Some(running) = self.state.lock().running.as_ref() {
                        running.store(false, Ordering::Release);
                    }
                    // Tear down unconditionally. Even if the parent transition
                    // errors (e.g. a stuck live pipeline), the session pump must
                    // be released or it keeps polling forever.
                    let ret = self.parent_change_state(transition);
                    self.stop_streaming();
                    ret
                }
                _ => self.parent_change_state(transition),
            }
        }
    }

    impl BinImpl for SabrumpSrc {}

    impl URIHandlerImpl for SabrumpSrc {
        const URI_TYPE: gst::URIType = gst::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["sabrump"]
        }

        fn uri(&self) -> Option<String> {
            self.state.lock().uri.clone()
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            let spec = parse_spec(uri).map_err(|msg| {
                glib::Error::new(gst::URIError::BadUri, &format!("invalid sabrump URI: {msg}"))
            })?;

            let client = build_reqwest_client()
                .map_err(|msg| glib::Error::new(gst::ResourceError::OpenRead, &msg))?;
            let session = SabrSession::new(spec.clone(), SabrTransport::http(client));

            let duration_us = spec.duration_us;
            let is_live = spec.is_live;

            let element = self.obj();
            let bin: &gst::Bin = element.upcast_ref();

            // Shared across both live branches: the first buffer's PTS, used to
            // rebase absolute media timestamps to a zero-based timeline so the
            // sinks can schedule them. One offset also keeps A/V in sync.
            let live_offset = Arc::new(AtomicI64::new(i64::MIN));

            // Each branch's parsebin exposes exactly one elementary stream, so
            // we know up front how many ghost src pads the bin will grow. Track
            // them so we can emit `no-more-pads` once they're all present,
            // letting decodebin3/playbin3 finalise linking deterministically.
            let expected_pads = usize::from(!spec.video_formats.is_empty())
                + usize::from(!spec.audio_formats.is_empty());
            let exposed_pads = Arc::new(AtomicUsize::new(0));

            let mut branches = Vec::new();
            if !spec.video_formats.is_empty() {
                branches.push(self.build_branch(
                    bin,
                    Role::Video,
                    spec.video_formats.clone(),
                    duration_us,
                    is_live,
                    live_offset.clone(),
                    expected_pads,
                    exposed_pads.clone(),
                )?);
                session.set_demand_alternates(Role::Video, spec.video_formats.clone(), 0);
            }
            if !spec.audio_formats.is_empty() {
                branches.push(self.build_branch(
                    bin,
                    Role::Audio,
                    spec.audio_formats.clone(),
                    duration_us,
                    is_live,
                    live_offset.clone(),
                    expected_pads,
                    exposed_pads.clone(),
                )?);
                session.set_demand_alternates(Role::Audio, spec.audio_formats.clone(), 0);
            }
            if branches.is_empty() {
                return Err(glib::Error::new(
                    gst::URIError::BadUri,
                    "sabrump spec has no audio or video formats",
                ));
            }

            let mut state = self.state.lock();
            // Release any previous session so its pump task exits. Otherwise the
            // old pump keeps polling (and logging) forever after being replaced.
            if let Some(old) = state.session.take() {
                old.release();
            }
            state.uri = Some(uri.to_owned());
            state.session = Some(session);
            state.branches = branches;
            state.duration_us = duration_us;
            state.is_live = is_live;
            Ok(())
        }
    }

    impl SabrumpSrc {
        /// Build one `appsrc → parsebin` branch and ghost the parsed elementary
        /// pads out of the bin, wiring up seek handling on the appsrc.
        /// `parsebin` typefinds the container from the bytes (fMP4 → qtdemux,
        /// WebM → matroskademux internally), so a branch works regardless of
        /// which container the server's ABR delivers. YouTube commonly pairs
        /// AAC/MP4 audio with VP9/AV1 WebM video.
        #[allow(clippy::too_many_arguments)]
        fn build_branch(
            &self,
            bin: &gst::Bin,
            role: Role,
            alternates: Vec<SabrFormat>,
            duration_us: i64,
            is_live: bool,
            live_offset: Arc<AtomicI64>,
            expected_pads: usize,
            exposed_pads: Arc<AtomicUsize>,
        ) -> Result<Branch, glib::Error> {
            // VOD only. Live seeking (DVR window) is not handled yet.
            let seekable = duration_us > 0 && !is_live;
            let stream_type = if seekable {
                gst_app::AppStreamType::Seekable
            } else {
                gst_app::AppStreamType::Stream
            };

            let appsrc = gst_app::AppSrc::builder()
                .stream_type(stream_type)
                // TIME format so the demuxer is driven with a time segment (like
                // DASH), and seeks are expressed and handled in time.
                .format(gst::Format::Time)
                // Live sources must not block preroll. With is_live=false a live
                // stream that hasn't produced a decodable buffer wedges the
                // pipeline in async PAUSED, so it can never be torn down.
                .is_live(is_live)
                .do_timestamp(false)
                // No caps. parsebin typefinds the container (fMP4/WebM) from the
                // pushed bytes, so we don't assert a container the ABR-selected
                // format might not match.
                .build();
            // Do NOT block inside push_buffer for pacing. While paused the queue
            // fills and a blocked push can't observe seeks (the flush wakes it
            // unreliably when paused). Instead pace in the feeder loop off the
            // enough-data/need-data signals, so the feeder always stays
            // responsive to the seek generation.
            appsrc.set_property("block", false);
            appsrc.set_property("max-bytes", 8_000_000u64);
            if seekable {
                appsrc.set_duration(gst::ClockTime::from_useconds(duration_us as u64));
            }

            // `enough` starts true so the feeder waits for the first need-data
            // rather than racing ahead before the pipeline is prerolling.
            let enough = Arc::new(AtomicBool::new(true));

            // Reposition the session when the appsrc is seeked. Pace feeding via
            // the demand signals.
            {
                let elem_weak = self.obj().downgrade();
                let enough_need = enough.clone();
                let enough_enough = enough.clone();
                appsrc.set_callbacks(
                    gst_app::AppSrcCallbacks::builder()
                        .seek_data(move |_appsrc, offset| {
                            let target_us = (offset / 1000) as i64;
                            gst::debug!(CAT, "appsrc seek-data offset={offset}ns -> {target_us}us");
                            if let Some(elem) = elem_weak.upgrade() {
                                elem.imp().reposition(target_us);
                            }
                            true
                        })
                        .need_data(move |_appsrc, _length| {
                            enough_need.store(false, Ordering::Release);
                        })
                        .enough_data(move |_appsrc| {
                            enough_enough.store(true, Ordering::Release);
                        })
                        .build(),
                );
            }

            let parsebin = gst::ElementFactory::make("parsebin")
                .build()
                .map_err(|e| glib::Error::new(gst::CoreError::MissingPlugin, &e.to_string()))?;

            bin.add_many([appsrc.upcast_ref::<gst::Element>(), &parsebin])
                .map_err(|e| glib::Error::new(gst::CoreError::Failed, &e.to_string()))?;
            gst::Element::link_many([appsrc.upcast_ref::<gst::Element>(), &parsebin])
                .map_err(|e| glib::Error::new(gst::CoreError::Failed, &e.to_string()))?;

            parsebin.connect_pad_added({
                let bin_weak = bin.downgrade();
                let elem_weak = self.obj().downgrade();
                let live_offset = live_offset.clone();
                move |_, pad| {
                    let Some(bin) = bin_weak.upgrade() else {
                        return;
                    };
                    // Create the ghost from our `src_%u` template with an
                    // explicit unique name. Every parsebin names its first src
                    // pad `src_0`, so a template-derived name would just reuse
                    // the target's `src_0` and collide across branches. We
                    // assign `src_<n>` from the shared counter ourselves (as
                    // demuxers do for their `%u` sometimes-pads), which also
                    // drives no-more-pads.
                    let idx = exposed_pads.fetch_add(1, Ordering::AcqRel);
                    let Some(templ) = bin.pad_template("src_%u") else {
                        gst::warning!(CAT, "missing src_%u pad template");
                        return;
                    };
                    let ghost = match gst::GhostPad::builder_from_template_with_target(&templ, pad) {
                        Ok(builder) => builder.name(format!("src_{idx}")).build(),
                        Err(e) => {
                            gst::warning!(CAT, "failed to ghost pad: {e}");
                            return;
                        }
                    };
                    let _ = ghost.set_active(true);

                    add_diag_probe(&ghost, role);

                    // Live streams carry absolute media timestamps. Rebase them
                    // to a zero-based timeline (shared offset across branches)
                    // so the sinks can schedule them.
                    if is_live {
                        add_live_rebase_probe(&ghost, live_offset.clone());
                    }

                    if seekable {
                        add_seek_probes(&ghost, elem_weak.clone(), duration_us);
                    }

                    if bin.add_pad(&ghost).is_err() {
                        gst::warning!(CAT, "failed to add ghost pad");
                        return;
                    }

                    // Once every branch has exposed its stream, tell downstream
                    // no further pads are coming.
                    if idx + 1 >= expected_pads {
                        bin.no_more_pads();
                    }
                }
            });

            Ok(Branch {
                appsrc,
                role,
                alternates,
                enough,
            })
        }

        fn start_streaming(&self) {
            let mut state = self.state.lock();
            if state.running.is_some() {
                return;
            }
            let session = match &state.session {
                Some(s) => s.clone(),
                None => return,
            };

            let running = Arc::new(AtomicBool::new(true));
            state.running = Some(running.clone());
            let seek_gen = state.seek_gen.clone();

            // The session pump and the per-branch feeders all run as tasks on
            // the shared runtime, not dedicated threads.
            let mut tasks = Vec::new();
            tasks.push(crate::RUNTIME.spawn({
                let session = session.clone();
                async move { session.run().await }
            }));
            for branch in &state.branches {
                let session = session.clone();
                let appsrc = branch.appsrc.clone();
                let role = branch.role;
                let alternates = branch.alternates.clone();
                let running = running.clone();
                let seek_gen = seek_gen.clone();
                let enough = branch.enough.clone();
                let elem = self.obj().downgrade();
                tasks.push(crate::RUNTIME.spawn(async move {
                    feed(session, appsrc, role, alternates, running, seek_gen, enough, elem).await;
                }));
            }
            state.tasks = tasks;
        }

        fn stop_streaming(&self) {
            let (tasks, session) = {
                let mut state = self.state.lock();
                if let Some(running) = state.running.as_ref() {
                    running.store(false, Ordering::Release);
                }
                (std::mem::take(&mut state.tasks), state.session.clone())
            };
            // Release first so the tasks observe shutdown and unwind at their
            // next await. Abort as a backstop for any stuck on I/O.
            if let Some(session) = session {
                session.release();
            }
            for handle in tasks {
                handle.abort();
            }
            self.state.lock().running = None;
        }

        /// Reposition the SABR session to `target_us` (called from an appsrc
        /// `seek-data` callback). appsrc has already flushed itself. We just
        /// move the session and bump the generation so feeders re-feed.
        /// Coalesces the two per-branch seek callbacks via `restart`'s return
        /// value.
        fn reposition(&self, target_us: i64) {
            let (session, seek_gen) = {
                let state = self.state.lock();
                let Some(session) = state.session.clone() else {
                    return;
                };
                (session, state.seek_gen.clone())
            };
            if session.restart(target_us, false) {
                seek_gen.fetch_add(1, Ordering::AcqRel);
                gst::debug!(CAT, "repositioned session to {target_us}us");
            }
        }

        /// Forward a seek event to every branch's appsrc so they all flush and
        /// reposition. The pipeline seek only reaches one branch's ghost pad, so
        /// without this the other branch's feeder can stay blocked in
        /// `push_buffer` and never notice the seek. Deduped by seqnum since both
        /// ghost pads may fire the same seek.
        fn forward_seek_to_all(&self, event: &gst::Event) {
            let appsrcs = {
                let mut state = self.state.lock();
                if state.last_seek_seqnum == Some(event.seqnum()) {
                    return;
                }
                state.last_seek_seqnum = Some(event.seqnum());
                state
                    .branches
                    .iter()
                    .map(|b| b.appsrc.clone())
                    .collect::<Vec<_>>()
            };
            for appsrc in &appsrcs {
                let _ = appsrc.send_event(event.clone());
            }
        }
    }

    /// Add probes to a ghost pad. Forward SEEK events into every branch's
    /// appsrc (so the appsrcs, not the demuxer, handle the seek) and answer
    /// SEEKING queries as seekable.
    fn add_seek_probes(
        ghost: &gst::GhostPad,
        elem_weak: glib::WeakRef<super::SabrumpSrc>,
        duration_us: i64,
    ) {
        ghost.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_pad, info| {
            let Some(gst::PadProbeData::Event(ref event)) = info.data else {
                return gst::PadProbeReturn::Ok;
            };
            if let gst::EventView::Seek(_) = event.view() {
                gst::debug!(CAT, "forwarding seek event to all appsrcs");
                if let Some(elem) = elem_weak.upgrade() {
                    elem.imp().forward_seek_to_all(event);
                }
                // Handled here. Don't let the demuxer attempt a byte seek.
                return gst::PadProbeReturn::Handled;
            }
            gst::PadProbeReturn::Ok
        });

        ghost.add_probe(gst::PadProbeType::QUERY_UPSTREAM, move |_pad, info| {
            let Some(query) = info.query_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            if let gst::QueryViewMut::Seeking(q) = query.view_mut()
                && q.format() == gst::Format::Time
            {
                q.set(
                    true,
                    gst::ClockTime::ZERO,
                    gst::ClockTime::from_useconds(duration_us.max(0) as u64),
                );
                return gst::PadProbeReturn::Handled;
            }
            gst::PadProbeReturn::Ok
        });
    }

    /// Log the first segment event and first few buffers out of the demuxer, so
    /// we can see the actual timestamps a stream carries (esp. live, whose
    /// media timestamps are absolute and may need rebasing before the sink will
    /// schedule them).
    fn add_diag_probe(ghost: &gst::GhostPad, role: Role) {
        let count = std::sync::Arc::new(AtomicU64::new(0));
        ghost.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_pad, info| {
                match &info.data {
                    Some(gst::PadProbeData::Event(event)) => {
                        if let gst::EventView::Segment(s) = event.view() {
                            gst::debug!(CAT, "diag {role:?} segment {:?}", s.segment());
                        }
                    }
                    Some(gst::PadProbeData::Buffer(buf)) => {
                        let n = count.fetch_add(1, Ordering::AcqRel);
                        if n < 3 {
                            gst::debug!(
                                CAT,
                                "diag {role:?} buffer#{n} pts={:?} dts={:?} dur={:?}",
                                buf.pts(),
                                buf.dts(),
                                buf.duration(),
                            );
                        }
                    }
                    _ => {}
                }
                gst::PadProbeReturn::Ok
            },
        );
    }

    /// Rebase absolute live media timestamps to a zero-based timeline. Live
    /// SABR fragments carry absolute PTS/DTS (wall-clock-ish, ~1e13 ns), which
    /// the sinks would schedule ~days in the future. We force a zero-based TIME
    /// segment and subtract a single shared offset (the first buffer's PTS seen
    /// on either branch) from every buffer, so playback starts at running-time 0
    /// and video/audio stay aligned to the same origin.
    fn add_live_rebase_probe(ghost: &gst::GhostPad, offset: Arc<AtomicI64>) {
        ghost.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_pad, info| {
                match &mut info.data {
                    Some(gst::PadProbeData::Event(event)) => {
                        if let gst::EventView::Segment(_) = event.view() {
                            // Replace with a plain zero-based, open-ended TIME
                            // segment so rebased buffers schedule from 0.
                            let seg = gst::FormattedSegment::<gst::ClockTime>::new();
                            *event = gst::event::Segment::new(seg.as_ref());
                        }
                    }
                    Some(gst::PadProbeData::Buffer(buffer)) => {
                        let Some(pts) = buffer.pts() else {
                            return gst::PadProbeReturn::Ok;
                        };
                        let pts_ns = pts.nseconds() as i64;
                        // First buffer on either branch fixes the shared origin.
                        let _ = offset.compare_exchange(
                            i64::MIN,
                            pts_ns,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        let off = offset.load(Ordering::Acquire);
                        let buf = buffer.make_mut();
                        let rebase = |t: gst::ClockTime| {
                            gst::ClockTime::from_nseconds((t.nseconds() as i64 - off).max(0) as u64)
                        };
                        buf.set_pts(Some(rebase(pts)));
                        if let Some(dts) = buf.dts() {
                            buf.set_dts(Some(rebase(dts)));
                        }
                    }
                    _ => {}
                }
                gst::PadProbeReturn::Ok
            },
        );
    }

    enum PushOutcome {
        Ok,
        /// Transient: the appsrc is flushing (seek in progress) or still holds a
        /// stale EOS from a previous end-of-stream that a pending seek's flush
        /// hasn't cleared yet. Loop back to `'restart` and retry. Do NOT exit,
        /// or a post-EOS seek would permanently kill the feeder.
        Retry,
        /// A real downstream error. Stop feeding.
        Stop,
    }

    /// Per-track feeder: push the init segment, then push media segments in
    /// sequence order, advancing the session's demand window as it consumes.
    /// Restarts from the top (re-pushing init, like DASH) whenever a seek bumps
    /// `seek_gen`.
    /// Result of waiting for appsrc demand before a push.
    enum Demand {
        /// appsrc wants data. Go ahead and push.
        Go,
        /// The seek generation changed. Restart the feed loop.
        Restart,
        /// Shutting down. Exit the feeder.
        Stop,
    }

    /// Whether the feed loop should restart from the top. It restarts when a
    /// client seek bumped our `seek_gen`, when the *server* repositioned the
    /// stream (a `SABR_SEEK` or live-window clamp) bumping the session's seek
    /// generation, or when the server switched us to a different format among
    /// the alternates (ABR), after which the pump fills a *different* buffer. In
    /// every case the feeder's sequence cursor and/or buffer is stale, so it
    /// must re-resolve from the top. Otherwise it waits forever for data that
    /// will never arrive on the buffer it is watching.
    fn should_restart(
        session: &SabrSession,
        role: Role,
        format: &SabrFormat,
        seek_gen: &Arc<AtomicU64>,
        generation: u64,
        server_gen: u64,
    ) -> bool {
        seek_gen.load(Ordering::Acquire) != generation
            || session.server_seek_generation() != server_gen
            || session
                .active_format_key(role)
                .is_some_and(|k| k != format.key())
    }

    /// Wait (in our own loop, checking the seek generation each tick) until the
    /// appsrc asks for more data. This replaces blocking inside `push_buffer`,
    /// so a paused pipeline (queue full → `enough` stays set) never traps the
    /// feeder where it can't see a seek.
    #[allow(clippy::too_many_arguments)]
    async fn await_demand(
        session: &SabrSession,
        role: Role,
        format: &SabrFormat,
        running: &Arc<AtomicBool>,
        seek_gen: &Arc<AtomicU64>,
        generation: u64,
        server_gen: u64,
        enough: &Arc<AtomicBool>,
    ) -> Demand {
        loop {
            if !running.load(Ordering::Acquire) || session.is_released() {
                return Demand::Stop;
            }
            if should_restart(session, role, format, seek_gen, generation, server_gen) {
                return Demand::Restart;
            }
            if !enough.load(Ordering::Acquire) {
                return Demand::Go;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn feed(
        session: SabrSession,
        appsrc: gst_app::AppSrc,
        role: Role,
        alternates: Vec<SabrFormat>,
        running: Arc<AtomicBool>,
        seek_gen: Arc<AtomicU64>,
        enough: Arc<AtomicBool>,
        elem: glib::WeakRef<super::SabrumpSrc>,
    ) {
        // Re-resolved on each (re)start. `adopt_server_format` can switch the
        // session to a different format among the alternates (server-side ABR),
        // after which the pump fills that format's buffer. A feeder pinned to
        // the original buffer would then wait forever.
        let mut format = match session.active_format(role).or_else(|| alternates.first().cloned()) {
            Some(f) => f,
            None => return,
        };
        let mut buffer = session.buffer_for(&format);

        // Snapshot the server-seek generation. When the server repositions us,
        // the sequence cursor becomes stale, so we re-sync from the buffer front
        // and flush the appsrc. Unlike a client seek, nothing else flushed the
        // queued, now-stale buffers.
        let mut last_server_gen = session.server_seek_generation();

        'restart: loop {
            if !running.load(Ordering::Acquire) || session.is_released() {
                return;
            }
            let server_gen = session.server_seek_generation();
            if server_gen != last_server_gen {
                last_server_gen = server_gen;
                flush_appsrc(&appsrc);
            }
            // Follow a server-side format switch (see `format`/`buffer` above).
            // Re-point at the newly chosen format's buffer before re-priming
            // with its init segment.
            if let Some(active) = session.active_format(role)
                && active.key() != format.key()
            {
                gst::debug!(
                    CAT,
                    "feeder {role:?} format switch itag {} -> {}",
                    format.itag,
                    active.itag
                );
                buffer = session.buffer_for(&active);
                format = active;
            }
            let generation = seek_gen.load(Ordering::Acquire);
            gst::debug!(CAT, "feeder {role:?} (re)start gen={generation}");

            // Acquire the init segment (ftyp+moov) to open a fresh fragmented
            // stream after each (re)start. VOD announces a dedicated init
            // segment. Live fragments are self-initializing (each carries the
            // init as a prefix), so fall back to extracting that prefix from the
            // first complete media segment. If neither shows up, give up rather
            // than spin forever. A hung feeder never prerolls, wedging the
            // pipeline so it can't even be torn down.
            let mut init_waits = 0u32;
            // Whether media segments are self-initializing (each carries a
            // ftyp+moov prefix to strip). Only true when we sourced the init
            // from a media segment's prefix (live). A dedicated init segment
            // (VOD) means media segments carry no prefix, so skip re-parsing
            // each one.
            let mut self_init = false;
            let init_bytes: Bytes = loop {
                if !running.load(Ordering::Acquire) || session.is_released() {
                    return;
                }
                if should_restart(&session, role, &format, &seek_gen, generation, last_server_gen) {
                    continue 'restart;
                }
                // Must wait for the init to be *complete*. It is announced (with
                // no bytes yet) before its MEDIA parts arrive, and the async UMP
                // reader yields between parts, so an un-gated read here can push
                // a truncated `moov` and wedge the demuxer with bogus atom sizes.
                if let Some(init) = buffer.init_segment()
                    && init.is_complete()
                {
                    break init.bytes();
                }
                if let Some(seg) = buffer.first_at_or_after(-1)
                    && seg.is_complete()
                {
                    let bytes = seg.bytes();
                    let prefix = mp4_init_prefix_length(&bytes);
                    if prefix > 0 {
                        self_init = true;
                        break bytes.slice(..prefix);
                    }
                }
                if let Some(err) = session.fatal_error() {
                    fail_stream(&elem, &appsrc, &err);
                    return;
                }
                init_waits += 1;
                if init_waits >= INIT_WAIT_LIMIT {
                    fail_stream(
                        &elem,
                        &appsrc,
                        &format!("feeder {role:?} timed out waiting for an init segment"),
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            match await_demand(&session, role, &format, &running, &seek_gen, generation, last_server_gen, &enough).await {
                Demand::Go => {}
                Demand::Restart => continue 'restart,
                Demand::Stop => return,
            }
            match push(&appsrc, init_bytes) {
                PushOutcome::Ok => {}
                PushOutcome::Retry => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue 'restart;
                }
                PushOutcome::Stop => {
                    gst::warning!(CAT, "feeder {role:?} exiting (init push error)");
                    return;
                }
            }

            let mut next_seq: Option<i32> = None;
            'media: loop {
                if !running.load(Ordering::Acquire) || session.is_released() {
                    return;
                }
                if should_restart(&session, role, &format, &seek_gen, generation, last_server_gen) {
                    continue 'restart;
                }

                let segment = match next_seq {
                    Some(n) => buffer.await_sequence(n, AWAIT_TIMEOUT).await,
                    None => buffer.await_announced(-1, AWAIT_TIMEOUT).await,
                };
                let Some(segment) = segment else {
                    if let Some(err) = session.fatal_error() {
                        fail_stream(&elem, &appsrc, &err);
                        return;
                    }
                    continue;
                };
                let seq = segment.sequence_number;

                // Wait for the segment to finish downloading.
                while !segment.is_complete() {
                    if !running.load(Ordering::Acquire) || session.is_released() {
                        return;
                    }
                    if should_restart(&session, role, &format, &seek_gen, generation, last_server_gen) {
                        continue 'restart;
                    }
                    // The pump can discard this exact segment (a truncated
                    // response with incomplete pending dropped at end of
                    // `consume`, or a content-length mismatch at MEDIA_END) and
                    // re-announce the sequence as a fresh `Arc`. That bumps
                    // neither the seek nor the epoch generation, so
                    // `should_restart` won't catch it. Watch for the swap
                    // directly and re-await the sequence rather than block
                    // forever on an `Arc` that will never complete.
                    match buffer.get(seq) {
                        Some(cur) if Arc::ptr_eq(&cur, &segment) => {}
                        _ => {
                            next_seq = Some(seq);
                            continue 'media;
                        }
                    }
                    buffer.await_bytes(&segment, segment.size(), AWAIT_TIMEOUT).await;
                    if !segment.is_complete()
                        && let Some(err) = session.fatal_error()
                    {
                        fail_stream(&elem, &appsrc, &err);
                        return;
                    }
                }

                match await_demand(&session, role, &format, &running, &seek_gen, generation, last_server_gen, &enough).await {
                    Demand::Go => {}
                    Demand::Restart => continue 'restart,
                    Demand::Stop => return,
                }
                // Strip any self-init prefix (ftyp+moov) so we don't re-push the
                // moov mid-stream. Only self-initializing (live) segments carry
                // one. VOD segments never do, so skip the parse there. Slicing
                // the frozen `Bytes` is zero-copy and `Buffer::from_slice` wraps
                // it without copying the payload.
                let seg_bytes = segment.bytes();
                let payload = if self_init {
                    let strip = mp4_init_prefix_length(&seg_bytes).min(seg_bytes.len());
                    seg_bytes.slice(strip..)
                } else {
                    seg_bytes
                };
                match push(&appsrc, payload) {
                    PushOutcome::Ok => {}
                    PushOutcome::Retry => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue 'restart;
                    }
                    PushOutcome::Stop => {
                        gst::warning!(CAT, "feeder {role:?} exiting (segment push error)");
                        return;
                    }
                }

                next_seq = Some(seq + 1);

                // Advance the demand window and playhead so the pump fetches
                // ahead and old segments can be evicted. Cheap window bump: the
                // active format stays with the session (`adopt_server_format`),
                // so no need to re-send the alternates list every segment.
                session.set_playback_position(segment.end_us());
                session.advance_demand(role, segment.end_us());

                // VOD end-of-stream detection.
                if let Some(fim) = session.format_initialization_for(&format)
                    && fim.end_segment_number > 0
                    && seq >= fim.end_segment_number
                {
                    gst::debug!(CAT, "feeder {role:?} reached end (seq={seq}); EOS, awaiting seek");
                    let _ = appsrc.end_of_stream();
                    // Do NOT exit the feeder task. A seek after EOS (very common
                    // for short videos that fully buffer) must be able to
                    // re-feed. Wait for a generation bump, then restart from the
                    // new pos.
                    loop {
                        if !running.load(Ordering::Acquire) || session.is_released() {
                            return;
                        }
                        if should_restart(&session, role, &format, &seek_gen, generation, last_server_gen) {
                            continue 'restart;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }

    /// Length of the fragmented-MP4 init prefix (`ftyp`/`moov`/`free`/…) at the
    /// start of `data`, i.e. the offset of the first media box (`styp`, `sidx`,
    /// `moof`, `mdat`, `emsg`). Returns 0 when `data` already starts with media
    /// (normal VOD segments) or can't be parsed. Live SABR fragments are
    /// self-initializing (each carries this prefix), so it doubles as both the
    /// init source and the amount to strip from each media segment.
    fn mp4_init_prefix_length(data: &[u8]) -> usize {
        let mut pos = 0usize;
        while pos + 8 <= data.len() {
            let size32 = u32::from_be_bytes([
                data[pos],
                data[pos + 1],
                data[pos + 2],
                data[pos + 3],
            ]) as u64;
            let box_size = match size32 {
                1 => {
                    if pos + 16 > data.len() {
                        break;
                    }
                    u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap())
                }
                0 => (data.len() - pos) as u64,
                n => n,
            };
            if box_size < 8 {
                break;
            }
            match &data[pos + 4..pos + 8] {
                b"styp" | b"sidx" | b"moof" | b"mdat" | b"emsg" => return pos,
                _ => {}
            }
            pos += box_size as usize;
        }
        0
    }

    /// Flush the appsrc (and everything downstream) after a *server*-initiated
    /// seek. Client seeks already flush via the forwarded seek event. A server
    /// seek doesn't, so the appsrc still holds queued buffers on the now-stale
    /// timeline. Flush them before re-priming with a fresh init segment, so we
    /// don't splice a second `moov` behind old media. Only ever runs on an
    /// actual server reposition, so it can't affect steady-state playback.
    fn flush_appsrc(appsrc: &gst_app::AppSrc) {
        // reset_time=false: the two branch feeders flush independently, so
        // resetting the pipeline running-time from either would desync A/V.
        let _ = appsrc.send_event(gst::event::FlushStart::new());
        let _ = appsrc.send_event(gst::event::FlushStop::new(false));
    }

    /// Surface a fatal streaming failure as a bus `ERROR` (so the application
    /// sees a real error) rather than a silent `EOS` that looks like a clean
    /// end. Still EOS the appsrc afterward to unblock anything waiting on it.
    fn fail_stream(elem: &glib::WeakRef<super::SabrumpSrc>, appsrc: &gst_app::AppSrc, msg: &str) {
        gst::error!(CAT, "sabrump stream failed: {msg}");
        if let Some(elem) = elem.upgrade() {
            gst::element_error!(elem, gst::StreamError::Failed, ["{msg}"]);
        }
        let _ = appsrc.end_of_stream();
    }

    fn push(appsrc: &gst_app::AppSrc, data: Bytes) -> PushOutcome {
        // `Buffer::from_slice` wraps the `Bytes` without copying its payload.
        match appsrc.push_buffer(gst::Buffer::from_slice(data)) {
            Ok(_) => PushOutcome::Ok,
            Err(gst::FlowError::Flushing | gst::FlowError::Eos) => PushOutcome::Retry,
            Err(e) => {
                gst::warning!(CAT, "appsrc push failed: {e:?}");
                PushOutcome::Stop
            }
        }
    }

    // --- URI spec parsing ---

    fn parse_spec(uri: &str) -> Result<SabrStreamSpec, String> {
        let parsed = url::Url::parse(uri).map_err(|e| e.to_string())?;
        let spec_b64 = parsed
            .query_pairs()
            .find(|(k, _)| k == "spec")
            .map(|(_, v)| v.into_owned())
            .ok_or("missing `spec` query parameter")?;
        let json = decode_base64(&spec_b64).ok_or("`spec` is not valid base64")?;
        serde_json::from_slice(&json).map_err(|e| format!("bad spec JSON: {e}"))
    }

    fn decode_base64(value: &str) -> Option<Vec<u8>> {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(value))
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(value))
            .ok()
    }

    // --- reqwest client for the SABR transport ---

    /// Build the reqwest client `sabrump` sends SABR requests through. The
    /// timeouts bound every phase. Without them a stalled endpoint blocks the
    /// pump indefinitely (it only re-checks shutdown between UMP parts) with no
    /// error, backoff, or recovery. `read_timeout` resets on each successful
    /// read, so it detects a stall without capping a healthy streaming response.
    fn build_reqwest_client() -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| format!("failed to build reqwest client: {e}"))
    }
}

glib::wrapper! {
    pub struct SabrumpSrc(ObjectSubclass<imp::SabrumpSrc>)
        @extends gst::Bin, gst::Element, gst::Object,
        @implements gst::URIHandler;
}

pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "sabrumpsrc",
        gst::Rank::PRIMARY + 1,
        SabrumpSrc::static_type(),
    )
}
