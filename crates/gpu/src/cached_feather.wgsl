@group(0) @binding(0) var source:texture_2d<f32>;
struct Params { kernel:vec4<f32>, clip:vec4<i32> }
@group(0) @binding(1) var<uniform> params:Params;
@group(0) @binding(2) var destination:texture_storage_2d<r32float,write>;
fn at(i:i32,line:i32,vertical:bool)->f32 {let size=vec2<i32>(textureDimensions(source));let p=select(vec2(i,line),vec2(line,i),vertical);return textureLoad(source,clamp(p,max(params.clip.xy,vec2(0)),min(params.clip.zw,size)-vec2(1)),0).r;}
@compute @workgroup_size(64) fn main(@builtin(global_invocation_id) gid:vec3<u32>){
    let size=vec2<i32>(textureDimensions(source));let vertical=params.kernel.z>.5;let count=select(size.y,size.x,vertical);let length=select(size.x,size.y,vertical);let line=i32(gid.x);if line>=count{return;}
    let r=i32(params.kernel.x);var sum=0.;for(var i=-r;i<=r;i++){sum+=at(i,line,vertical);}var below=at(-r-1,line,vertical);let n=f32(2*r+1)+2.*params.kernel.y;
    for(var i=0;i<length;i++){let above=at(i+r+1,line,vertical);let leaving=at(i-r,line,vertical);let value=(sum+params.kernel.y*(below+above))/n;let p=select(vec2(i,line),vec2(line,i),vertical);textureStore(destination,p,vec4(value));sum+=above-leaving;below=leaving;}
}
