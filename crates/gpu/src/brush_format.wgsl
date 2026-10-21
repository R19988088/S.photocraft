// Slots 0/1 describe exactly the CMYK -> RGB / RGB -> CMYK fast transforms.
// Callers provide these two storage bindings before including this module.
struct NativePixel { color: vec4<f32>, alpha: f32 }
fn cms_node(offset:u32, channels:u32) -> vec4<f32> {
 var v=vec4(0.0); for(var k=0u;k<channels;k++){v[k]=cms_values[offset+k];} return v;
}
fn cms_tetra(base:u32,s:vec3<u32>,r:vec3<f32>,channels:u32)->vec4<f32>{
 var p1=0u; var p2=0u; var w=vec3(0.0);
 if(r.x>=r.y){if(r.y>=r.z){p1=s.x;p2=s.x+s.y;w=r;}else if(r.x>=r.z){p1=s.x;p2=s.x+s.z;w=r.xzy;}else{p1=s.z;p2=s.x+s.z;w=r.zxy;}}
 else if(r.x>=r.z){p1=s.y;p2=s.x+s.y;w=r.yxz;}else if(r.y>=r.z){p1=s.y;p2=s.y+s.z;w=r.yzx;}else{p1=s.z;p2=s.y+s.z;w=r.zyx;}
 let a=cms_node(base,channels);let b=cms_node(base+p1,channels);let c=cms_node(base+p2,channels);let d=cms_node(base+s.x+s.y+s.z,channels);
 return a+(b-a)*w.x+(c-b)*w.y+(d-c)*w.z;
}
fn cms_eval(slot:u32,input:vec4<f32>)->vec4<f32>{
 let h=slot*16u;let ni=cms_info[h];let no=cms_info[h+1u];var base=cms_info[h+10u];var r=vec4(0.0);var s=vec4(0u);
 for(var i=0u;i<ni;i++){let n=cms_info[h+2u+i];let pos=clamp(input[i],0.0,1.0)*f32(n-1u);let j=min(u32(pos),n-2u);s[i]=cms_info[h+6u+i];base+=j*s[i];r[i]=pos-f32(j);}
 var v=vec4(0.0);
 if(ni==4u){let lo=cms_tetra(base,s.yzw,r.yzw,no);v=lo;if(r.x>0.0 && s.x>0u){let hi=cms_tetra(base+s.x,s.yzw,r.yzw,no);v=lo+(hi-lo)*r.x;}}
 else{v=cms_tetra(base,s.xyz,r.xyz,no);}
 let length=cms_info[h+12u];if(length>0u){for(var k=0u;k<no;k++){let u=sqrt(sqrt(clamp(v[k],0.0,1.0)))*f32(length-1u);let i=min(u32(u),length-2u);let off=cms_info[h+11u]+k*length+i;let a=cms_values[off];v[k]=a+(cms_values[off+1u]-a)*(u-f32(i));}}
 return v;
}
fn format_linear(v:f32)->f32{if(v<=0.04045){return v/12.92;}return pow((v+0.055)/1.055,2.4);}
fn format_encoded(v:f32)->f32{if(v<=0.0031308){return v*12.92;}return 1.055*pow(v,1.0/2.4)-0.055;}
fn format_f(v:f32)->f32{let t=6.0/29.0;if(v>t*t*t){return pow(v,1.0/3.0);}return v/(3.0*t*t)+4.0/29.0;}
fn format_finv(v:f32)->f32{let t=6.0/29.0;if(v>t){return v*v*v;}return 3.0*t*t*(v-4.0/29.0);}
fn format_lab_from_rgb(rgb:vec3<f32>)->vec3<f32>{
 let v=vec3(format_linear(rgb.r),format_linear(rgb.g),format_linear(rgb.b));
 let xyz=vec3(0.436074*v.r+0.385064*v.g+0.143080*v.b,0.222504*v.r+0.716878*v.g+0.060618*v.b,0.013932*v.r+0.097104*v.g+0.714173*v.b);
 let f=vec3(format_f(xyz.x/0.96422),format_f(xyz.y),format_f(xyz.z/0.82521));
 return vec3((116.0*f.y-16.0)/100.0,(500.0*(f.x-f.y)+128.0)/255.0,(200.0*(f.y-f.z)+128.0)/255.0);
}
fn format_lab_to_rgb(c:vec3<f32>)->vec3<f32>{
 let fy=(c.x*100.0+16.0)/116.0;let fx=fy+(c.y*255.0-128.0)/500.0;let fz=fy-(c.z*255.0-128.0)/200.0;
 let v=vec3(format_finv(fx)*0.96422,format_finv(fy),format_finv(fz)*0.82521);
 let lin=clamp(vec3(3.133856*v.x-1.616867*v.y-0.490615*v.z,-0.978768*v.x+1.916142*v.y+0.033454*v.z,0.071945*v.x-0.228991*v.y+1.405243*v.z),vec3(0.0),vec3(1.0));
 return vec3(format_encoded(lin.x),format_encoded(lin.y),format_encoded(lin.z));
}
// Mode: 0 RGB-like, 1 gray-like, 2 CMYK, 3 Lab. Alpha is stored separately,
// including CMYKA's fifth channel; native ink values never pass through RGB implicitly.
fn native_from_rgba(mode:u32,has_alpha:bool,rgba:vec4<f32>)->NativePixel{
 var c=vec4(rgba.rgb,0.0);switch mode{case 1u:{c=vec4(0.299*rgba.r+0.587*rgba.g+0.114*rgba.b,0.0,0.0,0.0);}case 2u:{c=cms_eval(1u,vec4(clamp(rgba.rgb,vec3(0.0),vec3(1.0)),0.0));}case 3u:{c=vec4(format_lab_from_rgb(rgba.rgb),0.0);}default:{}}
 return NativePixel(c,select(1.0,rgba.a,has_alpha));
}
fn native_to_rgba(mode:u32,has_alpha:bool,pixel:NativePixel)->vec4<f32>{
 var rgb=pixel.color.rgb;switch mode{case 1u:{rgb=vec3(pixel.color.r);}case 2u:{rgb=cms_eval(0u,pixel.color).rgb;}case 3u:{rgb=format_lab_to_rgb(pixel.color.rgb);}default:{}}
 return vec4(rgb,select(1.0,pixel.alpha,has_alpha));
}
fn native_quantize(v:f32,sample:u32)->f32{if(sample==0u){return floor(clamp(v,0.0,1.0)*255.0+0.5)/255.0;}if(sample==1u){return floor(clamp(v,0.0,1.0)*65535.0+0.5)/65535.0;}return v;}
fn native_over(destination:NativePixel,source:NativePixel,coverage:f32,has_alpha:bool,lock:bool)->NativePixel{
 let amount=coverage*select(1.0,source.alpha,has_alpha);var result=destination;
 if(!has_alpha){result.color=destination.color+(source.color-destination.color)*coverage;return result;}
 let alpha=amount+destination.alpha*(1.0-amount);if(!lock){result.alpha=alpha;}
 if(alpha>0.0){result.color=(source.color*amount+destination.color*destination.alpha*(1.0-amount))/alpha;}
 return result;
}
