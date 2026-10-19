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
/// `merge`, `push`, `tag`, `add`, `reset`, `restore`, `stash`, `worktree`,
/// `clone`, `init`, `remote`, `rebase`, `cherry-pick`, `revert`, `status`,
/// `log`, `diff`, and `show`. Each
/// operation is authorized through the settings permission system with a
/// structured input string
/// (`operation:<verb> [ref:<ref>] [remote:<remote>] [tag:<tag>] protected:<bool>`),
/// so "Always allow" decisions apply per operation and stick. Protected refs
/// (`feature`, `dev`, `main`) always prompt; `reset`/`restore`/`rebase`/
/// `cherry-pick`/`revert` additionally prompt whenever the *current* branch
/// is protected, because they are history-destructive.
///
/// Interactive git forms (`add -p`, `rebase -i`, …) are never modeled — flag
/// pathspecs and flag arguments are refused at argv level.
///
/// Read-only git inspection (`status`, `log`, `diff`, `show`) is not modeled
/// here; run it through the `terminal` tool as usual.
///
/// ### Operations
/// - `commit` — commit staged changes. Requires `message`.
/// - `checkout` / `switch` — switch to an existing ref. Requires `ref`.
/// - `branch` — create a branch at the current HEAD (`ref` = new branch
///   name), list branches without `ref`, or delete one (`delete` passes
///   `-d`; `force` upgrades to `-D`; the deleted ref's protected
///   boundary applies).
/// - `fetch` / `pull` — update from a remote. `remote` and `ref` optional;
///   `refspec` passes an explicit refspec (flag refspecs are refused).
/// - `merge` — merge `ref` into the current branch. `no_ff` and `message`
///   optional.
/// - `push` — push to a remote. `remote` and `ref` optional; `delete`
///   passes `--delete` (requires `ref`; the protected-ref boundary
///   applies to the ref being deleted).
/// - `tag` — create a lightweight tag. Requires `tag`; `ref` is an optional
///   start point.
/// - `add` — stage changes. Requires `pathspecs`; only the non-interactive
///   flags `-A`/`--all` and `-u`/`--update` are accepted.
/// - `reset` — reset HEAD to an optional `ref`, with an optional `mode`
///   (`soft`/`mixed`/`hard`). Prompted while HEAD is a protected branch.
/// - `restore` — restore working-tree files. Requires `pathspecs`; `staged`
///   passes `--staged`, `source` passes `--source=<ref>`. Prompted while HEAD
///   is a protected branch.
/// - `stash` — stash changes (`push` with an optional `message`, `pop`, or
///   `apply`).
/// - `worktree` — manage worktrees (`add`/`remove`/`list`/`prune`). `add`
///   requires `ref` (the new branch, passed with `-b`) and `destination`
///   (an absolute path), with `source` as the optional start point;
///   `remove` requires `destination`, and `force` passes `--force`;
///   `list` lists worktrees; `prune` prunes stale worktree records.
/// - `clone` — clone `url` into `destination` (both required; `destination`
///   absolute). External-facing: joins `fetch`/`pull`/`push` in the
///   restricted-mode set.
/// - `init` — initialize a repository at `destination` (required,
///   absolute); `bare` passes `--bare`.
/// - `remote` — manage remotes (`add`/`remove`/`rename`/`set-url`/`list`).
///   `remote` is the remote name; `url` for `add`/`set-url`; `new_name`
///   for `rename`; `list` prints the remote names.
/// - `rebase` — rebase the current branch onto `ref` (required). Interactive
///   forms are refused; prompted while HEAD is a protected branch.
/// - `cherry-pick` — apply the commit `ref` onto the current branch
///   (required). Prompted while HEAD is a protected branch.
/// - `revert` — revert the commit `ref` on the current branch (required;
///   `--no-edit` is always passed). Prompted while HEAD is a protected
///   branch.
/// - `status` — working-tree status (`--short --branch`). Read-only.
/// - `log` — commit history (`--oneline -n <count>`, default 20; optional
///   `ref` as the start point). Read-only.
/// - `diff` — working-tree diff (optional `staged`, `ref`, `pathspecs`;
///   flag pathspecs are refused). Read-only.
/// - `show` — show a commit (optional `ref`; HEAD when omitted).
///   Read-only.
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

    /// The ref to operate on: the ref to `checkout`/`switch`/`merge`/`reset`,
    /// the branch name for `branch`, the branch to delete for
    /// `branch --delete`, the new branch name for `worktree add` (passed
    /// with `-b`), the upstream for `rebase`, the commit for
    /// `cherry-pick`/`revert`, the start point for `log`/`diff`/`show`, or
    /// an optional start point for `tag`.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,

    /// The remote for `fetch`/`pull`/`push`, or the remote name for
    /// `remote` (e.g. `origin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,

    /// For `tag`: the tag name to create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,

    /// The commit message: required for `commit`, optional for `merge`, and
    /// the message for `stash push`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// For `merge`: pass `--no-ff` to always create a merge commit.
    #[serde(default)]
    pub no_ff: bool,

    /// For `push`/`branch`: pass `--delete` to delete the ref instead of
    /// updating or creating it (`ref` required).
    #[serde(default)]
    pub delete: bool,

    /// For `log`: the number of commits to show (`-n`; defaults to 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,

    /// For `fetch`/`pull`: an explicit refspec (e.g.
    /// `refs/heads/wip/foo:refs/remotes/origin/wip/foo`), appended after
    /// `remote`. Flag refspecs are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refspec: Option<String>,

    /// For `init`: pass `--bare` to create a bare repository.
    #[serde(default)]
    pub bare: bool,

    /// For `add`/`restore`: the pathspecs to stage or restore (required).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pathspecs: Option<Vec<String>>,

    /// For `reset`: the reset mode (`soft`/`mixed`/`hard`; defaults to
    /// `mixed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ResetMode>,

    /// For `restore`: pass `--staged` to unstage instead of restoring the
    /// working tree.
    #[serde(default)]
    pub staged: bool,

    /// For `restore`: the ref to restore from (`--source=`). For
    /// `worktree add`: the optional start point for the new branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// For `stash`: which stash command to run (`push` default, `pop`,
    /// `apply`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stash_subcommand: Option<StashSubcommand>,

    /// For `worktree add`/`worktree remove`, `clone`, and `init`: the
    /// destination path (required, absolute). `worktree add` writes the new
    /// worktree here; `clone` and `init` create the repository here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,

    /// For `clone`: the repository URL to clone from. For `remote add` and
    /// `remote set-url`: the remote URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// For `remote rename`: the new remote name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,

    /// For `worktree remove`: pass `--force` to remove a worktree with
    /// uncommitted changes. For `branch --delete`, `force` passes `-D`
    /// (delete even if unmerged).
    #[serde(default)]
    pub force: bool,

    /// For `worktree`: which subcommand to run (`add` default, `remove`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_subcommand: Option<WorktreeSubcommand>,

    /// For `remote`: which subcommand to run (`add` default, `remove`,
    /// `rename`, `set-url`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_subcommand: Option<RemoteSubcommand>,
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
    /// Stage changes (`pathspecs` required; non-interactive flags only).
    Add,
    /// Reset HEAD to an optional `ref` (`mode` optional). Prompted while HEAD
    /// is a protected branch.
    Reset,
    /// Restore working-tree files (`pathspecs` required; `staged`/`source`
    /// optional). Prompted while HEAD is a protected branch.
    Restore,
    /// Stash changes (`push` with an optional `message`, `pop`, or `apply`).
    Stash,
    /// Manage worktrees (`add` requires `ref` + `destination`; `remove`
    /// requires `destination`, `force` optional).
    Worktree,
    /// Clone `url` into `destination` (both required). External-facing.
    Clone,
    /// Initialize a repository at `destination` (required).
    Init,
    /// Manage remotes (`add`/`remove`/`rename`/`set-url`).
    Remote,
    /// Rebase the current branch onto `ref` (required; interactive forms
    /// refused). Prompted while HEAD is a protected branch.
    Rebase,
    /// Apply the commit `ref` onto the current branch (required). Prompted
    /// while HEAD is a protected branch.
    CherryPick,
    /// Revert the commit `ref` on the current branch (required; `--no-edit`
    /// is always passed). Prompted while HEAD is a protected branch.
    Revert,
    /// Working-tree status (`--short --branch`). Read-only.
    Status,
    /// Commit history (`--oneline -n <count>`). Read-only.
    Log,
    /// Working-tree diff (optional `staged`/`ref`/`pathspecs`). Read-only.
    Diff,
    /// Show a commit (`ref` optional, HEAD when omitted). Read-only.
    Show,
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
            Self::Add => "add",
            Self::Reset => "reset",
            Self::Restore => "restore",
            Self::Stash => "stash",
            Self::Worktree => "worktree",
            Self::Clone => "clone",
            Self::Init => "init",
            Self::Remote => "remote",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Status => "status",
            Self::Log => "log",
            Self::Diff => "diff",
            Self::Show => "show",
        }
    }
}

/// The `git reset` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResetMode {
    /// Move HEAD only, leaving the index and working tree intact.
    Soft,
    /// Move HEAD and reset the index (the default).
    Mixed,
    /// Move HEAD, reset the index, and discard working-tree changes.
    Hard,
}

impl ResetMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Mixed => "mixed",
            Self::Hard => "hard",
        }
    }
}

/// The `git stash` subcommand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StashSubcommand {
    /// Stash changes (default); `message` is passed as `-m`.
    #[default]
    Push,
    /// Apply and drop the most recent stash.
    Pop,
    /// Apply the most recent stash without dropping it.
    Apply,
}

impl StashSubcommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Pop => "pop",
            Self::Apply => "apply",
        }
    }
}

/// The `git worktree` subcommand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeSubcommand {
    /// Create a worktree from `-b` `ref` at `destination` (default).
    #[default]
    Add,
    /// Remove the worktree at `destination`; `force` passes `--force`.
    Remove,
    /// List worktrees.
    List,
    /// Prune stale worktree records.
    Prune,
}

impl WorktreeSubcommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::List => "list",
            Self::Prune => "prune",
        }
    }
}

/// The `git remote` subcommand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSubcommand {
    /// Add a remote named `remote` pointing at `url` (default).
    #[default]
    Add,
    /// Remove the remote named `remote`.
    Remove,
    /// Rename the remote `remote` to `new_name`.
    Rename,
    /// Point the remote `remote` at `url`.
    SetUrl,
    /// List remote names.
    List,
}

impl RemoteSubcommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Rename => "rename",
            Self::SetUrl => "set-url",
            Self::List => "list",
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

/// The permission input for operations gated on the *current* branch:
/// `operation:<verb> ref:<HEAD-branch> protected:<bool>`. `reset`/`restore`/
/// `rebase`/`cherry-pick`/`revert` are history-destructive, so they prompt
/// whenever HEAD is a protected ref — regardless of which ref the operation
/// targets.
pub fn current_branch_permission_input(operation: &str, current_branch: &str) -> String {
    format!(
        "operation:{operation} ref:{current_branch} protected:{}",
        is_protected_ref(Some(current_branch))
    )
}

fn required_arg(value: Option<&str>, field: &str, operation: &str) -> Result<String, String> {
    value
        .map(str::to_string)
        .ok_or_else(|| format!("`{operation}` requires the `{field}` field"))
}

/// The commit/upstream argument for the history ops. Leading dashes are
/// refused so interactive forms (`rebase -i`) and unsupported flags can never
/// reach git argv through a structured field.
fn required_history_arg(
    value: Option<&str>,
    field: &str,
    operation: &str,
) -> Result<String, String> {
    let argument = required_arg(value, field, operation)?;
    if argument.starts_with('-') {
        return Err(format!(
            "`{operation}` refuses flag argument {argument:?}: interactive forms (`-i`) and unsupported flags are not modeled by the git tool"
        ));
    }
    Ok(argument)
}

/// Non-interactive flags `git add` may receive; anything else starting with
/// `-` is refused so interactive forms (`add -p`, `add -i`) and unknown flags
/// can never reach git argv.
const ADD_FLAG_ALLOWLIST: &[&str] = &["-A", "--all", "-u", "--update"];

/// The destination path for `worktree add`/`worktree remove`, `clone`, and
/// `init`. It must be absolute — a destination is a fresh filesystem location
/// (a new worktree, a new clone, a new repo), not a project root, so it is
/// never resolved through `working_dir`.
fn required_destination(input: &GitToolInput, operation: &str) -> Result<String, String> {
    let destination = input
        .destination
        .as_deref()
        .ok_or_else(|| format!("`{operation}` requires the `destination` field"))?;
    if destination.trim().is_empty() {
        return Err(format!("`{operation}` requires a non-empty `destination`"));
    }
    if !Path::new(destination).is_absolute() {
        return Err(format!(
            "`{operation}` requires an absolute `destination` path (got {destination:?})"
        ));
    }
    Ok(destination.to_string())
}

fn required_pathspecs(input: &GitToolInput, operation: &str) -> Result<Vec<String>, String> {
    let pathspecs = input
        .pathspecs
        .as_ref()
        .ok_or_else(|| format!("`{operation}` requires the `pathspecs` field"))?;
    if pathspecs.is_empty() {
        return Err(format!("`{operation}` requires at least one pathspec"));
    }
    let flag_allowlist: &[&str] = if operation == "add" {
        ADD_FLAG_ALLOWLIST
    } else {
        &[]
    };
    for pathspec in pathspecs {
        if pathspec.starts_with('-') && !flag_allowlist.contains(&pathspec.as_str()) {
            return Err(format!(
                "`{operation}` refuses flag pathspec {pathspec:?}: interactive forms (`-p`, `-i`) and unsupported flags are not modeled by the git tool"
            ));
        }
    }
    Ok(pathspecs.clone())
}

/// The git argv for an operation, validating required fields up front.
fn assemble_git_args(input: &GitToolInput) -> Result<Vec<String>, String> {
    let operation = input.operation.as_str();
    let reference = input.reference.as_deref();
    if input.delete && !matches!(input.operation, GitOperation::Push | GitOperation::Branch) {
        return Err(format!(
            "`{operation}` does not support the `delete` field (push only)"
        ));
    }
    let mut args: Vec<String> = vec![operation.to_string()];
    match input.operation {
        GitOperation::Commit => {
            args.push("-m".to_string());
            args.push(required_arg(
                input.message.as_deref(),
                "message",
                operation,
            )?);
        }
        GitOperation::Checkout | GitOperation::Switch => {
            args.push(required_arg(reference, "ref", operation)?);
        }
        GitOperation::Branch => {
            if input.delete {
                args.push(if input.force { "-D" } else { "-d" }.to_string());
                args.push(required_arg(reference, "ref", operation)?);
            } else if let Some(reference) = reference {
                args.push(reference.to_string());
            } else {
                args.push("--list".to_string());
            }
        }
        GitOperation::Fetch | GitOperation::Pull => {
            if let Some(remote) = &input.remote {
                args.push(remote.clone());
            }
            if let Some(reference) = reference {
                args.push(reference.to_string());
            }
            if let Some(refspec) = &input.refspec {
                args.push(required_history_arg(
                    Some(refspec.as_str()),
                    "refspec",
                    operation,
                )?);
            }
        }
        GitOperation::Push => {
            if input.delete {
                args.push("--delete".to_string());
                required_arg(reference, "ref", operation)?;
            }
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
        GitOperation::Add => {
            args.extend(required_pathspecs(input, operation)?);
        }
        GitOperation::Reset => {
            if let Some(mode) = input.mode {
                args.push(format!("--{}", mode.as_str()));
            }
            if let Some(reference) = reference {
                args.push(reference.to_string());
            }
        }
        GitOperation::Restore => {
            if input.staged {
                args.push("--staged".to_string());
            }
            if let Some(source) = &input.source {
                args.push(format!("--source={source}"));
            }
            args.extend(required_pathspecs(input, operation)?);
        }
        GitOperation::Stash => match input.stash_subcommand.unwrap_or_default() {
            StashSubcommand::Push => {
                if let Some(message) = &input.message {
                    args.push("push".to_string());
                    args.push("-m".to_string());
                    args.push(message.clone());
                }
            }
            StashSubcommand::Pop => args.push("pop".to_string()),
            StashSubcommand::Apply => args.push("apply".to_string()),
        },
        GitOperation::Worktree => match input.worktree_subcommand.unwrap_or_default() {
            WorktreeSubcommand::Add => {
                args.push("add".to_string());
                args.push("-b".to_string());
                args.push(required_arg(reference, "ref", operation)?);
                args.push(required_destination(input, operation)?);
                if let Some(start_point) = &input.source {
                    args.push(start_point.clone());
                }
            }
            WorktreeSubcommand::Remove => {
                args.push("remove".to_string());
                if input.force {
                    args.push("--force".to_string());
                }
                args.push(required_destination(input, operation)?);
            }
            WorktreeSubcommand::List => args.push("list".to_string()),
            WorktreeSubcommand::Prune => args.push("prune".to_string()),
        },
        GitOperation::Clone => {
            args.push(required_arg(input.url.as_deref(), "url", operation)?);
            args.push(required_destination(input, operation)?);
        }
        GitOperation::Init => {
            if input.bare {
                args.push("--bare".to_string());
            }
            args.push(required_destination(input, operation)?);
        }
        GitOperation::Remote => match input.remote_subcommand.unwrap_or_default() {
            RemoteSubcommand::Add => {
                args.push("add".to_string());
                args.push(required_arg(input.remote.as_deref(), "remote", operation)?);
                args.push(required_arg(input.url.as_deref(), "url", operation)?);
            }
            RemoteSubcommand::Remove => {
                args.push("remove".to_string());
                args.push(required_arg(input.remote.as_deref(), "remote", operation)?);
            }
            RemoteSubcommand::Rename => {
                args.push("rename".to_string());
                args.push(required_arg(input.remote.as_deref(), "remote", operation)?);
                args.push(required_arg(
                    input.new_name.as_deref(),
                    "new_name",
                    operation,
                )?);
            }
            RemoteSubcommand::SetUrl => {
                args.push("set-url".to_string());
                args.push(required_arg(input.remote.as_deref(), "remote", operation)?);
                args.push(required_arg(input.url.as_deref(), "url", operation)?);
            }
            RemoteSubcommand::List => {}
        },
        GitOperation::Rebase => {
            args.push(required_history_arg(reference, "ref", operation)?);
        }
        GitOperation::CherryPick => {
            args.push(required_history_arg(reference, "ref", operation)?);
        }
        GitOperation::Revert => {
            // A revert opens the commit-message editor by default; the tool
            // has no TTY, so the editor is skipped.
            args.push("--no-edit".to_string());
            args.push(required_history_arg(reference, "ref", operation)?);
        }
        GitOperation::Status => {
            args.push("--short".to_string());
            args.push("--branch".to_string());
        }
        GitOperation::Log => {
            args.push("--oneline".to_string());
            args.push("-n".to_string());
            args.push(input.count.unwrap_or(20).to_string());
            if let Some(reference) = reference {
                args.push(reference.to_string());
            }
        }
        GitOperation::Diff => {
            if input.staged {
                args.push("--staged".to_string());
            }
            if let Some(reference) = reference {
                args.push(reference.to_string());
            }
            if let Some(pathspecs) = &input.pathspecs {
                for pathspec in pathspecs {
                    if pathspec.starts_with('-') {
                        return Err(format!(
                            "`diff` refuses flag pathspec {pathspec:?}: flags are not modeled by the git tool"
                        ));
                    }
                    args.push(pathspec.clone());
                }
            }
        }
        GitOperation::Show => {
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

    let directory = cx
        .update(|cx| working_dir(&input.cd, &tool.project, cx))
        .map_err(|error| GitToolOutput::Error {
            error: error.to_string(),
        })?
        .ok_or_else(|| GitToolOutput::Error {
            error: format!(
                "No project root resolved for `cd` directory {:?}.",
                input.cd
            ),
        })?;

    // `reset`/`restore`/`rebase`/`cherry-pick`/`revert` are gated on the
    // *current* branch, which is only known after the working directory is
    // resolved.
    let permission_input = match input.operation {
        GitOperation::Reset
        | GitOperation::Restore
        | GitOperation::Rebase
        | GitOperation::CherryPick
        | GitOperation::Revert => {
            let current_branch = cx
                .background_spawn({
                    let directory = directory.clone();
                    async move {
                        run_git(
                            &[
                                "rev-parse".to_string(),
                                "--abbrev-ref".to_string(),
                                "HEAD".to_string(),
                            ],
                            &directory,
                        )
                        .await
                    }
                })
                .await
                .map_err(|error| GitToolOutput::Error {
                    error: format!(
                        "Failed to resolve the current branch for `{operation}`: {error}"
                    ),
                })?;
            current_branch_permission_input(operation, current_branch.trim())
        }
        // `add`/`stash`/`worktree`/`init` never take a ref, remote, or tag:
        // keep their permission input free of stray fields so rules key on
        // the operation alone.
        GitOperation::Add
        | GitOperation::Stash
        | GitOperation::Worktree
        | GitOperation::Init
        | GitOperation::Status
        | GitOperation::Log
        | GitOperation::Diff
        | GitOperation::Show => {
            git_permission_input(operation, None, None, None)
        }
        // `clone` keys on the source URL; `remote` on the remote name.
        GitOperation::Clone => git_permission_input(operation, None, input.url.as_deref(), None),
        GitOperation::Remote => {
            git_permission_input(operation, None, input.remote.as_deref(), None)
        }
        _ => git_permission_input(
            operation,
            input.reference.as_deref(),
            input.remote.as_deref(),
            input.tag.as_deref(),
        ),
    };
    authorize_git_operation(&event_stream, operation, permission_input, cx)
        .await
        .map_err(|error| GitToolOutput::Error {
            error: error.to_string(),
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
            delete: false,
            count: None,
            refspec: None,
            bare: false,
            pathspecs: None,
            mode: None,
            staged: false,
            source: None,
            stash_subcommand: None,
            destination: None,
            url: None,
            new_name: None,
            force: false,
            worktree_subcommand: None,
            remote_subcommand: None,
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

        let tag = test_input(
            GitOperation::Tag,
            Some("main"),
            None,
            Some("v1"),
            None,
            false,
        );
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
    fn current_branch_permission_input_flags_protected_head() {
        assert_eq!(
            current_branch_permission_input("reset", "feature"),
            "operation:reset ref:feature protected:true"
        );
        assert_eq!(
            current_branch_permission_input("restore", "dev"),
            "operation:restore ref:dev protected:true"
        );
        assert_eq!(
            current_branch_permission_input("reset", "MAIN"),
            "operation:reset ref:MAIN protected:true"
        );
        assert_eq!(
            current_branch_permission_input("reset", "wip/native-git-tool-phase7"),
            "operation:reset ref:wip/native-git-tool-phase7 protected:false"
        );
        assert_eq!(
            current_branch_permission_input("restore", "HEAD"),
            "operation:restore ref:HEAD protected:false"
        );
    }

    #[test]
    fn staging_and_undo_argument_assembly_validates_fields() {
        let add = GitToolInput {
            pathspecs: Some(vec!["crates/agent".to_string()]),
            ..test_input(GitOperation::Add, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&add).unwrap(),
            vec!["add", "crates/agent"]
        );

        let add_all = GitToolInput {
            pathspecs: Some(vec!["-A".to_string()]),
            ..test_input(GitOperation::Add, None, None, None, None, false)
        };
        assert_eq!(assemble_git_args(&add_all).unwrap(), vec!["add", "-A"]);

        for interactive in ["-p", "-i", "--patch", "--interactive", "--unknown"] {
            let input = GitToolInput {
                pathspecs: Some(vec![interactive.to_string()]),
                ..test_input(GitOperation::Add, None, None, None, None, false)
            };
            assert!(
                assemble_git_args(&input).unwrap_err().contains("refuses"),
                "flag pathspec {interactive:?} should be refused"
            );
        }

        let add_missing = test_input(GitOperation::Add, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&add_missing),
            Err("`add` requires the `pathspecs` field".to_string())
        );

        let add_empty = GitToolInput {
            pathspecs: Some(vec![]),
            ..test_input(GitOperation::Add, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&add_empty),
            Err("`add` requires at least one pathspec".to_string())
        );

        let reset_hard = GitToolInput {
            mode: Some(ResetMode::Hard),
            ..test_input(GitOperation::Reset, Some("HEAD~1"), None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&reset_hard).unwrap(),
            vec!["reset", "--hard", "HEAD~1"]
        );

        let reset_bare = test_input(GitOperation::Reset, None, None, None, None, false);
        assert_eq!(assemble_git_args(&reset_bare).unwrap(), vec!["reset"]);

        let restore_staged = GitToolInput {
            staged: true,
            source: Some("HEAD~1".to_string()),
            pathspecs: Some(vec!["src".to_string()]),
            ..test_input(GitOperation::Restore, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&restore_staged).unwrap(),
            vec!["restore", "--staged", "--source=HEAD~1", "src"]
        );

        let restore_interactive = GitToolInput {
            pathspecs: Some(vec!["-p".to_string()]),
            ..test_input(GitOperation::Restore, None, None, None, None, false)
        };
        assert!(
            assemble_git_args(&restore_interactive)
                .unwrap_err()
                .contains("refuses")
        );

        let stash_bare = test_input(GitOperation::Stash, None, None, None, None, false);
        assert_eq!(assemble_git_args(&stash_bare).unwrap(), vec!["stash"]);

        let stash_push = test_input(GitOperation::Stash, None, None, None, Some("wip"), false);
        assert_eq!(
            assemble_git_args(&stash_push).unwrap(),
            vec!["stash", "push", "-m", "wip"]
        );

        let stash_pop = GitToolInput {
            stash_subcommand: Some(StashSubcommand::Pop),
            ..test_input(GitOperation::Stash, None, None, None, None, false)
        };
        assert_eq!(assemble_git_args(&stash_pop).unwrap(), vec!["stash", "pop"]);

        let stash_apply = GitToolInput {
            stash_subcommand: Some(StashSubcommand::Apply),
            ..test_input(GitOperation::Stash, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&stash_apply).unwrap(),
            vec!["stash", "apply"]
        );
    }

    #[test]
    fn lifecycle_permission_inputs() {
        assert_eq!(
            git_permission_input("worktree", None, None, None),
            "operation:worktree protected:false"
        );
        assert_eq!(
            git_permission_input("init", None, None, None),
            "operation:init protected:false"
        );
        assert_eq!(
            git_permission_input(
                "clone",
                None,
                Some("https://github.com/example/repo.git"),
                None
            ),
            "operation:clone remote:https://github.com/example/repo.git protected:false"
        );
        assert_eq!(
            git_permission_input("remote", None, Some("origin"), None),
            "operation:remote remote:origin protected:false"
        );
    }

    #[test]
    fn lifecycle_argument_assembly_validates_fields() {
        let destination = std::env::temp_dir()
            .join("git-tool-acceptance")
            .to_string_lossy()
            .to_string();

        let worktree_add = GitToolInput {
            destination: Some(destination.clone()),
            source: Some("feature".to_string()),
            ..test_input(
                GitOperation::Worktree,
                Some("wip/foo"),
                None,
                None,
                None,
                false,
            )
        };
        assert_eq!(
            assemble_git_args(&worktree_add).unwrap(),
            vec![
                "worktree",
                "add",
                "-b",
                "wip/foo",
                destination.as_str(),
                "feature"
            ]
        );

        let worktree_add_missing_ref =
            test_input(GitOperation::Worktree, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&worktree_add_missing_ref),
            Err("`worktree` requires the `ref` field".to_string())
        );

        let worktree_add_missing_destination = test_input(
            GitOperation::Worktree,
            Some("wip/foo"),
            None,
            None,
            None,
            false,
        );
        assert_eq!(
            assemble_git_args(&worktree_add_missing_destination),
            Err("`worktree` requires the `destination` field".to_string())
        );

        let worktree_add_relative = GitToolInput {
            destination: Some("zed-wip-foo".to_string()),
            ..test_input(
                GitOperation::Worktree,
                Some("wip/foo"),
                None,
                None,
                None,
                false,
            )
        };
        assert!(
            assemble_git_args(&worktree_add_relative)
                .unwrap_err()
                .contains("absolute `destination` path")
        );

        let worktree_remove = GitToolInput {
            destination: Some(destination.clone()),
            force: true,
            worktree_subcommand: Some(WorktreeSubcommand::Remove),
            ..test_input(GitOperation::Worktree, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&worktree_remove).unwrap(),
            vec!["worktree", "remove", "--force", destination.as_str()]
        );

        let clone = GitToolInput {
            destination: Some(destination.clone()),
            url: Some("https://github.com/example/repo.git".to_string()),
            ..test_input(GitOperation::Clone, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&clone).unwrap(),
            vec![
                "clone",
                "https://github.com/example/repo.git",
                destination.as_str()
            ]
        );

        let clone_missing_url = GitToolInput {
            destination: Some(destination.clone()),
            ..test_input(GitOperation::Clone, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&clone_missing_url),
            Err("`clone` requires the `url` field".to_string())
        );

        let clone_missing_destination = GitToolInput {
            url: Some("https://github.com/example/repo.git".to_string()),
            ..test_input(GitOperation::Clone, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&clone_missing_destination),
            Err("`clone` requires the `destination` field".to_string())
        );

        let clone_relative = GitToolInput {
            destination: Some("new-repo".to_string()),
            url: Some("https://github.com/example/repo.git".to_string()),
            ..test_input(GitOperation::Clone, None, None, None, None, false)
        };
        assert!(
            assemble_git_args(&clone_relative)
                .unwrap_err()
                .contains("absolute `destination` path")
        );

        let init = GitToolInput {
            destination: Some(destination.clone()),
            ..test_input(GitOperation::Init, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&init).unwrap(),
            vec!["init", destination.as_str()]
        );

        let init_missing = test_input(GitOperation::Init, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&init_missing),
            Err("`init` requires the `destination` field".to_string())
        );

        let remote_add = GitToolInput {
            remote: Some("origin".to_string()),
            url: Some("https://github.com/example/repo.git".to_string()),
            ..test_input(GitOperation::Remote, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&remote_add).unwrap(),
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/example/repo.git"
            ]
        );

        let remote_add_missing_url = GitToolInput {
            remote: Some("origin".to_string()),
            ..test_input(GitOperation::Remote, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&remote_add_missing_url),
            Err("`remote` requires the `url` field".to_string())
        );

        let remote_remove = GitToolInput {
            remote: Some("origin".to_string()),
            remote_subcommand: Some(RemoteSubcommand::Remove),
            ..test_input(GitOperation::Remote, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&remote_remove).unwrap(),
            vec!["remote", "remove", "origin"]
        );

        let remote_rename = GitToolInput {
            remote: Some("origin".to_string()),
            new_name: Some("upstream".to_string()),
            remote_subcommand: Some(RemoteSubcommand::Rename),
            ..test_input(GitOperation::Remote, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&remote_rename).unwrap(),
            vec!["remote", "rename", "origin", "upstream"]
        );

        let remote_rename_missing_new_name = GitToolInput {
            remote: Some("origin".to_string()),
            remote_subcommand: Some(RemoteSubcommand::Rename),
            ..test_input(GitOperation::Remote, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&remote_rename_missing_new_name),
            Err("`remote` requires the `new_name` field".to_string())
        );

        let remote_set_url = GitToolInput {
            remote: Some("origin".to_string()),
            url: Some("https://github.com/example/repo.git".to_string()),
            remote_subcommand: Some(RemoteSubcommand::SetUrl),
            ..test_input(GitOperation::Remote, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&remote_set_url).unwrap(),
            vec![
                "remote",
                "set-url",
                "origin",
                "https://github.com/example/repo.git"
            ]
        );
    }

    #[test]
    fn history_permission_inputs() {
        assert_eq!(
            current_branch_permission_input("rebase", "feature"),
            "operation:rebase ref:feature protected:true"
        );
        assert_eq!(
            current_branch_permission_input("cherry-pick", "dev"),
            "operation:cherry-pick ref:dev protected:true"
        );
        assert_eq!(
            current_branch_permission_input("revert", "wip/foo"),
            "operation:revert ref:wip/foo protected:false"
        );
        assert_eq!(
            current_branch_permission_input("revert", "MAIN"),
            "operation:revert ref:MAIN protected:true"
        );
    }

    #[test]
    fn history_argument_assembly_validates_fields() {
        let rebase = test_input(
            GitOperation::Rebase,
            Some("origin/main"),
            None,
            None,
            None,
            false,
        );
        assert_eq!(
            assemble_git_args(&rebase).unwrap(),
            vec!["rebase", "origin/main"]
        );

        for interactive in ["-i", "--interactive", "--onto"] {
            let input = test_input(
                GitOperation::Rebase,
                Some(interactive),
                None,
                None,
                None,
                false,
            );
            assert!(
                assemble_git_args(&input).unwrap_err().contains("refuses"),
                "flag argument {interactive:?} should be refused"
            );
        }

        let rebase_missing = test_input(GitOperation::Rebase, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&rebase_missing),
            Err("`rebase` requires the `ref` field".to_string())
        );

        let cherry_pick = test_input(
            GitOperation::CherryPick,
            Some("abc1234"),
            None,
            None,
            None,
            false,
        );
        assert_eq!(
            assemble_git_args(&cherry_pick).unwrap(),
            vec!["cherry-pick", "abc1234"]
        );

        let cherry_pick_flag = test_input(
            GitOperation::CherryPick,
            Some("-n"),
            None,
            None,
            None,
            false,
        );
        assert!(
            assemble_git_args(&cherry_pick_flag)
                .unwrap_err()
                .contains("refuses")
        );

        let cherry_pick_missing =
            test_input(GitOperation::CherryPick, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&cherry_pick_missing),
            Err("`cherry-pick` requires the `ref` field".to_string())
        );

        let revert = test_input(
            GitOperation::Revert,
            Some("HEAD~1"),
            None,
            None,
            None,
            false,
        );
        assert_eq!(
            assemble_git_args(&revert).unwrap(),
            vec!["revert", "--no-edit", "HEAD~1"]
        );

        let revert_flag = test_input(
            GitOperation::Revert,
            Some("--mainline"),
            None,
            None,
            None,
            false,
        );
        assert!(
            assemble_git_args(&revert_flag)
                .unwrap_err()
                .contains("refuses")
        );

        let revert_missing = test_input(GitOperation::Revert, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&revert_missing),
            Err("`revert` requires the `ref` field".to_string())
        );
    }

    #[test]
    fn push_delete_argument_assembly_validates_fields() {
        let delete_push = GitToolInput {
            delete: true,
            ..test_input(
                GitOperation::Push,
                Some("wip/foo"),
                Some("origin"),
                None,
                None,
                false,
            )
        };
        assert_eq!(
            assemble_git_args(&delete_push).unwrap(),
            vec!["push", "--delete", "origin", "wip/foo"]
        );

        let delete_missing_ref = GitToolInput {
            delete: true,
            ..test_input(GitOperation::Push, None, Some("origin"), None, None, false)
        };
        assert_eq!(
            assemble_git_args(&delete_missing_ref),
            Err("`push` requires the `ref` field".to_string())
        );

        let delete_on_fetch = GitToolInput {
            delete: true,
            ..test_input(
                GitOperation::Fetch,
                Some("origin/dev"),
                Some("origin"),
                None,
                None,
                false,
            )
        };
        assert!(
            assemble_git_args(&delete_on_fetch)
                .unwrap_err()
                .contains("`delete` field")
        );

        // The canonical permission input is unchanged; the protected-ref
        // predicate keys on the ref being deleted.
        assert_eq!(
            git_permission_input("push", Some("origin/dev"), Some("origin"), None),
            "operation:push ref:origin/dev remote:origin protected:true"
        );
        assert_eq!(
            git_permission_input("push", Some("wip/foo"), Some("origin"), None),
            "operation:push ref:wip/foo remote:origin protected:false"
        );
    }

    #[test]
    fn read_ops_and_write_gap_argument_assembly() {
        let status = test_input(GitOperation::Status, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&status).unwrap(),
            vec!["status", "--short", "--branch"]
        );

        let log = test_input(GitOperation::Log, Some("feature"), None, None, None, false);
        assert_eq!(
            assemble_git_args(&log).unwrap(),
            vec!["log", "--oneline", "-n", "20", "feature"]
        );

        let log_count = GitToolInput {
            count: Some(5),
            ..test_input(GitOperation::Log, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&log_count).unwrap(),
            vec!["log", "--oneline", "-n", "5"]
        );

        let diff_staged = GitToolInput {
            staged: true,
            pathspecs: Some(vec!["crates/agent".to_string()]),
            ..test_input(GitOperation::Diff, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&diff_staged).unwrap(),
            vec!["diff", "--staged", "crates/agent"]
        );

        let diff_flag = GitToolInput {
            pathspecs: Some(vec!["-w".to_string()]),
            ..test_input(GitOperation::Diff, None, None, None, None, false)
        };
        assert!(
            assemble_git_args(&diff_flag)
                .unwrap_err()
                .contains("refuses")
        );

        let show = test_input(GitOperation::Show, Some("HEAD~1"), None, None, None, false);
        assert_eq!(assemble_git_args(&show).unwrap(), vec!["show", "HEAD~1"]);

        let show_bare = test_input(GitOperation::Show, None, None, None, None, false);
        assert_eq!(assemble_git_args(&show_bare).unwrap(), vec!["show"]);

        let branch_list = test_input(GitOperation::Branch, None, None, None, None, false);
        assert_eq!(
            assemble_git_args(&branch_list).unwrap(),
            vec!["branch", "--list"]
        );

        let branch_delete = GitToolInput {
            delete: true,
            ..test_input(GitOperation::Branch, Some("wip/foo"), None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&branch_delete).unwrap(),
            vec!["branch", "-d", "wip/foo"]
        );

        let branch_delete_force = GitToolInput {
            delete: true,
            force: true,
            ..test_input(GitOperation::Branch, Some("wip/foo"), None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&branch_delete_force).unwrap(),
            vec!["branch", "-D", "wip/foo"]
        );

        let branch_delete_missing = GitToolInput {
            delete: true,
            ..test_input(GitOperation::Branch, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&branch_delete_missing),
            Err("`branch` requires the `ref` field".to_string())
        );

        let remote_list = GitToolInput {
            remote_subcommand: Some(RemoteSubcommand::List),
            ..test_input(GitOperation::Remote, None, None, None, None, false)
        };
        assert_eq!(assemble_git_args(&remote_list).unwrap(), vec!["remote"]);

        let worktree_list = GitToolInput {
            worktree_subcommand: Some(WorktreeSubcommand::List),
            ..test_input(GitOperation::Worktree, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&worktree_list).unwrap(),
            vec!["worktree", "list"]
        );

        let worktree_prune = GitToolInput {
            worktree_subcommand: Some(WorktreeSubcommand::Prune),
            ..test_input(GitOperation::Worktree, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&worktree_prune).unwrap(),
            vec!["worktree", "prune"]
        );

        let destination = std::env::temp_dir()
            .join("git-tool-acceptance")
            .to_string_lossy()
            .to_string();
        let init_bare = GitToolInput {
            bare: true,
            destination: Some(destination.clone()),
            ..test_input(GitOperation::Init, None, None, None, None, false)
        };
        assert_eq!(
            assemble_git_args(&init_bare).unwrap(),
            vec!["init", "--bare", destination.as_str()]
        );

        let fetch_refspec = GitToolInput {
            refspec: Some(
                "refs/heads/wip/foo:refs/remotes/origin/wip/foo".to_string(),
            ),
            ..test_input(GitOperation::Fetch, None, Some("origin"), None, None, false)
        };
        assert_eq!(
            assemble_git_args(&fetch_refspec).unwrap(),
            vec![
                "fetch",
                "origin",
                "refs/heads/wip/foo:refs/remotes/origin/wip/foo"
            ]
        );

        let fetch_flag_refspec = GitToolInput {
            refspec: Some("-f".to_string()),
            ..test_input(GitOperation::Fetch, None, Some("origin"), None, None, false)
        };
        assert!(
            assemble_git_args(&fetch_flag_refspec)
                .unwrap_err()
                .contains("refuses")
        );

        // Deleting a protected ref stays gated through the canonical input.
        assert_eq!(
            git_permission_input("branch", Some("dev"), None, None),
            "operation:branch ref:dev protected:true"
        );
        assert_eq!(
            git_permission_input("status", None, None, None),
            "operation:status protected:false"
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
