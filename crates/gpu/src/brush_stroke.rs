//! Sparse, immutable GPU stroke pages. Only a dab's neighbourhood is gathered into mutable
//! scratch; publishing a page or forking a tail never materializes CPU pixels.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use photocraft_color::{ColorMode, PixelFormat};
use photocraft_geom::{Rect, TILE_SIZE, TileCoord};
use photocraft_paint::gpu_backend::{StrokeBackend, StrokeBackendFactory};
use photocraft_paint::material::{WetEdgeSettings, WetOutline};
use photocraft_paint::transport::MixAlgorithm;
use photocraft_paint::{BrushContext, BrushSettings, Dab};
use photocraft_raster::{Surface, Tile};
use wgpu::util::DeviceExt;

use crate::brush::{Stage, TransportParams, TransportPipelines};
use crate::brush_composite::{CompositeParams, CompositePipelines, CompositeStage, UnionPipelines};
use crate::brush_format::{NativeFormat, NativeFormats};
use crate::brush_pigment::{PigmentParams, PigmentPipelines, PigmentStage, PigmentState};
use crate::brush_raster::{RasterBrush, RasterOutput, RasterPipelines};
use crate::brush_wet::{WetParams, WetPipelines, WetStage};
#[path = "brush_transport_native.rs"]
mod native_transport;
use crate::tile::{GpuTileSnapshot, TileSnapshots};
use native_transport::NativeTransportPipelines;

const SIDE: u32 = TILE_SIZE as u32;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba32Float;

struct Kit {
    device: wgpu::Device,
    queue: wgpu::Queue,
    health: crate::DeviceHealth,
    raster: RasterPipelines,
    composite: CompositePipelines,
    pigment: PigmentPipelines,
    wet: WetPipelines,
    transport: TransportPipelines,
    native_transport: NativeTransportPipelines,
    native_formats: NativeFormats,
    snapshots: TileSnapshots,
    union: UnionPipelines,
    glue_layout: wgpu::BindGroupLayout,
    glue: wgpu::ComputePipeline,
    regions: wgpu::ComputePipeline,
    frames: Mutex<Vec<Frame>>,
    zero: wgpu::Texture,
    sink: wgpu::Texture,
    sink_alpha: wgpu::Texture,
}

/// Register once using photocraft_paint::gpu_backend::install.
pub struct GpuStrokeFactory {
    kit: Arc<Kit>,
}
impl GpuStrokeFactory {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        Self::with_health(device, queue, crate::DeviceHealth::new())
    }
    /// Share the device owner's existing watch; never replace its callbacks.
    pub fn with_health(device: &wgpu::Device, queue: &wgpu::Queue, health: crate::DeviceHealth) -> Self {
        let mut entries: Vec<_> = (0..4)
            .map(|binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            })
            .collect();
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: 4,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format: FORMAT,
                view_dimension: wgpu::TextureViewDimension::D2,
            },
            count: None,
        });
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: 5,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        });
        let glue_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pc_stroke_glue"), entries: &entries });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pc_stroke_glue"),
            source: wgpu::ShaderSource::Wgsl(include_str!("brush_stroke.wgsl").into()),
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pc_stroke_glue"),
            bind_group_layouts: &[Some(&glue_layout)],
            immediate_size: 0,
        });
        let glue = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pc_stroke_glue"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("glue"),
            compilation_options: Default::default(),
            cache: None,
        });
        let regions = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pc_stroke_coverage_regions"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("coverage_regions"),
            compilation_options: Default::default(),
            cache: None,
        });
        Self {
            kit: Arc::new(Kit {
                device: device.clone(),
                queue: queue.clone(),
                health,
                raster: RasterPipelines::new(device),
                composite: CompositePipelines::new(device),
                pigment: PigmentPipelines::new(device),
                wet: WetPipelines::new(device),
                transport: TransportPipelines::new(device),
                native_transport: NativeTransportPipelines::new(device),
                native_formats: NativeFormats::new(device),
                snapshots: TileSnapshots::new(device),
                union: UnionPipelines::new(device),
                glue_layout,
                glue,
                regions,
                frames: Mutex::new(Vec::new()),
                zero: texture(device, [1, 1]),
                sink: texture(device, [1, 1]),
                sink_alpha: texture(device, [1, 1]),
            }),
        }
    }
}
impl StrokeBackendFactory for GpuStrokeFactory {
    fn sample_rgba(&self, surface: &Surface, x: i32, y: i32) -> Result<[f32; 4], String> {
        let kit = &self.kit;
        if let Some(fault) = kit.health.fault() {
            return Err(fault.to_string());
        }
        let coord = TileCoord::containing(x, y);
        let Some(backend) = surface.tile(coord).and_then(|tile| tile.backend()) else {
            return Ok(surface.rgba(x, y));
        };
        let snapshot = backend.as_any().downcast_ref::<GpuTileSnapshot>().ok_or("The brush backend cannot sample this external tile")?;
        let sources: Vec<_> = match (snapshot.native_color(), snapshot.native_alpha()) {
            (Some(color), Some(alpha)) => vec![color, alpha],
            (None, None) => vec![snapshot.pixels()],
            _ => return Err("GPU sample is missing a native channel texture".into()),
        };
        // Auto Erase chooses its brush colour before Rust generates the dabs. Read only that
        // input pixel; retain the immutable tile entirely on the GPU.
        let sample_stride = u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = kit.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pc_brush_input_sample"),
            size: sources.len() as u64 * sample_stride,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = kit.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pc_brush_input_sample") });
        for (index, source) in sources.iter().enumerate() {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: source,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: x.rem_euclid(TILE_SIZE) as u32, y: y.rem_euclid(TILE_SIZE) as u32, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: index as u64 * sample_stride,
                        bytes_per_row: Some(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT),
                        rows_per_image: Some(1),
                    },
                },
                wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            );
        }
        if let Some(fault) = kit.health.fault() {
            return Err(fault.to_string());
        }
        let submission = kit.queue.submit([encoder.finish()]);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        if !kit.health.wait(&kit.device, Some(submission)) {
            return Err(kit.health.fault().map_or_else(|| "GPU input sample failed".into(), |fault| fault.to_string()));
        }
        receiver.recv().map_err(|error| format!("GPU input sample failed: {error}"))?.map_err(|error| format!("GPU input sample failed: {error}"))?;
        let mapped = readback.slice(..).get_mapped_range().map_err(|error| format!("GPU input sample failed: {error}"))?;
        let values: Vec<f32> = mapped.chunks_exact(4).map(|bytes| f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])).collect();
        let format = surface.format();
        let mut native = values.get(..format.mode.color_channels()).ok_or("GPU input sample has incomplete native channels")?.to_vec();
        if format.alpha {
            native.push(*values.get(if sources.len() == 2 { sample_stride as usize / 4 } else { 3 }).ok_or("GPU input sample has no alpha value")?);
        }
        let rgba = photocraft_raster::to_rgba(&format, &native);
        drop(mapped);
        readback.unmap();
        if let Some(fault) = kit.health.fault() {
            return Err(fault.to_string());
        }
        Ok(rgba)
    }
    fn begin(&self, ctx: &BrushContext, pre: &Surface) -> Option<Box<dyn StrokeBackend>> {
        let mut error = self.kit.health.fault().map(|fault| fault.to_string());
        let native_format = if error.is_none() && (pre.format().mode != ColorMode::Rgb || !pre.format().alpha) {
            match self.kit.native_formats.get(&self.kit.device, pre.format()) {
                Ok(format) => Some(format),
                Err(message) => {
                    error = Some(message);
                    None
                }
            }
        } else {
            None
        };
        let primary = if error.is_none() && self.kit.health.is_ok() {
            match RasterBrush::new(&self.kit.device, &self.kit.queue, ctx, false) {
                Ok(raster) => Some(Arc::new(raster)),
                Err(message) => {
                    error = Some(message);
                    None
                }
            }
        } else {
            None
        };
        let secondary = if error.is_none() && self.kit.health.is_ok() && ctx.brush.dual_brush.enabled {
            match RasterBrush::new(&self.kit.device, &self.kit.queue, ctx, true) {
                Ok(raster) => Some(Arc::new(raster)),
                Err(message) => {
                    error = Some(message);
                    None
                }
            }
        } else {
            None
        };
        Some(Box::new(GpuStroke {
            kit: self.kit.clone(),
            ctx: ctx.clone(),
            raster: [primary, secondary],
            pre: pre.clone(),
            pages: BTreeMap::new(),
            pre_cache: Arc::new(Mutex::new(BTreeMap::new())),
            native_pre_cache: Arc::new(Mutex::new(BTreeMap::new())),
            native_format,
            selection_cache: Arc::new(Mutex::new(BTreeMap::new())),
            metadata: Default::default(),
            bounds: Rect::EMPTY,
            dirty: BTreeSet::new(),
            error,
        }))
    }
}

#[derive(Clone)]
struct Lane {
    cov: wgpu::Texture,
    color: wgpu::Texture,
    wet: wgpu::Texture,
    material: wgpu::Texture,
    work: wgpu::Texture,
    work_alpha: Option<wgpu::Texture>,
    states: wgpu::Buffer,
}
impl Lane {
    fn new(device: &wgpu::Device, size: [u32; 2], stateful: bool, native: bool) -> Self {
        Self {
            cov: texture(device, size),
            color: texture(device, size),
            wet: texture(device, size),
            material: texture(device, size),
            work: texture(device, size),
            work_alpha: native.then(|| texture(device, size)),
            states: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pc_stroke_material_states"),
                size: if stateful { u64::from(size[0]) * u64::from(size[1]) * 48 } else { 48 },
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
        }
    }
    fn textures(&self) -> [&wgpu::Texture; 5] {
        [&self.cov, &self.color, &self.wet, &self.material, &self.work]
    }
}
struct Page {
    lanes: Vec<Lane>,
}
struct Frame {
    region: Rect,
    pre: wgpu::Texture,
    native_pre: Option<[wgpu::Texture; 2]>,
    selection: wgpu::Texture,
    lanes: Vec<Lane>,
    scratch: [wgpu::Texture; 5],
    raster: RasterOutput,
    extras: Vec<wgpu::Texture>,
}
impl Frame {
    fn new(kit: &Kit, region: Rect, stateful: &[bool], native: bool) -> Self {
        let size = [region.width(), region.height()];
        Self {
            region,
            pre: texture(&kit.device, size),
            native_pre: native.then(|| std::array::from_fn(|_| texture(&kit.device, size))),
            selection: texture(&kit.device, size),
            lanes: stateful.iter().map(|&enabled| Lane::new(&kit.device, size, enabled, native)).collect(),
            scratch: std::array::from_fn(|_| texture(&kit.device, size)),
            raster: RasterOutput::new(&kit.device, size),
            extras: Vec::new(),
        }
    }
    fn rect(&self) -> [u32; 4] {
        [0, 0, self.region.width(), self.region.height()]
    }
    fn same_shape(&self, other: &Self) -> bool {
        self.region.width() == other.region.width()
            && self.region.height() == other.region.height()
            && self.native_pre.is_some() == other.native_pre.is_some()
            && self.lanes.len() == other.lanes.len()
            && self.lanes.iter().zip(&other.lanes).all(|(a, b)| a.states.size() == b.states.size())
    }
    fn byte_len(&self) -> u64 {
        let texture_count = 2
            + self.native_pre.as_ref().map_or(0, |_| 2)
            + self.lanes.iter().map(|l| 5 + usize::from(l.work_alpha.is_some())).sum::<usize>()
            + 5
            + 3
            + self.extras.len();
        u64::from(self.region.width()) * u64::from(self.region.height()) * 16 * texture_count as u64 + self.lanes.iter().map(|l| l.states.size()).sum::<u64>()
    }
}
#[derive(Clone, Default)]
struct Metadata {
    origin: Option<[f32; 2]>,
    previous: Option<[f32; 2]>,
    last: Option<Dab>,
    carrier: Option<Arc<wgpu::Texture>>,
    carrier_alpha: Option<Arc<wgpu::Texture>>,
    carrier_radius: f32,
}
#[derive(Clone)]
struct GpuStroke {
    kit: Arc<Kit>,
    ctx: BrushContext,
    raster: [Option<Arc<RasterBrush>>; 2],
    pre: Surface,
    pages: BTreeMap<TileCoord, Arc<Page>>,
    pre_cache: Arc<Mutex<BTreeMap<TileCoord, Arc<wgpu::Texture>>>>,
    native_pre_cache: Arc<Mutex<BTreeMap<TileCoord, Arc<[wgpu::Texture; 2]>>>>,
    native_format: Option<Arc<NativeFormat>>,
    selection_cache: Arc<Mutex<BTreeMap<TileCoord, SelectionUpload>>>,
    metadata: [Metadata; 2],
    bounds: Rect,
    dirty: BTreeSet<TileCoord>,
    error: Option<String>,
}
impl fmt::Debug for GpuStroke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuStroke").field("bounds", &self.bounds).field("pages", &self.pages.len()).field("error", &self.error).finish()
    }
}
fn texture(device: &wgpu::Device, size: [u32; 2]) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pc_stroke_page"),
        size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}
fn view(t: &wgpu::Texture) -> wgpu::TextureView {
    t.create_view(&Default::default())
}
fn aligned(rect: Rect) -> Rect {
    rect.tiles().fold(Rect::EMPTY, |r, c| r.union(&c.rect()))
}
fn expanded(r: Rect, n: i32) -> Rect {
    Rect::new(r.x0.saturating_sub(n), r.y0.saturating_sub(n), r.x1.saturating_add(n), r.y1.saturating_add(n))
}
fn copy(encoder: &mut wgpu::CommandEncoder, source: &wgpu::Texture, target: &wgpu::Texture, src: [u32; 2], dst: [u32; 2], size: [u32; 2]) {
    if size.contains(&0) {
        return;
    }
    encoder.copy_texture_to_texture(
        wgpu::TexelCopyTextureInfo { texture: source, mip_level: 0, origin: wgpu::Origin3d { x: src[0], y: src[1], z: 0 }, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyTextureInfo { texture: target, mip_level: 0, origin: wgpu::Origin3d { x: dst[0], y: dst[1], z: 0 }, aspect: wgpu::TextureAspect::All },
        wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
    );
}
fn copy_rect(encoder: &mut wgpu::CommandEncoder, source: &wgpu::Texture, target: &wgpu::Texture, r: [u32; 4]) {
    copy(encoder, source, target, [r[0], r[1]], [r[0], r[1]], [r[2], r[3]]);
}

impl Kit {
    fn glue(&self, encoder: &mut wgpu::CommandEncoder, inputs: [&wgpu::Texture; 4], output: &wgpu::Texture, rect: [u32; 4], mode: u32, value: [f32; 4]) {
        #[repr(C)]
        #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
        struct Params {
            rect: [u32; 4],
            value: [f32; 4],
            mode: [u32; 4],
        }
        let params = Params { rect, value, mode: [mode, 0, 0, 0] };
        let uniform = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pc_stroke_glue"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let views = inputs.map(view);
        let out = view(output);
        let mut entries: Vec<_> = views
            .iter()
            .enumerate()
            .map(|(binding, v)| wgpu::BindGroupEntry { binding: binding as u32, resource: wgpu::BindingResource::TextureView(v) })
            .collect();
        entries.extend([
            wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&out) },
            wgpu::BindGroupEntry { binding: 5, resource: uniform.as_entire_binding() },
        ]);
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("pc_stroke_glue"), layout: &self.glue_layout, entries: &entries });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pc_stroke_glue"), timestamp_writes: None });
        pass.set_pipeline(if mode == 9 { &self.regions } else { &self.glue });
        pass.set_bind_group(0, &group, &[]);
        let side = if mode == 9 { 64 } else { 8 };
        pass.dispatch_workgroups(rect[2].div_ceil(side), rect[3].div_ceil(side), 1);
    }
    fn composite(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        inputs: [&wgpu::Texture; 8],
        outputs: [&wgpu::Texture; 3],
        params: CompositeParams,
        stage: CompositeStage,
    ) {
        let iv = inputs.map(view);
        let ov = outputs.map(view);
        self.composite.encode(&self.device, encoder, std::array::from_fn(|i| &iv[i]), std::array::from_fn(|i| &ov[i]), params, stage);
    }
    fn pigment(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        inputs: [&wgpu::Texture; 6],
        output: &wgpu::Texture,
        states: &wgpu::Buffer,
        params: PigmentParams,
        stage: PigmentStage,
    ) {
        let iv = inputs.map(view);
        self.pigment.encode(&self.device, encoder, std::array::from_fn(|i| &iv[i]), &view(output), states, params, stage);
    }
}

impl GpuStroke {
    fn per_dab_color(&self, brush: &BrushSettings, index: usize) -> bool {
        if index == 1 && self.ctx.secondary_context().is_some() {
            return true;
        }
        let single = brush.wet_edges
            && brush.wet_edge.as_ref().is_some_and(|w| w.outline == WetOutline::Single && w.color_mode != photocraft_paint::material::WetColorMode::Normal);
        !brush.erase
            && (brush.saturation_record_strength > 0.0
                || brush.saturation_preserve_output > 0.0
                || brush.color_dynamics.enabled
                || brush.pattern_enabled
                || brush.canvas_material.is_some()
                || brush.pigment.as_ref().is_some_and(|p| p.enabled)
                || single)
    }
    fn native_wet_color(&self) -> bool {
        let b = &self.ctx.brush;
        b.wet_edges && b.wet_edge.as_ref().is_some_and(|w| w.outline == WetOutline::Stroke && w.color_mode != photocraft_paint::material::WetColorMode::Normal)
            || b.dual_brush.enabled
                && b.dual_brush.wet_edges
                && b.dual_brush.wet_edge.as_ref().is_some_and(|w| w.color_mode != photocraft_paint::material::WetColorMode::Normal)
    }
    fn native_params(&self, mut params: CompositeParams, brush: &BrushSettings, index: usize) -> CompositeParams {
        if let Some(format) = &self.native_format {
            params = params.with_native(format, brush, false);
            params.primary.flags[3] = u32::from(self.per_dab_color(brush, index));
        }
        params
    }
    fn encode_composite(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        inputs: [&wgpu::Texture; 8],
        outputs: [&wgpu::Texture; 3],
        params: CompositeParams,
        stage: CompositeStage,
    ) {
        if let Some(format) = &self.native_format {
            let params = if params.native[2] == 0 { self.native_params(params, &self.ctx.brush, 0) } else { params };
            let iv = inputs.map(view);
            let ov = outputs.map(view);
            self.kit.composite.encode_native(
                &self.kit.device,
                encoder,
                std::array::from_fn(|i| &iv[i]),
                std::array::from_fn(|i| &ov[i]),
                params,
                stage,
                format,
            );
        } else {
            self.kit.composite(encoder, inputs, outputs, params, stage);
        }
    }
    fn lane_count(&self) -> usize {
        if self.raster[1].is_some() { 2 } else { 1 }
    }
    fn pigment_lane(&self, i: usize) -> bool {
        !self.ctx.brush.erase && (i == 0 || self.ctx.secondary_context().is_some()) && self.brush(i).pigment.as_ref().is_some_and(|p| p.enabled)
    }
    fn brush(&self, i: usize) -> &BrushSettings {
        if i == 1 { self.ctx.secondary_context().map_or(&self.ctx.brush, |c| &c.brush) } else { &self.ctx.brush }
    }
    fn upload_pre(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        coord: TileCoord,
        target: &wgpu::Texture,
        native_target: Option<&[wgpu::Texture; 2]>,
        converted_alpha: &wgpu::Texture,
        offset: [u32; 2],
    ) {
        if let (Some(format), Some(targets)) = (&self.native_format, native_target) {
            if let Some(snapshot) = self.pre.tile(coord).and_then(|t| t.backend()).and_then(|b| b.as_any().downcast_ref::<GpuTileSnapshot>())
                && let (Some(color), Some(alpha)) = (snapshot.native_color(), snapshot.native_alpha())
            {
                copy(encoder, color, &targets[0], [0, 0], offset, [SIDE, SIDE]);
                copy(encoder, alpha, &targets[1], [0, 0], offset, [SIDE, SIDE]);
                format.encode(&self.kit.device, encoder, true, &targets[0], &targets[1], target, converted_alpha, [offset[0], offset[1], SIDE, SIDE]);
                return;
            }
            let mut cache = self.native_pre_cache.lock().unwrap_or_else(|e| e.into_inner());
            let image = cache.entry(coord).or_insert_with(|| {
                let images = Arc::new(std::array::from_fn(|_| texture(&self.kit.device, [SIDE, SIDE])));
                let pixels = self.pre.read_region(coord.rect());
                let nc = self.pre.format().mode.color_channels();
                let mut color = Vec::<[f32; 4]>::with_capacity((SIDE * SIDE) as usize);
                let mut alpha = Vec::<[f32; 4]>::with_capacity((SIDE * SIDE) as usize);
                for px in pixels.chunks_exact(self.pre.channels()) {
                    let mut c = [0.0; 4];
                    c[..nc].copy_from_slice(&px[..nc]);
                    color.push(c);
                    alpha.push([if self.pre.format().alpha { px[nc] } else { 1.0 }, 0.0, 0.0, 0.0]);
                }
                for (image, bytes) in images.iter().zip([bytemuck::cast_slice(&color), bytemuck::cast_slice(&alpha)]) {
                    self.kit.queue.write_texture(
                        image.as_image_copy(),
                        bytes,
                        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIDE * 16), rows_per_image: Some(SIDE) },
                        wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 },
                    );
                }
                images
            });
            for (image, target) in image.iter().zip(targets) {
                copy(encoder, image, target, [0, 0], offset, [SIDE, SIDE]);
            }
            format.encode(&self.kit.device, encoder, true, &targets[0], &targets[1], target, converted_alpha, [offset[0], offset[1], SIDE, SIDE]);
            return;
        }
        if self.pre.tile(coord).is_none() && self.pre.default_pixel().iter().all(|v| *v == 0.0) {
            return;
        }
        if let Some(snapshot) = self.pre.tile(coord).and_then(|t| t.backend()).and_then(|b| b.as_any().downcast_ref::<GpuTileSnapshot>()) {
            copy(encoder, snapshot.pixels(), target, [0, 0], offset, [SIDE, SIDE]);
            return;
        }
        let mut cache = self.pre_cache.lock().unwrap_or_else(|e| e.into_inner());
        let image = cache.entry(coord).or_insert_with(|| {
            let image = Arc::new(texture(&self.kit.device, [SIDE, SIDE]));
            let pixels = self.pre.read_region(coord.rect());
            self.kit.queue.write_texture(
                image.as_image_copy(),
                bytemuck::cast_slice(&pixels),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIDE * 16), rows_per_image: Some(SIDE) },
                wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 },
            );
            image
        });
        copy(encoder, image, target, [0, 0], offset, [SIDE, SIDE]);
    }
    fn gather(&self, encoder: &mut wgpu::CommandEncoder, region: Rect) -> Frame {
        let stateful: Vec<_> = (0..self.lane_count()).map(|i| self.pigment_lane(i)).collect();
        let mut frames = self.kit.frames.lock().unwrap_or_else(|e| e.into_inner());
        let cached = frames.iter().position(|frame| {
            frame.region.width() == region.width()
                && frame.region.height() == region.height()
                && frame.lanes.len() == stateful.len()
                && frame.native_pre.is_some() == self.native_format.is_some()
                && frame.lanes.iter().zip(&stateful).all(|(lane, &enabled)| (lane.states.size() > 48) == enabled)
        });
        let mut frame = cached.map_or_else(|| Frame::new(&self.kit, region, &stateful, self.native_format.is_some()), |i| frames.swap_remove(i));
        drop(frames);
        frame.region = region;
        let zero = &self.kit.zero;
        for image in std::iter::once(&frame.pre).chain(frame.lanes.iter().flat_map(|lane| lane.textures())).chain([
            &frame.raster.coverage,
            &frame.raster.depth,
            &frame.raster.source,
        ]) {
            self.kit.glue(encoder, [zero, zero, zero, zero], image, frame.rect(), 4, [0.0; 4]);
        }
        for lane in &frame.lanes {
            encoder.clear_buffer(&lane.states, 0, None);
            if let Some(alpha) = &lane.work_alpha {
                self.kit.glue(encoder, [zero, zero, zero, zero], alpha, frame.rect(), 4, [0.0; 4]);
            }
        }
        self.kit.glue(encoder, [zero, zero, zero, zero], &frame.selection, frame.rect(), 4, [1.0; 4]);
        let size = [region.width(), region.height()];
        for coord in region.tiles() {
            let offset = [(coord.rect().x0 - region.x0) as u32, (coord.rect().y0 - region.y0) as u32];
            self.upload_pre(encoder, coord, &frame.pre, frame.native_pre.as_ref(), &frame.scratch[4], offset);
            for i in 0..self.lane_count() {
                let target = &frame.lanes[i];
                if let Some(source) = self.pages.get(&coord).and_then(|p| p.lanes.get(i)) {
                    for (s, t) in source.textures().into_iter().zip(target.textures()) {
                        copy(encoder, s, t, [0, 0], offset, [SIDE, SIDE]);
                    }
                    if let (Some(s), Some(t)) = (&source.work_alpha, &target.work_alpha) {
                        copy(encoder, s, t, [0, 0], offset, [SIDE, SIDE]);
                    }
                    if self.pigment_lane(i) {
                        self.kit.pigment.encode_state_region_copy(
                            &self.kit.device,
                            encoder,
                            &source.states,
                            &target.states,
                            [SIDE, SIDE],
                            size,
                            [0, 0],
                            offset,
                            [SIDE, SIDE],
                        );
                    }
                } else {
                    if let (Some(pre), Some(alpha)) = (&frame.native_pre, &target.work_alpha) {
                        copy(encoder, &pre[0], &target.work, offset, offset, [SIDE, SIDE]);
                        copy(encoder, &pre[1], alpha, offset, offset, [SIDE, SIDE]);
                    } else {
                        copy(encoder, &frame.pre, &target.work, offset, offset, [SIDE, SIDE]);
                    }
                }
            }
        }
        frame
    }
    fn recycle(&self, frame: Frame) {
        const CACHE_BYTES: u64 = 768 * 1024 * 1024;
        let mut frames = self.kit.frames.lock().unwrap_or_else(|e| e.into_inner());
        if frame.byte_len() > CACHE_BYTES {
            return;
        }
        if frames.iter().filter(|f| f.same_shape(&frame)).count() >= 2
            && let Some(i) = frames.iter().position(|f| f.same_shape(&frame))
        {
            frames.remove(i);
        }
        while frames.iter().map(Frame::byte_len).sum::<u64>() + frame.byte_len() > CACHE_BYTES {
            let i = frames.iter().position(|f| !f.same_shape(&frame)).unwrap_or(0);
            frames.remove(i);
        }
        frames.push(frame);
    }
    fn scatter(&mut self, encoder: &mut wgpu::CommandEncoder, frame: &Frame, rect: Rect) {
        let size = [frame.region.width(), frame.region.height()];
        for coord in rect.tiles() {
            let page = Page {
                lanes: (0..self.lane_count()).map(|i| Lane::new(&self.kit.device, [SIDE, SIDE], self.pigment_lane(i), self.native_format.is_some())).collect(),
            };
            let origin = [(coord.rect().x0 - frame.region.x0) as u32, (coord.rect().y0 - frame.region.y0) as u32];
            for (i, (source, target)) in frame.lanes.iter().zip(&page.lanes).enumerate() {
                for (s, t) in source.textures().into_iter().zip(target.textures()) {
                    copy(encoder, s, t, origin, [0, 0], [SIDE, SIDE]);
                }
                if let (Some(s), Some(t)) = (&source.work_alpha, &target.work_alpha) {
                    copy(encoder, s, t, origin, [0, 0], [SIDE, SIDE]);
                }
                if self.pigment_lane(i) {
                    self.kit.pigment.encode_state_region_copy(
                        &self.kit.device,
                        encoder,
                        &source.states,
                        &target.states,
                        size,
                        [SIDE, SIDE],
                        origin,
                        [0, 0],
                        [SIDE, SIDE],
                    );
                }
            }
            self.pages.insert(coord, Arc::new(page));
            self.dirty.insert(coord);
        }
    }
    fn dab(&mut self, encoder: &mut wgpu::CommandEncoder, lane_index: usize, d: &Dab) {
        let bounds = self.ctx.dab_rect(d, lane_index == 1);
        if bounds.is_empty() {
            return;
        }
        let brush = self.brush(lane_index);
        let halo = if brush.mix_settings.enabled {
            ((d.radius * (0.18 + d.blur_strength * 0.82)).clamp(1.5, 127.0) + d.radius * 2.0 * brush.spacing).ceil() as i32
        } else {
            2
        };
        let region = aligned(expanded(bounds, halo));
        let limits = self.kit.device.limits();
        let stateful = (0..self.lane_count()).any(|i| self.pigment_lane(i));
        let oversized = region.width() > limits.max_texture_dimension_2d
            || region.height() > limits.max_texture_dimension_2d
            || u64::from(region.width()) * u64::from(region.height()) > 1024 * 1024
            || (stateful && u64::from(region.width()) * u64::from(region.height()) * 48 > limits.max_storage_buffer_binding_size);
        if oversized {
            self.paged_dab(encoder, lane_index, d);
        } else {
            self.dab_part(encoder, lane_index, d, None);
        }
    }
    fn paged_dab(&mut self, encoder: &mut wgpu::CommandEncoder, lane_index: usize, d: &Dab) {
        let previous = self.metadata[lane_index].previous;
        for coord in self.ctx.dab_rect(d, lane_index == 1).tiles() {
            self.metadata[lane_index].previous = previous;
            self.dab_part(encoder, lane_index, d, Some(coord.rect()));
            if self.error.is_some() {
                return;
            }
        }
        if self.brush(lane_index).mix_settings.enabled && !self.brush(lane_index).erase {
            self.metadata[lane_index].previous = previous;
            self.paged_transport(encoder, lane_index, d);
            self.metadata[lane_index].previous = Some([d.center.x as f32, d.center.y as f32]);
        }
    }

    fn dab_part(&mut self, encoder: &mut wgpu::CommandEncoder, lane_index: usize, d: &Dab, core: Option<Rect>) {
        let dab_rect = self.ctx.dab_rect(d, lane_index == 1);
        if dab_rect.is_empty() {
            return;
        }
        let brush = self.brush(lane_index).clone();
        let halo = if brush.mix_settings.enabled {
            ((d.radius * (0.18 + d.blur_strength * 0.82)).clamp(1.5, 127.0) + d.radius * 2.0 * brush.spacing).ceil() as i32
        } else {
            2
        };
        let region = core.unwrap_or_else(|| aligned(expanded(dab_rect, halo)));
        let limits = self.kit.device.limits();
        if region.width() > limits.max_texture_dimension_2d
            || region.height() > limits.max_texture_dimension_2d
            || ((0..self.lane_count()).any(|i| self.pigment_lane(i))
                && u64::from(region.width()) * u64::from(region.height()) * 48 > limits.max_storage_buffer_binding_size)
        {
            self.error = Some("The brush footprint exceeds this GPU's working-region limit".into());
            return;
        }
        let kit = self.kit.clone();
        let mut frame = self.gather(encoder, region);
        let origin = *self.metadata[lane_index].origin.get_or_insert([d.center.x as f32, d.center.y as f32]);
        let Some(raster) = &self.raster[lane_index] else { return };
        raster.encode_into(
            &kit.raster,
            &kit.device,
            encoder,
            d,
            [region.x0, region.y0],
            origin,
            &view(&frame.pre),
            &mut frame.raster,
            self.native_format.as_deref(),
        );
        let r = frame.raster.rect;
        let lane = &frame.lanes[lane_index];
        kit.glue(encoder, [&frame.raster.coverage, &frame.raster.depth, &frame.pre, &frame.pre], &frame.scratch[3], r, 0, [d.alpha, 0.0, 0.0, 0.0]);
        let mut params = self.native_params(CompositeParams::from_brush(&brush, None, false, true), &brush, lane_index);
        params.rect = r;
        let (wet_enabled, wet_settings) = if lane_index == 1 && self.ctx.secondary_context().is_none() {
            (self.ctx.brush.dual_brush.wet_edges, self.ctx.brush.dual_brush.wet_edge.as_ref())
        } else {
            (brush.wet_edges, brush.wet_edge.as_ref())
        };
        params.dab = [
            d.opacity * if lane_index == 1 && self.ctx.secondary_context().is_some() { brush.opacity } else { 1.0 },
            wet_settings.filter(|w| w.outline == WetOutline::Single).map_or(0.0, |w| w.color_strength(d.wet_edge.strength)),
            0.0,
            0.0,
        ];
        params.primary.color = d.color;
        params.wet = [d.wet_edge.strength, d.wet_edge.width, d.wet_edge.stamp_width, 0.0];
        params.controls =
            [1, 1, u32::from(wet_enabled && !brush.erase), u32::from(brush.saturation_record_strength > 0.0 || brush.saturation_preserve_output > 0.0)];
        self.encode_composite(
            encoder,
            [&lane.cov, &lane.color, &lane.wet, &frame.scratch[3], &frame.raster.source, &frame.pre, &frame.pre, &frame.pre],
            [&frame.scratch[0], &frame.scratch[1], &frame.scratch[2]],
            params,
            CompositeStage::Accumulate,
        );
        for (source, target) in [&frame.scratch[0], &frame.scratch[1], &frame.scratch[2]].into_iter().zip([&lane.cov, &lane.color, &lane.wet]) {
            copy_rect(encoder, source, target, r);
        }
        let previous = self.metadata[lane_index].previous;
        let center = [d.center.x as f32, d.center.y as f32];
        let motion = previous.map_or([0.0; 2], |p| {
            let dx = center[0] - p[0];
            let dy = center[1] - p[1];
            let length = dx.hypot(dy);
            let cap = (d.radius * 0.16).clamp(2.0, 8.0);
            let scale = if length > cap { cap / length } else { 1.0 };
            [dx * scale, dy * scale]
        });
        if !brush.erase && d.pigment.enabled {
            kit.glue(encoder, [&frame.raster.coverage, &frame.raster.depth, &frame.pre, &frame.pre], &frame.scratch[3], r, 1, [d.alpha, 0.0, 0.0, 0.0]);
            kit.glue(encoder, [&frame.raster.coverage, &frame.raster.depth, &frame.pre, &frame.pre], &frame.scratch[4], r, 2, [0.0; 4]);
            let pigment = d.pigment;
            let params = PigmentParams {
                rect: r,
                blend: [0.0, 0.0, d.alpha, d.opacity * brush.opacity],
                state: PigmentState {
                    material: [1.0, pigment.pressure, pigment.thickness, pigment.visual_thickness],
                    highlight: [pigment.liquidity, pigment.highlight_strength, pigment.highlight_brightness, pigment.highlight_ratio],
                    motion: [motion[0], motion[1], 0.0, 0.0],
                },
            };
            kit.pigment(
                encoder,
                [&lane.material, &frame.scratch[3], &frame.scratch[4], &frame.pre, &frame.pre, &frame.pre],
                &frame.scratch[0],
                &lane.states,
                params,
                PigmentStage::Accumulate,
            );
            copy_rect(encoder, &frame.scratch[0], &lane.material, r);
        }
        if core.is_none() && brush.mix_settings.enabled && !brush.erase {
            if self.native_format.is_some() {
                self.transport_native(encoder, &frame, lane_index, d, &brush);
            } else {
                self.transport(encoder, &frame, lane_index, d, &brush);
            }
        }
        self.metadata[lane_index].previous = Some(center);
        self.metadata[lane_index].last = Some(*d);
        self.scatter(encoder, &frame, core.unwrap_or_else(|| aligned(dab_rect)));
        self.recycle(frame);
        self.bounds = self.bounds.union(&dab_rect);
    }
    fn transport_params(&self, region: Rect, rect: [u32; 4], index: usize, d: &Dab, brush: &BrushSettings) -> TransportParams {
        let settings = &brush.mix_settings;
        let meta = &self.metadata[index];
        let previous = meta.previous.unwrap_or([d.center.x as f32, d.center.y as f32]);
        let dx = d.center.x as f32 - previous[0];
        let dy = d.center.y as f32 - previous[1];
        let distance = dx.hypot(dy);
        let push = distance.min(2.0 * d.radius * brush.spacing.max(0.01)) * (0.18 + d.mix_length * 1.45).min(1.0);
        let halo = ((d.radius * (0.18 + d.blur_strength * 0.82)).clamp(1.5, 127.0) + d.radius * 2.0 * brush.spacing).ceil() as i32;
        let sample = expanded(self.ctx.dab_rect(d, index == 1), halo);
        TransportParams {
            rect,
            offset: if distance > 0.0001 { [-dx / distance * push, -dy / distance * push] } else { [0.0; 2] },
            strength: d.mix_strength,
            blur_radius: settings.quality,
            target: settings.blur_color_target.unwrap_or([0.0, 0.0, 0.0, -1.0]),
            center: [d.center.x as f32 - region.x0 as f32, d.center.y as f32 - region.y0 as f32],
            radius: d.radius.max(0.5),
            previous_radius: meta.carrier_radius,
            reservoir: [settings.strength, settings.water, d.mix_length, distance / (d.radius * 2.0).max(1.0)],
            sample_rect: [sample.x0 - region.x0, sample.y0 - region.y0, sample.x1 - region.x0, sample.y1 - region.y0],
            paint: [0.0, 0.0, 0.0, brush.pigment_mix_strength],
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn carrier_patches(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        index: usize,
        d: &Dab,
        brush: &BrushSettings,
        old: &wgpu::Texture,
        next: &wgpu::Texture,
        initial: bool,
        alphas: Option<[&wgpu::Texture; 2]>,
    ) -> Result<(), String> {
        let side = next.width();
        let sample_params = self.transport_params(Rect::new(0, 0, 1, 1), [0, 0, 1, 1], index, d, brush);
        let sample = sample_params.sample_rect;
        for y in (0..side).step_by(SIDE as usize) {
            for x in (0..side).step_by(SIDE as usize) {
                let w = SIDE.min(side - x);
                let h = SIDE.min(side - y);
                let center = [d.center.x as f32, d.center.y as f32];
                let origin = if brush.mix_settings.algorithm == MixAlgorithm::V1 {
                    [center[0].floor() - (side / 2) as f32, center[1].floor() - (side / 2) as f32]
                } else {
                    [center[0] - side as f32 * 0.5, center[1] - side as f32 * 0.5]
                };
                let clamp_x = |value: f32| (value.floor() as i32).clamp(sample[0], sample[2] - 1);
                let clamp_y = |value: f32| (value.floor() as i32).clamp(sample[1], sample[3] - 1);
                let region = aligned(Rect::new(
                    clamp_x(origin[0] + x as f32),
                    clamp_y(origin[1] + y as f32),
                    clamp_x(origin[0] + (x + w) as f32) + 1,
                    clamp_y(origin[1] + (y + h) as f32) + 1,
                ));
                let frame = self.gather(encoder, region);
                let mut params = self.transport_params(region, [x, y, w, h], index, d, brush);
                if brush.mix_settings.algorithm == MixAlgorithm::V1 {
                    params.previous_radius = if initial { 0.0 } else { d.radius };
                }
                let stage = if brush.mix_settings.algorithm == MixAlgorithm::V1 { Stage::CarrierV1 } else { Stage::CarrierV2 };
                if let (Some(format), Some(alpha)) = (&self.native_format, alphas) {
                    let work_alpha = frame.lanes[index].work_alpha.as_ref().ok_or("GPU native carrier is missing its alpha texture")?;
                    self.kit.native_transport.encode(
                        &self.kit.device,
                        encoder,
                        [&frame.lanes[index].work, work_alpha],
                        &frame.raster.coverage,
                        [&self.kit.sink, &self.kit.sink_alpha],
                        [old, alpha[0]],
                        [next, alpha[1]],
                        [&frame.raster.source, &frame.raster.source],
                        params,
                        [format.format.mode.color_channels() as u32, u32::from(format.format.alpha), 0, 0],
                        stage,
                    );
                } else {
                    self.kit.transport.encode(
                        &self.kit.device,
                        encoder,
                        &view(&frame.lanes[index].work),
                        &view(&frame.raster.coverage),
                        &view(&self.kit.sink),
                        &view(old),
                        &view(next),
                        &view(&frame.raster.source),
                        params,
                        if brush.mix_settings.algorithm == MixAlgorithm::V1 { Stage::CarrierV1 } else { Stage::CarrierV2 },
                    );
                }
                self.recycle(frame);
            }
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn transport_page_stage(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        index: usize,
        d: &Dab,
        brush: &BrushSettings,
        stage: Option<Stage>,
        carrier: &wgpu::Texture,
        carrier_alpha: Option<&wgpu::Texture>,
    ) {
        // Every output tile reads one immutable stage snapshot, so neighbours see the same dab state.
        let source = self.clone();
        let kit = self.kit.clone();
        let bounds = self.ctx.dab_rect(d, index == 1);
        let halo = if matches!(stage, Some(Stage::Blur)) { (d.radius * (0.18 + d.blur_strength * 0.82)).clamp(1.5, 127.0).ceil() as i32 + 2 } else { 2 };
        for coord in bounds.tiles() {
            let region = aligned(expanded(coord.rect(), halo));
            let mut frame = source.gather(encoder, region);
            let origin = self.metadata[index].origin.unwrap_or([d.center.x as f32, d.center.y as f32]);
            let Some(raster) = self.raster[index].as_ref() else {
                self.fail("GPU brush raster resources are missing".into());
                return;
            };
            raster.encode_into(
                &kit.raster,
                &kit.device,
                encoder,
                d,
                [region.x0, region.y0],
                origin,
                &view(&frame.pre),
                &mut frame.raster,
                self.native_format.as_deref(),
            );
            let core = coord.rect().intersect(&bounds);
            let r = [(core.x0 - region.x0) as u32, (core.y0 - region.y0) as u32, core.width(), core.height()];
            let lane = &frame.lanes[index];
            let mut params = self.transport_params(region, r, index, d, brush);
            let mut remote = None;
            if let Some(stage) = stage {
                match stage {
                    Stage::DepositV2 => params.strength *= brush.mix_settings.concentration * brush.mix_settings.output_strength,
                    Stage::ColorOutput => params.strength = d.opacity,
                    Stage::Blur => {
                        params.strength = d.blur_strength;
                        params.blur_radius = (d.radius * (0.18 + d.blur_strength * 0.82)).clamp(1.5, 127.0);
                    }
                    Stage::Pickup => {
                        let sample = params.sample_rect;
                        let clamp_x = |value: f32| (value.floor() as i32).clamp(sample[0], sample[2] - 1) + region.x0;
                        let clamp_y = |value: f32| (value.floor() as i32).clamp(sample[1], sample[3] - 1) + region.y0;
                        let remote_region = aligned(Rect::new(
                            clamp_x(r[0] as f32 + params.offset[0]),
                            clamp_y(r[1] as f32 + params.offset[1]),
                            clamp_x((r[0] + r[2]) as f32 + params.offset[0]) + 1,
                            clamp_y((r[1] + r[3]) as f32 + params.offset[1]) + 1,
                        ));
                        remote = Some(source.gather(encoder, remote_region));
                        params.paint[0] = (region.x0 - remote_region.x0) as f32;
                        params.paint[1] = (region.y0 - remote_region.y0) as f32;
                        params.paint[2] = 1.0;
                    }
                    _ => {}
                }
                let color = remote.as_ref().map_or(&frame.raster.source, |f| &f.lanes[index].work);
                if let Some(format) = &self.native_format {
                    let Some(work_alpha) = lane.work_alpha.as_ref() else {
                        self.fail("GPU native transport is missing its alpha texture".into());
                        return;
                    };
                    let color_pair = if let Some(remote) = &remote {
                        let Some(alpha) = remote.lanes[index].work_alpha.as_ref() else {
                            self.fail("GPU native pickup is missing its alpha texture".into());
                            return;
                        };
                        [&remote.lanes[index].work, alpha]
                    } else {
                        format.encode(&kit.device, encoder, false, &frame.raster.source, &frame.raster.source, &frame.scratch[3], &frame.scratch[4], r);
                        [&frame.scratch[3], &frame.scratch[4]]
                    };
                    kit.native_transport.encode(
                        &kit.device,
                        encoder,
                        [&lane.work, work_alpha],
                        &frame.raster.coverage,
                        [&frame.scratch[0], &frame.scratch[1]],
                        [carrier, carrier_alpha.unwrap_or(&kit.zero)],
                        [&kit.sink, &kit.sink_alpha],
                        color_pair,
                        params,
                        [format.format.mode.color_channels() as u32, u32::from(format.format.alpha), 0, 0],
                        stage,
                    );
                    copy_rect(encoder, &frame.scratch[1], work_alpha, r);
                } else {
                    kit.transport.encode(
                        &kit.device,
                        encoder,
                        &view(&lane.work),
                        &view(&frame.raster.coverage),
                        &view(&frame.scratch[0]),
                        &view(carrier),
                        &view(&kit.sink),
                        &view(color),
                        params,
                        stage,
                    );
                }
                copy_rect(encoder, &frame.scratch[0], &lane.work, r);
            } else {
                let scale = match self.pre.format().sample {
                    photocraft_color::SampleType::U8 => 255.0,
                    photocraft_color::SampleType::U16 => 65535.0,
                    photocraft_color::SampleType::F32 => 0.0,
                };
                if scale > 0.0 {
                    kit.glue(encoder, [&lane.work, &frame.pre, &frame.pre, &frame.pre], &frame.scratch[0], r, 7, [scale, 0.0, 0.0, 0.0]);
                    copy_rect(encoder, &frame.scratch[0], &lane.work, r);
                    if let Some(alpha) = &lane.work_alpha {
                        kit.glue(encoder, [alpha, &frame.pre, &frame.pre, &frame.pre], &frame.scratch[0], r, 7, [scale, 0.0, 0.0, 0.0]);
                        copy_rect(encoder, &frame.scratch[0], alpha, r);
                    }
                }
            }
            let mut lanes = self.pages[&coord].lanes.clone();
            let work = texture(&kit.device, [SIDE, SIDE]);
            copy(encoder, &lane.work, &work, [(coord.rect().x0 - region.x0) as u32, (coord.rect().y0 - region.y0) as u32], [0, 0], [SIDE, SIDE]);
            lanes[index].work = work;
            if let Some(source) = &lane.work_alpha {
                let alpha = texture(&kit.device, [SIDE, SIDE]);
                copy(encoder, source, &alpha, [(coord.rect().x0 - region.x0) as u32, (coord.rect().y0 - region.y0) as u32], [0, 0], [SIDE, SIDE]);
                lanes[index].work_alpha = Some(alpha);
            }
            self.pages.insert(coord, Arc::new(Page { lanes }));
            self.dirty.insert(coord);
            self.recycle(frame);
            if let Some(remote) = remote {
                self.recycle(remote);
            }
        }
    }
    fn paged_native_transport(&mut self, encoder: &mut wgpu::CommandEncoder, index: usize, d: &Dab) {
        let brush = self.brush(index).clone();
        let settings = &brush.mix_settings;
        let kit = self.kit.clone();
        let old = self.metadata[index].carrier.clone();
        let old_alpha = self.metadata[index].carrier_alpha.clone();
        let old_pair = [old.as_deref().unwrap_or(&kit.zero), old_alpha.as_deref().unwrap_or(&kit.zero)];
        let mut carrier = None;
        if settings.smudge_enabled && d.mix_strength > 0.0001 {
            match settings.algorithm {
                MixAlgorithm::V1 => {
                    let side = ((brush.size * 0.5).ceil() as u32 + 2) * 2 + 1;
                    let initial: [wgpu::Texture; 2] = std::array::from_fn(|_| texture(&kit.device, [side, side]));
                    if old.is_some() {
                        for (a, b) in old_pair.into_iter().zip(&initial) {
                            let w = a.width().min(side);
                            let h = a.height().min(side);
                            copy(encoder, a, b, [(a.width() - w) / 2, (a.height() - h) / 2], [(side - w) / 2, (side - h) / 2], [w, h]);
                        }
                    } else {
                        if let Err(error) = self.carrier_patches(encoder, index, d, &brush, old_pair[0], &initial[0], true, Some([old_pair[1], &initial[1]])) {
                            self.fail(error);
                            return;
                        }
                    }
                    self.transport_page_stage(encoder, index, d, &brush, Some(Stage::DepositV1), &initial[0], Some(&initial[1]));
                    let updated: [wgpu::Texture; 2] = std::array::from_fn(|_| texture(&kit.device, [side, side]));
                    if let Err(error) = self.carrier_patches(encoder, index, d, &brush, &initial[0], &updated[0], false, Some([&initial[1], &updated[1]])) {
                        self.fail(error);
                        return;
                    }
                    carrier = Some(updated);
                }
                MixAlgorithm::V2 => {
                    let side = (d.radius * 2.0).ceil().max(1.0) as u32;
                    let next: [wgpu::Texture; 2] = std::array::from_fn(|_| texture(&kit.device, [side, side]));
                    if let Err(error) = self.carrier_patches(encoder, index, d, &brush, old_pair[0], &next[0], false, Some([old_pair[1], &next[1]])) {
                        self.fail(error);
                        return;
                    }
                    self.transport_page_stage(encoder, index, d, &brush, Some(Stage::DepositV2), &next[0], Some(&next[1]));
                    carrier = Some(next);
                }
                MixAlgorithm::V3 => self.transport_page_stage(encoder, index, d, &brush, Some(Stage::Pickup), old_pair[0], Some(old_pair[1])),
            }
        }
        if settings.color_output {
            self.transport_page_stage(encoder, index, d, &brush, Some(Stage::ColorOutput), old_pair[0], Some(old_pair[1]));
        }
        if settings.blur_enabled && d.blur_strength > 0.0001 {
            self.transport_page_stage(encoder, index, d, &brush, Some(Stage::Blur), old_pair[0], Some(old_pair[1]));
        }
        self.transport_page_stage(encoder, index, d, &brush, None, old_pair[0], Some(old_pair[1]));
        if let Some([color, alpha]) = carrier {
            self.metadata[index].carrier = Some(Arc::new(color));
            self.metadata[index].carrier_alpha = Some(Arc::new(alpha));
            self.metadata[index].carrier_radius = d.radius;
        }
    }
    fn paged_transport(&mut self, encoder: &mut wgpu::CommandEncoder, index: usize, d: &Dab) {
        if self.native_format.is_some() {
            self.paged_native_transport(encoder, index, d);
            return;
        }
        let brush = self.brush(index).clone();
        let settings = &brush.mix_settings;
        let kit = self.kit.clone();
        let old = self.metadata[index].carrier.clone();
        let empty = &kit.zero;
        let old_texture = old.as_deref().unwrap_or(empty);
        let mut carrier = None;
        if settings.smudge_enabled && d.mix_strength > 0.0001 {
            match settings.algorithm {
                MixAlgorithm::V1 => {
                    let side = ((brush.size * 0.5).ceil() as u32 + 2) * 2 + 1;
                    let initial = texture(&kit.device, [side, side]);
                    if let Some(old) = &old {
                        let w = old.width().min(side);
                        let h = old.height().min(side);
                        copy(encoder, old, &initial, [(old.width() - w) / 2, (old.height() - h) / 2], [(side - w) / 2, (side - h) / 2], [w, h]);
                    } else {
                        if let Err(error) = self.carrier_patches(encoder, index, d, &brush, empty, &initial, true, None) {
                            self.fail(error);
                            return;
                        }
                    }
                    self.transport_page_stage(encoder, index, d, &brush, Some(Stage::DepositV1), &initial, None);
                    let updated = texture(&kit.device, [side, side]);
                    if let Err(error) = self.carrier_patches(encoder, index, d, &brush, &initial, &updated, false, None) {
                        self.fail(error);
                        return;
                    }
                    carrier = Some(updated);
                }
                MixAlgorithm::V2 => {
                    let side = (d.radius * 2.0).ceil().max(1.0) as u32;
                    let next = texture(&kit.device, [side, side]);
                    if let Err(error) = self.carrier_patches(encoder, index, d, &brush, old_texture, &next, false, None) {
                        self.fail(error);
                        return;
                    }
                    self.transport_page_stage(encoder, index, d, &brush, Some(Stage::DepositV2), &next, None);
                    carrier = Some(next);
                }
                MixAlgorithm::V3 => self.transport_page_stage(encoder, index, d, &brush, Some(Stage::Pickup), old_texture, None),
            }
        }
        if settings.color_output {
            self.transport_page_stage(encoder, index, d, &brush, Some(Stage::ColorOutput), old_texture, None);
        }
        if settings.blur_enabled && d.blur_strength > 0.0001 {
            self.transport_page_stage(encoder, index, d, &brush, Some(Stage::Blur), old_texture, None);
        }
        self.transport_page_stage(encoder, index, d, &brush, None, old_texture, None);
        if let Some(carrier) = carrier {
            self.metadata[index].carrier = Some(Arc::new(carrier));
            self.metadata[index].carrier_radius = d.radius;
        }
    }
    fn transport_native(&mut self, encoder: &mut wgpu::CommandEncoder, frame: &Frame, index: usize, d: &Dab, brush: &BrushSettings) {
        let kit = self.kit.clone();
        let Some(format) = self.native_format.as_ref() else {
            self.fail("GPU native transport is missing its document format".into());
            return;
        };
        let lane = &frame.lanes[index];
        let Some(work_alpha) = lane.work_alpha.as_ref() else {
            self.fail("GPU native transport is missing its alpha texture".into());
            return;
        };
        let settings = &brush.mix_settings;
        let r = frame.raster.rect;
        let mut params = self.transport_params(frame.region, r, index, d, brush);
        let old = self.metadata[index].carrier.clone();
        let old_alpha = self.metadata[index].carrier_alpha.clone();
        let old_pair = [old.as_deref().unwrap_or(&kit.zero), old_alpha.as_deref().unwrap_or(&kit.zero)];
        let kind = [format.format.mode.color_channels() as u32, u32::from(format.format.alpha), 0, 0];
        if settings.color_output {
            format.encode(&kit.device, encoder, false, &frame.raster.source, &frame.raster.source, &frame.scratch[3], &frame.scratch[4], r);
        }
        let run = |encoder: &mut wgpu::CommandEncoder, stage: Stage, p: TransportParams, carry: [&wgpu::Texture; 2], next: [&wgpu::Texture; 2]| {
            kit.native_transport.encode(
                &kit.device,
                encoder,
                [&lane.work, work_alpha],
                &frame.raster.coverage,
                [&frame.scratch[0], &frame.scratch[1]],
                carry,
                next,
                [&frame.scratch[3], &frame.scratch[4]],
                p,
                kind,
                stage,
            );
        };
        let store = |encoder: &mut wgpu::CommandEncoder| {
            copy_rect(encoder, &frame.scratch[0], &lane.work, r);
            copy_rect(encoder, &frame.scratch[1], work_alpha, r);
        };
        let sink = [&kit.sink, &kit.sink_alpha];
        let mut carrier = None;
        if settings.smudge_enabled && d.mix_strength > 0.0001 {
            let side =
                if settings.algorithm == MixAlgorithm::V1 { ((brush.size * 0.5).ceil() as u32 + 2) * 2 + 1 } else { (d.radius * 2.0).ceil().max(1.0) as u32 };
            match settings.algorithm {
                MixAlgorithm::V1 => {
                    let initial: [wgpu::Texture; 2] = std::array::from_fn(|_| texture(&kit.device, [side, side]));
                    let mut cp = params;
                    cp.rect = [0, 0, side, side];
                    if old.is_none() {
                        cp.previous_radius = 0.0;
                        run(encoder, Stage::CarrierV1, cp, old_pair, [&initial[0], &initial[1]]);
                    } else {
                        for (a, b) in old_pair.into_iter().zip(&initial) {
                            let w = a.width().min(side);
                            let h = a.height().min(side);
                            copy(encoder, a, b, [(a.width() - w) / 2, (a.height() - h) / 2], [(side - w) / 2, (side - h) / 2], [w, h]);
                        }
                    }
                    run(encoder, Stage::DepositV1, params, [&initial[0], &initial[1]], sink);
                    store(encoder);
                    let updated: [wgpu::Texture; 2] = std::array::from_fn(|_| texture(&kit.device, [side, side]));
                    cp.previous_radius = d.radius;
                    run(encoder, Stage::CarrierV1, cp, [&initial[0], &initial[1]], [&updated[0], &updated[1]]);
                    carrier = Some(updated);
                }
                MixAlgorithm::V2 => {
                    let next: [wgpu::Texture; 2] = std::array::from_fn(|_| texture(&kit.device, [side, side]));
                    let mut cp = params;
                    cp.rect = [0, 0, side, side];
                    run(encoder, Stage::CarrierV2, cp, old_pair, [&next[0], &next[1]]);
                    params.strength = d.mix_strength * settings.concentration * settings.output_strength;
                    run(encoder, Stage::DepositV2, params, [&next[0], &next[1]], sink);
                    store(encoder);
                    carrier = Some(next);
                }
                MixAlgorithm::V3 => {
                    run(encoder, Stage::Pickup, params, old_pair, sink);
                    store(encoder);
                }
            }
        }
        if settings.color_output {
            params.strength = d.opacity;
            run(encoder, Stage::ColorOutput, params, old_pair, sink);
            store(encoder);
        }
        if settings.blur_enabled && d.blur_strength > 0.0001 {
            params.strength = d.blur_strength;
            params.blur_radius = (d.radius * (0.18 + d.blur_strength * 0.82)).clamp(1.5, 127.0);
            run(encoder, Stage::Blur, params, old_pair, sink);
            store(encoder);
        }
        let scale = match self.pre.format().sample {
            photocraft_color::SampleType::U8 => 255.0,
            photocraft_color::SampleType::U16 => 65535.0,
            photocraft_color::SampleType::F32 => 0.0,
        };
        if scale > 0.0 {
            for input in [&lane.work, work_alpha] {
                kit.glue(encoder, [input, &frame.pre, &frame.pre, &frame.pre], &frame.scratch[0], r, 7, [scale, 0.0, 0.0, 0.0]);
                copy_rect(encoder, &frame.scratch[0], input, r);
            }
        }
        if let Some([color, alpha]) = carrier {
            self.metadata[index].carrier = Some(Arc::new(color));
            self.metadata[index].carrier_alpha = Some(Arc::new(alpha));
            self.metadata[index].carrier_radius = d.radius;
        }
    }
    fn transport(&mut self, encoder: &mut wgpu::CommandEncoder, frame: &Frame, index: usize, d: &Dab, brush: &BrushSettings) {
        let kit = &self.kit;
        let lane = &frame.lanes[index];
        let settings = &brush.mix_settings;
        let r = frame.raster.rect;
        let meta = &mut self.metadata[index];
        let previous = meta.previous.unwrap_or([d.center.x as f32, d.center.y as f32]);
        let dx = d.center.x as f32 - previous[0];
        let dy = d.center.y as f32 - previous[1];
        let distance = dx.hypot(dy);
        let push = distance.min(2.0 * d.radius * brush.spacing.max(0.01)) * (0.18 + d.mix_length * 1.45).min(1.0);
        let halo = ((d.radius * (0.18 + d.blur_strength * 0.82)).clamp(1.5, 127.0) + d.radius * 2.0 * brush.spacing).ceil() as i32;
        let sample = expanded(self.ctx.dab_rect(d, index == 1), halo);
        let mut p = TransportParams {
            rect: r,
            offset: if distance > 0.0001 { [-dx / distance * push, -dy / distance * push] } else { [0.0; 2] },
            strength: d.mix_strength,
            blur_radius: settings.quality,
            target: settings.blur_color_target.unwrap_or([0.0, 0.0, 0.0, -1.0]),
            center: [d.center.x as f32 - frame.region.x0 as f32, d.center.y as f32 - frame.region.y0 as f32],
            radius: d.radius.max(0.5),
            previous_radius: meta.carrier_radius,
            reservoir: [settings.strength, settings.water, d.mix_length, distance / (d.radius * 2.0).max(1.0)],
            sample_rect: [sample.x0 - frame.region.x0, sample.y0 - frame.region.y0, sample.x1 - frame.region.x0, sample.y1 - frame.region.y0],
            paint: [0.0, 0.0, 0.0, brush.pigment_mix_strength],
        };
        let side = match settings.algorithm {
            MixAlgorithm::V1 => ((brush.size * 0.5).ceil() as u32 + 2) * 2 + 1,
            _ => (d.radius * 2.0).ceil().max(1.0) as u32,
        };
        let empty = &kit.zero;
        let sink = &kit.sink;
        let old = meta.carrier.as_deref().unwrap_or(empty);
        let mut carrier = None;
        let run = |encoder: &mut wgpu::CommandEncoder, stage: Stage, params: TransportParams, carry: &wgpu::Texture, next: &wgpu::Texture| {
            kit.transport.encode(
                &kit.device,
                encoder,
                &view(&lane.work),
                &view(&frame.raster.coverage),
                &view(&frame.scratch[0]),
                &view(carry),
                &view(next),
                &view(&frame.raster.source),
                params,
                stage,
            );
        };
        if settings.smudge_enabled && d.mix_strength > 0.0001 {
            match settings.algorithm {
                MixAlgorithm::V1 => {
                    let next = texture(&kit.device, [side, side]);
                    let mut cp = p;
                    cp.rect = [0, 0, side, side];
                    if meta.carrier.is_none() {
                        cp.previous_radius = 0.0;
                        run(encoder, Stage::CarrierV1, cp, old, &next);
                    } else if old.width() == side && old.height() == side {
                        copy(encoder, old, &next, [0, 0], [0, 0], [side, side]);
                    } else {
                        // V1 keeps the carrier centred when tip size changes.
                        let w = old.width().min(side);
                        let h = old.height().min(side);
                        copy(encoder, old, &next, [(old.width() - w) / 2, (old.height() - h) / 2], [(side - w) / 2, (side - h) / 2], [w, h]);
                    }
                    run(encoder, Stage::DepositV1, p, &next, sink);
                    copy_rect(encoder, &frame.scratch[0], &lane.work, r);
                    let updated = texture(&kit.device, [side, side]);
                    cp.previous_radius = d.radius;
                    run(encoder, Stage::CarrierV1, cp, &next, &updated);
                    carrier = Some(updated);
                }
                MixAlgorithm::V2 => {
                    let next = texture(&kit.device, [side, side]);
                    let mut cp = p;
                    cp.rect = [0, 0, side, side];
                    run(encoder, Stage::CarrierV2, cp, old, &next);
                    p.strength = d.mix_strength * settings.concentration * settings.output_strength;
                    run(encoder, Stage::DepositV2, p, &next, sink);
                    copy_rect(encoder, &frame.scratch[0], &lane.work, r);
                    carrier = Some(next);
                }
                MixAlgorithm::V3 => {
                    run(encoder, Stage::Pickup, p, old, sink);
                    copy_rect(encoder, &frame.scratch[0], &lane.work, r);
                }
            }
        }
        if settings.color_output {
            p.strength = d.opacity;
            run(encoder, Stage::ColorOutput, p, old, sink);
            copy_rect(encoder, &frame.scratch[0], &lane.work, r);
        }
        if settings.blur_enabled && d.blur_strength > 0.0001 {
            p.strength = d.blur_strength;
            p.blur_radius = (d.radius * (0.18 + d.blur_strength * 0.82)).clamp(1.5, 127.0);
            run(encoder, Stage::Blur, p, old, sink);
            copy_rect(encoder, &frame.scratch[0], &lane.work, r);
        }
        let scale = match self.pre.format().sample {
            photocraft_color::SampleType::U8 => 255.0,
            photocraft_color::SampleType::U16 => 65535.0,
            photocraft_color::SampleType::F32 => 0.0,
        };
        if scale > 0.0 {
            kit.glue(encoder, [&lane.work, &frame.pre, &frame.pre, &frame.pre], &frame.scratch[0], r, 7, [scale, 0.0, 0.0, 0.0]);
            copy_rect(encoder, &frame.scratch[0], &lane.work, r);
        }
        if let Some(carrier) = carrier {
            meta.carrier = Some(Arc::new(carrier));
            meta.carrier_radius = d.radius;
        }
    }

    fn wet_params(settings: &WetEdgeSettings, rect: [u32; 4]) -> WetParams {
        WetParams {
            rect,
            style: settings.style as u32,
            width_mode: settings.width_mode as u32,
            color_mode: settings.color_mode as u32,
            edge_control: settings.edge_control as u32,
            softness: settings.softness,
            brightness: settings.brightness,
            saturation: settings.saturation,
            _padding: 0.0,
            resolved: [0.0; 4],
        }
    }
    fn prepare_lane(&self, encoder: &mut wgpu::CommandEncoder, frame: &Frame, index: usize) -> Result<PreparedLane, String> {
        let kit = &self.kit;
        let brush = self.brush(index);
        let lane = &frame.lanes[index];
        let r = frame.rect();
        let size = [r[2], r[3]];
        let source = frame.scratch[3].clone();
        let wet_cov = frame.extras[index * 2].clone();
        let colored = frame.extras[index * 2 + 1].clone();
        let mut params = self.native_params(CompositeParams::from_brush(brush, None, false, true), brush, index);
        params.rect = r;
        self.encode_composite(
            encoder,
            [&lane.cov, &lane.color, &frame.pre, &frame.pre, &frame.pre, &frame.pre, &frame.pre, &frame.pre],
            [&source, &frame.scratch[0], &frame.scratch[1]],
            params,
            CompositeStage::Source,
        );
        let transport = brush.mix_settings.enabled && !brush.erase && (index == 0 || self.ctx.secondary_context().is_some());
        let transported = if let (true, Some(format)) = (transport, self.native_format.as_ref()) {
            let alpha = lane.work_alpha.as_ref().ok_or("GPU native lane is missing its alpha texture")?;
            let output = frame.scratch[4].clone();
            format.encode(&kit.device, encoder, true, &lane.work, alpha, &output, &frame.scratch[2], r);
            Some(output)
        } else {
            None
        };
        let source = if transport { transported.as_ref().unwrap_or(&lane.work) } else { &source };
        let wet = if !self.ctx.brush.erase {
            if index == 1 && self.ctx.secondary_context().is_none() {
                self.ctx.brush.dual_brush.wet_edges.then_some(self.ctx.brush.dual_brush.wet_edge.as_ref()).flatten()
            } else {
                brush.wet_edges.then_some(brush.wet_edge.as_ref()).flatten()
            }
        } else {
            None
        };
        if let Some(settings) = wet {
            let params = Self::wet_params(settings, r);
            if settings.outline == WetOutline::Stroke {
                kit.wet.encode(
                    &kit.device,
                    encoder,
                    &view(&lane.cov),
                    &view(&lane.wet),
                    &view(&frame.pre),
                    &view(source),
                    &view(&wet_cov),
                    params,
                    WetStage::StrokeCoverage,
                );
                kit.wet.encode(
                    &kit.device,
                    encoder,
                    &view(&wet_cov),
                    &view(&lane.wet),
                    &view(&frame.pre),
                    &view(source),
                    &view(&colored),
                    params,
                    WetStage::Color,
                );
            } else {
                copy(encoder, &lane.cov, &wet_cov, [0, 0], [0, 0], size);
                copy(encoder, source, &colored, [0, 0], [0, 0], size);
            }
        } else {
            copy(encoder, &lane.cov, &wet_cov, [0, 0], [0, 0], size);
            copy(encoder, source, &colored, [0, 0], [0, 0], size);
        }
        Ok(PreparedLane { coverage: wet_cov, source: colored })
    }

    fn final_maps(&self, encoder: &mut wgpu::CommandEncoder, frame: &mut Frame, lock: bool) -> Result<FinalMaps, String> {
        let kit = &self.kit;
        let r = frame.rect();
        let size = [r[2], r[3]];
        while frame.extras.len() < if self.native_format.is_some() { 16 } else { 12 } {
            frame.extras.push(texture(&kit.device, size));
        }
        let mut prepared: Vec<_> = (0..self.lane_count()).map(|i| self.prepare_lane(encoder, frame, i)).collect::<Result<_, _>>()?;
        let zero = frame.extras[4].clone();
        kit.glue(encoder, [&kit.zero, &kit.zero, &kit.zero, &kit.zero], &zero, r, 4, [0.0; 4]);
        let secondary_raw = frame.lanes.get(1).map_or(&zero, |l| &l.cov);
        let secondary_cov = prepared.get(1).map_or(&zero, |l| &l.coverage);
        let mut params = CompositeParams::from_brush(&self.ctx.brush, self.ctx.secondary_context().map(|c| &c.brush), lock, true);
        params.rect = r;
        params.origin = [frame.region.x0, frame.region.y0];
        if self.ctx.brush.wet_edges && self.ctx.brush.wet_edge.is_none() && !self.ctx.brush.erase {
            params.mask[3] |= 2;
        }
        let mut secondary_texture =
            RasterOutput { coverage: frame.extras[5].clone(), depth: frame.raster.depth.clone(), source: frame.raster.source.clone(), rect: r, size };
        if let (Some(raster), Some(last), Some(context)) = (&self.raster[1], self.metadata[1].last, self.ctx.secondary_context())
            && context.brush.texture.enabled
            && !context.brush.texture.each_tip
        {
            kit.glue(encoder, [&zero, &zero, &zero, &zero], &frame.scratch[3], r, 4, [1.0; 4]);
            kit.glue(
                encoder,
                [secondary_cov, &frame.scratch[3], &zero, &zero],
                &frame.scratch[4],
                r,
                3,
                [self.ctx.brush.dual_brush.secondary_strength, 0.0, 0.0, 0.0],
            );
            raster.encode_stroke_texture(
                &kit.raster,
                &kit.device,
                encoder,
                &last,
                [frame.region.x0, frame.region.y0],
                self.metadata[1].origin.unwrap_or([0.0; 2]),
                &view(&frame.scratch[4]),
                &mut secondary_texture,
            );
        }
        let coverage = frame.extras[6].clone();
        let raw_combined = frame.extras[7].clone();
        let original_primary_texture = params.mask[1];
        params.mask[1] = 0;
        self.encode_composite(
            encoder,
            [&frame.lanes[0].cov, secondary_raw, &prepared[0].coverage, secondary_cov, &zero, &secondary_texture.coverage, &zero, &zero],
            [&raw_combined, &frame.scratch[0], &frame.scratch[1]],
            params,
            CompositeStage::Coverage,
        );
        if original_primary_texture != 0 {
            let mut primary_texture =
                RasterOutput { coverage: frame.extras[8].clone(), depth: frame.raster.depth.clone(), source: frame.raster.source.clone(), rect: r, size };
            if let (Some(raster), Some(last)) = (&self.raster[0], self.metadata[0].last) {
                raster.encode_stroke_texture(
                    &kit.raster,
                    &kit.device,
                    encoder,
                    &last,
                    [frame.region.x0, frame.region.y0],
                    self.metadata[0].origin.unwrap_or([0.0; 2]),
                    &view(&raw_combined),
                    &mut primary_texture,
                );
            }
            params.mask[1] = original_primary_texture;
            self.encode_composite(
                encoder,
                [
                    &frame.lanes[0].cov,
                    secondary_raw,
                    &prepared[0].coverage,
                    secondary_cov,
                    &primary_texture.coverage,
                    &secondary_texture.coverage,
                    &zero,
                    &zero,
                ],
                [&coverage, &frame.scratch[0], &frame.scratch[1]],
                params,
                CompositeStage::Coverage,
            );
        } else {
            copy(encoder, &raw_combined, &coverage, [0, 0], [0, 0], size);
        }
        // Legacy dual wet colour modifies the primary lane's source after primary material colour.
        if self.ctx.secondary_context().is_none()
            && self.lane_count() > 1
            && self.ctx.brush.dual_brush.wet_edges
            && let Some(settings) = &self.ctx.brush.dual_brush.wet_edge
        {
            let colored = frame.scratch[3].clone();
            let stage = if settings.outline == WetOutline::Single { WetStage::SingleAccumulatedColor } else { WetStage::Color };
            kit.wet.encode(
                &kit.device,
                encoder,
                &view(&prepared[1].coverage),
                &view(&frame.lanes[1].wet),
                &view(&frame.pre),
                &view(&prepared[0].source),
                &view(&colored),
                Self::wet_params(settings, r),
                stage,
            );
            prepared[0].source = colored;
        }
        let wet_source = prepared[0].source.clone();
        let mut candidate = frame.extras[9].clone();
        params.controls = [
            u32::from(self.ctx.brush.mix_settings.enabled && !self.ctx.brush.erase),
            u32::from(self.ctx.secondary_context().is_some_and(|c| c.brush.mix_settings.enabled && !c.brush.erase)),
            0,
            0,
        ];
        let secondary_source = prepared.get(1).map_or(&zero, |l| &l.source);
        self.encode_composite(
            encoder,
            [&frame.pre, &frame.lanes[0].cov, &prepared[0].source, secondary_raw, secondary_source, &frame.selection, &coverage, &zero],
            [&candidate, &frame.scratch[0], &frame.scratch[1]],
            params,
            CompositeStage::Composite,
        );
        let pigment_active = !self.ctx.brush.erase
            && (self.ctx.brush.pigment.as_ref().is_some_and(|p| p.enabled)
                || self.ctx.secondary_context().is_some_and(|c| c.brush.pigment.as_ref().is_some_and(|p| p.enabled)));
        if !pigment_active {
            return Ok(FinalMaps { coverage, output: candidate, wet_source });
        }
        if let (Some(format), Some(pre)) = (&self.native_format, &frame.native_pre) {
            let b = &self.ctx.brush;
            let native_only = self.ctx.secondary_context().is_none()
                && !b.color_attributes.enabled
                && b.pigment_mix_strength <= 0.0
                && matches!(b.mode, photocraft_color::BlendMode::Normal | photocraft_color::BlendMode::Dissolve)
                && !b.mix_settings.enabled;
            let mut checkpoint = self.native_params(params, b, 0).with_native(format, b, native_only);
            checkpoint.controls = [0, u32::from(b.aliased), 0, 0];
            if self.native_wet_color() {
                checkpoint.native[3] |= 2;
            }
            self.encode_composite(
                encoder,
                [&pre[0], &pre[1], &frame.lanes[0].color, &frame.lanes[0].cov, &candidate, &frame.selection, &coverage, &wet_source],
                [&frame.extras[12], &frame.extras[13], &frame.scratch[0]],
                checkpoint,
                CompositeStage::NativeFinish,
            );
            format.encode(&kit.device, encoder, true, &frame.extras[12], &frame.extras[13], &frame.extras[14], &frame.scratch[2], r);
            candidate = frame.extras[14].clone();
            let secondary = if self.ctx.secondary_context().is_some() { secondary_raw } else { &zero };
            let bounds = self.bounds;
            kit.glue(
                encoder,
                [&frame.lanes[0].cov, secondary, &zero, &zero],
                &frame.extras[15],
                r,
                9,
                [
                    (bounds.x0 - frame.region.x0) as f32,
                    (bounds.y0 - frame.region.y0) as f32,
                    (bounds.x1 - frame.region.x0) as f32,
                    (bounds.y1 - frame.region.y0) as f32,
                ],
            );
        }
        let mut shaded = Vec::new();
        for (i, lane) in frame.lanes.iter().enumerate().take(if self.ctx.secondary_context().is_some() { 2 } else { 1 }) {
            if !self.pigment_lane(i) {
                shaded.push(candidate.clone());
                continue;
            }
            let material = frame.scratch[4].clone();
            let result = frame.extras[10 + i].clone();
            let opacity = if self.ctx.secondary_context().is_some() { 1.0 } else { self.ctx.brush.opacity };
            kit.glue(encoder, [&coverage, &frame.selection, &zero, &zero], &frame.scratch[3], r, 3, [opacity, 0.0, 0.0, 0.0]);
            kit.pigment(
                encoder,
                [&lane.material, &zero, &zero, &zero, &zero, &frame.scratch[3]],
                &material,
                &lane.states,
                PigmentParams { rect: r, ..Default::default() },
                PigmentStage::Prepare,
            );
            kit.glue(encoder, [&coverage, &frame.selection, &zero, &zero], &frame.scratch[3], r, 3, [1.0, 0.0, 0.0, 0.0]);
            kit.pigment(
                encoder,
                [&candidate, &zero, &zero, &material, &candidate, &frame.scratch[3]],
                &result,
                &lane.states,
                PigmentParams { rect: r, ..Default::default() },
                PigmentStage::Shade,
            );
            shaded.push(result);
        }
        if shaded.len() == 1 {
            return Ok(FinalMaps { coverage, output: shaded.remove(0), wet_source });
        }
        let output = frame.scratch[2].clone();
        self.encode_composite(
            encoder,
            [&shaded[0], &frame.lanes[0].cov, &shaded[1], secondary_raw, &zero, &zero, &zero, &zero],
            [&output, &frame.scratch[0], &frame.scratch[1]],
            params,
            CompositeStage::Merge,
        );
        Ok(FinalMaps { coverage, output, wet_source })
    }
    fn selection(&self, encoder: &mut wgpu::CommandEncoder, frame: &Frame, selection: Option<&Surface>) {
        let Some(selection) = selection else { return };
        let mut cache = self.selection_cache.lock().unwrap_or_else(|e| e.into_inner());
        let default = selection.default_pixel();
        for coord in frame.region.tiles() {
            let tile = selection.tile(coord);
            let valid = cache.get(&coord).is_some_and(|cached| {
                cached.format == selection.format()
                    && cached.default == default
                    && match (&cached.tile, tile) {
                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                        (None, None) => true,
                        _ => false,
                    }
            });
            if !valid {
                let pixels = selection.read_region(coord.rect());
                let rgba: Vec<[f32; 4]> = pixels.chunks_exact(selection.channels()).map(|p| [p[0]; 4]).collect();
                let source = Arc::new(texture(&self.kit.device, [SIDE, SIDE]));
                self.kit.queue.write_texture(
                    source.as_image_copy(),
                    bytemuck::cast_slice(&rgba),
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIDE * 16), rows_per_image: Some(SIDE) },
                    wgpu::Extent3d { width: SIDE, height: SIDE, depth_or_array_layers: 1 },
                );
                cache.insert(coord, SelectionUpload { tile: tile.cloned(), format: selection.format(), default: default.clone(), texture: source });
            }
            if let Some(cached) = cache.get(&coord) {
                copy(
                    encoder,
                    &cached.texture,
                    &frame.selection,
                    [0, 0],
                    [(coord.rect().x0 - frame.region.x0) as u32, (coord.rect().y0 - frame.region.y0) as u32],
                    [SIDE, SIDE],
                );
            }
        }
    }
    fn fail(&mut self, message: String) {
        log::error!("GPU stroke: {message}");
        self.error = Some(message);
    }
}
struct SelectionUpload {
    tile: Option<Arc<Tile>>,
    format: PixelFormat,
    default: Vec<f32>,
    texture: Arc<wgpu::Texture>,
}
struct PreparedLane {
    coverage: wgpu::Texture,
    source: wgpu::Texture,
}
struct FinalMaps {
    coverage: wgpu::Texture,
    output: wgpu::Texture,
    wet_source: wgpu::Texture,
}

impl StrokeBackend for GpuStroke {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn error(&self) -> Option<String> {
        self.error.clone().or_else(|| self.kit.health.fault().map(|fault| fault.to_string()))
    }
    fn set_error(&mut self, error: String) {
        self.fail(error);
    }
    fn fork(&self) -> Box<dyn StrokeBackend> {
        Box::new(self.clone())
    }
    fn rasterize(&mut self, _ctx: &BrushContext, primary: &[Dab], secondary: &[Dab]) {
        if self.error().is_some() || (primary.is_empty() && secondary.is_empty()) {
            return;
        }
        let kit = self.kit.clone();
        // Each compute pass may allocate a Metal command buffer. Whole preview strokes
        // must submit bounded batches before reaching Metal's 4096-buffer limit.
        for (lane, dabs) in [primary, secondary].into_iter().enumerate().take(self.lane_count()) {
            for batch in dabs.chunks(16) {
                let mut encoder = kit.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pc_stroke_dabs") });
                for d in batch {
                    self.dab(&mut encoder, lane, d);
                    if self.error().is_some() {
                        return;
                    }
                }
                kit.queue.submit([encoder.finish()]);
            }
        }
    }
    fn bounds(&self) -> Rect {
        self.bounds
    }
    fn mark_dirty(&mut self, rect: Rect) {
        self.dirty.extend(rect.tiles());
    }
    fn take_dirty_rect(&mut self) -> Rect {
        std::mem::take(&mut self.dirty).iter().fold(Rect::EMPTY, |r, c| r.union(&c.rect())).intersect(&self.bounds)
    }
    fn take_dirty_tiles(&mut self) -> Vec<TileCoord> {
        std::mem::take(&mut self.dirty).into_iter().collect()
    }
    fn retain_tiles(&mut self, tiles: &[TileCoord]) {
        let keep: BTreeSet<_> = tiles.iter().copied().collect();
        self.pages.retain(|coord, _| keep.contains(coord));
        self.dirty.retain(|coord| keep.contains(coord));
    }
    fn composite(&mut self, target: &mut Surface, selection: Option<&Surface>, lock: bool, all: bool) -> Rect {
        if self.error().is_some() {
            return Rect::EMPTY;
        }
        let keys: BTreeSet<_> =
            if all { self.pages.keys().copied().collect() } else { self.dirty.iter().flat_map(|c| expanded(c.rect(), SIDE as i32).tiles()).collect() };
        self.dirty.clear();
        let mut damage = Rect::EMPTY;
        let kit = self.kit.clone();
        let mut encoder = kit.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pc_stroke_composite") });
        let mut updates = Vec::new();
        for coord in keys {
            if self.error().is_some() {
                return Rect::EMPTY;
            }
            if !self.pages.contains_key(&coord) {
                continue;
            }
            let region = expanded(coord.rect(), SIDE as i32);
            let mut frame = self.gather(&mut encoder, region);
            self.selection(&mut encoder, &frame, selection);
            let maps = match self.final_maps(&mut encoder, &mut frame, lock) {
                Ok(maps) => maps,
                Err(error) => {
                    self.fail(error);
                    return Rect::EMPTY;
                }
            };
            let quantized = frame.scratch[4].clone();
            let mut params = CompositeParams::from_brush(&self.ctx.brush, self.ctx.secondary_context().map(|c| &c.brush), lock, true);
            params.rect = frame.rect();
            params.origin = [region.x0, region.y0];
            let pigment_active = !self.ctx.brush.erase
                && (self.ctx.brush.pigment.as_ref().is_some_and(|p| p.enabled)
                    || self.ctx.secondary_context().is_some_and(|c| c.brush.pigment.as_ref().is_some_and(|p| p.enabled)));
            params.controls =
                [u32::from(target.format().sample == photocraft_color::SampleType::U8), u32::from(self.ctx.brush.aliased), u32::from(pigment_active), 0];
            let capture = if let (Some(format), Some(pre)) = (&self.native_format, &frame.native_pre) {
                let b = &self.ctx.brush;
                let native_only = self.ctx.secondary_context().is_none()
                    && !b.color_attributes.enabled
                    && b.pigment_mix_strength <= 0.0
                    && matches!(b.mode, photocraft_color::BlendMode::Normal | photocraft_color::BlendMode::Dissolve)
                    && !b.mix_settings.enabled
                    && !pigment_active;
                params = self.native_params(params, b, 0).with_native(format, b, native_only);
                if self.native_wet_color() {
                    params.native[3] |= 2;
                }
                self.encode_composite(
                    &mut encoder,
                    [
                        &pre[0],
                        &pre[1],
                        &frame.lanes[0].color,
                        &frame.lanes[0].cov,
                        &maps.output,
                        &frame.selection,
                        &maps.coverage,
                        if pigment_active { &frame.extras[15] } else { &maps.wet_source },
                    ],
                    [&frame.extras[12], &frame.extras[13], &frame.scratch[0]],
                    params,
                    CompositeStage::NativeFinish,
                );
                kit.snapshots.capture_native(&kit.device, &kit.queue, &mut encoder, &frame.extras[12], &frame.extras[13], [SIDE, SIDE], format)
            } else {
                self.encode_composite(
                    &mut encoder,
                    [&maps.output, &maps.coverage, &frame.pre, &frame.pre, &frame.pre, &frame.selection, &frame.pre, &frame.pre],
                    [&quantized, &frame.scratch[0], &frame.scratch[1]],
                    params,
                    CompositeStage::Dither,
                );
                kit.snapshots.capture(&kit.device, &kit.queue, &mut encoder, &quantized, [SIDE, SIDE], target.format())
            };
            let tile = match capture {
                Ok(tile) => tile,
                Err(e) => {
                    self.fail(e.to_string());
                    return Rect::EMPTY;
                }
            };
            self.recycle(frame);
            updates.push((coord, tile));
            damage = damage.union(&coord.rect());
        }
        if !updates.is_empty() {
            if self.error().is_some() {
                return Rect::EMPTY;
            }
            kit.queue.submit([encoder.finish()]);
            if self.error().is_some() {
                return Rect::EMPTY;
            }
            for (coord, tile) in updates {
                target.set_tile(coord, tile);
            }
        }
        damage
    }
    fn union_max(&mut self, other: &dyn StrokeBackend) {
        if let Some(error) = self.error().or_else(|| other.error()) {
            self.fail(error);
            return;
        }
        let Some(other) = other.as_any().downcast_ref::<Self>() else {
            self.fail("Cannot union strokes from different GPU backends".into());
            return;
        };
        let kit = self.kit.clone();
        for (coord, incoming) in &other.pages {
            if self.error().is_some() {
                return;
            }
            let Some(old) = self.pages.get(coord) else {
                self.pages.insert(*coord, incoming.clone());
                self.dirty.insert(*coord);
                continue;
            };
            let output = Page {
                lanes: (0..self.lane_count()).map(|i| Lane::new(&kit.device, [SIDE, SIDE], self.pigment_lane(i), self.native_format.is_some())).collect(),
            };
            let mut encoder = kit.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pc_stroke_union") });
            for (i, ((a, b), out)) in old.lanes.iter().zip(&incoming.lanes).zip(&output.lanes).enumerate() {
                let inputs = [view(&a.cov), view(&a.color), view(&a.wet), view(&a.material), view(&b.cov), view(&b.color), view(&b.wet), view(&b.material)];
                let outputs = [view(&out.cov), view(&out.color), view(&out.wet), view(&out.material)];
                kit.union.encode(
                    &kit.device,
                    &mut encoder,
                    std::array::from_fn(|i| &inputs[i]),
                    std::array::from_fn(|i| &outputs[i]),
                    [&a.states, &b.states, &out.states],
                    [0, 0, SIDE, SIDE],
                    if self.pigment_lane(i) { 7 } else { 3 },
                );
                kit.glue(&mut encoder, [&a.work, &b.work, &a.cov, &b.cov], &out.work, [0, 0, SIDE, SIDE], 8, [0.0; 4]);
                if let (Some(a), Some(out)) = (&a.work_alpha, &out.work_alpha) {
                    let Some(b_alpha) = b.work_alpha.as_ref() else {
                        self.fail("GPU stroke union is missing its native alpha texture".into());
                        return;
                    };
                    kit.glue(&mut encoder, [a, b_alpha, &old.lanes[i].cov, &incoming.lanes[i].cov], out, [0, 0, SIDE, SIDE], 8, [0.0; 4]);
                }
            }
            if self.error().is_some() {
                return;
            }
            kit.queue.submit([encoder.finish()]);
            if self.error().is_some() {
                return;
            }
            self.pages.insert(*coord, Arc::new(output));
            self.dirty.insert(*coord);
        }
        self.bounds = self.bounds.union(&other.bounds);
    }
    fn coverage_in(&self, rect: Rect) -> Result<Vec<f32>, String> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        if rect.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = vec![0.0; rect.width() as usize * rect.height() as usize];
        for coord in rect.tiles() {
            if let Some(error) = self.error() {
                return Err(error);
            }
            if !self.pages.contains_key(&coord) {
                continue;
            }
            let mut encoder = self.kit.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pc_stroke_coverage") });
            let mut frame = self.gather(&mut encoder, expanded(coord.rect(), SIDE as i32));
            let maps = self.final_maps(&mut encoder, &mut frame, false)?;
            let tile = match self.kit.snapshots.capture(&self.kit.device, &self.kit.queue, &mut encoder, &maps.coverage, [SIDE, SIDE], PixelFormat::RGBA32F) {
                Ok(t) => t,
                Err(e) => return Err(e.to_string()),
            };
            if let Some(error) = self.error() {
                return Err(error);
            }
            self.kit.queue.submit([encoder.finish()]);
            if let Some(error) = self.error() {
                return Err(error);
            }
            self.recycle(frame);
            let bytes = match tile.try_bytes() {
                Ok(b) => b,
                Err(e) => return Err(e.to_string()),
            };
            if let Some(error) = self.error() {
                return Err(error);
            }
            let overlap = coord.rect().intersect(&rect);
            for y in overlap.y0..overlap.y1 {
                for x in overlap.x0..overlap.x1 {
                    let i = ((y - coord.rect().y0) as usize * SIDE as usize + (x - coord.rect().x0) as usize) * 16;
                    let v = f32::from_ne_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
                    out[(y - rect.y0) as usize * rect.width() as usize + (x - rect.x0) as usize] = v;
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_paint::{StrokePoint, dynamics::DabGenerator};

    #[test]
    fn input_sampling_keeps_gpu_tiles_unmaterialized() {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let factory = GpuStrokeFactory::with_health(&device, &queue, crate::DeviceHealth::watch(&device));
        for format in [PixelFormat::RGBA8, PixelFormat::CMYKA8, PixelFormat::new(ColorMode::Lab, photocraft_color::SampleType::F32, true)] {
            let mut native = photocraft_raster::from_rgba(&format, [0.4, 0.7, 0.2, 0.65]);
            if format.mode == ColorMode::Cmyk {
                native[3] = 0.25;
            }
            let color = texture(&device, [SIDE, SIDE]);
            let alpha = texture(&device, [SIDE, SIDE]);
            let mut values = [0.0; 4];
            values[..format.mode.color_channels()].copy_from_slice(&native[..format.mode.color_channels()]);
            if format.mode == ColorMode::Rgb {
                values[3] = native[3];
            }
            for (target, pixel) in [(&color, values), (&alpha, [native[format.mode.color_channels()], 0.0, 0.0, 0.0])] {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: target,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: 17, y: 23, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    bytemuck::cast_slice(&pixel),
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: None, rows_per_image: None },
                    wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
                );
            }
            let mut encoder = device.create_command_encoder(&Default::default());
            let tile = if format.mode == ColorMode::Rgb {
                factory.kit.snapshots.capture(&device, &queue, &mut encoder, &color, [0, 0], format).unwrap()
            } else {
                let conversion = factory.kit.native_formats.get(&device, format).unwrap();
                factory.kit.snapshots.capture_native(&device, &queue, &mut encoder, &color, &alpha, [0, 0], &conversion).unwrap()
            };
            queue.submit([encoder.finish()]);
            let mut surface = Surface::new(format);
            surface.set_tile(TileCoord::new(-1, 0), tile.clone());
            let mut expected = Surface::new(format);
            expected.write_pixel(-239, 23, &native);
            let actual = factory.sample_rgba(&surface, -239, 23).unwrap();
            assert!(!tile.is_cpu_materialized());
            for (actual, expected) in actual.into_iter().zip(expected.rgba(-239, 23)) {
                assert!((actual - expected).abs() < 1e-6, "{format:?}: {actual} != {expected}");
            }
        }
    }

    #[test]
    fn shared_device_fault_stops_stroke_work_without_cpu_fallback() {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let health = crate::DeviceHealth::watch(&device);
        let factory = GpuStrokeFactory::with_health(&device, &queue, health.clone());
        let pre = Surface::new(PixelFormat::RGBA32F);
        let brush = BrushSettings::default();
        let ctx = BrushContext::new(&brush);
        let mut stroke = factory.begin(&ctx, &pre).unwrap();
        let other = factory.begin(&ctx, &pre).unwrap();
        health.mark(crate::Fault::Lost("stroke device reset".into()));
        let error = "GPU device lost: stroke device reset";
        assert_eq!(stroke.error().as_deref(), Some(error));
        assert_eq!(factory.sample_rgba(&pre, 8, 8), Err(error.into()));
        let mut generator = DabGenerator::new(&brush, 1.0);
        let (mut primary, mut secondary) = (Vec::new(), Vec::new());
        generator.push(&[StrokePoint::new(8.0, 8.0, 1.0)], &mut primary, &mut secondary);
        generator.finish(&mut primary, &mut secondary);
        assert!(!primary.is_empty());
        stroke.rasterize(&ctx, &primary, &secondary);
        assert!(stroke.bounds().is_empty());
        let mut target = pre.clone();
        assert_eq!(stroke.composite(&mut target, None, false, true), Rect::EMPTY);
        stroke.union_max(other.as_ref());
        assert_eq!(stroke.error().as_deref(), Some(error));
        assert_eq!(stroke.coverage_in(Rect::new(0, 0, 16, 16)), Err(error.into()));
        let failed = factory.begin(&ctx, &pre).unwrap();
        assert_eq!(failed.error().as_deref(), Some(error));
        let failed = failed.as_any().downcast_ref::<GpuStroke>().unwrap();
        assert!(failed.raster.iter().all(Option::is_none));
        assert!(failed.pages.is_empty());
        assert_eq!(health.errors(), 0);
    }

    #[test]
    fn paged_transport_matches_whole_dab_across_tiles() {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).unwrap();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let factory = GpuStrokeFactory::new(&device, &queue);
        let rect = Rect::new(230, 230, 285, 285);
        for format in [
            PixelFormat::RGBA32F,
            PixelFormat::new(ColorMode::Rgb, photocraft_color::SampleType::F32, false),
            PixelFormat::new(ColorMode::Cmyk, photocraft_color::SampleType::F32, true),
        ] {
            let mut pre = Surface::new(format);
            let pixels: Vec<_> = (rect.y0..rect.y1)
                .flat_map(|y| {
                    (rect.x0..rect.x1).flat_map(move |x| {
                        let mut native = photocraft_raster::from_rgba(&format, [0.2 + (x - 230) as f32 / 100.0, 0.6, 0.3, 0.3 + (y - 230) as f32 / 100.0]);
                        if format.mode == ColorMode::Cmyk {
                            native[3] = 0.3;
                        }
                        native
                    })
                })
                .collect();
            pre.write_region(rect, &pixels);
            for algorithm in [MixAlgorithm::V1, MixAlgorithm::V2, MixAlgorithm::V3] {
                let mut brush = BrushSettings { size: 260.0, spacing: 0.4, pressure_size: true, color: [0.8, 0.2, 0.4, 0.8], ..Default::default() };
                brush.mix_settings.enabled = true;
                brush.mix_settings.algorithm = algorithm;
                brush.mix_settings.strength = 0.65;
                brush.mix_settings.blur_strength = 0.4;
                brush.mix_settings.color_output = true;
                brush.pigment_mix_strength = 0.35;
                let ctx = BrushContext::new(&brush);
                let points = [StrokePoint::new(250.5, 251.25, 0.06), StrokePoint::new(261.75, 260.5, 0.08)];
                let mut generator = DabGenerator::new(&brush, 1.0);
                let (mut primary, mut secondary) = (Vec::new(), Vec::new());
                generator.push(&points, &mut primary, &mut secondary);
                generator.finish(&mut primary, &mut secondary);
                assert!(primary.len() > 1);
                let mut whole = factory.begin(&ctx, &pre).unwrap();
                whole.rasterize(&ctx, &primary, &secondary);
                let mut paged = factory.begin(&ctx, &pre).unwrap().as_any().downcast_ref::<GpuStroke>().unwrap().clone();
                let mut encoder = device.create_command_encoder(&Default::default());
                for dab in &primary {
                    paged.paged_dab(&mut encoder, 0, dab);
                }
                queue.submit([encoder.finish()]);
                let (mut actual, mut expected) = (pre.clone(), pre.clone());
                whole.composite(&mut expected, None, false, true);
                paged.composite(&mut actual, None, false, true);
                assert_eq!(paged.error(), None);
                assert!(actual.tiles().filter(|(_, t)| t.backend().is_some()).all(|(_, t)| !t.is_cpu_materialized()));
                let a = actual.read_region(rect);
                let e = expected.read_region(rect);
                let error = a.iter().zip(e).fold(0.0f32, |m, (a, e)| m.max((a - e).abs()));
                assert!(error <= 1e-5, "{format:?} {algorithm:?}: paged transport error {error}");
            }
        }
    }
}
