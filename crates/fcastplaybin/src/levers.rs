//! The `FCAST_*` levers that are read through a named accessor rather
//! than inline at their decision site.

use crate::{Inner, api::BitmapSubFormat};

/// Whether the cue-IR arm is live.
///
/// Lever: `FCAST_NO_CUE_IR` (set = off). With it set, this crate behaves
/// exactly as it did before cue-IR existed: `item_from_sample` never looks for
/// a `CueIrMeta`, so
/// [`SubtitleTextFormat::CueIr`](crate::SubtitleTextFormat::CueIr) is never
/// constructed and every cue arrives as `Utf8`/`PangoMarkup` as decided by the
/// caps alone. The receiver consults the same answer and then does not ask the
/// parsers for cue-ir output either, so NEGOTIATION is restored bit-for-bit
/// too: `rssubparse`/`rsssaparse` stay in their default pango-markup mode.
///
/// Read once, on first use, and never again: a lever that could change under a
/// running pipeline would let the caps and the payload disagree.
pub fn cue_ir_enabled() -> bool {
    static ENABLED: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("FCAST_NO_CUE_IR").is_none());
    *ENABLED
}

/// Which bitmap subtitle formats this instance may carry, read ONCE at
/// construction ([`Inner`]).
///
/// Levers: `FCAST_NO_BITMAP_SUBS` (master, set = all three off) and the
/// per-format `FCAST_NO_PGS_SUBS` / `FCAST_NO_VOBSUB_SUBS` /
/// `FCAST_NO_DVB_SUBS`. A disabled format answers `None` at the caps gate,
/// which is bit-for-bit the loud refusal every subpicture stream got before
/// bitmap subtitles existed. The lever restores the old behavior by collapsing
/// to the same answer, not by taking a different path.
///
/// One read for the same reason [`cue_ir_enabled`] takes one: the gate is
/// consulted at LINK time and again per sample, and a lever that changed under
/// a running pipeline would let a branch exist for a stream whose samples are
/// then dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BitmapSubsEnabled {
    pub(crate) pgs: bool,
    pub(crate) vobsub: bool,
    pub(crate) dvb: bool,
}

impl BitmapSubsEnabled {
    pub(crate) fn from_env() -> Self {
        Self::from_levers(|lever| std::env::var_os(lever).is_some())
    }

    /// The lever RULE, with its answers supplied rather than looked up, so a
    /// test can pin it without mutating a process-wide environment (which is
    /// shared by every test thread and by the pipelines they build).
    pub(crate) fn from_levers(off: impl Fn(&str) -> bool) -> Self {
        let master = off("FCAST_NO_BITMAP_SUBS");
        Self {
            pgs: !master && !off("FCAST_NO_PGS_SUBS"),
            vobsub: !master && !off("FCAST_NO_VOBSUB_SUBS"),
            dvb: !master && !off("FCAST_NO_DVB_SUBS"),
        }
    }

    /// Every format off: what the master lever produces, and what the pure-fn
    /// tests use to ask for the pre-bitmap answers.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self {
            pgs: false,
            vobsub: false,
            dvb: false,
        }
    }

    /// Every format on: the shipping default, and the state in which a gate
    /// answer of `None` means "no decoder yet", not "levered off".
    #[cfg(test)]
    pub(crate) fn all() -> Self {
        Self {
            pgs: true,
            vobsub: true,
            dvb: true,
        }
    }

    pub(crate) fn allows(self, format: BitmapSubFormat) -> bool {
        match format {
            BitmapSubFormat::Pgs => self.pgs,
            BitmapSubFormat::Vobsub => self.vobsub,
            BitmapSubFormat::Dvb => self.dvb,
        }
    }
}

/// Opt-in (`FCAST_FORCE_SYSTEM_CLOCK=1`): pin the pipeline to the monotonic
/// system clock instead of electing the audio sink's.
///
/// Every captured player wedge shares one keystone: a video-branch thread
/// parked in `gst_base_sink_wait_clock` on the AUDIO SINK's clock after that
/// clock stopped advancing (switch backpressure, an audio deselect releasing
/// the ring buffer, or a stuck pulse stream). The parked thread holds the
/// sink's stream lock and back-pressures the single demuxer thread into a
/// cycle nothing internal can break. A monotonic clock's waits always
/// complete, so the cycles cannot close (validated under stress).
///
/// NOT the default yet: through the PulseAudio shim the audio sink must
/// SLAVE to the external clock and both slaving modes audibly regress
/// (`skew` pops on jittery-latency corrections, `resample` broke near-EOS
/// draining). The native PipeWire sink shares the monotonic clock domain,
/// so once it is everywhere this becomes the default.
pub(crate) fn force_system_clock() -> bool {
    force_system_clock_lever(std::env::var("FCAST_FORCE_SYSTEM_CLOCK").ok().as_deref())
}

/// The rule with the env read supplied (the environment is process-global,
/// see [`BitmapSubsEnabled::from_levers`]). Alone among the levers this one
/// is opt-in by VALUE, exactly "1", not merely present: it flips the clock
/// under real playback, so a stray empty or "0" export must stay inert.
fn force_system_clock_lever(var: Option<&str>) -> bool {
    var == Some("1")
}

impl Inner {
    /// Whether `FCAST_NO_TEXT_RECONCILE` is set: the reconcile pass is off and
    /// the v1 remembered slots and drains are back.
    pub(crate) fn text_reconcile_levered() -> bool {
        std::env::var_os("FCAST_NO_TEXT_RECONCILE").is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::force_system_clock_lever;

    /// Every other lever is presence-tested (`is_some`); this one demands
    /// the exact value "1". Pinned so a refactor unifying the lever reads
    /// cannot silently widen the opt-in to any set value.
    #[test]
    fn force_system_clock_requires_exactly_the_value_1() {
        assert!(force_system_clock_lever(Some("1")));
        assert!(!force_system_clock_lever(None));
        // Present but not "1": inert, unlike the is_some levers.
        assert!(!force_system_clock_lever(Some("")));
        assert!(!force_system_clock_lever(Some("0")));
        assert!(!force_system_clock_lever(Some("true")));
        assert!(!force_system_clock_lever(Some("1 ")));
        assert!(!force_system_clock_lever(Some("11")));
    }
}
