//! From input points to dabs: smoothing, brush pose, path walking (spacing, airbrush build-up) and
//! the per-dab dynamics (shape, scattering, transfer, colour, texture depth).
//!
//! Everything is incremental (feed points in any chunking, get the same dabs) and deterministic:
//! jitter comes from [`crate::rng`] keyed by the stroke seed and the step/dab index.

use photocraft_geom::Point;

use crate::brush::{BrushSettings, Control, Dynamic, Smoothing};
use crate::rng::{rand_signed, rand01, stream};
use crate::{Dab, StrokePoint};

/// Maximum pulled-string length at 100 % smoothing, in screen pixels.
pub const MAX_STRING_PX: f64 = 100.0;
/// Stroke time over which exponential smoothing moves its fraction of the way to the pointer.
pub const SMOOTHING_STEP_MS: f64 = 16.0;

/// With Spacing unchecked, one dab per this many milliseconds of stroke time, so faster strokes
/// space their dabs further apart (Photoshop: "the speed of the cursor determines the spacing").
pub const SPEED_SPACING_MS: f64 = 8.0;

/// Longest pixel-grid segment (in pixels) before falling back to distance spacing.
const MAX_GRID_STEPS: f64 = 1e7;

/// Densest speed spacing, in pixels between dabs (a slow, long stroke never floods the buffer).
const MIN_SPEED_STEP: f64 = 0.5;
/// Maximum airbrush catch-up dabs for one input segment.
const MAX_AIRBRUSH_DABS_PER_SEGMENT: usize = 512;

/// The dab centre of the pixel-grid cell holding `v` (see [`PathWalker::pixel_grid`]): the
/// pixel's centre for `offset` 0.5, the nearest pixel corner for 0. A value within rounding
/// noise of a cell boundary counts as on it, so a line lands on the same pixels both ways.
fn grid_cell(v: f64, offset: f64) -> f64 {
    let w = v - offset + 0.5;
    let w = if (w - w.round()).abs() < 1e-7 { w.round() } else { w };
    w.floor() + offset
}

#[inline]
fn lerp_pt(a: &StrokePoint, b: &StrokePoint, f: f64) -> StrokePoint {
    let ff = f as f32;
    StrokePoint {
        is_pen: a.is_pen || b.is_pen,
        x: a.x + (b.x - a.x) * f,
        y: a.y + (b.y - a.y) * f,
        pressure: a.pressure + (b.pressure - a.pressure) * ff,
        tilt_x: a.tilt_x + (b.tilt_x - a.tilt_x) * ff,
        tilt_y: a.tilt_y + (b.tilt_y - a.tilt_y) * ff,
        rotation: a.rotation + (b.rotation - a.rotation) * ff,
        wheel: a.wheel + (b.wheel - a.wheel) * ff,
        time: a.time + (b.time - a.time) * f,
    }
}

// ---------------------------------------------------------------------------
// Smoothing
// ---------------------------------------------------------------------------

/// The smoothed brush moved `f` of the way from `brush` towards the pointer `p`. Its pose
/// (pressure, tilt, rotation, wheel) moves with it, so the lagging brush paints with the pose the
/// pen had along the way, not the pointer's newest one. Taking the pointer's pressure made a fast
/// stroke's lift thin the brush while it still trailed far behind, and the end catch-up a hairline
/// (#5713). Time is the pointer's.
fn pull(brush: &StrokePoint, p: &StrokePoint, f: f64) -> StrokePoint {
    let ff = f as f32;
    // Barrel rotation takes the shorter way round.
    let dr = (p.rotation - brush.rotation + 540.0).rem_euclid(360.0) - 180.0;
    StrokePoint {
        is_pen: brush.is_pen || p.is_pen,
        x: brush.x + (p.x - brush.x) * f,
        y: brush.y + (p.y - brush.y) * f,
        pressure: brush.pressure + (p.pressure - brush.pressure) * ff,
        tilt_x: brush.tilt_x + (p.tilt_x - brush.tilt_x) * ff,
        tilt_y: brush.tilt_y + (p.tilt_y - brush.tilt_y) * ff,
        rotation: (brush.rotation + dr * ff).rem_euclid(360.0),
        wheel: brush.wheel + (p.wheel - brush.wheel) * ff,
        time: p.time,
    }
}

/// Incremental stroke smoothing: exponential (with optional time-based catch-up) or pulled string.
#[derive(Clone, Debug)]
pub struct Smoother {
    cfg: Smoothing,
    zoom: f64,
    pos: Option<StrokePoint>,
    last_input: Option<StrokePoint>,
}

impl Smoother {
    /// `zoom` is the view zoom (only used with `adjust_for_zoom`).
    pub fn new(cfg: &Smoothing, zoom: f32) -> Self {
        Self { cfg: cfg.clone(), zoom: f64::from(zoom.max(0.01)), pos: None, last_input: None }
    }

    fn active(&self) -> bool {
        self.cfg.enabled && self.cfg.amount > 0.0
    }

    /// Feed one input point; returns the brush positions to paint through.
    pub fn push(&mut self, p: StrokePoint, out: &mut Vec<StrokePoint>) {
        let prev_input = self.last_input.replace(p);
        if !self.active() {
            out.push(p);
            return;
        }
        let Some(pos) = self.pos else {
            self.pos = Some(p);
            out.push(p);
            return;
        };
        let amount = f64::from(self.cfg.amount.clamp(0.0, 1.0));
        if self.cfg.pulled_string {
            let mut len = amount * MAX_STRING_PX;
            if self.cfg.adjust_for_zoom {
                len /= self.zoom;
            }
            let (dx, dy) = (p.x - pos.x, p.y - pos.y);
            let d = (dx * dx + dy * dy).sqrt();
            if d > len {
                let k = (d - len) / d;
                let np = pull(&pos, &p, k);
                self.pos = Some(np);
                out.push(np);
            }
            return;
        }
        // Exponential: every SMOOTHING_STEP_MS of stroke time moves `a` of the way towards the
        // pointer, so the feel doesn't depend on how often the device reports. Input without
        // timestamps takes one step per point.
        let a = 1.0 - amount.min(0.95);
        let dt = prev_input.map_or(0.0, |q| p.time - q.time);
        let still = prev_input.is_some_and(|q| q.x == p.x && q.y == p.y);
        if still && !self.cfg.catch_up {
            // Without Stroke Catch-Up the brush stops while the pointer pauses (Photoshop).
            return;
        }
        let (iters, step) = if dt > 0.0 && dt.is_finite() {
            let steps = dt / SMOOTHING_STEP_MS;
            let iters = steps.ceil().clamp(1.0, 64.0);
            // Each of `iters` moves covers `steps / iters` steps' worth of pull.
            (iters as usize, 1.0 - (1.0 - a).powf(steps.min(64.0) / iters))
        } else {
            (1, a)
        };
        let mut cur = pos;
        for _ in 0..iters {
            cur = pull(&cur, &p, step);
            out.push(cur);
        }
        self.pos = Some(cur);
    }

    /// Is Stroke Catch-Up still pulling the brush towards a pointer that has stopped? Then the
    /// stroke needs time to keep passing (repeats of the last point) while the pointer is held.
    pub fn lagging(&self) -> bool {
        match (self.active() && self.cfg.catch_up && !self.cfg.pulled_string, self.pos, self.last_input) {
            (true, Some(pos), Some(last)) => (pos.x - last.x).abs() + (pos.y - last.y).abs() > 0.01,
            _ => false,
        }
    }

    /// The string between the brush and the pointer: `(brush, pointer)` document positions while
    /// smoothing holds the brush back from the last input point, `None` when they coincide (or
    /// smoothing is off). The canvas draws it, as Photoshop does for Pulled String Mode.
    pub fn string(&self) -> Option<([f64; 2], [f64; 2])> {
        let (pos, last) = (self.pos?, self.last_input?);
        (self.active() && (pos.x != last.x || pos.y != last.y)).then_some(([pos.x, pos.y], [last.x, last.y]))
    }

    /// End of stroke: with Catch-Up On Stroke End the brush finishes at the last pointer position.
    pub fn finish(&mut self, out: &mut Vec<StrokePoint>) {
        if !self.active() || !self.cfg.catch_up_on_end {
            return;
        }
        if let (Some(pos), Some(last)) = (self.pos, self.last_input)
            && (pos.x != last.x || pos.y != last.y)
        {
            self.pos = Some(last);
            out.push(last);
        }
    }
}

/// Smooth a whole point list with the brush's smoothing settings.
pub fn smooth_points(cfg: &Smoothing, zoom: f32, pts: &[StrokePoint]) -> Vec<StrokePoint> {
    let mut s = Smoother::new(cfg, zoom);
    let mut out = Vec::with_capacity(pts.len());
    for p in pts {
        s.push(*p, &mut out);
    }
    s.finish(&mut out);
    out
}

// ---------------------------------------------------------------------------
// Path walking
// ---------------------------------------------------------------------------

/// One spacing (or airbrush time) step along the path.
#[derive(Clone, Copy, Debug)]
pub struct StepInput {
    pub point: StrokePoint,
    /// Visual (counter-clockwise, y-up) direction of travel in radians.
    pub direction: f32,
    pub initial_direction: f32,
    /// Sequential step number (0 = first).
    pub step: u64,
    /// Smoothed travel speed in pixels per millisecond.
    pub speed: f32,
    pub pressure_released: bool,
    pub distance: f32,
}

/// Walks a polyline and emits steps every `step_len(point)` pixels, plus airbrush steps in time.
#[derive(Clone, Debug)]
pub struct PathWalker {
    last: Option<StrokePoint>,
    /// Path distance from `last` to the next step.
    next_at: f64,
    /// First point, emitted lazily once the initial direction is known.
    pending_first: Option<StrokePoint>,
    initial_dir: Option<f32>,
    dir: f32,
    step: u64,
    /// Airbrush interval in ms (None = off).
    interval: Option<f64>,
    time_acc: f64,
    /// Speed spacing (Spacing unchecked): one step per this many ms of stroke time.
    speed: Option<f64>,
    speed_acc: f64,
    /// Pixel-grid stepping for small aliased tips: the dab-centre offset inside a pixel (see
    /// [`PathWalker::pixel_grid`]).
    grid: Option<f64>,
    sensor_point: Option<StrokePoint>,
    sensor_speed: f32,
    sensor_distance: f32,
    release_reference: f32,
    pressure_released: bool,
}

impl PathWalker {
    pub fn new(build_up_interval_ms: Option<f64>) -> Self {
        Self {
            last: None,
            next_at: 0.0,
            pending_first: None,
            initial_dir: None,
            dir: 0.0,
            step: 0,
            interval: build_up_interval_ms,
            time_acc: 0.0,
            speed: None,
            speed_acc: 0.0,
            grid: None,
            sensor_point: None,
            sensor_speed: 0.0,
            sensor_distance: 0.0,
            release_reference: 1.0,
            pressure_released: false,
        }
    }

    /// Step pixel by pixel instead of by distance: one step per pixel along the segment's major
    /// axis, each at the dab centre (`offset` 0.5 for odd tip diameters, 0 for even ones, as
    /// `render::grid_center` snaps) of the pixel the segment's line crosses at that column (or
    /// row, when steep). A small aliased tip (the Pencil) then draws a clean 8-connected
    /// staircase: stepping by distance put two dabs in a row wherever the line crossed a pixel
    /// corner (#2139). The line is sampled where it really runs, not between the pixels holding
    /// the segment's ends, so a straight line split into pieces (by Smoothing, or the pointer
    /// samples of a ⇧-drag) paints the pixels of the whole line, whichever end it starts from
    /// (#6336).
    pub fn pixel_grid(mut self, offset: f64) -> Self {
        self.grid = Some(offset);
        self
    }

    /// Emits the pixel steps from `a` to `p` (see [`PathWalker::pixel_grid`]). False, emitting
    /// nothing, when the span is not finite or implausibly long: distance spacing takes over.
    fn grid_steps(&mut self, a: &StrokePoint, p: &StrokePoint, offset: f64, out: &mut impl FnMut(StepInput)) -> bool {
        // The pieces of a 45° line differ in rounding noise: keep them all on one major axis.
        let steep = (p.y - a.y).abs() > (p.x - a.x).abs() * (1.0 + 1e-9);
        let (am, an, pm, pn) = if steep { (a.y, a.x, p.y, p.x) } else { (a.x, a.y, p.x, p.y) };
        let (m0, m1) = (grid_cell(am, offset), grid_cell(pm, offset));
        let (dm, dn) = (pm - am, pn - an);
        let n = (m1 - m0).abs();
        if !(n.is_finite() && dn.is_finite()) || n > MAX_GRID_STEPS || dm == 0.0 {
            return false;
        }
        let dir = (m1 - m0).signum();
        for i in 1..=n as u64 {
            let c = m0 + dir * i as f64;
            // The last column's centre can sit up to half a pixel past `p`: the line goes on to
            // it, so the next piece of a straight line picks up where this one left off.
            let t = (c - am) / dm;
            let mut q = lerp_pt(a, p, t.clamp(0.0, 1.0));
            let cn = grid_cell(an + dn * t, offset);
            (q.x, q.y) = if steep { (cn, c) } else { (c, cn) };
            self.emit(q, out);
        }
        true
    }

    /// Space steps by stroke time instead of distance ([`SPEED_SPACING_MS`]): faster movement
    /// spreads them out. Segments without timestamps are spaced by distance.
    pub fn speed_spacing(mut self, interval_ms: f64) -> Self {
        self.speed = Some(interval_ms.max(0.1));
        self
    }

    fn input(&self, point: StrokePoint) -> StepInput {
        StepInput {
            point,
            direction: self.dir,
            initial_direction: self.initial_dir.unwrap_or(self.dir),
            step: self.step,
            speed: self.sensor_speed,
            pressure_released: self.pressure_released,
            distance: self.sensor_distance,
        }
    }

    fn emit(&mut self, point: StrokePoint, out: &mut impl FnMut(StepInput)) {
        if let Some(previous) = self.sensor_point {
            let distance = (point.x - previous.x).hypot(point.y - previous.y);
            self.sensor_distance += distance as f32;
            let elapsed = (point.time - previous.time).max(0.1);
            let speed = (distance / elapsed) as f32;
            if speed.is_finite() {
                self.sensor_speed += (speed - self.sensor_speed) * 0.35;
            }
        }
        self.sensor_point = Some(point);
        if point.pressure < self.release_reference - 0.002 {
            self.pressure_released = true;
            self.release_reference = point.pressure;
        } else if point.pressure > self.release_reference + 0.002 {
            self.pressure_released = false;
            self.release_reference = point.pressure;
        }
        let s = self.input(point);
        self.step += 1;
        out(s);
    }

    pub fn push(&mut self, p: StrokePoint, step_len: &impl Fn(&StepInput) -> f64, out: &mut impl FnMut(StepInput)) {
        let Some(a) = self.last else {
            self.last = Some(p);
            self.pending_first = Some(p);
            self.next_at = step_len(&self.input(p));
            return;
        };
        let (dx, dy) = (p.x - a.x, p.y - a.y);
        let len = (dx * dx + dy * dy).sqrt();
        if len > 1e-9 {
            self.dir = (-dy).atan2(dx) as f32;
            if self.initial_dir.is_none() {
                self.initial_dir = Some(self.dir);
            }
        }
        if let Some(f) = self.pending_first.take() {
            self.emit(f, out);
        }
        if len > 1e-9 {
            // Speed spacing needs the segment's duration; without timestamps (a straight ⇧-click
            // line, or input that carries no times) the segment is spaced by distance instead.
            let dt = p.time - a.time;
            match self.speed.filter(|_| dt > 0.0 && dt.is_finite()) {
                None => {
                    if !self.grid.is_some_and(|g| self.grid_steps(&a, &p, g, out)) {
                        while self.next_at <= len + 1e-9 {
                            let q = lerp_pt(&a, &p, (self.next_at / len).min(1.0));
                            self.emit(q, out);
                            self.next_at += step_len(&self.input(q)).max(0.25);
                        }
                        self.next_at -= len;
                    }
                }
                Some(iv) => {
                    self.speed_acc += dt;
                    let max_n = (len / MIN_SPEED_STEP).ceil().max(1.0) as usize;
                    let mut n = 0;
                    while self.speed_acc >= iv && n < max_n {
                        self.speed_acc -= iv;
                        let f = ((dt - self.speed_acc) / dt).clamp(0.0, 1.0);
                        self.emit(lerp_pt(&a, &p, f), out);
                        n += 1;
                    }
                    if n == max_n {
                        self.speed_acc = self.speed_acc.rem_euclid(iv);
                    }
                }
            }
        }
        if let Some(iv) = self.interval {
            let dt = p.time - a.time;
            if dt > 0.0 && dt.is_finite() {
                if dt > iv * MAX_AIRBRUSH_DABS_PER_SEGMENT as f64 {
                    // A stalled or hostile timestamp must not replay an arbitrarily long backlog.
                    self.time_acc = 0.0;
                    self.emit(p, out);
                    self.last = Some(p);
                    return;
                }
                self.time_acc += dt;
                let mut n = 0;
                while self.time_acc >= iv && n < MAX_AIRBRUSH_DABS_PER_SEGMENT {
                    self.time_acc -= iv;
                    let f = ((dt - self.time_acc) / dt).clamp(0.0, 1.0);
                    self.emit(lerp_pt(&a, &p, f), out);
                    n += 1;
                }
            }
        }
        self.last = Some(p);
    }

    pub fn finish(&mut self, out: &mut impl FnMut(StepInput)) {
        if let Some(f) = self.pending_first.take() {
            self.emit(f, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Dynamics
// ---------------------------------------------------------------------------

/// Scalar control value in 0..1.
pub fn control_value(c: Control, fade_steps: u32, s: &StepInput) -> f32 {
    let p = &s.point;
    match c {
        Control::Off => 1.0,
        Control::Direction => s.direction.rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU,
        Control::InitialDirection => s.initial_direction.rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU,
        Control::Fade => 1.0 - (s.step as f32 / fade_steps.max(1) as f32).min(1.0),
        Control::DistanceFade => 1.0 - s.distance.clamp(0.0, 1.0),
        Control::PenPressure => p.pressure.clamp(0.0, 1.0),
        Control::PenPressureInverse => 1.0 - p.pressure.clamp(0.0, 1.0),
        Control::Speed => s.speed.clamp(0.0, 1.0),
        Control::PressureRelease => {
            if s.pressure_released {
                1.0
            } else {
                0.0
            }
        }
        Control::PenTilt => (p.tilt_x.hypot(p.tilt_y) / 90.0).clamp(0.0, 1.0),
        Control::StylusWheel => p.wheel.clamp(0.0, 1.0),
        Control::Rotation => (p.rotation.rem_euclid(360.0)) / 360.0,
    }
}

/// Material controls use tilt magnitude and normalized travel direction, as in the reference.
fn material_control_value(c: Control, fade_steps: u32, s: &StepInput) -> f32 {
    match c {
        Control::PenTilt => (s.point.tilt_x.hypot(s.point.tilt_y) / 90.0).clamp(0.0, 1.0),
        Control::Direction => s.direction.rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU,
        Control::InitialDirection => s.initial_direction.rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU,
        _ => control_value(c, fade_steps, s),
    }
}

/// A dynamic's control value passed through its own response curve ([`Dynamic::curve`]). Controls
/// with no 0..1 input (Off, the direction controls) read 1 and skip the curve.
pub fn dynamic_value(d: &Dynamic, s: &StepInput) -> f32 {
    let v = control_value(d.control, d.fade_steps, s);
    match d.control {
        Control::Off => v,
        _ => d.curve.eval(v),
    }
}

/// Angle contributed by a control, in degrees (counter-clockwise).
pub fn control_angle(c: Control, fade_steps: u32, s: &StepInput) -> f32 {
    let p = &s.point;
    match c {
        Control::Off => 0.0,
        Control::Fade => 360.0 * (s.step as f32 / fade_steps.max(1) as f32).min(1.0),
        Control::PenPressure => 360.0 * p.pressure.clamp(0.0, 1.0),
        Control::PenPressureInverse | Control::Speed | Control::PressureRelease | Control::DistanceFade => 360.0 * control_value(c, fade_steps, s),
        Control::PenTilt => {
            if p.tilt_x == 0.0 && p.tilt_y == 0.0 {
                0.0
            } else {
                (-p.tilt_y).atan2(p.tilt_x).to_degrees()
            }
        }
        Control::StylusWheel => 360.0 * p.wheel.clamp(0.0, 1.0),
        Control::Rotation => p.rotation,
        Control::InitialDirection => s.initial_direction.to_degrees(),
        Control::Direction => s.direction.to_degrees(),
    }
}

/// A 0..1 multiplier from a dynamic: control lerps from `minimum` to 1, jitter reduces randomly,
/// never below `minimum`.
fn scalar(d: &Dynamic, s: &StepInput, r: f32) -> f32 {
    if !d.is_active() {
        return 1.0;
    }
    let min = d.minimum.clamp(0.0, 1.0);
    let cv = dynamic_value(d, s);
    let controlled = if d.control == Control::Off { 1.0 } else { min + (1.0 - min) * cv };
    (controlled * (1.0 + d.jitter.clamp(0.0, 1.0) * (r * 2.0 - 1.0))).max(0.0)
}

fn rgb_to_hsv(c: [f32; 3]) -> [f32; 3] {
    let mx = c[0].max(c[1]).max(c[2]);
    let mn = c[0].min(c[1]).min(c[2]);
    let d = mx - mn;
    let h = if d <= 0.0 {
        0.0
    } else if mx == c[0] {
        ((c[1] - c[2]) / d).rem_euclid(6.0)
    } else if mx == c[1] {
        (c[2] - c[0]) / d + 2.0
    } else {
        (c[0] - c[1]) / d + 4.0
    } / 6.0;
    [h, if mx > 0.0 { d / mx } else { 0.0 }, mx]
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h6 = h.rem_euclid(1.0) * 6.0;
    let i = h6.floor();
    let f = h6 - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    match i as i32 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// Colour Dynamics for one dab (or once per stroke, with `index = 0`).
pub fn dynamic_color(b: &BrushSettings, s: &StepInput, seed: u64, index: u64) -> [f32; 4] {
    let cd = &b.color_dynamics;
    let (fg, bg) = (b.color, b.background);
    let r = |st: u64| rand01(seed, index, st);
    let amount = |base: f32, control: Control, minimum: f32| {
        if control == Control::Off {
            base
        } else {
            let min = minimum.clamp(0.0, 1.0);
            base * (min + (1.0 - min) * control_value(control, 25, s))
        }
    };
    let t = amount(cd.fg_bg.jitter.clamp(0.0, 1.0), cd.fg_bg.control, 0.0) * r(stream::FG_BG);
    let rgb = [fg[0] + (bg[0] - fg[0]) * t, fg[1] + (bg[1] - fg[1]) * t, fg[2] + (bg[2] - fg[2]) * t];
    let [mut h, mut sat, mut v] = rgb_to_hsv(rgb);
    h += rand_signed(seed, index, stream::HUE) * amount(cd.hue_jitter.clamp(0.0, 1.0), cd.hue_control, cd.hue_minimum) * 0.5;
    sat = (sat + rand_signed(seed, index, stream::SAT) * amount(cd.saturation_jitter.clamp(0.0, 1.0), cd.saturation_control, cd.saturation_minimum))
        .clamp(0.0, 1.0);
    let pu = amount(cd.purity.clamp(-1.0, 1.0), cd.purity_control, cd.purity_minimum);
    sat = if pu >= 0.0 { sat + (1.0 - sat) * pu } else { sat * (1.0 + pu) };
    v = (v + rand_signed(seed, index, stream::BRIGHT) * amount(cd.brightness_jitter.clamp(0.0, 1.0), cd.brightness_control, cd.brightness_minimum))
        .clamp(0.0, 1.0);
    let o = hsv_to_rgb(h, sat, v);
    [o[0], o[1], o[2], fg[3]]
}

/// Base (pre-jitter) diameter at a step: size × size control (pressure etc.). Drives spacing.
pub fn base_diameter(b: &BrushSettings, s: &StepInput) -> f32 {
    let sd = &b.shape_dynamics;
    let mut f = 1.0;
    if sd.enabled && sd.size.control != Control::Off {
        let min = sd.size.minimum.clamp(0.0, 1.0);
        f = min + (1.0 - min) * dynamic_value(&sd.size, s);
    } else if b.pressure_size {
        f = s.point.pressure.clamp(0.0, 1.0);
    }
    b.size * f
}

fn resolved_flow(b: &BrushSettings, seed: u64, index: u64, s: &StepInput) -> f32 {
    let mut flow = b.flow;
    if b.transfer.enabled {
        flow *= scalar(&b.transfer.flow, s, rand01(seed, index, stream::FLOW));
    }
    if b.pressure_flow && !(b.transfer.enabled && b.transfer.flow.control != Control::Off) {
        flow *= s.point.pressure.clamp(0.0, 1.0);
    }
    if b.flow_algorithm_version == 2 { 0.12 + 0.88 * flow.clamp(0.0, 1.0) } else { flow.clamp(0.0, 1.0) }
}

/// Builds the primary dabs of a step (Shape Dynamics, Scattering, Transfer, Color, Texture depth).
#[derive(Clone, Debug)]
pub struct DabBuilder {
    pub brush: BrushSettings,
    pub seed: u64,
    next_index: u64,
    sampled_extents: Option<std::sync::Arc<[f32; 256]>>,
}

impl DabBuilder {
    pub fn new(brush: &BrushSettings) -> Self {
        let sampled_extents = brush.tip.bitmap().filter(|tip| tip.is_valid()).map(|tip| {
            let mut extents = [0.0f32; 256];
            let side = tip.width.max(tip.height) as f32;
            for (i, alpha) in tip.data.iter().enumerate() {
                let x = (i % tip.width as usize) as f32 + 0.5 - tip.width as f32 * 0.5;
                let y = (i / tip.width as usize) as f32 + 0.5 - tip.height as f32 * 0.5;
                let bin = usize::from(*alpha / 257);
                if let Some(extent) = extents.get_mut(bin) {
                    *extent = extent.max((2.0 * x.abs().max(y.abs()) + 1.0) / side);
                }
            }
            for i in (1..255).rev() {
                extents[i] = extents[i].max(extents[i + 1]);
            }
            std::sync::Arc::new(extents)
        });
        Self { brush: brush.clone(), seed: brush.seed, next_index: 0, sampled_extents }
    }

    fn controlled_input(&self, s: &StepInput) -> StepInput {
        let speed = (s.speed / (self.brush.size * 0.022).max(0.16)).clamp(0.0, 1.0);
        StepInput { speed: speed * speed * (3.0 - 2.0 * speed), distance: s.distance / (self.brush.size * self.brush.spacing.max(0.05) * 32.0).max(96.0), ..*s }
    }

    pub fn step_len(&self, s: &StepInput) -> f64 {
        let s = self.controlled_input(s);
        let b = &self.brush;
        let diameter = self.diameter_at(&s, s.step).max(1.0);
        let alpha = resolved_flow(b, self.seed, s.step, &s);
        let hardness = self.hardness_at(&s, s.step);
        let radius = diameter * 0.5;
        // Use the footprint that survives 8-bit alpha quantization, without resizing the dab.
        let visible_radius = |flow: f32| {
            let threshold = (0.5 / 255.0) / flow.max(0.000001);
            if threshold >= 1.0 {
                return 0.0;
            }
            if let Some(extents) = &self.sampled_extents {
                let bin = (threshold * 255.0).ceil().clamp(1.0, 255.0) as usize;
                return radius * extents.get(bin).copied().unwrap_or(0.0);
            }
            if b.aliased {
                return radius;
            }
            if b.c3_tip_math {
                if hardness >= 0.999 {
                    return radius;
                }
                let tail = (-2.5f32).exp();
                let t = (-(tail + threshold * (1.0 - tail)).ln() / 2.5).sqrt();
                return radius * (hardness + (1.0 - hardness) * t);
            }
            let (core, shoulder) = crate::tip_shape(radius, hardness);
            // Inverse of tip_falloff's six-squaring exponential.
            core + shoulder * (64.0 * (1.0 - threshold.powf(1.0 / 64.0))).sqrt()
        };
        let full_radius = visible_radius(1.0);
        let ratio = if alpha <= 0.5 / 255.0 || full_radius <= 0.000001 { 1.0 } else { (visible_radius(alpha) / full_radius).clamp(0.0, 1.0) };
        f64::from(diameter * ratio * b.spacing.max(0.01)).max(0.5)
    }

    fn diameter_at(&self, s: &StepInput, index: u64) -> f32 {
        let sd = &self.brush.shape_dynamics;
        let jitter = if sd.enabled { sd.size.jitter.clamp(0.0, 1.0) } else { 0.0 };
        base_diameter(&self.brush, s) * (1.0 + jitter * (rand01(self.seed, index, stream::SIZE) * 2.0 - 1.0))
    }

    fn hardness_at(&self, s: &StepInput, index: u64) -> f32 {
        let b = &self.brush;
        let hardness = (b.hardness * scalar(&b.hardness_dynamic, s, rand01(self.seed, index, stream::HARDNESS))).clamp(0.0, 1.0);
        if let Some(contour) = &b.contour {
            let base = if b.pigment.as_ref().is_some_and(|p| p.enabled && p.gaussian_mask) { 0.0 } else { hardness };
            contour.adjusted_hardness(base, s.point.pressure)
        } else {
            b.pigment.as_ref().map_or(hardness, |p| p.adjusted_hardness(hardness, s.point.pressure))
        }
    }

    pub fn build(&mut self, s: &StepInput, out: &mut Vec<Dab>) {
        let s = &self.controlled_input(s);
        let b = &self.brush;
        let seed = self.seed;
        let sd = &b.shape_dynamics;
        let sc = &b.scattering;
        let count = if sc.enabled {
            let n = sc.count.clamp(1, 16) as f32;
            let minimum = sc.minimum_count.unwrap_or(n * sc.count_jitter.minimum.clamp(0.0, 1.0)).clamp(1.0, n);
            let controlled =
                if sc.count_jitter.control == Control::Off { n } else { (minimum + (n - minimum) * dynamic_value(&sc.count_jitter, s)).round().max(1.0) };
            let jitter = 1.0 + rand_signed(seed, s.step, stream::COUNT) * sc.count_jitter.jitter.clamp(0.0, 1.0);
            ((controlled * jitter).round() as u32).clamp(1, 32)
        } else {
            1
        };
        let base_d = base_diameter(b, s);
        for _ in 0..count {
            let i = self.next_index;
            self.next_index += 1;
            let r = |st: u64| rand01(seed, i, st);
            // Shape.
            let diameter = self.diameter_at(s, i);
            let mut angle = b.angle;
            let mut roundness = b.roundness.clamp(0.01, 1.0);
            let (mut fx, mut fy) = (b.flip_x, b.flip_y);
            if sd.enabled {
                let angle_input = if b.c3_tip_math { StepInput { direction: -s.direction, initial_direction: -s.initial_direction, ..*s } } else { *s };
                let controlled_angle = match sd.angle.control {
                    Control::Off => 0.0,
                    Control::PenTilt if b.c3_tip_math => {
                        if s.point.tilt_x.hypot(s.point.tilt_y) > 1e-6 {
                            s.point.tilt_y.atan2(s.point.tilt_x).to_degrees().rem_euclid(360.0) - 180.0
                        } else {
                            0.0
                        }
                    }
                    Control::PenTilt => control_angle(sd.angle.control, sd.angle.fade_steps, &angle_input),
                    Control::Fade => control_angle(sd.angle.control, sd.angle.fade_steps, &angle_input),
                    _ => 360.0 * (sd.angle.minimum.clamp(0.0, 1.0) + (1.0 - sd.angle.minimum.clamp(0.0, 1.0)) * dynamic_value(&sd.angle, &angle_input)),
                };
                angle += controlled_angle + rand_signed(seed, i, stream::ANGLE) * sd.angle.jitter.clamp(0.0, 1.0) * 360.0;
                roundness = (roundness * scalar(&sd.roundness, s, r(stream::ROUNDNESS))).clamp(0.01, 1.0);
                if sd.flip_x_jitter && r(stream::FLIP_X) < 0.5 {
                    fx = !fx;
                }
                if sd.flip_y_jitter && r(stream::FLIP_Y) < 0.5 {
                    fy = !fy;
                }
            }
            // Tilt Scale and Brush Projection (Shape Dynamics).
            let (mut proj_angle, mut proj_scale) = (0.0, 1.0);
            if sd.enabled {
                let tilt = s.point.tilt_x.hypot(s.point.tilt_y).clamp(0.0, 90.0);
                if sd.size.control == Control::PenTilt && sd.tilt_scale > 0.0 {
                    let k = 1.0 - sd.tilt_scale.clamp(0.0, 2.0) * 0.5 * tilt / 90.0;
                    roundness = (roundness * k).clamp(0.01, 1.0);
                }
                if sd.brush_projection {
                    angle += s.point.rotation;
                    if tilt > 0.0 {
                        proj_angle = (-s.point.tilt_y).atan2(s.point.tilt_x);
                        proj_scale = tilt.to_radians().cos().max(0.05);
                    }
                }
            }
            let radius = (diameter / 2.0).max(0.5);
            // Scatter.
            let mut c = Point::new(s.point.x, s.point.y);
            if sd.enabled && sd.stroke_jitter.jitter > 0.0 {
                let j = sd.stroke_jitter.jitter.clamp(0.0, 10.0);
                let min = sd.stroke_jitter.minimum.clamp(0.0, 1.0);
                let weight = if sd.stroke_jitter.control == Control::Off { 1.0 } else { min + (1.0 - min) * dynamic_value(&sd.stroke_jitter, s) };
                let diameter = base_d.max(0.0);
                let amplitude = diameter * j * if j <= 0.1 { 0.5 } else { 1.0 } * weight;
                let wavelength = (diameter * 3.5 + amplitude * 2.0).max(12.0);
                let offset = (s.distance * (b.size * b.spacing.max(0.05) * 32.0).max(96.0) / wavelength * std::f32::consts::TAU).sin() * amplitude;
                c.x += f64::from(s.direction.sin() * offset);
                c.y += f64::from(s.direction.cos() * offset);
            }
            if sc.enabled && sc.scatter.jitter > 0.0 {
                let min = sc.scatter.minimum.clamp(0.0, 1.0);
                let cv = if sc.scatter.control == Control::Off { 1.0 } else { min + (1.0 - min) * dynamic_value(&sc.scatter, s) };
                let range = 1.0 + rand_signed(seed, i, stream::SCATTER_RANGE) * sc.range_jitter.clamp(0.0, 1.0);
                let amt = f64::from(sc.scatter.jitter * cv * radius * range);
                let spacing = sc.spacing_jitter.jitter.clamp(0.0, 1.0)
                    * if sc.spacing_jitter.control == Control::Off {
                        1.0
                    } else {
                        let min = sc.spacing_jitter.minimum.clamp(0.0, 1.0);
                        min + (1.0 - min) * dynamic_value(&sc.spacing_jitter, s)
                    };
                let offset =
                    |axis, variance| f64::from(rand_signed(seed, i, axis) * (1.0 + spacing * 1.75) + rand_signed(seed, i, variance) * spacing * spacing) * amt;
                let (ox, oy) = (offset(stream::SCATTER_X, stream::SCATTER_SPACING_X), offset(stream::SCATTER_Y, stream::SCATTER_SPACING_Y));
                let parallel = if sc.both_axes { oy } else { 0.0 };
                let (dx, dy) = (f64::from(s.direction.cos()), -f64::from(s.direction.sin()));
                if sc.both_axes && !b.c3_tip_math {
                    c.x += ox;
                    c.y += oy;
                } else {
                    c.x += -dy * ox + dx * parallel;
                    c.y += dx * ox + dy * parallel;
                }
            }
            // Transfer.
            let tr = &b.transfer;
            let mut opacity = 1.0;
            if tr.enabled {
                opacity = scalar(&tr.opacity, s, r(stream::OPACITY));
            }
            let (wet, mix) = if tr.enabled {
                (scalar(&tr.wetness, s, r(stream::WET)).clamp(0.0, 1.0), scalar(&tr.mix, s, r(stream::MIX)).clamp(0.0, 1.0))
            } else {
                (1.0, 1.0)
            };
            if b.pressure_opacity && !(tr.enabled && tr.opacity.control != Control::Off) {
                opacity *= s.point.pressure.clamp(0.0, 1.0);
            }
            // Colour.
            let color = if b.color_dynamics.enabled && !b.erase { dynamic_color(b, s, seed, if b.color_dynamics.per_tip { i } else { 0 }) } else { b.color };
            // Texture depth (per tip).
            let tx = &b.texture;
            let depth = if tx.enabled && tx.each_tip {
                (tx.depth.clamp(0.0, 1.0) * scalar(&tx.depth_jitter, s, r(stream::DEPTH))).clamp(0.0, 1.0)
            } else {
                tx.depth.clamp(0.0, 1.0)
            };
            out.push(Dab {
                pigment: b.pigment.as_ref().map(|p| p.resolve(s, [r(25), r(26)])).unwrap_or_default(),
                hardness: self.hardness_at(s, i),
                mix_strength: crate::transport::pressure_value(
                    b.mix_settings.mix_pressure_curve.as_ref(),
                    b.mix_settings.strength,
                    s.point.pressure,
                    s.point.is_pen,
                ) * scalar(&b.mix_settings.strength_dynamic, s, r(27)),
                mix_length: b.mix_settings.length
                    * scalar(
                        &b.mix_settings.length_dynamic,
                        &StepInput {
                            point: StrokePoint {
                                pressure: if b.mix_settings.length_pressure_inverted { 1.0 - s.point.pressure } else { s.point.pressure },
                                ..s.point
                            },
                            ..*s
                        },
                        r(28),
                    ),
                blur_strength: crate::transport::pressure_value(
                    b.mix_settings.blur_pressure_curve.as_ref(),
                    b.mix_settings.blur_strength,
                    s.point.pressure,
                    s.point.is_pen,
                ) * scalar(
                    &b.mix_settings.blur_dynamic,
                    &StepInput {
                        point: StrokePoint {
                            pressure: if b.mix_settings.blur_pressure_inverted { 1.0 - s.point.pressure } else { s.point.pressure },
                            ..s.point
                        },
                        ..*s
                    },
                    r(29),
                ),
                center: c,
                radius,
                alpha: resolved_flow(b, seed, i, s),
                angle: angle.to_radians(),
                roundness,
                flip_x: fx,
                flip_y: fy,
                opacity: opacity.clamp(0.0, 1.0),
                color,
                depth,
                index: i,
                proj_angle,
                proj_scale,
                wet,
                mix,
                grain_density: b.grain.as_ref().map_or(0.0, |g| g.density.clamp(0.0, 1.0) * material_control_value(g.control, g.fade_steps, s)),
                wet_edge: b
                    .wet_edge
                    .as_ref()
                    .map_or(crate::material::ResolvedWet::default(), |w| w.resolve(material_control_value(w.control, w.fade_steps, s), 2.0 * radius)),
            });
        }
    }
}

/// Builds Dual Brush dabs (the second tip: size, spacing, scatter, count, flip).
#[derive(Clone, Debug)]
pub struct DualBuilder {
    pub brush: BrushSettings,
    next_index: u64,
    full: Option<DabBuilder>,
}

impl DualBuilder {
    pub fn new(brush: &BrushSettings) -> Self {
        Self { brush: brush.clone(), next_index: 0, full: brush.dual_brush.resolved_brush(brush).as_ref().map(DabBuilder::new) }
    }
    pub fn step_len(&self, _p: &StepInput) -> f64 {
        if let Some(full) = &self.full {
            return full.step_len(_p);
        }
        let d = &self.brush.dual_brush;
        f64::from(d.size.max(1.0) * d.spacing.max(0.01)).max(0.5)
    }
    pub fn build(&mut self, s: &StepInput, out: &mut Vec<Dab>) {
        if let Some(full) = &mut self.full {
            let start = out.len();
            full.build(s, out);
            if self.brush.dual_brush.flip {
                for dab in &mut out[start..] {
                    dab.flip_x = !dab.flip_x;
                }
            }
            return;
        }
        let d = &self.brush.dual_brush;
        let seed = self.brush.seed ^ 0xD0A1;
        for _ in 0..d.count.clamp(1, 16) {
            let i = self.next_index;
            self.next_index += 1;
            let radius = (d.size / 2.0).max(0.5);
            let amt = f64::from(d.scatter * radius);
            let (ox, oy) = (rand_signed(seed, i, stream::DUAL_X) as f64 * amt, rand_signed(seed, i, stream::DUAL_Y) as f64 * amt);
            let mut c = Point::new(s.point.x, s.point.y);
            if d.both_axes {
                c.x += ox;
                c.y += oy;
            } else {
                let (dx, dy) = (f64::from(s.direction.cos()), -f64::from(s.direction.sin()));
                c.x += -dy * ox;
                c.y += dx * ox;
            }
            let flip = d.flip && rand01(seed, i, stream::DUAL_FLIP) < 0.5;
            out.push(Dab {
                pigment: crate::pigment_material::ResolvedPigment::default(),
                hardness: (d.hardness * scalar(&d.hardness_dynamic, s, rand01(seed, i, stream::HARDNESS))).clamp(0.0, 1.0),
                mix_strength: 0.0,
                mix_length: 0.5,
                blur_strength: 0.0,
                center: c,
                radius,
                alpha: d.flow.clamp(0.0, 1.0),
                angle: d.angle.to_radians(),
                roundness: d.roundness.clamp(0.01, 1.0),
                flip_x: flip,
                flip_y: false,
                opacity: 1.0,
                color: [0.0, 0.0, 0.0, 1.0],
                depth: 1.0,
                index: i,
                proj_angle: 0.0,
                proj_scale: 1.0,
                wet: 1.0,
                mix: 1.0,
                grain_density: d.grain.as_ref().map_or(0.0, |g| g.density.clamp(0.0, 1.0) * material_control_value(g.control, g.fade_steps, s)),
                wet_edge: d
                    .wet_edge
                    .as_ref()
                    .map_or(crate::material::ResolvedWet::default(), |w| w.resolve(material_control_value(w.control, w.fade_steps, s), 2.0 * radius)),
            });
        }
    }
}

/// Apply Brush Pose to an input point.
pub fn apply_pose(b: &BrushSettings, mut p: StrokePoint) -> StrokePoint {
    let pose = &b.pose;
    if pose.enabled {
        if pose.override_tilt {
            p.tilt_x = pose.tilt_x;
            p.tilt_y = pose.tilt_y;
        }
        if pose.override_rotation {
            p.rotation = pose.rotation;
        }
        if pose.override_pressure {
            p.pressure = pose.pressure;
        }
    }
    p
}

/// Incremental dab generation for a whole brush (primary and optional dual dabs).
#[derive(Clone, Debug)]
pub struct DabGenerator {
    smoother: Smoother,
    walker: PathWalker,
    dual_walker: Option<PathWalker>,
    dual_smoother: Option<Smoother>,
    motion: crate::motion::Motion,
    dual_motion: crate::motion::Motion,
    inertia: crate::motion::Inertia,
    dual_inertia: crate::motion::Inertia,
    builder: DabBuilder,
    dual: Option<DualBuilder>,
    scratch: Vec<StrokePoint>,
}

impl DabGenerator {
    /// Does the stroke change with time while the pointer is held still (airbrush Build-up, or
    /// smoothing still catching up)? The canvas then keeps feeding it the held point.
    pub fn wants_time(&self) -> bool {
        self.walker.interval.is_some() || self.smoother.lagging() || self.dual_smoother.as_ref().is_some_and(Smoother::lagging)
    }

    /// See [`Smoother::string`].
    pub fn smoothing_string(&self) -> Option<([f64; 2], [f64; 2])> {
        self.smoother.string()
    }

    pub fn new(brush: &BrushSettings, zoom: f32) -> Self {
        let brush = brush.bounded_for_render();
        let interval = None;
        let dual_on = brush.dual_brush.enabled;
        let mut walker = if brush.spacing_enabled { PathWalker::new(interval) } else { PathWalker::new(interval).speed_spacing(SPEED_SPACING_MS) };
        // A small aliased tip whose dabs are at most a pixel apart steps pixel by pixel (#2139).
        if brush.aliased && brush.size * brush.spacing.max(0.01) <= 1.0 {
            walker = walker.pixel_grid(f64::from(crate::render::grid_center(0.0, 0.0, brush.size).0));
        }
        Self {
            smoother: Smoother::new(&brush.smoothing, zoom),
            walker,
            dual_walker: dual_on.then(|| {
                let secondary = brush.dual_brush.resolved_brush(&brush);
                if secondary.as_ref().is_none_or(|b| b.spacing_enabled) { PathWalker::new(None) } else { PathWalker::new(None).speed_spacing(SPEED_SPACING_MS) }
            }),
            dual_smoother: dual_on.then(|| {
                let secondary = brush.dual_brush.resolved_brush(&brush);
                Smoother::new(secondary.as_ref().map_or(&brush.smoothing, |b| &b.smoothing), zoom)
            }),
            motion: crate::motion::Motion::default(),
            dual_motion: crate::motion::Motion::default(),
            inertia: crate::motion::Inertia::default(),
            dual_inertia: crate::motion::Inertia::default(),
            builder: DabBuilder::new(&brush),
            dual: dual_on.then(|| DualBuilder::new(&brush)),
            scratch: Vec::new(),
        }
    }

    fn walk(&mut self, pts: &[StrokePoint], dabs: &mut Vec<Dab>) {
        for p in pts {
            let p = apply_pose(&self.builder.brush, *p);
            let b = &self.builder;
            let len = |q: &StepInput| b.step_len(q);
            let mut steps = Vec::new();
            self.walker.push(p, &len, &mut |s| steps.push(s));
            for s in &steps {
                self.builder.build(s, dabs);
            }
            let input = self.walker.input(p);
            let controlled = self.builder.controlled_input(&input);
            let diameter = base_diameter(&self.builder.brush, &controlled);
            let brush = &self.builder.brush;
            let control = control_value(brush.inertia.control, 25, &controlled);
            let branches = self.inertia.append(p, &brush.inertia, diameter, self.builder.step_len(&input) as f32, control);
            let mut fork = self.builder.clone();
            fork.seed ^= input.step.wrapping_mul(0x9E3779B97F4A7C15);
            fork.next_index = 0;
            for (point, direction, distance) in branches {
                fork.build(&StepInput { point, direction, distance, ..input }, dabs);
            }
        }
    }

    fn walk_dual(&mut self, pts: &[StrokePoint], dual: &mut Vec<Dab>) {
        for p in pts {
            let p = self.dual.as_ref().map_or(*p, |d| apply_pose(d.full.as_ref().map_or(&d.brush, |b| &b.brush), *p));
            if let (Some(w), Some(db)) = (self.dual_walker.as_mut(), self.dual.as_mut()) {
                let mut steps = Vec::new();
                let len = |q: &StepInput| db.step_len(q);
                w.push(p, &len, &mut |s| steps.push(s));
                for s in &steps {
                    db.build(s, dual);
                }
                if let Some(full) = &db.full {
                    let input = w.input(p);
                    let controlled = full.controlled_input(&input);
                    let diameter = base_diameter(&full.brush, &controlled);
                    let control = control_value(full.brush.inertia.control, 25, &controlled);
                    let branches = self.dual_inertia.append(p, &full.brush.inertia, diameter, full.step_len(&input) as f32, control);
                    let mut fork = full.clone();
                    fork.seed ^= input.step.wrapping_mul(0xD1B54A32D192ED03);
                    fork.next_index = 0;
                    for (point, direction, distance) in branches {
                        fork.build(&StepInput { point, direction, distance, ..input }, dual);
                    }
                }
            }
        }
    }

    /// Feed input points; appends the new primary and dual dabs.
    pub fn push(&mut self, pts: &[StrokePoint], dabs: &mut Vec<Dab>, dual: &mut Vec<Dab>) {
        let mut sm = std::mem::take(&mut self.scratch);
        sm.clear();
        for p in pts {
            let brush = &self.builder.brush;
            let point = self.motion.resolve(*p, &brush.motion, brush.size);
            self.smoother.push(point, &mut sm);
        }
        self.walk(&sm, dabs);
        self.scratch = sm;
        if let Some(smoother) = &mut self.dual_smoother {
            let mut secondary = Vec::new();
            let brush = self.dual.as_ref().map(|d| d.full.as_ref().map_or(&d.brush, |b| &b.brush));
            for p in pts {
                let point = brush.map_or(*p, |b| self.dual_motion.resolve(*p, &b.motion, b.size));
                smoother.push(point, &mut secondary);
            }
            self.walk_dual(&secondary, dual);
        }
    }

    /// End of stroke.
    pub fn finish(&mut self, dabs: &mut Vec<Dab>, dual: &mut Vec<Dab>) {
        let mut sm = Vec::new();
        self.smoother.finish(&mut sm);
        self.walk(&sm, dabs);
        if let Some(smoother) = &mut self.dual_smoother {
            let mut secondary = Vec::new();
            smoother.finish(&mut secondary);
            self.walk_dual(&secondary, dual);
        }
        let mut steps = Vec::new();
        self.walker.finish(&mut |s| steps.push(s));
        for s in &steps {
            self.builder.build(s, dabs);
        }
        if let (Some(w), Some(db)) = (self.dual_walker.as_mut(), self.dual.as_mut()) {
            let mut steps = Vec::new();
            w.finish(&mut |s| steps.push(s));
            for s in &steps {
                db.build(s, dual);
            }
        }
    }
}

#[cfg(test)]
mod dual_brush_tests {
    use super::*;

    #[test]
    fn dual_brush_flow_is_applied_to_each_secondary_dab() {
        let brush = BrushSettings { dual_brush: crate::DualBrush { enabled: true, flow: 0.35, ..Default::default() }, ..Default::default() };
        let mut generator = DabGenerator::new(&brush, 1.0);
        let mut primary = Vec::new();
        let mut dual = Vec::new();
        generator.push(&[StrokePoint::new(10.0, 10.0, 1.0)], &mut primary, &mut dual);
        generator.finish(&mut primary, &mut dual);
        assert!(!dual.is_empty());
        assert!(dual.iter().all(|dab| (dab.alpha - 0.35).abs() < 1e-6));
    }
}

#[cfg(test)]
mod c3_full_secondary_tests {
    use super::*;
    #[test]
    fn c3_count_minimum_stays_absolute_when_base_count_changes() {
        let mut brush = BrushSettings {
            scattering: crate::Scattering {
                enabled: true,
                count: 6,
                minimum_count: Some(3.0),
                count_jitter: crate::Dynamic { control: Control::PenPressure, ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };
        for count in [6, 12] {
            brush.scattering.count = count;
            let mut builder = DabBuilder::new(&brush);
            let mut dabs = Vec::new();
            builder.build(
                &StepInput {
                    point: StrokePoint::new(0., 0., 0.),
                    direction: 0.,
                    initial_direction: 0.,
                    step: 0,
                    speed: 0.,
                    pressure_released: false,
                    distance: 0.,
                },
                &mut dabs,
            );
            assert_eq!(dabs.len(), 3);
        }
    }
    #[test]
    fn full_secondary_uses_its_own_motion_and_resize_ratio() {
        let secondary = BrushSettings {
            size: 12.0,
            motion: crate::motion::MotionSettings { enabled: true, guide_mode: crate::motion::GuideMode::Anchor, anchor_point_x: 1.0, ..Default::default() },
            ..Default::default()
        };
        let mut b = BrushSettings {
            size: 20.0,
            dual_brush: crate::DualBrush { enabled: true, full_parameters: Some(Box::new(secondary)), ..Default::default() },
            ..Default::default()
        };
        let mut generator = DabGenerator::new(&b, 1.0);
        let (mut main, mut dual) = (Vec::new(), Vec::new());
        generator.push(&[StrokePoint::new(50., 50., 1.)], &mut main, &mut dual);
        generator.finish(&mut main, &mut dual);
        assert_eq!(main[0].center.x, 50.);
        assert_eq!(dual[0].center.x, 44.);
        b.resize_with_dual_brush(40.0);
        assert_eq!(b.dual_brush.full_parameters.as_ref().unwrap().size, 24.0);
    }
    #[test]
    fn color_stroke_mode_freezes_randomness_but_not_live_pressure() {
        let b = BrushSettings {
            color: [1., 0., 0., 0.4],
            color_dynamics: crate::ColorDynamics { enabled: true, per_tip: false, purity: -1., purity_control: Control::PenPressure, ..Default::default() },
            ..Default::default()
        };
        let mut builder = DabBuilder::new(&b);
        let mut d = Vec::new();
        let base =
            StepInput { point: StrokePoint::new(0., 0., 0.), direction: 0., initial_direction: 0., step: 0, speed: 0., pressure_released: false, distance: 0. };
        builder.build(&base, &mut d);
        builder.build(&StepInput { point: StrokePoint::new(1., 0., 1.), step: 1, ..base }, &mut d);
        assert_eq!(d[0].color, [1., 0., 0., 0.4]);
        assert_eq!(d[1].color, [1., 1., 1., 0.4]);
    }
}
