//! Spatial grain and wet-edge curves. Parameters use the reference brush's storage units.
use crate::brush::Control;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GrainSettings {
    pub density: f32,
    pub size: f32,
    /// Spatial jitter fractions, not random changes per dab.
    pub size_jitter: f32,
    pub density_jitter: f32,
    pub control: Control,
    pub fade_steps: u32,
}
impl Default for GrainSettings {
    fn default() -> Self {
        Self { density: 0.5, size: 1.0, size_jitter: 0.0, density_jitter: 0.0, control: Control::Off, fade_steps: 25 }
    }
}

macro_rules! choices {
    ($name:ident, $default:ident $(, $variant:ident)*) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub enum $name { #[default] $default, $($variant),* }
    };
}
choices!(WetStyle, Photoshop, Watercolor, Watercolor2);
choices!(WetWidthMode, Ratio, Fixed);
choices!(WetOutline, Stroke, Single);
choices!(WetEdgeControl, Expansion, SoftCoverage);
choices!(WetColorMode, Normal, Hsl, Multiply, ColorBurn);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WetEdgeSettings {
    pub style: WetStyle,
    /// Reference units: strength 0..2, width/softness 0..5, fixed width 0..256 px.
    pub strength: f32,
    pub minimum_strength: f32,
    pub control: Control,
    pub fade_steps: u32,
    pub width_mode: WetWidthMode,
    pub width: f32,
    pub fixed_width: f32,
    pub minimum_width: f32,
    pub softness: f32,
    pub brightness: f32,
    pub saturation: f32,
    pub color_mode: WetColorMode,
    pub edge_control: WetEdgeControl,
    pub outline: WetOutline,
}
impl Default for WetEdgeSettings {
    fn default() -> Self {
        Self {
            style: WetStyle::Photoshop,
            strength: 1.0,
            minimum_strength: 0.0,
            control: Control::Off,
            fade_steps: 25,
            width_mode: WetWidthMode::Ratio,
            width: 1.0,
            fixed_width: 24.0,
            minimum_width: 0.0,
            softness: 0.0,
            brightness: 0.0,
            saturation: 0.0,
            color_mode: WetColorMode::Normal,
            edge_control: WetEdgeControl::Expansion,
            outline: WetOutline::Stroke,
        }
    }
}

/// Resolved per-dab strength, width and single-dab width in the renderer's domain.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResolvedWet {
    pub strength: f32,
    pub width: f32,
    pub stamp_width: f32,
}

fn unit(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}
fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = unit((x - a) / (b - a).max(0.00001));
    t * t * (3.0 - 2.0 * t)
}
fn continuous(v: f32) -> f32 {
    if v <= 0.75 { v } else { 1.0 - 0.25 / (1.0 + 4.0 * (v - 0.75)) }
}

impl WetEdgeSettings {
    pub fn resolve(&self, control: f32, diameter: f32) -> ResolvedWet {
        let watercolor = self.style != WetStyle::Photoshop;
        let strength = self.minimum_strength.min(self.strength).max(0.0);
        let raw_width = match self.width_mode {
            WetWidthMode::Ratio => self.width.clamp(0.0, 5.0),
            WetWidthMode::Fixed => self.fixed_width.clamp(0.0, 256.0),
        };
        let width = mix(self.minimum_width.clamp(0.0, raw_width), raw_width, unit(control));
        let fixed = self.width_mode == WetWidthMode::Fixed;
        let w = if fixed { width / diameter.max(1.0) } else { width };
        let sw = if fixed { 2.0 * width / diameter.max(1.0) } else { width };
        ResolvedWet {
            strength: mix(strength, self.strength.clamp(0.0, 2.0), unit(control)) * if watercolor { 1.4 } else { 0.5 },
            width: w.clamp(0.0, 5.0) * if watercolor { 0.7 } else { 0.4 },
            stamp_width: sw.clamp(0.0, 5.0) * if watercolor { 0.7 } else { 0.4 },
        }
    }
    fn width01(&self, w: f32) -> f32 {
        if self.style == WetStyle::Photoshop { unit(w) } else { continuous(w.clamp(0.0, 5.0)) }
    }
    fn strength01(&self, s: f32) -> f32 {
        let s = s.clamp(0.0, if self.style == WetStyle::Photoshop { 2.0 } else { 2.8 });
        let e = s * (0.6 + 0.4 * smooth(0.3, 0.7, s));
        if self.style == WetStyle::Photoshop { unit(e) } else { continuous(e) }
    }
    pub fn color_mask(&self, a: f32, w: f32) -> f32 {
        let t = self.width01(w);
        match self.style {
            WetStyle::Watercolor => smooth(0.008, mix(0.04, 0.12, t), a) * (1.0 - smooth(mix(0.45, 0.72, t), mix(0.78, 0.96, t), a)),
            WetStyle::Photoshop => smooth(0.01, mix(0.06, 0.14, t), a) * (1.0 - smooth(mix(0.32, 0.58, t), mix(0.66, 0.90, t), a)),
            WetStyle::Watercolor2 => 1.0,
        }
    }
    pub fn dab_edge(&self, a: f32, rn: f32, wet: ResolvedWet) -> f32 {
        if self.width_mode == WetWidthMode::Fixed {
            smooth((1.0 - wet.stamp_width).max(0.0), 1.0, rn)
        } else if self.style == WetStyle::Watercolor2 {
            smooth(1.0 - mix(0.10, 0.32, self.width01(wet.stamp_width)), 1.0, rn)
        } else {
            self.color_mask(a, wet.stamp_width)
        }
    }
    pub fn alpha(&self, a: f32, edge: f32, wet: ResolvedWet) -> f32 {
        if a <= 0.0 || wet.strength <= 0.0001 {
            return a;
        }
        let t = self.width01(wet.width);
        let lift_max = match self.style {
            WetStyle::Photoshop => mix(0.55, 0.85, t),
            WetStyle::Watercolor => mix(0.40, 0.70, t),
            WetStyle::Watercolor2 => mix(0.50, 0.85, t),
        };
        let lift = self.strength01(wet.strength) * lift_max * edge;
        let t = self.width01(if self.outline == WetOutline::Single { wet.stamp_width } else { wet.width });
        let soften = match self.style {
            WetStyle::Watercolor2 => edge,
            WetStyle::Watercolor => smooth(0.008, mix(0.03, 0.10, t), a) * (1.0 - smooth(mix(0.24, 0.42, t), mix(0.42, 0.68, t), a)),
            WetStyle::Photoshop => smooth(0.01, mix(0.04, 0.11, t), a) * (1.0 - smooth(mix(0.18, 0.34, t), mix(0.34, 0.60, t), a)),
        };
        let lifted = if self.style == WetStyle::Photoshop { a + (1.0 - a) * lift } else { 1.0 - (1.0 - unit(a)).powf(1.0 + 3.0 * lift) };
        let filtered = unit(lifted * (1.0 - unit(self.softness / 5.0) * soften * 0.75));
        if self.edge_control == WetEdgeControl::Expansion { a.max(filtered) } else { filtered }
    }
    /// Spatial Watercolor 2 stencil; sampling uses global coordinates, including adjacent tiles.
    pub fn spatial_edge(&self, a: f32, w: f32, x: i32, y: i32, sample: impl Fn(i32, i32) -> f32) -> f32 {
        let step = (1.0 + w.clamp(0.0, 5.0) * 1.5).max(1.0) / 3.0;
        let mut gradient = 0.0;
        let mut spread = 0.0;
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let (px, py) = (x as f32 + dx as f32 * step, y as f32 + dy as f32 * step);
                let (ix, iy) = (px.floor() as i32, py.floor() as i32);
                let (fx, fy) = (px.fract().rem_euclid(1.0), py.fract().rem_euclid(1.0));
                let n = mix(
                    mix(sample(ix, iy), sample(ix.saturating_add(1), iy), fx),
                    mix(sample(ix, iy.saturating_add(1)), sample(ix.saturating_add(1), iy.saturating_add(1)), fx),
                    fy,
                );
                let weight = ((4 - dx.abs()) * (4 - dy.abs())) as f32;
                gradient += weight * (a - n).max(0.0);
                spread += weight * (a - n).abs();
            }
        }
        let signal = (spread * 1.55 + gradient * 1.75) / 240.0;
        let t = self.width01(w);
        unit(signal / (0.25 + signal) * smooth(0.02, mix(0.16, 0.34, t), a) * (1.0 - smooth(mix(0.42, 0.70, t), mix(0.80, 0.98, t), a)) * smooth(0.02, 0.3, a))
    }
    /// Straight source/base colours. Transparent base pixels do not supply a watercolour tone.
    pub fn color(&self, paint: [f32; 4], base: [f32; 4], edge: f32, wet: ResolvedWet) -> [f32; 4] {
        self.color_with_effect(paint, base, self.color_strength(wet.strength) * edge)
    }
    pub(crate) fn color_strength(&self, strength: f32) -> f32 {
        if strength <= 0.0001 {
            0.0
        } else if self.style == WetStyle::Photoshop {
            unit(strength)
        } else {
            self.strength01(strength)
        }
    }
    pub(crate) fn color_with_effect(&self, paint: [f32; 4], base: [f32; 4], effect: f32) -> [f32; 4] {
        if self.color_mode == WetColorMode::Normal || effect <= 0.0 {
            return paint;
        }
        let watercolor = self.style != WetStyle::Photoshop;
        if watercolor && base[3] <= 0.0001 {
            return paint;
        }
        let c = if watercolor { base } else { paint };
        let effect = if watercolor { effect * unit(base[3]) } else { effect };
        let (h, s, l) = rgb_to_hsl([c[0], c[1], c[2]]);
        let (sat, drop) = match self.style {
            WetStyle::Photoshop => (0.05, 0.10),
            WetStyle::Watercolor => (0.27, 0.45),
            WetStyle::Watercolor2 => (0.33, 0.54),
        };
        let gain = if watercolor { 2.0 } else { 1.0 };
        let tone = hsl_to_rgb(h, unit(s + sat + self.saturation * gain), unit(l - drop - self.brightness * gain));
        let mut out = paint;
        for i in 0..3 {
            let target = match self.color_mode {
                WetColorMode::Multiply => c[i] * tone[i],
                WetColorMode::ColorBurn => unit(1.0 - (1.0 - c[i]) / tone[i].max(0.01)),
                _ => tone[i],
            };
            out[i] = mix(paint[i], target, effect).min(paint[i]);
        }
        out
    }
}

fn value_noise(x: f32, y: f32) -> f32 {
    // The renderer's integer hash keeps spatial phase stable across CPU architectures.
    let (ix, iy) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - x.floor(), y - y.floor());
    let (fx, fy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let n = |dx: i32, dy: i32| crate::rng::hash2(ix.saturating_add(dx), iy.saturating_add(dy), 0);
    mix(mix(n(0, 0), n(1, 0), fx), mix(n(0, 1), n(1, 1), fx), fy)
}
fn fbm(x: f32, y: f32) -> f32 {
    (value_noise(x, y) * 0.56 + value_noise(x * 2.07, y * 2.07) * 0.28 + value_noise(x * 4.2849, y * 4.2849) * 0.14) / 0.98
}
impl GrainSettings {
    pub fn sample(&self, x: f32, y: f32) -> f32 {
        let scale = 0.7 + unit((self.size - 1.0) / 199.0).powf(1.35) * 20.0;
        let warp = scale * 0.55 * unit(self.size_jitter).powf(1.35);
        let (wx, wy) = if warp > 0.0001 {
            ((fbm(x * 0.013 + 3.1, y * 0.013 + 7.2) - 0.5) * warp, (fbm(x * 0.013 + 11.3, y * 0.013 + 17.5) - 0.5) * warp)
        } else {
            (0.0, 0.0)
        };
        fbm((x + wx) / scale, (y + wy) / scale)
    }
    pub fn alpha(&self, a: f32, density: f32, x: i32, y: i32) -> f32 {
        if density <= 0.0001 || a <= 0.0 {
            return a;
        }
        let (x, y) = (x as f32, y as f32);
        let noise = self.sample(x, y);
        let overlay = if a < 0.5 { 2.0 * a * noise } else { 1.0 - 2.0 * (1.0 - a) * (1.0 - noise) };
        let amount = unit(self.density_jitter).powf(1.15);
        let mask = if amount <= 0.0001 { 1.0 } else { 1.0 - amount + self.sample(x + 157.0, y + 89.0) * amount };
        unit(mix(a, overlay, unit(density)) * mask)
    }
}

fn rgb_to_hsl(c: [f32; 3]) -> (f32, f32, f32) {
    let hi = c[0].max(c[1]).max(c[2]);
    let lo = c[0].min(c[1]).min(c[2]);
    let l = (hi + lo) * 0.5;
    let d = hi - lo;
    if d < 0.000001 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs()).max(0.000001);
    let h = if hi == c[0] {
        ((c[1] - c[2]) / d).rem_euclid(6.0)
    } else if hi == c[1] {
        (c[2] - c[0]) / d + 2.0
    } else {
        (c[0] - c[1]) / d + 4.0
    };
    (h / 6.0, s, l)
}
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h = h * 6.0;
    let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let m = l - c * 0.5;
    let rgb = if h < 1.0 {
        [c, x, 0.0]
    } else if h < 2.0 {
        [x, c, 0.0]
    } else if h < 3.0 {
        [0.0, c, x]
    } else if h < 4.0 {
        [0.0, x, c]
    } else if h < 5.0 {
        [x, 0.0, c]
    } else {
        [c, 0.0, x]
    };
    rgb.map(|v| v + m)
}
