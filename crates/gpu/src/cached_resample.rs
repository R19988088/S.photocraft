//! Native-channel bicubic prefilter. Only one-dimensional weights are sent by the CPU.
use crate::{Unsupported, cached_image::Image};
use photocraft_geom::Rect;
use wgpu::util::DeviceExt;
pub struct Resample {
    pipeline: wgpu::RenderPipeline,
}
impl Resample {
    pub fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cached native reduction"),
            source: wgpu::ShaderSource::Wgsl(include_str!("cached_resample.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cached native reduction"),
            layout: None,
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vertex"), compilation_options: Default::default(), buffers: &[] },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                targets: &[
                    Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Rgba32Float, blend: None, write_mask: wgpu::ColorWrites::ALL }),
                    Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::R32Float, blend: None, write_mask: wgpu::ColorWrites::ALL }),
                ],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: [&Image; 2],
        rect: Rect,
        scale: f64,
        format: photocraft_color::PixelFormat,
        default: &[f32],
    ) -> Result<([Image; 2], Rect), Unsupported> {
        let sample = match format.sample {
            photocraft_color::SampleType::U8 => 0,
            photocraft_color::SampleType::U16 => 1,
            _ => 2,
        };
        let mut current: Option<[Image; 2]> = None;
        let mut src = rect;
        for axis in 0..2 {
            let dst = photocraft_algo::resample::scaled_rect(src, if axis == 0 { scale } else { 1.0 }, if axis == 1 { scale } else { 1.0 });
            let taps = photocraft_algo::resample::bicubic_axis_taps(
                if axis == 0 { dst.x0 } else { dst.y0 },
                if axis == 0 { dst.x1 } else { dst.y1 },
                scale,
                if axis == 0 { (src.x0, src.x1 - 1) } else { (src.y0, src.y1 - 1) },
            );
            let mut index = Vec::<[i32; 4]>::new();
            let mut weights = Vec::new();
            for (start, w, outside) in taps {
                index.push([start, w.len() as i32, weights.len() as i32, outside.to_bits() as i32]);
                weights.extend(w);
            }
            if weights.is_empty() {
                weights.push(0.0);
            }
            let indices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cached reduction taps"),
                contents: bytemuck::cast_slice(&index),
                usage: wgpu::BufferUsages::STORAGE,
            });
            let weights = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("cached reduction weights"),
                contents: bytemuck::cast_slice(&weights),
                usage: wgpu::BufferUsages::STORAGE,
            });
            let input = current.as_ref().map_or(source, |v| [&v[0], &v[1]]);
            let output = [Image::new_empty(device, dst, wgpu::TextureFormat::Rgba32Float)?, Image::new_empty(device, dst, wgpu::TextureFormat::R32Float)?];
            let views = input.map(Image::view);
            for (layer, page) in output[0].pages() {
                let columns = src.width().div_ceil(input[0].side);
                let mut params_data = bytemuck::cast_slice(&[
                    src.x0,
                    src.y0,
                    src.x1,
                    src.y1,
                    page.x0,
                    page.y0,
                    page.x1,
                    page.y1,
                    axis,
                    sample,
                    input[0].side as i32,
                    columns as i32,
                ])
                .to_vec();
                let mut def = [0f32; 8];
                let colors = format.mode.color_channels();
                for (k, channel) in def.iter_mut().take(colors.min(4)).enumerate() {
                    *channel = default.get(k).copied().unwrap_or(0.0);
                }
                def[4] = if format.alpha { default.get(colors).copied().unwrap_or(0.0) } else { 1.0 };
                params_data.extend_from_slice(bytemuck::cast_slice(&def));
                params_data.extend_from_slice(bytemuck::cast_slice(&[dst.x0, dst.y0, dst.x1, dst.y1]));
                let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("cached reduction parameters"),
                    contents: &params_data,
                    usage: wgpu::BufferUsages::UNIFORM,
                });
                let out_views = output.each_ref().map(|t| t.layer_view(layer));
                let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("cached reduction"),
                    layout: &self.pipeline.get_bind_group_layout(0),
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&views[0]) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&views[1]) },
                        wgpu::BindGroupEntry { binding: 2, resource: indices.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 3, resource: weights.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 4, resource: params.as_entire_binding() },
                    ],
                });
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("cached reduction"),
                        color_attachments: &out_views
                            .iter()
                            .map(|view| {
                                Some(wgpu::RenderPassColorAttachment {
                                    view,
                                    resolve_target: None,
                                    depth_slice: None,
                                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                                })
                            })
                            .collect::<Vec<_>>(),
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    pass.set_viewport(0.0, 0.0, page.width() as f32, page.height() as f32, 0.0, 1.0);
                    pass.set_scissor_rect(0, 0, page.width(), page.height());
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &group, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
            current = Some(output);
            src = dst;
        }
        current.map(|images| (images, src)).ok_or_else(|| Unsupported("cached reduction produced no axis".into()))
    }
}
