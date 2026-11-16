# Rusd Generator Suite — 首批：城市（决策 D4）

状态：v0 设计 · 2026-11-16 · M3 实现目标
域决策（D4，采纳用户建议）：首批 generator 做**城市/街区**——常见、易演示，且集中演练
递归 generator、预算约束、分区驱动分布、原型复用（GPU instancing）这几个 agentic PCG 核心难点。

## 设计原则（继承 SPEC §5）

- 纯函数 `(params, seed, inputs) → 子树`；路径键控 ChaCha8 RNG；确定性实例命名。
- 预算硬约束（城市极易爆炸）；递归组合；session overlay 可覆盖/pin 任意生成实例。
- 展开结果经 openusd 作者 API 写入 Stage（D3）；`.rusd` 只存参数，不存烘焙几何。

## 组合骨架

```
RusdStreetGrid → 路网 + 街区 parcel（每个 parcel 注入 extent）
  每个 parcel → RusdBlockFill → 地块 lot → RusdBuilding 实例
  街道两侧 → RusdScatter（树 / 灯 / 长椅）
```

递归 generator + prototype 引用（USD instance 模式）。**parcel 上下文契约**：
StreetGrid 给每个街区实例注入 `parcel:extent`（vec2，米）与 `zone`（token）；
BlockFill 消费这两个量来细分地块、采样高度。这是 generator 间传递上下文的标准方式
（实例 primvar / 属性注入），M3 落实其精确机制。

## 1. RusdStreetGrid — 路网与街区

生成正交路网、街区 parcel 与地面。

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `blocks_x` / `blocks_y` | u64 | 街区数 |
| `block_size` | vec2 | 街区尺寸（米） |
| `street_width` | f64 | 普通街道宽 |
| `avenue_every` | u64 | 每隔几条街设大道（0 = 无） |
| `avenue_width` | f64 | 大道宽 |
| `jitter` | f64 | 路口偏移 0..1，打破呆板网格（可选） |
| `block_prototype` | path | 每个街区实例引用的原型（内含 BlockFill） |
| `ground_material` | path | 地面材质（可选） |
| `max_prims` | u64 | 预算 |
| `seed` | u64 | 局部种子 |

输出：`Ground`（plane）、`Roads`（mesh ribbons）、`Blocks/b0000…`（Xform 实例，
行主序确定性命名，注入 `parcel:extent` + `zone`）。

## 2. RusdBuilding — 参数化单体建筑（原子）

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `footprint` | vec2 / polygon | 底面轮廓（默认矩形） |
| `floors` 或 `height` | u64 / f64 | 层数或总高（米），二选一 |
| `floor_height` | f64 | 层高（默认 3.2） |
| `setback_every` / `setback_depth` | u64 / f64 | 退台间隔与深度 |
| `roof` | token | flat / gable / hip |
| `facade` | path | 立面材质 |
| `base_color` | color3f | 基色 |
| `max_triangles` | u64 | 预算 |
| `seed` | u64 | 立面细节变化 |

输出：massing mesh（挤出 + 退台）+ roof mesh；`primvars:displayColor`。

## 3. RusdBlockFill — 街区填充

把一个 parcel 细分为地块并填充建筑。

| 参数 | 类型 | 说明 |
| --- | --- | --- |
| `parcel:extent` | vec2 | 由 StreetGrid 注入（也可手动） |
| `lot_width` 或 `lots` | f64 / u64 | 地块宽或地块数 |
| `margin` | f64 | 人行道退距 |
| `occupancy` | f64 | 0..1，地块建造概率 |
| `height` | distribution | 高度分布（米），分区驱动 |
| `zone` | token | residential / commercial / downtown / industrial |
| `building_prototype` | path | RusdBuilding 原型 |
| `max_prims` / `max_triangles` | u64 | 预算 |
| `seed` | u64 | 局部种子 |

输出：`lots/l0000…` → RusdBuilding 实例（高度从分布采样，路径键控 ⇒ 改一个地块不 reshuffle 其它）。

## 4. RusdScatter — 通用撒布（沿用）

沿街道/人行道撒布树、灯、长椅。`domain` 支持 rect / disc / path（沿线）；`prototype` 引用；
`scale` / `yaw` 分布。城市里用于街道家具与行道树。

## 分区（zoning）v0

`zone` token → 内置预设（高度均值/方差、密度、屋顶风格）。v0 用预设表；
后续可接 `RusdZoning` 场（噪声 / 手绘图）驱动空间渐变（如市中心高、郊区低）。

## 预算与 LOD

- 城市必须设 `max_prims` / `max_triangles`；超预算给 `W_BUDGET` 并**确定性截断**（按实例命名序）。
- LOD（远处建筑简化 massing）列为后续。

## 覆盖与确定性

- agent 可 pin 某栋建筑（如地标加高）via session overlay；身份稳定则重展开保留覆盖。
- 改街区 A 的参数，街区 B 的实例不变（路径键控 RNG）。

## 验收（M3）

- 同 seed 两次展开逐字节一致。
- 改街区 A 参数，街区 B 实例不变。
- 6×6 街区城市在预算内生成（M4 出图）。
- 超预算有确定性截断 + `W_BUDGET` 诊断。

## 后续域（非首批）

自然（RusdTerrain / RusdLSystem 植被）、RusdWfc（约束拼贴）。城市 suite 稳定后再扩展。