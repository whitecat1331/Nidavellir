//! Read-only bottom-dock panel that renders the dev-flow "fleet": every plan
//! with a `progress.json` traversal and every active workstream claim, joined
//! into one "where is everything now" view.
//!
//! It reads the harness files live — `WORKSTREAMS.json` and
//! `plans/**/*.progress.json` — from the first open workspace root that
//! contains `WORKSTREAMS.json`, so it needs no configuration and stores nothing
//! (the projection is derived, never a second copy). "Stuck" is derived from a
//! heartbeat/stall check, not a status string: the panel observes, it does not
//! drive, so the Layer-C stop predicates stay in the 1.23d coordinator.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use gpui::{
    actions, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Render,
    Task, WeakEntity, Window,
};
use ui::{Button, ButtonSize, IconName, Label, LabelSize, prelude::*};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(fleet, [ToggleFleetPanel]);

const FLEET_PANEL_KEY: &str = "FleetPanel";

/// The dev-flow route and its human gates — the same readiness table the 1.23d
/// coordinator uses, reduced to what a read-only view needs.
const ROUTE: [&str; 11] = [
    "plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept", "merge",
    "release", "complete",
];

/// A plan with no activity for this long is visibly stalled, not "moving".
const STALL_SECONDS: i64 = 15 * 60;

/// A claim whose heartbeat is older than this is flagged stale (mirrors
/// `ledger.WIP_STALE_HOURS` in the harness).
const STALE_CLAIM_HOURS: f64 = 24.0;

// ---- file schemas. Every field defaults, so a partial file still parses. ----

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct WorkstreamsFile {
    claims: Vec<Claim>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Claim {
    id: String,
    holder: String,
    repo: String,
    branch: Option<String>,
    heartbeat: Option<String>,
    status: String,
    note: Option<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ProgressFile {
    plan: Option<String>,
    repo: Option<String>,
    current: Option<String>,
    done: Vec<DoneEntry>,
    active: Option<ActiveNode>,
    blocker: Option<Blocker>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct DoneEntry {
    node: String,
    attempts: Vec<Attempt>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Attempt {
    cost: Option<NodeCost>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct NodeCost {
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ActiveNode {
    node: Option<String>,
    executor: Option<String>,
    started_at: Option<String>,
    last_activity_at: Option<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Blocker {
    reason: Option<String>,
}

// ---- derived rows ----

struct PlanRow {
    plan: String,
    phase: String,
    state: String,
    breaker: String,
    spent_tokens: u64,
    agent: Option<String>,
    blocked_on_you: bool,
}

struct AgentRow {
    holder: String,
    repo: String,
    branch: Option<String>,
    liveness: String,
    age_seconds: Option<f64>,
    note: Option<String>,
}

// ---- derivation ----

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

fn next_ready(progress: &ProgressFile) -> Option<&'static str> {
    let done: HashSet<&str> = progress
        .done
        .iter()
        .map(|entry| entry.node.as_str())
        .collect();
    ROUTE
        .iter()
        .copied()
        .find(|node| !done.contains(node) && prerequisites(node).iter().all(|p| done.contains(p)))
}

fn spent_tokens(progress: &ProgressFile) -> u64 {
    progress
        .done
        .iter()
        .flat_map(|entry| &entry.attempts)
        .filter_map(|attempt| attempt.cost.as_ref())
        .map(|cost| cost.input_tokens + cost.output_tokens)
        .sum()
}

/// (state label, blocked_on_you, breaker). The breaker maps to the Ralph
/// circuit-breaker taxonomy: closed (moving/ready/done), half-open (waiting on
/// a human), open (stalled or blocked). A completed plan is closed — the
/// circuit finished, it did not open.
fn derive_state(progress: &ProgressFile, now: DateTime<Utc>) -> (String, bool, String) {
    if progress.current.as_deref() == Some("complete") {
        return ("complete".to_string(), false, "closed".to_string());
    }

    if let Some(blocker) = &progress.blocker {
        let reason = blocker.reason.as_deref().unwrap_or("unknown");
        return (
            format!("blocked on you (resume — {reason})"),
            true,
            "open".to_string(),
        );
    }

    if let Some(active) = &progress.active {
        let last = active
            .last_activity_at
            .as_deref()
            .or(active.started_at.as_deref());
        let node = active.node.as_deref().unwrap_or("active");
        let idle = last
            .and_then(parse_time)
            .map(|time| (now - time).num_seconds())
            .unwrap_or(i64::MAX);
        if idle > STALL_SECONDS {
            return (format!("stuck (stalled — {node})"), false, "open".to_string());
        }
        return (format!("moving ({node})"), false, "closed".to_string());
    }

    match next_ready(progress) {
        Some(node) if is_human_gate(node) => (
            format!("blocked on you (approve {node})"),
            true,
            "half-open".to_string(),
        ),
        Some(node) => (format!("ready → {node}"), false, "closed".to_string()),
        None => ("paused".to_string(), false, "half-open".to_string()),
    }
}

fn claim_liveness(claim: &Claim, now: DateTime<Utc>) -> (String, Option<f64>) {
    match claim.heartbeat.as_deref().and_then(parse_time) {
        None => ("missing-heartbeat".to_string(), None),
        Some(heartbeat) => {
            let age = (now - heartbeat).num_seconds() as f64;
            if age > STALE_CLAIM_HOURS * 3600.0 {
                ("stale".to_string(), Some(age))
            } else {
                ("fresh".to_string(), Some(age))
            }
        }
    }
}

// ---- loading ----

fn collect_progress_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_progress_files(&path, out);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".progress.json"))
        {
            out.push(path);
        }
    }
}

fn load_plans(root: &Path, now: DateTime<Utc>) -> Vec<PlanRow> {
    let mut paths = Vec::new();
    collect_progress_files(&root.join("plans"), &mut paths);
    paths.sort();

    let mut rows: Vec<PlanRow> = paths
        .into_iter()
        .filter_map(|path| {
            let contents = std::fs::read_to_string(&path).ok()?;
            let progress: ProgressFile = serde_json::from_str(&contents).ok()?;
            Some(progress)
        })
        .map(|progress| {
            let (state, blocked_on_you, breaker) = derive_state(&progress, now);
            let agent = progress
                .active
                .as_ref()
                .and_then(|active| active.executor.clone());
            PlanRow {
                plan: progress.plan.clone().unwrap_or_else(|| "?".to_string()),
                phase: progress.current.clone().unwrap_or_else(|| "—".to_string()),
                state,
                breaker,
                spent_tokens: spent_tokens(&progress),
                agent,
                blocked_on_you,
            }
        })
        .collect();

    rows.sort_by(|a, b| {
        let rank = |row: &PlanRow| {
            if row.phase == "complete" {
                2
            } else if row.blocked_on_you {
                0
            } else {
                1
            }
        };
        rank(a).cmp(&rank(b)).then(a.plan.cmp(&b.plan))
    });
    rows
}

fn load_agents(root: &Path, now: DateTime<Utc>) -> Vec<AgentRow> {
    let Ok(contents) = std::fs::read_to_string(root.join("WORKSTREAMS.json")) else {
        return Vec::new();
    };
    let Ok(file) = serde_json::from_str::<WorkstreamsFile>(&contents) else {
        return Vec::new();
    };
    file.claims
        .into_iter()
        .filter(|claim| claim.status == "active")
        .map(|claim| {
            let (liveness, age_seconds) = claim_liveness(&claim, now);
            AgentRow {
                holder: claim.holder,
                repo: claim.repo,
                branch: claim.branch,
                liveness,
                age_seconds,
                note: claim.note,
            }
        })
        .collect()
}

fn discover_harness_root(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    workspace.worktrees(cx).find_map(|worktree| {
        let root = worktree.read(cx).abs_path();
        let candidate = root.as_ref().to_path_buf();
        candidate
            .join("WORKSTREAMS.json")
            .exists()
            .then_some(candidate)
    })
}

// ---- panel ----

pub struct FleetPanel {
    focus_handle: FocusHandle,
    harness_root: Option<PathBuf>,
    plans: Vec<PlanRow>,
    agents: Vec<AgentRow>,
    last_error: Option<String>,
}

impl Focusable for FleetPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for FleetPanel {}

impl FleetPanel {
    pub fn new(
        workspace: &Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let harness_root = discover_harness_root(workspace, cx);
        cx.new(|cx| {
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                harness_root,
                plans: Vec::new(),
                agents: Vec::new(),
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
                FleetPanel::new(workspace, window, cx)
            })
        })
    }

    fn refresh(&mut self) {
        let Some(root) = self.harness_root.clone() else {
            self.plans.clear();
            self.agents.clear();
            self.last_error = Some(
                "No harness root found — open the zed-debugger-demo folder \
                 (it contains WORKSTREAMS.json) as a workspace root."
                    .to_string(),
            );
            return;
        };
        let now = Utc::now();
        self.plans = load_plans(&root, now);
        self.agents = load_agents(&root, now);
        self.last_error = None;
    }
}

fn render_plan_row(row: &PlanRow) -> impl IntoElement {
    let agent = row.agent.clone().unwrap_or_else(|| "—".to_string());
    h_flex()
        .gap_2()
        .child(Label::new(row.plan.clone()).weight(FontWeight::SEMIBOLD))
        .child(Label::new(format!("phase={}", row.phase)).size(LabelSize::Small))
        .child(Label::new(row.state.clone()).size(LabelSize::Small))
        .child(Label::new(format!("breaker={}", row.breaker)).size(LabelSize::Small))
        .child(Label::new(format!("agent={agent}")).size(LabelSize::Small))
        .child(Label::new(format!("spent={}", row.spent_tokens)).size(LabelSize::Small))
}

fn render_agent_row(row: &AgentRow) -> impl IntoElement {
    let age = match row.age_seconds {
        None => "—".to_string(),
        Some(seconds) if seconds < 60.0 => format!("{seconds:.0}s"),
        Some(seconds) if seconds < 3600.0 => format!("{:.0}m", seconds / 60.0),
        Some(seconds) if seconds < 86400.0 => format!("{:.0}h", seconds / 3600.0),
        Some(seconds) => format!("{:.0}d", seconds / 86400.0),
    };
    let branch = row.branch.as_deref().unwrap_or("—");
    h_flex()
        .gap_2()
        .child(Label::new(row.holder.clone()).weight(FontWeight::SEMIBOLD))
        .child(Label::new(format!("repo={}", row.repo)).size(LabelSize::Small))
        .child(Label::new(format!("branch={branch}")).size(LabelSize::Small))
        .child(Label::new(format!("{} ({age})", row.liveness)).size(LabelSize::Small))
        .when_some(row.note.clone(), |element, note| {
            element.child(Label::new(note).size(LabelSize::Small))
        })
}

impl Render for FleetPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let refresh = cx.listener(|this: &mut FleetPanel, _, _, cx| {
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
                    .child(Label::new("Fleet"))
                    .child(
                        Button::new("fleet-refresh", "Refresh")
                            .size(ButtonSize::Compact)
                            .on_click(refresh),
                    ),
            )
            .child(
                v_flex()
                    .size_full()
                    .px_2()
                    .gap_1()
                    .when_some(self.last_error.clone(), |element, error| {
                        element.child(Label::new(error))
                    })
                    .child(
                        Label::new(format!("Plans ({})", self.plans.len()))
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .when(self.plans.is_empty(), |element| {
                        element.child(Label::new("(no traversals)").size(LabelSize::Small))
                    })
                    .children(self.plans.iter().map(render_plan_row))
                    .child(
                        Label::new(format!("Agents ({})", self.agents.len()))
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .when(self.agents.is_empty(), |element| {
                        element.child(Label::new("(none active)").size(LabelSize::Small))
                    })
                    .children(self.agents.iter().map(render_agent_row)),
            )
    }
}

impl Panel for FleetPanel {
    fn persistent_name() -> &'static str {
        "FleetPanel"
    }

    fn panel_key() -> &'static str {
        FLEET_PANEL_KEY
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
        px(240.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::GitBranch)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Fleet")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFleetPanel)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

/// Registers the panel toggle action on every new workspace.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(
            |workspace: &mut Workspace, _: &ToggleFleetPanel, window, cx| {
                workspace.toggle_panel_focus::<FleetPanel>(window, cx);
            },
        );
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-16T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn attempt(input: u64, output: u64) -> Attempt {
        Attempt {
            cost: Some(NodeCost {
                input_tokens: input,
                output_tokens: output,
            }),
        }
    }

    fn done_entry(node: &str, attempts: Vec<Attempt>) -> DoneEntry {
        DoneEntry {
            node: node.to_string(),
            attempts,
        }
    }

    fn active_node(node: &str, last_activity_at: DateTime<Utc>) -> ActiveNode {
        ActiveNode {
            node: Some(node.to_string()),
            executor: Some("executor-a".to_string()),
            started_at: None,
            last_activity_at: Some(last_activity_at.to_rfc3339()),
        }
    }

    #[test]
    fn complete_plan_is_closed() {
        let progress = ProgressFile {
            current: Some("complete".to_string()),
            ..Default::default()
        };
        let (state, blocked, breaker) = derive_state(&progress, now());
        assert_eq!(state, "complete");
        assert!(!blocked);
        assert_eq!(breaker, "closed");
    }

    #[test]
    fn active_plan_moves() {
        let progress = ProgressFile {
            current: Some("implement".to_string()),
            active: Some(active_node("implement", now() - chrono::Duration::minutes(1))),
            ..Default::default()
        };
        let (state, blocked, breaker) = derive_state(&progress, now());
        assert_eq!(state, "moving (implement)");
        assert!(!blocked);
        assert_eq!(breaker, "closed");
    }

    #[test]
    fn stalled_plan_is_open() {
        let progress = ProgressFile {
            current: Some("implement".to_string()),
            active: Some(active_node("implement", now() - chrono::Duration::minutes(16))),
            ..Default::default()
        };
        let (state, blocked, breaker) = derive_state(&progress, now());
        assert_eq!(state, "stuck (stalled — implement)");
        assert!(!blocked);
        assert_eq!(breaker, "open");
    }

    #[test]
    fn human_gate_blocks_on_you() {
        let nodes = [
            "plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept",
        ];
        let done: Vec<DoneEntry> = nodes
            .iter()
            .map(|node| done_entry(node, vec![attempt(0, 0)]))
            .collect();
        let progress = ProgressFile {
            current: Some("accept".to_string()),
            done,
            ..Default::default()
        };
        let (state, blocked, breaker) = derive_state(&progress, now());
        assert_eq!(state, "blocked on you (approve merge)");
        assert!(blocked);
        assert_eq!(breaker, "half-open");
    }

    #[test]
    fn blocker_blocks_on_you() {
        let progress = ProgressFile {
            current: Some("implement".to_string()),
            blocker: Some(Blocker {
                reason: Some("waiting on secret rotation".to_string()),
            }),
            ..Default::default()
        };
        let (state, blocked, breaker) = derive_state(&progress, now());
        assert!(blocked);
        assert!(state.contains("resume"));
        assert_eq!(breaker, "open");
    }

    #[test]
    fn spent_tokens_sums_cost() {
        let progress = ProgressFile {
            done: vec![done_entry("implement", vec![attempt(100, 50), attempt(20, 5)])],
            ..Default::default()
        };
        assert_eq!(spent_tokens(&progress), 175);
    }

    #[test]
    fn claim_liveness_fresh_stale_missing() {
        let at = now();
        let fresh = Claim {
            heartbeat: Some((at - chrono::Duration::minutes(1)).to_rfc3339()),
            ..Default::default()
        };
        assert_eq!(claim_liveness(&fresh, at).0, "fresh");

        let stale = Claim {
            heartbeat: Some((at - chrono::Duration::hours(25)).to_rfc3339()),
            ..Default::default()
        };
        assert_eq!(claim_liveness(&stale, at).0, "stale");

        let missing = Claim {
            heartbeat: None,
            ..Default::default()
        };
        assert_eq!(claim_liveness(&missing, at).0, "missing-heartbeat");
    }
}
