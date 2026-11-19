//! Read-only bottom-dock panel that renders the roadmap DAG: every plan in the
//! SQLite plan store grouped by priority tier (`order_rank`), with its
//! `Blocked by:` / `Related:` edges, so "what depends on what, what's next,
//! what's deferred" is read off the store — not re-parsed from plan prose or
//! `progress.json`.
//!
//! The roadmap is a *projection*, not a second copy: it reads the same
//! `plan_store` rows the fleet panel reads (`roadmap_plans` + `edges`), grouped
//! by `order_rank`, and renders each plan's `blocked_by`/`related` targets as
//! indented edge lines. A plan with `order_rank` 9999 (the deferred sentinel)
//! renders under "Deferred" rather than a priority tier.

use std::collections::BTreeMap;

use gpui::{
    actions, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Render,
    Task, WeakEntity, Window,
};
use plan_store::PlanStore;
use ui::{prelude::*, Button, ButtonSize, IconName, Label, LabelSize, StatefulInteractiveElement};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(roadmap, [ToggleRoadmapPanel]);

const ROADMAP_PANEL_KEY: &str = "RoadmapPanel";

/// The `order_rank` sentinel for a plan in the deferred register (R7 —
/// externally blocked, do not schedule). It sorts last and renders under
/// "Deferred" instead of a priority tier.
const DEFERRED_RANK: i64 = 9999;

/// One plan as the roadmap renders it: identity, lifecycle stage, and its edges.
struct RoadmapPlanView {
    name: String,
    stage: String,
    blocked_by: Vec<String>,
    related: Vec<String>,
}

/// A priority tier: its label and the plans in it (already name-ordered).
struct RoadmapTier {
    label: String,
    plans: Vec<RoadmapPlanView>,
}

fn tier_label(rank: i64) -> &'static str {
    match rank {
        0 => "Unranked (no priority tier)",
        100 => "Priority 0 — repair the ledger",
        200 => "Priority 1 — workflow, tooling & skills",
        300 => "Priority 2a — integrity & correctness",
        310 => "Priority 2b — quality of daily use",
        320 => "Priority 2c — ergonomics",
        400 => "Priority 3L — features (low)",
        500 => "Priority 3M — features (medium)",
        600 => "Priority 3H — features (high)",
        _ => "Other",
    }
}

fn stage_label(stage: Option<&str>) -> String {
    stage.unwrap_or("—").to_string()
}

/// Project the store into (tiers, deferred): tiers ordered by `order_rank`, each
/// plan carrying its `blocked_by`/`related` targets. `None` rank (unseeded
/// plans) is grouped under an "Unranked" tier so it is visibly not scheduled.
fn load_roadmap(store: &PlanStore) -> (Vec<RoadmapTier>, Vec<RoadmapPlanView>) {
    let plans = store.roadmap_plans().unwrap_or_default();
    let edges = store.edges().unwrap_or_default();

    let edge_targets = |from: &str, kind: &str| -> Vec<String> {
        edges
            .iter()
            .filter(|edge| edge.from_plan == from && edge.kind == kind)
            .map(|edge| edge.to_plan.clone())
            .collect()
    };

    let mut tiered: BTreeMap<i64, Vec<RoadmapPlanView>> = BTreeMap::new();
    let mut deferred: Vec<RoadmapPlanView> = Vec::new();

    for plan in &plans {
        let view = RoadmapPlanView {
            name: plan.name.clone(),
            stage: stage_label(plan.stage.as_deref()),
            blocked_by: edge_targets(&plan.id, "blocked_by"),
            related: edge_targets(&plan.id, "related"),
        };
        match plan.order_rank {
            Some(DEFERRED_RANK) => deferred.push(view),
            Some(rank) => tiered.entry(rank).or_default().push(view),
            None => tiered.entry(0).or_default().push(view),
        }
    }

    let tiers = tiered
        .into_iter()
        .map(|(rank, plans)| RoadmapTier {
            label: tier_label(rank).to_string(),
            plans,
        })
        .collect();

    (tiers, deferred)
}

fn render_plan(view: &RoadmapPlanView) -> impl IntoElement {
    v_flex()
        .px_2()
        .child(Label::new(format!("{} [{}]", view.name, view.stage)))
        .when(!view.blocked_by.is_empty(), |element| {
            element.child(
                Label::new(format!("← blocked by {}", view.blocked_by.join(", ")))
                    .size(LabelSize::Small),
            )
        })
        .when(!view.related.is_empty(), |element| {
            element.child(
                Label::new(format!("↔ related {}", view.related.join(", ")))
                    .size(LabelSize::Small),
            )
        })
}

pub struct RoadmapPanel {
    focus_handle: FocusHandle,
    tiers: Vec<RoadmapTier>,
    deferred: Vec<RoadmapPlanView>,
    last_error: Option<String>,
}

impl Focusable for RoadmapPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for RoadmapPanel {}

impl RoadmapPanel {
    pub fn new(
        _workspace: &Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                tiers: Vec::new(),
                deferred: Vec::new(),
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
                RoadmapPanel::new(workspace, window, cx)
            })
        })
    }

    fn refresh(&mut self) {
        let store = match PlanStore::open_default() {
            Ok(store) => store,
            Err(error) => {
                self.tiers.clear();
                self.deferred.clear();
                self.last_error = Some(format!("Failed to open the plan store: {error:#}"));
                return;
            }
        };
        let (tiers, deferred) = load_roadmap(&store);
        self.tiers = tiers;
        self.deferred = deferred;
        self.last_error = None;
    }
}

impl Render for RoadmapPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let refresh = cx.listener(|this: &mut RoadmapPanel, _, _, cx| {
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
                    .child(Label::new("Roadmap"))
                    .child(
                        Button::new("roadmap-refresh", "Refresh")
                            .size(ButtonSize::Compact)
                            .on_click(refresh),
                    ),
            )
            .child(
                div()
                    .id("roadmap-scroll")
                    .flex_col()
                    .size_full()
                    .overflow_y_scroll()
                    .px_2()
                    .gap_1()
                    .when_some(self.last_error.clone(), |element, error| {
                        element.child(Label::new(error))
                    })
                    .when(self.tiers.is_empty() && self.deferred.is_empty(), |element| {
                        element.child(Label::new("(no plans in the store)").size(LabelSize::Small))
                    })
                    .children(self.tiers.iter().map(|tier| {
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new(format!("{} ({})", tier.label, tier.plans.len()))
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .children(tier.plans.iter().map(render_plan))
                    }))
                    .when(!self.deferred.is_empty(), |element| {
                        element.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new(format!("Deferred ({})", self.deferred.len()))
                                        .weight(FontWeight::SEMIBOLD),
                                )
                                .children(self.deferred.iter().map(render_plan)),
                        )
                    }),
            )
    }
}

impl Panel for RoadmapPanel {
    fn persistent_name() -> &'static str {
        "RoadmapPanel"
    }

    fn panel_key() -> &'static str {
        ROADMAP_PANEL_KEY
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
        Some("Roadmap")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleRoadmapPanel)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

/// Registers the panel toggle action on every new workspace.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(
            |workspace: &mut Workspace, _: &ToggleRoadmapPanel, window, cx| {
                workspace.toggle_panel_focus::<RoadmapPanel>(window, cx);
            },
        );
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use plan_store::{EdgeRow, PlanStore, RoadmapPlanRow};

    fn store() -> PlanStore {
        PlanStore::open_in_memory().expect("open in-memory store")
    }

    fn metadata(id: &str, rank: i64) -> RoadmapPlanRow {
        RoadmapPlanRow {
            id: id.to_string(),
            name: id.to_string(),
            repo: Some("zed".to_string()),
            stage: Some("plan".to_string()),
            status: Some("in progress".to_string()),
            order_rank: Some(rank),
        }
    }

    #[test]
    fn groups_by_tier_and_reads_edges_from_the_store() {
        let store = store();
        store.upsert_plan_metadata(&metadata("P1", 200)).unwrap();
        store.upsert_plan_metadata(&metadata("P3H", 600)).unwrap();
        store
            .upsert_edge(&EdgeRow {
                from_plan: "P3H".to_string(),
                to_plan: "P1".to_string(),
                kind: "blocked_by".to_string(),
                order_rank: None,
            })
            .unwrap();

        let (tiers, deferred) = load_roadmap(&store);
        assert!(deferred.is_empty());
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].plans[0].name, "P1");
        assert!(tiers[0].plans[0].blocked_by.is_empty());
        assert_eq!(tiers[1].plans[0].name, "P3H");
        assert_eq!(tiers[1].plans[0].blocked_by, vec!["P1".to_string()]);
    }

    #[test]
    fn deferred_plans_are_separated_not_a_tier() {
        let store = store();
        store.upsert_plan_metadata(&metadata("P1", 200)).unwrap();
        store.upsert_plan_metadata(&metadata("DEF", 9999)).unwrap();

        let (tiers, deferred) = load_roadmap(&store);
        assert_eq!(tiers.len(), 1);
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].name, "DEF");
    }
}
