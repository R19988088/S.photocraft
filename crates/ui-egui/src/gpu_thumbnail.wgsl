struct Params { canvas: vec4<i32>, tile: vec4<i32>, window: vec4<u32>, default_pixel: vec4<f32>, native_default: vec4<u32>, kind: vec4<u32> }
struct Frame { bounds: array<atomic<i32>,4>, value: vec4<i32> }
@group(0) @binding(0) var source_map: texture_2d<f32>;
@group(0) @binding(1) var destination: texture_storage_2d<rgba8unorm,write>;
@group(0) @binding(2) var<uniform> params: Params;
@group(0) @binding(3) var<storage,read_write> frame: Frame;
@group(0) @binding(4) var alpha_map: texture_2d<f32>;
var<workgroup> group_bounds: array<atomic<i32>,4>;
fn encoded(value:vec4<f32>)->vec4<u32>{
    if(params.kind.x==0u){return vec4<u32>(clamp(value,vec4(0.0),vec4(1.0))*255.0+0.5);}
    if(params.kind.x==1u){return vec4<u32>(clamp(value,vec4(0.0),vec4(1.0))*65535.0+0.5);}
    return bitcast<vec4<u32>>(value);
}
@compute @workgroup_size(8,8) fn reduce_bounds(@builtin(global_invocation_id) id:vec3<u32>,@builtin(local_invocation_index) index:u32){
    if(index<4u){atomicStore(&group_bounds[index],select(-2147483647-1,2147483647,index<2u));}workgroupBarrier();
    if(all(id.xy<textureDimensions(source_map))){
        let at=vec2<i32>(id.xy);let pixel=encoded(textureLoad(source_map,at,0));var differs=false;
        for(var ch=0u;ch<params.kind.y;ch++){differs=differs||pixel[ch]!=params.native_default[ch];}
        if(params.kind.z!=0u){let alpha=textureLoad(alpha_map,at,0);let a=select(alpha.r,alpha.a,params.kind.z==2u);differs=differs||encoded(vec4(a)).r!=params.kind.w;}
        if(differs){let point=at+params.tile.xy;atomicMin(&group_bounds[0],point.x);atomicMin(&group_bounds[1],point.y);atomicMax(&group_bounds[2],point.x+1);atomicMax(&group_bounds[3],point.y+1);}
    }
    workgroupBarrier();
    if(index==0u&&atomicLoad(&group_bounds[0])!=2147483647){atomicMin(&frame.bounds[0],atomicLoad(&group_bounds[0]));atomicMin(&frame.bounds[1],atomicLoad(&group_bounds[1]));atomicMax(&frame.bounds[2],atomicLoad(&group_bounds[2]));atomicMax(&frame.bounds[3],atomicLoad(&group_bounds[3]));}
}
@compute @workgroup_size(8,8) fn resolve_frame(@builtin(global_invocation_id) id:vec3<u32>){
    if(any(id!=vec3(0u))){return;}
    let lo=max(vec2(atomicLoad(&frame.bounds[0]),atomicLoad(&frame.bounds[1])),params.canvas.xy);
    let hi=min(vec2(atomicLoad(&frame.bounds[2]),atomicLoad(&frame.bounds[3])),params.canvas.xy+params.canvas.zw);
    frame.value=params.canvas;if(all(hi>lo)){frame.value=vec4(lo,hi-lo);}
}
fn thumbnail_color(color:vec4<f32>)->vec4<f32>{
    let q=vec4<u32>(floor(clamp(color,vec4(0.0),vec4(1.0))*255.0+0.5));let rgb=(q.rgb*q.a+vec3(127u))/255u;return vec4<f32>(vec4(rgb,q.a))/255.0;
}
fn sample_point(pos:vec2<u32>)->vec2<i32>{
    let size=vec2<f32>(max(frame.value.zw,vec2(1)));let scale=max(size.x,size.y)/64.0;let offset=(vec2(64.0)-size/scale)*0.5;let point=(vec2<f32>(pos)-offset+0.5)*scale;return frame.value.xy+vec2<i32>(floor(point));
}
fn in_frame(pos:vec2<u32>)->bool{
    let size=vec2<f32>(max(frame.value.zw,vec2(1)));let scale=max(size.x,size.y)/64.0;let offset=(vec2(64.0)-size/scale)*0.5;let point=(vec2<f32>(pos)-offset+0.5)*scale;return all(point>=vec2(0.0))&&all(point<size);
}
@compute @workgroup_size(8,8) fn clear(@builtin(global_invocation_id) id:vec3<u32>){
    if(any(id.xy>=vec2(64u))){return;}var color=vec4(0.0);if(in_frame(id.xy)){color=thumbnail_color(params.default_pixel);}textureStore(destination,vec2<i32>(id.xy),color);
}
@compute @workgroup_size(8,8) fn paint(@builtin(global_invocation_id) id:vec3<u32>){
    if(any(id.xy>=params.window.zw)){return;}let pos=id.xy+params.window.xy;if(any(pos>=vec2(64u))||!in_frame(pos)){return;}
    let local=sample_point(pos)-params.tile.xy;if(any(local<vec2(0))||any(local>=vec2<i32>(textureDimensions(source_map)))){return;}
    textureStore(destination,vec2<i32>(pos),thumbnail_color(textureLoad(source_map,local,0)));
}
