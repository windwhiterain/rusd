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
- **出图水平镜像（验收时注意）**：`scripts/verify/usd_to_cycles.py` 的 XML 桥出图相对
  `rusd-render` 是**水平镜像**的——Cycles XML 相机沿本地 **+Z**（USD 是 −Z）、屏幕坐标随之
  呈左手，同 eye/target 下同一棵树会出现在画面左右相反侧。这只影响**出图观感**，
  **不影响几何校验**（点数/bbox/prim 数照旧可比）。详见 `docs/VERIFICATION.md` §4 的
  XML 相机约定。

## 5. Tier B — 协同合成镜像（命名 layer stack 逐层推送）

要 client 真正"协同合成"，就得让它持有 layer 栈，而不是一张烘过的图：

- **命名 identifier 的 layer stack 逐层推送**：每层按标识符推一份内容，
  **usda 文本**或 **usdc 字节**（字节经 `LayerRegistry::find_by_id("usdc").write_bytes`）。
- client 侧：**自定义 `ar::Resolver`**（把 identifier 映射到已在内存里的字节）
  \+ `Stage::builder().resolver(...).session_layer(...).open()`，逐 opinion 镜像 server 的栈。
- **usda 的 `over` 不能写绝对路径**（openusd 0.8 解析器行为）：`over "/World/City"` 这类
  绝对路径不成立，必须**阶梯嵌套**——`over "World" { over "City" { … } }`。做 Tier B 的
  session layer 镜像时按同样形状生成，否则整条 override 静默丢失。
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

### 7.1 DPI：窗口的三种尺寸，和一个必须成立的等式

client 侧一个窗口同时有三种尺寸，它们**不是同一个数**：

| 尺寸 | 来源 | 单位 |
| --- | --- | --- |
| client area | `Window::inner_size()` | **物理**像素 |
| 布局空间（screen rect） | `RawInput::screen_rect` | **point** |
| presenter 的 back buffer | `egui_wgpu::winit::Painter` 的 swapchain | **物理**像素 |

三者只有一个约束：`screen_points == inner_physical / scale_factor`，**一次**。破坏它只有
三种方式，`crate::viewport::ScreenSpace` 把比值打印出来，让截图自己说明是哪一种：

| 比值 | 含义 |
| --- | --- |
| `1.000` | 正确 |
| `scale` | scale 从未除过：物理像素当 point 用，整个 UI 放大 `scale` 倍 |
| `1 / scale` | scale 除了两次：布局以为窗口更大，右/下边缘落在窗外 |

**第四条约束在 presenter 上**：swapchain 必须与当前 `inner_size()` 同尺寸，否则合成器
按 `DXGI_SCALING_STRETCH`（`wgpu-hal` 在 DX12 上请求的唯一 scaling 模式）把旧尺寸的
back buffer 拉伸铺满窗口，两轴系数不同时 UI 连形状都是错的。`Painter` 只在 `set_window`
和 `on_window_resized` 里配置它，**所以每次 `Resized` 都必须调用后者**；这也是
egui-winit 自己的集成的做法。

两条都是**数据可验**的，不靠叙事：`--debug-ui` 叠加层逐行打印

```
screen <w>x<h> pt · inner <w>x<h> px · scale <s> · expect <w>x<h> pt
winit scale count <r>x (1.000 = ok) · <ok | NO pixels as points | NO scaled twice | NO not a whole scale | NO winit agrees with itself; its scale is stale (see the os row)>
os client <w>x<h> px · os scale <s> · window <w>x<h> px · <agrees with winit | NO winit disagrees>      (仅 Windows；否则 `os probe n/a`)
os expect <w>x<h> pt · layout is <r>x<r> of it · <point space fits the os window | NO point space overflows the os window>
dpi awareness <per-monitor-aware | ...>
monitor <W>x<H> px · client <w>x<h> px · <fits the screen | NO the window is bigger than its screen>
layout used <w>x<h> pt: the screen rect was corrected this pass | layout used the platform's own screen rect (no correction)
back buffer <w>x<h> px · window inner <w>x<h> px · matches <yes|NO>
present stretch <sx>x<sy> (1.000x1.000 = ok) · <blit | NO magnified | NO anisotropic ...>
swapchain resized this pass <w>x<h> → <w>x<h> px          (仅修复那一帧出现)
partition OK|NO
width <left> + <rect> = <sum> vs window <w> (Δ <d>) · fits <yes|NO>
height <top> + <bottom> + <rect> = <sum> vs window <h> (Δ <d>) · fits <yes|NO> · bars <yes|NO>
```

`winit scale count` 只能检查 winit **自己**是否自洽（`screen_rect == inner / scale`，两边都是
winit 的数字），因此 winit 的 scale 陈旧时它照样读 `ok`——而那正是陈旧 scale 唯一的症状。
所以 OS 行说"不一致"时，这一行也改说 `NO ... its scale is stale`：读者不必越过后面的行
才看见缺陷，`NO` 的红字也和其它失败一视同仁。

`back buffer` 报的是**这一趟继承到的**尺寸，不是这一趟装上去的：否则修好的那一帧会永远
读成 `matches yes`，唯一能看见"UI 被拉伸"的仪表就自己闭眼了。修复本身由 `swapchain
resized this pass` 单独一行说明。

`partition` 的两条恒等式与 `tests/panel_layout.rs` 断言的是**同一段算术**
（`crate::viewport::Partition`），所以截图上的 OK 和测试里的绿是同一句话。`bars` 单独
一行是因为一条零高度的控件栏仍然让两条恒等式成立——上上轮探针报的
`25 + 0 + 702 = 727` 正是这种情形，只有把 `bottom` 单独打印才看得见。

修正本身（`ScreenSpace::correction`）是**有条件**的：只在比值恰好是 scale 的整数幂时
改写 `screen_rect`，比值 `1.000` 时一个字节都不动。理由是那次探针的两个状态
（`2240/1.75 = 1280.0`、`2560/1.75 = 1462.9`）都属于正确行，无条件覆盖会改写一个本来就
对的布局，再把自己的算术当成"修好了"。比值既不是 `1` 也不是整数幂时报 `odd` 且不改写：
两个数都在说谎，而代码没有依据判定哪一边错，猜一次就是第二个缺陷。

窗口缩放**不重新 fit 相机**（§6：相机是 client 的）；这条约束与前两条无关——DPR 只决定
"一个 point 有几个像素"，不决定"看多远"。

### 7.2 为什么上表还不够：一个库不能作自己的第二意见

7.1 的三条约束都由 **winit 自己的输出**构成：`inner_size()` 的实现就是 `GetClientRect`
（winit-0.30.13 `platform_impl/windows/window.rs:214`），`scale_factor()` 是它缓存的
`WindowState` 字段，而 7.1 的应有值又是这两个数相除。于是「比值 1.000」只证明**winit 与
自己一致**——第九轮的真机就出现了这种情形：当时的叠加层（那一版把第一行叫 `scale count`，
现在叫 `winit scale count`）报 `1.000x · ok`、
`present stretch 1.000x1.000 · blit`、`partition OK` 全绿，而城市仍在窗外。仪表当时把
自洽当成了健康。

**第十轮把窗口外扩到 outer 1440×800 后的实测数字**，值得逐个看清：

| 项 | 值 | 来源 |
| --- | --- | --- |
| winit `inner_size()` | 2496×1336 px | `GetClientRect` |
| `scale_factor()` | 1.75 | `WindowState` 缓存 |
| `screen_rect` | 1426.3×763.4 pt | 上两者相除 |
| 「实际物理 inner」 | ~1424×761 | **未观测**（外部工具，未说明 instrument） |

`2496 / 1424 = 1.7528`，`1336 / 761 = 1.7556`，而 `1424 × 1.75 = 2492`：所谓「实际物理」
恰好是 winit 的报值**除以 scale**。这正是「又一次 logical↔physical 混淆」的签名——
一个 `1440×800` 的**逻辑**窗口在 1.75 下是 `2520×1400` 物理，其 client area 正是
`2496×1336`，也就是 winit 报的那个数。**故：winit 并未把 inner_size 报大 1.75 倍。**

`inner_size()` 是同一个进程、同一个 HWND 上的 `GetClientRect`，它不可能与 OS 同一次调用
的结果不一致；把线源「改用 GetClientRect」因此无法改变任何一个数字。能不一致的只有
**scale**：winit 只在 `WM_DPICHANGED`（event_loop.rs:2216）刷新缓存值，窗口被 resize 而
DPI 不变时它保持原值——这才是「点空间被 1.75 误乘」的唯一可达机制。

于是第十轮引入**独立第二意见**（`crate::platform_metrics`，`windows-sys` 直连 win32）：

- `client rect`——`GetClientRect`，与 winit 的 `inner_size` 同一次调用，用来证明探针接的
  是同一个窗口（若不一致，是探针的接线 bug，其余各行都不可信）；
- `window rect`——`GetWindowRect`，其差值是边框/标题栏，用来区分 client-area 报值与
  整窗报值；
- `os dpi`——`GetDpiForWindow`，`scale = dpi / 96`；
- `dpi awareness`——`GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext())`，
  `per-monitor` 即「进程不被 DPI 虚拟化，`GetClientRect` 返回真实设备像素」；
- `monitor`——`GetMonitorInfoW`，**决定「窗口本身就比屏幕大」**。这是唯一能在其余全部
  检查通过时仍然成立的缺陷：client area 2496 物理像素装不进 1920 宽的屏幕，窗口就真的
  挂在窗外，与任何点空间算术无关。

修正规则随之明确（`ScreenSpace::correction`）：**有探针时 OS 是权威，无论它与 winit 是否
一致**——布局必须占满 OS 描述的 client area；要求两者一致才修正，恰好会在最需要的时候
跳过。比例与 rect 一起改（`native_pixels_per_point` 同用 OS 值），否则布局对而渲染错，
就是 7.1 已经踩过的 `texels != rect × ppp`。无探针时退回 winit 算术并打印
`os probe n/a`——没有第二意见就不声称一致。

`partition` 的 `window` 项也随之改为**权威值**（`authoritative_points`：优先 OS 的
`client/scale`）。第九轮它取自 `expected_points()`，那是同一个除法，于是恒等式在两个
同谋者之间成立——截图报 OK 而视口越过屏边，正是这个错误的直接后果。

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
2. **②headless 内存合成 + 渲染 parity — 已完成**（合并 main `71f5a82` ← `spike/viewer-headless`
   = `601cb0c`）。`rusd-viewer` 新建 crate（17 测试），全 workspace **123 绿**，fmt/clippy 零警告。
   两子命令 `snapshot` / `compose`。本机（同机）parity 三项：内存合成栈 ≡ 磁盘栈 **434 项
   逐 item 1e-9**；session 覆盖 `(15,12,7)` = block `(12,12,0)` + 覆盖 `(3,0,7)`；镜像 ≡
   直接合成**逐 opinion 全等**。这是 Tier B 成立的前置证据。
   - **跨机器腿（peer 构建 → scp 用户机 → 本地 GPU 跑）**：peer 构建 release
     `rusd-viewer.exe`，scp 到用户机 `C:\Users\17124\rusd-client\`，在**用户机本地 GPU**
     跑 `snapshot`。结果与 peer **逐位相同**：items/prims **434**；bbox min
     `(-210.26225399971008, -211.44087135791779, -2.0)` / max `(444, 444, 52.065452575683594)`；
     foreground **121073** 像素两机相同；PNG 均 **41752B**。**逐像素差 125/518400 = 0.024%**，
     max channel Δ86，**全在几何边缘**——独立 GPU 光栅化精度差，属预期容差（不是逻辑不一致）。
     ⇒ D12 §8 的命名空间不变量在真实跨机器、真实双 GPU 下站得住。
   - **产物位置**：peer `target/peer_city.png`；用户机
     `C:\Users\17124\rusd-client\{server_city.png, peer_city.png, rusd-viewer.exe, city.usdc}`。
     注意这些是**用户机上**的路径，peer 看不到。
   - **备注：parity 测试依赖主树产物**。`crates/rusd-viewer/tests/parity.rs` 要读主轨的
     `target/city.usdc`（**gitignored**），在 worktree 里跑测试时会向上探测 6 层找它；
     找不到就显式 `panic` 并列出所有探过的路径。在干净的 worktree / 另一台机器上跑，用
     **`RUSD_MAIN_TREE=<主轨根>`** 环境变量直接指定。这不是新坑，是“成品产物不进仓库”的必然
     代价；提醒：不要误以为测试红了就是 parity 坏了。
   - **备注：出图水平镜像**同 §4（Cycles XML 相机 +Z 左手屏幕坐标）；跨机器比对像素时
     两机走同一条路径，所以不受影响，但**不要**拿 rusd-render 的图跟 XML 桥的图直接叠图比较。
   - **状态：现在两条子 agent 在实现**：`m4/serve-transport`（server 传输层 + 推送）与
     `m4/viewer-egui`（egui 壳 + gizmo → opinion），契约 `docs/PROTOCOL.md`（`67b5522`）。
3. **③egui 壳 + gizmo → opinion 闭环 — ②过后开**：交互拖动产出 override opinion，
   经协议回到 server，再收到新快照。

### 热重载延迟：证据与缺口

四层时间轴（epoch ms 对齐，`release` 提交 `8767` 实测；共六个可观测阶段，归四层）：

| 层 | 阶段 | 量级 |
| --- | --- | --- |
| 1 | `notify` 交付（文件事件到达监视回调） | 毫秒级 |
| 1 | wake 入队（投进 sync 线程的消息队列） | 毫秒级 |
| 2 | sync 出队（循环取到该消息 / 检查 deadline） | 毫秒级 |
| 3 | deadline（debounce 到期，约 **+205ms**） | 毫秒级 |
| 4 | 快照就绪（重读 → 重建 → cook → flatten → `write_bytes`） | 毫秒级 |
| 4 | writer flush（写入订阅连接） | 毫秒级 |

每一个阶段本身都是**毫秒级**——延迟不藏在某一层里，而在**第 2/3 层的交互方式**上（见下）。

client 侧实测端到端 `dt`（落盘 → 收到 `state_changed{reason:"reload"}`）：

| 构建 | 无 client 附着 | idle 附着（只读） | busy 附着（每 200µs 发 `state`） |
| --- | --- | --- | --- |
| **`release` `8767`** | —（无接收者） | **252.7ms** | **256.6ms** |
| `debug` 对照 | A **748ms** | B **911.6ms** | — |

另：`rename` 与 `truncate` **两种改写都触发** reload，且 **watch 跨 rename 存活**
（第三轮探针：交付 **+36ms / +25ms**，均远小于 200ms debounce 上限）。

**机制与修复**（merge `f88af5f` ← `7f585ba`）：sync 循环原先**只在 `recv_timeout` 报超时时**
才认定 debounce 到期。持续 client 流量会让每次 `recv_timeout` 立即返回，于是 reload 被
**无界推迟到流量停止**——这正是“viewer 一边动画一边收不到更新”的现场症状。
修复 = **每轮 `recv` 前先查 `deadline_passed()`**，到期就地发布，不等超时。
（`is_ok()` 吞掉错误交付的修复更早，在第三轮 `034cb13` 已入。）

#### 诚实缺口（必须写明）

1. **busy 回归测试没有锁住缺陷本身**。`a_busy_client_still_gets_its_reload` 做过
   red-check：**撤掉 `deadline_passed()` 修复后该测试仍 **646ms 绿****。也就是说它验证的是
   “busy 下能收到 reload”，而不是“没有饥饿”；现有回归**未锁住饥饿缺陷本身**。
2. **15.59s 的单次观测未被任何组合复现**。`release` / `debug` × 无 / idle / busy 六种组合
   全跑过，**均未复现**父会话那次 15.59s。
3. **早先“reload 延迟 10–20s”的结论已撤回**。那是 harness 错觉：批内 `exec` 串行执行，
   改写落在观测窗口之外，被误读成延迟（AGENTS 已有对应纪律条目）。
4. **15.59s 因此保留为“未解释的单次观测”**，既不当已知 bug、也不当已证伪。

**后置项**：要真正锁住饥饿需要**负载注入式测试基础设施**（可控持续流量 + 断言
“deadline 到期即发布”而非“最终收到”），列为可选项，不挡 M4'。

**测试读取器两真缺陷已修**（同一批）：`notification_within` 超时丢半行导致 80KB 快照行错乱；
拒收带 `id` 响应导致 `poll` / `listen` 互吃。这两条是测试工具的 bug，不是 server 的。

**回归预算仍为 2s**（`BUDGET = Duration::from_secs(2)`，多处共用）；
`release` 实测 ≤1s，判据成立且留有约 4× 余量。

## 11. 里程碑（M4 重排为 M4'）

原 M4 的"热重载交互 viewer（winit + egui），走 SPEC §6 增量管线"被本决策取代：
viewer 不再是 server 进程内的一个窗口，而是一个**独立 client 二进制**。M4' 的条目
（server 传输 + 推送、`rusd-viewer` crate、隧道文档）见 ROADMAP。

## 12. 后置与开放

- **Diff / apply_diff 细粒度增量**：需先写 `sdf::Value` / `Diff` 的自定义 codec。
- **`--bind LAN`**：默认只 loopback + 隧道；LAN 直连要连带认证决策。
- **浏览器 client**：已否（wasm 跑完整 openusd 合成过重）。
- **mute / variant 等合成语义的 client 侧交互**：Tier B 之后。