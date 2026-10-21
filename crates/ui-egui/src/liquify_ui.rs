//! Filter › Liquify…: a full-window dialog like Photoshop's (tool strip on the left, properties on
//! the right, preview in the middle, OK / Cancel).
//!
//! Native previews keep the original full-resolution pixels resident on the GPU. Pointer events
//! edit the inverse field; the shader samples the original without resampling document pixels.
//! Only confirmation commits the recorded strokes, as one document history entry.

#[path = "liquify_gpu.rs"]
mod gpu;

use egui::{Align2, Color32, FontId, Pos2, Rect as ERect, Sense, Stroke, TextureHandle, pos2, vec2};
use photocraft_algo::liquify::{LiquifyField, LiquifyStroke, LiquifyTool, ProxyImage, auto_cell};
use photocraft_doc::LayerId;
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::canvas::ToolEvent;
use crate::theme::Tokens;
use crate::widgets;

/// Longest side of the preview proxy (px).
const PROXY_SIDE: usize = 1600;
const MAX_VIEW_PREVIEW_PIXELS: usize = 8 * 1024 * 1024;
const LEFT_W: f32 = 48.0;
const RIGHT_W: f32 = 292.0;
const REDO_STACK_LIMIT: usize = 100;

/// Options shown in the properties panel (and settable over the control channel).
#[derive(Clone, Debug)]
pub struct LiquifyOpts {
    pub tool: LiquifyTool,
    pub scope: String,
    pub size: f32,
    pub density: f32,
    pub pressure: f32,
    pub rate: f32,
    pub show_mesh: bool,
    /// 0 small, 1 medium, 2 large.
    pub mesh_size: usize,
    pub show_mask: bool,
    pub show_backdrop: bool,
    pub backdrop_opacity: f32,
    pub reconstruct_amount: f32,
}

impl Default for LiquifyOpts {
    fn default() -> Self {
        LiquifyOpts {
            tool: LiquifyTool::ForwardWarp,
            scope: "layer".into(),
            size: 100.0,
            density: 50.0,
            pressure: 100.0,
            rate: 80.0,
            show_mesh: false,
            mesh_size: 1,
            show_mask: true,
            show_backdrop: false,
            backdrop_opacity: 50.0,
            reconstruct_amount: 100.0,
        }
    }
}

/// The `prefs.dialogs` key under which the brush settings are remembered between uses and
/// launches (#418).
const REMEMBERED: &str = "filter.liquify";

impl LiquifyOpts {
    /// Applies the settings named in `v` (the control channel's and the remembered keys),
    /// clamped to their ranges; the others are left as they are. A bad `tool` is an error, after
    /// everything else has been applied.
    pub fn apply(&mut self, v: &Value) -> Result<(), String> {
        if let Some(scope) = v.get("scope").and_then(Value::as_str) {
            if !matches!(scope, "layer" | "all" | "group") {
                return Err("scope must be layer, all or group".into());
            }
            self.scope = scope.into();
        }
        let num = |k: &str| v.get(k).and_then(Value::as_f64).filter(|n| n.is_finite()).map(|n| n as f32);
        let flag = |k: &str| v.get(k).and_then(Value::as_bool);
        if let Some(n) = num("size") {
            self.size = n.clamp(1.0, 15000.0);
        }
        for (k, slot) in
            [("density", &mut self.density), ("pressure", &mut self.pressure), ("rate", &mut self.rate), ("backdropOpacity", &mut self.backdrop_opacity)]
        {
            if let Some(n) = num(k) {
                *slot = n.clamp(0.0, 100.0);
            }
        }
        for (k, slot) in [("showMesh", &mut self.show_mesh), ("showMask", &mut self.show_mask), ("showBackdrop", &mut self.show_backdrop)] {
            if let Some(b) = flag(k) {
                *slot = b;
            }
        }
        if let Some(m) = v.get("meshSize").and_then(Value::as_str) {
            self.mesh_size = match m {
                "small" => 0,
                "large" => 2,
                _ => 1,
            };
        }
        if let Some(t) = v.get("tool") {
            self.tool = serde_json::from_value(t.clone()).map_err(|e| format!("bad tool: {e}"))?;
        }
        Ok(())
    }

    /// The settings as [`LiquifyOpts::apply`] reads them.
    pub fn to_json(&self) -> Value {
        let mesh = ["small", "medium", "large"].get(self.mesh_size).copied().unwrap_or("medium");
        json!({
            "tool": self.tool,
            "scope": self.scope,
            "size": self.size,
            "density": self.density,
            "pressure": self.pressure,
            "rate": self.rate,
            "showMesh": self.show_mesh,
            "meshSize": mesh,
            "showMask": self.show_mask,
            "showBackdrop": self.show_backdrop,
            "backdropOpacity": self.backdrop_opacity,
        })
    }
}

pub struct LiquifyDialog {
    pub layer: LayerId,
    targets: Vec<LayerId>,
    cached_scope: String,
    scope_dirty: bool,
    gpu: Option<std::sync::Arc<gpu::Preview>>,
    gpu_dirty: bool,
    gpu_doc: Option<std::sync::Arc<photocraft_doc::Document>>,
    gpu_error: Option<String>,
    gpu_damage: Option<Rect>,
    layer_name: String,
    canvas: Rect,
    cell: f64,
    pub field: LiquifyField,
    pub strokes: Vec<LiquifyStroke>,
    redo: Vec<LiquifyStroke>,
    /// The stroke being drawn and its last point.
    cur: Option<(LiquifyStroke, [f64; 3])>,
    /// The lasso polygon being drawn (document px) and whether it thaws (Alt held at pointer-down).
    lasso: Option<(bool, Vec<[f64; 2]>)>,
    proxy: ProxyImage,
    source: Surface,
    out: Vec<[u8; 4]>,
    tex: Option<TextureHandle>,
    /// Proxy pixels to re-render and upload (x0, y0, x1, y1).
    dirty: Option<[usize; 4]>,
    /// Document-pixel preview used above 100% zoom.
    view_bounds: Option<Rect>,
    view_out: Vec<[u8; 4]>,
    view_source_scratch: Vec<[f32; 4]>,
    view_tex: Option<TextureHandle>,
    view_dirty: Option<Rect>,
    backdrop: Option<TextureHandle>,
    mask_tex: Option<TextureHandle>,
    mask_dirty: bool,
    pub opts: LiquifyOpts,
    /// Screen px per document px (0 = fit on next frame) and the view centre (document px).
    zoom: f32,
    center: [f64; 2],
    hover: Option<[f64; 2]>,
    /// Alt+right-drag (or Control+Alt-drag) resizing the brush: screen anchor, starting size, starting density.
    brush_resize: Option<(Pos2, f32, f32)>,
    last_dab: f64,
    /// Last dab + proxy update time (ms), shown in the footer.
    pub dab_ms: f64,
    /// CPU time spent refreshing the zoomed-in source-resolution preview.
    pub preview_ms: f64,
}

fn union(a: Option<[usize; 4]>, b: [usize; 4]) -> Option<[usize; 4]> {
    if b[0] >= b[2] || b[1] >= b[3] {
        return a;
    }
    Some(match a {
        Some(a) => [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])],
        None => b,
    })
}

fn union_rect(a: Option<Rect>, b: Rect) -> Option<Rect> {
    if b.is_empty() {
        return a;
    }
    Some(match a {
        Some(a) => Rect::new(a.x0.min(b.x0), a.y0.min(b.y0), a.x1.max(b.x1), a.y1.max(b.y1)),
        None => b,
    })
}

impl LiquifyDialog {
    pub fn describe(&self) -> Value {
        json!({
            "layer": self.layer.0,
            "scope": self.opts.scope,
            "layers": self.targets.iter().map(|id| id.0).collect::<Vec<_>>(),
            "gpuCached": self.gpu.is_some(),
            "tool": self.opts.tool,
            "size": self.opts.size,
            "strokes": self.strokes.len() + usize::from(self.cur.is_some()),
            "redo": self.redo.len(),
            "meshSize": self.cell,
            "maxDisplacement": self.field.max_displacement(),
            "proxy": [self.proxy.w, self.proxy.h],
            "dabMs": self.dab_ms,
            "fullResolutionPreview": self.gpu.is_some() || self.view_bounds.is_some(),
            "previewMs": self.preview_ms,
        })
    }

    fn template(&self, tool: LiquifyTool) -> LiquifyStroke {
        let o = &self.opts;
        LiquifyStroke {
            tool,
            size: f64::from(o.size),
            density: f64::from(o.density),
            pressure: f64::from(o.pressure),
            rate: f64::from(o.rate),
            points: Vec::new(),
            amount: None,
        }
    }

    fn mark(&mut self, doc_rect: Rect, mask: bool) {
        if doc_rect.is_empty() {
            return;
        }
        self.gpu_damage = union_rect(self.gpu_damage, doc_rect.inflate((self.cell * 2.).ceil() as i32).intersect(&self.canvas));
        self.dirty = union(self.dirty, self.proxy.proxy_rect(doc_rect));
        self.view_dirty = union_rect(self.view_dirty, doc_rect);
        self.mask_dirty |= mask;
    }

    /// Starts a stroke; `alt` draws with the selected tool's opposite (see [`alt_tool`]).
    fn begin(&mut self, p: [f64; 3], now: f64, alt: bool) {
        self.redo.clear();
        let tool = if alt { alt_tool(self.opts.tool) } else { self.opts.tool };
        let mut s = self.template(tool);
        s.points.push(p.to_vec());
        let t0 = crate::gpu_canvas::now_ms();
        let d = self.field.stroke_begin(&s, p);
        self.mark(d, matches!(s.tool, LiquifyTool::Freeze | LiquifyTool::Thaw));
        self.render_dirty();
        self.dab_ms = crate::gpu_canvas::now_ms() - t0;
        self.cur = Some((s, p));
        self.last_dab = now;
    }

    fn extend(&mut self, p: [f64; 3], now: f64) {
        let Some((mut s, last)) = self.cur.take() else { return };
        let t0 = crate::gpu_canvas::now_ms();
        let d = self.field.stroke_segment(&s, last, p);
        s.points.push(p.to_vec());
        self.mark(d, matches!(s.tool, LiquifyTool::Freeze | LiquifyTool::Thaw));
        self.render_dirty();
        self.dab_ms = crate::gpu_canvas::now_ms() - t0;
        self.cur = Some((s, p));
        self.last_dab = now;
    }

    fn end(&mut self) {
        if let Some((s, _)) = self.cur.take() {
            self.strokes.push(s);
        }
    }

    /// Closes the lasso polygon into the freeze mask, recorded like any stroke (so undo, replay
    /// and OK all treat it the same as Freeze/Thaw brush work).
    fn close_lasso(&mut self, subtract: bool, mut pts: Vec<[f64; 2]>) {
        if pts.len() < 3 {
            return;
        }
        if let (Some(&first), Some(&last)) = (pts.first(), pts.last())
            && ((first[0] - last[0]).abs() > 0.5 || (first[1] - last[1]).abs() > 0.5)
        {
            pts.push(first);
        }
        self.redo.clear();
        let mut s = self.template(LiquifyTool::LassoMask);
        s.amount = Some(if subtract { 0.0 } else { 1.0 });
        s.points = pts.into_iter().map(|p| p.to_vec()).collect();
        let d = self.field.apply_stroke(&s);
        self.strokes.push(s);
        self.mark(d, true);
        self.render_dirty();
    }

    /// Applies a whole-field operation (Reconstruct…, mask buttons) as a stroke.
    fn global(&mut self, tool: LiquifyTool, amount: Option<f64>) {
        self.end();
        self.redo.clear();
        let mut s = LiquifyStroke::new(tool, 1.0);
        s.amount = amount;
        let d = self.field.apply_stroke(&s);
        self.strokes.push(s);
        self.mark(d, true);
        self.render_dirty();
    }

    fn rebuild(&mut self) {
        self.gpu_dirty = true;
        let gpu_only = self.field.gpu_only;
        self.field = LiquifyField::new(self.canvas, self.cell);
        self.field.gpu_only = gpu_only;
        for stroke in &self.strokes {
            self.field.apply_stroke(stroke);
        }
        self.dirty = Some([0, 0, self.proxy.w, self.proxy.h]);
        // Undo rebuilds the whole field. The zoomed viewport is a separate buffer; leaving it
        // clean kept the pre-undo pixels on screen until the next zoom (#4100).
        self.view_dirty = self.view_bounds;
        self.mask_dirty = true;
        self.render_dirty();
    }

    fn undo(&mut self) {
        self.end();
        if let Some(stroke) = self.strokes.pop() {
            if self.redo.len() == REDO_STACK_LIMIT {
                self.redo.remove(0);
            }
            self.redo.push(stroke);
            self.rebuild();
        }
    }

    fn redo(&mut self) {
        self.end();
        if let Some(stroke) = self.redo.pop() {
            let d = self.field.apply_stroke(&stroke);
            self.strokes.push(stroke);
            self.mark(d, true);
            self.render_dirty();
        }
    }

    fn restore_all(&mut self) {
        self.cur = None;
        self.lasso = None;
        self.strokes.clear();
        self.redo.clear();
        self.rebuild();
    }

    /// Re-renders the dirty proxy pixels into `out` (the texture upload happens in `show`).
    fn render_dirty(&mut self) {
        if self.gpu.is_some() {
            return;
        }
        if let Some(r) = self.dirty {
            self.proxy.render(&self.field, r, &mut self.out);
        }
    }

    /// Builds or updates the visible source-resolution viewport. At ordinary zoom the whole-layer
    /// proxy remains cheaper; above 100%, a full-resolution crop is enough to fill the preview.
    fn update_view_preview(&mut self, ctx: &egui::Context, area: ERect) {
        if self.gpu.is_some() || self.zoom <= 1.0 {
            return;
        }
        let half_w = f64::from(area.width() / self.zoom) * 0.5;
        let half_h = f64::from(area.height() / self.zoom) * 0.5;
        let clamp_x = |x: f64| x.max(f64::from(self.canvas.x0)).min(f64::from(self.canvas.x1)) as i32;
        let clamp_y = |y: f64| y.max(f64::from(self.canvas.y0)).min(f64::from(self.canvas.y1)) as i32;
        let bounds = Rect::new(
            clamp_x((self.center[0] - half_w).floor()),
            clamp_y((self.center[1] - half_h).floor()),
            clamp_x((self.center[0] + half_w).ceil()),
            clamp_y((self.center[1] + half_h).ceil()),
        );
        if bounds.is_empty() {
            self.view_bounds = None;
            self.view_tex = None;
            self.view_dirty = None;
            return;
        }
        let (w, h) = (bounds.width() as usize, bounds.height() as usize);
        let Some(len) = w.checked_mul(h).filter(|n| *n <= MAX_VIEW_PREVIEW_PIXELS) else {
            self.view_bounds = None;
            self.view_tex = None;
            self.view_dirty = None;
            return;
        };
        if self.view_bounds != Some(bounds) || self.view_out.len() != len {
            if self.view_out.try_reserve(len.saturating_sub(self.view_out.len())).is_err() {
                self.view_bounds = None;
                self.view_tex = None;
                self.view_dirty = None;
                return;
            }
            self.view_out.resize(len, [0; 4]);
            self.view_bounds = Some(bounds);
            self.view_tex = None;
            self.view_dirty = Some(bounds);
        }
        let Some(dirty) = self.view_dirty.take().map(|r| r.intersect(&bounds)) else { return };
        if dirty.is_empty() {
            return;
        }
        let started = crate::gpu_canvas::now_ms();
        let rendered = ProxyImage::render_full_resolution(&self.source, &self.field, bounds, dirty, &mut self.view_out, &mut self.view_source_scratch);
        if !rendered {
            self.view_bounds = None;
            self.view_tex = None;
            self.preview_ms = crate::gpu_canvas::now_ms() - started;
            return;
        }
        let x0 = (dirty.x0 - bounds.x0) as usize;
        let y0 = (dirty.y0 - bounds.y0) as usize;
        let x1 = (dirty.x1 - bounds.x0) as usize;
        let y1 = (dirty.y1 - bounds.y0) as usize;
        let mut pixels = Vec::with_capacity((x1 - x0) * (y1 - y0));
        for y in y0..y1 {
            if let Some(row) = self.view_out.get(y * w + x0..y * w + x1) {
                pixels.extend(row.iter().map(|p| Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3])));
            }
        }
        let image = egui::ColorImage::new([x1 - x0, y1 - y0], pixels);
        match &mut self.view_tex {
            Some(tex) => tex.set_partial([x0, y0], image, egui::TextureOptions::NEAREST),
            None => {
                // A new viewport always starts dirty over its full bounds.
                self.view_tex = Some(ctx.load_texture(
                    "liquify-detail",
                    {
                        let pixels = self.view_out.iter().map(|p| Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3])).collect();
                        egui::ColorImage::new([w, h], pixels)
                    },
                    egui::TextureOptions::NEAREST,
                ));
            }
        }
        self.preview_ms = crate::gpu_canvas::now_ms() - started;
    }

    fn upload(&mut self, ctx: &egui::Context) {
        let (w, h) = (self.proxy.w, self.proxy.h);
        if self.gpu.is_none() {
            match (&mut self.tex, self.dirty.take()) {
                (None, _) => {
                    self.proxy.render(&self.field, [0, 0, w, h], &mut self.out);
                    let img = egui::ColorImage::new([w, h], self.out.iter().map(|p| Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3])).collect());
                    self.tex = Some(ctx.load_texture("liquify-proxy", img, egui::TextureOptions::LINEAR));
                }
                (Some(tex), Some(r)) => {
                    let (x0, y0, x1, y1) = (r[0].min(w), r[1].min(h), r[2].min(w), r[3].min(h));
                    if x1 > x0 && y1 > y0 {
                        let mut px = Vec::with_capacity((x1 - x0) * (y1 - y0));
                        for y in y0..y1 {
                            px.extend(self.out[y * w + x0..y * w + x1].iter().map(|p| Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3])));
                        }
                        tex.set_partial([x0, y0], egui::ColorImage::new([x1 - x0, y1 - y0], px), egui::TextureOptions::LINEAR);
                    }
                }
                _ => {}
            }
        }
        if self.gpu.is_none() && (self.mask_dirty || self.mask_tex.is_none()) {
            self.mask_dirty = false;
            let f = &self.field;
            let img = egui::ColorImage::new(
                [f.w, f.h],
                f.freeze.iter().map(|v| Color32::from_rgba_unmultiplied(230, 40, 40, (v.clamp(0.0, 1.0) * 140.0) as u8)).collect(),
            );
            match &mut self.mask_tex {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.mask_tex = Some(ctx.load_texture("liquify-mask", img, egui::TextureOptions::LINEAR)),
            }
        }
    }
}

pub(crate) fn has_pending_edits(app: &PhotocraftApp) -> bool {
    app.distort.liquify.as_ref().is_some_and(|dialog| !dialog.strokes.is_empty() || dialog.cur.is_some())
}

/// Opens the dialog on the active layer.
pub fn open(app: &mut PhotocraftApp, ctx: &egui::Context) -> Result<(), String> {
    #[cfg(not(test))]
    if app.gpu.as_ref().is_none_or(|gpu| !gpu.health().is_ok()) {
        return Err("Liquify requires an available GPU canvas.".into());
    }
    let st = app.session.active().ok_or("no document")?;
    let layer = st.active_layer.ok_or("no active layer")?;
    let ids = photocraft_engine::distort_cmds::liquify_targets(&app.session, &json!({})).map_err(|e| e.to_string())?;
    let surf = ids.first().and_then(|id| st.doc.layer(*id)).and_then(|l| l.surface()).cloned().ok_or("target has no pixel cache")?;
    let st = app.session.active().ok_or("no document")?;
    let canvas = st.doc.bounds();
    let name = st.doc.layer(layer).map(|l| l.name.clone()).unwrap_or_default();
    let cell = auto_cell(canvas);
    let proxy = if app.gpu.is_some() {
        ProxyImage::new(&Surface::new(photocraft_color::PixelFormat::RGBA32F), Rect::new(0, 0, 1, 1), 1)
    } else {
        ProxyImage::new(&surf, canvas, PROXY_SIDE)
    };
    let out = vec![[0u8; 4]; proxy.w * proxy.h];
    let center = [f64::from(canvas.x0 + canvas.x1) / 2.0, f64::from(canvas.y0 + canvas.y1) / 2.0];
    let mut d = LiquifyDialog {
        layer,
        targets: vec![layer],
        cached_scope: "layer".into(),
        scope_dirty: true,
        gpu: None,
        gpu_dirty: true,
        gpu_doc: None,
        gpu_error: None,
        gpu_damage: None,
        layer_name: name,
        canvas,
        cell,
        field: LiquifyField::new(canvas, cell),
        strokes: Vec::new(),
        redo: Vec::new(),
        cur: None,
        lasso: None,
        proxy,
        source: surf,
        out,
        tex: None,
        dirty: None,
        view_bounds: None,
        view_out: Vec::new(),
        view_source_scratch: Vec::new(),
        view_tex: None,
        view_dirty: None,
        backdrop: None,
        mask_tex: None,
        mask_dirty: true,
        opts: LiquifyOpts::default(),
        zoom: 0.0,
        center,
        hover: None,
        brush_resize: None,
        last_dab: 0.0,
        dab_ms: 0.0,
        preview_ms: 0.0,
    };
    // A sensible starting brush (about a tenth of the image), then the settings last used, so
    // Liquify opens as it was left (#418). Saved values are clamped; a bad one is skipped.
    d.opts.size = ((canvas.width().max(canvas.height()) as f32) / 10.0).round().clamp(10.0, 1500.0);
    if let Some(saved) = app.session.prefs().dialogs.get(REMEMBERED) {
        let _ = d.opts.apply(saved);
    }
    app.distort.liquify = Some(d);
    if let Err(error) = refresh_scope(app, Some(ctx)) {
        app.distort.liquify = None;
        return Err(error);
    }
    Ok(())
}

fn scope_label(scope: &str) -> &'static str {
    match scope {
        "all" => "全部图层",
        "group" => "图层组",
        _ => "图层",
    }
}

fn cached_mask(layer: &photocraft_doc::Layer, canvas: Rect) -> Option<Surface> {
    if let Some(mask) = photocraft_compose::masks::combined_mask(layer, canvas) {
        return Some(mask);
    }
    let mask = layer.mask.as_ref().filter(|m| m.enabled)?;
    let mut surface = Surface::new(photocraft_color::PixelFormat::new(photocraft_color::ColorMode::Grayscale, photocraft_color::SampleType::F32, false));
    let channels = mask.surface.channels().max(1);
    let values =
        mask.surface.read_region(canvas).chunks_exact(channels).map(|p| 1. - mask.density * (1. - p.first().copied().unwrap_or(1.))).collect::<Vec<_>>();
    surface.write_region(canvas, &values);
    Some(surface)
}

fn refresh_scope(app: &mut PhotocraftApp, ctx: Option<&egui::Context>) -> Result<(), String> {
    #[cfg(not(test))]
    if app.gpu.as_ref().is_none_or(|gpu| !gpu.health().is_ok()) {
        return Err("Liquify requires an available GPU canvas.".into());
    }
    let scope = app.distort.liquify.as_ref().ok_or("Liquify is not open")?.opts.scope.clone();
    let st = app.session.active().ok_or("no document")?;
    let canvas = st.doc.bounds();
    let targets = photocraft_engine::distort_cmds::liquify_targets(&app.session, &json!({"scope":scope})).map_err(|e| e.to_string())?;
    let mut doc = (*st.doc).clone();
    // The dialog owns its display texture. The document canvas and Navigator can refresh
    // independently while the cached Liquify edit remains unchanged.
    doc.id = app.distort.liquify.as_ref().and_then(|d| d.gpu_doc.as_ref().map(|doc| doc.id)).unwrap_or_else(photocraft_doc::DocId::fresh);
    fn retain(layers: &mut [photocraft_doc::Layer], targets: &[LayerId]) -> bool {
        let mut any = false;
        for layer in layers {
            let keep = if let Some(children) = layer.children_mut() { retain(children, targets) } else { targets.contains(&layer.id) };
            layer.visible &= keep;
            any |= keep;
        }
        any
    }
    retain(&mut doc.layers, &targets);
    let d = app.distort.liquify.as_mut().ok_or("Liquify is not open")?;
    let preview = if let Some(renderer) = &app.gpu {
        let mut sources = Vec::new();
        for id in &targets {
            let layer = doc.layer_mut(*id).ok_or("missing target layer")?;
            let surface = layer.surface().cloned().ok_or("target has no pixel cache")?;
            let linked = layer.mask.as_ref().is_some_and(|m| m.linked);
            let mut moving_mask = layer.clone();
            moving_mask.vector_mask = None;
            if !linked {
                moving_mask.mask = None;
            }
            let mut fixed_mask = layer.clone();
            if linked {
                fixed_mask.mask = None;
            }
            sources.push(gpu::Source {
                id: *id,
                surface: surface.clone(),
                mask: moving_mask.mask.as_ref().map(|m| m.surface.clone()),
                linked,
                stationary: cached_mask(&fixed_mask, canvas),
                feather: moving_mask.mask.as_ref().map_or(0., |m| if m.enabled { m.feather } else { 0. }),
                density: moving_mask.mask.as_ref().map_or(1., |m| if m.enabled { m.density } else { 0. }),
            });
            // The compositor reads a resident replacement through its regular raster planner.
            layer.content = photocraft_doc::LayerContent::Raster(surface);
        }
        Some(photocraft_color::convert::with_cmyk_space(photocraft_compose::cmyk_space(&doc).as_ref(), || {
            gpu::Preview::new(renderer.render_state(), sources, doc.selection.as_ref(), canvas, &d.field)
        })?)
    } else {
        None
    };
    let source = if preview.is_none() {
        let buffer = photocraft_compose::render(&doc, canvas);
        let mut source = Surface::new(photocraft_color::PixelFormat::new(photocraft_color::ColorMode::Rgb, photocraft_color::SampleType::F32, true));
        source.write_region(canvas, &buffer.px.iter().flat_map(|p| *p).collect::<Vec<_>>());
        source
    } else {
        d.source.clone()
    };
    d.gpu_doc = preview.as_ref().map(|_| std::sync::Arc::new(doc));
    d.gpu_error = None;
    d.targets = targets;
    d.cached_scope = scope;
    d.scope_dirty = false;
    d.source = source;
    d.gpu = preview;
    if d.gpu.is_some() {
        d.field = LiquifyField::new(d.canvas, d.cell);
        d.field.gpu_only = true;
        for stroke in &d.strokes {
            d.field.apply_stroke(stroke);
        }
        if let Some((stroke, _)) = &d.cur {
            d.field.apply_stroke(stroke);
        }
    }
    d.gpu_dirty = true;
    d.proxy = if d.gpu.is_some() {
        ProxyImage::new(&Surface::new(photocraft_color::PixelFormat::RGBA32F), Rect::new(0, 0, 1, 1), 1)
    } else {
        ProxyImage::new(&d.source, canvas, PROXY_SIDE)
    };
    d.out = vec![[0; 4]; d.proxy.w * d.proxy.h];
    d.tex = None;
    d.view_tex = None;
    d.view_bounds = None;
    d.dirty = Some([0, 0, d.proxy.w, d.proxy.h]);
    if let Some(ctx) = ctx {
        d.upload(ctx);
    }
    Ok(())
}

/// Keeps the brush settings for the next time Liquify opens, also after a restart.
fn remember(app: &mut PhotocraftApp, opts: &LiquifyOpts) {
    let v = opts.to_json();
    app.session.prefs.edit(|p| p.dialogs.insert(REMEMBERED.into(), v));
}

pub(crate) fn gpu_key(app: &PhotocraftApp) -> Option<u64> {
    app.distort.liquify.as_ref()?.gpu_doc.as_ref().map(|doc| doc.id.0)
}

fn restore_canvas(app: &PhotocraftApp, dialog: &LiquifyDialog) {
    if let (Some(renderer), Some(st)) = (&app.gpu, app.session.active()) {
        if let Some(doc) = &dialog.gpu_doc {
            renderer.clear_source_overrides(doc.id.0);
        }
        renderer.clear_source_overrides(st.doc.id.0);
        renderer.refresh(st.doc.id.0, &st.doc, None, None);
    }
}

/// Cancel (button, Esc or the control channel): the document is untouched; the brush settings
/// are kept.
pub fn cancel(app: &mut PhotocraftApp) {
    if let Some(d) = app.distort.liquify.take() {
        restore_canvas(app, &d);
        remember(app, &d.opts);
    }
}

/// OK: runs `filter.liquify` with the recorded strokes (one history step).
pub fn commit(app: &mut PhotocraftApp) {
    if let Some(d) = app.distort.liquify.as_mut() {
        d.end();
    }
    if app.distort.liquify.as_ref().is_some_and(|d| d.scope_dirty)
        && app.gpu.is_some()
        && let Err(error) = refresh_scope(app, None)
    {
        crate::notices::error(app, error);
        return;
    }
    let Some(mut d) = app.distort.liquify.take() else { return };
    remember(app, &d.opts);
    if d.strokes.is_empty() {
        restore_canvas(app, &d);
        return;
    }
    if d.scope_dirty {
        match photocraft_engine::distort_cmds::liquify_targets(&app.session, &json!({"scope":d.opts.scope})) {
            Ok(targets) => {
                d.targets = targets;
                d.cached_scope = d.opts.scope.clone();
                d.scope_dirty = false;
            }
            Err(error) => {
                crate::notices::error(app, error.to_string());
                app.distort.liquify = Some(d);
                return;
            }
        }
    }
    #[cfg(not(test))]
    if d.gpu.is_none() || app.gpu.as_ref().is_none_or(|r| !r.health().is_ok()) {
        crate::notices::error(app, "Liquify requires an available GPU canvas.".into());
        app.distort.liquify = Some(d);
        return;
    }
    if let (Some(gpu), Some(renderer)) = (&d.gpu, &app.gpu) {
        let ops = std::mem::take(&mut d.field.gpu_ops);
        gpu.apply_ops(renderer.render_state(), &d.field, &ops, d.gpu_dirty);
        // Refresh tile flags after the final queued dab, then map native planes once.
        gpu.render_outputs(renderer.render_state(), None);
        let result = gpu.commit_surfaces(renderer.render_state()).and_then(|(pixels, masks)| {
            let document = app.session.active().ok_or("no document")?.doc.id;
            app.session.stage_cached_transform(photocraft_engine::cached_transform::CachedCommit { document, pixels, masks }).map_err(|e| e.to_string())
        });
        if let Err(error) = result {
            crate::notices::error(app, error);
            d.rebuild();
            app.distort.liquify = Some(d);
            return;
        }
    }
    let p = json!({"strokes":d.strokes,"meshSize":d.cell,"layer":d.layer.0,"layers":d.targets.iter().map(|id|id.0).collect::<Vec<_>>(),"scope":d.opts.scope});
    let result = app.run("filter.liquify", p);
    app.session.take_cached_transform();
    if result.is_err() {
        d.rebuild();
        app.distort.liquify = Some(d);
    } else {
        restore_canvas(app, &d);
    }
}

/// Control channel: `filter.liquify {"ui": {...}}` while the dialog is open.
pub fn control(app: &mut PhotocraftApp, ui: &Value) -> Result<Value, String> {
    if ui.get("commit").and_then(Value::as_bool) == Some(true) {
        commit(app);
        return if app.distort.liquify.is_none() { Ok(json!({"committed": true})) } else { Err("Liquify commit failed; the cached edit is still open".into()) };
    }
    if ui.get("cancel").and_then(Value::as_bool) == Some(true) {
        cancel(app);
        return Ok(json!({"cancelled": true}));
    }
    let d = app.distort.liquify.as_mut().ok_or(tl!("Liquify is not open"))?;
    let mut options = d.opts.clone();
    options.apply(ui)?;
    if options.scope != d.opts.scope {
        d.scope_dirty = true;
    }
    d.opts = options;
    let num = |k: &str| ui.get(k).and_then(Value::as_f64).map(|v| v as f32);
    let flag = |k: &str| ui.get(k).and_then(Value::as_bool);
    if flag("undo") == Some(true) {
        d.undo();
    }
    if flag("redo") == Some(true) {
        d.redo();
    }
    if flag("restoreAll") == Some(true) {
        d.restore_all();
    }
    if let Some(a) = num("reconstruct") {
        d.global(LiquifyTool::ReconstructAll, Some(f64::from(a)));
    }
    for (k, t) in [("maskAll", LiquifyTool::FreezeAll), ("maskNone", LiquifyTool::ThawAll), ("invertMask", LiquifyTool::InvertFreeze)] {
        if flag(k) == Some(true) {
            d.global(t, None);
        }
    }
    Ok(d.describe())
}

/// Photoshop's Alt (Option) variant of a Liquify brush: the twirls reverse direction and Pucker
/// and Bloat swap. Other tools are unchanged.
fn alt_tool(t: LiquifyTool) -> LiquifyTool {
    match t {
        LiquifyTool::TwirlCw => LiquifyTool::TwirlCcw,
        LiquifyTool::TwirlCcw => LiquifyTool::TwirlCw,
        LiquifyTool::Pucker => LiquifyTool::Bloat,
        LiquifyTool::Bloat => LiquifyTool::Pucker,
        t => t,
    }
}

/// Alt+right-drag, and Control+Alt-drag, change the Liquify brush instead of painting (#4702).
fn brush_resize_gesture(mods: egui::Modifiers, secondary: bool) -> bool {
    mods.alt && (secondary || mods.ctrl)
}

/// Horizontal motion sets the diameter (the circle's edge follows the pointer). Vertical motion
/// sets density, Liquify's edge falloff, over the same 200 px travel the canvas uses for hardness.
fn resized_brush(start_size: f32, start_density: f32, dx: f32, dy: f32) -> (f32, f32) {
    let dx = if dx.is_finite() { dx } else { 0.0 };
    let dy = if dy.is_finite() { dy } else { 0.0 };
    let size = (start_size + dx).round().clamp(1.0, 15000.0);
    let density = (start_density + dy * 100.0 / crate::brush_resize::HARDNESS_TRAVEL).clamp(0.0, 100.0);
    (size, (density * 10.0).round() / 10.0)
}

/// Pointer in document coordinates (from the preview or the control channel). Alt at
/// pointer-down subtracts from the freeze mask (lasso), reverses a twirl and swaps Pucker/Bloat;
/// Shift adds to the mask, same as no modifier.
pub fn pointer(app: &mut PhotocraftApp, ev: ToolEvent, mods: egui::Modifiers) {
    let now = crate::gpu_canvas::now_ms();
    let Some(d) = app.distort.liquify.as_mut() else { return };
    match ev {
        ToolEvent::Down { x, y, pressure } => {
            if d.opts.tool == LiquifyTool::LassoMask {
                d.lasso = Some((mods.alt, vec![[x, y]]));
            } else {
                d.begin([x, y, f64::from(pressure)], now, mods.alt);
            }
        }
        ToolEvent::Move { x, y, pressure } => {
            d.hover = Some([x, y]);
            if let Some((_, pts)) = &mut d.lasso {
                if pts.last().is_none_or(|&l| l[0] != x || l[1] != y) {
                    pts.push([x, y]);
                }
            } else if d.cur.is_some() {
                d.extend([x, y, f64::from(pressure)], now);
            }
        }
        ToolEvent::Up { x, y } => {
            if let Some((subtract, pts)) = d.lasso.take() {
                d.close_lasso(subtract, pts);
                return;
            }
            if let Some((_, last)) = d.cur
                && (last[0] != x || last[1] != y)
            {
                d.extend([x, y, last[2]], now);
            }
            d.end();
        }
    }
}

/// Ctrl+Z / Ctrl+Shift+Z inside Liquify, including while a slider's text field is focused.
pub fn history_keys(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let Some(d) = app.distort.liquify.as_mut() else { return };
    let cmd_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
    if ctx.input_mut(|i| i.consume_key(cmd_shift, egui::Key::Z)) {
        d.redo();
    } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z)) {
        d.undo();
    }
}

pub fn keys(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if app.distort.liquify.is_none() {
        return;
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
        commit(app);
        return;
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        cancel(app);
        return;
    }
    history_keys(app, ctx);
    let Some(d) = app.distort.liquify.as_mut() else { return };
    // Ctrl+H (Hide Extras) toggles the red freeze-mask overlay; the mask itself stays active.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::H)) {
        d.opts.show_mask = !d.opts.show_mask;
    }
    // Ctrl+I inverts the freeze mask (the Invert All button); Ctrl+D clears it (None).
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::I)) {
        d.global(LiquifyTool::InvertFreeze, None);
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::D)) {
        d.global(LiquifyTool::ThawAll, None);
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::OpenBracket)) {
        // At least one pixel, or 2 px * 0.9 rounds back to 2 and `[` never reaches 1.
        d.opts.size = (d.opts.size * 0.9).round().min(d.opts.size - 1.0).clamp(1.0, 15000.0);
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::CloseBracket)) {
        d.opts.size = (d.opts.size * 1.1).round().max(d.opts.size + 1.0).min(15000.0);
    }
    let tools = [
        (egui::Key::W, LiquifyTool::ForwardWarp),
        (egui::Key::R, LiquifyTool::Reconstruct),
        (egui::Key::E, LiquifyTool::Smooth),
        (egui::Key::C, LiquifyTool::TwirlCw),
        (egui::Key::S, LiquifyTool::Pucker),
        (egui::Key::B, LiquifyTool::Bloat),
        (egui::Key::O, LiquifyTool::PushLeft),
        (egui::Key::F, LiquifyTool::Freeze),
        (egui::Key::D, LiquifyTool::Thaw),
        (egui::Key::L, LiquifyTool::LassoMask),
    ];
    for (k, t) in tools {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k)) {
            d.opts.tool = t;
        }
    }
}

fn tool_icon(t: LiquifyTool) -> &'static str {
    match t {
        LiquifyTool::ForwardWarp => "pointer",
        LiquifyTool::Reconstruct => "brush-cleaning",
        LiquifyTool::Smooth => "droplet",
        LiquifyTool::TwirlCw => "rotate-cw",
        LiquifyTool::TwirlCcw => "undo-2",
        LiquifyTool::Pucker => "scan",
        LiquifyTool::Bloat => "maximize-2",
        LiquifyTool::PushLeft => "chevrons-left",
        LiquifyTool::Freeze => "lock",
        LiquifyTool::Thaw => "lock-open",
        LiquifyTool::LassoMask => "lasso",
        _ => "circle",
    }
}

fn shortcut(t: LiquifyTool) -> &'static str {
    match t {
        LiquifyTool::ForwardWarp => "W",
        LiquifyTool::Reconstruct => "R",
        LiquifyTool::Smooth => "E",
        LiquifyTool::TwirlCw => "C",
        LiquifyTool::Pucker => "S",
        LiquifyTool::Bloat => "B",
        LiquifyTool::PushLeft => "O",
        LiquifyTool::Freeze => "F",
        LiquifyTool::Thaw => "D",
        LiquifyTool::LassoMask => "L",
        _ => "",
    }
}

fn load_backdrop(app: &PhotocraftApp, ctx: &egui::Context, layer: LayerId, canvas: Rect) -> Option<TextureHandle> {
    let st = app.session.active()?;
    let doc = crate::distort_ui::without_layer(&st.doc, layer);
    let buf = photocraft_compose::render(&doc, canvas);
    let mut s = photocraft_raster::Surface::new(photocraft_color::PixelFormat::RGBA8);
    let flat: Vec<f32> = buf.px.iter().flat_map(|p| *p).collect();
    s.write_region(canvas, &flat);
    Some(ctx.load_texture("liquify-backdrop", crate::distort_ui::surface_image(&s, canvas, PROXY_SIDE), egui::TextureOptions::LINEAR))
}

/// Draws the dialog (a full-window layer over the app).
pub fn show(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if app.distort.liquify.as_ref().is_some_and(|d| d.scope_dirty)
        && let Err(error) = refresh_scope(app, Some(ctx))
    {
        if let Some(d) = app.distort.liquify.as_mut() {
            d.opts.scope = d.cached_scope.clone();
            d.scope_dirty = false;
        }
        crate::notices::error(app, error);
    }
    if let (Some(renderer), Some(d)) = (app.gpu.as_ref(), app.distort.liquify.as_mut())
        && let Some(gpu) = &d.gpu
    {
        let ops = std::mem::take(&mut d.field.gpu_ops);
        if !ops.is_empty() || d.gpu_dirty {
            gpu.apply_ops(renderer.render_state(), &d.field, &ops, d.gpu_dirty);
            if let Some(doc) = &d.gpu_doc {
                renderer.set_source_overrides(doc.id.0, gpu.render_outputs(renderer.render_state(), if d.gpu_dirty { None } else { d.gpu_damage.take() }));
                if let Err(error) = renderer.composite(doc, doc.bounds(), false) {
                    d.gpu_error = Some(error.0);
                }
            }
        }
        d.gpu_dirty = false;
    }
    let t = Tokens::get(ctx);
    let screen = ctx.content_rect();
    if app.distort.liquify.as_ref().is_some_and(|d| d.opts.show_backdrop && d.backdrop.is_none()) {
        let tex = app.distort.liquify.as_ref().map(|d| (d.layer, d.canvas)).and_then(|(layer, canvas)| load_backdrop(app, ctx, layer, canvas));
        if let Some(d) = app.distort.liquify.as_mut() {
            d.backdrop = tex;
        }
    }
    let mut action: Option<&str> = None;
    let mut events: Vec<(ToolEvent, egui::Modifiers)> = Vec::new();
    egui::Area::new(egui::Id::new("liquify-dialog")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
        let Some(d) = app.distort.liquify.as_mut() else { return };
        d.upload(ctx);
        let (full, _) = ui.allocate_exact_size(screen.size(), Sense::hover());
        let painter = ui.painter().clone();
        painter.rect_filled(full, 0.0, t.chrome);
        // Title strip.
        let title = ERect::from_min_size(full.min, vec2(full.width(), 30.0));
        painter.rect_filled(title, 0.0, t.dock);
        painter.line_segment([title.left_bottom(), title.right_bottom()], Stroke::new(1.0, t.separator));
        let pct = if d.zoom > 0.0 { d.zoom * 100.0 } else { 100.0 };
        painter.text(
            title.center(),
            Align2::CENTER_CENTER,
            format!("{} ({}, {:.0}%)", tl!("Liquify…").trim_end_matches('…'), d.layer_name, pct),
            FontId::proportional(13.0),
            t.text,
        );
        let body = ERect::from_min_max(pos2(full.left(), title.bottom()), full.max);
        // Left tool strip.
        let left = ERect::from_min_size(body.min, vec2(LEFT_W, body.height()));
        painter.rect_filled(left, 0.0, t.dock);
        painter.line_segment([left.right_top(), left.right_bottom()], Stroke::new(1.0, t.separator));
        let mut strip = ui.new_child(egui::UiBuilder::new().max_rect(left.shrink2(vec2(6.0, 8.0))));
        strip.spacing_mut().item_spacing.y = 4.0;
        for tool in LiquifyTool::ALL {
            let tip = match tool {
                // The lasso works on the same freeze mask as Freeze/Thaw.
                LiquifyTool::LassoMask => tl!("Freeze Lasso: drag to freeze an area, Alt-drag to thaw it (L)").to_string(),
                _ => format!("{} ({})", tl!(tool.label()), shortcut(tool)),
            };
            if crate::icons::button(&mut strip, tool_icon(tool), 34.0, d.opts.tool == tool, &tip).clicked() {
                d.opts.tool = tool;
            }
            if matches!(tool, LiquifyTool::Smooth | LiquifyTool::PushLeft | LiquifyTool::Thaw) {
                strip.add_space(6.0);
            }
        }
        // Right properties panel.
        let right = ERect::from_min_size(pos2(body.right() - RIGHT_W, body.top()), vec2(RIGHT_W, body.height()));
        painter.rect_filled(right, 0.0, t.dock);
        painter.line_segment([right.left_top(), right.left_bottom()], Stroke::new(1.0, t.separator));
        let mut props = ui.new_child(egui::UiBuilder::new().max_rect(right.shrink2(vec2(14.0, 12.0))));
        egui::ScrollArea::vertical().id_salt("liquify-props").show(&mut props, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            if let Some(error) = &d.gpu_error {
                ui.colored_label(t.danger, error);
            }
            let old_scope = d.opts.scope.clone();
            egui::ComboBox::from_id_salt("liquify-target-scope").selected_text(scope_label(&d.opts.scope)).show_ui(ui, |ui| {
                for scope in ["layer", "all", "group"] {
                    ui.selectable_value(&mut d.opts.scope, scope.into(), scope_label(scope));
                }
            });
            if old_scope != d.opts.scope {
                d.end();
                d.scope_dirty = true;
            }
            widgets::section_label(ui, tl!("Brush Tool Options"));
            widgets::slider_row(ui, tl!("Size"), &mut d.opts.size, 1.0..=3000.0, "", None);
            widgets::slider_row(ui, tl!("Density"), &mut d.opts.density, 0.0..=100.0, "", None);
            widgets::slider_row(ui, tl!("Pressure"), &mut d.opts.pressure, 0.0..=100.0, "", None);
            widgets::slider_row(ui, tl!("Rate"), &mut d.opts.rate, 0.0..=100.0, "", None);
            widgets::hairline(ui);
            widgets::section_label(ui, tl!("Brush Reconstruct Options"));
            widgets::slider_row(ui, tl!("Amount"), &mut d.opts.reconstruct_amount, 0.0..=100.0, "%", None);
            ui.horizontal(|ui| {
                if widgets::secondary_button(ui, tl!("Reconstruct"), 110.0).clicked() {
                    let a = f64::from(d.opts.reconstruct_amount);
                    d.global(LiquifyTool::ReconstructAll, Some(a));
                }
                if widgets::secondary_button(ui, tl!("Restore All"), 110.0).clicked() {
                    d.restore_all();
                }
            });
            widgets::hairline(ui);
            widgets::section_label(ui, tl!("Mask Options"));
            ui.horizontal(|ui| {
                if widgets::secondary_button(ui, tl!("None"), 70.0).clicked() {
                    d.global(LiquifyTool::ThawAll, None);
                }
                if widgets::secondary_button(ui, tl!("Mask All"), 80.0).clicked() {
                    d.global(LiquifyTool::FreezeAll, None);
                }
                if widgets::secondary_button(ui, tl!("Invert All"), 80.0).clicked() {
                    d.global(LiquifyTool::InvertFreeze, None);
                }
            });
            widgets::hairline(ui);
            widgets::section_label(ui, tl!("View Options"));
            widgets::checkbox(ui, &mut d.opts.show_mesh, tl!("Show Mesh"));
            ui.horizontal(|ui| {
                ui.label(tl!("Mesh Size"));
                widgets::dropdown(ui, "liquify-mesh-size", &mut d.opts.mesh_size, &[(0, tl!("Small")), (1, tl!("Medium")), (2, tl!("Large"))], 110.0);
            });
            widgets::checkbox(ui, &mut d.opts.show_mask, tl!("Show Mask"));
            widgets::checkbox(ui, &mut d.opts.show_backdrop, tl!("Show Backdrop"));
            if d.opts.show_backdrop {
                widgets::slider_row(ui, tl!("Opacity"), &mut d.opts.backdrop_opacity, 0.0..=100.0, "", None);
            }
            widgets::hairline(ui);
            ui.label(
                egui::RichText::new(format!(
                    "{} stroke(s) · field {} px/node · dab {:.2} ms · preview {:.2} ms",
                    d.strokes.len(),
                    d.cell,
                    d.dab_ms,
                    d.preview_ms
                ))
                .color(t.text_faint)
                .size(11.0),
            );
        });
        // Footer buttons.
        let foot = ERect::from_min_size(pos2(right.left() + 14.0, right.bottom() - 48.0), vec2(RIGHT_W - 28.0, 32.0));
        let mut fb = ui.new_child(egui::UiBuilder::new().max_rect(foot).layout(egui::Layout::right_to_left(egui::Align::Center)));
        if let Some(role) = widgets::dialog_buttons(
            &mut fb,
            &[
                widgets::DialogButton::new(widgets::ButtonRole::Default, tl!("OK"), 84.0),
                widgets::DialogButton::new(widgets::ButtonRole::Cancel, tl!("Cancel"), 84.0),
            ],
        ) {
            action = Some(if role == widgets::ButtonRole::Default { "ok" } else { "cancel" });
        }
        // Preview.
        let area = ERect::from_min_max(pos2(left.right(), body.top()), pos2(right.left(), body.bottom()));
        painter.rect_filled(area, 0.0, t.canvas);
        let (cw, ch) = (d.canvas.width() as f32, d.canvas.height() as f32);
        if d.zoom <= 0.0 {
            d.zoom = ((area.width() - 40.0) / cw).min((area.height() - 40.0) / ch).clamp(0.01, 4.0);
            d.center = [f64::from(d.canvas.x0) + f64::from(cw) / 2.0, f64::from(d.canvas.y0) + f64::from(ch) / 2.0];
        }
        let resp = ui.interact(area, egui::Id::new("liquify-preview"), Sense::click_and_drag());
        // Wheel zooms about the pointer.
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0
                && let Some(hp) = resp.hover_pos()
            {
                let before = to_doc(d, area, hp);
                // Liquify's own preview zoom, not the canvas (`zoom_levels` doesn't apply).
                d.zoom = (d.zoom * (scroll / 200.0).exp()).clamp(0.01, 32.0);
                let after = to_doc(d, area, hp);
                d.center = [d.center[0] + before[0] - after[0], d.center[1] + before[1] - after[1]];
            }
        }
        d.update_view_preview(ctx, area);
        let img = ERect::from_min_max(
            to_screen(d, area, [f64::from(d.canvas.x0), f64::from(d.canvas.y0)]),
            to_screen(d, area, [f64::from(d.canvas.x1), f64::from(d.canvas.y1)]),
        );
        let clip = painter.with_clip_rect(area);
        widgets::checker(&clip, img.intersect(area), 8.0);
        let uv = ERect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        if d.opts.show_backdrop
            && let Some(b) = &d.backdrop
        {
            clip.image(b.id(), img, uv, Color32::from_white_alpha((d.opts.backdrop_opacity / 100.0 * 255.0) as u8));
        }
        if let Some(gpu) = &d.gpu {
            if let Some(doc) = &d.gpu_doc {
                crate::gpu_canvas::GpuCanvas::paint(
                    &clip,
                    area,
                    crate::gpu_canvas::ViewParams {
                        doc: doc.id.0,
                        doc_size: [doc.size.width, doc.size.height],
                        zoom: d.zoom,
                        center: [d.center[0] as f32, d.center[1] as f32],
                        rotation: 0.0,
                        shadow: false,
                        pixel_grid: false,
                        view_key: 0x4c495155494659,
                        display: 0,
                        output: 0,
                        hdr: None,
                        offset: [0.0; 2],
                    },
                );
            }
            clip.add(gpu.callback(area, img, d.canvas, &d.field, d.opts.show_mask, d.opts.show_mesh, d.opts.mesh_size));
        } else if d.zoom > 1.0
            && let (Some(bounds), Some(tex)) = (d.view_bounds, &d.view_tex)
        {
            let detail = ERect::from_min_max(
                to_screen(d, area, [f64::from(bounds.x0), f64::from(bounds.y0)]),
                to_screen(d, area, [f64::from(bounds.x1), f64::from(bounds.y1)]),
            );
            clip.image(tex.id(), detail, uv, Color32::WHITE);
        } else {
            // The proxy covers whole proxy pixels, a little past the canvas edge.
            let pr = ERect::from_min_max(
                img.min,
                img.min + vec2(d.proxy.w as f32 * d.proxy.scale as f32 * d.zoom, d.proxy.h as f32 * d.proxy.scale as f32 * d.zoom),
            );
            if let Some(tex) = &d.tex {
                clip.image(tex.id(), pr, uv, Color32::WHITE);
            }
        }
        if d.gpu.is_none()
            && d.opts.show_mask
            && let Some(m) = &d.mask_tex
        {
            let f = &d.field;
            let mr = ERect::from_min_size(
                img.min - vec2(0.5, 0.5) * d.zoom * f.cell as f32 + vec2(0.5, 0.5) * d.zoom,
                vec2(f.w as f32, f.h as f32) * f.cell as f32 * d.zoom,
            );
            clip.image(m.id(), mr, uv, Color32::WHITE);
        }
        if d.gpu.is_none() && d.opts.show_mesh {
            draw_mesh(d, &clip, area);
        }
        // The lasso polygon being drawn (danger colour freezes, accent thaws; the mask overlay paints the
        // applied area red like the Freeze brush).
        if let Some((subtract, pts)) = &d.lasso
            && pts.len() > 1
        {
            let color = if *subtract { t.accent } else { t.danger };
            let line: Vec<Pos2> = pts.iter().map(|p| to_screen(d, area, *p)).collect();
            clip.add(egui::Shape::line(line, Stroke::new(1.0, color)));
        }
        // Brush outline (the lasso has none).
        if let Some(hp) = resp.hover_pos() {
            if d.opts.tool != LiquifyTool::LassoMask {
                let r = d.opts.size * 0.5 * d.zoom;
                clip.circle_stroke(hp, r, Stroke::new(1.5, Color32::BLACK));
                clip.circle_stroke(hp, r, Stroke::new(0.75, Color32::WHITE));
            }
            d.hover = Some(to_doc(d, area, hp));
        }
        clip.rect_stroke(img, 0.0, Stroke::new(1.0, t.separator), egui::StrokeKind::Outside);
        // Mouse → strokes (Space or middle button pans). Alt+right-drag resizes the brush (#4702).
        let (pan, secondary, mods_now) =
            ui.input(|i| (i.key_down(egui::Key::Space) || i.pointer.middle_down(), i.pointer.button_down(egui::PointerButton::Secondary), i.modifiers));
        let resize = d.brush_resize.is_some() || (resp.drag_started() && brush_resize_gesture(mods_now, secondary));
        if resize {
            if d.brush_resize.is_none()
                && let Some(p) = resp.interact_pointer_pos()
            {
                d.brush_resize = Some((p, d.opts.size, d.opts.density));
            }
            if let (Some((anchor, size0, dens0)), Some(p)) = (d.brush_resize, resp.interact_pointer_pos()) {
                let delta = p - anchor;
                let (size, density) = resized_brush(size0, dens0, delta.x, delta.y);
                d.opts.size = size;
                d.opts.density = density;
            }
            if resp.drag_stopped() || !ui.input(|i| i.pointer.any_down()) {
                d.brush_resize = None;
            }
        } else if pan && resp.dragged() {
            let dl = resp.drag_delta();
            d.center = [d.center[0] - f64::from(dl.x / d.zoom), d.center[1] - f64::from(dl.y / d.zoom)];
        } else if !pan {
            // Replay the original sequence: a whole drag can arrive in one frame,
            // where Response's aggregate drag flags no longer retain the press.
            let mut owned = d.cur.is_some() || d.lasso.is_some();
            let mut last = d.cur.as_ref().map(|(_, p)| [p[0], p[1]]).or_else(|| d.lasso.as_ref().and_then(|(_, p)| p.last().copied()));
            let mut sampled = false;
            for event in ui.input(|i| i.events.clone()) {
                match event {
                    egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers }
                        if !owned && area.contains(pos) && ctx.layer_id_at(pos) == Some(resp.layer_id) && !egui::Popup::is_any_open(ctx) =>
                    {
                        let q = to_doc(d, area, pos);
                        events.push((ToolEvent::Down { x: q[0], y: q[1], pressure: 1.0 }, modifiers));
                        owned = true;
                        last = Some(q);
                        sampled = true;
                    }
                    egui::Event::PointerMoved(pos) if owned => {
                        let q = to_doc(d, area, pos);
                        if last != Some(q) {
                            events.push((ToolEvent::Move { x: q[0], y: q[1], pressure: 1.0 }, mods_now));
                            last = Some(q);
                            sampled = true;
                        }
                    }
                    egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers } if owned => {
                        let q = to_doc(d, area, pos);
                        events.push((ToolEvent::Up { x: q[0], y: q[1] }, modifiers));
                        owned = false;
                        sampled = true;
                    }
                    _ => {}
                }
            }
            if owned {
                if !ui.input(|i| i.pointer.primary_down()) {
                    if let Some(q) = last {
                        events.push((ToolEvent::Up { x: q[0], y: q[1] }, mods_now));
                    }
                } else {
                    // Stationary tools continue at 30 dabs/s without duplicating
                    // any movement already replayed from this input frame.
                    if !sampled
                        && d.opts.tool.is_stationary()
                        && crate::gpu_canvas::now_ms() - d.last_dab > 33.0
                        && let Some(q) = last
                    {
                        events.push((ToolEvent::Move { x: q[0], y: q[1], pressure: 1.0 }, mods_now));
                    }
                    ctx.request_repaint();
                }
            }
        }
    });
    let changed = !events.is_empty();
    for (ev, mods) in events {
        pointer(app, ev, mods);
    }
    // Pointer events are collected after this frame's GPU sampling. Always present their
    // result next frame, including a stationary dab and the final movement on release.
    if changed {
        ctx.request_repaint();
    }
    match action {
        Some("ok") => commit(app),
        Some("cancel") => cancel(app),
        _ => {}
    }
}

fn to_screen(d: &LiquifyDialog, area: ERect, p: [f64; 2]) -> Pos2 {
    area.center() + vec2(((p[0] - d.center[0]) as f32) * d.zoom, ((p[1] - d.center[1]) as f32) * d.zoom)
}

fn to_doc(d: &LiquifyDialog, area: ERect, p: Pos2) -> [f64; 2] {
    let v = (p - area.center()) / d.zoom;
    [d.center[0] + f64::from(v.x), d.center[1] + f64::from(v.y)]
}

/// The deformed mesh: where the source grid lines land. Each output sample knows its source
/// position `p + d(p)`, so a source line `x = c` is the iso-line of that function, traced with
/// marching squares on a sample grid four times finer than the mesh (exact for any field, unlike
/// inverting the field point by point, which fails where it folds).
fn draw_mesh(d: &LiquifyDialog, painter: &egui::Painter, area: ERect) {
    let c = d.canvas;
    let cells = [64.0, 40.0, 24.0][d.opts.mesh_size.min(2)];
    let step = f64::from(c.width().max(c.height())) / cells;
    let h = step / 4.0;
    let nx = (f64::from(c.width()) / h).ceil() as usize + 1;
    let ny = (f64::from(c.height()) / h).ceil() as usize + 1;
    let (x0, y0) = (f64::from(c.x0), f64::from(c.y0));
    let mut src = Vec::with_capacity(nx * ny);
    for j in 0..ny {
        for i in 0..nx {
            let p = [x0 + i as f64 * h, y0 + j as f64 * h];
            let v = d.field.sample(p[0], p[1]);
            src.push([p[0] + v[0], p[1] + v[1]]);
        }
    }
    let stroke = Stroke::new(0.75, Color32::from_rgba_unmultiplied(90, 90, 90, 210));
    let pos = |i: usize, j: usize| [x0 + i as f64 * h, y0 + j as f64 * h];
    for axis in [0usize, 1] {
        let origin = if axis == 0 { x0 } else { y0 };
        for jj in 0..ny - 1 {
            for ii in 0..nx - 1 {
                let corners = [(ii, jj), (ii + 1, jj), (ii + 1, jj + 1), (ii, jj + 1)];
                let vals = corners.map(|(i, j)| src[j * nx + i][axis]);
                let (lo, hi) = (vals.iter().copied().fold(f64::MAX, f64::min), vals.iter().copied().fold(f64::MIN, f64::max));
                let mut k = ((lo - origin) / step).ceil() as i64;
                while origin + k as f64 * step <= hi {
                    let level = origin + k as f64 * step;
                    let mut pts: Vec<[f64; 2]> = Vec::with_capacity(4);
                    for e in 0..4 {
                        let (a, b) = (vals[e] - level, vals[(e + 1) % 4] - level);
                        if (a <= 0.0) != (b <= 0.0) {
                            let t = a / (a - b);
                            let (pa, pb) = (pos(corners[e].0, corners[e].1), pos(corners[(e + 1) % 4].0, corners[(e + 1) % 4].1));
                            pts.push([pa[0] + (pb[0] - pa[0]) * t, pa[1] + (pb[1] - pa[1]) * t]);
                        }
                    }
                    for seg in pts.as_chunks::<2>().0 {
                        painter.line_segment([to_screen(d, area, seg[0]), to_screen(d, area, seg[1])], stroke);
                    }
                    k += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alt_right_drag_resizes_the_liquify_brush_and_does_not_paint() {
        assert!(brush_resize_gesture(egui::Modifiers::ALT, true));
        assert!(brush_resize_gesture(egui::Modifiers::CTRL | egui::Modifiers::ALT, false));
        assert!(!brush_resize_gesture(egui::Modifiers::ALT, false), "Alt+left stays the tool's opposite");
        assert!(!brush_resize_gesture(egui::Modifiers::NONE, true));
        let (size, density) = resized_brush(40.0, 50.0, 25.0, crate::brush_resize::HARDNESS_TRAVEL);
        assert_eq!((size, density), (65.0, 100.0));
        let (size, density) = resized_brush(40.0, 50.0, -100.0, 0.0);
        assert_eq!((size, density), (1.0, 50.0));
    }

    fn app_with_layer() -> PhotocraftApp {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        app.session.execute("file.new", json!({"width": 120, "height": 80, "depth": 8})).unwrap();
        app.session.execute("layer.new.layer", json!({})).unwrap();
        app.session
            .edit("paint", |doc, active| {
                let s = doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap();
                for y in 10..70 {
                    for x in 10..110 {
                        let v = ((x / 5 + y / 5) % 2) as f32;
                        s.write_pixel(x, y, &[v, 0.3, 1.0 - v, 1.0]);
                    }
                }
                Ok(())
            })
            .unwrap();
        app.sync_views();
        app
    }

    #[test]
    fn one_move_mouse_drag_keeps_the_press_point_and_schedules_gpu_dabs() {
        short_drag_input(false);
    }

    #[test]
    fn one_frame_mouse_drag_keeps_the_press_point_and_schedules_gpu_dabs() {
        short_drag_input(true);
    }

    fn short_drag_input(one_frame: bool) {
        let ctx = egui::Context::default();
        PhotocraftApp::setup_context(&ctx, crate::theme::ThemeKind::ALL[0]);
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        control(&mut app, &json!({"tool":"forwardWarp","size":40,"pressure":100})).unwrap();
        app.distort.liquify.as_mut().unwrap().field.gpu_only = true;
        let screen = ERect::from_min_size(Pos2::ZERO, vec2(800., 600.));
        let frame = |app: &mut PhotocraftApp, events| {
            let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ui| show(app, ui.ctx()));
            output.textures_delta.clear();
        };
        // Let egui register the dialog and its input rectangle before the press.
        frame(&mut app, vec![]);
        frame(&mut app, vec![]);
        let area = ERect::from_min_max(pos2(LEFT_W, 30.), pos2(screen.right() - RIGHT_W, screen.bottom()));
        let a = area.center();
        let b = a + vec2(24., 0.);
        let (start, end) = {
            let d = app.distort.liquify.as_ref().unwrap();
            (to_doc(d, area, a), to_doc(d, area, b))
        };
        let down = egui::Event::PointerButton { pos: a, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE };
        let up = egui::Event::PointerButton { pos: b, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE };
        if one_frame {
            frame(&mut app, vec![egui::Event::PointerMoved(a), down, egui::Event::PointerMoved(b), up.clone()]);
        } else {
            frame(&mut app, vec![egui::Event::PointerMoved(a), down]);
            frame(&mut app, vec![egui::Event::PointerMoved(b)]);
        }
        let d = app.distort.liquify.as_ref().unwrap();
        assert!(!d.field.gpu_ops.is_empty(), "the sole movement must deform the resident field");
        assert!(d.field.gpu_ops.iter().any(|op| op.delta[0] > 0.), "the movement must survive crossing egui's drag threshold");
        if !one_frame {
            frame(&mut app, vec![up]);
        }
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.strokes.len(), 1);
        let points = &d.strokes[0].points;
        assert_eq!(&points.first().unwrap()[..2], &start);
        assert_eq!(&points.last().unwrap()[..2], &end);
        assert!(
            LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes).max_displacement() > 0.,
            "the recorded short drag must also replay to an actual deformation"
        );
    }

    #[test]
    fn zoomed_liquify_preview_uses_only_the_visible_full_resolution_crop() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        let d = app.distort.liquify.as_mut().unwrap();
        d.zoom = 2.0;
        d.center = [60.0, 40.0];
        d.update_view_preview(&ctx, ERect::from_min_size(pos2(0.0, 0.0), vec2(60.0, 40.0)));
        assert_eq!(d.view_bounds, Some(Rect::new(45, 30, 75, 50)));
        assert_eq!(d.view_out.len(), 30 * 20, "one preview texel per document pixel");
        assert!(d.view_tex.is_some());

        let before = d.view_out.clone();
        let mut stroke = LiquifyStroke::new(LiquifyTool::ForwardWarp, 24.0);
        stroke.points = vec![vec![50.0, 40.0, 1.0], vec![58.0, 40.0, 1.0]];
        let dirty = d.field.stroke_segment(&stroke, [50.0, 40.0, 1.0], [58.0, 40.0, 1.0]);
        d.mark(dirty, false);
        d.render_dirty();
        d.update_view_preview(&ctx, ERect::from_min_size(pos2(0.0, 0.0), vec2(60.0, 40.0)));
        assert_ne!(d.view_out, before, "a brush update refreshes the visible source-resolution crop");
        assert!(d.preview_ms.is_finite());
    }

    /// #418: the brush settings survive Cancel, OK and Esc, and come back the next time Liquify
    /// opens (also after a restart: they live in the preferences).
    #[test]
    fn brush_settings_are_remembered_between_uses() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        let fresh = app.distort.liquify.as_ref().unwrap().opts.clone();
        assert_eq!((fresh.density, fresh.tool), (50.0, LiquifyTool::ForwardWarp), "first use: the defaults");
        let set = json!({"tool": "twirlCw", "size": 37, "density": 85, "pressure": 60, "rate": 30, "showMesh": true, "meshSize": "large", "showBackdrop": true, "backdropOpacity": 25});
        control(&mut app, &set).unwrap();
        control(&mut app, &json!({"cancel": true})).unwrap();
        assert!(app.distort.liquify.is_none());
        open(&mut app, &ctx).unwrap();
        assert_eq!(app.distort.liquify.as_ref().unwrap().opts.to_json(), {
            let mut o = LiquifyOpts::default();
            o.apply(&set).unwrap();
            o.to_json()
        });
        // OK and Esc keep the latest settings too.
        control(&mut app, &json!({"density": 90})).unwrap();
        commit(&mut app);
        open(&mut app, &ctx).unwrap();
        assert_eq!(app.distort.liquify.as_ref().unwrap().opts.density, 90.0);
        control(&mut app, &json!({"density": 70})).unwrap();
        cancel(&mut app);
        assert_eq!(app.session.prefs().dialogs[REMEMBERED]["density"], json!(70.0));
        // A damaged preference keeps what it can and never fails to open.
        app.session.prefs.edit(|p| p.dialogs.insert(REMEMBERED.into(), json!({"tool": "nope", "density": "lots", "size": 1e12, "rate": 20})));
        open(&mut app, &ctx).unwrap();
        let o = &app.distort.liquify.as_ref().unwrap().opts;
        assert_eq!((o.tool, o.density, o.size, o.rate), (LiquifyTool::ForwardWarp, 50.0, 15000.0, 20.0));
        app.session.prefs.edit(|p| p.dialogs.insert(REMEMBERED.into(), json!("not an object")));
        cancel(&mut app);
        assert!(open(&mut app, &ctx).is_ok());
    }

    /// Follow-up to #1494 / #1484: Alt is captured at stroke-down and stays recorded
    /// through undo, redo and deterministic Liquify field reconstruction.
    #[test]
    fn alt_reversed_twirl_survives_replay_undo_and_redo() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        control(&mut app, &json!({"tool": "twirlCw", "size": 36, "rate": 80})).unwrap();

        // Releasing Alt while dragging must not change the tool recorded at pointer-down.
        pointer(&mut app, ToolEvent::Down { x: 53.0, y: 40.0, pressure: 1.0 }, egui::Modifiers::ALT);
        pointer(&mut app, ToolEvent::Move { x: 58.0, y: 41.0, pressure: 1.0 }, egui::Modifiers::NONE);
        pointer(&mut app, ToolEvent::Up { x: 58.0, y: 41.0 }, egui::Modifiers::NONE);
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.opts.tool, LiquifyTool::TwirlCw, "Alt must not change the selected tool");
        assert_eq!(d.strokes.len(), 1);
        assert_eq!(d.strokes[0].tool, LiquifyTool::TwirlCcw);
        assert_eq!(LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes), d.field);

        // A following stroke without Alt records the original tool.
        pointer(&mut app, ToolEvent::Down { x: 70.0, y: 40.0, pressure: 1.0 }, egui::Modifiers::NONE);
        pointer(&mut app, ToolEvent::Move { x: 75.0, y: 42.0, pressure: 1.0 }, egui::Modifiers::ALT);
        pointer(&mut app, ToolEvent::Up { x: 75.0, y: 42.0 }, egui::Modifiers::ALT);
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.strokes.len(), 2);
        assert_eq!(d.strokes[1].tool, LiquifyTool::TwirlCw);
        assert_eq!(LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes), d.field);

        control(&mut app, &json!({"undo": true})).unwrap();
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.strokes.len(), 1);
        assert_eq!(d.strokes[0].tool, LiquifyTool::TwirlCcw);
        assert_eq!(d.redo[0].tool, LiquifyTool::TwirlCw);
        assert_eq!(LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes), d.field);

        control(&mut app, &json!({"redo": true})).unwrap();
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.strokes.len(), 2);
        assert!(d.redo.is_empty());
        assert_eq!(d.strokes[0].tool, LiquifyTool::TwirlCcw);
        assert_eq!(d.strokes[1].tool, LiquifyTool::TwirlCw);
        assert_eq!(LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes), d.field);
    }

    #[test]
    fn dialog_strokes_match_the_engine_and_commit_once() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        control(&mut app, &json!({"tool": "forwardWarp", "size": 40})).unwrap();
        for ev in [
            ToolEvent::Down { x: 40.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Move { x: 48.0, y: 41.0, pressure: 1.0 },
            ToolEvent::Move { x: 56.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Up { x: 56.0, y: 40.0 },
        ] {
            pointer(&mut app, ev, egui::Modifiers::NONE);
        }
        control(&mut app, &json!({"tool": "bloat"})).unwrap();
        for ev in [ToolEvent::Down { x: 80.0, y: 40.0, pressure: 1.0 }, ToolEvent::Move { x: 80.0, y: 40.0, pressure: 1.0 }, ToolEvent::Up { x: 80.0, y: 40.0 }]
        {
            pointer(&mut app, ev, egui::Modifiers::NONE);
        }
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.strokes.len(), 2);
        assert_eq!(d.redo.len(), 0);
        // Replaying the recorded strokes gives exactly the interactive field.
        let replay = LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes);
        assert_eq!(replay, d.field);
        assert!(d.field.max_displacement() > 3.0);
        // Undo inside the dialog drops the last stroke.
        control(&mut app, &json!({"undo": true})).unwrap();
        assert_eq!(app.distort.liquify.as_ref().unwrap().strokes.len(), 1);
        assert_eq!(app.distort.liquify.as_ref().unwrap().redo.len(), 1);
        control(&mut app, &json!({"redo": true})).unwrap();
        assert_eq!(app.distort.liquify.as_ref().unwrap().strokes.len(), 2);
        assert_eq!(app.distort.liquify.as_ref().unwrap().redo.len(), 0);
        control(&mut app, &json!({"undo": true})).unwrap();
        let before = app.session.active().unwrap().history.past_len();
        control(&mut app, &json!({"commit": true})).unwrap();
        assert!(app.distort.liquify.is_none());
        assert_eq!(app.session.active().unwrap().history.past_len(), before + 1);
    }

    #[test]
    fn undo_rerenders_the_proxy_before_the_next_texture_upload() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        control(&mut app, &json!({"tool": "forwardWarp", "size": 40})).unwrap();
        for ev in [
            ToolEvent::Down { x: 35.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Move { x: 55.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Up { x: 55.0, y: 40.0 },
            ToolEvent::Down { x: 70.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Move { x: 90.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Up { x: 90.0, y: 40.0 },
        ] {
            pointer(&mut app, ev, egui::Modifiers::NONE);
        }

        let before = app.distort.liquify.as_ref().unwrap().out.clone();
        control(&mut app, &json!({"undo": true})).unwrap();
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.strokes.len(), 1);
        assert_ne!(d.out, before, "undo must update the pixels shown by the existing texture");

        let mut expected = vec![[0u8; 4]; d.proxy.w * d.proxy.h];
        d.proxy.render(&d.field, [0, 0, d.proxy.w, d.proxy.h], &mut expected);
        assert_eq!(d.out, expected, "dirty upload source must match the rebuilt field");
    }

    #[test]
    fn undo_dirties_the_zoomed_preview() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        control(&mut app, &json!({"tool": "forwardWarp", "size": 40})).unwrap();
        for ev in [ToolEvent::Down { x: 35.0, y: 40.0, pressure: 1.0 }, ToolEvent::Move { x: 55.0, y: 40.0, pressure: 1.0 }, ToolEvent::Up { x: 55.0, y: 40.0 }]
        {
            pointer(&mut app, ev, egui::Modifiers::NONE);
        }
        {
            let d = app.distort.liquify.as_mut().unwrap();
            d.zoom = 2.0;
            d.view_bounds = Some(Rect::new(0, 0, 20, 20));
            d.view_dirty = None;
        }
        control(&mut app, &json!({"undo": true})).unwrap();
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.view_dirty, d.view_bounds);
    }

    #[test]
    fn liquify_redo_restores_undone_stroke_and_a_new_stroke_clears_redo() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        control(&mut app, &json!({"tool": "forwardWarp", "size": 40})).unwrap();
        let draw = |app: &mut PhotocraftApp, x: f64| {
            for ev in
                [ToolEvent::Down { x, y: 40.0, pressure: 1.0 }, ToolEvent::Move { x: x + 14.0, y: 40.0, pressure: 1.0 }, ToolEvent::Up { x: x + 14.0, y: 40.0 }]
            {
                pointer(app, ev, egui::Modifiers::NONE);
            }
        };

        draw(&mut app, 30.0);
        draw(&mut app, 70.0);
        control(&mut app, &json!({"undo": true})).unwrap();
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!((d.strokes.len(), d.redo.len()), (1, 1));

        control(&mut app, &json!({"redo": true})).unwrap();
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!((d.strokes.len(), d.redo.len()), (2, 0));
        assert_eq!(LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes), d.field);

        control(&mut app, &json!({"undo": true})).unwrap();
        draw(&mut app, 50.0);
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!((d.strokes.len(), d.redo.len()), (2, 0));
        control(&mut app, &json!({"redo": true})).unwrap();
        assert_eq!(app.distort.liquify.as_ref().unwrap().strokes.len(), 2, "redo after a new stroke is a no-op");
    }

    /// The Freeze Lasso freezes the dragged polygon (⌥ thaws it) as one recorded stroke, and a
    /// new lasso clears Redo like any other stroke.
    #[test]
    fn freeze_lasso_records_a_stroke_and_clears_redo() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        control(&mut app, &json!({"tool": "lassoMask"})).unwrap();
        let lasso = |app: &mut PhotocraftApp, m: egui::Modifiers, r: [f64; 4]| {
            pointer(app, ToolEvent::Down { x: r[0], y: r[1], pressure: 1.0 }, m);
            for (x, y) in [(r[2], r[1]), (r[2], r[3]), (r[0], r[3])] {
                pointer(app, ToolEvent::Move { x, y, pressure: 1.0 }, m);
            }
            pointer(app, ToolEvent::Up { x: r[0], y: r[3] }, m);
        };
        lasso(&mut app, egui::Modifiers::NONE, [20.0, 20.0, 80.0, 60.0]);
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.strokes.len(), 1);
        assert_eq!(d.field.freeze_at(50.0, 40.0), 1.0, "frozen inside");
        assert_eq!(d.field.freeze_at(5.0, 5.0), 0.0, "not outside");
        lasso(&mut app, egui::Modifiers::ALT, [40.0, 30.0, 60.0, 50.0]);
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.field.freeze_at(50.0, 40.0), 0.0, "⌥ thaws");
        assert_eq!(d.field.freeze_at(25.0, 25.0), 1.0);
        control(&mut app, &json!({"undo": true})).unwrap();
        assert_eq!(app.distort.liquify.as_ref().unwrap().field.freeze_at(50.0, 40.0), 1.0, "undo replays the first lasso");
        assert_eq!(app.distort.liquify.as_ref().unwrap().redo.len(), 1);
        lasso(&mut app, egui::Modifiers::NONE, [0.0, 0.0, 10.0, 10.0]);
        assert_eq!(app.distort.liquify.as_ref().unwrap().redo.len(), 0, "a new lasso clears redo");
    }

    #[test]
    fn alt_reverses_twirl_and_swaps_pucker_and_bloat() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        let dab = |app: &mut PhotocraftApp, m: egui::Modifiers| {
            pointer(app, ToolEvent::Down { x: 50.0, y: 40.0, pressure: 1.0 }, m);
            pointer(app, ToolEvent::Up { x: 50.0, y: 40.0 }, m);
            app.distort.liquify.as_ref().unwrap().strokes.last().unwrap().tool
        };
        for (tool, plain, alt) in [
            ("twirlCw", LiquifyTool::TwirlCw, LiquifyTool::TwirlCcw),
            ("twirlCcw", LiquifyTool::TwirlCcw, LiquifyTool::TwirlCw),
            ("pucker", LiquifyTool::Pucker, LiquifyTool::Bloat),
            ("bloat", LiquifyTool::Bloat, LiquifyTool::Pucker),
            ("forwardWarp", LiquifyTool::ForwardWarp, LiquifyTool::ForwardWarp),
        ] {
            control(&mut app, &json!({"tool": tool})).unwrap();
            assert_eq!(dab(&mut app, egui::Modifiers::NONE), plain, "{tool}");
            assert_eq!(dab(&mut app, egui::Modifiers::ALT), alt, "⌥ {tool}");
            assert_eq!(app.distort.liquify.as_ref().unwrap().opts.tool, plain, "⌥ leaves the selected tool alone");
        }
    }

    #[test]
    fn liquify_redo_stack_is_bounded() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        let d = app.distort.liquify.as_mut().unwrap();
        d.strokes = (0..=REDO_STACK_LIMIT).map(|_| LiquifyStroke::new(LiquifyTool::ForwardWarp, 1.0)).collect();
        for _ in 0..=REDO_STACK_LIMIT {
            d.undo();
        }
        assert_eq!(d.redo.len(), REDO_STACK_LIMIT);
    }

    #[test]
    fn shortcut_handler_undoes_liquify_even_when_egui_owns_keyboard_focus() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        for ev in [ToolEvent::Down { x: 40.0, y: 40.0, pressure: 1.0 }, ToolEvent::Move { x: 52.0, y: 40.0, pressure: 1.0 }, ToolEvent::Up { x: 52.0, y: 40.0 }]
        {
            pointer(&mut app, ev, egui::Modifiers::NONE);
        }
        assert_eq!(app.distort.liquify.as_ref().unwrap().strokes.len(), 1);

        let raw = egui::RawInput {
            events: vec![egui::Event::Key { key: egui::Key::Z, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND }],
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            let ctx = ui.ctx();
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("liquify-slider-focus")));
            assert!(ctx.egui_wants_keyboard_input());
            crate::shortcuts::handle(&mut app, ctx);
        });
        out.textures_delta.clear();
        assert_eq!(app.distort.liquify.as_ref().unwrap().strokes.len(), 0);
        let raw = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Z,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            }],
            ..Default::default()
        };
        let _ = ctx.run_ui(raw, |ui| {
            crate::shortcuts::handle(&mut app, ui.ctx());
        });
        assert_eq!(app.distort.liquify.as_ref().unwrap().strokes.len(), 1, "Cmd+Shift+Z redoes the last stroke");
    }

    #[test]
    fn menu_opens_the_dialog_and_routes_pointer_events() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        let r = crate::distort_ui::menu(&mut app, &ctx, "filter.liquify", &json!({})).unwrap().unwrap();
        assert!(r["liquify"]["proxy"].is_array());
        assert!(crate::distort_ui::pointer(&mut app, ToolEvent::Down { x: 50.0, y: 40.0, pressure: 1.0 }, egui::Modifiers::NONE));
        crate::distort_ui::pointer(&mut app, ToolEvent::Up { x: 60.0, y: 40.0 }, egui::Modifiers::NONE);
        assert_eq!(app.distort.liquify.as_ref().unwrap().strokes.len(), 1);
        control(&mut app, &json!({"cancel": true})).unwrap();
        assert!(app.distort.liquify.is_none());
        assert_eq!(app.session.active().unwrap().history.past_len(), 2, "cancel records nothing");
    }
}

#[cfg(test)]
mod cached_session_tests {
    use super::*;

    #[test]
    fn scope_changes_preserve_strokes_and_failed_commit_preserves_session() {
        let ctx = egui::Context::default();
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        app.session.execute("file.new", json!({"width":80,"height":60,"depth":8})).unwrap();
        app.session.execute("layer.new.layer", json!({})).unwrap();
        app.session
            .edit("pixels", |doc, active| {
                doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap().fill_rect(Rect::new(20, 20, 50, 40), &[1., 0., 0., 1.]);
                Ok(())
            })
            .unwrap();
        app.sync_views();
        open(&mut app, &ctx).unwrap();
        pointer(&mut app, ToolEvent::Down { x: 30., y: 30., pressure: 1. }, egui::Modifiers::NONE);
        pointer(&mut app, ToolEvent::Move { x: 42., y: 30., pressure: 1. }, egui::Modifiers::NONE);
        pointer(&mut app, ToolEvent::Up { x: 42., y: 30. }, egui::Modifiers::NONE);
        let before = app.session.active().unwrap().history.past_len();
        control(&mut app, &json!({"scope":"all"})).unwrap();
        refresh_scope(&mut app, Some(&ctx)).unwrap();
        assert_eq!(app.distort.liquify.as_ref().unwrap().strokes.len(), 1);
        assert_eq!(app.session.active().unwrap().history.past_len(), before, "changing scope never commits");
        app.session
            .edit("lock", |doc, active| {
                doc.layer_mut(active.unwrap()).unwrap().locks.pixels = true;
                Ok(())
            })
            .unwrap();
        commit(&mut app);
        let d = app.distort.liquify.as_ref().expect("failed commit must retain all pending edits");
        assert_eq!(d.strokes.len(), 1);
        assert!(d.gpu_dirty, "failed commit must restore its resident preview on the next frame");
        cancel(&mut app);
    }
}
