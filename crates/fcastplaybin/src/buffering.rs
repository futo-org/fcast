//! Buffering and seekability queries about how much of the timeline is
//! already in hand.

use gst::prelude::*;

use crate::FcastPlaybin;

/// A buffered region of the current media, expressed as fractions `[0.0, 1.0]`
/// of the whole timeline. Derived from a `GST_QUERY_BUFFERING` in `PERCENT`
/// format, so the values map directly onto a scrubber. There can be several
/// disjoint ranges (e.g. after a seek into an unbuffered region).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BufferedRange {
    pub start: f64,
    pub stop: f64,
}

/// Overall buffering state plus the buffered ranges, for the inspector. See
/// [`FcastPlaybin::buffering_info`].
#[derive(Debug, Clone)]
pub struct BufferingInfo {
    /// Fill level of the buffer that gates playback, `0..=100`.
    pub percent: i32,
    /// Whether the pipeline is actively (re)filling and would stall if the
    /// buffer drained now.
    pub busy: bool,
    /// How the source buffers (stream, on-disk download, timeshift, live).
    pub mode: gst::BufferingMode,
    /// Estimated time until the buffer is full, when known.
    pub buffering_left: Option<gst::ClockTime>,
    /// Buffered regions of the media (may be empty even when the query
    /// otherwise succeeds).
    pub ranges: Vec<BufferedRange>,
}

/// `PERCENT`-format buffering values run `0..=GST_FORMAT_PERCENT_MAX`.
const GST_FORMAT_PERCENT_MAX: f64 = 1_000_000.0;

/// Convert a `PERCENT`-format buffering bound to a `[0.0, 1.0]` fraction.
fn percent_fraction(v: gst::GenericFormattedValue) -> Option<f64> {
    (v.format() == gst::Format::Percent)
        .then(|| (v.value() as f64 / GST_FORMAT_PERCENT_MAX).clamp(0.0, 1.0))
}

/// Extract the buffered ranges from an answered `PERCENT`-format buffering
/// query, dropping any empty or malformed range.
fn buffered_ranges_from(query: &gst::query::Buffering) -> Vec<BufferedRange> {
    query
        .ranges()
        .filter_map(|(start, stop)| {
            let start = percent_fraction(start)?;
            let stop = percent_fraction(stop)?;
            (stop > start).then_some(BufferedRange { start, stop })
        })
        .collect()
}

impl FcastPlaybin {
    /// Ask the pipeline whether the current media is seekable. `None` while
    /// it cannot answer (the seeking query only succeeds around preroll
    /// completion, well after streams are first advertised).
    pub fn query_seekable(&self) -> Option<bool> {
        let mut query = gst::query::Seeking::new(gst::Format::Time);
        if self.inner.pipeline.query(query.query_mut()) {
            Some(query.result().0)
        } else {
            None
        }
    }

    /// Buffered regions of the current media as timeline fractions, from a
    /// `GST_QUERY_BUFFERING` in `PERCENT` format. Cheap and non-blocking, so
    /// callers can poll it to drive a buffered indicator on the scrubber.
    /// Empty when nothing in the pipeline can answer (a local file with no
    /// buffering element, a live/SABR source, or before preroll).
    pub fn buffered_ranges(&self) -> Vec<BufferedRange> {
        let mut query = gst::query::Buffering::new(gst::Format::Percent);
        if !self.inner.pipeline.query(query.query_mut()) {
            return Vec::new();
        }
        buffered_ranges_from(&query)
    }

    /// Full buffering state (fill percent, mode, rates, ranges) for the
    /// inspector, from a single `GST_QUERY_BUFFERING`. `None` when nothing in
    /// the pipeline can answer the query.
    pub fn buffering_info(&self) -> Option<BufferingInfo> {
        let mut query = gst::query::Buffering::new(gst::Format::Percent);
        if !self.inner.pipeline.query(query.query_mut()) {
            return None;
        }
        let (busy, percent) = query.percent();
        let (mode, _avg_in, _avg_out, buffering_left_ms) = query.stats();
        Some(BufferingInfo {
            percent,
            busy,
            mode,
            buffering_left: (buffering_left_ms > 0)
                .then(|| gst::ClockTime::from_mseconds(buffering_left_ms as u64)),
            ranges: buffered_ranges_from(&query),
        })
    }

    /// Best-effort "buffered ahead of the playhead" duration. In STREAM mode
    /// (the receiver's default) the buffering query exposes no ranges, but the
    /// queue elements still track how much media is queued: queue2,
    /// downloadbuffer, queue and appsrc (the SABR source's per-track feed
    /// buffer) expose it element-wide, multiqueue per sink pad. Returns the
    /// deepest level found (the network-side buffer), `None` if nothing
    /// reports one. Poll it to size a buffered-ahead nub on the scrubber.
    pub fn buffered_ahead(&self) -> Option<gst::ClockTime> {
        let mut best: Option<u64> = None;
        let mut it = self.inner.pipeline.iterate_recurse();
        while let Ok(Some(elem)) = it.next() {
            let level_ns = match elem.factory().map(|f| f.name()).as_deref() {
                Some("queue2" | "downloadbuffer" | "queue" | "appsrc") => elem
                    .find_property("current-level-time")
                    .map(|_| elem.property::<u64>("current-level-time")),
                Some("multiqueue") => elem
                    .sink_pads()
                    .iter()
                    .filter_map(|pad| {
                        pad.find_property("current-level-time")
                            .map(|_| pad.property::<u64>("current-level-time"))
                    })
                    .max(),
                _ => None,
            };
            if let Some(ns) = level_ns {
                best = Some(best.map_or(ns, |b| b.max(ns)));
            }
        }
        best.filter(|ns| *ns > 0).map(gst::ClockTime::from_nseconds)
    }
}

#[cfg(test)]
mod tests {
    use super::{BufferedRange, GST_FORMAT_PERCENT_MAX, buffered_ranges_from, percent_fraction};

    const MAX: i64 = GST_FORMAT_PERCENT_MAX as i64;

    fn pct(raw: i64) -> gst::GenericFormattedValue {
        gst::GenericFormattedValue::new(gst::Format::Percent, raw)
    }

    /// The domain gate and the scrubber mapping: only PERCENT answers, and
    /// the fraction is clamped so a scrubber never draws outside [0, 1].
    #[test]
    fn percent_fraction_maps_the_percent_domain_onto_the_unit_interval() {
        assert_eq!(percent_fraction(pct(0)), Some(0.0));
        assert_eq!(percent_fraction(pct(MAX / 4)), Some(0.25));
        assert_eq!(percent_fraction(pct(MAX / 2)), Some(0.5));
        assert_eq!(percent_fraction(pct(MAX)), Some(1.0));
        // The unset marker (-1, how the binding renders a range value the C
        // side holds outside the percent domain) clamps to 0.0, never to a
        // negative fraction.
        assert_eq!(percent_fraction(pct(-1)), Some(0.0));
        // An out-of-domain raw collapses to that same marker at
        // construction, so it too answers a clamped 0.0 rather than > 1.
        assert_eq!(percent_fraction(pct(2 * MAX)), Some(0.0));
        // A value in any other format is refused, not misread as percent.
        assert_eq!(
            percent_fraction(gst::GenericFormattedValue::new(gst::Format::Time, MAX / 2)),
            None
        );
        assert_eq!(
            percent_fraction(gst::GenericFormattedValue::new(gst::Format::Bytes, 0)),
            None
        );
    }

    /// An answered query's ranges map to fractions, and malformed shapes
    /// (empty, single-point, inverted, out-of-domain) yield NO range rather
    /// than a wrong one.
    #[test]
    fn buffered_ranges_keep_only_forward_in_domain_ranges() {
        gst::init().unwrap();
        // Nothing answered: empty, not an error.
        let query = gst::query::Buffering::new(gst::Format::Percent);
        assert_eq!(buffered_ranges_from(&query), Vec::new());

        // Two disjoint ranges, the shape a seek into unbuffered media leaves.
        let mut query = gst::query::Buffering::new(gst::Format::Percent);
        query.add_buffering_ranges([(pct(0), pct(MAX / 2)), (pct(3 * MAX / 4), pct(MAX))]);
        assert_eq!(
            buffered_ranges_from(&query),
            vec![
                BufferedRange {
                    start: 0.0,
                    stop: 0.5
                },
                BufferedRange {
                    start: 0.75,
                    stop: 1.0
                },
            ]
        );

        // A single-point range and an inverted one are refused at the query
        // (gst_query_add_buffering_range drops start >= stop), so neither
        // can reach the scrubber.
        let mut query = gst::query::Buffering::new(gst::Format::Percent);
        query.add_buffering_ranges([(pct(MAX / 2), pct(MAX / 2)), (pct(MAX / 2), pct(MAX / 4))]);
        assert_eq!(buffered_ranges_from(&query), Vec::new());

        // A start below the domain arrives as the unset marker and clamps
        // to 0.0; the range survives because its stop is still ahead.
        let mut query = gst::query::Buffering::new(gst::Format::Percent);
        query.add_buffering_ranges([(pct(-1), pct(MAX / 2))]);
        assert_eq!(
            buffered_ranges_from(&query),
            vec![BufferedRange {
                start: 0.0,
                stop: 0.5
            }]
        );

        // A stop above the domain collapses to the marker before the query
        // stores it, so the whole range drops instead of clamping wide.
        let mut query = gst::query::Buffering::new(gst::Format::Percent);
        query.add_buffering_ranges([(pct(0), pct(2 * MAX))]);
        assert_eq!(buffered_ranges_from(&query), Vec::new());

        // Ranges answered in another format are dropped wholesale.
        let mut query = gst::query::Buffering::new(gst::Format::Time);
        query.add_buffering_ranges([(
            gst::ClockTime::from_seconds(1),
            gst::ClockTime::from_seconds(2),
        )]);
        assert_eq!(buffered_ranges_from(&query), Vec::new());
    }
}
