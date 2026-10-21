//! Device-owned stroke rendering. Input dynamics stay in Rust; pixel state stays on the device.
use std::any::Any;
use std::fmt::Debug;
use std::sync::{Arc, OnceLock, RwLock};

use photocraft_geom::{Rect, TileCoord};
use photocraft_raster::Surface;

use crate::{BrushContext, Dab};

pub trait StrokeBackend: Debug + Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn error(&self) -> Option<String>;
    fn set_error(&mut self, error: String);
    /// A tail preview must be able to branch without mutating its parent stroke.
    fn fork(&self) -> Box<dyn StrokeBackend>;
    fn rasterize(&mut self, ctx: &BrushContext, primary: &[Dab], secondary: &[Dab]);
    fn bounds(&self) -> Rect;
    fn mark_dirty(&mut self, rect: Rect);
    fn take_dirty_rect(&mut self) -> Rect;
    /// Drain sparse dirty pages without joining distant symmetry passes into one rectangle.
    fn take_dirty_tiles(&mut self) -> Vec<TileCoord>;
    /// Limit a temporary union branch to the pages needed for this redraw and its halo.
    fn retain_tiles(&mut self, tiles: &[TileCoord]);
    fn composite(&mut self, target: &mut Surface, selection: Option<&Surface>, lock: bool, all: bool) -> Rect;
    fn union_max(&mut self, other: &dyn StrokeBackend);
    /// Explicit mask consumers may request a readback; painting never calls this method.
    fn coverage_in(&self, rect: Rect) -> Result<Vec<f32>, String>;
}

impl Clone for Box<dyn StrokeBackend> {
    fn clone(&self) -> Self {
        self.fork()
    }
}

pub trait StrokeBackendFactory: Send + Sync {
    /// None denotes a native channel format this device renderer does not support.
    fn begin(&self, ctx: &BrushContext, pre: &Surface) -> Option<Box<dyn StrokeBackend>>;
    /// Explicit one-pixel input decision, such as Pencil Auto Erase before dab generation.
    fn sample_rgba(&self, surface: &Surface, x: i32, y: i32) -> Result<[f32; 4], String> {
        sample_cpu_rgba(surface, x, y)
    }
}

static FACTORY: OnceLock<RwLock<Option<Arc<dyn StrokeBackendFactory>>>> = OnceLock::new();

pub fn install(factory: Arc<dyn StrokeBackendFactory>) {
    let slot = FACTORY.get_or_init(|| RwLock::new(None));
    *slot.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(factory);
}

pub(crate) fn begin(ctx: &BrushContext, pre: &Surface) -> Option<Box<dyn StrokeBackend>> {
    let factory = FACTORY.get()?.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()?;
    factory.begin(ctx, pre)
}

fn sample_cpu_rgba(surface: &Surface, x: i32, y: i32) -> Result<[f32; 4], String> {
    if surface.tile(TileCoord::containing(x, y)).is_some_and(|tile| tile.backend().is_some()) {
        return Err("The installed brush backend cannot sample this GPU tile".into());
    }
    Ok(surface.rgba(x, y))
}

/// Sample one input pixel explicitly, without silently materializing external tile storage.
pub fn sample_rgba(surface: &Surface, x: i32, y: i32) -> Result<[f32; 4], String> {
    let factory = FACTORY.get().and_then(|slot| slot.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone());
    match factory {
        Some(factory) => factory.sample_rgba(surface, x, y),
        None => sample_cpu_rgba(surface, x, y),
    }
}
