struct State { material: vec4<f32>, highlight: vec4<f32>, motion: vec4<f32> }
struct Params { rect: vec4<u32>, fields: vec4<u32> }
@group(0) @binding(0) var old_cov: texture_2d<f32>;
@group(0) @binding(1) var old_color: texture_2d<f32>;
@group(0) @binding(2) var old_wet: texture_2d<f32>;
@group(0) @binding(3) var old_pigment: texture_2d<f32>;
@group(0) @binding(4) var new_cov: texture_2d<f32>;
@group(0) @binding(5) var new_color: texture_2d<f32>;
@group(0) @binding(6) var new_wet: texture_2d<f32>;
@group(0) @binding(7) var new_pigment: texture_2d<f32>;
@group(0) @binding(8) var output_cov: texture_storage_2d<rgba32float,write>;
@group(0) @binding(9) var output_color: texture_storage_2d<rgba32float,write>;
@group(0) @binding(10) var output_wet: texture_storage_2d<rgba32float,write>;
@group(0) @binding(11) var output_pigment: texture_storage_2d<rgba32float,write>;
@group(0) @binding(12) var<storage,read> old_states: array<State>;
@group(0) @binding(13) var<storage,read> new_states: array<State>;
@group(0) @binding(14) var<storage,read_write> output_states: array<State>;
@group(0) @binding(15) var<uniform> params: Params;
@compute @workgroup_size(8,8) fn union_max(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) {return;}let pos=id.xy+params.rect.xy;let at=vec2<i32>(pos);
    let a=textureLoad(old_cov,at,0);let b=textureLoad(new_cov,at,0);let incoming=b.r>a.r;
    textureStore(output_cov,at,vec4(max(a.r,b.r),max(a.g,b.g),a.b,a.a));
    var color=textureLoad(old_color,at,0);var wet=textureLoad(old_wet,at,0);var pigment=textureLoad(old_pigment,at,0);
    let i=pos.y*textureDimensions(old_cov).x+pos.x;
    if(incoming) {
        if((params.fields.x&1u)!=0u) {color=textureLoad(new_color,at,0);}
        if((params.fields.x&2u)!=0u) {wet=textureLoad(new_wet,at,0);}
        if((params.fields.x&4u)!=0u) {pigment=textureLoad(new_pigment,at,0);}
    }
    textureStore(output_color,at,color);textureStore(output_wet,at,wet);textureStore(output_pigment,at,pigment);
    if((params.fields.x&4u)!=0u) {
        var state=old_states[i];if(incoming) {state=new_states[i];}output_states[i]=state;
    }
}
