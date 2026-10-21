struct State { material: vec4<f32>, highlight: vec4<f32>, motion: vec4<f32> }
struct Params { rect: vec4<u32>, blend: vec4<f32>, state: State }
@group(0) @binding(0) var normal_map: texture_2d<f32>;
@group(0) @binding(1) var destination_map: texture_2d<f32>;
@group(0) @binding(2) var source_map: texture_2d<f32>;
@group(0) @binding(3) var material_map: texture_2d<f32>;
@group(0) @binding(4) var candidate_map: texture_2d<f32>;
@group(0) @binding(5) var coverage_map: texture_2d<f32>;
@group(0) @binding(6) var output_map: texture_storage_2d<rgba32float, write>;
@group(0) @binding(7) var<uniform> params: Params;
@group(0) @binding(8) var<storage, read> lut: array<f32>;
@group(0) @binding(9) var<storage, read_write> states: array<State>;
@compute @workgroup_size(8,8) fn paint(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) { return; } let at=vec2<i32>(id.xy+params.rect.xy);
    let normal=textureLoad(normal_map,at,0); let dst=textureLoad(destination_map,at,0); let src=textureLoad(source_map,at,0);
    let amount=textureLoad(coverage_map,at,0).r*params.blend.x;
    let result=pigment_paint(normal,dst,src,amount,params.blend.y);
    textureStore(output_map,at,result);
}
@compute @workgroup_size(8,8) fn accumulate(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) { return; } let pos=id.xy+params.rect.xy; let at=vec2<i32>(pos);
    let previous=textureLoad(normal_map,at,0); let coverage=textureLoad(destination_map,at,0).r;
    var result=previous;
    if(params.state.material.x!=0.0 && params.blend.z>0.0 && coverage>0.0) {
        let emboss=unit(textureLoad(source_map,at,0).r); let flow=unit(params.blend.z); let alpha=unit(coverage*flow*unit(params.blend.w));
        let deposit=vec4(alpha*clamp(params.state.material.z,-1.0,1.0),alpha*unit(params.state.material.w)*emboss*(0.35+0.65*flow),0.0,alpha);
        result=deposit+previous*(1.0-unit(deposit.a));
        states[pos.y*textureDimensions(normal_map).x+pos.x]=params.state;
    }
    textureStore(output_map,at,result);
}
@compute @workgroup_size(8,8) fn prepare(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) { return; } let at=vec2<i32>(id.xy+params.rect.xy);
    let raw=textureLoad(normal_map,at,0); let coverage=unit(textureLoad(coverage_map,at,0).r);
    var result=vec4(0.0,0.0,0.0,coverage); if(raw.a>0.0) { result=raw*coverage/raw.a; }
    textureStore(output_map,at,result);
}
fn read_map(map: texture_2d<f32>, at: vec2<i32>) -> vec4<f32> {
    if(any(at<vec2(0)) || any(at>=vec2<i32>(textureDimensions(map)))) { return vec4(0.0); }
    return textureLoad(map,at,0);
}
fn bilinear(map: texture_2d<f32>, at: vec2<f32>) -> vec4<f32> {
    let p=vec2<i32>(floor(at)); let f=at-floor(at);
    let a=read_map(map,p); let b=read_map(map,p+vec2(1,0)); let c=read_map(map,p+vec2(0,1)); let d=read_map(map,p+vec2(1,1));
    return (a+(b-a)*f.x)*(1.0-f.y)+(c+(d-c)*f.x)*f.y;
}
fn material_input(at: vec2<f32>, motion: vec2<f32>, liquidity: f32) -> vec4<f32> {
    let q=at-motion*liquidity; let center=bilinear(material_map,q); if(liquidity<=0.0) { return center; }
    let spread=0.5+4.0*liquidity; var neighbors=vec4(0.0);
    let offsets=array<vec2<f32>,4>(vec2(spread,0.0),vec2(-spread,0.0),vec2(0.0,spread),vec2(0.0,-spread));
    for(var i=0u;i<4u;i++) { neighbors+=bilinear(material_map,q+offsets[i])*0.25; }
    return center+(neighbors-center)*liquidity*0.85;
}
fn height(at: vec2<f32>, motion: vec2<f32>, liquidity: f32) -> f32 { let v=material_input(at,motion,liquidity); return v.r+v.g*0.85; }
fn specular(light: vec3<f32>, normal: vec3<f32>, width: f32) -> f32 {
    let half_vector=norm(light+vec3(0.0,0.0,1.0)); let nh=unit(dot(normal,half_vector)); let nl=unit(dot(normal,light)); let nv=unit(normal.z); let lh=unit(dot(light,half_vector));
    let a2=0.075*0.075; var denominator=nh*nh*(a2-1.0)+1.0;
    if(width!=1.0) { denominator=a2+(denominator-a2)/max(width*width,1e-6); }
    let distribution=a2/(3.141592653589793*denominator*denominator);
    let geometry=1.0/((nl+sqrt(a2+(1.0-a2)*nl*nl))*(nv+sqrt(a2+(1.0-a2)*nv*nv)));
    let f=1.0-lh; return distribution*geometry*(0.05+0.95*f*f*f*f*f);
}
@compute @workgroup_size(8,8) fn shade(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) { return; } let pos=id.xy+params.rect.xy; let pixel=vec2<i32>(pos); let at=vec2<f32>(pos);
    let old=textureLoad(normal_map,pixel,0); let p=states[pos.y*textureDimensions(normal_map).x+pos.x];
    if(p.material.x==0.0 || textureLoad(coverage_map,pixel,0).r<=0.0) { textureStore(output_map,pixel,old); return; }
    let liquidity=unit(p.highlight.x); let pressure=unit(p.material.y); let motion=p.motion.xy;
    let deposit=material_input(at,motion,liquidity); let local=bilinear(material_map,at-motion*liquidity);
    if(local.a<=0.0) { textureStore(output_map,pixel,old); return; }
    let tl=height(at+vec2(-1.0,-1.0),motion,liquidity); let tr=height(at+vec2(1.0,-1.0),motion,liquidity);
    let bl=height(at+vec2(-1.0,1.0),motion,liquidity); let br=height(at+vec2(1.0,1.0),motion,liquidity);
    let l=height(at+vec2(-1.0,0.0),motion,liquidity); let r=height(at+vec2(1.0,0.0),motion,liquidity);
    let t=height(at+vec2(0.0,-1.0),motion,liquidity); let b=height(at+vec2(0.0,1.0),motion,liquidity);
    let slope=vec2(tl-tr+2.0*(l-r)+bl-br,tl-bl+2.0*(t-b)+tr-br);
    let normal=norm(vec3(-slope*32.0*(1.0-0.55*pressure),7.0)); let light=norm(vec3(0.0,1.0,1.0)); let flat=vec3(0.0,0.0,1.0);
    var highlight=0.0;
    if(p.highlight.y>0.0) {
        highlight=(specular(light,normal,1.0)-specular(light,flat,1.0))*0.5;
        let width=unit(p.highlight.w)*5.0;
        if(width!=1.0) { let widened=(specular(light,normal,width)-specular(light,flat,width))*0.5;
            if(width>1.0 && widened>0.0) { highlight=max(highlight,widened); }
            if(width<1.0 && highlight>0.0) { highlight=min(highlight,max(widened,0.0)); }
        }
        highlight*=unit(p.highlight.y);
    }
    let pit=deposit.b/max(deposit.a,1e-6);
    let shadow=(unit(dot(normal,light))-light.z)*0.15-min(length(slope)*0.12*(1.0+pressure)+pit*deposit.r*0.06,0.12);
    var rgb=old.rgb;
    if(liquidity>0.0) {
        var carried=rgb*old.a; var weight=old.a;
        for(var i=1u;i<=3u;i++) { let q=at-motion*liquidity*f32(i)/3.0; let neighbor=bilinear(candidate_map,q); let w=neighbor.a*bilinear(material_map,q).a; carried+=neighbor.rgb*w; weight+=w; }
        rgb=lerp3(rgb,carried/max(weight,1e-6),liquidity*0.8);
    }
    let texture_amount=local.g/max(local.a,1e-6);
    if(texture_amount>0.0) {
        var mean=vec3(0.0); var weights=0.0; var coverage_mean=0.0; var texture_mean=0.0; var min_coverage=local.a;
        let offsets=array<vec2<f32>,4>(vec2(-2.0,0.0),vec2(2.0,0.0),vec2(0.0,-2.0),vec2(0.0,2.0));
        for(var i=0u;i<4u;i++) { let q=at+offsets[i]; let neighbor=bilinear(candidate_map,q); let covered=bilinear(material_map,q); let a=covered.a;
            texture_mean+=covered.g/max(a,1e-6)*0.25; min_coverage=min(min_coverage,a); coverage_mean+=a*0.25; let w=neighbor.a*a; mean+=neighbor.rgb*w; weights+=w;
        }
        if(weights>0.0) { mean/=weights; } else { mean=rgb; }
        let difference=old.rgb-mean; let brightness=dot(difference,vec3(0.2126,0.7152,0.0722));
        let detail=(local.a-coverage_mean)/max(local.a+coverage_mean,0.01)+(texture_amount-texture_mean)/max(texture_amount+texture_mean,0.01);
        let structure=smooth_value(0.015,0.15,abs(detail)); let chroma=length(difference-vec3(brightness));
        let contrast=clamp(detail*1.55+brightness*structure*1.1+sign(detail)*chroma*structure*0.45,-0.52,0.42)*max(texture_amount,0.32)*smooth_value(0.02,0.3,min_coverage)*(1.0-liquidity*0.85);
        let tint=norm(max(old.rgb,vec3(0.001)))*contrast*0.18;
        if(contrast<0.0) { rgb*=1.0+contrast; } else { rgb=clamp(rgb+(vec3(1.0)-rgb)*contrast+tint,vec3(0.0),vec3(1.0)); }
    }
    let lit=clamp(rgb+rgb*shadow+vec3(highlight),vec3(0.0),vec3(1.0));
    let pigment=clamp(rgb+rgb*shadow+vec3(min(highlight,0.0)),vec3(0.0),vec3(1.0));
    let shaded=lerp3(pigment,lit,unit(p.highlight.z)); let contribution=unit(local.a/max(old.a,1e-6));
    textureStore(output_map,pixel,vec4(lerp3(old.rgb,shaded,contribution),old.a));
}

/// Same compass order and arithmetic as pigment_material::emboss_height. Raster callers can
/// use this with their fractional hard-tip evaluator (before grain, wet edges and flow).
fn emboss_height(mask: f32, local: vec2<f32>, pattern: f32, near: array<f32,8>, far: array<f32,8>) -> f32 {
    if(mask<=0.0001) { return 0.0; }
    var edge_sum=0.0; var inner_sum=0.0; var outer_sum=0.0;
    for(var i=0u;i<8u;i++) { edge_sum+=abs(mask-near[i]); inner_sum+=near[i]; outer_sum+=far[i]; }
    let edge=unit(edge_sum*0.22); let inner=inner_sum*0.125; let outer=outer_sum*0.125;
    let interior=smooth_value(0.015,0.84,mask); let rim=smooth_value(0.02,0.42,abs(inner-outer))*smooth_value(0.015,0.65,mask); let gap=smooth_value(0.12,0.78,1.0-pattern);
    let x=local.x; let y=local.y;
    let layers=0.28*smooth_value(0.48,0.56,fract(x*8.0+y*1.3))+0.24*smooth_value(0.46,0.55,fract(x*13.0-y*2.1+0.31))+0.26*smooth_value(0.47,0.57,fract(y*10.0+x*1.7+0.63))+0.22*smooth_value(0.45,0.56,fract((x+y)*18.0+0.17));
    return unit(interior*0.34+edge*0.18+rim*0.24+(1.0-gap*layers)*0.24);
}

struct EmbossSamples { local_pattern_mask: vec4<f32>, near_a: vec4<f32>, near_b: vec4<f32>, far_a: vec4<f32>, far_b: vec4<f32> }
@group(0) @binding(10) var<storage, read> emboss_samples: array<EmbossSamples>;
@compute @workgroup_size(8,8) fn emboss(@builtin(global_invocation_id) id: vec3<u32>) {
    if(any(id.xy>=params.rect.zw)) { return; }
    let pos=id.xy+params.rect.xy; let s=emboss_samples[pos.y*textureDimensions(normal_map).x+pos.x];
    let near=array<f32,8>(s.near_a.x,s.near_a.y,s.near_a.z,s.near_a.w,s.near_b.x,s.near_b.y,s.near_b.z,s.near_b.w);
    let far=array<f32,8>(s.far_a.x,s.far_a.y,s.far_a.z,s.far_a.w,s.far_b.x,s.far_b.y,s.far_b.z,s.far_b.w);
    textureStore(output_map,vec2<i32>(pos),vec4(emboss_height(s.local_pattern_mask.w,s.local_pattern_mask.xy,s.local_pattern_mask.z,near,far),0.0,0.0,0.0));
}
