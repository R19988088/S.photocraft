//! C3 schema-5 brush interchange (`preset.json`, optionally in a STORE/DEFLATE ZIP).
//! Parameters use C3's own units; unimplemented effects are reported by full parameter path.
use photocraft_format::zip::ZipReader;
use photocraft_paint::tile::GrayTile;
use photocraft_paint::{BrushPreset, BrushSettings, Dynamic, Pattern, TipShape};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub struct C3Import {
    pub presets: Vec<BrushPreset>,
    pub canvas_presets: Vec<photocraft_paint::canvas_material::CanvasPreset>,
    pub warnings: Vec<String>,
    pub version: u16,
}

const MAX_MANIFEST: usize = 16 << 20;
const MAX_ASSET: usize = 128 << 20;

fn parameters(value: Option<&Value>, prefix: &str, out: &mut BTreeMap<String, Value>) -> Result<(), String> {
    match value {
        None | Some(Value::Null) => (),
        Some(Value::Array(items)) => {
            for item in items {
                let id = item.get("id").and_then(Value::as_str).ok_or("C3 parameter requires an id")?;
                let value = item.get("value").ok_or_else(|| format!("C3 parameter {id} requires a value"))?;
                out.insert(format!("{prefix}{id}"), value.clone());
                if let Some(control) = item.get("control") {
                    out.insert(format!("{prefix}{id}Control"), control.clone());
                }
            }
        }
        Some(Value::Object(items)) => {
            for (id, value) in items {
                out.insert(format!("{prefix}{id}"), value.get("value").unwrap_or(value).clone());
            }
        }
        _ => return Err("C3 parameters must be an array or object".into()),
    }
    Ok(())
}

fn num(v: &Value, path: &str) -> Result<f32, String> {
    let n = v.as_f64().ok_or_else(|| format!("{path}: expected a number"))?;
    if !n.is_finite() || n.abs() > f32::MAX as f64 {
        return Err(format!("{path}: non-finite number"));
    }
    Ok(n as f32)
}
fn flag(v: &Value, path: &str) -> Result<bool, String> {
    v.as_bool().ok_or_else(|| format!("{path}: expected a boolean"))
}
fn dynamic(id: &str, v: &Value, target: &mut Dynamic) -> Result<bool, String> {
    if id.ends_with("Control") {
        target.control = if v.as_str() == Some("initial") {
            photocraft_paint::Control::InitialDirection
        } else if v.as_str() == Some("fade") {
            photocraft_paint::Control::DistanceFade
        } else {
            serde_json::from_value(v.clone()).map_err(|e| format!("{id}: {e}"))?
        };
    } else if id.ends_with("Minimum") || id.rsplit('.').next().is_some_and(|s| s.starts_with("minimum")) {
        target.minimum = num(v, id)? / 100.0;
    } else if id.ends_with("FadeSteps") {
        target.fade_steps = num(v, id)?.clamp(1.0, 10000.0) as u32;
    } else {
        target.jitter = num(v, id)? / 100.0;
    }
    Ok(true)
}

fn attribute(v: &Value, path: &str) -> Result<photocraft_paint::AttributeMode, String> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok().and_then(photocraft_paint::AttributeMode::from_code).ok_or_else(|| format!("{path}: unsupported C3 attribute code {n}"));
    }
    let id = v.as_str().ok_or_else(|| format!("{path}: expected attribute string or mode code"))?;
    let alias = match id {
        "watercolor-burn" => "watercolorBurn",
        "overlay-sp" | "overlaySP" => "overlaySp",
        "lumi-shade" => "lumiShade",
        "dissolve" => "normal",
        "linearDodgeAdd" => "linearDodge",
        _ => id,
    };
    let mut normalized = String::new();
    for (i, part) in alias.split('-').enumerate() {
        let mut chars = part.chars();
        if i > 0
            && let Some(first) = chars.next()
        {
            normalized.extend(first.to_uppercase());
        }
        normalized.extend(chars);
    }
    serde_json::from_value(Value::String(normalized)).map_err(|e| format!("{path}: unsupported C3 attribute {id}: {e}"))
}

fn signed_curve(value: &Value, path: &str) -> Result<photocraft_paint::pigment_material::SignedCurve, String> {
    let points = value
        .get("bezierPoints")
        .or_else(|| value.get("points"))
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{path}: curve requires seven bezierPoints"))?;
    if points.len() != 7 {
        return Err(format!("{path}: curve requires seven points"));
    }
    let mut out =
        photocraft_paint::pigment_material::SignedCurve { enabled: value.get("isEnabled").and_then(Value::as_bool).unwrap_or(true), ..Default::default() };
    for (p, source) in out.points.iter_mut().zip(points) {
        let x = source.get("x").or_else(|| source.get(0)).ok_or_else(|| format!("{path}: point requires x"))?;
        let y = source.get("y").or_else(|| source.get(1)).ok_or_else(|| format!("{path}: point requires y"))?;
        *p = [num(x, path)?, num(y, path)?];
    }
    if !out.is_valid() {
        return Err(format!("{path}: invalid signed pressure curve"));
    }
    Ok(out)
}

fn canonical_parameter(id: &str) -> String {
    if id == "scatter.minimum" {
        return "scatter.minimumScatter".into();
    }
    if id.contains('.') && !id.starts_with("scatter.amount.") {
        return id.into();
    }
    if matches!(
        id,
        "shapeDynamicsEnabled"
            | "colorDynamicsEnabled"
            | "colorModeEnabled"
            | "textureEnabled"
            | "texturePatternStyle"
            | "texturePatternSize"
            | "textureCoordinateMode"
    ) {
        return id.into();
    }
    let direct = match id {
        "brushFlow" => "flow",
        "brushMaskType" => "maskType",
        "brushTipSpacing" => "tip.spacing",
        "brushTipRoundness" => "tip.roundness",
        "brushTipAngle" => "tip.angle",
        "brushMixEnabled" => "mix.enabled",
        "brushMixConcentration" => "mix.concentration",
        "mixAlgorithmMode" => "mix.algorithm",
        "mixOutputStrength" => "mix.outputStrength",
        "mixQuality" => "mix.quality",
        "mixSmooth" => "mix.smooth",
        "mixWater" => "mix.water",
        "blendAttributeStrength" => "brushBlendAttributeStrength",
        "colorAttribute" => "brushColorAttribute",
        "colorAttributeStrength" => "brushColorAttributeStrength",
        "spacingControl" => "scatter.spacingControl",
        "spacingMinimum" => "scatter.spacingMinimum",
        "brushOutputsSmudge" | "outputsSmudge" => "outputsSmudge",
        "brushSmudgeControl" | "smudgeControl" => "smudge.brushControl",
        "smudgeStrengthControl" => "smudge.control",
        "brushSmudgeLength" | "smudgeLength" => "smudge.length",
        "brushSmudgeLengthControl" | "smudgeLengthControl" => "smudge.lengthControl",
        "brushSmudgeLengthPressureInverted" | "smudgeLengthPressureInverted" => "smudge.lengthPressureInverted",
        "pressureControlsSize" => "pressureControlsSize",
        "tipHardnessControl" => "hardnessControl",
        "flipXJitter" => "shapeDynamics.flipXJitter",
        "flipYJitter" => "shapeDynamics.flipYJitter",
        "spacingJitter" => "scatter.spacingJitter",
        "anchorDelay" => "strokeMotion.anchorDelay",
        "anchorSmooth" => "strokeMotion.anchorSmooth",
        "anchorPointX" => "strokeMotion.anchorPointX",
        "anchorPointY" => "strokeMotion.anchorPointY",
        "anchorDelayPressureEnabled" => "strokeMotion.anchorDelayPressureEnabled",
        "strokeSmoothingLevel" => "strokeMotion.strokeSmoothingLevel",
        "brushTiltDelay" | "tiltDelay" => "strokeMotion.brushTiltDelay",
        "strokeGuideMode" => "strokeMotion.strokeGuideMode",
        "inertia" => "shapeDynamics.inertia",
        "inertiaControl" => "shapeDynamics.inertiaControl",
        "inertiaMinDistance" => "shapeDynamics.inertiaMinDistance",
        "shapeScatterRange" => "scatter.scatter",
        "shapeScatterRangeControl" => "scatter.scatterControl",
        "shapeScatterMinimum" => "scatter.minimumScatter",
        "shapeScatterMinimumCount" => "scatter.minimumCount",
        "scatter.minimum" => "scatter.minimumScatter",
        "scatterMinimum" => "scatter.minimumScatter",
        "scatter" => "scatter.scatter",
        "scatterControl" => "scatter.scatterControl",
        "roundnessMinimum" => "shapeDynamics.minimumRoundness",
        "sizeMinimum" => "shapeDynamics.minimumDiameter",
        "colorApplyPerTip" => "colorDynamics.applyPerTip",
        "colorPurity" => "colorDynamics.purity",
        _ => id,
    };
    if direct != id {
        return direct.into();
    }
    for (prefix, out) in
        [("scatter.amount.", "scatter."), ("shapeScatter", "scatter."), ("shape", "shapeDynamics."), ("color", "colorDynamics."), ("texture", "texture.")]
    {
        if let Some(tail) = id.strip_prefix(prefix) {
            if tail.starts_with(char::is_uppercase) {
                let mut chars = tail.chars();
                let first = chars.next().unwrap_or_default().to_ascii_lowercase();
                return format!("{out}{first}{}", chars.as_str());
            }
            if prefix == "scatter.amount." {
                return format!("scatter.{tail}");
            }
        }
    }
    id.into()
}

fn map_brush(p: BTreeMap<String, Value>, label: &str, warnings: &mut Vec<String>) -> Result<BrushSettings, String> {
    // Unlike PhotoCraft's ordinary brush defaults, C3 sparse presets do not enable size pressure.
    let mut b = BrushSettings { size: 120.0, pressure_size: false, ..Default::default() };
    b.texture.c3_math = true;
    b.c3_tip_math = true;
    b.shape_dynamics.roundness.minimum = 0.25;
    b.color_attributes.blend = photocraft_paint::AttributeMode::Normal;
    b.color_attributes.color = photocraft_paint::AttributeMode::Normal;
    b.color_attributes.blend_strength = 1.0;
    b.color_attributes.color_strength = 1.0;
    let mut grain = photocraft_paint::material::GrainSettings { density: 0.0, ..Default::default() };
    let mut wet = photocraft_paint::material::WetEdgeSettings { strength: 0.0, width: 0.5, fixed_width: 8.0, ..Default::default() };
    let mut pigment = photocraft_paint::pigment_material::PigmentSettings {
        visual_thickness: 80.0,
        visual_thickness_minimum: 10.0,
        visual_thickness_control: photocraft_paint::Control::PenPressure,
        highlight_strength: 0.4,
        highlight_brightness: 0.2,
        highlight_ratio: 1.0,
        ..Default::default()
    };
    let mut has_pigment = false;
    let mut has_grain = false;
    let mut has_wet = false;
    let mut normalized = BTreeMap::new();
    for (id, v) in &p {
        let key = canonical_parameter(id);
        if &key != id {
            normalized.entry(key).or_insert_with(|| v.clone());
        }
    }
    for (id, v) in p {
        let key = canonical_parameter(&id);
        if key == id {
            normalized.insert(key, v);
        }
    }
    for (id, v) in normalized {
        let path = format!("{label}.{id}");
        macro_rules! n {
            () => {
                num(&v, &path)?
            };
        }
        macro_rules! q {
            () => {
                n!() / 100.0
            };
        }
        macro_rules! f {
            () => {
                flag(&v, &path)?
            };
        }
        macro_rules! choice {
            ($target:expr) => {
                $target = serde_json::from_value(if v.as_str() == Some("fade") {
                    Value::String("distanceFade".into())
                } else if v.as_str() == Some("initial") {
                    Value::String("initialDirection".into())
                } else {
                    v.clone()
                })
                .map_err(|e| format!("{path}: {e}"))?
            };
        }
        if matches!(
            id.as_str(),
            "sizeJitter"
                | "sizeControl"
                | "sizeMinimum"
                | "angleJitter"
                | "angleControl"
                | "angleMinimum"
                | "roundnessJitter"
                | "roundnessControl"
                | "roundnessMinimum"
        ) {
            b.shape_dynamics.enabled = true;
        }
        let supported = match id.as_str() {
            "size" => {
                b.size = n!().clamp(0.5, 5000.0);
                true
            }
            "opacity" => {
                b.opacity = n!().clamp(0.0, 1.0);
                true
            }
            "flow" => {
                b.flow = n!().clamp(0.0, 1.0);
                true
            }
            "tip.spacing" | "tipSpacing" | "spacing" => {
                b.spacing = q!().max(0.01);
                true
            }
            "tip.hardness" | "tipHardness" | "hardness" => {
                b.hardness = q!().clamp(0.0, 1.0);
                true
            }
            "tip.roundness" | "tipRoundness" | "roundness" => {
                b.roundness = q!().clamp(0.01, 1.0);
                true
            }
            "tip.angle" | "tipAngle" | "angle" => {
                b.angle = n!();
                true
            }
            "tip.shape" | "tipShape" | "brushTipShape" => match v.as_str() {
                Some("round") => {
                    b.triangle = false;
                    true
                }
                Some("triangle") => {
                    b.triangle = true;
                    true
                }
                _ => false,
            },
            "tip.hardnessJitter" | "tip.hardnessControl" | "tip.hardnessMinimum" | "hardnessJitter" | "hardnessControl" | "hardnessMinimum" => {
                dynamic(&id, &v, &mut b.hardness_dynamic)?
            }
            "smudge.pigmentModeEnabled" => {
                b.color_attributes.enabled = f!();
                true
            }
            "smudge.blendMode" => {
                b.color_attributes.blend = attribute(&v, &path)?;
                true
            }
            "wetEdgePressureEnabled" => {
                if f!() {
                    wet.control = photocraft_paint::Control::PenPressure;
                }
                true
            }
            "pressureControlsSize" => {
                b.pressure_size = f!();
                true
            }
            "outputsSmudge" => {
                b.mix_settings.outputs_smudge = f!();
                true
            }
            "pressureControlsOpacity" => {
                b.pressure_opacity = f!();
                true
            }
            "pressureControlsFlow" => {
                b.pressure_flow = f!();
                true
            }
            "shapeDynamicsEnabled" | "shapeDynamics.enabled" => {
                b.shape_dynamics.enabled = f!();
                true
            }
            "sizeJitter" | "sizeControl" | "sizeMinimum" | "shapeDynamics.sizeJitter" | "shapeDynamics.sizeControl" | "shapeDynamics.minimumDiameter" => {
                dynamic(&id, &v, &mut b.shape_dynamics.size)?
            }
            "angleJitter" | "shapeDynamics.angleJitter" => {
                b.shape_dynamics.angle.jitter = (n!() / 360.0).clamp(0.0, 1.0);
                true
            }
            "angleControl" | "angleMinimum" | "shapeDynamics.angleControl" | "shapeDynamics.angleMinimum" => dynamic(&id, &v, &mut b.shape_dynamics.angle)?,
            "roundnessJitter"
            | "roundnessControl"
            | "roundnessMinimum"
            | "shapeDynamics.roundnessJitter"
            | "shapeDynamics.roundnessControl"
            | "shapeDynamics.minimumRoundness" => dynamic(&id, &v, &mut b.shape_dynamics.roundness)?,
            "shapeDynamics.strokeJitter"
            | "shapeDynamics.strokeJitterControl"
            | "shapeDynamics.strokeJitterMinimum"
            | "strokeJitter"
            | "strokeJitterControl"
            | "strokeJitterMinimum" => dynamic(&id, &v, &mut b.shape_dynamics.stroke_jitter)?,
            "shapeDynamics.flipXJitter" => {
                b.shape_dynamics.flip_x_jitter = f!();
                true
            }
            "shapeDynamics.flipYJitter" => {
                b.shape_dynamics.flip_y_jitter = f!();
                true
            }
            "scatter.enabled" => {
                b.scattering.enabled = f!();
                true
            }
            "scatter" | "scatterControl" | "scatterMinimum" | "scatter.amount" | "scatter.scatter" | "scatter.scatterControl" | "scatter.minimumScatter" => {
                b.scattering.enabled = true;
                dynamic(&id, &v, &mut b.scattering.scatter)?
            }
            "scatter.jitter" | "scatter.scatterJitter" | "scatterJitter" => {
                b.scattering.range_jitter = q!();
                true
            }
            "scatter.spacingJitter" | "scatter.spacingControl" | "scatter.spacingMinimum" => dynamic(&id, &v, &mut b.scattering.spacing_jitter)?,
            "scatter.bothAxes" | "bothAxes" => {
                b.scattering.both_axes = f!();
                true
            }
            "scatter.count" | "count" => {
                b.scattering.count = n!().clamp(1.0, 16.0) as u32;
                true
            }
            "countMinimum" | "scatter.minimumCount" => {
                b.scattering.minimum_count = Some(n!().clamp(1.0, 16.0));
                true
            }
            "countJitter" | "countControl" | "scatter.countJitter" | "scatter.countControl" => dynamic(&id, &v, &mut b.scattering.count_jitter)?,
            "colorDynamicsEnabled" | "colorDynamics.enabled" => {
                b.color_dynamics.enabled = f!();
                true
            }
            "colorDynamics.applyPerTip" => {
                b.color_dynamics.per_tip = f!();
                true
            }
            "colorDynamics.foregroundBackgroundJitter" | "colorDynamics.foregroundBackgroundControl" => dynamic(&id, &v, &mut b.color_dynamics.fg_bg)?,
            "strokeMotion.enabled" | "strokeMotionEnabled" => {
                b.motion.enabled = f!();
                true
            }
            "strokeMotion.strokeGuideMode" => {
                choice!(b.motion.guide_mode);
                true
            }
            "strokeMotion.anchorPointX" => {
                b.motion.anchor_point_x = n!();
                true
            }
            "strokeMotion.anchorPointY" => {
                b.motion.anchor_point_y = n!();
                true
            }
            "strokeMotion.anchorDelay" => {
                b.motion.anchor_delay = q!();
                true
            }
            "strokeMotion.anchorSmooth" => {
                b.motion.anchor_smooth = q!();
                true
            }
            "strokeMotion.strokeSmoothingLevel" => {
                b.motion.smoothing = q!();
                true
            }
            "strokeMotion.brushTiltDelay" => {
                b.motion.tilt_delay = q!();
                true
            }
            "strokeMotion.anchorDelayPressureEnabled" => {
                b.motion.anchor_delay_pressure_enabled = f!();
                true
            }
            "shapeDynamics.inertia" => {
                b.inertia.amount = q!();
                b.inertia.enabled = b.inertia.amount > 0.0;
                true
            }
            "shapeDynamics.inertiaControl" => {
                choice!(b.inertia.control);
                true
            }
            "shapeDynamics.inertiaMinDistance" => {
                b.inertia.min_distance = n!();
                true
            }
            "flow.algorithmVersion" | "flowAlgorithmVersion" => {
                b.flow_algorithm_version = n!().clamp(1.0, 2.0) as u8;
                true
            }
            "color.saturationRecordStrength" | "saturationRecordStrength" => {
                b.saturation_record_strength = n!();
                true
            }
            "canvas.presetID" | "canvasPresetID" => {
                if !v.is_null() {
                    b.canvas_material.get_or_insert_with(Default::default).preset_id =
                        Some(v.as_str().ok_or_else(|| format!("{path}: expected canvas preset ID"))?.into());
                }
                true
            }
            "canvas.scale" | "canvasPresetScaleOverride" => {
                b.canvas_material.get_or_insert_with(Default::default).scale = q!();
                true
            }
            "canvas.blendMode" => {
                b.canvas_material.get_or_insert_with(Default::default).blend_mode = attribute(&v, &path)?;
                true
            }
            "canvas.curve" | "canvasPresetCurvePoints" => {
                b.canvas_material.get_or_insert_with(Default::default).set_curve(canvas_curve(&v, &path)?);
                true
            }
            "canvas.opacity" => {
                let m = b.canvas_material.get_or_insert_with(Default::default);
                let factor = q!().clamp(0., 1.);
                for point in &mut m.curve {
                    point[1] *= factor;
                }
                m.curve_lut = photocraft_paint::canvas_material::curve_lut(&m.curve);
                true
            }
            "canvas.enabled" => {
                b.canvas_material.get_or_insert_with(Default::default).enabled = f!();
                true
            }
            "color.saturationPreserveOutput" | "saturationPreserveOutput" => {
                b.saturation_preserve_output = n!();
                true
            }
            "colorDynamics.hueControl" | "colorHueControl" => {
                choice!(b.color_dynamics.hue_control);
                true
            }
            "colorDynamics.minHue" => {
                b.color_dynamics.hue_minimum = q!();
                true
            }
            "colorDynamics.saturationControl" => {
                choice!(b.color_dynamics.saturation_control);
                true
            }
            "colorDynamics.minSaturation" => {
                b.color_dynamics.saturation_minimum = q!();
                true
            }
            "colorDynamics.brightnessControl" => {
                choice!(b.color_dynamics.brightness_control);
                true
            }
            "colorDynamics.minBrightness" => {
                b.color_dynamics.brightness_minimum = q!();
                true
            }
            "colorDynamics.purityControl" => {
                choice!(b.color_dynamics.purity_control);
                true
            }
            "colorDynamics.minPurity" => {
                b.color_dynamics.purity_minimum = q!();
                true
            }
            "colorDynamics.hueJitter" | "colorHueJitter" => {
                b.color_dynamics.hue_jitter = q!();
                true
            }
            "colorDynamics.saturationJitter" => {
                b.color_dynamics.saturation_jitter = q!();
                true
            }
            "colorDynamics.brightnessJitter" => {
                b.color_dynamics.brightness_jitter = q!();
                true
            }
            "colorDynamics.purity" => {
                b.color_dynamics.purity = q!();
                true
            }
            "transfer.enabled" => {
                b.transfer.enabled = f!();
                true
            }
            "opacityJitter" | "opacityControl" | "opacityMinimum" | "transfer.opacityJitter" | "transfer.opacityControl" | "transfer.minimumOpacity" => {
                b.transfer.enabled = true;
                dynamic(&id, &v, &mut b.transfer.opacity)?
            }
            "flowJitter" | "flowMinimum" | "transfer.flowJitter" | "transfer.flowControl" | "transfer.minimumFlow" | "flowControl" => {
                b.transfer.enabled = true;
                dynamic(&id, &v, &mut b.transfer.flow)?
            }
            "texture.coordinateMode" | "textureCoordinateMode" => {
                choice!(b.texture.coordinate_mode);
                true
            }
            "texture.patternStyle" | "texturePatternStyle" => {
                if v.as_str() == Some("custom") {
                    true
                } else if let Pattern::Procedural { style, .. } = &mut b.texture.pattern {
                    choice!(*style);
                    true
                } else {
                    false
                }
            }
            "texture.patternSize" | "texturePatternSize" => {
                if let Pattern::Procedural { size, .. } = &mut b.texture.pattern {
                    *size = n!().clamp(1.0, 4096.0) as u32;
                    true
                } else {
                    false
                }
            }
            "texture.enabled" | "textureEnabled" => {
                b.texture.enabled = f!();
                true
            }
            "texture.scale" | "textureScale" => {
                b.texture.scale = q!().max(0.01);
                true
            }
            "texture.brightness" => {
                b.texture.brightness = n!() / 150.0;
                true
            }
            "texture.contrast" => {
                let x = n!();
                b.texture.contrast = x / if x < 0.0 { 50.0 } else { 100.0 };
                true
            }
            "texture.eachTip" => {
                b.texture.each_tip = f!();
                true
            }
            "texture.invert" => {
                b.texture.invert = f!();
                true
            }
            "texture.mode" => {
                choice!(b.texture.mode);
                true
            }
            "texture.depth" => {
                b.texture.depth = q!();
                true
            }
            "texture.depthJitter" | "texture.depthControl" | "texture.minimumDepth" | "texture.depthFadeSteps" => {
                dynamic(&id, &v, &mut b.texture.depth_jitter)?
            }
            "protectTexture" => {
                b.protect_texture = f!();
                true
            }
            "colorModeEnabled" => {
                b.color_attributes.enabled = f!();
                true
            }
            "brushBlendMode" => {
                b.color_attributes.blend = attribute(&v, &path)?;
                true
            }
            "brushBlendAttributeStrength" => {
                b.color_attributes.blend_strength = n!();
                true
            }
            "brushColorAttribute" => {
                b.color_attributes.color = attribute(&v, &path)?;
                true
            }
            "brushColorAttributeStrength" => {
                b.color_attributes.color_strength = n!();
                true
            }
            "grainDensity" => {
                grain.density = q!();
                has_grain = true;
                true
            }
            "grainSize" => {
                grain.size = n!();
                has_grain = true;
                true
            }
            "grainSizeJitter" => {
                grain.size_jitter = q!();
                has_grain = true;
                true
            }
            "grainDensityJitter" => {
                grain.density_jitter = q!();
                has_grain = true;
                true
            }
            "grainControl" => {
                choice!(grain.control);
                has_grain = true;
                true
            }
            "wetEdgeEnabled" | "wetEdge.enabled" => {
                b.wet_edges = f!();
                true
            }
            "wetEdge" => {
                wet.strength = n!();
                has_wet = true;
                true
            }
            "wetEdgeMinimumStrength" => {
                wet.minimum_strength = n!();
                has_wet = true;
                true
            }
            "wetEdgeControl" => {
                choice!(wet.control);
                has_wet = true;
                true
            }
            "wetEdgeWidth" => {
                wet.width = n!();
                has_wet = true;
                true
            }
            "wetEdgeWidthMinimum" => {
                wet.minimum_width = n!();
                has_wet = true;
                true
            }
            "wetEdgeFixedWidth" => {
                wet.fixed_width = n!();
                has_wet = true;
                true
            }
            "wetEdgeWidthMode" => {
                choice!(wet.width_mode);
                has_wet = true;
                true
            }
            "wetEdgeSoftness" => {
                wet.softness = n!();
                has_wet = true;
                true
            }
            "wetEdgeStyle" => {
                choice!(wet.style);
                has_wet = true;
                true
            }
            "wetEdgeOutlineMode" => {
                choice!(wet.outline);
                has_wet = true;
                true
            }
            "wetEdgeEdgeControl" => {
                choice!(wet.edge_control);
                has_wet = true;
                true
            }
            "wetEdgeColorMode" => {
                choice!(wet.color_mode);
                has_wet = true;
                true
            }
            "wetEdgeBrightnessBoost" => {
                wet.brightness = n!();
                has_wet = true;
                true
            }
            "wetEdgeSaturationBoost" => {
                wet.saturation = n!();
                has_wet = true;
                true
            }
            "pigment.enabled" | "pigmentEnabled" => {
                pigment.enabled = f!();
                has_pigment = true;
                true
            }
            "pigmentThickness" => {
                pigment.thickness = n!();
                has_pigment = true;
                true
            }
            "pigmentThicknessMinimum" => {
                pigment.thickness_minimum = n!();
                has_pigment = true;
                true
            }
            "pigmentThicknessJitter" => {
                pigment.thickness_jitter = n!();
                has_pigment = true;
                true
            }
            "pigmentThicknessControl" => {
                choice!(pigment.thickness_control);
                has_pigment = true;
                true
            }
            "pigmentThicknessCurve" => {
                pigment.thickness_curve = Some(signed_curve(&v, &path)?);
                has_pigment = true;
                true
            }
            "visualThickness" => {
                pigment.visual_thickness = n!();
                has_pigment = true;
                true
            }
            "visualThicknessMinimum" => {
                pigment.visual_thickness_minimum = n!();
                has_pigment = true;
                true
            }
            "visualThicknessJitter" => {
                pigment.visual_thickness_jitter = n!();
                has_pigment = true;
                true
            }
            "visualThicknessControl" => {
                choice!(pigment.visual_thickness_control);
                has_pigment = true;
                true
            }
            "pigment.liquidity" | "pigmentLiquidity" => {
                pigment.liquidity = n!();
                has_pigment = true;
                true
            }
            "pigment.highlightStrength" | "pigmentHighlightStrength" => {
                pigment.highlight_strength = n!();
                has_pigment = true;
                true
            }
            "pigment.highlightBrightness" | "pigmentHighlightBrightness" => {
                pigment.highlight_brightness = n!();
                has_pigment = true;
                true
            }
            "pigment.highlightRatio" | "pigmentHighlightRatio" => {
                pigment.highlight_ratio = n!();
                has_pigment = true;
                true
            }
            "contourStrength" => {
                let value = n!();
                b.contour.get_or_insert_with(Default::default).strength = if value.abs() > 1. { value / 100. } else { value };
                true
            }
            "contourStrengthJitter" => {
                b.contour.get_or_insert_with(Default::default).jitter = n!();
                true
            }
            "contourStrengthMinimum" => {
                b.contour.get_or_insert_with(Default::default).minimum = n!();
                true
            }
            "contourStrengthControl" => {
                choice!(b.contour.get_or_insert_with(Default::default).control);
                true
            }
            "pigmentFluidStrength" => {
                pigment.fluid_strength = n!();
                has_pigment = true;
                true
            }
            "pigmentContourPressureEnabled" => {
                if label.ends_with(".dualBrush") {
                    b.contour.get_or_insert_with(Default::default).pressure_enabled = f!();
                }
                pigment.fluid_pressure_enabled = f!();
                has_pigment = true;
                true
            }
            "canvas.ignoreHighlight" | "ignoreCanvasHighlight" => {
                pigment.ignore_canvas_highlight = f!();
                has_pigment = true;
                true
            }
            "maskType" => {
                pigment.gaussian_mask = v.as_str() == Some("gaussian");
                has_pigment = true;
                true
            }
            "mix.enabled" | "mixEnabled" => {
                b.mix_settings.enabled = f!();
                true
            }
            "mix.algorithm" | "brushMixAlgorithmMode" | "brushMixAlgorithmModeCode" => {
                let id = v.as_str().ok_or_else(|| format!("{path}: expected algorithm"))?;
                b.mix_settings.algorithm = match id {
                    "sssp.v1" | "v1" | "compositor" => photocraft_paint::transport::MixAlgorithm::V1,
                    "sssp.v2" | "v2" | "sai" => photocraft_paint::transport::MixAlgorithm::V2,
                    "sssp.v3" | "v3" | "c2v1" => photocraft_paint::transport::MixAlgorithm::V3,
                    _ => return Err(format!("{path}: unsupported C3 mix algorithm {id}")),
                };
                true
            }
            "smudge" => {
                b.mix_settings.strength = n!() / 10.0;
                true
            }
            "smudgeStrength" => {
                b.mix_settings.strength = n!();
                true
            }
            "smudge.control" | "smudge.brushControl" | "importedSmudgeStrengthControl" | "importedBrushSmudgeControl" => {
                choice!(b.mix_settings.strength_dynamic.control);
                true
            }
            "smudge.length" | "importedSmudgeLength" => {
                b.mix_settings.length = (n!() / 100.).clamp(0., 1.);
                true
            }
            "smudge.lengthControl" | "importedSmudgeLengthControl" => {
                choice!(b.mix_settings.length_dynamic.control);
                true
            }
            "smudge.lengthPressureInverted" | "importedSmudgeLengthPressureInverted" => {
                b.mix_settings.length_pressure_inverted = f!();
                true
            }
            "mix.concentration" | "mixConcentration" => {
                b.mix_settings.concentration = n!();
                true
            }
            "mix.water" | "brushMixWater" => {
                b.mix_settings.water = n!();
                true
            }
            "mix.smooth" | "brushMixSmooth" => {
                b.mix_settings.smooth = n!();
                true
            }
            "mix.quality" | "brushMixQuality" => {
                b.mix_settings.quality = n!();
                true
            }
            "mix.outputStrength" | "brushMixOutputStrength" => {
                b.mix_settings.output_strength = n!();
                true
            }
            "blurStrength" => {
                b.mix_settings.blur_strength = n!();
                true
            }
            "blurControl" => {
                choice!(b.mix_settings.blur_dynamic.control);
                true
            }
            "blurPressureInverted" => {
                b.mix_settings.blur_pressure_inverted = f!();
                true
            }
            "mixPressureCurve" => {
                b.mix_settings.mix_pressure_curve = Some(signed_curve(&v, &path)?);
                true
            }
            "blurPressureCurve" => {
                b.mix_settings.blur_pressure_curve = Some(signed_curve(&v, &path)?);
                true
            }
            "mixingVersion" => true,
            "pigmentMixStrength" => {
                b.pigment_mix_strength = n!();
                true
            }
            // Legacy C2 controls have no consumer in C3's BrushDabExtensionParams / SimpleShaders.
            "wetEdgeWaterControl"
            | "wetEdgeWaterAccumulation"
            | "wetEdgeMoistureControl"
            | "wetEdgeWaterAmount"
            | "wetEdgeWaterMinimum"
            | "wetEdgeWaterJitter"
            | "wetEdgeMoisture"
            | "wetEdgeMinimumMoisture"
            | "watercolorDiffusion"
            | "watercolorDiffusionMinimum"
            | "watercolorDiffusionControl"
            | "watercolorStain"
            | "wetEdgeWaterColorMode"
            | "wetEdgeMoistureColorMode"
            | "strokeMotion.anchorLineStretch" => true,
            _ if id.starts_with("canvas.")
                || id.to_ascii_lowercase().contains("calligraphy")
                || id.to_ascii_lowercase().contains("taper")
                || id.starts_with("strokeMotion.taper") =>
            {
                b.c3_preserved_parameters.insert(id.clone(), v.to_string());
                true
            }
            "brushTipElasticity"
            | "brushTipElasticityControl"
            | "texture.patternID"
            | "brushTipID"
            | "texturePatternID"
            | "tipID"
            | "packageOwnerID"
            | "packagePresetID"
            | "pigmentAppliesToDualBrush"
            | "pigment.appliesToDualBrush"
            | "primaryBrushStrengthPercent"
            | "dualBrush.enabled"
            | "anchorLineStretch"
            | "angleJitterSmoothing"
            | "shapeDynamics.angleJitterSmoothing"
            | "shapeDynamics.tipElasticity"
            | "shapeDynamics.tipElasticityControl"
            | "tipElasticity"
            | "tipElasticityControl"
            | "liquifyHardness"
            | "grainDensityMinimum"
            | "grainSizeMinimum" => {
                b.c3_preserved_parameters.insert(id.clone(), v.to_string());
                true
            }
            _ if id.starts_with("dualBrush") => {
                b.c3_preserved_parameters.insert(id.clone(), v.to_string());
                true
            }
            _ if id.to_ascii_lowercase().contains("buildup") => true,
            _ if id.ends_with("CategoryPresent") => true,
            _ => false,
        };
        if !supported {
            b.c3_preserved_parameters.insert(id.clone(), v.to_string());
            warnings.push(format!("{path}: unsupported C3 parameter = {v}"));
        }
    }
    if has_pigment {
        b.pigment = Some(pigment);
    }
    if has_grain {
        b.noise = grain.density > 0.0;
        b.grain = Some(grain);
    }
    if has_wet {
        b.wet_edge = Some(wet);
    }
    Ok(b)
}

fn flatten(id: &str, value: &Value, out: &mut BTreeMap<String, Value>) {
    if id.ends_with("Curve") || id.ends_with("curve") || id.ends_with("CurvePoints") {
        out.insert(id.into(), value.clone());
        return;
    }
    if let Some(map) = value.as_object() {
        for (key, v) in map {
            flatten(&format!("{id}.{key}"), v, out);
        }
    } else {
        out.insert(id.into(), value.clone());
    }
}

fn canvas_curve(v: &Value, path: &str) -> Result<Vec<[f32; 2]>, String> {
    v.as_array()
        .ok_or_else(|| format!("{path}: expected curve points"))?
        .iter()
        .map(|p| {
            let x = p.get("x").or_else(|| p.get(0)).ok_or_else(|| format!("{path}: missing x"))?;
            let y = p.get("y").or_else(|| p.get(1)).ok_or_else(|| format!("{path}: missing y"))?;
            Ok([num(x, path)?, num(y, path)?])
        })
        .collect()
}
fn bind_canvas(b: &mut BrushSettings, root: &Value, zip: Option<&ZipReader<'_>>, cache: &mut BTreeMap<(String, bool), GrayTile>) -> Result<(), String> {
    let Some(material) = b.canvas_material.as_mut() else { return Ok(()) };
    let Some(id) = material.preset_id.as_deref() else { return Ok(()) };
    if id.is_empty() || id == "blank-canvas" {
        material.enabled = false;
        return Ok(());
    }
    let preset = root
        .get("canvasPresets")
        .and_then(Value::as_array)
        .and_then(|v| v.iter().find(|p| p.get("id").and_then(Value::as_str) == Some(id)))
        .ok_or_else(|| format!("missing C3 canvas preset {id}"))?;
    let preserved = apply_canvas_preset(material, preset, root, zip, cache)?;
    for (k, v) in preserved {
        b.c3_preserved_parameters.insert(format!("canvasPreset.{k}"), v);
    }
    Ok(())
}

fn apply_canvas_preset(
    material: &mut photocraft_paint::canvas_material::CanvasMaterial,
    preset: &Value,
    root: &Value,
    zip: Option<&ZipReader<'_>>,
    cache: &mut BTreeMap<(String, bool), GrayTile>,
) -> Result<BTreeMap<String, String>, String> {
    let texture = preset.get("textureID").or_else(|| preset.get("textureId")).and_then(Value::as_str);
    if let Some(texture) = texture.filter(|id| !id.is_empty()) {
        material.pattern = Pattern::Tile(resource_tile_impl(texture, root, zip, true, cache)?);
    } else {
        material.enabled = false;
    }
    let mut settings = BTreeMap::new();
    parameters(preset.get("parameters"), "", &mut settings)?;
    if let Some(v) = settings.get("canvas.scale") {
        material.scale = num(v, "canvas.scale")? / 100.;
    }
    if let Some(v) = settings.get("canvas.blendMode") {
        material.blend_mode = attribute(v, "canvas.blendMode")?;
    }
    if let Some(v) = settings.get("canvas.curve") {
        material.set_curve(canvas_curve(v, "canvas.curve")?);
    }
    if let Some(v) = settings.get("canvas.opacity") {
        let factor = (num(v, "canvas.opacity")? / 100.).clamp(0., 1.);
        for point in &mut material.curve {
            point[1] *= factor;
        }
        material.curve_lut = photocraft_paint::canvas_material::curve_lut(&material.curve);
    }
    if !material.is_valid() {
        return Err("invalid C3 canvas material".into());
    }
    Ok(settings.into_iter().map(|(k, v)| (k, v.to_string())).collect())
}

fn resource_tile(id: &str, root: &Value, zip: Option<&ZipReader<'_>>, _tip: bool, cache: &mut BTreeMap<(String, bool), GrayTile>) -> Result<GrayTile, String> {
    resource_tile_impl(id, root, zip, false, cache)
}
fn resource_tile_impl(
    id: &str,
    root: &Value,
    zip: Option<&ZipReader<'_>>,
    canvas: bool,
    cache: &mut BTreeMap<(String, bool), GrayTile>,
) -> Result<GrayTile, String> {
    let key = (id.to_string(), canvas);
    if let Some(tile) = cache.get(&key) {
        return Ok(tile.clone());
    }
    let resources = root.get("resources").ok_or_else(|| format!("missing C3 resource {id}"))?;
    let all: Vec<&Value> = match resources {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => o.values().filter_map(Value::as_array).flatten().collect(),
        _ => Vec::new(),
    };
    let resource = if let Some(exact) = all.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(id)) {
        *exact
    } else {
        let prefix = format!("{id}-");
        let mut matches = all.into_iter().filter(|r| r.get("id").and_then(Value::as_str).is_some_and(|v| v.starts_with(&prefix)));
        let found = matches.next().ok_or_else(|| format!("missing C3 resource {id}"))?;
        if matches.next().is_some() {
            return Err(format!("ambiguous C3 resource {id}"));
        }
        found
    };
    let path = resource.get("path").and_then(Value::as_str).ok_or_else(|| format!("resource {id}: missing path"))?;
    let bytes = zip
        .ok_or_else(|| format!("resource {id} ({path}): images require the packaged ZIP"))?
        .read_by_name(path, MAX_ASSET)
        .map_err(|e| format!("resource {id} ({path}): {e}"))?;
    let opts = photocraft_codecs::DecodeOptions {
        limits: photocraft_codecs::Limits { max_width: 16384, max_height: 16384, max_pixels: 16_000_000, max_alloc: 256 << 20 },
        ..Default::default()
    };
    let image = photocraft_codecs::decode_with(&bytes, &opts).map_err(|e| format!("resource {id}: {e}"))?;
    if image.width() == 0 || image.height() == 0 || image.width() > 16384 || image.height() > 16384 {
        return Err(format!("resource {id}: brush image exceeds 16384 px"));
    }
    if !image.layout().is_gray() && !image.layout().is_rgb() {
        return Err(format!("resource {id}: expected grayscale or RGB brush image"));
    }
    let rgba = image.to_rgba_f32();
    // C3 package resources use positive luminance of premultiplied RGB.
    let data: Vec<u16> = rgba
        .chunks_exact(4)
        .map(|c| {
            let l = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2];
            let x = if canvas { 1.0 - c[3] + l * c[3] } else { l * c[3] };
            (x.clamp(0.0, 1.0) * 65535.0).round() as u16
        })
        .collect();
    let tile = GrayTile { width: image.width(), height: image.height(), data: data.into() };
    cache.insert(key, tile.clone());
    Ok(tile)
}

pub fn read_c3(bytes: &[u8], group: &str) -> Result<C3Import, String> {
    let zip = if bytes.starts_with(b"PK\x03\x04") { Some(ZipReader::new(bytes).map_err(|e| e.to_string())?) } else { None };
    let manifest = match &zip {
        Some(z) => z.read_by_name("preset.json", MAX_MANIFEST).map_err(|e| format!("C3 preset.json: {e}"))?,
        None => {
            if bytes.len() > MAX_MANIFEST {
                return Err("C3 manifest too large".into());
            }
            bytes.to_vec()
        }
    };
    let root: Value = serde_json::from_slice(&manifest).map_err(|e| format!("C3 manifest: {e}"))?;
    let version = root.get("schemaVersion").and_then(Value::as_u64).unwrap_or(5);
    if version != 5 {
        return Err(format!("unsupported C3 schema {version}; expected 5"));
    }
    let raw = root.get("presets").and_then(Value::as_array).ok_or("C3 manifest requires a presets array")?;
    if raw.is_empty() || raw.len() > 4096 {
        return Err("C3 package must contain 1–4096 presets".into());
    }
    let groups = root.get("groups").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut warnings = Vec::new();
    let mut cache = BTreeMap::new();
    let mut presets = Vec::new();
    let mut seen = BTreeSet::new();
    for source in raw {
        let id = source.get("id").and_then(Value::as_str).ok_or("C3 preset requires id")?;
        if !seen.insert(id) {
            return Err(format!("duplicate C3 preset id {id}"));
        }
        let name = source.get("name").and_then(Value::as_str).ok_or("C3 preset requires name")?;
        let tool = source.get("tool").and_then(Value::as_str).unwrap_or("brush");
        let mut params = BTreeMap::new();
        parameters(source.get("parameters"), "", &mut params)?;
        let mut flat = BTreeMap::new();
        for (k, v) in params {
            flatten(&k, &v, &mut flat);
        }
        let mut dual = None;
        if let Some(sections) = source.get("sections").and_then(Value::as_object) {
            for (section, value) in sections {
                if section == "dualBrush" {
                    dual = Some(value);
                    continue;
                }
                if value.get("enabled").and_then(Value::as_bool) != Some(true) {
                    continue;
                }
                flat.insert(format!("{section}.enabled"), Value::Bool(true));
                let mut section_params = BTreeMap::new();
                parameters(value.get("parameters"), "", &mut section_params)?;
                for (k, v) in section_params {
                    let key = if k.contains('.') || matches!(section.as_str(), "basic" | "wetEdge" | "grain" | "pigment" | "smudge" | "mix") {
                        k
                    } else {
                        format!("{section}.{k}")
                    };
                    flatten(&key, &v, &mut flat);
                }
            }
        }
        let mut raw_dual = BTreeMap::new();
        for (key, value) in &flat {
            if let Some(tail) = key.strip_prefix("dualBrush").filter(|s| s.starts_with(char::is_uppercase)) {
                let mut c = tail.chars();
                raw_dual.insert(format!("{}{}", c.next().unwrap_or_default().to_ascii_lowercase(), c.as_str()), value.clone());
            }
        }
        let dual_enabled = flat.get("dualBrush.enabled").or_else(|| flat.get("dualBrushEnabled")).and_then(Value::as_bool).unwrap_or(false);
        let pigment_applies_to_dual = flat
            .remove("pigment.appliesToDualBrush")
            .or_else(|| flat.remove("pigmentAppliesToDualBrush"))
            .map(|v| flag(&v, "pigment.appliesToDualBrush"))
            .transpose()?
            .unwrap_or(false);
        let mut b = map_brush(flat, name, &mut warnings)?;
        bind_canvas(&mut b, &root, zip.as_ref(), &mut cache)?;
        b.erase = tool == "eraser";
        b.mix_settings.outputs_smudge |= tool == "smudge";
        if tool == "smudge" {
            b.mix_settings.enabled = true;
        }
        if !matches!(tool, "brush" | "eraser" | "smudge") {
            warnings.push(format!("{name}: C3 tool {tool} imported as brush; tool behavior unsupported"));
        }
        let refs = source.get("resources").cloned().unwrap_or(Value::Object(Map::new()));
        if let Some(id) = refs.get("tip").and_then(Value::as_str)
            && id != "round"
            && id != "builtin-round"
            && !id.is_empty()
        {
            b.tip = TipShape::Sampled(resource_tile(id, &root, zip.as_ref(), true, &mut cache)?);
        }
        if let Some(id) = refs.get("texture").and_then(Value::as_str) {
            if id == "paper" {
                if b.texture.enabled {
                    warnings.push(format!("{name}.texture: C3 built-in paper texture approximated by PhotoCraft paper"));
                }
            } else if !id.is_empty() {
                b.texture.pattern = Pattern::Tile(resource_tile(id, &root, zip.as_ref(), false, &mut cache)?);
            }
        }
        if let Some(dual) = dual {
            let mut p = raw_dual;
            parameters(dual.get("parameters"), "", &mut p)?;
            p.remove("enabled");
            let pattern_tip = p
                .remove("tipSource")
                .map(|v| match v.as_str() {
                    Some("pattern") => Ok(true),
                    Some("tip") => Ok(false),
                    _ => Err(format!("{name}.dualBrush.tipSource: expected tip or pattern")),
                })
                .transpose()?
                .unwrap_or(false);
            let strength_version = p.remove("strengthMappingVersion").and_then(|v| v.as_u64()).unwrap_or(2);
            let raw_strength = p
                .remove("strengthRatioPercent")
                .or_else(|| p.remove("secondaryStrengthPercent"))
                .map(|v| num(&v, "strengthRatioPercent"))
                .transpose()?
                .unwrap_or(50.0)
                .clamp(0.0, 100.0);
            let secondary = if strength_version >= 2 { raw_strength } else { 100.0 - raw_strength } / 100.0;
            let primary = p
                .remove("primaryStrengthPercent")
                .map(|v| num(&v, "primaryStrengthPercent"))
                .transpose()?
                .map(|v| v.clamp(0.0, 100.0) / 100.0)
                .unwrap_or(1.0 - secondary);
            let mode = p.remove("mode").map(serde_json::from_value).transpose().map_err(|e| format!("{name}.dualBrush.mode: {e}"))?.unwrap_or_default();
            let flip = p.remove("flip").map(|v| flag(&v, "flip")).transpose()?.unwrap_or(false);
            let mut flat = BTreeMap::new();
            for (k, v) in p {
                if k == "fullParameters" {
                    if let Some(map) = v.as_object() {
                        for (k, v) in map {
                            flatten(k, v, &mut flat);
                        }
                    }
                } else {
                    flatten(&k, &v, &mut flat);
                }
            }
            let mut second = map_brush(flat, &format!("{name}.dualBrush"), &mut warnings)?;
            bind_canvas(&mut second, &root, zip.as_ref(), &mut cache)?;
            if pigment_applies_to_dual && second.pigment.as_ref().is_none_or(|p| !p.enabled) {
                second.pigment = b.pigment.clone();
            }
            if let Some(id) = refs.get("dualTip").and_then(Value::as_str).filter(|_| !pattern_tip)
                && id != "round"
                && id != "builtin-round"
                && !id.is_empty()
            {
                second.tip = TipShape::Sampled(resource_tile(id, &root, zip.as_ref(), true, &mut cache)?);
            }
            if let Some(id) = refs.get("dualTexture").and_then(Value::as_str) {
                if id == "paper" {
                    if second.texture.enabled {
                        warnings.push(format!("{name}.dualBrush.texture: C3 built-in paper texture approximated by PhotoCraft paper"));
                    }
                } else if !id.is_empty() {
                    second.texture.pattern = Pattern::Tile(resource_tile(id, &root, zip.as_ref(), false, &mut cache)?);
                }
            }
            if pattern_tip {
                if let Some(tile) = second.texture.pattern.bitmap() {
                    second.tip = TipShape::Sampled(tile.clone());
                } else {
                    warnings.push(format!("{name}.dualBrush.tipSource: built-in pattern tip has no exact image resource"));
                }
            }
            b.dual_brush = photocraft_paint::DualBrush {
                enabled: dual.get("enabled").and_then(Value::as_bool).unwrap_or(dual_enabled),
                mode,
                flip,
                primary_strength: primary,
                secondary_strength: secondary,
                size: second.size,
                opacity: second.opacity,
                flow: second.flow,
                tip: second.tip.clone(),
                hardness: second.hardness,
                hardness_dynamic: second.hardness_dynamic,
                roundness: second.roundness,
                angle: second.angle,
                spacing: second.spacing,
                scatter: second.scattering.scatter.jitter,
                count: second.scattering.count,
                both_axes: second.scattering.both_axes,
                grain: second.grain.clone(),
                wet_edges: second.wet_edges,
                wet_edge: second.wet_edge.clone(),
                full_parameters: Some(Box::new(second)),
            };
        }
        let mut folder = Vec::new();
        for g in &groups {
            let matches = g.get("id") == source.get("groupID")
                || g.get("presetIDs").and_then(Value::as_array).is_some_and(|ids| ids.iter().any(|i| i.as_str() == Some(id)));
            let sub = g.get("subgroups").and_then(Value::as_array).and_then(|subs| {
                subs.iter().find(|sg| {
                    sg.get("id") == source.get("subgroupID")
                        || sg.get("presetIDs").and_then(Value::as_array).is_some_and(|ids| ids.iter().any(|i| i.as_str() == Some(id)))
                })
            });
            if matches || sub.is_some() {
                if let Some(n) = g.get("name").and_then(Value::as_str) {
                    folder.push(n.into());
                }
                if let Some(n) = sub.and_then(|s| s.get("name")).and_then(Value::as_str) {
                    folder.push(n.into());
                }
                break;
            }
        }
        if source.get("enabled").and_then(Value::as_bool) == Some(false) {
            warnings.push(format!("{name}: disabled C3 preset imported into library"));
        }
        presets.push(BrushPreset { name: name.into(), brush: b, builtin: false, group: group.into(), folder, captures: Default::default() });
    }
    let mut canvas_presets = Vec::new();
    if let Some(items) = root.get("canvasPresets").and_then(Value::as_array) {
        for preset in items {
            let id = preset.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).ok_or("C3 canvas preset requires an id")?;
            let name = preset.get("name").and_then(Value::as_str).unwrap_or(id);
            let mut material = photocraft_paint::canvas_material::CanvasMaterial { preset_id: Some(id.into()), ..Default::default() };
            let preserved_parameters = apply_canvas_preset(&mut material, preset, &root, zip.as_ref(), &mut cache)?;
            canvas_presets.push(photocraft_paint::canvas_material::CanvasPreset { id: id.into(), name: name.into(), material, preserved_parameters });
        }
    }
    Ok(C3Import { presets, canvas_presets, warnings, version: 5 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn package(preset: Value) -> Value {
        json!({"schemaVersion":5,"packageID":"test","name":"Test","groups":[{"id":"g","name":"Ink","subgroups":[{"id":"s","name":"Fine","presetIDs":["p"]}]}],"presets":[preset]})
    }
    #[test]
    #[ignore = "requires an external C3 reference package"]
    fn c3_external_reference_package() {
        let path = std::env::var("C3_REFERENCE_PACKAGE").unwrap();
        let out = read_c3(&std::fs::read(path).unwrap(), "C3 Reference").unwrap();
        println!("presets: {}", out.presets.len());
        for warning in &out.warnings {
            println!("{warning}");
        }
        assert_eq!(out.presets.len(), 88);
        assert_eq!(out.canvas_presets.len(), 7);
        for canvas in &out.canvas_presets {
            assert!(canvas.material.is_valid());
            assert!(canvas.material.pattern.bitmap().is_some_and(|tile| tile.is_valid()), "{}", canvas.name);
        }
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        let b = &out.presets.iter().find(|p| p.name == "水边 p2").unwrap().brush;
        assert!(b.dual_brush.enabled);
        assert!((b.dual_brush.secondary_strength - 0.08).abs() < 1e-6);
        let secondary = b.dual_brush.full_parameters.as_ref().unwrap();
        assert!(secondary.shape_dynamics.enabled);
        assert_eq!(secondary.grain.as_ref().unwrap().size, 100.);
        assert_eq!(secondary.mix_settings.algorithm, photocraft_paint::transport::MixAlgorithm::V3);
        for p in &out.presets {
            for lane in std::iter::once(&p.brush).chain(p.brush.dual_brush.full_parameters.as_deref()) {
                assert!(lane.pigment.as_ref().is_none_or(|p| p.is_valid()), "invalid pigment in {}", p.name);
                assert!(lane.canvas_material.as_ref().is_none_or(|m| m.is_valid()), "invalid canvas in {}", p.name);
            }
        }
    }
    #[test]
    fn c3_sparse_defaults_and_parameter_units() {
        let doc = package(
            json!({"id":"p","name":"Fine ink","tool":"brush","parameters":{"opacity":0.4,"tip.spacing":25,"shapeDynamics":{"sizeControl":"penPressure","minimumDiameter":30,"angleJitter":72}},"sections":{"shapeDynamics":{"enabled":true}}}),
        );
        let out = read_c3(&serde_json::to_vec(&doc).unwrap(), "Imported").unwrap();
        let b = &out.presets[0].brush;
        assert_eq!((b.size, b.opacity, b.flow, b.spacing), (120.0, 0.4, 1.0, 0.25));
        assert!(!b.pressure_size);
        assert!(b.shape_dynamics.enabled);
        assert_eq!(b.shape_dynamics.size.control, photocraft_paint::Control::PenPressure);
        assert_eq!(b.shape_dynamics.size.minimum, 0.3);
        assert_eq!(b.shape_dynamics.angle.jitter, 0.2);
        assert_eq!(out.presets[0].folder, ["Ink", "Fine"]);
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    }
    #[test]
    fn c3_archive_resources_dual_parameters_and_warnings() {
        let mut doc = package(
            json!({"id":"p","name":"Dual","tool":"brush","parameters":[{"id":"unmappedEffect","value":0.7}],"resources":{"tip":"tip","dualTip":"tip","dualTexture":"texture"},"sections":{"dualBrush":{"enabled":true,"parameters":{"size":42,"opacity":0.3,"flow":0.6,"primaryStrengthPercent":70,"secondaryStrengthPercent":80,"fullParameters":{"textureEnabled":true,"grainDensity":25}}}}}),
        );
        doc["resources"] =
            json!({"tips":[{"id":"tip","name":"tip","path":"tips/t.png"}],"textures":[{"id":"texture","name":"texture","path":"textures/t.png"}]});
        let image = photocraft_codecs::Image::from_u8(2, 1, photocraft_codecs::ChannelLayout::Rgba, vec![255, 255, 255, 255, 0, 0, 0, 255]).unwrap();
        let png = photocraft_codecs::encode(&image, photocraft_codecs::Format::Png, &Default::default()).unwrap();
        let mut zip = photocraft_format::zip::ZipWriter::new();
        zip.add("preset.json", &serde_json::to_vec(&doc).unwrap()).unwrap();
        zip.add("tips/t.png", &png).unwrap();
        zip.add("textures/t.png", &png).unwrap();
        let out = read_c3(&zip.finish().unwrap(), "C3").unwrap();
        let b = &out.presets[0].brush;
        assert_eq!(b.tip.bitmap().unwrap().data.as_ref(), &[65535, 0]);
        let d = &b.dual_brush;
        assert!(d.enabled);
        assert_eq!((d.size, d.opacity, d.flow, d.primary_strength, d.secondary_strength), (42.0, 0.3, 0.6, 0.7, 0.8));
        let second = d.full_parameters.as_ref().unwrap();
        assert!(second.texture.enabled);
        assert!(second.texture.pattern.bitmap().is_some());
        assert_eq!(second.grain.as_ref().unwrap().density, 0.25);
        assert_eq!(out.warnings.len(), 1);
        assert!(out.warnings[0].contains("Dual.unmappedEffect"));
    }
    #[test]
    fn c3_canvas_binding_uses_curve_pattern_and_white_alpha() {
        let mut doc = package(json!({"id":"p","name":"Canvas ink","parameters":{"canvas.presetID":"paper","canvas.heightStrength":73}}));
        doc["canvasPresets"] = json!([{"id":"paper","textureID":"canvas","parameters":[{"id":"canvas.scale","value":200},{"id":"canvas.blendMode","value":"multiply"},{"id":"canvas.curve","value":[{"x":0,"y":0},{"x":0.33,"y":0.33},{"x":0.66,"y":0.66},{"x":1,"y":1}]}]}]);
        doc["resources"] = json!({"canvasTextures":[{"id":"canvas","path":"canvas.png"}]});
        let image = photocraft_codecs::Image::from_u8(2, 1, photocraft_codecs::ChannelLayout::Rgba, vec![0, 0, 0, 0, 0, 0, 0, 255]).unwrap();
        let png = photocraft_codecs::encode(&image, photocraft_codecs::Format::Png, &Default::default()).unwrap();
        let mut zip = photocraft_format::zip::ZipWriter::new();
        zip.add("preset.json", &serde_json::to_vec(&doc).unwrap()).unwrap();
        zip.add("canvas.png", &png).unwrap();
        let out = read_c3(&zip.finish().unwrap(), "C3").unwrap();
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        let b = &out.presets[0].brush;
        let m = b.canvas_material.as_ref().unwrap();
        assert_eq!(m.pattern.bitmap().unwrap().data.as_ref(), &[65535, 0]);
        assert_eq!(m.scale, 2.);
        assert_eq!(m.blend_mode, photocraft_paint::AttributeMode::Multiply);
        assert_eq!(m.pigment_response([0.8; 3], [3., 1.], 1., 1.), [0.8; 3]);
        let dark = m.pigment_response([0.8; 3], [3., 1.], 0.5, 1.);
        assert!(dark[0] < 0.41 && dark[0] > 0.39);
        assert_eq!(b.c3_preserved_parameters.get("canvas.heightStrength").map(String::as_str), Some("73"));
    }
    #[test]
    fn c3_material_and_transport_use_source_units_and_signed_bezier() {
        let curve = json!({"isEnabled":true,"isSigned":true,"bezierPoints":[{"x":0.0,"y":0.0},{"x":0.15,"y":-0.2},{"x":0.3,"y":0.4},{"x":0.5,"y":0.6},{"x":0.7,"y":0.4},{"x":0.85,"y":0.2},{"x":1.0,"y":0.0}]});
        let doc = package(
            json!({"id":"p","name":"Pigment","tool":"smudge","parameters":{"smudge":5,"mix.algorithm":"sssp.v3","mix.water":0.2,"mix.smooth":0.3,"mix.quality":6,"mix.outputStrength":0.8,"pigmentThickness":62,"pigmentThicknessMinimum":8,"pigmentThicknessCurve":curve,"visualThickness":77,"pigment.liquidity":0.25,"pigmentFluidStrength":0.32,"texture.coordinateMode":"tipLocal","texture.patternStyle":"canvas","tip.shape":"triangle","flow.algorithmVersion":2,"strokeMotion":{"anchorDelay":35,"strokeGuideMode":"anchor"},"colorDynamics":{"hueJitter":50,"hueControl":"penPressureInverse","minHue":20}},"sections":{"strokeMotion":{"enabled":true},"colorDynamics":{"enabled":true},"mix":{"enabled":true},"pigment":{"enabled":true},"wetEdge":{"enabled":true}}}),
        );
        let out = read_c3(&serde_json::to_vec(&doc).unwrap(), "C3").unwrap();
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        let b = &out.presets[0].brush;
        let p = b.pigment.as_ref().unwrap();
        assert!(p.enabled);
        assert!(b.wet_edges);
        assert!(b.triangle);
        assert_eq!(b.flow_algorithm_version, 2);
        assert!(b.motion.enabled);
        assert_eq!(b.motion.anchor_delay, 0.35);
        assert_eq!(b.color_dynamics.hue_control, photocraft_paint::Control::PenPressureInverse);
        assert_eq!(b.color_dynamics.hue_minimum, 0.2);
        assert_eq!((p.thickness, p.thickness_minimum, p.visual_thickness, p.liquidity, p.fluid_strength), (62.0, 8.0, 77.0, 0.25, 0.32));
        assert_eq!(p.thickness_curve.as_ref().unwrap().points[1], [0.15, -0.2]);
        assert!(b.mix_settings.enabled && b.mix_settings.outputs_smudge);
        assert_eq!((b.mix_settings.strength, b.mix_settings.water, b.mix_settings.quality), (0.5, 0.2, 6.0));
        assert_eq!(b.mix_settings.algorithm, photocraft_paint::transport::MixAlgorithm::V3);
    }

    #[test]
    fn c3_rejects_invalid_schema_missing_asset_and_bad_numbers() {
        assert!(read_c3(br#"{"schemaVersion":6,"presets":[]}"#, "C3").is_err());
        let mut doc = package(json!({"id":"p","name":"Missing","resources":{"tip":"missing"}}));
        assert!(read_c3(&serde_json::to_vec(&doc).unwrap(), "C3").err().unwrap().contains("missing"));
        doc["presets"][0] = json!({"id":"p","name":"Bad","parameters":{"size":"wide"}});
        assert!(read_c3(&serde_json::to_vec(&doc).unwrap(), "C3").is_err());
    }
}
