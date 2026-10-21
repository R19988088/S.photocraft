@group(0) @binding(0) var color:texture_2d_array<f32>;
@group(0) @binding(1) var alpha:texture_2d_array<f32>;
@group(0) @binding(2) var<storage,read> taps:array<vec4<i32>>;
@group(0) @binding(3) var<storage,read> weights:array<f32>;
struct Params { src:vec4<i32>,dst:vec4<i32>,format:vec4<u32>,default_color:vec4<f32>,default_alpha:vec4<f32>,root:vec4<i32> }
@group(0) @binding(4) var<uniform> p:Params;
struct V { @builtin(position) pos:vec4<f32> }
@vertex fn vertex(@builtin(vertex_index) i:u32)->V {let x=f32((i<<1u)&2u);let y=f32(i&2u);return V(vec4(x*2.0-1.0,1.0-y*2.0,0.0,1.0));}
fn quantize(v:f32)->f32{if(p.format.y==0u){return floor(clamp(v,0.0,1.0)*255.0+0.5)/255.0;}if(p.format.y==1u){return floor(clamp(v,0.0,1.0)*65535.0+0.5)/65535.0;}return v;}
struct Out { @location(0) color:vec4<f32>, @location(1) alpha:f32 }
@fragment fn main(v:V)->Out {
 let at=vec2<i32>(v.pos.xy);let axis=p.format.x;let index=select(at.x+p.dst.x-p.root.x,at.y+p.dst.y-p.root.y,axis==1u);let t=taps[index];
 let outside=bitcast<f32>(t.w);var a=p.default_alpha.x*outside;var c=p.default_color*a;
 for(var k=0;k<t.y;k++){
  var q=at+p.dst.xy-p.src.xy;if(axis==0u){q.x=t.x+k-p.src.x;}else{q.y=t.x+k-p.src.y;}
  let side=i32(p.format.z);let columns=i32(p.format.w);let page=(q.y/side)*columns+q.x/side;let pos=q%vec2<i32>(side);
  let al=textureLoad(alpha,pos,page,0).r;let w=weights[t.z+k];c+=textureLoad(color,pos,page,0)*al*w;a+=al*w;
 }
 a=clamp(a,0.0,1.0);if(a>0.0){c/=a;}else{c=vec4(0.0);}
 for(var k=0;k<4;k++){c[k]=quantize(c[k]);}return Out(c,quantize(a));
}
