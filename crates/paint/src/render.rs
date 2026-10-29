//! Dab rasterisation and stroke compositing.
//!
//! A stroke paints into a sparse, tiled *stroke buffer* (coverage plus, with Color Dynamics, a
//! premultiplied colour), exactly like a private stroke layer: dabs build up with flow up to their
//! opacity ceiling, stroke-level masks (Dual Brush, Texture) apply when the buffer is composited,
//! and the whole buffer composites onto the pre-stroke pixels at the stroke opacity. Because the
//! buffer is sparse and compositing always starts from the pre-stroke pixels, strokes can be fed in
//! chunks and re-composited incrementally for interactive painting (see [`StrokeRenderer`]).

use crate::material::{ResolvedWet, WetOutline, WetStyle};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use photocraft_color::{BlendMode, ColorMode, PixelFormat, SampleType};
use photocraft_geom::{Point, Rect};
use photocraft_raster::{Surface, from_rgba_into, to_rgba};

use crate::brush::{BrushSettings, MaskMode, Pattern, TipShape};
use crate::dynamics::DabGenerator;
use crate::retouch::{Footprint, alpha_index, over_native};
use crate::rng::hash2;
use crate::tile::{Mips, PatternImage};
use crate::{Dab, StrokePoint, dab_coverage, tip_falloff, tip_reach};

/// Salt of the 8-bit dither's per-pixel noise.
const DITHER_SALT: u64 = 0x0000_6469_7468_6572;

/// Before a stroke's result is quantised to 8 bits, offset every channel of the pixel by the same
/// deterministic noise of under half a level, so fractional values round up or down in proportion
/// (a soft, low-flow stroke's fine gradient survives as grain instead of bands). A value already on
/// an 8-bit level still rounds back to itself, and because the noise depends only on the pixel's
/// position, the live preview, the commit and a replay all agree.
#[inline]
fn dither8(px: &mut [f32], x: i32, y: i32) {
    let d = (hash2(x, y, DITHER_SALT) - 0.5) / 255.0;
    for v in px.iter_mut() {
        *v += d;
    }
}

/// Edge of a stroke-buffer tile.
pub const COV_TILE: i32 = 64;
/// Computed tips with a smaller radius (in pixels) are rasterised by area, not at pixel centres.
const SMALL_TIP_RADIUS: f32 = 3.0;
/// Sub-pixel sample offsets (a 4×4 grid) for the area coverage of small tips.
const SUBPIXEL: [f32; 4] = [-0.375, -0.125, 0.125, 0.375];
const NOISE_SALT: u64 = 0x006E_6F69_7365;

#[inline]
/// Where an aliased (Pencil) dab of `diameter` pixels centred near (`x`, `y`) lands on the pixel
/// grid: the centre of the pixel holding the point for odd diameters, the nearest pixel corner
/// for even ones. Its footprint is then a whole-pixel block around that point, the same square
/// the Pencil cursor shows ([`grid_square`]).
pub fn grid_center(x: f64, y: f64, diameter: f32) -> (f32, f32) {
    let n = if diameter.is_finite() { diameter.round().max(1.0) as i64 } else { 1 };
    let snap = |v: f64| if n % 2 == 1 { v.floor() + 0.5 } else { v.round() };
    (snap(x) as f32, snap(y) as f32)
}

/// The whole-pixel square `[x0, y0, x1, y1]` an aliased dab of `diameter` pixels at (`x`, `y`)
/// can touch (see [`grid_center`]): the Pencil's cursor.
pub fn grid_square(x: f64, y: f64, diameter: f32) -> [f64; 4] {
    let n = if diameter.is_finite() { f64::from(diameter.round().clamp(1.0, 100_000.0)) } else { 1.0 };
    let (cx, cy) = grid_center(x, y, n as f32);
    let h = n / 2.0;
    [f64::from(cx) - h, f64::from(cy) - h, f64::from(cx) + h, f64::from(cy) + h]
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Wet Edges: how much paint stays at a pixel whose nearest dab reaches it at `rn` (0 at a dab's
/// centre, 1 at its rim). The interior keeps half the paint, the rim all of it.
#[inline]
fn wet_factor(rn: f32) -> f32 {
    0.5 + 0.5 * smoothstep(0.5, 1.0, rn)
}

/// Combine brush coverage `v` with a mask value `t` (texture/dual tip) at `depth`.
/// Every mode maps `v = 0` to 0 (a mask never adds paint where the brush has none).
pub fn mask_combine(mode: MaskMode, v: f32, t: f32, d: f32) -> f32 {
    if v <= 0.0 {
        return 0.0;
    }
    let lerp = |a: f32, b: f32| a + (b - a) * d;
    let r = match mode {
        MaskMode::Normal => v + d * t - v * d * t,
        MaskMode::Multiply => v * (1.0 - d + d * t),
        MaskMode::Subtract => (v - d * (1.0 - t)).max(0.0),
        MaskMode::Darken => v.min(1.0 - d + d * t),
        MaskMode::Overlay => {
            let o = if v < 0.5 { 2.0 * v * t } else { 1.0 - 2.0 * (1.0 - v) * (1.0 - t) };
            lerp(v, o)
        }
        MaskMode::ColorDodge => lerp(v, (v / (1.0 - t).max(1e-3)).min(1.0)),
        MaskMode::ColorBurn => lerp(v, 1.0 - ((1.0 - v) / t.max(1e-3)).min(1.0)),
        MaskMode::LinearBurn => lerp(v, (v + t - 1.0).max(0.0)),
        MaskMode::HardMix => lerp(v, if v + t > 1.0 { 1.0 } else { 0.0 }),
        MaskMode::LinearHeight => v * ((t - (1.0 - d * v)) * 4.0 + 0.5).clamp(0.0, 1.0),
        MaskMode::Height => {
            if t >= 1.0 - d * v {
                v
            } else {
                0.0
            }
        }
    };
    r.clamp(0.0, 1.0)
}

/// C3 combines completed lane coverages; lifting modes also show a secondary-only footprint.
pub fn dual_blend(mode: MaskMode, p: f32, s: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    let s = s.clamp(0.0, 1.0);
    let lifts = matches!(mode, MaskMode::Normal | MaskMode::Darken | MaskMode::Overlay | MaskMode::LinearBurn | MaskMode::HardMix);
    if p <= 0.001 {
        return if lifts { s } else { 0.0 };
    }
    let alpha = match mode {
        MaskMode::Normal => p + s - p * s,
        MaskMode::Multiply => p * s,
        MaskMode::Darken => p.min(s),
        MaskMode::Overlay => {
            if p < 0.5 {
                2.0 * p * s
            } else {
                1.0 - 2.0 * (1.0 - p) * (1.0 - s)
            }
        }
        MaskMode::ColorDodge => {
            if s >= 1.0 {
                1.0
            } else {
                (p / (1.0 - s).max(0.0001)).min(1.0)
            }
        }
        MaskMode::ColorBurn => {
            if s <= 0.0 {
                0.0
            } else {
                (1.0 - (1.0 - p) / s.max(0.0001)).max(0.0)
            }
        }
        MaskMode::LinearBurn => (p + s - 1.0).max(0.0),
        MaskMode::HardMix => {
            if p + s >= 1.0 {
                1.0
            } else {
                0.0
            }
        }
        MaskMode::LinearHeight => p * (0.5 + 0.5 * s),
        MaskMode::Subtract | MaskMode::Height => mask_combine(mode, p, s, 1.0),
    };
    if lifts { alpha } else { alpha.min(p) }
}

const HEIGHT_REACH: f32 = 12.0;

/// Combine brush coverage `v` with a Texture pattern value `t` at `depth` (Brush Settings ›
/// Texture). Where it differs from [`mask_combine`] (still used for the Dual Brush) it follows
/// Photoshop as measured with single dabs over a grey ramp (#3340):
/// - Subtract takes `depth × t` away, so a white pattern removes the most paint.
/// - Color Dodge and Color Burn move the pattern toward their neutral colour as depth falls.
/// - Height treats the pattern as a height field the brush presses into: the paint reaches
///   `12 × depth × v` and fills whatever lies below that, so a low depth paints only the dark
///   valleys, from about 17 % the whole pattern is under the paint, and a brighter pattern
///   takes paint away.
/// - Linear Height is Height, but where the reach stays below the top of the pattern the paint
///   fades with the pattern instead of being cut off.
pub fn texture_combine(mode: MaskMode, v: f32, t: f32, d: f32) -> f32 {
    if v <= 0.0 {
        return 0.0;
    }
    let r = match mode {
        MaskMode::Subtract => v - d * t,
        MaskMode::ColorDodge => v / (1.0 - d * t).max(1e-3),
        MaskMode::ColorBurn => 1.0 - (1.0 - v) / (1.0 - d + d * t).max(1e-3),
        // Photoshop steps the depth in 1/255ths here, which shows at the lowest depths.
        MaskMode::Height => HEIGHT_REACH * (d * 255.0).round() / 255.0 * v - t,
        MaskMode::LinearHeight => {
            let reach = HEIGHT_REACH * (d * 255.0).round() / 255.0 * v;
            if reach < 1.0 { reach * (1.0 - t) } else { reach - t }
        }
        _ => mask_combine(mode, v, t, d),
    };
    if r.is_finite() { r.clamp(0.0, 1.0) } else { 0.0 }
}

/// A stroke-ready brush: the settings plus prepared tip mips and texture.
#[derive(Clone, Debug)]
pub struct BrushContext {
    pub brush: BrushSettings,
    tip: Option<Arc<Mips>>,
    dual_tip: Option<Arc<Mips>>,
    texture: Option<Arc<PatternImage>>,
    secondary: Option<Box<BrushContext>>,
    texture_origin: Option<Point>,
    texture_tip: Option<Dab>,
}

fn prepare_pattern(b: &BrushSettings) -> Option<Arc<PatternImage>> {
    let t = &b.texture;
    if !t.enabled {
        return None;
    }
    let mut img = match &t.pattern {
        Pattern::Procedural { style, size, seed } => crate::procedural::pattern(*style, *size, *seed),
        p => match p.bitmap() {
            Some(g) if g.is_valid() => PatternImage { width: g.width as usize, height: g.height as usize, data: g.to_f32() },
            _ => return None,
        },
    };
    for v in &mut img.data {
        *v = t.level(*v);
    }
    Some(Arc::new(img))
}

/// The pixel rectangle `reach` (plus 1 px of anti-aliasing slack) around a dab's centre. The casts
/// saturate and the arithmetic saturates, so a far or non-finite centre or reach from a direct
/// caller gives a (possibly empty) rectangle instead of an `i32` overflow (#977).
pub(crate) fn rect_around(d: &Dab, reach: f32) -> Rect {
    let rr = (reach.ceil() as i32).saturating_add(1);
    let (cx, cy) = (d.center.x.floor() as i32, d.center.y.floor() as i32);
    Rect::new(cx.saturating_sub(rr), cy.saturating_sub(rr), cx.saturating_add(rr).saturating_add(1), cy.saturating_add(rr).saturating_add(1))
}

impl BrushContext {
    pub fn new(brush: &BrushSettings) -> Self {
        let brush = brush.bounded_for_render();
        let mips = |t: &TipShape| match t.bitmap() {
            Some(g) if g.is_valid() => Some(Arc::new(Mips::new(g))),
            _ => None,
        };
        Self {
            texture_origin: None,
            texture_tip: None,
            tip: mips(&brush.tip),
            dual_tip: if brush.dual_brush.enabled { mips(&brush.dual_brush.tip) } else { None },
            texture: prepare_pattern(&brush),
            secondary: brush.dual_brush.enabled.then(|| brush.dual_brush.resolved_brush(&brush)).flatten().as_ref().map(|b| Box::new(Self::new(b))),
            brush,
        }
    }

    /// Texture value at a document pixel (bilinear, tiled).
    #[inline]
    pub fn texture_at(&self, x: i32, y: i32) -> f32 {
        self.texture_at_point(x as f32 + 0.5, y as f32 + 0.5, self.texture_tip.as_ref())
    }

    fn texture_at_point(&self, mut x: f32, mut y: f32, dab: Option<&Dab>) -> f32 {
        use crate::brush::TextureCoordinateMode;
        let Some(p) = &self.texture else { return 1.0 };
        match self.brush.texture.coordinate_mode {
            TextureCoordinateMode::CanvasLocked => {}
            TextureCoordinateMode::StrokeLocked => {
                if let Some(origin) = self.texture_origin {
                    x -= origin.x as f32;
                    y -= origin.y as f32;
                }
            }
            TextureCoordinateMode::TipLocal => {
                if let Some(d) = dab {
                    let dx = x - d.center.x as f32;
                    let dy = -(y - d.center.y as f32);
                    let (sn, cs) = (if self.brush.c3_tip_math { -d.angle } else { d.angle }).sin_cos();
                    let u = (dx * cs + dy * sn) * if d.flip_x { -1.0 } else { 1.0 };
                    let v = (-dx * sn + dy * cs) * if d.flip_y { -1.0 } else { 1.0 };
                    x = u + d.radius;
                    y = (if self.brush.c3_tip_math { -v } else { v }) / d.roundness.max(0.001) + d.radius;
                }
            }
        }
        let scale = self.brush.texture.scale.max(0.01);
        p.sample_wrap(x / scale - 0.5, y / scale - 0.5)
    }

    /// Bounding rectangle of a dab (rotated sampled tips need the corner reach).
    pub fn dab_rect(&self, d: &Dab, dual: bool) -> Rect {
        if dual && let Some(ctx) = &self.secondary {
            return ctx.dab_rect(d, false);
        }
        let sampled = if dual { self.dual_tip.is_some() } else { self.tip.is_some() };
        let hardness = if dual {
            if self.brush.dual_brush.hardness_dynamic.is_active() { d.hardness } else { self.brush.dual_brush.hardness }
        } else if self.brush.hardness_dynamic.is_active() || self.brush.pigment.is_some() || self.brush.contour.is_some() {
            d.hardness
        } else {
            self.brush.hardness
        };
        // A computed soft tip paints past its nominal radius (see `tip_shape`).
        let aliased = self.brush.aliased && !dual;
        let reach = if sampled || self.brush.triangle {
            d.radius * std::f32::consts::SQRT_2
        } else if aliased || self.brush.c3_tip_math {
            d.radius + 1.0
        } else {
            crate::tip_reach(d.radius, hardness).max(d.radius)
        };
        rect_around(d, reach)
    }

    /// Rasterise a dab over `rect` into `out` (tip shape × noise × per-tip texture × wet edges × flow).
    /// Wet Edges shapes this one dab; a stroke applies it to its accumulated coverage instead.
    pub fn rasterize(&self, d: &Dab, dual: bool, rect: Rect, out: &mut Vec<f32>) {
        self.rasterize_into(d, dual, rect, out, None);
    }

    /// [`rasterize`](Self::rasterize); with `depth`, Wet Edges is left out of `out` and each
    /// painted pixel's depth inside the dab (`1 − rn`) goes to `depth`, for the stroke to apply it.
    pub fn rasterize_into(&self, d: &Dab, dual: bool, rect: Rect, out: &mut Vec<f32>, mut depth: Option<&mut Vec<f32>>) {
        if dual && let Some(ctx) = &self.secondary {
            ctx.rasterize_into(d, false, rect, out, depth);
            return;
        }
        let (w, h) = (rect.width() as usize, rect.height() as usize);
        out.clear();
        out.resize(w * h, 0.0);
        if let Some(dp) = depth.as_deref_mut() {
            dp.clear();
            dp.resize(w * h, 0.0);
        }
        let b = &self.brush;
        let hardness = if dual {
            if b.dual_brush.hardness_dynamic.is_active() { d.hardness } else { b.dual_brush.hardness }
        } else if b.hardness_dynamic.is_active() || b.pigment.is_some() || b.contour.is_some() {
            d.hardness
        } else {
            b.hardness
        };
        let mips = if dual { self.dual_tip.as_deref() } else { self.tip.as_deref() };
        let aliased = b.aliased && !dual;
        let wet_settings = if dual {
            b.dual_brush.wet_edges.then_some(b.dual_brush.wet_edge.as_ref()).flatten()
        } else {
            b.wet_edges.then_some(b.wet_edge.as_ref()).flatten()
        };
        let single_wet = wet_settings.filter(|w| !b.erase && w.outline == WetOutline::Single);
        let wet = b.wet_edges && b.wet_edge.is_none() && !dual && depth.is_none();
        let grain = if dual { b.dual_brush.grain.as_ref() } else { b.noise.then_some(b.grain.as_ref()).flatten() };
        let noise = b.noise && b.grain.is_none() && !dual;
        let tex_tip = !dual && b.texture.enabled && b.texture.each_tip && self.texture.is_some();
        let (cx, cy) = if aliased { grid_center(d.center.x, d.center.y, 2.0 * d.radius) } else { (d.center.x as f32, d.center.y as f32) };
        let (sn, cs) = (if b.c3_tip_math { -d.angle } else { d.angle }).sin_cos();
        let (fx, fy) = (if d.flip_x { -1.0 } else { 1.0 }, if d.flip_y { -1.0 } else { 1.0 });
        // Brush Projection: stretch the sampling coordinates along the tilt direction, which
        // foreshortens the tip there by `proj_scale`.
        let proj = (!dual && d.proj_scale < 0.999).then(|| {
            let (ps, pc) = d.proj_angle.sin_cos();
            (pc, ps, 1.0 / d.proj_scale.max(0.05) - 1.0)
        });
        let r = d.radius;
        let ro = if b.c3_tip_math { d.roundness.max(0.01) } else { d.roundness.max(0.5 / r).min(1.0) };
        let rm = (r * ro).max(0.5);
        let reach = if b.c3_tip_math {
            rm + 1.0
        } else if aliased || mips.is_some() {
            rm
        } else {
            crate::tip_reach(rm, hardness).max(rm)
        };
        let reach2 = (reach + 0.5) * (reach + 0.5);
        // Sampled-tip mapping.
        let (level, inv_scale, tw, th) = match mips {
            Some(m) => {
                let (w0, h0, _) = &m.levels[0];
                let big = *w0.max(h0) as f32;
                (m.level_for(2.0 * r * ro.max(0.25)), big / (2.0 * r), *w0 as f32, *h0 as f32)
            }
            None => (0, 1.0, 1.0, 1.0),
        };
        // Keep local Y up for bitmap sampling; C3 angles turn clockwise on the screen.
        let to_tip = |dx: f32, dy: f32| {
            let (mut ux, mut uy) = (dx, -dy);
            if let Some((ax, ay, k)) = proj {
                let t = (ux * ax + uy * ay) * k;
                ux += t * ax;
                uy += t * ay;
            }
            ((ux * cs + uy * sn) * fx, (-ux * sn + uy * cs) * fy)
        };
        // A tip a few pixels across takes each pixel's covered area (a 4×4 grid inside it):
        // sampling only the pixel centre made a tiny dab's total coverage swing with its sub-pixel
        // position, and thin lines beaded.
        let small = mips.is_none() && !aliased && if b.c3_tip_math { r * 2.0 < 5.0 } else { rm < SMALL_TIP_RADIUS };
        let small_reach2 = (reach + 0.75) * (reach + 0.75);
        // Keep ordinary computed tips separate from the feature loop, so their dense rows
        // can be vectorised without the per-pixel sampled-mask and stroke-depth branches.
        if !(b.c3_tip_math
            || mips.is_some()
            || b.triangle
            || small
            || (aliased && b.square)
            || noise
            || grain.is_some()
            || tex_tip
            || wet
            || single_wet.is_some())
            && depth.is_none()
            && proj.is_none()
            && d.angle == 0.0
            && d.radius.is_finite()
            && d.roundness.is_finite()
            && rm.is_finite()
            && cx.is_finite()
            && cy.is_finite()
            && hardness.is_finite()
            && d.alpha.is_finite()
            && w > 0
        {
            rasterize_round(d, rect, out, hardness, aliased);
            return;
        }
        for yy in 0..h {
            let y = rect.y0 + yy as i32;
            let dy = y as f32 + 0.5 - cy;
            for xx in 0..w {
                let x = rect.x0 + xx as i32;
                let dx = x as f32 + 0.5 - cx;
                let (u, v) = to_tip(dx, dy);
                let (mut val, rn) = match mips {
                    _ if b.triangle => {
                        let value = if small {
                            let mut sum = 0.0;
                            for sy in SUBPIXEL {
                                for sx in SUBPIXEL {
                                    let (su, sv) = to_tip(dx + sx, dy + sy);
                                    sum += triangle_mask(su, sv, r, ro, hardness, true);
                                }
                            }
                            sum / (SUBPIXEL.len() * SUBPIXEL.len()) as f32
                        } else {
                            triangle_mask(u, v, r, ro, hardness, false)
                        };
                        (if aliased { if value >= 0.5 { 1.0 } else { 0.0 } } else { value }, 1.0 - value)
                    }
                    // Block: the whole-pixel square, hard (the Eraser's Block mode).
                    _ if aliased && b.square => {
                        if dx.abs() >= r || dy.abs() >= r {
                            continue;
                        }
                        (1.0, 0.0)
                    }
                    None if small => {
                        let d2 = (u * ro).powi(2) + v * v;
                        if d2 >= small_reach2 {
                            continue;
                        }
                        let mut sum = 0.0;
                        for sy in SUBPIXEL {
                            for sx in SUBPIXEL {
                                let (su, sv) = to_tip(dx + sx, dy + sy);
                                let sd = ((su * ro).powi(2) + sv * sv).sqrt();
                                // Each sample takes the tip's own edge, so the covered area
                                // changes smoothly with the dab's position.
                                sum += if b.c3_tip_math { c3_tip_falloff(sd / ro, r, hardness, true) } else { tip_falloff(sd, rm, hardness) };
                            }
                        }
                        (sum / (SUBPIXEL.len() * SUBPIXEL.len()) as f32, d2.sqrt() / rm)
                    }
                    None => {
                        let d2 = (u * ro).powi(2) + v * v;
                        if d2 >= reach2 {
                            continue;
                        }
                        let dd = d2.sqrt();
                        let val = if aliased {
                            if dd <= rm { 1.0 } else { 0.0 }
                        } else if b.c3_tip_math {
                            c3_tip_falloff(dd / ro, r, hardness, false)
                        } else {
                            dab_coverage(dd, rm, hardness)
                        };
                        (val, dd / rm)
                    }
                    Some(m) => {
                        let (px, py) = (u * inv_scale + tw / 2.0, -(v / ro) * inv_scale + th / 2.0);
                        if px < -1.0 || py < -1.0 || px > tw + 1.0 || py > th + 1.0 {
                            continue;
                        }
                        let s = if b.c3_tip_math { c3_sampled_mask(m, px / tw, py / th, hardness) } else { m.sample_clamped(level, px / tw, py / th) };
                        let val = if aliased { if s >= 0.5 { 1.0 } else { 0.0 } } else { s };
                        (val, 1.0 - s)
                    }
                };
                if val <= 0.0 {
                    continue;
                }
                if noise && val < 1.0 {
                    let n = hash2(x, y, NOISE_SALT);
                    let hard = if n < val { 1.0 } else { 0.0 };
                    val += (hard - val) * 0.7;
                }
                if let Some(g) = grain {
                    val = g.alpha(val, d.grain_density, x, y);
                }
                let single_edge = single_wet.map(|ws| ws.dab_edge(val, rn, d.wet_edge));
                if let (Some(ws), Some(edge)) = (single_wet, single_edge) {
                    val = ws.alpha(val, edge, d.wet_edge);
                }
                if tex_tip {
                    val = b.texture.combine(val, self.texture_at_point(x as f32 + 0.5, y as f32 + 0.5, Some(d)), d.depth);
                }
                if wet {
                    val *= wet_factor(rn);
                }
                if let Some(slot) = depth.as_deref_mut().and_then(|dp| dp.get_mut(yy * w + xx)) {
                    *slot = single_edge.unwrap_or_else(|| (1.0 - rn).clamp(0.0, 1.0));
                }
                out[yy * w + xx] = val * d.alpha;
            }
        }
    }

    /// One dab's dense footprint (for the sequential retouch tools).
    pub fn footprint(&self, d: &Dab, index: usize) -> Footprint {
        let rect = self.dab_rect(d, false);
        let mut cov = Vec::new();
        self.rasterize(d, false, rect, &mut cov);
        Footprint { dab: *d, index, rect, cov }
    }

    /// Stroke-level masks applied to accumulated coverage `c` at a pixel.
    #[inline]
    pub fn stroke_mask(&self, c: f32, dual: Option<f32>, x: i32, y: i32) -> f32 {
        self.stroke_mask_wet(c, None, dual, x, y)
    }

    /// [`stroke_mask`](Self::stroke_mask) with Wet Edges from the stroke's depth at the pixel
    /// (the deepest any dab reaches it, see [`CoverageMap::depth`]).
    #[inline]
    pub fn stroke_mask_wet(&self, c: f32, depth: Option<f32>, dual: Option<f32>, x: i32, y: i32) -> f32 {
        let b = &self.brush;
        if let Some(ctx) = &self.secondary {
            let p = c * b.opacity * b.dual_brush.primary_strength;
            let mut secondary = dual.unwrap_or(0.0) * b.dual_brush.secondary_strength;
            if ctx.texture.is_some() && !ctx.brush.texture.each_tip {
                secondary = ctx.brush.texture.combine(secondary, ctx.texture_at(x, y), ctx.brush.texture.depth.clamp(0.0, 1.0));
            }
            let mut result = dual_blend(b.dual_brush.mode, p, secondary);
            if self.texture.is_some() && !b.texture.each_tip {
                result = b.texture.combine(result, self.texture_at(x, y), b.texture.depth.clamp(0.0, 1.0));
            }
            return result;
        }
        let c = c * b.dual_brush.primary_strength.clamp(0.0, 1.0);
        let mut m = match depth {
            Some(dp) if b.wet_edge.is_none() => c * wet_factor(1.0 - dp),
            Some(_) => c,
            None => c,
        };
        if let Some(dv) = dual {
            m = mask_combine(b.dual_brush.mode, m, dv, (b.dual_brush.opacity * b.dual_brush.secondary_strength).clamp(0.0, 1.0));
        }
        if self.texture.is_some() && !b.texture.each_tip {
            m = b.texture.combine(m, self.texture_at(x, y), b.texture.depth.clamp(0.0, 1.0));
        }
        m
    }
}

// ---------------------------------------------------------------------------
// Sparse stroke buffer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct CovTile {
    cov: Vec<f32>,
    /// Wet Edges: the deepest any dab reaches each pixel (`1 − rn` of the nearest dab), empty
    /// without Wet Edges. The stroke's interior is deep, its rim shallow, whatever the dabs' overlap.
    depth: Vec<f32>,
    /// Premultiplied native colour channels (`nc` per pixel), empty without per-dab colour.
    col: Vec<f32>,
    /// Per-pixel resolved wet parameters and single-dab edge weight.
    material: Vec<[f32; 4]>,
    pigment: Vec<[f32; 4]>,
    pigment_settings: Vec<crate::pigment_material::ResolvedPigment>,
    pigment_motion: Vec<[f32; 2]>,
    saturation: Vec<[f32; 2]>,
}

/// A sparse tiled coverage (+ optional colour) buffer.
#[derive(Clone, Debug, Default)]
pub struct CoverageMap {
    tiles: HashMap<(i32, i32), CovTile>,
    nc: usize,
    bounds: Rect,
    dirty: HashSet<(i32, i32)>,
}

impl CoverageMap {
    pub fn new(color_channels: usize) -> Self {
        Self { nc: color_channels, ..Default::default() }
    }
    pub fn bounds(&self) -> Rect {
        self.bounds
    }
    #[inline]
    pub fn get(&self, x: i32, y: i32) -> f32 {
        let (tx, ty) = (x.div_euclid(COV_TILE), y.div_euclid(COV_TILE));
        self.tiles.get(&(tx, ty)).map_or(0.0, |t| t.cov[((y - ty * COV_TILE) * COV_TILE + (x - tx * COV_TILE)) as usize])
    }

    /// Wet Edges depth at a pixel (see [`CovTile::depth`]); `None` when the stroke has none.
    #[inline]
    pub fn depth(&self, x: i32, y: i32) -> Option<f32> {
        let (tx, ty) = (x.div_euclid(COV_TILE), y.div_euclid(COV_TILE));
        self.tiles.get(&(tx, ty)).and_then(|t| t.depth.get(((y - ty * COV_TILE) * COV_TILE + (x - tx * COV_TILE)) as usize).copied())
    }

    fn material_at(&self, x: i32, y: i32) -> (ResolvedWet, f32) {
        let (tx, ty) = (x.div_euclid(COV_TILE), y.div_euclid(COV_TILE));
        let i = ((y - ty * COV_TILE) * COV_TILE + (x - tx * COV_TILE)) as usize;
        let m = self.tiles.get(&(tx, ty)).and_then(|t| t.material.get(i)).copied().unwrap_or([0.0; 4]);
        (ResolvedWet { strength: m[0], width: m[1], stamp_width: m[2] }, m[3])
    }

    fn pigment_at(&self, x: i32, y: i32) -> [f32; 4] {
        let (tx, ty) = (x.div_euclid(COV_TILE), y.div_euclid(COV_TILE));
        let i = ((y - ty * COV_TILE) * COV_TILE + x - tx * COV_TILE) as usize;
        self.tiles.get(&(tx, ty)).and_then(|t| t.pigment.get(i)).copied().unwrap_or([0.0; 4])
    }
    #[cfg(test)]
    fn pigment_sample(&self, x: f32, y: f32) -> [f32; 4] {
        let (ix, iy) = (x.floor() as i32, y.floor() as i32);
        let (fx, fy) = (x - x.floor(), y - y.floor());
        let (a, b, c, d) = (self.pigment_at(ix, iy), self.pigment_at(ix + 1, iy), self.pigment_at(ix, iy + 1), self.pigment_at(ix + 1, iy + 1));
        std::array::from_fn(|i| (a[i] + (b[i] - a[i]) * fx) * (1.0 - fy) + (c[i] + (d[i] - c[i]) * fx) * fy)
    }
    fn accumulate_pigment(&mut self, ctx: &BrushContext, d: &Dab, rect: Rect, vals: &[f32], motion: [f32; 2]) {
        if !d.pigment.enabled || ctx.brush.erase || d.alpha <= 0.0 || rect.is_empty() {
            return;
        }
        let width = rect.width() as usize;
        let mask = |ix: i32, iy: i32| {
            if ix < rect.x0 || ix >= rect.x1 || iy < rect.y0 || iy >= rect.y1 {
                return 0.0;
            }
            vals.get((iy - rect.y0) as usize * width + (ix - rect.x0) as usize).copied().unwrap_or(0.0) / d.alpha.max(1e-6)
        };
        // Emboss samples the hard tip at fractional positions, before grain, wet edges or flow.
        let hard_tip = |x: f32, y: f32| {
            let dx = x + 0.5 - d.center.x as f32;
            let dy = -(y + 0.5 - d.center.y as f32);
            let (sn, cs) = (if ctx.brush.c3_tip_math { -d.angle } else { d.angle }).sin_cos();
            let (u, v) = (dx * cs + dy * sn, -dx * sn + dy * cs);
            let roundness = d.roundness.max(0.01);
            if ctx.brush.triangle {
                return triangle_mask(u, v, d.radius.max(0.1), roundness, 1.0, false);
            }
            if let Some(m) = ctx.tip.as_ref() {
                let (w, h, _) = &m.levels[0];
                let big = *w.max(h) as f32;
                let u = u * if d.flip_x { -1.0 } else { 1.0 };
                let v = v * if d.flip_y { -1.0 } else { 1.0 };
                m.sample_clamped(0, u * big / (d.radius * 2.0).max(0.2) / *w as f32 + 0.5, -v / roundness * big / (d.radius * 2.0).max(0.2) / *h as f32 + 0.5)
            } else {
                dab_coverage(u.hypot(v / roundness), d.radius.max(0.1), 1.0)
            }
        };
        let step = (d.radius * 0.075).clamp(0.65, 3.0);
        let dirs = [
            [1.0, 0.0],
            [-1.0, 0.0],
            [0.0, 1.0],
            [0.0, -1.0],
            [std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2],
            [-std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2],
            [std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2],
            [-std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2],
        ];
        for y in rect.y0..rect.y1 {
            for x in rect.x0..rect.x1 {
                let (tx, ty) = (x.div_euclid(COV_TILE), y.div_euclid(COV_TILE));
                let Some(tile) = self.tiles.get_mut(&(tx, ty)) else { continue };
                let i = ((y - ty * COV_TILE) * COV_TILE + x - tx * COV_TILE) as usize;
                let coverage = mask(x, y);
                if coverage <= 0.0 {
                    continue;
                }
                let near = dirs.map(|o| hard_tip(x as f32 + o[0] * step, y as f32 + o[1] * step));
                let far = dirs.map(|o| hard_tip(x as f32 + o[0] * step * 2.15, y as f32 + o[1] * step * 2.15));
                let (sn, cs) = (if ctx.brush.c3_tip_math { -d.angle } else { d.angle }).sin_cos();
                let dx = x as f32 + 0.5 - d.center.x as f32;
                let dy = -(y as f32 + 0.5 - d.center.y as f32);
                let aspect = ctx.tip.as_ref().map_or([1.0, 1.0], |m| {
                    let (w, h, _) = &m.levels[0];
                    let big = *w.max(h) as f32;
                    [*w as f32 / big, *h as f32 / big]
                });
                let local = [
                    (dx * cs + dy * sn) / (d.radius * aspect[0]).max(0.1),
                    (if ctx.brush.c3_tip_math { dx * sn - dy * cs } else { -dx * sn + dy * cs }) / (d.radius * d.roundness * aspect[1]).max(0.1),
                ];
                let pattern = if ctx.texture.is_some() && ctx.brush.texture.each_tip {
                    ctx.texture_at_point(x as f32 + 0.5, y as f32 + 0.5, Some(d))
                } else if ctx.tip.is_some() {
                    hard_tip(x as f32, y as f32)
                } else {
                    1.0
                };
                let emboss = crate::pigment_material::emboss_height(coverage, local, pattern, near, far);
                let deposit = crate::pigment_material::deposit(d.pigment, coverage, d.alpha, d.opacity * ctx.brush.opacity, emboss);
                if tile.pigment.is_empty() {
                    let len = (COV_TILE * COV_TILE) as usize;
                    tile.pigment = vec![[0.0; 4]; len];
                    tile.pigment_settings = vec![Default::default(); len];
                    tile.pigment_motion = vec![[0.0; 2]; len];
                }
                tile.pigment[i] = crate::pigment_material::over_material(tile.pigment[i], deposit);
                tile.pigment_settings[i] = d.pigment;
                tile.pigment_motion[i] = motion;
            }
        }
    }

    /// Accumulate dab values over `rect`: flow builds up towards the dab's opacity ceiling
    /// (`c ← c + v·(ceil − c)`). `color` = native colour for per-dab colour. `depth` (Wet Edges, one
    /// value per pixel of `rect`) keeps the deepest any dab reaches each pixel.
    pub fn accumulate(&mut self, rect: Rect, vals: &[f32], ceil: f32, color: Option<&[f32]>, depth: Option<&[f32]>) {
        self.accumulate_wet(rect, vals, ceil, color.map(|c| (c, false)), depth, None, None);
    }

    #[allow(clippy::too_many_arguments)]
    fn accumulate_wet(
        &mut self,
        rect: Rect,
        vals: &[f32],
        ceil: f32,
        color: Option<(&[f32], bool)>,
        depth: Option<&[f32]>,
        wet: Option<(ResolvedWet, Option<f32>)>,
        saturation: Option<(f32, f32, f32, PixelFormat)>,
    ) {
        if rect.is_empty() {
            return;
        }
        self.bounds = self.bounds.union(&rect);
        let w = rect.width() as usize;
        let nc = self.nc;
        let (t0x, t0y) = (rect.x0.div_euclid(COV_TILE), rect.y0.div_euclid(COV_TILE));
        let (t1x, t1y) = ((rect.x1 - 1).div_euclid(COV_TILE), (rect.y1 - 1).div_euclid(COV_TILE));
        let want_col = color.is_some() && nc > 0;
        for ty in t0y..=t1y {
            for tx in t0x..=t1x {
                let tr = Rect::new(tx * COV_TILE, ty * COV_TILE, (tx + 1) * COV_TILE, (ty + 1) * COV_TILE).intersect(&rect);
                if tr.is_empty() {
                    continue;
                }
                // Skip tiles this dab doesn't touch.
                let any = (tr.y0..tr.y1).any(|y| {
                    let row = (y - rect.y0) as usize * w;
                    vals[row + (tr.x0 - rect.x0) as usize..row + (tr.x1 - rect.x0) as usize].iter().any(|&v| v > 0.0)
                });
                if !any {
                    continue;
                }
                let tile = self.tiles.entry((tx, ty)).or_insert_with(|| CovTile {
                    cov: vec![0.0; (COV_TILE * COV_TILE) as usize],
                    depth: Vec::new(),
                    material: Vec::new(),
                    pigment: Vec::new(),
                    pigment_settings: Vec::new(),
                    pigment_motion: Vec::new(),
                    saturation: Vec::new(),
                    col: if want_col { vec![0.0; (COV_TILE * COV_TILE) as usize * nc] } else { Vec::new() },
                });
                if want_col && tile.col.is_empty() {
                    tile.col = vec![0.0; (COV_TILE * COV_TILE) as usize * nc];
                }
                if depth.is_some() && tile.depth.is_empty() {
                    tile.depth = vec![0.0; (COV_TILE * COV_TILE) as usize];
                }
                if wet.is_some() && tile.material.is_empty() {
                    tile.material = vec![[0.0; 4]; (COV_TILE * COV_TILE) as usize];
                }
                if saturation.is_some() && tile.saturation.is_empty() {
                    tile.saturation = vec![[0.0; 2]; (COV_TILE * COV_TILE) as usize];
                }
                self.dirty.insert((tx, ty));
                for y in tr.y0..tr.y1 {
                    let row = (y - rect.y0) as usize * w;
                    let trow = ((y - ty * COV_TILE) * COV_TILE) as usize;
                    for x in tr.x0..tr.x1 {
                        let v = vals[row + (x - rect.x0) as usize];
                        if v <= 0.0 {
                            continue;
                        }
                        let ti = trow + (x - tx * COV_TILE) as usize;
                        if let (Some(dv), Some(slot)) = (depth.and_then(|dp| dp.get(row + (x - rect.x0) as usize)), tile.depth.get_mut(ti)) {
                            *slot = slot.max(*dv);
                        }
                        let c = tile.cov[ti];
                        let nv = if ceil > c { c + v * (ceil - c) } else { c };
                        if let (Some((wet, single)), Some(slot)) = (wet, tile.material.get_mut(ti)) {
                            let edge =
                                single.map_or(0.0, |strength| strength * depth.and_then(|v| v.get(row + (x - rect.x0) as usize)).copied().unwrap_or(0.0));
                            let incoming = [wet.strength, wet.width, wet.stamp_width, edge];
                            let weight = if nv > 0.0 { (nv - c) / nv } else { 0.0 };
                            for (dst, src) in slot.iter_mut().zip(incoming) {
                                *dst += (src - *dst) * weight;
                            }
                        }
                        if let (true, Some((colors, per_pixel))) = (want_col, color) {
                            let offset = if per_pixel { (row + (x - rect.x0) as usize) * nc } else { 0 };
                            let col = &colors[offset..offset + nc];
                            let incoming = nv.min(ceil * v);
                            let retained = (nv - incoming).max(0.0);
                            let p = &mut tile.col[ti * nc..(ti + 1) * nc];
                            for i in 0..nc {
                                p[i] = col[i] * incoming + if c > 0.0001 { p[i] / c * retained } else { 0.0 };
                            }
                            if let Some((record, preserve, opacity, fmt)) = saturation {
                                let mut native = [0.0; 8];
                                native[..nc].copy_from_slice(p);
                                if fmt.alpha {
                                    native[nc] = 1.0;
                                }
                                let mut rgba = to_rgba(&fmt, &native[..fmt.channels()]);
                                for channel in &mut rgba[..3] {
                                    *channel *= opacity;
                                }
                                let memory = &mut tile.saturation[ti];
                                let previous = if memory[1] > 0.0001 { memory[0] / memory[1] } else { 0.0 };
                                let rgb = crate::attributes::preserve_saturation([rgba[0], rgba[1], rgba[2]], previous, preserve);
                                rgba[..3].copy_from_slice(&rgb.map(|v| v / opacity.max(0.000001)));
                                from_rgba_into(&fmt, rgba, &mut native);
                                p.copy_from_slice(&native[..nc]);
                                native[..nc].copy_from_slice(col);
                                if fmt.alpha {
                                    native[nc] = 1.0;
                                }
                                let source = to_rgba(&fmt, &native[..fmt.channels()]);
                                let sat = crate::attributes::saturation([source[0], source[1], source[2]]);
                                let weight = (incoming * opacity * record).clamp(0.0, 1.0);
                                *memory = [memory[0] * (1.0 - weight) + sat * weight, memory[1] * (1.0 - weight) + weight];
                            }
                        }
                        tile.cov[ti] = nv;
                    }
                }
            }
        }
    }

    fn take_dirty(&mut self) -> Vec<(i32, i32)> {
        let mut v: Vec<_> = self.dirty.drain().collect();
        v.sort_unstable();
        v
    }

    /// The finished coverage over the tiles `keys`: `tail`'s copy of a tile where finishing the
    /// stroke changes it (see [`StrokeRenderer::tail_preview`]), else this map's own.
    fn finished_subset(&self, tail: Option<&Self>, keys: &HashSet<(i32, i32)>) -> Self {
        let tile = |k: &(i32, i32)| tail.and_then(|t| t.tiles.get(k)).or_else(|| self.tiles.get(k)).map(|t| (*k, t.clone()));
        Self { tiles: keys.iter().filter_map(tile).collect(), nc: self.nc, bounds: tail.map_or(self.bounds, |t| t.bounds), dirty: HashSet::new() }
    }

    /// Union two passes of the same brush stroke without applying opacity twice where they
    /// overlap. The stronger coverage wins, including its per-dab colour when present.
    fn union_max(&mut self, other: &Self) {
        self.bounds = self.bounds.union(&other.bounds);
        for (&key, source) in &other.tiles {
            let Some(target) = self.tiles.get_mut(&key) else {
                self.tiles.insert(key, source.clone());
                self.dirty.insert(key);
                continue;
            };
            if target.depth.is_empty() {
                target.depth.clone_from(&source.depth);
            } else {
                for (dst, src) in target.depth.iter_mut().zip(&source.depth) {
                    *dst = dst.max(*src);
                }
            }
            for (index, (dst, src)) in target.cov.iter_mut().zip(&source.cov).enumerate() {
                if *src > *dst {
                    *dst = *src;
                    if let Some(material) = source.material.get(index) {
                        if target.material.is_empty() {
                            target.material = vec![[0.0; 4]; source.cov.len()];
                        }
                        if let Some(slot) = target.material.get_mut(index) {
                            *slot = *material;
                        }
                    }
                    if let Some(value) = source.pigment.get(index) {
                        if target.pigment.is_empty() {
                            target.pigment = vec![[0.0; 4]; source.cov.len()];
                            target.pigment_settings = vec![Default::default(); source.cov.len()];
                            target.pigment_motion = vec![[0.0; 2]; source.cov.len()];
                        }
                        target.pigment[index] = *value;
                        target.pigment_settings[index] = source.pigment_settings[index];
                        target.pigment_motion[index] = source.pigment_motion[index];
                    }
                    let start = index.saturating_mul(self.nc);
                    let end = start.saturating_add(self.nc);
                    if let (Some(dst_color), Some(src_color)) = (target.col.get_mut(start..end), source.col.get(start..end)) {
                        dst_color.copy_from_slice(src_color);
                    }
                }
            }
            self.dirty.insert(key);
        }
    }
}

// ---------------------------------------------------------------------------
// Stroke renderer
// ---------------------------------------------------------------------------

/// What [`StrokeRenderer::composite_union_live`] drew last: the coverage tiles of the passes'
/// smoothing tails (redrawn without them next time) and the merged bounds.
#[derive(Clone, Debug, Default)]
pub struct UnionLive {
    tail_keys: HashSet<(i32, i32)>,
    bounds: Rect,
}

impl UnionLive {
    /// The merged bounds of the finished passes, as last drawn.
    pub fn bounds(&self) -> Rect {
        self.bounds
    }
}

/// Each colour-transport lane owns its carrier and sparse working pixels.
#[derive(Clone, Debug)]
struct LaneTransport {
    work: Surface,
    state: crate::transport::Transport,
}
impl LaneTransport {
    fn new(brush: &BrushSettings, pre: &Surface) -> Option<Self> {
        let m = &brush.mix_settings;
        (!brush.erase && m.enabled).then(|| Self { work: pre.clone(), state: crate::transport::Transport::new(brush, alpha_index(&pre.format())) })
    }
    fn dab(&mut self, ctx: &BrushContext, d: &Dab, rect: Rect, mask: &[f32], color: Option<(&[f32], bool)>) {
        let halo = ((d.radius * (0.18 + d.blur_strength * 0.82)).clamp(1.5, 127.0) + d.radius * 2.0 * ctx.brush.spacing).ceil() as i32;
        let mut region = crate::retouch::Region::read(&self.work, rect.inflate(halo));
        let footprint = Footprint { dab: *d, index: 0, rect, cov: mask.to_vec() };
        self.state.smudge_dab(&mut region, &footprint);
        let fmt = self.work.format();
        let nc = fmt.mode.color_channels();
        let n = fmt.channels();
        if ctx.brush.mix_settings.color_output {
            let mut native = [0.0; 8];
            from_rgba_into(&fmt, d.color, &mut native);
            let width = rect.width() as usize;
            for y in rect.y0..rect.y1 {
                for x in rect.x0..rect.x1 {
                    let i = (y - rect.y0) as usize * width + (x - rect.x0) as usize;
                    let k = mask[i] * d.opacity;
                    if k <= 0.0 {
                        continue;
                    }
                    if let Some((colors, per_pixel)) = color {
                        let offset = if per_pixel { i * nc } else { 0 };
                        if let Some(c) = colors.get(offset..offset + nc) {
                            native[..nc].copy_from_slice(c);
                        }
                    }
                    over_native(&fmt, region.px_mut(x, y), &native[..n], k, false);
                }
            }
        }
        self.state.blur_dab(&mut region, &footprint);
        region.crop(rect).write(&mut self.work);
    }
}

/// Incremental stroke rendering: feed points, composite dirty tiles from the pre-stroke pixels.
#[derive(Clone, Debug)]
pub struct StrokeRenderer {
    pub ctx: BrushContext,
    generator: DabGenerator,
    cov: CoverageMap,
    dual: Option<CoverageMap>,
    fmt: Option<PixelFormat>,
    per_dab_color: bool,
    material_base: Option<Arc<Surface>>,
    material_candidates: HashMap<(i32, i32), Arc<[[f32; 4]]>>,
    previous_centers: [Option<Point>; 2],
    transports: [Option<LaneTransport>; 2],
    dabs_done: usize,
    scratch: Vec<f32>,
    /// Wet Edges depth of the dab being accumulated (see [`CovTile::depth`]).
    depth_scratch: Vec<f32>,
    dab_buf: Vec<Dab>,
    dual_buf: Vec<Dab>,
    all_dabs: Option<Vec<Dab>>,
}

impl StrokeRenderer {
    /// Composite the union of this stroke and a mirrored pass as one stroke. This keeps
    /// overlapping dabs on a symmetry axis under one opacity ceiling at every bit depth.
    pub fn composite_union(&self, other: &Self, pre: &Surface, target: &mut Surface, selection: Option<&Surface>, lock_transparency: bool) -> Rect {
        self.composite_union_many(std::iter::once(other), pre, target, selection, lock_transparency)
    }

    fn union_transport_work(to: &mut Option<LaneTransport>, from: &Option<LaneTransport>, coverage: &CoverageMap, incoming: &CoverageMap) {
        let Some(source) = from else {
            return;
        };
        let Some(target) = to else {
            *to = Some(source.clone());
            return;
        };
        let channels = target.work.format().channels();
        for (&(tx, ty), tile) in &incoming.tiles {
            let rect = Rect::new(tx * COV_TILE, ty * COV_TILE, (tx + 1) * COV_TILE, (ty + 1) * COV_TILE);
            let previous = coverage.tiles.get(&(tx, ty));
            let owns = |i: usize| tile.cov[i] > previous.map_or(0.0, |t| t.cov[i]);
            if !(0..tile.cov.len()).any(owns) {
                continue;
            }
            let mut pixels = target.work.read_region(rect);
            let values = source.work.read_region(rect);
            for i in 0..tile.cov.len() {
                if owns(i) {
                    let start = i * channels;
                    pixels[start..start + channels].copy_from_slice(&values[start..start + channels]);
                }
            }
            target.work.write_region(rect, &pixels);
        }
    }

    /// Combine all symmetry passes before applying brush opacity and selection once.
    pub fn composite_union_many<'a>(
        &self,
        others: impl IntoIterator<Item = &'a Self>,
        pre: &Surface,
        target: &mut Surface,
        selection: Option<&Surface>,
        lock_transparency: bool,
    ) -> Rect {
        let mut merged = self.clone();
        for other in others {
            Self::union_transport_work(&mut merged.transports[0], &other.transports[0], &merged.cov, &other.cov);
            if let (Some(to), Some(from)) = (&merged.dual, &other.dual) {
                Self::union_transport_work(&mut merged.transports[1], &other.transports[1], to, from);
            }
            merged.cov.union_max(&other.cov);
            if let (Some(to), Some(from)) = (&mut merged.dual, &other.dual) {
                to.union_max(from);
            }
        }
        merged.composite(pre, target, selection, lock_transparency, true)
    }

    /// Coverage tiles (stroke and Dual Brush) changed since the last call or composite, into
    /// `keys`; forgets them.
    fn drain_dirty_into(&mut self, keys: &mut HashSet<(i32, i32)>) {
        keys.extend(self.cov.dirty.drain());
        if let Some(d) = self.dual.as_mut() {
            keys.extend(d.dirty.drain());
        }
    }

    /// Every coverage tile (stroke and Dual Brush), into `keys`.
    fn keys_into(&self, keys: &mut HashSet<(i32, i32)>) {
        keys.extend(self.cov.tiles.keys());
        if let Some(d) = &self.dual {
            keys.extend(d.tiles.keys());
        }
    }

    /// Live preview of a stroke painted as several passes (the stroke, then its symmetry
    /// mirrors): leaves `target` as finishing every pass now and compositing them with
    /// [`composite_union_many`](Self::composite_union_many) over `pre` would, but redraws only the
    /// coverage tiles that changed since the previous call (new dabs, and the smoothing tail now
    /// and before). `full` redraws every tile: on the first call, or when a pass joins. Returns
    /// the damaged rectangle.
    pub fn composite_union_live(
        passes: &mut [&mut Self],
        live: &mut UnionLive,
        full: bool,
        pre: &Surface,
        target: &mut Surface,
        selection: Option<&Surface>,
        lock_transparency: bool,
    ) -> Rect {
        let mut keys = std::mem::take(&mut live.tail_keys);
        for p in passes.iter_mut() {
            p.drain_dirty_into(&mut keys);
            if full {
                p.keys_into(&mut keys);
            }
        }
        let spatial = passes.iter().any(|p| p.spatial_wet());
        let tails: Vec<_> = passes.iter().map(|p| p.tail_preview()).collect();
        let mut tail_keys = HashSet::new();
        for t in tails.iter().flatten() {
            t.keys_into(&mut tail_keys);
        }
        keys.extend(&tail_keys);
        if spatial {
            Self::expand_keys(&mut keys);
        }
        let output_keys = keys.clone();
        if spatial {
            Self::expand_keys(&mut keys);
        }
        let mut merged: Option<Self> = None;
        for (p, t) in passes.iter().zip(&tails) {
            let cov = p.cov.finished_subset(t.as_ref().map(|t| &t.cov), &keys);
            let dual = p.dual.as_ref().map(|d| d.finished_subset(t.as_ref().and_then(|t| t.dual.as_ref()), &keys));
            match merged.as_mut() {
                None => {
                    merged = Some(Self {
                        ctx: p.ctx.clone(),
                        generator: p.generator.clone(),
                        cov,
                        dual,
                        fmt: p.fmt,
                        per_dab_color: p.per_dab_color,
                        material_base: p.material_base.clone(),
                        material_candidates: p.material_candidates.clone(),
                        previous_centers: p.previous_centers,
                        transports: t.as_ref().unwrap_or(p).transports.clone(),
                        dabs_done: p.dabs_done,
                        scratch: Vec::new(),
                        depth_scratch: Vec::new(),
                        dab_buf: Vec::new(),
                        dual_buf: Vec::new(),
                        all_dabs: None,
                    })
                }
                Some(m) => {
                    let source = t.as_ref().unwrap_or(p);
                    Self::union_transport_work(&mut m.transports[0], &source.transports[0], &m.cov, &cov);
                    if let (Some(to), Some(from)) = (&m.dual, &dual) {
                        Self::union_transport_work(&mut m.transports[1], &source.transports[1], to, from);
                    }
                    m.cov.union_max(&cov);
                    if let (Some(to), Some(from)) = (&mut m.dual, &dual) {
                        to.union_max(from);
                    }
                }
            }
        }
        let Some(mut merged) = merged else { return Rect::EMPTY };
        let (before, after) = (live.bounds, merged.bounds());
        // Back to the pre-stroke pixels where the previous call drew and nothing covers now.
        let mut keys: Vec<_> = output_keys.iter().copied().collect();
        keys.sort_unstable();
        let mut dmg = Rect::EMPTY;
        let mut region = Vec::new();
        for &(tx, ty) in &keys {
            let tile = Rect::new(tx * COV_TILE, ty * COV_TILE, (tx + 1) * COV_TILE, (ty + 1) * COV_TILE);
            let old = tile.intersect(&before);
            if !old.is_empty() {
                pre.read_region_into(old, &mut region);
                target.write_region(old, &region);
                dmg = dmg.union(&old);
            }
            if merged.cov.tiles.contains_key(&(tx, ty)) {
                dmg = dmg.union(&tile.intersect(&after));
            }
        }
        merged.cov.dirty = output_keys;
        if let Some(d) = merged.dual.as_mut() {
            d.dirty.clear();
        }
        merged.composite_tiles(pre, target, selection, lock_transparency, false, false);
        live.tail_keys = tail_keys;
        live.bounds = after;
        dmg
    }
    /// `fmt` = target pixel format (needed for per-dab colour); `zoom` for smoothing.
    pub fn new(brush: &BrushSettings, fmt: Option<PixelFormat>, zoom: f32) -> Self {
        let single = brush.wet_edges
            && brush.wet_edge.as_ref().is_some_and(|w| w.outline == WetOutline::Single && w.color_mode != crate::material::WetColorMode::Normal);
        let per_dab_color = (brush.saturation_record_strength > 0.0
            || brush.saturation_preserve_output > 0.0
            || brush.color_dynamics.enabled
            || brush.pattern_enabled
            || brush.canvas_material.is_some()
            || brush.pigment.as_ref().is_some_and(|p| p.enabled)
            || single)
            && !brush.erase
            && fmt.is_some();
        let nc = if per_dab_color { fmt.map_or(0, |f| f.mode.color_channels()) } else { 0 };
        Self {
            ctx: BrushContext::new(brush),
            generator: DabGenerator::new(brush, zoom),
            cov: CoverageMap::new(nc),
            dual: brush
                .dual_brush
                .enabled
                .then(|| CoverageMap::new(if brush.dual_brush.full_parameters.is_some() { fmt.map_or(0, |f| f.mode.color_channels()) } else { 0 })),
            fmt,
            per_dab_color,
            material_base: None,
            material_candidates: HashMap::new(),
            previous_centers: [None; 2],
            transports: [None, None],
            dabs_done: 0,
            scratch: Vec::new(),
            depth_scratch: Vec::new(),
            dab_buf: Vec::new(),
            dual_buf: Vec::new(),
            all_dabs: None,
        }
    }

    /// Prepare colour filtering against the immutable pre-stroke pixels.
    pub fn on_surface(brush: &BrushSettings, pre: &Surface, zoom: f32) -> Self {
        let mut renderer = Self::new(brush, Some(pre.format()), zoom);
        renderer.transports[0] = LaneTransport::new(&renderer.ctx.brush, pre);
        renderer.transports[1] = renderer.ctx.secondary.as_ref().and_then(|c| LaneTransport::new(&c.brush, pre));
        let needs_base = |b: &BrushSettings| {
            b.wet_edges && b.wet_edge.as_ref().is_some_and(|w| w.outline == WetOutline::Single && w.color_mode != crate::material::WetColorMode::Normal)
        };
        if needs_base(brush) || renderer.ctx.secondary.as_ref().is_some_and(|c| needs_base(&c.brush)) {
            renderer.material_base = Some(Arc::new(pre.clone()));
        }
        renderer
    }

    /// Keep a copy of every primary dab (for tests and sequential tools).
    pub fn record_dabs(mut self) -> Self {
        self.all_dabs = Some(Vec::new());
        self
    }
    pub fn dabs(&self) -> &[Dab] {
        self.all_dabs.as_deref().unwrap_or(&[])
    }
    pub fn dab_count(&self) -> usize {
        self.dabs_done
    }
    /// Union of the dab rectangles so far.
    pub fn bounds(&self) -> Rect {
        self.dual.as_ref().filter(|_| self.ctx.secondary.is_some()).map_or(self.cov.bounds, |d| self.cov.bounds.union(&d.bounds))
    }

    fn raster_pending(&mut self) {
        let mut dabs = std::mem::take(&mut self.dab_buf);
        let mut duals = std::mem::take(&mut self.dual_buf);
        let wet = self.ctx.brush.wet_edges && !self.ctx.brush.erase;
        let mut native = [0.0f32; 8];
        let mut colors = Vec::new();
        let mut base_pixels = Vec::new();
        let mut canvas_colors = Vec::new();
        for d in &dabs {
            self.ctx.texture_origin.get_or_insert(d.center);
            self.ctx.texture_tip = Some(*d);
            let rect = self.ctx.dab_rect(d, false);
            // Wet Edges acts on the whole stroke (#2088): the dab records how deep it reaches each
            // pixel and the stroke darkens its own rim when composited, not every dab's.
            self.ctx.rasterize_into(d, false, rect, &mut self.scratch, wet.then_some(&mut self.depth_scratch));
            let single =
                self.ctx.brush.wet_edge.as_ref().filter(|w| wet && w.outline == WetOutline::Single && w.color_mode != crate::material::WetColorMode::Normal);
            let pattern = self.ctx.brush.pattern_enabled.then_some(self.ctx.brush.sampled_pattern.as_ref()).flatten();
            let col = if let (Some(pattern), Some(f)) = (pattern, self.fmt) {
                colors.clear();
                if single.is_some()
                    && let Some(base) = &self.material_base
                {
                    base.read_region_into(rect, &mut base_pixels);
                }
                let width = rect.width() as usize;
                let scale = pattern.width.max(pattern.height) as f32 / (2.0 * d.radius).max(1.0);
                for (i, coverage) in self.scratch.iter_mut().enumerate() {
                    let mut rgba = pattern.sample_pigment(
                        scale * (rect.x0 as f32 + (i % width) as f32 + 0.5 - d.center.x as f32),
                        scale * (rect.y0 as f32 + (i / width) as f32 + 0.5 - d.center.y as f32),
                    );
                    *coverage *= rgba[3];
                    if let Some(w) = single {
                        let channels = f.channels();
                        let base = base_pixels.get(i * channels..(i + 1) * channels).map_or([0.0; 4], |p| to_rgba(&f, p));
                        rgba = w.color(rgba, base, self.depth_scratch[i], d.wet_edge);
                    }
                    from_rgba_into(&f, rgba, &mut native);
                    colors.extend_from_slice(&native[..f.mode.color_channels()]);
                }
                Some((colors.as_slice(), true))
            } else {
                match (self.per_dab_color, self.fmt, single) {
                    (true, Some(f), Some(w)) => {
                        let nc = f.mode.color_channels();
                        let n = f.channels();
                        colors.clear();
                        if let Some(base) = &self.material_base {
                            base.read_region_into(rect, &mut base_pixels);
                        } else {
                            base_pixels.clear();
                        }
                        for (i, edge) in self.depth_scratch.iter().enumerate() {
                            let base = base_pixels.get(i * n..(i + 1) * n).map_or([0.0; 4], |p| to_rgba(&f, p));
                            let rgba = w.color(d.color, base, *edge, d.wet_edge);
                            from_rgba_into(&f, rgba, &mut native);
                            colors.extend_from_slice(&native[..nc]);
                        }
                        Some((colors.as_slice(), true))
                    }
                    (true, Some(f), None) => {
                        from_rgba_into(&f, d.color, &mut native);
                        Some((&native[..f.mode.color_channels()], false))
                    }
                    _ => None,
                }
            };
            let col = if let (Some(material), Some(f)) = (&self.ctx.brush.canvas_material, self.fmt) {
                canvas_colors.clear();
                let nc = f.mode.color_channels();
                let width = rect.width() as usize;
                for i in 0..self.scratch.len() {
                    let mut rgba = d.color;
                    if let Some((values, per_pixel)) = col {
                        let off = if per_pixel { i * nc } else { 0 };
                        let mut value = [0.0; 8];
                        value[..nc].copy_from_slice(&values[off..off + nc]);
                        if let Some(ai) = alpha_index(&f) {
                            value[ai] = d.color[3];
                        }
                        rgba = to_rgba(&f, &value[..f.channels()]);
                    }
                    let rgb = material.pigment_response(
                        [rgba[0], rgba[1], rgba[2]],
                        [rect.x0 as f32 + (i % width) as f32 + 0.5, rect.y0 as f32 + (i / width) as f32 + 0.5],
                        self.ctx.brush.opacity,
                        d.alpha,
                    );
                    rgba[..3].copy_from_slice(&rgb);
                    let mut encoded = [0.0; 8];
                    from_rgba_into(&f, rgba, &mut encoded);
                    canvas_colors.extend_from_slice(&encoded[..nc]);
                }
                Some((canvas_colors.as_slice(), true))
            } else {
                col
            };
            let material = if wet {
                self.ctx.brush.wet_edge.as_ref().map(|w| (d.wet_edge, (w.outline == WetOutline::Single).then(|| w.color_strength(d.wet_edge.strength))))
            } else {
                None
            };
            if let Some(transport) = self.transports[0].as_mut() {
                transport.dab(&self.ctx, d, rect, &self.scratch, col);
            }
            self.cov.accumulate_wet(
                rect,
                &self.scratch,
                d.opacity,
                col,
                wet.then_some(&self.depth_scratch[..]),
                material,
                self.fmt
                    .filter(|_| self.ctx.brush.saturation_record_strength > 0.0 || self.ctx.brush.saturation_preserve_output > 0.0)
                    .map(|f| (self.ctx.brush.saturation_record_strength, self.ctx.brush.saturation_preserve_output, self.ctx.brush.opacity, f)),
            );
            let motion = dab_motion(&mut self.previous_centers[0], d);
            self.cov.accumulate_pigment(&self.ctx, d, rect, &self.scratch, motion);
            // Bounds track the full dab rectangle (even fully transparent parts), like the damage.
            self.cov.bounds = self.cov.bounds.union(&rect);
        }
        if let Some(dm) = self.dual.as_mut() {
            for d in &duals {
                if let Some(ctx) = self.ctx.secondary.as_mut() {
                    ctx.texture_origin.get_or_insert(d.center);
                    ctx.texture_tip = Some(*d);
                }
                let rect = self.ctx.dab_rect(d, true);
                let secondary = self.ctx.secondary.as_ref();
                let secondary_brush = secondary.map(|ctx| &ctx.brush);
                let wet_settings = secondary_brush.map_or(self.ctx.brush.dual_brush.wet_edge.as_ref(), |b| b.wet_edge.as_ref());
                let wet = secondary_brush.map_or(self.ctx.brush.dual_brush.wet_edges, |b| b.wet_edges) && !self.ctx.brush.erase;
                self.ctx.rasterize_into(d, true, rect, &mut self.scratch, wet.then_some(&mut self.depth_scratch));
                let material = if wet {
                    wet_settings.map(|w| (d.wet_edge, (w.outline == WetOutline::Single).then(|| w.color_strength(d.wet_edge.strength))))
                } else {
                    None
                };
                let single =
                    wet.then_some(wet_settings).flatten().filter(|w| w.outline == WetOutline::Single && w.color_mode != crate::material::WetColorMode::Normal);
                if single.is_some()
                    && let (Some(base), Some(_)) = (&self.material_base, self.fmt)
                {
                    base.read_region_into(rect, &mut base_pixels);
                }
                let col = if let (Some(ctx), Some(f)) = (secondary, self.fmt) {
                    colors.clear();
                    let width = rect.width() as usize;
                    for (i, coverage) in self.scratch.iter_mut().enumerate() {
                        let mut rgba = if ctx.brush.pattern_enabled {
                            if let Some(p) = &ctx.brush.sampled_pattern {
                                let rgba = p.sample_pigment(
                                    rect.x0 as f32 + (i % width) as f32 + 0.5 - d.center.x as f32,
                                    rect.y0 as f32 + (i / width) as f32 + 0.5 - d.center.y as f32,
                                );
                                *coverage *= rgba[3];
                                rgba
                            } else {
                                d.color
                            }
                        } else {
                            d.color
                        };
                        if let Some(material) = &ctx.brush.canvas_material {
                            let rgb = material.pigment_response(
                                [rgba[0], rgba[1], rgba[2]],
                                [rect.x0 as f32 + (i % width) as f32 + 0.5, rect.y0 as f32 + (i / width) as f32 + 0.5],
                                ctx.brush.opacity,
                                d.alpha,
                            );
                            rgba[..3].copy_from_slice(&rgb);
                        }
                        if let Some(ws) = single {
                            let n = f.channels();
                            let base = base_pixels.get(i * n..(i + 1) * n).map_or([0.0; 4], |p| to_rgba(&f, p));
                            rgba = ws.color(rgba, base, self.depth_scratch[i], d.wet_edge);
                        }
                        from_rgba_into(&f, rgba, &mut native);
                        colors.extend_from_slice(&native[..f.mode.color_channels()]);
                    }
                    Some((colors.as_slice(), true))
                } else {
                    None
                };
                let ceiling = secondary.map_or(1.0, |ctx| ctx.brush.opacity) * d.opacity;
                if let (Some(transport), Some(ctx)) = (self.transports[1].as_mut(), secondary) {
                    transport.dab(ctx, d, rect, &self.scratch, col);
                }
                dm.accumulate_wet(
                    rect,
                    &self.scratch,
                    ceiling,
                    col,
                    wet.then_some(&self.depth_scratch[..]),
                    material,
                    self.fmt.and_then(|f| {
                        secondary_brush
                            .filter(|b| b.saturation_record_strength > 0.0 || b.saturation_preserve_output > 0.0)
                            .map(|b| (b.saturation_record_strength, b.saturation_preserve_output, b.opacity, f))
                    }),
                );
                let motion = dab_motion(&mut self.previous_centers[1], d);
                if let Some(ctx) = secondary {
                    dm.accumulate_pigment(ctx, d, rect, &self.scratch, motion);
                }
            }
        }
        self.dabs_done += dabs.len();
        if let Some(all) = self.all_dabs.as_mut() {
            all.extend_from_slice(&dabs);
        }
        dabs.clear();
        duals.clear();
        self.dab_buf = dabs;
        self.dual_buf = duals;
    }

    /// See [`DabGenerator::wants_time`].
    pub fn wants_time(&self) -> bool {
        self.generator.wants_time()
    }

    /// The string between the brush and the pointer while smoothing lags (see
    /// [`Smoother::string`](crate::dynamics::Smoother::string)).
    pub fn smoothing_string(&self) -> Option<([f64; 2], [f64; 2])> {
        self.generator.smoothing_string()
    }

    /// Feed input points (any chunking gives the same result).
    pub fn push(&mut self, pts: &[StrokePoint]) {
        self.generator.push(pts, &mut self.dab_buf, &mut self.dual_buf);
        self.raster_pending();
    }

    /// End of stroke (smoothing catch-up, a lone first point).
    pub fn finish(&mut self) {
        self.generator.finish(&mut self.dab_buf, &mut self.dual_buf);
        self.raster_pending();
    }

    /// The primary dabs finishing the stroke now would add, without finishing it: for live
    /// previews of sequential-dab tools, which run each dab themselves (see [`tail_preview`](Self::tail_preview)).
    pub fn tail_dabs(&self) -> Vec<Dab> {
        let mut generator = self.generator.clone();
        let (mut dabs, mut duals) = (Vec::new(), Vec::new());
        generator.finish(&mut dabs, &mut duals);
        dabs
    }

    /// What finishing the stroke now would add (the smoothing catch-up tail to the last point, or
    /// a lone first dab), for live previews: a renderer holding copies of the coverage tiles the
    /// tail touches with the tail rendered in, so its `composite` draws exactly those tiles as
    /// [`finish`](Self::finish) would leave them. `None` when finishing adds nothing.
    pub fn tail_preview(&self) -> Option<StrokeRenderer> {
        let mut generator = self.generator.clone();
        let (mut dabs, mut duals) = (Vec::new(), Vec::new());
        generator.finish(&mut dabs, &mut duals);
        if dabs.is_empty() && duals.is_empty() {
            return None;
        }
        let mut keys = HashSet::new();
        for r in dabs.iter().map(|d| self.ctx.dab_rect(d, false)).chain(duals.iter().map(|d| self.ctx.dab_rect(d, true))) {
            if r.is_empty() {
                continue;
            }
            for ty in r.y0.div_euclid(COV_TILE)..=(r.y1 - 1).div_euclid(COV_TILE) {
                for tx in r.x0.div_euclid(COV_TILE)..=(r.x1 - 1).div_euclid(COV_TILE) {
                    keys.insert((tx, ty));
                }
            }
        }
        let subset = |m: &CoverageMap| CoverageMap {
            tiles: keys.iter().filter_map(|k| m.tiles.get(k).map(|t| (*k, t.clone()))).collect(),
            nc: m.nc,
            bounds: m.bounds,
            dirty: HashSet::new(),
        };
        let mut t = StrokeRenderer {
            ctx: self.ctx.clone(),
            generator,
            cov: subset(&self.cov),
            dual: self.dual.as_ref().map(subset),
            fmt: self.fmt,
            per_dab_color: self.per_dab_color,
            material_base: self.material_base.clone(),
            material_candidates: self.material_candidates.clone(),
            previous_centers: self.previous_centers,
            transports: self.transports.clone(),
            dabs_done: self.dabs_done,
            scratch: Vec::new(),
            depth_scratch: Vec::new(),
            dab_buf: dabs,
            dual_buf: duals,
            all_dabs: None,
        };
        t.raster_pending();
        // Tiles whose coverage the tail doesn't change still composite the same: redraw them all.
        t.cov.dirty.extend(t.cov.tiles.keys().copied());
        Some(t)
    }

    /// Mark the coverage tiles over `r` for the next `composite` (e.g. to redraw over a preview).
    pub fn mark_dirty(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        for ty in r.y0.div_euclid(COV_TILE)..=(r.y1 - 1).div_euclid(COV_TILE) {
            for tx in r.x0.div_euclid(COV_TILE)..=(r.x1 - 1).div_euclid(COV_TILE) {
                if self.cov.tiles.contains_key(&(tx, ty)) {
                    self.cov.dirty.insert((tx, ty));
                }
            }
        }
    }

    /// The coverage tiles touched since the last call (or `composite`), as one rectangle clipped to
    /// the stroke bounds, and forget them: what a live preview that composites its own paint has
    /// to redraw.
    pub fn take_dirty_rect(&mut self) -> Rect {
        let r = self
            .cov
            .take_dirty()
            .into_iter()
            .fold(Rect::EMPTY, |acc, (tx, ty)| acc.union(&Rect::new(tx * COV_TILE, ty * COV_TILE, (tx + 1) * COV_TILE, (ty + 1) * COV_TILE)));
        if let Some(d) = self.dual.as_mut() {
            d.dirty.clear();
        }
        r.intersect(&self.bounds())
    }

    /// Final coverage over `r`, one value per pixel in rows (as [`dense_coverage`](Self::dense_coverage)
    /// over its bounds).
    pub fn coverage_in(&self, r: Rect) -> Vec<f32> {
        (r.y0..r.y1).flat_map(|y| (r.x0..r.x1).map(move |x| (x, y))).map(|(x, y)| self.coverage_at(x, y)).collect()
    }

    fn expand_keys(keys: &mut HashSet<(i32, i32)>) {
        let changed: Vec<_> = keys.iter().copied().collect();
        for (x, y) in changed {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    keys.insert((x + dx, y + dy));
                }
            }
        }
    }

    fn spatial_wet(&self) -> bool {
        let b = &self.ctx.brush;
        let spatial = |w: &crate::material::WetEdgeSettings| w.style == WetStyle::Watercolor2 && w.outline == WetOutline::Stroke;
        (b.wet_edges && b.wet_edge.as_ref().is_some_and(spatial))
            || self.ctx.secondary.as_ref().is_some_and(|c| c.brush.wet_edges && c.brush.wet_edge.as_ref().is_some_and(spatial))
            || (self.ctx.secondary.is_none() && b.dual_brush.enabled && b.dual_brush.wet_edges && b.dual_brush.wet_edge.as_ref().is_some_and(spatial))
    }

    fn wet_pixel(&self, dual: bool, x: i32, y: i32) -> (f32, f32, ResolvedWet) {
        let b = &self.ctx.brush;
        let map = if dual { self.dual.as_ref().unwrap_or(&self.cov) } else { &self.cov };
        let c = map.get(x, y);
        let ws = if dual {
            if let Some(ctx) = &self.ctx.secondary {
                ctx.brush.wet_edges.then_some(ctx.brush.wet_edge.as_ref()).flatten()
            } else {
                b.dual_brush.wet_edges.then_some(b.dual_brush.wet_edge.as_ref()).flatten()
            }
        } else {
            b.wet_edges.then_some(b.wet_edge.as_ref()).flatten()
        };
        let (wet, single_edge) = map.material_at(x, y);
        let Some(ws) = ws.filter(|_| !b.erase) else { return (c, 0.0, wet) };
        if c <= 0.0 {
            return (0.0, 0.0, wet);
        }
        if ws.outline == WetOutline::Single {
            return (c, single_edge, wet);
        }
        let edge = if ws.style == WetStyle::Watercolor2 { ws.spatial_edge(c, wet.width, x, y, |px, py| map.get(px, py)) } else { ws.color_mask(c, wet.width) };
        (ws.alpha(c, edge, wet), edge, wet)
    }

    fn material_color(&self, mut src: [f32; 4], base: [f32; 4], x: i32, y: i32) -> [f32; 4] {
        let b = &self.ctx.brush;
        if b.erase {
            return src;
        }
        if b.wet_edges
            && let Some(ws) = b.wet_edge.as_ref().filter(|w| w.outline != WetOutline::Single)
        {
            let (_, edge, wet) = self.wet_pixel(false, x, y);
            src = ws.color(src, base, edge, wet);
        }
        if self.ctx.secondary.is_none()
            && self.dual.is_some()
            && b.dual_brush.wet_edges
            && let Some(ws) = &b.dual_brush.wet_edge
        {
            let (_, edge, wet) = self.wet_pixel(true, x, y);
            src = if ws.outline == WetOutline::Single { ws.color_with_effect(src, base, edge) } else { ws.color(src, base, edge, wet) };
        }
        src
    }

    fn transported_output(&self, destination: [f32; 4], normal: [f32; 4], x: i32, y: i32, selection: f32) -> [f32; 4] {
        if self.transports.iter().all(Option::is_none) {
            return normal;
        }
        let brush = &self.ctx.brush;
        let lane_source = |map: &CoverageMap, b: &BrushSettings| {
            let c = map.get(x, y);
            let mut source = b.color;
            let (tx, ty) = (x.div_euclid(COV_TILE), y.div_euclid(COV_TILE));
            let i = ((y - ty * COV_TILE) * COV_TILE + x - tx * COV_TILE) as usize;
            if let (Some(tile), Some(fmt)) = (map.tiles.get(&(tx, ty)), self.fmt)
                && c > 0.0
                && !tile.col.is_empty()
            {
                let mut px = [0.0; 8];
                from_rgba_into(&fmt, source, &mut px);
                for (ch, v) in px.iter_mut().take(map.nc).enumerate() {
                    *v = tile.col[i * map.nc + ch] / c;
                }
                source = to_rgba(&fmt, &px[..fmt.channels()]);
            }
            source
        };
        let lane_wet_color = |secondary: bool, b: &BrushSettings, value: [f32; 4]| {
            if !secondary {
                return self.material_color(value, destination, x, y);
            }
            if b.wet_edges
                && let Some(w) = b.wet_edge.as_ref().filter(|w| w.outline == WetOutline::Stroke)
            {
                let (_, edge, wet) = self.wet_pixel(true, x, y);
                w.color(value, destination, edge, wet)
            } else {
                value
            }
        };
        let primary = self.transports[0].as_ref().map_or_else(
            || {
                let source = self.material_color(lane_source(&self.cov, brush), destination, x, y);
                let coverage = self.wet_pixel(false, x, y).0 * brush.opacity;
                self.fmt.map_or_else(
                    || photocraft_color::blend::composite(brush.mode, destination, source, coverage),
                    |fmt| lane_composite(&fmt, brush, destination, source, coverage, false),
                )
            },
            |t| {
                let value = lane_wet_color(false, brush, t.work.rgba(x, y));
                if brush.color_attributes.enabled { brush.color_attributes.composite(value, destination, value, self.cov.get(x, y) * value[3]) } else { value }
            },
        );
        let output: [f32; 4] = if let (Some(ctx), Some(dm)) = (&self.ctx.secondary, &self.dual) {
            let secondary = self.transports[1].as_ref().map_or_else(
                || {
                    let c = self.wet_pixel(true, x, y).0;
                    let source = lane_wet_color(true, &ctx.brush, lane_source(dm, &ctx.brush));
                    self.fmt.map_or_else(
                        || photocraft_color::blend::composite(ctx.brush.mode, destination, source, c),
                        |fmt| lane_composite(&fmt, &ctx.brush, destination, source, c, false),
                    )
                },
                |t| {
                    let value = lane_wet_color(true, &ctx.brush, t.work.rgba(x, y));
                    if ctx.brush.color_attributes.enabled {
                        ctx.brush.color_attributes.composite(value, destination, value, dm.get(x, y) * value[3])
                    } else {
                        value
                    }
                },
            );
            let p = self.cov.get(x, y) * brush.opacity * brush.dual_brush.primary_strength;
            let d = dm.get(x, y) * brush.dual_brush.secondary_strength;
            let weight = (d / (p + d).max(0.0001)).clamp(0.0, 1.0);
            let union = p + d - p * d;
            let filtered = self.coverage_at(x, y);
            let factor = (filtered / union.max(0.0001)).clamp(0.0, 1.0);
            std::array::from_fn(|i| destination[i] + (primary[i] + (secondary[i] - primary[i]) * weight - destination[i]) * factor)
        } else {
            let base = self.cov.get(x, y);
            let masked = self.coverage_at(x, y);
            let factor = if base > 0.0 { (masked / base).clamp(0.0, 1.0) } else { 0.0 };
            std::array::from_fn(|i| destination[i] + (primary[i] - destination[i]) * factor * brush.opacity)
        };
        let mut result = std::array::from_fn(|i| destination[i] + (output[i] - destination[i]) * selection);
        if brush.erase {
            result = normal;
        }
        result
    }

    fn shade_pigment(
        &self,
        normal: [f32; 4],
        x: i32,
        y: i32,
        pre: &Surface,
        candidates: &HashMap<(i32, i32), Arc<[[f32; 4]]>>,
        selection: Option<&Surface>,
    ) -> [f32; 4] {
        let mut selected = [0.0; 8];
        let selection_value = selection.map_or(1.0, |s| {
            s.read_pixel(x, y, &mut selected);
            selected[0]
        });
        if self.coverage_at(x, y) * selection_value <= 0.0 {
            return normal;
        }
        let bilinear = |point: [f32; 2], read: &dyn Fn(i32, i32) -> [f32; 4]| {
            let (ix, iy) = (point[0].floor() as i32, point[1].floor() as i32);
            let (fx, fy) = (point[0] - point[0].floor(), point[1] - point[1].floor());
            let (a, b, c, d) = (read(ix, iy), read(ix + 1, iy), read(ix, iy + 1), read(ix + 1, iy + 1));
            std::array::from_fn(|i| (a[i] + (b[i] - a[i]) * fx) * (1.0 - fy) + (c[i] + (d[i] - c[i]) * fx) * fy)
        };
        let source = |xx: i32, yy: i32| {
            let (tx, ty) = (xx.div_euclid(COV_TILE), yy.div_euclid(COV_TILE));
            let i = ((yy - ty * COV_TILE) * COV_TILE + xx - tx * COV_TILE) as usize;
            candidates.get(&(tx, ty)).and_then(|v| v.get(i)).copied().unwrap_or_else(|| pre.rgba(xx, yy))
        };
        let shade_lane = |map: &CoverageMap| {
            let (tx, ty) = (x.div_euclid(COV_TILE), y.div_euclid(COV_TILE));
            let i = ((y - ty * COV_TILE) * COV_TILE + x - tx * COV_TILE) as usize;
            let tile = map.tiles.get(&(tx, ty));
            let settings = tile.and_then(|t| t.pigment_settings.get(i)).copied().unwrap_or_default();
            let motion = tile.and_then(|t| t.pigment_motion.get(i)).copied().unwrap_or([0.0; 2]);
            let material = |xx: i32, yy: i32| {
                let raw = map.pigment_at(xx, yy);
                let mut selected = [0.0; 8];
                let selection = selection.map_or(1.0, |s| {
                    s.read_pixel(xx, yy, &mut selected);
                    selected[0]
                });
                let opacity = if self.ctx.secondary.is_some() { 1.0 } else { self.ctx.brush.opacity };
                let coverage = (self.coverage_at(xx, yy) * opacity * selection).clamp(0.0, 1.0);
                if raw[3] > 0.0 { raw.map(|v| v * coverage / raw[3]) } else { [0.0, 0.0, 0.0, coverage] }
            };
            crate::pigment_material::shade(
                normal,
                settings,
                settings.pressure,
                motion,
                |o| bilinear([x as f32 + o[0], y as f32 + o[1]], &material),
                |o| bilinear([x as f32 + o[0], y as f32 + o[1]], &source),
            )
        };
        let primary = shade_lane(&self.cov);
        if let (Some(_), Some(dm)) = (&self.ctx.secondary, &self.dual) {
            let secondary = shade_lane(dm);
            let p = self.cov.get(x, y) * self.ctx.brush.opacity * self.ctx.brush.dual_brush.primary_strength;
            let d = dm.get(x, y) * self.ctx.brush.dual_brush.secondary_strength;
            let w = (d / (p + d).max(0.0001)).clamp(0.0, 1.0);
            std::array::from_fn(|i| primary[i] + (secondary[i] - primary[i]) * w)
        } else {
            primary
        }
    }

    /// Final stroke coverage at a pixel (stroke-level masks applied, before opacity/selection).
    pub fn coverage_at(&self, x: i32, y: i32) -> f32 {
        let c = self.cov.get(x, y);
        if c <= 0.0 && self.ctx.secondary.is_none() {
            return 0.0;
        }
        let b = &self.ctx.brush;
        if b.wet_edge.is_none() && b.dual_brush.wet_edge.is_none() && self.ctx.secondary.as_ref().is_none_or(|c| c.brush.wet_edge.is_none()) {
            return self.ctx.stroke_mask_wet(c, self.cov.depth(x, y), self.dual.as_ref().map(|d| d.get(x, y)), x, y);
        }
        let c = self.wet_pixel(false, x, y).0;
        let dual = self.dual.as_ref().map(|_| self.wet_pixel(true, x, y).0);
        self.ctx.stroke_mask_wet(c, self.cov.depth(x, y), dual, x, y)
    }

    /// Dense final coverage over the stroke bounds.
    pub fn dense_coverage(&self) -> (Rect, Vec<f32>) {
        let b = self.bounds();
        let out = (b.y0..b.y1).flat_map(|y| (b.x0..b.x1).map(move |x| (x, y))).map(|(x, y)| self.coverage_at(x, y)).collect();
        (b, out)
    }

    /// Composite the stroke buffer onto `target`, reading original pixels from `pre` (the
    /// pre-stroke surface; pass a clone of the target taken before the first `push`). With
    /// `all = false` only tiles touched since the last composite are processed (interactive use).
    /// Returns the damaged rectangle.
    pub fn composite(&mut self, pre: &Surface, target: &mut Surface, selection: Option<&Surface>, lock_transparency: bool, all: bool) -> Rect {
        self.composite_tiles(pre, target, selection, lock_transparency, all, true)
    }

    fn composite_tiles(&mut self, pre: &Surface, target: &mut Surface, selection: Option<&Surface>, lock_transparency: bool, all: bool, halo: bool) -> Rect {
        let pigment_active = !self.ctx.brush.erase
            && (self.ctx.brush.pigment.as_ref().is_some_and(|p| p.enabled)
                || self.ctx.secondary.as_ref().is_some_and(|c| c.brush.pigment.as_ref().is_some_and(|p| p.enabled)));
        let spatial = self.spatial_wet() || pigment_active;
        if !all && let Some(d) = &self.dual {
            self.cov.dirty.extend(d.dirty.iter().copied());
        }
        if spatial && !all && halo {
            Self::expand_keys(&mut self.cov.dirty);
        }
        let keys: Vec<(i32, i32)> = if all {
            self.cov.dirty.clear();
            let mut k: Vec<_> = self.cov.tiles.keys().copied().collect();
            if self.ctx.secondary.is_some()
                && let Some(d) = &self.dual
            {
                k.extend(d.tiles.keys().copied());
            }
            k.sort_unstable();
            k.dedup();
            k
        } else {
            self.cov.take_dirty()
        };
        if let Some(d) = self.dual.as_mut() {
            d.dirty.clear();
        }
        let bounds = self.bounds();
        let fmt = target.format();
        let n = fmt.channels();
        let a_idx = alpha_index(&fmt);
        let nc = fmt.mode.color_channels();
        let b = &self.ctx.brush;
        let opacity = if self.ctx.secondary.is_some() { 1.0 } else { b.opacity.clamp(0.0, 1.0) };
        let mut src = [0.0f32; 8];
        from_rgba_into(&fmt, b.color, &mut src);
        // A colour picked as CMYK paints its inks as they are in a CMYK target (#4344).
        if let (ColorMode::Cmyk, Some((rgb, inks))) = (fmt.mode, b.color_cmyk)
            && rgb == [b.color[0], b.color[1], b.color[2]]
        {
            src[..4].copy_from_slice(&inks);
        }
        if let Some(a) = a_idx {
            src[a] = b.color[3];
        }
        // 8-bit targets: the pixels the stroke changes are dithered as they are quantised, so
        // soft, low-flow strokes don't band (see `dither8`). Aliased strokes (the Pencil, the
        // Eraser's Pencil and Block modes) paint every pixel by the same amount, so they have no
        // gradient to band and are not dithered: a Pencil line at partial opacity is one flat
        // colour, as in Photoshop, not grain that a tolerance-0 Magic Wand or Paint Bucket trips on.
        let dither = fmt.sample == SampleType::U8 && !b.aliased;
        let mut dmg = Rect::EMPTY;
        let mut candidates = std::mem::take(&mut self.material_candidates);
        let mut material_regions = Vec::new();
        let mut region = Vec::new();
        for (tx, ty) in keys {
            let primary_tile = self.cov.tiles.get(&(tx, ty));
            let Some(tile) = primary_tile.or_else(|| self.dual.as_ref().and_then(|d| d.tiles.get(&(tx, ty)))) else { continue };
            let tr = Rect::new(tx * COV_TILE, ty * COV_TILE, (tx + 1) * COV_TILE, (ty + 1) * COV_TILE).intersect(&bounds);
            if tr.is_empty() {
                continue;
            }
            pre.read_region_into(tr, &mut region);
            let sel = selection.map(|s| (s.channels(), s.read_region(tr)));
            let w = tr.width() as usize;
            for y in tr.y0..tr.y1 {
                for x in tr.x0..tr.x1 {
                    let ti = ((y - ty * COV_TILE) * COV_TILE + (x - tx * COV_TILE)) as usize;
                    let c = primary_tile.map_or(0.0, |t| t.cov[ti]);
                    if c <= 0.0 && self.ctx.secondary.is_none() {
                        continue;
                    }
                    let i = (y - tr.y0) as usize * w + (x - tr.x0) as usize;
                    let m = self.coverage_at(x, y);
                    let s = sel.as_ref().map_or(1.0, |(sc, v)| v[i * sc]);
                    let mut k = (m * opacity * s).min(1.0);
                    if k <= 0.0 {
                        continue;
                    }
                    if b.mode == BlendMode::Dissolve {
                        k = if hash2(x, y, b.seed) < k { 1.0 } else { 0.0 };
                        if k == 0.0 {
                            continue;
                        }
                    }
                    let px = &mut region[i * n..(i + 1) * n];
                    if b.erase {
                        if let (Some(a), false) = (a_idx, lock_transparency) {
                            px[a] *= 1.0 - k;
                        }
                        if dither {
                            dither8(px, x, y);
                        }
                        continue;
                    }
                    if lock_transparency && a_idx.is_some_and(|a| px[a] <= 0.0) {
                        continue;
                    }
                    if self.per_dab_color && c > 0.0 && !tile.col.is_empty() {
                        let p = &tile.col[ti * nc..(ti + 1) * nc];
                        for ch in 0..nc {
                            src[ch] = p[ch] / c;
                        }
                    }
                    let mut pixel_src = src;
                    let primary_color = b.wet_edges
                        && b.wet_edge.as_ref().is_some_and(|w| w.outline == WetOutline::Stroke && w.color_mode != crate::material::WetColorMode::Normal);
                    let dual_color = b.dual_brush.enabled
                        && b.dual_brush.wet_edges
                        && b.dual_brush.wet_edge.as_ref().is_some_and(|w| w.color_mode != crate::material::WetColorMode::Normal);
                    if primary_color || dual_color {
                        let rgba = self.material_color(to_rgba(&fmt, &src[..n]), to_rgba(&fmt, px), x, y);
                        from_rgba_into(&fmt, rgba, &mut pixel_src);
                    }
                    let destination = to_rgba(&fmt, px);
                    let source = to_rgba(&fmt, &pixel_src[..n]);
                    let native_only = self.ctx.secondary.is_none()
                        && !b.color_attributes.enabled
                        && b.pigment_mix_strength <= 0.0
                        && matches!(b.mode, BlendMode::Normal | BlendMode::Dissolve);
                    let mut result = if native_only { destination } else { lane_composite(&fmt, b, destination, source, k, lock_transparency) };
                    if let (Some(ctx), Some(dm)) = (&self.ctx.secondary, &self.dual)
                        && let Some(tile) = dm.tiles.get(&(tx, ty))
                        && tile.cov[ti] > 0.0
                        && !tile.col.is_empty()
                    {
                        let coverage = tile.cov[ti];
                        let mut secondary = pixel_src;
                        for (ch, value) in secondary.iter_mut().enumerate().take(nc) {
                            *value = tile.col[ti * nc + ch] / coverage;
                        }
                        let mut other = to_rgba(&fmt, &secondary[..n]);
                        if ctx.brush.wet_edges
                            && let Some(ws) = ctx.brush.wet_edge.as_ref().filter(|w| w.outline != WetOutline::Single)
                        {
                            let (_, edge, wet) = self.wet_pixel(true, x, y);
                            other = ws.color(other, destination, edge, wet);
                        }
                        let secondary = lane_composite(&fmt, &ctx.brush, destination, other, k, lock_transparency);
                        let p = self.wet_pixel(false, x, y).0 * b.opacity * b.dual_brush.primary_strength;
                        let d = self.wet_pixel(true, x, y).0 * b.dual_brush.secondary_strength;
                        let weight = (d / (p + d).max(0.0001)).clamp(0.0, 1.0);
                        for ch in 0..4 {
                            result[ch] += (secondary[ch] - result[ch]) * weight;
                        }
                    }
                    if native_only {
                        over_native(&fmt, px, &pixel_src[..n], k, lock_transparency);
                    } else {
                        let mut encoded = [0.0; 8];
                        from_rgba_into(&fmt, result, &mut encoded);
                        px.copy_from_slice(&encoded[..n]);
                    }
                    if self.transports.iter().any(Option::is_some) {
                        let mut result = self.transported_output(destination, to_rgba(&fmt, px), x, y, s);
                        if lock_transparency {
                            result[3] = destination[3];
                        }
                        let mut encoded = [0.0; 8];
                        from_rgba_into(&fmt, result, &mut encoded);
                        px.copy_from_slice(&encoded[..n]);
                    }
                    if dither && !pigment_active {
                        dither8(px, x, y);
                    }
                }
            }
            if pigment_active {
                let pixels = candidates.entry((tx, ty)).or_insert_with(|| {
                    (0..COV_TILE).flat_map(|yy| (0..COV_TILE).map(move |xx| pre.rgba(tx * COV_TILE + xx, ty * COV_TILE + yy))).collect::<Vec<_>>().into()
                });
                let pixels = Arc::make_mut(pixels);
                for y in tr.y0..tr.y1 {
                    for x in tr.x0..tr.x1 {
                        let i = (y - tr.y0) as usize * w + (x - tr.x0) as usize;
                        let ti = ((y - ty * COV_TILE) * COV_TILE + x - tx * COV_TILE) as usize;
                        pixels[ti] = to_rgba(&fmt, &region[i * n..(i + 1) * n]);
                    }
                }
                material_regions.push(tr);
            } else {
                target.write_region(tr, &region);
            }
            dmg = dmg.union(&tr);
        }
        for tr in material_regions {
            region.clear();
            for y in tr.y0..tr.y1 {
                for x in tr.x0..tr.x1 {
                    let (tx, ty) = (x.div_euclid(COV_TILE), y.div_euclid(COV_TILE));
                    let ti = ((y - ty * COV_TILE) * COV_TILE + x - tx * COV_TILE) as usize;
                    let normal = candidates.get(&(tx, ty)).and_then(|v| v.get(ti)).copied().unwrap_or_else(|| pre.rgba(x, y));
                    let shaded = self.shade_pigment(normal, x, y, pre, &candidates, selection);
                    let mut native = [0.0; 8];
                    from_rgba_into(&fmt, shaded, &mut native);
                    let mut selected = [0.0; 8];
                    let selection_value = selection.map_or(1.0, |s| {
                        s.read_pixel(x, y, &mut selected);
                        selected[0]
                    });
                    if dither && self.coverage_at(x, y) * selection_value > 0.0 {
                        dither8(&mut native[..n], x, y);
                    }
                    region.extend_from_slice(&native[..n]);
                }
            }
            target.write_region(tr, &region);
        }
        self.material_candidates = candidates;
        if all { bounds } else { dmg }
    }
}

fn lane_composite(fmt: &PixelFormat, b: &BrushSettings, destination: [f32; 4], source: [f32; 4], coverage: f32, lock: bool) -> [f32; 4] {
    let mut normal = if matches!(b.mode, BlendMode::Normal | BlendMode::Dissolve) {
        let n = fmt.channels();
        let mut d = [0.0; 8];
        let mut s = [0.0; 8];
        from_rgba_into(fmt, destination, &mut d);
        from_rgba_into(fmt, source, &mut s);
        over_native(fmt, &mut d[..n], &s[..n], coverage, lock);
        to_rgba(fmt, &d[..n])
    } else {
        photocraft_color::blend::composite(b.mode, destination, source, coverage)
    };
    if lock {
        normal[3] = destination[3];
    }
    let amount = coverage * source[3];
    if b.pigment_mix_strength > 0.0 && fmt.mode == ColorMode::Rgb {
        let combined = amount + destination[3] * (1.0 - amount);
        normal = crate::pigment::paint(normal, destination, source, amount / combined.max(0.000001), b.pigment_mix_strength);
    }
    if b.color_attributes.enabled {
        normal = b.color_attributes.composite(normal, destination, source, amount);
    }
    if lock {
        normal[3] = destination[3];
    }
    normal
}

fn c3_tip_falloff(distance: f32, radius: f32, hardness: f32, subpixel: bool) -> f32 {
    if hardness >= 0.999 {
        return if subpixel { if distance <= radius { 1.0 } else { 0.0 } } else { (radius - distance + 0.5).clamp(0.0, 1.0) };
    }
    let h = hardness.clamp(0.0, 0.999);
    let t = ((distance / radius.max(0.0005) - h) / (1.0 - h)).clamp(0.0, 1.0);
    let tail = (-2.5f32).exp();
    ((-2.5 * t * t).exp() - tail) / (1.0 - tail)
}

fn c3_sampled_mask(m: &Mips, u: f32, v: f32, hardness: f32) -> f32 {
    if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
        return 0.0;
    }
    let (w, h, _) = &m.levels[0];
    let (w, h) = (*w as f32, *h as f32);
    let sample = |x: f32, y: f32| m.sample_clamped(0, x.clamp(0.5 / w, 1.0 - 0.5 / w), y.clamp(0.5 / h, 1.0 - 0.5 / h));
    let base = sample(u, v);
    let hrd = hardness.clamp(0.0, 1.0);
    if hrd >= 0.999 {
        return base;
    }
    let boost = ((1.0 - hrd) * 5.0).clamp(0.0, 1.0);
    let rx = ((0.6 + boost * 6.4) / w).min(0.125);
    let ry = ((0.6 + boost * 6.4) / h).min(0.125);
    let mut blur = base * 0.28;
    for (x, y) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
        blur += sample(u + x * rx, v + y * ry) * 0.11;
    }
    for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        blur += sample(u + x * rx, v + y * ry) * 0.07;
    }
    base.min(base + (blur - base) * boost).clamp(0.0, 1.0).powf(2.2 - 1.2 * hrd)
        * c3_tip_falloff((u * 2.0 - 1.0).abs().max((v * 2.0 - 1.0).abs()), 1.0, hrd, false)
}

fn triangle_mask(u: f32, v: f32, radius: f32, roundness: f32, hardness: f32, subpixel: bool) -> f32 {
    let qx = u / radius.max(0.1);
    let qy = -v / (radius * roundness).max(0.1);
    let edge = ((qy + 1.0 - 2.0 * qx) / 5.0f32.sqrt()).min(1.0 - qy).min((qy + 2.0 * qx + 1.0) / 5.0f32.sqrt());
    if edge <= 0.0 {
        return 0.0;
    }
    let distance = (1.0 - edge / 0.4472136).clamp(0.0, 1.0);
    c3_tip_falloff(distance * radius, radius, hardness, subpixel)
}

fn dab_motion(previous: &mut Option<Point>, d: &Dab) -> [f32; 2] {
    let last = previous.replace(d.center).unwrap_or(d.center);
    let dx = (d.center.x - last.x) as f32;
    let dy = (d.center.y - last.y) as f32;
    let length = dx.hypot(dy);
    let cap = (d.radius * 2.0 * 0.08).clamp(2.0, 8.0);
    let k = if length > cap { cap / length } else { 1.0 };
    [dx * k, dy * k]
}

/// Render a whole stroke onto `target` (one-shot). Returns the damaged rectangle.
pub fn render_stroke(
    target: &mut Surface,
    brush: &BrushSettings,
    points: &[StrokePoint],
    selection: Option<&Surface>,
    lock_transparency: bool,
    zoom: f32,
) -> Rect {
    if points.is_empty() {
        return Rect::EMPTY;
    }
    let pre = target.clone();
    let mut r = StrokeRenderer::on_surface(brush, &pre, zoom);
    r.push(points);
    r.finish();
    r.composite(&pre, target, selection, lock_transparency, true)
}

// The zero-angle transform only changes signs; squaring gives the same radial distance.
#[inline(never)]
fn rasterize_round(d: &Dab, rect: Rect, out: &mut [f32], hardness: f32, aliased: bool) {
    let (cx, cy) = if aliased { grid_center(d.center.x, d.center.y, 2.0 * d.radius) } else { (d.center.x as f32, d.center.y as f32) };
    let ro = d.roundness.max(0.5 / d.radius).min(1.0);
    let rm = (d.radius * ro).max(0.5);
    let w = rect.width() as usize;
    let reach = if aliased { rm } else { tip_reach(rm, hardness).max(rm) };
    let reach2 = (reach + 0.5) * (reach + 0.5);
    for (yy, row) in out.chunks_exact_mut(w).enumerate() {
        let dy = (rect.y0 + yy as i32) as f32 + 0.5 - cy;
        if aliased {
            for (xx, dst) in row.iter_mut().enumerate() {
                let dx = (rect.x0 + xx as i32) as f32 + 0.5 - cx;
                let d2 = (dx * ro).powi(2) + dy * dy;
                *dst = if d2 < reach2 && d2.sqrt() <= rm { d.alpha } else { 0.0 };
            }
        } else {
            for (xx, dst) in row.iter_mut().enumerate() {
                let dx = (rect.x0 + xx as i32) as f32 + 0.5 - cx;
                let d2 = (dx * ro).powi(2) + dy * dy;
                let val = if d2 >= reach2 { 0.0 } else { tip_falloff(d2.sqrt(), rm, hardness) };
                *dst = if val <= 0.0 { 0.0 } else { val * d.alpha };
            }
        }
    }
}

#[cfg(test)]
#[path = "render_reference.rs"]
mod reference;

#[cfg(test)]
mod material_regression {
    use super::*;
    use crate::material::{WetColorMode, WetEdgeSettings};
    use photocraft_geom::Point;

    #[test]
    fn transported_lanes_filter_their_own_completed_wet_colors_before_merging() {
        let first = WetEdgeSettings { outline: WetOutline::Stroke, color_mode: WetColorMode::Hsl, ..Default::default() };
        let second = WetEdgeSettings { outline: WetOutline::Stroke, color_mode: WetColorMode::Multiply, ..Default::default() };
        let secondary = BrushSettings {
            wet_edges: true,
            wet_edge: Some(second.clone()),
            mix_settings: crate::transport::BrushMixSettings { enabled: true, ..Default::default() },
            ..Default::default()
        };
        let brush = BrushSettings {
            wet_edges: true,
            wet_edge: Some(first.clone()),
            mix_settings: crate::transport::BrushMixSettings { enabled: true, ..Default::default() },
            dual_brush: crate::DualBrush { enabled: true, mode: crate::MaskMode::Normal, full_parameters: Some(Box::new(secondary)), ..Default::default() },
            ..Default::default()
        };
        let fmt = PixelFormat::new(ColorMode::Rgb, SampleType::F32, true);
        let destination = [0.8, 0.6, 0.4, 1.0];
        let base = Surface::with_default(fmt, &destination);
        let mut renderer = StrokeRenderer::on_surface(&brush, &base, 1.0);
        let rect = Rect::new(0, 0, 1, 1);
        renderer.cov.accumulate_wet(rect, &[0.4], 1.0, None, None, Some((first.resolve(1.0, 20.0), None)), None);
        renderer.dual.as_mut().unwrap().accumulate_wet(rect, &[0.6], 1.0, None, None, Some((second.resolve(1.0, 20.0), None)), None);
        let a = [0.5, 0.3, 0.1, 1.0];
        let b = [0.2, 0.6, 0.8, 1.0];
        renderer.transports[0].as_mut().unwrap().work.write_pixel(0, 0, &a);
        renderer.transports[1].as_mut().unwrap().work.write_pixel(0, 0, &b);
        let (_, edge_a, wet_a) = renderer.wet_pixel(false, 0, 0);
        let (_, edge_b, wet_b) = renderer.wet_pixel(true, 0, 0);
        let a = first.color(a, destination, edge_a, wet_a);
        let b = second.color(b, destination, edge_b, wet_b);
        let p = 0.4 * brush.dual_brush.primary_strength;
        let d = 0.6 * brush.dual_brush.secondary_strength;
        let weight = d / (p + d);
        let factor = (renderer.coverage_at(0, 0) / (p + d - p * d)).clamp(0.0, 1.0);
        let expected: [f32; 4] = std::array::from_fn(|i| destination[i] + (a[i] + (b[i] - a[i]) * weight - destination[i]) * factor);
        let actual = renderer.transported_output(destination, [0.0; 4], 0, 0, 1.0);
        for i in 0..4 {
            assert!((actual[i] - expected[i]).abs() < 1e-6, "{actual:?} != {expected:?}");
        }
    }

    #[test]
    fn saturated_alpha_still_accepts_the_next_dab_color() {
        let mut map = CoverageMap::new(3);
        let rect = Rect::new(0, 0, 1, 1);
        map.accumulate(rect, &[1.0], 1.0, Some(&[1.0, 0.0, 0.0]), None);
        map.accumulate(rect, &[1.0], 1.0, Some(&[0.0, 0.0, 1.0]), None);
        let tile = &map.tiles[&(0, 0)];
        assert_eq!(tile.cov[0], 1.0);
        assert_eq!(&tile.col[..3], &[0.0, 0.0, 1.0]);
    }

    #[test]
    fn c3_soft_tip_has_exact_gaussian_edge_and_sampled_hardness_only_removes_coverage() {
        let tail = (-2.5f32).exp();
        let expected = ((-2.5f32 * 0.25).exp() - tail) / (1.0 - tail);
        assert!((c3_tip_falloff(5.0, 10.0, 0.0, false) - expected).abs() < 1e-6);
        assert_eq!(c3_tip_falloff(10.0, 10.0, 0.0, false), 0.0);
        assert_eq!(c3_tip_falloff(10.1, 10.0, 1.0, true), 0.0);
        let tile = crate::GrayTile::from_fn(8, 8, |x, y| if x > 1 && x < 6 && y > 1 && y < 6 { 1.0 } else { 0.0 });
        let m = Mips::new(&tile);
        for y in 0..16 {
            for x in 0..16 {
                let u = (x as f32 + 0.5) / 16.0;
                let v = (y as f32 + 0.5) / 16.0;
                assert!(c3_sampled_mask(&m, u, v, 0.0) <= c3_sampled_mask(&m, u, v, 1.0) + 1e-6);
            }
        }
        assert_eq!(triangle_mask(9.0, 0.0, 10.0, 1.0, 1.0, false), 0.0);
        assert!(triangle_mask(0.0, 0.0, 10.0, 1.0, 1.0, false) > 0.9);
    }

    #[test]
    fn pigment_shading_is_incremental_and_selection_clipped() {
        use crate::pigment_material::PigmentSettings;
        let brush = BrushSettings {
            c3_tip_math: true,
            hardness: 0.6,
            color: [0.6, 0.2, 0.1, 1.0],
            pigment: Some(PigmentSettings {
                enabled: true,
                thickness: 80.0,
                visual_thickness: 70.0,
                highlight_strength: 0.6,
                liquidity: 0.4,
                ..Default::default()
            }),
            ..Default::default()
        };
        let fmt = PixelFormat::new(ColorMode::Rgb, SampleType::F32, true);
        let base = Surface::with_default(fmt, &[0.2, 0.3, 0.4, 1.0]);
        let mut renderer = StrokeRenderer::on_surface(&brush, &base, 1.0);
        let mut incremental = base.clone();
        let mut dabs = Vec::new();
        for x in [60.0, 64.0, 69.0] {
            let mut d = Dab::round(Point::new(x, 32.0), 9.0, 0.7);
            d.color = brush.color;
            d.hardness = brush.hardness;
            d.pigment = crate::pigment_material::ResolvedPigment {
                enabled: true,
                pressure: 1.0,
                thickness: 0.8,
                visual_thickness: 0.7,
                highlight_strength: 0.6,
                liquidity: 0.4,
                ..Default::default()
            };
            dabs.push(d);
        }
        for d in &dabs {
            renderer.dab_buf.push(*d);
            renderer.raster_pending();
            renderer.composite(&base, &mut incremental, None, false, false);
        }
        let mut whole = StrokeRenderer::on_surface(&brush, &base, 1.0);
        whole.dab_buf = dabs;
        whole.raster_pending();
        let mut target = base.clone();
        whole.composite(&base, &mut target, None, false, true);
        for y in 20..45 {
            for x in 48..82 {
                let a = incremental.rgba(x, y);
                let b = target.rgba(x, y);
                for ch in 0..4 {
                    assert!((a[ch] - b[ch]).abs() < 1e-5, "({x},{y}) {a:?} != {b:?}");
                }
            }
        }
        let sf = PixelFormat::new(ColorMode::Grayscale, SampleType::F32, false);
        let mut selection = Surface::new(sf);
        selection.write_pixel(64, 32, &[1.0]);
        let mut clipped = base.clone();
        whole.composite(&base, &mut clipped, Some(&selection), false, true);
        assert_eq!(clipped.rgba(63, 32), base.rgba(63, 32));
        assert_eq!(clipped.rgba(65, 32), base.rgba(65, 32));
        assert_ne!(clipped.rgba(64, 32), base.rgba(64, 32));
    }

    #[test]
    fn pigment_bilinear_sampling_interpolates_across_negative_tile_boundary() {
        let mut map = CoverageMap::new(0);
        map.accumulate(Rect::new(-1, 0, 1, 1), &[1.0, 1.0], 1.0, None, None);
        for ((tx, _), tile) in &mut map.tiles {
            tile.pigment = vec![[if *tx < 0 { 0.0 } else { 1.0 }; 4]; tile.cov.len()];
        }
        assert_eq!(map.pigment_sample(-0.25, 0.0), [0.75; 4]);
        assert_eq!(map.pigment_sample(-0.75, 0.0), [0.25; 4]);
    }

    #[test]
    fn dual_brush_opacity_controls_mask_strength() {
        let brush = BrushSettings { dual_brush: crate::DualBrush { opacity: 0.25, ..Default::default() }, ..Default::default() };
        let ctx = BrushContext::new(&brush);
        let got = ctx.stroke_mask(0.8, Some(0.2), 0, 0);
        assert!((got - 0.64).abs() < 1e-6);
    }

    #[test]
    fn material_single_filters_each_dab_before_accumulating_color() {
        for style in [WetStyle::Photoshop, WetStyle::Watercolor, WetStyle::Watercolor2] {
            let settings = WetEdgeSettings { style, outline: WetOutline::Single, color_mode: WetColorMode::Hsl, ..Default::default() };
            let brush = BrushSettings { hardness: 0.2, wet_edges: true, wet_edge: Some(settings.clone()), ..Default::default() };
            let fmt = PixelFormat::new(ColorMode::Rgb, SampleType::F32, true);
            let base = Surface::with_default(fmt, &[0.8, 0.7, 0.6, 1.0]);
            let mut renderer = StrokeRenderer::on_surface(&brush, &base, 1.0);
            let mut a = Dab::round(Point::new(32.0, 32.0), 10.0, 0.5);
            a.color = [0.6, 0.3, 0.2, 1.0];
            a.wet_edge = settings.resolve(0.0, 20.0);
            a.wet_edge.strength = 0.0;
            let mut b = a;
            b.radius = 30.0;
            b.color = [0.2, 0.5, 0.4, 1.0];
            b.wet_edge = settings.resolve(1.0, 60.0);
            let rect = Rect::new(41, 32, 42, 33);
            let mut expected = [0.0; 3];
            let mut coverage = 0.0;
            for dab in [a, b] {
                let (mut alpha, mut edge) = (Vec::new(), Vec::new());
                renderer.ctx.rasterize_into(&dab, false, rect, &mut alpha, Some(&mut edge));
                let filtered = settings.color(dab.color, [0.8, 0.7, 0.6, 1.0], edge[0], dab.wet_edge);
                let next = coverage + alpha[0] * (dab.opacity - coverage);
                let weight = next.min(dab.opacity * alpha[0]);
                let retained = (next - weight).max(0.0);
                for ch in 0..3 {
                    expected[ch] = filtered[ch] * weight + if coverage > 0.0001 { expected[ch] / coverage * retained } else { 0.0 };
                }
                coverage = next;
            }
            renderer.dab_buf = vec![a, b];
            renderer.raster_pending();
            let tile = &renderer.cov.tiles[&(0, 0)];
            let offset = (32 * COV_TILE as usize + 41) * 3;
            for (got, want) in tile.col[offset..offset + 3].iter().zip(expected) {
                assert!((*got - want).abs() < 1e-6, "{style:?}: {got} != {want}");
            }
            let mut target = base.clone();
            renderer.composite(&base, &mut target, None, false, true);
            let actual = target.rgba(41, 32);
            for ch in 0..3 {
                let want = expected[ch] + [0.8, 0.7, 0.6][ch] * (1.0 - coverage);
                assert!((actual[ch] - want).abs() < 1e-6, "{style:?} composite channel {ch}");
            }
        }
    }
}
