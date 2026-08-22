//! The routing dispositions [`crate::Inner::route_db3_pad`] decides before it
//! touches the graph, separated from the surgery that acts on them.

/// What [`crate::Inner::route_db3_pad`] does with a decodebin3 VIDEO output
/// pad. Audio has no equivalent: only video owns a chain that can be
/// deactivated under it, and only video is re-routed for a stream whose end
/// is already spent.
///
/// Both parks are the same mechanism ([`crate::Inner::park_stream`]), and the
/// variants stay two because the hazards they answer are two and each is a
/// separate measured wedge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoRoute {
    /// A video pad appearing while the crate's last dispatched selection
    /// turned video OFF is decodebin3's collection-default auto-select
    /// resurrecting the deselected stream (an attach makes it re-select over
    /// the applied state). Rebuilding the chain for it drops the pipeline
    /// into an async re-preroll that the deselected stream may never feed
    /// again, and that unfinished transition holds the receiver's pump gate
    /// closed forever, so the engine's corrective re-assert can never run
    /// (fuzz_buffering seed 300002, campaign 5).
    ///
    /// Park it the way text parks instead. The parking sink consumes the
    /// stream without any async element, the pipeline stays settled, the
    /// engine notices the divergence and re-asserts on the next pump, and
    /// decodebin3 drops the pad again. A genuine re-enable flips the
    /// dispatched intent before its selection reaches decodebin3, so it takes
    /// [`Self::Build`].
    ///
    /// [`crate::TestStaging::route_deselected_video`] restores the
    /// unconditional rebuild and gates this whole disposition (the mirror it
    /// reads is inert without this read).
    ParkDeselected,
    /// A video pad re-routed for a group whose end has already entered
    /// streamsynchronizer cannot preroll a fresh chain. The stream's own end
    /// was consumed before the re-route (its EOS either passed with the old
    /// chain or died with the dropped slot), so the new branch will never see
    /// a buffer or an EOS. The sink parks in Ready->Paused forever, the async
    /// transition never completes, the receiver's pump gate never opens, and
    /// a sibling EOS arriving after the splice parks its streaming thread
    /// inside gst_stream_synchronizer_wait against the fresh pad
    /// (fuzz_buffering seed 1600058, gdb captured: multiqueue src in
    /// gststreamsynchronizer.c:480 behind the resurrected pad, the video
    /// source task idle at EOS, see the findings entry on the
    /// drained-resurrect park). This also enforces the invariant the
    /// sibling-pass gate states, that once one EOS of a group entered ssync
    /// the group MUST complete, which a fresh EOS-less pad makes impossible.
    ///
    /// Parking trades the permanent wedge for a silent video slot on the
    /// drained remainder of the item. A flushing seek restarts the streams
    /// and clears the mirrors (see `Job::Seek`), so a re-enable after a seek
    /// still builds the chain normally.
    ParkDrained,
    /// Route it: a streamsynchronizer pair, video-chain membership, and a
    /// deferred chain join.
    Build,
}

/// Everything the video disposition reads, gathered by
/// `Inner::video_disposition` in one place.
///
/// The two drain signals are separate on purpose: either one alone marks the
/// stream drained. [`Self::group_passing`] covers a stream whose end reached
/// the OUTPUT before the re-route, [`Self::input_drained`] covers a stream
/// whose end was consumed invisibly (its slot was dropped while deselected,
/// so no output probe ever saw it, fuzz_buffering seed 1600008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VideoFacts {
    /// [`crate::Inner::video_deselected`]: the last dispatched selection
    /// turned video off. An atomic mirror of an in-flight intent, not the
    /// engine's current desire.
    pub(crate) deselected: bool,
    /// [`crate::TestStaging::route_deselected_video`]: build the chain
    /// unconditionally, as the crate did before the deselected park.
    pub(crate) stage_route_deselected: bool,
    /// The pad's stream group is the one an EOS already passed into
    /// streamsynchronizer with ([`crate::Inner::passing_eos_group`]).
    pub(crate) group_passing: bool,
    /// The pad's stream id is in [`crate::Inner::input_eos_sids`]: its input
    /// side has already pushed EOS.
    pub(crate) input_drained: bool,
    /// [`crate::Inner::video_unrouted_once`]: a video output of this item has
    /// been unrouted, so this pad is a RE-route rather than a first one. A
    /// drained stream that was never unrouted is an ordinary end of item.
    pub(crate) rerouted: bool,
}

/// The video disposition, pure over everything the three arms read.
///
/// The deselected park outranks the drained one. Both perform the same
/// surgery, so the order only decides which hazard the log names, and the
/// dispatched-intent answer is the more specific one.
pub(crate) fn video_route(facts: VideoFacts) -> VideoRoute {
    if facts.deselected && !facts.stage_route_deselected {
        return VideoRoute::ParkDeselected;
    }
    if (facts.group_passing || facts.input_drained) && facts.rerouted {
        return VideoRoute::ParkDrained;
    }
    VideoRoute::Build
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Facts with everything off: a first video pad of a live stream.
    const FRESH: VideoFacts = VideoFacts {
        deselected: false,
        stage_route_deselected: false,
        group_passing: false,
        input_drained: false,
        rerouted: false,
    };

    #[test]
    fn video_route_table() {
        let rows: &[(VideoFacts, VideoRoute, &str)] = &[
            (FRESH, VideoRoute::Build, "a first pad of a live stream"),
            (
                VideoFacts {
                    deselected: true,
                    ..FRESH
                },
                VideoRoute::ParkDeselected,
                "a resurrected pad the dispatched selection has off",
            ),
            (
                VideoFacts {
                    deselected: true,
                    stage_route_deselected: true,
                    ..FRESH
                },
                VideoRoute::Build,
                "the staging knob restores the unconditional rebuild",
            ),
            (
                VideoFacts {
                    group_passing: true,
                    rerouted: true,
                    ..FRESH
                },
                VideoRoute::ParkDrained,
                "an output-side drain signal on a re-route",
            ),
            (
                VideoFacts {
                    input_drained: true,
                    rerouted: true,
                    ..FRESH
                },
                VideoRoute::ParkDrained,
                "an input-side drain signal on a re-route",
            ),
            (
                VideoFacts {
                    group_passing: true,
                    input_drained: true,
                    rerouted: true,
                    ..FRESH
                },
                VideoRoute::ParkDrained,
                "both drain signals agree",
            ),
            (
                VideoFacts {
                    group_passing: true,
                    ..FRESH
                },
                VideoRoute::Build,
                "a drained group with no re-route is an ordinary item end",
            ),
            (
                VideoFacts {
                    input_drained: true,
                    ..FRESH
                },
                VideoRoute::Build,
                "a drained input with no re-route is an ordinary item end",
            ),
            (
                VideoFacts {
                    rerouted: true,
                    ..FRESH
                },
                VideoRoute::Build,
                "a re-route of a stream that still has data",
            ),
            (
                VideoFacts {
                    deselected: true,
                    group_passing: true,
                    input_drained: true,
                    rerouted: true,
                    ..FRESH
                },
                VideoRoute::ParkDeselected,
                "the dispatched intent outranks the drain signals",
            ),
            (
                VideoFacts {
                    deselected: true,
                    stage_route_deselected: true,
                    input_drained: true,
                    rerouted: true,
                    ..FRESH
                },
                VideoRoute::ParkDrained,
                "the knob only restores the deselected arm, never the drained one",
            ),
        ];
        for (facts, want, why) in rows {
            assert_eq!(video_route(*facts), *want, "{why}: {facts:?}");
        }
    }

    /// The wedge the drained park exists for: a re-routed pad of a stream
    /// whose EOS is already inside streamsynchronizer must never build a
    /// chain, whatever else is true. Building one prerolls a sink that no
    /// buffer and no EOS can ever reach.
    #[test]
    fn a_drained_reroute_never_builds() {
        for deselected in [false, true] {
            for group_passing in [false, true] {
                let facts = VideoFacts {
                    deselected,
                    stage_route_deselected: deselected,
                    group_passing,
                    input_drained: !group_passing,
                    rerouted: true,
                };
                assert_eq!(
                    video_route(facts),
                    VideoRoute::ParkDrained,
                    "a drained re-route built a chain that nothing can preroll: {facts:?}"
                );
            }
        }
    }

    /// The other wedge: while the dispatched selection has video off, a pad
    /// decodebin3's auto-select resurrects must not rebuild the chain, or the
    /// async re-preroll holds the pump gate closed forever. Only the staging
    /// knob may take that path back.
    #[test]
    fn a_deselected_resurrect_never_builds() {
        for group_passing in [false, true] {
            for input_drained in [false, true] {
                for rerouted in [false, true] {
                    let facts = VideoFacts {
                        deselected: true,
                        stage_route_deselected: false,
                        group_passing,
                        input_drained,
                        rerouted,
                    };
                    assert_eq!(
                        video_route(facts),
                        VideoRoute::ParkDeselected,
                        "a resurrected pad rebuilt the deselected video chain: {facts:?}"
                    );
                }
            }
        }
    }
}
