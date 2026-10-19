//! C3 picker layouts with shared colour state and a sampled image palette.
use crate::state::PaletteMode;
use crate::theme::Tokens;
use crate::{PhotocraftApp, panels, widgets};
use egui::{Color32, Mesh, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use serde_json::json;

const MODES: [(PaletteMode, &str); 4] =
    [(PaletteMode::HueCube, "Hue Cube"), (PaletteMode::Hsb, "HSB"), (PaletteMode::ColorWheel, "Color Wheel"), (PaletteMode::Lab, "Lab")];

pub(crate) fn menu(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    ui.set_width(260.0);
    for (mode, label) in MODES {
        if ui.selectable_value(&mut app.ui.color_panel.mode, mode, tl!(label)).clicked() {
            ui.close();
        }
    }
    ui.separator();
    ui.checkbox(&mut app.ui.color_panel.show_values, tl!("Show Color Values"));
    let mut strength = app.session.prefs().painting.pigment_mix_strength;
    if widgets::slider_row(ui, tl!("Mixing Strength"), &mut strength, 0.0..=100.0, "%", None).changed() {
        let _ = app.run("prefs.set", json!({"path":"painting.pigmentMixStrength", "value":strength}));
    }
    if ui.button(tl!("Painting Settings…")).clicked() {
        crate::prefs_ui::open_preferences(app, "painting");
        ui.close();
    }
}

fn set_color(app: &mut PhotocraftApp, c: [f32; 4]) {
    let target = if app.ui.color_panel.background { "background" } else { "foreground" };
    if app.run("tools.setColors", json!({(target):c})).is_ok() && target == "foreground" {
        crate::type_tool::foreground_changed(app);
    }
}

fn knob(ui: &egui::Ui, p: egui::Pos2, color: Color32) {
    ui.painter().circle_filled(p, 6.0, color);
    ui.painter().circle_stroke(p, 6.5, Stroke::new(1.5, Color32::WHITE));
    ui.painter().circle_stroke(p, 8.0, Stroke::new(1.0, Color32::from_black_alpha(150)));
}

fn gradient(ui: &egui::Ui, rect: Rect, nx: u32, ny: u32, color: impl Fn(f32, f32) -> Color32) {
    let mut mesh = Mesh::default();
    for y in 0..=ny {
        for x in 0..=nx {
            let (u, v) = (x as f32 / nx as f32, y as f32 / ny as f32);
            mesh.colored_vertex(pos2(rect.left() + u * rect.width(), rect.top() + v * rect.height()), color(u, v));
        }
    }
    for y in 0..ny {
        for x in 0..nx {
            let a = y * (nx + 1) + x;
            mesh.add_triangle(a, a + 1, a + nx + 2);
            mesh.add_triangle(a, a + nx + 2, a + nx + 1);
        }
    }
    ui.painter().add(mesh);
}

fn sv(ui: &mut egui::Ui, r: Rect, h: &mut egui::ecolor::Hsva, id: &str) -> bool {
    gradient(ui, r, 20, 20, |s, v| panels::hsva_color32(egui::ecolor::Hsva::new(h.h, s, 1. - v, 1.)));
    let response = ui.interact(r, ui.id().with(id), Sense::click_and_drag());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, "Saturation / Brightness"));
    let changed = (response.clicked() || response.dragged()) && response.interact_pointer_pos().is_some();
    if changed && let Some(p) = response.interact_pointer_pos() {
        h.s = ((p.x - r.left()) / r.width()).clamp(0., 1.);
        h.v = (1. - (p.y - r.top()) / r.height()).clamp(0., 1.);
    }
    knob(ui, pos2(r.left() + h.s * r.width(), r.top() + (1. - h.v) * r.height()), panels::hsva_color32(*h));
    changed
}

fn hue(ui: &mut egui::Ui, r: Rect, h: &mut egui::ecolor::Hsva, looping: bool) -> bool {
    gradient(ui, r, 1, 36, |_, v| panels::hsva_color32(egui::ecolor::Hsva::new(v, 1., 1., 1.)));
    let response = ui.interact(r, ui.id().with("palette-hue"), Sense::click_and_drag());
    response.widget_info(|| egui::WidgetInfo::slider(true, f64::from(h.h * 360.0), "Hue"));
    let changed = (response.clicked() || response.dragged()) && response.interact_pointer_pos().is_some();
    if changed && let Some(p) = response.interact_pointer_pos() {
        let v = (p.y - r.top()) / r.height();
        h.h = if looping { v.rem_euclid(1.) } else { v.clamp(0., 1.) };
    }
    let y = r.top() + h.h * r.height();
    ui.painter().line_segment([pos2(r.left() - 2., y), pos2(r.right() + 2., y)], Stroke::new(2., Tokens::get(ui.ctx()).text));
    changed
}

fn wheel(ui: &mut egui::Ui, r: Rect, h: &mut egui::ecolor::Hsva) -> bool {
    let center = r.center();
    let outer = r.width().min(r.height()) * 0.5 - 8.;
    let inner = outer * 0.73;
    let mut mesh = Mesh::default();
    let n = 120;
    for i in 0..=n {
        let f = i as f32 / n as f32;
        let a = f * std::f32::consts::TAU;
        let direction = vec2(a.cos(), a.sin());
        let c = panels::hsva_color32(egui::ecolor::Hsva::new(f, 1., 1., 1.));
        mesh.colored_vertex(center + direction * inner, c);
        mesh.colored_vertex(center + direction * outer, c);
    }
    for i in 0..n {
        let a = i * 2;
        mesh.add_triangle(a, a + 1, a + 3);
        mesh.add_triangle(a, a + 3, a + 2);
    }
    ui.painter().add(mesh);
    let response = ui.interact(r, ui.id().with("palette-wheel"), Sense::click_and_drag());
    // Lock the drag to its starting region so the ring never captures an SV drag.
    let key = ui.id().with("palette-wheel-region");
    if response.is_pointer_button_down_on()
        && ui.input(|i| i.pointer.any_pressed())
        && let Some(p) = response.interact_pointer_pos()
    {
        ui.data_mut(|d| d.insert_temp(key, (p - center).length() >= inner));
    }
    let ring = ui.data(|d| d.get_temp::<bool>(key)).unwrap_or(false);
    let mut changed = false;
    if (response.clicked() || response.dragged())
        && ring
        && let Some(p) = response.interact_pointer_pos()
    {
        let p = p - center;
        h.h = (p.y.atan2(p.x) / std::f32::consts::TAU).rem_euclid(1.);
        changed = true;
    }
    let angle = h.h * std::f32::consts::TAU;
    knob(ui, center + vec2(angle.cos(), angle.sin()) * (outer + inner) * 0.5, panels::hsva_color32(egui::ecolor::Hsva::new(h.h, 1., 1., 1.)));
    let side = inner * 1.3;
    let square = Rect::from_center_size(center, vec2(side, side));
    changed |= sv(ui, square, h, "wheel-sv");
    changed
}

// C3's Lab layout is OKLCH (L/C/H), not CIE Lab's L/a/b.
fn to_lch(rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb.map(|v| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) });
    let l = (0.41222146 * r + 0.53633255 * g + 0.051445995 * b).cbrt();
    let m = (0.2119035 * r + 0.6806995 * g + 0.10739696 * b).cbrt();
    let s = (0.08830246 * r + 0.28171885 * g + 0.6299787 * b).cbrt();
    let a = 1.9779985 * l - 2.4285922 * m + 0.4505937 * s;
    let b = 0.025904037 * l + 0.78277177 * m - 0.80867577 * s;
    [0.21045426 * l + 0.7936178 * m - 0.004072047 * s, a.hypot(b), b.atan2(a).to_degrees().rem_euclid(360.)]
}
fn lch_linear([l, c, h]: [f32; 3]) -> [f32; 3] {
    let (a, b) = (c * h.to_radians().cos(), c * h.to_radians().sin());
    let p = (l + 0.39633778 * a + 0.21580376 * b).powi(3);
    let q = (l - 0.105561346 * a - 0.06385417 * b).powi(3);
    let s = (l - 0.08948418 * a - 1.2914855 * b).powi(3);
    [4.0767417 * p - 3.3077116 * q + 0.23096994 * s, -1.268438 * p + 2.6097574 * q - 0.3413194 * s, -0.0041960863 * p - 0.7034186 * q + 1.7076147 * s]
}
fn from_lch(mut lch: [f32; 3]) -> [f32; 3] {
    let in_gamut = |v: [f32; 3]| v.iter().all(|v| (-0.000001..=1.000001).contains(v));
    if !in_gamut(lch_linear(lch)) {
        let (mut lo, mut hi) = (0., lch[1]);
        for _ in 0..24 {
            let c = (lo + hi) * 0.5;
            if in_gamut(lch_linear([lch[0], c, lch[2]])) { lo = c } else { hi = c }
        }
        lch[1] = lo;
    }
    lch_linear(lch).map(|v| if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.max(0.).powf(1. / 2.4) - 0.055 }).map(|v| v.clamp(0., 1.))
}

fn sliders(ui: &mut egui::Ui, h: &mut egui::ecolor::Hsva, lab: bool, looping: bool) -> bool {
    let rgb = panels::hsva_srgb(*h);
    let lch = to_lch([rgb[0], rgb[1], rgb[2]]);
    let mut values = if lab { lch } else { [h.h * 360., h.s * 100., h.v * 100.] };
    let mut changed = false;
    let row_height = (ui.available_height() / 3. - 6.).max(1.);
    ui.spacing_mut().item_spacing.y = 0.;
    for i in 0..3 {
        let (label, max) = if lab { [("L", 1.), ("C", 0.4), ("H", 360.)][i] } else { [("H", 360.), ("S", 100.), ("B", 100.)][i] };
        ui.add_space(6.);
        ui.horizontal(|ui| {
            ui.add_sized(vec2(14., row_height), egui::Label::new(label));
            let (r, response) = ui.allocate_exact_size(vec2(ui.available_width().max(1.), row_height), Sense::click_and_drag());
            gradient(ui, r, 36, 1, |u, _| {
                let mut v = values;
                v[i] = u * max;
                if lab {
                    let c = from_lch(v);
                    Color32::from_rgb((c[0] * 255.).round() as u8, (c[1] * 255.).round() as u8, (c[2] * 255.).round() as u8)
                } else {
                    panels::hsva_color32(if i == 0 {
                        egui::ecolor::Hsva::new(v[0] / 360., 1.0, 1.0, 1.0)
                    } else {
                        egui::ecolor::Hsva::new(v[0] / 360., v[1] / 100., v[2] / 100., 1.)
                    })
                }
            });
            if (response.clicked() || response.dragged())
                && let Some(p) = response.interact_pointer_pos()
            {
                let raw = (p.x - r.left()) / r.width();
                let wraps = if lab { i == 2 } else { i == 0 && looping };
                values[i] = max * if wraps { raw.rem_euclid(1.) } else { raw.clamp(0., 1.) };
                changed = true;
            }
            knob(ui, pos2(r.left() + values[i] / max * r.width(), r.center().y), panels::hsva_color32(*h));
            response.widget_info(|| egui::WidgetInfo::slider(true, f64::from(values[i]), label));
        });
    }
    if changed {
        if lab {
            let c = from_lch(values);
            *h = panels::srgb_hsva([c[0], c[1], c[2], 1.]);
        } else {
            *h = egui::ecolor::Hsva::new(values[0] / 360., values[1] / 100., values[2] / 100., 1.);
        }
    }
    changed
}

fn sources(app: &mut PhotocraftApp, ui: &mut egui::Ui, r: Rect) {
    let t = Tokens::get(ui.ctx());
    let width = r.width();
    let scale = width / 54.;
    let fg = Rect::from_min_size(r.min, vec2(width, width));
    let response = ui.interact(fg, ui.id().with("source-foreground"), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Foreground Color"));
    ui.painter().rect_filled(fg, t.radius_sm, panels::hsva_color32(panels::srgb_hsva(app.session.tools.foreground)));
    ui.painter().rect_stroke(fg, t.radius_sm, Stroke::new(if app.ui.color_panel.background { 1. } else { 2. }, t.accent), StrokeKind::Inside);
    if response.clicked() {
        app.ui.color_panel.background = false;
    }
    if response.double_clicked() {
        crate::color_picker_ui::open(app, "foreground");
    }
    let bg = Rect::from_min_size(r.min + vec2(0., width + 4. * scale), vec2(width, 18. * scale));
    let response = ui.interact(bg, ui.id().with("source-background"), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Background Color"));
    ui.painter().rect_filled(bg, t.radius_sm, crate::panels::c32(app.session.tools.background));
    ui.painter().rect_stroke(bg, t.radius_sm, Stroke::new(if app.ui.color_panel.background { 2. } else { 1. }, t.field_border), StrokeKind::Inside);
    if response.clicked() {
        app.ui.color_panel.background = true;
    }
    if response.double_clicked() {
        crate::color_picker_ui::open(app, "background");
    }
    let c = panels::srgb_hsva(app.session.tools.foreground);
    let complement = egui::ecolor::Hsva::new((c.h + 0.5).rem_euclid(1.), c.s, c.v, 1.);
    let comp = Rect::from_min_size(pos2(r.left(), bg.bottom() + 4. * scale), vec2(width, 18. * scale));
    ui.painter().rect_filled(comp, t.radius_sm, panels::hsva_color32(complement));
    if ui.interact(comp, ui.id().with("source-complement"), Sense::click()).on_hover_text(tl!("Complementary Color")).clicked() {
        app.ui.color_panel.background = false;
        set_color(app, panels::hsva_srgb(complement));
    }
    let size = (width - 5. * scale) * 0.5;
    for (i, c) in app.ui.color_panel.recent.into_iter().enumerate() {
        let recent = Rect::from_min_size(
            pos2(r.left() + (i % 2) as f32 * (size + 5. * scale), comp.bottom() + 4. * scale + (i / 2) as f32 * 23. * scale),
            vec2(size, 18. * scale),
        );
        ui.painter().rect_filled(recent, t.radius_sm, if c[3] > 0. { crate::panels::c32(c) } else { t.field });
        ui.painter().rect_stroke(recent, t.radius_sm, Stroke::new(1., t.field_border), StrokeKind::Inside);
        if c[3] > 0. && ui.interact(recent, ui.id().with(("recent", i)), Sense::click()).clicked() {
            set_color(app, c);
        }
    }
}

pub(crate) fn show(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let bg = app.ui.color_panel.background;
    let color = if bg { app.session.tools.background } else { app.session.tools.foreground };
    let key = egui::Id::new(("color-field-hue", bg));
    let raw = panels::srgb_hsva(color);
    let remembered = ui.data(|d| d.get_temp::<(f32, f32)>(key)).unwrap_or((raw.h, raw.s));
    let mut h = remember_hsva(raw, remembered, app.session.prefs().painting.saturation_memory / 100.);
    // C2's 54 : 8 : 168 : 8 : 25 geometry scales together, never with dock height.
    let width = ui.available_width().max(1.);
    let scale = width / 263.;
    let left = 54. * scale;
    let side = 168. * scale;
    let (rect, response) = ui.allocate_exact_size(vec2(width, side), Sense::hover());
    sources(app, ui, Rect::from_min_size(rect.min, vec2(left, side)));
    let picker = Rect::from_min_size(rect.min + vec2(left + 8. * scale, 0.), vec2(width - left - 8. * scale, side));
    let mode = app.ui.color_panel.mode;
    let looping = app.session.prefs().painting.hue_slider_loop_enabled;
    let mut changed = false;
    match mode {
        PaletteMode::HueCube => {
            let sw = 25. * scale;
            let square = Rect::from_min_size(picker.min, vec2(side, side));
            changed |= sv(ui, square, &mut h, "palette-sv");
            changed |= hue(ui, Rect::from_min_size(pos2(square.right() + 8. * scale, square.top()), vec2(sw, square.height())), &mut h, looping);
        }
        PaletteMode::ColorWheel => changed |= wheel(ui, picker, &mut h),
        PaletteMode::Hsb | PaletteMode::Lab => {
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(picker));
            changed |= sliders(&mut child, &mut h, mode == PaletteMode::Lab, looping);
        }
    }
    if changed {
        set_color(app, panels::hsva_srgb(h));
    }
    if changed || (raw.s > 0.001 && (raw.v > 0.04 || raw.s > remembered.1)) {
        ui.data_mut(|d| d.insert_temp(key, (h.h, h.s)));
    }
    // Store completed selections, not every intermediate colour along a drag.
    if !ui.input(|i| i.pointer.any_down()) {
        let current = if app.ui.color_panel.background { app.session.tools.background } else { app.session.tools.foreground };
        let recent = &mut app.ui.color_panel.recent;
        if current[3] > 0. && panels::srgb_bytes(recent[0]) != panels::srgb_bytes(current) {
            recent.rotate_right(1);
            recent[0] = current;
        }
    }
    image_palette(app, ui);
    if app.ui.color_panel.show_values {
        ui.add_space(6.);
        let bg = app.ui.color_panel.background;
        let mut c = if bg { app.session.tools.background } else { app.session.tools.foreground };
        if panels::color_readout(ui, ui.id().with(("color-field", bg)), &mut c) {
            set_color(app, c);
            let h = panels::srgb_hsva(c);
            if h.s > 0.001 && h.v > 0.04 {
                ui.data_mut(|d| d.insert_temp(key, (h.h, h.s)));
            }
        }
    }
    let popup = egui::Popup::context_menu(&response);
    let open = ui.input(|i| {
        i.pointer.button_clicked(egui::PointerButton::Secondary)
            && i.pointer.interact_pos().is_some_and(|p| Rect::from_min_max(rect.min, ui.min_rect().max).contains(p))
    });
    popup.open_memory(open.then_some(egui::SetOpenCommand::Bool(true))).show(|ui| menu(app, ui));
}

pub(crate) fn remember_hsva(mut h: egui::ecolor::Hsva, remembered: (f32, f32), strength: f32) -> egui::ecolor::Hsva {
    if h.s < 0.01 {
        h.h = remembered.0;
    }
    if h.v < 0.04 && h.s < remembered.1 {
        let weight = (1. - h.v / 0.04).clamp(0., 1.) * strength;
        h.s += (remembered.1 - h.s) * weight;
    }
    h
}

fn image_palette(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let Some(st) = app.session.active() else { return };
    let key = (st.doc.id, st.revision);
    let id = egui::Id::new("image-palette-cache");
    let cached = ui.data(|d| d.get_temp::<((photocraft_doc::DocId, u64), Vec<[f32; 4]>)>(id));
    let colors = if let Some((_, v)) = cached.filter(|(k, _)| *k == key) {
        v
    } else {
        let image = photocraft_compose::thumbnail(&st.doc, 96);
        let colors = extract_colors(&image.pixels);
        ui.data_mut(|d| d.insert_temp(id, (key, colors.clone())));
        colors
    };
    let scale = ui.available_width() / 263.;
    ui.add_space(6. * scale);
    let t = Tokens::get(ui.ctx());
    let size = ((ui.available_width() - 9. * 2. * scale) / 10.).max(0.);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2. * scale;
        for i in 0..10 {
            let (r, response) = ui.allocate_exact_size(vec2(size, 22. * scale), Sense::click());
            ui.painter().rect_filled(r, t.radius_sm, colors.get(i).copied().map(crate::panels::c32).unwrap_or(t.field));
            ui.painter().rect_stroke(r, t.radius_sm, Stroke::new(1., t.field_border), StrokeKind::Inside);
            if response.on_hover_text(tl!("Image Palette Color")).clicked()
                && let Some(c) = colors.get(i)
            {
                set_color(app, *c);
            }
        }
    });
}

fn extract_colors(pixels: &[u8]) -> Vec<[f32; 4]> {
    let mut bins = std::collections::BTreeMap::<(i32, i32, i32), ([f32; 4], u32, [f32; 3])>::new();
    for p in pixels.chunks_exact(4).step_by((pixels.len() / 4 / 220).max(1)) {
        if p[3] < 21 {
            continue;
        }
        let rgb = [f32::from(p[0]) / 255., f32::from(p[1]) / 255., f32::from(p[2]) / 255.];
        let hsv = crate::color_picker_ui::rgb_to_hsv(rgb);
        let h = [hsv[0] / 360., hsv[1], hsv[2]];
        let key = ((h[0] * 24.).round() as i32, (h[1] * 8.).round() as i32, (h[2] * 8.).round() as i32);
        let entry = bins.entry(key).or_insert(([rgb[0], rgb[1], rgb[2], 1.], 0, h));
        entry.1 += 1;
    }
    let mut candidates: Vec<_> = bins.into_values().collect();
    candidates.sort_by_key(|v| std::cmp::Reverse(v.1));
    let mut selected = Vec::<([f32; 4], u32, [f32; 3])>::new();
    while !candidates.is_empty() && selected.len() < 10 {
        let score = |v: &([f32; 4], u32, [f32; 3])| {
            let nearest = selected
                .iter()
                .map(|p| {
                    let h = (p.2[0] - v.2[0]).abs();
                    h.min(1. - h) * 0.56 + (p.2[2] - v.2[2]).abs() * 0.32 + (p.2[1] - v.2[1]).abs() * 0.12
                })
                .reduce(f32::min)
                .unwrap_or(0.);
            (v.1 as f32 + 1.).ln() * 0.42 + v.2[1] * 0.18 + v.2[2] * 0.12 + nearest * 2.4
        };
        let at = candidates.iter().enumerate().max_by(|(_, a), (_, b)| score(a).total_cmp(&score(b))).map_or(0, |(i, _)| i);
        selected.push(candidates.remove(at));
    }
    selected.into_iter().map(|v| v.0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn near_black_sampling_retains_saturation_without_changing_sampled_rgb() {
        let raw = panels::srgb_hsva([0.001, 0.001, 0.001, 1.]);
        let remembered = remember_hsva(raw, (0.64, 0.92), 1.);
        assert!(remembered.s > 0.89);
        assert_eq!(remembered.h, 0.64);
        assert_eq!(remember_hsva(raw, (0.64, 0.92), 0.).s, raw.s);
        let saturated = egui::ecolor::Hsva::new(0.2, 0.97, 0.002, 1.);
        assert_eq!(remember_hsva(saturated, (0.64, 0.92), 1.).s, 0.97);
        assert_eq!(remember_hsva(saturated, (0.64, 0.92), 1.).h, 0.2);
    }

    #[test]
    fn lch_layout_preserves_rgb_and_maps_chroma_to_gamut() {
        for rgb in [[1., 0., 0.], [0.2, 0.6, 0.8], [0.4, 0.4, 0.4], [1., 1., 1.]] {
            let back = from_lch(to_lch(rgb));
            for (a, b) in rgb.into_iter().zip(back) {
                assert!((a - b).abs() < 0.002, "{rgb:?} => {back:?}");
            }
        }
        let rgb = from_lch([0.7, 0.4, 120.]);
        assert!(rgb.into_iter().all(|v| (0.0..=1.0).contains(&v)));
    }

    #[test]
    fn image_palette_skips_transparency_and_keeps_distinct_colors() {
        let colors = extract_colors(&[255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 255, 0, 0]);
        assert_eq!(colors.len(), 2);
        assert!(colors.contains(&[1., 0., 0., 1.]));
        assert!(colors.contains(&[0., 0., 1., 1.]));
    }

    fn harness(mode: PaletteMode) -> Harness<'static, PhotocraftApp> {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.ui.color_panel.mode = mode;
        app.session.tools.foreground = [1., 0., 0., 1.];
        let mut h = Harness::builder().with_size(vec2(320., 400.)).build_ui_state(
            |ui, app: &mut PhotocraftApp| {
                if ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                    show(app, ui);
                }
            },
            app,
        );
        PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::Studio);
        h.run_steps(3);
        h
    }

    #[test]
    fn right_click_switches_layout_and_values_without_recoloring() {
        let mut h = harness(PaletteMode::HueCube);
        let p = pos2(150., 80.);
        h.hover_at(p);
        h.run_steps(1);
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Secondary, pressed, modifiers: egui::Modifiers::NONE });
            h.run_steps(1);
        }
        h.run_steps(3);
        h.get_by_label("Color Wheel").click();
        h.run_steps(2);
        assert_eq!(h.state().ui.color_panel.mode, PaletteMode::ColorWheel);
        assert_eq!(h.state().session.tools.foreground, [1., 0., 0., 1.]);
        assert!(h.query_by_role(egui::accesskit::Role::TextInput).is_some());
        let p = h.get_by_role(egui::accesskit::Role::TextInput).rect().center();
        h.hover_at(p);
        h.run_steps(1);
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Secondary, pressed, modifiers: egui::Modifiers::NONE });
            h.run_steps(1);
        }
        h.run_steps(3);
        h.get_by_label("Show Color Values").click();
        h.run_steps(2);
        assert!(!h.state().ui.color_panel.show_values);
        assert!(h.query_by_role(egui::accesskit::Role::TextInput).is_none());
        let p = pos2(150.0, 80.0);
        h.hover_at(p);
        h.run_steps(1);
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Secondary, pressed, modifiers: egui::Modifiers::NONE });
            h.run_steps(1);
        }
        h.run_steps(3);
        h.get_by_label("Show Color Values").click();
        h.run_steps(2);
        assert!(h.state().ui.color_panel.show_values);
        assert!(h.query_by_role(egui::accesskit::Role::TextInput).is_some());
    }

    #[test]
    fn external_near_black_sample_keeps_picker_position_until_brightness_changes() {
        let mut h = harness(PaletteMode::HueCube);
        let sample = [0.001, 0.001, 0.001, 1.0];
        h.state_mut().run("tools.setColors", json!({"foreground":sample})).unwrap();
        h.run_steps(3);
        assert_eq!(h.state().session.tools.foreground, sample);
        let r = h.get_by_label("Saturation / Brightness").rect();
        let p = pos2(r.right() - 1.0, r.center().y);
        h.hover_at(p);
        h.run_steps(1);
        h.drag_at(p);
        h.run_steps(1);
        h.drop_at(p);
        h.run_steps(2);
        let rgb = h.state().session.tools.foreground;
        assert!(rgb[0] > 0.4 && rgb[1] < 0.02 && rgb[2] < 0.02, "{rgb:?}");
    }

    #[test]
    fn every_layout_changes_the_selected_color() {
        for (mode, _) in MODES {
            let mut h = harness(mode);
            let r = match mode {
                PaletteMode::HueCube | PaletteMode::ColorWheel => h.get_by_label("Saturation / Brightness").rect(),
                PaletteMode::Hsb => h.query_all_by_role(egui::accesskit::Role::Slider).nth(2).unwrap().rect(),
                PaletteMode::Lab => h.query_all_by_role(egui::accesskit::Role::Slider).next().unwrap().rect(),
            };
            let p = r.center();
            h.hover_at(p);
            h.run_steps(1);
            h.drag_at(p);
            h.run_steps(1);
            h.drop_at(p);
            h.run_steps(2);
            let rgb = h.state().session.tools.foreground;
            assert_ne!(rgb, [1.0, 0.0, 0.0, 1.0], "{mode:?}");
            assert!(rgb.into_iter().all(|v| v.is_finite() && (0.0..=1.0).contains(&v)));
        }
    }

    #[test]
    fn color_panel_geometry_is_stable_across_theme_values_and_dock_height() {
        for width in [260., 280., 320.] {
            let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
            app.run("file.new", json!({"width":8,"height":8})).unwrap();
            let mut h = Harness::builder().with_size(vec2(width, 600.)).build_ui_state(
                |ui, app: &mut PhotocraftApp| {
                    if ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                        ui.spacing_mut().scroll.floating_allocated_width = 0.;
                        egui::ScrollArea::vertical()
                            .max_height(if app.ui.color_panel.background { 200. } else { 560. })
                            .auto_shrink([false, false])
                            .show(ui, |ui| show(app, ui));
                    }
                },
                app,
            );
            for theme in [crate::theme::ThemeKind::Studio, crate::theme::ThemeKind::StudioLight] {
                PhotocraftApp::setup_context(&h.ctx, theme);
                for (mode, _) in MODES {
                    h.state_mut().ui.color_panel.mode = mode;
                    let mut source = None;
                    for (values, short) in [(false, false), (true, false), (true, true), (false, true)] {
                        h.state_mut().ui.color_panel.show_values = values;
                        h.state_mut().ui.color_panel.background = short;
                        h.run_steps(16);
                        let rect = h.get_by_label("Foreground Color").rect();
                        if let Some(previous) = source {
                            assert_eq!(rect, previous, "{theme:?} {mode:?}: values/height changed source size");
                        }
                        source = Some(rect);
                        let tracks: Vec<_> = h.query_all_by_role(egui::accesskit::Role::Slider).map(|n| n.rect()).collect();
                        assert!(!tracks.is_empty());
                        if matches!(mode, PaletteMode::Hsb | PaletteMode::Lab) {
                            assert_eq!(h.query_all_by_role(egui::accesskit::Role::SpinButton).count(), if values { 3 } else { 0 });
                        }
                        for _ in 0..8 {
                            h.run_steps(1);
                            assert_eq!(h.get_by_label("Foreground Color").rect(), rect);
                            assert_eq!(
                                h.query_all_by_role(egui::accesskit::Role::Slider).map(|n| n.rect()).collect::<Vec<_>>(),
                                tracks,
                                "{theme:?} {mode:?}: idle layout moved"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn switching_all_modes_is_idle_and_state_roundtrips() {
        let mut h = harness(PaletteMode::HueCube);
        for (mode, _) in MODES {
            h.state_mut().ui.color_panel.mode = mode;
            h.run_steps(2);
            assert_eq!(h.state().session.tools.foreground, [1., 0., 0., 1.]);
        }
        let state = h.state().ui.color_panel;
        let restored: crate::state::ColorPanelState = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
        assert_eq!(state, restored);
    }
}
