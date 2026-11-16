# AGENTS.md — 在 Rusd 仓库工作的 agent 须知

Rusd：面向 Agentic PCG 的 Rust USD 方言 + 渲染器。先读 `docs/SPEC.md`（单一事实来源）
与 `docs/ROADMAP.md`，再动手。

## 环境

- 仓库在远端 `lichen-vm` 的 `C:\resource\rusd`（gongga 会话 `Rusd`）。
  会话的文件工具（read/write/edit）直接读写该机器；`exec` 也在该机器上跑。
- **Git Bash 构建前必须先 `source scripts/msvc-env.sh`**：runner 的 PATH 缺 Git 的
  `usr/bin`（`ls`/`sed` 找不到），且没有 vcvars（MSVC `link.exe` 与 SDK `LIB`/`INCLUDE`
  缺失）。注意 Git 自带 coreutils 的 `link.exe` 会顶掉 MSVC 链接器，Windows System32 的
  `sort.exe`/`find.exe` 会顶掉 GNU 工具——脚本已按 MSVC → Git usr/bin → 其余 排好序。
- 此 shell 的 `python` 受 pdm 的 `PYTHONPATH`（pep582）干扰会启动失败；
  文本处理优先用 bash/sed/perl，非要用 python 先排查环境。
- Rust stable MSVC（rustup，本机 1.99），**workspace MSRV 1.96**（openusd 0.8 要求）。
  GPU：RTX 3060 Ti（wgpu 走 DX12）。crates.io 可达。`lichen-vm` 只是远端名，**非虚拟机**，GPU 直通。
- `C:\resource` 下其它目录是别的项目，**不要改动**。

## 命令

```bash
source scripts/msvc-env.sh    # 每个新 shell 先跑这个
cargo test                    # 默认成员（rusd 各 crate，不含 spike）
cargo test --workspace        # 含 spike-openusd（会编译 openusd，首次较久）
cargo fmt --all               # 格式化（提交前必跑）
cargo clippy --workspace --all-targets
cargo run -p spike-openusd    # O6 增量合成实证基准（SPEC §6）
```

## 架构要点（决策见 SPEC §11）

- **D1** 表面语法 = TOML 1.0 的 Rusd profile；解析/编辑走 `toml_edit` CST，保注释保空白。
- **D2** 增量管线为一等需求（SPEC §6），"增量 ≡ 全量"是硬不变量。
- **D3** 场景/合成/互操作后端 = **openusd 0.8**（纯 Rust，已过 AOUSD 合规基线，增量合成已实证）。
  `rusd-scene` 是其上的薄适配层，不要自建合成引擎或 USDA 解析。openusd 1.0 前 API 不稳定：
  适配层隔离、pin 精确版本、升级先跑 spike 回归。

## 约定

- **确定性**：输出路径、序列化、展开结果不得依赖 HashMap 迭代序；用 BTreeMap 或显式排序。
- **诊断 code 稳定**：`E_` / `W_` / `N_` 前缀；发布过的 code 不复用、不改语义。
- **agent 循环里不 panic**：渲染 / 校验 / 展开失败一律返回诊断，让调用方能自我纠正。
- **语法改动先改 SPEC**，再改实现与测试；每个语法规则配往返测试。
- 过程化输出必须可复现：RNG 只从 `(stage.seed, prim_path, local_seed)` 派生。
- 提交信息：`<crate>: <做了什么>`，小步提交；提交前 fmt + test 全绿。