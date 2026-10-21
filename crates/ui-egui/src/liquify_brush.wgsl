@group(0) @binding(0) var<storage,read> op:array<vec4<f32>>;
@group(0) @binding(1) var previous:texture_2d_array<f32>;
@group(0) @binding(2) var field:texture_storage_2d_array<rgba32float,write>;
fn node4(p:vec2<i32>)->vec4<f32>{let q=clamp(p,vec2(0),vec2<i32>(op[0].xy)-vec2(1));let size=vec2<i32>(textureDimensions(previous));let page=q/size;let cols=i32(ceil(op[0].x/f32(size.x)));return textureLoad(previous,q%size,page.y*cols+page.x,0);}
fn put(p:vec2<u32>,value:vec4<f32>){let size=textureDimensions(field);let page=p/size;let cols=u32(ceil(op[0].x/f32(size.x)));textureStore(field,vec2<i32>(p%size),i32(page.y*cols+page.x),value);}
fn at(p:vec2<i32>)->vec2<f32>{let size=vec2<i32>(op[0].xy);let q=clamp(p,vec2(0),size-vec2(1));return node4(q).xy;}
fn sample_old(delta:vec2<f32>,g:vec2<i32>)->vec2<f32>{let origin=vec2<f32>(g);let size=op[0].xy;if any(delta<=-origin-vec2(1.))||any(delta>=size-origin){return vec2(0.);}let q=clamp(delta,-origin,size-origin-vec2(1.));let b=vec2<i32>(floor(q))+g;let t=fract(q);return mix(mix(at(b),at(b+vec2(1,0)),t.x),mix(at(b+vec2(0,1)),at(b+vec2(1,1)),t.x),t.y);}

@compute @workgroup_size(8,8) fn main(@builtin(global_invocation_id) gid:vec3<u32>){
    let g=vec2<u32>(op[1].zw)+gid.xy;if any(g>=vec2<u32>(op[4].zw)){return;}
    let index=g.y*u32(op[0].x)+g.x;let old=node4(vec2<i32>(g));var value=old;let tool=u32(op[0].w);let p=vec2<f32>(g-vec2<u32>(op[1].zw))*op[0].z;
    if tool>=11u {
        if tool==11u {value=vec4(old.xy*(1.-clamp(op[4].x/100.,0.,1.)*(1.-old.z)),old.zw);}
        if tool==12u {value.z=1.;}if tool==13u {value.z=0.;}if tool==14u {value.z=1.-old.z;}
        put(g,value);return;
    }
    if tool==10u {
        let count=u32(op[4].y);var inside=false;
        if count>=3u {var a=op[5u+count-1u].xy;for(var i=0u;i<count;i++){let b=op[5u+i].xy;if (a.y>p.y)!=(b.y>p.y){if p.x<(b.x-a.x)*(p.y-a.y)/(b.y-a.y)+a.x {inside=!inside;}}a=b;}}
        if inside {value.z=select(0.,1.,op[4].x>0.5);}put(g,value);return;
    }
    let offset=p-op[2].xy;let radius=max(op[3].x,.5);let t2=dot(offset,offset)/(radius*radius);if t2>=1.{return;}
    let feather=pow(1.-t2,3.);let dense=pow(1.-t2*t2,2.);let raw=op[3].y*mix(feather,dense,op[3].w);
    if tool==8u {value.z=min(1.,old.z+raw);put(g,value);return;}
    if tool==9u {value.z=max(0.,old.z-raw);put(g,value);return;}
    let k=raw*(1.-old.z);let rate=op[3].z;if k<=0.{return;}
    if tool==1u {value=vec4(old.xy*(1.-k*(.1+.4*rate)),old.zw);}
    else if tool==2u {
        let b=vec2<i32>(g);let mean=(at(b+vec2(-1,0))+at(b+vec2(1,0))+at(b+vec2(0,-1))+at(b+vec2(0,1))+at(b+vec2(-1,-1))+at(b+vec2(1,-1))+at(b+vec2(-1,1))+at(b+vec2(1,1)))/8.;
        value=vec4(mix(old.xy,mean,k*(.1+.4*rate)),old.zw);
    } else {
        var delta=vec2(0.);
        if tool==0u {delta=-k*op[2].zw;}
        if tool==7u {delta=k*vec2(-op[2].w,op[2].z);}
        if tool==3u||tool==4u {let angle=-k*rate*.12*select(-1.,1.,tool==3u);let sn=sin(angle);let cm=cos(angle)-1.;delta=vec2(offset.x*cm-offset.y*sn,offset.x*sn+offset.y*cm);}
        if tool==5u {delta=offset*(k*rate*.06);}
        if tool==6u {delta=-offset*(k*rate*.06);}
        value=vec4(delta+sample_old(delta/op[0].z,vec2<i32>(g)),old.zw);
    }
    put(g,value);
}
