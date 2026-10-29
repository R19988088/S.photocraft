//! Native document colour helpers and the exact CMYK tables used by the CPU colour manager.
use photocraft_color::{ColorMode, PixelFormat};
use wgpu::util::DeviceExt;

pub const SHADER: &str = include_str!("native_format.wgsl");

/// Immutable per-stroke profile tables. Upload once while the document CMYK space is active.
/// These are the CPU transform's existing tables, not a sampled approximation of its output.
#[derive(Clone, Debug)]
pub struct NativeFormat {
    pub format: PixelFormat,
    pub info: wgpu::Buffer,
    pub values: wgpu::Buffer,
}
impl NativeFormat {
    pub fn new(device: &wgpu::Device, format: PixelFormat) -> Result<Self, String> {
        let mut info = [0u32; 32];
        let mut values = Vec::<f32>::new();
        if format.mode == ColorMode::Cmyk {
            photocraft_color::convert::with_cmyk_transforms(|transforms| -> Result<(), String> {
                let (forward, reverse) = transforms.ok_or("CMYK profile transforms could not be linked")?;
                for (slot, transform) in [forward, reverse].into_iter().enumerate() {
                    let grid = transform.fast_grid().ok_or("CMYK profile has no GPU device-link grid")?;
                    let clut = grid.clut;
                    if !matches!(clut.inputs, 3 | 4) || clut.outputs > 4 || clut.grid.iter().any(|&n| n < 2) {
                        return Err("CMYK device-link dimensions are unsupported".into());
                    }
                    let h = slot * 16;
                    info[h] = clut.inputs as u32;
                    info[h + 1] = clut.outputs as u32;
                    let mut stride = clut.outputs;
                    for dimension in (0..clut.inputs).rev() {
                        info[h + 2 + dimension] = clut.grid[dimension] as u32;
                        info[h + 6 + dimension] = stride as u32;
                        stride *= clut.grid[dimension];
                    }
                    info[h + 10] = values.len() as u32;
                    values.extend_from_slice(&clut.data);
                    if let Some(post) = grid.post {
                        info[h + 11] = values.len() as u32;
                        info[h + 12] = post[0].len() as u32;
                        for table in post {
                            values.extend_from_slice(table);
                        }
                    }
                }
                Ok(())
            })?;
        }
        if values.is_empty() {
            values.push(0.0);
        }
        let info = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_native_format_info"),
            contents: bytemuck::cast_slice(&info),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let values = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_native_format_values"),
            contents: bytemuck::cast_slice(&values),
            usage: wgpu::BufferUsages::STORAGE,
        });
        Ok(Self { format, info, values })
    }
    pub fn mode(&self) -> u32 {
        match self.format.mode {
            ColorMode::Bitmap | ColorMode::Grayscale | ColorMode::Duotone => 1,
            ColorMode::Cmyk => 2,
            ColorMode::Lab => 3,
            _ => 0,
        }
    }
    /// WGSL helpers plus storage declarations at the caller's existing bind group.
    pub fn shader(group: u32, info_binding: u32, values_binding: u32) -> String {
        format!(
            "@group({group}) @binding({info_binding}) var<storage,read> cms_info:array<u32>;\n@group({group}) @binding({values_binding}) var<storage,read> cms_values:array<f32>;\n{SHADER}"
        )
    }
}
