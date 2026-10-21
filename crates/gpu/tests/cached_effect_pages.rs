use photocraft_color::{BlendMode, Color, ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Effect, FxCommon, FxPaint, Layer, LayerContent, StrokeFx, StrokePosition};
use photocraft_geom::{Rect, Size};
use photocraft_gpu::{Compositor, cached_transform::{CachedTransform, LayerSource}, render_to_vec};

#[test]
fn cached_effect_pages_keep_source_window_coordinates() {
    let adapter = pollster::block_on(wgpu::Instance::default().request_adapter(&Default::default())).unwrap();
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let mut compositor = Compositor::try_new_with_format(&device, wgpu::TextureFormat::Rgba32Float).unwrap();
    // Force multiple pages on a small fixture so the test covers page origins and aprons.
    compositor.set_texture_limit(128);
    let mut doc = Document::new("effect pages", Size::new(288, 224), ColorMode::Rgb, SampleType::F32);
    let mut layer = Layer::raster("source", PixelFormat::RGBA32F);
    let rect = Rect::new(113, 98, 148, 137);
    let color = [0.2, 0.7, 0.9, 0.8];
    layer.surface_mut().unwrap().fill_rect(rect, &color);
    layer.effects.items.push(Effect::Stroke(StrokeFx { common: FxCommon::new(BlendMode::Normal, 0.9), size: 4., position: StrokePosition::Outside, paint: FxPaint::Color(Color::rgb(0.9, 0.1, 0.3)) }));
    let id = layer.id;
    doc.layers.push(layer);
    let output = doc.bounds().inflate(8);
    let count = output.width() as usize * output.height() as usize;
    let mut cache = CachedTransform::new(&device, &queue, output, vec![LayerSource { id, rect, linked_mask: None, vector_mask: None, default: vec![0.; 4], format: PixelFormat::RGBA32F, alpha: vec![color[3]; rect.width() as usize * rect.height() as usize], pixels: vec![color; rect.width() as usize * rect.height() as usize], rest: vec![[0.; 4]; count], rest_alpha: vec![0.; count], stationary_mask: vec![1.; count] }]).unwrap();
    let mut encoder = device.create_command_encoder(&Default::default());
    let sources = cache.encode(&queue, &mut encoder, [[0.; 2]; 4], false, |x, y| (x + 11., y - 7.)).unwrap();
    queue.submit([encoder.finish()]);
    compositor.set_source_overrides(sources);
    let actual = render_to_vec(&mut compositor, &device, &queue, &doc, doc.bounds()).unwrap();
    let mut committed = doc.clone();
    let surface = doc.layer(id).unwrap().surface().unwrap().translated(11, -7, rect);
    committed.layer_mut(id).unwrap().content = LayerContent::Raster(surface);
    let expected = photocraft_compose::flatten(&committed);
    let worst = expected.px.iter().zip(&actual).flat_map(|(a,b)| (0..4).map(move |k| if k == 3 { (a[k]-b[k]).abs() } else { (a[k]*a[3]-b[k]*b[3]).abs() })).fold(0f32, f32::max);
    assert!(worst < 1. / 255. + 1e-5, "cached effect page error {worst}");
}
