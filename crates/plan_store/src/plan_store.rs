//! Schema'd store for dev-flow plan state and evidence (1.29 Phase 2).
//!
//! Content (narrative) stays in git-tracked markdown; `progress.json` stays the
//! git-tracked per-traversal snapshot; this store is the relational
//! state-of-record the fleet/roadmap/plan views query. It is written by the
//! producers of that state — the graph tool co-writes it on every mutation from
//! the same in-memory `GraphState` that writes `progress.json` — not re-ingested
//! from `progress.json` later, so it is a richer co-record, not a second copy.

use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

use anyhow::{Context as _, Result};
use indoc::indoc;
use sqlez::connection::Connection;

/// One-time store repairs are gated on `PRAGMA user_version`, so reopening a
/// store repeats the cheap schema checks but not the row rewrites.
const SCHEMA_VERSION: i64 = 1;

/// Normalize a plan id to its canonical store key.
///
/// The graph tool addresses a plan by its source filename
/// (`PLAN_OF_ACTION-…-interactive-graph-roadmap.md`), while the roadmap writer
/// keys the same plan by its lowercased, dash-separated stem
/// (`plan-of-action-…-interactive-graph-roadmap`). They name one plan, so the
/// store aliases the filename form onto the stem form at every boundary; an id
/// that is not a filename is left untouched.
pub fn normalize_plan_id(id: &str) -> String {
    let trimmed = id.trim();
    let lower = trimmed.to_ascii_lowercase();
    match lower.strip_suffix(".md") {
        Some(stem) => stem.replace('_', "-"),
        None => trimmed.to_string(),
    }
}

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

/// A node in the plan taxonomy (`Feature / Bug / QOL / Process` + subcategories).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryRow {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub rank: i64,
}

/// A plan's category assignment. `source` records whether a human override or
/// the batch categorizer produced it; `input_hash` lets an incremental re-run
/// tell whether the plan changed since the assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanCategoryRow {
    pub plan_id: String,
    pub category_id: String,
    pub source: String,
    pub input_hash: String,
}

/// A slice's parent link (`PLAN_OF_ACTION-*` → the plan it was sliced from).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanParentRow {
    pub plan_id: String,
    pub parent_id: String,
}

/// A plan joined with its category assignment and the state the wheel groups by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategorizedPlanRow {
    pub id: String,
    pub name: String,
    pub repo: Option<String>,
    pub stage: Option<String>,
    pub status: Option<String>,
    pub current_node: Option<String>,
    pub category_id: Option<String>,
    pub category_source: Option<String>,
    /// One-line summary for the wheel leaf (from the plan markdown).
    pub summary: Option<String>,
    /// One-paragraph summary for the plan page.
    pub detail: Option<String>,
}

impl CategorizedPlanRow {
    pub fn lifecycle(&self) -> Lifecycle {
        classify_lifecycle(
            self.stage.as_deref(),
            self.status.as_deref(),
            self.current_node.as_deref(),
        )
    }
}

/// Whether a plan is still live or belongs behind the History toggle (D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Live,
    History,
}

const HISTORY_HEADS: &[&str] = &[
    "done",
    "rejected",
    "completed",
    "complete",
    "cancelled",
    "canceled",
    "superseded",
];

fn status_head(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let head = value
        .split(" — ")
        .next()
        .unwrap_or(value)
        .split(" - ")
        .next()
        .unwrap_or(value)
        .split(':')
        .next()
        .unwrap_or(value)
        .trim();
    Some(head.to_ascii_lowercase())
}

/// Classify lifecycle from the same fields the harness categorizer reads.
/// Status/`stage` values are prose (`done — …`, `in progress — …`), so only the
/// leading word is significant; `current_node == complete` is the tool's own
/// end marker.
pub fn classify_lifecycle(
    stage: Option<&str>,
    status: Option<&str>,
    current_node: Option<&str>,
) -> Lifecycle {
    let node_complete = current_node
        .map(|node| node.trim().eq_ignore_ascii_case("complete"))
        .unwrap_or(false);
    let head_is_history = |value: Option<&str>| {
        status_head(value)
            .map(|head| HISTORY_HEADS.contains(&head.as_str()))
            .unwrap_or(false)
    };
    if node_complete || head_is_history(stage) || head_is_history(status) {
        Lifecycle::History
    } else {
        Lifecycle::Live
    }
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
        Self::migrate(&connection)?;
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
                debugger_scoped INTEGER NOT NULL DEFAULT 0,
                summary TEXT,
                detail TEXT,
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
        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS categories (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                name TEXT NOT NULL,
                rank INTEGER NOT NULL DEFAULT 0
            )
        "})?()?;
        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS plan_categories (
                plan_id TEXT PRIMARY KEY,
                category_id TEXT NOT NULL,
                source TEXT NOT NULL DEFAULT 'auto',
                input_hash TEXT NOT NULL DEFAULT '',
                assigned_at TEXT NOT NULL
            )
        "})?()?;
        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS plan_parents (
                plan_id TEXT PRIMARY KEY,
                parent_id TEXT NOT NULL,
                source TEXT NOT NULL DEFAULT 'parent',
                updated_at TEXT NOT NULL
            )
        "})?()?;
        Ok(())
    }

    /// Bring a pre-existing store up to the current schema, then repair plan
    /// identity so every edge resolves to one row.
    ///
    /// `CREATE TABLE IF NOT EXISTS` cannot add a column to an existing table, so
    /// stores created before the debugger-scope marker need an explicit
    /// `ALTER TABLE`. The identity repair is gated once; the column check is
    /// cheap enough to repeat on every open.
    fn migrate(connection: &Connection) -> Result<()> {
        Self::ensure_plan_columns(connection)?;

        let mut version_select = connection.select::<i64>("PRAGMA user_version")?;
        let version = version_select()?.into_iter().next().unwrap_or(0);
        if version < SCHEMA_VERSION {
            Self::dedupe_plan_identities(connection)?;
            Self::clean_dangling_edges(connection)?;
            connection.exec("PRAGMA user_version = 1")?()?;
        }
        Ok(())
    }

    fn ensure_plan_columns(connection: &Connection) -> Result<()> {
        let mut select = connection.select::<(i64, String, String, i64, Option<String>, i64)>(
            "PRAGMA table_info(plans)",
        )?;
        let columns: HashSet<String> = select()?.into_iter().map(|(_, name, ..)| name).collect();
        if !columns.contains("debugger_scoped") {
            connection.exec(
                "ALTER TABLE plans ADD COLUMN debugger_scoped INTEGER NOT NULL DEFAULT 0",
            )?()?;
        }
        if !columns.contains("summary") {
            connection.exec("ALTER TABLE plans ADD COLUMN summary TEXT")?()?;
        }
        if !columns.contains("detail") {
            connection.exec("ALTER TABLE plans ADD COLUMN detail TEXT")?()?;
        }
        Ok(())
    }

    /// Merge rows that name the same plan through different id spellings into
    /// one canonical row: fill the survivor's empty columns from the twin, move
    /// its nodes/evidence/edges across, then drop the twin.
    fn dedupe_plan_identities(connection: &Connection) -> Result<()> {
        let mut id_select = connection.select::<String>("SELECT id FROM plans")?;
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for id in id_select()? {
            groups.entry(normalize_plan_id(&id)).or_default().push(id);
        }

        for (canonical, group) in groups {
            if group.len() < 2 {
                continue;
            }
            let survivor = group
                .iter()
                .find(|id| **id == canonical)
                .cloned()
                .unwrap_or_else(|| group[0].clone());
            for twin in group.iter().filter(|id| **id != survivor) {
                Self::merge_plan_row(connection, &survivor, twin)?;
                Self::remap_plan_dependents(connection, &survivor, twin)?;
                let mut delete =
                    connection.exec_bound::<String>("DELETE FROM plans WHERE id = ?1")?;
                delete(twin.clone())?;
            }
        }
        Ok(())
    }

    fn merge_plan_row(connection: &Connection, survivor: &str, twin: &str) -> Result<()> {
        let mut update = connection.exec_bound::<(String, String)>(indoc! {"
            UPDATE plans SET
                repo = COALESCE(repo, (SELECT repo FROM plans WHERE id = ?2)),
                stage = COALESCE(stage, (SELECT stage FROM plans WHERE id = ?2)),
                status = COALESCE(status, (SELECT status FROM plans WHERE id = ?2)),
                current_node = COALESCE(current_node, (SELECT current_node FROM plans WHERE id = ?2)),
                active_node = COALESCE(active_node, (SELECT active_node FROM plans WHERE id = ?2)),
                holder = COALESCE(holder, (SELECT holder FROM plans WHERE id = ?2)),
                heartbeat = COALESCE(heartbeat, (SELECT heartbeat FROM plans WHERE id = ?2)),
                blocked_reason = COALESCE(blocked_reason, (SELECT blocked_reason FROM plans WHERE id = ?2)),
                spent_tokens = MAX(spent_tokens, (SELECT spent_tokens FROM plans WHERE id = ?2)),
                debugger_scoped = MAX(debugger_scoped, (SELECT debugger_scoped FROM plans WHERE id = ?2)),
                order_rank = COALESCE(order_rank, (SELECT order_rank FROM plans WHERE id = ?2)),
                updated_at = MAX(updated_at, (SELECT updated_at FROM plans WHERE id = ?2))
            WHERE id = ?1
        "})?;
        update((survivor.to_string(), twin.to_string()))
    }

    fn remap_plan_dependents(connection: &Connection, survivor: &str, twin: &str) -> Result<()> {
        let mut move_nodes = connection.exec_bound::<(String, String)>(indoc! {"
            INSERT OR IGNORE INTO plan_nodes (plan_id, node, completed_at)
            SELECT ?1, node, completed_at FROM plan_nodes WHERE plan_id = ?2
        "})?;
        move_nodes((survivor.to_string(), twin.to_string()))?;

        let mut move_evidence = connection.exec_bound::<(String, String)>(indoc! {"
            INSERT OR IGNORE INTO evidence (plan_id, node, artifact_sha, gate_exit_code,
                                           report_path, falsifying_condition,
                                           negative_control, updated_at)
            SELECT ?1, node, artifact_sha, gate_exit_code, report_path,
                   falsifying_condition, negative_control, updated_at
            FROM evidence WHERE plan_id = ?2
        "})?;
        move_evidence((survivor.to_string(), twin.to_string()))?;

        let mut move_outgoing = connection.exec_bound::<(String, String)>(indoc! {"
            INSERT OR IGNORE INTO edges (from_plan, to_plan, kind, order_rank)
            SELECT ?1, to_plan, kind, order_rank FROM edges WHERE from_plan = ?2
        "})?;
        move_outgoing((survivor.to_string(), twin.to_string()))?;

        let mut move_incoming = connection.exec_bound::<(String, String)>(indoc! {"
            INSERT OR IGNORE INTO edges (from_plan, to_plan, kind, order_rank)
            SELECT from_plan, ?1, kind, order_rank FROM edges WHERE to_plan = ?2
        "})?;
        move_incoming((survivor.to_string(), twin.to_string()))?;

        connection.exec_bound::<String>("DELETE FROM plan_nodes WHERE plan_id = ?1")?(
            twin.to_string(),
        )?;
        connection.exec_bound::<String>("DELETE FROM evidence WHERE plan_id = ?1")?(
            twin.to_string(),
        )?;
        connection
            .exec_bound::<String>("DELETE FROM edges WHERE from_plan = ?1 OR to_plan = ?1")?(
            twin.to_string(),
        )?;
        Ok(())
    }

    /// Drop edge endpoints that name no plan, after giving a prose-truncated id
    /// one chance to resolve to its leading plan name.
    fn clean_dangling_edges(connection: &Connection) -> Result<()> {
        let mut id_select = connection.select::<String>("SELECT id FROM plans")?;
        let plan_ids: HashSet<String> = id_select()?.into_iter().collect();

        let mut edge_select = connection.select::<(String, String, String, Option<i64>)>(
            "SELECT from_plan, to_plan, kind, order_rank FROM edges",
        )?;
        let resolved = edge_select()?
            .into_iter()
            .filter_map(|(from_plan, to_plan, kind, order_rank)| {
                let from_plan = Self::resolve_edge_endpoint(&from_plan, &plan_ids)?;
                let to_plan = Self::resolve_edge_endpoint(&to_plan, &plan_ids)?;
                (from_plan != to_plan).then_some((from_plan, to_plan, kind, order_rank))
            })
            .collect::<Vec<_>>();

        connection.exec("DELETE FROM edges")?()?;
        let mut insert =
            connection.exec_bound::<(String, String, String, Option<i64>)>(indoc! {"
            INSERT OR IGNORE INTO edges (from_plan, to_plan, kind, order_rank)
            VALUES (?1, ?2, ?3, ?4)
        "})?;
        for edge in resolved {
            insert(edge)?;
        }
        Ok(())
    }

    fn resolve_edge_endpoint(raw: &str, plan_ids: &HashSet<String>) -> Option<String> {
        let without_prose = raw.split(" (").next().unwrap_or(raw).trim();
        let canonical = normalize_plan_id(without_prose);
        plan_ids.contains(&canonical).then_some(canonical)
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
        let plan_id = normalize_plan_id(plan_id);
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
        let rows = select(plan_id)?;
        Ok(rows.into_iter().next().map(
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
        let plan_id = normalize_plan_id(plan_id);
        let mut select = self.connection.select_bound::<String, String>(indoc! {"
                SELECT node FROM plan_nodes WHERE plan_id = ?1 ORDER BY node
            "})?;
        select(plan_id)
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
        let mut select = self
            .connection
            .select_bound::<(), (String, String, String, Option<i64>)>(indoc! {"
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
            .map(
                |(id, name, repo, stage, status, order_rank)| RoadmapPlanRow {
                    id,
                    name,
                    repo,
                    stage,
                    status,
                    order_rank,
                },
            )
            .collect())
    }

    pub fn evidence_for(&self, plan_id: &str) -> Result<Vec<EvidenceRow>> {
        let plan_id = normalize_plan_id(plan_id);
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
        let rows = select(plan_id)?;
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

    pub fn categories(&self) -> Result<Vec<CategoryRow>> {
        let mut select = self
            .connection
            .select_bound::<(), (String, Option<String>, String, i64)>(indoc! {"
                SELECT id, parent_id, name, rank FROM categories ORDER BY rank, name
            "})?;
        let rows = select(())?;
        Ok(rows
            .into_iter()
            .map(|(id, parent_id, name, rank)| CategoryRow {
                id,
                parent_id,
                name,
                rank,
            })
            .collect())
    }

    pub fn plan_categories(&self) -> Result<Vec<PlanCategoryRow>> {
        let mut select = self
            .connection
            .select_bound::<(), (String, String, String, String)>(indoc! {"
            SELECT plan_id, category_id, source, input_hash
            FROM plan_categories ORDER BY plan_id
        "})?;
        let rows = select(())?;
        Ok(rows
            .into_iter()
            .map(
                |(plan_id, category_id, source, input_hash)| PlanCategoryRow {
                    plan_id,
                    category_id,
                    source,
                    input_hash,
                },
            )
            .collect())
    }

    pub fn plan_category(&self, plan_id: &str) -> Result<Option<PlanCategoryRow>> {
        let plan_id = normalize_plan_id(plan_id);
        let mut select = self
            .connection
            .select_bound::<String, (String, String, String, String)>(indoc! {"
            SELECT plan_id, category_id, source, input_hash
            FROM plan_categories WHERE plan_id = ?1
        "})?;
        let rows = select(plan_id)?;
        Ok(rows
            .into_iter()
            .next()
            .map(
                |(plan_id, category_id, source, input_hash)| PlanCategoryRow {
                    plan_id,
                    category_id,
                    source,
                    input_hash,
                },
            ))
    }

    pub fn plan_parents(&self) -> Result<Vec<PlanParentRow>> {
        let mut select = self
            .connection
            .select_bound::<(), (String, String)>(indoc! {"
                SELECT plan_id, parent_id FROM plan_parents ORDER BY plan_id
            "})?;
        let rows = select(())?;
        Ok(rows
            .into_iter()
            .map(|(plan_id, parent_id)| PlanParentRow { plan_id, parent_id })
            .collect())
    }

    /// Every plan with its category assignment joined in — the wheel's single
    /// read. `category_*` are `None` for a slice (attached to a parent instead)
    /// or a plan the categorizer has not seen yet.
    pub fn categorized_plans(&self) -> Result<Vec<CategorizedPlanRow>> {
        let mut select = self.connection.select_bound::<(), (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )>(indoc! {"
            SELECT p.id, p.name, p.repo, p.stage, p.status, p.current_node,
                   c.category_id, c.source, p.summary, p.detail
            FROM plans p
            LEFT JOIN plan_categories c ON c.plan_id = p.id
            ORDER BY p.name
        "})?;
        let rows = select(())?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    name,
                    repo,
                    stage,
                    status,
                    current_node,
                    category_id,
                    category_source,
                    summary,
                    detail,
                )| CategorizedPlanRow {
                    id,
                    name,
                    repo,
                    stage,
                    status,
                    current_node,
                    category_id,
                    category_source,
                    summary,
                    detail,
                },
            )
            .collect())
    }

    /// Whether the plan is debugger-scoped — the marker the graph tool co-writes
    /// (1.29 D8) so the plan graph renders the conditional `stage→…→fix`
    /// sub-graph. `false` for an unknown plan id.
    pub fn debugger_scoped(&self, plan_id: &str) -> Result<bool> {
        let plan_id = normalize_plan_id(plan_id);
        let mut select = self.connection.select_bound::<String, bool>(indoc! {"
                SELECT debugger_scoped FROM plans WHERE id = ?1
            "})?;
        let rows = select(plan_id)?;
        Ok(rows.into_iter().next().unwrap_or(false))
    }

    // ---- write API (the state-of-record) ----

    /// Apply the graph tool's state at save time: plan row + done nodes +
    /// evidence, in one go. The plan's `stage`/`status`/`order_rank` (roadmap
    /// substrate) are left untouched so a metadata writer can own them.
    pub fn apply_graph_snapshot(&self, snapshot: &GraphSnapshot) -> Result<()> {
        let plan_id = normalize_plan_id(&snapshot.plan_id);
        self.upsert_plan(&plan_id, snapshot)?;
        self.replace_plan_nodes(&plan_id, &snapshot.done_nodes)?;
        for evidence in &snapshot.evidence {
            self.upsert_evidence(&plan_id, evidence)?;
        }
        Ok(())
    }

    /// Set the plan's debugger-scope marker. The graph tool calls this after
    /// `apply_graph_snapshot` (which creates the plan row), so the marker rides
    /// the same state-of-record write as the rest of the graph state;
    /// `apply_graph_snapshot` deliberately leaves this column untouched so this
    /// is its sole writer.
    pub fn set_debugger_scoped(&self, plan_id: &str, scoped: bool) -> Result<()> {
        let plan_id = normalize_plan_id(plan_id);
        let now = chrono::Utc::now().to_rfc3339();
        let mut update = self
            .connection
            .exec_bound::<(bool, String, String)>(indoc! {"
            UPDATE plans SET debugger_scoped = ?1, updated_at = ?2 WHERE id = ?3
        "})?;
        update((scoped, now, plan_id))
    }

    fn upsert_plan(&self, plan_id: &str, snapshot: &GraphSnapshot) -> Result<()> {
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
            plan_id.to_string(),
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
        let mut delete = self.connection.exec_bound::<String>(indoc! {"
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

    fn upsert_evidence(&self, plan_id: &str, evidence: &EvidenceRow) -> Result<()> {
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
            plan_id.to_string(),
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
        let plan_id = normalize_plan_id(&plan.id);
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
            plan_id,
            plan.name.clone(),
            plan.repo.clone(),
            plan.stage.clone(),
            plan.status.clone(),
            plan.order_rank,
            now,
        ))
    }

    pub fn upsert_edge(&self, edge: &EdgeRow) -> Result<()> {
        let mut upsert = self
            .connection
            .exec_bound::<(String, String, String, Option<i64>)>(indoc! {"
            INSERT INTO edges (from_plan, to_plan, kind, order_rank)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(from_plan, to_plan, kind) DO UPDATE SET
                order_rank = excluded.order_rank
        "})?;
        upsert((
            normalize_plan_id(&edge.from_plan),
            normalize_plan_id(&edge.to_plan),
            edge.kind.clone(),
            edge.order_rank,
        ))
    }

    pub fn upsert_category(&self, category: &CategoryRow) -> Result<()> {
        let mut upsert = self
            .connection
            .exec_bound::<(String, Option<String>, String, i64)>(indoc! {"
            INSERT INTO categories (id, parent_id, name, rank)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(id) DO UPDATE SET
                parent_id = excluded.parent_id,
                name = excluded.name,
                rank = excluded.rank
        "})?;
        upsert((
            category.id.clone(),
            category.parent_id.clone(),
            category.name.clone(),
            category.rank,
        ))
    }

    /// Write an assignment only when it actually changed, so a re-run over
    /// unchanged input leaves the row (including `assigned_at`) byte-identical.
    /// Returns whether the row was written.
    pub fn upsert_plan_category(&self, row: &PlanCategoryRow) -> Result<bool> {
        if let Some(existing) = self.plan_category(&row.plan_id)? {
            if existing.category_id == row.category_id
                && existing.source == row.source
                && existing.input_hash == row.input_hash
            {
                return Ok(false);
            }
        }

        let now = chrono::Utc::now().to_rfc3339();
        let mut upsert = self
            .connection
            .exec_bound::<(String, String, String, String, String)>(indoc! {"
            INSERT INTO plan_categories (plan_id, category_id, source, input_hash, assigned_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(plan_id) DO UPDATE SET
                category_id = excluded.category_id,
                source = excluded.source,
                input_hash = excluded.input_hash,
                assigned_at = excluded.assigned_at
        "})?;
        upsert((
            normalize_plan_id(&row.plan_id),
            row.category_id.clone(),
            row.source.clone(),
            row.input_hash.clone(),
            now,
        ))?;
        Ok(true)
    }

    pub fn delete_plan_category(&self, plan_id: &str) -> Result<()> {
        let plan_id = normalize_plan_id(plan_id);
        let mut delete = self
            .connection
            .exec_bound::<String>("DELETE FROM plan_categories WHERE plan_id = ?1")?;
        delete(plan_id)
    }

    pub fn upsert_plan_parent(&self, parent: &PlanParentRow) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut upsert = self
            .connection
            .exec_bound::<(String, String, String)>(indoc! {"
            INSERT INTO plan_parents (plan_id, parent_id, source, updated_at)
            VALUES (?1, ?2, 'parent', ?3)
            ON CONFLICT(plan_id) DO UPDATE SET
                parent_id = excluded.parent_id,
                updated_at = excluded.updated_at
        "})?;
        upsert((
            normalize_plan_id(&parent.plan_id),
            normalize_plan_id(&parent.parent_id),
            now,
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
    fn debugger_scope_round_trips() {
        let store = store();
        store
            .apply_graph_snapshot(&snapshot("PLAN_A", "implement", 0))
            .unwrap();

        assert!(!store.debugger_scoped("PLAN_A").unwrap());
        assert!(!store.debugger_scoped("MISSING").unwrap());

        store.set_debugger_scoped("PLAN_A", true).unwrap();
        assert!(store.debugger_scoped("PLAN_A").unwrap());

        store.set_debugger_scoped("PLAN_A", false).unwrap();
        assert!(!store.debugger_scoped("PLAN_A").unwrap());
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

        store
            .upsert_plan_metadata(&plan("P1", "plan", 200))
            .unwrap();
        store
            .upsert_plan_metadata(&plan("P3H", "draft", 600))
            .unwrap();
        store
            .upsert_plan_metadata(&plan("DEFERRED", "plan", 9999))
            .unwrap();

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

    #[test]
    fn normalize_plan_id_aliases_filenames_to_their_stem() {
        assert_eq!(
            normalize_plan_id("PLAN_OF_ACTION-2026-11-19-interactive-graph-roadmap.md"),
            "plan-of-action-2026-11-19-interactive-graph-roadmap"
        );
        assert_eq!(
            normalize_plan_id("KEEP_AGENT_CHECKPOINTS_DURABLE.md"),
            "keep-agent-checkpoints-durable"
        );
        // A non-filename id keeps the caller's spelling.
        assert_eq!(
            normalize_plan_id("add-deepseek-vision-models"),
            "add-deepseek-vision-models"
        );
    }

    #[test]
    fn a_graph_tool_written_plan_shows_its_kebab_edges() {
        let store = store();
        store
            .upsert_plan_metadata(&RoadmapPlanRow {
                id: "plan-of-action-2026-11-19-x".to_string(),
                name: "PLAN_OF_ACTION-2026-11-19-x".to_string(),
                repo: Some("zed-debugger-demo".to_string()),
                stage: Some("plan".to_string()),
                status: Some("in progress".to_string()),
                order_rank: None,
            })
            .unwrap();
        store
            .upsert_edge(&EdgeRow {
                from_plan: "plan-of-action-2026-11-19-x".to_string(),
                to_plan: "graph-engineering-ui-control-surface".to_string(),
                kind: "related".to_string(),
                order_rank: None,
            })
            .unwrap();

        // The graph tool addresses the same plan by its source filename.
        store
            .apply_graph_snapshot(&snapshot("PLAN_OF_ACTION-2026-11-19-x.md", "fork", 0))
            .unwrap();

        let plans = store.fleet_plans().unwrap();
        assert_eq!(
            plans.len(),
            1,
            "the filename write aliases onto the kebab row"
        );
        assert_eq!(plans[0].id, "plan-of-action-2026-11-19-x");
        assert_eq!(plans[0].current_node.as_deref(), Some("fork"));

        assert_eq!(store.edges().unwrap().len(), 1);
        assert_eq!(
            store
                .plan("PLAN_OF_ACTION-2026-11-19-x.md")
                .unwrap()
                .unwrap()
                .id,
            "plan-of-action-2026-11-19-x"
        );
    }

    #[test]
    fn migration_merges_filename_twins_and_remaps_dependents() {
        let store = store();
        let exec = |sql: &str| store.connection.exec(sql).unwrap()().unwrap();
        exec(indoc! {"
            INSERT INTO plans (id, name, repo, stage, status, updated_at)
            VALUES ('foo-bar', 'FOO_BAR', 'zed-debugger-demo', 'plan', 'in progress',
                    '2026-01-01T00:00:00Z')
        "});
        exec(indoc! {"
            INSERT INTO plans (id, name, repo, current_node, updated_at)
            VALUES ('FOO_BAR.md', 'FOO_BAR.md', 'zed', 'fork', '2026-01-02T00:00:00Z')
        "});
        exec(
            "INSERT INTO plan_nodes (plan_id, node, completed_at) VALUES ('FOO_BAR.md', 'plan', '2026-01-02T00:00:00Z')",
        );
        exec(
            "INSERT INTO evidence (plan_id, node, updated_at) VALUES ('FOO_BAR.md', 'plan', '2026-01-02T00:00:00Z')",
        );
        exec(
            "INSERT INTO edges (from_plan, to_plan, kind) VALUES ('FOO_BAR.md', 'other', 'related')",
        );
        exec(
            "INSERT INTO edges (from_plan, to_plan, kind) VALUES ('before', 'FOO_BAR.md', 'blocked_by')",
        );

        PlanStore::dedupe_plan_identities(&store.connection).unwrap();

        let plans = store.fleet_plans().unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "foo-bar");
        assert_eq!(plans[0].repo.as_deref(), Some("zed-debugger-demo"));
        assert_eq!(plans[0].current_node.as_deref(), Some("fork"));
        assert_eq!(store.done_nodes("foo-bar").unwrap(), vec!["plan"]);
        assert_eq!(store.evidence_for("foo-bar").unwrap().len(), 1);

        let mut edge_select = store
            .connection
            .select::<(String, String, String)>(
                "SELECT from_plan, to_plan, kind FROM edges ORDER BY from_plan",
            )
            .unwrap();
        assert_eq!(
            edge_select().unwrap(),
            vec![
                (
                    "before".to_string(),
                    "foo-bar".to_string(),
                    "blocked_by".to_string()
                ),
                (
                    "foo-bar".to_string(),
                    "other".to_string(),
                    "related".to_string()
                ),
            ]
        );
    }

    #[test]
    fn clean_dangling_edges_drops_unknown_ids_and_resolves_prose() {
        let store = store();
        store
            .upsert_plan_metadata(&RoadmapPlanRow {
                id: "interactive-graph-roadmap".to_string(),
                name: "INTERACTIVE_GRAPH_ROADMAP".to_string(),
                repo: Some("zed-debugger-demo".to_string()),
                stage: Some("plan".to_string()),
                status: None,
                order_rank: None,
            })
            .unwrap();
        store
            .upsert_plan_metadata(&RoadmapPlanRow {
                id: "dispatch".to_string(),
                name: "UI_DRIVEN_DEV_FLOW_DISPATCH".to_string(),
                repo: Some("zed-debugger-demo".to_string()),
                stage: Some("draft".to_string()),
                status: None,
                order_rank: None,
            })
            .unwrap();
        let exec = |sql: &str| store.connection.exec(sql).unwrap()().unwrap();
        exec(
            "INSERT INTO edges (from_plan, to_plan, kind) VALUES ('dispatch', 'interactive-graph-roadmap (the read-only graph slice must land and', 'blocked_by')",
        );
        exec(
            "INSERT INTO edges (from_plan, to_plan, kind) VALUES ('suite', 'browser-test', 'related')",
        );
        exec(
            "INSERT INTO edges (from_plan, to_plan, kind) VALUES ('interactive-graph-roadmap', 'interactive-graph-roadmap', 'related')",
        );

        PlanStore::clean_dangling_edges(&store.connection).unwrap();

        let edges = store.edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from_plan, "dispatch");
        assert_eq!(edges[0].to_plan, "interactive-graph-roadmap");
        assert_eq!(edges[0].kind, "blocked_by");
    }

    #[test]
    fn migrate_adds_the_debugger_scoped_column_to_an_old_store() {
        let connection = Connection::open_memory(None);
        connection
            .exec(indoc! {"
                CREATE TABLE plans (
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
            "})
            .unwrap()()
        .unwrap();
        connection
            .exec("INSERT INTO plans (id, name, updated_at) VALUES ('OLD', 'OLD', '2026-01-01T00:00:00Z')")
            .unwrap()()
            .unwrap();

        let store = PlanStore::init(connection).expect("migrate an old store");
        assert!(!store.debugger_scoped("OLD").unwrap());
        store.set_debugger_scoped("OLD", true).unwrap();
        assert!(store.debugger_scoped("OLD").unwrap());
    }

    #[test]
    fn lifecycle_classifier_reads_prose_statuses() {
        assert_eq!(
            classify_lifecycle(None, Some("done — 3/3 phases accepted"), None),
            Lifecycle::History
        );
        assert_eq!(
            classify_lifecycle(None, Some("rejected"), None),
            Lifecycle::History
        );
        assert_eq!(
            classify_lifecycle(Some("done"), None, None),
            Lifecycle::History
        );
        assert_eq!(
            classify_lifecycle(None, None, Some("complete")),
            Lifecycle::History
        );

        assert_eq!(
            classify_lifecycle(Some("plan"), Some("in progress — all five phases"), None),
            Lifecycle::Live
        );
        assert_eq!(
            classify_lifecycle(Some("draft"), None, Some("fork")),
            Lifecycle::Live
        );
        assert_eq!(
            classify_lifecycle(
                None,
                Some("2026-10-13 (parked — waiting on upstream)"),
                None
            ),
            Lifecycle::Live
        );
        assert_eq!(
            classify_lifecycle(Some("plan  (promoted from draft)"), None, None),
            Lifecycle::Live
        );
    }

    #[test]
    fn categories_and_assignments_round_trip() {
        let store = store();
        store
            .upsert_category(&CategoryRow {
                id: "feature".to_string(),
                parent_id: None,
                name: "Feature".to_string(),
                rank: 100,
            })
            .unwrap();
        store
            .upsert_category(&CategoryRow {
                id: "feature/agent-core".to_string(),
                parent_id: Some("feature".to_string()),
                name: "Agent Core".to_string(),
                rank: 110,
            })
            .unwrap();
        store
            .upsert_plan_metadata(&RoadmapPlanRow {
                id: "PLAN_A".to_string(),
                name: "PLAN_A".to_string(),
                repo: Some("zed".to_string()),
                stage: Some("plan".to_string()),
                status: None,
                order_rank: None,
            })
            .unwrap();
        assert!(
            store
                .upsert_plan_category(&PlanCategoryRow {
                    plan_id: "PLAN_A".to_string(),
                    category_id: "feature/agent-core".to_string(),
                    source: "auto".to_string(),
                    input_hash: "h1".to_string(),
                })
                .unwrap()
        );

        let categories = store.categories().unwrap();
        assert_eq!(categories.len(), 2);
        assert_eq!(categories[0].id, "feature");
        assert_eq!(categories[1].parent_id.as_deref(), Some("feature"));

        let plans = store.categorized_plans().unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].category_id.as_deref(), Some("feature/agent-core"));
        assert_eq!(plans[0].category_source.as_deref(), Some("auto"));
        assert_eq!(plans[0].lifecycle(), Lifecycle::Live);
    }

    #[test]
    fn plan_category_write_is_idempotent() {
        let store = store();
        let category = PlanCategoryRow {
            plan_id: "PLAN_A".to_string(),
            category_id: "feature".to_string(),
            source: "auto".to_string(),
            input_hash: "h1".to_string(),
        };
        assert!(store.upsert_plan_category(&category).unwrap());

        let mut select = store
            .connection
            .select_bound::<String, String>(
                "SELECT assigned_at FROM plan_categories WHERE plan_id = ?1",
            )
            .unwrap();
        let timestamp_before = select("PLAN_A".to_string()).unwrap().remove(0);

        assert!(!store.upsert_plan_category(&category).unwrap());
        let timestamp_after = select("PLAN_A".to_string()).unwrap().remove(0);
        assert_eq!(timestamp_before, timestamp_after);

        let changed = PlanCategoryRow {
            category_id: "feature/agent-core".to_string(),
            ..category
        };
        assert!(store.upsert_plan_category(&changed).unwrap());
    }

    #[test]
    fn plan_parents_round_trip() {
        let store = store();
        store
            .upsert_plan_parent(&PlanParentRow {
                plan_id: "PLAN_OF_ACTION-x.md".to_string(),
                parent_id: "parent-plan".to_string(),
            })
            .unwrap();

        let parents = store.plan_parents().unwrap();
        assert_eq!(parents.len(), 1);
        assert_eq!(parents[0].plan_id, "plan-of-action-x");
        assert_eq!(parents[0].parent_id, "parent-plan");
    }
}
