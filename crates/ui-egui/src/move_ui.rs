//! Move previews retain an immutable source texture and translate its GPU mesh. Releasing
//! preserves the session across drags; Enter or a tool switch commits a single history step.
//!
//! Raster selections are lifted once into that source. Marquee-tool floating selections keep
//! their existing independent preview path.

use std::sync::Arc;

use photocraft_doc::{DocId, Document, LayerId};
use photocraft_geom::Rect;

use crate::PhotocraftApp;
use crate::state::Tool;

/// Does the Move tool drag (and do the arrow keys nudge) the selected pixels of the active layer
/// rather than the whole layer, with `tool` in effect? With a selection, as in Photoshop; a layer
/// whose pixels can't float (type, shape, Smart Object, group, position-locked), a hidden layer or
/// several selected layers still move whole, as without one. The canvas cursor asks before the
/// press, and a pointer event passes `canvas::event_tool`, which also sees a ⌘ sent with the event
/// (#2768).
pub(crate) fn moves_selected_pixels_with(app: &PhotocraftApp, tool: Tool) -> bool {
    let Some(st) = app.session.active() else { return false };
    tool == Tool::Move
        && app.ui.tool_options.move_target == "layer"
        && st.doc.selection.is_some()
        && st.selected_layers().len() <= 1
        && st.active_layer.and_then(|id| st.doc.layer(id)).is_some_and(|l| l.visible)
        && app.session.is_enabled("select.float")
}

/// Floats the selected pixels and moves them by (dx, dy) (`select.float`): cuts them the first
/// time, or with `copy` (⌥) floats a duplicate and leaves them; `copy` on a piece already floating
/// drops it where it is and floats a copy of it. False when it failed (reported in the status bar).
#[cfg(test)]
pub(crate) fn float_selected(app: &mut PhotocraftApp, copy: bool, dx: f64, dy: f64) -> bool {
    use serde_json::json;
    let floating = app.session.active().is_some_and(|st| photocraft_engine::float_cmds::floating(st).is_some());
    let dropped = if copy && floating { app.run("select.drop", json!({})) } else { Ok(json!(null)) };
    match dropped.and_then(|_| app.run("select.float", json!({"dx": dx, "dy": dy, "copy": copy}))) {
        Ok(_) => true,
        Err(e) => {
            app.ui.status = e;
            app.ui.status_error = true;
            false
        }
    }
}

/// Commands that would replace the immutable source must wait for an explicit commit.
pub(crate) fn guards_command(app: &PhotocraftApp, id: &str) -> bool {
    if !crate::transform_tool::is_cached_raster(app) {
        return false;
    }
    if id.starts_with("file.save") || id.starts_with("file.export") || matches!(id, "file.close" | "file.closeAll" | "file.closeOthers" | "file.revert") {
        return true;
    }
    if matches!(
        id,
        "edit.undo"
            | "edit.redo"
            | "edit.copy"
            | "edit.copyMerged"
            | "edit.freeTransform"
            | "layer.setProps"
            | "layer.renameLayer"
            | "layer.setLabelColor"
            | "layer.setExpanded"
            | "layer.select"
            | "layer.selectAbove"
            | "layer.selectBelow"
            | "layer.selectTop"
            | "layer.selectBottom"
    ) || id.starts_with("edit.transform.")
    {
        return false;
    }
    ["layer.", "paint.", "filter.", "image.", "select.", "edit."].iter().any(|prefix| id.starts_with(prefix)) && !id.ends_with(".list") && !id.ends_with(".get")
}

/// Translate an immutable transform source without writing document pixels on pointer up.
/// The first move uploads the source; subsequent moves change only the quad's coordinates.
pub(crate) fn cached_pointer(app: &mut PhotocraftApp, ev: crate::canvas::ToolEvent, mods: egui::Modifiers) -> bool {
    use crate::canvas::{Drag, ToolEvent};
    let cached = crate::transform_tool::is_cached_move(app);
    if let ToolEvent::Down { x, y, pressure } = ev {
        if !cached {
            return false;
        }
        let _ = crate::move_mods::filter_event(app, ev, mods);
        app.drag = Some(Drag::new(Tool::Move, [x, y], vec![[x, y, f64::from(pressure)]], mods, false));
        return true;
    }
    let Some(d) = app.drag.as_ref().filter(|d| d.tool == Tool::Move && d.sel_move.is_none()) else { return false };
    if app.ui.transform.is_some() && !cached {
        return false;
    }
    let start = d.start;
    let previous = d.points.last().map_or(start, |p| [p[0], p[1]]);
    let ev = crate::move_mods::filter_event(app, ev, mods);
    let cached = crate::transform_tool::is_cached_move(app);
    let (x, y, pressure, released) = match ev {
        ToolEvent::Move { x, y, pressure } => (x, y, pressure, false),
        ToolEvent::Up { x, y } => (x, y, 1.0, true),
        ToolEvent::Down { .. } => return false,
    };
    let delta = [(x - start[0]).round() - (previous[0] - start[0]).round(), (y - start[1]).round() - (previous[1] - start[1]).round()];
    if !cached && delta != [0.0, 0.0] {
        let ctx = app.bounds_context.clone().unwrap_or_default();
        if let Err(error) = crate::transform_tool::begin_cached_move(app, &ctx) {
            app.ui.status = error;
            app.ui.status_error = true;
            app.drag = None;
            crate::move_mods::finish(app);
            return true;
        }
        app.move_preview = None;
    }
    if crate::transform_tool::is_cached_move(app) {
        crate::transform_tool::nudge(app, delta[0], delta[1]);
    }
    if let Some(d) = app.drag.as_mut() {
        d.points.push([x, y, f64::from(pressure)]);
    }
    if released {
        app.drag = None;
        if let Some(ctx) = app.bounds_context.clone() {
            crate::transform_tool::track_steps(app, &ctx);
        }
        crate::move_mods::finish(app);
    }
    true
}

/// Preview keys of move drags: `BASE + n`, one per offset shown (see `canvas::display_doc`).
const BASE: u64 = 1 << 39;

pub(crate) struct MovePreview {
    doc: DocId,
    revision: u64,
    /// What moving them can change at offset (0, 0), not clipped to the canvas: a layer larger
    /// than the canvas brings its pixels from beyond the edge into view (`None` = anything).
    bounds: Option<Rect>,
    canvas: Rect,
    /// Offsets shown so far, by preview key (`BASE + index`).
    offsets: Vec<(i32, i32)>,
    /// The document at the latest offset.
    shown: Option<Arc<Document>>,
    /// Shows a floating selection (`select.float`): offsets are the piece's, from where it was cut.
    floating: bool,
}

impl MovePreview {
    fn key(&self) -> u64 {
        BASE + self.offsets.len() as u64
    }
    fn offset_of(&self, key: u64) -> Option<(i32, i32)> {
        if key == 0 {
            return Some((0, 0));
        }
        let i = usize::try_from(key.checked_sub(BASE + 1)?).ok()?;
        self.offsets.get(i).copied()
    }
    fn area(&self, d: (i32, i32)) -> Option<Rect> {
        let b = self.bounds?;
        Some(if b.is_empty() { b } else { b.translate(d.0, d.1).inflate(1).intersect(&self.canvas) })
    }
}

/// Legacy floating selections used by marquee tools. Move-tool previews use the resident
/// transform textures instead of creating a translated document on every frame.
pub(crate) fn display_doc(app: &mut PhotocraftApp, idx: usize) -> Option<(Arc<Document>, u64)> {
    if crate::transform_tool::is_cached_move(app) {
        return None;
    }
    if app.session.active_index() == Some(idx)
        && let Some(st) = app.session.documents().get(idx)
        && photocraft_engine::float_cmds::floating(st).is_some()
    {
        return floating_doc(app, idx);
    }
    None
}

/// Where the canvas shows layer `id` of document `doc` at `revision` relative to where it is: the
/// offset of the latest Move drag frame drawn ([`display_doc`]) when the layer moves with the drag,
/// so its Layer Edges outline moves in lockstep with its pixels (#2413). `None` when it shows
/// where it is.
pub(crate) fn shown_offset(app: &PhotocraftApp, doc: DocId, revision: u64, id: LayerId) -> Option<(i32, i32)> {
    let st = app.session.active()?;
    if st.doc.id != doc || st.revision != revision {
        return None;
    }
    crate::transform_tool::cached_move_offset(app, doc, id)
}

/// A floating selection's offset plus a drag of it in progress (`canvas::selection_drag_delta`).
pub(crate) fn floating_offset(app: &PhotocraftApp) -> Option<(i32, i32)> {
    let f = photocraft_engine::float_cmds::floating(app.session.active()?)?;
    let d = crate::canvas::selection_drag_delta(app).unwrap_or((0, 0));
    Some((f.offset.0 + d.0, f.offset.1 + d.1))
}

/// The document shown while a selection floats: the cut piece at its offset, over the cut-out
/// layer. Only the piece's area changes between offsets, so each move costs the piece, not the
/// layer.
fn floating_doc(app: &mut PhotocraftApp, idx: usize) -> Option<(Arc<Document>, u64)> {
    let offset = floating_offset(app)?;
    let st = app.session.documents().get(idx)?;
    photocraft_engine::float_cmds::floating(st)?;
    let (doc_id, revision) = (st.doc.id, st.revision);
    let fresh = app.move_preview.as_ref().is_some_and(|p| p.doc == doc_id && p.revision == revision && p.floating);
    if !fresh {
        let bounds = st.doc.selection.as_ref().map(|s| s.content_bounds());
        let canvas = st.doc.bounds();
        app.move_preview = Some(MovePreview { doc: doc_id, revision, bounds, canvas, offsets: Vec::new(), shown: None, floating: true });
    }
    let st = app.session.documents().get(idx)?;
    let shown = app.move_preview.as_ref()?.offsets.last() != Some(&offset);
    if shown {
        let t0 = crate::gpu_canvas::now_ms();
        let d = photocraft_engine::float_cmds::displayed(st, crate::canvas::selection_drag_delta(app).unwrap_or((0, 0)))?;
        let d = photocraft_engine::mode_cmds::display_document(&d).unwrap_or(d);
        let p = app.move_preview.as_mut()?;
        p.shown = Some(Arc::new(d));
        p.offsets.push(offset);
        app.perf.span("floating preview", crate::gpu_canvas::now_ms() - t0);
    }
    let p = app.move_preview.as_ref()?;
    Some((p.shown.clone()?, p.key()))
}

/// What changed between preview (or document) key `seen` and `now` of the current Move drag on
/// document `doc` at `revision`: where the moving layers were in both. `None` when either key
/// isn't one of this drag's (the canvas then recomposites everything).
pub(crate) fn damage(app: &PhotocraftApp, doc: DocId, revision: u64, seen: u64, now: u64) -> Option<Rect> {
    let p = app.move_preview.as_ref().filter(|p| p.doc == doc && p.revision == revision)?;
    if seen == now || (seen < BASE && seen != 0) || (now < BASE && now != 0) {
        return None;
    }
    let (a, b) = (p.area(p.offset_of(seen)?)?, p.area(p.offset_of(now)?)?);
    Some(if a.is_empty() {
        b
    } else if b.is_empty() {
        a
    } else {
        a.union(&b)
    })
}

/// Whether the canvas shows a Move drag's layers at the pointer.
pub(crate) fn showing(app: &PhotocraftApp) -> bool {
    crate::transform_tool::is_cached_move(app) || app.move_preview.as_ref().is_some_and(|p| p.shown.is_some())
}

/// Keep the final translation pending when a caller finishes a Move gesture directly.
pub(crate) fn finish(app: &mut PhotocraftApp, dx: f64, dy: f64) {
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    if !crate::transform_tool::is_cached_move(app) {
        let ctx = app.bounds_context.clone().unwrap_or_default();
        if let Err(error) = crate::transform_tool::begin_cached_move(app, &ctx) {
            app.ui.status = error;
            app.ui.status_error = true;
            return;
        }
    }
    crate::transform_tool::nudge(app, dx, dy);
}

#[cfg(test)]
mod tests {
    use crate::{PhotocraftApp, canvas::ToolEvent, state::Tool};
    use serde_json::json;

    fn app() -> (PhotocraftApp, photocraft_doc::LayerId) {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.session.execute("file.new", json!({"width":64,"height":64,"background":"transparent"})).unwrap();
        app.sync_views();
        app.session.execute("select.rect", json!({"x":8,"y":8,"width":8,"height":8})).unwrap();
        app.session.execute("edit.fill", json!({"color":"#ff0000"})).unwrap();
        app.session.execute("select.deselect", json!({})).unwrap();
        app.ui.tool = Tool::Move;
        app.ui.tool_options.move_auto_select = false;
        app.ui.extras.snap = false;
        app.ui.view.show.smart_guides = false;
        app.bounds_context = Some(egui::Context::default());
        let id = app.session.active().unwrap().active_layer.unwrap();
        (app, id)
    }

    fn drag(app: &mut PhotocraftApp, start: [f64; 2], end: [f64; 2]) {
        let m = egui::Modifiers::NONE;
        crate::canvas::tool_event(app, ToolEvent::Down { x: start[0], y: start[1], pressure: 1.0 }, m);
        crate::canvas::tool_event(app, ToolEvent::Move { x: end[0], y: end[1], pressure: 1.0 }, m);
        crate::canvas::tool_event(app, ToolEvent::Up { x: end[0], y: end[1] }, m);
    }

    #[test]
    fn cached_move_keeps_source_unchanged_across_drags_until_enter() {
        let (mut app, id) = app();
        let revision = app.session.active().unwrap().revision;
        let history = app.session.active().unwrap().history.past_len();
        drag(&mut app, [10.0, 10.0], [20.0, 15.0]);
        drag(&mut app, [20.0, 15.0], [23.0, 17.0]);
        let st = app.session.active().unwrap();
        assert_eq!(st.revision, revision);
        assert_eq!(st.history.past_len(), history);
        assert_eq!(st.doc.layer(id).unwrap().surface().unwrap().rgba(10, 10)[3], 1.0);
        let t = app.ui.transform.as_ref().unwrap();
        assert_eq!([t.quad[0][0] - t.rect[0], t.quad[0][1] - t.rect[1]], [13.0, 7.0]);
        assert!(super::display_doc(&mut app, 0).is_none(), "the canvas uses its cached mesh");
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            events: vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }],
            ..Default::default()
        };
        ctx.run_ui(input, |ui| crate::shortcuts::handle(&mut app, ui.ctx())).textures_delta.clear();
        assert!(app.ui.transform.is_none());
        let st = app.session.active().unwrap();
        assert_eq!(st.history.past_len(), history + 1);
        assert_eq!(st.doc.layer(id).unwrap().surface().unwrap().rgba(10, 10)[3], 0.0);
        assert_eq!(st.doc.layer(id).unwrap().surface().unwrap().rgba(23, 17)[3], 1.0);
        assert!(app.session.undo());
        assert_eq!(app.session.active().unwrap().doc.layer(id).unwrap().surface().unwrap().rgba(10, 10)[3], 1.0);
    }

    #[test]
    fn switching_tools_commits_the_pending_move_once() {
        let (mut app, id) = app();
        let h0 = app.session.active().unwrap().history.past_len();
        drag(&mut app, [10.0, 10.0], [20.0, 10.0]);
        assert_eq!(app.session.active().unwrap().history.past_len(), h0);
        app.ui.tool = Tool::Brush;
        crate::transform_tool::end_if_left(&mut app);
        assert!(app.ui.transform.is_none());
        assert_eq!(app.session.active().unwrap().history.past_len(), h0 + 1);
        assert_eq!(app.session.active().unwrap().doc.layer(id).unwrap().surface().unwrap().rgba(20, 10)[3], 1.0);
    }

    #[test]
    fn document_commands_cannot_overwrite_or_commit_pending_move() {
        let (mut app, id) = app();
        drag(&mut app, [10.0, 10.0], [20.0, 10.0]);
        let revision = app.session.active().unwrap().revision;
        let session = app.ui.transform.as_ref().unwrap().session;
        assert!(app.run("layer.new.layer", json!({})).is_err());
        assert!(app.run("edit.fill", json!({"color":"#0000ff"})).is_err());
        assert_eq!(app.session.active().unwrap().revision, revision);
        assert_eq!(app.ui.transform.as_ref().unwrap().session, session);
        assert_eq!(app.session.active().unwrap().doc.layer(id).unwrap().surface().unwrap().rgba(10, 10)[3], 1.0);
    }

    #[test]
    fn move_then_transform_preserves_the_same_source_and_pending_translation() {
        let (mut app, _) = app();
        drag(&mut app, [10.0, 10.0], [20.0, 10.0]);
        let before = app.ui.transform.clone().unwrap();
        let revision = app.session.active().unwrap().revision;
        crate::menus::invoke(&mut app, &egui::Context::default(), "edit.freeTransform", json!({})).unwrap();
        let after = app.ui.transform.as_ref().unwrap();
        assert_eq!(after.session, before.session);
        assert_eq!(after.quad, before.quad);
        assert_eq!(app.session.active().unwrap().revision, revision);
        assert!(!crate::transform_tool::is_cached_move(&app));
    }

    #[test]
    fn alt_move_copies_only_selected_pixels_on_the_same_layer() {
        let (mut app, id) = app();
        app.ui.tool_options.move_target = "layer".into();
        app.session.execute("select.rect", json!({"x":8,"y":8,"width":4,"height":8})).unwrap();
        let n = app.session.active().unwrap().doc.layers.len();
        let m = egui::Modifiers::ALT;
        crate::canvas::tool_event(&mut app, ToolEvent::Down { x: 10.0, y: 10.0, pressure: 1.0 }, m);
        crate::canvas::tool_event(&mut app, ToolEvent::Move { x: 30.0, y: 10.0, pressure: 1.0 }, m);
        crate::canvas::tool_event(&mut app, ToolEvent::Up { x: 30.0, y: 10.0 }, m);
        assert_eq!(app.session.active().unwrap().doc.layers.len(), n);
        crate::transform_tool::commit(&mut app);
        let st = app.session.active().unwrap();
        let surface = st.doc.layer(id).unwrap().surface().unwrap();
        assert_eq!(surface.rgba(10, 10)[3], 1.0);
        assert_eq!(surface.rgba(30, 10)[3], 1.0);
        assert_eq!(surface.rgba(34, 10)[3], 0.0, "unselected pixels were not copied");
    }

    #[test]
    fn cached_move_accumulates_subpixel_pointer_samples() {
        let (mut app, _) = app();
        let m = egui::Modifiers::NONE;
        crate::canvas::tool_event(&mut app, ToolEvent::Down { x: 10.0, y: 10.0, pressure: 1.0 }, m);
        for x in [10.2, 10.4, 10.6, 10.8, 11.0] {
            crate::canvas::tool_event(&mut app, ToolEvent::Move { x, y: 10.0, pressure: 1.0 }, m);
        }
        crate::canvas::tool_event(&mut app, ToolEvent::Up { x: 11.0, y: 10.0 }, m);
        let t = app.ui.transform.as_ref().unwrap();
        assert_eq!(t.quad[0][0] - t.rect[0], 1.0);
    }

    #[test]
    fn cached_move_escape_discards_without_history_or_pixel_changes() {
        let (mut app, id) = app();
        let revision = app.session.active().unwrap().revision;
        drag(&mut app, [10.0, 10.0], [30.0, 30.0]);
        crate::transform_tool::cancel(&mut app);
        assert!(app.ui.transform.is_none());
        let st = app.session.active().unwrap();
        assert_eq!(st.revision, revision);
        assert_eq!(st.doc.layer(id).unwrap().surface().unwrap().rgba(10, 10)[3], 1.0);
    }

    #[test]
    fn cached_move_alt_duplicate_cancels_without_leaving_a_layer_or_history() {
        let (mut app, id) = app();
        let n = app.session.active().unwrap().doc.layers.len();
        let h = app.session.active().unwrap().history.past_len();
        let mods = egui::Modifiers::ALT;
        crate::canvas::tool_event(&mut app, ToolEvent::Down { x: 10., y: 10., pressure: 1. }, mods);
        crate::canvas::tool_event(&mut app, ToolEvent::Move { x: 30., y: 10., pressure: 1. }, mods);
        crate::canvas::tool_event(&mut app, ToolEvent::Up { x: 30., y: 10. }, mods);
        assert_eq!(app.session.active().unwrap().doc.layers.len(), n + 1);
        crate::transform_tool::cancel(&mut app);
        let st = app.session.active().unwrap();
        assert_eq!(st.doc.layers.len(), n);
        assert_eq!(st.history.past_len(), h);
        assert_eq!(st.active_layer, Some(id));
        assert_eq!(st.doc.layer(id).unwrap().surface().unwrap().rgba(10, 10)[3], 1.);
    }

    #[test]
    fn cached_move_alt_on_pending_source_cannot_commit_or_duplicate() {
        let (mut app, _) = app();
        drag(&mut app, [10., 10.], [20., 10.]);
        let revision = app.session.active().unwrap().revision;
        let quad = app.ui.transform.as_ref().unwrap().quad;
        let session = app.ui.transform.as_ref().unwrap().session;
        crate::move_mods::nudge(&mut app, 1., 0., true);
        let mods = egui::Modifiers::ALT;
        crate::canvas::tool_event(&mut app, ToolEvent::Down { x: 20., y: 10., pressure: 1. }, mods);
        crate::canvas::tool_event(&mut app, ToolEvent::Move { x: 35., y: 10., pressure: 1. }, mods);
        crate::canvas::tool_event(&mut app, ToolEvent::Up { x: 40., y: 10. }, mods);
        assert_eq!(app.session.active().unwrap().revision, revision);
        assert_eq!(app.ui.transform.as_ref().unwrap().session, session);
        assert_eq!(app.ui.transform.as_ref().unwrap().quad, quad);
        assert!(app.ui.status_error);
    }

    #[test]
    fn cached_move_save_entry_cannot_write_the_uncommitted_source() {
        let (mut app, _) = app();
        drag(&mut app, [10., 10.], [20., 10.]);
        let result = app.write_document("pending.png".into(), &crate::ExportSettings::default(), false);
        assert!(result.unwrap_err().contains("Press Enter"));
        let doc = app.session.active().unwrap().doc.clone();
        assert!(crate::view_cmds::write_copy(&mut app, &doc, "pending.png", None).unwrap_err().contains("Press Enter"));
        assert!(app.ui.transform.is_some());
    }

    #[test]
    fn cached_move_metadata_and_layer_selection_leave_the_frozen_target_pending() {
        let (mut app, id) = app();
        app.session.execute("layer.new.layer", json!({})).unwrap();
        let other = app.session.active().unwrap().active_layer.unwrap();
        app.session.execute("layer.select", json!({"layer":id.0})).unwrap();
        drag(&mut app, [10., 10.], [20., 10.]);
        let session = app.ui.transform.as_ref().unwrap().session;
        app.run("layer.setProps", json!({"layer":id.0,"name":"Pending","opacity":0.4})).unwrap();
        app.run("layer.select", json!({"layer":other.0})).unwrap();
        crate::transform_tool::end_if_left(&mut app);
        assert_eq!(app.ui.transform.as_ref().unwrap().session, session);
        assert_eq!(app.ui.transform.as_ref().unwrap().layer, id.0);
        let st = app.session.active().unwrap();
        assert_eq!(super::shown_offset(&app, st.doc.id, st.revision, id), Some((10, 0)), "the frozen source keeps its offset");
        assert_eq!(super::shown_offset(&app, st.doc.id, st.revision, other), None, "a newly selected layer stays in place");
        assert_eq!(app.session.active().unwrap().doc.layer(id).unwrap().surface().unwrap().rgba(10, 10)[3], 1.);
        crate::transform_tool::commit(&mut app);
        let st = app.session.active().unwrap();
        assert_eq!(st.doc.layer(id).unwrap().opacity, 0.4);
        assert_eq!(st.doc.layer(id).unwrap().surface().unwrap().rgba(20, 10)[3], 1.);
        assert!(st.doc.layer(other).unwrap().surface().unwrap().content_bounds().is_empty());
    }
}
