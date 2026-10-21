//! Immutable brush tile snapshots with deferred CPU readback.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use photocraft_color::{ColorMode, PixelFormat, SampleType, write_sample};
use photocraft_geom::TILE_SIZE;
use photocraft_raster::{Tile, TileBackend, TileReadError};
use wgpu::util::DeviceExt;

type ContentBounds = Option<(u16, u16, u16, u16)>;

pub const CONTENT_BOUNDS_SHADER: &str = r#"
struct Params { pixel: vec4<u32>, extra: vec4<u32>, sample: vec4<u32> }
@group(0) @binding(0) var pixels: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> bounds: array<atomic<u32>, 4>;
@group(0) @binding(2) var<uniform> params: Params;
@group(0) @binding(3) var alpha_pixels: texture_2d<f32>;
var<workgroup> local_bounds: array<atomic<u32>, 4>;
@compute @workgroup_size(8,8)
fn content_bounds(@builtin(global_invocation_id) id: vec3<u32>, @builtin(local_invocation_index) index: u32) {
    if index < 4u { atomicStore(&local_bounds[index], select(0u, 0xffffffffu, index < 2u)); }
    workgroupBarrier();
    if all(id.xy < textureDimensions(pixels)) {
        let value = textureLoad(pixels, vec2<i32>(id.xy), 0);
        var encoded = bitcast<vec4<u32>>(value);
        if params.sample.x == 0u { encoded = vec4<u32>(clamp(value, vec4<f32>(0.0), vec4<f32>(1.0)) * 255.0 + 0.5); }
        if params.sample.x == 1u { encoded = vec4<u32>(clamp(value, vec4<f32>(0.0), vec4<f32>(1.0)) * 65535.0 + 0.5); }
        var differs = false;
        for(var channel=0u;channel<params.sample.y;channel++){differs=differs || encoded[channel]!=params.pixel[channel];}
        if(params.sample.z!=0u){
            let alpha=textureLoad(alpha_pixels,vec2<i32>(id.xy),0).r;var ea=bitcast<u32>(alpha);
            if(params.sample.x==0u){ea=u32(clamp(alpha,0.0,1.0)*255.0+0.5);}
            if(params.sample.x==1u){ea=u32(clamp(alpha,0.0,1.0)*65535.0+0.5);}
            differs=differs || ea!=params.extra.x;
        }
        if differs {
            atomicMin(&local_bounds[0], id.x); atomicMin(&local_bounds[1], id.y);
            atomicMax(&local_bounds[2], id.x + 1u); atomicMax(&local_bounds[3], id.y + 1u);
        }
    }
    workgroupBarrier();
    if index == 0u && atomicLoad(&local_bounds[0]) != 0xffffffffu {
        atomicMin(&bounds[0], atomicLoad(&local_bounds[0])); atomicMin(&bounds[1], atomicLoad(&local_bounds[1]));
        atomicMax(&bounds[2], atomicLoad(&local_bounds[2])); atomicMax(&bounds[3], atomicLoad(&local_bounds[3]));
    }
}
"#;

#[derive(Clone, Debug)]
struct ContentBoundsKernel {
    pipeline: wgpu::ComputePipeline,
}

impl ContentBoundsKernel {
    fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pc_tile_content_bounds"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_tile_content_bounds"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_tile_content_bounds"),
            source: wgpu::ShaderSource::Wgsl(CONTENT_BOUNDS_SHADER.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pc_tile_content_bounds"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("content_bounds"),
            compilation_options: Default::default(),
            cache: None,
        });
        Self { pipeline }
    }

    fn submit(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pixels: &wgpu::Texture,
        alpha: Option<&wgpu::Texture>,
        format: PixelFormat,
        dp: &[u8],
    ) -> PendingContentBounds {
        let mut encoded = [0u32; 8];
        let sample = match format.sample {
            SampleType::U8 => {
                for (word, byte) in encoded.iter_mut().zip(dp) {
                    *word = u32::from(*byte);
                }
                0u32
            }
            SampleType::U16 => {
                for (word, bytes) in encoded.iter_mut().zip(dp.chunks_exact(2)) {
                    *word = u32::from(u16::from_ne_bytes([bytes[0], bytes[1]]));
                }
                1u32
            }
            SampleType::F32 => {
                for (word, bytes) in encoded.iter_mut().zip(dp.chunks_exact(4)) {
                    *word = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                }
                2u32
            }
        };
        let split_alpha = alpha.is_some() && format.alpha;
        let count = if alpha.is_some() { format.mode.color_channels() } else { format.channels() };
        let params = [
            [encoded[0], encoded[1], encoded[2], encoded[3]],
            [encoded[format.mode.color_channels()], 0, 0, 0],
            [sample, count as u32, u32::from(split_alpha), 0],
        ];
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_tile_bounds_params"),
            contents: bytemuck::cast_slice(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bounds = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_tile_bounds"),
            contents: bytemuck::cast_slice(&[u32::MAX, u32::MAX, 0u32, 0u32]),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pc_tile_bounds_readback"),
            size: 16,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let view = pixels.create_view(&Default::default());
        let alpha_view = alpha.unwrap_or(pixels).create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_tile_content_bounds"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: bounds.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&alpha_view) },
            ],
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pc_tile_content_bounds") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_tile_content_bounds"), timestamp_writes: None });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(pixels.width().div_ceil(8), pixels.height().div_ceil(8), 1);
        }
        encoder.copy_buffer_to_buffer(&bounds, 0, &readback, 0, 16);
        let submission = queue.submit([encoder.finish()]);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        PendingContentBounds { readback, receiver, submission }
    }

    fn read(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pixels: &wgpu::Texture,
        alpha: Option<&wgpu::Texture>,
        format: PixelFormat,
        dp: &[u8],
    ) -> Result<ContentBounds, TileReadError> {
        let pending = self.submit(device, queue, pixels, alpha, format, dp);
        device
            .poll(wgpu::PollType::Wait { submission_index: Some(pending.submission.clone()), timeout: None })
            .map_err(|e| TileReadError(format!("GPU tile bounds readback failed: {e}")))?;
        pending
            .receiver
            .recv()
            .map_err(|e| TileReadError(format!("GPU tile bounds readback failed: {e}")))?
            .map_err(|e| TileReadError(format!("GPU tile bounds readback failed: {e}")))?;
        pending.finish()
    }
}

#[derive(Debug)]
struct PendingContentBounds {
    readback: wgpu::Buffer,
    receiver: std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
    submission: wgpu::SubmissionIndex,
}

impl PendingContentBounds {
    fn finish(&self) -> Result<ContentBounds, TileReadError> {
        let mapped = self.readback.slice(..).get_mapped_range().map_err(|e| TileReadError(format!("GPU tile bounds readback failed: {e}")))?;
        let mut values = [0u32; 4];
        for (word, bytes) in values.iter_mut().zip(mapped.chunks_exact(4)) {
            *word = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        drop(mapped);
        self.readback.unmap();
        Ok((values[0] != u32::MAX).then_some((values[0] as u16, values[1] as u16, values[2] as u16, values[3] as u16)))
    }
}

/// Pipelines shared by all snapshots on one device. Input texels are straight RGBA32F.
#[derive(Clone, Debug)]
pub struct TileSnapshots {
    pipelines: [wgpu::ComputePipeline; 3],
    restrict_channel: wgpu::ComputePipeline,
    content_bounds: ContentBoundsKernel,
}

impl TileSnapshots {
    pub fn new(device: &wgpu::Device) -> Self {
        // U8 snapshots need an rgba8unorm destination layout, the others rgba16float.
        let pipelines = std::array::from_fn(|i| {
            let native_format = if i == 0 { wgpu::TextureFormat::Rgba8Unorm } else { wgpu::TextureFormat::Rgba16Float };
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("pc_tile_snapshot"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::StorageTexture {
                            access: wgpu::StorageTextureAccess::WriteOnly,
                            format: native_format,
                            view_dimension: wgpu::TextureViewDimension::D2,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::StorageTexture {
                            access: wgpu::StorageTextureAccess::WriteOnly,
                            format: wgpu::TextureFormat::Rgba32Float,
                            view_dimension: wgpu::TextureViewDimension::D2,
                        },
                        count: None,
                    },
                ],
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("pc_tile_snapshot"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
            let format = if i == 0 { "rgba8unorm" } else { "rgba16float" };
            let quantize = match i {
                0 => "floor(clamp(v, vec4(0.0), vec4(1.0)) * 255.0 + 0.5) / 255.0",
                1 => "floor(clamp(v, vec4(0.0), vec4(1.0)) * 65535.0 + 0.5) / 65535.0",
                _ => "v",
            };
            let shader = format!(
                "@group(0) @binding(0) var source: texture_2d<f32>;\n\
                 @group(0) @binding(1) var destination: texture_storage_2d<{format}, write>;\n\
                 @group(0) @binding(2) var encoded: texture_storage_2d<rgba32float, write>;\n\
                 @compute @workgroup_size(8,8) fn main(@builtin(global_invocation_id) p: vec3<u32>) {{\n\
                 let v = textureLoad(source, vec2<i32>(p.xy), 0);\n\
                 let q = {quantize};\n\
                 textureStore(destination, vec2<i32>(p.xy), q);\n\
                 textureStore(encoded, vec2<i32>(p.xy), q); }}"
            );
            let module =
                device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("pc_tile_snapshot"), source: wgpu::ShaderSource::Wgsl(shader.into()) });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("pc_tile_snapshot"),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_tile_restrict_channel"),
            source: wgpu::ShaderSource::Wgsl(include_str!("tile_restrict_channel.wgsl").into()),
        });
        let mut entries = Vec::new();
        for binding in 0..3 {
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
        for binding in 3..5 {
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
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: 5,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_tile_restrict_channel"), entries: &entries });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_tile_restrict_channel"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let restrict_channel = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pc_tile_restrict_channel"),
            layout: Some(&layout),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        Self { pipelines, restrict_channel, content_bounds: ContentBoundsKernel::new(device) }
    }

    /// Capture native channels without an RGB round trip. The alpha texture stores alpha in red.
    #[allow(clippy::too_many_arguments)]
    pub fn capture_native(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        color: &wgpu::Texture,
        alpha: &wgpu::Texture,
        origin: [u32; 2],
        format: &crate::brush_format::NativeFormat,
    ) -> Result<Arc<Tile>, TileReadError> {
        let side = TILE_SIZE as u32;
        for source in [color, alpha] {
            if origin[0].checked_add(side).is_none_or(|v| v > source.width()) || origin[1].checked_add(side).is_none_or(|v| v > source.height()) {
                return Err(TileReadError("GPU native snapshot tile exceeds its source texture".into()));
            }
        }
        let sample = match format.format.sample {
            SampleType::U8 => 0,
            SampleType::U16 => 1,
            SampleType::F32 => 2,
        };
        let make = |label, texture_format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: texture_format,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let encoded = make("pc_native_tile_color", wgpu::TextureFormat::Rgba32Float);
        let encoded_alpha = make("pc_native_tile_alpha", wgpu::TextureFormat::Rgba32Float);
        let pixels = make("pc_native_tile_display", wgpu::TextureFormat::Rgba32Float);
        let native = make("pc_native_tile_compositor", if sample == 0 { wgpu::TextureFormat::Rgba8Unorm } else { wgpu::TextureFormat::Rgba16Float });
        let views = [color, alpha, &encoded, &encoded_alpha, &pixels, &native].map(|t| t.create_view(&Default::default()));
        let params = [[origin[0], origin[1], 0, 0], [format.mode(), u32::from(format.format.alpha), sample as u32, 0]];
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_native_snapshot"),
            contents: bytemuck::cast_slice(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let mut entries: Vec<_> = views
            .iter()
            .enumerate()
            .map(|(binding, view)| wgpu::BindGroupEntry { binding: binding as u32, resource: wgpu::BindingResource::TextureView(view) })
            .collect();
        entries.push(wgpu::BindGroupEntry { binding: 6, resource: format.info.as_entire_binding() });
        entries.push(wgpu::BindGroupEntry { binding: 7, resource: format.values.as_entire_binding() });
        entries.push(wgpu::BindGroupEntry { binding: 8, resource: uniform.as_entire_binding() });
        let pipeline = &format.pipelines.snapshots[sample];
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_native_snapshot"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &entries,
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_native_snapshot"), timestamp_writes: None });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(side.div_ceil(8), side.div_ceil(8), 1);
        }
        let mask = if matches!(format.format.mode, ColorMode::Bitmap | ColorMode::Grayscale | ColorMode::Duotone) {
            let target = match format.format.sample {
                SampleType::U8 => wgpu::TextureFormat::R8Unorm,
                SampleType::U16 => wgpu::TextureFormat::R16Float,
                SampleType::F32 => wgpu::TextureFormat::R32Float,
            };
            Some(format.encode_display(device, encoder, &encoded, &encoded_alpha, target).map_err(TileReadError)?)
        } else {
            None
        };
        let backend = GpuTileSnapshot {
            device: device.clone(),
            queue: queue.clone(),
            pixels,
            native,
            mask,
            native_data: Some((encoded, encoded_alpha)),
            format: format.format,
            snapshots: self.clone(),
            native_format: Some(format.clone()),
            bounds_cache: Mutex::new(BTreeMap::new()),
            pending_bounds: Mutex::new(BTreeMap::new()),
        };
        Ok(Arc::new(Tile::from_backend(format.format, Arc::new(backend))))
    }

    /// Copy one complete tile out of mutable stroke state. Submit this encoder before exposing
    /// the tile to CPU readers or a compositor using a different encoder. Subsequent source
    /// writes cannot change this snapshot, including tiles retained by undo.
    pub fn capture(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::Texture,
        origin: [u32; 2],
        format: PixelFormat,
    ) -> Result<Arc<Tile>, TileReadError> {
        if format.mode != ColorMode::Rgb || !format.alpha || source.format() != wgpu::TextureFormat::Rgba32Float {
            return Err(TileReadError("GPU tile snapshots require an RGBA layer and an RGBA32F source".into()));
        }
        let side = TILE_SIZE as u32;
        if origin[0].checked_add(side).is_none_or(|n| n > source.width()) || origin[1].checked_add(side).is_none_or(|n| n > source.height()) {
            return Err(TileReadError("GPU snapshot tile exceeds its source texture".into()));
        }
        let size = wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 };
        let input = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pc_immutable_tile_pixels"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: source,
                mip_level: 0,
                origin: wgpu::Origin3d { x: origin[0], y: origin[1], z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            input.as_image_copy(),
            size,
        );
        let pixels = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pc_immutable_tile_encoded"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let i = match format.sample {
            SampleType::U8 => 0,
            SampleType::U16 => 1,
            SampleType::F32 => 2,
        };
        let native = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pc_immutable_tile_native"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: if i == 0 { wgpu::TextureFormat::Rgba8Unorm } else { wgpu::TextureFormat::Rgba16Float },
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let source_view = input.create_view(&Default::default());
        let target_view = native.create_view(&Default::default());
        let encoded_view = pixels.create_view(&Default::default());
        let pipeline = &self.pipelines[i];
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_tile_snapshot"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&source_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&target_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&encoded_view) },
            ],
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_tile_snapshot"), timestamp_writes: None });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(side / 8, side / 8, 1);
        }
        let backend = GpuTileSnapshot {
            device: device.clone(),
            queue: queue.clone(),
            pixels,
            native,
            native_data: None,
            mask: None,
            format,
            snapshots: self.clone(),
            native_format: None,
            bounds_cache: Mutex::new(BTreeMap::new()),
            pending_bounds: Mutex::new(BTreeMap::new()),
        };
        Ok(Arc::new(Tile::from_backend(format, Arc::new(backend))))
    }
}

/// Resources are private so a published tile cannot accidentally be used as mutable stroke state.
#[derive(Debug)]
pub struct GpuTileSnapshot {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pixels: wgpu::Texture,
    native: wgpu::Texture,
    native_data: Option<(wgpu::Texture, wgpu::Texture)>,
    mask: Option<wgpu::Texture>,
    format: PixelFormat,
    snapshots: TileSnapshots,
    native_format: Option<crate::brush_format::NativeFormat>,
    bounds_cache: Mutex<BTreeMap<Vec<u8>, ContentBounds>>,
    pending_bounds: Mutex<BTreeMap<Vec<u8>, PendingContentBounds>>,
}

impl GpuTileSnapshot {
    /// Exact bounds once the GPU reduction is ready; `None` means it is still pending.
    /// Only the four bounds coordinates are mapped, without waiting for the GPU.
    pub fn poll_content_bounds(&self, dp: &[u8]) -> Result<Option<ContentBounds>, TileReadError> {
        if dp.len() != self.format.bytes_per_pixel() {
            return Err(TileReadError("GPU tile bounds default pixel has the wrong size".into()));
        }
        let mut cache = self.bounds_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bounds) = cache.get(dp) {
            return Ok(Some(*bounds));
        }
        let mut requests = self.pending_bounds.lock().unwrap_or_else(|e| e.into_inner());
        let pending = requests.entry(dp.to_vec()).or_insert_with(|| {
            self.snapshots.content_bounds.submit(
                &self.device,
                &self.queue,
                self.native_data.as_ref().map_or(&self.pixels, |(c, _)| c),
                self.native_data.as_ref().map(|(_, a)| a),
                self.format,
                dp,
            )
        });
        self.device.poll(wgpu::PollType::Poll).map_err(|e| TileReadError(format!("GPU tile bounds readback failed: {e}")))?;
        match pending.receiver.try_recv() {
            Ok(result) => {
                let result = result.map_err(|e| TileReadError(format!("GPU tile bounds readback failed: {e}"))).and_then(|()| pending.finish());
                requests.remove(dp);
                let bounds = result?;
                cache.insert(dp.to_vec(), bounds);
                Ok(Some(bounds))
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(e) => {
                requests.remove(dp);
                Err(TileReadError(format!("GPU tile bounds readback failed: {e}")))
            }
        }
    }

    /// Upload CPU-native channels, or borrow immutable GPU channels without materializing them.
    fn restriction_input(&self, tile: Option<&Tile>, default: &[f32]) -> Result<(wgpu::Texture, wgpu::Texture, bool), TileReadError> {
        if let Some(backend) = tile.and_then(Tile::backend) {
            let snapshot = backend.as_any().downcast_ref::<Self>().ok_or_else(|| TileReadError("unsupported GPU channel restriction source".into()))?;
            if snapshot.format != self.format || snapshot.device != self.device {
                return Err(TileReadError("GPU channel restriction sources use different formats or devices".into()));
            }
            return Ok(match &snapshot.native_data {
                Some((color, alpha)) => (color.clone(), alpha.clone(), false),
                None => (snapshot.pixels.clone(), snapshot.pixels.clone(), true),
            });
        }
        let channels = self.format.channels();
        if default.len() != channels {
            return Err(TileReadError("GPU channel restriction default pixel has the wrong size".into()));
        }
        let side = TILE_SIZE as u32;
        let count = (side * side) as usize;
        let mut color = vec![[0.0f32; 4]; count];
        let mut alpha = vec![[1.0f32; 4]; count];
        let bytes = tile.map(Tile::try_bytes).transpose()?;
        for i in 0..count {
            for channel in 0..self.format.mode.color_channels() {
                color[i][channel] = bytes.map_or(default[channel], |data| photocraft_color::read_sample(data, self.format.sample, i * channels + channel));
            }
            if self.format.alpha {
                let channel = self.format.mode.color_channels();
                alpha[i] = [bytes.map_or(default[channel], |data| photocraft_color::read_sample(data, self.format.sample, i * channels + channel)); 4];
            }
        }
        let upload = |data: &[[f32; 4]], label| {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            self.queue.write_texture(
                texture.as_image_copy(),
                bytemuck::cast_slice(data),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(side * 16), rows_per_image: Some(side) },
                texture.size(),
            );
            texture
        };
        Ok((upload(&color, "pc_channel_before_color"), upload(&alpha, "pc_channel_before_alpha"), false))
    }

    #[allow(clippy::too_many_arguments)]
    fn restrict_channel(
        &self,
        format: PixelFormat,
        before: Option<&Tile>,
        after: Option<&Tile>,
        before_default: &[f32],
        after_default: &[f32],
        channel: usize,
    ) -> Result<Arc<Tile>, TileReadError> {
        if format != self.format || channel >= format.mode.color_channels() {
            return Err(TileReadError("invalid GPU colour channel restriction".into()));
        }
        let (old_color, old_alpha, packed_alpha) = self.restriction_input(before, before_default)?;
        let (new_color, _, _) = self.restriction_input(after, after_default)?;
        let side = TILE_SIZE as u32;
        let make = |label| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let color = make("pc_restricted_channel_color");
        let alpha = make("pc_restricted_channel_alpha");
        let uniform = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_restrict_channel_params"),
            contents: bytemuck::cast_slice(&[channel as u32, u32::from(packed_alpha), u32::from(format.mode == ColorMode::Rgb && format.alpha), 0]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let views = [&old_color, &old_alpha, &new_color, &color, &alpha].map(|t| t.create_view(&Default::default()));
        let mut entries: Vec<_> = views
            .iter()
            .enumerate()
            .map(|(binding, view)| wgpu::BindGroupEntry { binding: binding as u32, resource: wgpu::BindingResource::TextureView(view) })
            .collect();
        entries.push(wgpu::BindGroupEntry { binding: 5, resource: uniform.as_entire_binding() });
        let pipeline = &self.snapshots.restrict_channel;
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_tile_restrict_channel"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &entries,
        });
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pc_tile_restrict_channel") });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_tile_restrict_channel"), timestamp_writes: None });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(side.div_ceil(8), side.div_ceil(8), 1);
        }
        let result = if let Some(native) = &self.native_format {
            self.snapshots.capture_native(&self.device, &self.queue, &mut encoder, &color, &alpha, [0, 0], native)?
        } else {
            self.snapshots.capture(&self.device, &self.queue, &mut encoder, &color, [0, 0], format)?
        };
        self.queue.submit([encoder.finish()]);
        Ok(result)
    }

    pub(crate) fn native(&self) -> &wgpu::Texture {
        &self.native
    }

    pub fn native_mask(&self) -> Option<&wgpu::Texture> {
        self.mask.as_ref()
    }
    /// Refresh a CMYK tile's display under a newly assigned profile, entirely on the GPU.
    pub fn encode_display(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        format: &crate::brush_format::NativeFormat,
        target: wgpu::TextureFormat,
    ) -> Result<Option<wgpu::Texture>, TileReadError> {
        self.native_data.as_ref().map(|(color, alpha)| format.encode_display(device, encoder, color, alpha, target).map_err(TileReadError)).transpose()
    }

    /// Native channels when this tile was captured through the native-format pipeline.
    pub fn native_color(&self) -> Option<&wgpu::Texture> {
        self.native_data.as_ref().map(|(c, _)| c)
    }
    pub fn native_alpha(&self) -> Option<&wgpu::Texture> {
        self.native_data.as_ref().map(|(_, a)| a)
    }

    /// Immutable straight RGBA32F input for subsequent GPU brush work. Do not write this texture.
    pub fn pixels(&self) -> &wgpu::Texture {
        &self.pixels
    }
}

impl TileBackend for GpuTileSnapshot {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn storage_byte_len(&self) -> usize {
        let side = TILE_SIZE as usize;
        let mask = self.mask.as_ref().map_or(0, |t| t.format().block_copy_size(None).unwrap_or(0) as usize);
        side * side * (16 + if self.format.sample == SampleType::U8 { 4 } else { 8 } + if self.native_data.is_some() { 32 } else { 0 } + mask)
    }

    fn read_content_bounds(&self, dp: &[u8]) -> Option<Result<ContentBounds, TileReadError>> {
        if dp.len() != self.format.bytes_per_pixel() {
            return None;
        }
        let mut cache = self.bounds_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bounds) = cache.get(dp) {
            return Some(Ok(*bounds));
        }
        let result = self.snapshots.content_bounds.read(
            &self.device,
            &self.queue,
            self.native_data.as_ref().map_or(&self.pixels, |(c, _)| c),
            self.native_data.as_ref().map(|(_, a)| a),
            self.format,
            dp,
        );
        if let Ok(bounds) = result.as_ref() {
            cache.insert(dp.to_vec(), *bounds);
        }
        Some(result)
    }

    fn restrict_color_channel(
        &self,
        format: PixelFormat,
        before: Option<&Tile>,
        after: Option<&Tile>,
        before_default: &[f32],
        after_default: &[f32],
        channel: usize,
    ) -> Option<Result<Arc<Tile>, TileReadError>> {
        Some(self.restrict_channel(format, before, after, before_default, after_default, channel))
    }

    fn read_bytes(&self) -> Result<Box<[u8]>, TileReadError> {
        let side = TILE_SIZE as u32;
        let row = side * 16;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pc_tile_materialize"),
            size: u64::from(row * side) * if self.native_data.is_some() { 2 } else { 1 },
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pc_tile_materialize") });
        let sources: Vec<&wgpu::Texture> = match self.native_data.as_ref() {
            Some((color, alpha)) => vec![color, alpha],
            None => vec![&self.pixels],
        };
        for (index, source) in sources.iter().enumerate() {
            encoder.copy_texture_to_buffer(
                source.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: (index as u64) * u64::from(row * side),
                        bytes_per_row: Some(row),
                        rows_per_image: Some(side),
                    },
                },
                source.size(),
            );
        }
        let submission = self.queue.submit([encoder.finish()]);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: None })
            .map_err(|e| TileReadError(format!("GPU tile readback failed: {e}")))?;
        receiver
            .recv()
            .map_err(|e| TileReadError(format!("GPU tile readback failed: {e}")))?
            .map_err(|e| TileReadError(format!("GPU tile readback failed: {e}")))?;
        let mapped = buffer.slice(..).get_mapped_range().map_err(|e| TileReadError(format!("GPU tile readback failed: {e}")))?;
        let mut bytes = vec![0u8; (side * side) as usize * self.format.bytes_per_pixel()];
        let channels = self.format.channels();
        let colors = self.format.mode.color_channels();
        for pixel in 0..(side * side) as usize {
            for channel in 0..channels {
                let index = if self.native_data.is_some() && channel == colors { (side * side) as usize * 4 + pixel * 4 } else { pixel * 4 + channel };
                let offset = index * 4;
                let sample = &mapped[offset..offset + 4];
                let value = f32::from_ne_bytes([sample[0], sample[1], sample[2], sample[3]]);
                write_sample(&mut bytes, self.format.sample, pixel * channels + channel, value);
            }
        }
        drop(mapped);
        buffer.unmap();
        Ok(bytes.into_boxed_slice())
    }
}
