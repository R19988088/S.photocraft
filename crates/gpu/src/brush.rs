//! GPU resources for sequential brush transport. Pixel state belongs to a stroke, while the
//! pipelines are shared by strokes on the same device.
use wgpu::util::DeviceExt;

pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;
pub const SHADER: &str = include_str!("brush.wgsl");

/// All positions are relative to the working texture. `rect` limits writes to the dab footprint.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TransportParams {
    pub rect: [u32; 4],
    pub offset: [f32; 2],
    pub strength: f32,
    pub blur_radius: f32,
    pub target: [f32; 4],
    pub center: [f32; 2],
    pub radius: f32,
    pub previous_radius: f32,
    pub reservoir: [f32; 4],
    pub sample_rect: [i32; 4],
    /// pigment strength in a; per-pixel paint colour is supplied separately.
    pub paint: [f32; 4],
}

#[derive(Clone, Copy, Debug)]
pub enum Stage {
    Pickup,
    Blur,
    CarrierV1,
    CarrierV2,
    DepositV1,
    DepositV2,
    ColorOutput,
}

pub struct TransportPipelines {
    layout: wgpu::BindGroupLayout,
    pickup: wgpu::ComputePipeline,
    carrier_v1: wgpu::ComputePipeline,
    carrier_v2: wgpu::ComputePipeline,
    blur: wgpu::ComputePipeline,
    deposit_v1: wgpu::ComputePipeline,
    deposit_v2: wgpu::ComputePipeline,
    color: wgpu::ComputePipeline,
    lut: wgpu::Buffer,
}

impl TransportPipelines {
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
            label: Some("pc_brush_transport"),
            entries: &[
                texture(0),
                texture(1),
                texture(4),
                texture(6),
                wgpu::BindGroupLayoutEntry {
                    binding: 7,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_brush_transport"),
            source: wgpu::ShaderSource::Wgsl(format!("{}\n{}", include_str!("brush_pigment_mix.wgsl"), SHADER).into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_brush_transport"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let make = |entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pl),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let table: Vec<f32> = include_bytes!("../../paint/assets/PM_lut.bin")
            .chunks_exact(2)
            .map(|v| {
                let bits = u16::from_le_bytes([v[0], v[1]]);
                let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
                let exponent = i32::from((bits >> 10) & 31);
                let mantissa = f32::from(bits & 1023);
                if exponent == 0 { sign * mantissa * 2.0_f32.powi(-24) } else { sign * (1.0 + mantissa / 1024.0) * 2.0_f32.powi(exponent - 15) }
            })
            .collect();
        let lut = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_transport_PM_lut"),
            contents: bytemuck::cast_slice(&table),
            usage: wgpu::BufferUsages::STORAGE,
        });
        Self {
            pickup: make("pickup"),
            blur: make("blur"),
            carrier_v1: make("carrier_v1"),
            carrier_v2: make("carrier_v2"),
            deposit_v1: make("deposit_v1"),
            deposit_v2: make("deposit_v2"),
            color: make("color_output"),
            layout,
            lut,
        }
    }

    /// Source and output must be distinct textures. Pixels outside `rect` remain untouched.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::TextureView,
        coverage: &wgpu::TextureView,
        output: &wgpu::TextureView,
        carry: &wgpu::TextureView,
        next_carry: &wgpu::TextureView,
        color_source: &wgpu::TextureView,
        params: TransportParams,
        stage: Stage,
    ) {
        if params.rect[2] == 0 || params.rect[3] == 0 {
            return;
        }
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_brush_transport_params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_brush_transport"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(source) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(coverage) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(output) },
                wgpu::BindGroupEntry { binding: 3, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(carry) },
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(next_carry) },
                wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(color_source) },
                wgpu::BindGroupEntry { binding: 7, resource: self.lut.as_entire_binding() },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_brush_transport"), timestamp_writes: None });
        pass.set_pipeline(match stage {
            Stage::Pickup => &self.pickup,
            Stage::Blur => &self.blur,
            Stage::CarrierV1 => &self.carrier_v1,
            Stage::CarrierV2 => &self.carrier_v2,
            Stage::DepositV1 => &self.deposit_v1,
            Stage::DepositV2 => &self.deposit_v2,
            Stage::ColorOutput => &self.color,
        });
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups(params.rect[2].div_ceil(8), params.rect[3].div_ceil(8), 1);
    }
}

/// Working colour and carrier state for one brush lane. Primary, secondary and symmetry lanes
/// must each own a separate instance. All intermediate copies stay on the device.
pub struct LaneTextures {
    work: [wgpu::Texture; 2],
    carry: [wgpu::Texture; 2],
    work_index: usize,
    carry_index: usize,
    pub size: [u32; 2],
}

impl LaneTextures {
    pub fn new(device: &wgpu::Device, size: [u32; 2], carrier_size: [u32; 2]) -> Self {
        let texture = |label, size: [u32; 2]| {
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
        Self {
            work: [texture("pc_brush_work_a", size), texture("pc_brush_work_b", size)],
            carry: [texture("pc_brush_carry_a", carrier_size), texture("pc_brush_carry_b", carrier_size)],
            work_index: 0,
            carry_index: 0,
            size,
        }
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.work[self.work_index]
    }

    /// Seed the lane once at stroke start, with a GPU copy of the pre-stroke layer.
    pub fn initialize(&mut self, encoder: &mut wgpu::CommandEncoder, source: &wgpu::Texture) {
        encoder.copy_texture_to_texture(
            source.as_image_copy(),
            self.texture().as_image_copy(),
            wgpu::Extent3d { width: self.size[0], height: self.size[1], depth_or_array_layers: 1 },
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &mut self,
        pipelines: &TransportPipelines,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        coverage: &wgpu::TextureView,
        color_source: &wgpu::TextureView,
        mut params: TransportParams,
        stage: Stage,
    ) {
        let updates_carrier = matches!(stage, Stage::CarrierV1 | Stage::CarrierV2);
        let next_work = 1 - self.work_index;
        if updates_carrier {
            let size = self.carry[1 - self.carry_index].size();
            params.rect = [0, 0, size.width, size.height];
        }
        pipelines.encode(
            device,
            encoder,
            &self.texture().create_view(&Default::default()),
            coverage,
            &self.work[next_work].create_view(&Default::default()),
            &self.carry[self.carry_index].create_view(&Default::default()),
            &self.carry[1 - self.carry_index].create_view(&Default::default()),
            color_source,
            params,
            stage,
        );
        if updates_carrier {
            self.carry_index = 1 - self.carry_index;
        } else if params.rect[2] > 0 && params.rect[3] > 0 {
            let origin = wgpu::Origin3d { x: params.rect[0], y: params.rect[1], z: 0 };
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo { origin, ..self.work[next_work].as_image_copy() },
                wgpu::TexelCopyTextureInfo { origin, ..self.texture().as_image_copy() },
                wgpu::Extent3d { width: params.rect[2], height: params.rect[3], depth_or_array_layers: 1 },
            );
        }
    }
}
