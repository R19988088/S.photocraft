//! Sequential C3 transport, with independent state for each brush lane.
use photocraft_geom::Point;
use serde::{Deserialize, Serialize};

use crate::brush::{BrushSettings, Dynamic};
use crate::retouch::{Footprint, Region, Smudge};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MixAlgorithm {
    #[default]
    V1,
    V2,
    V3,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BrushMixSettings {
    pub enabled: bool,
    pub outputs_smudge: bool,
    pub smudge_enabled: bool,
    pub blur_enabled: bool,
    pub color_output: bool,
    pub strength: f32,
    pub strength_dynamic: Dynamic,
    pub mix_pressure_curve: Option<crate::pigment_material::SignedCurve>,
    pub algorithm: MixAlgorithm,
    pub length: f32,
    pub length_dynamic: Dynamic,
    pub length_pressure_inverted: bool,
    pub concentration: f32,
    pub water: f32,
    pub smooth: f32,
    pub quality: f32,
    pub output_strength: f32,
    pub blur_strength: f32,
    pub blur_dynamic: Dynamic,
    pub blur_pressure_curve: Option<crate::pigment_material::SignedCurve>,
    pub blur_pressure_inverted: bool,
    pub blur_color_target: Option<[f32; 4]>,
}

impl Default for BrushMixSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            outputs_smudge: false,
            smudge_enabled: true,
            blur_enabled: true,
            color_output: true,
            strength: 0.0,
            strength_dynamic: Dynamic::default(),
            mix_pressure_curve: None,
            algorithm: MixAlgorithm::V1,
            length: 0.5,
            length_dynamic: Dynamic::default(),
            length_pressure_inverted: false,
            concentration: 1.0,
            water: 0.0,
            smooth: 0.0,
            quality: 4.0,
            output_strength: 1.0,
            blur_strength: 0.0,
            blur_dynamic: Dynamic::default(),
            blur_pressure_curve: None,
            blur_pressure_inverted: false,
            blur_color_target: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Transport {
    settings: BrushMixSettings,
    alpha: Option<usize>,
    legacy: Smudge,
    previous: Option<Point>,
    carrier: Option<Region>,
    carrier_radius: f32,
    spacing: f32,
    pigment: f32,
}

impl Transport {
    pub fn new(brush: &BrushSettings, alpha: Option<usize>) -> Self {
        Self {
            settings: brush.mix_settings.clone(),
            alpha,
            legacy: Smudge::new(brush.mix_settings.strength, None, brush.size).with_alpha(alpha),
            previous: None,
            carrier: None,
            carrier_radius: 0.0,
            spacing: brush.spacing,
            pigment: brush.pigment_mix_strength,
        }
    }

    pub fn dab(&mut self, work: &mut Region, fp: &Footprint) {
        self.smudge_dab(work, fp);
        self.blur_dab(work, fp);
    }

    /// Smudge precedes colour deposition; blur follows it.
    pub(crate) fn smudge_dab(&mut self, work: &mut Region, fp: &Footprint) {
        if work.rect.is_empty() || work.ch == 0 {
            return;
        }
        let settings = &self.settings;
        if settings.enabled && settings.smudge_enabled && fp.dab.mix_strength > 0.0001 {
            match settings.algorithm {
                MixAlgorithm::V1 => {
                    self.legacy.strength = fp.dab.mix_strength;
                    self.legacy.dab(work, fp);
                }
                MixAlgorithm::V2 => self.sai_dab(work, fp),
                MixAlgorithm::V3 => self.pickup_dab(work, fp),
            }
        }
        self.previous = Some(fp.dab.center);
    }

    pub(crate) fn blur_dab(&self, work: &mut Region, fp: &Footprint) {
        if !work.rect.is_empty() && work.ch > 0 && self.settings.enabled && self.settings.blur_enabled && fp.dab.blur_strength > 0.0001 {
            self.blur(work, fp);
        }
    }

    fn pickup_dab(&self, work: &mut Region, fp: &Footprint) {
        let center = fp.dab.center;
        let previous = self.previous.unwrap_or(center);
        let (dx, dy) = (center.x - previous.x, center.y - previous.y);
        let distance = dx.hypot(dy);
        let push = distance.min(f64::from(2.0 * fp.dab.radius * self.spacing.max(0.01))) * f64::from((0.18 + fp.dab.mix_length * 1.45).min(1.0));
        let (ox, oy) = if distance > 0.0001 { (-dx / distance * push, -dy / distance * push) } else { (0.0, 0.0) };
        let snapshot = work.clone();
        let rect = fp.rect.intersect(&work.rect);
        let alpha = self.alpha.filter(|a| *a < work.ch);
        for y in rect.y0..rect.y1 {
            for x in rect.x0..rect.x1 {
                let source = sample(&snapshot, f64::from(x) + ox, f64::from(y) + oy);
                let coverage = (fp.at(x, y) * fp.dab.mix_strength).clamp(0.0, 1.0);
                let destination = work.px_mut(x, y);
                let source_alpha = alpha.map_or(1.0, |a| source[a]);
                let old_alpha = alpha.map_or(1.0, |a| destination[a]);
                let carried_alpha = coverage.min(source_alpha);
                let result_alpha = old_alpha.max(carried_alpha + old_alpha * (1.0 - carried_alpha));
                let weight = (coverage * source_alpha).clamp(0.0, 1.0);
                let old = destination.to_vec();
                for c in 0..destination.len() {
                    if Some(c) != alpha {
                        destination[c] += (source[c] - destination[c]) * weight;
                    }
                }
                if alpha == Some(3) && destination.len() == 4 && self.pigment > 0.0 {
                    let normal = [destination[0], destination[1], destination[2], result_alpha];
                    let result = crate::pigment::paint(
                        normal,
                        [old[0], old[1], old[2], old_alpha],
                        [source[0], source[1], source[2], source_alpha],
                        weight,
                        self.pigment,
                    );
                    destination.copy_from_slice(&result);
                }
                if let Some(a) = alpha {
                    destination[a] = result_alpha;
                }
            }
        }
    }

    fn sai_dab(&mut self, work: &mut Region, fp: &Footprint) {
        let radius = fp.dab.radius.max(0.5);
        let side = ((radius * 2.0).ceil() as i32).max(1);
        let local = photocraft_geom::Rect::new(0, 0, side, side);
        let mut next = Region::new(local, work.ch);
        let alpha = self.alpha.filter(|a| *a < work.ch);
        let center = fp.dab.center;
        let spatial_step = self.previous.map_or(0.0, |p| (center.x - p.x).hypot(center.y - p.y) as f32 / (radius * 2.0).max(1.0));
        let stretch = fp.dab.mix_length.clamp(0.0, 1.0);
        let retention = sai_stretch_retention(stretch, self.settings.quality, spatial_step);
        let dynamic = if self.settings.strength > 0.0001 { fp.dab.mix_strength / self.settings.strength } else { 0.0 };
        for y in 0..side {
            for x in 0..side {
                let (dx, dy) = (f64::from(x) + 0.5 - f64::from(side) * 0.5, f64::from(y) + 0.5 - f64::from(side) * 0.5);
                let sampled = sample(work, center.x + dx - 0.5, center.y + dy - 0.5);
                let mut sampled = premultiplied(&sampled, alpha);
                let retained = self.carrier.as_ref().map_or_else(
                    || sampled.clone(),
                    |c| {
                        let scale = self.carrier_radius / radius;
                        sample(c, dx * f64::from(scale) + f64::from(c.rect.width()) * 0.5 - 0.5, dy * f64::from(scale) + f64::from(c.rect.height()) * 0.5 - 0.5)
                    },
                );
                let a = alpha.map_or(1.0, |a| sampled[a]);
                let sample_alpha16 = (a.clamp(0.0, 1.0) * 65535.0).floor();
                if sample_alpha16 == 0.0 {
                    let old_a = alpha.map_or(1.0, |a| retained[a]);
                    for c in 0..sampled.len() {
                        if Some(c) != alpha {
                            sampled[c] = if old_a > 0.0 { retained[c] / old_a * a } else { 0.0 };
                        }
                    }
                }
                let base = if sample_alpha16 > 0.0 { (1.0 - self.settings.strength).powi(2) } else { (1.0 - self.settings.water).powi(2) };
                let effective = (base * dynamic).clamp(0.0, 1.0) * (1.0 - retention);
                let pixel = next.px_mut(x, y);
                for c in 0..pixel.len() {
                    pixel[c] = (retained[c] + (sampled[c] - retained[c]) * effective).clamp(0.0, 1.0);
                }
            }
        }
        let rect = fp.rect.intersect(&work.rect);
        for y in rect.y0..rect.y1 {
            for x in rect.x0..rect.x1 {
                let local_x = f64::from(x) + 0.5 - center.x + f64::from(side) * 0.5 - 0.5;
                let local_y = f64::from(y) + 0.5 - center.y + f64::from(side) * 0.5 - 0.5;
                let carried = sample(&next, local_x, local_y);
                let coverage = (fp.at(x, y) * fp.dab.mix_strength * self.settings.concentration * self.settings.output_strength).clamp(0.0, 1.0);
                over_carried(work.px_mut(x, y), &carried, alpha, coverage, self.pigment);
            }
        }
        self.carrier = Some(next);
        self.carrier_radius = radius;
    }

    fn blur(&self, work: &mut Region, fp: &Footprint) {
        let snapshot = work.clone();
        let rect = fp.rect.intersect(&work.rect);
        for y in rect.y0..rect.y1 {
            for x in rect.x0..rect.x1 {
                let alpha = self.alpha.filter(|a| *a < work.ch);
                if let Some(target) = self.settings.blur_color_target
                    && work.ch >= 3
                    && target[3] >= 0.0
                {
                    let p = snapshot.px(x, y);
                    let distance = ((p[0] - target[0]).powi(2) + (p[1] - target[1]).powi(2) + (p[2] - target[2]).powi(2)).sqrt();
                    if alpha.is_some_and(|a| p[a] <= 0.0001) || distance > target[3] {
                        continue;
                    }
                }
                let radius = f64::from((fp.dab.radius * (0.18 + fp.dab.blur_strength * 0.82)).clamp(1.5, 127.0));
                let offsets = [
                    (0., 0., 0.18),
                    (0., -1., 0.14),
                    (0., 1., 0.14),
                    (-1., 0., 0.14),
                    (1., 0., 0.14),
                    (-0.7, -0.7, 0.10),
                    (0.7, -0.7, 0.10),
                    (-0.7, 0.7, 0.10),
                    (0.7, 0.7, 0.10),
                ];
                let mut sum = vec![0.0; work.ch];
                let mut total = 0.0;
                for (ox, oy, weight) in offsets {
                    let pixel = premultiplied(&sample(&snapshot, f64::from(x) + ox * radius, f64::from(y) + oy * radius), alpha);
                    for (s, v) in sum.iter_mut().zip(pixel) {
                        *s += v * weight;
                    }
                    total += weight;
                }
                for v in &mut sum {
                    *v /= total;
                }
                let sampled_alpha = alpha.map_or(1.0, |a| sum[a]);
                if sampled_alpha <= 0.0 {
                    continue;
                }
                let weight = (fp.at(x, y) * fp.dab.blur_strength).clamp(0.0, 1.0);
                let old = snapshot.px(x, y);
                let old_alpha = alpha.map_or(1.0, |a| old[a]);
                let result_alpha = old_alpha + (old_alpha.max(sampled_alpha) - old_alpha) * weight;
                for c in 0..sum.len() {
                    sum[c] = if Some(c) == alpha {
                        result_alpha
                    } else {
                        (old[c] * old_alpha * (1.0 - weight) + sum[c] / sampled_alpha * old_alpha.max(sampled_alpha) * weight) / result_alpha.max(1e-6)
                    };
                }
                let out = work.px_mut(x, y);
                out.copy_from_slice(&sum);
            }
        }
    }
}

pub fn sai_stretch_retention(stretch: f32, quality: f32, spatial_step: f32) -> f32 {
    if spatial_step <= 0.0 {
        return 1.0;
    }
    (1.0 - (1.0 - stretch.clamp(0.0, 1.0)).powi(2)).powf(32.0 * spatial_step / quality.max(1.0))
}

fn sample(region: &Region, x: f64, y: f64) -> Vec<f32> {
    let x = x.clamp(f64::from(region.rect.x0), f64::from(region.rect.x1 - 1));
    let y = y.clamp(f64::from(region.rect.y0), f64::from(region.rect.y1 - 1));
    let (ix, iy) = (x.floor() as i32, y.floor() as i32);
    let (jx, jy) = ((ix + 1).min(region.rect.x1 - 1), (iy + 1).min(region.rect.y1 - 1));
    let (fx, fy) = ((x - x.floor()) as f32, (y - y.floor()) as f32);
    let (a, b, c, d) = (region.px(ix, iy), region.px(jx, iy), region.px(ix, jy), region.px(jx, jy));
    (0..region.ch).map(|n| (a[n] + (b[n] - a[n]) * fx) * (1.0 - fy) + (c[n] + (d[n] - c[n]) * fx) * fy).collect()
}

fn premultiplied(pixel: &[f32], alpha: Option<usize>) -> Vec<f32> {
    let mut out = pixel.to_vec();
    if let Some(a) = alpha {
        for (c, v) in out.iter_mut().enumerate() {
            if c != a {
                *v *= pixel[a];
            }
        }
    }
    out
}

fn over_carried(destination: &mut [f32], carried: &[f32], alpha: Option<usize>, coverage: f32, pigment: f32) {
    let da = alpha.map_or(1.0, |a| destination[a]);
    let sa = alpha.map_or(1.0, |a| carried[a]) * coverage;
    if sa <= 0.0 {
        return;
    }
    let result_a = sa + da * (1.0 - sa);
    let old = destination.to_vec();
    for c in 0..destination.len() {
        if Some(c) != alpha {
            destination[c] = (carried[c] * coverage + destination[c] * da * (1.0 - sa)) / result_a.max(1e-6);
        }
    }
    if alpha == Some(3) && destination.len() == 4 && pigment > 0.0 {
        let source_a = carried[3];
        let source = [carried[0] / source_a, carried[1] / source_a, carried[2] / source_a, source_a];
        let result = crate::pigment::paint(
            [destination[0], destination[1], destination[2], result_a],
            [old[0], old[1], old[2], da],
            source,
            sa / result_a.max(1e-6),
            pigment,
        );
        destination.copy_from_slice(&result);
    }
    if let Some(a) = alpha {
        destination[a] = result_a;
    }
}

/// C3's absolute two-segment pressure curve; mouse strokes use its maximum.
pub fn pressure_value(curve: Option<&crate::pigment_material::SignedCurve>, strength: f32, pressure: f32, is_pen: bool) -> f32 {
    if curve.is_some_and(|c| !c.enabled) {
        return 0.0;
    }
    let bell = crate::pigment_material::SignedCurve {
        points: [[0.0, 0.0], [0.1, 0.0], [0.2, strength], [0.3, strength], [0.53, strength], [0.77, 0.0], [1.0, 0.0]],
        enabled: true,
    };
    let curve = curve.unwrap_or(&bell);
    if is_pen { curve.value(pressure).clamp(0.0, 1.0) } else { curve_maximum(curve).clamp(0.0, 1.0) }
}

fn curve_maximum(curve: &crate::pigment_material::SignedCurve) -> f32 {
    let mut maximum = curve.points[0][1].max(curve.points[3][1]).max(curve.points[6][1]);
    for start in [0, 3] {
        let [a, b, c, d] = [curve.points[start][1], curve.points[start + 1][1], curve.points[start + 2][1], curve.points[start + 3][1]];
        let q = -a + 3.0 * b - 3.0 * c + d;
        let l = 2.0 * (a - 2.0 * b + c);
        let k = b - a;
        let discriminant = l * l - 4.0 * q * k;
        let roots = if q.abs() < 1e-12 {
            if l.abs() < 1e-12 { [f32::NAN; 2] } else { [-k / l, f32::NAN] }
        } else if discriminant < 0.0 {
            [f32::NAN; 2]
        } else {
            [(-l - discriminant.sqrt()) / (2.0 * q), (-l + discriminant.sqrt()) / (2.0 * q)]
        };
        for t in roots {
            if t > 0.0 && t < 1.0 {
                let u = 1.0 - t;
                maximum = maximum.max(u * u * u * a + 3.0 * u * u * t * b + 3.0 * u * t * t * c + t * t * t * d);
            }
        }
    }
    maximum
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Control, Dab, Dynamic};
    use photocraft_geom::Rect;

    fn footprint(x: f64, index: usize) -> Footprint {
        let dab = Dab { mix_strength: 0.6, mix_length: 0.5, blur_strength: 0.0, ..Dab::round(Point::new(x, 2.0), 1.0, 1.0) };
        crate::retouch::footprint(&dab, index, 1.0)
    }
    fn paint_region() -> Region {
        let mut r = Region::new(Rect::new(0, 0, 12, 5), 4);
        for y in 0..5 {
            for x in 0..4 {
                r.px_mut(x, y).copy_from_slice(&[0., 0., 1., 1.]);
            }
        }
        r
    }
    #[test]
    fn v1_is_the_existing_smudge_and_v2_carries_color_without_erasing() {
        let b = BrushSettings { mix_settings: BrushMixSettings { enabled: true, strength: 0.6, ..Default::default() }, ..Default::default() };
        let mut transport = Transport::new(&b, Some(3));
        let mut original = Smudge::new(0.6, None, b.size).with_alpha(Some(3));
        let mut a = paint_region();
        let mut expected = a.clone();
        for (i, x) in [2., 5., 8.].into_iter().enumerate() {
            let fp = footprint(x, i);
            transport.dab(&mut a, &fp);
            original.dab(&mut expected, &fp);
        }
        assert_eq!(a, expected);
        let mut b = b;
        b.mix_settings.algorithm = MixAlgorithm::V2;
        let mut transport = Transport::new(&b, Some(3));
        let mut a = paint_region();
        transport.dab(&mut a, &footprint(2., 0));
        transport.dab(&mut a, &footprint(5., 1));
        assert!(a.px(4, 2)[3] > 0.);
        assert_eq!(a.px(2, 2)[3], 1.);
        assert!(a.px(4, 2)[2] > 0.99);
    }
    #[test]
    fn mixing_master_and_internal_switches_gate_effects_without_resetting_strengths() {
        let mut b = BrushSettings { mix_settings: BrushMixSettings { strength: 0.8, blur_strength: 1.0, ..Default::default() }, ..Default::default() };
        let mut fp = footprint(4., 0);
        fp.dab.mix_strength = 0.8;
        fp.dab.blur_strength = 1.;
        let original = paint_region();
        let mut disabled = original.clone();
        Transport::new(&b, Some(3)).dab(&mut disabled, &fp);
        assert_eq!(disabled, original, "master off gates both effects");
        b.mix_settings.enabled = true;
        b.mix_settings.smudge_enabled = false;
        b.mix_settings.blur_enabled = false;
        Transport::new(&b, Some(3)).dab(&mut disabled, &fp);
        assert_eq!(disabled, original, "both internal switches off preserve pixels");
        b.mix_settings.blur_enabled = true;
        Transport::new(&b, Some(3)).dab(&mut disabled, &fp);
        assert_ne!(disabled, original, "blur operates independently of smudge");
        assert_eq!((b.mix_settings.strength, b.mix_settings.blur_strength), (0.8, 1.0));
    }

    #[test]
    fn c3_pressure_curves_are_absolute_and_nonmonotone() {
        assert!((pressure_value(None, 0.7, 1., false) - 0.7).abs() < 1e-6);
        assert!(pressure_value(None, 0.7, 0.3, true) > 0.69);
        assert_eq!(pressure_value(None, 0.7, 1., true), 0.);
        assert_eq!(sai_stretch_retention(0., 4., 1.), 0.);
        assert_eq!(sai_stretch_retention(1., 4., 1.), 1.);
        let b = BrushSettings {
            mix_settings: BrushMixSettings { strength: 0.7, strength_dynamic: Dynamic::controlled(Control::PenPressure), ..Default::default() },
            ..Default::default()
        };
        assert_eq!(b.mix_settings.strength, 0.7);
    }
    #[test]
    fn blur_averages_premultiplied_color_and_obeys_target_tolerance() {
        let mut b = BrushSettings { mix_settings: BrushMixSettings { enabled: true, blur_strength: 1., ..Default::default() }, ..Default::default() };
        let mut fp = footprint(4., 0);
        fp.dab.blur_strength = 1.;
        let mut a = paint_region();
        let original = a.clone();
        b.mix_settings.blur_color_target = Some([1., 0., 0., 0.01]);
        Transport::new(&b, Some(3)).dab(&mut a, &fp);
        assert_eq!(a, original);
        b.mix_settings.blur_color_target = None;
        Transport::new(&b, Some(3)).dab(&mut a, &fp);
        assert!(a.px(4, 2)[3] > 0.);
        assert!((a.px(4, 2)[2] - 1.).abs() < 1e-6);
    }
}
