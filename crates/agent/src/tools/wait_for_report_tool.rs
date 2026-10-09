use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, AppContext, BackgroundExecutor, Entity, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_TIMEOUT_SECONDS: u64 = 1800;
const DEFAULT_POLL_SECONDS: u64 = 10;

/// Wait for a launched build's verification report to reach a terminal state.
///
/// After launching a fresh fork build with a `VERIFY_*.md` plan injected, call
/// this tool to block until that report is done. The launched agent fills the
/// `## Report back` ✅/❌ boxes and, last, writes a `Result: PASS`/`FAIL` line
/// (or flips `## Status` to `complete`/`rejected`). The tool polls the file and
/// returns the verdict plus the report text, so the originating agent can
/// continue without a human relaying the result.
///
/// When the plan has `[agent-origin]` GUI steps it ends its headless half with a
/// `Handoff: READY-FOR-GUI` line instead (leaving `## Status` `open`); pass
/// `phase: "handoff"` to return as soon as that line appears, so the originating
/// agent can drive the window. A terminal `Result`/`Status` always wins over the
/// handoff, so the originator's final verdict supersedes it.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WaitForReportToolInput {
    /// Path to the `VERIFY_*.md` report to wait on (absolute, or relative to a
    /// worktree root).
    pub report_path: String,
    /// How to end the wait. `final` (default) blocks until the terminal
    /// `Result`/`Status` marker; `handoff` also returns on a
    /// `Handoff: READY-FOR-GUI` line.
    #[serde(default)]
    pub phase: WaitPhase,
    /// Maximum seconds to wait before returning a timeout result. Default 1800.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Seconds between re-reads of the report. Default 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_seconds: Option<u64>,
}

/// Whether the wait stops at the one-spawn `READY-FOR-GUI` handoff or runs
/// through to the terminal verdict. Mirrors `wait-for-report.py --phase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WaitPhase {
    /// Block until the terminal `Result`/`Status` marker.
    #[default]
    Final,
    /// Also return as soon as a `Handoff: READY-FOR-GUI` line appears.
    Handoff,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WaitForReportToolOutput {
    Success { message: String },
    Error { error: String },
}

impl From<WaitForReportToolOutput> for LanguageModelToolResultContent {
    fn from(value: WaitForReportToolOutput) -> Self {
        match value {
            WaitForReportToolOutput::Success { message } => message.into(),
            WaitForReportToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct WaitForReportTool {
    project: Entity<Project>,
}

impl WaitForReportTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for WaitForReportTool {
    type Input = WaitForReportToolInput;
    type Output = WaitForReportToolOutput;

    const NAME: &'static str = "wait_for_report";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Waiting for report {}", input.report_path).into(),
            Err(_) => "Waiting for report".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        let background_executor: BackgroundExecutor = cx.background_executor().clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| WaitForReportToolOutput::Error {
                    error: format!("Failed to receive wait_for_report tool input: {error}"),
                })?;
            let report_path = resolve_report_path(&project, &input.report_path, cx);
            wait_for_report(input, report_path, &background_executor).await
        })
    }
}

/// Resolves the report path to an absolute path. Absolute paths pass through
/// untouched; relative paths resolve against a worktree root so they are not
/// interpreted relative to the process working directory.
fn resolve_report_path<C: AppContext>(project: &Entity<Project>, path: &str, cx: &C) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    project
        .read_with(cx, |project, cx| {
            project
                .find_project_path(path, cx)
                .and_then(|project_path| project.absolute_path(&project_path, cx))
        })
        .unwrap_or_else(|| path.to_path_buf())
}

async fn wait_for_report(
    input: WaitForReportToolInput,
    report_path: PathBuf,
    background_executor: &BackgroundExecutor,
) -> Result<WaitForReportToolOutput, WaitForReportToolOutput> {
    let timeout = Duration::from_secs(
        input
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
            .clamp(1, 24 * 60 * 60),
    );
    let poll = Duration::from_secs(
        input
            .poll_seconds
            .unwrap_or(DEFAULT_POLL_SECONDS)
            .clamp(1, 60),
    );
    let deadline = Instant::now() + timeout;
    let phase = input.phase;

    loop {
        // The report may not exist yet right after launch; an empty read just
        // keeps the loop waiting until the timeout.
        let text = std::fs::read_to_string(&report_path).unwrap_or_default();
        let (state, verdict) = parse_state(&text);

        if state == "complete" || state == "rejected" {
            return Ok(WaitForReportToolOutput::Success {
                message: format_terminal(&state, verdict.as_deref(), &text),
            });
        }

        if phase == WaitPhase::Handoff && state == "handoff" {
            return Ok(WaitForReportToolOutput::Success {
                message: format!(
                    "wait_for_report: READY — handoff reached; drive the GUI against the \
                     launched window now.\n\n{}",
                    extract_report(&text),
                ),
            });
        }

        if Instant::now() >= deadline {
            return Ok(WaitForReportToolOutput::Success {
                message: format!(
                    "wait_for_report: TIMEOUT after {}s — no terminal marker in \"{}\". \
                     Escalate with the report below; do not auto-verdict.\n\n{}",
                    timeout.as_secs(),
                    report_path.display(),
                    extract_report(&text),
                ),
            });
        }

        background_executor.timer(poll).await;
    }
}

/// Parses the report's state. A `Result: PASS`/`FAIL` line anywhere is
/// authoritative; then the first non-empty line under `## Status`
/// (`complete`/`rejected`); then a `Handoff: READY-FOR-GUI` line (a
/// non-terminal `handoff`); then `open`. A terminal Result/Status always wins
/// over the handoff, so the originator's final verdict supersedes it. Returns
/// `(state, verdict)`.
fn parse_state(text: &str) -> (String, Option<String>) {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Result:") {
            let verdict = rest.trim().to_uppercase();
            if verdict == "PASS" {
                return ("complete".to_string(), Some("PASS".to_string()));
            }
            if verdict == "FAIL" {
                return ("rejected".to_string(), Some("FAIL".to_string()));
            }
        }
    }

    let mut status_value: Option<String> = None;
    let mut in_status = false;
    for line in text.lines() {
        if is_status_heading(line) {
            in_status = true;
            continue;
        }
        if !in_status {
            continue;
        }
        if is_heading(line) {
            break;
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            status_value = Some(trimmed.to_lowercase());
            break;
        }
    }

    if let Some(value) = &status_value {
        if value.contains("complete") {
            return ("complete".to_string(), None);
        }
        if value.contains("rejected") {
            return ("rejected".to_string(), None);
        }
    }

    if has_handoff(text) {
        return ("handoff".to_string(), None);
    }

    if let Some(value) = &status_value {
        if value.contains("open") {
            return ("open".to_string(), None);
        }
    }

    ("unknown".to_string(), None)
}

/// True when the report carries the one-spawn handoff sentinel on its own line —
/// `Handoff: READY-FOR-GUI` — the same signal `wait-for-report.py --phase handoff`
/// returns on.
fn has_handoff(text: &str) -> bool {
    text.lines().any(|line| {
        line.trim()
            .strip_prefix("Handoff:")
            .map(|rest| rest.trim().eq_ignore_ascii_case("READY-FOR-GUI"))
            .unwrap_or(false)
    })
}

fn format_terminal(state: &str, verdict: Option<&str>, text: &str) -> String {
    let label = match (state, verdict) {
        ("complete", Some(verdict)) => {
            format!("wait_for_report: PASS ({verdict}) — report complete")
        }
        ("complete", None) => "wait_for_report: PASS — report complete".to_string(),
        ("rejected", Some(verdict)) => {
            format!("wait_for_report: FAIL ({verdict}) — report rejected")
        }
        ("rejected", None) => "wait_for_report: FAIL — report rejected".to_string(),
        _ => format!("wait_for_report: {state}"),
    };
    let report = extract_report(text);
    if report.is_empty() {
        label
    } else {
        format!("{label}\n\n{report}")
    }
}

/// Returns the `## Report back` section, or the whole file if there isn't one.
fn extract_report(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|line| is_report_heading(line)) else {
        return text.trim().to_string();
    };
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| is_heading(line))
        .map(|(index, _)| index)
        .unwrap_or(lines.len());
    lines[start..end].join("\n").trim().to_string()
}

fn is_heading(line: &str) -> bool {
    line.trim_start().starts_with("## ")
}

fn is_status_heading(line: &str) -> bool {
    let line = line.trim();
    line.strip_prefix("##")
        .map(|rest| rest.trim().eq_ignore_ascii_case("status"))
        .unwrap_or(false)
}

fn is_report_heading(line: &str) -> bool {
    let line = line.trim();
    line.strip_prefix("##")
        .map(|rest| rest.trim().eq_ignore_ascii_case("report back"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_state_detects_pass_result_line() {
        let text = "## Report back\n\n- [x] Step 1 — ✅\n\n## Status\n\ncomplete — all passed\n\nResult: PASS\n";
        let (state, verdict) = parse_state(text);
        assert_eq!(state, "complete");
        assert_eq!(verdict.as_deref(), Some("PASS"));
    }

    #[test]
    fn parse_state_detects_fail_result_line() {
        let (state, verdict) = parse_state("## Status\n\ncomplete\n\nResult: FAIL\n");
        assert_eq!(state, "rejected");
        assert_eq!(verdict.as_deref(), Some("FAIL"));
    }

    #[test]
    fn parse_state_falls_back_to_status() {
        let (state, verdict) = parse_state("## Status\n\ncomplete\n");
        assert_eq!(state, "complete");
        assert_eq!(verdict, None);
    }

    #[test]
    fn parse_state_open_is_not_terminal() {
        let (state, _) = parse_state("## Status\n\nopen — verification pending\n");
        assert_eq!(state, "open");
    }

    #[test]
    fn parse_state_unknown_without_marker() {
        let (state, _) = parse_state("## Status\n\n\n");
        assert_eq!(state, "unknown");
    }

    #[test]
    fn parse_state_detects_handoff() {
        let text = "## Report back\n\n- [x] Step 1 — ✅\n\n## Status\n\nopen — awaiting handoff\n\nHandoff: READY-FOR-GUI\n";
        let (state, verdict) = parse_state(text);
        assert_eq!(state, "handoff");
        assert_eq!(verdict, None);
    }

    #[test]
    fn parse_state_terminal_result_beats_handoff() {
        let text = "## Status\n\nopen\n\nHandoff: READY-FOR-GUI\n\nResult: PASS\n";
        let (state, verdict) = parse_state(text);
        assert_eq!(state, "complete");
        assert_eq!(verdict.as_deref(), Some("PASS"));
    }

    #[test]
    fn parse_state_status_complete_beats_handoff() {
        let text = "## Status\n\ncomplete\n\nHandoff: READY-FOR-GUI\n";
        let (state, _) = parse_state(text);
        assert_eq!(state, "complete");
    }

    #[test]
    fn parse_state_handoff_requires_exact_sentinel() {
        // The plan template's placeholder line must not read as a real handoff.
        let text =
            "## Status\n\nopen\n\nHandoff: (agent-test writes `READY-FOR-GUI` here, then stops)\n";
        let (state, _) = parse_state(text);
        assert_eq!(state, "open");
    }

    #[test]
    fn wait_phase_defaults_to_final() {
        let input: WaitForReportToolInput =
            serde_json::from_str(r#"{"report_path":"x.md"}"#).expect("input deserializes");
        assert_eq!(input.phase, WaitPhase::Final);
    }

    #[test]
    fn wait_phase_parses_handoff() {
        let input: WaitForReportToolInput =
            serde_json::from_str(r#"{"report_path":"x.md","phase":"handoff"}"#)
                .expect("input deserializes");
        assert_eq!(input.phase, WaitPhase::Handoff);
    }

    #[test]
    fn extract_report_returns_report_back_section() {
        let text = "# Verify\n\n## Report back\n\n- [x] Step 1 — ✅\n\n## Status\n\ncomplete\n";
        let report = extract_report(text);
        assert!(report.contains("Step 1"));
        assert!(!report.contains("## Status"));
    }

    #[test]
    fn extract_report_falls_back_to_whole_file() {
        let text = "# Verify\n\nno report back section\n";
        assert_eq!(extract_report(text), text.trim());
    }
}
