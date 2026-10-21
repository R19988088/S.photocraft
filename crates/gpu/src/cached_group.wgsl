struct V { @builtin(position) pos:vec4<f32>, @location(0) uvq:vec4<f32> };
@group(0) @binding(0) var source:texture_2d<f32>;
@group(0) @binding(1) var stationary:texture_2d<f32>;
@group(0) @binding(2) var content:texture_2d<f32>;
@group(0) @binding(3) var vector_mask:texture_2d<f32>;
struct Settings { densities:vec4<f32>, canvas:vec4<f32>, origin_default:vec4<f32> };
@group(0) @binding(4) var<uniform> settings:Settings;
@vertex fn vertex(@location(0) p:vec2<f32>,@location(1) uvq:vec4<f32>)->V{return V(vec4<f32>(p,0.,1.),uvq);}
fn sample_mask(uv:vec2<f32>,mode:f32)->f32{
 let size=vec2<i32>(textureDimensions(source));
 if mode<0.5{return textureLoad(source,clamp(vec2<i32>(floor(uv*vec2<f32>(size))),vec2<i32>(0),size-vec2<i32>(1)),0).r;}
 let q=uv*vec2<f32>(size)-vec2<f32>(.5);let at=vec2<i32>(floor(q));let f=fract(q);let lo=vec2<i32>(0);let hi=size-vec2<i32>(1);
 if mode>1.5{let wx=cubic(f.x);let wy=cubic(f.y);var total=0.;for(var y=0;y<4;y++){for(var x=0;x<4;x++){total+=textureLoad(source,clamp(at+vec2<i32>(x-1,y-1),lo,hi),0).r*wx[x]*wy[y];}}return clamp(total,0.,1.);}
 return mix(mix(textureLoad(source,clamp(at,lo,hi),0).r,textureLoad(source,clamp(at+vec2<i32>(1,0),lo,hi),0).r,f.x),mix(textureLoad(source,clamp(at+vec2<i32>(0,1),lo,hi),0).r,textureLoad(source,clamp(at+vec2<i32>(1,1),lo,hi),0).r,f.x),f.y);
}
@fragment fn mask(v:V)->@location(0) f32{return sample_mask(v.uvq.xy/v.uvq.z,v.uvq.w);}
@fragment fn shape(v:V)->@location(0) f32{let at=vec2<i32>(v.pos.xy);return textureLoad(source,at,0).r*textureLoad(content,at,0).a;}

@fragment fn multiply(v:V)->@location(0) f32{let at=vec2<i32>(v.pos.xy);let doc=v.pos.xy+settings.origin_default.xy;
 if settings.origin_default.w>0.5 && (any(doc<settings.canvas.xy)||any(doc>=settings.canvas.zw)){return settings.origin_default.z;}
 return (1.0-settings.densities.x*(1.0-textureLoad(source,at,0).r))*textureLoad(stationary,at,0).r*(1.0-settings.densities.y*(1.0-textureLoad(vector_mask,at,0).r));}

fn cubic(t:f32)->vec4<f32>{let a=-0.75;let t0=1.+t;let t1=1.-t;let t2=2.-t;return vec4<f32>(a*(t0*t0*t0-5.*t0*t0+8.*t0-4.),(a+2.)*t*t*t-(a+3.)*t*t+1.,(a+2.)*t1*t1*t1-(a+3.)*t1*t1+1.,a*(t2*t2*t2-5.*t2*t2+8.*t2-4.));}

@fragment fn extract(v:V)->@location(0) f32{let value=textureLoad(source,vec2<i32>(v.pos.xy),0);return mix(settings.densities.z,value.r,value.a);}
