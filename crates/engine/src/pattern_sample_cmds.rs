//! Native-resolution coloured patches for the painting colour source.
use crate::{EngineError, Session, commands::CommandSpec, sample_cmds};
use photocraft_geom::Rect;
use photocraft_paint::brush::ColorPattern;
use serde_json::{Value, json};

fn bad(cmd: &str, msg: &str) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "tools.setColorSource",
            label: "Painting Color Source",
            menu: &[],
            shortcut: None,
            params: r#"{"pattern":bool} → select solid colour or sampled pattern; pattern mode can be armed before sampling"#,
            enabled: crate::commands::always,
            run: |s, p| {
                let pattern = p.get("pattern").and_then(Value::as_bool).ok_or_else(|| bad("tools.setColorSource", "`pattern` must be a boolean"))?;
                s.tools.brush.pattern_enabled = pattern;
                s.tools.brush_changed();
                Ok(Value::Null)
            },
            journal: true,
        },
        CommandSpec {
            id: "document.samplePattern",
            label: "Sample Painting Pattern",
            menu: &[],
            shortcut: None,
            params: r#"{"x":f64,"y":f64,"size":u32=brushSize (1..2048),"sampleLayer":"current"|"currentAndBelow"|"all"|"allNoAdjustments"|"currentAndBelowNoAdjustments"="all"} → {width,height}; stores a native-scale square RGBA patch, clamping canvas-edge pixels"#,
            enabled: |s| s.active().map(|_| ()).ok_or_else(|| "no document open".into()),
            run: sample,
            journal: true,
        },
    ]
}

fn sample(s: &mut Session, p: &Value) -> crate::Result<Value> {
    const CMD: &str = "document.samplePattern";
    let coord = |key| p.get(key).and_then(Value::as_f64).filter(|v| v.is_finite()).ok_or_else(|| bad(CMD, "`x` and `y` must be finite numbers"));
    let (x, y) = (coord("x")?, coord("y")?);
    let size = match p.get("size") {
        None => {
            let n = s.tools.brush.size.ceil();
            if !n.is_finite() {
                return Err(bad(CMD, "brush size must be finite"));
            }
            n.clamp(1.0, 2048.0) as u32
        }
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (1..=2048).contains(n))
            .ok_or_else(|| bad(CMD, "`size` must be an integer between 1 and 2048"))?,
    };
    let which = sample_cmds::layers_param(CMD, p)?;
    let d = s.active().ok_or(EngineError::NoDocument)?;
    // Reuse the point sampler's finite/in-canvas validation and all five layer scopes.
    let center = sample_cmds::sample_rect(x, y, 1, d.doc.bounds()).ok_or_else(|| bad(CMD, "sample point is outside the canvas"))?;
    let src = sample_cmds::sampled_document(&d.doc, d.active_layer, which).ok_or_else(|| bad(CMD, "sample scope requires an active layer"))?;
    let half = (size / 2) as i32;
    let left = center.x0.saturating_sub(half);
    let top = center.y0.saturating_sub(half);
    let bounds = d.doc.bounds();
    let rect = Rect::new(left, top, left.saturating_add(size as i32), top.saturating_add(size as i32)).intersect(&bounds);
    let rendered = photocraft_compose::render(&src, rect);
    let width = (rect.x1 - rect.x0) as usize;
    let mut pixels = Vec::with_capacity(size as usize * size as usize);
    for yy in 0..size {
        let sy = top.saturating_add(yy as i32).clamp(bounds.y0, bounds.y1 - 1);
        for xx in 0..size {
            let sx = left.saturating_add(xx as i32).clamp(bounds.x0, bounds.x1 - 1);
            let at = (sy - rect.y0) as usize * width + (sx - rect.x0) as usize;
            let pixel = rendered.px.get(at).copied().ok_or_else(|| bad(CMD, "sample render did not cover the requested patch"))?;
            pixels.push(pixel);
        }
    }
    let pattern = ColorPattern { width: size, height: size, pixels: pixels.into() };
    if !pattern.is_valid() {
        return Err(bad(CMD, "sample contains invalid colour values"));
    }
    s.tools.brush.sampled_pattern = Some(pattern);
    s.tools.brush.pattern_enabled = true;
    s.tools.brush_changed();
    Ok(json!({"width":size,"height":size}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pattern_sampling_preserves_native_pixels_scopes_depth_and_edges() {
        for depth in [8, 16, 32] {
            let mut s = Session::new();
            s.execute("file.new", json!({"width":4,"height":3,"depth":depth,"background":"white"})).unwrap();
            sample(&mut s, &json!({"x":0,"y":0,"size":6,"sampleLayer":"current"})).unwrap();
            let p = s.tools.brush.sampled_pattern.as_ref().unwrap();
            assert_eq!((p.width, p.height, p.pixels.len()), (6, 6, 36));
            assert!(p.pixels.iter().all(|c| *c == [1.; 4]));
            assert!(s.tools.brush.pattern_enabled);
            let original = p.clone();
            for params in [
                json!({}),
                json!({"x":-1,"y":0}),
                json!({"x":1e100,"y":0}),
                json!({"x":0,"y":0,"size":0}),
                json!({"x":0,"y":0,"size":2049}),
                json!({"x":0,"y":0,"size":1.5}),
                json!({"x":0,"y":0,"size":"3"}),
                json!({"x":0,"y":0,"sampleLayer":"bad"}),
            ] {
                assert!(sample(&mut s, &params).is_err(), "{params}");
                assert_eq!(s.tools.brush.sampled_pattern.as_ref(), Some(&original));
            }
        }
    }

    #[test]
    fn float_pattern_keeps_hdr_and_sub_byte_precision() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width":2,"height":2,"depth":32,"background":"transparent"})).unwrap();
        let d = s.active_mut().unwrap();
        let id = d.active_layer.unwrap();
        let pixel = [1.25, 0.123456, 0.456789, 1.];
        std::sync::Arc::make_mut(&mut d.doc).layer_mut(id).unwrap().surface_mut().unwrap().write_pixel(1, 1, &pixel);
        sample(&mut s, &json!({"x":1,"y":1,"size":1})).unwrap();
        let p = s.tools.brush.sampled_pattern.as_ref().unwrap();
        for (a, b) in p.pixels[0].iter().zip(pixel) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn pattern_preserves_spatial_colors_and_matches_each_eyedropper_scope() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width":8,"height":8,"background":"white"})).unwrap();
        s.execute("shape.create", json!({"kind":"rect","rect":[0,0,4,8],"fill":"#ff0000"})).unwrap();
        for (which, name) in sample_cmds::SampleLayers::ALL {
            let d = s.active().unwrap();
            let expected: Vec<_> =
                (2..6).flat_map(|y| (2..6).map(move |x| sample_cmds::sample_color(&d.doc, d.active_layer, f64::from(x), f64::from(y), 1, which))).collect();
            sample(&mut s, &json!({"x":4,"y":4,"size":4,"sampleLayer":name})).unwrap();
            assert_eq!(s.tools.brush.sampled_pattern.as_ref().unwrap().pixels.as_ref(), expected.as_slice(), "{name}");
        }
        let patch = s.tools.brush.sampled_pattern.as_ref().unwrap().clone();
        let toggle = specs().remove(0);
        (toggle.run)(&mut s, &json!({"pattern":false})).unwrap();
        assert!(!s.tools.brush.pattern_enabled);
        assert_eq!(s.tools.brush.sampled_pattern.as_ref(), Some(&patch));
        for value in [json!({}), json!({"pattern":1}), json!({"pattern":"true"})] {
            assert!((toggle.run)(&mut s, &value).is_err());
        }
    }
}
