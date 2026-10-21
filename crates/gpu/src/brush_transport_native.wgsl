struct Params {
 rect:vec4<u32>,offset:vec2<f32>,strength:f32,blur_radius:f32,target_color:vec4<f32>,
 center:vec2<f32>,radius:f32,previous_radius:f32,reservoir:vec4<f32>,sample_rect:vec4<i32>,paint:vec4<f32>,kind:vec4<u32>
}
@group(0) @binding(0) var source:texture_2d<f32>;
@group(0) @binding(1) var coverage:texture_2d<f32>;
@group(0) @binding(2) var output:texture_storage_2d<rgba32float,write>;
@group(0) @binding(3) var<uniform> p:Params;
@group(0) @binding(4) var carry:texture_2d<f32>;
@group(0) @binding(5) var next_carry:texture_storage_2d<rgba32float,write>;
@group(0) @binding(6) var color_source:texture_2d<f32>;
@group(0) @binding(7) var<storage,read> lut:array<f32>;
@group(0) @binding(8) var source_alpha:texture_2d<f32>;
@group(0) @binding(9) var carry_alpha:texture_2d<f32>;
@group(0) @binding(10) var next_alpha:texture_storage_2d<rgba32float,write>;
@group(0) @binding(11) var output_alpha:texture_storage_2d<rgba32float,write>;
@group(0) @binding(12) var color_alpha:texture_2d<f32>;
@group(0) @binding(13) var remote_alpha:texture_2d<f32>;
struct Pixel {c:vec4<f32>,a:f32}
fn alpha(v:f32)->f32{return select(1.0,v,p.kind.y!=0u);}
fn load(q:vec2<i32>)->Pixel{
 let at=clamp(q,max(p.sample_rect.xy,vec2(0)),min(p.sample_rect.zw,vec2<i32>(textureDimensions(source)))-vec2(1));
 return Pixel(textureLoad(source,at,0),alpha(textureLoad(source_alpha,at,0).r));
}
fn lerp_pixel(a:Pixel,b:Pixel,t:f32)->Pixel{return Pixel(a.c+(b.c-a.c)*t,a.a+(b.a-a.a)*t);}
fn sampled(q:vec2<f32>)->Pixel{
 let a=vec2<i32>(floor(q));let f=fract(q);
 return lerp_pixel(lerp_pixel(load(a),load(a+vec2(1,0)),f.x),lerp_pixel(load(a+vec2(0,1)),load(a+vec2(1,1)),f.x),f.y);
}
fn remote_load(q:vec2<i32>)->Pixel{
 let at=clamp(q,p.sample_rect.xy,p.sample_rect.zw-vec2(1))+vec2<i32>(p.paint.xy);
 return Pixel(textureLoad(color_source,at,0),alpha(textureLoad(remote_alpha,at,0).r));
}
fn remote_sample(q:vec2<f32>)->Pixel{
 let a=vec2<i32>(floor(q));let f=fract(q);
 return lerp_pixel(lerp_pixel(remote_load(a),remote_load(a+vec2(1,0)),f.x),lerp_pixel(remote_load(a+vec2(0,1)),remote_load(a+vec2(1,1)),f.x),f.y);
}
fn carried(q:vec2<i32>)->Pixel{
 let at=clamp(q,vec2(0),vec2<i32>(textureDimensions(carry))-vec2(1));
 return Pixel(textureLoad(carry,at,0),alpha(textureLoad(carry_alpha,at,0).r));
}
fn carry_sample(q:vec2<f32>)->Pixel{
 let a=vec2<i32>(floor(q));let f=fract(q);
 return lerp_pixel(lerp_pixel(carried(a),carried(a+vec2(1,0)),f.x),lerp_pixel(carried(a+vec2(0,1)),carried(a+vec2(1,1)),f.x),f.y);
}
fn store(q:vec2<i32>,value:Pixel){textureStore(output,q,value.c);textureStore(output_alpha,q,vec4(value.a,0.0,0.0,0.0));}
fn store_carrier(q:vec2<i32>,value:Pixel){textureStore(next_carry,q,value.c);textureStore(next_alpha,q,vec4(value.a,0.0,0.0,0.0));}
fn pigment(normal:Pixel,old:Pixel,src:Pixel,weight:f32)->Pixel{
 if(p.kind.x==3u && p.kind.y!=0u && p.paint.a>0.0){
  let value=pigment_paint(vec4(normal.c.rgb,normal.a),vec4(old.c.rgb,old.a),vec4(src.c.rgb,src.a),weight,p.paint.a);
  return Pixel(vec4(value.rgb,normal.c.a),value.a);
 }
 return normal;
}
@compute @workgroup_size(8,8) fn pickup(@builtin(global_invocation_id) id:vec3<u32>){
 if(any(id.xy>=p.rect.zw)){return;}let q=vec2<i32>(id.xy+p.rect.xy);let old=load(q);var picked=sampled(vec2<f32>(q)+p.offset);
 if(p.paint.z>0.0){picked=remote_sample(vec2<f32>(q)+p.offset);}
 let amount=clamp(textureLoad(coverage,q,0).r*p.strength,0.0,1.0);let a=min(amount,picked.a);let weight=clamp(amount*picked.a,0.0,1.0);
 let normal=Pixel(mix(old.c,picked.c,weight),max(old.a,a+old.a*(1.0-a)));store(q,pigment(normal,old,picked,weight));
}
@compute @workgroup_size(8,8) fn blur(@builtin(global_invocation_id) id:vec3<u32>){
 if(any(id.xy>=p.rect.zw)){return;}let q=vec2<i32>(id.xy+p.rect.xy);let old=load(q);
 if(p.kind.x>=3u && p.target_color.a>=0.0 && ((p.kind.y!=0u && old.a<=0.0001)||distance(old.c.rgb,p.target_color.rgb)>p.target_color.a)){store(q,old);return;}
 let offsets=array<vec2<f32>,9>(vec2(0.0),vec2(0.0,-1.0),vec2(0.0,1.0),vec2(-1.0,0.0),vec2(1.0,0.0),vec2(-0.7,-0.7),vec2(0.7,-0.7),vec2(-0.7,0.7),vec2(0.7,0.7));
 let weights=array<f32,9>(0.18,0.14,0.14,0.14,0.14,0.10,0.10,0.10,0.10);var sum=vec4(0.0);var sa=0.0;var total=0.0;
 for(var i=0u;i<9u;i++){let value=sampled(vec2<f32>(q)+offsets[i]*p.blur_radius);sum+=value.c*value.a*weights[i];sa+=value.a*weights[i];total+=weights[i];}sum/=total;sa/=total;
 if(sa<=0.0){store(q,old);return;}let weight=clamp(textureLoad(coverage,q,0).r*p.strength,0.0,1.0);let a=mix(old.a,max(old.a,sa),weight);
 store(q,Pixel((old.c*old.a*(1.0-weight)+sum/sa*max(old.a,sa)*weight)/max(a,0.000001),a));
}
@compute @workgroup_size(8,8) fn carrier_v1(@builtin(global_invocation_id) id:vec3<u32>){
 let dims=textureDimensions(next_carry);let pos=id.xy+p.rect.xy;if(any(id.xy>=p.rect.zw)||any(pos>=dims)){return;}
 let at=vec2<i32>(floor(p.center))+vec2<i32>(pos)-vec2<i32>(dims/2u);let value=load(at);let sample=Pixel(value.c*value.a,value.a);let retained=carried(vec2<i32>(pos));var result=sample;
 if(p.previous_radius>0.0){result=lerp_pixel(sample,retained,p.strength);if(any(at<p.sample_rect.xy)||any(at>=p.sample_rect.zw)){result=retained;}}
 store_carrier(vec2<i32>(pos),result);
}
@compute @workgroup_size(8,8) fn carrier_v2(@builtin(global_invocation_id) id:vec3<u32>){
 let dims=textureDimensions(next_carry);let pos=id.xy+p.rect.xy;if(any(id.xy>=p.rect.zw)||any(pos>=dims)){return;}
 let local=vec2<f32>(pos)+vec2(0.5)-vec2<f32>(dims)*0.5;let value=sampled(p.center+local-vec2(0.5));var sample=Pixel(value.c*value.a,value.a);var retained=sample;
 if(p.previous_radius>0.0){retained=carry_sample(local*p.previous_radius/max(p.radius,0.5)+vec2<f32>(textureDimensions(carry))*0.5-vec2(0.5));}
 let a16=floor(clamp(sample.a,0.0,1.0)*65535.0);if(a16==0.0){sample.c=vec4(0.0);if(retained.a>0.0){sample.c=retained.c/retained.a*sample.a;}}
 let base=select(pow(1.0-p.reservoir.x,2.0),pow(1.0-p.reservoir.y,2.0),a16==0.0);let stretch=clamp(p.reservoir.z,0.0,1.0);
 let retention=select(1.0,pow(1.0-pow(1.0-stretch,2.0),32.0*p.reservoir.w/max(p.blur_radius,1.0)),p.reservoir.w>0.0);let dynamic=select(0.0,p.strength/max(p.reservoir.x,0.0001),p.reservoir.x>0.0001);
 let value_out=lerp_pixel(retained,sample,clamp(base*dynamic,0.0,1.0)*(1.0-retention));store_carrier(vec2<i32>(pos),Pixel(clamp(value_out.c,vec4(0.0),vec4(1.0)),clamp(value_out.a,0.0,1.0)));
}
@compute @workgroup_size(8,8) fn deposit_v1(@builtin(global_invocation_id) id:vec3<u32>){
 if(any(id.xy>=p.rect.zw)){return;}let q=vec2<i32>(id.xy+p.rect.xy);let old=load(q);let at=q-vec2<i32>(floor(p.center))+vec2<i32>(textureDimensions(carry)/2u);
 if(any(at<vec2(0))||any(at>=vec2<i32>(textureDimensions(carry)))){store(q,old);return;}let c=carried(at);let k=textureLoad(coverage,q,0).r*p.strength;let a=mix(old.a,c.a,k);var color=mix(old.c,c.c,k);
 if(a>0.000001){color=(old.c*old.a*(1.0-k)+c.c*k)/a;}store(q,Pixel(color,a));
}
@compute @workgroup_size(8,8) fn deposit_v2(@builtin(global_invocation_id) id:vec3<u32>){
 if(any(id.xy>=p.rect.zw)){return;}let q=vec2<i32>(id.xy+p.rect.xy);let old=load(q);let c=carry_sample(vec2<f32>(q)-p.center+vec2<f32>(textureDimensions(carry))*0.5);
 let k=clamp(textureLoad(coverage,q,0).r*p.strength,0.0,1.0);let sa=c.a*k;let a=sa+old.a*(1.0-sa);if(sa<=0.0){store(q,old);return;}
 let normal=Pixel((c.c*k+old.c*old.a*(1.0-sa))/max(a,0.000001),a);store(q,pigment(normal,old,Pixel(c.c/max(c.a,0.000001),c.a),sa/max(a,0.000001)));
}
@compute @workgroup_size(8,8) fn color_output(@builtin(global_invocation_id) id:vec3<u32>){
 if(any(id.xy>=p.rect.zw)){return;}let q=vec2<i32>(id.xy+p.rect.xy);let old=load(q);let c=textureLoad(color_source,q,0);let a=alpha(textureLoad(color_alpha,q,0).r)*textureLoad(coverage,q,0).r*p.strength;
 let result=a+old.a*(1.0-a);var color=old.c;if(result>0.000001){color=(c*a+old.c*old.a*(1.0-a))/result;}store(q,Pixel(color,result));
}
