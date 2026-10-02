use std::sync::Arc;

use crate::{AgentTool, DbThreadMetadata, ThreadsDatabase, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, AppContext as _, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The operation to perform against the persisted agent thread store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadSearchAction {
    /// List all past agent threads, newest first.
    #[default]
    List,
    /// Search thread summaries and full content for a case-insensitive query.
    Search,
    /// Show the full transcript of a single thread.
    Show,
}

/// Search and read past agent conversations stored on this machine.
///
/// Use this to find and read what was discussed or decided in earlier threads —
/// for example when asked to recall a previous conversation, check another
/// thread, or remember what was decided. `list` enumerates threads, `search`
/// finds threads matching a query across summaries and full content, and `show`
/// returns one thread's full transcript by id (or by the 1-based index printed
/// by `list`).
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ThreadSearchToolInput {
    /// Which operation to perform.
    pub action: ThreadSearchAction,
    /// Case-insensitive substring to search for. Required for `action: "search"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Thread id, or the 1-based index shown by `list`. Required for
    /// `action: "show"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThreadSearchToolOutput {
    Success { message: String },
    Error { error: String },
}

impl From<ThreadSearchToolOutput> for LanguageModelToolResultContent {
    fn from(value: ThreadSearchToolOutput) -> Self {
        match value {
            ThreadSearchToolOutput::Success { message } => message.into(),
            ThreadSearchToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct ThreadSearchTool;

impl AgentTool for ThreadSearchTool {
    type Input = ThreadSearchToolInput;
    type Output = ThreadSearchToolOutput;

    const NAME: &'static str = "thread_search";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => match input.action {
                ThreadSearchAction::List => "Listing threads".into(),
                ThreadSearchAction::Search => "Searching threads".into(),
                ThreadSearchAction::Show => "Showing thread".into(),
            },
            Err(_) => "Searching threads".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let database_future = ThreadsDatabase::connect(cx);
        cx.background_spawn(async move {
            let input = input
                .recv()
                .await
                .map_err(|error| ThreadSearchToolOutput::Error {
                    error: format!("Failed to receive thread search tool input: {error}"),
                })?;
            let database =
                database_future
                    .await
                    .map_err(|error| ThreadSearchToolOutput::Error {
                        error: format!("Failed to open the thread store: {error}"),
                    })?;
            run_action(input, database.as_ref()).await
        })
    }
}

async fn run_action(
    input: ThreadSearchToolInput,
    database: &ThreadsDatabase,
) -> Result<ThreadSearchToolOutput, ThreadSearchToolOutput> {
    let threads = database
        .list_threads()
        .await
        .map_err(|error| ThreadSearchToolOutput::Error {
            error: format!("Failed to list threads: {error}"),
        })?;

    match input.action {
        ThreadSearchAction::List => Ok(ThreadSearchToolOutput::Success {
            message: format_list(&threads),
        }),
        ThreadSearchAction::Search => {
            let query = input
                .query
                .filter(|query| !query.trim().is_empty())
                .ok_or_else(|| ThreadSearchToolOutput::Error {
                    error: "`query` is required for `action: \"search\"`.".to_string(),
                })?;
            search_threads(&query, &threads, database).await
        }
        ThreadSearchAction::Show => {
            let id = input.id.filter(|id| !id.trim().is_empty()).ok_or_else(|| {
                ThreadSearchToolOutput::Error {
                    error: "`id` is required for `action: \"show\"`.".to_string(),
                }
            })?;
            show_thread(&id, &threads, database).await
        }
    }
}

async fn search_threads(
    query: &str,
    threads: &[DbThreadMetadata],
    database: &ThreadsDatabase,
) -> Result<ThreadSearchToolOutput, ThreadSearchToolOutput> {
    let query_lower = query.to_lowercase();
    let mut matches: Vec<String> = Vec::new();

    for metadata in threads {
        if matches_query(metadata.title.as_str(), &query_lower) {
            matches.push(format_match(metadata, "summary"));
            continue;
        }

        if let Some(thread) = database
            .load_thread(metadata.id.clone())
            .await
            .map_err(|error| ThreadSearchToolOutput::Error {
                error: format!("Failed to load thread \"{}\": {error}", metadata.id.0),
            })?
        {
            if matches_query(&thread.to_markdown(), &query_lower) {
                matches.push(format_match(metadata, "content"));
            }
        }
    }

    if matches.is_empty() {
        return Ok(ThreadSearchToolOutput::Success {
            message: format!("No threads matched \"{query}\"."),
        });
    }

    let total = matches.len();
    let shown = matches.iter().take(50);
    let mut message = format!("{total} thread(s) matched \"{query}\":\n");
    for (index, entry) in shown.enumerate() {
        message.push_str(&format!("{}. {entry}\n", index + 1));
    }
    if total > 50 {
        message.push_str(&format!("\n(Showing first 50 of {total} matches.)"));
    }
    Ok(ThreadSearchToolOutput::Success { message })
}

async fn show_thread(
    id: &str,
    threads: &[DbThreadMetadata],
    database: &ThreadsDatabase,
) -> Result<ThreadSearchToolOutput, ThreadSearchToolOutput> {
    let metadata =
        resolve_thread(id, threads).map_err(|error| ThreadSearchToolOutput::Error { error })?;

    let thread = database
        .load_thread(metadata.id.clone())
        .await
        .map_err(|error| ThreadSearchToolOutput::Error {
            error: format!("Failed to load thread \"{}\": {error}", metadata.id.0),
        })?
        .ok_or_else(|| ThreadSearchToolOutput::Error {
            error: format!("No thread with id \"{}\".", metadata.id.0),
        })?;

    Ok(ThreadSearchToolOutput::Success {
        message: thread.to_markdown(),
    })
}

fn format_list(threads: &[DbThreadMetadata]) -> String {
    if threads.is_empty() {
        return "No threads found.".to_string();
    }

    let mut message = format!("{} thread(s):\n", threads.len());
    for (index, metadata) in threads.iter().enumerate() {
        message.push_str(&format!(
            "{}. [{}] {}\n   id={}\n",
            index + 1,
            metadata.updated_at,
            metadata.title,
            metadata.id.0,
        ));
    }
    message
}

fn format_match(metadata: &DbThreadMetadata, source: &str) -> String {
    format!(
        "{} [{}] (matched in {source})\n   id={}",
        metadata.title, metadata.updated_at, metadata.id.0
    )
}

/// Resolves a 1-based `list` index or a full thread id to a thread.
fn resolve_thread<'a>(
    id: &str,
    threads: &'a [DbThreadMetadata],
) -> Result<&'a DbThreadMetadata, String> {
    if let Ok(index) = id.parse::<usize>() {
        if index == 0 || index > threads.len() {
            return Err(format!(
                "No thread at index {index} ({} thread(s)).",
                threads.len()
            ));
        }
        return Ok(&threads[index - 1]);
    }

    threads
        .iter()
        .find(|metadata| metadata.id.0.as_ref() == id)
        .ok_or_else(|| format!("No thread with id \"{id}\"."))
}

fn matches_query(haystack: &str, query_lower: &str) -> bool {
    haystack.to_lowercase().contains(query_lower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DbSandboxGrants, DbThread, Message, UserMessage, UserMessageContent};
    use acp_thread::ClientUserMessageId;
    use chrono::{DateTime, Utc};
    use collections::HashMap;
    use util::path_list::PathList;

    fn session_id(value: &str) -> acp::SessionId {
        acp::SessionId::new(Arc::<str>::from(value))
    }

    fn make_thread(title: &str, updated_at: DateTime<Utc>) -> DbThread {
        DbThread {
            title: title.to_string().into(),
            messages: Vec::new(),
            updated_at,
            detailed_summary: None,
            initial_project_snapshot: None,
            cumulative_token_usage: Default::default(),
            request_token_usage: HashMap::default(),
            model: None,
            profile: None,
            planning: false,
            subagent_context: None,
            speed: None,
            thinking_enabled: false,
            thinking_effort: None,
            draft_prompt: None,
            ui_scroll_position: None,
            sandboxed_terminal_temp_dir: None,
            sandbox_grants: DbSandboxGrants::default(),
        }
    }

    fn user_message(text: &str) -> Arc<Message> {
        Arc::new(Message::User(UserMessage {
            id: ClientUserMessageId::new(),
            content: Arc::from([UserMessageContent::Text(text.to_string())]),
        }))
    }

    fn metadata(id: &str, title: &str, updated_at: DateTime<Utc>) -> DbThreadMetadata {
        DbThreadMetadata {
            id: session_id(id),
            parent_session_id: None,
            title: title.into(),
            updated_at,
            created_at: None,
            folder_paths: PathList::default(),
            workspace_id: None,
        }
    }

    #[test]
    fn resolve_thread_accepts_index_and_id() {
        let timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let threads = vec![
            metadata("thread-a", "Alpha", timestamp),
            metadata("thread-b", "Beta", timestamp),
        ];

        assert_eq!(
            resolve_thread("1", &threads).unwrap().id.0.as_ref(),
            "thread-a"
        );
        assert_eq!(
            resolve_thread("2", &threads).unwrap().id.0.as_ref(),
            "thread-b"
        );
        assert_eq!(
            resolve_thread("thread-b", &threads).unwrap().id.0.as_ref(),
            "thread-b"
        );
        assert!(resolve_thread("0", &threads).is_err());
        assert!(resolve_thread("99", &threads).is_err());
        assert!(resolve_thread("missing", &threads).is_err());
    }

    #[test]
    fn format_list_numbers_newest_first_order() {
        let timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let threads = vec![
            metadata("thread-a", "Alpha", timestamp),
            metadata("thread-b", "Beta", timestamp),
        ];

        let rendered = format_list(&threads);
        assert!(rendered.starts_with("2 thread(s):\n"));
        assert!(rendered.contains("1. ["));
        assert!(rendered.contains("Alpha"));
        assert!(rendered.contains("id=thread-a"));
        assert!(rendered.contains("id=thread-b"));
    }

    #[test]
    fn format_list_is_empty_when_there_are_no_threads() {
        assert_eq!(format_list(&[]), "No threads found.");
    }

    #[test]
    fn matches_query_is_case_insensitive() {
        assert!(matches_query("Hello World", "hello"));
        assert!(matches_query("Hello World", "WORLD"));
        assert!(!matches_query("Hello World", "nope"));
    }

    #[gpui::test]
    async fn list_search_and_show_round_trip(cx: &mut gpui::TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let mut alpha = make_thread("Alpha", Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap());
        alpha
            .messages
            .push(user_message("hello from the alpha thread"));
        database
            .save_thread(session_id("thread-a"), alpha, PathList::default(), None)
            .await
            .unwrap();

        let mut beta = make_thread("Beta", Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap());
        beta.messages
            .push(user_message("goodbye from the beta thread"));
        database
            .save_thread(session_id("thread-b"), beta, PathList::default(), None)
            .await
            .unwrap();

        // list
        let list = run_action(
            ThreadSearchToolInput {
                action: ThreadSearchAction::List,
                query: None,
                id: None,
            },
            &database,
        )
        .await
        .unwrap();
        let ThreadSearchToolOutput::Success { message } = list else {
            panic!("list should succeed");
        };
        assert!(message.contains("Alpha"));
        assert!(message.contains("Beta"));

        // search by summary (case-insensitive)
        let summary_hit = run_action(
            ThreadSearchToolInput {
                action: ThreadSearchAction::Search,
                query: Some("ALPHA".to_string()),
                id: None,
            },
            &database,
        )
        .await
        .unwrap();
        let ThreadSearchToolOutput::Success { message } = summary_hit else {
            panic!("summary search should succeed");
        };
        assert!(message.contains("Alpha"));
        assert!(message.contains("(matched in summary)"));

        // search by content only
        let content_hit = run_action(
            ThreadSearchToolInput {
                action: ThreadSearchAction::Search,
                query: Some("goodbye".to_string()),
                id: None,
            },
            &database,
        )
        .await
        .unwrap();
        let ThreadSearchToolOutput::Success { message } = content_hit else {
            panic!("content search should succeed");
        };
        assert!(message.contains("Beta"));
        assert!(message.contains("(matched in content)"));

        // show by id
        let shown = run_action(
            ThreadSearchToolInput {
                action: ThreadSearchAction::Show,
                query: None,
                id: Some("thread-a".to_string()),
            },
            &database,
        )
        .await
        .unwrap();
        let ThreadSearchToolOutput::Success { message } = shown else {
            panic!("show should succeed");
        };
        assert!(message.contains("hello from the alpha thread"));

        // show by 1-based index (newest first: thread-b is index 1)
        let shown_by_index = run_action(
            ThreadSearchToolInput {
                action: ThreadSearchAction::Show,
                query: None,
                id: Some("1".to_string()),
            },
            &database,
        )
        .await
        .unwrap();
        let ThreadSearchToolOutput::Success { message } = shown_by_index else {
            panic!("show by index should succeed");
        };
        assert!(message.contains("goodbye from the beta thread"));

        // unknown id is an error
        let missing = run_action(
            ThreadSearchToolInput {
                action: ThreadSearchAction::Show,
                query: None,
                id: Some("missing".to_string()),
            },
            &database,
        )
        .await
        .unwrap();
        assert!(matches!(
            missing,
            ThreadSearchToolOutput::Error { error } if error.contains("No thread with id")
        ));
    }
}
