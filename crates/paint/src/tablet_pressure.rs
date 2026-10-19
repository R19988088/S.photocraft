//! Causal tablet pressure and coordinate filtering shared by native input consumers.
use serde::{Deserialize, Serialize};

use crate::StrokePoint;

const PRESSURE_MAX: i64 = 65535;
const PRESSURE_FLOOR: i64 = 66;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TabletPressureSettings {
    pub enabled: bool,
    pub maximum_pressure: f64,
    pub dead_zone: f64,
    pub elasticity: f64,
    pub coordinate_window: usize,
    pub pressure_window: usize,
}

impl Default for TabletPressureSettings {
    fn default() -> Self {
        Self { enabled: cfg!(target_os = "macos"), maximum_pressure: 0.76, dead_zone: 0.0, elasticity: 2.1, coordinate_window: 2, pressure_window: 5 }
    }
}

impl TabletPressureSettings {
    pub fn clamped(self) -> Self {
        let defaults = Self::default();
        let maximum_pressure = finite(self.maximum_pressure, defaults.maximum_pressure).clamp(0.001, 1.0);
        Self {
            maximum_pressure,
            dead_zone: finite(self.dead_zone, defaults.dead_zone).clamp(0.0, (maximum_pressure - 1.0 / 65535.0).max(0.0)),
            elasticity: finite(self.elasticity, defaults.elasticity).clamp(0.1, 16.0),
            coordinate_window: self.coordinate_window.clamp(1, 16),
            pressure_window: self.pressure_window.clamp(1, 16),
            ..self
        }
    }
}

fn finite(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
}

#[derive(Clone, Debug, Default)]
struct Filter16 {
    history: [i64; 16],
    index: usize,
    count: usize,
}

impl Filter16 {
    fn apply(&mut self, input: i64, window: usize) -> i64 {
        let previous = (window.clamp(1, 16) - 1).min(self.count);
        let sum = (0..previous).fold(input, |sum, offset| sum + self.history.get((self.index + 15 - offset) & 15).copied().unwrap_or(0));
        let used = previous as i64 + 1;
        if let Some(slot) = self.history.get_mut(self.index) {
            *slot = input;
        }
        self.index = (self.index + 1) & 15;
        self.count = (self.count + 1).min(16);
        (sum + used - 1) / used
    }
}

#[derive(Clone, Copy, Debug)]
struct Node {
    x: i64,
    y: i64,
    pressure: i64,
    sample: StrokePoint,
}

impl Node {
    fn same_value(self, other: Self) -> bool {
        self.x == other.x && self.y == other.y && self.pressure == other.pressure
    }
}

#[derive(Clone, Copy, Debug)]
struct Curve {
    a: Node,
    b: Node,
    c: Node,
    outgoing: (i64, i64),
}

impl Curve {
    fn new(node: Node) -> Self {
        Self { a: node, b: node, c: node, outgoing: (node.x, node.y) }
    }

    fn append(&mut self, next: Node) -> Vec<StrokePoint> {
        let first_control = self.outgoing;
        self.a = self.b;
        self.b = self.c;
        self.c = next;
        let (incoming, outgoing) = handles(self.a, self.b, self.c);
        self.outgoing = outgoing;
        (1..=4)
            .map(|step| {
                let t = step as f64 / 4.0;
                let pressure = self.a.pressure + (self.b.pressure - self.a.pressure) * step / 4;
                let pressure = if pressure == 0 { 0 } else { pressure.max(PRESSURE_FLOOR) };
                let mix = |a: f32, b: f32| a + (b - a) * t as f32;
                StrokePoint {
                    x: cubic(self.a.x, first_control.0, incoming.0, self.b.x, t) as f64 / 256.0,
                    y: cubic(self.a.y, first_control.1, incoming.1, self.b.y, t) as f64 / 256.0,
                    pressure: pressure_value(pressure),
                    tilt_x: mix(self.a.sample.tilt_x, self.b.sample.tilt_x),
                    tilt_y: mix(self.a.sample.tilt_y, self.b.sample.tilt_y),
                    rotation: mix(self.a.sample.rotation, self.b.sample.rotation),
                    wheel: mix(self.a.sample.wheel, self.b.sample.wheel),
                    time: self.a.sample.time + (self.b.sample.time - self.a.sample.time) * t,
                }
            })
            .collect()
    }
}

fn cubic(a: i64, b: i64, c: i64, d: i64, t: f64) -> i64 {
    let s = 1.0 - t;
    (a as f64 * s * s * s + 3.0 * b as f64 * t * s * s + 3.0 * c as f64 * t * t * s + d as f64 * t * t * t) as i64
}

fn handles(a: Node, b: Node, c: Node) -> ((i64, i64), (i64, i64)) {
    let previous = ((a.x - b.x) as f64).hypot((a.y - b.y) as f64);
    let next = ((c.x - b.x) as f64).hypot((c.y - b.y) as f64);
    if previous == 0.0 || next == 0.0 {
        return ((b.x, b.y), (b.x, b.y));
    }
    let (tx, ty, incoming, outgoing) = if previous <= next {
        (
            b.x as f64 + (c.x - b.x) as f64 * previous / next - a.x as f64,
            b.y as f64 + (c.y - b.y) as f64 * previous / next - a.y as f64,
            0.2,
            0.2 * next / previous,
        )
    } else {
        (
            c.x as f64 - (b.x as f64 + (a.x - b.x) as f64 * next / previous),
            c.y as f64 - (b.y as f64 + (a.y - b.y) as f64 * next / previous),
            0.2 * previous / next,
            0.2,
        )
    };
    (((b.x as f64 - tx * incoming) as i64, (b.y as f64 - ty * incoming) as i64), ((b.x as f64 + tx * outgoing) as i64, (b.y as f64 + ty * outgoing) as i64))
}

fn pressure_value(pressure: i64) -> f32 {
    if pressure <= 0 {
        0.0
    } else if pressure <= PRESSURE_FLOOR {
        0.001
    } else {
        pressure as f32 / 65535.0
    }
}

fn coordinate(value: f64) -> i64 {
    (finite(value, 0.0) * 256.0).round().clamp(i32::MIN as f64, i32::MAX as f64) as i64
}

/// Outputs the first sample immediately; filter windows use only samples already received.
#[derive(Clone, Debug, Default)]
pub struct TabletPressureFilter {
    settings: Option<TabletPressureSettings>,
    x: Filter16,
    y: Filter16,
    pressure: Filter16,
    curve: Option<Curve>,
    last_raw: Option<StrokePoint>,
    last_emitted: Option<Node>,
    pressure_active: bool,
    pressure_activated: bool,
}

impl TabletPressureFilter {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn begin(&mut self, sample: StrokePoint, settings: TabletPressureSettings) -> Vec<StrokePoint> {
        self.reset();
        let settings = settings.clamped();
        self.settings = Some(settings);
        self.last_raw = Some(sample);
        if !settings.enabled {
            return vec![sample];
        }
        let node = self.filtered_node(sample, settings);
        self.pressure_activated = false;
        self.last_emitted = Some(node);
        if settings.coordinate_window > 1 {
            self.curve = Some(Curve::new(node));
        }
        vec![node.sample]
    }

    pub fn push(&mut self, sample: StrokePoint) -> Vec<StrokePoint> {
        let Some(settings) = self.settings else {
            return Vec::new();
        };
        self.last_raw = Some(sample);
        if !settings.enabled {
            return vec![sample];
        }
        let node = self.filtered_node(sample, settings);
        if self.pressure_activated || settings.coordinate_window == 1 {
            self.pressure_activated = false;
            self.last_emitted = Some(node);
            if settings.coordinate_window > 1 {
                self.curve = Some(Curve::new(node));
            }
            return vec![node.sample];
        }
        let Some(curve) = self.curve.as_mut() else {
            return Vec::new();
        };
        let candidates = curve.append(node);
        let mut output = Vec::with_capacity(4);
        for sample in candidates {
            let node = Node { x: coordinate(sample.x), y: coordinate(sample.y), pressure: (sample.pressure as f64 * 65535.0).round() as i64, sample };
            if self.last_emitted.is_some_and(|last| node.same_value(last)) {
                continue;
            }
            self.last_emitted = Some(node);
            output.push(sample);
        }
        output
    }

    pub fn finish(&mut self, sample: Option<StrokePoint>) -> Vec<StrokePoint> {
        let Some(settings) = self.settings else {
            return Vec::new();
        };
        let mut output = Vec::with_capacity(8);
        if let Some(sample) = sample {
            let same =
                self.last_raw.is_some_and(|last| last.x == sample.x && last.y == sample.y && last.pressure == sample.pressure && last.time == sample.time);
            if !same {
                output.extend(self.push(sample));
            }
        }
        if settings.enabled
            && settings.coordinate_window > 1
            && output.len() <= 4
            && let Some(last) = self.last_raw
        {
            output.extend(self.push(StrokePoint { pressure: 0.0, ..last }));
        }
        self.reset();
        output
    }

    fn filtered_node(&mut self, sample: StrokePoint, settings: TabletPressureSettings) -> Node {
        let maximum = (settings.maximum_pressure * 65535.0).round() as i64;
        let dead_zone = (settings.dead_zone * 65535.0).round() as i64;
        let maximum = maximum.max(dead_zone + 1);
        let raw = (finite(sample.pressure as f64, 0.0).clamp(0.0, 1.0) * 65535.0).round() as i64;
        let mapped = if raw <= dead_zone { 0 } else { ((raw.min(maximum) - dead_zone) * PRESSURE_MAX / (maximum - dead_zone)).min(PRESSURE_MAX) };
        let pressure = if mapped == 0 {
            self.pressure = Filter16::default();
            self.pressure_active = false;
            self.pressure_activated = false;
            0
        } else {
            let p = mapped as f64 / 65535.0;
            let rise = p.powf(0.92);
            let curved = (rise / (rise + settings.elasticity * (1.0 - p).powf(1.19)) * 65535.0).round() as i64;
            let filtered = self.pressure.apply(curved, settings.pressure_window);
            if self.pressure_active {
                filtered.max(PRESSURE_FLOOR)
            } else {
                self.pressure_active = true;
                self.pressure_activated = true;
                PRESSURE_FLOOR
            }
        };
        let x = self.x.apply(coordinate(sample.x), settings.coordinate_window);
        let y = self.y.apply(coordinate(sample.y), settings.coordinate_window);
        Node { x, y, pressure, sample: StrokePoint { x: x as f64 / 256.0, y: y as f64 / 256.0, pressure: pressure_value(pressure), ..sample } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> TabletPressureSettings {
        TabletPressureSettings { enabled: true, ..Default::default() }
    }

    #[test]
    fn first_sample_is_immediate_and_pressure_activation_restarts_after_release() {
        let mut filter = TabletPressureFilter::default();
        let first = filter.begin(StrokePoint::new(10.0, 20.0, 0.5), settings());
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].pressure, 0.001);
        filter.push(StrokePoint::new(11.0, 20.0, 0.0));
        let next = filter.push(StrokePoint::new(12.0, 20.0, 0.5));
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].pressure, 0.001);
    }

    #[test]
    fn pressure_window_is_causal_and_maximum_normalizes_to_full_pressure() {
        let mut filter = TabletPressureFilter::default();
        let settings = TabletPressureSettings { coordinate_window: 1, ..settings() };
        filter.begin(StrokePoint::new(0.0, 0.0, 0.76), settings);
        let next = filter.push(StrokePoint::new(2.0, 0.0, 0.76));
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].pressure, 1.0);
        assert_eq!(next[0].x, 2.0);
    }

    #[test]
    fn smoothing_preserves_pose_and_finishes_the_last_curve_segment() {
        let mut filter = TabletPressureFilter::default();
        filter.begin(StrokePoint::new(0.0, 0.0, 0.5), settings());
        filter.push(StrokePoint { x: 8.0, time: 10.0, tilt_x: 30.0, ..StrokePoint::new(0.0, 0.0, 0.5) });
        let end = filter.finish(None);
        assert_eq!(end.last().unwrap().x, 4.0);
        assert_eq!(end.last().unwrap().time, 10.0);
        assert_eq!(end.last().unwrap().tilt_x, 30.0);
        assert!(filter.push(StrokePoint::default()).is_empty());
    }

    #[test]
    fn disabled_filter_preserves_samples() {
        let mut filter = TabletPressureFilter::default();
        let point = StrokePoint::new(1.25, 2.5, 0.4);
        assert_eq!(filter.begin(point, TabletPressureSettings { enabled: false, ..settings() }), vec![point]);
        assert_eq!(filter.push(point), vec![point]);
        assert!(filter.finish(Some(point)).is_empty());
    }
}
