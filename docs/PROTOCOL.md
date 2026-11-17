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
- **debounce 的到期检查不看 `recv_timeout`**：sync 循环**每轮 `recv` 前先查
  `deadline_passed()`**（deadline 已过期就就地发布 reload），而不是等 `recv_timeout`
  报超时才算到期。理由：持续 client 流量（viewer 边动画边每 200µs 发 `state`）会让每次
  `recv_timeout` 立即返回，若只靠超时认定到期，reload 就会被**无界推迟到流量停止**。
  修复见 `f88af5f` ← `7f585ba`；证据与缺口详见 `docs/REMOTE.md` §10「热重载延迟：证据与缺口」。
- **回归未锁住饥饿（诚实注记）**：busy 回归测试做过 red-check——撤掉 `deadline_passed()`
  修复后它仍 **646ms 绿**，说明它验证"busy 下能收到 reload"而**不是**"没有饥饿"。
  父会话曾观测到一次 **15.59s** 延迟，但 release/debug × 无/idle/busy **六种组合均未复现**；
  早先"reload 延迟 10–20s"的结论**已撤回**（批内 exec 串行致改写落在观测窗口外的 harness
  错觉），15.59s 保留为**未解释的单次观测**。锁饥饿需负载注入式测试基础设施，列为后置可选。
- **延迟预算**：从落盘到收到 `state_changed{"reason":"reload"}` 的实测值是
  **634ms / 786ms**（peer x64，debug 构建，city 475 prim：200ms debounce +
  重建/cook/flatten/usdc 写）。回归测试以 **2s** 为预算断言。release 构建下管线
  显著更快（`release` `8767`：无 client 附着 — / idle 附着 **252.7ms** / busy 附着
  **256.6ms**；debug 对照 A **748ms** / B **911.6ms**），四层时间轴逐层均为毫秒级，
  同样的 debounce 仍是主要固定开销。
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
（父会话执行）。## v1.1 增补（D13：合成图进 viewer + 视觉引用 capture；2026-11-17 父会话定契约）

### 合成图与 prim 检视

- client 用**本地 composed stage** 建 prim 树（含非几何 prim：Xform / Scope / Camera /
  Prototypes / class prim），点击节点 = 选中 + 检视面板（composed 属性、世界变换、bbox）。
  树不依赖 server 推送：快照到达即重建。
- **点选 prim 自动把 prim path 拷贝到剪切板**（egui ctx.copy_text），并 toast 提示
  「已拷贝 <path>」；prim path 即对话中引用该 prim 的 id。
- `prim_info{path}` → `{path, type, specifier, active, instanceable, children:[path],
  attrs:[{name, value, authored_by:[layer identifier]}], arcs:[{kind, target}],
  world_transform:[16], bbox:{min,max}|null}`。
  来源 = **server 权威 stack**（Tier B-lite 查询）：flatten 快照丢 arcs/instanceable，
  provenance 必须问 server。`authored_by` 按 opinion 强度序。crate 若无公开 opinion-source
  API，`authored_by` 回 `[]` 并在响应加 `provenance:"unavailable"` 注记，不许自创。
- server 未实现该方法时 client 降级：只显示 composed 值（method not found = -32601）。

### 视觉引用 capture

- `capture{camera:{position,look_at,up,fov_deg}, image:<base64 png>,
  mask:<base64 png>|null, scene_version, note?}` → `{id}`。
  mask 与 image **同尺寸**，白 = 选区、黑 = 其余；矩形选区为 MVP。尺寸不一致 = -32602。
- **capture 成功后 id 自动拷贝到剪切板** + toast「已拷贝 <id>」；用户随后在与 agent
  的对话中直接粘贴引用该 id。
- server 存 `<cwd>/target/captures/<id>.png / <id>.mask.png / <id>.meta.json`
  （gitignored）；bind 时扫目录重建索引；id = `cap-<seq:04d>` 单调递增（重启续号）。
- `capture_get{id}` → `{id, camera, image, mask, scene_version, created_at, note}`；
  未知 id = -32602（message 写明）。`capture_list{}` → `[{id, created_at,
  scene_version, note, has_mask}]`。
- agent 经 TCP 或 stdio 同协议取回，**在对话中以 id 引用**；相机位姿 = client 截图瞬间
  orbit 相机（世界绝对），agent 可以同位姿 server 重渲或据 mask 定位 region。
- capture 不推进 scene version（不是场景 mutation）；不广播通知。

### 验收

- server：capture 往返 / mask 尺寸校验 / 未知 id / list / 重启索引重建；prim_info 对
  overrides session layer 覆盖过的 prim 须列出该 session layer。
- client（后续轮）：树面板点击选中 + 检视 + **path 自动拷贝**；区域拖选 → mask →
  capture → **id 自动拷贝** + toast；离线（--usdc）时 capture 仍可用但
  scene_version=0、prim_info 降级。剪切板拷贝须有 headless 可测的纯函数出口
  （copy payload 计算与 egui 调用分离）。
## v1.2 增补（D13 修正：合成树 ≠ prim 树；2026-11-17 用户裁定，取代 v1.1 合成图节）

**用户裁定：viewer 要的是 USD 合成树（composition tree），不是 composed prim 树。**
flatten 快照丢 arcs/instanceable/variant 结构 ⇒ 合成树**不能**从 client 本地快照建；
由 **server 从权威 stack 的 sdf layer specs 构建**（arc/variantSet 皆是 sdf 字段，公开
API 可读，不依赖 pcp 内部），序列化后供 client 画树。Tier B 本地镜像仍后置。

**v1.1 的「prim 树面板」与 `prim_info` 撤销**，以本节为准；capture 节不变。

### 方法

- `composition_tree{}` → `{layers:[{id, kind, strength}], arcs:[{id, kind,
  source_layer, source_prim, target_layer, target_prim}], prim_nodes:[{id, path,
  authored_in:[layer id 按 strength 序]}], variant_sets:[{id, prim, name,
  variants:[name], selection}]}`。
  kind ∈ root/session/sublayer/reference/payload/inherit/specialize/variant。
  layers 含 server resolver 能开到的全部层（含外部 reference 目标层）。
- `comp_node{id}` → 详情 union：
  - layer → `{identifier, usda_text, spec_count}`（export_to_string）；
  - arc → `{kind, source_layer, source_prim, target_layer, target_prim,
    target_spec_fields:[{name, value}]}`；
  - prim → `{path, opinions:[{layer, specifier, fields:[{name, value}]}] 按
    strength 序, arcs_out:[arc id], variant_sets:[id]}`；
  - variant_set → `{prim, name, variants, selection, per_variant:[{variant,
    authored_in:[layer id]}]}`。
  未知 id = -32602。

### client UI（后续轮）

- 树面板根 = layer stack（strength 序，session 最强）；arc 作边/子节点指向目标层；
  prim 节点挂在作者层下；variant_set/variant 可展开。**点击任何节点** = 选中 +
  `comp_node` 详情面板（layer 的 usda 文本 / arc 的目标 spec / prim 的逐层 opinions /
  variant 选择）。
- 点选 prim 节点仍自动拷贝 prim path 到剪切板 + toast（v1.1 剪切板条款保留）。
- server 未实现时面板显示「合成树不可用（server 版本）」降级，不崩。

### 验收（server 侧并入 capture-server 轮）

- composition_tree 对含 reference/prototypes 的 city 栈：layers 含 cook/root/session，
  arcs 含 reference 到 prototypes，prim_nodes 的 authored_in 强度序正确；
- comp_node 四类详情各一测试；overrides 覆盖过的 prim 的 opinions 首条 = session layer；
- 未知 id -32602。

## v1.3 增补（prim 节点移出树面板；2026-11-17 用户裁定，取代 v1.2 的「prim 节点挂在作者层下」）

**用户裁定：合成树面板不画 prim 节点列表（「太多了没有用」）。**

city 这样的栈有数百个 prim，逐个列进树里既撑爆面板宽度又没人读；用户要的是
**层与结构**。因此：

### 树面板只画三类行

1. **layers** —— 按 strength 序（session 最强）；
2. **arcs** —— 作边/命名行指向目标层（cross-layer 的目标层作其叶子行）；
3. **variant_sets / variants** —— 可展开。

**prim 行不再出现在树里。** `composition_tree` 的 payload 与 server **不动**，
`prim_nodes` 仍在响应里（其余消费者与 `authored_in` 校验照旧）；只是 client 不渲染它。

### prim 检视的入口改为 viewport 点选

```
viewport 点选 pick → prim path
  → 在 composition_tree 的 prim_nodes 里按 path 查 id
  → comp_node{id}   （详情面板）
```

- 点选仍自动拷贝 prim path 到剪切板 + toast（v1.1 条款保留）。
- path 在 `prim_nodes` 里查不到（例如该 prim 来自 cook 的 flatten，或 server 的报告
  尚未到达）时，详情面板说明这一点，不报错、不崩。
- 树面板点击仍走 `comp_node{id}`（layer / arc / variant_set 三类），行为不变。

### 理由

面板变窄 ⇒ viewport 可用空间变大；同时把「检视哪个 prim」交给用户实际指着的那一个，
而不是让人在几百行里找一个 path。
