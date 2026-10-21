//! Stroke accumulation and final lane compositing, with independent native inks and alpha.
use photocraft_color::BlendMode;
use photocraft_paint::{BrushSettings, MaskMode};
use wgpu::util::DeviceExt;

pub const SHADER: &str = include_str!("brush_composite.wgsl");
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CompositeLane {
    pub color: [f32; 4],
    /// opacity, primary strength, secondary strength, pigment mixing strength.
    pub gains: [f32; 4],
    /// blend mode, attributes enabled, attribute blend mode, attribute colour mode.
    pub modes: [u32; 4],
    /// attribute blend/colour strength, saturation recording/preservation strength.
    pub attributes: [f32; 4],
    /// erase, lock transparency, full secondary lane present, per-dab colour present.
    pub flags: [u32; 4],
}
impl CompositeLane {
    pub fn from_brush(brush: &BrushSettings, lock: bool, secondary: bool, per_dab_color: bool) -> Self {
        Self {
            color: brush.color,
            gains: [brush.opacity, brush.dual_brush.primary_strength, brush.dual_brush.secondary_strength, brush.pigment_mix_strength],
            modes: [
                blend_code(brush.mode),
                u32::from(brush.color_attributes.enabled),
                brush.color_attributes.blend as u32,
                brush.color_attributes.color as u32,
            ],
            attributes: [
                brush.color_attributes.blend_strength,
                brush.color_attributes.color_strength,
                brush.saturation_record_strength,
                brush.saturation_preserve_output,
            ],
            flags: [u32::from(brush.erase), u32::from(lock), u32::from(secondary), u32::from(per_dab_color)],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CompositeParams {
    /// Texture-local x/y/width/height.
    pub rect: [u32; 4],
    /// Document coordinates of texture (0,0), used by Dissolve.
    pub origin: [i32; 2],
    pub seed: [u32; 2],
    pub primary: CompositeLane,
    pub secondary: CompositeLane,
    /// Dab opacity ceiling, single-outline colour strength, unused, unused.
    pub dab: [f32; 4],
    /// Resolved wet strength/width/stamp width, unused.
    pub wet: [f32; 4],
    /// Accumulate: colour present, colour per pixel, wet material present, saturation present.
    /// Composite: primary transport exists, secondary transport exists, unused, unused.
    pub controls: [u32; 4],
    /// dual MaskMode, primary stroke texture present, secondary stroke texture present,
    /// legacy dual-tip enabled (bit 0), legacy depth wet factor (bit 1).
    pub mask: [u32; 4],
    /// Primary stroke texture depth, secondary stroke texture depth, legacy dual opacity, unused.
    pub texture_depth: [f32; 4],
    /// Native mode, alpha present, native path enabled, output flags:
    /// bit 0 = native-only Normal/Dissolve, bit 1 = wet-coloured source in NativeFinish input7.
    pub native: [u32; 4],
    /// Native constant brush colour (including independently picked CMYK black).
    pub native_color: [f32; 4],
}
impl CompositeParams {
    pub fn from_brush(brush: &BrushSettings, secondary: Option<&BrushSettings>, lock: bool, per_dab_color: bool) -> Self {
        let primary = CompositeLane::from_brush(brush, lock, secondary.is_some(), per_dab_color);
        Self {
            seed: [brush.seed as u32, (brush.seed >> 32) as u32],
            primary,
            secondary: secondary.map_or_else(CompositeLane::default, |b| CompositeLane::from_brush(b, lock, false, true)),
            mask: [
                mask_code(brush.dual_brush.mode),
                u32::from(brush.texture.enabled && !brush.texture.each_tip),
                u32::from(secondary.is_some_and(|b| b.texture.enabled && !b.texture.each_tip)),
                u32::from(brush.dual_brush.enabled),
            ],
            texture_depth: [brush.texture.depth.clamp(0.0, 1.0), secondary.map_or(0.0, |b| b.texture.depth.clamp(0.0, 1.0)), brush.dual_brush.opacity, 0.0],
            ..Default::default()
        }
    }
    pub fn with_native(mut self, format: &crate::brush_format::NativeFormat, brush: &BrushSettings, native_only: bool) -> Self {
        self.native = [format.mode(), u32::from(format.format.alpha), 1, u32::from(native_only)];
        let mut native = [0.0; 8];
        photocraft_raster::from_rgba_into(&format.format, brush.color, &mut native);
        if format.mode() == 2
            && let Some((rgb, inks)) = brush.color_cmyk
            && rgb == [brush.color[0], brush.color[1], brush.color[2]]
        {
            native[..4].copy_from_slice(&inks);
        }
        self.native_color.copy_from_slice(&native[..4]);
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub enum CompositeStage {
    Accumulate,
    Source,
    Coverage,
    Composite,
    Merge,
    Dither,
    NativeFinish,
}

pub struct CompositePipelines {
    layout: wgpu::BindGroupLayout,
    pipelines: [wgpu::ComputePipeline; 7],
    lut: wgpu::Buffer,
    empty_cms: wgpu::Buffer,
}
impl CompositePipelines {
    pub fn new(device: &wgpu::Device) -> Self {
        let mut entries: Vec<_> = (0..8)
            .map(|binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            })
            .collect();
        for binding in 8..11 {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: FORMAT,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            });
        }
        for (binding, ty) in [(11, wgpu::BufferBindingType::Uniform), (12, wgpu::BufferBindingType::Storage { read_only: true })] {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            });
        }
        entries.extend(crate::brush_format::NativeFormat::layout_entries(13, 14));
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_brush_composite"), entries: &entries });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_brush_composite"),
            source: wgpu::ShaderSource::Wgsl(
                format!("{}\n{}\n{}", include_str!("brush_pigment_mix.wgsl"), crate::brush_format::NativeFormat::shader(0, 13, 14), SHADER).into(),
            ),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_brush_composite"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipelines = ["accumulate", "source", "coverage", "finish", "merge", "dither", "native_finish"].map(|entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pl),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        let lut = crate::brush_pigment::create_pm_lut(device);
        let empty_cms = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_empty_cms"),
            contents: &[0u8; 128],
            usage: wgpu::BufferUsages::STORAGE,
        });
        Self { layout, pipelines, lut, empty_cms }
    }

    /// Inputs and outputs have identical extents; outputs must be distinct from all inputs and
    /// from each other. Preserve pixels outside rect by copying before a partial dispatch.
    ///
    /// Accumulate: inputs 0=cov/depth/sat-moment/sat-weight, 1=coverage-weighted RGB,
    /// 2=wet material, 3=dab value/depth, 4=per-pixel source; outputs 0/1/2 update these fields.
    /// Source: input0=cov, input1=weighted colour; output0=straight source with brush alpha.
    /// Coverage: inputs0/1=raw primary/secondary cov, 2/3=wet-adjusted primary/secondary cov,
    /// 4/5=primary/secondary stroke texture; output0=final coverage/wet-primary/wet-secondary/0.
    /// Stroke texture inputs 4/5 must contain the exact post-texture result in g (not raw pattern),
    /// calculated with the respective texture's existing C3/Photoshop mode and coordinate math.
    /// Composite: inputs0=pre, 1=primary raw cov, 2=primary wet-coloured straight source,
    /// 3=secondary raw cov, 4=secondary wet-coloured source, 5=selection (white when absent),
    /// 6=Coverage output; output0=candidate. With controls[0/1] transport enabled,
    /// corresponding input2/4 is its wet-coloured transport work map.
    /// Shade pigment after this entire candidate pass.
    /// Merge: inputs0/2=primary/secondary pigment-shaded candidate, 1/3=raw lane coverage;
    /// output0=shade_pigment's final lane-weighted output. Use only with full secondary lanes.
    /// Dither: input0=final output, input1.r=final coverage, input5.r=selection;
    /// controls[0]=U8 target, controls[1]=aliased brush, controls[2]=pigment active.
    /// Output0 rounds U8 pixels after the CPU's deterministic sub-half-level dither.
    /// NativeFinish: inputs0/1=native pre/alpha, 2=weighted native source, 3=raw coverage,
    /// 4=finished RGBA candidate, 5=selection, 6=final coverage, 7=wet RGBA source;
    /// final pigment output (controls[2]) instead uses input7 as the exact material-region mask.
    /// Outputs0/1 hold native inks and independent alpha, including final U8 dithering.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        inputs: [&wgpu::TextureView; 8],
        outputs: [&wgpu::TextureView; 3],
        params: CompositeParams,
        stage: CompositeStage,
    ) {
        self.encode_inner(device, encoder, inputs, outputs, params, stage, None);
    }
    #[allow(clippy::too_many_arguments)]
    pub fn encode_native(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        inputs: [&wgpu::TextureView; 8],
        outputs: [&wgpu::TextureView; 3],
        params: CompositeParams,
        stage: CompositeStage,
        format: &crate::brush_format::NativeFormat,
    ) {
        self.encode_inner(device, encoder, inputs, outputs, params, stage, Some(format));
    }
    #[allow(clippy::too_many_arguments)]
    fn encode_inner(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        inputs: [&wgpu::TextureView; 8],
        outputs: [&wgpu::TextureView; 3],
        params: CompositeParams,
        stage: CompositeStage,
        format: Option<&crate::brush_format::NativeFormat>,
    ) {
        if params.rect[2] == 0 || params.rect[3] == 0 {
            return;
        }
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_brush_composite_params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let mut entries: Vec<_> = inputs
            .into_iter()
            .enumerate()
            .map(|(binding, view)| wgpu::BindGroupEntry { binding: binding as u32, resource: wgpu::BindingResource::TextureView(view) })
            .collect();
        entries.extend(
            outputs.into_iter().enumerate().map(|(i, view)| wgpu::BindGroupEntry { binding: i as u32 + 8, resource: wgpu::BindingResource::TextureView(view) }),
        );
        entries.extend([
            wgpu::BindGroupEntry { binding: 11, resource: uniform.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 12, resource: self.lut.as_entire_binding() },
        ]);
        entries.extend([
            wgpu::BindGroupEntry { binding: 13, resource: format.map_or(&self.empty_cms, |f| &f.info).as_entire_binding() },
            wgpu::BindGroupEntry { binding: 14, resource: format.map_or(&self.empty_cms, |f| &f.values).as_entire_binding() },
        ]);
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("pc_brush_composite"), layout: &self.layout, entries: &entries });
        let index = match stage {
            CompositeStage::Accumulate => 0,
            CompositeStage::Source => 1,
            CompositeStage::Coverage => 2,
            CompositeStage::Composite => 3,
            CompositeStage::Merge => 4,
            CompositeStage::Dither => 5,
            CompositeStage::NativeFinish => 6,
        };
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_brush_composite"), timestamp_writes: None });
        pass.set_pipeline(&self.pipelines[index]);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(params.rect[2].div_ceil(8), params.rect[3].div_ceil(8), 1);
    }
}

fn blend_code(mode: BlendMode) -> u32 {
    match mode {
        BlendMode::PassThrough => 0,
        BlendMode::Normal => 1,
        BlendMode::Dissolve => 2,
        BlendMode::Darken => 3,
        BlendMode::Multiply => 4,
        BlendMode::ColorBurn => 5,
        BlendMode::LinearBurn => 6,
        BlendMode::DarkerColor => 7,
        BlendMode::Lighten => 8,
        BlendMode::Screen => 9,
        BlendMode::ColorDodge => 10,
        BlendMode::LinearDodge => 11,
        BlendMode::LighterColor => 12,
        BlendMode::Overlay => 13,
        BlendMode::SoftLight => 14,
        BlendMode::HardLight => 15,
        BlendMode::VividLight => 16,
        BlendMode::LinearLight => 17,
        BlendMode::PinLight => 18,
        BlendMode::HardMix => 19,
        BlendMode::Difference => 20,
        BlendMode::Exclusion => 21,
        BlendMode::Subtract => 22,
        BlendMode::Divide => 23,
        BlendMode::Hue => 24,
        BlendMode::Saturation => 25,
        BlendMode::Color => 26,
        BlendMode::Luminosity => 27,
        BlendMode::Reflect => 28,
        BlendMode::Glow => 29,
        BlendMode::Negation => 30,
        BlendMode::Xor => 31,
        BlendMode::PaintNetColorBurn => 32,
        BlendMode::PaintNetColorDodge => 33,
    }
}
fn mask_code(mode: MaskMode) -> u32 {
    match mode {
        MaskMode::Normal => 0,
        MaskMode::Multiply => 1,
        MaskMode::Subtract => 2,
        MaskMode::Darken => 3,
        MaskMode::Overlay => 4,
        MaskMode::ColorDodge => 5,
        MaskMode::ColorBurn => 6,
        MaskMode::LinearBurn => 7,
        MaskMode::HardMix => 8,
        MaskMode::LinearHeight => 9,
        MaskMode::Height => 10,
    }
}

#[path = "brush_union.rs"]
mod union_pass;
pub use union_pass::UnionPipelines;
