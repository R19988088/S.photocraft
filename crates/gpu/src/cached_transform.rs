//! Frozen native source planes and reusable paged GPU presentation targets.
use crate::{SourceOverride, SourceOverridePage, Unsupported, cached_image::Image};
use photocraft_doc::LayerId;
use photocraft_geom::Rect;
use std::collections::HashMap;
use wgpu::util::DeviceExt;

pub struct MaskSource {
    pub rect: Rect,
    pub values: Vec<f32>,
    pub feather: f32,
    pub default: f32,
    pub density: f32,
    pub alpha: Option<Vec<f32>>,
    pub rest: Option<Vec<f32>>,
}
pub struct LayerSource {
    pub id: LayerId,
    pub rect: Rect,
    pub pixels: Vec<[f32; 4]>,
    pub alpha: Vec<f32>,
    pub format: photocraft_color::PixelFormat,
    pub default: Vec<f32>,
    pub rest: Vec<[f32; 4]>,
    pub rest_alpha: Vec<f32>,
    pub stationary_mask: Vec<f32>,
    pub linked_mask: Option<MaskSource>,
    pub vector_mask: Option<MaskSource>,
}
struct Mask {
    present: bool,
    source: [Image; 2],
    rest: Image,
    feather: f32,
    default: f32,
    density: f32,
    rest_bounds: Rect,
}
struct Page {
    rect: Rect,
    output: wgpu::Texture,
    shape: wgpu::Texture,
    native: Option<[wgpu::Texture; 2]>,
    mask: Option<wgpu::Texture>,
}
struct Scratch {
    rest: wgpu::Texture,
    rest_alpha: wgpu::Texture,
    stationary: wgpu::Texture,
    moved: wgpu::Texture,
    moved_alpha: wgpu::Texture,
    merged: wgpu::Texture,
    merged_alpha: wgpu::Texture,
    mask_rest: wgpu::Texture,
    mask_raw: wgpu::Texture,
    mask_value: wgpu::Texture,
    mask_scratch: wgpu::Texture,
    vector_rest: wgpu::Texture,
    vector_raw: wgpu::Texture,
    vector_value: wgpu::Texture,
    vector_scratch: wgpu::Texture,
    output: wgpu::Texture,
    shape: wgpu::Texture,
}
struct Layer {
    id: LayerId,
    source: [Image; 2],
    rest: [Image; 2],
    stationary: Image,
    native: crate::native_format::NativeFormat,
    default: Vec<f32>,
    mask: Mask,
    vector: Mask,
    scratch: Scratch,
    pages: Vec<Page>,
    apron: i32,
    page_side: u32,
    rest_bounds: Rect,
    has_mask: bool,
    mask_clip: Option<(Rect, f32)>,
}
pub struct CachedTransform {
    device: wgpu::Device,
    layers: Vec<Layer>,
    content: wgpu::RenderPipeline,
    alpha: wgpu::RenderPipeline,
    mask: wgpu::RenderPipeline,
    composite: wgpu::RenderPipeline,
    merge: wgpu::RenderPipeline,
    preview: wgpu::RenderPipeline,
    preview_background: wgpu::RenderPipeline,
    preview_region: Option<Rect>,
    native_valid: bool,
    feather: crate::cached_feather::Feather,
    reducer: crate::cached_resample::Resample,
    layout: wgpu::BindGroupLayout,
    vertices: wgpu::Buffer,
    capacity: u64,
    revision: u64,
}
pub(crate) fn texture(device: &wgpu::Device, rect: Rect, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cached transform page"),
        size: wgpu::Extent3d { width: rect.width().max(1), height: rect.height().max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC
            | if format == wgpu::TextureFormat::R32Float { wgpu::TextureUsages::STORAGE_BINDING } else { wgpu::TextureUsages::empty() },
        view_formats: &[],
    })
}
fn image(device: &wgpu::Device, queue: &wgpu::Queue, rect: Rect, format: wgpu::TextureFormat, values: &[f32], channels: u32) -> Result<Image, Unsupported> {
    if values.len() != rect.width() as usize * rect.height() as usize * channels as usize {
        return Err(Unsupported("cached source plane extent mismatch".into()));
    }
    let image = Image::new_empty(device, rect, format)?;
    image.upload(queue, values, channels);
    Ok(image)
}
fn nondefault_bounds(values: &[f32], rect: Rect, default: f32) -> Rect {
    let mut bounds = Rect::EMPTY;
    for (i, v) in values.iter().enumerate() {
        if *v != default {
            let x = rect.x0 + (i % rect.width() as usize) as i32;
            let y = rect.y0 + (i / rect.width() as usize) as i32;
            bounds = bounds.union(&Rect::new(x, y, x + 1, y + 1));
        }
    }
    bounds
}
fn mask_input(device: &wgpu::Device, queue: &wgpu::Queue, output: Rect, raw: Option<MaskSource>) -> Result<Mask, Unsupported> {
    let count = output.width() as usize * output.height() as usize;
    let present = raw.is_some();
    let raw = raw.unwrap_or(MaskSource { rect: output, values: vec![1.; count], feather: 0., default: 1., density: 1., alpha: None, rest: None });
    let rest = raw.rest.unwrap_or_else(|| vec![raw.default; count]);
    let rest_bounds = nondefault_bounds(&rest, output, raw.default);
    let alpha = raw.alpha.unwrap_or_else(|| vec![1.; raw.rect.width() as usize * raw.rect.height() as usize]);
    Ok(Mask {
        present,
        source: [
            image(device, queue, raw.rect, wgpu::TextureFormat::R32Float, &raw.values, 1)?,
            image(device, queue, raw.rect, wgpu::TextureFormat::R32Float, &alpha, 1)?,
        ],
        rest: image(device, queue, output, wgpu::TextureFormat::R32Float, &rest, 1)?,
        feather: raw.feather,
        default: raw.default,
        density: raw.density,
        rest_bounds,
    })
}
#[allow(clippy::too_many_arguments)]
fn bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    textures: [&wgpu::Texture; 7],
    source: [&Image; 2],
    native: &crate::native_format::NativeFormat,
    output: Rect,
    densities: [f32; 2],
    coverage: Rect,
    mapping: [f32; 12],
    mask_clip: Option<(Rect, f32)>,
) -> wgpu::BindGroup {
    let views = textures.map(|t| t.create_view(&Default::default()));
    let arrays = source.map(Image::view);
    let sample = match native.format.sample {
        photocraft_color::SampleType::U8 => 0,
        photocraft_color::SampleType::U16 => 1,
        _ => 2,
    };
    let mut data = bytemuck::cast_slice(&[native.mode(), sample, source[0].rect.width(), source[0].rect.height()]).to_vec();
    data.extend_from_slice(bytemuck::cast_slice(&[output.x0 as f32, output.y0 as f32, output.width() as f32, output.height() as f32]));
    data.extend_from_slice(bytemuck::cast_slice(&[
        source[0].side,
        source[0].rect.width().div_ceil(source[0].side),
        densities[0].to_bits(),
        densities[1].to_bits(),
    ]));
    data.extend_from_slice(bytemuck::cast_slice(&[coverage.x0 as f32, coverage.y0 as f32, coverage.x1 as f32, coverage.y1 as f32]));
    data.extend_from_slice(bytemuck::cast_slice(&mapping));
    let (canvas, outside) = mask_clip.unwrap_or((Rect::EMPTY, 1.));
    data.extend_from_slice(bytemuck::cast_slice(&[canvas.x0 as f32, canvas.y0 as f32, canvas.x1 as f32, canvas.y1 as f32]));
    data.extend_from_slice(bytemuck::cast_slice(&[outside, if mask_clip.is_some() { 1. } else { 0. }, 0., 0.]));
    let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("cached transform page origin"),
        contents: &data,
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let mut entries = views
        .iter()
        .enumerate()
        .map(|(i, v)| wgpu::BindGroupEntry { binding: if i == 6 { 9 } else { i as u32 }, resource: wgpu::BindingResource::TextureView(v) })
        .collect::<Vec<_>>();
    entries.extend([
        wgpu::BindGroupEntry { binding: 6, resource: native.info.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 7, resource: native.values.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 8, resource: params.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 10, resource: wgpu::BindingResource::TextureView(&arrays[0]) },
        wgpu::BindGroupEntry { binding: 11, resource: wgpu::BindingResource::TextureView(&arrays[1]) },
    ]);
    device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("cached transform page bindings"), layout, entries: &entries })
}
fn clear(encoder: &mut wgpu::CommandEncoder, target: &wgpu::Texture, value: f64) {
    let view = target.create_view(&Default::default());
    let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("cached page clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: value, g: value, b: value, a: value }), store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
}
impl CachedTransform {
    /// Preview only the requested document region. None materializes the full native result.
    pub fn set_preview_region(&mut self, region: Option<Rect>) {
        self.preview_region = region;
    }
    /// CPU combined feather/vector masks use their constant default outside the canvas.
    pub fn set_mask_clip(&mut self, id: LayerId, canvas: Rect, outside: f32) {
        if let Some(layer) = self.layers.iter_mut().find(|layer| layer.id == id) {
            layer.mask_clip = Some((canvas, outside));
        }
    }
    pub fn readback_native(&self, queue: &wgpu::Queue) -> Result<Vec<(LayerId, photocraft_raster::Surface, Option<photocraft_raster::Surface>)>, Unsupported> {
        if !self.native_valid {
            return Err(Unsupported("cached transform must be materialized before readback".into()));
        }
        let planes: Vec<_> = self
            .layers
            .iter()
            .flat_map(|l| {
                l.pages.iter().flat_map(|p| {
                    let Some(native) = p.native.as_ref() else { return Vec::new() };
                    let mut planes = vec![(&native[0], p.rect, 4), (&native[1], p.rect, 1)];
                    if let Some(mask) = &p.mask {
                        planes.push((mask, p.rect, 1));
                    }
                    planes
                })
            })
            .collect();
        let values = crate::cached_image::readback(&self.device, queue, &planes)?;
        let mut values = values.into_iter();
        let mut output = Vec::new();
        for layer in &self.layers {
            let format = photocraft_color::PixelFormat::new(layer.native.format.mode, layer.native.format.sample, true);
            let colors = format.mode.color_channels();
            let mut surface = photocraft_raster::Surface::new(format);
            let mut mask = layer.has_mask.then(|| {
                photocraft_raster::Surface::with_default(
                    photocraft_color::PixelFormat::new(photocraft_color::ColorMode::Grayscale, photocraft_color::SampleType::F32, false),
                    &[layer.mask.default],
                )
            });
            for page in &layer.pages {
                let color = values.next().ok_or_else(|| Unsupported("missing cached transform color readback".into()))?;
                let alpha = values.next().ok_or_else(|| Unsupported("missing cached transform alpha readback".into()))?;
                let mut native = Vec::with_capacity(alpha.len() * (colors + 1));
                for (pixel, alpha) in color.chunks_exact(4).zip(alpha) {
                    native.extend_from_slice(&pixel[..colors]);
                    native.push(alpha);
                }
                surface.write_region(page.rect, &native);
                if let Some(mask) = &mut mask {
                    let raw = values.next().ok_or_else(|| Unsupported("missing cached transform mask readback".into()))?;
                    mask.write_region(page.rect, &raw);
                }
            }
            surface.prune();
            if let Some(mask) = &mut mask {
                mask.prune();
            }
            output.push((layer.id, surface, mask));
        }
        Ok(output)
    }
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, rect: Rect, sources: Vec<LayerSource>) -> Result<Self, Unsupported> {
        if rect.is_empty() || sources.iter().any(|s| s.rect.is_empty()) {
            return Err(Unsupported("empty cached transform source".into()));
        }
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cached transform bindings"),
            entries: &(0..12)
                .map(|binding| wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: if (6..=8).contains(&binding) {
                        wgpu::BindingType::Buffer {
                            ty: if binding == 8 { wgpu::BufferBindingType::Uniform } else { wgpu::BufferBindingType::Storage { read_only: true } },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        }
                    } else {
                        wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: if binding >= 10 { wgpu::TextureViewDimension::D2Array } else { wgpu::TextureViewDimension::D2 },
                            multisampled: false,
                        }
                    },
                    count: None,
                })
                .collect::<Vec<_>>(),
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cached transform"),
            source: wgpu::ShaderSource::Wgsl((crate::native_format::NativeFormat::shader(0, 6, 7) + include_str!("cached_transform.wgsl")).into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cached transform pipeline"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry, format| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vertex"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: 24,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0=>Float32x2,1=>Float32x4],
                    })],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &if matches!(entry, "content" | "merge" | "preview" | "preview_background") {
                        vec![
                            Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL }),
                            Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::R32Float, blend: None, write_mask: wgpu::ColorWrites::ALL }),
                        ]
                    } else {
                        vec![Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })]
                    },
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let content = pipeline("content", wgpu::TextureFormat::Rgba32Float);
        let alpha = pipeline("alpha", wgpu::TextureFormat::R32Float);
        let mask_pipeline = pipeline("raw_mask", wgpu::TextureFormat::R32Float);
        let composite = pipeline("composite", wgpu::TextureFormat::Rgba32Float);
        let merge = pipeline("merge", wgpu::TextureFormat::Rgba32Float);
        let preview = pipeline("preview", wgpu::TextureFormat::Rgba32Float);
        let preview_background = pipeline("preview_background", wgpu::TextureFormat::Rgba32Float);
        let mut layers = Vec::new();
        for s in sources {
            let native = crate::native_format::NativeFormat::new(device, s.format).map_err(Unsupported)?;
            let source = [
                image(device, queue, s.rect, wgpu::TextureFormat::Rgba32Float, bytemuck::cast_slice(&s.pixels), 4)?,
                image(device, queue, s.rect, wgpu::TextureFormat::R32Float, &s.alpha, 1)?,
            ];
            let rest = [
                image(device, queue, rect, wgpu::TextureFormat::Rgba32Float, bytemuck::cast_slice(&s.rest), 4)?,
                image(device, queue, rect, wgpu::TextureFormat::R32Float, &s.rest_alpha, 1)?,
            ];
            let stationary = image(device, queue, rect, wgpu::TextureFormat::R32Float, &s.stationary_mask, 1)?;
            let has_mask = s.linked_mask.is_some();
            let mask = mask_input(device, queue, rect, s.linked_mask)?;
            let vector = mask_input(device, queue, rect, s.vector_mask)?;
            let apron = (mask.feather.max(vector.feather) * 3.).ceil() as i32 + 3;
            let limit = device.limits().max_texture_dimension_2d;
            let Some(available) = limit.checked_sub((apron as u32).saturating_mul(2)) else {
                return Err(Unsupported("mask feather exceeds GPU page extent".into()));
            };
            let page_side = 1024.min(available).min(rect.width().max(rect.height()));
            if page_side == 0 {
                return Err(Unsupported("mask feather exceeds GPU page extent".into()));
            }
            let scratch_rect = Rect::new(0, 0, (page_side + 2 * apron as u32) as i32, (page_side + 2 * apron as u32) as i32);
            let scalar = || texture(device, scratch_rect, wgpu::TextureFormat::R32Float);
            let color = || texture(device, scratch_rect, wgpu::TextureFormat::Rgba32Float);
            let scratch = Scratch {
                rest: color(),
                rest_alpha: scalar(),
                stationary: scalar(),
                moved: color(),
                moved_alpha: scalar(),
                merged: color(),
                merged_alpha: scalar(),
                mask_rest: scalar(),
                mask_raw: scalar(),
                mask_value: scalar(),
                mask_scratch: scalar(),
                vector_rest: scalar(),
                vector_raw: scalar(),
                vector_value: scalar(),
                vector_scratch: scalar(),
                output: color(),
                shape: scalar(),
            };
            let pages = Vec::new();
            let rest_bounds = nondefault_bounds(&s.rest_alpha, rect, 0.0);
            layers.push(Layer {
                id: s.id,
                source,
                rest,
                stationary,
                native,
                default: s.default,
                mask,
                vector,
                scratch,
                pages,
                apron,
                page_side,
                rest_bounds,
                has_mask,
                mask_clip: None,
            });
        }
        let capacity = 144 + layers.len() as u64 * 3 * 6 * 96 * 96 * 24;
        let vertices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cached transform geometry"),
            size: capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            device: device.clone(),
            layers,
            content,
            alpha,
            mask: mask_pipeline,
            composite,
            merge,
            preview,
            preview_background,
            preview_region: None,
            native_valid: false,
            feather: crate::cached_feather::Feather::new(device),
            reducer: crate::cached_resample::Resample::new(device),
            layout,
            vertices,
            capacity,
            revision: 0,
        })
    }
    pub fn encode(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        quad: [[f64; 2]; 4],
        warping: bool,
        map: impl Fn(f64, f64) -> (f64, f64),
    ) -> Result<HashMap<LayerId, SourceOverride>, Unsupported> {
        self.encode_sampled(queue, encoder, quad, warping, "bilinear", map, |_, _| 1.)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn encode_sampled(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        _quad: [[f64; 2]; 4],
        warping: bool,
        interpolation: &str,
        map: impl Fn(f64, f64) -> (f64, f64),
        weight: impl Fn(f64, f64) -> f64,
    ) -> Result<HashMap<LayerId, SourceOverride>, Unsupported> {
        self.encode_sampled_with_homography(queue, encoder, _quad, warping, interpolation, None, map, weight)
    }

    /// Supply the actual projective geometry when it is known. Nonlinear meshes must use None.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_sampled_with_homography(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        _quad: [[f64; 2]; 4],
        warping: bool,
        interpolation: &str,
        homography: Option<photocraft_algo::transform::Homography>,
        map: impl Fn(f64, f64) -> (f64, f64),
        weight: impl Fn(f64, f64) -> f64,
    ) -> Result<HashMap<LayerId, SourceOverride>, Unsupported> {
        let base_mode = match interpolation {
            "nearest" => 0.,
            "bilinear" => 1.,
            _ => 2.,
        };
        let mode = base_mode + if warping { 3.0 } else { 0.0 };
        // Full-screen draws retain NDC vertices; the shader marks these with negative mode.
        let mut vertices = vec![
            [-1., 1., 0., 0., 1., -1.],
            [1., 1., 1., 0., 1., -1.],
            [1., -1., 1., 1., 1., -1.],
            [-1., 1., 0., 0., 1., -1.],
            [1., -1., 1., 1., 1., -1.],
            [-1., -1., 0., 1., 1., -1.],
        ];
        let mut ranges = Vec::new();
        let mut bounds = Vec::new();
        let mut reductions = Vec::new();
        let mut mappings = Vec::new();
        for l in &self.layers {
            for (source_index, source) in [&l.source, &l.mask.source, &l.vector.source].into_iter().enumerate() {
                let rect = source[0].rect;
                let (cx, cy) = (f64::from(rect.x0 + rect.x1) * 0.5, f64::from(rect.y0 + rect.y1) * 0.5);
                let (a, b, c, d) = (map(cx - 0.5, cy), map(cx + 0.5, cy), map(cx, cy - 0.5), map(cx, cy + 0.5));
                let scale = ((b.0 - a.0) * (d.1 - c.1) - (b.1 - a.1) * (d.0 - c.0)).abs().sqrt();
                let present = source_index == 0 || if source_index == 1 { l.mask.present } else { l.vector.present };
                let reduction = if present && !warping && mode > 1.5 && scale > 0. && scale < 0.999 {
                    let f = scale.max(1. / 64.);
                    let format = if source_index == 0 {
                        l.native.format
                    } else {
                        photocraft_color::PixelFormat::new(photocraft_color::ColorMode::Grayscale, photocraft_color::SampleType::F32, true)
                    };
                    let default = if source_index == 0 { l.default.as_slice() } else { &[0., 0.] };
                    let (images, rect) = self.reducer.encode(&self.device, encoder, [&source[0], &source[1]], rect, f, format, default)?;
                    Some((images, rect, f))
                } else {
                    None
                };
                let (source_rect, source_scale) = reduction.as_ref().map_or((rect, 1.), |(_, r, f)| (*r, *f));
                reductions.push(reduction);
                let corners = [
                    [f64::from(source_rect.x0), f64::from(source_rect.y0)],
                    [f64::from(source_rect.x1), f64::from(source_rect.y0)],
                    [f64::from(source_rect.x1), f64::from(source_rect.y1)],
                    [f64::from(source_rect.x0), f64::from(source_rect.y1)],
                ];
                let quad = corners.map(|[x, y]| {
                    let (x, y) = map(x / source_scale, y / source_scale);
                    [x, y]
                });
                let mut sampling = [0f32; 12];
                // Non-mesh callers explicitly declare projective geometry through warping=false.
                // Mesh callers use an analytical inverse only when they supply their actual H.
                let projective = homography
                    .map(|h| h.mul(&photocraft_algo::transform::Homography([1. / source_scale, 0., 0., 0., 1. / source_scale, 0., 0., 0., 1.])))
                    .or_else(|| {
                        (!warping)
                            .then(|| photocraft_algo::transform::Homography::rect_to_quad([corners[0][0], corners[0][1], corners[2][0], corners[2][1]], quad))
                            .flatten()
                    });
                // Every interpolation kernel is the identity at an integer translation.
                // Avoid normalized f32 coordinates turning a texel centre into a tiny
                // fractional sample and leaving alpha outside an otherwise exact move.
                let integer_translation = projective.is_some_and(|h| {
                    let m = h.0;
                    m[8] != 0.
                        && m[0] == m[8]
                        && m[4] == m[8]
                        && [m[1], m[3], m[6], m[7]].into_iter().all(|v| v == 0.)
                        && (m[2] / m[8]).fract() == 0.
                        && (m[5] / m[8]).fract() == 0.
                });
                let mode = if integer_translation { 0. } else { mode };
                if let Some(inv) = projective.and_then(|h| h.inverse()) {
                    let m = inv.0;
                    for k in 0..3 {
                        sampling[k] = ((m[k] - f64::from(source_rect.x0) * m[6 + k]) / f64::from(source_rect.width())) as f32;
                        sampling[4 + k] = ((m[3 + k] - f64::from(source_rect.y0) * m[6 + k]) / f64::from(source_rect.height())) as f32;
                        sampling[8 + k] = m[6 + k] as f32;
                    }
                    sampling[3] = 1.0;
                }
                sampling[7] = mode;
                mappings.push(sampling);
                // Analytical sampling needs only the projected boundary. Keep the mesh when
                // the mapping is nonlinear or its projective horizon crosses the source.
                let corner_weights = corners.map(|[x, y]| weight(x / source_scale, y / source_scale));
                let same_side = corner_weights.iter().all(|w| w.is_finite() && *w > 1e-9) || corner_weights.iter().all(|w| w.is_finite() && *w < -1e-9);
                let n = if sampling[3] > 0.5 && same_side {
                    1
                } else if warping {
                    96
                } else {
                    24
                };
                let start = vertices.len() as u32;
                let (mut minx, mut miny, mut maxx, mut maxy) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
                for y in 0..n {
                    for x in 0..n {
                        for (i, j) in [(x, y), (x + 1, y), (x + 1, y + 1), (x, y), (x + 1, y + 1), (x, y + 1)] {
                            let (u, v) = (f64::from(i) / f64::from(n), f64::from(j) / f64::from(n));
                            let (u, v) = (
                                u * (1. + 2. / f64::from(source_rect.width())) - 1. / f64::from(source_rect.width()),
                                v * (1. + 2. / f64::from(source_rect.height())) - 1. / f64::from(source_rect.height()),
                            );
                            let (sx, sy) = (
                                (f64::from(source_rect.x0) + u * f64::from(source_rect.width())) / source_scale,
                                (f64::from(source_rect.y0) + v * f64::from(source_rect.height())) / source_scale,
                            );
                            let (dx, dy) = map(sx, sy);
                            let (bx, by) = map(
                                (f64::from(source_rect.x0) + u.clamp(0., 1.) * f64::from(source_rect.width())) / source_scale,
                                (f64::from(source_rect.y0) + v.clamp(0., 1.) * f64::from(source_rect.height())) / source_scale,
                            );
                            minx = minx.min(bx);
                            miny = miny.min(by);
                            maxx = maxx.max(bx);
                            maxy = maxy.max(by);
                            let q = 1. / weight(sx, sy);
                            vertices.push([dx as f32, dy as f32, (u * q) as f32, (v * q) as f32, q as f32, mode]);
                        }
                    }
                }
                let corners =
                    [(source_rect.x0, source_rect.y0), (source_rect.x1, source_rect.y0), (source_rect.x1, source_rect.y1), (source_rect.x0, source_rect.y1)];
                let positives = corners.iter().filter(|(x, y)| weight(f64::from(*x) / source_scale, f64::from(*y) / source_scale) > 0.).count();
                let front = match positives {
                    3 | 4 => 1.,
                    0 | 1 => -1.,
                    _ => {
                        if weight(
                            f64::from(source_rect.x0 + source_rect.x1) * 0.5 / source_scale,
                            f64::from(source_rect.y0 + source_rect.y1) * 0.5 / source_scale,
                        ) >= 0.
                        {
                            1.
                        } else {
                            -1.
                        }
                    }
                };
                let end = vertices.len();
                let triangles: Vec<_> = vertices[start as usize..end]
                    .chunks_exact(3)
                    .filter(|t| t.iter().all(|v| v.iter().all(|x| x.is_finite()) && f64::from(v[4]) * front > 1e-9))
                    .flat_map(|v| v.iter().copied())
                    .collect();
                vertices.truncate(start as usize);
                vertices.extend(triangles);
                ranges.push(start..vertices.len() as u32);
                bounds.push(Rect::new(minx.floor() as i32, miny.floor() as i32, maxx.ceil() as i32, maxy.ceil() as i32));
            }
        }
        let bytes = bytemuck::cast_slice(&vertices);
        if bytes.len() as u64 > self.capacity {
            return Err(Unsupported("cached transform mesh exceeds vertex capacity".into()));
        }
        queue.write_buffer(&self.vertices, 0, bytes);
        self.revision = self.revision.wrapping_add(1);
        self.native_valid = self.preview_region.is_none();
        let mut overrides = HashMap::new();
        for (index, l) in self.layers.iter_mut().enumerate() {
            let side = l.page_side as i32;
            let regions =
                [bounds[index * 3].inflate(2), l.rest_bounds, if l.has_mask { bounds[index * 3 + 1].inflate(2) } else { Rect::EMPTY }, l.mask.rest_bounds];
            let mut wanted = std::collections::BTreeSet::new();
            for region in regions {
                let region = self.preview_region.map_or(region, |preview| region.intersect(&preview));
                if region.is_empty() {
                    continue;
                }
                for y in region.y0.div_euclid(side)..=(region.y1 - 1).div_euclid(side) {
                    for x in region.x0.div_euclid(side)..=(region.x1 - 1).div_euclid(side) {
                        wanted.insert((x, y));
                        if wanted.len() > self.device.limits().max_texture_array_layers as usize * 4 {
                            return Err(Unsupported("cached transform result exceeds GPU page capacity".into()));
                        }
                    }
                }
            }
            if wanted.is_empty()
                && let Some(preview) = self.preview_region
            {
                wanted.insert((preview.x0.div_euclid(side), preview.y0.div_euclid(side)));
            }
            if self.preview_region.is_some() {
                // Published frames retain their textures while the worker prepares the next.
                l.pages.clear();
            } else {
                l.pages.retain(|p| wanted.contains(&(p.rect.x0.div_euclid(side), p.rect.y0.div_euclid(side))));
            }
            for (x, y) in wanted {
                let rect = Rect::new(x * side, y * side, (x + 1) * side, (y + 1) * side);
                if l.pages.iter().any(|p| p.rect == rect) {
                    continue;
                }
                l.pages.push(Page {
                    rect,
                    output: texture(&self.device, rect, wgpu::TextureFormat::Rgba32Float),
                    shape: texture(&self.device, rect, wgpu::TextureFormat::R32Float),
                    native: None,
                    mask: None,
                });
            }
            if self.preview_region.is_none() {
                for page in &mut l.pages {
                    page.native.get_or_insert_with(|| {
                        [texture(&self.device, page.rect, wgpu::TextureFormat::Rgba32Float), texture(&self.device, page.rect, wgpu::TextureFormat::R32Float)]
                    });
                    if l.has_mask {
                        page.mask.get_or_insert_with(|| texture(&self.device, page.rect, wgpu::TextureFormat::R32Float));
                    }
                }
            }
            let s = &l.scratch;
            let pixel_source = reductions[index * 3].as_ref().map_or([&l.source[0], &l.source[1]], |(t, _, _)| [&t[0], &t[1]]);
            let mask_source = reductions[index * 3 + 1].as_ref().map_or([&l.mask.source[0], &l.mask.source[1]], |(t, _, _)| [&t[0], &t[1]]);
            let vector_source = reductions[index * 3 + 2].as_ref().map_or([&l.vector.source[0], &l.vector.source[1]], |(t, _, _)| [&t[0], &t[1]]);
            for page in &l.pages {
                let window = Rect::new(
                    page.rect.x0 - l.apron,
                    page.rect.y0 - l.apron,
                    page.rect.x0 + l.page_side as i32 + l.apron,
                    page.rect.y0 + l.page_side as i32 + l.apron,
                );
                for (target, source, default) in [
                    (&s.rest, &l.rest[0], 0.),
                    (&s.rest_alpha, &l.rest[1], 0.),
                    (&s.stationary, &l.stationary, 1.),
                    (&s.mask_rest, &l.mask.rest, l.mask.default),
                    (&s.vector_rest, &l.vector.rest, l.vector.default),
                ] {
                    clear(encoder, target, f64::from(default));
                    source.copy_window(encoder, target, window);
                }
                let make = |src: [&Image; 2], color: &wgpu::Texture, alpha: &wgpu::Texture, rest: &wgpu::Texture, source_index: usize| {
                    bind(
                        &self.device,
                        &self.layout,
                        [color, rest, &s.stationary, &s.mask_value, alpha, &s.vector_value, &s.rest_alpha],
                        src,
                        &l.native,
                        window,
                        [l.mask.density, l.vector.density],
                        bounds[index * 3 + source_index].inflate(1),
                        mappings[index * 3 + source_index],
                        l.mask_clip,
                    )
                };
                let content = make(pixel_source, &s.rest, &s.rest_alpha, &s.rest, 0);
                let mask = make(mask_source, &s.rest, &s.rest_alpha, &s.mask_rest, 1);
                let vector = make(vector_source, &s.rest, &s.rest_alpha, &s.vector_rest, 2);
                let merge = make(pixel_source, &s.moved, &s.moved_alpha, &s.rest, 0);
                let composite = make(pixel_source, &s.merged, &s.merged_alpha, &s.rest, 0);
                let alpha = make(pixel_source, &s.output, &s.moved_alpha, &s.rest, 0);
                let draw = |encoder: &mut wgpu::CommandEncoder,
                            target: &wgpu::Texture,
                            pipeline: &wgpu::RenderPipeline,
                            binding: &wgpu::BindGroup,
                            range: std::ops::Range<u32>,
                            value: f64| {
                    let view = target.create_view(&Default::default());
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("cached transform page"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            depth_slice: None,
                            ops: wgpu::Operations {
                                load: if value.is_nan() {
                                    wgpu::LoadOp::Load
                                } else {
                                    wgpu::LoadOp::Clear(wgpu::Color { r: value, g: value, b: value, a: value })
                                },
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_vertex_buffer(0, self.vertices.slice(..));
                    pass.set_pipeline(pipeline);
                    pass.set_bind_group(0, binding, &[]);
                    pass.draw(range, 0..1);
                };
                let draw_native = |encoder: &mut wgpu::CommandEncoder,
                                   targets: [&wgpu::Texture; 2],
                                   pipeline: &wgpu::RenderPipeline,
                                   binding: &wgpu::BindGroup,
                                   range: std::ops::Range<u32>| {
                    let views = targets.map(|t| t.create_view(&Default::default()));
                    let attachments = views
                        .iter()
                        .map(|view| {
                            Some(wgpu::RenderPassColorAttachment {
                                view,
                                resolve_target: None,
                                depth_slice: None,
                                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                            })
                        })
                        .collect::<Vec<_>>();
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("cached native source page"),
                        color_attachments: &attachments,
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_vertex_buffer(0, self.vertices.slice(..));
                    pass.set_bind_group(0, binding, &[]);
                    pass.set_pipeline(pipeline);
                    pass.draw(range, 0..1);
                };
                if self.preview_region.is_none() {
                    draw_native(encoder, [&s.moved, &s.moved_alpha], &self.content, &content, ranges[index * 3].clone());
                    draw_native(encoder, [&s.merged, &s.merged_alpha], &self.merge, &merge, 0..6);
                }
                if l.mask.present {
                    clear(encoder, &s.mask_raw, f64::from(l.mask.default));
                    l.mask.rest.copy_window(encoder, &s.mask_raw, window);
                    draw(encoder, &s.mask_raw, &self.mask, &mask, ranges[index * 3 + 1].clone(), f64::NAN);
                    if self.preview_region.is_none()
                        && let Some(target) = &page.mask
                    {
                        encoder.copy_texture_to_texture(
                            wgpu::TexelCopyTextureInfo {
                                texture: &s.mask_raw,
                                mip_level: 0,
                                origin: wgpu::Origin3d { x: l.apron as u32, y: l.apron as u32, z: 0 },
                                aspect: wgpu::TextureAspect::All,
                            },
                            target.as_image_copy(),
                            target.size(),
                        );
                    }
                    self.feather.encode_clipped(
                        &self.device,
                        encoder,
                        [&s.mask_raw, &s.mask_scratch, &s.mask_value],
                        l.mask.feather,
                        l.mask_clip.map(|(canvas, _)| canvas.translate(-window.x0, -window.y0)),
                    );
                } else {
                    clear(encoder, &s.mask_value, f64::from(l.mask.default));
                }
                if l.vector.present {
                    clear(encoder, &s.vector_raw, f64::from(l.vector.default));
                    l.vector.rest.copy_window(encoder, &s.vector_raw, window);
                    draw(encoder, &s.vector_raw, &self.mask, &vector, ranges[index * 3 + 2].clone(), f64::NAN);
                    self.feather.encode_clipped(
                        &self.device,
                        encoder,
                        [&s.vector_raw, &s.vector_scratch, &s.vector_value],
                        l.vector.feather,
                        l.mask_clip.map(|(canvas, _)| canvas.translate(-window.x0, -window.y0)),
                    );
                } else {
                    clear(encoder, &s.vector_value, f64::from(l.vector.default));
                }
                if self.preview_region.is_some() {
                    let binding = make(pixel_source, &s.rest, &s.rest_alpha, &s.rest, 0);
                    // A projective inverse samples every destination pixel directly. Mesh
                    // warps first retain the stationary content, then replace mapped pixels.
                    let mut render = |pipeline: &wgpu::RenderPipeline, range: std::ops::Range<u32>, load: bool| {
                        let views = [&s.output, &s.shape].map(|t| t.create_view(&Default::default()));
                        let attachments = views
                            .iter()
                            .map(|view| {
                                Some(wgpu::RenderPassColorAttachment {
                                    view,
                                    resolve_target: None,
                                    depth_slice: None,
                                    ops: wgpu::Operations {
                                        load: if load { wgpu::LoadOp::Load } else { wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT) },
                                        store: wgpu::StoreOp::Store,
                                    },
                                })
                            })
                            .collect::<Vec<_>>();
                        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("cached transform direct preview"),
                            color_attachments: &attachments,
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        });
                        pass.set_pipeline(pipeline);
                        pass.set_vertex_buffer(0, self.vertices.slice(..));
                        pass.set_bind_group(0, &binding, &[]);
                        pass.draw(range, 0..1);
                    };
                    if mappings[index * 3][3] > 0.5 {
                        render(&self.preview, 0..6, false);
                    } else {
                        render(&self.preview_background, 0..6, false);
                        render(&self.preview, ranges[index * 3].clone(), true);
                    }
                } else {
                    draw(encoder, &s.output, &self.composite, &composite, 0..6, 0.);
                    draw(encoder, &s.shape, &self.alpha, &alpha, 0..6, 0.);
                }
                let mut copies = vec![(&s.output, &page.output), (&s.shape, &page.shape)];
                if self.preview_region.is_none()
                    && let Some(native) = page.native.as_ref()
                {
                    copies.extend([(&s.merged, &native[0]), (&s.merged_alpha, &native[1])]);
                }
                for (from, to) in copies {
                    encoder.copy_texture_to_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: from,
                            mip_level: 0,
                            origin: wgpu::Origin3d { x: l.apron as u32, y: l.apron as u32, z: 0 },
                            aspect: wgpu::TextureAspect::All,
                        },
                        to.as_image_copy(),
                        wgpu::Extent3d { width: page.rect.width(), height: page.rect.height(), depth_or_array_layers: 1 },
                    );
                }
            }
            if let Some(first) = l.pages.first() {
                overrides.insert(
                    l.id,
                    SourceOverride {
                        texture: first.output.clone(),
                        shape: first.shape.clone(),
                        mask: None,
                        rect: bounds[index * 3].union(&l.rest_bounds),
                        bounds: bounds[index * 3].union(&l.rest_bounds),
                        revision: self.revision,
                        pages: l
                            .pages
                            .iter()
                            .map(|p| SourceOverridePage { texture: p.output.clone(), shape: p.shape.clone(), mask: None, rect: p.rect })
                            .collect(),
                    },
                );
            }
        }
        Ok(overrides)
    }
}
