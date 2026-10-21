struct Params { w:u32, h:u32, kind:u32, reach:u32 };
@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var shape: texture_2d<f32>;
@group(0) @binding(2) var<storage, read_write> a: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> b: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read_write> envelope: array<vec2<f32>>;
@group(0) @binding(5) var<storage, read_write> sharp0: array<vec4<f32>>;
@group(0) @binding(6) var<storage, read_write> sharp1: array<vec4<f32>>;
@group(0) @binding(7) var output: texture_storage_2d<r32float, write>;
const EPS:f32 = 0.00196078431372549;
const INF:f32 = 1e20;
fn index(q:vec2<i32>)->u32 { return u32(q.y)*p.w+u32(q.x); }
fn valid(q:vec2<i32>)->bool { return all(q>=vec2(0)) && q.x<i32(p.w) && q.y<i32(p.h); }
fn alpha(q:vec2<i32>)->f32 { if !valid(q) {return 0.0;} return textureLoad(shape,q,0).r; }
fn coverage(q:vec2<i32>)->f32 {
    let x=alpha(q); if x<=EPS {return x;}
    var m=x;
    for(var y=-1;y<=1;y++){for(var xx=-1;xx<=1;xx++){m=max(m,alpha(q+vec2(xx,y)));}}
    return min(x/m,1.0);
}
fn cov_clamp(q:vec2<i32>)->f32 {return coverage(clamp(q,vec2(0),vec2(i32(p.w)-1,i32(p.h)-1)));}
fn value(q:vec2<i32>)->f32 {
    if p.kind==5u {return 1.0-alpha(q);}
    if p.kind==8u || p.kind==9u {return coverage(q);}
    return alpha(q);
}
fn is_chamfer()->bool {return p.kind==4u || p.kind==5u || p.kind==6u || p.kind==9u;}
fn is_inside()->bool {return p.kind==1u || p.kind==3u || p.kind==6u;}
@compute @workgroup_size(8,8)
fn seed(@builtin(global_invocation_id) id:vec3<u32>){
    if id.x>=p.w || id.y>=p.h {return;}
    let q=vec2<i32>(id.xy);let v=value(q);
    var d=INF;
    if is_chamfer(){
        if p.kind==6u {
            if v<=EPS {d=0.0;} else {let c=coverage(q);if c<1.0-EPS {d=c;}}
        } else if v>EPS {d=1.0-min(v,1.0);}
    }else if (is_inside() && v<=EPS) || (!is_inside() && v>EPS) {d=0.0;}
    a[index(q)]=vec4(vec2<f32>(q),d,0.0);
}
fn at(line:u32,pos:u32,vertical:bool)->u32 {if vertical {return pos*p.w+line;}return line*p.w+pos;}
fn distance_line(line:u32,vertical:bool){
    let n=select(p.w,p.h,vertical);let base=line*(n+1u);
    var k=0u;
    envelope[base]=vec2(0.0,-INF);envelope[base+1u]=vec2(0.0,INF);
    for(var q=1u;q<n;q++){
        loop {
            let v=u32(envelope[base+k].x);
            let s=((a[at(line,q,vertical)].z+f32(q)*f32(q))-(a[at(line,v,vertical)].z+f32(v)*f32(v)))/(2.0*f32(q-v));
            if s<=envelope[base+k].y && k>0u {k--;continue;}
            if s<=envelope[base+k].y {
                envelope[base]=vec2(f32(q),-INF);envelope[base+1u]=vec2(0.0,INF);break;
            }
            k++;envelope[base+k]=vec2(f32(q),s);envelope[base+k+1u]=vec2(0.0,INF);break;
        }
    }
    k=0u;
    for(var q=0u;q<n;q++){
        loop {if envelope[base+k+1u].y>=f32(q) {break;}k++;}
        let v=u32(envelope[base+k].x);let src=a[at(line,v,vertical)];let delta=f32(q)-f32(v);
        b[at(line,q,vertical)]=vec4(src.xy,delta*delta+src.z,0.0);
    }
}
@compute @workgroup_size(64)
fn columns(@builtin(global_invocation_id) id:vec3<u32>){if id.x<p.w {distance_line(id.x,true);}}
@compute @workgroup_size(64)
fn rows(@builtin(global_invocation_id) id:vec3<u32>){if id.x<p.h {distance_line(id.x,false);}}
@compute @workgroup_size(8,8)
fn relax(@builtin(global_invocation_id) id:vec3<u32>){
    if id.x>=p.w || id.y>=p.h {return;}
    let q=vec2<i32>(id.xy);var best=a[index(q)];
    for(var y=-2;y<=2;y++){for(var x=-2;x<=2;x++){
        let step=vec2(x,y);let v=q+step;
        if !valid(v) || all(step==vec2(0)) || (abs(x)==2 && abs(y)!=1) || (abs(y)==2 && abs(x)!=1) {continue;}
        let src=a[index(v)];let d=src.z+length(vec2<f32>(step));if d<best.z {best=vec4(src.xy,d,0.0);}
    }}
    b[index(q)]=best;
}
fn edge_offset(n:vec2<f32>,v:f32)->f32 {
    let u=max(abs(n.x),abs(n.y));let vv=min(abs(n.x),abs(n.y));
    let inside=v>0.5;let c=clamp(select(v,1.0-v,inside),0.0,0.5);
    var corner=0.0;if u>0.0 {corner=vv/(2.0*u);}
    var r=0.5*(u+vv)-sqrt(2.0*u*vv*c);if c>=corner {r=(0.5-c)*u;}
    return select(-r,r,inside);
}
@compute @workgroup_size(8,8)
fn edges(@builtin(global_invocation_id) id:vec3<u32>){
    if id.x>=p.w || id.y>=p.h {return;}
    let q=vec2<i32>(id.xy);let i=index(q);let v=alpha(q);let c=coverage(q);sharp0[i]=vec4(0.0);
    if v<=EPS || c>=1.0-EPS {return;}
    var empty=false;var full=false;
    for(var y=-1;y<=1;y++){for(var x=-1;x<=1;x++){
        let z=q+vec2(x,y);if !valid(z) || (x==0 && y==0) {continue;}
        empty=empty || alpha(z)<=EPS;full=full || coverage(z)>=1.0-EPS;
    }}
    if !empty || !full {return;}
    let gx=(cov_clamp(q+vec2(1,-1))+2.0*cov_clamp(q+vec2(1,0))+cov_clamp(q+vec2(1,1)))-(cov_clamp(q+vec2(-1,-1))+2.0*cov_clamp(q+vec2(-1,0))+cov_clamp(q+vec2(-1,1)));
    let gy=(cov_clamp(q+vec2(-1,1))+2.0*cov_clamp(q+vec2(0,1))+cov_clamp(q+vec2(1,1)))-(cov_clamp(q+vec2(-1,-1))+2.0*cov_clamp(q+vec2(0,-1))+cov_clamp(q+vec2(1,-1)));
    let len=length(vec2(gx,gy));var normal=vec2(0.0);var off=c-0.5;
    if len>1e-6 {normal=vec2(gx,gy)/len;off=edge_offset(normal,c);}
    sharp0[i]=vec4(normal,off,c);
}
@compute @workgroup_size(8,8)
fn refine(@builtin(global_invocation_id) id:vec3<u32>){
    if id.x>=p.w || id.y>=p.h {return;}
    let q=vec2<i32>(id.xy);let i=index(q);let e=sharp0[i];sharp1[i]=e;if e.w<=0.0 {return;}
    var sum=vec2(0.0);var n=0.0;
    for(var y=-3;y<=3;y++){for(var x=-3;x<=3;x++){
        let z=q+vec2(x,y);if x*x+y*y>9 || !valid(z) {continue;}let o=sharp0[index(z)];
        if o.w>0.0 && dot(o.xy,e.xy)>0.9 {sum+=vec2<f32>(z)-o.z*o.xy;n+=1.0;}
    }}
    if n<3.0 {return;}let mean=sum/n;
    var sxx=0.0;var sxy=0.0;var syy=0.0;
    for(var y=-3;y<=3;y++){for(var x=-3;x<=3;x++){
        let z=q+vec2(x,y);if x*x+y*y>9 || !valid(z) {continue;}let o=sharp0[index(z)];
        if o.w>0.0 && dot(o.xy,e.xy)>0.9 {let d=vec2<f32>(z)-o.z*o.xy-mean;sxx+=d.x*d.x;sxy+=d.x*d.y;syy+=d.y*d.y;}
    }}
    let half=0.5*(sxx+syy);let root=sqrt(0.25*(sxx-syy)*(sxx-syy)+sxy*sxy);
    let along=sqrt(max(0.0,(half+root)/n));let across=sqrt(max(0.0,(half-root)/n));if along<=0.7 || across>=0.1 {return;}
    let angle=0.5*atan2(2.0*sxy,sxx-syy);var normal=vec2(-sin(angle),cos(angle));if dot(normal,e.xy)<0.0 {normal=-normal;}
    sharp1[i]=vec4(normal,edge_offset(normal,e.w),e.w);
}
fn sharp(q:vec2<i32>)->vec4<f32>{if !valid(q){return vec4(0.0);}return sharp1[index(q)];}
fn edge_distance(e:vec4<f32>,d:vec2<f32>,inside:bool)->f32 {
    if all(e.xy==vec2(0.0)) {return INF;}
    let signed=e.z+dot(d,e.xy);let across=select(-signed,signed,inside);if across<=0.0 {return INF;}
    let along=abs(dot(d,vec2(-e.y,e.x)));return length(vec2(across,max(0.0,along-0.75)));
}
fn round_away(v:vec2<f32>)->vec2<i32>{return vec2<i32>(sign(v)*floor(abs(v)+0.5));}
fn measured_edge(q:vec2<i32>,near:vec2<i32>,inside:bool,refine_foot:bool)->f32 {
    let e=sharp(near);if e.w<=0.0 {return INF;}let d=vec2<f32>(q-near);var best=edge_distance(e,d,inside);if best>=INF || !refine_foot {return best;}
    let signed=e.z+dot(d,e.xy);let foot=round_away(vec2<f32>(q)-signed*e.xy);
    for(var y=-2;y<=2;y++){for(var x=-2;x<=2;x++){let z=foot+vec2(x,y);let e2=sharp(z);if e2.w>0.0 {best=min(best,edge_distance(e2,vec2<f32>(q-z),inside));}}}
    return best;
}
@compute @workgroup_size(8,8)
fn finish(@builtin(global_invocation_id) id:vec3<u32>){
    if id.x>=p.w || id.y>=p.h {return;}
    let q=vec2<i32>(id.xy);let src=a[index(q)];let v=value(q);let near=vec2<i32>(src.xy);
    var distance=sqrt(src.z);if is_chamfer(){distance=src.z;}
    var result=distance-0.5;
    if is_inside(){if v<=EPS {result=-0.5;}}
    else if v>EPS {result=-0.5;}else if !is_chamfer(){result+=1.0-min(value(near),1.0);}
    if p.kind==2u || p.kind==3u {
        let e=sharp(q);
        if e.w>0.0 {result=select(-e.z,e.z,p.kind==3u);}
        else if p.kind==3u && v>EPS && distance<=f32(p.reach)+2.0 {
            var best=INF;var candidate=vec2(-1);
            for(var y=-1;y<=1;y++){for(var x=-1;x<=1;x++){
                let z=near+vec2(x,y);if sharp(z).w>0.0 {let d=vec2<f32>(z-q);let d2=dot(d,d);if d2<best {best=d2;candidate=z;}}
            }}
            if candidate.x>=0 {result=min(result,measured_edge(q,candidate,true,true));}
        }else if p.kind==2u && v<=EPS {
            let ne=sharp(near);if ne.w>0.0 {
                let d=measured_edge(q,near,false,distance<=f32(p.reach)+2.0);result=distance-ne.z;if d<INF {result=d;}
            }
        }
    }
    textureStore(output,q,vec4(result,0.0,0.0,0.0));
}
