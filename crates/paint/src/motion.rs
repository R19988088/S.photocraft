//! C3 input motion and turn branches; each brush lane keeps its own state.
use crate::StrokePoint;
use crate::brush::Control;
use photocraft_geom::Point;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GuideMode {
    #[default]
    Off,
    Anchor,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MotionSettings {
    pub enabled: bool,
    pub guide_mode: GuideMode,
    pub anchor_point_x: f32,
    pub anchor_point_y: f32,
    pub anchor_delay: f32,
    pub anchor_smooth: f32,
    pub smoothing: f32,
    pub tilt_delay: f32,
    pub anchor_delay_pressure_enabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InertiaSettings {
    pub enabled: bool,
    pub amount: f32,
    pub min_distance: f32,
    pub control: Control,
}

#[derive(Clone, Debug, Default)]
pub struct Motion {
    previous: Option<Point>,
    accumulated: Point,
    direction: Option<Point>,
    orientation: Option<f64>,
    last_raw: f64,
    outliers: u32,
    pending_sin: f64,
    pending_cos: f64,
    pending_count: u32,
}

fn angle_delta(v: f64) -> f64 {
    (v + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
}

impl Motion {
    pub fn resolve(&mut self, mut p: StrokePoint, c: &MotionSettings, diameter: f32) -> StrokePoint {
        if !c.enabled {
            return p;
        }
        let delay = f64::from(c.tilt_delay.clamp(0., 1.)).powf(0.55);
        let magnitude = p.tilt_x.hypot(p.tilt_y);
        let raw = f64::from(p.tilt_y.atan2(p.tilt_x)).rem_euclid(std::f64::consts::TAU);
        if let Some(mut rotation) = self.orientation {
            if delay > 0.000001 {
                let jump = angle_delta(raw - self.last_raw);
                self.last_raw = raw;
                let mut suppress = false;
                if delay >= 0.55 && jump.abs() > (145. - delay.powf(1.8) * 108.).to_radians() {
                    self.outliers += 1;
                    suppress = self.outliers < 3;
                } else {
                    self.outliers = 0;
                }
                if delay >= 0.75 && !suppress {
                    self.pending_sin += raw.sin();
                    self.pending_cos += raw.cos();
                    self.pending_count += 1;
                    let ratio = ((delay - 0.75) / 0.25).clamp(0., 1.);
                    let interval = (3. + ratio.powf(2.2) * 45.).round().max(3.) as u32;
                    if self.pending_count >= interval {
                        let average = self.pending_sin.atan2(self.pending_cos);
                        let delta = angle_delta(average - rotation);
                        if delta.abs() > (1.8 + delay.powf(2.1) * 18.).to_radians() {
                            let step = (delta * (0.12 * (1. - delay).powf(2.4)).max(0.01))
                                .clamp(-(0.45 + (1. - delay) * 3.2).to_radians(), (0.45 + (1. - delay) * 3.2).to_radians());
                            rotation = (rotation + step).rem_euclid(std::f64::consts::TAU);
                        }
                        self.pending_sin = 0.;
                        self.pending_cos = 0.;
                        self.pending_count = 0;
                    }
                } else if !suppress {
                    let delta = angle_delta(raw - rotation);
                    if delta.abs() > (0.8 + delay.powf(1.9) * 12.5).to_radians() {
                        let max_step = (0.35 + (1. - delay) * 5.8).to_radians();
                        rotation = (rotation + (delta * (0.22 * (1. - delay).powi(2)).max(0.008)).clamp(-max_step, max_step)).rem_euclid(std::f64::consts::TAU);
                    }
                }
                p.tilt_x = magnitude * rotation.cos() as f32;
                p.tilt_y = magnitude * rotation.sin() as f32;
                self.orientation = Some(rotation);
            } else {
                self.orientation = Some(raw);
                self.last_raw = raw;
            }
        } else {
            self.orientation = Some(raw);
            self.last_raw = raw;
        }
        let point = Point::new(p.x, p.y);
        let resolved = if c.guide_mode == GuideMode::Anchor {
            let delay = f64::from(c.anchor_delay.clamp(0., 1.)) * if c.anchor_delay_pressure_enabled { 1. - f64::from(p.pressure.clamp(0., 1.)) } else { 1. };
            let radius = f64::from(diameter.max(0.1)) * 0.5;
            let previous = self.previous.unwrap_or(point);
            self.previous = Some(point);
            self.accumulated.x += point.x - previous.x;
            self.accumulated.y += point.y - previous.y;
            let distance = self.accumulated.x.hypot(self.accumulated.y);
            if distance >= (radius * 0.24).clamp(0.75, 3.) {
                self.direction = Some(Point::new(self.accumulated.x / distance, self.accumulated.y / distance));
                self.accumulated = Point::new(0., 0.);
            }
            let target = self.direction.map_or(Point::new(0., 0.), |d| Point::new(-d.x * radius * delay * 1.25, -d.y * radius * delay * 1.25));
            Point::new(
                point.x - f64::from(c.anchor_point_x.clamp(-1., 1.)) * radius + target.x,
                point.y - f64::from(c.anchor_point_y.clamp(-1., 1.)) * radius + target.y,
            )
        } else {
            point
        };
        p.x = resolved.x;
        p.y = resolved.y;
        p
    }
}

#[derive(Clone, Debug, Default)]
pub struct Inertia {
    previous: Option<StrokePoint>,
    before: Option<Point>,
    distance: f64,
    cooldown: f64,
}
impl Inertia {
    pub fn append(&mut self, p: StrokePoint, c: &InertiaSettings, diameter: f32, spacing: f32, control: f32) -> Vec<(StrokePoint, f32, f32)> {
        let Some(previous) = self.previous else {
            self.previous = Some(p);
            self.before = Some(Point::new(p.x, p.y));
            return Vec::new();
        };
        let raw = self.before.unwrap_or(Point::new(previous.x, previous.y));
        let (dx, dy) = (p.x - previous.x, p.y - previous.y);
        let travel = dx.hypot(dy);
        if !travel.is_finite() {
            return Vec::new();
        }
        if travel <= 0.000001 {
            self.previous = Some(p);
            return Vec::new();
        }
        let starting = self.distance;
        self.distance = (self.distance + travel).min(1e9);
        self.cooldown = (self.cooldown - travel).max(0.);
        self.before = Some(Point::new(previous.x, previous.y));
        self.previous = Some(p);
        if !c.enabled || c.amount <= 0. {
            return Vec::new();
        }
        let (old_x, old_y) = (previous.x - raw.x, previous.y - raw.y);
        let old_length = old_x.hypot(old_y);
        let step = f64::from(spacing.clamp(1., 1e6));
        if old_length <= 0.000001 || self.distance < f64::from(c.min_distance.max(0.)).max(step * 1.25) || self.cooldown > 0.000001 {
            return Vec::new();
        }
        let (ux, uy) = (old_x / old_length, old_y / old_length);
        if ux * dx / travel + uy * dy / travel >= 0.3 {
            return Vec::new();
        }
        let weight = if c.control == Control::Off { 1. } else { f64::from(control.clamp(0., 1.)) };
        let distance = (step * 8.).max(f64::from(diameter.max(0.001)) * 10.) * f64::from(c.amount.clamp(0., 1.)) * weight;
        if distance <= 0.000001 || previous.pressure <= 0.0001 {
            return Vec::new();
        }
        self.cooldown = distance;
        let count = (distance / step).ceil().min(50000.) as usize;
        (1..=count)
            .map(|i| {
                let offset = i as f64 * step;
                let scale = (1. - 0.6 * (offset / distance).min(1.)) as f32;
                (
                    StrokePoint { x: previous.x + ux * offset, y: previous.y + uy * offset, pressure: previous.pressure.clamp(0., 1.) * scale, ..previous },
                    (-uy).atan2(ux) as f32,
                    (starting + offset) as f32,
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn motion_offsets_are_independent_and_dont_append_a_tail() {
        let c =
            MotionSettings { enabled: true, guide_mode: GuideMode::Anchor, anchor_point_x: 1., anchor_delay: 0.8, anchor_smooth: 0.5, ..Default::default() };
        let mut a = Motion::default();
        let mut b = Motion::default();
        let p = StrokePoint::new(50., 50., 1.);
        let first = a.resolve(p, &c, 20.);
        assert_eq!((first.x, first.y), (40., 50.));
        let q = StrokePoint::new(80., 50., 1.);
        let moved = a.resolve(q, &c, 20.);
        assert!(moved.x < 70.);
        assert_eq!(b.resolve(q, &MotionSettings::default(), 20.), q);
    }
    #[test]
    fn legacy_motion_smoothing_does_not_filter_coordinates() {
        let mut motion = Motion::default();
        let settings = MotionSettings { enabled: true, smoothing: 1.0, anchor_smooth: 1.0, ..Default::default() };
        for (x, y) in [(10.0, 20.0), (150.0, 40.0), (40.0, 200.0)] {
            let point = StrokePoint::new(x, y, 0.7);
            assert_eq!(motion.resolve(point, &settings, 20.0), point);
        }
        let settings = MotionSettings { guide_mode: GuideMode::Anchor, ..settings };
        let point = StrokePoint::new(300.0, 100.0, 0.7);
        assert_eq!(motion.resolve(point, &settings, 20.0), point);
    }

    #[test]
    fn inertia_only_branches_at_turns_and_has_a_distance_cooldown() {
        let c = InertiaSettings { enabled: true, amount: 0.5, ..Default::default() };
        let mut state = Inertia::default();
        assert!(state.append(StrokePoint::new(0., 0., 1.), &c, 10., 2., 1.).is_empty());
        assert!(state.append(StrokePoint::new(20., 0., 1.), &c, 10., 2., 1.).is_empty());
        let branch = state.append(StrokePoint::new(20., 20., 1.), &c, 10., 2., 1.);
        assert_eq!(branch.len(), 25);
        assert!(branch.iter().all(|(p, _, _)| p.y == 0. && p.x > 20.));
        assert!((branch.last().unwrap().0.pressure - 0.4).abs() < 1e-6);
        assert!(state.append(StrokePoint::new(10., 20., 1.), &c, 10., 2., 1.).is_empty());
    }
}
