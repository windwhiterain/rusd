# .agents/notes — Rusd 会话记忆

2026-11-21 做梦整理（二轮：实证更正机器拓扑——用户桌面是 gongga server 机而非 lichen-vm——并刷新部署进度）：把散落的草稿合并成有序记忆，去重，退役过期散记。
只整理 **Rusd** 项目；`C:/resource` 下 gongga 等别的项目的 `docs/notes/` 不动。

## 索引
- [`current-state.md`](current-state.md) — 当前 HEAD（700be53）、已落地（D15 / 伪代码视图 / control script 钩子 / 部署到用户桌面并证实渲染）、未结案、runner 楔住绕行、「没看到」根因。
- [`environment-and-verification.md`](environment-and-verification.md) — 机器边界（lichen-vm 无头 / server 机＝用户桌面）、构建、python、serve/viewer 部署链与隧道守护、屏幕截图点击、通道纪律、中文写盘可靠性。

## 散记退役（retired）
`target/*.md` 是 gitignore 的历史草稿，已被本目录与 canonical 文档取代，**不再作为记忆来源**：
- `cm-*.md`（约 30 个）：历次提交信息草稿 → 由 `git log` 取代。
- `protocol_v11.md … v17-protocol.md`：协议迭代草稿 → 由 `docs/PROTOCOL.md` 取代。
- `v15* / v151* / v153* / v154* / v155* / v156*-*.md`：版本路线 / 验证 / remote 草稿 → 由 `docs/ROADMAP.md`、`VERIFICATION.md`、`REMOTE.md` 取代。
- `agents_append*.md`：AGENTS.md 追加草稿 → 已并入 `AGENTS.md`。
其中 durable 的部分（部署链、DPI、通道纪律）已合并进 `environment-and-verification.md`，op 行 cook 序等规范见 canonical（`REMOTE.md` §15.2）；物理文件留在 `target/`（gitignore），按本索引视为已退役。

## canonical（不要在此重复其内容）
`docs/SPEC.md`（单一事实来源）、`PROTOCOL.md`、`REMOTE.md`、`ROADMAP.md`、`VERIFICATION.md`、`SCRIPT.md`、`GENERATORS.md`、`PIPELINE.md`、根 `AGENTS.md`。

## 约定
- 一个事实只在一处记；速查放本目录，规范放 canonical，历史放 git。
- 本目录是 agent 记忆，不是仓库文档；不做 git 提交。
