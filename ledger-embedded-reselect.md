# Ledger — re-selecting the embedded DASH text track shows nothing

Base: `313a8663` ("fcastplaybin: the park's removal latched the slot the heal
then emptied"). Field report: main checkout's `selecting-embedded-doesnt-show.txt`.

## What shipped

* `crates/fcastplaybin/tests/dash_testbed.rs`
  * `dash_embedded_text_rejoins_after_a_round_trip_through_an_external` — a
    reproduction of the field gesture, RED on `313a8663`, `#[ignore]`d because
    no repair landed.
  * `Harness::select_subtitle` / `Harness::confirmations_of` — a subtitle
    switch that WAITS for the crate's own confirmation. Without it a leg is
    skipped silently and the cue wait after it is satisfied by the previous
    track's stragglers (an external replays its whole file, so there are
    dozens). Three of my own earlier red runs were that artefact and not the
    defect; every alternating test in this file rests on the switch under it
    having actually happened.

No `src/` change. See "Why no fix landed".

## The mechanism

Evidence: `GST_DEBUG=decodebin3:6` over the reproduction, plus
`target/patched-playback/src/gst/playback/gstdecodebin3.c`.

An adaptive input answers SELECTABLE, so `dbin->upstream_handles_selection`
is 1 on every decodebin3 sink pad ("Upstream is selectable : 1"). Two
consequences, both from the source:

1. `handle_stream_switch()` — the function that assigns outputs to slots — is
   never called at all: gstdecodebin3.c:3663 guards it with
   `if (!dbin->upstream_handles_selection && collection == dbin->output_collection)`.
2. The only remaining path that attaches an output to a slot is
   `mq_slot_check_reconfiguration()`, and `multiqueue_src_probe()` calls it on
   **`GST_EVENT_CAPS` and nothing else** (gstdecodebin3.c:3690-3694).

So decodebin3 never sees the crate's SELECT_STREAMS (it is forwarded upstream;
the whole 2309-line decodebin3 trace contains no `select_streams` line). The
demuxer answers a re-select by exposing a FRESH pad, the crate links it into a
fresh decodebin3 sink (`linked input pad into decodebin3 src=src_3 sink=sink_4`),
and decodebin3 then does, in one breath at `0:00:05.350`:

```
gstdecodebin3.c:3766:create_new_slot:      Creating new slot for type text
gstdecodebin3.c:3796:create_new_slot:      Created new slot 4 (0x7fff9801be90) (multiqueue1:src_4)
gstdecodebin3.c:1208:remove_input_stream:  Removing input stream ... ID <sid>/text-2
gstdecodebin3.c:1232:remove_input_stream:  slot 0x7fff98013f10 cleared          <- slot 2
gstdecodebin3.c:1313:...unblock_streams:   <multiqueue1:sink_2> Sending EOS to unused slot
```

Slot 2 is the slot output pad `text_0` is ghosted onto. From that instant
`text_0` can never carry another buffer. Slot 4 has the stream and **no output
pad**, and will only get one if a CAPS event crosses it.

Every event that crossed `multiqueue1:src_4` in the failing capture:

```
0:00:05.350589  Got event stream-start
0:00:05.350617  Got event segment
0:00:05.350623  Got event stream-collection
0:00:07.416527  Got event eos
```

No CAPS, no buffer — so `mq_slot_check_reconfiguration` never ran for slot 4
and no output pad was ever built. The last `db_output_stream_reconfigure` for
a text output in the whole run is `text_1 -> multiqueue1:src_3`, seconds
earlier.

Why the demuxer pushed nothing, from the crate's own timeline (wall clock,
GST clock = wall − 4.700):

| wall | event |
|---|---|
| 09.9899 | `sent SELECT_STREAMS seqnum=946` — embedded ON |
| 10.0497 | `linked input pad into decodebin3 src=src_3 sink=sink_4` — the demuxer answers, and slot 2 is cleared + EOSed |
| 10.0605 | `sent SELECT_STREAMS seqnum=1035` — embedded OFF, **11 ms later** |
| 10.7618 | `sent SELECT_STREAMS seqnum=1115` — embedded ON again |
| 10.7619 | `text stream joined its consumer tail pad=text_0 segment=true` — the seat goes onto the dead slot |
| 12.1161 | `input stream drained (EOS into decodebin3) sid=…/text-2`; `gst_decodebin3_input_pad_unlink:<sink_4>` |
| — | nothing, ever |

The deselect at 10.0605 lands while the demuxer is still bringing the pad it
built at 10.0497 up; it drains that pad, and the re-select 0.7 s later is
swallowed while the track is draining. The crate then joins `text_0`, reports
success, and `consumer_branch_live` (D8) refuses every other candidate for the
life of the selection.

One second of dwell per leg (`advance_past(video_buffers() + FPS)` after each
cue wait) turns the reproduction GREEN, which localises the loss exactly: it is
the deselect arriving inside the previous re-select's pad bring-up.

## The four questions

1. **Are the fresh pads routed into our routing table when they appear?**
   In the reproduction there is no fresh OUTPUT pad to route — decodebin3
   builds a slot with no output (above). When decodebin3 does build one, the
   crate routes it and logs `routed decodebin3 pad pad=text_N kind=Text`
   (routing.rs:1768) and `route_db3_pad`'s tail requests a
   `Job::PollTextPolicy` for every Text pad (routing.rs:1853-1864), so the
   corrective poll does happen. The field log's missing "routed" line for
   `text_4` is a log-tail artefact: the capture starts at t=86.77 and `text_4`
   predates it.

2. **Why doesn't the seat reclaim move the seat off the EOS-dead pad?**
   Two independent reasons, and the second is the one that bites.
   * The dead pad keeps its SEGMENT sticky, so the segmentless-holder rule does
     not see it as dead; `superseded` and the `last_buffer` flow reclaim both
     need a same-sid COMPETITOR entry, and in the reproduction there is none.
   * Every reclaim lives inside `poll_text_policy`, which only runs when
     something queues `Job::PollTextPolicy`. In the TEST harness the receiver
     pumps every 10 ms so a poll always follows; **in the field it does not** —
     the field capture's polls are at 86.772, 87.827, 88.116, 88.617, 95.151,
     96.717, 102.172, 107.803, 112.383, i.e. one per event and none at all in
     the 6.5 s after the join that seated the dead pad. So in the field a join
     that seats the wrong pad is never re-examined.
   `saw_eos` taps exist on INPUT pads only (`Inner::input_eos_sids`,
   routing.rs:476-508). There is **no** EOS signal on a routed decodebin3
   OUTPUT pad, so the policy cannot currently tell a dead output from a live
   one. That is the missing instrument.

3. **What does decodebin3 do on re-select — reuse or add?** Neither, in
   upstream-selection mode: it builds a NEW SLOT for the re-added input,
   CLEARS and EOSes the old slot, and leaves the old output pad pointed at the
   dead slot. An output is attached to the new slot only if a CAPS event
   crosses it. And on the unconfirmed `seqnum 4352`: decodebin3 handles no
   SELECT_STREAMS at all in this mode (`upstream_handles_selection`), so a
   missing confirmation is expected, not a swallow — `dispatch.rs` arm (5)
   already says exactly this and confirms locally. **C-family observation: no
   bug there.**

4. **The −21.9 s `SyncTextRunningTime` alignment.** Secondary, as predicted. It
   is `Inner::sync_text_running_time` doing its job on a branch that then never
   carries a buffer; a bogus base would mistime cues, not remove them, and the
   reproduction shows zero cues with a correct-looking segment
   (`joined … segment=true`).

## Why no fix landed

Both repairs the evidence points at are refused for stated reasons:

* **Refusing the dead seat** (an EOS tap on routed text outputs beside the
  existing `last_buffer` BUFFER probe, feeding the `evicted_dead` /
  `superseded` machinery) is correct, is a pure candidate-refusal tightening,
  and is what the FIELD capture needs — there a live replacement output
  (`text_4`) sat routed beside the dead `text_0` and was refused by D8 with the
  reclaim unable to run. It does **not** repaint the reproduction, where
  decodebin3 built no replacement output at all: refusing `text_0` there just
  turns a silent wrong seat into a loud empty one.
* **Re-asserting the selection** is explicitly forbidden: `dispatch.rs` arm (5)
  — "Re-sending a SELECT_STREAMS at an adaptive demuxer mid-drain is the
  `g_assert(track->draining && !track->selected)` abort".

What is left is either holding a re-select of a main-input text stream until
its previous drain has landed (the crate already observes that edge — "input
stream drained (EOS into decodebin3) sid=…"), which is a dispatch-side change
with its own deadlock surface, or a targeted flushing seek. Neither is a
refusal tightening and neither should be guessed at under a bite test that
cannot distinguish them. The reproduction and the mechanism are the deliverable;
the repair wants its own pass.

### The next pass, in order

1. **A poll after a seat.** Every reclaim lives in `poll_text_policy`, and
   nothing queues a poll on "a text branch was just seated". Requesting one
   coalesced follow-up poll after the link loop seats a branch would let the
   `superseded` reclaim re-examine the seat, and against the field capture
   (`text_0` at routed index 0 held, `text_4` later and waiting, so
   `held < newest`) that alone moves the seat onto the live pad and the track
   renders. Cheapest candidate, and the one the field capture actually needs.
2. **The EOS tap on routed text outputs** (`saw_eos` beside `last_buffer`,
   set by an EVENT_DOWNSTREAM probe, cleared by FLUSH_STOP and STREAM_START
   exactly as the input-side tap does), feeding a refusal in the link loop and
   a new victim rule in the reclaim. Makes the seat decision correct rather
   than merely re-examined, and gives "no text branch joined … refusals=[…: its
   decodebin3 slot ended]" instead of a silent wrong seat.
3. Only then the dispatch-side drain interlock, which is what this
   reproduction needs and what nothing above fixes.

Benching (1) and (2) needs a harness knob that stops `settle_pump` from
calling `poll_text_policy` on a timer — the field's receiver polls on events
only, and that difference is the whole reason the field never healed.

## Bite results

`dash_embedded_text_rejoins_after_a_round_trip_through_an_external`, RED on
`313a8663`, four runs, four failures, always on an embedded re-select and never
on an external one:

```
no EMB cue reached the renderer (the embedded track after round trip 0 through an external);
  text tail peers ["src"]; video buffers 603; vtt fetches: embedded=2 a=3 b=0
no EMB cue reached the renderer (the embedded track after round trip 1 through an external);
  text tail peers ["src"]; video buffers 707; vtt fetches: embedded=2 a=4 b=0
```

`embedded=2` is the demuxer having re-fetched the whole-period VTT, so this is
not "nobody re-requested the track": it is fetched and then lost between the
demuxer and the renderer, which is the field's sentence exactly.

Same test with one second of dwell per leg: `ok. 1 passed` in 35.22 s.

### Honest scoping of the bite

The reproduction's trigger window is TIGHTER than the field's. Here the
deselect lands 11 ms after the demuxer exposes the pad; the field's gaps
between selections are 0.54 s, 0.9 s and 1.3 s. So this is a reproduction of
the same LOSS (a re-select swallowed mid-drain, a seat on a slot decodebin3 has
cleared and EOSed, no cue for the rest of the item) reached through a tighter
race than the owner hit. It is a real defect on its own terms — a fast
double-switch kills the embedded track permanently — and its crate-visible
signature is identical to the field's, which is what makes it useful. It is
NOT proof that the owner's 0.5-1.3 s gesture takes the same path, and it must
not be cited as such.

What the field capture has and this one does not is a LIVE replacement output
(`text_4`) sitting routed beside the dead `text_0`. Benching that shape needs
the corrective poll suppressed, because the test harness's receiver pumps
`poll_text_policy` every 10 ms and the existing `superseded` reclaim then heals
the wrong seat within one tick — see question 2.

## Suites

`dash_testbed`, serial (`--test-threads=1`):

```
test result: ok. 16 passed; 0 failed; 6 ignored; 0 measured; 0 filtered out; finished in 509.13s
```

Six ignored, not the file's usual five: the five legitimate ones plus the new
reproduction.

The other guard suites (`sink_subtitles`, `regression_text_reconcile`,
`external_subtitle_lifecycle`, `caller_bounded_switch`,
`multiqueue_slot_unlatch`) were NOT run, and the reason is that they cannot
have moved: this commit changes no `src/` file and no file any of them
compiles. `crates/fcastplaybin/tests/dash_testbed.rs` is the only edit, the
diff is 100% additions, and the two new `Harness` methods are private to that
file and called only from the ignored test. Stated rather than assumed, since
the discipline asks for numbers and there are none to give here.

## Field probe

Untouched. `probe_first_cue_latency_on_a_whole_period_track` and its
`FCAST_PROBE_MANIFEST` / `FCAST_PROBE_VTT` / `FCAST_PROBE_VTT_DELAY_MS` reads
(dash_testbed.rs:1307-1325) are not on any path this commit edits.

---

# Pass 2 — the repair

Base: `cf7e33c6` (the record above). The reproduction is GREEN and un-ignored.
What makes it green is NOT what pass 1 predicted, and the field capture's own
shape turns out to sit behind an UPSTREAM decodebin3 defect. Both are evidenced
below.

## The finding that reorders the whole thing

Pass 1 stopped at "the fresh slot has the stream and no output because no CAPS
crossed it", and proposed a C12-style rescue: store the slot SINK's caps onto
its SRC pad. That is a dead end, and the census this pass shipped
(`Inner::adopt_outputless_text_slot`, which enumerates decodebin3's multiqueue
directly instead of walking in from a routed pad) is what says so — the sink
has no caps either:

```
src_4[caps=None sticky=StreamStart+Segment+StreamCollection eos=false
      active=true flow=Err(NotLinked) linked=false]
  <- sink_4[caps=None sticky=StreamStart+Segment+StreamCollection sid=…/text-2]
```

Nothing was destroyed inside decodebin3. **Nothing ever arrived.** C12's family
is the wrong family: this is a SEND-side defect, and the missing output is a
consequence of it rather than a cause.

The crate's own log on the failing reproduction says what the send-side defect
is:

```
27.498941  sent SELECT_STREAMS [video, audio, text-2]   embedded ON
27.556889  linked input pad into decodebin3 sink_4      the demuxer answers
27.560281  sent SELECT_STREAMS [video, audio]           embedded OFF, 3.4 ms later
28.263871  sent SELECT_STREAMS [video, audio, text-2]   embedded ON again
29.623236  input stream drained (EOS into decodebin3) sid=…/text-2
           (the drain of the pad exposed at 27.556889 — 2.07 s long)
 —         nothing, ever
```

The re-select at 28.263871 lands 1.36 s BEFORE the drain it races finishes, and
the demuxer swallows it. Pass 1 saw that drain and read it as the cause of a
SEAT problem; it is the cause of a DISPATCH problem, and the seat is downstream
of it.

## What shipped, per rung

### Rung 3 — the drain interlock (`Inner::await_text_input_drain`, dispatch.rs)

THE repair. A text re-select is held on the select lane until the demuxer has
finished draining the pad it exposed for that stream last time. Three
conditions, all pad state rather than crate memory: the send RE-ADDS the stream
(it is in this event and not in `last_upstream_ids`); a decodebin3 SINK pad of
the MAIN input still carries that stream id; that pad has no sticky EOS.

Waiting rather than re-sending is what keeps it clear of `dispatch.rs` arm (5)
— the event still goes out exactly ONCE, after the state that would have
swallowed it (and that `g_assert(track->draining && !track->selected)` guards)
is over. The select lane is the one thread in the crate where blocking is
allowed, it holds no crate lock across the wait, and the drain it waits for is
the demuxer's output loop pushing into an unlinked decodebin3 slot — a path
that touches nothing the crate holds, so it cannot be waiting on us.

Bounded at 5 s (`TEXT_DRAIN_INTERLOCK_BUDGET`) against a measured 2.07 s worst
case; past the bound the event goes out exactly as before, loudly.
Lever: `FCAST_NO_TEXT_RESELECT_DRAIN_INTERLOCK`.
Counters: `text_drain_interlocks()`, `text_drain_interlock_timeouts()`.

Observed on a green run: ONE interlock, 1.364 s of wait (hold at `08.598713`,
release at `09.962382`), 0 timeouts.

### Rung 2 — the EOS tap on routed text OUTPUTS (`RoutedStream::saw_eos`)

The instrument pass 1 named as missing. An EVENT_DOWNSTREAM probe on each
routed TEXT output pad records EOS and clears on STREAM_START or FLUSH_STOP —
proof of life, never a latch, so C11's walk-back (decodebin3 re-pointing an
existing ghost at a live slot, with no pad-added) clears it by itself. It feeds
two rules, both scoped to "…while a LIVE same-sid rival is waiting", so an
end-of-item EOS is never a trigger:

* a new victim rule in the seat reclaim (the dead holder loses the seat), and
* a refusal in the link loop, placed BEFORE the `evicted_dead` arm because that
  arm clears its verdict for any pad carrying a segment and an EOSed pad keeps
  the segment it had.

`superseded` is deliberately NOT set on the victim: that is the verdict for a
pad decodebin3 REPLACED, and this one may yet be re-pointed at a live slot.
Levers: `FCAST_NO_TEXT_OUTPUT_EOS_TAP` (the instrument),
`FCAST_NO_TEXT_EOS_SEAT_RECLAIM` (both rules, which are one rule).
Counter: `text_eos_seat_reclaims()`.

### Rung 1 — two polls that did not exist

* **after a seat** (tail of `poll_text_policy`): the link loop seating a branch
  asked nobody to look again. The field capture seats a dead `text_0` with a
  live `text_4` routed beside it and then has NO POLL AT ALL for 6.5 s.
  Lever `FCAST_NO_TEXT_SEAT_FOLLOWUP_POLL`, counter
  `text_seat_followup_polls()`.
* **on a fresh decodebin3 INPUT pad** (`link_input_pad`): this is the demuxer
  ANSWERING a selection, and the answer can be slow — measured at 2.32 s after
  the send on the field-paced gesture. `route_db3_pad` only asks when an OUTPUT
  pad appears, and the whole defect is the case where decodebin3 builds none.
  Lever `FCAST_NO_INPUT_PAD_TEXT_POLL`.

### The census (`Inner::adopt_outputless_text_slot`, flush.rs)

Enumerates decodebin3's multiqueue by factory, finds slots that carry the
SELECTED text stream and that no decodebin3 output ghosts, and describes them —
one line per (stream, load), after a 1 s grace so the healthy transient is not
reported. Where the slot SINK still holds a caps its src has lost, it also
stores it back: the C12 rescue one step further out, which the shipped one
cannot reach because its preconditions walk from a routed OUTPUT pad and this
slot has none. Lever `FCAST_NO_OUTPUTLESS_TEXT_SLOT_ADOPT`, counters
`outputless_text_slots()` / `outputless_text_slot_adoptions()`.

This is the instrument that answered pass 1's open question, and it is what a
field capture now has instead of silence.

## The repair that was TRIED and REFUSED, with numbers

The field-paced gesture has a second cause underneath it. A whole-period text
Representation is ONE segment covering the period, so once it has been
downloaded and pushed the demuxer has NOTHING LEFT TO SEND and a re-select gets
an empty track — captured as stream-start, segment and stream-collection across
the fresh slot and then no caps and no buffer for forty seconds while the track
is selected. Only a flushing seek re-downloads it, which is exactly why the
field report says the subtitles "appear after you seek", and
`SelectionEngine::pump` cancels that seek whenever an external subtitle is
attached ("any flush races the external inputs' reconfiguration and can freeze
the item").

An item-CONFINED version was built and measured: the same flushing seek sent to
the MAIN INPUT ELEMENT only, so an external's pads never see it, issued on the
select lane immediately after the `SELECT_STREAMS` so the ordering is
structural. It works at the layer it aims at — the demuxer re-downloads
(`embedded=3`, then `4`, VTT fetches) and the CAPS does cross the fresh slot —
and it is WRONG:

```
dash_embedded_text_rejoins_after_a_round_trip_through_an_external
  with the item re-emit:  0 pass / 8 fail, video flat at 76 buffers (~5 s),
                          text tail peers [] — the item is frozen
  without it:             8 pass / 0 fail
```

That is the engine's own refusal measured a second time from the other side.
Confining a seek to one ELEMENT does not confine its FLUSH: it still travels
decodebin3 into sinks the external shares while the external's own pads keep
their pre-flush stickies. Reverted whole. The reasoning is left at the site in
`flush.rs` so the next reader does not spend the same day on it.

## The third shape, which is UPSTREAM

With that re-emit in place long enough to capture it, `GST_DEBUG=decodebin3:6`
shows what decodebin3 does when the CAPS finally crosses the fresh slot while a
DESELECTED external's output has just been freed:

```
mq_slot_get_or_create_output:<multiqueue1:src_4> Reassigning to output …:text_1
mq_slot_reassign:<multiqueue1:src_3> Unlinking from previous output
mq_slot_reassign:<multiqueue1:src_3> Attempting to re-assing output stream
mq_slot_reassign:<multiqueue1:src_3> No target slot, removing output
db_output_stream_free:<fpb-decodebin:text_1> Freeing
```

It picks the external's just-freed output to recycle for the new slot, and the
reassign meant to detach that output from its OLD slot destroys it instead,
because the old slot's own stream has nowhere to go. The slot it was chosen for
keeps the stream and the caps and never gets an output, and decodebin3 never
revisits the decision. No crate-side rule can see it: they all reason about
routed pads and there is no pad.

`dash_embedded_text_rejoins_at_field_pace_with_no_timer_poll` pins this and is
`#[ignore]`d for it. It is red with the repairs, without them, and with this
suite's timer poll left ON (`embedded=4` fetches and still no cue), so it is
neither poll starvation nor a seat the crate holds wrongly.

## Per-shape coverage, stated honestly

| shape | covered? | by what |
|---|---|---|
| the REPRODUCTION (legs back to back, deselect ~3 ms after the pad appears, re-select swallowed mid-drain) | YES | the drain interlock, 8/8 |
| the FIELD capture's shape (live `text_4` routed beside a dead seated `text_0`, no poll for 6.5 s) | PARTIALLY, unbenched | the EOS tap + reclaim make the dead pad lose the seat to the live one, and the two new polls give the reclaim an occasion to run in a receiver that polls on events. NOT reproduced in a test |
| the THIRD shape (whole-period track with nothing left to send; decodebin3 destroys the output it chose to recycle) | NO | upstream; named in the log by the census, pinned by an ignored test |

The honest gap: the field capture's shape needs decodebin3 to have BUILT the
replacement output, and neither the reproduction (no output at all) nor the
field-paced test (output built, then destroyed) reaches that state on this
fixture. So rung 2 ships as a candidate-refusal tightening argued from the
field capture and from the decodebin3 source, with its lever, and it did NOT
bite in any run measured here. It is not claimed as proven.

## Bite results

`dash_embedded_text_rejoins_after_a_round_trip_through_an_external`, legs back
to back with no dwell, so the race is run on every execution:

```
default arm                                      8 pass / 0 fail
FCAST_NO_TEXT_RESELECT_DRAIN_INTERLOCK=1         0 pass / 4 fail
FCAST_NO_TEXT_SEAT_FOLLOWUP_POLL=1               3 pass / 0 fail
FCAST_NO_TEXT_EOS_SEAT_RECLAIM=1                 3 pass / 0 fail
FCAST_NO_INPUT_PAD_TEXT_POLL=1                   3 pass / 0 fail
```

Every failure under the interlock lever is identical to pass 1's RED signature:
`no EMB cue reached the renderer (… after round trip 1 …); text tail peers
["src"]; video buffers 698`. So the interlock is the rung that repaints it and
the other three are not — stated rather than implied, because a green test that
credits the wrong change is worse than a red one.

## The harness knob

`Harness::poll_on_events_only()` stops `settle_pump` from calling
`poll_text_policy` on a timer, which is the single biggest difference between
this suite and the shipped receiver: every wait in this file re-drives the link
policy every 10 ms, and the field's receiver polls when something happens. A
test that means to reproduce a FIELD shape has to turn it off, or the harness
repairs the defect before the crate can be asked to. `pump_selection` still
runs — the engine's dispatch is a different channel and the receiver drives it
on every pump too.

## Suites

Serial (`--test-threads=1`), all zero failures:

```
dash_testbed                 17 passed;  0 failed;  6 ignored;  543.71s
sink_subtitles               19 passed;  0 failed;  0 ignored;   18.68s
regression_text_reconcile    14 passed;  0 failed;  0 ignored;   16.96s
external_subtitle_lifecycle  20 passed;  0 failed;  0 ignored;    8.43s
caller_bounded_switch         1 passed;  0 failed;  0 ignored;   12.07s
multiqueue_slot_unlatch      10 passed;  0 failed;  0 ignored;    2.20s
segment_sticky_census         5 passed;  0 failed;  0 ignored;    6.25s
flush_census                  3 passed;  0 failed;  0 ignored;    0.22s
```

`dash_testbed`'s 17 passed is pass 1's 16 plus the un-ignored reproduction, and
its 6 ignored is pass 1's 6 minus that reproduction plus the new field-paced
one.

REPORTED IN FULL, including the run that was not clean: the FIRST `dash_testbed`
pass came out `16 passed; 1 failed; 6 ignored`, on
`dash_segmented_embedded_text_shows_on_a_first_mid_play_select` —
`a SEG cue rendered before the track was ever selected … left: 5 right: 0`,
i.e. the auto-selected track delivered cues before the test's explicit
subtitle-off landed. That test then passed 4/4 in isolation and the whole suite
passed on the re-run above, which puts it in the known dash flake band rather
than on this commit. The mechanism agrees: the only change that could
plausibly accelerate a FIRST text link is the input-pad poll, and at
`link_input_pad` time decodebin3 has exposed no output pads at all, so that
poll finds no text entry to link — the first link still waits for
`route_db3_pad`, which has always asked for its own poll.

## Field probe

Correct. `probe_first_cue_latency_on_a_whole_period_track` with the field VTT
spliced into the fixture tree as `vod/field.vtt` and the fixture's own
`manifest-text.mpd` sed'd to point at it (`vod/field-mirror.mpd`),
`FCAST_PROBE_VTT_DELAY_MS` unset:

```
PROBE frames with a cue up: 126/147 (first covered frame at Some("0.228"));
      consumer clears=0
PROBE video=148 frames (= 9.87 s of media) over 10.01 s of wall clock;
      slot unlatches=0 joins into an inactive branch=0 parked cues replayed=1
```

First covered frame 0.228 s against the 0.26 s this was left at, i.e. inside
the run-to-run band, with the same zero unlatches and zero inactive-branch
joins. The drain interlock is not on this path at all: nothing here re-selects,
so no send is ever held.
