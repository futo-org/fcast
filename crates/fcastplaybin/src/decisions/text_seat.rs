//! The text seat contest: the pure rules behind
//! [`crate::Inner::poll_text_policy`].
//!
//! ONE consumer branch may feed the subtitle transport at a time (the
//! one-live-branch rule), so every routed text pad carrying the selected stream
//! competes for a single seat. Which pad holds it, when the seat moves and why
//! a candidate is refused used to be inline in a 1,200-line poll, reachable
//! only by provoking a live decodebin3 into shapes that took 40-second field
//! captures to observe once. They are pure functions over a projection here, so
//! every documented hazard is one row of a table test instead.
//!
//! The rules read a [`TextEntryView`] per routed text entry and never a pad, an
//! element or a lock. The projection is built under the routing lock (see
//! [`crate::routing::RoutingState::text_seat_view`]) and the caller applies the
//! verdicts to the real entries, so every gst call and every mutation stays on
//! the decider's side of the boundary.

use std::fmt;

/// One routed text entry, projected to exactly what the seat rules read.
///
/// `index` is the entry's position in `RoutingState::routed`, kept for two
/// reasons: the superseded-order rule is ORDERED on it (append-only routing is
/// the direction decodebin3 replaces an output in), and the caller applies the
/// verdict there. The projection preserves routed order, so a comparison of
/// view positions is a comparison of routed indices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextEntryView {
    pub(crate) index: usize,
    /// This entry's pad carries the sid the link policy allows (see
    /// [`allowed_sid`]). FALSE for every entry when nothing is allowed, which
    /// is what keeps the three sid-scoped reclaim rules from firing at all
    /// then, without any of them repeating the `is_some` gate.
    pub(crate) same_sid: bool,
    /// A consumer branch is linked to this pad (`downstream.is_some()`).
    pub(crate) seated: bool,
    /// decodebin3 exposed another output pad for this entry's stream, so this
    /// pad is the one it left behind. Not permanent (see
    /// [`crate::routing::RoutedStream::superseded`]).
    pub(crate) superseded: bool,
    /// The dead-branch reclaim took this entry's branch out. Cleared by proof
    /// of life, which is a sticky segment on the pad.
    pub(crate) evicted_dead: bool,
    /// An EOS crossed the pad since its last STREAM_START or FLUSH_STOP, i.e.
    /// decodebin3 has ENDED the slot this output ghosts.
    pub(crate) saw_eos: bool,
    /// A sticky SEGMENT is on the pad, i.e. a branch built on it could time its
    /// first cue.
    pub(crate) has_segment: bool,
    /// The flow ticket the most recent buffer to cross the pad stamped, 0 if
    /// none ever has (see [`crate::routing::RoutedStream::last_buffer`]).
    pub(crate) flow: u64,
}

impl TextEntryView {
    /// Holds the one consumer seat for the allowed stream.
    pub(crate) fn holds_seat(&self) -> bool {
        self.same_sid && self.seated
    }

    /// Carries the allowed stream with no branch on it: what every reclaim
    /// moves the seat TOWARDS and what the link loop considers.
    pub(crate) fn waiting(&self) -> bool {
        self.same_sid && !self.seated
    }

    /// A waiting same-sid pad the seat could actually move to. Scopes the EOS
    /// seat reclaim and the link loop's EOS refusal (one rule, one lever), so
    /// it must not count a pad that loop refuses itself: an evicted, still
    /// segmentless husk as the "live rival" left the seat EMPTY forever
    /// (mid-play detach + same-URL re-attach, where the husk's input is gone
    /// and the newcomer's slot EOSed before its first poll).
    pub(crate) fn seat_ready_rival(&self) -> bool {
        self.waiting()
            && !self.superseded
            && !self.saw_eos
            && !(self.evicted_dead && !self.has_segment)
    }
}

/// Which stream, if any, the link policy may build a branch for.
///
/// Only the selected subtitle stream may relink. A disabled stream stays routed
/// until decodebin3 removes its pad, and relinking it would resurrect the cue
/// the eager detach just cleared.
///
/// A stream the applied selection names while the DESIRED state is an explicit
/// subtitle-off is decodebin3's collection-default auto-select stomping that
/// off (an attach makes it re-select over the applied state), the TEXT twin of
/// the video resurrect `route_db3_pad` guards against. Splicing it into a fresh
/// branch adds a reconfiguration to whatever transition is in flight, that
/// transition never completes, and the receiver's pump gate
/// (`quiet = running && !has_async_transition()`) then holds the engine's
/// corrective re-assert back forever, so the only thing that would undo the
/// stomp is postponed behind the stall the stomp caused. Leaving the stream
/// parked keeps the pipeline settled, and the re-assert dispatches on the next
/// pump and makes decodebin3 drop the pad. `fuzz_buffering` seed 400009, whose
/// schedule ends `disable_subtitles` then `attach_external`. The staging knob
/// (`TestStaging::link_stomped_subtitle`) restores the unconditional relink and
/// gates this whole rule.
///
/// The companion invariant of the eager disable detach (CLEANUP.md): between a
/// disable's dispatch and decodebin3's pad removal the stream is still routed,
/// and a caller's settle-point poll would otherwise relink it.
pub(crate) fn allowed_sid(
    explicitly_off: bool,
    stage_link_stomped: bool,
    applied: Option<String>,
) -> Option<String> {
    if explicitly_off && !stage_link_stomped {
        None
    } else {
        applied
    }
}

/// Whether the contention latches have outlived their argument and must be
/// cleared for the link loop to re-run the contest.
///
/// THE STALEMATE BREAK, and it runs before every reclaim because all of them
/// can only MOVE a seat that exists.
///
/// `superseded` and `evicted_dead` are latches that hold a pad out of
/// contention, and both are only ever justified BY A COMPETITOR: they exist so
/// two same-sid entries cannot trade the one consumer branch back and forth
/// once per poll. With NOTHING holding the seat there is no competitor and
/// nothing to protect, and the latches stop being a tie-break and become a
/// lock-out.
///
/// How both get set with no survivor, measured on
/// `external_subtitle_lifecycle::reattaching_the_same_url_while_paused_renders_after_resume`
/// (137 failures in 160 runs at 16-way load, 46.4 ms to 46.9 ms of one
/// capture):
///
///   text_0 joins and holds the seat
///   the same URL is detached, then re-attached while PAUSED
///   decodebin3 exposes text_1 for the SAME sid
///   the superseded reclaim evicts text_0   -> text_0.superseded
///   text_1 joins and holds the seat
///   decodebin3 RECYCLES the pad name text_0 for the re-attached input
///     (the walk-back), so a FRESH routed entry for text_0 is appended with
///     clean flags, after text_1
///   the superseded reclaim now reads that fresh entry as the newest and
///     evicts text_1                        -> text_1.superseded
///   the late detach disposes text_0's branch, text_0 rejoins segmentless, and
///     the segmentless-holder reclaim takes it out
///                                          -> text_0.evicted_dead
///
/// End state: every same-sid entry latched out, `downstream: None` on all of
/// them, and the link loop refusing both forever - `text_0: seat-evicted and
/// still segmentless`, `text_1: decodebin3 replaced this pad for the same
/// stream`, 4024 times over 40 s, with the selection CONFIRMED and the caller
/// shown a track that never appears. No reclaim can heal it: each one needs a
/// holder to move.
///
/// So: no holder, and nothing admissible, means the latches have outlived their
/// argument. Clear them and let the link loop re-run the contest; whichever pad
/// is actually carrying data wins the seat back through the flow reclaim on the
/// next poll, which is the self-stabilising predicate that already exists.
pub(crate) fn stalemate_broken(view: &[TextEntryView]) -> bool {
    let mut candidates = 0usize;
    let mut seated = false;
    let mut locked_out = true;
    for entry in view.iter().filter(|entry| entry.same_sid) {
        candidates += 1;
        seated |= entry.seated;
        locked_out &= entry.superseded || entry.evicted_dead;
    }
    !seated && candidates > 0 && locked_out
}

/// WHICH of the two verdicts a reclaim leaves on the pad it takes the seat
/// from. The two STORED flags stay two, and this only says how to set them: a
/// holder that merely lost its segment may yet revive, and `evicted_dead`
/// alone is the right, clearable verdict for it; a holder decodebin3 has
/// REPLACED never will (see [`crate::routing::RoutedStream::superseded`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeatVerdict {
    /// `evicted_dead` only. Clears on a segment appearing on the pad.
    Dead,
    /// `evicted_dead` and `superseded`. Clears only on a buffer, through the
    /// flow rule.
    Replaced,
}

/// Which of the four reclaim rules moved the seat. Only the caller's log lines
/// and counters read it; the rules themselves are ordered by their position in
/// [`select_victim`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReclaimRule {
    /// A holder with no sticky segment while a same-sid pad waits.
    SegmentlessHolder,
    /// A holder whose decodebin3 slot has ENDED while a live rival waits.
    EndedSlot,
    /// A holder decodebin3 replaced with a LATER output for the same stream.
    SupersededOrder,
    /// A waiting pad has carried a buffer more recently than the holder.
    FlowTicket,
}

/// The seat move one poll decided on: which entry loses the branch, what
/// verdict it keeps, and which rule said so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reclaim {
    /// Routed index of the entry the branch comes off.
    pub(crate) held: usize,
    pub(crate) verdict: SeatVerdict,
    pub(crate) rule: ReclaimRule,
    /// Routed index of the pad the seat is moving TOWARDS, whose stale
    /// verdicts must be cleared first. Only the flow rule names one: it is the
    /// only rule that can seat a pad an earlier reclaim condemned.
    pub(crate) revive: Option<usize>,
    /// Flow tickets, for the flow rule's log line only.
    pub(crate) held_flow: u64,
    pub(crate) winner_flow: u64,
}

/// The seat's victim, from the four reclaim rules in their load-bearing order.
///
/// THE ORDER IS THE RULE. Each rule only runs where the ones above it found no
/// victim, and every one of them was added because the rules above it cannot
/// SEE the shape it answers. Reordering them silently changes which pad the
/// seat lands on.
///
/// # 1. The segment-less holder
///
/// The overlay's one subtitle seat may still be held by a DEAD branch. A
/// detached input's decodebin3 output pad can linger linked to a live branch
/// past `remove_input` (the id stays in the collection while a same-id stream
/// re-materializes, so no pad-removed fires), its sticky segment wiped by the
/// removal's flush with nothing upstream left to ever send another one. Left
/// alone it holds the ONE live text slot forever and the `consumer_branch_live`
/// check in the link loop refuses every later text stream with no error
/// surfaced anywhere. A branch that will render again always gets a segment
/// re-sent by its own reconfigure, so a holder WITHOUT one, while a parked
/// stream of the selected sid is waiting, is beyond recovery.
///
/// KEPT when subtitleoverlay went. The rule was written against the overlay's
/// geometry, where "holds the seat" meant "occupies `subtitle_sink`". The
/// consumer transport states the same scarcity itself
/// (`consumer_branch_live`), so a dead branch blocks its successor exactly as
/// before and
/// `external_subtitle_lifecycle::reattaching_the_same_url_after_a_detach_renders_again`
/// still pins it. The holder search is deliberately NOT scoped to the allowed
/// sid: any segmentless text holder is blocking the seat.
///
/// # 2. The seat decodebin3 has already ENDED
///
/// The shape none of the other three can see. On a re-select in
/// upstream-selection mode decodebin3 builds a FRESH slot for the re-added
/// input and CLEARS + EOSes the old one (`remove_input_stream` -> "Sending EOS
/// to unused slot", gstdecodebin3.c:1232/1313), leaving the OUTPUT pad this
/// entry holds ghosted onto a slot that is over. What the other rules can read
/// about that pad:
///
/// * its sticky SEGMENT survives the clear, so rule 1 says it is healthy;
/// * routed ORDER says nothing, because the replacement output can be built
///   either side of it (decodebin3 recycles outputs in both directions) and in
///   one capture there was no new entry at all for seconds;
/// * its `last_buffer` ticket is whatever it was, and a rival that has not
///   carried a buffer YET stamps zero, so rule 4 cannot order a dead pad
///   against a live-but-idle one, which is precisely a sparse subtitle track's
///   normal condition.
///
/// `saw_eos` asks decodebin3 instead. A holder whose slot has ended can never
/// deliver again, and the ONLY thing keeping it in the seat is that nothing
/// said so.
///
/// SCOPED TO A LIVE RIVAL, which is what keeps a legitimate end-of-item EOS
/// from being a repair trigger: at the end of an item every same-sid pad is EOS
/// and this finds no rival, so it does nothing at all and the branch stays up
/// to carry the EOS through. It fires only where there is somewhere better for
/// the seat to go.
///
/// [`SeatVerdict::Dead`] deliberately, NOT `Replaced`: `superseded` is the
/// verdict for a pad decodebin3 has REPLACED, and this one may yet be
/// re-pointed at a live slot, at which point its STREAM_START clears `saw_eos`
/// by itself. The link loop's own EOS refusal keeps the evicted pad out of
/// contention meanwhile, and is scoped identically, so the pair cannot trade
/// the seat back and forth: the flag only clears on proof of life.
///
/// # 3. Routed order, the replacement decodebin3 announced
///
/// THE SECOND SHAPE OF A DEAD HOLDER, and the one `dash-reenable-freeze.txt`
/// hit: decodebin3 EXPOSED A NEW PAD FOR THE SAME STREAM.
/// `db_output_stream_new` names outputs off per-type counters that are never
/// decremented (gstdecodebin3.c:4761-4784), and `find_free_compatible_output`
/// refuses to re-use an existing output whose slot's stream is still REQUESTED
/// (3169-3183). So a flushing seek that re-slots a subtitle stream which stays
/// selected - a `Job::RefreshSeek` at a re-enable is exactly that - produces a
/// SECOND text pad, `text_1`, beside a `text_0` that will never carry another
/// buffer.
///
/// Rule 1 cannot see it: whether `text_0`'s pad kept its pre-seek sticky
/// depends on whether the flush reached it, so the holder can be stone dead
/// with a segment still on it. ROUTED ORDER can: `routing.routed` is
/// append-only per route, so a waiting entry that comes AFTER the holder and
/// carries the same stream id is decodebin3's replacement for it. Without this
/// the field log's shape is permanent: `consumer_branch_live` refuses `text_1`
/// on every later poll, the caller's diagnostic is suppressed because the dead
/// holder makes `already_joined` true, and the track renders nothing for the
/// rest of the item.
///
/// THE TWO CONSTRAINTS THAT KEEP IT FROM EATING THE GRAPH. Measured, not
/// argued: without them `dash_testbed`'s `dash_embedded_text_track_plays`
/// evicted `text_0` and then `text_1` 10 ms later and rendered nothing at all.
/// The eviction FEEDS the predicate - an evicted holder becomes a waiting entry
/// of the allowed sid - so
///
/// * only a LATER entry may supersede an earlier one (`newest > held`), which
///   is the direction decodebin3 actually replaces in, and
/// * an entry this reclaim already took out (`superseded`) or evicted
///   (`evicted_dead`) cannot be the one that justifies the next eviction.
///
/// The rival must also be ALIVE, or the verdict is wrong on its face: this rule
/// exists for "decodebin3 replaced the holder with THIS pad", and a rival whose
/// own slot has ended is not a replacement for anything, just a second corpse.
/// Measured: a re-select found both same-sid pads EOSed, this rule flipped the
/// seat from one dead pad to the other and LATCHED the first superseded, 0.5 s
/// before decodebin3 re-pointed that very pad (the reuse shape), which then had
/// to be re-admitted through rule 4.
///
/// # 4. The flow ticket, the walk-back
///
/// THE THIRD SHAPE: decodebin3 walked BACK onto a pad this reclaim had already
/// condemned.
///
/// Rules 1 and 3 are ordered on `routing.routed`, which is append-only, so both
/// can only ever move the seat FORWARD. That was believed to be the only
/// direction decodebin3 replaces in. Measured against `manifest-text-seg.mpd`,
/// it is not: at the SECOND off/on the old text input has drained and been
/// released before the re-enable's new input is added, so slot 2 is free,
/// `gst_decodebin_get_slot_for_input_stream_locked` takes it as the
/// lowest-indexed unused compatible slot ("Re-using existing unused slot 2",
/// gstdecodebin3.c:3886) and `db_output_stream_reconfigure` re-points the
/// ORIGINAL pad `text_0` at it. At the first off/on the old input was still on
/// slot 2, so decodebin3 built slot 3 and a new pad `text_1` instead. Forward
/// then back, and the crate, holding `text_1` with `text_0` marked permanently
/// superseded, refused the only pad still carrying cues for the rest of the
/// item: 40 s of `queue_2` pushing one buffer per second into the parking
/// fakesink while `fpb-tqueue-text_1` saw nothing and every adaptivedemux2 push
/// returned `ok`.
///
/// So the seat follows the DATA, which is the thing routed order was only ever
/// a proxy for. A waiting same-sid pad that has carried a buffer MORE RECENTLY
/// than the holder is the pad decodebin3 is feeding now, whichever side of the
/// holder it sits on, and no sticky can argue otherwise (a superseded pad's
/// segment is whatever it held before the flush; a buffer is not).
///
/// Why this cannot thrash, which is what rule 3's ordering constraint was there
/// to prevent: the predicate is self-stabilising. It seats the pad that is
/// carrying data and leaves the loser carrying none, and a pad carrying none
/// can never satisfy `>` against one that is. An eviction therefore cannot feed
/// the next one, which is exactly the failure (`text_0` evicted, then `text_1`
/// 10 ms later, nothing rendered) the `superseded`/`evicted_dead` guards were
/// added for.
///
/// SEAT-READY, not merely fresher. The freshness test on its own can be
/// satisfied by a ticket stamped BEFORE the re-enable's flushing seek, so it
/// will seat the winner inside the window where that flush has stripped the
/// pad's stickies and the new segment has not arrived yet. The branch is built
/// and linked there, and the first buffer out of the slot then crosses it with
/// no segment to compute a running time from: measured as one
/// `fpb-tqueue-text_0` "Got data flow before segment event" per re-seat, and a
/// cue timed off a missing segment is exactly the silent corruption this whole
/// path exists to prevent. Waiting for the segment costs one poll (10 ms) and
/// is the same proof of life rule 1's verdict already clears on.
///
/// The FRESHEST waiting pad wins, so a third pad cannot leave the seat on a
/// stale one; ties go to the later entry, which is decodebin3's replacement
/// direction.
pub(crate) fn select_victim(view: &[TextEntryView]) -> Option<Reclaim> {
    let plain = |held: usize, verdict: SeatVerdict, rule: ReclaimRule| Reclaim {
        held: view[held].index,
        verdict,
        rule,
        revive: None,
        held_flow: 0,
        winner_flow: 0,
    };
    // RULE 1: the segment-less holder, whoever it carries.
    if view.iter().any(TextEntryView::waiting)
        && let Some(held) = view
            .iter()
            .position(|entry| entry.seated && !entry.has_segment)
    {
        return Some(plain(
            held,
            SeatVerdict::Dead,
            ReclaimRule::SegmentlessHolder,
        ));
    }
    // RULE 2: the slot decodebin3 has ended, while a live rival waits.
    if let Some(held) = view.iter().position(TextEntryView::holds_seat)
        && view[held].saw_eos
        && view.iter().any(TextEntryView::seat_ready_rival)
    {
        return Some(plain(held, SeatVerdict::Dead, ReclaimRule::EndedSlot));
    }
    // RULE 3: a LATER, admissible, live pad for the same stream.
    let newest = view.iter().rposition(|entry| {
        entry.waiting() && !entry.superseded && !entry.evicted_dead && !entry.saw_eos
    });
    if let Some(newest) = newest
        && let Some(held) = view.iter().position(TextEntryView::holds_seat)
        && held < newest
    {
        return Some(plain(
            held,
            SeatVerdict::Replaced,
            ReclaimRule::SupersededOrder,
        ));
    }
    // RULE 4: whichever pad decodebin3 is actually feeding.
    if let Some(held) = view.iter().position(TextEntryView::holds_seat) {
        let held_flow = view[held].flow;
        let fresher = view
            .iter()
            .filter(|entry| entry.waiting() && entry.has_segment)
            .filter(|entry| entry.flow > held_flow)
            .max_by_key(|entry| entry.flow);
        if let Some(winner) = fresher {
            return Some(Reclaim {
                held: view[held].index,
                verdict: SeatVerdict::Replaced,
                rule: ReclaimRule::FlowTicket,
                revive: Some(winner.index),
                held_flow,
                winner_flow: winner.flow,
            });
        }
    }
    None
}

/// Why the link loop refused one candidate, formatted LAZILY.
///
/// Every arm used to `format!` its line into a `Vec<String>` whether or not the
/// debug line that reports them was enabled, which is six string allocations
/// per refused candidate per poll (~600 allocations a second at receiver
/// cadence, for a usually-filtered log line). The variant is a discriminant;
/// the pad-derived detail is read back off the pad at format time by the
/// caller's wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// No stream id yet, or not the allowed one.
    SidNotAllowed,
    /// decodebin3 replaced this pad for the same stream.
    Superseded,
    /// The pad's decodebin3 slot has ENDED and a live rival is waiting.
    SlotEnded,
    /// The seat reclaim evicted this entry and its pad is still segmentless.
    EvictedSegmentless,
    /// Another branch already feeds the consumer (the one-live-branch rule).
    ConsumerBusy,
    /// GStreamer already refused to wire this branch under this load.
    Unwirable,
    /// The caps are not subtitles the renderer can carry, or absent.
    CapsUnsupported,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::SidNotAllowed => "its sid is not the allowed one",
            Refusal::Superseded => "decodebin3 replaced this pad for the same stream",
            Refusal::SlotEnded => "its decodebin3 slot ended",
            Refusal::EvictedSegmentless => "seat-evicted and still segmentless",
            Refusal::ConsumerBusy => "another text branch already feeds the consumer",
            Refusal::Unwirable => "its branch could not be wired under this load",
            Refusal::CapsUnsupported => "its caps are not subtitles the renderer can carry",
        })
    }
}

/// What the rest of the contest says about one candidate, snapshotted before
/// the link loop starts mutating entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SeatContest {
    /// A SEAT-READY candidate for the allowed sid exists (see
    /// [`TextEntryView::seat_ready_rival`]). Read once rather than per
    /// candidate, and blind to pads the loop refuses itself, so the loop cannot
    /// refuse every pad on the strength of a rival it is about to refuse as
    /// well.
    pub(crate) live_rival_waiting: bool,
    /// A consumer branch is already feeding. Updated when the loop links one.
    pub(crate) consumer_branch_live: bool,
}

/// What the admission cascade concluded about one candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Admission {
    /// The `evicted_dead` latch this candidate cleared by proof of life. Set
    /// INDEPENDENTLY of the refusal, because a candidate refused by a LATER arm
    /// has still shown its proof: the clear is what lets it compete on the next
    /// poll, when the branch ahead of it may be gone.
    pub(crate) revived: bool,
    /// `None` means the candidate may be built.
    pub(crate) refusal: Option<Refusal>,
}

/// THE ADMISSION CASCADE: six arms, in an order that is itself the rule.
///
/// Everything here is about WHICH stream may take the seat; everything after it
/// in the link loop is construction.
///
/// 1. Only the allowed stream relinks (see [`allowed_sid`]). Silent until a
///    field log needed it: a switched-to external whose branch never joined
///    left no trace of WHY, and every refusal is a `continue`.
///
/// 2. A pad decodebin3 has REPLACED never competes again, and no sticky can
///    argue otherwise (see [`crate::routing::RoutedStream::superseded`]).
///    Without this the reclaim and this loop trade the consumer back and forth
///    once per poll, which is a busier version of the wedge the reclaim exists
///    to break.
///
/// 3. A pad whose decodebin3 slot has ENDED cannot deliver another buffer, so
///    seating it is the silent wrong seat: the branch joins, reports success,
///    and refuses every live rival for the life of the item. BEFORE the
///    `evicted_dead` arm ON PURPOSE - that arm CLEARS its verdict for any pad
///    carrying a segment, and a pad decodebin3 EOSed keeps the segment it had,
///    so letting it run first would hand the seat straight back to the pad the
///    reclaim just took it from. Scoped exactly as that reclaim is, only while
///    a rival with a LIVE slot is waiting: with no rival this refuses nothing,
///    so a track whose only pad has EOSed keeps its branch (and its end-of-item
///    EOS reaches the consumer) instead of the caller being told the stream is
///    unrenderable. One rule with the reclaim it pairs with, so the two move
///    together.
///
/// 4. An entry the seat reclaim evicted stays out of contention while its pad
///    remains segmentless. Relinked, it would only win the seat back from the
///    same-sid stream that can render (routed order is stable, and the evicted
///    pad comes first). A segment on the pad means the branch revived and may
///    compete again - which is [`Admission::revived`], recorded even when a
///    later arm refuses.
///
/// 5. ONE live consumer branch, by construction. This used to come free from
///    subtitleoverlay's single `subtitle_sink` being physically occupied. A
///    per-stream appsink has no such natural limit, so the rule is stated.
///    Without it a poll that runs before an outgoing branch's disposal has
///    landed can link a SECOND branch, and both then feed the one consumer: two
///    tracks interleaved on screen, and a `Clear` from either wiping the other.
///
/// 6. A stream whose branch could not be WIRED under this load is not tried
///    again: the link is decided on caps GStreamer has already refused once,
///    and the poll runs every tick. The caller was told at the first refusal
///    (see `TextDegradation::Unwirable`). Asked LAZILY, because the answer
///    costs a lock and a key allocation and the arms above it filter almost
///    every candidate.
///
/// The caps gate is deliberately not here: it is the seventh refusal but it
/// reads the pad's caps and emits two different degradation reports off them,
/// so it stays beside the reporting it drives.
pub(crate) fn admit(
    entry: &TextEntryView,
    contest: &SeatContest,
    unwirable: impl FnOnce() -> bool,
) -> Admission {
    let refused = |refusal| Admission {
        revived: false,
        refusal: Some(refusal),
    };
    if !entry.same_sid {
        return refused(Refusal::SidNotAllowed);
    }
    if entry.superseded {
        return refused(Refusal::Superseded);
    }
    if contest.live_rival_waiting && entry.saw_eos {
        return refused(Refusal::SlotEnded);
    }
    let mut revived = false;
    if entry.evicted_dead {
        if !entry.has_segment {
            return refused(Refusal::EvictedSegmentless);
        }
        revived = true;
    }
    let refusal = if contest.consumer_branch_live {
        Some(Refusal::ConsumerBusy)
    } else if unwirable() {
        Some(Refusal::Unwirable)
    } else {
        None
    };
    Admission { revived, refusal }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view row. Defaults are a healthy waiting candidate for the allowed
    /// stream, so each row states only the hazard it is about.
    fn entry(index: usize) -> TextEntryView {
        TextEntryView {
            index,
            same_sid: true,
            seated: false,
            superseded: false,
            evicted_dead: false,
            saw_eos: false,
            has_segment: true,
            flow: 0,
        }
    }

    fn seated(index: usize) -> TextEntryView {
        TextEntryView {
            seated: true,
            ..entry(index)
        }
    }

    // ---- allowed_sid -----------------------------------------------------

    #[test]
    fn allowed_sid_table() {
        let sid = || Some("t0".to_string());
        // (explicitly off, staging lever, applied) -> allowed
        assert_eq!(
            allowed_sid(false, false, sid()),
            sid(),
            "the applied stream"
        );
        assert_eq!(allowed_sid(false, false, None), None, "nothing applied");
        assert_eq!(
            allowed_sid(true, false, sid()),
            None,
            "the stomp guard: an explicit off outranks an applied sid"
        );
        assert_eq!(
            allowed_sid(true, true, sid()),
            sid(),
            "the staging lever restores the unconditional relink"
        );
        assert_eq!(allowed_sid(true, true, None), None, "nothing to restore");
    }

    // ---- stalemate break -------------------------------------------------

    #[test]
    fn stalemate_table() {
        // No candidate at all: nothing to clear.
        assert!(!stalemate_broken(&[]), "an empty table");
        assert!(
            !stalemate_broken(&[TextEntryView {
                same_sid: false,
                superseded: true,
                evicted_dead: true,
                ..entry(0)
            }]),
            "another stream's latched-out pads are not this contest"
        );
        // A holder exists: every reclaim can still move the seat.
        assert!(
            !stalemate_broken(&[
                seated(0),
                TextEntryView {
                    superseded: true,
                    ..entry(1)
                }
            ]),
            "a seated holder means a competitor exists"
        );
        // Latched out but a clean candidate remains: the link loop can seat it.
        assert!(
            !stalemate_broken(&[
                TextEntryView {
                    superseded: true,
                    ..entry(0)
                },
                entry(1)
            ]),
            "one admissible candidate is not a stalemate"
        );
        // THE 137-of-160 SHAPE: every same-sid entry latched, nothing seated.
        assert!(
            stalemate_broken(&[
                TextEntryView {
                    evicted_dead: true,
                    has_segment: false,
                    ..entry(0)
                },
                TextEntryView {
                    superseded: true,
                    ..entry(1)
                }
            ]),
            "the re-attach stalemate: superseded + evicted_dead with no holder"
        );
        // Either latch alone locks a pad out, so either alone counts.
        assert!(
            stalemate_broken(&[TextEntryView {
                superseded: true,
                ..entry(0)
            }]),
            "a lone superseded pad with no holder"
        );
        assert!(
            stalemate_broken(&[TextEntryView {
                evicted_dead: true,
                ..entry(0)
            }]),
            "a lone evicted pad with no holder"
        );
        // A holder for ANOTHER stream does not rescue this contest.
        assert!(
            stalemate_broken(&[
                TextEntryView {
                    same_sid: false,
                    seated: true,
                    ..entry(0)
                },
                TextEntryView {
                    superseded: true,
                    ..entry(1)
                }
            ]),
            "a foreign holder is not this stream's competitor"
        );
    }

    // ---- victim selection ------------------------------------------------

    #[test]
    fn no_victim_without_a_holder() {
        assert_eq!(select_victim(&[]), None, "an empty table");
        assert_eq!(select_victim(&[entry(0)]), None, "a waiting pad alone");
        assert_eq!(
            select_victim(&[seated(0)]),
            None,
            "a healthy holder with no rival"
        );
    }

    #[test]
    fn rule_1_segmentless_holder() {
        // The dead holder whose flush wiped its segment, with a rival waiting.
        let view = [
            TextEntryView {
                has_segment: false,
                ..seated(0)
            },
            entry(1),
        ];
        let reclaim = select_victim(&view).expect("the segmentless holder loses the seat");
        assert_eq!(reclaim.held, 0);
        assert_eq!(reclaim.rule, ReclaimRule::SegmentlessHolder);
        assert_eq!(
            reclaim.verdict,
            SeatVerdict::Dead,
            "it may yet be re-sent a segment"
        );
        assert_eq!(reclaim.revive, None);
    }

    #[test]
    fn rule_1_needs_a_waiting_pad() {
        // Nothing is waiting, so evicting the holder buys nothing and costs
        // the item its branch.
        let view = [TextEntryView {
            has_segment: false,
            ..seated(0)
        }];
        assert_eq!(select_victim(&view), None);
    }

    #[test]
    fn rule_1_holder_is_not_scoped_to_the_allowed_sid() {
        // Any segmentless text holder blocks the one seat.
        let view = [
            TextEntryView {
                same_sid: false,
                has_segment: false,
                ..seated(0)
            },
            entry(1),
        ];
        let reclaim = select_victim(&view).expect("a foreign segmentless holder still blocks");
        assert_eq!(reclaim.held, 0);
        assert_eq!(reclaim.rule, ReclaimRule::SegmentlessHolder);
    }

    #[test]
    fn rule_2_ended_slot_with_a_live_rival() {
        let view = [
            TextEntryView {
                saw_eos: true,
                ..seated(0)
            },
            entry(1),
        ];
        let reclaim = select_victim(&view).expect("the ghosted holder loses the seat");
        assert_eq!(reclaim.held, 0);
        assert_eq!(reclaim.rule, ReclaimRule::EndedSlot);
        assert_eq!(
            reclaim.verdict,
            SeatVerdict::Dead,
            "a re-pointed pad clears saw_eos itself, so this is not a replacement"
        );
    }

    #[test]
    fn rule_2_is_silent_at_the_end_of_an_item() {
        // Every pad EOSed: no live rival, so the branch stays up to carry the
        // EOS through.
        let view = [
            TextEntryView {
                saw_eos: true,
                ..seated(0)
            },
            TextEntryView {
                saw_eos: true,
                ..entry(1)
            },
        ];
        assert_eq!(select_victim(&view), None);
    }

    #[test]
    fn rule_2_ignores_a_husk_rival() {
        // The evicted, still-segmentless husk is not seat-ready, and counting
        // it left the seat EMPTY forever.
        let view = [
            TextEntryView {
                saw_eos: true,
                ..seated(0)
            },
            TextEntryView {
                evicted_dead: true,
                has_segment: false,
                ..entry(1)
            },
        ];
        assert_eq!(select_victim(&view), None);
    }

    #[test]
    fn rule_3_a_later_pad_supersedes_the_holder() {
        let view = [seated(0), entry(1)];
        let reclaim = select_victim(&view).expect("decodebin3 replaced the holder");
        assert_eq!(reclaim.held, 0);
        assert_eq!(reclaim.rule, ReclaimRule::SupersededOrder);
        assert_eq!(reclaim.verdict, SeatVerdict::Replaced);
    }

    #[test]
    fn rule_3_never_moves_the_seat_backwards() {
        // newest > held is the direction decodebin3 replaces in; without it
        // dash evicted both text pads within 10 ms and rendered nothing.
        let view = [entry(0), seated(1)];
        assert_eq!(select_victim(&view), None);
    }

    #[test]
    fn rule_3_refuses_a_latched_out_justifier() {
        // An entry this reclaim already took out cannot justify the next
        // eviction, or the eviction feeds itself.
        let superseded_rival = [
            seated(0),
            TextEntryView {
                superseded: true,
                ..entry(1)
            },
        ];
        assert_eq!(select_victim(&superseded_rival), None);
        let evicted_rival = [
            seated(0),
            TextEntryView {
                evicted_dead: true,
                ..entry(1)
            },
        ];
        assert_eq!(select_victim(&evicted_rival), None);
    }

    #[test]
    fn rule_3_refuses_a_dead_rival() {
        // A rival whose own slot has ended is not a replacement, just a second
        // corpse; it latched the holder superseded 0.5 s before a walk-back.
        let view = [
            seated(0),
            TextEntryView {
                saw_eos: true,
                ..entry(1)
            },
        ];
        assert_eq!(select_victim(&view), None);
    }

    #[test]
    fn rule_4_follows_the_data_backwards() {
        // The walk-back: an EARLIER pad, already condemned, is the one
        // decodebin3 is feeding.
        let view = [
            TextEntryView {
                superseded: true,
                flow: 90,
                ..entry(0)
            },
            TextEntryView {
                flow: 10,
                ..seated(1)
            },
        ];
        let reclaim = select_victim(&view).expect("the seat follows the data");
        assert_eq!(reclaim.held, 1);
        assert_eq!(reclaim.rule, ReclaimRule::FlowTicket);
        assert_eq!(reclaim.verdict, SeatVerdict::Replaced);
        assert_eq!(
            reclaim.revive,
            Some(0),
            "the winner's stale verdicts must not survive into the link loop"
        );
        assert_eq!((reclaim.held_flow, reclaim.winner_flow), (10, 90));
    }

    #[test]
    fn rule_4_only_answers_where_rule_3_cannot() {
        // Rule 3 has no freshness term, so ANY clean later candidate answers
        // first and rule 4 is reached only for a BACKWARD move or against a
        // latched-out forward one. The table below is rule 4's shape with the
        // rival moved after the holder, and rule 3 takes it.
        let view = [
            TextEntryView {
                flow: 10,
                ..seated(0)
            },
            TextEntryView {
                flow: 90,
                ..entry(1)
            },
        ];
        assert_eq!(
            select_victim(&view).expect("something must move").rule,
            ReclaimRule::SupersededOrder
        );
    }

    #[test]
    fn rule_4_cannot_thrash() {
        // The loser carries no data, so it can never satisfy `>` against the
        // pad that does. (The rival sits BEFORE the holder, or rule 3 answers.)
        let view = [
            TextEntryView {
                flow: 10,
                ..entry(0)
            },
            TextEntryView {
                flow: 90,
                ..seated(1)
            },
        ];
        assert_eq!(select_victim(&view), None);
        // An equal ticket is not fresher either.
        let tied = [
            TextEntryView {
                flow: 90,
                ..entry(0)
            },
            TextEntryView {
                flow: 90,
                ..seated(1)
            },
        ];
        assert_eq!(select_victim(&tied), None);
    }

    #[test]
    fn rule_4_waits_for_the_segment() {
        // A ticket stamped before the re-enable's flushing seek, with the
        // stickies gone: seating it means a cue timed off no segment.
        let view = [
            TextEntryView {
                flow: 90,
                has_segment: false,
                ..entry(0)
            },
            TextEntryView {
                flow: 10,
                ..seated(1)
            },
        ];
        assert_eq!(select_victim(&view), None);
    }

    #[test]
    fn rule_4_takes_the_freshest_waiting_pad() {
        // A third pad must not leave the seat on a stale one.
        let view = [
            TextEntryView {
                flow: 20,
                ..entry(0)
            },
            TextEntryView {
                flow: 30,
                ..entry(1)
            },
            TextEntryView {
                flow: 10,
                ..seated(2)
            },
        ];
        let reclaim = select_victim(&view).expect("a fresher pad exists");
        assert_eq!(reclaim.held, 2);
        assert_eq!(reclaim.rule, ReclaimRule::FlowTicket);
        assert_eq!(reclaim.revive, Some(1));
        assert_eq!(reclaim.winner_flow, 30);
    }

    #[test]
    fn the_rule_order_is_load_bearing() {
        // One table satisfying rules 1, 2, 3 and 4 at once. Rule 1 answers,
        // and its verdict is the clearable one.
        let view = [
            TextEntryView {
                saw_eos: true,
                has_segment: false,
                flow: 1,
                ..seated(0)
            },
            TextEntryView {
                flow: 9,
                ..entry(1)
            },
        ];
        let reclaim = select_victim(&view).expect("something must move");
        assert_eq!(reclaim.rule, ReclaimRule::SegmentlessHolder);
        assert_eq!(reclaim.verdict, SeatVerdict::Dead);
        // With the segment back, rule 2 answers before 3 and 4.
        let view = [
            TextEntryView {
                saw_eos: true,
                flow: 1,
                ..seated(0)
            },
            TextEntryView {
                flow: 9,
                ..entry(1)
            },
        ];
        assert_eq!(
            select_victim(&view).expect("something must move").rule,
            ReclaimRule::EndedSlot
        );
        // Alive again, so routed order answers before the flow ticket.
        let view = [
            TextEntryView {
                flow: 1,
                ..seated(0)
            },
            TextEntryView {
                flow: 9,
                ..entry(1)
            },
        ];
        assert_eq!(
            select_victim(&view).expect("something must move").rule,
            ReclaimRule::SupersededOrder
        );
    }

    // ---- the admission cascade -------------------------------------------

    fn admit_with(entry: &TextEntryView, contest: SeatContest, unwirable: bool) -> Admission {
        admit(entry, &contest, || unwirable)
    }

    #[test]
    fn admission_table() {
        let idle = SeatContest::default();
        let rival = SeatContest {
            live_rival_waiting: true,
            consumer_branch_live: false,
        };
        let busy = SeatContest {
            live_rival_waiting: false,
            consumer_branch_live: true,
        };

        // Arm 0: nothing refuses a healthy candidate.
        let admission = admit_with(&entry(0), idle, false);
        assert_eq!(admission.refusal, None);
        assert!(!admission.revived);

        // Arm 1: the stomp guard's other half. A foreign or id-less pad never
        // takes the seat.
        assert_eq!(
            admit_with(
                &TextEntryView {
                    same_sid: false,
                    ..entry(0)
                },
                idle,
                false
            )
            .refusal,
            Some(Refusal::SidNotAllowed)
        );

        // Arm 2: a replaced pad never competes again.
        assert_eq!(
            admit_with(
                &TextEntryView {
                    superseded: true,
                    ..entry(0)
                },
                idle,
                false
            )
            .refusal,
            Some(Refusal::Superseded)
        );

        // Arm 3: an ended slot, but ONLY while a live rival waits.
        let ended = TextEntryView {
            saw_eos: true,
            ..entry(0)
        };
        assert_eq!(
            admit_with(&ended, rival, false).refusal,
            Some(Refusal::SlotEnded)
        );
        assert_eq!(
            admit_with(&ended, idle, false).refusal,
            None,
            "with no rival the track keeps its branch and its end-of-item EOS"
        );

        // Arm 3 BEFORE arm 4: an EOSed pad keeps the segment it had, so the
        // revive would hand the seat straight back to the pad the reclaim
        // just took it from.
        let ended_and_evicted = TextEntryView {
            saw_eos: true,
            evicted_dead: true,
            ..entry(0)
        };
        let admission = admit_with(&ended_and_evicted, rival, false);
        assert_eq!(admission.refusal, Some(Refusal::SlotEnded));
        assert!(!admission.revived, "the EOS arm must not revive anything");

        // Arm 4: evicted and segmentless is refused; evicted with a segment is
        // proof of life.
        assert_eq!(
            admit_with(
                &TextEntryView {
                    evicted_dead: true,
                    has_segment: false,
                    ..entry(0)
                },
                idle,
                false
            )
            .refusal,
            Some(Refusal::EvictedSegmentless)
        );
        let admission = admit_with(
            &TextEntryView {
                evicted_dead: true,
                ..entry(0)
            },
            idle,
            false,
        );
        assert_eq!(admission.refusal, None);
        assert!(admission.revived);

        // Arm 5: the one-live-branch rule.
        assert_eq!(
            admit_with(&entry(0), busy, false).refusal,
            Some(Refusal::ConsumerBusy)
        );

        // Arm 6: a link GStreamer already refused is not retried.
        assert_eq!(
            admit_with(&entry(0), idle, true).refusal,
            Some(Refusal::Unwirable)
        );
        // ... and it is outranked by the busy consumer, which is the cheaper
        // question.
        assert_eq!(
            admit_with(&entry(0), busy, true).refusal,
            Some(Refusal::ConsumerBusy)
        );
    }

    #[test]
    fn a_revived_candidate_refused_later_still_clears_its_latch() {
        // The clear is what lets it compete on the NEXT poll, when the branch
        // ahead of it may be gone.
        let admission = admit_with(
            &TextEntryView {
                evicted_dead: true,
                ..entry(0)
            },
            SeatContest {
                live_rival_waiting: false,
                consumer_branch_live: true,
            },
            false,
        );
        assert_eq!(admission.refusal, Some(Refusal::ConsumerBusy));
        assert!(admission.revived);
    }

    #[test]
    fn the_unwirable_question_is_only_asked_last() {
        // It costs a lock and a key allocation, and the arms above filter
        // almost every candidate.
        let mut asked = false;
        admit(
            &TextEntryView {
                superseded: true,
                ..entry(0)
            },
            &SeatContest::default(),
            || {
                asked = true;
                true
            },
        );
        assert!(!asked);
    }

    #[test]
    fn refusals_render_without_allocating_until_formatted() {
        // The variant is a discriminant; this is the text a field log shows.
        assert_eq!(
            Refusal::Superseded.to_string(),
            "decodebin3 replaced this pad for the same stream"
        );
        assert_eq!(Refusal::SlotEnded.to_string(), "its decodebin3 slot ended");
        assert_eq!(
            Refusal::EvictedSegmentless.to_string(),
            "seat-evicted and still segmentless"
        );
        assert_eq!(
            Refusal::ConsumerBusy.to_string(),
            "another text branch already feeds the consumer"
        );
        assert_eq!(
            Refusal::Unwirable.to_string(),
            "its branch could not be wired under this load"
        );
    }
}
