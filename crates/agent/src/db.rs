use crate::{AgentMessage, AgentMessageContent, UserMessage, UserMessageContent};
use acp_thread::ClientUserMessageId;
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentProfileId;
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use collections::{HashMap, IndexMap};
use futures::{FutureExt, future::Shared};
use gpui::{BackgroundExecutor, Global, Task};
use indoc::indoc;
use language_model::{LanguageModelToolResultContent, Speed};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sqlez::{
    bindable::{Bind, Column},
    connection::Connection,
    statement::Statement,
};
use std::{io::ErrorKind, ops::Range, path::PathBuf, sync::Arc};
use ui::{App, SharedString};
use util::path_list::PathList;
use zed_env_vars::ZED_STATELESS;

pub type DbMessage = crate::Message;
pub type DbSummary = crate::legacy_thread::DetailedSummaryState;
pub type DbLanguageModel = crate::legacy_thread::SerializedLanguageModel;

#[derive(Debug, Clone)]
pub struct DbThreadMetadata {
    pub id: acp::SessionId,
    pub parent_session_id: Option<acp::SessionId>,
    pub title: SharedString,
    pub updated_at: DateTime<Utc>,
    pub created_at: Option<DateTime<Utc>>,
    /// The workspace folder paths this thread was created against, sorted
    /// lexicographically. Used for grouping threads by workspace in the sidebar.
    pub folder_paths: PathList,
    /// The stable managed-workspace id this thread belongs to, if one has been
    /// minted. Stored as the hyphenated string form of the manager id.
    pub workspace_id: Option<String>,
}

impl From<&DbThreadMetadata> for acp_thread::AgentSessionInfo {
    fn from(meta: &DbThreadMetadata) -> Self {
        Self {
            session_id: meta.id.clone(),
            work_dirs: Some(meta.folder_paths.clone()),
            title: Some(meta.title.clone()),
            updated_at: Some(meta.updated_at),
            created_at: meta.created_at,
            meta: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DbThread {
    pub title: SharedString,
    pub messages: Vec<Arc<DbMessage>>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub detailed_summary: Option<SharedString>,
    #[serde(default)]
    pub initial_project_snapshot: Option<Arc<crate::ProjectSnapshot>>,
    #[serde(default)]
    pub cumulative_token_usage: language_model::TokenUsage,
    #[serde(default)]
    pub request_token_usage: HashMap<acp_thread::ClientUserMessageId, language_model::TokenUsage>,
    #[serde(default)]
    pub model: Option<DbLanguageModel>,
    #[serde(default)]
    pub profile: Option<AgentProfileId>,
    #[serde(default)]
    pub planning: bool,
    #[serde(default)]
    pub subagent_context: Option<crate::SubagentContext>,
    #[serde(default)]
    pub speed: Option<Speed>,
    #[serde(default)]
    pub thinking_enabled: bool,
    #[serde(default)]
    pub thinking_effort: Option<String>,
    #[serde(default)]
    pub draft_prompt: Option<Vec<acp::ContentBlock>>,
    #[serde(default)]
    pub ui_scroll_position: Option<SerializedScrollPosition>,
    #[serde(default)]
    pub sandboxed_terminal_temp_dir: Option<PathBuf>,
    /// Sandbox escalations the user approved "for the rest of this thread".
    /// Persisted so reopening a thread keeps its grants. See
    /// [`crate::sandboxing::ThreadSandboxGrants`].
    #[serde(default)]
    pub sandbox_grants: DbSandboxGrants,
}

/// Serialized form of the sandbox permissions the user granted "for the rest of
/// this thread" (the "Allow for this thread" prompt option). Stored inside the
/// thread blob; round-trips with [`crate::sandboxing::ThreadSandboxGrants`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DbSandboxGrants {
    /// Paths granted write access, each paired with the canonical
    /// (symlink-resolved) target established when the grant was approved; each
    /// covers its whole subtree. Legacy rows stored a bare path string per
    /// entry, which still deserializes (as a grant with no resolved canonical)
    /// via [`settings::GrantedWritePath`]'s string-or-object format.
    #[serde(default)]
    pub write_paths: Vec<settings::GrantedWritePath>,
    /// Host patterns granted network access, in canonical string form (e.g.
    /// `github.com`, `*.npmjs.org`). Parsed back into patterns on load.
    #[serde(default)]
    pub network_hosts: Vec<String>,
    /// Whether arbitrary-host network access was granted.
    #[serde(default)]
    pub network_any_host: bool,
    /// Whether unrestricted filesystem writes (the broad escape hatch) were
    /// granted.
    #[serde(default)]
    pub allow_fs_write_all: bool,

    /// Whether the model-requested fully-unsandboxed escape was granted.
    #[serde(default)]
    pub unsandboxed: bool,
    /// Whether running commands unsandboxed was allowed because the OS sandbox
    /// could not be created (the fallback prompt's "for this thread" option).
    #[serde(default)]
    pub sandbox_fallback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SerializedScrollPosition {
    pub item_ix: usize,
    pub offset_in_item: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedThread {
    pub title: SharedString,
    pub messages: Vec<Arc<DbMessage>>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub model: Option<DbLanguageModel>,
    pub version: String,
}

impl SharedThread {
    pub const VERSION: &'static str = "1.0.0";

    pub fn from_db_thread(thread: &DbThread) -> Self {
        Self {
            title: thread.title.clone(),
            messages: thread.messages.clone(),
            updated_at: thread.updated_at,
            model: thread.model.clone(),
            version: Self::VERSION.to_string(),
        }
    }

    pub fn to_db_thread(self) -> DbThread {
        DbThread {
            title: format!("🔗 {}", self.title).into(),
            messages: self.messages,
            updated_at: self.updated_at,
            detailed_summary: None,
            initial_project_snapshot: None,
            cumulative_token_usage: Default::default(),
            request_token_usage: Default::default(),
            model: self.model,
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

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        const COMPRESSION_LEVEL: i32 = 3;
        let json = serde_json::to_vec(self)?;
        let compressed = zstd::encode_all(json.as_slice(), COMPRESSION_LEVEL)?;
        Ok(compressed)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let decompressed = zstd::decode_all(data)?;
        Ok(serde_json::from_slice(&decompressed)?)
    }
}

/// A projection of a [`DbThread`] with the (potentially huge) message bodies
/// omitted, so it can be serialized cheaply when comparing a thread against the
/// content last written to the database.
#[derive(Serialize)]
struct DbThreadContentProjection<'a> {
    title: &'a SharedString,
    updated_at: &'a DateTime<Utc>,
    detailed_summary: &'a Option<SharedString>,
    cumulative_token_usage: &'a language_model::TokenUsage,
    request_token_usage: &'a HashMap<ClientUserMessageId, language_model::TokenUsage>,
    model: &'a Option<DbLanguageModel>,
    profile: &'a Option<AgentProfileId>,
    planning: bool,
    subagent_context: &'a Option<crate::SubagentContext>,
    speed: &'a Option<Speed>,
    thinking_enabled: bool,
    thinking_effort: &'a Option<String>,
    draft_prompt: &'a Option<Vec<acp::ContentBlock>>,
    sandboxed_terminal_temp_dir: &'a Option<PathBuf>,
    sandbox_grants: &'a DbSandboxGrants,
    initial_project_snapshot: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ui_scroll_position: Option<&'a SerializedScrollPosition>,
}

impl DbThread {
    pub const VERSION: &'static str = "0.3.0";

    /// A cheap identity of the content that `save_thread` would persist, used to
    /// skip re-serializing and re-compressing the whole thread when nothing the
    /// database stores has changed.
    ///
    /// Message bodies participate by `Arc` pointer rather than by value —
    /// messages are immutable once stored in a thread, so a stable pointer means
    /// an unchanged body — which keeps this O(message count) in pointer
    /// comparisons instead of serializing tens of megabytes.
    ///
    /// `include_scroll` controls whether the UI scroll offset participates.
    /// Passing `false` yields the identity of the conversation content alone, so
    /// a scroll-offset-only change can be told apart from a content change.
    pub(crate) fn save_fingerprint(
        &self,
        folder_paths: &PathList,
        workspace_id: Option<&str>,
        include_scroll: bool,
    ) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.messages.len().hash(&mut hasher);
        for message in &self.messages {
            (Arc::as_ptr(message) as usize).hash(&mut hasher);
        }
        let serialized_folder_paths = folder_paths.serialize();
        serialized_folder_paths.paths.hash(&mut hasher);
        serialized_folder_paths.order.hash(&mut hasher);
        workspace_id.hash(&mut hasher);

        let projection = DbThreadContentProjection {
            title: &self.title,
            updated_at: &self.updated_at,
            detailed_summary: &self.detailed_summary,
            cumulative_token_usage: &self.cumulative_token_usage,
            request_token_usage: &self.request_token_usage,
            model: &self.model,
            profile: &self.profile,
            planning: self.planning,
            subagent_context: &self.subagent_context,
            speed: &self.speed,
            thinking_enabled: self.thinking_enabled,
            thinking_effort: &self.thinking_effort,
            draft_prompt: &self.draft_prompt,
            sandboxed_terminal_temp_dir: &self.sandboxed_terminal_temp_dir,
            sandbox_grants: &self.sandbox_grants,
            initial_project_snapshot: self
                .initial_project_snapshot
                .as_ref()
                .map(|snapshot| Arc::as_ptr(snapshot) as usize),
            ui_scroll_position: include_scroll
                .then_some(self.ui_scroll_position.as_ref())
                .flatten(),
        };
        match serde_json::to_vec(&projection) {
            Ok(bytes) => bytes.hash(&mut hasher),
            Err(error) => {
                // Projection serialization can't realistically fail; if it does,
                // fold in a value that forces a real save rather than skipping.
                log::error!("failed to fingerprint thread content: {error:#}");
                u64::MAX.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    pub fn to_markdown(&self) -> String {
        crate::messages_to_markdown(&self.messages)
    }

    pub fn from_json(json: &[u8]) -> Result<Self> {
        let saved_thread_json = serde_json::from_slice::<serde_json::Value>(json)?;
        match saved_thread_json.get("version") {
            Some(serde_json::Value::String(version)) => match version.as_str() {
                Self::VERSION => Ok(serde_json::from_value(saved_thread_json)?),
                _ => Self::upgrade_from_agent_1(crate::legacy_thread::SerializedThread::from_json(
                    json,
                )?),
            },
            _ => {
                Self::upgrade_from_agent_1(crate::legacy_thread::SerializedThread::from_json(json)?)
            }
        }
    }

    fn upgrade_from_agent_1(thread: crate::legacy_thread::SerializedThread) -> Result<Self> {
        let mut messages = Vec::new();
        let mut request_token_usage = HashMap::default();

        let mut last_user_message_id = None;
        for (ix, msg) in thread.messages.into_iter().enumerate() {
            let message = match msg.role {
                language_model::Role::User => {
                    let mut content = Vec::new();

                    // Convert segments to content
                    for segment in msg.segments {
                        match segment {
                            crate::legacy_thread::SerializedMessageSegment::Text { text } => {
                                content.push(UserMessageContent::Text(text));
                            }
                            crate::legacy_thread::SerializedMessageSegment::Thinking {
                                text,
                                ..
                            } => {
                                // User messages don't have thinking segments, but handle gracefully
                                content.push(UserMessageContent::Text(text));
                            }
                            crate::legacy_thread::SerializedMessageSegment::RedactedThinking {
                                ..
                            } => {
                                // User messages don't have redacted thinking, skip.
                            }
                        }
                    }

                    // If no content was added, add context as text if available
                    if content.is_empty() && !msg.context.is_empty() {
                        content.push(UserMessageContent::Text(msg.context));
                    }

                    let id = ClientUserMessageId::new();
                    last_user_message_id = Some(id.clone());

                    crate::Message::User(UserMessage {
                        // MessageId from old format can't be meaningfully converted, so generate a new one
                        id,
                        content: Arc::from(content),
                    })
                }
                language_model::Role::Assistant => {
                    let mut content = Vec::new();

                    // Convert segments to content
                    for segment in msg.segments {
                        match segment {
                            crate::legacy_thread::SerializedMessageSegment::Text { text } => {
                                content.push(AgentMessageContent::Text(text));
                            }
                            crate::legacy_thread::SerializedMessageSegment::Thinking {
                                text,
                                signature,
                            } => {
                                content.push(AgentMessageContent::Thinking { text, signature });
                            }
                            crate::legacy_thread::SerializedMessageSegment::RedactedThinking {
                                data,
                            } => {
                                content.push(AgentMessageContent::RedactedThinking(data));
                            }
                        }
                    }

                    // Convert tool uses
                    let mut tool_names_by_id = HashMap::default();
                    for tool_use in msg.tool_uses {
                        tool_names_by_id.insert(tool_use.id.clone(), tool_use.name.clone());
                        content.push(AgentMessageContent::ToolUse(
                            language_model::LanguageModelToolUse {
                                id: tool_use.id,
                                name: tool_use.name.into(),
                                raw_input: serde_json::to_string(&tool_use.input)
                                    .unwrap_or_default(),
                                input: language_model::LanguageModelToolUseInput::Json(
                                    tool_use.input,
                                ),
                                is_input_complete: true,
                                thought_signature: None,
                            },
                        ));
                    }

                    // Convert tool results
                    let mut tool_results = IndexMap::default();
                    for tool_result in msg.tool_results {
                        let name = tool_names_by_id
                            .remove(&tool_result.tool_use_id)
                            .unwrap_or_else(|| SharedString::from("unknown"));
                        tool_results.insert(
                            tool_result.tool_use_id.clone(),
                            language_model::LanguageModelToolResult {
                                tool_use_id: tool_result.tool_use_id,
                                tool_name: name.into(),
                                is_error: tool_result.is_error,
                                content: vec![tool_result.content],
                                output: tool_result.output,
                            },
                        );
                    }

                    if let Some(last_user_message_id) = &last_user_message_id
                        && let Some(token_usage) = thread.request_token_usage.get(ix).copied()
                    {
                        request_token_usage.insert(last_user_message_id.clone(), token_usage);
                    }

                    crate::Message::Agent(AgentMessage {
                        content,
                        tool_results,
                        reasoning_details: None,
                    })
                }
                language_model::Role::System => {
                    // Skip system messages as they're not supported in the new format
                    continue;
                }
            };

            messages.push(Arc::new(message));
        }

        Ok(Self {
            title: thread.summary,
            messages,
            updated_at: thread.updated_at,
            detailed_summary: match thread.detailed_summary_state {
                crate::legacy_thread::DetailedSummaryState::NotGenerated
                | crate::legacy_thread::DetailedSummaryState::Generating => None,
                crate::legacy_thread::DetailedSummaryState::Generated { text, .. } => Some(text),
            },
            initial_project_snapshot: thread.initial_project_snapshot,
            cumulative_token_usage: thread.cumulative_token_usage,
            request_token_usage,
            model: thread.model,
            profile: thread.profile,
            planning: false,
            subagent_context: None,
            speed: None,
            thinking_enabled: false,
            thinking_effort: None,
            draft_prompt: None,
            ui_scroll_position: None,
            sandboxed_terminal_temp_dir: None,
            sandbox_grants: DbSandboxGrants::default(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataType {
    #[serde(rename = "json")]
    Json,
    #[serde(rename = "zstd")]
    Zstd,
}

impl Bind for DataType {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        let value = match self {
            DataType::Json => "json",
            DataType::Zstd => "zstd",
        };
        value.bind(statement, start_index)
    }
}

impl Column for DataType {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (value, next_index) = String::column(statement, start_index)?;
        let data_type = match value.as_str() {
            "json" => DataType::Json,
            "zstd" => DataType::Zstd,
            _ => anyhow::bail!("Unknown data type: {}", value),
        };
        Ok((data_type, next_index))
    }
}

/// Summary of a thread reconcile pass. Reconcile never deletes a thread: the
/// losing duplicates are archived with a `merged_into` pointer to the canonical
/// thread and can be recovered by clearing that pointer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadReconcileSummary {
    /// Number of distinct dedup groups that contained more than one thread.
    pub duplicate_groups: usize,
    /// Number of threads archived (merged into a canonical thread).
    pub merged_threads: usize,
    /// Session ids of the archived threads, so callers can reconcile secondary
    /// indexes (e.g. the sidebar metadata store) against the same merge.
    pub merged_session_ids: Vec<String>,
}

/// Lifecycle of a review comment anchored to an agent edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCommentStatus {
    /// Anchored to its hunk and shown as active.
    Open,
    /// Marked done by the user; retained for history.
    Resolved,
    /// The anchor could not be relocated onto the current buffer (the hunk was
    /// rewritten wholesale, or the file left the action log). Kept visible so a
    /// comment is never silently lost, but not treated as attached to code.
    Unresolved,
}

impl ReviewCommentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Unresolved => "unresolved",
        }
    }

    pub fn from_db_str(value: &str) -> Self {
        match value {
            "resolved" => Self::Resolved,
            "unresolved" => Self::Unresolved,
            _ => Self::Open,
        }
    }
}

/// A persistable stand-in for a live `text::Anchor`.
///
/// `text::Anchor` is a CRDT position that is only meaningful against the buffer
/// replica that minted it and does not implement `Serialize`, so a review
/// comment stores this descriptor instead and re-mints an anchor on load via
/// [`AnchorDescriptor::resolve_offset`]. Relocation is by exact anchored text
/// first (nearest the recorded offset when the text repeats), which is what lets
/// a comment survive inserts and deletes even though the raw anchor cannot be
/// persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorDescriptor {
    /// Project-relative path of the commented file, as shown in the review pane.
    pub file_path: String,
    /// Exact text of the anchored region at creation time, bounded in length.
    /// Empty for a zero-width anchor (a pure insertion/deletion boundary).
    pub anchored_text: String,
    /// Byte offset of the anchor within the file at creation time. Used to pick
    /// between repeated matches, and as the fallback for a zero-width anchor.
    pub offset_hint: usize,
    /// Lines immediately preceding the anchor, captured for display and as a
    /// future fuzzy-relocation input.
    #[serde(default)]
    pub context_before: Vec<String>,
}

impl AnchorDescriptor {
    /// Cap on the anchored text stored per comment, so a whole-file "created"
    /// hunk does not bloat the row.
    const MAX_ANCHORED_TEXT_BYTES: usize = 1024;
    /// Number of preceding lines captured as context.
    const CONTEXT_LINES: usize = 3;

    /// Build a descriptor for the byte range `anchor_range` within `text`.
    pub fn from_text(file_path: String, text: &str, anchor_range: Range<usize>) -> Self {
        let start = anchor_range.start.min(text.len());
        let end = anchor_range.end.min(text.len()).max(start);
        Self {
            file_path,
            anchored_text: truncate_at_char_boundary(
                &text[start..end],
                Self::MAX_ANCHORED_TEXT_BYTES,
            )
            .to_string(),
            offset_hint: start,
            context_before: preceding_lines(text, start, Self::CONTEXT_LINES),
        }
    }

    /// Resolve this descriptor to a byte offset in `current_text`, or `None`
    /// when the anchor can no longer be trusted (the caller should mark the
    /// comment `Unresolved` rather than mis-anchor it).
    pub fn resolve_offset(&self, current_text: &str) -> Option<usize> {
        if self.anchored_text.is_empty() {
            // A zero-width anchor has no text to match; the recorded offset is
            // the best available position, clamped to the current buffer.
            return Some(self.offset_hint.min(current_text.len()));
        }

        let mut matches = current_text
            .match_indices(self.anchored_text.as_str())
            .map(|(offset, _)| offset);
        let first = matches.next()?;
        // Prefer the occurrence closest to where the anchor was recorded, so a
        // repeated block relocates to its own instance rather than the first.
        Some(matches.fold(first, |best, offset| {
            if offset.abs_diff(self.offset_hint) < best.abs_diff(self.offset_hint) {
                offset
            } else {
                best
            }
        }))
    }
}

/// A review comment anchored to a specific agent edit hunk.
#[derive(Debug, Clone)]
pub struct DbReviewComment {
    pub id: String,
    pub thread_id: acp::SessionId,
    pub anchor: AnchorDescriptor,
    pub body: String,
    pub status: ReviewCommentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn truncate_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The up-to-`count` whole lines immediately before `offset`, oldest first.
fn preceding_lines(text: &str, offset: usize, count: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let mut lines: Vec<String> = text[..offset]
        .lines()
        .rev()
        .take(count)
        .map(str::to_string)
        .collect();
    lines.reverse();
    lines
}

pub(crate) struct ThreadsDatabase {
    executor: BackgroundExecutor,
    connection: Arc<Mutex<Connection>>,
    /// In production, saves take real time (serialization, zstd, disk I/O) while
    /// the user keeps typing, so new save requests routinely arrive mid-write.
    /// The test executor completes writes instantly, so tests use this gate to
    /// hold a write in flight and interleave more save requests with it.
    #[cfg(test)]
    write_gate: Mutex<Option<Shared<futures::channel::oneshot::Receiver<()>>>>,
    /// Counts completed writes so a test can assert that a redundant save was
    /// skipped instead of re-serializing the thread.
    #[cfg(test)]
    save_writes: Arc<std::sync::atomic::AtomicUsize>,
}

struct GlobalThreadsDatabase(Shared<Task<Result<Arc<ThreadsDatabase>, Arc<anyhow::Error>>>>);

impl Global for GlobalThreadsDatabase {}

impl ThreadsDatabase {
    pub fn connect(cx: &mut App) -> Shared<Task<Result<Arc<ThreadsDatabase>, Arc<anyhow::Error>>>> {
        if cx.has_global::<GlobalThreadsDatabase>() {
            return cx.global::<GlobalThreadsDatabase>().0.clone();
        }
        let executor = cx.background_executor().clone();
        let task = executor
            .spawn({
                let executor = executor.clone();
                async move {
                    match ThreadsDatabase::new(executor) {
                        Ok(db) => Ok(Arc::new(db)),
                        Err(err) => Err(Arc::new(err)),
                    }
                }
            })
            .shared();

        cx.set_global(GlobalThreadsDatabase(task.clone()));
        task
    }

    pub fn new(executor: BackgroundExecutor) -> Result<Self> {
        let connection = if *ZED_STATELESS {
            Connection::open_memory(Some("THREAD_FALLBACK_DB"))
        } else if cfg!(any(feature = "test-support", test)) {
            // rust stores the name of the test on the current thread.
            // We use this to automatically create a database that will
            // be shared within the test (for the test_retrieve_old_thread)
            // but not with concurrent tests.
            let thread = std::thread::current();
            let test_name = thread.name();
            Connection::open_memory(Some(&format!(
                "THREAD_FALLBACK_{}",
                test_name.unwrap_or_default()
            )))
        } else {
            let threads_dir = paths::data_dir().join("threads");
            std::fs::create_dir_all(&threads_dir)?;
            let sqlite_path = threads_dir.join("threads.db");
            Connection::open_file(&sqlite_path.to_string_lossy())
        };

        connection.exec("PRAGMA journal_mode=WAL;")?()?;
        connection.exec("PRAGMA busy_timeout=1000;")?()?;

        connection.exec(indoc! {"
            CREATE TABLE IF NOT EXISTS threads (
                id TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                data_type TEXT NOT NULL,
                data BLOB NOT NULL
            )
        "})?()
        .map_err(|e| e.context("Failed to create threads table"))?;

        if let Ok(mut s) = connection.exec(indoc! {"
            ALTER TABLE threads ADD COLUMN parent_id TEXT
        "})
        {
            s().ok();
        }

        if let Ok(mut s) = connection.exec(indoc! {"
            ALTER TABLE threads ADD COLUMN folder_paths TEXT;
            ALTER TABLE threads ADD COLUMN folder_paths_order TEXT;
        "})
        {
            s().ok();
        }

        if let Ok(mut s) = connection.exec(indoc! {"
            ALTER TABLE threads ADD COLUMN workspace_id TEXT
        "})
        {
            s().ok();
        }

        if let Ok(mut s) = connection.exec(indoc! {"
            ALTER TABLE threads ADD COLUMN created_at TEXT;
        "})
        {
            if s().is_ok() {
                connection.exec(indoc! {"
                    UPDATE threads SET created_at = updated_at WHERE created_at IS NULL
                "})?()?;
            }
        }

        if let Ok(mut s) = connection.exec(indoc! {"
            ALTER TABLE threads ADD COLUMN dedup_key TEXT
        "})
        {
            s().ok();
        }

        if let Ok(mut s) = connection.exec(indoc! {"
            ALTER TABLE threads ADD COLUMN merged_into TEXT
        "})
        {
            s().ok();
        }

        // Phase C: review comments anchored to agent edit hunks. Kept in their
        // own table (not inside the thread blob) so comment edits stay off the
        // transcript-save path and the reconcile pass can re-point them when it
        // archives a merged-away thread.
        connection.exec(indoc! {"
                CREATE TABLE IF NOT EXISTS review_comments (
                    id TEXT PRIMARY KEY,
                    thread_id TEXT NOT NULL,
                    file_path TEXT NOT NULL,
                    anchor TEXT NOT NULL,
                    body TEXT NOT NULL,
                    status TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )
            "})?()
        .map_err(|e| e.context("Failed to create review_comments table"))?;

        connection.exec(indoc! {"
                CREATE INDEX IF NOT EXISTS review_comments_thread_id
                    ON review_comments(thread_id)
            "})?()
        .map_err(|e| e.context("Failed to create review_comments index"))?;

        let db = Self {
            executor,
            connection: Arc::new(Mutex::new(connection)),
            #[cfg(test)]
            write_gate: Mutex::new(None),
            #[cfg(test)]
            save_writes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };

        Ok(db)
    }

    fn save_thread_sync(
        connection: &Arc<Mutex<Connection>>,
        id: acp::SessionId,
        thread: DbThread,
        folder_paths: &PathList,
        workspace_id: Option<String>,
    ) -> Result<usize> {
        const COMPRESSION_LEVEL: i32 = 3;

        #[derive(Serialize)]
        struct SerializedThread {
            #[serde(flatten)]
            thread: DbThread,
            version: &'static str,
        }

        let title = thread.title.to_string();
        let updated_at = thread.updated_at.to_rfc3339();
        let parent_id = thread
            .subagent_context
            .as_ref()
            .map(|ctx| ctx.parent_thread_id.0.clone());
        let dedup_key = thread_dedup_key(workspace_id.as_deref(), folder_paths, &thread.messages);
        let serialized_folder_paths = folder_paths.serialize();
        let (folder_paths_str, folder_paths_order_str): (Option<String>, Option<String>) =
            if folder_paths.is_empty() {
                (None, None)
            } else {
                (
                    Some(serialized_folder_paths.paths),
                    Some(serialized_folder_paths.order),
                )
            };
        let json_data = serde_json::to_string(&SerializedThread {
            thread,
            version: DbThread::VERSION,
        })?;

        let connection = connection.lock();

        let compressed = zstd::encode_all(json_data.as_bytes(), COMPRESSION_LEVEL)?;
        let data_type = DataType::Zstd;
        let data = compressed;

        // Use the thread's updated_at as created_at for new threads.
        // This ensures the creation time reflects when the thread was conceptually
        // created, not when it was saved to the database.
        let created_at = updated_at.clone();

        let id_for_dedup = id.0.clone();

        let mut insert = connection.exec_bound::<(Arc<str>, Option<Arc<str>>, Option<String>, Option<String>, Option<String>, String, String, DataType, Vec<u8>, String)>(indoc! {"
            INSERT INTO threads (id, parent_id, folder_paths, folder_paths_order, workspace_id, summary, updated_at, data_type, data, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(id) DO UPDATE SET
                parent_id = excluded.parent_id,
                folder_paths = excluded.folder_paths,
                folder_paths_order = excluded.folder_paths_order,
                workspace_id = COALESCE(excluded.workspace_id, threads.workspace_id),
                summary = excluded.summary,
                updated_at = excluded.updated_at,
                data_type = excluded.data_type,
                data = excluded.data
        "})?;

        insert((
            id.0,
            parent_id,
            folder_paths_str,
            folder_paths_order_str,
            workspace_id,
            title,
            updated_at,
            data_type,
            data,
            created_at,
        ))?;

        let mut update_dedup = connection.exec_bound::<(Option<String>, Arc<str>)>(indoc! {"
            UPDATE threads SET dedup_key = ?1 WHERE id = ?2
        "})?;
        update_dedup((dedup_key, id_for_dedup))?;

        Ok(json_data.len())
    }

    pub fn list_threads(&self) -> Task<Result<Vec<DbThreadMetadata>>> {
        let connection = self.connection.clone();

        self.executor.spawn(async move {
            let connection = connection.lock();

            let mut select = connection
                .select_bound::<(), (Arc<str>, Option<Arc<str>>, Option<String>, Option<String>, Option<String>, String, String, Option<String>)>(indoc! {"
                SELECT id, parent_id, folder_paths, folder_paths_order, workspace_id, summary, updated_at, created_at FROM threads WHERE merged_into IS NULL ORDER BY updated_at DESC, created_at DESC
            "})?;

            let rows = select(())?;
            let mut threads = Vec::new();

            for (id, parent_id, folder_paths, folder_paths_order, workspace_id, summary, updated_at, created_at) in rows {
                let folder_paths = folder_paths
                    .map(|paths| {
                        PathList::deserialize(&util::path_list::SerializedPathList {
                            paths,
                            order: folder_paths_order.unwrap_or_default(),
                        })
                    })
                    .unwrap_or_default();
                let created_at = created_at
                    .as_deref()
                    .map(DateTime::parse_from_rfc3339)
                    .transpose()?
                    .map(|dt| dt.with_timezone(&Utc));

                threads.push(DbThreadMetadata {
                    id: acp::SessionId::new(id),
                    parent_session_id: parent_id.map(acp::SessionId::new),
                    title: summary.into(),
                    updated_at: DateTime::parse_from_rfc3339(&updated_at)?.with_timezone(&Utc),
                    created_at,
                    folder_paths,
                    workspace_id,
                });
            }

            Ok(threads)
        })
    }

    pub fn load_thread(&self, id: acp::SessionId) -> Task<Result<Option<DbThread>>> {
        let connection = self.connection.clone();

        self.executor.spawn(async move {
            let connection = connection.lock();
            let mut select = connection
                .select_bound::<Arc<str>, (DataType, Vec<u8>, Option<String>)>(indoc! {"
                    SELECT data_type, data, merged_into FROM threads WHERE id = ? LIMIT 1
                "})?;

            // Follow `merged_into` pointers to the canonical thread, so opening
            // a thread that was reconciled into another opens the merged result.
            // The loop is bounded to guard against a malformed pointer cycle;
            // the merge only ever points losers at a canonical, so one hop is
            // the norm.
            let mut current_id = id.0;
            for _ in 0..64 {
                let rows = select(current_id.clone())?;
                let Some((data_type, data, merged_into)) = rows.into_iter().next() else {
                    return Ok(None);
                };
                match merged_into {
                    Some(next) => current_id = Arc::from(next.as_str()),
                    None => return Ok(Some(Self::deserialize_thread(data_type, data)?)),
                }
            }

            Ok(None)
        })
    }

    /// Returns the persisted `updated_at` for a thread without deserializing
    /// its full content. Used to detect when another Zed instance has written
    /// a newer copy of a thread to the shared database.
    pub fn thread_updated_at(&self, id: acp::SessionId) -> Task<Result<Option<DateTime<Utc>>>> {
        let connection = self.connection.clone();

        self.executor.spawn(async move {
            let connection = connection.lock();
            let mut select = connection.select_row_bound::<Arc<str>, String>(indoc! {"
                SELECT updated_at FROM threads WHERE id = ? LIMIT 1
            "})?;

            let Some(updated_at) = select(id.0)? else {
                return Ok(None);
            };
            Ok(Some(
                DateTime::parse_from_rfc3339(&updated_at)?.with_timezone(&Utc),
            ))
        })
    }

    pub fn save_thread(
        &self,
        id: acp::SessionId,
        thread: DbThread,
        folder_paths: PathList,
        workspace_id: Option<String>,
    ) -> Task<Result<usize>> {
        let connection = self.connection.clone();
        #[cfg(test)]
        let write_gate = self.write_gate.lock().clone();
        #[cfg(test)]
        let save_writes = self.save_writes.clone();

        self.executor.spawn(async move {
            #[cfg(test)]
            if let Some(write_gate) = write_gate {
                write_gate.await.ok();
            }
            #[cfg(test)]
            save_writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Self::save_thread_sync(&connection, id, thread, &folder_paths, workspace_id)
        })
    }

    #[cfg(test)]
    pub fn set_write_gate(&self, gate: futures::channel::oneshot::Receiver<()>) {
        *self.write_gate.lock() = Some(gate.shared());
    }

    /// Number of writes that actually ran, so a test can assert a redundant save
    /// was skipped before it reached the serializer.
    #[cfg(test)]
    pub fn save_write_count(&self) -> usize {
        self.save_writes.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn deserialize_thread(data_type: DataType, data: Vec<u8>) -> Result<DbThread> {
        let json_data = match data_type {
            DataType::Zstd => {
                let decompressed = zstd::decode_all(&data[..])?;
                String::from_utf8(decompressed)?
            }
            DataType::Json => String::from_utf8(data)?,
        };
        DbThread::from_json(json_data.as_bytes())
    }

    fn sandboxed_terminal_temp_dir(data_type: DataType, data: Vec<u8>) -> Option<PathBuf> {
        match Self::deserialize_thread(data_type, data) {
            Ok(thread) => thread.sandboxed_terminal_temp_dir,
            Err(error) => {
                log::warn!("failed to deserialize thread before deleting it: {error:#}");
                None
            }
        }
    }

    fn remove_sandboxed_terminal_temp_dir(temp_dir: PathBuf) {
        match std::fs::remove_dir_all(&temp_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                log::warn!(
                    "failed to remove sandboxed terminal temp directory {}: {error}",
                    temp_dir.display()
                );
            }
        }
    }

    pub fn delete_thread(&self, id: acp::SessionId) -> Task<Result<()>> {
        let connection = self.connection.clone();

        self.executor.spawn(async move {
            let sandboxed_terminal_temp_dirs = {
                let connection = connection.lock();

                let mut select_children =
                    connection.select_bound::<Arc<str>, Arc<str>>(indoc! {"
                    SELECT id FROM threads WHERE parent_id = ?
                "})?;

                // Collect target thread together with all of its transitive
                // subagent threads
                let mut ids_to_delete = vec![id.0.clone()];
                let mut frontier = vec![id.0.clone()];
                while let Some(parent) = frontier.pop() {
                    for child in select_children(parent)? {
                        ids_to_delete.push(child.clone());
                        frontier.push(child);
                    }
                }

                let mut select =
                    connection.select_bound::<Arc<str>, (DataType, Vec<u8>)>(indoc! {"
                    SELECT data_type, data FROM threads WHERE id = ? LIMIT 1
                "})?;

                let mut delete = connection.exec_bound::<Arc<str>>(indoc! {"
                    DELETE FROM threads WHERE id = ?
                "})?;

                let mut delete_comments = connection.exec_bound::<Arc<str>>(indoc! {"
                    DELETE FROM review_comments WHERE thread_id = ?
                "})?;

                let mut sandboxed_terminal_temp_dirs = Vec::new();
                for thread_id in ids_to_delete {
                    if let Some(temp_dir) = select(thread_id.clone())?.into_iter().next().and_then(
                        |(data_type, data)| Self::sandboxed_terminal_temp_dir(data_type, data),
                    ) {
                        sandboxed_terminal_temp_dirs.push(temp_dir);
                    }
                    delete(thread_id.clone())?;
                    delete_comments(thread_id)?;
                }

                sandboxed_terminal_temp_dirs
            };

            for temp_dir in sandboxed_terminal_temp_dirs {
                Self::remove_sandboxed_terminal_temp_dir(temp_dir);
            }

            Ok(())
        })
    }

    pub fn delete_threads(&self) -> Task<Result<()>> {
        let connection = self.connection.clone();

        self.executor.spawn(async move {
            let sandboxed_terminal_temp_dirs = {
                let connection = connection.lock();

                let mut select = connection.select_bound::<(), (DataType, Vec<u8>)>(indoc! {"
                    SELECT data_type, data FROM threads
                "})?;

                let sandboxed_terminal_temp_dirs = select(())?
                    .into_iter()
                    .filter_map(|(data_type, data)| {
                        Self::sandboxed_terminal_temp_dir(data_type, data)
                    })
                    .collect::<Vec<_>>();

                let mut delete = connection.exec_bound::<()>(indoc! {"
                    DELETE FROM threads
                "})?;

                delete(())?;

                sandboxed_terminal_temp_dirs
            };

            for temp_dir in sandboxed_terminal_temp_dirs {
                Self::remove_sandboxed_terminal_temp_dir(temp_dir);
            }

            Ok(())
        })
    }

    /// All review comments attached to `thread_id`, oldest first.
    pub fn list_review_comments(
        &self,
        thread_id: acp::SessionId,
    ) -> Task<Result<Vec<DbReviewComment>>> {
        let connection = self.connection.clone();

        self.executor.spawn(async move {
            let connection = connection.lock();
            let mut select = connection
                .select_bound::<Arc<str>, (String, String, String, String, String, String, String)>(
                    indoc! {"
                SELECT id, file_path, anchor, body, status, created_at, updated_at
                FROM review_comments
                WHERE thread_id = ?
                ORDER BY created_at ASC, id ASC
            "},
                )?;

            let mut comments = Vec::new();
            for (id, file_path, anchor, body, status, created_at, updated_at) in
                select(thread_id.0.clone())?
            {
                let mut anchor =
                    serde_json::from_str::<AnchorDescriptor>(&anchor).unwrap_or_else(|_| {
                        AnchorDescriptor {
                            file_path: file_path.clone(),
                            anchored_text: String::new(),
                            offset_hint: 0,
                            context_before: Vec::new(),
                        }
                    });
                // The path has its own column; keep the parsed descriptor in sync
                // if the stored JSON is stale or damaged.
                anchor.file_path = file_path;

                comments.push(DbReviewComment {
                    id,
                    thread_id: thread_id.clone(),
                    anchor,
                    body,
                    status: ReviewCommentStatus::from_db_str(&status),
                    created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                    updated_at: DateTime::parse_from_rfc3339(&updated_at)?.with_timezone(&Utc),
                });
            }

            Ok(comments)
        })
    }

    /// Insert a new review comment or update an existing one (matched by id).
    pub fn upsert_review_comment(&self, comment: DbReviewComment) -> Task<Result<()>> {
        let connection = self.connection.clone();

        self.executor.spawn(async move {
            let connection = connection.lock();
            let anchor = serde_json::to_string(&comment.anchor)?;

            let mut upsert = connection.exec_bound::<(
                String,
                Arc<str>,
                String,
                String,
                String,
                String,
                String,
                String,
            )>(indoc! {"
                INSERT INTO review_comments
                    (id, thread_id, file_path, anchor, body, status, created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(id) DO UPDATE SET
                    thread_id = excluded.thread_id,
                    file_path = excluded.file_path,
                    anchor = excluded.anchor,
                    body = excluded.body,
                    status = excluded.status,
                    updated_at = excluded.updated_at
            "})?;

            upsert((
                comment.id,
                comment.thread_id.0,
                comment.anchor.file_path.clone(),
                anchor,
                comment.body,
                comment.status.as_str().to_string(),
                comment.created_at.to_rfc3339(),
                comment.updated_at.to_rfc3339(),
            ))?;

            Ok(())
        })
    }

    /// Delete a single review comment by id.
    pub fn delete_review_comment(&self, id: String) -> Task<Result<()>> {
        let connection = self.connection.clone();

        self.executor.spawn(async move {
            let connection = connection.lock();
            let mut delete = connection.exec_bound::<String>(indoc! {"
                DELETE FROM review_comments WHERE id = ?
            "})?;
            delete(id)?;
            Ok(())
        })
    }

    /// Reconcile diverged threads in the shared threads database.
    ///
    /// Threads that share a non-null `dedup_key` (same workspace identity and
    /// first user message) are collapsed into a single canonical thread (the
    /// oldest by `created_at`). Losers are archived via `merged_into`, never
    /// deleted, so a reconcile is reversible. The whole pass runs in one
    /// SQLite savepoint.
    pub(crate) fn reconcile_threads(&self) -> Task<Result<ThreadReconcileSummary>> {
        let connection = self.connection.clone();
        self.executor
            .spawn(async move { reconcile_threads_sync(&connection) })
    }

    /// Reconcile a single thread, identified by its session id, against any
    /// other thread that shares its dedup key. Only that dedup group is
    /// touched; unrelated threads are left alone. Used when opening a thread
    /// so that a duplicate opens the merged result.
    pub(crate) fn reconcile_thread(
        &self,
        id: acp::SessionId,
    ) -> Task<Result<ThreadReconcileSummary>> {
        let connection = self.connection.clone();
        self.executor
            .spawn(async move { reconcile_thread_sync(&connection, &id.0) })
    }

    /// Resolve a session id through any `merged_into` pointers to the
    /// canonical thread it belongs to. Returns the input id unchanged when it
    /// has not been merged, and `None` when no such thread exists.
    pub fn resolve_thread_id(&self, id: acp::SessionId) -> Task<Result<Option<acp::SessionId>>> {
        let connection = self.connection.clone();
        self.executor.spawn(async move {
            let connection = connection.lock();
            let mut select = connection.select_bound::<Arc<str>, Option<String>>(indoc! {"
                    SELECT merged_into FROM threads WHERE id = ? LIMIT 1
                "})?;

            let mut current_id = id.0;
            for _ in 0..64 {
                let rows = select(current_id.clone())?;
                let Some(merged_into) = rows.into_iter().next() else {
                    return Ok(None);
                };
                match merged_into {
                    Some(next) => current_id = Arc::from(next.as_str()),
                    None => return Ok(Some(acp::SessionId::new(current_id))),
                }
            }

            Ok(None)
        })
    }
}

/// Returns the concatenated text of the first user message in a thread, so a
/// dedup key can be derived from "what the user first asked" rather than the
/// mutable title. Mentions contribute their text; images do not.
fn first_user_message_text(messages: &[Arc<DbMessage>]) -> String {
    for message in messages {
        if let crate::Message::User(user) = message.as_ref() {
            let mut text = String::new();
            for block in user.content.iter() {
                match block {
                    UserMessageContent::Text(t) => text.push_str(t),
                    UserMessageContent::Mention { content, .. } => {
                        text.push(' ');
                        text.push_str(content);
                    }
                    UserMessageContent::Image(_) => {}
                }
            }
            return text;
        }
    }
    String::new()
}

/// Stable, dependency-free 64-bit FNV-1a. Used for thread dedup only: it is not
/// a cryptographic hash, and a collision merely over-merges two threads, which
/// the reconcile pass keeps reversible via archive + link.
fn fnv1a_64(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Derives a stable dedup key for a thread from its workspace identity and its
/// first user message. Two records with the same key are the same logical thread.
fn thread_dedup_key(
    workspace_id: Option<&str>,
    folder_paths: &PathList,
    messages: &[Arc<DbMessage>],
) -> Option<String> {
    let folder_paths_key = {
        let serialized = folder_paths.serialize();
        if serialized.paths.is_empty() {
            String::new()
        } else {
            serialized.paths
        }
    };
    let group_key = workspace_id.unwrap_or(folder_paths_key.as_str());
    let first_user_text = first_user_message_text(messages);
    if group_key.is_empty() && first_user_text.is_empty() {
        return None;
    }
    Some(format!(
        "{:016x}",
        fnv1a_64(&format!("{}\0{}", group_key, first_user_text))
    ))
}

/// Serialize a thread to the zstd-compressed JSON blob stored in the `data`
/// column, using the same `version`-tagged envelope as `save_thread_sync`.
fn serialize_thread_blob(thread: &DbThread) -> Result<Vec<u8>> {
    const COMPRESSION_LEVEL: i32 = 3;

    #[derive(Serialize)]
    struct SerializedThread<'a> {
        #[serde(flatten)]
        thread: &'a DbThread,
        version: &'static str,
    }

    let json_data = serde_json::to_string(&SerializedThread {
        thread,
        version: DbThread::VERSION,
    })?;
    Ok(zstd::encode_all(json_data.as_bytes(), COMPRESSION_LEVEL)?)
}

/// Reconcile diverged threads in the shared threads database.
///
/// Runs the merge policy in a single SQLite savepoint: threads that share a
/// `dedup_key` (same workspace identity and first user message) are collapsed
/// into the oldest canonical thread, and every duplicate is archived via
/// `merged_into`. Nothing is deleted, so a reconcile is reversible.
pub fn reconcile_threads(cx: &mut App) -> Task<Result<ThreadReconcileSummary>> {
    let database_future = ThreadsDatabase::connect(cx);
    let executor = cx.background_executor().clone();
    executor.spawn(async move {
        let database = database_future.await.map_err(|err| anyhow!(err))?;
        database.reconcile_threads().await
    })
}

/// Reconcile a single thread, identified by its session id, against its
/// dedup-group peers. See [`ThreadsDatabase::reconcile_thread`].
pub fn reconcile_thread(cx: &mut App, id: acp::SessionId) -> Task<Result<ThreadReconcileSummary>> {
    let database_future = ThreadsDatabase::connect(cx);
    let executor = cx.background_executor().clone();
    executor.spawn(async move {
        let database = database_future.await.map_err(|err| anyhow!(err))?;
        database.reconcile_thread(id).await
    })
}

/// Resolve a session id through any `merged_into` pointers to the canonical
/// thread it belongs to. Returns the input id unchanged when it has not been
/// merged. See [`ThreadsDatabase::resolve_thread_id`].
pub fn resolve_thread_id(cx: &mut App, id: acp::SessionId) -> Task<Result<Option<acp::SessionId>>> {
    let database_future = ThreadsDatabase::connect(cx);
    let executor = cx.background_executor().clone();
    executor.spawn(async move {
        let database = database_future.await.map_err(|err| anyhow!(err))?;
        database.resolve_thread_id(id).await
    })
}

/// Load the review comments persisted for `thread_id`, oldest first.
/// See [`ThreadsDatabase::list_review_comments`].
pub fn load_review_comments(
    cx: &mut App,
    thread_id: acp::SessionId,
) -> Task<Result<Vec<DbReviewComment>>> {
    let database_future = ThreadsDatabase::connect(cx);
    let executor = cx.background_executor().clone();
    executor.spawn(async move {
        let database = database_future.await.map_err(|err| anyhow!(err))?;
        database.list_review_comments(thread_id).await
    })
}

/// Insert or update a review comment.
/// See [`ThreadsDatabase::upsert_review_comment`].
pub fn save_review_comment(cx: &mut App, comment: DbReviewComment) -> Task<Result<()>> {
    let database_future = ThreadsDatabase::connect(cx);
    let executor = cx.background_executor().clone();
    executor.spawn(async move {
        let database = database_future.await.map_err(|err| anyhow!(err))?;
        database.upsert_review_comment(comment).await
    })
}

/// Delete a review comment by id.
/// See [`ThreadsDatabase::delete_review_comment`].
pub fn delete_review_comment(cx: &mut App, id: String) -> Task<Result<()>> {
    let database_future = ThreadsDatabase::connect(cx);
    let executor = cx.background_executor().clone();
    executor.spawn(async move {
        let database = database_future.await.map_err(|err| anyhow!(err))?;
        database.delete_review_comment(id).await
    })
}

fn reconcile_threads_sync(connection: &Arc<Mutex<Connection>>) -> Result<ThreadReconcileSummary> {
    reconcile_threads_scoped_sync(connection, None)
}

/// Reconcile only the dedup group that `id` belongs to, and nothing else.
fn reconcile_thread_sync(
    connection: &Arc<Mutex<Connection>>,
    id: &str,
) -> Result<ThreadReconcileSummary> {
    // Resolve `id` through any existing `merged_into` pointer first, then scope
    // the pass to that thread's dedup key.
    let scope = {
        let connection = connection.lock();
        let mut select =
            connection.select_bound::<Arc<str>, (Option<String>, Option<String>)>(indoc! {"
                SELECT dedup_key, merged_into FROM threads WHERE id = ? LIMIT 1
            "})?;
        let mut current_id: Arc<str> = Arc::<str>::from(id);
        let mut scope = None;
        for _ in 0..64 {
            let rows = select(current_id.clone())?;
            let Some((dedup_key, merged_into)) = rows.into_iter().next() else {
                break;
            };
            match merged_into {
                Some(next) => current_id = Arc::from(next.as_str()),
                None => {
                    scope = dedup_key;
                    break;
                }
            }
        }
        scope
    };

    match scope {
        Some(scope) => reconcile_threads_scoped_sync(connection, Some(scope.as_str())),
        None => Ok(ThreadReconcileSummary::default()),
    }
}

fn reconcile_threads_scoped_sync(
    connection: &Arc<Mutex<Connection>>,
    scope: Option<&str>,
) -> Result<ThreadReconcileSummary> {
    struct ReconcileRow {
        id: String,
        created_at: Option<String>,
        updated_at: String,
        data_type: DataType,
        data: Vec<u8>,
    }

    let connection = connection.lock();
    connection.with_savepoint("thread_reconcile", || {
        let mut select = connection.select_bound::<(), (
            Arc<str>,
            Option<String>,
            Option<String>,
            String,
            DataType,
            Vec<u8>,
        )>(indoc! {"
            SELECT id, dedup_key, created_at, updated_at, data_type, data
            FROM threads
            WHERE dedup_key IS NOT NULL AND merged_into IS NULL
        "})?;

        let mut groups: HashMap<String, Vec<ReconcileRow>> = HashMap::default();
        for (id, dedup_key, created_at, updated_at, data_type, data) in select(())? {
            let Some(dedup_key) = dedup_key else {
                continue;
            };
            groups.entry(dedup_key).or_default().push(ReconcileRow {
                id: id.to_string(),
                created_at,
                updated_at,
                data_type,
                data,
            });
        }

        if let Some(scope) = scope {
            groups.retain(|dedup_key, _| dedup_key.as_str() == scope);
        }

        let mut summary = ThreadReconcileSummary::default();

        for members in groups.into_values() {
            if members.len() < 2 {
                continue;
            }

            let mut members = members;
            members.sort_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| left.id.cmp(&right.id))
            });

            let canonical = &members[0];
            let mut merged_thread = ThreadsDatabase::deserialize_thread(
                canonical.data_type.clone(),
                canonical.data.clone(),
            )?;
            for member in members.iter().skip(1) {
                let other = ThreadsDatabase::deserialize_thread(
                    member.data_type.clone(),
                    member.data.clone(),
                )?;
                merge_message_lists(&mut merged_thread, &other);
            }

            let merged_data = serialize_thread_blob(&merged_thread)?;

            // Surface the merge with the most recent activity so the canonical
            // thread stays ordered where the user expects it.
            let mut max_updated_at =
                DateTime::parse_from_rfc3339(&canonical.updated_at)?.with_timezone(&Utc);
            for member in members.iter().skip(1) {
                let candidate =
                    DateTime::parse_from_rfc3339(&member.updated_at)?.with_timezone(&Utc);
                if candidate > max_updated_at {
                    max_updated_at = candidate;
                }
            }
            let max_updated_at = max_updated_at.to_rfc3339();

            let mut update_canonical = connection
                .exec_bound::<(Vec<u8>, DataType, String, Arc<str>)>(indoc! {"
                    UPDATE threads SET data = ?1, data_type = ?2, updated_at = ?3 WHERE id = ?4
                "})?;
            update_canonical((
                merged_data,
                DataType::Zstd,
                max_updated_at,
                Arc::from(canonical.id.as_str()),
            ))?;

            let mut archive = connection.exec_bound::<(Arc<str>, Arc<str>)>(indoc! {"
                    UPDATE threads SET merged_into = ?1 WHERE id = ?2
                "})?;
            let mut repoint_comments = connection.exec_bound::<(Arc<str>, Arc<str>)>(indoc! {"
                    UPDATE review_comments SET thread_id = ?1 WHERE thread_id = ?2
                "})?;
            for member in members.iter().skip(1) {
                archive((
                    Arc::from(canonical.id.as_str()),
                    Arc::from(member.id.as_str()),
                ))?;
                // Review comments follow the merge so they are never orphaned on
                // an archived thread.
                repoint_comments((
                    Arc::from(canonical.id.as_str()),
                    Arc::from(member.id.as_str()),
                ))?;
                summary.merged_session_ids.push(member.id.clone());
            }

            summary.duplicate_groups += 1;
            summary.merged_threads += members.len() - 1;
        }

        Ok(summary)
    })
}

/// A content-based, id-independent signature of a single message. Two messages
/// with the same signature are treated as the same conversation turn when
/// detecting the common prefix of diverged threads. Message ids are minted per
/// instance, so they are deliberately excluded from the signature.
fn message_signature(message: &DbMessage) -> Vec<String> {
    let mut parts = Vec::new();
    match message {
        crate::Message::User(user) => {
            parts.push("user".to_string());
            for block in user.content.iter() {
                match block {
                    UserMessageContent::Text(text) => parts.push(text.clone()),
                    UserMessageContent::Mention { content, .. } => parts.push(content.to_string()),
                    UserMessageContent::Image(_) => parts.push("<image>".to_string()),
                }
            }
        }
        crate::Message::Agent(agent) => {
            parts.push("agent".to_string());
            for content in &agent.content {
                match content {
                    AgentMessageContent::Text(text) => parts.push(text.clone()),
                    AgentMessageContent::Thinking { text, .. } => parts.push(text.clone()),
                    AgentMessageContent::RedactedThinking(_) => {
                        parts.push("<redacted>".to_string())
                    }
                    AgentMessageContent::ToolUse(tool) => {
                        parts.push(format!("tool:{}", tool.name));
                        parts.push(format!("input:{}", tool.raw_input));
                    }
                }
            }
            for result in agent.tool_results.values() {
                parts.push(format!("result:{}", result.tool_name));
                for part in &result.content {
                    match part {
                        LanguageModelToolResultContent::Text(text) => parts.push(text.to_string()),
                        LanguageModelToolResultContent::Image(_) => {
                            parts.push("<image>".to_string())
                        }
                    }
                }
            }
        }
        crate::Message::Resume => parts.push("resume".to_string()),
        crate::Message::Compaction(info) => {
            parts.push("compaction".to_string());
            match info {
                crate::CompactionInfo::Summary(text) => parts.push(text.to_string()),
                crate::CompactionInfo::ProviderNative { .. } => {
                    parts.push("provider-native".to_string())
                }
            }
        }
    }
    parts
}

/// Length of the longest common prefix of two message lists, compared by
/// content signature rather than message id.
fn common_message_prefix_len(left: &[Arc<DbMessage>], right: &[Arc<DbMessage>]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| message_signature(left) == message_signature(right))
        .count()
}

/// Merge `other`'s messages into `base`'s message list. Messages after the
/// longest common prefix are appended, preserving `base`'s order first. This is
/// the stable-order fallback for the divergence case (messages carry no
/// timestamps): it never drops a message, so the merge is lossless.
fn merge_message_lists(base: &mut DbThread, other: &DbThread) {
    let prefix_len = common_message_prefix_len(&base.messages, &other.messages);
    base.messages
        .extend(other.messages[prefix_len..].iter().cloned());
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use collections::HashMap;
    use gpui::TestAppContext;
    use std::sync::Arc;

    #[test]
    fn test_shared_thread_roundtrip() {
        let original = SharedThread {
            title: "Test Thread".into(),
            messages: vec![],
            updated_at: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            model: None,
            version: SharedThread::VERSION.to_string(),
        };

        let bytes = original.to_bytes().expect("Failed to serialize");
        let restored = SharedThread::from_bytes(&bytes).expect("Failed to deserialize");

        assert_eq!(restored.title, original.title);
        assert_eq!(restored.version, original.version);
        assert_eq!(restored.updated_at, original.updated_at);
    }

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

    fn user_message(text: &str) -> Arc<DbMessage> {
        Arc::new(crate::Message::User(UserMessage {
            id: ClientUserMessageId::new(),
            content: Arc::from([UserMessageContent::Text(text.to_string())]),
        }))
    }

    #[test]
    fn test_thread_dedup_key() {
        let timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        let mut a = make_thread("thread a", timestamp);
        a.messages.push(user_message("hello"));
        let mut b = make_thread("thread b", timestamp);
        b.messages.push(user_message("hello"));

        let key_a = thread_dedup_key(Some("ws-1"), &PathList::default(), &a.messages);
        let key_b = thread_dedup_key(Some("ws-1"), &PathList::default(), &b.messages);
        assert_eq!(key_a, key_b);
        assert!(key_a.is_some());

        // Different workspace identity => different key.
        let key_c = thread_dedup_key(Some("ws-2"), &PathList::default(), &a.messages);
        assert_ne!(key_a, key_c);

        // Different first message => different key.
        let mut d = make_thread("thread d", timestamp);
        d.messages.push(user_message("goodbye"));
        let key_d = thread_dedup_key(Some("ws-1"), &PathList::default(), &d.messages);
        assert_ne!(key_a, key_d);

        // No workspace, no folder paths, no messages => no key.
        let empty = make_thread("empty", timestamp);
        assert_eq!(
            thread_dedup_key(None, &PathList::default(), &empty.messages),
            None
        );
    }

    #[test]
    fn test_save_fingerprint_tracks_content_not_scroll() {
        let timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let folder_paths = PathList::default();

        let mut thread = make_thread("thread", timestamp);
        thread.messages.push(user_message("hello"));

        let content = thread.save_fingerprint(&folder_paths, None, false);
        let full = thread.save_fingerprint(&folder_paths, None, true);
        assert_eq!(content, full, "no scroll offset: both identities agree");

        // Moving only the scroll offset changes the full identity but not the
        // content identity, so a scroll-only save can be skipped on a large
        // thread.
        thread.ui_scroll_position = Some(SerializedScrollPosition {
            item_ix: 3,
            offset_in_item: 12.5,
        });
        assert_eq!(
            thread.save_fingerprint(&folder_paths, None, false),
            content,
            "the content identity must ignore the scroll offset",
        );
        assert_ne!(
            thread.save_fingerprint(&folder_paths, None, true),
            full,
            "the full identity must include the scroll offset",
        );

        // A new message changes the content identity.
        thread.messages.push(user_message("world"));
        assert_ne!(
            thread.save_fingerprint(&folder_paths, None, false),
            content,
            "adding a message must change the content identity",
        );

        // The workspace identity participates too.
        assert_ne!(
            thread.save_fingerprint(&folder_paths, Some("ws-1"), false),
            thread.save_fingerprint(&folder_paths, None, false),
            "the workspace identity must participate",
        );
    }

    #[gpui::test]
    async fn test_list_threads_orders_by_created_at(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let older_id = session_id("thread-a");
        let newer_id = session_id("thread-b");

        let older_thread = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        let newer_thread = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );

        database
            .save_thread(older_id.clone(), older_thread, PathList::default(), None)
            .await
            .unwrap();
        database
            .save_thread(newer_id.clone(), newer_thread, PathList::default(), None)
            .await
            .unwrap();

        let entries = database.list_threads().await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, newer_id);
        assert_eq!(entries[1].id, older_id);
    }

    #[gpui::test]
    async fn test_save_thread_replaces_metadata(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let thread_id = session_id("thread-a");
        let original_thread = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        let updated_thread = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );

        database
            .save_thread(
                thread_id.clone(),
                original_thread,
                PathList::default(),
                None,
            )
            .await
            .unwrap();
        database
            .save_thread(thread_id.clone(), updated_thread, PathList::default(), None)
            .await
            .unwrap();

        let entries = database.list_threads().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, thread_id);
        assert_eq!(entries[0].title.as_ref(), "Thread B");
        assert_eq!(
            entries[0].updated_at,
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap()
        );
        assert!(
            entries[0].created_at.is_some(),
            "created_at should be populated"
        );
    }

    fn make_review_comment(id: &str, thread: &str, body: &str) -> DbReviewComment {
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        DbReviewComment {
            id: id.to_string(),
            thread_id: session_id(thread),
            anchor: AnchorDescriptor {
                file_path: "src/main.rs".to_string(),
                anchored_text: "fn main() {}".to_string(),
                offset_hint: 0,
                context_before: Vec::new(),
            },
            body: body.to_string(),
            status: ReviewCommentStatus::Open,
            created_at: now,
            updated_at: now,
        }
    }

    #[gpui::test]
    async fn test_review_comments_roundtrip(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();
        database
            .upsert_review_comment(make_review_comment("c1", "thread-a", "first"))
            .await
            .unwrap();
        database
            .upsert_review_comment(make_review_comment("c2", "thread-a", "second"))
            .await
            .unwrap();
        database
            .upsert_review_comment(make_review_comment("c3", "thread-b", "other"))
            .await
            .unwrap();

        let comments = database
            .list_review_comments(session_id("thread-a"))
            .await
            .unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, "c1");
        assert_eq!(comments[0].body, "first");
        assert_eq!(comments[0].status, ReviewCommentStatus::Open);
        assert_eq!(comments[0].anchor.file_path, "src/main.rs");
        assert_eq!(comments[0].anchor.anchored_text, "fn main() {}");
        assert_eq!(comments[1].id, "c2");
    }

    #[gpui::test]
    async fn test_review_comment_upsert_updates_in_place(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();
        database
            .upsert_review_comment(make_review_comment("c1", "thread-a", "before"))
            .await
            .unwrap();

        let mut updated = make_review_comment("c1", "thread-a", "after");
        updated.status = ReviewCommentStatus::Resolved;
        database.upsert_review_comment(updated).await.unwrap();

        let comments = database
            .list_review_comments(session_id("thread-a"))
            .await
            .unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "after");
        assert_eq!(comments[0].status, ReviewCommentStatus::Resolved);
    }

    #[gpui::test]
    async fn test_review_comment_delete(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();
        database
            .upsert_review_comment(make_review_comment("c1", "thread-a", "x"))
            .await
            .unwrap();
        database
            .delete_review_comment("c1".to_string())
            .await
            .unwrap();
        assert!(
            database
                .list_review_comments(session_id("thread-a"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[gpui::test]
    async fn test_review_comments_follow_reconcile(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();
        let older_id = session_id("older");
        let newer_id = session_id("newer");
        let timestamp = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();

        let mut older = make_thread("Thread", timestamp);
        older.messages.push(user_message("hello reconcile"));
        let mut newer = make_thread("Thread", Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 1).unwrap());
        newer.messages.push(user_message("hello reconcile"));

        database
            .save_thread(older_id.clone(), older, PathList::default(), None)
            .await
            .unwrap();
        database
            .save_thread(newer_id.clone(), newer, PathList::default(), None)
            .await
            .unwrap();

        database
            .upsert_review_comment(make_review_comment("c1", "newer", "on the loser"))
            .await
            .unwrap();

        database.reconcile_threads().await.unwrap();

        let comments = database.list_review_comments(older_id).await.unwrap();
        assert_eq!(
            comments.len(),
            1,
            "comment should follow the merge to the canonical thread"
        );
        assert_eq!(comments[0].id, "c1");
        assert!(
            database
                .list_review_comments(session_id("newer"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_anchor_descriptor_resolves_exact_match() {
        let text = "line one\nline two\nfn target() {}\nline four\n";
        let start = text.find("fn target()").unwrap();
        let descriptor = AnchorDescriptor::from_text(
            "src/lib.rs".to_string(),
            text,
            start..start + "fn target()".len(),
        );
        assert_eq!(descriptor.resolve_offset(text), Some(start));
        assert!(
            descriptor
                .context_before
                .iter()
                .any(|line| line == "line two")
        );
    }

    #[test]
    fn test_anchor_descriptor_follows_offset_after_insert() {
        let original = "alpha\nbravo\ntarget\ncharlie\n";
        let start = original.find("target").unwrap();
        let descriptor = AnchorDescriptor::from_text(
            "f.rs".to_string(),
            original,
            start..start + "target".len(),
        );

        let edited = "new line\nalpha\nbravo\ntarget\ncharlie\n";
        let expected = edited.find("target").unwrap();
        assert_eq!(descriptor.resolve_offset(edited), Some(expected));
    }

    #[test]
    fn test_anchor_descriptor_prefers_nearest_repeated_match() {
        let original = "dup\nx\ndup\ndup\n";
        // Anchor the second occurrence.
        let start = original.match_indices("dup").nth(1).unwrap().0;
        let descriptor =
            AnchorDescriptor::from_text("f.rs".to_string(), original, start..start + "dup".len());
        assert_eq!(descriptor.resolve_offset(original), Some(start));
    }

    #[test]
    fn test_anchor_descriptor_unresolved_when_text_removed() {
        let original = "keep\nremove me\nkeep\n";
        let start = original.find("remove me").unwrap();
        let descriptor = AnchorDescriptor::from_text(
            "f.rs".to_string(),
            original,
            start..start + "remove me".len(),
        );
        assert_eq!(descriptor.resolve_offset("keep\nkeep\n"), None);
    }

    #[test]
    fn test_subagent_context_defaults_to_none() {
        let json = r#"{
            "title": "Old Thread",
            "messages": [],
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;

        let db_thread: DbThread = serde_json::from_str(json).expect("Failed to deserialize");

        assert!(
            db_thread.subagent_context.is_none(),
            "Legacy threads without subagent_context should default to None"
        );
    }

    #[test]
    fn test_draft_prompt_defaults_to_none() {
        let json = r#"{
            "title": "Old Thread",
            "messages": [],
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;

        let db_thread: DbThread = serde_json::from_str(json).expect("Failed to deserialize");

        assert!(
            db_thread.draft_prompt.is_none(),
            "Legacy threads without draft_prompt field should default to None"
        );
    }

    #[test]
    fn test_sandboxed_terminal_temp_dir_defaults_to_none() {
        let json = r#"{
            "title": "Old Thread",
            "messages": [],
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;

        let db_thread: DbThread = serde_json::from_str(json).expect("Failed to deserialize");

        assert!(
            db_thread.sandboxed_terminal_temp_dir.is_none(),
            "Legacy threads without sandboxed_terminal_temp_dir should default to None"
        );
    }

    #[test]
    fn test_sandbox_grants_default_when_absent() {
        let json = r#"{
            "title": "Old Thread",
            "messages": [],
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;

        let db_thread: DbThread = serde_json::from_str(json).expect("Failed to deserialize");

        assert_eq!(
            db_thread.sandbox_grants,
            DbSandboxGrants::default(),
            "Legacy threads without sandbox_grants should default to empty grants"
        );
    }

    #[gpui::test]
    async fn test_sandbox_grants_roundtrip_through_save_load(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();
        let thread_id = session_id("sandbox-grants-thread");
        let mut thread = make_thread(
            "Sandbox Grants Thread",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        let grants = DbSandboxGrants {
            write_paths: vec![
                // A legacy bare-string grant (no resolved canonical) and a grant
                // carrying its resolved canonical, to exercise both forms of the
                // string-or-object round-trip.
                settings::GrantedWritePath::from_requested(PathBuf::from("/tmp/build")),
                settings::GrantedWritePath::resolved(
                    PathBuf::from("/tmp/link"),
                    PathBuf::from("/tmp/real"),
                ),
            ],
            network_hosts: vec!["github.com".to_string(), "*.npmjs.org".to_string()],
            network_any_host: false,
            allow_fs_write_all: false,
            unsandboxed: true,
            sandbox_fallback: true,
        };
        thread.sandbox_grants = grants.clone();

        database
            .save_thread(thread_id.clone(), thread, PathList::default(), None)
            .await
            .unwrap();

        let loaded = database
            .load_thread(thread_id)
            .await
            .unwrap()
            .expect("thread should exist");
        assert_eq!(loaded.sandbox_grants, grants);
    }

    #[gpui::test]
    async fn test_sandboxed_terminal_temp_dir_roundtrips_through_save_load(
        cx: &mut TestAppContext,
    ) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();
        let thread_id = session_id("sandbox-temp-dir-thread");
        let temp_dir = tempfile::Builder::new()
            .prefix("zed-agent-terminal-test-")
            .tempdir()
            .unwrap()
            .keep();
        let mut thread = make_thread(
            "Sandbox Temp Dir Thread",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        thread.sandboxed_terminal_temp_dir = Some(temp_dir.clone());

        database
            .save_thread(thread_id.clone(), thread, PathList::default(), None)
            .await
            .unwrap();

        let loaded = database
            .load_thread(thread_id)
            .await
            .unwrap()
            .expect("thread should exist");
        assert_eq!(loaded.sandboxed_terminal_temp_dir, Some(temp_dir.clone()));
        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    #[gpui::test]
    async fn test_delete_thread_removes_sandboxed_terminal_temp_dir(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();
        let thread_id = session_id("sandbox-temp-dir-delete-thread");
        let temp_dir = tempfile::Builder::new()
            .prefix("zed-agent-terminal-test-")
            .tempdir()
            .unwrap()
            .keep();
        std::fs::write(temp_dir.join("sentinel"), b"content").unwrap();
        let mut thread = make_thread(
            "Sandbox Temp Dir Delete Thread",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        thread.sandboxed_terminal_temp_dir = Some(temp_dir.clone());

        database
            .save_thread(thread_id.clone(), thread, PathList::default(), None)
            .await
            .unwrap();
        database.delete_thread(thread_id).await.unwrap();

        assert!(!temp_dir.exists());
    }

    #[gpui::test]
    async fn test_delete_thread_deletes_subagent_threads(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let parent_id = session_id("parent-thread");
        let child_id = session_id("child-thread");
        let grandchild_id = session_id("grandchild-thread");
        let unrelated_id = session_id("unrelated-thread");

        let parent_thread = make_thread(
            "Parent Thread",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );

        let mut child_thread = make_thread(
            "Child Subagent Thread",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        child_thread.subagent_context = Some(crate::SubagentContext {
            parent_thread_id: parent_id.clone(),
            depth: 1,
        });

        let mut grandchild_thread = make_thread(
            "Grandchild Subagent Thread",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        grandchild_thread.subagent_context = Some(crate::SubagentContext {
            parent_thread_id: child_id.clone(),
            depth: 2,
        });

        let unrelated_thread = make_thread(
            "Unrelated Thread",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );

        for (id, thread) in [
            (parent_id.clone(), parent_thread),
            (child_id.clone(), child_thread),
            (grandchild_id.clone(), grandchild_thread),
            (unrelated_id.clone(), unrelated_thread),
        ] {
            database
                .save_thread(id, thread, PathList::default(), None)
                .await
                .unwrap();
        }

        database.delete_thread(parent_id.clone()).await.unwrap();

        let remaining = database.list_threads().await.unwrap();
        let remaining_ids: Vec<_> = remaining.iter().map(|thread| thread.id.clone()).collect();
        assert_eq!(remaining_ids, vec![unrelated_id]);
    }

    #[gpui::test]
    async fn test_subagent_context_roundtrips_through_save_load(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let parent_id = session_id("parent-thread");
        let child_id = session_id("child-thread");

        let mut child_thread = make_thread(
            "Subagent Thread",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        child_thread.subagent_context = Some(crate::SubagentContext {
            parent_thread_id: parent_id.clone(),
            depth: 2,
        });

        database
            .save_thread(child_id.clone(), child_thread, PathList::default(), None)
            .await
            .unwrap();

        let loaded = database
            .load_thread(child_id)
            .await
            .unwrap()
            .expect("thread should exist");

        let context = loaded
            .subagent_context
            .expect("subagent_context should be restored");
        assert_eq!(context.parent_thread_id, parent_id);
        assert_eq!(context.depth, 2);
    }

    #[gpui::test]
    async fn test_non_subagent_thread_has_no_subagent_context(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let thread_id = session_id("regular-thread");
        let thread = make_thread(
            "Regular Thread",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );

        database
            .save_thread(thread_id.clone(), thread, PathList::default(), None)
            .await
            .unwrap();

        let loaded = database
            .load_thread(thread_id)
            .await
            .unwrap()
            .expect("thread should exist");

        assert!(
            loaded.subagent_context.is_none(),
            "Regular threads should have no subagent_context"
        );
    }

    #[gpui::test]
    async fn test_folder_paths_roundtrip(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let thread_id = session_id("folder-thread");
        let thread = make_thread(
            "Folder Thread",
            Utc.with_ymd_and_hms(2024, 6, 15, 12, 0, 0).unwrap(),
        );

        let folder_paths = PathList::new(&[
            std::path::PathBuf::from("/home/user/project-a"),
            std::path::PathBuf::from("/home/user/project-b"),
        ]);

        database
            .save_thread(thread_id.clone(), thread, folder_paths.clone(), None)
            .await
            .unwrap();

        let threads = database.list_threads().await.unwrap();
        assert_eq!(threads.len(), 1);
    }

    #[gpui::test]
    async fn test_folder_paths_empty_when_not_set(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let thread_id = session_id("no-folder-thread");
        let thread = make_thread(
            "No Folder Thread",
            Utc.with_ymd_and_hms(2024, 6, 15, 12, 0, 0).unwrap(),
        );

        database
            .save_thread(thread_id.clone(), thread, PathList::default(), None)
            .await
            .unwrap();

        let threads = database.list_threads().await.unwrap();
        assert_eq!(threads.len(), 1);
    }

    #[test]
    fn test_scroll_position_defaults_to_none() {
        let json = r#"{
            "title": "Old Thread",
            "messages": [],
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;

        let db_thread: DbThread = serde_json::from_str(json).expect("Failed to deserialize");

        assert!(
            db_thread.ui_scroll_position.is_none(),
            "Legacy threads without scroll_position field should default to None"
        );
    }

    #[gpui::test]
    async fn test_scroll_position_roundtrips_through_save_load(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let thread_id = session_id("thread-with-scroll");

        let mut thread = make_thread(
            "Thread With Scroll",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        thread.ui_scroll_position = Some(SerializedScrollPosition {
            item_ix: 42,
            offset_in_item: 13.5,
        });

        database
            .save_thread(thread_id.clone(), thread, PathList::default(), None)
            .await
            .unwrap();

        let loaded = database
            .load_thread(thread_id)
            .await
            .unwrap()
            .expect("thread should exist");

        let scroll = loaded
            .ui_scroll_position
            .expect("scroll_position should be restored");
        assert_eq!(scroll.item_ix, 42);
        assert!((scroll.offset_in_item - 13.5).abs() < f32::EPSILON);
    }

    fn user_texts(thread: &DbThread) -> Vec<String> {
        thread
            .messages
            .iter()
            .filter_map(|message| match message.as_ref() {
                crate::Message::User(user) => {
                    let mut text = String::new();
                    for block in user.content.iter() {
                        if let UserMessageContent::Text(part) = block {
                            text.push_str(part);
                        }
                    }
                    Some(text)
                }
                _ => None,
            })
            .collect()
    }

    #[gpui::test]
    async fn test_reconcile_merges_prefix_duplicate(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let older_id = session_id("thread-a");
        let newer_id = session_id("thread-b");

        let mut older = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        older.messages.push(user_message("hello"));
        older.messages.push(user_message("world"));

        let mut newer = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );
        newer.messages.push(user_message("hello"));

        database
            .save_thread(
                older_id.clone(),
                older,
                PathList::default(),
                Some("ws-1".into()),
            )
            .await
            .unwrap();
        database
            .save_thread(
                newer_id.clone(),
                newer,
                PathList::default(),
                Some("ws-1".into()),
            )
            .await
            .unwrap();

        let summary = database.reconcile_threads().await.unwrap();
        assert_eq!(summary.merged_threads, 1);
        assert_eq!(summary.merged_session_ids, vec![newer_id.0.to_string()]);

        let entries = database.list_threads().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, older_id);

        // The canonical kept the superset (the duplicate was a strict prefix).
        let merged = database
            .load_thread(older_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user_texts(&merged), vec!["hello", "world"]);

        // Opening the archived duplicate resolves to the canonical, merged
        // thread rather than returning its stale prefix.
        let via_duplicate = database.load_thread(newer_id).await.unwrap().unwrap();
        assert_eq!(user_texts(&via_duplicate), vec!["hello", "world"]);
    }

    #[gpui::test]
    async fn test_reconcile_interleaves_diverged_threads(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let older_id = session_id("thread-a");
        let newer_id = session_id("thread-b");

        let mut older = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        older.messages.push(user_message("hello"));
        older.messages.push(user_message("alpha"));

        let mut newer = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );
        newer.messages.push(user_message("hello"));
        newer.messages.push(user_message("beta"));

        database
            .save_thread(
                older_id.clone(),
                older,
                PathList::default(),
                Some("ws-1".into()),
            )
            .await
            .unwrap();
        database
            .save_thread(
                newer_id.clone(),
                newer,
                PathList::default(),
                Some("ws-1".into()),
            )
            .await
            .unwrap();

        let summary = database.reconcile_threads().await.unwrap();
        assert_eq!(summary.merged_threads, 1);

        // Canonical order first, then the duplicate's unique trailing messages.
        let merged = database.load_thread(older_id).await.unwrap().unwrap();
        assert_eq!(user_texts(&merged), vec!["hello", "alpha", "beta"]);
    }

    #[gpui::test]
    async fn test_reconcile_never_merges_across_workspaces(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let first_id = session_id("thread-a");
        let second_id = session_id("thread-b");

        let mut first = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        first.messages.push(user_message("hello"));

        let mut second = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );
        second.messages.push(user_message("hello"));

        database
            .save_thread(
                first_id.clone(),
                first,
                PathList::default(),
                Some("ws-1".into()),
            )
            .await
            .unwrap();
        database
            .save_thread(
                second_id.clone(),
                second,
                PathList::default(),
                Some("ws-2".into()),
            )
            .await
            .unwrap();

        let summary = database.reconcile_threads().await.unwrap();
        assert_eq!(summary.merged_threads, 0);
        assert_eq!(summary.duplicate_groups, 0);

        let entries = database.list_threads().await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[gpui::test]
    async fn test_reconcile_noop_without_duplicates(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let first_id = session_id("thread-a");
        let second_id = session_id("thread-b");

        let mut first = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        first.messages.push(user_message("hello"));

        let mut second = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );
        second.messages.push(user_message("goodbye"));

        database
            .save_thread(
                first_id.clone(),
                first,
                PathList::default(),
                Some("ws-1".into()),
            )
            .await
            .unwrap();
        database
            .save_thread(
                second_id.clone(),
                second,
                PathList::default(),
                Some("ws-1".into()),
            )
            .await
            .unwrap();

        let summary = database.reconcile_threads().await.unwrap();
        assert_eq!(summary.merged_threads, 0);

        let entries = database.list_threads().await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[gpui::test]
    async fn test_reconcile_thread_scopes_to_single_group(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let group_a_older = session_id("group-a-older");
        let group_a_newer = session_id("group-a-newer");
        let group_b_older = session_id("group-b-older");
        let group_b_newer = session_id("group-b-newer");

        let mut group_a_older_thread = make_thread(
            "Group A Older",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        group_a_older_thread.messages.push(user_message("hello"));
        group_a_older_thread.messages.push(user_message("alpha"));

        let mut group_a_newer_thread = make_thread(
            "Group A Newer",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );
        group_a_newer_thread.messages.push(user_message("hello"));

        let mut group_b_older_thread = make_thread(
            "Group B Older",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        group_b_older_thread.messages.push(user_message("goodbye"));

        let mut group_b_newer_thread = make_thread(
            "Group B Newer",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );
        group_b_newer_thread.messages.push(user_message("goodbye"));

        for (id, thread) in [
            (group_a_older.clone(), group_a_older_thread),
            (group_a_newer.clone(), group_a_newer_thread),
            (group_b_older.clone(), group_b_older_thread),
            (group_b_newer.clone(), group_b_newer_thread),
        ] {
            database
                .save_thread(id, thread, PathList::default(), Some("ws-1".into()))
                .await
                .unwrap();
        }

        // Scoped to group A: only its two threads collapse; group B is untouched.
        let summary = database
            .reconcile_thread(group_a_newer.clone())
            .await
            .unwrap();
        assert_eq!(summary.merged_threads, 1);
        assert_eq!(
            summary.merged_session_ids,
            vec![group_a_newer.0.to_string()]
        );

        let entries = database.list_threads().await.unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().any(|entry| entry.id == group_a_older));
        assert!(entries.iter().any(|entry| entry.id == group_b_older));
        assert!(entries.iter().any(|entry| entry.id == group_b_newer));
    }

    #[gpui::test]
    async fn test_resolve_thread_id_follows_merged_into(cx: &mut TestAppContext) {
        let database = ThreadsDatabase::new(cx.executor()).unwrap();

        let older_id = session_id("thread-a");
        let newer_id = session_id("thread-b");

        let mut older = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        older.messages.push(user_message("hello"));

        let mut newer = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );
        newer.messages.push(user_message("hello"));

        database
            .save_thread(
                older_id.clone(),
                older,
                PathList::default(),
                Some("ws-1".into()),
            )
            .await
            .unwrap();
        database
            .save_thread(
                newer_id.clone(),
                newer,
                PathList::default(),
                Some("ws-1".into()),
            )
            .await
            .unwrap();

        // Before reconciling, neither id resolves anywhere.
        assert_eq!(
            database.resolve_thread_id(newer_id.clone()).await.unwrap(),
            Some(newer_id.clone()),
        );

        database.reconcile_threads().await.unwrap();

        // The archived duplicate resolves to the canonical, and the canonical
        // resolves to itself.
        assert_eq!(
            database.resolve_thread_id(newer_id.clone()).await.unwrap(),
            Some(older_id.clone()),
        );
        assert_eq!(
            database.resolve_thread_id(older_id.clone()).await.unwrap(),
            Some(older_id),
        );
    }
}
