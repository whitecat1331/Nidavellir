//! Read-only bottom-dock panel that renders the dev-flow "fleet": every plan
//! in the SQLite plan store and every active workstream claim, joined into one
//! "where is everything now" view.
//!
//! Plan state and evidence are read from the `plan_store` SQLite store (the
//! state-of-record the graph tool co-writes on every mutation) — not re-parsed
//! from `progress.json` / plan prose. Claims (the "agents" view) are synced from
//! `WORKSTREAMS.json` on refresh, because that file remains the harness's
//! authoritative claim ledger. "Stuck" is derived from a heartbeat/stall check,
//! not a status string: the panel observes, it does not drive, so the Layer-C
//! stop predicates stay in the 1.23d coordinator.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use gpui::{
    actions, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Render,
    Task, WeakEntity, Window,
};
use plan_flow::{is_human_gate, next_ready, STALL_SECONDS};
use plan_store::{ClaimRow, FleetPlanRow, PlanStore};
use ui::{Button, ButtonSize, IconName, Label, LabelSize, prelude::*};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(fleet, [ToggleFleetPanel]);

const FLEET_PANEL_KEY: &str = "FleetPanel";

/// A claim whose heartbeat is older than this is flagged stale (mirrors
/// `ledger.WIP_STALE_HOURS` in the harness).
const STALE_CLAIM_HOURS: f64 = 24.0;

// ---- `WORKSTREAMS.json` schema (the claims sync source only). ----

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

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

/// (state label, blocked_on_you, breaker). The breaker maps to the Ralph
/// circuit-breaker taxonomy: closed (moving/ready/done), half-open (waiting on
/// a human), open (stalled or blocked). A completed plan is closed — the
/// circuit finished, it did not open.
fn derive_state(plan: &FleetPlanRow, done_nodes: &[String], now: DateTime<Utc>) -> (String, bool, String) {
    if plan.current_node.as_deref() == Some("complete") {
        return ("complete".to_string(), false, "closed".to_string());
    }

    if let Some(reason) = &plan.blocked_reason {
        return (
            format!("blocked on you (resume — {reason})"),
            true,
            "open".to_string(),
        );
    }

    if let Some(active) = &plan.active_node {
        let idle = plan
            .heartbeat
            .as_deref()
            .and_then(parse_time)
            .map(|time| (now - time).num_seconds())
            .unwrap_or(i64::MAX);
        if idle > STALL_SECONDS {
            return (format!("stuck (stalled — {active})"), false, "open".to_string());
        }
        return (format!("moving ({active})"), false, "closed".to_string());
    }

    let done: HashSet<&str> = done_nodes.iter().map(|node| node.as_str()).collect();
    match next_ready(&done) {
        Some(node) if is_human_gate(node) => (
            format!("blocked on you (approve {node})"),
            true,
            "half-open".to_string(),
        ),
        Some(node) => (format!("ready → {node}"), false, "closed".to_string()),
        None => ("paused".to_string(), false, "half-open".to_string()),
    }
}

fn claim_liveness(heartbeat: Option<&str>, now: DateTime<Utc>) -> (String, Option<f64>) {
    match heartbeat.and_then(parse_time) {
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

fn load_plans(store: &PlanStore, now: DateTime<Utc>) -> Vec<PlanRow> {
    let mut rows: Vec<PlanRow> = store
        .fleet_plans()
        .unwrap_or_default()
        .into_iter()
        .map(|plan| {
            let done_nodes = store.done_nodes(&plan.id).unwrap_or_default();
            let (state, blocked_on_you, breaker) = derive_state(&plan, &done_nodes, now);
            PlanRow {
                plan: plan.name,
                phase: plan.current_node.unwrap_or_else(|| "—".to_string()),
                state,
                breaker,
                spent_tokens: plan.spent_tokens,
                agent: plan.holder,
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

fn load_agents(store: &PlanStore, now: DateTime<Utc>) -> Vec<AgentRow> {
    store
        .claims()
        .unwrap_or_default()
        .into_iter()
        .map(|claim| {
            let (liveness, age_seconds) = claim_liveness(claim.heartbeat.as_deref(), now);
            AgentRow {
                holder: claim.holder,
                repo: claim.repo.unwrap_or_default(),
                branch: claim.branch,
                liveness,
                age_seconds,
                note: claim.note,
            }
        })
        .collect()
}

/// Mirror the active `WORKSTREAMS.json` claims into the store. Best-effort:
/// `WORKSTREAMS.json` remains the authoritative claim ledger, and a failed upsert
/// only means the agents view is missing a row until the next refresh.
fn sync_claims(root: &Path, store: &PlanStore) {
    let Ok(contents) = std::fs::read_to_string(root.join("WORKSTREAMS.json")) else {
        return;
    };
    let Ok(file) = serde_json::from_str::<WorkstreamsFile>(&contents) else {
        return;
    };
    for claim in file.claims {
        if claim.status != "active" {
            continue;
        }
        let id = claim.id;
        let result = store.upsert_claim(&ClaimRow {
            id,
            holder: claim.holder,
            repo: Some(claim.repo),
            branch: claim.branch,
            heartbeat: claim.heartbeat,
            note: claim.note,
        });
        if let Err(error) = result {
            log::error!("[FLEET] failed to sync claim to the store: {error:#}");
        }
    }
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
        let store = match PlanStore::open_default() {
            Ok(store) => store,
            Err(error) => {
                self.plans.clear();
                self.agents.clear();
                self.last_error = Some(format!("Failed to open the plan store: {error:#}"));
                return;
            }
        };

        let now = Utc::now();
        if let Some(root) = self.harness_root.clone() {
            sync_claims(&root, &store);
        }
        self.plans = load_plans(&store, now);
        self.agents = load_agents(&store, now);
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

    fn store() -> PlanStore {
        PlanStore::open_in_memory().expect("open in-memory store")
    }

    fn snapshot(
        name: &str,
        current: Option<&str>,
        active: Option<&str>,
        heartbeat: Option<&str>,
        blocked: Option<&str>,
        done: &[&str],
        spent: u64,
    ) -> plan_store::GraphSnapshot {
        plan_store::GraphSnapshot {
            plan_id: name.to_string(),
            name: name.to_string(),
            repo: Some("zed".to_string()),
            current_node: current.map(|s| s.to_string()),
            active_node: active.map(|s| s.to_string()),
            holder: None,
            heartbeat: heartbeat.map(|s| s.to_string()),
            blocked_reason: blocked.map(|s| s.to_string()),
            spent_tokens: spent,
            done_nodes: done.iter().map(|node| node.to_string()).collect(),
            evidence: Vec::new(),
        }
    }

    #[test]
    fn complete_plan_is_closed() {
        let (state, blocked, breaker) = derive_state(
            &FleetPlanRow {
                id: "P".to_string(),
                name: "P".to_string(),
                repo: None,
                current_node: Some("complete".to_string()),
                active_node: None,
                holder: None,
                heartbeat: None,
                blocked_reason: None,
                spent_tokens: 0,
            },
            &[],
            now(),
        );
        assert_eq!(state, "complete");
        assert!(!blocked);
        assert_eq!(breaker, "closed");
    }

    #[test]
    fn active_plan_moves() {
        let heartbeat = (now() - chrono::Duration::minutes(1)).to_rfc3339();
        let (state, blocked, breaker) = derive_state(
            &FleetPlanRow {
                id: "P".to_string(),
                name: "P".to_string(),
                repo: None,
                current_node: Some("implement".to_string()),
                active_node: Some("implement".to_string()),
                holder: None,
                heartbeat: Some(heartbeat),
                blocked_reason: None,
                spent_tokens: 0,
            },
            &[],
            now(),
        );
        assert_eq!(state, "moving (implement)");
        assert!(!blocked);
        assert_eq!(breaker, "closed");
    }

    #[test]
    fn stalled_plan_is_open() {
        let heartbeat = (now() - chrono::Duration::minutes(16)).to_rfc3339();
        let (state, blocked, breaker) = derive_state(
            &FleetPlanRow {
                id: "P".to_string(),
                name: "P".to_string(),
                repo: None,
                current_node: Some("implement".to_string()),
                active_node: Some("implement".to_string()),
                holder: None,
                heartbeat: Some(heartbeat),
                blocked_reason: None,
                spent_tokens: 0,
            },
            &[],
            now(),
        );
        assert_eq!(state, "stuck (stalled — implement)");
        assert!(!blocked);
        assert_eq!(breaker, "open");
    }

    #[test]
    fn human_gate_blocks_on_you() {
        let done = [
            "plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept",
        ];
        let (state, blocked, breaker) = derive_state(
            &FleetPlanRow {
                id: "P".to_string(),
                name: "P".to_string(),
                repo: None,
                current_node: Some("accept".to_string()),
                active_node: None,
                holder: None,
                heartbeat: None,
                blocked_reason: None,
                spent_tokens: 0,
            },
            &done.map(str::to_string),
            now(),
        );
        assert_eq!(state, "blocked on you (approve merge)");
        assert!(blocked);
        assert_eq!(breaker, "half-open");
    }

    #[test]
    fn blocker_blocks_on_you() {
        let (state, blocked, breaker) = derive_state(
            &FleetPlanRow {
                id: "P".to_string(),
                name: "P".to_string(),
                repo: None,
                current_node: Some("implement".to_string()),
                active_node: None,
                holder: None,
                heartbeat: None,
                blocked_reason: Some("waiting on secret rotation".to_string()),
                spent_tokens: 0,
            },
            &[],
            now(),
        );
        assert!(blocked);
        assert!(state.contains("resume"));
        assert_eq!(breaker, "open");
    }

    #[test]
    fn claim_liveness_fresh_stale_missing() {
        let at = now();
        let fresh = (at - chrono::Duration::minutes(1)).to_rfc3339();
        assert_eq!(claim_liveness(Some(&fresh), at).0, "fresh");

        let stale = (at - chrono::Duration::hours(25)).to_rfc3339();
        assert_eq!(claim_liveness(Some(&stale), at).0, "stale");

        assert_eq!(claim_liveness(None, at).0, "missing-heartbeat");
    }

    #[test]
    fn a_plan_is_frozen_or_moving_distinctly_from_the_store() {
        let store = store();

        let moving_heartbeat = (now() - chrono::Duration::minutes(1)).to_rfc3339();
        store
            .apply_graph_snapshot(&snapshot(
                "MOVING",
                Some("verify"),
                Some("verify"),
                Some(&moving_heartbeat),
                None,
                &["plan", "promote", "claim", "fork", "implement"],
                42,
            ))
            .unwrap();
        store
            .apply_graph_snapshot(&snapshot(
                "FROZEN",
                Some("verify"),
                None,
                None,
                Some("human gate"),
                &["plan", "promote", "claim", "fork", "implement"],
                7,
            ))
            .unwrap();

        let rows = load_plans(&store, now());
        let by_name = |name: &str| rows.iter().find(|row| row.plan == name).unwrap();
        assert_eq!(by_name("MOVING").state, "moving (verify)");
        assert_eq!(by_name("FROZEN").state, "blocked on you (resume — human gate)");
        assert_eq!(by_name("MOVING").spent_tokens, 42);
        assert_eq!(by_name("FROZEN").spent_tokens, 7);
    }
}
