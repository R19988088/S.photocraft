//! Per-brush C3 canvas binding and its pigment response (independent of pigment height).
use crate::{AttributeMode, Pattern, pigment_material::SignedCurve};
use serde::{Deserialize, Serialize};

/// A standalone C3 canvas preset, independent of the brush library.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanvasPreset {
    pub id: String,
    pub name: String,
    pub material: CanvasMaterial,
    #[serde(default)]
    pub preserved_parameters: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CanvasMaterial {
    pub enabled: bool,
    pub preset_id: Option<String>,
    pub pattern: Pattern,
    /// Source percentage divided by 100: canvas pixels per texture pixel.
    pub scale: f32,
    pub strength: f32,
    pub blend_mode: AttributeMode,
    pub curve: Vec<[f32; 2]>,
    pub curve_lut: Vec<f32>,
}
impl Default for CanvasMaterial {
    fn default() -> Self {
        let curve = vec![[0., 0.], [0.33, 0.33], [0.66, 0.66], [1., 1.]];
        Self {
            enabled: true,
            preset_id: None,
            pattern: Pattern::default(),
            scale: 1.,
            strength: 1.,
            blend_mode: AttributeMode::Normal,
            curve_lut: curve_lut(&curve),
            curve,
        }
    }
}
impl CanvasMaterial {
    pub fn is_valid(&self) -> bool {
        self.scale.is_finite()
            && self.scale > 0.
            && self.strength.is_finite()
            && (0.0..=1.0).contains(&self.strength)
            && self.curve.iter().flatten().all(|v| v.is_finite())
            && self.curve_lut.len() == 256
            && self.curve_lut.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v))
    }
    pub fn set_curve(&mut self, points: Vec<[f32; 2]>) {
        self.curve_lut = curve_lut(&points);
        self.curve = points;
    }
    pub fn pigment_response(&self, pigment: [f32; 3], canvas_pixel: [f32; 2], opacity: f32, flow: f32) -> [f32; 3] {
        if !self.enabled {
            return pigment;
        }
        let Some(tile) = self.pattern.bitmap().filter(|t| t.is_valid()) else {
            return pigment;
        };
        let weight = (self.strength * (1. - opacity.clamp(0., 1.) * flow.clamp(0., 1.))).clamp(0., 1.);
        if weight <= 0. || self.scale <= 0. || self.curve_lut.len() != 256 {
            return pigment;
        }
        let [x, y] = canvas_pixel.map(|v| v / self.scale - 0.5);
        let at = |x: f32, y: f32| tile.get((x as i64).rem_euclid(i64::from(tile.width)) as u32, (y as i64).rem_euclid(i64::from(tile.height)) as u32);
        let (bx, by, fx, fy) = (x.floor(), y.floor(), x - x.floor(), y - y.floor());
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let v = lerp(lerp(at(bx, by), at(bx + 1., by), fx), lerp(at(bx, by + 1.), at(bx + 1., by + 1.), fx), fy).clamp(0., 1.) * 255.;
        let left = v.floor() as usize;
        let mapped = lerp(self.curve_lut[left], self.curve_lut[(left + 1).min(255)], v - v.floor());
        let blended = self.blend_mode.blend(pigment, [mapped; 3]);
        std::array::from_fn(|i| lerp(pigment[i], blended[i].clamp(0., 1.), weight))
    }
}
/// C3's 256-entry natural spline, or the seven-node pressure Bézier representation.
pub fn curve_lut(points: &[[f32; 2]]) -> Vec<f32> {
    if points.len() == 7 {
        let c = SignedCurve { points: std::array::from_fn(|i| points[i]), enabled: true };
        if c.is_valid() {
            return (0..256).map(|i| c.value(i as f32 / 255.).clamp(0., 1.)).collect();
        }
    }
    let mut p: Vec<_> = points.iter().filter(|p| p.iter().all(|v| v.is_finite())).map(|p| p.map(|v| v.clamp(0., 1.))).collect();
    p.sort_by(|a, b| a[0].total_cmp(&b[0]));
    let p = if p.len() < 2 {
        vec![[0., 0.], [0.33, 0.33], [0.66, 0.66], [1., 1.]]
    } else {
        let (a, b, first, last) = if p.len() == 2 { (p[0], p[1], 0., 1.) } else { (p[1], p[p.len() - 2], p[0][1], p[p.len() - 1][1]) };
        let x1 = a[0].clamp(0.04, 0.92);
        let x2 = b[0].clamp(x1 + 0.04, 0.96);
        vec![[0., first], [x1, a[1]], [x2, b[1]], [1., last]]
    };
    let p: Vec<_> = p.into_iter().map(|p| p.map(|v| (v * 255.).round())).collect();
    let n = p.len();
    let mut second = vec![0.; n];
    let mut work = vec![0.; n];
    for i in 1..n - 1 {
        let d = p[i + 1][0] - p[i - 1][0];
        let sig = (p[i][0] - p[i - 1][0]) / d;
        let v = sig * second[i - 1] + 2.;
        second[i] = (sig - 1.) / v;
        let next = (p[i + 1][1] - p[i][1]) / (p[i + 1][0] - p[i][0]).max(1.);
        let prev = (p[i][1] - p[i - 1][1]) / (p[i][0] - p[i - 1][0]).max(1.);
        work[i] = (6. * (next - prev) / d - sig * work[i - 1]) / v;
    }
    for i in (0..n - 1).rev() {
        second[i] = second[i] * second[i + 1] + work[i];
    }
    (0..256)
        .map(|i| {
            let x = i as f32;
            let lo = (0..n - 1).find(|&j| x <= p[j + 1][0]).unwrap_or(n - 2);
            let hi = lo + 1;
            let w = p[hi][0] - p[lo][0];
            let a = (p[hi][0] - x) / w;
            let b = (x - p[lo][0]) / w;
            ((a * p[lo][1] + b * p[hi][1] + ((a * a * a - a) * second[lo] + (b * b * b - b) * second[hi]) * w * w / 6.) / 255.).clamp(0., 1.)
        })
        .collect()
}
