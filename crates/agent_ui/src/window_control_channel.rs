//! Dev-only, local transport that lets **non-agent** drivers (verify scripts,
//! the debugger loop) drive the `window_control` surface without an agent turn
//! — Phase 3 of `plans/in-progress-plans/AGENT_WINDOW_CONTROL.md`.
//!
//! A driver writes a token-authorized envelope to
//! `<data_dir>/window-control/request.json`; a foreground poll loop in the
//! process verifies the token, dispatches the request through the same
//! in-process path the `window_control` tool uses, and writes the result to
//! `<data_dir>/window-control/response.json`.
//!
//! Local-only, dev-gated, **token-gated**: the directory lives under the user's
//! Zed data dir (no network, no remote reach), the whole channel is installed
//! only when `agent::window_control_enabled(cx)` is true, and every request must
//! present the current bearer token, which the process writes to `token` in the
//! same directory (owner-only where the platform supports it). The token is
//! regenerated on every launch, so a stale one cannot be replayed.
//!
//! Driver contract:
//! - read `token` (the fresh per-launch bearer token);
//! - write `request.json` atomically (a temp file, then rename/replace) as a
//!   `{"token": …, "request": …}` envelope;
//! - delete `response.json` before writing `request.json`;
//! - poll until `response.json` exists, and read it.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{AnyWindowHandle, App, WeakEntity};
use serde_json::json;

use crate::agent_panel::{AgentPanel, window_control_dispatch};

const CHANNEL_DIR: &str = "window-control";
const REQUEST_FILE: &str = "request.json";
const RESPONSE_FILE: &str = "response.json";
const TOKEN_FILE: &str = "token";
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Only one poll loop runs per process: the first agent panel to install its
/// host wins, so several windows don't all race to answer the same request.
static STARTED: AtomicBool = AtomicBool::new(false);

/// Start the command-file channel for `panel` in `window`, if it isn't already
/// running. A no-op in non-dev builds.
pub(crate) fn ensure_started(panel: WeakEntity<AgentPanel>, window: AnyWindowHandle, cx: &mut App) {
    if !agent::window_control_enabled(cx) {
        return;
    }
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    let dir = paths::data_dir().join(CHANNEL_DIR);
    if let Err(error) = std::fs::create_dir_all(&dir) {
        log::warn!(
            "window-control channel: could not create {}: {error}",
            dir.display()
        );
        STARTED.store(false, Ordering::SeqCst);
        return;
    }

    // Fail closed: no token, no channel. A driver that cannot read the token
    // cannot drive the UI.
    let Some(token) = write_channel_token(&dir) else {
        STARTED.store(false, Ordering::SeqCst);
        return;
    };

    let request_path = dir.join(REQUEST_FILE);
    let response_path = dir.join(RESPONSE_FILE);

    // Start from a clean slate so a stale request/response isn't mistaken for
    // this run's.
    let _ = std::fs::remove_file(&request_path);
    let _ = std::fs::remove_file(&response_path);

    log::info!(
        "window-control channel listening at {} (token in {})",
        request_path.display(),
        dir.join(TOKEN_FILE).display()
    );

    cx.spawn(async move |cx| {
        let mut last_modified = None;
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;

            let Ok(metadata) = std::fs::metadata(&request_path) else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if last_modified == Some(modified) {
                continue;
            }
            last_modified = Some(modified);

            let Ok(text) = std::fs::read_to_string(&request_path) else {
                continue;
            };

            let response = match serde_json::from_str::<agent::WindowControlEnvelope>(&text) {
                Ok(envelope) if envelope.is_authorized(&token) => {
                    let request = envelope.request;
                    let operation = request.operation.label().to_string();
                    let result = panel
                        .upgrade()
                        .ok_or_else(|| anyhow::anyhow!("Agent panel is no longer available"))
                        .and_then(|panel| {
                            window.update(cx, |_root, window, cx| {
                                window_control_dispatch(&panel, request, window, cx)
                            })
                        });
                    match result {
                        Ok(Ok(value)) => json!({ "operation": operation, "result": value }),
                        Ok(Err(error)) | Err(error) => {
                            json!({ "operation": operation, "error": error.to_string() })
                        }
                    }
                }
                Ok(_) => json!({ "error": "unauthorized: window-control token mismatch" }),
                Err(error) => json!({ "error": format!("invalid request: {error}") }),
            };

            match serde_json::to_string_pretty(&response) {
                Ok(body) => {
                    if let Err(error) = std::fs::write(&response_path, body) {
                        log::warn!("window-control channel: could not write response: {error}");
                    }
                }
                Err(error) => log::warn!("window-control channel: could not serialize: {error}"),
            }
        }
    })
    .detach();
}

/// Write a fresh channel token to `<dir>/token` and return it.
///
/// Returns `None` (and the caller declines to start the channel) if the token
/// cannot be persisted — failing closed is safer than running ungated. On Unix
/// the file is restricted to the owner; on Windows it inherits the data dir's
/// user-only ACL.
fn write_channel_token(dir: &Path) -> Option<String> {
    let path = dir.join(TOKEN_FILE);
    let token = agent::generate_channel_token();
    if let Err(error) = std::fs::write(&path, &token) {
        log::warn!(
            "window-control channel: could not write token {}: {error}",
            path.display()
        );
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        {
            log::warn!("window-control channel: could not restrict token permissions: {error}");
        }
    }
    Some(token)
}
