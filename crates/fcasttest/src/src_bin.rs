//! `ftestsrc`: source element and `ftest://` URI handler. Owner: agent A.
//!
//! One sometimes src pad per [`crate::spec::StreamSpec`], each driven by its own
//! streaming task, so a scenario can stall, error or flush a single stream.
//! Stream collections are left to parsebin and urisourcebin: the field path.

mod imp {
    use std::{
        sync::{Arc, LazyLock},
        time::{Duration, Instant},
    };

    use gst::{glib, prelude::*, subclass::prelude::*};
    use parking_lot::{Condvar, Mutex};

    use crate::{
        caps,
        prng::Prng,
        registry::{self, Scenario, SyncPoint},
        spec::{
            AUDIO_PACKET, BufferingRecovery, BufferingSpec, Fault, FlowStopReason, Pacing,
            StreamKind, StreamSpec, buffer_count, frame_pts,
        },
    };

    /// Re-check step while a push is parked on a stall gate. Only the test ends a
    /// stall, the step exists so flush and shutdown stay bounded.
    const STALL_POLL: Duration = Duration::from_millis(5);
    /// Re-push step while a src pad has no peer, see [`NOT_LINKED_BOUND`].
    const NOT_LINKED_RETRY: Duration = Duration::from_millis(2);
    /// Re-check interval for a stream whose pad is unlinked while a sibling
    /// still delivers (a deselected stream idles instead of erroring).
    const NOT_LINKED_IDLE: Duration = Duration::from_millis(100);
    /// How long a NotLinked push is retried before it becomes a stream error.
    /// parsebin unlinks `typefind:src` from its parse pad while it plugs the
    /// parser, and fcasttest caps are sticky so typefind classifies from the CAPS
    /// event alone and forwards the first buffer straight into that window. Real
    /// media never hits it (typefind holds data until it has typefound). The bound
    /// is orders of magnitude above the observed window, so a pad that is never
    /// linked still fails, just later.
    const NOT_LINKED_BOUND: Duration = Duration::from_secs(2);

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "ftestsrc",
            gst::DebugColorFlags::empty(),
            Some("FCast test source"),
        )
    });

    static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
        vec![
            gst::PadTemplate::new(
                "video_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps::video_caps(),
            )
            .unwrap(),
            gst::PadTemplate::new(
                "audio_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps::audio_caps(),
            )
            .unwrap(),
            gst::PadTemplate::new(
                "text_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps::text_caps(),
            )
            .unwrap(),
        ]
    });

    /// Template, pad-name prefix and per-kind counter slot for a stream.
    fn pad_layout(kind: &StreamKind) -> (&'static gst::PadTemplate, &'static str, usize) {
        match kind {
            StreamKind::Video { .. } => (&PAD_TEMPLATES[0], "video", 0),
            StreamKind::Audio { .. } => (&PAD_TEMPLATES[1], "audio", 1),
            StreamKind::Text { .. } => (&PAD_TEMPLATES[2], "text", 2),
        }
    }

    enum Payload {
        /// Arbitrary bytes, filled at push time from the buffer index.
        Pattern(usize),
        Utf8(Vec<u8>),
    }

    struct Buf {
        /// Position within the stream's buffers, what [`Fault`] indexes.
        index: u64,
        pts: gst::ClockTime,
        duration: gst::ClockTime,
        keyframe: bool,
        payload: Payload,
    }

    enum Item {
        Buffer(Buf),
        /// Sparse-stream filler between text cues.
        Gap {
            pts: gst::ClockTime,
            duration: gst::ClockTime,
        },
    }

    fn build_schedule(spec: &StreamSpec) -> Vec<Item> {
        match &spec.kind {
            StreamKind::Video {
                fps,
                keyframe_interval,
                ..
            } => {
                let count = buffer_count(spec.duration, frame_pts(1, *fps));
                (0..count)
                    .map(|index| {
                        Item::Buffer(Buf {
                            index,
                            pts: frame_pts(index, *fps),
                            duration: frame_pts(index + 1, *fps) - frame_pts(index, *fps),
                            // ftestdec's needs-keyframe-after-flush knob depends
                            // on this exact rule.
                            keyframe: *keyframe_interval == 0
                                || index % u64::from(*keyframe_interval) == 0,
                            payload: Payload::Pattern(spec.bytes_per_buffer),
                        })
                    })
                    .collect()
            }
            StreamKind::Audio { .. } => {
                let count = buffer_count(spec.duration, AUDIO_PACKET);
                (0..count)
                    .map(|index| {
                        Item::Buffer(Buf {
                            index,
                            pts: AUDIO_PACKET * index,
                            duration: AUDIO_PACKET,
                            keyframe: true,
                            payload: Payload::Pattern(spec.bytes_per_buffer),
                        })
                    })
                    .collect()
            }
            // Sparse stream: one buffer per cue carrying the cue text (so
            // bytes_per_buffer does not apply), GAP events over the rest.
            StreamKind::Text { cues } => {
                let mut items = Vec::new();
                let mut cursor = gst::ClockTime::ZERO;
                for (index, cue) in cues.iter().enumerate() {
                    if cue.start > cursor {
                        items.push(Item::Gap {
                            pts: cursor,
                            duration: cue.start - cursor,
                        });
                    }
                    let end = cue.end.max(cue.start);
                    items.push(Item::Buffer(Buf {
                        index: index as u64,
                        pts: cue.start,
                        duration: end - cue.start,
                        keyframe: true,
                        payload: Payload::Utf8(cue.text.clone().into_bytes()),
                    }));
                    cursor = end;
                }
                if cursor < spec.duration {
                    items.push(Item::Gap {
                        pts: cursor,
                        duration: spec.duration - cursor,
                    });
                }
                items
            }
        }
    }

    /// First schedule step of a restart at `offset`: the first item reaching
    /// past it, so an item spanning the offset is re-delivered and the sink
    /// clips it. Past-the-end returns the schedule length, which the task turns
    /// into an immediate EOS. Video walks back to the nearest keyframe like a
    /// demuxer, so a decoder that needs one after a flush starts clean (the
    /// lead-in frames precede the segment start and render nothing).
    fn start_index(schedule: &[Item], offset: gst::ClockTime, video: bool) -> usize {
        let Some(first) = schedule.iter().position(|item| match item {
            Item::Buffer(buf) => buf.pts + buf.duration > offset,
            Item::Gap { pts, duration } => *pts + *duration > offset,
        }) else {
            return schedule.len();
        };
        if !video {
            return first;
        }
        schedule[..=first]
            .iter()
            .rposition(|item| matches!(item, Item::Buffer(buf) if buf.keyframe))
            .unwrap_or(first)
    }

    fn make_buffer(buf: &Buf) -> gst::Buffer {
        let mut buffer = match &buf.payload {
            Payload::Pattern(len) => {
                let mut buffer = gst::Buffer::with_size(*len).expect("allocating a test buffer");
                {
                    let buffer = buffer.get_mut().unwrap();
                    let mut map = buffer.map_writable().expect("mapping a test buffer");
                    for (offset, byte) in map.iter_mut().enumerate() {
                        *byte = (buf.index as usize).wrapping_add(offset) as u8;
                    }
                }
                buffer
            }
            Payload::Utf8(text) => gst::Buffer::from_slice(text.clone()),
        };
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(buf.pts);
            buffer.set_duration(buf.duration);
            buffer.set_offset(buf.index);
            buffer.set_offset_end(buf.index + 1);
            if !buf.keyframe {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        buffer
    }

    struct RunState {
        /// Next schedule position.
        next: usize,
        need_stream_start: bool,
        need_segment: bool,
        flushing: bool,
        stopping: bool,
        /// The schedule ended: EOS was sent, an error was posted, or the task
        /// hit an injected end. No further pushes until a reset.
        done: bool,
        position: gst::ClockTime,
        /// Where the current segment begins: zero until a non-zero flushing
        /// seek restarts the schedule from an offset (see `handle_seek`).
        segment_start: gst::ClockTime,
        prng: Prng,
    }

    /// Hand-off from the streaming threads to the buffering worker. An
    /// anchored dip fires on the streaming thread, which must never block on
    /// the bus, so it queues the dip's recovery here and the worker does every
    /// post. See [`run_buffering_schedule`].
    struct BufferingShared {
        queue: Mutex<BufferingQueue>,
        cv: Condvar,
    }

    #[derive(Default)]
    struct BufferingQueue {
        stopping: bool,
        /// Recoveries of anchored dips that fired, oldest first.
        requests: Vec<BufferingRecovery>,
    }

    /// The buffering worker and its queue, held by the element while the media
    /// schedules buffering. The queue is created together with the pads so
    /// every stream can reach it, the thread itself starts with the tasks.
    struct BufferingCtl {
        shared: Arc<BufferingShared>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    /// Posts the scheduled `GST_MESSAGE_BUFFERING` messages until told to stop,
    /// on its own thread across the element's PAUSED and PLAYING life. See
    /// [`BufferingSpec`] for the dip contract.
    fn run_buffering_schedule(
        element: glib::WeakRef<super::FTestSrc>,
        scenario: Arc<Scenario>,
        spec: BufferingSpec,
        shared: Arc<BufferingShared>,
    ) {
        let low = spec.low_percent.clamp(0, 99);
        // False once the element is gone, which ends the schedule.
        let post = |percent: i32| -> bool {
            let Some(element) = element.upgrade() else {
                return false;
            };
            gst::debug!(CAT, obj = &element, "posting buffering {percent}%");
            let _ = element.post_message(
                gst::message::Buffering::builder(percent)
                    .src(&element)
                    .build(),
            );
            true
        };
        // Waits out `duration`. False when a stop cut the wait short.
        let wait_out = |duration: Duration| -> bool {
            let deadline = Instant::now() + duration;
            let mut queue = shared.queue.lock();
            while !queue.stopping {
                if shared.cv.wait_until(&mut queue, deadline).timed_out() {
                    return true;
                }
            }
            false
        };
        // One full dip. The low percent goes out now and 100 goes out when
        // the recovery says so. False when a stop or the element's end cut
        // the dip short, in which case nothing further may be posted.
        let dip = |recovery: &BufferingRecovery| -> bool {
            if !post(low) {
                return false;
            }
            let recovered = match recovery {
                BufferingRecovery::AfterMs(ms) => wait_out(Duration::from_millis(*ms)),
                BufferingRecovery::OnSyncPoint(name) => {
                    let gate = scenario.sync_point(name);
                    loop {
                        if gate.is_released() {
                            break true;
                        }
                        if !wait_out(STALL_POLL) {
                            break false;
                        }
                    }
                }
            };
            if !recovered {
                // A lost 100 is invisible from a consumer's side, so name the
                // reason: the element went down between the two posts.
                gst::debug!(
                    CAT,
                    "the element went down mid-dip, no 100 will be posted for this low"
                );
            }
            recovered && post(100)
        };

        if let Some(ms) = spec.initial_ms
            && !dip(&BufferingRecovery::AfterMs(ms))
        {
            return;
        }
        let mut next_periodic = spec
            .periodic
            .map(|periodic| Instant::now() + Duration::from_millis(periodic.period_ms));
        loop {
            // An anchored dip that fired goes first, then a due periodic
            // tick. With neither the thread sleeps until woken.
            let request = {
                let mut queue = shared.queue.lock();
                loop {
                    if queue.stopping {
                        return;
                    }
                    if !queue.requests.is_empty() {
                        break Some(queue.requests.remove(0));
                    }
                    match next_periodic {
                        Some(due) if Instant::now() >= due => break None,
                        Some(due) => {
                            let _ = shared.cv.wait_until(&mut queue, due);
                        }
                        None => shared.cv.wait(&mut queue),
                    }
                }
            };
            match request {
                Some(recovery) => {
                    if !dip(&recovery) {
                        return;
                    }
                }
                None => {
                    // A periodic tick only falls due when one is configured.
                    let Some(periodic) = spec.periodic else {
                        return;
                    };
                    if !dip(&BufferingRecovery::AfterMs(periodic.low_ms)) {
                        return;
                    }
                    next_periodic =
                        Some(Instant::now() + Duration::from_millis(periodic.period_ms));
                }
            }
        }
    }

    /// Last push outcome per stream of one media (true = delivered), the
    /// harness's stand-in for a demuxer's flow combiner: a real multi-stream
    /// source aggregates flow and errors only when EVERY pad refuses, so one
    /// deselected (not-linked) stream must not kill the input while a sibling
    /// still plays. Single-stream inputs keep the immediate death, which is
    /// what a deselected external subtitle input does in the field.
    type FlowBoard = Arc<Mutex<Vec<Option<bool>>>>;

    /// One stream's schedule and streaming-task state.
    /// Shared by sibling streams of one media (like [`FlowBoard`]): the
    /// upstream-selection mirror. SELECT_STREAMS arrives once per routed src
    /// pad, so the seqnum dedupes it, and `selected` models an adaptive
    /// demuxer's current track set (a STREAMS_SELECTED posts only on change,
    /// adaptivedemux2's activation edge).
    #[derive(Default)]
    struct SelectState {
        last_seqnum: Option<gst::Seqnum>,
        selected: Option<Vec<String>>,
    }
    type SelectBoard = Arc<Mutex<SelectState>>;

    /// STREAMS_SELECTED as an adaptive demuxer posts it: the media's own
    /// streams as the collection, `selected_ids` marked selected.
    fn streams_selected_message(
        scenario: &Scenario,
        element: &gst::Element,
        selected_ids: &[String],
    ) -> gst::Message {
        let mut collection = gst::StreamCollection::builder(None);
        let mut selected = Vec::new();
        for stream in &scenario.spec().streams {
            let sid = scenario.stream_id(&stream.id);
            let kind = match stream.kind {
                StreamKind::Video { .. } => gst::StreamType::VIDEO,
                StreamKind::Audio { .. } => gst::StreamType::AUDIO,
                StreamKind::Text { .. } => gst::StreamType::TEXT,
            };
            let entry = gst::Stream::new(
                Some(&sid),
                Some(&stream.kind.caps()),
                kind,
                gst::StreamFlags::empty(),
            );
            if selected_ids.contains(&sid) {
                selected.push(entry.clone());
            }
            collection = collection.stream(entry);
        }
        gst::message::StreamsSelected::builder(&collection.build())
            .streams(selected)
            .src(element)
            .build()
    }

    struct Stream {
        scenario: Arc<Scenario>,
        spec: StreamSpec,
        /// Position within the MediaSpec, the PRNG derivation key.
        index: usize,
        stream_id: String,
        caps: gst::Caps,
        group_id: gst::GroupId,
        /// Immutable after construction.
        schedule: Vec<Item>,
        state: Mutex<RunState>,
        cv: Condvar,
        /// Shared with every sibling stream, see [`FlowBoard`].
        flows: FlowBoard,
        /// Shared with every sibling stream, see [`SelectState`].
        select: SelectBoard,
        /// Shared with the buffering worker while the media schedules
        /// buffering, see [`BufferingShared`].
        buffering: Option<Arc<BufferingShared>>,
        /// This stream's anchored dips, precomputed from the media spec.
        dips: Vec<(u64, BufferingRecovery)>,
    }

    impl Stream {
        fn new(
            scenario: Arc<Scenario>,
            index: usize,
            group_id: gst::GroupId,
            flows: FlowBoard,
            select: SelectBoard,
            buffering: Option<Arc<BufferingShared>>,
        ) -> Arc<Self> {
            let spec = scenario.spec().streams[index].clone();
            let prng = Prng::derive(scenario.spec().seed, index);
            let dips = scenario
                .spec()
                .buffering
                .as_ref()
                .map(|buffering| {
                    buffering
                        .dips
                        .iter()
                        .filter(|dip| dip.stream == spec.id)
                        .map(|dip| (dip.buffer_index, dip.recovery.clone()))
                        .collect()
                })
                .unwrap_or_default();
            Arc::new(Self {
                stream_id: scenario.stream_id(&spec.id),
                caps: spec.kind.caps(),
                schedule: build_schedule(&spec),
                buffering,
                dips,
                state: Mutex::new(RunState {
                    next: 0,
                    need_stream_start: true,
                    need_segment: true,
                    flushing: false,
                    stopping: false,
                    done: false,
                    position: gst::ClockTime::ZERO,
                    segment_start: gst::ClockTime::ZERO,
                    prng,
                }),
                cv: Condvar::new(),
                flows,
                select,
                scenario,
                spec,
                index,
                group_id,
            })
        }

        /// Rewinds to the start of the schedule. A flush leaves STREAM_START and
        /// CAPS on the pad and only drops the segment, so only a fresh
        /// activation (`full`) has to send them again.
        fn reset(&self, full: bool) {
            self.reset_to(full, gst::ClockTime::ZERO);
        }

        /// [`reset`](Self::reset) at an offset: the next segment starts at
        /// `start` and the schedule resumes at [`start_index`]. Faults index
        /// buffers by their schedule number, so a fault before the restart
        /// point simply never fires again, like a demuxer that never re-reads
        /// the bytes.
        fn reset_to(&self, full: bool, start: gst::ClockTime) {
            let video = matches!(self.spec.kind, StreamKind::Video { .. });
            let mut state = self.state.lock();
            state.next = start_index(&self.schedule, start, video);
            state.need_segment = true;
            state.need_stream_start |= full;
            state.done = false;
            state.position = start;
            state.segment_start = start;
            state.prng = Prng::derive(self.scenario.spec().seed, self.index);
            if full {
                state.flushing = false;
                state.stopping = false;
            }
        }

        fn set_flushing(&self, flushing: bool) {
            self.state.lock().flushing = flushing;
            self.cv.notify_all();
        }

        fn request_stop(&self) {
            self.state.lock().stopping = true;
            self.cv.notify_all();
        }

        fn finish(&self) {
            self.state.lock().done = true;
        }

        /// Waits out `duration`. Returns false when a flush or shutdown cut it
        /// short.
        fn hold(&self, duration: Duration) -> bool {
            let deadline = Instant::now() + duration;
            let mut state = self.state.lock();
            loop {
                if state.flushing || state.stopping {
                    return false;
                }
                if self.cv.wait_until(&mut state, deadline).timed_out() {
                    return true;
                }
            }
        }

        /// Parks the push until the test releases `gate`. A flush or shutdown
        /// unparks it so teardown stays bounded, a timeout never does.
        fn park(&self, gate: &SyncPoint) -> bool {
            // Registers the arrival exactly once: repeated blocking waits would
            // inflate SyncPoint::arrivals.
            if gate.wait_timeout(Duration::ZERO) {
                return true;
            }
            while !gate.is_released() {
                if !self.hold(STALL_POLL) {
                    return false;
                }
            }
            true
        }

        fn pace(&self, duration: gst::ClockTime) -> bool {
            match self.spec.pacing {
                Pacing::AsFastAsPossible => true,
                Pacing::Realtime => self.hold(Duration::from_nanos(duration.nseconds())),
                Pacing::Jitter { base_ms, jitter_ms } => {
                    let extra = {
                        let mut state = self.state.lock();
                        state.prng.next_range(0..jitter_ms.saturating_add(1))
                    };
                    self.hold(Duration::from_millis(base_ms.saturating_add(extra)))
                }
            }
        }

        fn spawn(self: &Arc<Self>, pad: &gst::Pad) -> Result<(), glib::BoolError> {
            let this = self.clone();
            let pad_weak = pad.downgrade();
            pad.start_task(move || {
                let Some(pad) = pad_weak.upgrade() else {
                    return;
                };
                this.iterate(&pad);
            })
        }

        /// One schedule step per task invocation: the streaming lock must be
        /// free between steps so flushes and seeks can synchronise with us.
        fn iterate(&self, pad: &gst::Pad) {
            enum Step {
                Sticky,
                Item(usize),
            }

            let step = {
                let state = self.state.lock();
                if state.stopping || state.flushing || state.done {
                    None
                } else if state.need_stream_start || state.need_segment {
                    Some(Step::Sticky)
                } else {
                    Some(Step::Item(state.next))
                }
            };

            match step {
                None => {
                    // A paused task looks like a blocked one in a stack, and the
                    // three reasons differ (a flush resumes, `done` never does).
                    let state = self.state.lock();
                    gst::debug!(
                        CAT,
                        obj = pad,
                        "pausing the task: stopping={} flushing={} done={}",
                        state.stopping,
                        state.flushing,
                        state.done
                    );
                    drop(state);
                    let _ = pad.pause_task();
                }
                Some(Step::Sticky) => self.push_sticky(pad),
                Some(Step::Item(index)) => self.push_item(pad, index),
            }
        }

        /// A pad that turns flushing mid-way drops sticky events instead of
        /// storing them, so each pending flag only clears once its event is
        /// actually on the pad. A buffer without them is an event-order violation.
        fn push_sticky(&self, pad: &gst::Pad) {
            let (stream_start, segment) = {
                let state = self.state.lock();
                (state.need_stream_start, state.need_segment)
            };

            if stream_start {
                pad.push_event(
                    gst::event::StreamStart::builder(&self.stream_id)
                        .group_id(self.group_id)
                        .build(),
                );
                pad.push_event(gst::event::Caps::new(&self.caps));
                if pad.sticky_event::<gst::event::StreamStart>(0).is_none()
                    || pad.sticky_event::<gst::event::Caps>(0).is_none()
                {
                    return;
                }
                self.state.lock().need_stream_start = false;
            }
            if segment {
                let start = self.state.lock().segment_start;
                let mut time_segment = gst::FormattedSegment::<gst::ClockTime>::new();
                time_segment.set_start(start);
                time_segment.set_time(start);
                time_segment.set_position(start);
                pad.push_event(gst::event::Segment::new(&time_segment));
                if pad.sticky_event::<gst::event::Segment>(0).is_none() {
                    return;
                }
                self.state.lock().need_segment = false;
            }
        }

        fn push_item(&self, pad: &gst::Pad, index: usize) {
            let Some(item) = self.schedule.get(index) else {
                gst::debug!(CAT, obj = pad, "schedule exhausted, sending EOS");
                pad.push_event(gst::event::Eos::new());
                self.finish();
                let _ = pad.pause_task();
                return;
            };

            let duration = match item {
                Item::Buffer(buf) => buf.duration,
                Item::Gap { duration, .. } => *duration,
            };

            if let Item::Buffer(buf) = item {
                let mut error = false;
                let mut flow_stopped: Option<FlowStopReason> = None;
                let mut eos = false;
                for fault in &self.spec.faults {
                    match fault {
                        Fault::StallAt {
                            buffer_index,
                            sync_point,
                        } if *buffer_index == buf.index => {
                            gst::debug!(
                                CAT,
                                obj = pad,
                                "parking buffer {} on sync point {}",
                                buf.index,
                                sync_point
                            );
                            if !self.park(&self.scenario.sync_point(sync_point)) {
                                return;
                            }
                        }
                        Fault::ErrorAt { buffer_index } if *buffer_index == buf.index => {
                            error = true;
                        }
                        Fault::FlowStoppedAt {
                            buffer_index,
                            reason,
                        } if *buffer_index == buf.index => flow_stopped = Some(*reason),
                        Fault::EosAt { buffer_index } if *buffer_index == buf.index => eos = true,
                        _ => {}
                    }
                }
                if error {
                    if let Some(element) = pad.parent_element() {
                        gst::element_error!(
                            element,
                            gst::StreamError::Failed,
                            [
                                "injected error at buffer {} of {}",
                                buf.index,
                                self.stream_id
                            ]
                        );
                    }
                    self.finish();
                    let _ = pad.pause_task();
                    return;
                }
                if let Some(reason) = flow_stopped {
                    // Deliberately the SAME message and debug shape as the
                    // unlinked give-up below: a consumer classifying on the
                    // debug text must not tell an injected race from a real one.
                    gst::debug!(
                        CAT,
                        obj = pad,
                        "injected flow-stop error at buffer {}: {}",
                        buf.index,
                        reason.debug_text()
                    );
                    if let Some(element) = pad.parent_element() {
                        gst::element_error!(
                            element,
                            gst::StreamError::Failed,
                            ("Internal data stream error."),
                            ["{}", reason.debug_text()]
                        );
                    }
                    self.finish();
                    let _ = pad.pause_task();
                    return;
                }
                if eos {
                    gst::debug!(CAT, obj = pad, "injected EOS at buffer {}", buf.index);
                    pad.push_event(gst::event::Eos::new());
                    self.finish();
                    let _ = pad.pause_task();
                    return;
                }
            }

            if !self.pace(duration) {
                return;
            }

            match item {
                Item::Gap { pts, duration } => {
                    pad.push_event(gst::event::Gap::new(*pts, *duration));
                    self.advance(index, *pts + *duration);
                }
                Item::Buffer(buf) => self.push_buffer(pad, index, buf),
            }
        }

        /// Pushes one buffer, retrying the same buffer while the pad has no peer.
        /// Only a flush or a shutdown ends the retry loop early, and only
        /// [`NOT_LINKED_BOUND`] turns NotLinked into the stream error every
        /// demuxer posts for a fatal flow return.
        fn push_buffer(&self, pad: &gst::Pad, index: usize, buf: &Buf) {
            let give_up_at = Instant::now() + NOT_LINKED_BOUND;
            loop {
                match pad.push(make_buffer(buf)) {
                    Ok(_) => {
                        self.flows.lock()[self.index] = Some(true);
                        self.fire_buffering_dips(buf.index);
                        self.advance(index, buf.pts + buf.duration);
                        return;
                    }
                    Err(gst::FlowError::NotLinked) if Instant::now() < give_up_at => {
                        self.flows.lock()[self.index] = Some(false);
                        gst::trace!(
                            CAT,
                            obj = pad,
                            "buffer {} found the pad unlinked, retrying",
                            buf.index
                        );
                        if !self.hold(NOT_LINKED_RETRY) {
                            return;
                        }
                    }
                    Err(gst::FlowError::Flushing) => {
                        // A flush is in progress: it decides where we resume.
                        gst::debug!(
                            CAT,
                            obj = pad,
                            "buffer {} refused as flushing, parking until FLUSH_STOP",
                            buf.index
                        );
                        let _ = pad.pause_task();
                        return;
                    }
                    Err(gst::FlowError::Eos) => {
                        gst::debug!(
                            CAT,
                            obj = pad,
                            "buffer {} refused as EOS, ending the stream",
                            buf.index
                        );
                        self.finish();
                        let _ = pad.pause_task();
                        return;
                    }
                    Err(gst::FlowError::NotLinked) => {
                        // The bound expired. Aggregate like a demuxer's flow
                        // combiner first: while any SIBLING still delivers,
                        // this is a deselected stream (video turned off, an
                        // embedded track unselected), and it idles instead
                        // of taking the whole input down.
                        let sibling_delivers = {
                            let flows = self.flows.lock();
                            flows
                                .iter()
                                .enumerate()
                                .any(|(i, state)| i != self.index && *state == Some(true))
                        };
                        if sibling_delivers {
                            gst::trace!(
                                CAT,
                                obj = pad,
                                "buffer {} unlinked but a sibling delivers; idling",
                                buf.index
                            );
                            if !self.hold(NOT_LINKED_IDLE) {
                                return;
                            }
                            continue;
                        }
                        // Every pad refused: post basesrc's EXACT shape.
                        // fcastplaybin classifies an external input's death by
                        // the "reason not-linked" debug text (recover in
                        // place), and a deselected external dies this way.
                        if let Some(element) = pad.parent_element() {
                            gst::element_error!(
                                element,
                                gst::StreamError::Failed,
                                ("Internal data stream error."),
                                ["{}", FlowStopReason::NotLinked.debug_text()]
                            );
                        }
                        self.finish();
                        let _ = pad.pause_task();
                        return;
                    }
                    Err(err) => {
                        if let Some(element) = pad.parent_element() {
                            gst::element_error!(
                                element,
                                gst::StreamError::Failed,
                                ["flow error on {}: {err:?}", self.stream_id]
                            );
                        }
                        self.finish();
                        let _ = pad.pause_task();
                        return;
                    }
                }
            }
        }

        /// Hands this buffer's anchored buffering dips to the posting worker.
        /// Runs on the streaming thread right after the push succeeded and
        /// never blocks. A schedule restarted behind the index delivers the
        /// buffer again and fires the dip again, exactly like a fault.
        fn fire_buffering_dips(&self, index: u64) {
            let Some(shared) = &self.buffering else {
                return;
            };
            for (buffer_index, recovery) in &self.dips {
                if *buffer_index == index {
                    shared.queue.lock().requests.push(recovery.clone());
                    shared.cv.notify_all();
                }
            }
        }

        /// Only advances when nothing reset us while the push was in flight.
        fn advance(&self, from: usize, position: gst::ClockTime) {
            let mut state = self.state.lock();
            if state.next == from {
                state.next = from + 1;
                state.position = position;
            }
        }

        fn handle_event(self: &Arc<Self>, pad: &gst::Pad, event: gst::Event) -> bool {
            match event.view() {
                gst::EventView::FlushStart(_) => {
                    self.set_flushing(true);
                    true
                }
                gst::EventView::FlushStop(_) => {
                    // The core holds the streaming lock here, so the task is
                    // between steps and the sticky segment is already gone.
                    self.reset(false);
                    self.set_flushing(false);
                    if let Err(err) = self.spawn(pad) {
                        gst::error!(CAT, obj = pad, "failed to restart the task: {err}");
                        return false;
                    }
                    true
                }
                gst::EventView::Seek(seek) => self.handle_seek(pad, seek),
                gst::EventView::SelectStreams(select)
                    if self.scenario.spec().upstream_selection =>
                {
                    self.handle_select_streams(pad, select)
                }
                _ => gst::Pad::event_default(pad, pad.parent().as_ref(), event),
            }
        }

        /// adaptivedemux2's SELECT_STREAMS semantics: one unrecognized id
        /// rejects the WHOLE event (`gstadaptivedemux.c:2494`), a fully-known
        /// set applies, and STREAMS_SELECTED posts only when the selected set
        /// actually changed (the activation edge). The event arrives once per
        /// routed src pad, deduped by seqnum.
        fn handle_select_streams(
            &self,
            pad: &gst::Pad,
            select: &gst::event::SelectStreams,
        ) -> bool {
            let spec = self.scenario.spec();
            let known: Vec<String> = spec
                .streams
                .iter()
                .map(|stream| self.scenario.stream_id(&stream.id))
                .collect();
            let mut requested: Vec<String> =
                select.streams().iter().map(|id| id.to_string()).collect();
            requested.sort();
            for id in &requested {
                if !known.contains(id) {
                    gst::warning!(CAT, obj = pad, "Unrecognized stream_id '{id}'");
                    return false;
                }
            }
            let seqnum = select.event().seqnum();
            {
                let mut state = self.select.lock();
                if state.last_seqnum == Some(seqnum) {
                    return true;
                }
                state.last_seqnum = Some(seqnum);
                let current = state.selected.get_or_insert_with(|| {
                    let mut all = known.clone();
                    all.sort();
                    all
                });
                if *current == requested {
                    gst::debug!(CAT, obj = pad, "selection unchanged, not posting");
                    return true;
                }
                *current = requested.clone();
            }
            let Some(element) = pad
                .parent()
                .and_then(|parent| parent.downcast::<gst::Element>().ok())
            else {
                return false;
            };
            let _ = element.post_message(streams_selected_message(
                &self.scenario,
                &element,
                &requested,
            ));
            true
        }

        /// Flushing 1.0x seeks at any position: the schedule restarts at the
        /// target (see [`start_index`]) and the new segment begins there, which
        /// covers fcastplaybin's zero-seeks and a `StartPoint::Seek` load. Other
        /// rates are refused, the schedule has no trick modes.
        fn handle_seek(self: &Arc<Self>, pad: &gst::Pad, seek: &gst::event::Seek) -> bool {
            let (rate, flags, start_type, start, ..) = seek.get();
            let start = match start {
                gst::GenericFormattedValue::Time(start) => start,
                _ => None,
            };
            // A SeekType::None keeps the historical meaning of "restart from
            // the beginning" rather than "keep the current position".
            let target = match (start_type, start) {
                (gst::SeekType::None, _) => Some(gst::ClockTime::ZERO),
                (gst::SeekType::Set, position) => position,
                _ => None,
            };
            let target = match target {
                Some(target) if rate == 1.0 && flags.contains(gst::SeekFlags::FLUSH) => target,
                _ => {
                    gst::debug!(
                        CAT,
                        obj = pad,
                        "refusing seek rate {rate} flags {flags:?} start {start:?}"
                    );
                    return false;
                }
            };

            self.set_flushing(true);
            pad.push_event(gst::event::FlushStart::new());
            {
                // Waits for the current schedule step to finish.
                let _stream_lock = pad.stream_lock();
                self.reset_to(false, target);
                self.set_flushing(false);
                pad.push_event(gst::event::FlushStop::new(true));
            }
            if let Err(err) = self.spawn(pad) {
                gst::error!(CAT, obj = pad, "failed to restart the task: {err}");
                return false;
            }
            true
        }

        fn handle_query(
            &self,
            pad: &gst::Pad,
            parent: Option<&gst::Object>,
            query: &mut gst::QueryRef,
        ) -> bool {
            match query.view_mut() {
                gst::QueryViewMut::Scheduling(q) => {
                    q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                    q.add_scheduling_modes([gst::PadMode::Push]);
                    true
                }
                gst::QueryViewMut::Duration(q) if q.format() == gst::Format::Time => {
                    q.set(self.spec.duration);
                    true
                }
                gst::QueryViewMut::Position(q) if q.format() == gst::Format::Time => {
                    q.set(self.state.lock().position);
                    true
                }
                // Seekable anywhere at 1.0x, see handle_seek.
                gst::QueryViewMut::Seeking(q) if q.format() == gst::Format::Time => {
                    q.set(true, gst::ClockTime::ZERO, self.spec.duration);
                    true
                }
                // decodebin3 asks at STREAM_START; answering TRUE flips it
                // into upstream-selection mode for the WHOLE element.
                gst::QueryViewMut::Selectable(q)
                    if self.scenario.spec().upstream_selection =>
                {
                    q.set_selectable(true);
                    true
                }
                gst::QueryViewMut::Latency(q) => {
                    q.set(false, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                    true
                }
                _ => gst::Pad::query_default(pad, parent, query),
            }
        }
    }

    #[derive(Default)]
    pub struct FTestSrc {
        uri: Mutex<Option<String>>,
        scenario: Mutex<Option<Arc<Scenario>>>,
        streams: Mutex<Vec<(gst::Pad, Arc<Stream>)>>,
        buffering: Mutex<Option<BufferingCtl>>,
    }

    impl FTestSrc {
        fn store_uri(&self, uri: Option<&str>) -> Result<(), glib::Error> {
            let Some(uri) = uri else {
                *self.uri.lock() = None;
                return Ok(());
            };
            if caps::key_from_uri(uri).is_none() {
                return Err(glib::Error::new(
                    gst::URIError::BadUri,
                    &format!("'{uri}' is not an {}:// URI with a key", caps::URI_SCHEME),
                ));
            }
            *self.uri.lock() = Some(uri.to_owned());
            Ok(())
        }

        /// Resolves the scenario the URI points at. Missing scenarios are a bus
        /// error, not a panic: a test typo has to be readable.
        fn prepare(&self) -> Result<(), gst::StateChangeError> {
            let uri = self.uri.lock().clone();
            let Some(key) = uri.as_deref().and_then(caps::key_from_uri) else {
                gst::element_imp_error!(
                    self,
                    gst::ResourceError::NotFound,
                    ["no {}:// URI set", caps::URI_SCHEME]
                );
                return Err(gst::StateChangeError);
            };
            let Some(scenario) = registry::lookup(key) else {
                gst::element_imp_error!(
                    self,
                    gst::ResourceError::NotFound,
                    ["no scenario registered for key '{key}'"]
                );
                return Err(gst::StateChangeError);
            };
            *self.scenario.lock() = Some(scenario);
            Ok(())
        }

        /// Adds every stream's pad while still in READY, so downstream is linked
        /// before the parent activates the pads and the tasks start pushing.
        fn create_pads(&self) -> Result<(), gst::StateChangeError> {
            let Some(scenario) = self.scenario.lock().clone() else {
                return Err(gst::StateChangeError);
            };
            // All streams of one media share a group.
            let group_id = gst::GroupId::next();
            let mut counters = [0u32; 3];
            let flows: FlowBoard =
                Arc::new(Mutex::new(vec![None; scenario.spec().streams.len()]));
            // The queue exists before any stream so every task can reach it.
            // The worker itself starts with the tasks, see start_buffering.
            self.stop_buffering();
            let buffering = scenario.spec().buffering.as_ref().map(|_| {
                Arc::new(BufferingShared {
                    queue: Mutex::new(BufferingQueue::default()),
                    cv: Condvar::new(),
                })
            });
            *self.buffering.lock() = buffering.clone().map(|shared| BufferingCtl {
                shared,
                worker: None,
            });

            let select: SelectBoard = Arc::new(Mutex::new(SelectState::default()));
            for index in 0..scenario.spec().streams.len() {
                let stream = Stream::new(
                    scenario.clone(),
                    index,
                    group_id,
                    flows.clone(),
                    select.clone(),
                    buffering.clone(),
                );
                let (template, prefix, slot) = pad_layout(&stream.spec.kind);
                let name = format!("{prefix}_{}", counters[slot]);
                counters[slot] += 1;

                let pad = gst::Pad::builder_from_template(template)
                    .name(name)
                    .activatemode_function({
                        let stream = stream.clone();
                        move |pad, _parent, mode, active| {
                            if mode != gst::PadMode::Push {
                                return Err(gst::loggable_error!(
                                    CAT,
                                    "only push mode is supported"
                                ));
                            }
                            if active {
                                // Pads are activated from inside add_pad, before
                                // pad-added ran and downstream got linked, so the
                                // tasks start later, see start_tasks.
                                Ok(())
                            } else {
                                stream.request_stop();
                                pad.stop_task().map_err(|err| {
                                    gst::loggable_error!(CAT, "failed to stop the task: {err}")
                                })
                            }
                        }
                    })
                    .event_function({
                        let stream = stream.clone();
                        move |pad, _parent, event| stream.handle_event(pad, event)
                    })
                    .query_function({
                        let stream = stream.clone();
                        move |pad, parent, query| stream.handle_query(pad, parent, query)
                    })
                    .build();

                self.streams.lock().push((pad.clone(), stream));
                if self.obj().add_pad(&pad).is_err() {
                    gst::element_imp_error!(
                        self,
                        gst::CoreError::Pad,
                        ["failed to add pad {}", pad.name()]
                    );
                    return Err(gst::StateChangeError);
                }
            }
            Ok(())
        }

        /// Starts pushing once every pad has been added and linked, the way a
        /// demuxer only pushes after pad-added and no-more-pads.
        fn start_tasks(&self) -> Result<(), gst::StateChangeError> {
            let streams = self.streams.lock().clone();
            for (pad, stream) in &streams {
                stream.reset(true);
                if stream.spawn(pad).is_err() {
                    gst::element_imp_error!(
                        self,
                        gst::CoreError::Failed,
                        ["failed to start the task of pad {}", pad.name()]
                    );
                    return Err(gst::StateChangeError);
                }
            }
            // An adaptive demuxer announces its initial selection when its
            // tracks activate; the caller's engine seeds from that post.
            if let Some((_, stream)) = streams.first()
                && stream.scenario.spec().upstream_selection
            {
                let mut all: Vec<String> = stream
                    .scenario
                    .spec()
                    .streams
                    .iter()
                    .map(|s| stream.scenario.stream_id(&s.id))
                    .collect();
                all.sort();
                stream.select.lock().selected = Some(all.clone());
                let element = self.obj().clone().upcast::<gst::Element>();
                let _ = element.post_message(streams_selected_message(
                    &stream.scenario,
                    &element,
                    &all,
                ));
            }
            Ok(())
        }

        fn destroy_pads(&self) {
            for (pad, _) in std::mem::take(&mut *self.streams.lock()) {
                let _ = self.obj().remove_pad(&pad);
            }
        }

        /// Starts the buffering worker, when the media schedules buffering.
        /// Runs after the pads are added and linked, next to the tasks, so
        /// the initial low post lands during the pipeline's first preroll.
        fn start_buffering(&self) {
            let Some(scenario) = self.scenario.lock().clone() else {
                return;
            };
            let Some(spec) = scenario.spec().buffering.clone() else {
                return;
            };
            let mut ctl = self.buffering.lock();
            let Some(ctl) = ctl.as_mut() else {
                return;
            };
            if ctl.worker.is_some() {
                return;
            }
            let element = self.obj().downgrade();
            let shared = ctl.shared.clone();
            ctl.worker = Some(
                std::thread::Builder::new()
                    .name("ftestsrc-buffering".to_owned())
                    .spawn(move || run_buffering_schedule(element, scenario, spec, shared))
                    .expect("spawning the buffering worker"),
            );
        }

        /// Stops and joins the buffering worker. The worker re-checks its
        /// queue at least every [`STALL_POLL`], so the join is bounded even
        /// while a dip waits on a sync point the test never released.
        fn stop_buffering(&self) {
            let Some(ctl) = self.buffering.lock().take() else {
                return;
            };
            ctl.shared.queue.lock().stopping = true;
            ctl.shared.cv.notify_all();
            if let Some(worker) = ctl.worker {
                let _ = worker.join();
            }
        }
    }

    impl ObjectImpl for FTestSrc {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecString::builder("uri")
                        .nick("URI")
                        .blurb("ftest:// URI naming the scenario to serve")
                        .readwrite()
                        .mutable_ready()
                        .build(),
                ]
            });

            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "uri" => {
                    let uri = value.get::<Option<&str>>().expect("type checked upstream");
                    if let Err(err) = self.store_uri(uri) {
                        gst::error!(CAT, imp = self, "failed to set the URI: {err}");
                    }
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "uri" => self.uri.lock().to_value(),
                _ => unimplemented!(),
            }
        }
    }

    impl GstObjectImpl for FTestSrc {}

    impl ElementImpl for FTestSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "FCast Test Source",
                        "Source/Testing",
                        "Serves a registered fcasttest scenario as one stream per pad",
                        "Marcus Hanestad <marcus@futo.org>",
                    )
                });

            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            PAD_TEMPLATES.as_ref()
        }

        fn send_event(&self, event: gst::Event) -> bool {
            // The default implementation picks one random src pad, a seek has to
            // reach every stream.
            if let gst::EventView::Seek(seek) = event.view() {
                let streams = self.streams.lock().clone();
                if !streams.is_empty() {
                    let mut res = true;
                    for (pad, stream) in &streams {
                        res &= stream.handle_seek(pad, seek);
                    }
                    return res;
                }
            }
            self.parent_send_event(event)
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            match transition {
                gst::StateChange::NullToReady => self.prepare()?,
                gst::StateChange::ReadyToPaused => self.create_pads()?,
                _ => (),
            }

            let res = self.parent_change_state(transition)?;

            match transition {
                gst::StateChange::ReadyToPaused => {
                    self.obj().no_more_pads();
                    self.start_tasks()?;
                    self.start_buffering();
                }
                gst::StateChange::PausedToReady => {
                    self.stop_buffering();
                    self.destroy_pads();
                }
                gst::StateChange::ReadyToNull => {
                    // Covers a ReadyToPaused that failed before the tasks
                    // started, where PausedToReady never runs.
                    self.stop_buffering();
                    *self.scenario.lock() = None;
                }
                _ => (),
            }

            Ok(res)
        }
    }

    impl URIHandlerImpl for FTestSrc {
        const URI_TYPE: gst::URIType = gst::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &[caps::URI_SCHEME]
        }

        fn uri(&self) -> Option<String> {
            self.uri.lock().clone()
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            self.store_uri(Some(uri))
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FTestSrc {
        const NAME: &'static str = "FTestSrc";
        type Type = super::FTestSrc;
        type ParentType = gst::Element;
        type Interfaces = (gst::URIHandler,);
    }
}

use gst::{glib, prelude::*};

glib::wrapper! {
    pub struct FTestSrc(ObjectSubclass<imp::FTestSrc>)
        @extends gst::Element, gst::Object,
        @implements gst::URIHandler;
}

pub fn register() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "ftestsrc",
        gst::Rank::PRIMARY,
        FTestSrc::static_type(),
    )
}
