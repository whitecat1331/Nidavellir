use std::path::Path;
use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, AppContext as _, AsyncApp, Entity, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tools::terminal_tool::working_dir;
use crate::{AgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};

/// Run a Git operation in the workspace with per-operation permission rules.
///
/// Prefer this over the `terminal` tool for git's write and network
/// operations: `commit`, `checkout`, `switch`, `branch`, `fetch`, `pull`,
/// `merge`, `push`, and `tag`. Each operation is authorized through the
/// settings permission system with a structured input string
/// (`operation:<verb> [ref:<ref>] [remote:<remote>] [tag:<tag>] protected:<bool>`),
/// so "Always allow" decisions apply per operation and stick. Protected refs
/// (`feature`, `dev`, `main`) always prompt.
///
/// Read-only git inspection (`status`, `log`, `diff`, `show`) is not modeled
/// here; run it through the `terminal` tool as usual.
///
/// ### Operations
/// - `commit` — commit staged changes. Requires `message`.
/// - `checkout` / `switch` — switch to an existing ref. Requires `ref`.
/// - `branch` — create a branch at the current HEAD. Requires `ref` (the new
///   branch name).
/// - `fetch` / `pull` — update from a remote. `remote` and `ref` optional.
/// - `merge` — merge `ref` into the current branch. `no_ff` and `message`
///   optional.
/// - `push` — push to a remote. `remote` and `ref` optional.
/// - `tag` — create a lightweight tag. Requires `tag`; `ref` is an optional
///   start point.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GitToolInput {
    /// The git operation to perform.
    pub operation: GitOperation,

    /// The working directory to run git in: a project root directory or any
    /// subdirectory of one, given by name or absolute path (e.g. `zed` or
    /// `C:\Users\me\Desktop\zed`). Selects both the repository and the
    /// worktree, so pass the specific `zed-wip-*` worktree when operating on a
    /// `wip/` branch.
    pub cd: String,

    /// The ref to operate on: the ref to `checkout`/`switch`/`merge`, the
    /// branch name for `branch`, or an optional start point for `tag`.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,

    /// The remote for `fetch`/`pull`/`push` (e.g. `origin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,

    /// For `tag`: the tag name to create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,

    /// The commit message: required for `commit`, optional for `merge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// For `merge`: pass `--no-ff` to always create a merge commit.
    #[serde(default)]
    pub no_ff: bool,
}

/// A git operation. Only write and network operations are modeled; read-only
/// inspection stays on the `terminal` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitOperation {
    /// Commit staged changes (`message` required).
    Commit,
    /// Switch to an existing ref (`ref` required).
    Checkout,
    /// Switch to an existing ref (`ref` required).
    Switch,
    /// Create a branch at the current HEAD (`ref` required).
    Branch,
    /// Fetch from a remote (`remote`/`ref` optional).
    Fetch,
    /// Pull from a remote (`remote`/`ref` optional).
    Pull,
    /// Merge `ref` into the current branch (`no_ff`/`message` optional).
    Merge,
    /// Push to a remote (`remote`/`ref` optional).
    Push,
    /// Create a lightweight tag (`tag` required, `ref` optional).
    Tag,
}

impl GitOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Checkout => "checkout",
            Self::Switch => "switch",
            Self::Branch => "branch",
            Self::Fetch => "fetch",
            Self::Pull => "pull",
            Self::Merge => "merge",
            Self::Push => "push",
            Self::Tag => "tag",
        }
    }
}

/// The workspace's protected refs. `feature` is included because the personal
/// `AGENTS.md` gate covers merges to `feature` as well as `dev` and `main`.
const PROTECTED_REFS: &[&str] = &["feature", "dev", "main"];

/// Returns whether `reference` names a protected ref. A remote or qualified
/// form (`origin/dev`, `refs/heads/main`) is protected on its bare name, and
/// matching is case-insensitive.
pub fn is_protected_ref(reference: Option<&str>) -> bool {
    let Some(name) = reference.and_then(|reference| reference.rsplit('/').next()) else {
        return false;
    };
    PROTECTED_REFS
        .iter()
        .any(|protected| name.eq_ignore_ascii_case(protected))
}

/// The canonical permission input for one git operation (the settled schema):
/// `operation:<verb> [ref:<ref>] [remote:<remote>] [tag:<tag>] protected:<bool>`.
/// `protected` is always present so a settings rule can key on it directly.
pub fn git_permission_input(
    operation: &str,
    reference: Option<&str>,
    remote: Option<&str>,
    tag: Option<&str>,
) -> String {
    let mut parts = vec![format!("operation:{operation}")];
    if let Some(reference) = reference {
        parts.push(format!("ref:{reference}"));
    }
    if let Some(remote) = remote {
        parts.push(format!("remote:{remote}"));
    }
    if let Some(tag) = tag {
        parts.push(format!("tag:{tag}"));
    }
    parts.push(format!("protected:{}", is_protected_ref(reference.or(tag))));
    parts.join(" ")
}

fn required_arg(value: Option<&str>, field: &str, operation: &str) -> Result<String, String> {
    value
        .map(str::to_string)
        .ok_or_else(|| format!("`{operation}` requires the `{field}` field"))
}

/// The git argv for an operation, validating required fields up front.
fn assemble_git_args(input: &GitToolInput) -> Result<Vec<String>, String> {
    let operation = input.operation.as_str();
    let reference = input.reference.as_deref();
    let mut args: Vec<String> = vec![operation.to_string()];
    match input.operation {
        GitOperation::Commit => {
            args.push("-m".to_string());
            args.push(required_arg(input.message.as_deref(), "message", operation)?);
        }
        GitOperation::Checkout | GitOperation::Switch | GitOperation::Branch => {
            args.push(required_arg(reference, "ref", operation)?);
        }
        GitOperation::Fetch | GitOperation::Pull | GitOperation::Push => {
            if let Some(remote) = &input.remote {
                args.push(remote.clone());
            }
            if let Some(reference) = reference {
                args.push(reference.to_string());
            }
        }
        GitOperation::Merge => {
            if input.no_ff {
                args.push("--no-ff".to_string());
            }
            if let Some(message) = &input.message {
                args.push("-m".to_string());
                args.push(message.clone());
            }
            args.push(required_arg(reference, "ref", operation)?);
        }
        GitOperation::Tag => {
            args.push(required_arg(input.tag.as_deref(), "tag", operation)?);
            if let Some(reference) = reference {
                args.push(reference.to_string());
            }
        }
    }
    Ok(args)
}

/// Bound on the output returned to the model. A run whose combined output
/// exceeds this is truncated (git failure text can be arbitrarily long).
const GIT_OUTPUT_BYTE_LIMIT: usize = 64 * 1024;

pub struct GitTool {
    project: Entity<Project>,
}

impl GitTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GitToolOutput {
    Success { message: String },
    Error { error: String },
}

impl From<GitToolOutput> for LanguageModelToolResultContent {
    fn from(value: GitToolOutput) -> Self {
        match value {
            GitToolOutput::Success { message } => message.into(),
            GitToolOutput::Error { error } => error.into(),
        }
    }
}

impl AgentTool for GitTool {
    type Input = GitToolInput;
    type Output = GitToolOutput;

    const NAME: &'static str = "git";

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
            Ok(input) => format!("Git: {}", input.operation.as_str()).into(),
            Err(_) => "Git operation".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| GitToolOutput::Error {
                error: format!("Failed to receive git tool input: {error}"),
            })?;
            run_git_operation(self, input, event_stream, cx).await
        })
    }
}

async fn run_git_operation(
    tool: Arc<GitTool>,
    input: GitToolInput,
    event_stream: ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<GitToolOutput, GitToolOutput> {
    let operation = input.operation.as_str();
    let args = assemble_git_args(&input).map_err(|error| GitToolOutput::Error { error })?;

    let permission_input = git_permission_input(
        operation,
        input.reference.as_deref(),
        input.remote.as_deref(),
        input.tag.as_deref(),
    );
    authorize_git_operation(&event_stream, operation, permission_input, cx)
        .await
        .map_err(|error| GitToolOutput::Error {
            error: error.to_string(),
        })?;

    let directory = cx
        .update(|cx| working_dir(&input.cd, &tool.project, cx))
        .map_err(|error| GitToolOutput::Error {
            error: error.to_string(),
        })?
        .ok_or_else(|| GitToolOutput::Error {
            error: format!("No project root resolved for `cd` directory {:?}.", input.cd),
        })?;

    let result = cx
        .background_spawn(async move { run_git(&args, &directory).await })
        .await;
    match result {
        Ok(message) => Ok(GitToolOutput::Success { message }),
        Err(error) => Err(GitToolOutput::Error { error }),
    }
}

/// Authorize a git operation through the settings permission system.
///
/// Mirrors `debugger_tool.rs::authorize_debugger_operation`; the permission
/// input is the canonical `operation:<verb> [...] protected:<bool>` string, so
/// `always_allow`/`always_confirm`/`always_deny` rules and the prompt's
/// "Always for `operation:<verb>`" button all key on the operation rather than
/// on raw command text.
async fn authorize_git_operation(
    event_stream: &ToolCallEventStream,
    operation: &str,
    permission_input: String,
    cx: &mut AsyncApp,
) -> Result<()> {
    let title = format!("Git: {operation}");
    let task = cx.update(|cx| {
        event_stream.authorize(
            title,
            ToolPermissionContext::new(GitTool::NAME, vec![permission_input]),
            cx,
        )
    });
    task.await
}

async fn run_git(args: &[String], directory: &Path) -> Result<String, String> {
    let output = util::command::new_command("git")
        .args(args)
        .current_dir(directory)
        // Never wait on an editor or a terminal credential prompt: a git
        // invocation that would open one must fail fast with a readable error
        // instead of hanging the tool. (Stored credentials provided by the
        // configured credential helper still work.)
        .env("GIT_EDITOR", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .output()
        .await
        .map_err(|error| format!("Failed to run `git {}`: {error}", args.join(" ")))?;

    let stdout = truncate_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = truncate_output(&String::from_utf8_lossy(&output.stderr));
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

    if output.status.success() {
        if message.is_empty() {
            message = format!("`git {}` completed successfully.", args.join(" "));
        }
        Ok(message)
    } else {
        Err(format!(
            "`git {}` exited with {}: {message}",
            args.join(" "),
            output.status
        ))
    }
}

fn truncate_output(output: &str) -> String {
    if output.len() <= GIT_OUTPUT_BYTE_LIMIT {
        return output.to_string();
    }
    let mut end = GIT_OUTPUT_BYTE_LIMIT;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[... output truncated at {} bytes ...]",
        &output[..end],
        GIT_OUTPUT_BYTE_LIMIT
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_input(
        operation: GitOperation,
        reference: Option<&str>,
        remote: Option<&str>,
        tag: Option<&str>,
        message: Option<&str>,
        no_ff: bool,
    ) -> GitToolInput {
        GitToolInput {
            operation,
            cd: "zed".to_string(),
            reference: reference.map(str::to_string),
            remote: remote.map(str::to_string),
            tag: tag.map(str::to_string),
            message: message.map(str::to_string),
            no_ff,
        }
    }

    #[test]
    fn permission_input_is_canonical_and_flags_protected_refs() {
        assert_eq!(
            git_permission_input("push", Some("feature"), Some("origin"), None),
            "operation:push ref:feature remote:origin protected:true"
        );
        assert_eq!(
            git_permission_input("merge", Some("wip/foo"), None, None),
            "operation:merge ref:wip/foo protected:false"
        );
        assert_eq!(
            git_permission_input("tag", None, None, Some("dev")),
            "operation:tag tag:dev protected:true"
        );
        assert_eq!(
            git_permission_input("commit", None, None, None),
            "operation:commit protected:false"
        );
        assert_eq!(
            git_permission_input("fetch", None, Some("origin"), None),
            "operation:fetch remote:origin protected:false"
        );
    }

    #[test]
    fn protected_ref_predicate_matches_final_component_case_insensitively() {
        assert!(is_protected_ref(Some("feature")));
        assert!(is_protected_ref(Some("dev")));
        assert!(is_protected_ref(Some("main")));
        assert!(is_protected_ref(Some("origin/dev")));
        assert!(is_protected_ref(Some("refs/heads/main")));
        assert!(is_protected_ref(Some("DEV")));
        assert!(!is_protected_ref(Some("wip/feature-tooling")));
        assert!(!is_protected_ref(Some("feature/span")));
        assert!(!is_protected_ref(Some("")));
        assert!(!is_protected_ref(None));
    }

    #[test]
    fn argument_assembly_validates_required_fields() {
        let commit = test_input(GitOperation::Commit, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&commit),
            Err("`commit` requires the `message` field".to_string())
        );

        let commit = test_input(GitOperation::Commit, None, None, None, Some("msg"), false);
        assert_eq!(
            assemble_git_args(&commit).unwrap(),
            vec!["commit", "-m", "msg"]
        );

        let merge = test_input(
            GitOperation::Merge,
            Some("wip/foo"),
            None,
            None,
            Some("Merge wip/foo"),
            true,
        );
        assert_eq!(
            assemble_git_args(&merge).unwrap(),
            vec!["merge", "--no-ff", "-m", "Merge wip/foo", "wip/foo"]
        );

        let push = test_input(
            GitOperation::Push,
            Some("feature"),
            Some("origin"),
            None,
            None,
            false,
        );
        assert_eq!(
            assemble_git_args(&push).unwrap(),
            vec!["push", "origin", "feature"]
        );

        let tag = test_input(GitOperation::Tag, Some("main"), None, Some("v1"), None, false);
        assert_eq!(assemble_git_args(&tag).unwrap(), vec!["tag", "v1", "main"]);

        let checkout = test_input(GitOperation::Checkout, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&checkout),
            Err("`checkout` requires the `ref` field".to_string())
        );

        let tag = test_input(GitOperation::Tag, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&tag),
            Err("`tag` requires the `tag` field".to_string())
        );
    }

    #[test]
    fn output_truncation_respects_char_boundaries() {
        assert_eq!(truncate_output("ok"), "ok");

        // A multi-byte character straddling the byte limit must not be split.
        let long = format!("{}é", "a".repeat(GIT_OUTPUT_BYTE_LIMIT - 1));
        let truncated = truncate_output(&long);
        assert!(truncated.starts_with(&"a".repeat(GIT_OUTPUT_BYTE_LIMIT - 1)));
        assert!(truncated.ends_with("[... output truncated at 65536 bytes ...]"));
    }
}
