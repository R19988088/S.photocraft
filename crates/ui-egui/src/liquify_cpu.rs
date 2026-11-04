//! CPU Liquify preview: the same brush deformation without a GPU.
//!
//! The preview is a document proxy (every layer downsampled by `k`, `k = 1` at 100 % zoom and
//! closer) whose target layers carry the field's warp. A dab warps only the proxy pixels under the
//! brush and re-composites only that rectangle, then uploads just those pixels — so the preview
//! stays live on machines whose GPU cannot run the compute-shader path, and after the device was
//! lost mid-session.
//!
//! `k` follows the view: zoomed out, the proxy is small (the screen cannot show more, and a
//! brush-sized warp stays cheap); at 100 % and beyond it is 1, so pixels under the brush are full
//! resolution. The proxy is rebuilt only when that factor, the document or the target set changes
//! — never per dab.

use egui::TextureHandle;
use photocraft_algo::liquify::LiquifyField;
use photocraft_doc::{Document, LayerContent, LayerId};
use photocraft_geom::Rect;
use photocraft_raster::Surface;

/// Longest side of the preview texture (round-number headroom under every driver's limit).
const MAX_SIDE: i32 = 4096;
/// Proxy pixels the preview aims for when the texture limit is not the tighter one.
const BUDGET_PIXELS: f64 = 4_000_000.0;

/// Downsample factor for a document shown at `zoom` physical px per document px. `faithful`
/// documents (layer effects or vector masks, whose sizes do not scale with a proxy) stay at 1.
pub(super) fn scale_for(canvas: Rect, zoom: f32, faithful: bool) -> i32 {
    if faithful {
        return 1;
    }
    let (w, h) = (i64::from(canvas.width().max(1)), i64::from(canvas.height().max(1)));
    let side = ((w.max(h) as f64) / f64::from(MAX_SIDE)).ceil() as i32;
    let budget = (((w * h) as f64) / BUDGET_PIXELS).sqrt().ceil() as i32;
    let zoomed = if zoom.is_finite() && zoom > 0.0 { (1.0 / f64::from(zoom)).ceil() as i32 } else { 1 };
    zoomed.max(side).max(budget).max(1).min(64)
}

pub(super) struct Preview {
    /// Proxy document: target surfaces carry the warp, everything else is the reduced original.
    doc: Document,
    doc_id: u64,
    k: i32,
    /// Proxy rectangle covering the whole canvas.
    bounds: Rect,
    /// Document canvas the proxy covers (proxy pixels are document pixels / k).
    origin: Rect,
    /// Document-resolution pixels of the target layers, warped into their proxy copies.
    pixels: Vec<(LayerId, Surface)>,
    /// Document-resolution linked masks of the targets.
    masks: Vec<(LayerId, Surface)>,
    tex: Option<TextureHandle>,
    /// Proxy rectangle to warp and upload.
    dirty: Option<Rect>,
    mask_tex: Option<TextureHandle>,
    mask_dirty: bool,
}

impl Preview {
    pub(super) fn new(doc: &Document, targets: &[LayerId], canvas: Rect, zoom: f32, faithful: bool) -> Result<Self, String> {
        let pixels: Vec<(LayerId, Surface)> = targets.iter().filter_map(|id| Some((*id, doc.layer(*id)?.surface()?.clone()))).collect();
        if pixels.is_empty() {
            return Err("Liquify has no pixel sources".into());
        }
        let masks: Vec<(LayerId, Surface)> =
            targets.iter().filter_map(|id| Some((*id, doc.layer(*id)?.mask.as_ref().filter(|m| m.linked)?.surface.clone()))).collect();
        let mut preview = Preview {
            doc: doc.clone(),
            doc_id: doc.id.0,
            k: 0,
            bounds: Rect::EMPTY,
            origin: canvas,
            pixels,
            masks,
            tex: None,
            dirty: None,
            mask_tex: None,
            mask_dirty: true,
        };
        preview.rebuild(doc, scale_for(canvas, zoom, faithful), canvas);
        Ok(preview)
    }

    #[cfg(test)]
    pub(super) fn scale(&self) -> i32 {
        self.k
    }

    /// The warped proxy document (tests read its pixels).
    #[cfg(test)]
    pub(super) fn doc(&self) -> &Document {
        &self.doc
    }

    /// Proxy size, and the whole document area it covers.
    pub(super) fn size(&self) -> [usize; 2] {
        [self.bounds.width().max(0) as usize, self.bounds.height().max(0) as usize]
    }

    /// Rebuilds the proxy at `k`: the targets' proxy surfaces start from the unwarped copy.
    fn rebuild(&mut self, doc: &Document, k: i32, canvas: Rect) {
        let k = k.max(1);
        self.doc = crate::proxy::proxy_document(doc, k as u32);
        self.doc_id = doc.id.0;
        self.k = k;
        self.origin = canvas;
        // Proxy pixels are document pixels divided by `k`, keeping the document's own origin
        // (`proxy_document` divides content bounds the same way).
        let q = |v: i32| v.div_euclid(k);
        let ceil = |v: i32| (i64::from(v) + i64::from(k) - 1).div_euclid(i64::from(k)) as i32;
        self.bounds = Rect::new(q(canvas.x0), q(canvas.y0), ceil(canvas.x1), ceil(canvas.y1));
        self.tex = None;
        self.dirty = Some(self.bounds);
        self.mask_tex = None;
        self.mask_dirty = true;
    }

    /// Marks document pixels whose preview must change (a dab's damage). `r` is in document
    /// coordinates; the proxy starts at the field's own origin, so the offset comes off first.
    pub(super) fn mark(&mut self, r: Rect) {
        if r.is_empty() || self.k <= 0 {
            return;
        }
        let k = i64::from(self.k);
        let rect = Rect::new(
            (i64::from(r.x0)).div_euclid(k) as i32,
            (i64::from(r.y0)).div_euclid(k) as i32,
            (i64::from(r.x1)).div_euclid(k) as i32,
            (i64::from(r.y1)).div_euclid(k) as i32,
        )
        .inflate(2)
        .intersect(&self.bounds);
        if rect.is_empty() {
            return;
        }
        self.dirty = Some(match self.dirty {
            Some(d) => Rect::new(d.x0.min(rect.x0), d.y0.min(rect.y0), d.x1.max(rect.x1), d.y1.max(rect.y1)),
            None => rect,
        });
    }

    /// Full refresh: the whole proxy (undo, restore all, scope change).
    pub(super) fn mark_all(&mut self) {
        self.dirty = Some(self.bounds);
        self.mask_dirty = true;
    }

    pub(super) fn dirty_mask(&mut self) {
        self.mask_dirty = true;
    }

    /// Warps the dirty proxy region from the untouched document pixels and uploads the
    /// re-composited result. Rebuilds the proxy when the view scale, document or targets changed.
    pub(super) fn update(
        &mut self,
        field: &LiquifyField,
        doc: &Document,
        targets: &[LayerId],
        faithful: bool,
        display: Option<&photocraft_engine::display_color::CanvasDisplay>,
        ctx: &egui::Context,
        zoom: f32,
    ) {
        let want = scale_for(field.bounds, zoom, faithful);
        let same_targets = self.pixels.len() == targets.len() && targets.iter().all(|id| self.pixels.iter().any(|(p, _)| p == id));
        if want != self.k || self.doc_id != doc.id.0 || !same_targets {
            self.rebuild(doc, want, field.bounds);
        }
        let Some(dirty) = self.dirty.take() else { return };
        if dirty.is_empty() {
            return;
        }
        let k = i64::from(self.k);
        // Displaced pixels reach a little further than the warp centre.
        let reach = (i64::from(field.max_displacement().ceil() as i32).max(0) / k).max(0) as i32 + 2;
        let region = dirty.inflate(reach).intersect(&self.bounds);
        if region.is_empty() {
            return;
        }
        for (id, src) in &self.pixels {
            if let Some(surface) = self.doc.layer_mut(*id).and_then(|l| match &mut l.content {
                LayerContent::Raster(s) => Some(s),
                _ => None,
            }) {
                warp_region(surface, src, field, self.k, region);
            }
        }
        for (id, src) in &self.masks {
            if let Some(surface) = self.doc.layer_mut(*id).and_then(|l| l.mask.as_mut().map(|m| &mut m.surface)) {
                warp_region(surface, src, field, self.k, region);
            }
        }
        let buf = photocraft_compose::render(&self.doc, region);
        let image = crate::canvas::display_image(display, &buf);
        let options = egui::TextureOptions::LINEAR;
        let full = self.size();
        match &mut self.tex {
            Some(tex) if tex.size() == full => {
                tex.set_partial([region.x0.max(0) as usize, region.y0.max(0) as usize], image, options);
            }
            _ => {
                let whole = photocraft_compose::render(&self.doc, self.bounds);
                self.tex = Some(ctx.load_texture("liquify-cpu-preview", crate::canvas::display_image(display, &whole), options));
            }
        }
    }

    pub(super) fn texture(&self) -> Option<egui::TextureId> {
        self.tex.as_ref().map(|t| t.id())
    }

    /// The red freeze-mask overlay, sampled from the field at the proxy resolution.
    pub(super) fn mask_texture(&mut self, field: &LiquifyField, ctx: &egui::Context) -> Option<egui::TextureId> {
        if self.mask_dirty || self.mask_tex.is_none() {
            self.mask_dirty = false;
            let k = f64::from(self.k);
            let [w, h] = self.size();
            let mut px = vec![egui::Color32::TRANSPARENT; w * h];
            for y in 0..h {
                for x in 0..w {
                    let v = field.freeze_at(f64::from(field.bounds.x0) + (x as f64 + 0.5) * k, f64::from(field.bounds.y0) + (y as f64 + 0.5) * k);
                    px[y * w + x] = egui::Color32::from_rgba_unmultiplied(230, 40, 40, (v.clamp(0.0, 1.0) * 140.0) as u8);
                }
            }
            let image = egui::ColorImage::new([w, h], px);
            match &mut self.mask_tex {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => self.mask_tex = Some(ctx.load_texture("liquify-cpu-mask", image, egui::TextureOptions::LINEAR)),
            }
        }
        self.mask_tex.as_ref().map(|t| t.id())
    }
}

/// Warps `src` (document resolution) through `field` into `dst`'s proxy pixels over `region`,
/// bilinearly and premultiplied. Pixels outside `region` keep what they hold.
fn warp_region(dst: &mut Surface, src: &Surface, field: &LiquifyField, k: i32, region: Rect) {
    let kf = f64::from(k);
    let bounds = field.bounds;
    // `region` is in proxy coordinates (document / k), so multiplying it back lands on the
    // document grid directly.
    let pad = f64::from(field.max_displacement()).ceil() as i32 + 3;
    let foot = Rect::new(region.x0 * k - pad, region.y0 * k - pad, region.x1 * k + pad, region.y1 * k + pad).intersect(&bounds);
    if foot.is_empty() {
        return;
    }
    let (fw, fh) = (foot.width() as usize, foot.height() as usize);
    let mut source = vec![[0.0f32; 4]; fw * fh];
    src.read_rgba_into(foot, &mut source);
    for p in &mut source {
        let a = p[3];
        p[0] *= a;
        p[1] *= a;
        p[2] *= a;
    }
    let at = |x: i32, y: i32| -> [f32; 4] {
        let x = x.clamp(foot.x0, foot.x1 - 1) - foot.x0;
        let y = y.clamp(foot.y0, foot.y1 - 1) - foot.y0;
        source[y as usize * fw + x as usize]
    };
    let sample = |x: f64, y: f64| -> [f32; 4] {
        let fx = (x - 0.5).clamp(f64::from(foot.x0), f64::from(foot.x1 - 1));
        let fy = (y - 0.5).clamp(f64::from(foot.y0), f64::from(foot.y1 - 1));
        let (ix, iy) = (fx.floor(), fy.floor());
        let (tx, ty) = ((fx - ix) as f32, (fy - iy) as f32);
        let (a, b, c, e) = (at(ix as i32, iy as i32), at(ix as i32 + 1, iy as i32), at(ix as i32, iy as i32 + 1), at(ix as i32 + 1, iy as i32 + 1));
        std::array::from_fn(|i| (a[i] * (1.0 - tx) + b[i] * tx) * (1.0 - ty) + (c[i] * (1.0 - tx) + e[i] * tx) * ty)
    };
    let format = dst.format();
    let n = format.channels();
    let (w, h) = (region.width() as usize, region.height() as usize);
    let mut values = vec![0.0f32; w * h * n];
    let mut px = [0.0f32; 8];
    for y in 0..h {
        let dy = (region.y0 as f64 + y as f64 + 0.5) * kf;
        for x in 0..w {
            let dx = (region.x0 as f64 + x as f64 + 0.5) * kf;
            let d = field.sample(dx, dy);
            let p = sample(dx + d[0], dy + d[1]);
            let a = p[3].clamp(0.0, 1.0);
            let rgb = if a > 0.0 { [p[0] / a, p[1] / a, p[2] / a] } else { [0.0; 3] };
            let rgba = [rgb[0].clamp(0.0, 1.0), rgb[1].clamp(0.0, 1.0), rgb[2].clamp(0.0, 1.0), a];
            let written = photocraft_raster::from_rgba_into(&format, rgba, &mut px);
            let o = (y * w + x) * n;
            values[o..o + written].copy_from_slice(&px[..written]);
        }
    }
    dst.write_region(region, &values);
}
