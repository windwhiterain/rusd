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

## wgpu 30 API 注意（M4 渲染器，已踩平）

- `Instance::new` 收**值**（非引用）；`InstanceDescriptor` 无 Default，用
  `..InstanceDescriptor::new_without_display_handle()`。
- `RequestAdapterOptions` / `DeviceDescriptor` 有 Default，可 `..Default::default()`。
- `RenderPassColorAttachment` 需 `depth_slice: None`；`RenderPassDescriptor` /
  `RenderPipelineDescriptor` 需 `multiview_mask: None`（不是 multiview）。
- `DepthStencilState.depth_write_enabled` / `depth_compare` 是 `Option`；
  `VertexState.buffers` 元素是 `Option<VertexBufferLayout>`。
- `PipelineLayoutDescriptor.bind_group_layouts` 元素是 `Option<&BindGroupLayout>`；
  无 `push_constant_ranges`，改 `immediate_size: 0`。
- 两个 pipeline 共用 bind group ⇒ 必须显式共享 BindGroupLayout + PipelineLayout
  （`layout: None` 的自动布局互斥，跨 pipeline 用会 validation error）。
- copy 用 `TexelCopyTextureInfo` / `TexelCopyBufferInfo` / `TexelCopyBufferLayout`；
  `device.poll(wgpu::PollType::wait_indefinitely())`；`get_mapped_range()` 返回 Result。
- readback 行距须对齐 `COPY_BYTES_PER_ROW_ALIGNMENT`(256)，读回按行去 padding。
- prim-id pass 用 `R32Uint` 渲染目标 + `@interpolate(flat)` u32，pick 读精确整数 id。
- 相机：Z-up look-at + 透视，wgpu clip z 在 [0,1]（near→0, far→1），depth_compare Less。

## 会话工具坑（已踩平）

- write/edit 偶发丢字节：中文落盘可现 U+FFFD（docs-m6 第一/三轮实测）。写后兜底：
  `perl -CSD -ne 'print "$ARGV:$.\n" if /\x{FFFD}/' <files>`
  （peer locale 非 UTF-8，`grep -P` 拒绝跑）；按字节定位修回。
- perl 双引号串里写 `\x{...}` 转义要小心：单反斜杠会被插值成宽字符，触发
  "Wide character in print" 并把整串中文按 Latin-1 升级双重编码成 mojibake。
  追加中文段落用 write 产字节 + `cat >>` 拼接，别让 perl 插值层碰中文。
- edit 工具在本类会话曾报参数编码错误：整文件 write 或 perl 替代；跨行替换用 `perl -0777`。
- 子 agent turn_end 通知正文偶发乱码：验收以磁盘文件与实测为准，通知只作线索。
- 丢字节不止 U+FFFD：还有中间字节翻转成「合法但语义错」的字符（实测 `≡` e2 89 a1
  变成 `⩡` e2 a9 a1），FFFD 检查完全漏掉。中文/数学符号写后第二步兜底——生僻码点
  白名单扫描：
  `perl -CSD -ne 'while (/([^\x00-\x7F])/g) { my $ch=$1; next if $ch =~ /[\x{4E00}-\x{9FFF}\x{3000}-\x{303F}\x{FF00}-\x{FFEF}]/; $c{$ch}++ } END { print map { sprintf("U+%04X %s x%d\n", ord($_), $_, $c{$_}) } sort keys %c }' <files>`
  输出应只剩 § × Δ µ · ≡ ⇒ → ← − … — “ ” ① ② ③ ─ ✓ 这类合法字符；出现生僻码点
  即损坏，按字节 perl 修回后再扫一遍（AGENTS 本条自带 ⩡ 作反例，扫描时豁免）。
- 损坏集中在 flash 子 agent 的 write/edit 调用（父会话 write 多轮实测干净）：疑似
  模型侧 tool-call 多字节序列化缺陷，而非写盘逻辑。故子 agent 交付的中文文档必须
  过 FFFD + 生僻码点两步扫描才验收。
- 时序实验的 harness 纪律：批内多个 exec 调用**可能被串行执行**（观测 client 的
  wait 会先把批次占住，改写调用排在它后面）——曾因此把「改写落在观测窗口外」误判为
  「server 延迟 10–20s」。观测 client 一律走**后台通道**（wait 0）再单独发改写调用；
  跨进程时间轴用 epoch ms 对齐，别信单侧 dt。
- `del_channel` 只终止通道里的 shell，**GUI/长驻子进程可能成孤儿继续跑**（实测
  rusd-viewer 窗口进程在通道结束后仍锁着 exe，导致 scp 覆盖报 Broken pipe）。
  覆盖二进制或重开窗口前先 `tasklist | grep <name>` 查孤儿；只清自己起的 smoke
  进程，子 agent 的进程按协调规矩处理。
- 本机屏幕 2560×1440 @ scale 1.75。**DPI-unaware 的 powershell/python 做截图与鼠标
  归一化会被 Windows 虚拟化到 1463×823**，曾造成「窗口越界/城市被裁/点击偏移」一连串
  假象（viewer 本身布局一直正确）。任何屏幕测量或合成输入前先
  `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2 = -4)`；`mouse_event` ABSOLUTE
  用虚拟屏归一化（GetSystemMetrics 76..79）；截图用 aware 后的 PrimaryScreen.Bounds。
- PowerShell 自定义函数名避开内建别名：`Move`/`Mv` 都是 Move-Item（大小写不敏感），
  曾让合成拖选静默失效、mask 全黑被误判为产品 bug。
- viewer 开窗按 monitor **work area**（GetMonitorInfoW rcWork）夹取居中，底栏不被
  任务栏遮；egui-winit 必须 `features = ["clipboard"]`，否则 `ctx.copy_text` 被平台层
  静默丢弃（toast 会显示「已拷贝」但剪切板不变——验收要读真剪切板）。
