//! Validate the actual device pipelines, including their WGSL and binding layouts.
#[test]
fn brush_device_pipelines() {
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
    device.on_uncaptured_error(std::sync::Arc::new(|error| panic!("GPU validation: {error}")));
    let factory = std::sync::Arc::new(photocraft_gpu::brush_stroke::GpuStrokeFactory::new(&device, &queue));
    parity(&factory);
    native_formats(&factory);
    live(factory);
}

fn parity(factory: &photocraft_gpu::brush_stroke::GpuStrokeFactory) {
    use photocraft_color::PixelFormat;
    use photocraft_geom::Rect;
    use photocraft_paint::gpu_backend::StrokeBackendFactory;
    use photocraft_paint::material::{WetColorMode, WetEdgeSettings, WetOutline, WetStyle};
    use photocraft_paint::transport::MixAlgorithm;
    use photocraft_paint::{BrushContext, BrushSettings, StrokePoint};
    use photocraft_raster::Surface;

    let base_brush = BrushSettings { size: 10.0, hardness: 0.6, spacing: 0.4, pressure_size: false, color: [0.8, 0.2, 0.3, 0.8], ..Default::default() };
    let mut cases = vec![("normal".to_owned(), base_brush.clone())];
    for algorithm in [MixAlgorithm::V1, MixAlgorithm::V2, MixAlgorithm::V3] {
        let mut brush = base_brush.clone();
        brush.mix_settings.enabled = true;
        brush.mix_settings.strength = 0.65;
        brush.mix_settings.blur_strength = 0.25;
        brush.pigment_mix_strength = 0.45;
        brush.mix_settings.algorithm = algorithm;
        cases.push((format!("transport {algorithm:?}"), brush));
    }
    for style in [WetStyle::Photoshop, WetStyle::Watercolor, WetStyle::Watercolor2] {
        for outline in [WetOutline::Stroke, WetOutline::Single] {
            let mut brush = base_brush.clone();
            brush.wet_edges = true;
            brush.wet_edge = Some(WetEdgeSettings { style, outline, color_mode: WetColorMode::Multiply, brightness: 0.25, ..Default::default() });
            cases.push((format!("wet {style:?} {outline:?}"), brush));
        }
    }
    let mut legacy_dual = base_brush.clone();
    legacy_dual.dual_brush.enabled = true;
    legacy_dual.dual_brush.size = 7.0;
    legacy_dual.dual_brush.wet_edges = true;
    legacy_dual.dual_brush.wet_edge = Some(WetEdgeSettings { color_mode: WetColorMode::Multiply, outline: WetOutline::Single, ..Default::default() });
    cases.push(("legacy dual wet".into(), legacy_dual));
    let mut full_dual = base_brush.clone();
    let mut second = base_brush.clone();
    second.color = [0.1, 0.7, 0.3, 0.6];
    second.mix_settings.enabled = true;
    second.mix_settings.strength = 0.6;
    second.mix_settings.algorithm = MixAlgorithm::V2;
    full_dual.dual_brush.enabled = true;
    full_dual.dual_brush.full_parameters = Some(Box::new(second));
    cases.push(("independent dual transport".into(), full_dual));
    let mut texture = base_brush.clone();
    texture.texture.enabled = true;
    texture.texture.each_tip = false;
    cases.push(("stroke texture".into(), texture));
    let mut erase = base_brush.clone();
    erase.erase = true;
    cases.push(("erase".into(), erase));
    let asset = photocraft_paint::GrayTile::from_fn(9, 7, |x, y| ((x * 3 + y * 5) % 13) as f32 / 12.0);
    for c3 in [false, true] {
        let mut sampled = base_brush.clone();
        sampled.tip = photocraft_paint::TipShape::Sampled(asset.clone());
        sampled.c3_tip_math = c3;
        sampled.angle = 29.0;
        sampled.roundness = 0.65;
        sampled.texture.enabled = true;
        sampled.texture.pattern = photocraft_paint::Pattern::Tile(asset.clone());
        sampled.texture.c3_math = c3;
        sampled.texture.depth = 0.6;
        sampled.texture.scale = 0.7;
        cases.push((format!("sampled textured c3={c3}"), sampled));
    }
    let mut grain = base_brush.clone();
    grain.noise = true;
    grain.grain = Some(photocraft_paint::material::GrainSettings { density: 0.65, size: 2.5, size_jitter: 0.3, density_jitter: 0.4, ..Default::default() });
    cases.push(("spatial grain".into(), grain));
    let mut colors = base_brush.clone();
    colors.color_dynamics.enabled = true;
    colors.color_dynamics.hue_jitter = 0.3;
    colors.saturation_record_strength = 0.6;
    colors.saturation_preserve_output = 0.4;
    cases.push(("color dynamics and saturation".into(), colors));
    let mut sampled_color = base_brush.clone();
    sampled_color.pattern_enabled = true;
    sampled_color.sampled_pattern = Some(photocraft_paint::ColorPattern {
        width: 2,
        height: 2,
        pixels: vec![[0.8, 0.1, 0.3, 0.5], [0.1, 0.6, 0.2, 0.8], [0.2, 0.2, 0.9, 1.0], [0.6, 0.3, 0.2, 0.7]].into(),
    });
    sampled_color.mix_settings.enabled = true;
    sampled_color.mix_settings.strength = 0.65;
    cases.push(("sampled color transport".into(), sampled_color));
    let mut material = base_brush.clone();
    material.flow = 0.55;
    material.canvas_material =
        Some(photocraft_paint::canvas_material::CanvasMaterial { pattern: photocraft_paint::Pattern::Tile(asset), ..Default::default() });
    cases.push(("sampled canvas material".into(), material));
    let mut pigment = base_brush;
    pigment.pigment = Some(photocraft_paint::pigment_material::PigmentSettings {
        enabled: true,
        thickness: 65.0,
        visual_thickness: 55.0,
        liquidity: 0.4,
        highlight_strength: 0.6,
        highlight_brightness: 0.3,
        highlight_ratio: 0.5,
        ..Default::default()
    });
    pigment.pigment_mix_strength = 0.65;
    cases.push(("pigment".into(), pigment));

    let rect = Rect::new(242, 242, 278, 278);
    let mut pre = Surface::new(PixelFormat::RGBA32F);
    let mut pixels = Vec::new();
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            pixels.extend_from_slice(&[0.15 + (x - rect.x0) as f32 / 70.0, 0.5, 0.7, if x < 260 { 0.8 } else { 0.2 + (y - rect.y0) as f32 / 70.0 }]);
        }
    }
    pre.write_region(rect, &pixels);
    let points = [StrokePoint::new(251.25, 250.5, 1.0), StrokePoint::new(261.75, 259.25, 0.7)];
    let mut failures = Vec::new();
    for (label, brush) in cases {
        let mut expected = pre.clone();
        photocraft_paint::render_stroke(&mut expected, &brush, &points, None, false, 1.0);
        let ctx = BrushContext::new(&brush);
        let mut generator = photocraft_paint::dynamics::DabGenerator::new(&brush, 1.0);
        let (mut primary, mut secondary) = (Vec::new(), Vec::new());
        generator.push(&points, &mut primary, &mut secondary);
        generator.finish(&mut primary, &mut secondary);
        let mut gpu = factory.begin(&ctx, &pre).unwrap();
        gpu.rasterize(&ctx, &primary, &secondary);
        assert_eq!(gpu.error(), None, "{label}");
        let mut fork = gpu.fork();
        fork.rasterize(&ctx, &primary, &secondary);
        assert_eq!(fork.error(), None, "{label}: GPU fork failed");
        let mut actual = pre.clone();
        gpu.composite(&mut actual, None, false, true);
        assert_eq!(gpu.error(), None, "{label}");
        let changed: Vec<_> = actual.tiles().filter(|(_, tile)| tile.backend().is_some()).map(|(_, tile)| tile).collect();
        assert!(!changed.is_empty(), "{label}: no GPU snapshots published");
        assert!(changed.iter().all(|tile| !tile.is_cpu_materialized()), "{label}: painting read pixels back");
        assert_eq!(actual.content_bounds(), expected.content_bounds(), "{label}: GPU bounds differ");
        assert!(changed.iter().all(|tile| !tile.is_cpu_materialized()), "{label}: bounds read pixels back");
        let (expected, actual) = (expected.read_region(rect), actual.read_region(rect));
        let mut worst = 0.0f32;
        for (expected, actual) in expected.chunks_exact(4).zip(actual.chunks_exact(4)) {
            for c in 0..4 {
                let error = if c == 3 { (expected[c] - actual[c]).abs() } else { (expected[c] * expected[3] - actual[c] * actual[3]).abs() };
                worst = worst.max(error);
            }
        }
        if worst > 1.0 / 255.0 {
            failures.push(format!("{label}: premultiplied error {worst}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn live(factory: std::sync::Arc<photocraft_gpu::brush_stroke::GpuStrokeFactory>) {
    use photocraft_color::PixelFormat;
    use photocraft_geom::Rect;
    use photocraft_paint::{BrushSettings, StrokePoint, StrokeRenderer};
    use photocraft_raster::Surface;
    fn step(renderer: &mut StrokeRenderer, pre: &Surface, output: &mut Surface, tail: &mut Rect, points: &[StrokePoint]) {
        renderer.push(points);
        if !tail.is_empty() {
            let restored = renderer.restore_preview(pre, output, *tail);
            renderer.mark_dirty(restored);
        }
        renderer.composite(pre, output, None, false, false);
        *tail = Rect::EMPTY;
        if let Some(mut preview) = renderer.tail_preview() {
            *tail = preview.composite(pre, output, None, false, false);
            assert_eq!(preview.error(), None);
        }
        assert_eq!(renderer.error(), None);
    }
    let mut brush = BrushSettings { size: 12.0, spacing: 0.3, pressure_size: false, color: [0.8, 0.2, 0.3, 0.9], ..Default::default() };
    brush.smoothing.amount = 0.4;
    brush.mix_settings.enabled = true;
    brush.mix_settings.strength = 0.7;
    brush.mix_settings.algorithm = photocraft_paint::transport::MixAlgorithm::V2;
    let mut pre = Surface::new(PixelFormat::RGBA8);
    pre.fill_rect(Rect::new(220, 220, 290, 290), &[0.2, 0.6, 0.4, 0.7]);
    let points = [StrokePoint::new(248.25, 249.5, 1.0), StrokePoint::new(257.5, 256.25, 0.8), StrokePoint::new(266.75, 264.5, 0.5)];
    let mut oracle = StrokeRenderer::on_surface(&brush, &pre, 1.0);
    let mut expected = pre.clone();
    let mut tail = Rect::EMPTY;
    let mut frames = Vec::new();
    for point in &points {
        step(&mut oracle, &pre, &mut expected, &mut tail, std::slice::from_ref(point));
        frames.push(expected.clone());
    }
    let reflected: Vec<_> = points.iter().map(|p| StrokePoint::new(514.0 - p.x, p.y, p.pressure)).collect();
    let mut union_oracles = [StrokeRenderer::on_surface(&brush, &pre, 1.0), StrokeRenderer::on_surface(&brush, &pre, 1.0)];
    let mut union_expected = pre.clone();
    let mut union_live = photocraft_paint::UnionLive::default();
    let mut union_frames = Vec::new();
    for (i, (point, mirror)) in points.iter().zip(&reflected).enumerate() {
        union_oracles[0].push(std::slice::from_ref(point));
        union_oracles[1].push(std::slice::from_ref(mirror));
        let mut passes: Vec<_> = union_oracles.iter_mut().collect();
        StrokeRenderer::composite_union_live(&mut passes, &mut union_live, i == 0, &pre, &mut union_expected, None, false);
        union_frames.push(union_expected.clone());
    }
    photocraft_paint::gpu_backend::install(factory);
    let mut renderer = StrokeRenderer::on_surface(&brush, &pre, 1.0);
    let mut actual = pre.clone();
    tail = Rect::EMPTY;
    for (point, expected) in points.iter().zip(frames) {
        step(&mut renderer, &pre, &mut actual, &mut tail, std::slice::from_ref(point));
        assert!(actual.tiles().filter(|(_, t)| t.backend().is_some()).all(|(_, t)| !t.is_cpu_materialized()));
        let region = Rect::new(235, 235, 280, 280);
        let a = actual.read_region(region);
        let e = expected.read_region(region);
        let worst = a.iter().zip(e).fold(0.0f32, |m, (a, e)| m.max((a - e).abs()));
        assert!(worst <= 2.0 / 255.0, "live tail parity error {worst}");
    }
    let mut union_renderers = [StrokeRenderer::on_surface(&brush, &pre, 1.0), StrokeRenderer::on_surface(&brush, &pre, 1.0)];
    let mut actual = pre.clone();
    let mut union_live = photocraft_paint::UnionLive::default();
    for (i, ((point, mirror), expected)) in points.iter().zip(&reflected).zip(union_frames).enumerate() {
        union_renderers[0].push(std::slice::from_ref(point));
        union_renderers[1].push(std::slice::from_ref(mirror));
        let mut passes: Vec<_> = union_renderers.iter_mut().collect();
        StrokeRenderer::composite_union_live(&mut passes, &mut union_live, i == 0, &pre, &mut actual, None, false);
        for pass in passes {
            assert_eq!(pass.error(), None);
        }
        assert!(actual.tiles().filter(|(_, t)| t.backend().is_some()).all(|(_, t)| !t.is_cpu_materialized()));
        let region = Rect::new(235, 235, 280, 280);
        let a = actual.read_region(region);
        let e = expected.read_region(region);
        let worst = a.iter().zip(e).fold(0.0f32, |m, (a, e)| m.max((a - e).abs()));
        assert!(worst <= 2.0 / 255.0, "live symmetry tail parity error {worst} at {i}");
    }
}

fn native_formats(factory: &photocraft_gpu::brush_stroke::GpuStrokeFactory) {
    use photocraft_color::{ColorMode, PixelFormat, SampleType};
    use photocraft_geom::Rect;
    use photocraft_paint::gpu_backend::StrokeBackendFactory;
    use photocraft_paint::transport::MixAlgorithm;
    use photocraft_paint::{BrushContext, BrushSettings, StrokePoint};
    use photocraft_raster::Surface;
    let region = Rect::new(244, 244, 273, 273);
    let points = [StrokePoint::new(251.25, 251.5, 1.0), StrokePoint::new(261.5, 261.75, 0.7)];
    let mut cases = vec![("normal", BrushSettings { size: 9.0, spacing: 0.4, pressure_size: false, color: [0.8, 0.2, 0.3, 0.8], ..Default::default() })];
    for (name, algorithm) in [("V1", MixAlgorithm::V1), ("V2", MixAlgorithm::V2), ("V3", MixAlgorithm::V3)] {
        let mut brush = cases[0].1.clone();
        brush.mix_settings.enabled = true;
        brush.mix_settings.algorithm = algorithm;
        brush.mix_settings.strength = 0.6;
        brush.mix_settings.blur_strength = 0.2;
        brush.pigment_mix_strength = 0.4;
        cases.push((name, brush));
    }
    let mut wet = cases[0].1.clone();
    wet.wet_edges = true;
    wet.wet_edge = Some(photocraft_paint::material::WetEdgeSettings { style: photocraft_paint::material::WetStyle::Watercolor2, ..Default::default() });
    cases.push(("wet", wet));
    let mut pigment = cases[0].1.clone();
    pigment.pigment = Some(photocraft_paint::pigment_material::PigmentSettings {
        enabled: true,
        thickness: 65.0,
        visual_thickness: 55.0,
        liquidity: 0.4,
        highlight_strength: 0.6,
        highlight_brightness: 0.3,
        highlight_ratio: 0.5,
        ..Default::default()
    });
    pigment.pigment_mix_strength = 0.5;
    cases.push(("pigment", pigment));
    let mut canvas = cases[0].1.clone();
    canvas.flow = 0.55;
    canvas.canvas_material = Some(photocraft_paint::canvas_material::CanvasMaterial {
        pattern: photocraft_paint::Pattern::Tile(photocraft_paint::GrayTile::from_fn(4, 4, |x, y| ((x + y) % 4) as f32 / 3.0)),
        ..Default::default()
    });
    cases.push(("canvas material", canvas));
    let formats = [
        PixelFormat::RGBA8,
        PixelFormat::RGBA16,
        PixelFormat::new(ColorMode::Rgb, SampleType::F32, false),
        PixelFormat::GRAY8,
        PixelFormat::new(ColorMode::Grayscale, SampleType::F32, true),
        PixelFormat::CMYKA8,
        PixelFormat::new(ColorMode::Cmyk, SampleType::F32, true),
        PixelFormat::new(ColorMode::Cmyk, SampleType::U16, false),
        PixelFormat::new(ColorMode::Lab, SampleType::F32, true),
    ];
    let mut failures = Vec::new();
    for format in formats {
        let mut pre = Surface::new(format);
        let mut pixel = photocraft_raster::from_rgba(&format, [0.2, 0.5, 0.7, 0.65]);
        if format.mode == ColorMode::Cmyk {
            pixel[3] = 0.3;
        }
        pre.fill_rect(region, &pixel);
        for (label, brush) in &cases {
            let mut expected = pre.clone();
            photocraft_paint::render_stroke(&mut expected, brush, &points, None, false, 1.0);
            let ctx = BrushContext::new(brush);
            let mut generator = photocraft_paint::dynamics::DabGenerator::new(brush, 1.0);
            let (mut primary, mut secondary) = (Vec::new(), Vec::new());
            generator.push(&points, &mut primary, &mut secondary);
            generator.finish(&mut primary, &mut secondary);
            let mut backend = factory.begin(&ctx, &pre).unwrap_or_else(|| panic!("GPU rejected {format:?}"));
            backend.rasterize(&ctx, &primary, &secondary);
            assert_eq!(backend.error(), None, "{format:?} {label}");
            let mut actual = pre.clone();
            backend.composite(&mut actual, None, false, true);
            assert_eq!(backend.error(), None, "{format:?} {label}");
            assert!(actual.tiles().any(|(_, tile)| tile.backend().is_some()), "{format:?} {label}: no GPU tiles");
            assert!(actual.tiles().all(|(_, tile)| !tile.is_cpu_materialized() || tile.backend().is_none()), "{format:?} {label}: pixel readback");
            if actual.content_bounds() != expected.content_bounds() {
                failures.push(format!("{format:?} {label}: bounds {:?} vs {:?}", actual.content_bounds(), expected.content_bounds()));
            }
            let expected = expected.read_region(region);
            let actual = actual.read_region(region);
            let worst = expected.iter().zip(&actual).fold(0.0f32, |worst, (a, b)| worst.max((a - b).abs()));
            if worst > 2.0 / 255.0 {
                failures.push(format!("{format:?} {label}: native channel error {worst}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
