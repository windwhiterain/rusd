# D12 wire protocol v1（Tier A：快照镜像）

状态：定稿 · 2026-11-17 · 对应 SPEC §11 D12、docs/REMOTE.md。M4' 实现契约。

## 传输

- TCP，NDJSON（每行一个 JSON 对象）。server 默认绑 `127.0.0.1:8765`（peer）；
  用户机经 `ssh -L 8765:127.0.0.1:8765 Admin@10.144.144.2` 隧道连 `127.0.0.1:8765`。
- `--bind <addr>` 开放 LAN 属后置项，连带认证决策；v1 不做认证（隧道即边界）。
- JSON-RPC 形状：请求 `{"id":n,"method":m,"params":{…}}`；响应
  `{"id":n,"result":{…}}` 或 `{"id":n,"error":{"code":c,"message":s}}`；
  通知 `{"method":m,"params":{…}}`（无 id）。错误码用 JSON-RPC 标准
  （-32601 method not found、-32602 invalid params、-32000 server error）。

## 方法（复用 rusd-agent 工具名，语义不变）

`open{path}`、`query{…}`、`validate`、`expand`、`render{…}`、`pick{…}`、
`patch{ops}`、`diff`、`undo`、`redo`、`export{…}`、`state`。
这些供 agent client（Python/stdio 之外的 TCP 前端）使用；**viewer 不走
render/pick**——它在本地合成、本地渲染、本地拾取（D12 核心）。

## viewer 专用

- `hello{client, protocol:1}` → `{protocol:1, session, version}`。
- `subscribe{client}` → `{version}`；此后该连接收通知。`unsubscribe{client}`。
- `opinion{ops:[{path, field, value, op:"set"|"remove"}]}` → `{version}`。
  落点 = server 的 **overrides session layer**（D12 §3 第三编辑面），不是文档 CST。
  gizmo 拖动节流 ≤30Hz 发 opinion；server 作者后重合成并广播。
  **`value` 的形状由字段的 Sdf 类型决定**（本协议不另定壳）；`remove` 不带 `value`。
  v1 client 只发一种：`field="xformOp:translate"`、`value=[x,y,z]` 三个数的 JSON 数组
  （世界坐标；server 若需相对父变换，按自己的 session layer 语义落成 `point3f`）。
  **待 server 侧确认**：本行是 client 侧的唯一假设，若 server 选别的 enc（如 `{x,y,z}`）只改
  `protocol::OpinionOp::set_translate` 一处。
- `undo`/`redo` 覆盖文档 CST 与 overrides 两个编辑面（D11 + D12 §3）。

## 通知（server → 订阅者）

- `stage_snapshot{version, format:"usdc", data:<base64>}`：flattened usdc 全快照
  （Tier A）。自足：flatten 烘资产、丢 arcs ⇒ client `Layer::from_bytes` 单层栈即可，
  匿名安全（REMOTE §4）。city 实测 58KB。
- `state_changed{version, reason:"open"|"patch"|"opinion"|"undo"|"redo"|"reload"}`。
- `diagnostics{items:[…]}`（cook/validate 诊断，形状同 agent state 里的 diags）。

## 规则

- `version` 单调递增 u64；client 丢弃 version ≤ 已见最大值的快照/通知。
- 每次 mutation（open/patch/opinion/undo/redo/文件热重载）后 server cook（缓存命中
  跳过）→ flatten → `write_bytes` → 向**所有**订阅者广播 stage_snapshot + state_changed。
- 相机归 client：任何消息都不携带相机；client 本地 orbit/pan/zoom 只触发本地重渲。
- pick/annotate 全本地（prim-id pass + 描边），零网络往返。
- 线程：client sync 线程独占 `Stage`（`Rc` ⇒ `!Send`），owned 几何经 channel 交
  渲染线程（REMOTE §7）。
- 确定性：双端 openusd 0.8 pin、同 win-x64；wire 只 usda/usdc 不用 usdz；
  匿名 id 不上 wire（Tier A 快照是匿名层但自足，identifier 不序列化）。

## Tier B（后置，替换 stage_snapshot）

`layers_changed{version, root, session, layers:[{identifier, encoding:"usda"|"usdc",
data}]}`：命名 identifier 逐层推送；client 自定义 `ar::Resolver` +
`builder().resolver().session_layer().open()` 镜像，layer 变更 = 重建 stage。
细粒度增量（`Diff`/`apply_diff` + 自定义 codec）再后置。v1 不实现。

## 验收（M4' spike③ / 实现期）

loopback 集成测试：open city → subscribe → patch ⇒ client 收 version+1 快照且本地
合成 bbox 随之变化；opinion translate ⇒ 快照反映覆盖；undo ⇒ 恢复。egui 壳人工
smoke 在用户机经隧道跑（父会话执行）。
