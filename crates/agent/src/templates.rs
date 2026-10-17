use agent_settings::builtin_profiles;
use anyhow::Result;
use gpui::SharedString;
use handlebars::Handlebars;
use serde::Serialize;
use std::sync::Arc;

// Dev builds read the checkout's templates at runtime instead of embedding
// them; see the `assets` crate for the rationale.
util::fs_embed! {
    struct Assets,
    crate_relative = "src/templates",
    root_relative = "crates/agent/src/templates",
    include = ["*.hbs"],
}

pub struct Templates(Handlebars<'static>);

impl Templates {
    pub fn new() -> Arc<Self> {
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(true);
        handlebars.register_helper("contains", Box::new(contains));
        handlebars.register_embed_templates::<Assets>().unwrap();
        Arc::new(Self(handlebars))
    }
}

pub trait Template: Sized {
    const TEMPLATE_NAME: &'static str;

    fn render(&self, templates: &Templates) -> Result<String>
    where
        Self: Serialize + Sized,
    {
        Ok(templates.0.render(Self::TEMPLATE_NAME, self)?)
    }
}

#[derive(Serialize)]
pub struct SystemPromptTemplate<'a> {
    #[serde(flatten)]
    pub project: &'a prompt_store::ProjectContext,
    pub available_tools: Vec<SharedString>,
    pub model_name: Option<String>,
    pub date: String,
    /// Contents of the user-global `~/.config/zed/AGENTS.md` file (or the
    /// platform equivalent), if present and non-empty.
    pub user_agents_md: Option<SharedString>,
    /// Whether agent-run terminal commands are wrapped in an OS-level
    /// sandbox for this thread. When `true` — and the `terminal` tool is
    /// in `available_tools` — the rendered prompt describes the sandbox's
    /// read/write/network rules and the per-command flags the model can
    /// request to relax them. Otherwise the prompt omits the sandbox
    /// section entirely.
    pub sandboxing: bool,
    /// Whether the host is Linux. The writable-temp story differs by
    /// platform (Linux exposes an ephemeral `tmpfs` over `/tmp`; other
    /// platforms provide a persistent per-thread `$TMPDIR`), so the sandbox
    /// section describes the right one rather than advertising a `$TMPDIR`
    /// that doesn't behave as stated.
    pub is_linux: bool,
    /// Whether sandboxed terminal commands run through WSL on Windows.
    pub is_windows: bool,
}

impl Template for SystemPromptTemplate<'_> {
    const TEMPLATE_NAME: &'static str = "system_prompt.hbs";
}

/// Returns a short mode-specific framing fragment for the given profile, or
/// `None` for profiles without special instructions (custom profiles and
/// `minimal`).
///
/// Kept as a rendered fragment rather than a `system_prompt.hbs` section so the
/// mode copy lives beside the template without adding a new embedded asset
/// (new `.hbs` files are silently skipped on incremental rebuilds).
pub fn mode_framing(profile_id: &str) -> Option<&'static str> {
    match profile_id {
        builtin_profiles::READ => Some(
            "## Agent mode: read\n\n\
             You have full read access but cannot make changes. You may inspect \
             the codebase, run the terminal, use the debugger to observe state \
             (without modifying it), and ask the user questions — but you must \
             not edit, create, move, or delete any files.",
        ),
        builtin_profiles::WRITE => Some(
            "## Agent mode: write\n\n\
             You may make changes to the project, but you cannot execute \
             anything: you have no terminal and no debugger.",
        ),
        builtin_profiles::EXECUTE => Some(
            "## Agent mode: execute\n\n\
             You have full autonomy: you may edit files, run the terminal, and \
             use the debugger. Route write and network git operations \
             (`commit`, `checkout`, `switch`, `branch`, `fetch`, `pull`, \
             `merge`, `push`, `tag`) through the `git` tool so its \
             per-operation permissions gate merges and pushes into protected \
             refs; use `terminal` for git commands the tool does not model \
             (e.g. `git worktree add`, `git status`) and for build/test/run \
             commands.",
        ),
        builtin_profiles::DEBUG => Some(
            "## Agent mode: debug\n\n\
             Work a tight diagnose-fix-verify loop: scaffold a minimal \
             reproduction, ask the user to replicate it, apply a fix, then ask \
             whether the issue is resolved — repeating until it is. For rebuilds \
             use the `remote-compiler` skill; for debugger-tool acceptance runs \
             use the `debugger-suite` skill.",
        ),
        _ => None,
    }
}

/// The planning overlay, layered on top of whichever base profile is active.
/// Unlike the old `plan` profile (a read-only permission tier), planning is
/// permission-neutral: the base profile still governs the tool set, so the
/// model may author plan files with the editor tools its base profile grants.
pub fn plan_framing() -> &'static str {
    "## Plan mode\n\n\
     You are in plan mode. Always load the `plan-manager` skill and draft or \
     edit plans in `plans/`. Your base-mode permissions still apply, so you \
     may write plan files with whatever editor tools your base profile grants. \
     Before finalizing a plan, state which crate tags it touches (e.g. `dap`, \
     `browser_tools`), map those tags to validation suites with \
     `python zed-debugger-demo/zed-tools/loop/run-suites.py --tags <tags>`, run \
     each returned suite, and cite the results in the plan. Always load the \
     `dev-flow` skill and follow its end-to-end route: mark the plan's \
     `## Checklist` items as each completes (in the same edit, never in bulk), \
     and never merge or release before every gate has actually run and passed. \
     Route write and network git operations through the `git` tool where \
     available; its per-operation permissions gate merges and pushes into \
     protected refs."
}

/// The REA (reverse-engineering) overlay. Unlike plan mode (framing-only),
/// the `rea_mode` toggle also exposes the `rea` context server's tools; this
/// text only tells the model how to use them.
pub fn rea_framing() -> &'static str {
    "## REA mode\n\n\
     You have REA (reverse-engineering) tools available. Load the \
     `reverse-engineer-anything` skill and follow its workflow: pick the \
     tool that directly answers the question, cite the Evidence each result \
     returns, and state each conclusion's coverage and limitations. Analysis \
     runs locally on the `rea` host."
}

/// The sub-agent overlay. A sub-agent is spawned for one task and only its
/// final message reaches the parent, so it must execute the task and report
/// back rather than explore, narrate, or re-derive the parent's context.
pub fn subagent_framing() -> &'static str {
    "## Sub-agent\n\n\
     You are a sub-agent spawned by the Zed coding agent to complete one \
     specific task. The user message contains that task and any context you \
     need. Do exactly that task and report the result concisely. Your final \
     message is all the parent agent receives, so make it a direct answer or \
     result, not a narrative of your process. Do not explore the codebase, \
     make changes, or re-derive the parent's work unless the task explicitly \
     asks for it."
}

/// Handlebars helper for checking if an item is in a list
fn contains(
    h: &handlebars::Helper,
    _: &handlebars::Handlebars,
    _: &handlebars::Context,
    _: &mut handlebars::RenderContext,
    out: &mut dyn handlebars::Output,
) -> handlebars::HelperResult {
    let list = h
        .param(0)
        .and_then(|v| v.value().as_array())
        .ok_or_else(|| {
            handlebars::RenderError::new("contains: missing or invalid list parameter")
        })?;
    let query = h.param(1).map(|v| v.value()).ok_or_else(|| {
        handlebars::RenderError::new("contains: missing or invalid query parameter")
    })?;

    if list.contains(query) {
        out.write("true")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_framing() {
        assert!(
            mode_framing(builtin_profiles::READ)
                .unwrap()
                .contains("cannot make changes")
        );
        assert!(
            mode_framing(builtin_profiles::WRITE)
                .unwrap()
                .contains("no terminal")
        );
        assert!(
            mode_framing(builtin_profiles::DEBUG)
                .unwrap()
                .contains("remote-compiler")
        );
        assert!(mode_framing(builtin_profiles::EXECUTE).is_some());
        assert!(mode_framing("custom").is_none());
        assert!(mode_framing(builtin_profiles::MINIMAL).is_none());
        assert!(plan_framing().contains("plan-manager"));
        assert!(plan_framing().contains("base-mode permissions"));
    }

    #[test]
    fn test_system_prompt_template() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(rendered.contains("You are the Zed coding agent"));
        assert!(rendered.contains("Today's Date: 2026-01-01"));
        assert!(rendered.contains("## Fixing Diagnostics"));
        assert!(rendered.contains("test-model"));
    }

    #[test]
    fn test_system_prompt_renders_user_agents_md_before_project_rules() {
        use prompt_store::{ProjectContext, RulesFileContext, WorktreeContext};
        use util::rel_path::RelPath;

        let worktrees = vec![WorktreeContext {
            root_name: "my-project".to_string(),
            abs_path: std::path::Path::new("/tmp/my-project").into(),
            rules_file: Some(RulesFileContext {
                path_in_worktree: RelPath::from_unix_str("AGENTS.md").unwrap().into(),
                text: "project-specific guidance".to_string(),
                project_entry_id: 1,
            }),
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: Some("always be concise".into()),
            sandboxing: false,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("### Personal `AGENTS.md`"));
        assert!(rendered.contains("always be concise"));
        assert!(rendered.contains("### Project Rules"));
        assert!(rendered.contains("project-specific guidance"));

        let personal_idx = rendered.find("### Personal `AGENTS.md`").unwrap();
        let project_idx = rendered.find("### Project Rules").unwrap();
        assert!(
            personal_idx < project_idx,
            "personal AGENTS.md should render before project rules so project rules can override it"
        );
    }

    #[test]
    fn test_system_prompt_omits_sandbox_section_when_sandboxing_disabled() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(!rendered.contains("## Terminal sandbox"));
        assert!(!rendered.contains("allow_hosts"));
    }

    #[test]
    fn test_system_prompt_renders_sandbox_section_with_worktrees_when_enabled() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![
            WorktreeContext {
                root_name: "alpha".to_string(),
                abs_path: std::path::Path::new("/tmp/alpha").into(),
                rules_file: None,
            },
            WorktreeContext {
                root_name: "beta".to_string(),
                abs_path: std::path::Path::new("/tmp/beta").into(),
                rules_file: None,
            },
        ];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        assert!(rendered.contains("`/tmp/alpha`"));
        assert!(rendered.contains("`/tmp/beta`"));
        assert!(rendered.contains("allow_hosts"));
        assert!(rendered.contains("allow_all_hosts: true"));
        assert!(rendered.contains("fs_write_paths"));
        assert!(rendered.contains("allow_fs_write_all: true"));
        assert!(rendered.contains("unsandboxed: true"));
        assert!(rendered.contains("`.git` directories remain protected"));
        assert!(rendered.contains("Git metadata writes are never grantable inside the sandbox"));
        assert!(rendered.contains("request `unsandboxed: true` with a reason"));
        assert!(rendered.contains("git --no-optional-locks status"));
        assert!(rendered.contains("for the rest of the thread"));
        // macOS tolerates granting a not-yet-existing path, so the
        // existing-directory requirement must not be stated there; the
        // `create_directory` flow is the preferred guidance instead.
        assert!(!rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_linux_sandbox_section_omits_tmpdir() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![WorktreeContext {
            root_name: "alpha".to_string(),
            abs_path: std::path::Path::new("/tmp/alpha").into(),
            rules_file: None,
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: true,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        // On Linux we must not advertise the special persistent `$TMPDIR`.
        assert!(!rendered.contains("$TMPDIR"));
        assert!(rendered.contains("`/tmp` is writable"));
        assert!(rendered.contains("`/tmp/alpha`"));
        // Linux write grants must already exist (bwrap binds existing paths).
        assert!(rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_windows_sandbox_section_rejects_host_specific_network() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![WorktreeContext {
            root_name: "alpha".to_string(),
            abs_path: std::path::Path::new("C:/Users/me/project").into(),
            rules_file: None,
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: true,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("commands run inside WSL under Bubblewrap"));
        assert!(rendered.contains("Protected Git metadata remains read-only"));
        assert!(rendered.contains("do not use this on Windows"));
        assert!(rendered.contains("such requests are rejected"));
        assert!(rendered.contains("allow_all_hosts: true"));
        assert!(rendered.contains("git --no-optional-locks status"));
        // Out-of-project `create_directory` grants aren't supported on Windows,
        // so the prompt must not recommend that flow; it suggests granting the
        // nearest existing parent instead.
        assert!(rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("nearest existing parent directory"));
        assert!(!rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_sandbox_section_handles_zero_worktrees() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        assert!(rendered.contains("No project directories are currently writable"));
    }

    #[test]
    fn test_system_prompt_omits_sandbox_section_when_terminal_tool_unavailable() {
        // A profile can disable the terminal tool entirely; the prompt must not
        // describe a sandboxed `terminal` tool the model doesn't have.
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(!rendered.contains("## Terminal sandbox"));
        assert!(!rendered.contains("allow_hosts"));
    }

    #[test]
    fn test_system_prompt_omits_user_agents_md_section_when_absent() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(!rendered.contains("### Personal `AGENTS.md`"));
    }

    #[test]
    fn test_system_prompt_does_not_render_legacy_zed_rules_section() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(!rendered.contains("The user has specified the following rules"));
        assert!(!rendered.contains("Rules title:"));
    }
}
