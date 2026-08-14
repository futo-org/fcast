//! The per-item external subtitle catalog and the STABLE track ids it hands
//! out.
//!
//! Ids share the embedded subtitle namespace, above [`EXTERNAL_TRACK_ID_BASE`].
//! A removal must NOT renumber survivors: senders cache advertised ids, so
//! renumbering makes an in-flight `SetTrack` name the wrong track. An id is
//! assigned once at attach, from a counter no removal or `clear` rewinds.

use smol_str::SmolStr;

use crate::{application::PacketOrigin, player};

/// Track ids at or above this value denote external subtitles rather than
/// indices into `Player::streams`; real stream indices are small, so the
/// namespaces never collide.
pub(crate) const EXTERNAL_TRACK_ID_BASE: u32 = 0x1000_0000;

/// Whether a protocol/GUI track id names an external catalog entry rather than
/// a `Player::streams` index.
pub(crate) fn is_external_track_id(id: u32) -> bool {
    id >= EXTERNAL_TRACK_ID_BASE
}

/// The live-input handle in production: fcastplaybin's attached-input id. The
/// catalog is generic over it only so the id policy can be unit-tested without
/// a pipeline.
pub(crate) type SubHandle = fcastplaybin::ExternalSubId;

pub(crate) struct ExternalSubtitle<H = SubHandle> {
    /// Stable id advertised as this track's `MediaTrack.id`
    /// (>= [`EXTERNAL_TRACK_ID_BASE`]); assigned once at attach, never
    /// reassigned or reused.
    pub(crate) id: u32,
    pub(crate) url: String,
    pub(crate) name: Option<SmolStr>,
    pub(crate) requested_by: PacketOrigin,
    /// The live input attached for this entry (every catalog external is
    /// attached simultaneously, selection is pure SELECT_STREAMS). Stable for
    /// the entry's whole life.
    pub(crate) handle: H,
    /// The entry's GStreamer stream id, learned when its stream first
    /// materializes. URI-derived, so it stays valid across fcastplaybin's
    /// internal input replacements; all id/stream mapping goes through this.
    pub(crate) stream_sid: Option<String>,
}

/// Every external subtitle of the current item, in catalog (attach) order.
pub(crate) struct Catalog<H = SubHandle> {
    entries: Vec<ExternalSubtitle<H>>,
    /// Monotonic id source; only ever moves forward, including across
    /// [`clear`](Self::clear).
    next_id: u32,
}

// Hand-written: `derive(Default)` would demand `H: Default`.
impl<H> Default for Catalog<H> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 0,
        }
    }
}

impl<H: Copy + PartialEq> Catalog<H> {
    /// Record a newly attached external and assign it its stable track id.
    pub(crate) fn attach(
        &mut self,
        url: String,
        name: Option<SmolStr>,
        requested_by: PacketOrigin,
        handle: H,
    ) -> u32 {
        let id = EXTERNAL_TRACK_ID_BASE + self.next_id;
        // Saturating rather than wrapping: wrapping would alias a live id.
        self.next_id = self.next_id.saturating_add(1);
        self.entries.push(ExternalSubtitle {
            id,
            url,
            name,
            requested_by,
            handle,
            stream_sid: None,
        });
        id
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every entry, in stable catalog order (advertised after the embedded
    /// tracks).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &ExternalSubtitle<H>> {
        self.entries.iter()
    }

    pub(crate) fn by_id(&self, id: u32) -> Option<&ExternalSubtitle<H>> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// The stable track id of the entry fcastplaybin knows by `handle`.
    pub(crate) fn id_of_handle(&self, handle: H) -> Option<u32> {
        self.entries
            .iter()
            .find(|entry| entry.handle == handle)
            .map(|entry| entry.id)
    }

    /// The stable track id of the entry whose stream is `sid`. `None` means
    /// the stream belongs to no entry, so it is an embedded track.
    pub(crate) fn id_of_stream(&self, sid: &str) -> Option<u32> {
        self.entries
            .iter()
            .find(|entry| entry.stream_sid.as_deref() == Some(sid))
            .map(|entry| entry.id)
    }

    /// The stream ids of the entries whose streams have materialized. Those
    /// streams are advertised under their entry's stable id, so their
    /// `Player::streams` positions must be excluded from the embedded track
    /// list.
    pub(crate) fn materialized_sids(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter_map(|entry| entry.stream_sid.as_deref())
    }

    /// Learn the stream id of every entry whose stream just materialized, and
    /// return what was resolved. Already-known sids are never overwritten (the
    /// first one is the stable, URI-derived answer).
    pub(crate) fn learn_stream_sids(
        &mut self,
        mut lookup: impl FnMut(H) -> Option<player::StreamId>,
    ) -> Vec<(u32, String)> {
        let mut learned = Vec::new();
        for entry in self.entries.iter_mut() {
            if entry.stream_sid.is_none()
                && let Some(sid) = lookup(entry.handle)
            {
                learned.push((entry.id, sid.clone()));
                entry.stream_sid = Some(sid);
            }
        }
        learned
    }

    /// Drop the entry with this id. The counter is deliberately NOT rewound:
    /// the survivors keep the ids senders already hold, and the removed id
    /// stays retired for the rest of the media source.
    pub(crate) fn remove(&mut self, id: u32) -> Option<ExternalSubtitle<H>> {
        let pos = self.entries.iter().position(|entry| entry.id == id)?;
        Some(self.entries.remove(pos))
    }

    /// Drop every entry (external subtitles are per-item). The id counter keeps
    /// advancing, so a stale id can never alias a new one.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handles are plain integers here: the real
    /// [`fcastplaybin::ExternalSubId`] can only be minted by a live playbin.
    type TestCatalog = Catalog<u32>;

    fn attach(catalog: &mut TestCatalog, url: &str, handle: u32) -> u32 {
        catalog.attach(
            url.to_owned(),
            Some(SmolStr::new(url)),
            PacketOrigin::Gui,
            handle,
        )
    }

    #[test]
    fn ids_start_at_the_base_and_count_up() {
        let mut catalog = TestCatalog::default();
        assert_eq!(attach(&mut catalog, "a.vtt", 1), EXTERNAL_TRACK_ID_BASE);
        assert_eq!(attach(&mut catalog, "b.vtt", 2), EXTERNAL_TRACK_ID_BASE + 1);
        assert_eq!(attach(&mut catalog, "c.vtt", 3), EXTERNAL_TRACK_ID_BASE + 2);
        // No embedded track index can reach the external namespace.
        assert!(catalog.iter().all(|entry| is_external_track_id(entry.id)));
        assert!(!is_external_track_id(0));
        assert!(!is_external_track_id(EXTERNAL_TRACK_ID_BASE - 1));
    }

    /// Removing one external must not renumber the survivors, or a sender's
    /// cached or in-flight SetTrack names the wrong track.
    #[test]
    fn removing_the_middle_entry_leaves_the_survivors_ids_alone() {
        let mut catalog = TestCatalog::default();
        let a = attach(&mut catalog, "a.vtt", 1);
        let b = attach(&mut catalog, "b.vtt", 2);
        let c = attach(&mut catalog, "c.vtt", 3);

        let removed = catalog.remove(b).expect("b is in the catalog");
        assert_eq!(removed.url, "b.vtt");

        // Survivors keep both their ids and their catalog order.
        let ids: Vec<u32> = catalog.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, [a, c]);
        assert_eq!(catalog.by_id(a).map(|e| e.url.as_str()), Some("a.vtt"));
        assert_eq!(catalog.by_id(c).map(|e| e.url.as_str()), Some("c.vtt"));
        // And they still resolve to the right live inputs, in both directions.
        assert_eq!(catalog.by_id(a).map(|e| e.handle), Some(1));
        assert_eq!(catalog.by_id(c).map(|e| e.handle), Some(3));
        assert_eq!(catalog.id_of_handle(3), Some(c));
    }

    #[test]
    fn a_removed_id_resolves_to_nothing_and_stays_retired() {
        let mut catalog = TestCatalog::default();
        attach(&mut catalog, "a.vtt", 1);
        let b = attach(&mut catalog, "b.vtt", 2);
        catalog.remove(b);

        assert!(catalog.by_id(b).is_none());
        assert!(catalog.id_of_handle(2).is_none());
        assert!(catalog.remove(b).is_none());

        // A later attach gets a FRESH id, never the retired one.
        let c = attach(&mut catalog, "c.vtt", 3);
        assert_ne!(c, b);
        assert_eq!(c, EXTERNAL_TRACK_ID_BASE + 2);
        assert!(catalog.by_id(b).is_none());
    }

    #[test]
    fn ids_are_never_reused_within_one_media_source() {
        let mut catalog = TestCatalog::default();
        let mut seen = Vec::new();
        for round in 0..4u32 {
            seen.push(attach(&mut catalog, "a.vtt", round * 2));
            seen.push(attach(&mut catalog, "b.vtt", round * 2 + 1));
            catalog.clear();
            assert!(catalog.is_empty());
        }
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), seen.len(), "an id was reused: {seen:?}");
    }

    #[test]
    fn a_fresh_catalog_restarts_the_numbering() {
        let mut first = TestCatalog::default();
        attach(&mut first, "a.vtt", 1);
        attach(&mut first, "b.vtt", 2);

        let mut second = TestCatalog::default();
        assert_eq!(attach(&mut second, "a.vtt", 1), EXTERNAL_TRACK_ID_BASE);
        assert!(second.by_id(EXTERNAL_TRACK_ID_BASE + 1).is_none());
    }

    #[test]
    fn stream_ids_map_back_to_the_stable_track_id() {
        let mut catalog = TestCatalog::default();
        let a = attach(&mut catalog, "a.vtt", 1);
        let b = attach(&mut catalog, "b.vtt", 2);
        let c = attach(&mut catalog, "c.vtt", 3);

        // Only b's and c's streams materialize.
        let learned = catalog.learn_stream_sids(|h| match h {
            2 => Some("sub-b".to_owned()),
            3 => Some("sub-c".to_owned()),
            _ => None,
        });
        assert_eq!(learned, [(b, "sub-b".to_owned()), (c, "sub-c".to_owned())]);

        assert_eq!(catalog.id_of_stream("sub-b"), Some(b));
        assert_eq!(catalog.id_of_stream("sub-c"), Some(c));
        // An embedded track's stream belongs to no entry.
        assert_eq!(catalog.id_of_stream("embedded-text"), None);

        // Removing b must not move c's mapping.
        catalog.remove(b);
        assert_eq!(catalog.id_of_stream("sub-b"), None);
        assert_eq!(catalog.id_of_stream("sub-c"), Some(c));
        assert_eq!(catalog.by_id(a).map(|e| e.stream_sid.clone()), Some(None));

        // The embedded advertising reads the same field, so the two sides
        // cannot disagree about which streams are externals.
        let materialized: Vec<&str> = catalog.materialized_sids().collect();
        assert_eq!(materialized, ["sub-c"]);
    }

    #[test]
    fn learning_a_stream_id_is_idempotent() {
        let mut catalog = TestCatalog::default();
        let a = attach(&mut catalog, "a.vtt", 1);
        assert_eq!(
            catalog.learn_stream_sids(|_| Some("sub-a".to_owned())),
            [(a, "sub-a".to_owned())]
        );
        // A later answer with a different sid does not move the mapping.
        assert!(
            catalog
                .learn_stream_sids(|_| Some("sub-a-replaced".to_owned()))
                .is_empty()
        );
        assert_eq!(catalog.id_of_stream("sub-a"), Some(a));
    }
}
