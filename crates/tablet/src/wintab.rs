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

/// Bytes of a packet with [`packet_mask`]: four 32-bit fields plus the three-int orientation.
pub const PACKET_BYTES: usize = 28;

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
        assert_eq!(PACKET_BYTES, 7 * 4);
    }
}
