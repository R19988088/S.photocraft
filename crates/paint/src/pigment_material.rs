//! C3 material controls and the separate signed-thickness / visual-height dab field.
use crate::{
    brush::Control,
    dynamics::{StepInput, control_value},
};
use serde::{Deserialize, Serialize};

fn bounded(v: f32, lo: f32, hi: f32) -> f32 {
    if v.is_finite() { v.clamp(lo, hi) } else { lo }
}
fn unit(v: f32) -> f32 {
    bounded(v, 0., 1.)
}
fn smooth(a: f32, b: f32, v: f32) -> f32 {
    let t = unit((v - a) / (b - a));
    t * t * (3. - 2. * t)
}

/// Two connected cubic Bézier segments. Y is signed and need not be monotone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SignedCurve {
    pub points: [[f32; 2]; 7],
    pub enabled: bool,
}
impl Default for SignedCurve {
    fn default() -> Self {
        Self { points: std::array::from_fn(|i| [i as f32 / 6., 0.]), enabled: true }
    }
}
impl SignedCurve {
    /// C3 constrains the middle join to one tangent, shortening handles at the bounds.
    pub fn normalize_handles(&mut self) {
        self.normalize_handles_with_minimum(-1.0);
    }

    pub fn normalize_handles_with_minimum(&mut self, minimum: f32) {
        let gap = 1.0 / 255.0;
        let p = &mut self.points;
        for point in p.iter_mut() {
            point[0] = unit(point[0]);
            point[1] = bounded(point[1], minimum, 1.0);
        }
        p[0][0] = 0.0;
        p[6][0] = 1.0;
        p[3][0] = p[3][0].clamp(3.0 * gap, 1.0 - 3.0 * gap);
        for start in [0, 3] {
            let end = start + 3;
            p[start + 1][0] = p[start + 1][0].clamp(p[start][0] + gap, p[end][0] - 2.0 * gap);
            p[start + 2][0] = p[start + 2][0].clamp(p[start + 1][0] + gap, p[end][0] - gap);
        }
        let left = (p[3][1] - p[2][1]) / (p[3][0] - p[2][0]);
        let right = (p[4][1] - p[3][1]) / (p[4][0] - p[3][0]);
        if (left - right).abs() > 1e-12 {
            let center = p[3];
            let limit = (center[1] - minimum).min(1.0 - center[1]) / gap;
            let slope = ((p[4][1] - p[2][1]) / (p[4][0] - p[2][0])).clamp(-limit, limit);
            let dx = 1.0 / (1.0 + slope * slope).sqrt();
            for index in [2, 4] {
                let ux = if index == 2 { -dx } else { dx };
                let uy = ux * slope;
                let edge = if index == 2 { p[1][0] + gap } else { p[5][0] - gap };
                let mut maximum = (gap / dx).max((edge - center[0]) / ux);
                if uy > 0.0 {
                    maximum = maximum.min((1.0 - center[1]) / uy);
                } else if uy < 0.0 {
                    maximum = maximum.min((minimum - center[1]) / uy);
                }
                let length = (gap / dx).max((p[index][0] - center[0]).hypot(p[index][1] - center[1]).min(maximum));
                p[index] = [center[0] + ux * length, center[1] + uy * length];
            }
        }
        for index in [1, 5] {
            p[index][1] = p[index][1].clamp(p[index - 1][1].min(p[index + 1][1]), p[index - 1][1].max(p[index + 1][1]));
        }
    }
    pub fn is_valid(&self) -> bool {
        self.points.iter().all(|p| p[0].is_finite() && p[1].is_finite() && (0.0..=1.0).contains(&p[0]) && (-1.0..=1.0).contains(&p[1]))
            && self.points[0][0] == 0.
            && self.points[6][0] == 1.
            && self.points.windows(2).all(|w| w[1][0] > w[0][0])
    }
    pub fn value(&self, input: f32) -> f32 {
        if !self.enabled || !self.is_valid() {
            return 0.;
        }
        let x = unit(input);
        let start = if x <= self.points[3][0] { 0 } else { 3 };
        let p = &self.points[start..start + 4];
        let cubic = |axis: usize, t: f32| {
            let u = 1. - t;
            u * u * u * p[0][axis] + 3. * u * u * t * p[1][axis] + 3. * u * t * t * p[2][axis] + t * t * t * p[3][axis]
        };
        // Match C3's 257-sample segment table and linear interpolation between samples.
        let position = (x - p[0][0]) / (p[3][0] - p[0][0]) * 256.;
        let index = position.floor().clamp(0., 255.);
        let f = unit(position - index);
        let sample = |index: f32| {
            if index == 0. {
                return p[0][1];
            }
            if index == 256. {
                return p[3][1];
            }
            let target = p[0][0] + (p[3][0] - p[0][0]) * index / 256.;
            let (mut low, mut high) = (0., 1.);
            for _ in 0..24 {
                let t = (low + high) * 0.5;
                if cubic(0, t) < target { low = t } else { high = t }
            }
            cubic(1, (low + high) * 0.5)
        };
        sample(index) * (1. - f) + sample(index + 1.) * f
    }
}

/// C2 secondary contour controls. Minimum/jitter are retained preset values;
/// C2 rendering uses signed strength and pressure only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContourSettings {
    pub strength: f32,
    pub minimum: f32,
    pub jitter: f32,
    pub control: Control,
    pub pressure_enabled: bool,
}
impl ContourSettings {
    pub fn is_valid(&self) -> bool {
        self.strength.is_finite()
            && (-1.0..=1.0).contains(&self.strength)
            && [self.minimum, self.jitter].iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v))
    }
    pub fn adjusted_hardness(&self, base: f32, pressure: f32) -> f32 {
        let signed = bounded(self.strength, -1.0, 1.0);
        let effective = if self.pressure_enabled || self.control == Control::PenPressure { signed.max(0.0) * unit(pressure) } else { signed };
        unit(unit(base) - effective * 0.35)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PigmentSettings {
    pub enabled: bool,
    /// C3's absolute 0..100 units, not a mixing strength or a minimum ratio.
    pub thickness: f32,
    pub thickness_minimum: f32,
    pub thickness_jitter: f32,
    pub thickness_control: Control,
    pub thickness_curve: Option<SignedCurve>,
    pub visual_thickness: f32,
    pub visual_thickness_minimum: f32,
    pub visual_thickness_jitter: f32,
    pub visual_thickness_control: Control,
    pub liquidity: f32,
    pub highlight_strength: f32,
    pub highlight_brightness: f32,
    pub highlight_ratio: f32,
    pub ignore_canvas_highlight: bool,
    pub gaussian_mask: bool,
    /// Independent of the pigment toggle, just as C3's primary fluid control is.
    pub fluid_strength: f32,
    pub fluid_pressure_enabled: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResolvedPigment {
    pub enabled: bool,
    pub pressure: f32,
    pub thickness: f32,
    pub visual_thickness: f32,
    pub liquidity: f32,
    pub highlight_strength: f32,
    pub highlight_brightness: f32,
    pub highlight_ratio: f32,
}

/// Absolute floor + controller + additive jitter, bounded by the absolute floor and ceiling.
pub fn parameter(maximum: f32, minimum: f32, jitter: f32, weight: f32, sample: f32) -> f32 {
    let upper = bounded(maximum, 0., 100.) / 100.;
    let lower = (bounded(minimum, 0., 100.) / 100.).min(upper);
    let sample = if sample.is_finite() { unit(sample) } else { 0.5 };
    (lower + (upper - lower) * unit(weight) + (sample * 2. - 1.) * bounded(jitter, 0., 100.) / 100.).clamp(lower, upper)
}
impl PigmentSettings {
    pub fn is_valid(&self) -> bool {
        [self.thickness, self.thickness_minimum, self.thickness_jitter, self.visual_thickness, self.visual_thickness_minimum, self.visual_thickness_jitter]
            .into_iter()
            .all(|v| v.is_finite() && (0.0..=100.0).contains(&v))
            && [self.liquidity, self.highlight_strength, self.highlight_brightness, self.highlight_ratio]
                .into_iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(&v))
            && self.fluid_strength.is_finite()
            && (-1.0..=1.0).contains(&self.fluid_strength)
            && self.thickness_curve.as_ref().is_none_or(SignedCurve::is_valid)
    }
    pub fn resolve(&self, s: &StepInput, jitter: [f32; 2]) -> ResolvedPigment {
        let w = control_value(self.thickness_control, 25, s);
        ResolvedPigment {
            enabled: self.enabled,
            pressure: unit(s.point.pressure),
            thickness: self
                .thickness_curve
                .as_ref()
                .map_or_else(|| parameter(self.thickness, self.thickness_minimum, self.thickness_jitter, w, jitter[0]), |c| c.value(w)),
            visual_thickness: parameter(
                self.visual_thickness,
                self.visual_thickness_minimum,
                self.visual_thickness_jitter,
                control_value(self.visual_thickness_control, 25, s),
                jitter[1],
            ),
            liquidity: unit(self.liquidity),
            highlight_strength: unit(self.highlight_strength),
            highlight_brightness: unit(self.highlight_brightness),
            highlight_ratio: unit(self.highlight_ratio),
        }
    }
    pub fn adjusted_hardness(&self, base: f32, pressure: f32) -> f32 {
        let base = if self.enabled && self.gaussian_mask { 0. } else { unit(base) };
        let signed = bounded(self.fluid_strength, -1., 1.);
        let effective = if self.fluid_pressure_enabled { signed.max(0.) * unit(pressure) } else { signed };
        unit(base - effective * 0.35)
    }
}

/// `near` / `far`: hard-tip coverage at the eight compass directions, at step and 2.15×step.
/// Step is clamp(radius*0.075, 0.65, 3). `local` is inverse-rotated tip coordinates / half-extents.
pub fn emboss_height(mask: f32, local: [f32; 2], pattern: f32, near: [f32; 8], far: [f32; 8]) -> f32 {
    if mask <= 0.0001 {
        return 0.;
    }
    let edge = unit(near.iter().map(|v| (mask - v).abs()).sum::<f32>() * 0.22);
    let inner = near.iter().sum::<f32>() * 0.125;
    let outer = far.iter().sum::<f32>() * 0.125;
    let interior = smooth(0.015, 0.84, mask);
    let rim = smooth(0.02, 0.42, (inner - outer).abs()) * smooth(0.015, 0.65, mask);
    let gap = smooth(0.12, 0.78, 1. - pattern);
    let [x, y] = local;
    let fract = |v: f32| v - v.floor();
    let layers = 0.28 * smooth(0.48, 0.56, fract(x * 8. + y * 1.3))
        + 0.24 * smooth(0.46, 0.55, fract(x * 13. - y * 2.1 + 0.31))
        + 0.26 * smooth(0.47, 0.57, fract(y * 10. + x * 1.7 + 0.63))
        + 0.22 * smooth(0.45, 0.56, fract((x + y) * 18. + 0.17));
    unit(interior * 0.34 + edge * 0.18 + rim * 0.24 + (1. - gap * layers) * 0.24)
}
/// Exact material field written by C3's renderC2PigmentThickness, independently of pigment RGB.
pub fn deposit(p: ResolvedPigment, coverage: f32, flow: f32, opacity: f32, emboss: f32) -> [f32; 4] {
    if !p.enabled {
        return [0.; 4];
    }
    let alpha = unit(coverage * unit(flow) * unit(opacity));
    [alpha * bounded(p.thickness, -1., 1.), alpha * unit(p.visual_thickness) * unit(emboss) * (0.35 + 0.65 * unit(flow)), 0., alpha]
}
pub fn over_material(previous: [f32; 4], dab: [f32; 4]) -> [f32; 4] {
    std::array::from_fn(|i| dab[i] + previous[i] * (1. - unit(dab[3])))
}

#[cfg(test)]
mod tests {
    #[test]
    fn signed_curve_edits_keep_a_continuous_middle_tangent() {
        let mut curve = super::SignedCurve { points: [[0.0, 0.0], [0.1, 0.8], [0.3, 0.1], [0.5, 0.2], [0.7, 0.9], [0.9, -0.8], [1.0, 0.0]], enabled: true };
        curve.normalize_handles();
        assert!(curve.is_valid());
        let p = curve.points;
        let a = (p[3][1] - p[2][1]) / (p[3][0] - p[2][0]);
        let b = (p[4][1] - p[3][1]) / (p[4][0] - p[3][0]);
        assert!((a - b).abs() < 1e-5);
    }

    use super::*;
    #[test]
    fn absolute_minimum_and_additive_jitter_match_c3() {
        assert!((parameter(80., 20., 0., 0.5, 0.5) - 0.5).abs() < 1e-6);
        assert!((parameter(80., 20., 10., 0.5, 1.) - 0.6).abs() < 1e-6);
        assert_eq!(parameter(80., 20., 100., 0., 0.), 0.2);
        assert_eq!(parameter(80., 90., 0., 0., 0.5), 0.8);
    }
    #[test]
    fn signed_curve_preserves_negative_and_nonmonotone_height() {
        let c = SignedCurve {
            points: std::array::from_fn(|i| {
                let x = i as f32 / 6.;
                [x, x * 2. - 1.]
            }),
            enabled: true,
        };
        assert!((c.value(0.25) + 0.5).abs() < 1e-5);
        assert!((c.value(0.75) - 0.5).abs() < 1e-5);
        assert_eq!(c.value(0.), -1.);
        assert_eq!(c.value(1.), 1.);
    }
    #[test]
    fn secondary_contour_uses_signed_hardness_and_pressure_without_web_jitter() {
        let c = ContourSettings { strength: 1.0, minimum: 1.0, jitter: 1.0, control: Control::Speed, ..Default::default() };
        assert!((c.adjusted_hardness(1.0, 0.25) - 0.65).abs() < 1e-6);
        let c = ContourSettings { control: Control::PenPressure, ..c };
        assert!((c.adjusted_hardness(1.0, 0.25) - 0.9125).abs() < 1e-6);
        let c = ContourSettings { strength: -1.0, ..c };
        assert_eq!(c.adjusted_hardness(0.3, 1.0), 0.3);
        let c = ContourSettings { control: Control::Off, ..c };
        assert!((c.adjusted_hardness(0.3, 1.0) - 0.65).abs() < 1e-6);
    }

    #[test]
    fn fluid_and_thickness_have_separate_consumers() {
        let p = PigmentSettings { fluid_strength: 1., ..Default::default() };
        assert!((p.adjusted_hardness(1., 0.5) - 0.65).abs() < 1e-6);
        let p = PigmentSettings { fluid_pressure_enabled: true, ..p };
        assert!((p.adjusted_hardness(1., 0.5) - 0.825).abs() < 1e-6);
        let m = ResolvedPigment { enabled: true, thickness: -0.4, visual_thickness: 0.8, ..Default::default() };
        let field = deposit(m, 1., 0.5, 0.8, 0.75);
        assert!((field[0] + 0.16).abs() < 1e-6);
        assert!((field[1] - 0.162).abs() < 1e-6);
        assert_eq!(field[3], 0.4);
        assert_eq!(deposit(ResolvedPigment::default(), 1., 1., 1., 1.), [0.; 4]);
        assert_eq!(over_material([1.; 4], [0.; 4]), [1.; 4]);
    }
    #[test]
    fn emboss_matches_flat_tip_and_never_creates_coverage() {
        assert_eq!(emboss_height(0., [0.; 2], 0., [1.; 8], [0.; 8]), 0.);
        assert!((emboss_height(1., [0.; 2], 1., [1.; 8], [1.; 8]) - 0.58).abs() < 1e-6);
        let p = ResolvedPigment { enabled: true, visual_thickness: 1., ..Default::default() };
        assert_eq!(deposit(p, 0., 1., 1., 1.), [0.; 4]);
    }
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.into_iter().zip(b).map(|(x, y)| x * y).sum()
}
fn norm(a: [f32; 3]) -> [f32; 3] {
    let n = dot(a, a).sqrt().max(1e-12);
    a.map(|v| v / n)
}
fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}
fn specular(light: [f32; 3], normal: [f32; 3], width: f32) -> f32 {
    let half = norm([light[0], light[1], light[2] + 1.]);
    let nh = unit(dot(normal, half));
    let nl = unit(dot(normal, light));
    let nv = unit(normal[2]);
    let lh = unit(dot(light, half));
    let a2 = 0.075 * 0.075;
    let mut denominator = nh * nh * (a2 - 1.) + 1.;
    if width != 1. {
        denominator = a2 + (denominator - a2) / (width * width).max(1e-6);
    }
    let distribution = a2 / (std::f32::consts::PI * denominator * denominator);
    let geometry = 1. / ((nl + (a2 + (1. - a2) * nl * nl).sqrt()) * (nv + (a2 + (1. - a2) * nv * nv).sqrt()));
    distribution * geometry * (0.05 + 0.95 * (1. - lh).powi(5))
}
/// Native-scale port of C2CanvasPaintPipeline's `oilColorDeposit`. Call with the unshaded
/// candidate colour and bilinear samplers of the completed material and candidate colour maps.
/// `motion` is the capped dab displacement (C3 caps to min(8,max(2,diameter*0.08))).
pub fn shade(
    old: [f32; 4],
    p: ResolvedPigment,
    pressure: f32,
    motion: [f32; 2],
    material_at: impl Fn([f32; 2]) -> [f32; 4],
    color_at: impl Fn([f32; 2]) -> [f32; 4],
) -> [f32; 4] {
    if !p.enabled {
        return old;
    }
    let liquidity = unit(p.liquidity);
    let pressure = unit(pressure);
    let input = |point: [f32; 2]| {
        let q = [point[0] - motion[0] * liquidity, point[1] - motion[1] * liquidity];
        let center = material_at(q);
        if liquidity <= 0. {
            return (center, center);
        }
        let spread = 0.5 + 4. * liquidity;
        let mut neighbors = [0.; 4];
        for offset in [[spread, 0.], [-spread, 0.], [0., spread], [0., -spread]] {
            let sample = material_at([q[0] + offset[0], q[1] + offset[1]]);
            for (sum, v) in neighbors.iter_mut().zip(sample) {
                *sum += v * 0.25;
            }
        }
        (std::array::from_fn(|i| center[i] + (neighbors[i] - center[i]) * liquidity * 0.85), center)
    };
    let (deposit, local) = input([0.; 2]);
    if local[3] <= 0. {
        return old;
    }
    let height = |point| {
        let v = input(point).0;
        v[0] + v[1] * 0.85
    };
    let tl = height([-1., -1.]);
    let tr = height([1., -1.]);
    let bl = height([-1., 1.]);
    let br = height([1., 1.]);
    let l = height([-1., 0.]);
    let r = height([1., 0.]);
    let t = height([0., -1.]);
    let b = height([0., 1.]);
    let slope = [tl - tr + 2. * (l - r) + bl - br, tl - bl + 2. * (t - b) + tr - br];
    let normal = norm([-slope[0] * 32. * (1. - 0.55 * pressure), -slope[1] * 32. * (1. - 0.55 * pressure), 7.]);
    let light = norm([0., 1., 1.]);
    let flat = [0., 0., 1.];
    let mut highlight = 0.;
    if p.highlight_strength > 0. {
        highlight = (specular(light, normal, 1.) - specular(light, flat, 1.)) * 0.5;
        let width = unit(p.highlight_ratio) * 5.;
        if width != 1. {
            let widened = (specular(light, normal, width) - specular(light, flat, width)) * 0.5;
            if width > 1. && widened > 0. {
                highlight = highlight.max(widened);
            }
            if width < 1. && highlight > 0. {
                highlight = highlight.min(widened.max(0.));
            }
        }
        highlight *= unit(p.highlight_strength);
    }
    let pit = deposit[2] / deposit[3].max(1e-6);
    let shade = (unit(dot(normal, light)) - light[2]) * 0.15 - (slope[0].hypot(slope[1]) * 0.12 * (1. + pressure) + pit * deposit[0] * 0.06).min(0.12);
    let mut rgb = [old[0], old[1], old[2]];
    if liquidity > 0. {
        let mut carried = rgb.map(|v| v * old[3]);
        let mut weight = old[3];
        for i in 1..=3 {
            let at = [-motion[0] * liquidity * i as f32 / 3., -motion[1] * liquidity * i as f32 / 3.];
            let neighbor = color_at(at);
            let w = neighbor[3] * material_at(at)[3];
            for (sum, v) in carried.iter_mut().zip(neighbor) {
                *sum += v * w;
            }
            weight += w;
        }
        rgb = mix3(rgb, carried.map(|v| v / weight.max(1e-6)), liquidity * 0.8);
    }
    let texture = local[1] / local[3].max(1e-6);
    if texture > 0. {
        let mut mean = [0.; 3];
        let mut weights = 0.;
        let mut coverage_mean = 0.;
        let mut texture_mean = 0.;
        let mut min_coverage = local[3];
        for at in [[-2., 0.], [2., 0.], [0., -2.], [0., 2.]] {
            let neighbor = color_at(at);
            let covered = material_at(at);
            let a = covered[3];
            texture_mean += covered[1] / a.max(1e-6) * 0.25;
            min_coverage = min_coverage.min(a);
            coverage_mean += a * 0.25;
            let w = neighbor[3] * a;
            for (sum, v) in mean.iter_mut().zip(neighbor) {
                *sum += v * w;
            }
            weights += w;
        }
        mean = if weights > 0. { mean.map(|v| v / weights) } else { rgb };
        let difference = std::array::from_fn::<_, 3, _>(|i| old[i] - mean[i]);
        let brightness = dot(difference, [0.2126, 0.7152, 0.0722]);
        let detail = (local[3] - coverage_mean) / (local[3] + coverage_mean).max(0.01) + (texture - texture_mean) / (texture + texture_mean).max(0.01);
        let structure = smooth(0.015, 0.15, detail.abs());
        let chroma = dot(difference.map(|v| v - brightness), difference.map(|v| v - brightness)).sqrt();
        let sign = if detail > 0. {
            1.
        } else if detail < 0. {
            -1.
        } else {
            0.
        };
        let contrast = (detail * 1.55 + brightness * structure * 1.1 + sign * chroma * structure * 0.45).clamp(-0.52, 0.42)
            * texture.max(0.32)
            * smooth(0.02, 0.3, min_coverage)
            * (1. - liquidity * 0.85);
        let tint = norm([old[0].max(0.001), old[1].max(0.001), old[2].max(0.001)]).map(|v| v * contrast * 0.18);
        rgb = if contrast < 0. { rgb.map(|v| v * (1. + contrast)) } else { std::array::from_fn(|i| unit(rgb[i] + (1. - rgb[i]) * contrast + tint[i])) };
    }
    let lit = rgb.map(|v| unit(v + v * shade + highlight));
    let pigment = rgb.map(|v| unit(v + v * shade + highlight.min(0.)));
    let shaded = mix3(pigment, lit, unit(p.highlight_brightness));
    let contribution = unit(local[3] / old[3].max(1e-6));
    let rgb = mix3([old[0], old[1], old[2]], shaded, contribution);
    [rgb[0], rgb[1], rgb[2], old[3]]
}

#[cfg(test)]
mod shade_tests {
    use super::*;
    #[test]
    fn flat_material_stays_flat_and_slope_shades_without_changing_alpha() {
        let rgb = [0.4, 0.5, 0.6, 0.75];
        let p = ResolvedPigment { enabled: true, highlight_strength: 1., highlight_brightness: 1., highlight_ratio: 0.2, ..Default::default() };
        let flat = shade(rgb, p, 0.5, [0.; 2], |_| [0.2, 0., 0., 0.5], |_| rgb);
        for (actual, expected) in flat.iter().zip(rgb) {
            assert!((actual - expected).abs() < 1e-6);
        }
        let slope = shade(rgb, p, 0.5, [0.; 2], |q| [0.2 + q[1] * 0.02, 0., 0., 0.5], |_| rgb);
        assert_eq!(slope[3], rgb[3]);
        assert!(slope[..3].iter().zip(rgb).any(|(a, b)| (a - b).abs() > 0.001));
        assert!(slope.iter().all(|v| v.is_finite()));
    }
    #[test]
    fn absent_coverage_cannot_shade_and_disabled_settings_are_identity() {
        let rgb = [0.2, 0.3, 0.4, 0.5];
        assert_eq!(shade(rgb, ResolvedPigment { enabled: true, ..Default::default() }, 1., [0.; 2], |_| [0.; 4], |_| rgb), rgb);
        assert_eq!(shade(rgb, ResolvedPigment::default(), 1., [1.; 2], |_| [1.; 4], |_| rgb), rgb);
    }
}
