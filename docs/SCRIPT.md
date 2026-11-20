# Rusd script — 算子是 Rust function，编译即校验（D15）

状态：v1.15 · 2026-11-20 · 决策 D15（SPEC §11）

## 0. 一句话

**一个算子定义就是一个 Rust function，它的签名就是它的 schema。**
Rust script 调用这些 function，造 Struct / Array / Vec，生成算子图（`.ops.toml`）与
USD 嵌套（`.rusd`）。类型校验从运行时移到**编译期**：接错线不是 cook 之后的 `E_SCHEMA`，
而是 rustc 在脚本那一行给出的 `E0277`。

运行时校验仍然保留——给**手写**的 `.ops.toml`，它背后没有编译器。脚本不需要它：
脚本能跑起来的时候，线已经接对了。

## 1. 为什么

v1.14 把算子的 I/O 声明成静态表（`Shape::Prim(SlotSchema { ty, apis })`），cook 在
运行时拿 `UsdSchemaRegistry::IsA` 去比。那是对的，但有两份代价：

1. **反馈太晚**。接错线要等到 cook，诊断指向节点而不是作者写下的那一行。
2. **两份声明**。算子的参数表、端口表、生成器实现各说各话，靠测试维持一致。

D15 把声明收进 Rust 类型：

- 端口的类型是 `Prim<Gprim>`、`Prim<Points>`、`List<Prim<Cube>>`、一个具名 struct；
- `IsA` 是 trait bound，**由 USD 自己的 schema manifest 生成**（`build.rs` 读
  `rusd-scene/schemas/rusd_types_manifest.usda`，写出每个祖先对一条 impl）；
- cook 的运行时表（`rusd_script::ops::{inputs, outputs}`）**从这些类型导出**，
  不再是第二份手写表。`tests/schema_graph.rs` 逐对比对生成的 `IsA` 与注册表的 `is_a`。

## 2. 怎么写一个脚本

脚本是一个普通的 Rust program（`fn main()`），依赖 `rusd-script`。放在 `examples/`：

```rust
use rusd_script::graph::{Graph, out};
use rusd_script::ops::{self, InstancesIn, InstancesParams, SamplePointsIn, SamplePointsParams};
use rusd_script::schema::{Cube, Mesh};

fn main() {
    let mut g = Graph::new();
    g.default_prim("/World");

    // 素材：指向已有 USD 的类型化句柄。
    let volume = g.material::<Cube>("volume.rusd", "/World/Volume");
    let terrain = g.material::<Mesh>("terrain.rusd", "/World/Terrain");
    let box_proto = g.material::<Cube>("box.rusd", "/World/Prototypes/Box");

    // 算子调用。第二个吃第一个的输出端口 —— 一个 Prim<Points>。
    let samples = ops::sample_points(
        &mut g,
        out("Samples", "samples.rusd:/World/Samples"),
        SamplePointsIn::new(volume),
        SamplePointsParams { count: 60, seed: 11 },
    );
    let boxes = ops::instances(
        &mut g,
        out("Boxes", "boxes.rusd:/World/Boxes"),
        InstancesIn::new(samples.ports.points, box_proto),
        InstancesParams { max_prims: 5000, seed: 11, ..Default::default() },
    );

    // 整个产物（anchor，一个 Xform）可以被下游整体实例化。
    let _ = boxes.anchor();

    g.write(std::path::Path::new("examples/box_scatter.ops.toml")).unwrap();
}
```

跑它：

```bash
rusd script check examples/box_scatter.rs   # 只编译，不跑 —— 这就是类型校验
rusd script run   examples/box_scatter.rs   # 编译并跑；脚本自己写 .ops.toml / .rusd
rusd expand       examples/box_scatter.ops.toml   # 照常 cook
```

- `check` 成功回 `{"ok":true,"script":"…"}`（外加 warning 的 JSON 行）；失败回
  `E_SCRIPT_TYPE` / `E_SCRIPT_COMPILE` 诊断行，`message` 带 `文件:行:列`，`hint` 是
  rustc 的完整渲染，`property` 是 rustc 的错误码（`E0277` 等）。
- `run` 的 cwd 是 **workspace 根**，所以脚本里写的相对路径就是仓库认识的路径。
- 编译在 `target/rusd-script/<hash>/` 的一个临时 crate 里，`--offline`、复用
  workspace 的 `target/` 与 `Cargo.lock`：热校验 ~0.4s，不联网。链接需要的 MSVC
  环境若进程没有，runner 从 `vcvars64.bat` 读一次补上。

## 3. 类型系统

### 3.1 一个 prim 的类型 = class + interfaces

```rust
Prim<Cube>                                  // 一个 Cube
Prim<Applied<MaterialBindingAPI, Cube>>     // 一个 Cube，且应用了 MaterialBindingAPI
```

- **class**：`Wire<Base>` bound。`Prim<Cube>` 能接进要 `Gprim` 的端口，因为
  `Cube: IsA<Gprim>`；`Prim<Xform>` 不能（兄弟不是基类）；`Prim<Material>` 不能。
  `cube.up::<Gprim>()` 显式上溯，`SamplePointsIn::new(cube)` 自动上溯。
- **interfaces**：`Has<A, M>` bound。`prim.apply::<MaterialBindingAPI>()` 应用一个
  API schema；要两者的端口同时 bound `Wire<Xformable> + Has<MaterialBindingAPI, M>`。
  `Applied` 不改变 class：它仍然回答 tail 的一切 `IsA`。

`IsA`/`Has` 的 impl 由 `build.rs` 从 manifest 生成，`Schema::NAME` 就是 USD 的
`typeName`。`Untyped`（`NAME = ""`）是实例 prim：它的类型经 reference 弧合成，
本地不作者 typeName。

### 3.2 Struct / Array / Vec

算子的 I/O 形状就是 Rust 类型：

| Rust | 形状 | USD 嵌套 |
| --- | --- | --- |
| `Prim<T>` | 一个 prim | 一个 prim |
| 具名 struct（`ports!`） | 具名部件 | 父 prim 下的具名子 prim |
| `List<T>` | 任意多个（长度是数据的） | `<prefix>{k:06}` 一串 |
| `Arr<T, N>` | 恰好 N 个 | `<prefix>{k:06}`，N 个 |

`ports!` 宏一次声明 struct + 它的 `Ports`（端口名与形状）+ `ShapeOf`：

```rust
ports! {
    pub struct StreetGridOut {
        #[part("Ground")] ground: Prim<Cube>,
        #[part("b*")]     blocks: List<Block>,   // 一个 block 又是一个 struct
    }
}
ports! {
    pub struct Block { #[part("lot*")] lots: List<Prim<Cube>> }
}
```

`#[part("…")]` 是端口在 wire 上的名字（`Ground`、`i*`、`lot*`）；不写就是字段名。
`*` 结尾是前缀，匹配任意多个部件——与运行时 `conform` 的规则一致。

**脚本自己造 Struct/Array/Vec 作者 USD** 也一样（`examples/nesting.rs`）：

```rust
struct Gate { left: Prim<Cube>, right: Prim<Cube>, lintel: Prim<Cube> }
let gate = Gate { left: layer.cube("/World/Gate/left", 1.0), … };
let markers: Vec<Prim<Sphere>> = (0..5).map(|k| layer.sphere(…)).collect();
let corners: [Prim<Sphere>; 4] = std::array::from_fn(|k| layer.sphere(…));
layer.write(dir)?;   // → nesting.rusd：父 prim + 具名子 prim + 编号串
```

`Layer::cube/sphere/…` 返回的句柄**由构造即为真**（不是对外部文件的断言），
所以它接进任何要 `Gprim`/`Xformable` 的端口都不需要运行时再查。
`g.material::<T>(file, path)` 是对已有文件的**断言**，cook 仍会按 USD 核它。

## 4. 算子清单（`rusd_script::ops`）

| function | 输入端口 | 输出端口 | 参数 struct |
| --- | --- | --- | --- |
| `sample_points` | `domain: Prim<Gprim>` | `points: Prim<Points>` | `SamplePointsParams { count, seed }` |
| `instances` | `points: Prim<Points>`, `prototype: Prim<Xformable>` | `i*: List<Prim<Untyped>>` | `InstancesParams { scale, max_prims, seed }` |
| `scatter` | `prototype: Prim<Xformable>`（+ 可选 `points`） | `i*: List<Prim<Sphere>>` | `ScatterParams { count, seed, domain, scale, yaw, points, max_prims, max_triangles }` |
| `grid` | （无） | `c*: List<Prim<Sphere>>` | `GridParams { count_x, count_y, spacing, radius, seed }` |
| `street_grid` | （无） | `Ground: Prim<Cube>`, `b*: List<Block>` | `StreetGridParams { … }` |

每个 function 返回 `Product<Out>`：`ports` 是声明的输出形状，`anchor()` 是它写下的
那个 `Xform`（下游可整体实例化）。`Dist::{Uniform, Normal, Rect, Fixed}` 是分布参数。

`RusdBuilding` / `RusdBlockFill` 还没有脚本签名（也没有输出形状声明）——要先有一个
building 的示例图才能定形状（交接未结案 #2）。

## 5. 接错线长什么样

```text
rusd script check target/bad_wire.rs
{"code":"E_SCRIPT_TYPE","severity":"error","property":"E0277",
 "message":"C:/resource/rusd/target/bad_wire.rs:13:29: the trait bound `Material: IsA<Gprim>` is not satisfied",
 "hint":"error[E0277]: …\n 13 | SamplePointsIn::new(red),\n    | ^^^ the trait `IsA<Gprim>` is not implemented for `Material`\n …"}
```

错误直接命名 **USD 的关系**（`Material: IsA<Gprim>` 不成立），指向脚本的那一行。
`compile_fail` doctest（`schema.rs`、`ops.rs`）把这三条钉死：Material 接 Xformable、
Xform 接 Gprim、无 API 的 Cube 接 `Has` bound——都必须编译失败。

## 6. 与运行时校验的关系

- **脚本路径**：rustc 校验 → 脚本写图 → cook。cook 的 `expect_slot`/`check_output`
  仍在，但对手写图才有意义；脚本图的线已由类型保证。
- **手写 `.ops.toml`**：没有编译器，cook 用 `rusd_script::ops::{inputs, outputs}`
  （从 Rust 类型导出的同一份声明）+ `rusd_scene::operator::{unmet, conform}` 核线，
  不符报 `E_SCHEMA` / `E_OUTPUT_SCHEMA`。
- **一份声明**：端口表不再是手写静态表。`rusd-scene::operator` 只剩词汇
  （`Shape`/`Port`/`SlotSchema`）、检查器（`unmet`/`conform`）与参数表（`parameters`）。

## 7. 文件

- `crates/rusd-script/`：`build.rs`（schema 代码生成）、`src/{schema,prim,shape,ops,graph,usd,runner}.rs`。
- `crates/rusd-script/tests/{schema_graph,operator_io}.rs`：IsA 逐对一致、端口表=类型、
  参数表一致、图往返、USD 嵌套可开。
- `examples/box_scatter.rs`、`examples/nesting.rs`：两个示例脚本。
- `crates/rusd-proc/tests/compose_products.rs::a_scripts_graph_cooks_end_to_end`：
  脚本 → 图 → cook → 60 个实例，零诊断。
