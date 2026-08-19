//! A pipeline-graph snapshot for the receiver's inspector.
//!
//! The walk reads live element properties, so it must not race element
//! teardown. Run it on the crate worker ([`FcastPlaybin::debug_graph_async`]).
//!
//! [`FcastPlaybin::debug_graph_async`]: crate::FcastPlaybin::debug_graph_async

use std::collections::HashMap;

use gst::prelude::*;

/// One element's worth of the snapshot. Bins carry their children and expose
/// ghost pads as ordinary pads, so the pipeline is one recursive cell tree.
#[derive(Debug, Default, Clone)]
pub struct GraphCell {
    /// Element name (unique within its bin), e.g. `avdec_h264-0`.
    pub name: String,
    /// Factory name, else the GType name (directly-constructed bins report
    /// `GstBin` and their `name` carries the kind).
    pub type_name: String,
    /// Settled state (`PLAYING`) or transition (`READY → PAUSED`).
    pub state: String,
    /// `prop=value` for readable properties differing from their default.
    pub properties: Vec<String>,
    pub sink_pads: Vec<GraphPad>,
    pub src_pads: Vec<GraphPad>,
    /// Non-empty only for bins.
    pub children: Vec<GraphCell>,
    pub is_bin: bool,
}

#[derive(Debug, Clone)]
pub struct GraphPad {
    /// Snapshot-wide pad id, referenced by [`GraphLink`].
    pub id: u32,
    pub name: String,
    /// Activation/task/flag summary, e.g. `push`, `pull task:started`,
    /// `push blocked EOS`. Empty for a plain idle pad.
    pub detail: String,
    /// Ghost pad (sits on a bin boundary).
    pub ghost: bool,
}

/// A pad-to-pad connection, plus the internal leg of every ghost sink pad
/// (ghost -> target), so following links traverses bin boundaries.
#[derive(Debug, Clone)]
pub struct GraphLink {
    pub src_pad: u32,
    pub sink_pad: u32,
    /// Negotiated caps as display lines; empty when not negotiated yet.
    pub caps: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct GraphSnapshot {
    /// The pipeline itself (a bin cell).
    pub root: GraphCell,
    pub links: Vec<GraphLink>,
}

/// Snapshot `bin` and everything below it. Reads live element state, so call
/// it where element teardown cannot race (the crate worker).
pub fn snapshot(bin: &gst::Bin) -> GraphSnapshot {
    let mut walker = Walker::default();
    let root = walker.bin_cell(bin);

    let links = walker
        .raw_links
        .iter()
        .filter_map(|(src, sink, caps)| {
            Some(GraphLink {
                src_pad: *walker.pad_ids.get(src)?,
                sink_pad: *walker.pad_ids.get(sink)?,
                caps: caps.as_ref().map(caps_lines).unwrap_or_default(),
            })
        })
        .collect();

    GraphSnapshot { root, links }
}

#[derive(Default)]
struct Walker {
    next_pad_id: u32,
    pad_ids: HashMap<gst::Pad, u32>,
    /// (src, sink, caps), resolved to ids once every pad has been visited.
    /// Links to peers outside the walked hierarchy are dropped.
    raw_links: Vec<(gst::Pad, gst::Pad, Option<gst::Caps>)>,
}

impl Walker {
    fn bin_cell(&mut self, bin: &gst::Bin) -> GraphCell {
        let mut cell = self.element_cell(bin.upcast_ref());
        cell.is_bin = true;
        // `children()` lists newest-added first; reverse for creation order.
        cell.children = bin
            .children()
            .iter()
            .rev()
            .map(|child| match child.downcast_ref::<gst::Bin>() {
                Some(sub) => self.bin_cell(sub),
                None => self.element_cell(child),
            })
            .collect();
        cell
    }

    fn element_cell(&mut self, elem: &gst::Element) -> GraphCell {
        let (_, current, pending) = elem.state(Some(gst::ClockTime::ZERO));
        let state = if pending == gst::State::VoidPending {
            format!("{current:?}").to_uppercase()
        } else {
            format!("{current:?} → {pending:?}").to_uppercase()
        };

        let mut cell = GraphCell {
            name: elem.name().to_string(),
            type_name: elem
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| elem.type_().name().to_string()),
            state,
            properties: non_default_properties(elem),
            ..Default::default()
        };

        for pad in elem.pads() {
            let id = self.pad_id(&pad);
            let entry = GraphPad {
                id,
                name: pad.name().to_string(),
                detail: pad_detail(&pad),
                ghost: pad.is::<gst::GhostPad>(),
            };
            match pad.direction() {
                gst::PadDirection::Src => {
                    cell.src_pads.push(entry);
                    if let Some(peer) = pad.peer() {
                        let caps = pad.current_caps();
                        self.raw_links.push((pad, resolve_peer(peer), caps));
                    }
                }
                gst::PadDirection::Sink => {
                    cell.sink_pads.push(entry);
                    // Internal leg of a ghost sink pad. Ghost src pads need
                    // none because `resolve_peer` maps their proxy back to
                    // the ghost.
                    if let Some(target) = pad
                        .downcast_ref::<gst::GhostPad>()
                        .and_then(|ghost| ghost.target())
                    {
                        let caps = pad.current_caps();
                        self.raw_links.push((pad, target, caps));
                    }
                }
                _ => {}
            }
        }

        cell
    }

    fn pad_id(&mut self, pad: &gst::Pad) -> u32 {
        *self.pad_ids.entry(pad.clone()).or_insert_with(|| {
            self.next_pad_id += 1;
            self.next_pad_id
        })
    }
}

/// Logical link endpoint for `peer`: map a ghost's internal proxy pad back to
/// the ghost, so links always terminate on pads the walk has visited.
fn resolve_peer(peer: gst::Pad) -> gst::Pad {
    if peer.is::<gst::GhostPad>() {
        return peer;
    }
    if let Some(proxy) = peer.downcast_ref::<gst::ProxyPad>()
        && let Some(ghost) = proxy.internal()
    {
        return ghost.upcast();
    }
    peer
}

fn pad_detail(pad: &gst::Pad) -> String {
    let mut parts: Vec<&str> = Vec::new();
    match pad.mode() {
        gst::PadMode::Push => parts.push("push"),
        gst::PadMode::Pull => parts.push("pull"),
        _ => {}
    }
    match pad.task_state() {
        gst::TaskState::Started => parts.push("task:started"),
        gst::TaskState::Paused => parts.push("task:paused"),
        _ => {}
    }
    let flags = pad.pad_flags();
    if flags.contains(gst::PadFlags::BLOCKED) {
        parts.push("blocked");
    }
    if flags.contains(gst::PadFlags::FLUSHING) {
        parts.push("flushing");
    }
    if flags.contains(gst::PadFlags::EOS) {
        parts.push("EOS");
    }
    parts.join(" ")
}

/// `prop=value` for every readable property differing from its default.
/// Unserializable values are skipped, long ones truncated.
fn non_default_properties(elem: &gst::Element) -> Vec<String> {
    let mut out = Vec::new();
    for pspec in elem.list_properties() {
        if !pspec.flags().contains(gst::glib::ParamFlags::READABLE)
            || pspec.flags().contains(gst::glib::ParamFlags::DEPRECATED)
        {
            continue;
        }
        let name = pspec.name();
        if name == "name" || name == "parent" {
            continue;
        }
        // Reading Sample-typed properties would take a reference on a
        // whole video frame.
        if pspec.value_type().is_a(gst::Sample::static_type()) {
            continue;
        }
        let Some(value) = serialize_value(&elem.property_value(name)) else {
            continue;
        };
        let default = serialize_value(pspec.default_value());
        if Some(&value) == default.as_ref() {
            continue;
        }
        out.push(format!("{name}={}", truncate(&value, 80)));
    }
    out
}

/// `gst_value_serialize`, but NULL boxed values map to `None` first. Boxed
/// GType serializers assert on NULL and log CRITICALs.
fn serialize_value(value: &gst::glib::Value) -> Option<gst::glib::GString> {
    use gst::glib::translate::ToGlibPtr;

    if value.type_().is_a(gst::glib::Type::BOXED) {
        let boxed = unsafe { gst::glib::gobject_ffi::g_value_get_boxed(value.to_glib_none().0) };
        if boxed.is_null() {
            return None;
        }
    }
    value.serialize().ok()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

/// Caps as display lines: `media/type (features)` then `  field: value`.
fn caps_lines(caps: &gst::Caps) -> Vec<String> {
    if caps.is_any() {
        return vec!["ANY".to_string()];
    }
    if caps.is_empty() {
        return vec!["EMPTY".to_string()];
    }
    let mut lines = Vec::new();
    for (i, structure) in caps.iter().enumerate() {
        let mut head = structure.name().to_string();
        if let Some(features) = caps.features(i)
            && !features.is_equal(&gst::CAPS_FEATURES_MEMORY_SYSTEM_MEMORY)
        {
            head.push_str(&format!(" ({features})"));
        }
        lines.push(head);
        for (field, value) in structure.iter() {
            let value = value
                .serialize()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "?".to_string());
            lines.push(format!("  {field}: {}", truncate(&value, 60)));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| gst::init().unwrap());
    }

    /// fakesrc -> [bin: identity (ghosted both sides)] -> fakesink.
    #[test]
    fn walks_bins_and_ghost_pads() {
        init();

        let pipeline = gst::Pipeline::with_name("p");
        let src = gst::ElementFactory::make("fakesrc").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();

        let bin = gst::Bin::with_name("wrap");
        let identity = gst::ElementFactory::make("identity").build().unwrap();
        bin.add(&identity).unwrap();
        let ghost_sink = gst::GhostPad::with_target(&identity.static_pad("sink").unwrap()).unwrap();
        let ghost_src = gst::GhostPad::builder_with_target(&identity.static_pad("src").unwrap())
            .unwrap()
            .name("src")
            .build();
        bin.add_pad(&ghost_sink).unwrap();
        bin.add_pad(&ghost_src).unwrap();

        pipeline.add_many([&src, bin.upcast_ref(), &sink]).unwrap();
        gst::Element::link_many([&src, bin.upcast_ref(), &sink]).unwrap();

        let snap = snapshot(pipeline.upcast_ref());

        assert!(snap.root.is_bin);
        assert_eq!(snap.root.name, "p");
        assert_eq!(snap.root.children.len(), 3);

        let wrap = snap
            .root
            .children
            .iter()
            .find(|c| c.name == "wrap")
            .expect("wrapper bin cell");
        assert!(wrap.is_bin);
        assert_eq!(wrap.children.len(), 1);
        assert_eq!(wrap.children[0].type_name, "identity");
        assert_eq!(wrap.sink_pads.len(), 1);
        assert_eq!(wrap.src_pads.len(), 1);
        assert!(wrap.sink_pads[0].ghost);

        let mut ids = Vec::new();
        fn collect(cell: &GraphCell, ids: &mut Vec<u32>) {
            for p in cell.sink_pads.iter().chain(&cell.src_pads) {
                ids.push(p.id);
            }
            for c in &cell.children {
                collect(c, ids);
            }
        }
        collect(&snap.root, &mut ids);
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());

        // src -> ghost sink -> identity -> ghost src -> sink: four links.
        assert_eq!(snap.links.len(), 4, "links: {:?}", snap.links);

        let find = |cell_name: &str, pad_name: &str| -> u32 {
            fn walk<'a>(cell: &'a GraphCell, name: &str) -> Option<&'a GraphCell> {
                if cell.name == name {
                    return Some(cell);
                }
                cell.children.iter().find_map(|c| walk(c, name))
            }
            let cell = walk(&snap.root, cell_name).expect(cell_name);
            cell.sink_pads
                .iter()
                .chain(&cell.src_pads)
                .find(|p| p.name == pad_name)
                .unwrap_or_else(|| panic!("{cell_name}:{pad_name}"))
                .id
        };
        let has_link = |src: u32, sink: u32| {
            snap.links
                .iter()
                .any(|l| l.src_pad == src && l.sink_pad == sink)
        };

        let src_name = snap.root.children[0].name.clone();
        let sink_name = snap.root.children[2].name.clone();
        let identity_name = wrap.children[0].name.clone();
        assert!(has_link(find(&src_name, "src"), find("wrap", "sink")));
        assert!(has_link(find("wrap", "sink"), find(&identity_name, "sink")));
        assert!(has_link(find(&identity_name, "src"), find("wrap", "src")));
        assert!(has_link(find("wrap", "src"), find(&sink_name, "sink")));
    }

    #[test]
    fn caps_lines_format() {
        init();
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "NV12")
            .field("width", 1920i32)
            .build();
        let lines = caps_lines(&caps);
        assert_eq!(lines[0], "video/x-raw");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("width") && l.contains("1920"))
        );
    }

    #[test]
    fn truncation_keeps_short_strings() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 5), "abcde…");
    }

    /// The cut counts CHARS, not bytes: multi-byte input under the limit
    /// stays whole even when its byte length exceeds it, and a cut lands on a
    /// char boundary (a byte slice here would panic mid-codepoint).
    #[test]
    fn truncation_is_char_boundary_safe_on_multibyte_input() {
        // 5 chars, 10 bytes: within the char limit, so untouched.
        assert_eq!(truncate("ααααα", 5), "ααααα");
        assert_eq!(truncate("ααααα", 3), "ααα…");
        // 4-byte codepoints get the same treatment.
        assert_eq!(truncate("🦀🦀🦀", 2), "🦀🦀…");
        // The ellipsis only appears on an actual cut.
        assert_eq!(truncate("🦀🦀🦀", 3), "🦀🦀🦀");
        assert_eq!(truncate("", 0), "");
    }

    /// `serialize_value`: a plain value serializes, a NULL boxed value maps
    /// to None instead of tripping the boxed serializer's NULL assert.
    #[test]
    fn serialize_value_skips_null_boxed_values() {
        init();
        assert_eq!(serialize_value(&123i32.to_value()).as_deref(), Some("123"));
        let null_caps = gst::glib::Value::from_type(gst::Caps::static_type());
        assert_eq!(serialize_value(&null_caps), None);
        let real_caps = gst::Caps::builder("video/x-raw").build().to_value();
        assert!(serialize_value(&real_caps).is_some());
    }

    /// `non_default_properties` lists exactly the properties that moved off
    /// their default, and never `name`.
    #[test]
    fn non_default_properties_lists_only_changed_properties() {
        init();
        let stock = gst::ElementFactory::make("identity").build().unwrap();
        let props = non_default_properties(&stock);
        assert!(
            !props.iter().any(|p| p.starts_with("sync=")),
            "a default-valued property must not be listed: {props:?}"
        );
        assert!(
            !props.iter().any(|p| p.starts_with("name=")),
            "name is always skipped: {props:?}"
        );

        let tuned = gst::ElementFactory::make("identity")
            .property("sync", true)
            .build()
            .unwrap();
        let props = non_default_properties(&tuned);
        assert!(
            props.iter().any(|p| p == "sync=true"),
            "a changed property must be listed as prop=value: {props:?}"
        );
    }
}
