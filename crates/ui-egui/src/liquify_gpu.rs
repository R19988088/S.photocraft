//! Resident full-resolution layer sources and inverse-field GPU preview.
use eframe::egui_wgpu::{self, CallbackResources, CallbackTrait, RenderState, ScreenDescriptor, wgpu};
use photocraft_algo::liquify::{LiquifyField, LiquifyGpuOp, LiquifyTool};
use photocraft_doc::LayerId;
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone)]
pub(super) struct Source {
    pub id: LayerId,
    pub surface: Surface,
    pub mask: Option<Surface>,
    pub linked: bool,
    pub stationary: Option<Surface>,
    pub feather: f32,
    pub density: f32,
}
struct Layer {
    rect: Rect,
    id: LayerId,
    bind: wgpu::BindGroup,
    output: wgpu::Texture,
    alpha: wgpu::Texture,
    mask_pass: wgpu::BindGroup,
    mask_value: wgpu::Texture,
    feather: f32,
    mask_scratch: [wgpu::Texture; 2],
}
pub(super) struct Preview {
    pipeline: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    field: wgpu::Texture,
    linear: bool,
    snapshot: wgpu::Texture,
    compute: wgpu::ComputePipeline,
    compute_layout: wgpu::BindGroupLayout,
    layers: Vec<Layer>,
    content: wgpu::RenderPipeline,
    native_color: wgpu::RenderPipeline,
    native_alpha: wgpu::RenderPipeline,
    native_mask: wgpu::RenderPipeline,
    originals: Vec<Source>,
    alpha: wgpu::RenderPipeline,
    canvas: Rect,
    revision: AtomicU64,
    raw_mask: wgpu::RenderPipeline,
    feather: photocraft_gpu::cached_feather::Feather,
    moved_pipeline: wgpu::ComputePipeline,
    moved_bind: wgpu::BindGroup,
    moved_uniform: wgpu::Buffer,
}
fn bytes(data: impl IntoIterator<Item = f32>) -> Vec<u8> {
    data.into_iter().flat_map(f32::to_le_bytes).collect()
}
fn texture(device: &wgpu::Device, canvas: Rect, format: wgpu::TextureFormat, output: bool) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("liquify resident pixels"),
        size: wgpu::Extent3d { width: canvas.width(), height: canvas.height(), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC
            | if output {
                wgpu::TextureUsages::RENDER_ATTACHMENT
                    | if format == wgpu::TextureFormat::R32Float { wgpu::TextureUsages::STORAGE_BINDING } else { wgpu::TextureUsages::empty() }
            } else {
                wgpu::TextureUsages::empty()
            },
        view_formats: &[],
    })
}
fn upload(rs: &RenderState, canvas: Rect, source: Option<&Surface>, mode: u8) -> wgpu::Texture {
    let side = photocraft_gpu::PAGE.min(rs.device.limits().max_texture_dimension_2d);
    let cols = canvas.width().div_ceil(side);
    let rows = canvas.height().div_ceil(side);
    let t = rs.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("liquify paged immutable source"),
        size: wgpu::Extent3d { width: side.min(canvas.width()), height: side.min(canvas.height()), depth_or_array_layers: cols * rows },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: if mode == 1 { wgpu::TextureFormat::Rgba32Float } else { wgpu::TextureFormat::R32Float },
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for py in 0..rows {
        for px in 0..cols {
            let x = canvas.x0 + (px * side) as i32;
            let y = canvas.y0 + (py * side) as i32;
            let w = side.min((canvas.x1 - x) as u32);
            let h = side.min((canvas.y1 - y) as u32);
            for dy in 0..h {
                let raw = source.map(|source| source.read_region(Rect::new(x, y + dy as i32, x + w as i32, y + dy as i32 + 1)));
                let format = source.map(|s| s.format());
                let mut values = Vec::with_capacity(w as usize * if mode == 1 { 4 } else { 1 });
                for i in 0..w as usize {
                    let pixel = raw.as_ref().zip(format).map(|(v, f)| &v[i * f.channels()..(i + 1) * f.channels()]);
                    let alpha = pixel.zip(format).map_or(1., |(v, f)| if f.alpha { v[f.channels() - 1] } else { 1. });
                    if mode == 1 {
                        for c in 0..4 {
                            values.push(pixel.zip(format).map_or(0., |(v, f)| if c < f.channels() - usize::from(f.alpha) { v[c] } else { 0. }));
                        }
                    } else if mode == 2 {
                        values.push(alpha);
                    } else {
                        values.push(pixel.and_then(|v| v.first()).copied().unwrap_or(1.));
                    }
                }
                let data = bytes(values);
                rs.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &t,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: 0, y: dy, z: py * cols + px },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &data,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * if mode == 1 { 16 } else { 4 }), rows_per_image: Some(1) },
                    wgpu::Extent3d { width: w, height: 1, depth_or_array_layers: 1 },
                );
            }
        }
    }
    t
}
fn array_view(t: &wgpu::Texture) -> wgpu::TextureView {
    t.create_view(&wgpu::TextureViewDescriptor { dimension: Some(wgpu::TextureViewDimension::D2Array), ..Default::default() })
}

fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}
impl Preview {
    pub(super) fn new(rs: &RenderState, sources: Vec<Source>, selection: Option<&Surface>, canvas: Rect, field: &LiquifyField) -> Result<Arc<Self>, String> {
        let device = &rs.device;
        let (w, h) = (canvas.width(), canvas.height());
        if w == 0 || h == 0 || w.div_ceil(photocraft_gpu::PAGE) * h.div_ceil(photocraft_gpu::PAGE) > device.limits().max_texture_array_layers {
            return Err("Liquify image exceeds GPU resident page capacity".into());
        }
        if sources.is_empty() {
            return Err("Liquify has no pixel sources".into());
        }
        let side = photocraft_gpu::PAGE.min(device.limits().max_texture_dimension_2d);
        let field_cols = (field.w as u32).div_ceil(side);
        let field_rows = (field.h as u32).div_ceil(side);
        if field_cols * field_rows > device.limits().max_texture_array_layers {
            return Err("Liquify displacement exceeds GPU page capacity".into());
        }
        let field_texture = || {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("liquify paged displacement"),
                size: wgpu::Extent3d { width: side.min(field.w as u32), height: side.min(field.h as u32), depth_or_array_layers: field_cols * field_rows },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
        };
        let field_buffer = field_texture();
        let field_view = array_view(&field_buffer);
        use wgpu::util::DeviceExt;
        let tiles = [w.div_ceil(256), h.div_ceil(256)];
        let moved = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("liquify changed native tiles"),
            size: u64::from(tiles[0]) * u64::from(tiles[1]) * 4,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let moved_params = bytes([field.w as f32, field.h as f32, field.cell as f32, 0.])
            .into_iter()
            .chain([w, h, tiles[0], 0].into_iter().flat_map(u32::to_le_bytes))
            .collect::<Vec<_>>();
        let moved_uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("liquify changed tile dimensions"),
            contents: &moved_params,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let moved_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("liquify exact tile reduction"),
            source: wgpu::ShaderSource::Wgsl(include_str!("liquify_tiles.wgsl").into()),
        });
        let moved_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("liquify exact tile reduction"),
            layout: None,
            module: &moved_shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let moved_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("liquify exact tile reduction"),
            layout: &moved_pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: moved_uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&field_view) },
                wgpu::BindGroupEntry { binding: 2, resource: moved.as_entire_binding() },
            ],
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("liquify view"),
            size: 112,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut entries = vec![
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            },
        ];
        for binding in [1, 3, 4, 5, 6, 10] {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: if binding == 6 { wgpu::TextureViewDimension::D2 } else { wgpu::TextureViewDimension::D2Array },
                    multisampled: false,
                },
                count: None,
            });
        }
        for binding in [7, 8, 9, 11] {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: if binding == 9 { wgpu::BufferBindingType::Uniform } else { wgpu::BufferBindingType::Storage { read_only: true } },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            });
        }
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("liquify sampled sources"), entries: &entries });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("liquify preview"),
            source: wgpu::ShaderSource::Wgsl((photocraft_gpu::brush_format::NativeFormat::shader(0, 7, 8) + include_str!("liquify_preview.wgsl")).into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("liquify source sampling"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry, format, blend| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("liquify cached sampler"),
                layout: Some(&pl),
                vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let preview_pipeline = pipeline("fs_overlay", rs.target_format, Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING));
        let content = pipeline("fs_layer", wgpu::TextureFormat::Rgba32Float, None);
        let alpha = pipeline("fs_alpha", wgpu::TextureFormat::R32Float, None);
        let raw_mask = pipeline("fs_mask", wgpu::TextureFormat::R32Float, None);
        let native_color = pipeline("fs_native_color", wgpu::TextureFormat::Rgba32Float, None);
        let native_alpha = pipeline("fs_native_alpha", wgpu::TextureFormat::R32Float, None);
        let native_mask = pipeline("fs_native_mask", wgpu::TextureFormat::R32Float, None);
        let feather = photocraft_gpu::cached_feather::Feather::new(device);
        let has_selection = selection.is_some();
        let selection = upload(rs, canvas, selection, 0);
        let mut layers = Vec::new();
        let mut overlay_bind = None;
        let native_pipelines = Arc::new(photocraft_gpu::brush_format::NativeFormatPipelines::new(device));
        let originals = sources.clone();
        for s in sources {
            let source = upload(rs, canvas, Some(&s.surface), 1);
            let native = photocraft_gpu::brush_format::NativeFormat::with_pipelines(device, s.surface.format(), native_pipelines.clone())?;
            let native_alpha = upload(rs, canvas, Some(&s.surface), 2);
            let sample = |f: photocraft_color::PixelFormat| match f.sample {
                photocraft_color::SampleType::U8 => 0u32,
                photocraft_color::SampleType::U16 => 1,
                _ => 2,
            };
            use wgpu::util::DeviceExt;
            let native_params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("liquify native format"),
                contents: &[
                    native.mode(),
                    sample(native.format),
                    s.mask.as_ref().map_or(2, |m| sample(m.format())),
                    u32::from(has_selection) | (u32::from(native.format.alpha) * 2),
                ]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            let mask = upload(rs, canvas, s.mask.as_ref(), 0);
            let stationary = upload(rs, canvas, s.stationary.as_ref(), 0);
            let side = photocraft_gpu::PAGE.min(device.limits().max_texture_dimension_2d);
            let sigma = photocraft_compose::masks::feather_sigma(s.feather);
            // Three horizontal and three vertical extended boxes have this finite support.
            let halo = (sigma * 3.).ceil() as i32 + 6;
            for py in 0..h.div_ceil(side) {
                for px in 0..w.div_ceil(side) {
                    let rect = Rect::new(
                        canvas.x0 + (px * side) as i32,
                        canvas.y0 + (py * side) as i32,
                        (canvas.x0 + ((px + 1) * side) as i32).min(canvas.x1),
                        (canvas.y0 + ((py + 1) * side) as i32).min(canvas.y1),
                    );
                    let mask_rect = if sigma > 0. { rect.inflate(halo).intersect(&canvas) } else { rect };
                    if mask_rect.width().max(mask_rect.height()) > device.limits().max_texture_dimension_2d {
                        return Err("Liquify mask feather exceeds GPU page halo capacity".into());
                    }
                    let mask_value = texture(device, mask_rect, wgpu::TextureFormat::R32Float, true);
                    let make_uniform = |r: Rect| {
                        let data = [
                            0.,
                            0.,
                            r.width() as f32,
                            r.height() as f32,
                            r.width() as f32,
                            r.height() as f32,
                            w as f32,
                            h as f32,
                            canvas.x0 as f32,
                            canvas.y0 as f32,
                            field.cell as f32,
                            0.,
                            field.w as f32,
                            field.h as f32,
                            if s.linked { 1. } else { 0. },
                            0.,
                            s.density,
                            0.,
                            0.,
                            0.,
                            (r.x0 - canvas.x0) as f32,
                            (r.y0 - canvas.y0) as f32,
                            r.width() as f32,
                            r.height() as f32,
                            (mask_rect.x0 - canvas.x0) as f32,
                            (mask_rect.y0 - canvas.y0) as f32,
                            mask_rect.width() as f32,
                            mask_rect.height() as f32,
                        ];
                        let u = device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("liquify page view"),
                            size: 112,
                            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        });
                        rs.queue.write_buffer(&u, 0, &bytes(data));
                        u
                    };
                    let view_uniform = make_uniform(rect);
                    let mask_uniform = make_uniform(mask_rect);
                    let views = [
                        array_view(&source),
                        array_view(&selection),
                        array_view(&mask),
                        array_view(&stationary),
                        mask_value.create_view(&Default::default()),
                        array_view(&native_alpha),
                    ];
                    // Binding 6 is not read by mask/overlay shaders; bind a separate 2D dummy to avoid render feedback.
                    let dummy = texture(device, Rect::new(0, 0, 1, 1), wgpu::TextureFormat::R32Float, false);
                    let dummy_view = dummy.create_view(&Default::default());
                    let make_bind = |uniform: &wgpu::Buffer, resolved: bool| {
                        device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("liquify immutable paged source"),
                            layout: &layout,
                            entries: &[
                                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&views[0]) },
                                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&field_view) },
                                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&views[1]) },
                                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&views[2]) },
                                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&views[3]) },
                                wgpu::BindGroupEntry {
                                    binding: 6,
                                    resource: wgpu::BindingResource::TextureView(if resolved { &views[4] } else { &dummy_view }),
                                },
                                wgpu::BindGroupEntry { binding: 7, resource: native.info.as_entire_binding() },
                                wgpu::BindGroupEntry { binding: 8, resource: native.values.as_entire_binding() },
                                wgpu::BindGroupEntry { binding: 9, resource: native_params.as_entire_binding() },
                                wgpu::BindGroupEntry { binding: 10, resource: wgpu::BindingResource::TextureView(&views[5]) },
                                wgpu::BindGroupEntry { binding: 11, resource: moved.as_entire_binding() },
                            ],
                        })
                    };
                    if overlay_bind.is_none() {
                        overlay_bind = Some(make_bind(&uniform, false));
                    }
                    layers.push(Layer {
                        id: s.id,
                        rect,
                        bind: make_bind(&view_uniform, true),
                        output: texture(device, rect, wgpu::TextureFormat::Rgba32Float, true),
                        alpha: texture(device, rect, wgpu::TextureFormat::R32Float, true),
                        mask_pass: make_bind(&mask_uniform, false),
                        mask_value,
                        feather: s.feather,
                        mask_scratch: [
                            texture(device, mask_rect, wgpu::TextureFormat::R32Float, true),
                            texture(device, mask_rect, wgpu::TextureFormat::R32Float, true),
                        ],
                    });
                }
            }
        }
        let bind = overlay_bind.ok_or("Liquify has no source binding")?;
        let snapshot = field_texture();
        let compute_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("liquify paged brush"),
            entries: &[
                storage_entry(0, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba32Float,
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                    },
                    count: None,
                },
            ],
        });
        let cl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("liquify brush"),
            bind_group_layouts: &[Some(&compute_layout)],
            immediate_size: 0,
        });
        let cs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("liquify GPU brush"),
            source: wgpu::ShaderSource::Wgsl(include_str!("liquify_brush.wgsl").into()),
        });
        let compute = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("liquify GPU brush"),
            layout: Some(&cl),
            module: &cs,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        Ok(Arc::new(Self {
            pipeline: preview_pipeline,
            bind,
            uniform,
            field: field_buffer,
            linear: rs.target_format.is_srgb(),
            snapshot,
            compute,
            compute_layout,
            layers,
            content,
            native_color,
            native_alpha,
            native_mask,
            originals,
            alpha,
            canvas,
            revision: AtomicU64::new(0),
            raw_mask,
            feather,
            moved_pipeline,
            moved_bind,
            moved_uniform,
        }))
    }
    fn render_mask(&self, rs: &RenderState, encoder: &mut wgpu::CommandEncoder, layer: &Layer) {
        let [first, second] = &layer.mask_scratch;
        let draw = |encoder: &mut wgpu::CommandEncoder, target: &wgpu::Texture, pipeline: &wgpu::RenderPipeline, bind: &wgpu::BindGroup| {
            let view = target.create_view(&Default::default());
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("liquify mask feather"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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
            pass.set_bind_group(0, bind, &[]);
            pass.draw(0..6, 0..1);
        };
        // Raw mask is warped and selection-mixed before the Properties feather is evaluated.
        let sigma = photocraft_compose::masks::feather_sigma(layer.feather);
        if sigma <= 0. {
            draw(encoder, &layer.mask_value, &self.raw_mask, &layer.mask_pass);
            return;
        }
        draw(encoder, first, &self.raw_mask, &layer.mask_pass);
        self.feather.encode(&rs.device, encoder, [first, second, &layer.mask_value], sigma);
    }

    pub(super) fn render_outputs(&self, rs: &RenderState, damage: Option<Rect>) -> HashMap<LayerId, photocraft_gpu::SourceOverride> {
        let mut encoder = rs.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("liquify cached layer sampling") });
        {
            let region = damage.unwrap_or(self.canvas).intersect(&self.canvas);
            let x = (region.x0 - self.canvas.x0).max(0) as u32 / 256;
            let y = (region.y0 - self.canvas.y0).max(0) as u32 / 256;
            rs.queue.write_buffer(&self.moved_uniform, 12, &(y as f32).to_le_bytes());
            rs.queue.write_buffer(&self.moved_uniform, 28, &x.to_le_bytes());
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("liquify identify changed tiles"), timestamp_writes: None });
            pass.set_pipeline(&self.moved_pipeline);
            pass.set_bind_group(0, &self.moved_bind, &[]);
            pass.dispatch_workgroups(
                ((region.x1 - self.canvas.x0).max(0) as u32).div_ceil(256).saturating_sub(x),
                ((region.y1 - self.canvas.y0).max(0) as u32).div_ceil(256).saturating_sub(y),
                1,
            );
        }
        // Native replay rewrites complete changed raster tiles, including their F32 clamp.
        let damage = damage.map(|r| {
            Rect::new(r.x0.div_euclid(256) * 256, r.y0.div_euclid(256) * 256, (r.x1 + 255).div_euclid(256) * 256, (r.y1 + 255).div_euclid(256) * 256)
                .intersect(&self.canvas)
        });
        let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
        let mut out = HashMap::new();
        for layer in &self.layers {
            let damage = damage.map(|r| r.inflate((layer.feather * 3.).ceil() as i32 + 3).intersect(&self.canvas));
            if damage.is_none_or(|r| !r.intersect(&layer.rect).is_empty()) {
                self.render_mask(rs, &mut encoder, layer);
                for (texture, pipeline) in [(&layer.output, &self.content), (&layer.alpha, &self.alpha)] {
                    let view = texture.create_view(&Default::default());
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("liquify sample source once"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            depth_slice: None,
                            ops: wgpu::Operations {
                                load: if damage.is_some() { wgpu::LoadOp::Load } else { wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT) },
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    if let Some(damage) = damage.map(|d| d.intersect(&layer.rect)) {
                        if damage.is_empty() {
                            continue;
                        }
                        pass.set_scissor_rect((damage.x0 - layer.rect.x0) as u32, (damage.y0 - layer.rect.y0) as u32, damage.width(), damage.height());
                    }
                    pass.set_pipeline(pipeline);
                    pass.set_bind_group(0, &layer.bind, &[]);
                    pass.draw(0..6, 0..1);
                }
            }
            let source = out.entry(layer.id).or_insert_with(|| photocraft_gpu::SourceOverride {
                texture: layer.output.clone(),
                shape: layer.alpha.clone(),
                mask: None,
                pages: Vec::new(),
                rect: self.canvas,
                bounds: self.canvas,
                revision,
            });
            source.pages.push(photocraft_gpu::SourceOverridePage { texture: layer.output.clone(), shape: layer.alpha.clone(), mask: None, rect: layer.rect });
        }
        for source in out.values_mut() {
            if source.pages.len() == 1 {
                source.pages.clear();
            }
        }
        rs.queue.submit([encoder.finish()]);
        out
    }
    /// Final native pixels leave the GPU only here, after explicit confirmation.
    #[allow(clippy::type_complexity)]
    pub(super) fn commit_surfaces(&self, rs: &RenderState) -> Result<(HashMap<LayerId, Surface>, HashMap<LayerId, Surface>), String> {
        struct Read {
            id: LayerId,
            rect: Rect,
            kind: u8,
            stride: u32,
            buffer: wgpu::Buffer,
        }
        let mut encoder = rs.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("liquify final native commit") });
        let mut reads = Vec::new();
        for layer in &self.layers {
            let linked = self.originals.iter().find(|s| s.id == layer.id).is_some_and(|s| s.linked && s.mask.is_some());
            for (kind, pipeline, format) in [
                (0, &self.native_color, wgpu::TextureFormat::Rgba32Float),
                (1, &self.native_alpha, wgpu::TextureFormat::R32Float),
                (2, &self.native_mask, wgpu::TextureFormat::R32Float),
            ] {
                if kind == 2 && !linked {
                    continue;
                }
                let target = texture(&rs.device, layer.rect, format, true);
                let view = target.create_view(&Default::default());
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("liquify final native samples"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
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
                    pass.set_bind_group(0, &layer.bind, &[]);
                    pass.draw(0..6, 0..1);
                }
                let stride = (layer.rect.width() * if kind == 0 { 16 } else { 4 }).div_ceil(256) * 256;
                let buffer = rs.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("liquify final native readback"),
                    size: u64::from(stride) * u64::from(layer.rect.height()),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                encoder.copy_texture_to_buffer(
                    target.as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(layer.rect.height()) },
                    },
                    target.size(),
                );
                reads.push(Read { id: layer.id, rect: layer.rect, kind, stride, buffer });
            }
        }
        let submission = rs.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        for read in &reads {
            let tx = tx.clone();
            read.buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        }
        rs.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: None }).map_err(|e| e.to_string())?;
        for _ in &reads {
            rx.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        }
        let mut planes: HashMap<(LayerId, i32, i32), [Vec<f32>; 3]> = HashMap::new();
        for read in reads {
            let view = read.buffer.slice(..).get_mapped_range().map_err(|e| e.to_string())?;
            let channels = if read.kind == 0 { 4 } else { 1 };
            let count = read.rect.width() as usize * channels;
            let values = view
                .chunks_exact(read.stride as usize)
                .flat_map(|row| row[..count * 4].chunks_exact(4).map(|p| f32::from_le_bytes([p[0], p[1], p[2], p[3]])))
                .collect();
            planes.entry((read.id, read.rect.x0, read.rect.y0)).or_default()[read.kind as usize] = values;
            drop(view);
            read.buffer.unmap();
        }
        let mut pixels: HashMap<_, _> = self.originals.iter().map(|s| (s.id, s.surface.clone())).collect();
        let mut masks: HashMap<_, _> = self.originals.iter().filter(|s| s.linked).filter_map(|s| s.mask.clone().map(|m| (s.id, m))).collect();
        for layer in &self.layers {
            let Some([color, alpha, mask]) = planes.remove(&(layer.id, layer.rect.x0, layer.rect.y0)) else {
                return Err("missing final Liquify samples".into());
            };
            let surface = pixels.get_mut(&layer.id).ok_or("missing frozen Liquify source")?;
            let format = surface.format();
            let colors = format.channels() - usize::from(format.alpha);
            let values = color.chunks_exact(4).zip(alpha).flat_map(|(c, a)| c[..colors].iter().copied().chain(format.alpha.then_some(a))).collect::<Vec<_>>();
            surface.write_region(layer.rect, &values);
            if let Some(surface) = masks.get_mut(&layer.id) {
                surface.write_region(layer.rect, &mask);
            }
        }
        Ok((pixels, masks))
    }
    pub(super) fn apply_ops(&self, rs: &RenderState, field: &LiquifyField, ops: &[LiquifyGpuOp], reset: bool) {
        if ops.is_empty() && !reset {
            return;
        }
        let device = &rs.device;
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("liquify cached brush commands") });
        if reset {
            for page in 0..self.field.depth_or_array_layers() {
                let view = self.field.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: page,
                    array_layer_count: Some(1),
                    ..Default::default()
                });
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("reset liquid field"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            }
        }
        for op in ops {
            let tool = match op.tool {
                LiquifyTool::ForwardWarp => 0.,
                LiquifyTool::Reconstruct => 1.,
                LiquifyTool::Smooth => 2.,
                LiquifyTool::TwirlCw => 3.,
                LiquifyTool::TwirlCcw => 4.,
                LiquifyTool::Pucker => 5.,
                LiquifyTool::Bloat => 6.,
                LiquifyTool::PushLeft => 7.,
                LiquifyTool::Freeze => 8.,
                LiquifyTool::Thaw => 9.,
                LiquifyTool::LassoMask => 10.,
                LiquifyTool::ReconstructAll => 11.,
                LiquifyTool::FreezeAll => 12.,
                LiquifyTool::ThawAll => 13.,
                LiquifyTool::InvertFreeze => 14.,
            };
            let global = op.tool.is_global() || op.tool == LiquifyTool::LassoMask;
            let origin = [field.bounds.x0 as f32 + 0.5, field.bounds.y0 as f32 + 0.5];
            let cell = field.cell as f32;
            let minx = if global { 0 } else { (((op.center[0] - op.radius - origin[0] as f64) / field.cell).floor().max(0.) as usize).min(field.w) };
            let miny = if global { 0 } else { (((op.center[1] - op.radius - origin[1] as f64) / field.cell).floor().max(0.) as usize).min(field.h) };
            let maxx = if global { field.w } else { (((op.center[0] + op.radius - origin[0] as f64) / field.cell).ceil().max(0.) as usize + 1).min(field.w) };
            let maxy = if global { field.h } else { (((op.center[1] + op.radius - origin[1] as f64) / field.cell).ceil().max(0.) as usize + 1).min(field.h) };
            if maxx <= minx || maxy <= miny {
                continue;
            }
            let mut data = vec![
                field.w as f32,
                field.h as f32,
                cell,
                tool,
                origin[0],
                origin[1],
                minx as f32,
                miny as f32,
                (op.center[0] - (field.bounds.x0 as f64 + 0.5 + minx as f64 * field.cell)) as f32,
                (op.center[1] - (field.bounds.y0 as f64 + 0.5 + miny as f64 * field.cell)) as f32,
                op.delta[0] as f32,
                op.delta[1] as f32,
                op.radius as f32,
                op.strength as f32,
                op.rate as f32,
                op.density as f32,
                op.amount as f32,
                op.polygon.len() as f32,
                maxx as f32,
                maxy as f32,
            ];
            data.extend(op.polygon.iter().flat_map(|p| {
                [
                    (p[0] - (field.bounds.x0 as f64 + 0.5 + minx as f64 * field.cell)) as f32,
                    (p[1] - (field.bounds.y0 as f64 + 0.5 + miny as f64 * field.cell)) as f32,
                    0.,
                    0.,
                ]
            }));
            let data = bytes(data);
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("liquify dab parameters"),
                size: data.len() as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            rs.queue.write_buffer(&buffer, 0, &data);
            let old_view = array_view(&self.snapshot);
            let field_view = array_view(&self.field);
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("liquify dab"),
                layout: &self.compute_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&old_view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&field_view) },
                ],
            });
            // Each dab samples the previous field, never its partially updated neighbours.
            let margin = ((op.delta[0].abs().max(op.delta[1].abs()) + op.radius * 0.2) / field.cell).ceil() as usize + 2;
            let first_row = if global { 0 } else { miny.saturating_sub(margin) };
            let last_row = if global { field.h } else { maxy.saturating_add(margin).min(field.h) };
            let side_x = self.field.width();
            let side_y = self.field.height();
            let cols = (field.w as u32).div_ceil(side_x);
            for page in 0..self.field.depth_or_array_layers() {
                let py = page / cols;
                let y0 = (first_row as u32).max(py * side_y);
                let y1 = (last_row as u32).min((py + 1) * side_y);
                if y1 <= y0 {
                    continue;
                }
                let origin = wgpu::Origin3d { x: 0, y: y0 - py * side_y, z: page };
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo { texture: &self.field, mip_level: 0, origin, aspect: wgpu::TextureAspect::All },
                    wgpu::TexelCopyTextureInfo { texture: &self.snapshot, mip_level: 0, origin, aspect: wgpu::TextureAspect::All },
                    wgpu::Extent3d { width: side_x, height: y1 - y0, depth_or_array_layers: 1 },
                );
            }
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("liquify displacement only"), timestamp_writes: None });
            pass.set_pipeline(&self.compute);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(((maxx - minx) as u32).div_ceil(8), ((maxy - miny) as u32).div_ceil(8), 1);
        }
        rs.queue.submit([encoder.finish()]);
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn callback(
        self: &Arc<Self>,
        area: egui::Rect,
        image: egui::Rect,
        canvas: Rect,
        field: &LiquifyField,
        show_mask: bool,
        show_mesh: bool,
        mesh_size: usize,
    ) -> egui::PaintCallback {
        egui_wgpu::Callback::new_paint_callback(
            area,
            Paint { preview: self.clone(), image, canvas, cell: field.cell as f32, fw: field.w, fh: field.h, show_mask, show_mesh, mesh_size },
        )
    }
}
struct Paint {
    preview: Arc<Preview>,
    image: egui::Rect,
    canvas: Rect,
    cell: f32,
    fw: usize,
    fh: usize,
    show_mask: bool,
    show_mesh: bool,
    mesh_size: usize,
}
impl CallbackTrait for Paint {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        _resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let p = screen.pixels_per_point;
        let data = [
            self.image.min.x * p,
            self.image.min.y * p,
            self.image.width() * p,
            self.image.height() * p,
            screen.size_in_pixels[0] as f32,
            screen.size_in_pixels[1] as f32,
            self.canvas.width() as f32,
            self.canvas.height() as f32,
            self.canvas.x0 as f32,
            self.canvas.y0 as f32,
            self.cell,
            if self.preview.linear { 1.0 } else { 0.0 },
            self.fw as f32,
            self.fh as f32,
            0.0,
            0.0,
            if self.show_mask { 1. } else { 0. },
            if self.show_mesh { 1. } else { 0. },
            [64., 40., 24.].get(self.mesh_size).copied().unwrap_or(40.),
            0.0,
            0.,
            0.,
            self.canvas.width() as f32,
            self.canvas.height() as f32,
            0.,
            0.,
            self.canvas.width() as f32,
            self.canvas.height() as f32,
        ];
        queue.write_buffer(&self.preview.uniform, 0, &bytes(data));
        Vec::new()
    }
    fn paint(&self, info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, _resources: &CallbackResources) {
        let [w, h] = info.screen_size_px;
        if w == 0 || h == 0 {
            return;
        }
        pass.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
        pass.set_pipeline(&self.preview.pipeline);
        pass.set_bind_group(0, &self.preview.bind, &[]);
        pass.draw(0..6, 0..1);
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use photocraft_algo::liquify::{LiquifyStroke, apply_liquify};
    use photocraft_color::{BlendMode, ColorMode, PixelFormat, SampleType};
    use photocraft_doc::{Document, Layer, LayerContent};
    use photocraft_geom::Size;
    use std::{
        future::Future,
        task::{Context, Poll, Wake, Waker},
    };

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        struct ThreadWake(std::thread::Thread);
        impl Wake for ThreadWake {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => std::thread::park(),
            }
        }
    }

    #[test]
    fn native_liquify_scope_switch_keeps_an_independent_display_cache() {
        let instance = wgpu::Instance::default();
        let rs = block_on(RenderState::create(&Default::default(), &instance, None, Default::default())).expect("native GPU");
        let mut app = crate::PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        app.session.execute("file.new", serde_json::json!({"width":48,"height":40,"depth":8})).unwrap();
        app.session.execute("layer.new.layer", serde_json::json!({})).unwrap();
        app.sync_views();
        app.gpu = Some(crate::gpu_canvas::GpuCanvas::new(&rs));
        let ctx = egui::Context::default();
        super::super::open(&mut app, &ctx).unwrap();
        let document = app.session.active().unwrap().doc.id.0;
        let preview = super::super::gpu_key(&app).unwrap();
        assert_ne!(preview, document, "the normal canvas must never overwrite Liquify's resident display");
        super::super::control(&mut app, &serde_json::json!({"scope":"all"})).unwrap();
        super::super::refresh_scope(&mut app, Some(&ctx)).unwrap();
        assert_eq!(super::super::gpu_key(&app), Some(preview), "changing targets keeps the same dialog-owned display cache");
        super::super::cancel(&mut app);
        assert_eq!(super::super::gpu_key(&app), None);
    }

    #[test]
    fn native_liquify_gpu_matches_cpu_field_and_layer_composite() {
        let instance = wgpu::Instance::default();
        let rs = block_on(RenderState::create(&Default::default(), &instance, None, Default::default()))
            .expect("native GPU is required for Liquify preview acceptance");
        rs.device.on_uncaptured_error(Arc::new(|error| panic!("Liquify GPU validation: {error}")));
        eprintln!("Liquify device: {:?}", rs.adapter.get_info());
        let canvas = Rect::new(0, 0, 4112, 60);
        let mut source = Surface::new(PixelFormat::RGBA32F);
        for y in 0..60 {
            for x in 0..4112 {
                source.write_pixel(x, y, &[x as f32 / 4120., y as f32 / 70., 0.3, 0.8]);
            }
        }
        let first = Layer::new("first", LayerContent::Raster(source.clone()));
        let mut second = Layer::new("second", LayerContent::Raster(source.clone()));
        second.opacity = 0.65;
        second.blend = BlendMode::Multiply;
        let sources = vec![
            Source { id: first.id, surface: source.clone(), mask: None, linked: false, stationary: None, feather: 0., density: 1. },
            Source { id: second.id, surface: source.clone(), mask: None, linked: false, stationary: None, feather: 0., density: 1. },
        ];
        let mut doc = Document::new("gpu liquify", Size::new(4112, 60), ColorMode::Rgb, SampleType::F32);
        doc.layers = vec![first, second];
        let mut strokes = Vec::new();
        for (tool, x, y) in [
            (LiquifyTool::ForwardWarp, 30., 30.),
            (LiquifyTool::PushLeft, 40., 35.),
            (LiquifyTool::Pucker, 35., 25.),
            (LiquifyTool::Bloat, 50., 35.),
            (LiquifyTool::TwirlCw, 40., 30.),
            (LiquifyTool::TwirlCcw, 45., 35.),
            (LiquifyTool::Freeze, 35., 30.),
            (LiquifyTool::Smooth, 35., 30.),
            (LiquifyTool::Thaw, 35., 30.),
            (LiquifyTool::Reconstruct, 45., 30.),
        ] {
            let x = x + 4040.;
            let mut stroke = LiquifyStroke::new(tool, 32.);
            stroke.points = vec![vec![x, y, 0.8], vec![x + 8., y + 2., 0.7]];
            strokes.push(stroke);
        }
        let mut lasso = LiquifyStroke::new(LiquifyTool::LassoMask, 1.);
        lasso.points = vec![vec![4070., 20.], vec![4082., 20.], vec![4082., 32.], vec![4070., 32.]];
        strokes.push(lasso);
        let mut reconstruct = LiquifyStroke::new(LiquifyTool::ReconstructAll, 1.);
        reconstruct.amount = Some(20.);
        strokes.push(reconstruct);
        strokes.push(LiquifyStroke::new(LiquifyTool::InvertFreeze, 1.));
        let cpu = LiquifyField::from_strokes(canvas, 2., &strokes);
        let mut scheduled = LiquifyField::new(canvas, 2.);
        scheduled.gpu_only = true;
        for stroke in &strokes {
            scheduled.apply_stroke(stroke);
        }
        let preview = Preview::new(&rs, sources, None, canvas, &scheduled).unwrap();
        preview.apply_ops(&rs, &scheduled, &scheduled.gpu_ops, true);
        let readback = rs.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("liquify test field readback"),
            size: (scheduled.w as u64 * 16).div_ceil(256) * 256 * scheduled.h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = rs.device.create_command_encoder(&Default::default());
        let stride = (scheduled.w as u64 * 16).div_ceil(256) * 256;
        let cols = (scheduled.w as u32).div_ceil(preview.field.width());
        for page in 0..preview.field.depth_or_array_layers() {
            let x = (page % cols) * preview.field.width();
            let y = (page / cols) * preview.field.height();
            let w = preview.field.width().min(scheduled.w as u32 - x);
            let h = preview.field.height().min(scheduled.h as u32 - y);
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &preview.field,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: page },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: y as u64 * stride + x as u64 * 16,
                        bytes_per_row: Some(stride as u32),
                        rows_per_image: Some(h),
                    },
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
        rs.queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
        rs.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).unwrap();
        let mapped = readback.slice(..).get_mapped_range().unwrap();
        let values: Vec<f32> = mapped
            .chunks_exact(stride as usize)
            .flat_map(|row| row[..scheduled.w * 16].chunks_exact(4).map(|p| f32::from_le_bytes(p.try_into().unwrap())))
            .collect();
        let mut worst = 0.0f32;
        for (i, pixel) in values.chunks_exact(4).enumerate() {
            worst = worst.max((pixel[0] - cpu.d[i][0]).abs()).max((pixel[1] - cpu.d[i][1]).abs()).max((pixel[2] - cpu.freeze[i]).abs());
        }
        assert!(worst < 0.002, "GPU brush field differs by {worst}");
        drop(mapped);
        readback.unmap();
        let mut comp = photocraft_gpu::Compositor::try_new_with_format(&rs.device, wgpu::TextureFormat::Rgba32Float).unwrap();
        comp.set_source_overrides(preview.render_outputs(&rs, None));
        let rendered = photocraft_gpu::render_to_vec(&mut comp, &rs.device, &rs.queue, &doc, canvas).unwrap();
        for layer in &mut doc.layers {
            if let Some(surface) = layer.surface_mut() {
                *surface = apply_liquify(surface, &cpu);
            }
        }
        let expected = photocraft_compose::render(&doc, canvas);
        let worst = rendered.iter().zip(&expected.px).flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (a - b).abs())).fold(0.0, f32::max);
        assert!(worst < 0.003, "resident per-layer composite differs from final replay by {worst}");
        eprintln!("Liquify field and layer composite parity passed; maximum pixel error {worst}");
    }
    #[test]
    fn native_liquify_linked_feather_mask_matches_commit() {
        let instance = wgpu::Instance::default();
        let rs = block_on(RenderState::create(&Default::default(), &instance, None, Default::default())).expect("native GPU required");
        rs.device.on_uncaptured_error(Arc::new(|error| panic!("Liquify feather GPU validation: {error}")));
        let canvas = Rect::new(0, 0, 96, 80);
        let mut source = Surface::new(PixelFormat::RGBA32F);
        source.fill_rect(canvas, &[0.8, 0.3, 0.15, 0.8]);
        let mut raw_mask = Surface::new(PixelFormat::new(ColorMode::Grayscale, SampleType::F32, false));
        raw_mask.fill_rect(Rect::new(18, 14, 76, 66), &[0.9]);
        raw_mask.fill_rect(Rect::new(32, 26, 58, 54), &[0.15]);
        let mut selection = Surface::new(PixelFormat::new(ColorMode::Grayscale, SampleType::F32, false));
        selection.fill_rect(Rect::new(10, 8, 64, 74), &[0.7]);
        let mut stroke = LiquifyStroke::new(LiquifyTool::ForwardWarp, 44.);
        stroke.points = vec![vec![44., 40., 1.], vec![57., 46., 1.]];
        let cpu = LiquifyField::from_strokes(canvas, 2., &[stroke.clone()]);
        for feather in [0.7, 4., 9.] {
            let mut scheduled = LiquifyField::new(canvas, 2.);
            scheduled.gpu_only = true;
            scheduled.apply_stroke(&stroke);
            let mut layer = Layer::new("masked", LayerContent::Raster(source.clone()));
            let mut mask = photocraft_doc::LayerMask::hide_all();
            mask.surface = raw_mask.clone();
            mask.linked = true;
            mask.feather = feather;
            mask.density = 0.63;
            layer.mask = Some(mask);
            let mut doc = Document::new("masked GPU Liquify", Size::new(96, 80), ColorMode::Rgb, SampleType::F32);
            doc.layers = vec![layer.clone()];
            doc.selection = Some(selection.clone());
            let preview = Preview::new(
                &rs,
                vec![Source { id: layer.id, surface: source.clone(), mask: Some(raw_mask.clone()), linked: true, stationary: None, feather, density: 0.63 }],
                Some(&selection),
                canvas,
                &scheduled,
            )
            .unwrap();
            preview.apply_ops(&rs, &scheduled, &scheduled.gpu_ops, true);
            let mut comp = photocraft_gpu::Compositor::try_new_with_format(&rs.device, wgpu::TextureFormat::Rgba32Float).unwrap();
            comp.set_source_overrides(preview.render_outputs(&rs, None));
            let actual = photocraft_gpu::render_to_vec(&mut comp, &rs.device, &rs.queue, &doc, canvas).unwrap();
            let mut final_mask = apply_liquify(&raw_mask, &cpu);
            let warped = final_mask.read_region(canvas);
            let original = raw_mask.read_region(canvas);
            let selected = selection.read_region(canvas);
            let mixed = original.iter().zip(&warped).zip(&selected).map(|((a, b), k)| a + (b - a) * k).collect::<Vec<_>>();
            final_mask.write_region(canvas, &mixed);
            doc.layers[0].mask.as_mut().unwrap().surface = final_mask;
            // Source is constant, so its selection-limited warp has the same pixels.
            let expected = photocraft_compose::render(&doc, canvas);
            let worst = actual.iter().zip(&expected.px).flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (a - b).abs())).fold(0., f32::max);
            assert!(worst < 0.0001, "linked feather {feather} disagrees with commit: {worst}");
            eprintln!("GPU raw-mask warp, selection, density and feather {feather}: error {worst}");
        }
    }
    #[test]
    fn native_liquify_color_formats_match_engine_commit() {
        let rs = block_on(RenderState::create(&Default::default(), &wgpu::Instance::default(), None, Default::default())).unwrap();
        rs.device.on_uncaptured_error(Arc::new(|error| panic!("Liquify native colour: {error}")));
        let canvas = Rect::new(0, 0, 560, 80);
        let mut stroke = LiquifyStroke::new(LiquifyTool::ForwardWarp, 38.);
        stroke.points = vec![vec![135., 36., 0.8], vec![147., 42., 0.9]];
        for (mode, depth, selected) in [
            (ColorMode::Rgb, SampleType::U16, true),
            (ColorMode::Cmyk, SampleType::U16, true),
            (ColorMode::Lab, SampleType::U16, true),
            (ColorMode::Rgb, SampleType::F32, true),
            (ColorMode::Rgb, SampleType::F32, false),
        ] {
            let format = PixelFormat::new(mode, depth, true);
            let mut source = Surface::new(format);
            let mut values = Vec::new();
            for y in 0..80 {
                for x in 0..560 {
                    let mut rgba = [0.1 + x as f32 / 700., 0.15 + y as f32 / 100., 0.65 - x as f32 / 1000., 0.25 + ((x + y) % 23) as f32 / 35.];
                    if depth == SampleType::F32 && x > 280 {
                        rgba[0] = 1.3;
                        rgba[2] = -0.15;
                    }
                    if !selected && x == 350 && y == 10 {
                        rgba[3] = 0.;
                    }
                    values.extend(photocraft_raster::from_rgba(&format, rgba));
                }
            }
            source.write_region(canvas, &values);
            let mut mask = Surface::new(PixelFormat::new(ColorMode::Grayscale, depth, false));
            mask.fill_rect(canvas, &[0.85]);
            mask.fill_rect(Rect::new(118, 22, 158, 54), &[0.2]);
            let mut selection = Surface::new(PixelFormat::new(ColorMode::Grayscale, SampleType::F32, false));
            selection.fill_rect(canvas, &[0.7]);
            let mut layer = Layer::new("native cached", LayerContent::Raster(source.clone()));
            let mut layer_mask = photocraft_doc::LayerMask::hide_all();
            layer_mask.surface = mask.clone();
            layer_mask.linked = true;
            layer_mask.feather = 3.;
            layer_mask.density = 0.67;
            layer.mask = Some(layer_mask);
            let id = layer.id;
            let mut doc = Document::new("native cache", Size::new(560, 80), mode, depth);
            doc.layers = vec![layer];
            doc.selection = selected.then(|| selection.clone());
            let mut field = LiquifyField::new(canvas, 2.);
            field.gpu_only = true;
            field.apply_stroke(&stroke);
            let preview = photocraft_color::convert::with_cmyk_space(photocraft_compose::cmyk_space(&doc).as_ref(), || {
                Preview::new(
                    &rs,
                    vec![Source { id, surface: source, mask: Some(mask), linked: true, stationary: None, feather: 3., density: 0.67 }],
                    selected.then_some(&selection),
                    canvas,
                    &field,
                )
            })
            .unwrap();
            preview.render_outputs(&rs, None);
            preview.apply_ops(&rs, &field, &field.gpu_ops, true);
            let mut compositor = photocraft_gpu::Compositor::try_new_with_format(&rs.device, wgpu::TextureFormat::Rgba32Float).unwrap();
            compositor.set_source_overrides(preview.render_outputs(&rs, Some(Rect::new(110, 12, 176, 68))));
            let actual = photocraft_gpu::render_to_vec(&mut compositor, &rs.device, &rs.queue, &doc, canvas).unwrap();
            let mut cached_session = photocraft_engine::Session::new();
            cached_session.add_document(doc.clone(), None);
            let mut session = photocraft_engine::Session::new();
            session.add_document(doc, None);
            session.execute("filter.liquify", serde_json::json!({"layer": id.0, "meshSize": 2., "strokes": [stroke.clone()]})).unwrap();
            let expected = photocraft_compose::render(&session.active().unwrap().doc, canvas);
            let worst = actual
                .iter()
                .zip(&expected.px)
                .flat_map(|(a, b)| (0..4).map(move |k| if k == 3 { (a[k] - b[k]).abs() } else { (a[k] * a[3] - b[k] * b[3]).abs() }))
                .fold(0f32, f32::max);
            assert!(worst < 0.003, "{mode:?}/{depth:?} cached native colour differs from engine commit: {worst}");
            eprintln!("Liquify native {mode:?}/{depth:?} selection={selected} premultiplied error {worst}");
            let (pixels, masks) = preview.commit_surfaces(&rs).unwrap();
            cached_session
                .stage_cached_transform(photocraft_engine::cached_transform::CachedCommit { document: cached_session.active().unwrap().doc.id, pixels, masks })
                .unwrap();
            cached_session.execute("filter.liquify", serde_json::json!({"layer":id.0,"meshSize":2.,"strokes":[stroke.clone()]})).unwrap();
            let committed = photocraft_compose::render(&cached_session.active().unwrap().doc, canvas);
            let error = actual
                .iter()
                .zip(&committed.px)
                .flat_map(|(a, b)| (0..4).map(move |k| if k == 3 { (a[k] - b[k]).abs() } else { (a[k] * a[3] - b[k] * b[3]).abs() }))
                .fold(0f32, f32::max);
            assert!(error < 0.0001, "{mode:?}/{depth:?} final GPU native commit changed preview by {error}");
            if !selected {
                let area = Rect::new(350, 10, 351, 11);
                let actual = cached_session.active().unwrap().doc.layer(id).unwrap().surface().unwrap().read_region(area);
                let expected = session.active().unwrap().doc.layer(id).unwrap().surface().unwrap().read_region(area);
                assert_eq!(actual, expected, "commit must preserve hidden native colours in unchanged tiles");
            }
        }
    }
}
