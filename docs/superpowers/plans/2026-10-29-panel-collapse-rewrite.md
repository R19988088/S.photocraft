# 右侧小窗口折叠与缩放重写实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 `superpowers:subagent-driven-development` 或 `superpowers:executing-plans` 逐任务实现此计划。步骤使用复选框跟踪进度。

**目标：** 重写右侧小窗口的几何布局与双击折叠逻辑，使折叠后窗口向上收缩、下方内容上移，不产生预留空白或错误最小高度；再次双击在原位置恢复展开高度。

**架构：** 把“窗口外壳高度”和“组树内部高度”分成两层独立的几何状态。折叠组只通过当前帧的布局计算得到标签栏高度，不写入持久化 split 尺寸；折叠动作以组的顶部为锚点，压缩自身并把后续组整体上移。双击命中区只负责发出一次明确的 collapse/expand 命令，渲染层负责根据状态计算矩形。

**技术栈：** Rust、egui、craft-ui docking、egui_kittest、现有 `Layout<Node>`/`SplitSize` 和 UI 状态持久化。

---

## 文件范围与职责

- 修改：`crates/ui-egui/src/panel_docking.rs`
  - 重写自定义停靠树的折叠布局计算、浮动小窗口的高度约束和底部缩放锚点。
  - 保留现有面板命令、拖拽、浮动和持久化协议，不新增用户可见配置。
- 修改：`crates/ui-egui/src/dock.rs`
  - 仅在共享的 legacy dock 仍复用同一双击辅助函数时保留其语义；不再让 legacy 高度算法影响自定义停靠树。
- 修改：`crates/ui-egui/src/panel_docking_tests.rs`
  - 增加几何断言，覆盖折叠前后顶部、底部和相邻组位置。
  - 修正测试按面板组身份查找响应，避免用第一个空白响应误判目标组。
- 生成：`code/dist/PhotoCraft.app`
  - 最终 arm64、未压缩 Mac App；不覆盖已有备份 App。

## 任务 0：以最近源码备份核定重写基线

**文件：**

- 参考：`/Users/ddd/Documents/PS_photocraft/backup/PhotoCraft-source-20261028-230157/code/crates/ui-egui/src/panel_docking.rs`
- 参考：`/Users/ddd/Documents/PS_photocraft/backup/PhotoCraft-source-20261028-230157/code/crates/ui-egui/src/dock.rs`
- 修改：当前仓库同名文件

- [ ] **步骤 1：对照备份与当前实现。**

  只检查右侧 dock 的折叠、split 计算、浮动窗口 resize 和对应测试。备份作为实现基线参考；逐段确认哪些逻辑是既有行为，哪些是此前的错误补丁，不整文件覆盖。

- [ ] **步骤 2：保留无关本地修改。**

  修改范围限于上述功能和测试，不回退当前工作区其他文件，不重置仓库，不删除现有 `.app` 备份。

## 任务 1：建立当前几何行为的失败回归测试

**文件：** `crates/ui-egui/src/panel_docking_tests.rs`

- [ ] **步骤 1：记录折叠前每个组的矩形。**

  在现有自定义停靠 harness 中读取每个目标组的 tab strip 和 group rect，使用 group identity 过滤 `layers/channels/paths`，不要按响应遍历顺序取第一个结果。

- [ ] **步骤 2：添加折叠方向断言。**

  双击目标组空白标题区后，断言目标组高度等于标签栏高度，目标组顶部不变，目标组底部减少，目标组下面第一个可见组的顶部向上移动，且没有额外空档。

- [ ] **步骤 3：添加展开恢复断言。**

  在同一坐标区域再次双击，断言目标组恢复到折叠前的有效高度，后续组回到折叠前的相对顺序；断言持久化 `Layout` 的原始 split 尺寸没有被渲染帧改写。

- [ ] **步骤 4：添加浮动小窗口缩放断言。**

  浮动整个右侧窗口后，从底部缩放把高度减少 180，再增加 90；断言顶部保持不变、底部按拖动量变化，折叠组不会把最小高度强行扩大。

- [ ] **步骤 5：运行失败测试。**

  运行：

  ```bash
  cargo test -p photocraft-ui-egui \
    panel_docking::tests::double_clicking_blank_custom_group_moves_following_groups_up --lib
  cargo test -p photocraft-ui-egui \
    panel_docking::tests::floating_dock_resize_keeps_top_anchor --lib
  ```

  预期：当前实现至少在“后续组上移”或“原始 split 未被改写”的断言处失败，证明测试捕获的是用户报告的问题。

## 任务 2：重写自定义停靠树的折叠几何

**文件：** `crates/ui-egui/src/panel_docking.rs`

- [ ] **步骤 1：删除渲染前修改布局副本 split 尺寸的补丁。**

  删除 `compact_collapsed_splits` 及其调用。渲染帧不得把某个折叠组直接改成 `FixedFirst(tab_height)`/`FixedSecond(tab_height)`，因为嵌套 split 会把这个尺寸传播成错误的剩余空间。

- [ ] **步骤 2：实现只计算不持久化的树几何函数。**

  新函数接收 `&Layout<String>`、折叠面板集合和 `tab_height`，递归返回每个 `Node` 的最小/首选高度。`Node::Tabs` 全部折叠时返回 `tab_height`；展开组返回内容最小高度；`Node::Split` 按轴合并子树高度，并把剩余空间分配给展开侧。

- [ ] **步骤 3：用计算结果生成当帧 Layout。**

  对垂直 split 只在渲染副本中设置本帧所需的 `FixedFirst` 或 `FixedSecond`，并保证同一 split 两侧至少有一个展开侧时，折叠侧尺寸精确等于一个 tab strip。连续嵌套折叠时从叶节点向上计算，避免父 split 再次保留旧高度。

- [ ] **步骤 4：修正 PanelLimits。**

  `craft-ui` 的 `node_limits` 会在 `Node::Tabs` 限值上额外加一次 `style.tab_height`，因此这里传入的是 body 高度：折叠组传 `min.y = max.y = 0.0`，由 renderer 产生唯一的标题栏高度；展开组不设置人为的固定最小高度，只保留内容和窗口实际需要的横向最小宽度。不得用“展开最小高度”给折叠状态预留空间。

- [ ] **步骤 5：验证双击命令只切换状态一次。**

  保留现有 release 配对 helper，但将 collapse/expand 命令目标绑定到命中响应对应的 group identity；同一帧不得因 tab response、blank response 和菜单 response 重复提交命令。

## 任务 3：修正浮动小窗口的缩放锚点

**文件：** `crates/ui-egui/src/panel_docking.rs`

- [ ] **步骤 1：明确浮动窗口的几何约定。**

  `rect.min` 是顶部锚点，底部拖拽只修改 `rect.max.y`；折叠内部组不得修改浮动窗口的 `rect.min.y` 或向下制造空白。

- [ ] **步骤 2：重写底部 resize 计算。**

  在 `DockSurface` 临时状态中保存 resize 开始时的 `height` 和指针起点，以 `start_height + (current_pointer_y - start_pointer_y)` 计算新高度，先按窗口内容区上限和实际最小高度 clamp，再写回 `rect.max.y = rect.min.y + height`。不要在每帧重复叠加已经应用过的 `drag_delta`。

- [ ] **步骤 3：保持折叠上移语义。**

  折叠组减少的是该组在布局树中的高度，后续组的 `top` 变小；浮动窗口自身顶部不变。展开时只恢复该组保存的有效展开尺寸。

## 任务 4：补齐测试和视觉证据

**文件：** `crates/ui-egui/src/panel_docking_tests.rs`

- [ ] **步骤 1：运行聚焦测试。**

  ```bash
  cargo test -p photocraft-ui-egui \
    panel_docking::tests::double_clicking_blank_custom_group_moves_following_groups_up --lib
  cargo test -p photocraft-ui-egui \
    panel_docking::tests::floating_dock_resize_keeps_top_anchor --lib
  cargo test -p photocraft-ui-egui \
    dock::tests::double_clicking_a_tab_collapses_and_dragging_a_strip_reorders --lib
  ```

- [ ] **步骤 2：生成自定义停靠视觉 fixture。**

  使用已有 `capture_custom_panel_docking_visual_fixtures`，至少覆盖 ProMedium、1280×800、1× 和 2×；比较折叠前后目标组底部、后续组顶部以及窗口底部，不接受中间出现固定空白带。

- [ ] **步骤 3：检查持久化与构建。**

  ```bash
  cargo test -p photocraft-ui-egui panel_docking --lib
  cargo build --release --locked -p photocraft --features heif --target aarch64-apple-darwin
  codesign --verify --strict --deep --verbose=1 dist/PhotoCraft.app
  lipo -info dist/PhotoCraft.app/Contents/MacOS/PhotoCraft
  ```

  只有聚焦交互测试、几何断言、视觉 fixture 和签名检查都通过后，才交付 App。

## 任务 5：交付检查

- [ ] 保留现有 `dist/PhotoCraft-before-*.app` 备份，不删除用户已有脏改动和无关源码。
- [ ] 确认 `dist/PhotoCraft.app` 是未压缩目录形式、arm64、ad-hoc 签名、未公证。
- [ ] 确认本次 diff 仅涉及停靠布局、双击交互、对应测试和计划文档。
- [ ] 记录尚未完成的真实设备/手动截图验证，不以编译通过替代视觉交互验收。

