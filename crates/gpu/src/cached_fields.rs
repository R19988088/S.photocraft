//! GPU distance fields for resident geometric previews. Source shape and intermediates never
//! leave the device; the compositor reuses these pipelines across preview frames.
use photocraft_compose::effects::FieldKind;
use wgpu::util::DeviceExt;

use crate::Unsupported;

pub(crate) struct Fields {
    layout: wgpu::BindGroupLayout,
    pipelines: Vec<wgpu::ComputePipeline>,
    scratch: Option<Scratch>,
    crop_layout: wgpu::BindGroupLayout,
    crop_pipeline: wgpu::ComputePipeline,
}

struct Scratch {
    buffers: [wgpu::Buffer; 5],
}

impl Fields {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let mut entries = vec![
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
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
        ];
        for binding in 2..7 {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only: false }, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            });
        }
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: 7,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format: wgpu::TextureFormat::R32Float,
                view_dimension: wgpu::TextureViewDimension::D2,
            },
            count: None,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("cached field bindings"), entries: &entries });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cached distance fields"),
            source: wgpu::ShaderSource::Wgsl(include_str!("cached_fields.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cached distance fields"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipelines = ["seed", "columns", "rows", "relax", "edges", "refine", "finish"]
            .iter()
            .map(|entry| {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&pipeline_layout),
                    module: &shader,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })
            .collect();
        let crop_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cached shape window"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
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
        let crop_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cached shape window"),
            source: wgpu::ShaderSource::Wgsl(include_str!("cached_shape_window.wgsl").into()),
        });
        let crop_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cached shape window"),
            bind_group_layouts: &[Some(&crop_layout)],
            immediate_size: 0,
        });
        let crop_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("cached shape window"),
            layout: Some(&crop_pl),
            module: &crop_shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        Self { layout, pipelines, scratch: None, crop_layout, crop_pipeline }
    }

    /// Extract an effect page and its apron on the GPU, including transparent space outside
    /// the cached source. Both distance-field scratch and effect maps then stay page-sized.
    pub(crate) fn shape_window(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        shape: &wgpu::TextureView,
        offset: [i32; 2],
        size: [u32; 2],
    ) -> Result<wgpu::Texture, Unsupported> {
        if size.contains(&0) || size[0].max(size[1]) > device.limits().max_texture_dimension_2d {
            return Err(Unsupported("cached shape page dimensions".into()));
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cached shape page"),
            size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING,
            view_formats: &[],
        });
        let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cached shape origin"),
            contents: bytemuck::cast_slice(&[offset[0], offset[1], 0, 0]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let view = texture.create_view(&Default::default());
        let binding = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cached shape page"),
            layout: &self.crop_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(shape) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&view) },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("cached shape window"), timestamp_writes: None });
        pass.set_pipeline(&self.crop_pipeline);
        pass.set_bind_group(0, &binding, &[]);
        pass.dispatch_workgroups(size[0].div_ceil(8), size[1].div_ceil(8), 1);
        drop(pass);
        Ok(texture)
    }

    pub(crate) fn encode(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        shape: &wgpu::TextureView,
        size: [u32; 2],
        kind: FieldKind,
        reach: i32,
    ) -> Result<wgpu::Texture, Unsupported> {
        let [w, h] = size;
        if w == 0 || h == 0 || w.max(h) > device.limits().max_texture_dimension_2d {
            return Err(Unsupported("cached effect field dimensions".into()));
        }
        let bytes =
            u64::from(w).checked_mul(u64::from(h)).and_then(|n| n.checked_mul(16)).ok_or_else(|| Unsupported("cached effect field allocation".into()))?;
        let envelope_bytes = (u64::from(w) * (u64::from(h) + 1)).max(u64::from(h) * (u64::from(w) + 1)) * 8;
        if bytes.max(envelope_bytes) > device.limits().max_storage_buffer_binding_size {
            return Err(Unsupported("cached effect field exceeds GPU page budget".into()));
        }
        if self
            .scratch
            .as_ref()
            .is_none_or(|s| s.buffers.first().is_none_or(|b| b.size() < bytes) || s.buffers.get(2).is_none_or(|b| b.size() < envelope_bytes))
        {
            let buffers = std::array::from_fn(|i| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("cached field scratch"),
                    size: if i == 2 { envelope_bytes } else { bytes },
                    usage: wgpu::BufferUsages::STORAGE,
                    mapped_at_creation: false,
                })
            });
            self.scratch = Some(Scratch { buffers });
        }
        let Some(scratch) = &self.scratch else { return Err(Unsupported("cached effect field resources".into())) };
        let code = match kind {
            FieldKind::Outside => 0,
            FieldKind::Inside => 1,
            FieldKind::BevelOutside => 2,
            FieldKind::BevelInside => 3,
            FieldKind::StrokeOutside => 4,
            FieldKind::ChokeInside => 5,
            FieldKind::StrokeInside => 6,
            FieldKind::StrokeOutsideExact => 7,
            FieldKind::StrokeOutsideVector => 8,
            FieldKind::StrokeOutsideVectorChamfer => 9,
        };
        let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("cached field parameters"),
            contents: bytemuck::cast_slice(&[w, h, code, reach.max(0) as u32]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cached effect distance"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = output.create_view(&Default::default());
        let bindings: Vec<_> = [false, true]
            .into_iter()
            .map(|swap| {
                let mut entries = vec![
                    wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(shape) },
                ];
                for i in 0..5usize {
                    let index = if swap && i < 2 { 1 - i } else { i };
                    if let Some(buffer) = scratch.buffers.get(index) {
                        entries.push(wgpu::BindGroupEntry { binding: i as u32 + 2, resource: buffer.as_entire_binding() });
                    }
                }
                entries.push(wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(&view) });
                device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("cached field dispatch"), layout: &self.layout, entries: &entries })
            })
            .collect();
        let dispatch = |encoder: &mut wgpu::CommandEncoder, stage: usize, swap: bool, groups: [u32; 2]| {
            if let (Some(pipeline), Some(group)) = (self.pipelines.get(stage), bindings.get(usize::from(swap))) {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("cached effect distance"), timestamp_writes: None });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, group, &[]);
                pass.dispatch_workgroups(groups[0], groups[1], 1);
            }
        };
        let grid = [w.div_ceil(8), h.div_ceil(8)];
        dispatch(encoder, 0, false, grid);
        let chamfer = matches!(kind, FieldKind::StrokeOutside | FieldKind::ChokeInside | FieldKind::StrokeInside | FieldKind::StrokeOutsideVectorChamfer);
        let mut swap = false;
        if chamfer {
            // A local step advances at least one pixel. Past the effect's reach consumers saturate.
            for _ in 0..reach.max(0).saturating_add(3) {
                dispatch(encoder, 3, swap, grid);
                swap = !swap;
            }
        } else {
            dispatch(encoder, 1, false, [w.div_ceil(64), 1]);
            dispatch(encoder, 2, true, [h.div_ceil(64), 1]);
        }
        if matches!(kind, FieldKind::BevelOutside | FieldKind::BevelInside) {
            dispatch(encoder, 4, swap, grid);
            dispatch(encoder, 5, swap, grid);
        }
        dispatch(encoder, 6, swap, grid);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moved_feathered_group_shadow_field_matches_cpu() {
        use photocraft_color::{ColorMode, PixelFormat, SampleType};
        use photocraft_doc::{Layer, LayerMask};
        use photocraft_geom::Rect;
        let adapter = pollster::block_on(wgpu::Instance::default().request_adapter(&Default::default())).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        for region in [Rect::new(-20, -20, 68, 60), Rect::new(-20, 5, 16, 41), Rect::new(0, 5, 16, 40)] {
            let (w, h) = (region.width(), region.height());
            let mut group = Layer::group("group", vec![]);
            let mut mask = LayerMask::reveal_all();
            mask.density = 0.65;
            mask.feather = 2.2;
            mask.surface.fill_rect(Rect::new(-3, 13, 1, 35), &[0.]);
            group.mask = Some(mask);
            let mask = photocraft_compose::masks::combined_mask(&group, region).unwrap();
            let mut pixels = photocraft_raster::Surface::new(PixelFormat::new(ColorMode::Rgb, SampleType::F32, true));
            pixels.fill_rect(Rect::new(-10, 15, 6, 31), &[0.1, 0.8, 0.3, 0.9]);
            let mut alpha = Vec::new();
            for y in region.y0..region.y1 {
                for x in region.x0..region.x1 {
                    alpha.push(pixels.sample_channel(x, y, 3) * mask.sample_channel(x, y, 0));
                }
            }
            let shape = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                shape.as_image_copy(),
                bytemuck::cast_slice(&alpha),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                shape.size(),
            );
            let mut fields = Fields::new(&device);
            let mut encoder = device.create_command_encoder(&Default::default());
            let output = fields.encode(&device, &mut encoder, &shape.create_view(&Default::default()), [w, h], FieldKind::StrokeOutside, 3).unwrap();
            let stride = (w * 4).div_ceil(256) * 256;
            let read = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(stride * h),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                output.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &read,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(h) },
                },
                output.size(),
            );
            queue.submit([encoder.finish()]);
            let (send, recv) = std::sync::mpsc::channel();
            read.slice(..).map_async(wgpu::MapMode::Read, move |r| send.send(r).unwrap());
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            recv.recv().unwrap().unwrap();
            let bytes = read.slice(..).get_mapped_range().unwrap();
            let reference = photocraft_compose::effects::distance_field(FieldKind::StrokeOutside, alpha, w as usize, h as usize, 3.);
            let mut worst = 0f32;
            let mut at = (0, 0, 0f32, 0f32);
            for y in 0..h {
                for x in 0..w {
                    let i = (y * stride + x * 4) as usize;
                    let a = f32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
                    let b = reference[(y * w + x) as usize];
                    let d = (a.min(3.) - b.min(3.)).abs();
                    if d > worst {
                        worst = d;
                        at = (region.x0 + x as i32, region.y0 + y as i32, a, b);
                    }
                }
            }
            assert!(worst < 0.0005, "feathered shadow field region {region:?} delta {worst} at {at:?}");
        }
    }

    #[test]
    fn resident_fields_match_cpu_in_the_effect_reach() {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
        device.on_uncaptured_error(std::sync::Arc::new(|e| panic!("GPU fields: {e}")));
        let (w, h) = (48u32, 40u32);
        let alpha: Vec<f32> = (0..h)
            .flat_map(|y| {
                (0..w).map(move |x| {
                    let edge = (x as f32 - 0.31 * y as f32 - 7.25).clamp(0.0, 1.0);
                    let right = (38.4 - x as f32).clamp(0.0, 1.0);
                    let top = (y as f32 - 5.25).clamp(0.0, 1.0);
                    let bottom = (32.6 - y as f32).clamp(0.0, 1.0);
                    if (23..27).contains(&x) && (18..23).contains(&y) { 0.0 } else { edge * right * top * bottom * 0.8 }
                })
            })
            .collect();
        let shape = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &shape, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            bytemuck::cast_slice(&alpha),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            shape.size(),
        );
        let mut fields = Fields::new(&device);
        let mut window = device.create_command_encoder(&Default::default());
        let shape = fields.shape_window(&device, &mut window, &shape.create_view(&Default::default()), [2, -1], [w, h]).unwrap();
        queue.submit([window.finish()]);
        let alpha: Vec<f32> = (0..h)
            .flat_map(|y| {
                let alpha = &alpha;
                (0..w).map(move |x| if x + 2 < w && y > 0 { alpha[((y - 1) * w + x + 2) as usize] } else { 0.0 })
            })
            .collect();
        for kind in [
            FieldKind::Outside,
            FieldKind::Inside,
            FieldKind::BevelOutside,
            FieldKind::BevelInside,
            FieldKind::StrokeOutside,
            FieldKind::ChokeInside,
            FieldKind::StrokeInside,
            FieldKind::StrokeOutsideExact,
            FieldKind::StrokeOutsideVector,
            FieldKind::StrokeOutsideVectorChamfer,
        ] {
            let mut encoder = device.create_command_encoder(&Default::default());
            let output = fields.encode(&device, &mut encoder, &shape.create_view(&Default::default()), [w, h], kind, 10).unwrap();
            let stride = (w * 4).div_ceil(256) * 256;
            let read = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(stride * h),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture: &output, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo {
                    buffer: &read,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(h) },
                },
                output.size(),
            );
            queue.submit([encoder.finish()]);
            let (send, recv) = std::sync::mpsc::channel();
            read.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                send.send(r).unwrap();
            });
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            recv.recv().unwrap().unwrap();
            let bytes = read.slice(..).get_mapped_range().unwrap();
            let expected = photocraft_compose::effects::distance_field(kind, alpha.clone(), w as usize, h as usize, 10.0);
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let pos = y * stride as usize + x * 4;
                    let actual = f32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
                    let reference = expected[y * w as usize + x];
                    assert!((actual.min(10.0) - reference.min(10.0)).abs() < 0.0005, "{kind:?} ({x},{y}) GPU={actual} CPU={reference}");
                }
            }
            drop(bytes);
            read.unmap();
        }
    }
}
