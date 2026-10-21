struct Params { rect:vec4<u32>, value:vec4<f32>, mode:vec4<u32> }
@group(0) @binding(0) var a:texture_2d<f32>;
@group(0) @binding(1) var b:texture_2d<f32>;
@group(0) @binding(2) var c:texture_2d<f32>;
@group(0) @binding(3) var d:texture_2d<f32>;
@group(0) @binding(4) var output:texture_storage_2d<rgba32float,write>;
@group(0) @binding(5) var<uniform> p:Params;
@compute @workgroup_size(8,8) fn glue(@builtin(global_invocation_id) id:vec3<u32>) {
 if(any(id.xy>=p.rect.zw)){return;} let at=vec2<i32>(id.xy+p.rect.xy);
 let av=textureLoad(a,at,0); let bv=textureLoad(b,at,0); var v=av;
 switch p.mode.x {
 case 0u:{ v=vec4(av.r,bv.r,bv.a,av.r/max(p.value.x,1e-8)); }
 case 1u:{ v=vec4(av.r/max(p.value.x,1e-6),0.0,0.0,0.0); }
 case 2u:{ v=vec4(bv.a,0.0,0.0,0.0); }
 case 3u:{ v=vec4(clamp(av.r*bv.r*p.value.x,0.0,1.0),0.0,0.0,0.0); }
 case 4u:{ v=p.value; }
 case 7u:{ v=floor(clamp(av,vec4(0.0),vec4(1.0))*p.value.x+0.5)/p.value.x; }
 case 8u:{ v=select(av,bv,textureLoad(d,at,0).r>textureLoad(c,at,0).r); }
 default:{}
 }
 textureStore(output,at,v);
}

// CPU material candidates occupy allocated 64px coverage tiles clipped to stroke bounds.
var<workgroup> occupied: atomic<u32>;
@compute @workgroup_size(8,8) fn coverage_regions(
 @builtin(workgroup_id) group:vec3<u32>, @builtin(local_invocation_id) local:vec3<u32>,
 @builtin(local_invocation_index) index:u32
) {
 if(index==0u){atomicStore(&occupied,0u);} workgroupBarrier();
 let base=group.xy*64u+p.rect.xy;
 for(var y=0u;y<8u;y++){for(var x=0u;x<8u;x++){
  let at=base+local.xy+vec2(x,y)*8u;
  if(all(at<p.rect.xy+p.rect.zw)){
   if(textureLoad(a,vec2<i32>(at),0).r>0.0 || textureLoad(b,vec2<i32>(at),0).r>0.0){atomicStore(&occupied,1u);}
  }
 }}
 workgroupBarrier();
 let region_occupied=atomicLoad(&occupied)!=0u;
 for(var y=0u;y<8u;y++){for(var x=0u;x<8u;x++){
  let at=base+local.xy+vec2(x,y)*8u;
  if(all(at<p.rect.xy+p.rect.zw)){
   let inside=all(vec2<f32>(at)>=p.value.xy) && all(vec2<f32>(at)<p.value.zw);
   textureStore(output,vec2<i32>(at),vec4(select(0.0,1.0,region_occupied && inside),0.0,0.0,0.0));
  }
 }}
}
