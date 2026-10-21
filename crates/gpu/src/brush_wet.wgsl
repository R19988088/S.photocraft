struct Params {
    rect: vec4<u32>,
    style: u32,
    width_mode: u32,
    color_mode: u32,
    edge_control: u32,
    softness: f32,
    brightness: f32,
    saturation: f32,
    padding: f32,
    resolved: vec4<f32>,
}
@group(0) @binding(0) var coverage: texture_2d<f32>;
@group(0) @binding(1) var depth: texture_2d<f32>;
@group(0) @binding(2) var base: texture_2d<f32>;
@group(0) @binding(3) var source: texture_2d<f32>;
@group(0) @binding(4) var output: texture_storage_2d<rgba32float, write>;
@group(0) @binding(5) var<uniform> p: Params;

fn unit(x: f32) -> f32 { return clamp(x, 0.0, 1.0); }
fn lerp(a: f32, b: f32, t: f32) -> f32 { return a + (b - a) * t; }
fn smooth_value(a: f32, b: f32, x: f32) -> f32 {
    let t = unit((x - a) / max(b - a, 0.00001));
    return t * t * (3.0 - 2.0 * t);
}
fn continuous(v: f32) -> f32 {
    if v <= 0.75 { return v; }
    return 1.0 - 0.25 / (1.0 + 4.0 * (v - 0.75));
}
fn width01(w: f32) -> f32 {
    if p.style == 0u { return unit(w); }
    return continuous(clamp(w, 0.0, 5.0));
}
fn strength01(strength: f32) -> f32 {
    let s = clamp(strength, 0.0, select(2.8, 2.0, p.style == 0u));
    let e = s * (0.6 + 0.4 * smooth_value(0.3, 0.7, s));
    if p.style == 0u { return unit(e); }
    return continuous(e);
}
fn color_strength(strength: f32) -> f32 {
    if strength <= 0.0001 { return 0.0; }
    if p.style == 0u { return unit(strength); }
    return strength01(strength);
}
fn color_mask(a: f32, w: f32) -> f32 {
    let t = width01(w);
    if p.style == 1u {
        return smooth_value(0.008, lerp(0.04, 0.12, t), a) * (1.0 - smooth_value(lerp(0.45, 0.72, t), lerp(0.78, 0.96, t), a));
    }
    if p.style == 0u {
        return smooth_value(0.01, lerp(0.06, 0.14, t), a) * (1.0 - smooth_value(lerp(0.32, 0.58, t), lerp(0.66, 0.90, t), a));
    }
    return 1.0;
}
fn dab_edge(a: f32, rn: f32, wet: vec3<f32>) -> f32 {
    if p.width_mode == 1u { return smooth_value(max(1.0 - wet.z, 0.0), 1.0, rn); }
    if p.style == 2u { return smooth_value(1.0 - lerp(0.10, 0.32, width01(wet.z)), 1.0, rn); }
    return color_mask(a, wet.z);
}
fn wet_alpha(a: f32, edge: f32, wet: vec3<f32>, single: bool) -> f32 {
    if a <= 0.0 || wet.x <= 0.0001 { return a; }
    let t = width01(wet.y);
    var lift_max = lerp(0.55, 0.85, t);
    if p.style == 1u { lift_max = lerp(0.40, 0.70, t); }
    if p.style == 2u { lift_max = lerp(0.50, 0.85, t); }
    let lift = strength01(wet.x) * lift_max * edge;
    let sw = width01(select(wet.y, wet.z, single));
    var soften = edge;
    if p.style == 1u {
        soften = smooth_value(0.008, lerp(0.03, 0.10, sw), a) * (1.0 - smooth_value(lerp(0.24, 0.42, sw), lerp(0.42, 0.68, sw), a));
    }
    if p.style == 0u {
        soften = smooth_value(0.01, lerp(0.04, 0.11, sw), a) * (1.0 - smooth_value(lerp(0.18, 0.34, sw), lerp(0.34, 0.60, sw), a));
    }
    var lifted = 1.0 - pow(1.0 - unit(a), 1.0 + 3.0 * lift);
    if p.style == 0u { lifted = a + (1.0 - a) * lift; }
    let filtered = unit(lifted * (1.0 - unit(p.softness / 5.0) * soften * 0.75));
    if p.edge_control == 0u { return max(a, filtered); }
    return filtered;
}
fn sample_coverage(q: vec2<i32>) -> f32 {
    if any(q < vec2<i32>(0)) || any(q >= vec2<i32>(textureDimensions(coverage))) { return 0.0; }
    return textureLoad(coverage, q, 0).r;
}
fn spatial_edge(a: f32, w: f32, q: vec2<i32>) -> f32 {
    let step = max(1.0 + clamp(w, 0.0, 5.0) * 1.5, 1.0) / 3.0;
    var gradient = 0.0;
    var spread = 0.0;
    for (var dy = -3; dy <= 3; dy++) {
        for (var dx = -3; dx <= 3; dx++) {
            if dx == 0 && dy == 0 { continue; }
            let pos = vec2<f32>(q) + vec2<f32>(f32(dx), f32(dy)) * step;
            let lo = vec2<i32>(floor(pos));
            let f = pos - floor(pos);
            let n = lerp(lerp(sample_coverage(lo), sample_coverage(lo + vec2<i32>(1, 0)), f.x),
                         lerp(sample_coverage(lo + vec2<i32>(0, 1)), sample_coverage(lo + vec2<i32>(1, 1)), f.x), f.y);
            let weight = f32((4 - abs(dx)) * (4 - abs(dy)));
            gradient += weight * max(a - n, 0.0);
            spread += weight * abs(a - n);
        }
    }
    let signal = (spread * 1.55 + gradient * 1.75) / 240.0;
    let t = width01(w);
    return unit(signal / (0.25 + signal) * smooth_value(0.02, lerp(0.16, 0.34, t), a)
        * (1.0 - smooth_value(lerp(0.42, 0.70, t), lerp(0.80, 0.98, t), a)) * smooth_value(0.02, 0.3, a));
}
fn rgb_to_hsl(c: vec3<f32>) -> vec3<f32> {
    let hi = max(max(c.r, c.g), c.b);
    let lo = min(min(c.r, c.g), c.b);
    let l = (hi + lo) * 0.5;
    let d = hi - lo;
    if d < 0.000001 { return vec3<f32>(0.0, 0.0, l); }
    let s = d / max(1.0 - abs(2.0 * l - 1.0), 0.000001);
    var h = (c.r - c.g) / d + 4.0;
    if hi == c.r {
        let raw = (c.g - c.b) / d;
        h = raw - 6.0 * floor(raw / 6.0);
    } else if hi == c.g { h = (c.b - c.r) / d + 2.0; }
    return vec3<f32>(h / 6.0, s, l);
}
fn hsl_to_rgb(hsl: vec3<f32>) -> vec3<f32> {
    let c = (1.0 - abs(2.0 * hsl.z - 1.0)) * hsl.y;
    let h = hsl.x * 6.0;
    let x = c * (1.0 - abs((h - 2.0 * floor(h / 2.0)) - 1.0));
    let m = hsl.z - c * 0.5;
    var rgb = vec3<f32>(c, 0.0, x);
    if h < 1.0 { rgb = vec3<f32>(c, x, 0.0); }
    else if h < 2.0 { rgb = vec3<f32>(x, c, 0.0); }
    else if h < 3.0 { rgb = vec3<f32>(0.0, c, x); }
    else if h < 4.0 { rgb = vec3<f32>(0.0, x, c); }
    else if h < 5.0 { rgb = vec3<f32>(x, 0.0, c); }
    return rgb + vec3<f32>(m);
}
fn color_with_effect(paint: vec4<f32>, backdrop: vec4<f32>, effect_in: f32) -> vec4<f32> {
    if p.color_mode == 0u || effect_in <= 0.0 { return paint; }
    let watercolor = p.style != 0u;
    if watercolor && backdrop.a <= 0.0001 { return paint; }
    let c = select(paint, backdrop, watercolor);
    let effect = select(effect_in, effect_in * unit(backdrop.a), watercolor);
    let hsl = rgb_to_hsl(c.rgb);
    var sat = 0.05;
    var drop = 0.10;
    if p.style == 1u { sat = 0.27; drop = 0.45; }
    if p.style == 2u { sat = 0.33; drop = 0.54; }
    let gain = select(1.0, 2.0, watercolor);
    let tone = hsl_to_rgb(vec3<f32>(hsl.x, unit(hsl.y + sat + p.saturation * gain), unit(hsl.z - drop - p.brightness * gain)));
    var tone_target = tone;
    if p.color_mode == 2u { tone_target = c.rgb * tone; }
    if p.color_mode == 3u { tone_target = clamp(vec3<f32>(1.0) - (vec3<f32>(1.0) - c.rgb) / max(tone, vec3<f32>(0.01)), vec3<f32>(0.0), vec3<f32>(1.0)); }
    return vec4<f32>(min(paint.rgb + (tone_target - paint.rgb) * effect, paint.rgb), paint.a);
}
fn inside(gid: vec3<u32>) -> bool {
    return all(gid.xy < p.rect.zw) && all(gid.xy + p.rect.xy < textureDimensions(output));
}
@compute @workgroup_size(8, 8)
fn stroke_coverage(@builtin(global_invocation_id) gid: vec3<u32>) {
    if !inside(gid) { return; }
    let q = vec2<i32>(gid.xy + p.rect.xy);
    let a = sample_coverage(q);
    let wet = textureLoad(depth, q, 0).xyz;
    var edge = 0.0;
    if a > 0.0 {
        edge = color_mask(a, wet.y);
        if p.style == 2u { edge = spatial_edge(a, wet.y, q); }
    }
    textureStore(output, q, vec4<f32>(wet_alpha(a, edge, wet, false), edge, color_strength(wet.x) * edge, 0.0));
}
@compute @workgroup_size(8, 8)
fn single_dab_coverage(@builtin(global_invocation_id) gid: vec3<u32>) {
    if !inside(gid) { return; }
    let q = vec2<i32>(gid.xy + p.rect.xy);
    let a = sample_coverage(q);
    let wet = p.resolved.xyz;
    var edge = 0.0;
    if a > 0.0 { edge = dab_edge(a, textureLoad(depth, q, 0).r, wet); }
    textureStore(output, q, vec4<f32>(wet_alpha(a, edge, wet, true), edge, color_strength(wet.x) * edge, 0.0));
}
@compute @workgroup_size(8, 8)
fn wet_color(@builtin(global_invocation_id) gid: vec3<u32>) {
    if !inside(gid) { return; }
    let q = vec2<i32>(gid.xy + p.rect.xy);
    textureStore(output, q, color_with_effect(textureLoad(source, q, 0), textureLoad(base, q, 0), textureLoad(coverage, q, 0).b));
}
@compute @workgroup_size(8, 8)
fn single_accumulated_color(@builtin(global_invocation_id) gid: vec3<u32>) {
    if !inside(gid) { return; }
    let q = vec2<i32>(gid.xy + p.rect.xy);
    textureStore(output, q, color_with_effect(textureLoad(source, q, 0), textureLoad(base, q, 0), textureLoad(depth, q, 0).a));
}
