//! The **scene tree** panel: the prims the scene on screen actually has, as a hierarchy.
//!
//! `docs/PROTOCOL.md` v1.3 ruled prim rows **out of the composition tree** — that panel is
//! about structure (layers, arcs, variant sets), and a stacked city's hundreds of instance
//! prims would bury it. The user's correction keeps that ruling and adds the thing it
//! implied but never had: a **second** panel, beside it, that shows the prims as a tree and
//! nothing else.
//!
//! - **Where the paths come from is the wire.** `composition_tree`'s `prim_nodes` is still
//!   sent (`docs/PROTOCOL.md` v1.5.7: it is carried «供 pick 解析，只不画»); this module
//!   builds the hierarchy out of those `path`s and draws it. No new request, no second
//!   source: the model is the report the composition panel is already holding.
//! - **A node is a path, not a spec.** Two prims are one node when their paths are equal,
//!   and a node's children are the paths one component longer. So `/World/Clusters/b000003`
//!   appears exactly once even though four layers may each author an opinion on it, which
//!   is what makes the tree a picture of the *scene* rather than of the stack.
//! - **A click selects the prim**, exactly as a viewport pick does: the shell's `select_path`
//!   (`crate::app::Shell::select_path`), which is also what the control channel's
//!   `select{path}` calls. Nothing here selects, tunes or toggles anything itself — it draws
//!   rows and reports which one was clicked, like `crate::panel`.
//!
//! **Determinism.** Children are sorted by path, and the roots too; a `BTreeMap` holds the
//! hierarchy while it is being built. Nothing depends on a hash order, so the same report
//! always draws the same tree.

use std::collections::BTreeMap;

use egui::{Align, Color32, Layout, RichText, ScrollArea};

/// One prim in the scene tree: its path, its name, and its children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneNode {
    /// The full prim path (`/World/Clusters/b000003`) — the id a selection takes.
    pub path: String,
    /// The last path component, which is what the row shows.
    pub name: String,
    /// How many prims this one's subtree holds, itself included. Reported so a row can say
    /// `112` beside a collapsed branch instead of making the reader expand it to find out.
    pub subtree: usize,
    pub children: Vec<SceneNode>,
}

/// Build the scene tree from the paths a report lists.
///
/// Paths are **not** assumed to be complete: a report may list `/World/A/B` without
/// `/World/A` (a prim a cook flattened is authored nowhere, so no spec names it). Such a
/// middle prim is built as a node of its own, because a tree with a missing middle would
/// drop the branch instead of showing where the leaf hangs. Its `subtree` counts it, and it
/// is a path a click may select — the same thing the viewport would pick if the pixel were
/// there.
///
/// Sorted by path at every level, so the order is the same on every run.
pub fn build(paths: &[String]) -> Vec<SceneNode> {
    let mut roots: BTreeMap<String, Branch> = BTreeMap::new();
    for path in paths {
        let parts = components(path);
        if parts.is_empty() {
            continue;
        }
        let mut level = &mut roots;
        let mut so_far = String::new();
        for name in &parts {
            so_far.push('/');
            so_far.push_str(name);
            level = &mut level
                .entry(name.clone())
                .or_insert_with(|| Branch {
                    path: so_far.clone(),
                    children: BTreeMap::new(),
                })
                .children;
        }
    }
    roots.into_values().map(Branch::into_node).collect()
}

/// A path's components, with the leading `/` and any empty or `.` run dropped.
///
/// `[]` for `/`, `["World", "A"]` for `/World/A`, `["World", "A"]` for `/World/A/`, and
/// `["World", "A"]` for the same path with a doubled slash — a malformed path is still a
/// row rather than an exception, because the tree shows what the report said.
pub fn components(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_owned)
        .collect()
}

/// A half-built node: the path it stands for and its children, until it is a
/// [`SceneNode`].
struct Branch {
    path: String,
    children: BTreeMap<String, Branch>,
}

impl Branch {
    fn into_node(self) -> SceneNode {
        let children: Vec<SceneNode> = self.children.into_values().map(Branch::into_node).collect();
        let subtree: usize = 1 + children.iter().map(|c| c.subtree).sum::<usize>();
        let name = components(&self.path)
            .pop()
            .unwrap_or_else(|| self.path.clone());
        SceneNode {
            path: self.path,
            name,
            subtree,
            children,
        }
    }
}

/// Every node of a tree, depth first — the shape a test asserts against without walking the
/// hierarchy by hand, and what a caller counting rows asks.
pub fn flatten(roots: &[SceneNode]) -> Vec<(usize, SceneNode)> {
    fn walk(node: &SceneNode, depth: usize, out: &mut Vec<(usize, SceneNode)>) {
        out.push((depth, node.clone()));
        for child in &node.children {
            walk(child, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for root in roots {
        walk(root, 0, &mut out);
    }
    out
}

/// What a click in the scene tree asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SceneAction {
    /// Nothing this pass.
    None,
    /// Select the prim at this path — the same selection a viewport pick makes.
    Select(String),
}

/// The scene tree's panel state: whether it is shown, and the one filter it has.
///
/// Expansion is not here: a row's open/closed state is a widget's own data in eguis's
/// per-frame store (`Row::open`), keyed by the path, so a rebuild of the model does not
/// collapse what the user opened.
#[derive(Clone, Debug, Default)]
pub struct ScenePanel {
    /// Whether the panel is shown at all. **Closed by default**: the viewport is what the
    /// window is for, and the composition panel already takes the left side.
    pub open: bool,
}

impl ScenePanel {
    pub fn new() -> Self {
        Self::default()
    }
}

/// How many rows are drawn at most: a scene of 240 instances is fine to walk, a flattened
/// cook of 40 000 prims is not, and a panel that tries freezes the window.
///
/// The limit is **reported** when it is hit (`… 还有 N 个未画出`) rather than silently
/// truncating, because a tree that quietly stops is worse than one that says where.
pub const MAX_ROWS: usize = 2000;

/// Draw the scene tree: a header with the node count, then the hierarchy.
///
/// `selected` is the path the viewport has selected, so the row can be marked; `roots` is
/// [`build`]'s answer. Returns what the click asked for, or [`SceneAction::None`] — the
/// shell carries it out, which is the same split `crate::panel` keeps.
pub fn scene_tree_panel(
    ui: &mut egui::Ui,
    panel: &mut ScenePanel,
    roots: &[SceneNode],
    selected: Option<&str>,
) -> SceneAction {
    let mut action = SceneAction::None;
    ui.horizontal(|ui| {
        if ui.small_button("⟨ 收起场景树").clicked() {
            panel.open = false;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(format!("{} 个 prim", flatten(roots).len()))
                    .small()
                    .weak(),
            );
        });
    });
    if roots.is_empty() {
        ui.add_space(4.0);
        ui.label(
            RichText::new("（场景树为空：合成树还没有到达，或它没有列出任何 prim）")
                .small()
                .weak(),
        );
        return action;
    }
    let mut drawn = 0usize;
    ScrollArea::vertical()
        .id_salt("scene-tree")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for root in roots {
                if let SceneAction::None = &action
                    && let Some(a) = node_row(ui, root, 0, selected, &mut drawn)
                {
                    action = a;
                }
            }
        });
    let total = flatten(roots).len();
    if total > drawn {
        ui.label(
            RichText::new(format!("… 还有 {} 个未画出", total - drawn))
                .small()
                .weak(),
        );
    }
    action
}

/// One row and its children: a chevron expands, the label selects.
///
/// Two hit areas and two jobs, the same arrangement the composition panel keeps (and for
/// the same reason: a `CollapsingHeader`'s whole header is one target, so expanding a
/// branch would also be reported as a click on its prim).
fn node_row(
    ui: &mut egui::Ui,
    node: &SceneNode,
    depth: usize,
    selected: Option<&str>,
    drawn: &mut usize,
) -> Option<SceneAction> {
    if *drawn >= MAX_ROWS {
        return None;
    }
    *drawn += 1;
    let is_selected = selected == Some(node.path.as_str());
    let leaf = node.children.is_empty();
    let id = ui.make_persistent_id(("scene-tree-row", &node.path));
    let mut open = ui.data(|d| d.get_temp::<bool>(id.with("open")).unwrap_or(false));
    let mut clicked = None;
    ui.horizontal(|ui| {
        if depth > 0 {
            ui.add_space(8.0 * depth as f32);
        }
        if leaf {
            // A leaf gets no chevron, and no gap either: the indent already says where it
            // hangs, and an empty button would read as a control that does nothing.
        } else if ui
            .small_button(if open { "▾" } else { "▸" })
            .on_hover_text(if open { "收起" } else { "展开" })
            .clicked()
        {
            open = !open;
            ui.data_mut(|d| d.insert_temp(id.with("open"), open));
        }
        let text = if is_selected {
            RichText::new(&node.name).color(Color32::from_rgb(120, 200, 255))
        } else {
            RichText::new(&node.name)
        };
        let response = ui
            .selectable_label(is_selected, text)
            .on_hover_text(&node.path);
        if response.clicked() {
            clicked = Some(SceneAction::Select(node.path.clone()));
        }
        if !leaf && !open {
            // What is inside, without opening it: the count says whether a branch is worth
            // a click, which is the question a collapsed row raises.
            ui.label(
                RichText::new(format!("({})", node.subtree - 1))
                    .small()
                    .weak()
                    .color(Color32::from_rgb(160, 170, 180)),
            );
        }
    });
    if clicked.is_some() {
        return clicked;
    }
    if open {
        for child in &node.children {
            if let Some(a) = node_row(ui, child, depth + 1, selected, drawn) {
                return Some(a);
            }
        }
    }
    None
}
