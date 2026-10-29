//! Exercise the actual cached transform shader and native compositor on the device. The
//! document remains unchanged while two geometry revisions are compared with committed pixels.
use photocraft_color::{BlendMode, Color, ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Effect, FxCommon, FxPaint, Layer, LayerContent, Shadow, StrokeFx, StrokePosition};
use photocraft_geom::{Rect, Size};
use photocraft_gpu::{
    Compositor,
    cached_transform::{CachedTransform, LayerSource},
    render_to_vec,
};

#[test]
fn cached_sources_keep_blends_opacity_and_styles_without_reupload() {
    let adapter = pollster::block_on(wgpu::Instance::default().request_adapter(&Default::default())).expect("GPU adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).expect("GPU device");
    let mut compositor = Compositor::try_new_with_format(&device, wgpu::TextureFormat::Rgba32Float).expect("GPU compositor");
    let mut doc = Document::with_background("cached", Size::new(72, 64), ColorMode::Rgb, SampleType::F32, Color::rgb(0.7, 0.5, 0.3));
    let mut sources = Vec::new();
    let output = doc.bounds().inflate(16);
    let count = output.width() as usize * output.height() as usize;
    for (index, (rect, color, mode, opacity)) in [
        (Rect::new(12, 18, 30, 36), [0.1, 0.8, 0.2, 0.85], BlendMode::Multiply, 0.65),
        (Rect::new(22, 24, 40, 42), [0.2, 0.3, 0.95, 0.75], BlendMode::Screen, 0.7),
    ]
    .into_iter()
    .enumerate()
    {
        let mut layer = Layer::raster(format!("source {index}"), PixelFormat::RGBA32F);
        layer.surface_mut().unwrap().fill_rect(rect, &color);
        layer.blend = mode;
        layer.opacity = opacity;
        layer.fill_opacity = 0.8;
        layer.effects.items.push(Effect::Stroke(StrokeFx {
            common: FxCommon::new(BlendMode::Normal, 0.9),
            size: 2.,
            position: StrokePosition::Outside,
            paint: FxPaint::Color(Color::rgb(0.9, 0.1, 0.4)),
        }));
        if index == 0 {
            layer.effects.items.push(Effect::DropShadow(Shadow {
                distance: 4.,
                size: 2.,
                spread: 0.2,
                angle: 135.,
                use_global_light: false,
                common: FxCommon::new(BlendMode::Multiply, 0.7),
                color: Color::rgb(0., 0., 0.),
                contour: photocraft_doc::Contour::Linear,
                anti_alias: false,
                noise: 0.,
                knocks_out: true,
            }));
        }
        sources.push(LayerSource {
            id: layer.id,
            rect,
            pixels: vec![color; rect.width() as usize * rect.height() as usize],
            rest: vec![[0.; 4]; count],
            rest_alpha: vec![0.; count],
            stationary_mask: vec![1.; count],
            linked_mask: None,
            vector_mask: None,
            format: PixelFormat::RGBA32F,
            default: vec![0.; 4],
            alpha: vec![color[3]; rect.width() as usize * rect.height() as usize],
        });
        doc.layers.push(layer);
    }
    let originals: Vec<_> =
        doc.layers.iter().skip(1).map(|l| (l.id, l.surface().unwrap().clone(), photocraft_compose::layer_bounds(l, doc.bounds()))).collect();
    let mut cached = CachedTransform::new(&device, &queue, output, sources).expect("frozen GPU sources");
    for (dx, dy) in [(5, 3), (-3, 7)] {
        let mut encoder = device.create_command_encoder(&Default::default());
        let overrides = cached.encode(&queue, &mut encoder, [[0.; 2]; 4], false, |x, y| (x + f64::from(dx), y + f64::from(dy))).expect("GPU transform");
        queue.submit([encoder.finish()]);
        compositor.set_source_overrides(overrides);
        let mut committed = doc.clone();
        for (id, surface, bounds) in &originals {
            committed.layer_mut(*id).unwrap().content = LayerContent::Raster(surface.translated(dx, dy, *bounds));
        }
        let actual = render_to_vec(&mut compositor, &device, &queue, &doc, doc.bounds()).expect("native source overrides");
        let expected = photocraft_compose::flatten(&committed);
        let worst = expected
            .px
            .iter()
            .zip(&actual)
            .flat_map(|(a, b)| (0..4).map(move |k| if k == 3 { (a[k] - b[k]).abs() } else { (a[k] * a[3] - b[k] * b[3]).abs() }))
            .fold(0f32, f32::max);
        assert!(worst < 1.0 / 255.0 + 1e-5, "offset {dx},{dy}: maximum premultiplied difference {worst}");
        // A subsequent encode sees the exact same COW document and frozen source textures.
        let stats = compositor.render(&device, &queue, &doc, doc.bounds(), |_, _| {}).expect("resident second frame");
        assert_eq!(stats.tiles_uploaded, 0, "no transformed pixels are uploaded");
        assert_eq!(stats.fx_shapes, 0, "no CPU alpha image is built");
        assert!(originals.iter().all(|(id, _, bounds)| photocraft_compose::layer_bounds(doc.layer(*id).unwrap(), doc.bounds()) == *bounds));
    }
}
