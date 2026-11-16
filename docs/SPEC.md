# Rusd 规范（v0 草案）

状态：草案 · 2026-11-16 · 发起会话 `Rusd`
本文是 Rusd 方言与系统的单一事实来源。实现与本文冲突时，先改本文（或明确记录偏差），再改代码。

## 0. 定位与原则

Rusd = 面向 Agentic PCG 的 USD 方言 + 配套渲染器，纯 Rust 实现。

四个第一公民：

1. **可编辑性**：agent 用最小 diff 精确编辑。每个 prim 一个块，绝对路径寻址，规范化排序。
2. **确定性**：同 seed + 同输入 ⇒ 同场景 ⇒ 同像素。RNG 流按路径键控，编辑别处不 reshuffle。
3. **可反馈**：校验诊断、渲染结果（含 prim-id 分割 pass）全部机器可读，支撑自我纠正循环。
4. **过程化第一公民**：generator 是 schema，展开是解释器行为；文件里永远存参数，不存烘焙结果。

**非目标（v0）**：完整 OpenUSD 兼容（LIVERPS 全集、时间采样/动画、Hydra、crate 二进制）、
C++ USD 依赖、DCC 插件。互操作走 USDA 导出/导入桥（§8）。

## 1. 数据模型（对齐 USD）

概念沿用 USD：Stage、Prim、Property、Path、Layer、Opinion、Schema、Reference、Variant。

- **Prim 路径**：绝对路径，如 `/World/Forest/i000042`。v0 组件语法 `[A-Za-z_][A-Za-z0-9_]*`，
  比 USD 严格，换取可预测的 diff 与稳定的哈希键；导出 USDA 时按需转义。
- **属性寻址**：v0 用 `<prim>.<property>`（如 `/World/Forest.seed`）。USD 的 `primvars:` 等
  命名空间在导出层映射，不进表面语法。
- **属性**：有类型、默认值、可选范围与单位；schema 注册表是校验和文档的唯一来源。
- **Prim 元数据**：`kind`、`active`、`instanceable`、provenance（author / turn / intent）。

## 2. 表面语法（`.rusd`）

- UTF-8，LF，`#` 行注释。
- **规范形式 = 扁平块**：每个 prim 一个 `def <Schema> <绝对路径> { ... }` 块；
  块按路径字节序排序，块内属性按名字排序。v0 没有嵌套语法——路径即结构。
  - 理由：一次编辑 = 一个块的最小 diff；块可独立增删；合并 overlay 不需要理解括号深度；
    LLM 不会犯缩进/括号配对错误。
- **字面量**：`true` / `42`（i64）/ `3.14`（f64，规范形式恒带小数点）/ `"string"` /
  `token` / `/Prim/Path` / `@asset.png@` / `[1.0, 2.0, 3.0]`（schema 定长时解释为 vecN）/ 数组。
- **分布字面量**：`uniform(a, b)`、`normal(mu, sigma)`、`rect(w, h)`、`disc(r)` 等，
  只能出现在 generator 的属性位置，类型由 schema 决定。
- **stage 头**：`stage { up_axis, meters_per_unit, default_prim, seed }`。
- `rusd fmt`：幂等、确定性；解析 → AST → 规范写出，语义无损、空白归一。
- 示例见 `examples/forest.rusd`。
- 开放问题 O1：是否提供 USDA 风格嵌套语法作为糖（默认不做）。

## 3. 类型与 Schema

- 标量：`bool, i64, u64, f64, string, token, path, asset`；
  向量：`vec2/vec3/vec4`（f64）、`color3f`、`matrix4d`；数组：`T[]`。
- 内建 schema（首批）：`Xform, Cube, Sphere, Cylinder, Mesh, Camera, Material,
  DirectionalLight, PointLight`。
- Generator schema（`Rusd` 命名空间，按里程碑引入）：`RusdScatter, RusdGrid, RusdInstance,
  RusdLSystem, RusdTerrain, RusdWfc`。
- 未知 schema：警告 `W_UNKNOWN_SCHEMA`，按无类型 prim 保留（前向兼容，同 USD 精神）。
- **诊断**（JSON，code 稳定、一旦发布不复用）：

```json
{"code":"E_TYPE","severity":"error","prim":"/World/Forest","property":"count",
 "message":"expected u64, got f64","hint":"count 是整数；写 500 而不是 500.0"}
```

## 4. 合成：LIVERPS 的文档化子集

- **v0 支持**：Layer stack（root + sublayers + session overlay）、References（内部路径 / 外部文件）、
  VariantSets、Payloads（延迟加载）。
- **v0 不支持**：Inherits、Specializes、关系属性、时间采样。遇到即诊断，不静默。
- **Opinion 强度**（强 → 弱）：session overlay → ���地层 → sublayers（按声明顺序，后者弱）。
  规则简单可预测，刻意不搬 USD 的 arc 强度表。
- **Agent 编辑只写 session overlay**，带 provenance；base 层对 agent 只读。
  undo = 弹层；审计 = 读层。非破坏、可撤销、可追溯。

## 5. 过程化语义（核心差异）

- Generator = **纯函数**：`(params, seed, inputs) → 子树`。禁止隐式全局状态、时钟、外部随机源。
- **RNG**：ChaCha8；流键 = `hash(stage.seed, prim_path, local_seed)`。
  路径键控 ⇒ 增删兄弟节点不会 reshuffle 既有实例。
- **展开**：惰性、增量、按内容哈希缓存。输出 prim 名确定：`i000000` 起（schema 可指定命名规则）。
- **覆盖**：生成实例可被 overlay 覆盖 / pin；身份稳定则重展开后覆盖仍生效。
- **预算**：`max_prims`、`max_triangles`；`rusd expand --dry-run` 返回成本估计而不展开。
- **递归**：prototype 内可含 generator；环检测 + 深度上限（默认 8，可配）。
- `.rusd` 永远存 generator 参数；烘焙只发生在导出 USDA 或渲染缓存里。

## 6. 渲染器（agent 的眼睛和手）

- **wgpu** 后端（本机 DX12；Linux Vulkan / macOS Metal 可移植），自研轻量 forward PBR（GGX），
  GPU instancing，内容哈希网格缓存。
- **双模式**：headless 离屏（agent）与交互 viewport（winit + egui，人），同一渲染核心。
- **Passes**：beauty、prim-id（每 prim 平涂稳定 id ⇒ 分割 / 拾取）、depth、normal、wireframe。
- **容错渲染**：非法 / 缺失资源 → 品红占位几何 + 诊断叠加；渲染调用永远返回
  （image + diagnostics），不 panic 打断 agent 循环。
- **确定性截图**：同 scene + camera + seed ⇒ 同像素；GPU / 驱动间浮点容差记录在 docs。
- **热重载**：监听 `.rusd` → 增量重合成 → 只重建受影响的 GPU 资源。
- `pick(x, y)`：读 prim-id pass ⇒ prim path。agent 用"指一个像素"来选中对象。

## 7. Agent 工具协议

- 工具集（JSON 进 / 出）：`open, query, patch, validate, expand, render, pick, diff, undo,
  export_usda`。
- `patch` = opinion 操作列表（set / add / remove prim / property），可带 expected 前置哈希
  ⇒ 并发 / 过期编辑冲突可检测。
- `render` 返回 image + metadata（可见 prim 列表、screen bbox、诊断、耗时）。
- 传输：`rusd serve`（stdio JSON-RPC）先行；MCP adapter 随后，让任意 MCP agent 直接编辑场景。
- 编辑循环：`query → patch → validate → expand → render →（看 id pass）→ 再 patch`。

## 8. USD 互操作

- `rusd-usd` crate：USDA 导出（generator 保留为自定义 schema prim，或 `--bake` 展开）；
  USDA 子集导入（映射到 Rusd schema，未识别的保留为通用 prim + 警告）。
- 纯 Rust，无 C++ 依赖。全保真 OpenUSD 桥（C API，vcpkg 构建；本机已有 vcpkg + CMake + MSVC）
  列为远期可选。

## 9. Crate 布局

| crate | 职责 | 里程碑 |
| --- | --- | --- |
| `rusd-core` | 路径、类型化值、AST、parser、规范 writer、诊断 | M0–M1 |
| `rusd-scene` | stage、layer stack、合成子集、schema 注册表、校验 | M2 |
| `rusd-proc` | generator 运行时、路径键控 RNG、展开缓存、预算 | M3 |
| `rusd-render` | wgpu 渲染核心、passes、headless + viewport | M4 |
| `rusd-cli` | `rusd` 二进制：fmt / validate / expand / render / serve / export | M1 起 |
| `rusd-usd` | USDA 导出 / 导入桥 | M6 |
| `rusd-agent` | JSON-RPC / MCP 协议适配 | M5 |

## 10. 开放问题

- **O1** 嵌套语法糖要不要？（默认：不要，扁平块即规范形式。）
- **O2** 互操作是否硬约束——必须能被 Blender / Omniverse 直接打开？
  （默认：不是；走导出桥，语法为 agent 优化。）
- **O3** 首批 generator 的内容域：自然（森林 / 地形）、城市 / 建筑，还是通用？
- **O4** 默认 `up_axis`：建议 Z-up 右手系（PCG 数学习惯）；USD 默认 Y-up，导出时转换。
- **O5** MCP adapter 的优先级（M5 内做，还是提前）。