//! Resident group composites and independently transformed raw masks, paged in document space.
use crate::{
    Compositor, SourceOverride, SourceOverridePage, Unsupported,
    cached_image::Image,
    cached_transform::{CachedTransform, LayerSource},
};
use photocraft_doc::{Document, LayerContent, LayerId};
use photocraft_geom::Rect;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use wgpu::util::DeviceExt;

pub struct GroupSource {
    pub id: LayerId,
    pub linked: Vec<f32>,
    pub stationary: Vec<f32>,
    pub default: f32,
    pub moving: bool,
    pub feather: f32,
    pub density: f32,
    pub vector: Option<Vec<f32>>,
    pub vector_default: f32,
    pub vector_feather: f32,
    pub vector_density: f32,
}
struct Scratch {
    content: wgpu::Texture,
    raw: wgpu::Texture,
    pixel: [wgpu::Texture; 3],
    vector: [wgpu::Texture; 3],
    stationary: wgpu::Texture,
    mask: wgpu::Texture,
    shape: wgpu::Texture,
}
struct Group {
    id: LayerId,
    vector_id: LayerId,
    doc: Document,
    compositor: Compositor,
    masks: CachedTransform,
    stationary: Image,
    stationary_default: f32,
    scratch: Scratch,
    pages: BTreeMap<(i32, i32), SourceOverridePage>,
    side: i32,
    apron: i32,
    feather: f32,
    vector_feather: f32,
    density: f32,
    vector_density: f32,
    default: f32,
    vector_default: f32,
    moving: bool,
    commit_mask: bool,
    transparency_shapes: bool,
    clip_mask: bool,
    effect_margin: i32,
}
pub struct CachedGroups {
    groups: Vec<Group>,
    rect: Rect,
    layout: wgpu::BindGroupLayout,
    mask_pipeline: wgpu::RenderPipeline,
    multiply_pipeline: wgpu::RenderPipeline,
    shape_pipeline: wgpu::RenderPipeline,
    vertices: wgpu::Buffer,
    revision: u64,
    preview_region: Option<Rect>,
    feather: crate::cached_feather::Feather,
}
fn texture(device: &wgpu::Device, rect: Rect, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cached group page"),
        size: wgpu::Extent3d { width: rect.width(), height: rect.height(), depth_or_array_layers: 1 },
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
fn clear(encoder: &mut wgpu::CommandEncoder, t: &wgpu::Texture, value: f32) {
    let view = t.create_view(&Default::default());
    let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("cached group clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color { r: value as f64, g: value as f64, b: value as f64, a: value as f64 }),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
}
fn copy(encoder: &mut wgpu::CommandEncoder, src: &wgpu::Texture, src_rect: Rect, dst: &wgpu::Texture, dst_rect: Rect) {
    let r = src_rect.intersect(&dst_rect);
    if r.is_empty() {
        return;
    }
    encoder.copy_texture_to_texture(
        wgpu::TexelCopyTextureInfo {
            texture: src,
            mip_level: 0,
            origin: wgpu::Origin3d { x: (r.x0 - src_rect.x0) as u32, y: (r.y0 - src_rect.y0) as u32, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyTextureInfo {
            texture: dst,
            mip_level: 0,
            origin: wgpu::Origin3d { x: (r.x0 - dst_rect.x0) as u32, y: (r.y0 - dst_rect.y0) as u32, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d { width: r.width(), height: r.height(), depth_or_array_layers: 1 },
    );
}
fn raw_source(id: LayerId, rect: Rect, values: Vec<f32>, default: f32) -> LayerSource {
    let n = values.len();
    let mut bounds = Rect::EMPTY;
    for (index, value) in values.iter().enumerate() {
        if *value != default {
            let x = rect.x0 + (index % rect.width() as usize) as i32;
            let y = rect.y0 + (index / rect.width() as usize) as i32;
            bounds = bounds.union(&Rect::new(x, y, x + 1, y + 1));
        }
    }
    if bounds.is_empty() {
        bounds = Rect::from_xywh(rect.x0, rect.y0, 1, 1);
    }
    let mut pixels = Vec::with_capacity(bounds.width() as usize * bounds.height() as usize);
    for y in bounds.y0..bounds.y1 {
        for x in bounds.x0..bounds.x1 {
            let value = values[(y - rect.y0) as usize * rect.width() as usize + (x - rect.x0) as usize];
            pixels.push([value, 0., 0., 0.]);
        }
    }
    let count = pixels.len();
    LayerSource {
        id,
        rect: bounds,
        pixels,
        alpha: vec![1.; count],
        format: photocraft_color::PixelFormat::new(photocraft_color::ColorMode::Grayscale, photocraft_color::SampleType::F32, true),
        default: vec![default, 1.],
        rest: vec![[default, 0., 0., 0.]; n],
        rest_alpha: vec![1.; n],
        stationary_mask: vec![1.; n],
        linked_mask: None,
        vector_mask: None,
    }
}
impl CachedGroups {
    pub fn set_preview_region(&mut self, region: Option<Rect>) {
        self.preview_region = region;
    }
    /// Child sources must cover group page windows, mask feathers and nested effects.
    pub fn preview_source_region(&self, region: Rect) -> Rect {
        let mut halos = HashMap::new();
        for group in &self.groups {
            let nested = group.doc.walk().into_iter().filter_map(|(_, _, layer)| halos.get(&layer.id).copied()).max().unwrap_or(0i32);
            halos.insert(group.id, nested.saturating_add(group.side).saturating_add(group.apron).saturating_add(group.effect_margin));
        }
        region.inflate(halos.values().copied().max().unwrap_or(0))
    }
    pub fn readback_masks(&self, _device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Vec<(LayerId, photocraft_raster::Surface)>, Unsupported> {
        let mut result = Vec::new();
        for g in &self.groups {
            if !g.commit_mask {
                continue;
            }
            for (id, surface, _) in g.masks.readback_native(queue)? {
                if id == g.id {
                    let mut raw = photocraft_raster::Surface::with_default(
                        photocraft_color::PixelFormat::new(photocraft_color::ColorMode::Grayscale, photocraft_color::SampleType::F32, false),
                        &[g.default],
                    );
                    for (coord, _) in surface.tiles() {
                        let bounds = coord.rect();
                        let mut values = Vec::with_capacity(bounds.width() as usize * bounds.height() as usize);
                        for y in bounds.y0..bounds.y1 {
                            for x in bounds.x0..bounds.x1 {
                                values.push(
                                    surface.sample_channel(x, y, 0) * surface.sample_channel(x, y, 1) + g.default * (1. - surface.sample_channel(x, y, 1)),
                                );
                            }
                        }
                        raw.write_region(bounds, &values);
                    }
                    raw.prune();
                    result.push((id, raw));
                }
            }
        }
        Ok(result)
    }
    pub fn refresh_metadata(&mut self, doc: &Document) {
        for g in &mut self.groups {
            let Some(l) = doc.layer(g.id) else { continue };
            let LayerContent::Group(children) = &l.content else { continue };
            g.doc = doc.clone();
            g.doc.layers = children.children.clone();
            g.doc.selection = None;
            g.transparency_shapes = l.advanced.transparency_shapes;
            g.effect_margin = photocraft_compose::effects::margin(l);
        }
    }
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, doc: &Document, rect: Rect, mut sources: Vec<GroupSource>) -> Result<Self, Unsupported> {
        if rect.is_empty() {
            return Err(Unsupported("empty cached group extent".into()));
        }
        sources.sort_by_key(|s| std::cmp::Reverse(doc.path_of(s.id).map_or(0, |p| p.len())));
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pc_cached_group"),
            entries: &(0..5)
                .map(|binding| wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: if binding == 4 {
                        wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }
                    } else {
                        wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        }
                    },
                    count: None,
                })
                .collect::<Vec<_>>(),
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_cached_group"),
            source: wgpu::ShaderSource::Wgsl(include_str!("cached_group.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_cached_group"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("pc_cached_group"),
                layout: Some(&pl),
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
                    targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::R32Float, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let mask_pipeline = pipeline("extract");
        let multiply_pipeline = pipeline("multiply");
        let shape_pipeline = pipeline("shape");
        let vertices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pc_cached_group_vertices"),
            size: 144,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let full: [[f32; 6]; 6] = [
            [-1., 1., 0., 0., 1., 0.],
            [1., 1., 1., 0., 1., 0.],
            [1., -1., 1., 1., 1., 0.],
            [-1., 1., 0., 0., 1., 0.],
            [1., -1., 1., 1., 1., 0.],
            [-1., -1., 0., 1., 1., 0.],
        ];
        queue.write_buffer(&vertices, 0, bytemuck::cast_slice(&full));
        let mut groups = Vec::new();
        for source in sources {
            let layer = doc.layer(source.id).ok_or_else(|| Unsupported("cached group no longer exists".into()))?;
            let LayerContent::Group(children) = &layer.content else { continue };
            let expected = rect.width() as usize * rect.height() as usize;
            if source.linked.len() != expected || source.stationary.len() != expected || source.vector.as_ref().is_some_and(|v| v.len() != expected) {
                return Err(Unsupported("cached group mask extent mismatch".into()));
            }
            let vector_id = LayerId::fresh();
            let masks = CachedTransform::new(
                device,
                queue,
                rect,
                vec![
                    raw_source(source.id, rect, source.linked, source.default),
                    raw_source(vector_id, rect, source.vector.unwrap_or_else(|| vec![1.; expected]), source.vector_default),
                ],
            )?;
            let stationary = Image::new_empty(device, rect, wgpu::TextureFormat::R32Float)?;
            stationary.upload(queue, &source.stationary, 1);
            let mut stationary_layer = layer.clone();
            if source.moving {
                if stationary_layer.mask.as_ref().is_some_and(|m| m.linked) {
                    stationary_layer.mask = None;
                }
                if stationary_layer.vector_mask.as_ref().is_some_and(|m| m.linked) {
                    stationary_layer.vector_mask = None;
                }
            }
            let pixel_default = stationary_layer.mask.as_ref().filter(|m| m.enabled).map_or(1., |m| 1. - m.density * (1. - m.surface.default_pixel()[0]));
            let far = Rect::new(-1000000, -1000000, -999999, -999999);
            let stationary_default =
                photocraft_compose::masks::combined_mask(&stationary_layer, far).map_or(pixel_default, |s| s.sample_channel(far.x0, far.y0, 0));
            let apron = (3. * source.feather.max(source.vector_feather)).ceil() as i32 + 3;
            let available = device.limits().max_texture_dimension_2d as i32 - 2 * apron;
            if available < 1 {
                return Err(Unsupported("group mask feather exceeds GPU page limit".into()));
            }
            let side = 1024.min(available).min(rect.width().max(rect.height()) as i32).max(1);
            let window = Rect::from_xywh(0, 0, (side + 2 * apron) as u32, (side + 2 * apron) as u32);
            let scalar = || texture(device, window, wgpu::TextureFormat::R32Float);
            let scratch = Scratch {
                content: texture(device, window, wgpu::TextureFormat::Rgba32Float),
                raw: texture(device, window, wgpu::TextureFormat::Rgba32Float),
                pixel: [scalar(), scalar(), scalar()],
                vector: [scalar(), scalar(), scalar()],
                stationary: scalar(),
                mask: scalar(),
                shape: scalar(),
            };
            let mut isolated = doc.clone();
            isolated.layers = children.children.clone();
            isolated.selection = None;
            groups.push(Group {
                id: source.id,
                vector_id,
                doc: isolated,
                compositor: Compositor::new(device),
                masks,
                stationary,
                stationary_default,
                scratch,
                pages: BTreeMap::new(),
                side,
                apron,
                feather: source.feather,
                vector_feather: source.vector_feather,
                density: source.density,
                vector_density: source.vector_density,
                default: source.default,
                vector_default: source.vector_default,
                moving: source.moving,
                commit_mask: source.moving && layer.mask.as_ref().is_some_and(|m| m.linked && m.enabled),
                transparency_shapes: layer.advanced.transparency_shapes,
                clip_mask: photocraft_compose::masks::has_feather(layer) || layer.vector_mask.as_ref().is_some_and(|m| m.enabled),
                effect_margin: photocraft_compose::effects::margin(layer),
            });
        }
        Ok(Self {
            groups,
            rect,
            layout,
            mask_pipeline,
            multiply_pipeline,
            shape_pipeline,
            vertices,
            revision: 0,
            preview_region: None,
            feather: crate::cached_feather::Feather::new(device),
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        sources: &mut HashMap<LayerId, SourceOverride>,
        quad: [[f64; 2]; 4],
        warping: bool,
        map: &impl Fn(f64, f64) -> (f64, f64),
    ) -> Result<(), Unsupported> {
        self.encode_sampled(device, queue, encoder, sources, quad, warping, "bilinear", map, &|_, _| 1.)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn encode_sampled(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        sources: &mut HashMap<LayerId, SourceOverride>,
        quad: [[f64; 2]; 4],
        warping: bool,
        interpolation: &str,
        map: &impl Fn(f64, f64) -> (f64, f64),
        weight: &impl Fn(f64, f64) -> f64,
    ) -> Result<(), Unsupported> {
        self.revision = self.revision.wrapping_add(1);
        let source_region = self.preview_region.map(|region| self.preview_source_region(region));
        for g in &mut self.groups {
            g.masks.set_preview_region(source_region.map(|region| region.inflate(g.apron)));
            let masks = g.masks.encode_sampled(
                queue,
                encoder,
                quad,
                warping,
                interpolation,
                |x, y| if g.moving { map(x, y) } else { (x, y) },
                |x, y| if g.moving { weight(x, y) } else { 1. },
            )?;
            let mut wanted = BTreeSet::new();
            let mut content_bounds = Rect::EMPTY;
            let mut paint_bounds = Rect::EMPTY;
            for child in g.doc.layers.iter().filter(|l| l.visible) {
                let margin = if photocraft_compose::effects::has_effects(child) { photocraft_compose::effects::margin(child) } else { 0 };
                let actual_bounds = sources.get(&child.id).map_or_else(|| photocraft_compose::effect_source_bounds(child, self.rect), |source| source.rect);
                if !actual_bounds.is_empty() {
                    let actual_bounds = actual_bounds.inflate(margin);
                    content_bounds = if content_bounds.is_empty() { actual_bounds } else { content_bounds.union(&actual_bounds) };
                }
                let child_paint = sources.get(&child.id).map_or_else(|| photocraft_compose::layer_bounds(child, self.rect), |source| source.bounds);
                if !child_paint.is_empty() {
                    paint_bounds = if paint_bounds.is_empty() { child_paint } else { paint_bounds.union(&child_paint) };
                }
                let bounds: Vec<_> = if let Some(source) = sources.get(&child.id) {
                    if source.pages.is_empty() { vec![source.bounds] } else { source.pages.iter().map(|p| p.rect).collect() }
                } else {
                    vec![photocraft_compose::effect_source_bounds(child, self.rect)]
                };
                for bounds in bounds {
                    if bounds.is_empty() {
                        continue;
                    }
                    let bounds = bounds.inflate(margin);
                    let bounds = source_region.map_or(bounds, |preview| bounds.intersect(&preview));
                    if bounds.is_empty() {
                        continue;
                    }
                    for y in bounds.y0.div_euclid(g.side)..=(bounds.y1 - 1).div_euclid(g.side) {
                        for x in bounds.x0.div_euclid(g.side)..=(bounds.x1 - 1).div_euclid(g.side) {
                            wanted.insert((x, y));
                        }
                    }
                }
            }
            if !g.transparency_shapes {
                let canvas = g.doc.bounds();
                content_bounds = canvas;
                paint_bounds = canvas;
                let shape_region = canvas.inflate(g.effect_margin);
                let shape_region = source_region.map_or(shape_region, |preview| shape_region.intersect(&preview));
                if !shape_region.is_empty() {
                    for y in shape_region.y0.div_euclid(g.side)..=(shape_region.y1 - 1).div_euclid(g.side) {
                        for x in shape_region.x0.div_euclid(g.side)..=(shape_region.x1 - 1).div_euclid(g.side) {
                            wanted.insert((x, y));
                        }
                    }
                }
            }
            if wanted.is_empty() {
                wanted.insert((self.rect.x0.div_euclid(g.side), self.rect.y0.div_euclid(g.side)));
            }
            if wanted.len() > device.limits().max_texture_array_layers as usize * 4 {
                return Err(Unsupported("cached group exceeds resident page capacity".into()));
            }
            if self.preview_region.is_some() {
                g.pages.clear();
            } else {
                g.pages.retain(|key, _| wanted.contains(key));
            }
            let binding = |a: &wgpu::Texture, b: &wgpu::Texture, c: &wgpu::Texture, d: &wgpu::Texture, default: f32, window: Rect| {
                let densities = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("group densities"),
                    contents: bytemuck::cast_slice(&[
                        g.density,
                        g.vector_density,
                        default,
                        0f32,
                        g.doc.bounds().x0 as f32,
                        g.doc.bounds().y0 as f32,
                        g.doc.bounds().x1 as f32,
                        g.doc.bounds().y1 as f32,
                        window.x0 as f32,
                        window.y0 as f32,
                        (1. - g.density * (1. - g.default)) * g.stationary_default * (1. - g.vector_density * (1. - g.vector_default)),
                        if g.clip_mask { 1. } else { 0. },
                    ]),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
                let views = [a, b, c, d].map(|t| t.create_view(&Default::default()));
                let mut entries = views
                    .iter()
                    .enumerate()
                    .map(|(i, v)| wgpu::BindGroupEntry { binding: i as u32, resource: wgpu::BindingResource::TextureView(v) })
                    .collect::<Vec<_>>();
                entries.push(wgpu::BindGroupEntry { binding: 4, resource: densities.as_entire_binding() });
                device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("group page"), layout: &self.layout, entries: &entries })
            };
            let draw = |encoder: &mut wgpu::CommandEncoder, target: &wgpu::Texture, pipeline: &wgpu::RenderPipeline, binding: &wgpu::BindGroup| {
                let view = target.create_view(&Default::default());
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("group page mask"),
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
                pass.set_vertex_buffer(0, self.vertices.slice(..));
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, binding, &[]);
                pass.draw(0..6, 0..1);
            };
            g.compositor.set_source_overrides(sources.clone());
            for key in wanted {
                let rect = Rect::from_xywh(key.0 * g.side, key.1 * g.side, g.side as u32, g.side as u32);
                let window = rect.inflate(g.apron);
                let s = &g.scratch;
                clear(encoder, &s.content, 0.);
                g.compositor.set_render_bounds(Some(window));
                g.compositor.encode(device, queue, encoder, &g.doc, window, &mut |encoder, out| copy(encoder, out.texture, out.rect, &s.content, window))?;
                clear(encoder, &s.stationary, g.stationary_default);
                g.stationary.copy_window(encoder, &s.stationary, window);
                for (id, default, target) in [(g.id, g.default, &s.pixel[0]), (g.vector_id, g.vector_default, &s.vector[0])] {
                    clear(encoder, &s.raw, default);
                    if let Some(source) = masks.get(&id) {
                        for page in &source.pages {
                            copy(encoder, &page.texture, page.rect, &s.raw, window);
                        }
                    }
                    let b = binding(&s.raw, &s.stationary, &s.content, &s.stationary, default, window);
                    draw(encoder, target, &self.mask_pipeline, &b);
                }
                let canvas = g.doc.bounds();
                let clip = g.clip_mask.then(|| Rect::new(canvas.x0 - window.x0, canvas.y0 - window.y0, canvas.x1 - window.x0, canvas.y1 - window.y0));
                self.feather.encode_clipped(device, encoder, [&s.pixel[0], &s.pixel[1], &s.pixel[2]], g.feather, clip);
                self.feather.encode_clipped(device, encoder, [&s.vector[0], &s.vector[1], &s.vector[2]], g.vector_feather, clip);
                let b = binding(&s.pixel[2], &s.stationary, &s.content, &s.vector[2], 0., window);
                draw(encoder, &s.mask, &self.multiply_pipeline, &b);
                let b = binding(&s.mask, &s.stationary, &s.content, &s.stationary, 0., window);
                draw(encoder, &s.shape, &self.shape_pipeline, &b);
                let page = g.pages.entry(key).or_insert_with(|| SourceOverridePage {
                    texture: texture(device, rect, wgpu::TextureFormat::Rgba32Float),
                    shape: texture(device, rect, wgpu::TextureFormat::R32Float),
                    mask: Some(texture(device, rect, wgpu::TextureFormat::R32Float)),
                    rect,
                });
                copy(encoder, &s.content, window, &page.texture, rect);
                if let Some(mask) = page.mask.as_ref() {
                    copy(encoder, &s.mask, window, mask, rect);
                }
                copy(encoder, if g.transparency_shapes { &s.shape } else { &s.mask }, window, &page.shape, rect);
            }
            let pages = g.pages.values().cloned().collect::<Vec<_>>();
            let first = &pages[0];
            let bounds = paint_bounds;
            sources.insert(
                g.id,
                SourceOverride {
                    texture: first.texture.clone(),
                    shape: first.shape.clone(),
                    mask: first.mask.clone(),
                    rect: content_bounds,
                    bounds,
                    revision: self.revision,
                    pages,
                },
            );
        }
        Ok(())
    }
}
