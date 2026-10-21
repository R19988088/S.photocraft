//! Device regression for moved group silhouettes, linked masks and nested styled groups.
use photocraft_color::{BlendMode, Color, ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Effect, FxCommon, Layer, LayerMask, Shadow};
use photocraft_geom::{Rect, Size};
use photocraft_gpu::{
    Compositor,
    cached_group::{CachedGroups, GroupSource},
    cached_transform::{CachedTransform, LayerSource},
    render_to_vec,
};

#[test]
fn cached_nested_groups_follow_moved_mask_and_offcanvas_shadow_without_readback() {
    let adapter = pollster::block_on(wgpu::Instance::default().request_adapter(&Default::default())).expect("GPU adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).expect("GPU device");
    let mut compositor = Compositor::try_new_with_format(&device, wgpu::TextureFormat::Rgba32Float).expect("GPU compositor");
    let mut doc = Document::with_background("groups", Size::new(48, 40), ColorMode::Rgb, SampleType::F32, Color::rgb(0.7, 0.5, 0.3));
    let mut child = Layer::raster("child", PixelFormat::RGBA32F);
    let content = Rect::new(-7, 12, 9, 28);
    let color = [0.1, 0.8, 0.3, 0.9];
    child.surface_mut().unwrap().fill_rect(content, &color);
    let child_id = child.id;
    let mut inner = Layer::group("inner", vec![child]);
    inner.blend = BlendMode::Normal;
    inner.effects.items.push(Effect::DropShadow(Shadow {
        distance: 5.,
        size: 3.,
        spread: 0.2,
        angle: 180.,
        use_global_light: false,
        common: FxCommon::new(BlendMode::Multiply, 0.7),
        color: Color::rgb(0., 0., 0.),
        contour: photocraft_doc::Contour::Linear,
        anti_alias: false,
        noise: 0.,
        knocks_out: true,
    }));
    let mut mask = LayerMask::reveal_all();
    mask.density = 0.65;
    mask.feather = 2.2;
    mask.surface.fill_rect(Rect::new(0, 10, 4, 32), &[0.]);
    inner.mask = Some(mask);
    let inner_id = inner.id;
    let mut outer = Layer::group("outer", vec![inner]);
    outer.blend = BlendMode::Normal;
    outer.opacity = 0.8;
    let outer_id = outer.id;
    doc.layers.push(outer);
    let output = doc.bounds().inflate(20);
    let count = output.width() as usize * output.height() as usize;
    let mut leaf = CachedTransform::new(
        &device,
        &queue,
        output,
        vec![LayerSource {
            id: child_id,
            format: PixelFormat::RGBA32F,
            default: vec![0.; 4],
            rect: content,
            pixels: vec![color; content.width() as usize * content.height() as usize],
            alpha: vec![color[3]; content.width() as usize * content.height() as usize],
            rest: vec![[0.; 4]; count],
            rest_alpha: vec![0.; count],
            stationary_mask: vec![1.; count],
            linked_mask: None,
            vector_mask: None,
        }],
    )
    .unwrap();
    let effective_mask: Vec<f32> = (output.y0..output.y1)
        .flat_map(|y| (output.x0..output.x1).map(move |x| (x, y)))
        .map(|(x, y)| doc.layer(inner_id).unwrap().mask.as_ref().unwrap().surface.sample_channel(x, y, 0))
        .collect();
    let mut groups = CachedGroups::new(
        &device,
        &queue,
        &doc,
        output,
        vec![
            GroupSource {
                id: inner_id,
                linked: effective_mask,
                stationary: vec![1.; count],
                default: 1.,
                moving: true,
                feather: 2.2,
                density: 0.65,
                vector: None,
                vector_default: 1.,
                vector_feather: 0.,
                vector_density: 1.,
            },
            GroupSource {
                id: outer_id,
                linked: vec![1.; count],
                stationary: vec![1.; count],
                default: 1.,
                moving: true,
                feather: 0.,
                density: 1.,
                vector: None,
                vector_default: 1.,
                vector_feather: 0.,
                vector_density: 1.,
            },
        ],
    )
    .unwrap();
    for (dx, dy) in [(6, 0), (-3, 3), (6, 0)] {
        let preview = (dy != 0).then_some(doc.bounds());
        leaf.set_preview_region(preview.map(|region| groups.preview_source_region(region)));
        groups.set_preview_region(preview);
        let mut encoder = device.create_command_encoder(&Default::default());
        let mut sources =
            leaf.encode_sampled(&queue, &mut encoder, [[0.; 2]; 4], false, "nearest", |x, y| (x + f64::from(dx), y + f64::from(dy)), |_, _| 1.).unwrap();
        groups
            .encode_sampled(
                &device,
                &queue,
                &mut encoder,
                &mut sources,
                [[0.; 2]; 4],
                false,
                "nearest",
                &|x, y| (x + f64::from(dx), y + f64::from(dy)),
                &|_, _| 1.,
            )
            .unwrap();
        queue.submit([encoder.finish()]);
        compositor.set_source_overrides(sources);
        let mut expected_doc = doc.clone();
        let original = doc.layer(child_id).unwrap().surface().unwrap();
        *expected_doc.layer_mut(child_id).unwrap().surface_mut().unwrap() = original.translated(dx, dy, content);
        let mask = expected_doc.layer_mut(inner_id).unwrap().mask.as_mut().unwrap();
        mask.surface = mask.surface.translated(dx, dy, mask.surface.content_bounds());
        let actual = render_to_vec(&mut compositor, &device, &queue, &doc, doc.bounds()).unwrap();
        let expected = photocraft_compose::flatten(&expected_doc);
        let worst = actual.iter().zip(&expected.px).flat_map(|(a, b)| (0..4).map(move |k| (a[k] - b[k]).abs())).fold(0f32, f32::max);
        let (index, _) = actual
            .iter()
            .zip(&expected.px)
            .enumerate()
            .max_by(|(_, (a, b)), (_, (c, d))| {
                let delta = |x: &[f32; 4], y: &[f32; 4]| (0..4).map(|k| (x[k] - y[k]).abs()).fold(0f32, f32::max);
                delta(a, b).total_cmp(&delta(c, d))
            })
            .unwrap();
        assert!(
            worst < 1. / 255. + 1e-4,
            "group offset {dx},{dy}: max difference {worst} at {},{} actual {:?} expected {:?}",
            index % 48,
            index / 48,
            actual[index],
            expected.px[index]
        );
        let stats = compositor.render(&device, &queue, &doc, doc.bounds(), |_, _| {}).unwrap();
        assert_eq!(stats.fx_shapes, 0, "group effect shape remains resident");
        assert_eq!(doc.layer(child_id).unwrap().surface().unwrap().content_bounds(), content);
    }
}
