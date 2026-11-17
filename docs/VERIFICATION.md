# Rusd 导出桥独立验证（M6）

状态：完成（post-fix 复测）· 2026-11-17 · 对应 ROADMAP M6 导出桥验收

本文记录 **M6 导出桥（展开→flatten→usda/usdc/usdz）的独立验证链**：用第三方参考实现
（pxr / usd-core）读回 Rusd 导出的 USD，再用路径追踪渲染器（Cycles）出图，对照
`rusd expand` 的 prim 数与几何。验证链不依赖 openusd（Rusd 自己的后端），因此是独立背书，
而非自证。

脚本在 `scripts/verify/`（tracked）；产物在 `target/`（gitignored，本地产物，需先跑
`rusd export` 才会出现）。

## 1. 验证链

```
Rusd 场景 (city.rusd / forest.rusd)
  └─ rusd expand ─ rusd export ─→ target/city.usdc, target/forest.usda
        │                          │
        │  prim 数对照              ├─ verify_usd.py   (参考实现 pxr 读回，schema/元数据/bbox)
        │                          ├─ make_review.py   (生成 review wrapper：相机 rig + 灯光)
        │                          └─ usd_to_cycles.py (pxr 读回 → 三角化 → Cycles XML)
        │                                   └─ cycles.exe --background … ─→ target/{city,forest}_cycles.png
```

两条独立腿：

1. **结构腿**：参考实现 pxr 打开导出件，逐 gprim 校验 schema（Cube 有 size>0、Sphere 有
   radius>0、Mesh 有 ≥3 点），并核对 stage 元数据（defaultPrim / upAxis / metersPerUnit）
   与世界 bbox。
2. **视觉腿**：pxr 读回 → Cycles XML → Cycles 5.3 local build 路径追踪出图，人工确认城市/
   森林几何连贯（法线正确、构图可用）。**post-fix 已无浮空**：city 全部 133 个建筑 Cube 的
   世界 z-min 精确为 0.0，且 `/World/City` 子树 bbox 落在 `/World/City/Ground` 的
   0..444 足迹之内；此事由回归测试 `city::tests::ground_covers_the_whole_street_grid`
   长期保证（见 §7.1）。

## 2. 复现命令

先确认 `target/` 里已有**用 post-fix 二进制**导出的产物——注意 `target/release/rusd.exe`
的 mtime 必须晚于合并提交；否则先重建（`cargo build --release -p rusd-cli`）再 export，
否则你复现出来的是 pre-fix 的 783 bbox（本轮实测踩过这个坑）。所有 Python 命令用
`uv run --no-project --with usd-core --python 3.12`，避免依赖仓库的 venv / 环境变量。

```bash
# 从仓库根跑；Git Bash 里 cargo 类工具先 source scripts/msvc-env.sh
source scripts/msvc-env.sh
export PATH="$HOME/.local/bin:$PATH"

# 用当前源码的二进制导出（--release 若不存在就用 target/debug/rusd.exe）
cargo build --release -p rusd-cli
target/release/rusd.exe export examples/city.rusd   target/city.usdc
target/release/rusd.exe export examples/forest.rusd target/forest.usda

# 结构腿：参考实现读回校验（target/city.usdc 与 target/forest.usda 均需在）
uv run --no-project --with usd-core --python 3.12 python scripts/verify/verify_usd.py

# review wrapper：bbox 拟合 look-at 相机 + 太阳/补光，包住导出
uv run --no-project --with usd-core --python 3.12 \
    python scripts/verify/make_review.py target/city.usdc target/city_review.usda
uv run --no-project --with usd-core --python 3.12 \
    python scripts/verify/make_review.py target/forest.usda target/forest_review.usda

# XML 桥：pxr 读回 → 三角化 → Cycles XML
uv run --no-project --with usd-core --python 3.12 \
    python scripts/verify/usd_to_cycles.py target/city_review.usda target/city.xml 960 540
uv run --no-project --with usd-core --python 3.12 \
    python scripts/verify/usd_to_cycles.py target/forest_review.usda target/forest.xml 960 540

# 出图（Cycles 5.3 local build，同 whiting 用的那个）
/c/resource/cycles/install/cycles.exe --background --device CPU --samples 64 \
    --width 960 --height 540 --output target/city_cycles.png target/city.xml
/c/resource/cycles/install/cycles.exe --background --device CPU --samples 64 \
    --width 960 --height 540 --output target/forest_cycles.png target/forest.xml
```

`verify_usd.py` 以 `target/` 下的固定路径为准（假定 repo-root cwd）；另外两个脚本以
argv 取路径。

## 3. 数字表（post-fix 实测）

参考实现读回结果（2026-11-17，二进制重建后复测；`verify_usd.py` EXIT=0，两件均 PASS）：

| 项 | target/city.usdc | target/forest.usda |
| --- | --- | --- |
| prim 总数（asserted） | **475** | **505** |
| `rusd expand` prim 数 | 475（精确一致） | 505（精确一致） |
| 几何 gprim 数 | 434 | 501 |
| 按类型 | Cube 133 + Sphere 301 | Sphere 501 |
| 无效 gprim | 0 | 0 |
| defaultPrim | `/World` | `/World` |
| upAxis / metersPerUnit | `Z` / `1` | `Z` / `1` |
| 世界 bbox min | `(-210.26, -211.44, -2.0)` | `(-50.47, -50.43, -0.5)` |
| 世界 bbox max | **`(444.0, 444.0, 52.07)`** | `(50.48, 50.62, 1.40)` |
| bbox 尺寸 | `(654.26, 655.44, 54.07)` | `(100.96, 101.05, 1.90)` |

（city 434 几何 = 133 Cube + 301 Sphere，由 475 总数减去 41 个非 gprim 得到；forest 505
总数 = 501 Sphere + 4 个非 gprim。）

分子树拆解（同一份 city.usdc，用于把 bbox 的每个分量归因）：

| 子树 | gprim | bbox min | bbox max |
| --- | --- | --- | --- |
| `/World/City`（含 Ground + 133 建筑） | 133 | `(0.0, 0.0, 0.0)` | `(444.0, 444.0, 52.07)` |
| `/World/StreetProps`（行道树） | 300 | `(-210.26, -211.44, 0.0)` | `(208.66, 210.58, 5.2)` |
| `/World/City/Ground` | （1） | `(0.0, 0.0, 0.0)` | `(444.0, 444.0, 0.5)` |

所以整件 bbox 的 `min z = -2.0` 来自未变换的 `/World/Prototypes/Tree` 原型球
（`-2..2`），`min x/y` 来自 StreetProps；而 `/World/City` 自身正好落在 0..444 的地面
足迹内。

**历史注记（pre-fix vs post-fix）**：修复前 city 的 bbox max 是 `(783.0, 783.0, 52.07)`，
外圈建筑铺到 ~783 而地面只到 444，视觉腿上表现为建筑悬空。**错因不是地面尺寸不足**：Ground
444×444 恰等于网格公式 `6×60 + 7×12 = 444`，一直是对的。真正的错因是 **lot 落位把街区原点
加了两遍**——block prim 的 `xformOp:translate` 已经携带 `(cx, cy)`，lot 的 translate 又加了
一次，于是 lot 被推到网格外。初诊「ground 尺寸不足」被子 agent 实测推翻后改为上述根因。修复
提交 `7dbe97a`，合并进 main 为 `6189136`；回归测试
`city::tests::ground_covers_the_whole_street_grid`（断言 Ground 起于网格原点、宽深等于网格
尺寸、slab 在 z∈[0,0.5]，且每个 lot 落在 Ground 足迹内）已做 red-green 验证。

## 4. Cycles XML 桥的约定与坑

本地 Cycles 5.3.0 standalone build 的 **USD 直入模式 film 全空**（集成 bug），而它的 XML
模式渲染正常。因此 `usd_to_cycles.py` 用参考实现读回 stage、自行三角化、产出 XML。该 build
的其它实测事实（外部工具缺陷，记录在案、不归 Rusd 修）：

- **USD 直入模式空 film**：`cycles.exe --background … scene.usdc` 出空图；`probe_sun.xml`
  这类自带示例在 5.3 里也渲黑。
- **XML 灯光需 emission shader**：5.3 里裸 `<light strength="...">` 已失效（0 灯）；灯光必须
  是 `emission` shader，且包在 `<state shader="...">` 里。
- **XML 相机约定（实测校准）**：XML 相机视线沿**本地 +Z**（USD 是 −Z）；`translate` = 相机
  位置；`rotate` = 列约定右手 axis-angle（度）。`usd_to_cycles.py` 已按此从 USD 相机矩阵反解
  look-at。
- 太阳沿其 transform 的本地 −Z 照射（光方向与相机约定相反，实现里已对齐）。
- **review wrapper 坑**：`xformOpOrder` 必须是 `token[]`；若误写成 `string[]`，
  `UsdGeom.Xformable` 会**静默读空**该阶 op、变换退化为单位矩阵——相机/灯光不动，构图全错，
  且无警告。

## 5. review-rig 工作流

导出件**自带相机是单位矩阵、且无灯光**（见 §7.2）。DCC 直开 → 黑图或错构图。所以
先 `make_review.py` 生成一个 wrapper usda：

- `references` 原导出件（不改原文件）；
- `over` 覆盖 `/World/Camera`：按世界 bbox 拟合 look-at，`eye` 在中心 + `(0.85d, -0.85d,
  0.62d)`（d = 平面尺寸较大者），focal 35 / aperture 36×20.25；
- 加 `ReviewSun`（intensity 3，angle 0.53）与 `ReviewFill`（intensity 0.8，angle 5）两盏
  **DistantLight**。

拿到的 wrapper 再由 `usd_to_cycles.py` 转 XML 出图。post-fix 的 city wrapper 实测
`center=(116.87, 116.28, 25.03) size=(654.26, 655.44, 54.07)`（pre-fix 时 center/size 都由
783 的越界几何决定，这也是 pre-fix 构图偏掉的一个原因）。

## 6. 产物位置

| 产物 | 位置 | tracked |
| --- | --- | --- |
| 验证脚本 | `scripts/verify/{verify_usd,make_review,usd_to_cycles}.py` | 是 |
| 导出件 | `target/city.usdc`, `target/forest.usda` | 否（gitignored） |
| review wrapper | `target/{city,forest}_review.usda` | 否 |
| Cycles XML | `target/{city,forest}.xml` | 否 |
| 渲染图（peer 原件） | `target/{city,forest}_cycles.png` | 否 |
| 渲染图（服务器副本） | `C:\Users\17124\rusd-shots\{city,forest}_cycles.png` | 否（在服务器机器，非 peer） |

`target/` 全部 gitignored：这些是本地重跑出来的产物，不进仓库；脚本才是可复现的凭据。

## 7. backlog

1. **city ground / lot 落位重复叠加街区原点 —— 已修（merged）**：根因是 lot translate 把
   block prim `xformOp:translate` 里已有的 `(cx, cy)` 又加了一次，不是 ground 尺寸不足
   （Ground 444×444 = `6×60 + 7×12`，一直正确）。修复 `7dbe97a`，合并 `6189136`；回归测试
   `city::tests::ground_covers_the_whole_street_grid` red-green 验证。post-fix 实测：133 个
   建筑 Cube 世界 z-min 精确 0.0，`/World/City` bbox 落在 Ground 足迹 0..444 内。
2. **导出不带可用相机与灯光**：导出件相机是单位矩阵、无灯光，DCC 直接打开会黑图/错构图，
   必须过 review-rig（§5）。是否让 `rusd export` 产出可用相机 + 默认灯光，**决策待定**。
3. **Cycles local build 的 USD 直入模式空 film**：外部工具缺陷（§4），绕过方式是 XML 模式；
   记录在案，不归 Rusd 修，也不因此改变导出格式。
4. **行道树 scatter 域与街道网格不同原点（本轮新发现，未修）**：`examples/city.rusd` 的
   `[World.StreetProps]` 用 `domain = { w = 420, h = 420 }`，`RusdScatter::eval` 按
   `[-w/2, w/2]` 即以**世界原点为中心**采样；而 `RusdStreetGrid` 的网格/地面是**角点原点**
   `0..444`。两者原点不同，结果 300 棵树里约 230 棵落在 Ground 足迹之外（实测 x∈
   `[-210.26, 208.66]`、y∈`[-211.44, 210.58]`，而 Ground 是 `0..444`）。这与 §7.1 的 lot
   叠加问题是**两个独立问题**，lot 修复没有、也不该顺带解决它。修法有两种取向（把 scatter
   域改成对网格居中/偏移，或给 scatter 加显式 `origin` 参数），属场景/API 决策，本轮不动
   `crates/`，仅记录。注意它不影响导出桥的正确性：树仍然是合法 gprim、prim 数仍 475。

## 8. 开放项（M6 剩余）

- USD 导入桥（USD→Rusd）；
- 更多 generator（RusdLSystem / RusdTerrain / RusdWfc）；
- 示例画廊 + 文档；
- 导出相机/灯光 rig 决策（见 §7.2）；
- 行道树 scatter 域原点（见 §7.4）；
- usdview / Blender 实际打开确认（子集范围内）——本文用 pxr + Cycles 覆盖，未用 usdview。