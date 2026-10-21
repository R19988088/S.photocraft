struct View { image:vec4<f32>, sizes:vec4<f32>, origin:vec4<f32>, field_size:vec4<f32>, spare:vec4<f32>, page:vec4<f32>, mask_page:vec4<f32> };
@group(0) @binding(0) var<uniform> u:View;
@group(0) @binding(1) var source:texture_2d_array<f32>;
@group(0) @binding(2) var field:texture_2d_array<f32>;
@group(0) @binding(3) var selection:texture_2d_array<f32>;
@group(0) @binding(4) var mask:texture_2d_array<f32>;
@group(0) @binding(5) var stationary:texture_2d_array<f32>;
@group(0) @binding(6) var resolved_mask:texture_2d<f32>;
@group(0) @binding(9) var<uniform> native_params:vec4<u32>;
@group(0) @binding(11) var<storage,read> moved:array<u32>;
@group(0) @binding(10) var native_alpha:texture_2d_array<f32>;
struct Vertex { @builtin(position) position:vec4<f32>, @location(0) uv:vec2<f32> };
@vertex fn vs(@builtin(vertex_index) i:u32)->Vertex {
    var corners=array<vec2<f32>,6>(vec2(0.,0.),vec2(1.,0.),vec2(0.,1.),vec2(0.,1.),vec2(1.,0.),vec2(1.,1.));
    let uv=corners[i];let p=u.image.xy+uv*u.image.zw;
    var v:Vertex;v.position=vec4(p.x/u.sizes.x*2.-1.,1.-p.y/u.sizes.y*2.,0.,1.);v.uv=(u.page.xy+uv*u.page.zw)/u.sizes.zw;return v;
}
fn node4(p:vec2<i32>)->vec4<f32>{let q=clamp(p,vec2(0),vec2<i32>(u.field_size.xy)-vec2(1));let size=vec2<i32>(textureDimensions(field));let page=q/size;let cols=i32(ceil(u.field_size.x/f32(size.x)));return textureLoad(field,q%size,page.y*cols+page.x,0);}
fn node(p:vec2<i32>)->vec2<f32>{let s=vec2<i32>(u.field_size.xy);let q=clamp(p,vec2(0),s-vec2(1));return node4(q).xy;}
fn weights(t:f32)->vec4<f32>{let t2=t*t;let t3=t2*t;return vec4(-.5*t+t2-.5*t3,1.-2.5*t2+1.5*t3,.5*t+2.*t2-1.5*t3,-.5*t2+.5*t3);}
fn displacement(p:vec2<f32>)->vec2<f32>{
    let q=(p-vec2(.5))/u.origin.z;let base=vec2<i32>(floor(q));let wx=weights(fract(q.x));let wy=weights(fract(q.y));var d=vec2(0.);
    for(var y=0;y<4;y++){for(var x=0;x<4;x++){d+=node(base+vec2(x-1,y-1))*wx[x]*wy[y];}}return d;
}
fn paged(tex:texture_2d_array<f32>,p:vec2<i32>)->vec4<f32>{let size=vec2<i32>(textureDimensions(tex));let q=clamp(p,vec2(0),vec2<i32>(u.sizes.zw)-vec2(1));let page=q/size;let cols=i32(ceil(u.sizes.z/f32(size.x)));return textureLoad(tex,q%size,page.y*cols+page.x,0);}
fn pixel(p:vec2<i32>)->vec4<f32>{return paged(source,p);}
fn sample_source(p:vec2<f32>)->NativePixel {
    let base=vec2<i32>(floor(p));let wx=weights(fract(p.x));let wy=weights(fract(p.y));var c=vec4(0.);var a=0.;
    for(var y=0;y<4;y++){for(var x=0;x<4;x++){let q=base+vec2(x-1,y-1);let w=wx[x]*wy[y];let alpha=paged(native_alpha,q).r;c+=pixel(q)*alpha*w;a+=alpha*w;}}
    a=clamp(a,0.,1.);if a>0. {c=clamp(c/a,vec4(0.),vec4(1.));}else{c=vec4(0.);}
    for(var i=0;i<4;i++){c[i]=native_quantize(c[i],native_params.y);}a=native_quantize(a,native_params.y);return NativePixel(c,a);
}
fn freeze(p:vec2<f32>)->f32 {let q=clamp((p-vec2(.5))/u.origin.z,vec2(0.),u.field_size.xy-vec2(1.));let b=vec2<i32>(floor(q));let size=vec2<i32>(u.field_size.xy);let a=clamp(b,vec2(0),size-vec2(1));let e=min(a+vec2(1),size-vec2(1));let t=fract(q);return mix(mix(node4(vec2(a.x,a.y)).z,node4(vec2(e.x,a.y)).z,t.x),mix(node4(vec2(a.x,e.y)).z,node4(vec2(e.x,e.y)).z,t.x),t.y);}
fn sample_mask(p:vec2<f32>)->f32 {let base=vec2<i32>(floor(p));let wx=weights(fract(p.x));let wy=weights(fract(p.y));let size=vec2<i32>(textureDimensions(mask));var value=0.;for(var y=0;y<4;y++){for(var x=0;x<4;x++){value+=paged(mask,base+vec2(x-1,y-1)).r*wx[x]*wy[y];}}return clamp(value,0.,1.);}
fn layer_native(v:Vertex)->NativePixel{
    let p=v.uv*u.sizes.zw-vec2(.5);let d=displacement(p+vec2(.5));let size=vec2<i32>(textureDimensions(selection));let k=paged(selection,vec2<i32>(floor(p+vec2(.5)))).r;
    let original=NativePixel(paged(source,vec2<i32>(floor(p+vec2(.5)))),paged(native_alpha,vec2<i32>(floor(p+vec2(.5)))).r);
    let tile=vec2<u32>(p+vec2(.5))/256u;let changed=moved[tile.y*u32(ceil(u.sizes.z/256.))+tile.x]!=0u;
    var warped=original;
    if changed {warped=sample_source(p+d);}
    var alpha=warped.alpha;var color=warped.color;
    if (native_params.w&1u)!=0u {
        alpha=mix(original.alpha,warped.alpha,k);color=mix(original.color*original.alpha,warped.color*warped.alpha,k);
        if alpha>0. {color/=alpha;}else{color=vec4(0.);}
        if (native_params.w&2u)!=0u {color=clamp(color,vec4(0.),vec4(1.));}
        for(var i=0;i<4;i++){color[i]=native_quantize(color[i],native_params.y);}alpha=native_quantize(alpha,native_params.y);
    }
    return NativePixel(color,alpha);
}
fn layer_pixel(v:Vertex)->vec4<f32>{
    let p=v.uv*u.sizes.zw-vec2(.5);let native=layer_native(v);
    var c=native_to_rgba(native_params.x,true,native);c=vec4(c.rgb*c.a,c.a);
    c*=textureLoad(resolved_mask,clamp(vec2<i32>(floor(p+vec2(.5)-u.mask_page.xy)),vec2(0),vec2<i32>(u.mask_page.zw)-vec2(1)),0).r*paged(stationary,vec2<i32>(floor(p+vec2(.5)))).r;
    return c;
}
@fragment fn fs_layer(v:Vertex)->@location(0) vec4<f32>{let c=layer_pixel(v);if c.a>0.{return vec4(c.rgb/c.a,c.a);}return vec4(0.);}
@fragment fn fs_alpha(v:Vertex)->@location(0) f32{return layer_pixel(v).a;}
@fragment fn fs_overlay(v:Vertex)->@location(0) vec4<f32>{
    let p=v.uv*u.sizes.zw-vec2(.5);let d=displacement(p+vec2(.5));var c=vec4(0.);
    if u.spare.x>0.5 {let a=freeze(p+vec2(.5))*.55;c=vec4(vec3(.9,.16,.16)*a,a);}
    if u.spare.y>0.5 {let spacing=max(u.sizes.z,u.sizes.w)/u.spare.z;let q=(p+d)/spacing;let edge=abs(fract(q-vec2(.5))-vec2(.5))*spacing;let width=max(abs(dpdx(p.x)),abs(dpdy(p.y)));if min(edge.x,edge.y)<width*.65 {c=vec4(mix(c.rgb,vec3(.35),.8),max(c.a,.8));}}
    if u.origin.w>0.5&&c.a>0.{let s=c.rgb/c.a;let low=s/12.92;let high=pow((s+vec3(.055))/1.055,vec3(2.4));c=vec4(select(high,low,s<=vec3(.04045))*c.a,c.a);}return c;
}

fn raw_mask_value(v:Vertex)->f32 {
    let p=v.uv*u.sizes.zw-vec2(.5);let d=displacement(p+vec2(.5));let size=vec2<i32>(textureDimensions(selection));let k=paged(selection,vec2<i32>(floor(p+vec2(.5)))).r;
    let warped=native_quantize(select(sample_mask(p),sample_mask(p+d),u.field_size.z>.5),native_params.z);
    let value=native_quantize(mix(sample_mask(p),warped,k),native_params.z);
    return value;
}

@fragment fn fs_native_color(v:Vertex)->@location(0) vec4<f32>{return layer_native(v).color;}
@fragment fn fs_native_alpha(v:Vertex)->@location(0) f32{return layer_native(v).alpha;}
@fragment fn fs_native_mask(v:Vertex)->@location(0) f32{return raw_mask_value(v);}
@fragment fn fs_mask(v:Vertex)->@location(0) f32{return 1.-u.spare.x*(1.-raw_mask_value(v));}
