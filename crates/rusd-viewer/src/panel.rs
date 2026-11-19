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
//!   Clicking any other row is unchanged: `comp_node{id}`, and the composition stays where
//!   it was (`docs/PROTOCOL.md` v1.4).
//! - **the root layer's row is a preview control too** (v1.5.3): clicking it sends
//!   `preview{upto: <root layer id>}`, which composes the root and the session
//!   overrides with **no cook op at all** -- the input-only picture a pipeline
//!   starts from. It carries the same `≤` marker and the same click as an op
//!   row, and it selects nothing: a control row's click is its preview
//!   (`docs/PROTOCOL.md` v1.5.3: 「root 行成为预览控制行」).
//! - **a material layer's row is the other form of control** (v1.8): a reference or a
//!   plain sublayer is not a stop in the cook order, so 「up to here」 has nothing to say
//!   about it; its click sends `preview{solo: <that layer>}` instead — that layer's content
//!   and nothing else. Which form a row asks for is [`ask_for`]'s answer over the row's own
//!   `op`/`root`/`session` flags, and the session layer is the one row that asks for
//!   nothing (its click is detail only). Both forms are the same `preview` method on the
//!   wire, so a refused id is reported the same way for either.
//! - **an op row says which op it is and what it reads and writes** (v1.5): the title
//!   is the generator's schema name (`RusdSamplePoints`, `RusdInstances`,
//!   `RusdScatter`) — with the op's anchor prim after it from v1.8, so two instances of
//!   one generator read `RusdInstances /World/Boxes` and `RusdInstances /World/Tufts` — and
//!   a dim subtitle reads `{input} → {output summary}`, with `(parametric)` where the op
//!   has no input geometry. A row whose answer carried no `op_name` falls back to the
//!   identifier, which is exactly the pre-v1.5 picture.
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

/// Which composition a **layer row's** click asks the server for (`docs/PROTOCOL.md`
/// v1.7's two preview forms).
///
/// The two are different questions about the same stack, and which one a row asks is the
/// **row's** to say — never a list of layer names kept anywhere else:
///
/// - [`LayerAsk::Upto`] is 「composed up to here」: everything the cook order reaches at or
///   before this layer. Only a cook op's output layer and the stack's root layer have
///   such a position.
/// - [`LayerAsk::Solo`] is 「this layer alone」: every other layer muted (the session layer
///   excepted, so an override stays visible). That is the only composition a document's
///   own material layer has — it is not a stop in the cook order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerAsk {
    Upto,
    Solo,
}

impl LayerAsk {
    /// The word a control reply reports this ask as (`tree{id}`'s `action`).
    pub fn as_str(self) -> &'static str {
        match self {
            LayerAsk::Upto => "preview-upto",
            LayerAsk::Solo => "preview-solo",
        }
    }
}

/// What a click in the tree asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeAction {
    /// Nothing this pass.
    None,
    /// A row was clicked: select it, ask its detail, and (when it stands for a
    /// prim) also select that prim and copy its path.
    Select(TreeSelection),
    /// A **layer row** was clicked -- the layer's own first-level row, or an arc's
    /// target-layer leaf, which is the same layer and therefore the same click.
    ///
    /// One variant carrying both halves, because the two are not alternatives: a layer
    /// row selects that layer (so its `comp_node{id}` detail is shown and the tree jumps
    /// to the layer's own row) **and**, when the layer is one of the two compositions a
    /// click can ask for, sends that request.
    ///
    /// `ask` says which, and there are three cases rather than two since v1.8:
    ///
    /// - `Some(Upto)` for a cook op's output layer (v1.4) or the stack's root layer
    ///   (v1.5.3) — a **cook prefix**, `preview{upto}`;
    /// - `Some(Solo)` for a **material** layer — a reference or a plain sublayer, which is
    ///   not a stop in the cook order at all, so `preview{upto}` has nothing to say about
    ///   it and the click asks to see that layer's content and nothing else instead;
    /// - `None` for a layer that is neither (the session layer, an arc's target leaf that
    ///   names a layer of the first two kinds' opposites…): the click is detail only.
    ///
    /// Two variants would let a row send a detail *instead of* a preview or the other way
    /// round, which is exactly the disagreement the user's ruling removes: 点 reference
    /// 的层叶子 == 点这个 layer 本体.
    Layer {
        selection: TreeSelection,
        ask: Option<LayerAsk>,
    },
    /// A **layer row** was clicked, and the row is a preview control in one of the two
    /// forms (`docs/PROTOCOL.md` v1.7): ask the server for the composition this names.
    ///
    /// Never produced by [`action_for`] — a layer row is a [`TreeAction::Layer`] and its
    /// `ask` says which form it wants. It exists for the 「final」 button
    /// ([`final_action`]), which is not a row at all, and for the shell's own
    /// `preview{upto}` / `preview{solo}` entry points.
    Preview(Option<String>),
    PreviewSolo(String),
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
    /// The layer row to bring into view on the next pass: a click on an arc's target-layer
    /// leaf asks the tree to jump to that layer's own first-level row, which is a row a
    /// reader may have scrolled far from (`TreePanel::scroll_target`).
    ///
    /// **Taken, not merely read**, at the start of the pass that draws ([`tree_panel`]),
    /// and set again by a click during that pass: so a click schedules exactly one scroll
    /// for the following frame, and a target the tree no longer holds just falls away
    /// instead of pinning the view somewhere forever.
    pub scroll_target: Option<String>,
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

    /// Ask the tree to bring a **layer's own row** into view on the next pass.
    ///
    /// This is what makes a click on an arc's target-layer leaf land somewhere: the leaf
    /// selects that layer and asks its detail, and the layer's first-level row may be
    /// scrolled far away, so a `Select` of a `Layer` row also sets this (the shell does
    /// it, once, in `on_tree_select`). Nothing here checks that the tree holds the id:
    /// [`tree_panel`] takes the target at the top of a pass and a row that never matches
    /// simply leaves it spent.
    pub fn scroll_to(&mut self, id: impl Into<String>) {
        self.scroll_target = Some(id.into());
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
/// A **layer row** is one kind of click whatever drew it: the layer's own root row, and
/// an arc's leaf that names it, are the same layer at the same id, so they are the same
/// [`TreeAction::Layer`] — select it, ask its detail, jump the tree to its own row, and
/// ask for the composition [`ask_for`] names. Every other row is a plain selection.
pub fn action_for(node: &TreeNode) -> TreeAction {
    let selection = TreeSelection::of(node);
    match node.kind {
        TreeNodeKind::Layer => TreeAction::Layer {
            selection,
            ask: ask_for(node),
        },
        _ => TreeAction::Select(selection),
    }
}

/// Which composition a **layer row's** click asks for, or `None` when it asks for none.
///
/// **One rule, over the row's own flags** — never a list of layer names, and never a
/// second opinion held by the control channel. The three cases are the three shapes a
/// layer in a stack actually has (`docs/PROTOCOL.md` v1.4 / v1.5.3 / v1.8):
///
/// - a cook op's output layer, and the stack's root layer, are **stops in the cook
///   order**: 「compose up to here」 is a question about them, so they ask
///   [`LayerAsk::Upto`] (`preview{upto}`);
/// - every other layer of a document — a reference, a plain sublayer — is **not** a stop
///   and has no prefix to show, so its click asks [`LayerAsk::Solo`] (`preview{solo}`:
///   that layer's content and nothing else);
/// - the session layer is neither: it is the layer every composition already starts from,
///   so `preview{upto: <session>}` would compose nothing at all and `solo` would hide the
///   document entirely. Its click is detail only, which is also v1.4's rule for a row
///   that is not a control.
///
/// An arc's leaf that *names* a layer carries the target's own `op`/`root` flags
/// (`crate::tree::layer_subtree`), so this answers for it exactly as for the layer's own
/// row — which is the user's ruling that the two clicks are one click.
pub fn ask_for(node: &TreeNode) -> Option<LayerAsk> {
    if node.kind != TreeNodeKind::Layer {
        return None;
    }
    ask_for_flags(node.op, node.root, node.session)
}

/// [`ask_for`] for a **layer** rather than a row: the same rule, for a caller that holds
/// the composition tree's layer list and no row (the status bar's badge, which names a
/// layer the tree may not currently have drawn).
pub fn ask_for_layer(layer: &crate::tree::LayerNode) -> Option<LayerAsk> {
    ask_for_flags(
        layer.op,
        crate::tree::is_root_layer(layer),
        crate::tree::is_session_layer(layer),
    )
}

/// **The rule itself**, over the three flags every layer row and layer carries.
///
/// One body and two adapters ([`ask_for`], [`ask_for_layer`]), because a row and the layer
/// it names must answer the same question the same way: a badge that said 「只看」 about a
/// layer whose row says 「预览 ≤」 would be two opinions of one click.
fn ask_for_flags(op: bool, root: bool, session: bool) -> Option<LayerAsk> {
    if op || root {
        Some(LayerAsk::Upto)
    } else if session {
        None
    } else {
        Some(LayerAsk::Solo)
    }
}

/// Whether `node` is the stack's **session layer** (`kind == "session"`), read off the
/// row's own flag.
///
/// The flag is on the row for the same reason [`TreeNode::op`] and [`TreeNode::root`] are:
/// the row says what a click means, and a leaf naming the layer carries the same flag as
/// the layer's own row (`crate::tree::layer_subtree`).
pub fn is_session_layer(node: &TreeNode) -> bool {
    node.kind == TreeNodeKind::Layer && node.session
}

/// Whether `node` is a row whose click asks for a **cook prefix**: a cook op's layer row
/// (v1.4) or the stack's root layer row (v1.5.3).
///
/// The narrower half of [`is_preview_row`]: this is the row that carries the `≤` marker in
/// its label, because `≤` means 「composed up to here」 and only a cook stop has such a
/// position. A material row's click asks for a **solo** ([`LayerAsk::Solo`]), which is a
/// different picture and gets the panel's marker instead (`≤` on the row in force,
/// [`control_row_title`]).
pub fn is_preview_control(node: &TreeNode) -> bool {
    node.kind == TreeNodeKind::Layer && (node.op || node.root)
}

/// Whether `node` is a layer row whose click asks for a composition of **any** form
/// ([`ask_for`]) — the predicate the drawing, the marker and the tooltip read, so a
/// material row looks like the control it is and not like an inert label.
pub fn is_preview_row(node: &TreeNode) -> bool {
    ask_for(node).is_some()
}

/// The 「final」 control's action: the whole composition (`preview{upto: null}`).
///
/// A function rather than a literal at the button, because it is the *other half* of the
/// op-row rule above and a test asserting one without the other would let them drift.
pub fn final_action() -> TreeAction {
    TreeAction::Preview(None)
}

/// Whether `node` is the layer row the viewport is **currently composed up to** —
/// whichever form the row asks for ([`is_preview_row`]) and whether the echoed layer is
/// this row's id.
///
/// Pure, so the highlight rule is testable without a screen. A prim or arc row whose id
/// happened to equal the string is not a preview row and must not be coloured as one.
pub fn is_current_preview(node: &TreeNode, preview: Option<&str>) -> bool {
    is_preview_row(node) && preview == Some(node.id.as_str())
}

/// Draw the tree panel's whole body: its status line, then either the rows or the
/// degraded sentence.
///
/// The panel is `&mut` for one reason: [`TreePanel::scroll_target`] is **taken** here, at
/// the top of the pass that draws the rows, so the row it names can bring itself into
/// view. A click made during this pass sets it again for the next frame (the shell does
/// that, from the action `row` returns), which is why one click asks for exactly one
/// scroll and a target the rebuilt tree no longer holds simply falls away.
pub fn tree_panel(ui: &mut egui::Ui, panel: &mut TreePanel) -> TreeAction {
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
    // The row to bring into view, taken so it is spent by this pass. Handed down as
    // `&mut` and cleared by the row that uses it: the tree holds a layer twice (its own
    // first-level row and every arc leaf that names it), and clearing is what keeps the
    // second match from scrolling somewhere else afterwards.
    let mut scroll_to = panel.scroll_target.take();
    // **[v1.4] the whole body reads which rows are previews from the rows themselves.**
    // `row` returns a `TreeAction`, not a `TreeSelection`, so the click rule lives in
    // exactly one place: [`action_for`], over the flags `crate::tree::roots` set.
    let mut action = TreeAction::None;
    egui::ScrollArea::vertical()
        .id_salt("composition-tree")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (root_index, root) in roots.iter().enumerate() {
                if let TreeAction::None = &action
                    && let Some(a) = row(
                        ui,
                        root,
                        0,
                        root_index,
                        panel.selection.as_ref(),
                        panel.preview.as_deref(),
                        &mut scroll_to,
                    )
                {
                    action = a;
                }
            }
        });
    action
}

/// One row, and its subtree. Returns what a click asked for, if anything.
///
/// `root` -- the index of the top-level row this subtree hangs from -- is part of the
/// row's **widget id**, not of what it means. Two layer roots can legitimately hold a
/// row with the same `comp_node` id (two layers referencing one target both draw an arc
/// class whose id is that target's prim, v1.5.4), and egui would then share one
/// collapsed/expanded state between them: opening one would open the other, and a
/// click on one would be read as a click on the other. `(id, root, depth)` is unique
/// per row of a drawn tree, because a row's depth and its root each have one path.
///
/// `scroll_to` is the id the panel is bringing into view (v1.5.8). **Only the
/// first-level row of that layer scrolls**: a layer is drawn once as its own root row
/// and again as a leaf under every arc that reaches it, so `depth == 0` is what says
/// which of them a reader means by "that layer's row". The row that scrolls clears the
/// target, so the leaf's later match in the same pass has nothing left to do.
fn row(
    ui: &mut egui::Ui,
    node: &TreeNode,
    depth: usize,
    root: usize,
    selected: Option<&TreeSelection>,
    preview: Option<&str>,
    scroll_to: &mut Option<String>,
) -> Option<TreeAction> {
    let is_selected = selected.is_some_and(|s| s.id == node.id);
    // A row that is **the current preview** is marked too: the user needs to see where
    // they are, and the current prefix is not a selection (it does not follow a
    // `comp_node` ask).
    let is_current_preview = is_current_preview(node, preview);
    // The row's **drawn** title: an op row already carries the `<=` marker in its label
    // (v1.4), and the root row gains it while it is the prefix in force (v1.5.3).
    let title = control_row_title(node, is_current_preview);
    let text = match node.kind {
        TreeNodeKind::Layer if is_preview_row(node) => RichText::new(title).strong().underline(),
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

    // What a click on this row means -- **one rule, for every row** ([`action_for`]).
    //
    // This is what makes an arc's target-layer leaf and the layer's own row the same
    // click (the user's ruling: 点 reference 的层叶子 == 点这个 layer 本体). The leaf is a
    // `Layer` row carrying the target's id *and its `op`/`root` flags*
    // (`crate::tree::layer_subtree`), so the one rule answers for both: the same
    // `comp_node{layer id}` detail, the same `preview{upto}` for a control, and the same
    // `scroll_target` (set by the shell from the selection). Nothing here asks which of
    // the two rows it is drawing.
    let click_action = action_for;

    // Whether this row is the one the panel was asked to bring into view.
    let wants_scroll = depth == 0 && scroll_to.as_deref() == Some(node.id.as_str());

    let mut clicked: Option<TreeAction> = None;
    if is_preview_row(node) {
        // **A control row is a control, not a twisty.** Its click previews; if it also
        // collapsed its subtree, one click would mean two things and the user could not
        // expand without changing what the viewport composes. A control layer's children
        // are a couple of arcs and variant sets at most (v1.3 draws no prim rows), so it
        // is drawn as a plain row with its subtree always visible.
        ui.horizontal(|ui| {
            indent(ui, depth);
            let response = ui
                .selectable_label(is_current_preview, text)
                .on_hover_text(row_hover_text(node, true));
            if wants_scroll {
                response.scroll_to_me(Some(Align::Center));
                *scroll_to = None;
            }
            if response.clicked() {
                // The same rule as every other row: a control layer row is a layer row.
                clicked = Some(click_action(node));
            }
            // v1.5: the row's second half — `{input} → {output summary}` — is drawn
            // **beside** the title and is not itself a control: one row, one click,
            // and that click is the preview. Drawing it outside the `selectable_label`
            // is what keeps the two from becoming two hit areas with one meaning.
            subtitle(ui, node);
        });
        for child in &node.children {
            if let Some(a) = row(ui, child, depth + 1, root, selected, preview, scroll_to) {
                clicked = Some(a);
            }
        }
    } else if node.children.is_empty() {
        ui.horizontal(|ui| {
            indent(ui, depth);
            let response = ui
                .selectable_label(is_selected || is_current_preview, text)
                .on_hover_text(row_hover_text(node, is_preview_row(node)));
            if wants_scroll {
                response.scroll_to_me(Some(Align::Center));
                *scroll_to = None;
            }
            if response.clicked() {
                clicked = Some(click_action(node));
            }
        });
    } else {
        // **A chevron expands, the label selects** (v1.5.4: 「聚合行自身点击：仍不选
        // prim（它是一条边）；展开箭头/行内 chevron 负责展开」). Two hit areas, two jobs --
        // which is why `CollapsingHeader` is not used here: its whole header is one
        // target, so a click meant to open the row would also be reported as a click on
        // the row, and the row's own click is a `comp_node` ask.
        //
        // Collapsed unless the row is a **layer root**: a layer's children have to be
        // visible (they are the arcs and variant sets the panel exists to show), while a
        // 240-instance class's children are the noise the aggregation removes -- those
        // open when the user asks, one row at a time.
        let id = ui.make_persistent_id((&node.id, root, depth));
        let mut open = ui.data(|d| d.get_temp::<bool>(id.with("open")).unwrap_or(depth == 0));
        ui.horizontal(|ui| {
            indent(ui, depth);
            // The chevron. A row with nothing to open still gets the column's width, so
            // the titles of a tree line up whatever branch they are on.
            let chevron = if node.children.is_empty() {
                "  "
            } else if open {
                "\u{25be}"
            } else {
                "\u{25b8}"
            };
            let enabled = !node.children.is_empty();
            let arrow = ui
                .add_enabled_ui(enabled, |ui| {
                    ui.add(egui::Button::new(chevron).frame(false))
                })
                .inner
                .on_hover_text(if enabled {
                    if open {
                        "收起"
                    } else {
                        "展开这一行的子节点"
                    }
                } else {
                    ""
                });
            if arrow.clicked() && enabled {
                open = !open;
                ui.data_mut(|d| d.insert_temp(id.with("open"), open));
            }
            let response = ui
                .selectable_label(is_selected || is_current_preview, text)
                .on_hover_text(row_hover_text(node, is_preview_row(node)));
            if wants_scroll {
                response.scroll_to_me(Some(Align::Center));
                *scroll_to = None;
            }
            if response.clicked() {
                clicked = Some(click_action(node));
            }
            subtitle(ui, node);
        });
        if open {
            for child in &node.children {
                if let Some(a) = row(ui, child, depth + 1, root, selected, preview, scroll_to) {
                    clicked = Some(a);
                }
            }
        }
    }
    clicked
}

/// The title a control row **draws**: its label, with the `≤` marker on it when that
/// row is the composition in force.
///
/// Op rows carry the marker in the label itself (`crate::tree::roots` builds
/// `≤ <generator> [...]`, v1.4), because for them it says "clicking previews up to
/// here". The root row is a control from v1.5.3 only, and the contract spells its
/// marker as part of *being the current preview* — 「`preview == root id` 时高亮 +
/// `≤` 标记」 — so it is added here, at the one place that knows both the row and the
/// state, rather than baked into a label the model cannot re-derive when the preview
/// moves. A **material** row (v1.8: `preview{solo}`) is the same case, for the same
/// reason.
///
/// A row titled by its **description** (the user's ruling for every described layer) is a
/// control row like any other: the marker is still about the click, not about the text.
pub fn control_row_title(node: &TreeNode, is_current_preview: bool) -> String {
    if is_current_preview && !node.op {
        format!("≤ {}", node.label)
    } else {
        node.label.clone()
    }
}

/// What a row says when the pointer rests on it: what its click means (for a control
/// row), then the **wire identifier** of the layer the row stands for.
///
/// The sentence names the **request the click sends**, so a user reading a tooltip can
/// write the same call a script would: `preview{upto: …}` for a cook prefix (an op row, or
/// the root row — which composes no op at all), `preview{solo: …}` for a material layer,
/// and neither sentence for the session layer, whose click is detail only.
///
/// The identifier is here and nowhere else on the row, because a row is titled by a
/// display name now: `anon:60:rusd_instances.usda` is what a request takes and what a log
/// line shows, so a user who needs to quote it hovers rather than reading it off every
/// row. A row whose label already *is* a wire spelling (a prim path, an arc id, a
/// variant set) carries no `hover` and gets an empty tooltip.
fn row_hover_text(node: &TreeNode, control: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if control {
        parts.push(match ask_for(node) {
            Some(LayerAsk::Upto) if node.op => {
                format!("预览到此 op 之前：preview{{upto: \"{}\"}}", node.id)
            }
            Some(LayerAsk::Upto) => format!(
                "预览到根层（不含任何 op）：preview{{upto: \"{}\"}}",
                node.id
            ),
            Some(LayerAsk::Solo) => format!(
                "只看这一层（其余除 session 全 mute）：preview{{solo: \"{}\"}}",
                node.id
            ),
            None => String::new(),
        });
    }
    if let Some(hover) = &node.hover {
        parts.push(hover.clone());
    }
    parts.join("\n")
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
            // The tree is passed so every line that names a layer names it the way the
            // tree's own rows do: one layer, one name, everywhere.
            for line in detail.lines(panel.model.tree()) {
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
