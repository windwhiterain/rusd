# Rusd Python 操作脚本（D10）

agent 操作 Rusd 的 Python 接口（替代 MCP，决策 D10）。纯标准库，无第三方依赖。
一个客户端 = 一个有状态 session：启动 `rusd serve` 子进程，走 stdio JSON-RPC 2.0
（NDJSON）。文档是唯一事实来源，`patch` 编辑文档（D11），cook 从中展开 generator。

## 文件

- `rusd_client.py` — `RusdClient`：`open / query / validate / expand / render / pick /
  patch / set / define / remove / undo / redo / diff / state`。上下文管理器自动收尾。
- `example_build_forest.py` — 完整 agent 循环示例：从最小场景经 `patch` 建出森林，
  `render` / `pick` / `undo` / `redo`。即 M5 验收。
- `pyproject.toml` / `.python-version` — uv 项目（stdlib-only，pin Python 3.12）。

## 运行（uv）

本机用 **uv** 提供 Python（见下「环境」）。在 `python/` 目录：

```bash
uv run example_build_forest.py        # uv 自动用 .python-version 的 3.12 建 .venv
uv run rusd_client.py                 # 客户端冒烟测试
```

`rusd` 二进制查找顺序：`RUSD_BIN` 环境变量 → `PATH` → 从脚本向上找 `target/{debug,release}/rusd`。
先 `cargo build -p rusd-cli` 出二进制。渲染（`render`/`pick`）需本机 GPU；编辑步骤不需要。

## 环境（本机实测）

- **peer `lichen-vm`（Admin）自带的 Python 3.13 是坏的嵌入式包**（缺 `Lib/` stdlib），
  `PYTHONPATH` 还指向不存在的 pdm pep582 路径 —— 不要用它。
- **uv 已装**（`~/.local/bin/uv`），并管理一个自包含 CPython 3.12
  （`uv python install 3.12`，`uv run` 自动选用）。脚本已在 peer 用 uv 端到端验证。
- 若 `uv` 不在 PATH：`export PATH="$HOME/.local/bin:$PATH"`。

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