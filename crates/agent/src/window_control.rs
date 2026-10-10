//! The in-process command surface that lets the agent drive the Zed UI by
//! command instead of by pixel (the reason `zed_ui`/`window_control` exists —
//! see `plans/in-progress-plans/AGENT_WINDOW_CONTROL.md`).
//!
//! A [`WindowControlHost`] is installed on the `NativeAgent` by the agent
//! panel, mirroring `SiblingThreadHost`/`DebuggerHost`. Native-agent tools
//! reach it through `ThreadEnvironment::window_control`, so the tool layer
//! never depends on `agent_ui`.
//!
//! This module also owns the two security requirements of the plan's Design D
//! contract: [`redact_sensitive`] scrubs secrets and private paths out of the
//! `state` snapshot (it is an *output* boundary), and [`WindowControlEnvelope`]
//! carries the bearer token the local command file (Phase 3) must present.
//!
//! Everything here is **dev-channel only**: [`window_control_enabled`] gates
//! both installing the host and exposing the `window_control` tool, so the
//! surface is never shipped in a real release (`Preview`/`Stable`) or
//! `Nightly` build.

use std::sync::OnceLock;

use anyhow::Result;
use gpui::{App, AsyncApp, Task};
use regex::Regex;
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
    /// List the registered, addressable UI selectors (optionally filtered).
    Find,
    /// Invoke a selector's handler directly (no synthetic mouse event).
    Click,
    /// Assert on a selector's visibility or text.
    Assert,
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
            Self::Find => "find",
            Self::Click => "click",
            Self::Assert => "assert",
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

    /// For `find`/`click`/`assert`: the selector id (e.g.
    /// `agent_panel.message_editor`). For `find`, an optional substring filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,

    /// For `assert`: the exact text the selector must currently show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_text: Option<String>,

    /// For `assert`: the visibility the selector must currently have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_visible: Option<bool>,
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
            selector: None,
            expect_text: None,
            expect_visible: None,
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
    fn window_control(
        &self,
        request: WindowControlRequest,
        cx: &mut AsyncApp,
    ) -> Task<Result<serde_json::Value>>;
}

/// Whether a build on `channel` exposes the agent window-control surface.
///
/// Deliberately dev-only: any non-`Dev` channel gets nothing, so the surface
/// cannot leak into a shipped release.
pub fn window_control_enabled_for(channel: release_channel::ReleaseChannel) -> bool {
    matches!(channel, release_channel::ReleaseChannel::Dev)
}

/// Whether the running app exposes the agent window-control surface.
///
/// Reads the app's release-channel global (the same value `release_channel::init`
/// installs at startup), falling back to the compiled-in channel. Taking `cx`
/// keeps the gate a single, testable decision: a test can
/// `release_channel::init_test(.., Stable, cx)` and assert the surface is gone.
pub fn window_control_enabled(cx: &App) -> bool {
    let channel = release_channel::ReleaseChannel::try_global(cx)
        .unwrap_or(*release_channel::RELEASE_CHANNEL);
    window_control_enabled_for(channel)
}

/// The wire format for the non-agent (Phase 3) command file: a bearer token plus
/// the request it authorizes.
///
/// Kept separate from [`WindowControlRequest`] so the model-facing tool schema
/// never grows a token field — the token is a *transport* concern that only the
/// local command channel uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowControlEnvelope {
    /// The token the channel was started with. A request whose token does not
    /// match is rejected without being dispatched.
    pub token: String,
    /// The operation to dispatch once the token checks out.
    pub request: WindowControlRequest,
}

impl WindowControlEnvelope {
    /// Whether `expected` authorizes this envelope.
    pub fn is_authorized(&self, expected: &str) -> bool {
        !expected.is_empty() && self.token == expected
    }
}

/// Generate a fresh, unguessable token for the local command channel (128
/// random bits, hex-encoded).
pub fn generate_channel_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// Redact likely secrets and private absolute paths from a string before it
/// leaves the process through the `window_control` surface (`state`, and the
/// selector text that `find`/`assert` return).
///
/// The snapshot is an *output* boundary: without redaction, `state` would hand
/// the model — and any transcript or log that captures its tool result — the
/// user's credentials, tokens, and account path. It is intentionally
/// conservative (over-redacts rather than under-redacts) and only ever removes
/// detail, so it cannot reveal more than the raw string would.
pub fn redact_sensitive(text: &str) -> String {
    let mut redacted = redact_home_dir(text);
    for pattern in secret_patterns() {
        redacted = pattern.replace_all(&redacted, "<redacted>").into_owned();
    }
    redacted
}

/// Replace the user's home directory prefix with `~`, so absolute paths in the
/// snapshot don't reveal the account name.
fn redact_home_dir(text: &str) -> String {
    let home = paths::home_dir();
    let home = home.to_string_lossy();
    if home.is_empty() {
        return text.to_owned();
    }
    text.replace(home.as_ref(), "~")
}

/// Substrings that are almost always credentials. Compiled once.
fn secret_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // Provider API keys (OpenAI, Anthropic, GitHub, Slack, AWS, Google).
            r"\bsk-[A-Za-z0-9_-]{16,}\b",
            r"\bsk-ant-[A-Za-z0-9_-]{16,}\b",
            r"\bgh[pousr]_[A-Za-z0-9]{20,}\b",
            r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b",
            r"\bAKIA[0-9A-Z]{16}\b",
            r"\bAIza[0-9A-Za-z_-]{30,}\b",
            // JSON Web Tokens: three base64url segments.
            r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}\b",
            // `key = value` / `key: value` assignments for secret-ish keys.
            r#"(?i)\b(?:api[_-]?key|secret|token|password|passwd|authorization|bearer)\b\s*[:=]\s*['\"]?[^\s'\"]{6,}"#,
            // `Bearer <token>` authorization headers.
            r"(?i)\bbearer\s+[A-Za-z0-9._-]{8,}",
        ]
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
    })
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
        // `submit` is a plain bool with a serde default, so it serializes as
        // `false` rather than being omitted; the unused optional fields are
        // omitted to keep the model's JSON small.
        assert_eq!(value["submit"], false);
        assert!(value.get("text").is_none());
    }

    #[test]
    fn default_request_is_state() {
        assert_eq!(
            WindowControlRequest::default().operation,
            WindowControlOperation::State
        );
    }

    #[test]
    fn click_and_assert_operations_serialize() {
        let value = serde_json::to_value(WindowControlRequest {
            operation: WindowControlOperation::Assert,
            selector: Some("agent_panel.message_editor".into()),
            expect_text: Some("hi".into()),
            expect_visible: Some(true),
            ..WindowControlRequest::default()
        })
        .unwrap();
        assert_eq!(value["operation"], "assert");
        assert_eq!(value["selector"], "agent_panel.message_editor");
        assert_eq!(value["expect_text"], "hi");
        assert_eq!(value["expect_visible"], true);
    }

    #[test]
    fn redaction_removes_secrets_and_home_paths() {
        let home = paths::home_dir().to_string_lossy().into_owned();
        let input =
            format!("open {home}/src/secret.rs\nOPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwx\n");
        let redacted = redact_sensitive(&input);
        assert!(!redacted.contains(&home), "home dir leaked: {redacted}");
        assert!(redacted.contains('~'), "home path should collapse to ~");
        assert!(
            !redacted.contains("sk-abcdefghijklmnopqrstuvwx"),
            "api key leaked: {redacted}"
        );
        assert!(redacted.contains("<redacted>"));
    }

    #[test]
    fn redaction_leaves_ordinary_text_untouched() {
        assert_eq!(redact_sensitive("hello window"), "hello window");
        assert_eq!(
            redact_sensitive("Continued from thread \"Old thread\""),
            "Continued from thread \"Old thread\""
        );
    }

    #[test]
    fn channel_tokens_are_hex_and_unique() {
        let a = generate_channel_token();
        let b = generate_channel_token();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "each launch must mint a fresh token");
    }

    #[test]
    fn envelope_authorizes_only_a_matching_token() {
        let envelope = WindowControlEnvelope {
            token: "abc123".into(),
            request: WindowControlRequest::state(),
        };
        assert!(envelope.is_authorized("abc123"));
        assert!(!envelope.is_authorized("def456"));
        assert!(
            !envelope.is_authorized(""),
            "an unset token must never authorize"
        );
    }

    #[test]
    fn envelope_round_trips() {
        let envelope = WindowControlEnvelope {
            token: "deadbeef".into(),
            request: WindowControlRequest {
                operation: WindowControlOperation::Type,
                text: Some("hi".into()),
                submit: true,
                ..WindowControlRequest::default()
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: WindowControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.token, "deadbeef");
        assert_eq!(parsed.request.operation, WindowControlOperation::Type);
        assert!(parsed.request.submit);
    }
}
