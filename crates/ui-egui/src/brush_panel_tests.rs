use egui::vec2;
use egui_kittest::Harness;
use photocraft_engine::paint::{Control, GrayTile, Pattern, PatternStyle, TipShape};

use super::*;

fn app() -> PhotocraftApp {
    PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default())
}

fn last_journal(app: &PhotocraftApp) -> Option<(String, Value)> {
    app.session.journal.last().cloned()
}

#[test]
fn sections_with_boxes_have_flags() {
    let mut b = BrushSettings::default();
    for (i, (_, has_box)) in SECTIONS.iter().enumerate() {
        if *has_box {
            set_section_enabled(&mut b, i, false);
        }
        assert_eq!(section_flag(&mut b, i).is_some(), matches!(i, 2 | 3 | 5 | 6 | 7), "{i}");
    }
    *section_flag(&mut b, 3).unwrap() = true;
    assert!(b.texture.enabled);
}

#[test]
fn brush_categories_match_the_reference_list() {
    assert_eq!(
        SECTIONS,
        [("Tip", false), ("Basic", false), ("Motion", true), ("Texture", true), ("Color Mode", true), ("Wet Edge", true), ("Mix", true), ("Pigment", true),]
    );
}

fn edit_section(i: usize, b: &mut BrushSettings) {
    let dynamic = |jitter: f32, control: Control, minimum: f32| paint::Dynamic { jitter, control, fade_steps: 40, minimum, ..Default::default() };
    match i {
        0 => {
            b.size = 87.0;
            b.flip_x = true;
            b.angle = -35.0;
            b.hardness = 0.25;
            b.spacing = 0.6;
        }
        1 => {
            b.shape_dynamics.enabled = true;
            b.shape_dynamics.size = dynamic(0.3, Control::Fade, 0.2);
            b.scattering.enabled = true;
            b.scattering.scatter = dynamic(2.5, Control::PenPressure, 0.0);
            b.transfer.enabled = true;
            b.transfer.flow = dynamic(0.25, Control::Fade, 0.3);
        }
        2 => {
            b.motion.enabled = true;
            b.motion.smoothing = 0.45;
            b.inertia.enabled = true;
            b.inertia.amount = 0.3;
        }
        3 => {
            b.texture.enabled = true;
            b.texture.pattern = Pattern::Procedural { style: PatternStyle::Canvas, size: 200, seed: 1 };
            b.texture.depth = 0.7;
            b.noise = true;
            b.grain = Some(paint::material::GrainSettings { density: 0.8, size: 24.0, ..Default::default() });
            b.protect_texture = true;
        }
        4 => {
            b.color_dynamics.enabled = true;
            b.color_dynamics.hue_jitter = 0.1;
            b.color_attributes.enabled = true;
            b.color_attributes.blend = paint::AttributeMode::Multiply;
            b.color_attributes.blend_strength = 0.6;
        }
        5 => {
            b.wet_edges = true;
            b.wet_edge = Some(paint::material::WetEdgeSettings { strength: 1.6, width: 2.0, ..Default::default() });
        }
        6 => {
            b.mix_settings.enabled = true;
            b.mix_settings.strength = 0.65;
            b.mix_settings.color_output = true;
        }
        7 => b.pigment = Some(paint::pigment_material::PigmentSettings { enabled: true, thickness: 65.0, visual_thickness: 35.0, ..Default::default() }),
        _ => unreachable!(),
    }
}

#[test]
fn every_section_round_trips_through_set_brush() {
    for (i, (name, _)) in SECTIONS.iter().enumerate() {
        let mut app = app();
        let before = app.session.tools.brush.clone();
        let mut after = before.clone();
        edit_section(i, &mut after);
        assert_ne!(before, after, "section {i} edits nothing");
        commit(&mut app, &before, &after);
        assert_eq!(app.session.tools.brush, after, "section {i} ({name})");
        // Through the command, with only the changed fields.
        let (id, p) = last_journal(&app).expect("journaled");
        assert_eq!(id, "tools.setBrush");
        let patch = &p["brush"];
        assert!(patch.as_object().is_some_and(|o| !o.is_empty() && o.len() <= 8), "section {i}: {patch}");
        // And the engine's view (`brush.get`) reads back the same settings.
        let got = app.session.execute("brush.get", json!({})).unwrap();
        assert_eq!(serde_json::from_value::<BrushSettings>(got).unwrap(), after, "section {i}");
        // Replaying the journaled call onto a fresh session gives the same brush (drivable).
        let mut s = photocraft_engine::Session::new();
        s.execute(&id, p).unwrap();
        assert_eq!(s.tools.brush, after, "replay of section {i}");
    }
}

#[test]
fn patches_are_minimal_and_skip_unchanged_bitmaps() {
    let tip = TipShape::Sampled(GrayTile::from_fn(400, 300, |x, y| ((x * y) % 7) as f32 / 7.0));
    let old = BrushSettings {
        tip: tip.clone(),
        texture: paint::Texture { pattern: Pattern::Tile(GrayTile::from_fn(64, 64, |x, _| x as f32 / 64.0)), ..Default::default() },
        ..Default::default()
    };
    let new = BrushSettings { size: 44.0, ..old.clone() };
    assert_eq!(brush_patch(&old, &new), json!({"size": 44.0}));
    let new = BrushSettings { shape_dynamics: paint::ShapeDynamics { enabled: true, ..Default::default() }, ..old.clone() };
    assert_eq!(brush_patch(&old, &new), json!({"shapeDynamics": {"enabled": true}}));
    assert_eq!(brush_patch(&old, &old), json!({}));
    // A changed tip is sent whole.
    let new = BrushSettings { tip: TipShape::Round, ..old.clone() };
    assert_eq!(brush_patch(&old, &new), json!({"tip": "round"}));
    // Applying any of these through the engine gives back `new`.
    let mut app = app();
    app.session.tools.brush = old.clone();
    let new = BrushSettings { size: 9.0, roundness: 0.5, ..old.clone() };
    commit(&mut app, &old, &new);
    assert_eq!(app.session.tools.brush, new);
    // No change, no command.
    let n = app.session.journal.len();
    commit(&mut app, &new, &new);
    assert_eq!(app.session.journal.len(), n);
}

#[test]
fn bad_patches_fail_without_touching_the_brush() {
    let mut app = app();
    let before = app.session.tools.brush.clone();
    for bad in [
        json!({"brush": {"size": "huge"}}),
        json!({"brush": {"shapeDynamics": {"size": {"control": "telepathy"}}}}),
        json!({"brush": {"tip": {"sampled": {"width": 0}}}}),
    ] {
        assert!(app.run("tools.setBrush", bad).is_err());
        assert_eq!(app.session.tools.brush, before);
    }
}

#[test]
fn brush_categories_do_not_expose_legacy_lock_rows() {
    let mut b = BrushSettings::default();
    for i in 0..SECTIONS.len() {
        assert!(section_lock(&mut b, i).is_none());
    }
}

/// A harness that shows the options bar with `tool` in the Studio (non-pro) theme.
fn options_bar_harness(tool: crate::state::Tool) -> Harness<'static, PhotocraftApp> {
    let mut app = app();
    app.ui.tool = tool;
    let mut h = Harness::builder().with_size(vec2(2000.0, 60.0)).build_ui_state(
        |ui, app: &mut PhotocraftApp| {
            if !ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                return;
            }
            crate::panels::options_bar(app, ui);
        },
        app,
    );
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::Studio);
    h.run_steps(4);
    h
}

#[test]
fn clone_stamp_opacity_and_flow_are_journaled_per_gesture() {
    use egui_kittest::kittest::Queryable;
    let mut h = options_bar_harness(crate::state::Tool::CloneStamp);
    for (label, field) in [("Opacity", "opacity"), ("Flow", "flow")] {
        let n = h.state().session.journal.len();
        let rect = h.get_by_label(label).rect();
        let start = egui::pos2(rect.right() + 40.0, rect.center().y);
        h.hover_at(start);
        h.run();
        h.drag_at(start);
        h.run();
        for k in 1..=8 {
            h.hover_at(start - vec2(5.0 * k as f32, 0.0));
            h.run();
        }
        h.drop_at(start - vec2(40.0, 0.0));
        h.run_steps(2);
        let brush = &h.state().session.tools.brush;
        let value = if field == "opacity" { brush.opacity } else { brush.flow };
        assert!(value < 0.95, "the drag lowered {field}: {value}");
        let journal = &h.state().session.journal;
        assert_eq!(journal.len(), n + 1, "one command per gesture");
        let (id, params) = journal.last().unwrap();
        assert_eq!(id, "tools.setBrush");
        assert_eq!(params["brush"][field].as_f64().map(|v| v as f32), Some(value));
    }
    h.state_mut().ui.clone_source = Some([0.0, 0.0]);
    let params = crate::retouch_ui::clone_params(h.state()).unwrap();
    assert_eq!(params["opacity"], json!(h.state().session.tools.brush.opacity * 100.0));
    assert_eq!(params["flow"], json!(h.state().session.tools.brush.flow * 100.0));
    let mut replay = photocraft_engine::Session::new();
    for (id, params) in &h.state().session.journal {
        replay.execute(id, params.clone()).unwrap();
    }
    assert_eq!(replay.tools.brush, h.state().session.tools.brush);
}

#[test]
fn options_bar_edits_are_one_set_brush_per_gesture() {
    use egui_kittest::kittest::Queryable;
    let mut h = options_bar_harness(crate::state::Tool::Brush);
    let n = h.state().session.journal.len();
    h.run_steps(3);
    assert_eq!(h.state().session.journal.len(), n, "showing the bar issues nothing");
    // Drag the Opacity field left: many frames, one journal entry.
    let label = h.get_by_label("Opacity").rect();
    let start = egui::pos2(label.right() + 40.0, label.center().y);
    h.hover_at(start);
    h.run();
    h.drag_at(start);
    h.run();
    for k in 1..=8 {
        h.hover_at(start - vec2(5.0 * k as f32, 0.0));
        h.run();
    }
    h.drop_at(start - vec2(40.0, 0.0));
    h.run_steps(2);
    let opacity = h.state().session.tools.brush.opacity;
    assert!(opacity < 0.95, "the drag lowered the opacity: {opacity}");
    let j = &h.state().session.journal;
    assert_eq!(j.len(), n + 1, "{:?}", &j[n..]);
    let (id, p) = j.last().unwrap();
    assert_eq!(id, "tools.setBrush");
    assert_eq!(p["brush"]["opacity"].as_f64().map(|v| v as f32), Some(opacity));
    // A pressure-button click is its own gesture.
    let label = h.get_by_label("Pressure for Size").rect();
    let was = h.state().session.tools.brush.pressure_size;
    let at = label.center();
    h.hover_at(at);
    h.run();
    h.drag_at(at);
    h.run();
    h.drop_at(at);
    h.run_steps(2);
    assert_eq!(h.state().session.tools.brush.pressure_size, !was);
    assert_eq!(h.state().session.journal.len(), n + 2);
    assert_eq!(h.state().session.journal.last().unwrap().1["brush"], json!({"pressureSize": !was}));
    // Replaying the journal gives the same brush.
    let mut s = photocraft_engine::Session::new();
    for (id, p) in &h.state().session.journal {
        s.execute(id, p.clone()).unwrap();
    }
    assert_eq!(s.tools.brush, h.state().session.tools.brush);
}

#[test]
fn brush_mode_dropdown_shows_outside_the_pro_themes() {
    // The toolbar's Mode slot edits the main color attribute in every theme.
    use egui_kittest::kittest::{NodeT, Queryable};
    let mut h = options_bar_harness(crate::state::Tool::Brush);
    let n = h.state().session.journal.len();
    assert!(h.query_by_label("Mode").is_some(), "the Brush's Mode label");
    h.get_all_by_role(egui::accesskit::Role::ComboBox)
        .find(|c| c.accesskit_node().value().as_deref() == Some("Off"))
        .expect("the Brush's Mode dropdown")
        .click();
    h.run();
    h.key_press(egui::Key::ArrowDown);
    h.run_steps(3);
    assert_ne!(h.state().session.tools.brush.color_attributes.blend, paint::AttributeMode::Off);
    assert!(h.state().session.tools.brush.color_attributes.enabled);
    assert_eq!(h.state().session.tools.brush.mode, photocraft_doc::BlendMode::Normal);
    let j = &h.state().session.journal;
    assert_eq!(j.len(), n + 1, "{:?}", &j[n..]);
    assert_eq!(j[n].0, "tools.setBrush");
}

#[test]
fn brush_and_eraser_size_fields_keep_sizes_above_2500_px() {
    // #5708: outside the Pro themes the Size field capped at 2500 px and clamped a bigger brush
    // (set from the picker or `]`) on the next frame; Photoshop and the engine allow 5000 px.
    for tool in [crate::state::Tool::Brush, crate::state::Tool::Eraser] {
        let mut h = options_bar_harness(tool);
        let n = h.state().session.journal.len();
        h.state_mut().session.tools.brush.size = 4000.0;
        h.run_steps(4);
        assert_eq!(h.state().session.tools.brush.size, 4000.0, "{tool:?}");
        let j = &h.state().session.journal;
        assert_eq!(j.len(), n, "showing the bar issues nothing: {:?}", &j[n..]);
    }
}

#[test]
fn unsupported_brush_options_and_smoothing_are_hidden_and_symmetry_works() {
    // #1177: in the default (Photoshop) theme these options-bar buttons did nothing.
    use egui_kittest::kittest::Queryable;
    let mut h = app_harness();
    h.state_mut().session.tools.brush.c3_tip_math = false;
    h.run_steps(2);
    let brush = |h: &Harness<'static, PhotocraftApp>| h.state().session.tools.brush.clone();

    assert!(h.query_by_label("Enable airbrush-style build-up effects").is_none());
    assert!(!brush(&h).build_up);

    for label in ["Smoothing", "Smoothing:", "Set additional smoothing options", "Pulled String Mode", "Stroke Catch-up", "Catch-up on Stroke End", "Adjust for Zoom"] {
        assert!(h.query_by_label(label).is_none(), "{label} is absent from the options bar");
    }

    // Symmetry: the menu lists the document's paths and turns symmetry on and off.
    h.state_mut().run("file.new", json!({"width": 80, "height": 60})).unwrap();
    h.state_mut().run("path.set", json!({"name": "work", "path": {"subpaths": [{"closed": false, "knots": [[40, 0], [40, 60]]}]}})).unwrap();
    h.run_steps(3);
    h.get_by_label("Set painting symmetry options").click();
    h.run_steps(3);
    h.get_by_label("Work Path").click();
    h.run_steps(3);
    let source = |h: &Harness<'static, PhotocraftApp>| h.state().session.active().unwrap().symmetry.as_ref().and_then(|a| a.path_source().map(str::to_owned));
    assert_eq!(source(&h).as_deref(), Some("work"));
    assert_eq!(last_journal(h.state()).unwrap(), ("paint.symmetryFromPath".to_string(), json!({"name": "work"})));
    h.get_by_label("Set painting symmetry options").click();
    h.run_steps(3);
    h.get_by_label("Symmetry Off").click();
    h.run_steps(3);
    assert_eq!(source(&h), None);
    assert!(h.state().ui.status.is_empty(), "{}", h.state().ui.status);
}

#[test]
fn mixer_brush_options_update_the_persisted_mixer_settings() {
    use egui_kittest::kittest::Queryable;
    let mut h = options_bar_harness(crate::state::Tool::MixerBrush);
    for label in ["Wet", "Load", "Mix", "Flow", "Sample All Layers"] {
        assert!(h.get_by_label(label).rect().is_positive(), "missing Mixer Brush option {label}");
    }

    let initial = h.state().session.journal.len();
    let at = h.get_by_label("Sample All Layers").rect().center();
    h.hover_at(at);
    h.run();
    h.drag_at(at);
    h.run();
    h.drop_at(at);
    h.run_steps(2);

    assert!(h.state().session.tools.brush.mixer.sample_all_layers);
    assert_eq!(h.state().session.journal.len(), initial + 1);
    let (id, params) = h.state().session.journal.last().unwrap();
    assert_eq!(id, "tools.setBrush");
    assert_eq!(params["brush"]["mixer"]["sampleAllLayers"], true);
}

#[test]
fn drop_targets_and_actions_reorder_presets() {
    use crate::brushes_tab::{Action, apply, drop_target, group_key};
    let mut app = app();
    for (n, g) in [("A1", "Alpha"), ("A2", "Alpha"), ("A3", "Alpha"), ("B1", "Beta")] {
        app.session.tools.presets.push(paint::BrushPreset {
            name: n.into(),
            brush: Default::default(),
            builtin: false,
            group: g.into(),
            folder: Vec::new(),
            captures: Default::default(),
        });
    }
    let p = &app.session.tools.presets;
    // Dropping A1 on A3's lower half lands after A3 (index 2 once A1 is out); on its upper half, before it.
    assert_eq!(drop_target(p, "A1", "A3", true), Some(("Alpha".into(), vec![], 2)));
    assert_eq!(drop_target(p, "A1", "A3", false), Some(("Alpha".into(), vec![], 1)));
    assert_eq!(drop_target(p, "A3", "A1", false), Some(("Alpha".into(), vec![], 0)));
    assert_eq!(drop_target(p, "A2", "B1", true), Some(("Beta".into(), vec![], 1)));
    assert_eq!(drop_target(p, "A2", "Nope", true), None);
    assert_eq!(group_key(p, UNGROUPED), "");
    let (group, folder, index) = drop_target(p, "A1", "A3", true).unwrap();
    apply(&mut app, vec![Action::Move { name: "A1".into(), group, folder: Some(folder), index: Some(index) }]);
    let names = |app: &PhotocraftApp, g: &str| app.session.tools.presets.iter().filter(|x| x.group == g).map(|x| x.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&app, "Alpha"), ["A2", "A3", "A1"]);
    apply(&mut app, vec![Action::Move { name: "A2".into(), group: "Beta".into(), folder: None, index: None }]);
    assert_eq!(names(&app, "Beta"), ["B1", "A2"]);
    apply(&mut app, vec![Action::MoveGroup { group: "Beta".into(), folder: vec![], before: Some("Alpha".into()) }]);
    let order = photocraft_engine::brush_preset_cmds::group_order(&app.session.tools.presets);
    assert!(order.iter().position(|g| g == "Beta") < order.iter().position(|g| g == "Alpha"));
    // Rename and delete from the context menu.
    apply(&mut app, vec![Action::Rename(Renaming { group: false, name: "A3".into(), folder: vec![], text: "Third".into() })]);
    assert!(paint::presets::find(&app.session.tools.presets, "Third").is_some());
    assert!(app.ui.brushes_panel.renaming.is_none());
    apply(&mut app, vec![Action::Rename(Renaming { group: true, name: "Beta".into(), folder: vec![], text: "Bees".into() })]);
    assert_eq!(names(&app, "Bees"), ["B1", "A2"]);
    apply(&mut app, vec![Action::Delete("Third".into()), Action::DeleteGroup { group: "Bees".into(), folder: vec![] }]);
    assert_eq!(names(&app, "Alpha"), ["A1"]);
    assert!(names(&app, "Bees").is_empty());
    // Every action went through a journaled command.
    let ids: Vec<&str> = app.session.journal.iter().map(|(id, _)| id.as_str()).collect();
    for id in [
        "brush.presets.move",
        "brush.presets.moveGroup",
        "brush.presets.rename",
        "brush.presets.renameGroup",
        "brush.presets.delete",
        "brush.presets.deleteGroup",
    ] {
        assert!(ids.contains(&id), "{id}: {ids:?}");
    }
}

fn harness(tab: usize, section: usize) -> Harness<'static, PhotocraftApp> {
    let mut app = app();
    app.ui.panels.brush_settings = true;
    app.ui.brush_tab = tab;
    app.ui.brush_section = section;
    let mut h = Harness::builder().with_size(vec2(1200.0, 900.0)).build_ui_state(
        |ui, app: &mut PhotocraftApp| {
            let ctx = ui.ctx().clone();
            if !ctx.fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                return;
            }
            window(app, &ctx);
        },
        app,
    );
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::ALL[0]);
    h.run_steps(4);
    h
}

fn toggle_value(app: &PhotocraftApp, section: usize) -> bool {
    section_enabled(&app.session.tools.brush, section)
}

#[test]
fn hue_jitter_amount_edits_keep_percentage_units_and_command_routing() {
    use egui::accesskit::Role;
    use egui_kittest::kittest::{NodeT, Queryable};

    let mut h = harness(0, 4);
    h.state_mut()
        .session
        .execute(
            "tools.setBrush",
            json!({"brush": {"colorDynamics": {
                "enabled": true, "hueJitter": 0.37, "saturationJitter": 0.23, "brightnessJitter": 0.41, "purity": -0.16
            }}}),
        )
        .unwrap();
    let before = h.state().session.tools.brush.clone();
    h.run_steps(2);
    let field = h.query_all_by_role(Role::SpinButton).find(|n| n.accesskit_node().numeric_value() == Some(37.0)).unwrap();
    let center = field.rect().center();
    field.click();
    h.key_press(egui::Key::ArrowUp);
    h.run_steps(2);
    assert_eq!(h.state().session.tools.brush.color_dynamics.hue_jitter, 0.38);
    h.key_press(egui::Key::Enter);
    // Click near the high end of the track, then beyond its right edge while dragging.
    let from = egui::pos2(center.x, center.y + 24.0);
    h.event(egui::Event::PointerMoved(from));
    h.event(egui::Event::PointerButton { pos: from, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE });
    h.run_steps(1);
    let to = from + vec2(200.0, 0.0);
    h.event(egui::Event::PointerMoved(to));
    h.run_steps(1);
    h.event(egui::Event::PointerButton { pos: to, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE });
    h.run_steps(2);
    let mut expected = before;
    expected.color_dynamics.hue_jitter = 1.0;
    assert_eq!(h.state().session.tools.brush, expected, "the amount clamps at 100% and preserves neighboring settings");
    assert_eq!(last_journal(h.state()).unwrap().0, "tools.setBrush");
    let mut replay = photocraft_engine::Session::new();
    for (id, params) in h.state().session.journal.clone() {
        replay.execute(&id, params).unwrap();
    }
    assert_eq!(replay.tools.brush, expected, "the edit remains replayable through the engine");
}

#[test]
fn visible_flag_rows_enable_and_select_their_controls() {
    use egui_kittest::kittest::Queryable;
    for (section, &(name, has_box)) in SECTIONS.iter().enumerate().filter(|(i, _)| section_visible(*i)) {
        if !has_box {
            continue;
        }
        let mut h = harness(0, 0);
        set_section_enabled(&mut h.state_mut().session.tools.brush, section, false);
        h.run_steps(2);
        let n = h.state().session.journal.len();
        h.get_by_label(name).click();
        h.run_steps(3);
        assert_eq!(h.state().ui.brush_section, section, "{name} controls selected");
        assert!(toggle_value(h.state(), section), "{name} enabled");
        assert_eq!(h.state().session.journal.len(), n + 1, "one command for {name}");
        h.get_by_label(name).click();
        h.run_steps(3);
        assert!(toggle_value(h.state(), section), "selecting {name} again keeps it enabled");
        assert_eq!(h.state().session.journal.len(), n + 1, "reselecting {name} changes nothing");
    }
}

#[test]
fn visible_enable_boxes_toggle_without_changing_selected_controls() {
    use egui::accesskit::Role;
    use egui_kittest::kittest::{NodeT, Queryable};
    for (section, &(name, has_box)) in SECTIONS.iter().enumerate().filter(|(i, _)| section_visible(*i)) {
        if !has_box {
            continue;
        }
        let mut h = harness(0, 0);
        let label = format!("Enable {name}");
        let node = h.get_by_label(&label);
        assert_eq!(node.accesskit_node().role(), Role::CheckBox, "{name}");
        node.focus();
        h.run_steps(2);
        let before = toggle_value(h.state(), section);
        let n = h.state().session.journal.len();
        h.key_press(egui::Key::Space);
        h.run_steps(3);
        assert_eq!(toggle_value(h.state(), section), !before, "Space toggles {name}");
        assert_eq!(h.state().ui.brush_section, 0, "{name} enable box keeps controls selected");
        assert_eq!(h.state().session.journal.len(), n + 1);
    }
}

#[test]
fn obsolete_section_indices_recover_to_tip() {
    for section in 8..17 {
        let h = harness(0, section);
        assert_eq!(h.state().ui.brush_section, 0);
    }
}

#[test]
fn brush_options_bar_has_no_independent_smoothing_controls() {
    use egui_kittest::kittest::Queryable;
    for tool in [crate::state::Tool::Brush, crate::state::Tool::Pencil, crate::state::Tool::Eraser] {
        let h = options_bar_harness(tool);
        for label in ["Smoothing", "Smoothing:", "Stroke Smoothing", "Anchor Smooth", "Set additional smoothing options"] {
            assert!(h.query_by_label(label).is_none(), "{tool:?}: {label}");
        }
    }
}

#[test]
fn motion_panel_rope_stays_editable_when_anchor_motion_is_disabled() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness(0, 2);
    assert!(!h.state().session.tools.brush.motion.enabled);
    h.get_by_label("Pulled String Mode").click();
    h.run_steps(3);
    let brush = &h.state().session.tools.brush;
    assert!(brush.smoothing.enabled && brush.smoothing.pulled_string && brush.smoothing.amount > 0.0);
    assert!(!brush.motion.enabled);
    assert_eq!(last_journal(h.state()).unwrap().0, "tools.setBrush");
}

/// #4282: every name in the section list starts in the same column, Brush Tip Shape included,
/// whether or not its row has an enable box (Photoshop indents the rows without one).
#[test]
fn section_names_line_up_whether_or_not_they_have_a_box() {
    fn text_x(shape: &egui::Shape, out: &mut Vec<(String, f32)>) {
        match shape {
            egui::Shape::Text(text) => out.push((text.galley.job.text.clone(), text.pos.x)),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| text_x(s, out)),
            _ => {}
        }
    }
    // Shape Dynamics is shown, so its heading paints too; the list's copy is the leftmost.
    let h = harness(0, 1);
    let mut painted = Vec::new();
    for shape in &h.output().shapes {
        text_x(&shape.shape, &mut painted);
    }
    let left = |name: &str| painted.iter().filter(|(t, _)| t == name).map(|(_, x)| *x).reduce(f32::min);
    let columns: Vec<(&str, f32)> = SECTIONS
        .iter()
        .enumerate()
        .filter(|(i, _)| section_visible(*i))
        .map(|(_, (name, _))| (*name, left(name).unwrap_or_else(|| panic!("{name} painted"))))
        .collect();
    let unboxed = columns[0].1;
    for (i, (name, x)) in columns.iter().enumerate() {
        let expected = unboxed + if SECTIONS[i].1 { 20.0 } else { 0.0 };
        assert!((x - expected).abs() < 0.5, "{name} starts at {x}, expected {expected}: {columns:?}");
    }
}

#[test]
fn preview_strip_re_renders_only_when_the_brush_changes() {
    let mut h = harness(0, 0);
    let ctx = h.ctx.clone();
    let n0 = brush_preview::render_count(&ctx);
    assert!(n0 >= 1, "the strip rendered");
    h.run_steps(6);
    assert_eq!(brush_preview::render_count(&ctx), n0, "idle frames re-rendered a preview");
    // Change the brush through the engine: the strip renders once more, then stays cached.
    h.state_mut().run("tools.setBrush", json!({"brush": {"scattering": {"enabled": true, "scatter": {"jitter": 2.0}}}})).unwrap();
    h.run_steps(5);
    assert_eq!(brush_preview::render_count(&ctx), n0 + 1);
    // Colour and the jitter seed of strokes aren't preview inputs: no re-render.
    h.state_mut().session.tools.brush.color = [1.0, 0.0, 0.0, 1.0];
    h.run_steps(3);
    assert_eq!(brush_preview::render_count(&ctx), n0 + 1);
}

#[test]
fn every_section_draws_without_issuing_commands() {
    for tab in 0..3 {
        for section in 0..SECTIONS.len() {
            // Tab 2: the Brushes tab in grid view.
            let mut h = harness(tab.min(1), section);
            if tab == 2 {
                h.state_mut().ui.brushes_panel.view = BrushesView::Grid;
            }
            // A brush with every section on and a sampled tip.
            let mut b = BrushSettings::default();
            for i in 0..SECTIONS.len() {
                edit_section(i, &mut b);
            }
            b.tip = TipShape::Sampled(GrayTile::from_fn(50, 40, |x, y| ((x ^ y) & 1) as f32));
            h.state_mut().session.tools.brush = b.clone();
            let n = h.state().session.journal.len();
            h.run_steps(4);
            // Merely showing the panel must not rewrite the brush (no rounding drift, no commands).
            assert_eq!(h.state().session.journal.len(), n, "tab {tab} section {section}");
            assert_eq!(h.state().session.tools.brush, b, "tab {tab} section {section}");
        }
    }
}

#[test]
fn brushes_panel_groups_collapse_and_filter() {
    let mut h = harness(1, 0);
    let groups = grouped_presets(&h.state().session.tools.presets);
    assert!(groups.len() >= 2, "{groups:?}");
    let ctx = h.ctx.clone();
    let shown = |h: &mut Harness<'static, PhotocraftApp>| {
        h.run_steps(2);
        brush_preview::with_cache(&ctx, |c| c.len())
    };
    let all = shown(&mut h);
    // Collapse every group: no more previews are needed, and the old ones are kept (cached).
    h.state_mut().ui.brushes_panel.collapsed = groups.iter().map(|(g, _)| g.clone()).collect();
    let n = brush_preview::render_count(&ctx);
    assert_eq!(shown(&mut h), all);
    assert_eq!(brush_preview::render_count(&ctx), n);
    // A filter shows matching presets even in collapsed groups.
    let first = h.state().session.tools.presets[0].name.clone();
    h.state_mut().ui.brushes_panel.filter = first.to_uppercase();
    h.run_steps(2);
    assert!(brush_preview::render_count(&ctx) >= n);
}

fn kit(name: &str, folder: &[&str]) -> paint::BrushPreset {
    paint::BrushPreset {
        name: name.into(),
        brush: Default::default(),
        builtin: false,
        group: "Kit".into(),
        folder: folder.iter().map(|s| s.to_string()).collect(),
        captures: Default::default(),
    }
}

#[test]
fn folder_tree_nests_presets_in_panel_order() {
    use crate::brushes_tab::{MAX_VIEW_DEPTH, Node, folder_tree};
    let deep: Vec<&str> = vec!["d"; MAX_VIEW_DEPTH + 5];
    let presets = [kit("Top", &[]), kit("Pen", &["Inks"]), kit("Chalk", &["Dry"]), kit("Nib", &["Inks", "Fine"]), kit("Quill", &["Inks"]), kit("Deep", &deep)];
    let items: Vec<&paint::BrushPreset> = presets.iter().collect();
    let tree = folder_tree(&items);
    fn show(nodes: &[Node], out: &mut Vec<String>) {
        for n in nodes {
            match n {
                Node::Preset(p) => out.push(p.name.clone()),
                Node::Folder(f) => {
                    out.push(format!("[{} {}]", f.path.join("/"), f.count));
                    show(&f.children, out);
                    out.push("[end]".into());
                }
            }
        }
    }
    let mut out = Vec::new();
    show(&tree, &mut out);
    let deep_path = vec!["d"; MAX_VIEW_DEPTH].join("/");
    assert_eq!(out[..12], ["Top", "[Inks 3]", "Pen", "[Inks/Fine 1]", "Nib", "[end]", "Quill", "[end]", "[Dry 1]", "Chalk", "[end]", "[d 1]"]);
    assert!(out.contains(&format!("[{deep_path} 1]")), "paths are cut at the view depth");
    assert_eq!(out.iter().filter(|s| s.starts_with("[d")).count(), MAX_VIEW_DEPTH);
}

/// Nested folders show under their group, open and close, and their context menu renames and
/// deletes them through the group commands (#1851).
#[test]
fn brushes_panel_shows_nested_folders() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness(1, 0);
    let builtin_groups: Vec<String> = grouped_presets(&h.state().session.tools.presets).into_iter().map(|(g, _)| g).collect();
    h.state_mut().ui.brushes_panel.collapsed = builtin_groups;
    for p in [kit("Kit Top", &[]), kit("Kit Pen", &["Test Inks"]), kit("Kit Nib", &["Test Inks", "Test Fine"]), kit("Kit Chalk", &["Test Dry"])] {
        h.state_mut().session.tools.presets.push(p);
    }
    h.run_steps(3);
    for label in ["Kit", "Test Inks", "Test Fine", "Test Dry", "Kit Nib", "Kit Chalk"] {
        assert!(h.query_by_label(label).is_some(), "{label} is shown");
    }
    // Closing a folder hides what it holds, and only that.
    h.get_by_label("Test Inks").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.brushes_panel.collapsed.last().map(String::as_str), Some("Kit/Test Inks"));
    assert!(h.query_by_label("Kit Nib").is_none() && h.query_by_label("Test Fine").is_none());
    assert!(h.query_by_label("Kit Chalk").is_some());
    // Rename a nested folder from its context menu.
    h.get_by_label("Test Dry").click_secondary();
    h.run_steps(2);
    h.get_by_label("Rename Group…").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.brushes_panel.renaming.as_ref().map(|r| r.folder.clone()), Some(vec!["Test Dry".to_string()]));
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.event(egui::Event::Text("Test Wet".into()));
    h.run_steps(1);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert_eq!(
        last_journal(h.state()).unwrap(),
        ("brush.presets.renameGroup".to_string(), json!({ "group": "Kit", "folder": ["Test Dry"], "newName": "Test Wet" }))
    );
    assert_eq!(paint::presets::find(&h.state().session.tools.presets, "Kit Chalk").unwrap().folder, ["Test Wet"]);
    // Delete it: only its presets go.
    h.get_by_label("Test Wet").click_secondary();
    h.run_steps(2);
    h.get_by_label("Delete Group").click();
    h.run_steps(3);
    assert_eq!(last_journal(h.state()).unwrap(), ("brush.presets.deleteGroup".to_string(), json!({ "group": "Kit", "folder": ["Test Wet"] })));
    let kit_left: Vec<String> = h.state().session.tools.presets.iter().filter(|p| p.group == "Kit").map(|p| p.name.clone()).collect();
    assert_eq!(kit_left, ["Kit Top", "Kit Pen", "Kit Nib"]);
}

#[test]
fn folder_actions_become_folder_commands() {
    use crate::brushes_tab::{Action, apply, drop_target};
    let mut app = app();
    for p in [kit("Kit Top", &[]), kit("Kit Pen", &["Inks"]), kit("Kit Nib", &["Inks"]), kit("Kit Chalk", &["Dry"])] {
        app.session.tools.presets.push(p);
    }
    // Dropping a preset on one inside a folder moves it into that folder.
    let (group, folder, index) = drop_target(&app.session.tools.presets, "Kit Top", "Kit Nib", false).unwrap();
    assert_eq!((group.as_str(), folder.clone(), index), ("Kit", vec!["Inks".to_string()], 1));
    apply(&mut app, vec![Action::Move { name: "Kit Top".into(), group, folder: Some(folder), index: Some(index) }]);
    assert_eq!(paint::presets::find(&app.session.tools.presets, "Kit Top").unwrap().folder, ["Inks"]);
    // Dropping on a folder header appends to the folder; a folder header dropped on a sibling's moves it.
    apply(&mut app, vec![Action::Move { name: "Kit Pen".into(), group: "Kit".into(), folder: Some(vec!["Dry".into()]), index: None }]);
    assert_eq!(paint::presets::find(&app.session.tools.presets, "Kit Pen").unwrap().folder, ["Dry"]);
    apply(&mut app, vec![Action::MoveGroup { group: "Kit".into(), folder: vec!["Dry".into()], before: Some("Inks".into()) }]);
    assert_eq!(photocraft_engine::brush_preset_cmds::subfolder_order(&app.session.tools.presets, "Kit", &[]), ["Dry", "Inks"]);
    assert_eq!(last_journal(&app).unwrap(), ("brush.presets.moveGroup".to_string(), json!({ "group": "Kit", "folder": ["Dry"], "before": "Inks" })));
}

/// Dragging a folder or group header onto another header (#2014): the upper edge places it
/// before, the rest puts it inside; the action runs `brush.presets.moveGroup` with `into`.
#[test]
fn folder_drops_map_to_move_commands() {
    use crate::brushes_tab::{Action, apply, folder_drop};
    let v = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    // Into: a folder onto another folder, a group onto a group.
    assert_eq!(
        folder_drop("Kit", &v(&["Dry"]), "Kit", &v(&["Inks"]), false),
        Some(Action::MoveInto { group: "Kit".into(), folder: v(&["Dry"]), into: Some(("Kit".into(), v(&["Inks"]))), before: None })
    );
    assert_eq!(
        folder_drop("Pack", &[], "Kit", &v(&["Inks"]), false),
        Some(Action::MoveInto { group: "Pack".into(), folder: vec![], into: Some(("Kit".into(), v(&["Inks"]))), before: None })
    );
    // Onto itself, into its own folder, or into the folder it is already in: nothing.
    assert_eq!(folder_drop("Kit", &v(&["Inks"]), "Kit", &v(&["Inks"]), false), None);
    assert_eq!(folder_drop("Kit", &v(&["Inks"]), "Kit", &v(&["Inks", "Fine"]), true), None);
    assert_eq!(folder_drop("Kit", &[], "Kit", &v(&["Inks"]), false), None);
    assert_eq!(folder_drop("Kit", &v(&["Inks", "Fine"]), "Kit", &v(&["Inks"]), false), None);
    // Upper edge: siblings reorder; a nested folder before a group goes to the top level; a
    // folder before a folder elsewhere moves next to it.
    assert_eq!(
        folder_drop("Kit", &v(&["Dry"]), "Kit", &v(&["Inks"]), true),
        Some(Action::MoveGroup { group: "Kit".into(), folder: v(&["Dry"]), before: Some("Inks".into()) })
    );
    assert_eq!(folder_drop("Pack", &[], "Kit", &[], true), Some(Action::MoveGroup { group: "Pack".into(), folder: vec![], before: Some("Kit".into()) }));
    assert_eq!(
        folder_drop("Kit", &v(&["Inks", "Fine"]), "Pack", &[], true),
        Some(Action::MoveInto { group: "Kit".into(), folder: v(&["Inks", "Fine"]), into: None, before: Some("Pack".into()) })
    );
    assert_eq!(
        folder_drop("Kit", &v(&["Inks", "Fine"]), "Pack", &v(&["Box"]), true),
        Some(Action::MoveInto { group: "Kit".into(), folder: v(&["Inks", "Fine"]), into: Some(("Pack".into(), vec![])), before: Some("Box".into()) })
    );

    let mut app = app();
    for p in [kit("Kit Pen", &["Inks"]), kit("Kit Nib", &["Inks", "Fine"]), kit("Kit Chalk", &["Dry"])] {
        app.session.tools.presets.push(p);
    }
    let into = folder_drop("Kit", &v(&["Dry"]), "Kit", &v(&["Inks", "Fine"]), false).unwrap();
    apply(&mut app, vec![into]);
    assert_eq!(paint::presets::find(&app.session.tools.presets, "Kit Chalk").unwrap().folder, ["Inks", "Fine", "Dry"]);
    assert_eq!(
        last_journal(&app).unwrap(),
        ("brush.presets.moveGroup".to_string(), json!({ "group": "Kit", "folder": ["Dry"], "into": {"group": "Kit", "folder": ["Inks", "Fine"]} }))
    );
    // Out to the top level, before a group.
    let out = folder_drop("Kit", &v(&["Inks", "Fine"]), "Kit", &[], true).unwrap();
    apply(&mut app, vec![out]);
    let chalk = paint::presets::find(&app.session.tools.presets, "Kit Chalk").unwrap();
    assert_eq!((chalk.group.as_str(), chalk.folder.clone()), ("Fine", v(&["Dry"])));
    let order = photocraft_engine::brush_preset_cmds::group_order(&app.session.tools.presets);
    assert!(order.iter().position(|g| g == "Fine") < order.iter().position(|g| g == "Kit"));
}

/// A folder header dragged onto another folder's header in the Brushes panel goes inside it.
#[test]
fn dragging_a_folder_header_onto_another_moves_it_inside() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness(1, 0);
    let builtin_groups: Vec<String> = grouped_presets(&h.state().session.tools.presets).into_iter().map(|(g, _)| g).collect();
    h.state_mut().ui.brushes_panel.collapsed = builtin_groups;
    for p in [kit("Kit Pen", &["Test Inks"]), kit("Kit Chalk", &["Test Dry"])] {
        h.state_mut().session.tools.presets.push(p);
    }
    h.run_steps(3);
    let from = h.get_by_label("Test Dry").rect().center();
    let to = h.get_by_label("Test Inks").rect();
    let to = egui::pos2(to.center().x, to.top() + to.height() * 0.7);
    h.hover_at(from);
    h.run();
    h.drag_at(from);
    h.run();
    for k in 1..=6 {
        h.hover_at(from + (to - from) * (k as f32 / 6.0));
        h.run();
    }
    h.drop_at(to);
    h.run_steps(3);
    assert_eq!(paint::presets::find(&h.state().session.tools.presets, "Kit Chalk").unwrap().folder, ["Test Inks", "Test Dry"]);
    assert_eq!(last_journal(h.state()).unwrap().0, "brush.presets.moveGroup");
}

/// A fresh session with the Brush tool: shortcuts, the options bar and the Brush Settings window,
/// in the default theme (#258).
fn app_harness() -> Harness<'static, PhotocraftApp> {
    app_harness_with_language(crate::i18n::Lang::EN)
}

fn app_harness_with_language(lang: crate::i18n::Lang) -> Harness<'static, PhotocraftApp> {
    let mut app = app();
    app.ui.tool = crate::state::Tool::Brush;
    // This harness draws the toolbar directly, without document_area assigning its bounds.
    app.last_canvas_rect = egui::Rect::from_min_max(egui::pos2(0.0, 80.0), egui::pos2(1400.0, 900.0));
    let mut h = Harness::builder().with_size(vec2(1400.0, 900.0)).build_ui_state(
        move |ui, app: &mut PhotocraftApp| {
            let ctx = ui.ctx().clone();
            if !ctx.fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                return;
            }
            let previous = crate::i18n::current();
            crate::i18n::set_current(lang);
            crate::shortcuts::handle(app, &ctx);
            crate::panels::options_bar(app, ui);
            window(app, &ctx);
            crate::paint_mouse::show_picker(app, &ctx);
            crate::i18n::set_current(previous);
        },
        app,
    );
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::default());
    h.run_steps(4);
    h
}

#[test]
fn brush_sections_paint_simplified_chinese_labels() {
    fn collect_text(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => out.push(text.galley.job.text.clone()),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text(shape, out);
                }
            }
            _ => {}
        }
    }
    let zh = crate::i18n::Lang::from_code("zh-hans").unwrap();
    let mut h = app_harness_with_language(zh);
    h.key_press(egui::Key::F5);
    h.run_steps(3);
    let mut painted = Vec::new();
    for shape in &h.output().shapes {
        collect_text(&shape.shape, &mut painted);
    }
    for (_, &(name, _)) in SECTIONS.iter().enumerate().filter(|(i, _)| section_visible(*i)) {
        let translated = crate::i18n::tr_ctx(zh, "brushSection", name);
        assert!(painted.iter().any(|s| s == translated), "{name} must paint as {translated}");
        assert!(!painted.iter().any(|s| s == name), "{name} must not paint in English");
    }
}

/// #3477: the options pane has no title of its own. The selected row in the section list is the
/// only place the section's name is painted.
#[test]
fn options_pane_does_not_repeat_the_selected_sections_name() {
    fn collect_text(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => out.push(text.galley.job.text.clone()),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_text(s, out)),
            _ => {}
        }
    }
    for (section, (name, _)) in SECTIONS.iter().enumerate().filter(|(i, _)| section_visible(*i)) {
        let h = harness(0, section);
        let mut painted = Vec::new();
        for shape in &h.output().shapes {
            collect_text(&shape.shape, &mut painted);
        }
        assert_eq!(painted.iter().filter(|s| s.as_str() == *name).count(), 1, "{name} is painted once, in the section list");
    }
}

#[test]
fn f5_edits_the_default_brush_with_every_section() {
    use egui_kittest::kittest::Queryable;
    let mut h = app_harness();
    assert!(!h.state().ui.panels.brush_settings);
    let fresh = h.state().session.tools.brush.clone();
    assert!(!fresh.shape_dynamics.enabled && !fresh.transfer.enabled, "the default round brush has no dynamics yet");
    h.key_press(egui::Key::F5);
    h.run_steps(3);
    assert!(h.state().ui.panels.brush_settings && h.state().ui.brush_tab == 0, "F5 shows Brush Settings");
    // Every section is there for the built-in round brush, not only for sampled or imported ones.
    for (_, &(name, _)) in SECTIONS.iter().enumerate().filter(|(i, _)| section_visible(*i)) {
        assert!(h.query_all_by_label(name).next().is_some(), "{name} listed");
    }
    // Basic is always available; selecting it does not toggle engine settings.
    let n = h.state().session.journal.len();
    h.get_by_label("Basic").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.brush_section, 1);
    assert_eq!(h.state().session.journal.len(), n);
    assert!(h.query_by_label("Enable Basic").is_none());
    // Color Mode has a category switch and preserves the selected Basic controls.
    h.get_by_label("Enable Color Mode").click();
    h.run_steps(3);
    assert!(h.state().session.tools.brush.color_attributes.enabled);
    assert!(h.state().session.tools.brush.color_dynamics.enabled);
    assert_eq!(h.state().ui.brush_section, 1);
    assert_eq!(h.state().session.journal.len(), n + 1);
    let (id, p) = last_journal(h.state()).unwrap();
    assert_eq!(id, "tools.setBrush");
    assert_eq!(p["brush"]["colorAttributes"]["enabled"], json!(true));
    // The rest of the brush is untouched.
    let b = &h.state().session.tools.brush;
    assert_eq!((b.size, b.hardness, &b.tip), (fresh.size, fresh.hardness, &fresh.tip));
    // F5 again hides the panel.
    h.key_press(egui::Key::F5);
    h.run_steps(2);
    assert!(!h.state().ui.panels.brush_settings);
}

#[test]
fn update_current_brush_button_overwrites_the_picked_preset_in_place() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness(0, 0);
    // Nothing picked yet: the button is disabled and clicking it does nothing.
    assert!(h.state().session.tools.current_preset.is_none());
    h.get_by_label("Update the current brush with these settings").click();
    h.run_steps(3);
    assert!(!h.state().session.journal.iter().any(|(id, _)| id == "brush.presets.update"));
    // Pick a preset, then turn a section on.
    h.state_mut().run("tools.setBrush", json!({ "preset": "Chalk" })).unwrap();
    h.run_steps(2);
    let n = h.state().session.tools.presets.len();
    let at = h.state().session.tools.presets.iter().position(|p| p.name == "Chalk").unwrap();
    h.get_by_label("Enable Color Mode").click();
    h.run_steps(3);
    assert!(h.state().session.tools.brush.color_attributes.enabled);
    // The update button overwrites the picked preset in place: no new preset, position kept.
    h.get_by_label("Update the current brush with these settings").click();
    h.run_steps(3);
    assert!(h.state().session.journal.iter().any(|(id, p)| id == "brush.presets.update" && p["brush"]["colorAttributes"]["enabled"] == json!(true)));
    assert_eq!(h.state().session.tools.presets.len(), n);
    let chalk = h.state().session.tools.presets.get(at).unwrap();
    assert_eq!(chalk.name, "Chalk");
    assert!(chalk.brush.color_attributes.enabled);
    assert!(!chalk.builtin, "an updated built-in becomes the user's preset");
}

/// Each painting tool keeps its own brush (#218): the preset picked for one tool is not the
/// current preset of the next, so Update can't overwrite it with the other tool's brush.
#[test]
fn switching_tools_forgets_the_picked_preset() {
    let mut app = app();
    app.ui.tool = crate::state::Tool::Brush;
    crate::paint_mouse::sync_tool_brush(&mut app);
    app.run("tools.setBrush", json!({ "preset": "Chalk" })).unwrap();
    assert_eq!(app.session.tools.current_preset.as_deref(), Some("Chalk"));
    app.ui.tool = crate::state::Tool::Eraser;
    crate::paint_mouse::sync_tool_brush(&mut app);
    assert_eq!(app.session.tools.current_preset, None);
    assert!(app.run("brush.presets.update", json!({})).is_err());
}

#[test]
fn options_bar_reaches_brush_settings_and_the_preset_library() {
    use egui_kittest::kittest::Queryable;
    let mut h = app_harness();
    // Photoshop's "Toggle the Brush Settings panel" button beside the brush chip.
    h.get_by_label("Toggle the Brush Settings panel").click();
    h.run_steps(3);
    assert!(h.state().ui.panels.brush_settings);
    h.get_by_label("Toggle the Brush Settings panel").click();
    h.run_steps(3);
    assert!(!h.state().ui.panels.brush_settings);
    // The chip opens the same picker a right-click on the canvas does, below the chip; it lists
    // the session's presets in their groups, not a fixed set of tips.
    h.get_by_label("Brush Preset picker").click();
    h.run_steps(3);
    let chip = h.get_by_label("Brush Preset picker").rect();
    let at = h.state().ui.brush_picker.expect("the chip opened the picker");
    assert!((at[0] - chip.left()).abs() < 1.0 && at[1] >= chip.bottom(), "{at:?} below {chip:?}");
    let presets = h.state().session.tools.presets.clone();
    for (group, _) in grouped_presets(&presets) {
        assert!(h.query_by_label(&group).is_some(), "group {group}");
    }
    let target = presets.iter().find(|p| p.group == "Dry Media").or(presets.last()).unwrap().name.clone();
    assert_ne!(h.state().session.tools.current_preset.as_deref(), Some(target.as_str()));
    // The picker's cards are tall: scroll the target into the list before clicking it.
    h.get_by_label(&target).scroll_to_me();
    h.run_steps(3);
    h.get_by_label(&target).click();
    h.run_steps(3);
    assert_eq!(last_journal(h.state()).unwrap(), ("tools.setBrush".to_string(), json!({ "preset": target })));
    assert_eq!(h.state().session.tools.current_preset.as_deref(), Some(target.as_str()));
    // A click picks and keeps the picker open; the chip's click closes it again.
    assert!(h.state().ui.brush_picker.is_some());
    h.get_by_label("Brush Preset picker").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.brush_picker, None, "the chip toggles the picker");
    // Its Brush Settings button opens the full editor and closes the picker.
    h.get_by_label("Brush Preset picker").click();
    h.run_steps(3);
    h.get_by_label("Brush Settings…").click();
    h.run_steps(3);
    assert!(h.state().ui.panels.brush_settings && h.state().ui.brush_tab == 0);
    assert_eq!(h.state().ui.brush_picker, None);
}

/// A preset's context menu (the Brushes tab's and the picker's) renames it in the rename bar.
#[test]
fn context_menu_renames_a_preset() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness(1, 0);
    let name = h.state().session.tools.presets[0].name.clone();
    h.get_by_label(&name).click_secondary();
    h.run_steps(2);
    h.get_by_label("Rename Brush…").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.brushes_panel.renaming.as_ref().map(|r| r.name.clone()), Some(name.clone()), "the rename bar opened");
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.event(egui::Event::Text("Renamed".into()));
    h.run_steps(1);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert!(paint::presets::find(&h.state().session.tools.presets, "Renamed").is_some());
    assert!(h.state().ui.brushes_panel.renaming.is_none());
    assert_eq!(last_journal(h.state()).unwrap(), ("brush.presets.rename".to_string(), json!({ "name": name, "newName": "Renamed" })));
}

#[test]
fn hand_tool_offers_scale_buttons_and_requests_the_right_view() {
    use egui_kittest::kittest::Queryable;

    let mut app = app();
    app.run("file.new", json!({"width": 400, "height": 200})).unwrap();
    app.sync_views();
    app.ui.tool = crate::state::Tool::Hand;
    app.ui.views[0].zoom = 2.0;
    let mut h = Harness::builder().with_size(vec2(2000.0, 60.0)).build_ui_state(
        |ui, app: &mut PhotocraftApp| {
            if ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                crate::panels::options_bar(app, ui);
            }
        },
        app,
    );
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::Studio);
    h.run_steps(4);

    h.get_by_label("100%").click();
    h.run_steps(1);
    assert_eq!(h.state().ui.views[0].zoom, 1.0);

    h.get_by_label("Fit Screen").click();
    h.run_steps(1);
    assert!(h.state().ui.views[0].fit_pending);
    assert!(!h.state().ui.views[0].fill_pending);

    h.get_by_label("Fill Screen").click();
    h.run_steps(1);
    assert!(h.state().ui.views[0].fill_pending);
    assert!(!h.state().ui.views[0].fit_pending);
}

#[test]
fn a_dynamic_gets_its_own_response_curve() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness(0, 1);
    h.state_mut().run("tools.setBrush", json!({"shapeDynamics": {"enabled": true, "size": {"control": "penPressure"}}})).unwrap();
    h.run_steps(3);
    // Only Size has a control with an input to shape; its curve button shows the editor.
    assert_eq!(h.query_all_by_label("Response curve").count(), 1);
    h.get_by_label("Response curve").scroll_to_me();
    h.run_steps(3);
    h.get_by_label("Response curve").click();
    h.run_steps(3);
    let content = h.get_by_label("Basic").rect();
    h.hover_at(egui::pos2(content.right() + 180.0, content.center().y + 160.0));
    for _ in 0..8 {
        if h.query_by_label("Soft").is_some() {
            break;
        }
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, -80.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(3);
    }
    h.get_by_label("Soft").scroll_to_me();
    h.run_steps(3);
    h.get_by_label("Soft").click();
    h.run_steps(3);
    let (id, p) = last_journal(h.state()).unwrap();
    assert_eq!(id, "tools.setBrush");
    let curve: Vec<[f32; 2]> = serde_json::from_value(p["brush"]["shapeDynamics"]["size"]["curve"].clone()).unwrap();
    assert_eq!(curve, vec![[0.0, 0.0], [0.3, 0.6], [1.0, 1.0]]);
    let b = &h.state().session.tools.brush;
    assert_eq!(b.shape_dynamics.size.curve.points(), curve);
    assert!(b.shape_dynamics.roundness.curve.is_linear() && b.transfer.opacity.curve.is_linear());
}

#[test]
fn brush_settings_hamburger_menu_actions() {
    use egui_kittest::kittest::Queryable;

    // 1. Menu opens and offers Photoshop actions
    let mut h = harness(0, 0);
    h.get_by_label("Panel menu").click();
    h.run_steps(2);

    assert!(h.query_all_by_label("New Brush Preset…").count() > 0);
    assert!(h.query_all_by_label("Clear Brush Controls").count() > 0);
    assert!(h.query_all_by_label("Reset All Locked Settings").count() > 0);
    assert!(h.query_all_by_label("Copy Texture to Other Tools").count() > 0);
    assert!(h.query_all_by_label("Close").count() > 0);
    assert!(h.query_all_by_label("Close Tab Group").count() > 0);

    // 2. New Brush Preset… saves a preset
    let count0 = h.state().session.tools.presets.len();
    h.get_by_label("New Brush Preset…").click();
    h.run_steps(2);
    assert_eq!(h.state().session.tools.presets.len(), count0 + 1);
    assert_eq!(last_journal(h.state()).unwrap().0, "brush.presets.save");

    // 3. Clear Brush Controls resets dynamics and rope, keeping locked sections.
    let mut h = harness(0, 0);
    h.state_mut().session.tools.brush.size = 45.0;
    h.state_mut().session.tools.brush.scattering.enabled = true;
    h.state_mut().session.tools.brush.texture.enabled = true;
    h.state_mut().session.tools.brush.locks.texture = true;
    h.state_mut().session.tools.brush.smoothing.amount = 0.5;
    h.run_steps(1);

    h.get_by_label("Panel menu").click();
    h.run_steps(2);
    h.get_by_label("Clear Brush Controls").click();
    h.run_steps(2);

    let b = &h.state().session.tools.brush;
    assert!(!b.scattering.enabled, "unlocked scattering cleared");
    assert!(b.texture.enabled, "locked texture preserved");
    assert_eq!(b.smoothing.amount, 0.0, "rope controls reset with the brush");
    assert_eq!(b.size, 45.0, "tip size preserved");

    // 4. Reset All Locked Settings clears all locks
    assert!(b.locks.texture);
    h.get_by_label("Panel menu").click();
    h.run_steps(2);
    h.get_by_label("Reset All Locked Settings").click();
    h.run_steps(2);
    assert_eq!(h.state().session.tools.brush.locks, photocraft_paint::brush::SectionLocks::default());

    // 5. Copy Texture to Other Tools copies texture to other tools with texture enabled
    let mut h = harness(0, 0);
    h.state_mut().session.tools.brush.texture.enabled = true;
    h.state_mut().session.tools.brush.texture.scale = 2.5;
    let mut pencil_brush = BrushSettings::default();
    pencil_brush.texture.enabled = true;
    pencil_brush.texture.scale = 1.0;
    h.state_mut().ui.tool_brushes = vec![(crate::state::Tool::Pencil, pencil_brush)];
    h.run_steps(1);

    h.get_by_label("Panel menu").click();
    h.run_steps(2);
    h.get_by_label("Copy Texture to Other Tools").click();
    h.run_steps(2);

    let pencil = h.state().ui.tool_brushes.iter().find(|(t, _)| *t == crate::state::Tool::Pencil).unwrap();
    assert_eq!(pencil.1.texture.scale, 2.5);

    // 6. Close closes the window
    let mut h = harness(0, 0);
    assert!(h.state().ui.panels.brush_settings);
    h.get_by_label("Panel menu").click();
    h.run_steps(2);
    let close_buttons: Vec<_> = h.query_all_by_label("Close").collect();
    assert!(close_buttons.len() >= 2);
    close_buttons.last().unwrap().click();
    h.run_steps(2);
    assert!(!h.state().ui.panels.brush_settings);

    // 7. Close Tab Group closes the window
    let mut h = harness(0, 0);
    assert!(h.state().ui.panels.brush_settings);
    h.get_by_label("Panel menu").click();
    h.run_steps(2);
    h.get_by_label("Close Tab Group").click();
    h.run_steps(2);
    assert!(!h.state().ui.panels.brush_settings);
}

#[test]
fn brushes_button_at_top_of_brush_settings_opens_brushes_panel() {
    use egui_kittest::kittest::Queryable;

    let mut h = harness(0, 0);
    assert_eq!(h.state().ui.brush_tab, 0);
    h.get_by_label("Brushes").click();
    h.run_steps(3);
    assert!(h.state().ui.panels.brush_settings);
    assert_eq!(h.state().ui.brush_tab, 1);
}

/// The shared check box (options bars, dialogs) does the same, and pressing its label counts.
#[test]
fn checkboxes_show_a_pressed_state_from_their_label() {
    use egui_kittest::kittest::Queryable;
    let mut h = Harness::builder().with_size(vec2(300.0, 100.0)).build_ui_state(
        |ui, on: &mut bool| {
            widgets::checkbox(ui, on, "Sample All Layers");
        },
        false,
    );
    h.run();
    let t = Tokens::get(&h.ctx);
    let label = h.get_by_label("Sample All Layers").rect();
    let boxr = egui::Rect::from_min_size(egui::pos2(label.left() - 6.0 - 14.0, label.center().y - 7.0), vec2(14.0, 14.0));
    let fill = |h: &Harness<'_, bool>| {
        h.output()
            .shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::Shape::Rect(r) if (r.rect.center() - boxr.center()).length() < 1.5 && r.rect.width() < 15.0 && r.fill != Color32::TRANSPARENT => {
                    Some(r.fill)
                }
                _ => None,
            })
            .next_back()
    };
    assert_eq!(fill(&h), Some(widgets::check_box_fill(false, false, &t)));
    h.drag_at(label.center());
    h.run();
    assert_eq!(fill(&h), Some(widgets::check_box_fill(false, true, &t)), "pressing the label presses the box");
    h.drop_at(label.center());
    h.run();
    assert!(*h.state(), "the release checks it");
    assert_eq!(fill(&h), Some(widgets::check_box_fill(true, false, &t)));
}

#[test]
fn material_sections_open_controls_and_refresh_preview() {
    use egui_kittest::kittest::Queryable;
    for (section, name, parameter) in [(1, "Basic", "Grain Density"), (5, "Wet Edge", "Strength")] {
        let mut h = harness(0, 0);
        let before = h.state().session.tools.brush.clone();
        h.get_by_label(name).click();
        h.run_steps(3);
        assert_eq!(h.state().ui.brush_section, section);
        assert!(section == 1 || toggle_value(h.state(), section));
        assert!(h.state().session.tools.brush.grain.is_none() && h.state().session.tools.brush.wet_edge.is_none());
        let row = h.get_by_label(name).rect();
        h.hover_at(egui::pos2(row.right() + 180.0, row.center().y + 120.0));
        for _ in 0..50 {
            if h.query_by_label(parameter).is_some() {
                break;
            }
            h.event(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: vec2(0.0, -160.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            });
            h.run_steps(3);
        }
        h.get_by_label(parameter).scroll_to_me();
        h.run_steps(3);
        let label = h.get_by_label(parameter).rect();
        let field = h
            .query_all_by_role(egui::accesskit::Role::SpinButton)
            .find(|n| n.rect().left() > label.right() && (n.rect().center().y - label.center().y).abs() < 10.0)
            .expect("material parameter amount");
        field.click();
        h.key_press(egui::Key::ArrowUp);
        h.run_steps(2);
        h.key_press(egui::Key::Enter);
        h.run_steps(3);
        let brush = &h.state().session.tools.brush;
        if section == 1 {
            assert!(brush.grain.is_some());
        } else {
            assert!(brush.wet_edges && brush.wet_edge.is_some());
        }
        assert_ne!(preview_sig(&before), preview_sig(brush));
        let mut reset = clear_brush_controls(brush);
        assert!(reset.grain.is_none() && reset.wet_edge.is_none());
        reset.locks.noise = true;
        reset.locks.wet_edges = true;
        reset.grain = brush.grain.clone();
        reset.wet_edge = brush.wet_edge.clone();
        let kept = clear_brush_controls(&reset);
        assert_eq!(kept.grain, brush.grain);
        assert_eq!(kept.wet_edge, brush.wet_edge);
    }
}

#[test]
fn inline_pressure_buttons_commit_and_replay_independently() {
    use egui_kittest::kittest::Queryable;
    let mut h = options_bar_harness(crate::state::Tool::Brush);
    let initial = h.state().session.tools.brush.clone();
    for label in ["Pressure for Size", "Pressure for Opacity", "Pressure for Flow"] {
        let n = h.state().session.journal.len();
        h.get_by_label(label).click();
        h.run_steps(3);
        assert_eq!(h.state().session.journal.len(), n + 1, "{label}: one command");
    }
    let b = &h.state().session.tools.brush;
    assert_eq!(b.pressure_size, !initial.pressure_size);
    assert_eq!(b.pressure_opacity, !initial.pressure_opacity);
    assert_eq!(b.pressure_flow, !initial.pressure_flow);
    assert_eq!(b.transfer, initial.transfer);
    h.get_by_label("Pressure for Flow").click();
    h.run_steps(3);
    assert_eq!(h.state().session.tools.brush.pressure_flow, initial.pressure_flow);
    assert_eq!(h.state().session.tools.brush.opacity, initial.opacity);
    assert_eq!(h.state().session.tools.brush.flow, initial.flow);
    let mut replay = photocraft_engine::Session::new();
    for (id, params) in &h.state().session.journal {
        replay.execute(id, params.clone()).unwrap();
    }
    assert_eq!(replay.tools.brush, h.state().session.tools.brush);
}

#[test]
fn c3_section_masters_live_in_sidebar_and_preserve_parameters() {
    use egui_kittest::kittest::Queryable;
    for (section, label) in [(7, "Pigment"), (6, "Mix")] {
        let mut h = harness(0, 0);
        h.get_by_label(label).click();
        h.run_steps(3);
        assert!(section_flag(&mut h.state_mut().session.tools.brush, section).is_some_and(|f| *f));
        assert_eq!(h.state().ui.brush_section, section);
        h.get_by_label(&format!("Enable {label}")).click();
        h.run_steps(3);
        assert!(!*section_flag(&mut h.state_mut().session.tools.brush, section).unwrap());
        assert_eq!(h.state().ui.brush_section, section);
    }
}

#[test]
fn primary_and_secondary_panels_edit_only_their_own_lane() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness(0, 0);
    let primary = h.state().session.tools.brush.clone();
    h.get_by_label("Secondary Brush").click();
    h.run_steps(3);
    h.get_by_label("Mix").click();
    h.run_steps(3);
    let b = &h.state().session.tools.brush;
    assert_eq!(b.mix_settings, primary.mix_settings);
    assert!(b.dual_brush.full_parameters.as_ref().unwrap().mix_settings.enabled);
    assert!(!b.dual_brush.enabled, "editing does not enable dual painting");
    h.get_by_label("Primary Brush").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.brush_section, 0);
    h.get_by_label("Color Mode").click();
    h.run_steps(3);
    assert!(h.state().session.tools.brush.color_dynamics.enabled && h.state().session.tools.brush.color_attributes.enabled);
    let second = h.state().session.tools.brush.dual_brush.full_parameters.as_ref().unwrap();
    assert!(!second.color_attributes.enabled);
    h.get_by_label("Secondary Brush").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.brush_secondary_section, 6);
    assert!(h.get_by_label("Mixing Strength").rect().is_positive());
    assert!(h.query_by_label("Color Output").is_none());
}

#[test]
fn saving_secondary_panel_keeps_primary_and_full_secondary_settings() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness(0, 0);
    h.state_mut().run("tools.setBrush", json!({"preset":"Chalk"})).unwrap();
    h.run_steps(2);
    let primary = h.state().session.tools.brush.clone();
    h.get_by_label("Secondary Brush").click();
    h.run_steps(2);
    h.get_by_label("Mix").click();
    h.run_steps(3);
    h.get_by_label("Update the current brush with these settings").click();
    h.run_steps(3);
    let saved = &h.state().session.tools.presets.iter().find(|p| p.name == "Chalk").unwrap().brush;
    assert_eq!((saved.size, &saved.tip, &saved.texture), (primary.size, &primary.tip, &primary.texture));
    assert_eq!(saved.mix_settings, primary.mix_settings);
    assert!(saved.dual_brush.full_parameters.as_ref().unwrap().mix_settings.enabled);
}
