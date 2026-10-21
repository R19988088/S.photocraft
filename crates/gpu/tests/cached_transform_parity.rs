//! Device preview against the CPU resamplers used by the transform commit.
use photocraft_algo::transform::{Homography, Interp, warp_surface};
use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Layer, LayerMask};
use photocraft_geom::{Rect, Size};
use photocraft_gpu::{
    Compositor,
    cached_transform::{CachedTransform, LayerSource, MaskSource},
    render_to_vec,
};
use photocraft_raster::Surface;

fn rgba(surface: &Surface, rect: Rect) -> Vec<[f32; 4]> {
    let f = surface.format();
    surface.read_region(rect).chunks_exact(f.channels()).map(|p| photocraft_raster::to_rgba(&f, p)).collect()
}

fn source(id: photocraft_doc::LayerId, surface: &Surface, rect: Rect, output: Rect, rest: Vec<[f32; 4]>, mask: Option<MaskSource>) -> LayerSource {
    let format = surface.format();
    let colors = format.mode.color_channels();
    let raw = surface.read_region(rect);
    LayerSource {
        id,
        rect,
        format,
        default: surface.default_pixel(),
        pixels: raw
            .chunks_exact(format.channels())
            .map(|p| {
                let mut c = [0.; 4];
                c[..colors].copy_from_slice(&p[..colors]);
                c
            })
            .collect(),
        alpha: raw.chunks_exact(format.channels()).map(|p| if format.alpha { p[colors] } else { 1. }).collect(),
        rest_alpha: rest.iter().map(|p| p[3]).collect(),
        rest,
        stationary_mask: vec![1.; output.width() as usize * output.height() as usize],
        linked_mask: mask,
        vector_mask: None,
    }
}

fn patterned(format: PixelFormat, rect: Rect) -> Surface {
    let mut out = Surface::new(format);
    let mut pixels = Vec::new();
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            let u = (x - rect.x0) as f32 / rect.width() as f32;
            let v = (y - rect.y0) as f32 / rect.height() as f32;
            let checker = ((x + y) % 3) as f32 / 2.;
            match format.mode {
                ColorMode::Cmyk => pixels.extend([0.1 + u * 0.6, 0.15 + v * 0.4, 0.2 + checker * 0.3, 0.13 + v * 0.2]),
                ColorMode::Lab => pixels.extend([0.25 + u * 0.55, 0.38 + v * 0.2, 0.42 + checker * 0.14]),
                _ => pixels.extend([0.1 + u * 0.7, 0.15 + v * 0.6, 0.2 + checker * 0.55]),
            }
            pixels.push(0.35 + (u + v) * 0.3);
        }
    }
    out.write_region(rect, &pixels);
    out
}

// The commit warps a mask's painted region with alpha, then restores its constant outside.
fn warp_mask(mask: &Surface, h: &Homography, interpolation: Interp) -> Surface {
    let rect = mask.content_bounds();
    let default = mask.default_pixel()[0];
    let format = mask.format();
    let mut lifted = Surface::new(PixelFormat::new(format.mode, format.sample, true));
    lifted.write_region(rect, &mask.read_region(rect).into_iter().flat_map(|v| [v, 1.]).collect::<Vec<_>>());
    let warped = warp_surface(&lifted, rect, h, interpolation).unwrap();
    let bounds = warped.content_bounds();
    let mut result = Surface::with_default(format, &[default]);
    result.write_region(bounds, &warped.read_region(bounds).chunks_exact(2).map(|p| p[0] * p[1] + default * (1. - p[1])).collect::<Vec<_>>());
    result
}

struct Device {
    device: wgpu::Device,
    queue: wgpu::Queue,
    compositor: Compositor,
    failures: Vec<String>,
}
impl Device {
    fn new() -> Self {
        let adapter = pollster::block_on(wgpu::Instance::default().request_adapter(&Default::default())).expect("GPU adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).expect("GPU device");
        let compositor = Compositor::try_new_with_format(&device, wgpu::TextureFormat::Rgba32Float).expect("compositor");
        Self { device, queue, compositor, failures: Vec::new() }
    }

    #[allow(clippy::too_many_arguments)]
    fn compare(&mut self, name: &str, original: &Document, committed: &Document, cached: &mut CachedTransform, h: Homography, interpolation: &str, mesh: bool) {
        cached.set_preview_region(Some(original.bounds()));
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let preview_sources = cached
            .encode_sampled_with_homography(&self.queue, &mut encoder, [[0.; 2]; 4], mesh, interpolation, Some(h), |x, y| h.apply(x, y), |x, y| h.w(x, y))
            .expect(name);
        self.queue.submit([encoder.finish()]);
        self.compositor.set_source_overrides(preview_sources);
        let preview = render_to_vec(&mut self.compositor, &self.device, &self.queue, original, original.bounds()).expect(name);
        assert!(cached.readback_native(&self.queue).is_err(), "preview does not materialize native commit planes");
        cached.set_preview_region(None);
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let sources = cached
            .encode_sampled_with_homography(&self.queue, &mut encoder, [[0.; 2]; 4], mesh, interpolation, Some(h), |x, y| h.apply(x, y), |x, y| h.w(x, y))
            .expect(name);
        self.queue.submit([encoder.finish()]);
        self.compositor.set_source_overrides(sources);
        let actual = render_to_vec(&mut self.compositor, &self.device, &self.queue, original, original.bounds()).expect(name);
        let preview_worst = preview
            .iter()
            .zip(&actual)
            .flat_map(|(a, b)| (0..4).map(move |c| if c == 3 { (a[c] - b[c]).abs() } else { (a[c] * a[3] - b[c] * b[3]).abs() }))
            .fold(0f32, f32::max);
        if preview_worst > 1. / 255. + 2e-4 {
            self.failures.push(format!("{name}: fused ROI preview differs from canonical sampling by {preview_worst}"));
        }
        let mut resident_commit = original.clone();
        for (id, pixels, mask) in cached.readback_native(&self.queue).expect("native GPU commit readback") {
            if let Some(layer) = resident_commit.layer_mut(id) {
                *layer.surface_mut().unwrap() = pixels;
                if let (Some(raw), Some(original_mask)) = (mask, layer.mask.as_mut()) {
                    original_mask.surface = raw.try_convert(original_mask.surface.format()).expect("commit mask storage");
                }
            }
        }
        let expected = photocraft_compose::flatten(&resident_commit);
        let cpu = photocraft_compose::flatten(committed);
        let mut resident_worst = 0f32;
        let mut cpu_worst = 0f32;
        let mut location = (0, 0);
        let mut ties = 0;
        let inverse = h.inverse().unwrap();
        for (i, ((actual, resident), cpu)) in actual.iter().zip(&expected.px).zip(&cpu.px).enumerate() {
            let x = (i % original.bounds().width() as usize) as f64 + 0.5;
            let y = (i / original.bounds().width() as usize) as f64 + 0.5;
            let (u, v) = inverse.apply(x, y);
            // CPU commits previously evaluated nearest's integer boundaries in f64, whereas
            // GPU interpolation uses f32. The authoritative commit now stores these same GPU
            // samples. Continue comparing every other nearest sample and all filtered samples.
            let nearest_tie = interpolation == "nearest" && ((u - u.round()).abs() < 1e-5 || (v - v.round()).abs() < 1e-5);
            if nearest_tie {
                ties += 1;
            }
            for c in 0..4 {
                let error = |a: &[f32; 4], b: &[f32; 4]| if c == 3 { (a[c] - b[c]).abs() } else { (a[c] * a[3] - b[c] * b[3]).abs() };
                resident_worst = resident_worst.max(error(actual, resident));
                let d = error(actual, cpu);
                if !nearest_tie && d > cpu_worst {
                    cpu_worst = d;
                    location = (i, c);
                }
            }
        }
        if resident_worst > 1. / 255. + 2e-4 {
            self.failures.push(format!("{name}: GPU native commit differs from its preview by {resident_worst}"));
        }
        if cpu_worst > 1. / 255. + 2e-4 {
            self.failures
                .push(format!("{name}: CPU sampling differs by {cpu_worst} at {location:?}; {ties} nearest boundary samples checked through native commit"));
        }
        let stats = self.compositor.render(&self.device, &self.queue, original, original.bounds(), |_, _| {}).unwrap();
        if stats.tiles_uploaded != 0 || stats.fx_shapes != 0 {
            self.failures.push(format!("{name}: repeated frame uploaded {} tiles and built {} CPU shapes", stats.tiles_uploaded, stats.fx_shapes));
        }
    }
}

#[test]
fn cached_transform_matches_cpu_commit_geometry_interpolation_native_masks_and_selection() {
    let mut gpu = Device::new();
    let rect = Rect::new(12, 14, 36, 38);
    let geometries = [
        ("fractional shift", Homography([1., 0., 3.23, 0., 1., -2.37, 0., 0., 1.])),
        ("rotation", {
            let a = 0.27f64;
            let (s, c) = a.sin_cos();
            Homography([c, -s, 24. - c * 24. + s * 26., s, c, 26. - s * 24. - c * 26., 0., 0., 1.])
        }),
        ("perspective", Homography::rect_to_quad([12., 14., 36., 38.], [[10.3, 12.2], [40.4, 16.7], [32.6, 43.3], [15.2, 36.4]]).unwrap()),
        ("shrink", Homography([0.41, 0., 17.3, 0., 0.41, 13.7, 0., 0., 1.])),
    ];
    for (mode, sample) in [(ColorMode::Rgb, SampleType::F32), (ColorMode::Cmyk, SampleType::U16), (ColorMode::Lab, SampleType::U16)] {
        let format = PixelFormat::new(mode, sample, true);
        let mut doc = Document::new("parity", Size::new(64, 56), mode, sample);
        let mut layer = Layer::raster("pattern", format);
        *layer.surface_mut().unwrap() = patterned(format, rect);
        let id = layer.id;
        doc.layers.push(layer);
        let output = doc.bounds().inflate(12);
        let rest = vec![[0.; 4]; output.width() as usize * output.height() as usize];
        let surface = doc.layer(id).unwrap().surface().unwrap();
        let mut cached = CachedTransform::new(&gpu.device, &gpu.queue, output, vec![source(id, surface, rect, output, rest, None)]).unwrap();
        for interpolation in ["nearest", "bilinear", "bicubic"] {
            for (geometry, h) in geometries {
                let mut committed = doc.clone();
                *committed.layer_mut(id).unwrap().surface_mut().unwrap() = warp_surface(surface, rect, &h, Interp::parse(interpolation)).unwrap();
                gpu.compare(&format!("{mode:?}/{interpolation}/{geometry}"), &doc, &committed, &mut cached, h, interpolation, false);
            }
        }
        assert_eq!(surface.content_bounds(), rect, "all preview revisions preserve source pixels");
    }

    let format = PixelFormat::RGBA32F;
    let mut doc = Document::new("mask", Size::new(64, 56), ColorMode::Rgb, SampleType::F32);
    let mut layer = Layer::raster("feather", format);
    *layer.surface_mut().unwrap() = patterned(format, rect);
    let mut mask = LayerMask::reveal_all();
    mask.density = 0.72;
    mask.feather = 2.3;
    mask.surface.fill_rect(Rect::new(19, 18, 27, 35), &[0.]);
    layer.mask = Some(mask);
    let id = layer.id;
    doc.layers.push(layer);
    let output = doc.bounds().inflate(20);
    let count = output.width() as usize * output.height() as usize;
    let mask = doc.layer(id).unwrap().mask.as_ref().unwrap();
    let mask_rect = mask.surface.content_bounds();
    let raw = mask.surface.read_region(mask_rect);
    let mask_source = MaskSource {
        rect: mask_rect,
        values: raw,
        default: 1.,
        density: mask.density,
        alpha: None,
        rest: None,
        feather: photocraft_compose::masks::feather_sigma(mask.feather),
    };
    let surface = doc.layer(id).unwrap().surface().unwrap();
    let mut cached =
        CachedTransform::new(&gpu.device, &gpu.queue, output, vec![source(id, surface, rect, output, vec![[0.; 4]; count], Some(mask_source))]).unwrap();
    let h = Homography([0.73, 0., 9.2, 0., 0.84, 3.6, 0., 0., 1.]);
    let mut committed = doc.clone();
    *committed.layer_mut(id).unwrap().surface_mut().unwrap() = warp_surface(surface, rect, &h, Interp::Bilinear).unwrap();
    committed.layer_mut(id).unwrap().mask.as_mut().unwrap().surface = warp_mask(&mask.surface, &h, Interp::Bilinear);
    gpu.compare("linked raw mask, density and feather after scale", &doc, &committed, &mut cached, h, "bilinear", false);
    let shrink = Homography([0.41, 0., 17.3, 0., 0.41, 13.7, 0., 0., 1.]);
    let mut committed = doc.clone();
    *committed.layer_mut(id).unwrap().surface_mut().unwrap() = warp_surface(surface, rect, &shrink, Interp::Bicubic).unwrap();
    committed.layer_mut(id).unwrap().mask.as_mut().unwrap().surface = warp_mask(&mask.surface, &shrink, Interp::Bicubic);
    gpu.compare("linked raw mask bicubic prefilter", &doc, &committed, &mut cached, shrink, "bicubic", false);

    // Feather clamps to the canvas boundary; only presentation uses the outside default.
    // Raw mask pixels beyond that boundary must still survive the one-time native commit.
    cached.set_mask_clip(id, doc.bounds(), 1.0);
    let edge_shift = Homography([1., 0., -24., 0., 1., -16., 0., 0., 1.]);
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    let sources = cached
        .encode_sampled_with_homography(&gpu.queue, &mut encoder, [[0.; 2]; 4], false, "bilinear", Some(edge_shift), |x, y| edge_shift.apply(x, y), |_, _| 1.)
        .unwrap();
    gpu.queue.submit([encoder.finish()]);
    gpu.compositor.set_source_overrides(sources);
    let region = doc.bounds();
    let preview = render_to_vec(&mut gpu.compositor, &gpu.device, &gpu.queue, &doc, region).unwrap();
    let mut committed = doc.clone();
    for (layer, pixels, mask) in cached.readback_native(&gpu.queue).unwrap() {
        let target = committed.layer_mut(layer).unwrap();
        *target.surface_mut().unwrap() = pixels;
        if let Some(raw) = mask {
            let mask = target.mask.as_mut().unwrap();
            mask.surface = raw.try_convert(mask.surface.format()).unwrap();
        }
    }
    assert!(committed.layer(id).unwrap().mask.as_ref().unwrap().surface.content_bounds().x0 < 0, "offcanvas raw mask survives commit");
    let expected = photocraft_compose::render(&committed, region);
    let worst = preview
        .iter()
        .zip(&expected.px)
        .flat_map(|(a, b)| (0..4).map(move |c| if c == 3 { (a[c] - b[c]).abs() } else { (a[c] * a[3] - b[c] * b[3]).abs() }))
        .fold(0f32, f32::max);
    assert!(worst <= 1. / 255. + 2e-4, "canvas edge feather/default preview-native commit {worst}");

    // Lift the left half and retain the right half. The oracle composites the CPU-warped
    // lifted surface over the retained pixels using the normal layer compositor.
    let mut doc = Document::new("selection", Size::new(64, 56), ColorMode::Rgb, SampleType::F32);
    let surface = patterned(format, rect);
    let mut lifted = surface.clone();
    lifted.fill_rect(Rect::new(24, 14, 36, 38), &[0.; 4]);
    let mut left = surface.clone();
    left.fill_rect(Rect::new(12, 14, 24, 38), &[0.; 4]);
    let mut layer = Layer::raster("selected pixels", format);
    let id = layer.id;
    *layer.surface_mut().unwrap() = surface;
    doc.layers.push(layer);
    let output = doc.bounds().inflate(12);
    let rect = lifted.content_bounds();
    let mut cached = CachedTransform::new(&gpu.device, &gpu.queue, output, vec![source(id, &lifted, rect, output, rgba(&left, output), None)]).unwrap();
    for interpolation in ["nearest", "bilinear", "bicubic"] {
        let h = Homography([1.13, 0.06, 7.23, -0.03, 1.07, -2.37, 0., 0., 1.]);
        let moved = warp_surface(&lifted, rect, &h, Interp::parse(interpolation)).unwrap();
        let mut committed = doc.clone();
        *committed.layer_mut(id).unwrap().surface_mut().unwrap() = left.clone();
        let mut top = Layer::raster("moved selection", format);
        *top.surface_mut().unwrap() = moved;
        committed.layers.push(top);
        gpu.compare(&format!("partial selection/{interpolation}"), &doc, &committed, &mut cached, h, interpolation, false);
    }

    // The mesh path has a separate resampler at commit, even for an affine control grid.
    let surface = doc.layer(id).unwrap().surface().unwrap();
    let rect = surface.content_bounds();
    let mut cached = CachedTransform::new(
        &gpu.device,
        &gpu.queue,
        output,
        vec![source(id, surface, rect, output, vec![[0.; 4]; output.width() as usize * output.height() as usize], None)],
    )
    .unwrap();
    let h = Homography([1.1, 0.07, 0.23, -0.04, 1.12, 0.37, 0., 0., 1.]);
    for interpolation in ["nearest", "bilinear", "bicubic"] {
        let mut committed = doc.clone();
        *committed.layer_mut(id).unwrap().surface_mut().unwrap() =
            photocraft_algo::warp::warp_mesh_surface(surface, rect, &|x, y| h.apply(x, y), Interp::parse(interpolation));
        gpu.compare(&format!("mesh grid/{interpolation}"), &doc, &committed, &mut cached, h, interpolation, true);
    }
    // A genuinely nonlinear control field must keep the triangle inverse path. The native
    // result stored at Enter is the oracle: the former CPU mesh used a different adaptive
    // tessellation and f64 inverse, so it cannot define the GPU rasterizer's boundary samples.
    let nonlinear = |x: f64, y: f64| {
        let u = (x - f64::from(rect.x0)) / f64::from(rect.width());
        let v = (y - f64::from(rect.y0)) / f64::from(rect.height());
        (x + 4.2 * (std::f64::consts::PI * v).sin() * u, y + 2.7 * (std::f64::consts::PI * u).sin() * v)
    };
    for interpolation in ["nearest", "bilinear", "bicubic"] {
        cached.set_preview_region(Some(doc.bounds()));
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        let sources = cached
            .encode_sampled_with_homography(&gpu.queue, &mut encoder, [[0.; 2]; 4], true, interpolation, None, nonlinear, |_, _| 1.)
            .expect("nonlinear mesh preview");
        gpu.queue.submit([encoder.finish()]);
        gpu.compositor.set_source_overrides(sources);
        let preview = render_to_vec(&mut gpu.compositor, &gpu.device, &gpu.queue, &doc, doc.bounds()).unwrap();
        assert!(cached.readback_native(&gpu.queue).is_err());
        cached.set_preview_region(None);
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        cached.encode_sampled_with_homography(&gpu.queue, &mut encoder, [[0.; 2]; 4], true, interpolation, None, nonlinear, |_, _| 1.).unwrap();
        gpu.queue.submit([encoder.finish()]);
        let mut committed = doc.clone();
        for (id, pixels, _) in cached.readback_native(&gpu.queue).unwrap() {
            *committed.layer_mut(id).unwrap().surface_mut().unwrap() = pixels;
        }
        let expected = photocraft_compose::flatten(&committed);
        let worst = preview
            .iter()
            .zip(&expected.px)
            .flat_map(|(a, b)| (0..4).map(move |c| if c == 3 { (a[c] - b[c]).abs() } else { (a[c] * a[3] - b[c] * b[3]).abs() }))
            .fold(0f32, f32::max);
        assert!(worst <= 1. / 255. + 2e-4, "nonlinear mesh/{interpolation}: preview/native commit {worst}");
        assert_eq!(surface.content_bounds(), rect, "nonlinear preview preserves the source");
    }
    assert!(gpu.failures.is_empty(), "{}", gpu.failures.join("\n"));
}

#[test]
fn roi_preview_hides_the_old_source_and_commit_materializes_offscreen_pixels() {
    let mut gpu = Device::new();
    let rect = Rect::new(12, 14, 36, 38);
    let mut doc = Document::new("offscreen move", Size::new(128, 64), ColorMode::Rgb, SampleType::F32);
    let mut layer = Layer::raster("moving", PixelFormat::RGBA32F);
    *layer.surface_mut().unwrap() = patterned(PixelFormat::RGBA32F, rect);
    let id = layer.id;
    doc.layers.push(layer);
    let original = doc.layer(id).unwrap().surface().unwrap();
    let output = doc.bounds().inflate(12);
    let count = output.width() as usize * output.height() as usize;
    let mut cached = CachedTransform::new(&gpu.device, &gpu.queue, output, vec![source(id, original, rect, output, vec![[0.; 4]; count], None)]).unwrap();
    let h = Homography([1., 0., 300., 0., 1., 0., 0., 0., 1.]);
    cached.set_preview_region(Some(Rect::new(0, 0, 64, 64)));
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    let sources =
        cached.encode_sampled_with_homography(&gpu.queue, &mut encoder, [[0.; 2]; 4], false, "bicubic", Some(h), |x, y| h.apply(x, y), |_, _| 1.).unwrap();
    assert!(sources.contains_key(&id), "offscreen targets retain an override instead of falling back to frozen CPU pixels");
    gpu.queue.submit([encoder.finish()]);
    let frozen_preview = sources.clone();
    gpu.compositor.set_source_overrides(sources);
    let preview = render_to_vec(&mut gpu.compositor, &gpu.device, &gpu.queue, &doc, Rect::new(0, 0, 64, 64)).unwrap();
    assert!(preview.iter().all(|p| p[3] == 0.), "moving entirely outside the viewport hides the source immediately");
    assert!(cached.readback_native(&gpu.queue).is_err());
    let identity = Homography([1., 0., 0., 0., 1., 0., 0., 0., 1.]);
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    cached.encode_sampled_with_homography(&gpu.queue, &mut encoder, [[0.; 2]; 4], false, "bicubic", Some(identity), |x, y| (x, y), |_, _| 1.).unwrap();
    gpu.queue.submit([encoder.finish()]);
    gpu.compositor.set_source_overrides(frozen_preview);
    let old_frame = render_to_vec(&mut gpu.compositor, &gpu.device, &gpu.queue, &doc, Rect::new(0, 0, 64, 64)).unwrap();
    assert_eq!(old_frame, preview, "preparing a newer geometry cannot overwrite published preview textures");
    cached.set_preview_region(None);
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    cached.encode_sampled_with_homography(&gpu.queue, &mut encoder, [[0.; 2]; 4], false, "bicubic", Some(h), |x, y| h.apply(x, y), |_, _| 1.).unwrap();
    gpu.queue.submit([encoder.finish()]);
    let commits = cached.readback_native(&gpu.queue).unwrap();
    let committed = &commits[0].1;
    assert_eq!(committed.content_bounds(), rect.translate(300, 0), "canonical commit includes the whole result beyond both viewport and canvas");
    for y in rect.y0..rect.y1 {
        for x in rect.x0..rect.x1 {
            for c in 0..4 {
                assert!((committed.sample_channel(x + 300, y, c) - original.sample_channel(x, y, c)).abs() < 2e-5, "offscreen native commit pixel {x},{y}/{c}");
            }
        }
    }
    assert_eq!(original.content_bounds(), rect);
}
