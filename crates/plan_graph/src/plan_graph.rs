//! Interactive node/flow graph of the dev-flow plan state (1.29 re-scope).
//!
//! Replaces the text-first `roadmap_panel`/`plan_view` with one main-editor-area
//! `Item` that renders the plan DAG from the `plan_store` (graph-state plans +
//! `edges`) as a layered node graph: a plan is a **node**, its `Blocked by` /
//! `Related` relations are **edges**, and build progress is a **color/shape**
//! with a legend — never a text waterfall. The pure projection
//! (`derive_node_state` / `node_color` / `node_shape` / `tooltip_text`) is the
//! testable core; the render only positions what it projects.

use std::collections::{BTreeSet, HashSet};

use chrono::{DateTime, Utc};
use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, Window, actions};
use plan_store::{EdgeRow, FleetPlanRow, PlanStore};
use ui::{Label, LabelSize, prelude::*};
use workspace::Workspace;
use workspace::item::Item;

actions!(plan_graph, [DeployPlanGraph]);

/// The dev-flow route — the same readiness table the `graph_tool` coordinator
/// and the fleet panel use. The y-axis of the layered layout.
const ROUTE: [&str; 11] = [
    "plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept", "merge",
    "release", "complete",
];

/// A plan with no activity for this long is visibly stalled, not "moving".
const STALL_SECONDS: i64 = 15 * 60;

/// The four build-progress states the legend renders. `Pending` is a fifth
/// "not started" state that is not part of the progress legend.
const LEGEND_STATES: [NodeState; 4] = [
    NodeState::Moving,
    NodeState::Stuck,
    NodeState::Blocked,
    NodeState::Done,
];

/// A plan's build-progress state — the color/shape axis (1.29 D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Moving,
    Stuck,
    Blocked,
    Done,
    Pending,
}

impl NodeState {
    pub fn label(self) -> &'static str {
        match self {
            NodeState::Moving => "moving",
            NodeState::Stuck => "stuck",
            NodeState::Blocked => "blocked",
            NodeState::Done => "done",
            NodeState::Pending => "pending",
        }
    }
}

/// The shape glyph a state renders as — the "glyph" the acceptance requires to
/// be distinct per state.
pub fn node_shape(state: NodeState) -> &'static str {
    match state {
        NodeState::Moving => "●",
        NodeState::Stuck => "▲",
        NodeState::Blocked => "◆",
        NodeState::Done => "■",
        NodeState::Pending => "○",
    }
}

/// The color a state renders as. Uses the theme's semantic colors (not fixed
/// hues) so it adapts to light/dark themes; each state maps to a distinct
/// semantic role.
pub fn node_color(state: NodeState) -> ui::Color {
    match state {
        NodeState::Moving => ui::Color::Success,
        NodeState::Stuck => ui::Color::Warning,
        NodeState::Blocked => ui::Color::Error,
        NodeState::Done => ui::Color::Info,
        NodeState::Pending => ui::Color::Muted,
    }
}

/// The legend: one entry per progress state, `(state, shape, label)`.
pub fn legend() -> Vec<(NodeState, &'static str, &'static str)> {
    LEGEND_STATES
        .iter()
        .map(|state| (*state, node_shape(*state), state.label()))
        .collect()
}

fn prerequisites(node: &str) -> &'static [&'static str] {
    match node {
        "plan" => &[],
        "promote" => &["plan"],
        "claim" => &["promote"],
        "fork" => &["claim"],
        "implement" => &["fork"],
        "verify" => &["implement"],
        "compile" => &["verify"],
        "accept" => &["compile"],
        "merge" => &["accept"],
        "release" => &["merge"],
        "complete" => &["release"],
        _ => &[],
    }
}

fn is_human_gate(node: &str) -> bool {
    matches!(node, "merge" | "release" | "complete")
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn next_ready(done: &HashSet<&str>) -> Option<&'static str> {
    ROUTE.iter().copied().find(|node| {
        !done.contains(node) && prerequisites(node).iter().all(|p| done.contains(p))
    })
}

/// Derive a plan's node state from its graph-state row + done nodes. Mirrors the
/// fleet panel's `derive_state`, reduced to the four progress states plus
/// `Pending` for a plan that is neither active nor blocked nor complete.
pub fn derive_node_state(
    plan: &FleetPlanRow,
    done_nodes: &[String],
    now: DateTime<Utc>,
) -> NodeState {
    if plan.current_node.as_deref() == Some("complete") {
        return NodeState::Done;
    }

    if plan.blocked_reason.is_some() {
        return NodeState::Blocked;
    }

    if let Some(active) = &plan.active_node {
        let idle = plan
            .heartbeat
            .as_deref()
            .and_then(parse_time)
            .map(|time| (now - time).num_seconds())
            .unwrap_or(i64::MAX);
        if idle > STALL_SECONDS {
            return NodeState::Stuck;
        }
        return NodeState::Moving;
    }

    let done: HashSet<&str> = done_nodes.iter().map(|node| node.as_str()).collect();
    match next_ready(&done) {
        Some(node) if is_human_gate(node) => NodeState::Blocked,
        Some(_) | None => NodeState::Pending,
    }
}

/// One plan node as the graph renders it.
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub state: NodeState,
    /// The current dev-flow node (the y-axis phase), or `—` when unset.
    pub phase: String,
    pub done_count: usize,
    pub breaker: String,
}

/// A `Blocked by` / `Related` edge between two plan nodes.
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

fn breaker_for(plan: &FleetPlanRow) -> String {
    if plan.current_node.as_deref() == Some("complete") {
        return "closed".to_string();
    }
    if plan.blocked_reason.is_some() {
        return "open".to_string();
    }
    if plan.active_node.is_some() {
        return "closed".to_string();
    }
    "half-open".to_string()
}

/// Project the store into graph nodes (plans) and edges (relations). This is a
/// pure read over the same `plan_store` rows the fleet/roadmap panels read.
pub fn project_graph(store: &PlanStore, now: DateTime<Utc>) -> (Vec<GraphNode>, Vec<EdgeRow>) {
    let plans = store.fleet_plans().unwrap_or_default();
    let edges = store.edges().unwrap_or_default();

    let nodes = plans
        .into_iter()
        .map(|plan| {
            let done_nodes = store.done_nodes(&plan.id).unwrap_or_default();
            let state = derive_node_state(&plan, &done_nodes, now);
            GraphNode {
                id: plan.id.clone(),
                name: plan.name.clone(),
                state,
                phase: plan.current_node.clone().unwrap_or_else(|| "—".to_string()),
                done_count: done_nodes.len(),
                breaker: breaker_for(&plan),
            }
        })
        .collect();

    (nodes, edges)
}

/// The hover summary (1.29 D7): name + status + current node + done count +
/// breaker state.
pub fn tooltip_text(node: &GraphNode) -> String {
    format!(
        "{} — {} (current: {}, {} done, breaker: {})",
        node.name, node.state.label(), node.phase, node.done_count, node.breaker
    )
}

/// One layer of the layout: a phase (route node) and the plans currently at it.
fn phase_order(phase: &str) -> usize {
    ROUTE.iter().position(|p| *p == phase).unwrap_or(ROUTE.len())
}

fn group_by_phase(nodes: &[GraphNode]) -> Vec<(String, Vec<&GraphNode>)> {
    let phases: BTreeSet<String> = nodes.iter().map(|node| node.phase.clone()).collect();
    let mut phases: Vec<String> = phases.into_iter().collect();
    phases.sort_by_key(|phase| (phase_order(phase), phase.clone()));

    phases
        .into_iter()
        .map(|phase| {
            let mut group: Vec<&GraphNode> = nodes
                .iter()
                .filter(|node| node.phase == phase)
                .collect();
            group.sort_by(|a, b| a.name.cmp(&b.name));
            (phase, group)
        })
        .collect()
}

/// The graph view: a workspace `Item` (main editor area, not a bottom `Panel`).
pub struct PlanGraph {
    focus_handle: FocusHandle,
    nodes: Vec<GraphNode>,
    edges: Vec<EdgeRow>,
}

impl PlanGraph {
    fn new(cx: &mut Context<Self>) -> Self {
        let store = PlanStore::open_default()
            .or_else(|_| PlanStore::open_in_memory())
            .expect("plan store (on-disk or in-memory)");
        let (nodes, edges) = project_graph(&store, Utc::now());
        Self {
            focus_handle: cx.focus_handle(),
            nodes,
            edges,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let store = PlanStore::open_default()
            .or_else(|_| PlanStore::open_in_memory())
            .expect("plan store (on-disk or in-memory)");
        let (nodes, edges) = project_graph(&store, Utc::now());
        self.nodes = nodes;
        self.edges = edges;
        cx.notify();
    }

    fn deploy(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        if let Some(existing) = workspace.items_of_type::<PlanGraph>(cx).next() {
            workspace.activate_item(&existing, true, true, window, cx);
        } else {
            let item = cx.new(|cx| PlanGraph::new(cx));
            workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
        }
    }

    fn render_legend() -> impl IntoElement {
        h_flex()
            .gap_3()
            .children(legend().into_iter().map(|(state, shape, label)| {
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(Label::new(shape).color(node_color(state)))
                    .child(Label::new(label).size(LabelSize::Small))
            }))
    }

    fn render_node(node: &GraphNode) -> impl IntoElement {
        h_flex()
            .gap_1()
            .items_center()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .child(Label::new(node_shape(node.state)).color(node_color(node.state)))
            .child(Label::new(node.name.clone()).size(LabelSize::Small))
    }
}

impl Focusable for PlanGraph {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for PlanGraph {}

impl Render for PlanGraph {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let groups = group_by_phase(&self.nodes);

        v_flex()
            .size_full()
            .p_4()
            .gap_4()
            .child(Label::new("Plan graph"))
            .child(Self::render_legend())
            .child(
                v_flex().gap_2().children(groups.into_iter().map(|(phase, nodes)| {
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(
                            Label::new(phase)
                                .size(LabelSize::Small)
                                .color(ui::Color::Muted)
                                .w_24(),
                        )
                        .child(h_flex().gap_1().flex_wrap().children(
                            nodes.into_iter().map(Self::render_node),
                        ))
                })),
            )
    }
}

impl Item for PlanGraph {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> ui::SharedString {
        "Plan graph".into()
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(
            |workspace: &mut Workspace, _: &DeployPlanGraph, window, cx| {
                PlanGraph::deploy(workspace, window, cx);
            },
        );
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        id: &str,
        current: Option<&str>,
        active: Option<&str>,
        heartbeat: Option<&str>,
        blocked: Option<&str>,
    ) -> FleetPlanRow {
        FleetPlanRow {
            id: id.to_string(),
            name: id.to_string(),
            repo: Some("zed".to_string()),
            current_node: current.map(str::to_string),
            active_node: active.map(str::to_string),
            holder: None,
            heartbeat: heartbeat.map(str::to_string),
            blocked_reason: blocked.map(str::to_string),
            spent_tokens: 0,
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-11-19T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn states_derive_from_graph_state() {
        let n = now();

        assert_eq!(
            derive_node_state(&row("D", Some("complete"), None, None, None), &[], n),
            NodeState::Done
        );
        assert_eq!(
            derive_node_state(
                &row("B", Some("accept"), None, None, Some("human gate")),
                &[],
                n
            ),
            NodeState::Blocked
        );
        assert_eq!(
            derive_node_state(
                &row(
                    "M",
                    Some("verify"),
                    Some("verify"),
                    Some("2026-11-19T11:59:00Z"),
                    None
                ),
                &[],
                n
            ),
            NodeState::Moving
        );
        assert_eq!(
            derive_node_state(
                &row(
                    "S",
                    Some("verify"),
                    Some("verify"),
                    Some("2026-11-19T10:00:00Z"),
                    None
                ),
                &[],
                n
            ),
            NodeState::Stuck
        );
        // Ready at a human gate (everything done through accept) blocks on you.
        assert_eq!(
            derive_node_state(
                &row("H", Some("merge"), None, None, None),
                &[
                    "plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept"
                ]
                .map(str::to_string),
                n
            ),
            NodeState::Blocked
        );
        assert_eq!(
            derive_node_state(&row("P", Some("implement"), None, None, None), &[], n),
            NodeState::Pending
        );
    }

    #[test]
    fn states_render_distinct_shape_and_color() {
        let states = [
            NodeState::Moving,
            NodeState::Stuck,
            NodeState::Blocked,
            NodeState::Done,
        ];

        let shapes: HashSet<&str> = states.iter().map(|state| node_shape(*state)).collect();
        assert_eq!(shapes.len(), states.len());

        let colors: HashSet<String> = states
            .iter()
            .map(|state| format!("{:?}", node_color(*state)))
            .collect();
        assert_eq!(colors.len(), states.len());
    }

    #[test]
    fn legend_lists_all_four_states() {
        let legend = legend();
        assert_eq!(legend.len(), 4);
        let shapes: HashSet<&str> = legend.iter().map(|(_, shape, _)| *shape).collect();
        assert_eq!(shapes.len(), 4);
        let labels: HashSet<&str> = legend.iter().map(|(_, _, label)| *label).collect();
        assert_eq!(labels.len(), 4);
    }

    #[test]
    fn tooltip_carries_the_summary() {
        let node = GraphNode {
            id: "P".to_string(),
            name: "MY_PLAN".to_string(),
            state: NodeState::Moving,
            phase: "verify".to_string(),
            done_count: 5,
            breaker: "closed".to_string(),
        };
        let text = tooltip_text(&node);
        assert!(text.contains("MY_PLAN"));
        assert!(text.contains("moving"));
        assert!(text.contains("verify"));
        assert!(text.contains("5 done"));
        assert!(text.contains("closed"));
    }

    #[test]
    fn project_graph_reads_nodes_and_edges_from_the_store() {
        let store = PlanStore::open_in_memory().expect("in-memory store");
        let mut snapshot = plan_store::GraphSnapshot {
            plan_id: "P".to_string(),
            name: "P".to_string(),
            repo: Some("zed".to_string()),
            current_node: Some("complete".to_string()),
            active_node: None,
            holder: None,
            heartbeat: None,
            blocked_reason: None,
            spent_tokens: 0,
            done_nodes: ROUTE.iter().map(|node| node.to_string()).collect(),
            evidence: vec![],
        };
        snapshot.evidence = vec![];
        store.apply_graph_snapshot(&snapshot).unwrap();
        store
            .upsert_edge(&plan_store::EdgeRow {
                from_plan: "P".to_string(),
                to_plan: "Q".to_string(),
                kind: "blocked_by".to_string(),
                order_rank: Some(1),
            })
            .unwrap();

        let (nodes, edges) = project_graph(&store, now());
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].state, NodeState::Done);
        assert_eq!(nodes[0].done_count, ROUTE.len());
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "blocked_by");
    }
}
