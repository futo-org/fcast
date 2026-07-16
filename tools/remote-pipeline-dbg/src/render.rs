//! Turn graphviz `dot -Tjson` output into flat drawing primitives.
//!
//! We let graphviz do the *layout* (the hard part) but never let it rasterize
//! or emit SVG. The JSON output is a list of xdot draw operations (polygons,
//! b-splines, polylines, ellipses, text) in graphviz's bottom-left coordinate
//! space. We translate those into SVG path strings + positioned text that the
//! Slint UI renders natively, so the result is crisp at any zoom and cheap to
//! produce.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

/// A batch of geometry sharing one fill/stroke, encoded as an SVG path string.
pub struct RenderPath {
    pub commands: String,
    /// Packed RGBA, or `None` for no fill.
    pub fill: Option<[u8; 4]>,
    /// Packed RGBA, or `None` for no stroke.
    pub stroke: Option<[u8; 4]>,
    pub stroke_width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

/// A single text label. `x`/`y` is graphviz's anchor point (Slint coordinates);
/// the UI offsets horizontally by the label's own width according to `align`.
/// We deliberately don't clamp to graphviz's cell width: Slint's font metrics
/// differ, so clamping would truncate labels. Natural sizing keeps them legible.
pub struct RenderText {
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub text: String,
    pub color: [u8; 4],
    pub align: TextAlign,
}

#[derive(Default)]
pub struct RenderGraph {
    pub width: f32,
    pub height: f32,
    pub paths: Vec<RenderPath>,
    pub texts: Vec<RenderText>,
}

#[derive(Deserialize)]
struct Graph {
    #[serde(default)]
    bb: String,
    #[serde(default, rename = "_draw_")]
    draw: Vec<Value>,
    #[serde(default)]
    objects: Vec<Object>,
    #[serde(default)]
    edges: Vec<Object>,
}

/// Nodes, clusters and edges all carry the same set of draw arrays; we only
/// care about the drawing ops, so one struct covers them all.
#[derive(Deserialize)]
struct Object {
    #[serde(default, rename = "_draw_")]
    draw: Vec<Value>,
    #[serde(default, rename = "_ldraw_")]
    ldraw: Vec<Value>,
    #[serde(default, rename = "_hdraw_")]
    hdraw: Vec<Value>,
    #[serde(default, rename = "_tdraw_")]
    tdraw: Vec<Value>,
    #[serde(default, rename = "_hldraw_")]
    hldraw: Vec<Value>,
    #[serde(default, rename = "_tldraw_")]
    tldraw: Vec<Value>,
}

/// Grouping key so that all geometry sharing a fill/stroke collapses into a
/// single `Path` element regardless of how many nodes/edges the graph has.
#[derive(Clone, PartialEq, Eq, Hash)]
struct PathKey {
    fill: Option<[u8; 4]>,
    stroke: Option<[u8; 4]>,
    /// Stroke width in 1/100 px, so the key stays hashable.
    width_centi: u32,
}

/// Mutable pen state while walking a draw-op list.
struct Pen {
    stroke: Option<[u8; 4]>,
    fill: Option<[u8; 4]>,
    font_size: f32,
    line_width: f32,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            stroke: Some([0, 0, 0, 255]),
            fill: None,
            font_size: 14.0,
            line_width: 1.0,
        }
    }
}

pub fn parse(json: &[u8]) -> serde_json::Result<RenderGraph> {
    let graph: Graph = serde_json::from_slice(json)?;

    // bb = "llx,lly,urx,ury". Height is used to flip the y axis.
    let bb: Vec<f32> = graph
        .bb
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let (llx, urx, ury) = match bb.as_slice() {
        [llx, _lly, urx, ury] => (*llx, *urx, *ury),
        _ => (0.0, 0.0, 0.0),
    };

    let mut builder = Builder {
        flip_y: ury,
        offset_x: llx,
        groups: HashMap::new(),
        order: Vec::new(),
        texts: Vec::new(),
    };

    builder.walk(&graph.draw);
    for obj in graph.objects.iter().chain(graph.edges.iter()) {
        builder.walk(&obj.draw);
        builder.walk(&obj.ldraw);
        builder.walk(&obj.hdraw);
        builder.walk(&obj.tdraw);
        builder.walk(&obj.hldraw);
        builder.walk(&obj.tldraw);
    }

    Ok(RenderGraph {
        width: urx - llx,
        height: ury,
        paths: builder.take_paths(),
        texts: builder.texts,
    })
}

struct Builder {
    flip_y: f32,
    offset_x: f32,
    groups: HashMap<PathKey, String>,
    /// Preserve first-seen order so draw order (z-order) is stable.
    order: Vec<PathKey>,
    texts: Vec<RenderText>,
}

impl Builder {
    fn x(&self, x: f32) -> f32 {
        x - self.offset_x
    }

    fn y(&self, y: f32) -> f32 {
        self.flip_y - y
    }

    fn walk(&mut self, ops: &[Value]) {
        let mut pen = Pen::default();

        for op in ops {
            let Some(name) = op.get("op").and_then(Value::as_str) else {
                continue;
            };

            match name {
                // Pen (stroke) colour.
                "c" => pen.stroke = op.get("color").and_then(parse_color),
                // Fill colour.
                "C" => pen.fill = op.get("color").and_then(parse_color),
                // Font.
                "F" => {
                    if let Some(size) = op.get("size").and_then(Value::as_f64) {
                        pen.font_size = size as f32;
                    }
                }
                // Style, e.g. setlinewidth. Dashes aren't representable on a
                // Slint Path, so we only pick up the line width.
                "S" => {
                    if let Some(w) = op
                        .get("style")
                        .and_then(Value::as_str)
                        .and_then(|s| s.strip_prefix("setlinewidth("))
                        .and_then(|s| s.strip_suffix(')'))
                        .and_then(|s| s.parse::<f32>().ok())
                    {
                        pen.line_width = w;
                    }
                }
                // Polygon: 'P' filled, 'p' outline only.
                "P" | "p" => {
                    let cmds = self.polygon(op, true);
                    self.push(&pen, name == "P", cmds);
                }
                // Polyline (never filled).
                "L" => {
                    let cmds = self.polygon(op, false);
                    self.push(&pen, false, cmds);
                }
                // B-spline: 'B' filled, 'b' outline only.
                "B" | "b" => {
                    let cmds = self.bspline(op, name == "B");
                    self.push(&pen, name == "B", cmds);
                }
                // Ellipse: 'E' filled, 'e' outline only.
                "E" | "e" => {
                    let cmds = self.ellipse(op);
                    self.push(&pen, name == "E", cmds);
                }
                // Text.
                "T" => self.text(&pen, op),
                _ => {}
            }
        }
    }

    fn push(&mut self, pen: &Pen, filled: bool, commands: String) {
        if commands.is_empty() {
            return;
        }
        let key = PathKey {
            fill: if filled { pen.fill } else { None },
            stroke: pen.stroke,
            width_centi: (pen.line_width * 100.0).round() as u32,
        };
        if !self.groups.contains_key(&key) {
            self.order.push(key.clone());
        }
        let buf = self.groups.entry(key).or_default();
        buf.push_str(&commands);
        buf.push(' ');
    }

    fn points(&self, op: &Value) -> Vec<(f32, f32)> {
        op.get("points")
            .and_then(Value::as_array)
            .map(|pts| {
                pts.iter()
                    .filter_map(|p| {
                        let p = p.as_array()?;
                        let x = p.first()?.as_f64()? as f32;
                        let y = p.get(1)?.as_f64()? as f32;
                        Some((self.x(x), self.y(y)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn polygon(&self, op: &Value, close: bool) -> String {
        let pts = self.points(op);
        let Some(((fx, fy), rest)) = pts.split_first() else {
            return String::new();
        };
        let mut s = format!("M {:.2} {:.2}", fx, fy);
        for (x, y) in rest {
            s.push_str(&format!(" L {:.2} {:.2}", x, y));
        }
        if close {
            s.push_str(" Z");
        }
        s
    }

    /// B-splines are a start point followed by cubic bezier triples.
    fn bspline(&self, op: &Value, close: bool) -> String {
        let pts = self.points(op);
        let Some(((fx, fy), rest)) = pts.split_first() else {
            return String::new();
        };
        let mut s = format!("M {:.2} {:.2}", fx, fy);
        for chunk in rest.chunks_exact(3) {
            let [(c1x, c1y), (c2x, c2y), (ex, ey)] = chunk else {
                break;
            };
            s.push_str(&format!(
                " C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2}",
                c1x, c1y, c2x, c2y, ex, ey
            ));
        }
        if close {
            s.push_str(" Z");
        }
        s
    }

    fn ellipse(&self, op: &Value) -> String {
        let Some(rect) = op.get("rect").and_then(Value::as_array) else {
            return String::new();
        };
        let f = |i: usize| rect.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let (cx, cy, rx, ry) = (self.x(f(0)), self.y(f(1)), f(2), f(3));
        // Two half-arcs make a full ellipse in SVG path form.
        format!(
            "M {:.2} {:.2} A {:.2} {:.2} 0 1 0 {:.2} {:.2} A {:.2} {:.2} 0 1 0 {:.2} {:.2} Z",
            cx - rx,
            cy,
            rx,
            ry,
            cx + rx,
            cy,
            rx,
            ry,
            cx - rx,
            cy,
        )
    }

    fn text(&mut self, pen: &Pen, op: &Value) {
        let Some(text) = op.get("text").and_then(Value::as_str) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let pt = op.get("pt").and_then(Value::as_array);
        let px = pt
            .and_then(|p| p.first())
            .and_then(Value::as_f64)
            .unwrap_or(0.0) as f32;
        let py = pt
            .and_then(|p| p.get(1))
            .and_then(Value::as_f64)
            .unwrap_or(0.0) as f32;
        let align = match op.get("align").and_then(Value::as_str) {
            Some("l") => TextAlign::Left,
            Some("r") => TextAlign::Right,
            _ => TextAlign::Center,
        };

        // Graphviz gives the text's baseline anchor; Slint positions by the
        // top-left. Approximate the ascent to lift the anchor to the box top;
        // the horizontal offset by label width happens in the UI.
        self.texts.push(RenderText {
            x: self.x(px),
            y: self.y(py) - pen.font_size * 0.8,
            size: pen.font_size,
            text: text.to_owned(),
            color: pen.stroke.unwrap_or([0, 0, 0, 255]),
            align,
        });
    }

    fn take_paths(&mut self) -> Vec<RenderPath> {
        self.order
            .drain(..)
            .filter_map(|key| {
                let commands = self.groups.remove(&key)?;
                Some(RenderPath {
                    commands,
                    fill: key.fill,
                    stroke: key.stroke,
                    stroke_width: key.width_centi as f32 / 100.0,
                })
            })
            .collect()
    }
}

/// Parse graphviz colours: `#RRGGBB`, `#RRGGBBAA`, or `none`.
fn parse_color(v: &Value) -> Option<[u8; 4]> {
    let s = v.as_str()?;
    if s.eq_ignore_ascii_case("none") {
        return None;
    }
    let hex = s.strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    match hex.len() {
        6 => Some([byte(0)?, byte(2)?, byte(4)?, 255]),
        8 => Some([byte(0)?, byte(2)?, byte(4)?, byte(6)?]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 100x50 graph: one filled green box (black border) and one centered
    // label. Exercises colour parsing, y-flip, grouping and text alignment.
    const JSON: &str = r##"{
        "bb": "0,0,100,50",
        "objects": [{
            "_draw_": [
                {"op": "c", "color": "#000000"},
                {"op": "C", "color": "#00ff00"},
                {"op": "P", "points": [[10,10],[10,40],[90,40],[90,10]]}
            ],
            "_ldraw_": [
                {"op": "F", "size": 12.0},
                {"op": "c", "color": "#000000"},
                {"op": "T", "pt": [50,25], "align": "c", "width": 30, "text": "hi"}
            ]
        }]
    }"##;

    #[test]
    fn parses_box_and_label() {
        let g = parse(JSON.as_bytes()).unwrap();
        assert_eq!(g.width, 100.0);
        assert_eq!(g.height, 50.0);

        // Filled box + its border collapse into a single path group.
        assert_eq!(g.paths.len(), 1);
        let p = &g.paths[0];
        assert_eq!(p.fill, Some([0, 255, 0, 255]));
        assert_eq!(p.stroke, Some([0, 0, 0, 255]));
        // y is flipped: graphviz y=10 -> 50-10 = 40.
        assert!(p.commands.starts_with("M 10.00 40.00"));
        assert!(p.commands.trim_end().ends_with('Z'));

        assert_eq!(g.texts.len(), 1);
        let t = &g.texts[0];
        assert_eq!(t.text, "hi");
        assert_eq!(t.align, TextAlign::Center);
        // x is the raw anchor; the UI offsets by the label's own width.
        assert_eq!(t.x, 50.0);
        assert_eq!(t.color, [0, 0, 0, 255]);
    }

    #[test]
    fn none_colour_is_transparent() {
        assert_eq!(parse_color(&Value::String("none".into())), None);
        assert_eq!(
            parse_color(&Value::String("#aabbcc".into())),
            Some([0xaa, 0xbb, 0xcc, 255])
        );
        assert_eq!(
            parse_color(&Value::String("#11223344".into())),
            Some([0x11, 0x22, 0x33, 0x44])
        );
    }
}
