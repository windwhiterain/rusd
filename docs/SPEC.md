# Rusd 规范（v0 草案）

状态：草案 · 2026-11-16 · 发起会话 `Rusd`
本文是 Rusd 方言与系统的单一事实来源。实现与本文冲突时，先改本文（或明确记录偏差），再改代码。

## 0. 定位与原则

Rusd = 面向 Agentic PCG 的 USD 方言 + 配套渲染器，纯 Rust 实现。

四个第一公民：

1. **可编辑性**：agent 用最小 diff 精确编辑。每个 prim 一个区块，绝对路径寻址，规范化排序。
2. **确定性**：同 seed + 同输入 ⇒ 同场景 ⇒ 同像素。RNG 流按路径键控，编辑别处不 reshuffle。
3. **可反馈**：校验诊断、渲染结果（含 prim-id 分割 pass）全部机器可读，支撑自我纠正循环。
4. **过程化第一公民**：generator 是 schema，展开是解释器行为；文件里永远存参数，不存烘焙结果。

贯穿性需求：**增量**（§6）。快速迭代是 agentic 编辑的生命线，管线每一层都必须可增量。

**后端决策（D3）**：场景/合成/USD 互操作构建在 **openusd**（纯 Rust USD 库）之上，
Rusd 专注 agent 优先的表层语法、过程化展开、增量前端与渲染器。依据见 §6/§9 的源码与实证。

**非目标（v0）**：自建 USD 合成引擎、自建 USDA/USDC 解析、C++ USD 依赖、DCC 插件、
时间采样/动画的 agent 表面（后端支持，表面暂不暴露）。

## 1. 数据模型（对齐 USD）

概念沿用 USD：Stage、Prim、Property、Path、Layer、Opinion、Schema、Reference、Variant。
合成后的 Stage 由 openusd 提供（`openusd::usd::Stage`），Rusd 在其上做适配与扩展。

- **Prim 路径**：绝对路径，如 `/World/Forest/i000042`。v0 组件语法 `[A-Za-z_][A-Za-z0-9_]*`，
  比 USD 严格，换取可预测的 diff 与稳定的哈希键；导出 USDA 时按需转义。
- **属性寻址**：内部用 `<prim>.<property>`（如 `/World/Forest.seed`）。USD 的 `primvars:` 等
  命名空间在导出层映射，不进表面语法。
- **属性**：有类型、默认值、可选范围与单位；schema 注册表是校验和文档的唯一来源。
- **Prim 元数据**：`kind`、`active`、`instanceable`、provenance（author / turn / intent）。

## 2. 表面语法：`.rusd` = TOML 1.0 的 Rusd profile

**决策 D1（2026-11-16，采纳用户建议）**：表面语法直接采用 TOML 1.0 的受限 profile，不自造语法。

理由：

- 扁平 table header `[World.Forest]` 与"一 prim 一区块"天然同构；区块相互独立，
  使区块级增量解析（§6）几乎免费——嵌套语法做不到这一点。
- LLM 对 TOML 极其熟悉，生成/编辑错误率低。
- 现成 CST 解析器 `toml_edit`：保留注释与空白，支持原位修改，agent patch 无格式化噪声。
- TOML 的整数/浮点区分（`1` vs `1.0`）、`inf`/`nan`、`#` 注释、多行数组与规范形几乎重合。

映射规则：

- `[stage]` 为保留 table：`up_axis`、`meters_per_unit`、`default_prim`、`seed`。
- 其余每个 table = 一个 prim：`[World.Forest]` ↔ `/World/Forest`（点号 ↔ 斜杠）。
  组件必须匹配 §1 语法；TOML bare key 允许的 `-` 在 profile 里禁止（`E_NAME`）。
- 每个显式 prim table 必须有 `schema = "RusdScatter"` 键（`E_MISSING_SCHEMA`）。
  header 隐含的祖先 prim 自动生成为 `Xform`；`rusd fmt` 会把它们显式写出。
- 属性 = table 内键值。dotted key（`budget.max_prims`）作为命名空间属性名整体保留，
  不解释为子 prim。
- **字面量**：TOML 原生。`token` / `path` / `asset` 表面都是字符串，由 schema 声明的类型
  coerce（§3）；`vecN` = 定长浮点数组；**分布 = inline table**：
  `scale = { dist = "uniform", min = 0.8, max = 1.4 }`，参数由 schema 校验。
- **profile 禁止**：`[[array of tables]]`（多重性交给 generator，`E_AOT`）、
  datetime（`E_DATETIME`）、`[stage]` 之外的顶层裸键（`E_TOPLEVEL`）。
- **规范形（`rusd fmt`）**：table 按路径字节序排序（`[stage]` 恒首）；键按名字排序；
  数字去下划线、非十进制归一为十进制；浮点恒带小数点；字符串归一为 basic string 最小转义；
  注释随 item 保留（toml_edit CST）。
- 文件 UTF-8、LF（`.gitattributes` 强制）。
- 示例见 `examples/forest.rusd`。

## 3. 类型与 Schema

- 标量：`bool, i64, u64, f64, string, token, path, asset`；
  向量：`vec2/vec3/vec4`（f64）、`color3f`、`matrix4d`；数组：`T[]`；
  结构化值：`dict`（分布、预算等，表面为 inline table）。
- 表面字符串 → `token/path/asset` 的 coerce 由 schema 属性类型驱动；coerce 失败给 `E_TYPE`。
- 内建 schema（首批）：`Xform, Cube, Sphere, Cylinder, Mesh, Camera, Material,
  DirectionalLight, PointLight`。几何/材质/灯光尽量映射到 openusd 的 UsdGeom/UsdShade/UsdLux
  schema，减少自维护表面。
- Generator schema（`Rusd` 命名空间，Rusd 独有、openusd 没有的过程化扩展）。**首批做城市（D4）**：
  `RusdStreetGrid, RusdBuilding, RusdBlockFill` + 通用 `RusdScatter`；后续 `RusdLSystem,
  RusdTerrain, RusdWfc`。参数与组合详见 `docs/GENERATORS.md`。
- 未知 schema：警告 `W_UNKNOWN_SCHEMA`，按无类型 prim 保留（前向兼容，同 USD 精神）。
- **诊断**（JSON，code 稳定、一旦发布不复用）：

```json
{"code":"E_TYPE","severity":"error","prim":"/World/Forest","property":"count",
 "message":"expected u64, got f64","hint":"count 是整数；写 500 而不是 500.0"}
```

## 4. 合成：openusd 提供完整 LIVERPS，Rusd 约定 agent 作者子集

- **后端**：openusd 实现完整 LIVERPS（local / inherits / variants / relocates / references /
  payloads / specializes），通过 AOUSD Core Spec 1.0 合规基线（138 资产合成套件）。
  Rusd 不再自建合成引擎，也不自造简化的强度表——直接用合规的 USD 顺序。
- **agent 作者子集（v0 表面暴露）**：sublayers、references、variant sets、payloads。
  inherits/specializes/relocates 后端可用，但 v0 表面语法暂不暴露（遇到给 `W_UNSUPPORTED_ARC`）。
- **Session overlay**：agent 编辑写入 openusd 的 session layer（`Stage::edit_target_session()`），
  带 provenance；base 层对 agent 只读。undo = 弹层；审计 = 读层。非破坏、可撤销、可追溯。
  openusd 的 `StageSink` 编辑事件 + 可回放 `Diff` 直接支撑 provenance/diff/undo。

## 5. 过程化语义：PCG = 合成图里的 layer-op（核心差异，Rusd 独有）

**决策 D5（2026-11-16，采纳用户建议，Solaris/LOP 模型）**：generator 不是展开成子 prim 的特殊 prim，
而是**嵌在 USD 合成图里的 layer-op 节点**：读只读输入 layer，产出新输出 layer，经标准 USD arc
（sublayer/reference）接入合成。**展开即合成**，没有 bespoke 展开 pass。详见 `docs/PIPELINE.md`。
首批域 = 城市（D4），suite 见 `docs/GENERATORS.md`。

- Generator = **纯函数** `G(params, seed, input_layers) → output_layer`。禁止隐式全局状态/时钟/外部随机源。
- **输出 layer**：`sdf::Layer::new_anonymous` 作者的内存 layer，经 `Stage::insert_layer` 按强度接入合成图。
- **触发**：由 openusd 之上的 cook 引擎按依赖序求值（D6），非合成回调；详见 `docs/PIPELINE.md` §9。
- **RNG**：ChaCha8；流键 = hash(stage.seed, prim_path, local_seed)。路径键控 ⇒ 改一个节点不 reshuffle 其它。
- **增量**：节点输出按内容哈希 memo（键 = generator id+version, params, seed, 输入 layer 哈希）；
  输入/参数变才重跑节点，openusd pcp 定向重合成（§6）。
- **覆盖**：agent 编辑 = 在图某点插入 edit-layer（session overlay）；可 pin 生成实例，身份稳定则保留。
- **预算**：max_prims/max_triangles；超预算确定性截断 + W_BUDGET。
- **递归**：prototype 节点可实例化（每实例读自己的输入上下文）；环检测 + 深度上限。
- `.rusd` 只存节点参数；输出 layer 是缓存产物，导出 USD 时才 bake。
- **实证**（cargo run -p spike-openusd --bin layer_graph）：generator 产匿名 layer → insert_layer →
  合成值反映输出（radius 1→5）；重跑+mute 旧 layer 增量重合成 ~389µs，indexed_count 不变。

## 6. 增量管线（决策 D2，采纳用户建议）

快速迭代是 agentic 编辑的生命线。管线：

```
parse (CST) → compose → validate → expand → render
```

每层 memoize + revision 戳，失效沿依赖图传播，绝不全局重来。

- **合成层增量由 openusd 提供（D3，已实证）**：openusd 的 pcp 模块镜像 C++ `PcpChanges`：
  作者操作返回 `sdf::ChangeList`，`Changes::did_change` 三级分类（Significant / Prim / Spec），
  `pcp::Dependencies` 维护 layer-site → prim-index 的反向依赖索引（子树扇出、mute/unmute、
  表达式变量按名定向 resync）。值编辑走 Spec 级"原地重扫、保留 index"。
  - **实证**（`cargo run -p spike-openusd`，2000 prim）：冷合成 151ms；单属性值编辑后
    `indexed_count` 2001→2001（缓存零失效），编辑+重查询 **320µs**，约 **472×** 于冷合成；
    结构性删除只掉被删 prim（2001→2000）。远优于 <5ms 目标。
- **Rusd 自建的增量层**：
  - **区块级增量解析**：扁平 TOML table 相互独立。文件变更 → 按 header 切块 → 块文本哈希 →
    只重解析变化的块（toml_edit），未变块复用 CST 与语义节点。
  - **TOML → Stage 下沉**：只把变化的 prim 区块下沉为 openusd 作者操作，触发其定向失效。
  - **generator 节点 memo**：输出 layer 键 = (generator id+version, params, seed, 输入 layer 哈希)；只重跑变化节点（§5）。
  - **GPU 资源缓存**：网格/实例内容哈希键，只重建受影响资源。
- **两个入口，同一管线**：agent patch 用 toml_edit 原位改 CST（保注释/空白）后最小化序列化；
  人类编辑器改文件走 watch。两者汇入同一增量管线。
- **诊断也增量**：只重校验受影响 prim，输出按 (path, code) 稳定排序。
- **正确性不变量**：增量结果 ≡ 冷重建结果——场景与诊断逐字节一致，渲染像素一致
  （modulo §7 的 GPU 容差）。合成层的该不变量由 openusd 合规测试背书；Rusd 各层用
  随机编辑序列做增量 vs 全量对照（property test）。
- **延迟目标**：10k prim 场景单属性编辑，重解析+下沉+重校验 < 5ms；热重载端到端 < 100ms（不含渲染）。

## 7. 渲染器（agent 的眼睛和手，Rusd 独有）

- **wgpu** 后端（本机 DX12；Linux Vulkan / macOS Metal 可移植），自研轻量 forward PBR（GGX），
  GPU instancing，内容哈希网格缓存。渲染消费 openusd 合成后的 Stage（经 Rusd 适配层提取几何）。
- **双模式**：headless 离屏（agent）与交互 viewport（winit + egui，人），同一渲染核心。
- **Passes**：beauty、prim-id（每 prim 平涂稳定 id ⇒ 分割 / 拾取）、depth、normal、wireframe。
- **容错渲染**：非法 / 缺失资源 → 品红占位几何 + 诊断叠加；渲染调用永远返回
  （image + diagnostics），不 panic 打断 agent 循环。
- **确定性截图**：同 scene + camera + seed ⇒ 同像素；GPU / 驱动间浮点容差记录在 docs。
- **热重载**：走 §6 增量管线，只重建受影响的 GPU 资源。
- `pick(x, y)`：读 prim-id pass ⇒ prim path。agent 用"指一个像素"来选中对象。
- **视觉选择闭环**：`pick --visual` 把 agent 指的点画准星 + 描边被指中 prim；`render --select path=color:mode` 多选多色换色/描边。描边**只向该 prim 自身像素内部扩张**，不同 id 互不覆盖。
- **全参数可配置**：`RenderConfig`（尺寸/背景/fov/光照方向/ambient/diffuse）+ `annotate::Style`（描边宽/准星半径/换色保明度），CLI flag 暴露，默认复现参考观感。
- 全保真 USD 渲染（Hydra）列为远期备选（rust-usd 的 hydra-rs）；v0 不做。

## 8. Agent 工具协议

- 工具集（JSON 进 / 出）：`open, query, patch, validate, expand, render, pick, diff, undo,
  export_usd`。
- `patch` = opinion 操作列表（set / add / remove prim / property），可带 expected 前置哈希
  ⇒ 并发 / 过期编辑冲突可检测。下沉为 openusd session-layer 作者操作。
- `diff` / `undo` 复用 openusd 的可回放 Diff 与 undo/replay 记录器。
- **agent 绑定（D10）**：对外提供一套 Python 操作脚本（subprocess 驱动 `rusd` CLI / `rusd serve` JSON-RPC），不做 MCP adapter；脚本与 CLI 输出同为机器可读 JSON。
- `render` 返回 image + metadata（可见 prim 列表、screen bbox、诊断、耗时）。
- 传输：`rusd serve`（stdio JSON-RPC）先行；MCP adapter 随后，让任意 MCP agent 直接编辑场景。
- 编辑循环：`query → patch → validate → expand → render →（看 id pass）→ 再 patch`。

## 9. USD 互操作与先行技术（2026-11-16 联网核查 + 源码实证）

纯 Rust 的 USD 实现已经存在，互操作不必从零造：

- **openusd**（LunCoSim，MIT，crates.io 0.8，活跃开发、1.0 前不保证 API 稳定，MSRV 1.96）：
  读写 usda/usdc/usdz；完整 LIVRPS 合成；通过 AOUSD Core Spec 1.0 合规基线
  （文本/二进制解析、138 资产合成套件）；EditTarget 分层编辑、session layer、可回放 Diff、
  undo/replay、StageSink 事件、细粒度缓存失效（§6 实证）；schema readers（geom/lux/shade/...，0.8 内置）。
- **rust-usd + hydra-rs**：USD 核心 + Hydra 渲染管线（sidecar layer 编辑）。
- **polysmelt**：纯 Rust 资产互换（glTF 为枢纽，含 USD 读写），2026-11 仍在更新。

**决策 D3（O6 已决）**：依赖 openusd 0.8 作为场景/合成/互操作后端。
- `rusd-scene` = openusd Stage 之上的薄适配层：TOML→Stage 下沉、schema 注册表、
  诊断映射（openusd `CompositionDiagnostic` → Rusd JSON）、session overlay/provenance。
- `rusd-usd` 几乎免费：openusd 原生读写 usda/usdc/usdz，导出即可被 usdview/Blender/Omniverse
  打开（O2 互操作硬约束因此基本满足，代价极低）。generator 导出为自定义 schema prim 或 `--bake`。
- 渲染器仍自研 wgpu（agent 优先：prim-id pass、容错、确定性截图）；hydra-rs 列为全保真远期备选。
- 风险与对策：openusd 1.0 前 API 不稳定 ⇒ 适配层隔离，pin 精确版本（0.8.x），升级走 spike 回归。

## 10. Crate 布局

| crate | 职责 | 里程碑 |
| --- | --- | --- |
| `rusd-core` | 路径、类型化值、TOML profile 解析（toml_edit CST）、规范 writer、诊断 | M0–M1 |
| `rusd-scene` | openusd Stage 薄适配层：TOML→Stage 下沉、schema 注册表、诊断映射、overlay/provenance | M2 |
| `rusd-proc` | generator 运行时、路径键控 RNG、展开缓存、预算 | M3 |
| `rusd-render` | wgpu 渲染核心、passes、headless + viewport | M4 |
| `rusd-cli` | `rusd` 二进制：fmt / validate / expand / render / serve / export | M1 起 |
| `rusd-usd` | Rusd ↔ openusd 导出/导入桥（格式读写由 openusd 承担） | M6 |
| `rusd-agent` | JSON-RPC 工具协议 + Python 操作脚本套件（无 MCP，D10） | M5 |
| `spike-openusd` | O6 实证 spike（增量合成基准），publish=false，默认不构建 | 证据 |

## 11. 决策记录与开放问题

已决：

- **D1**（2026-11-16）：表面语法 = TOML 1.0 的 Rusd profile（§2）。原 O1（嵌套语法糖）关闭：不做。
- **D2**（2026-11-16）：增量管线为一等需求（§6），含"增量 ≡ 全量"不变量与延迟目标。
- **D3**（2026-11-16）：依赖 openusd 0.8 作为场景/合成/互操作后端（§6/§9，源码 + 实证）。
  原 O6 关闭。workspace MSRV 提升至 1.96。
- **D4**（2026-11-16）：首批 generator 域 = 城市（§3/§5，docs/GENERATORS.md）。原 O3 关闭。
- **D5**（2026-11-16）：PCG = 合成图里的 layer-op（Solaris 模型，§5，docs/PIPELINE.md，已实证）。
- **D6**（2026-11-16）：generator 触发 = openusd 之上的 cook 引擎（图求值 + memo），非合成回调、不改 composer（docs/PIPELINE.md §9，cook_graph 实证）。
- **D7**（2026-11-16）：默认坐标系 Z-up 右手系；USD 默认 Y-up，导出时转换。原 O4 关闭。
- **D8**（2026-11-16）：cook 图 = 路径依赖隐式 DAG（read∩write 诱导），cook 包裹 USD 合成；节点输出 layer 非 stage（docs/PIPELINE.md §10，implicit_cook 实证）。
- **D9**（2026-11-16）：cook 只包过程化生成，不包 USD 合成操作；compose op 全归 openusd pcp（docs/PIPELINE.md §10）。
- **D10**（2026-11-16）：agent 操作接口 = 一套 Python 脚本（subprocess 驱动 rusd CLI / JSON-RPC），不做 MCP adapter。原 O5 关闭。
- **D11**（2026-11-16）：`patch` 编辑 TOML 文档本身（toml_edit CST opinion ops），非 session-layer——cook 从文档读 generator 节点，文档是唯一事实来源（可编辑性）。解决 §6/§8 措辞张力，取 §6；M2 Overlay 保留为独立非破坏运行时机制。undo/redo = 文档快照回放。
- **D12**（2026-11-17）：**viewer 是 client，rusd 是 server；remote 切在 USD 层，不传像素**（docs/REMOTE.md）。
  背景：server 机器有 GPU 而仓库/GPU 在 peer，SSH 无显示；传帧方案因带宽/延迟被否。同步对象 =
  USD layer / 快照；client 本地合成 + 本地 wgpu 渲染 + 本地 pick/annotate ⇒ orbit/zoom/点击零网络往返。
  viewer 是人类 + agent 协同开发的 interface，共享物是场景。浏览器 client 出局（wasm 合成 USD 太重），
  原生 Rust 二进制（peer 构建 scp 到用户机，同 win-x64）。**分层两条**：Tier A = change 时推 flattened
  usdc 快照（city 58KB；release export 0.159s / expand 0.107s），client `Layer::from_bytes` 单层栈
  → extract → 本地渲染（flatten 烘资产/丢 arcs ⇒ 匿名安全）；Tier B = 命名 identifier 的 layer stack
  逐层推送（usda 文本 / usdc 字节），client 自定义 `ar::Resolver` + `Stage::builder().resolver()
  .session_layer().open()` 逐 opinion 镜像；layer 变更 = **重建 stage**（crate 无 reload/rebind，
  session 仅 builder 期），`Diff`/`apply_diff` 留作细粒度增量后置（`sdf::Value`/`Diff` 无 serde，
  需自定义 codec）。协议 = TCP NDJSON JSON-RPC，复用 rusd-agent 工具名 + `subscribe` + 通知
  `stage_snapshot`/`layers_changed`/`diagnostics`/`state_changed`；默认 loopback + SSH 隧道（`ssh -L`），
  `--bind LAN` 后置；相机归 client（每次 render 带全 `RenderConfig`），server 对相机无状态。
  线程：`Stage` = `Rc` ⇒ `!Send`，sync 线程独占 stage，owned `GeoItems` 经 channel 交渲染线程。
  确定性：openusd 0.8 双端 pin、同 win-x64；只序列化命名 identifier（匿名 id 是进程计数器）；
  resolver identity 双端恒等；wire 用 usda/usdc 不用 usdz（zip 时间戳风险）。
  取代原 M4 的"进程内 winit + egui 热重载 viewer"，M4 重排为 M4'（见 ROADMAP）。

开放：

- **O2** 互操作硬约束——D3 后基本满足（openusd 原生 usdc/usdz 导出）；待确认目标 DCC 清单。