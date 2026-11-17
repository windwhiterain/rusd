# D12 v2 — viewer 是 client，rusd 是 server（remote 切在 USD 层）

状态：设计定稿 · 2026-11-17 · 对应 SPEC §11 D12、ROADMAP M4'

本文定义 Rusd 的远程交互查看架构。一句话：**同步的对象是 USD layer / 快照，不是像素；
合成与渲染留在 client 本地**。这样 orbit / zoom / 点击 / 拾取都不产生网络往返。

## 1. 背景

- 用户的 shell 在 **server 机器**（有 GPU），而**仓库与 GPU 都在 peer `lichen-vm`**，
  SSH 会话没有显示。人要看场景、agent 要"指一个像素"来选中对象，两边都需要画面。
- 传帧方案（server 渲染、client 收图）被否：带宽与延迟都不可接受——每一帧都是
  全幅像素，而 orbit 一次就是几十帧；远程链路上这条路走不通。
- 反过来想：**场景比画面小得多**。city 展平后的 usdc 快照只有 **58KB**，而它的
  一张 960×540 RGBA 帧是 2MB 量级。传场景、在本地渲染，数据量差两个数量级，
  而且一次传输可以支撑任意多次相机运动。

## 2. 决策

**D12 v2：viewer 是 client，rusd 是 server；remote 切在 USD 层，不传像素。**

- **同步对象** = USD layer / 快照。
- **client 负责**：本地合成（openusd）、本地 wgpu 渲染、本地 pick / annotate。
  ⇒ orbit、zoom、点击零网络往返。
- **viewer 是人类 + agent 协同开发的 interface**：人用 gizmo 拖、agent 用工具改，
  **共享物是场景**本身，而不是某一方的画面。两端看到的是同一份合成结果的两次渲染。
- **client 形态 = 原生 Rust 二进制**（winit + egui），**浏览器 client 出局**：
  wasm 里跑一套完整 USD 合成太重（openusd 依赖、内存、线程模型都不合适），
  收益不抵成本。分发方式：**peer 上构建、scp 到用户机**，要求 **同 win-x64**。

## 3. server 权威 stack

编辑面有三个 layer，合成顺序即权威顺序：

| layer | 内容 | 决策 |
| --- | --- | --- |
| 文档 profile layers | `.rusd` TOML 本体（CST，保注释保空白） | D11 |
| cook 输出 layer | generator 展开产物 | D6 缓存 |
| overrides session layer | human gizmo 拖动 / agent 意见 | §8 patch 的落点 |

- **undo 覆盖两个编辑面**：文档 CST 与 overrides session layer 都要能撤销。文档侧是
  文档快照回放（D11）；override 侧是 session layer 上的作者操作回放。
- server 是唯一权威：client 不产生"真相"，它只产生**意见**（opinion），意见经协议
  回到 server 的 overrides layer，再由 server 广播新快照 / 新 stack 回来。

## 4. Tier A — MVP：渲染镜像（flattened 快照）

最省事、也最先能用的形态：

```
server: expand + flatten ─→ 一份 usdc 字节 ──wire──→ client
client: Layer::from_bytes → 单层栈 → extract → 本地 wgpu 渲染
```

- **change 时推 flattened usdc 快照**。实测（父会话，release）：city 展平快照 **58KB**；
  全进程 **export 0.159s / expand 0.107s**。也就是说一次变更的传输代价是几十 KB。
- client 侧用 **`Layer::from_bytes`** 装成**单层栈**再提取几何。
- flatten 把资产烘进去、丢掉全部 arcs ⇒ 这份快照**是自足的**，client 不需要任何
  外部 asset，因此**匿名安全**：它不引用别处的路径，落盘/入库都不担心 dangling。
- 代价：client 不再有 prim 级的合成语义（mute、variant、arc 全靠 server 决定），
  也不能对未展平的结构做局部编辑。**这正是 Tier A 的定位：渲染镜像。**

## 5. Tier B — 协同合成镜像（命名 layer stack 逐层推送）

要 client 真正"协同合成"，就得让它持有 layer 栈，而不是一张烘过的图：

- **命名 identifier 的 layer stack 逐层推送**：每层按标识符推一份内容，
  **usda 文本**或 **usdc 字节**（字节经 `LayerRegistry::find_by_id("usdc").write_bytes`）。
- client 侧：**自定义 `ar::Resolver`**（把 identifier 映射到已在内存里的字节）
  \+ `Stage::builder().resolver(...).session_layer(...).open()`，逐 opinion 镜像 server 的栈。
- **layer 变更 ⇒ 重建 stage**。这不是偷懒，而是 crate 的限制：openusd 0.8 的
  `Stage` **没有 reload / rebind**，`session_layer` **只在 builder 期**能设
  （`Stage::session_layer()` 是只读 getter）。所以要镜像新状态，只能重新 build。
- **`Diff` / `apply_diff` 留作细粒度增量，后置**：`sdf::Value` / `Diff` **没有 serde**，
  想在 wire 上跑 diff 必须先写一个自定义 codec（枚举 → JSON），工作量与风险都不小，
  不值得挡在第一版前面。Tier B v1 用"整层替换 + 重建"就够。

## 6. 线协议

- **TCP NDJSON JSON-RPC**，与 `rusd serve` 同族：一行一个 JSON-RPC 2.0 对象。
  （现状 `rusd-agent` 是 stdio JSON-RPC，方法名 `open/query/validate/expand/render/pick/
  patch/diff/undo/redo/export/state`，`handle/call` 的分派结构直接可复用。）
- **复用 rusd-agent 的工具名**，加 **`subscribe`** 与通知：
  `stage_snapshot` / `layers_changed` / `diagnostics` / `state_changed`。
- **默认 loopback + SSH 隧道**：server 只 bind `127.0.0.1`，client 走 `ssh -L` 转发。
  `--bind LAN` **后置**（要显式开，且届时需要认证话题）。
- **相机归 client**：**每次 `render` 带全部 `RenderConfig` 参数**，**server 对相机无状态**。
  这是"不传像素"的必然推论——相机在谁手里，谁就负责成像；server 不必记住任何视角，
  也因此在多 client 之间天然不冲突。

## 7. 线程模型

`Stage` 是 **`Rc` 包裹的（`pub struct Stage(Rc<StageInner>)`）⇒ `!Send`**，不能跨线程。
所以：

- **sync 线程独占 stage**：所有 openusd 合成、作者操作、拿几何都在这一条线程上。
- **owned `GeoItems` 经 channel 交渲染线程**：跨线程传的是已经提取好的、自持的几何
  数据（不是 `Stage` 的引用），渲染线程因此可以是独立的 wgpu 线程。
- 这条边界是硬边界：任何"把 `Stage` 挪到渲染线程"的想法都编译不过，方向是对的。

## 8. 确定性约束

远程两端各跑一份 openusd、各跑一个 wgpu，要做到"同一场景两端看到的几何一致"：

- **openusd 0.8 双端 pin 同一精确版本**，**同 win-x64**。1.0 前 API 不稳，跨版本
  合成差异虽小但足以让 parity 变得不可解释。
- **只序列化命名 identifier**：**匿名 id 是进程内计数器**，在另一进程里没有意义，
  绝不能上 wire。这是 Tier B 的核心纪律。
- **resolver 两侧 identity**：client 的自定义 `ar::Resolver` 必须对 identifier 做
  恒等映射（不改写、不规范化），否则两端打开的其实是两个不同的栈。
- **wire 用 usda / usdc，不用 usdz**：usdz 是 zip 容器，**zip 时间戳**会让"同内容不同字节"，
  破坏可复现与哈希比对，属潜在风险，直接避开。

## 9. 侦察缺口映射（Rusd/usd-api-recon 六问，G 编号）

| 缺口 | 含义 | D12 v2 的处置 |
| --- | --- | --- |
| G1 / G2 / G6 / G10 | client 侧拿不到 server 的 layer 内容、identifier 解析不了 | **resolver 方案**：自定义 `ar::Resolver` + 逐层推送（§5） |
| G3 / G5 | crate 无 stage reload / session layer 运行期不可改 | **重建**：layer 变更即 rebuild stage（§5） |
| G4 | 没有外部变更探测（无 watch / 无事件） | **推送驱动**：由 server 主动推，不做外部变更探测 |
| G7 | `sdf::Value` / `Diff` 无 serde，diff 上不了 wire | **layer 文本 wire 先行**，Diff codec 后置（§5） |
| G8 | flatten 后丢失合成结构 | **flatten 仅用于渲染快照**（Tier A），结构协同走 Tier B |
| G9 | `Stage` 是 `Rc` ⇒ `!Send` | **线程钉死**：sync 线程独占，几何经 channel（§7） |

## 10. spike 计划

1. **①传输可行性 — 已完成**：city flatten usdc **58KB**；release 全进程
   **export 0.159s / expand 0.107s**。结论：传场景的量级完全可接受。
2. **②headless 内存合成 + 渲染 parity — 进行中**（`Rusd/viewer-spike`）：
   两个子命令 `snapshot` / `compose`，比对 Tier A 快照镜像与本地逐层合成的几何/像素
   是否一致。这是 Tier B 能不能成立的前置证据。
3. **③egui 壳 + gizmo → opinion 闭环 — ②过后开**：交互拖动产出 override opinion，
   经协议回到 server，再收到新快照。

## 11. 里程碑（M4 重排为 M4'）

原 M4 的"热重载交互 viewer（winit + egui），走 SPEC §6 增量管线"被本决策取代：
viewer 不再是 server 进程内的一个窗口，而是一个**独立 client 二进制**。M4' 的条目
（server 传输 + 推送、`rusd-viewer` crate、隧道文档）见 ROADMAP。

## 12. 后置与开放

- **Diff / apply_diff 细粒度增量**：需先写 `sdf::Value` / `Diff` 的自定义 codec。
- **`--bind LAN`**：默认只 loopback + 隧道；LAN 直连要连带认证决策。
- **浏览器 client**：已否（wasm 跑完整 openusd 合成过重）。
- **mute / variant 等合成语义的 client 侧交互**：Tier B 之后。