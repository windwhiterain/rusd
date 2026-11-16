# Rusd 路线图

每个里程碑有可运行的验收标准；完成即打勾并记录日期。

## M0 — 脚手架 + 规范（2026-11-16，完成）

- [x] 仓库、workspace、crate 布局
- [x] docs/SPEC.md v0 草案、AGENTS.md
- [x] `rusd-core`：PrimPath / Value / Diagnostic + 单元测试
- [x] `cargo test --workspace` 全绿（2026-11-16）

## M1 — 语法核心（进行中）

表面语法 = TOML 1.0 的 Rusd profile（SPEC §2，决策 D1）；增量见 SPEC §6（D2）。

- [x] toml_edit 结构解析 spike：`rusd parse`，forest.rusd → prim 树（2026-11-16）
- [ ] profile 规则完整化：显式 table 缺 `schema` → E_MISSING_SCHEMA（spike 暂默认 Xform）；
      命名 / 禁用构造诊断齐全
- [ ] 规范 writer：排序、数字/字符串归一、保注释；`rusd fmt`
- [ ] 区块级增量解析 + "增量 ≡ 全量" property test
- 验收：forest.rusd 往返无损；fmt 幂等；单块变更只重解析该块

## O6 实证 — openusd 增量合成（2026-11-16，完成 → 决策 D3）

- [x] 源码核查：pcp/mod.rs "Cache invalidation"（镜像 C++ PcpChanges，三级分类）+
      pcp/dependencies.rs（layer-site → prim-index 反向依赖索引）
- [x] 实证 spike（`cargo run -p spike-openusd`，2000 prim）：值编辑后 indexed_count
      2001→2001（零失效），编辑+重查询 320µs，~472× 于冷合成；结构删除只掉被删 prim
- [x] 结论：openusd 支持增量合成 ⇒ 依赖它（D3）。MSRV 提至 1.96，pin openusd 0.8

## M2 — 场景适配层（openusd 后端，D3）

- [ ] `rusd-scene`：openusd Stage 薄适配层；TOML prim 树 → Stage 作者操作下沉
- [ ] schema 注册表（内建几何映射 UsdGeom/UsdShade/UsdLux + 属性约束 + 字符串 coerce）
- [ ] session overlay（edit_target_session）+ provenance（StageSink/Diff）
- [ ] 诊断映射：openusd CompositionDiagnostic → Rusd JSON（稳定 code）
- [ ] `rusd validate` / `rusd query`
- 验收：forest.rusd 下沉为合规 Stage；非法场景给可定位诊断；overlay 覆盖 base 可演示；
      单属性编辑走 openusd 定向失效（复用 spike 基准）

## M3 — 过程化核心

- ChaCha8 路径键控 RNG 流
- 城市 suite（D4，docs/GENERATORS.md）：RusdStreetGrid / RusdBuilding / RusdBlockFill + 通用 RusdScatter（写入 openusd Stage）
- 分区预设、预算与确定性截断、增量展开、内容哈希缓存、实例覆盖
- `rusd expand`（含 --dry-run）
- 验收：同 seed 逐字节一致；改街区 A 不 reshuffle 街区 B；6×6 城市在预算内；超预算确定性截断+诊断

## M4 — 渲染 MVP

- wgpu headless：从 openusd Stage 提取几何、内建几何、camera、beauty + prim-id pass
- 容错占位 + 诊断叠加；确定性截图
- 热重载交互 viewer（winit + egui），走 SPEC §6 增量管线
- `rusd render` / `rusd pick`
- 验收：forest.rusd 出图；id pass 像素 → prim path 往返正确

## M5 — Agent 协议

- `rusd serve`：stdio JSON-RPC 工具集（SPEC §8）；diff/undo 复用 openusd
- MCP adapter
- 验收：一个 agent 从一句 prompt 出发，经 query→patch→validate→render 循环建出森林场景

## M6 — 互操作与扩展

- rusd-usd：Rusd ↔ openusd 导出/导入桥（usda/usdc/usdz 由 openusd 承担，含 --bake）
- RusdLSystem / RusdTerrain / RusdWfc
- 示例画廊 + 文档
- 验收：导出的 USD 能被 usdview / Blender 打开（子集范围内）