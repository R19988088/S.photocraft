//! Published GPU brush tiles stay resident across masks, profile changes and undo.
use std::sync::Arc;

use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Layer, LayerMask};
use photocraft_geom::{Rect, Size, TILE_SIZE, TileCoord};
use photocraft_gpu::brush_format::NativeFormats;
use photocraft_gpu::tile::{GpuTileSnapshot, TileSnapshots};
use photocraft_gpu::{Compositor, render_to_vec};
use photocraft_raster::{Surface, Tile};

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    formats: NativeFormats,
    snapshots: TileSnapshots,
    compositor: Compositor,
}
impl Gpu {
    fn snapshot(&self, format: PixelFormat, pixels: &[f32]) -> Arc<Tile> {
        let side = TILE_SIZE as u32;
        let make = || {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("snapshot_test_input"),
                size: wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let color = make();
        let alpha = make();
        let colors = format.mode.color_channels();
        let mut channels = Vec::new();
        let mut alphas = Vec::new();
        for pixel in pixels.chunks_exact(format.channels()) {
            let mut c = [0.0f32; 4];
            c[..colors].copy_from_slice(&pixel[..colors]);
            channels.push(c);
            alphas.push([if format.alpha { pixel[colors] } else { 1.0 }, 0.0, 0.0, 0.0]);
        }
        for (texture, values) in [(&color, channels), (&alpha, alphas)] {
            self.queue.write_texture(
                texture.as_image_copy(),
                bytemuck::cast_slice(&values),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(side * 16), rows_per_image: Some(side) },
                texture.size(),
            );
        }
        let format = self.formats.get(&self.device, format).unwrap();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let tile = self.snapshots.capture_native(&self.device, &self.queue, &mut encoder, &color, &alpha, [0, 0], &format).unwrap();
        self.queue.submit([encoder.finish()]);
        assert!(!tile.is_cpu_materialized());
        let snapshot = tile.backend().unwrap().as_any().downcast_ref::<GpuTileSnapshot>().unwrap();
        let dp = vec![0u8; tile.byte_len() / ((TILE_SIZE * TILE_SIZE) as usize)];
        let started = std::time::Instant::now();
        let bounds = loop {
            if let Some(bounds) = snapshot.poll_content_bounds(&dp).unwrap() {
                break bounds;
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(10), "asynchronous GPU bounds never completed");
            std::thread::yield_now();
        };
        assert_eq!(bounds, Some((0, 0, TILE_SIZE as u16, TILE_SIZE as u16)));
        assert!(!tile.is_cpu_materialized());
        tile
    }

    fn compare(&mut self, gpu: &Document, cpu: &Document, tile: &Arc<Tile>, label: &str) -> Vec<[f32; 4]> {
        let expected = photocraft_compose::render(cpu, cpu.bounds());
        let actual = render_to_vec(&mut self.compositor, &self.device, &self.queue, gpu, gpu.bounds()).unwrap();
        assert!(!tile.is_cpu_materialized(), "{label}: compositor materialized the brush tile");
        assert_eq!(actual.len(), expected.px.len());
        let mut worst = 0.0f32;
        for (actual, expected) in actual.iter().zip(&expected.px) {
            for channel in 0..4 {
                let a = if channel == 3 { actual[3] } else { actual[channel] * actual[3] };
                let b = if channel == 3 { expected[3] } else { expected[channel] * expected[3] };
                assert!(a.is_finite() && b.is_finite(), "{label}: non-finite channel");
                worst = worst.max((a - b).abs());
            }
        }
        assert!(worst <= 1.0 / 255.0 + 0.00001, "{label}: maximum premultiplied error {worst}");
        actual
    }
}

#[test]
fn native_snapshots_composite_masks_profiles_eviction_and_undo() {
    let adapter = pollster::block_on(wgpu::Instance::default().request_adapter(&wgpu::RequestAdapterOptions::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
    device.on_uncaptured_error(Arc::new(|error| panic!("GPU validation: {error}")));
    let formats = NativeFormats::new(&device);
    let snapshots = TileSnapshots::new(&device);
    let mut compositor = Compositor::new(&device);
    compositor.set_composite_cache_budget(0);
    let mut g = Gpu { device, queue, formats, snapshots, compositor };
    let area = Rect::new(0, 0, TILE_SIZE, TILE_SIZE);
    let coord = TileCoord::new(0, 0);
    let count = (TILE_SIZE * TILE_SIZE) as usize;

    for sample in [SampleType::U8, SampleType::U16, SampleType::F32] {
        let format = PixelFormat::new(ColorMode::Grayscale, sample, false);
        let values: Vec<_> = (0..count).map(|i| 0.1 + (i % 251) as f32 / 300.0).collect();
        let tile = g.snapshot(format, &values);
        let mut mask = LayerMask::reveal_all();
        mask.surface = Surface::new(format);
        mask.surface.set_tile(coord, tile.clone());
        let mut layer = Layer::raster("Masked", PixelFormat::RGBA8);
        layer.surface_mut().unwrap().fill_rect(area, &[0.81, 0.23, 0.57, 0.92]);
        layer.mask = Some(mask);
        let mut gpu = Document::new("GPU mask", Size::new(40, 30), ColorMode::Rgb, SampleType::U8);
        gpu.layers.push(layer);
        let mut cpu = gpu.clone();
        let mask = &mut cpu.layers[0].mask.as_mut().unwrap().surface;
        *mask = Surface::new(format);
        mask.write_region(area, &values);
        g.compare(&gpu, &cpu, &tile, &format!("{sample:?} mask first render"));
        g.compositor.retain_docs(|_| false);
        g.compare(&gpu, &cpu, &tile, &format!("{sample:?} mask after eviction"));
        let undo_gpu = gpu.layers[0].mask.clone();
        let undo_cpu = cpu.layers[0].mask.clone();
        gpu.layers[0].mask.as_mut().unwrap().surface = Surface::new(format);
        cpu.layers[0].mask.as_mut().unwrap().surface = Surface::new(format);
        g.compare(&gpu, &cpu, &tile, "empty mask before undo");
        gpu.layers[0].mask = undo_gpu;
        cpu.layers[0].mask = undo_cpu;
        g.compare(&gpu, &cpu, &tile, &format!("{sample:?} mask undo"));
    }

    let format = PixelFormat::new(ColorMode::Cmyk, SampleType::U16, true);
    let values: Vec<f32> = (0..count).flat_map(|i| [0.11 + (i % 31) as f32 / 80.0, 0.34, 0.26, 0.43, 0.77]).collect();
    let tile = g.snapshot(format, &values);
    let mut gpu = Document::new("CMYK snapshots", Size::new(40, 30), ColorMode::Cmyk, SampleType::U16);
    let mut layer = Layer::raster("Inks", format);
    layer.surface_mut().unwrap().set_tile(coord, tile.clone());
    gpu.layers.push(layer);
    let mut cpu = gpu.clone();
    *cpu.layers[0].surface_mut().unwrap() = Surface::new(format);
    cpu.layers[0].surface_mut().unwrap().write_region(area, &values);
    let before = g.compare(&gpu, &cpu, &tile, "CMYK initial profile");
    let profile = photocraft_cms::synth::cmyk_profile(&photocraft_cms::synth::CmykParams {
        description: "Snapshot alternate ink response".into(),
        tvi: [0.32, 0.27, 0.22, 0.35],
        grid_a2b: 3,
        grid_b2a: 3,
        ..Default::default()
    })
    .to_bytes();
    gpu.icc_profile = Some(profile.clone());
    cpu.icc_profile = Some(profile);
    let after = g.compare(&gpu, &cpu, &tile, "CMYK reassigned profile");
    assert!(before.iter().zip(&after).any(|(a, b)| (a[0] - b[0]).abs() > 0.01), "the alternate profile must visibly change the stored inks");
    g.compositor.retain_docs(|_| false);
    g.compare(&gpu, &cpu, &tile, "CMYK profile after eviction");
    let undo_gpu = gpu.layers[0].surface().unwrap().clone();
    let undo_cpu = cpu.layers[0].surface().unwrap().clone();
    *gpu.layers[0].surface_mut().unwrap() = Surface::new(format);
    *cpu.layers[0].surface_mut().unwrap() = Surface::new(format);
    g.compare(&gpu, &cpu, &tile, "CMYK before undo");
    *gpu.layers[0].surface_mut().unwrap() = undo_gpu;
    *cpu.layers[0].surface_mut().unwrap() = undo_cpu;
    g.compare(&gpu, &cpu, &tile, "CMYK restored native inks");
    channel_restrictions(&g);
}

fn channel_restrictions(g: &Gpu) {
    let area = Rect::new(0, 0, TILE_SIZE, TILE_SIZE);
    let coord = TileCoord::new(0, 0);
    let count = (TILE_SIZE * TILE_SIZE) as usize;
    for format in [
        PixelFormat::RGBA8,
        PixelFormat::RGBA16,
        PixelFormat::RGBA32F,
        PixelFormat::new(ColorMode::Rgb, SampleType::F32, false),
        PixelFormat::GRAY8,
        PixelFormat::new(ColorMode::Grayscale, SampleType::F32, true),
        PixelFormat::CMYKA8,
        PixelFormat::new(ColorMode::Cmyk, SampleType::F32, true),
        PixelFormat::new(ColorMode::Lab, SampleType::F32, true),
    ] {
        let before_pixel = photocraft_raster::from_rgba(&format, [0.2, 0.6, 0.4, 0.45]);
        let after_pixel = photocraft_raster::from_rgba(&format, [0.8, 0.1, 0.7, 0.85]);
        let before_values: Vec<_> = (0..count).flat_map(|_| before_pixel.iter().copied()).collect();
        let after_values: Vec<_> = (0..count).flat_map(|_| after_pixel.iter().copied()).collect();
        let before = g.snapshot(format, &before_values);
        let after = g.snapshot(format, &after_values);
        let mut cpu_before = Surface::new(format);
        cpu_before.write_region(area, &before_values);
        let mut cpu_after = Surface::new(format);
        cpu_after.write_region(area, &after_values);
        let default = vec![0.0; format.channels()];
        for channel in 0..format.mode.color_channels() {
            for old in [Some(before.as_ref()), Some(cpu_before.tile(coord).unwrap().as_ref()), None] {
                let result = after.backend().unwrap().restrict_color_channel(format, old, Some(after.as_ref()), &default, &default, channel).unwrap().unwrap();
                assert!(
                    !before.is_cpu_materialized() && !after.is_cpu_materialized() && !result.is_cpu_materialized(),
                    "{format:?} channel restriction materialized source pixels"
                );
                let expected_old = if old.is_some() { cpu_before.tile(coord).unwrap().bytes() } else { &[] };
                let expected_new = cpu_after.tile(coord).unwrap().bytes();
                let actual = result.try_bytes().unwrap();
                for pixel in 0..count {
                    for c in 0..format.channels() {
                        let expected = if c == channel {
                            photocraft_color::read_sample(expected_new, format.sample, pixel * format.channels() + c)
                        } else if old.is_some() {
                            photocraft_color::read_sample(expected_old, format.sample, pixel * format.channels() + c)
                        } else {
                            0.0
                        };
                        let value = photocraft_color::read_sample(actual, format.sample, pixel * format.channels() + c);
                        assert!((value - expected).abs() <= 1e-6, "{format:?} channel {channel} component {c}: {value} vs {expected}");
                    }
                }
            }
        }
    }
}
