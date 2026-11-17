# Rusd 导出桥独立验证（M6）

状态：完成 · 2026-11-17 · 对应 ROADMAP M6 导出桥验收

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
2. **视觉腿**：pxr 读回 → Cycles XML → Cycles 5.3 local build 路径追踪出图，人工确认城市/森林几何连贯（法线正确、构图可用；city 外圈建筑浮空属场景 ground 尺寸 bug，见 §7.1，非导出缺陷）。

## 2. 复现命令

先确认 `target/` 里已有导出产物（否则先跑 `rusd export`）。所有 Python 命令用
`uv run --no-project --with usd-core --python 3.12`，避免依赖仓库的 venv / 环境变量。

```bash
# 从仓库根跑；Git Bash 里 cargo 类工具先 source scripts/msvc-env.sh
export PATH="$HOME/.local/bin:$PATH"

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

## 3. 数字表

参考实现读回结果（2026-11-17 实测）：

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
| 世界 bbox max | `(783.0, 783.0, 52.07)` | `(50.48, 50.62, 1.40)` |
| bbox 尺寸 | `(993.26, 994.44, 54.07)` | `(100.96, 101.05, 1.90)` |

（city 434 几何 = 133 Cube + 301 Sphere，由 475 总数减去 41 个非 gprim 得到；forest 505
总数 = 501 Sphere + 4 个非 gprim。）

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

导出件**自带相机是单位矩阵、且无灯光**（见 §7 backlog (b)）。DCC 直开 → 黑图或错构图。所以
先 `make_review.py` 生成一个 wrapper usda：

- `references` 原导出件（不改原文件）；
- `over` 覆盖 `/World/Camera`：按世界 bbox 拟合 look-at，`eye` 在中心 + `(0.85d, -0.85d,
  0.62d)`（d = 平面尺寸较大者），focal 35 / aperture 36×20.25；
- 加 `ReviewSun`（intensity 3，angle 0.53）与 `ReviewFill`（intensity 0.8，angle 5）两盏
  **DistantLight**。

拿到的 wrapper 再由 `usd_to_cycles.py` 转 XML 出图。

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

## 7. backlog（验证中发现，待决策/修复）

1. **Ground 尺寸不足致外圈建筑浮空**：city 场景的 Ground 是 444×444，而街道网格约 800，
   外圈建筑落在空地上（视觉腿出图可见）。修复在另一子 agent 的 `fix/ground-size` 分支进行；
   本文只记录现象与分支名，不在此修。
2. **导出不带可用相机与灯光**：导出件相机是单位矩阵、无灯光，DCC 直接打开会黑图/错构图，
   必须过 review-rig（§5）。是否让 `rusd export` 产出可用相机 + 默认灯光，**决策待定**。
3. **Cycles local build 的 USD 直入模式空 film**：外部工具缺陷（§4），绕过方式是 XML 模式；
   记录在案，不归 Rusd 修，也不因此改变导出格式。

## 8. 开放项（M6 剩余）

- USD 导入桥（USD→Rusd）；
- 更多 generator（RusdLSystem / RusdTerrain / RusdWfc）；
- 示例画廊 + 文档；
- 导出相机/灯光 rig 决策（见 §7 (b)）；
- usdview / Blender 实际打开确认（子集范围内）——本文用 pxr + Cycles 覆盖，未用 usdview。