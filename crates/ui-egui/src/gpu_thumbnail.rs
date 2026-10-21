//! Exact layer-thumbnail samples and framing from GPU tiles, without tile or bounds readback.
use eframe::{egui_wgpu, wgpu};
#[cfg(test)]
use photocraft_geom::TileCoord;
use photocraft_geom::{Rect, TILE_SIZE};
use photocraft_raster::{Surface, Tile};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use wgpu::util::DeviceExt;

struct Thumbnail {
    revision: u64,
    frame: Rect,
    clip: bool,
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    bounds: wgpu::Buffer,
    id: egui::TextureId,
}
struct CpuTile {
    owner: Weak<Tile>,
    pixels: wgpu::Texture,
    bounds: Option<(u16, u16, u16, u16)>,
}
pub(super) struct LayerThumbnails {
    layout: wgpu::BindGroupLayout,
    clear: wgpu::ComputePipeline,
    paint: wgpu::ComputePipeline,
    reduce: wgpu::ComputePipeline,
    resolve: wgpu::ComputePipeline,
    entries: HashMap<(u64, u64), Thumbnail>,
    cpu: HashMap<usize, CpuTile>,
    display: Option<DisplayPipeline>,
}
impl LayerThumbnails {
    pub(super) fn new(device: &wgpu::Device) -> Self {
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pc_layer_thumbnail"),
            entries: &[
                texture(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture(4),
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_layer_thumbnail"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu_thumbnail.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_layer_thumbnail"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pl),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        Self {
            layout,
            clear: pipeline("clear"),
            paint: pipeline("paint"),
            reduce: pipeline("reduce_bounds"),
            resolve: pipeline("resolve_frame"),
            entries: HashMap::new(),
            cpu: HashMap::new(),
            display: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        renderer: &mut egui_wgpu::Renderer,
        key: (u64, u64),
        revision: u64,
        surface: &Surface,
        frame: Rect,
        clip: bool,
    ) -> Option<egui::TextureId> {
        if surface.tiles().any(|(_, t)| t.backend().is_some_and(|b| !b.as_any().is::<photocraft_gpu::tile::GpuTileSnapshot>())) {
            return None;
        }
        if let Some(entry) = self.entries.get(&key)
            && entry.revision == revision
            && entry.frame == frame
            && entry.clip == clip
        {
            return Some(entry.id);
        }
        let format = surface.format();
        let default_native = surface.default_pixel();
        let default = photocraft_raster::to_rgba(&format, &default_native);
        let sample = match format.sample {
            photocraft_doc::SampleType::U8 => 0,
            photocraft_doc::SampleType::U16 => 1,
            photocraft_doc::SampleType::F32 => 2,
        };
        let encoded = |v: f32| match sample {
            0 => (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32,
            1 => (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u32,
            _ => v.to_bits(),
        };
        let nc = format.mode.color_channels();
        let mut native_default = [0u32; 4];
        for (out, v) in native_default.iter_mut().zip(&default_native[..nc]) {
            *out = encoded(*v);
        }
        let default_alpha = if format.alpha { encoded(default_native[nc]) } else { 0 };
        self.cpu.retain(|_, t| t.owner.strong_count() > 0);
        let mut cpu_bounds = Rect::EMPTY;
        for (coord, tile) in surface.tiles().filter(|(_, t)| t.backend().is_none()) {
            let key = Arc::as_ptr(tile) as usize;
            let entry = self.cpu.entry(key).or_insert_with(|| {
                // CPU tiles are uploaded once per immutable tile identity. GPU tile pixels
                // never enter this loop, even on a layer mixing both storage types.
                let mut pixels = Vec::with_capacity((TILE_SIZE * TILE_SIZE * 16) as usize);
                let mut native = [0.0; 8];
                let bytes = tile.bytes();
                for i in 0..(TILE_SIZE * TILE_SIZE) as usize {
                    for (ch, out) in native.iter_mut().take(format.channels()).enumerate() {
                        *out = photocraft_color::read_sample(bytes, format.sample, i * format.channels() + ch);
                    }
                    for v in photocraft_raster::to_rgba(&format, &native[..format.channels()]) {
                        pixels.extend_from_slice(&v.to_ne_bytes());
                    }
                }
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("pc_thumbnail_cpu_tile"),
                    size: wgpu::Extent3d { width: TILE_SIZE as u32, height: TILE_SIZE as u32, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba32Float,
                    usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                queue.write_texture(
                    texture.as_image_copy(),
                    &pixels,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(TILE_SIZE as u32 * 16), rows_per_image: Some(TILE_SIZE as u32) },
                    texture.size(),
                );
                let mut dp = vec![0; format.bytes_per_pixel()];
                for (ch, &v) in default_native.iter().enumerate() {
                    photocraft_color::write_sample(&mut dp, format.sample, ch, v);
                }
                CpuTile { owner: Arc::downgrade(tile), pixels: texture, bounds: tile.content_bounds(&dp) }
            });
            if let Some((x0, y0, x1, y1)) = entry.bounds {
                cpu_bounds = cpu_bounds.union(&Rect::new(x0.into(), y0.into(), x1.into(), y1.into()).translate(coord.rect().x0, coord.rect().y0));
            }
        }
        self.entries.entry(key).or_insert_with(|| {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("pc_layer_thumbnail"),
                size: wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let id = renderer.register_native_texture(device, &view, wgpu::FilterMode::Linear);
            let bounds = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pc_thumbnail_frame"),
                size: 32,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            Thumbnail { revision, frame, clip, _texture: texture, view, bounds, id }
        });
        let entry = self.entries.get(&key)?;
        let initial =
            if cpu_bounds.is_empty() { [i32::MAX, i32::MAX, i32::MIN, i32::MIN] } else { [cpu_bounds.x0, cpu_bounds.y0, cpu_bounds.x1, cpu_bounds.y1] };
        let data: Vec<_> = initial.into_iter().chain([frame.x0, frame.y0, frame.width() as i32, frame.height() as i32]).flat_map(|v| v.to_ne_bytes()).collect();
        queue.write_buffer(&entry.bounds, 0, &data);
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pc_layer_thumbnail") });
        let dummy = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pc_thumb_default"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let dummy = dummy.create_view(&Default::default());
        if clip {
            for (coord, tile) in surface.tiles() {
                let Some(snapshot) = tile.backend().and_then(|b| b.as_any().downcast_ref::<photocraft_gpu::tile::GpuTileSnapshot>()) else {
                    continue;
                };
                let source = snapshot.native_color().unwrap_or_else(|| snapshot.pixels()).create_view(&Default::default());
                let alpha = snapshot.native_alpha().unwrap_or_else(|| snapshot.pixels()).create_view(&Default::default());
                let alpha_kind = if !format.alpha {
                    0
                } else if snapshot.native_alpha().is_some() {
                    1
                } else {
                    2
                };
                self.dispatch(
                    device,
                    &mut encoder,
                    &self.reduce,
                    &source,
                    &alpha,
                    entry,
                    frame,
                    [coord.rect().x0, coord.rect().y0],
                    [0, 0, TILE_SIZE as u32, TILE_SIZE as u32],
                    default,
                    native_default,
                    [sample, nc as u32, alpha_kind, default_alpha],
                );
            }
            self.dispatch(device, &mut encoder, &self.resolve, &dummy, &dummy, entry, frame, [0, 0], [0, 0, 1, 1], default, native_default, [0; 4]);
        }
        self.dispatch(device, &mut encoder, &self.clear, &dummy, &dummy, entry, frame, [0, 0], [0, 0, 64, 64], default, native_default, [0; 4]);
        for (coord, tile) in surface.tiles() {
            let source = if let Some(snapshot) = tile.backend().and_then(|b| b.as_any().downcast_ref::<photocraft_gpu::tile::GpuTileSnapshot>()) {
                snapshot.pixels()
            } else {
                &self.cpu[&(Arc::as_ptr(tile) as usize)].pixels
            };
            // The resolved frame stays on device; dispatching 64px permits exact sampling
            // without fetching even its four bounding coordinates back to the CPU.
            self.dispatch(
                device,
                &mut encoder,
                &self.paint,
                &source.create_view(&Default::default()),
                &dummy,
                entry,
                frame,
                [coord.rect().x0, coord.rect().y0],
                [0, 0, 64, 64],
                default,
                native_default,
                [0; 4],
            );
        }
        let id = entry.id;
        queue.submit([encoder.finish()]);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.revision = revision;
            entry.frame = frame;
            entry.clip = clip;
        }
        Some(id)
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        pipeline: &wgpu::ComputePipeline,
        source: &wgpu::TextureView,
        alpha: &wgpu::TextureView,
        entry: &Thumbnail,
        frame: Rect,
        tile: [i32; 2],
        window: [u32; 4],
        default: [f32; 4],
        native_default: [u32; 4],
        kind: [u32; 4],
    ) {
        let mut bytes = Vec::with_capacity(96);
        for v in [frame.x0, frame.y0, frame.width() as i32, frame.height() as i32, tile[0], tile[1], 0, 0] {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        for v in window {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        for v in default {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        for v in native_default.into_iter().chain(kind) {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_thumb_params"),
            contents: &bytes,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_layer_thumbnail"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(source) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&entry.view) },
                wgpu::BindGroupEntry { binding: 2, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: entry.bounds.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(alpha) },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_layer_thumbnail"), timestamp_writes: None });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(window[2].div_ceil(8), window[3].div_ceil(8), 1);
    }

    pub(super) fn paint_callback(
        &mut self,
        device: &wgpu::Device,
        target: wgpu::TextureFormat,
        key: (u64, u64),
        rect: egui::Rect,
    ) -> Option<egui::PaintCallback> {
        let entry = self.entries.get(&key)?;
        let display = self.display.get_or_insert_with(|| DisplayPipeline::new(device, target));
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pc_thumb_display_view"),
            size: 48,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_thumb_display"),
            layout: &display.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: entry.bounds.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&entry.view) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&display.sampler) },
            ],
        });
        Some(egui_wgpu::Callback::new_paint_callback(
            rect,
            ThumbnailCallback { rect, uniform, bind, pipeline: display.pipeline.clone(), linear: target.is_srgb() },
        ))
    }
    pub(super) fn retain(&mut self, renderer: &mut egui_wgpu::Renderer, live: &HashSet<photocraft_doc::LayerId>) {
        self.entries.retain(|(_, layer), entry| {
            let keep = live.contains(&photocraft_doc::LayerId(*layer));
            if !keep {
                renderer.free_texture(&entry.id);
            }
            keep
        });
        self.cpu.retain(|_, t| t.owner.strong_count() > 0);
    }
    pub(super) fn free(self, renderer: &mut egui_wgpu::Renderer) {
        for entry in self.entries.into_values() {
            renderer.free_texture(&entry.id);
        }
    }
}

struct DisplayPipeline {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
}
impl DisplayPipeline {
    fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let vis = wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pc_thumb_display"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: vis,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: vis,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_thumb_display"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu_thumbnail_display.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_thumb_display"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline =
            super::pipeline(device, "pc_thumb_display", &pl, &module, ("vertex", "fragment"), target, Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING));
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pc_thumb_display"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self { layout, pipeline, sampler }
    }
}
struct ThumbnailCallback {
    rect: egui::Rect,
    uniform: wgpu::Buffer,
    bind: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    linear: bool,
}
impl egui_wgpu::CallbackTrait for ThumbnailCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        _resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let data = [
            screen.size_in_pixels[0] as f32,
            screen.size_in_pixels[1] as f32,
            screen.pixels_per_point,
            u8::from(self.linear) as f32,
            self.rect.min.x,
            self.rect.min.y,
            self.rect.width(),
            self.rect.height(),
            250.0 / 255.0,
            214.0 / 255.0,
            5.0,
            0.0,
        ];
        let bytes: Vec<_> = data.into_iter().flat_map(|v| v.to_ne_bytes()).collect();
        queue.write_buffer(&self.uniform, 0, &bytes);
        Vec::new()
    }
    fn paint(&self, info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, _resources: &egui_wgpu::CallbackResources) {
        let [w, h] = info.screen_size_px;
        let clip = info.clip_rect_in_pixels();
        if w == 0 || h == 0 || clip.width_px <= 0 || clip.height_px <= 0 {
            return;
        }
        pass.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
        pass.set_scissor_rect(clip.left_px.max(0) as u32, clip.top_px.max(0) as u32, clip.width_px as u32, clip.height_px as u32);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind, &[]);
        pass.draw(0..6, 0..2);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_thumbnail_matches_cpu_samples_without_materializing_tiles() {
        let _gpu = photocraft_testkit::gpu_lock();
        let rs =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| egui_kittest::wgpu::create_render_state(super::super::wgpu_setup(), Default::default())));
        let Ok(rs) = rs else {
            eprintln!("skipping thumbnail comparison: no GPU adapter");
            return;
        };
        let (device, queue) = (&rs.device, &rs.queue);
        let side = TILE_SIZE as u32;
        let source = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("thumbnail_test_source"),
            size: wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let mut values = Vec::new();
        for y in 0..side {
            for x in 0..side {
                values.extend([x as f32 / 255.0, y as f32 / 255.0, 0.7, ((x + y) % 256) as f32 / 255.0]);
            }
        }
        let bytes: Vec<_> = values.iter().flat_map(|v| v.to_ne_bytes()).collect();
        queue.write_texture(
            source.as_image_copy(),
            &bytes,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(side * 16), rows_per_image: Some(side) },
            wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
        );
        let mut encoder = device.create_command_encoder(&Default::default());
        let tile =
            photocraft_gpu::tile::TileSnapshots::new(device).capture(device, queue, &mut encoder, &source, [0, 0], photocraft_doc::PixelFormat::RGBA8).unwrap();
        queue.submit([encoder.finish()]);
        let coord = TileCoord { tx: -1, ty: 0 };
        let mut surface = Surface::new(photocraft_doc::PixelFormat::RGBA8);
        let mut reference = surface.clone();
        surface.set_tile(coord, tile.clone());
        reference.write_region(coord.rect(), &values);
        let frame = Rect::new(-200, 16, 220, 136);
        let mut thumbnails = LayerThumbnails::new(device);
        for mixed in [false, true] {
            if mixed {
                let rect = Rect::new(0, 16, 220, 136);
                let pixels: Vec<_> = (0..rect.width() * rect.height()).flat_map(|_| [0.15, 0.8, 0.35, 0.6]).collect();
                surface.write_region(rect, &pixels);
                reference.write_region(rect, &pixels);
            }
            for clip in [false, true] {
                thumbnails.render(device, queue, &mut rs.renderer.write(), (1, 1), u64::from(mixed), &surface, frame, clip).unwrap();
                // Construct the real render pipeline as well as all four compute pipelines.
                assert!(
                    thumbnails.paint_callback(device, rs.target_format, (1, 1), egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(32.0, 32.0))).is_some()
                );
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("thumbnail_test_readback"),
                    size: 64 * 64 * 4 + 32,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                let mut encoder = device.create_command_encoder(&Default::default());
                encoder.copy_texture_to_buffer(
                    thumbnails.entries[&(1, 1)]._texture.as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(64) },
                    },
                    wgpu::Extent3d { width: 64, height: 64, depth_or_array_layers: 1 },
                );
                encoder.copy_buffer_to_buffer(&thumbnails.entries[&(1, 1)].bounds, 0, &buffer, 64 * 64 * 4, 32);
                queue.submit([encoder.finish()]);
                buffer.slice(..).map_async(wgpu::MapMode::Read, |result| result.unwrap());
                device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).unwrap();
                let data = buffer.slice(..).get_mapped_range().unwrap();
                let bounds = reference.content_bounds().intersect(&frame);
                let expected_frame = if clip && !bounds.is_empty() { bounds } else { frame };
                let expected = crate::thumb_image(expected_frame, 64, |x, y| reference.rgba(x, y));
                let expected: Vec<_> = expected.pixels.iter().flat_map(|p| p.to_array()).collect();
                assert_eq!(&data[..64 * 64 * 4], expected.as_slice(), "mixed={mixed} clip={clip}");
                let actual: Vec<_> = data[64 * 64 * 4 + 16..].chunks_exact(4).map(|b| i32::from_ne_bytes(b.try_into().unwrap())).collect();
                assert_eq!(actual, [expected_frame.x0, expected_frame.y0, expected_frame.width() as i32, expected_frame.height() as i32]);
                assert!(!tile.is_cpu_materialized());
            }
        }
        thumbnails.free(&mut rs.renderer.write());
    }
}
