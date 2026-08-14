# Bitmap subtitle decoder fuzzing

`cargo-fuzz` targets for the `subpic` decoders — the decoder-level instrument
under the pipeline-level drivers (`fcastplaybin`'s `fuzz_scenarios` /
`fuzz_buffering`), which fuzz the driver and never reach these parsers with
anything but well-formed bytes.

These decoders parse **untrusted stream bytes** and every one of the three
formats has a field that says "and now allocate this much". The phase-6 contract
(P14) is that malformed input is a counted reset and never a panic, and that a
decoder holds at most 32 MiB — both are claims about ALL inputs, which is what a
fuzzer is for and what a hand-written vector cannot be.

## Targets

| target | input | reaches |
|---|---|---|
| `pgs_decode` | raw bytes, chunked into 64-byte packets | the framing, the carry across packets, and whatever gets past them |
| `pgs_decode_structured` | fuzzed segments in valid framing | the per-segment parsers, the RLE expander, the geometry |
| `vobsub_decode` | a fuzzed `.idx` and fuzzed subpicture units | the out-of-band palette parser as well as the packet |
| `dvb_decode` | fuzzed segments in valid framing | the persistent regions, the three RLE depths, the accounting |

VOBSUB (step 5) and DVB (step 6) add `vobsub_decode` and `dvb_decode` beside
them. **A new target is one file**: `fuzz_targets/<name>.rs` builds its format's
bytes and calls `push_and_check` / `assert_recovers` from `src/lib.rs`, which is
where the invariants live so that they cannot drift per format. VOBSUB's will
want a `codec_data` dimension (its palette is out of band); DVB's persistent
region buffers make the budget invariant the marquee check there.

## Invariants (in `src/lib.rs`, asserted for every input)

1. **No panic** — the libFuzzer default, and the weakest of the four.
2. **The allocation cap holds** after every packet: `held_bytes() <=
   ALLOCATION_BUDGET`, AND `allocated_bytes() <= held_bytes()` — what the
   decoder has taken from the allocator against what it believes it holds. The
   second half is adversarial review 2's: a cap enforced on a number that is not
   the memory is not a cap, and both defects it found were invisible to the
   first half alone.
3. **Regions are internally consistent**: `pixels.len() == width * height * 4`,
   non-zero dimensions, and a rect CONTAINED in the picture — origin and far
   edge both, which is what "lands on the video" has to mean and did not until
   adversarial review 2 said so. A region whose buffer does not match its
   dimensions is a read past the end of an allocation in the RENDERER, which is
   worse than anything inside a decoder.
4. **A reset leaves the decoder usable**: a known-good input is fed at the end of
   every run and must still decode. A decoder that wedges after garbage passes a
   no-panic fuzzer forever while showing no subtitles at all.

   Two things about that tail, both from adversarial review 3. **The expected
   update count is per TARGET, not shared**: PGS and VOBSUB must answer with
   exactly one update, and only DVB may answer with two (it force-terminates
   whatever set was open when a packet arrives at a new time). A shared helper
   that tolerated extras stopped noticing when they grew. And **DVB runs two
   tails**: first a display set that does NOT reset anything, so the state the
   fuzzer left is still in place while it decodes — the only way a
   state-poisoning defect is observable — and then an ACQUISITION set (its own
   display grid, a mode-change page) that must draw. With only the resetting
   tail, two of the three checked-in DVB seeds had quietly stopped reproducing
   their own defects.

   **Both tails must DRAW.** The non-resetting one could not be held to a
   picture while it used the minimal display set, because the fuzzer may have
   left a display definition that legitimately puts that picture off screen —
   so it asks only for the accounting and the region contract, which a decoder
   wedged into drawing nothing satisfies perfectly. It now uses
   `grounded_display_set`, which states its own grid (and whose page state is 1,
   an acquisition POINT, so it still resets nothing), and one region is
   required. Checked as the LAST update, for the force-terminate reason above.

## Running

```sh
eval "$(cargo xtask patched-plugins --quiet)"          # from the repo root
cd crates/fcast-video/fuzz

cargo fuzz build                                        # all targets
cargo fuzz run pgs_decode corpus/pgs_decode seeds/pgs_decode -- -runs=0
cargo fuzz run pgs_decode corpus/pgs_decode seeds/pgs_decode -- -max_total_time=120

# DVB wants a bigger input than libFuzzer's default: one region segment can
# carry ten thousand object placements, and that is the shape its worst
# allocation case has. Without this the target cannot build the input at all.
cargo fuzz run dvb_decode corpus/dvb_decode seeds/dvb_decode -- \
    -max_total_time=120 -max_len=65536
```

**Record `cov:` and `ft:` with the run counts.** They are how a later campaign
knows whether it explored anything new or just re-ran the corpus, and a target
that stops reaching a parser shows up there before it shows up as a missed
defect.

The per-step gate is that pair: a **replay** of the seeds and the corpus
(`-runs=0`) plus a **120-second fresh run** per target, zero crashes and an empty
`artifacts/`. Longer campaigns belong to the phase exit gate.

`corpus/` and `artifacts/` are gitignored — a corpus is earned, not reviewed.
`seeds/` is CHECKED IN: the hand-crafted vectors from the decoder's own fixtures
(regenerate with `cargo run --bin seed_corpus`) plus any input a campaign found
interesting enough to keep as a regression. The generator only writes the files
it knows about, so a kept regression is never deleted by it.

`seeds/pgs_decode/regression-budget-overrun.bin` is one of those: the first
120-second campaign found an object store charged to exactly the budget followed
by a packet ending mid-segment, which left the decoder holding 22 bytes more
than the cap allowed. Fixed by reserving the carry's ceiling out of the budget
(`CARRY_RESERVE` in `pgs.rs`).

**What the cap invariant can and cannot see.** It reads the decoder's own
accounting (`held_bytes`), so it is exactly as good as that accounting is
honest. Adversarial review 2 found two allocations it was blind to — a `Vec`
whose CAPACITY outlived the bytes it once held, and an object store charged its
declared length while its buffer doubled behind it — neither of which any input
could have exposed through a length-based count. Both are now counted by
capacity. A reviewer with a counting allocator is still the instrument that
finds the next one; this invariant is what keeps it fixed.
