//! The brush model: tip shape plus Photoshop-style dynamics sections, all plain serde data.
//!
//! Every section is optional (an `enabled` flag, or a neutral default), so a partial JSON object
//! such as `{"size": 40, "scattering": {"enabled": true, "scatter": {"jitter": 2.0}}}` deserialises
//! onto the defaults. Field names are camelCase in JSON, matching the command parameters.
//!
//! Units: sizes are pixels, angles are degrees, every percentage is a 0..1 fraction (scatter and
//! texture scale may exceed 1, like Photoshop's 1000 % scatter).

use photocraft_color::BlendMode;
use serde::{Deserialize, Serialize};

use crate::curve::ResponseCurve;
use crate::material::{GrainSettings, WetEdgeSettings};
use crate::mixer::MixerSettings;
use crate::tile::{GrayTile, StoredTile};

/// Largest brush diameter accepted by the rasterizer and brush controls.
pub const MAX_BRUSH_SIZE: f32 = 5000.0;

/// Largest Scatter amount (primary and Dual Brush): 10 = 1000 %, the Brush Settings slider's top.
/// Beyond it a dab could land arbitrarily far away, and an unbounded amount overflowed the dab
/// centre to infinity (#977).
pub const MAX_SCATTER: f32 = 10.0;
pub const MAX_C3_SCATTER: f32 = 50.0;

/// What drives a dynamic parameter (Photoshop's "Control" pop-ups).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Control {
    #[default]
    Off,
    /// Ramp over `fadeSteps` spacing steps (from full to the minimum).
    Fade,
    /// C3 fade over max(96, size × max(0.05, spacing) × 32) pixels.
    DistanceFade,
    PenPressure,
    PenPressureInverse,
    /// Smoothed travel speed relative to the brush diameter.
    Speed,
    /// Latched while pen pressure is falling.
    PressureRelease,
    PenTilt,
    StylusWheel,
    /// Barrel rotation of the stylus.
    Rotation,
    /// Angle only: the direction of the first stroke segment.
    InitialDirection,
    /// Angle only: the current stroke direction.
    Direction,
}

pub const REFERENCE_BASE_CONTROLS: &[Control] = &[Control::Off, Control::PenPressure, Control::Direction, Control::PenTilt, Control::Rotation];
pub const REFERENCE_SHAPE_CONTROLS: &[Control] =
    &[Control::Off, Control::DistanceFade, Control::PenPressure, Control::Direction, Control::PenTilt, Control::Rotation];
pub const REFERENCE_SIZE_CONTROLS: &[Control] =
    &[Control::Off, Control::DistanceFade, Control::PenPressure, Control::Direction, Control::PenTilt, Control::Rotation, Control::InitialDirection];
pub const REFERENCE_SPACING_CONTROLS: &[Control] =
    &[Control::Off, Control::DistanceFade, Control::PenPressure, Control::PenPressureInverse, Control::Direction, Control::PenTilt, Control::Rotation];
pub const REFERENCE_TEXTURE_CONTROLS: &[Control] = &[
    Control::Off,
    Control::Fade,
    Control::PenPressure,
    Control::Direction,
    Control::PenTilt,
    Control::StylusWheel,
    Control::Rotation,
    Control::InitialDirection,
];
pub const REFERENCE_PIGMENT_CONTROLS: &[Control] = &[Control::Off, Control::PenPressure, Control::PenPressureInverse, Control::PenTilt];

fn reference_control(control: &mut Control, allowed: &[Control]) {
    if matches!(*control, Control::Fade | Control::DistanceFade) {
        if allowed.contains(&Control::DistanceFade) {
            *control = Control::DistanceFade;
        } else if allowed.contains(&Control::Fade) {
            *control = Control::Fade;
        }
    }
    if !allowed.contains(control) {
        *control = Control::Off;
    }
}

/// One dynamic parameter: random jitter plus a controller with a floor.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Dynamic {
    /// 0..1 random variation per dab (for scatter: the amount, may exceed 1).
    pub jitter: f32,
    pub control: Control,
    /// Steps for [`Control::Fade`].
    pub fade_steps: u32,
    /// 0..1 floor the control/jitter may reduce the value to (minimum diameter, roundness, depth…).
    pub minimum: f32,
    /// This dynamic's own response to its control (Fade, Pen Pressure, Pen Tilt, Stylus Wheel,
    /// Rotation), applied after the global pen curve. Linear by default. Always written to JSON, so
    /// a `tools.setBrush` patch can set it back to linear. Angle Jitter's controls ignore it.
    pub curve: ResponseCurve,
}

impl Default for Dynamic {
    fn default() -> Self {
        Self { jitter: 0.0, control: Control::Off, fade_steps: 25, minimum: 0.0, curve: ResponseCurve::LINEAR }
    }
}

impl Dynamic {
    pub const fn controlled(control: Control) -> Self {
        Self { jitter: 0.0, control, fade_steps: 25, minimum: 0.0, curve: ResponseCurve::LINEAR }
    }
    pub const fn jitter(jitter: f32) -> Self {
        Self { jitter, control: Control::Off, fade_steps: 25, minimum: 0.0, curve: ResponseCurve::LINEAR }
    }
    pub fn is_active(&self) -> bool {
        self.jitter > 0.0 || self.control != Control::Off
    }
}

/// The brush tip bitmap source.
///
/// A loaded [`TipShape::Stored`] tip serialises as `sampled` (its full bitmap), so tool presets,
/// recorded actions and `brush.get` see the same JSON as for an embedded tip.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TipShape {
    /// Computed round/elliptical tip (hardness, angle, roundness).
    #[default]
    Round,
    /// Sampled tip: grayscale bitmap where 1 = full paint. Scaled so its larger side equals `size`.
    Sampled(GrayTile),
    /// A sampled tip its owner (the preset store) keeps outside the settings and loads on demand.
    Stored(StoredTile),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum TipShapeOut<'a> {
    Round,
    Sampled(&'a GrayTile),
    Stored(&'a StoredTile),
}

impl Serialize for TipShape {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            TipShape::Round => TipShapeOut::Round,
            TipShape::Sampled(g) => TipShapeOut::Sampled(g),
            TipShape::Stored(r) => match r.full.as_deref() {
                Some(g) => TipShapeOut::Sampled(g),
                None => TipShapeOut::Stored(r),
            },
        }
        .serialize(s)
    }
}

impl TipShape {
    /// The bitmap a sampled tip paints with (a stored tip's full bitmap, or its preview while it
    /// isn't loaded); `None` for a computed tip.
    pub fn bitmap(&self) -> Option<&GrayTile> {
        match self {
            TipShape::Round => None,
            TipShape::Sampled(g) => Some(g),
            TipShape::Stored(r) => Some(r.bitmap()),
        }
    }
    /// The tip's real pixel size (a stored tip's, not its preview's); `None` for a computed tip.
    pub fn bitmap_size(&self) -> Option<(u32, u32)> {
        match self {
            TipShape::Round => None,
            TipShape::Sampled(g) => Some((g.width, g.height)),
            TipShape::Stored(r) => Some((r.width, r.height)),
        }
    }
}

/// Shape Dynamics.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ShapeDynamics {
    pub enabled: bool,
    /// Size jitter / control; `minimum` is Photoshop's Minimum Diameter.
    pub size: Dynamic,
    /// Tilt Scale, 0..2 (0–200 %): with the size control on Pen Tilt, how much a tilted pen
    /// squashes the tip's height (before the tip angle is applied). At full tilt the height is
    /// scaled by `1 − tiltScale / 2`, so 0 leaves the tip alone and 200 % flattens it.
    pub tilt_scale: f32,
    /// Angle jitter (fraction of ±180°) / control.
    pub angle: Dynamic,
    /// Roundness jitter / control; `minimum` is Minimum Roundness.
    pub roundness: Dynamic,
    /// Continuous perpendicular path wobble; jitter is a fraction of the diameter.
    pub stroke_jitter: Dynamic,
    pub flip_x_jitter: bool,
    pub flip_y_jitter: bool,
    /// Brush Projection: the pen's tilt and barrel rotation are applied to the tip as a
    /// projection (the tip is foreshortened along the tilt direction and turns with the pen).
    pub brush_projection: bool,
}

/// Scattering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Scattering {
    pub enabled: bool,
    /// `jitter` = scatter amount (1.0 = 100 %: up to one radius of displacement); control scales it.
    pub scatter: Dynamic,
    pub range_jitter: f32,
    pub spacing_jitter: Dynamic,
    /// Scatter radially instead of only perpendicular to the stroke.
    pub both_axes: bool,
    /// Dabs per spacing step (1..16).
    pub count: u32,
    /// C3 keeps this absolute minimum when the base count changes.
    pub minimum_count: Option<f32>,
    /// Count jitter / control (reduces the count towards 1).
    pub count_jitter: Dynamic,
}

impl Default for Scattering {
    fn default() -> Self {
        Self {
            enabled: false,
            scatter: Dynamic::default(),
            range_jitter: 0.0,
            spacing_jitter: Dynamic::default(),
            both_axes: false,
            count: 1,
            minimum_count: None,
            count_jitter: Dynamic::default(),
        }
    }
}

/// How a grayscale mask (texture or dual tip) combines with the brush coverage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MaskMode {
    Normal,
    #[default]
    Multiply,
    Subtract,
    Darken,
    Overlay,
    ColorDodge,
    ColorBurn,
    LinearBurn,
    HardMix,
    LinearHeight,
    Height,
}

/// Procedural pattern styles (generated deterministically; no bundled pattern files).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PatternStyle {
    /// Smooth value noise.
    #[default]
    Noise,
    /// Woven canvas.
    Canvas,
    /// Fibrous paper tooth.
    Paper,
    /// Halftone-like dots.
    Dots,
}

/// A texture pattern: a procedural generator or an explicit grayscale tile (tiled infinitely).
/// A loaded [`Pattern::Stored`] tile serialises as `tile`, like [`TipShape::Stored`].
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Pattern {
    Procedural {
        style: PatternStyle,
        size: u32,
        seed: u32,
    },
    Tile(GrayTile),
    /// A tile its owner (the preset store) keeps outside the settings and loads on demand.
    Stored(StoredTile),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum PatternOut<'a> {
    Procedural { style: PatternStyle, size: u32, seed: u32 },
    Tile(&'a GrayTile),
    Stored(&'a StoredTile),
}

impl Serialize for Pattern {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Pattern::Procedural { style, size, seed } => PatternOut::Procedural { style: *style, size: *size, seed: *seed },
            Pattern::Tile(g) => PatternOut::Tile(g),
            Pattern::Stored(r) => match r.full.as_deref() {
                Some(g) => PatternOut::Tile(g),
                None => PatternOut::Stored(r),
            },
        }
        .serialize(s)
    }
}

impl Pattern {
    /// The tile a bitmap pattern textures with (a stored tile's full bitmap, or its preview while
    /// it isn't loaded); `None` for a procedural pattern.
    pub fn bitmap(&self) -> Option<&GrayTile> {
        match self {
            Pattern::Procedural { .. } => None,
            Pattern::Tile(g) => Some(g),
            Pattern::Stored(r) => Some(r.bitmap()),
        }
    }
}

impl Default for Pattern {
    fn default() -> Self {
        Pattern::Procedural { style: PatternStyle::Paper, size: 128, seed: 1 }
    }
}

/// Coordinates for a C3 texture: document, stroke origin or the transformed tip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TextureCoordinateMode {
    #[default]
    CanvasLocked,
    StrokeLocked,
    TipLocal,
}

/// Texture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Texture {
    /// C3 texture math; false retains imported ABR texture semantics.
    pub c3_math: bool,
    pub enabled: bool,
    pub coordinate_mode: TextureCoordinateMode,
    pub pattern: Pattern,
    pub invert: bool,
    /// Pattern scale (1 = 100 %).
    pub scale: f32,
    /// -1..1 (Photoshop's -150..150 mapped to ±1).
    pub brightness: f32,
    /// -1..1: Photoshop's -50..0 mapped to -1..0 and its 0..100 to 0..1.
    pub contrast: f32,
    /// Apply the texture to every dab (enables depth jitter) instead of once to the stroke.
    pub each_tip: bool,
    pub mode: MaskMode,
    /// 0..1.
    pub depth: f32,
    /// Depth jitter / control (per tip only); `minimum` is Minimum Depth.
    pub depth_jitter: Dynamic,
}

impl Texture {
    pub fn combine(&self, coverage: f32, pattern: f32, depth: f32) -> f32 {
        if self.c3_math {
            crate::render::mask_combine(self.mode, coverage, pattern, depth)
        } else {
            crate::render::texture_combine(self.mode, coverage, pattern, depth)
        }
    }
    /// A pattern value (0..1) after Brightness, Contrast and Invert: the value the mode combines
    /// with the brush. Measured against Photoshop (#3340): Brightness shifts the pattern by
    /// `brightness / 255` (±150 moves it 150 levels of 255, not all the way to white or black);
    /// positive Contrast steepens it around mid-grey by `1 / (1 − c/100)` after the shift (100 is
    /// a threshold), negative Contrast flattens it by `1 + c/100` before the shift; Invert is last.
    pub fn level(&self, v: f32) -> f32 {
        if self.c3_math {
            let x = ((v - 0.5) * (1.0 + self.contrast.clamp(-1.0, 1.0)) + 0.5 + self.brightness.clamp(-1.0, 1.0)).clamp(0.0, 1.0);
            return if self.invert { 1.0 - x } else { x };
        }
        let b = if self.brightness.is_finite() { self.brightness.clamp(-1.0, 1.0) * (150.0 / 255.0) } else { 0.0 };
        let c = if self.contrast.is_finite() { self.contrast.clamp(-1.0, 1.0) } else { 0.0 };
        let x = if c >= 0.0 { (v + b - 0.5) / (1.0 - c).max(1e-4) + 0.5 } else { (v - 0.5) * (1.0 + c / 2.0) + 0.5 + b };
        let x = if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.5 };
        if self.invert { 1.0 - x } else { x }
    }
}

impl Default for Texture {
    fn default() -> Self {
        Self {
            enabled: false,
            c3_math: false,
            coordinate_mode: TextureCoordinateMode::CanvasLocked,
            pattern: Pattern::default(),
            invert: false,
            scale: 1.0,
            brightness: 0.0,
            contrast: 0.0,
            each_tip: false,
            mode: MaskMode::Multiply,
            depth: 1.0,
            depth_jitter: Dynamic::default(),
        }
    }
}

/// Dual Brush: a second tip stroked along the same path; the stroke is masked by it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DualBrush {
    pub full_parameters: Option<Box<BrushSettings>>,
    pub primary_strength: f32,
    pub secondary_strength: f32,
    pub grain: Option<GrainSettings>,
    pub wet_edges: bool,
    pub wet_edge: Option<WetEdgeSettings>,
    pub enabled: bool,
    pub mode: MaskMode,
    pub tip: TipShape,
    pub size: f32,
    /// Maximum contribution of the secondary brush to the primary coverage.
    pub opacity: f32,
    /// Per-dab contribution of the secondary brush; repeated dabs build up to `opacity`.
    pub flow: f32,
    pub hardness: f32,
    pub hardness_dynamic: Dynamic,
    pub roundness: f32,
    pub angle: f32,
    pub spacing: f32,
    pub scatter: f32,
    pub both_axes: bool,
    pub count: u32,
    pub flip: bool,
}

impl Default for DualBrush {
    fn default() -> Self {
        Self {
            full_parameters: None,
            primary_strength: 1.0,
            secondary_strength: 0.0,
            grain: None,
            wet_edges: false,
            wet_edge: None,
            enabled: false,
            mode: MaskMode::Multiply,
            tip: TipShape::Round,
            size: 20.0,
            opacity: 1.0,
            flow: 1.0,
            hardness: 1.0,
            hardness_dynamic: Dynamic::default(),
            roundness: 1.0,
            angle: 0.0,
            spacing: 0.25,
            scatter: 0.0,
            both_axes: false,
            count: 1,
            flip: false,
        }
    }
}

/// Color Dynamics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ColorDynamics {
    pub enabled: bool,
    /// Vary every dab (true) or once per stroke (false).
    pub per_tip: bool,
    /// Foreground/background mix jitter / control.
    pub fg_bg: Dynamic,
    /// 0..1 fraction of ±half the hue wheel.
    pub hue_jitter: f32,
    pub hue_control: Control,
    pub hue_minimum: f32,
    pub saturation_jitter: f32,
    pub saturation_control: Control,
    pub saturation_minimum: f32,
    pub brightness_jitter: f32,
    pub brightness_control: Control,
    pub brightness_minimum: f32,
    /// -1 (fully desaturated) .. 1 (fully saturated).
    pub purity: f32,
    pub purity_control: Control,
    pub purity_minimum: f32,
}

impl Default for ColorDynamics {
    fn default() -> Self {
        Self {
            enabled: false,
            per_tip: true,
            fg_bg: Dynamic::default(),
            hue_jitter: 0.0,
            hue_control: Control::Off,
            hue_minimum: 0.0,
            saturation_jitter: 0.0,
            saturation_control: Control::Off,
            saturation_minimum: 0.0,
            brightness_jitter: 0.0,
            brightness_control: Control::Off,
            brightness_minimum: 0.0,
            purity: 0.0,
            purity_control: Control::Off,
            purity_minimum: 0.0,
        }
    }
}

/// Transfer: per-dab opacity ceiling and flow, plus the Mixer Brush's wetness and mix.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Transfer {
    pub enabled: bool,
    pub opacity: Dynamic,
    pub flow: Dynamic,
    /// Wetness Jitter / control (Mixer Brush only): scales the Wet setting per dab.
    pub wetness: Dynamic,
    /// Mix Jitter / control (Mixer Brush only): scales the Mix setting per dab.
    pub mix: Dynamic,
}

/// The sections a lock can hold (the lock icons in Photoshop's Brush Settings list): a locked
/// section keeps its settings when another brush preset is picked.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SectionLocks {
    pub shape_dynamics: bool,
    pub scattering: bool,
    pub texture: bool,
    pub dual_brush: bool,
    pub color_dynamics: bool,
    pub transfer: bool,
    pub pose: bool,
    pub noise: bool,
    pub wet_edges: bool,
    pub build_up: bool,
    pub smoothing: bool,
    pub protect_texture: bool,
}

/// Brush Pose: stylus values used instead of (override) or for missing device data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Pose {
    pub enabled: bool,
    /// Degrees, -90..90 (W3C Pointer Events convention).
    pub tilt_x: f32,
    pub tilt_y: f32,
    /// Degrees 0..360.
    pub rotation: f32,
    /// 0..1.
    pub pressure: f32,
    pub override_tilt: bool,
    pub override_rotation: bool,
    pub override_pressure: bool,
}

impl Default for Pose {
    fn default() -> Self {
        Self {
            enabled: false,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation: 0.0,
            pressure: 1.0,
            override_tilt: false,
            override_rotation: false,
            override_pressure: false,
        }
    }
}

/// Pulled-string controls; legacy exponential settings remain readable in saved brushes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Smoothing {
    /// Switch smoothing off without discarding its amount or mode. Old presets remain enabled.
    pub enabled: bool,
    /// String length, 0..1 of 100 screen pixels.
    pub amount: f32,
    /// Lazy-mouse: paint only when the pointer pulls a string of `amount × 100 px` taut.
    pub pulled_string: bool,
    /// Legacy exponential catch-up option, retained for preset round-trips and ignored.
    pub catch_up: bool,
    /// Finish the stroke at the last pointer position.
    pub catch_up_on_end: bool,
    /// Divide the string length by the view zoom (the command's `zoom` param).
    pub adjust_for_zoom: bool,
}

impl Default for Smoothing {
    fn default() -> Self {
        Self { enabled: true, amount: 0.0, pulled_string: false, catch_up: true, catch_up_on_end: true, adjust_for_zoom: true }
    }
}

/// A complete brush: tip shape, dynamics, and stroke-level options.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BrushSettings {
    /// C3 pigment response, 0 = ordinary blending, 1 = full Kubelka-Munk response.
    pub pigment_mix_strength: f32,
    pub saturation_preserve_output: f32,
    pub saturation_record_strength: f32,
    pub canvas_material: Option<crate::canvas_material::CanvasMaterial>,
    pub c3_preserved_parameters: std::collections::BTreeMap<String, String>,
    pub pigment: Option<crate::pigment_material::PigmentSettings>,
    pub contour: Option<crate::pigment_material::ContourSettings>,
    pub mix_settings: crate::transport::BrushMixSettings,
    pub flow_algorithm_version: u8,
    pub motion: crate::motion::MotionSettings,
    pub inertia: crate::motion::InertiaSettings,
    pub color_attributes: ColorAttributes,
    pub pattern_enabled: bool,
    pub sampled_pattern: Option<ColorPattern>,
    /// Diameter in pixels.
    pub size: f32,
    /// 0 (soft) ..1 (hard). Computed tips only.
    pub hardness: f32,
    pub hardness_dynamic: Dynamic,
    /// Spacing between dabs as a fraction of the diameter.
    pub spacing: f32,
    /// Photoshop's Spacing checkbox. Off: the pointer's speed sets the spacing (one dab every
    /// [`crate::dynamics::SPEED_SPACING_MS`] of stroke time; without timestamps, by distance).
    pub spacing_enabled: bool,
    /// Maximum coverage for the whole stroke.
    pub opacity: f32,
    /// Per-dab coverage.
    pub flow: f32,
    /// Shortcut for Shape Dynamics › Size › Pen Pressure (the options-bar button).
    #[serde(alias = "pressure_size")]
    pub pressure_size: bool,
    /// Shortcut for Transfer › Opacity › Pen Pressure.
    #[serde(alias = "pressure_opacity")]
    pub pressure_opacity: bool,
    /// Shortcut for Transfer › Flow › Pen Pressure.
    #[serde(alias = "pressure_flow")]
    pub pressure_flow: bool,
    /// Straight RGBA colour (display RGB). Commands fill it from the foreground colour.
    pub color: [f32; 4],
    /// The inks `color` was picked as (its sRGB colour, the CMYK values), for a CMYK target: the
    /// stroke paints exactly these, since an sRGB round trip does not give them back (#4344).
    /// Only valid while `color` is that sRGB colour; ignored with Color Dynamics.
    #[serde(skip)]
    pub color_cmyk: Option<([f32; 3], [f32; 4])>,
    /// Background colour for Color Dynamics' foreground/background jitter.
    pub background: [f32; 4],
    /// Erase instead of paint.
    pub erase: bool,
    /// Brush blend mode (Normal, Multiply, Dissolve…).
    pub mode: BlendMode,
    // --- Brush Tip Shape ---
    pub tip: TipShape,
    /// Degrees; clockwise with `c3_tip_math`, counter-clockwise for legacy tips.
    pub angle: f32,
    /// 0..1 (1 = circle).
    pub roundness: f32,
    pub flip_x: bool,
    pub flip_y: bool,
    /// Pencil: no anti-aliasing (coverage is 0 or 1).
    pub aliased: bool,
    pub triangle: bool,
    /// Use C3 Gaussian tip falloff and sampled-tip hardness.
    pub c3_tip_math: bool,
    /// With `aliased`, a square tip: every pixel of the dab's whole-pixel square
    /// ([`crate::grid_square`]), whatever its angle or roundness. The Eraser's Block mode.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub square: bool,
    // --- dynamics sections ---
    pub shape_dynamics: ShapeDynamics,
    pub scattering: Scattering,
    pub texture: Texture,
    pub dual_brush: DualBrush,
    pub color_dynamics: ColorDynamics,
    pub transfer: Transfer,
    pub pose: Pose,
    /// Add grain to the soft parts of the tip.
    pub noise: bool,
    /// None retains the original Noise effect for existing presets.
    pub grain: Option<GrainSettings>,
    /// Watercolour look: lighter interior, darker rims; overlapping dabs don't build up.
    pub wet_edges: bool,
    /// None retains the original Wet Edges curve for existing presets.
    pub wet_edge: Option<WetEdgeSettings>,
    /// Airbrush: keep depositing dabs over time while the pointer lingers.
    pub build_up: bool,
    /// Build-up rate in dabs per second.
    pub build_up_rate: f32,
    pub smoothing: Smoothing,
    /// Keep the current texture (pattern + scale) when switching to another textured preset.
    pub protect_texture: bool,
    /// Sections whose settings stay when another preset is picked (tool state, like Photoshop's locks).
    pub locks: SectionLocks,
    /// Mixer Brush options (Wet, Load, Mix, Flow, Sample All Layers) used by `paint.mixerBrush`.
    pub mixer: MixerSettings,
    /// Random seed for jitters (commands derive it from the stroke when not given), so strokes replay.
    pub seed: u64,
}

impl Default for BrushSettings {
    fn default() -> Self {
        Self {
            pigment_mix_strength: 0.0,
            saturation_preserve_output: 0.0,
            saturation_record_strength: 0.0,
            canvas_material: None,
            c3_preserved_parameters: Default::default(),
            pigment: None,
            contour: None,
            mix_settings: crate::transport::BrushMixSettings::default(),
            flow_algorithm_version: 1,
            motion: crate::motion::MotionSettings::default(),
            inertia: crate::motion::InertiaSettings::default(),
            color_attributes: ColorAttributes::default(),
            pattern_enabled: false,
            sampled_pattern: None,
            size: 20.0,
            hardness: 1.0,
            hardness_dynamic: Dynamic::default(),
            spacing: 0.1,
            spacing_enabled: true,
            opacity: 1.0,
            flow: 1.0,
            pressure_size: true,
            pressure_opacity: false,
            pressure_flow: false,
            color: [0.0, 0.0, 0.0, 1.0],
            color_cmyk: None,
            background: [1.0, 1.0, 1.0, 1.0],
            erase: false,
            mode: BlendMode::Normal,
            tip: TipShape::Round,
            angle: 0.0,
            roundness: 1.0,
            flip_x: false,
            flip_y: false,
            aliased: false,
            triangle: false,
            c3_tip_math: false,
            square: false,
            shape_dynamics: ShapeDynamics::default(),
            scattering: Scattering::default(),
            texture: Texture::default(),
            dual_brush: DualBrush::default(),
            color_dynamics: ColorDynamics::default(),
            transfer: Transfer::default(),
            pose: Pose::default(),
            noise: false,
            grain: None,
            wet_edges: false,
            wet_edge: None,
            build_up: false,
            build_up_rate: 20.0,
            smoothing: Smoothing::default(),
            protect_texture: false,
            locks: SectionLocks::default(),
            mixer: MixerSettings::default(),
            seed: 0,
        }
    }
}

impl BrushSettings {
    /// Resize the primary tip and keep an enabled Dual Brush tip at the same relative size.
    /// An explicit Dual Brush size edit should bypass this and retain the requested diameter.
    ///
    /// As in Photoshop, the dual diameter is multiplied by the size ratio unrounded and only capped
    /// at the top ([`MAX_BRUSH_SIZE`]): a 100 px brush with a 20 px dual tip shrunk to 1 px has a
    /// 0.2 px dual tip and grows back to exactly 20 px. A sub-pixel floor here would lose the
    /// proportion on the way back up (#4809); rendering bounds the diameter itself
    /// ([`Self::bounded_for_render`]).
    pub fn resize_with_dual_brush(&mut self, new_size: f32) {
        if self.dual_brush.enabled && self.size.is_finite() && self.size > 0.0 && new_size.is_finite() && new_size > 0.0 {
            let ratio = new_size / self.size;
            if let Some(secondary) = &mut self.dual_brush.full_parameters {
                let scaled = secondary.size * ratio;
                if scaled > 0.0 {
                    secondary.size = scaled.min(MAX_BRUSH_SIZE);
                }
            }
            let scaled = self.dual_brush.size * ratio;
            // NaN fails the comparison; infinity is capped.
            if scaled > 0.0 {
                self.dual_brush.size = scaled.min(MAX_BRUSH_SIZE);
            }
        }
        self.size = new_size;
    }

    /// Copy the settings with the primary and dual diameters and Scatter amounts constrained for
    /// rasterization.
    ///
    /// Engine commands reject out-of-range values; this is a final guard for direct users of the
    /// infallible paint API, which must not turn malformed brush dimensions into giant allocations,
    /// nor a huge Scatter amount into non-finite dab centres (#977).
    pub fn bounded_for_render(&self) -> Self {
        let safe_size = |size: f32| {
            if size.is_finite() { size.clamp(0.5, MAX_BRUSH_SIZE) } else { 0.5 }
        };
        // NaN scatters nothing (as before: it never passed the `> 0` check); infinity is the maximum.
        let scatter_limit = if self.c3_tip_math { MAX_C3_SCATTER } else { MAX_SCATTER };
        let safe_scatter = |amount: f32| if amount.is_nan() { 0.0 } else { amount.clamp(0.0, scatter_limit) };
        let mut brush = self.clone();
        // Hidden legacy options must not affect strokes loaded from older presets.
        brush.pose.enabled = false;
        brush.canvas_material = None;
        brush.color_dynamics.fg_bg = Dynamic::default();
        brush.saturation_record_strength = 0.0;
        brush.saturation_preserve_output = 0.0;
        brush.flip_x = false;
        brush.flip_y = false;
        brush.shape_dynamics.flip_x_jitter = false;
        brush.shape_dynamics.flip_y_jitter = false;
        brush.shape_dynamics.brush_projection = false;
        brush.contour = None;
        brush.noise = brush.grain.is_some();
        brush.shape_dynamics.tilt_scale = 0.0;
        brush.scattering.both_axes = false;
        brush.dual_brush.both_axes = false;
        brush.transfer.wetness = Dynamic::default();
        brush.transfer.mix = Dynamic::default();
        brush.spacing_enabled = true;
        brush.build_up = false;
        // Pulled string is an explicit Motion control; legacy exponential settings stay inert.
        brush.smoothing.enabled &= brush.smoothing.pulled_string;
        if !brush.erase && !brush.aliased {
            brush.mode = BlendMode::Normal;
        }
        reference_control(&mut brush.dual_brush.hardness_dynamic.control, REFERENCE_SIZE_CONTROLS);
        for control in [
            &mut brush.shape_dynamics.size.control,
            &mut brush.hardness_dynamic.control,
            &mut brush.transfer.opacity.control,
            &mut brush.transfer.flow.control,
            &mut brush.shape_dynamics.angle.control,
        ] {
            reference_control(control, REFERENCE_SIZE_CONTROLS);
        }
        for control in [
            &mut brush.shape_dynamics.roundness.control,
            &mut brush.shape_dynamics.stroke_jitter.control,
            &mut brush.scattering.scatter.control,
            &mut brush.scattering.count_jitter.control,
            &mut brush.inertia.control,
        ] {
            reference_control(control, REFERENCE_SHAPE_CONTROLS);
        }
        reference_control(&mut brush.scattering.spacing_jitter.control, REFERENCE_SPACING_CONTROLS);
        reference_control(&mut brush.texture.depth_jitter.control, REFERENCE_TEXTURE_CONTROLS);
        for control in [
            &mut brush.color_dynamics.hue_control,
            &mut brush.color_dynamics.saturation_control,
            &mut brush.color_dynamics.brightness_control,
            &mut brush.color_dynamics.purity_control,
        ] {
            reference_control(control, REFERENCE_BASE_CONTROLS);
        }
        for grain in [&mut brush.grain, &mut brush.dual_brush.grain].into_iter().flatten() {
            reference_control(&mut grain.control, REFERENCE_BASE_CONTROLS);
        }
        for wet in [&mut brush.wet_edge, &mut brush.dual_brush.wet_edge].into_iter().flatten() {
            reference_control(&mut wet.control, REFERENCE_BASE_CONTROLS);
        }
        if let Some(pigment) = brush.pigment.as_mut() {
            reference_control(&mut pigment.thickness_control, REFERENCE_PIGMENT_CONTROLS);
            reference_control(&mut pigment.visual_thickness_control, REFERENCE_PIGMENT_CONTROLS);
            pigment.liquidity = 0.0;
            pigment.fluid_strength = 0.0;
            pigment.fluid_pressure_enabled = false;
            pigment.gaussian_mask = false;
        }
        brush.mix_settings.smudge_enabled = true;
        brush.mix_settings.blur_enabled = true;
        brush.mix_settings.strength_dynamic = Dynamic::default();
        brush.mix_settings.length_dynamic = Dynamic::default();
        brush.mix_settings.length_pressure_inverted = false;
        brush.mix_settings.blur_dynamic = Dynamic::default();
        brush.mix_settings.blur_pressure_inverted = false;
        brush.mix_settings.length = 0.5;
        brush.mix_settings.concentration = 1.0;
        brush.mix_settings.water = 0.0;
        brush.mix_settings.smooth = 0.0;
        brush.mix_settings.quality = 4.0;
        brush.mix_settings.output_strength = 1.0;
        brush.mix_settings.blur_color_target = None;
        brush.size = safe_size(brush.size);
        brush.dual_brush.size = safe_size(brush.dual_brush.size);
        brush.dual_brush.opacity = finite_unit(brush.dual_brush.opacity);
        brush.dual_brush.flow = finite_unit(brush.dual_brush.flow);
        brush.dual_brush.primary_strength = finite_unit(brush.dual_brush.primary_strength);
        brush.dual_brush.secondary_strength = finite_unit(brush.dual_brush.secondary_strength);
        brush.scattering.scatter.jitter = safe_scatter(brush.scattering.scatter.jitter);
        brush.dual_brush.scatter = safe_scatter(brush.dual_brush.scatter);
        brush
    }

    /// This brush (a preset) picked while `current` is the tool's brush: Smoothing is a tool
    /// option, so it stays the tool's (Photoshop), a protected texture stays too, and so does every
    /// section `current` has locked (the locks themselves are tool state and carry over).
    pub fn picked_over(self, current: &BrushSettings) -> Self {
        let mut b = self.with_protected_texture(current);
        b.smoothing = current.smoothing.clone();
        b.pattern_enabled = current.pattern_enabled;
        b.sampled_pattern = current.sampled_pattern.clone();
        let l = &current.locks;
        if l.shape_dynamics {
            b.shape_dynamics = current.shape_dynamics.clone();
        }
        if l.scattering {
            b.scattering = current.scattering.clone();
        }
        if l.texture {
            b.texture = current.texture.clone();
        }
        if l.dual_brush {
            b.dual_brush = current.dual_brush.clone();
        }
        if l.color_dynamics {
            b.color_dynamics = current.color_dynamics.clone();
        }
        if l.transfer {
            b.transfer = current.transfer.clone();
        }
        if l.pose {
            b.pose = current.pose.clone();
        }
        if l.noise {
            b.noise = current.noise;
            b.grain = current.grain.clone();
        }
        if l.wet_edges {
            b.wet_edges = current.wet_edges;
            b.wet_edge = current.wet_edge.clone();
        }
        if l.build_up {
            b.build_up = current.build_up;
            b.build_up_rate = current.build_up_rate;
        }
        if l.protect_texture {
            b.protect_texture = current.protect_texture;
        }
        b.locks = current.locks.clone();
        b
    }

    /// Apply Protect Texture: if `current` protects its texture, a newly chosen brush with a texture
    /// keeps `current`'s pattern and scale.
    pub fn with_protected_texture(mut self, current: &BrushSettings) -> Self {
        if current.protect_texture && current.texture.enabled && self.texture.enabled {
            self.texture.pattern = current.texture.pattern.clone();
            self.texture.scale = current.texture.scale;
            self.protect_texture = true;
        }
        self
    }
}

fn finite_unit(value: f32) -> f32 {
    if value.is_finite() { value.clamp(0.0, 1.0) } else { 0.0 }
}

/// A named brush preset (session tool state; serde round-trips).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrushPreset {
    pub name: String,
    pub brush: BrushSettings,
    /// Shipped with PhotoCraft (built-ins can be deleted from a session but are regenerated on reset).
    #[serde(default)]
    pub builtin: bool,
    /// Preset group (folder) in the Brushes panel, e.g. "General" or an imported file's name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    /// Folders inside `group`, outermost first (empty = directly in the group), like the nested
    /// folders of Photoshop's Brushes panel; e.g. an imported `.abr` file's own folders.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folder: Vec<String>,
    /// What picking the preset brings besides the tip and its sections: the New Brush Preset
    /// dialog's boxes.
    #[serde(default, skip_serializing_if = "PresetCaptures::is_default")]
    pub captures: PresetCaptures,
}

/// The New Brush Preset dialog's boxes, kept with the preset. The default is a preset made
/// without the dialog (a built-in, an import, an older save): picking it brings its size and tool
/// settings, as it always did, and leaves the foreground colour alone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PresetCaptures {
    /// Capture Brush Size in Preset. Off: picking the preset keeps the tool's size.
    pub size: bool,
    /// Include Tool Settings: the options bar's mode, opacity, flow and pressure buttons. Off:
    /// picking the preset keeps the tool's.
    pub tool_settings: bool,
    /// Include Color: picking the preset sets the foreground colour to the brush's `color`.
    pub color: bool,
}

impl Default for PresetCaptures {
    fn default() -> Self {
        Self { size: true, tool_settings: true, color: false }
    }
}

impl PresetCaptures {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl BrushPreset {
    /// The tool's brush once this preset is picked while `current` is the tool's brush: the
    /// preset's brush ([`BrushSettings::picked_over`]), with the size and tool settings it didn't
    /// capture left as they were.
    pub fn picked_over(&self, current: &BrushSettings) -> BrushSettings {
        let mut b = self.brush.clone().picked_over(current);
        if !self.captures.size {
            b.resize_with_dual_brush(current.size);
        }
        if !self.captures.tool_settings {
            b.mode = current.mode;
            b.opacity = current.opacity;
            b.flow = current.flow;
            b.pressure_size = current.pressure_size;
            b.pressure_opacity = current.pressure_opacity;
            b.pressure_flow = current.pressure_flow;
        }
        b
    }
}

/// The two C3 colour attributes are evaluated against the destination, independently of coverage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ColorAttributes {
    pub enabled: bool,
    pub blend: crate::AttributeMode,
    pub blend_strength: f32,
    pub color: crate::AttributeMode,
    pub color_strength: f32,
}
impl Default for ColorAttributes {
    fn default() -> Self {
        Self { enabled: false, blend: crate::AttributeMode::Off, blend_strength: 0.0, color: crate::AttributeMode::Off, color_strength: 0.0 }
    }
}
impl ColorAttributes {
    /// Apply both independent attributes to the same brush and destination after normal painting.
    /// The normal result owns alpha; coverage is the effect's alpha rather than output alpha.
    pub fn composite(&self, normal: [f32; 4], destination: [f32; 4], source: [f32; 4], coverage: f32) -> [f32; 4] {
        if !self.enabled || coverage <= 0.0001 || normal[3] <= 0.5 / 255.0 {
            return normal;
        }
        let dst = [destination[0], destination[1], destination[2]];
        let brush = [source[0].clamp(0.0, 1.0), source[1].clamp(0.0, 1.0), source[2].clamp(0.0, 1.0)];
        let mut out = normal;
        for (mode, strength) in [(self.blend, self.blend_strength), (self.color, self.color_strength)] {
            if mode == crate::AttributeMode::Off {
                continue;
            }
            let mixed = mode.blend(dst, brush);
            for i in 0..3 {
                let value = dst[i] + (mixed[i] - dst[i]) * coverage.clamp(0.0, 1.0);
                out[i] += (value - out[i]) * strength.clamp(0.0, 1.0);
            }
        }
        out
    }
}

/// An RGBA patch sampled from the canvas. Its longest side scales to each dab diameter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorPattern {
    pub width: u32,
    pub height: u32,
    pub pixels: std::sync::Arc<[[f32; 4]]>,
}
impl ColorPattern {
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= 2048
            && self.height <= 2048
            && self.pixels.len() == self.width as usize * self.height as usize
            && self.pixels.iter().all(|p| p.iter().all(|v| v.is_finite()) && (0.0..=1.0).contains(&p[3]))
    }
    /// C3 sampled paint uses an alpha-weighted five-tap color; coverage uses center alpha.
    pub fn sample_pigment(&self, x: f32, y: f32) -> [f32; 4] {
        let center = self.sample(x, y);
        if center[3] <= 0.0001 {
            return center;
        }
        let mut sum = [center[0] * center[3] * 4.0, center[1] * center[3] * 4.0, center[2] * center[3] * 4.0];
        let mut weight = center[3] * 4.0;
        let hx = self.width as f32 * 0.5;
        let hy = self.height as f32 * 0.5;
        for (dx, dy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
            let p = self.sample((x + dx).clamp(-hx + 0.5, hx - 0.5), (y + dy).clamp(-hy + 0.5, hy - 0.5));
            weight += p[3];
            for i in 0..3 {
                sum[i] += p[i] * p[3];
            }
        }
        [sum[0] / weight.max(0.0001), sum[1] / weight.max(0.0001), sum[2] / weight.max(0.0001), center[3]]
    }
    pub fn sample(&self, x: f32, y: f32) -> [f32; 4] {
        let (x, y) = (x + self.width as f32 * 0.5 - 0.5, y + self.height as f32 * 0.5 - 0.5);
        if x < -0.5 || y < -0.5 || x >= self.width as f32 - 0.5 || y >= self.height as f32 - 0.5 {
            return [0.0; 4];
        }
        let (x, y) = (x.max(0.0), y.max(0.0));
        let (ix, iy) = (x.floor() as u32, y.floor() as u32);
        let at = |xx: u32, yy: u32| {
            self.pixels.get((yy.min(self.height.saturating_sub(1)) * self.width + xx.min(self.width.saturating_sub(1))) as usize).copied().unwrap_or([0.0; 4])
        };
        let a = at(ix, iy);
        let b = at(ix + 1, iy);
        let c = at(ix, iy + 1);
        let d = at(ix + 1, iy + 1);
        std::array::from_fn(|i| (a[i] + (b[i] - a[i]) * x.fract()) * (1.0 - y.fract()) + (c[i] + (d[i] - c[i]) * x.fract()) * y.fract())
    }
}

impl DualBrush {
    pub fn resolved_brush(&self, parent: &BrushSettings) -> Option<BrushSettings> {
        let mut b = self.full_parameters.as_deref()?.clone();
        b.dual_brush = Self::default();
        b = b.bounded_for_render();
        b.color = parent.color;
        b.background = parent.background;
        b.seed = parent.seed ^ 0xD1B54A32D192ED03;
        // C3's sampled colour patch belongs to the primary lane.
        b.pattern_enabled = false;
        b.sampled_pattern = None;
        b.erase = parent.erase;
        Some(b)
    }
}
