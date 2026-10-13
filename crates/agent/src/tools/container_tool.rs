use std::sync::Arc;

use crate::containers::{ContainerOperation, ContainerStore, assemble_command};
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use gpui::{App, AppContext as _, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Run a Docker Compose or systemd operation on a configured remote host over
/// SSH.
///
/// Targets are defined by the operator in `containers.json` (next to
/// `automations.json`); this tool only resolves a target id to an SSH endpoint
/// and assembles the remote command. Read-only operations (`ps`/`logs`/`images`/
/// `stats`/`status`/`is_active`) run as-is; mutating operations (`up`/`down`/
/// `restart`/`exec`/`rm`/`pull`/`run`) require `confirm: true`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ContainerToolInput {
    /// The target id from `containers.json`.
    pub target: String,
    /// The operation to perform.
    pub operation: ContainerOperation,
    /// Optional compose service (or ignored for systemd targets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Extra arguments appended to the remote command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Set to `true` to confirm a mutating operation.
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContainerToolOutput {
    Success { message: String },
    Error { error: String },
}

impl From<ContainerToolOutput> for LanguageModelToolResultContent {
    fn from(value: ContainerToolOutput) -> Self {
        match value {
            ContainerToolOutput::Success { message } => message.into(),
            ContainerToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct ContainerTool;

impl AgentTool for ContainerTool {
    type Input = ContainerToolInput;
    type Output = ContainerToolOutput;

    const NAME: &'static str = "container";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Execute
    }

    fn allow_in_restricted_mode() -> bool {
        false
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => {
                format!("Container {} on {}", input.operation.as_str(), input.target).into()
            }
            Err(_) => "Running container operation".into(),
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
                .map_err(|error| ContainerToolOutput::Error {
                    error: format!("Failed to receive container tool input: {error}"),
                })?;
            run_operation(input).await
        })
    }
}

/// Performs a container operation against the configured targets on disk.
async fn run_operation(
    input: ContainerToolInput,
) -> Result<ContainerToolOutput, ContainerToolOutput> {
    let store = ContainerStore::load(ContainerStore::default_path()).map_err(|error| {
        ContainerToolOutput::Error {
            error: format!("Failed to load container config: {error}"),
        }
    })?;

    let target = store.target(&input.target).ok_or_else(|| {
        ContainerToolOutput::Error {
            error: format!(
                "Unknown container target \"{}\". Configured targets: {}",
                input.target,
                store
                    .targets
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    })?;

    if input.operation.is_mutating() && !input.confirm {
        return Err(ContainerToolOutput::Error {
            error: format!(
                "`{}` on target `{}` is destructive; re-run with `confirm: true` to proceed.",
                input.operation.as_str(),
                input.target
            ),
        });
    }

    let assembled = assemble_command(
        target,
        input.operation,
        input.service.as_deref(),
        &input.args,
    )
    .map_err(|error| ContainerToolOutput::Error {
        error: error.to_string(),
    })?;

    let output = util::command::new_command(&assembled.program)
        .args(&assembled.args)
        .output()
        .await
        .map_err(|error| ContainerToolOutput::Error {
            error: format!("Failed to run `{}`: {error}", assembled.display()),
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut message = String::new();
    if !stdout.trim().is_empty() {
        message.push_str(stdout.trim());
    }
    if !stderr.trim().is_empty() {
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(stderr.trim());
    }
    if !output.status.success() {
        return Err(ContainerToolOutput::Error {
            error: format!(
                "`{}` exited with {}: {message}",
                assembled.display(),
                output.status
            ),
        });
    }
    if message.is_empty() {
        message = format!("`{}` completed successfully.", assembled.display());
    }
    Ok(ContainerToolOutput::Success { message })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_operations_do_not_require_confirmation() {
        assert!(!ContainerOperation::Ps.is_mutating());
        assert!(!ContainerOperation::Status.is_mutating());
    }

    #[test]
    fn mutating_operations_require_confirmation() {
        assert!(ContainerOperation::Down.is_mutating());
        assert!(ContainerOperation::Exec.is_mutating());
    }
}
