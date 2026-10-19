//! C3's 38-band Kubelka-Munk pigment response. Alpha stays with the compositor.
use std::sync::OnceLock;

const CMF: [[f32; 3]; 38] = [
    [6.4692e-05, 1.8443e-06, 0.0003050171],
    [0.0002194099, 6.2053e-06, 0.0010368067],
    [0.0011205744, 3.10096e-05, 0.005313136],
    [0.0037666134, 0.0001047484, 0.017954393],
    [0.011_880_553, 0.0003536405, 0.057077582],
    [0.023_286_443, 0.0009514714, 0.11365162],
    [0.034559418, 0.0022822632, 0.17335873],
    [0.03722379, 0.004207329, 0.196_206_57],
    [0.032418376, 0.0066887984, 0.186_082_38],
    [0.021_233_205, 0.009888396, 0.139_950_47],
    [0.010490991, 0.015_249_452, 0.08917453],
    [0.0032958376, 0.02141831, 0.04789621],
    [0.0005070352, 0.033_422_932, 0.028_145_626],
    [0.0009486742, 0.051310014, 0.016137662],
    [0.006273718, 0.070402084, 0.0077591019],
    [0.016864624, 0.08783871, 0.004296148],
    [0.028689649, 0.094249054, 0.002005509],
    [0.042674812, 0.09795667, 0.0008614711],
    [0.05625475, 0.094_152_18, 0.0003690387],
    [0.0694704, 0.086781024, 0.0001914287],
    [0.08305315, 0.078856534, 0.0001495556],
    [0.0861261, 0.063526703, 9.23109e-05],
    [0.090_466_134, 0.053741417, 6.81349e-05],
    [0.08500387, 0.042646064, 2.88264e-05],
    [0.07090667, 0.03161735, 1.57672e-05],
    [0.050628892, 0.020885206, 3.9406e-06],
    [0.03547396, 0.01386011, 1.584e-06],
    [0.021_468_211, 0.0081026402, 0.0],
    [0.012516457, 0.0046301023, 0.0],
    [0.0068045816, 0.00249138, 0.0],
    [0.0034645658, 0.0012593034, 0.0],
    [0.0014976098, 0.0005416465, 0.0],
    [0.0007697005, 0.0002779529, 0.0],
    [0.0004073681, 0.0001471081, 0.0],
    [0.0001690104, 6.10327e-05, 0.0],
    [9.52245e-05, 3.43873e-05, 0.0],
    [4.9031e-05, 1.7706e-05, 0.0],
    [2e-05, 7.221e-06, 0.0],
];

fn lut() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| {
        include_bytes!("../assets/PM_lut.bin")
            .chunks_exact(2)
            .map(|v| {
                let bits = u16::from_le_bytes([v[0], v[1]]);
                let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
                let exponent = i32::from((bits >> 10) & 31);
                let mantissa = f32::from(bits & 1023);
                if exponent == 0 { sign * mantissa * 2.0_f32.powi(-24) } else { sign * (1.0 + mantissa / 1024.0) * 2.0_f32.powi(exponent - 15) }
            })
            .collect()
    })
}

fn lookup(rgb: [f32; 3]) -> ([f32; 38], f32) {
    let xyz = rgb.map(|v| v.clamp(0.0, 1.0) * 31.0);
    let lo = xyz.map(|v| v.floor() as usize);
    let f = std::array::from_fn::<_, 3, _>(|i| xyz[i] - lo[i] as f32);
    let table = lut();
    let mut channels = [0.0; 40];
    for band in 0..10 {
        for dz in 0..2 {
            for dy in 0..2 {
                for dx in 0..2 {
                    let x = (lo[0] + dx).min(31);
                    let y = (lo[1] + dy).min(31);
                    let z = (lo[2] + dz).min(31) + band * 32;
                    let weight = if dx == 0 { 1.0 - f[0] } else { f[0] } * if dy == 0 { 1.0 - f[1] } else { f[1] } * if dz == 0 { 1.0 - f[2] } else { f[2] };
                    let offset = ((z * 32 + y) * 32 + x) * 4;
                    for ch in 0..4 {
                        if let (Some(out), Some(v)) = (channels.get_mut(band * 4 + ch), table.get(offset + ch)) {
                            *out += v * weight;
                        }
                    }
                }
            }
        }
    }
    let ks = std::array::from_fn(|i| (channels[i].exp2() - 1.0).max(0.0));
    (ks, channels[38].clamp(0.0, 1.0))
}

fn mix(from: [f32; 3], to: [f32; 3], t: f32) -> [f32; 3] {
    if t <= 0.000001 {
        return from;
    }
    if t >= 0.999999 {
        return to;
    }
    let (a, la) = lookup(from);
    let (b, lb) = lookup(to);
    let wa = (1.0 - t).powi(2) * la;
    let wb = t * t * lb;
    let sum = wa + wb;
    if sum <= 0.000001 {
        return if t < 0.5 { from } else { to };
    }
    let mut xyz = [0.0; 3];
    for ((a, b), cmf) in a.into_iter().zip(b).zip(CMF) {
        let ks = (a * wa + b * wb) / sum;
        let reflectance = 1.0 / (1.0 + ks + (ks * ks + 2.0 * ks).sqrt());
        for (channel, matching) in xyz.iter_mut().zip(cmf) {
            *channel += reflectance * matching;
        }
    }
    let [x, y, z] = xyz;
    [3.24097 * x - 1.5373832 * y - 0.49861076 * z, -0.96924365 * x + 1.8759675 * y + 0.04155506 * z, 0.05563008 * x - 0.20397696 * y + 1.0569715 * z]
        .map(|v| if v < 0.0031308 { v.max(0.0) * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 })
        .map(|v| v.clamp(0.0, 1.0))
}

fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Apply pigment colour to an already composited pixel, preserving its alpha.
pub fn paint(normal: [f32; 4], dst: [f32; 4], src: [f32; 4], amount: f32, strength: f32) -> [f32; 4] {
    let support = smooth(0.02, 0.22, dst[3]);
    let weight = strength.clamp(0.0, 1.0) * support;
    if weight <= 0.0001 {
        return normal;
    }
    let base = std::array::from_fn(|i| normal[i] + (dst[i] - normal[i]) * support);
    let source = [src[0], src[1], src[2]];
    let t = amount.clamp(0.0, 1.0);
    let pigment = mix(base, source, t);
    let gate = smooth(0.025, 0.16, t);
    let rgb: [f32; 3] = std::array::from_fn(|i| {
        let linear = base[i] + (source[i] - base[i]) * t;
        let mixed = linear + (pigment[i] - linear) * gate;
        normal[i] + (mixed - normal[i]) * weight
    });
    [rgb[0], rgb[1], rgb[2], normal[3]]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pigment_changes_overlap_color_but_preserves_alpha_and_empty_paper() {
        let src = [0., 0., 1., 1.];
        let dst = [1., 1., 0., 1.];
        let normal = [0.5, 0.5, 0.5, 1.];
        assert_eq!(paint(normal, dst, src, 0.5, 0.), normal);
        let mixed = paint(normal, dst, src, 0.5, 1.);
        assert!(mixed[1] > mixed[0] && mixed[1] > mixed[2], "blue + yellow should mix green: {mixed:?}");
        assert_eq!(mixed[3], normal[3]);
        let empty = [0., 0., 0., 0.];
        let output = [0., 0., 1., 0.5];
        assert_eq!(paint(output, empty, src, 1., 1.), output);
        assert_eq!(mix([0.2, 0.4, 0.6], [1., 0., 0.], 0.), [0.2, 0.4, 0.6]);
    }
}
