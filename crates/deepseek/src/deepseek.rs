use anyhow::{Result, anyhow};
use futures::{
    AsyncBufReadExt, AsyncReadExt,
    io::BufReader,
    stream::{BoxStream, StreamExt},
};
use http_client::{
    AsyncBody, CustomHeaders, HttpClient, Method, Request as HttpRequest, RequestBuilderExt,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::convert::TryFrom;

pub const DEEPSEEK_API_URL: &str = "https://api.deepseek.com/v1";

#[derive(Clone, Copy, Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

impl TryFrom<String> for Role {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        match value.as_str() {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "system" => Ok(Self::System),
            "tool" => Ok(Self::Tool),
            _ => anyhow::bail!("invalid role '{value}'"),
        }
    }
}

impl From<Role> for String {
    fn from(val: Role) -> Self {
        match val {
            Role::User => "user".to_owned(),
            Role::Assistant => "assistant".to_owned(),
            Role::System => "system".to_owned(),
            Role::Tool => "tool".to_owned(),
        }
    }
}

#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum Model {
    #[serde(rename = "deepseek-v4-flash")]
    V4Flash,
    #[serde(rename = "deepseek-flash")]
    Flash,
    #[serde(rename = "deepseek-v4-pro")]
    #[default]
    V4Pro,
    #[serde(rename = "custom")]
    Custom {
        name: String,
        /// The name displayed in the UI, such as in the agent panel model dropdown menu.
        display_name: Option<String>,
        max_tokens: u64,
        max_output_tokens: Option<u64>,
        /// Whether the model accepts image input.
        #[serde(default)]
        supports_images: bool,
    },
}

impl Model {
    pub fn default_fast() -> Self {
        Model::V4Flash
    }

    pub fn from_id(id: &str) -> Result<Self> {
        match id {
            "deepseek-v4-flash" => Ok(Self::V4Flash),
            "deepseek-flash" => Ok(Self::Flash),
            "deepseek-v4-pro" => Ok(Self::V4Pro),
            _ => anyhow::bail!("invalid model id {id}"),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::V4Flash => "deepseek-v4-flash",
            Self::Flash => "deepseek-flash",
            Self::V4Pro => "deepseek-v4-pro",
            Self::Custom { name, .. } => name,
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            Self::V4Flash => "DeepSeek V4 Flash",
            Self::Flash => "DeepSeek V4.1 Flash",
            Self::V4Pro => "DeepSeek V4 Pro",
            Self::Custom {
                name, display_name, ..
            } => display_name.as_ref().unwrap_or(name).as_str(),
        }
    }

    pub fn max_token_count(&self) -> u64 {
        match self {
            // The API's real maximum context length for deepseek-v4-pro is
            // 1,048,576 (live 400 response, 2026-10-18: "maximum context length
            // is 1048576 tokens"). Declaring less under-counts the window for
            // the UI ring and the agent's compaction thresholds. The flash
            // variants keep 1,000,000: no live evidence of a larger window,
            // and under-declaring them is the safe direction (the completion
            // clamp only ever shrinks the budget against this value).
            Self::V4Pro => 1_048_576,
            Self::V4Flash | Self::Flash => 1_000_000,
            Self::Custom { max_tokens, .. } => *max_tokens,
        }
    }

    pub fn max_output_tokens(&self) -> Option<u64> {
        match self {
            Self::V4Flash | Self::Flash | Self::V4Pro => Some(384_000),
            Self::Custom {
                max_output_tokens, ..
            } => *max_output_tokens,
        }
    }

    pub fn supports_images(&self) -> bool {
        match self {
            Self::Flash => true,
            Self::V4Flash | Self::V4Pro => false,
            Self::Custom {
                supports_images, ..
            } => *supports_images,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub model: String,
    pub messages: Vec<RequestMessage>,
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
}

/// Tokens reserved so a request whose size is only estimated never quite
/// reaches the API's context limit.
pub const COMPLETION_BUDGET_SAFETY_MARGIN: u64 = 16_384;

/// The floor for `max_tokens` when headroom is nearly exhausted, so a doomed
/// request still gets a positive completion budget and fails honestly rather
/// than being sent with a nonsensical zero.
pub const MIN_COMPLETION_BUDGET: u64 = 1_024;

/// Tokens charged for each image part. DeepSeek bills vision input per image
/// (resolution-banded), not by the base64 payload's byte count, so counting the
/// payload as text would massively over-estimate the input (an ~800 KB
/// screenshot ≈ 200K phantom tokens). A flat allowance keeps the estimate
/// conservative without letting screenshots inflate it past the window.
pub const IMAGE_TOKEN_ALLOWANCE: u64 = 1_600;

impl Request {
    /// Estimate of the request's input-token count using the "1 token ≈ 4
    /// bytes" heuristic over the *text* it actually sends, plus a fixed
    /// allowance per image. It feeds the completion-budget clamp and must not
    /// count base64 image payloads as text — doing so floors the budget on
    /// image-heavy threads (live 2026-10-23: eight screenshots added ~1.4M
    /// phantom tokens and silently truncated every turn).
    pub fn estimated_input_tokens(&self) -> u64 {
        let mut text_bytes = 0usize;
        let mut image_count = 0u64;

        for message in &self.messages {
            match message {
                RequestMessage::System { content } => text_bytes += content.len(),
                RequestMessage::User { content } => match content {
                    MessageContent::Plain(text) => text_bytes += text.len(),
                    MessageContent::Multipart(parts) => {
                        for part in parts {
                            match part {
                                MessagePart::Text { text } => text_bytes += text.len(),
                                MessagePart::Image { .. } => image_count += 1,
                            }
                        }
                    }
                },
                RequestMessage::Assistant {
                    content,
                    tool_calls,
                    reasoning_content,
                } => {
                    if let Some(content) = content {
                        text_bytes += content.len();
                    }
                    if let Some(reasoning) = reasoning_content {
                        text_bytes += reasoning.len();
                    }
                    for tool_call in tool_calls {
                        text_bytes += tool_call.id.len();
                        match &tool_call.content {
                            ToolCallContent::Function { function } => {
                                text_bytes += function.name.len();
                                text_bytes += function.arguments.len();
                            }
                        }
                    }
                }
                RequestMessage::Tool {
                    content,
                    tool_call_id,
                } => {
                    text_bytes += content.len();
                    text_bytes += tool_call_id.len();
                }
            }
        }

        // Tool definitions are text payloads that consume context; count them
        // the same way as messages.
        text_bytes += serde_json::to_string(&self.tools).unwrap_or_default().len();

        (text_bytes / 4) as u64 + image_count * IMAGE_TOKEN_ALLOWANCE
    }
}

/// Clamps the request's completion budget so `estimated_input + max_tokens`
/// cannot exceed the model's context window: the DeepSeek API rejects a
/// request whose messages plus reserved completion exceed its maximum context
/// length even when the messages alone fit (live 400, 2026-10-18).
pub fn clamp_completion_budget(request: &mut Request, context_window: u64) {
    let Some(max_tokens) = request.max_tokens else {
        return;
    };
    let headroom = context_window
        .saturating_sub(request.estimated_input_tokens())
        .saturating_sub(COMPLETION_BUDGET_SAFETY_MARGIN);
    request.max_tokens = Some(max_tokens.min(headroom).max(MIN_COMPLETION_BUDGET));
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Thinking {
    #[serde(rename = "type")]
    pub kind: ThinkingType,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingType {
    Enabled,
    Disabled,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    High,
    Max,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    #[serde(rename = "json_object")]
    JsonObject,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    None,
    Auto,
    Required,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolDefinition {
    Function { function: FunctionDefinition },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Plain(String),
    Multipart(Vec<MessagePart>),
}

impl MessageContent {
    pub fn push_part(&mut self, part: MessagePart) {
        match self {
            MessageContent::Plain(text) => {
                *self =
                    MessageContent::Multipart(vec![MessagePart::Text { text: text.clone() }, part]);
            }
            MessageContent::Multipart(parts) => parts.push(part),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(tag = "type")]
pub enum MessagePart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    Image { image_url: ImageUrl },
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum RequestMessage {
    Assistant {
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    User {
        content: MessageContent,
    },
    System {
        content: String,
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(flatten)]
    pub content: ToolCallContent,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolCallContent {
    Function { function: FunctionContent },
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct FunctionContent {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Response {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u64,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Choice {
    pub index: u32,
    pub message: RequestMessage,
    pub finish_reason: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StreamResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
    pub usage: Option<Usage>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StreamChoice {
    pub index: u32,
    pub delta: StreamDelta,
    pub finish_reason: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StreamDelta {
    pub role: Option<Role>,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallChunk>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ToolCallChunk {
    pub index: usize,
    pub id: Option<String>,
    pub function: Option<FunctionChunk>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct FunctionChunk {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

pub async fn stream_completion(
    client: &dyn HttpClient,
    api_url: &str,
    api_key: &str,
    request: Request,
    extra_headers: &CustomHeaders,
) -> Result<BoxStream<'static, Result<StreamResponse>>> {
    let uri = format!("{api_url}/chat/completions");
    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_key.trim()))
        .extra_headers(extra_headers)
        .body(AsyncBody::from(serde_json::to_string(&request)?))?;
    let mut response = client.send(request).await?;

    if response.status().is_success() {
        let reader = BufReader::new(response.into_body());
        Ok(reader
            .lines()
            .filter_map(|line| async move {
                match line {
                    Ok(line) => {
                        let line = line.strip_prefix("data: ")?;
                        if line == "[DONE]" {
                            None
                        } else {
                            Some(serde_json::from_str(line).map_err(Into::into))
                        }
                    }
                    Err(error) => Some(Err(anyhow!(error))),
                }
            })
            .boxed())
    } else {
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        anyhow::bail!(
            "Failed to connect to DeepSeek API: {} {}",
            response.status(),
            body,
        );
    }
}
