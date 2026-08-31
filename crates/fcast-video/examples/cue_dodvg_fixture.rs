// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Dump cues as dodvg oracle fixtures.
//!
//! One directory per case, each holding:
//!
//! * `scene.txt`   the `CueScene` as text, one primitive per line
//! * `font*.bin`   the font blobs the run table points at
//! * `oracle.rgba` the pixels the vello_cpu backend paints from that very
//!   scene, tightly packed straight-alpha RGBA
//!
//! The scene is the product, not a test artefact: the fork's `cues.rs` draws
//! the same display list through dodvg and the rig diffs the two backends. What
//! keeps that honest is the assertion below that the one-piece
//! [`RasterCtx::render`] paints the oracle byte for byte, so a scene that lost
//! a primitive shows up here rather than cancelling out on both sides.
//!
//! Build with `--features cue-ir-spike`. Consume with
//! `DODVG_CUE_FIXTURE=<dir> cargo test -p i-slint-renderer-dodvg --features
//! wgpu-executor cue`.

use std::{collections::HashMap, fmt::Write as _};

use fcast_video::{
    cue_ir::{CueStyle, RasterCtx, VideoRect, ir, ir::CueIr},
    cue_scene::{ALL_REVEALED, VelloBackend},
};

const CANVAS: (u32, u32) = (1280, 720);

/// The window filled by the picture, which is what an unpositioned cue is
/// placed against.
fn full_frame() -> VideoRect {
    VideoRect {
        x: 0,
        y: 0,
        width: 1280,
        height: 720,
    }
}

/// A 2.39:1 picture letterboxed into a 16:9 window, so a positioned cue that
/// anchors to the window instead of the picture lands 92 px off.
fn letterboxed() -> VideoRect {
    VideoRect {
        x: 0,
        y: 92,
        width: 1280,
        height: 536,
    }
}

/// The house style with a gaussian-feathered box rim instead of a hard edge.
fn feathered() -> CueStyle {
    let mut house = CueStyle::default();
    if let Some(background) = house.background.as_mut() {
        background.edge_softness = 0.12;
    }
    house
}

/// Per-span colours, a span background, an underline and a cue-wide drop
/// shadow: every pass and every primitive kind in one cue.
fn busy_cue() -> CueIr {
    let mut cue = CueIr::from_plain_text("");
    let mut first = ir::Span::plain("first ");
    first.style.underline = Some(true);
    first.style.background = Some(ir::Color::rgba(0, 60, 120, 140));
    let mut second = ir::Span::plain("second");
    second.style.foreground = Some(ir::Color::rgb(255, 80, 80));
    cue.lines[0].spans = vec![first, second];
    cue.base.shadow = Some(ir::Shadow {
        color: ir::Color::rgba(0, 0, 0, 180),
        dx: 2.0,
        dy: 3.0,
        blur: 0.0,
    });
    cue
}

/// A line whose house family is NAMED rather than generic, and which mixes
/// scripts: U+266A bracketing ASCII letters and digits.
///
/// Named `sans-serif` used to resolve to no family at all, and fontique then
/// picked a face per character: the letters landed in a text face and the
/// digits in the colour emoji face, because `0` to `9` carry the Unicode Emoji
/// property. Both backends drew that faithfully, so the case is here to hold
/// the LAYOUT honest, and it still covers a multi face cue on any machine
/// whose sans-serif does not carry the notes.
fn fallback_cue() -> CueIr {
    CueIr::from_plain_text("\u{266a} It's a little after 12 \u{266a}")
}

/// The style of a real subtitle file, `Style: Default,sans-serif,53`, at the
/// size and family it names.
fn ass_like() -> CueStyle {
    CueStyle {
        font_family: Some("sans-serif".into()),
        font_height_fraction: 53.0 / 720.0,
        ..CueStyle::default()
    }
}

/// SSA `\pos`: the anchor point as a percentage of the frame.
fn ass_positioned() -> CueIr {
    let mut cue = CueIr::from_plain_text("Positioned by pos");
    cue.layout.origin = Some((70.0, 25.0));
    cue
}

/// WebVTT `line` and `position`, the other route into the same placement.
fn vtt_positioned() -> CueIr {
    let mut cue = CueIr::from_plain_text("Positioned by line");
    cue.layout.line = Some(ir::LinePosition::Percent(20.0));
    cue.layout.position = Some(30.0);
    cue
}

fn main() {
    let root = std::env::args()
        .nth(1)
        .expect("usage: cue_dodvg_fixture <dir>");

    // Plan section 5's cases, by number. 1 to 3 are the spike's, and 4, 8 and
    // 9 are what wave 4 adds.
    let text = || CueIr::from_plain_text("The quick brown fox, 42%");
    let cases: [(&str, CueIr, CueStyle, VideoRect); 6] = [
        // 1: glyph rasterizer parity, isolated.
        (
            "plain",
            text(),
            CueStyle {
                outline: None,
                background: None,
                ..CueStyle::default()
            },
            full_frame(),
        ),
        // 2: the rounded rect SDF.
        ("boxed", text(), CueStyle::boxed(), full_frame()),
        // 3: the stroker on top of both.
        ("full", text(), CueStyle::default(), full_frame()),
        // 4: shadow_rect against fill_blurred_rounded_rect.
        ("feathered", text(), feathered(), full_frame()),
        // 8: place() against a letterboxed picture, both routes.
        (
            "positioned",
            ass_positioned(),
            CueStyle::default(),
            letterboxed(),
        ),
        (
            "positioned_vtt",
            vtt_positioned(),
            CueStyle::default(),
            letterboxed(),
        ),
    ];

    let mut ctx = RasterCtx::new();
    let mut backend = VelloBackend::new();
    for (name, ir, house, frame) in cases {
        dump(
            &mut ctx,
            &mut backend,
            &format!("{root}/{name}"),
            &ir,
            &house,
            frame,
        );
    }
    // 9: per-span colours, shadow and decorations, on the plain house style so
    // the box does not hide the span backgrounds.
    dump(
        &mut ctx,
        &mut backend,
        &format!("{root}/spans"),
        &busy_cue(),
        &CueStyle::outline_only(),
        full_frame(),
    );
    // A named house family and a mixed script line, the shape of a real
    // subtitle file.
    dump(
        &mut ctx,
        &mut backend,
        &format!("{root}/fallback"),
        &fallback_cue(),
        &ass_like(),
        full_frame(),
    );
}

fn dump(
    ctx: &mut RasterCtx,
    backend: &mut VelloBackend,
    dir: &str,
    ir: &CueIr,
    house: &CueStyle,
    frame: VideoRect,
) {
    std::fs::create_dir_all(dir).expect("the fixture directory must be creatable");

    let direct = ctx
        .render(ir, house, CANVAS, Some(frame), usize::MAX)
        .expect("the cue must rasterize");
    let scene = ctx
        .build_scene(ir, house, CANVAS, Some(frame))
        .expect("the cue must build a scene");
    let oracle = backend
        .rasterize(&scene, ALL_REVEALED)
        .expect("the scene must rasterize");

    // The scene is not lossy: the one-piece rasterizer and the scene plus the
    // backend paint the same pixels in the same place.
    assert_eq!(
        (direct.width, direct.height, direct.x, direct.y),
        (oracle.width, oracle.height, oracle.x, oracle.y),
        "{dir}: the scene disagrees about the surface"
    );
    assert!(
        direct.pixels == oracle.pixels,
        "{dir}: the scene painted different pixels"
    );

    std::fs::write(format!("{dir}/oracle.rgba"), &oracle.pixels).expect("oracle write");

    // The run table points at font blobs by slot; identical blobs share a file
    // and a slot, which is also what the atlas key on the other side assumes.
    let mut slots: HashMap<(u64, u32), usize> = HashMap::new();
    let mut wire = String::new();
    writeln!(wire, "origin {} {}", scene.origin[0], scene.origin[1]).ok();
    writeln!(wire, "size {} {}", scene.size[0], scene.size[1]).ok();
    writeln!(
        wire,
        "translate {} {}",
        scene.translate[0], scene.translate[1]
    )
    .ok();
    // Where the production rasterizer put the surface, so the rig can check
    // the placement it was handed rather than trusting the scene about itself.
    writeln!(wire, "place {} {}", direct.x, direct.y).ok();

    for (i, run) in scene.runs.iter().enumerate() {
        let id = (run.font.data.id(), run.font.index);
        let next = slots.len();
        let slot = *slots.entry(id).or_insert(next);
        if slot == next {
            std::fs::write(format!("{dir}/font{slot}.bin"), run.font.data.data())
                .expect("font write");
        }
        write!(
            wire,
            "run {slot} {} {} {}",
            run.font.index, run.font_size, run.stroke_width
        )
        .ok();
        for c in scene.run_coords(i as u16) {
            write!(wire, " {c}").ok();
        }
        wire.push('\n');
    }

    for r in 0..scene.rect_count() {
        let [x0, y0, x1, y1] = scene.rect_xyxy[r];
        let [tl, tr, br, bl] = scene.rect_radii[r];
        let c = scene.rect_color[r];
        writeln!(
            wire,
            "rect {x0} {y0} {x1} {y1} {tl} {tr} {br} {bl} {} {} {} {} {} {} {}",
            scene.rect_sigma[r],
            c[0],
            c[1],
            c[2],
            c[3],
            scene.rect_after_glyph[r],
            scene.rect_rank[r]
        )
        .ok();
    }
    for g in 0..scene.glyph_count() {
        let [x, y] = scene.glyph_xy[g];
        let c = scene.glyph_color[g];
        writeln!(
            wire,
            "glyph {} {} {x} {y} {} {} {} {} {}",
            scene.glyph_run[g], scene.glyph_id[g], c[0], c[1], c[2], c[3], scene.glyph_rank[g]
        )
        .ok();
    }
    std::fs::write(format!("{dir}/scene.txt"), &wire).expect("scene write");

    println!(
        "wrote {dir}: surface {}x{} at ({}, {}), {} rects, {} glyphs, {} runs, {} fonts",
        scene.size[0],
        scene.size[1],
        scene.origin[0],
        scene.origin[1],
        scene.rect_count(),
        scene.glyph_count(),
        scene.runs.len(),
        slots.len(),
    );
}
