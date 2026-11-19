//! Schema'd store for dev-flow plan state and evidence (1.29 Phase 2).
//!
//! Content (narrative) stays in git-tracked markdown; `progress.json` stays the
//! git-tracked per-traversal snapshot; this store is the relational
//! state-of-record the fleet/roadmap/plan views query. It is written by the
//! producers of that state — the graph tool co-writes it on every mutation from
//! the same in-memory `GraphState` that writes `progress.json` — not re-ingested
//! from `progress.json` later, so it is a richer co-record, not a second copy.

use std::path::Path;

use anyhow::{Context as _, Result};
use indoc::indoc;
use sqlez::connection::Connection;

/// The fleet view's plan row: the graph-state slice Phase 0's projection fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetPlanRow {
    pub id: String,
    pub name: String,
    pub repo: Option<String>,
    pub current_node: Option<String>,
    pub active_node: Option<String>,
    pub holder: Option<String>,
    pub heartbeat: Option<String>,
    pub blocked_reason: Option<String>,
    pub spent_tokens: u64,
}

/// The fleet view's agent row (a workstream claim).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRow {
    pub id: String,
    pub holder: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub heartbeat: Option<String>,
    pub note: Option<String>,
}

/// A roadmap edge (`Blocked by` / `Related`) — the Phase 3 DAG substrate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRow {
    pub from_plan: String,
    pub to_plan: String,
    pub kind: String,
    pub order_rank: Option<i64>,
}

/// The roadmap view's plan row: the priority tier (`order_rank`) and lifecycle
/// stage/status the roadmap panel groups by, plus the plan's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoadmapPlanRow {
    pub id: String,
    pub name: String,
    pub repo: Option<String>,
    pub stage: Option<String>,
    pub status: Option<String>,
    pub order_rank: Option<i64>,
}

/// Per-node evidence — the 1.28 ledger slice the plan view renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRow {
    pub plan_id: String,
    pub node: String,
    pub artifact_sha: Option<String>,
    pub gate_exit_code: Option<i64>,
    pub report_path: Option<String>,
    pub falsifying_condition: Option<String>,
    pub negative_control: Option<i64>,
}

/// Everything the graph tool can project into the store at save time.
#[derive(Debug, Clone, Default)]
pub struct GraphSnapshot {
    pub plan_id: String,
    pub name: String,
    pub repo: Option<String>,
    pub current_node: Option<String>,
    pub active_node: Option<String>,
    pub holder: Option<String>,
    pub heartbeat: Option<String>,
    pub blocked_reason: Option<String>,
    pub spent_tokens: u64,
    pub done_nodes: Vec<String>,
    pub evidence: Vec<EvidenceRow>,
}

pub struct PlanStore {
    connection: Connection,
}

impl PlanStore {
    /// Open the on-disk store under the Zed data dir, creating the schema.
    pub fn open_default() -> Result<Self> {
        if *zed_env_vars::ZED_STATELESS {
            return Self::open_in_memory();
        }
        let dir = paths::data_dir().join("plans");
        std::fs::create_dir_all(&dir).context("Failed to create plans data dir")?;
        Self::open_at(&dir.join("plans.db"))
    }

    /// Open an in-memory store (tests, stateless contexts).
    pub fn open_in_memory() -> Result<Self> {
        let connection = Connection::open_memory(None);
        Self::init(connection)
    }

    /// Open (or create) the store at `path`.
    pub fn open_at(path: &Path) -> Result<Self> {
        let connection = Connection::open_file(&path.to_string_lossy());
        Self::init(connection)
    }

    fn init(connection: Connection) -> Result<Self> {
        Self::create_schema(&connection)?;
        Ok(Self { connection })
    }

    fn create_schema(connection: &Connection) -> Result<()> {
        connection.exec("PRAGMA journal_mode=WAL;")?()?;
        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS plans (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                repo TEXT,
                stage TEXT,
                status TEXT,
                current_node TEXT,
                active_node TEXT,
                holder TEXT,
                heartbeat TEXT,
                blocked_reason TEXT,
                spent_tokens INTEGER NOT NULL DEFAULT 0,
                order_rank INTEGER,
                updated_at TEXT NOT NULL
            )
        "})?()?;
        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS plan_nodes (
                plan_id TEXT NOT NULL,
                node TEXT NOT NULL,
                completed_at TEXT NOT NULL,
                PRIMARY KEY (plan_id, node)
            )
        "})?()?;
        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS claims (
                id TEXT PRIMARY KEY,
                holder TEXT NOT NULL,
                repo TEXT,
                branch TEXT,
                heartbeat TEXT,
                note TEXT,
                updated_at TEXT NOT NULL
            )
        "})?()?;
        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS edges (
                from_plan TEXT NOT NULL,
                to_plan TEXT NOT NULL,
                kind TEXT NOT NULL,
                order_rank INTEGER,
                PRIMARY KEY (from_plan, to_plan, kind)
            )
        "})?()?;
        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS evidence (
                plan_id TEXT NOT NULL,
                node TEXT NOT NULL,
                artifact_sha TEXT,
                gate_exit_code INTEGER,
                report_path TEXT,
                falsifying_condition TEXT,
                negative_control INTEGER,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (plan_id, node)
            )
        "})?()?;
        Ok(())
    }

    // ---- read API (matches Phase 0's query shapes) ----

    pub fn fleet_plans(&self) -> Result<Vec<FleetPlanRow>> {
        let mut select = self.connection.select_bound::<(), (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            u64,
        )>(indoc! {"
            SELECT id, name, repo, current_node, active_node, holder,
                   heartbeat, blocked_reason, spent_tokens
            FROM plans
            ORDER BY name
        "})?;
        let rows = select(())?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    name,
                    repo,
                    current_node,
                    active_node,
                    holder,
                    heartbeat,
                    blocked_reason,
                    spent_tokens,
                )| FleetPlanRow {
                    id,
                    name,
                    repo,
                    current_node,
                    active_node,
                    holder,
                    heartbeat,
                    blocked_reason,
                    spent_tokens,
                },
            )
            .collect())
    }

    /// The fleet view's row for a single plan, or `None` when absent — the
    /// plan view's selection seam (`fleet_plans` returns every plan; the plan
    /// view selects one and reads its nodes + evidence).
    pub fn plan(&self, plan_id: &str) -> Result<Option<FleetPlanRow>> {
        let mut select = self.connection.select_bound::<String, (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            u64,
        )>(indoc! {"
            SELECT id, name, repo, current_node, active_node, holder,
                   heartbeat, blocked_reason, spent_tokens
            FROM plans WHERE id = ?1
        "})?;
        let rows = select(plan_id.to_string())?;
        Ok(rows
            .into_iter()
            .next()
            .map(
                |(
                    id,
                    name,
                    repo,
                    current_node,
                    active_node,
                    holder,
                    heartbeat,
                    blocked_reason,
                    spent_tokens,
                )| FleetPlanRow {
                    id,
                    name,
                    repo,
                    current_node,
                    active_node,
                    holder,
                    heartbeat,
                    blocked_reason,
                    spent_tokens,
                },
            ))
    }

    pub fn done_nodes(&self, plan_id: &str) -> Result<Vec<String>> {
        let mut select = self
            .connection
            .select_bound::<String, String>(indoc! {"
                SELECT node FROM plan_nodes WHERE plan_id = ?1 ORDER BY node
            "})?;
        select(plan_id.to_string())
    }

    pub fn claims(&self) -> Result<Vec<ClaimRow>> {
        let mut select = self.connection.select_bound::<(), (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )>(indoc! {"
            SELECT id, holder, repo, branch, heartbeat, note FROM claims ORDER BY holder
        "})?;
        let rows = select(())?;
        Ok(rows
            .into_iter()
            .map(|(id, holder, repo, branch, heartbeat, note)| ClaimRow {
                id,
                holder,
                repo,
                branch,
                heartbeat,
                note,
            })
            .collect())
    }

    pub fn edges(&self) -> Result<Vec<EdgeRow>> {
        let mut select = self.connection.select_bound::<(), (
            String,
            String,
            String,
            Option<i64>,
        )>(indoc! {"
            SELECT from_plan, to_plan, kind, order_rank FROM edges
        "})?;
        let rows = select(())?;
        Ok(rows
            .into_iter()
            .map(|(from_plan, to_plan, kind, order_rank)| EdgeRow {
                from_plan,
                to_plan,
                kind,
                order_rank,
            })
            .collect())
    }

    pub fn roadmap_plans(&self) -> Result<Vec<RoadmapPlanRow>> {
        let mut select = self.connection.select_bound::<(), (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
        )>(indoc! {"
            SELECT id, name, repo, stage, status, order_rank FROM plans
            ORDER BY order_rank, name
        "})?;
        let rows = select(())?;
        Ok(rows
            .into_iter()
            .map(|(id, name, repo, stage, status, order_rank)| RoadmapPlanRow {
                id,
                name,
                repo,
                stage,
                status,
                order_rank,
            })
            .collect())
    }

    pub fn evidence_for(&self, plan_id: &str) -> Result<Vec<EvidenceRow>> {
        let mut select = self.connection.select_bound::<String, (
            String,
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<i64>,
        )>(indoc! {"
            SELECT plan_id, node, artifact_sha, gate_exit_code, report_path,
                   falsifying_condition, negative_control
            FROM evidence WHERE plan_id = ?1 ORDER BY node
        "})?;
        let rows = select(plan_id.to_string())?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    plan_id,
                    node,
                    artifact_sha,
                    gate_exit_code,
                    report_path,
                    falsifying_condition,
                    negative_control,
                )| EvidenceRow {
                    plan_id,
                    node,
                    artifact_sha,
                    gate_exit_code,
                    report_path,
                    falsifying_condition,
                    negative_control,
                },
            )
            .collect())
    }

    // ---- write API (the state-of-record) ----

    /// Apply the graph tool's state at save time: plan row + done nodes +
    /// evidence, in one go. The plan's `stage`/`status`/`order_rank` (roadmap
    /// substrate) are left untouched so a metadata writer can own them.
    pub fn apply_graph_snapshot(&self, snapshot: &GraphSnapshot) -> Result<()> {
        self.upsert_plan(snapshot)?;
        self.replace_plan_nodes(&snapshot.plan_id, &snapshot.done_nodes)?;
        for evidence in &snapshot.evidence {
            self.upsert_evidence(evidence)?;
        }
        Ok(())
    }

    fn upsert_plan(&self, snapshot: &GraphSnapshot) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut upsert = self.connection.exec_bound::<(
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            u64,
            String,
        )>(indoc! {"
            INSERT INTO plans (id, name, repo, current_node, active_node, holder,
                               heartbeat, blocked_reason, spent_tokens, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                repo = excluded.repo,
                current_node = excluded.current_node,
                active_node = excluded.active_node,
                holder = excluded.holder,
                heartbeat = excluded.heartbeat,
                blocked_reason = excluded.blocked_reason,
                spent_tokens = excluded.spent_tokens,
                updated_at = excluded.updated_at
        "})?;
        upsert((
            snapshot.plan_id.clone(),
            snapshot.name.clone(),
            snapshot.repo.clone(),
            snapshot.current_node.clone(),
            snapshot.active_node.clone(),
            snapshot.holder.clone(),
            snapshot.heartbeat.clone(),
            snapshot.blocked_reason.clone(),
            snapshot.spent_tokens,
            now,
        ))
    }

    fn replace_plan_nodes(&self, plan_id: &str, done_nodes: &[String]) -> Result<()> {
        let mut delete = self
            .connection
            .exec_bound::<String>(indoc! {"
                DELETE FROM plan_nodes WHERE plan_id = ?1
            "})?;
        delete(plan_id.to_string())?;

        let now = chrono::Utc::now().to_rfc3339();
        let mut insert = self
            .connection
            .exec_bound::<(String, String, String)>(indoc! {"
                INSERT INTO plan_nodes (plan_id, node, completed_at) VALUES (?1, ?2, ?3)
            "})?;
        for node in done_nodes {
            insert((plan_id.to_string(), node.clone(), now.clone()))?;
        }
        Ok(())
    }

    fn upsert_evidence(&self, evidence: &EvidenceRow) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut upsert = self.connection.exec_bound::<(
            String,
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<i64>,
            String,
        )>(indoc! {"
            INSERT INTO evidence (plan_id, node, artifact_sha, gate_exit_code,
                                  report_path, falsifying_condition, negative_control,
                                  updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(plan_id, node) DO UPDATE SET
                artifact_sha = excluded.artifact_sha,
                gate_exit_code = excluded.gate_exit_code,
                report_path = excluded.report_path,
                falsifying_condition = excluded.falsifying_condition,
                negative_control = excluded.negative_control,
                updated_at = excluded.updated_at
        "})?;
        upsert((
            evidence.plan_id.clone(),
            evidence.node.clone(),
            evidence.artifact_sha.clone(),
            evidence.gate_exit_code,
            evidence.report_path.clone(),
            evidence.falsifying_condition.clone(),
            evidence.negative_control,
            now,
        ))
    }

    pub fn upsert_claim(&self, claim: &ClaimRow) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut upsert = self.connection.exec_bound::<(
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
        )>(indoc! {"
            INSERT INTO claims (id, holder, repo, branch, heartbeat, note, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(id) DO UPDATE SET
                holder = excluded.holder,
                repo = excluded.repo,
                branch = excluded.branch,
                heartbeat = excluded.heartbeat,
                note = excluded.note,
                updated_at = excluded.updated_at
        "})?;
        upsert((
            claim.id.clone(),
            claim.holder.clone(),
            claim.repo.clone(),
            claim.branch.clone(),
            claim.heartbeat.clone(),
            claim.note.clone(),
            now,
        ))
    }

    /// Write the roadmap metadata columns (`name`/`repo`/`stage`/`status`/
    /// `order_rank`). This is the fork-side contract for those columns — the
    /// harness roadmap writer (`zed-tools/roadmap/`) writes the same columns
    /// through SQLite directly — and the test seam the roadmap panel's read
    /// path is exercised through. `apply_graph_snapshot` deliberately leaves
    /// these columns untouched so this metadata writer owns them.
    pub fn upsert_plan_metadata(&self, plan: &RoadmapPlanRow) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut upsert = self.connection.exec_bound::<(
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
            String,
        )>(indoc! {"
            INSERT INTO plans (id, name, repo, stage, status, order_rank, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                repo = excluded.repo,
                stage = excluded.stage,
                status = excluded.status,
                order_rank = excluded.order_rank,
                updated_at = excluded.updated_at
        "})?;
        upsert((
            plan.id.clone(),
            plan.name.clone(),
            plan.repo.clone(),
            plan.stage.clone(),
            plan.status.clone(),
            plan.order_rank,
            now,
        ))
    }

    pub fn upsert_edge(&self, edge: &EdgeRow) -> Result<()> {
        let mut upsert = self.connection.exec_bound::<(
            String,
            String,
            String,
            Option<i64>,
        )>(indoc! {"
            INSERT INTO edges (from_plan, to_plan, kind, order_rank)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(from_plan, to_plan, kind) DO UPDATE SET
                order_rank = excluded.order_rank
        "})?;
        upsert((
            edge.from_plan.clone(),
            edge.to_plan.clone(),
            edge.kind.clone(),
            edge.order_rank,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> PlanStore {
        PlanStore::open_in_memory().expect("open in-memory store")
    }

    fn snapshot(name: &str, current: &str, spent: u64) -> GraphSnapshot {
        GraphSnapshot {
            plan_id: name.to_string(),
            name: name.to_string(),
            repo: Some("zed".to_string()),
            current_node: Some(current.to_string()),
            active_node: None,
            holder: None,
            heartbeat: None,
            blocked_reason: None,
            spent_tokens: spent,
            done_nodes: vec!["plan".to_string(), "promote".to_string()],
            evidence: vec![EvidenceRow {
                plan_id: name.to_string(),
                node: "plan".to_string(),
                artifact_sha: Some("abc123".to_string()),
                gate_exit_code: Some(0),
                report_path: None,
                falsifying_condition: None,
                negative_control: None,
            }],
        }
    }

    #[test]
    fn fleet_plans_round_trips_a_graph_snapshot() {
        let store = store();
        store
            .apply_graph_snapshot(&snapshot("PLAN_A", "implement", 42))
            .unwrap();

        let plans = store.fleet_plans().unwrap();
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.id, "PLAN_A");
        assert_eq!(plan.name, "PLAN_A");
        assert_eq!(plan.repo.as_deref(), Some("zed"));
        assert_eq!(plan.current_node.as_deref(), Some("implement"));
        assert_eq!(plan.spent_tokens, 42);

        let done = store.done_nodes("PLAN_A").unwrap();
        assert_eq!(done, vec!["plan", "promote"]);
    }

    #[test]
    fn plan_returns_single_plan_or_none() {
        let store = store();
        store
            .apply_graph_snapshot(&snapshot("PLAN_A", "implement", 42))
            .unwrap();

        let plan = store.plan("PLAN_A").unwrap().expect("PLAN_A exists");
        assert_eq!(plan.id, "PLAN_A");
        assert_eq!(plan.current_node.as_deref(), Some("implement"));
        assert_eq!(plan.spent_tokens, 42);

        assert!(store.plan("MISSING").unwrap().is_none());
    }

    #[test]
    fn evidence_carries_fields_progress_json_lacks() {
        let store = store();
        store
            .apply_graph_snapshot(&snapshot("PLAN_A", "implement", 0))
            .unwrap();

        let evidence = store.evidence_for("PLAN_A").unwrap();
        assert_eq!(evidence.len(), 1);
        let row = &evidence[0];
        assert_eq!(row.node, "plan");
        assert_eq!(row.artifact_sha.as_deref(), Some("abc123"));
        // `gate_exit_code` is a column `progress.json` does not carry — proving
        // the store is a richer co-record, not a re-ingest of progress.json.
        assert_eq!(row.gate_exit_code, Some(0));
    }

    #[test]
    fn a_plan_is_frozen_or_moving_distinctly() {
        let store = store();
        let mut moving = snapshot("MOVING", "verify", 0);
        moving.active_node = Some("verify".to_string());
        moving.heartbeat = Some("2026-11-18T00:00:00+00:00".to_string());
        moving.holder = Some("agent-1".to_string());

        let mut frozen = snapshot("FROZEN", "verify", 0);
        frozen.blocked_reason = Some("human gate".to_string());

        store.apply_graph_snapshot(&moving).unwrap();
        store.apply_graph_snapshot(&frozen).unwrap();

        let plans = store.fleet_plans().unwrap();
        let by_name = |name: &str| plans.iter().find(|plan| plan.name == name).unwrap();

        assert_eq!(by_name("MOVING").active_node.as_deref(), Some("verify"));
        assert_eq!(by_name("MOVING").holder.as_deref(), Some("agent-1"));
        assert_eq!(
            by_name("MOVING").heartbeat.as_deref(),
            Some("2026-11-18T00:00:00+00:00")
        );
        assert_eq!(by_name("FROZEN").active_node, None);
        assert_eq!(
            by_name("FROZEN").blocked_reason.as_deref(),
            Some("human gate")
        );
    }

    #[test]
    fn claims_round_trip() {
        let store = store();
        store
            .upsert_claim(&ClaimRow {
                id: "c1".to_string(),
                holder: "agent-1".to_string(),
                repo: Some("zed".to_string()),
                branch: Some("wip/foo".to_string()),
                heartbeat: Some("2026-11-18T00:00:00+00:00".to_string()),
                note: Some("work".to_string()),
            })
            .unwrap();

        let claims = store.claims().unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].id, "c1");
        assert_eq!(claims[0].holder, "agent-1");
        assert_eq!(claims[0].branch.as_deref(), Some("wip/foo"));
    }

    #[test]
    fn edges_round_trip() {
        let store = store();
        store
            .upsert_edge(&EdgeRow {
                from_plan: "A".to_string(),
                to_plan: "B".to_string(),
                kind: "blocked_by".to_string(),
                order_rank: Some(1),
            })
            .unwrap();

        let edges = store.edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from_plan, "A");
        assert_eq!(edges[0].to_plan, "B");
        assert_eq!(edges[0].kind, "blocked_by");
    }

    #[test]
    fn roadmap_plans_returns_tier_stage_and_rank() {
        let store = store();
        let plan = |id: &str, stage: &str, rank: i64| RoadmapPlanRow {
            id: id.to_string(),
            name: id.to_string(),
            repo: Some("zed".to_string()),
            stage: Some(stage.to_string()),
            status: Some("in progress".to_string()),
            order_rank: Some(rank),
        };

        store.upsert_plan_metadata(&plan("P1", "plan", 200)).unwrap();
        store.upsert_plan_metadata(&plan("P3H", "draft", 600)).unwrap();
        store.upsert_plan_metadata(&plan("DEFERRED", "plan", 9999)).unwrap();

        // Ordered by order_rank, then name — so the roadmap panel reads
        // "what's next" (lowest rank) off the top of this list.
        let rows = store.roadmap_plans().unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["P1", "P3H", "DEFERRED"]
        );
        assert_eq!(rows[0].stage.as_deref(), Some("plan"));
        assert_eq!(rows[2].order_rank, Some(9999));
    }
}
