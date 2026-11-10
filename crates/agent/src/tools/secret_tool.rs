//! Keyring-backed secret handling for the agent, with a hard
//! **use-not-read** invariant: no operation ever returns a secret value to
//! the model.
//!
//! The OS keyring is the only at-rest store. The standalone `secretsh` helper
//! owns all keychain access — `store`/`rotate` prompt for the value in a
//! visible terminal (echo disabled at the termios level), `run` and `delete`
//! resolve by name, and `exists` lets this tool gate mutations — so a secret
//! value never enters the Zed process at all. The model sees a *name* and the
//! command's output, never the value.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::FutureExt as _;
use gpui::{App, AsyncApp, SharedString, Task};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::thread::{ThreadEnvironment, ToolPermissionContext};
use crate::{AgentTool, SecretAuditEvent, ThreadsDatabase, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use language_model::LanguageModelToolResultContent;
use util::ResultExt as _;

/// How long to wait for the human to type the secret into the masked prompt.
const STORE_PROMPT_TIMEOUT: Duration = Duration::from_secs(180);

/// One `secret-tool` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretOperation {
    /// List stored secret names (names only, never values).
    List,
    /// Prompt the human to store a new secret value (never supplied by the model).
    Store,
    /// Run a command with the named secret injected (stdin, or a single env var).
    Run,
    /// Delete a stored secret.
    Delete,
    /// Prompt the human to replace an existing secret value.
    Rotate,
}

impl SecretOperation {
    fn as_str(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Store => "store",
            Self::Run => "run",
            Self::Delete => "delete",
            Self::Rotate => "rotate",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SecretToolInput {
    /// The operation to perform.
    pub operation: SecretOperation,
    /// The secret name (store/run/delete/rotate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Program to run with the secret injected (run only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// Arguments for `program` (run only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Inject the secret into this environment variable instead of stdin (run only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SecretToolOutput {
    Success { message: String },
    Error { error: String },
}

impl From<SecretToolOutput> for LanguageModelToolResultContent {
    fn from(value: SecretToolOutput) -> Self {
        match value {
            SecretToolOutput::Success { message } => message.into(),
            SecretToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct SecretTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl SecretTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for SecretTool {
    type Input = SecretToolInput;
    type Output = SecretToolOutput;

    const NAME: &'static str = "secret-tool";

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
            Ok(input) => format!("secret-tool {}", input.operation.as_str()).into(),
            Err(_) => "Running secret-tool operation".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let environment = self.environment.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| SecretToolOutput::Error {
                    error: format!("Failed to receive secret-tool input: {error}"),
                })?;
            run_operation(input, environment, event_stream, cx).await
        })
    }
}

async fn run_operation(
    input: SecretToolInput,
    environment: Rc<dyn ThreadEnvironment>,
    event_stream: ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<SecretToolOutput, SecretToolOutput> {
    match input.operation {
        SecretOperation::List => list(cx).await,
        SecretOperation::Store => {
            let name = required_name(&input)?;
            validate_name(&name)?;
            if secret_exists(&name).await {
                return Err(SecretToolOutput::Error {
                    error: format!(
                        "A secret named {name:?} already exists; use `rotate` to change it."
                    ),
                });
            }
            run_masked_prompt(&name, environment, &event_stream, cx).await?;
            record_event(cx, &name, "store", &event_stream).await?;
            Ok(SecretToolOutput::Success {
                message: format!("stored {name:?}"),
            })
        }
        SecretOperation::Rotate => {
            let name = required_name(&input)?;
            validate_name(&name)?;
            if !secret_exists(&name).await {
                return Err(SecretToolOutput::Error {
                    error: format!("No secret named {name:?} is stored; use `store` first."),
                });
            }
            run_masked_prompt(&name, environment, &event_stream, cx).await?;
            record_event(cx, &name, "rotate", &event_stream).await?;
            Ok(SecretToolOutput::Success {
                message: format!("rotated {name:?}"),
            })
        }
        SecretOperation::Delete => {
            let name = required_name(&input)?;
            validate_name(&name)?;
            if !secret_exists(&name).await {
                return Err(SecretToolOutput::Error {
                    error: format!("No secret named {name:?} is stored."),
                });
            }
            run_secretsh(&["delete", name.as_str()])
                .await
                .map_err(|error| SecretToolOutput::Error { error })?;
            record_event(cx, &name, "delete", &event_stream).await?;
            Ok(SecretToolOutput::Success {
                message: format!("deleted {name:?}"),
            })
        }
        SecretOperation::Run => {
            let name = required_name(&input)?;
            validate_name(&name)?;
            let program = input
                .program
                .clone()
                .ok_or_else(|| SecretToolOutput::Error {
                    error: "`run` requires a `program`.".into(),
                })?;
            if program.is_empty() {
                return Err(SecretToolOutput::Error {
                    error: "`program` must not be empty.".into(),
                });
            }
            authorize_run(&name, &program, &event_stream, cx).await?;
            let output = run_secret_command(&name, &program, &input.args, input.env_var.as_deref())
                .await
                .map_err(|error| SecretToolOutput::Error { error })?;
            record_event(cx, &name, "run", &event_stream).await?;
            Ok(SecretToolOutput::Success { message: output })
        }
    }
}

fn required_name(input: &SecretToolInput) -> Result<String, SecretToolOutput> {
    input.name.clone().ok_or_else(|| SecretToolOutput::Error {
        error: format!("`{}` requires a secret `name`.", input.operation.as_str()),
    })
}

/// A secret name becomes the keyring entry name and is embedded in the
/// `secretsh store <name>` shell command string, so it is restricted to a
/// character set with no shell metacharacters.
fn validate_name(name: &str) -> Result<(), SecretToolOutput> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if valid {
        Ok(())
    } else {
        Err(SecretToolOutput::Error {
            error: format!(
                "Invalid secret name {name:?}: use 1–128 ASCII letters, digits, `_`, `-`, or `.`."
            ),
        })
    }
}

async fn list(cx: &mut AsyncApp) -> Result<SecretToolOutput, SecretToolOutput> {
    let database = connect_db(cx).await?;
    let names = database
        .secret_names()
        .await
        .map_err(|error| SecretToolOutput::Error {
            error: format!("Failed to list stored secrets: {error}"),
        })?;

    // Drop names whose keyring entries were removed outside this tool.
    let mut present = Vec::new();
    for name in names {
        if secret_exists(&name).await {
            present.push(name);
        }
    }
    present.sort();

    let message = if present.is_empty() {
        "no secrets are stored yet".to_string()
    } else {
        present.join("\n")
    };
    Ok(SecretToolOutput::Success { message })
}

async fn connect_db(cx: &mut AsyncApp) -> Result<Arc<ThreadsDatabase>, SecretToolOutput> {
    cx.update(|cx| ThreadsDatabase::connect(cx))
        .await
        .map_err(|error| SecretToolOutput::Error {
            error: format!("Failed to connect to the thread database: {error}"),
        })
}

async fn secret_exists(name: &str) -> bool {
    run_secretsh(&["exists", name]).await.is_ok()
}

/// Ask the human to confirm the command before it runs with the named secret
/// injected. This is the control that prevents the model from exfiltrating the
/// value by pointing `run` at an arbitrary command: the user sees the exact
/// program and the secret name before it is allowed to proceed.
async fn authorize_run(
    name: &str,
    program: &str,
    event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<(), SecretToolOutput> {
    let title = format!("Run `{program}` with secret `{name}` injected");
    let context = ToolPermissionContext::new(SecretTool::NAME, vec![program.to_string()]);
    cx.update(|cx| event_stream.authorize(title, context, cx))
        .await
        .map_err(|error| SecretToolOutput::Error {
            error: format!("Secret run cancelled: {error}"),
        })
}

/// Spawn `secretsh store <name>` in a visible terminal so the human can type
/// the value with echo disabled. The value never enters this process.
async fn run_masked_prompt(
    name: &str,
    environment: Rc<dyn ThreadEnvironment>,
    _event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<(), SecretToolOutput> {
    let extra_env: Vec<acp::EnvVariable> = Vec::new();
    let terminal = environment
        .create_terminal(
            format!("secretsh store {name}"),
            extra_env,
            None,
            None,
            None,
            cx,
        )
        .await
        .map_err(|error| SecretToolOutput::Error {
            error: format!("Failed to start the secret prompt: {error:#}"),
        })?;

    let wait_for_exit = terminal
        .wait_for_exit(cx)
        .map_err(|error| SecretToolOutput::Error {
            error: format!("Failed to wait for the secret prompt: {error}"),
        })?;
    let timeout = cx.background_executor().timer(STORE_PROMPT_TIMEOUT).fuse();

    futures::select! {
        _ = wait_for_exit.clone().fuse() => {}
        _ = timeout => {
            terminal.kill(cx).log_err();
            return Err(SecretToolOutput::Error {
                error: "Timed out waiting for the secret to be entered.".into(),
            });
        }
    }

    let output = terminal
        .current_output(cx)
        .map_err(|error| SecretToolOutput::Error {
            error: format!("Failed to read the secret prompt result: {error}"),
        })?;
    if output.exit_status.and_then(|status| status.exit_code) == Some(0) {
        Ok(())
    } else {
        Err(SecretToolOutput::Error {
            error: format!(
                "Secret entry failed: {}",
                output.output.trim().lines().last().unwrap_or_default()
            ),
        })
    }
}

/// Run `secretsh run <name> [--env VAR] -- <program> <args...>`, capturing and
/// returning the scrubbed output. The secret never enters this process.
async fn run_secret_command(
    name: &str,
    program: &str,
    args: &[String],
    env_var: Option<&str>,
) -> Result<String, String> {
    let mut argv: Vec<String> = vec!["run".into(), name.to_string()];
    if let Some(var) = env_var {
        argv.push("--env".into());
        argv.push(var.to_string());
    }
    argv.push("--".into());
    argv.push(program.to_string());
    argv.extend(args.iter().cloned());

    let output = util::command::new_command("secretsh")
        .args(&argv)
        .output()
        .await
        .map_err(|error| format!("Failed to run `secretsh`: {error}"))?;

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
        return Err(format!(
            "`secretsh run {name}` exited with {}: {message}",
            output.status
        ));
    }
    if message.is_empty() {
        message = format!("`{program}` completed successfully.");
    }
    Ok(message)
}

/// Run a non-interactive `secretsh` subcommand and return its combined output.
async fn run_secretsh(args: &[&str]) -> Result<String, String> {
    let output = util::command::new_command("secretsh")
        .args(args)
        .output()
        .await
        .map_err(|error| format!("Failed to run `secretsh`: {error}"))?;

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
        return Err(message);
    }
    Ok(message)
}

async fn record_event(
    cx: &mut AsyncApp,
    name: &str,
    operation: &str,
    event_stream: &ToolCallEventStream,
) -> Result<(), SecretToolOutput> {
    let database = connect_db(cx).await?;
    let event = SecretAuditEvent {
        id: Uuid::new_v4().to_string(),
        secret_name: name.to_string(),
        operation: operation.to_string(),
        tool_call_id: event_stream.tool_call_id().0.to_string(),
        at: Utc::now().to_rfc3339(),
    };
    database
        .insert_secret_audit_event(event)
        .await
        .map_err(|error| SecretToolOutput::Error {
            error: format!("Failed to record the secret audit event: {error}"),
        })
}
