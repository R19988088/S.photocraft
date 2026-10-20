//! Window › Brush Settings (F5) and Window › Brushes: Photoshop's floating brush panel.
//!
//! Brush Settings: the section list with enable boxes on the left (clicking a controls section
//! shows and enables it; Build-up, Smoothing and Protect Texture only toggle), the selected section's controls on the right (see [`crate::brush_sections`]),
//! and a live stroke preview strip below, rendered with the real brush engine and cached on the
//! settings (it re-renders only when they change). Brushes: the presets in collapsible groups with
//! tip thumbnails and stroke previews, a size slider and a search field.
//!
//! The panel never writes the session brush itself: each frame's edits become one
//! `tools.setBrush` call carrying only the changed fields (Rule 1), so the journal, the control
//! channel and the MCP server see exactly what the panel did and can do the same. A drag's calls
//! share a `coalesce` key, so a whole gesture is one journal entry ([`commit_gesture`]).
//!
//! The lock beside a section name keeps that section's settings when another preset is picked
//! (`locks` in the brush, applied by the engine). The Brushes tab lives in [`crate::brushes_tab`].

use egui::{Color32, CornerRadius, Sense, Stroke, vec2};
use photocraft_engine::BrushSettings;
use photocraft_engine::paint;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::brush_preview;
use crate::theme::{self, Tokens};
use crate::{PhotocraftApp, icons, widgets};

pub use crate::brush_preview::{preview_pixels, preview_sig};
pub use crate::brush_sections::section_body;
pub use photocraft_engine::brush_cmds::brush_patch;

/// The eight brush-setting categories mirrored by the reference panel.
pub const SECTIONS: [(&str, bool); 8] =
    [("Tip", false), ("Basic", false), ("Motion", true), ("Texture", true), ("Color Mode", true), ("Wet Edge", true), ("Mix", true), ("Pigment", true)];

pub(crate) fn section_label(name: &str) -> &str {
    crate::i18n::tr_ctx(crate::i18n::current(), "brushSection", name)
}

pub(crate) fn section_visible(section: usize) -> bool {
    section < SECTIONS.len()
}

/// Panel width and the stroke strip's size.
pub(crate) const WIDTH: f32 = 540.0;
const STRIP: (u32, u32) = (488, 76);

/// How the Brushes tab lists presets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BrushesView {
    /// Rows: tip, size, stroke preview, name.
    #[default]
    List,
    /// Tip thumbnails with their sizes.
    Grid,
}

/// A rename in progress in the Brushes tab (a preset, or a group when `group` is set).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Renaming {
    pub group: bool,
    /// The preset (or group) being renamed.
    pub name: String,
    /// With `group`: the nested folder of group `name` being renamed (empty = the group itself).
    pub folder: Vec<String>,
    /// The text typed so far.
    pub text: String,
}

/// Brushes panel view state (serde, so the control channel can read and drive it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BrushesPanelState {
    /// Collapsed groups and folders: a group's label, or for a nested folder the group's label and
    /// the folder path joined with `/` ([`crate::brushes_tab::folder_view_key`]).
    pub collapsed: Vec<String>,
    /// Case-insensitive name filter.
    pub filter: String,
    pub view: BrushesView,
    pub renaming: Option<Renaming>,
    /// The Brush Preset picker's cards: show the preset's name. At least one of the three parts
    /// is always on, and the picker's cards never go empty (see [`crate::brush_picker`]).
    pub show_name: bool,
    /// The Brush Preset picker's cards: show the stroke preview.
    pub show_stroke: bool,
    /// The Brush Preset picker's cards: show the tip thumbnail.
    pub show_tip: bool,
    /// The Brush Preset picker's cards: their size scale, from the picker's footer slider
    /// (`crate::brush_picker::body`). 1 is the standard size; the cards' width, height and
    /// insides multiply by it (`crate::brushes_tab`) — the height down to a 0.50 floor, the
    /// width all the way — the text holds its 1.0 size, and at or below the compact threshold
    /// the tips drop the size number under them and take their cell whole.
    pub scale: f32,
}

impl Default for BrushesPanelState {
    fn default() -> Self {
        Self {
            collapsed: Vec::new(),
            filter: String::new(),
            view: BrushesView::default(),
            renaming: None,
            show_name: true,
            show_stroke: true,
            show_tip: true,
            scale: 1.0,
        }
    }
}

/// The enable flag behind section `i` (None for Brush Tip Shape).
pub fn section_flag(b: &mut BrushSettings, i: usize) -> Option<&mut bool> {
    Some(match i {
        2 => &mut b.motion.enabled,
        3 => &mut b.texture.enabled,
        5 => &mut b.wet_edges,
        6 => &mut b.mix_settings.enabled,
        7 => &mut b.pigment.as_mut()?.enabled,
        _ => return None,
    })
}

fn section_enabled(b: &BrushSettings, section: usize) -> bool {
    match section {
        1 => b.shape_dynamics.enabled || b.scattering.enabled || b.transfer.enabled,
        2 => b.motion.enabled,
        3 => b.texture.enabled,
        4 => b.color_attributes.enabled || b.color_dynamics.enabled,
        5 => b.wet_edges,
        6 => b.mix_settings.enabled,
        7 => b.pigment.as_ref().is_some_and(|p| p.enabled),
        _ => false,
    }
}

fn set_section_enabled(b: &mut BrushSettings, section: usize, enabled: bool) {
    match section {
        1 => {
            b.shape_dynamics.enabled = enabled;
            b.scattering.enabled = enabled;
            b.transfer.enabled = enabled;
        }
        4 => {
            b.color_attributes.enabled = enabled;
            b.color_dynamics.enabled = enabled;
        }
        7 => b.pigment.get_or_insert_with(Default::default).enabled = enabled,
        _ => {
            if let Some(flag) = section_flag(b, section) {
                *flag = enabled;
            }
        }
    }
}

/// The lock behind section `i` (every section but Brush Tip Shape has one, as in Photoshop).
pub fn section_lock(b: &mut BrushSettings, i: usize) -> Option<&mut bool> {
    let _ = (b, i);
    None
}

/// Send the panel's edits (`before` → `after`) through `tools.setBrush`.
pub fn commit(app: &mut PhotocraftApp, before: &BrushSettings, after: &BrushSettings) {
    send(app, before, after, None);
}

fn send(app: &mut PhotocraftApp, before: &BrushSettings, after: &BrushSettings, key: Option<String>) -> bool {
    if before == after {
        return false;
    }
    let patch = brush_patch(before, after);
    if patch.as_object().is_some_and(Map::is_empty) {
        return false;
    }
    let mut p = json!({ "brush": patch });
    if let Some(k) = key {
        p["coalesce"] = json!(k);
    }
    if let Err(e) = app.run("tools.setBrush", p) {
        app.ui.status = e;
    }
    true
}

const GESTURE: &str = "brush-set-gesture";

/// The current brush-edit gesture number. A pointer press starts a new gesture (once per frame);
/// [`commit_gesture`] ends one after an edit made with the pointer up (a click, a typed value).
fn gesture(ctx: &egui::Context) -> u64 {
    let frame = ctx.cumulative_pass_nr();
    let pressed = ctx.input(|i| i.pointer.any_pressed());
    ctx.data_mut(|d| {
        let g = d.get_temp_mut_or_default::<(u64, u64)>(egui::Id::new(GESTURE));
        if pressed && g.1 != frame {
            g.0 += 1;
            g.1 = frame;
        }
        g.0
    })
}

/// [`commit`] as part of a gesture: every call of one drag carries the same `coalesce` key, so
/// the engine journals the drag as a single `tools.setBrush` (Rule 1: one command per gesture).
/// Call it every frame (it tracks presses even when nothing changed).
pub fn commit_gesture(app: &mut PhotocraftApp, ctx: &egui::Context, before: &BrushSettings, after: &BrushSettings) {
    let g = gesture(ctx);
    if send(app, before, after, Some(format!("brush-ui:{g}"))) && !ctx.input(|i| i.pointer.any_down()) {
        ctx.data_mut(|d| d.get_temp_mut_or_default::<(u64, u64)>(egui::Id::new(GESTURE)).0 += 1);
    }
}

/// Group label for presets saved without a group.
pub const UNGROUPED: &str = "My Brushes";

/// Preset indices in panel order: groups in order of first appearance.
pub fn grouped_presets(presets: &[paint::BrushPreset]) -> Vec<(String, Vec<usize>)> {
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, p) in presets.iter().enumerate() {
        let g = if p.group.is_empty() { UNGROUPED } else { p.group.as_str() };
        match out.iter_mut().find(|(n, _)| n == g) {
            Some((_, v)) => v.push(i),
            None => out.push((g.to_string(), vec![i])),
        }
    }
    out
}

/// A fresh "Brush N" name.
pub(crate) fn new_preset_name(presets: &[paint::BrushPreset]) -> String {
    (1..).map(|n| format!("Brush {n}")).find(|n| !presets.iter().any(|p| &p.name == n)).unwrap_or_else(|| "Brush".into())
}

pub(crate) fn run_or_status(app: &mut PhotocraftApp, id: &str, p: Value) {
    if let Err(e) = app.run(id, p) {
        app.ui.status = e;
    }
}

pub(crate) fn full_uv() -> egui::Rect {
    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0))
}

/// Clicking a controls section shows and enables it; the final five rows only toggle, keeping
/// the current controls open. The lock on the right is a separate hit target.
fn section_list(app: &mut PhotocraftApp, ui: &mut egui::Ui, b: &mut BrushSettings) {
    let t = Tokens::get(ui.ctx());
    ui.vertical(|ui| {
        ui.set_width(150.0);
        ui.spacing_mut().item_spacing.y = 1.0;
        let (br_rect, br_resp) = ui.allocate_exact_size(vec2(150.0, 23.0), Sense::click());
        if br_resp.hovered() {
            ui.painter().rect_filled(br_rect, t.radius_sm, t.hover);
        }
        let icon_rect = egui::Rect::from_center_size(egui::pos2(br_rect.left() + 12.0, br_rect.center().y), vec2(13.0, 13.0));
        icons::paint(ui, icon_rect, "brush", 12.0, if br_resp.hovered() { t.text } else { t.icon });
        let galley = ui.painter().layout_no_wrap(tl!("Brushes").to_string(), theme::semibold(12.0), if br_resp.hovered() { t.text } else { t.text_dim });
        ui.painter().galley(egui::pos2(br_rect.left() + 26.0, br_rect.center().y - galley.size().y / 2.0), galley, t.text);
        if br_resp.clicked() {
            let _ = crate::menus::invoke(app, ui.ctx(), "window.panel.brushes", json!({}));
        }
        ui.add_space(3.0);
        widgets::hairline(ui);
        ui.add_space(3.0);
        for (i, (name, has_box)) in SECTIONS.iter().enumerate() {
            if app.ui.tool == crate::Tool::Eraser && matches!(i, 4 | 6) {
                continue;
            }
            let sel = app.ui.brush_section == i;
            let (r, resp) = ui.allocate_exact_size(vec2(150.0, 23.0), Sense::click());
            resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, sel, section_label(name)));
            if sel {
                ui.painter().rect_filled(r, t.radius_sm, t.accent_soft);
            } else if resp.hovered() {
                ui.painter().rect_filled(r, t.radius_sm, t.hover);
            }
            let mut x = r.left() + 7.0;
            if *has_box {
                let box_rect = egui::Rect::from_center_size(egui::pos2(x + 6.0, r.center().y), vec2(13.0, 13.0));
                let enabled = section_enabled(b, i);
                let box_resp = ui.interact(box_rect, ui.id().with(("brush-sec-box", i)), Sense::click());
                box_resp.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::Checkbox,
                        true,
                        enabled,
                        crate::i18n::fmt(tl!("Enable {name}"), &[("name", section_label(name))]),
                    )
                });
                widgets::check_box(ui.painter(), box_rect, enabled, box_resp.hovered(), box_resp.is_pointer_button_down_on(), &t);
                if box_resp.clicked() {
                    set_section_enabled(b, i, !enabled);
                }
                x += 20.0;
            }
            let color = if sel { t.text } else { t.text_dim };
            let font = if i == 0 { theme::semibold(12.0) } else { theme::medium(12.0) };
            let mut job = egui::text::LayoutJob::simple_singleline(section_label(name).to_string(), font, color);
            job.wrap = egui::text::TextWrapping::truncate_at_width((r.right() - 22.0 - x).max(0.0));
            let galley = ui.painter().layout_job(job);
            let elided = galley.elided;
            ui.painter().galley(egui::pos2(x, r.center().y - galley.size().y / 2.0), galley, color);
            let resp = if elided { resp.on_hover_text(section_label(name)) } else { resp };
            if resp.clicked() {
                let on_checkbox = *has_box && resp.interact_pointer_pos().is_some_and(|p| p.x < r.left() + 24.0);
                if !on_checkbox {
                    if *has_box && !section_enabled(b, i) {
                        set_section_enabled(b, i, true);
                    }
                    app.ui.brush_section = i;
                }
            }
        }
    });
}

fn settings_tab(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let before = app.session.tools.brush.clone();
    let mut brush = before.clone();
    crate::brush_sections::dual_brush_header(ui, &mut brush, &mut app.ui.brush_lane);
    commit_gesture(app, ui.ctx(), &before, &brush);
    ui.add_space(6.0);
    ui.push_id(("brush-lane", app.ui.brush_lane), |ui| settings_lane(app, ui));
}

fn brush_with_secondary(parent: &BrushSettings, secondary: &BrushSettings) -> BrushSettings {
    let mut brush = parent.clone();
    let dual = &mut brush.dual_brush;
    dual.tip = secondary.tip.clone();
    dual.size = secondary.size;
    dual.hardness = secondary.hardness;
    dual.roundness = secondary.roundness;
    dual.angle = secondary.angle;
    dual.spacing = secondary.spacing;
    dual.opacity = secondary.opacity;
    dual.flow = secondary.flow;
    dual.grain = secondary.grain.clone();
    dual.wet_edges = secondary.wet_edges;
    dual.wet_edge = secondary.wet_edge.clone();
    dual.full_parameters = Some(Box::new(secondary.clone()));
    brush
}

fn settings_lane(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    let before = app.session.tools.brush.clone();
    let secondary = app.ui.brush_lane == 1;
    if secondary {
        std::mem::swap(&mut app.ui.brush_section, &mut app.ui.brush_secondary_section);
    }
    let mut b = if secondary { crate::brush_sections::secondary_defaults(&before) } else { before.clone() };
    let lane_before = secondary.then(|| b.clone());
    let mut secondary_flip = before.dual_brush.flip;
    // Older UI state and control clients could select any of the previous section entries.
    if !section_visible(app.ui.brush_section) || (app.ui.tool == crate::Tool::Eraser && matches!(app.ui.brush_section, 4 | 6)) {
        app.ui.brush_section = 0;
    }
    let section = app.ui.brush_section;
    ui.horizontal_top(|ui| {
        section_list(app, ui, &mut b);
        widgets::vline(ui, 482.0);
        ui.vertical(|ui| {
            ui.set_width(WIDTH - 190.0);
            // No title above the options (#3477): the selected row in the list names them, as
            // in Photoshop, and the line is better spent on the options.
            let on = !SECTIONS[section].1 || section_enabled(&b, section);
            egui::ScrollArea::vertical().id_salt(("brush-section", section)).max_height(450.0).auto_shrink([false, false]).show(ui, |ui| {
                ui.set_width(WIDTH - 204.0);
                // A section that's off shows its options greyed out (Photoshop). A tip picked in
                // the grid copies the tip fields only — the dynamics sections stay (Photoshop).
                // The picked preset is recorded by name (`brush.presets.setCurrent`, identity, so
                // look-alike duplicates stay distinct), since the engine can't infer it from the
                // brush afterwards.
                ui.add_enabled_ui(on, |ui| {
                    if secondary && section == 0 {
                        widgets::checkbox(ui, &mut secondary_flip, tl!("Flip"));
                    }
                    crate::brush_sections::section_body_with_canvas(
                        ui,
                        &mut b,
                        section,
                        &app.session.tools.presets,
                        app.session.tools.current_preset.as_deref(),
                        &app.session.tools.canvas_presets,
                    )
                })
                .inner
                .inspect(|name| {
                    if !secondary {
                        run_or_status(app, "brush.presets.setCurrent", json!({ "name": name }));
                    }
                });
            });
        });
    });
    ui.add_space(6.0);
    widgets::hairline(ui);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(STRIP.0 as f32, STRIP.1 as f32), Sense::hover());
        ui.painter().rect_filled(r, t.radius_sm, t.field);
        let tex = brush_preview::stroke_texture(ui.ctx(), "settings-strip", &b, STRIP.0, STRIP.1, t.text);
        ui.painter().image(tex.id(), r, full_uv(), Color32::WHITE);
        ui.vertical(|ui| {
            let new = icons::button(ui, "square-plus", 24.0, false, tl!("Create new brush from these settings"));
            if crate::brush_picker::named(new, "Create new brush from these settings").clicked() {
                let preset = if secondary { brush_with_secondary(&before, &b) } else { b.clone() };
                crate::brush_preset_dialog::open(app, Some(&preset));
            }
            if icons::button(ui, "undo-2", 24.0, false, tl!("Reset the brush to the defaults")).clicked() {
                b = BrushSettings { color: b.color, background: b.background, smoothing: b.smoothing.clone(), locks: b.locks.clone(), ..Default::default() };
            }
            // Overwrite the preset the current brush was picked from (Photoshop's "update the
            // selected brush"): no new preset, the brush keeps its place in the list.
            let current = app.session.tools.current_preset.clone().filter(|n| app.session.tools.presets.iter().any(|p| p.name.eq_ignore_ascii_case(n)));
            let update =
                ui.add_enabled_ui(current.is_some(), |ui| icons::button(ui, "check", 24.0, false, tl!("Update the current brush with these settings"))).inner;
            update.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, current.is_some(), "Update the current brush with these settings"));
            if update.clicked() {
                let preset = if secondary { brush_with_secondary(&before, &b) } else { b.clone() };
                run_or_status(app, "brush.presets.update", json!({ "brush": serde_json::to_value(&preset).unwrap_or(Value::Null) }));
            }
        });
    });
    if secondary {
        std::mem::swap(&mut app.ui.brush_section, &mut app.ui.brush_secondary_section);
        if lane_before.as_ref().is_some_and(|previous| *previous != b) || secondary_flip != before.dual_brush.flip {
            let mut parent = brush_with_secondary(&before, &b);
            parent.dual_brush.flip = secondary_flip;
            commit_gesture(app, ui.ctx(), &before, &parent);
        }
    } else {
        commit_gesture(app, ui.ctx(), &before, &b);
    }
}

/// Clear brush controls: turns off every section except Smoothing and resets their options
/// to the defaults (Photoshop), keeping locked sections intact.
pub fn clear_brush_controls(current: &BrushSettings) -> BrushSettings {
    let mut b = current.clone();
    let d = BrushSettings::default();
    let l = &current.locks;
    if !l.shape_dynamics {
        b.shape_dynamics = d.shape_dynamics;
    }
    if !l.scattering {
        b.scattering = d.scattering;
    }
    if !l.texture {
        b.texture = d.texture;
    }
    if !l.dual_brush {
        b.dual_brush = d.dual_brush;
    }
    if !l.color_dynamics {
        b.color_dynamics = d.color_dynamics;
    }
    if !l.transfer {
        b.transfer = d.transfer;
    }
    if !l.pose {
        b.pose = d.pose;
    }
    if !l.noise {
        b.noise = d.noise;
        b.grain = d.grain;
    }
    if !l.wet_edges {
        b.wet_edges = d.wet_edges;
        b.wet_edge = d.wet_edge;
    }
    if !l.build_up {
        b.build_up = d.build_up;
        b.build_up_rate = d.build_up_rate;
    }
    if !l.protect_texture {
        b.protect_texture = d.protect_texture;
    }
    b.pigment = d.pigment;
    b.mix_settings = d.mix_settings;
    b.color_attributes = d.color_attributes;
    b.hardness_dynamic = d.hardness_dynamic;
    b.build_up = false;
    b
}

/// Copy the current brush's Texture settings to other painting tools that have Texture enabled.
pub fn copy_texture_to_other_tools(app: &mut PhotocraftApp) {
    let texture = app.session.tools.brush.texture.clone();
    for (_, brush) in &mut app.ui.tool_brushes {
        if brush.texture.enabled {
            brush.texture = texture.clone();
        }
    }
}

/// The Brush Settings panel menu actions (hamburger menu).
pub fn panel_menu(app: &mut PhotocraftApp, ui: &mut egui::Ui) {
    if ui.button(tl!("New Brush Preset…")).clicked() {
        let name = new_preset_name(&app.session.tools.presets);
        run_or_status(app, "brush.presets.save", json!({ "name": name }));
        if app.session.tools.presets.iter().any(|p| p.name == name) {
            app.ui.brushes_panel.renaming = Some(Renaming { group: false, name: name.clone(), folder: Vec::new(), text: String::new() });
            app.ui.brush_picker_list.renaming = Some(Renaming { group: false, name, folder: Vec::new(), text: String::new() });
        }
        ui.close();
    }
    ui.separator();
    if ui.button(tl!("Clear Brush Controls")).clicked() {
        let before = app.session.tools.brush.clone();
        let cleared = clear_brush_controls(&before);
        commit(app, &before, &cleared);
        ui.close();
    }
    let has_locks = app.session.tools.brush.locks != photocraft_paint::brush::SectionLocks::default();
    if ui.add_enabled(has_locks, egui::Button::new(tl!("Reset All Locked Settings"))).clicked() {
        let before = app.session.tools.brush.clone();
        let mut after = before.clone();
        after.locks = photocraft_paint::brush::SectionLocks::default();
        commit(app, &before, &after);
        ui.close();
    }
    let has_texture = app.session.tools.brush.texture.enabled;
    if ui.add_enabled(has_texture, egui::Button::new(tl!("Copy Texture to Other Tools"))).clicked() {
        copy_texture_to_other_tools(app);
        ui.close();
    }
}

pub fn window(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if !app.ui.panels.brush_settings {
        return;
    }
    let t = Tokens::get(ctx);
    let frame = egui::Frame::NONE
        .fill(t.card)
        .stroke(Stroke::new(1.0, t.card_border))
        .corner_radius(CornerRadius::same(t.radius_lg as u8))
        .shadow(egui::Shadow { offset: [0, 10], blur: 30, spread: 0, color: t.shadow })
        .inner_margin(egui::Margin::same(10));
    let canvas = app.last_canvas_rect;
    let mut open = true;
    egui::Window::new(tl!("Brush Settings"))
        .id(egui::Id::new("brush-settings"))
        .title_bar(false)
        .resizable(false)
        .frame(frame)
        .default_pos(egui::pos2((canvas.right() - WIDTH - 30.0).max(canvas.left() + 8.0), canvas.top() + 24.0))
        .show(ctx, |ui| {
            ui.set_width(WIDTH);
            ui.horizontal(|ui| {
                let mut tab = app.ui.brush_tab;
                for (i, name) in [tl!("Brush Settings"), tl!("Brushes")].iter().enumerate() {
                    if widgets::pill_tab(ui, name, tab == i).clicked() {
                        tab = i;
                    }
                }
                app.ui.brush_tab = tab;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icons::button(ui, "x", 20.0, false, tl!("Close")).clicked() {
                        open = false;
                    }
                    let menu_btn = widgets::panel_menu_button(ui);
                    egui::Popup::menu(&menu_btn).show(|ui| {
                        widgets::style_spectrum_popup_menu(ui);
                        ui.set_min_width(180.0);
                        panel_menu(app, ui);
                        ui.separator();
                        if ui.button(tl!("Close")).clicked() {
                            open = false;
                            ui.close();
                        }
                        if ui.button(tl!("Close Tab Group")).clicked() {
                            open = false;
                            ui.close();
                        }
                    });
                });
            });
            ui.add_space(6.0);
            widgets::hairline(ui);
            ui.add_space(6.0);
            if app.ui.brush_tab == 1 {
                crate::brushes_tab::show(app, ui);
            } else {
                settings_tab(app, ui);
            }
        });
    if !open {
        app.ui.panels.brush_settings = false;
    }
}

#[cfg(test)]
#[path = "brush_panel_tests.rs"]
mod tests;
