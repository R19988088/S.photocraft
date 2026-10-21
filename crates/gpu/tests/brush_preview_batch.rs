//! Settings previews submit a whole S-curve, unlike incremental canvas input.
use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_geom::Rect;
use photocraft_paint::gpu_backend::StrokeBackendFactory;
use photocraft_paint::{BrushContext, BrushSettings, StrokePoint};
use photocraft_raster::Surface;

#[test]
fn mixing_curve_preview_fits_metal_command_buffer_budget() {
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    device.on_uncaptured_error(std::sync::Arc::new(|error| panic!("GPU validation: {error}")));
    let factory = photocraft_gpu::brush_stroke::GpuStrokeFactory::new(&device, &queue);
    let mut brush = BrushSettings { size: 76.0 * 0.55, spacing: 0.02, pressure_size: false, hardness: 0.65, pigment_mix_strength: 0.65, ..Default::default() };
    brush.mix_settings.enabled = true;
    brush.mix_settings.strength = 0.1;
    brush.pigment = Some(photocraft_paint::pigment_material::PigmentSettings {
        enabled: true,
        thickness: 65.0,
        visual_thickness: 55.0,
        liquidity: 0.4,
        highlight_strength: 0.6,
        ..Default::default()
    });
    let points: Vec<_> = (0..=64)
        .map(|i| {
            let s = i as f64 / 64.0;
            StrokePoint::new(10.0 + s * 468.0, 38.0 - (s * std::f64::consts::TAU).sin() * 76.0 * 0.22, (s * std::f64::consts::PI).sin().max(0.05) as f32)
        })
        .collect();
    let ctx = BrushContext::new(&brush);
    let mut generator = photocraft_paint::dynamics::DabGenerator::new(&brush, 1.0);
    let (mut primary, mut secondary) = (Vec::new(), Vec::new());
    generator.push(&points, &mut primary, &mut secondary);
    generator.finish(&mut primary, &mut secondary);
    let pre = Surface::new(PixelFormat::new(ColorMode::Rgb, SampleType::U8, true));
    let mut batch = factory.begin(&ctx, &pre).unwrap();
    batch.rasterize(&ctx, &primary, &secondary);
    assert_eq!(batch.error(), None);
    let mut actual = pre.clone();
    batch.composite(&mut actual, None, false, true);
    assert_eq!(batch.error(), None);
    let region = Rect::new(0, 0, 488, 76);
    let pixels = actual.read_region(region);
    assert!(pixels.iter().any(|v| *v > 0.0));

    // Submission boundaries must not change transport history or pigment accumulation.
    let mut incremental = factory.begin(&ctx, &pre).unwrap();
    for dab in &primary {
        incremental.rasterize(&ctx, std::slice::from_ref(dab), &[]);
    }
    let mut expected = pre.clone();
    incremental.composite(&mut expected, None, false, true);
    assert_eq!(incremental.error(), None);
    assert_eq!(pixels, expected.read_region(region));
}
