use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, AppContext as _, SharedString, Task};
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
}

/// Read or advance a plan's dev-flow progress state.
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
    /// Node to advance to (required for `advance`).
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
    /// For `init`: the plan's stated budget (D13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<CostBudget>,
}

/// A structured failure record for a failed attempt (D12, `Reflexion`-shaped).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FailureRecord {
    /// What failed.
    pub what_failed: String,
    /// Why it failed.
    pub why: String,
    /// The suggested correction to feed into the next attempt.
    pub suggestion: String,
}

/// Per-node token usage and wall-clock captured for one attempt (D13).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NodeCost {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub wall_clock_ms: u64,
}

/// A plan's stated budget (D13) — the ceiling on total tokens and wall-clock.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CostBudget {
    #[serde(default)]
    pub max_tokens: u64,
    #[serde(default)]
    pub max_wall_clock_ms: u64,
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

/// The fixed dev-flow route (D6) — the `dev-flow` #1–12 stages in order.
const ROUTE: &[&str] = &[
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

/// Attempt budget for a single node before it is declared exhausted (D12).
const RETRY_BUDGET: usize = 3;

/// The fast-path threshold (D13 / P7): a plan whose stated budget stays below
/// this is not worth traversing the graph for. Stated in total tokens. Tunable;
/// calibrated against plan 1.24's eval baseline, where the cheapest measured
/// task used ~35k input tokens.
const FAST_PATH_THRESHOLD_TOKENS: u64 = 10_000;

/// Incoming edges for each node. `merge` has exactly one: `accept`.
fn prereqs(node: &str) -> &'static [&'static str] {
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

/// The shape an edge's artifact must satisfy before the transition is permitted
/// (D11). A node's contract is declared, not inferred; a malformed artifact is
/// refused deterministically, mirroring the edge guard's skip-to-merge refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactContract {
    /// Any artifact (including empty) is accepted.
    Any,
    /// A non-empty artifact is required.
    NonEmpty,
    /// The artifact must be a git commit sha (7–40 hexadecimal characters).
    CommitSha,
}

fn artifact_contract(node: &str) -> ArtifactContract {
    match node {
        // A claim produces a workstream id; a fork produces a branch name.
        "claim" | "fork" => ArtifactContract::NonEmpty,
        // Compile/merge/release produce a build, merge, or release commit sha.
        "compile" | "merge" | "release" => ArtifactContract::CommitSha,
        _ => ArtifactContract::Any,
    }
}

fn validate_artifact(node: &str, artifact: &str) -> Result<(), String> {
    match artifact_contract(node) {
        ArtifactContract::Any => Ok(()),
        ArtifactContract::NonEmpty => {
            if artifact.is_empty() {
                Err(format!("{node} requires a non-empty artifact"))
            } else {
                Ok(())
            }
        }
        ArtifactContract::CommitSha => {
            let is_sha = (7..=40).contains(&artifact.len())
                && artifact
                    .chars()
                    .all(|character| character.is_ascii_hexdigit());
            if is_sha {
                Ok(())
            } else {
                Err(format!(
                    "{node} requires a git commit sha (7–40 hex characters), got {artifact:?}"
                ))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Attempt {
    at: String,
    artifact: String,
    failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<FailureRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost: Option<NodeCost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DoneEntry {
    node: String,
    artifact: String,
    at: String,
    #[serde(default)]
    failed: bool,
    #[serde(default)]
    attempts: Vec<Attempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cost: Option<NodeCost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Blocker {
    edge: String,
    reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GraphState {
    plan: String,
    repo: String,
    #[serde(default)]
    current: Option<String>,
    #[serde(default)]
    done: Vec<DoneEntry>,
    #[serde(default)]
    next: Vec<String>,
    #[serde(default)]
    blocker: Option<Blocker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget: Option<CostBudget>,
}

/// The permitted successors, derived from the route and `done` (D6 — never stored).
fn derive_next(state: &GraphState) -> Vec<String> {
    let done: HashSet<&str> = state.done.iter().map(|entry| entry.node.as_str()).collect();
    ROUTE
        .iter()
        .filter(|&node| !done.contains(node) && prereqs(node).iter().all(|p| done.contains(*p)))
        .map(|node| (*node).to_string())
        .collect()
}

fn transitive_prereqs(node: &str, out: &mut Vec<&'static str>) {
    for prereq in prereqs(node) {
        out.push(prereq);
        transitive_prereqs(prereq, out);
    }
}

/// The latest failure record on a node, for surfacing when the retry budget is
/// exhausted (D12).
fn summarize_failure(entry: &DoneEntry) -> String {
    entry
        .attempts
        .iter()
        .rev()
        .find_map(|attempt| attempt.failure.as_ref())
        .map(|failure| {
            format!(
                "what_failed={:?}, why={:?}, suggestion={:?}",
                failure.what_failed, failure.why, failure.suggestion
            )
        })
        .unwrap_or_else(|| "no failure record".to_string())
}

/// The edge guard: a transition is refused unless every transitive prerequisite
/// is done, the artifact satisfies its contract, and — on `merge` of a protected
/// change — no verification gate failed.
fn advance(
    state: &mut GraphState,
    node: &str,
    artifact: &str,
    protected: bool,
    failed: bool,
    failure: Option<FailureRecord>,
    cost: Option<NodeCost>,
) -> Result<(), String> {
    if !ROUTE.contains(&node) {
        return Err(format!("unknown node: {node}"));
    }
    if let Some(blocker) = &state.blocker {
        return Err(format!("blocked: {}", blocker.reason));
    }
    // Layer B (D11): the artifact must satisfy the node's declared contract.
    validate_artifact(node, artifact)?;
    // Layer C (D12): a failed attempt must carry a structured failure record.
    if failed && failure.is_none() {
        return Err(format!(
            "a failed {node} attempt must carry a structured failure record (what failed, why, suggestion)"
        ));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let attempt = Attempt {
        at: now.clone(),
        artifact: artifact.to_string(),
        failed,
        failure,
        cost: cost.clone(),
    };

    // Retry path: the node is already recorded and its latest attempt failed.
    if let Some(entry) = state.done.iter_mut().find(|entry| entry.node == node) {
        if !entry.failed {
            return Err(format!("{node} already done"));
        }
        if entry.attempts.len() >= RETRY_BUDGET {
            return Err(format!(
                "retry budget for {node} exhausted after {} attempt(s): {}",
                entry.attempts.len(),
                summarize_failure(entry)
            ));
        }
        entry.attempts.push(attempt);
        entry.artifact = artifact.to_string();
        entry.at = now;
        entry.failed = failed;
        entry.cost = cost;
        return Ok(());
    }

    let done: HashSet<&str> = state.done.iter().map(|entry| entry.node.as_str()).collect();
    let mut missing = Vec::new();
    transitive_prereqs(node, &mut missing);
    let mut missing: Vec<&str> = missing.into_iter().filter(|p| !done.contains(p)).collect();
    missing.sort_unstable();
    missing.dedup();
    if !missing.is_empty() {
        return Err(format!(
            "edge guard refused {node} — missing gate(s): {}",
            missing.join(", ")
        ));
    }

    // D17 conditional blocking: a protected change whose verification failed
    // must not merge. Unprotected changes stay advisory.
    if node == "merge" && protected {
        let failed_gate = state.done.iter().any(|entry| {
            matches!(entry.node.as_str(), "verify" | "compile" | "accept") && entry.failed
        });
        if failed_gate {
            return Err("protected change: a verification gate failed — merge blocked".to_string());
        }
    }

    state.done.push(DoneEntry {
        node: node.to_string(),
        artifact: artifact.to_string(),
        at: now,
        failed,
        attempts: vec![attempt],
        cost,
    });
    state.current = Some(node.to_string());
    state.next = derive_next(state);
    Ok(())
}

pub struct GraphTool;

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
        cx.background_spawn(async move {
            let input = input.recv().await.map_err(|error| GraphToolOutput::Error {
                error: format!("Failed to receive graph tool input: {error}"),
            })?;
            let path = PathBuf::from(&input.path);
            run_operation(input, path)
        })
    }
}

fn load_state(path: &Path) -> Result<GraphState, GraphToolOutput> {
    let content = std::fs::read_to_string(path).map_err(|error| GraphToolOutput::Error {
        error: format!("Failed to read {}: {error}", path.display()),
    })?;
    serde_json::from_str(&content).map_err(|error| GraphToolOutput::Error {
        error: format!("Failed to parse {}: {error}", path.display()),
    })
}

fn save_state(path: &Path, state: &GraphState) -> Result<(), GraphToolOutput> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| GraphToolOutput::Error {
            error: format!("Failed to create {}: {error}", parent.display()),
        })?;
    }
    let content = serde_json::to_string_pretty(state).map_err(|error| GraphToolOutput::Error {
        error: format!("Failed to serialize dev-flow state: {error}"),
    })?;
    std::fs::write(path, content).map_err(|error| GraphToolOutput::Error {
        error: format!("Failed to write {}: {error}", path.display()),
    })
}

fn state_json(state: &GraphState) -> String {
    serde_json::to_string(state).unwrap_or_else(|_| "{}".to_string())
}

fn run_operation(input: GraphToolInput, path: PathBuf) -> Result<GraphToolOutput, GraphToolOutput> {
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
                current: None,
                done: Vec::new(),
                next: vec!["plan".to_string()],
                blocker: None,
                budget: input.budget,
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
            )
            .map_err(|error| GraphToolOutput::Error { error })?;
            save_state(&path, &state)?;
            Ok(GraphToolOutput::Success {
                message: format!("Advanced to {node}: {}", state_json(&state)),
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

    fn cost() -> NodeCost {
        NodeCost {
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 30,
            wall_clock_ms: 1500,
        }
    }

    fn fresh() -> GraphState {
        GraphState {
            plan: "TRIVIAL".to_string(),
            repo: "zed".to_string(),
            current: None,
            done: Vec::new(),
            next: vec!["plan".to_string()],
            blocker: None,
            budget: None,
        }
    }

    #[test]
    fn happy_path_walks_the_route() {
        let mut state = fresh();
        for node in ROUTE {
            advance(&mut state, node, SHA, false, false, None, None).unwrap();
        }
        assert_eq!(state.current.as_deref(), Some("complete"));
        assert!(state.next.is_empty());
    }

    #[test]
    fn edge_guard_refuses_skip_to_merge() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            advance(&mut state, node, "artifact", false, false, None, None).unwrap();
        }
        let error = advance(&mut state, "merge", SHA, false, false, None, None).unwrap_err();
        assert!(error.contains("verify"), "error was: {error}");
        assert!(error.contains("compile"), "error was: {error}");
        assert!(error.contains("accept"), "error was: {error}");
    }

    #[test]
    fn protected_change_blocks_merge_on_failed_verify() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            advance(&mut state, node, "artifact", false, false, None, None).unwrap();
        }
        advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            Some(failed_record()),
            None,
        )
        .unwrap();
        advance(&mut state, "compile", SHA, false, false, None, None).unwrap();
        advance(&mut state, "accept", "report", false, false, None, None).unwrap();
        let error = advance(&mut state, "merge", SHA, true, false, None, None).unwrap_err();
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
            advance(&mut state, node, SHA, false, false, None, None).unwrap();
        }
        advance(&mut state, "merge", SHA, true, false, None, None).unwrap();
        assert_eq!(state.current.as_deref(), Some("merge"));
    }

    #[test]
    fn unprotected_change_allows_merge_even_if_verification_failed() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            advance(&mut state, node, "artifact", false, false, None, None).unwrap();
        }
        advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            Some(failed_record()),
            None,
        )
        .unwrap();
        advance(&mut state, "compile", SHA, false, false, None, None).unwrap();
        advance(&mut state, "accept", "report", false, false, None, None).unwrap();
        advance(&mut state, "merge", SHA, false, false, None, None).unwrap();
        assert_eq!(state.current.as_deref(), Some("merge"));
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
            },
            path.clone(),
        )
        .unwrap();
        assert!(matches!(output, GraphToolOutput::Success { .. }));

        let loaded = load_state(&path).unwrap();
        assert_eq!(loaded.plan, "PLAN_A");
        assert_eq!(loaded.next, vec!["plan".to_string()]);
        let budget = loaded.budget.expect("budget should be stored");
        assert_eq!(budget.max_tokens, 100_000);
        assert_eq!(budget.max_wall_clock_ms, 600_000);
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
            advance(&mut state, node, SHA, false, false, None, None).unwrap();
        }
        let error =
            advance(&mut state, "merge", "not-a-sha", false, false, None, None).unwrap_err();
        assert!(error.contains("git commit sha"), "error was: {error}");
    }

    #[test]
    fn failed_attempt_requires_failure_record() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            advance(&mut state, node, "artifact", false, false, None, None).unwrap();
        }
        let error = advance(&mut state, "verify", "report", false, true, None, None).unwrap_err();
        assert!(error.contains("failure record"), "error was: {error}");
    }

    #[test]
    fn retry_is_bounded_and_exhausts_with_failure_record() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            advance(&mut state, node, "artifact", false, false, None, None).unwrap();
        }
        // First attempt fails.
        advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            Some(failed_record()),
            None,
        )
        .unwrap();
        // Retries up to the budget.
        for attempt in 0..(RETRY_BUDGET - 1) {
            advance(
                &mut state,
                "verify",
                "report",
                false,
                true,
                Some(failed_record()),
                None,
            )
            .unwrap_or_else(|error| panic!("attempt {attempt} should be allowed: {error}"));
        }
        // The next retry exceeds the budget and surfaces the failure record.
        let error = advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            Some(failed_record()),
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
            advance(&mut state, node, "artifact", false, false, None, None).unwrap();
        }
        advance(
            &mut state,
            "verify",
            "report",
            false,
            true,
            Some(failed_record()),
            None,
        )
        .unwrap();
        advance(&mut state, "verify", "report", false, false, None, None).unwrap();
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
            advance(&mut state, node, "artifact", false, false, None, None).unwrap();
        }
        advance(
            &mut state,
            "verify",
            "report",
            false,
            false,
            None,
            Some(cost()),
        )
        .unwrap();
        let entry = state
            .done
            .iter()
            .find(|entry| entry.node == "verify")
            .unwrap();
        let recorded = entry.cost.as_ref().expect("cost should be recorded");
        assert_eq!(recorded.wall_clock_ms, 1500);
        assert_eq!(
            recorded.input_tokens
                + recorded.output_tokens
                + recorded.cache_creation_input_tokens
                + recorded.cache_read_input_tokens,
            155
        );
    }
}
