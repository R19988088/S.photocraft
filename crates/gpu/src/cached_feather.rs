//! Exact three-pass extended box approximation used by mask Properties feather.
use wgpu::util::DeviceExt;
pub struct Feather {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}
impl Feather {
    pub fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("liquify feather"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::R32Float,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
            ],
        });
        let blur_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("liquify feather"),
            source: wgpu::ShaderSource::Wgsl(include_str!("cached_feather.wgsl").into()),
        });
        let blur_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("liquify feather"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("liquify feather"),
            layout: Some(&blur_pl),
            module: &blur_shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        Self { pipeline, layout }
    }
    /// The first plane contains raw coverage; all planes are equally sized R32Float.
    /// The first two planes are scratch and may be overwritten.
    pub fn encode(&self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, planes: [&wgpu::Texture; 3], sigma: f32) {
        self.encode_clipped(device, encoder, planes, sigma, None);
    }
    /// Blur inside a canvas rectangle expressed in the planes' local coordinates.
    pub fn encode_clipped(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        planes: [&wgpu::Texture; 3],
        sigma: f32,
        clip: Option<photocraft_geom::Rect>,
    ) {
        let [first, second, destination] = planes;
        if sigma <= 0. && clip.is_none() {
            encoder.copy_texture_to_texture(first.as_image_copy(), destination.as_image_copy(), first.size());
            return;
        }
        let v = f64::from(sigma).powi(2) / 3.;
        let mut radius = (((12. * v + 1.).sqrt() - 1.) / 2.).floor().max(0.) as u32;
        let variance = |r: u32| f64::from(r) * f64::from(r + 1) / 3.;
        while radius > 0 && variance(radius) > v {
            radius -= 1;
        }
        while variance(radius + 1) <= v {
            radius += 1;
        }
        let r = f64::from(radius);
        let n = 2. * r + 1.;
        let end = (n * (r * (r + 1.) / 3. - v) / (2. * v - 2. * (r + 1.).powi(2))).clamp(0., 1.) as f32;
        let stages = if sigma <= 0. { 1 } else { 6 };
        for stage in 0..stages {
            let (source, target) = if stages == 1 {
                (first, destination)
            } else {
                match stage {
                    0 | 2 | 4 => (first, second),
                    1 | 3 => (second, first),
                    _ => (second, destination),
                }
            };
            let data = [radius as f32, end, if stage < 3 { 0. } else { 1. }, 0.];
            let extent = photocraft_geom::Rect::new(0, 0, first.width() as i32, first.height() as i32);
            let clipped = clip.map_or(extent, |clip| clip.intersect(&extent));
            let clipped = if clipped.is_empty() { extent } else { clipped };
            let mut parameters = bytemuck::cast_slice(&data).to_vec();
            parameters.extend_from_slice(bytemuck::cast_slice(&[clipped.x0, clipped.y0, clipped.x1, clipped.y1]));
            let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cached feather parameters"),
                contents: &parameters,
                usage: wgpu::BufferUsages::UNIFORM,
            });
            let view = source.create_view(&Default::default());
            let target_view = target.create_view(&Default::default());
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("liquify feather"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: params.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&target_view) },
                ],
            });
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("liquify feather sliding sums"), timestamp_writes: None });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(if stage < 3 { first.height() } else { first.width() }.div_ceil(64), 1, 1);
        }
    }
}
