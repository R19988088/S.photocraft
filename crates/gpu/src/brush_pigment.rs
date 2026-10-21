//! GPU ports of the 38-band PM response and signed pigment material field.
use wgpu::util::DeviceExt;

pub const SHADER: &str = include_str!("brush_pigment.wgsl");
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

/// Last covering dab's resolved material controls, stored per pixel, never interpolated.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PigmentState {
    /// enabled (0/1), pressure, signed thickness, visual thickness.
    pub material: [f32; 4],
    /// liquidity, highlight strength, highlight brightness, highlight ratio.
    pub highlight: [f32; 4],
    /// capped dab motion x/y; remaining lanes unused.
    pub motion: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PigmentParams {
    /// x, y, width, height, relative to the bound textures.
    pub rect: [u32; 4],
    /// paint amount, paint strength, dab flow, dab opacity × brush opacity.
    pub blend: [f32; 4],
    pub state: PigmentState,
}

/// Fractional hard-tip samples calculated by the tip rasterizer, before grain/flow/wet edges.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct EmbossSamples {
    /// inverse-rotated normalized local x/y, texture pattern, coverage / dab alpha.
    pub local_pattern_mask: [f32; 4],
    /// Compass order: +x, -x, +y, -y, ++, -+, +-, --.
    pub near: [f32; 8],
    pub far: [f32; 8],
}

#[derive(Clone, Copy, Debug)]
pub enum PigmentStage {
    Paint,
    Accumulate,
    Prepare,
    Shade,
    Emboss,
}

pub struct PigmentPipelines {
    layout: wgpu::BindGroupLayout,
    pipelines: [wgpu::ComputePipeline; 5],
    empty_emboss: wgpu::Buffer,
    lut: wgpu::Buffer,
    copy_layout: wgpu::BindGroupLayout,
    copy_pipeline: wgpu::ComputePipeline,
}

impl PigmentPipelines {
    pub fn new(device: &wgpu::Device) -> Self {
        let mut entries: Vec<_> = (0..6)
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
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: 6,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format: FORMAT,
                view_dimension: wgpu::TextureViewDimension::D2,
            },
            count: None,
        });
        for (binding, ty) in [
            (7, wgpu::BufferBindingType::Uniform),
            (8, wgpu::BufferBindingType::Storage { read_only: true }),
            (9, wgpu::BufferBindingType::Storage { read_only: false }),
            (10, wgpu::BufferBindingType::Storage { read_only: true }),
        ] {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            });
        }
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_pigment"), entries: &entries });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_pigment"),
            source: wgpu::ShaderSource::Wgsl(format!("{}\n{}", include_str!("brush_pigment_mix.wgsl"), SHADER).into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_pigment"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipelines = ["paint", "accumulate", "prepare", "shade", "emboss"].map(|entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        let lut = create_pm_lut(device);
        let empty_emboss = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_empty_emboss"),
            contents: bytemuck::bytes_of(&EmbossSamples::default()),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let copy_entries = [
            (0, wgpu::BufferBindingType::Storage { read_only: true }),
            (1, wgpu::BufferBindingType::Storage { read_only: false }),
            (2, wgpu::BufferBindingType::Uniform),
        ]
        .map(|(binding, ty)| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        });
        let copy_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_pigment_state_copy"), entries: &copy_entries });
        let copy_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_pigment_state_copy"),
            source: wgpu::ShaderSource::Wgsl(include_str!("brush_pigment_copy.wgsl").into()),
        });
        let copy_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_pigment_state_copy"),
            bind_group_layouts: &[Some(&copy_layout)],
            immediate_size: 0,
        });
        let copy_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pc_pigment_state_copy"),
            layout: Some(&copy_pl),
            module: &copy_module,
            entry_point: Some("copy_states"),
            compilation_options: Default::default(),
            cache: None,
        });
        Self { layout, pipelines, lut, empty_emboss, copy_layout, copy_pipeline }
    }

    /// Reindex a resident state field after changing its texture extent or origin. Source
    /// pixel (0,0) is placed at `target_offset` in the zero-initialized target buffer.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_state_copy(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::Buffer,
        target: &wgpu::Buffer,
        source_size: [u32; 2],
        target_size: [u32; 2],
        target_offset: [i32; 2],
    ) {
        let source_origin = [target_offset[0].saturating_neg().max(0) as u32, target_offset[1].saturating_neg().max(0) as u32];
        let target_origin = [target_offset[0].max(0) as u32, target_offset[1].max(0) as u32];
        let extent = [
            source_size[0].saturating_sub(source_origin[0]).min(target_size[0].saturating_sub(target_origin[0])),
            source_size[1].saturating_sub(source_origin[1]).min(target_size[1].saturating_sub(target_origin[1])),
        ];
        self.encode_state_region_copy(device, encoder, source, target, source_size, target_size, source_origin, target_origin, extent);
    }

    /// Copy a bounded rectangle between dense state fields, dispatching only its extent.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_state_region_copy(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::Buffer,
        target: &wgpu::Buffer,
        source_size: [u32; 2],
        target_size: [u32; 2],
        source_origin: [u32; 2],
        target_origin: [u32; 2],
        extent: [u32; 2],
    ) {
        if extent.contains(&0) {
            return;
        }
        #[repr(C)]
        #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
        struct CopyParams {
            sizes: [u32; 4],
            origins: [u32; 4],
            extent: [u32; 4],
        }
        let params = CopyParams {
            sizes: [source_size[0], source_size[1], target_size[0], target_size[1]],
            origins: [source_origin[0], source_origin[1], target_origin[0], target_origin[1]],
            extent: [extent[0], extent[1], 0, 0],
        };
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_pigment_state_copy_params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pc_pigment_state_copy"),
            layout: &self.copy_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: source.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: target.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: uniform.as_entire_binding() },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_pigment_state_copy"), timestamp_writes: None });
        pass.set_pipeline(&self.copy_pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(extent[0].div_ceil(8), extent[1].div_ceil(8), 1);
    }

    /// State buffer is width × height × `size_of::<PigmentState>()`, initially zeroed.
    /// All six sampled textures and output share an extent. Output must be distinct from inputs.
    /// Paint: inputs 0/1/2 = normal/destination/source; input 5.r = per-pixel amount.
    /// Accumulate: 0 = previous material, 1.r = pre-flow coverage, 2.r = exact emboss.
    /// Prepare: 0 = raw material, 5.r = final coverage × lane opacity × selection.
    /// Shade: 0 = normal, 3 = prepared material, 4 = candidate colour including pre-stroke
    /// fallback outside candidates, 5.r = coverage × selection (write gate).
    /// Each call writes only rect; preserve other pixels with a GPU copy or disjoint dispatches.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        inputs: [&wgpu::TextureView; 6],
        output: &wgpu::TextureView,
        states: &wgpu::Buffer,
        params: PigmentParams,
        stage: PigmentStage,
    ) {
        self.encode_with_emboss(device, encoder, inputs, output, states, &self.empty_emboss, params, stage);
    }

    /// `samples` is a dense full-texture EmbossSamples storage buffer, generated on the GPU
    /// by the exact hard-tip evaluator at step=clamp(radius*.075,.65,3) and 2.15×step.
    /// Output r contains emboss; g/b/a are zero. Other inputs and states are unused by this stage.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_emboss(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        inputs: [&wgpu::TextureView; 6],
        output: &wgpu::TextureView,
        states: &wgpu::Buffer,
        samples: &wgpu::Buffer,
        params: PigmentParams,
    ) {
        self.encode_with_emboss(device, encoder, inputs, output, states, samples, params, PigmentStage::Emboss);
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_with_emboss(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        inputs: [&wgpu::TextureView; 6],
        output: &wgpu::TextureView,
        states: &wgpu::Buffer,
        samples: &wgpu::Buffer,
        params: PigmentParams,
        stage: PigmentStage,
    ) {
        if params.rect[2] == 0 || params.rect[3] == 0 {
            return;
        }
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_pigment_params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let mut entries: Vec<_> = inputs
            .into_iter()
            .enumerate()
            .map(|(binding, view)| wgpu::BindGroupEntry { binding: binding as u32, resource: wgpu::BindingResource::TextureView(view) })
            .collect();
        entries.extend([
            wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(output) },
            wgpu::BindGroupEntry { binding: 7, resource: uniform.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 8, resource: self.lut.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 9, resource: states.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 10, resource: samples.as_entire_binding() },
        ]);
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("pc_pigment"), layout: &self.layout, entries: &entries });
        let index = match stage {
            PigmentStage::Paint => 0,
            PigmentStage::Accumulate => 1,
            PigmentStage::Prepare => 2,
            PigmentStage::Shade => 3,
            PigmentStage::Emboss => 4,
        };
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_pigment"), timestamp_writes: None });
        pass.set_pipeline(&self.pipelines[index]);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(params.rect[2].div_ceil(8), params.rect[3].div_ceil(8), 1);
    }
}

/// Decode the bundled PM table exactly as the CPU reference and upload it once per device.
pub fn create_pm_lut(device: &wgpu::Device) -> wgpu::Buffer {
    // Match pigment.rs's binary16 decoding, including its handling of exponent 31.
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
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("pc_PM_lut"),
        contents: bytemuck::cast_slice(&table),
        usage: wgpu::BufferUsages::STORAGE,
    })
}
