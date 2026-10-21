@group(0) @binding(0) var<uniform> origin: vec4<i32>;
@group(0) @binding(1) var source: texture_2d<f32>;
@group(0) @binding(2) var output_image: texture_storage_2d<r32float, write>;
@compute @workgroup_size(8,8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= textureDimensions(output_image)) { return; }
    let q = vec2<i32>(id.xy) + origin.xy;
    var value = 0.0;
    if all(q >= vec2(0)) && all(q < vec2<i32>(textureDimensions(source))) {
        value = textureLoad(source, q, 0).r;
    }
    textureStore(output_image, vec2<i32>(id.xy), vec4(value));
}
