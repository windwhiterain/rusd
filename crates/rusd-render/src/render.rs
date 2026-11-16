//! Headless wgpu rendering core (SPEC §7): bake extracted geometry to world-space
//! GPU buffers, render a beauty pass and a prim-id pass, read both back.
//!
//! - Beauty: simple forward directional + ambient lighting into an sRGB target.
//! - Prim-id: each prim flat-shaded with its stable id (index+1) into an `R32Uint`
//!   target, so `pick(x, y)` reads an exact integer id → prim path (0 = background).
//! - Z-up right-handed camera (D7); wgpu clip z in [0, 1].
//! - Error-tolerant: a device/render failure returns diagnostics, never panics.
//!
//! v0 bakes one world-space mesh and issues a single draw per pass (GPU instancing
//! and the content-hash mesh cache land later in M4).

use bytemuck::{Pod, Zeroable};
use rusd_core::Diagnostic;
use rusd_scene::geometry::{self, Mat, SceneGeometry};
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
    color: [f32; 3],
    id: u32,
}

/// The result of a headless render: beauty pixels, per-pixel prim ids, and the
/// id→path table. `ids[y * width + x]` is 0 for background, else `paths[id - 1]`.
pub struct RenderOutput {
    pub width: u32,
    pub height: u32,
    pub beauty: Vec<u8>,
    pub ids: Vec<u32>,
    pub paths: Vec<String>,
    pub diags: Vec<Diagnostic>,
}

impl RenderOutput {
    /// Prim path at a pixel, or `None` for background / out of bounds.
    pub fn pick(&self, x: u32, y: u32) -> Option<&str> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let id = self.ids[(y * self.width + x) as usize];
        if id == 0 {
            None
        } else {
            self.paths.get((id - 1) as usize).map(String::as_str)
        }
    }
}

const SHADER: &str = r#"
@group(0) @binding(0) var<uniform> view_proj: mat4x4<f32>;

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
    out.pos = view_proj * vec4<f32>(pos, 1.0);
    out.normal = normal;
    out.color = color;
    out.id = id;
    return out;
}

@fragment
fn fs_beauty(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let l = normalize(vec3<f32>(0.5, -0.6, 0.8));
    let diff = max(dot(n, l), 0.0);
    let c = in.color * (0.28 + 0.82 * diff);
    return vec4<f32>(c, 1.0);
}

@fragment
fn fs_id(in: VsOut) -> @location(0) u32 {
    return in.id;
}
"#;

/// Render a scene headlessly at `width`×`height`.
pub fn render(
    geo: &SceneGeometry,
    width: u32,
    height: u32,
) -> Result<RenderOutput, Vec<Diagnostic>> {
    pollster::block_on(render_async(geo, width, height))
}

fn device_err(e: impl std::fmt::Display) -> Vec<Diagnostic> {
    vec![Diagnostic::error("E_RENDER_DEVICE", e.to_string())]
}

async fn render_async(
    geo: &SceneGeometry,
    width: u32,
    height: u32,
) -> Result<RenderOutput, Vec<Diagnostic>> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        })
        .await
        .map_err(device_err)?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("rusd-render"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        })
        .await
        .map_err(device_err)?;

    // --- bake world-space geometry ---
    let (vertices, indices, paths) = bake(geo);
    let vertex_bytes = bytemuck::cast_slice(&vertices);
    let index_bytes = bytemuck::cast_slice(&indices);
    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("vertices"),
        contents: vertex_bytes,
        usage: wgpu::BufferUsages::VERTEX,
    });
    let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("indices"),
        contents: index_bytes,
        usage: wgpu::BufferUsages::INDEX,
    });

    // --- camera ---
    let cam = geo.camera.unwrap_or(geometry::Camera {
        position: [0.0, -10.0, 5.0],
        look_at: [0.0; 3],
        fov_deg: 50.0,
    });
    let view_proj = camera_matrix(cam, geo.bounds, width, height);
    let vp_f32: [f32; 16] = view_proj.map(|v| v as f32);
    let camera_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("camera"),
        contents: bytemuck::cast_slice(&vp_f32),
        usage: wgpu::BufferUsages::UNIFORM,
    });

    // --- targets ---
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let beauty_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("beauty"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let id_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("prim-id"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Uint,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let depth_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("depth"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let beauty_view = beauty_tex.create_view(&Default::default());
    let id_view = id_tex.create_view(&Default::default());
    let depth_view = depth_tex.create_view(&Default::default());

    // --- pipelines ---
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("rusd"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
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
        format: wgpu::TextureFormat::Depth32Float,
        depth_write_enabled: Some(true),
        depth_compare: Some(wgpu::CompareFunction::Less),
        stencil: Default::default(),
        bias: Default::default(),
    };
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("camera"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("rusd"),
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });
    let beauty_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("beauty"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[Some(vertex_layout.clone())],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_beauty"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                blend: Some(wgpu::BlendState::REPLACE),
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
    });
    let id_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("prim-id"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[Some(vertex_layout)],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_id"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::R32Uint,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: Some(depth_state),
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("camera"),
        layout: &bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: camera_buf.as_entire_binding(),
        }],
    });

    // --- encode both passes ---
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("rusd"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("beauty"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &beauty_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.55,
                        g: 0.7,
                        b: 0.85,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&beauty_pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.set_index_buffer(index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..indices.len() as u32, 0, 0..1);
    }
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("prim-id"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &id_view,
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
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&id_pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.set_index_buffer(index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..indices.len() as u32, 0, 0..1);
    }

    // --- readback (rows padded to COPY_BYTES_PER_ROW_ALIGNMENT) ---
    let bpr = aligned_bytes_per_row(width);
    let beauty_read = readback_buffer(&device, bpr * height);
    let id_read = readback_buffer(&device, bpr * height);
    copy_to_buffer(&mut encoder, &beauty_tex, &beauty_read, width, height, bpr);
    copy_to_buffer(&mut encoder, &id_tex, &id_read, width, height, bpr);
    queue.submit(Some(encoder.finish()));

    let bslice = beauty_read.slice(..);
    let islice = id_read.slice(..);
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
    beauty_read.unmap();
    id_read.unmap();

    Ok(RenderOutput {
        width,
        height,
        beauty,
        ids,
        paths,
        diags: geo.diags.clone(),
    })
}

// --- helpers --------------------------------------------------------------

fn bake(geo: &SceneGeometry) -> (Vec<Vertex>, Vec<u32>, Vec<String>) {
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    for (i, item) in geo.items.iter().enumerate() {
        let id = (i + 1) as u32;
        let base = vertices.len() as u32;
        let nm = NormalMatrix::new(item.transform);
        for (p, n) in item.mesh.positions.iter().zip(&item.mesh.normals) {
            let wp = geometry::mat_transform_point(
                item.transform,
                [p[0] as f64, p[1] as f64, p[2] as f64],
            );
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
        // upper 3x3 in row-major
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

fn camera_matrix(cam: geometry::Camera, bounds: ([f64; 3], [f64; 3]), w: u32, h: u32) -> Mat {
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

fn aligned_bytes_per_row(width: u32) -> u32 {
    (width * 4 + wgpu::COPY_BYTES_PER_ROW_ALIGNMENT - 1) & !(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT - 1)
}

fn readback_buffer(device: &wgpu::Device, size: u32) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: size as u64,
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
