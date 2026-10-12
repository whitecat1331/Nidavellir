use agent_client_protocol::schema::v1 as acp;
use agent_settings::builtin_profiles;
use anyhow::{Context as _, Result, anyhow};
use dap::{DapRegistry, SteppingGranularity, client::SessionId};
use gpui::{App, Entity, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use project::{Project, WorktreeId, debugger::agent_api::*};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, rc::Rc, sync::Arc, time::Duration};
use task::{DebugScenario, SharedTaskContext};
use util::{markdown::MarkdownInlineCode, paths::normalize_lexically};

use crate::{
    AgentTool, DebugSessionRequest, Thread, ThreadEnvironment, ToolCallEventStream, ToolInput,
    ToolPermissionContext,
    sandboxing::{SandboxRequest, sandboxing_enabled},
};

const DEFAULT_CONTROL_TIMEOUT_MS: u64 = 30_000;
const MAX_CONTROL_TIMEOUT_MS: u64 = 300_000;
const MAX_SNAPSHOT_FRAMES: usize = 200;
const MAX_SNAPSHOT_VARIABLES_PER_SCOPE: usize = 500;
const MAX_SNAPSHOT_VARIABLE_VALUE_LENGTH: usize = 16 * 1024;
const MAX_SNAPSHOT_OUTPUT_EVENTS: usize = 1_000;
const MAX_SNAPSHOT_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_SNAPSHOT_SOURCE_CONTEXT_LINES: usize = 200;
const SESSION_BOOT_POLL_INTERVAL_MS: u64 = 250;
const SESSION_BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// Interact with Zed's debugger. Read-only operations such as `snapshot`,
/// `list_sessions`, `list_breakpoints`, and `list_adapters` are available in
/// read-only modes (`read` and `plan`). Operations that start sessions, change
/// breakpoints, control execution, evaluate expressions, or change variables
/// require a mode that can execute (`execute` or `debug`) and user permission.
///
/// Prefer `snapshot` when inspecting a paused debug session: it returns a
/// bounded view of threads, stack frames, source context, variables, and recent
/// output in one call. Use `list_sessions` first when there are multiple active
/// sessions.
///
/// <guidelines>
/// - In read-only modes, only use read-only operations.
/// - Before controlling execution, inspect `list_sessions` or `snapshot` and use
///   explicit `session_id` and `thread_id` when possible.
/// - `continue`, `step`, `pause`, and `run_to_line` wait for the debugger to
///   stop, exit, or time out, then return a fresh snapshot.
/// - `detach` disconnects from an attach session without terminating the
///   debuggee; `restart` sends the DAP restart request; `restart_frame`
///   restarts the current stack frame and requires a `frame_id` from a
///   snapshot. These return an acknowledgement rather than a fresh snapshot.
/// - `start_session` runs code through Zed's debugger UI; pass the launch
///   config fields at the scenario top level (`{"adapter", "label",
///   "request", "program", "cwd", ...}`). A nested `"config"` object is
///   also accepted. Program output is routed to the debug console so that
///   snapshots include it.
/// - `evaluate` runs an expression in the debuggee using the DAP `watch`
///   context (the adapter's expression evaluator, not its REPL command
///   interpreter) and returns the result string plus its `variables_reference`;
///   pass a `frame_id` from a snapshot to scope the evaluation to a specific
///   stack frame. It can run code or mutate state, so it requires Write mode.
/// - `set_variable` changes a variable's value. Pass the container
///   `variables_reference` (a scope's `variables_reference` from a snapshot),
///   the variable `name`, and the new `value`; follow with a `snapshot` to
///   confirm. Requires Write mode.
/// </guidelines>
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DebuggerOperation {
    /// List active debug sessions.
    #[default]
    ListSessions,
    /// Inspect debugger state for a session.
    Snapshot,
    /// List source breakpoints in the project.
    ListBreakpoints,
    /// Add or update source breakpoints.
    SetBreakpoints,
    /// Remove source breakpoints.
    RemoveBreakpoints,
    /// Continue, pause, step, run to a line, detach, restart, or restart a stack frame.
    Control,
    /// List registered debug adapters and their configuration schemas.
    ListAdapters,
    /// Start a debug session through Zed's debugger UI.
    StartSession,
    /// Stop a debug session.
    StopSession,
    /// Evaluate an expression in a debug session.
    Evaluate,
    /// Set the value of a variable in a debug session.
    SetVariable,
    /// List exception breakpoint filters for a debug session.
    ListExceptionBreakpoints,
    /// Enable or disable exception breakpoint filters for a debug session.
    SetExceptionBreakpoints,
    /// List data breakpoints for a debug session.
    ListDataBreakpoints,
    /// Set data breakpoints on variables in a debug session.
    SetDataBreakpoints,
    /// Toggle ignoring all breakpoints for a debug session.
    SetIgnoreBreakpoints,
    /// Clear all source breakpoints in the project.
    ClearBreakpoints,
    /// Read raw memory from a debug session.
    ReadMemory,
    /// List historic debug session snapshots.
    ListHistory,
    /// Select a historic debug session snapshot.
    SelectHistory,
    /// List watch expressions for a debug session.
    ListWatchExpressions,
    /// Add a watch expression to a debug session.
    AddWatchExpression,
    /// Remove a watch expression from a debug session.
    RemoveWatchExpression,
    /// List the modules (DLLs/shared libraries) loaded by the debuggee.
    ListModules,
    /// List the source files the debugger has loaded.
    ListLoadedSources,
}

/// A single debugger operation and the fields it needs.
///
/// Kept as a flat struct (rather than a tagged enum) so the JSON schema
/// advertised to language-model providers is a `type: "object"` with an
/// `operation` discriminator. Some providers reject top-level union schemas
/// that internally-tagged enums produce.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct DebuggerToolInput {
    /// Which debugger operation to run.
    pub operation: DebuggerOperation,
    /// DAP session id, used by snapshot, control, and stop_session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    /// Optional bounds for the snapshot operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<SnapshotLimitsInput>,
    /// Source breakpoints. `set_breakpoints` uses every field;
    /// `remove_breakpoints` only reads `path` and `line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breakpoints: Option<Vec<BreakpointInput>>,
    /// DAP thread id, used by control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
    /// Execution control action, used by control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ControlAction>,
    /// Step granularity for control step actions. One of `line`, `statement`,
    /// or `instruction`. Defaults to `line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granularity: Option<SteppingGranularity>,
    /// Source path for control run_to_line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// 1-based line for control run_to_line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Maximum time to wait for the debugger to stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Optional bounds for the snapshot returned after control completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_limits: Option<SnapshotLimitsInput>,
    /// Debug scenario for start_session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<DebugScenario>,
    /// Optional worktree id for start_session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<u64>,
    /// Expression to evaluate, used by evaluate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    /// Stack frame id, used by evaluate, set_variable, and control `restart_frame`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<u64>,
    /// Variable container reference, used by set_variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variables_reference: Option<u64>,
    /// Variable name, used by set_variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// New variable value, used by set_variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Exception breakpoint filters, used by set_exception_breakpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception_breakpoints: Option<Vec<ExceptionBreakpointInput>>,
    /// Data breakpoints, used by set_data_breakpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_breakpoints: Option<Vec<DataBreakpointInput>>,
    /// Whether to ignore all breakpoints, used by set_ignore_breakpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore: Option<bool>,
    /// Memory reference for read_memory (e.g. "0x1000").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_reference: Option<String>,
    /// Byte offset into the memory reference, used by read_memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Number of bytes to read, used by read_memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// History snapshot index, used by select_history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SnapshotInput {
    /// DAP session id. When omitted, uses the active debug session, otherwise the first session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    /// Optional bounds for returned stack, variables, output, and source context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<SnapshotLimitsInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SnapshotLimitsInput {
    /// Maximum total stack frames across all stopped threads. Defaults to 20; maximum 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_frames: Option<usize>,
    /// Maximum variables per scope. Defaults to 50; maximum 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_variables_per_scope: Option<usize>,
    /// Maximum bytes per variable value. Defaults to 1024; maximum 16384.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_variable_value_length: Option<usize>,
    /// Maximum recent output events. Defaults to 100; maximum 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_events: Option<usize>,
    /// Maximum recent output bytes. Defaults to 16384; maximum 1048576.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<usize>,
    /// Maximum source context lines around each frame. Defaults to 5; maximum 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_source_context_lines: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct BreakpointInput {
    /// Absolute source path as reported by the debugger, or a project-resolvable absolute path.
    pub path: PathBuf,
    /// 1-based line number.
    pub line: u32,
    /// Whether the breakpoint should be enabled. Defaults to true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional condition expression that must be true for the breakpoint to stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Optional hit count condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    /// Optional logpoint message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct BreakpointLocationInput {
    /// Absolute source path as reported by the debugger, or a project-resolvable absolute path.
    pub path: PathBuf,
    /// 1-based line number.
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ExceptionBreakpointInput {
    /// The adapter's exception filter id (from a snapshot or list).
    pub id: String,
    /// Whether the filter should be enabled.
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct DataBreakpointInput {
    /// The variable container reference (a scope or variable's `variables_reference` from a snapshot).
    pub variables_reference: u64,
    /// The variable name to set the data breakpoint on.
    pub name: String,
    /// Optional access type: "read", "write", or "readWrite".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_type: Option<String>,
    /// Optional condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Optional hit condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ControlInput {
    /// DAP session id. When omitted, uses the active debug session, otherwise the first session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    /// DAP thread id. When omitted, chooses a suitable thread based on the action.
    /// Not used by `detach`, `restart`, or `restart_frame`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
    /// Execution control action.
    pub action: ControlAction,
    /// Step granularity for step actions. Defaults to `line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granularity: Option<SteppingGranularity>,
    /// Source path for `run_to_line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// 1-based line for `run_to_line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Stack frame id for `restart_frame`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<u64>,
    /// Maximum time to wait for the debugger to stop. Defaults to 30000ms; maximum 300000ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Optional bounds for the snapshot returned after control completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_limits: Option<SnapshotLimitsInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Continue,
    Pause,
    StepOver,
    StepIn,
    StepOut,
    StepBack,
    RunToLine,
    Detach,
    Restart,
    RestartFrame,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct StartSessionInput {
    /// Debug scenario to start. This is the same shape as Zed debug scenarios:
    /// include `adapter`, `label`, and adapter-specific launch/attach config.
    pub scenario: DebugScenario,
    /// Optional worktree id. Omit to use the active buffer's worktree or first visible worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DebuggerToolOutput {
    Success {
        operation: String,
        message: String,
        data: Value,
    },
    Error {
        operation: Option<String>,
        error: String,
    },
}

impl From<DebuggerToolOutput> for LanguageModelToolResultContent {
    fn from(output: DebuggerToolOutput) -> Self {
        match &output {
            DebuggerToolOutput::Success {
                operation,
                message,
                data,
            } => {
                let data = serde_json::to_string_pretty(data).unwrap_or_else(|error| {
                    format!("<failed to serialize debugger output: {error}>")
                });
                format!("Debugger `{operation}` succeeded: {message}\n\n```json\n{data}\n```")
                    .into()
            }
            DebuggerToolOutput::Error { operation, error } => {
                let operation = operation.as_deref().unwrap_or("unknown");
                format!("Debugger `{operation}` failed: {error}").into()
            }
        }
    }
}

pub struct DebuggerTool {
    project: Entity<Project>,
    environment: Rc<dyn ThreadEnvironment>,
    thread: WeakEntity<Thread>,
}

impl DebuggerTool {
    pub fn new(
        project: Entity<Project>,
        environment: Rc<dyn ThreadEnvironment>,
        thread: WeakEntity<Thread>,
    ) -> Self {
        Self {
            project,
            environment,
            thread,
        }
    }

    fn api(&self, cx: &App) -> AgentDebuggerApi {
        let project = self.project.read(cx);
        AgentDebuggerApi::new(project.dap_store(), project.breakpoint_store())
    }

    fn is_read_only_profile(&self, cx: &App) -> bool {
        self.thread
            .read_with(cx, |thread, _| {
                builtin_profiles::is_read_only(thread.profile())
            })
            .unwrap_or(false)
    }
}

impl AgentTool for DebuggerTool {
    type Input = DebuggerToolInput;
    type Output = DebuggerToolOutput;

    const NAME: &'static str = "debugger";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => initial_title_for_input(&input),
            Err(value) => value
                .get("operation")
                .and_then(|value| value.as_str())
                .map(|operation| format!("Debugger: {operation}").into())
                .unwrap_or_else(|| "Debugger".into()),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| DebuggerToolOutput::Error {
                    operation: None,
                    error: format!("Failed to receive debugger tool input: {error}"),
                })?;
            let operation = operation_name(&input).to_string();
            match self
                .run_operation(input, operation.clone(), event_stream, cx)
                .await
            {
                Ok(output) => Ok(output),
                Err(error) => Err(DebuggerToolOutput::Error {
                    operation: Some(operation),
                    error: error.to_string(),
                }),
            }
        })
    }
}

impl DebuggerTool {
    async fn run_operation(
        self: Arc<Self>,
        input: DebuggerToolInput,
        operation: String,
        event_stream: ToolCallEventStream,
        cx: &mut gpui::AsyncApp,
    ) -> Result<DebuggerToolOutput> {
        match input.operation {
            DebuggerOperation::ListSessions => {
                let data = cx.update(|cx| sessions_to_json(self.api(cx).list_sessions(cx)));
                Ok(success(operation, "listed debug sessions", data))
            }
            DebuggerOperation::Snapshot => {
                let session_id = input.session_id;
                let limits = limits_from_input(input.limits)?;
                let (api, session_id) = cx.update(|cx| {
                    let api = self.api(cx);
                    let session_id = resolve_session_id(&self.project, &api, session_id, cx)?;
                    anyhow::Ok((api, session_id))
                })?;
                let snapshot_task = cx.update(|cx| api.snapshot(session_id, limits, None, cx));
                let snapshot = snapshot_task.await?;
                Ok(success(
                    operation,
                    "captured debugger snapshot",
                    snapshot_to_json(snapshot),
                ))
            }
            DebuggerOperation::ListBreakpoints => {
                let data = cx.update(|cx| breakpoints_to_json(self.api(cx).list_breakpoints(cx)));
                Ok(success(operation, "listed breakpoints", data))
            }
            DebuggerOperation::ListAdapters => {
                let data = cx
                    .update(|cx| serde_json::to_value(DapRegistry::global(cx).adapters_schema()))?;
                Ok(success(operation, "listed debug adapters", data))
            }
            DebuggerOperation::SetBreakpoints => {
                self.ensure_write_mode(&operation, cx)?;
                let breakpoints = input
                    .breakpoints
                    .context("breakpoints is required for debugger set_breakpoints")?;
                let breakpoints = breakpoints
                    .into_iter()
                    .map(|breakpoint| resolve_breakpoint_input(&self.project, breakpoint, cx))
                    .collect::<Result<Vec<_>>>()?;
                authorize_debugger_operation(
                    &event_stream,
                    "Set debugger breakpoint(s)",
                    breakpoint_permission_inputs(&operation, breakpoints.iter())?,
                    cx,
                )
                .await?;

                let api = cx.update(|cx| self.api(cx));
                let mut results = Vec::new();
                for breakpoint in breakpoints {
                    let task = cx
                        .update(|cx| api.set_source_breakpoint(breakpoint.into_agent_input(), cx));
                    let result = task.await?;
                    results.push(breakpoint_edit_result_to_json(result));
                }
                Ok(success(
                    operation,
                    "set breakpoint(s)",
                    Value::Array(results),
                ))
            }
            DebuggerOperation::RemoveBreakpoints => {
                self.ensure_write_mode(&operation, cx)?;
                let breakpoints = input
                    .breakpoints
                    .context("breakpoints is required for debugger remove_breakpoints")?
                    .into_iter()
                    .map(|breakpoint| BreakpointLocationInput {
                        path: breakpoint.path,
                        line: breakpoint.line,
                    })
                    .collect::<Vec<_>>();
                let breakpoints = breakpoints
                    .into_iter()
                    .map(|breakpoint| resolve_breakpoint_location(&self.project, breakpoint, cx))
                    .collect::<Result<Vec<_>>>()?;
                authorize_debugger_operation(
                    &event_stream,
                    "Remove debugger breakpoint(s)",
                    breakpoint_location_permission_inputs(&operation, breakpoints.iter()),
                    cx,
                )
                .await?;

                let api = cx.update(|cx| self.api(cx));
                let mut results = Vec::new();
                for breakpoint in breakpoints {
                    let task = cx.update(|cx| {
                        api.remove_source_breakpoint(breakpoint.path, breakpoint.line, cx)
                    });
                    let result = task.await?;
                    results.push(breakpoint_edit_result_to_json(result));
                }
                Ok(success(
                    operation,
                    "removed breakpoint(s)",
                    Value::Array(results),
                ))
            }
            DebuggerOperation::Control => {
                self.ensure_write_mode(&operation, cx)?;
                let control_input = ControlInput {
                    session_id: input.session_id,
                    thread_id: input.thread_id,
                    action: input
                        .action
                        .context("action is required for debugger control")?,
                    granularity: input.granularity,
                    path: input.path,
                    line: input.line,
                    frame_id: input.frame_id,
                    timeout_ms: input.timeout_ms,
                    snapshot_limits: input.snapshot_limits,
                };
                validate_control_timeout(control_input.timeout_ms)?;
                let snapshot_limits = limits_from_input(control_input.snapshot_limits.clone())?;
                let resolved_input = self.resolve_control_input(control_input, cx).await?;
                let action = resolved_input.action;
                authorize_debugger_operation(
                    &event_stream,
                    format!("Debugger {}", action.label()),
                    permission_inputs(&operation, [control_permission_input(&resolved_input)]),
                    cx,
                )
                .await?;

                let (session_id, control_result) = self.run_control(resolved_input, cx).await?;
                let Some(control_result) = control_result else {
                    return Ok(success(
                        operation,
                        format!("controlled debugger execution ({})", action.label()),
                        json!({ "action": action.label() }),
                    ));
                };
                let preferred_thread_id = control_result.stopped_thread_id;
                let api = cx.update(|cx| self.api(cx));
                let snapshot_task = cx.update(|cx| {
                    api.snapshot(session_id, snapshot_limits, preferred_thread_id, cx)
                });
                let snapshot = snapshot_task.await?;
                Ok(success(
                    operation,
                    "controlled debugger execution and captured snapshot",
                    json!({
                        "control": control_result_to_json(control_result),
                        "snapshot": snapshot_to_json(snapshot),
                    }),
                ))
            }
            DebuggerOperation::StartSession => {
                self.ensure_write_mode(&operation, cx)?;
                let mut start_session_input = StartSessionInput {
                    scenario: input
                        .scenario
                        .context("scenario is required for debugger start_session")?,
                    worktree_id: input.worktree_id,
                };
                normalize_scenario_config(&mut start_session_input.scenario);
                ensure_stop_on_entry(&mut start_session_input.scenario);
                ensure_console_output(&mut start_session_input.scenario);
                authorize_debugger_operation(
                    &event_stream,
                    format!(
                        "Start debug session {}",
                        MarkdownInlineCode(&start_session_input.scenario.label)
                    ),
                    start_session_permission_inputs(&operation, &start_session_input)?,
                    cx,
                )
                .await?;

                if cx.update(|cx| sandboxing_enabled(cx)) {
                    let request = SandboxRequest {
                        unsandboxed: true,
                        ..Default::default()
                    };
                    let approve = cx.update(|cx| {
                        event_stream.authorize_sandbox(
                            request,
                            "Start debug session outside the agent terminal sandbox".to_string(),
                            cx,
                        )
                    });
                    approve.await?;
                }

                let request = DebugSessionRequest {
                    scenario: start_session_input.scenario,
                    task_context: SharedTaskContext::default(),
                    active_buffer: None,
                    worktree_id: start_session_input.worktree_id.map(WorktreeId::from_proto),
                };
                let info = self.environment.start_debug_session(request, cx).await?;
                let api = cx.update(|cx| self.api(cx));
                await_session_boot(api, info.session_id, cx).await?;
                Ok(success(
                    operation,
                    "started debug session",
                    json!({
                        "session_id": info.session_id,
                        "label": info.label,
                        "adapter": info.adapter,
                    }),
                ))
            }
            DebuggerOperation::StopSession => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = input
                    .session_id
                    .context("session_id is required for debugger stop_session")?;
                authorize_debugger_operation(
                    &event_stream,
                    format!("Stop debug session {session_id}"),
                    permission_inputs(
                        &operation,
                        [format!("stop_session session_id:{session_id}")],
                    ),
                    cx,
                )
                .await?;
                let project = self.project.clone();
                let shutdown = cx.update(|cx| {
                    let dap_store = project.read(cx).dap_store();
                    dap_store.update(cx, |dap_store, cx| {
                        dap_store.shutdown_session(SessionId::from_proto(session_id), cx)
                    })
                });
                // Bound the wait for the adapter to confirm the disconnect: a
                // wedged debuggee must not block the tool (ISSUE-0046).
                futures::pin_mut!(shutdown);
                match futures::future::select(
                    shutdown,
                    cx.background_executor().timer(Duration::from_secs(5)),
                )
                .await
                {
                    futures::future::Either::Left((result, _)) => result?,
                    futures::future::Either::Right(_) => {}
                }
                Ok(success(
                    operation,
                    "stopped debug session",
                    json!({ "session_id": session_id }),
                ))
            }
            DebuggerOperation::Evaluate => {
                self.ensure_write_mode(&operation, cx)?;
                let expression = input
                    .expression
                    .context("expression is required for debugger evaluate")?;
                let frame_id = input.frame_id;
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                authorize_debugger_operation(
                    &event_stream,
                    format!("Debugger evaluate {}", MarkdownInlineCode(&expression)),
                    evaluate_permission_inputs(&operation, session_id, &expression, frame_id)?,
                    cx,
                )
                .await?;
                let _guard = api.acquire_agent_control(session_id, cx)?;
                let task = cx.update(|cx| {
                    api.evaluate(
                        session_id,
                        expression,
                        dap::EvaluateArgumentsContext::Watch,
                        frame_id,
                        cx,
                    )
                });
                let result = task.await?;
                Ok(success(
                    operation,
                    "evaluated expression",
                    evaluate_result_to_json(result),
                ))
            }
            DebuggerOperation::SetVariable => {
                self.ensure_write_mode(&operation, cx)?;
                let frame_id = input.frame_id;
                let variables_reference = input
                    .variables_reference
                    .context("variables_reference is required for debugger set_variable")?;
                let name = input
                    .name
                    .context("name is required for debugger set_variable")?;
                let value = input
                    .value
                    .context("value is required for debugger set_variable")?;
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                authorize_debugger_operation(
                    &event_stream,
                    format!("Debugger set variable {}", MarkdownInlineCode(&name)),
                    set_variable_permission_inputs(&operation, session_id, &name, &value)?,
                    cx,
                )
                .await?;
                let _guard = api.acquire_agent_control(session_id, cx)?;
                let task = cx.update(|cx| {
                    api.set_variable(session_id, frame_id, variables_reference, name, value, cx)
                });
                let result = task.await?;
                Ok(success(
                    operation,
                    "set variable value",
                    set_variable_result_to_json(result),
                ))
            }
            DebuggerOperation::ListExceptionBreakpoints => {
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                let data = cx.update(|cx| api.list_exception_breakpoints(session_id, cx))?;
                Ok(success(
                    operation,
                    "listed exception breakpoints",
                    exception_breakpoints_to_json(data),
                ))
            }
            DebuggerOperation::SetExceptionBreakpoints => {
                self.ensure_write_mode(&operation, cx)?;
                let breakpoints = input.exception_breakpoints.context(
                    "exception_breakpoints is required for debugger set_exception_breakpoints",
                )?;
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                authorize_debugger_operation(
                    &event_stream,
                    "Set debugger exception breakpoint(s)",
                    exception_breakpoint_permission_inputs(&operation, &breakpoints, session_id),
                    cx,
                )
                .await?;
                let task = cx.update(|cx| {
                    api.set_exception_breakpoints(
                        session_id,
                        breakpoints.into_iter().map(Into::into).collect(),
                        cx,
                    )
                });
                let result = task.await?;
                Ok(success(
                    operation,
                    "set exception breakpoint(s)",
                    exception_breakpoints_to_json(result),
                ))
            }
            DebuggerOperation::ListDataBreakpoints => {
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                let data = cx.update(|cx| api.list_data_breakpoints(session_id, cx))?;
                Ok(success(
                    operation,
                    "listed data breakpoints",
                    data_breakpoints_to_json(data),
                ))
            }
            DebuggerOperation::SetDataBreakpoints => {
                self.ensure_write_mode(&operation, cx)?;
                let breakpoints = input
                    .data_breakpoints
                    .context("data_breakpoints is required for debugger set_data_breakpoints")?;
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                authorize_debugger_operation(
                    &event_stream,
                    "Set debugger data breakpoint(s)",
                    data_breakpoint_permission_inputs(&operation, &breakpoints, session_id),
                    cx,
                )
                .await?;
                let task = cx.update(|cx| {
                    api.set_data_breakpoints(
                        session_id,
                        breakpoints.into_iter().map(Into::into).collect(),
                        cx,
                    )
                });
                let result = task.await?;
                Ok(success(
                    operation,
                    "set data breakpoint(s)",
                    data_breakpoints_to_json(result),
                ))
            }
            DebuggerOperation::SetIgnoreBreakpoints => {
                self.ensure_write_mode(&operation, cx)?;
                let ignore = input
                    .ignore
                    .context("ignore is required for debugger set_ignore_breakpoints")?;
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                authorize_debugger_operation(
                    &event_stream,
                    "Set debugger ignore breakpoints",
                    permission_inputs(
                        &operation,
                        [format!("session_id:{} ignore:{}", session_id.0, ignore)],
                    ),
                    cx,
                )
                .await?;
                let task = cx.update(|cx| api.set_ignore_breakpoints(session_id, ignore, cx));
                task.await?;
                Ok(success(
                    operation,
                    "set ignore breakpoints",
                    json!({ "session_id": session_id.0, "ignore": ignore }),
                ))
            }
            DebuggerOperation::ClearBreakpoints => {
                self.ensure_write_mode(&operation, cx)?;
                authorize_debugger_operation(
                    &event_stream,
                    "Clear all debugger breakpoints",
                    permission_inputs(&operation, std::iter::empty()),
                    cx,
                )
                .await?;
                let api = cx.update(|cx| self.api(cx));
                let task = cx.update(|cx| api.clear_breakpoints(cx));
                task.await?;
                Ok(success(operation, "cleared all breakpoints", json!({})))
            }
            DebuggerOperation::ReadMemory => {
                let memory_reference = input
                    .memory_reference
                    .context("memory_reference is required for debugger read_memory")?;
                let count = input
                    .count
                    .context("count is required for debugger read_memory")?;
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                authorize_debugger_operation(
                    &event_stream,
                    "Read debug session memory",
                    read_memory_permission_inputs(
                        &operation,
                        session_id,
                        &memory_reference,
                        input.offset,
                        count,
                    ),
                    cx,
                )
                .await?;
                let task = cx.update(|cx| {
                    api.read_memory(session_id, memory_reference, input.offset, count, cx)
                });
                let result = task.await?;
                Ok(success(
                    operation,
                    "read debug session memory",
                    memory_read_to_json(result),
                ))
            }
            DebuggerOperation::ListHistory => {
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                let data = cx.update(|cx| api.list_history(session_id, cx))?;
                Ok(success(
                    operation,
                    "listed debug session history",
                    history_to_json(data),
                ))
            }
            DebuggerOperation::SelectHistory => {
                self.ensure_write_mode(&operation, cx)?;
                let index = input
                    .index
                    .context("index is required for debugger select_history")?;
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                authorize_debugger_operation(
                    &event_stream,
                    "Select debug session history snapshot",
                    select_history_permission_inputs(&operation, session_id, index),
                    cx,
                )
                .await?;
                let task = cx.update(|cx| api.select_history(session_id, index, cx));
                task.await?;
                Ok(success(
                    operation,
                    "selected debug session history snapshot",
                    json!({ "session_id": session_id.0, "index": index }),
                ))
            }
            DebuggerOperation::ListWatchExpressions => {
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                let data = cx.update(|cx| api.list_watch_expressions(session_id, cx))?;
                Ok(success(
                    operation,
                    "listed watch expressions",
                    watch_expressions_to_json(data),
                ))
            }
            DebuggerOperation::AddWatchExpression => {
                self.ensure_write_mode(&operation, cx)?;
                let expression = input
                    .expression
                    .context("expression is required for debugger add_watch_expression")?;
                let frame_id = input.frame_id;
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                authorize_debugger_operation(
                    &event_stream,
                    format!(
                        "Add debugger watch expression {}",
                        MarkdownInlineCode(&expression)
                    ),
                    add_watch_expression_permission_inputs(
                        &operation,
                        session_id,
                        &expression,
                        frame_id,
                    )?,
                    cx,
                )
                .await?;
                let _guard = api.acquire_agent_control(session_id, cx)?;
                let task =
                    cx.update(|cx| api.add_watch_expression(session_id, expression, frame_id, cx));
                task.await?;
                Ok(success(
                    operation,
                    "added watch expression",
                    json!({ "session_id": session_id.0 }),
                ))
            }
            DebuggerOperation::RemoveWatchExpression => {
                self.ensure_write_mode(&operation, cx)?;
                let expression = input
                    .expression
                    .context("expression is required for debugger remove_watch_expression")?;
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                authorize_debugger_operation(
                    &event_stream,
                    format!(
                        "Remove debugger watch expression {}",
                        MarkdownInlineCode(&expression)
                    ),
                    remove_watch_expression_permission_inputs(&operation, session_id, &expression)?,
                    cx,
                )
                .await?;
                let task = cx.update(|cx| api.remove_watch_expression(session_id, expression, cx));
                task.await?;
                Ok(success(
                    operation,
                    "removed watch expression",
                    json!({ "session_id": session_id.0 }),
                ))
            }
            DebuggerOperation::ListModules => {
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                let task = cx.update(|cx| api.list_modules(session_id, cx));
                let data = task.await?;
                Ok(success(
                    operation,
                    "listed debug session modules",
                    modules_to_json(data),
                ))
            }
            DebuggerOperation::ListLoadedSources => {
                let api = cx.update(|cx| self.api(cx));
                let session_id =
                    cx.update(|cx| resolve_session_id(&self.project, &api, input.session_id, cx))?;
                let task = cx.update(|cx| api.list_loaded_sources(session_id, cx));
                let data = task.await?;
                Ok(success(
                    operation,
                    "listed debug session loaded sources",
                    loaded_sources_to_json(data),
                ))
            }
        }
    }

    fn ensure_write_mode(&self, operation: &str, cx: &gpui::AsyncApp) -> Result<()> {
        if cx.update(|cx| self.is_read_only_profile(cx)) {
            anyhow::bail!(
                "debugger.{operation} is not available in read-only modes. Switch to Execute or Debug mode to start sessions, change breakpoints, or control execution."
            );
        }
        Ok(())
    }

    async fn resolve_control_input(
        &self,
        mut input: ControlInput,
        cx: &mut gpui::AsyncApp,
    ) -> Result<ResolvedControlInput> {
        if input.action == ControlAction::RunToLine {
            let path = input
                .path
                .take()
                .context("path is required for debugger control run_to_line")?;
            input.path = Some(resolve_debugger_path(&self.project, path, cx)?);
            input
                .line
                .context("line is required for debugger control run_to_line")?;
        }

        let (api, session_id) = cx.update(|cx| {
            let api = self.api(cx);
            let session_id = resolve_session_id(&self.project, &api, input.session_id, cx)?;
            anyhow::Ok((api, session_id))
        })?;

        let thread_id = if input.action.requires_thread() {
            match input.thread_id.map(project::debugger::session::ThreadId) {
                Some(thread_id) => Some(thread_id),
                None => Some(choose_thread_for_action(&api, session_id, input.action, cx).await?),
            }
        } else {
            None
        };

        Ok(ResolvedControlInput {
            session_id,
            thread_id,
            action: input.action,
            granularity: input.granularity,
            path: input.path,
            line: input.line,
            frame_id: input.frame_id,
            timeout_ms: input.timeout_ms,
        })
    }

    async fn run_control(
        &self,
        input: ResolvedControlInput,
        cx: &mut gpui::AsyncApp,
    ) -> Result<(SessionId, Option<AgentDebuggerControlResult>)> {
        let timeout = Duration::from_millis(control_timeout_ms(input.timeout_ms)?);
        let session_id = input.session_id;
        let api = cx.update(|cx| self.api(cx));
        let granularity = input.granularity.unwrap_or(SteppingGranularity::Line);

        let result = match input.action {
            ControlAction::Continue => {
                let thread_id = input
                    .thread_id
                    .context("thread_id is required for debugger control continue")?;
                let task = cx.update(|cx| api.continue_thread(session_id, thread_id, timeout, cx));
                Some(task.await?)
            }
            ControlAction::Pause => {
                let thread_id = input
                    .thread_id
                    .context("thread_id is required for debugger control pause")?;
                let task = cx.update(|cx| api.pause_thread(session_id, thread_id, timeout, cx));
                Some(task.await?)
            }
            ControlAction::StepOver => {
                let thread_id = input
                    .thread_id
                    .context("thread_id is required for debugger control step_over")?;
                let task = cx.update(|cx| {
                    api.step_thread(
                        session_id,
                        thread_id,
                        AgentDebuggerStepKind::Over,
                        granularity,
                        timeout,
                        cx,
                    )
                });
                Some(task.await?)
            }
            ControlAction::StepIn => {
                let thread_id = input
                    .thread_id
                    .context("thread_id is required for debugger control step_in")?;
                let task = cx.update(|cx| {
                    api.step_thread(
                        session_id,
                        thread_id,
                        AgentDebuggerStepKind::In,
                        granularity,
                        timeout,
                        cx,
                    )
                });
                Some(task.await?)
            }
            ControlAction::StepOut => {
                let thread_id = input
                    .thread_id
                    .context("thread_id is required for debugger control step_out")?;
                let task = cx.update(|cx| {
                    api.step_thread(
                        session_id,
                        thread_id,
                        AgentDebuggerStepKind::Out,
                        granularity,
                        timeout,
                        cx,
                    )
                });
                Some(task.await?)
            }
            ControlAction::StepBack => {
                let thread_id = input
                    .thread_id
                    .context("thread_id is required for debugger control step_back")?;
                let task = cx.update(|cx| {
                    api.step_thread(
                        session_id,
                        thread_id,
                        AgentDebuggerStepKind::Back,
                        granularity,
                        timeout,
                        cx,
                    )
                });
                Some(task.await?)
            }
            ControlAction::RunToLine => {
                let thread_id = input
                    .thread_id
                    .context("thread_id is required for debugger control run_to_line")?;
                let path = input
                    .path
                    .context("path is required for debugger control run_to_line")?;
                let line = input
                    .line
                    .context("line is required for debugger control run_to_line")?;
                let task =
                    cx.update(|cx| api.run_to_line(session_id, thread_id, path, line, timeout, cx));
                Some(task.await?)
            }
            ControlAction::Detach => {
                let task = cx.update(|cx| api.detach_session(session_id, cx));
                task.await?;
                None
            }
            ControlAction::Restart => {
                let task = self.environment.restart_session(session_id.to_proto(), cx);
                task.await?;
                None
            }
            ControlAction::RestartFrame => {
                let frame_id = input
                    .frame_id
                    .context("frame_id is required for debugger control restart_frame")?;
                let task = cx.update(|cx| api.restart_stack_frame(session_id, frame_id, cx));
                task.await?;
                None
            }
        };

        Ok((session_id, result))
    }
}

impl BreakpointInput {
    fn into_agent_input(self) -> AgentSourceBreakpointInput {
        AgentSourceBreakpointInput {
            path: self.path,
            line: self.line,
            enabled: self.enabled,
            condition: self.condition,
            hit_condition: self.hit_condition,
            log_message: self.log_message,
        }
    }
}

impl From<ExceptionBreakpointInput> for AgentExceptionBreakpointInput {
    fn from(input: ExceptionBreakpointInput) -> Self {
        Self {
            id: input.id,
            enabled: input.enabled,
        }
    }
}

impl From<DataBreakpointInput> for AgentDataBreakpointInput {
    fn from(input: DataBreakpointInput) -> Self {
        Self {
            variables_reference: input.variables_reference,
            name: input.name,
            access_type: input.access_type,
            condition: input.condition,
            hit_condition: input.hit_condition,
        }
    }
}

struct ResolvedControlInput {
    session_id: SessionId,
    thread_id: Option<project::debugger::session::ThreadId>,
    action: ControlAction,
    granularity: Option<SteppingGranularity>,
    path: Option<PathBuf>,
    line: Option<u32>,
    frame_id: Option<u64>,
    timeout_ms: Option<u64>,
}

impl ControlAction {
    fn label(self) -> &'static str {
        match self {
            ControlAction::Continue => "continue",
            ControlAction::Pause => "pause",
            ControlAction::StepOver => "step over",
            ControlAction::StepIn => "step in",
            ControlAction::StepOut => "step out",
            ControlAction::StepBack => "step back",
            ControlAction::RunToLine => "run to line",
            ControlAction::Detach => "detach",
            ControlAction::Restart => "restart",
            ControlAction::RestartFrame => "restart frame",
        }
    }

    fn permission_name(self) -> &'static str {
        match self {
            ControlAction::Continue => "continue",
            ControlAction::Pause => "pause",
            ControlAction::StepOver => "step_over",
            ControlAction::StepIn => "step_in",
            ControlAction::StepOut => "step_out",
            ControlAction::StepBack => "step_back",
            ControlAction::RunToLine => "run_to_line",
            ControlAction::Detach => "detach",
            ControlAction::Restart => "restart",
            ControlAction::RestartFrame => "restart_frame",
        }
    }

    fn requires_thread(self) -> bool {
        matches!(
            self,
            ControlAction::Continue
                | ControlAction::Pause
                | ControlAction::StepOver
                | ControlAction::StepIn
                | ControlAction::StepOut
                | ControlAction::StepBack
                | ControlAction::RunToLine
        )
    }
}

fn default_true() -> bool {
    true
}

fn success(operation: String, message: impl Into<String>, data: Value) -> DebuggerToolOutput {
    DebuggerToolOutput::Success {
        operation,
        message: message.into(),
        data,
    }
}

async fn authorize_debugger_operation(
    event_stream: &ToolCallEventStream,
    title: impl Into<String>,
    input_values: Vec<String>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let title = title.into();
    let task = cx.update(|cx| {
        event_stream.authorize(
            title,
            ToolPermissionContext::new(DebuggerTool::NAME, input_values),
            cx,
        )
    });
    task.await
}

fn permission_inputs(operation: &str, values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut inputs = values.into_iter().collect::<Vec<_>>();
    if inputs.is_empty() {
        inputs.push(operation.to_string());
    } else {
        for input in &mut inputs {
            *input = format!("{operation} {input}");
        }
    }
    inputs
}

fn breakpoint_permission_inputs<'a>(
    operation: &str,
    breakpoints: impl IntoIterator<Item = &'a BreakpointInput>,
) -> Result<Vec<String>> {
    let inputs = breakpoints
        .into_iter()
        .map(|breakpoint| {
            let condition =
                permission_value_to_string(&breakpoint.condition, "breakpoint condition")?;
            let hit_condition =
                permission_value_to_string(&breakpoint.hit_condition, "breakpoint hit condition")?;
            let log_message =
                permission_value_to_string(&breakpoint.log_message, "breakpoint log message")?;
            anyhow::Ok(format!(
                "path:{} line:{} enabled:{} condition:{} hit_condition:{} log_message:{}",
                breakpoint.path.display(),
                breakpoint.line,
                breakpoint.enabled,
                condition,
                hit_condition,
                log_message
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(permission_inputs(operation, inputs))
}

fn breakpoint_location_permission_inputs<'a>(
    operation: &str,
    breakpoints: impl IntoIterator<Item = &'a BreakpointLocationInput>,
) -> Vec<String> {
    permission_inputs(
        operation,
        breakpoints.into_iter().map(|breakpoint| {
            format!(
                "path:{} line:{}",
                breakpoint.path.display(),
                breakpoint.line
            )
        }),
    )
}

fn exception_breakpoint_permission_inputs(
    operation: &str,
    breakpoints: &[ExceptionBreakpointInput],
    session_id: SessionId,
) -> Vec<String> {
    permission_inputs(
        operation,
        breakpoints.iter().map(|breakpoint| {
            format!(
                "session_id:{} id:{} enabled:{}",
                session_id.0, breakpoint.id, breakpoint.enabled
            )
        }),
    )
}

fn data_breakpoint_permission_inputs(
    operation: &str,
    breakpoints: &[DataBreakpointInput],
    session_id: SessionId,
) -> Vec<String> {
    permission_inputs(
        operation,
        breakpoints.iter().map(|breakpoint| {
            format!(
                "session_id:{} variables_reference:{} name:{} access_type:{} condition:{} hit_condition:{}",
                session_id.0,
                breakpoint.variables_reference,
                breakpoint.name,
                breakpoint.access_type.as_deref().unwrap_or("none"),
                breakpoint.condition.as_deref().unwrap_or("none"),
                breakpoint.hit_condition.as_deref().unwrap_or("none"),
            )
        }),
    )
}

fn control_permission_input(input: &ResolvedControlInput) -> String {
    let thread_id = input
        .thread_id
        .map(|thread_id| thread_id.0.to_string())
        .unwrap_or_else(|| "none".to_string());
    let mut value = format!(
        "action:{} session_id:{} thread_id:{}",
        input.action.permission_name(),
        input.session_id.0,
        thread_id
    );

    if input.action == ControlAction::RunToLine {
        let path = input
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "missing".to_string());
        let line = input
            .line
            .map(|line| line.to_string())
            .unwrap_or_else(|| "missing".to_string());
        value.push_str(&format!(" path:{path} line:{line}"));
    }

    if input.action == ControlAction::RestartFrame {
        let frame_id = input
            .frame_id
            .map(|frame_id| frame_id.to_string())
            .unwrap_or_else(|| "missing".to_string());
        value.push_str(&format!(" frame_id:{frame_id}"));
    }

    value
}

fn evaluate_permission_inputs(
    operation: &str,
    session_id: SessionId,
    expression: &str,
    frame_id: Option<u64>,
) -> Result<Vec<String>> {
    let expression = permission_value_to_string(&expression, "evaluate expression")?;
    let frame_id = frame_id
        .map(|frame_id| frame_id.to_string())
        .unwrap_or_else(|| "none".to_string());
    Ok(permission_inputs(
        operation,
        [format!(
            "session_id:{} expression:{} frame_id:{}",
            session_id.0, expression, frame_id
        )],
    ))
}

fn set_variable_permission_inputs(
    operation: &str,
    session_id: SessionId,
    name: &str,
    value: &str,
) -> Result<Vec<String>> {
    let name = permission_value_to_string(&name, "set_variable name")?;
    let value = permission_value_to_string(&value, "set_variable value")?;
    Ok(permission_inputs(
        operation,
        [format!(
            "session_id:{} name:{} value:{}",
            session_id.0, name, value
        )],
    ))
}

fn read_memory_permission_inputs(
    operation: &str,
    session_id: SessionId,
    memory_reference: &str,
    offset: Option<u64>,
    count: u64,
) -> Vec<String> {
    let offset = offset
        .map(|offset| offset.to_string())
        .unwrap_or_else(|| "none".to_string());
    permission_inputs(
        operation,
        [format!(
            "session_id:{} memory_reference:{} offset:{} count:{}",
            session_id.0, memory_reference, offset, count
        )],
    )
}

fn select_history_permission_inputs(
    operation: &str,
    session_id: SessionId,
    index: usize,
) -> Vec<String> {
    permission_inputs(
        operation,
        [format!("session_id:{} index:{}", session_id.0, index)],
    )
}

fn add_watch_expression_permission_inputs(
    operation: &str,
    session_id: SessionId,
    expression: &str,
    frame_id: Option<u64>,
) -> Result<Vec<String>> {
    let expression = permission_value_to_string(&expression, "add_watch_expression expression")?;
    let frame_id = frame_id
        .map(|frame_id| frame_id.to_string())
        .unwrap_or_else(|| "none".to_string());
    Ok(permission_inputs(
        operation,
        [format!(
            "session_id:{} expression:{} frame_id:{}",
            session_id.0, expression, frame_id
        )],
    ))
}

fn remove_watch_expression_permission_inputs(
    operation: &str,
    session_id: SessionId,
    expression: &str,
) -> Result<Vec<String>> {
    let expression = permission_value_to_string(&expression, "remove_watch_expression expression")?;
    Ok(permission_inputs(
        operation,
        [format!(
            "session_id:{} expression:{}",
            session_id.0, expression
        )],
    ))
}

#[cfg(test)]
pub fn control_permission_inputs_for_test(
    operation: &str,
    input: ControlInput,
    resolved_session_id: u64,
    resolved_thread_id: i64,
) -> Vec<String> {
    permission_inputs(
        operation,
        [control_permission_input(&ResolvedControlInput {
            session_id: SessionId::from_proto(resolved_session_id),
            thread_id: Some(project::debugger::session::ThreadId(resolved_thread_id)),
            action: input.action,
            granularity: input.granularity,
            path: input.path,
            line: input.line,
            frame_id: input.frame_id,
            timeout_ms: input.timeout_ms,
        })],
    )
}

/// Launch with `stopOnEntry` so a fast-running debuggee doesn't exit (and tear
/// down the session) before the agent can set breakpoints. The agent sets
/// breakpoints *after* starting the session, so without this the program runs
/// to completion and the session is dropped from the DAP store.
fn ensure_stop_on_entry(scenario: &mut DebugScenario) {
    let Value::Object(config) = &mut scenario.config else {
        return;
    };
    let is_attach = config
        .get("request")
        .and_then(Value::as_str)
        .is_some_and(|request| request == "attach");
    if !is_attach && !config.contains_key("stopOnEntry") {
        config.insert("stopOnEntry".to_string(), Value::Bool(true));
    }
}

/// The tool documents launch-config fields spread at the scenario top level
/// (`scenario.config` is serde(flatten)), but models sometimes nest them under
/// a `"config"` key, which arrives as `{"config": {...}}` inside the flattened
/// config and is rejected by the debugger panel. Unwrap the nested form so both
/// shapes start the same way.
fn normalize_scenario_config(scenario: &mut DebugScenario) {
    if scenario.config.get("config").is_some_and(Value::is_object)
        && scenario.config.get("request").is_none()
    {
        scenario.config = scenario.config.get("config").cloned().unwrap_or_default();
    }
}

/// The `console` value that routes an adapter's debuggee stdio to the debug
/// console (DAP output events) instead of an integrated terminal, so agent
/// snapshots can include program output. Centralizes the per-adapter console
/// quirk in one place; `None` means the adapter routes output itself.
fn adapter_console_value(adapter: &str) -> Option<&'static str> {
    // Both adapters use `internalConsole`: Debugpy defaults stdio to a
    // terminal, and CodeLLDB rejects unknown `console` variants (its serde
    // enum is case-sensitive camelCase).
    match adapter {
        "Debugpy" | "CodeLLDB" => Some("internalConsole"),
        _ => None,
    }
}

/// Route the debuggee's stdio to the debug console rather than an integrated
/// terminal. Only overrides adapters whose launch configs default stdio to a
/// terminal; an explicit `console`/`terminal`/`stdio` in the config is left
/// untouched.
fn ensure_console_output(scenario: &mut DebugScenario) {
    let Some(console_value) = adapter_console_value(scenario.adapter.as_ref()) else {
        return;
    };
    let Value::Object(config) = &mut scenario.config else {
        return;
    };
    if !config.contains_key("console")
        && !config.contains_key("terminal")
        && !config.contains_key("stdio")
    {
        config.insert("console".to_string(), Value::String(console_value.into()));
    }
}

/// `start_debug_session` registers the session synchronously, but adapter boot
/// and scenario validation finish asynchronously — a rejected config or a
/// failed launch removes the session moments later. Wait until the session is
/// actually alive so callers don't get an id for a session that never booted.
async fn await_session_boot(
    api: AgentDebuggerApi,
    session_id: u64,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let deadline = std::time::Instant::now() + SESSION_BOOT_TIMEOUT;
    loop {
        let sessions = cx.update(|cx| api.list_sessions(cx));
        let session = sessions
            .iter()
            .find(|session| session.session_id.to_proto() == session_id);
        let child_booted = sessions
            .iter()
            .any(|session| session.parent_session_id.map(|id| id.to_proto()) == Some(session_id));

        match session {
            Some(session) if session.status != AgentDebuggerSessionStatus::Booting => return Ok(()),
            Some(_) => {}
            None if child_booted => return Ok(()),
            None => anyhow::bail!(
                "debug session {session_id} failed to start and was removed (the adapter rejected the config or failed to launch — see Zed.log for the adapter error)"
            ),
        }

        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "debug session {session_id} was still booting after {SESSION_BOOT_TIMEOUT:?}"
            );
        }
        cx.background_executor()
            .timer(Duration::from_millis(SESSION_BOOT_POLL_INTERVAL_MS))
            .await;
    }
}

fn start_session_permission_inputs(
    operation: &str,
    input: &StartSessionInput,
) -> Result<Vec<String>> {
    let scenario = &input.scenario;
    let worktree_id = input
        .worktree_id
        .map(|worktree_id| worktree_id.to_string())
        .unwrap_or_else(|| "none".to_string());
    let build_task = scenario
        .build
        .as_ref()
        .map(|build| permission_value_to_string(build, "start_session build"))
        .transpose()?
        .unwrap_or_else(|| "null".to_string());
    let tcp_connection = scenario
        .tcp_connection
        .as_ref()
        .map(|tcp_connection| {
            permission_value_to_string(tcp_connection, "start_session tcp_connection")
        })
        .transpose()?
        .unwrap_or_else(|| "null".to_string());
    let config = permission_value_to_string(&scenario.config, "start_session config")?;

    Ok(permission_inputs(
        operation,
        [format!(
            "adapter:{} label:{} worktree_id:{} build_task:{} tcp_connection:{} config:{}",
            scenario.adapter, scenario.label, worktree_id, build_task, tcp_connection, config
        )],
    ))
}

fn permission_value_to_string(value: &impl Serialize, value_name: &str) -> Result<String> {
    let value = serde_json::to_value(value).with_context(|| {
        format!("Failed to serialize debugger {value_name} for permission matching")
    })?;
    serde_json::to_string(&sort_json_value(value)).with_context(|| {
        format!("Failed to serialize debugger {value_name} JSON for permission matching")
    })
}

fn sort_json_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(sort_json_value).collect()),
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|(left_key, _), (right_key, _)| left_key.cmp(right_key));

            let mut sorted = serde_json::Map::new();
            for (key, value) in entries {
                sorted.insert(key, sort_json_value(value));
            }
            Value::Object(sorted)
        }
        value => value,
    }
}

fn operation_name(input: &DebuggerToolInput) -> &'static str {
    match input.operation {
        DebuggerOperation::ListSessions => "list_sessions",
        DebuggerOperation::Snapshot => "snapshot",
        DebuggerOperation::ListBreakpoints => "list_breakpoints",
        DebuggerOperation::SetBreakpoints => "set_breakpoints",
        DebuggerOperation::RemoveBreakpoints => "remove_breakpoints",
        DebuggerOperation::Control => "control",
        DebuggerOperation::ListAdapters => "list_adapters",
        DebuggerOperation::StartSession => "start_session",
        DebuggerOperation::StopSession => "stop_session",
        DebuggerOperation::Evaluate => "evaluate",
        DebuggerOperation::SetVariable => "set_variable",
        DebuggerOperation::ListExceptionBreakpoints => "list_exception_breakpoints",
        DebuggerOperation::SetExceptionBreakpoints => "set_exception_breakpoints",
        DebuggerOperation::ListDataBreakpoints => "list_data_breakpoints",
        DebuggerOperation::SetDataBreakpoints => "set_data_breakpoints",
        DebuggerOperation::SetIgnoreBreakpoints => "set_ignore_breakpoints",
        DebuggerOperation::ClearBreakpoints => "clear_breakpoints",
        DebuggerOperation::ReadMemory => "read_memory",
        DebuggerOperation::ListHistory => "list_history",
        DebuggerOperation::SelectHistory => "select_history",
        DebuggerOperation::ListWatchExpressions => "list_watch_expressions",
        DebuggerOperation::AddWatchExpression => "add_watch_expression",
        DebuggerOperation::RemoveWatchExpression => "remove_watch_expression",
        DebuggerOperation::ListModules => "list_modules",
        DebuggerOperation::ListLoadedSources => "list_loaded_sources",
    }
}

fn initial_title_for_input(input: &DebuggerToolInput) -> SharedString {
    match input.operation {
        DebuggerOperation::ListSessions => "List debug sessions".into(),
        DebuggerOperation::Snapshot => input
            .session_id
            .map(|session_id| format!("Inspect debug session {session_id}").into())
            .unwrap_or_else(|| "Inspect debugger".into()),
        DebuggerOperation::ListBreakpoints => "List debugger breakpoints".into(),
        DebuggerOperation::SetBreakpoints => {
            let breakpoints = input.breakpoints.as_deref().unwrap_or_default();
            if breakpoints.len() == 1 {
                let breakpoint = &breakpoints[0];
                format!(
                    "Set debugger breakpoint at {}:{}",
                    MarkdownInlineCode(&breakpoint.path.to_string_lossy()),
                    breakpoint.line
                )
                .into()
            } else {
                format!("Set {} debugger breakpoints", breakpoints.len()).into()
            }
        }
        DebuggerOperation::RemoveBreakpoints => {
            let breakpoints = input.breakpoints.as_deref().unwrap_or_default();
            if breakpoints.len() == 1 {
                let breakpoint = &breakpoints[0];
                format!(
                    "Remove debugger breakpoint at {}:{}",
                    MarkdownInlineCode(&breakpoint.path.to_string_lossy()),
                    breakpoint.line
                )
                .into()
            } else {
                format!("Remove {} debugger breakpoints", breakpoints.len()).into()
            }
        }
        DebuggerOperation::Control => match input.action {
            Some(ControlAction::RunToLine) => match (input.path.as_deref(), input.line) {
                (Some(path), Some(line)) => format!(
                    "Debugger run to line at {}:{}",
                    MarkdownInlineCode(&path.to_string_lossy()),
                    line
                )
                .into(),
                _ => "Debugger run to line".into(),
            },
            Some(action) => format!("Debugger {}", action.label()).into(),
            None => "Debugger control".into(),
        },
        DebuggerOperation::ListAdapters => "List debug adapters".into(),
        DebuggerOperation::StartSession => input
            .scenario
            .as_ref()
            .map(|scenario| {
                format!(
                    "Start debug session {}",
                    MarkdownInlineCode(&scenario.label)
                )
                .into()
            })
            .unwrap_or_else(|| "Start debug session".into()),
        DebuggerOperation::StopSession => input
            .session_id
            .map(|session_id| format!("Stop debug session {session_id}").into())
            .unwrap_or_else(|| "Stop debug session".into()),
        DebuggerOperation::Evaluate => input
            .expression
            .as_ref()
            .map(|expression| {
                format!(
                    "Evaluate debugger expression {}",
                    MarkdownInlineCode(expression)
                )
                .into()
            })
            .unwrap_or_else(|| "Evaluate debugger expression".into()),
        DebuggerOperation::SetVariable => input
            .name
            .as_ref()
            .map(|name| format!("Set debugger variable {}", MarkdownInlineCode(name)).into())
            .unwrap_or_else(|| "Set debugger variable".into()),
        DebuggerOperation::ListExceptionBreakpoints => "List debugger exception breakpoints".into(),
        DebuggerOperation::SetExceptionBreakpoints => {
            let count = input
                .exception_breakpoints
                .as_deref()
                .map_or(0, |b| b.len());
            format!("Set debugger exception breakpoint filters ({count})").into()
        }
        DebuggerOperation::ListDataBreakpoints => "List debugger data breakpoints".into(),
        DebuggerOperation::SetDataBreakpoints => {
            let count = input.data_breakpoints.as_deref().map_or(0, |b| b.len());
            format!("Set debugger data breakpoints ({count})").into()
        }
        DebuggerOperation::SetIgnoreBreakpoints => match input.ignore {
            Some(true) => "Ignore all debugger breakpoints".into(),
            Some(false) => "Stop ignoring debugger breakpoints".into(),
            None => "Set debugger ignore breakpoints".into(),
        },
        DebuggerOperation::ClearBreakpoints => "Clear all debugger breakpoints".into(),
        DebuggerOperation::ReadMemory => input
            .memory_reference
            .as_ref()
            .map(|memory_reference| {
                format!(
                    "Read debugger memory {}",
                    MarkdownInlineCode(memory_reference)
                )
                .into()
            })
            .unwrap_or_else(|| "Read debugger memory".into()),
        DebuggerOperation::ListHistory => "List debugger history".into(),
        DebuggerOperation::SelectHistory => input
            .index
            .map(|index| format!("Select debugger history snapshot {index}").into())
            .unwrap_or_else(|| "Select debugger history snapshot".into()),
        DebuggerOperation::ListWatchExpressions => "List debugger watch expressions".into(),
        DebuggerOperation::AddWatchExpression => input
            .expression
            .as_ref()
            .map(|expression| {
                format!(
                    "Add debugger watch expression {}",
                    MarkdownInlineCode(expression)
                )
                .into()
            })
            .unwrap_or_else(|| "Add debugger watch expression".into()),
        DebuggerOperation::RemoveWatchExpression => input
            .expression
            .as_ref()
            .map(|expression| {
                format!(
                    "Remove debugger watch expression {}",
                    MarkdownInlineCode(expression)
                )
                .into()
            })
            .unwrap_or_else(|| "Remove debugger watch expression".into()),
        DebuggerOperation::ListModules => "List debugger modules".into(),
        DebuggerOperation::ListLoadedSources => "List debugger loaded sources".into(),
    }
}

fn limits_from_input(input: Option<SnapshotLimitsInput>) -> Result<AgentDebuggerSnapshotLimits> {
    let mut limits = AgentDebuggerSnapshotLimits::default();
    if let Some(input) = input {
        if let Some(value) = input.max_frames {
            limits.max_frames = validate_snapshot_limit("max_frames", value, MAX_SNAPSHOT_FRAMES)?;
        }
        if let Some(value) = input.max_variables_per_scope {
            limits.max_variables_per_scope = validate_snapshot_limit(
                "max_variables_per_scope",
                value,
                MAX_SNAPSHOT_VARIABLES_PER_SCOPE,
            )?;
        }
        if let Some(value) = input.max_variable_value_length {
            limits.max_variable_value_length = validate_snapshot_limit(
                "max_variable_value_length",
                value,
                MAX_SNAPSHOT_VARIABLE_VALUE_LENGTH,
            )?;
        }
        if let Some(value) = input.max_output_events {
            limits.max_output_events =
                validate_snapshot_limit("max_output_events", value, MAX_SNAPSHOT_OUTPUT_EVENTS)?;
        }
        if let Some(value) = input.max_output_bytes {
            limits.max_output_bytes =
                validate_snapshot_limit("max_output_bytes", value, MAX_SNAPSHOT_OUTPUT_BYTES)?;
        }
        if let Some(value) = input.max_source_context_lines {
            limits.max_source_context_lines = validate_snapshot_limit(
                "max_source_context_lines",
                value,
                MAX_SNAPSHOT_SOURCE_CONTEXT_LINES,
            )?;
        }
    }
    Ok(limits)
}

fn validate_snapshot_limit(field_name: &str, value: usize, maximum: usize) -> Result<usize> {
    if value > maximum {
        anyhow::bail!(
            "debugger snapshot limit `{field_name}` must be at most {maximum}, got {value}"
        );
    }
    Ok(value)
}

fn validate_control_timeout(timeout_ms: Option<u64>) -> Result<()> {
    control_timeout_ms(timeout_ms).map(|_| ())
}

fn control_timeout_ms(timeout_ms: Option<u64>) -> Result<u64> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_CONTROL_TIMEOUT_MS);
    if timeout_ms > MAX_CONTROL_TIMEOUT_MS {
        anyhow::bail!(
            "debugger control `timeout_ms` must be at most {MAX_CONTROL_TIMEOUT_MS}, got {timeout_ms}"
        );
    }
    Ok(timeout_ms)
}

fn thread_picker_limits() -> AgentDebuggerSnapshotLimits {
    AgentDebuggerSnapshotLimits {
        max_frames: 0,
        max_variables_per_scope: 0,
        max_variable_value_length: 0,
        max_output_events: 0,
        max_output_bytes: 0,
        max_source_context_lines: 0,
    }
}

fn resolve_breakpoint_input(
    project: &Entity<Project>,
    mut breakpoint: BreakpointInput,
    cx: &gpui::AsyncApp,
) -> Result<BreakpointInput> {
    breakpoint.path = resolve_debugger_path(project, breakpoint.path, cx)?;
    Ok(breakpoint)
}

fn resolve_breakpoint_location(
    project: &Entity<Project>,
    mut breakpoint: BreakpointLocationInput,
    cx: &gpui::AsyncApp,
) -> Result<BreakpointLocationInput> {
    breakpoint.path = resolve_debugger_path(project, breakpoint.path, cx)?;
    Ok(breakpoint)
}

fn resolve_debugger_path(
    project: &Entity<Project>,
    path: PathBuf,
    cx: &gpui::AsyncApp,
) -> Result<PathBuf> {
    if path.is_absolute() {
        return normalize_debugger_path(path);
    }

    project.read_with(cx, |project, cx| {
        let project_path = project.find_project_path(&path, cx).ok_or_else(|| {
            anyhow!(
                "Could not resolve debugger source path `{}` in this project",
                path.display()
            )
        })?;
        let worktree = project
            .worktree_for_id(project_path.worktree_id, cx)
            .with_context(|| format!("Could not find worktree for `{}`", path.display()))?;
        normalize_debugger_path(worktree.read(cx).absolutize(&project_path.path))
    })
}

fn normalize_debugger_path(path: PathBuf) -> Result<PathBuf> {
    normalize_lexically(&path).with_context(|| {
        format!(
            "Could not normalize debugger source path `{}`",
            path.display()
        )
    })
}

fn resolve_session_id(
    project: &Entity<Project>,
    api: &AgentDebuggerApi,
    session_id: Option<u64>,
    cx: &App,
) -> Result<SessionId> {
    if let Some(session_id) = session_id {
        return Ok(SessionId::from_proto(session_id));
    }
    if let Some((session, _)) = project.read(cx).active_debug_session(cx) {
        return Ok(session.read(cx).session_id());
    }
    api.list_sessions(cx)
        .first()
        .map(|session| session.session_id)
        .ok_or_else(|| anyhow!("No active debug sessions. Start a debug session first."))
}

async fn choose_thread_for_action(
    api: &AgentDebuggerApi,
    session_id: SessionId,
    action: ControlAction,
    cx: &mut gpui::AsyncApp,
) -> Result<project::debugger::session::ThreadId> {
    let preferred_status = match action {
        ControlAction::Pause => AgentDebuggerThreadStatus::Running,
        ControlAction::Continue
        | ControlAction::StepOver
        | ControlAction::StepIn
        | ControlAction::StepOut
        | ControlAction::StepBack
        | ControlAction::RunToLine
        // These never reach the thread picker (`requires_thread` is false), but
        // the match must stay exhaustive.
        | ControlAction::Detach
        | ControlAction::Restart
        | ControlAction::RestartFrame => AgentDebuggerThreadStatus::Stopped,
    };

    // The session state machine races adapter boot: right after
    // `start_session` the adapter may not have reported threads yet, and for
    // launches with `stopOnEntry` the first `stopped` event can land a
    // moment after the session is listed as started. Poll briefly so the
    // first control operation doesn't fail spuriously (a hard error is only
    // correct once the session has had a chance to settle).
    let mut attempts = 0u32;
    // Allow the adapter up to ~30s to surface threads before giving up — the
    // first launch of an auto-downloaded adapter (e.g. vscode-js-debug) can take
    // tens of seconds to boot — and a shorter grace window after the session
    // settles so the first `stopped` event of a stopOnEntry launch isn't raced.
    let max_attempts = 300;
    let grace_attempts = 5;
    let poll_interval = std::time::Duration::from_millis(100);
    loop {
        let snapshot_task =
            cx.update(|cx| api.snapshot(session_id, thread_picker_limits(), None, cx));
        let snapshot = snapshot_task.await?;

        if let Some(thread) = snapshot
            .threads
            .iter()
            .find(|thread| thread.status == preferred_status)
        {
            return Ok(thread.thread_id);
        }

        let settled = !snapshot.session.status.is_booting();
        let has_threads = !snapshot.threads.is_empty();
        if attempts >= max_attempts || (settled && has_threads && attempts >= grace_attempts) {
            return match action {
                ControlAction::Pause => {
                    if has_threads {
                        // Some adapters accept a pause-by-thread-id even when no
                        // thread is currently running, so fall back to the
                        // first thread.
                        Ok(snapshot.threads[0].thread_id)
                    } else {
                        Err(anyhow!(
                            "No debugger threads available in session {:?}. The session must be running before it can be paused.",
                            session_id
                        ))
                    }
                }
                ControlAction::Continue
                | ControlAction::StepOver
                | ControlAction::StepIn
                | ControlAction::StepOut
                | ControlAction::StepBack
                | ControlAction::RunToLine
                | ControlAction::Detach
                | ControlAction::Restart
                | ControlAction::RestartFrame => {
                    if has_threads {
                        Err(anyhow!(
                            "No stopped debugger thread is available in session {:?}. The debugger must be paused at a breakpoint before this action can run; pause the session or wait for a breakpoint to hit.",
                            session_id
                        ))
                    } else {
                        Err(anyhow!(
                            "No debugger threads available in session {:?}. Inspect a snapshot to confirm the session is still running.",
                            session_id
                        ))
                    }
                }
            };
        }

        attempts += 1;
        cx.background_executor().timer(poll_interval).await;
    }
}

fn sessions_to_json(sessions: Vec<AgentDebuggerSession>) -> Value {
    Value::Array(sessions.into_iter().map(session_to_json).collect())
}

fn session_to_json(session: AgentDebuggerSession) -> Value {
    json!({
        "session_id": session.session_id.0,
        "parent_session_id": session.parent_session_id.map(|id| id.0),
        "child_session_ids": session.child_session_ids.into_iter().map(|id| id.0).collect::<Vec<_>>(),
        "label": session.label,
        "adapter": session.adapter,
        "status": format!("{:?}", session.status).to_lowercase(),
        "is_attached": session.is_attached,
        "has_ever_stopped": session.has_ever_stopped,
    })
}

fn breakpoints_to_json(breakpoints: Vec<AgentSourceBreakpoint>) -> Value {
    Value::Array(
        breakpoints
            .into_iter()
            .map(|breakpoint| {
                json!({
                    "path": breakpoint.path,
                    "line": breakpoint.line,
                    "enabled": breakpoint.enabled,
                    "condition": breakpoint.condition,
                    "hit_condition": breakpoint.hit_condition,
                    "log_message": breakpoint.log_message,
                })
            })
            .collect(),
    )
}

fn breakpoint_edit_result_to_json(result: AgentBreakpointEditResult) -> Value {
    json!({
        "path": result.path,
        "line": result.line,
        "changed": result.changed,
    })
}

fn exception_breakpoints_to_json(breakpoints: Vec<AgentExceptionBreakpoint>) -> Value {
    Value::Array(
        breakpoints
            .into_iter()
            .map(|breakpoint| {
                json!({
                    "id": breakpoint.id,
                    "label": breakpoint.label,
                    "description": breakpoint.description,
                    "default": breakpoint.default,
                    "enabled": breakpoint.enabled,
                })
            })
            .collect(),
    )
}

fn data_breakpoints_to_json(breakpoints: Vec<AgentDataBreakpoint>) -> Value {
    Value::Array(
        breakpoints
            .into_iter()
            .map(|breakpoint| {
                json!({
                    "data_id": breakpoint.data_id,
                    "context": breakpoint.context,
                    "enabled": breakpoint.enabled,
                    "access_type": breakpoint.access_type,
                    "condition": breakpoint.condition,
                    "hit_condition": breakpoint.hit_condition,
                })
            })
            .collect(),
    )
}

fn control_result_to_json(result: AgentDebuggerControlResult) -> Value {
    json!({
        "status": format!("{:?}", result.status).to_lowercase(),
        "stopped_thread_id": result.stopped_thread_id.map(|thread_id| thread_id.0),
        "notes": result.notes,
    })
}

fn evaluate_result_to_json(result: AgentDebuggerEvaluateResult) -> Value {
    json!({
        "result": result.result,
        "type": result.type_name,
        "variables_reference": result.variables_reference,
    })
}

fn set_variable_result_to_json(result: AgentDebuggerSetVariableResult) -> Value {
    json!({
        "value": result.value,
        "type": result.type_name,
        "variables_reference": result.variables_reference,
    })
}

fn memory_read_to_json(read: AgentMemoryRead) -> Value {
    json!({
        "address": read.address,
        "unreadable_bytes": read.unreadable_bytes,
        "content": read.content,
        "byte_count": read.byte_count,
    })
}

fn history_to_json(entries: Vec<AgentDebuggerHistoryEntry>) -> Value {
    Value::Array(
        entries
            .into_iter()
            .map(|entry| {
                json!({
                    "index": entry.index,
                    "thread_count": entry.thread_count,
                })
            })
            .collect(),
    )
}

fn watch_expressions_to_json(expressions: Vec<AgentWatchExpression>) -> Value {
    Value::Array(
        expressions
            .into_iter()
            .map(|expression| {
                json!({
                    "expression": expression.expression,
                    "value": expression.value,
                    "variables_reference": expression.variables_reference,
                })
            })
            .collect(),
    )
}

fn modules_to_json(modules: Vec<AgentModule>) -> Value {
    Value::Array(
        modules
            .into_iter()
            .map(|module| {
                json!({
                    "id": module.id,
                    "name": module.name,
                    "path": module.path,
                    "symbol_status": module.symbol_status,
                    "symbol_file_path": module.symbol_file_path,
                    "version": module.version,
                    "is_optimized": module.is_optimized,
                })
            })
            .collect(),
    )
}

fn loaded_sources_to_json(sources: Vec<AgentLoadedSource>) -> Value {
    Value::Array(
        sources
            .into_iter()
            .map(|source| {
                json!({
                    "name": source.name,
                    "path": source.path,
                    "source_reference": source.source_reference,
                })
            })
            .collect(),
    )
}

fn snapshot_to_json(snapshot: AgentDebuggerSnapshot) -> Value {
    json!({
        "session": session_to_json(snapshot.session),
        "threads": snapshot.threads.into_iter().map(thread_to_json).collect::<Vec<_>>(),
        "output": snapshot.output.into_iter().map(output_to_json).collect::<Vec<_>>(),
        "notes": snapshot.notes,
    })
}

fn thread_to_json(thread: AgentDebuggerThread) -> Value {
    json!({
        "thread_id": thread.thread_id.0,
        "name": thread.name,
        "status": format!("{:?}", thread.status).to_lowercase(),
        "frames": thread.frames.into_iter().map(frame_to_json).collect::<Vec<_>>(),
    })
}

fn frame_to_json(frame: AgentDebuggerStackFrame) -> Value {
    json!({
        "frame_id": frame.frame_id,
        "name": frame.name,
        "source_path": frame.source_path,
        "line": frame.line,
        "column": frame.column,
        "scopes": frame.scopes.into_iter().map(scope_to_json).collect::<Vec<_>>(),
        "source_context": frame.source_context.map(source_context_to_json),
    })
}

fn scope_to_json(scope: AgentDebuggerScope) -> Value {
    json!({
        "name": scope.name,
        "expensive": scope.expensive,
        "variables_reference": scope.variables_reference,
        "variables_truncated": scope.variables_truncated,
        "variables": scope.variables.into_iter().map(variable_to_json).collect::<Vec<_>>(),
    })
}

fn variable_to_json(variable: AgentDebuggerVariable) -> Value {
    json!({
        "name": variable.name,
        "value": variable.value,
        "type": variable.type_name,
        "variables_reference": variable.variables_reference,
        "named_variables": variable.named_variables,
        "indexed_variables": variable.indexed_variables,
        "value_truncated": variable.value_truncated,
    })
}

fn source_context_to_json(context: AgentSourceContext) -> Value {
    json!({
        "start_line": context.start_line,
        "truncated_before": context.truncated_before,
        "truncated_after": context.truncated_after,
        "lines": context.lines.into_iter().map(|line| {
            json!({
                "line": line.line,
                "text": line.text,
            })
        }).collect::<Vec<_>>(),
    })
}

fn output_to_json(output: AgentDebuggerOutputEvent) -> Value {
    json!({
        "category": output.category,
        "output": output.output,
        "output_truncated": output.output_truncated,
        "source_path": output.source_path,
        "line": output.line,
        "column": output.column,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use task::TcpArgumentsTemplate;

    fn scenario(adapter: &str, config: Value) -> DebugScenario {
        DebugScenario {
            adapter: adapter.into(),
            label: "test".into(),
            build: None,
            config,
            tcp_connection: None::<TcpArgumentsTemplate>,
        }
    }

    #[test]
    fn ensure_console_output_uses_values_every_adapter_accepts() {
        // Regression: CodeLLDB was given `"console": "console"`, which its
        // launch-config parser rejects with `unknown variant 'console'`,
        // killing every CodeLLDB launch.
        for (adapter, expected) in [
            ("Debugpy", "internalConsole"),
            ("CodeLLDB", "internalConsole"),
        ] {
            let mut scenario = scenario(adapter, json!({"request": "launch"}));
            ensure_console_output(&mut scenario);
            assert_eq!(scenario.config["console"], Value::String(expected.into()));
        }

        // Adapters that route output themselves are left untouched.
        let mut js_scenario = scenario("JavaScript", json!({"request": "launch"}));
        ensure_console_output(&mut js_scenario);
        assert!(js_scenario.config.get("console").is_none());

        // An explicit console config is never overridden.
        let mut explicit_scenario = scenario(
            "Debugpy",
            json!({"request": "launch", "console": "integratedTerminal"}),
        );
        ensure_console_output(&mut explicit_scenario);
        assert_eq!(explicit_scenario.config["console"], "integratedTerminal");
    }

    #[test]
    fn ensure_stop_on_entry_skips_attach_and_explicit_configs() {
        let mut launch = scenario("Debugpy", json!({"request": "launch"}));
        ensure_stop_on_entry(&mut launch);
        assert_eq!(launch.config["stopOnEntry"], true);

        let mut attach = scenario("Debugpy", json!({"request": "attach", "pid": 1}));
        ensure_stop_on_entry(&mut attach);
        assert!(attach.config.get("stopOnEntry").is_none());

        let mut explicit = scenario(
            "Debugpy",
            json!({"request": "launch", "stopOnEntry": false}),
        );
        ensure_stop_on_entry(&mut explicit);
        assert_eq!(explicit.config["stopOnEntry"], false);
    }

    #[test]
    fn normalize_scenario_config_unwraps_nested_config() {
        let mut nested = scenario(
            "Debugpy",
            json!({"config": {"request": "launch", "program": "main.py"}}),
        );
        normalize_scenario_config(&mut nested);
        assert_eq!(nested.config["program"], "main.py");
        assert!(nested.config.get("config").is_none());

        let mut spread = scenario("Debugpy", json!({"request": "launch"}));
        normalize_scenario_config(&mut spread);
        assert_eq!(spread.config["request"], "launch");
    }

    #[test]
    fn evaluate_permission_inputs_include_expression_and_frame() {
        let inputs =
            evaluate_permission_inputs("evaluate", SessionId::from_proto(7), "a + b", Some(3))
                .unwrap();
        assert_eq!(
            inputs,
            vec!["evaluate session_id:7 expression:\"a + b\" frame_id:3".to_string()]
        );

        let inputs =
            evaluate_permission_inputs("evaluate", SessionId::from_proto(7), "a + b", None)
                .unwrap();
        assert_eq!(
            inputs,
            vec!["evaluate session_id:7 expression:\"a + b\" frame_id:none".to_string()]
        );
    }

    #[test]
    fn set_variable_permission_inputs_include_name_and_value() {
        let inputs =
            set_variable_permission_inputs("set_variable", SessionId::from_proto(7), "count", "42")
                .unwrap();
        assert_eq!(
            inputs,
            vec!["set_variable session_id:7 name:\"count\" value:\"42\"".to_string()]
        );
    }

    #[test]
    fn read_memory_permission_inputs_include_memory_reference_and_count() {
        let inputs = read_memory_permission_inputs(
            "read_memory",
            SessionId::from_proto(7),
            "0x1000",
            Some(0),
            4,
        );
        assert_eq!(
            inputs,
            vec!["read_memory session_id:7 memory_reference:0x1000 offset:0 count:4".to_string()]
        );

        let inputs = read_memory_permission_inputs(
            "read_memory",
            SessionId::from_proto(7),
            "0x1000",
            None,
            4,
        );
        assert_eq!(
            inputs,
            vec![
                "read_memory session_id:7 memory_reference:0x1000 offset:none count:4".to_string()
            ]
        );
    }

    #[test]
    fn select_history_permission_inputs_include_index() {
        let inputs =
            select_history_permission_inputs("select_history", SessionId::from_proto(7), 2);
        assert_eq!(
            inputs,
            vec!["select_history session_id:7 index:2".to_string()]
        );
    }

    #[test]
    fn watch_expression_permission_inputs_include_expression_and_frame() {
        let inputs = add_watch_expression_permission_inputs(
            "add_watch_expression",
            SessionId::from_proto(7),
            "a + b",
            Some(3),
        )
        .unwrap();
        assert_eq!(
            inputs,
            vec!["add_watch_expression session_id:7 expression:\"a + b\" frame_id:3".to_string()]
        );

        let inputs = remove_watch_expression_permission_inputs(
            "remove_watch_expression",
            SessionId::from_proto(7),
            "a + b",
        )
        .unwrap();
        assert_eq!(
            inputs,
            vec!["remove_watch_expression session_id:7 expression:\"a + b\"".to_string()]
        );
    }

    #[test]
    fn debugger_operation_serializes_snake_case() {
        for (operation, expected) in [
            (DebuggerOperation::ReadMemory, "read_memory"),
            (DebuggerOperation::ListHistory, "list_history"),
            (DebuggerOperation::SelectHistory, "select_history"),
            (
                DebuggerOperation::ListWatchExpressions,
                "list_watch_expressions",
            ),
            (
                DebuggerOperation::AddWatchExpression,
                "add_watch_expression",
            ),
            (
                DebuggerOperation::RemoveWatchExpression,
                "remove_watch_expression",
            ),
            (DebuggerOperation::ListModules, "list_modules"),
            (DebuggerOperation::ListLoadedSources, "list_loaded_sources"),
        ] {
            assert_eq!(
                serde_json::to_value(operation).unwrap(),
                Value::String(expected.to_string())
            );
        }
    }

    #[test]
    fn control_action_serializes_snake_case() {
        for (action, expected) in [
            (ControlAction::Continue, "continue"),
            (ControlAction::Pause, "pause"),
            (ControlAction::StepOver, "step_over"),
            (ControlAction::StepIn, "step_in"),
            (ControlAction::StepOut, "step_out"),
            (ControlAction::StepBack, "step_back"),
            (ControlAction::RunToLine, "run_to_line"),
            (ControlAction::Detach, "detach"),
            (ControlAction::Restart, "restart"),
            (ControlAction::RestartFrame, "restart_frame"),
        ] {
            assert_eq!(
                serde_json::to_value(action).unwrap(),
                Value::String(expected.to_string())
            );
        }
    }

    #[test]
    fn control_permission_input_includes_restart_frame_id() {
        let restart_frame = ResolvedControlInput {
            session_id: SessionId::from_proto(7),
            thread_id: None,
            action: ControlAction::RestartFrame,
            granularity: None,
            path: None,
            line: None,
            frame_id: Some(3),
            timeout_ms: None,
        };
        assert_eq!(
            control_permission_input(&restart_frame),
            "action:restart_frame session_id:7 thread_id:none frame_id:3"
        );

        let detach = ResolvedControlInput {
            session_id: SessionId::from_proto(7),
            thread_id: None,
            action: ControlAction::Detach,
            granularity: None,
            path: None,
            line: None,
            frame_id: None,
            timeout_ms: None,
        };
        assert_eq!(
            control_permission_input(&detach),
            "action:detach session_id:7 thread_id:none"
        );
    }

    #[test]
    fn exception_breakpoint_permission_inputs_include_id_and_enabled() {
        let inputs = exception_breakpoint_permission_inputs(
            "set_exception_breakpoints",
            &[ExceptionBreakpointInput {
                id: "uncaught".into(),
                enabled: true,
            }],
            SessionId::from_proto(7),
        );
        assert_eq!(
            inputs,
            vec!["set_exception_breakpoints session_id:7 id:uncaught enabled:true".to_string()]
        );
    }

    #[test]
    fn data_breakpoint_permission_inputs_include_name_and_access_type() {
        let inputs = data_breakpoint_permission_inputs(
            "set_data_breakpoints",
            &[DataBreakpointInput {
                variables_reference: 100,
                name: "a".into(),
                access_type: Some("write".into()),
                condition: None,
                hit_condition: None,
            }],
            SessionId::from_proto(7),
        );
        assert_eq!(
            inputs,
            vec![
                "set_data_breakpoints session_id:7 variables_reference:100 name:a access_type:write condition:none hit_condition:none"
                    .to_string()
            ]
        );
    }

    #[test]
    fn debugger_operation_serializes_new_breakpoint_operations() {
        for (operation, expected) in [
            (
                DebuggerOperation::ListExceptionBreakpoints,
                "list_exception_breakpoints",
            ),
            (
                DebuggerOperation::SetExceptionBreakpoints,
                "set_exception_breakpoints",
            ),
            (
                DebuggerOperation::ListDataBreakpoints,
                "list_data_breakpoints",
            ),
            (
                DebuggerOperation::SetDataBreakpoints,
                "set_data_breakpoints",
            ),
            (
                DebuggerOperation::SetIgnoreBreakpoints,
                "set_ignore_breakpoints",
            ),
            (DebuggerOperation::ClearBreakpoints, "clear_breakpoints"),
        ] {
            assert_eq!(
                serde_json::to_value(operation).unwrap(),
                Value::String(expected.to_string())
            );
        }
    }
}
