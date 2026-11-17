//! The client's camera: orbit / pan / zoom, and the screen→world mapping a pick
//! and a gizmo drag need.
//!
//! `docs/PROTOCOL.md` puts the camera on the client — no message carries one, so
//! moving it re-renders locally and costs no round trip. The projection is
//! `rusd_render`'s (Z-up look-at, `fov_deg` vertical, wgpu clip z in [0, 1]);
//! [`view_proj`] here is that same matrix, and the `gpu` viewport draws with it,
//! so a pixel picked here is a pixel drawn there.
//!
//! The interactive state is an orbit: the frame-fitted [`Camera`] is turned into
//! (target, yaw, pitch, distance) once, and every gesture moves one of those, so
//! the eye can never drift out of frame and a zoom is a distance, not a fov.

use rusd_scene::geometry::{self, Camera, Mat};

/// The orbit camera: the client's own state, distinct from the fitted [`Camera`]
/// it starts from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Orbit {
    /// What the eye looks at, and what orbit and dolly pivot around.
    pub target: [f64; 3],
    /// Rotation about the +Z axis, radians.
    pub yaw: f64,
    /// Elevation above the XY plane, radians, clamped away from the poles.
    pub pitch: f64,
    /// Eye-to-target distance, world units.
    pub distance: f64,
    /// Vertical field of view, degrees — the scene's fitted one, kept.
    pub fov_deg: f64,
}

/// How far past the poles a pitch may go (radians): at exactly ±π/2 the up vector
/// and the view direction are parallel and the basis degenerates.
const PITCH_LIMIT: f64 = std::f64::consts::FRAC_PI_2 - 1e-3;

impl Orbit {
    /// The orbit that reproduces `cam` exactly: same eye, same target, same fov.
    pub fn from_camera(cam: &Camera) -> Self {
        let target = cam.look_at;
        let d = sub(cam.position, target);
        let distance = dot(d, d).sqrt().max(1e-9);
        Self {
            target,
            yaw: d[1].atan2(d[0]),
            pitch: (d[2] / distance).asin().clamp(-PITCH_LIMIT, PITCH_LIMIT),
            distance,
            fov_deg: cam.fov_deg,
        }
    }

    /// The eye at `target + distance · dir(yaw, pitch)`.
    pub fn eye(&self) -> [f64; 3] {
        let (cp, sp) = (self.pitch.cos(), self.pitch.sin());
        let (cy, sy) = (self.yaw.cos(), self.yaw.sin());
        [
            self.target[0] + self.distance * cp * cy,
            self.target[1] + self.distance * cp * sy,
            self.target[2] + self.distance * sp,
        ]
    }

    /// The plain [`Camera`] the renderer takes.
    pub fn camera(&self) -> Camera {
        Camera {
            position: self.eye(),
            look_at: self.target,
            fov_deg: self.fov_deg,
        }
    }

    /// Orbit by a mouse drag: `dx`/`dy` in points, `sensitivity` radians per
    /// point. Dragging right turns the eye around +Z; dragging down raises it
    /// (a trackball's feel: pull the scene down, the eye goes up).
    pub fn orbit(&mut self, dx: f64, dy: f64, sensitivity: f64) {
        self.yaw -= dx * sensitivity;
        self.pitch = (self.pitch + dy * sensitivity).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }

    /// Pan: slide the target (and so the whole view) in the screen plane. `dx`
    /// moves the scene right, `dy` down, by exactly `dx`/`dy` points at the
    /// target's distance: the world units per point is `2·distance·tan(fov/2) /
    /// height`, which is what makes a pan track the cursor at the pivot plane.
    pub fn pan(&mut self, dx: f64, dy: f64, height: f64) {
        let (right, up) = self.basis();
        let per_point =
            2.0 * self.distance * (self.fov_deg.to_radians() / 2.0).tan() / height.max(1.0);
        for k in 0..3 {
            self.target[k] += -dx * per_point * right[k] + dy * per_point * up[k];
        }
    }

    /// Dolly: `amount` in scroll lines, positive = closer. The distance scales
    /// geometrically, so a zoom step feels the same near and far, and it never
    /// crosses the target.
    pub fn zoom(&mut self, amount: f64) {
        let factor = 0.9f64.powf(amount);
        let min = 1e-6f64.max(self.distance * 1e-4);
        let max = self.distance.max(1.0) * 1e6;
        self.distance = (self.distance * factor).clamp(min, max);
    }

    /// Camera right and up in world space (Z-up): the directions a pan moves in.
    pub fn basis(&self) -> ([f64; 3], [f64; 3]) {
        let eye = self.eye();
        let f = norm(sub(self.target, eye));
        let right = norm(cross(f, [0.0, 0.0, 1.0]));
        let up = cross(right, f);
        (right, up)
    }

    /// The world-space ray through a viewport pixel: `(origin, unit direction)`.
    ///
    /// `x`/`y` are pixels with the origin at the top left, `bounds` the scene's
    /// world box (the projection's near/far come from it, exactly as the
    /// renderer's do). The origin is the eye itself — a pick or a gizmo only
    /// cares about where the ray starts, not where the near plane cuts it.
    /// Outside the viewport the ray is still defined: it points where that pixel
    /// would be.
    pub fn ray(
        &self,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        bounds: ([f64; 3], [f64; 3]),
    ) -> ([f64; 3], [f64; 3]) {
        let m = view_proj(&self.camera(), bounds, width as u32, height as u32);
        let inv = mat_inverse(m).unwrap_or(geometry::IDENTITY);
        let ndc_x = 2.0 * x / width.max(1.0) - 1.0;
        let ndc_y = 1.0 - 2.0 * y / height.max(1.0);
        let near = unproject(inv, [ndc_x, ndc_y, 0.0]).unwrap_or_else(|| self.eye());
        let far = unproject(inv, [ndc_x, ndc_y, 1.0]).unwrap_or(self.target);
        (self.eye(), norm(sub(far, near)))
    }
}

/// Where a ray meets the plane through `point` with `normal`; `None` when the
/// ray is parallel to it or the hit is behind the eye.
pub fn plane_hit(ray: ([f64; 3], [f64; 3]), point: [f64; 3], normal: [f64; 3]) -> Option<[f64; 3]> {
    let (o, d) = ray;
    let denom = dot(d, normal);
    if denom.abs() < 1e-9 {
        return None;
    }
    let t = dot(sub(point, o), normal) / denom;
    if t < 0.0 {
        return None;
    }
    Some([o[0] + t * d[0], o[1] + t * d[1], o[2] + t * d[2]])
}

/// The projection `rusd_render` uses: perspective (wgpu clip z [0,1]) times a
/// Z-up look-at, with near/far derived from the eye-to-scene distance and the
/// world box.
pub fn view_proj(cam: &Camera, bounds: ([f64; 3], [f64; 3]), w: u32, h: u32) -> Mat {
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
    let aspect = f64::from(w) / f64::from(h.max(1));
    let view = look_at(cam.position, cam.look_at, [0.0, 0.0, 1.0]);
    let proj = perspective(cam.fov_deg, aspect, near, far);
    geometry::mat_mul(proj, view)
}

/// A world point under the projection, as a pixel: `None` when it is behind the
/// eye (clip w ≤ 0).
pub fn project(m: Mat, p: [f64; 3], w: u32, h: u32) -> Option<(f64, f64)> {
    let clip = [
        m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
        m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
        m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
        m[3] * p[0] + m[7] * p[1] + m[11] * p[2] + m[15],
    ];
    if clip[3] <= 0.0 {
        return None;
    }
    let ndc = [clip[0] / clip[3], clip[1] / clip[3]];
    Some((
        (ndc[0] + 1.0) * 0.5 * f64::from(w),
        (1.0 - ndc[1]) * 0.5 * f64::from(h),
    ))
}

/// The inverse of a column-major 4×4, `None` when it is singular.
pub fn mat_inverse(m: Mat) -> Option<Mat> {
    // Gauss-Jordan on the augmented matrix, in rows: a[4][8] with the identity
    // on the right, then read the right half back as columns.
    let mut a = [[0f64; 8]; 4];
    for row in 0..4 {
        for col in 0..4 {
            a[row][col] = m[col * 4 + row];
        }
        a[row][4 + row] = 1.0;
    }
    for col in 0..4 {
        let mut pivot = col;
        for row in col + 1..4 {
            if a[row][col].abs() > a[pivot][col].abs() {
                pivot = row;
            }
        }
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        let d = a[col][col];
        for v in a[col].iter_mut() {
            *v /= d;
        }
        for row in 0..4 {
            if row == col {
                continue;
            }
            let f = a[row][col];
            if f == 0.0 {
                continue;
            }
            let pivot_row = a[col];
            for (k, v) in a[row].iter_mut().enumerate() {
                *v -= f * pivot_row[k];
            }
        }
    }
    let mut out = [0f64; 16];
    for row in 0..4 {
        for col in 0..4 {
            out[col * 4 + row] = a[row][4 + col];
        }
    }
    Some(out)
}

fn unproject(inv: Mat, ndc: [f64; 3]) -> Option<[f64; 3]> {
    let p = [
        inv[0] * ndc[0] + inv[4] * ndc[1] + inv[8] * ndc[2] + inv[12],
        inv[1] * ndc[0] + inv[5] * ndc[1] + inv[9] * ndc[2] + inv[13],
        inv[2] * ndc[0] + inv[6] * ndc[1] + inv[10] * ndc[2] + inv[14],
        inv[3] * ndc[0] + inv[7] * ndc[1] + inv[11] * ndc[2] + inv[15],
    ];
    if p[3].abs() < 1e-12 {
        return None;
    }
    Some([p[0] / p[3], p[1] / p[3], p[2] / p[3]])
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
    use super::*;
    use crate::scene;

    fn fitted() -> (Orbit, ([f64; 3], [f64; 3])) {
        let bbox = scene::BBox {
            min: [-100.0, -100.0, 0.0],
            max: [100.0, 100.0, 40.0],
        };
        let cam = scene::fit_camera(&bbox);
        (Orbit::from_camera(&cam), bbox.corners())
    }

    #[test]
    fn an_orbit_built_from_the_fitted_camera_reproduces_it() {
        let (orbit, _) = fitted();
        let bbox = scene::BBox {
            min: [-100.0, -100.0, 0.0],
            max: [100.0, 100.0, 40.0],
        };
        let want = scene::fit_camera(&bbox);
        let got = orbit.camera();
        for k in 0..3 {
            assert!(
                (want.position[k] - got.position[k]).abs() < 1e-9,
                "eye {k}: {} vs {}",
                want.position[k],
                got.position[k]
            );
            assert!((want.look_at[k] - got.look_at[k]).abs() < 1e-9);
        }
        assert!((want.fov_deg - got.fov_deg).abs() < 1e-12);
    }

    #[test]
    fn orbiting_keeps_the_distance_and_the_target() {
        let (mut o, _) = fitted();
        let before = o.distance;
        o.orbit(120.0, -40.0, 0.005);
        assert!((o.distance - before).abs() < 1e-12, "orbit is a rotation");
        let eye = o.eye();
        let d = dist(eye, o.target);
        assert!((d - before).abs() < 1e-9, "eye stays on the sphere");
    }

    #[test]
    fn pitch_stops_short_of_the_poles() {
        let (mut o, _) = fitted();
        o.orbit(0.0, 1e6, 0.005);
        assert!(o.pitch <= PITCH_LIMIT);
        let (right, up) = o.basis();
        assert!(dot(right, up).abs() < 1e-9, "basis stays orthogonal");
        assert!(norm(up)[2].abs() < 1.0, "up is not degenerate");
    }

    #[test]
    fn panning_moves_the_target_in_the_screen_plane() {
        let (mut o, _) = fitted();
        let t0 = o.target;
        let (right, up) = o.basis();
        // Dragging right pulls the scene right: the camera (and the target) goes
        // left, along -right.
        o.pan(10.0, 0.0, 600.0);
        let d = sub(o.target, t0);
        assert!(
            dot(norm(d), right) < -0.999,
            "10 points right moves the scene right"
        );
        assert!(dot(d, up).abs() < 1e-9, "no vertical drift");

        // Dragging down pulls the scene down: the camera goes up, along +up.
        let t1 = o.target;
        o.pan(0.0, 10.0, 600.0);
        let d = sub(o.target, t1);
        assert!(
            dot(norm(d), up) > 0.999,
            "10 points down moves the scene down"
        );
        assert!(dot(d, right).abs() < 1e-9, "no horizontal drift");
    }

    #[test]
    fn pan_tracks_the_cursor_at_the_pivot_plane() {
        // A world point at the target's depth must land exactly where the cursor
        // moved: the whole point of the per-point scale.
        let (mut o, bounds) = fitted();
        let h = 400.0;
        let pw = o.target; // a fixed world point, not the (moving) target
        let m0 = view_proj(&o.camera(), bounds, 600, 400);
        let p0 = project(m0, pw, 600, 400).expect("projects");
        o.pan(30.0, -20.0, h);
        let m1 = view_proj(&o.camera(), bounds, 600, 400);
        let p1 = project(m1, pw, 600, 400).expect("projects");
        assert!(
            (p1.0 - p0.0 - 30.0).abs() < 1e-6,
            "moved with the cursor: {}",
            p1.0 - p0.0
        );
        assert!(
            (p1.1 - p0.1 + 20.0).abs() < 1e-6,
            "moved with the cursor: {}",
            p1.1 - p0.1
        );
    }

    #[test]
    fn zoom_is_geometric_both_ways_and_never_crosses_the_target() {
        let (mut o, _) = fitted();
        let d0 = o.distance;
        o.zoom(1.0);
        assert!(o.distance < d0, "one wheel line in is closer");
        o.zoom(-1.0);
        assert!((o.distance - d0).abs() < 1e-9, "and one line out is back");
        for _ in 0..100_000 {
            o.zoom(1.0);
        }
        assert!(o.distance > 0.0, "distance stays positive: {}", o.distance);
    }

    #[test]
    fn the_ray_through_the_viewport_center_points_at_the_target() {
        let (o, bounds) = fitted();
        let (w, h) = (800.0, 600.0);
        let (origin, dir) = o.ray(w / 2.0, h / 2.0, w, h, bounds);
        assert!(dist(origin, o.eye()) < 1e-6, "ray starts at the eye");
        let want = norm(sub(o.target, o.eye()));
        for k in 0..3 {
            assert!(
                (dir[k] - want[k]).abs() < 1e-9,
                "dir {k}: {} vs {}",
                dir[k],
                want[k]
            );
        }
    }

    #[test]
    fn project_and_ray_agree_so_a_pick_lands_where_the_pointer_is() {
        let (o, bounds) = fitted();
        let (w, h) = (960u32, 540u32);
        let m = view_proj(&o.camera(), bounds, w, h);
        let target_world = [10.0, -25.0, 15.0];
        let (px, py) = project(m, target_world, w, h).expect("projects inside the frame");
        let ray = o.ray(px, py, f64::from(w), f64::from(h), bounds);
        // The ray passes through the point it was projected from: the miss is the
        // component of eye→point perpendicular to the direction.
        let to_point = sub(target_world, ray.0);
        let along = dot(to_point, ray.1);
        let len2 = dot(to_point, to_point);
        let miss = (len2 - along * along).max(0.0).sqrt();
        // The projection is inverted through a 16-float matrix, so the residual is
        // float-rounding, not geometry: a thousandth of a scene unit is far below
        // one pixel at this distance (where a pixel is ~0.6 units).
        assert!(miss < 1e-3, "ray misses the projected point by {miss}");
    }

    #[test]
    fn a_point_behind_the_eye_does_not_project() {
        let (o, bounds) = fitted();
        let m = view_proj(&o.camera(), bounds, 800, 600);
        let eye = o.eye();
        let behind = [
            eye[0] + (eye[0] - o.target[0]),
            eye[1] + (eye[1] - o.target[1]),
            eye[2] + (eye[2] - o.target[2]),
        ];
        assert!(project(m, behind, 800, 600).is_none());
    }

    #[test]
    fn a_drag_on_the_ground_plane_moves_the_prim_with_the_cursor() {
        let (o, bounds) = fitted();
        let (w, h) = (800u32, 600u32);
        let prim_center = [0.0, 0.0, 10.0];
        let m = view_proj(&o.camera(), bounds, w, h);
        let (px, py) = project(m, prim_center, w, h).expect("projects");
        let hit0 = plane_hit(
            o.ray(px, py, f64::from(w), f64::from(h), bounds),
            prim_center,
            [0.0, 0.0, 1.0],
        )
        .expect("the ray meets the ground plane through the prim");
        assert!(
            dist(hit0, prim_center) < 1e-6,
            "the cursor's pixel is the prim"
        );

        let hit1 = plane_hit(
            o.ray(px + 40.0, py + 25.0, f64::from(w), f64::from(h), bounds),
            prim_center,
            [0.0, 0.0, 1.0],
        )
        .expect("a nearby pixel still meets the plane");
        let d = sub(hit1, hit0);
        assert!(
            d[0].abs() + d[1].abs() > 1e-3,
            "the drag moved on the plane"
        );
        assert!(d[2].abs() < 1e-9, "no motion along the plane normal");
    }

    #[test]
    fn a_ray_parallel_to_the_plane_has_no_hit() {
        let ray = ([0.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
        assert!(plane_hit(ray, [0.0, 0.0, 5.0], [0.0, 0.0, 1.0]).is_none());
    }

    #[test]
    fn mat_inverse_undoes_a_projection() {
        let (o, bounds) = fitted();
        let m = view_proj(&o.camera(), bounds, 800, 600);
        let inv = mat_inverse(m).expect("invertible");
        let i = geometry::mat_mul(m, inv);
        for (k, v) in i.iter().enumerate() {
            let want = if k % 5 == 0 { 1.0 } else { 0.0 };
            assert!((v - want).abs() < 1e-9, "identity at {k}: {v}");
        }
    }
}
