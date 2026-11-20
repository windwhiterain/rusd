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
//! set_scene(owned geometry)   // bake + upload vertex/index buffers
//! render(camera, config)      // uniforms + 2 passes + readback (no device, no shader build)
//! ```
//!
//! Everything drawn here is `rusd_render`'s own pipeline, shader and math,
//! copied with the lifetime split so the passes can be re-run: this is the same
//! renderer, not a second opinion about the pixels. There is one deliberate
//! difference (v1.11): a prim's local-to-world matrix is applied **by the vertex
//! shader**, not baked into its vertices, because that is what makes a moved prim a
//! 64-byte write. The arithmetic is then f32 where `rusd_render` bakes in f64, so the
//! two can differ in the last bit of a world position; geometry, lighting and camera
//! math are otherwise the same code.
//!
//! Error-tolerant by the crate's rule: every failure is a [`Diagnostic`] with a
//! stable `E_` code, never a panic.

use bytemuck::{Pod, Zeroable};
use rusd_core::Diagnostic;
use rusd_render::RenderConfig;
use rusd_render::render::RenderOutput;
use rusd_scene::geometry::{self, Camera, GeoItem, Mat, SceneGeometry};

use crate::diag;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
    color: [f32; 3],
    id: u32,
    flag: f32,
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

// --- the prim's own matrices (v1.11) -----------------------------------------
// One record per **prim id**, so a prim that moves is a 64-byte write into
// `model` and its vertices are never touched again: the vertex buffer holds the
// prim's local geometry, and the world is what this record says it is.
struct Prim {
    model: mat4x4<f32>,
    // inverse-transpose of `model`'s upper 3x3, translation zero: a normal is not a
    // position, so a squashed prim must not get its lighting inverted.
    normal: mat4x4<f32>,
};
@group(0) @binding(1) var<storage, read> prims: array<Prim>;

@vertex
fn vs_main(
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) id: u32,
    @location(4) flag: f32,
) -> VsOut {
    var out: VsOut;
    out.color = color;
    out.id = id;
    out.flag = flag;
    // `pos` is the prim's **local** position; its matrix is what puts it in the world.
    let p = prims[id];
    let world = p.model * vec4<f32>(pos, 1.0);
    if (flag > 0.5) {
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
        out.normal = (p.normal * vec4<f32>(normal, 0.0)).xyz;
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
}
"#;

/// Beauty target format: sRGB, so written values are gamma-encoded bytes and the
/// CPU side can hand them to egui, whose textures are sampled as gamma
/// (`egui`'s `fs_main_gamma_framebuffer`). Same as `rusd_render`.
const BEAUTY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const ID_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Uint;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// The persistent-device viewport: pipelines and a uniform buffer built once,
/// the scene's vertex/index buffers rebuilt only when the snapshot changes.
/// How many bytes one prim's matrix record takes: a model `mat4x4<f32>` and its
/// normal matrix, so a moved prim is exactly one 64-byte write.
const PRIM_RECORD_BYTES: u64 = 128;

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

    /// The per-prim matrix records (v1.11), indexed by prim id: `model` at `id * 128`,
    /// the normal matrix at `id * 128 + 64`. **A moved prim writes only one of them** —
    /// a drag is 64 bytes, never a vertex.
    prim_buf: wgpu::Buffer,
    /// Capacity of [`Self::prim_buf`] in records.
    prim_cap: u32,
    /// What each prim's record holds now. An absent entry is a record the next sync
    /// must write; a matching one is why a drag writes nothing for the other prims.
    prim_mats: std::collections::BTreeMap<String, PrimMatrices>,
    /// How many bytes the last sync wrote into [`Self::prim_buf`]. What makes 「拖动一个
    /// prim 只写 64 字节」 an observation instead of a claim.
    pub matrix_bytes: usize,

    // Targets, rebuilt on resize.
    beauty_tex: wgpu::Texture,
    beauty_view: wgpu::TextureView,
    id_tex: wgpu::Texture,
    id_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,

    // Scene buffers, **kept across snapshots** (v1.10: 渲染是 incremental 的 — a USD
    // delta is a GPU buffer delta). Each prim owns a slab; only the slabs whose prim
    // moved are rewritten, so a drag costs one prim's vertices and a one-operator patch
    // costs that operator's prims. The buffers are an append-only arena: a slab that
    // outgrows its space is appended at the tail and the old space is waste until the
    // arena is compacted (at half waste, or when it must grow).
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
    slots: std::collections::BTreeMap<String, Slot>,
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
    /// The draws the live slabs leave, as `(first index, count)` runs over the index
    /// buffer; contiguous slabs merge, so a freshly compacted arena is one draw.
    draws: Vec<(u32, u32)>,
    /// How many prims the last sync actually rewrote, and how many draws it left. What
    /// makes "only the delta went to the GPU" observable instead of a claim.
    pub rewritten: usize,
    pub slots_live: usize,

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
                // The per-prim matrices (v1.11): one record per prim id, read by the
                // vertex shader, so a moved prim is a 64-byte write and no vertex moves.
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
        // The per-prim matrix records (v1.11): one 128-byte record per prim id. Never
        // sized zero — one record of slack keeps the binding valid before a scene.
        let prim_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer-prim-matrices"),
            size: PRIM_RECORD_BYTES * 64,
            // COPY_SRC as well: a grown record table is copied to the new buffer
            // **on the GPU**, like every other buffer move here.
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let bind_group = Self::make_bind_group(&device, &bgl, &uniform_buf, &prim_buf);
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
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
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 24,
                    shader_location: 2,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Uint32,
                    offset: 36,
                    shader_location: 3,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32,
                    offset: 40,
                    shader_location: 4,
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
            prim_buf,
            prim_cap: 64,
            prim_mats: std::collections::BTreeMap::new(),
            matrix_bytes: 0,
            beauty_tex: targets.beauty_tex,
            beauty_view: targets.beauty_view,
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
        self.slots.contains_key(path).then_some(id)
    }

    /// The beauty texture, for a UI to display without a CPU round trip.
    pub fn beauty_view(&self) -> &wgpu::TextureView {
        &self.beauty_view
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
    /// matrix records. Rebuilt whenever [`Self::prim_buf`] is replaced.
    fn make_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        uniform_buf: &wgpu::Buffer,
        prim_buf: &wgpu::Buffer,
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
                    resource: prim_buf.as_entire_binding(),
                },
            ],
        })
    }

    /// Bring the GPU geometry up to `geo`: **only the prims that actually changed are
    /// baked and written** (v1.10 — a USD delta is a GPU buffer delta).
    ///
    /// `overrides` is a gizmo drag's per-prim translation, and its keys are the only
    /// prims that can have moved — `only` is what says so, and with it set a drag costs
    /// one prim's vertices instead of the scene's. `None` means "diff the whole scene",
    /// which is what a fresh snapshot needs: it still bakes nothing whose signature
    /// matches the slab already on the GPU.
    pub fn set_scene(&mut self, geo: &SceneGeometry) {
        self.sync(geo, &std::collections::BTreeMap::new(), None);
    }

    /// The same, with a few prims moved: `overrides` maps a prim path to the
    /// world translation it should be drawn at. This is a gizmo drag's preview —
    /// local, so the prim follows the pointer without waiting for the server's
    /// snapshot.
    ///
    /// **A drag is a 64-byte write** (v1.11): the override lands in the prim's matrix
    /// record, not in its vertices, so the scene's vertex buffer is not touched at all
    /// and `matrix_bytes` comes out `64` for a one-prim drag.
    pub fn set_scene_with_translates(
        &mut self,
        geo: &SceneGeometry,
        overrides: &std::collections::BTreeMap<String, [f64; 3]>,
    ) {
        let only: std::collections::BTreeSet<String> = overrides.keys().cloned().collect();
        self.sync(geo, overrides, Some(&only));
    }

    fn sync(
        &mut self,
        geo: &SceneGeometry,
        overrides: &std::collections::BTreeMap<String, [f64; 3]>,
        only: Option<&std::collections::BTreeSet<String>>,
    ) {
        let vertex_stride = std::mem::size_of::<Vertex>() as u64;
        self.uploaded += 1;
        self.rewritten = 0;
        self.matrix_bytes = 0;
        if self.vertex_buf.is_none() {
            self.allocate(1024, 4096);
        }
        // What the scene would need if every prim were appended fresh, and how much of
        // the arena is already dead: either answer is a compaction.
        let need_v: u32 = geo.items.iter().map(item_vertices).sum();
        let need_i: u32 = geo.items.iter().map(item_indices).sum();
        let waste_v = self.vertex_len.saturating_sub(self.live_vertices);
        let waste_i = self.index_len.saturating_sub(self.live_indices);
        let grow = self.vertex_len + need_v > self.vertex_cap || self.index_len + need_i > self.index_cap;
        let stale = (self.vertex_cap > 8192 && waste_v > self.vertex_cap / 2)
            || (self.index_cap > 8192 && waste_i > self.index_cap / 2);
        if grow || stale {
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
            let mut slabs: Vec<(String, Slot)> = self
                .slots
                .iter()
                .map(|(path, slot)| (path.clone(), slot.clone()))
                .collect();
            slabs.sort_by_key(|(_, slot)| slot.vstart);
            let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer-compact"),
            });
            if let (Some(from_v), Some(from_i)) = (&old_vertex, &old_index) {
                for (path, slot) in slabs {
                    if slot.vcount > 0 {
                        encoder.copy_buffer_to_buffer(
                            from_v,
                            u64::from(slot.vstart) * stride,
                            &new_vertex,
                            u64::from(vertex_at) * stride,
                            u64::from(slot.vcount) * stride,
                        );
                    }
                    if slot.icount > 0 {
                        encoder.copy_buffer_to_buffer(
                            from_i,
                            u64::from(slot.istart) * 4,
                            &new_index,
                            u64::from(index_at) * 4,
                            u64::from(slot.icount) * 4,
                        );
                    }
                    moved.insert(
                        path,
                        Slot {
                            vstart: vertex_at,
                            istart: index_at,
                            ..slot
                        },
                    );
                    vertex_at += slot.vcount;
                    index_at += slot.icount;
                }
            }
            self.queue.submit(Some(encoder.finish()));
            self.slots = moved;
            self.vertex_len = vertex_at;
            self.index_len = index_at;
            self.live_vertices = vertex_at;
            self.live_indices = index_at;
        }
        let mut live: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        // Room for every prim this scene could mint an id for, **in one go**: a first upload
        // of a 2400-prim scene would otherwise recreate the table (and its bind group) seven
        // times on the way up from 64 records.
        self.grow_prim((self.paths.len() + geo.items.len() + 1) as u32);
        for item in &geo.items {
            live.insert(item.path.clone());
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
            // A prim the hint says cannot have moved keeps its slab and its matrices
            // without even being hashed: that is what makes a drag a one-prim delta
            // rather than a scene walk.
            let skipped = only.is_some_and(|set| !set.contains(&item.path));
            let want = PrimMatrices::of(&transform);
            if skipped
                && self.slots.contains_key(&item.path)
                && self.prim_mats.get(&item.path) == Some(&want)
            {
                continue;
            }
            let id = self.prim_id(&item.path);
            self.grow_prim(id + 1);
            // **Only a changed mesh is re-baked** (v1.11): a prim that merely moved keeps
            // its slab — its vertices stay local, and the move is one matrix record.
            let signature = signature_of(item);
            let stale = self
                .slots
                .get(&item.path)
                .map_or(true, |slot| slot.signature != signature);
            if !stale {
                self.write_prim(&item.path, id, want);
                continue;
            }
            let (vcount, icount) = (item_vertices(item), item_indices(item));
            // In place when the prim's slab is exactly the right size (the common case:
            // a re-coloured prim); otherwise a fresh slab at the tail.
            let in_place = self.slots.get(&item.path).map_or(false, |s| {
                s.vcount == vcount && s.icount == icount
            });
            let (vstart, istart) = if in_place {
                let s = &self.slots[&item.path];
                (s.vstart, s.istart)
            } else {
                (self.vertex_len, self.index_len)
            };
            let mut vertices: Vec<Vertex> = Vec::with_capacity(vcount as usize);
            let mut indices: Vec<u32> = Vec::with_capacity(icount as usize);
            bake_item(item, id, vstart, &mut vertices, &mut indices);
            let Some(vertex_buf) = &self.vertex_buf else { return };
            let Some(index_buf) = &self.index_buf else { return };
            if !vertices.is_empty() {
                self.queue.write_buffer(
                    vertex_buf,
                    u64::from(vstart) * vertex_stride,
                    bytemuck::cast_slice(&vertices),
                );
            }
            if !indices.is_empty() {
                self.queue.write_buffer(
                    index_buf,
                    u64::from(istart) * 4,
                    bytemuck::cast_slice(&indices),
                );
            }
            if !in_place {
                self.vertex_len += vcount;
                self.index_len += icount;
                self.live_vertices += vcount;
                self.live_indices += icount;
            }
            self.slots.insert(
                item.path.clone(),
                Slot {
                    signature,
                    vstart,
                    vcount,
                    istart,
                    icount,
                },
            );
            self.rewritten += 1;
            self.write_prim(&item.path, id, want);
        }
        // Prims the scene no longer has: their ids stay (a pick may still name one for
        // a frame), their slabs become waste.
        let gone: Vec<String> = self
            .slots
            .keys()
            .filter(|p| !live.contains(p.as_str()))
            .cloned()
            .collect();
        for path in gone {
            if let Some(slot) = self.slots.remove(&path) {
                self.live_vertices = self.live_vertices.saturating_sub(slot.vcount);
                self.live_indices = self.live_indices.saturating_sub(slot.icount);
            }
        }
        // The draws the slabs leave: contiguous runs merge, so an arena whose slabs were
        // appended together is one draw, and a fragmented one a few more.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        let mut slabs: Vec<&Slot> = self.slots.values().collect();
        slabs.sort_by_key(|s| s.istart);
        for slot in slabs {
            if slot.icount == 0 {
                continue;
            }
            match runs.last_mut() {
                Some((start, count)) if *start + *count == slot.istart => *count += slot.icount,
                _ => runs.push((slot.istart, slot.icount)),
            }
        }
        self.draws = runs;
        self.slots_live = self.slots.len();
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

    /// Make room for `records` per-prim records. The old table is copied to the new
    /// buffer **on the GPU**, and [`Self::prim_mats`] is kept, so a scene that grew does
    /// not rewrite the records that already match.
    fn grow_prim(&mut self, records: u32) {
        if records <= self.prim_cap {
            return;
        }
        let cap = records.max(self.prim_cap.saturating_mul(2));
        let new = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewer-prim-matrices"),
            size: u64::from(cap) * PRIM_RECORD_BYTES,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer-grow-prims"),
            });
        encoder.copy_buffer_to_buffer(
            &self.prim_buf,
            0,
            &new,
            0,
            u64::from(self.prim_cap) * PRIM_RECORD_BYTES,
        );
        self.queue.submit(Some(encoder.finish()));
        self.prim_buf = new;
        self.prim_cap = cap;
        let bg = Self::make_bind_group(
            &self.device,
            &self.uniform_bgl,
            &self.uniform_buf,
            &self.prim_buf,
        );
        self.bind_group = bg;
    }

    /// Point a prim's record at `want`, writing **only the halves that differ**: a drag
    /// moves the model matrix and leaves the normal matrix alone, so a drag of one prim
    /// writes 64 bytes and touches no vertex.
    fn write_prim(&mut self, path: &str, id: u32, want: PrimMatrices) {
        let have = self.prim_mats.get(path).copied();
        let base = u64::from(id) * PRIM_RECORD_BYTES;
        let model = have.map_or(true, |h| h.model != want.model);
        let normal = have.map_or(true, |h| h.normal != want.normal);
        // The record is contiguous, so a prim whose matrices both changed (a first upload, a
        // rotated parent) is **one** 128-byte write; a drag is one 64-byte write.
        if model && normal {
            let mut record = [0f32; 32];
            record[..16].copy_from_slice(&want.model);
            record[16..].copy_from_slice(&want.normal);
            self.queue
                .write_buffer(&self.prim_buf, base, bytemuck::cast_slice(&record));
            self.matrix_bytes += 128;
        } else if model || normal {
            let (at, half) = if model {
                (base, &want.model)
            } else {
                (base + 64, &want.normal)
            };
            self.queue
                .write_buffer(&self.prim_buf, at, bytemuck::cast_slice(half));
            self.matrix_bytes += 64;
        }
        self.prim_mats.insert(path.to_owned(), want);
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
            // One draw per *run* of live slabs (v1.10): the index buffer is an arena whose
            // slabs are appended in prim order, so an uncompacted scene is one run and a
            // fragmented one is a few. Indices are absolute, so the base vertex is 0.
            for (start, count) in &self.draws {
                pass.draw_indexed(*start..*start + *count, 0, 0..1);
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
            for (start, count) in &self.draws {
                pass.draw_indexed(*start..*start + *count, 0, 0..1);
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
    beauty_view: wgpu::TextureView,
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
            view_formats: &[],
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
        let id_view = id_tex.create_view(&Default::default());
        let depth_view = depth_tex.create_view(&Default::default());
        Self {
            beauty_tex,
            beauty_view,
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
/// One prim's slab in the two scene buffers (v1.10). `vstart` is in vertices, `istart`
/// in indices; the slab's indices are **absolute** (already biased by `vstart`), which is
/// what lets a slab be rewritten in place, skipped, or copied device-to-device unchanged.
#[derive(Clone)]
struct Slot {
    /// What the slab was baked from. Equal means "the prim did not move" and the GPU is
    /// left alone — the whole point of the delta path.
    signature: u64,
    vstart: u32,
    vcount: u32,
    istart: u32,
    icount: u32,
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

/// A hash of everything `bake_item` reads — and it reads **only the prim's own
/// geometry** (v1.11): where the prim is drawn is the matrix record's business, so a
/// move leaves this signature alone and the slab on the GPU untouched. The mesh is hashed
/// whole rather than by pointer: `extract` builds fresh vectors per scene, so identity
/// says nothing, and a signature that lies would leave a stale slab on the GPU.
fn signature_of(item: &GeoItem) -> u64 {
    fn mix(state: &mut u64, value: u64) {
        // FNV-1a over the value, one multiply per word: cheap and order-sensitive.
        *state ^= value;
        *state = state.wrapping_mul(0x100_0000_01b3);
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in item.color.iter() {
        mix(&mut h, u64::from(v.to_bits()));
    }
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
            for p in &item.mesh.positions {
                for v in p {
                    mix(&mut h, u64::from(v.to_bits()));
                }
            }
            for center in &item.mesh.indices {
                mix(&mut h, u64::from(*center));
            }
        }
    }
    h
}

fn bake_item(
    item: &GeoItem,
    id: u32,
    base: u32,
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
) {
    if let Some(markers) = &item.markers {
        // One screen-space quad per marker (6 vertices): `normal` carries the
        // corner (-1..1) and `flag` tells the shader to build the dot in clip
        // space, so a point keeps its pixel size at any zoom. `pos` is the marker's
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
                    color: item.color,
                    id,
                    flag: 1.0,
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
                color: item.color,
                id,
                flag: 0.0,
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
