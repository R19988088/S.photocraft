//! CoverageMap::union_max for symmetry/tail passes, including material and resolved settings.
use wgpu::util::DeviceExt;

pub struct UnionPipelines {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
}
impl UnionPipelines {
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
        for binding in 8..12 {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rgba32Float,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            });
        }
        for (binding, ty) in [
            (12, wgpu::BufferBindingType::Storage { read_only: true }),
            (13, wgpu::BufferBindingType::Storage { read_only: true }),
            (14, wgpu::BufferBindingType::Storage { read_only: false }),
            (15, wgpu::BufferBindingType::Uniform),
        ] {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            });
        }
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_brush_union"), entries: &entries });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_brush_union"),
            source: wgpu::ShaderSource::Wgsl(include_str!("brush_union.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_brush_union"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pc_brush_union"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("union_max"),
            compilation_options: Default::default(),
            cache: None,
        });
        Self { layout, pipeline }
    }

    /// Inputs: existing cov/colour/wet/pigment, then incoming cov/colour/wet/pigment.
    /// Outputs: merged cov/colour/wet/pigment, each distinct from all sampled inputs.
    /// State buffers: existing, incoming, merged, each dense PigmentState fields with the
    /// texture's width/height. All resources cover the same texture-local coordinate space.
    /// `fields` bit 0/1/2 indicates incoming colour/wet/pigment exists; absent optional fields
    /// keep the existing value, matching the CPU's unallocated vectors. A newly created union
    /// tile should copy the complete incoming tile directly, including saturation memory.
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        inputs: [&wgpu::TextureView; 8],
        outputs: [&wgpu::TextureView; 4],
        states: [&wgpu::Buffer; 3],
        rect: [u32; 4],
        fields: u32,
    ) {
        if rect[2] == 0 || rect[3] == 0 {
            return;
        }
        let params = [rect, [fields, 0, 0, 0]];
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_brush_union_params"),
            contents: bytemuck::cast_slice(&params),
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
        entries.extend(states.into_iter().enumerate().map(|(i, buffer)| wgpu::BindGroupEntry { binding: i as u32 + 12, resource: buffer.as_entire_binding() }));
        entries.push(wgpu::BindGroupEntry { binding: 15, resource: uniform.as_entire_binding() });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("pc_brush_union"), layout: &self.layout, entries: &entries });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_brush_union"), timestamp_writes: None });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(rect[2].div_ceil(8), rect[3].div_ceil(8), 1);
    }
}
