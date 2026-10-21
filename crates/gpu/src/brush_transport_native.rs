//! Transport in document channels, including CMYK's fourth ink and separate alpha.
use crate::brush::{Stage, TransportParams};
use wgpu::util::DeviceExt;

pub struct NativeTransportPipelines {
    layout: wgpu::BindGroupLayout,
    pipelines: [wgpu::ComputePipeline; 7],
    lut: wgpu::Buffer,
}
impl NativeTransportPipelines {
    pub fn new(device: &wgpu::Device) -> Self {
        let entries: Vec<_> = (0..14)
            .map(|binding| {
                let ty = match binding {
                    2 | 5 | 10 | 11 => wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    3 => wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    7 => wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    _ => wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                };
                wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::COMPUTE, ty, count: None }
            })
            .collect();
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_native_transport"), entries: &entries });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_native_transport"),
            source: wgpu::ShaderSource::Wgsl(format!("{}\n{}", include_str!("brush_pigment_mix.wgsl"), include_str!("brush_transport_native.wgsl")).into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_native_transport"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipelines = ["pickup", "blur", "carrier_v1", "carrier_v2", "deposit_v1", "deposit_v2", "color_output"].map(|entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pl),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        });
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
            label: Some("pc_native_transport_lut"),
            contents: bytemuck::cast_slice(&table),
            usage: wgpu::BufferUsages::STORAGE,
        });
        Self { layout, pipelines, lut }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: [&wgpu::Texture; 2],
        coverage: &wgpu::Texture,
        output: [&wgpu::Texture; 2],
        carry: [&wgpu::Texture; 2],
        next: [&wgpu::Texture; 2],
        color: [&wgpu::Texture; 2],
        params: TransportParams,
        kind: [u32; 4],
        stage: Stage,
    ) {
        #[repr(C)]
        #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
        struct Params {
            transport: TransportParams,
            kind: [u32; 4],
        }
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_native_transport_params"),
            contents: bytemuck::bytes_of(&Params { transport: params, kind }),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let textures = [
            (0, source[0]),
            (1, coverage),
            (2, output[0]),
            (4, carry[0]),
            (5, next[0]),
            (6, color[0]),
            (8, source[1]),
            (9, carry[1]),
            (10, next[1]),
            (11, output[1]),
            (12, color[1]),
            (13, color[1]),
        ];
        let views: Vec<_> = textures.iter().map(|(_, t)| t.create_view(&Default::default())).collect();
        let mut entries: Vec<_> = textures
            .iter()
            .zip(&views)
            .map(|((binding, _), view)| wgpu::BindGroupEntry { binding: *binding, resource: wgpu::BindingResource::TextureView(view) })
            .collect();
        entries.push(wgpu::BindGroupEntry { binding: 3, resource: uniform.as_entire_binding() });
        entries.push(wgpu::BindGroupEntry { binding: 7, resource: self.lut.as_entire_binding() });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("pc_native_transport"), layout: &self.layout, entries: &entries });
        let i = match stage {
            Stage::Pickup => 0,
            Stage::Blur => 1,
            Stage::CarrierV1 => 2,
            Stage::CarrierV2 => 3,
            Stage::DepositV1 => 4,
            Stage::DepositV2 => 5,
            Stage::ColorOutput => 6,
        };
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_native_transport"), timestamp_writes: None });
        pass.set_pipeline(&self.pipelines[i]);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(params.rect[2].div_ceil(8), params.rect[3].div_ceil(8), 1);
    }
}
