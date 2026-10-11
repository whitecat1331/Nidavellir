use agent_client_protocol::schema::v1 as acp;
use agent_settings::builtin_profiles;
use anyhow::{Context as _, Result};
use browser_tools::{AgentBrowserApi, DrivenBy};
use gpui::{App, AppContext as _, SharedString, Task, WeakEntity};
use language_model::{LanguageModelImage, LanguageModelImageExt, LanguageModelToolResultContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::{AgentTool, Thread, ToolCallEventStream, ToolInput, ToolPermissionContext};

/// Interact with a browser the agent controls. Read-only operations such as
/// `list_sessions`, `snapshot`, `screenshot`, `read_console`, and `read_network` are available
/// in read-only modes (`read` and `plan`). Operations that start sessions, navigate, click, type,
/// evaluate JavaScript, manage targets, or stop sessions require a mode that can
/// change things (`write` or `execute`) and user permission.
///
/// The observation surface is text-first: `snapshot` returns the page URL,
/// title, body text, and interactive elements. Screenshots are not a
/// requirement; text-only models get full interactive control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserOperation {
    /// List active browser sessions and their targets.
    #[default]
    ListSessions,
    /// Launch a browser (Chromium over CDP) with a target URL.
    StartSession,
    /// Navigate a target to a URL.
    Navigate,
    /// Capture a bounded text snapshot of a target.
    Snapshot,
    /// Capture a screenshot of a target as an image.
    Screenshot,
    /// Click an element in a target.
    Click,
    /// Type text into a target.
    Type,
    /// Evaluate a JavaScript expression in a target.
    Evaluate,
    /// Read recent console events for a target.
    ReadConsole,
    /// Read recent network events for a target.
    ReadNetwork,
    /// Open a new target (tab) in a session.
    OpenTarget,
    /// Close a target in a session.
    CloseTarget,
    /// Set the active target in a session.
    ActivateTarget,
    /// Stop a browser session.
    StopSession,
}

/// A single browser operation and the fields it needs.
///
/// Kept as a flat struct (rather than a tagged enum) so the JSON schema
/// advertised to language-model providers is a `type: "object"` with an
/// `operation` discriminator.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct BrowserToolInput {
    /// Which browser operation to run.
    pub operation: BrowserOperation,
    /// Browser session id, used by most operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    /// Target id within a session, used to scope an operation to a specific
    /// tab. When omitted, the session's active target is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    /// URL, used by `start_session`, `navigate`, and `open_target`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Element selector or ref, used by `click` and `type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Text to type, used by `type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// JavaScript expression to evaluate, used by `evaluate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    /// Keep the browser process running after stopping the session. Defaults to
    /// false, which closes the browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_open: Option<bool>,
    /// Launch the browser headless (no visible window). Defaults to false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BrowserToolOutput {
    Success {
        operation: String,
        message: String,
        data: Value,
    },
    Screenshot {
        operation: String,
        image: LanguageModelImage,
    },
    Error {
        operation: Option<String>,
        error: String,
    },
}

impl From<BrowserToolOutput> for LanguageModelToolResultContent {
    fn from(output: BrowserToolOutput) -> Self {
        match output {
            BrowserToolOutput::Success {
                operation,
                message,
                data,
            } => {
                let data = serde_json::to_string_pretty(&data).unwrap_or_else(|error| {
                    format!("<failed to serialize browser output: {error}>")
                });
                format!("Browser `{operation}` succeeded: {message}\n\n```json\n{data}\n```").into()
            }
            BrowserToolOutput::Screenshot { image, .. } => {
                LanguageModelToolResultContent::Image(image)
            }
            BrowserToolOutput::Error { operation, error } => {
                let operation = operation.as_deref().unwrap_or("unknown");
                format!("Browser `{operation}` failed: {error}").into()
            }
        }
    }
}

pub struct BrowserTool {
    api: Arc<AgentBrowserApi>,
    thread: WeakEntity<Thread>,
}

impl BrowserTool {
    pub fn new(thread: WeakEntity<Thread>, api: Arc<AgentBrowserApi>) -> Self {
        Self { api, thread }
    }

    fn is_read_only_profile(&self, cx: &App) -> bool {
        self.thread
            .read_with(cx, |thread, _| {
                builtin_profiles::is_read_only(thread.profile())
            })
            .unwrap_or(false)
    }

    fn ensure_write_mode(&self, operation: &str, cx: &gpui::AsyncApp) -> Result<()> {
        if cx.update(|cx| self.is_read_only_profile(cx)) {
            anyhow::bail!(
                "browser.{operation} is not available in read-only modes. Switch to Write or Execute mode to start sessions, navigate, or interact with the browser."
            );
        }
        Ok(())
    }

    async fn run_operation(
        self: Arc<Self>,
        input: BrowserToolInput,
        operation: String,
        event_stream: ToolCallEventStream,
        cx: &mut gpui::AsyncApp,
    ) -> Result<BrowserToolOutput> {
        match input.operation {
            BrowserOperation::ListSessions => {
                let sessions = self.api.list_sessions().await;
                Ok(success(
                    operation,
                    "listed browser sessions",
                    Value::Array(sessions),
                ))
            }
            BrowserOperation::Snapshot => {
                let session_id = input
                    .session_id
                    .context("session_id is required for browser snapshot")?;
                let target_id = input.target_id.as_deref();
                let snapshot = self.api.snapshot(session_id, target_id).await?;
                Ok(success(operation, "captured browser snapshot", snapshot))
            }
            BrowserOperation::Screenshot => {
                let session_id = input
                    .session_id
                    .context("session_id is required for browser screenshot")?;
                let target_id = input.target_id.as_deref();
                let data = self.api.screenshot(session_id, target_id).await?;
                let supports_images = cx.update(|cx| {
                    self.thread
                        .read_with(cx, |thread, _| {
                            thread.model().is_some_and(|model| model.supports_images())
                        })
                        .unwrap_or(false)
                });
                if supports_images {
                    let image = cx
                        .background_spawn(async move {
                            LanguageModelImage::from_base64_image(&data, "image/png")
                        })
                        .await
                        .context("failed to convert browser screenshot")?
                        .context(
                            "browser screenshot could not be converted for language model input",
                        )?;
                    Ok(BrowserToolOutput::Screenshot { operation, image })
                } else {
                    Ok(success(
                        operation,
                        "captured screenshot, but the current model cannot accept image input",
                        json!({
                            "captured": true,
                            "base64_bytes": data.len(),
                        }),
                    ))
                }
            }
            BrowserOperation::StartSession => {
                self.ensure_write_mode(&operation, cx)?;
                let url = input
                    .url
                    .context("url is required for browser start_session")?;
                let headless = input.headless.unwrap_or(false);
                authorize_browser_operation(
                    &event_stream,
                    "Start browser session",
                    permission_inputs(&operation, [format!("url:{url}")]),
                    cx,
                )
                .await?;
                let session_id = self.api.start_session(&url, headless).await?;
                let _ = self.api.set_driven_by(session_id, DrivenBy::Agent).await;
                Ok(success(
                    operation,
                    "started browser session",
                    json!({ "session_id": session_id }),
                ))
            }
            BrowserOperation::Navigate => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = input
                    .session_id
                    .context("session_id is required for browser navigate")?;
                let target_id = input.target_id.as_deref();
                let url = input.url.context("url is required for browser navigate")?;
                authorize_browser_operation(
                    &event_stream,
                    "Navigate browser",
                    permission_inputs(&operation, [format!("session_id:{session_id} url:{url}")]),
                    cx,
                )
                .await?;
                self.api.navigate(session_id, target_id, &url).await?;
                Ok(success(operation, "navigated", json!({})))
            }
            BrowserOperation::StopSession => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = input
                    .session_id
                    .context("session_id is required for browser stop_session")?;
                let keep_open = input.keep_open.unwrap_or(false);
                authorize_browser_operation(
                    &event_stream,
                    "Stop browser session",
                    permission_inputs(&operation, [format!("session_id:{session_id}")]),
                    cx,
                )
                .await?;
                self.api.stop_session(session_id, keep_open).await?;
                Ok(success(operation, "stopped browser session", json!({})))
            }
            BrowserOperation::Click => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = input
                    .session_id
                    .context("session_id is required for browser click")?;
                let target_id = input.target_id.as_deref();
                let selector = input
                    .selector
                    .context("selector is required for browser click")?;
                authorize_browser_operation(
                    &event_stream,
                    "Click browser element",
                    permission_inputs(
                        &operation,
                        [format!("session_id:{session_id} selector:{selector}")],
                    ),
                    cx,
                )
                .await?;
                let result = self.api.click(session_id, target_id, &selector).await?;
                Ok(success(operation, "clicked element", result))
            }
            BrowserOperation::Type => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = input
                    .session_id
                    .context("session_id is required for browser type")?;
                let target_id = input.target_id.as_deref();
                let selector = input
                    .selector
                    .context("selector is required for browser type")?;
                let text = input.text.context("text is required for browser type")?;
                authorize_browser_operation(
                    &event_stream,
                    "Type into browser",
                    permission_inputs(
                        &operation,
                        [format!(
                            "session_id:{session_id} selector:{selector} text:{text}"
                        )],
                    ),
                    cx,
                )
                .await?;
                let result = self
                    .api
                    .type_text(session_id, target_id, &selector, &text)
                    .await?;
                Ok(success(operation, "typed text", result))
            }
            BrowserOperation::Evaluate => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = input
                    .session_id
                    .context("session_id is required for browser evaluate")?;
                let target_id = input.target_id.as_deref();
                let expression = input
                    .expression
                    .context("expression is required for browser evaluate")?;
                authorize_browser_operation(
                    &event_stream,
                    "Evaluate browser JavaScript",
                    permission_inputs(
                        &operation,
                        [format!("session_id:{session_id} expression:{expression}")],
                    ),
                    cx,
                )
                .await?;
                let result = self
                    .api
                    .evaluate(session_id, target_id, &expression)
                    .await?;
                Ok(success(operation, "evaluated expression", result))
            }
            BrowserOperation::ReadConsole => {
                let session_id = input
                    .session_id
                    .context("session_id is required for browser read_console")?;
                let target_id = input.target_id.as_deref();
                let result = self.api.read_console(session_id, target_id).await?;
                Ok(success(operation, "read console events", result))
            }
            BrowserOperation::ReadNetwork => {
                let session_id = input
                    .session_id
                    .context("session_id is required for browser read_network")?;
                let target_id = input.target_id.as_deref();
                let result = self.api.read_network(session_id, target_id).await?;
                Ok(success(operation, "read network events", result))
            }
            BrowserOperation::OpenTarget => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = input
                    .session_id
                    .context("session_id is required for browser open_target")?;
                let url = input
                    .url
                    .context("url is required for browser open_target")?;
                authorize_browser_operation(
                    &event_stream,
                    "Open browser target",
                    permission_inputs(&operation, [format!("session_id:{session_id} url:{url}")]),
                    cx,
                )
                .await?;
                let target = self.api.open_target(session_id, &url).await?;
                Ok(success(operation, "opened browser target", target))
            }
            BrowserOperation::CloseTarget => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = input
                    .session_id
                    .context("session_id is required for browser close_target")?;
                let target_id = input
                    .target_id
                    .context("target_id is required for browser close_target")?;
                authorize_browser_operation(
                    &event_stream,
                    "Close browser target",
                    permission_inputs(
                        &operation,
                        [format!("session_id:{session_id} target_id:{target_id}")],
                    ),
                    cx,
                )
                .await?;
                self.api.close_target(session_id, &target_id).await?;
                Ok(success(operation, "closed browser target", json!({})))
            }
            BrowserOperation::ActivateTarget => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = input
                    .session_id
                    .context("session_id is required for browser activate_target")?;
                let target_id = input
                    .target_id
                    .context("target_id is required for browser activate_target")?;
                authorize_browser_operation(
                    &event_stream,
                    "Activate browser target",
                    permission_inputs(
                        &operation,
                        [format!("session_id:{session_id} target_id:{target_id}")],
                    ),
                    cx,
                )
                .await?;
                self.api.activate_target(session_id, &target_id).await?;
                Ok(success(operation, "activated browser target", json!({})))
            }
        }
    }
}

impl AgentTool for BrowserTool {
    type Input = BrowserToolInput;
    type Output = BrowserToolOutput;

    const NAME: &'static str = "browser";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Browser: {}", operation_name(&input)).into(),
            Err(value) => value
                .get("operation")
                .and_then(|value| value.as_str())
                .map(|operation| format!("Browser: {operation}").into())
                .unwrap_or_else(|| "Browser".into()),
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
                .map_err(|error| BrowserToolOutput::Error {
                    operation: None,
                    error: format!("Failed to receive browser tool input: {error}"),
                })?;
            let operation = operation_name(&input).to_string();
            let session_id = input.session_id;
            let api = self.api.clone();
            match self
                .run_operation(input, operation.clone(), event_stream, cx)
                .await
            {
                Ok(output) => {
                    if let Some(session_id) = session_id {
                        let _ = api.set_driven_by(session_id, DrivenBy::Agent).await;
                    }
                    Ok(output)
                }
                Err(error) => Err(BrowserToolOutput::Error {
                    operation: Some(operation),
                    error: error.to_string(),
                }),
            }
        })
    }
}

async fn authorize_browser_operation(
    event_stream: &ToolCallEventStream,
    title: impl Into<String>,
    input_values: Vec<String>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let title = title.into();
    let task = cx.update(|cx| {
        event_stream.authorize(
            title,
            ToolPermissionContext::new(BrowserTool::NAME, input_values),
            cx,
        )
    });
    task.await
}

pub(crate) fn permission_inputs(
    operation: &str,
    values: impl IntoIterator<Item = String>,
) -> Vec<String> {
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

fn success(operation: String, message: impl Into<String>, data: Value) -> BrowserToolOutput {
    BrowserToolOutput::Success {
        operation,
        message: message.into(),
        data,
    }
}

fn operation_name(input: &BrowserToolInput) -> &'static str {
    match input.operation {
        BrowserOperation::ListSessions => "list_sessions",
        BrowserOperation::StartSession => "start_session",
        BrowserOperation::Navigate => "navigate",
        BrowserOperation::Snapshot => "snapshot",
        BrowserOperation::Screenshot => "screenshot",
        BrowserOperation::Click => "click",
        BrowserOperation::Type => "type",
        BrowserOperation::Evaluate => "evaluate",
        BrowserOperation::ReadConsole => "read_console",
        BrowserOperation::ReadNetwork => "read_network",
        BrowserOperation::OpenTarget => "open_target",
        BrowserOperation::CloseTarget => "close_target",
        BrowserOperation::ActivateTarget => "activate_target",
        BrowserOperation::StopSession => "stop_session",
    }
}
