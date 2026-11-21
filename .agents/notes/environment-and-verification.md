# 环境 / 构建 / 验证

> 2026-11-21 做梦合并（二轮：实证更正机器拓扑与部署链）。canonical 仍以 `AGENTS.md` 与 `docs/REMOTE.md` 为准；本文件是去重后的速查。

## 机器与边界（已实证更正）
- **lichen-vm**（`Admin@10.144.144.2`，exec 默认）：**无头构建/渲染/serve 机**。shell 在 Session 0、无 explorer；GPU RTX 3060 Ti 直通，非虚拟机。仓库 `C:/resource/rusd`、serve 在此。**它没有给用户看的桌面，不要在这起 viewer。**
- **gongga server 机**（`exec remote:none`，用户 `yanyangtheselec\17124`，`C:/Users/17124`）：Session 1 有 explorer/dwm，**这才是用户桌面**；viewer 窗口在此显示（`C:/Users/17124/rusd-client/`）。`ctl.py`/`check_rows.py`/截图点击脚本都在这台。
- `remote:none` 的 exec **可用**（本地通道，Session 1）。所谓 server build `35db…` 与 harness `62dd…` 不匹配**只影响 `run_skill` 的协议判定，不影响本地 exec**（一轮曾误记「暂不可达」，已更正）。
- `C:/resource` 下其它目录是别的项目，不要改动（gongga 的 `docs/notes/` 属于它，不动）。
- SSH 坐标（server→peer）：`ssh -i C:/Users/17124/.ssh/id_ed25519 Admin@10.144.144.2`；runner `C:/Users/Admin/.gongga/bin/gongga-608aef29bd723eee.exe runner`（见 server `~/.agents/remotes.toml`）。

## 构建（在 lichen-vm）
- Git Bash：每个新 shell 先 `source scripts/msvc-env.sh`（补 Git usr/bin 与 MSVC 环境）。
- 或 powershell：`cmd /c '"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat" >nul && cargo …'`，cwd `C:/resource/rusd`。
- Rust stable MSVC，workspace MSRV 1.96。`cargo test` 默认成员；`--workspace` 含 spike-openusd。提交前 `cargo fmt` + test 全绿（注意既有 fmt 漂移，见 current-state）。
- 改 `rusd.exe` 前先停 serve 解 exe 锁（`Stop-Process -Name rusd`）。viewer 是独立二进制，改 viewer 不重链 `rusd.exe`。
- runner 楔住时经 SSH 从 server 构建/提交：`ssh … 'powershell -NoProfile -EncodedCommand <base64(UTF16LE)>'` 避开嵌套引号；scp 拉/推二进制（server 持 key）。

## python
- lichen-vm 系统 `python` 受 pdm 的 PYTHONPATH（pep582）干扰会启动失败；用 `C:/resource/rusd/python/.venv/Scripts/python.exe`。文本处理优先 bash/sed/perl。server 机 `python` 可直接跑 `ctl.py`。

## serve / viewer 部署链（REMOTE.md §15.3）
- **serve 在 lichen-vm**：`rusd.exe serve --listen 127.0.0.1:8766`（实测 PID 52708，protocol 1，NDJSON JSON-RPC；本地 hello 应答 `{protocol:1, session:"rusd-serve/52708", version:N}`）。
- **viewer 在 server 机**：`C:/Users/17124/rusd-client/rusd-viewer-debug.exe view --connect 127.0.0.1:8766 --control 127.0.0.1:8797`。
- **隧道**（server→peer）：`ssh -i … -o BatchMode=yes -o ServerAliveInterval=10 -o ServerAliveCountMax=6 -o TCPKeepAlive=yes -o ExitOnForwardFailure=yes -N -L 8766:127.0.0.1:8766 Admin@10.144.144.2`。server:8766 → peer:8766。长连 SSH 偶发掉线，keepalive + 守护兜底。
- 链：peer 构建 → 停旧 viewer 解 exe 锁 → scp viewer 到 server rusd-client → 建隧道 → server 起 viewer → `ctl.py` 载入/切换。
- **隧道守护**：`rusd-client/tunnel-watch.ps1` 后台循环（每 20s 查 8766，掉线即重建）；viewer 网络层**自动重连**（net.rs `reconnect()`，worker 每轮重试发 `Reconnected`），故只需保隧道存活。
- viewer 无 Open 入口：经 control `open path=<peer 路径>`（serve 解析），或 serve 广播。control 方法（`control::METHODS`）：`state/open/preview/tree/select/isolate/script/translate/fit/orbit/capture/shot`（**无 pick/key**）。`script{on}` 切伪代码视图；`capture` 只截 3D 视口（GPU readback），**不含 egui 面板**——看面板要整屏截图或 PrintWindow。
- **起 GUI**：在 server（交互 Session 1）`Start-Process … -RedirectStandardOutput/Error <log>` 正常开窗、不堵通道（子进程不随通道关闭被杀）。一轮「起 GUI 堵 runner」发生在无头的 lichen-vm，不是 server。

## 屏幕 / 截图 / 点击（server 机）
- 屏 2560×1440 @ 1.75。截图与鼠标归一化前先 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2 = -4)`，否则被虚拟化到 1463×823，造成「越界/被裁/点击偏移」假象。
- 脚本（rusd-client）：`shot.ps1`（DPI-aware 整屏截图）、`winshot.ps1 -procId <pid>`（PrintWindow 抓指定窗口，**锁屏下仍可抓**，flag 2 = PW_RENDERFULLCONTENT）、`bring-front.ps1 -proc rusd-viewer-debug`（AttachThreadInput 置前台；进程名是 `rusd-viewer-debug`，不是 `rusd-viewer`）、`click.ps1 x y`（DPI-aware 合成点击+截图）、`ctl.py`（control 8797，`RUSD_CTL` 可改端口）。
- 真窗点击按屏幕 px 校准（viewer 行距 ≈ 33px @ 2560）；`mouse_event` ABSOLUTE 用虚拟屏 GetSystemMetrics 76..79 归一。**锁屏时前景是 LockApp，置前台无效，需用户解锁。**
- egui-winit 需 `features = ["clipboard"]`，否则 `ctx.copy_text` 被静默丢弃；验收读真剪切板。

## 通道纪律
- `exec` 每次带 `bin=powershell`；通道一次性，程序结束即销毁，复用要重带 `bin`（否则「no channel … and no bin」）。
- 批内多个 exec 可能被串行执行；观测 client 走后台通道（wait 0）再单独发改写。
- `del_channel` 只杀通道里的 shell，GUI 孤儿可能续跑锁 exe；覆盖二进制前 `tasklist` 查孤儿。
- lichen-vm runner 楔住时新 exec 卡「still starting」，`ls_channel`/`del_channel` 可挂 120s；read/write/edit 文件工具仍可用，改用 SSH-from-server 绕行。
- .NET `[IO.File]::ReadAllBytes/ReadAllText` 用**进程 cwd**，不受 PowerShell `Set-Location` 影响——相对路径会落到 `C:/resource` 而非仓库；用绝对路径。

## 写盘可靠性（中文）
- write/edit 偶发丢字节：中文可现 U+FFFD，还有「合法但语义错」的码点翻转（如 ≡→⩡）。写后两步扫描：FFFD + 生僻码点白名单（见 AGENTS.md 的 perl 一行）。损坏多见于 flash 子 agent 的 write/edit。