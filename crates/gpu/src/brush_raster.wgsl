struct Params {
    rect: vec4<u32>,
    style: u32, width_mode: u32, color_mode: u32, edge_control: u32,
    softness: f32, brightness: f32, saturation: f32, padding: f32,
    resolved: vec4<f32>,
    origin: vec4<i32>, center: vec4<f32>, shape: vec4<f32>, rotation: vec4<f32>,
    projection: vec4<f32>, tip_shape: vec4<f32>, flags: vec4<u32>, flags2: vec4<u32>,
    tip: vec4<u32>, tip_base: vec4<u32>, texture: vec4<u32>, texture_params: vec4<f32>,
    texture_origin: vec4<f32>, grain: vec4<f32>, grain2: vec4<f32>, paint: vec4<f32>,
    pigment: vec4<u32>, canvas: vec4<u32>, canvas_params: vec4<f32>,native:vec4<u32>,
}
@group(0) @binding(0) var<storage, read> data: array<f32>;
@group(0) @binding(1) var base: texture_2d<f32>;
@group(0) @binding(2) var coverage_out: texture_storage_2d<rgba32float, write>;
@group(0) @binding(3) var depth_out: texture_storage_2d<rgba32float, write>;
@group(0) @binding(4) var source_out: texture_storage_2d<rgba32float, write>;
@group(0) @binding(5) var<uniform> p: Params;
@group(0) @binding(6) var stroke_coverage: texture_2d<f32>;

fn at_gray(t: vec4<u32>, q: vec2<i32>, wrap: bool) -> f32 {
    var pos = q;
    let size = vec2<i32>(t.yz);
    if wrap { pos = ((q % size) + size) % size; }
    else if any(q < vec2<i32>(0)) || any(q >= size) { return 0.0; }
    return data[t.x + u32(pos.y) * t.y + u32(pos.x)];
}
fn bilinear_gray(t: vec4<u32>, pos: vec2<f32>, wrap: bool) -> f32 {
    let lo = vec2<i32>(floor(pos));
    let f = pos - floor(pos);
    return lerp(lerp(at_gray(t,lo,wrap), at_gray(t,lo+vec2<i32>(1,0),wrap),f.x),
        lerp(at_gray(t,lo+vec2<i32>(0,1),wrap),at_gray(t,lo+vec2<i32>(1,1),wrap),f.x),f.y);
}
fn sample_tip(t: vec4<u32>, uv: vec2<f32>) -> f32 {
    return bilinear_gray(t,uv*vec2<f32>(t.yz)-0.5,false);
}
fn c3_tip(distance: f32, radius: f32, hardness: f32, subpixel: bool) -> f32 {
    if hardness >= 0.999 {
        if subpixel { return select(0.0,1.0,distance<=radius); }
        return unit(radius-distance+0.5);
    }
    let h = clamp(hardness,0.0,0.999);
    let t = unit((distance/max(radius,0.0005)-h)/(1.0-h));
    let tail = exp(-2.5);
    return (exp(-2.5*t*t)-tail)/(1.0-tail);
}
fn regular_tip(distance: f32) -> f32 {
    if distance <= p.tip_shape.x { return 1.0; }
    let x = (distance-p.tip_shape.x)/p.tip_shape.y;
    var g = unit(1.0-x*x/64.0);
    for(var i=0;i<6;i++){g*=g;}
    return g;
}
fn c3_sample(uv: vec2<f32>) -> f32 {
    if any(uv<vec2<f32>(0.0)) || any(uv>vec2<f32>(1.0)){return 0.0;}
    let size=vec2<f32>(p.tip_base.yz);
    let low=0.5/size;
    let high=vec2<f32>(1.0)-low;
    let original=sample_tip(p.tip_base,clamp(uv,low,high));
    let h=unit(p.shape.w);
    if h>=0.999{return original;}
    let boost=unit((1.0-h)*5.0);
    let r=min(vec2<f32>((0.6+boost*6.4))/size,vec2<f32>(0.125));
    var blur=original*0.28;
    blur+=sample_tip(p.tip_base,clamp(uv+vec2<f32>(-r.x,0.0),low,high))*0.11;
    blur+=sample_tip(p.tip_base,clamp(uv+vec2<f32>(r.x,0.0),low,high))*0.11;
    blur+=sample_tip(p.tip_base,clamp(uv+vec2<f32>(0.0,-r.y),low,high))*0.11;
    blur+=sample_tip(p.tip_base,clamp(uv+vec2<f32>(0.0,r.y),low,high))*0.11;
    for(var y=-1;y<=1;y+=2){for(var x=-1;x<=1;x+=2){blur+=sample_tip(p.tip_base,clamp(uv+vec2<f32>(f32(x),f32(y))*r,low,high))*0.07;}}
    return pow(unit(min(original,lerp(original,blur,boost))),2.2-1.2*h)
        *c3_tip(max(abs(uv.x*2.0-1.0),abs(uv.y*2.0-1.0)),1.0,h,false);
}
fn to_tip(delta: vec2<f32>) -> vec2<f32> {
    var v=vec2<f32>(delta.x,-delta.y);
    v+=dot(v,p.projection.xy)*p.projection.z*p.projection.xy;
    return vec2<f32>(v.x*p.rotation.y+v.y*p.rotation.x,-v.x*p.rotation.x+v.y*p.rotation.y)*p.rotation.zw;
}
fn triangle(v: vec2<f32>, subpixel: bool) -> f32 {
    let q=vec2<f32>(v.x/max(p.shape.x,0.1),-v.y/max(p.shape.x*p.shape.y,0.1));
    let edge=min(min((q.y+1.0-2.0*q.x)/sqrt(5.0),1.0-q.y),(q.y+2.0*q.x+1.0)/sqrt(5.0));
    if edge<=0.0{return 0.0;}
    return c3_tip(unit(1.0-edge/0.4472136)*p.shape.x,p.shape.x,p.shape.w,subpixel);
}
// SplitMix64 in pairs of 32-bit words, preserving the CPU integer hash exactly.
fn high_mul(a:u32,b:u32)->u32{
    let a0=a&65535u;let a1=a>>16u;let b0=b&65535u;let b1=b>>16u;
    let w0=a0*b0;let t=a1*b0+(w0>>16u);let w1=(t&65535u)+a0*b1;
    return a1*b1+(t>>16u)+(w1>>16u);
}
fn mul64(a:vec2<u32>,b:vec2<u32>)->vec2<u32>{return vec2<u32>(a.x*b.x,high_mul(a.x,b.x)+a.x*b.y+a.y*b.x);}
fn shr64(a:vec2<u32>,n:u32)->vec2<u32>{return vec2<u32>((a.x>>n)|(a.y<<(32u-n)),a.y>>n);}
fn coordinate_hash(q:vec2<i32>,salt:vec2<u32>)->f32{
    var z=vec2<u32>(bitcast<u32>(q.y),bitcast<u32>(q.x))^salt;
    let low=z.x+0x7f4a7c15u;
    z=vec2<u32>(low,z.y+0x9e3779b9u+select(0u,1u,low<z.x));
    z=mul64(z^shr64(z,30u),vec2<u32>(0x1ce4e5b9u,0xbf58476du));
    z=mul64(z^shr64(z,27u),vec2<u32>(0x133111ebu,0x94d049bbu));
    z=z^shr64(z,31u);
    return f32(z.y>>8u)/16777216.0;
}
fn value_noise(pos:vec2<f32>)->f32{
    let q=vec2<i32>(floor(pos));let f0=pos-floor(pos);let f=f0*f0*(vec2<f32>(3.0)-2.0*f0);
    return lerp(lerp(coordinate_hash(q,vec2<u32>(0u)),coordinate_hash(q+vec2<i32>(1,0),vec2<u32>(0u)),f.x),
        lerp(coordinate_hash(q+vec2<i32>(0,1),vec2<u32>(0u)),coordinate_hash(q+vec2<i32>(1,1),vec2<u32>(0u)),f.x),f.y);
}
fn fbm(pos:vec2<f32>)->f32{return (value_noise(pos)*0.56+value_noise(pos*2.07)*0.28+value_noise(pos*4.2849)*0.14)/0.98;}
fn grain_sample(pos:vec2<f32>)->f32{
    let scale=0.7+pow(unit((p.grain.y-1.0)/199.0),1.35)*20.0;
    let warp=scale*0.55*pow(unit(p.grain.z),1.35);
    var delta=vec2<f32>(0.0);
    if warp>0.0001{delta=(vec2<f32>(fbm(pos*0.013+vec2<f32>(3.1,7.2)),fbm(pos*0.013+vec2<f32>(11.3,17.5)))-0.5)*warp;}
    return fbm((pos+delta)/scale);
}
fn grain_alpha(a:f32,q:vec2<i32>)->f32{
    if p.grain2.x<=0.0001||a<=0.0{return a;}
    let pos=vec2<f32>(q);let noise=grain_sample(pos);
    let overlay=select(1.0-2.0*(1.0-a)*(1.0-noise),2.0*a*noise,a<0.5);
    let amount=pow(unit(p.grain.w),1.15);
    var mask=1.0;
    if amount>0.0001{mask=1.0-amount+grain_sample(pos+vec2<f32>(157.0,89.0))*amount;}
    return unit(lerp(a,overlay,unit(p.grain2.x))*mask);
}
fn texture_value(pos_in:vec2<f32>)->f32{
    var pos=pos_in;
    if p.texture_origin.z==1.0{pos-=p.texture_origin.xy;}
    if p.texture_origin.z==2.0{
        let delta=vec2<f32>(pos.x-p.center.z,-(pos.y-p.center.w));
        let local=vec2<f32>(delta.x*p.rotation.y+delta.y*p.rotation.x,-delta.x*p.rotation.x+delta.y*p.rotation.y)*p.rotation.zw;
        pos=vec2<f32>(local.x+p.shape.x,select(local.y,-local.y,p.flags.x!=0u)/max(p.projection.w,0.001)+p.shape.x);
    }
    // PatternImage::sample_wrap uses nearest floor, despite its older bilinear doc comment.
    return at_gray(p.texture,vec2<i32>(floor(pos/p.texture_params.x-0.5)),true);
}
fn mask_combine(mode:u32,v:f32,t:f32,d:f32)->f32{
    if v<=0.0{return 0.0;}
    var result=v;
    switch mode{
        case 0u:{result=v+d*t-v*d*t;}
        case 1u:{result=v*(1.0-d+d*t);}
        case 2u:{result=max(v-d*(1.0-t),0.0);}
        case 3u:{result=min(v,1.0-d+d*t);}
        case 4u:{let o=select(1.0-2.0*(1.0-v)*(1.0-t),2.0*v*t,v<0.5);result=lerp(v,o,d);}
        case 5u:{result=lerp(v,min(v/max(1.0-t,0.001),1.0),d);}
        case 6u:{result=lerp(v,1.0-min((1.0-v)/max(t,0.001),1.0),d);}
        case 7u:{result=lerp(v,max(v+t-1.0,0.0),d);}
        case 8u:{result=lerp(v,select(0.0,1.0,v+t>1.0),d);}
        case 9u:{result=v*unit((t-(1.0-d*v))*4.0+0.5);}
        case 10u:{result=select(0.0,v,t>=1.0-d*v);}
        default:{}
    }
    return unit(result);
}
fn texture_combine(v:f32,t:f32)->f32{
    let d=p.texture_params.y;let mode=u32(p.texture_params.z);
    if p.texture_params.w!=0.0{return mask_combine(mode,v,t,d);}
    if v<=0.0{return 0.0;}
    switch mode{
        case 2u:{return unit(v-d*t);}
        case 5u:{return unit(v/max(1.0-d*t,0.001));}
        case 6u:{return unit(1.0-(1.0-v)/max(1.0-d+d*t,0.001));}
        case 10u:{return unit(12.0*round(d*255.0)/255.0*v-t);}
        case 9u:{let reach=12.0*round(d*255.0)/255.0*v;return unit(select(reach-t,reach*(1.0-t),reach<1.0));}
        default:{return mask_combine(mode,v,t,d);}
    }
}
fn color_at(q:vec2<i32>)->vec4<f32>{
    let pos=clamp(q,vec2<i32>(0),vec2<i32>(p.pigment.yz)-1);
    let offset=p.pigment.x+4u*(u32(pos.y)*p.pigment.y+u32(pos.x));
    return vec4<f32>(data[offset],data[offset+1u],data[offset+2u],data[offset+3u]);
}
fn pattern_color(pos_in:vec2<f32>)->vec4<f32>{
    let pos=pos_in+vec2<f32>(p.pigment.yz)*0.5-0.5;
    if any(pos<vec2<f32>(-0.5))||any(pos>=vec2<f32>(p.pigment.yz)-0.5){return vec4<f32>(0.0);}
    let uv=max(pos,vec2<f32>(0.0));let lo=vec2<i32>(floor(uv));let f=fract(uv);
    return (color_at(lo)+(color_at(lo+vec2<i32>(1,0))-color_at(lo))*f.x)*(1.0-f.y)
        +(color_at(lo+vec2<i32>(0,1))+(color_at(lo+vec2<i32>(1,1))-color_at(lo+vec2<i32>(0,1)))*f.x)*f.y;
}
fn sample_pigment(pos:vec2<f32>)->vec4<f32>{
    let center=pattern_color(pos);if center.a<=0.0001{return center;}
    var sum=center.rgb*center.a*4.0;var weight=center.a*4.0;
    let half_size=vec2<f32>(p.pigment.yz)*0.5;
    for(var i=0;i<4;i++){
        var offset=vec2<f32>(-1.0,0.0);
        if i==1{offset=vec2<f32>(1.0,0.0);}if i==2{offset=vec2<f32>(0.0,-1.0);}if i==3{offset=vec2<f32>(0.0,1.0);}
        let value=pattern_color(clamp(pos+offset,-half_size+0.5,half_size-0.5));
        weight+=value.a;sum+=value.rgb*value.a;
    }
    return vec4<f32>(sum/max(weight,0.0001),center.a);
}
fn lum(c:vec3<f32>)->f32{return dot(c,vec3<f32>(0.3,0.59,0.11));}
fn set_lum(c:vec3<f32>,l:f32)->vec3<f32>{
    var o=c+l-lum(c);let n=min(min(o.r,o.g),o.b);let high=max(max(o.r,o.g),o.b);
    if n<0.0{o=vec3<f32>(l)+(o-l)*l/max(l-n,0.0001);}
    if high>1.0{o=vec3<f32>(l)+(o-l)*(1.0-l)/max(high-l,0.0001);}
    return clamp(o,vec3<f32>(0.0),vec3<f32>(1.0));
}
fn burn(a:f32,b:f32)->f32{if b<=0.0001{return 0.0;}return 1.0-min((1.0-a)/b,1.0);}
fn dodge(a:f32,b:f32)->f32{if b>=0.9999{return 1.0;}return min(a/max(1.0-b,0.0001),1.0);}
fn blend_channel(a:f32,b:f32,mode:u32)->f32{
    switch mode{
        case 2u:{return min(a,b);}case 3u:{return a*b;}case 4u:{return burn(a,b);}case 5u:{return max(a+b-1.0,0.0);}
        case 7u:{return max(a,b);}case 8u:{return 1.0-(1.0-a)*(1.0-b);}case 9u:{return dodge(a,b);}case 10u:{return min(a+b,1.0);}
        case 12u:{return select(1.0-2.0*(1.0-a)*(1.0-b),2.0*a*b,a<=0.5);}
        case 13u:{if b<=0.5{return a-(1.0-2.0*b)*a*(1.0-a);}return a+(2.0*b-1.0)*(sqrt(max(a,0.0))-a);}
        case 14u:{return select(1.0-2.0*(1.0-a)*(1.0-b),2.0*a*b,b<=0.5);}
        case 15u,18u:{var v=dodge(a,2.0*(b-0.5));if b<=0.5{v=burn(a,2.0*b);}if mode==18u{return select(0.0,1.0,v>=0.5);}return v;}
        case 16u:{return unit(a+2.0*b-1.0);}case 17u:{return select(max(a,2.0*b-1.0),min(a,2.0*b),b<=0.5);}
        case 19u:{return abs(a-b);}case 20u:{return a+b-2.0*a*b;}case 21u:{return max(a-b,0.0);}
        case 22u:{if b<=0.0001{return 1.0;}return unit(a/b);}case 33u:{return 1.0-a;}
        default:{return b;}
    }
}
fn attribute_hsl(c:vec3<f32>)->vec3<f32>{
    let hi=max(max(c.r,c.g),c.b);let lo=min(min(c.r,c.g),c.b);let l=(hi+lo)*0.5;let d=hi-lo;
    if d<0.000001{return vec3<f32>(0.0,0.0,l);}
    var sat=d/(hi+lo);if l>0.5{sat=d/(2.0-hi-lo);}
    var h=(c.r-c.g)/d+4.0;
    if hi==c.r{h=(c.g-c.b)/d+select(0.0,6.0,c.g<c.b);}else if hi==c.g{h=(c.b-c.r)/d+2.0;}
    return vec3<f32>(h/6.0,sat,l);
}
fn attribute_hue(t_in:f32,a:f32,b:f32)->f32{
    let t=t_in-floor(t_in);
    if t<1.0/6.0{return a+(b-a)*6.0*t;}
    if t<0.5{return b;}
    if t<2.0/3.0{return a+(b-a)*(2.0/3.0-t)*6.0;}
    return a;
}
fn attribute_rgb(hsl:vec3<f32>)->vec3<f32>{
    if hsl.y<0.000001{return vec3<f32>(hsl.z);}
    var q=hsl.z*(1.0+hsl.y);if hsl.z>=0.5{q=hsl.z+hsl.y-hsl.z*hsl.y;}
    let a=2.0*hsl.z-q;
    return vec3<f32>(attribute_hue(hsl.x+1.0/3.0,a,q),attribute_hue(hsl.x,a,q),attribute_hue(hsl.x-1.0/3.0,a,q));
}
fn canvas_blend(d:vec3<f32>,value:f32,mode:u32)->vec3<f32>{
    let s=vec3<f32>(value);
    if mode==6u{return select(s,d,d.r+d.g+d.b<=3.0*value);}
    if mode==11u{return select(s,d,d.r+d.g+d.b>=3.0*value);}
    if mode==23u{let h=attribute_hsl(d);return attribute_rgb(vec3<f32>(0.0,h.y,h.z));}
    if mode==24u{return vec3<f32>(attribute_hsl(d).z);}
    if mode==25u{return set_lum(s,lum(d));}
    if mode==26u{return set_lum(d,value);}
    if mode==27u{
        let burned=vec3<f32>(burn(d.r,value),burn(d.g,value),burn(d.b,value));let a=attribute_hsl(d);let c=attribute_hsl(burned);
        let gate=unit((1.0-smooth_value(0.72,1.0,a.z))*(0.35+0.65*smooth_value(0.015,0.16,a.y)));
        let tone_target=attribute_rgb(vec3<f32>(a.xy,unit(a.z-max(a.z-c.z,0.0)*gate)));
        let mixed=attribute_hsl(d+(tone_target-d)*(gate*gate*(3.0-2.0*gate)));
        return attribute_rgb(vec3<f32>(a.xy,min(mixed.z,a.z)));
    }
    if mode==28u{
        let intensity=1.0-(1.0-lum(d))*(1.0-lum(s));let rainbow=attribute_rgb(vec3<f32>(fract(intensity*4.0),1.0,0.5));
        let t=unit((1.0-value)*0.8);return vec3<f32>(1.0)-(vec3<f32>(1.0)-d)*(vec3<f32>(1.0)-(rainbow+(s-rainbow)*t));
    }
    if mode==29u{
        let a=lum(d);let b=lum(s);let l=select(2.0*a*b,1.0-2.0*(1.0-a)*(1.0-b),a>=0.5);
        let t=unit(0.2+abs(l-a)*1.6);let c=d+(s-d)*t;return clamp(c*(l/max(lum(c),0.0001)),vec3<f32>(0.0),vec3<f32>(1.0));
    }
    if mode==30u{
        let l=lum(d);let c=clamp(d+s*(l*0.85+0.15),vec3<f32>(0.0),vec3<f32>(1.0));let out=max(lum(c),0.0001);
        return clamp(vec3<f32>(out)+(c-out)*(1.0+smooth_value(0.35,0.95,l)*0.16),vec3<f32>(0.0),vec3<f32>(1.0));
    }
    if mode==31u{
        if value>0.5{return clamp(d+2.0*(value-0.5)*s*d,vec3<f32>(0.0),vec3<f32>(1.0));}
        return clamp(d*(vec3<f32>(1.0)-2.0*(0.5-value)*(vec3<f32>(1.0)-d)),vec3<f32>(0.0),vec3<f32>(1.0));
    }
    return vec3<f32>(blend_channel(d.r,value,mode),blend_channel(d.g,value,mode),blend_channel(d.b,value,mode));
}
fn canvas_color(paint:vec4<f32>,pos:vec2<f32>)->vec4<f32>{
    if p.canvas.y==0u||p.canvas_params.y<=0.0{return paint;}
    let v=unit(bilinear_gray(p.canvas,pos/p.canvas_params.x-0.5,true))*255.0;
    let index=u32(floor(v));let offset=u32(p.canvas_params.w);
    let mapped=lerp(data[offset+index],data[offset+min(index+1u,255u)],v-floor(v));
    let blended=clamp(canvas_blend(paint.rgb,mapped,u32(p.canvas_params.z)),vec3<f32>(0.0),vec3<f32>(1.0));
    return vec4<f32>(paint.rgb+(blended-paint.rgb)*unit(p.canvas_params.y),paint.a);
}
fn hard_tip(pos:vec2<f32>)->f32{
    let delta=vec2<f32>(pos.x+0.5-p.center.z,-(pos.y+0.5-p.center.w));
    let uv=vec2<f32>(delta.x*p.rotation.y+delta.y*p.rotation.x,-delta.x*p.rotation.x+delta.y*p.rotation.y);
    let ro=max(p.projection.w,0.01);let radius=max(p.shape.x,0.1);
    if p.flags.z!=0u{
        let q=vec2<f32>(uv.x/max(radius,0.1),-uv.y/max(radius*ro,0.1));
        let edge=min(min((q.y+1.0-2.0*q.x)/sqrt(5.0),1.0-q.y),(q.y+2.0*q.x+1.0)/sqrt(5.0));
        if edge<=0.0{return 0.0;}
        return c3_tip(unit(1.0-edge/0.4472136)*radius,radius,1.0,false);
    }
    if p.tip_base.y!=0u{
        let size=vec2<f32>(p.tip_base.yz);let big=max(size.x,size.y);
        let flipped=uv*p.rotation.zw;
        return sample_tip(p.tip_base,vec2<f32>(flipped.x,-flipped.y/ro)*big/max(p.shape.x*2.0,0.2)/size+0.5);
    }
    let distance=length(vec2<f32>(uv.x,uv.y/ro));
    if distance>=p.grain2.z+2.6*p.grain2.w+0.5{return 0.0;}
    if distance<=p.grain2.z{return 1.0;}
    let x=(distance-p.grain2.z)/p.grain2.w;
    var g=unit(1.0-x*x/64.0);for(var i=0;i<6;i++){g*=g;}return g;
}
fn emboss_height(mask:f32,pos:vec2<f32>)->f32{
    if mask<=0.0001{return 0.0;}
    let step=clamp(p.shape.x*0.075,0.65,3.0);
    var inner=0.0;var outer=0.0;var edge=0.0;
    for(var i=0;i<8;i++){
        var direction=vec2<f32>(1.0,0.0);
        switch i{
            case 1:{direction=vec2<f32>(-1.0,0.0);}case 2:{direction=vec2<f32>(0.0,1.0);}case 3:{direction=vec2<f32>(0.0,-1.0);}
            case 4:{direction=vec2<f32>(0.70710677,0.70710677);}case 5:{direction=vec2<f32>(-0.70710677,0.70710677);}
            case 6:{direction=vec2<f32>(0.70710677,-0.70710677);}case 7:{direction=vec2<f32>(-0.70710677,-0.70710677);}default:{}
        }
        let n=hard_tip(pos+direction*step);let f=hard_tip(pos+direction*step*2.15);
        edge+=abs(mask-n);inner+=n;outer+=f;
    }
    edge=unit(edge*0.22);inner*=0.125;outer*=0.125;
    let interior=smooth_value(0.015,0.84,mask);
    let rim=smooth_value(0.02,0.42,abs(inner-outer))*smooth_value(0.015,0.65,mask);
    let delta=vec2<f32>(pos.x+0.5-p.center.z,-(pos.y+0.5-p.center.w));
    let uv=vec2<f32>(delta.x*p.rotation.y+delta.y*p.rotation.x,select(-delta.x*p.rotation.x+delta.y*p.rotation.y,delta.x*p.rotation.x-delta.y*p.rotation.y,p.flags.x!=0u));
    var aspect=vec2<f32>(1.0);
    if p.tip_base.y!=0u{let size=vec2<f32>(p.tip_base.yz);aspect=size/max(size.x,size.y);}
    let local=uv/max(vec2<f32>(p.shape.x,p.shape.x*p.projection.w)*aspect,vec2<f32>(0.1));
    var pattern=1.0;
    if p.texture_origin.w!=0.0{pattern=texture_value(pos+0.5);}else if p.tip_base.y!=0u{pattern=hard_tip(pos);}
    let gap=smooth_value(0.12,0.78,1.0-pattern);let x=local.x;let y=local.y;
    let layers=0.28*smooth_value(0.48,0.56,fract(x*8.0+y*1.3))+0.24*smooth_value(0.46,0.55,fract(x*13.0-y*2.1+0.31))
        +0.26*smooth_value(0.47,0.57,fract(y*10.0+x*1.7+0.63))+0.22*smooth_value(0.45,0.56,fract((x+y)*18.0+0.17));
    return unit(interior*0.34+edge*0.18+rim*0.24+(1.0-gap*layers)*0.24);
}
@compute @workgroup_size(8,8)
fn stroke_texture(@builtin(global_invocation_id) gid:vec3<u32>){
    if any(gid.xy>=p.rect.zw){return;}
    let local=vec2<i32>(gid.xy+p.rect.xy);let q=local+p.origin.xy;
    let c=textureLoad(stroke_coverage,local,0).r;var result=c;
    if p.texture.y>0u&&p.texture_origin.w==0.0{result=texture_combine(c,texture_value(vec2<f32>(q)+0.5));}
    textureStore(coverage_out,local,vec4<f32>(c,result,0.0,0.0));
}
@compute @workgroup_size(8,8)
fn raster(@builtin(global_invocation_id) gid:vec3<u32>){
    if any(gid.xy>=p.rect.zw){return;}
    let local=vec2<i32>(gid.xy+p.rect.xy);let q=local+p.origin.xy;
    let pos=vec2<f32>(q)+0.5;let delta=pos-p.center.xy;let uv=to_tip(delta);
    let radius=p.shape.x;let ro=p.shape.y;let rm=p.shape.z;let hardness=p.shape.w;
    let aliased=p.flags.y!=0u;let c3=p.flags.x!=0u;let sampled=p.flags2.x!=0u;
    let small=!sampled&&!aliased&&select(rm<3.0,radius*2.0<5.0,c3);
    var a=0.0;var rn=0.0;
    if p.flags.z!=0u{
        if small{
            for(var sy=0;sy<4;sy++){for(var sx=0;sx<4;sx++){
                a+=triangle(to_tip(delta+vec2<f32>(-0.375+f32(sx)*0.25,-0.375+f32(sy)*0.25)),true);
            }}a/=16.0;
        }else{a=triangle(uv,false);}
        if aliased{a=select(0.0,1.0,a>=0.5);}rn=1.0-a;
    }else if aliased&&p.flags.w!=0u{
        if abs(delta.x)<radius&&abs(delta.y)<radius{a=1.0;}
    }else if !sampled{
        let d2=(uv.x*ro)*(uv.x*ro)+uv.y*uv.y;let distance=sqrt(d2);rn=distance/rm;
        if small{
            if d2<(p.tip_shape.z+0.75)*(p.tip_shape.z+0.75){
                for(var sy=0;sy<4;sy++){for(var sx=0;sx<4;sx++){
                    let v=to_tip(delta+vec2<f32>(-0.375+f32(sx)*0.25,-0.375+f32(sy)*0.25));let sd=sqrt((v.x*ro)*(v.x*ro)+v.y*v.y);
                    if c3{a+=c3_tip(sd/ro,radius,hardness,true);}else{a+=regular_tip(sd);}
                }}a/=16.0;
            }
        }else if d2<(p.tip_shape.z+0.5)*(p.tip_shape.z+0.5){
            if aliased{a=select(0.0,1.0,distance<=rm);}else if c3{a=c3_tip(distance/ro,radius,hardness,false);}
            else if distance<p.tip_shape.z+0.5{a=regular_tip(distance);}
        }
    }else{
        let size=vec2<f32>(p.tip_base.yz);let inv_scale=max(size.x,size.y)/(2.0*radius);
        let pixel=vec2<f32>(uv.x,-uv.y/ro)*inv_scale+size*0.5;
        if all(pixel>=vec2<f32>(-1.0))&&all(pixel<=size+1.0){
            if c3{a=c3_sample(pixel/size);}else{a=sample_tip(p.tip,pixel/size);}
            if aliased{a=select(0.0,1.0,a>=0.5);}rn=1.0-a;
        }
    }
    var edge=0.0;var depth_value=0.0;var effect=0.0;
    var paint=p.paint;
    if a>0.0{
        if p.flags2.z!=0u&&a<1.0{
            let n=coordinate_hash(q,vec2<u32>(0x171e1bc3u,0x78c2590au));let hard=select(0.0,1.0,n<a);a+=(hard-a)*0.7;
        }
        if p.grain.x!=0.0{a=grain_alpha(a,q);}
        if p.flags2.y!=0u{edge=dab_edge(a,rn,p.resolved.xyz);a=wet_alpha(a,edge,p.resolved.xyz,true);effect=color_strength(p.resolved.x)*edge;}
        if p.texture_origin.w!=0.0{a=texture_combine(a,texture_value(pos));}
        if p.grain2.y!=0.0{a*=0.5+0.5*smooth_value(0.5,1.0,rn);}
        depth_value=select(unit(1.0-rn),edge,p.flags2.y!=0u);
        a*=p.tip_shape.w;
    }
    if p.pigment.y>0u{
        let scale=f32(max(p.pigment.y,p.pigment.z))/max(2.0*radius,1.0);
        paint=sample_pigment(scale*(pos-p.center.zw));a*=paint.a;
    }
    if p.flags2.w!=0u{paint=canvas_color(paint,pos);}
    if p.flags2.y!=0u{paint=color_with_effect(paint,textureLoad(base,local,0),effect);}
    if p.flags2.w==0u{
        if p.native.z!=0u{paint=native_to_rgba(p.native.x,p.native.y!=0u,native_from_rgba(p.native.x,p.native.y!=0u,paint));}
        paint=canvas_color(paint,pos);
    }
    textureStore(coverage_out,local,vec4<f32>(a,0.0,0.0,0.0));
    var emboss=0.0;
    if p.origin.z!=0&&p.tip_shape.w>0.0{emboss=emboss_height(a/max(p.tip_shape.w,0.000001),vec2<f32>(q));}
    textureStore(depth_out,local,vec4<f32>(depth_value,edge,effect,emboss));
    // Pattern alpha has already masked coverage. The native per-dab colour buffer carries
    // colour channels only; transport keeps the dab's own source alpha.
    textureStore(source_out,local,vec4<f32>(paint.rgb,p.paint.a));
}
