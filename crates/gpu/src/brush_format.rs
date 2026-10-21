//! Native document colour helpers and the exact CMYK tables used by the CPU colour manager.
use photocraft_color::{ColorMode, PixelFormat};
use std::sync::{Arc, Mutex};
use wgpu::util::DeviceExt;

pub const SHADER: &str = include_str!("brush_format.wgsl");

/// Immutable per-stroke profile tables. Upload once while the document CMYK space is active.
/// These are the CPU transform's existing tables, not a sampled approximation of its output.
#[derive(Clone, Debug)]
pub struct NativeFormat {
    pub format: PixelFormat,
    pub info: wgpu::Buffer,
    pub values: wgpu::Buffer,
    pub(crate) pipelines: Arc<NativeFormatPipelines>,
}
impl NativeFormat {
    pub fn new(device: &wgpu::Device, format: PixelFormat) -> Result<Self, String> {
        Self::with_pipelines(device, format, Arc::new(NativeFormatPipelines::new(device)))
    }
    /// Stroke factories build pipelines once, before first pen contact.
    pub fn with_pipelines(device: &wgpu::Device, format: PixelFormat, pipelines: Arc<NativeFormatPipelines>) -> Result<Self, String> {
        let mut info = [0u32; 32];
        let mut values = Vec::<f32>::new();
        if format.mode == ColorMode::Cmyk {
            photocraft_color::convert::with_cmyk_transforms(|transforms| -> Result<(), String> {
                let (forward, reverse) = transforms.ok_or("CMYK profile transforms could not be linked")?;
                for (slot, transform) in [forward, reverse].into_iter().enumerate() {
                    let grid = transform.fast_grid().ok_or("CMYK profile has no GPU device-link grid")?;
                    let clut = grid.clut;
                    if !matches!(clut.inputs, 3 | 4) || clut.outputs > 4 || clut.grid.iter().any(|&n| n < 2) {
                        return Err("CMYK device-link dimensions are unsupported".into());
                    }
                    let h = slot * 16;
                    info[h] = clut.inputs as u32;
                    info[h + 1] = clut.outputs as u32;
                    let mut stride = clut.outputs;
                    for dimension in (0..clut.inputs).rev() {
                        info[h + 2 + dimension] = clut.grid[dimension] as u32;
                        info[h + 6 + dimension] = stride as u32;
                        stride *= clut.grid[dimension];
                    }
                    info[h + 10] = values.len() as u32;
                    values.extend_from_slice(&clut.data);
                    if let Some(post) = grid.post {
                        info[h + 11] = values.len() as u32;
                        info[h + 12] = post[0].len() as u32;
                        for table in post {
                            values.extend_from_slice(table);
                        }
                    }
                }
                Ok(())
            })?;
        }
        if values.is_empty() {
            values.push(0.0);
        }
        let info = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_native_format_info"),
            contents: bytemuck::cast_slice(&info),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let values = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_native_format_values"),
            contents: bytemuck::cast_slice(&values),
            usage: wgpu::BufferUsages::STORAGE,
        });
        Ok(Self { format, info, values, pipelines })
    }
    /// Convert a region without quantisation. Native alpha is the red channel of its own texture.
    /// RGBA-to-native reads alpha from the RGBA colour texture; its alpha input may alias that texture.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        to_rgba: bool,
        input_color: &wgpu::Texture,
        input_alpha: &wgpu::Texture,
        output_color: &wgpu::Texture,
        output_alpha: &wgpu::Texture,
        rect: [u32; 4],
    ) {
        let pipeline = &self.pipelines.conversions[usize::from(!to_rgba)];
        let params = [rect, [self.mode(), u32::from(self.format.alpha), 0, 0]];
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_native_conversion"),
            contents: bytemuck::cast_slice(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let views = [input_color, input_alpha, output_color, output_alpha].map(|t| t.create_view(&Default::default()));
        let mut entries = Vec::new();
        for (binding, view) in views.iter().enumerate() {
            entries.push(wgpu::BindGroupEntry { binding: binding as u32, resource: wgpu::BindingResource::TextureView(view) });
        }
        entries.push(wgpu::BindGroupEntry { binding: 4, resource: self.info.as_entire_binding() });
        entries.push(wgpu::BindGroupEntry { binding: 5, resource: self.values.as_entire_binding() });
        entries.push(wgpu::BindGroupEntry { binding: 6, resource: uniform.as_entire_binding() });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_native_conversion"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &entries,
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_native_conversion"), timestamp_writes: None });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(rect[2].div_ceil(8), rect[3].div_ceil(8), 1);
    }
    /// Render native channels to compositor/mask texels under this format's current profile.
    pub fn encode_display(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        color: &wgpu::Texture,
        alpha: &wgpu::Texture,
        target: wgpu::TextureFormat,
    ) -> Result<wgpu::Texture, String> {
        let index = DISPLAY_FORMATS.iter().position(|&f| f == target).ok_or("unsupported native display target")?;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pc_native_display"),
            size: color.size(),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: target,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let views = [color, alpha].map(|t| t.create_view(&Default::default()));
        let output = texture.create_view(&Default::default());
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_native_display"),
            contents: bytemuck::cast_slice(&[self.mode(), u32::from(self.format.alpha), 0, 0]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let pipeline = &self.pipelines.displays[index];
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_native_display"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&views[0]) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&views[1]) },
                wgpu::BindGroupEntry { binding: 2, resource: self.info.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: self.values.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: uniform.as_entire_binding() },
            ],
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pc_native_display"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }
        Ok(texture)
    }

    pub fn mode(&self) -> u32 {
        match self.format.mode {
            ColorMode::Bitmap | ColorMode::Grayscale | ColorMode::Duotone => 1,
            ColorMode::Cmyk => 2,
            ColorMode::Lab => 3,
            _ => 0,
        }
    }
    /// WGSL helpers plus storage declarations at the caller's existing bind group.
    pub fn shader(group: u32, info_binding: u32, values_binding: u32) -> String {
        format!(
            "@group({group}) @binding({info_binding}) var<storage,read> cms_info:array<u32>;\n@group({group}) @binding({values_binding}) var<storage,read> cms_values:array<f32>;\n{SHADER}"
        )
    }
    pub fn layout_entries(info_binding: u32, values_binding: u32) -> [wgpu::BindGroupLayoutEntry; 2] {
        [info_binding, values_binding].map(|binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        })
    }
}

/// Profile-aware device cache owned by the brush factory/compositor.
/// Analytic formats and all shader pipelines are ready before first contact.
pub struct NativeFormats {
    pipelines: Arc<NativeFormatPipelines>,
    formats: Mutex<Vec<(PixelFormat, u64, Arc<NativeFormat>)>>,
}
impl NativeFormats {
    pub fn new(device: &wgpu::Device) -> Self {
        let pipelines = Arc::new(NativeFormatPipelines::new(device));
        let mut formats = Vec::new();
        for mode in [ColorMode::Rgb, ColorMode::Grayscale, ColorMode::Lab] {
            for sample in [photocraft_color::SampleType::U8, photocraft_color::SampleType::U16, photocraft_color::SampleType::F32] {
                for alpha in [false, true] {
                    let format = PixelFormat::new(mode, sample, alpha);
                    if let Ok(value) = NativeFormat::with_pipelines(device, format, pipelines.clone()) {
                        formats.push((format, 0, Arc::new(value)));
                    }
                }
            }
        }
        Self { pipelines, formats: Mutex::new(formats) }
    }
    /// Call while entering a document/profile as well as from begin; repeated calls only clone an Arc.
    pub fn get(&self, device: &wgpu::Device, format: PixelFormat) -> Result<Arc<NativeFormat>, String> {
        let profile = if format.mode == ColorMode::Cmyk { photocraft_color::convert::active_cmyk_space().map_or(0, |p| p.id) } else { 0 };
        let mut formats = self.formats.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((_, _, value)) = formats.iter().find(|(f, p, _)| *f == format && *p == profile) {
            return Ok(value.clone());
        }
        let value = Arc::new(NativeFormat::with_pipelines(device, format, self.pipelines.clone())?);
        formats.push((format, profile, value.clone()));
        Ok(value)
    }
}

/// Device-shared pipelines. Create with the brush factory, never on first contact.
#[derive(Debug)]
pub struct NativeFormatPipelines {
    conversions: [wgpu::ComputePipeline; 2],
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) snapshots: [wgpu::ComputePipeline; 3],
    displays: [wgpu::RenderPipeline; 5],
}
impl NativeFormatPipelines {
    pub fn new(device: &wgpu::Device) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let snapshots = std::array::from_fn(|sample| {
            let native = if sample == 0 { "rgba8unorm" } else { "rgba16float" };
            let shader = format!("{}\n{}", NativeFormat::shader(0, 6, 7), SNAPSHOT_SHADER.replace("NATIVE_FORMAT", native));
            let mut entries = vec![];
            for binding in 0..2 {
                entries.push(wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                });
            }
            for binding in 2..6 {
                entries.push(wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: if binding == 5 {
                            if sample == 0 { wgpu::TextureFormat::Rgba8Unorm } else { wgpu::TextureFormat::Rgba16Float }
                        } else {
                            wgpu::TextureFormat::Rgba32Float
                        },
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                });
            }
            entries.extend(NativeFormat::layout_entries(6, 7));
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 8,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            });
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_native_snapshot"), entries: &entries });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("pc_native_snapshot"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let module = device
                .create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("pc_native_snapshot"), source: wgpu::ShaderSource::Wgsl(shader.into()) });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("pc_native_snapshot"),
                layout: Some(&layout),
                module: &module,
                entry_point: Some("native_snapshot"),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        let conversions = std::array::from_fn(|direction| {
            let shader = format!("{}\n{}", NativeFormat::shader(0, 4, 5), CONVERSION_SHADER);
            let mut entries = Vec::new();
            for binding in 0..2 {
                entries.push(wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                });
            }
            for binding in 2..4 {
                entries.push(wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                });
            }
            entries.extend(NativeFormat::layout_entries(4, 5));
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 6,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            });
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_native_conversion"), entries: &entries });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("pc_native_conversion"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let module = device
                .create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("pc_native_conversion"), source: wgpu::ShaderSource::Wgsl(shader.into()) });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("pc_native_conversion"),
                layout: Some(&layout),
                module: &module,
                entry_point: Some(if direction == 0 { "to_rgba" } else { "from_rgba" }),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        let displays = std::array::from_fn(|index| {
            let shader = format!("{}\n{}", NativeFormat::shader(0, 2, 3), DISPLAY_SHADER);
            let mut entries = Vec::new();
            for binding in 0..2 {
                entries.push(wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                });
            }
            entries.extend(NativeFormat::layout_entries(2, 3).map(|mut e| {
                e.visibility = wgpu::ShaderStages::FRAGMENT;
                e
            }));
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            });
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_native_display"), entries: &entries });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("pc_native_display"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let module =
                device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("pc_native_display"), source: wgpu::ShaderSource::Wgsl(shader.into()) });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("pc_native_display"),
                layout: Some(&layout),
                vertex: wgpu::VertexState { module: &module, entry_point: Some("display_vertex"), compilation_options: Default::default(), buffers: &[] },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("display_fragment"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: DISPLAY_FORMATS[index], blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            })
        });
        Self {
            conversions,
            #[cfg(not(target_arch = "wasm32"))]
            snapshots,
            displays,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
const SNAPSHOT_SHADER: &str = r#"
@group(0) @binding(0) var source_color:texture_2d<f32>;
@group(0) @binding(1) var source_alpha:texture_2d<f32>;
@group(0) @binding(2) var encoded_color:texture_storage_2d<rgba32float,write>;
@group(0) @binding(3) var encoded_alpha:texture_storage_2d<rgba32float,write>;
@group(0) @binding(4) var display_pixels:texture_storage_2d<rgba32float,write>;
@group(0) @binding(5) var display_native:texture_storage_2d<NATIVE_FORMAT,write>;
struct SnapshotParams { origin:vec4<u32>, format:vec4<u32> }
@group(0) @binding(8) var<uniform> snapshot:SnapshotParams;
@compute @workgroup_size(8,8) fn native_snapshot(@builtin(global_invocation_id) id:vec3<u32>){
 let out=vec2<i32>(id.xy);let at=vec2<i32>(id.xy+snapshot.origin.xy);
 var color=textureLoad(source_color,at,0);for(var i=0u;i<4u;i++){color[i]=native_quantize(color[i],snapshot.format.z);}
 let alpha=native_quantize(select(1.0,textureLoad(source_alpha,at,0).r,snapshot.format.y!=0u),snapshot.format.z);
 let rgba=native_to_rgba(snapshot.format.x,snapshot.format.y!=0u,NativePixel(color,alpha));
 textureStore(encoded_color,out,color);textureStore(encoded_alpha,out,vec4(alpha,0.0,0.0,0.0));textureStore(display_pixels,out,rgba);textureStore(display_native,out,rgba);
}
"#;

const CONVERSION_SHADER: &str = r#"
@group(0) @binding(0) var conversion_color:texture_2d<f32>;
@group(0) @binding(1) var conversion_alpha:texture_2d<f32>;
@group(0) @binding(2) var converted_color:texture_storage_2d<rgba32float,write>;
@group(0) @binding(3) var converted_alpha:texture_storage_2d<rgba32float,write>;
struct ConversionParams{rect:vec4<u32>,kind:vec4<u32>}
@group(0) @binding(6) var<uniform> conversion:ConversionParams;
@compute @workgroup_size(8,8) fn to_rgba(@builtin(global_invocation_id) id:vec3<u32>){
 if(any(id.xy>=conversion.rect.zw)){return;}let at=vec2<i32>(id.xy+conversion.rect.xy);
 let v=native_to_rgba(conversion.kind.x,conversion.kind.y!=0u,NativePixel(textureLoad(conversion_color,at,0),textureLoad(conversion_alpha,at,0).r));
 textureStore(converted_color,at,v);textureStore(converted_alpha,at,vec4(v.a,0.0,0.0,0.0));
}
@compute @workgroup_size(8,8) fn from_rgba(@builtin(global_invocation_id) id:vec3<u32>){
 if(any(id.xy>=conversion.rect.zw)){return;}let at=vec2<i32>(id.xy+conversion.rect.xy);
 let v=native_from_rgba(conversion.kind.x,conversion.kind.y!=0u,textureLoad(conversion_color,at,0));
 textureStore(converted_color,at,v.color);textureStore(converted_alpha,at,vec4(v.alpha,0.0,0.0,0.0));
}
"#;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use photocraft_color::SampleType;
    use photocraft_geom::TILE_SIZE;
    use photocraft_raster::{from_rgba, to_rgba};

    #[test]
    fn native_format_device_roundtrip() {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let pipelines = Arc::new(NativeFormatPipelines::new(&device));
        let snapshots = crate::tile::TileSnapshots::new(&device);
        let side = TILE_SIZE as u32;
        let make = || {
            device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let upload = |texture: &wgpu::Texture, pixel: [f32; 4]| {
            let pixels = vec![pixel; (side * side) as usize];
            queue.write_texture(
                texture.as_image_copy(),
                bytemuck::cast_slice(&pixels),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(side * 16), rows_per_image: Some(side) },
                texture.size(),
            );
        };
        for (mode, alpha) in
            [(ColorMode::Rgb, false), (ColorMode::Grayscale, false), (ColorMode::Grayscale, true), (ColorMode::Lab, true), (ColorMode::Cmyk, true)]
        {
            let fmt = PixelFormat::new(mode, SampleType::F32, alpha);
            let format = NativeFormat::with_pipelines(&device, fmt, pipelines.clone()).unwrap();
            let rgba = [0.17, 0.43, 0.71, 0.63];
            let expected = from_rgba(&fmt, rgba);
            let nc = fmt.mode.color_channels();
            let color = make();
            let separate_alpha = make();
            let rgba_source = make();
            let extra = make();
            upload(&rgba_source, rgba);
            let mut encoder = device.create_command_encoder(&Default::default());
            format.encode(&device, &mut encoder, false, &rgba_source, &rgba_source, &color, &separate_alpha, [0, 0, side, side]);
            let native = snapshots.capture_native(&device, &queue, &mut encoder, &color, &separate_alpha, [0, 0], &format).unwrap();
            format.encode(&device, &mut encoder, true, &color, &separate_alpha, &rgba_source, &extra, [0, 0, side, side]);
            let display = snapshots.capture(&device, &queue, &mut encoder, &rgba_source, [0, 0], PixelFormat::RGBA32F).unwrap();
            queue.submit([encoder.finish()]);
            let bytes = native.bytes();
            for (channel, value) in expected.iter().enumerate() {
                let actual = f32::from_ne_bytes(bytes[channel * 4..channel * 4 + 4].try_into().unwrap());
                assert!((actual - value).abs() < 0.0001, "{fmt:?} native channel {channel}: {actual} != {value}");
            }
            let expected_display = to_rgba(&fmt, &expected);
            let bytes = display.bytes();
            for (channel, value) in expected_display.iter().enumerate() {
                let actual = f32::from_ne_bytes(bytes[channel * 4..channel * 4 + 4].try_into().unwrap());
                assert!((actual - value).abs() < 0.0002, "{fmt:?} display channel {channel}: {actual} != {value}");
            }
            if mode == ColorMode::Cmyk {
                let ink = [0.19, 0.27, 0.36, 0.58];
                upload(&color, ink);
                upload(&separate_alpha, [0.72, 0.0, 0.0, 0.0]);
                let mut encoder = device.create_command_encoder(&Default::default());
                let tile = snapshots.capture_native(&device, &queue, &mut encoder, &color, &separate_alpha, [0, 0], &format).unwrap();
                queue.submit([encoder.finish()]);
                let default = vec![0u8; fmt.bytes_per_pixel()];
                assert_eq!(tile.content_bounds(&default), Some((0, 0, TILE_SIZE as u16, TILE_SIZE as u16)));
                assert!(!tile.is_cpu_materialized());
                let bytes = tile.bytes();
                for (channel, value) in ink.iter().chain(std::iter::once(&0.72)).enumerate() {
                    let actual = f32::from_ne_bytes(bytes[channel * 4..channel * 4 + 4].try_into().unwrap());
                    assert_eq!(actual, *value, "CMYK native channel {channel}");
                }
                upload(&color, [0.0; 4]);
                upload(&separate_alpha, [0.0; 4]);
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &separate_alpha,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: 3, y: 5, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    bytemuck::cast_slice(&[0.5f32, 0.0, 0.0, 0.0]),
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(16), rows_per_image: Some(1) },
                    wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
                );
                let mut encoder = device.create_command_encoder(&Default::default());
                let sparse = snapshots.capture_native(&device, &queue, &mut encoder, &color, &separate_alpha, [0, 0], &format).unwrap();
                queue.submit([encoder.finish()]);
                assert_eq!(sparse.content_bounds(&vec![0u8; fmt.bytes_per_pixel()]), Some((3, 5, 4, 6)));
                assert!(!sparse.is_cpu_materialized());
            }
            assert!(nc <= 4);
        }
    }
}

const DISPLAY_FORMATS: [wgpu::TextureFormat; 5] = [
    wgpu::TextureFormat::Rgba8Unorm,
    wgpu::TextureFormat::Rgba16Float,
    wgpu::TextureFormat::R8Unorm,
    wgpu::TextureFormat::R16Float,
    wgpu::TextureFormat::R32Float,
];
const DISPLAY_SHADER: &str = r#"
@group(0) @binding(0) var display_color:texture_2d<f32>;
@group(0) @binding(1) var display_alpha:texture_2d<f32>;
@group(0) @binding(4) var<uniform> display_kind:vec4<u32>;
@vertex fn display_vertex(@builtin(vertex_index) vertex:u32)->@builtin(position) vec4<f32>{
 let x=f32((vertex<<1u)&2u);let y=f32(vertex&2u);return vec4(x*2.0-1.0,y*2.0-1.0,0.0,1.0);
}
@fragment fn display_fragment(@builtin(position) position:vec4<f32>)->@location(0) vec4<f32>{
 let at=vec2<i32>(position.xy);return native_to_rgba(display_kind.x,display_kind.y!=0u,NativePixel(textureLoad(display_color,at,0),textureLoad(display_alpha,at,0).r));
}
"#;
