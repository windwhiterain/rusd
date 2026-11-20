//! **Authoring a USD layer from Rust** — the nesting half of a script (SPEC D15).
//!
//! Every authoring call returns a typed handle: `layer.cube("/World/Volume", 6.0)` is a
//! `Prim<Cube>`, true by construction, and an operator can take it where a `Gprim` is
//! asked without anything being read or checked at runtime. Nesting is the path: a
//! struct of prims is a prim with named children, a `Vec` of prims is a run of them,
//! and the ancestors are authored as `Xform` on the way down.
//!
//! ```
//! use rusd_script::usd::Layer;
//! let mut layer = Layer::new("volume.rusd");
//! let world = layer.xform("/World");
//! let volume = layer.cube(&format!("{}/Volume", world.path()), 6.0);
//! layer.translate(&volume, [0.0, 0.0, 20.0]);
//! let text = layer.to_rusd();
//! assert!(text.contains("[World.Volume]"));
//! assert!(text.contains("schema = \"Cube\""));
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use rusd_core::Value;

use crate::prim::Prim;
use crate::schema::{Camera, Cube, Cylinder, Material, Mesh, Points, Schema, Sphere, Untyped, Xform};

/// One prim's authored opinion: its schema, its properties, and its reference arc.
#[derive(Clone, Debug, Default)]
struct Opinion {
    schema: String,
    props: BTreeMap<String, Value>,
    references: Option<String>,
}

/// A USD layer under construction, written as a `.rusd` document.
#[derive(Clone, Debug)]
pub struct Layer {
    file: String,
    default_prim: String,
    prims: BTreeMap<String, Opinion>,
}

impl Layer {
    pub fn new(file: &str) -> Self {
        Self {
            file: file.to_owned(),
            default_prim: "/World".to_owned(),
            prims: BTreeMap::new(),
        }
    }

    pub fn file(&self) -> &str {
        &self.file
    }

    pub fn default_prim(&mut self, path: &str) -> &mut Self {
        self.default_prim = path.to_owned();
        self
    }

    /// Author a prim of type `T` at `path`, with the ancestors it needs. The handle is
    /// the type the call named — nothing reads a file to decide it.
    fn author<T: Schema>(&mut self, path: &str, props: Vec<(&str, Value)>) -> Prim<T> {
        self.ancestors(path);
        let opinion = self.prims.entry(path.to_owned()).or_default();
        opinion.schema = T::NAME.to_owned();
        for (name, value) in props {
            opinion.props.insert(name.to_owned(), value);
        }
        Prim::in_file(&self.file, path)
    }

    /// Every ancestor of `path` as an `Xform`, so the layer spells the hierarchy out
    /// rather than leaving it implied.
    fn ancestors(&mut self, path: &str) {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        for depth in 1..parts.len() {
            let parent = format!("/{}", parts[..depth].join("/"));
            let opinion = self.prims.entry(parent).or_default();
            if opinion.schema.is_empty() && opinion.references.is_none() {
                opinion.schema = Xform::NAME.to_owned();
            }
        }
    }

    pub fn xform(&mut self, path: &str) -> Prim<Xform> {
        self.author::<Xform>(path, Vec::new())
    }

    pub fn cube(&mut self, path: &str, size: f64) -> Prim<Cube> {
        self.author::<Cube>(path, vec![("size", Value::Float(size))])
    }

    pub fn sphere(&mut self, path: &str, radius: f64) -> Prim<Sphere> {
        self.author::<Sphere>(path, vec![("radius", Value::Float(radius))])
    }

    pub fn cylinder(&mut self, path: &str, radius: f64, height: f64) -> Prim<Cylinder> {
        self.author::<Cylinder>(
            path,
            vec![("radius", Value::Float(radius)), ("height", Value::Float(height))],
        )
    }

    /// A point cloud: `points` as one `point3f[]` value, which is what a sampler reads.
    pub fn points(&mut self, path: &str, points: &[[f64; 3]]) -> Prim<Points> {
        let flat: Vec<Value> = points
            .iter()
            .flat_map(|p| p.iter().map(|v| Value::Float(*v)))
            .collect();
        self.author::<Points>(path, vec![("points", Value::Array(flat))])
    }

    /// A mesh: its vertices, face counts and face indices — the surface a sampler draws
    /// from, authored with the types the mesh reader accepts.
    pub fn mesh(
        &mut self,
        path: &str,
        points: &[[f64; 3]],
        face_vertex_counts: &[i64],
        face_vertex_indices: &[i64],
    ) -> Prim<Mesh> {
        let flat: Vec<Value> = points
            .iter()
            .flat_map(|p| p.iter().map(|v| Value::Float(*v)))
            .collect();
        self.author::<Mesh>(
            path,
            vec![
                ("points", Value::Array(flat)),
                (
                    "faceVertexCounts",
                    Value::Array(face_vertex_counts.iter().map(|i| Value::Int(*i)).collect()),
                ),
                (
                    "faceVertexIndices",
                    Value::Array(face_vertex_indices.iter().map(|i| Value::Int(*i)).collect()),
                ),
            ],
        )
    }

    pub fn camera(&mut self, path: &str, fov: f64, position: [f64; 3], look_at: [f64; 3]) -> Prim<Camera> {
        self.author::<Camera>(
            path,
            vec![
                ("fov", Value::Float(fov)),
                ("position", Value::Vec3(position)),
                ("look_at", Value::Vec3(look_at)),
            ],
        )
    }

    pub fn material(&mut self, path: &str, diffuse: [f64; 3]) -> Prim<Material> {
        self.author::<Material>(path, vec![("diffuse", Value::Vec3(diffuse))])
    }

    /// **An instance**: a prim that authors no type of its own, composing it through a
    /// reference arc to `target`. The handle is [`Untyped`] for exactly that reason.
    pub fn instance<T: Schema>(&mut self, path: &str, target: &Prim<T>) -> Prim<Untyped> {
        self.ancestors(path);
        let opinion = self.prims.entry(path.to_owned()).or_default();
        opinion.schema = String::new();
        opinion.references = Some(target.reference());
        Prim::in_file(&self.file, path)
    }

    /// A reference arc on an existing prim, without changing the type it authors.
    pub fn reference<T: Schema, U: Schema>(&mut self, prim: &Prim<T>, target: &Prim<U>) {
        if let Some(opinion) = self.prims.get_mut(prim.path()) {
            opinion.references = Some(target.reference());
        }
    }

    pub fn translate<T: Schema>(&mut self, prim: &Prim<T>, v: [f64; 3]) {
        self.xform_op(prim.path(), "xformOp:translate", Value::Vec3(v));
    }

    pub fn scale<T: Schema>(&mut self, prim: &Prim<T>, v: [f64; 3]) {
        self.xform_op(prim.path(), "xformOp:scale", Value::Vec3(v));
    }

    pub fn rotate_xyz<T: Schema>(&mut self, prim: &Prim<T>, v: [f64; 3]) {
        self.xform_op(prim.path(), "xformOp:rotateXYZ", Value::Vec3(v));
    }

    /// One transform op, and its place in `xformOpOrder` — the order is authored, never
    /// inferred from which keys happen to be present.
    fn xform_op(&mut self, path: &str, op: &str, value: Value) {
        let opinion = self.prims.entry(path.to_owned()).or_default();
        opinion.props.insert(op.to_owned(), value);
        let order = opinion
            .props
            .entry("xformOpOrder".to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(items) = order {
            let name = Value::String(op.to_owned());
            if !items.contains(&name) {
                items.push(name);
            }
        }
    }

    /// Set any property by name. The typed calls above are the ones a script should
    /// reach for; this is the escape hatch for a schema attribute they do not cover.
    pub fn set<T: Schema>(&mut self, prim: &Prim<T>, name: &str, value: Value) {
        if let Some(opinion) = self.prims.get_mut(prim.path()) {
            opinion.props.insert(name.to_owned(), value);
        }
    }

    /// **The `.rusd` document this layer is**: `[stage]`, then one table per prim in
    /// path order, each with its `schema` and its sorted properties.
    pub fn to_rusd(&self) -> String {
        let mut text = String::from("[stage]\n");
        text.push_str(&format!("default_prim = \"{}\"\n", self.default_prim));
        for (path, opinion) in &self.prims {
            let table = path.trim_start_matches('/').replace('/', ".");
            text.push_str(&format!("\n[{table}]\n"));
            if !opinion.schema.is_empty() {
                text.push_str(&format!("schema = \"{}\"\n", opinion.schema));
            }
            if let Some(arc) = &opinion.references {
                text.push_str(&format!("references = \"{arc}\"\n"));
            }
            for (name, value) in &opinion.props {
                text.push_str(&format!("{} = {value}\n", key(name)));
            }
        }
        text
    }

    /// Write the layer beside the graph that names it.
    pub fn write(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(&self.file), self.to_rusd())
    }
}

/// A TOML key: bare when it is a bare key, quoted otherwise (`xformOp:translate`).
fn key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare { name.to_owned() } else { format!("\"{name}\"") }
}
