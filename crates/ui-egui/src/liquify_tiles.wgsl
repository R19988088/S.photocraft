struct Params { grid:vec4<f32>, canvas:vec4<u32> };
@group(0) @binding(0) var<uniform> u:Params;
@group(0) @binding(1) var field:texture_2d_array<f32>;
@group(0) @binding(2) var<storage,read_write> moved:array<u32>;
fn node(p:vec2<i32>)->vec2<f32>{let q=clamp(p,vec2(0),vec2<i32>(u.grid.xy)-vec2(1));let size=vec2<i32>(textureDimensions(field));let page=q/size;let cols=i32(ceil(u.grid.x/f32(size.x)));return textureLoad(field,q%size,page.y*cols+page.x,0).xy;}
fn weights(t:f32)->vec4<f32>{let t2=t*t;let t3=t2*t;return vec4(-.5*t+t2-.5*t3,1.-2.5*t2+1.5*t3,.5*t+2.*t2-1.5*t3,-.5*t2+.5*t3);}
fn displacement(p:vec2<u32>)->vec2<f32>{let q=vec2<f32>(p)/u.grid.z;let b=vec2<i32>(floor(q));let wx=weights(fract(q.x));let wy=weights(fract(q.y));var d=vec2(0.);for(var y=0;y<4;y++){for(var x=0;x<4;x++){d+=node(b+vec2(x-1,y-1))*wx[x]*wy[y];}}return d;}
var<workgroup> changed:atomic<u32>;
@compute @workgroup_size(64) fn main(@builtin(workgroup_id) group:vec3<u32>,@builtin(local_invocation_index) thread:u32){
    let tile=group.xy+vec2(u.canvas.w,u32(u.grid.w));
    if thread==0u {atomicStore(&changed,0u);}workgroupBarrier();
    for(var i=thread;i<65536u;i+=64u){if atomicLoad(&changed)!=0u {break;}let p=tile*256u+vec2(i%256u,i/256u);if any(p>=u.canvas.xy){continue;}if any(displacement(p)!=vec2(0.)){atomicStore(&changed,1u);}}
    workgroupBarrier();if thread==0u {moved[tile.y*u.canvas.z+tile.x]=atomicLoad(&changed);}
}
