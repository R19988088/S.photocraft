//! Resident document-space planes packed into texture-array pages.
use crate::Unsupported;
use photocraft_geom::Rect;

pub(crate) struct Image {
    pub texture: wgpu::Texture,
    pub rect: Rect,
    pub side: u32,
}

/// One explicit commit transfer: stage every plane before waiting on the device once.
pub(crate) fn readback(device: &wgpu::Device, queue: &wgpu::Queue, planes: &[(&wgpu::Texture, Rect, u32)]) -> Result<Vec<Vec<f32>>, Unsupported> {
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("cached native commit") });
    let mut buffers = Vec::new();
    for (texture, rect, channels) in planes {
        let row = (rect.width() * channels * 4).div_ceil(256) * 256;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cached native commit plane"),
            size: u64::from(row) * u64::from(rect.height()),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(rect.height()) },
            },
            wgpu::Extent3d { width: rect.width(), height: rect.height(), depth_or_array_layers: 1 },
        );
        buffers.push((buffer, row));
    }
    queue.submit([encoder.finish()]);
    for (buffer, _) in &buffers {
        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    }
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).map_err(|e| Unsupported(format!("cached commit GPU wait failed: {e}")))?;
    let mut output = Vec::new();
    for ((buffer, row), (_, rect, channels)) in buffers.iter().zip(planes) {
        let mapped = buffer.slice(..).get_mapped_range().map_err(|e| Unsupported(format!("cached commit readback failed: {e:?}")))?;
        let mut values = Vec::with_capacity(rect.width() as usize * rect.height() as usize * *channels as usize);
        for y in 0..rect.height() as usize {
            let bytes = &mapped[y * *row as usize..][..rect.width() as usize * *channels as usize * 4];
            values.extend(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        }
        output.push(values);
    }
    Ok(output)
}
impl Image {
    pub fn new_empty(device: &wgpu::Device, rect: Rect, format: wgpu::TextureFormat) -> Result<Self, Unsupported> {
        if rect.is_empty() {
            return Err(Unsupported("empty cached source plane".into()));
        }
        let limit = device.limits().max_texture_dimension_2d;
        let mut side = 1024.min(limit);
        while u64::from(rect.width().div_ceil(side)) * u64::from(rect.height().div_ceil(side)) > u64::from(device.limits().max_texture_array_layers) {
            if side == limit {
                return Err(Unsupported("cached source exceeds GPU array capacity".into()));
            }
            side = (side * 2).min(limit);
        }
        let layers = rect.width().div_ceil(side) * rect.height().div_ceil(side);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cached source pages"),
            size: wgpu::Extent3d { width: side.min(rect.width()), height: side.min(rect.height()), depth_or_array_layers: layers },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        Ok(Self { texture, rect, side })
    }
    pub fn view(&self) -> wgpu::TextureView {
        self.texture.create_view(&wgpu::TextureViewDescriptor { dimension: Some(wgpu::TextureViewDimension::D2Array), ..Default::default() })
    }
    pub fn layer_view(&self, layer: u32) -> wgpu::TextureView {
        self.texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2),
            base_array_layer: layer,
            array_layer_count: Some(1),
            ..Default::default()
        })
    }
    pub fn pages(&self) -> Vec<(u32, Rect)> {
        let columns = self.rect.width().div_ceil(self.side);
        (0..self.rect.height().div_ceil(self.side))
            .flat_map(|row| {
                (0..columns).map(move |col| {
                    let x0 = self.rect.x0 + (col * self.side) as i32;
                    let y0 = self.rect.y0 + (row * self.side) as i32;
                    (row * columns + col, Rect::new(x0, y0, (x0 + self.side as i32).min(self.rect.x1), (y0 + self.side as i32).min(self.rect.y1)))
                })
            })
            .collect()
    }
    pub fn upload(&self, queue: &wgpu::Queue, values: &[f32], channels: u32) {
        let bytes: &[u8] = bytemuck::cast_slice(values);
        let stride = self.rect.width() * channels * 4;
        for (layer, page) in self.pages() {
            let offset = (u64::from((page.y0 - self.rect.y0) as u32) * u64::from(stride) + u64::from((page.x0 - self.rect.x0) as u32) * u64::from(channels * 4))
                as usize;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
                    aspect: wgpu::TextureAspect::All,
                },
                &bytes[offset..],
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(page.height()) },
                wgpu::Extent3d { width: page.width(), height: page.height(), depth_or_array_layers: 1 },
            );
        }
    }
    pub fn copy_window(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::Texture, rect: Rect) {
        for (layer, page) in self.pages() {
            let area = page.intersect(&rect);
            if area.is_empty() {
                continue;
            }
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: (area.x0 - page.x0) as u32, y: (area.y0 - page.y0) as u32, z: layer },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: target,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: (area.x0 - rect.x0) as u32, y: (area.y0 - rect.y0) as u32, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d { width: area.width(), height: area.height(), depth_or_array_layers: 1 },
            );
        }
    }
}
