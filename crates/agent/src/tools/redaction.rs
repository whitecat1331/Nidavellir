//! Value- and pattern-based redaction for every boundary that feeds the
//! transcript, the thread database, or the logs.
//!
//! [`redact_sensitive`] is the pattern backstop that used to live in
//! `window_control.rs`; it was extracted here so the whole agent crate shares
//! one implementation. [`SecretRedactor`] adds the value-based scrub: secret
//! values the tool has *used* this session (plus the resident provider keys)
//! are held in a bounded in-memory LRU — never persisted — and replaced with
//! `<redacted>` wherever they would otherwise reach the model or the thread
//! blob. Patterns stay as the backstop for values this process has never seen.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use gpui::App;
use language_model::{
    LanguageModelProvider as _, LanguageModelRegistry, LanguageModelToolResultContent,
    LanguageModelToolUse,
};
use regex::Regex;

use crate::thread::{
    AgentMessage, AgentMessageContent, CompactionInfo, Message, UserMessage, UserMessageContent,
};

/// Upper bound on the number of recently-used secret values kept in memory.
const MAX_VALUES: usize = 16;
/// Values shorter than this are never scrubbed, to avoid turning a value like
/// `"if"` into an output-destroying blanket replacement.
const MIN_VALUE_LEN: usize = 6;

/// Replace the user's home directory prefix with `~`, so absolute paths in a
/// redacted string don't reveal the account name.
fn redact_home_dir(text: &str) -> String {
    let home = paths::home_dir();
    let home = home.to_string_lossy();
    if home.is_empty() {
        return text.to_owned();
    }
    text.replace(home.as_ref(), "~")
}

/// Substrings that are almost always credentials. Compiled once.
fn secret_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // Provider API keys (OpenAI, Anthropic, GitHub, Slack, AWS, Google).
            r"\bsk-[A-Za-z0-9_-]{16,}\b",
            r"\bsk-ant-[A-Za-z0-9_-]{16,}\b",
            r"\bgh[pousr]_[A-Za-z0-9]{20,}\b",
            r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b",
            r"\bAKIA[0-9A-Z]{16}\b",
            r"\bAIza[0-9A-Za-z_-]{30,}\b",
            // JSON Web Tokens: three base64url segments.
            r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}\b",
            // `key = value` / `key: value` assignments for secret-ish keys.
            r#"(?i)\b(?:api[_-]?key|secret|token|password|passwd|authorization|bearer)\b\s*[:=]\s*['\"]?[^\s'\"]{6,}"#,
            // `Bearer <token>` authorization headers.
            r"(?i)\bbearer\s+[A-Za-z0-9._-]{8,}",
        ]
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
    })
}

/// Redact likely secrets and private absolute paths from a string, using the
/// pattern backstop only (no value scrub). Conservative: it over-redacts and
/// only ever removes detail.
pub fn redact_sensitive(text: &str) -> String {
    let mut redacted = redact_home_dir(text);
    for pattern in secret_patterns() {
        redacted = pattern.replace_all(&redacted, "<redacted>").into_owned();
    }
    redacted
}

/// Holds the recently-used secret values this process has *seen* (e.g. a value
/// the `secret-tool` resolved to inject) so they can be scrubbed out of any
/// later output. In-memory only, LRU-capped; it is never written to disk.
#[derive(Default)]
pub struct SecretRedactor {
    values: VecDeque<String>,
}

impl SecretRedactor {
    /// Remember a value for subsequent scrubbing. Short values are ignored.
    pub fn note_value(&mut self, value: &str) {
        if value.len() < MIN_VALUE_LEN {
            return;
        }
        if let Some(position) = self.values.iter().position(|known| known == value) {
            self.values.remove(position);
        }
        self.values.push_front(value.to_owned());
        while self.values.len() > MAX_VALUES {
            self.values.pop_back();
        }
    }

    /// Redact known values (longest first, so overlapping values can't leave a
    /// fragment) and then apply the pattern backstop.
    pub fn redact(&self, text: &str) -> String {
        let mut redacted = redact_home_dir(text);
        let mut values: Vec<&str> = self.values.iter().map(String::as_str).collect();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        for value in values {
            redacted = redacted.replace(value, "<redacted>");
        }
        for pattern in secret_patterns() {
            redacted = pattern.replace_all(&redacted, "<redacted>").into_owned();
        }
        redacted
    }
}

fn redactor() -> &'static Mutex<SecretRedactor> {
    static REDACTOR: OnceLock<Mutex<SecretRedactor>> = OnceLock::new();
    REDACTOR.get_or_init(|| Mutex::new(SecretRedactor::default()))
}

/// Remember a value for subsequent scrubbing (no-op for short values).
pub fn note_secret_value(value: &str) {
    redactor()
        .lock()
        .expect("redactor poisoned")
        .note_value(value);
}

/// Redact known values plus the pattern backstop.
pub fn redact_all(text: &str) -> String {
    redactor().lock().expect("redactor poisoned").redact(text)
}

/// Add the authenticated providers' in-memory API keys to the scrub set. This
/// is the "resident provider keys" half of value redaction; the pattern
/// backstop already covers the common key shapes, and this catches keys that
/// don't match a pattern but have already been loaded into provider state.
pub fn note_provider_keys(cx: &App) {
    // `read_global` panics when no `LanguageModelRegistry` global is installed
    // (e.g. an agent unit test that serializes a thread without booting the
    // registry). Provider-key enrichment is best-effort — the pattern backstop
    // already covers the common key shapes — so treat an absent registry as a
    // no-op instead of panicking the save path.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let registry = LanguageModelRegistry::read_global(cx);
        for model in registry.available_models(cx) {
            let Ok(provider) = registry.provider_for_model(&model) else {
                continue;
            };
            if let Some(key) = provider.api_key(&model, cx) {
                note_secret_value(&key);
            }
        }
    }));
    if outcome.is_err() {
        // Best-effort only: fall back to pattern redaction.
    }
}

/// Scrub a tool result's model-visible content at the insertion boundary.
pub fn redact_tool_result_content(
    content: Vec<LanguageModelToolResultContent>,
) -> Vec<LanguageModelToolResultContent> {
    content
        .into_iter()
        .map(|part| match part {
            LanguageModelToolResultContent::Text(text) => {
                LanguageModelToolResultContent::Text(Arc::from(redact_all(&text)))
            }
            image @ LanguageModelToolResultContent::Image(_) => image,
        })
        .collect()
}

/// Recursively scrub every string in a raw tool output, in place.
pub fn redact_json_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(redact_all(&text)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(redact_json_value).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, redact_json_value(value)))
                .collect(),
        ),
        other => other,
    }
}

/// Scrub every text-bearing field of a thread's messages in place. Called on
/// the save path as defense-in-depth in case a value was learned *after* an
/// earlier message was already inserted.
pub fn redact_messages(messages: &mut Vec<Arc<Message>>) {
    for message in messages {
        let message = Arc::make_mut(message);
        match message {
            Message::User(user) => redact_user_message(user),
            Message::Agent(agent) => redact_agent_message(agent),
            Message::Resume => {}
            Message::Compaction(CompactionInfo::Summary(summary)) => {
                *summary = redact_all(summary).into();
            }
            Message::Compaction(CompactionInfo::ProviderNative { .. }) => {}
        }
    }
}

fn redact_user_message(user: &mut UserMessage) {
    let content = Arc::make_mut(&mut user.content);
    for part in content.iter_mut() {
        if let UserMessageContent::Text(text) = part {
            *text = redact_all(text);
        }
    }
}

fn redact_agent_message(agent: &mut AgentMessage) {
    for part in &mut agent.content {
        match part {
            AgentMessageContent::Text(text) => *text = redact_all(text),
            AgentMessageContent::Thinking { text, .. } => *text = redact_all(text),
            AgentMessageContent::RedactedThinking(text) => *text = redact_all(text),
            AgentMessageContent::ToolUse(tool_use) => redact_tool_use(tool_use),
        }
    }
    for result in agent.tool_results.values_mut() {
        result.content = redact_tool_result_content(std::mem::take(&mut result.content));
        if let Some(output) = &mut result.output {
            *output = redact_json_value(std::mem::take(output));
        }
    }
}

fn redact_tool_use(tool_use: &mut LanguageModelToolUse) {
    tool_use.raw_input = redact_all(&tool_use.raw_input);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_all_scrubs_known_values() {
        note_secret_value("hunter2");
        let text = "the password is hunter2 and it must not leak";
        assert!(!redact_all(text).contains("hunter2"));
        assert!(redact_all(text).contains("<redacted>"));
    }

    #[test]
    fn redact_all_ignores_short_values() {
        note_secret_value("if");
        assert_eq!(
            redact_all("nothing to hide if you ask me"),
            "nothing to hide if you ask me"
        );
    }

    #[test]
    fn redact_sensitive_removes_patterns_and_home() {
        let home = paths::home_dir().to_string_lossy().into_owned();
        let input = format!("open {home}/x\nOPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwx\n");
        let redacted = redact_sensitive(&input);
        assert!(!redacted.contains(&home));
        assert!(!redacted.contains("sk-abcdefghijklmnopqrstuvwx"));
        assert!(redacted.contains("<redacted>"));
    }
}
