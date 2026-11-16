# Rusd PCG 管线 — 合成图里的 layer-op（决策 D5）

状态：v0 设计 · 2026-11-16 · 采纳用户建议（Houdini Solaris / LOP 模型），openusd 实证通过
本文定义 Rusd 的过程化管线架构。generator 的具体参数见 `docs/GENERATORS.md`；增量总管线见 SPEC §6。

## 1. 模型：generator = 合成图里的 layer-op 节点

照 Houdini Solaris 的 LOP 语义：**generator 嵌在 USD 合成图里，输入是只读 layer，输出是一个新 layer**。

- generator 不是"展开成子 prim"的特殊 prim，而是一个**layer 变换节点**：
  `G(params, seed, input_layers) → output_layer`，纯函数。
- 输出 layer 经标准 USD arc（sublayer / reference）接入合成图。**展开即合成**——没有 bespoke
  展开 pass，合成引擎（openusd pcp）就是展开器。
- 输入只读 ⇒ 非破坏；输出新 layer ⇒ 可缓存、可 diff、可撤销、可溯源。

这取代了早期 §5 草案里"generator prim 展开成子 prim"的说法：那是两级模型（作者场景 + 展开 pass），
而 layer-op 模型把过程化直接表达为 USD 合成，更原生、更函数式。

## 2. 为什么这是对的模型（对齐四个第一公民 + D2/D3）

- **USD 原生**：输出是真 layer，走标准 arc 合成 ⇒ openusd 直接合成，导出 usda/usdc/usdz 免费（D3）。
- **非破坏 / 函数式**：只读输入 + 新输出 layer = 纯函数。undo = 丢弃/mute 一个 layer；
  审计 = 读 layer  provenance；diff = 两个输出 layer 之差。
- **增量自然落地（D2）**：节点输出按内容哈希 memo，输入/参数变才重跑；openusd pcp 定向重合成。
  合成图本身就是依赖图。
- **agent 编辑统一**：agent 的修改 = 在图某点插入一个 edit-layer（session overlay）。
  generator 节点与人类/agent 编辑在合成里地位一致，都是 layer 意见。

## 3. 节点类型

| 节点 | 输入 | 输出 | 说明 |
| --- | --- | --- | --- |
| **source** | — | 静态作者 layer / 导入 USD | `.rusd` 的 prim table → base layer；或 reference/payload 外部文件 |
| **generator** | 只读 layer(s) | 新匿名 layer | 纯函数 layer-op（RusdStreetGrid 等） |
| **edit** | 只读 layer(s) | edit-layer | agent/人类意见，session overlay，带 provenance |
| **compose** | 多 layer | 合成 stack | sublayer 叠放 / reference 合并 |

## 4. generator 契约与 openusd 映射

```
读输入：query 合成场景（只读）——例如 BlockFill 读 StreetGrid 输出的 parcel:extent
计算：  纯函数 + 路径键控 ChaCha8 RNG
作者：  sdf::Layer::new_anonymous(tag) + layer.edit(|l| PrimSpec/AttributeSpec::new ...)
接入：  Stage::insert_layer(parent_id, strength_pos, output_layer, LayerOffset)
合成：  openusd pcp 增量重合成
```

- **事务性**：`layer.edit` 自带 commit/rollback（`sdf::Data` overlay），作者失败整体回滚。
- **memo 键**：`(generator id + version, params, seed, 输入 layer 内容哈希)`。命中则复用输出 layer。
- **非破坏重跑**：参数变 → 产新输出 layer → insert 新 + mute 旧 → 增量重合成。

**实证**（`cargo run -p spike-openusd --bin layer_graph`）：
generator 产匿名 layer → insert_layer → 合成 radius=1.0；重跑(radius=5)+mute 旧 → 合成 radius=5.0，
耗时 ~389µs，`indexed_count` 不变。机制成立。

## 5. 表面语法决策（agent-first 适配 Solaris）

Solaris 用节点编辑器连 LOP 网络；agent 编辑扁平 TOML 更顺手。决策：**保持扁平 TOML 表面，
generator 是一个 prim-table，其 schema 为 generator 类型，语义是 layer-op 节点**。

```toml
[World.City]
schema = "RusdStreetGrid"     # generator 节点，锚定在 /World/City
blocks_x = 6
block_size = [60.0, 60.0]
block_prototype = "/World/Prototypes/Block"
seed = 7
# 输入：默认 = 图中弱于它的合成场景；可用 inputs = [...] 显式引用其它节点/layer
# 输出：新匿名 layer，作者到 /World/City/... 命名空间，按图位置接入合成
```

- 节点标记 prim 可保留为轻量 group（携带参数 + provenance，便于 agent query/edit），输出 prim 在其下合成。
- 跨 generator 数据流（BlockFill 读 StreetGrid 的 parcel）：用 reference 或显式 `inputs` 元数据。
- **备选**（未采纳）：独立 `[pipeline.*]` 节点表 + 显式 DAG。更贴近 Solaris 节点编辑器，但表面更复杂、
  LLM 编辑更难、且与 arc 已能表达的东西重复。v0 不做；若后续需要显式图编辑再引入。

## 6. 实例化与递归

- **模板实例**（输出相同）：USD scene-graph instance，共享原型 ⇒ GPU instancing。
- **参数实例**（输出不同，如每栋楼高度采样不同）：每实例是独立节点求值，读自己的输入上下文，
  路径键控 RNG 保证确定。GPU instancing 落在 **mesh 模板级**（共享几何 + 逐实例变换），不是节点级。
- prototype 节点可含 generator（递归）；环检测 + 深度上限（默认 8）。

## 7. layer 管理（风险与对策）

每节点求值产一个匿名 layer ⇒ 可能 layer 爆炸。对策：
- **内容哈希去重**：相同求值共享同一输出 layer。
- **合并**：静态/稳定输出 layer 可 coalesce 减少 stack 深度。
- **GC**：mute/被取代的 layer 及时回收（openusd mute/unmute 支撑）。
- 强度顺序由图位置决定，遵循 USD LIVERPS（openusd 合规）。

## 8. 与城市 suite 的关系

`docs/GENERATORS.md` 的每个 generator（RusdStreetGrid / RusdBuilding / RusdBlockFill / RusdScatter）
都是一个 layer-op 节点：
- StreetGrid 读 base → 输出路网 + parcel layer；
- BlockFill 读 parcel layer（只读 extent/zone）→ 输出建筑 layer；
- Building 是参数化原子，被 BlockFill 实例化；
- Scatter 读街道 layer → 输出街道家具 layer。
组合链 = layer 依赖图，增量沿图传播。

## 9. 触发与求值（决策 D6）

**问题**：generator 怎么被触发？要不要自定义合成回调 / 魔改 composer？

**答案**：都不需要。USD/openusd 的合成（pcp）是"现有 layer 的纯函数"，**没有"合成到某 arc 时跑过程化代码"的回调**
（pcp 里仅有的 Fn 是时间采样插值与内部遍历谓词，已核查）。这是设计如此，不是缺憾。
Solaris 也不靠合成回调：Houdini 的 **LOP cook 引擎在 USD 之上**，先 cook 出 layer，USD 再合成。
Omniverse 的 OmniGraph 同理。Rusd 照此分层：

```
.rusd 节点图
   │  Rusd cook 引擎（触发器，图求值 + 内容哈希 memo）   ← 过程化在这里跑
   ▼  产出匿名输出 layer，insert_layer 接入
openusd pcp（合成既有 layer，增量）                      ← 纯合成，不改
   ▼
 composed Stage → 渲染 / 查询
```

- **触发器 = cook 引擎**，不是 pcp 回调。它按依赖序求值节点：读上游已合成的只读输入 → 跑 G →
  产匿名 layer → insert_layer。sublayer 是 generator 的**输出产物**（数据），generator 是生产它的节点，
  cook 引擎是触发器——不是"pcp 读到 sublayer 时跑代码"。
- **两层增量**：cook 图（哪些节点重跑，内容键 memo）+ pcp（哪些 prim 重合成，依赖追踪）。
- **触发时机**：open 时 eager cook 全图（v0）；edit/patch 标脏下游重 cook；query 可 lazy cook 所需子图（后续）。
- **现有勾子够用，无需改 composer**：读输入=Stage query；产 layer=new_anonymous+edit；
  接入=insert_layer/add_sublayer；增量=pcp；脏追踪=StageSink/LayerSink 编辑事件。
- **唯一的合成期勾子备选**：`ar::Resolver`（自定义资产解析器）可把 generator 输出建模成 payload 资产，
  解析时跑 generator → pull-based 懒生成。但 resolver 在合成中途被调，读不到"正在合成的场景"作输入，
  有重入/环风险；只适合纯参数 generator，不适合读场景的 generator（如 BlockFill 读 parcel）。v0 不用。

**实证**（`cargo run -p spike-openusd --bin cook_graph`）：3 节点 A(parcel.width)→B(building.height=width*2)、C(独立)。
初始 cook height=20；no-op pass 计数 A1/B1/C1（全 memo 命中，零重 cook）；改 A=30 后 A2/B2/C1
（下游 B 重 cook、独立 C 不动），height=60，重 cook A+B ~616µs。触发=cook 引擎，无回调、无魔改。

## 10. 开放问题

- **P1** 节点标记 prim 是否在最终场景保留（vs 仅输出 prim 保留）？倾向保留轻量 group 便于 agent 寻址。
- **P2** 显式 `inputs` 元数据 vs 纯 reference 表达跨节点数据流？v0 先用 reference + 隐式上下文。
- **P3** 输出 layer 的持久化/缓存格式（内存匿名 vs 落盘内容哈希缓存）？v0 内存，落盘缓存后续。
- **P4** layer 合并/GC 策略的触发时机与阈值。
- **P5** cook 触发粒度：v0 eager 全图；lazy cook（按 query 子图）与 ar::Resolver 备选后续评估。