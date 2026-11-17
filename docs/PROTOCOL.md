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
  v1 client 只发一种：`field="xformOp:translate"`、`value=[x,y,z]` 三个数的 JSON 数组。

### `opinion` 的 `value` 语义（父会话裁定，2026-11-17，第二轮）

`value` = **该 field 的 Sdf 值原样**，由 server 按合成的属性声明类型写进 overrides
session layer 的同名 op（实现见 `rusd-agent` 的 `apply_opinion`/`json_to_sdf`）。因此：

- **`xformOp:translate` 的 `value` 是 prim 的局部坐标**（父坐标系下的 op 值），**不是**世界坐标。
  server 现状即原样写入；集成测试断言写进去的 translate 等于覆盖值。
- **世界 → 局部的转换归 client**：gizmo 的世界目标须经**父 prim 世界矩阵之逆**变换后再发
  （旋转/缩放的一般情形也是逆矩阵，不只是减平移）。client 实现：`rusd-viewer` 的
  `gizmo::drag_translate`（delta 形，按压瞬间不加漂移）/ `gizmo::world_to_local`（绝对形），
  锚点由 `anchor::Anchor` 从合成 stage 读（子代 local translate + 父世界矩阵）。
- **理由**：lot 挂在带 translate 的 block 下，直接写世界直会重演「原点加两遍」类错误
  （M6 `fix/ground-size` 的根因）。
- client 若持有绝对世界目标，用绝对形；拖拽本身用 delta 形，使几何中心不在局部原点的 prim
  不会在按下瞬间跳动。
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

## v1 实现语义补记（2026-11-17，实现阶段确立；父会话裁定，与上文同效力）

上文是形状契约；这一节补记 v1 实现把形状之外必须定下来的语义钉死的地方。写代码的
一方与写 client 的一方都以本节为准；本节与上文冲突时以本节为先，并回头改上文。

### 启动与监听

- `rusd serve` **无 `--listen`** ⇒ stdio（M5 行为不变）。
- `rusd serve --listen`（裸 flag，无地址）⇒ 绑上文默认 `127.0.0.1:8765`。
- `rusd serve --listen <addr:port>` ⇒ 绑该地址；`0` 端口可用于 ephemeral（测试）。
  端口被占时报错退出（exit 2），不静默换端口。

### subscribe 补推当前快照

`subscribe{client}` 成功后，server **除** `{version}` 响应外，**立即向该连接补推一条
`stage_snapshot`**（若已有 stage）。理由：协议没有"拉取当前快照"的方法，而快照只在
mutation 时推；否则"先 open、后 subscribe"的 client 永远拿不到当前状态。

- 补推发生在 `subscribe` 的**响应之后**，形状与普通 `stage_snapshot` 相同，client 的
  version 丢弃规则原样适用（不会与后续广播重复生效）。
- 尚无 stage（未 `open`）时不补推，只有 `{version}`。

### 消息顺序

一次 mutation 引起的消息，与触发它的请求的相对顺序固定为：

```
响应（request 的 result/error） → stage_snapshot → diagnostics（有则） → state_changed
```

即"先答请求，再报后果"。理由：客户端按请求 id 等待结果时不会先消费掉一串大快照；
把因果关系摆在响应之后也让"这个快照是谁引起的"一目了然。

### 合成失败的 mutation

mutation（open/patch/opinion/undo/redo/reload）若解析/校验/展平失败，**只广播
`diagnostics`，不推进 `version`、不发 `state_changed`、不发 `stage_snapshot`**。

理由：没有可快照的 stage，发一个 `state_changed{version:n}` 会让所有 client 卡在
"等 version n 的快照"而永不到达。client 因此**只能**用 `state_changed` 判断版本推进。

### 版本与 client 侧丢弃

`version` 仍是单调递增 u64。注意一个由此产生的细节：失败 mutation 不占版本号，所以
client 见到的版本序列没有空洞——除非它错过了某条通知（断线、unsubscribe），此时
整体跳过若干版本是正常的，client 不做连续性断言、只做"丢弃 ≤ 已见最大值"。

### hello

- `session` = `rusd-serve/<pid>`（进程身份，非用户会话名；多 session 由 server 实例区分）。
- `version` = 当前权威版本。
- **协议不匹配**（`protocol` 不是 1）⇒ JSON-RPC 错误 `-32602 invalid params`，message
  写明"this server speaks protocol 1, not N"。`protocol` 缺失同样是 `-32602`。

### 热重载（`reason:"reload"`）

- server 在 `open{path}` 成功后，对该**文件路径**加 `notify` 文件监视（非递归，只盯
  该文件，不盯目录）；`open{content}` 无文件可盯，不加监视。
- 事件 **debounce ~200ms**（一次保存在磁盘上通常是多次事件：截断、写入、改名、touch），
  静默期过后 server 重读文件；内容与当前文档**逐字相同则不发**任何消息。
- 内容不同 ⇒ 记为一次文档 mutation（push 文档快照到 undo 栈、清 redo），重建 stage、
  重放 overrides、cook，按"消息顺序"节广播 `stage_snapshot` +
  `state_changed{reason:"reload"}`。
- `open` 换到另一个路径 ⇒ 旧监视撤销、新监视建立。文件在保存中途短暂不可读不计错误，
  等下一次事件。
- **任何一次事件交付都触发重读，包括错误交付**（Windows 后端在事件突发时会用错误
  形态报缓冲区溢出）。丢掉错误交付就会让 reload 等到下一次保存才发生，这是实测
  过的故障形态，所以不按 `is_ok()` 过滤。
- **延迟预算**：从落盘到收到 `state_changed{"reason":"reload"}` 的实测值是
  **634ms / 786ms**（peer x64，debug 构建，city 475 prim：200ms debounce +
  重建/cook/flatten/usdc 写）。回归测试以 **2s** 为预算断言。release 构建下管线
  显著更快，同样的 debounce 仍是主要固定开销。
- 该监视**不**违反 REMOTE §9 G4：那条说的是 openusd crate **无 stage reload / 无外部变更
  探测**（client 镜像侧的限制），不是禁止 server 侧文件 watch——重载由 server 重读文档 +
  重建 stage 完成，crate 缺口不参与。

### 编辑面与重放

- `opinion` 就地编辑 overrides session layer 后**只 re-cook**（D6 memo 命中），不重建 stage。
- `open`/`patch`/`undo`/`redo`/`reload` 改变文档 ⇒ **重建 stage** 并把已记录的 overrides
  重放进新 stage 的 session layer（REMOTE §5/G3：crate 无 reload/rebind，session 仅 builder 期）。
- `undo`/`redo` 覆盖**两个**编辑面（文档快照 / overrides 列表），单一撤销栈按发生顺序。
- opinions 是"当前生效集合"（每个 `(prim, field)` 一条）：重复 `set` 覆盖、`remove` 删除；
  重放该集合即重建 session layer。

## 验收（M4' spike③ / 实现期）

loopback 集成测试：open city → subscribe → patch ⇒ client 收 version+1 快照且本地
合成 bbox 随之变化；opinion translate ⇒ 快照反映覆盖；undo ⇒ 恢复；改写文档文件 ⇒
收 `reload` 快照且 client 侧合成几何随之变化。egui 壳人工 smoke 在用户机经隧道跑
（父会话执行）。