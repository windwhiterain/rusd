//! A persistable wgpu viewport: the same two passes `rusd_render` bakes and
//! draws (beauty + prim-id), with **the device created once**.
//!
//! `rusd_render::render_with` builds an instance, adapter and device on every
//! call — measured on this machine at **~250 ms of its ~310 ms**, which is why
//! it is the right shape for a one-shot screenshot and the wrong one for an
//! orbit: a camera move that costs 300 ms of device setup cannot reach 10 fps.
//! The viewer therefore takes the device egui already made (one GPU device for
//! the whole window, `docs/REMOTE.md` §7's render thread), uploads the snapshot
//! geometry once, and re-draws per camera change:
//!
//! ```text
//! set_scene(owned geometry)   // bake each **mesh** once + upload one record per prim
//! render(camera, config)      // uniforms + 2 passes + readback (no device, no shader build)
//! ```
//!
//! Everything drawn here is `rusd_render`'s own pipeline, shader and math,
//! copied with the lifetime split so the passes can be re-run: this is the same
//! renderer, not a second opinion about the pixels. There are two deliberate
//! differences. A prim's local-to-world matrix is applied **by the vertex shader**
//! (v1.11), not baked into its vertices, because that is what makes a moved prim a
//! 64-byte write. And since v1.14 the vertices are **only** the prototype's geometry:
//! the colour, the id and the marker flag live in a per-prim *instance record*, so every
//! prim that draws the same mesh shares one upload and one `draw_indexed` call — a box
//! scattered 2400 times is one 24-vertex box, 2400 records and one draw. The arithmetic
//! is then f32 where `rusd_render` bakes in f64, so the two can differ in the last bit of
//! a world position; geometry, lighting and camera math are otherwise the same code.
//!
//! Error-tolerant by the crate's rule: every failure is a [`Diagnostic`] with a
//! stable `E_` code, never a panic.

use bytemuck::{Pod, Zeroable};
use rusd_core::Diagnostic;
use rusd_render::RenderConfig;
use rusd_render::render::RenderOutput;
use rusd_scene::geometry::{self, Camera, GeoItem, Mat, SceneGeometry};

use crate::diag;

/// **A prototype's own geometry**, and nothing else (v1.14). Where it is drawn, what
/// colour it is and which prim it is belong to the **instance record**, so the same 24
/// bytes serve every copy of a mesh: a scene of 2400 identical boxes uploads one box.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
}

/// One prim's record in the instance buffer (v1.14) — `Inst` in the shader, 160 bytes:
/// two matrices, the colour, and the id the `prim-id` pass writes.
///
/// **The whole point of `draw_indexed(…, first..first + count)`**: the vertex shader reads
/// this by `@builtin(instance_index)`, so one draw call draws every prim that shares a
/// mesh, and a moved prim is a 64-byte write into `model` with no vertex touched.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, PartialEq)]
struct Inst {
    /// `model` at 0, the normal matrix at 64, the colour at 128, the id and the marker
    /// flag at 144: four spans a partial write can address on its own.
    model: [f32; 16],
    normal: [f32; 16],
    color: [f32; 4],
    id_flag: [u32; 4],
}

impl Inst {
    /// What a record holds before its first write: NaN compares unequal to everything,
    /// including itself, so the first diff writes every span without a `fresh` flag.
    const EMPTY: Self = Self {
        model: [f32::NAN; 16],
        normal: [f32::NAN; 16],
        color: [f32::NAN; 4],
        id_flag: [u32::MAX; 4],
    };
}

/// Shader byte-identical to `rusd_render`'s: beauty (directional + ambient) and
/// prim-id (`R32Uint`, flat `@interpolate(flat) u32`).
const SHADER: &str = r#"
struct Uniforms {
    view_proj: mat4x4<f32>,
    light: vec4<f32>,    // xyz = direction, w = ambient
    params: vec4<f32>,   // x = diffuse
};
@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) color: vec3<f32>,
    @location(2) @interpolate(flat) id: u32,
    @location(3) flag: f32,
};

// --- the instance record (v1.14) ----------------------------------------------
// **One draw, N instances**: the vertices hold a prototype's local geometry once, and
// every prim that draws it is one record here — its matrices, colour, id and the marker
// flag. `@builtin(instance_index)` picks the record, so a moved prim is 64 bytes of this
// record and never a vertex again.
struct Inst {
    model: mat4x4<f32>,
    // inverse-transpose of `model`'s upper 3x3, translation zero: a normal is not a
    // position, so a squashed prim must not get its lighting inverted.
    normal: mat4x4<f32>,
    color: vec4<f32>,
    // x = the prim id this record draws under, y = 1 for a point marker (a quad the
    // shader builds in clip space) and 0 for geometry.
    id_flag: vec4<u32>,
};
@group(0) @binding(1) var<storage, read> insts: array<Inst>;

@vertex
fn vs_main(
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @builtin(instance_index) index: u32,
) -> VsOut {
    let inst = insts[index];
    var out: VsOut;
    out.color = inst.color.rgb;
    out.id = inst.id_flag.x;
    out.flag = f32(inst.id_flag.y);
    // `pos` is the prototype's **local** position; its matrix is what puts it in the world.
    let world = inst.model * vec4<f32>(pos, 1.0);
    if (out.flag > 0.5) {
        // A point marker is a screen-space dot: the quad is built in clip space
        // from its centre, so zooming never turns a point into a tile. The half-size is
        // 14 px (v1.9: it was 6, and a sampling node's 40 points then read as an empty
        // frame — the dot has to be visible on its own, since a cloud is all that node is).
        var clip = u.view_proj * world;
        clip.x = clip.x + normal.x * u.params.y * clip.w * 14.0;
        clip.y = clip.y + normal.y * u.params.z * clip.w * 14.0;
        out.pos = clip;
        out.normal = vec3<f32>(0.0, 0.0, 1.0);
    } else {
        out.pos = u.view_proj * world;
        out.normal = (inst.normal * vec4<f32>(normal, 0.0)).xyz;
    }
    return out;
}

@fragment
fn fs_beauty(in: VsOut) -> @location(0) vec4<f32> {
    if (in.flag > 0.5) {
        // Markers are flat colour: no shading, so they can never pass for a mesh.
        return vec4<f32>(in.color * (u.light.w + u.params.x), 1.0);
    }
    let n = normalize(in.normal);
    let l = normalize(u.light.xyz);
    let diff = max(dot(n, l), 0.0);
    let c = in.color * (u.light.w + u.params.x * diff);
    return vec4<f32>(c, 1.0);
}

@fragment
fn fs_id(in: VsOut) -> @location(0) u32 {
    return in.id;
}

// --- the selection's outline (v1.10: drawn on the GPU, from the id buffer) ----------
@group(1) @binding(0) var id_tex: texture_2d<u32>;

@vertex
fn vs_outline(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    // One full-screen triangle: three vertices, no buffer.
    var corners = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    return vec4<f32>(corners[index], 0.0, 1.0);
}

@fragment
fn fs_outline(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let selected = u32(u.params.w);
    if (selected == 0u) {
        discard;
    }
    let dim = vec2<i32>(textureDimensions(id_tex));
    let at = vec2<i32>(i32(pos.x), i32(pos.y));
    if (textureLoad(id_tex, at, 0).x != selected) {
        discard;
    }
    // The selected prim's pixels that touch a pixel of anything else are the outline.
    var neighbours = array<vec2<i32>, 4>(vec2<i32>(1, 0), vec2<i32>(-1, 0), vec2<i32>(0, 1), vec2<i32>(0, -1));
    for (var k = 0; k < 4; k = k + 1) {
        let q = clamp(at + neighbours[k], vec2<i32>(0, 0), dim - vec2<i32>(1, 1));
        if (textureLoad(id_tex, q, 0).x != selected) {
            return vec4<f32>(1.0, 0.235, 0.235, 1.0);
        }
    }
    discard;
    // Unreachable after a `discard`, but **not redundant**: DXC reads `discard` as a
    // statement, not as the end of the function, so without this line it refuses the
    // whole shader with "control may reach end of non-void function" — and the
    // `selection-outline` pipeline then never builds.
    return vec4<f32>(0.0);
}
"#;

/// Beauty target format: sRGB, so written values are gamma-encoded bytes and the
/// CPU side can hand them to egui, whose textures are sampled as gamma
/// (`egui`'s `fs_main_gamma_framebuffer`). Same as `rusd_render`.
const BEAUTY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// The format egui is handed the beauty texture under: the same bytes, **no sRGB decode**.
const DISPLAY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const ID_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Uint;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// The persistent-device viewport: pipelines and a uniform buffer built once,
/// the scene's vertex/index buffers rebuilt only when the snapshot changes.
///
/// How many bytes one **instance record** takes (v1.14): a model `mat4x4<f32>`, its
/// normal matrix, the colour and the id/flag pair. Its two 64-byte halves are why a
/// moved prim is exactly one 64-byte write, and its four spans
/// (`0..64`, `64..128`, `128..144`, `144..160`) are what a partial write addresses.
const INST_RECORD_BYTES: u64 = 160;
/// Where each span of an [`Inst`] starts and ends, in bytes.
const INST_MODEL: std::ops::Range<usize> = 0..64;
const INST_NORMAL: std::ops::Range<usize> = 64..128;
const INST_COLOR: std::ops::Range<usize> = 128..144;
const INST_ID_FLAG: std::ops::Range<usize> = 144..160;

pub struct Viewport {
    device: wgpu::Device,
    queue: wgpu::Queue,
    width: u32,
    height: u32,

    beauty_pipeline: wgpu::RenderPipeline,
    id_pipeline: wgpu::RenderPipeline,
    /// The selection's outline (v1.10): draws the border of the selected prim from the
    /// id buffer, so a selection costs no CPU pixel work and `capture` shows it too.
    outline_pipeline: wgpu::RenderPipeline,
    id_bgl: wgpu::BindGroupLayout,
    id_bind_group: wgpu::BindGroup,
    bind_group: wgpu::BindGroup,
    uniform_bgl: wgpu::BindGroupLayout,
    uniform_buf: wgpu::Buffer,

    /// The per-prim **instance records** (v1.14), 160 bytes each, read by the vertex
    /// shader through `@builtin(instance_index)`: `model` at `n * 160`, the normal
    /// matrix at `n * 160 + 64`, the colour at `+128`, the id/flag pair at `+144`.
    /// **A moved prim writes only the first span** — a drag is 64 bytes, never a vertex.
    inst_buf: wgpu::Buffer,
    /// Capacity of [`Self::inst_buf`] in records.
    inst_cap: u32,
    /// Every instance record, in instance-index order: one entry per prim on the GPU.
    /// The record it carries is what the GPU holds now, so a sync writes **only the
    /// spans that differ** — which is why a repeat sync writes nothing at all.
    insts: Vec<InstSlot>,
    /// A prim path to its record's index. Stable while the prim lives; a compaction
    /// renumbers it, and only a compaction does.
    inst_index: std::collections::BTreeMap<String, u32>,
    /// How many bytes the last sync wrote into [`Self::prim_buf`]. What makes 「拖动一个
    /// prim 只写 64 字节」 an observation instead of a claim.
    pub matrix_bytes: usize,

    // Targets, rebuilt on resize.
    beauty_tex: wgpu::Texture,
    beauty_view: wgpu::TextureView,
    /// The same texture as [`Self::beauty_view`], under [`DISPLAY_FORMAT`]: what a UI is
    /// handed, so egui samples the sRGB bytes without decoding them again.
    beauty_display: wgpu::TextureView,
    id_tex: wgpu::Texture,
    id_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,

    // Scene buffers, **kept across snapshots** (v1.10: 渲染是 incremental 的 — a USD
    // delta is a GPU buffer delta). A slab belongs to a **mesh**, not to a prim (v1.14):
    // every prim that draws the same geometry shares one, and the per-prim part is a
    // record in [`Self::inst_buf`]. So a drag costs one record's model matrix, and a box
    // scattered 2400 times costs one box. The buffers are an append-only arena: a slab
    // that outgrows its space is appended at the tail and the old space is waste until
    // the arena is compacted (at half waste, or when it must grow).
    vertex_buf: Option<wgpu::Buffer>,
    index_buf: Option<wgpu::Buffer>,
    /// Capacity in vertices / in indices.
    vertex_cap: u32,
    index_cap: u32,
    /// Where the next append starts.
    vertex_len: u32,
    index_len: u32,
    /// What the live slabs actually hold, so the waste is visible at a glance.
    live_vertices: u32,
    live_indices: u32,
    /// One slab per **mesh**, keyed by the hash of its geometry: a mesh is baked and
    /// uploaded once however many prims draw it, which is what makes draw instancing
    /// worth anything (2400 scattered boxes: one 24-vertex box on the GPU).
    slots: std::collections::BTreeMap<u64, Slot>,
    /// The id a prim is drawn under in the `prim-id` pass. Stable for the life of the
    /// session, so a slab can be rewritten without renaming the prim to the picker.
    id_of: std::collections::BTreeMap<String, u32>,
    paths: Vec<String>,
    /// [`Self::paths_arc`]'s cached share of the same list.
    paths_arc: Option<std::sync::Arc<Vec<String>>>,
    /// What generation the current targets are; bumped by [`Self::resize`] so a UI that
    /// displays [`Self::beauty_view`] knows when to register the texture again.
    generation: u64,
    /// The 256-byte staging buffer a pick reads one texel into.
    pick_read: Option<wgpu::Buffer>,
    /// The draws the live slabs leave: one indexed draw per **run of consecutive
    /// instances of one mesh** (v1.14), so a box scattered 2400 times is **one** draw
    /// call. Contiguous slabs merge, so a freshly compacted arena is one draw.
    draws: Vec<Draw>,
    /// How many meshes the last sync actually baked and uploaded, and how many draws it
    /// left. What makes 「只把变化的部分传给 GPU」 observable instead of a claim.
    pub rewritten: usize,
    pub slots_live: usize,
    /// How many instance records are live: the prims on the GPU. Read with `draws` to
    /// see the instancing itself — 2400 instances, 1 draw, 1 slab.
    pub instances_live: usize,

    // Reused readback buffers (grown as needed).
    beauty_read: wgpu::Buffer,
    id_read: wgpu::Buffer,
    read_bytes_per_row: u32,

    /// How many scenes were uploaded (the counter a client shows as "this is
    /// snapshot N", useful when a stale one never appears).
    pub uploaded: u64,
}

impl Viewport {
    /// Build the pipelines on an existing device. `device`/`queue` are egui's,
    /// so the viewport's textures and the UI's are the same GPU and a frame can
    /// present both without a cross-device copy.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, width: u32, height: u32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rusd-viewer"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewer-uniforms"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // The per-prim **instance records** (v1.14): one 160-byte record per
                // prim on the GPU, read by the vertex shader by `@builtin(instance_index)`,
                // so one draw call draws every prim that shares a mesh.
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewer"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer-uniforms"),
            size: 96, // 16 matrix floats + 8 params floats
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // The instance records (v1.14): one 160-byte record per prim on the GPU. Never
        // sized zero — one record of slack keeps the binding valid before a scene.
        let inst_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer-instances"),
            size: INST_RECORD_BYTES * 64,
            // No COPY_SRC: a record table that outgrows its space is rewritten whole
            // (the instances' indices compact with it), so nothing is moved on the GPU.
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = Self::make_bind_group(&device, &bgl, &uniform_buf, &inst_buf);
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            // **Per vertex, never per instance**: every copy of a mesh shares these 24
            // bytes, which is the whole point of the split (v1.14).
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 12,
                    shader_location: 1,
                },
            ],
        };
        let depth_state = wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: Default::default(),
            bias: Default::default(),
        };
        let make = |label: &str, target: wgpu::TextureFormat, entry: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[Some(vertex_layout.clone())],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target,
                        // An integer (R32Uint) target is not blendable; the beauty
                        // target is opaque. Both match `rusd_render`'s pipelines.
                        blend: if target == ID_FORMAT {
                            None
                        } else {
                            Some(wgpu::BlendState::REPLACE)
                        },
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: Some(depth_state.clone()),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let beauty_pipeline = make("beauty", BEAUTY_FORMAT, "fs_beauty");
        let id_pipeline = make("prim-id", ID_FORMAT, "fs_id");

        // **The selection's outline, on the GPU** (v1.10): a full-screen pass that reads
        // the id buffer the prim-id pass just wrote and colours the selected prim's
        // border. Until this existed the outline was a CPU pass over the read-back frame,
        // which is why a selected prim cost a whole-frame copy every frame.
        let id_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewer-prim-id"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Uint,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let outline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewer-outline"),
            bind_group_layouts: &[Some(&bgl), Some(&id_bgl)],
            immediate_size: 0,
        });
        let targets = Targets::new(&device, width, height);
        let outline_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("selection-outline"),
            layout: Some(&outline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_outline"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_outline"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: BEAUTY_FORMAT,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let read_bytes_per_row = aligned_bytes_per_row(width);
        let read_len = u64::from(read_bytes_per_row * height);
        let beauty_read = readback_buffer(&device, read_len);
        let id_read = readback_buffer(&device, read_len);
        let id_bind_group = Self::id_bind(&device, &id_bgl, &targets.id_view);

        Self {
            device,
            queue,
            width,
            height,
            beauty_pipeline,
            id_pipeline,
            outline_pipeline,
            id_bgl,
            id_bind_group,
            bind_group,
            uniform_bgl: bgl,
            uniform_buf,
            inst_buf,
            inst_cap: 64,
            insts: Vec::new(),
            inst_index: std::collections::BTreeMap::new(),
            matrix_bytes: 0,
            beauty_tex: targets.beauty_tex,
            beauty_view: targets.beauty_view,
            beauty_display: targets.beauty_display,
            id_tex: targets.id_tex,
            id_view: targets.id_view,
            depth_view: targets.depth_view,
            vertex_buf: None,
            index_buf: None,
            paths: Vec::new(),
            paths_arc: None,
            generation: 0,
            pick_read: None,
            beauty_read,
            id_read,
            read_bytes_per_row,
            uploaded: 0,
            vertex_cap: 0,
            index_cap: 0,
            vertex_len: 0,
            index_len: 0,
            live_vertices: 0,
            live_indices: 0,
            slots: std::collections::BTreeMap::new(),
            id_of: std::collections::BTreeMap::new(),
            draws: Vec::new(),
            rewritten: 0,
            slots_live: 0,
            instances_live: 0,
        }
    }

    /// The pixels' size this viewport renders at.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The prim paths in id order: `paths[id - 1]` is the prim an id names.
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// How many draw runs the live slabs leave (diagnostics, and what a test reads to
    /// tell an uncompacted arena from a fragmented one).
    pub fn draws_count(&self) -> usize {
        self.draws.len()
    }

    /// The id a prim draws under, if it is on the GPU: what the outline pass needs to
    /// know which prim to outline.
    pub fn id_of(&self, path: &str) -> Option<u32> {
        let id = self.id_of.get(path).copied()?;
        // "On the GPU" is an instance record now (v1.14), not a slab of its own: a prim
        // shares its slab with every prim that draws the same mesh.
        self.inst_index.contains_key(path).then_some(id)
    }

    /// The beauty texture, for a UI to display without a CPU round trip.
    ///
    /// This is the **display** view (v1.11): the same bytes as the render target, under
    /// [`DISPLAY_FORMAT`] so egui's shader — which treats a sampled texel as already
    /// gamma-encoded — shows the picture the PNG holds. Handing egui the sRGB view made
    /// the window show `linear(capture)`: 140,178,216 where the capture had 196,218,237.
    pub fn beauty_view(&self) -> &wgpu::TextureView {
        &self.beauty_display
    }

    /// Resize the render targets (the camera and scene are untouched, so a
    /// window resize never needs a new snapshot).
    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.width, self.height) {
            return;
        }
        self.width = width;
        self.height = height;
        self.generation += 1;
        let targets = Targets::new(&self.device, width, height);
        self.beauty_tex = targets.beauty_tex;
        self.beauty_view = targets.beauty_view;
        self.beauty_display = targets.beauty_display;
        self.id_tex = targets.id_tex;
        self.id_view = targets.id_view;
        self.depth_view = targets.depth_view;
        self.read_bytes_per_row = aligned_bytes_per_row(width);
        let len = u64::from(self.read_bytes_per_row * height);
        self.beauty_read = readback_buffer(&self.device, len);
        self.id_read = readback_buffer(&self.device, len);
        self.id_bind_group = Self::id_bind(&self.device, &self.id_bgl, &self.id_view);
    }

    /// The bind group the outline pass reads the id buffer through.
    fn id_bind(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewer-prim-id"),
            layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            }],
        })
    }

    /// The bind group both scene passes use: the frame's uniforms and the per-prim
    /// instance records. Rebuilt whenever [`Self::inst_buf`] is replaced.
    fn make_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniform_buf: &wgpu::Buffer,
        inst_buf: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewer-uniforms"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: inst_buf.as_entire_binding(),
                },
            ],
        })
    }

    /// Bring the GPU geometry up to `geo`: **only what actually changed is baked and
    /// written** (v1.10 — a USD delta is a GPU buffer delta), and what changes for a
    /// moved prim is one instance record (v1.14).
    ///
    /// `overrides` is a gizmo drag's per-prim translation, and its keys are the only
    /// prims that can have moved — `only` is what says so, and with it set a drag costs
    /// one record's model matrix instead of the scene's. `None` means "diff the whole
    /// scene", which is what a fresh snapshot needs: it still bakes nothing whose mesh is
    /// already on the GPU and writes no record that already matches.
    ///
    /// `scope` is what a prim `geo` does not mention *is*: gone (`Scope::Whole`, a new
    /// composition) or merely hidden (`Scope::Filtered`, 「只看选中」 on). A hidden prim
    /// keeps its mesh and its record, and its run simply is not drawn, so turning the
    /// filter off costs nothing — v1.11: a filter is not a scene change.
    pub fn set_scene(&mut self, geo: &SceneGeometry, scope: Scope) {
        self.sync(geo, &std::collections::BTreeMap::new(), None, scope);
    }

    /// The same, with a few prims moved: `overrides` maps a prim path to the
    /// world translation it should be drawn at. This is a gizmo drag's preview —
    /// local, so the prim follows the pointer without waiting for the server's
    /// snapshot.
    ///
    /// **A drag is a 64-byte write** (v1.11, still true under v1.14's instancing): the
    /// override lands in the prim's instance record, in the `model` matrix half, not in
    /// any vertex, so the scene's vertex buffer is not touched at all and `matrix_bytes`
    /// comes out `64` for a one-prim drag.
    pub fn set_scene_with_translates(
        &mut self,
        geo: &SceneGeometry,
        overrides: &std::collections::BTreeMap<String, [f64; 3]>,
        scope: Scope,
    ) {
        let only: std::collections::BTreeSet<String> = overrides.keys().cloned().collect();
        self.sync(geo, overrides, Some(&only), scope);
    }

    fn sync(
        &mut self,
        geo: &SceneGeometry,
        overrides: &std::collections::BTreeMap<String, [f64; 3]>,
        only: Option<&std::collections::BTreeSet<String>>,
        scope: Scope,
    ) {
        let vertex_stride = std::mem::size_of::<Vertex>() as u64;
        self.uploaded += 1;
        self.rewritten = 0;
        self.matrix_bytes = 0;
        if self.vertex_buf.is_none() {
            self.allocate(1024, 4096);
        }

        // --- what the scene wants: one entry per prim, reduced to a mesh hash -------
        // After this the sync never looks at a mesh again unless it has to **bake** one,
        // so a prim's geometry reaches the GPU once per hash and not once per prim —
        // which is what makes 2400 scattered boxes one 24-vertex box (v1.14).
        let mut live: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut wants: Vec<Want> = Vec::with_capacity(geo.items.len());
        for (at, item) in geo.items.iter().enumerate() {
            live.insert(item.path.clone());
            // A prim the hint says cannot have moved is not even hashed: that is what
            // makes a drag a one-prim delta rather than a scene walk.
            if only.is_some_and(|set| !set.contains(&item.path))
                && self.inst_index.contains_key(&item.path)
            {
                continue;
            }
            let transform = match overrides.get(&item.path) {
                Some(t) => {
                    let mut m = item.transform;
                    m[12] = t[0];
                    m[13] = t[1];
                    m[14] = t[2];
                    m
                }
                None => item.transform,
            };
            wants.push(Want {
                at,
                sig: mesh_signature(item),
                vcount: item_vertices(item),
                icount: item_indices(item),
                transform,
                color: [item.color[0], item.color[1], item.color[2], 1.0],
                marker: item.markers.is_some(),
            });
        }

        // --- the arena: grow or compact when the meshes it lacks no longer fit -------
        // Only the meshes that are **not on the GPU yet** count: a scene whose every mesh
        // is already a slab needs no room at all, however many prims it names.
        let mut need_v = 0u32;
        let mut need_i = 0u32;
        let mut seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        for want in &wants {
            if seen.insert(want.sig) && !self.slots.contains_key(&want.sig) {
                need_v += want.vcount;
                need_i += want.icount;
            }
        }
        let waste_v = self.vertex_len.saturating_sub(self.live_vertices);
        let waste_i = self.index_len.saturating_sub(self.live_indices);
        let must = self.vertex_len + need_v > self.vertex_cap
            || self.index_len + need_i > self.index_cap;
        let stale = (self.vertex_cap > 8192 && waste_v > self.vertex_cap / 2)
            || (self.index_cap > 8192 && waste_i > self.index_cap / 2);
        // **A delta sync compacts only when it must**: a drag must not move the arena.
        if must || (only.is_none() && stale) {
            // **A compaction is a device-to-device move** (v1.10): the live slabs are copied
            // from the old buffers to the new ones with `copy_buffer_to_buffer`, so nothing
            // that did not change crosses the bus — no re-bake, no re-upload, no CPU pixels.
            // Amortised: the new arena has room to spare, so this is rare.
            let cap_v = (need_v.max(1024) * 2).max(self.live_vertices * 2);
            let cap_i = (need_i.max(4096) * 2).max(self.live_indices * 2);
            let old_vertex = self.vertex_buf.take();
            let old_index = self.index_buf.take();
            self.allocate(cap_v, cap_i);
            let (Some(new_vertex), Some(new_index)) = (self.vertex_buf.clone(), self.index_buf.clone())
            else {
                return;
            };
            let stride = std::mem::size_of::<Vertex>() as u64;
            let mut moved = std::collections::BTreeMap::new();
            let mut vertex_at = 0u32;
            let mut index_at = 0u32;
            let mut rebased: Vec<u32> = Vec::new();
            let mut slabs: Vec<(u64, Slot)> = self
                .slots
                .iter()
                .map(|(sig, slot)| (*sig, slot.clone()))
                .collect();
            slabs.sort_by_key(|(_, slot)| slot.vstart);
            let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer-compact"),
            });
            if let Some(from_v) = &old_vertex {
                let _ = &old_index;
                for (sig, slot) in slabs {
                    if slot.vcount > 0 {
                        encoder.copy_buffer_to_buffer(
                            from_v,
                            u64::from(slot.vstart) * stride,
                            &new_vertex,
                            u64::from(vertex_at) * stride,
                            u64::from(slot.vcount) * stride,
                        );
                    }
                    // **Rebase, do not copy**: the indices are absolute, so a slab that moves
                    // from `slot.vstart` to `vertex_at` has every index shifted by the same
                    // delta. Copying them raw drew triangles between prims.
                    let delta = i64::from(vertex_at) - i64::from(slot.vstart);
                    let mut slab_indices: Vec<u32> = Vec::with_capacity(slot.indices.len());
                    for i in &slot.indices {
                        slab_indices.push((i64::from(*i) + delta) as u32);
                    }
                    rebased.extend(slab_indices.iter().copied());
                    moved.insert(
                        sig,
                        Slot {
                            vstart: vertex_at,
                            istart: index_at,
                            indices: slab_indices,
                            ..slot
                        },
                    );
                    vertex_at += slot.vcount;
                    index_at += slot.icount;
                }
            }
            self.queue.submit(Some(encoder.finish()));
            if !rebased.is_empty() {
                self.queue
                    .write_buffer(&new_index, 0, bytemuck::cast_slice(&rebased));
            }
            self.slots = moved;
            self.vertex_len = vertex_at;
            self.index_len = index_at;
            self.live_vertices = vertex_at;
            self.live_indices = index_at;
        }
        // A mesh no live prim names is either **gone** (a composition changed) or merely
        // **hidden** (a filter is on), and only the caller knows which. Gone frees its
        // slab; hidden keeps it, so lifting 「只看选中」 re-bakes nothing.
        if scope == Scope::Whole && only.is_none() {
            let kept: std::collections::BTreeSet<u64> = wants.iter().map(|w| w.sig).collect();
            let gone_meshes: Vec<u64> = self
                .slots
                .keys()
                .copied()
                .filter(|sig| !kept.contains(sig))
                .collect();
            for sig in gone_meshes {
                if let Some(slot) = self.slots.remove(&sig) {
                    self.live_vertices = self.live_vertices.saturating_sub(slot.vcount);
                    self.live_indices = self.live_indices.saturating_sub(slot.icount);
                }
            }
        }

        // --- bake the meshes that are not on the GPU yet, **one per hash** ----------
        for want in &wants {
            if self.slots.contains_key(&want.sig) {
                continue;
            }
            let (Some(vertex_buf), Some(index_buf)) = (self.vertex_buf.clone(), self.index_buf.clone())
            else {
                return;
            };
            let (vstart, istart) = (self.vertex_len, self.index_len);
            let mut vertices: Vec<Vertex> = Vec::with_capacity(want.vcount as usize);
            let mut indices: Vec<u32> = Vec::with_capacity(want.icount as usize);
            bake_mesh(&geo.items[want.at], vstart, &mut vertices, &mut indices);
            if !vertices.is_empty() {
                self.queue.write_buffer(
                    &vertex_buf,
                    u64::from(vstart) * vertex_stride,
                    bytemuck::cast_slice(&vertices),
                );
            }
            if !indices.is_empty() {
                self.queue.write_buffer(
                    &index_buf,
                    u64::from(istart) * 4,
                    bytemuck::cast_slice(&indices),
                );
            }
            self.vertex_len += want.vcount;
            self.index_len += want.icount;
            self.live_vertices += want.vcount;
            self.live_indices += want.icount;
            self.slots.insert(
                want.sig,
                Slot {
                    vstart,
                    vcount: want.vcount,
                    istart,
                    icount: want.icount,
                    indices,
                },
            );
            self.rewritten += 1;
        }

        // --- the instance records: one per prim, and nothing written for a match -----
        // Room for every record this sync could mint, **in one go**: a first upload of a
        // 2400-prim scene would otherwise recreate the table (and its bind group) a dozen
        // times on the way up from 64 records. A grown table holds garbage, so it is
        // rewritten whole below and the per-record diff has nothing to add.
        let fresh = wants
            .iter()
            .filter(|w| !self.inst_index.contains_key(&geo.items[w.at].path))
            .count() as u32;
        let regrown = self.grow_inst((self.insts.len() + fresh as usize + 1) as u32);
        let mut changed: Vec<(u32, Inst)> = Vec::new();
        for want in &wants {
            let path = &geo.items[want.at].path;
            let id = self.prim_id(path);
            let m = PrimMatrices::of(&want.transform);
            let record = Inst {
                model: m.model,
                normal: m.normal,
                color: want.color,
                id_flag: [id, u32::from(want.marker), 0, 0],
            };
            match self.inst_index.get(path).copied() {
                Some(index) => {
                    // A prim whose **mesh** changed draws another prototype, so it joins
                    // that mesh's run. Its record keeps its slot: only a compaction
                    // renumbers records.
                    let slot = &mut self.insts[index as usize];
                    slot.sig = want.sig;
                    if slot.inst != record {
                        let have = slot.inst;
                        slot.inst = record;
                        changed.push((index, have));
                    }
                }
                None => {
                    let index = self.insts.len() as u32;
                    self.insts.push(InstSlot {
                        path: path.clone(),
                        sig: want.sig,
                        inst: record,
                        alive: true,
                        shown: true,
                    });
                    self.inst_index.insert(path.clone(), index);
                    // A record the GPU has never held: compared against the sentinel,
                    // every one of its four spans differs, so its first upload is 160.
                    changed.push((index, Inst::EMPTY));
                }
            }
        }
        // A prim the scene no longer names is gone or hidden, the same pair as a mesh.
        if scope == Scope::Whole && only.is_none() {
            let gone: Vec<String> = self
                .inst_index
                .keys()
                .filter(|path| !live.contains(path.as_str()))
                .cloned()
                .collect();
            for path in gone {
                if let Some(index) = self.inst_index.remove(&path) {
                    self.insts[index as usize].alive = false;
                }
            }
        }
        // Show exactly what the scene names. A hidden prim keeps its record, so a filter
        // costs no upload at all: the runs below simply stop drawing it.
        for slot in self.insts.iter_mut() {
            slot.shown = slot.alive && live.contains(&slot.path);
        }

        // --- write the records that differ ---------------------------------------
        // Dead records waste the table, so a full rewrite is the moment to drop them (it
        // costs nothing extra, and it leaves the table exactly the live prims), and half of
        // it being dead is a compaction on its own. A compaction renumbers records, which
        // is the one thing that makes the next write the whole table — and a delta sync
        // never does either.
        let dead = self.insts.iter().filter(|slot| !slot.alive).count();
        let compacted = dead > 0
            && (regrown || (only.is_none() && dead * 2 > self.insts.len()))
            && self.compact_insts();
        if regrown || compacted {
            self.write_all_insts();
        } else {
            for (index, have) in changed {
                self.write_inst(index, have);
            }
        }

        // --- the draws: one per run of consecutive, drawn instances of one mesh ------
        // A run breaks on a hidden prim or on the next prim naming another mesh, so 2400
        // boxes scattered in one go are a **single** `draw_indexed(indices, 0, 0..2400)`.
        let mut runs: Vec<(u64, u32, u32)> = Vec::new();
        for (i, slot) in self.insts.iter().enumerate() {
            if !slot.shown {
                continue;
            }
            let at = i as u32;
            match runs.last_mut() {
                Some((sig, start, count)) if *sig == slot.sig && *start + *count == at => *count += 1,
                _ => runs.push((slot.sig, at, 1)),
            }
        }
        self.draws.clear();
        for (sig, first, count) in runs {
            if let Some(slot) = self.slots.get(&sig) {
                if slot.icount > 0 {
                    self.draws.push(Draw {
                        istart: slot.istart,
                        icount: slot.icount,
                        first,
                        count,
                    });
                }
            }
        }
        self.slots_live = self.slots.len();
        self.instances_live = self.inst_index.len();
    }

    /// (Re)create the two scene buffers at `vertex_cap` vertices and `index_cap` indices.
    fn allocate(&mut self, vertex_cap: u32, index_cap: u32) {
        let vb = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer-vertices"),
            size: u64::from(vertex_cap) * std::mem::size_of::<Vertex>() as u64,
            // COPY_SRC as well: a compaction moves the live slabs to a bigger buffer
            // **on the GPU** (v1.10), so the old one has to be a copy source.
            usage: wgpu::BufferUsages::VERTEX
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let ib = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer-indices"),
            size: u64::from(index_cap) * 4,
            usage: wgpu::BufferUsages::INDEX
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        self.vertex_buf = Some(vb);
        self.index_buf = Some(ib);
        self.vertex_cap = vertex_cap.max(1);
        self.index_cap = index_cap.max(1);
    }

    /// Make room for `records` instance records, answering whether the buffer was
    /// replaced. The old contents are **not** copied: a grown table holds garbage, so the
    /// sync that grew it rewrites every record from [`Self::insts`] — 160 bytes per prim,
    /// which is what a first snapshot costs.
    fn grow_inst(&mut self, records: u32) -> bool {
        if records <= self.inst_cap {
            return false;
        }
        let cap = records.max(self.inst_cap.saturating_mul(2));
        self.inst_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer-instances"),
            size: u64::from(cap) * INST_RECORD_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.inst_cap = cap;
        self.bind_group = Self::make_bind_group(
            &self.device,
            &self.uniform_bgl,
            &self.uniform_buf,
            &self.inst_buf,
        );
        true
    }

    /// Drop the dead records and renumber the live ones, in order. The one thing that
    /// moves a prim to another instance index, and so the one thing that makes the next
    /// write a whole-table one.
    fn compact_insts(&mut self) -> bool {
        let mut kept: Vec<InstSlot> = Vec::with_capacity(self.insts.len());
        let mut index = std::collections::BTreeMap::new();
        for slot in self.insts.drain(..) {
            if !slot.alive {
                continue;
            }
            index.insert(slot.path.clone(), kept.len() as u32);
            kept.push(slot);
        }
        self.insts = kept;
        self.inst_index = index;
        true
    }

    /// Rewrite the spans of one record that differ from `have`, as **one** contiguous
    /// write: a drag differs in `model` alone (64 bytes), a re-colour in `color` alone
    /// (16), and a prim's first record in all four (160).
    fn write_inst(&mut self, index: u32, have: Inst) {
        let want = self.insts[index as usize].inst;
        if have == want {
            return;
        }
        let old = bytemuck::bytes_of(&have);
        let new = bytemuck::bytes_of(&want);
        let mut span: Option<(usize, usize)> = None;
        for part in [INST_MODEL, INST_NORMAL, INST_COLOR, INST_ID_FLAG] {
            if old[part.clone()] != new[part.clone()] {
                span = Some(match span {
                    Some((lo, _)) => (lo, part.end),
                    None => (part.start, part.end),
                });
            }
        }
        let Some((lo, hi)) = span else { return };
        let at = u64::from(index) * INST_RECORD_BYTES + lo as u64;
        self.queue.write_buffer(&self.inst_buf, at, &new[lo..hi]);
        self.matrix_bytes += hi - lo;
    }

    /// Write the whole instance table, as one contiguous upload: what a first snapshot
    /// costs (`N * 160` bytes) and what a compaction costs to put right.
    fn write_all_insts(&mut self) {
        if self.insts.is_empty() {
            return;
        }
        let recs: Vec<Inst> = self.insts.iter().map(|slot| slot.inst).collect();
        self.queue
            .write_buffer(&self.inst_buf, 0, bytemuck::cast_slice(&recs));
        self.matrix_bytes += recs.len() * INST_RECORD_BYTES as usize;
    }

    /// The stable id a prim draws under in the `prim-id` pass.
    fn prim_id(&mut self, path: &str) -> u32 {
        if let Some(id) = self.id_of.get(path) {
            return *id;
        }
        let id = (self.paths.len() + 1) as u32;
        self.paths.push(path.to_owned());
        self.id_of.insert(path.to_owned(), id);
        // A new id is a new list: the cached shareable copy is stale.
        self.paths_arc = None;
        id
    }

    /// The id→path list as a **shareable** copy: a frame hands it to the UI thread for a
    /// few hundred microseconds and gives it back, so it is cloned when a prim is added,
    /// never once per frame (a 2500-prim scene would clone 2500 strings a frame).
    pub fn paths_arc(&mut self) -> std::sync::Arc<Vec<String>> {
        if self.paths_arc.is_none() {
            self.paths_arc = Some(std::sync::Arc::new(self.paths.clone()));
        }
        self.paths_arc.clone().expect("just filled")
    }

    /// The beauty texture's generation: bumped whenever the texture is replaced (a
    /// resize). A UI that displays the texture directly re-registers on a new generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Copy the **current** frame back to the CPU: the beauty bytes and the prim ids.
    ///
    /// This is no longer part of drawing a frame (v1.10). A frame is displayed straight
    /// from [`Self::beauty_view`], so the round trip happens only when someone actually
    /// needs pixels on the CPU — a `capture` (a PNG to write) or a selection's outline —
    /// and a pick asks for one texel ([`Self::pick`]) instead of a whole buffer.
    pub fn grab(&mut self) -> Result<(Vec<u8>, Vec<u32>), Vec<Diagnostic>> {
        let (Some(vertex_buf), Some(index_buf)) = (&self.vertex_buf, &self.index_buf) else {
            return Err(vec![diag(
                "E_VIEW_NO_SCENE",
                "no snapshot has been uploaded yet",
            )]);
        };
        let _ = (vertex_buf, index_buf);
        let (width, height) = (self.width, self.height);
        let bpr = self.read_bytes_per_row;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer-grab"),
            });
        copy_to_buffer(
            &mut encoder,
            &self.beauty_tex,
            &self.beauty_read,
            width,
            height,
            bpr,
        );
        copy_to_buffer(&mut encoder, &self.id_tex, &self.id_read, width, height, bpr);
        self.queue.submit(Some(encoder.finish()));
        read_back(
            &self.device,
            &self.beauty_read,
            &self.id_read,
            width,
            height,
            bpr,
        )
    }

    /// The prim id at one pixel of the last frame: **one texel read back**, not a whole
    /// buffer (v1.10). Zero is the background.
    pub fn pick(&mut self, x: u32, y: u32) -> Result<u32, Vec<Diagnostic>> {
        if x >= self.width || y >= self.height {
            return Ok(0);
        }
        let buf = self.pick_read.get_or_insert_with(|| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("viewer-pick"),
                size: 256,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer-pick"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.id_tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = buf.slice(0..4);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv()
            .map_err(device_err)?
            .map_err(device_err)?;
        let data = slice.get_mapped_range().map_err(device_err)?;
        let id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        drop(data);
        buf.unmap();
        Ok(id)
    }

    /// Draw both passes with `cam` and read them back.
    ///
    /// The readback is what lets the caller annotate (a selection's outline is a
    /// CPU operation in `rusd_render::annotate`) and pick (`ids[y*w+x]` → path)
    /// with no second render, and it is the price of one frame: the two passes
    /// plus two texture→buffer copies, no device or pipeline work.
    pub fn render(
        &mut self,
        cam: &Camera,
        bounds: ([f64; 3], [f64; 3]),
        cfg: &RenderConfig,
        selected: Option<u32>,
    ) -> Result<RenderOutput, Vec<Diagnostic>> {
        let (Some(vertex_buf), Some(index_buf)) = (&self.vertex_buf, &self.index_buf) else {
            return Err(vec![diag(
                "E_VIEW_NO_SCENE",
                "no snapshot has been uploaded yet",
            )]);
        };
        let (width, height) = (self.width, self.height);

        let view_proj = camera_matrix(*cam, bounds, width, height);
        let mut uni = [0f32; 24];
        for (i, v) in view_proj.iter().enumerate() {
            uni[i] = *v as f32;
        }
        uni[16] = cfg.light_dir[0];
        uni[17] = cfg.light_dir[1];
        uni[18] = cfg.light_dir[2];
        uni[19] = cfg.ambient;
        uni[20] = cfg.diffuse;
        uni[21] = 2.0 / width as f32;
        uni[22] = 2.0 / height as f32;
        // Zero means "nothing selected": the outline pass discards every pixel then, and
        // no prim has id 0 (ids start at 1).
        uni[23] = selected.map(|id| id as f32).unwrap_or(0.0);
        self.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::cast_slice(&uni));

        // One submission: both passes, then both copies, so a frame never waits
        // on the GPU between the passes. The copies target two persistent
        // buffers, reused by the next frame.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer-frame"),
            });
        let bg = wgpu::Color {
            r: f64::from(cfg.background[0]),
            g: f64::from(cfg.background[1]),
            b: f64::from(cfg.background[2]),
            a: 1.0,
        };
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("beauty"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.beauty_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(bg),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(depth_attachment(&self.depth_view)),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.beauty_pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buf.slice(..));
            pass.set_index_buffer(index_buf.slice(..), wgpu::IndexFormat::Uint32);
            // **One draw per run of consecutive prims that share a mesh** (v1.14): the
            // index range is the prototype's, the instance range is the prims'. A box
            // scattered 2400 times is one `draw_indexed`, 36 indices, 2400 instances.
            // Indices are absolute, so the base vertex is 0.
            for draw in &self.draws {
                pass.draw_indexed(
                    draw.istart..draw.istart + draw.icount,
                    0,
                    draw.first..draw.first + draw.count,
                );
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("prim-id"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.id_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 0.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(depth_attachment(&self.depth_view)),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.id_pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buf.slice(..));
            pass.set_index_buffer(index_buf.slice(..), wgpu::IndexFormat::Uint32);
            // The same instanced draws as the beauty pass: the id comes out of the very
            // record the colour did, so a pick of one of 2400 boxes works.
            for draw in &self.draws {
                pass.draw_indexed(
                    draw.istart..draw.istart + draw.icount,
                    0,
                    draw.first..draw.first + draw.count,
                );
            }
        }
        // **The selection's outline** (v1.10): after the id pass, colour the border of the
        // selected prim in the beauty target. It reads the id buffer, writes only the
        // outline pixels (LoadOp::Load), and costs one full-screen triangle.
        if selected.is_some() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("selection-outline"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.beauty_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.outline_pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_bind_group(1, &self.id_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));

        // **No readback when drawing** (v1.10): the picture is displayed straight from
        // [`Self::beauty_view`], so a frame costs the two passes and nothing else. The
        // CPU copy is [`Self::grab`], asked for only by a capture or a selection outline.
        Ok(RenderOutput {
            width,
            height,
            beauty: Vec::new(),
            ids: Vec::new(),
            paths: Vec::new(),
            diags: Vec::new(),
        })
    }
}

struct Targets {
    beauty_tex: wgpu::Texture,
    /// The render target: `Rgba8UnormSrgb`, so the hardware encodes what `fs_beauty`
    /// writes (a linear colour) into the sRGB bytes `capture` reads back.
    beauty_view: wgpu::TextureView,
    /// The **same bytes**, handed to egui: a `Rgba8Unorm` view, i.e. no sRGB decode on
    /// the sample. egui's shader expects a texel it samples to be *gamma-encoded* (its own
    /// comment: “We expect "normal" textures that are NOT sRGB-aware”) and converts from
    /// there — so an sRGB view would decode a second time and the window would show
    /// `linear(capture)`: measured 196,218,237 in the PNG against 140,178,216 on screen.
    beauty_display: wgpu::TextureView,
    id_tex: wgpu::Texture,
    id_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
}

impl Targets {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let beauty_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewer-beauty"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: BEAUTY_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                // Sampled by egui itself (v1.10): the frame is shown straight from this
                // texture, so it has to be bindable.
                | wgpu::TextureUsages::TEXTURE_BINDING,
            // …through the non-sRGB view below (v1.11: the window was showing the picture
            // once too dark because egui decoded an already-decoded texel).
            view_formats: &[DISPLAY_FORMAT],
        });
        let id_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewer-prim-id"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ID_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                // Read by the outline pass (v1.10): the selection's outline is drawn on
                // the GPU from this buffer, so it has to be bindable.
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let depth_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewer-depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let beauty_view = beauty_tex.create_view(&Default::default());
        let beauty_display = beauty_tex.create_view(&wgpu::TextureViewDescriptor {
            label: Some("viewer-beauty-display"),
            format: Some(DISPLAY_FORMAT),
            ..Default::default()
        });
        let id_view = id_tex.create_view(&Default::default());
        let depth_view = depth_tex.create_view(&Default::default());
        Self {
            beauty_tex,
            beauty_view,
            beauty_display,
            id_tex,
            id_view,
            depth_view,
        }
    }
}

fn depth_attachment(view: &wgpu::TextureView) -> wgpu::RenderPassDepthStencilAttachment<'_> {
    wgpu::RenderPassDepthStencilAttachment {
        view,
        depth_ops: Some(wgpu::Operations {
            load: wgpu::LoadOp::Clear(1.0),
            store: wgpu::StoreOp::Store,
        }),
        stencil_ops: None,
    }
}

fn aligned_bytes_per_row(width: u32) -> u32 {
    (width * 4 + wgpu::COPY_BYTES_PER_ROW_ALIGNMENT - 1) & !(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT - 1)
}

fn readback_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewer-readback"),
        size: size.max(4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    })
}

fn copy_to_buffer(
    encoder: &mut wgpu::CommandEncoder,
    tex: &wgpu::Texture,
    buf: &wgpu::Buffer,
    width: u32,
    height: u32,
    bpr: u32,
) {
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bpr),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

fn read_back(
    device: &wgpu::Device,
    beauty_buf: &wgpu::Buffer,
    id_buf: &wgpu::Buffer,
    width: u32,
    height: u32,
    bpr: u32,
) -> Result<(Vec<u8>, Vec<u32>), Vec<Diagnostic>> {
    let bslice = beauty_buf.slice(..);
    let islice = id_buf.slice(..);
    let (btx, brx) = std::sync::mpsc::channel();
    let (itx, irx) = std::sync::mpsc::channel();
    bslice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = btx.send(r);
    });
    islice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = itx.send(r);
    });
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    brx.recv().map_err(device_err)?.map_err(device_err)?;
    irx.recv().map_err(device_err)?.map_err(device_err)?;
    let bdata = bslice.get_mapped_range().map_err(device_err)?;
    let idata = islice.get_mapped_range().map_err(device_err)?;

    let mut beauty = Vec::with_capacity((width * height * 4) as usize);
    let mut ids = Vec::with_capacity((width * height) as usize);
    for row in 0..height {
        let start = (row * bpr) as usize;
        let end = start + (width * 4) as usize;
        beauty.extend_from_slice(&bdata[start..end]);
        for px in idata[start..end].as_chunks::<4>().0 {
            ids.push(u32::from_le_bytes(*px));
        }
    }
    drop(bdata);
    drop(idata);
    beauty_buf.unmap();
    id_buf.unmap();
    Ok((beauty, ids))
}

fn device_err(e: impl std::fmt::Display) -> Vec<Diagnostic> {
    vec![diag("E_GPU", e)]
}

// --- bake (identical to rusd_render's) ------------------------------------

/// One prim's slab in the two scene buffers (v1.10). `vstart` is in vertices, `istart`
/// in indices; the slab's indices are **absolute** (already biased by `vstart`), which is
/// what lets a slab be rewritten in place or skipped altogether.
/// What a prim that `set_scene`'s geometry does not mention is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A whole composition: a prim the scene lacks is **gone** (its slab becomes free
    /// space).
    Whole,
    /// A filter over a scene already on the GPU: a prim the scene lacks is **hidden** —
    /// its slab stays and simply draws nothing, so lifting the filter re-bakes nothing.
    Filtered,
}

/// One **mesh's** slab in the two scene buffers (v1.10, shared since v1.14). `vstart` is
/// in vertices, `istart` in indices; the slab's indices are **absolute** (already biased
/// by `vstart`), which is what lets a slab be skipped, or copied device-to-device
/// unchanged when the arena compacts.
///
/// The map that owns it is keyed by the mesh's hash, so a slot **exists if and only if**
/// that exact geometry is on the GPU — which is why a mesh is baked once and never again.
#[derive(Clone)]
struct Slot {
    vstart: u32,
    vcount: u32,
    istart: u32,
    icount: u32,
    /// The slab's indices **as baked** (absolute: `slab base + mesh index`). Kept on the
    /// CPU because a compaction moves the slab's vertices to a new base, and an absolute
    /// index that is not rebased points into somebody else's vertices — boxes joined by
    /// long triangles was exactly that.
    indices: Vec<u32>,
}

/// One prim, as this sync wants it: what to draw and where. The mesh is reduced to its
/// hash at the door, so the rest of the sync never touches a mesh unless it bakes one.
struct Want {
    /// Index into `SceneGeometry::items` — where the mesh lives, if it must be baked.
    at: usize,
    /// The hash of the geometry this prim draws: the key of its slab.
    sig: u64,
    vcount: u32,
    icount: u32,
    transform: Mat,
    color: [f32; 4],
    /// A `Points` prim draws screen-space dots rather than its mesh (v1.14: the flag that
    /// says so lives in the instance record, not in every vertex).
    marker: bool,
}

/// One prim's place in the instance table: which slab draws it, what the GPU holds for
/// it, and whether it is drawn at all. Its index in `Viewport::insts` **is** the
/// `@builtin(instance_index)` the shader reads it by.
struct InstSlot {
    path: String,
    /// The mesh slab this prim draws — the mesh hash, as [`Want::sig`] has it.
    sig: u64,
    /// The record exactly as the GPU holds it (or holds nothing, before the first write).
    inst: Inst,
    /// False once the prim is gone: its record is waste until the table compacts.
    alive: bool,
    /// Whether this prim is drawn. A filter clears it without freeing the record, so the
    /// prim is back the moment the filter is, at no cost at all.
    shown: bool,
}

/// One draw call: `draw_indexed(istart..istart + icount, 0, first..first + count)` — one
/// mesh's indices, instanced over every consecutive prim that draws it.
struct Draw {
    istart: u32,
    icount: u32,
    first: u32,
    count: u32,
}

/// The vertices and indices one prim needs.
fn item_vertices(item: &GeoItem) -> u32 {
    match &item.markers {
        Some(markers) => (markers.len() * 6) as u32,
        None => item.mesh.positions.len() as u32,
    }
}

fn item_indices(item: &GeoItem) -> u32 {
    match &item.markers {
        Some(markers) => (markers.len() * 6) as u32,
        None => item.mesh.indices.len() as u32,
    }
}

/// A hash of the **geometry** `bake_mesh` reads, and of nothing else (v1.14). Where the
/// prim is drawn is the instance record's business, and what colour it is likewise, so a
/// moved prim and a re-coloured prim both leave this alone — and two prims with the same
/// geometry get the same hash, which is how they come to share one slab.
///
/// The mesh is hashed whole rather than by pointer: `extract` builds fresh vectors per
/// scene, so identity says nothing, and a hash that lies would leave a stale slab on the
/// GPU.
fn mesh_signature(item: &GeoItem) -> u64 {
    fn mix(state: &mut u64, value: u64) {
        // FNV-1a over the value, one multiply per word: cheap and order-sensitive.
        *state ^= value;
        *state = state.wrapping_mul(0x100_0000_01b3);
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    match &item.markers {
        Some(markers) => {
            mix(&mut h, 1);
            mix(&mut h, markers.len() as u64);
            for p in markers {
                for v in p {
                    mix(&mut h, u64::from(v.to_bits()));
                }
            }
        }
        None => {
            mix(&mut h, 2);
            mix(&mut h, item.mesh.positions.len() as u64);
            mix(&mut h, item.mesh.normals.len() as u64);
            for p in &item.mesh.positions {
                for v in p {
                    mix(&mut h, u64::from(v.to_bits()));
                }
            }
            for n in &item.mesh.normals {
                for v in n {
                    mix(&mut h, u64::from(v.to_bits()));
                }
            }
            mix(&mut h, item.mesh.indices.len() as u64);
            for centre in &item.mesh.indices {
                mix(&mut h, u64::from(*centre));
            }
        }
    }
    h
}

/// Bake a prim's **geometry** into vertices and indices: no colour, no id, no flag —
/// those are the instance record's (v1.14). `base` is the slab's first vertex, so the
/// indices are absolute and a slab can move without re-baking.
fn bake_mesh(item: &GeoItem, base: u32, vertices: &mut Vec<Vertex>, indices: &mut Vec<u32>) {
    if let Some(markers) = &item.markers {
        // One screen-space quad per marker (6 vertices): `normal` carries the
        // corner (-1..1) and the instance's flag tells the shader to build the dot in
        // clip space, so a point keeps its pixel size at any zoom. `pos` is the marker's
        // **local** centre; the prim's matrix puts it in the world (v1.11).
        for c in markers {
            let start = vertices.len() as u32;
            for (dx, dy) in [
                (-1.0, -1.0),
                (1.0, -1.0),
                (-1.0, 1.0),
                (1.0, -1.0),
                (1.0, 1.0),
                (-1.0, 1.0),
            ] {
                vertices.push(Vertex {
                    pos: *c,
                    normal: [dx, dy, 0.0],
                });
            }
            for k in 0..6 {
                indices.push(base + start + k);
            }
        }
    } else {
        let start = vertices.len() as u32;
        for (p, n) in item.mesh.positions.iter().zip(&item.mesh.normals) {
            vertices.push(Vertex {
                pos: *p,
                normal: *n,
            });
        }
        indices.extend(item.mesh.indices.iter().map(|&i| base + start + i));
    }
}

/// A prim's matrices exactly as the GPU holds them (f32, column-major), so a comparison
/// is a comparison with the buffer — a change below f32 precision writes nothing.
#[derive(Clone, Copy, PartialEq)]
struct PrimMatrices {
    model: [f32; 16],
    /// Inverse-transpose of `model`'s upper 3x3 (translation zero), so a squashed prim
    /// lights like a squashed prim.
    normal: [f32; 16],
}

impl PrimMatrices {
    /// The record a prim with local-to-world `m` should have.
    fn of(m: &Mat) -> Self {
        let mut model = [0f32; 16];
        for (to, from) in model.iter_mut().zip(m.iter()) {
            *to = *from as f32;
        }
        let nm = NormalMatrix::new(*m);
        let mut normal = [0f32; 16];
        normal[15] = 1.0;
        if nm.inv_det == 0.0 {
            // A matrix that scales an axis to nothing has no normal matrix; identity is
            // the only answer that is not NaN.
            normal[0] = 1.0;
            normal[5] = 1.0;
            normal[10] = 1.0;
        } else {
            for row in 0..3 {
                for col in 0..3 {
                    // `nm.c` is row-major; a column-major f32 matrix puts (row, col) at
                    // `col * 4 + row`.
                    normal[col * 4 + row] = (nm.c[row * 3 + col] * nm.inv_det) as f32;
                }
            }
        }
        Self { model, normal }
    }
}

/// Normal matrix = inverse-transpose of the upper 3x3 = cofactor / det.
struct NormalMatrix {
    c: [f64; 9],
    inv_det: f64,
}

impl NormalMatrix {
    fn new(m: Mat) -> Self {
        let r = [m[0], m[4], m[8], m[1], m[5], m[9], m[2], m[6], m[10]];
        let det = r[0] * (r[4] * r[8] - r[5] * r[7]) - r[1] * (r[3] * r[8] - r[5] * r[6])
            + r[2] * (r[3] * r[7] - r[4] * r[6]);
        let c = [
            r[4] * r[8] - r[5] * r[7],
            -(r[3] * r[8] - r[5] * r[6]),
            r[3] * r[7] - r[4] * r[6],
            -(r[1] * r[8] - r[2] * r[7]),
            r[0] * r[8] - r[2] * r[6],
            -(r[0] * r[7] - r[1] * r[6]),
            r[1] * r[5] - r[2] * r[4],
            -(r[0] * r[5] - r[2] * r[3]),
            r[0] * r[4] - r[1] * r[3],
        ];
        let inv_det = if det.abs() < 1e-12 { 0.0 } else { 1.0 / det };
        Self { c, inv_det }
    }
}

// --- camera math (identical to rusd_render's) -----------------------------

fn camera_matrix(cam: Camera, bounds: ([f64; 3], [f64; 3]), w: u32, h: u32) -> Mat {
    let (min, max) = bounds;
    let center = [
        (min[0] + max[0]) / 2.0,
        (min[1] + max[1]) / 2.0,
        (min[2] + max[2]) / 2.0,
    ];
    let radius = {
        let d = [max[0] - min[0], max[1] - min[1], max[2] - min[2]];
        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() / 2.0
    };
    let cam_dist = dist(cam.position, center);
    let far = (cam_dist + radius * 2.0).max(10.0);
    let near = (far / 10000.0).max(0.01);
    let aspect = w as f64 / h as f64;
    let view = look_at(cam.position, cam.look_at, [0.0, 0.0, 1.0]);
    let proj = perspective(cam.fov_deg, aspect, near, far);
    geometry::mat_mul(proj, view)
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn norm(v: [f64; 3]) -> [f64; 3] {
    let l = dot(v, v).sqrt();
    if l > 1e-12 {
        [v[0] / l, v[1] / l, v[2] / l]
    } else {
        [0.0, 0.0, 1.0]
    }
}
fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    dot(sub(a, b), sub(a, b)).sqrt()
}

fn look_at(eye: [f64; 3], target: [f64; 3], up: [f64; 3]) -> Mat {
    let f = norm(sub(target, eye));
    let s = norm(cross(f, up));
    let u = cross(s, f);
    let mut m = geometry::IDENTITY;
    m[0] = s[0];
    m[4] = s[1];
    m[8] = s[2];
    m[1] = u[0];
    m[5] = u[1];
    m[9] = u[2];
    m[2] = -f[0];
    m[6] = -f[1];
    m[10] = -f[2];
    m[12] = -dot(s, eye);
    m[13] = -dot(u, eye);
    m[14] = dot(f, eye);
    m
}

/// Perspective with wgpu clip z in [0, 1] (near→0, far→1).
fn perspective(fovy_deg: f64, aspect: f64, near: f64, far: f64) -> Mat {
    let f = 1.0 / (fovy_deg * std::f64::consts::PI / 360.0).tan();
    let mut m = [0.0; 16];
    m[0] = f / aspect;
    m[5] = f;
    m[10] = far / (near - far);
    m[11] = -1.0;
    m[14] = far * near / (near - far);
    m
}

#[cfg(test)]
mod tests {
    //! v1.14's claim, on a real device: **prims that share a mesh are one slab, one draw
    //! call and N instance records**, a drag is 64 bytes, a repeat is zero — and the
    //! picture proves each prim was drawn from its **own** record, which is exactly what a
    //! run starting at an instance offset other than 0 would get wrong.
    use super::*;
    use rusd_scene::geometry::{Camera, GeoItem, Mesh};

    fn headless() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("viewer-test"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .ok()
    }

    fn item(path: &str, mesh: Mesh, at: [f64; 3], color: [f32; 3]) -> GeoItem {
        let mut transform = [0.0f64; 16];
        transform[0] = 1.0;
        transform[5] = 1.0;
        transform[10] = 1.0;
        transform[15] = 1.0;
        transform[12] = at[0];
        transform[13] = at[1];
        transform[14] = at[2];
        GeoItem {
            path: path.to_owned(),
            mesh,
            transform,
            color,
            markers: None,
        }
    }

    /// Three boxes that share one mesh, then a sphere: the sphere's run starts at instance
    /// 3, so its draw is the one that catches a shader reading a record by the wrong index.
    fn scene() -> SceneGeometry {
        let red = [0.9, 0.1, 0.1];
        let green = [0.1, 0.8, 0.1];
        let cube = geometry::cube_mesh(2.0);
        SceneGeometry {
            items: vec![
                item("/World/A", cube.clone(), [-9.0, 0.0, 0.0], red),
                item("/World/B", cube.clone(), [-3.0, 0.0, 0.0], red),
                item("/World/C", cube, [3.0, 0.0, 0.0], red),
                item("/World/Z", geometry::sphere_mesh(1.2, 16, 12), [9.0, 0.0, 0.0], green),
            ],
            camera: None,
            bounds: ([-11.0, -5.0, -2.0], [11.0, 5.0, 2.0]),
            diags: Vec::new(),
        }
    }

    #[test]
    fn shared_meshes_are_one_slab_one_draw() {
        let Some((device, queue)) = headless() else {
            eprintln!("no DX12 adapter here; the GPU instancing test was skipped");
            return;
        };
        let geo = scene();
        let (w, h) = (160u32, 160u32);
        let mut vp = Viewport::new(device, queue, w, h);

        // --- the first upload: one slab per **mesh**, one record per prim --------------
        vp.set_scene(&geo, Scope::Whole);
        assert_eq!(vp.slots_live, 2, "three boxes share one slab; the sphere is the second");
        assert_eq!(vp.instances_live, 4);
        assert_eq!(vp.draws_count(), 2, "one draw per (mesh, run of instances)");
        assert_eq!(vp.rewritten, 2, "two meshes were baked, not four prims");
        assert_eq!(
            vp.matrix_bytes,
            4 * INST_RECORD_BYTES as usize,
            "one 160-byte record per prim and no per-prim geometry"
        );

        // --- the same scene again: not one byte crosses the bus -------------------
        vp.set_scene(&geo, Scope::Whole);
        assert_eq!(vp.matrix_bytes, 0);
        assert_eq!(vp.rewritten, 0);

        // --- a drag is 64 bytes: the model matrix, and not a vertex -------------------
        let mut moved = std::collections::BTreeMap::new();
        moved.insert("/World/B".to_owned(), [-3.0, 0.0, 1.5]);
        vp.set_scene_with_translates(&geo, &moved, Scope::Whole);
        assert_eq!(vp.matrix_bytes, 64, "a one-prim drag writes one model matrix");
        assert_eq!(vp.rewritten, 0, "no vertex was touched");
        vp.set_scene(&geo, Scope::Whole);

        // --- a filter is not a scene change --------------------------------------
        let only = crate::scene::isolate(&geo, Some("/World/Z"));
        vp.set_scene(&only, Scope::Filtered);
        assert_eq!(vp.matrix_bytes, 0, "hiding three prims uploads nothing");
        assert_eq!(vp.rewritten, 0);
        assert_eq!(vp.draws_count(), 1, "only the sphere's run is drawn");
        vp.set_scene(&geo, Scope::Whole);
        assert_eq!(vp.matrix_bytes, 0, "lifting the filter uploads nothing either");
        assert_eq!(vp.draws_count(), 2);

        // --- the picture: every prim from its own record ----------------------------
        let cam = Camera {
            // Z is up, so a camera on +Z looking at the origin has its up vector parallel
            // to its view direction — a degenerate view. Look along +Y instead.
            position: [0.0, -60.0, 8.0],
            look_at: [0.0, 0.0, 0.0],
            fov_deg: 40.0,
        };
        let cfg = RenderConfig {
            width: w,
            height: h,
            ambient: 1.0,
            diffuse: 0.0,
            background: [0.0, 0.0, 0.0],
            ..Default::default()
        };
        vp.render(&cam, geo.bounds, &cfg, None).expect("draw");
        let (beauty, ids) = vp.grab().expect("grab");
        let sphere_id = vp.id_of("/World/Z").expect("the sphere is on the GPU");
        assert_eq!(sphere_id, 4, "ids follow the order the prims were first named in");
        let mut seen = std::collections::BTreeSet::new();
        let (mut boxes, mut dots) = (0usize, 0usize);
        for (i, id) in ids.iter().enumerate() {
            if *id == 0 {
                continue;
            }
            seen.insert(*id);
            let px = &beauty[i * 4..i * 4 + 4];
            if *id == sphere_id {
                dots += 1;
                assert!(
                    px[1] > px[0],
                    "the sphere's run starts at instance 3: its pixels must be its own green"
                );
            } else {
                boxes += 1;
                assert!(px[0] > px[1], "a box's pixels must be red");
            }
        }
        assert_eq!(seen.len(), 4, "all four prims drew, each under its own id");
        assert!(boxes > 150 && dots > 40, "boxes={boxes} sphere={dots}");

        // A pick of the sphere's own pixels names the sphere: the `prim-id` pass read the
        // same instance record the beauty pass did.
        let at = ids.iter().position(|id| *id == sphere_id).expect("sphere pixels");
        let (x, y) = ((at % w as usize) as u32, (at / w as usize) as u32);
        assert_eq!(vp.pick(x, y).expect("pick"), sphere_id);
    }

    /// The mesh hash is what decides sharing, so it must ignore what a prim does with the
    /// mesh and see only the geometry itself.
    #[test]
    fn the_hash_sees_geometry_and_nothing_else() {
        let cube = geometry::cube_mesh(2.0);
        let a = item("/World/A", cube.clone(), [0.0, 0.0, 0.0], [0.9, 0.1, 0.1]);
        let moved = item("/World/B", cube.clone(), [7.0, 2.0, -1.0], [0.1, 0.1, 0.9]);
        let bigger = item("/World/C", geometry::cube_mesh(3.0), [0.0, 0.0, 0.0], [0.9, 0.1, 0.1]);
        assert_eq!(mesh_signature(&a), mesh_signature(&moved), "where and what colour: no");
        assert_ne!(mesh_signature(&a), mesh_signature(&bigger), "a different box is different");
    }

    /// **The instancing changed where the bytes come from, not the picture.**
    ///
    /// `rusd_render::render_with` is the untouched reference: it bakes world-space vertices
    /// in f64, with its own vertex layout and its own shader, and never looked at an
    /// instance record. Both paths are given the same scene, camera and config, and what
    /// comes back has to agree — except within a pixel or two of a silhouette, where f32
    /// world positions against f64 can fall on different sides of an edge.
    #[test]
    fn the_instanced_path_draws_what_the_baker_draws() {
        let Some((device, queue)) = headless() else {
            eprintln!("no DX12 adapter here; the reference-image test was skipped");
            return;
        };
        let mut geo = scene();
        let cam = Camera {
            position: [0.0, -60.0, 8.0],
            look_at: [0.0, 0.0, 0.0],
            fov_deg: 40.0,
        };
        geo.camera = Some(cam);
        let (w, h) = (160u32, 160u32);
        let cfg = RenderConfig {
            width: w,
            height: h,
            ambient: 1.0,
            diffuse: 0.0,
            background: [0.0, 0.0, 0.0],
            ..Default::default()
        };
        let baked = rusd_render::render_with(&geo, &cfg).expect("the baker draws the scene");

        let mut vp = Viewport::new(device, queue, w, h);
        vp.set_scene(&geo, Scope::Whole);
        vp.render(&cam, geo.bounds, &cfg, None).expect("the viewport draws");
        let (instanced, ids) = vp.grab().expect("grab");

        assert_eq!(baked.beauty.len(), instanced.len());
        let n = baked.beauty.len();
        let (mut sum, mut worst, mut differing, mut ids_apart) = (0u64, 0u8, 0usize, 0usize);
        for (k, (&a, &b)) in baked.beauty.iter().zip(&instanced).enumerate() {
            let d = a.abs_diff(b);
            sum += u64::from(d);
            worst = worst.max(d);
            differing += usize::from(d > 8);
            // The id buffer is integers: an edge pixel is the only place they can disagree.
            if let Some(&ref_id) = baked.ids.get(k / 4) {
                ids_apart += usize::from(ref_id != ids[k / 4]);
            }
        }
        let mean = sum as f64 / n as f64;
        println!(
            "vs rusd_render: mean |Δ| = {mean:.3}, worst = {worst}, pixels off by >8 = {:.2}%, ids apart = {:.2}%",
            100.0 * differing as f64 / (n / 4) as f64,
            100.0 * ids_apart as f64 / (n / 4) as f64
        );
        assert!(mean < 0.5, "the two renderers must agree on average: mean |Δ| = {mean}");
        assert!(
            differing * 100 < (n / 4),
            "under 1% of pixels may sit on a silhouette differently"
        );
        assert!(
            ids_apart * 100 < (n / 4),
            "the prim-id pass must name the same prim at the same pixels"
        );
    }
}
