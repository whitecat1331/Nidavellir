use crate::cdp::CdpClient;
use crate::driven_by::DrivenBy;
use crate::network::{
    InterceptionConfig, InterceptionPattern, NetworkControlState, NetworkRequest, NetworkStore,
    RequestFilter, ThrottleConditions,
};
use crate::session::{BrowserSession, BrowserTarget};
use anyhow::{Context, Result, anyhow};
use futures::{AsyncBufReadExt, StreamExt};
use gpui::BackgroundExecutor;
use http_client::HttpClient;
use serde_json::{Value, json};
use smol::process::Command;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The project-side browser automation surface, mirroring `AgentDebuggerApi`.
///
/// It is cheap to clone (shared session store) so an agent tool can hold one
/// instance across turns without recreating browser sessions.
#[derive(Clone)]
pub struct AgentBrowserApi {
    sessions: Arc<smol::lock::Mutex<HashMap<u64, BrowserSession>>>,
    next_session_id: Arc<AtomicU64>,
    chromium_path: Option<PathBuf>,
    http_client: Arc<dyn HttpClient>,
    background_executor: BackgroundExecutor,
}

impl AgentBrowserApi {
    pub fn new(
        chromium_path: Option<PathBuf>,
        http_client: Arc<dyn HttpClient>,
        background_executor: BackgroundExecutor,
    ) -> Self {
        Self {
            sessions: Arc::new(smol::lock::Mutex::new(HashMap::new())),
            next_session_id: Arc::new(AtomicU64::new(0)),
            chromium_path,
            http_client,
            background_executor,
        }
    }

    pub async fn list_sessions(&self) -> Vec<Value> {
        let sessions = self.sessions.lock().await;
        sessions
            .values()
            .map(|session| {
                json!({
                    "session_id": session.id,
                    "active_target_id": session.active_target_id,
                    "driven_by": session.driven_by.as_str(),
                    "targets": session.targets.values().map(target_to_json).collect::<Vec<_>>(),
                })
            })
            .collect()
    }

    pub async fn start_session(&self, url: &str, headless: bool) -> Result<u64> {
        let chromium = crate::chromium::resolve_chromium_binary(
            &self.http_client,
            self.chromium_path.as_deref(),
        )
        .await?;
        let profile_dir = std::env::temp_dir().join(format!("zed-browser-{}", std::process::id()));

        let mut command = Command::new(&chromium);
        command
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-extensions");
        if headless {
            command.arg("--headless=new");
        }
        let mut child = command
            .arg("about:blank")
            .stdout(smol::process::Stdio::null())
            .stderr(smol::process::Stdio::piped())
            .spawn()
            .context("failed to launch Chromium")?;

        let stderr = child.stderr.take().context("Chromium has no stderr")?;
        let endpoint = smol::future::race(read_devtools_endpoint(stderr), async {
            self.background_executor
                .timer(Duration::from_secs(15))
                .await;
            Err(anyhow!("timed out waiting for Chromium DevTools endpoint"))
        })
        .await?;

        let client = CdpClient::connect(&endpoint, self.background_executor.clone()).await?;
        let target = create_target(&client, url, &self.background_executor).await?;

        let id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
        let mut targets = HashMap::new();
        let active_target_id = target.target_id.clone();
        targets.insert(target.target_id.clone(), target);
        self.sessions.lock().await.insert(
            id,
            BrowserSession {
                id,
                client,
                targets,
                active_target_id: Some(active_target_id),
                child,
                network_store: NetworkStore::new(),
                control: NetworkControlState::default(),
                driven_by: DrivenBy::default(),
            },
        );
        Ok(id)
    }

    pub async fn open_target(&self, session_id: u64, url: &str) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target = create_target(&session.client, url, &self.background_executor).await?;
        let target_json = target_to_json(&target);
        let target_id = target.target_id.clone();
        session.active_target_id = Some(target_id.clone());
        session.targets.insert(target_id, target);
        Ok(target_json)
    }

    pub async fn close_target(&self, session_id: u64, target_id: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target = session
            .targets
            .remove(target_id)
            .context("unknown browser target")?;
        session
            .client
            .send_command(
                "Target.closeTarget",
                json!({ "targetId": target.target_id }),
            )
            .await?;
        if session.active_target_id.as_deref() == Some(target_id) {
            session.active_target_id = session.targets.keys().next().cloned();
        }
        Ok(())
    }

    pub async fn activate_target(&self, session_id: u64, target_id: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        if !session.targets.contains_key(target_id) {
            anyhow::bail!("unknown browser target");
        }
        session.active_target_id = Some(target_id.to_string());
        Ok(())
    }

    pub async fn navigate(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        url: &str,
    ) -> Result<()> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Page.navigate",
                json!({ "url": url }),
            )
            .await?;
        update_target_url(session, target_id, url);
        Ok(())
    }

    pub async fn snapshot(&self, session_id: u64, target_id: Option<&str>) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let page = evaluate_value(
            &session.client,
            Some(&target_session_id),
            SNAPSHOT_EXPRESSION,
        )
        .await?;
        let target = get_target(session, target_id)?;

        Ok(json!({
            "target_id": target.target_id,
            "url": page
                .get("url")
                .cloned()
                .unwrap_or_else(|| Value::String(target.url.clone())),
            "title": page.get("title").cloned().unwrap_or(Value::Null),
            "text": page.get("text").cloned().unwrap_or(Value::Null),
            "elements": page
                .get("elements")
                .cloned()
                .unwrap_or_else(|| Value::Array(vec![])),
        }))
    }

    pub async fn screenshot(&self, session_id: u64, target_id: Option<&str>) -> Result<String> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Page.captureScreenshot",
                json!({ "format": "png", "fromSurface": true }),
            )
            .await?;
        let data = result
            .get("data")
            .and_then(Value::as_str)
            .context("Page.captureScreenshot returned no image data")?;
        Ok(data.to_string())
    }

    pub async fn evaluate(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        expression: &str,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        let evaluated = result.get("result").cloned().unwrap_or(Value::Null);
        let exception = result
            .get("exceptionDetails")
            .and_then(|details| details.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string);

        Ok(json!({
            "value": evaluated.get("value").cloned().unwrap_or(Value::Null),
            "type": evaluated.get("type").cloned().unwrap_or(Value::Null),
            "description": evaluated.get("description").cloned().unwrap_or(Value::Null),
            "exception": exception,
        }))
    }

    pub async fn click(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        selector: &str,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let expression = format!(
            "(() => {{ const el = document.querySelector({selector}); if (!el) return {{ clicked: false, reason: 'no element matches selector' }}; el.click(); return {{ clicked: true, tag: el.tagName.toLowerCase(), text: (el.innerText || el.value || '').toString().slice(0, 500) }}; }})()",
            selector = serde_json::to_string(selector)?,
        );
        let value = evaluate_value(&session.client, Some(&target_session_id), &expression).await?;
        Ok(json!({ "clicked": value }))
    }

    pub async fn type_text(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        selector: &str,
        text: &str,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;

        let focus = format!(
            "(() => {{ const el = document.querySelector({selector}); if (!el) return {{ focused: false, reason: 'no element matches selector' }}; el.focus(); return {{ focused: true, tag: el.tagName.toLowerCase() }}; }})()",
            selector = serde_json::to_string(selector)?,
        );
        let focused = evaluate_value(&session.client, Some(&target_session_id), &focus).await?;
        if focused.get("focused").and_then(Value::as_bool) != Some(true) {
            return Ok(json!({
                "typed": false,
                "reason": focused
                    .get("reason")
                    .cloned()
                    .unwrap_or_else(|| Value::String("no element matches selector".into())),
            }));
        }

        session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Input.insertText",
                json!({ "text": text }),
            )
            .await?;
        Ok(json!({ "typed": true, "selector": selector, "text": text }))
    }

    pub async fn read_console(&self, session_id: u64, target_id: Option<&str>) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let events = session.client.recent_events();
        Ok(json!({
            "console": filter_events(&events, is_console_event, Some(&target_session_id), 100),
        }))
    }

    pub async fn read_network(&self, session_id: u64, target_id: Option<&str>) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        sync_network_store(session);
        let events = session.client.recent_events();
        Ok(json!({
            "network": filter_events(&events, is_network_event, Some(&target_session_id), 100),
        }))
    }

    /// List requests from the typed store, filtered and paginated.
    pub async fn list_requests(
        &self,
        session_id: u64,
        filter: &RequestFilter,
        offset: usize,
        limit: usize,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        sync_network_store(session);
        let total = session
            .network_store
            .requests()
            .filter(|request| filter.matches(request))
            .count();
        let requests = session
            .network_store
            .requests()
            .filter(|request| filter.matches(request))
            .skip(offset)
            .take(limit)
            .map(NetworkRequest::to_json)
            .collect::<Vec<_>>();
        let next_offset = if offset + limit < total {
            Some(offset + limit)
        } else {
            None
        };
        Ok(json!({
            "requests": requests,
            "total": total,
            "offset": offset,
            "next_offset": next_offset,
        }))
    }

    /// Fetch a single request by CDP `request_id`.
    pub async fn get_request(&self, session_id: u64, request_id: &str) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        sync_network_store(session);
        let request = session
            .network_store
            .request(request_id)
            .context("unknown request_id")?;
        Ok(request.to_json())
    }

    /// Fetch a response body slice. Text bodies are sliced by byte offset;
    /// binary bodies (images, fonts, media) are returned as metadata only.
    pub async fn get_response_body(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        request_id: &str,
        offset: usize,
        max_length: usize,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.getResponseBody",
                json!({ "requestId": request_id }),
            )
            .await?;
        let base64_encoded = result
            .get("base64Encoded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mime_type = session
            .network_store
            .request(request_id)
            .and_then(|request| request.mime_type.clone());
        if base64_encoded {
            let body_length = result
                .get("body")
                .and_then(Value::as_str)
                .map(str::len)
                .unwrap_or(0);
            let size = session
                .network_store
                .request(request_id)
                .and_then(|request| request.encoded_data_length)
                .unwrap_or(body_length as f64);
            return Ok(json!({
                "request_id": request_id,
                "binary": true,
                "mime_type": mime_type,
                "size": size,
            }));
        }
        let body = result.get("body").and_then(Value::as_str).unwrap_or("");
        let bytes = body.as_bytes();
        let start = offset.min(bytes.len());
        let end = offset.saturating_add(max_length).min(bytes.len());
        let slice = &bytes[start..end];
        let truncated = end < bytes.len();
        Ok(json!({
            "request_id": request_id,
            "binary": false,
            "mime_type": mime_type,
            "body_length": bytes.len(),
            "body": String::from_utf8_lossy(slice),
            "offset": offset,
            "truncated": truncated,
            "next_offset": if truncated { Some(end) } else { None },
        }))
    }

    /// Fetch a request's POST data, preferring the captured store and falling
    /// back to a live `Network.getRequestPostData` call.
    pub async fn get_request_post_data(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        request_id: &str,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        sync_network_store(session);
        if let Some(post_data) = session
            .network_store
            .request(request_id)
            .and_then(|request| request.post_data.clone())
        {
            return Ok(json!({ "request_id": request_id, "post_data": post_data }));
        }
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.getRequestPostData",
                json!({ "requestId": request_id }),
            )
            .await?;
        Ok(json!({ "request_id": request_id, "post_data": result.get("postData") }))
    }

    /// Fetch the browser's cookies, optionally scoped to a URL list.
    pub async fn get_cookies(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        urls: &[String],
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let mut params = json!({});
        if !urls.is_empty() {
            params["urls"] = json!(urls);
        }
        session
            .client
            .send_command_with_session(Some(&target_session_id), "Network.getCookies", params)
            .await
    }

    /// Fetch WebSocket frames for a connection, keyed by its `request_id`.
    pub async fn get_websocket_frames(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        request_id: &str,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.getWebSocketFrames",
                json!({ "requestId": request_id }),
            )
            .await
    }

    /// Wait until no requests have been in flight for `idle_for_ms`, bounded by
    /// `timeout_ms`. Returns whether the session reached network idle.
    pub async fn wait_for_network_idle(
        &self,
        session_id: u64,
        idle_for_ms: u64,
        timeout_ms: u64,
    ) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut idle_since: Option<Instant> = None;
        loop {
            let pending = {
                let mut sessions = self.sessions.lock().await;
                let session = sessions
                    .get_mut(&session_id)
                    .context("unknown browser session")?;
                sync_network_store(session);
                session
                    .network_store
                    .requests()
                    .filter(|request| !request.completed)
                    .map(|request| request.request_id.clone())
                    .collect::<Vec<_>>()
            };
            if pending.is_empty() {
                let now = Instant::now();
                let since = *idle_since.get_or_insert(now);
                if now.duration_since(since) >= Duration::from_millis(idle_for_ms) {
                    return Ok(json!({ "idle": true, "pending": [] }));
                }
            } else {
                idle_since = None;
            }
            if Instant::now() >= deadline {
                return Ok(json!({ "idle": false, "pending": pending }));
            }
            self.background_executor
                .timer(Duration::from_millis(100))
                .await;
        }
    }

    /// Export the session's typed store as a HAR object.
    pub async fn export_har(&self, session_id: u64) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        sync_network_store(session);
        Ok(crate::har::export_har(&session.network_store))
    }

    /// Replace the session's typed store with a parsed HAR object.
    pub async fn import_har(&self, session_id: u64, har: &Value) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        session.network_store = crate::har::import_har(har)?;
        Ok(json!({ "imported": session.network_store.len() }))
    }

    pub async fn set_throttle(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        conditions: ThrottleConditions,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.emulateNetworkConditions",
                conditions.to_cdp_params(),
            )
            .await?;
        session.control.offline = conditions.offline;
        session.control.throttle = Some(conditions);
        Ok(result)
    }

    pub async fn set_offline(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        offline: bool,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let mut conditions = session.control.throttle.clone().unwrap_or_default();
        conditions.offline = offline;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.emulateNetworkConditions",
                conditions.to_cdp_params(),
            )
            .await?;
        session.control.offline = offline;
        session.control.throttle = Some(conditions);
        Ok(result)
    }

    pub async fn set_cache_disabled(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        disabled: bool,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.setCacheDisabled",
                json!({ "cacheDisabled": disabled }),
            )
            .await?;
        session.control.cache_disabled = disabled;
        Ok(result)
    }

    pub async fn set_bypass_service_worker(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        bypass: bool,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.setBypassServiceWorker",
                json!({ "bypass": bypass }),
            )
            .await?;
        session.control.bypass_service_worker = bypass;
        Ok(result)
    }

    pub async fn set_blocked_urls(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        urls: &[String],
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.setBlockedURLs",
                json!({ "urls": urls }),
            )
            .await?;
        session.control.blocked_urls = urls.to_vec();
        Ok(result)
    }

    /// Add URLs to the blocked list (deduplicating against the current list).
    pub async fn block_urls(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        urls: &[String],
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let mut merged = session.control.blocked_urls.clone();
        for url in urls {
            if !merged.iter().any(|existing| existing == url) {
                merged.push(url.clone());
            }
        }
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.setBlockedURLs",
                json!({ "urls": &merged }),
            )
            .await?;
        session.control.blocked_urls = merged;
        Ok(result)
    }

    /// Remove URLs from the blocked list.
    pub async fn unblock_urls(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        urls: &[String],
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let merged = session
            .control
            .blocked_urls
            .iter()
            .filter(|existing| !urls.contains(existing))
            .cloned()
            .collect::<Vec<_>>();
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.setBlockedURLs",
                json!({ "urls": &merged }),
            )
            .await?;
        session.control.blocked_urls = merged;
        Ok(result)
    }

    pub async fn clear_browser_cache(
        &self,
        session_id: u64,
        target_id: Option<&str>,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.clearBrowserCache",
                json!({}),
            )
            .await
    }

    pub async fn clear_browser_cookies(
        &self,
        session_id: u64,
        target_id: Option<&str>,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.clearBrowserCookies",
                json!({}),
            )
            .await
    }

    pub async fn set_cookie(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        params: Value,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        session
            .client
            .send_command_with_session(Some(&target_session_id), "Network.setCookie", params)
            .await
    }

    pub async fn delete_cookies(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        params: Value,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        session
            .client
            .send_command_with_session(Some(&target_session_id), "Network.deleteCookies", params)
            .await
    }

    pub async fn set_extra_http_headers(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        headers: &HashMap<String, String>,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.setExtraHTTPHeaders",
                json!({ "headers": headers }),
            )
            .await?;
        session.control.extra_http_headers = headers.clone();
        Ok(result)
    }

    pub async fn set_user_agent(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        user_agent: &str,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Network.setUserAgentOverride",
                json!({ "userAgent": user_agent }),
            )
            .await?;
        session.control.user_agent = Some(user_agent.to_string());
        Ok(result)
    }

    pub async fn set_interception(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        patterns: &[InterceptionPattern],
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let config = InterceptionConfig {
            patterns: patterns.to_vec(),
        };
        let result = session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Fetch.enable",
                json!({
                    "patterns": config
                        .patterns
                        .iter()
                        .map(InterceptionPattern::to_cdp_params)
                        .collect::<Vec<_>>(),
                }),
            )
            .await?;
        session.control.interception = Some(config);
        Ok(result)
    }

    pub async fn continue_request(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        request_id: &str,
        modifications: Value,
    ) -> Result<Value> {
        self.fetch_disposition(
            session_id,
            target_id,
            "Fetch.continueRequest",
            request_id,
            modifications,
        )
        .await
    }

    pub async fn fulfill_request(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        request_id: &str,
        response: Value,
    ) -> Result<Value> {
        self.fetch_disposition(
            session_id,
            target_id,
            "Fetch.fulfillRequest",
            request_id,
            response,
        )
        .await
    }

    pub async fn fail_request(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        request_id: &str,
        error_reason: &str,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        session
            .client
            .send_command_with_session(
                Some(&target_session_id),
                "Fetch.failRequest",
                json!({ "requestId": request_id, "errorReason": error_reason }),
            )
            .await
    }

    pub async fn list_paused_requests(
        &self,
        session_id: u64,
        target_id: Option<&str>,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let events = session.client.recent_events();
        let paused = filter_events(
            &events,
            is_fetch_request_paused,
            Some(&target_session_id),
            100,
        );
        Ok(json!({ "paused_requests": paused }))
    }

    pub async fn network_control_state(&self, session_id: u64) -> Result<Value> {
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(&session_id)
            .context("unknown browser session")?;
        Ok(control_state_to_json(&session.control))
    }

    /// Mark who is driving the session, so the GUI and AIUI can hand off
    /// without stepping on each other.
    pub async fn set_driven_by(&self, session_id: u64, driven_by: DrivenBy) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        session.driven_by = driven_by;
        Ok(json!({ "driven_by": session.driven_by.as_str() }))
    }

    /// Clear the captured requests in a session's typed store.
    pub async fn clear_requests(&self, session_id: u64) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        session.network_store = NetworkStore::new();
        session.client.clear_events();
        Ok(json!({ "cleared": true }))
    }

    async fn fetch_disposition(
        &self,
        session_id: u64,
        target_id: Option<&str>,
        method: &str,
        request_id: &str,
        fields: Value,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .context("unknown browser session")?;
        let target_session_id = resolve_target_session_id(session, target_id)?;
        let mut params = json!({ "requestId": request_id });
        if let Some(object) = fields.as_object() {
            for (key, value) in object {
                params[key] = value.clone();
            }
        }
        session
            .client
            .send_command_with_session(Some(&target_session_id), method, params)
            .await
    }

    pub async fn stop_session(&self, session_id: u64, keep_open: bool) -> Result<()> {
        let session = self
            .sessions
            .lock()
            .await
            .remove(&session_id)
            .context("unknown browser session")?;

        let client = session.client;
        let mut child = session.child;

        if keep_open {
            // Close every target but leave the browser process running.
            for target in session.targets.values() {
                client
                    .send_command(
                        "Target.closeTarget",
                        json!({ "targetId": target.target_id }),
                    )
                    .await?;
            }
        } else {
            // Default teardown: terminate the whole Chromium process.
            child.kill().context("failed to close Chromium")?;
        }
        Ok(())
    }
}

fn target_to_json(target: &BrowserTarget) -> Value {
    json!({
        "target_id": target.target_id,
        "target_type": target.target_type,
        "url": target.url,
    })
}

/// Drain every buffered `Network.*` event into the typed store so store-backed
/// reads (`list_requests`, `get_request`, `export_har`, `wait_for_network_idle`)
/// see the full capture, not just the bounded slice returned to the caller.
fn sync_network_store(session: &mut BrowserSession) {
    let events = session.client.recent_events();
    for event in events.iter().filter(|event| is_network_event(event)) {
        session.network_store.ingest(event);
    }
}

fn resolve_target_session_id(session: &BrowserSession, target_id: Option<&str>) -> Result<String> {
    let resolved = match target_id {
        Some(target_id) => target_id.to_string(),
        None => session
            .active_target_id
            .clone()
            .context("no active browser target")?,
    };
    session
        .targets
        .get(&resolved)
        .map(|target| target.session_id.clone())
        .context("unknown browser target")
}

fn get_target<'a>(
    session: &'a BrowserSession,
    target_id: Option<&str>,
) -> Result<&'a BrowserTarget> {
    let resolved = match target_id {
        Some(target_id) => target_id.to_string(),
        None => session
            .active_target_id
            .clone()
            .context("no active browser target")?,
    };
    session
        .targets
        .get(&resolved)
        .context("unknown browser target")
}

fn update_target_url(session: &mut BrowserSession, target_id: Option<&str>, url: &str) {
    let resolved = target_id
        .map(str::to_string)
        .or_else(|| session.active_target_id.clone());
    if let Some(resolved) = resolved {
        if let Some(target) = session.targets.get_mut(&resolved) {
            target.url = url.to_string();
        }
    }
}

async fn create_target(
    client: &CdpClient,
    url: &str,
    executor: &BackgroundExecutor,
) -> Result<BrowserTarget> {
    let create_result = client
        .send_command("Target.createTarget", json!({ "url": "about:blank" }))
        .await?;
    let target_id = create_result
        .get("targetId")
        .and_then(Value::as_str)
        .context("Target.createTarget returned no targetId")?
        .to_string();
    let target_type = create_result
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("page")
        .to_string();

    let attach_result = client
        .send_command(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await?;
    let session_id = attach_result
        .get("sessionId")
        .and_then(Value::as_str)
        .context("Target.attachToTarget returned no sessionId")?
        .to_string();

    client
        .send_command_with_session(Some(&session_id), "Page.enable", json!({}))
        .await?;
    client
        .send_command_with_session(Some(&session_id), "Runtime.enable", json!({}))
        .await?;
    client
        .send_command_with_session(Some(&session_id), "Log.enable", json!({}))
        .await?;
    client
        .send_command_with_session(
            Some(&session_id),
            "Network.enable",
            json!({
                "maxTotalBufferSize": 50_000_000,
                "maxResourceBufferSize": 10_000_000,
                "maxPostDataSize": 1_000_000,
            }),
        )
        .await?;
    client
        .send_command_with_session(Some(&session_id), "Page.navigate", json!({ "url": url }))
        .await?;
    wait_for_page_load(client, Some(&session_id), url, executor).await?;

    Ok(BrowserTarget {
        target_id,
        session_id,
        target_type,
        url: url.to_string(),
    })
}

async fn read_devtools_endpoint(stderr: smol::process::ChildStderr) -> Result<String> {
    let mut lines = futures::io::BufReader::new(stderr).lines();
    while let Some(line) = lines.next().await {
        let line = line.context("failed to read Chromium stderr")?;
        if let Some(endpoint) = parse_devtools_endpoint(&line) {
            return Ok(endpoint);
        }
    }
    Err(anyhow!(
        "Chromium exited before exposing a DevTools endpoint"
    ))
}

fn parse_devtools_endpoint(line: &str) -> Option<String> {
    let marker = "DevTools listening on ";
    let start = line.find(marker)? + marker.len();
    Some(line[start..].trim().to_string())
}

const SNAPSHOT_EXPRESSION: &str = r#"(function() {
  var body = document.body ? document.body.innerText.slice(0, 10000) : '';
  var elements = [];
  var all = document.querySelectorAll('a, button, input, textarea, select, [role="button"], [role="link"], [role="textbox"], summary');
  var seen = 0;
  for (var i = 0; i < all.length; i++) {
    if (seen >= 100) break;
    var el = all[i];
    var rect = el.getBoundingClientRect();
    if (rect.width === 0 && rect.height === 0) continue;
    var label = (el.getAttribute('aria-label') || el.innerText || el.value || el.getAttribute('placeholder') || el.getAttribute('name') || '').toString().trim().slice(0, 200);
    elements.push({
      tag: el.tagName.toLowerCase(),
      id: el.id || null,
      type: el.getAttribute('type') || null,
      label: label,
      selector: cssPath(el),
    });
    seen += 1;
  }
  return { url: location.href, title: document.title, text: body, elements: elements };

  function cssPath(el) {
    if (el.id) return '#' + CSS.escape(el.id);
    var parts = [];
    var node = el;
    while (node && node.nodeType === 1 && node !== document.body) {
      var name = node.tagName.toLowerCase();
      var nth = 1;
      var sibling = node;
      while ((sibling = sibling.previousElementSibling)) {
        if (sibling.tagName.toLowerCase() === name) nth += 1;
      }
      parts.unshift(name + ':nth-of-type(' + nth + ')');
      node = node.parentElement;
    }
    return parts.join(' > ');
  }
})()"#;

async fn evaluate_value(
    client: &CdpClient,
    session_id: Option<&str>,
    expression: &str,
) -> Result<Value> {
    let result = client
        .send_command_with_session(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )
        .await?;
    Ok(result
        .get("result")
        .and_then(|value| value.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

async fn wait_for_page_load(
    client: &CdpClient,
    session_id: Option<&str>,
    url: &str,
    executor: &BackgroundExecutor,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let state = evaluate_value(
            client,
            session_id,
            "(function() { return { href: location.href, ready: document.readyState }; })()",
        )
        .await;
        let loaded = state
            .as_ref()
            .map(|value| {
                value
                    .get("href")
                    .and_then(Value::as_str)
                    .map(|href| href != "about:blank")
                    .unwrap_or(false)
                    && value.get("ready").and_then(Value::as_str) == Some("complete")
            })
            .unwrap_or(false);
        if loaded {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!("timed out waiting for {url} to load"));
        }
        executor.timer(Duration::from_millis(100)).await;
    }
}

fn is_console_event(event: &Value) -> bool {
    event
        .get("method")
        .and_then(Value::as_str)
        .map(|method| method == "Runtime.consoleAPICalled")
        .unwrap_or(false)
}

fn is_network_event(event: &Value) -> bool {
    event
        .get("method")
        .and_then(Value::as_str)
        .map(|method| method.starts_with("Network."))
        .unwrap_or(false)
}

fn filter_events(
    events: &[Value],
    predicate: fn(&Value) -> bool,
    session_id: Option<&str>,
    max: usize,
) -> Vec<Value> {
    let mut out = Vec::new();
    for event in events.iter().rev() {
        if predicate(event) && session_id_matches(event, session_id) {
            out.push(event.clone());
            if out.len() >= max {
                break;
            }
        }
    }
    out.reverse();
    out
}

fn session_id_matches(event: &Value, session_id: Option<&str>) -> bool {
    match session_id {
        Some(session_id) => event.get("sessionId").and_then(Value::as_str) == Some(session_id),
        None => true,
    }
}

fn is_fetch_request_paused(event: &Value) -> bool {
    event
        .get("method")
        .and_then(Value::as_str)
        .map(|method| method == "Fetch.requestPaused")
        .unwrap_or(false)
}

fn control_state_to_json(control: &NetworkControlState) -> Value {
    json!({
        "offline": control.offline,
        "throttle": control.throttle.as_ref().map(|conditions| json!({
            "offline": conditions.offline,
            "latency_ms": conditions.latency_ms,
            "download_throughput_bps": conditions.download_throughput_bps,
            "upload_throughput_bps": conditions.upload_throughput_bps,
            "connection_type": conditions.connection_type,
        })),
        "cache_disabled": control.cache_disabled,
        "bypass_service_worker": control.bypass_service_worker,
        "blocked_urls": control.blocked_urls,
        "extra_http_headers": control.extra_http_headers,
        "user_agent": control.user_agent,
        "interception": control.interception.as_ref().map(|config| json!({
            "patterns": config
                .patterns
                .iter()
                .map(|pattern| json!({
                    "url_pattern": pattern.url_pattern,
                    "request_stage": pattern.request_stage,
                }))
                .collect::<Vec<_>>(),
        })),
    })
}

#[cfg(test)]
mod control_state_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn control_state_serializes_full_surface() {
        let control = NetworkControlState {
            offline: true,
            throttle: Some(crate::network::ThrottlePreset::Slow3G.conditions()),
            cache_disabled: true,
            bypass_service_worker: true,
            blocked_urls: vec!["*://tracker.example.com/*".to_string()],
            extra_http_headers: HashMap::from([("X-Debug".to_string(), "1".to_string())]),
            user_agent: Some("ZedDebug/1.0".to_string()),
            interception: Some(InterceptionConfig {
                patterns: vec![InterceptionPattern {
                    url_pattern: "https://api.example.com/*".to_string(),
                    request_stage: Some("Request".to_string()),
                }],
            }),
        };
        let json = control_state_to_json(&control);
        assert_eq!(json["offline"].as_bool(), Some(true));
        assert_eq!(json["cache_disabled"].as_bool(), Some(true));
        assert_eq!(json["bypass_service_worker"].as_bool(), Some(true));
        assert_eq!(
            json["blocked_urls"][0].as_str(),
            Some("*://tracker.example.com/*")
        );
        assert_eq!(json["extra_http_headers"]["X-Debug"].as_str(), Some("1"));
        assert_eq!(json["user_agent"].as_str(), Some("ZedDebug/1.0"));
        let throttle = &json["throttle"];
        assert_eq!(throttle["offline"].as_bool(), Some(false));
        assert_eq!(throttle["latency_ms"].as_u64(), Some(2_000));
        assert_eq!(throttle["connection_type"].as_str(), Some("cellular3g"));
        let interception = &json["interception"];
        assert_eq!(
            interception["patterns"][0]["url_pattern"].as_str(),
            Some("https://api.example.com/*")
        );
        assert_eq!(
            interception["patterns"][0]["request_stage"].as_str(),
            Some("Request")
        );
    }

    #[test]
    fn fetch_request_paused_filter_matches_only_pause_events() {
        let paused = json!({
            "method": "Fetch.requestPaused",
            "params": { "requestId": "123" },
        });
        let will_be_sent = json!({
            "method": "Network.requestWillBeSent",
            "params": { "requestId": "123" },
        });
        assert!(is_fetch_request_paused(&paused));
        assert!(!is_fetch_request_paused(&will_be_sent));
    }

    #[test]
    fn session_id_matching_scopes_events_to_target() {
        let event = json!({ "method": "Network.requestWillBeSent", "sessionId": "s1" });
        assert!(session_id_matches(&event, Some("s1")));
        assert!(!session_id_matches(&event, Some("s2")));
        assert!(session_id_matches(&event, None));
    }
}

/// A process-wide shared browser API, registered once so the agent tools and
/// the GUI network panel drive the same sessions and store.
pub struct BrowserApiGlobal(pub Arc<AgentBrowserApi>);

impl gpui::Global for BrowserApiGlobal {}

/// Return the process-wide shared browser API, creating and registering it on
/// first use. Both the agent tools and the GUI pass the same Chromium path and
/// HTTP client, so whichever runs first establishes the shared instance.
pub fn shared_browser_api(
    cx: &mut gpui::App,
    chromium_path: Option<PathBuf>,
    http_client: Arc<dyn HttpClient>,
) -> Arc<AgentBrowserApi> {
    if let Some(global) = cx.try_global::<BrowserApiGlobal>() {
        return global.0.clone();
    }
    let api = Arc::new(AgentBrowserApi::new(
        chromium_path,
        http_client,
        cx.background_executor().clone(),
    ));
    cx.set_global(BrowserApiGlobal(api.clone()));
    api
}
