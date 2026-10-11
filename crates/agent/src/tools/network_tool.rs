use agent_client_protocol::schema::v1 as acp;
use agent_settings::builtin_profiles;
use anyhow::{Context as _, Result};
use browser_tools::{
    AgentBrowserApi, DrivenBy, InterceptionPattern, RequestFilter, ThrottleConditions,
    ThrottlePreset,
};
use gpui::{App, SharedString, Task, WeakEntity};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use super::browser_tool::permission_inputs;
use crate::{AgentTool, Thread, ToolCallEventStream, ToolInput, ToolPermissionContext};

/// Inspect and perturb the network traffic of a browser the agent controls.
///
/// This is the diagnosis surface over the same browser session the `browser`
/// tool drives: list requests, fetch response bodies and post data, read
/// cookies and WebSocket frames, wait for network idle, and export/import HAR.
/// Write operations throttle or take the page offline, block URLs, toggle cache
/// and service workers, set cookies/headers/user-agent, and intercept requests
/// via Fetch break/edit-and-continue.
///
/// Read operations are available in read-only modes. Write operations require a
/// mode that can change things (`write` or `execute`) and user permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetworkOperation {
    /// List requests from the capture, filtered and paginated.
    #[default]
    ListRequests,
    /// Fetch a single request by CDP request id.
    GetRequest,
    /// Fetch a (sliced) response body.
    GetResponseBody,
    /// Fetch a request's POST data.
    GetRequestPostData,
    /// Fetch the browser's cookies.
    GetCookies,
    /// Fetch WebSocket frames for a connection.
    GetWebSocketFrames,
    /// Wait until no requests are in flight.
    WaitForNetworkIdle,
    /// Export the capture as a HAR object.
    HarExport,
    /// Replace the capture with a HAR object.
    HarImport,
    /// List requests currently paused by Fetch interception.
    ListPausedRequests,
    /// Read the current throttle/offline/block/interception state.
    GetControlState,
    /// Set network throttling from a preset or custom conditions.
    SetThrottle,
    /// Toggle the offline state.
    SetOffline,
    /// Add URLs to the blocked list.
    BlockUrls,
    /// Remove URLs from the blocked list.
    UnblockUrls,
    /// Toggle the cache-disabled state.
    SetCacheDisabled,
    /// Toggle bypass of service workers.
    SetBypassServiceWorker,
    /// Clear the browser cache.
    ClearBrowserCache,
    /// Clear browser cookies.
    ClearBrowserCookies,
    /// Set a cookie.
    SetCookie,
    /// Delete cookies matching criteria.
    DeleteCookies,
    /// Override outgoing HTTP headers.
    SetExtraHttpHeaders,
    /// Override the user agent.
    SetUserAgent,
    /// Enable Fetch interception with URL patterns.
    SetInterception,
    /// Continue a paused request (optionally rewriting it).
    ContinueRequest,
    /// Fulfill a paused request with a synthetic response.
    FulfillRequest,
    /// Fail a paused request.
    FailRequest,
}

/// Custom throttle conditions for `set_throttle` (mutually exclusive with
/// `preset`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct NetworkThrottleInput {
    /// Whether to emulate being offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline: Option<bool>,
    /// Added latency in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// Download throughput in bytes per second (-1 disables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_throughput_bps: Option<i64>,
    /// Upload throughput in bytes per second (-1 disables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_throughput_bps: Option<i64>,
    /// Connection type (e.g. `cellular3g`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
}

/// A single Fetch interception URL pattern for `set_interception`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct NetworkInterceptionPatternInput {
    /// The CDP URL pattern (e.g. `https://api.example.com/*`).
    pub url_pattern: String,
    /// The request stage to intercept (`Request` or `Response`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_stage: Option<String>,
}

/// A single network operation and the fields it needs.
///
/// Kept as a flat struct (rather than a tagged enum) so the JSON schema
/// advertised to language-model providers is a `type: "object"` with an
/// `operation` discriminator.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct NetworkToolInput {
    /// Which network operation to run.
    pub operation: NetworkOperation,
    /// Browser session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    /// Target id within a session. When omitted, the session's active target is
    /// used (only for operations that talk to the page directly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    /// CDP request id, used by the request/body/disposition operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// URL filter (substring) for `list_requests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Method filter for `list_requests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Status filter for `list_requests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u32>,
    /// Resource-type filter for `list_requests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    /// Only failed requests for `list_requests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<bool>,
    /// Only in-flight requests for `list_requests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<bool>,
    /// Pagination cursor (request offset) for `list_requests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<usize>,
    /// Number of requests per page for `list_requests` (default 100, max 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Byte offset into a response body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// Maximum response-body bytes per call (default 64 KiB, max 1 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<usize>,
    /// How long the network must be idle before `wait_for_network_idle`
    /// returns (default 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_for_ms: Option<u64>,
    /// Upper bound on `wait_for_network_idle` (default 10000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Throttle preset name (`online`, `offline`, `slow-3g`, `fast-3g`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Custom throttle conditions (alternative to `preset`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<NetworkThrottleInput>,
    /// Offline flag for `set_offline`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline: Option<bool>,
    /// Toggle flag for `set_cache_disabled` and `set_bypass_service_worker`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// URL list for `block_urls`, `unblock_urls`, and `get_cookies`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
    /// Header map for `set_extra_http_headers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// User-agent string for `set_user_agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Cookie definition (`set_cookie`) or deletion criteria (`delete_cookies`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie: Option<Value>,
    /// Fetch interception URL patterns for `set_interception`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patterns: Option<Vec<NetworkInterceptionPatternInput>>,
    /// Rewrite fields (url, method, post_data, headers) for `continue_request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modifications: Option<Value>,
    /// Synthetic response (response_code, response_headers, body) for
    /// `fulfill_request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    /// CDP error reason for `fail_request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
    /// HAR object for `har_import`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub har: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NetworkToolOutput {
    Success {
        operation: String,
        message: String,
        data: Value,
    },
    Error {
        operation: Option<String>,
        error: String,
    },
}

impl From<NetworkToolOutput> for LanguageModelToolResultContent {
    fn from(output: NetworkToolOutput) -> Self {
        match output {
            NetworkToolOutput::Success {
                operation,
                message,
                data,
            } => {
                let data = serde_json::to_string_pretty(&data).unwrap_or_else(|error| {
                    format!("<failed to serialize network output: {error}>")
                });
                format!("Network `{operation}` succeeded: {message}\n\n```json\n{data}\n```").into()
            }
            NetworkToolOutput::Error { operation, error } => {
                let operation = operation.as_deref().unwrap_or("unknown");
                format!("Network `{operation}` failed: {error}").into()
            }
        }
    }
}

const DEFAULT_REQUESTS_PER_PAGE: usize = 100;
const MAX_REQUESTS_PER_PAGE: usize = 500;
const DEFAULT_BODY_BYTES: usize = 65_536;
const MAX_BODY_BYTES: usize = 1_048_576;

pub struct NetworkTool {
    api: Arc<AgentBrowserApi>,
    thread: WeakEntity<Thread>,
}

impl NetworkTool {
    pub fn new(thread: WeakEntity<Thread>, api: Arc<AgentBrowserApi>) -> Self {
        Self { api, thread }
    }

    fn is_read_only_profile(&self, cx: &App) -> bool {
        self.thread
            .read_with(cx, |thread, _| {
                builtin_profiles::is_read_only(thread.profile())
            })
            .unwrap_or(false)
    }

    fn ensure_write_mode(&self, operation: &str, cx: &gpui::AsyncApp) -> Result<()> {
        if cx.update(|cx| self.is_read_only_profile(cx)) {
            anyhow::bail!(
                "network.{operation} is not available in read-only modes. Switch to Write or Execute mode to change the browser network state."
            );
        }
        Ok(())
    }

    async fn run_operation(
        self: Arc<Self>,
        input: NetworkToolInput,
        operation: String,
        event_stream: ToolCallEventStream,
        cx: &mut gpui::AsyncApp,
    ) -> Result<NetworkToolOutput> {
        match input.operation {
            NetworkOperation::ListRequests => {
                let session_id = required_session(&input)?;
                let filter = RequestFilter {
                    url: input.url.clone(),
                    method: input.method.clone(),
                    status: input.status,
                    resource_type: input.resource_type.clone(),
                    failed: input.failed,
                    pending: input.pending,
                };
                let limit = validate_limit(input.limit)?;
                let offset = input.cursor.unwrap_or(0);
                let data = self
                    .api
                    .list_requests(session_id, &filter, offset, limit)
                    .await?;
                Ok(success(operation, "listed network requests", data))
            }
            NetworkOperation::GetRequest => {
                let session_id = required_session(&input)?;
                let request_id = input
                    .request_id
                    .as_deref()
                    .context("request_id is required for network get_request")?;
                let data = self.api.get_request(session_id, request_id).await?;
                Ok(success(operation, "fetched request", data))
            }
            NetworkOperation::GetResponseBody => {
                let session_id = required_session(&input)?;
                let request_id = input
                    .request_id
                    .as_deref()
                    .context("request_id is required for network get_response_body")?;
                let max_body_bytes = validate_body_limit(input.max_body_bytes)?;
                let offset = input.offset.unwrap_or(0);
                let data = self
                    .api
                    .get_response_body(
                        session_id,
                        input.target_id.as_deref(),
                        request_id,
                        offset,
                        max_body_bytes,
                    )
                    .await?;
                Ok(success(operation, "fetched response body", data))
            }
            NetworkOperation::GetRequestPostData => {
                let session_id = required_session(&input)?;
                let request_id = input
                    .request_id
                    .as_deref()
                    .context("request_id is required for network get_request_post_data")?;
                let data = self
                    .api
                    .get_request_post_data(session_id, input.target_id.as_deref(), request_id)
                    .await?;
                Ok(success(operation, "fetched request post data", data))
            }
            NetworkOperation::GetCookies => {
                let session_id = required_session(&input)?;
                let urls = input.urls.clone().unwrap_or_default();
                let data = self
                    .api
                    .get_cookies(session_id, input.target_id.as_deref(), &urls)
                    .await?;
                Ok(success(operation, "fetched cookies", data))
            }
            NetworkOperation::GetWebSocketFrames => {
                let session_id = required_session(&input)?;
                let request_id = input
                    .request_id
                    .as_deref()
                    .context("request_id is required for network get_websocket_frames")?;
                let data = self
                    .api
                    .get_websocket_frames(session_id, input.target_id.as_deref(), request_id)
                    .await?;
                Ok(success(operation, "fetched websocket frames", data))
            }
            NetworkOperation::WaitForNetworkIdle => {
                let session_id = required_session(&input)?;
                let idle_for_ms = input.idle_for_ms.unwrap_or(500);
                let timeout_ms = input.timeout_ms.unwrap_or(10_000);
                let data = self
                    .api
                    .wait_for_network_idle(session_id, idle_for_ms, timeout_ms)
                    .await?;
                Ok(success(operation, "waited for network idle", data))
            }
            NetworkOperation::HarExport => {
                let session_id = required_session(&input)?;
                let data = self.api.export_har(session_id).await?;
                Ok(success(operation, "exported HAR", data))
            }
            NetworkOperation::ListPausedRequests => {
                let session_id = required_session(&input)?;
                let data = self
                    .api
                    .list_paused_requests(session_id, input.target_id.as_deref())
                    .await?;
                Ok(success(operation, "listed paused requests", data))
            }
            NetworkOperation::GetControlState => {
                let session_id = required_session(&input)?;
                let data = self.api.network_control_state(session_id).await?;
                Ok(success(operation, "read network control state", data))
            }
            NetworkOperation::SetThrottle => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let conditions = if let Some(preset) = &input.preset {
                    ThrottlePreset::parse(preset)
                        .context("unknown throttle preset")?
                        .conditions()
                } else if let Some(custom) = &input.conditions {
                    ThrottleConditions {
                        offline: custom.offline.unwrap_or(false),
                        latency_ms: custom.latency_ms.unwrap_or(0),
                        download_throughput_bps: custom.download_throughput_bps.unwrap_or(-1),
                        upload_throughput_bps: custom.upload_throughput_bps.unwrap_or(-1),
                        connection_type: custom.connection_type.clone(),
                    }
                } else {
                    anyhow::bail!(
                        "set_throttle requires either a preset name or custom conditions"
                    );
                };
                authorize_network_operation(
                    &event_stream,
                    "Set network throttle",
                    permission_inputs(&operation, [format!("session_id:{session_id}")]),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .set_throttle(session_id, input.target_id.as_deref(), conditions)
                    .await?;
                Ok(success(operation, "set network throttle", data))
            }
            NetworkOperation::SetOffline => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let offline = input
                    .offline
                    .context("offline is required for network set_offline")?;
                authorize_network_operation(
                    &event_stream,
                    "Set network offline",
                    permission_inputs(
                        &operation,
                        [format!("session_id:{session_id} offline:{offline}")],
                    ),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .set_offline(session_id, input.target_id.as_deref(), offline)
                    .await?;
                Ok(success(operation, "set offline state", data))
            }
            NetworkOperation::BlockUrls => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let urls = input
                    .urls
                    .clone()
                    .context("urls is required for network block_urls")?;
                authorize_network_operation(
                    &event_stream,
                    "Block network URLs",
                    permission_inputs(
                        &operation,
                        urls.iter()
                            .map(|url| format!("session_id:{session_id} url:{url}")),
                    ),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .block_urls(session_id, input.target_id.as_deref(), &urls)
                    .await?;
                Ok(success(operation, "blocked URLs", data))
            }
            NetworkOperation::UnblockUrls => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let urls = input
                    .urls
                    .clone()
                    .context("urls is required for network unblock_urls")?;
                authorize_network_operation(
                    &event_stream,
                    "Unblock network URLs",
                    permission_inputs(
                        &operation,
                        urls.iter()
                            .map(|url| format!("session_id:{session_id} url:{url}")),
                    ),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .unblock_urls(session_id, input.target_id.as_deref(), &urls)
                    .await?;
                Ok(success(operation, "unblocked URLs", data))
            }
            NetworkOperation::SetCacheDisabled => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let disabled = input
                    .disabled
                    .context("disabled is required for network set_cache_disabled")?;
                authorize_network_operation(
                    &event_stream,
                    "Toggle browser cache",
                    permission_inputs(
                        &operation,
                        [format!("session_id:{session_id} disabled:{disabled}")],
                    ),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .set_cache_disabled(session_id, input.target_id.as_deref(), disabled)
                    .await?;
                Ok(success(operation, "toggled cache", data))
            }
            NetworkOperation::SetBypassServiceWorker => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let bypass = input
                    .disabled
                    .context("disabled is required for network set_bypass_service_worker")?;
                authorize_network_operation(
                    &event_stream,
                    "Toggle service worker bypass",
                    permission_inputs(
                        &operation,
                        [format!("session_id:{session_id} bypass:{bypass}")],
                    ),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .set_bypass_service_worker(session_id, input.target_id.as_deref(), bypass)
                    .await?;
                Ok(success(operation, "toggled service worker bypass", data))
            }
            NetworkOperation::ClearBrowserCache => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                authorize_network_operation(
                    &event_stream,
                    "Clear browser cache",
                    permission_inputs(&operation, [format!("session_id:{session_id}")]),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .clear_browser_cache(session_id, input.target_id.as_deref())
                    .await?;
                Ok(success(operation, "cleared browser cache", data))
            }
            NetworkOperation::ClearBrowserCookies => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                authorize_network_operation(
                    &event_stream,
                    "Clear browser cookies",
                    permission_inputs(&operation, [format!("session_id:{session_id}")]),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .clear_browser_cookies(session_id, input.target_id.as_deref())
                    .await?;
                Ok(success(operation, "cleared browser cookies", data))
            }
            NetworkOperation::SetCookie => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let cookie = input
                    .cookie
                    .clone()
                    .context("cookie is required for network set_cookie")?;
                authorize_network_operation(
                    &event_stream,
                    "Set browser cookie",
                    permission_inputs(&operation, [format!("session_id:{session_id}")]),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .set_cookie(session_id, input.target_id.as_deref(), cookie)
                    .await?;
                Ok(success(operation, "set cookie", data))
            }
            NetworkOperation::DeleteCookies => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let params = input
                    .cookie
                    .clone()
                    .context("cookie is required for network delete_cookies")?;
                authorize_network_operation(
                    &event_stream,
                    "Delete browser cookies",
                    permission_inputs(&operation, [format!("session_id:{session_id}")]),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .delete_cookies(session_id, input.target_id.as_deref(), params)
                    .await?;
                Ok(success(operation, "deleted cookies", data))
            }
            NetworkOperation::SetExtraHttpHeaders => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let headers = input
                    .headers
                    .clone()
                    .context("headers is required for network set_extra_http_headers")?;
                authorize_network_operation(
                    &event_stream,
                    "Override HTTP headers",
                    permission_inputs(&operation, [format!("session_id:{session_id}")]),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .set_extra_http_headers(session_id, input.target_id.as_deref(), &headers)
                    .await?;
                Ok(success(operation, "set extra HTTP headers", data))
            }
            NetworkOperation::SetUserAgent => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let user_agent = input
                    .user_agent
                    .clone()
                    .context("user_agent is required for network set_user_agent")?;
                authorize_network_operation(
                    &event_stream,
                    "Override user agent",
                    permission_inputs(&operation, [format!("session_id:{session_id}")]),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .set_user_agent(session_id, input.target_id.as_deref(), &user_agent)
                    .await?;
                Ok(success(operation, "set user agent", data))
            }
            NetworkOperation::SetInterception => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let patterns = input
                    .patterns
                    .clone()
                    .context("patterns is required for network set_interception")?;
                let patterns = patterns
                    .into_iter()
                    .map(|pattern| InterceptionPattern {
                        url_pattern: pattern.url_pattern,
                        request_stage: pattern.request_stage,
                    })
                    .collect::<Vec<_>>();
                authorize_network_operation(
                    &event_stream,
                    "Enable network interception",
                    permission_inputs(
                        &operation,
                        std::iter::once(format!("session_id:{session_id}")).chain(
                            patterns
                                .iter()
                                .map(|pattern| format!("url:{}", pattern.url_pattern)),
                        ),
                    ),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .set_interception(session_id, input.target_id.as_deref(), &patterns)
                    .await?;
                Ok(success(operation, "enabled network interception", data))
            }
            NetworkOperation::HarImport => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let har = input
                    .har
                    .clone()
                    .context("har is required for network har_import")?;
                authorize_network_operation(
                    &event_stream,
                    "Import HAR capture",
                    permission_inputs(&operation, [format!("session_id:{session_id}")]),
                    cx,
                )
                .await?;
                let data = self.api.import_har(session_id, &har).await?;
                Ok(success(operation, "imported HAR", data))
            }
            NetworkOperation::ContinueRequest => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let request_id = input
                    .request_id
                    .as_deref()
                    .context("request_id is required for network continue_request")?;
                let modifications = input.modifications.clone().unwrap_or_default();
                let data = self
                    .api
                    .continue_request(
                        session_id,
                        input.target_id.as_deref(),
                        request_id,
                        modifications,
                    )
                    .await?;
                Ok(success(operation, "continued request", data))
            }
            NetworkOperation::FulfillRequest => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let request_id = input
                    .request_id
                    .as_deref()
                    .context("request_id is required for network fulfill_request")?;
                let response = input
                    .response
                    .clone()
                    .context("response is required for network fulfill_request")?;
                let url = self
                    .api
                    .get_request(session_id, request_id)
                    .await
                    .ok()
                    .and_then(|request| {
                        request
                            .get("url")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                let mut values = vec![format!("request_id:{request_id}")];
                if let Some(url) = &url {
                    values.push(format!("url:{url}"));
                }
                authorize_network_operation(
                    &event_stream,
                    "Fulfill network request",
                    permission_inputs(&operation, values),
                    cx,
                )
                .await?;
                let data = self
                    .api
                    .fulfill_request(session_id, input.target_id.as_deref(), request_id, response)
                    .await?;
                Ok(success(operation, "fulfilled request", data))
            }
            NetworkOperation::FailRequest => {
                self.ensure_write_mode(&operation, cx)?;
                let session_id = required_session(&input)?;
                let request_id = input
                    .request_id
                    .as_deref()
                    .context("request_id is required for network fail_request")?;
                let error_reason = input
                    .error_reason
                    .as_deref()
                    .context("error_reason is required for network fail_request")?;
                let data = self
                    .api
                    .fail_request(
                        session_id,
                        input.target_id.as_deref(),
                        request_id,
                        error_reason,
                    )
                    .await?;
                Ok(success(operation, "failed request", data))
            }
        }
    }
}

impl AgentTool for NetworkTool {
    type Input = NetworkToolInput;
    type Output = NetworkToolOutput;

    const NAME: &'static str = "network";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Network: {}", operation_name(&input)).into(),
            Err(value) => value
                .get("operation")
                .and_then(|value| value.as_str())
                .map(|operation| format!("Network: {operation}").into())
                .unwrap_or_else(|| "Network".into()),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| NetworkToolOutput::Error {
                    operation: None,
                    error: format!("Failed to receive network tool input: {error}"),
                })?;
            let operation = operation_name(&input).to_string();
            let session_id = input.session_id;
            let api = self.api.clone();
            match self
                .run_operation(input, operation.clone(), event_stream, cx)
                .await
            {
                Ok(output) => {
                    if let Some(session_id) = session_id {
                        let _ = api.set_driven_by(session_id, DrivenBy::Agent).await;
                    }
                    Ok(output)
                }
                Err(error) => Err(NetworkToolOutput::Error {
                    operation: Some(operation),
                    error: error.to_string(),
                }),
            }
        })
    }
}

async fn authorize_network_operation(
    event_stream: &ToolCallEventStream,
    title: impl Into<String>,
    input_values: Vec<String>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let title = title.into();
    let task = cx.update(|cx| {
        event_stream.authorize(
            title,
            ToolPermissionContext::new(NetworkTool::NAME, input_values),
            cx,
        )
    });
    task.await
}

fn required_session(input: &NetworkToolInput) -> Result<u64> {
    input
        .session_id
        .context("session_id is required for this network operation")
}

fn validate_limit(limit: Option<usize>) -> Result<usize> {
    let limit = limit.unwrap_or(DEFAULT_REQUESTS_PER_PAGE);
    if limit > MAX_REQUESTS_PER_PAGE {
        anyhow::bail!(
            "limit {limit} exceeds the maximum of {MAX_REQUESTS_PER_PAGE} requests per page"
        );
    }
    Ok(limit)
}

fn validate_body_limit(max_body_bytes: Option<usize>) -> Result<usize> {
    let max_body_bytes = max_body_bytes.unwrap_or(DEFAULT_BODY_BYTES);
    if max_body_bytes > MAX_BODY_BYTES {
        anyhow::bail!(
            "max_body_bytes {max_body_bytes} exceeds the maximum of {MAX_BODY_BYTES} bytes (1 MiB)"
        );
    }
    Ok(max_body_bytes)
}

fn success(operation: String, message: impl Into<String>, data: Value) -> NetworkToolOutput {
    NetworkToolOutput::Success {
        operation,
        message: message.into(),
        data,
    }
}

fn operation_name(input: &NetworkToolInput) -> &'static str {
    match input.operation {
        NetworkOperation::ListRequests => "list_requests",
        NetworkOperation::GetRequest => "get_request",
        NetworkOperation::GetResponseBody => "get_response_body",
        NetworkOperation::GetRequestPostData => "get_request_post_data",
        NetworkOperation::GetCookies => "get_cookies",
        NetworkOperation::GetWebSocketFrames => "get_websocket_frames",
        NetworkOperation::WaitForNetworkIdle => "wait_for_network_idle",
        NetworkOperation::HarExport => "har_export",
        NetworkOperation::HarImport => "har_import",
        NetworkOperation::ListPausedRequests => "list_paused_requests",
        NetworkOperation::GetControlState => "get_control_state",
        NetworkOperation::SetThrottle => "set_throttle",
        NetworkOperation::SetOffline => "set_offline",
        NetworkOperation::BlockUrls => "block_urls",
        NetworkOperation::UnblockUrls => "unblock_urls",
        NetworkOperation::SetCacheDisabled => "set_cache_disabled",
        NetworkOperation::SetBypassServiceWorker => "set_bypass_service_worker",
        NetworkOperation::ClearBrowserCache => "clear_browser_cache",
        NetworkOperation::ClearBrowserCookies => "clear_browser_cookies",
        NetworkOperation::SetCookie => "set_cookie",
        NetworkOperation::DeleteCookies => "delete_cookies",
        NetworkOperation::SetExtraHttpHeaders => "set_extra_http_headers",
        NetworkOperation::SetUserAgent => "set_user_agent",
        NetworkOperation::SetInterception => "set_interception",
        NetworkOperation::ContinueRequest => "continue_request",
        NetworkOperation::FulfillRequest => "fulfill_request",
        NetworkOperation::FailRequest => "fail_request",
    }
}
