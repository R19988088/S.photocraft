struct State { material: vec4<f32>, highlight: vec4<f32>, motion: vec4<f32> }
struct CopyParams { sizes: vec4<u32>, origins: vec4<u32>, extent: vec4<u32> }
@group(0) @binding(0) var<storage,read> source_states: array<State>;
@group(0) @binding(1) var<storage,read_write> target_states: array<State>;
@group(0) @binding(2) var<uniform> params: CopyParams;
@compute @workgroup_size(8,8) fn copy_states(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.extent.xy)) {return;}
    let src=id.xy+params.origins.xy;let dst=id.xy+params.origins.zw;
    if(any(src>=params.sizes.xy) || any(dst>=params.sizes.zw)) {return;}
    target_states[dst.y*params.sizes.z+dst.x]=source_states[src.y*params.sizes.x+src.x];
}
