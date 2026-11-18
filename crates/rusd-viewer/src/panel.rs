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
//! - **an op row is a preview control** (v1.4): a cook generator's output layer is
//!   drawn with `≤` and clicking it sends `preview{upto: <that layer>}` instead of a
//!   `comp_node` ask. The 「final」 button beside 「刷新」 sends `preview{upto: null}`.
//!   Clicking any other row is unchanged: `comp_node{id}`, and the preview stays where
//!   it was (`docs/PROTOCOL.md` v1.4).
//! - **the root layer's row is a preview control too** (v1.5.3): clicking it sends
//!   `preview{upto: <root layer id>}`, which composes the root and the session
//!   overrides with **no cook op at all** -- the input-only picture a pipeline
//!   starts from. It carries the same `≤` marker and the same click as an op
//!   row, and it selects nothing: a control row's click is its preview
//!   (`docs/PROTOCOL.md` v1.5.3: 「root 行成为预览控制行」).
//! - **an op row says which op it is and what it reads and writes** (v1.5): the title
//!   is the generator's schema name (`RusdSamplePoints`, `RusdInstances`,
//!   `RusdScatter`) rather than an anonymous identifier, and a dim subtitle reads
//!   `{input} → {output summary}`, with `(parametric)` where the op has no input
//!   geometry. A row whose answer carried no `op_name` falls back to the identifier,
//!   which is exactly the pre-v1.5 picture.
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
    /// A **preview control row** was clicked: an op row (v1.4) or the root layer's
    /// row (v1.5.3). Ask the server for the prefix composed up to that layer;
    /// `None` is the 「final」 control — the whole composition.
    ///
    /// A third variant rather than a flag on `Select`, because the two clicks do
    /// different things: `Select` asks `comp_node{id}` for a detail and leaves the
    /// preview alone (`docs/PROTOCOL.md` v1.4: 点非 op 节点仍走 comp_node 详情，不改
    /// 预览), while this asks `preview{upto}` and asks no detail. A caller that had to
    /// tell them apart from one value would have to re-derive the panel's own rule.
    Preview(Option<String>),
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
    /// The composition the viewport is currently showing, as the server echoed it in
    /// `stage_snapshot.upto` (v1.4): `None` is the final composition.
    ///
    /// The shell keeps it equal to `ClientModel::preview`; the panel only reads it, to
    /// highlight the row that is where the user already is. It is **not** what the
    /// panel sends — a click sends the row's own identifier — so a stale copy cannot
    /// make a click ask for the wrong prefix, only mis-colour one row for a pass.
    pub preview: Option<String>,
    /// Whether a `composition_tree` request is **outstanding**.
    ///
    /// A `state_changed` now asks for the tree (a patch or an opinion can add or drop a
    /// cook op layer), and a gizmo drag sends an opinion up to 30 times a second
    /// (`docs/PROTOCOL.md`: ≤ 30 Hz). Asking per reason would therefore put ~30
    /// structural reads of the authority stack in flight during one drag, each one a
    /// whole stack walk on the server and a fresh row model here. So a reason that
    /// arrives while an answer is outstanding only records that another read is owed:
    /// the tree still follows the server, and it follows it once per round trip instead
    /// of once per notification.
    pub asking: bool,
    /// A re-read was asked for while one was outstanding (`TreePanel::take_reread`).
    pub reread_pending: bool,
}

impl TreePanel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a `composition_tree` answer.
    ///
    /// Clears [`TreePanel::asking`]: the round trip is over, so the next reason may ask
    /// again. A re-read that was asked for meanwhile is **not** sent from here (the
    /// panel holds no socket); the shell takes it with [`TreePanel::take_reread`].
    pub fn apply_tree(&mut self, reply: Result<serde_json::Value, (i64, String)>) {
        self.asking = false;
        // A degraded or failed answer also drops the detail: it describes rows of a
        // tree that no longer exists.
        if self.model.apply(reply).is_err() {
            self.detail = None;
            self.detail_error = None;
            self.selection = None;
        }
    }

    /// Take the re-read that was owed while a request was outstanding: `true` means
    /// the caller must ask again now.
    ///
    /// Called after [`TreePanel::apply_tree`], once the answer has cleared `asking`. A
    /// pending re-read is **coalesced**, not queued: however many reasons arrived during
    /// the round trip, one more read answers all of them (each reason moved the same
    /// layer stack on).
    pub fn take_reread(&mut self) -> bool {
        std::mem::take(&mut self.reread_pending)
    }

    /// Say a `composition_tree` request went out, or that one could not be sent (the
    /// panel is then not left believing one is outstanding).
    pub fn note_asked(&mut self, outstanding: bool) {
        self.asking = outstanding;
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

/// What a click on `node` asks the shell to do — **the one place that rule lives**.
///
/// A **preview control row** previews the composition up to that layer: an op row
/// (v1.4) or the root layer's row (v1.5.3, [`is_preview_control`]). Every other row
/// is a selection, which asks `comp_node{id}` and leaves the preview alone.
/// `panel::row` draws from this and the tests assert against it, so the picture and
/// the wire cannot disagree about which click does which.
pub fn action_for(node: &TreeNode) -> TreeAction {
    if is_preview_control(node) {
        TreeAction::Preview(Some(node.id.clone()))
    } else {
        TreeAction::Select(TreeSelection::of(node))
    }
}

/// Whether `node` is a row whose click is a **preview** rather than a selection:
/// a cook op's layer row (v1.4) or the stack's root layer row (v1.5.3).
///
/// One predicate for the click, the highlight and the drawing, because all three are
/// the same question asked at three places: a row that previews must look like one
/// (the `≤` marker), be marked when it is the prefix in force, and send `preview`
/// when clicked. An arc's leaf that *names* a layer is a `Layer` row too and is
/// deliberately not one of these: it points at a branch and is clicked to inspect it.
pub fn is_preview_control(node: &TreeNode) -> bool {
    node.kind == TreeNodeKind::Layer && (node.op || node.root)
}

/// The 「final」 control's action: the whole composition (`preview{upto: null}`).
///
/// A function rather than a literal at the button, because it is the *other half* of the
/// op-row rule above and a test asserting one without the other would let them drift.
pub fn final_action() -> TreeAction {
    TreeAction::Preview(None)
}

/// Whether `node` is the layer row the viewport is currently composed up to
/// (`docs/PROTOCOL.md` v1.4: the panel marks the prefix in force).
///
/// Pure, so the highlight rule is testable without a screen: a preview control row
/// ([`is_preview_control`] -- an op row, or the root layer's row since v1.5.3) whose
/// id is the echoed `upto`. A prim or arc row whose id happened to equal the string
/// is not a preview row and must not be coloured as one.
pub fn is_current_preview(node: &TreeNode, preview: Option<&str>) -> bool {
    is_preview_control(node) && preview == Some(node.id.as_str())
}

/// Draw the tree panel's whole body: its status line, then either the rows or the
/// degraded sentence.
pub fn tree_panel(ui: &mut egui::Ui, panel: &TreePanel) -> TreeAction {
    let mut refresh = false;
    let mut final_clicked = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(panel.model.status_zh()).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("刷新").clicked() {
                refresh = true;
            }
            // The 「final」 control (v1.4: 「final」 行/按钮 → preview{null}). A button as
            // well as the rows because the final composition is *not* a layer row — an
            // op row is a prefix, and "the whole thing" has to be reachable after a
            // preview even when no row stands for it. Green when it is already on
            // screen, so which of the two states the viewport is in is visible without
            // clicking anything.
            let is_final = panel.preview.is_none();
            let text = if is_final {
                RichText::new("final ✓")
            } else {
                RichText::new("final")
            };
            let button = egui::Button::new(text).fill(if is_final {
                Color32::from_rgb(40, 80, 60)
            } else {
                Color32::TRANSPARENT
            });
            if ui
                .add(button)
                .on_hover_text("显示完整合成（preview{upto: null}）")
                .clicked()
            {
                final_clicked = true;
            }
        });
    });
    if refresh {
        return TreeAction::Refresh;
    }
    if final_clicked {
        return final_action();
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
    // **[v1.4] the whole body reads which rows are previews from the rows themselves.**
    // `row` returns a `TreeAction`, not a `TreeSelection`, so the op-row rule lives in
    // exactly one place: the `op` flag `crate::tree::roots` set.
    let mut action = TreeAction::None;
    egui::ScrollArea::vertical()
        .id_salt("composition-tree")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for root in &roots {
                if let TreeAction::None = &action
                    && let Some(a) = row(
                        ui,
                        root,
                        0,
                        panel.selection.as_ref(),
                        panel.preview.as_deref(),
                    )
                {
                    action = a;
                }
            }
        });
    action
}

/// One row, and its subtree. Returns what a click asked for, if anything.
fn row(
    ui: &mut egui::Ui,
    node: &TreeNode,
    depth: usize,
    selected: Option<&TreeSelection>,
    preview: Option<&str>,
) -> Option<TreeAction> {
    let is_selected = selected.is_some_and(|s| s.id == node.id);
    // A preview control row that is **the current preview** is marked too: the user
    // needs to see where they are, and the current prefix is not a selection (it
    // does not follow a `comp_node` ask).
    let is_current_preview = is_current_preview(node, preview);
    // The row's **drawn** title: an op row already carries the `<=` marker in its label
    // (v1.4), and the root row gains it while it is the prefix in force (v1.5.3).
    let title = control_row_title(node, is_current_preview);
    let text = match node.kind {
        TreeNodeKind::Layer if is_preview_control(node) => {
            RichText::new(title).strong().underline()
        }
        TreeNodeKind::Layer => RichText::new(title).strong(),
        TreeNodeKind::Arc => RichText::new(&node.label).italics(),
        TreeNodeKind::Prim | TreeNodeKind::VariantSet => RichText::new(&node.label),
    };
    let text = if is_current_preview {
        text.color(Color32::from_rgb(140, 230, 170))
    } else if is_selected {
        text.color(Color32::from_rgb(120, 200, 255))
    } else {
        text
    };

    // What a click on this row means: a preview for a control row (an op layer's or
    // the root layer's), a selection otherwise.
    let click_action = action_for;

    let mut clicked: Option<TreeAction> = None;
    if is_preview_control(node) {
        // **A control row is a control, not a twisty.** Its click previews; if it also
        // collapsed its subtree, one click would mean two things and the user could not
        // expand without changing what the viewport composes. A control layer's children
        // are a couple of arcs and variant sets at most (v1.3 draws no prim rows), so it
        // is drawn as a plain row with its subtree always visible.
        ui.horizontal(|ui| {
            indent(ui, depth);
            if ui
                .selectable_label(is_current_preview, text)
                .on_hover_text(preview_hover_text(node))
                .clicked()
            {
                clicked = Some(TreeAction::Preview(Some(node.id.clone())));
            }
            // v1.5: the row's second half — `{input} → {output summary}` — is drawn
            // **beside** the title and is not itself a control: one row, one click,
            // and that click is the preview. Drawing it outside the `selectable_label`
            // is what keeps the two from becoming two hit areas with one meaning.
            subtitle(ui, node);
        });
        for child in &node.children {
            if let Some(a) = row(ui, child, depth + 1, selected, preview) {
                clicked = Some(a);
            }
        }
    } else if node.children.is_empty() {
        ui.horizontal(|ui| {
            indent(ui, depth);
            if ui
                .selectable_label(is_selected || is_current_preview, text)
                .clicked()
            {
                clicked = Some(click_action(node));
            }
        });
    } else {
        // **Collapsed unless the row is a layer root** (v1.5.4). A layer's children
        // have to be visible -- they are the arcs and variant sets the panel exists
        // to show -- but a row that stands for a *prim* is where a 240-instance
        // cluster stops being 240 rows of noise: those open on a click, one at a
        // time.
        //
        // The click on the header is the row's own click (`selectable` + `clicked`),
        // and the arrow beside it is the only thing that expands: one row, one
        // meaning, which is the same rule a control row follows.
        let header = egui::CollapsingHeader::new(text)
            .id_salt((&node.id, depth))
            .default_open(depth == 0)
            .show(ui, |ui| {
                for child in &node.children {
                    if let Some(a) = row(ui, child, depth + 1, selected, preview) {
                        clicked = Some(a);
                    }
                }
            });
        if header.header_response.clicked() {
            clicked = Some(click_action(node));
        }
    }
    clicked
}

/// The title a control row **draws**: its label, with the `≤` marker on it when that
/// row is the prefix in force.
///
/// Op rows carry the marker in the label itself (`crate::tree::roots` builds
/// `≤ <generator> [...]`, v1.4), because for them it says "clicking previews up to
/// here". The root row is a control from v1.5.3 only, and the contract spells its
/// marker as part of *being the current preview* — 「`preview == root id` 时高亮 +
/// `≤` 标记」 — so it is added here, at the one place that knows both the row and the
/// state, rather than baked into a label the model cannot re-derive when the preview
/// moves.
///
/// Pure, so the marker is testable without a screen: `(row, is_current_preview)` in,
/// the drawn text out.
pub fn control_row_title(node: &TreeNode, is_current_preview: bool) -> String {
    if is_current_preview && !node.op {
        format!("≤ {}", node.label)
    } else {
        node.label.clone()
    }
}

/// What a control row says when the pointer rests on it: which click it is.
///
/// Two sentences rather than one, because the two prefixes are not the same thing: an
/// op row's `upto` includes that op, and the root row's includes **no** op at all
/// (v1.5.3). Both name the identifier the wire takes, so a user reading the tooltip
/// can quote it.
fn preview_hover_text(node: &TreeNode) -> String {
    if node.op {
        format!("预览到此 op 之前：preview{{upto: \"{}\"}}", node.id)
    } else {
        format!(
            "预览到根层（不含任何 op）：preview{{upto: \"{}\"}}",
            node.id
        )
    }
}

fn indent(ui: &mut egui::Ui, depth: usize) {
    if depth > 0 {
        ui.add_space(8.0 * depth as f32);
    }
}

/// A row's dim secondary text (v1.5), when it has one.
///
/// Weak and small so it reads as an annotation rather than a second label, and never
/// as another control: the op row's click stays the title's.
fn subtitle(ui: &mut egui::Ui, node: &TreeNode) {
    if let Some(text) = &node.secondary {
        ui.label(
            RichText::new(text)
                .small()
                .weak()
                .color(Color32::from_rgb(160, 170, 180)),
        );
    }
}

/// Draw the detail of the selected node (`comp_node{id}`), or why there is none.
///
/// **A prim's detail is drawn by layer** (the user's ruling: 详情面板一律以「层」为主语).
/// A picked mesh -- from the viewport, or from an arc class's opened instance row --
/// is shown as one section per layer that authors it: the layer's own header (id, kind,
/// strength, and a cook op's generator name), then that layer's specifier and the
/// attributes it authors. The panel therefore answers "which layer says this", which
/// is the question a composite stack is read with, rather than "what does this prim
/// have".
///
/// Every other detail keeps its flat lines: a layer's own text, an arc's target spec,
/// a variant set's variants -- each is already about one layer.
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
            if let NodeDetail::Prim(p) = detail {
                prim_detail(ui, p, panel.model.tree());
                return;
            }
            for line in detail.lines() {
                ui.label(RichText::new(line).monospace());
            }
        });
}

/// One prim's detail, **by layer**: a header naming the prim, then a section per layer.
///
/// Pure text layout, no widgets that could take a click: a detail is read, not driven.
/// The section order is `comp_node`'s own (strongest layer first), so what is at the top
/// of the panel is what the composite is decided by.
pub fn prim_detail(
    ui: &mut egui::Ui,
    detail: &crate::tree::PrimDetail,
    tree: Option<&crate::tree::CompositionTree>,
) {
    ui.label(
        RichText::new(format!("prim {}", detail.path))
            .monospace()
            .strong(),
    );
    ui.label(
        RichText::new(format!(
            "{} 层写过它 · {} arcs out · {} variant sets",
            detail.opinions.len(),
            detail.arcs_out.len(),
            detail.variant_sets.len()
        ))
        .small()
        .weak(),
    );
    ui.add_space(2.0);
    for section in detail.layer_sections(tree) {
        ui.add_space(4.0);
        ui.label(RichText::new(format!("── {}", section.header)).strong());
        for line in section.lines() {
            ui.label(RichText::new(format!("    {line}")).monospace());
        }
    }
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
    fn no_prim_rows_are_drawn_and_a_layer_row_copies_nothing() {
        // v1.3 (`docs/PROTOCOL.md` 末节): a layer draws no rows for the prims it
        // *authors* -- a picked prim is inspected in the viewport instead. v1.5.4
        // adds one kind of prim row (an arc class's instances, under the arc), so the
        // assertion is about what hangs off a **layer**, which is what v1.3 ruled out.
        // The clipboard rule itself is unchanged and is covered from the pick side in
        // `tests/tree_v13.rs`.
        let mut panel = TreePanel::new();
        panel.apply_tree(Ok(sample_tree()));
        let rows = rows(&panel);
        let under_a_layer: Vec<&str> = rows
            .iter()
            .filter(|(depth, n)| *depth == 1 && n.kind == TreeNodeKind::Prim)
            .map(|(_, n)| n.label.as_str())
            .collect();
        assert!(
            under_a_layer.is_empty(),
            "a layer must not draw the prims it authors: {under_a_layer:?}"
        );
        let layer = rows
            .iter()
            .find(|(_, n)| n.kind == TreeNodeKind::Layer)
            .expect("a layer row");
        assert_eq!(
            copy_payload(&TreeSelection::of(&layer.1)),
            None,
            "a layer identifier is not a prim path"
        );
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
