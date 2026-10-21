const CMF: array<vec3<f32>, 38> = array<vec3<f32>, 38>(

    vec3<f32>(6.4692e-05, 1.8443e-06, 0.0003050171),
    vec3<f32>(0.0002194099, 6.2053e-06, 0.0010368067),
    vec3<f32>(0.0011205744, 3.10096e-05, 0.005313136),
    vec3<f32>(0.0037666134, 0.0001047484, 0.017954393),
    vec3<f32>(0.011880553, 0.0003536405, 0.057077582),
    vec3<f32>(0.023286443, 0.0009514714, 0.11365162),
    vec3<f32>(0.034559418, 0.0022822632, 0.17335873),
    vec3<f32>(0.03722379, 0.004207329, 0.19620657),
    vec3<f32>(0.032418376, 0.0066887984, 0.18608238),
    vec3<f32>(0.021233205, 0.009888396, 0.13995047),
    vec3<f32>(0.010490991, 0.015249452, 0.08917453),
    vec3<f32>(0.0032958376, 0.02141831, 0.04789621),
    vec3<f32>(0.0005070352, 0.033422932, 0.028145626),
    vec3<f32>(0.0009486742, 0.051310014, 0.016137662),
    vec3<f32>(0.006273718, 0.070402084, 0.0077591019),
    vec3<f32>(0.016864624, 0.08783871, 0.004296148),
    vec3<f32>(0.028689649, 0.094249054, 0.002005509),
    vec3<f32>(0.042674812, 0.09795667, 0.0008614711),
    vec3<f32>(0.05625475, 0.09415218, 0.0003690387),
    vec3<f32>(0.0694704, 0.086781024, 0.0001914287),
    vec3<f32>(0.08305315, 0.078856534, 0.0001495556),
    vec3<f32>(0.0861261, 0.063526703, 9.23109e-05),
    vec3<f32>(0.090466134, 0.053741417, 6.81349e-05),
    vec3<f32>(0.08500387, 0.042646064, 2.88264e-05),
    vec3<f32>(0.07090667, 0.03161735, 1.57672e-05),
    vec3<f32>(0.050628892, 0.020885206, 3.9406e-06),
    vec3<f32>(0.03547396, 0.01386011, 1.584e-06),
    vec3<f32>(0.021468211, 0.0081026402, 0.0),
    vec3<f32>(0.012516457, 0.0046301023, 0.0),
    vec3<f32>(0.0068045816, 0.00249138, 0.0),
    vec3<f32>(0.0034645658, 0.0012593034, 0.0),
    vec3<f32>(0.0014976098, 0.0005416465, 0.0),
    vec3<f32>(0.0007697005, 0.0002779529, 0.0),
    vec3<f32>(0.0004073681, 0.0001471081, 0.0),
    vec3<f32>(0.0001690104, 6.10327e-05, 0.0),
    vec3<f32>(9.52245e-05, 3.43873e-05, 0.0),
    vec3<f32>(4.9031e-05, 1.7706e-05, 0.0),
    vec3<f32>(2e-05, 7.221e-06, 0.0),
);
fn unit(v: f32) -> f32 { return clamp(v, 0.0, 1.0); }
fn smooth_value(a: f32, b: f32, v: f32) -> f32 { let t = unit((v-a)/(b-a)); return t*t*(3.0-2.0*t); }
fn lerp3(a: vec3<f32>, b: vec3<f32>, t: f32) -> vec3<f32> { return a+(b-a)*t; }
fn norm(a: vec3<f32>) -> vec3<f32> { return a/max(sqrt(dot(a,a)),1e-12); }
struct Spectrum { ks: array<f32,38>, luminance: f32 }
fn lookup(rgb: vec3<f32>) -> Spectrum {
    let xyz=clamp(rgb,vec3(0.0),vec3(1.0))*31.0;
    let lo=vec3<u32>(floor(xyz)); let f=xyz-vec3<f32>(lo);
    var channels: array<f32,40>;
    for(var band=0u;band<10u;band++) { for(var dz=0u;dz<2u;dz++) { for(var dy=0u;dy<2u;dy++) { for(var dx=0u;dx<2u;dx++) {
        let x=min(lo.x+dx,31u); let y=min(lo.y+dy,31u); let z=min(lo.z+dz,31u)+band*32u;
        let w=select(1.0-f.x,f.x,dx!=0u)*select(1.0-f.y,f.y,dy!=0u)*select(1.0-f.z,f.z,dz!=0u);
        let offset=((z*32u+y)*32u+x)*4u;
        for(var ch=0u;ch<4u;ch++) { channels[band*4u+ch]+=lut[offset+ch]*w; }
    } } } }
    var result: Spectrum;
    for(var i=0u;i<38u;i++) { result.ks[i]=max(exp2(channels[i])-1.0,0.0); }
    result.luminance=unit(channels[38]); return result;
}
fn pigment_mix(from_color: vec3<f32>, to: vec3<f32>, t: f32) -> vec3<f32> {
    if(t<=0.000001) { return from_color; } if(t>=0.999999) { return to; }
    let a=lookup(from_color); let b=lookup(to);
    let wa=(1.0-t)*(1.0-t)*a.luminance; let wb=t*t*b.luminance; let sum=wa+wb;
    if(sum<=0.000001) { return select(to,from_color,t<0.5); }
    var xyz=vec3(0.0);
    for(var i=0u;i<38u;i++) { let ks=(a.ks[i]*wa+b.ks[i]*wb)/sum; let reflectance=1.0/(1.0+ks+sqrt(ks*ks+2.0*ks)); xyz+=reflectance*CMF[i]; }
    let linear=vec3(3.24097*xyz.x-1.5373832*xyz.y-0.49861076*xyz.z,-0.96924365*xyz.x+1.8759675*xyz.y+0.04155506*xyz.z,0.05563008*xyz.x-0.20397696*xyz.y+1.0569715*xyz.z);
    var result=vec3(0.0);
    for(var i=0u;i<3u;i++) { if(linear[i]<0.0031308) { result[i]=max(linear[i],0.0)*12.92; } else { result[i]=1.055*pow(linear[i],1.0/2.4)-0.055; } }
    return clamp(result,vec3(0.0),vec3(1.0));
}

fn pigment_paint(normal: vec4<f32>, dst: vec4<f32>, src: vec4<f32>, amount: f32, strength: f32) -> vec4<f32> {
    let support=smooth_value(0.02,0.22,dst.a); let weight=unit(strength)*support;
    if(weight<=0.0001) {return normal;}
    let base=lerp3(normal.rgb,dst.rgb,support);let t=unit(amount);let pigment=pigment_mix(base,src.rgb,t);
    let linear=lerp3(base,src.rgb,t);let mixed=lerp3(linear,pigment,smooth_value(0.025,0.16,t));
    return vec4(lerp3(normal.rgb,mixed,weight),normal.a);
}
