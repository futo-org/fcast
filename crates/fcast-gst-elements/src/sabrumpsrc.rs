// URI handler and source bin for the SABR/UMP protocol (`sabrump://` scheme).
//
// URI format: `sabrump://<videoId>?spec=<base64url(JSON SabrStreamSpec)>`.

use gst::glib::{self, types::StaticType};

mod imp {
    use std::{
        sync::{
            Arc, LazyLock,
            atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use bytes::Bytes;
    use gst::{glib, prelude::*, subclass::prelude::*};
    use parking_lot::Mutex;
    use sabrump::{SabrFormat, SabrSession, SabrStreamSpec, SabrTransport, spec::Role};
    use tokio::task::JoinHandle;

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
        /// Set by `enough-data`, cleared by `need-data`. The feeder paces on
        /// this in its own loop rather than blocking inside
        /// `push_buffer`, so it stays responsive to seeks while paused.
        enough: Arc<AtomicBool>,
    }

    #[derive(Default)]
    struct State {
        uri: Option<String>,
        session: Option<SabrSession>,
        branches: Vec<Branch>,
        running: Option<Arc<AtomicBool>>,
        /// The session pump plus one feeder per branch, on the shared runtime.
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
                    // Unconditional. Even if the parent transition errors, the
                    // session pump must be released or it polls forever.
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
                glib::Error::new(
                    gst::URIError::BadUri,
                    &format!("invalid sabrump URI: {msg}"),
                )
            })?;

            let client = build_reqwest_client()
                .map_err(|msg| glib::Error::new(gst::ResourceError::OpenRead, &msg))?;
            let session = SabrSession::new(spec.clone(), SabrTransport::http(client));

            let duration_us = spec.duration_us;
            let is_live = spec.is_live;

            let element = self.obj();
            let bin: &gst::Bin = element.upcast_ref();

            // The first buffer's PTS, shared across branches so rebasing to a
            // zero-based timeline keeps A/V on one origin.
            let live_offset = Arc::new(AtomicI64::new(i64::MIN));

            // Each branch's parsebin exposes exactly one elementary stream, so the
            // ghost pad count is known up front. Track it to emit `no-more-pads`
            // once they are all present.
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
            // Release any previous session, or its pump keeps polling forever.
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
        /// pads out of the bin. `parsebin` typefinds the container from the
        /// bytes, so a branch works whichever container the server's
        /// ABR delivers.
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
                // TIME format so the demuxer is driven with a time segment and
                // seeks are expressed in time.
                .format(gst::Format::Time)
                // With is_live=false a live stream that has not produced a
                // decodable buffer wedges the pipeline in async PAUSED, so it can
                // never be torn down.
                .is_live(is_live)
                .do_timestamp(false)
                // No caps. parsebin typefinds the container from the pushed bytes,
                // so never assert one the ABR-selected format might not match.
                .build();
            // Do NOT block inside push_buffer for pacing. While paused, a blocked
            // push cannot observe seeks. Pace in the feeder loop instead.
            appsrc.set_property("block", false);
            appsrc.set_property("max-bytes", 8_000_000u64);
            if seekable {
                appsrc.set_duration(gst::ClockTime::from_useconds(duration_us as u64));
            }

            // Starts true so the feeder waits for the first need-data rather than
            // racing ahead before the pipeline is prerolling.
            let enough = Arc::new(AtomicBool::new(true));

            // Reposition the session on seek. Pace feeding via the demand signals.
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
                    // Explicit unique name. Every parsebin names its first src pad
                    // `src_0`, so a template-derived name would collide across
                    // branches. The shared counter also drives no-more-pads.
                    let idx = exposed_pads.fetch_add(1, Ordering::AcqRel);
                    let Some(templ) = bin.pad_template("src_%u") else {
                        gst::warning!(CAT, "missing src_%u pad template");
                        return;
                    };
                    let ghost = match gst::GhostPad::builder_from_template_with_target(&templ, pad)
                    {
                        Ok(builder) => builder.name(format!("src_{idx}")).build(),
                        Err(e) => {
                            gst::warning!(CAT, "failed to ghost pad: {e}");
                            return;
                        }
                    };
                    let _ = ghost.set_active(true);

                    add_diag_probe(&ghost, role);

                    // Live streams carry absolute media timestamps; rebase them so
                    // the sinks can schedule them.
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

                    // Once every branch exposed its stream, no more pads are coming.
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

            let mut tasks = Vec::new();
            tasks.push(fcast_runtime::RUNTIME.spawn({
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
                tasks.push(fcast_runtime::RUNTIME.spawn(async move {
                    feed(
                        session, appsrc, role, alternates, running, seek_gen, enough, elem,
                    )
                    .await;
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
            // Release FIRST so tasks unwind at their next await. Abort is only the
            // backstop for one stuck on I/O.
            if let Some(session) = session {
                session.release();
            }
            for handle in tasks {
                handle.abort();
            }
            self.state.lock().running = None;
        }

        /// Reposition the session to `target_us` from an appsrc `seek-data`
        /// callback. appsrc has already flushed itself, so this only moves the
        /// session and bumps the generation. `restart`'s return value coalesces
        /// the two per-branch callbacks.
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

        /// Forward a seek to every branch's appsrc. The pipeline seek reaches
        /// only one ghost pad, leaving the other branch's feeder
        /// unaware. Deduped by seqnum, since both ghost pads may fire
        /// the same seek.
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

    /// Forward SEEK events into every branch's appsrc (so the appsrcs, not the
    /// demuxer, handle them) and answer SEEKING queries as seekable.
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
                // Handled, so the demuxer never attempts a byte seek.
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

    /// Log the first segment event and first few buffers out of the demuxer, to
    /// see the timestamps a stream actually carries.
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
    /// SABR fragments carry wall-clock-ish PTS/DTS that the sinks would
    /// schedule days out. One shared offset (the first buffer's PTS on
    /// either branch) keeps video and audio on the same origin.
    fn add_live_rebase_probe(ghost: &gst::GhostPad, offset: Arc<AtomicI64>) {
        ghost.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_pad, info| {
                match &mut info.data {
                    Some(gst::PadProbeData::Event(event)) => {
                        if let gst::EventView::Segment(_) = event.view() {
                            // Plain zero-based open-ended TIME segment, so rebased
                            // buffers schedule from 0.
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
        /// Transient (appsrc flushing, or a stale EOS a pending seek's flush
        /// has not cleared): retry via `'restart`. Do NOT exit, or a
        /// post-EOS seek would permanently kill the feeder.
        Retry,
        /// A real downstream error. Stop feeding.
        Stop,
    }

    /// Result of waiting for appsrc demand before a push.
    enum Demand {
        /// appsrc wants data. Go ahead and push.
        Go,
        /// The seek generation changed. Restart the feed loop.
        Restart,
        /// Shutting down. Exit the feeder.
        Stop,
    }

    /// Whether the feed loop must restart from the top. A client seek, a server
    /// reposition, or an ABR format switch each leave the feeder's sequence
    /// cursor and/or buffer stale, and it would otherwise wait forever for data
    /// that never arrives on the buffer it watches.
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

    /// Wait until the appsrc asks for more data, checking the seek generation
    /// each tick. Replaces blocking inside `push_buffer`, which would trap
    /// the feeder where it cannot see a seek while the pipeline is paused.
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

    /// Per-track feeder. Push the init segment, then media segments in sequence
    /// order. Restarts from the top, re-pushing init, whenever `seek_gen`
    /// moves.
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
        // Re-resolved on each (re)start. Server-side ABR can switch the session to
        // another alternate, after which the pump fills THAT format's buffer and a
        // feeder pinned to the original would wait forever.
        let mut format = match session
            .active_format(role)
            .or_else(|| alternates.first().cloned())
        {
            Some(f) => f,
            None => return,
        };
        let mut buffer = session.buffer_for(&format);

        // A server reposition makes the sequence cursor stale, and unlike a client
        // seek nothing else flushes the queued buffers.
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
            // Follow a server-side format switch. Re-point at the new format's
            // buffer before re-priming with its init segment.
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

            // A fresh fragmented stream needs an init segment (ftyp+moov) after
            // every (re)start. VOD announces a dedicated one, live fragments are
            // self-initializing so it comes from the first media segment's prefix.
            // Give up rather than spin, a hung feeder never prerolls and wedges the
            // pipeline beyond teardown.
            let mut init_waits = 0u32;
            // Media segments carry a ftyp+moov prefix to strip only when the init
            // came from one (live); VOD segments never do.
            let mut self_init = false;
            let init_bytes: Bytes = loop {
                if !running.load(Ordering::Acquire) || session.is_released() {
                    return;
                }
                if should_restart(
                    &session,
                    role,
                    &format,
                    &seek_gen,
                    generation,
                    last_server_gen,
                ) {
                    continue 'restart;
                }
                // Must wait for the init to be COMPLETE. It is announced before its
                // MEDIA parts arrive, and pushing a truncated `moov` wedges the
                // demuxer with bogus atom sizes.
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
            match await_demand(
                &session,
                role,
                &format,
                &running,
                &seek_gen,
                generation,
                last_server_gen,
                &enough,
            )
            .await
            {
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
                if should_restart(
                    &session,
                    role,
                    &format,
                    &seek_gen,
                    generation,
                    last_server_gen,
                ) {
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
                    if should_restart(
                        &session,
                        role,
                        &format,
                        &seek_gen,
                        generation,
                        last_server_gen,
                    ) {
                        continue 'restart;
                    }
                    // The pump can discard this exact segment and re-announce the
                    // sequence as a fresh `Arc`, which bumps no generation and so
                    // escapes `should_restart`. Watch for the swap directly, or
                    // block forever on an `Arc` that never completes.
                    match buffer.get(seq) {
                        Some(cur) if Arc::ptr_eq(&cur, &segment) => {}
                        _ => {
                            next_seq = Some(seq);
                            continue 'media;
                        }
                    }
                    buffer
                        .await_bytes(&segment, segment.size(), AWAIT_TIMEOUT)
                        .await;
                    if !segment.is_complete()
                        && let Some(err) = session.fatal_error()
                    {
                        fail_stream(&elem, &appsrc, &err);
                        return;
                    }
                }

                match await_demand(
                    &session,
                    role,
                    &format,
                    &running,
                    &seek_gen,
                    generation,
                    last_server_gen,
                    &enough,
                )
                .await
                {
                    Demand::Go => {}
                    Demand::Restart => continue 'restart,
                    Demand::Stop => return,
                }
                // Strip any self-init prefix so the moov is not re-pushed
                // mid-stream. Slicing the frozen `Bytes` is zero-copy.
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

                // Advance the demand window and playhead so the pump fetches ahead
                // and old segments can be evicted.
                session.set_playback_position(segment.end_us());
                session.advance_demand(role, segment.end_us());

                // VOD end-of-stream detection.
                if let Some(fim) = session.format_initialization_for(&format)
                    && fim.end_segment_number > 0
                    && seq >= fim.end_segment_number
                {
                    gst::debug!(
                        CAT,
                        "feeder {role:?} reached end (seq={seq}); EOS, awaiting seek"
                    );
                    let _ = appsrc.end_of_stream();
                    // Do NOT exit the feeder task. A seek after EOS must still be
                    // able to re-feed. Wait for a generation bump, then restart.
                    loop {
                        if !running.load(Ordering::Acquire) || session.is_released() {
                            return;
                        }
                        if should_restart(
                            &session,
                            role,
                            &format,
                            &seek_gen,
                            generation,
                            last_server_gen,
                        ) {
                            continue 'restart;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }

    /// Length of the fragmented-MP4 init prefix (`ftyp`/`moov`/…) at the start
    /// of `data`, i.e. the offset of the first media box. Returns 0 when
    /// `data` already starts with media or cannot be parsed.
    fn mp4_init_prefix_length(data: &[u8]) -> usize {
        let mut pos = 0usize;
        while pos + 8 <= data.len() {
            let size32 =
                u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as u64;
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

    /// Flush the appsrc after a SERVER-initiated seek, which (unlike a client
    /// seek) leaves queued buffers on a stale timeline. They must go before
    /// re-priming, or a second `moov` is spliced in behind old media.
    fn flush_appsrc(appsrc: &gst_app::AppSrc) {
        // reset_time=false because the branch feeders flush independently, so resetting
        // the running-time from either would desync A/V.
        let _ = appsrc.send_event(gst::event::FlushStart::new());
        let _ = appsrc.send_event(gst::event::FlushStop::new(false));
    }

    /// Surface a fatal failure as a bus `ERROR` rather than a silent `EOS` that
    /// looks like a clean end, then EOS the appsrc to unblock its waiters.
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

    /// Build the reqwest client SABR requests go through. The timeouts bound
    /// every phase. Without them a stalled endpoint blocks the pump forever
    /// with no error or recovery. `read_timeout` resets on each successful
    /// read, so it catches a stall without capping a healthy streaming
    /// response.
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
