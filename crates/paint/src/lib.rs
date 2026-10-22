//! Brush engine: Photoshop-grade brushes (computed and sampled tips, Shape Dynamics, Scattering,
//! Texture, Dual Brush, Color Dynamics, Transfer, Brush Pose, Noise, Wet Edges, Build-up,
//! Smoothing) plus the dab machinery shared by the retouching tools.
//!
//! Strokes are data ([`Stroke`]) so they can be recorded, replayed by automation, and tested
//! deterministically: every jitter is a hash of the brush seed and the dab index ([`rng`]).
//! Dabs composite with "build-up up to stroke opacity" semantics: within one stroke, coverage
//! accumulates with flow towards each dab's opacity ceiling, and the stroke composites once at the
//! stroke opacity (Photoshop's Opacity versus Flow).
//!
//! Module map: [`brush`] (the serde model), [`dynamics`] (points → dabs), [`render`] (dabs →
//! stroke buffer → pixels), [`presets`] (built-ins), [`mixer`] (Mixer Brush), [`replace`] (Color
//! Replacement), [`retouch`] (sequential/accumulating helpers for the retouch tools).
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use photocraft_geom::{Point, Rect};
use photocraft_raster::Surface;
use serde::{Deserialize, Serialize};

pub mod attributes;
pub mod bg_erase;
pub mod brush;
pub mod canvas_material;
pub mod curve;
pub mod dynamics;
pub mod material;
pub mod mixer;
pub mod motion;
pub mod outline;
pub mod pigment;
pub mod pigment_material;
pub mod presets;
pub mod procedural;
pub mod render;
pub mod replace;
pub mod retouch;
pub mod rng;
pub mod symmetry;
pub mod tablet_pressure;
pub mod tile;
pub mod transport;

pub use attributes::AttributeMode;
pub use brush::{
    BrushPreset, BrushSettings, ColorDynamics, Control, DualBrush, Dynamic, MAX_BRUSH_SIZE, MAX_SCATTER, MaskMode, Pattern, PatternStyle, Pose, PresetCaptures,
    Scattering, SectionLocks, ShapeDynamics, Smoothing, Texture, TipShape, Transfer,
};
pub use brush::{ColorAttributes, ColorPattern};
pub use curve::ResponseCurve;
pub use mixer::MixerSettings;
pub use render::{BrushContext, StrokeRenderer, UnionLive, grid_center, grid_square, render_stroke};
pub use tablet_pressure::{TabletPressureFilter, TabletPressureSettings};
pub use tile::{GrayTile, StoredTile};

/// One input sample. Missing stylus data defaults to "mouse": full pressure, no tilt/rotation.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StrokePoint {
    pub is_pen: bool,
    pub x: f64,
    pub y: f64,
    /// 0..1, 1 for mouse.
    pub pressure: f32,
    /// Degrees -90..90 (W3C Pointer Events `tiltX`/`tiltY`).
    pub tilt_x: f32,
    pub tilt_y: f32,
    /// Barrel rotation in degrees 0..360.
    pub rotation: f32,
    /// Airbrush stylus wheel 0..1.
    pub wheel: f32,
    /// Timestamp in milliseconds (airbrush build-up).
    pub time: f64,
}

impl Default for StrokePoint {
    fn default() -> Self {
        Self { is_pen: false, x: 0.0, y: 0.0, pressure: 1.0, tilt_x: 0.0, tilt_y: 0.0, rotation: 0.0, wheel: 1.0, time: 0.0 }
    }
}

impl StrokePoint {
    pub fn new(x: f64, y: f64, pressure: f32) -> Self {
        Self { x, y, pressure, is_pen: pressure != 1.0, ..Default::default() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub brush: BrushSettings,
    pub points: Vec<StrokePoint>,
}

/// A dab placed along the stroke (after dynamics).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dab {
    pub hardness: f32,
    pub pigment: crate::pigment_material::ResolvedPigment,
    pub mix_strength: f32,
    pub mix_length: f32,
    pub blur_strength: f32,
    pub center: Point,
    pub radius: f32,
    /// Flow: per-dab build-up amount.
    pub alpha: f32,
    /// Radians; clockwise for C3 tips, counter-clockwise for legacy tips.
    pub angle: f32,
    pub roundness: f32,
    pub flip_x: bool,
    pub flip_y: bool,
    /// Opacity ceiling this dab builds up to (Transfer › Opacity).
    pub opacity: f32,
    /// Straight RGBA paint colour (Color Dynamics).
    pub color: [f32; 4],
    /// Texture depth for per-tip texturing.
    pub depth: f32,
    /// Position in the stroke (0 = first).
    pub index: u64,
    /// Brush Projection: direction (radians, counter-clockwise, y up) along which the tip is
    /// foreshortened, and the factor (1 = no projection).
    pub proj_angle: f32,
    pub proj_scale: f32,
    /// Mixer Brush multipliers of Wet and Mix (Transfer › Wetness/Mix Jitter), 1 = as set.
    pub wet: f32,
    pub mix: f32,
    pub grain_density: f32,
    pub wet_edge: material::ResolvedWet,
}

impl Dab {
    /// A plain round dab.
    pub fn round(center: Point, radius: f32, alpha: f32) -> Self {
        Self {
            hardness: 1.0,
            pigment: crate::pigment_material::ResolvedPigment::default(),
            mix_strength: 0.0,
            mix_length: 0.5,
            blur_strength: 0.0,
            center,
            radius,
            alpha,
            angle: 0.0,
            roundness: 1.0,
            flip_x: false,
            flip_y: false,
            opacity: 1.0,
            color: [0.0, 0.0, 0.0, 1.0],
            depth: 1.0,
            index: 0,
            proj_angle: 0.0,
            proj_scale: 1.0,
            wet: 1.0,
            mix: 1.0,
            grain_density: 0.0,
            wet_edge: material::ResolvedWet::default(),
        }
    }
}

/// All primary dabs of a stroke (smoothing, pose, spacing, build-up and dynamics applied).
pub fn dabs(stroke: &Stroke) -> Vec<Dab> {
    let mut g = dynamics::DabGenerator::new(&stroke.brush, 1.0);
    let (mut out, mut dual) = (Vec::new(), Vec::new());
    g.push(&stroke.points, &mut out, &mut dual);
    g.finish(&mut out, &mut dual);
    out
}

/// Photoshop's round tip, measured (Photoshop 27.11, single dabs at Spacing 1000 %, hardness 0–100
/// at 40 px and checked at 100, 9 and 3 px): a solid core of radius `r0 · R`, then a Gaussian
/// shoulder `exp(-((d − r0) / s)²)` of width `s · R`, both by hardness (0, 0.1, … 1). A soft tip
/// reaches well past its nominal radius (hardness 0 is a Gaussian of σ ≈ 0.47 R: 8 % at 1.05 R).
const TIP_SHAPE: [(f32, f32); 11] =
    [(0.00, 0.664), (0.14, 0.58), (0.28, 0.50), (0.40, 0.44), (0.52, 0.36), (0.63, 0.29), (0.70, 0.24), (0.78, 0.19), (0.84, 0.15), (0.89, 0.11), (1.00, 0.0)];

/// Photoshop's anti-aliased edge: the shoulder never narrows below this many pixels, and the core
/// stays this far inside the radius (a hardness-100 tip is a disc with a ~1 px soft edge at any size).
const TIP_EDGE_PX: f32 = 1.2;

/// The core radius and shoulder width (both in pixels) of a round tip of `radius` and `hardness`.
#[inline]
pub fn tip_shape(radius: f32, hardness: f32) -> (f32, f32) {
    let h = if hardness.is_nan() { 0.0 } else { hardness.clamp(0.0, 1.0) } * 10.0;
    let i = (h.floor() as usize).min(9);
    let t = h - i as f32;
    let (a, b) = (TIP_SHAPE[i], TIP_SHAPE[i + 1]);
    let (r0, s) = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
    let radius = if radius.is_finite() { radius.max(0.0) } else { 0.0 };
    // A tip of a few pixels keeps a narrower edge, so a small hard dab stays solid in the middle.
    // Never sharper than a quarter pixel, the small-tip supersampling step.
    let edge = TIP_EDGE_PX.min(0.3 * radius).max(0.25);
    ((r0 * radius).min(radius - edge).max(0.0), (s * radius).max(edge).max(1e-3))
}

/// `exp(-u)` as `(1 − u/64)⁶⁴` by six squarings: within 1/255 of `exp` where it matters, exact
/// arithmetic (the same on every platform) and exactly 0 from `u = 64`.
#[inline]
fn soft_exp(u: f32) -> f32 {
    let mut g = (1.0 - u / 64.0).clamp(0.0, 1.0);
    for _ in 0..6 {
        g *= g;
    }
    g
}

/// Opacity of a round tip at distance `d` from its centre, edge included (see [`tip_shape`]).
#[inline]
pub fn tip_falloff(d: f32, radius: f32, hardness: f32) -> f32 {
    let (r0, s) = tip_shape(radius, hardness);
    if d <= r0 {
        return 1.0;
    }
    let x = (d - r0) / s;
    soft_exp(x * x)
}

/// How far from its centre a round tip still paints (where [`tip_falloff`] drops below ~0.1 %).
#[inline]
pub fn tip_reach(radius: f32, hardness: f32) -> f32 {
    let (r0, s) = tip_shape(radius, hardness);
    r0 + 2.6 * s
}

/// Distance from the centre where [`tip_falloff`] drops to 50 %: the contour Preferences ›
/// Cursors › Normal Brush Tip outlines. `(1 − u/64)⁶⁴ = ½` at `u = 64 (1 − ½^(1/64))`.
#[inline]
pub fn half_coverage_radius(radius: f32, hardness: f32) -> f32 {
    let (r0, s) = tip_shape(radius, hardness);
    let u = 64.0 * (1.0 - 0.5f32.powf(1.0 / 64.0));
    r0 + s * u.sqrt()
}

/// Coverage of a round dab at distance `d` from its centre.
#[inline]
pub fn dab_coverage(d: f32, radius: f32, hardness: f32) -> f32 {
    // NaN distances paint nothing.
    if d.is_nan() || d >= tip_reach(radius, hardness) + 0.5 {
        return 0.0;
    }
    tip_falloff(d, radius, hardness)
}

/// Rasterize a stroke onto `target`, optionally limited by a selection (grayscale coverage surface).
/// Returns the damaged rectangle.
pub fn apply_stroke(target: &mut Surface, stroke: &Stroke, selection: Option<&Surface>, lock_transparency: bool) -> Rect {
    render_stroke(target, &stroke.brush, &stroke.points, selection, lock_transparency, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_color::PixelFormat;

    fn stroke(points: &[(f64, f64)], size: f32) -> Stroke {
        Stroke {
            brush: BrushSettings { size, pressure_size: false, ..Default::default() },
            points: points.iter().map(|&(x, y)| StrokePoint::new(x, y, 1.0)).collect(),
        }
    }

    #[test]
    fn dab_spacing() {
        let s = stroke(&[(0.0, 0.0), (100.0, 0.0)], 10.0);
        let d = dabs(&s);
        // spacing 0.1 * 10px = 1px → ~101 dabs
        assert!((95..=105).contains(&d.len()), "{}", d.len());
        let s = Stroke { brush: BrushSettings { spacing: 1.0, pressure_size: false, size: 10.0, ..Default::default() }, ..s };
        assert_eq!(dabs(&s).len(), 11);
    }

    #[test]
    fn coverage_profile() {
        assert_eq!(dab_coverage(0.0, 10.0, 1.0), 1.0);
        assert_eq!(dab_coverage(20.0, 10.0, 1.0), 0.0);
        let soft_mid = dab_coverage(5.0, 10.0, 0.0);
        assert!(soft_mid > 0.2 && soft_mid < 0.8);
        // A hard tip is opaque to just inside its radius and gone 1 px outside it.
        assert!(dab_coverage(8.5, 10.0, 1.0) > 0.99 && dab_coverage(9.5, 10.0, 1.0) > 0.5);
        assert!(dab_coverage(11.0, 10.0, 1.0) < 0.05);
    }

    #[test]
    fn hard_brush_paints_solid_line() {
        let mut s = Surface::new(PixelFormat::RGBA8);
        let dmg = apply_stroke(&mut s, &stroke(&[(10.0, 10.0), (90.0, 10.0)], 8.0), None, false);
        assert!(dmg.contains(50, 10));
        assert_eq!(s.pixel(50, 10), vec![0.0, 0.0, 0.0, 1.0]);
        assert_eq!(s.pixel(50, 30)[3], 0.0);
    }

    #[test]
    fn pigment_stroke_mixes_overlap_and_keeps_coverage() {
        let mut base = Surface::new(PixelFormat::RGBA8);
        base.fill_rect(Rect::new(0, 0, 32, 32), &[1.0, 1.0, 0.0, 1.0]);
        let mut st = stroke(&[(8.0, 16.0), (24.0, 16.0)], 8.0);
        st.brush.color = [0.0, 0.0, 1.0, 1.0];
        st.brush.opacity = 0.5;
        let pre = base.clone();
        let mut normal = base.clone();
        apply_stroke(&mut normal, &st, None, false);
        st.brush.pigment_mix_strength = 1.0;
        apply_stroke(&mut base, &st, None, false);
        let mut live = pre.clone();
        let mut renderer = crate::render::StrokeRenderer::on_surface(&st.brush, &pre, 1.0);
        for point in &st.points {
            renderer.push(&[*point]);
            renderer.composite(&pre, &mut live, None, false, false);
        }
        renderer.finish();
        renderer.composite(&pre, &mut live, None, false, false);
        assert_eq!(base.read_region(Rect::new(0, 0, 32, 32)), live.read_region(Rect::new(0, 0, 32, 32)));
        let mixed = base.pixel(16, 16);
        assert!(mixed[1] > mixed[0] && mixed[1] > mixed[2], "{mixed:?}");
        assert_ne!(mixed, normal.pixel(16, 16));
        assert_eq!(mixed[3], normal.pixel(16, 16)[3]);
        let mut empty = Surface::new(PixelFormat::RGBA8);
        apply_stroke(&mut empty, &st, None, false);
        assert!((empty.pixel(16, 16)[3] - 0.5).abs() < 0.02);
        let mut transparent = pre.clone();
        st.brush.color[3] = 0.0;
        apply_stroke(&mut transparent, &st, None, false);
        assert_eq!(transparent.pixel(16, 16), pre.pixel(16, 16));
        st.brush.color[3] = 0.5;
        let mut half_alpha = pre.clone();
        apply_stroke(&mut half_alpha, &st, None, false);
        st.brush.color[3] = 1.0;
        st.brush.opacity *= 0.5;
        let mut half_opacity = pre.clone();
        apply_stroke(&mut half_opacity, &st, None, false);
        assert_eq!(half_alpha.pixel(16, 16), half_opacity.pixel(16, 16));
    }

    #[test]
    fn opacity_caps_overlap() {
        let mut s = Surface::new(PixelFormat::RGBA8);
        let mut st = stroke(&[(10.0, 10.0), (60.0, 10.0), (10.0, 10.0)], 10.0);
        st.brush.opacity = 0.5;
        apply_stroke(&mut s, &st, None, false);
        let a = s.pixel(30, 10)[3];
        assert!((a - 0.5).abs() < 0.02, "{a}");
    }

    #[test]
    fn selection_limits_paint() {
        let mut s = Surface::new(PixelFormat::RGBA8);
        let mut sel = Surface::new(PixelFormat::GRAY8);
        sel.fill_rect(Rect::new(0, 0, 50, 100), &[1.0]);
        apply_stroke(&mut s, &stroke(&[(10.0, 10.0), (90.0, 10.0)], 8.0), Some(&sel), false);
        assert_eq!(s.pixel(20, 10)[3], 1.0);
        assert_eq!(s.pixel(70, 10)[3], 0.0);
    }

    #[test]
    fn eraser_removes_alpha() {
        let mut s = Surface::new(PixelFormat::RGBA8);
        s.fill_rect(Rect::new(0, 0, 100, 20), &[1.0, 0.0, 0.0, 1.0]);
        let mut st = stroke(&[(10.0, 10.0), (90.0, 10.0)], 8.0);
        st.brush.erase = true;
        apply_stroke(&mut s, &st, None, false);
        assert_eq!(s.pixel(50, 10)[3], 0.0);
        assert_eq!(s.pixel(50, 1)[3], 1.0);
    }

    #[test]
    fn lock_transparency_preserves_alpha() {
        let mut s = Surface::new(PixelFormat::RGBA8);
        s.fill_rect(Rect::new(0, 0, 50, 20), &[1.0, 1.0, 1.0, 1.0]);
        apply_stroke(&mut s, &stroke(&[(10.0, 10.0), (90.0, 10.0)], 8.0), None, true);
        assert_eq!(s.pixel(20, 10), vec![0.0, 0.0, 0.0, 1.0]);
        assert_eq!(s.pixel(70, 10)[3], 0.0);
    }

    #[test]
    fn pressure_changes_size() {
        let mut st = stroke(&[(0.0, 0.0), (100.0, 0.0)], 20.0);
        st.brush.pressure_size = true;
        st.points[0].pressure = 0.1;
        st.points[1].pressure = 1.0;
        let d = dabs(&st);
        assert!(d.first().unwrap().radius < d.last().unwrap().radius);
    }

    #[test]
    fn works_at_all_depths() {
        for f in [PixelFormat::RGBA8, PixelFormat::RGBA16, PixelFormat::RGBA32F, PixelFormat::GRAYA8, PixelFormat::CMYKA8] {
            let mut s = Surface::new(f);
            let mut st = stroke(&[(5.0, 5.0), (40.0, 5.0)], 6.0);
            st.brush.color = [1.0, 1.0, 1.0, 1.0];
            apply_stroke(&mut s, &st, None, false);
            let rgba = s.rgba(20, 5);
            assert!(rgba[3] > 0.99 && rgba[0] > 0.98, "{f:?} {rgba:?}");
        }
    }
}

#[cfg(test)]
mod brush_tests;
