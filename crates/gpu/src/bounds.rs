//! Exact bounds for content-dependent effect math, and conservative bounds used only to cull
//! composite passes. Planning a GPU-backed raster tile must not materialize its pixel bytes.

use photocraft_doc::{Layer, LayerContent};
use photocraft_geom::Rect;
use photocraft_raster::Surface;

pub use photocraft_compose::bounds::content_bounds;

/// An upper bound on changed pixels for scheduling only. An allocated GPU tile may include
/// transparent margins; processing those margins is harmless, while scanning them reads back
/// the stroke. CPU-only surfaces retain the existing exact cached scan.
pub fn planning_content_bounds(surface: &Surface) -> Rect {
    if surface.tiles().any(|(_, tile)| tile.backend().is_some()) { surface.tile_bounds() } else { content_bounds(surface) }
}

fn has_backend(layer: &Layer) -> bool {
    match &layer.content {
        LayerContent::Group(group) => group.children.iter().filter(|child| child.visible).any(has_backend),
        _ => layer.surface().is_some_and(|surface| surface.tiles().any(|(_, tile)| tile.backend().is_some())),
    }
}

/// Exact bounds: effects use these to anchor linked patterns and gradients. Substituting a
/// tile hull here moves those effects as a stroke crosses a tile edge.
pub fn layer_bounds(layer: &Layer, canvas: Rect) -> Rect {
    photocraft_compose::layer_bounds(layer, canvas)
}

fn planning_layer_bounds(layer: &Layer, canvas: Rect) -> Rect {
    if photocraft_compose::advanced::shapeless(layer) {
        return canvas;
    }
    match &layer.content {
        LayerContent::Group(group) if group.artboard.is_some() => group.artboard.as_ref().map_or(Rect::EMPTY, |a| a.rect),
        LayerContent::Group(group) => {
            group.children.iter().filter(|child| child.visible).fold(Rect::EMPTY, |r, child| r.union(&planning_layer_bounds(child, canvas)))
        }
        LayerContent::Fill(_) => canvas,
        _ => {
            let mut own = layer.surface().map_or(Rect::EMPTY, planning_content_bounds);
            if let LayerContent::Shape(shape) = &layer.content {
                own = own.union(&photocraft_compose::shape_bounds(shape).map_or(Rect::EMPTY, |r| r.intersect(&canvas)));
            }
            if photocraft_compose::effect_outline(layer).is_some()
                && photocraft_compose::effects::has_effects(layer)
                && let Some(paint) = photocraft_compose::paint_bounds(layer)
            {
                own = own.union(&paint);
            }
            own
        }
    }
}

/// Exact effect-map region, including children with their effects. Its frame also participates
/// in the effect math, so the CPU reference remains authoritative here.
pub fn effect_region(layer: &Layer, canvas: Rect) -> Rect {
    let margin = photocraft_compose::effects::margin(layer);
    let (whole, window) = (photocraft_compose::effect_source_bounds(layer, canvas).inflate(margin), canvas.inflate(margin));
    let area = |r: Rect| u64::from(r.width()) * u64::from(r.height());
    if area(whole) <= area(window).saturating_add(area(window) / 4) { whole } else { whole.intersect(&window) }
}

pub fn transparent_outside(layer: &Layer) -> bool {
    photocraft_compose::transparent_outside(layer)
}

/// Conservative scheduling bounds. These only limit where a pass runs; they never become an
/// effect's gradient frame, pattern anchor, distance-field source, or shape bounds.
pub fn composite_bounds(layer: &Layer, canvas: Rect) -> Option<Rect> {
    if !has_backend(layer) {
        return photocraft_compose::composite_bounds(layer, canvas);
    }
    if !transparent_outside(layer) {
        return None;
    }
    let own = match &layer.content {
        LayerContent::Group(group) if group.artboard.is_none() => {
            let mut bounds = Rect::EMPTY;
            for child in group.children.iter().filter(|child| child.visible) {
                bounds = bounds.union(&composite_bounds(child, canvas)?);
            }
            bounds
        }
        LayerContent::Adjustment(_) => Rect::EMPTY,
        _ => planning_layer_bounds(layer, canvas),
    };
    if own.is_empty() {
        return Some(Rect::EMPTY);
    }
    let margin = if photocraft_compose::effects::has_effects(layer) { photocraft_compose::effects::margin(layer) } else { 0 };
    Some(own.inflate(margin).intersect(&canvas))
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_color::PixelFormat;

    #[test]
    fn matches_surface_content_bounds() {
        let mut s = Surface::new(PixelFormat::RGBA8);
        assert_eq!(content_bounds(&s), s.content_bounds());
        s.fill_rect(Rect::new(10, 300, 40, 700), &[1.0, 0.0, 0.0, 1.0]);
        s.fill_rect(Rect::new(-50, 5, -3, 9), &[0.0, 0.0, 1.0, 0.5]);
        assert_eq!(content_bounds(&s), s.content_bounds());
        // Cached a second time, and after a change.
        assert_eq!(content_bounds(&s), s.content_bounds());
        s.fill_rect(Rect::new(600, 600, 601, 601), &[0.0, 1.0, 0.0, 1.0]);
        assert_eq!(content_bounds(&s), s.content_bounds());
        let mut g = Surface::with_default(PixelFormat::GRAY8, &[1.0]);
        g.fill_rect(Rect::new(3, 3, 9, 9), &[0.5]);
        assert_eq!(content_bounds(&g), g.content_bounds());
    }
}
