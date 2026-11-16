# Rusd Python 操作脚本（D10）

agent 操作 Rusd 的 Python 接口（替代 MCP，决策 D10）。纯标准库，无第三方依赖。
一个客户端 = 一个有状态 session：启动 `rusd serve` 子进程，走 stdio JSON-RPC 2.0
（NDJSON）。文档是唯一事实来源，`patch` 编辑文档（D11），cook 从中展开 generator。

## 文件

- `rusd_client.py` — `RusdClient`：`open / query / validate / expand / render / pick /
  patch / set / define / remove / undo / redo / diff / state`。上下文管理器自动收尾。
- `example_build_forest.py` — 完整 agent 循环示例：从最小场景经 `patch` 建出森林，
  `render` / `pick` / `undo` / `redo`。即 M5 验收。

## 运行

```bash
python example_build_forest.py          # 需要可用的 Python 3 + rusd 二进制
RUSD_BIN=/path/to/rusd python example_build_forest.py
```

`rusd` 二进制查找顺序：`RUSD_BIN` 环境变量 → `PATH` → 从脚本向上找 `target/{debug,release}/rusd`。
渲染（`render`/`pick`）需要本机 GPU；编辑步骤（open/patch/expand/undo）不需要。

## 环境注意（本机实测）

- **peer `lichen-vm`（用户 Admin）的 Python 是坏的嵌入式包**（只有 python.exe + dll，
  缺 `Lib/` stdlib），`PYTHONPATH` 还指向不存在的 pdm pep582 路径 —— 不能直接跑脚本。
- **服务器（用户 17124）有 Python 3.12 + GPU**，脚本开箱即用（已端到端验证）。
- 要在 peer 跑，需先装一个完整 Python（或修复嵌入式包的 stdlib）。

## 典型循环

```python
from rusd_client import RusdClient
with RusdClient() as c:
    c.open(path="examples/forest.rusd")
    c.set("/World/Forest", "count", 100)      # patch
    print(c.expand()["geo_items"])            # 101
    img = c.render(width=800, height=600)     # -> PNG 路径 + metadata
    print(c.pick(400, 300)["path"])           # 像素 -> prim path
    c.undo()
```