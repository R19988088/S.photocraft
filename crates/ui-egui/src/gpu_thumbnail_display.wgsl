struct View { screen:vec4<f32>, cell:vec4<f32>, checker:vec4<f32> }
struct Frame { bounds:vec4<i32>, value:vec4<i32> }
@group(0) @binding(0) var<uniform> view:View;
@group(0) @binding(1) var<storage,read> frame:Frame;
@group(0) @binding(2) var image:texture_2d<f32>;
@group(0) @binding(3) var image_sampler:sampler;
struct Vertex { @builtin(position) position:vec4<f32>, @location(0) uv:vec2<f32>, @location(1) @interpolate(flat) layer:u32, @location(2) local:vec2<f32> }
@vertex fn vertex(@builtin(vertex_index) index:u32,@builtin(instance_index) layer:u32)->Vertex{
    let corners=array<vec2<f32>,6>(vec2(0.0,0.0),vec2(1.0,0.0),vec2(0.0,1.0),vec2(0.0,1.0),vec2(1.0,0.0),vec2(1.0,1.0));let corner=corners[index];
    let dimensions=vec2<f32>(max(frame.value.zw,vec2(1)));let ratio=dimensions/max(dimensions.x,dimensions.y);let size=max(view.cell.zw*ratio,vec2(1.0));
    let point=view.cell.xy+(view.cell.zw-size)*0.5+corner*size;let clip=point*view.screen.z/view.screen.xy*2.0-1.0;
    var out:Vertex;out.position=vec4(clip.x,-clip.y,0.0,1.0);out.uv=(vec2(1.0)-ratio)*0.5+corner*ratio;out.local=corner*size;out.layer=layer;return out;
}
fn linear_from_gamma(srgb:vec3<f32>)->vec3<f32>{return select(pow((srgb+0.055)/1.055,vec3(2.4)),srgb/12.92,srgb<vec3(0.04045));}
@fragment fn fragment(in:Vertex)->@location(0) vec4<f32>{
    var result=vec4(0.0);
    if(in.layer==0u){let square=vec2<i32>(floor(in.local/view.checker.z));let shade=select(view.checker.x,view.checker.y,((square.x+square.y)&1)!=0);result=vec4(vec3(shade),1.0);}
    else{result=textureSample(image,image_sampler,in.uv);}
    if(view.screen.w>0.5){result=vec4(linear_from_gamma(result.rgb),result.a);}return result;
}
