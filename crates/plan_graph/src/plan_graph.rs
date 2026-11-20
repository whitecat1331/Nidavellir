//! Interactive plan tree and plan page (1.29 re-scope).
//!
//! One main-editor-area `Item` renders the plan taxonomy as a collapsible
//! master–detail tree: `Plans` at the root, categories, subcategories, and
//! plan/slice leaves on the left, the plan page on the right. Progressive
//! disclosure keeps it navigable: a click expands a branch's children, a click
//! on a plan opens its page. The page carries the plan's metadata, summary, the
//! read-only dev-flow decision (from `plan_flow`, the same coordinator the agent
//! tool uses), related/blocking/slice chips, and a plan-scoped agent composer +
//! thread sidebar.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use agent_client_protocol::schema::v1 as acp;
use chrono::{DateTime, Utc};
use editor::Editor;
use gpui::{
    Action, AnyElement, App, ClickEvent, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, Render, SharedString, WeakEntity, Window, actions, px,
};
use plan_flow::{Attempt, DoneEntry, GraphState, ROUTE, STALL_SECONDS, is_human_gate, next_ready};
use plan_store::{
    CategorizedPlanRow, CategoryRow, EdgeRow, EvidenceRow, FleetPlanRow, Lifecycle, PlanParentRow,
    PlanStore,
};
use ui::{Button, Color, Label, LabelSize, prelude::*};
use workspace::Workspace;
use workspace::item::Item;

actions!(plan_graph, [DeployPlanGraph]);

/// Open a specific plan in the plan graph: reveals it in the tree and opens its
/// page. Dispatched by thread-to-plan navigation (D21) and plan-page chips.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = plan_graph)]
pub struct OpenPlan {
    pub plan_id: SharedString,
}

/// The debugger loop's `stage→…→fix` sub-graph (1.29 D8).
const DEBUGGER_SUB_GRAPH: [&str; 6] = ["stage", "test", "verify", "restart", "run", "fix"];

pub fn debugger_sub_graph(scoped: bool) -> &'static [&'static str] {
    if scoped { &DEBUGGER_SUB_GRAPH } else { &[] }
}

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

/// The shape glyph a state renders as.
pub fn node_shape(state: NodeState) -> &'static str {
    match state {
        NodeState::Moving => "●",
        NodeState::Stuck => "▲",
        NodeState::Blocked => "◆",
        NodeState::Done => "■",
        NodeState::Pending => "○",
    }
}

/// The color a state renders as.
pub fn node_color(state: NodeState) -> Color {
    match state {
        NodeState::Moving => Color::Success,
        NodeState::Stuck => Color::Warning,
        NodeState::Blocked => Color::Error,
        NodeState::Done => Color::Info,
        NodeState::Pending => Color::Muted,
    }
}

/// The legend: one entry per progress state, `(state, shape, label)`.
pub fn legend() -> Vec<(NodeState, &'static str, &'static str)> {
    LEGEND_STATES
        .iter()
        .map(|state| (*state, node_shape(*state), state.label()))
        .collect()
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

/// Derive a plan's node state from its graph-state row + done nodes.
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
    if let Some(_active) = &plan.active_node {
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

/// One plan node as the flat graph projection renders it (kept for tests and
/// the fleet-style read path).
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub state: NodeState,
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

/// Project the store into flat graph nodes and edges (the projection tests use
/// this; the wheel reads `categorized_plans` instead).
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

/// The hover summary (1.29 D7).
pub fn tooltip_text(node: &GraphNode) -> String {
    format!(
        "{} — {} (current: {}, {} done, breaker: {})",
        node.name,
        node.state.label(),
        node.phase,
        node.done_count,
        node.breaker
    )
}

/// One node of a plan's dev-flow.
pub struct DevFlowNode {
    pub node: String,
    pub done: bool,
    /// A done node is `complete` only when its evidence is present.
    pub complete: bool,
    pub is_human_gate: bool,
    pub evidence: Option<EvidenceRow>,
}

/// A done node's evidence is complete iff the falsifying condition, the
/// negative-control flag, and the artifact are all present (1.28 D2).
pub fn evidence_complete(evidence: &EvidenceRow) -> bool {
    evidence
        .artifact_sha
        .as_deref()
        .is_some_and(|sha| !sha.is_empty())
        && evidence
            .falsifying_condition
            .as_deref()
            .is_some_and(|condition| !condition.is_empty())
        && evidence.negative_control.is_some()
}

/// Project one plan's 11-node dev-flow from the store.
pub fn project_dev_flow(store: &PlanStore, plan_id: &str) -> Vec<DevFlowNode> {
    let done = store.done_nodes(plan_id).unwrap_or_default();
    let evidence = store.evidence_for(plan_id).unwrap_or_default();
    dev_flow_from_rows(&done, evidence)
}

fn dev_flow_from_rows(done: &[String], evidence: Vec<EvidenceRow>) -> Vec<DevFlowNode> {
    let done_set: HashSet<&str> = done.iter().map(|node| node.as_str()).collect();
    let evidence_by_node: HashMap<String, EvidenceRow> = evidence
        .into_iter()
        .map(|row| (row.node.clone(), row))
        .collect();

    ROUTE
        .iter()
        .map(|&node| {
            let done = done_set.contains(node);
            let evidence = evidence_by_node.get(node).cloned();
            let complete = done && evidence.as_ref().is_some_and(evidence_complete);
            DevFlowNode {
                node: node.to_string(),
                done,
                complete,
                is_human_gate: is_human_gate(node),
                evidence,
            }
        })
        .collect()
}

/// The glyph + color a dev-flow node renders as.
pub fn dev_flow_visual(node: &DevFlowNode) -> (&'static str, Color) {
    if node.done && node.complete {
        ("●", Color::Success)
    } else if node.done {
        ("◐", Color::Warning)
    } else if node.is_human_gate {
        ("⏸", Color::Error)
    } else {
        ("○", Color::Muted)
    }
}

/// The coordinator's read-only decision for a plan (1.29 D2, Option B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionKind {
    Active,
    Blocked,
    Retry,
    Next,
    Paused,
    Done,
}

pub struct Decision {
    pub kind: DecisionKind,
    pub node: String,
    pub reason: String,
}

/// Project the coordinator's decision order as a read-only view over the store.
pub fn project_decision(store: &PlanStore, plan_id: &str) -> Decision {
    let plan = match store.plan(plan_id) {
        Ok(Some(plan)) => plan,
        Ok(None) => {
            return Decision {
                kind: DecisionKind::Paused,
                node: "—".to_string(),
                reason: "plan not found".to_string(),
            };
        }
        Err(error) => {
            log::error!("[GRAPH] failed to read plan {plan_id}: {error:#}");
            return Decision {
                kind: DecisionKind::Paused,
                node: "—".to_string(),
                reason: "plan store unavailable".to_string(),
            };
        }
    };

    if plan.current_node.as_deref() == Some("complete") {
        return Decision {
            kind: DecisionKind::Done,
            node: "complete".to_string(),
            reason: "plan complete".to_string(),
        };
    }
    if let Some(reason) = &plan.blocked_reason {
        return Decision {
            kind: DecisionKind::Blocked,
            node: plan.current_node.clone().unwrap_or_default(),
            reason: format!("blocked — {reason}"),
        };
    }

    // The store's row is a projection of `progress.json`; rebuild just enough of
    // the graph state for `plan_flow::coordinate` to decide with the same engine
    // the agent tool runs. The page never mutates the plan (D2).
    let evidence = store.evidence_for(plan_id).unwrap_or_default();
    let failed: HashSet<&str> = evidence
        .iter()
        .filter(|row| row.gate_exit_code == Some(1))
        .map(|row| row.node.as_str())
        .collect();
    let done = store
        .done_nodes(plan_id)
        .unwrap_or_default()
        .into_iter()
        .map(|node| {
            let failed = failed.contains(node.as_str());
            DoneEntry {
                node,
                artifact: String::new(),
                at: String::new(),
                failed,
                attempts: vec![Attempt {
                    at: String::new(),
                    artifact: String::new(),
                    failed,
                    failure: None,
                    cost: None,
                    warnings: Vec::new(),
                }],
                cost: None,
            }
        })
        .collect();
    let active = plan.active_node.map(|node| plan_flow::ActiveNode {
        node,
        executor: plan.holder.unwrap_or_default(),
        started_at: plan.heartbeat.clone().unwrap_or_default(),
        last_activity_at: plan.heartbeat.unwrap_or_default(),
    });
    let mut state = GraphState {
        plan: plan.id,
        repo: plan.repo.unwrap_or_default(),
        scope: None,
        current: plan.current_node,
        done,
        next: Vec::new(),
        blocker: None,
        budget: None,
        active,
        back_edges: Vec::new(),
        warnings: Vec::new(),
        authorized: None,
    };

    let retrying = plan_flow::retry_candidate(&state);
    let decision = plan_flow::coordinate(&mut state, false, Utc::now());
    let node = decision.node.unwrap_or_else(|| "—".to_string());
    let kind = match decision.action.as_str() {
        "continue" => DecisionKind::Active,
        "dispatch" if retrying.as_deref() == Some(node.as_str()) => DecisionKind::Retry,
        "dispatch" => DecisionKind::Next,
        "pause" if decision.predicate.as_deref() == Some("human_gate") => DecisionKind::Blocked,
        _ => DecisionKind::Paused,
    };
    Decision {
        kind,
        node,
        reason: decision.reason,
    }
}

pub fn decision_text(decision: &Decision) -> String {
    match decision.kind {
        DecisionKind::Done => "Done — plan complete".to_string(),
        DecisionKind::Active => format!("{} — active", decision.node),
        DecisionKind::Blocked => format!("⏸ blocked at {} — {}", decision.node, decision.reason),
        DecisionKind::Retry => format!("retry {} — {}", decision.node, decision.reason),
        DecisionKind::Next => format!("next step: {}", decision.node),
        DecisionKind::Paused => format!("paused — {}", decision.reason),
    }
}

// ---------------------------------------------------------------------------
// Wheel model (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKind {
    Root,
    History,
    Category,
    Subcategory,
    Plan,
    SliceGroup,
    Slice,
}

/// A visible wheel node before layout. `expandable` is what a click toggles;
/// `children` is populated only when the node is expanded.
#[derive(Debug, Clone, PartialEq)]
pub struct VisibleNode {
    pub key: String,
    pub label: String,
    pub kind: TreeKind,
    pub plan_id: Option<String>,
    pub state: NodeState,
    pub expanded: bool,
    pub expandable: bool,
    pub slice_count: usize,
    pub children: Vec<VisibleNode>,
}

impl VisibleNode {
    fn new(key: String, label: String, kind: TreeKind) -> Self {
        Self {
            key,
            label,
            kind,
            plan_id: None,
            state: NodeState::Pending,
            expanded: false,
            expandable: false,
            slice_count: 0,
            children: Vec::new(),
        }
    }
}

fn short_label(text: &str) -> String {
    let text = text.trim_end_matches(".md");
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 46 {
        collapse_plan_prefix(&collapsed)
    } else {
        let truncated: String = collapsed.chars().take(45).collect();
        format!("{}…", collapse_plan_prefix(&truncated))
    }
}

fn collapse_plan_prefix(text: &str) -> String {
    text.trim_start_matches("PLAN_OF_ACTION-")
        .trim_start_matches("plan-of-action-")
        .to_string()
}

fn plan_label(plan: &CategorizedPlanRow) -> String {
    short_label(&plan.name)
}

fn plan_state(plan: &CategorizedPlanRow, fleet: Option<&FleetPlanRow>) -> NodeState {
    if plan.lifecycle() == Lifecycle::History {
        return NodeState::Done;
    }
    let Some(fleet) = fleet else {
        return NodeState::Pending;
    };
    if fleet.blocked_reason.is_some() {
        return NodeState::Blocked;
    }
    if fleet.active_node.is_some() {
        return NodeState::Moving;
    }
    if fleet.current_node.as_deref() == Some("complete") {
        return NodeState::Done;
    }
    NodeState::Pending
}

fn slices_for<'a>(parents: &'a [PlanParentRow], plan_id: &str) -> Vec<&'a str> {
    parents
        .iter()
        .filter(|parent| parent.parent_id == plan_id)
        .map(|parent| parent.plan_id.as_str())
        .collect()
}

/// Build the visible wheel tree from the taxonomy, assignments, parent links
/// and expansion state. Pure so the disclosure rules are testable.
pub fn build_visible_tree(
    categories: &[CategoryRow],
    plans: &[CategorizedPlanRow],
    parents: &[PlanParentRow],
    fleet: &HashMap<String, FleetPlanRow>,
    expanded: &HashSet<String>,
    history_visible: bool,
) -> VisibleNode {
    let plan_by_id: HashMap<&str, &CategorizedPlanRow> =
        plans.iter().map(|plan| (plan.id.as_str(), plan)).collect();
    let live: Vec<&CategorizedPlanRow> = plans
        .iter()
        .filter(|plan| plan.lifecycle() == Lifecycle::Live)
        .collect();
    let history: Vec<&CategorizedPlanRow> = plans
        .iter()
        .filter(|plan| plan.lifecycle() == Lifecycle::History)
        .collect();

    let mut top_categories: Vec<&CategoryRow> = categories
        .iter()
        .filter(|category| category.parent_id.is_none())
        .collect();
    top_categories.sort_by_key(|category| (category.rank, category.name.clone()));

    let mut root_children = Vec::new();
    for category in top_categories {
        let mut subs: Vec<&CategoryRow> = categories
            .iter()
            .filter(|sub| sub.parent_id.as_deref() == Some(category.id.as_str()))
            .collect();
        subs.sort_by_key(|sub| (sub.rank, sub.name.clone()));

        let mut cat_children = Vec::new();
        let mut category_live_count = 0;
        for sub in subs {
            let mut members: Vec<&CategorizedPlanRow> = live
                .iter()
                .filter(|plan| plan.category_id.as_deref() == Some(sub.id.as_str()))
                .copied()
                .collect();
            members.sort_by(|a, b| a.name.cmp(&b.name));
            category_live_count += members.len();

            let mut sub_children = Vec::new();
            for plan in members {
                let slices = slices_for(parents, &plan.id);
                let plan_key = format!("plan:{}", plan.id);
                let plan_expanded = expanded.contains(&plan_key);
                let mut children = Vec::new();
                if plan_expanded && !slices.is_empty() {
                    let group_key = format!("slices:{}", plan.id);
                    let group_expanded = expanded.contains(&group_key);
                    let mut slice_children = Vec::new();
                    if group_expanded {
                        for slice_id in &slices {
                            if let Some(slice) = plan_by_id.get(slice_id) {
                                let mut node = VisibleNode::new(
                                    format!("slice:{slice_id}"),
                                    plan_label(slice),
                                    TreeKind::Slice,
                                );
                                node.plan_id = Some((*slice_id).to_string());
                                node.state = plan_state(slice, fleet.get(*slice_id));
                                slice_children.push(node);
                            }
                        }
                    }
                    let mut group = VisibleNode::new(
                        group_key,
                        format!("slices ({})", slices.len()),
                        TreeKind::SliceGroup,
                    );
                    group.plan_id = Some(plan.id.clone());
                    group.expandable = !slice_children.is_empty();
                    group.expanded = group_expanded;
                    group.slice_count = slices.len();
                    group.children = slice_children;
                    children.push(group);
                }

                let mut node = VisibleNode::new(plan_key, plan_label(plan), TreeKind::Plan);
                node.plan_id = Some(plan.id.clone());
                node.state = plan_state(plan, fleet.get(plan.id.as_str()));
                node.expandable = !slices.is_empty();
                node.expanded = plan_expanded;
                node.slice_count = slices.len();
                node.children = if plan_expanded { children } else { Vec::new() };
                sub_children.push(node);
            }

            let sub_key = format!("sub:{}", sub.id);
            let sub_expanded = expanded.contains(&sub_key);
            let mut node = VisibleNode::new(
                sub_key,
                format!("{} · {}", sub.name, sub_children.len()),
                TreeKind::Subcategory,
            );
            node.expandable = !sub_children.is_empty();
            node.expanded = sub_expanded;
            node.children = if sub_expanded {
                sub_children
            } else {
                Vec::new()
            };
            cat_children.push(node);
        }

        let cat_key = format!("cat:{}", category.id);
        let cat_expanded = expanded.contains(&cat_key);
        let mut node = VisibleNode::new(
            cat_key,
            format!("{} · {}", category.name, category_live_count),
            TreeKind::Category,
        );
        node.expandable = !cat_children.is_empty();
        node.expanded = cat_expanded;
        node.children = if cat_expanded {
            cat_children
        } else {
            Vec::new()
        };
        root_children.push(node);
    }

    if history_visible {
        let key = "history".to_string();
        let is_expanded = expanded.contains(&key);
        let mut history_nodes: Vec<VisibleNode> = history
            .iter()
            .map(|plan| {
                let mut node = VisibleNode::new(
                    format!("hist:{}", plan.id),
                    plan_label(plan),
                    TreeKind::Plan,
                );
                node.plan_id = Some(plan.id.clone());
                node.state = NodeState::Done;
                node
            })
            .collect();
        history_nodes.sort_by(|a, b| a.label.cmp(&b.label));
        let mut node = VisibleNode::new(
            key,
            format!("History · {}", history_nodes.len()),
            TreeKind::History,
        );
        node.state = NodeState::Done;
        node.expandable = !history_nodes.is_empty();
        node.expanded = is_expanded;
        node.children = if is_expanded {
            history_nodes
        } else {
            Vec::new()
        };
        root_children.push(node);
    }

    let mut root = VisibleNode::new("root".to_string(), "Plans".to_string(), TreeKind::Root);
    let root_expanded = expanded.contains("root");
    root.expandable = true;
    root.expanded = root_expanded;
    root.children = if root_expanded {
        root_children
    } else {
        Vec::new()
    };
    root
}

/// Strip directories and plan-file suffixes from a thread's `plan_path` and
/// normalize it to the store's plan id, so a graph-tool tag and a plan-page tag
/// match the same plan.
pub fn plan_id_from_path(path: &str) -> String {
    let name = path
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(path);
    let mut stem = name;
    for suffix in [".json", ".progress", ".md"] {
        stem = stem.strip_suffix(suffix).unwrap_or(stem);
    }
    stem.to_ascii_lowercase().replace('_', "-")
}

/// Reveal a plan in the tree by expanding the `root` to category to subcategory
/// path that leads to it (or, for a slice, its parent plan's `slices` group).
fn expand_path_to_plan(
    expanded: &mut HashSet<String>,
    categories: &[CategoryRow],
    plans: &[CategorizedPlanRow],
    parents: &[PlanParentRow],
    plan_id: &str,
) {
    expanded.insert("root".to_string());
    let Some(plan) = plans.iter().find(|plan| plan.id == plan_id) else {
        return;
    };

    let mut category_id = plan.category_id.clone();
    if category_id.is_none() {
        if let Some(parent) = parents.iter().find(|parent| parent.plan_id == plan_id) {
            expanded.insert(format!("plan:{}", parent.parent_id));
            expanded.insert(format!("slices:{}", parent.parent_id));
            category_id = plans
                .iter()
                .find(|plan| plan.id == parent.parent_id)
                .and_then(|plan| plan.category_id.clone());
        }
    }

    while let Some(id) = category_id {
        let Some(category) = categories.iter().find(|category| category.id == id) else {
            break;
        };
        let key = if category.parent_id.is_none() {
            format!("cat:{}", category.id)
        } else {
            format!("sub:{}", category.id)
        };
        expanded.insert(key);
        category_id = category.parent_id.clone();
    }
}

fn toggle(expanded: &mut HashSet<String>, key: &str) {
    if !expanded.remove(key) {
        expanded.insert(key.to_string());
    }
}

// ---------------------------------------------------------------------------
// The Item
// ---------------------------------------------------------------------------

struct RelationChip {
    id: String,
    label: String,
    incoming: bool,
}

struct PageData {
    id: String,
    name: String,
    repo: Option<String>,
    stage: Option<String>,
    status: Option<String>,
    current_node: Option<String>,
    lifecycle: Lifecycle,
    summary: Option<String>,
    detail: Option<String>,
    decision: Decision,
    related: Vec<RelationChip>,
    blocking: Vec<RelationChip>,
    slices: Vec<RelationChip>,
    dev_flow: Vec<DevFlowNode>,
    debugger_scoped: bool,
}

struct PlanThread {
    session_id: acp::SessionId,
    title: String,
    updated_at: DateTime<Utc>,
}

/// The plan navigation `Item` (main editor area, not a bottom `Panel`).
pub struct PlanGraph {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    categories: Vec<CategoryRow>,
    plans: Vec<CategorizedPlanRow>,
    parents: Vec<PlanParentRow>,
    fleet: HashMap<String, FleetPlanRow>,
    edges: Vec<EdgeRow>,
    expanded: HashSet<String>,
    history_visible: bool,
    page_stack: Vec<String>,
    page: Option<PageData>,
    composer: gpui::Entity<Editor>,
    threads: Vec<PlanThread>,
}

impl PlanGraph {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Ask the agent about this plan…", window, cx);
            editor
        });
        let mut graph = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            categories: Vec::new(),
            plans: Vec::new(),
            parents: Vec::new(),
            fleet: HashMap::new(),
            edges: Vec::new(),
            expanded: HashSet::new(),
            history_visible: false,
            page_stack: Vec::new(),
            page: None,
            composer,
            threads: Vec::new(),
        };
        graph.refresh();
        graph
    }

    fn open_store() -> PlanStore {
        PlanStore::open_default()
            .or_else(|_| PlanStore::open_in_memory())
            .expect("plan store (on-disk or in-memory)")
    }

    fn refresh(&mut self) {
        let store = Self::open_store();
        self.categories = store.categories().unwrap_or_else(|error| {
            log::error!("[GRAPH] failed to read categories: {error:#}");
            Vec::new()
        });
        self.plans = store.categorized_plans().unwrap_or_else(|error| {
            log::error!("[GRAPH] failed to read categorized plans: {error:#}");
            Vec::new()
        });
        self.parents = store.plan_parents().unwrap_or_else(|error| {
            log::error!("[GRAPH] failed to read plan parents: {error:#}");
            Vec::new()
        });
        self.edges = store.edges().unwrap_or_else(|error| {
            log::error!("[GRAPH] failed to read edges: {error:#}");
            Vec::new()
        });
        self.fleet = store
            .fleet_plans()
            .unwrap_or_else(|error| {
                log::error!("[GRAPH] failed to read fleet plans: {error:#}");
                Vec::new()
            })
            .into_iter()
            .map(|plan| (plan.id.clone(), plan))
            .collect();
    }

    fn visible_tree(&self) -> VisibleNode {
        build_visible_tree(
            &self.categories,
            &self.plans,
            &self.parents,
            &self.fleet,
            &self.expanded,
            self.history_visible,
        )
    }

    fn plan_display(&self, plan_id: &str) -> String {
        self.plans
            .iter()
            .find(|plan| plan.id == plan_id)
            .map(plan_label)
            .unwrap_or_else(|| short_label(plan_id))
    }

    fn open_plan(&mut self, plan_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.navigate_to_plan(&plan_id, cx);
        self.composer
            .update(cx, |editor, cx| editor.clear(window, cx));
    }

    fn navigate_to_plan(&mut self, plan_id: &str, cx: &mut Context<Self>) {
        expand_path_to_plan(
            &mut self.expanded,
            &self.categories,
            &self.plans,
            &self.parents,
            plan_id,
        );
        self.page_stack.push(plan_id.to_string());
        self.load_page(plan_id);
        self.threads.clear();
        self.load_threads(cx);
        cx.notify();
    }

    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.page_stack.pop();
        match self.page_stack.last().cloned() {
            Some(plan_id) => {
                self.load_page(&plan_id);
                self.threads.clear();
                self.load_threads(cx);
            }
            None => {
                self.page = None;
                self.threads.clear();
            }
        }
        let _ = window;
        cx.notify();
    }

    fn close_page(&mut self, cx: &mut Context<Self>) {
        if self.page_stack.is_empty() {
            return;
        }
        self.page_stack.clear();
        self.page = None;
        self.threads.clear();
        cx.notify();
    }

    fn load_page(&mut self, plan_id: &str) {
        let store = Self::open_store();
        let Some(plan) = self.plans.iter().find(|plan| plan.id == plan_id).cloned() else {
            return;
        };

        let mut related = Vec::new();
        let mut blocking = Vec::new();
        for edge in &self.edges {
            let (other, incoming) = if edge.from_plan == plan_id {
                (&edge.to_plan, false)
            } else if edge.to_plan == plan_id {
                (&edge.from_plan, true)
            } else {
                continue;
            };
            let chip = RelationChip {
                id: other.clone(),
                label: self.plan_display(other),
                incoming,
            };
            match edge.kind.as_str() {
                "blocked_by" => blocking.push(chip),
                "related" => related.push(chip),
                _ => {}
            }
        }
        related.sort_by(|a, b| a.label.cmp(&b.label));
        blocking.sort_by(|a, b| a.label.cmp(&b.label));

        let slices = self
            .parents
            .iter()
            .filter(|parent| parent.parent_id == plan_id)
            .map(|parent| RelationChip {
                id: parent.plan_id.clone(),
                label: self.plan_display(&parent.plan_id),
                incoming: false,
            })
            .collect();

        let done = store.done_nodes(plan_id).unwrap_or_default();
        let evidence = store.evidence_for(plan_id).unwrap_or_default();
        self.page = Some(PageData {
            id: plan.id.clone(),
            name: plan.name.clone(),
            repo: plan.repo.clone(),
            stage: plan.stage.clone(),
            status: plan.status.clone(),
            current_node: plan.current_node.clone(),
            lifecycle: plan.lifecycle(),
            summary: plan.summary.clone(),
            detail: plan.detail,
            decision: project_decision(&store, plan_id),
            related,
            blocking,
            slices,
            dev_flow: dev_flow_from_rows(&done, evidence),
            debugger_scoped: store.debugger_scoped(plan_id).unwrap_or(false),
        });
    }

    fn current_plan_id(&self) -> Option<String> {
        self.page.as_ref().map(|page| page.id.clone())
    }

    fn load_threads(&mut self, cx: &mut Context<Self>) {
        let Some(plan_id) = self.current_plan_id() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let database = cx.update(|cx| agent::ThreadsDatabase::connect(cx)).await;
            let database = match database {
                Ok(database) => database,
                Err(error) => {
                    log::error!("[GRAPH] thread database unavailable: {error:#}");
                    return;
                }
            };
            let threads = match database.list_threads().await {
                Ok(threads) => threads,
                Err(error) => {
                    log::error!("[GRAPH] failed to list threads: {error:#}");
                    return;
                }
            };
            let scoped: Vec<PlanThread> = threads
                .into_iter()
                .filter(|thread| {
                    thread
                        .plan_path
                        .as_deref()
                        .map(plan_id_from_path)
                        .as_deref()
                        == Some(plan_id.as_str())
                })
                .map(|thread| PlanThread {
                    session_id: thread.id,
                    title: thread.title.to_string(),
                    updated_at: thread.updated_at,
                })
                .collect();
            if let Err(error) = this.update(cx, |this, cx| {
                if this.current_plan_id().as_deref() == Some(plan_id.as_str()) {
                    this.threads = scoped;
                    cx.notify();
                }
            }) {
                log::debug!("[GRAPH] plan page closed before threads loaded: {error:#}");
            }
        })
        .detach();
    }

    fn send_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(plan_id) = self.current_plan_id() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let panel = workspace.update(cx, |workspace, cx| {
            workspace.focus_panel::<agent_ui::AgentPanel>(window, cx)
        });
        let Some(panel) = panel else {
            return;
        };
        panel.update(cx, |panel, cx| {
            panel.create_thread_with_options(
                agent_ui::CreateThreadOptions {
                    initial_content: Some(agent_ui::AgentInitialContent::ContentBlock {
                        blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                        auto_submit: true,
                    }),
                    focus: true,
                    plan_path: Some(plan_id),
                    ..Default::default()
                },
                agent_ui::AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        });
        self.composer
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.load_threads(cx);
        // The new thread is persisted asynchronously; reload once the save has
        // landed so it appears in this plan's sidebar without reopening the page.
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(900))
                .await;
            if let Err(error) = this.update(cx, |this, cx| this.load_threads(cx)) {
                log::debug!("[GRAPH] plan page closed before thread refresh: {error:#}");
            }
        })
        .detach();
        cx.notify();
    }

    fn open_thread(
        &mut self,
        session_id: acp::SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let panel = workspace.update(cx, |workspace, cx| {
            workspace.focus_panel::<agent_ui::AgentPanel>(window, cx)
        });
        if let Some(panel) = panel {
            panel.update(cx, |panel, cx| {
                panel.open_thread(session_id, None, None, window, cx);
            });
        }
    }

    fn toggle_history(&mut self, cx: &mut Context<Self>) {
        self.history_visible = !self.history_visible;
        cx.notify();
    }

    fn deploy(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) -> Entity<Self> {
        let existing = workspace.items_of_type::<PlanGraph>(cx).next();
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            existing
        } else {
            let workspace_handle = cx.entity().downgrade();
            let item = cx.new(|cx| PlanGraph::new(workspace_handle, window, cx));
            workspace.add_item_to_active_pane(Box::new(item.clone()), None, true, window, cx);
            item
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .items_center()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(format!(
                    "Plans · {} live",
                    self.plans
                        .iter()
                        .filter(|plan| plan.lifecycle() == Lifecycle::Live)
                        .count()
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(Button::new("tree-history", "History").on_click(
                cx.listener(|this: &mut PlanGraph, _: &ClickEvent, _, cx| this.toggle_history(cx)),
            ))
            .child(
                Label::new("click a branch to expand · click a plan to open it")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }

    fn click_node(&mut self, node: &VisibleNode, window: &mut Window, cx: &mut Context<Self>) {
        match node.kind {
            TreeKind::Root => {
                let expanding = !self.expanded.contains("root");
                if expanding {
                    self.expanded.insert("root".to_string());
                } else {
                    self.expanded.clear();
                }
            }
            TreeKind::Category
            | TreeKind::Subcategory
            | TreeKind::History
            | TreeKind::SliceGroup => {
                toggle(&mut self.expanded, &node.key);
            }
            TreeKind::Plan => {
                if node.expandable {
                    toggle(&mut self.expanded, &node.key);
                } else if let Some(plan_id) = node.plan_id.clone() {
                    self.open_plan(plan_id, window, cx);
                    return;
                }
            }
            TreeKind::Slice => {
                if let Some(plan_id) = node.plan_id.clone() {
                    self.open_plan(plan_id, window, cx);
                    return;
                }
            }
        }
        cx.notify();
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let root = self.visible_tree();
        v_flex()
            .id(ElementId::Name("plan-tree".into()))
            .flex_1()
            .overflow_y_scroll()
            .child(self.render_tree_row(&root, 0, cx))
    }

    fn render_tree_row(
        &self,
        node: &VisibleNode,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = node.key.clone();
        let expandable = node.expandable;
        let state = node.state;
        let selected = self
            .page
            .as_ref()
            .map(|page| page.id.as_str())
            .is_some_and(|id| node.plan_id.as_deref() == Some(id));
        let glyph = node_shape(state);
        let glyph_color = node_color(state);

        let click_node = node.clone();
        let row = div()
            .id(ElementId::Name(format!("tree-node:{key}").into()))
            .pl(px((depth as f32 * 14.0) + 8.0))
            .pr_2()
            .py_1()
            .flex()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .rounded_md()
            .when(selected, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .on_click(
                cx.listener(move |this: &mut PlanGraph, _: &ClickEvent, window, cx| {
                    this.click_node(&click_node, window, cx);
                }),
            )
            .child(
                Label::new(if expandable {
                    if node.expanded { "▾" } else { "▸" }
                } else {
                    "·"
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(Label::new(glyph).size(LabelSize::XSmall).color(glyph_color))
            .child(Label::new(node.label.clone()).size(LabelSize::Small))
            .when(node.slice_count > 0, |this| {
                this.child(
                    Label::new(format!("slices ({})", node.slice_count))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            });

        if node.expanded && !node.children.is_empty() {
            v_flex()
                .child(row)
                .children(
                    node.children
                        .iter()
                        .map(|child| self.render_tree_row(child, depth + 1, cx)),
                )
                .into_any_element()
        } else {
            row.into_any_element()
        }
    }

    fn render_detail(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.page.is_some() {
            self.render_page(window, cx)
        } else {
            v_flex()
                .id(ElementId::Name("plan-empty".into()))
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    Label::new("Select a plan to see its detail.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        }
    }

    fn render_page(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(page) = self.page.as_ref() else {
            return div().into_any_element();
        };
        let can_go_back = self.page_stack.len() > 1;
        let title = short_label(&page.name);
        let summary = page
            .detail
            .clone()
            .or_else(|| page.summary.clone())
            .unwrap_or_else(|| "No summary recorded for this plan.".to_string());
        let decision_color = match page.decision.kind {
            DecisionKind::Done => Color::Info,
            DecisionKind::Active => Color::Success,
            DecisionKind::Blocked => Color::Error,
            DecisionKind::Retry => Color::Warning,
            DecisionKind::Next => Color::Accent,
            DecisionKind::Paused => Color::Muted,
        };
        let has_relations =
            !page.related.is_empty() || !page.blocking.is_empty() || !page.slices.is_empty();

        v_flex()
            .id(ElementId::Name("plan-page".into()))
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(980.0))
                    .p_8()
                    .gap_6()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_4()
                            .items_start()
                            .justify_between()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_3()
                                    .child(
                                        h_flex()
                                            .gap_3()
                                            .items_center()
                                            .child(Label::new(title).size(LabelSize::Large))
                                            .child(badge(
                                                if page.lifecycle == Lifecycle::History {
                                                    "history"
                                                } else {
                                                    "live"
                                                },
                                                if page.lifecycle == Lifecycle::History {
                                                    Color::Muted
                                                } else {
                                                    Color::Success
                                                },
                                                cx,
                                            )),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .flex_wrap()
                                            .child(meta_chip("id", &page.id, cx))
                                            .when_some(page.repo.clone(), |this, repo| {
                                                this.child(meta_chip("repo", &repo, cx))
                                            })
                                            .when_some(page.stage.clone(), |this, stage| {
                                                this.child(meta_chip("stage", &stage, cx))
                                            })
                                            .when_some(page.status.clone(), |this, status| {
                                                this.child(meta_chip("status", &status, cx))
                                            })
                                            .when_some(page.current_node.clone(), |this, node| {
                                                this.child(meta_chip("current", &node, cx))
                                            }),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .when(can_go_back, |this| {
                                        this.child(Button::new("page-back", "← Back").on_click(
                                            cx.listener(
                                                |this: &mut PlanGraph,
                                                 _: &ClickEvent,
                                                 window,
                                                 cx| { this.back(window, cx) },
                                            ),
                                        ))
                                    })
                                    .child(Button::new("page-close", "Close").on_click(
                                        cx.listener(|this: &mut PlanGraph, _: &ClickEvent, _, cx| {
                                            this.close_page(cx)
                                        }),
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .bg(cx.theme().colors().element_background)
                            .px_4()
                            .py_3()
                            .child(
                                h_flex()
                                    .gap_3()
                                    .items_center()
                                    .child(
                                        Label::new("Decision")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(decision_text(&page.decision))
                                            .color(decision_color),
                                    ),
                            ),
                    )
                    .child(section(
                        "Summary",
                        div()
                            .w_full()
                            .child(Label::new(summary).color(Color::Default))
                            .into_any_element(),
                    ))
                    .when(has_relations, |this| {
                        this.child(section(
                            "Relationships",
                            v_flex()
                                .gap_4()
                                .when(!page.related.is_empty(), |this| {
                                    this.child(chip_row("Related", &page.related, cx))
                                })
                                .when(!page.blocking.is_empty(), |this| {
                                    this.child(chip_row("Blocked by", &page.blocking, cx))
                                })
                                .when(!page.slices.is_empty(), |this| {
                                    this.child(chip_row("Slices", &page.slices, cx))
                                })
                                .into_any_element(),
                        ))
                    })
                    .child(section(
                        "Dev flow",
                        v_flex()
                            .gap_3()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .flex_wrap()
                                    .children(page.dev_flow.iter().map(|node| flow_pill(node, cx))),
                            )
                            .when(page.debugger_scoped, |this| {
                                this.child(
                                    Label::new(
                                        "Debugger loop (stage → test → verify → restart → run → fix)",
                                    )
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                )
                            })
                            .into_any_element(),
                    ))
                    .child(section(
                        "Threads for this plan",
                        v_flex()
                            .gap_2()
                            .when(self.threads.is_empty(), |this| {
                                this.child(
                                    Label::new("No threads started from this plan yet.")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            })
                            .children(self.threads.iter().map(|thread| {
                                let session = thread.session_id.clone();
                                h_flex()
                                    .id(ElementId::Name(
                                        format!("plan-thread:{}", thread.session_id).into(),
                                    ))
                                    .w_full()
                                    .gap_3()
                                    .items_center()
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .hover(|this| this.bg(cx.theme().colors().element_hover))
                                    .on_click(cx.listener(
                                        move |this: &mut PlanGraph,
                                              _: &ClickEvent,
                                              window,
                                              cx| {
                                            this.open_thread(session.clone(), window, cx)
                                        },
                                    ))
                                    .child(Label::new(thread.title.clone()).size(LabelSize::Small))
                                    .child(
                                        Label::new(
                                            thread
                                                .updated_at
                                                .format("%Y-%m-%d %H:%M")
                                                .to_string(),
                                        )
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                    )
                            }))
                            .into_any_element(),
                    ))
                    .child(
                        v_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                Label::new("Ask the agent about this plan")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_1()
                                            .px_3()
                                            .py_2()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(cx.theme().colors().border_variant)
                                            .bg(cx.theme().colors().editor_background)
                                            .child(self.composer.clone()),
                                    )
                                    .child(Button::new("plan-send", "Send").on_click(cx.listener(
                                        |this: &mut PlanGraph, _: &ClickEvent, window, cx| {
                                            this.send_composer(window, cx)
                                        },
                                    ))),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn section(title: &str, body: AnyElement) -> AnyElement {
    v_flex()
        .w_full()
        .gap_3()
        .child(
            Label::new(title.to_string())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(body)
        .into_any_element()
}

fn meta_chip(label: &str, value: &str, cx: &App) -> AnyElement {
    h_flex()
        .gap_2()
        .items_center()
        .px_3()
        .py_1p5()
        .rounded_full()
        .bg(cx.theme().colors().element_background)
        .child(
            Label::new(label.to_string())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(value.to_string()).size(LabelSize::Small))
        .into_any_element()
}

fn flow_pill(node: &DevFlowNode, cx: &App) -> AnyElement {
    let (glyph, color) = dev_flow_visual(node);
    h_flex()
        .gap_2()
        .items_center()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().element_background)
        .child(Label::new(glyph).size(LabelSize::Small).color(color))
        .child(
            Label::new(node.node.clone())
                .size(LabelSize::Small)
                .color(if node.is_human_gate {
                    Color::Muted
                } else {
                    Color::Default
                }),
        )
        .into_any_element()
}

fn badge(label: &str, color: Color, cx: &App) -> AnyElement {
    div()
        .px_2()
        .py_1()
        .rounded_full()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            Label::new(label.to_string())
                .size(LabelSize::XSmall)
                .color(color),
        )
        .into_any_element()
}

fn chip_row(title: &str, chips: &[RelationChip], cx: &mut Context<PlanGraph>) -> AnyElement {
    v_flex()
        .w_full()
        .gap_2()
        .child(
            Label::new(title.to_string())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .children(chips.iter().map(|chip| {
                    let id = chip.id.clone();
                    let label = if chip.incoming {
                        format!("← {}", chip.label)
                    } else {
                        format!("→ {}", chip.label)
                    };
                    div()
                        .id(ElementId::Name(
                            format!("chip:{}:{}:{}", title, chip.id, chip.incoming).into(),
                        ))
                        .px_3()
                        .py_1p5()
                        .rounded_full()
                        .border_1()
                        .border_color(cx.theme().colors().border_variant)
                        .bg(cx.theme().colors().element_background)
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors().element_hover))
                        .on_click(cx.listener(
                            move |this: &mut PlanGraph, _: &ClickEvent, window, cx| {
                                this.open_plan(id.clone(), window, cx)
                            },
                        ))
                        .child(Label::new(label).size(LabelSize::Small))
                        .into_any_element()
                })),
        )
        .into_any_element()
}

impl Focusable for PlanGraph {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for PlanGraph {}

impl Render for PlanGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().child(self.render_toolbar(cx)).child(
            h_flex()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .w(px(320.0))
                        .h_full()
                        .border_r_1()
                        .border_color(cx.theme().colors().border)
                        .child(self.render_tree(cx)),
                )
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .child(self.render_detail(window, cx)),
                ),
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
        workspace.register_action(
            |workspace: &mut Workspace, action: &OpenPlan, window, cx| {
                let item = PlanGraph::deploy(workspace, window, cx);
                let plan_id = action.plan_id.clone();
                item.update(cx, |graph, cx| {
                    graph.navigate_to_plan(plan_id.as_ref(), cx);
                });
            },
        );
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use plan_store::{GraphSnapshot, RoadmapPlanRow};

    fn category(id: &str, parent: Option<&str>, name: &str, rank: i64) -> CategoryRow {
        CategoryRow {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            name: name.to_string(),
            rank,
        }
    }

    fn plan(
        id: &str,
        name: &str,
        category_id: Option<&str>,
        status: Option<&str>,
    ) -> CategorizedPlanRow {
        CategorizedPlanRow {
            id: id.to_string(),
            name: name.to_string(),
            repo: Some("zed".to_string()),
            stage: Some("plan".to_string()),
            status: status.map(str::to_string),
            current_node: Some("implement".to_string()),
            category_id: category_id.map(str::to_string),
            category_source: Some("auto".to_string()),
            summary: Some("one-line summary".to_string()),
            detail: Some("a paragraph".to_string()),
        }
    }

    fn taxonomy() -> Vec<CategoryRow> {
        vec![
            category("feature", None, "Feature", 100),
            category("feature/agent-core", Some("feature"), "Agent Core", 110),
            category("bug", None, "Bug", 200),
            category("bug/threads", Some("bug"), "Threads", 210),
        ]
    }

    fn expanded(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|key| key.to_string()).collect()
    }

    fn find<'a>(node: &'a VisibleNode, key: &str) -> Option<&'a VisibleNode> {
        if node.key == key {
            return Some(node);
        }
        node.children.iter().find_map(|child| find(child, key))
    }

    #[test]
    fn wheel_starts_with_only_the_root() {
        let plans = vec![
            plan("p1", "Plan one", Some("feature/agent-core"), None),
            plan("p2", "Plan two", Some("bug/threads"), None),
        ];
        let collapsed = build_visible_tree(
            &taxonomy(),
            &plans,
            &[],
            &HashMap::new(),
            &expanded(&[]),
            false,
        );
        assert_eq!(collapsed.key, "root");
        assert!(
            collapsed.children.is_empty(),
            "only Plans is visible at first"
        );
        assert!(collapsed.expandable);

        let open = build_visible_tree(
            &taxonomy(),
            &plans,
            &[],
            &HashMap::new(),
            &expanded(&["root"]),
            false,
        );
        let labels: Vec<&str> = open.children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["Feature · 1", "Bug · 1"]);
        assert!(open.children.iter().all(|c| c.children.is_empty()));
        assert!(open.children.iter().all(|c| c.expandable));
    }

    #[test]
    fn expanding_a_category_reveals_only_its_subcategories() {
        let plans = vec![
            plan("p1", "Plan one", Some("feature/agent-core"), None),
            plan("p2", "Plan two", Some("bug/threads"), None),
        ];
        let root = build_visible_tree(
            &taxonomy(),
            &plans,
            &[],
            &HashMap::new(),
            &expanded(&["root", "cat:feature"]),
            false,
        );
        let feature = find(&root, "cat:feature").expect("feature category");
        let bug = find(&root, "cat:bug").expect("bug category");
        assert_eq!(feature.children.len(), 1);
        assert!(bug.children.is_empty(), "siblings stay collapsed");
        assert_eq!(feature.children[0].key, "sub:feature/agent-core");
    }

    #[test]
    fn expanding_a_category_collapses_cleanly() {
        let root_open = build_visible_tree(
            &taxonomy(),
            &[],
            &[],
            &HashMap::new(),
            &expanded(&["root", "cat:feature"]),
            false,
        );
        assert_eq!(find(&root_open, "cat:feature").unwrap().children.len(), 1);
        let root_closed = build_visible_tree(
            &taxonomy(),
            &[],
            &[],
            &HashMap::new(),
            &expanded(&["root"]),
            false,
        );
        assert!(
            find(&root_closed, "cat:feature")
                .unwrap()
                .children
                .is_empty()
        );
    }

    #[test]
    fn plan_with_slices_gets_a_collapsible_slice_group() {
        let plans = vec![
            plan("parent", "Parent plan", Some("feature/agent-core"), None),
            plan("slice", "Slice plan", None, None),
        ];
        let parents = vec![PlanParentRow {
            plan_id: "slice".to_string(),
            parent_id: "parent".to_string(),
        }];
        let root = build_visible_tree(
            &taxonomy(),
            &plans,
            &parents,
            &HashMap::new(),
            &expanded(&["root", "cat:feature", "sub:feature/agent-core"]),
            false,
        );
        let parent = find(&root, "plan:parent").expect("parent plan");
        assert!(parent.expandable);
        assert!(parent.children.is_empty(), "slices are behind a toggle");

        let root = build_visible_tree(
            &taxonomy(),
            &plans,
            &parents,
            &HashMap::new(),
            &expanded(&[
                "root",
                "cat:feature",
                "sub:feature/agent-core",
                "plan:parent",
            ]),
            false,
        );
        let parent = find(&root, "plan:parent").expect("parent plan");
        assert_eq!(parent.children.len(), 1);
        assert_eq!(parent.children[0].key, "slices:parent");
        assert_eq!(parent.children[0].label, "slices (1)");
    }

    #[test]
    fn history_is_hidden_until_toggled() {
        let plans = vec![
            plan("live", "Live plan", Some("feature/agent-core"), None),
            plan("done", "Done plan", None, Some("done — shipped")),
        ];
        let hidden = build_visible_tree(
            &taxonomy(),
            &plans,
            &[],
            &HashMap::new(),
            &expanded(&["root"]),
            false,
        );
        assert!(find(&hidden, "history").is_none());

        let shown = build_visible_tree(
            &taxonomy(),
            &plans,
            &[],
            &HashMap::new(),
            &expanded(&["root", "history"]),
            true,
        );
        let history = find(&shown, "history").expect("history node");
        assert_eq!(history.children.len(), 1);
        assert_eq!(history.children[0].plan_id.as_deref(), Some("done"));
    }

    #[test]
    fn plan_id_from_path_normalizes_progress_and_markdown_paths() {
        assert_eq!(
            plan_id_from_path("/home/me/plans/in-progress-plans/PLAN_OF_ACTION-2026-11-19-x.md"),
            "plan-of-action-2026-11-19-x"
        );
        assert_eq!(
            plan_id_from_path("/x/PLAN_OF_ACTION-2026-11-19-x.progress.json"),
            "plan-of-action-2026-11-19-x"
        );
        assert_eq!(
            plan_id_from_path("plan-of-action-2026-11-19-x"),
            "plan-of-action-2026-11-19-x"
        );
    }

    fn row(id: &str, current: &str, spent: u64) -> FleetPlanRow {
        FleetPlanRow {
            id: id.to_string(),
            name: id.to_string(),
            repo: Some("zed".to_string()),
            current_node: Some(current.to_string()),
            active_node: None,
            holder: None,
            heartbeat: None,
            blocked_reason: None,
            spent_tokens: spent,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn snapshot(name: &str, current: &str) -> GraphSnapshot {
        GraphSnapshot {
            plan_id: name.to_string(),
            name: name.to_string(),
            repo: Some("zed".to_string()),
            current_node: Some(current.to_string()),
            active_node: None,
            holder: None,
            heartbeat: None,
            blocked_reason: None,
            spent_tokens: 0,
            done_nodes: vec!["plan".to_string(), "promote".to_string()],
            evidence: Vec::new(),
        }
    }

    fn evidence() -> EvidenceRow {
        EvidenceRow {
            plan_id: "PLAN_A".to_string(),
            node: "plan".to_string(),
            artifact_sha: Some("abc123".to_string()),
            gate_exit_code: Some(0),
            report_path: None,
            falsifying_condition: Some("falsifies".to_string()),
            negative_control: Some(0),
        }
    }

    #[test]
    fn states_derive_from_graph_state() {
        let moving = FleetPlanRow {
            active_node: Some("implement".to_string()),
            heartbeat: Some(Utc::now().to_rfc3339()),
            ..row("MOVING", "implement", 0)
        };
        assert_eq!(derive_node_state(&moving, &[], now()), NodeState::Moving);
        let stalled = FleetPlanRow {
            active_node: Some("implement".to_string()),
            heartbeat: Some(
                (Utc::now() - chrono::Duration::seconds(STALL_SECONDS + 1)).to_rfc3339(),
            ),
            ..row("STALLED", "implement", 0)
        };
        assert_eq!(derive_node_state(&stalled, &[], now()), NodeState::Stuck);
        let blocked = FleetPlanRow {
            blocked_reason: Some("human gate".to_string()),
            ..row("BLOCKED", "verify", 0)
        };
        assert_eq!(derive_node_state(&blocked, &[], now()), NodeState::Blocked);
        assert_eq!(
            derive_node_state(&row("DONE", "complete", 0), &[], now()),
            NodeState::Done
        );
        assert_eq!(
            derive_node_state(&row("PENDING", "plan", 0), &[], now()),
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
            NodeState::Pending,
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
        assert_eq!(legend().len(), 4);
    }

    #[test]
    fn tooltip_carries_the_summary() {
        let node = GraphNode {
            id: "PLAN_A".to_string(),
            name: "PLAN_A".to_string(),
            state: NodeState::Moving,
            phase: "implement".to_string(),
            done_count: 2,
            breaker: "closed".to_string(),
        };
        let text = tooltip_text(&node);
        assert!(text.contains("PLAN_A"));
        assert!(text.contains("moving"));
        assert!(text.contains("implement"));
    }

    #[test]
    fn evidence_complete_requires_sha_condition_and_negative_control() {
        let mut row = evidence();
        assert!(evidence_complete(&row));
        row.negative_control = None;
        assert!(!evidence_complete(&row));
        row.negative_control = Some(0);
        row.falsifying_condition = None;
        assert!(!evidence_complete(&row));
        row.falsifying_condition = Some("falsifies".to_string());
        row.artifact_sha = None;
        assert!(!evidence_complete(&row));
    }

    #[test]
    fn dev_flow_marks_done_complete_vacuous_and_human_gates() {
        let nodes = dev_flow_from_rows(
            &["plan".to_string(), "promote".to_string()],
            vec![evidence()],
        );
        let plan = nodes.iter().find(|node| node.node == "plan").unwrap();
        assert!(plan.done);
        assert!(plan.complete);
        let promote = nodes.iter().find(|node| node.node == "promote").unwrap();
        assert!(promote.done);
        assert!(!promote.complete, "a done node without evidence is vacuous");
        let merge = nodes.iter().find(|node| node.node == "merge").unwrap();
        assert!(merge.is_human_gate);
    }

    #[test]
    fn decision_projection_stops_at_human_gates_and_retries_failed_gates() {
        let store = PlanStore::open_in_memory().expect("in-memory store");

        let mut active = snapshot("PLAN_A", "implement");
        active.active_node = Some("implement".to_string());
        store.apply_graph_snapshot(&active).unwrap();
        assert_eq!(
            project_decision(&store, "PLAN_A").kind,
            DecisionKind::Active
        );

        let mut next = snapshot("PLAN_B", "verify");
        next.done_nodes = ["plan", "promote", "claim", "fork", "implement", "verify"]
            .into_iter()
            .map(str::to_string)
            .collect();
        store.apply_graph_snapshot(&next).unwrap();
        let decision = project_decision(&store, "PLAN_B");
        assert_eq!(decision.kind, DecisionKind::Next);
        assert_eq!(decision.node, "compile");

        let mut gate = snapshot("PLAN_C", "accept");
        gate.done_nodes = [
            "plan",
            "promote",
            "claim",
            "fork",
            "implement",
            "verify",
            "compile",
            "accept",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        store.apply_graph_snapshot(&gate).unwrap();
        let decision = project_decision(&store, "PLAN_C");
        assert_eq!(decision.kind, DecisionKind::Blocked);
        assert_eq!(decision.node, "merge");

        let mut retry = snapshot("PLAN_D", "verify");
        retry.done_nodes = ["plan", "promote", "claim", "fork", "implement", "verify"]
            .into_iter()
            .map(str::to_string)
            .collect();
        retry.evidence = vec![EvidenceRow {
            plan_id: "PLAN_D".to_string(),
            node: "compile".to_string(),
            artifact_sha: None,
            gate_exit_code: Some(1),
            report_path: None,
            falsifying_condition: None,
            negative_control: None,
        }];
        retry.done_nodes.push("compile".to_string());
        store.apply_graph_snapshot(&retry).unwrap();
        assert_eq!(project_decision(&store, "PLAN_D").kind, DecisionKind::Retry);
    }

    #[test]
    fn roadmap_metadata_round_trips_for_the_page() {
        let store = PlanStore::open_in_memory().expect("in-memory store");
        store
            .upsert_plan_metadata(&RoadmapPlanRow {
                id: "PLAN_A".to_string(),
                name: "PLAN_A".to_string(),
                repo: Some("zed".to_string()),
                stage: Some("plan".to_string()),
                status: Some("in progress".to_string()),
                order_rank: None,
            })
            .unwrap();
        let plans = store.categorized_plans().unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "PLAN_A");
    }

    #[test]
    fn expanding_a_path_reveals_the_plan_under_its_categories() {
        let plans = vec![plan("p1", "Plan one", Some("feature/agent-core"), None)];
        let mut expanded = HashSet::new();
        expand_path_to_plan(&mut expanded, &taxonomy(), &plans, &[], "p1");
        assert!(expanded.contains("root"));
        assert!(expanded.contains("cat:feature"));
        assert!(expanded.contains("sub:feature/agent-core"));
    }

    #[test]
    fn expanding_a_slice_reveals_its_parent_slices_group() {
        let plans = vec![
            plan("parent", "Parent", Some("feature/agent-core"), None),
            plan("slice", "Slice", None, None),
        ];
        let parents = vec![PlanParentRow {
            plan_id: "slice".to_string(),
            parent_id: "parent".to_string(),
        }];
        let mut expanded = HashSet::new();
        expand_path_to_plan(&mut expanded, &taxonomy(), &plans, &parents, "slice");
        assert!(expanded.contains("root"));
        assert!(expanded.contains("cat:feature"));
        assert!(expanded.contains("sub:feature/agent-core"));
        assert!(expanded.contains("plan:parent"));
        assert!(expanded.contains("slices:parent"));
    }
}
