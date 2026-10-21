const M_NORMAL: i32 = 1;
const M_DISSOLVE: i32 = 2;

// Hard Mix alone uses the CPU reference's 1e-4 backdrop edge tolerance.
const EDGE: f32 = 1e-4;
fn color_burn(cb: f32, cs: f32) -> f32 {
    if (cb >= 1.0) { return 1.0; }
    if (cs <= 0.0) { return 0.0; }
    return 1.0 - min((1.0 - cb) / cs, 1.0);
}
fn color_dodge(cb: f32, cs: f32) -> f32 {
    if (cb <= 0.0) { return 0.0; }
    if (cs >= 1.0) { return 1.0; }
    return min(cb / (1.0 - cs), 1.0);
}
fn hard_light(cb: f32, cs: f32) -> f32 {
    if (cs <= 0.5) { return cb * 2.0 * cs; }
    let s = 2.0 * cs - 1.0;
    return cb + s - cb * s;
}
fn soft_light_ps(cb: f32, cs: f32) -> f32 {
    if (cs <= 0.5) { return 2.0 * cb * cs + cb * cb * (1.0 - 2.0 * cs); }
    return 2.0 * cb * (1.0 - cs) + sqrt(max(cb, 0.0)) * (2.0 * cs - 1.0);
}
fn vivid_light_ps(cb: f32, cs: f32) -> f32 {
    if (cs <= 0.5) {
        if (cs <= 0.0) { return 0.0; }
        return 1.0 - min((1.0 - cb) / (2.0 * cs), 1.0);
    }
    if (cs >= 1.0) { return 1.0; }
    return min(cb / (2.0 * (1.0 - cs)), 1.0);
}
fn vivid_light_generic(cb: f32, cs: f32) -> f32 {
    if (cs <= 0.5) {
        if (cb >= 1.0 - EDGE) { return 1.0; }
        if (cs <= 0.0) { return 0.0; }
        return 1.0 - min((1.0 - cb) / (2.0 * cs), 1.0);
    }
    if (cb <= EDGE) { return 0.0; }
    if (cs >= 1.0) { return 1.0; }
    return min(cb / (2.0 * (1.0 - cs)), 1.0);
}

// f32::MAX exactly. The decimal 3.40282347e38 is just above it: naga rounds it down, but browsers'
// WebGPU compiler rejects it, which left the web app without a GPU compositor.
const F32_MAX: f32 = 0x1.fffffep+127f;

fn blend_channel(mode: i32, cb: f32, cs: f32) -> f32 {
    // Linear Dodge / Divide clip at 1 at integer depths, at f32::MAX in 32-bit documents.
    let hi = 1.0;
    switch mode {
        case 3: { return min(cb, cs); }                         // Darken
        case 4: { return cb * cs; }                             // Multiply
        case 5: { return color_burn(cb, cs); }                  // ColorBurn
        case 6: { return max(cb + cs - 1.0, 0.0); }             // LinearBurn
        case 8: { return max(cb, cs); }                         // Lighten
        case 9: { return cb + cs - cb * cs; }                   // Screen
        case 10: { return color_dodge(cb, cs); }                // ColorDodge
        case 11: { return min(cb + cs, hi); }                   // LinearDodge
        case 13: { return hard_light(cs, cb); }                 // Overlay
        case 14: { return soft_light_ps(cb, cs); }              // SoftLight
        case 15: { return hard_light(cb, cs); }                 // HardLight
        case 16: { return vivid_light_ps(cb, cs); }             // VividLight (Photoshop)
        case 17: { return clamp(cb + 2.0 * cs - 1.0, 0.0, 1.0); } // LinearLight
        case 18: {                                              // PinLight
            if (cs <= 0.5) { return min(cb, 2.0 * cs); }
            return max(cb, 2.0 * cs - 1.0);
        }
        case 19: {                                              // HardMix (Photoshop)
            return select(0.0, 1.0, vivid_light_generic(cb, cs) >= 0.5 - 1e-6);
        }
        case 20: { return abs(cb - cs); }                       // Difference
        case 21: { return cb + cs - 2.0 * cb * cs; }            // Exclusion
        case 22: { return max(cb - cs, 0.0); }                  // Subtract
        case 23: {                                              // Divide
            if (cs <= 0.0) { return select(1.0, 0.0, cb <= 0.0); }
            return min(cb / cs, hi);
        }
        case 28: {                                             // Reflect
            if (cs >= 1.0) { return 1.0; }
            return min(cb * cb / (1.0 - cs), 1.0);
        }
        case 29: {                                             // Glow
            if (cb >= 1.0) { return 1.0; }
            return min(cs * cs / (1.0 - cb), 1.0);
        }
        case 30: { return 1.0 - abs(1.0 - cb - cs); }            // Negation
        case 31: {                                             // XOR (8-bit channel operation)
            let b = u32(floor(clamp(cb, 0.0, 1.0) * 255.0 + 0.5));
            let s = u32(floor(clamp(cs, 0.0, 1.0) * 255.0 + 0.5));
            return f32(b ^ s) / 255.0;
        }
        case 32: {                                             // Paint.NET Color Burn
            if (cs <= 0.0) { return 0.0; }
            return max(1.0 - (1.0 - cb) / cs, 0.0);
        }
        case 33: {                                             // Paint.NET Color Dodge
            if (cs >= 1.0) { return 1.0; }
            return min(cb / (1.0 - cs), 1.0);
        }
        default: { return cs; }                                 // Normal, Dissolve, PassThrough
    }
}

fn lum(c: vec3<f32>) -> f32 { return 0.3 * c.r + 0.59 * c.g + 0.11 * c.b; }

// photocraft_color::blend::clip_color: towards grey until within 0..top; a luminosity outside
// that range has no in-gamut colour (and max − lum is rounding noise), so it gives its grey.
fn clip_color(c: vec3<f32>, top: f32) -> vec3<f32> {
    let l = lum(c);
    let n = min(min(c.r, c.g), c.b);
    let x = max(max(c.r, c.g), c.b);
    var out = c;
    if (n < 0.0) {
        let d = l - n;
        if (l > 0.0 && d > 0.0) { out = l + (out - l) * l / d; } else { out = vec3(l); }
    }
    if (x > top) {
        let d = x - l;
        if (l < top && d > 0.0) { out = l + (out - l) * (top - l) / d; } else { out = vec3(l); }
    }
    return out;
}
fn set_lum(c: vec3<f32>, l: f32, top: f32) -> vec3<f32> { return clip_color(c + (l - lum(c)), top); }
fn sat(c: vec3<f32>) -> f32 { return max(max(c.r, c.g), c.b) - min(min(c.r, c.g), c.b); }
fn set_sat(c: vec3<f32>, s: f32) -> vec3<f32> {
    let mx = max(max(c.r, c.g), c.b);
    let mn = min(min(c.r, c.g), c.b);
    if (mx > mn) { return (c - mn) * s / (mx - mn); }
    return vec3(0.0);
}

fn blend_rgb(mode: i32, cb: vec3<f32>, cs: vec3<f32>) -> vec3<f32> {
    // blend_rgb_within: 32-bit documents keep values above 1 (f32::MAX).
    let top = 1.0;
    switch mode {
        case 24: { return set_lum(set_sat(cs, sat(cb)), lum(cb), top); } // Hue
        case 25: { return set_lum(set_sat(cb, sat(cs)), lum(cb), top); } // Saturation
        case 26: { return set_lum(cs, lum(cb), top); }                  // Color
        case 27: { return set_lum(cb, lum(cs), top); }                  // Luminosity
        case 7: { return select(cb, cs, lum(cs) < lum(cb)); }      // DarkerColor
        case 12: { return select(cb, cs, lum(cs) > lum(cb)); }     // LighterColor
        default: {
            return vec3(blend_channel(mode, cb.r, cs.r), blend_channel(mode, cb.g, cs.g), blend_channel(mode, cb.b, cs.b));
        }
    }
}

fn composite(mode: i32, b: vec4<f32>, s: vec4<f32>, opacity: f32) -> vec4<f32> {
    let ab = b.a;
    let as_ = s.a * opacity;
    if (as_ <= 0.0) { return b; }
    let bl = blend_rgb(mode, b.rgb, s.rgb);
    let ao = as_ + ab * (1.0 - as_);
    if (ao <= 0.0) { return vec4(0.0); }
    let rgb = ((1.0 - as_) * ab * b.rgb + (1.0 - ab) * as_ * s.rgb + as_ * ab * bl) / ao;
    return vec4(rgb, ao);
}

struct Lane { color: vec4<f32>, gains: vec4<f32>, modes: vec4<u32>, attributes: vec4<f32>, flags: vec4<u32> }
struct Params { rect: vec4<u32>, origin: vec2<i32>, seed: vec2<u32>, primary: Lane, secondary: Lane, dab: vec4<f32>, wet: vec4<f32>, controls: vec4<u32>, mask: vec4<u32>, texture_depth: vec4<f32>, native: vec4<u32>, native_color: vec4<f32> }
@group(0) @binding(0) var in0: texture_2d<f32>;
@group(0) @binding(1) var in1: texture_2d<f32>;
@group(0) @binding(2) var in2: texture_2d<f32>;
@group(0) @binding(3) var in3: texture_2d<f32>;
@group(0) @binding(4) var in4: texture_2d<f32>;
@group(0) @binding(5) var in5: texture_2d<f32>;
@group(0) @binding(6) var in6: texture_2d<f32>;
@group(0) @binding(7) var in7: texture_2d<f32>;
@group(0) @binding(8) var out0: texture_storage_2d<rgba32float,write>;
@group(0) @binding(9) var out1: texture_storage_2d<rgba32float,write>;
@group(0) @binding(10) var out2: texture_storage_2d<rgba32float,write>;
@group(0) @binding(11) var<uniform> params: Params;
@group(0) @binding(12) var<storage,read> lut: array<f32>;
fn hsl(c: vec3<f32>) -> vec3<f32> {
    let mx=max(c.r,max(c.g,c.b)); let mn=min(c.r,min(c.g,c.b)); let l=(mx+mn)*0.5; let d=mx-mn;
    if(d<0.000001) {return vec3(0.0,0.0,l);}
    var s=d/(mx+mn); if(l>0.5) {s=d/(2.0-mx-mn);}
    var h=0.0;
    if(mx==c.r) {h=(c.g-c.b)/d; if(c.g<c.b) {h+=6.0;}} else if(mx==c.g) {h=(c.b-c.r)/d+2.0;} else {h=(c.r-c.g)/d+4.0;}
    return vec3(h/6.0,s,l);
}
fn hue(p: f32,q: f32,t0: f32) -> f32 {let t=fract(t0); if(t<1.0/6.0) {return p+(q-p)*6.0*t;} if(t<0.5) {return q;} if(t<2.0/3.0) {return p+(q-p)*(2.0/3.0-t)*6.0;} return p;}
fn rgb(c: vec3<f32>) -> vec3<f32> {
    let h=c.x; let s=c.y; let l=c.z; if(s<0.000001) {return vec3(l);}
    var q=l*(1.0+s); if(l>=0.5) {q=l+s-l*s;} let p=2.0*l-q;
    return vec3(hue(p,q,h+1.0/3.0),hue(p,q,h),hue(p,q,h-1.0/3.0));
}
fn attr_burn(a: f32,b: f32) -> f32 {if(b<=0.0001) {return 0.0;} return 1.0-min((1.0-a)/b,1.0);}
fn attr_dodge(a: f32,b: f32) -> f32 {if(b>=0.9999) {return 1.0;} return min(a/max(1.0-b,0.0001),1.0);}
fn attr_set_lum(c: vec3<f32>,l: f32) -> vec3<f32> {
    var o=c+vec3(l-lum(c)); let n=min(o.r,min(o.g,o.b)); let x=max(o.r,max(o.g,o.b));
    if(n<0.0) {o=vec3(l)+(o-vec3(l))*l/max(l-n,0.0001);}
    if(x>1.0) {o=vec3(l)+(o-vec3(l))*(1.0-l)/max(x-l,0.0001);}
    return clamp(o,vec3(0.0),vec3(1.0));
}
fn attr_blend(mode: u32,d: vec3<f32>,s: vec3<f32>) -> vec3<f32> {
    switch mode {
        case 2u,3u,5u,7u,8u,10u,12u,14u,16u,17u,19u,20u,21u: {return blend_rgb(i32(mode)+1,d,s);}
        case 4u: {return vec3(attr_burn(d.r,s.r),attr_burn(d.g,s.g),attr_burn(d.b,s.b));}
        case 9u: {return vec3(attr_dodge(d.r,s.r),attr_dodge(d.g,s.g),attr_dodge(d.b,s.b));}
        case 13u: {
            var out=vec3(0.0); for(var i=0u;i<3u;i++) {if(s[i]<=0.5) {out[i]=d[i]-(1.0-2.0*s[i])*d[i]*(1.0-d[i]);} else {out[i]=d[i]+(2.0*s[i]-1.0)*(sqrt(max(d[i],0.0))-d[i]);}} return out;
        }
        case 15u,18u: {
            var out=vec3(0.0); for(var i=0u;i<3u;i++) {var v=0.0;if(s[i]<=0.5) {v=attr_burn(d[i],2.0*s[i]);} else {v=attr_dodge(d[i],2.0*(s[i]-0.5));} if(mode==18u) {v=select(0.0,1.0,v>=0.5);} out[i]=v;} return out;
        }
        case 22u: {var out=vec3(0.0); for(var i=0u;i<3u;i++) {if(s[i]<=0.0001) {out[i]=1.0;} else {out[i]=unit(d[i]/s[i]);}} return out;}
        case 23u: {let a=hsl(d);let b=hsl(s);return rgb(vec3(b.x,a.y,a.z));}
        case 24u: {let a=hsl(d);let b=hsl(s);return rgb(vec3(a.x,b.y,a.z));}
        case 25u: {return attr_set_lum(s,lum(d));}
        case 26u: {return attr_set_lum(d,lum(s));}
        case 6u: {return select(s,d,d.r+d.g+d.b<=s.r+s.g+s.b);}
        case 11u: {return select(s,d,d.r+d.g+d.b>=s.r+s.g+s.b);}
        case 33u: {return vec3(1.0)-d;}
        case 28u: {
            let intensity=1.0-(1.0-lum(d))*(1.0-lum(s)); let rainbow=rgb(vec3(fract(intensity*4.0),1.0,0.5));let t=unit((1.0-min(s.r,min(s.g,s.b)))*0.8);
            return vec3(1.0)-(vec3(1.0)-d)*(vec3(1.0)-lerp3(rainbow,s,t));
        }
        case 29u: {
            let a=lum(d);let b=lum(s);var l=2.0*a*b;if(a>=0.5) {l=1.0-2.0*(1.0-a)*(1.0-b);}let t=unit(0.2+abs(l-a)*1.6);let c=lerp3(d,s,t);return clamp(c*(l/max(lum(c),0.0001)),vec3(0.0),vec3(1.0));
        }
        case 30u: {
            let l=lum(d);let c=clamp(d+s*(l*0.85+0.15),vec3(0.0),vec3(1.0));let out=max(lum(c),0.0001);let boost=smooth_value(0.35,0.95,l)*0.16;return clamp(vec3(out)+(c-vec3(out))*(1.0+boost),vec3(0.0),vec3(1.0));
        }
        case 31u: {
            let a=lum(s);if(a>0.5) {return clamp(d+2.0*(a-0.5)*s*d,vec3(0.0),vec3(1.0));}return clamp(d*(vec3(1.0)-2.0*(0.5-a)*(vec3(1.0)-d)),vec3(0.0),vec3(1.0));
        }
        case 27u: {
            let burned=vec3(attr_burn(d.r,s.r),attr_burn(d.g,s.g),attr_burn(d.b,s.b));let a=hsl(d);let b=hsl(s);let c=hsl(burned);
            let gate=unit((1.0-smooth_value(0.72,1.0,a.z))*(0.35+0.65*smooth_value(0.015,0.16,a.y)));let target_color=rgb(vec3(a.x,a.y,unit(a.z-max(a.z-c.z,0.0)*gate)));
            let mixed=hsl(lerp3(d,target_color,gate*gate*(3.0-2.0*gate)));let bias=gate*smooth_value(0.08,0.65,b.y);let dh=fract(b.x-a.x+0.5)-0.5;
            return rgb(vec3(fract(a.x+dh*0.42*bias),unit(a.y+(b.y-a.y)*0.3*bias),min(mixed.z,a.z)));
        }
        default: {return s;}
    }
}
fn attributes(normal: vec4<f32>,dst: vec4<f32>,src: vec4<f32>,amount: f32,lane: Lane) -> vec4<f32> {
    if(lane.modes.y==0u || amount<=0.0001 || normal.a<=0.5/255.0) {return normal;}
    let brush=clamp(src.rgb,vec3(0.0),vec3(1.0));var out=normal.rgb;
    if(lane.modes.z!=0u) {let mixed=attr_blend(lane.modes.z,dst.rgb,brush);let value=lerp3(dst.rgb,mixed,unit(amount));out=lerp3(out,value,unit(lane.attributes.x));}
    if(lane.modes.w!=0u) {let mixed=attr_blend(lane.modes.w,dst.rgb,brush);let value=lerp3(dst.rgb,mixed,unit(amount));out=lerp3(out,value,unit(lane.attributes.y));}
    return vec4(out,normal.a);
}
fn lane_composite(dst: vec4<f32>,src: vec4<f32>,k: f32,lane: Lane) -> vec4<f32> {
    let amount=k*src.a;let combined=amount+dst.a*(1.0-amount);var normal=dst;
    if(lane.modes.x<=2u) {
        if(params.native.z!=0u){
            let has_alpha=params.native.y!=0u;
            let destination=native_from_rgba(params.native.x,has_alpha,dst);
            let source=native_from_rgba(params.native.x,has_alpha,src);
            normal=native_to_rgba(params.native.x,has_alpha,native_over(destination,source,k,has_alpha,lane.flags.y!=0u));
        }else if(combined>0.0) {normal=vec4((src.rgb*amount+dst.rgb*dst.a*(1.0-amount))/combined,combined);}
    }
    else {normal=composite(i32(lane.modes.x),dst,src,k);}
    if(lane.flags.y!=0u) {normal.a=dst.a;}
    if(lane.gains.w>0.0 && (params.native.z==0u || params.native.x==0u)) {normal=pigment_paint(normal,dst,src,amount/max(combined,0.000001),lane.gains.w);}
    normal=attributes(normal,dst,src,amount,lane);if(lane.flags.y!=0u) {normal.a=dst.a;}return normal;
}
@compute @workgroup_size(8,8) fn accumulate(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) {return;}let at=vec2<i32>(id.xy+params.rect.xy);
    var cov=textureLoad(in0,at,0);var color=textureLoad(in1,at,0);var wet=textureLoad(in2,at,0);let dab=textureLoad(in3,at,0);let v=dab.r;
    if(v>0.0) {
        let c=cov.r;let ceiling=params.dab.x;var nv=c;if(ceiling>c) {nv=c+v*(ceiling-c);}cov.g=max(cov.g,dab.g);
        if(params.controls.z!=0u) {let incoming=vec4(params.wet.xyz,params.dab.y*dab.g);var weight=0.0;if(nv>0.0) {weight=(nv-c)/nv;}wet+=(incoming-wet)*weight;}
        if(params.controls.x!=0u) {
            if(params.native.z!=0u) {
                var incoming_rgba=params.primary.color;if(params.controls.y!=0u){incoming_rgba=textureLoad(in4,at,0);}
                let col=native_from_rgba(params.native.x,params.native.y!=0u,incoming_rgba).color;
                let incoming=min(nv,ceiling*v);let retained=max(nv-incoming,0.0);var old=vec4(0.0);if(c>0.0001){old=color/c*retained;}
                color=col*incoming+old;
                if(params.controls.w!=0u){
                    let opacity=params.primary.gains.x;var previous=0.0;if(cov.a>0.0001){previous=cov.b/cov.a;}
                    let accumulated=native_to_rgba(params.native.x,true,NativePixel(color,1.0));
                    var value=hsl(clamp(accumulated.rgb*opacity,vec3(0.0),vec3(1.0)));
                    let t=unit(params.primary.attributes.w)*smooth_value(0.0001,0.02,previous);value.y+=(unit(previous)-value.y)*t;
                    color=native_from_rgba(params.native.x,true,vec4(rgb(value)/max(opacity,0.000001),1.0)).color;
                    let source_rgb=native_to_rgba(params.native.x,true,NativePixel(col,1.0)).rgb;
                    let saturation=hsl(clamp(source_rgb,vec3(0.0),vec3(1.0))).y;
                    let weight=unit(incoming*opacity*params.primary.attributes.z);cov.b=cov.b*(1.0-weight)+saturation*weight;cov.a=cov.a*(1.0-weight)+weight;
                }
                cov.r=nv;textureStore(out0,at,cov);textureStore(out1,at,color);textureStore(out2,at,wet);return;
            }
            var col=params.primary.color.rgb;if(params.controls.y!=0u) {col=textureLoad(in4,at,0).rgb;}
            let incoming=min(nv,ceiling*v);let retained=max(nv-incoming,0.0);var old=vec3(0.0);if(c>0.0001) {old=color.rgb/c*retained;}color=vec4(col*incoming+old,params.primary.color.a);
            if(params.controls.w!=0u) {
                let opacity=params.primary.gains.x;var previous=0.0;if(cov.a>0.0001) {previous=cov.b/cov.a;}
                var value=hsl(clamp(color.rgb*opacity,vec3(0.0),vec3(1.0)));let t=unit(params.primary.attributes.w)*smooth_value(0.0001,0.02,previous);value.y+=(unit(previous)-value.y)*t;color=vec4(rgb(value)/max(opacity,0.000001),color.a);
                let saturation=hsl(clamp(col,vec3(0.0),vec3(1.0))).y;let weight=unit(incoming*opacity*params.primary.attributes.z);cov.b=cov.b*(1.0-weight)+saturation*weight;cov.a=cov.a*(1.0-weight)+weight;
            }
        }
        cov.r=nv;
    }
    textureStore(out0,at,cov);textureStore(out1,at,color);textureStore(out2,at,wet);
}
@compute @workgroup_size(8,8) fn source(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) {return;}let at=vec2<i32>(id.xy+params.rect.xy);let cov=textureLoad(in0,at,0);var color=params.primary.color;
    if(params.native.z!=0u){var native=params.native_color;if(params.primary.flags.w!=0u && cov.r>0.0){native=textureLoad(in1,at,0)/cov.r;}color=native_to_rgba(params.native.x,params.native.y!=0u,NativePixel(native,params.primary.color.a));textureStore(out0,at,color);return;}
    if(params.primary.flags.w!=0u && cov.r>0.0) {color=vec4(textureLoad(in1,at,0).rgb/cov.r,color.a);}textureStore(out0,at,color);
}

// Final native write: inputs are native pre/alpha, weighted native source, raw coverage,
// completed RGBA candidate, selection, final coverage, and wet-coloured RGBA source.
// native.w: bit 0 preserves native Normal/Dissolve over, bit 1 selects wet-coloured source.
@compute @workgroup_size(8,8) fn native_finish(@builtin(global_invocation_id) id:vec3<u32>){
    if(any(id.xy>=params.rect.zw)){return;}let at=vec2<i32>(id.xy+params.rect.xy);
    let has_alpha=params.native.y!=0u;let lock=params.primary.flags.y!=0u;
    let dst=NativePixel(textureLoad(in0,at,0),select(1.0,textureLoad(in1,at,0).r,has_alpha));var result=dst;
    let raw=textureLoad(in3,at,0).r;let coverage=textureLoad(in6,at,0).r;let selection=textureLoad(in5,at,0).r;
    var opacity=unit(params.primary.gains.x);if(params.primary.flags.z!=0u){opacity=1.0;}
    var k=min(coverage*opacity*selection,1.0);var changed=k>0.0 && (raw>0.0 || params.primary.flags.z!=0u);
    if(params.primary.modes.x==2u){k=select(0.0,1.0,dissolve_noise(at+params.origin)<k);changed=changed && k>0.0;}
    if(params.primary.flags.x==0u && lock && has_alpha && dst.alpha<=0.0){changed=false;}
    var dither_pixel=changed;
    if(params.controls.z!=0u){
        // CPU pigment output converts every pixel in its 64px coverage tile intersected
        // with stroke bounds, including zero-coverage transparent pixels. Lab/CMYK native
        // black is not the surface's all-zero default pixel, so this checkpoint matters.
        // Input7 is this exact material-region mask for the final pigment pass.
        if(textureLoad(in7,at,0).r>0.0){result=native_from_rgba(params.native.x,has_alpha,textureLoad(in4,at,0));}
        dither_pixel=coverage*selection>0.0;
    }else if(changed){
        if(params.primary.flags.x!=0u){if(has_alpha && !lock){result.alpha*=1.0-k;}}
        else if((params.native.w&1u)!=0u){
            var color=params.native_color;if(params.primary.flags.w!=0u && raw>0.0){color=textureLoad(in2,at,0)/raw;}
            if((params.native.w&2u)!=0u){color=native_from_rgba(params.native.x,has_alpha,textureLoad(in7,at,0)).color;}
            result=native_over(dst,NativePixel(color,params.primary.color.a),k,has_alpha,lock);
        }else{result=native_from_rgba(params.native.x,has_alpha,textureLoad(in4,at,0));}
    }
        // controls[0]=U8, [1]=aliased, [2]=pigment active, matching the final dither stage.
        if(dither_pixel && params.controls.x!=0u){
            if(params.controls.y==0u){let doc=at+params.origin;let key=vec2(bitcast<u32>(doc.y),bitcast<u32>(doc.x));let salt=mul64(vec2(0x74686572u,0x00006469u),vec2(0x963ee407u,0xa24baed4u));let noise=f32(shr64(mix64(key^salt),40u).x)/16777216.0;let delta=(noise-0.5)/255.0;result.color+=vec4(delta);if(has_alpha){result.alpha+=delta;}}
            result.color=floor(clamp(result.color,vec4(0.0),vec4(1.0))*255.0+vec4(0.5))/255.0;result.alpha=floor(clamp(result.alpha,0.0,1.0)*255.0+0.5)/255.0;
        }
    textureStore(out0,at,result.color);textureStore(out1,at,vec4(result.alpha,0.0,0.0,0.0));
}
fn mask_combine(mode: u32,v: f32,t: f32,d: f32) -> f32 {
    if(v<=0.0) {return 0.0;}var r=v;
    switch mode {
        case 0u: {r=v+d*t-v*d*t;}
        case 1u: {r=v*(1.0-d+d*t);}
        case 2u: {r=max(v-d*(1.0-t),0.0);}
        case 3u: {r=min(v,1.0-d+d*t);}
        case 4u: {var o=2.0*v*t;if(v>=0.5) {o=1.0-2.0*(1.0-v)*(1.0-t);}r=v+(o-v)*d;}
        case 5u: {r=v+(min(v/max(1.0-t,1e-3),1.0)-v)*d;}
        case 6u: {r=v+(1.0-min((1.0-v)/max(t,1e-3),1.0)-v)*d;}
        case 7u: {r=v+(max(v+t-1.0,0.0)-v)*d;}
        case 8u: {r=v+(select(0.0,1.0,v+t>1.0)-v)*d;}
        case 9u: {r=v*unit((t-(1.0-d*v))*4.0+0.5);}
        case 10u: {r=select(0.0,v,t>=1.0-d*v);}
        default: {}
    }
    return unit(r);
}
fn dual_blend(mode: u32,primary: f32,secondary: f32) -> f32 {
    let p=unit(primary);let s=unit(secondary);let lifts=mode==0u || mode==3u || mode==4u || mode==7u || mode==8u;
    if(p<=0.001) {return select(0.0,s,lifts);}var alpha=0.0;
    switch mode {
        case 0u: {alpha=p+s-p*s;}
        case 1u: {alpha=p*s;}
        case 3u: {alpha=min(p,s);}
        case 4u: {if(p<0.5) {alpha=2.0*p*s;} else {alpha=1.0-2.0*(1.0-p)*(1.0-s);}}
        case 5u: {alpha=1.0;if(s<1.0) {alpha=min(p/max(1.0-s,0.0001),1.0);}}
        case 6u: {if(s>0.0) {alpha=max(1.0-(1.0-p)/max(s,0.0001),0.0);}}
        case 7u: {alpha=max(p+s-1.0,0.0);}
        case 8u: {alpha=select(0.0,1.0,p+s>=1.0);}
        case 9u: {alpha=p*(0.5+0.5*s);}
        default: {alpha=mask_combine(mode,p,s,1.0);}
    }
    return select(min(alpha,p),alpha,lifts);
}
@compute @workgroup_size(8,8) fn coverage(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) {return;}let at=vec2<i32>(id.xy+params.rect.xy);
    let raw=textureLoad(in0,at,0);let p=textureLoad(in2,at,0).r;let d=textureLoad(in3,at,0).r;var m=0.0;
    if(params.primary.flags.z!=0u) {
        var secondary=d*params.primary.gains.z;if(params.mask.z!=0u) {secondary=textureLoad(in5,at,0).g;}
        m=dual_blend(params.mask.x,p*params.primary.gains.x*params.primary.gains.y,secondary);
    } else {
        m=p*unit(params.primary.gains.y);
        if((params.mask.w&2u)!=0u) {m*=0.5+0.5*smooth_value(0.5,1.0,1.0-raw.g);}
        if((params.mask.w&1u)!=0u) {m=mask_combine(params.mask.x,m,d,unit(params.texture_depth.z*params.primary.gains.z));}
    }
    if(params.mask.y!=0u) {m=textureLoad(in4,at,0).g;}
    textureStore(out0,at,vec4(m,p,d,0.0));
}
// SplitMix64 is represented by low/high u32 words so Dissolve keeps CPU seed and coordinates.
fn add64(a: vec2<u32>,b: vec2<u32>) -> vec2<u32> {let low=a.x+b.x;return vec2(low,a.y+b.y+select(0u,1u,low<a.x));}
fn shr64(a: vec2<u32>,n: u32) -> vec2<u32> {if(n>=32u) {return vec2(a.y>>(n-32u),0u);}return vec2((a.x>>n)|(a.y<<(32u-n)),a.y>>n);}
fn mul64(a: vec2<u32>,b: vec2<u32>) -> vec2<u32> {
    let al=a.x&65535u;let ah=a.x>>16u;let bl=b.x&65535u;let bh=b.x>>16u;
    let p0=al*bl;let p1=al*bh;let p2=ah*bl;let middle=(p0>>16u)+(p1&65535u)+(p2&65535u);
    return vec2((p0&65535u)|(middle<<16u),ah*bh+(p1>>16u)+(p2>>16u)+(middle>>16u)+a.x*b.y+a.y*b.x);
}
fn mix64(input: vec2<u32>) -> vec2<u32> {
    var z=add64(input,vec2(0x7f4a7c15u,0x9e3779b9u));z=mul64(z^shr64(z,30u),vec2(0x1ce4e5b9u,0xbf58476du));z=mul64(z^shr64(z,27u),vec2(0x133111ebu,0x94d049bbu));return z^shr64(z,31u);
}
fn dissolve_noise(at: vec2<i32>) -> f32 {let k=vec2(bitcast<u32>(at.y),bitcast<u32>(at.x));let salt=mul64(params.seed,vec2(0x963ee407u,0xa24baed4u));return f32(shr64(mix64(k^salt),40u).x)/16777216.0;}
@compute @workgroup_size(8,8) fn finish(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) {return;}let at=vec2<i32>(id.xy+params.rect.xy);let dst=textureLoad(in0,at,0);let raw=textureLoad(in1,at,0);let maps=textureLoad(in6,at,0);
    let dual=params.primary.flags.z!=0u;var opacity=unit(params.primary.gains.x);if(dual) {opacity=1.0;}
    var k=min(maps.r*opacity*textureLoad(in5,at,0).r,1.0);
    if(k<=0.0 || (raw.r<=0.0 && !dual)) {textureStore(out0,at,dst);return;}
    if(params.primary.modes.x==2u) {k=select(0.0,1.0,dissolve_noise(at+params.origin)<k);if(k==0.0) {textureStore(out0,at,dst);return;}}
    if(params.primary.flags.x!=0u) {var result=dst;if(params.primary.flags.y==0u) {result.a*=1.0-k;}textureStore(out0,at,result);return;}
    if(params.primary.flags.y!=0u && dst.a<=0.0) {textureStore(out0,at,dst);return;}
    let primary_source=textureLoad(in2,at,0);
    if(params.controls.x!=0u || params.controls.y!=0u) {
        var primary_lane=params.primary;primary_lane.flags.y=0u;
        var primary=primary_source;
        if(params.controls.x!=0u) {primary=attributes(primary_source,dst,primary_source,raw.r*primary_source.a,params.primary);}
        else {primary=lane_composite(dst,primary_source,maps.g*params.primary.gains.x,primary_lane);}
        var output=dst;
        if(dual) {
            let secondary_source=textureLoad(in4,at,0);let secondary_raw=textureLoad(in3,at,0).r;
            var secondary_lane=params.secondary;secondary_lane.flags.y=0u;var secondary=secondary_source;
            if(params.controls.y!=0u) {secondary=attributes(secondary_source,dst,secondary_source,secondary_raw*secondary_source.a,params.secondary);}
            else {secondary=lane_composite(dst,secondary_source,maps.b,secondary_lane);}
            let p=raw.r*params.primary.gains.x*params.primary.gains.y;let d=secondary_raw*params.primary.gains.z;
            let weight=unit(d/max(p+d,0.0001));let union_coverage=p+d-p*d;let factor=unit(maps.r/max(union_coverage,0.0001));
            output=dst+(primary+(secondary-primary)*weight-dst)*factor;
        } else {
            var factor=0.0;if(raw.r>0.0) {factor=unit(maps.r/raw.r);}
            output=dst+(primary-dst)*factor*params.primary.gains.x;
        }
        var result=dst+(output-dst)*textureLoad(in5,at,0).r;
        if(params.primary.flags.y!=0u) {result.a=dst.a;}textureStore(out0,at,result);return;
    }
    var result=lane_composite(dst,primary_source,k,params.primary);
    if(dual && textureLoad(in3,at,0).r>0.0 && params.secondary.flags.w!=0u) {
        let secondary_color=textureLoad(in4,at,0);let secondary=lane_composite(dst,vec4(secondary_color.rgb,primary_source.a),k,params.secondary);
        let p=maps.g*params.primary.gains.x*params.primary.gains.y;let d=maps.b*params.primary.gains.z;let weight=unit(d/max(p+d,0.0001));result+=(secondary-result)*weight;
    }
    textureStore(out0,at,result);
}
@compute @workgroup_size(8,8) fn merge(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) {return;}let at=vec2<i32>(id.xy+params.rect.xy);let a=textureLoad(in0,at,0);let b=textureLoad(in2,at,0);
    let p=textureLoad(in1,at,0).r*params.primary.gains.x*params.primary.gains.y;let d=textureLoad(in3,at,0).r*params.primary.gains.z;let weight=unit(d/max(p+d,0.0001));textureStore(out0,at,a+(b-a)*weight);
}

@compute @workgroup_size(8,8) fn dither(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) {return;}let at=vec2<i32>(id.xy+params.rect.xy);var value=textureLoad(in0,at,0);
    if(params.controls.x==0u) {textureStore(out0,at,value);return;}
    var gate=textureLoad(in1,at,0).r*textureLoad(in5,at,0).r>0.0;
    if(params.controls.z==0u) {
        let dual=params.primary.flags.z!=0u;var opacity=unit(params.primary.gains.x);if(dual) {opacity=1.0;}
        let k=min(textureLoad(in1,at,0).r*opacity*textureLoad(in5,at,0).r,1.0);
        if(params.primary.modes.x==2u && dissolve_noise(at+params.origin)>=k) {gate=false;}
        if(params.primary.flags.x==0u && params.primary.flags.y!=0u && value.a<=0.0) {gate=false;}
    }
    if(gate && params.controls.y==0u) {
        let doc=at+params.origin;let key=vec2(bitcast<u32>(doc.y),bitcast<u32>(doc.x));
        let salt=mul64(vec2(0x74686572u,0x00006469u),vec2(0x963ee407u,0xa24baed4u));
        let noise=f32(shr64(mix64(key^salt),40u).x)/16777216.0;
        value+=vec4((noise-0.5)/255.0);
    }
    textureStore(out0,at,floor(clamp(value,vec4(0.0),vec4(1.0))*255.0+vec4(0.5))/255.0);
}
