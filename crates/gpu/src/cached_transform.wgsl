struct VertexOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uvq: vec4<f32>,
};
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var rest: texture_2d<f32>;
@group(0) @binding(2) var stationary_mask: texture_2d<f32>;
@group(0) @binding(3) var linked_mask: texture_2d<f32>;
@group(0) @binding(4) var native_alpha: texture_2d<f32>;
@group(0) @binding(5) var vector_mask: texture_2d<f32>;
struct PageParams { native:vec4<u32>, output:vec4<f32>, atlas:vec4<u32>, coverage:vec4<f32>, map0:vec4<f32>,map1:vec4<f32>,map2:vec4<f32>, canvas:vec4<f32>, mask_outside:vec4<f32> };
@group(0) @binding(8) var<uniform> params:PageParams;
@group(0) @binding(10) var source_pages:texture_2d_array<f32>;
@group(0) @binding(11) var alpha_pages:texture_2d_array<f32>;
@group(0) @binding(9) var rest_alpha:texture_2d<f32>;
struct NativeOutput { @location(0) color:vec4<f32>, @location(1) alpha:f32 }
@vertex fn vertex(@location(0) position: vec2<f32>, @location(1) uvq: vec4<f32>) -> VertexOut {
    var p=position;
    if uvq.w>=0.0 {p=vec2<f32>((position.x-params.output.x)/params.output.z*2.0-1.0,1.0-(position.y-params.output.y)/params.output.w*2.0);}
    return VertexOut(vec4<f32>(p, 0.0, 1.0), uvq);
}
fn premultiply(p: vec4<f32>) -> vec4<f32> { return vec4<f32>(p.rgb * p.a, p.a); }
fn straight(p: vec4<f32>) -> vec4<f32> {
    if p.a <= 0.0000001 { return vec4<f32>(0.0); }
    return vec4<f32>(p.rgb / p.a, p.a);
}
fn cubic(t:f32)->vec4<f32>{
    let a=-0.75;
    let t0=1.0+t;let t1=1.0-t;let t2=2.0-t;
    return vec4<f32>(a*(t0*t0*t0-5.0*t0*t0+8.0*t0-4.0),(a+2.0)*t*t*t-(a+3.0)*t*t+1.0,
        (a+2.0)*t1*t1*t1-(a+3.0)*t1*t1+1.0,a*(t2*t2*t2-5.0*t2*t2+8.0*t2-4.0));
}
fn native_at(p:vec2<i32>,transparent:bool)->NativePixel {
 if transparent && (any(p<vec2<i32>(0))||any(p>=vec2<i32>(params.native.zw))){return NativePixel(vec4(0.0),0.0);}
 let at=clamp(p,vec2<i32>(0),vec2<i32>(params.native.zw)-vec2<i32>(1));
 let side=i32(params.atlas.x);let layer=(at.y/side)*i32(params.atlas.y)+at.x/side;
 let local=at%vec2<i32>(side);
 return NativePixel(textureLoad(source_pages,local,layer,0),textureLoad(alpha_pages,local,layer,0).r);
}
fn sample_native(uv:vec2<f32>,kind:f32)->NativePixel {
 let transparent=kind>=3.0;let mode=kind-select(0.0,3.0,transparent);
 let q=uv*vec2<f32>(params.native.zw)-vec2<f32>(0.5);let p=vec2<i32>(floor(q));let f=fract(q);
 if mode<0.5 {return native_at(vec2<i32>(floor(uv*vec2<f32>(params.native.zw))),transparent);}
 var c=vec4(0.0);var a=0.0;
 if mode>1.5 {let wx=cubic(f.x);let wy=cubic(f.y);for(var y=0;y<4;y++){for(var x=0;x<4;x++){let v=native_at(p+vec2<i32>(x-1,y-1),transparent);let w=wx[x]*wy[y];c+=v.color*v.alpha*w;a+=v.alpha*w;}}}
 else {for(var y=0;y<2;y++){for(var x=0;x<2;x++){let v=native_at(p+vec2<i32>(x,y),transparent);let w=select(1.0-f.x,f.x,x==1)*select(1.0-f.y,f.y,y==1);c+=v.color*v.alpha*w;a+=v.alpha*w;}}}
 a=clamp(a,0.0,1.0);if a>0.0000001 {c/=a;}else{c=vec4(0.0);}
 return NativePixel(c,a);
}
fn analytic_uv(global:vec2<f32>)->vec2<f32>{let p=vec3(global,1.0);return vec2(dot(params.map0.xyz,p),dot(params.map1.xyz,p))/dot(params.map2.xyz,p);}
fn source_uv(v:VertexOut)->vec2<f32>{if params.map0.w>0.5{return analytic_uv(v.pos.xy+params.output.xy);}return v.uvq.xy/v.uvq.z;}
fn footprint(v:VertexOut,size:vec2<f32>)->vec2<f32>{
 let global=v.pos.xy+params.output.xy;let tile=floor(global/256.0)*256.0;
 let center=(max(tile,params.coverage.xy)+min(tile+vec2(256.0),params.coverage.zw))*0.5;
 if params.map0.w>0.5 {let o=analytic_uv(center);return max((abs(analytic_uv(center+vec2(1.0,0.0))-o)+abs(analytic_uv(center+vec2(0.0,1.0))-o))*size,vec2(0.000001));}
 let uv=v.uvq.xy/v.uvq.z;return max(abs(dpdx(uv*size))+abs(dpdy(uv*size)),vec2(0.000001));
}
fn sampled_content(v: VertexOut) -> NativePixel {
    let uv=source_uv(v);
    let size=vec2<f32>(params.native.zw);
    let foot=footprint(v,size);
    if params.map0.w>0.5 && abs(dot(params.map2.xyz,vec3(v.pos.xy+params.output.xy,1.0)))<0.000000001 {
        return NativePixel(vec4(0.0),0.0);
    }

    let edge=min(uv*size,(vec2<f32>(1.0)-uv)*size);
    let coverage=clamp(edge/foot+vec2<f32>(0.5),vec2<f32>(0.0),vec2<f32>(1.0));
    var native=sample_native(uv,v.uvq.w);
    if v.uvq.w>=3.0 || v.uvq.w<0.5 {if any(uv<vec2<f32>(0.0))||any(uv>=vec2<f32>(1.0)){native.alpha=0.0;}}
    else {native.alpha*=coverage.x*coverage.y;if any(uv*size<vec2<f32>(-0.5))||any(uv*size>size+vec2<f32>(0.5)){native.alpha=0.0;}}
    for(var k=0;k<4;k++){native.color[k]=native_quantize(native.color[k],params.native.y);}
    native.alpha=native_quantize(native.alpha,params.native.y);
    return native;
}
@fragment fn content(v:VertexOut)->NativeOutput {
 let value=sampled_content(v);return NativeOutput(value.color,value.alpha);
}
struct PreviewOutput { @location(0) color:vec4<f32>, @location(1) shape:f32 }
fn preview_finish(v:VertexOut,moved:NativePixel)->PreviewOutput {
 let at=vec2<i32>(v.pos.xy);
 let under=NativePixel(textureLoad(rest,at,0),textureLoad(rest_alpha,at,0).r);
 var merged=native_over(under,moved,1.0,true,false);
 for(var k=0;k<4;k++){merged.color[k]=native_quantize(merged.color[k],params.native.y);}
 merged.alpha=native_quantize(merged.alpha,params.native.y);
 var rgba=native_to_rgba(params.native.x,true,merged);
 let global=v.pos.xy+params.output.xy;
 if params.mask_outside.y>0.5 && (any(global<params.canvas.xy)||any(global>=params.canvas.zw)){
     rgba.a*=params.mask_outside.x;
 }else{
     rgba.a*=textureLoad(stationary_mask,at,0).r*(1.0-bitcast<f32>(params.atlas.z)*(1.0-textureLoad(linked_mask,at,0).r))*(1.0-bitcast<f32>(params.atlas.w)*(1.0-textureLoad(vector_mask,at,0).r));
 }
 return PreviewOutput(rgba,rgba.a);
}
@fragment fn preview(v:VertexOut)->PreviewOutput {
 var source_vertex=v;source_vertex.uvq.w=params.map1.w;
 return preview_finish(v,sampled_content(source_vertex));
}
@fragment fn preview_background(v:VertexOut)->PreviewOutput {
 return preview_finish(v,NativePixel(vec4(0.0),0.0));
}
@fragment fn background(v: VertexOut) -> @location(0) vec4<f32> {
    return textureLoad(rest, vec2<i32>(v.pos.xy), 0);
}
@fragment fn alpha(v: VertexOut) -> @location(0) f32 {
    return textureLoad(source, vec2<i32>(v.pos.xy), 0).a;
}

@fragment fn raw_mask(v:VertexOut)->@location(0) f32 {
 let uv=source_uv(v);let size=vec2<f32>(params.native.zw);
 var value=sample_native(uv,v.uvq.w);
 if v.uvq.w>=3.0 || v.uvq.w<0.5 {if any(uv<vec2<f32>(0.0))||any(uv>=vec2<f32>(1.0)){value.alpha=0.0;}}
 else {let foot=footprint(v,size);let edge=min(uv*size,(vec2<f32>(1.0)-uv)*size);let coverage=clamp(edge/foot+vec2<f32>(.5),vec2<f32>(0.0),vec2<f32>(1.0));value.alpha*=coverage.x*coverage.y;if any(uv*size<vec2<f32>(-0.5))||any(uv*size>size+vec2<f32>(0.5)){value.alpha=0.0;}}
 let under=textureLoad(rest,vec2<i32>(v.pos.xy),0).r;
 return clamp(value.color.r*value.alpha+under*(1.0-value.alpha),0.0,1.0);
}
@fragment fn merge(v:VertexOut)->NativeOutput {
 let at=vec2<i32>(v.pos.xy);
 let moved=NativePixel(textureLoad(source,at,0),textureLoad(native_alpha,at,0).r);
 let under=NativePixel(textureLoad(rest,at,0),textureLoad(rest_alpha,at,0).r);
 var result=native_over(under,moved,1.0,true,false);
 for(var k=0;k<4;k++){result.color[k]=native_quantize(result.color[k],params.native.y);}result.alpha=native_quantize(result.alpha,params.native.y);
 return NativeOutput(result.color,result.alpha);
}
@fragment fn composite(v:VertexOut)->@location(0) vec4<f32>{
 let at=vec2<i32>(v.pos.xy);
 let value=NativePixel(textureLoad(source,at,0),textureLoad(native_alpha,at,0).r);
 var rgba=native_to_rgba(params.native.x,true,value);
 let global=v.pos.xy+params.output.xy;
 if params.mask_outside.y>0.5 && (any(global<params.canvas.xy)||any(global>=params.canvas.zw)){rgba.a*=params.mask_outside.x;return rgba;}
 rgba.a*=textureLoad(stationary_mask,at,0).r*(1.0-bitcast<f32>(params.atlas.z)*(1.0-textureLoad(linked_mask,at,0).r))*(1.0-bitcast<f32>(params.atlas.w)*(1.0-textureLoad(vector_mask,at,0).r));
 return rgba;
}
