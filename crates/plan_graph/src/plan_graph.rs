//! Interactive radial plan wheel (1.29 re-scope).
//!
//! One main-editor-area `Item` renders the plan taxonomy as a wheel-and-spoke:
//! the `Plans` root at the centre, the top-level categories on the first ring,
//! subcategories outward, and plan/slice leaves at the edge. Progressive
//! disclosure keeps it navigable: single-click expands a node's children,
//! double-click opens a plan's page, and the camera auto-fits as the frontier
//! grows. The page carries the plan's metadata, summary, read-only decision,
//! related/blocking/slice chips, and a plan-scoped agent composer + thread
//! sidebar.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::f32::consts::TAU;
use std::rc::Rc;
use std::time::Duration;

use agent_client_protocol::schema::v1 as acp;
use chrono::{DateTime, Utc};
use editor::Editor;
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, ElementId, EventEmitter, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Rems,
    Render, ScrollWheelEvent, Size, StatefulInteractiveElement, WeakEntity, Window, actions, point,
    px,
};
use plan_store::{
    CategorizedPlanRow, CategoryRow, EdgeRow, EvidenceRow, FleetPlanRow, Lifecycle, PlanParentRow,
    PlanStore,
};
use ui::{Button, Color, Label, LabelSize, prelude::*};
use workspace::Workspace;
use workspace::item::Item;

actions!(plan_graph, [DeployPlanGraph]);

/// The dev-flow route — the same readiness table the `graph_tool` coordinator
/// and the fleet panel use.
const ROUTE: [&str; 11] = [
    "plan",
    "promote",
    "claim",
    "fork",
    "implement",
    "verify",
    "compile",
    "accept",
    "merge",
    "release",
    "complete",
];

/// A plan with no activity for this long is visibly stalled, not "moving".
const STALL_SECONDS: i64 = 15 * 60;

/// The debugger loop's `stage→…→fix` sub-graph (1.29 D8).
const DEBUGGER_SUB_GRAPH: [&str; 6] = ["stage", "test", "verify", "restart", "run", "fix"];

/// Base distance between rings and the minimum spacing a ring's circumference
/// must provide for its nodes, both in unscaled wheel coordinates.
const RING_BASE: f32 = 220.0;
const NODE_SPACING: f32 = 250.0;
const NODE_WIDTH: f32 = 190.0;
const NODE_HEIGHT: f32 = 58.0;
const NODE_GAP: f32 = 28.0;
/// No child may claim less than this fraction of an even share of its ring, so a
/// small subtree beside a huge one cannot squeeze its neighbour into it.
const MIN_SLOT_SHARE: f32 = 0.55;
/// A ring whose tightest pair needs more than this is left overlapping rather
/// than blowing the wheel (and the camera fit) up to an unusable size.
const MAX_RING_RADIUS: f32 = 9000.0;
const MIN_ZOOM: f32 = 0.12;
const MAX_ZOOM: f32 = 2.5;

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
    ROUTE
        .iter()
        .copied()
        .find(|node| !done.contains(node) && prerequisites(node).iter().all(|p| done.contains(p)))
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
    if let Some(active) = &plan.active_node {
        return Decision {
            kind: DecisionKind::Active,
            node: active.clone(),
            reason: "node is active".to_string(),
        };
    }

    let done = store.done_nodes(plan_id).unwrap_or_default();
    let evidence = store.evidence_for(plan_id).unwrap_or_default();
    let done_set: HashSet<&str> = done.iter().map(|node| node.as_str()).collect();
    let failed: HashSet<&str> = evidence
        .iter()
        .filter(|row| row.gate_exit_code == Some(1))
        .map(|row| row.node.as_str())
        .collect();

    if let Some(retry) = ROUTE
        .iter()
        .copied()
        .find(|node| done_set.contains(node) && failed.contains(node))
    {
        return Decision {
            kind: DecisionKind::Retry,
            node: retry.to_string(),
            reason: "failed — retry".to_string(),
        };
    }

    match next_ready(&done_set) {
        Some(node) if is_human_gate(node) => Decision {
            kind: DecisionKind::Blocked,
            node: node.to_string(),
            reason: "human gate — pause and ask".to_string(),
        },
        Some(node) => Decision {
            kind: DecisionKind::Next,
            node: node.to_string(),
            reason: "next step".to_string(),
        },
        None => Decision {
            kind: DecisionKind::Paused,
            node: "—".to_string(),
            reason: "no ready node".to_string(),
        },
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
pub enum WheelKind {
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
    pub kind: WheelKind,
    pub plan_id: Option<String>,
    pub state: NodeState,
    pub expanded: bool,
    pub expandable: bool,
    pub slice_count: usize,
    pub children: Vec<VisibleNode>,
}

impl VisibleNode {
    fn new(key: String, label: String, kind: WheelKind) -> Self {
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

/// One positioned node. `x`/`y` are unscaled offsets from the wheel centre.
#[derive(Debug, Clone, PartialEq)]
pub struct WheelNode {
    pub key: String,
    pub label: String,
    pub kind: WheelKind,
    pub plan_id: Option<String>,
    pub state: NodeState,
    pub expanded: bool,
    pub expandable: bool,
    pub slice_count: usize,
    pub depth: usize,
    /// The node's slot midpoint on its ring, in radians. Kept so the layout can
    /// measure how tightly a ring packs without re-deriving the angles.
    pub angle: f32,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WheelSpoke {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WheelLayout {
    pub nodes: Vec<WheelNode>,
    pub spokes: Vec<WheelSpoke>,
    pub extent: f32,
}

/// One depth's ring: where it sits, and the smallest share of a parent's sector
/// any single child may claim.
#[derive(Debug, Clone, Copy, Default)]
struct Ring {
    radius: f32,
    min_share: f32,
}

/// The camera over the wheel. Positions are scaled by `zoom` and offset by the
/// pan; the plan page remembers this so `Back to graph` is reversible (D13).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub zoom: f32,
    pub pan_x: f32,
    pub pan_y: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }
}

impl Camera {
    /// Fit a wheel of `extent` into the viewport with a small margin.
    pub fn fit(extent: f32, viewport: Size<gpui::Pixels>) -> Self {
        if extent <= 0.0 {
            return Self::default();
        }
        let width = f32::from(viewport.width);
        let height = f32::from(viewport.height);
        let available = width.min(height) * 0.92;
        let zoom = (available / extent).clamp(MIN_ZOOM, 1.3);
        Self {
            zoom,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }

    pub fn zoomed(self, factor: f32) -> Self {
        Self {
            zoom: (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM),
            ..self
        }
    }

    /// Where a wheel coordinate lands on screen. The pane's centre is the
    /// origin of the wheel, so an identity camera puts the root in the middle and
    /// every other view is a `zoom` plus a `pan` around that one point.
    ///
    /// This is the single transform both the node layout and the spoke paint
    /// use, so the two can never drift apart.
    pub fn wheel_to_pane(self, x: f32, y: f32, pane: Size<Pixels>) -> Point<Pixels> {
        point(
            px(f32::from(pane.width) / 2.0 + x * self.zoom + self.pan_x),
            px(f32::from(pane.height) / 2.0 + y * self.zoom + self.pan_y),
        )
    }

    /// Put wheel coordinate `(x, y)` at `target` (pane coordinates) at the
    /// current zoom.
    ///
    /// This is d3-zoom's `translateTo`: `t = target - centre - k * (x, y)`, which
    /// is `t = -k * (x, y)` when the target is the pane's centre.
    pub fn translate_to(self, x: f32, y: f32, target: Point<Pixels>, pane: Size<Pixels>) -> Self {
        Self {
            pan_x: f32::from(target.x) - f32::from(pane.width) / 2.0 - x * self.zoom,
            pan_y: f32::from(target.y) - f32::from(pane.height) / 2.0 - y * self.zoom,
            ..self
        }
    }

    /// Centre `(x, y)` in the pane at the current zoom.
    pub fn centered_on(self, x: f32, y: f32, pane: Size<Pixels>) -> Self {
        self.translate_to(x, y, pane_center(pane), pane)
    }

    /// Frame `anchor` (the node the user clicked) in the middle of the pane,
    /// zoomed out just enough for its whole visible subtree to fit around it.
    pub fn fit_around(
        anchor_x: f32,
        anchor_y: f32,
        min_x: f32,
        min_y: f32,
        max_x: f32,
        max_y: f32,
        pane: Size<Pixels>,
    ) -> Self {
        let width = f32::from(pane.width).max(1.0);
        let height = f32::from(pane.height).max(1.0);
        let reach_x = (max_x - anchor_x).max(anchor_x - min_x) + (NODE_WIDTH + NODE_GAP) / 2.0;
        let reach_y = (max_y - anchor_y).max(anchor_y - min_y) + (NODE_HEIGHT + NODE_GAP) / 2.0;
        let zoom = (width * 0.46 / reach_x.max(1.0))
            .min(height * 0.46 / reach_y.max(1.0))
            .clamp(MIN_ZOOM, 1.6);
        Self {
            zoom,
            pan_x: 0.0,
            pan_y: 0.0,
        }
        .centered_on(anchor_x, anchor_y, pane)
    }

    /// Zoom about a pane point, keeping whatever sits under it in place.
    ///
    /// `scaleTo(k, p)` from d3-zoom: the reference point `p` does not move, so
    /// `pan' = p - centre - (k' / k) * (p - centre - pan)`.
    pub fn zoom_about(self, target_zoom: f32, anchor: Point<Pixels>, pane: Size<Pixels>) -> Self {
        let target_zoom = target_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let ratio = target_zoom / self.zoom;
        let rel_x = f32::from(anchor.x) - f32::from(pane.width) / 2.0;
        let rel_y = f32::from(anchor.y) - f32::from(pane.height) / 2.0;
        Self {
            zoom: target_zoom,
            pan_x: rel_x - (rel_x - self.pan_x) * ratio,
            pan_y: rel_y - (rel_y - self.pan_y) * ratio,
        }
    }
}

fn pane_center(pane: Size<Pixels>) -> Point<Pixels> {
    point(
        px(f32::from(pane.width) / 2.0),
        px(f32::from(pane.height) / 2.0),
    )
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

fn wheel_state(plan: &CategorizedPlanRow, fleet: Option<&FleetPlanRow>) -> NodeState {
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
pub fn build_visible_wheel(
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
                                    WheelKind::Slice,
                                );
                                node.plan_id = Some((*slice_id).to_string());
                                node.state = wheel_state(slice, fleet.get(*slice_id));
                                slice_children.push(node);
                            }
                        }
                    }
                    let mut group = VisibleNode::new(
                        group_key,
                        format!("slices ({})", slices.len()),
                        WheelKind::SliceGroup,
                    );
                    group.plan_id = Some(plan.id.clone());
                    group.expandable = !slice_children.is_empty();
                    group.expanded = group_expanded;
                    group.slice_count = slices.len();
                    group.children = slice_children;
                    children.push(group);
                }

                let mut node = VisibleNode::new(plan_key, plan_label(plan), WheelKind::Plan);
                node.plan_id = Some(plan.id.clone());
                node.state = wheel_state(plan, fleet.get(plan.id.as_str()));
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
                WheelKind::Subcategory,
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
            WheelKind::Category,
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
                    WheelKind::Plan,
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
            WheelKind::History,
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

    let mut root = VisibleNode::new("root".to_string(), "Plans".to_string(), WheelKind::Root);
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

fn descendant_count(node: &VisibleNode) -> usize {
    1 + node.children.iter().map(descendant_count).sum::<usize>()
}

/// Every visible key in `key`'s subtree, so the camera can frame a node and
/// what it just revealed rather than the whole map.
fn subtree_keys(node: &VisibleNode, key: &str) -> Option<Vec<String>> {
    if node.key == key {
        let mut keys = Vec::new();
        collect_keys(node, &mut keys);
        return Some(keys);
    }
    node.children
        .iter()
        .find_map(|child| subtree_keys(child, key))
}

fn collect_keys(node: &VisibleNode, keys: &mut Vec<String>) {
    keys.push(node.key.clone());
    for child in &node.children {
        collect_keys(child, keys);
    }
}

/// Lay the visible tree out on concentric rings. A child's wedge is
/// proportional to its subtree, so every branch keeps its own sector; a floor on
/// that wedge stops two small neighbours collapsing onto each other when they
/// sit beside a huge branch. Ring radii then grow to fit both the count on the
/// ring and the tightest gap actually assigned to it. Pure so the camera-fit and
/// disclosure tests can assert it.
pub fn layout_wheel(root: &VisibleNode) -> WheelLayout {
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    fn count(node: &VisibleNode, depth: usize, counts: &mut BTreeMap<usize, usize>) {
        *counts.entry(depth).or_insert(0) += 1;
        for child in &node.children {
            count(child, depth + 1, counts);
        }
    }
    count(root, 0, &mut counts);
    let max_depth = counts.keys().max().copied().unwrap_or(0);

    let mut rings = vec![Ring::default(); max_depth + 1];
    for depth in 1..=max_depth {
        let nodes = counts.get(&depth).copied().unwrap_or(1).max(1) as f32;
        rings[depth].min_share = MIN_SLOT_SHARE / nodes;
    }

    // Angles do not depend on the radii, so lay the tree out once at zero radius
    // to learn how tightly each floored ring packs.
    let mut probe = WheelLayout::default();
    let mut probe_extent = 0.0_f32;
    place_node(root, 0, 0.0, TAU, &rings, &mut probe, &mut probe_extent);

    let mut previous = 0.0_f32;
    for depth in 1..=max_depth {
        let nodes = counts.get(&depth).copied().unwrap_or(1).max(1) as f32;
        let radius = (depth as f32 * RING_BASE)
            .max(nodes * NODE_SPACING / TAU)
            .max(previous + RING_BASE)
            .max(required_clearance(&probe.nodes, depth))
            .min(MAX_RING_RADIUS);
        rings[depth].radius = radius;
        previous = radius;
    }

    let mut layout = WheelLayout::default();
    let mut max_radius = 0.0_f32;
    place_node(root, 0, 0.0, TAU, &rings, &mut layout, &mut max_radius);
    layout.extent = (max_radius + NODE_HEIGHT) * 2.0;
    layout
}

/// The radius at which the two closest nodes on `depth`'s ring are still a full
/// node apart. Adjacent boxes are `2 * r * sin(gap / 2)` apart, so clearing
/// `NODE_WIDTH + NODE_GAP` needs `r >= (width + gap) / (2 sin(gap / 2))`.
fn required_clearance(nodes: &[WheelNode], depth: usize) -> f32 {
    let mut angles: Vec<f32> = nodes
        .iter()
        .filter(|node| node.depth == depth)
        .map(|node| node.angle)
        .collect();
    if angles.len() < 2 {
        return 0.0;
    }
    angles.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut tightest = f32::MAX;
    for pair in angles.windows(2) {
        tightest = tightest.min(pair[1] - pair[0]);
    }
    tightest = tightest.min(angles[0] + TAU - angles[angles.len() - 1]);
    if tightest <= 0.0 {
        return 0.0;
    }
    (NODE_WIDTH + NODE_GAP) / (2.0 * (tightest / 2.0).sin())
}

fn place_node(
    node: &VisibleNode,
    depth: usize,
    start: f32,
    end: f32,
    rings: &[Ring],
    layout: &mut WheelLayout,
    max_radius: &mut f32,
) {
    let radius = rings.get(depth).map(|ring| ring.radius).unwrap_or(0.0);
    let angle = (start + end) / 2.0 - std::f32::consts::FRAC_PI_2;
    let x = angle.cos() * radius;
    let y = angle.sin() * radius;
    *max_radius = max_radius.max(radius);

    layout.nodes.push(WheelNode {
        key: node.key.clone(),
        label: node.label.clone(),
        kind: node.kind,
        plan_id: node.plan_id.clone(),
        state: node.state,
        expanded: node.expanded,
        expandable: node.expandable,
        slice_count: node.slice_count,
        depth,
        angle: (start + end) / 2.0,
        x,
        y,
    });

    if node.children.is_empty() {
        return;
    }
    let min_share = rings
        .get(depth + 1)
        .map(|ring| ring.min_share)
        .unwrap_or(0.0);
    let weights: Vec<f32> = node
        .children
        .iter()
        .map(|child| descendant_count(child) as f32)
        .collect();
    let total: f32 = weights.iter().sum();
    let shares: Vec<f32> = weights
        .iter()
        .map(|weight| (weight / total).max(min_share))
        .collect();
    let share_total: f32 = shares.iter().sum();

    let mut cursor = start;
    for (child, share) in node.children.iter().zip(shares) {
        let span = (end - start) * (share / share_total);
        place_node(
            child,
            depth + 1,
            cursor,
            cursor + span,
            rings,
            layout,
            max_radius,
        );
        let child_radius = rings.get(depth + 1).map(|ring| ring.radius).unwrap_or(0.0);
        let child_angle = cursor + span / 2.0 - std::f32::consts::FRAC_PI_2;
        layout.spokes.push(WheelSpoke {
            x1: x,
            y1: y,
            x2: child_angle.cos() * child_radius,
            y2: child_angle.sin() * child_radius,
        });
        cursor += span;
    }
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

struct DragState {
    start: Point<Pixels>,
    start_pan: (f32, f32),
    moved: bool,
}

/// The wheel `Item` (main editor area, not a bottom `Panel`).
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
    camera: Camera,
    needs_fit: bool,
    saved_camera: Option<Camera>,
    focused_key: Option<String>,
    saved_focus: Option<String>,
    page_stack: Vec<String>,
    page: Option<PageData>,
    composer: gpui::Entity<Editor>,
    threads: Vec<PlanThread>,
    drag: Option<DragState>,
    suppress_click: bool,
    /// The pane's bounds as laid out, recorded on paint so zoom anchors on the
    /// point under the cursor rather than the window.
    wheel_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
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
            camera: Camera::default(),
            needs_fit: true,
            saved_camera: None,
            focused_key: None,
            saved_focus: None,
            page_stack: Vec::new(),
            page: None,
            composer,
            threads: Vec::new(),
            drag: None,
            suppress_click: false,
            wheel_bounds: Rc::new(Cell::new(None)),
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

    fn visible_wheel(&self) -> VisibleNode {
        build_visible_wheel(
            &self.categories,
            &self.plans,
            &self.parents,
            &self.fleet,
            &self.expanded,
            self.history_visible,
        )
    }

    fn current_layout(&self) -> WheelLayout {
        layout_wheel(&self.visible_wheel())
    }

    fn fit_camera(&mut self, window: &mut Window) {
        let layout = self.current_layout();
        self.camera = Camera::fit(layout.extent, self.pane_size(window));
    }

    /// The pane's size, falling back to the window before the first paint has
    /// recorded the wheel's bounds.
    fn pane_size(&self, window: &Window) -> Size<Pixels> {
        self.wheel_bounds
            .get()
            .map(|bounds| bounds.size)
            .unwrap_or_else(|| window.viewport_size())
    }

    /// Frame the clicked node at the pane's centre, with everything it just
    /// revealed fitted around it. Expanding used to re-fit the whole map, and
    /// centring on the subtree's bounding box left the clicked node off-centre.
    fn fit_camera_to(&mut self, key: &str, window: &Window) {
        let wheel = self.visible_wheel();
        let layout = layout_wheel(&wheel);
        let pane = self.pane_size(window);
        let Some(anchor) = layout.nodes.iter().find(|node| node.key == key) else {
            self.camera = Camera::fit(layout.extent, pane);
            return;
        };
        let anchor = (anchor.x, anchor.y);
        let Some(keys) = subtree_keys(&wheel, key) else {
            self.camera = Camera::fit(layout.extent, pane);
            return;
        };
        let mut min_x = anchor.0;
        let mut min_y = anchor.1;
        let mut max_x = anchor.0;
        let mut max_y = anchor.1;
        for node in layout.nodes.iter().filter(|node| keys.contains(&node.key)) {
            min_x = min_x.min(node.x);
            min_y = min_y.min(node.y);
            max_x = max_x.max(node.x);
            max_y = max_y.max(node.y);
        }
        self.camera = Camera::fit_around(anchor.0, anchor.1, min_x, min_y, max_x, max_y, pane);
    }

    /// Collapsing zooms the wheel back out to fit everything now visible, but
    /// keeps the clicked node at the centre.
    fn fit_whole_centered_on(&mut self, key: &str, window: &Window) {
        let layout = self.current_layout();
        let pane = self.pane_size(window);
        let Some(anchor) = layout.nodes.iter().find(|node| node.key == key) else {
            return;
        };
        let mut min_x = anchor.x;
        let mut min_y = anchor.y;
        let mut max_x = anchor.x;
        let mut max_y = anchor.y;
        for node in &layout.nodes {
            min_x = min_x.min(node.x);
            min_y = min_y.min(node.y);
            max_x = max_x.max(node.x);
            max_y = max_y.max(node.y);
        }
        self.camera = Camera::fit_around(anchor.x, anchor.y, min_x, min_y, max_x, max_y, pane);
    }

    /// Wheel up zooms in, wheel down zooms out, anchored under the cursor so the
    /// node you are looking at stays put.
    fn handle_wheel_zoom(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(window.line_height());
        let steps = (f32::from(delta.y) / 120.0).clamp(-3.0, 3.0);
        if steps == 0.0 {
            return;
        }
        let target = (self.camera.zoom * 1.15_f32.powf(steps)).clamp(MIN_ZOOM, MAX_ZOOM);
        if (target - self.camera.zoom).abs() < f32::EPSILON {
            return;
        }
        let origin = self
            .wheel_bounds
            .get()
            .map(|bounds| bounds.origin)
            .unwrap_or_else(|| point(px(0.0), px(0.0)));
        let anchor = point(event.position.x - origin.x, event.position.y - origin.y);
        let pane = self.pane_size(window);
        self.camera = self.camera.zoom_about(target, anchor, pane);
        self.needs_fit = false;
        cx.notify();
    }

    fn plan_display(&self, plan_id: &str) -> String {
        self.plans
            .iter()
            .find(|plan| plan.id == plan_id)
            .map(plan_label)
            .unwrap_or_else(|| short_label(plan_id))
    }

    fn open_plan(&mut self, plan_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.page_stack.is_empty() {
            self.saved_camera = Some(self.camera);
            self.saved_focus = self.focused_key.clone();
        }
        self.focused_key = Some(format!("plan:{plan_id}"));
        self.page_stack.push(plan_id.clone());
        self.load_page(&plan_id);
        self.threads.clear();
        self.composer
            .update(cx, |editor, cx| editor.clear(window, cx));
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
                if let Some(camera) = self.saved_camera.take() {
                    self.camera = camera;
                }
                self.focused_key = self.saved_focus.take();
            }
        }
        let _ = window;
        cx.notify();
    }

    fn back_to_graph(&mut self, cx: &mut Context<Self>) {
        if self.page_stack.is_empty() {
            return;
        }
        self.page_stack.clear();
        self.page = None;
        self.threads.clear();
        if let Some(camera) = self.saved_camera.take() {
            self.camera = camera;
        }
        self.focused_key = self.saved_focus.take();
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
        self.needs_fit = true;
        cx.notify();
    }

    fn deploy(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let existing = workspace.items_of_type::<PlanGraph>(cx).next();
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
        } else {
            let workspace_handle = cx.entity().downgrade();
            let item = cx.new(|cx| PlanGraph::new(workspace_handle, window, cx));
            workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
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
            .child(Button::new("graph-history", "History").on_click(
                cx.listener(|this: &mut PlanGraph, _: &ClickEvent, _, cx| this.toggle_history(cx)),
            ))
            .child(Button::new("graph-zoom-out", "−").on_click(cx.listener(
                |this: &mut PlanGraph, _: &ClickEvent, _, cx| {
                    this.camera = this.camera.zoomed(0.8);
                    cx.notify();
                },
            )))
            .child(Button::new("graph-zoom-in", "+").on_click(cx.listener(
                |this: &mut PlanGraph, _: &ClickEvent, _, cx| {
                    this.camera = this.camera.zoomed(1.25);
                    cx.notify();
                },
            )))
            .child(Button::new("graph-fit", "Fit").on_click(cx.listener(
                |this: &mut PlanGraph, _: &ClickEvent, window, cx| {
                    this.fit_camera(window);
                    cx.notify();
                },
            )))
            .child(
                Label::new("single-click expands · double-click opens · Enter opens focused")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }

    fn single_click(&mut self, node: &WheelNode, window: &mut Window, cx: &mut Context<Self>) {
        match node.kind {
            WheelKind::Root => {
                let expanding = !self.expanded.contains("root");
                if expanding {
                    self.expanded.insert("root".to_string());
                } else {
                    self.expanded.clear();
                }
                if expanding {
                    self.fit_camera_to("root", window);
                } else {
                    self.fit_whole_centered_on("root", window);
                }
            }
            WheelKind::Category
            | WheelKind::Subcategory
            | WheelKind::History
            | WheelKind::SliceGroup => {
                toggle(&mut self.expanded, &node.key);
                if self.expanded.contains(&node.key) {
                    self.fit_camera_to(&node.key, window);
                } else {
                    self.fit_whole_centered_on(&node.key, window);
                }
            }
            WheelKind::Plan => {
                if node.expandable {
                    toggle(&mut self.expanded, &node.key);
                    if self.expanded.contains(&node.key) {
                        self.fit_camera_to(&node.key, window);
                    } else {
                        self.fit_whole_centered_on(&node.key, window);
                    }
                } else if let Some(plan_id) = node.plan_id.clone() {
                    self.open_plan(plan_id, window, cx);
                    return;
                }
            }
            WheelKind::Slice => {
                if let Some(plan_id) = node.plan_id.clone() {
                    self.open_plan(plan_id, window, cx);
                    return;
                }
            }
        }
        self.focused_key = Some(node.key.clone());
        cx.notify();
    }

    fn open_focused(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.focused_key.clone() else {
            return;
        };
        if let Some(plan_id) = key.strip_prefix("plan:") {
            self.open_plan(plan_id.to_string(), window, cx);
        } else if let Some(plan_id) = key.strip_prefix("slice:") {
            self.open_plan(plan_id.to_string(), window, cx);
        } else if let Some(plan_id) = key.strip_prefix("hist:") {
            self.open_plan(plan_id.to_string(), window, cx);
        }
    }

    fn render_wheel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let layout = self.current_layout();
        if self.needs_fit {
            self.camera = Camera::fit(layout.extent, self.pane_size(window));
            self.needs_fit = false;
        }
        let camera = self.camera;
        let pane = self.pane_size(window);
        let spoke_color = cx.theme().colors().border;
        let spokes = layout.spokes.clone();
        let wheel_bounds = self.wheel_bounds.clone();
        let notify = cx.weak_entity();

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(
                cx.listener(|this: &mut PlanGraph, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "enter" {
                        this.open_focused(window, cx);
                    }
                }),
            )
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .overflow_hidden()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut PlanGraph, event: &MouseDownEvent, _, cx| {
                            this.suppress_click = false;
                            this.drag = Some(DragState {
                                start: event.position,
                                start_pan: (this.camera.pan_x, this.camera.pan_y),
                                moved: false,
                            });
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(
                        |this: &mut PlanGraph, event: &MouseMoveEvent, _, cx| {
                            if this.drag.is_none() {
                                return;
                            }
                            if event.pressed_button != Some(MouseButton::Left) {
                                this.drag = None;
                                return;
                            }
                            let Some(drag) = this.drag.as_mut() else {
                                return;
                            };
                            let dx = f32::from(event.position.x - drag.start.x);
                            let dy = f32::from(event.position.y - drag.start.y);
                            if dx.abs() + dy.abs() > 3.0 {
                                drag.moved = true;
                            }
                            let (start_x, start_y) = drag.start_pan;
                            let moved = drag.moved;
                            this.camera.pan_x = start_x + dx;
                            this.camera.pan_y = start_y + dy;
                            this.suppress_click = moved;
                            cx.notify();
                        },
                    ))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this: &mut PlanGraph, _: &MouseUpEvent, _, cx| {
                            this.drag = None;
                            cx.notify();
                        }),
                    )
                    .on_scroll_wheel(cx.listener(Self::handle_wheel_zoom))
                    // `gpui::canvas` has no intrinsic size, so it must be given
                    // one explicitly; otherwise its bounds are 0x0 at the pane's
                    // top-left and the spokes lose the node frame's centre.
                    .child(
                        div().absolute().inset_0().child(
                            gpui::canvas(
                                |_, _, _| {},
                                move |bounds, _, window, cx| {
                                    if wheel_bounds.replace(Some(bounds)) != Some(bounds) {
                                        notify.update(cx, |_, cx| cx.notify()).ok();
                                    }
                                    // Same transform the nodes use, so a spoke always
                                    // ends exactly on its node's centre.
                                    let origin = bounds.origin;
                                    for spoke in &spokes {
                                        let from = origin
                                            + camera.wheel_to_pane(spoke.x1, spoke.y1, bounds.size);
                                        let to = origin
                                            + camera.wheel_to_pane(spoke.x2, spoke.y2, bounds.size);
                                        let mut builder = gpui::PathBuilder::stroke(px(1.0));
                                        builder.move_to(from);
                                        builder.line_to(to);
                                        if let Ok(path) = builder.build() {
                                            window.paint_path(path, spoke_color);
                                        }
                                    }
                                },
                            )
                            .size_full(),
                        ),
                    )
                    .child(
                        div().absolute().inset_0().children(
                            layout
                                .nodes
                                .iter()
                                .map(|node| self.render_wheel_node(node, camera, pane, cx)),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_wheel_node(
        &self,
        node: &WheelNode,
        camera: Camera,
        pane: Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = node.key.clone();
        let kind = node.kind;
        let expandable = node.expandable;
        let state = node.state;
        let focused = self.focused_key.as_deref() == Some(node.key.as_str());
        let zoom = camera.zoom;
        let width = NODE_WIDTH * zoom;
        let height = NODE_HEIGHT * zoom;
        let center = camera.wheel_to_pane(node.x, node.y, pane);
        let left = f32::from(center.x) - width / 2.0;
        let top = f32::from(center.y) - height / 2.0;
        let font = (0.85 * zoom).clamp(0.45, 1.1);

        let border_color = if focused {
            cx.theme().colors().border_focused
        } else {
            cx.theme().colors().border_variant
        };
        let background = if matches!(kind, WheelKind::Root) {
            cx.theme().colors().element_selected
        } else {
            cx.theme().colors().element_background
        };
        let glyph = node_shape(state);
        let glyph_color = node_color(state);
        let secondary = self.node_secondary(node);

        let click_node = node.clone();
        div()
            .id(ElementId::Name(node.key.clone().into()))
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(width))
            .h(px(height))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px_1()
            .rounded_lg()
            .border_1()
            .border_color(border_color)
            .bg(background)
            .cursor_pointer()
            .when(focused, |this| this.border_2())
            .on_hover(
                cx.listener(move |this: &mut PlanGraph, hovered: &bool, _, cx| {
                    if *hovered {
                        this.focused_key = Some(key.clone());
                        cx.notify();
                    }
                }),
            )
            .on_click(cx.listener(
                move |this: &mut PlanGraph, event: &ClickEvent, window, cx| {
                    if this.suppress_click {
                        this.suppress_click = false;
                        return;
                    }
                    if event.click_count() >= 2 {
                        if let Some(plan_id) = click_node.plan_id.clone() {
                            this.open_plan(plan_id, window, cx);
                        }
                    } else {
                        this.single_click(&click_node, window, cx);
                    }
                },
            ))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        Label::new(glyph)
                            .size(LabelSize::Custom(Rems(font)))
                            .color(glyph_color),
                    )
                    .child(
                        Label::new(node.label.clone())
                            .size(LabelSize::Custom(Rems(font)))
                            .color(Color::Default),
                    ),
            )
            .when(zoom > 0.45 && secondary.is_some(), |this| {
                this.child(
                    Label::new(secondary.unwrap_or_default())
                        .size(LabelSize::Custom(Rems((font * 0.82).max(0.4))))
                        .color(Color::Muted),
                )
            })
            .when(expandable && node.slice_count == 0, |this| {
                this.child(
                    Label::new(if node.expanded { "−" } else { "+" })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(node.slice_count > 0, |this| {
                this.child(
                    Label::new(format!("slices ({})", node.slice_count))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn node_secondary(&self, node: &WheelNode) -> Option<String> {
        let plan_id = node.plan_id.as_deref()?;
        let plan = self.plans.iter().find(|plan| plan.id == plan_id)?;
        if let Some(next) = plan.current_node.as_deref() {
            return Some(format!("next: {next}"));
        }
        plan.summary
            .as_deref()
            .map(|summary| summary.chars().take(60).collect::<String>())
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

        v_flex()
            .id(ElementId::Name("plan-page".into()))
            .size_full()
            .p_4()
            .gap_3()
            .overflow_y_scroll()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(can_go_back, |this| {
                        this.child(Button::new("page-back", "← Back").on_click(cx.listener(
                            |this: &mut PlanGraph, _: &ClickEvent, window, cx| {
                                this.back(window, cx)
                            },
                        )))
                    })
                    .child(
                        Button::new("page-graph", "Back to graph").on_click(cx.listener(
                            |this: &mut PlanGraph, _: &ClickEvent, _, cx| this.back_to_graph(cx),
                        )),
                    )
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
                    .gap_3()
                    .flex_wrap()
                    .child(meta("id", &page.id))
                    .when_some(page.repo.clone(), |this, repo| {
                        this.child(meta("repo", &repo))
                    })
                    .when_some(page.stage.clone(), |this, stage| {
                        this.child(meta("stage", &stage))
                    })
                    .when_some(page.status.clone(), |this, status| {
                        this.child(meta("status", &status))
                    })
                    .when_some(page.current_node.clone(), |this, node| {
                        this.child(meta("current", &node))
                    }),
            )
            .child(
                div().w_full().child(
                    Label::new(summary)
                        .size(LabelSize::Small)
                        .color(Color::Default),
                ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(cx.theme().colors().element_background)
                    .child(
                        Label::new(decision_text(&page.decision))
                            .size(LabelSize::Small)
                            .color(decision_color),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .when(!page.related.is_empty(), |this| {
                        this.child(chip_row("Related", &page.related, cx))
                    })
                    .when(!page.blocking.is_empty(), |this| {
                        this.child(chip_row("Blocked by", &page.blocking, cx))
                    })
                    .when(!page.slices.is_empty(), |this| {
                        this.child(chip_row("Slices", &page.slices, cx))
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(page.dev_flow.iter().map(|node| {
                        let (glyph, color) = dev_flow_visual(node);
                        Label::new(format!("{glyph} {}", node.node))
                            .size(LabelSize::XSmall)
                            .color(color)
                    })),
            )
            .when(page.debugger_scoped, |this| {
                this.child(
                    Label::new("Debugger loop (stage → test → verify → restart → run → fix)")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(Label::new("Threads for this plan").size(LabelSize::Small))
                            .when(self.threads.is_empty(), |this| {
                                this.child(
                                    Label::new("No threads started from this plan yet.")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            })
                            .children(self.threads.iter().map(|thread| {
                                let session = thread.session_id.clone();
                                div()
                                    .id(ElementId::Name(
                                        format!("plan-thread:{}", thread.session_id).into(),
                                    ))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .hover(|this| this.bg(cx.theme().colors().element_hover))
                                    .on_click(cx.listener(
                                        move |this: &mut PlanGraph, _: &ClickEvent, window, cx| {
                                            this.open_thread(session.clone(), window, cx)
                                        },
                                    ))
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                Label::new(thread.title.clone())
                                                    .size(LabelSize::Small),
                                            )
                                            .child(
                                                Label::new(
                                                    thread
                                                        .updated_at
                                                        .format("%Y-%m-%d %H:%M")
                                                        .to_string(),
                                                )
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                            ),
                                    )
                            })),
                    )
                    .child(
                        v_flex()
                            .w(px(340.0))
                            .gap_2()
                            .child(
                                div()
                                    .w_full()
                                    .px_2()
                                    .py_1()
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
            )
            .into_any_element()
    }
}

fn meta(label: &str, value: &str) -> AnyElement {
    h_flex()
        .gap_1()
        .child(
            Label::new(label.to_string())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(Label::new(value.to_string()).size(LabelSize::XSmall))
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
        .gap_1()
        .child(
            Label::new(title.to_string())
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            h_flex()
                .gap_1()
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
                        .px_2()
                        .py_1()
                        .rounded_full()
                        .border_1()
                        .border_color(cx.theme().colors().border_variant)
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors().element_hover))
                        .on_click(cx.listener(
                            move |this: &mut PlanGraph, _: &ClickEvent, window, cx| {
                                this.open_plan(id.clone(), window, cx)
                            },
                        ))
                        .child(Label::new(label).size(LabelSize::XSmall))
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
        if self.page.is_some() {
            self.render_page(window, cx).into_any_element()
        } else {
            self.render_wheel(window, cx).into_any_element()
        }
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
    use gpui::size;
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
        let collapsed = build_visible_wheel(
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

        let open = build_visible_wheel(
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
        let root = build_visible_wheel(
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
        let root_open = build_visible_wheel(
            &taxonomy(),
            &[],
            &[],
            &HashMap::new(),
            &expanded(&["root", "cat:feature"]),
            false,
        );
        assert_eq!(find(&root_open, "cat:feature").unwrap().children.len(), 1);
        let root_closed = build_visible_wheel(
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
        let root = build_visible_wheel(
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

        let root = build_visible_wheel(
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
        let hidden = build_visible_wheel(
            &taxonomy(),
            &plans,
            &[],
            &HashMap::new(),
            &expanded(&["root"]),
            false,
        );
        assert!(find(&hidden, "history").is_none());

        let shown = build_visible_wheel(
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
    fn layout_places_nodes_on_distinct_rings() {
        let plans = vec![
            plan("p1", "Plan one", Some("feature/agent-core"), None),
            plan("p2", "Plan two", Some("bug/threads"), None),
        ];
        let root = build_visible_wheel(
            &taxonomy(),
            &plans,
            &[],
            &HashMap::new(),
            &expanded(&[
                "root",
                "cat:feature",
                "cat:bug",
                "sub:feature/agent-core",
                "sub:bug/threads",
            ]),
            false,
        );
        let layout = layout_wheel(&root);
        assert_eq!(layout.nodes.len(), 7);
        let root_node = layout.nodes.iter().find(|n| n.key == "root").unwrap();
        assert_eq!((root_node.x, root_node.y), (0.0, 0.0));
        let categories: Vec<&WheelNode> = layout.nodes.iter().filter(|n| n.depth == 1).collect();
        assert_eq!(categories.len(), 2);
        assert_ne!(categories[0].x, categories[1].x);
        assert!(layout.extent > 0.0);
        assert_eq!(layout.spokes.len(), layout.nodes.len() - 1);
    }

    #[test]
    fn every_ring_is_round_and_its_neighbours_clear_each_other() {
        // The shape of the live store: a few large categories and several small
        // ones, which used to leave neighbours (Bug next to QOL) overlapping.
        let taxonomy = vec![
            category("feature", None, "Feature", 100),
            category("process", None, "Process", 150),
            category("bug", None, "Bug", 200),
            category("qol", None, "QOL", 210),
            category("build", None, "Build and infra", 220),
            category("browser", None, "Browser and preview", 230),
        ];
        let mut plans = Vec::new();
        for index in 0..12 {
            plans.push(plan(
                &format!("feature-{index}"),
                "Feature plan",
                Some("feature"),
                None,
            ));
        }
        for index in 0..7 {
            plans.push(plan(
                &format!("process-{index}"),
                "Process plan",
                Some("process"),
                None,
            ));
        }
        plans.push(plan("bug-0", "Bug plan", Some("bug"), None));
        plans.push(plan("qol-0", "QOL plan", Some("qol"), None));
        plans.push(plan("build-0", "Build plan", Some("build"), None));
        plans.push(plan("browser-0", "Browser plan", Some("browser"), None));

        let root = build_visible_wheel(
            &taxonomy,
            &plans,
            &[],
            &HashMap::new(),
            &expanded(&["root"]),
            false,
        );
        let layout = layout_wheel(&root);

        let mut depths: Vec<usize> = layout.nodes.iter().map(|node| node.depth).collect();
        depths.sort_unstable();
        depths.dedup();
        for depth in depths {
            let mut ring: Vec<&WheelNode> = layout
                .nodes
                .iter()
                .filter(|node| node.depth == depth)
                .collect();
            if depth == 0 {
                assert_eq!(ring.len(), 1, "the root sits alone at the centre");
                continue;
            }
            // One radius per ring: no node may sit inboard or outboard of its
            // siblings, which is what made the wheel look lopsided.
            let radius = (ring[0].x.powi(2) + ring[0].y.powi(2)).sqrt();
            for node in &ring {
                let node_radius = (node.x.powi(2) + node.y.powi(2)).sqrt();
                assert!(
                    (node_radius - radius).abs() < 0.5,
                    "{} sits at {node_radius} on a ring of {radius}",
                    node.label
                );
            }
            // Neighbours on a ring must clear each other, or the boxes overlap.
            ring.sort_by(|a, b| {
                a.angle
                    .partial_cmp(&b.angle)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for pair in ring.windows(2) {
                let distance =
                    ((pair[1].x - pair[0].x).powi(2) + (pair[1].y - pair[0].y).powi(2)).sqrt();
                assert!(
                    distance >= NODE_WIDTH,
                    "depth {depth}: {} and {} are only {distance} apart",
                    pair[0].label,
                    pair[1].label
                );
            }
        }
    }

    #[test]
    fn fitting_a_subtree_centres_the_clicked_node() {
        let pane = size(px(1200.0), px(800.0));
        // The clicked node sits off-centre in its own subtree; it must land on the
        // pane's centre while the whole subtree still fits around it.
        let camera = Camera::fit_around(400.0, 0.0, 300.0, -100.0, 500.0, 100.0, pane);
        let anchor = camera.wheel_to_pane(400.0, 0.0, pane);
        assert!(
            (f32::from(anchor.x) - 600.0).abs() < 0.5,
            "clicked node centred x: {}",
            anchor.x
        );
        assert!(
            (f32::from(anchor.y) - 400.0).abs() < 0.5,
            "clicked node centred y: {}",
            anchor.y
        );
        for (x, y) in [(300.0, -100.0), (500.0, 100.0)] {
            let edge = camera.wheel_to_pane(x, y, pane);
            assert!(
                f32::from(edge.x) > 0.0 && f32::from(edge.x) < 1200.0,
                "x inside pane: {}",
                edge.x
            );
            assert!(
                f32::from(edge.y) > 0.0 && f32::from(edge.y) < 800.0,
                "y inside pane: {}",
                edge.y
            );
        }
    }

    #[test]
    fn recentring_keeps_the_zoom() {
        let pane = size(px(1200.0), px(800.0));
        let camera = Camera::fit_around(0.0, 0.0, 0.0, 0.0, 100.0, 100.0, pane);
        let zoom = camera.zoom;
        let camera = camera.centered_on(600.0, 300.0, pane);
        let centered = camera.wheel_to_pane(600.0, 300.0, pane);
        assert!((f32::from(centered.x) - 600.0).abs() < 0.5);
        assert!((f32::from(centered.y) - 400.0).abs() < 0.5);
        assert_eq!(camera.zoom, zoom, "recentring must not change the scale");
    }

    #[test]
    fn a_clicked_node_lands_exactly_on_the_pane_centre() {
        let pane = size(px(1310.0), px(776.0));
        for anchor in [(0.0, 0.0), (412.5, -233.0), (-800.0, 640.0)] {
            let camera = Camera::fit_around(
                anchor.0,
                anchor.1,
                anchor.0 - 50.0,
                anchor.1 - 50.0,
                anchor.0 + 900.0,
                anchor.1 + 900.0,
                pane,
            );
            let landed = camera.wheel_to_pane(anchor.0, anchor.1, pane);
            assert!(
                (f32::from(landed.x) - 655.0).abs() < 0.5,
                "anchor {anchor:?} landed at x {}",
                landed.x
            );
            assert!(
                (f32::from(landed.y) - 388.0).abs() < 0.5,
                "anchor {anchor:?} landed at y {}",
                landed.y
            );
        }
    }

    #[test]
    fn wheel_zoom_keeps_the_point_under_the_cursor() {
        let pane = size(px(1200.0), px(800.0));
        let camera = Camera::fit_around(0.0, 0.0, -400.0, -400.0, 400.0, 400.0, pane);
        let cursor = point(px(900.0), px(250.0));
        let before = {
            // The wheel coordinate currently under the cursor.
            let x = (f32::from(cursor.x) - 600.0 - camera.pan_x) / camera.zoom;
            let y = (f32::from(cursor.y) - 400.0 - camera.pan_y) / camera.zoom;
            (x, y)
        };
        let zoomed = camera.zoom_about(camera.zoom * 1.6, cursor, pane);
        let after = zoomed.wheel_to_pane(before.0, before.1, pane);
        assert!((f32::from(after.x) - f32::from(cursor.x)).abs() < 0.5);
        assert!((f32::from(after.y) - f32::from(cursor.y)).abs() < 0.5);
        assert!(zoomed.zoom > camera.zoom);
    }

    #[test]
    fn subtree_keys_collects_only_the_clicked_branch() {
        let plans = vec![
            plan("p1", "Plan one", Some("feature/agent-core"), None),
            plan("p2", "Plan two", Some("bug/threads"), None),
        ];
        let root = build_visible_wheel(
            &taxonomy(),
            &plans,
            &[],
            &HashMap::new(),
            &expanded(&[
                "root",
                "cat:feature",
                "cat:bug",
                "sub:feature/agent-core",
                "sub:bug/threads",
            ]),
            false,
        );
        let feature = subtree_keys(&root, "cat:feature").expect("feature subtree");
        assert!(feature.contains(&"cat:feature".to_string()));
        assert!(feature.contains(&"sub:feature/agent-core".to_string()));
        assert!(feature.contains(&"plan:p1".to_string()));
        assert!(!feature.contains(&"cat:bug".to_string()));
        assert!(!feature.contains(&"plan:p2".to_string()));
        assert!(subtree_keys(&root, "cat:missing").is_none());
    }

    #[test]
    fn collapsing_fits_the_whole_wheel_around_the_clicked_node() {
        let taxonomy = vec![
            category("feature", None, "Feature", 100),
            category("process", None, "Process", 150),
            category("bug", None, "Bug", 200),
            category("qol", None, "QOL", 210),
            category("build", None, "Build and infra", 220),
            category("browser", None, "Browser and preview", 230),
        ];
        let plans = vec![plan("p0", "Plan zero", Some("feature"), None)];
        let root = build_visible_wheel(
            &taxonomy,
            &plans,
            &[],
            &HashMap::new(),
            &expanded(&["root", "cat:feature"]),
            false,
        );
        let layout = layout_wheel(&root);
        let anchor = layout.nodes.iter().find(|n| n.key == "cat:bug").unwrap();
        let mut min_x = anchor.x;
        let mut min_y = anchor.y;
        let mut max_x = anchor.x;
        let mut max_y = anchor.y;
        for node in &layout.nodes {
            min_x = min_x.min(node.x);
            min_y = min_y.min(node.y);
            max_x = max_x.max(node.x);
            max_y = max_y.max(node.y);
        }
        let pane = size(px(1310.0), px(776.0));
        let camera = Camera::fit_around(anchor.x, anchor.y, min_x, min_y, max_x, max_y, pane);
        let landed = camera.wheel_to_pane(anchor.x, anchor.y, pane);
        assert!((f32::from(landed.x) - 655.0).abs() < 0.5);
        assert!((f32::from(landed.y) - 388.0).abs() < 0.5);
        for node in &layout.nodes {
            let point = camera.wheel_to_pane(node.x, node.y, pane);
            assert!(
                f32::from(point.x) > 0.0 && f32::from(point.x) < 1310.0,
                "{} inside x: {}",
                node.label,
                point.x
            );
            assert!(
                f32::from(point.y) > 0.0 && f32::from(point.y) < 776.0,
                "{} inside y: {}",
                node.label,
                point.y
            );
        }
    }

    #[test]
    fn camera_fit_is_reversible() {
        let fitted = Camera::fit(2000.0, size(px(1200.0), px(800.0)));
        let zoomed = fitted.zoomed(0.5);
        assert!(zoomed.zoom < fitted.zoom);
        // Restoring the saved camera returns the same camera exactly.
        let restored = fitted;
        assert_eq!(restored, fitted);
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
}
