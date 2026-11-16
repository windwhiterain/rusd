# Rusd 路线图

每个里程碑有可运行的验收标准；完成即打勾并记录日期。

## M0 — 脚手架 + 规范（2026-11-16，完成）

- [x] 仓库、workspace、crate 布局
- [x] docs/SPEC.md v0 草案、AGENTS.md
- [x] `rusd-core`：PrimPath / Value / Diagnostic + 单元测试
- [x] `cargo test --workspace` 全绿（2026-11-16）

## M1 — 语法核心

- lexer / parser / AST：stage 头、def 块、字面量、分布字面量
- 规范 writer：排序、浮点规范形、幂等
- `rusd fmt` / `rusd parse`
- 验收：examples/forest.rusd 往返无损；fmt 幂等；每个语法规则有测试

## M2 — 场景与校验

- schema 注册表（内建几何 + 属性约束）
- stage、layer stack、references、variant sets、payloads
- JSON 诊断（稳定 code）
- `rusd validate` / `rusd query`
- 验收：非法场景给出可定位诊断；overlay 覆盖 base 可演示

## M3 — 过程化核心

- ChaCha8 路径键控 RNG 流
- RusdScatter / RusdGrid / RusdInstance
- 预算、增量展开、内容哈希缓存、实例覆盖
- `rusd expand`（含 --dry-run）
- 验收：同 seed 两次展开逐字节一致；改兄弟节点不 reshuffle；超预算有诊断

## M4 — 渲染 MVP

- wgpu headless：内建几何、camera、beauty + prim-id pass
- 容错占位 + 诊断叠加；确定性截图
- 热重载交互 viewer（winit + egui）
- `rusd render` / `rusd pick`
- 验收：forest.rusd 出图；id pass 像素 → prim path 往返正确

## M5 — Agent 协议

- `rusd serve`：stdio JSON-RPC 工具集（SPEC §7）
- MCP adapter
- 验收：一个 agent 从一句 prompt 出发，经 query→patch→validate→render 循环建出森林场景

## M6 — 互操作与扩展

- rusd-usd：USDA 导出（含 --bake）/ 子集导入
- RusdLSystem / RusdTerrain / RusdWfc
- 示例画廊 + 文档
- 验收：导出的 USDA 能被 usdview / Blender 打开（子集范围内）