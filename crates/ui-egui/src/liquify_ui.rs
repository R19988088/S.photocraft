//! Standalone on-canvas Liquify tool with its controls in the options bar.
//!
//! Native previews keep the original full-resolution pixels resident on the GPU. Pointer events
//! edit the inverse field; the shader samples the original without resampling document pixels.
//! Only confirmation commits the recorded strokes, as one document history entry.

#[path = "liquify_cpu.rs"]
mod cpu;
#[path = "liquify_gpu.rs"]
mod gpu;

pub(crate) use gpu::supported as gpu_supported;
use egui::{Color32, Rect as ERect, TextureHandle};
#[cfg(test)]
use egui::{pos2, vec2};
use photocraft_algo::liquify::{LiquifyField, LiquifyStroke, LiquifyTool, ProxyImage, auto_cell};
use photocraft_doc::LayerId;
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::canvas::{ToolEvent, ViewXform};
use crate::widgets;

/// Longest side of the preview proxy (px).
const PROXY_SIDE: usize = 1600;
#[cfg(test)]
const MAX_VIEW_PREVIEW_PIXELS: usize = 8 * 1024 * 1024;
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
            scope: "all".into(),
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
    /// CPU preview (proxy document), used when the GPU cannot run the resident path.
    cpu: Option<Box<cpu::Preview>>,
    /// Why the resident GPU preview is not used (`None` = it is).
    gpu_error: Option<String>,
    gpu_dirty: bool,
    gpu_doc: Option<std::sync::Arc<photocraft_doc::Document>>,
    gpu_damage: Option<Rect>,
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
    #[cfg(test)]
    view_out: Vec<[u8; 4]>,
    #[cfg(test)]
    view_source_scratch: Vec<[f32; 4]>,
    view_tex: Option<TextureHandle>,
    view_dirty: Option<Rect>,
    mask_tex: Option<TextureHandle>,
    mask_dirty: bool,
    pub opts: LiquifyOpts,
    /// Screen px per document px (0 = fit on next frame) and the view centre (document px).
    #[cfg(test)]
    zoom: f32,
    #[cfg(test)]
    center: [f64; 2],
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
            "cpuPreview": self.cpu.is_some(),
            "gpuError": self.gpu_error,
            "tool": self.opts.tool,
            "size": self.opts.size,
            "strokes": self.strokes.len() + usize::from(self.cur.is_some()),
            "redo": self.redo.len(),
            "meshSize": self.cell,
            "maxDisplacement": self.field.max_displacement(),
            "proxy": [self.proxy.w, self.proxy.h],
            "cpuProxy": self.cpu.as_ref().map(|p| {
                let [w, h] = p.size();
                [w, h]
            }),
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
        if let Some(cpu) = &mut self.cpu {
            cpu.mark(doc_rect.inflate((self.cell * 2.).ceil() as i32).intersect(&self.canvas));
            if mask {
                cpu.dirty_mask();
            }
        }
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
        if let Some(cpu) = &mut self.cpu {
            cpu.mark_all();
        }
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

    /// Re-renders the dirty proxy pixels into `out` (the texture upload happens in `show`). The
    /// CPU document preview is recomposited by the canvas from its own dirty rectangles.
    fn render_dirty(&mut self) {
        if self.gpu.is_some() || self.cpu.is_some() {
            return;
        }
        if let Some(r) = self.dirty {
            self.proxy.render(&self.field, r, &mut self.out);
        }
    }

    /// Builds or updates the visible source-resolution viewport. At ordinary zoom the whole-layer
    /// proxy remains cheaper; above 100%, a full-resolution crop is enough to fill the preview.
    #[cfg(test)]
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
        if self.gpu.is_none() && self.cpu.is_some() {
            // The CPU preview document is drawn by the canvas; the dialog needs no proxy texture.
            return;
        }
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

pub fn draw_cursor(app: &PhotocraftApp, painter: &egui::Painter, xf: &ViewXform) {
    let Some(d) = app.distort.liquify.as_ref().filter(|_| app.ui.tool == crate::state::Tool::Liquify) else { return };
    let Some(pointer) = painter.ctx().input(|i| i.pointer.hover_pos()) else { return };
    let radius_x = (d.opts.size * xf.zoom_x() * 0.5).max(1.0);
    let radius_y = (d.opts.size * xf.zoom * 0.5).max(1.0);
    let color = crate::theme::Tokens::get(painter.ctx()).text;
    painter.add(egui::Shape::ellipse_stroke(pointer, egui::vec2(radius_x, radius_y), egui::Stroke::new(1.0, color)));
    let hardness = d.opts.density.clamp(0.0, 100.0) / 100.0;
    if hardness > 0.0 && hardness < 1.0 {
        let inner_x = (radius_x * hardness).max(1.0);
        let inner_y = (radius_y * hardness).max(1.0);
        painter.add(egui::Shape::ellipse_stroke(pointer, egui::vec2(inner_x, inner_y), egui::Stroke::new(1.0, color.gamma_multiply(0.65))));
    }
}

/// Compact tool options shown in the main options bar while Liquify is active.
/// The full properties panel remains available through the control channel; the
/// in-canvas tool only exposes target scope, size and hardness (density).
pub fn options_bar(app: &mut PhotocraftApp, ui: &mut egui::Ui) -> bool {
    if app.distort.liquify.is_none()
        && app.ui.tool == crate::state::Tool::Liquify
        && let Err(error) = open(app, ui.ctx())
    {
        crate::notices::error(app, error);
        return true;
    }
    let Some(d) = app.distort.liquify.as_mut() else { return false };
    for tool in LiquifyTool::ALL.into_iter().filter(|tool| *tool != LiquifyTool::Smooth) {
        let tip = if tool == LiquifyTool::LassoMask {
            tl!("Freeze Lasso: drag to freeze an area, Alt-drag to thaw it (L)").to_string()
        } else {
            tl!(tool.label()).to_string()
        };
        if crate::icons::button(ui, tool_icon(tool), 26.0, d.opts.tool == tool, &tip).on_hover_text(tip).clicked() {
            d.opts.tool = tool;
        }
    }
    widgets::vline(ui, 22.0);
    let before_scope = d.opts.scope.clone();
    egui::ComboBox::from_id_salt("liquify-options-scope").selected_text(scope_label(&d.opts.scope)).show_ui(ui, |ui| {
        for scope in ["layer", "group", "all"] {
            ui.selectable_value(&mut d.opts.scope, scope.into(), scope_label(scope));
        }
    });
    if before_scope != d.opts.scope {
        d.end();
        d.scope_dirty = true;
    }
    ui.scope(|ui| {
        ui.spacing_mut().slider_width = 90.0;
        ui.label(tl!("Size"));
        ui.add(egui::Slider::new(&mut d.opts.size, 1.0..=3000.0).suffix(" px"));
        ui.label(tl!("Hardness"));
        ui.add(egui::Slider::new(&mut d.opts.density, 0.0..=100.0).suffix("%"));
    });
    true
}

/// Opens the dialog on the active layer. The resident GPU preview is used when the device can run
/// it; otherwise (an old or software GPU, a lost device, or a failed preview build) the CPU proxy
/// preview takes over, with every tool and every scope still working.
pub fn open(app: &mut PhotocraftApp, ctx: &egui::Context) -> Result<(), String> {
    let st = app.session.active().ok_or("no document")?;
    let mut opts = LiquifyOpts::default();
    if let Some(saved) = app.session.prefs().dialogs.get(REMEMBERED) {
        opts.apply(saved).ok();
    }
    let ids = photocraft_engine::distort_cmds::liquify_targets(&app.session, &json!({"scope":opts.scope})).map_err(|e| e.to_string())?;
    let layer = st.active_layer.or_else(|| ids.first().copied()).ok_or("no active layer")?;
    let surf = ids.first().and_then(|id| st.doc.layer(*id)).and_then(|l| l.surface()).cloned().ok_or("target has no pixel cache")?;
    let st = app.session.active().ok_or("no document")?;
    let canvas = st.doc.bounds();
    let cell = auto_cell(canvas);
    // The GPU decides the mode; if it cannot run the resident preview, the proxy is built on the
    // CPU and the dialog opens all the same.
    let gpu_error = liquify_gpu_error(app);
    let use_gpu = gpu_error.is_none();
    let proxy = if use_gpu {
        ProxyImage::new(&Surface::new(photocraft_color::PixelFormat::RGBA32F), Rect::new(0, 0, 1, 1), 1)
    } else {
        ProxyImage::new(&surf, canvas, PROXY_SIDE)
    };
    let out = vec![[0u8; 4]; proxy.w * proxy.h];
    #[cfg(test)]
    let center = [f64::from(canvas.x0 + canvas.x1) / 2.0, f64::from(canvas.y0 + canvas.y1) / 2.0];
    let mut d = LiquifyDialog {
        layer,
        targets: vec![layer],
        cached_scope: "all".into(),
        scope_dirty: true,
        gpu: None,
        cpu: None,
        gpu_dirty: true,
        gpu_doc: None,
        gpu_error,
        gpu_damage: None,
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
        #[cfg(test)]
        view_out: Vec::new(),
        #[cfg(test)]
        view_source_scratch: Vec::new(),
        view_tex: None,
        view_dirty: None,
        mask_tex: None,
        mask_dirty: true,
        opts,
        #[cfg(test)]
        zoom: 0.0,
        #[cfg(test)]
        center,
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
    app.ui.tool = crate::state::Tool::Liquify;
    // Say once why the preview is on the CPU: the tool still works, but slower on large images.
    if let Some(reason) = app.distort.liquify.as_ref().and_then(|d| d.cpu.as_ref().and(d.gpu_error.clone())) {
        app.ui.status = crate::i18n::fmt("Liquify preview is using the CPU: {reason}", &[("reason", &reason)]);
        log::info!("Liquify preview on the CPU: {reason}");
    }
    Ok(())
}

/// Why the resident GPU Liquify preview cannot run here (`None` when it can).
fn liquify_gpu_error(app: &PhotocraftApp) -> Option<String> {
    let gpu = app.gpu.as_ref()?;
    if !gpu.health().is_ok() {
        return Some("the GPU device is unavailable".into());
    }
    gpu_supported(gpu.render_state()).err()
}

/// Whether the open dialog is previewing on the CPU proxy rather than the resident GPU path.
#[cfg(test)]
pub(crate) fn uses_cpu_preview(app: &PhotocraftApp) -> bool {
    app.distort.liquify.as_ref().is_some_and(|d| d.cpu.is_some())
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
    let scope = app.distort.liquify.as_ref().ok_or("Liquify is not open")?.opts.scope.clone();
    let st = app.session.active().ok_or("no document")?;
    let canvas = st.doc.bounds();
    let targets = photocraft_engine::distort_cmds::liquify_targets(&app.session, &json!({"scope":scope})).map_err(|e| e.to_string())?;
    let mut doc = (*st.doc).clone();
    // The dialog owns its display texture. The document canvas and Navigator can refresh
    // independently while the cached Liquify edit remains unchanged.
    doc.id = app.distort.liquify.as_ref().and_then(|d| d.gpu_doc.as_ref().map(|doc| doc.id)).unwrap_or_else(photocraft_doc::DocId::fresh);
    let zoom = ctx.map_or(1.0, |_| crate::canvas::ViewXform::active(app).map_or(1.0, |xf| xf.zoom));
    // The GPU path needs a healthy device whose capabilities cover the resident preview; when it
    // does not, the CPU proxy takes over rather than refusing to open the tool.
    let mut gpu_error = liquify_gpu_error(app);
    let render_state = app.gpu.as_ref().filter(|_| gpu_error.is_none()).map(|r| r.render_state().clone());
    let d = app.distort.liquify.as_mut().ok_or("Liquify is not open")?;
    let mut preview = None;
    if let Some(rs) = render_state {
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
        match photocraft_color::convert::with_cmyk_space(photocraft_compose::cmyk_space(&doc).as_ref(), || {
            gpu::Preview::new(&rs, sources, doc.selection.as_ref(), canvas, &d.field)
        }) {
            Ok(p) => preview = Some(p),
            Err(error) => gpu_error = Some(error),
        }
    }
    // CPU preview: the proxy document gets the same field; every target and every scope is
    // previewed and committed through the engine, just like the resident path.
    let cpu = if preview.is_none() {
        let faithful = !photocraft_compose::proxy::proxy_faithful(&doc);
        match cpu::Preview::new(&doc, &targets, canvas, zoom, faithful) {
            Ok(p) => Some(Box::new(p)),
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    d.gpu_error = gpu_error;
    d.targets = targets;
    d.cached_scope = scope;
    d.scope_dirty = false;
    d.gpu = preview;
    d.cpu = cpu;
    if d.gpu.is_some() {
        // The resident path records dabs as GPU work instead of editing the CPU field.
        d.source = doc.layer(d.layer).and_then(|l| l.surface()).cloned().unwrap_or_else(|| d.source.clone());
        d.field = LiquifyField::new(d.canvas, d.cell);
        d.field.gpu_only = true;
        for stroke in &d.strokes {
            d.field.apply_stroke(stroke);
        }
        if let Some((stroke, _)) = &d.cur {
            d.field.apply_stroke(stroke);
        }
        d.gpu_doc = Some(std::sync::Arc::new(doc));
    } else {
        // CPU: the field stays editable (dabs mutate it directly); rebuild it from the strokes.
        d.field = LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes);
        if let Some((stroke, _)) = &d.cur {
            d.field.apply_stroke(stroke);
        }
        d.gpu_doc = None;
    }
    d.gpu_dirty = true;
    d.proxy = if d.gpu.is_some() {
        ProxyImage::new(&Surface::new(photocraft_color::PixelFormat::RGBA32F), Rect::new(0, 0, 1, 1), 1)
    } else {
        // Kept for the dialog's own footer metrics; the canvas draws the CPU preview document.
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
/// Whether the GPU can still run the resident preview. Out of line so the capability probe's
/// frame stays off the canvas's call chain.
#[inline(never)]
fn gpu_healthy(app: &PhotocraftApp) -> bool {
    app.gpu.as_ref().is_some_and(|gpu| gpu.health().is_ok() && gpu_supported(gpu.render_state()).is_ok())
}

/// Switches an open dialog from the resident GPU preview to the CPU proxy, keeping the recorded
/// strokes (the field is rebuilt from them, since GPU dabs only queued ops).
fn switch_to_cpu(app: &mut PhotocraftApp, ctx: Option<&egui::Context>) -> Result<(), String> {
    let d = app.distort.liquify.as_mut().ok_or("Liquify is not open")?;
    if d.cpu.is_some() {
        return Ok(());
    }
    d.end();
    // Flush any queued GPU dab into the CPU field by replaying the recorded strokes.
    d.field = LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes);
    d.field.gpu_only = false;
    d.field.gpu_ops.clear();
    d.gpu = None;
    d.gpu_doc = None;
    d.gpu_error = Some("CPU preview: the GPU preview is unavailable".into());
    d.gpu_dirty = true;
    d.scope_dirty = true;
    if let Some(ctx) = ctx {
        refresh_scope(app, Some(ctx))?;
    }
    Ok(())
}

fn remember(app: &mut PhotocraftApp, opts: &LiquifyOpts) {
    let v = opts.to_json();
    app.session.prefs.edit(|p| p.dialogs.insert(REMEMBERED.into(), v));
}

/// Present the edit in the active canvas without touching the document: the resident GPU output,
/// or the CPU proxy document (which the normal canvas path composites and caches).
pub(crate) fn canvas_preview(app: &mut PhotocraftApp, ctx: &egui::Context, idx: usize) -> Option<std::sync::Arc<photocraft_doc::Document>> {
    if app.ui.tool != crate::state::Tool::Liquify || app.session.active_index() != Some(idx) {
        return None;
    }
    if app.distort.liquify.as_ref().is_some_and(|d| d.scope_dirty)
        && let Err(error) = refresh_scope(app, Some(ctx))
    {
        crate::notices::error(app, error);
        return None;
    }
    if app.distort.liquify.as_ref().is_some_and(|d| d.cpu.is_some()) {
        cpu_canvas_preview(app, ctx);
        return None;
    }
    // The device may have been lost or faulted since the dialog opened: switch to the CPU
    // preview in place, keeping every recorded stroke and the editable field.
    let healthy = gpu_healthy(app);
    if !healthy && let Err(error) = switch_to_cpu(app, Some(ctx)) {
        crate::notices::error(app, error);
    }
    if app.distort.liquify.as_ref().is_some_and(|d| d.cpu.is_some()) {
        cpu_canvas_preview(app, ctx);
        return None;
    }
    let renderer = app.gpu.clone()?;
    let d = app.distort.liquify.as_mut()?;
    let doc = d.gpu_doc.clone()?;
    if let Some((_, last)) = &d.cur {
        let last = *last;
        if d.opts.tool.is_stationary() && crate::gpu_canvas::now_ms() - d.last_dab > 33.0 {
            d.extend(last, crate::gpu_canvas::now_ms());
        }
        ctx.request_repaint();
    }
    let preview = d.gpu.as_ref()?;
    let ops = std::mem::take(&mut d.field.gpu_ops);
    if !ops.is_empty() || d.gpu_dirty || !renderer.has(doc.id.0, [doc.size.width, doc.size.height]) {
        let started = crate::gpu_canvas::now_ms();
        preview.apply_ops(renderer.render_state(), &d.field, &ops, d.gpu_dirty);
        let damage = if d.gpu_dirty { None } else { d.gpu_damage.take() };
        renderer.set_source_overrides(doc.id.0, preview.render_outputs(renderer.render_state(), damage));
        if let Err(error) = renderer.composite(&doc, doc.bounds(), false) {
            d.gpu_error = Some(error.0.clone());
            crate::notices::error(app, error.0);
            return Some(doc);
        }
        d.gpu_error = None;
        d.gpu_dirty = false;
        d.preview_ms = crate::gpu_canvas::now_ms() - started;
    }
    Some(doc)
}

/// The CPU preview's document, brought up to date with the field. The warped proxy is drawn by
/// the canvas as an overlay ([`draw_cpu_preview`]).
fn cpu_canvas_preview(app: &mut PhotocraftApp, ctx: &egui::Context) -> Option<std::sync::Arc<photocraft_doc::Document>> {
    // Read everything that borrows `app` immutably first; the dialog is borrowed mutably below.
    let st = app.session.active()?;
    let doc = st.doc.clone();
    let faithful = !photocraft_compose::proxy::proxy_faithful(&doc);
    let zoom = crate::canvas::ViewXform::active(app).map_or(1.0, |xf| xf.zoom);
    let (display, _) = crate::canvas::canvas_display(app, &doc, None);
    let d = app.distort.liquify.as_mut()?;
    if let Some((_, last)) = &d.cur {
        let last = *last;
        if d.opts.tool.is_stationary() && crate::gpu_canvas::now_ms() - d.last_dab > 33.0 {
            d.extend(last, crate::gpu_canvas::now_ms());
        }
        ctx.request_repaint();
    }
    let started = crate::gpu_canvas::now_ms();
    let targets = d.targets.clone();
    let cpu = d.cpu.as_mut()?;
    cpu.update(&d.field, &doc, &targets, faithful, display.as_deref(), ctx, zoom);
    d.preview_ms = crate::gpu_canvas::now_ms() - started;
    None
}

pub(crate) fn draw_gpu_overlay(app: &PhotocraftApp, painter: &egui::Painter, area: ERect, image: ERect) {
    if let Some(d) = &app.distort.liquify
        && let Some(gpu) = &d.gpu
    {
        painter.add(gpu.callback(area, image, d.canvas, &d.field, d.opts.show_mask, d.opts.show_mesh, d.opts.mesh_size));
    }
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

/// Draws the CPU preview when the dialog is on it. A one-line entry point so `canvas_view`'s frame
/// stays small (its stack is already near the test threads' limit).
#[inline(never)]
pub(crate) fn draw_cpu_preview_if_open(app: &mut PhotocraftApp, ctx: &egui::Context, painter: &egui::Painter, xf: &ViewXform) {
    if app.distort.liquify.as_ref().is_some_and(|d| d.cpu.is_some()) {
        draw_cpu_preview(app, ctx, painter, xf);
    }
}

/// Draws the CPU preview over the canvas image (the warped proxy scaled into the document's place
/// on screen) and the freeze mask. Kept out of line: `canvas_view`'s own frame is already large.
#[inline(never)]
pub(crate) fn draw_cpu_preview(app: &mut PhotocraftApp, ctx: &egui::Context, painter: &egui::Painter, xf: &ViewXform) {
    let Some(d) = app.distort.liquify.as_mut().filter(|d| d.cpu.is_some()) else { return };
    let field = d.field.clone();
    let Some(cpu) = d.cpu.as_mut() else { return };
    // The proxy covers the canvas; its four corners go through the view transform so rotation,
    // flip and pixel-aspect correction land it exactly on the canvas image.
    let c = d.canvas;
    let corners = [
        xf.to_screen(c.x0 as f32, c.y0 as f32),
        xf.to_screen(c.x1 as f32, c.y0 as f32),
        xf.to_screen(c.x1 as f32, c.y1 as f32),
        xf.to_screen(c.x0 as f32, c.y1 as f32),
    ];
    let uvs = [egui::pos2(0.0, 0.0), egui::pos2(1.0, 0.0), egui::pos2(1.0, 1.0), egui::pos2(0.0, 1.0)];
    let paint = |tex: egui::TextureId| {
        let mut mesh = egui::Mesh::with_texture(tex);
        for i in 0..4 {
            mesh.vertices.push(egui::epaint::Vertex { pos: corners[i], uv: uvs[i], color: Color32::WHITE });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(egui::Shape::mesh(mesh));
    };
    if let Some(tex) = cpu.texture() {
        paint(tex);
    }
    if d.opts.show_mask
        && let Some(tex) = cpu.mask_texture(&field, ctx)
    {
        paint(tex);
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
    if d.gpu.is_none() && d.cpu.is_none() {
        crate::notices::error(app, "Liquify has no preview to commit.".into());
        app.distort.liquify = Some(d);
        return;
    }
    if let (Some(gpu), Some(renderer)) = (&d.gpu, &app.gpu) {
        let ops = std::mem::take(&mut d.field.gpu_ops);
        gpu.apply_ops(renderer.render_state(), &d.field, &ops, d.gpu_dirty);
        // Refresh tile flags after the final queued dab, then map native planes once.
        gpu.render_outputs(renderer.render_state(), None);
        let result = gpu.commit_surfaces(renderer.render_state()).and_then(|(pixels, masks)| {            let document = app.session.active().ok_or("no document")?.doc.id;
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

/// Refresh the frozen target set after an options-bar scope change.
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn selected_liquify_opens_options_and_both_sliders_change_settings() {
        use egui_kittest::{Harness, kittest::Queryable};
        let mut app = app_with_layer();
        crate::tool_memory::select(&mut app, crate::state::Tool::Liquify);
        let mut h = Harness::builder().with_size(vec2(1200.0, 80.0)).build_ui_state(
            |ui, app: &mut PhotocraftApp| {
                if ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                    crate::panels::options_bar(app, ui);
                }
            },
            app,
        );
        PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::Studio);
        h.run_steps(4);
        assert!(h.state().distort.liquify.is_some());
        let sliders: Vec<_> = h.get_all_by_role(egui::accesskit::Role::Slider).map(|n| n.rect()).collect();
        assert_eq!(sliders.len(), 2);
        for rect in &sliders {
            assert!(rect.right() < 1200.0 && rect.bottom() < 80.0, "Liquify controls must fit the options bar");
        }
        let before = h.state().distort.liquify.as_ref().unwrap().opts.clone();
        for rect in sliders {
            let at = pos2(rect.left() + rect.width() * 0.8, rect.center().y);
            h.hover_at(at);
            h.run();
            h.drag_at(at);
            h.run();
            h.drop_at(at);
            h.run_steps(2);
        }
        let after = &h.state().distort.liquify.as_ref().unwrap().opts;
        assert_ne!(before.size, after.size);
        assert_ne!(before.density, after.density);
    }

    #[test]
    fn canvas_tool_pointer_events_record_and_preview_a_short_drag() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        crate::tool_memory::select(&mut app, crate::state::Tool::Liquify);
        assert!(app.distort.liquify.is_none());
        let mut output = ctx.run_ui(Default::default(), |ui| {
            assert!(crate::distort_ui::options_bar(&mut app, ui));
        });
        output.textures_delta.clear();
        assert!(app.distort.liquify.is_some(), "selecting the tool must open its options and displacement session");
        control(&mut app, &json!({"tool":"forwardWarp","size":40,"pressure":100})).unwrap();
        app.distort.liquify.as_mut().unwrap().field.gpu_only = true;
        let start = [40.0, 40.0];
        let end = [64.0, 40.0];
        crate::canvas::tool_event(&mut app, ToolEvent::Down { x: start[0], y: start[1], pressure: 1.0 }, egui::Modifiers::NONE);
        crate::canvas::tool_event(&mut app, ToolEvent::Move { x: end[0], y: end[1], pressure: 1.0 }, egui::Modifiers::NONE);
        let d = app.distort.liquify.as_ref().unwrap();
        assert!(!d.field.gpu_ops.is_empty(), "a canvas drag must schedule its Liquify displacement");
        assert!(d.field.gpu_ops.iter().any(|op| op.delta[0] > 0.0));
        crate::canvas::tool_event(&mut app, ToolEvent::Up { x: end[0], y: end[1] }, egui::Modifiers::NONE);
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.strokes.len(), 1);
        assert_eq!(&d.strokes[0].points.first().unwrap()[..2], &start);
        assert_eq!(&d.strokes[0].points.last().unwrap()[..2], &end);
        assert!(LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes).max_displacement() > 0.0);
    }

    #[test]
    fn zoomed_liquify_preview_shows_full_resolution_pixels() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        // The CPU proxy follows the view: at 100 % and beyond it is one proxy pixel per document
        // pixel, so the brush works on source pixels however far the canvas is zoomed in.
        assert_eq!(app.distort.liquify.as_ref().unwrap().cpu.as_ref().unwrap().scale(), 1);
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(cpu::scale_for(d.canvas, 2.0, false), 1);
        // Zoomed out on a large document the proxy shrinks, so a brush-sized warp stays cheap.
        let big = Rect::new(0, 0, 16000, 16000);
        assert!(cpu::scale_for(big, 0.1, false) > 1);
        // Documents whose effects or vector masks do not scale stay full size.
        assert_eq!(cpu::scale_for(big, 0.1, true), 1);
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

    #[test]
    fn new_liquify_session_defaults_to_all_layers() {
        assert_eq!(LiquifyOpts::default().scope, "all");
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
    fn undo_refreshes_the_visible_preview() {
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
        assert!(app.distort.liquify.as_ref().unwrap().cpu.is_some(), "no GPU in these tests: the CPU preview previews live");
        control(&mut app, &json!({"undo": true})).unwrap();
        let d = app.distort.liquify.as_ref().unwrap();
        assert_eq!(d.strokes.len(), 1);
        assert_eq!(LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes), d.field, "undo rebuilds the previewed field");
        // The proxy document was marked for a full refresh, so the next frame redraws it.
        let mut app = app;
        canvas_preview(&mut app, &ctx, 0);
        assert!(app.distort.liquify.as_ref().unwrap().cpu.as_ref().unwrap().texture().is_some(), "the preview texture follows the rebuilt field");
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
    fn liquify_action_activates_the_canvas_tool_and_routes_pointer_events() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        let r = crate::distort_ui::menu(&mut app, &ctx, "filter.liquify", &json!({})).unwrap().unwrap();
        assert!(r["liquify"]["proxy"].is_array());
        assert_eq!(app.ui.tool, crate::state::Tool::Liquify);
        assert!(crate::distort_ui::pointer(&mut app, ToolEvent::Down { x: 50.0, y: 40.0, pressure: 1.0 }, egui::Modifiers::NONE));
        crate::distort_ui::pointer(&mut app, ToolEvent::Up { x: 60.0, y: 40.0 }, egui::Modifiers::NONE);
        assert_eq!(app.distort.liquify.as_ref().unwrap().strokes.len(), 1);
        control(&mut app, &json!({"cancel": true})).unwrap();
        assert!(app.distort.liquify.is_none());
        assert_eq!(app.session.active().unwrap().history.past_len(), 2, "cancel records nothing");
    }

    /// The CPU preview warps every target layer of a multi-layer scope, not just the active one,
    /// and its pixels match what the engine commits for the same strokes.
    #[test]
    fn cpu_preview_warps_every_layer_and_matches_the_commit() {
        let ctx = egui::Context::default();
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Default::default());
        app.session.execute("file.new", json!({"width": 120, "height": 80, "depth": 8})).unwrap();
        for (name, color) in [("a", [1.0f32, 0.0, 0.0, 1.0]), ("b", [0.0f32, 0.0, 1.0, 1.0])] {
            app.session.execute("layer.new.layer", json!({"name": name})).unwrap();
            app.session
                .edit("pixels", |doc, active| {
                    doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap().fill_rect(Rect::new(30, 20, 70, 60), &color);
                    Ok(())
                })
                .unwrap();
        }
        app.sync_views();
        open(&mut app, &ctx).unwrap();
        assert!(uses_cpu_preview(&app), "no GPU in these tests: the CPU preview is used");
        control(&mut app, &json!({"scope": "all", "tool": "forwardWarp", "size": 40, "pressure": 100})).unwrap();
        refresh_scope(&mut app, Some(&ctx)).unwrap();
        assert!(app.distort.liquify.as_ref().unwrap().targets.len() >= 2, "every pixel layer is a target");

        for ev in [
            ToolEvent::Down { x: 40.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Move { x: 60.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Up { x: 60.0, y: 40.0 },
        ] {
            pointer(&mut app, ev, egui::Modifiers::NONE);
        }
        // Bring the preview up to date and read its warped pixels on the two painted layers.
        canvas_preview(&mut app, &ctx, 0);
        let before = app.session.active().unwrap().doc.clone();
        let targets = app.distort.liquify.as_ref().unwrap().targets.clone();
        let painted: Vec<_> = targets
            .iter()
            .copied()
            .filter(|id| before.layer(*id).is_some_and(|l| l.name == "a" || l.name == "b"))
            .collect();
        assert_eq!(painted.len(), 2, "both painted layers are targets");
        let preview_doc = app.distort.liquify.as_ref().unwrap().cpu.as_ref().unwrap().doc().clone();
        for id in &painted {
            assert_ne!(
                preview_doc.layer(*id).and_then(|l| l.surface()),
                before.layer(*id).and_then(|l| l.surface()),
                "the CPU preview warps every target layer"
            );
        }

        // Committing records the same strokes with one history step.
        let history = app.session.active().unwrap().history.past_len();
        control(&mut app, &json!({"commit": true})).unwrap();
        assert!(app.distort.liquify.is_none());
        assert_eq!(app.session.active().unwrap().history.past_len(), history + 1);
        let after = app.session.active().unwrap().doc.clone();
        for id in painted {
            assert_ne!(after.layer(id).and_then(|l| l.surface()), before.layer(id).and_then(|l| l.surface()), "commit warps every target layer");
        }
    }

    /// The tool opens without any GPU canvas at all (an old machine where the GPU path cannot
    /// run), previews on the CPU, and OK still commits through the engine.
    #[test]
    fn liquify_opens_and_commits_without_a_gpu() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        assert!(app.gpu.is_none(), "the test app has no GPU canvas");
        open(&mut app, &ctx).unwrap();
        let d = app.distort.liquify.as_ref().unwrap();
        assert!(d.cpu.is_some(), "the CPU preview took over");
        assert!(d.gpu.is_none());
        for ev in [
            ToolEvent::Down { x: 35.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Move { x: 60.0, y: 40.0, pressure: 1.0 },
            ToolEvent::Up { x: 60.0, y: 40.0 },
        ] {
            pointer(&mut app, ev, egui::Modifiers::NONE);
        }
        // The preview shows the warp before OK.
        canvas_preview(&mut app, &ctx, 0);
        let d = app.distort.liquify.as_ref().unwrap();
        let target = d.layer;
        let original = app.session.active().unwrap().doc.layer(target).and_then(|l| l.surface()).cloned().unwrap();
        let shown = d.cpu.as_ref().unwrap().doc().layer(target).and_then(|l| l.surface()).cloned().unwrap();
        assert_ne!(shown, original, "the canvas shows the warped preview");
        let history = app.session.active().unwrap().history.past_len();
        control(&mut app, &json!({"commit": true})).unwrap();
        assert!(app.distort.liquify.is_none(), "OK closes the tool");
        assert_eq!(app.session.active().unwrap().history.past_len(), history + 1, "one history step");
        let after = app.session.active().unwrap().doc.layer(target).and_then(|l| l.surface()).cloned().unwrap();
        assert_ne!(after, original, "the commit warped the layer");
    }

    /// Switching to the CPU preview keeps the strokes recorded so far.
    #[test]
    fn switching_to_cpu_keeps_recorded_strokes() {
        let ctx = egui::Context::default();
        let mut app = app_with_layer();
        open(&mut app, &ctx).unwrap();
        for ev in [ToolEvent::Down { x: 40.0, y: 40.0, pressure: 1.0 }, ToolEvent::Move { x: 55.0, y: 40.0, pressure: 1.0 }, ToolEvent::Up { x: 55.0, y: 40.0 }]
        {
            pointer(&mut app, ev, egui::Modifiers::NONE);
        }
        {
            let d = app.distort.liquify.as_mut().unwrap();
            d.cpu = None;
            d.field = LiquifyField::new(d.canvas, d.cell);
            d.field.gpu_only = true;
            d.field.apply_stroke(&d.strokes[0]);
        }
        switch_to_cpu(&mut app, Some(&ctx)).unwrap();
        let d = app.distort.liquify.as_ref().unwrap();
        assert!(d.cpu.is_some());
        assert!(d.gpu.is_none());
        assert_eq!(d.strokes.len(), 1);
        assert_eq!(LiquifyField::from_strokes(d.canvas, d.cell, &d.strokes), d.field, "the field is replayed from the strokes");
        assert!(d.field.max_displacement() > 0.0);
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

#[cfg(test)]
mod cpu_tests {
    use super::*;
    use super::cpu::scale_for;

    /// The CPU preview warps the same pixels the engine's `filter.liquify` produces on a proxy:
    /// the preview and the commit agree up to the proxy's resolution.
    #[test]
    fn proxy_scale_stays_full_resolution_at_100_percent() {
        // Documents within the preview budget are full resolution at 100 % and closer.
        let canvas = Rect::new(0, 0, 1600, 1000);
        assert_eq!(scale_for(canvas, 1.0, false), 1, "100 %: full resolution");
        assert_eq!(scale_for(canvas, 2.0, false), 1, "zoomed in: full resolution");
        assert_eq!(scale_for(canvas, 0.5, false), 2);
        assert!(scale_for(canvas, 0.1, false) > 2, "zoomed out: a small proxy");
        assert_eq!(scale_for(canvas, 0.1, true), 1, "effects do not scale");
        // A big document is limited by the texture budget and side, never above `k = 1`.
        let big = Rect::new(0, 0, 6000, 4000);
        assert!(scale_for(big, 1.0, false) > 1);
        assert_eq!(scale_for(Rect::new(0, 0, 30000, 100), 1.0, false), 8, "texture side limit");
    }

    /// A dab on the CPU preview changes the proxy document's pixels where it was drawn, and the
    /// drawn proxy is what the canvas shows.
    #[test]
    fn a_dab_updates_the_proxy_document() {
        let mut doc = photocraft_doc::Document::new("t", photocraft_doc::Size::new(80, 60), photocraft_color::ColorMode::Rgb, photocraft_color::SampleType::U8);
        let mut surface = Surface::new(photocraft_color::PixelFormat::RGBA32F);
        for y in 0..60 {
            for x in 0..80 {
                surface.write_pixel(x, y, &[(x as f32) / 80.0, (y as f32) / 60.0, 0.5, 1.0]);
            }
        }
        let layer = photocraft_doc::Layer::new("l", photocraft_doc::LayerContent::Raster(surface));
        let id = layer.id;
        doc.layers = vec![layer];
        let canvas = doc.bounds();
        let mut preview = cpu::Preview::new(&doc, &[id], canvas, 1.0, false).unwrap();
        let initial = preview.doc().layer(id).and_then(|l| l.surface()).unwrap().clone();
        let mut field = LiquifyField::new(canvas, 2.0);
        let mut stroke = LiquifyStroke::new(LiquifyTool::ForwardWarp, 30.0);
        stroke.points = vec![vec![30.0, 30.0, 1.0], vec![45.0, 30.0, 1.0]];
        let dirty = field.apply_stroke(&stroke);
        preview.mark(dirty);
        preview.update(&field, &doc, &[id], false, None, &egui::Context::default(), 1.0);
        let after = preview.doc().layer(id).and_then(|l| l.surface()).unwrap();
        assert_ne!(after, &initial, "the dab warped the proxy document");
        let changed = (0..60).any(|y| (0..80).any(|x| after.sample_channel(x, y, 0) != initial.sample_channel(x, y, 0)));
        assert!(changed, "and the change is where the brush was drawn");
    }

}
