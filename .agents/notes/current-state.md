# Rusd 当前状态

> 2026-11-21 做梦整理（二轮：实证更正机器拓扑与部署后重写）。合并自会话交接 / live_compact 与本轮实测，取代 `target/*.md` 散记。
> 语法与架构的单一事实来源是 `docs/SPEC.md`（决策见 §11）；本文件只记「现在到了哪」。机器边界、部署链与验证速查见 `environment-and-verification.md`。

## 仓库与 HEAD
- 仓库 `C:/resource/rusd`，远端名 `lichen-vm`（无头构建/serve 机）；**用户桌面是 gongga server 机**（`exec remote:none`），viewer 窗口在那边给用户看——拓扑详见 environment 注记（一轮曾误记 lichen-vm 为桌面，已更正）。
- HEAD `700be53`「viewer: a script control method naming the pseudocode toggle」，父 `86b3c72`「脚本伪代码视图」，祖父 `4c92f7d`「D15 文档」。工作树干净（仅未跟踪 `.agents/`）。
- `C:/resource` 下其它目录（gongga、dsh-* 等）是别的项目，不要改动。

## 已落地
- **D15**（算子 = Rust function，编译即类型校验）：`crates/rusd-script`（`build.rs` 从 USD manifest 生成 `IsA` 闭包；`ops/graph/usd/prim/schema/shape/runner`）。示例 `examples/box_scatter.rs`、`nesting.rs`。文档 `docs/SCRIPT.md`。
- **viewer 脚本伪代码视图**（`86b3c72`）：树面板「脚本」开关；`crates/rusd-viewer/src/script.rs` 纯函数 `rows(&CompositionTree) -> Vec<TreeNode>`，把层读成 `let Volume: Cube = material(..)` / `let Samples: Xform = sample_points(domain = Volume)`，按依赖序、按 slot 去重。点击变量走既有 `solo → preview` 管线渲染该 USD（行携带与树层行相同的层 id，未改 agent/protocol）。`examples/pseudocode.rs` 无头预览。
- **control `script{on}` 钩子**（`700be53`）：与 `isolate` 同构，「control 方法＝一次点击的名字」，翻转 `TreePanel.script_mode` 并 `request_redraw`；登记进 `control::METHODS`（11→12）。`cargo test -p rusd-viewer` 8/8 绿。
- **部署到用户桌面并证实渲染**：新 debug viewer scp 到 server `C:/Users/17124/rusd-client/rusd-viewer-debug.exe`（字节校验一致），经 SSH 隧道连 lichen-vm serve；`ctl.py open path=…/box_scatter.ops.toml` + `script on=true`。PrintWindow（PW_RENDERFULLCONTENT=2）穿透锁屏抓到窗口，伪代码面板正确显示：`material / sample_points / instances` 逐行 + 类型标注（Cube/Mesh/Xform）+ USD 输出注释，变量行带下划线可点。证据图 `target/win_view.png`。

## 未结案
- **3D 视口当前帧 0 prim / draws 0**：伪代码面板（交付主体）已证；「点变量 → 渲染 USD」的 3D round-trip 尚未真窗复核（视口可能被折叠，或快照提取的独立问题）。用户若点变量不出图再查。
- RusdBuilding / RusdBlockFill 无脚本签名也无输出形状；三个形状位仍空（apis 空集 / `Array{len}` / `Value` 叶子）；树 UI 不显示 apiSchemas。
- 预存编译警告（rusd-proc / rusd-agent 的 unused / dead_code）可选清理。
- 既有提交并非完全 fmt-clean：`cargo fmt -p rusd-viewer` 会改 8 个无关文件（cache/gpu/lib/net/panel/script/tree/pseudocode 的纯格式化漂移，如 `struct LayerNode {    /// …` 拆行）。本轮只提交目标两文件，无关漂移已 `git checkout` 还原；全 crate 重排留作独立提交。

## lichen-vm runner 楔住（未恢复，已绕行）
- 症状：新 exec 通道卡「still starting there; nothing has been said yet」，`ls_channel`/`del_channel` 可挂满 120s；**read/write/edit 文件工具仍可用**。起因疑似上轮经 exec 起长驻 GUI 堵了 runner 进程树（未证实）。
- **绕行**：server 持 SSH key，`exec remote:none` 经 `ssh -i C:/Users/17124/.ssh/id_ed25519 Admin@10.144.144.2` 直驱 peer（短命令正常）；嵌套引号用 `powershell -EncodedCommand <base64(UTF16LE)>`。构建/提交/serve 诊断均经此完成，功能不依赖 runner。
- 恢复 runner 本身（重启 peer 上的 gongga runner 进程）尚未做，留待用户裁定。

## 「没看到」根因（已查清并修复前三层）
1. 用户桌面上跑的是**旧 viewer 二进制**（11/20 18:20，无脚本视图）且**隧道已断**（connected:false，E_IO 10054）。
2. 上轮把新 viewer 误起在**无头的 lichen-vm**（Session 0、无桌面）。
3. 用户屏幕一度**锁屏**（LockApp 前景）——需用户解锁，无法也不应绕过。
- 修复：重编含功能的二进制 → scp 到 server → 重建健壮隧道（+守护循环）→ 重启 viewer 连上（connected:true）→ 载入文档切脚本视图。viewer 网络层自动重连（net.rs `reconnect()`）。二轮整理时用户已解锁，viewer 窗口（PID 77456，标题 rusd-viewer）在桌面正常运行。