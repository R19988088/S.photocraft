# 笔迹 GPU 管线进度

日期：2026-10-20

## 目标

将笔迹渲染中的混色 V1、V2、V3，湿边和颜料模式接入独立 Rust + wgpu 管线，并贯通实时预览、提交、对称绘制、撤销快照和原生色彩通道。

## 已完成

### GPU 管线

- V1、V2、V3 transport：pickup、carrier、deposit、blur 和颜色输出均由 WGSL compute pipeline 完成。
- 湿边：Photoshop、Watercolor、Watercolor 2，以及 Single/Stroke outline 和 dual brush 路径均接入 GPU。
- 颜料：厚度、视觉厚度、流动性、液体混合、高光、emboss 和 pigment state accumulation 接入 GPU。
- 笔尖与材质：采样笔尖、纹理、颗粒、抖动、canvas material、pattern、emboss 和 wet material 接入 GPU。
- 原生格式：RGB、Gray、Lab、CMYK 的 U8/U16/F32 路径支持 GPU 原生通道和独立 alpha；CMYK 使用当前 ICC profile 的 GPU 转换表。
- 稀疏存储：笔迹使用不可变 GPU tile/page，跨 tile 的大笔触使用 paged transport；tail preview、union、对称绘制和撤销快照复用同一组 GPU 页面。
- 通道限制：实时预览和提交使用同一 GPU 通道限制逻辑，CMYK 的 K 通道保持独立。
- 设备故障：GPU health 在 live stroke、preview takeover、commit 和 union 之间共享；故障显式传播到 UI 状态，不重跑 CPU 笔迹。
- Pencil Auto Erase：首点只复制 GPU 中的单个 texel 到 staging buffer，不再物化整块 tile。

### 实时预览与提交

`LiveStroke` 保存笔迹参数、颜色通道限制、预笔迹 surface、对称 renderer 和 GPU health。普通 preview、symmetry preview 和最终 commit 在相同 GPU renderer 上执行；提交时保留 GPU surface，撤销/重做只交换不可变 tile 快照。

### CPU 消费边界

- 通道读取、Apply Image、Quick Mask、通道复制/拆分/合并以及 Content-Aware Fill/Scale 在读取 GPU tile 失败时返回错误，文档编辑保留原快照。
- Quick Mask Options 在文档编辑成功后更新会话选项，读回失败不会留下不一致的选项。
- 传统 CPU retouch、Mixer Brush、Color Replacement 和选区定义笔尖补齐读回错误传播；共享区域读取入口在读取范围内检查 GPU tile。
- `try_tile_mut` 支持仅由 Weak 缓存引用固定的 tile，避免保存、效果或缩略图缓存造成可失败写入误报尺寸溢出。

## 验证证据

### 本轮 CPU 消费边界验证

- `cargo test -p photocraft-raster --lib fallible_writes_copy_tiles_pinned_by_weak_caches`：1 项通过，覆盖跨 tile 写入及 Weak 缓存失效。
- `cargo test -p photocraft-engine --lib readback`：7 项通过，覆盖通道、Quick Mask、content-aware scale、CPU brush/retouch 和实时预览的读回错误与状态保留。
- `cargo test -p photocraft-engine --lib allocation::tests`：2 项通过，覆盖分配失败及限定区域的 GPU 读回失败。
- `cargo clippy -p photocraft-raster -p photocraft-engine --lib -- -D warnings`：通过。
- `cargo check -p photocraft-engine --target wasm32-unknown-unknown`：通过，保留 3 项既有 dead-code warning。
- 本轮修改的 7 个 Rust 文件通过 `rustfmt --check`。

下列记录为此前 GPU 管线验证。

### GPU 自动化测试

- `cargo test -p photocraft-gpu --test brush_gpu -- --nocapture`：通过。覆盖 V1/V2/V3、三种湿边、dual brush、采样笔尖、纹理、颗粒、颜色动态、canvas material、颜料、高位深度和 native formats，并验证 live tail 与 symmetry preview。
- `cargo test -p photocraft-gpu --test brush_snapshots -- --nocapture`：通过。覆盖 native snapshot、CMYK ICC 重分配、mask、bounds、undo 和 channel restriction。
- `cargo test -p photocraft-gpu --lib input_sampling_keeps_gpu_tiles_unmaterialized`：通过。覆盖 RGBA8、CMYKA8、LabF32、负 tile 坐标和未物化 tile。
- `cargo test -p photocraft-gpu --lib shared_device_fault_stops_stroke_work_without_cpu_fallback`：通过。
- `cargo test -p photocraft-gpu --lib paged_transport_matches_whole_dab_across_tiles`：通过，覆盖 RGBA、RGB no-alpha、CMYK 和 V1/V2/V3。
- `cargo test -p photocraft-raster --lib backend_`：通过，验证 lazy GPU tile、恢复和读回错误传播。
- `cargo test -p photocraft-paint --lib symmetry_`：通过，验证独立 transport tail 和 union 错误传播。
- `cargo test -p photocraft-ops --lib video_memory`：通过，验证 GPU storage accounting 不破坏历史预算。

### 工具链验证

- strict clippy：通过，覆盖 GPU、paint、raster、format、io、engine、ui-egui。
- wasm：`cargo check -p photocraft-ui-egui --target wasm32-unknown-unknown` 通过；仅保留既有 engine dead-code warning。
- layers：`cargo xtask layers` 通过，31 个 crate 无依赖层违规。
- native release：`cargo build --release --locked --target aarch64-apple-darwin -p photocraft --features heif` 通过。

### 实际 macOS 运行

使用当前 release binary 和正常用户偏好启动控制通道，创建 6000×4000 文档，分别绘制 10 条笔迹：

| 场景 | GPU | fallback | canvas | 状态错误 |
|---|---:|---|---|---:|
| V1 | true | null | gpu-rect | 否 |
| V2 | true | null | gpu-rect | 否 |
| V3 | true | null | gpu-rect | 否 |
| Photoshop Single/Stroke | true | null | gpu-rect | 否 |
| Watercolor Single/Stroke | true | null | gpu-rect | 否 |
| Watercolor 2 Single/Stroke | true | null | gpu-rect | 否 |
| Pigment | true | null | gpu-rect | 否 |

运行设备报告为 Apple M5 / Metal，`canvas: gpu`、`fallback: null`、`lost: null`。实际运行同时完成了 undo、redo、`.pcraft` 保存和截图。

产物：

- [PhotoCraft.app](/Users/ddd/Documents/PS_photocraft/dist/gpu-brush-20261020/PhotoCraft.app)
- [GPU smoke results](/Users/ddd/Documents/PS_photocraft/dist/gpu-brush-20261020/smoke-results.json)
- [GPU smoke screenshot](/Users/ddd/Documents/PS_photocraft/dist/gpu-brush-20261020/gpu-brush.png)
- [Saved document](/Users/ddd/Documents/PS_photocraft/dist/gpu-brush-20261020/gpu-brush.pcraft)

## 当前边界

- raster 仍保留旧的无错误返回 `Deref/DerefMut` 和部分 `Surface` 像素 API；GPU 读回失败在这些旧接口中无法表达，新的导出、保存、过滤、取色和 GPU 笔迹热路径已走可失败边界。
- 可选 layer effects 的 shape/distance field 仍是已有 CPU 路径。
- 部分非笔迹 retouch/composite CPU consumer 仍使用旧的数值 oracle；它们不会改变混色、湿边和颜料笔迹的 GPU 路径。
- 独立的 Remove、Patch、Content-Aware Move 和 Pattern Stamp 子模块仍需继续核对旧 CPU 像素入口。
- 当前 smoke 结果证明 GPU 选择和路径接通，未作为不同机器之间的 FPS 基线比较。

