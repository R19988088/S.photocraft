//! C3 colour-only attributes. Mode numbers belong to the brush format, not layer blending.
use photocraft_color::BlendMode;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(u8)]
pub enum AttributeMode {
    #[default]
    Off = 0,
    Normal,
    Darken,
    Multiply,
    ColorBurn,
    LinearBurn,
    DarkerColor,
    Lighten,
    Screen,
    ColorDodge,
    LinearDodge,
    LighterColor,
    Overlay,
    SoftLight,
    HardLight,
    VividLight,
    LinearLight,
    PinLight,
    HardMix,
    Difference,
    Exclusion,
    Subtract,
    Divide,
    Hue,
    Saturation,
    Color,
    Luminosity,
    WatercolorBurn,
    Spectral,
    OverlaySp,
    Shine,
    LumiShade,
    Invert = 33,
}
impl AttributeMode {
    pub const ALL: [Self; 33] = [
        Self::Off,
        Self::Normal,
        Self::Darken,
        Self::Multiply,
        Self::ColorBurn,
        Self::LinearBurn,
        Self::DarkerColor,
        Self::Lighten,
        Self::Screen,
        Self::ColorDodge,
        Self::LinearDodge,
        Self::LighterColor,
        Self::Overlay,
        Self::SoftLight,
        Self::HardLight,
        Self::VividLight,
        Self::LinearLight,
        Self::PinLight,
        Self::HardMix,
        Self::Difference,
        Self::Exclusion,
        Self::Subtract,
        Self::Divide,
        Self::Hue,
        Self::Saturation,
        Self::Color,
        Self::Luminosity,
        Self::WatercolorBurn,
        Self::Spectral,
        Self::OverlaySp,
        Self::Shine,
        Self::LumiShade,
        Self::Invert,
    ];
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|m| *m as u32 == code)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Normal => "Normal",
            Self::Darken => "Darken",
            Self::Multiply => "Multiply",
            Self::ColorBurn => "Color Burn",
            Self::LinearBurn => "Linear Burn",
            Self::DarkerColor => "Darker Color",
            Self::Lighten => "Lighten",
            Self::Screen => "Screen",
            Self::ColorDodge => "Color Dodge",
            Self::LinearDodge => "Linear Dodge",
            Self::LighterColor => "Lighter Color",
            Self::Overlay => "Overlay",
            Self::SoftLight => "Soft Light",
            Self::HardLight => "Hard Light",
            Self::VividLight => "Vivid Light",
            Self::LinearLight => "Linear Light",
            Self::PinLight => "Pin Light",
            Self::HardMix => "Hard Mix",
            Self::Difference => "Difference",
            Self::Exclusion => "Exclusion",
            Self::Subtract => "Subtract",
            Self::Divide => "Divide",
            Self::Hue => "Hue",
            Self::Saturation => "Saturation",
            Self::Color => "Color",
            Self::Luminosity => "Luminosity",
            Self::WatercolorBurn => "Watercolor Burn",
            Self::Spectral => "Spectral",
            Self::OverlaySp => "Overlay SP",
            Self::Shine => "Shine",
            Self::LumiShade => "Lumi Shade",
            Self::Invert => "Invert",
        }
    }
    pub fn blend(self, d: [f32; 3], s: [f32; 3]) -> [f32; 3] {
        let ordinary = match self {
            Self::Darken => Some(BlendMode::Darken),
            Self::Multiply => Some(BlendMode::Multiply),
            Self::LinearBurn => Some(BlendMode::LinearBurn),
            Self::Lighten => Some(BlendMode::Lighten),
            Self::Screen => Some(BlendMode::Screen),
            Self::LinearDodge => Some(BlendMode::LinearDodge),
            Self::Overlay => Some(BlendMode::Overlay),
            Self::HardLight => Some(BlendMode::HardLight),
            Self::LinearLight => Some(BlendMode::LinearLight),
            Self::PinLight => Some(BlendMode::PinLight),
            Self::Difference => Some(BlendMode::Difference),
            Self::Exclusion => Some(BlendMode::Exclusion),
            Self::Subtract => Some(BlendMode::Subtract),
            _ => None,
        };
        if let Some(mode) = ordinary {
            return photocraft_color::blend::blend_rgb(mode, d, s);
        }
        let burn = |a: f32, b: f32| if b <= 0.0001 { 0.0 } else { 1.0 - ((1.0 - a) / b).min(1.0) };
        let dodge = |a: f32, b: f32| if b >= 0.9999 { 1.0 } else { (a / (1.0 - b).max(0.0001)).min(1.0) };
        match self {
            Self::ColorBurn => std::array::from_fn(|i| burn(d[i], s[i])),
            Self::ColorDodge => std::array::from_fn(|i| dodge(d[i], s[i])),
            Self::SoftLight => std::array::from_fn(|i| {
                if s[i] <= 0.5 { d[i] - (1.0 - 2.0 * s[i]) * d[i] * (1.0 - d[i]) } else { d[i] + (2.0 * s[i] - 1.0) * (d[i].max(0.0).sqrt() - d[i]) }
            }),
            Self::VividLight | Self::HardMix => std::array::from_fn(|i| {
                let v = if s[i] <= 0.5 { burn(d[i], 2.0 * s[i]) } else { dodge(d[i], 2.0 * (s[i] - 0.5)) };
                if self == Self::HardMix { if v >= 0.5 { 1.0 } else { 0.0 } } else { v }
            }),
            Self::Divide => std::array::from_fn(|i| if s[i] <= 0.0001 { 1.0 } else { (d[i] / s[i]).clamp(0.0, 1.0) }),
            Self::Hue | Self::Saturation => {
                let a = hsl(d);
                let b = hsl(s);
                rgb(if self == Self::Hue { [b[0], a[1], a[2]] } else { [a[0], b[1], a[2]] })
            }
            Self::Color => set_lum(s, lum(d)),
            Self::Luminosity => set_lum(d, lum(s)),
            Self::DarkerColor => {
                if d.iter().sum::<f32>() <= s.iter().sum::<f32>() {
                    d
                } else {
                    s
                }
            }
            Self::LighterColor => {
                if d.iter().sum::<f32>() >= s.iter().sum::<f32>() {
                    d
                } else {
                    s
                }
            }
            Self::Invert => d.map(|x| 1.0 - x),
            Self::Spectral => {
                let intensity = 1.0 - (1.0 - lum(d)) * (1.0 - lum(s));
                let rainbow = rgb([(intensity * 4.0).fract(), 1.0, 0.5]);
                let t = ((1.0 - s.into_iter().fold(f32::INFINITY, f32::min)) * 0.8).clamp(0.0, 1.0);
                std::array::from_fn(|i| 1.0 - (1.0 - d[i]) * (1.0 - (rainbow[i] + (s[i] - rainbow[i]) * t)))
            }
            Self::OverlaySp => {
                let a = lum(d);
                let b = lum(s);
                let l = if a >= 0.5 { 1.0 - 2.0 * (1.0 - a) * (1.0 - b) } else { 2.0 * a * b };
                let t = (0.2 + (l - a).abs() * 1.6).clamp(0.0, 1.0);
                let c = lerp(d, s, t);
                let scale = l / lum(c).max(0.0001);
                c.map(|x| (x * scale).clamp(0.0, 1.0))
            }
            Self::Shine => {
                let l = lum(d);
                let c = std::array::from_fn(|i| (d[i] + s[i] * (l * 0.85 + 0.15)).clamp(0.0, 1.0));
                let out = lum(c).max(0.0001);
                let boost = smooth(0.35, 0.95, l) * 0.16;
                c.map(|v| (out + (v - out) * (1.0 + boost)).clamp(0.0, 1.0))
            }
            Self::LumiShade => {
                let a = lum(s);
                std::array::from_fn(|i| {
                    if a > 0.5 {
                        (d[i] + 2.0 * (a - 0.5) * s[i] * d[i]).clamp(0.0, 1.0)
                    } else {
                        (d[i] * (1.0 - 2.0 * (0.5 - a) * (1.0 - d[i]))).clamp(0.0, 1.0)
                    }
                })
            }
            Self::WatercolorBurn => {
                let burned = std::array::from_fn(|i| burn(d[i], s[i]));
                let a = hsl(d);
                let b = hsl(s);
                let c = hsl(burned);
                let gate = ((1.0 - smooth(0.72, 1.0, a[2])) * (0.35 + 0.65 * smooth(0.015, 0.16, a[1]))).clamp(0.0, 1.0);
                let target = rgb([a[0], a[1], (a[2] - (a[2] - c[2]).max(0.0) * gate).clamp(0.0, 1.0)]);
                let mixed = hsl(lerp(d, target, gate * gate * (3.0 - 2.0 * gate)));
                let bias = gate * smooth(0.08, 0.65, b[1]);
                let dh = (b[0] - a[0] + 0.5).rem_euclid(1.0) - 0.5;
                rgb([(a[0] + dh * 0.42 * bias).rem_euclid(1.0), (a[1] + (b[1] - a[1]) * 0.3 * bias).clamp(0.0, 1.0), mixed[2].min(a[2])])
            }
            _ => s,
        }
    }
}
fn lum(c: [f32; 3]) -> f32 {
    c[0] * 0.3 + c[1] * 0.59 + c[2] * 0.11
}
fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}
fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let mut o = c.map(|v| v + l - lum(c));
    let n = o.into_iter().fold(f32::INFINITY, f32::min);
    let x = o.into_iter().fold(f32::NEG_INFINITY, f32::max);
    if n < 0.0 {
        o = o.map(|v| l + (v - l) * l / (l - n).max(0.0001));
    }
    if x > 1.0 {
        o = o.map(|v| l + (v - l) * (1.0 - l) / (x - l).max(0.0001));
    }
    o.map(|v| v.clamp(0.0, 1.0))
}
fn hsl(c: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = c;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;
    let d = max - min;
    if d < 0.000001 {
        return [0.0, 0.0, l];
    }
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    [h / 6.0, s, l]
}
fn rgb([h, s, l]: [f32; 3]) -> [f32; 3] {
    if s < 0.000001 {
        return [l; 3];
    }
    let q = if l >= 0.5 { l + s - l * s } else { l * (1.0 + s) };
    let p = 2.0 * l - q;
    let hue = |t: f32| {
        let t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    [hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0)]
}

/// C3 stores a weighted HSL saturation moment independently of coverage.
pub(crate) fn saturation(rgb: [f32; 3]) -> f32 {
    hsl(rgb.map(|v| v.clamp(0.0, 1.0)))[1]
}
pub(crate) fn preserve_saturation(output: [f32; 3], recorded: f32, strength: f32) -> [f32; 3] {
    let mut value = hsl(output.map(|v| v.clamp(0.0, 1.0)));
    let t = strength.clamp(0.0, 1.0) * smooth(0.0001, 0.02, recorded);
    value[1] += (recorded.clamp(0.0, 1.0) - value[1]) * t;
    rgb(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn c3_color_attributes_use_independent_destination_and_preserve_alpha() {
        let attrs =
            crate::ColorAttributes { enabled: true, blend: AttributeMode::Multiply, blend_strength: 0.4, color: AttributeMode::Screen, color_strength: 0.6 };
        let d = [0.2, 0.4, 0.8, 0.7];
        let s = [0.9, 0.3, 0.1, 1.0];
        let normal = [0.55, 0.35, 0.45, 0.85];
        let out = attrs.composite(normal, d, s, 0.5);
        for i in 0..3 {
            let main = d[i] + (d[i] * s[i] - d[i]) * 0.5;
            let secondary = d[i] + (1.0 - (1.0 - d[i]) * (1.0 - s[i]) - d[i]) * 0.5;
            let expected = (normal[i] + (main - normal[i]) * 0.4) * 0.4 + secondary * 0.6;
            assert!((out[i] - expected).abs() < 1e-6);
        }
        assert_eq!(out[3], normal[3]);
    }
    #[test]
    fn c3_darker_color_compares_channel_sums() {
        let red = [0.9, 0.0, 0.0];
        let green = [0.0, 0.8, 0.0];
        assert_eq!(AttributeMode::DarkerColor.blend(red, green), green);
        assert_eq!(AttributeMode::LighterColor.blend(red, green), red);
    }
}
