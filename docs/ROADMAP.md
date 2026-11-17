# Rusd 路线图

每个里程碑有可运行的验收标准；完成即打勾并记录日期。

## M0 — 脚手架 + 规范（2026-11-16，完成）

- [x] 仓库、workspace、crate 布局
- [x] docs/SPEC.md v0 草案、AGENTS.md
- [x] `rusd-core`：PrimPath / Value / Diagnostic + 单元测试
- [x] `cargo test --workspace` 全绿（2026-11-16）

## M1 — 语法核心（2026-11-16，完成）

表面语法 = TOML 1.0 的 Rusd profile（SPEC §2，决策 D1）；增量见 SPEC §6（D2）。

- [x] toml_edit 结构解析 spike：`rusd parse`，forest.rusd → prim 树（2026-11-16）
- [x] profile 规则完整化：显式 table 缺 `schema` → E_MISSING_SCHEMA（spike 暂默认 Xform）；
      命名 / 禁用构造诊断齐全
- [x] 规范 writer：排序、数字/字符串归一、保注释；`rusd fmt`
- [x] 区块级增量解析 + "增量 ≡ 全量" property test
- 验收：forest.rusd 往返无损；fmt 幂等；单块变更只重解析该块

## O6 实证 — openusd 增量合成（2026-11-16，完成 → 决策 D3）

- [x] 源码核查：pcp/mod.rs "Cache invalidation"（镜像 C++ PcpChanges，三级分类）+
      pcp/dependencies.rs（layer-site → prim-index 反向依赖索引）
- [x] 实证 spike（`cargo run -p spike-openusd`，2000 prim）：值编辑后 indexed_count
      2001→2001（零失效），编辑+重查询 320µs，~472× 于冷合成；结构删除只掉被删 prim
- [x] 结论：openusd 支持增量合成 ⇒ 依赖它（D3）。MSRV 提至 1.96，pin openusd 0.8

## M2 — 场景适配层（openusd 后端，D3，2026-11-16 完成）

**Step 1（2026-11-16，完成）**：schema 注册表 + 校验

- [x] schema 注册表：内建几何 + 城市 generator schema，属性类型 / required 约束
- [x] `rusd validate`：E_TYPE / E_REQUIRED / E_PATH / W_UNKNOWN_SCHEMA / W_UNKNOWN_PROP
      （forest/city 干净，rusd-scene 12 测试）

**Step 2（2026-11-16，完成）**：openusd Stage 适配 + cook 引擎通用化

- [x] `rusd-scene`：TOML prim 树 → openusd Stage 下沉（`in_memory` 匿名 root，Z-up 元数据）
- [x] 值 coerce：标量/向量/数组 → USD 类型；generator 参数 author 为 custom 属性
- [x] 诊断映射：`CompositionDiagnostic` → Rusd JSON（E_ARC_CYCLE / E_UNRESOLVED_LAYER / …）
- [x] `rusd query`：合成 prim + USD 类型名 + 属性值（forest/city 实测，adapter 4 测试）
- [x] cook 引擎通用化（rusd-proc：隐式路径依赖 DAG + 拓扑 + 内容哈希 memo + 下游脏传播 + insert/mute；6 测试）
- [x] session overlay（Overlay：会话层作者 + provenance 日志 + undo/redo；4 测试。StageSink/Diff 为富回放备选）
- [x] `rusd expand`（RusdGrid 端到端：grid.rusd → 27 合成 prim；城市 generator M3 注册）
- 验收：forest.rusd 下沉为合规 Stage；非法场景给可定位诊断；overlay 覆盖 base 可演示；
      单属性编辑走 openusd 定向失效（复用 spike 基准）

## M3 — 过程化核心（2026-11-16，核心完成）

- [x] ChaCha8 路径键控 RNG 流（rng.rs，4 测试：确定性 / 路径隔离 / uniform / normal）
- [x] 城市 suite（D4）：RusdStreetGrid（完整城市）/ RusdBuilding / RusdScatter；BlockFill 逻辑 v0 内联于 StreetGrid
- [x] 分区预设、分布采样、预算与确定性截断（W_BUDGET）
- [x] `rusd expand`：city.rusd → 475 合成 prim；确定性 / 预算 / 读依赖验收测试（13 rusd-proc 测试）
- [ ] 增量展开缓存细化（cook memo 已有）、`--dry-run`、实例覆盖
- [ ] 递归 cook 图实例化（每 parcel 独立 BlockFill 节点，P6 动态发现）
- 验收：同 seed 逐字节一致 ✓；6×6 城市在预算内 ✓；超预算确定性截断 + 诊断 ✓；RNG 路径键控不 reshuffle ✓

## M4 — 渲染 MVP（2026-11-16 启动，headless 优先）

- [x] wgpu device/headless 出图 spike（DX12 + RTX 3060 Ti，离屏渲染 + readback + PNG，2026-11-16）
- [x] 几何提取层 rusd-scene::geometry：Stage→网格/世界变换/相机（xformOpOrder 复合、内建细分、Mesh 三角化、容错占位）；city/forest 验收（2026-11-16）
- [x] wgpu headless：beauty + prim-id（R32Uint）pass、look-at 相机、确定性截图（2026-11-16）
- [x] 容错品红占位 + 确定性截图；诊断叠加待做
- [ ] 热重载交互 viewer（winit + egui），走 SPEC §6 增量管线
- [x] `rusd render` / `rusd pick`（beauty/ids PNG + metadata JSON；pick 像素→path，2026-11-16）
- [x] 视觉选择闭环：指点准星标注 + 选中换色/描边（多选多色；描边只向自身内部扩张、不互相覆盖；pick --visual / render --select，2026-11-16）
- [x] 渲染/标注全参数可配置（RenderConfig + Style：尺寸/背景/fov/光照/描边宽/准星半径/换色保明度；CLI flag，2026-11-16）
- [x] 验收：forest/city 出图；id pass 像素 → prim path 往返正确（2000/2000，确定性，2026-11-16）

## M5 — Agent 协议（2026-11-16 完成）

- [x] `rusd serve`：stdio JSON-RPC（NDJSON）有状态 session + open/query/validate/expand/render/pick（2026-11-16）
- [x] patch（编辑文档 CST，D11）+ diff/undo/redo（文档快照，2026-11-16）
- [x] Python 操作脚本套件（D10，替代 MCP）：rusd_client.py + example_build_forest.py，subprocess 驱动 rusd serve（2026-11-16）
- [x] 验收：example_build_forest.py 从最小场景经 patch 建出 501 几何森林，render/pick/undo/redo 全通（2026-11-16）

## M6 — 互操作与扩展

- [x] rusd-usd 导出桥：展开→flatten→usda/usdc/usdz（--no-bake 保留 generator schema）；rusd export / agent export_usd（2026-11-17）
- [x] 导出 round-trip：usda/usdc 经 openusd 重导入 prim 数一致（AOUSD 合规背书）（2026-11-17）
- [x] 独立验证链：参考实现 pxr（usd-core）读回 + Cycles 路径追踪出图；city 475 / forest 505 prim
      与 expand 精确一致、gprim 0 无效、stage 元数据合规；post-fix 复测 city bbox max
      `(444,444,52.07)`、forest `(50.48,50.62,1.40)`，两件均 PASS（2026-11-17）
      → 证据、复现命令、数字表、Cycles XML 约定与坑、产物位置见 `docs/VERIFICATION.md`；
      脚本 `scripts/verify/{verify_usd,make_review,usd_to_cycles}.py`
- [ ] USD 导入桥（USD→Rusd）
- [ ] RusdLSystem / RusdTerrain / RusdWfc（更多 generator）
- [ ] 示例画廊 + 文档
- [ ] 导出相机/灯光 rig 决策：导出件相机为单位矩阵、无灯光，DCC 直开黑/错构图；
      是否让 `rusd export` 产出可用相机 + 默认灯光待定（现象见 VERIFICATION §7.2）
- [ ] usdview / Blender 实际打开确认（子集范围内）——目前由 pxr + Cycles 覆盖
- 已知 backlog（验证中发现）：
  - [x] city 外圈建筑浮空 —— **已修 6189136**（`7dbe97a` 合并）。根因更正：不是 Ground 尺寸
    不足（Ground 444×444 = `6×60+7×12`，一直正确），而是 lot 落位把 block prim
    `xformOp:translate` 里已有的 `(cx,cy)` 又加了一遍，lot 被推出网格；改 block-local 后
    city bbox max 从 783 回到 444，回归测试 `ground_covers_the_whole_street_grid` 保证。
  - [ ] 行道树 scatter 域原点与网格不一致：`StreetProps` 的 rect 域以世界原点为中心采样，
    而网格/地面是角点原点 `0..444`，约 230/300 棵树落在 Ground 外（上一条 lot 修复之外的独立
    问题，未修）。
  - Cycles local build USD 直入模式空 film（外部工具缺陷，XML 模式绕过）。
  详见 `docs/VERIFICATION.md` §7。
