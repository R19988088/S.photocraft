//! GPU dab rasterisation. The CPU uploads static brush assets and scalar dab parameters only.
use photocraft_geom::Rect;
use photocraft_paint::brush::TextureCoordinateMode;
use photocraft_paint::material::{WetColorMode, WetEdgeControl, WetOutline, WetStyle, WetWidthMode};
use photocraft_paint::{BrushContext, BrushSettings, Dab};
use wgpu::util::DeviceExt;

const BODY: &str = include_str!("brush_raster.wgsl");
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

/// Shares the exact Single wet-edge and colour functions with the standalone wet pipeline.
pub fn shader_source() -> String {
    let wet = crate::brush_wet::SHADER;
    let alpha = wet.split("fn unit(").nth(1).and_then(|s| s.split("fn sample_coverage(").next()).unwrap_or("");
    let color = wet.split("fn rgb_to_hsl(").nth(1).and_then(|s| s.split("fn inside(").next()).unwrap_or("");
    format!("{}\n{BODY}\nfn unit({alpha}\nfn rgb_to_hsl({color}", crate::brush_format::NativeFormat::shader(0, 7, 8))
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    rect: [u32; 4],
    style: u32,
    width_mode: u32,
    color_mode: u32,
    edge_control: u32,
    softness: f32,
    brightness: f32,
    saturation: f32,
    padding: f32,
    resolved: [f32; 4],
    origin: [i32; 4],
    center: [f32; 4],
    shape: [f32; 4],
    rotation: [f32; 4],
    projection: [f32; 4],
    tip_shape: [f32; 4],
    flags: [u32; 4],
    flags2: [u32; 4],
    tip: [u32; 4],
    tip_base: [u32; 4],
    texture: [u32; 4],
    texture_params: [f32; 4],
    texture_origin: [f32; 4],
    grain: [f32; 4],
    grain2: [f32; 4],
    paint: [f32; 4],
    pigment: [u32; 4],
    canvas: [u32; 4],
    canvas_params: [f32; 4],
    native: [u32; 4],
}

pub struct RasterPipelines {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
    stroke_texture: wgpu::ComputePipeline,
    empty_cms: wgpu::Buffer,
}
impl RasterPipelines {
    pub fn new(device: &wgpu::Device) -> Self {
        let mut entries = vec![
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: true }, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            storage(2),
            storage(3),
            storage(4),
            wgpu::BindGroupLayoutEntry {
                binding: 6,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 5,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            },
        ];
        entries.extend(crate::brush_format::NativeFormat::layout_entries(7, 8));
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_brush_raster"), entries: &entries });
        let module = device
            .create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("pc_brush_raster"), source: wgpu::ShaderSource::Wgsl(shader_source().into()) });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_brush_raster"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pc_brush_raster"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("raster"),
            compilation_options: Default::default(),
            cache: None,
        });
        let stroke_texture = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pc_stroke_texture"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("stroke_texture"),
            compilation_options: Default::default(),
            cache: None,
        });
        let empty_cms = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_raster_empty_cms"),
            contents: &[0u8; 128],
            usage: wgpu::BufferUsages::STORAGE,
        });
        Self { layout, pipeline, stroke_texture, empty_cms }
    }
}
fn storage(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture { access: wgpu::StorageTextureAccess::WriteOnly, format: FORMAT, view_dimension: wgpu::TextureViewDimension::D2 },
        count: None,
    }
}

/// Reuse these textures between dabs. Only `rect` is valid after a raster pass.
pub struct RasterOutput {
    pub coverage: wgpu::Texture,
    pub depth: wgpu::Texture,
    pub source: wgpu::Texture,
    pub rect: [u32; 4],
    pub size: [u32; 2],
}
impl RasterOutput {
    pub fn new(device: &wgpu::Device, size: [u32; 2]) -> Self {
        let texture = |label| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: size[0].max(1), height: size[1].max(1), depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        Self { coverage: texture("pc_dab_coverage"), depth: texture("pc_dab_depth"), source: texture("pc_dab_source"), rect: [0; 4], size }
    }
}

pub struct RasterBrush {
    context: BrushContext,
    dual: bool,
    secondary: bool,
    samples: wgpu::Buffer,
    mips: Vec<[u32; 4]>,
    texture: [u32; 4],
    pigment: [u32; 4],
    canvas: [u32; 4],
    curve_offset: u32,
}
impl RasterBrush {
    /// Resource preparation reproduces BrushContext's static assets. No stroke pixels are
    /// sampled or rasterised here. Procedural patterns are the same prepared brush assets.
    pub fn new(device: &wgpu::Device, _queue: &wgpu::Queue, context: &BrushContext, dual: bool) -> Result<Self, String> {
        let secondary = dual && context.secondary_context().is_some();
        let context = if dual { context.secondary_context().unwrap_or(context) } else { context };
        let dual = dual && !secondary;
        let b = &context.brush;
        let mut samples = vec![0.0f32];
        let mut push = |w: u32, h: u32, values: &[f32]| -> Result<[u32; 4], String> {
            let offset = u32::try_from(samples.len()).map_err(|_| "brush assets exceed GPU address space")?;
            samples.extend_from_slice(values);
            Ok([offset, w, h, 0])
        };
        let mut mips = Vec::new();
        if let Some(m) = context.tip_mips(dual) {
            for (w, h, data) in &m.levels {
                mips.push(push(*w as u32, *h as u32, data)?);
            }
        }
        let texture = match context.texture_image() {
            Some(t) => push(t.width as u32, t.height as u32, &t.data)?,
            None => [0; 4],
        };
        let pigment = if !dual && b.pattern_enabled {
            if let Some(p) = &b.sampled_pattern { push(p.width, p.height, bytemuck::cast_slice(p.pixels.as_ref()))? } else { [0; 4] }
        } else {
            [0; 4]
        };
        let mut curve_offset = 0;
        let canvas = if !dual {
            if let Some(c) = &b.canvas_material {
                if c.enabled && c.scale > 0.0 && c.curve_lut.len() == 256 {
                    if let Some(t) = c.pattern.bitmap().filter(|t| t.is_valid()) {
                        let tile = push(t.width, t.height, &t.to_f32())?;
                        curve_offset = push(256, 1, &c.curve_lut)?[0];
                        tile
                    } else {
                        [0; 4]
                    }
                } else {
                    [0; 4]
                }
            } else {
                [0; 4]
            }
        } else {
            [0; 4]
        };
        let bytes = bytemuck::cast_slice(&samples);
        if bytes.len() as u64 > device.limits().max_storage_buffer_binding_size {
            return Err("brush assets exceed GPU storage binding limit".into());
        }
        let samples = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_brush_assets"),
            contents: bytes,
            usage: wgpu::BufferUsages::STORAGE,
        });
        Ok(Self { context: context.clone(), dual, secondary, samples, mips, texture, pigment, canvas, curve_offset })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        pipelines: &RasterPipelines,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        dab: &Dab,
        origin: [i32; 2],
        size: [u32; 2],
        stroke_origin: [f32; 2],
        base: &wgpu::TextureView,
    ) -> RasterOutput {
        let mut output = RasterOutput::new(device, size);
        self.encode_into(pipelines, device, encoder, dab, origin, stroke_origin, base, &mut output, None);
        output
    }

    #[allow(clippy::too_many_arguments)]
    pub fn encode_into(
        &self,
        pipelines: &RasterPipelines,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        d: &Dab,
        origin: [i32; 2],
        stroke_origin: [f32; 2],
        base: &wgpu::TextureView,
        output: &mut RasterOutput,
        native: Option<&crate::brush_format::NativeFormat>,
    ) {
        let bounds = Rect::new(origin[0], origin[1], origin[0].saturating_add(output.size[0] as i32), origin[1].saturating_add(output.size[1] as i32));
        let r = self.context.dab_rect(d, self.dual).intersect(&bounds);
        output.rect = [(r.x0 - origin[0]).max(0) as u32, (r.y0 - origin[1]).max(0) as u32, r.width(), r.height()];
        if r.is_empty() {
            return;
        }
        let mut p = self.params(d, origin, stroke_origin, output.rect);
        if let Some(format) = native {
            p.native = [format.mode(), u32::from(format.format.alpha), u32::from(!self.secondary && self.context.brush.canvas_material.is_some()), 0];
        }
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_raster_params"),
            contents: bytemuck::bytes_of(&p),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let cov = output.coverage.create_view(&Default::default());
        let depth = output.depth.create_view(&Default::default());
        let source = output.source.create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_brush_raster"),
            layout: &pipelines.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.samples.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(base) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&cov) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&depth) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&source) },
                wgpu::BindGroupEntry { binding: 5, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(base) },
                wgpu::BindGroupEntry { binding: 7, resource: native.map_or(&pipelines.empty_cms, |f| &f.info).as_entire_binding() },
                wgpu::BindGroupEntry { binding: 8, resource: native.map_or(&pipelines.empty_cms, |f| &f.values).as_entire_binding() },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_brush_raster"), timestamp_writes: None });
        pass.set_pipeline(&pipelines.pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(r.width().div_ceil(8), r.height().div_ceil(8), 1);
    }

    /// Apply the completed lane's texture. Set output.rect to the damaged coverage region;
    /// writes [raw coverage, textured coverage, 0, 0] into output.coverage.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_stroke_texture(
        &self,
        pipelines: &RasterPipelines,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        last_dab: &Dab,
        origin: [i32; 2],
        stroke_origin: [f32; 2],
        coverage: &wgpu::TextureView,
        output: &mut RasterOutput,
    ) {
        if output.rect[2] == 0 || output.rect[3] == 0 {
            return;
        }
        let mut p = self.params(last_dab, origin, stroke_origin, output.rect);
        p.texture_params[1] = self.context.brush.texture.depth.clamp(0.0, 1.0);
        p.texture_origin[3] = if self.context.brush.texture.each_tip { 1.0 } else { 0.0 };
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_stroke_texture_params"),
            contents: bytemuck::bytes_of(&p),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let cov = output.coverage.create_view(&Default::default());
        let depth = output.depth.create_view(&Default::default());
        let source = output.source.create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_stroke_texture"),
            layout: &pipelines.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.samples.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(coverage) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&cov) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&depth) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&source) },
                wgpu::BindGroupEntry { binding: 5, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(coverage) },
                wgpu::BindGroupEntry { binding: 7, resource: pipelines.empty_cms.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 8, resource: pipelines.empty_cms.as_entire_binding() },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_stroke_texture"), timestamp_writes: None });
        pass.set_pipeline(&pipelines.stroke_texture);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(p.rect[2].div_ceil(8), p.rect[3].div_ceil(8), 1);
    }

    fn params(&self, d: &Dab, origin: [i32; 2], stroke_origin: [f32; 2], rect: [u32; 4]) -> Params {
        let b: &BrushSettings = &self.context.brush;
        let dual = self.dual;
        let hardness = if dual {
            if b.dual_brush.hardness_dynamic.is_active() { d.hardness } else { b.dual_brush.hardness }
        } else if b.hardness_dynamic.is_active() || b.pigment.is_some() || b.contour.is_some() {
            d.hardness
        } else {
            b.hardness
        };
        let aliased = b.aliased && !dual;
        let wet = if dual {
            b.dual_brush.wet_edges.then_some(b.dual_brush.wet_edge.as_ref()).flatten()
        } else {
            b.wet_edges.then_some(b.wet_edge.as_ref()).flatten()
        }
        .filter(|_| !b.erase);
        let single = wet.is_some_and(|w| w.outline == WetOutline::Single);
        let grain = if dual { b.dual_brush.grain.as_ref() } else { b.noise.then_some(b.grain.as_ref()).flatten() };
        let (cx, cy) = if aliased { photocraft_paint::grid_center(d.center.x, d.center.y, d.radius * 2.0) } else { (d.center.x as f32, d.center.y as f32) };
        let (sn, cs) = (if b.c3_tip_math { -d.angle } else { d.angle }).sin_cos();
        let ro = if b.c3_tip_math { d.roundness.max(0.01) } else { d.roundness.max(0.5 / d.radius).min(1.0) };
        let rm = (d.radius * ro).max(0.5);
        let (r0, shoulder) = photocraft_paint::tip_shape(rm, hardness);
        let tip_base = self.mips.first().copied().unwrap_or([0; 4]);
        let sampled = !self.mips.is_empty();
        let level = if sampled {
            let ratio = (tip_base[1].max(tip_base[2]) as f32 / (2.0 * d.radius * ro.max(0.25)).max(1.0)).max(1.0);
            (ratio.log2().floor() as usize).min(self.mips.len().saturating_sub(1))
        } else {
            0
        };
        let tip = self.mips.get(level).copied().unwrap_or([0; 4]);
        let reach = if b.c3_tip_math {
            rm + 1.0
        } else if aliased || sampled {
            rm
        } else {
            photocraft_paint::tip_reach(rm, hardness).max(rm)
        };
        let (ps, pc) = d.proj_angle.sin_cos();
        let project = !dual && d.proj_scale < 0.999;
        let canvas = b.canvas_material.as_ref();
        let w = wet.cloned().unwrap_or_default();
        Params {
            rect,
            style: match w.style {
                WetStyle::Photoshop => 0,
                WetStyle::Watercolor => 1,
                WetStyle::Watercolor2 => 2,
            },
            width_mode: u32::from(w.width_mode == WetWidthMode::Fixed),
            color_mode: match w.color_mode {
                WetColorMode::Normal => 0,
                WetColorMode::Hsl => 1,
                WetColorMode::Multiply => 2,
                WetColorMode::ColorBurn => 3,
            },
            edge_control: u32::from(w.edge_control == WetEdgeControl::SoftCoverage),
            softness: w.softness,
            brightness: w.brightness,
            saturation: w.saturation,
            padding: 0.0,
            resolved: [d.wet_edge.strength, d.wet_edge.width, d.wet_edge.stamp_width, 0.0],
            origin: [origin[0], origin[1], i32::from(d.pigment.enabled && !b.erase), 0],
            center: [cx, cy, d.center.x as f32, d.center.y as f32],
            shape: [d.radius, ro, rm, hardness],
            rotation: [sn, cs, if d.flip_x { -1.0 } else { 1.0 }, if d.flip_y { -1.0 } else { 1.0 }],
            projection: [pc, ps, if project { 1.0 / d.proj_scale.max(0.05) - 1.0 } else { 0.0 }, d.roundness],
            tip_shape: [r0, shoulder, reach, d.alpha],
            flags: [u32::from(b.c3_tip_math), u32::from(aliased), u32::from(b.triangle), u32::from(b.square)],
            flags2: [u32::from(sampled), u32::from(single), u32::from(b.noise && b.grain.is_none() && !dual), u32::from(self.secondary)],
            tip,
            tip_base,
            texture: self.texture,
            texture_params: [b.texture.scale.max(0.01), d.depth, b.texture.mode as u32 as f32, if b.texture.c3_math { 1.0 } else { 0.0 }],
            texture_origin: [
                stroke_origin[0],
                stroke_origin[1],
                match b.texture.coordinate_mode {
                    TextureCoordinateMode::CanvasLocked => 0.0,
                    TextureCoordinateMode::StrokeLocked => 1.0,
                    TextureCoordinateMode::TipLocal => 2.0,
                },
                f32::from(u8::from(!dual && b.texture.enabled && b.texture.each_tip && self.texture[1] > 0)),
            ],
            grain: grain.map_or([0.0; 4], |g| [1.0, g.size, g.size_jitter, g.density_jitter]),
            grain2: [
                d.grain_density,
                if b.wet_edges && b.wet_edge.is_none() && !dual && b.erase { 1.0 } else { 0.0 },
                photocraft_paint::tip_shape(d.radius.max(0.1), 1.0).0,
                photocraft_paint::tip_shape(d.radius.max(0.1), 1.0).1,
            ],
            paint: d.color,
            pigment: self.pigment,
            native: [0; 4],
            canvas: self.canvas,
            canvas_params: [
                canvas.map_or(1.0, |c| c.scale),
                canvas.map_or(0.0, |c| c.strength * (1.0 - b.opacity.clamp(0.0, 1.0) * d.alpha.clamp(0.0, 1.0))),
                canvas.map_or(0.0, |c| c.blend_mode as u32 as f32),
                self.curve_offset as f32,
            ],
        }
    }
}
