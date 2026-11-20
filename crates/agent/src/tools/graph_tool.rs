use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{AgentTool, Thread, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use chrono::Utc;
#[cfg(test)]
use chrono::DateTime;
use gpui::{App, AppContext as _, AsyncApp, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The operation to perform on a plan's dev-flow progress state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GraphOperation {
    /// Create the plan's initial `progress.json`.
    Init,
    /// Read the current progress state (current node, done, next, blocker).
    #[default]
    Read,
    /// Advance to a node; refused unless every prerequisite gate is done.
    Advance,
    /// Start a node: records which executor owns it and when its active window
    /// began. Refused unless the node is ready and no other node is active.
    Begin,
    /// Refresh a node's activity timestamp so a long, legitimate wait (a remote
    /// build queue, a human gate) is not mistaken for a stalled executor.
    Heartbeat,
    /// Compute the deterministic next decision: dispatch, continue, pause, stop.
    Coordinate,
    /// Clear a recorded blocker. A stop is resumable only through this explicit
    /// operation; the automatic loop never resumes itself.
    Resume,
}

/// Read, advance, and coordinate a plan's dev-flow progress state.
///
/// The dev flow (plan → promote → claim → fork → implement → verify → compile →
/// accept → merge → release → complete) is a graph of nodes and edges. Each plan
/// tracks its position in a `progress.json` next to the plan file. `advance` is
/// guarded: a transition is refused unless every prerequisite node is done, so a
/// "skip testing and merge" request cannot pass without `verify`/`compile`/`accept`.
///
/// Edges carry semantics beyond the bare route (plan 1.23c, Layers B–D):
/// - **Artifact contract (D11).** Each node declares the shape its artifact must
///   satisfy; a transition is refused on a malformed artifact, scripted rather
///   than model judgment.
/// - **Bounded retry (D12).** A failed node may be re-attempted up to a fixed
///   budget, each retry carrying a structured failure record fed into the next
///   attempt; exhausting the budget blocks further retries.
/// - **Cost and latency (D13).** Each attempt records its tokens and wall-clock;
///   a plan states a budget at `init`; below the fast-path threshold the graph
///   is not worth traversing (see `FAST_PATH_THRESHOLD_TOKENS`).
///
/// The coordinator (plan 1.23d, Layers C/F) adds:
/// - **Declared back-edges.** A failed gate may re-open `implement`; the edge is
///   declared and its traversals are counted, so `verify → implement` is
///   expressible and a second traversal stops.
/// - **Break predicates, not limits.** The loop stops on strict conditions
///   (attempt budget, same error, no artifact change, gate-failure streak,
///   output decline, cross-node cycle, stall) and pauses at human gates. There
///   is no spend ceiling.
/// - **A burn monitor.** `executor` and `active_ms` are recorded per attempt, and
///   non-blocking warnings compare spend against accepted artifacts. A repeated
///   warning pauses for a human; nothing silently aborts.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GraphToolInput {
    /// Which operation to perform.
    pub operation: GraphOperation,
    /// Path to the plan's `progress.json`.
    pub path: String,
    /// Plan file name (required for `init`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Repo the plan belongs to (required for `init`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Node to advance to (required for `advance` and `begin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Proof the node is done — commit sha, gate exit code, or report path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// For `advance`: whether the change touches protected surface (derived by
    /// the D17 predicate; passed by the caller, never stored in progress.json).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protected: Option<bool>,
    /// For `advance`: whether the gate being advanced failed. A protected change
    /// with a failed verification gate blocks the merge transition (D17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
    /// For `advance`: the structured failure record for a failed attempt (D12) —
    /// what failed, why, and the suggested correction. Required when `failed` is
    /// true, so a retry edge always carries the record it feeds forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureRecord>,
    /// For `advance`: the per-node cost and latency of this attempt (D13), in the
    /// four token fields the fork's eval harness reports plus wall-clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<NodeCost>,
    /// For `init`: the plan's stated budget (D13, inert data — nothing enforces it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<CostBudget>,
    /// For `advance`: the node type's baseline, supplied by the harness
    /// (`zed-tools/flow/`). Without it the burn warnings are `provisional`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<NodeBaseline>,
    /// For `coordinate`: whether the executor is explicitly asking for a decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<bool>,
    /// For `init`: the plan's scope. `"debugger"` marks the plan debugger-scoped,
    /// co-writing the store's `debugger_scoped` flag so the plan graph renders the
    /// conditional `stage→…→fix` sub-graph (1.29 D8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GraphToolOutput {
    Success { message: String },
    Error { error: String },
}

impl From<GraphToolOutput> for LanguageModelToolResultContent {
    fn from(value: GraphToolOutput) -> Self {
        match value {
            GraphToolOutput::Success { message } => message.into(),
            GraphToolOutput::Error { error } => error.into(),
        }
    }
}

pub use plan_flow::{
    ActiveNode, Attempt, AuthorizedGate, BackEdgeTraversal, Blocker, BurnWarning, CostBudget,
    DoneEntry, FailureRecord, GraphState, NodeBaseline, NodeCost, advance, begin, brief_for,
    burn_warnings, coordinate, derive_next, heartbeat, is_human_gate, next_decision_node,
    next_ready, parse_time, prereqs, resume, retry_candidate, spent_tokens, validate_artifact,
    BACK_EDGES, DEBUGGER_SCOPE, FAST_PATH_THRESHOLD_TOKENS, GATE_FAILURE_STREAK, GATE_NODES,
    HUMAN_GATES, MIN_BASELINE_SAMPLES, OUTPUT_DECLINE_PERCENT, PROVISIONAL_P90_TOKENS,
    RETRY_BUDGET, ROUTE, STALL_SECONDS,
};

pub struct GraphTool {
    /// The owning thread's session id, captured when the tool is registered.
    /// Reading the `Thread` entity inside `run` would panic: the tool is
    /// dispatched while the thread is already being updated.
    executor: String,
    /// The owning thread, so a successful operation can record the plan it is
    /// executing. It is only touched from a deferred foreground task, never
    /// from `run`'s synchronous body (see `executor`).
    thread: WeakEntity<Thread>,
}

impl GraphTool {
    pub fn new(executor: String, thread: WeakEntity<Thread>) -> Self {
        Self { executor, thread }
    }
}

impl AgentTool for GraphTool {
    type Input = GraphToolInput;
    type Output = GraphToolOutput;

    const NAME: &'static str = "graph";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => match input.operation {
                GraphOperation::Init => format!(
                    "Initializing dev-flow state for {}",
                    input.plan.as_deref().unwrap_or_default()
                )
                .into(),
                GraphOperation::Read => "Reading dev-flow progress".into(),
                GraphOperation::Advance => {
                    format!("Advancing to {}", input.node.as_deref().unwrap_or_default()).into()
                }
                GraphOperation::Begin => {
                    format!("Beginning {}", input.node.as_deref().unwrap_or_default()).into()
                }
                GraphOperation::Heartbeat => "Recording dev-flow activity".into(),
                GraphOperation::Coordinate => "Computing the next dev-flow decision".into(),
                GraphOperation::Resume => "Resuming a stopped dev-flow plan".into(),
            },
            Err(_) => "Dev-flow graph".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let executor = self.executor.clone();
        let thread = self.thread.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| GraphToolOutput::Error {
                error: format!("Failed to receive graph tool input: {error}"),
            })?;
            let path = resolve_graph_path(&thread, &input.path, cx);
            let path_string = path.to_string_lossy().into_owned();
            let consulted = matches!(
                input.operation,
                GraphOperation::Read | GraphOperation::Coordinate
            );
            let output = cx
                .background_spawn(async move { run_operation(input, path, Some(executor.as_str())) })
                .await;
            if output.is_ok() {
                if let Err(error) = thread.update(cx, |thread, cx| {
                    thread.set_plan_path(Some(path_string), cx);
                    if consulted {
                        thread.record_graph_consulted();
                    }
                }) {
                    log::error!("[GRAPH] failed to record plan path on thread: {error:#}");
                }
            }
            output
        })
    }
}

/// Resolve the graph tool's `path`. Absolute paths pass through unchanged; a
/// relative path is joined onto the worktree root whose name matches its first
/// component (mirroring the file tools), because a bare relative path would
/// otherwise resolve against the process CWD — one level above the workspace
/// the model's file tools see. Headless/eval contexts with no matching root
/// fall back to the process CWD, preserving the previous behavior.
fn resolve_graph_path(thread: &WeakEntity<Thread>, raw: &str, cx: &mut AsyncApp) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let resolved = thread
        .update(cx, |thread, cx| {
            thread.project.read_with(cx, |project, cx| {
                project
                    .find_project_path(path, cx)
                    .and_then(|project_path| project.absolute_path(&project_path, cx))
            })
        })
        .ok()
        .flatten();
    resolved.unwrap_or_else(|| path.to_path_buf())
}

/// The forced-consultation banner for the plan at `path`. `None` when the
/// thread is not executing a plan, the state is unreadable, or the plan has no
/// node left to run — a banner must never fail a request, and a completed plan
/// must not tell the agent to assume an implementation role.
pub fn plan_state_banner(path: Option<&str>) -> Option<String> {
    let Some(path) = path else {
        return None;
    };
    let state = match load_state(Path::new(path)) {
        Ok(state) => state,
        // Degrade, don't vanish: a recorded-but-unreadable plan still tells the
        // agent to consult the graph instead of silently dropping the banner.
        Err(_) => {
            return Some(
                "## Plan state (forced)\n\n\
                 You are executing a plan through the dev-flow graph, but the plan's \
                 `progress.json` could not be read. Run `graph read` on it before making \
                 any change."
                    .to_string(),
            );
        }
    };
    let done: Vec<String> = state.done.iter().map(|entry| entry.node.clone()).collect();
    let next = next_decision_node(&state)?;
    let status = if let Some(blocker) = &state.blocker {
        if blocker.edge == "human_gate" {
            Some("a human gate is waiting for authorization — pause and ask, then `graph resume`")
        } else {
            Some("blocked — `graph resume` with the human's decision")
        }
    } else if state.active.is_some() {
        Some("a node is active — `graph coordinate` returns `continue`")
    } else if retry_candidate(&state).is_some() {
        Some("a gate failed on a previous attempt — retry it before advancing")
    } else {
        None
    };
    Some(crate::plan_state_framing(&next, &done, status))
}

/// The next ready node for a plan's `progress.json`, or `None` when the state is
/// unreadable. Used by the Layer 2 tool-permission gate (plan 1.31).
pub fn next_plan_node(plan_path: &str) -> Option<String> {
    let state = load_state(Path::new(plan_path)).ok()?;
    next_decision_node(&state)
}

/// Tools that mutate source or working-tree state. The plan gate blocks these
/// while a gate node is ready and unadvanced. `git` is matched by operation
/// instead, because its read-only operations must stay available.
const PLAN_GATED_GIT_OPERATIONS: &[&str] = &["commit", "merge", "push"];

/// Whether this tool call is a mutation the plan gate covers. Source-mutating
/// tools are caught by their `ToolKind` (`Edit`/`Delete`/`Move`); git is caught
/// by operation because its kind is `Execute` regardless of read vs write.
pub fn gate_covers_tool(tool_name: &str, kind: acp::ToolKind, input_values: &[String]) -> bool {
    if matches!(
        kind,
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move
    ) {
        return true;
    }
    tool_name == "git"
        && input_values.first().is_some_and(|value| {
            value.strip_prefix("operation:").is_some_and(|operation| {
                PLAN_GATED_GIT_OPERATIONS
                    .iter()
                    .any(|gated| operation.starts_with(gated))
            })
        })
}

fn load_state(path: &Path) -> Result<GraphState, GraphToolOutput> {
    plan_flow::load_state(path).map_err(|error| GraphToolOutput::Error { error })
}

fn save_state(path: &Path, state: &GraphState) -> Result<(), GraphToolOutput> {
    plan_flow::save_state(path, state).map_err(|error| GraphToolOutput::Error { error })?;
    persist_to_store(state);
    Ok(())
}

/// Project the in-memory graph state into the store's snapshot shape. This is
/// the state-of-record write: co-persisted at save time from the same
/// `GraphState` that writes `progress.json`, never re-ingested from it later.
fn graph_snapshot(state: &GraphState) -> plan_store::GraphSnapshot {
    plan_store::GraphSnapshot {
        plan_id: state.plan.clone(),
        name: state.plan.clone(),
        repo: Some(state.repo.clone()),
        current_node: state.current.clone(),
        active_node: state.active.as_ref().map(|active| active.node.clone()),
        holder: state.active.as_ref().map(|active| active.executor.clone()),
        heartbeat: state.active.as_ref().map(|active| {
            if !active.last_activity_at.is_empty() {
                active.last_activity_at.clone()
            } else {
                active.started_at.clone()
            }
        }),
        blocked_reason: state.blocker.as_ref().map(|blocker| blocker.reason.clone()),
        spent_tokens: spent_tokens(state),
        done_nodes: state.done.iter().map(|entry| entry.node.clone()).collect(),
        evidence: state
            .done
            .iter()
            .map(|entry| plan_store::EvidenceRow {
                plan_id: state.plan.clone(),
                node: entry.node.clone(),
                artifact_sha: Some(entry.artifact.clone()),
                gate_exit_code: Some(i64::from(entry.failed)),
                report_path: None,
                falsifying_condition: None,
                negative_control: None,
            })
            .collect(),
    }
}

/// Write the snapshot to the SQLite plan store. Best-effort: `progress.json`
/// remains the hard truth for the traversal, so a store write failure must not
/// fail the graph operation — it is logged and the next save retries.
#[cfg(not(test))]
fn persist_to_store(state: &GraphState) {
    let snapshot = graph_snapshot(state);
    let debugger_scoped = state.scope.as_deref() == Some(DEBUGGER_SCOPE);
    if let Err(error) = (|| -> Result<()> {
        let store = plan_store::PlanStore::open_default()?;
        store.apply_graph_snapshot(&snapshot)?;
        store.set_debugger_scoped(&state.plan, debugger_scoped)?;
        Ok(())
    })() {
        log::error!("[GRAPH] failed to persist plan state to the store: {error:#}");
    }
}

#[cfg(test)]
fn persist_to_store(_state: &GraphState) {}

fn state_json(state: &GraphState) -> String {
    serde_json::to_string(state).unwrap_or_else(|_| "{}".to_string())
}

fn run_operation(
    input: GraphToolInput,
    path: PathBuf,
    executor: Option<&str>,
) -> Result<GraphToolOutput, GraphToolOutput> {
    match input.operation {
        GraphOperation::Init => {
            let plan = input.plan.ok_or_else(|| GraphToolOutput::Error {
                error: "`plan` is required for `operation: \"init\"`.".to_string(),
            })?;
            let repo = input.repo.ok_or_else(|| GraphToolOutput::Error {
                error: "`repo` is required for `operation: \"init\"`.".to_string(),
            })?;
            let state = GraphState {
                plan,
                repo,
                scope: input.scope,
                current: None,
                done: Vec::new(),
                next: vec!["plan".to_string()],
                blocker: None,
                budget: input.budget,
                active: None,
                back_edges: Vec::new(),
                warnings: Vec::new(),
                authorized: None,
            };
            save_state(&path, &state)?;
            Ok(GraphToolOutput::Success {
                message: format!(
                    "Initialized dev-flow state: {}\nfast_path_threshold_tokens: {FAST_PATH_THRESHOLD_TOKENS}",
                    state_json(&state)
                ),
            })
        }
        GraphOperation::Read => {
            let mut state = load_state(&path)?;
            state.next = derive_next(&state);
            Ok(GraphToolOutput::Success {
                message: format!(
                    "{}\nfast_path_threshold_tokens: {FAST_PATH_THRESHOLD_TOKENS}",
                    state_json(&state)
                ),
            })
        }
        GraphOperation::Advance => {
            let node = input.node.ok_or_else(|| GraphToolOutput::Error {
                error: "`node` is required for `operation: \"advance\"`.".to_string(),
            })?;
            let artifact = input.artifact.unwrap_or_default();
            let mut state = load_state(&path)?;
            advance(
                &mut state,
                &node,
                &artifact,
                input.protected.unwrap_or(false),
                input.failed.unwrap_or(false),
                input.failure,
                input.cost,
                executor,
                input.baseline.as_ref(),
            )
            .map_err(|error| GraphToolOutput::Error { error })?;
            save_state(&path, &state)?;
            Ok(GraphToolOutput::Success {
                message: format!("Advanced to {node}: {}", state_json(&state)),
            })
        }
        GraphOperation::Begin => {
            let node = input.node.ok_or_else(|| GraphToolOutput::Error {
                error: "`node` is required for `operation: \"begin\"`.".to_string(),
            })?;
            let executor = executor.ok_or_else(|| GraphToolOutput::Error {
                error: "`begin` needs an executor identity from the calling thread".to_string(),
            })?;
            let mut state = load_state(&path)?;
            begin(&mut state, &node, executor, Utc::now())
                .map_err(|error| GraphToolOutput::Error { error })?;
            save_state(&path, &state)?;
            Ok(GraphToolOutput::Success {
                message: format!("Began {node}: {}", state_json(&state)),
            })
        }
        GraphOperation::Heartbeat => {
            let mut state = load_state(&path)?;
            heartbeat(&mut state, Utc::now())
                .map_err(|error| GraphToolOutput::Error { error })?;
            save_state(&path, &state)?;
            Ok(GraphToolOutput::Success {
                message: format!("Heartbeat recorded: {}", state_json(&state)),
            })
        }
        GraphOperation::Coordinate => {
            let mut state = load_state(&path)?;
            let decision = coordinate(&mut state, input.ask.unwrap_or(false), Utc::now());
            save_state(&path, &state)?;
            let decision = serde_json::to_string(&decision).unwrap_or_else(|_| "{}".to_string());
            Ok(GraphToolOutput::Success {
                message: format!("{decision}\nstate: {}", state_json(&state)),
            })
        }
        GraphOperation::Resume => {
            let artifact = input.artifact.unwrap_or_default();
            let mut state = load_state(&path)?;
            let message = resume(&mut state, &artifact, Utc::now())
                .map_err(|error| GraphToolOutput::Error { error })?;
            save_state(&path, &state)?;
            Ok(GraphToolOutput::Success {
                message: format!("{message}\nstate: {}", state_json(&state)),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "a1b2c3d4e5f6";

    fn failed_record() -> FailureRecord {
        FailureRecord {
            what_failed: "verify".to_string(),
            why: "clippy".to_string(),
            suggestion: "fix the lint".to_string(),
        }
    }

    fn distinct_failure(index: usize) -> FailureRecord {
        FailureRecord {
            what_failed: format!("gate-{index}"),
            why: format!("reason-{index}"),
            suggestion: "try again".to_string(),
        }
    }

    fn cost() -> NodeCost {
        NodeCost {
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 30,
            wall_clock_ms: 1500,
            executor: None,
            active_ms: None,
        }
    }

    fn fresh() -> GraphState {
        GraphState {
            plan: "TRIVIAL".to_string(),
            repo: "zed".to_string(),
            scope: None,
            current: None,
            done: Vec::new(),
            next: vec!["plan".to_string()],
            blocker: None,
            budget: None,
            active: None,
            back_edges: Vec::new(),
            warnings: Vec::new(),
            authorized: None,
        }
    }

    fn step(state: &mut GraphState, node: &str) {
        let artifact = if matches!(node, "compile" | "merge" | "release") {
            SHA
        } else {
            "artifact"
        };
        advance(state, node, artifact, false, false, None, None, None, None).unwrap();
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-15T12:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::seconds(seconds)
    }

    /// Drive the human-gate pause → resume → authorize sequence a test needs
    /// before advancing `merge`/`release`/`complete`.
    fn mint_authorization(state: &mut GraphState, node: &str) {
        coordinate(state, false, at(0));
        resume(state, "approved in test", at(1)).unwrap();
        assert_eq!(state.authorized.as_ref().map(|a| a.node.as_str()), Some(node));
    }

    /// Set a gate authorization directly, for tests that exercise the D17 edge
    /// guard (where the coordinator would prefer a retry over pausing at the
    /// gate), so the authorization path is not what is under test.
    fn force_authorization(state: &mut GraphState, node: &str) {
        state.authorized = Some(AuthorizedGate {
            node: node.to_string(),
            artifact: "approved in test".to_string(),
            at: at(1).to_rfc3339(),
        });
    }

    #[test]
    fn happy_path_walks_the_route() {
        let mut state = fresh();
        for node in ROUTE {
            if HUMAN_GATES.contains(node) {
                mint_authorization(&mut state, node);
            }
            advance(&mut state, node, SHA, false, false, None, None, None, None).unwrap();
        }
        assert_eq!(state.current.as_deref(), Some("complete"));
        assert!(state.next.is_empty());
    }

    #[test]
    fn edge_guard_refuses_skip_to_merge() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        let error =
            advance(&mut state, "merge", SHA, false, false, None, None, None, None).unwrap_err();
        assert!(error.contains("verify"), "error was: {error}");
        assert!(error.contains("compile"), "error was: {error}");
        assert!(error.contains("accept"), "error was: {error}");
    }

    #[test]
    fn protected_change_blocks_merge_on_failed_verify() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            Some(failed_record()),
            None,
            None,
            None,
        )
        .unwrap();
        step(&mut state, "compile");
        step(&mut state, "accept");
        let error =
            advance(&mut state, "merge", SHA, true, false, None, None, None, None).unwrap_err();
        assert!(error.contains("protected"), "error was: {error}");
    }

    #[test]
    fn protected_change_allows_merge_when_verification_passed() {
        let mut state = fresh();
        for node in [
            "plan",
            "promote",
            "claim",
            "fork",
            "implement",
            "verify",
            "compile",
            "accept",
        ] {
            step(&mut state, node);
        }
        mint_authorization(&mut state, "merge");
        advance(&mut state, "merge", SHA, true, false, None, None, None, None).unwrap();
        assert_eq!(state.current.as_deref(), Some("merge"));
    }

    #[test]
    fn unprotected_change_allows_merge_even_if_verification_failed() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            Some(failed_record()),
            None,
            None,
            None,
        )
        .unwrap();
        step(&mut state, "compile");
        step(&mut state, "accept");
        force_authorization(&mut state, "merge");
        advance(&mut state, "merge", SHA, false, false, None, None, None, None).unwrap();
        assert_eq!(state.current.as_deref(), Some("merge"));
    }

    #[test]
    fn a_failed_gate_is_retried_before_its_successor() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        advance(
            &mut state,
            "verify",
            "report-1",
            false,
            true,
            Some(failed_record()),
            None,
            None,
            None,
        )
        .unwrap();
        let decision = coordinate(&mut state, false, at(0));
        assert_eq!(decision.action, "dispatch");
        assert_eq!(decision.node.as_deref(), Some("verify"));
        assert!(decision.brief.unwrap().contains("back-edge"));

        // The edge guard stays D17-advisory for successors; the coordinator is
        // what prefers the retry, so an unprotected flow may still proceed.
        advance(&mut state, "compile", SHA, false, false, None, None, None, None).unwrap();
    }

    #[test]
    fn derive_next_matches_prereqs() {
        let mut state = fresh();
        state.done = vec![DoneEntry {
            node: "plan".to_string(),
            artifact: String::new(),
            at: String::new(),
            failed: false,
            attempts: Vec::new(),
            cost: None,
        }];
        assert_eq!(derive_next(&state), vec!["promote".to_string()]);
    }

    fn done_entry(node: &str) -> DoneEntry {
        DoneEntry {
            node: node.to_string(),
            artifact: String::new(),
            at: String::new(),
            failed: false,
            attempts: Vec::new(),
            cost: None,
        }
    }

    #[test]
    fn plan_state_banner_names_the_next_role() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let mut state = fresh();
        state.done = ["plan", "promote", "claim", "fork"]
            .into_iter()
            .map(done_entry)
            .collect();
        save_state(&path, &state).unwrap();

        let banner = plan_state_banner(path.to_str()).expect("banner for an active plan");
        assert!(banner.contains("Next node: `implement`"));
        assert!(banner.contains("Role to assume: implementer"));
    }

    #[test]
    fn completed_plan_yields_no_banner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let mut state = fresh();
        state.done = ROUTE.iter().map(|node| done_entry(node)).collect();
        save_state(&path, &state).unwrap();

        assert!(plan_state_banner(path.to_str()).is_none());
    }

    #[test]
    fn plan_gate_covers_mutations_only() {
        let input = |value: &str| vec![value.to_string()];

        assert!(gate_covers_tool("write_file", acp::ToolKind::Edit, &[]));
        assert!(gate_covers_tool("edit_file", acp::ToolKind::Edit, &[]));
        assert!(gate_covers_tool("delete_path", acp::ToolKind::Delete, &[]));
        // A mutating kind is refused even for a tool not on any name list.
        assert!(gate_covers_tool("some_future_tool", acp::ToolKind::Move, &[]));
        assert!(gate_covers_tool("git", acp::ToolKind::Execute, &input("operation:commit protected:false")));
        assert!(gate_covers_tool("git", acp::ToolKind::Execute, &input("operation:merge ref:feature")));
        assert!(gate_covers_tool("git", acp::ToolKind::Execute, &input("operation:push")));

        assert!(!gate_covers_tool("git", acp::ToolKind::Execute, &input("operation:status")));
        assert!(!gate_covers_tool("git", acp::ToolKind::Execute, &input("operation:diff")));
        assert!(!gate_covers_tool("read_file", acp::ToolKind::Read, &[]));
        assert!(!gate_covers_tool("terminal", acp::ToolKind::Execute, &[]));
    }

    #[test]
    fn next_plan_node_tracks_ready_gates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let mut state = fresh();
        state.done = ["plan", "promote", "claim", "fork", "implement"]
            .into_iter()
            .map(done_entry)
            .collect();
        save_state(&path, &state).unwrap();
        let plan_path = path.to_string_lossy().into_owned();

        assert_eq!(next_plan_node(&plan_path).as_deref(), Some("verify"));

        state.done.push(done_entry("verify"));
        save_state(&path, &state).unwrap();
        assert_eq!(next_plan_node(&plan_path).as_deref(), Some("compile"));

        state.done.push(done_entry("compile"));
        save_state(&path, &state).unwrap();
        assert_eq!(next_plan_node(&plan_path).as_deref(), Some("accept"));

        state.done.push(done_entry("accept"));
        save_state(&path, &state).unwrap();
        assert_eq!(next_plan_node(&plan_path).as_deref(), Some("merge"));
    }

    #[test]
    fn failed_gate_names_the_retry_not_the_successor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement", "verify", "compile"] {
            step(&mut state, node);
        }
        advance(
            &mut state,
            "accept",
            "failed accept",
            false,
            true,
            Some(failed_record()),
            None,
            None,
            None,
        )
        .unwrap();
        save_state(&path, &state).unwrap();
        let plan_path = path.to_string_lossy().into_owned();

        assert_eq!(next_plan_node(&plan_path).as_deref(), Some("accept"));

        let banner = plan_state_banner(path.to_str()).expect("banner for a retrying plan");
        assert!(banner.contains("Next node: `accept`"));
        assert!(!banner.contains("Next node: `merge`"));
        assert!(banner.contains("retry it before advancing"));
    }

    #[test]
    fn accept_brief_names_the_live_launch_requirement() {
        assert!(brief_for("accept", "PLAN", false).contains("live launch"));
    }

    #[test]
    fn accept_refuses_an_empty_artifact() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement", "verify", "compile"] {
            step(&mut state, node);
        }
        let error = advance(&mut state, "accept", "", false, false, None, None, None, None)
            .unwrap_err();
        assert!(error.contains("non-empty artifact"));
    }

    #[test]
    fn unreadable_plan_yields_a_degraded_banner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        std::fs::write(&path, "not valid json").unwrap();
        let banner = plan_state_banner(path.to_str()).expect("a degraded banner, not None");
        assert!(banner.contains("could not be read"));
        assert!(banner.contains("graph read"));
    }

    #[test]
    fn init_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.json");
        let output = run_operation(
            GraphToolInput {
                operation: GraphOperation::Init,
                path: path.to_string_lossy().into_owned(),
                plan: Some("PLAN_A".to_string()),
                repo: Some("zed".to_string()),
                node: None,
                artifact: None,
                protected: None,
                failed: None,
                failure: None,
                cost: None,
                budget: Some(CostBudget {
                    max_tokens: 100_000,
                    max_wall_clock_ms: 600_000,
                }),
                baseline: None,
                ask: None,
                scope: Some("debugger".to_string()),
            },
            path.clone(),
            Some("thread-1"),
        )
        .unwrap();
        assert!(matches!(output, GraphToolOutput::Success { .. }));

        let loaded = load_state(&path).unwrap();
        assert_eq!(loaded.plan, "PLAN_A");
        assert_eq!(loaded.next, vec!["plan".to_string()]);
        let budget = loaded.budget.expect("budget should be stored");
        assert_eq!(budget.max_tokens, 100_000);
        assert_eq!(budget.max_wall_clock_ms, 600_000);
        assert_eq!(loaded.scope.as_deref(), Some("debugger"));
    }

    #[test]
    fn malformed_artifact_is_refused() {
        let mut state = fresh();
        for node in [
            "plan",
            "promote",
            "claim",
            "fork",
            "implement",
            "verify",
            "compile",
            "accept",
        ] {
            step(&mut state, node);
        }
        let error = advance(
            &mut state,
            "merge",
            "not-a-sha",
            false,
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(error.contains("git commit sha"), "error was: {error}");
    }

    #[test]
    fn failed_attempt_requires_failure_record() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        let error = advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(error.contains("failure record"), "error was: {error}");
    }

    #[test]
    fn retry_is_bounded_and_exhausts_with_failure_record() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            Some(failed_record()),
            None,
            None,
            None,
        )
        .unwrap();
        for attempt in 0..(RETRY_BUDGET - 1) {
            let artifact = format!("report-{attempt}");
            advance(
                &mut state,
                "verify",
                &artifact,
                false,
                true,
                Some(distinct_failure(attempt)),
                None,
                None,
                None,
            )
            .unwrap_or_else(|error| panic!("attempt {attempt} should be allowed: {error}"));
        }
        let error = advance(
            &mut state,
            "verify",
            "report-final",
            false,
            true,
            Some(distinct_failure(9)),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(error.contains("exhausted"), "error was: {error}");
        assert!(error.contains("what_failed"), "error was: {error}");
    }

    #[test]
    fn retry_can_recover_to_passed() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            Some(failed_record()),
            None,
            None,
            None,
        )
        .unwrap();
        step(&mut state, "verify");
        let entry = state
            .done
            .iter()
            .find(|entry| entry.node == "verify")
            .unwrap();
        assert!(!entry.failed);
        assert_eq!(entry.attempts.len(), 2);
    }

    #[test]
    fn cost_is_recorded_per_node() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        advance(
            &mut state,
            "verify",
            "report",
            false,
            false,
            None,
            Some(cost()),
            Some("thread-9"),
            None,
        )
        .unwrap();
        let entry = state
            .done
            .iter()
            .find(|entry| entry.node == "verify")
            .unwrap();
        let recorded = entry.cost.as_ref().expect("cost should be recorded");
        assert_eq!(recorded.wall_clock_ms, 1500);
        assert_eq!(recorded.executor.as_deref(), Some("thread-9"));
        assert_eq!(
            recorded.input_tokens
                + recorded.output_tokens
                + recorded.cache_creation_input_tokens
                + recorded.cache_read_input_tokens,
            155
        );
    }

    #[test]
    fn human_gate_pauses_instead_of_dispatching() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept"]
        {
            step(&mut state, node);
        }
        let decision = coordinate(&mut state, false, at(0));
        assert_eq!(decision.action, "pause");
        assert_eq!(decision.node.as_deref(), Some("merge"));
        assert_eq!(decision.predicate.as_deref(), Some("human_gate"));
        assert!(!decision.fresh_executor);
    }

    #[test]
    fn coordinate_persists_a_human_gate_blocker() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept"]
        {
            step(&mut state, node);
        }
        let decision = coordinate(&mut state, false, at(0));
        assert_eq!(decision.action, "pause");
        assert_eq!(decision.node.as_deref(), Some("merge"));
        let blocker = state.blocker.as_ref().expect("human gate persists a blocker");
        assert_eq!(blocker.edge, "human_gate");
        assert_eq!(blocker.node.as_deref(), Some("merge"));
        // A repeat coordinate keeps pausing — a gate is a decision, not a break.
        let again = coordinate(&mut state, false, at(1));
        assert_eq!(again.action, "pause");
        assert_eq!(again.predicate.as_deref(), Some("human_gate"));
    }

    #[test]
    fn advance_refuses_human_gate_without_authorization() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept"]
        {
            step(&mut state, node);
        }
        let error =
            advance(&mut state, "merge", SHA, false, false, None, None, None, None).unwrap_err();
        assert!(error.contains("human gate"), "error was: {error}");
        assert!(error.contains("resume"), "error was: {error}");
    }

    #[test]
    fn resume_authorizes_then_advance_consumes_one_shot() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept"]
        {
            step(&mut state, node);
        }
        mint_authorization(&mut state, "merge");
        advance(&mut state, "merge", SHA, false, false, None, None, None, None).unwrap();
        assert_eq!(state.current.as_deref(), Some("merge"));
        assert!(state.authorized.is_none(), "the authorization is one-shot");
        // The next gate is not authorized.
        let error =
            advance(&mut state, "release", SHA, false, false, None, None, None, None).unwrap_err();
        assert!(error.contains("human gate"), "error was: {error}");
    }

    #[test]
    fn stop_resume_does_not_mint_a_gate_authorization() {
        let mut state = fresh();
        begin(&mut state, "plan", "thread-7", at(0)).unwrap();
        coordinate(&mut state, false, at(STALL_SECONDS + 1));
        assert!(state.blocker.is_some());
        resume(&mut state, "human reviewed the stall", at(0)).unwrap();
        assert!(state.authorized.is_none(), "a stop resume must not authorize a gate");
    }

    #[test]
    fn a_ready_node_is_dispatched_to_a_fresh_executor() {
        let mut state = fresh();
        let decision = coordinate(&mut state, false, at(0));
        assert_eq!(decision.action, "dispatch");
        assert_eq!(decision.node.as_deref(), Some("plan"));
        assert!(decision.fresh_executor);
        assert!(decision.brief.is_some());
    }

    #[test]
    fn begin_records_the_executor_and_the_active_window() {
        let mut state = fresh();
        begin(&mut state, "plan", "thread-7", at(0)).unwrap();
        let active = state.active.as_ref().expect("active node should be recorded");
        assert_eq!(active.node, "plan");
        assert_eq!(active.executor, "thread-7");
        advance(
            &mut state,
            "plan",
            "artifact",
            false,
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(state.active.is_none());
    }

    #[test]
    fn a_different_node_cannot_advance_while_one_is_active() {
        let mut state = fresh();
        begin(&mut state, "plan", "thread-7", at(0)).unwrap();
        let error = advance(
            &mut state,
            "promote",
            "artifact",
            false,
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(error.contains("cannot advance"), "error was: {error}");
    }

    #[test]
    fn back_edge_reopens_implement_after_a_failed_verify() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            Some(failed_record()),
            None,
            None,
            None,
        )
        .unwrap();
        advance(
            &mut state,
            "implement",
            "sha-fixed",
            false,
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(state.back_edges.len(), 1);
        assert_eq!(state.current.as_deref(), Some("implement"));
        // The gate is still recorded as a failure until it is retried; the
        // back-edge re-opened the implementation, not the gate.
        let verify = state
            .done
            .iter()
            .find(|entry| entry.node == "verify")
            .expect("verify should still be recorded");
        assert!(verify.failed);
    }

    #[test]
    fn undeclared_back_edge_is_refused() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement", "verify"] {
            step(&mut state, node);
        }
        // `verify` may re-open `implement`; `compile` may not re-open `verify`
        // because only the implementation is re-openable.
        advance(
            &mut state,
            "compile",
            SHA,
            false,
            true,
            Some(failed_record()),
            None,
            None,
            None,
        )
        .unwrap();
        let error = advance(
            &mut state,
            "verify",
            "report",
            false,
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(error.contains("already done"), "error was: {error}");
    }

    #[test]
    fn a_second_back_edge_traversal_stops_the_plan() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        for traversal in 0..2 {
            let report = format!("report-{traversal}");
            advance(
                &mut state,
                "verify",
                &report,
                false,
                true,
                Some(distinct_failure(traversal)),
                None,
                None,
                None,
            )
            .unwrap();
            let fix = format!("sha-fix-{traversal}");
            advance(
                &mut state,
                "implement",
                &fix,
                false,
                false,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        }
        let decision = coordinate(&mut state, false, at(0));
        assert_eq!(decision.action, "stop");
        assert_eq!(decision.predicate.as_deref(), Some("cross_node_cycle"));
    }

    #[test]
    fn same_error_stops() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        advance(
            &mut state,
            "verify",
            "report-1",
            false,
            true,
            Some(failed_record()),
            None,
            None,
            None,
        )
        .unwrap();
        advance(
            &mut state,
            "verify",
            "report-2",
            false,
            true,
            Some(failed_record()),
            None,
            None,
            None,
        )
        .unwrap();
        let decision = coordinate(&mut state, false, at(0));
        assert_eq!(decision.action, "stop");
        assert_eq!(decision.predicate.as_deref(), Some("same_error"));
    }

    #[test]
    fn no_semantic_progress_stops() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        advance(
            &mut state,
            "verify",
            "same-report",
            false,
            true,
            Some(distinct_failure(0)),
            None,
            None,
            None,
        )
        .unwrap();
        advance(
            &mut state,
            "verify",
            "same-report",
            false,
            true,
            Some(distinct_failure(1)),
            None,
            None,
            None,
        )
        .unwrap();
        let decision = coordinate(&mut state, false, at(0));
        assert_eq!(decision.action, "stop");
        assert_eq!(decision.predicate.as_deref(), Some("no_semantic_progress"));
    }

    #[test]
    fn gate_failure_streak_stops() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        for attempt in 0..GATE_FAILURE_STREAK {
            let artifact = format!("report-{attempt}");
            advance(
                &mut state,
                "verify",
                &artifact,
                false,
                true,
                Some(distinct_failure(attempt)),
                None,
                None,
                None,
            )
            .unwrap();
        }
        let decision = coordinate(&mut state, false, at(0));
        assert_eq!(decision.action, "stop");
        assert_eq!(decision.predicate.as_deref(), Some("gate_failure_streak"));
    }

    #[test]
    fn output_decline_stops() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        let mut full = cost();
        full.output_tokens = 100;
        advance(
            &mut state,
            "verify",
            "report-1",
            false,
            true,
            Some(distinct_failure(0)),
            Some(full),
            None,
            None,
        )
        .unwrap();
        let mut thin = cost();
        thin.output_tokens = 20;
        advance(
            &mut state,
            "verify",
            "report-2",
            false,
            true,
            Some(distinct_failure(1)),
            Some(thin),
            None,
            None,
        )
        .unwrap();
        let decision = coordinate(&mut state, false, at(0));
        assert_eq!(decision.action, "stop");
        assert_eq!(decision.predicate.as_deref(), Some("output_decline"));
    }

    #[test]
    fn stall_stops() {
        let mut state = fresh();
        begin(&mut state, "plan", "thread-7", at(0)).unwrap();
        let decision = coordinate(&mut state, false, at(STALL_SECONDS + 1));
        assert_eq!(decision.action, "stop");
        assert_eq!(decision.predicate.as_deref(), Some("stall"));
        assert!(state.blocker.is_some());
        let resume_error = coordinate(&mut state, false, at(STALL_SECONDS + 2));
        assert_eq!(resume_error.action, "stop");
        resume(&mut state, "human reviewed the stall", at(0)).unwrap();
        let decision = coordinate(&mut state, false, at(STALL_SECONDS + 3));
        assert_eq!(decision.action, "dispatch");
    }

    #[test]
    fn heartbeat_keeps_a_long_node_alive() {
        let mut state = fresh();
        begin(&mut state, "plan", "thread-7", at(0)).unwrap();
        heartbeat(&mut state, at(STALL_SECONDS - 1)).unwrap();
        let decision = coordinate(&mut state, false, at(STALL_SECONDS + 1));
        assert_eq!(decision.action, "continue");
    }

    #[test]
    fn no_progress_burn_warning_is_recorded() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        let mut first = cost();
        first.input_tokens = 10_000;
        advance(
            &mut state,
            "verify",
            "same-report",
            false,
            true,
            Some(distinct_failure(0)),
            Some(first),
            None,
            None,
        )
        .unwrap();
        let mut second = cost();
        second.input_tokens = 40_000;
        advance(
            &mut state,
            "verify",
            "same-report",
            false,
            true,
            Some(distinct_failure(1)),
            Some(second),
            None,
            None,
        )
        .unwrap();
        assert!(state
            .warnings
            .iter()
            .any(|warning| warning.kind == "no_progress_burn"));
    }

    #[test]
    fn a_long_node_that_produces_a_new_artifact_does_not_warn() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            step(&mut state, node);
        }
        let mut first = cost();
        first.input_tokens = 500_000;
        advance(
            &mut state,
            "verify",
            "report-1",
            false,
            true,
            Some(distinct_failure(0)),
            Some(first),
            None,
            None,
        )
        .unwrap();
        let mut second = cost();
        second.input_tokens = 800_000;
        advance(
            &mut state,
            "verify",
            "report-2",
            false,
            true,
            Some(distinct_failure(1)),
            Some(second),
            None,
            None,
        )
        .unwrap();
        assert!(!state
            .warnings
            .iter()
            .any(|warning| warning.kind == "no_progress_burn"));
    }

    #[test]
    fn burn_anomaly_is_provisional_without_a_baseline() {
        let mut state = fresh();
        let mut hot = cost();
        hot.input_tokens = 400_000;
        advance(
            &mut state,
            "plan",
            "artifact",
            false,
            false,
            None,
            Some(hot),
            None,
            None,
        )
        .unwrap();
        let warning = state
            .warnings
            .iter()
            .find(|warning| warning.kind == "burn_anomaly")
            .expect("an anomaly should be recorded");
        assert!(warning.provisional, "no baseline means provisional");
    }

    #[test]
    fn burn_anomaly_uses_a_real_baseline_when_it_has_samples() {
        let baseline = NodeBaseline {
            samples: 5,
            median_tokens: 50_000,
            p90_tokens: 100_000,
            ..Default::default()
        };
        let mut too_cool = cost();
        too_cool.input_tokens = 40_000;
        let warnings = burn_warnings(
            "plan",
            "artifact",
            &[],
            Some(&too_cool),
            Some(&baseline),
            "2026-11-15T12:00:00+00:00",
        );
        assert!(warnings.is_empty());

        let mut hot = cost();
        hot.input_tokens = 200_000;
        let warnings = burn_warnings(
            "plan",
            "artifact",
            &[],
            Some(&hot),
            Some(&baseline),
            "2026-11-15T12:00:00+00:00",
        );
        assert_eq!(warnings.len(), 1);
        assert!(!warnings[0].provisional);
    }

    #[test]
    fn a_repeated_burn_warning_pauses() {
        let mut state = fresh();
        let warning = BurnWarning {
            kind: "no_progress_burn".to_string(),
            node: "plan".to_string(),
            at: String::new(),
            provisional: false,
            detail: "unchanged".to_string(),
        };
        state.done.push(DoneEntry {
            node: "plan".to_string(),
            artifact: "artifact".to_string(),
            at: String::new(),
            failed: false,
            attempts: vec![
                Attempt {
                    at: "1".to_string(),
                    artifact: "artifact".to_string(),
                    failed: false,
                    failure: None,
                    cost: None,
                    warnings: vec![warning.clone()],
                },
                Attempt {
                    at: "2".to_string(),
                    artifact: "artifact".to_string(),
                    failed: false,
                    failure: None,
                    cost: None,
                    warnings: vec![warning],
                },
            ],
            cost: None,
        });
        let decision = coordinate(&mut state, false, at(0));
        assert_eq!(decision.action, "pause");
        assert_eq!(decision.predicate.as_deref(), Some("repeated_burn_warning"));
    }

    #[test]
    fn resume_requires_an_artifact_and_clears_the_blocker() {
        let mut state = fresh();
        begin(&mut state, "plan", "thread-7", at(0)).unwrap();
        coordinate(&mut state, false, at(STALL_SECONDS + 1));
        assert!(resume(&mut state, "  ", at(0)).is_err());
        assert!(state.blocker.is_some());
        resume(&mut state, "human decision", at(0)).unwrap();
        assert!(state.blocker.is_none());
    }

    #[test]
    fn legacy_progress_json_still_loads() {
        let legacy = r#"{
            "plan": "OLD.md",
            "repo": "zed",
            "current": "plan",
            "done": [{
                "node": "plan",
                "artifact": "artifact",
                "at": "2026-11-15T09:00:00+00:00",
                "attempts": [{
                    "at": "2026-11-15T09:00:00+00:00",
                    "artifact": "artifact",
                    "failed": false,
                    "cost": {"input_tokens": 10, "output_tokens": 5}
                }]
            }],
            "next": ["promote"]
        }"#;
        let state: GraphState = serde_json::from_str(legacy).unwrap();
        assert_eq!(state.next, vec!["promote".to_string()]);
        assert!(state.active.is_none());
        assert!(state.warnings.is_empty());
        assert!(state.back_edges.is_empty());
        assert_eq!(spent_tokens(&state), 15);
    }

    #[test]
    fn graph_snapshot_projects_state_and_evidence() {
        let mut state = fresh();
        step(&mut state, "plan");
        step(&mut state, "promote");

        let snapshot = graph_snapshot(&state);
        assert_eq!(snapshot.plan_id, "TRIVIAL");
        assert_eq!(snapshot.name, "TRIVIAL");
        assert_eq!(snapshot.repo.as_deref(), Some("zed"));
        assert_eq!(snapshot.done_nodes, vec!["plan", "promote"]);
        assert_eq!(snapshot.spent_tokens, spent_tokens(&state));

        // Evidence carries `gate_exit_code` — a column `progress.json` does not
        // carry — proving the store is a richer co-record, not a re-ingest.
        assert_eq!(snapshot.evidence.len(), 2);
        let plan_evidence = &snapshot.evidence[0];
        assert_eq!(plan_evidence.node, "plan");
        assert_eq!(plan_evidence.artifact_sha.as_deref(), Some("artifact"));
        assert_eq!(plan_evidence.gate_exit_code, Some(0));
    }
}
