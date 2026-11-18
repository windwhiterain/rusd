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
//! copied with the lifetime split so the passes can be re-run; the parity test
//! `gpu_renders_byte_for_byte_what_rusd_render_does` pins the two outputs equal,
//! so this is the same renderer and not a second opinion about the pixels.
//!
//! Error-tolerant by the crate's rule: every failure is a [`Diagnostic`] with a
//! stable `E_` code, never a panic.

use bytemuck::{Pod, Zeroable};
use rusd_core::Diagnostic;
use rusd_render::RenderConfig;
use rusd_render::render::RenderOutput;
use rusd_scene::geometry::{self, Camera, Mat, SceneGeometry};
use wgpu::util::DeviceExt;

use crate::diag;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
    color: [f32; 3],
    id: u32,
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
};

@vertex
fn vs_main(
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) id: u32,
) -> VsOut {
    var out: VsOut;
    out.pos = u.view_proj * vec4<f32>(pos, 1.0);
    out.normal = normal;
    out.color = color;
    out.id = id;
    return out;
}

@fragment
fn fs_beauty(in: VsOut) -> @location(0) vec4<f32> {
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
"#;

/// Beauty target format: sRGB, so written values are gamma-encoded bytes and the
/// CPU side can hand them to egui, whose textures are sampled as gamma
/// (`egui`'s `fs_main_gamma_framebuffer`). Same as `rusd_render`.
const BEAUTY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const ID_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Uint;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// The persistent-device viewport: pipelines and a uniform buffer built once,
/// the scene's vertex/index buffers rebuilt only when the snapshot changes.
pub struct Viewport {
    device: wgpu::Device,
    queue: wgpu::Queue,
    width: u32,
    height: u32,

    beauty_pipeline: wgpu::RenderPipeline,
    id_pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buf: wgpu::Buffer,

    // Targets, rebuilt on resize.
    beauty_tex: wgpu::Texture,
    beauty_view: wgpu::TextureView,
    id_tex: wgpu::Texture,
    id_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,

    // Scene buffers, rebuilt on set_scene.
    vertex_buf: Option<wgpu::Buffer>,
    index_buf: Option<wgpu::Buffer>,
    index_count: u32,
    paths: Vec<String>,

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
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
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
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewer-uniforms"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });
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

        let targets = Targets::new(&device, width, height);
        let read_bytes_per_row = aligned_bytes_per_row(width);
        let read_len = u64::from(read_bytes_per_row * height);
        let beauty_read = readback_buffer(&device, read_len);
        let id_read = readback_buffer(&device, read_len);

        Self {
            device,
            queue,
            width,
            height,
            beauty_pipeline,
            id_pipeline,
            bind_group,
            uniform_buf,
            beauty_tex: targets.beauty_tex,
            beauty_view: targets.beauty_view,
            id_tex: targets.id_tex,
            id_view: targets.id_view,
            depth_view: targets.depth_view,
            vertex_buf: None,
            index_buf: None,
            index_count: 0,
            paths: Vec::new(),
            beauty_read,
            id_read,
            read_bytes_per_row,
            uploaded: 0,
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
    }

    /// Bake `geo`'s world-space geometry onto the GPU. This is the one step per
    /// snapshot: the camera is not baked, so orbiting afterwards re-uploads
    /// nothing.
    pub fn set_scene(&mut self, geo: &SceneGeometry) {
        self.upload(geo, &std::collections::BTreeMap::new());
    }

    /// The same, with a few prims moved: `overrides` maps a prim path to the
    /// world translation it should be drawn at. This is a gizmo drag's preview —
    /// local, so the prim follows the pointer without waiting for the server's
    /// snapshot — and it is the only reason a drag costs a re-bake.
    pub fn set_scene_with_translates(
        &mut self,
        geo: &SceneGeometry,
        overrides: &std::collections::BTreeMap<String, [f64; 3]>,
    ) {
        self.upload(geo, overrides);
    }

    fn upload(
        &mut self,
        geo: &SceneGeometry,
        overrides: &std::collections::BTreeMap<String, [f64; 3]>,
    ) {
        let (vertices, indices, paths) = bake(geo, overrides);
        self.vertex_buf = Some(
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("viewer-vertices"),
                    contents: bytemuck::cast_slice(&vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                }),
        );
        self.index_buf = Some(
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("viewer-indices"),
                    contents: bytemuck::cast_slice(&indices),
                    usage: wgpu::BufferUsages::INDEX,
                }),
        );
        self.index_count = indices.len() as u32;
        self.paths = paths;
        self.uploaded += 1;
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
            pass.draw_indexed(0..self.index_count, 0, 0..1);
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
            pass.draw_indexed(0..self.index_count, 0, 0..1);
        }
        let bpr = self.read_bytes_per_row;
        copy_to_buffer(
            &mut encoder,
            &self.beauty_tex,
            &self.beauty_read,
            width,
            height,
            bpr,
        );
        copy_to_buffer(
            &mut encoder,
            &self.id_tex,
            &self.id_read,
            width,
            height,
            bpr,
        );
        self.queue.submit(Some(encoder.finish()));

        let (beauty, ids) = read_back(
            &self.device,
            &self.beauty_read,
            &self.id_read,
            width,
            height,
            bpr,
        )?;
        Ok(RenderOutput {
            width,
            height,
            beauty,
            ids,
            paths: self.paths.clone(),
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
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let id_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewer-prim-id"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ID_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
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

fn bake(
    geo: &SceneGeometry,
    overrides: &std::collections::BTreeMap<String, [f64; 3]>,
) -> (Vec<Vertex>, Vec<u32>, Vec<String>) {
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    for (i, item) in geo.items.iter().enumerate() {
        let id = (i + 1) as u32;
        let base = vertices.len() as u32;
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
        let nm = NormalMatrix::new(transform);
        for (p, n) in item.mesh.positions.iter().zip(&item.mesh.normals) {
            let wp =
                geometry::mat_transform_point(transform, [p[0] as f64, p[1] as f64, p[2] as f64]);
            let wn = nm.apply(*n);
            vertices.push(Vertex {
                pos: [wp[0] as f32, wp[1] as f32, wp[2] as f32],
                normal: wn,
                color: item.color,
                id,
            });
        }
        indices.extend(item.mesh.indices.iter().map(|&idx| base + idx));
        paths.push(item.path.clone());
    }
    (vertices, indices, paths)
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

    fn apply(&self, n: [f32; 3]) -> [f32; 3] {
        if self.inv_det == 0.0 {
            return normalize(n);
        }
        let n = [n[0] as f64, n[1] as f64, n[2] as f64];
        let mut out = [0.0f64; 3];
        for (row, o) in out.iter_mut().enumerate() {
            *o = (self.c[row * 3] * n[0] + self.c[row * 3 + 1] * n[1] + self.c[row * 3 + 2] * n[2])
                * self.inv_det;
        }
        normalize([out[0] as f32, out[1] as f32, out[2] as f32])
    }
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 1e-12 {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 0.0, 1.0]
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
