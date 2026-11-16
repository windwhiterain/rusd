# Rusd

**Rusd** 是一个用 Rust 实现的、面向 **Agentic Procedural Content Generation** 的 USD 方言，
以及一个为 agent 编辑循环协同设计的渲染器。

> 让 agent 像编辑代码一样编辑 3D 世界：小 diff、强类型、确定性展开、机器可读的渲染反馈。

## 为什么不直接用 USD？

USD 是为人类 DCC 管线设计的场景描述：USDA 冗长、crate 二进制不可读、合成规则（LIVERPS）复杂，
且没有原生的过程化语义。Agent 需要的是：

1. **可编辑性** — 每个 prim 一个块、绝对路径寻址、规范化排序，一次编辑 = 一个最小 diff；
2. **确定性** — 同 seed + 同输入 ⇒ 同场景 ⇒ 同像素；编辑别处不会 reshuffle 已有内容；
3. **可反馈** — 校验诊断与渲染结果（含 prim-id 分割 pass）都是机器可读的，agent 能"看见"并自我纠正；
4. **过程化第一公民** — scatter / L-system / terrain 等 generator 是 schema，而不是烘焙后的死几何。

Rusd 语义对齐 USD（prim / property / layer / reference / variant），但只实现文档化的子集，
并把过程化扩展、agent 编辑协议和渲染器放在一起协同设计。

## 状态

M0（脚手架 + 规范草案）。见 [docs/ROADMAP.md](docs/ROADMAP.md)。

## 快速开始

```bash
cargo test --workspace
```

## 文档

- [docs/SPEC.md](docs/SPEC.md) — 方言与系统规范（v0 草案，单一事实来源）
- [docs/ROADMAP.md](docs/ROADMAP.md) — 里程碑与验收标准
- [AGENTS.md](AGENTS.md) — 在本仓库工作的 agent 须知
- [examples/forest.rusd](examples/forest.rusd) — 语法示例（M1 后可解析）

## License

Apache-2.0（默认选择，与 OpenUSD 一致；未定案前可改）。