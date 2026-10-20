//! The Brush Settings panel's eight reference categories. Each edits a working copy of the brush;
//! the panel turns the difference into one `tools.setBrush` call.

use egui::{Color32, RichText, Sense, Stroke, pos2, vec2};
use photocraft_engine::BrushSettings;
use photocraft_engine::paint::{self, BrushPreset, Control, Dynamic, MAX_BRUSH_SIZE, MaskMode, Pattern, PatternStyle, TipShape};

use crate::brush_preview;
use crate::theme::{self, Tokens};
use crate::widgets;
use paint::material::{GrainSettings, WetColorMode, WetEdgeControl, WetEdgeSettings, WetOutline, WetStyle, WetWidthMode};

pub const CONTROLS: [(Control, &str); 11] = [
    (Control::Off, "Off"),
    (Control::DistanceFade, "Fade"),
    (Control::PenPressureInverse, "Inverse Pen Pressure"),
    (Control::Speed, "Speed"),
    (Control::PressureRelease, "Pressure Release"),
    (Control::PenPressure, "Pen Pressure"),
    (Control::PenTilt, "Pen Tilt"),
    (Control::StylusWheel, "Stylus Wheel"),
    (Control::Rotation, "Rotation"),
    (Control::InitialDirection, "Initial Direction"),
    (Control::Direction, "Direction"),
];

fn reference_controls(id: &str) -> Vec<(Control, &'static str)> {
    use paint::brush::{
        REFERENCE_BASE_CONTROLS, REFERENCE_PIGMENT_CONTROLS, REFERENCE_SHAPE_CONTROLS, REFERENCE_SIZE_CONTROLS, REFERENCE_SPACING_CONTROLS,
        REFERENCE_TEXTURE_CONTROLS,
    };
    let allowed = match id {
        "brush-size-ctl" | "hardness-control" | "brush-opacity-ctl" | "brush-flow-ctl" | "brush-angle-ctl" => REFERENCE_SIZE_CONTROLS,
        "scatter-spacing-control" => REFERENCE_SPACING_CONTROLS,
        "brush-depth-ctl" => REFERENCE_TEXTURE_CONTROLS,
        "thickness" | "visual-height" => REFERENCE_PIGMENT_CONTROLS,
        "stroke-jitter-control" | "brush-round-ctl" | "brush-scatter-ctl" | "brush-count-ctl" | "inertia-control" => REFERENCE_SHAPE_CONTROLS,
        _ => REFERENCE_BASE_CONTROLS,
    };
    allowed
        .iter()
        .map(|&control| {
            let label = if control == Control::Fade { "Fade" } else { CONTROLS.iter().find(|(value, _)| *value == control).map_or("Off", |(_, label)| *label) };
            (control, label)
        })
        .collect()
}

pub const MASK_MODES: [(MaskMode, &str); 10] = [
    (MaskMode::Multiply, "Multiply"),
    (MaskMode::Subtract, "Subtract"),
    (MaskMode::Darken, "Darken"),
    (MaskMode::Overlay, "Overlay"),
    (MaskMode::ColorDodge, "Color Dodge"),
    (MaskMode::ColorBurn, "Color Burn"),
    (MaskMode::LinearBurn, "Linear Burn"),
    (MaskMode::HardMix, "Hard Mix"),
    (MaskMode::LinearHeight, "Linear Height"),
    (MaskMode::Height, "Height"),
];

const PATTERNS: [(PatternStyle, &str); 4] =
    [(PatternStyle::Paper, "Paper"), (PatternStyle::Canvas, "Canvas"), (PatternStyle::Noise, "Noise"), (PatternStyle::Dots, "Dots")];

/// Percent slider over a 0..`max` fraction.
fn pct(ui: &mut egui::Ui, label: &str, v: &mut f32, max: f32) -> bool {
    signed_pct(ui, label, v, 0.0, max)
}

/// Percent slider over a `min..max` fraction.
fn signed_pct(ui: &mut egui::Ui, label: &str, v: &mut f32, min: f32, max: f32) -> bool {
    let mut p = (*v * 100.0).round();
    let changed = widgets::slider_row(ui, label, &mut p, min * 100.0..=max * 100.0, "%", None).changed();
    if changed {
        *v = (p.round() / 100.0).clamp(min, max);
    }
    changed
}

fn num(ui: &mut egui::Ui, label: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>, suffix: &str) -> bool {
    let mut x = *v;
    let changed = widgets::slider_row(ui, label, &mut x, range.clone(), suffix, None).changed();
    if changed {
        *v = x.round().clamp(*range.start(), *range.end());
    }
    changed
}

fn dim(ui: &mut egui::Ui, s: &str) {
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new(s).color(t.text_dim));
}

/// Whether a dynamic's control has a 0..1 input its own response curve shapes (not Off, not the
/// direction controls, not Angle Jitter, whose controls set an angle).
pub(crate) fn takes_curve(control: Control, angle: bool) -> bool {
    !angle && control != Control::Off
}

/// egui memory key of the open/closed state of a dynamic's curve editor.
pub(crate) fn curve_open_id(id: &str) -> egui::Id {
    egui::Id::new(("brush-curve-open", id))
}

/// `Control: [Off ▾] [25] [~]`: the controller pop-up, Fade's step count and the button that
/// shows this dynamic's own response curve (lit while the curve isn't linear). `angle` offers the
/// direction controls (Photoshop only has them for Angle Jitter).
fn control_row(ui: &mut egui::Ui, id: &str, d: &mut Dynamic, angle: bool) {
    let open_id = curve_open_id(id);
    let mut open: bool = ui.data(|m| m.get_temp(open_id)).unwrap_or(false);
    ui.horizontal(|ui| {
        dim(ui, tl!("Control:"));
        widgets::dropdown(ui, id, &mut d.control, &reference_controls(id), 130.0);
        if d.control == Control::Fade {
            let mut steps = d.fade_steps as f32;
            if widgets::value_field(ui, &mut steps, 1.0..=9999.0, "", 56.0).on_hover_text(tl!("Fade steps")).changed() {
                d.fade_steps = steps.round().clamp(1.0, 9999.0) as u32;
            }
        }
        if takes_curve(d.control, angle) && crate::icons::button(ui, "spline", 22.0, open || !d.curve.is_linear(), tl!("Response curve")).clicked() {
            open = !open;
        }
    });
    if open && takes_curve(d.control, angle) {
        let mut points = d.curve.points();
        if crate::pressure_curve_ui::edit_points(ui, egui::Id::new(("brush-curve-rect", id)), &mut points) {
            d.curve = paint::ResponseCurve::new(&points);
        }
    }
    ui.data_mut(|m| m.insert_temp(open_id, open));
    ui.add_space(2.0);
}

/// A jitter slider, its Control row and (optionally) the Minimum slider: one Photoshop block.
fn dynamic(ui: &mut egui::Ui, id: &str, label: &str, d: &mut Dynamic, max: f32, minimum: Option<&str>, angle: bool) {
    pct(ui, label, &mut d.jitter, max);
    control_row(ui, id, d, angle);
    if let Some(m) = minimum {
        minimum_row(ui, m, d);
    }
    ui.add_space(6.0);
}

/// Minimum belongs to the control. Off disables editing without discarding its saved value.
fn minimum_row(ui: &mut egui::Ui, label: &str, d: &mut Dynamic) {
    ui.add_enabled_ui(d.control != Control::Off, |ui| pct(ui, label, &mut d.minimum, 1.0));
}

/// Size: a value field over a logarithmic slider (small sizes get most of the travel).
/// `restore` adds Photoshop's "Restore original size" button (sampled tips).
fn size_row(ui: &mut egui::Ui, label: &str, size: &mut f32, max: f32, restore: Option<f32>) {
    let t = Tokens::get(ui.ctx());
    ui.horizontal(|ui| {
        ui.label(RichText::new(tl!(label)).color(t.text_dim));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut s = *size;
            if widgets::value_field(ui, &mut s, 1.0..=max, "px", 74.0).changed() {
                *size = s.round().clamp(1.0, max);
            }
            if let Some(orig) = restore
                && crate::icons::button(ui, "undo-2", 22.0, false, tl!("Restore original size")).clicked()
            {
                *size = orig.clamp(1.0, max);
            }
        });
    });
    let mut lv = size.max(1.0).ln();
    if widgets::slider(ui, &mut lv, 0.0..=max.ln(), None).changed() {
        *size = lv.exp().round().clamp(1.0, max);
    }
    ui.add_space(4.0);
}

/// Which part of the angle/roundness widget a drag grabbed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EllipseGrab {
    Angle,
    Roundness,
}

/// Outer radius of the angle/roundness widget's ellipse.
const ELLIPSE_R: f32 = 34.0;

/// Screen positions (relative to the widget centre) of the two roundness handles.
pub fn roundness_handles(angle: f32, roundness: f32) -> [egui::Vec2; 2] {
    let (s, c) = angle.to_radians().sin_cos();
    // The minor axis is the major axis turned 90° counter-clockwise; screen y points down.
    let minor = vec2(-s, -c) * ELLIPSE_R * roundness.clamp(0.0, 1.0);
    [minor, -minor]
}

/// What a press at `rel` (relative to the centre) grabs: a roundness handle within 7 px, else the angle.
pub fn ellipse_grab(rel: egui::Vec2, angle: f32, roundness: f32) -> EllipseGrab {
    if roundness_handles(angle, roundness).iter().any(|h| (*h - rel).length() <= 7.0) { EllipseGrab::Roundness } else { EllipseGrab::Angle }
}

/// The new (angle, roundness) for a drag to `rel`: the angle points at the pointer (degrees,
/// counter-clockwise, -180..180); the roundness is the pointer's distance along the minor axis.
pub fn ellipse_drag(grab: EllipseGrab, rel: egui::Vec2, angle: f32, roundness: f32) -> (f32, f32) {
    match grab {
        EllipseGrab::Angle if rel.length() > 1.0 => ((-rel.y).atan2(rel.x).to_degrees().round(), roundness),
        EllipseGrab::Angle => (angle, roundness),
        EllipseGrab::Roundness => {
            let (s, c) = angle.to_radians().sin_cos();
            let along = (rel.x * -s + rel.y * -c).abs();
            (angle, (along / ELLIPSE_R).clamp(0.01, 1.0))
        }
    }
}

/// Photoshop's angle/roundness widget: drag the arrow to rotate, the dots to squash.
fn ellipse_widget(ui: &mut egui::Ui, angle: &mut f32, roundness: &mut f32) -> bool {
    let t = Tokens::get(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(84.0, 84.0), Sense::click_and_drag());
    let c = rect.center();
    let grab_id = resp.id.with("grab");
    let mut changed = false;
    if resp.drag_started()
        && let Some(p) = resp.interact_pointer_pos()
    {
        let g = ellipse_grab(p - c, *angle, *roundness);
        ui.data_mut(|d| d.insert_temp(grab_id, g == EllipseGrab::Roundness));
    }
    if (resp.dragged() || resp.clicked())
        && let Some(p) = resp.interact_pointer_pos()
    {
        let grab = if resp.clicked() {
            ellipse_grab(p - c, *angle, *roundness)
        } else if ui.data(|d| d.get_temp::<bool>(grab_id)).unwrap_or(false) {
            EllipseGrab::Roundness
        } else {
            EllipseGrab::Angle
        };
        let (a, r) = ellipse_drag(grab, p - c, *angle, *roundness);
        if a != *angle || (r - *roundness).abs() > 1e-4 {
            *angle = a;
            *roundness = r;
            changed = true;
        }
    }
    let p = ui.painter();
    p.circle_filled(c, 40.0, t.field);
    p.circle_stroke(c, 40.0, Stroke::new(1.0, t.field_border));
    p.line_segment([c - vec2(40.0, 0.0), c + vec2(40.0, 0.0)], Stroke::new(1.0, t.separator));
    p.line_segment([c - vec2(0.0, 40.0), c + vec2(0.0, 40.0)], Stroke::new(1.0, t.separator));
    let (s, co) = angle.to_radians().sin_cos();
    let major = vec2(co, -s);
    let minor = vec2(-s, -co);
    let rr = roundness.clamp(0.01, 1.0);
    let pts: Vec<egui::Pos2> = (0..48)
        .map(|i| {
            let a = i as f32 / 48.0 * std::f32::consts::TAU;
            c + major * a.cos() * ELLIPSE_R + minor * a.sin() * ELLIPSE_R * rr
        })
        .collect();
    p.add(egui::Shape::convex_polygon(pts, t.text.gamma_multiply(0.18), Stroke::new(1.5, t.text)));
    // Arrow along the major axis.
    let tip = c + major * (ELLIPSE_R + 5.0);
    p.line_segment([c, tip], Stroke::new(1.5, t.accent));
    let back = tip - major * 7.0;
    p.add(egui::Shape::convex_polygon(vec![tip, back + minor * 4.0, back - minor * 4.0], t.accent, Stroke::NONE));
    for h in roundness_handles(*angle, *roundness) {
        p.circle_filled(c + h, 4.0, Color32::WHITE);
        p.circle_stroke(c + h, 4.0, Stroke::new(1.0, t.accent));
    }
    let _ = resp.on_hover_text(tl!("Drag the arrow to set the angle, the dots to set the roundness"));
    changed
}

/// The tip a preset contributes to a tip grid: (shape, size, hardness, angle, roundness, spacing).
pub fn preset_tip(p: &BrushPreset) -> (&TipShape, f32, f32, f32, f32, f32) {
    let b = &p.brush;
    (&b.tip, b.size, b.hardness, b.angle, b.roundness, b.spacing)
}

/// Thumbnails show relative size: small tips are drawn smaller (down to 30 %), 40 px and up fill the cell.
pub fn thumb_scale(size: f32) -> f32 {
    (size / 40.0).clamp(0.3, 1.0)
}

/// Photoshop's scrolling grid of preset tips with their sizes; `highlight` is the selected
/// preset (by identity: look-alike duplicates must stay distinct). Returns the clicked index.
fn tip_grid(ui: &mut egui::Ui, id: &str, presets: &[BrushPreset], highlight: Option<usize>) -> Option<usize> {
    let t = Tokens::get(ui.ctx());
    let mut clicked = None;
    let frame = egui::Frame::NONE.fill(t.field).stroke(Stroke::new(1.0, t.field_border)).corner_radius(t.radius_sm as u8).inner_margin(egui::Margin::same(3));
    frame.show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt(id).max_height(104.0).auto_shrink([false, true]).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(2.0, 2.0);
                for (i, p) in presets.iter().enumerate() {
                    let (cell, resp) = ui.allocate_exact_size(vec2(44.0, 50.0), Sense::click());
                    if !ui.is_rect_visible(cell) {
                        continue;
                    }
                    if highlight == Some(i) {
                        ui.painter().rect_filled(cell, 3.0, t.accent_soft);
                        ui.painter().rect_stroke(cell, 3.0, Stroke::new(1.0, t.accent), egui::StrokeKind::Inside);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(cell, 3.0, t.hover);
                    }
                    let (_, size, _, _, _, _) = preset_tip(p);
                    let tex = brush_preview::tip_texture(ui.ctx(), &format!("{id}:{}", p.name), &p.brush, 34, t.text);
                    let ir = egui::Rect::from_center_size(pos2(cell.center().x, cell.top() + 20.0), vec2(34.0, 34.0) * thumb_scale(size));
                    ui.painter().image(tex.id(), ir, egui::Rect::from_min_max(egui::Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
                    ui.painter().text(
                        pos2(cell.center().x, cell.bottom() - 6.0),
                        egui::Align2::CENTER_CENTER,
                        format!("{}", size.round() as i64),
                        egui::FontId::proportional(9.5),
                        t.text_dim,
                    );
                    if resp.on_hover_text(&p.name).clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
    });
    ui.add_space(6.0);
    clicked
}

/// The Brush Tip Shape section: the tip grid and the tip's own controls. Picking a tip copies the
/// tip fields only — the dynamics sections stay as they are (Photoshop). The clicked preset's name
/// is returned so the caller can mark it current with `brush.presets.setCurrent` (by identity:
/// look-alike duplicates stay distinct; the engine can't infer it from the brush). The tip fields
/// are also copied into the working brush so this frame's controls start from them.
fn tip_shape(ui: &mut egui::Ui, b: &mut BrushSettings, presets: &[BrushPreset], current: Option<&str>) -> Option<String> {
    let highlight = current.and_then(|n| presets.iter().position(|p| p.name.eq_ignore_ascii_case(n)));
    if let Some(p) = tip_grid(ui, "brush-tip-grid", presets, highlight).and_then(|i| presets.get(i)) {
        let (pt, ps, ph, pa, pr, psp) = preset_tip(p);
        b.tip = pt.clone();
        b.triangle = p.brush.triangle;
        b.c3_tip_math = p.brush.c3_tip_math;
        b.size = ps;
        b.hardness = ph;
        b.angle = pa;
        b.roundness = pr;
        b.spacing = psp;
        return Some(p.name.clone());
    }
    let mut shape = if b.triangle {
        1u8
    } else if matches!(b.tip, TipShape::Round) {
        0
    } else {
        2
    };
    let mut shapes = vec![(0u8, "Round Tip"), (1, "Triangle")];
    if shape == 2 {
        shapes.push((2, "Sampled Tip"));
    }
    if widgets::dropdown(ui, "brush-tip-shape", &mut shape, &shapes, 130.0) {
        b.triangle = shape == 1;
        if shape != 2 {
            b.tip = TipShape::Round;
            b.c3_tip_math = true;
        }
    }
    let orig = b.tip.bitmap_size().map(|(w, h)| w.max(h) as f32);
    size_row(ui, tl!("Size"), &mut b.size, MAX_BRUSH_SIZE, orig);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let mut widget_angle = if b.c3_tip_math { -b.angle } else { b.angle };
        if ellipse_widget(ui, &mut widget_angle, &mut b.roundness) {
            b.angle = if b.c3_tip_math { -widget_angle } else { widget_angle };
        }
        ui.add_space(8.0);
        ui.vertical(|ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(vec2(72.0, 24.0), egui::Layout::left_to_right(egui::Align::Center), |ui| dim(ui, tl!("Angle:")));
                let mut a = b.angle;
                if widgets::value_field(ui, &mut a, -180.0..=180.0, "°", 70.0).changed() {
                    b.angle = a.round().clamp(-180.0, 180.0);
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(vec2(72.0, 24.0), egui::Layout::left_to_right(egui::Align::Center), |ui| dim(ui, tl!("Roundness:")));
                let mut r = (b.roundness * 100.0).round();
                if widgets::value_field(ui, &mut r, 1.0..=100.0, "%", 70.0).changed() {
                    b.roundness = (r.round() / 100.0).clamp(0.01, 1.0);
                }
            });
        });
    });
    ui.add_space(6.0);
    if signed_pct(ui, tl!("Spacing"), &mut b.spacing, 0.01, 10.0) {
        b.spacing_enabled = true;
    }
    None
}

fn shape_dynamics(ui: &mut egui::Ui, b: &mut BrushSettings) {
    let sd = &mut b.shape_dynamics;
    pct(ui, tl!("Size Jitter"), &mut sd.size.jitter, 1.0);
    control_row(ui, "brush-size-ctl", &mut sd.size, false);
    minimum_row(ui, tl!("Minimum Diameter"), &mut sd.size);
    dynamic(ui, "stroke-jitter-control", tl!("Stroke Jitter"), &mut sd.stroke_jitter, 10.0, Some(tl!("Minimum Stroke Jitter")), false);
    ui.add_space(6.0);
    let mut degrees = sd.angle.jitter * 360.0;
    if num(ui, tl!("Angle Jitter"), &mut degrees, 0.0..=360.0, "°") {
        sd.angle.jitter = degrees / 360.0;
    }
    control_row(ui, "brush-angle-ctl", &mut sd.angle, true);
    minimum_row(ui, tl!("Minimum Angle"), &mut sd.angle);
    dynamic(ui, "brush-round-ctl", tl!("Roundness Jitter"), &mut sd.roundness, 1.0, Some(tl!("Minimum Roundness")), false);
}

fn scattering(ui: &mut egui::Ui, b: &mut BrushSettings) {
    let sc = &mut b.scattering;
    pct(ui, tl!("Scatter"), &mut sc.scatter.jitter, if b.c3_tip_math { 50.0 } else { 10.0 });
    pct(ui, tl!("Range Jitter"), &mut sc.range_jitter, 1.0);
    dynamic(ui, "scatter-spacing-control", tl!("Spacing Jitter"), &mut sc.spacing_jitter, 1.0, Some(tl!("Minimum Spacing")), false);
    ui.add_space(2.0);
    control_row(ui, "brush-scatter-ctl", &mut sc.scatter, false);
    minimum_row(ui, tl!("Minimum Scatter"), &mut sc.scatter);
    ui.add_space(6.0);
    let mut c = sc.count as f32;
    if num(ui, tl!("Count"), &mut c, 1.0..=16.0, "") {
        sc.count = c as u32;
    }
    dynamic(ui, "brush-count-ctl", tl!("Count Jitter"), &mut sc.count_jitter, 1.0, None, false);
    ui.add_enabled_ui(sc.count_jitter.control != Control::Off, |ui| {
        let mut minimum = sc.minimum_count.unwrap_or(sc.count_jitter.minimum * sc.count as f32).max(1.0);
        if num(ui, tl!("Minimum Count"), &mut minimum, 1.0..=16.0, "") {
            sc.minimum_count = Some(minimum);
        }
    });
}

/// The pattern swatch: the (adjusted) texture as the stroke sees it.
fn pattern_swatch(ui: &mut egui::Ui, tx: &paint::Texture) {
    let t = Tokens::get(ui.ctx());
    let n = 56u32;
    let (rect, _) = ui.allocate_exact_size(vec2(n as f32, n as f32), Sense::hover());
    let sig = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_vec(&(
            match &tx.pattern {
                Pattern::Procedural { style, size, seed } => (*style as u32, *size, *seed, 0),
                // A bitmap tile (a stored one's preview until it is loaded, #1843).
                p => p.bitmap().map_or((0, 0, 0, 0), |g| {
                    (g.width, g.height, g.data.len() as u32, g.data.iter().step_by((g.data.len() / 512).max(1)).map(|v| *v as u32).sum::<u32>())
                }),
            },
            tx.invert,
            tx.brightness,
            tx.contrast,
        ))
        .unwrap_or_default()
        .hash(&mut h);
        h.finish()
    };
    let tex = brush_preview::with_cache(ui.ctx(), |c| {
        c.get("texture-swatch", sig, || {
            let img = match &tx.pattern {
                Pattern::Procedural { style, size, seed } => paint::procedural::pattern(*style, (*size).clamp(4, 1024), *seed),
                p => match p.bitmap() {
                    Some(g) if g.is_valid() => paint::tile::PatternImage { width: g.width as usize, height: g.height as usize, data: g.to_f32() },
                    _ => paint::tile::PatternImage { width: 1, height: 1, data: vec![0.5] },
                },
            };
            let px: Vec<u8> = (0..n * n)
                .flat_map(|i| {
                    let x = tx.level(img.sample_wrap((i % n) as f32, (i / n) as f32));
                    let g = (x * 255.0).round() as u8;
                    [g, g, g, 255]
                })
                .collect();
            let img = egui::ColorImage::from_rgba_unmultiplied([n as usize, n as usize], &px);
            ui.ctx().load_texture("texture-swatch", img, egui::TextureOptions::LINEAR)
        })
    });
    ui.painter().image(tex.id(), rect, egui::Rect::from_min_max(egui::Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
    ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
}

fn texture(ui: &mut egui::Ui, b: &mut BrushSettings) {
    let tx = &mut b.texture;
    ui.horizontal(|ui| {
        pattern_swatch(ui, tx);
        ui.add_space(8.0);
        ui.vertical(|ui| {
            let mut style = match &tx.pattern {
                Pattern::Procedural { style, .. } => Some(*style),
                Pattern::Tile(_) | Pattern::Stored(_) => None,
            };
            let mut opts: Vec<(Option<PatternStyle>, &str)> = PATTERNS.iter().map(|(s, l)| (Some(*s), *l)).collect();
            if style.is_none() {
                opts.push((None, tl!("Custom")));
            }
            if widgets::dropdown(ui, "brush-pattern", &mut style, &opts, 110.0)
                && let Some(s) = style
            {
                let size = match &tx.pattern {
                    Pattern::Procedural { size, .. } => *size,
                    Pattern::Tile(_) | Pattern::Stored(_) => 128,
                };
                tx.pattern = Pattern::Procedural { style: s, size, seed: 1 };
            }
            ui.add_space(4.0);
            widgets::checkbox(ui, &mut tx.invert, tl!("Invert"));
        });
    });
    ui.add_space(6.0);
    if let Pattern::Procedural { size, .. } = &mut tx.pattern {
        let mut s = *size as f32;
        if num(ui, tl!("Pattern Size"), &mut s, 16.0..=1024.0, "px") {
            *size = s as u32;
        }
    }
    pct(ui, tl!("Scale"), &mut tx.scale, 10.0);
    let mut br = (tx.brightness * 150.0).round();
    if num(ui, tl!("Brightness"), &mut br, -150.0..=150.0, "") {
        tx.brightness = br / 150.0;
    }
    // Photoshop's -50..100 contrast onto the model's -1..1.
    let mut ct = if tx.contrast < 0.0 { tx.contrast * 50.0 } else { tx.contrast * 100.0 }.round();
    if num(ui, tl!("Contrast"), &mut ct, -50.0..=100.0, "") {
        tx.contrast = if ct < 0.0 { ct / 50.0 } else { ct / 100.0 };
    }
    ui.horizontal(|ui| {
        dim(ui, tl!("Coordinates:"));
        use paint::brush::TextureCoordinateMode as M;
        widgets::dropdown(
            ui,
            "texture-coordinates",
            &mut tx.coordinate_mode,
            &[(M::CanvasLocked, "Canvas Locked"), (M::StrokeLocked, "Stroke Locked"), (M::TipLocal, "Tip Local")],
            130.0,
        );
    });
    widgets::checkbox(ui, &mut tx.each_tip, tl!("Texture Each Tip"));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        dim(ui, tl!("Mode:"));
        widgets::dropdown(ui, "brush-texture-mode", &mut tx.mode, &MASK_MODES, 130.0);
    });
    ui.add_space(4.0);
    pct(ui, tl!("Depth"), &mut tx.depth, 1.0);
    let each = tx.each_tip;
    ui.add_enabled_ui(each, |ui| {
        pct(ui, tl!("Minimum Depth"), &mut tx.depth_jitter.minimum, 1.0);
        pct(ui, tl!("Depth Jitter"), &mut tx.depth_jitter.jitter, 1.0);
        control_row(ui, "brush-depth-ctl", &mut tx.depth_jitter, false);
    });
}

pub(crate) fn secondary_defaults(b: &BrushSettings) -> BrushSettings {
    b.dual_brush.resolved_brush(b).unwrap_or_else(|| {
        let d = &b.dual_brush;
        BrushSettings {
            tip: d.tip.clone(),
            size: d.size,
            hardness: d.hardness,
            roundness: d.roundness,
            angle: d.angle,
            spacing: d.spacing,
            opacity: d.opacity,
            flow: d.flow,
            pressure_size: false,
            grain: d.grain.clone(),
            wet_edges: d.wet_edges,
            wet_edge: d.wet_edge.clone(),
            scattering: paint::Scattering {
                enabled: d.scatter > 0.0 || d.count > 1,
                scatter: Dynamic::jitter(d.scatter),
                both_axes: d.both_axes,
                count: d.count,
                ..Default::default()
            },
            ..Default::default()
        }
    })
}

pub(crate) fn dual_brush_header(ui: &mut egui::Ui, b: &mut BrushSettings, lane: &mut usize) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if widgets::pill_tab(ui, tl!("Primary Brush"), *lane == 0).clicked() {
            *lane = 0;
        }
        let mut primary = (b.dual_brush.primary_strength * 100.0).round();
        if widgets::value_field(ui, &mut primary, 0.0..=100.0, "%", 70.0).changed() {
            b.dual_brush.primary_strength = primary.round() / 100.0;
        }
        ui.add_space(24.0);
        if widgets::pill_tab(ui, tl!("Secondary Brush"), *lane == 1).clicked() {
            *lane = 1;
        }
        let mut secondary = if b.dual_brush.enabled { (b.dual_brush.secondary_strength * 100.0).round() } else { 0.0 };
        if widgets::value_field(ui, &mut secondary, 0.0..=100.0, "%", 70.0).changed() {
            b.dual_brush.enabled = secondary > 0.0;
            b.dual_brush.secondary_strength = secondary.round() / 100.0;
        }
        ui.add_space(12.0);
        dim(ui, tl!("Mode:"));
        let modes = [
            (MaskMode::Normal, "Normal"),
            (MaskMode::Multiply, "Multiply"),
            (MaskMode::Darken, "Darken"),
            (MaskMode::Overlay, "Overlay"),
            (MaskMode::ColorDodge, "Color Dodge"),
            (MaskMode::ColorBurn, "Color Burn"),
            (MaskMode::LinearBurn, "Linear Burn"),
            (MaskMode::HardMix, "Hard Mix"),
            (MaskMode::LinearHeight, "Linear Height"),
        ];
        widgets::dropdown(ui, "brush-dual-mode", &mut b.dual_brush.mode, &modes, 130.0);
    });
}

fn color_dynamics(ui: &mut egui::Ui, b: &mut BrushSettings) {
    let c = &mut b.color_dynamics;
    widgets::checkbox(ui, &mut c.per_tip, tl!("Apply Per Tip"));
    ui.add_space(6.0);
    for (id, label, amount, control, minimum) in [
        ("hue", "Hue Jitter", &mut c.hue_jitter, &mut c.hue_control, &mut c.hue_minimum),
        ("saturation", "Saturation Jitter", &mut c.saturation_jitter, &mut c.saturation_control, &mut c.saturation_minimum),
        ("brightness", "Brightness Jitter", &mut c.brightness_jitter, &mut c.brightness_control, &mut c.brightness_minimum),
        ("purity", "Purity", &mut c.purity, &mut c.purity_control, &mut c.purity_minimum),
    ] {
        ui.push_id(id, |ui| {
            signed_pct(ui, tl!(label), amount, if id == "purity" { -1.0 } else { 0.0 }, 1.0);
            material_control(ui, id, control, &mut 25);
            pct(ui, tl!("Minimum"), minimum, 1.0);
        });
    }
}

fn transfer(ui: &mut egui::Ui, b: &mut BrushSettings) {
    ui.horizontal(|ui| {
        dim(ui, tl!("Flow Algorithm:"));
        widgets::dropdown(ui, "brush-flow-algorithm", &mut b.flow_algorithm_version, &[(1u8, "Version 1"), (2u8, "Version 2")], 130.0);
    });
    let tr = &mut b.transfer;
    dynamic(ui, "brush-opacity-ctl", tl!("Opacity Jitter"), &mut tr.opacity, 1.0, Some(tl!("Minimum")), false);
    dynamic(ui, "brush-flow-ctl", tl!("Flow Jitter"), &mut tr.flow, 1.0, Some(tl!("Minimum")), false);
}

/// Tilt is stored in degrees (±90, W3C Pointer Events); Photoshop shows it as ±100 %.
fn tilt(ui: &mut egui::Ui, label: &str, deg: &mut f32) {
    let mut v = (*deg / 90.0).clamp(-1.0, 1.0);
    if signed_pct(ui, label, &mut v, -1.0, 1.0) {
        *deg = v * 90.0;
    }
}

#[allow(dead_code)] // Pose is not part of the reference categories.
fn pose(ui: &mut egui::Ui, b: &mut BrushSettings) {
    let p = &mut b.pose;
    tilt(ui, tl!("Tilt X"), &mut p.tilt_x);
    tilt(ui, tl!("Tilt Y"), &mut p.tilt_y);
    widgets::checkbox(ui, &mut p.override_tilt, tl!("Override Tilt"));
    ui.add_space(8.0);
    num(ui, tl!("Rotation"), &mut p.rotation, 0.0..=360.0, "°");
    widgets::checkbox(ui, &mut p.override_rotation, tl!("Override Rotation"));
    ui.add_space(8.0);
    pct(ui, tl!("Pressure"), &mut p.pressure, 1.0);
    widgets::checkbox(ui, &mut p.override_pressure, tl!("Override Pressure"));
}

#[allow(dead_code)] // Smoothing is not part of the reference categories.
fn smoothing(ui: &mut egui::Ui, b: &mut BrushSettings) {
    let s = &mut b.smoothing;
    pct(ui, tl!("Smoothing"), &mut s.amount, 1.0);
    widgets::checkbox(ui, &mut s.pulled_string, tl!("Pulled String Mode"));
    widgets::checkbox(ui, &mut s.catch_up, tl!("Stroke Catch-up"));
    widgets::checkbox(ui, &mut s.catch_up_on_end, tl!("Catch-up on Stroke End"));
    widgets::checkbox(ui, &mut s.adjust_for_zoom, tl!("Adjust for Zoom"));
}

fn material_control(ui: &mut egui::Ui, id: &str, control: &mut Control, fade_steps: &mut u32) {
    ui.horizontal(|ui| {
        dim(ui, tl!("Control:"));
        widgets::dropdown(ui, id, control, &reference_controls(id), 130.0);
        if *control == Control::Fade {
            let mut n = *fade_steps as f32;
            if num(ui, tl!("Fade steps"), &mut n, 1.0..=9999.0, "") {
                *fade_steps = n as u32;
            }
        }
    });
}

fn grain_settings(ui: &mut egui::Ui, settings: &mut Option<GrainSettings>) {
    let mut g = settings.clone().unwrap_or_default();
    let before = g.clone();
    pct(ui, tl!("Grain Density"), &mut g.density, 1.0);
    num(ui, tl!("Grain Size"), &mut g.size, 1.0..=300.0, "");
    pct(ui, tl!("Size Jitter"), &mut g.size_jitter, 1.0);
    pct(ui, tl!("Density Jitter"), &mut g.density_jitter, 1.0);
    material_control(ui, "grain-control", &mut g.control, &mut g.fade_steps);
    if before != g {
        *settings = Some(g.clone());
    }
}

fn wet_settings(ui: &mut egui::Ui, settings: &mut Option<WetEdgeSettings>) {
    let mut w = settings.clone().unwrap_or_default();
    let before = w.clone();
    ui.horizontal(|ui| {
        dim(ui, tl!("Wet Edge Mode"));
        widgets::dropdown(
            ui,
            "wet-style",
            &mut w.style,
            &[(WetStyle::Photoshop, "Photoshop"), (WetStyle::Watercolor, tl!("Watercolor")), (WetStyle::Watercolor2, tl!("Watercolor 2"))],
            140.0,
        );
    });
    material_num(ui, tl!("Strength"), &mut w.strength, 2.0);
    material_num(ui, tl!("Minimum Strength"), &mut w.minimum_strength, 2.0);
    material_control(ui, "wet-control", &mut w.control, &mut w.fade_steps);
    ui.horizontal(|ui| {
        dim(ui, tl!("Width Mode"));
        widgets::dropdown(ui, "wet-width", &mut w.width_mode, &[(WetWidthMode::Ratio, tl!("Ratio")), (WetWidthMode::Fixed, tl!("Fixed Size"))], 140.0);
    });
    if before.width_mode != w.width_mode && w.width_mode == WetWidthMode::Ratio {
        w.minimum_width = w.minimum_width.min(5.0);
    }
    if w.width_mode == WetWidthMode::Fixed {
        num(ui, tl!("Fixed Width"), &mut w.fixed_width, 0.0..=256.0, " px");
    } else {
        material_num(ui, tl!("Width"), &mut w.width, 5.0);
    }
    material_num(ui, tl!("Minimum Width"), &mut w.minimum_width, if w.width_mode == WetWidthMode::Fixed { 256.0 } else { 5.0 });
    ui.horizontal(|ui| {
        dim(ui, tl!("Edge Control"));
        widgets::dropdown(
            ui,
            "wet-edge",
            &mut w.edge_control,
            &[(WetEdgeControl::Expansion, tl!("Expansion")), (WetEdgeControl::SoftCoverage, tl!("Soft Coverage"))],
            140.0,
        );
    });
    ui.horizontal(|ui| {
        dim(ui, tl!("Outline"));
        widgets::dropdown(ui, "wet-outline", &mut w.outline, &[(WetOutline::Stroke, tl!("Whole Stroke")), (WetOutline::Single, tl!("Single Tip"))], 140.0);
    });
    ui.horizontal(|ui| {
        dim(ui, tl!("Color Mode"));
        widgets::dropdown(
            ui,
            "wet-color",
            &mut w.color_mode,
            &[
                (WetColorMode::Normal, tl!("Normal")),
                (WetColorMode::Hsl, "HSL"),
                (WetColorMode::Multiply, tl!("Multiply")),
                (WetColorMode::ColorBurn, tl!("Color Burn")),
            ],
            140.0,
        );
    });
    material_num(ui, tl!("Softness"), &mut w.softness, 5.0);
    pct(ui, tl!("Darkening"), &mut w.brightness, 1.0);
    pct(ui, tl!("Saturation Boost"), &mut w.saturation, 1.0);
    // Strength and width use continuous reference units, not integer pixel rows.
    if w != before {
        *settings = Some(w.clone());
    }
}

fn material_num(ui: &mut egui::Ui, label: &str, value: &mut f32, max: f32) {
    widgets::slider_row(ui, label, value, 0.0..=max, "", None);
}

/// Controls for section `i` (see [`crate::brush_panel::SECTIONS`]). `presets` feed the tip grids.
/// Returns the name of a preset whose tip was picked in the Brush Tip Shape grid, for
/// `brush.presets.setCurrent`.
pub fn section_body(ui: &mut egui::Ui, b: &mut BrushSettings, i: usize, presets: &[BrushPreset], current: Option<&str>) -> Option<String> {
    section_body_with_canvas(ui, b, i, presets, current, &[])
}

pub(crate) fn section_body_with_canvas(
    ui: &mut egui::Ui,
    b: &mut BrushSettings,
    i: usize,
    presets: &[BrushPreset],
    current: Option<&str>,
    _canvases: &[paint::canvas_material::CanvasPreset],
) -> Option<String> {
    let t = Tokens::get(ui.ctx());
    let note = |ui: &mut egui::Ui, s: &str| {
        ui.label(RichText::new(s).color(t.text_faint).font(theme::medium(11.5)));
    };
    match i {
        0 => return tip_shape(ui, b, presets, current),
        1 => {
            size_row(ui, tl!("Size"), &mut b.size, MAX_BRUSH_SIZE, None);
            ui.add_enabled_ui(b.c3_tip_math || matches!(b.tip, TipShape::Round), |ui| {
                pct(ui, tl!("Hardness"), &mut b.hardness, 1.0);
                dynamic(ui, "hardness-control", tl!("Hardness Jitter"), &mut b.hardness_dynamic, 1.0, Some(tl!("Minimum Hardness")), false);
            });
            pct(ui, tl!("Opacity"), &mut b.opacity, 1.0);
            pct(ui, tl!("Flow"), &mut b.flow, 1.0);
            let old_shape = b.shape_dynamics.clone();
            shape_dynamics(ui, b);
            if b.shape_dynamics != old_shape {
                b.shape_dynamics.enabled = true;
            }
            let old_scatter = b.scattering.clone();
            scattering(ui, b);
            if b.scattering != old_scatter {
                b.scattering.enabled = true;
            }
            let old_transfer = b.transfer.clone();
            transfer(ui, b);
            if b.transfer != old_transfer {
                b.transfer.enabled = true;
            }
            grain_settings(ui, &mut b.grain);
            inertia_settings(ui, b);
        }
        2 => motion_settings(ui, b),
        3 => {
            ui.add_enabled_ui(b.texture.enabled, |ui| texture(ui, b));
            widgets::checkbox(ui, &mut b.protect_texture, tl!("Protect Texture"));
        }
        4 => {
            color_attributes(ui, &mut b.color_attributes);
            ui.separator();
            color_dynamics(ui, b);
        }
        5 => {
            ui.add_enabled_ui(b.wet_edges, |ui| wet_settings(ui, &mut b.wet_edge));
        }
        6 => {
            ui.add_enabled_ui(b.mix_settings.enabled, |ui| mix_settings(ui, &mut b.mix_settings));
        }
        7 => pigment_settings(ui, &mut b.pigment, true),
        _ => note(ui, ""),
    }
    None
}

fn signed_curve(ui: &mut egui::Ui, id: &str, curve: &mut Option<paint::pigment_material::SignedCurve>) {
    let mut value = curve.clone().unwrap_or_else(|| paint::pigment_material::SignedCurve {
        points: std::array::from_fn(|i| {
            let x = i as f32 / 6.0;
            [x, x]
        }),
        enabled: true,
    });
    if effect_curve_graph(ui, id, &mut value, true) {
        *curve = Some(value);
    }
}

fn effect_curve_graph(ui: &mut egui::Ui, id: &str, curve: &mut paint::pigment_material::SignedCurve, signed: bool) -> bool {
    let before = curve.clone();
    let minimum = if signed { -1.0 } else { 0.0 };
    ui.push_id(id, |ui| {
        if signed {
            widgets::checkbox(ui, &mut curve.enabled, tl!("Enabled"));
        }
        let t = Tokens::get(ui.ctx());
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width().min(260.0), 120.0), Sense::click_and_drag());
        let graph = rect.shrink(7.0);
        ui.painter().rect_filled(rect, t.radius_sm, t.field);
        ui.painter().line_segment([pos2(graph.left(), graph.center().y), pos2(graph.right(), graph.center().y)], Stroke::new(1.0, t.field_border));
        let to_pos = |p: [f32; 2]| pos2(graph.left() + p[0] * graph.width(), graph.bottom() - (p[1] - minimum) / (1.0 - minimum) * graph.height());
        let points: Vec<_> = (0..=128)
            .map(|i| {
                let x = i as f32 / 128.0;
                to_pos([x, curve.value(x)])
            })
            .collect();
        ui.painter().add(egui::Shape::line(points, Stroke::new(1.5, t.accent)));
        for p in curve.points {
            ui.painter().circle_filled(to_pos(p), 3.5, t.text);
        }
        if (response.dragged() || response.clicked())
            && let Some(pos) = response.interact_pointer_pos()
        {
            let key = ui.id().with("signed-curve-point");
            let selected = (if response.drag_started() || response.clicked() {
                let nearest = curve
                    .points
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| to_pos(**a).distance_sq(pos).total_cmp(&to_pos(**b).distance_sq(pos)))
                    .map_or(0, |(i, _)| i);
                ui.data_mut(|d| d.insert_temp(key, nearest));
                nearest
            } else {
                ui.data(|d| d.get_temp::<usize>(key)).unwrap_or(0)
            })
            .min(6);
            let y = ((graph.bottom() - pos.y) / graph.height() * (1.0 - minimum) + minimum).clamp(minimum, 1.0);
            let x = if selected == 0 {
                0.0
            } else if selected == 6 {
                1.0
            } else {
                let lo = curve.points[selected - 1][0];
                let hi = curve.points[selected + 1][0];
                let margin = ((hi - lo) * 0.1).min(0.001);
                ((pos.x - graph.left()) / graph.width()).clamp(lo + margin, hi - margin)
            };
            curve.points[selected] = [x, y];
            curve.normalize_handles_with_minimum(minimum);
        }
    });
    *curve != before
}

pub(crate) fn pigment_settings(ui: &mut egui::Ui, settings: &mut Option<paint::pigment_material::PigmentSettings>, _primary: bool) {
    let mut p = settings.clone().unwrap_or_default();
    let before = p.clone();
    for (id, label, value, minimum, jitter, control) in [
        ("thickness", "Thickness", &mut p.thickness, &mut p.thickness_minimum, &mut p.thickness_jitter, &mut p.thickness_control),
        (
            "visual-height",
            "Texture Strength",
            &mut p.visual_thickness,
            &mut p.visual_thickness_minimum,
            &mut p.visual_thickness_jitter,
            &mut p.visual_thickness_control,
        ),
    ] {
        ui.push_id(id, |ui| {
            widgets::slider_row(ui, tl!(label), value, 0.0..=100.0, "", None);
            widgets::slider_row(ui, tl!("Minimum"), minimum, 0.0..=100.0, "", None);
            widgets::slider_row(ui, tl!("Jitter"), jitter, 0.0..=100.0, "", None);
            material_control(ui, id, control, &mut 25);
        });
    }
    signed_curve(ui, "pigment-thickness-curve", &mut p.thickness_curve);
    pct(ui, tl!("Highlight Strength"), &mut p.highlight_strength, 1.0);
    pct(ui, tl!("Highlight Brightness"), &mut p.highlight_brightness, 1.0);
    pct(ui, tl!("Highlight Ratio"), &mut p.highlight_ratio, 1.0);
    if p != before {
        *settings = Some(p);
    }
}

fn mix_settings(ui: &mut egui::Ui, m: &mut paint::transport::BrushMixSettings) {
    use paint::transport::MixAlgorithm as A;
    widgets::dropdown(ui, "mix-algorithm", &mut m.algorithm, &[(A::V1, "v1"), (A::V2, "v2"), (A::V3, "v3")], 130.0);
    reference_effect_curve(ui, "mix-pressure-curve", tl!("Mixing Strength"), &mut m.mix_pressure_curve, m.strength);
    reference_effect_curve(ui, "blur-pressure-curve", tl!("Blur Strength"), &mut m.blur_pressure_curve, m.blur_strength);
}

fn reference_effect_curve(ui: &mut egui::Ui, id: &str, label: &str, curve: &mut Option<paint::pigment_material::SignedCurve>, strength: f32) {
    let mut value = curve.clone().unwrap_or_else(|| paint::pigment_material::SignedCurve {
        points: [[0.0, 0.0], [0.1, 0.0], [0.2, strength], [0.3, strength], [0.53, strength], [0.77, 0.0], [1.0, 0.0]],
        enabled: true,
    });
    let before = value.clone();
    ui.push_id(id, |ui| {
        widgets::checkbox(ui, &mut value.enabled, label);
    });
    effect_curve_graph(ui, id, &mut value, false);
    if value != before {
        *curve = Some(value);
    }
}

pub(crate) fn color_attributes(ui: &mut egui::Ui, attributes: &mut paint::brush::ColorAttributes) {
    let modes: Vec<_> = paint::AttributeMode::ALL.into_iter().map(|m| (m, m.label())).collect();
    ui.label(tl!("Blend Attribute"));
    widgets::dropdown(ui, "blend-attribute", &mut attributes.blend, &modes, 130.0);
    pct(ui, tl!("Strength"), &mut attributes.blend_strength, 1.0);
    ui.label(tl!("Color Attribute"));
    ui.horizontal(|ui| {
        widgets::dropdown(ui, "color-attribute", &mut attributes.color, &modes, 130.0);
        let mut value = (attributes.color_strength * 100.0).round();
        let response = ui
            .scope(|ui| {
                ui.set_width(88.0);
                widgets::slider(ui, &mut value, 0.0..=100.0, None).on_hover_text(tl!("Strength"))
            })
            .inner;
        if response.changed() {
            attributes.color_strength = value / 100.0;
        }
        if widgets::value_field(ui, &mut value, 0.0..=100.0, "%", 52.0).changed() {
            attributes.color_strength = value / 100.0;
        }
    });
}

fn motion_settings(ui: &mut egui::Ui, b: &mut BrushSettings) {
    use paint::motion::GuideMode;
    let motion = &mut b.motion;
    ui.add_enabled_ui(motion.enabled, |ui| {
        widgets::dropdown(ui, "motion-guide-mode", &mut motion.guide_mode, &[(GuideMode::Off, "Off"), (GuideMode::Anchor, "Anchor")], 150.0);
        signed_pct(ui, tl!("Anchor X"), &mut motion.anchor_point_x, -1.0, 1.0);
        signed_pct(ui, tl!("Anchor Y"), &mut motion.anchor_point_y, -1.0, 1.0);
        pct(ui, tl!("Anchor Delay"), &mut motion.anchor_delay, 1.0);
        pct(ui, tl!("Anchor Smooth"), &mut motion.anchor_smooth, 1.0);
        pct(ui, tl!("Stroke Smoothing"), &mut motion.smoothing, 1.0);
        widgets::checkbox(ui, &mut motion.anchor_delay_pressure_enabled, tl!("Pressure Controls Anchor Delay"));
        pct(ui, tl!("Tilt Delay"), &mut motion.tilt_delay, 1.0);
    });
}

fn inertia_settings(ui: &mut egui::Ui, b: &mut BrushSettings) {
    let inertia = &mut b.inertia;
    let before = inertia.clone();
    pct(ui, tl!("Inertia"), &mut inertia.amount, 1.0);
    num(ui, tl!("Minimum Distance"), &mut inertia.min_distance, 0.0..=10000.0, " px");
    widgets::dropdown(ui, "inertia-control", &mut inertia.control, &reference_controls("inertia-control"), 160.0);
    if *inertia != before {
        inertia.enabled = inertia.amount > 0.0;
    }
}

#[allow(dead_code)] // Canvas material is not part of the reference categories.
fn canvas_settings(ui: &mut egui::Ui, b: &mut BrushSettings, presets: &[BrushPreset], canvases: &[paint::canvas_material::CanvasPreset]) {
    let Some(m) = b.canvas_material.as_mut() else { return };
    ui.horizontal(|ui| {
        dim(ui, tl!("Pattern:"));
        let selected = canvases
            .iter()
            .find(|p| Some(p.id.as_str()) == m.preset_id.as_deref())
            .map(|p| p.name.as_str())
            .or(m.preset_id.as_deref())
            .unwrap_or(tl!("Choose Pattern"));
        egui::ComboBox::from_id_salt("canvas-material-preset").selected_text(selected).show_ui(ui, |ui| {
            for preset in canvases {
                if ui.selectable_label(m.preset_id.as_deref() == Some(&preset.id), &preset.name).clicked() {
                    *m = preset.material.clone();
                    m.enabled = true;
                }
            }
            for preset in presets {
                if let Some(other) = &preset.brush.canvas_material
                    && other.pattern.bitmap().is_some()
                    && ui.selectable_label(m.preset_id == other.preset_id, &preset.name).clicked()
                {
                    *m = other.clone();
                    m.enabled = true;
                }
            }
        });
    });
    if m.pattern.bitmap().is_none() {
        dim(ui, tl!("Import a C3 preset with a packaged canvas pattern."));
    }
    pct(ui, tl!("Scale"), &mut m.scale, 10.0);
    m.scale = m.scale.max(0.01);
    pct(ui, tl!("Strength"), &mut m.strength, 1.0);
    let modes: Vec<_> = [
        paint::AttributeMode::Normal,
        paint::AttributeMode::Multiply,
        paint::AttributeMode::Overlay,
        paint::AttributeMode::WatercolorBurn,
        paint::AttributeMode::LinearBurn,
    ]
    .into_iter()
    .map(|m| (m, m.label()))
    .collect();
    widgets::dropdown(ui, "canvas-material-mode", &mut m.blend_mode, &modes, 160.0);
    let mut curve = canvas_edit_curve(&m.curve);
    if effect_curve_graph(ui, "canvas-color-curve", &mut curve, false) {
        m.set_curve(curve.points.to_vec());
    }
}

fn canvas_edit_curve(points: &[[f32; 2]]) -> paint::pigment_material::SignedCurve {
    let points = if let Ok(points) = <[[f32; 2]; 7]>::try_from(points) {
        points
    } else {
        let at = |x: f32| {
            points.windows(2).find(|p| x <= p[1][0]).map_or(x, |p| {
                let t = ((x - p[0][0]) / (p[1][0] - p[0][0]).max(f32::EPSILON)).clamp(0.0, 1.0);
                p[0][1] + (p[1][1] - p[0][1]) * t
            })
        };
        let anchors = [[0.0, points.first().map_or(0.0, |p| p[1])], [0.5, at(0.5)], [1.0, points.last().map_or(1.0, |p| p[1])]];
        std::array::from_fn(|i| {
            let lane = usize::from(i > 3);
            let t = (i - lane * 3) as f32 / 3.0;
            std::array::from_fn(|axis| anchors[lane][axis] + (anchors[lane + 1][axis] - anchors[lane][axis]) * t)
        })
    };
    paint::pigment_material::SignedCurve { points, enabled: true }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::accesskit::Role;
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };

    fn minimum_brush() -> BrushSettings {
        let mut b = BrushSettings { size: 137.0, ..Default::default() };
        b.shape_dynamics.size.minimum = 0.2;
        b.shape_dynamics.roundness.minimum = 0.35;
        b.transfer.opacity.minimum = 0.4;
        b.transfer.flow.minimum = 0.55;
        b.transfer.wetness.minimum = 0.6;
        b.transfer.mix.minimum = 0.75;
        b
    }

    fn section_harness(section: usize, b: BrushSettings) -> Harness<'static, BrushSettings> {
        let mut ready = false;
        let mut h = Harness::builder().with_size(vec2(340.0, 3600.0)).build_ui_state(
            move |ui, b: &mut BrushSettings| {
                if !ready {
                    crate::PhotocraftApp::setup_context(ui.ctx(), theme::ThemeKind::ProMedium);
                    ready = true;
                    return;
                }
                section_body(ui, b, section, &[], None);
            },
            b,
        );
        h.run_steps(3);
        h
    }

    /// #3825: jitter is an amount, not a hue selector; it uses the themed amount track.
    #[test]
    fn hue_jitter_paints_an_amount_track_in_every_theme() {
        for kind in theme::ThemeKind::ALL {
            let mut b = BrushSettings::default();
            b.color_dynamics.hue_jitter = 0.37;
            let mut h = section_harness(4, b.clone());
            crate::PhotocraftApp::setup_context(&h.ctx, kind);
            h.run_steps(3);
            let label = h.get_by_label("Hue Jitter").rect();
            let t = Tokens::get(&h.ctx);
            assert!(
                h.output().shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(r)
                    if r.rect.width() > 150.0 && (r.rect.height() - 3.0).abs() < 0.1
                    && r.rect.top() > label.bottom() && r.rect.bottom() < label.bottom() + 30.0
                    && r.fill == t.field_border)),
                "{kind:?}: Hue Jitter paints the same neutral track as other percentage amounts"
            );
            assert!(h.query_all_by_role(Role::SpinButton).any(|n| n.accesskit_node().numeric_value() == Some(37.0)));
            assert_eq!(h.state(), &b, "rendering does not change saved brush settings");
        }
    }

    #[test]
    fn secondary_tip_panel_exposes_independent_opacity_and_flow() {
        let mut brush = BrushSettings::default();
        brush.dual_brush.enabled = true;
        brush.dual_brush.opacity = 0.63;
        brush.dual_brush.flow = 0.28;
        let secondary = secondary_defaults(&brush);
        let h = section_harness(1, secondary);
        assert!(h.get_by_label("Opacity").rect().is_positive());
        assert!(h.get_by_label("Flow").rect().is_positive());
        assert_eq!(h.state().opacity, 0.63);
        assert_eq!(h.state().flow, 0.28);
    }

    fn set_controls(b: &mut BrushSettings, control: Control) {
        for d in [
            &mut b.shape_dynamics.size,
            &mut b.shape_dynamics.roundness,
            &mut b.transfer.opacity,
            &mut b.transfer.flow,
            &mut b.transfer.wetness,
            &mut b.transfer.mix,
        ] {
            d.control = control;
        }
    }

    /// #3825: all six minimum fields follow their own Control; jitter remains editable.
    #[test]
    fn minimum_fields_are_disabled_only_when_their_control_is_off() {
        for (section, values) in [(1, vec![20.0, 35.0]), (1, vec![40.0, 55.0])] {
            let mut h = section_harness(section, minimum_brush());
            for (control, _) in CONTROLS {
                set_controls(h.state_mut(), control);
                h.run_steps(2);
                for value in &values {
                    let field = h.query_all_by_role(Role::SpinButton).find(|n| n.accesskit_node().numeric_value() == Some(*value)).expect("minimum input");
                    assert_eq!(field.accesskit_node().is_disabled(), control == Control::Off, "section {section}, minimum {value}, {control:?}");
                }
                assert!(
                    h.query_all_by_role(Role::SpinButton).any(|n| n.accesskit_node().numeric_value() == Some(0.0) && !n.accesskit_node().is_disabled()),
                    "jitter stays editable"
                );
            }
            set_controls(h.state_mut(), Control::PenPressure);
            if values[0] == 20.0 {
                h.state_mut().shape_dynamics.size.control = Control::Off;
            } else {
                h.state_mut().transfer.opacity.control = Control::Off;
            }
            h.run_steps(2);
            for (index, value) in values.iter().enumerate() {
                let field = h.query_all_by_role(Role::SpinButton).find(|n| n.accesskit_node().numeric_value() == Some(*value)).unwrap();
                assert_eq!(field.accesskit_node().is_disabled(), index == 0, "each minimum follows its own control");
            }
        }
    }

    fn drag(h: &mut Harness<'static, BrushSettings>, from: egui::Pos2, to: egui::Pos2) {
        h.event(egui::Event::PointerMoved(from));
        h.run_steps(1);
        h.event(egui::Event::PointerButton { pos: from, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE });
        h.run_steps(1);
        h.event(egui::Event::PointerMoved(to));
        h.run_steps(1);
        h.event(egui::Event::PointerButton { pos: to, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
        h.run_steps(2);
    }

    #[test]
    fn disabled_minimum_ignores_input_and_keeps_its_value_for_reactivation() {
        let mut h = section_harness(1, minimum_brush());
        let before = h.state().clone();
        let field = h.query_all_by_role(Role::SpinButton).find(|n| n.accesskit_node().numeric_value() == Some(20.0)).unwrap();
        let center = field.rect().center();
        field.click();
        h.key_press(egui::Key::ArrowUp);
        h.event(egui::Event::Text("99".into()));
        h.key_press(egui::Key::Enter);
        // The slider below the number and its scrubby label are disabled too.
        let track = pos2(300.0, center.y + 24.0);
        drag(&mut h, track, track);
        let label = h.get_by_label("Minimum Diameter").rect().center();
        drag(&mut h, label, label + vec2(60.0, 0.0));
        assert_eq!(h.state(), &before, "disabled interactions never rewrite brush settings");

        h.state_mut().shape_dynamics.size.control = Control::PenPressure;
        h.run_steps(2);
        let field = h.query_all_by_role(Role::SpinButton).find(|n| n.accesskit_node().numeric_value() == Some(20.0)).unwrap();
        assert!(!field.accesskit_node().is_disabled());
        field.click();
        h.key_press(egui::Key::ArrowUp);
        h.run_steps(2);
        assert_eq!(h.state().shape_dynamics.size.minimum, 0.21, "active minimum is editable");
        drag(&mut h, track, track);
        let minimum = h.state().shape_dynamics.size.minimum;
        assert!(minimum > 0.8, "the active slider accepts the same pointer position: {minimum}");
        h.state_mut().shape_dynamics.size.control = Control::Off;
        h.run_steps(2);
        assert_eq!(h.state().shape_dynamics.size.minimum, minimum, "Off keeps the saved minimum");
    }

    #[test]
    fn ellipse_widget_drags_angle_and_roundness() {
        // Pointing straight up is 90° (counter-clockwise from +x; screen y is down).
        assert_eq!(ellipse_drag(EllipseGrab::Angle, vec2(0.0, -20.0), 0.0, 1.0), (90.0, 1.0));
        assert_eq!(ellipse_drag(EllipseGrab::Angle, vec2(-20.0, 0.0), 0.0, 1.0).0.abs(), 180.0);
        // The centre itself doesn't change the angle.
        assert_eq!(ellipse_drag(EllipseGrab::Angle, vec2(0.2, 0.1), 30.0, 0.5), (30.0, 0.5));
        // At angle 0 the minor axis is vertical: half the radius up is 50 % roundness.
        let (_, r) = ellipse_drag(EllipseGrab::Roundness, vec2(0.0, -ELLIPSE_R / 2.0), 0.0, 1.0);
        assert!((r - 0.5).abs() < 1e-4);
        // Handles sit on the minor axis and are what a press there grabs.
        let [h, _] = roundness_handles(0.0, 0.5);
        assert!((h - vec2(0.0, -ELLIPSE_R / 2.0)).length() < 1e-4);
        assert_eq!(ellipse_grab(h, 0.0, 0.5), EllipseGrab::Roundness);
        assert_eq!(ellipse_grab(vec2(ELLIPSE_R, 0.0), 0.0, 0.5), EllipseGrab::Angle);
        // Rotated 90°: the minor axis is horizontal.
        let [h, _] = roundness_handles(90.0, 1.0);
        assert!((h.x.abs() - ELLIPSE_R).abs() < 1e-3 && h.y.abs() < 1e-3, "{h:?}");
        let (_, r) = ellipse_drag(EllipseGrab::Roundness, vec2(500.0, 0.0), 90.0, 0.2);
        assert_eq!(r, 1.0);
    }
}
