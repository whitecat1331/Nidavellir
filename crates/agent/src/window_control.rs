//! The in-process command surface that lets the agent drive the Zed UI by
//! command instead of by pixel (the reason `zed_ui`/`window_control` exists —
//! see `plans/in-progress-plans/AGENT_WINDOW_CONTROL.md`).
//!
//! A [`WindowControlHost`] is installed on the `NativeAgent` by the agent
//! panel, mirroring `SiblingThreadHost`/`DebuggerHost`. Native-agent tools
//! reach it through `ThreadEnvironment::window_control`, so the tool layer
//! never depends on `agent_ui`.
//!
//! Everything here is **dev-channel only**: [`window_control_enabled`] gates
//! both installing the host and exposing the `window_control` tool, so the
//! surface is never shipped in a real release (`Preview`/`Stable`) or
//! `Nightly` build.

use anyhow::Result;
use gpui::{AsyncApp, Task};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A single UI control operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowControlOperation {
    /// Return a structured snapshot of the current UI state (active thread,
    /// focused editor text, visible affordances). Use this instead of OCR.
    State,
    /// Dispatch a registered action by name, e.g. `agent::ToggleFocus`.
    DispatchAction,
    /// Move keyboard focus to a named view/handle, e.g.
    /// `agent_panel.message_editor`.
    Focus,
    /// Insert text into the focused editor, optionally submitting it.
    Type,
    /// Send a single key chord, e.g. `ctrl-shift-p` or `enter`.
    Press,
    /// Open an existing agent thread by id or `zed:///agent/thread/<uuid>` URL.
    OpenThread,
}

impl WindowControlOperation {
    pub fn label(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::DispatchAction => "dispatch_action",
            Self::Focus => "focus",
            Self::Type => "type",
            Self::Press => "press",
            Self::OpenThread => "open_thread",
        }
    }
}

/// A request from a native-agent tool to the UI host.
///
/// This doubles as the `window_control` tool's input schema, so the doc
/// comment on each field is what the model sees when deciding how to call it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct WindowControlRequest {
    /// The operation to perform.
    pub operation: WindowControlOperation,

    /// For `dispatch_action`: the registered action name (e.g.
    /// `agent::ToggleFocus`, `workspace::SaveAll`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,

    /// For `dispatch_action`: JSON arguments for the action, when it takes any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,

    /// For `focus`: the target view/handle. Known targets are
    /// `agent_panel.message_editor`, `agent_panel`, `editor`, and `terminal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,

    /// For `type`: the text to insert into the focused editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    /// For `type`: whether to submit the editor contents after inserting.
    #[serde(default)]
    pub submit: bool,

    /// For `press`: a single key chord, e.g. `ctrl-shift-p`, `escape`, `enter`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<String>,

    /// For `open_thread`: the thread id (uuid) to open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,

    /// For `open_thread`: a `zed:///agent/thread/<uuid>` deep link to open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl WindowControlRequest {
    pub fn state() -> Self {
        Self {
            operation: WindowControlOperation::State,
            ..Self::default()
        }
    }
}

impl Default for WindowControlRequest {
    fn default() -> Self {
        Self {
            operation: WindowControlOperation::State,
            action: None,
            args: None,
            target: None,
            text: None,
            submit: false,
            keys: None,
            thread_id: None,
            url: None,
        }
    }
}

/// Implemented by the UI layer to let native-agent tools drive the window:
/// read structured state and dispatch actions/focus/typing, with no synthetic
/// mouse events and no OS foreground dependency.
///
/// `agent_ui::AgentPanel` installs an implementation on the `NativeAgent` when
/// it sets up a connection (dev builds only). Tools discover it through
/// `ThreadEnvironment::window_control`.
pub trait WindowControlHost {
    /// Perform `request` and return a JSON result.
    ///
    /// `state` returns the structured snapshot; the control operations return a
    /// small acknowledgement object describing what changed.
    fn window_control(&self, request: WindowControlRequest, cx: &mut AsyncApp)
    -> Task<Result<serde_json::Value>>;
}

/// Whether a build on `channel` exposes the agent window-control surface.
///
/// Deliberately dev-only: any non-`Dev` channel gets nothing, so the surface
/// cannot leak into a shipped release.
pub fn window_control_enabled_for(channel: release_channel::ReleaseChannel) -> bool {
    matches!(channel, release_channel::ReleaseChannel::Dev)
}

/// Whether *this* build exposes the agent window-control surface.
pub fn window_control_enabled() -> bool {
    window_control_enabled_for(*release_channel::RELEASE_CHANNEL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use release_channel::ReleaseChannel;

    #[test]
    fn window_control_is_dev_channel_only() {
        assert!(window_control_enabled_for(ReleaseChannel::Dev));
        assert!(!window_control_enabled_for(ReleaseChannel::Nightly));
        assert!(!window_control_enabled_for(ReleaseChannel::Preview));
        assert!(!window_control_enabled_for(ReleaseChannel::Stable));
    }

    #[test]
    fn request_serializes_snake_case_operation() {
        let value = serde_json::to_value(WindowControlRequest {
            operation: WindowControlOperation::DispatchAction,
            action: Some("agent::ToggleFocus".into()),
            ..WindowControlRequest::default()
        })
        .unwrap();
        assert_eq!(value["operation"], "dispatch_action");
        assert_eq!(value["action"], "agent::ToggleFocus");
        // Defaulted fields are omitted so the model's JSON stays small.
        assert!(value.get("text").is_none());
        assert!(value.get("submit").is_none());
    }

    #[test]
    fn default_request_is_state() {
        assert_eq!(
            WindowControlRequest::default().operation,
            WindowControlOperation::State
        );
    }
}
