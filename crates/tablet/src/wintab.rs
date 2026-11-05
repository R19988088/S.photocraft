//! WinTab packet → [`Sample`]s. Pure (no Win32 calls), so it is tested on every platform;
//! `windows.rs` opens the context and feeds the driver's packets here.
//!
//! A WinTab `PACKET` is a driver-filled buffer whose layout is the packet-data mask's fields in
//! the order `pktdef.h` declares (cursor, buttons, x, y, normal pressure, orientation), so
//! [`RawPacket::parse`] reads exactly the mask [`packet_mask`] asks for. Pressure is scaled by
//! the device's `DVC_NPRESSURE` axis; the orientation triple is azimuth, altitude and twist in
//! tenths of a degree, mapped to the same convention as the AppKit and XInput2 readers (tilt X
//! to the right, tilt Y towards the user, rotation clockwise).

use crate::Sample;

pub const PK_CURSOR: u32 = 0x0020;
pub const PK_BUTTONS: u32 = 0x0040;
pub const PK_NORMAL_PRESSURE: u32 = 0x0400;
pub const PK_ORIENTATION: u32 = 0x1000;

/// The packet-data mask the context asks for; [`RawPacket::parse`] reads exactly this layout.
pub fn packet_mask() -> u32 {
    PK_CURSOR | PK_BUTTONS | PK_NORMAL_PRESSURE | PK_ORIENTATION
}

/// Bytes of a packet with [`packet_mask`]: cursor, buttons, normal pressure, then the
/// three-int orientation. The layout must match `pktdef.h` for exactly this mask.
pub const PACKET_BYTES: usize = 6 * 4;

/// The pen fields of a WinTab packet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawPacket {
    /// Index of the tool in use (`pkCursor`).
    pub cursor: u32,
    /// Button state; bit 0 is the tip (or the eraser's contact).
    pub buttons: u32,
    /// Normal (tip) pressure in the device's `DVC_NPRESSURE` range.
    pub pressure: u32,
    /// `orAzimuth`, `orAltitude`, `orTwist`, tenths of a degree.
    pub orientation: [i32; 3],
}

impl RawPacket {
    /// Read a packet the driver filled for [`packet_mask`]; `None` when the buffer is short.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < PACKET_BYTES {
            return None;
        }
        let u = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        Some(Self {
            cursor: u(0),
            buttons: u(4),
            pressure: u(8),
            orientation: [u(12) as i32, u(16) as i32, u(20) as i32],
        })
    }
}

/// Idle time after which no packet means the pen left range: a mouse painting now must not
/// inherit the pen's last pressure. A pen resting on the tablet still reports at the packet rate.
pub const IDLE_MS: f64 = 250.0;

/// The receiving side: turns the driver's raw packet buffers into [`Sample`]s. Keeping it here
/// (rather than in `windows.rs`) makes the whole receive path testable on any platform: a test
/// feeds synthetic packet bytes exactly as the driver would.
#[derive(Clone, Debug)]
pub struct Pump {
    pressure: Axis,
    /// `CSR_TYPE` per cursor, asked once and remembered.
    cursor_types: Vec<(u32, Option<u32>)>,
    /// A pen reported recently (packets are arriving).
    pen: bool,
    last_ms: f64,
}

impl Default for Pump {
    fn default() -> Self {
        Self { pressure: Axis { min: 0, max: 1023 }, cursor_types: Vec::new(), pen: false, last_ms: f64::NEG_INFINITY }
    }
}

impl Pump {
    pub fn with_pressure(pressure: Axis) -> Self {
        Self { pressure, ..Self::default() }
    }

    /// The samples of `bytes` (a whole number of [`PACKET_BYTES`] packets), in order.
    /// `cursor_type` supplies `CSR_TYPE` for a cursor (called once per cursor); `None` means the
    /// driver doesn't report it and [`eraser`]'s convention applies.
    pub fn drain(&mut self, bytes: &[u8], now_ms: f64, mut cursor_type: impl FnMut(u32) -> Option<u32>) -> Vec<Sample> {
        let mut out = Vec::with_capacity(bytes.len() / PACKET_BYTES);
        for chunk in bytes.chunks_exact(PACKET_BYTES) {
            let Some(packet) = RawPacket::parse(chunk) else { continue };
            let ty = self.cursor_type(packet.cursor, &mut cursor_type);
            out.push(sample(&packet, self.pressure, eraser(packet.cursor, ty)));
        }
        if !out.is_empty() {
            self.pen = true;
            self.last_ms = now_ms;
        }
        out
    }

    /// Whether the pen left range since the last [`Self::drain`] (the caller reports `None` once,
    /// which resets this). Time is passed in so the check is testable without waiting.
    pub fn went_away(&mut self, now_ms: f64) -> bool {
        if !self.pen || now_ms - self.last_ms < IDLE_MS {
            return false;
        }
        self.pen = false;
        true
    }

    fn cursor_type(&mut self, cursor: u32, lookup: &mut impl FnMut(u32) -> Option<u32>) -> Option<u32> {
        if let Some(ty) = self.cursor_types.iter().find(|(c, _)| *c == cursor) {
            return ty.1;
        }
        let ty = lookup(cursor);
        if self.cursor_types.len() < 256 {
            self.cursor_types.push((cursor, ty));
        }
        ty
    }
}

/// A `DVC_NPRESSURE` / `DVC_ORIENTATION` axis range (`AXIS.axMin..axMax`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Axis {
    pub min: i32,
    pub max: i32,
}

impl Axis {
    /// A raw value as 0..1 (a degenerate range reads as full pressure, like a mouse).
    pub fn unit(&self, value: u32) -> f32 {
        let span = f64::from(self.max) - f64::from(self.min);
        if !span.is_finite() || span <= 0.0 {
            return 1.0;
        }
        (((f64::from(value) - f64::from(self.min)) / span) as f32).clamp(0.0, 1.0)
    }
}

/// Is the cursor an eraser end? The tool type lives in bits `0x0F06` of `CSR_TYPE` (Wacom's
/// documented convention, as GTK reads it): a pen or airbrush with bit `0x0008` set is the
/// eraser end.
pub fn is_eraser(cursor_type: u32) -> bool {
    match cursor_type & 0x0F06 {
        0x0802 | 0x0804 | 0x0902 => cursor_type & 0x0008 != 0,
        _ => (cursor_type & 0x0006) == 0x0002 && cursor_type & 0x0008 != 0,
    }
}

/// Is cursor `cursor` an eraser? Drivers that don't report `CSR_TYPE` follow Wacom's convention
/// of repeating cursors in groups of puck, pen and eraser, so every third one is the eraser.
pub fn eraser(cursor: u32, cursor_type: Option<u32>) -> bool {
    match cursor_type.filter(|t| *t != 0) {
        Some(t) => is_eraser(t),
        None => cursor % 3 == 2,
    }
}

/// Azimuth and altitude (tenths of a degree) as W3C tilt in degrees. Qt's conversion: the pen's
/// horizontal direction is the azimuth, its height over the tablet the altitude.
pub fn tilt(azimuth: i32, altitude: i32) -> (f32, f32) {
    let az = f64::from(azimuth) / 10.0;
    let tan_alt = (f64::from(altitude).abs() / 10.0).to_radians().tan();
    if !tan_alt.is_finite() || tan_alt.abs() < 1e-9 {
        // Lying flat: no azimuth can be told apart; the pen tilts fully towards the user when
        // the azimuth points that way, which `cos` gives.
        let y = if az.to_radians().cos() >= 0.0 { -90.0 } else { 90.0 };
        return (0.0, y as f32);
    }
    let rad = az.to_radians();
    let x = (rad.sin() / tan_alt).atan().to_degrees() as f32;
    let y = -(rad.cos() / tan_alt).atan().to_degrees() as f32;
    // Rounding leaves tiny values where the pen is upright: report them as no tilt.
    (if x.abs() < 1e-4 { 0.0 } else { x }, if y.abs() < 1e-4 { 0.0 } else { y })
}

/// Barrel rotation (`orTwist`, tenths of a degree) clockwise from 0, as the other readers report.
pub fn rotation(twist: i32) -> f32 {
    (360.0 - f64::from(twist) / 10.0) as f32
}

/// The pen sample of a packet. A lifted pen (`pkButtons` bit 0 clear) has no pressure, whatever
/// the driver left in the field.
pub fn sample(p: &RawPacket, pressure: Axis, eraser: bool) -> Sample {
    let touching = p.buttons & 0x1 != 0;
    let (tilt_x, tilt_y) = tilt(p.orientation[0], p.orientation[1]);
    Sample {
        pressure: if touching { pressure.unit(p.pressure) } else { 0.0 },
        tilt_x,
        tilt_y,
        rotation: rotation(p.orientation[2]),
        eraser,
    }
    .sanitized()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(p: &RawPacket) -> [u8; PACKET_BYTES] {
        let mut b = [0u8; PACKET_BYTES];
        for (i, v) in [p.cursor, p.buttons, p.pressure, p.orientation[0] as u32, p.orientation[1] as u32, p.orientation[2] as u32]
            .into_iter()
            .enumerate()
        {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn a_packet_round_trips_its_layout() {
        let p = RawPacket { cursor: 7, buttons: 1, pressure: 512, orientation: [900, 450, 2700] };
        assert_eq!(RawPacket::parse(&bytes(&p)), Some(p));
        assert_eq!(RawPacket::parse(&bytes(&p)[..PACKET_BYTES - 1]), None);
        assert_eq!(RawPacket::parse(&[]), None);
    }

    #[test]
    fn the_pump_reads_a_batch_of_packets_in_order() {
        let mut pump = Pump::with_pressure(Axis { min: 0, max: 1023 });
        let mut batch = Vec::new();
        for p in [100u32, 300, 1023] {
            batch.extend_from_slice(&bytes(&RawPacket { cursor: 1, buttons: 1, pressure: p, orientation: [0, 900, 0] }));
        }
        let samples = pump.drain(&batch, 0.0, |_| None);
        assert_eq!(samples.len(), 3);
        assert!((samples[0].pressure - 100.0 / 1023.0).abs() < 1e-6);
        assert!(samples[1].pressure > samples[0].pressure && samples[2].pressure > samples[1].pressure);
        assert!(!pump.went_away(IDLE_MS - 1.0), "a pen still in use");
    }

    /// A pen is in range while packets keep arriving; once they stop for `IDLE_MS` it left range,
    /// which the caller reports as `None` exactly once.
    #[test]
    fn the_pump_reports_the_pen_leaving_range_once() {
        let mut pump = Pump::with_pressure(Axis { min: 0, max: 1023 });
        assert!(!pump.went_away(1e9), "no pen yet: nothing to retract");
        let _ = pump.drain(&bytes(&RawPacket { cursor: 1, buttons: 1, pressure: 500, ..Default::default() }), 1_000.0, |_| None);
        assert!(!pump.went_away(1_000.0 + IDLE_MS - 1.0));
        assert!(pump.went_away(1_000.0 + IDLE_MS));
        assert!(!pump.went_away(1_000.0 + IDLE_MS), "reported only once");
        // The pen coming back starts a new contact.
        let _ = pump.drain(&bytes(&RawPacket { cursor: 1, buttons: 1, pressure: 500, ..Default::default() }), 5_000.0, |_| None);
        assert!(!pump.went_away(5_000.0 + IDLE_MS - 1.0));
    }

    /// The whole receive path on synthetic driver bytes: cursor types are looked up once and
    /// cached, so an eraser stays an eraser even when the lookup stops answering.
    #[test]
    fn the_pump_asks_for_each_cursor_type_once_and_remembers_it() {
        let mut pump = Pump::with_pressure(Axis { min: 0, max: 1023 });
        let mut asked = Vec::new();
        let mut lookup = |cursor: u32| {
            asked.push(cursor);
            Some(if cursor == 2 { 0x080A } else { 0x0802 })
        };
        let pen = bytes(&RawPacket { cursor: 1, buttons: 1, pressure: 1023, ..Default::default() });
        let eraser = bytes(&RawPacket { cursor: 2, buttons: 1, pressure: 1023, ..Default::default() });
        let mut batch = Vec::new();
        batch.extend_from_slice(&pen);
        batch.extend_from_slice(&eraser);
        batch.extend_from_slice(&pen);
        let samples = pump.drain(&batch, 0.0, &mut lookup);
        assert_eq!(asked, [1, 2], "each cursor type is read once");
        assert!(!samples[0].eraser && samples[1].eraser && !samples[2].eraser);
        // Past the cache, a driver that stops answering changes nothing.
        let samples = pump.drain(&eraser, 1.0, |_| panic!("cached cursors are not asked again"));
        assert!(samples[0].eraser);
    }

    #[test]
    fn the_pump_ignores_trailing_bytes_and_survives_an_empty_read() {
        let mut pump = Pump::default();
        assert!(pump.drain(&[], 0.0, |_| None).is_empty());
        let mut short = bytes(&RawPacket { buttons: 1, pressure: 700, ..Default::default() }).to_vec();
        short.extend_from_slice(&[1, 2, 3]);
        assert_eq!(pump.drain(&short, 0.0, |_| None).len(), 1, "a partial packet is dropped");
    }

    #[test]
    fn pressure_is_scaled_and_zero_while_lifted() {
        let axis = Axis { min: 0, max: 1023 };
        assert_eq!(axis.unit(0), 0.0);
        assert!((axis.unit(512) - 0.5005).abs() < 1e-3);
        assert_eq!(axis.unit(1023), 1.0);
        assert_eq!(axis.unit(u32::MAX), 1.0);
        // A degenerate axis reads as a mouse.
        assert_eq!(Axis { min: 5, max: 5 }.unit(5), 1.0);
        let mut p = RawPacket { buttons: 1, pressure: 1023, ..Default::default() };
        assert_eq!(sample(&p, axis, false).pressure, 1.0);
        p.buttons = 0;
        assert_eq!(sample(&p, axis, false).pressure, 0.0, "a lifted pen never keeps its pressure");
        // A side button alone is no contact either.
        p.buttons = 2;
        assert_eq!(sample(&p, axis, false).pressure, 0.0);
    }

    #[test]
    fn orientation_becomes_tilt_and_rotation() {
        // Upright (90°): no tilt. Azimuth 0 points away from the user, 90° to the right.
        assert_eq!(tilt(0, 900), (0.0, 0.0));
        let (x, y) = tilt(900, 450);
        assert!((x - 45.0).abs() < 0.1 && y.abs() < 0.1, "{x}, {y}");
        let (x, y) = tilt(0, 450);
        assert!(x.abs() < 0.1 && (y + 45.0).abs() < 0.1, "{x}, {y}");
        let (x, y) = tilt(1800, 450);
        assert!(x.abs() < 0.1 && (y - 45.0).abs() < 0.1, "{x}, {y}");
        // Flat on the tablet: a full tilt, never a NaN.
        let (x, y) = tilt(0, 0);
        assert_eq!((x, y), (0.0, -90.0));
        assert!(tilt(i32::MIN, i32::MAX).0.is_finite() && tilt(i32::MIN, i32::MAX).1.is_finite());
        // Artistic pen barrel rotation, clockwise from 0.
        assert_eq!(rotation(0), 360.0);
        assert_eq!(rotation(900), 270.0);
        assert_eq!(sample(&RawPacket { orientation: [0, 900, 3600], ..Default::default() }, Axis::default(), false).rotation, 0.0);
    }

    #[test]
    fn the_eraser_end_is_recognised() {
        assert!(!is_eraser(0x0802), "a pen");
        assert!(is_eraser(0x080A), "a pen's eraser end (bit 0x0008)");
        assert!(!is_eraser(0x0804));
        assert!(is_eraser(0x080C), "an Art Pen's eraser");
        assert!(!is_eraser(0x0902), "an airbrush");
        assert!(is_eraser(0x090A), "an airbrush's eraser");
        assert!(!is_eraser(0x0004), "a mouse");
        assert!(!is_eraser(0x0006), "a puck");
        assert!(!is_eraser(0x4020), "Surface Pro's pen (no eraser bit)");
        // Without CSR_TYPE, every third cursor of a group is the eraser.
        assert!(!eraser(0, None) && !eraser(1, None) && eraser(2, None));
        assert!(eraser(0, Some(0x080A)) && !eraser(0, Some(0x0802)));
        assert!(eraser(2, Some(0)), "a zero CSR_TYPE falls back to the convention");
    }

    #[test]
    fn a_full_packet_maps_through() {
        let p = RawPacket { cursor: 2, buttons: 1, pressure: 767, orientation: [900, 450, 1800] };
        let s = sample(&p, Axis { min: 0, max: 1023 }, eraser(2, Some(0x080A)));
        assert!(s.eraser && (s.pressure - 0.75).abs() < 0.01 && (s.tilt_x - 45.0).abs() < 0.2 && (s.rotation - 180.0).abs() < 0.1);
        assert_eq!(packet_mask(), PK_CURSOR | PK_BUTTONS | PK_NORMAL_PRESSURE | PK_ORIENTATION);
        assert_eq!(PACKET_BYTES, 6 * 4, "cursor, buttons, pressure, then 3 orientation ints");
    }
}
