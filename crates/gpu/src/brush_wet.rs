//! Wet-edge coverage and colour passes. Every input shares one texture coordinate space.
use wgpu::util::DeviceExt;

pub const SHADER: &str = include_str!("brush_wet.wgsl");
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

/// Numeric wet-edge settings, independent of the paint engine's storage types.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct WetParams {
    /// Texture-local x, y, width, height of the writes.
    pub rect: [u32; 4],
    /// Photoshop=0, Watercolor=1, Watercolor2=2.
    pub style: u32,
    /// Ratio=0, Fixed=1.
    pub width_mode: u32,
    /// Normal=0, Hsl=1, Multiply=2, ColorBurn=3.
    pub color_mode: u32,
    /// Expansion=0, SoftCoverage=1.
    pub edge_control: u32,
    pub softness: f32,
    pub brightness: f32,
    pub saturation: f32,
    pub _padding: f32,
    /// Already resolved per-dab strength, width and stamp_width; last value unused.
    pub resolved: [f32; 4],
}

#[derive(Clone, Copy, Debug)]
pub enum WetStage {
    /// Raw accumulated coverage.r and per-pixel depth.xyz = ResolvedWet. Writes
    /// [filtered coverage, edge, colour effect, 0]. Watercolor2 reads the neighbouring
    /// raw coverage, so the input must include adjacent tiles, not clamp at tile seams.
    StrokeCoverage,
    /// Raw tip coverage.r and depth.r = normalized tip radius. Uses params.resolved,
    /// writes the same layout as StrokeCoverage. Run before texture, legacy wet_factor
    /// and dab alpha, and accumulate the edge separately from the filtered coverage.
    SingleDabCoverage,
    /// Straight source/base colours and coverage.b = precomputed colour effect from a
    /// coverage pass. Single-dab colour runs before accumulating each dab's colour.
    Color,
    /// Straight source/base colours with depth.a = already accumulated single-edge
    /// colour effect. Used for the completed dual/transport lane semantics.
    SingleAccumulatedColor,
}

pub struct WetPipelines {
    layout: wgpu::BindGroupLayout,
    stroke: wgpu::ComputePipeline,
    single: wgpu::ComputePipeline,
    color: wgpu::ComputePipeline,
    single_color: wgpu::ComputePipeline,
}

impl WetPipelines {
    pub fn new(device: &wgpu::Device) -> Self {
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
            label: Some("pc_brush_wet"),
            entries: &[
                texture(0),
                texture(1),
                texture(2),
                texture(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("pc_brush_wet"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_brush_wet"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        Self {
            stroke: pipeline("stroke_coverage"),
            single: pipeline("single_dab_coverage"),
            color: pipeline("wet_color"),
            single_color: pipeline("single_accumulated_color"),
            layout,
        }
    }

    /// Inputs may alias one another; output must be a separate RGBA32Float texture.
    /// Pixels outside rect are untouched. Missing coverage beyond the working texture is
    /// zero, matching CoverageMap. Source/base are straight-alpha colours.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        coverage: &wgpu::TextureView,
        depth: &wgpu::TextureView,
        base: &wgpu::TextureView,
        source: &wgpu::TextureView,
        output: &wgpu::TextureView,
        params: WetParams,
        stage: WetStage,
    ) {
        if params.rect[2] == 0 || params.rect[3] == 0 {
            return;
        }
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_brush_wet_params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_brush_wet"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(coverage) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(depth) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(base) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(source) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(output) },
                wgpu::BindGroupEntry { binding: 5, resource: uniform.as_entire_binding() },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_brush_wet"), timestamp_writes: None });
        pass.set_pipeline(match stage {
            WetStage::StrokeCoverage => &self.stroke,
            WetStage::SingleDabCoverage => &self.single,
            WetStage::Color => &self.color,
            WetStage::SingleAccumulatedColor => &self.single_color,
        });
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(params.rect[2].div_ceil(8), params.rect[3].div_ceil(8), 1);
    }
}
