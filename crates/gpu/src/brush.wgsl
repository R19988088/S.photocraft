struct Params {
    rect: vec4<u32>,
    offset: vec2<f32>,
    strength: f32,
    blur_radius: f32,
    target_color: vec4<f32>,
    center: vec2<f32>, radius: f32, previous_radius: f32,
    reservoir: vec4<f32>,
    sample_rect: vec4<i32>,
    paint: vec4<f32>,
};
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var coverage: texture_2d<f32>;
@group(0) @binding(2) var output: texture_storage_2d<rgba32float, write>;
@group(0) @binding(3) var<uniform> p: Params;
@group(0) @binding(4) var carry: texture_2d<f32>;
@group(0) @binding(6) var color_source: texture_2d<f32>;
@group(0) @binding(7) var<storage, read> lut: array<f32>;
@group(0) @binding(5) var next_carry: texture_storage_2d<rgba32float, write>;

fn load(q: vec2<i32>) -> vec4<f32> {
    return textureLoad(source, clamp(q, max(p.sample_rect.xy,vec2<i32>(0)), min(p.sample_rect.zw,vec2<i32>(textureDimensions(source))) - vec2<i32>(1)), 0);
}
fn sample_pixel(q: vec2<f32>) -> vec4<f32> {
    let a = vec2<i32>(floor(q)); let f = fract(q);
    return mix(mix(load(a), load(a + vec2<i32>(1, 0)), f.x), mix(load(a + vec2<i32>(0, 1)), load(a + vec2<i32>(1, 1)), f.x), f.y);
}
fn pickup_load(q: vec2<i32>) -> vec4<f32> {
    let at=clamp(q,p.sample_rect.xy,p.sample_rect.zw-vec2<i32>(1))+vec2<i32>(p.paint.xy);
    return textureLoad(color_source,at,0);
}
fn pickup_sample(q: vec2<f32>) -> vec4<f32> {
    let a=vec2<i32>(floor(q)); let f=fract(q);
    return mix(mix(pickup_load(a),pickup_load(a+vec2(1,0)),f.x),mix(pickup_load(a+vec2(0,1)),pickup_load(a+vec2(1,1)),f.x),f.y);
}
fn premul(v: vec4<f32>) -> vec4<f32> { return vec4(v.rgb * v.a, v.a); }

@compute @workgroup_size(8, 8)
fn pickup(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= p.rect.zw) { return; }
    let q = vec2<i32>(id.xy + p.rect.xy);
    let old = load(q);
    var picked = sample_pixel(vec2<f32>(q) + p.offset);
    if p.paint.z>0.0 { picked=pickup_sample(vec2<f32>(q)+p.offset); }
    let amount = clamp(textureLoad(coverage, q, 0).r * p.strength, 0.0, 1.0);
    let a = min(amount, picked.a);
    let weight=clamp(amount*picked.a,0.0,1.0);
    let normal=vec4(mix(old.rgb,picked.rgb,weight),max(old.a,a+old.a*(1.0-a)));
    textureStore(output,q,pigment_paint(normal,old,picked,weight,p.paint.a));
}

@compute @workgroup_size(8, 8)
fn blur(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= p.rect.zw) { return; }
    let q = vec2<i32>(id.xy + p.rect.xy);
    let old = load(q);
    if p.target_color.a >= 0.0 && (old.a <= 0.0001 || distance(old.rgb, p.target_color.rgb) > p.target_color.a) {
        textureStore(output, q, old); return;
    }
    let offsets = array<vec2<f32>, 9>(vec2(0.0), vec2(0.0, -1.0), vec2(0.0, 1.0), vec2(-1.0, 0.0), vec2(1.0, 0.0), vec2(-0.7, -0.7), vec2(0.7, -0.7), vec2(-0.7, 0.7), vec2(0.7, 0.7));
    let weights = array<f32, 9>(0.18, 0.14, 0.14, 0.14, 0.14, 0.10, 0.10, 0.10, 0.10);
    var sum = vec4(0.0); var total = 0.0;
    for (var i = 0u; i < 9u; i++) {
        sum += premul(sample_pixel(vec2<f32>(q) + offsets[i] * p.blur_radius)) * weights[i];
        total += weights[i];
    }
    sum /= total;
    if sum.a <= 0.0 { textureStore(output, q, old); return; }
    let weight = clamp(textureLoad(coverage, q, 0).r * p.strength, 0.0, 1.0);
    let a = mix(old.a, max(old.a, sum.a), weight);
    let rgb = (old.rgb * old.a * (1.0 - weight) + sum.rgb / sum.a * max(old.a, sum.a) * weight) / max(a, 0.000001);
    textureStore(output, q, vec4(rgb, a));
}

fn carry_load(q: vec2<i32>) -> vec4<f32> {
    return textureLoad(carry, clamp(q, vec2<i32>(0), vec2<i32>(textureDimensions(carry)) - vec2<i32>(1)), 0);
}
fn carry_sample(q: vec2<f32>) -> vec4<f32> {
    let a = vec2<i32>(floor(q)); let f = fract(q);
    return mix(mix(carry_load(a), carry_load(a + vec2(1, 0)), f.x), mix(carry_load(a + vec2(0, 1)), carry_load(a + vec2(1, 1)), f.x), f.y);
}
// The carrier dispatch covers the carrier texture, not the document-space dab rectangle.
@compute @workgroup_size(8, 8)
fn carrier_v1(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(next_carry);
    let pos=id.xy+p.rect.xy;
    if any(id.xy>=p.rect.zw)||any(pos >= dims) { return; }
    let local = vec2<i32>(pos) - vec2<i32>(dims / 2u);
    let at=vec2<i32>(floor(p.center))+local;
    let sampled = premul(load(at));
    let retained = carry_load(vec2<i32>(pos));
    var result=select(mix(sampled,retained,p.strength),sampled,p.previous_radius<=0.0);
    if p.previous_radius>0.0 && (any(at<p.sample_rect.xy)||any(at>=p.sample_rect.zw)) { result=retained; }
    textureStore(next_carry, vec2<i32>(pos), result);
}
@compute @workgroup_size(8, 8)
fn carrier_v2(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = textureDimensions(next_carry);
    let pos=id.xy+p.rect.xy;
    if any(id.xy>=p.rect.zw)||any(pos >= dims) { return; }
    let local = vec2<f32>(pos) + vec2(0.5) - vec2<f32>(dims) * 0.5;
    var sampled = premul(sample_pixel(p.center + local - vec2(0.5)));
    var retained = sampled;
    if p.previous_radius > 0.0 {
        retained = carry_sample(local * p.previous_radius / max(p.radius, 0.5) + vec2<f32>(textureDimensions(carry)) * 0.5 - vec2(0.5));
    }
    let sample_alpha16 = floor(clamp(sampled.a, 0.0, 1.0) * 65535.0);
    if sample_alpha16 == 0.0 {
        sampled = vec4(select(vec3(0.0), retained.rgb / max(retained.a, 0.000001) * sampled.a, retained.a > 0.0), sampled.a);
    }
    let base = select(pow(1.0 - p.reservoir.x, 2.0), pow(1.0 - p.reservoir.y, 2.0), sample_alpha16 == 0.0);
    let stretch = clamp(p.reservoir.z, 0.0, 1.0);
    let retention = select(1.0, pow(1.0 - pow(1.0 - stretch, 2.0), 32.0 * p.reservoir.w / max(p.blur_radius, 1.0)), p.reservoir.w > 0.0);
    let dynamic = select(0.0, p.strength / max(p.reservoir.x, 0.0001), p.reservoir.x > 0.0001);
    textureStore(next_carry, vec2<i32>(pos), clamp(mix(retained, sampled, clamp(base * dynamic, 0.0, 1.0) * (1.0 - retention)), vec4(0.0), vec4(1.0)));
}

@compute @workgroup_size(8,8)
fn deposit_v1(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy>=p.rect.zw) { return; }
    let q=vec2<i32>(id.xy+p.rect.xy); let old=load(q);
    let at=q-vec2<i32>(floor(p.center))+vec2<i32>(textureDimensions(carry)/2u);
    if any(at<vec2<i32>(0))||any(at>=vec2<i32>(textureDimensions(carry))) { textureStore(output,q,old); return; }
    let c=textureLoad(carry,at,0);
    let k=textureLoad(coverage,q,0).r*p.strength;
    let a=mix(old.a,c.a,k);
    var rgb=mix(old.rgb,c.rgb,k);
    if a>0.000001 { rgb=(old.rgb*old.a*(1.0-k)+c.rgb*k)/a; }
    textureStore(output,q,vec4(rgb,a));
}
@compute @workgroup_size(8,8)
fn deposit_v2(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy>=p.rect.zw) { return; }
    let q=vec2<i32>(id.xy+p.rect.xy); let old=load(q);
    let local=vec2<f32>(q)-p.center+vec2<f32>(textureDimensions(carry))*0.5;
    let c=carry_sample(local);
    let k=clamp(textureLoad(coverage,q,0).r*p.strength,0.0,1.0);
    let sa=c.a*k; let a=sa+old.a*(1.0-sa);
    if sa<=0.0 { textureStore(output,q,old); return; }
    let normal=vec4((c.rgb*k+old.rgb*old.a*(1.0-sa))/max(a,0.000001),a);
    let src=vec4(c.rgb/max(c.a,0.000001),c.a);
    textureStore(output,q,pigment_paint(normal,old,src,sa/max(a,0.000001),p.paint.a));
}
@compute @workgroup_size(8,8)
fn color_output(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy>=p.rect.zw) { return; }
    let q=vec2<i32>(id.xy+p.rect.xy); let old=load(q);
    let c=textureLoad(color_source,q,0);
    let a=c.a*textureLoad(coverage,q,0).r*p.strength;
    let alpha=a+old.a*(1.0-a);
    var rgb=old.rgb;
    if alpha>0.000001 { rgb=(c.rgb*a+old.rgb*old.a*(1.0-a))/alpha; }
    textureStore(output,q,vec4(rgb,alpha));
}
