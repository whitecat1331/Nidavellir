use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::{
    AgentTool, ThreadEnvironment, ToolCallEventStream, ToolInput, WindowControlOperation,
    WindowControlRequest,
};

/// Drive the Zed UI by command: read structured state and dispatch
/// actions/focus/typing, with no synthetic mouse events and no OS foreground
/// dependency.
///
/// This is a **dev-channel-only** surface (see
/// `agent::window_control_enabled`); on a release build the tool is never
/// exposed to the model.
///
/// ### When to use
/// - To drive or inspect the Zed window in place of the pixel-based panel
///   harness: focus a view, type into the agent's message editor and submit,
///   dispatch a registered action, or read back structured UI state.
///
/// ### Operations
/// - `state` — a JSON snapshot of the active thread, focused/agent message
///   editor text, and the active item.
/// - `dispatch_action` — dispatch a registered action by name.
/// - `focus` — move focus to a named view/handle
///   (`agent_panel.message_editor`, `agent_panel`, `editor`, `terminal`).
/// - `type` — insert text into the agent message editor, optionally submitting.
/// - `press` — send a single key chord (e.g. `ctrl-shift-p`, `enter`).
/// - `open_thread` — open an existing agent thread by id or deep-link URL.
pub struct WindowControlTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl WindowControlTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WindowControlToolOutput {
    Success {
        operation: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    Error {
        operation: String,
        error: String,
    },
}

impl From<WindowControlToolOutput> for LanguageModelToolResultContent {
    fn from(output: WindowControlToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|e| format!("Failed to serialize window_control output: {e}"))
            .into()
    }
}

impl AgentTool for WindowControlTool {
    type Input = WindowControlRequest;
    type Output = WindowControlToolOutput;

    const NAME: &'static str = "window_control";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(i) => format!("Window control: {}", i.operation.label()).into(),
            Err(value) => value
                .get("operation")
                .and_then(|v| v.as_str())
                .map(|s| format!("Window control: {s}").into())
                .unwrap_or_else(|| "Window control".into()),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let request = input.recv().await.map_err(|e| WindowControlToolOutput::Error {
                operation: WindowControlOperation::State.label().to_string(),
                error: format!("Failed to receive tool input: {e}"),
            })?;

            let operation = request.operation.label().to_string();
            match self.environment.window_control(request, cx).await {
                Ok(data) => Ok(WindowControlToolOutput::Success {
                    operation,
                    message: "Window control request completed".to_string(),
                    data: Some(data),
                }),
                Err(error) => Err(WindowControlToolOutput::Error {
                    operation,
                    error: error.to_string(),
                }),
            }
        })
    }
}
