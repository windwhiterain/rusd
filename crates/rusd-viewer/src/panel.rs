//! The two panels D13 adds to the window: the **composition tree** (`docs/PROTOCOL.md`
//! v1.2) and the **capture toolbar** (v1.1).
//!
//! Everything here draws and reports; it decides nothing. A click comes back as an
//! [`TreeAction`] or a [`CaptureAction`], and the shell (`crate::app`) carries it
//! out — asks the server, moves the viewport's selection, puts an id on the
//! clipboard. That split is what keeps the rules testable without a screen: the
//! tree's shape lives in [`crate::tree`], the capture's pixels and payload in
//! [`crate::capture`], and this module is only the widgets between them.
//!
//! ## What the tree panel shows
//!
//! - **roots are the layer stack, strongest first.** The session layer (rank 0) is
//!   the top row; the root layer and its sublayers follow, then whatever an arc
//!   opened.
//! - **an arc is an edge**, drawn under the layer that authors it and pointing at
//!   `target_layer:target_prim`; opening it walks into the target layer's branch.
//! - **a prim hangs under every layer that authors it**, so "who says what" is
//!   answered by which branch a row is under.
//! - **a variant set expands** into its variants, the selected one first shown in
//!   the row (`size = small`).
//! - **clicking any row** selects it and asks `comp_node{id}` for its detail. A row
//!   that stands for a prim also selects that prim in the viewport and puts its
//!   path on the clipboard (`docs/PROTOCOL.md` v1.2: 「点选 prim 节点仍自动拷贝
//!   prim path」).
//! - **a server without the method** leaves the panel on one sentence — 「合成树不
//!   可用（server 版本）」 — and nothing crashes.

use egui::{Align, Color32, Layout, Rect, RichText, Stroke, StrokeKind};

use crate::capture::{SelectionRect, rect_from_drag};
use crate::tree::{
    NodeDetail, TreeModel, TreeNode, TreeNodeKind, TreeSelection, clipboard_for, flatten,
};

/// What a click in the tree asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeAction {
    /// Nothing this pass.
    None,
    /// A row was clicked: select it, ask its detail, and (when it stands for a
    /// prim) also select that prim and copy its path.
    Select(TreeSelection),
    /// The 「刷新」 button: ask `composition_tree{}` again.
    Refresh,
}

/// The tree panel's state, as drawn.
#[derive(Default)]
pub struct TreePanel {
    /// The tree itself, and whether it is unavailable.
    pub model: TreeModel,
    /// The clicked row, kept so the panel can highlight it across a redraw.
    pub selection: Option<TreeSelection>,
    /// The detail of the last click (`comp_node{id}`), or why it failed.
    pub detail: Option<NodeDetail>,
    pub detail_error: Option<String>,
}

impl TreePanel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a `composition_tree` answer.
    pub fn apply_tree(&mut self, reply: Result<serde_json::Value, (i64, String)>) {
        // A degraded or failed answer also drops the detail: it describes rows of a
        // tree that no longer exists.
        if self.model.apply(reply).is_err() {
            self.detail = None;
            self.detail_error = None;
            self.selection = None;
        }
    }

    /// Take a `comp_node{id}` answer.
    pub fn apply_node(&mut self, reply: Result<serde_json::Value, (i64, String)>) {
        match reply {
            Ok(value) => match NodeDetail::parse(&value) {
                Ok(d) => {
                    self.detail = Some(d);
                    self.detail_error = None;
                }
                Err(e) => {
                    self.detail = None;
                    self.detail_error = Some(e);
                }
            },
            Err((crate::protocol::TIMEOUT_CODE, _)) => {
                self.detail = None;
                self.detail_error = Some("合成树：服务器未在时限内应答".to_owned());
            }
            Err((crate::tree::METHOD_NOT_FOUND, _)) => {
                self.detail = None;
                self.detail_error = Some(crate::tree::unavailable_text());
            }
            Err((code, message)) => {
                self.detail = None;
                self.detail_error = Some(format!("comp_node 失败：{message}（code {code}）"));
            }
        }
    }
}

/// Draw the tree panel's whole body: its status line, then either the rows or the
/// degraded sentence.
pub fn tree_panel(ui: &mut egui::Ui, panel: &TreePanel) -> TreeAction {
    let mut refresh = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(panel.model.status_zh()).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("刷新").clicked() {
                refresh = true;
            }
        });
    });
    if refresh {
        return TreeAction::Refresh;
    }
    if panel.model.is_unavailable() {
        ui.add_space(4.0);
        ui.colored_label(
            Color32::from_rgb(255, 170, 60),
            crate::tree::unavailable_text(),
        );
        return TreeAction::None;
    }
    let roots = panel.model.roots();
    if roots.is_empty() {
        ui.label("（尚无合成树）");
        return TreeAction::None;
    }
    let mut action = TreeAction::None;
    egui::ScrollArea::vertical()
        .id_salt("composition-tree")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for root in &roots {
                if let Some(sel) = row(ui, root, 0, panel.selection.as_ref()) {
                    action = TreeAction::Select(sel);
                }
            }
        });
    action
}

/// One row, and its subtree. Returns the selection a click asked for.
fn row(
    ui: &mut egui::Ui,
    node: &TreeNode,
    depth: usize,
    selected: Option<&TreeSelection>,
) -> Option<TreeSelection> {
    let is_selected = selected.is_some_and(|s| s.id == node.id);
    let text = match node.kind {
        TreeNodeKind::Layer => RichText::new(&node.label).strong(),
        TreeNodeKind::Arc => RichText::new(&node.label).italics(),
        TreeNodeKind::Prim | TreeNodeKind::VariantSet => RichText::new(&node.label),
    };
    let text = if is_selected {
        text.color(Color32::from_rgb(120, 200, 255))
    } else {
        text
    };

    let mut clicked: Option<TreeSelection> = None;
    if node.children.is_empty() {
        ui.horizontal(|ui| {
            indent(ui, depth);
            if ui.selectable_label(is_selected, text).clicked() {
                clicked = Some(TreeSelection::of(node));
            }
        });
    } else {
        // `default_open` only at the top: 434 prim rows under a layer must not all
        // be laid out before the user has opened anything.
        let header = egui::CollapsingHeader::new(text)
            .id_salt(&node.id)
            .default_open(depth == 0)
            .show(ui, |ui| {
                for child in &node.children {
                    if let Some(sel) = row(ui, child, depth + 1, selected) {
                        clicked = Some(sel);
                    }
                }
            });
        if header.header_response.clicked() {
            clicked = Some(TreeSelection::of(node));
        }
    }
    clicked
}

fn indent(ui: &mut egui::Ui, depth: usize) {
    if depth > 0 {
        ui.add_space(8.0 * depth as f32);
    }
}

/// Draw the detail of the selected node (`comp_node{id}`), or why there is none.
pub fn detail_panel(ui: &mut egui::Ui, panel: &TreePanel) {
    ui.label(RichText::new("详情").strong());
    if let Some(err) = &panel.detail_error {
        ui.colored_label(Color32::from_rgb(255, 170, 60), err);
        return;
    }
    let Some(detail) = &panel.detail else {
        ui.label("（点选一个节点）");
        return;
    };
    egui::ScrollArea::vertical()
        .id_salt("comp-node-detail")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for line in detail.lines() {
                ui.label(RichText::new(line).monospace());
            }
        });
}

/// What the capture toolbar asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureAction {
    None,
    /// The 「选区截图」 button: enter (or leave) selection mode.
    ToggleArmed,
    /// The user started a drag over the viewport.
    DragStarted(egui::Pos2),
    /// The drag ended: take the capture of this region.
    DragFinished(egui::Pos2),
    /// The note field changed (the shell keeps the text; this is only a signal that
    /// the panel drew it).
    NoteEdited,
}

/// The capture toolbar: the arm button, a hint, and the optional note.
pub fn capture_toolbar(ui: &mut egui::Ui, armed: bool, note: &mut String) -> CaptureAction {
    let mut action = CaptureAction::None;
    let label = if armed {
        "取消选区"
    } else {
        "选区截图"
    };
    let button = egui::Button::new(label).fill(if armed {
        Color32::from_rgb(60, 110, 60)
    } else {
        Color32::TRANSPARENT
    });
    if ui
        .add(button)
        .on_hover_text("拖一个矩形，生成 mask 与 capture")
        .clicked()
    {
        action = CaptureAction::ToggleArmed;
    }
    ui.separator();
    ui.label("note：");
    let response = ui.add(
        egui::TextEdit::singleline(note)
            .desired_width(220.0)
            .hint_text("可选"),
    );
    if response.changed() {
        action = CaptureAction::NoteEdited;
    }
    if armed {
        ui.separator();
        ui.colored_label(Color32::from_rgb(120, 200, 255), "在视口里拖一个矩形");
    }
    action
}

/// The mask rectangle a drag over the viewport covers, for the shell to turn into
/// pixels — the same rule a pick uses, so what the user outlines is what the mask
/// marks.
pub fn mask_rect(
    viewport: Rect,
    start: egui::Pos2,
    end: egui::Pos2,
    frame: (u32, u32),
) -> Option<SelectionRect> {
    rect_from_drag(viewport, start, end, frame)
}

/// Draw the selection the user is dragging, so the region is visible before the
/// capture is taken.
pub fn paint_capture_rect(ui: &egui::Ui, start: egui::Pos2, end: egui::Pos2) {
    let rect = Rect::from_two_pos(start, end);
    ui.painter().rect_filled(
        rect,
        0.0,
        Color32::from_rgba_unmultiplied(120, 200, 255, 40),
    );
    ui.painter().rect_stroke(
        rect,
        0.0,
        Stroke::new(1.5, Color32::from_rgb(120, 200, 255)),
        StrokeKind::Inside,
    );
}

/// The toast for a copy: the line, in the top bar, until it expires.
///
/// `text` is what [`crate::capture::copied_toast_zh`] or the capture's own message
/// says; showing it is the caller's job (this only decides whether it is still
/// worth drawing), so a headless test can assert the text without a window.
pub fn toast_visible(now: std::time::Instant, shown_at: std::time::Instant) -> bool {
    now.duration_since(shown_at) < TOAST_FOR
}

/// How long a toast stays up.
pub const TOAST_FOR: std::time::Duration = std::time::Duration::from_secs(4);

/// Every row of the panel's tree, flattened — the shape a test asserts against.
pub fn rows(panel: &TreePanel) -> Vec<(usize, TreeNode)> {
    flatten(&panel.roots())
}

impl TreePanel {
    /// The rows, from the model.
    pub fn roots(&self) -> Vec<TreeNode> {
        self.model.roots()
    }
}

/// The clipboard payload for a clicked row: its prim path, when it has one.
pub fn copy_payload(sel: &TreeSelection) -> Option<String> {
    clipboard_for(sel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_tree() -> serde_json::Value {
        json!({
            "layers": [
                {"id": "anon:session", "kind": "session", "strength": 0},
                {"id": "city.usda", "kind": "root", "strength": 1},
            ],
            "arcs": [
                {"id": "arc:0", "kind": "reference", "source_layer": "city.usda",
                 "source_prim": "/World/Block", "target_layer": "protos.usda",
                 "target_prim": "/Prototypes/Block"},
            ],
            "prim_nodes": [
                {"id": "prim:/World/Block", "path": "/World/Block",
                 "authored_in": ["city.usda", "anon:session"]},
            ],
            "variant_sets": [],
        })
    }

    #[test]
    fn a_tree_answer_fills_the_panel_and_a_degraded_one_empties_it() {
        let mut panel = TreePanel::new();
        panel.apply_tree(Ok(sample_tree()));
        assert!(!panel.model.is_unavailable());
        assert_eq!(panel.roots().len(), 2, "one root per layer");

        // A selected row and its detail are dropped when the tree degrades: they
        // describe rows that no longer exist.
        panel.selection = Some(TreeSelection {
            id: "prim:/World/Block".to_owned(),
            kind: TreeNodeKind::Prim,
            prim: Some("/World/Block".to_owned()),
        });
        panel.apply_node(Ok(json!({
            "kind_type": "prim", "id": "prim:/World/Block", "path": "/World/Block",
            "opinions": [], "arcs_out": [], "variant_sets": []
        })));
        assert!(panel.detail.is_some());

        panel.apply_tree(Err((
            crate::tree::METHOD_NOT_FOUND,
            "no such method".to_owned(),
        )));
        assert!(panel.model.is_unavailable());
        assert!(panel.selection.is_none());
        assert!(panel.detail.is_none());
        assert!(panel.roots().is_empty());
        assert!(panel.model.status_zh().contains("server 版本"));
    }

    #[test]
    fn a_node_answer_becomes_a_detail_and_a_bad_one_becomes_a_message() {
        let mut panel = TreePanel::new();
        panel.apply_node(Ok(json!({
            "kind_type": "layer", "id": "city.usda", "identifier": "city.usda",
            "kind": "root", "strength": 1, "usda_text": "#usda 1.0\n", "spec_count": 2
        })));
        assert!(matches!(panel.detail, Some(NodeDetail::Layer(_))));
        assert!(panel.detail_error.is_none());

        panel.apply_node(Err((
            crate::tree::INVALID_PARAMS,
            "no node has the id".to_owned(),
        )));
        assert!(panel.detail.is_none());
        let e = panel.detail_error.clone().expect("an error line");
        assert!(e.contains("no node has the id"), "{e}");
    }

    #[test]
    fn a_prim_row_copies_its_path_and_a_layer_row_copies_nothing() {
        let mut panel = TreePanel::new();
        panel.apply_tree(Ok(sample_tree()));
        let rows = rows(&panel);
        let prim = rows
            .iter()
            .find(|(_, n)| n.kind == TreeNodeKind::Prim)
            .expect("a prim row");
        assert_eq!(
            copy_payload(&TreeSelection::of(&prim.1)).as_deref(),
            Some("/World/Block")
        );
        let layer = rows
            .iter()
            .find(|(_, n)| n.kind == TreeNodeKind::Layer)
            .expect("a layer row");
        assert_eq!(copy_payload(&TreeSelection::of(&layer.1)), None);
    }

    #[test]
    fn the_mask_rect_is_the_pick_rule_and_a_click_is_one_pixel() {
        let vp = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 50.0));
        let sel = mask_rect(
            vp,
            egui::pos2(10.0, 10.0),
            egui::pos2(50.0, 30.0),
            (200, 100),
        )
        .expect("a rect");
        // 200 frame texels over a 100-point viewport is 2 texels per point, and the
        // drag's end texel is included: 10→50 points is texels 20..=100.
        assert_eq!(
            (sel.width(), sel.height()),
            (81, 41),
            "points → frame texels, end texel included: {sel:?}"
        );
        let click = mask_rect(
            vp,
            egui::pos2(10.0, 10.0),
            egui::pos2(10.0, 10.0),
            (200, 100),
        )
        .expect("a click");
        assert_eq!((click.width(), click.height()), (1, 1));
    }

    #[test]
    fn a_toast_lives_four_seconds() {
        let now = std::time::Instant::now();
        assert!(toast_visible(now + TOAST_FOR / 2, now));
        assert!(!toast_visible(now + TOAST_FOR * 2, now));
    }
}
