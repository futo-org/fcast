//! Layout for the inspector's pipeline graph.
//!
//! Takes the [`GraphSnapshot`] the playbin worker produced (see
//! `fcastplaybin::graph`) and lays it out into flat primitives Slint draws
//! natively: axis-aligned rectangles, positioned left-aligned text, and two
//! batched SVG path strings (edge polylines and arrowheads). No graphviz.
//!
//! The layout is the classic left-to-right layered scheme. Within every bin,
//! direct children are ranked by longest path over the links that connect
//! them, ranks become columns, and each column is stacked vertically in the
//! order of the children's upstream neighbours. Bins recurse, and their
//! ghost pads sit on the bin border so cross-boundary links attach where
//! the eye expects them. Edges are routed orthogonally around the boxes,
//! and each wire carries a one-line caps chip that the UI expands on hover.
//!
//! Text metrics are estimated (the `CHAR_W` em-width factor) because the
//! layout runs off the GUI thread with no font access. Boxes get enough
//! slack that a few pixels of error never clip.

use std::collections::HashMap;

use fcastplaybin::graph::{GraphCell, GraphSnapshot};
use slint::Color;

#[derive(Debug, Clone)]
pub struct SceneRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub fill: Color,
    pub stroke: Color,
}

#[derive(Debug, Clone)]
pub struct SceneText {
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub color: Color,
    pub text: String,
}

/// A caps label: a compact always-visible chip on the wire, expanding to
/// the full caps block on hover. Full caps blocks cannot be placed sanely
/// next to a multi-stream fan-out, so only the chip is always visible.
#[derive(Debug, Clone)]
pub struct SceneLabel {
    /// Chip box.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// One line: media type plus the load-bearing params.
    pub summary: String,
    /// Full caps block, newline-separated, shown when the chip expands.
    pub detail: String,
    /// Expanded-chip size for the full block.
    pub detail_w: f32,
    pub detail_h: f32,
    /// Index into [`Scene::edge_paths`]: hovering the chip highlights its wire.
    pub edge: usize,
}

/// One routed wire's own geometry, for hover highlighting.
#[derive(Debug, Clone)]
pub struct SceneEdge {
    pub commands: String,
    pub arrow: String,
}

/// A hover hit-zone covering one segment of a wire.
#[derive(Debug, Clone)]
pub struct SceneEdgeHit {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub edge: usize,
}

/// The laid-out graph. `edges`/`arrows` are single SVG path strings (all
/// edges share one stroke, all arrowheads one fill) so the UI draws the
/// whole wiring in two Path elements regardless of graph size.
#[derive(Debug, Default, Clone)]
pub struct Scene {
    pub width: f32,
    pub height: f32,
    pub rects: Vec<SceneRect>,
    pub texts: Vec<SceneText>,
    pub edges: String,
    pub arrows: String,
    /// Caps chips, drawn above the wiring with hover interaction in the UI.
    pub labels: Vec<SceneLabel>,
    /// Per-wire geometry, in routing order.
    pub edge_paths: Vec<SceneEdge>,
    /// Thin hover rectangles along every wire segment.
    pub edge_hits: Vec<SceneEdgeHit>,
}

const MARGIN: f32 = 16.0;
const PADDING: f32 = 8.0;
/// Vertical distance between stacked siblings in a column.
const V_GAP: f32 = 22.0;
/// Minimum horizontal gap between columns (before caps labels widen it).
const BASE_GAP: f32 = 56.0;
const TITLE_SIZE: f32 = 13.0;
const BODY_SIZE: f32 = 11.0;
const PAD_SIZE: f32 = 10.0;
const CAPS_SIZE: f32 = 10.0;
/// Estimated average glyph advance as a fraction of the font size. Slightly
/// generous on purpose, so estimated text never sits flush on a box border.
const CHAR_W: f32 = 0.65;
const LINE_H: f32 = 14.0;
/// Show at most this many property lines per element (the rest collapse
/// into a `+N more` line) so a property-heavy element can't dwarf the graph.
const MAX_PROPS: usize = 14;

// The inspector's own palette (see inspector.slint): #1c1c1e surfaces,
// #ffffff20 hairlines, white/#b0b0b0/#8a8a8a text tiers, #4da3ff blue and
// #6ee7a0 green accents.
// The canvas itself is card-coloured (#1c1c1e, set in inspector.slint), so
// the box fills step one shade lighter per nesting level to stay visible.
const BIN_FILL: [Color; 2] = [
    Color::from_rgb_u8(0x21, 0x21, 0x24),
    Color::from_rgb_u8(0x26, 0x26, 0x2a),
];
const BIN_STROKE: Color = Color::from_argb_u8(0x20, 0xff, 0xff, 0xff);
const ELEM_FILL: Color = Color::from_rgb_u8(0x2c, 0x2c, 0x30);
const ELEM_STROKE: Color = Color::from_argb_u8(0x38, 0xff, 0xff, 0xff);
const TITLE_COLOR: Color = Color::from_rgb_u8(0xff, 0xff, 0xff);
const NAME_COLOR: Color = Color::from_rgb_u8(0xb0, 0xb0, 0xb0);
const PROP_COLOR: Color = Color::from_rgb_u8(0x8a, 0x8a, 0x8a);
const SINK_PAD_FILL: Color = Color::from_rgb_u8(0x1d, 0x2f, 0x42);
const SINK_PAD_STROKE: Color = Color::from_rgb_u8(0x4d, 0xa3, 0xff);
const SINK_PAD_TEXT: Color = Color::from_rgb_u8(0xcf, 0xe4, 0xfa);
const SRC_PAD_FILL: Color = Color::from_rgb_u8(0x1e, 0x36, 0x29);
const SRC_PAD_STROKE: Color = Color::from_rgb_u8(0x6e, 0xe7, 0xa0);
const SRC_PAD_TEXT: Color = Color::from_rgb_u8(0xd7, 0xf4, 0xe3);
const GHOST_PAD_FILL: Color = Color::from_rgb_u8(0x2c, 0x2c, 0x31);
const GHOST_PAD_STROKE: Color = Color::from_rgb_u8(0xb0, 0xb0, 0xb0);
const GHOST_PAD_TEXT: Color = Color::from_rgb_u8(0xde, 0xde, 0xe2);
const STATE_PLAYING: Color = Color::from_rgb_u8(0x6e, 0xe7, 0xa0);
const STATE_PAUSED: Color = Color::from_rgb_u8(0xff, 0xd4, 0x79);
const STATE_OTHER: Color = Color::from_rgb_u8(0x8a, 0x8a, 0x8a);
const STATE_PENDING: Color = Color::from_rgb_u8(0xff, 0x9f, 0x43);

fn text_w(s: &str, size: f32) -> f32 {
    s.chars().count() as f32 * size * CHAR_W
}

fn state_color(state: &str) -> Color {
    if state.contains('→') {
        STATE_PENDING
    } else if state == "PLAYING" {
        STATE_PLAYING
    } else if state == "PAUSED" {
        STATE_PAUSED
    } else {
        STATE_OTHER
    }
}

/// Lay out a snapshot into a drawable [`Scene`].
pub fn layout(snap: &GraphSnapshot) -> Scene {
    // Pad ownership and cell parenthood decide which links rank the
    // children of any given bin.
    let mut pad_owner: HashMap<u32, CellId> = HashMap::new();
    let mut parent_of: HashMap<CellId, CellId> = HashMap::new();
    index_cells(&snap.root, None, &mut pad_owner, &mut parent_of);

    let ctx = Ctx {
        snap,
        pad_owner,
        parent_of,
    };

    let root = measure(&snap.root, &ctx, 0);

    let mut out = Out::default();
    draw_cell(&root, MARGIN, MARGIN, &mut out);
    route_edges(&ctx, &mut out);

    Scene {
        width: out.max_x + MARGIN,
        height: out.max_y + MARGIN,
        rects: out.rects,
        texts: out.texts,
        edges: out.edges,
        arrows: out.arrows,
        labels: out.labels,
        edge_paths: out.edge_paths,
        edge_hits: out.edge_hits,
    }
}

/// A ghost-pad chain (`elem.src -> ghost -> elem.sink`) carries identical
/// caps on every leg, so only one leg gets the caps chip: the leg between
/// two cells of the SAME bin, which is also the leg whose column gap is
/// widened to fit it. Inner legs (a bin's own ghost pad to or from one of
/// its direct children) stay unlabelled.
fn carries_label(ctx: &Ctx<'_>, src_owner: CellId, sink_owner: CellId) -> bool {
    ctx.parent_of.get(&src_owner) == ctx.parent_of.get(&sink_owner)
}

/// Identity of a cell = its address inside the borrowed snapshot.
type CellId = usize;

fn cell_id(cell: &GraphCell) -> CellId {
    cell as *const GraphCell as usize
}

fn index_cells(
    cell: &GraphCell,
    parent: Option<CellId>,
    pad_owner: &mut HashMap<u32, CellId>,
    parent_of: &mut HashMap<CellId, CellId>,
) {
    let id = cell_id(cell);
    if let Some(parent_id) = parent {
        parent_of.insert(id, parent_id);
    }
    for pad in cell.sink_pads.iter().chain(&cell.src_pads) {
        pad_owner.insert(pad.id, id);
    }
    for child in &cell.children {
        index_cells(child, Some(id), pad_owner, parent_of);
    }
}

struct Ctx<'a> {
    snap: &'a GraphSnapshot,
    pad_owner: HashMap<u32, CellId>,
    parent_of: HashMap<CellId, CellId>,
}

/// A measured cell: its size plus the relative positions of its children.
/// Pad boxes and header text are re-derived in the draw pass from the same
/// deterministic rules, so they aren't stored.
struct Measured<'a> {
    cell: &'a GraphCell,
    depth: usize,
    w: f32,
    h: f32,
    header_h: f32,
    /// Property lines actually shown (truncated to [`MAX_PROPS`]).
    props: Vec<String>,
    children: Vec<Measured<'a>>,
    /// Child origins relative to this cell's origin.
    child_pos: Vec<(f32, f32)>,
}

/// Widths of a cell's own pad boxes.
fn pad_box_w(name: &str, detail: &str) -> f32 {
    (text_w(name, PAD_SIZE).max(text_w(detail, PAD_SIZE - 2.0)) + 10.0).max(30.0)
}

fn pad_box_h(detail: &str) -> f32 {
    if detail.is_empty() { 16.0 } else { 25.0 }
}

fn pad_col_w(pads: &[fcastplaybin::graph::GraphPad]) -> f32 {
    pads.iter()
        .map(|pad| pad_box_w(&pad.name, &pad.detail))
        .fold(0.0, f32::max)
}

fn pad_col_h(pads: &[fcastplaybin::graph::GraphPad]) -> f32 {
    pads.iter().map(|pad| pad_box_h(&pad.detail) + 6.0).sum()
}

fn shown_props(cell: &GraphCell) -> Vec<String> {
    let mut props: Vec<String> = cell.properties.iter().take(MAX_PROPS).cloned().collect();
    if cell.properties.len() > MAX_PROPS {
        props.push(format!("+{} more…", cell.properties.len() - MAX_PROPS));
    }
    props
}

fn header_h(props: &[String]) -> f32 {
    // Title row + name row + property rows.
    6.0 + 17.0 + LINE_H + props.len() as f32 * LINE_H + 4.0
}

fn header_w(cell: &GraphCell, props: &[String]) -> f32 {
    let title = text_w(&cell.type_name, TITLE_SIZE) + 8.0 + text_w(&cell.state, TITLE_SIZE);
    let name = text_w(&cell.name, BODY_SIZE);
    let widest_prop = props
        .iter()
        .map(|prop| text_w(prop, BODY_SIZE))
        .fold(0.0, f32::max);
    // Extra breathing room so the longest line never touches the border.
    title.max(name).max(widest_prop) + 6.0
}

fn measure<'a>(cell: &'a GraphCell, ctx: &Ctx<'a>, depth: usize) -> Measured<'a> {
    let props = shown_props(cell);
    let header_h = header_h(&props);
    let sink_w = pad_col_w(&cell.sink_pads);
    let src_w = pad_col_w(&cell.src_pads);

    if !cell.is_bin {
        let pads_h = pad_col_h(&cell.sink_pads).max(pad_col_h(&cell.src_pads));
        let w = (header_w(cell, &props).max(sink_w + src_w + 28.0) + 2.0 * PADDING).max(96.0);
        let h = header_h + pads_h + PADDING;
        return Measured {
            cell,
            depth,
            w,
            h,
            header_h,
            props,
            children: Vec::new(),
            child_pos: Vec::new(),
        };
    }

    // ---- Bin: place children in ranked columns ----
    let children: Vec<Measured<'a>> = cell
        .children
        .iter()
        .map(|child| measure(child, ctx, depth + 1))
        .collect();

    let bin_id = cell_id(cell);
    let child_index: HashMap<CellId, usize> = cell
        .children
        .iter()
        .enumerate()
        .map(|(index, child)| (cell_id(child), index))
        .collect();

    // Links whose two endpoints belong to two different direct children of
    // this bin. Deeper links rank a deeper bin's children instead, and
    // ghost-pad internal legs connect the bin itself so they never rank.
    let mut ranking_edges: Vec<(usize, usize)> = Vec::new();
    for link in &ctx.snap.links {
        let (Some(&src_owner), Some(&sink_owner)) = (
            ctx.pad_owner.get(&link.src_pad),
            ctx.pad_owner.get(&link.sink_pad),
        ) else {
            continue;
        };
        if ctx.parent_of.get(&src_owner) == Some(&bin_id)
            && ctx.parent_of.get(&sink_owner) == Some(&bin_id)
            && src_owner != sink_owner
            && let (Some(&upstream), Some(&downstream)) =
                (child_index.get(&src_owner), child_index.get(&sink_owner))
        {
            ranking_edges.push((upstream, downstream));
        }
    }

    // Longest-path ranks, with bounded relaxation so a feedback loop
    // cannot hang.
    let child_count = children.len();
    let mut rank = vec![0usize; child_count];
    for _ in 0..child_count.max(1) {
        let mut changed = false;
        for &(upstream, downstream) in &ranking_edges {
            if rank[downstream] < rank[upstream] + 1 {
                rank[downstream] = rank[upstream] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let column_count = rank.iter().copied().max().map_or(0, |max| max + 1);
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); column_count];
    for (child, &child_rank) in rank.iter().enumerate() {
        columns[child_rank].push(child);
    }

    // Column-by-column vertical placement: order each column by the mean
    // center of its already-placed upstream neighbours to keep chains
    // roughly horizontal, then stack.
    let mut center_y = vec![0.0f32; child_count];
    let mut child_pos = vec![(0.0f32, 0.0f32); child_count];
    let mut col_w = vec![0.0f32; column_count];
    let mut content_h = 0.0f32;

    for (col, members) in columns.iter().enumerate() {
        let mut members = members.clone();
        if col > 0 {
            let upstream_center = |child: usize| -> f32 {
                let centers: Vec<f32> = ranking_edges
                    .iter()
                    .filter(|&&(upstream, downstream)| downstream == child && rank[upstream] < col)
                    .map(|&(upstream, _)| center_y[upstream])
                    .collect();
                if centers.is_empty() {
                    f32::MAX // unconnected: sink to the bottom
                } else {
                    centers.iter().sum::<f32>() / centers.len() as f32
                }
            };
            members
                .sort_by(|&left, &right| upstream_center(left).total_cmp(&upstream_center(right)));
        }

        let mut stack_y = 0.0f32;
        for &child in &members {
            child_pos[child].1 = stack_y;
            center_y[child] = stack_y + children[child].h / 2.0;
            stack_y += children[child].h + V_GAP;
            col_w[col] = col_w[col].max(children[child].w);
        }
        content_h = content_h.max((stack_y - V_GAP).max(0.0));
    }

    // Column gaps widen to fit the caps chips drawn after the source
    // column. Only links that will carry a chip reserve space (see
    // `carries_label`), and only chip-sized space, since the full caps
    // block appears on hover.
    let mut gap_after = vec![BASE_GAP; column_count.saturating_sub(1)];
    for link in &ctx.snap.links {
        let (Some(&src_owner), Some(&sink_owner)) = (
            ctx.pad_owner.get(&link.src_pad),
            ctx.pad_owner.get(&link.sink_pad),
        ) else {
            continue;
        };
        if ctx.parent_of.get(&src_owner) != Some(&bin_id)
            || !carries_label(ctx, src_owner, sink_owner)
        {
            continue;
        }
        let Some(&src_child) = child_index.get(&src_owner) else {
            continue;
        };
        let col = rank[src_child];
        if col < gap_after.len() {
            let chip_w = text_w(&caps_summary(&link.caps), CAPS_SIZE) + 12.0;
            gap_after[col] = gap_after[col].max(chip_w + 44.0);
        }
    }

    // Ghost pads occupy a lane just inside the bin's left/right border. The
    // extra room doubles as an edge corridor for their internal legs.
    let sink_lane = if sink_w > 0.0 { sink_w + 24.0 } else { 0.0 };
    let src_lane = if src_w > 0.0 { src_w + 24.0 } else { 0.0 };

    // No gap is added after the last column (gap_after has one entry fewer
    // than the columns), so the cursor ends at the content's right edge.
    let mut cursor_x = PADDING + sink_lane;
    for (col, members) in columns.iter().enumerate() {
        for &child in members {
            child_pos[child].0 = cursor_x;
        }
        cursor_x += col_w[col] + gap_after.get(col).copied().unwrap_or(0.0);
    }
    let content_w = cursor_x - PADDING - sink_lane;

    for pos in &mut child_pos {
        pos.1 += header_h + 8.0;
    }

    let ghost_h = pad_col_h(&cell.sink_pads).max(pad_col_h(&cell.src_pads));
    let w = (PADDING + sink_lane + content_w + src_lane + PADDING)
        .max(header_w(cell, &props) + 2.0 * PADDING)
        .max(96.0);
    let h = header_h + 8.0 + content_h.max(ghost_h) + PADDING;

    Measured {
        cell,
        depth,
        w,
        h,
        header_h,
        props,
        children,
        child_pos,
    }
}

/// Where an edge attaches to a pad, in absolute coordinates.
#[derive(Clone, Copy)]
struct PadGeom {
    /// Left edge of the pad box (edges terminate here on sink pads).
    x_in: f32,
    /// Right edge of the pad box (edges depart here on src pads).
    x_out: f32,
    y: f32,
}

#[derive(Default)]
struct Out {
    rects: Vec<SceneRect>,
    texts: Vec<SceneText>,
    edges: String,
    arrows: String,
    labels: Vec<SceneLabel>,
    edge_paths: Vec<SceneEdge>,
    edge_hits: Vec<SceneEdgeHit>,
    pad_geom: HashMap<u32, PadGeom>,
    /// Every cell box (element or bin), for obstacle-aware edge routing.
    obstacles: Vec<(f32, f32, f32, f32)>,
    max_x: f32,
    max_y: f32,
}

impl Out {
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, fill: Color, stroke: Color) {
        self.max_x = self.max_x.max(x + w);
        self.max_y = self.max_y.max(y + h);
        self.rects.push(SceneRect {
            x,
            y,
            w,
            h,
            fill,
            stroke,
        });
    }

    fn text(&mut self, x: f32, y: f32, size: f32, color: Color, text: &str) {
        if text.is_empty() {
            return;
        }
        self.max_x = self.max_x.max(x + text_w(text, size));
        self.max_y = self.max_y.max(y + size * 1.3);
        self.texts.push(SceneText {
            x,
            y,
            size,
            color,
            text: text.to_string(),
        });
    }
}

fn draw_cell(measured: &Measured<'_>, x: f32, y: f32, out: &mut Out) {
    let cell = measured.cell;
    let (fill, stroke) = if cell.is_bin {
        (BIN_FILL[measured.depth % 2], BIN_STROKE)
    } else {
        (ELEM_FILL, ELEM_STROKE)
    };
    out.rect(x, y, measured.w, measured.h, fill, stroke);
    out.obstacles.push((x, y, measured.w, measured.h));

    // Header: type and state, then name, then properties.
    let text_x = x + PADDING;
    let mut text_y = y + 5.0;
    out.text(text_x, text_y, TITLE_SIZE, TITLE_COLOR, &cell.type_name);
    out.text(
        text_x + text_w(&cell.type_name, TITLE_SIZE) + 8.0,
        text_y + 1.0,
        TITLE_SIZE - 1.0,
        state_color(&cell.state),
        &cell.state,
    );
    text_y += 17.0;
    out.text(text_x, text_y, BODY_SIZE, NAME_COLOR, &cell.name);
    text_y += LINE_H;
    for prop in &measured.props {
        out.text(text_x, text_y, BODY_SIZE, PROP_COLOR, prop);
        text_y += LINE_H;
    }

    // Pads. Elements keep them tight under the header while bins push them
    // to the border lanes so they read as boundary connectors.
    let pad_top = y + measured.header_h + if cell.is_bin { 8.0 } else { 2.0 };
    let sink_x = x + if cell.is_bin { 4.0 } else { 6.0 };
    draw_pad_col(&cell.sink_pads, sink_x, pad_top, true, out);
    let src_col_w = pad_col_w(&cell.src_pads);
    let src_x = x + measured.w - src_col_w - if cell.is_bin { 4.0 } else { 6.0 };
    draw_pad_col(&cell.src_pads, src_x, pad_top, false, out);

    for (child, (child_dx, child_dy)) in measured.children.iter().zip(&measured.child_pos) {
        draw_cell(child, x + child_dx, y + child_dy, out);
    }
}

fn draw_pad_col(
    pads: &[fcastplaybin::graph::GraphPad],
    x: f32,
    top: f32,
    is_sink: bool,
    out: &mut Out,
) {
    // Sink pads align flush left and src pads flush right, so every pad
    // sits on its cell's border and wires (and their caps chips) depart
    // from the border instead of from inside the box.
    let col_w = pad_col_w(pads);
    let mut box_y = top;
    for pad in pads {
        let box_w = pad_box_w(&pad.name, &pad.detail);
        let box_h = pad_box_h(&pad.detail);
        let box_x = if is_sink { x } else { x + col_w - box_w };
        let (fill, stroke, text) = if pad.ghost {
            (GHOST_PAD_FILL, GHOST_PAD_STROKE, GHOST_PAD_TEXT)
        } else if is_sink {
            (SINK_PAD_FILL, SINK_PAD_STROKE, SINK_PAD_TEXT)
        } else {
            (SRC_PAD_FILL, SRC_PAD_STROKE, SRC_PAD_TEXT)
        };
        out.rect(box_x, box_y, box_w, box_h, fill, stroke);
        out.text(box_x + 5.0, box_y + 2.0, PAD_SIZE, text, &pad.name);
        if !pad.detail.is_empty() {
            out.text(
                box_x + 5.0,
                box_y + 13.0,
                PAD_SIZE - 2.0,
                PROP_COLOR,
                &pad.detail,
            );
        }
        out.pad_geom.insert(
            pad.id,
            PadGeom {
                x_in: box_x,
                x_out: box_x + box_w,
                y: box_y + box_h / 2.0,
            },
        );
        box_y += box_h + 6.0;
    }
}

/// Caps line pitch inside the hover card.
const CAPS_LINE_H: f32 = CAPS_SIZE + 2.0;
/// Wrap width (in characters) for packed caps fields.
const CAPS_WRAP: usize = 56;
/// Height of an always-visible caps chip.
const CHIP_H: f32 = 16.0;

/// The one-line chip text: media type plus the parameters someone actually
/// scans for (geometry and rates). Everything else lives in the hover card.
fn caps_summary(lines: &[String]) -> String {
    let mut fields: HashMap<&str, &str> = HashMap::new();
    let mut media_lines = lines.iter().filter(|line| !line.starts_with(' '));
    let mut summary = media_lines.next().cloned().unwrap_or_default();
    for line in lines {
        if let Some((key, value)) = line.strip_prefix("  ").and_then(|f| f.split_once(": ")) {
            fields.entry(key).or_insert(value);
        }
    }
    if let (Some(width), Some(height)) = (fields.get("width"), fields.get("height")) {
        summary.push_str(&format!(" {width}×{height}"));
    }
    if let Some(format) = fields.get("format") {
        summary.push_str(&format!(" {format}"));
    }
    if let Some(framerate) = fields.get("framerate") {
        summary.push_str(&format!(" @{framerate}"));
    }
    if let Some(rate) = fields.get("rate") {
        summary.push_str(&format!(" {rate}Hz"));
    }
    if let Some(channels) = fields.get("channels") {
        summary.push_str(&format!(" {channels}ch"));
    }
    let extra_structures = media_lines.count();
    if extra_structures > 0 {
        summary.push_str(&format!(" +{extra_structures}"));
    }
    summary
}

/// Re-flow the walker's one-field-per-line caps into a compact block. The
/// media-type line stays on its own and the fields pack greedily into
/// wrapped lines, shrinking a video caps block from about 15 lines to 5.
fn wrap_caps(lines: &[String]) -> Vec<String> {
    let mut wrapped: Vec<String> = Vec::new();
    let mut packed = String::new();
    for line in lines {
        if let Some(field) = line.strip_prefix("  ") {
            if packed.is_empty() {
                packed = format!("  {field}");
            } else if packed.chars().count() + field.chars().count() + 2 <= CAPS_WRAP {
                packed.push_str(", ");
                packed.push_str(field);
            } else {
                wrapped.push(std::mem::take(&mut packed));
                packed = format!("  {field}");
            }
        } else {
            // A media-type line always gets its own line.
            if !packed.is_empty() {
                wrapped.push(std::mem::take(&mut packed));
            }
            wrapped.push(line.clone());
        }
    }
    if !packed.is_empty() {
        wrapped.push(packed);
    }
    wrapped
}

/// True when every segment of an orthogonal route avoids the cell boxes.
///
/// Two kinds of box are exempt. A box containing BOTH edge endpoints is an
/// ancestor bin of the whole edge, and routing inside it is the point. The
/// source or sink cell's own box is exempt for the first or last segment,
/// because pads sit a few pixels inside their cell and the stub must pass
/// through that strip to reach the pad.
fn route_clear(
    obstacles: &[(f32, f32, f32, f32)],
    corners: &[(f32, f32)],
    edge_src: (f32, f32),
    edge_sink: (f32, f32),
) -> bool {
    let last_segment = corners.len().saturating_sub(2);
    corners.windows(2).enumerate().all(|(segment, ends)| {
        let [(from_x, from_y), (to_x, to_y)] = ends else {
            return true;
        };
        let (lo_x, hi_x) = (from_x.min(*to_x), from_x.max(*to_x));
        let (lo_y, hi_y) = (from_y.min(*to_y), from_y.max(*to_y));
        !obstacles.iter().any(|&(box_x, box_y, box_w, box_h)| {
            let contains = |point: (f32, f32)| {
                point.0 > box_x - 0.5
                    && point.0 < box_x + box_w + 0.5
                    && point.1 > box_y - 0.5
                    && point.1 < box_y + box_h + 0.5
            };
            let overlaps =
                hi_x > box_x && lo_x < box_x + box_w && hi_y > box_y && lo_y < box_y + box_h;
            let ancestor = contains(edge_src) && contains(edge_sink);
            let source_stub = segment == 0 && contains(edge_src);
            let sink_stub = segment == last_segment && contains(edge_sink);
            overlaps && !ancestor && !source_stub && !sink_stub
        })
    })
}

/// Route every link orthogonally between its pad geometries, avoiding the
/// cell boxes. The router tries, in order: the sink-height lane through any
/// source-side corridor slot, the source-height lane through any sink-side
/// corridor slot, then every combination of corridor slots and clear
/// channels between rows. Only when nothing at all is clear does it accept
/// a crossing. Links between two cells of the same bin also carry their
/// caps chip (see [`carries_label`]). Chips are pushed below earlier chips
/// they would overlap.
fn route_edges(ctx: &Ctx<'_>, out: &mut Out) {
    use std::fmt::Write as _;

    // Corridor key (quantized departure x) mapped to used slots, so wires
    // departing the same column spread across parallel verticals.
    let mut corridors: HashMap<i32, u32> = HashMap::new();
    // Already-placed chips, for collision placement.
    let mut chips: Vec<(f32, f32, f32, f32)> = Vec::new();

    let mut links: Vec<_> = ctx
        .snap
        .links
        .iter()
        .filter_map(|link| {
            let src = *out.pad_geom.get(&link.src_pad)?;
            let sink = *out.pad_geom.get(&link.sink_pad)?;
            Some((link, src, sink))
        })
        .collect();
    // Top-to-bottom so the chip de-overlap pushes labels downwards in
    // reading order.
    links.sort_by(|left, right| left.1.y.total_cmp(&right.1.y));

    for (link, src, sink) in links {
        let slot = corridors.entry((src.x_out / 24.0) as i32).or_insert(0);
        let stagger = (*slot % 8) as f32 * 7.0;
        *slot += 1;

        let (src_x, src_y) = (src.x_out, src.y);
        let (sink_x, sink_y) = (sink.x_in, sink.y);
        let src_end = (src_x, src_y);
        let sink_end = (sink_x, sink_y);
        // The line stops short of the pad so the arrowhead has room.
        let line_end_x = sink_x - 7.0;
        let obstacles = &out.obstacles;
        let clear = |corners: &[(f32, f32)]| route_clear(obstacles, corners, src_end, sink_end);

        let corners: Vec<(f32, f32)> = if sink_x >= src_x + 24.0 {
            // Corridor slots on both sides, the wire's own stagger slot
            // first so parallel wires spread out by default.
            let src_slots: Vec<f32> = std::iter::once(stagger)
                .chain((0..8).map(|slot| slot as f32 * 7.0))
                .map(|offset| (src_x + 12.0 + offset).min(sink_x - 8.0))
                .collect();
            let sink_slots: Vec<f32> = (0..4)
                .map(|slot| (sink_x - 12.0 - slot as f32 * 7.0).max(src_x + 12.0))
                .collect();

            let sink_lane = src_slots
                .iter()
                .map(|&corridor_x| {
                    vec![
                        (src_x, src_y),
                        (corridor_x, src_y),
                        (corridor_x, sink_y),
                        (line_end_x, sink_y),
                    ]
                })
                .find(|route| clear(route));
            let src_lane = || {
                sink_slots
                    .iter()
                    .map(|&corridor_x| {
                        vec![
                            (src_x, src_y),
                            (corridor_x, src_y),
                            (corridor_x, sink_y),
                            (line_end_x, sink_y),
                        ]
                    })
                    .find(|route| clear(route))
            };

            if let Some(route) = sink_lane.or_else(src_lane) {
                route
            } else {
                // Both lanes blocked: try every corridor combination with a
                // channel between rows, nearest channel first.
                let span_lo = src_x + 12.0;
                let span_hi = sink_x - 8.0;
                let target_y = (src_y + sink_y) / 2.0;
                let mut channels: Vec<f32> = Vec::new();
                for &(box_x, box_y, box_w, box_h) in obstacles {
                    if span_hi > box_x && span_lo < box_x + box_w {
                        channels.push(box_y - 10.0 - stagger);
                        channels.push(box_y + box_h + 10.0 + stagger);
                    }
                }
                channels.sort_by(|left, right| {
                    (left - target_y).abs().total_cmp(&(right - target_y).abs())
                });

                let mut found = None;
                'search: for &channel_y in &channels {
                    for &src_corridor in &src_slots {
                        for &sink_corridor in &sink_slots {
                            if sink_corridor <= src_corridor {
                                continue;
                            }
                            let route = vec![
                                (src_x, src_y),
                                (src_corridor, src_y),
                                (src_corridor, channel_y),
                                (sink_corridor, channel_y),
                                (sink_corridor, sink_y),
                                (line_end_x, sink_y),
                            ];
                            if clear(&route) {
                                found = Some(route);
                                break 'search;
                            }
                        }
                    }
                }
                // With no clear route at all, accept the crossing.
                found.unwrap_or_else(|| {
                    vec![
                        (src_x, src_y),
                        (src_slots[0], src_y),
                        (src_slots[0], sink_y),
                        (line_end_x, sink_y),
                    ]
                })
            }
        } else {
            // Backward link: drop below both endpoints and come back.
            let below_y = src_y.max(sink_y) + 36.0 + stagger;
            let exit_x = src_x + 10.0 + stagger;
            let entry_x = sink_x - 12.0 - stagger;
            vec![
                (src_x, src_y),
                (exit_x, src_y),
                (exit_x, below_y),
                (entry_x, below_y),
                (entry_x, sink_y),
                (line_end_x, sink_y),
            ]
        };

        // Emit the batched path, the per-wire path, and a thin hit-zone
        // rect per segment for hover highlighting.
        let edge_idx = out.edge_paths.len();
        let mut path_cmds = String::new();
        for (corner, (corner_x, corner_y)) in corners.iter().enumerate() {
            let op = if corner == 0 { 'M' } else { 'L' };
            let _ = write!(path_cmds, "{op} {corner_x:.1} {corner_y:.1} ");
            out.max_x = out.max_x.max(*corner_x);
            out.max_y = out.max_y.max(*corner_y);
        }
        out.edges.push_str(&path_cmds);
        for segment in corners.windows(2) {
            let [(from_x, from_y), (to_x, to_y)] = segment else {
                continue;
            };
            const HIT_PAD: f32 = 3.5;
            out.edge_hits.push(SceneEdgeHit {
                x: from_x.min(*to_x) - HIT_PAD,
                y: from_y.min(*to_y) - HIT_PAD,
                w: (from_x - to_x).abs() + 2.0 * HIT_PAD,
                h: (from_y - to_y).abs() + 2.0 * HIT_PAD,
                edge: edge_idx,
            });
        }
        let arrow = format!(
            "M {sink_x:.1} {sink_y:.1} L {line_end_x:.1} {:.1} L {line_end_x:.1} {:.1} Z ",
            sink_y - 3.5,
            sink_y + 3.5
        );
        out.arrows.push_str(&arrow);
        out.edge_paths.push(SceneEdge {
            commands: path_cmds,
            arrow,
        });

        // One chip per wire: only the same-level leg of a ghost chain.
        let labelled = !link.caps.is_empty()
            && match (
                ctx.pad_owner.get(&link.src_pad),
                ctx.pad_owner.get(&link.sink_pad),
            ) {
                (Some(&src_owner), Some(&sink_owner)) => carries_label(ctx, src_owner, sink_owner),
                _ => false,
            };
        if labelled {
            let summary = caps_summary(&link.caps);
            let detail_lines = wrap_caps(&link.caps);
            let detail_w = detail_lines
                .iter()
                .map(|line| text_w(line, CAPS_SIZE))
                .fold(0.0, f32::max)
                + 12.0;
            let detail_h = detail_lines.len() as f32 * CAPS_LINE_H + 8.0;

            let chip_w = text_w(&summary, CAPS_SIZE) + 12.0;
            let chip_x = src_x + 14.0 + stagger;
            let mut chip_y = src_y.min(sink_y) + 4.0;
            // Push below every earlier chip this one would intersect. Chips
            // are visited top-to-bottom, so this converges.
            loop {
                let blocking = chips.iter().find(|&&(other_x, other_y, other_w, other_h)| {
                    chip_x < other_x + other_w
                        && other_x < chip_x + chip_w
                        && chip_y < other_y + other_h
                        && other_y < chip_y + CHIP_H
                });
                match blocking {
                    Some(&(_, other_y, _, other_h)) => chip_y = other_y + other_h + 4.0,
                    None => break,
                }
            }
            chips.push((chip_x, chip_y, chip_w, CHIP_H));

            out.max_x = out.max_x.max(chip_x + chip_w);
            out.max_y = out.max_y.max(chip_y + CHIP_H);
            out.labels.push(SceneLabel {
                x: chip_x,
                y: chip_y,
                w: chip_w,
                h: CHIP_H,
                summary,
                detail: detail_lines.join("\n"),
                detail_w,
                detail_h,
                edge: edge_idx,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use fcastplaybin::graph::{GraphLink, GraphPad};
    use gst::prelude::*;

    use super::*;

    fn pad(id: u32, name: &str) -> GraphPad {
        GraphPad {
            id,
            name: name.to_string(),
            detail: String::new(),
            ghost: false,
        }
    }

    fn elem(name: &str, sink: Option<u32>, src: Option<u32>) -> GraphCell {
        GraphCell {
            name: name.to_string(),
            type_name: name.to_string(),
            state: "PLAYING".to_string(),
            properties: vec![],
            sink_pads: sink.map(|id| pad(id, "sink")).into_iter().collect(),
            src_pads: src.map(|id| pad(id, "src")).into_iter().collect(),
            children: vec![],
            is_bin: false,
        }
    }

    fn root(children: Vec<GraphCell>) -> GraphCell {
        GraphCell {
            name: "pipeline".to_string(),
            type_name: "GstPipeline".to_string(),
            state: "PLAYING".to_string(),
            is_bin: true,
            children,
            ..Default::default()
        }
    }

    fn link(src: u32, sink: u32) -> GraphLink {
        GraphLink {
            src_pad: src,
            sink_pad: sink,
            caps: vec!["video/x-raw".to_string()],
        }
    }

    fn text_x(scene: &Scene, s: &str) -> f32 {
        scene
            .texts
            .iter()
            .find(|t| t.text == s)
            .unwrap_or_else(|| panic!("no text {s:?}"))
            .x
    }

    #[test]
    fn chain_flows_left_to_right() {
        let snap = GraphSnapshot {
            root: root(vec![
                elem("a", None, Some(1)),
                elem("c", Some(4), None),
                elem("b", Some(2), Some(3)),
            ]),
            links: vec![link(1, 2), link(3, 4)],
        };
        let scene = layout(&snap);

        // Rank order wins over child order: a < b < c on the x axis.
        let (ax, bx, cx) = (
            text_x(&scene, "a"),
            text_x(&scene, "b"),
            text_x(&scene, "c"),
        );
        assert!(ax < bx && bx < cx, "ax={ax} bx={bx} cx={cx}");

        // Two forward edges, two arrowheads, one caps chip per wire.
        assert_eq!(scene.edges.matches('M').count(), 2);
        assert_eq!(scene.arrows.matches('Z').count(), 2);
        assert_eq!(scene.labels.len(), 2);
        assert!(
            scene
                .labels
                .iter()
                .all(|l| l.summary.starts_with("video/x-raw"))
        );
        assert!(scene.width > 0.0 && scene.height > 0.0);
    }

    #[test]
    fn bin_contains_its_children() {
        let mut bin = root(vec![elem("inner", Some(2), Some(3))]);
        bin.name = "wrap".to_string();
        bin.type_name = "bin".to_string();
        bin.sink_pads = vec![GraphPad {
            id: 10,
            name: "sink".to_string(),
            detail: String::new(),
            ghost: true,
        }];

        let snap = GraphSnapshot {
            root: root(vec![elem("src", None, Some(1)), bin]),
            links: vec![link(1, 10), link(10, 2)],
        };
        let scene = layout(&snap);

        // The bin rect fully contains the inner element's rect. Rects are
        // emitted parent-first, so find them by size ordering: the
        // pipeline rect is the largest, the bin next, inner smallest.
        let mut areas: Vec<&SceneRect> = scene.rects.iter().collect();
        areas.sort_by(|a, b| (b.w * b.h).total_cmp(&(a.w * a.h)));
        let pipeline = areas[0];
        for r in &scene.rects {
            assert!(
                r.x >= pipeline.x - 0.5 && r.y >= pipeline.y - 0.5,
                "rect {r:?} outside pipeline {pipeline:?}"
            );
            assert!(
                r.x + r.w <= pipeline.x + pipeline.w + 0.5,
                "rect {r:?} exceeds pipeline right edge {pipeline:?}"
            );
            assert!(
                r.y + r.h <= pipeline.y + pipeline.h + 0.5,
                "rect {r:?} exceeds pipeline bottom edge {pipeline:?}"
            );
        }

        // Ghost link into the bin and its internal leg are both routed…
        assert_eq!(scene.edges.matches('M').count(), 2);
        // …but the wire's caps are labelled exactly once (on the outer,
        // same-level leg), not repeated per leg of the ghost chain.
        assert_eq!(scene.labels.len(), 1);
    }

    /// Parse the batched edges path back into polylines and assert that no
    /// segment crosses an element box. Bins are skipped because edges
    /// legitimately route inside them, and segment ends are trimmed so the
    /// stubs touching pad boxes do not count.
    fn assert_no_element_crossings(scene: &Scene) {
        let mut polylines: Vec<Vec<(f32, f32)>> = Vec::new();
        let mut pending: Vec<f32> = Vec::new();
        for token in scene.edges.split_whitespace() {
            match token {
                "M" => polylines.push(Vec::new()),
                "L" => {}
                value => {
                    pending.push(value.parse().unwrap());
                    if pending.len() == 2 {
                        polylines.last_mut().unwrap().push((pending[0], pending[1]));
                        pending.clear();
                    }
                }
            }
        }

        let elements: Vec<&SceneRect> =
            scene.rects.iter().filter(|r| r.fill == ELEM_FILL).collect();
        for polyline in &polylines {
            let last_segment = polyline.len().saturating_sub(2);
            for (index, segment) in polyline.windows(2).enumerate() {
                // The first and last segments are pad stubs and may pass
                // through their own cell's border strip.
                if index == 0 || index == last_segment {
                    continue;
                }
                let [(from_x, from_y), (to_x, to_y)] = segment else {
                    continue;
                };
                const TRIM: f32 = 8.0;
                let (mut lo_x, mut hi_x) = (from_x.min(*to_x), from_x.max(*to_x));
                let (mut lo_y, mut hi_y) = (from_y.min(*to_y), from_y.max(*to_y));
                if hi_x - lo_x > 2.0 * TRIM {
                    lo_x += TRIM;
                    hi_x -= TRIM;
                }
                if hi_y - lo_y > 2.0 * TRIM {
                    lo_y += TRIM;
                    hi_y -= TRIM;
                }
                for rect in &elements {
                    assert!(
                        !(hi_x > rect.x
                            && lo_x < rect.x + rect.w
                            && hi_y > rect.y
                            && lo_y < rect.y + rect.h),
                        "segment {segment:?} crosses element box {rect:?}"
                    );
                }
            }
        }
    }

    /// A demuxer-like fan-out: video runs through two parser stages while
    /// meta and subtitle wires skip straight across to a tall queue. The
    /// straight lanes for the skip wires are blocked by the parser column,
    /// so they must route around it.
    #[test]
    fn dense_rows_route_around_elements() {
        let mut demux = elem("demux", Some(10), Some(1));
        demux.src_pads.push(pad(2, "meta"));
        demux.src_pads.push(pad(3, "sub"));
        let mut parse = elem("parse", Some(4), Some(5));
        parse.properties =
            vec!["config-interval=0123456789012345678901234567890123456789".to_string()];
        let mut filter = elem("filter", Some(6), Some(7));
        filter.properties =
            vec!["caps=video/x-h264, stream-format=(string)avc, alignment=au".to_string()];
        let mut queue = elem("mq", Some(8), None);
        queue.sink_pads.push(pad(9, "sink_1"));
        queue.sink_pads.push(pad(11, "sink_2"));

        let snap = GraphSnapshot {
            root: root(vec![demux, parse, filter, queue]),
            links: vec![link(1, 4), link(5, 6), link(7, 8), link(2, 9), link(3, 11)],
        };
        let scene = layout(&snap);
        assert_eq!(scene.edges.matches('M').count(), 5);
        assert_no_element_crossings(&scene);
    }

    /// A skip-level edge (a to c with b in the middle column, all three in
    /// the same row) must not run straight through b's box. Both the
    /// sink-height and source-height lanes are blocked, so the router picks
    /// a channel above or below b, a 6-point path instead of a 4-point one.
    #[test]
    fn skip_edge_routes_around_boxes() {
        let mut a = elem("a", None, Some(1));
        a.src_pads.push(pad(5, "src2"));
        let mut c = elem("c", Some(4), None);
        c.sink_pads.push(pad(6, "sink2"));
        // b spans both pad rows, so the skip edge can't sneak under its box.
        let mut b = elem("b", Some(2), Some(3));
        b.sink_pads.push(pad(7, "sink2"));
        b.src_pads.push(pad(8, "src2"));
        let snap = GraphSnapshot {
            root: root(vec![a, b, c]),
            links: vec![link(1, 2), link(3, 4), link(5, 6)],
        };
        let scene = layout(&snap);
        // Two direct edges with 3 segments each, one channel route with 5.
        assert_eq!(scene.edges.matches('M').count(), 3);
        assert_eq!(scene.edges.matches('L').count(), 11, "{}", scene.edges);
        assert_no_element_crossings(&scene);
    }

    #[test]
    fn backward_link_routes_without_panic() {
        let snap = GraphSnapshot {
            root: root(vec![
                elem("a", Some(4), Some(1)),
                elem("b", Some(2), Some(3)),
            ]),
            links: vec![link(1, 2), link(3, 4)],
        };
        let scene = layout(&snap);
        assert_eq!(scene.edges.matches('M').count(), 2);
    }

    #[test]
    fn empty_pipeline_is_a_single_box() {
        let scene = layout(&GraphSnapshot {
            root: root(vec![]),
            links: vec![],
        });
        assert_eq!(scene.rects.len(), 1);
        assert!(scene.edges.is_empty());
    }

    /// End-to-end over a real pipeline: identity -> [bin: identity, ghost
    /// pads on both sides] -> identity, walked by fcastplaybin's snapshot
    /// and laid out here. Guards the walker/layout contract (pad ids line
    /// up, ghost links route, ranks flow left to right). All-identity
    /// because the static test build trims the fake/test elements.
    #[test]
    fn real_pipeline_snapshot_lays_out() {
        gst::init().unwrap();

        let pipeline = gst::Pipeline::with_name("p");
        let src = gst::ElementFactory::make("identity")
            .name("head")
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("identity")
            .name("tail")
            .build()
            .unwrap();

        let bin = gst::Bin::with_name("wrap");
        let identity = gst::ElementFactory::make("identity")
            .name("mid")
            .build()
            .unwrap();
        bin.add(&identity).unwrap();
        bin.add_pad(&gst::GhostPad::with_target(&identity.static_pad("sink").unwrap()).unwrap())
            .unwrap();
        bin.add_pad(
            &gst::GhostPad::builder_with_target(&identity.static_pad("src").unwrap())
                .unwrap()
                .name("src")
                .build(),
        )
        .unwrap();

        pipeline.add_many([&src, bin.upcast_ref(), &sink]).unwrap();
        gst::Element::link_many([&src, bin.upcast_ref(), &sink]).unwrap();

        let snap = fcastplaybin::graph::snapshot(pipeline.upcast_ref());
        let scene = layout(&snap);

        // head.src -> ghost sink -> mid.sink, mid.src -> ghost src ->
        // tail.sink: four routed edges with arrowheads.
        assert_eq!(scene.edges.matches('M').count(), 4, "{}", scene.edges);
        assert_eq!(scene.arrows.matches('Z').count(), 4);

        let (sx, ix, kx) = (
            text_x(&scene, "head"),
            text_x(&scene, "mid"),
            text_x(&scene, "tail"),
        );
        assert!(sx < ix && ix < kx, "sx={sx} ix={ix} kx={kx}");
        assert!(scene.width > 0.0 && scene.height > 0.0);
    }

    #[test]
    fn siblings_do_not_overlap() {
        // Fan-out: one source feeding three parallel sinks stacked in a column.
        let snap = GraphSnapshot {
            root: root(vec![
                elem("src", None, Some(1)),
                elem("s1", Some(2), None),
                elem("s2", Some(3), None),
                elem("s3", Some(4), None),
            ]),
            links: vec![link(1, 2), link(1, 3), link(1, 4)],
        };
        let scene = layout(&snap);
        // The three sink boxes share a column: same x, disjoint y ranges.
        let mut sinks: Vec<&SceneRect> = scene
            .rects
            .iter()
            .filter(|r| {
                scene
                    .texts
                    .iter()
                    .any(|t| t.text.starts_with('s') && t.text.len() == 2 && t.x == r.x + PADDING)
            })
            .collect();
        assert_eq!(sinks.len(), 3, "expected 3 sink boxes");
        sinks.sort_by(|a, b| a.y.total_cmp(&b.y));
        for pair in sinks.windows(2) {
            assert!(pair[0].y + pair[0].h <= pair[1].y, "overlap: {pair:?}");
        }
    }
}
