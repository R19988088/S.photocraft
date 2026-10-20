# Brush and painting parity

> **Last reviewed:** 2026-10-20 · **Last updated:** 2026-10-20 · **Change:** major (C3 brush controls, full dual lanes, colour attributes, pattern sampling, independent mixing and material pipelines, schema-5 presets) · **Target:** Adobe Photoshop 2026

Painting is where Photoshop users notice differences first, so it gets its own checklist: the
brush engine (`crates/paint/src/brush.rs`), the Brush Settings UI
(`crates/ui-egui/src/brush_sections.rs`), the live stroke renderer, pen input
([`hardware-parity.md`](hardware-parity.md)), and the brush-based tools. Status words as in
[`ui-parity.md`](ui-parity.md).

**Summary (estimated):** breadth ~90% (C3 painting controls are exposed; Photoshop-only options differ), ready for
real work ~55%. The engine models what Photoshop models; the gaps are feel (latency, pressure
response, edge quality, smoothing), a few missing options, and live previews for retouch tools.
Remaining: 100–180 h (gap G4 in [`gaps.md`](gaps.md)). Human: feel must be checked on real
tablets against Photoshop side by side.

## C3 painting system (2026-10-20)

The painting controls now follow C3 schema 5 within PhotoCraft’s existing egui panel. Brush,
Eraser and Smudge share the Rust dab generator and renderer, with tool restrictions. Both dual
brush lanes have full settings and separate strengths, motion, material, colour attributes and
transport state. Mixing v1 deliberately retains PhotoCraft’s original Smudge algorithm; v2 and
v3 use their own transport paths. Wet edges and pigment material run independently.

The settings sidebar follows the named C3 reference directory: Basic Parameters, Shape Controls,
Motion, Texture, Colour Attributes, Wet Edge, Mixing and Pigment. Basic Parameters and Shape
Controls stay enabled. Grain and inertia belong to Shape Controls. Eraser hides Colour Attributes
and Mixing. Each category limits control sources to the reference's supported choices.

The options bar keeps one attribute selector at its existing Mode position, with strength to its
right and the restored brush-size slider. Both attributes remain editable in Brush Settings.
Primary and secondary settings keep independent selected sections. The dual-brush header exposes
lane strengths and the reference blend modes. Mixing and blur curves have independent enable
switches; mixing can also deposit the foreground or sampled colour patch.

Hidden legacy brush options are neutralized in the render copy, including values loaded from
older presets. This covers pose, canvas material, saturation-memory overrides, legacy smoothing,
projection, speed spacing, unsupported control sources and transport tuning controls. Preset
serialization retains its existing schema. The renderer and UI remain Rust on native and wasm.

## Brush Settings sections

| Section | Status | Evidence |
|---|---|---|
| Basic Parameters: tip resource, size, spacing, angle, roundness | implemented | `brush_sections.rs::tip_shape` |
| Shape Controls: size, opacity, flow, hardness, dynamics, scatter, grain, inertia | implemented | `section_body_with_canvas`, category 1 |
| Motion: anchor, guidance, smoothing, delays | implemented | `motion_settings` |
| Texture: pattern, scale, tone, coordinates, depth, per-tip, invert, protection | implemented | `texture` |
| Colour Attributes: main/secondary attribute, strengths, colour dynamics | implemented | `color_attributes`, `color_dynamics` |
| Wet Edge: style, strength, width, softness and colour | implemented | `wet_settings` |
| Mixing: algorithm, enabled mixing/blur curves and colour output | implemented | `mix_settings`, `LaneTransport` |
| Pigment: thickness curve, texture strength and highlights | implemented | `pigment_settings` |
| Primary/secondary independent parameters and strengths | implemented | `settings_lane`, `dual_brush_header` |
| Non-reference legacy parameters and control combinations | disabled | `BrushSettings::bounded_for_render` |
| Brush tip from an image file | missing | TOOL-213-12 |
| Erodible, Airbrush and Bristle tips (Photoshop's special tips) | partial | not audited in detail; "3D-based brushes don't work" (#1660) |
| Anti-aliasing off for brushes | missing | #1666 (Pencil is the aliased path) |

## Pressure and pen

| Behaviour | Status | Evidence |
|---|---|---|
| Pressure drives size/opacity/flow per sample (no shared pressure within a frame) | done | #2112 |
| Global pressure curve in Preferences | done | #2195 |
| Per-control pressure curves | done | Dynamic curves and signed C3 effect curves. |
| Quick taps keep their pressure | partial | #1967 fixed one case; tap paints a full blob (#1798), tap sensitivity (#1933) |
| Smooth pressure on Windows | partial | "jumpy" (#2046) |
| Tilt and rotation drive angle/roundness | partial | macOS, X11, web; not Windows; rotation not applied (#1301) |
| Eraser end of the pen switches to Eraser | partial | macOS; flipped Wacom on Windows not (#2285) |

## Stroke quality and feel

| Behaviour | Status | Evidence |
|---|---|---|
| No seams, hard edges stay hard, zero-scatter defaults, ABR tips never black | done | TOOL-213-13 |
| Smooth line weight along a pressure ramp | partial | stair-steps (#1776) |
| Interpolation and smoothing match Photoshop | partial | #2138, #2057 |
| Soft-edge falloff matches Photoshop's curve | partial | #2149, #1619 |
| Brush preview and cursor crisp, not pixelated | partial | #1433 |
| Cursor outline equals the painted footprint at every zoom | partial | #1761, #2364; shaped tips trace their outline (#3467), turned and flipped with a rotated or flipped view |
| Latency: dab to screen within a frame | partial | budgets P13/P16/P33 over (scorecard); big-brush lag #2619, #1994; cursor lag #2566, #1343 |
| No hitch at mouse-up on a long stroke | not measurable | P32 needs a frame harness |
| Live stroke for Brush and Eraser | done | |
| Live stroke for every brush-based retouch tool | missing | TOOL-213-7 |
| No OS arrow cursor over the canvas with a pen | partial | TOOL-213-14 |

## Gestures and cursors

| Behaviour | Status | Evidence |
|---|---|---|
| ⌃⌥-drag (macOS) / Alt+right-drag (Windows) resizes and changes hardness with a readout | done | TOOL-213-10 |
| `[` `]` size, `⇧[` `⇧]` hardness, number keys opacity, ⇧+numbers flow | done | 2026-10-07 |
| ⌥-click samples a colour with painting tools; Eyedropper comparison ring | done | TOOL-213-9 |
| ⇧-click draws a straight line from the last point | done | |
| Hold `~` with the Brush or Pencil to erase with its brush: Clear mode while held (rebindable under Tools › Temporary) | done | `hold_keys.rs`, #3439 |
| Clear painting mode in the Brush and Pencil Mode menus | done | `paint_mode_dropdown` in `panels.rs`, `"mode": "clear"`, #3439 |
| Right-click opens the Brush Preset picker | done | [`context-menu-parity.md`](context-menu-parity.md); XP-Pen right-click not (#1980), macOS issue #2360 |
| Precise / normal / full-size tip cursors, crosshair in the tip | partial | Caps Lock precise cursor not audited; round Pencil cursor requested (#2662, #1057) |
| Brush choice kept per tool (Brush, Eraser, Blur each their own) | partial | #2542 |

## Symmetry

| Mode | Status |
|---|---|
| Vertical, Horizontal, Dual axis, Diagonal | done |
| From a path (Make Symmetry Path) | done |
| Wavy, Circle, Spiral, Parallel Lines, Radial, Mandala | missing (#2659) |

## Brush-based tools

| Tool | Status | Notes |
|---|---|---|
| Brush, Pencil (aliased, Auto Erase), Eraser (Brush/Pencil/Block) | done | Pencil 1 px quality #2139 |
| Mixer Brush | integrated | Brush/Smudge share C3 transport; separate toolbar tool removed. |
| Clone Stamp (opacity, flow, aligned, sample, overlay) | partial | Clone Source W/H link and Alt target cursor (TOOL-213-8) |
| Pattern Stamp (aligned, impressionist) | done | |
| History Brush | done | Art History Brush missing |
| Spot Healing, Healing | partial | grain not matched on flat colour (#2104), "horrible in general" (#1092), stops responding after switching (#1751) |
| Blur, Sharpen, Smudge (premultiplied, Sample All Layers, spacing) | done | BUG-207-2/3, #2171 |
| Dodge, Burn, Sponge | done | |
| Background Eraser, Magic Eraser | done | |
| Color Replacement | done | |
| Quick Selection (brush-driven) | done | |

## Presets

| Behaviour | Status | Evidence |
|---|---|---|
| .abr import (v1/v2/v6) and export, persistent presets, folders | done | `psd/src/abr.rs` |
| Brushes panel: drag a folder into a folder | missing | #2014 |
| Dragged brush preset preview | done | Cached, translucent tip thumbnail plus name in panel list/grid and the preset picker; `ui-egui/src/brushes_tab.rs`, real pointer-drag test in `brushes_tab_tests.rs` (#4225, partial) |
| Tool presets (.tpl) | partial | Tool Presets panel exists; .tpl import not found |
| Import 500 brushes without blocking the UI | not run | P24 |

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-20 | major | C3 settings, independent full dual brushes, colour attributes, pattern sampling, three mixing paths, pigment material and schema-5 presets. |
| 2026-10-19 | minor | Added grain and three wet-edge styles with main/dual controls, sensor resolution, persistent presets and rendering regressions. No brush-feel estimate changed. |
| 2026-10-13 | minor | Stale "missing tool" row corrected against main: Color Replacement (`paint.colorReplacement`) is a live toolbar tool; no brush-feel estimate changed |
| 2026-10-10 | major | First version, from a source audit and the open issues |
| 2026-10-11 | minor | `~` held with the Brush or Pencil erases with that tool's brush, and Clear joins their Mode menus (#3439) |
| 2026-10-12 | minor | Five Brush Settings switch rows toggle without selecting a properties pane; Smoothing has a default-on switch and retains its settings while disabled (#4283). No brush-feel estimate changed. |
| 2026-10-12 | minor | A shaped tip's cursor outline turns and flips with a rotated or flipped view, and follows pixel aspect correction |
| 2026-10-13 | minor | Shape Dynamics and Transfer minimum inputs follow their Control and retain their values while disabled (#3825, partial). No brush-feel estimate changed. |
| 2026-10-13 | minor | Hue Jitter uses the same themed amount slider as other jitter controls (#3825, partial). No brush-feel estimate changed. |
| 2026-10-13 | minor | Dragging a brush preset shows its cached tip thumbnail and name in the Brushes panel and preset picker (#4225, partial). No brush-feel estimate changed. |

### Brush material parameters (2026-10-19)

Grain exposes density, size (1–300), spatial size/density jitter and pen pressure, direction,
tilt or rotation control. Wet Edges exposes Photoshop, Watercolor and Watercolor 2 styles;
strength/minimum, ratio or fixed pixel width/minimum, softness, darkening, saturation,
colour mode, expansion or soft coverage, and single-tip or whole-stroke outlines.
The primary and secondary tabs have independent grain and wet-edge controls. Existing presets without
these parameter blocks retain their original Noise/Wet Edges behaviour. New parameters travel
through the existing brush JSON, native preset store, preset export/import, locks and reset.
