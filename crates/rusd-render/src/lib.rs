//! # rusd-render
//!
//! The agent's eyes and hands (SPEC §7). M4 in progress:
//!
//! - geometry extraction lives in [`rusd_scene::geometry`] (Stage → meshes +
//!   world transforms + camera); this crate bakes/uploads it to the GPU.
//! - `device`: wgpu setup (DX12 here; Vulkan/Metal elsewhere), headless-first.
//!   Verified by `examples/wgpu_spike.rs` (RTX 3060 Ti, offscreen + readback).
//! - `passes`: beauty, prim-id (flat stable id per prim ⇒ segmentation/picking),
//!   depth, normal, wireframe.
//! - `scene_gpu`: content-hash mesh cache, GPU instancing for generator output.
//! - `capture`: deterministic offscreen render → image + metadata JSON.
//! - `viewport`: winit + egui interactive mode with hot reload.
//!
//! Error-tolerant by contract: invalid prims become magenta placeholders plus
//! diagnostics; a render call always returns, never panics the agent loop.

pub use rusd_core as core;
pub use rusd_proc as proc;
pub use rusd_scene as scene;

#[cfg(test)]
mod tests {
    use rusd_scene::geometry;

    const FOREST: &str = include_str!("../../../examples/forest.rusd");
    const CITY: &str = include_str!("../../../examples/city.rusd");

    fn extract(src: &str) -> geometry::SceneGeometry {
        let doc = rusd_core::syntax::parse(src).unwrap();
        let out = rusd_proc::expand_doc(&doc).unwrap();
        geometry::extract(&out.stage)
    }

    // Item counts track the current generator output + seeds (acceptance, M4).
    #[test]
    fn forest_extracts_spheres_and_camera() {
        let g = extract(FOREST);
        assert_eq!(g.items.len(), 501); // 500 scatter + 1 prototype
        assert!(g.items.iter().all(|it| it.mesh.positions.len() == 425));
        let cam = g.camera.expect("forest has a Camera prim");
        assert_eq!(cam.position, [60.0, -60.0, 40.0]);
        assert_eq!(cam.look_at, [0.0, 0.0, 0.0]);
        assert_eq!(cam.fov_deg, 45.0);
        assert!(g.diags.is_empty());
    }

    #[test]
    fn city_extracts_buildings_and_props() {
        let g = extract(CITY);
        assert_eq!(g.items.len(), 434);
        let cubes = g
            .items
            .iter()
            .filter(|it| it.mesh.positions.len() == 24)
            .count();
        let spheres = g
            .items
            .iter()
            .filter(|it| it.mesh.positions.len() == 425)
            .count();
        assert_eq!(cubes, 133);
        assert_eq!(spheres, 301);
        assert!(g.camera.is_some());
        assert!(g.diags.is_empty());
    }

    #[test]
    fn extraction_is_deterministic() {
        let a = extract(CITY);
        let b = extract(CITY);
        assert_eq!(a.items.len(), b.items.len());
        for (x, y) in a.items.iter().zip(&b.items) {
            assert_eq!(x.path, y.path);
            assert_eq!(x.transform, y.transform);
            assert_eq!(x.color, y.color);
        }
    }

    #[test]
    fn city_building_sits_on_ground() {
        let g = extract(CITY);
        let b = g
            .items
            .iter()
            .find(|it| it.path == "/World/City/b0_0/lot0_0")
            .expect("building exists");
        // M = T * S (xformOpOrder [translate, scale]): translate z == scale z ==
        // half-height, so the box's bottom face lands at z = 0.
        let tz = b.transform[14];
        let sz = b.transform[10];
        assert!(tz > 0.0);
        assert!((tz - sz).abs() < 1e-3, "base at z=0: tz={tz} sz={sz}");
    }
}
