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
## v1.3 增补（D13 UI 裁定：合成树面板不画 prim 列表；2026-11-17 用户裁定）

**用户裁定：「不要显示 prim tree，太多了没有用。」** 合成树面板只画**合成结构**：
layers（strength 序，session 最强）+ arcs（边/命名行指向目标层）+ variant_sets/variants。
**不画 prim 节点列表**（city 475 行是噪声）。

- prim 的检视入口 = **viewport 点选**：pick 得 path → 按 path 查 `composition_tree`
  payload 里 prim_nodes 的 id → `comp_node{id}` 详情面板（逐层 opinions 强度序）。
  点选仍自动拷贝 prim path 到剪切板 + toast（v1.1 条款不变）。
- `composition_tree` / `comp_node` **payload 不变**（server 不动）；prim_nodes 仍返回
  供 path→id 映射与 provenance，只是不上树。
- 面板因此变窄，viewport 可用空间变大（与 v1.2 布局修复同轮交付）。

### 验收（client 侧）

- tree 模型/面板回归：断言渲染行只含 layer/arc/variant 三类，**无 prim 行**；
- pick→path→id→comp_node 详情链路测试（mock server）；
- 剪切板条款回归保持。
## v1.4 增补（D14 垂直切片契约：三 crate 并行切分；2026-11-18 父会话定）

切分：**rusd-proc**（几何进几何出）/ **rusd-agent**（前缀预览 server）/ **rusd-viewer**
（预览 client + overlay compact）。三者 crate 不相交，契约如下，各自对契约写测试。

### 前缀预览（server ↔ client）

- `preview{upto: <layer identifier> | null}`：**按连接**维护预览态。upto = cook op 输出层
  identifier（见 composition_tree layers）；null = 最终全量。未知 upto = -32602。
- server 合成前缀 = root + **cook 顺序中 ≤ upto 的 op sublayer** + session overrides；
  推 `stage_snapshot` 增可选字段 `upto`（缺省 = final）。preview 不推进 scene version。
- `composition_tree` 的 layers 条目增 `op: true` 标记 cook generator 输出层（kind 仍
  sublayer），viewer 依 strength 序画 op 行。
- client：op 行可点 → 发 preview{upto}；「final」行/按钮 → preview{null}；status/overlay
  显示当前 upto。点非 op 节点仍走 comp_node 详情，不改预览。

### 几何进几何出（proc）

- RusdScatter 增可选 `input_points = <prim path>`：输入层上的点几何（UsdGeom.Points 或
  point3f[] 属性，含世界变换）。有时：读点 → 在**自己的输出 layer** 写原型实例 prim
  （替换点语义：点留在输入层；前缀预览 scatter 之前见点、之后见实例）。
- 无 input_points 时保持现参数式回退，现有场景不破坏。
- 新增 examples/points_scatter.rusd（点层 + scatter op）。

### overlay compact（viewer）

- `--debug-ui` 保留但默认 ≤11pt 字号、≤460pt 宽、可折叠（键 `o` 或按钮）；
  面积 ≤ viewport 25%（纯函数回归）。

### 验收（各 crate）

- proc：点→实例 count/逐点世界变换一致；参数式回退回归保持。
- agent：前缀排除后续 op（city upto=rusd_city 层 ⇒ 无 scatter 树）；null 恢复；
  未知 upto -32602；preview 不动 version。
- viewer：op 行点击发 preview（mock server 断言 wire）；final 恢复；overlay 面积界；
  真窗终验归父会话。
## v1.4.1 增补（用户裁定：scatter 必须基于几何；2026-11-18）

**scatter 的分布必须来自输入几何，不是参数域。** 输入几何两种形态：

- **UsdGeom.Mesh**：按三角形**面积加权**采样（count + seed 参数；累积面积 + 重心坐标），
  实例位置落在输入 mesh 表面上。
- **Points / point3f[]**：逐点实例（含世界变换）。

**替换语义**：输出 layer 写原型实例 prim，**并写 visibility override 隐藏输入几何 prim**
⇒ 合成结果 scatter 之后只见实例；前缀预览 scatter 之前见输入几何。输入几何保留在输入层
（不删 spec，只 override visibility）。

确定性：路径键控 ChaCha8（与现 generator 同纪律）。参数式域 scatter 仅作无 input 时回退。

### 验收（proc）

- mesh 输入：实例数 == count；每个实例位置到输入 mesh 表面距离 ≈ 0（点在三角形内）；
- points 输入：逐点世界变换一致；
- 替换语义：scatter 后输入 prim 的 composed visibility = invisible；前缀之前 = visible；
- 同 seed 同输入 ⇒ 同输出（确定性回归）。
## v1.4.2 增补（切片裁定与集成语义；2026-11-18 父会话裁定）

### pcg-slice 偏差裁定（全部照准）

1. 替换语义用 **visibility="invisible"** 而非 active=false（active 被合成压制后弱层拿不回来）。
2. `input_points` 一个参数吃 Mesh 与 Points 两形态（probe 顺序 surface → cloud）。
3. 诊断 code `E_INPUT_GEOMETRY` 保留。
4. surface 模式 `kept = min(count, max_prims)`，超预算 `W_BUDGET`；cloud 模式截断保原序。
5. Mesh 的 surface 优先于其 vertices 点云（否则既不"在表面"也不加权）。
6. **rusd-scene 支撑照准**：Points schema / mesh 读取修复（此前 points 映射不到 USD 类型、
   索引落 int64[]，Mesh 根本不可作者——真缺口）/ world_points/world_triangles /
   extract 遵守 composed visibility。

### agent-preview 偏差裁定（照准）

1. 预览态**存 cook 顺序位置**而非 identifier（op 层是 anon id，rebuild 必换；位置跨
   patch/undo 稳定）。契约字面"按 identifier"改为"按 cook 顺序位置，快照报当前 identifier"。
2. `preview` 不向非订阅连接补推快照（快照是订阅的东西）。
3. **同 version 前缀帧的 accept 规则**（viewer 侧，session.rs apply 已实现）：
   stale = 旧于 floor，或（等于 floor 且与屏上合成相同）；**前缀不抬 floor**——否则下一次
   同 version 的真 mutation 会被误判 stale、画面永久冻住。floor 为 Option（无 floor 时
   version 0 是首帧而非重复）。

### 收口

- main 上 rusd-viewer/window.rs 的 fmt 漂移由父会话统一 `cargo fmt --all` 收口。
## v1.5 增补（用户裁定：viewer 要看到 mesh→points→instances 三段管线；2026-11-18）

**scatter 拆成两个显式 op**，中间点云必须是一个真实 layer，前缀预览才看得到中间态：

- `RusdSamplePoints`（mesh → points）：读输入 mesh，面积加权采样 count 个点（seed），
  在**自己的输出 layer** 写 Points prim（point3f[]，采样位置）。
- `RusdInstances`（points → instances）：读输入 points，逐点写原型实例到**自己的输出
  layer**，并对输入 points prim 写 visibility override（替换语义）。
- 链：作者 mesh 层 → sample op 层（points）→ instances op 层（instances）。
  `examples/mesh_scatter.rusd` 重写为两段 op；旧单步 RusdScatter 保留回退（city/forest 不动）。

### 管线可读性（agent ↔ viewer）

- `composition_tree` 的 op 层条目增：`op_name`（generator schema 名，如
  `RusdSamplePoints`/`RusdInstances`/`RusdScatter`）、`input`（输入 prim path 或层）、
  `output`（输出层 identifier）。非 op 层不带这些字段。
- viewer op 行标签 = `op_name`＋`input → output` 摘要（不再只显示 anon identifier）；
  行顺序 = cook 顺序；前缀预览语义不变（v1.4）。
- 三档前缀可见：upto=null 之前（root）= 输入 mesh；upto=sample 层 = 点云；
  upto=instances 层 = 实例（final 同）。

### 验收（各 crate）

- proc：sample 点数==count 且落在输入表面；instances 逐点变换一致；三段前缀几何断言
  （mesh / points / instances 互斥可见）；旧 RusdScatter 回退回归不变。
- agent：op_name/input/output 上 wire；cook 顺序正确；未知字段不破坏旧 client。
- viewer：op 行标签含 op_name 与 input→output；点击仍发 preview{upto}（mock 断言）。
## v1.5.1 增补（派活前裁定：钉死跨 crate 表面；2026-11-18 父会话裁定）

v1.5 的实现拆给三个并行 subagent（proc / agent / viewer）。下列裁定把跨 crate 表面钉死，
各 crate 只改自己的；偏离先报告父会话，不私自改契约。

### 参数与替换语义（proc）

- `RusdSamplePoints` 与 `RusdInstances` 的输入参数都拼作 `input_points = <prim path>`
  （与 `RusdScatter` 同名）。sample 只吃 **Mesh surface**（无 mesh 拓扑 ⇒ `E_INPUT_GEOMETRY`）；
  instances 只吃 **Points / point3f[] 点云**（无 points ⇒ `E_INPUT_GEOMETRY`）。
- sample 参数：`count` + `seed`（面积加权采样，纪律同 v1.4.1）；instances 参数：
  `prototype` + `scale` + `max_prims`（逐点实例，超预算截断保原序 + `W_BUDGET`），无 `count`。
- **替换语义逐 op 成立**：sample 的输出层对输入 mesh 写 `visibility = "invisible"` over；
  instances 的输出层对输入 points 写同样的 over。⇒ 三档前缀**互斥可见**：
  root = 仅 mesh；upto=sample = 仅点云；upto=instances = 仅实例。输入 spec 不删。
- 输出 prim 路径沿用 RusdScatter 现有约定（proc 内部一致）；旧单步 RusdScatter 与
  city/forest 示例不动。`examples/mesh_scatter.rusd` 重写为 sample → instances 两段 op。

### cook 暴露 op 元数据（proc → agent）

- `CookResult` 增 `pub ops: Vec<CookOp>`；`CookOp { pub id: String, pub generator: String,
  pub input: Option<String>, pub layer_id: String }`：
  - `id` = node prim path；`generator` = schema 名；`input` = `input_points` 参数值（无则 None）；
    `layer_id` = 该 node **当前**输出层 identifier。
  - `ops` = 本次 pass 后**所有有 memo 输出层的 node**（含 reused 的），按 cook 顺序（topo）。
    rebuild 后 layer_id 必换；调用方每次 cook 后整体替换，不得累积。
- 父会话已在 main 落了该 API 的机械 stub（最小填充），proc subagent 负责 generators、
  示例重写、三段前缀测试，以及 ops 的回归测试（含 reused 场景）。

### composition_tree join（agent）

- op 层条目增 `op_name` / `input` / `output`：join 键 = `CookOp.layer_id == 条目 id`；
  `op_name` = generator，`input` = input（None ⇒ JSON null），`output` = layer_id。
- join 缺失（手工构造的 stage 等）⇒ `op_name`/`input` = null，`output` = 条目 id。
  非 op 层不带三字段。additive：旧 client 忽略未知字段，wire 既有字段不改。
- agent 每次 cook 后保存最新 `CookResult.ops` 供 join；预览前缀不改变 ops（ops 是全量 cook 的）。

### op 行标签（viewer）

- 主标签 = `op_name`（null 时回退 anon identifier，现状行为）；
  副文本 = `{input} → {output 摘要}`：input null ⇒ `(parametric)`；output 摘要 = identifier
  截断 ≤ 20 字符 + `…`（不再整行 anon id）。
- 行序 = cook 顺序（现状已对）；点击仍发 `preview{upto: 该层 id}`，final 行/按钮语义不变。
- mock server 断言：标签含 op_name 与 input→output；点击 wire 不变。

### 验收补充

- proc：ops 含 reused node（增量 pass 后 ops 不丢旧 node）；三段前缀互斥可见（composed
  visibility 断言）；sample 点数==count 且落在输入表面；instances 逐点变换一致。
- agent：op_name/input/output 上 wire 且 join 正确（含 input=null 的参数式 scatter）；
  未知字段不破坏旧 client（既有 wire 回归）。
- viewer：op 行标签断言（op_name + input→output + parametric 回退 + anon 回退）。

## v1.5.2 增补（集成裁定：op 行绘制顺序；2026-11-18 父会话裁定）

**撤回 v1.5.1「行序 = cook 顺序（现状已对）」**：那是父会话的事实错误。真实 wire 的
`layers` 是 **strength 序**（session 0、root 1、instances 2、sample 3）——cook 每步把输出层
插到 root stack 的 position 0，越晚 cook 越强，所以 **cook 顺序 = op 行 strength 降序**。
v1.5 的 mock 夹具把 strength 关系摆反（拿升序当 cook 顺序），「cook order」测试测的是假形状，
真窗面板会把管线读反（instances 在 sample 上面）。

### 绘制顺序（viewer）

- 结构层（root/session/arc 目标）保持 **strength 序**，不动。
- **op 行之间按 strength 降序画**（= cook 顺序）：在 root_layers 的 strength 序上，对每一段
  最大连续 op 行就地反转；结构层位置不变。`root_layers` 本身仍报 strength 序（wire 不改）。
- 点击语义不变：op 行 → `preview{upto: 该层 id}`；final → `preview{null}`。

### 参数补正（proc）

- `RusdInstances` 补 `seed: U64`（通用 generator 参数，v1.5.1 起草疏漏；示例 `seed = 11` 保留）。
  `RusdSamplePoints` 的 `seed` 不变。两 op 的 RNG 仍只从 `(stage.seed, prim_path, local_seed)` 派生。

### 验收补充

- viewer：真实 strength 形状（op 行 strength 降序）的绘制顺序回归——mock 夹具必须摆成
  wire 的真实关系，不能拿升序当 cook 顺序（red-check：反转前该回归须红）。

## v1.5.3 增补（用户裁定：root 档必须可见；2026-11-18）

**合成图的每一档前缀都要看得到，包括「仅输入（root）」档**——合成 = root + cook ops +
session overrides；root 档 = 只有 root 与 overrides、不含任何 op。

### wire（agent）

- `preview{upto}` 新增合法值 = **当前 root 层 identifier**（`composition_tree` layers 里
  `kind = "root"` 那层的 id）。语义 = 「cook 顺序 ≤ root」⇒ 不含任何 op。快照 `upto` 字段报
  该 root id；回应 `{"upto": <root id>, "root": true}`。
- 预览态仍按 cook 顺序位置存：root 档 = **位置 -1**（patch/rebuild 后位置不变，identifier 可换）。
- 未知 upto 仍 -32602，错误信息列 op 层 id **与** root id。
- 其余不变：null = final、op id = 含到该 op、preview 不动 version、非订阅不补推。

### viewer

- **root 行（`kind = "root"` 的层行）成为预览控制行**（与 op 行同权）：点击发
  `preview{upto: <root id>}`；`preview == root id` 时高亮 + `≤` 标记。root 行的
  `comp_node` 详情让位（同 op 行先例：控制行点击即预览）。
- 其余结构行（session / arc 目标 / prim）仍走选择，不改预览；final 按钮语义不变。

### 验收补充

- agent：preview{upto: root id} ⇒ 快照 upto = root id、无 op 层、version 不动、跨 rebuild
  存活（位置 -1）、未知 id 仍 -32602。
- viewer：root 行点击发 preview{upto: root id}（mock 断言）+ 高亮断言 + 非 op 非 root 行
  仍选择。
- 真窗：点 root 行 ⇒ 只见输入 mesh（起伏地面）；sample / instances / final 照旧。

## v1.5.4 增补（用户裁定：合成树 reference 条目去重；2026-11-18）

**同一层里同源同目标的弧只画一行**：wire 的 `arcs` 仍**逐条上报**（无损，其它 client 可自取）；
聚合是 **client 展示层**的事。

- 聚合键 = `(kind, source_layer, target_layer, target_prim)`；`source_prim` **不参与**（它正是
  实例之间唯一不同的部分）。组内条数 N ≥ 2 时行标签后缀 `×N`；N = 1 不加后缀。
- 聚合只改**绘制**：层行/结构行/prim 行不受影响；`composition_tree` 的 `arcs` 计数与 wire 不变
  （面板头「242 arc」仍是事实计数）。
- 点击语义不变：该行仍选择**目标 prim**（`comp_node{目标}`），与今天点任一条 reference 行的
  结果一致。payload / inherit 等弧同理按此键聚合。
- 验收（viewer）：mock 树 240 条同键 reference 弧 ⇒ 渲染 1 行带 `×240`；不同键不合并；
  单击仍发 comp_node 目标的 wire 断言；既有 312 测试不破坏。

## v1.5.5 增补（用户裁定：合成图每个节点都要能看；2026-11-18）

**详情面板（detail panel）的规则：树里点任何一个节点，详情面板都必须显示这个节点自己的东西；
不允许点了没反应。** 分种类落定：

### 层行（session / root / sublayer / op）

- 发新请求 `layer{id: <层 identifier>}` → `{"id", "kind", "specs": [...]}`。`specs` 按层内
  spec 的 traversal 顺序，每项：`path`、`spec_type`（"def"/"over"/"class"/"variant_set" 等）、
  `type_name?`、`properties`（该 spec 作者的属性/字段名，如 `points`、`radius`、
  `xformOp:translate`）、`references`（该 spec 带的引用弧：`[{layer?, prim}]`）。
- 详情面板显示「层内容 · N 个 spec」，每行 `spec_type type_name path · properties…`，带引用的
  行尾 `→ 目标`；**层内容列表不聚合**（它就是层的事实内容，滚动查看）。
- 控制行（root/op 行）的那一次点击**同时**做两件事：视口换档（preview，不变）+ 详情换层内容
  （同一层，一次点击一个对象，不歧义）。
- 未知 id ⇒ -32602（列已开的层 id）。纯读：不动 version、不推快照、确定性输出。

### 弧行（reference / payload / inherit / sublayer…）

- 详情面板显示**这条弧本身**：kind、`source_layer:source_prim → target_layer:target_prim`；
  并显示**目标 prim 在目标层的 spec**（该弧指向的东西在被指层里写了什么——取自
  `comp_node{目标 prim}` 的对应层 opinion，不新增 wire）。
- 弧行点击**不改预览、不改视口选中**（它描述一条边，不是选一个 prim）。
- 去重后的弧行（v1.5.4 `×N`）详情报 N 与源 prim 范围（首/尾各一条）。

### prim 行 / variant set 行

- 不变：`comp_node{id}` 的 opinions 视图；prim 行点击同时视口选中（既有）。

### 验收

- agent：`layer{id}` 对每层返回完整 spec 集（root 层含 `/World/Terrain` 的 points 与
  `/World/Prototypes/Tuft` 的 Sphere/radius；instances 层含 240 个实例 spec 及各自到
  `/World/Prototypes/Tuft` 的 reference）；未知 id -32602；version 不动；同层同输出。
- viewer：mock 断言——点 root/op/sublayer 行发 `layer{id}` 且详情渲染 spec 列表；点弧行
  详情含 kind 与 source→target（发 `comp_node{目标}`，不发 layer、不改预览）；点 prim 行
  行为不变；既有测试不破坏。

### v1.5.5 修订（弧行详情只讲层，不涉 prim；同日父会话裁定）

弧行点击的详情**只讲层**：源层与目标层各取该弧端点 spec 的层内容（`layer{源层}` /
`layer{目标层}` 过滤到端点路径），并排显示「源层 · 端点 spec 的字段」与「目标层 · 端点
spec 的字段」——这条边在两层里各自的样子。**不发 `comp_node`、不选 prim、不改预览。**
上一节弧行里的「目标 prim 的目标层 spec（取 comp_node opinion）」**作废**。去重行（v1.5.4
`×N`）同样按两端层显示，并报 N 与源 prim 范围。

### v1.5.4 修订（去重行可展开到单个 mesh；同日用户裁定）

聚合行是**可展开**的：默认收起（一行 `×N`），展开后列出该组每条弧的**源 prim 节点**
（`/World/Tufts/i000000` … `/World/Tufts/i000239`），一个 mesh 一行。点源 prim 行 = **选中该
prim**（视口描边高亮 + `comp_node` 详情，与视口 pick 同一条路径）。聚合管默认整洁，展开管
单个 mesh——「合成图每个节点都要能看」的完整落法。

## v1.5.6 增补（用户裁定：要看单个 mesh；2026-11-18）

- **隔离显示开关**：底栏「只看选中 (i)」按钮 / 键 `i`。开启后视口**只画当前选中的 prim
  （含其子树）**；切换选中即切换画面；无选中时画整场景。纯 client 本地视图模式：不动 wire、
  不改合成、不改 version。
- 节点路径闭环：展开去重弧行（v1.5.4 修订）→ 点某个源 prim 行（选中该 mesh）→ 开隔离
  ⇒ 视口只剩这一个 mesh。三步可达，无需别的入口。
- 验收（viewer）：单测（隔离过滤：路径精确 + 子树前缀匹配；无选中回退整场景；`i` 切换）；
  真窗（点一颗球 → `i` → 只剩那一颗；再点同行另一颗 → 只看那一颗；关 `i` → 恢复 240）。

## v1.5.7 增补（撤销树内 prim 行；用户裁定重述 v1.3）

**撤销 v1.5.4 修订里「聚合行可展开列出源 prim 节点」**：合成树继续只画结构（层/弧/variant set），
**任何情况下不出现 prim 行**（v1.3 裁定）。聚合行（`xN`）不可展开、无 chevron；单个 mesh 的检视
走视口 pick -> 详情按层视图，或 `i` 只看选中。`composition_tree` 的 `prim_nodes` 仍随 wire 发送，
供 pick 解析，只不画。

## v1.7 增补（viewer 控制 RPC：GUI 的每个操作都有对等的 agent 入口；2026-11-18）

**目的**：agent 不该用 computer use 去点 viewer 的 GUI（会和用户抢鼠标）。viewer 以
`view --connect <server> [--control <addr>]` 启动时，在 `--control` 地址上开一条
**NDJSON JSON-RPC** 控制通道（一请求一行，同 server wire 的形状），把 GUI 的每个操作
暴露成有名字的方法。所有方法走 **和 GUI 事件同一条路径**（egui 线程上的同一处理函数），
不是第二套实现。

### 方法表（v1）

| 方法 | 参数 | 语义（= GUI 上哪一下） |
| --- | --- | --- |
| `state` | — | 当前连接、version、preview、selection、isolate、树是否已载入 |
| `open` | `{path}` | 等同文档打开（转发给 server 的 `open`，回 server 的答复） |
| `preview` | `{upto: <层 id> \| null}` | 点该层行 / final 按钮（预览控制） |
| `tree` | `{id}` | 点树上该 id 的行（按行类型：层行=选中+详情+预览，弧=详情，最终与 GUI 同一 `action_for`） |
| `select` | `{path}` | 等同视口 pick：选中该 prim（描边+详情） |
| `isolate` | `{on: bool}` | 「只看选中」开关（= 键 `i` / 底栏按钮） |
| `fit` | — | Fit 按钮 |
| `orbit` | `{yaw, pitch, dist, target:[x,y,z]?}` | 轨道相机（等同鼠标拖/滚） |
| `capture` | `{path}` | 把当前帧写成 PNG（等同「选区截图」去掉选区：整帧） |
| `shot` | `{path}` | 同上（别名，留给脚本习惯） |

- 答复形状与 server wire 一致：`{"jsonrpc":"2.0","id":n,"result":{...}}` / `"error":{code,message}`；
  `state` 永远可答；涉及 server 的方法在没有连接时答 `-32000`（未连接）。
- 未知方法 `-32601`；参数错 `-32602`。
- 控制端口默认**关闭**（不加 `--control` 不监听）；默认只绑 loopback。
- 纯 GUI 驱动：控制 RPC **不改合成、不推快照**——preview/open 仍是 viewer 自己那条 server
  连接上的普通请求，语义与用户点出来的完全一样。

### 验收

- 脚本（无 GUI 交互）能完成：open → tree 列出层 → preview 到某 op 层 → select 一个实例 →
  isolate 开 → capture 出 PNG → preview null 恢复 —— 全部只经控制 RPC。
- 未知方法/未连接/参数错三种错误码按上表。
- 既有点击路径不变（控制 RPC 是同一处理函数的第二入口）。
