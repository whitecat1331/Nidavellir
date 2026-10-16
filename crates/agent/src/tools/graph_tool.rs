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
    "plan", "promote", "claim", "fork", "implement", "verify", "compile", "accept", "merge",
    "release", "complete",
];

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DoneEntry {
    node: String,
    artifact: String,
    at: String,
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

/// The edge guard: a transition is refused unless every transitive prerequisite is done.
fn advance(state: &mut GraphState, node: &str, artifact: &str) -> Result<(), String> {
    if !ROUTE.contains(&node) {
        return Err(format!("unknown node: {node}"));
    }
    let done: HashSet<&str> = state.done.iter().map(|entry| entry.node.as_str()).collect();
    if done.contains(node) {
        return Err(format!("{node} already done"));
    }
    if let Some(blocker) = &state.blocker {
        return Err(format!("blocked: {}", blocker.reason));
    }

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

    state.done.push(DoneEntry {
        node: node.to_string(),
        artifact: artifact.to_string(),
        at: chrono::Utc::now().to_rfc3339(),
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
            let input = input
                .recv()
                .await
                .map_err(|error| GraphToolOutput::Error {
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
            };
            save_state(&path, &state)?;
            Ok(GraphToolOutput::Success {
                message: format!("Initialized dev-flow state: {}", state_json(&state)),
            })
        }
        GraphOperation::Read => {
            let mut state = load_state(&path)?;
            state.next = derive_next(&state);
            Ok(GraphToolOutput::Success {
                message: state_json(&state),
            })
        }
        GraphOperation::Advance => {
            let node = input.node.ok_or_else(|| GraphToolOutput::Error {
                error: "`node` is required for `operation: \"advance\"`.".to_string(),
            })?;
            let artifact = input.artifact.unwrap_or_default();
            let mut state = load_state(&path)?;
            advance(&mut state, &node, &artifact)
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

    fn fresh() -> GraphState {
        GraphState {
            plan: "TRIVIAL".to_string(),
            repo: "zed".to_string(),
            current: None,
            done: Vec::new(),
            next: vec!["plan".to_string()],
            blocker: None,
        }
    }

    #[test]
    fn happy_path_walks_the_route() {
        let mut state = fresh();
        for node in ROUTE {
            advance(&mut state, node, "artifact").unwrap();
        }
        assert_eq!(state.current.as_deref(), Some("complete"));
        assert!(state.next.is_empty());
    }

    #[test]
    fn edge_guard_refuses_skip_to_merge() {
        let mut state = fresh();
        for node in ["plan", "promote", "claim", "fork", "implement"] {
            advance(&mut state, node, "").unwrap();
        }
        let error = advance(&mut state, "merge", "").unwrap_err();
        assert!(error.contains("verify"), "error was: {error}");
        assert!(error.contains("compile"), "error was: {error}");
        assert!(error.contains("accept"), "error was: {error}");
    }

    #[test]
    fn derive_next_matches_prereqs() {
        let mut state = fresh();
        state.done = vec![DoneEntry {
            node: "plan".to_string(),
            artifact: String::new(),
            at: String::new(),
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
            },
            path.clone(),
        )
        .unwrap();
        assert!(matches!(output, GraphToolOutput::Success { .. }));

        let loaded = load_state(&path).unwrap();
        assert_eq!(loaded.plan, "PLAN_A");
        assert_eq!(loaded.next, vec!["plan".to_string()]);
    }
}
