@group(0) @binding(0) var before_color: texture_2d<f32>;
@group(0) @binding(1) var before_alpha: texture_2d<f32>;
@group(0) @binding(2) var after_color: texture_2d<f32>;
@group(0) @binding(3) var result_color: texture_storage_2d<rgba32float, write>;
@group(0) @binding(4) var result_alpha: texture_storage_2d<rgba32float, write>;
@group(0) @binding(5) var<uniform> params: vec4<u32>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let p = vec2<i32>(id.xy);
    var color = textureLoad(before_color, p, 0);
    color[params.x] = textureLoad(after_color, p, 0)[params.x];
    let alpha_texel = textureLoad(before_alpha, p, 0);
    let alpha = select(alpha_texel.r, alpha_texel.a, params.y != 0u);
    if params.z != 0u { color.a = alpha; }
    textureStore(result_color, p, color);
    textureStore(result_alpha, p, vec4<f32>(alpha));
}
