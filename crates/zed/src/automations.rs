use std::{rc::Rc, str::FromStr, sync::Arc, time::Duration as StdDuration};

use acp_thread::AgentConnection;
use agent::automations::{Automation, AutomationsStore, Schedule};
use agent::{NativeAgent, NativeAgentConnection, Templates, ThreadStore};
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result};
use chrono::Utc;
use client::{Client, UserStore};
use fs::Fs;
use gpui::{App, AsyncApp, Entity};
use language::LanguageRegistry;
use language_model::{LanguageModelRegistry, SelectedModel};
use node_runtime::NodeRuntime;
use project::Project;
use util::path_list::PathList;

/// Arms the persisted automation schedule for the lifetime of the app. Each
/// enabled automation fires on its interval, running the prompt in-process
/// through the native agent and recording the run.
pub fn init(
    cx: &mut App,
    fs: Arc<dyn Fs>,
    node_runtime: NodeRuntime,
    user_store: Entity<UserStore>,
    languages: Arc<LanguageRegistry>,
) {
    let entries = match AutomationsStore::load(AutomationsStore::default_path()) {
        Ok(store) => store
            .automations()
            .entries()
            .iter()
            .filter(|entry| entry.enabled)
            .cloned()
            .collect::<Vec<_>>(),
        Err(error) => {
            log::error!("[AUTOMATION] failed to load schedule: {error:#}");
            return;
        }
    };

    for entry in entries {
        let fs = fs.clone();
        let node_runtime = node_runtime.clone();
        let user_store = user_store.clone();
        let languages = languages.clone();
        cx.spawn(async move |cx| {
            run_on_schedule(entry, fs, node_runtime, user_store, languages, cx).await;
        })
        .detach();
    }
}

async fn run_on_schedule(
    mut entry: Automation,
    fs: Arc<dyn Fs>,
    node_runtime: NodeRuntime,
    user_store: Entity<UserStore>,
    languages: Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) {
    loop {
        let Some(delay) = next_delay(&entry) else {
            log::warn!(
                "[AUTOMATION] {} has an invalid schedule {:?}; stopping",
                entry.id,
                entry.schedule
            );
            return;
        };

        cx.background_executor().timer(delay).await;

        let started = Utc::now();
        log::info!("[AUTOMATION] {} firing: {}", entry.id, entry.prompt);
        match run_prompt(
            &entry,
            fs.clone(),
            node_runtime.clone(),
            user_store.clone(),
            languages.clone(),
            cx,
        )
        .await
        {
            Ok(answer) => log::info!("[AUTOMATION] {} completed: {}", entry.id, answer.trim()),
            Err(error) => log::error!("[AUTOMATION] {} failed: {error:#}", entry.id),
        }

        entry.last_run = Some(started);
        record_run(&entry.id, started);
    }
}

fn next_delay(entry: &Automation) -> Option<StdDuration> {
    let schedule = Schedule::parse(&entry.schedule).ok()?;
    let now = Utc::now();
    let due = schedule.next_fire(now, entry.last_run);
    (due - now).to_std().ok()
}

async fn run_prompt(
    entry: &Automation,
    fs: Arc<dyn Fs>,
    node_runtime: NodeRuntime,
    user_store: Entity<UserStore>,
    languages: Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<String> {
    // Best-effort model selection; fall back to the current default on error.
    if let Ok(selected) = SelectedModel::from_str(&entry.model) {
        cx.update(|cx| {
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.select_default_model(Some(&selected), cx);
            });
        });
    }

    let workdir = paths::home_dir().clone();

    let project = cx.update(|cx| {
        Project::local(
            Client::global(cx),
            node_runtime,
            user_store,
            languages,
            fs.clone(),
            None,
            project::LocalProjectFlags {
                init_worktree_trust: false,
                ..Default::default()
            },
            cx,
        )
    });

    project
        .update(cx, |project, cx| project.create_worktree(&workdir, true, cx))
        .await
        .context("creating worktree")?;

    let thread_store = cx.update(|cx| ThreadStore::global(cx));
    let agent = cx.update(|cx| NativeAgent::new(thread_store, Templates::new(), fs, cx));
    let connection = Rc::new(NativeAgentConnection(agent));

    let acp_thread = cx
        .update(|cx| {
            connection
                .clone()
                .new_session(project, PathList::new(&[workdir]), cx)
        })
        .await
        .context("creating agent session")?;

    let message = vec![acp::ContentBlock::Text(acp::TextContent::new(
        entry.prompt.clone(),
    ))];
    let submission = acp_thread.update(cx, |thread, cx| thread.send(message, cx));
    let response = submission.await.context("running prompt")?;

    match response {
        Some(acp_thread::SubmissionResponse::LegacyCompleted(_)) => {
            Ok(final_response_text(&acp_thread, cx).unwrap_or_default())
        }
        Some(acp_thread::SubmissionResponse::Accepted(_)) => {
            anyhow::bail!("native agent returned acceptance instead of turn completion")
        }
        None => Ok(String::new()),
    }
}

fn final_response_text(
    acp_thread: &Entity<acp_thread::AcpThread>,
    cx: &mut AsyncApp,
) -> Option<String> {
    cx.update(|cx| {
        let entries = acp_thread.read(cx).entries();
        let mut chunks: Vec<String> = Vec::new();
        for entry in entries.iter().rev() {
            let acp_thread::AgentThreadEntry::AssistantMessage(message) = entry else {
                continue;
            };
            for chunk in &message.chunks {
                if let acp_thread::AssistantMessageChunk::Message { block, .. } = chunk {
                    for markdown in block.markdowns() {
                        let text = markdown.read(cx).source().to_string();
                        if !text.is_empty() {
                            chunks.push(text);
                        }
                    }
                }
            }
            if !chunks.is_empty() {
                break;
            }
        }
        if chunks.is_empty() {
            None
        } else {
            Some(chunks.join(""))
        }
    })
}

fn record_run(id: &str, at: chrono::DateTime<Utc>) {
    let result = AutomationsStore::load(AutomationsStore::default_path()).and_then(|mut store| {
        store.automations_mut().record_run(id, at);
        store.save()
    });
    if let Err(error) = result {
        log::error!("[AUTOMATION] failed to record run for {id}: {error:#}");
    }
}
