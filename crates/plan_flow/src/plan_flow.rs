//! The dev-flow coordinator, shared by the `graph` agent tool and the plan UI.
//!
//! The dev flow (plan → promote → claim → fork → implement → verify → compile →
//! accept → merge → release → complete) is a graph of nodes and edges. Each plan
//! tracks its position in a `progress.json` next to the plan file. `advance` is
//! guarded: a transition is refused unless every prerequisite node is done, so a
//! "skip testing and merge" request cannot pass without `verify`/`compile`/`accept`.
//!
//! Edges carry semantics beyond the bare route (plan 1.23c, Layers B–D):
//! - **Artifact contract (D11).** Each node declares the shape its artifact must
//!   satisfy; a transition is refused on a malformed artifact, scripted rather
//!   than model judgment.
//! - **Bounded retry (D12).** A failed node may be re-attempted up to a fixed
//!   budget, each retry carrying a structured failure record fed into the next
//!   attempt; exhausting the budget blocks further retries.
//! - **Cost and latency (D13).** Each attempt records its tokens and wall-clock;
//!   a plan states a budget at `init`; below the fast-path threshold the graph
//!   is not worth traversing (see `FAST_PATH_THRESHOLD_TOKENS`).
//!
//! The coordinator (plan 1.23d, Layers C/F) adds:
//! - **Declared back-edges.** A failed gate may re-open `implement`; the edge is
//!   declared and its traversals are counted, so `verify → implement` is
//!   expressible and a second traversal stops.
//! - **Break predicates, not limits.** The loop stops on strict conditions
//!   (attempt budget, same error, no artifact change, gate-failure streak,
//!   output decline, cross-node cycle, stall) and pauses at human gates. There
//!   is no spend ceiling.
//! - **A burn monitor.** `executor` and `active_ms` are recorded per attempt, and
//!   non-blocking warnings compare spend against accepted artifacts. A repeated
//!   warning pauses for a human; nothing silently aborts.
//!
//! This crate owns the *only* route/gate table and the decision + mutation
//! logic. The agent's `graph` tool wraps it with file/store persistence, and the
//! plan UI calls `coordinate`/`advance` directly, so the two can never disagree.

use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    pub fn tokens(&self) -> u64 {
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

/// The fixed dev-flow route (D6) — the `dev-flow` #1–12 stages in order.
pub const ROUTE: &[&str] = &[
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
pub const GATE_NODES: &[&str] = &["verify", "compile", "accept"];

/// Declared back-edges. A failed gate returns to `implement`; every other
/// backward transition is undeclared and refused.
pub const BACK_EDGES: &[(&str, &str)] = &[
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
pub const HUMAN_GATES: &[&str] = &["merge", "release", "complete"];

/// Attempt budget for a single node before it is declared exhausted (D12).
pub const RETRY_BUDGET: usize = 3;

/// The same gate failing this many times in a plan stops the loop (Layer C row 4).
pub const GATE_FAILURE_STREAK: usize = 3;

/// Output below this percentage of the previous attempt stops the loop (row 5).
pub const OUTPUT_DECLINE_PERCENT: u64 = 30;

/// No heartbeat for this long while a node is active stops the loop (row 7).
pub const STALL_SECONDS: i64 = 15 * 60;

/// The number of samples a baseline needs before a warning stops being
/// provisional.
pub const MIN_BASELINE_SAMPLES: u32 = 3;

/// 1.24 cold baselines (57 task runs): p90 343k tokens. Used only while a node
/// type has fewer than three samples.
pub const PROVISIONAL_P90_TOKENS: u64 = 343_000;

/// The fast-path threshold (D13 / P7): a plan whose stated budget stays below
/// this is not worth traversing the graph for. Stated in total tokens. Tunable;
/// calibrated against plan 1.24's eval baseline, where the cheapest measured
/// task used ~35k input tokens.
pub const FAST_PATH_THRESHOLD_TOKENS: u64 = 10_000;

/// The `graph init` scope value that marks a plan debugger-scoped (1.29 D8).
pub const DEBUGGER_SCOPE: &str = "debugger";

/// Incoming edges for each node. `merge` has exactly one: `accept`.
pub fn prereqs(node: &str) -> &'static [&'static str] {
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

/// Nodes that halt automatic traversal until a human authorizes them.
pub fn is_human_gate(node: &str) -> bool {
    HUMAN_GATES.contains(&node)
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
        // Accept produces an evidence reference (a VERIFY report + live call),
        // never an empty "I ran it" claim.
        "claim" | "fork" | "accept" => ArtifactContract::NonEmpty,
        // Compile/merge/release produce a build, merge, or release commit sha.
        "compile" | "merge" | "release" => ArtifactContract::CommitSha,
        _ => ArtifactContract::Any,
    }
}

pub fn validate_artifact(node: &str, artifact: &str) -> Result<(), String> {
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
pub struct Attempt {
    pub at: String,
    pub artifact: String,
    pub failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<NodeCost>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<BurnWarning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoneEntry {
    pub node: String,
    pub artifact: String,
    pub at: String,
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub attempts: Vec<Attempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<NodeCost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blocker {
    pub edge: String,
    pub reason: String,
    #[serde(default)]
    pub spent_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<FailureRecord>,
    /// Which human gate is blocked, when `edge == "human_gate"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// A one-shot authorization for a human gate: minted by `resume` when a human
/// approves a blocked gate, and consumed by `advance`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizedGate {
    pub node: String,
    /// The human's approval, recorded for the trace (e.g. "approved in thread N").
    pub artifact: String,
    pub at: String,
}

/// The node currently being executed. Its active window is the interval from
/// `started_at` to the advance that completes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveNode {
    pub node: String,
    pub executor: String,
    pub started_at: String,
    pub last_activity_at: String,
}

/// One traversal of a declared back-edge (Layer C row 6 counts them).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackEdgeTraversal {
    pub from: String,
    pub to: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphState {
    pub plan: String,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub done: Vec<DoneEntry>,
    #[serde(default)]
    pub next: Vec<String>,
    #[serde(default)]
    pub blocker: Option<Blocker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<CostBudget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<ActiveNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub back_edges: Vec<BackEdgeTraversal>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<BurnWarning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized: Option<AuthorizedGate>,
}

/// The decision `coordinate` returns. `dispatch` and `continue` advance the
/// loop; `pause` waits for a human; `stop` requires an explicit `resume`.
#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
    pub fresh_executor: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    pub spent_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<FailureRecord>,
}

/// The permitted successors, derived from the route and `done` (D6 — never stored).
///
/// Presence-based by design: D17 makes a failed gate advisory for an unprotected
/// change (merge may still proceed), so the edge guard cannot hard-block on it.
/// The *coordinator* prefers a retry of a failed node before any successor —
/// see `retry_candidate`.
pub fn derive_next(state: &GraphState) -> Vec<String> {
    let done: HashSet<&str> = state.done.iter().map(|entry| entry.node.as_str()).collect();
    ROUTE
        .iter()
        .filter(|&node| !done.contains(node) && prereqs(node).iter().all(|p| done.contains(*p)))
        .map(|node| (*node).to_string())
        .collect()
}

/// The first route node that is unfinished and whose prerequisites are all done.
pub fn next_ready(done: &HashSet<&str>) -> Option<&'static str> {
    ROUTE
        .iter()
        .copied()
        .find(|node| !done.contains(node) && prereqs(node).iter().all(|p| done.contains(p)))
}

/// A node recorded but not passed, which must be retried before its successors
/// are dispatched. Route order makes the choice deterministic.
pub fn retry_candidate(state: &GraphState) -> Option<String> {
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

/// The node `coordinate` would act on next, computed without its side effects
/// (`coordinate` persists a blocker when it pauses). Mirrors `coordinate`'s
/// precedence — a blocked human gate, then an active node, then a
/// recorded-but-failed node to retry, then the first ready node — so the banner
/// and the tool gate can never disagree with `coordinate`.
pub fn next_decision_node(state: &GraphState) -> Option<String> {
    if let Some(blocker) = &state.blocker {
        if let Some(node) = &blocker.node {
            return Some(node.clone());
        }
    }
    if let Some(active) = &state.active {
        return Some(active.node.clone());
    }
    retry_candidate(state).or_else(|| derive_next(state).into_iter().next())
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

pub fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

pub fn spent_tokens(state: &GraphState) -> u64 {
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
pub fn burn_warnings(
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
        let latest_output = latest
            .cost
            .as_ref()
            .map(|cost| cost.output_tokens)
            .unwrap_or(0);
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

pub fn brief_for(node: &str, plan: &str, retrying: bool) -> String {
    let retry_note = if retrying && GATE_NODES.contains(&node) {
        " This gate failed on a previous attempt: if the failure is in the implementation, \
         advance `implement` instead to re-open it (a declared back-edge)."
    } else {
        ""
    };
    let accept_note = if node == "accept" {
        " `accept` requires a live launch of the artifact plus a real exercise of the feature — \
         a headless verifier verdict alone is not acceptance."
    } else {
        ""
    };
    format!(
        "Dispatch {node} for {plan} to a fresh executor. It must act on external evidence, \
         return the node's declared artifact, and record its cost from usage deltas — never a \
         self-report.{retry_note}{accept_note}"
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
pub fn coordinate(state: &mut GraphState, ask: bool, now: DateTime<Utc>) -> Decision {
    if let Some(blocker) = &state.blocker {
        // A human gate stays a pause, not a stop: it is a decision, not a break.
        if blocker.edge == "human_gate" {
            return pause_decision(
                state,
                blocker.node.clone(),
                Some("human_gate"),
                blocker.reason.clone(),
            );
        }
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
            node: None,
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
        let spent = spent_tokens(state);
        state.blocker = Some(Blocker {
            edge: "human_gate".to_string(),
            reason: format!(
                "{node} is a human gate — automatic traversal halts until a human authorizes it via `resume`"
            ),
            spent_tokens: spent,
            last_failure: None,
            node: Some(node.to_string()),
        });
        return pause_decision(
            state,
            Some(node.to_string()),
            Some("human_gate"),
            format!(
                "{node} is a human gate — automatic traversal halts; authorize it via `resume`"
            ),
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
pub fn begin(
    state: &mut GraphState,
    node: &str,
    executor: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
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

pub fn heartbeat(state: &mut GraphState, now: DateTime<Utc>) -> Result<(), String> {
    let Some(active) = &mut state.active else {
        return Err("no node is active".to_string());
    };
    active.last_activity_at = now.to_rfc3339();
    Ok(())
}

pub fn resume(
    state: &mut GraphState,
    artifact: &str,
    now: DateTime<Utc>,
) -> Result<String, String> {
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

    // A human gate's resume is the human's authorization: mint a one-shot token
    // that `advance` consumes, so the gate cannot be auto-advanced.
    if blocker.edge == "human_gate" {
        let node = blocker.node.unwrap_or_default();
        state.authorized = Some(AuthorizedGate {
            node: node.clone(),
            artifact: artifact.to_string(),
            at: now.to_rfc3339(),
        });
        return Ok(format!(
            "authorized {node} ({artifact}); a single `advance {node}` may now proceed — {reset}next: {}",
            state.next.join(", ")
        ));
    }

    Ok(format!(
        "resumed after {:?} — spent {} tokens; {reset}next: {}",
        blocker.edge,
        blocker.spent_tokens,
        state.next.join(", ")
    ))
}

/// Refuse to complete a human gate that has not been authorized by a prior
/// `resume`; consume the one-shot authorization on success.
fn authorize_human_gate(state: &mut GraphState, node: &str) -> Result<(), String> {
    if !HUMAN_GATES.contains(&node) {
        return Ok(());
    }
    match state.authorized.take() {
        Some(authorized) if authorized.node.as_str() == node => Ok(()),
        Some(authorized) => Err(format!(
            "{node} is a human gate but {other} was authorized — authorize {node} via `resume` first",
            other = authorized.node
        )),
        None => Err(format!(
            "{node} is a human gate — `coordinate` pauses there; the human must authorize it via `resume` before `advance`"
        )),
    }
}

/// The edge guard: a transition is refused unless every transitive prerequisite
/// is done, the artifact satisfies its contract, and — on `merge` of a protected
/// change — no verification gate failed.
#[allow(clippy::too_many_arguments)]
pub fn advance(
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
                    "protected change: a verification gate failed — merge blocked".to_string(),
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

    // A human gate is the one transition the coordinator pauses at; `advance`
    // must not complete it unless a prior `resume` authorized it (one-shot).
    authorize_human_gate(state, node)?;

    state.active = None;
    state.warnings.extend(warnings);
    state.current = Some(node.to_string());
    state.next = derive_next(state);
    Ok(())
}

/// Read a plan's `progress.json`. The error text is the caller-facing message.
pub fn load_state(path: &Path) -> Result<GraphState, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    serde_json::from_str(&content)
        .map_err(|error| format!("Failed to parse {}: {error}", path.display()))
}

/// Write a plan's `progress.json`, creating parent directories as needed.
pub fn save_state(path: &Path, state: &GraphState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(state)
        .map_err(|error| format!("Failed to serialize dev-flow state: {error}"))?;
    std::fs::write(path, content)
        .map_err(|error| format!("Failed to write {}: {error}", path.display()))?;
    Ok(())
}
