//! Read-only bottom-dock panel that renders one plan's dev-flow node graph with
//! per-node evidence, and human gates as stop-points.
//!
//! It reads the selected plan's rows from the `plan_store` SQLite store —
//! `plan(plan_id)` + `done_nodes(plan_id)` + `evidence_for(plan_id)` — and
//! projects the fixed 11-node route into per-node views. A done node renders
//! fully green only when its evidence satisfies the 1.28 invariant (falsifying
//! condition + negative-control flag + artifact all present); a done node whose
//! evidence is missing any of those renders visibly incomplete ("vacuous"), so
//! a green node over a vacuous acceptance — the 1.28 failure to prevent — is
//! never shown as green. Human gates (`merge`/`release`/`complete`) render as
//! stop-points regardless of done state. The panel observes, it does not drive:
//! the only mutation is local UI state (which plan is selected, which node's
//! evidence is expanded).

use std::collections::{HashMap, HashSet};

use gpui::{
    actions, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Render,
    Task, WeakEntity, Window,
};
use plan_store::{EvidenceRow, FleetPlanRow, PlanStore};
use ui::{prelude::*, Button, ButtonSize, IconName, Label, LabelSize};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(plan_view, [TogglePlanViewPanel]);

const PLAN_VIEW_PANEL_KEY: &str = "PlanViewPanel";

/// The dev-flow route and its human gates — the same readiness table the fleet
/// panel and the 1.23d coordinator use.
const ROUTE: [&str; 11] = [
    "plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept", "merge",
    "release", "complete",
];

fn is_human_gate(node: &str) -> bool {
    matches!(node, "merge" | "release" | "complete")
}

fn non_empty(value: Option<&str>) -> bool {
    value.map_or(false, |value| !value.trim().is_empty())
}

/// The 1.28 invariant as a predicate: a done node is "complete" (renders green)
/// iff its evidence carries all three signals — a falsifying condition, a
/// negative-control flag, and an artifact. Anything less is a vacuous green.
fn evidence_is_complete(evidence: Option<&EvidenceRow>) -> bool {
    match evidence {
        None => false,
        Some(evidence) => {
            non_empty(evidence.falsifying_condition.as_deref())
                && evidence.negative_control.is_some()
                && non_empty(evidence.artifact_sha.as_deref())
        }
    }
}

/// Which evidence signals a vacuous (done-but-not-complete) node is missing, in
/// a fixed order the panel renders and the tests assert.
fn missing_fields(evidence: Option<&EvidenceRow>) -> Vec<&'static str> {
    match evidence {
        None => vec!["evidence row"],
        Some(evidence) => {
            let mut missing = Vec::new();
            if !non_empty(evidence.falsifying_condition.as_deref()) {
                missing.push("falsifying condition");
            }
            if evidence.negative_control.is_none() {
                missing.push("negative-control flag");
            }
            if !non_empty(evidence.artifact_sha.as_deref()) {
                missing.push("artifact");
            }
            missing
        }
    }
}

/// The per-node state the panel renders.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeState {
    Done,
    Current,
    Active,
    Pending,
}

/// One route node as the plan view renders it: its state, its human-gate
/// membership, its evidence row (for done nodes), and whether that evidence
/// satisfies the 1.28 invariant.
#[derive(Debug, Clone)]
struct NodeView {
    node: String,
    state: NodeState,
    is_human_gate: bool,
    evidence: Option<EvidenceRow>,
    complete: bool,
}

impl NodeView {
    fn glyph(&self) -> &'static str {
        match self.state {
            NodeState::Done => "✔",
            NodeState::Current => "▶",
            NodeState::Active => "◉",
            NodeState::Pending => "○",
        }
    }

    /// The label the panel shows next to a node — the string the tests assert,
    /// so "vacuous renders visibly incomplete" is a tested property, not a
    /// visual assertion.
    fn state_label(&self) -> String {
        match self.state {
            NodeState::Done if self.complete => "done (evidence)".to_string(),
            NodeState::Done => "done (vacuous — incomplete)".to_string(),
            NodeState::Current => "current".to_string(),
            NodeState::Active => "active".to_string(),
            NodeState::Pending if self.is_human_gate => "stop-point (human gate)".to_string(),
            NodeState::Pending => "pending".to_string(),
        }
    }
}

/// Project the store's rows into the route's per-node views. `done` is the set
/// of completed node names; `evidence` maps a node name to its evidence row.
/// The node is `complete` only when it is done *and* its evidence satisfies the
/// invariant — a done node with no evidence row, or with an incomplete one, is
/// `vacuous`, never green.
fn derive_nodes(
    plan: &FleetPlanRow,
    done: &HashSet<String>,
    evidence: &HashMap<String, EvidenceRow>,
) -> Vec<NodeView> {
    ROUTE
        .iter()
        .map(|node| {
            let is_done = done.contains(*node);
            let state = if is_done {
                NodeState::Done
            } else if plan.active_node.as_deref() == Some(node) {
                NodeState::Active
            } else if plan.current_node.as_deref() == Some(node) {
                NodeState::Current
            } else {
                NodeState::Pending
            };
            let evidence_row = evidence.get(*node).cloned();
            let complete = is_done && evidence_is_complete(evidence_row.as_ref());
            NodeView {
                node: (*node).to_string(),
                state,
                is_human_gate: is_human_gate(node),
                evidence: evidence_row,
                complete,
            }
        })
        .collect()
}

fn render_evidence(node: &NodeView) -> impl IntoElement {
    let rows: Vec<String> = match (&node.evidence, node.complete) {
        (Some(evidence), true) => vec![
            format!("falsifying condition: {}", evidence.falsifying_condition.as_deref().unwrap_or("—")),
            format!(
                "negative control: {}",
                if evidence.negative_control.is_some() { "yes" } else { "no" }
            ),
            format!("artifact: {}", evidence.artifact_sha.as_deref().unwrap_or("—")),
            format!(
                "gate exit: {}",
                evidence.gate_exit_code.map_or("—".to_string(), |code| code.to_string())
            ),
            format!("report: {}", evidence.report_path.as_deref().unwrap_or("—")),
        ],
        (Some(_), false) => vec![format!("missing: {}", missing_fields(node.evidence.as_ref()).join(", "))],
        (None, _) => vec!["no evidence recorded".to_string()],
    };

    v_flex()
        .gap_0p5()
        .pl_4()
        .children(rows.iter().map(|row| Label::new(row.clone()).size(LabelSize::Small)))
}

pub struct PlanViewPanel {
    focus_handle: FocusHandle,
    plans: Vec<FleetPlanRow>,
    selected_plan_id: Option<String>,
    nodes: Vec<NodeView>,
    expanded_node: Option<String>,
    last_error: Option<String>,
}

impl Focusable for PlanViewPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PlanViewPanel {}

impl PlanViewPanel {
    pub fn new(
        _workspace: &Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                plans: Vec::new(),
                selected_plan_id: None,
                nodes: Vec::new(),
                expanded_node: None,
                last_error: None,
            };
            this.refresh();
            this
        })
    }

    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: &mut gpui::AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                PlanViewPanel::new(workspace, window, cx)
            })
        })
    }

    fn select(&mut self, plan_id: String) {
        self.selected_plan_id = Some(plan_id);
        self.refresh();
    }

    fn refresh(&mut self) {
        let store = match PlanStore::open_default() {
            Ok(store) => store,
            Err(error) => {
                self.plans.clear();
                self.selected_plan_id = None;
                self.nodes.clear();
                self.last_error = Some(format!("Failed to open the plan store: {error:#}"));
                return;
            }
        };

        self.plans = store.fleet_plans().unwrap_or_default();
        let selected = self
            .selected_plan_id
            .clone()
            .filter(|id| self.plans.iter().any(|plan| &plan.id == id))
            .or_else(|| self.plans.first().map(|plan| plan.id.clone()));
        self.selected_plan_id = selected;
        self.project(&store);
        self.last_error = None;
    }

    fn project(&mut self, store: &PlanStore) {
        let Some(plan_id) = self.selected_plan_id.clone() else {
            self.nodes.clear();
            return;
        };
        let Ok(Some(plan)) = store.plan(&plan_id) else {
            self.nodes.clear();
            return;
        };
        let done: HashSet<String> = store
            .done_nodes(&plan_id)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let evidence: HashMap<String, EvidenceRow> = store
            .evidence_for(&plan_id)
            .unwrap_or_default()
            .into_iter()
            .map(|evidence| (evidence.node.clone(), evidence))
            .collect();
        self.nodes = derive_nodes(&plan, &done, &evidence);
    }
}

impl Render for PlanViewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let refresh = cx.listener(|this: &mut PlanViewPanel, _, _, cx| {
            this.refresh();
            cx.notify();
        });

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(Label::new("Plan view"))
                    .child(
                        Button::new("plan-view-refresh", "Refresh")
                            .size(ButtonSize::Compact)
                            .on_click(refresh),
                    ),
            )
            .child(
                div()
                    .id("plan-view-scroll")
                    .flex_col()
                    .size_full()
                    .overflow_y_scroll()
                    .px_2()
                    .gap_1()
                    .when_some(self.last_error.clone(), |element, error| {
                        element.child(Label::new(error))
                    })
                    .when(self.plans.is_empty(), |element| {
                        element.child(Label::new("(no plans in the store)").size(LabelSize::Small))
                    })
                    .child(Label::new("Plans").weight(FontWeight::SEMIBOLD))
                    .children(self.plans.iter().map(|plan| {
                        let selected = self.selected_plan_id.as_deref() == Some(plan.id.as_str());
                        let label = format!(
                            "{}{}",
                            if selected { "▶ " } else { "  " },
                            plan.name
                        );
                        let select = cx.listener({
                            let id = plan.id.clone();
                            move |this: &mut PlanViewPanel, _, _, cx| {
                                this.select(id.clone());
                                cx.notify();
                            }
                        });
                        Button::new(format!("plan-view-select-{}", plan.id), label)
                            .size(ButtonSize::Compact)
                            .on_click(select)
                    }))
                    .child(
                        Label::new(match &self.selected_plan_id {
                            Some(id) => format!("Nodes — {id}"),
                            None => "Nodes".to_string(),
                        })
                        .weight(FontWeight::SEMIBOLD),
                    )
                    .children(self.nodes.iter().map(|node| {
                        let expanded = self.expanded_node.as_deref() == Some(node.node.as_str());
                        let row_label = format!(
                            "{} {} — {}{}",
                            node.glyph(),
                            node.node,
                            node.state_label(),
                            if node.is_human_gate && node.state != NodeState::Done {
                                " (⏸)"
                            } else {
                                ""
                            }
                        );
                        let toggle = cx.listener({
                            let node_name = node.node.clone();
                            move |this: &mut PlanViewPanel, _, _, cx| {
                                this.expanded_node =
                                    if this.expanded_node.as_deref() == Some(node_name.as_str()) {
                                        None
                                    } else {
                                        Some(node_name.clone())
                                    };
                                cx.notify();
                            }
                        });
                        v_flex()
                            .gap_0p5()
                            .child(
                                Button::new(format!("plan-view-node-{}", node.node), row_label)
                                    .size(ButtonSize::Compact)
                                    .on_click(toggle),
                            )
                            .when(expanded, |element| element.child(render_evidence(node)))
                    })),
            )
    }
}

impl Panel for PlanViewPanel {
    fn persistent_name() -> &'static str {
        "PlanViewPanel"
    }

    fn panel_key() -> &'static str {
        PLAN_VIEW_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.refresh();
        }
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        px(300.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTree)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Plan view")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(TogglePlanViewPanel)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

/// Registers the panel toggle action on every new workspace.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(
            |workspace: &mut Workspace, _: &TogglePlanViewPanel, window, cx| {
                workspace.toggle_panel_focus::<PlanViewPanel>(window, cx);
            },
        );
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> PlanStore {
        PlanStore::open_in_memory().expect("open in-memory store")
    }

    fn plan_row(current: Option<&str>, active: Option<&str>) -> FleetPlanRow {
        FleetPlanRow {
            id: "P".to_string(),
            name: "P".to_string(),
            repo: Some("zed".to_string()),
            current_node: current.map(str::to_string),
            active_node: active.map(str::to_string),
            holder: None,
            heartbeat: None,
            blocked_reason: None,
            spent_tokens: 0,
        }
    }

    fn evidence(
        node: &str,
        falsifying: Option<&str>,
        negative_control: Option<i64>,
        artifact: Option<&str>,
    ) -> EvidenceRow {
        EvidenceRow {
            plan_id: "P".to_string(),
            node: node.to_string(),
            artifact_sha: artifact.map(str::to_string),
            gate_exit_code: Some(0),
            report_path: None,
            falsifying_condition: falsifying.map(str::to_string),
            negative_control,
        }
    }

    fn complete_evidence(node: &str) -> EvidenceRow {
        evidence(node, Some("the bug reproduces on the pre-fix artifact"), Some(1), Some("abc123"))
    }

    fn node<'a>(nodes: &'a [NodeView], name: &str) -> &'a NodeView {
        nodes.iter().find(|node| node.node == name).unwrap()
    }

    #[test]
    fn a_done_node_with_full_evidence_is_complete_and_green() {
        let plan = plan_row(None, None);
        let done = HashSet::from(["plan".to_string()]);
        let evidence = HashMap::from([("plan".to_string(), complete_evidence("plan"))]);

        let nodes = derive_nodes(&plan, &done, &evidence);
        let view = node(&nodes, "plan");
        assert_eq!(view.state, NodeState::Done);
        assert!(view.complete);
        assert!(evidence_is_complete(view.evidence.as_ref()));
        assert_eq!(view.glyph(), "✔");
        assert_eq!(view.state_label(), "done (evidence)");
    }

    #[test]
    fn a_done_node_without_falsifying_condition_is_vacuous() {
        let plan = plan_row(None, None);
        let done = HashSet::from(["plan".to_string()]);
        let evidence = HashMap::from([(
            "plan".to_string(),
            evidence("plan", None, Some(1), Some("abc123")),
        )]);

        let nodes = derive_nodes(&plan, &done, &evidence);
        let view = node(&nodes, "plan");
        assert_eq!(view.state, NodeState::Done);
        assert!(!view.complete);
        assert_eq!(missing_fields(view.evidence.as_ref()), vec!["falsifying condition"]);
        assert_eq!(view.state_label(), "done (vacuous — incomplete)");
        // Visibly distinct from a complete node: the glyph is not a bare green
        // check and the label names the vacuous state.
        assert_ne!(view.state_label(), "done (evidence)");
    }

    #[test]
    fn a_done_node_without_negative_control_is_vacuous() {
        let plan = plan_row(None, None);
        let done = HashSet::from(["verify".to_string()]);
        let evidence = HashMap::from([(
            "verify".to_string(),
            evidence("verify", Some("still broken: clippy red"), None, Some("abc123")),
        )]);

        let nodes = derive_nodes(&plan, &done, &evidence);
        let view = node(&nodes, "verify");
        assert!(!view.complete);
        assert_eq!(missing_fields(view.evidence.as_ref()), vec!["negative-control flag"]);
    }

    #[test]
    fn a_done_node_without_artifact_is_vacuous() {
        let plan = plan_row(None, None);
        let done = HashSet::from(["verify".to_string()]);
        let evidence = HashMap::from([(
            "verify".to_string(),
            evidence("verify", Some("still broken: clippy red"), Some(1), None),
        )]);

        let nodes = derive_nodes(&plan, &done, &evidence);
        let view = node(&nodes, "verify");
        assert!(!view.complete);
        assert_eq!(missing_fields(view.evidence.as_ref()), vec!["artifact"]);
    }

    #[test]
    fn a_done_node_with_no_evidence_row_is_vacuous() {
        let plan = plan_row(None, None);
        let done = HashSet::from(["plan".to_string()]);
        let evidence = HashMap::new();

        let nodes = derive_nodes(&plan, &done, &evidence);
        let view = node(&nodes, "plan");
        assert_eq!(view.state, NodeState::Done);
        assert!(!view.complete);
        assert!(view.evidence.is_none());
        assert_eq!(missing_fields(view.evidence.as_ref()), vec!["evidence row"]);
    }

    #[test]
    fn human_gates_are_stop_points_regardless_of_done_state() {
        let plan = plan_row(None, None);
        let done = HashSet::from(["merge".to_string()]);
        let evidence = HashMap::from([("merge".to_string(), complete_evidence("merge"))]);

        let nodes = derive_nodes(&plan, &done, &evidence);
        for gate in ["merge", "release", "complete"] {
            let view = node(&nodes, gate);
            assert!(view.is_human_gate);
        }
        // A pending human gate renders as a stop-point, not a plain pending node.
        let pending_merge = node(&nodes, "release");
        assert_eq!(pending_merge.state, NodeState::Pending);
        assert_eq!(pending_merge.state_label(), "stop-point (human gate)");
    }

    #[test]
    fn pending_and_done_nodes_render_distinctly() {
        let plan = plan_row(Some("implement"), Some("implement"));
        let done = HashSet::from(["plan", "promote", "claim", "fork"].map(str::to_string));
        let evidence = HashMap::new();

        let nodes = derive_nodes(&plan, &done, &evidence);
        assert_eq!(node(&nodes, "plan").state, NodeState::Done);
        assert_eq!(node(&nodes, "implement").state, NodeState::Active);
        assert_eq!(node(&nodes, "verify").state, NodeState::Pending);
        assert_ne!(node(&nodes, "plan").glyph(), node(&nodes, "verify").glyph());
    }

    #[test]
    fn selection_projects_the_chosen_plans_nodes_from_the_store() {
        let store = store();
        store
            .apply_graph_snapshot(&plan_store::GraphSnapshot {
                plan_id: "P".to_string(),
                name: "P".to_string(),
                repo: Some("zed".to_string()),
                current_node: Some("verify".to_string()),
                active_node: None,
                holder: None,
                heartbeat: None,
                blocked_reason: None,
                spent_tokens: 0,
                done_nodes: vec!["plan".to_string(), "promote".to_string()],
                evidence: vec![complete_evidence("plan")],
            })
            .unwrap();

        let plan = store.plan("P").unwrap().unwrap();
        let done: HashSet<String> = store.done_nodes("P").unwrap().into_iter().collect();
        let evidence: HashMap<String, EvidenceRow> = store
            .evidence_for("P")
            .unwrap()
            .into_iter()
            .map(|evidence| (evidence.node.clone(), evidence))
            .collect();
        let nodes = derive_nodes(&plan, &done, &evidence);

        assert_eq!(node(&nodes, "plan").complete, true);
        // `promote` is done but has no evidence row — vacuous, not green.
        assert_eq!(node(&nodes, "promote").complete, false);
        assert_eq!(node(&nodes, "verify").state, NodeState::Current);
    }
}
