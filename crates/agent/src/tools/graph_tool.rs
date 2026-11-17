use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{AgentTool, Thread, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use chrono::{DateTime, Utc};
use gpui::{App, AppContext as _, SharedString, Task, WeakEntity};
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
    /// The identity that ran this attempt, taken from the thread, never
    /// self-reported by the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    /// Time the executor spent working. The clock excludes human gates, remote
    /// build queues and tool waits, so a slow CI cannot look like a slow agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_ms: Option<u64>,
}

impl NodeCost {
    fn tokens(&self) -> u64 {
        // Cache tokens are recorded but never summed: they double-count input.
        self.input_tokens + self.output_tokens
    }
}

/// A plan's stated budget (D13) — inert data; nothing enforces it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CostBudget {
    #[serde(default)]
    pub max_tokens: u64,
    #[serde(default)]
    pub max_wall_clock_ms: u64,
}

/// A node type's burn baseline, owned by the harness and supplied per call.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NodeBaseline {
    /// Number of samples behind the summary. Below three the warning is
    /// `provisional` and falls back to the 1.24 eval band.
    #[serde(default)]
    pub samples: u32,
    #[serde(default)]
    pub median_tokens: u64,
    #[serde(default)]
    pub p90_tokens: u64,
    #[serde(default)]
    pub median_active_ms: u64,
    #[serde(default)]
    pub p90_active_ms: u64,
}

/// A non-blocking burn warning (Layer D). It is recorded, never enforced; a
/// repeated warning is the pause condition (Layer C row 10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurnWarning {
    /// `no_progress_burn` or `burn_anomaly`.
    pub kind: String,
    pub node: String,
    pub at: String,
    #[serde(default)]
    pub provisional: bool,
    pub detail: String,
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

/// The nodes that verify the work, and therefore the ones a back-edge may leave.
const GATE_NODES: &[&str] = &["verify", "compile", "accept"];

/// Declared back-edges. A failed gate returns to `implement`; every other
/// backward transition is undeclared and refused.
const BACK_EDGES: &[(&str, &str)] = &[
    ("verify", "implement"),
    ("compile", "implement"),
    ("accept", "implement"),
];

/// Nodes that need a human decision. Automatic traversal halts before them.
///
/// `HUMAN_GATES` is the route-node subset of the fork's full human-gate list.
/// The authoritative list — launch, laptop takeover, merge/rebuild of shared
/// slots, release, plan completion — lives in plan 1.25
/// (`FORK_OPERATING_KNOWLEDGE_FINAL_LAYER.md` in the zed-debugger-demo plans
/// tree, "Testing & verification method" item 8); this table cites that plan
/// rather than restating the list. The launch and laptop-takeover gates are
/// operational (owned by `dev-flow` / `remote-compiler`), not route nodes.
const HUMAN_GATES: &[&str] = &["merge", "release", "complete"];

/// Attempt budget for a single node before it is declared exhausted (D12).
const RETRY_BUDGET: usize = 3;

/// The same gate failing this many times in a plan stops the loop (Layer C row 4).
const GATE_FAILURE_STREAK: usize = 3;

/// Output below this percentage of the previous attempt stops the loop (row 5).
const OUTPUT_DECLINE_PERCENT: u64 = 30;

/// No heartbeat for this long while a node is active stops the loop (row 7).
const STALL_SECONDS: i64 = 15 * 60;

/// The number of samples a baseline needs before a warning stops being
/// provisional.
const MIN_BASELINE_SAMPLES: u32 = 3;

/// 1.24 cold baselines (57 task runs): p90 343k tokens. Used only while a node
/// type has fewer than three samples.
const PROVISIONAL_P90_TOKENS: u64 = 343_000;

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<BurnWarning>,
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
    #[serde(default)]
    spent_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_failure: Option<FailureRecord>,
}

/// The node currently being executed. Its active window is the interval from
/// `started_at` to the advance that completes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveNode {
    node: String,
    executor: String,
    started_at: String,
    last_activity_at: String,
}

/// One traversal of a declared back-edge (Layer C row 6 counts them).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BackEdgeTraversal {
    from: String,
    to: String,
    at: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<ActiveNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    back_edges: Vec<BackEdgeTraversal>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<BurnWarning>,
}

/// The decision `coordinate` returns. `dispatch` and `continue` advance the
/// loop; `pause` waits for a human; `stop` requires an explicit `resume`.
#[derive(Debug, Clone, Serialize)]
struct Decision {
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<String>,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    brief: Option<String>,
    fresh_executor: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    predicate: Option<String>,
    spent_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_failure: Option<FailureRecord>,
}

/// The permitted successors, derived from the route and `done` (D6 — never stored).
///
/// Presence-based by design: D17 makes a failed gate advisory for an unprotected
/// change (merge may still proceed), so the edge guard cannot hard-block on it.
/// The *coordinator* prefers a retry of a failed node before any successor —
/// see `retry_candidate`.
fn derive_next(state: &GraphState) -> Vec<String> {
    let done: HashSet<&str> = state.done.iter().map(|entry| entry.node.as_str()).collect();
    ROUTE
        .iter()
        .filter(|&node| !done.contains(node) && prereqs(node).iter().all(|p| done.contains(*p)))
        .map(|node| (*node).to_string())
        .collect()
}

/// A node recorded but not passed, which must be retried before its successors
/// are dispatched. Route order makes the choice deterministic.
fn retry_candidate(state: &GraphState) -> Option<String> {
    ROUTE
        .iter()
        .find(|node| {
            state
                .done
                .iter()
                .any(|entry| entry.node == **node && entry.failed)
        })
        .map(|node| (*node).to_string())
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

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn spent_tokens(state: &GraphState) -> u64 {
    state
        .done
        .iter()
        .flat_map(|entry| entry.attempts.iter())
        .filter_map(|attempt| attempt.cost.as_ref())
        .map(NodeCost::tokens)
        .sum()
}

fn latest_failure(state: &GraphState) -> Option<FailureRecord> {
    state
        .done
        .iter()
        .rev()
        .flat_map(|entry| entry.attempts.iter().rev())
        .find_map(|attempt| attempt.failure.clone())
}

/// Burn warnings for one attempt, conditioned on progress rather than raw spend.
fn burn_warnings(
    node: &str,
    artifact: &str,
    previous: &[Attempt],
    cost: Option<&NodeCost>,
    baseline: Option<&NodeBaseline>,
    at: &str,
) -> Vec<BurnWarning> {
    let mut warnings = Vec::new();
    let tokens = cost.map(NodeCost::tokens).unwrap_or(0);

    if tokens > 0 {
        let provisional = baseline
            .map(|baseline| baseline.samples < MIN_BASELINE_SAMPLES)
            .unwrap_or(true);
        let p90 = if provisional {
            PROVISIONAL_P90_TOKENS
        } else {
            baseline.map(|baseline| baseline.p90_tokens).unwrap_or(0)
        };
        if p90 > 0 && tokens > p90 {
            warnings.push(BurnWarning {
                kind: "burn_anomaly".to_string(),
                node: node.to_string(),
                at: at.to_string(),
                provisional,
                detail: format!(
                    "{tokens} tokens is above the {} of {p90}",
                    if provisional {
                        "provisional 1.24 band"
                    } else {
                        "node p90"
                    }
                ),
            });
        }
    }

    if let Some(previous) = previous.last() {
        let previous_tokens = previous.cost.as_ref().map(NodeCost::tokens).unwrap_or(0);
        if previous.artifact == artifact && tokens >= previous_tokens {
            warnings.push(BurnWarning {
                kind: "no_progress_burn".to_string(),
                node: node.to_string(),
                at: at.to_string(),
                provisional: false,
                detail: format!(
                    "spend grew {previous_tokens} -> {tokens} tokens while the artifact stayed {artifact:?}"
                ),
            });
        }
    }

    warnings
}

fn repeated_burn_warning(state: &GraphState) -> Option<BurnWarning> {
    for entry in state.done.iter().rev() {
        if entry.attempts.len() < 2 {
            continue;
        }
        let latest = entry.attempts.last()?;
        let previous = &entry.attempts[entry.attempts.len() - 2];
        if let Some(warning) = latest
            .warnings
            .iter()
            .find(|warning| previous.warnings.iter().any(|other| other.kind == warning.kind))
        {
            return Some(warning.clone());
        }
    }
    None
}

fn stop_decision(state: &GraphState, predicate: &str, reason: String) -> Decision {
    Decision {
        action: "stop".to_string(),
        node: None,
        reason,
        brief: None,
        fresh_executor: false,
        predicate: Some(predicate.to_string()),
        spent_tokens: spent_tokens(state),
        last_failure: latest_failure(state),
    }
}

/// Layer C rows 1–7. The specific failure-shape predicates are reported before
/// the generic attempt budget, because they say *why* the loop is stuck.
fn evaluate_stop(state: &GraphState, now: DateTime<Utc>) -> Option<Decision> {
    // Row 2 — the same error twice.
    for entry in &state.done {
        if !entry.failed {
            continue;
        }
        if entry.attempts.len() < 2 {
            continue;
        }
        let latest = &entry.attempts[entry.attempts.len() - 1];
        let previous = &entry.attempts[entry.attempts.len() - 2];
        if let (Some(latest_failure), Some(previous_failure)) =
            (&latest.failure, &previous.failure)
        {
            if latest_failure.what_failed == previous_failure.what_failed
                && latest_failure.why == previous_failure.why
            {
                return Some(stop_decision(
                    state,
                    "same_error",
                    format!(
                        "{}: the same failure repeated — {:?}",
                        entry.node, latest_failure.what_failed
                    ),
                ));
            }
        }
    }

    // Row 3 — no semantic progress: the artifact did not change.
    for entry in &state.done {
        if !entry.failed || entry.attempts.len() < 2 {
            continue;
        }
        let latest = &entry.attempts[entry.attempts.len() - 1];
        let previous = &entry.attempts[entry.attempts.len() - 2];
        if latest.artifact == previous.artifact {
            return Some(stop_decision(
                state,
                "no_semantic_progress",
                format!(
                    "{}: artifact {:?} unchanged between attempts",
                    entry.node, latest.artifact
                ),
            ));
        }
    }

    // Row 4 — the same gate failed three times in the plan.
    for gate in GATE_NODES {
        let Some(entry) = state.done.iter().find(|entry| entry.node == *gate) else {
            continue;
        };
        let failures = entry.attempts.iter().filter(|attempt| attempt.failed).count();
        if failures >= GATE_FAILURE_STREAK {
            return Some(stop_decision(
                state,
                "gate_failure_streak",
                format!("{gate}: {failures} failed attempts in this plan"),
            ));
        }
    }

    // Row 1 — attempt budget exhausted on a node that is still failing.
    for entry in &state.done {
        if entry.failed && entry.attempts.len() >= RETRY_BUDGET {
            return Some(stop_decision(
                state,
                "attempt_budget",
                format!(
                    "{}: attempt budget exhausted after {} attempts",
                    entry.node,
                    entry.attempts.len()
                ),
            ));
        }
    }

    // Row 5 — output decline.
    for entry in &state.done {
        if !entry.failed || entry.attempts.len() < 2 {
            continue;
        }
        let latest = &entry.attempts[entry.attempts.len() - 1];
        let previous = &entry.attempts[entry.attempts.len() - 2];
        let latest_output = latest.cost.as_ref().map(|cost| cost.output_tokens).unwrap_or(0);
        let previous_output = previous
            .cost
            .as_ref()
            .map(|cost| cost.output_tokens)
            .unwrap_or(0);
        if previous_output > 0 && latest_output * 100 < previous_output * OUTPUT_DECLINE_PERCENT {
            return Some(stop_decision(
                state,
                "output_decline",
                format!(
                    "{}: output fell to {latest_output} tokens of {previous_output}",
                    entry.node
                ),
            ));
        }
    }

    // Row 6 — any declared back-edge traversed twice.
    for edge in BACK_EDGES {
        let count = state
            .back_edges
            .iter()
            .filter(|traversal| traversal.from == edge.0 && traversal.to == edge.1)
            .count();
        if count >= 2 {
            return Some(stop_decision(
                state,
                "cross_node_cycle",
                format!("{}->{} traversed {count} times", edge.0, edge.1),
            ));
        }
    }

    // Row 7 — stall: active, but no heartbeat inside the window.
    if let Some(active) = &state.active {
        if let Some(last) = parse_time(&active.last_activity_at) {
            let idle = (now - last).num_seconds();
            if idle > STALL_SECONDS {
                return Some(stop_decision(
                    state,
                    "stall",
                    format!("{}: no activity for {} min", active.node, idle / 60),
                ));
            }
        }
    }

    None
}

fn brief_for(node: &str, plan: &str, retrying: bool) -> String {
    let retry_note = if retrying && GATE_NODES.contains(&node) {
        " This gate failed on a previous attempt: if the failure is in the implementation, \
         advance `implement` instead to re-open it (a declared back-edge)."
    } else {
        ""
    };
    format!(
        "Dispatch {node} for {plan} to a fresh executor. It must act on external evidence, \
         return the node's declared artifact, and record its cost from usage deltas — never a \
         self-report.{retry_note}"
    )
}

fn pause_decision(
    state: &GraphState,
    node: Option<String>,
    predicate: Option<&str>,
    reason: String,
) -> Decision {
    Decision {
        action: "pause".to_string(),
        node,
        reason,
        brief: None,
        fresh_executor: false,
        predicate: predicate.map(|predicate| predicate.to_string()),
        spent_tokens: spent_tokens(state),
        last_failure: latest_failure(state),
    }
}

/// The coordinator's deterministic readiness and break decision (Layer F).
///
/// A stop is persisted as a resumable blocker; only `resume` clears it.
fn coordinate(state: &mut GraphState, ask: bool, now: DateTime<Utc>) -> Decision {
    if let Some(blocker) = &state.blocker {
        return Decision {
            action: "stop".to_string(),
            node: None,
            reason: format!("blocked: {}", blocker.reason),
            brief: None,
            fresh_executor: false,
            predicate: Some("blocker".to_string()),
            spent_tokens: spent_tokens(state),
            last_failure: blocker.last_failure.clone(),
        };
    }

    if let Some(decision) = evaluate_stop(state, now) {
        state.blocker = Some(Blocker {
            edge: decision.predicate.clone().unwrap_or_default(),
            reason: decision.reason.clone(),
            spent_tokens: decision.spent_tokens,
            last_failure: decision.last_failure.clone(),
        });
        state.next.clear();
        return decision;
    }

    if let Some(active) = &state.active {
        return Decision {
            action: "continue".to_string(),
            node: Some(active.node.clone()),
            reason: format!("{} is active ({})", active.node, active.executor),
            brief: None,
            fresh_executor: false,
            predicate: None,
            spent_tokens: spent_tokens(state),
            last_failure: None,
        };
    }

    if let Some(warning) = repeated_burn_warning(state) {
        return pause_decision(
            state,
            Some(warning.node.clone()),
            Some("repeated_burn_warning"),
            format!(
                "the same burn warning ({}) repeated on two consecutive attempts — a human \
                 decides whether to continue",
                warning.kind
            ),
        );
    }

    if ask {
        return pause_decision(
            state,
            None,
            Some("explicit_ask"),
            "the executor asked for a decision".to_string(),
        );
    }

    // A recorded-but-failed node is retried before any successor is dispatched.
    if let Some(node) = retry_candidate(state) {
        return Decision {
            action: "dispatch".to_string(),
            node: Some(node.clone()),
            reason: format!("{node} failed on a previous attempt — retry it"),
            brief: Some(brief_for(&node, &state.plan, true)),
            fresh_executor: true,
            predicate: None,
            spent_tokens: spent_tokens(state),
            last_failure: latest_failure(state),
        };
    }

    let ready = derive_next(state);
    let Some(node) = ready.first() else {
        return pause_decision(
            state,
            None,
            None,
            "no ready node — the route is exhausted or the state is inconsistent".to_string(),
        );
    };

    if HUMAN_GATES.contains(&node.as_str()) {
        return pause_decision(
            state,
            Some(node.clone()),
            Some("human_gate"),
            format!("{node} is a human gate — automatic traversal halts"),
        );
    }

    Decision {
        action: "dispatch".to_string(),
        node: Some(node.clone()),
        reason: format!("{node} is ready"),
        brief: Some(brief_for(
            node,
            &state.plan,
            state
                .done
                .iter()
                .any(|entry| entry.node == *node && entry.failed),
        )),
        fresh_executor: true,
        predicate: None,
        spent_tokens: spent_tokens(state),
        last_failure: None,
    }
}

/// Start a node, recording the executor identity and the active window.
fn begin(state: &mut GraphState, node: &str, executor: &str, now: DateTime<Utc>) -> Result<(), String> {
    if !ROUTE.contains(&node) {
        return Err(format!("unknown node: {node}"));
    }
    if let Some(blocker) = &state.blocker {
        return Err(format!("blocked: {}", blocker.reason));
    }
    if let Some(active) = &state.active {
        return Err(format!(
            "{} is already active ({}) — finish or heartbeat it before beginning {node}",
            active.node, active.executor
        ));
    }
    if HUMAN_GATES.contains(&node) {
        return Err(format!(
            "{node} is a human gate and is never auto-dispatched; coordinate pauses before it"
        ));
    }
    if !derive_next(state).iter().any(|ready| ready == node) {
        if retry_candidate(state).as_deref() != Some(node) {
            return Err(format!("{node} is not ready"));
        }
    }
    let timestamp = now.to_rfc3339();
    state.active = Some(ActiveNode {
        node: node.to_string(),
        executor: executor.to_string(),
        started_at: timestamp.clone(),
        last_activity_at: timestamp,
    });
    state.next = derive_next(state);
    Ok(())
}

fn heartbeat(state: &mut GraphState, now: DateTime<Utc>) -> Result<(), String> {
    let Some(active) = &mut state.active else {
        return Err("no node is active".to_string());
    };
    active.last_activity_at = now.to_rfc3339();
    Ok(())
}

fn resume(state: &mut GraphState, artifact: &str) -> Result<String, String> {
    if artifact.trim().is_empty() {
        return Err(
            "resuming a stopped plan needs a non-empty artifact naming the human decision"
                .to_string(),
        );
    }
    let Some(blocker) = state.blocker.take() else {
        return Err("the plan is not blocked".to_string());
    };
    // A stalled node is presumed dead: drop its window so the next dispatch goes
    // to a fresh executor rather than pushing the same bloated context.
    let cleared = state.active.take();
    state.next = derive_next(state);
    let reset = match cleared {
        Some(active) => format!(
            "cleared the {} window held by {}; ",
            active.node, active.executor
        ),
        None => String::new(),
    };
    Ok(format!(
        "resumed after {:?} — spent {} tokens; {reset}next: {}",
        blocker.edge,
        blocker.spent_tokens,
        state.next.join(", ")
    ))
}

/// The edge guard: a transition is refused unless every transitive prerequisite
/// is done, the artifact satisfies its contract, and — on `merge` of a protected
/// change — no verification gate failed.
#[allow(clippy::too_many_arguments)]
fn advance(
    state: &mut GraphState,
    node: &str,
    artifact: &str,
    protected: bool,
    failed: bool,
    failure: Option<FailureRecord>,
    cost: Option<NodeCost>,
    executor: Option<&str>,
    baseline: Option<&NodeBaseline>,
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
    if let Some(active) = &state.active {
        if active.node != node {
            return Err(format!(
                "{node} cannot advance while {} is active ({})",
                active.node, active.executor
            ));
        }
    }

    let now_at = Utc::now();
    let now = now_at.to_rfc3339();

    let mut cost = cost;
    if let Some(active) = &state.active {
        let cost = cost.get_or_insert_with(NodeCost::default);
        if cost.active_ms.is_none() {
            let elapsed = parse_time(&active.started_at)
                .map(|started| (now_at - started).num_milliseconds().max(0) as u64)
                .unwrap_or(0);
            cost.active_ms = Some(elapsed);
        }
        if cost.executor.is_none() {
            cost.executor = Some(active.executor.clone());
        }
    }
    if let Some(cost) = &mut cost {
        if cost.executor.is_none() {
            cost.executor = executor.map(|executor| executor.to_string());
        }
    }

    let previous_attempts: Vec<Attempt> = state
        .done
        .iter()
        .find(|entry| entry.node == node)
        .map(|entry| entry.attempts.clone())
        .unwrap_or_default();
    let warnings = burn_warnings(
        node,
        artifact,
        &previous_attempts,
        cost.as_ref(),
        baseline,
        &now,
    );

    let attempt = Attempt {
        at: now.clone(),
        artifact: artifact.to_string(),
        failed,
        failure,
        cost: cost.clone(),
        warnings: warnings.clone(),
    };

    // Retry path: the node is already recorded and its latest attempt failed.
    let existing = state.done.iter().position(|entry| entry.node == node);
    if let Some(index) = existing {
        if state.done[index].failed {
            if state.done[index].attempts.len() >= RETRY_BUDGET {
                return Err(format!(
                    "retry budget for {node} exhausted after {} attempt(s): {}",
                    state.done[index].attempts.len(),
                    summarize_failure(&state.done[index])
                ));
            }
            let entry = &mut state.done[index];
            entry.attempts.push(attempt);
            entry.artifact = artifact.to_string();
            entry.at = now;
            entry.failed = failed;
            entry.cost = cost;
        } else {
            // Back-edge path: a declared edge may re-open a completed node after
            // a failed gate. Every other backward transition stays refused.
            let from = GATE_NODES.iter().copied().find(|gate| {
                BACK_EDGES.contains(&(*gate, node))
                    && state
                        .done
                        .iter()
                        .any(|entry| entry.node == *gate && entry.failed)
            });
            let Some(from) = from else {
                return Err(format!("{node} already done"));
            };
            let traversals = state
                .back_edges
                .iter()
                .filter(|traversal| traversal.from == from && traversal.to == node)
                .count();
            if traversals >= 2 {
                return Err(format!(
                    "cross-node cycle: {from}->{node} already traversed {traversals} times"
                ));
            }
            state.back_edges.push(BackEdgeTraversal {
                from: from.to_string(),
                to: node.to_string(),
                at: now.clone(),
            });
            state.done.retain(|entry| entry.node != node);
            state.done.push(DoneEntry {
                node: node.to_string(),
                artifact: artifact.to_string(),
                at: now,
                failed,
                attempts: vec![attempt],
                cost,
            });
        }
    } else {
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
                return Err(
                    "protected change: a verification gate failed — merge blocked".to_string()
                );
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
    }

    state.active = None;
    state.warnings.extend(warnings);
    state.current = Some(node.to_string());
    state.next = derive_next(state);
    Ok(())
}

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
            let path = PathBuf::from(&input.path);
            let path_string = path.to_string_lossy().into_owned();
            let output = cx
                .background_spawn(async move { run_operation(input, path, Some(executor.as_str())) })
                .await;
            if output.is_ok() {
                if let Err(error) = thread.update(cx, |thread, cx| {
                    thread.set_plan_path(Some(path_string), cx);
                }) {
                    log::error!("[GRAPH] failed to record plan path on thread: {error:#}");
                }
            }
            output
        })
    }
}

/// The forced-consultation banner for the plan at `path`. `None` when the
/// thread is not executing a plan, the state is unreadable, or the plan has no
/// node left to run — a banner must never fail a request, and a completed plan
/// must not tell the agent to assume an implementation role.
pub fn plan_state_banner(path: Option<&str>) -> Option<String> {
    let state = load_state(Path::new(path?)).ok()?;
    let done: Vec<String> = state.done.iter().map(|entry| entry.node.clone()).collect();
    let next = derive_next(&state);
    let next = next.first()?;
    Some(crate::plan_state_framing(next, &done))
}

/// The next ready node for a plan's `progress.json`, or `None` when the state is
/// unreadable. Used by the Layer 2 tool-permission gate (plan 1.31).
pub fn next_plan_node(plan_path: &str) -> Option<String> {
    let state = load_state(Path::new(plan_path)).ok()?;
    derive_next(&state).into_iter().next()
}

/// Tools that mutate source or working-tree state. The plan gate blocks these
/// while a gate node is ready and unadvanced. `git` is matched by operation
/// instead, because its read-only operations must stay available.
const PLAN_GATED_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "create_directory",
    "delete_path",
    "copy_path",
    "move_path",
    "rename_symbol",
    "apply_code_action",
];

const PLAN_GATED_GIT_OPERATIONS: &[&str] = &["commit", "merge", "push"];

/// Whether this tool call is a mutation the plan gate covers.
pub fn gate_covers_tool(tool_name: &str, input_values: &[String]) -> bool {
    if PLAN_GATED_TOOLS.contains(&tool_name) {
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
                current: None,
                done: Vec::new(),
                next: vec!["plan".to_string()],
                blocker: None,
                budget: input.budget,
                active: None,
                back_edges: Vec::new(),
                warnings: Vec::new(),
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
            let message =
                resume(&mut state, &artifact).map_err(|error| GraphToolOutput::Error { error })?;
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
            current: None,
            done: Vec::new(),
            next: vec!["plan".to_string()],
            blocker: None,
            budget: None,
            active: None,
            back_edges: Vec::new(),
            warnings: Vec::new(),
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

    #[test]
    fn happy_path_walks_the_route() {
        let mut state = fresh();
        for node in ROUTE {
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

        assert!(gate_covers_tool("write_file", &[]));
        assert!(gate_covers_tool("edit_file", &[]));
        assert!(gate_covers_tool("delete_path", &[]));
        assert!(gate_covers_tool("git", &input("operation:commit protected:false")));
        assert!(gate_covers_tool("git", &input("operation:merge ref:feature")));
        assert!(gate_covers_tool("git", &input("operation:push")));

        assert!(!gate_covers_tool("git", &input("operation:status")));
        assert!(!gate_covers_tool("git", &input("operation:diff")));
        assert!(!gate_covers_tool("read_file", &[]));
        assert!(!gate_covers_tool("terminal", &[]));
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
        resume(&mut state, "human reviewed the stall").unwrap();
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
        assert!(resume(&mut state, "  ").is_err());
        assert!(state.blocker.is_some());
        resume(&mut state, "human decision").unwrap();
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
}
