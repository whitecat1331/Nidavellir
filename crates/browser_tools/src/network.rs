use serde_json::{Value, json};
use std::collections::HashMap;

/// A single HTTP/HTTPS request/response reconstructed from CDP `Network.*`
/// events, keyed by CDP `requestId`.
#[derive(Debug, Clone, Default)]
pub struct NetworkRequest {
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub status: Option<u32>,
    pub status_text: Option<String>,
    pub mime_type: Option<String>,
    pub resource_type: Option<String>,
    pub initiator: Option<String>,
    pub initiator_stack: Option<Value>,
    pub request_headers: HashMap<String, String>,
    pub response_headers: HashMap<String, String>,
    pub post_data: Option<String>,
    pub timing: Option<NetworkTiming>,
    pub failure_reason: Option<String>,
    pub blocked_reason: Option<String>,
    pub cors_error_status: Option<String>,
    pub encoded_data_length: Option<f64>,
    pub from_cache: bool,
    /// True once the request has finished (or failed) and is no longer in flight.
    pub completed: bool,
}

impl NetworkRequest {
    /// Serialize to a stable JSON shape consumed by the AIUI and GUI.
    pub fn to_json(&self) -> Value {
        json!({
            "request_id": self.request_id,
            "url": self.url,
            "method": self.method,
            "status": self.status,
            "status_text": self.status_text,
            "mime_type": self.mime_type,
            "resource_type": self.resource_type,
            "initiator": self.initiator,
            "initiator_stack": self.initiator_stack,
            "request_headers": self.request_headers,
            "response_headers": self.response_headers,
            "post_data": self.post_data,
            "timing": self.timing.as_ref().map(timing_to_json),
            "failure_reason": self.failure_reason,
            "blocked_reason": self.blocked_reason,
            "cors_error_status": self.cors_error_status,
            "encoded_data_length": self.encoded_data_length,
            "from_cache": self.from_cache,
            "completed": self.completed,
        })
    }
}

/// Timing phases reported by CDP for a single request.
#[derive(Debug, Clone, Default)]
pub struct NetworkTiming {
    pub request_time: Option<f64>,
    pub proxy_start: Option<f64>,
    pub proxy_end: Option<f64>,
    pub dns_start: Option<f64>,
    pub dns_end: Option<f64>,
    pub connect_start: Option<f64>,
    pub connect_end: Option<f64>,
    pub ssl_start: Option<f64>,
    pub ssl_end: Option<f64>,
    pub send_start: Option<f64>,
    pub send_end: Option<f64>,
    pub receive_headers_end: Option<f64>,
}

/// A typed store of requests keyed by CDP `requestId`, fed from `Network.*`
/// events and importable/exportable as HAR.
#[derive(Debug, Clone, Default)]
pub struct NetworkStore {
    requests: HashMap<String, NetworkRequest>,
    order: Vec<String>,
}

impl NetworkStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the store from a single CDP event (a `Network.*` event).
    pub fn ingest(&mut self, event: &Value) {
        let Some(method) = event.get("method").and_then(Value::as_str) else {
            return;
        };
        let params = event.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "Network.requestWillBeSent" => self.ingest_request_will_be_sent(&params),
            "Network.responseReceived" => self.ingest_response_received(&params),
            "Network.loadingFinished" => self.ingest_loading_finished(&params),
            "Network.loadingFailed" => self.ingest_loading_failed(&params),
            "Network.requestWillBeSentExtraInfo" => self.ingest_request_extra_info(&params),
            "Network.responseReceivedExtraInfo" => self.ingest_response_extra_info(&params),
            "Network.requestServedFromCache" => self.ingest_served_from_cache(&params),
            _ => {}
        }
    }

    pub fn request(&self, request_id: &str) -> Option<&NetworkRequest> {
        self.requests.get(request_id)
    }

    /// Requests in arrival order.
    pub fn requests(&self) -> impl Iterator<Item = &NetworkRequest> {
        self.order.iter().filter_map(|id| self.requests.get(id))
    }

    pub fn len(&self) -> usize {
        self.requests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    fn entry(&mut self, request_id: &str) -> &mut NetworkRequest {
        let request_id = request_id.to_string();
        if !self.requests.contains_key(&request_id) {
            self.order.push(request_id.clone());
        }
        let request = self.requests.entry(request_id.clone()).or_default();
        request.request_id = request_id;
        request
    }

    /// Borrow an already-known request without creating one. Response-side
    /// events only enrich a request whose `requestWillBeSent` we captured;
    /// otherwise (buffer eviction or a cleared store) they would reconstruct a
    /// degraded entry missing method/url/headers.
    fn existing(&mut self, request_id: &str) -> Option<&mut NetworkRequest> {
        self.requests.get_mut(request_id)
    }

    pub(crate) fn insert(&mut self, request: NetworkRequest) {
        let request_id = request.request_id.clone();
        if !self.requests.contains_key(&request_id) {
            self.order.push(request_id.clone());
        }
        self.requests.insert(request_id, request);
    }

    fn ingest_request_will_be_sent(&mut self, params: &Value) {
        let Some(request_id) = string_field(params, "requestId") else {
            return;
        };
        let request = self.entry(&request_id);
        let request_data = params.get("request");
        if let Some(url) = request_data.and_then(|data| string_field(data, "url")) {
            request.url = url;
        }
        if let Some(method) = request_data.and_then(|data| string_field(data, "method")) {
            request.method = method;
        }
        if let Some(headers) = request_data.and_then(|data| data.get("headers")) {
            request.request_headers = headers_object(headers);
        }
        if let Some(post_data) = request_data
            .and_then(|data| data.get("postData"))
            .and_then(Value::as_str)
        {
            request.post_data = Some(post_data.to_string());
        }
        if let Some(resource_type) = string_field(params, "type") {
            request.resource_type = Some(resource_type);
        }
        let initiator = params.get("initiator");
        if let Some(initiator_type) = initiator.and_then(|data| string_field(data, "type")) {
            request.initiator = Some(initiator_type);
        }
        request.initiator_stack = initiator.and_then(|data| data.get("stack")).cloned();
    }

    fn ingest_response_received(&mut self, params: &Value) {
        let Some(request_id) = string_field(params, "requestId") else {
            return;
        };
        let Some(request) = self.existing(&request_id) else {
            return;
        };
        let response = params.get("response");
        if let Some(status) = response.and_then(|data| u32_field(data, "status")) {
            request.status = Some(status);
        }
        if let Some(status_text) = response.and_then(|data| string_field(data, "statusText")) {
            request.status_text = Some(status_text);
        }
        if let Some(mime_type) = response.and_then(|data| string_field(data, "mimeType")) {
            request.mime_type = Some(mime_type);
        }
        if let Some(headers) = response.and_then(|data| data.get("headers")) {
            request.response_headers.extend(headers_object(headers));
        }
        if let Some(timing) = response.and_then(|data| data.get("timing")) {
            request.timing = Some(timing_from_value(timing));
        }
        if request.url.is_empty() {
            if let Some(url) = response.and_then(|data| string_field(data, "url")) {
                request.url = url;
            }
        }
    }

    fn ingest_loading_finished(&mut self, params: &Value) {
        let Some(request_id) = string_field(params, "requestId") else {
            return;
        };
        let Some(request) = self.existing(&request_id) else {
            return;
        };
        request.encoded_data_length = f64_field(params, "encodedDataLength");
        request.completed = true;
    }

    fn ingest_loading_failed(&mut self, params: &Value) {
        let Some(request_id) = string_field(params, "requestId") else {
            return;
        };
        let Some(request) = self.existing(&request_id) else {
            return;
        };
        if let Some(error_text) = string_field(params, "errorText") {
            request.failure_reason = Some(error_text);
        }
        if let Some(blocked_reason) = string_field(params, "blockedReason") {
            request.blocked_reason = Some(blocked_reason);
        }
        if let Some(cors_error_status) = params
            .get("corsErrorStatus")
            .and_then(|status| status.get("corsError"))
            .and_then(Value::as_str)
        {
            request.cors_error_status = Some(cors_error_status.to_string());
        }
        request.completed = true;
    }

    fn ingest_request_extra_info(&mut self, params: &Value) {
        let Some(request_id) = string_field(params, "requestId") else {
            return;
        };
        let Some(request) = self.existing(&request_id) else {
            return;
        };
        if let Some(headers) = params.get("headers") {
            request.request_headers = headers_object(headers);
        }
    }

    fn ingest_response_extra_info(&mut self, params: &Value) {
        let Some(request_id) = string_field(params, "requestId") else {
            return;
        };
        let Some(request) = self.existing(&request_id) else {
            return;
        };
        if let Some(headers) = params.get("headers") {
            request.response_headers.extend(headers_object(headers));
        }
        if let Some(status) = u32_field(params, "statusCode") {
            request.status = Some(status);
        }
    }

    fn ingest_served_from_cache(&mut self, params: &Value) {
        let Some(request_id) = string_field(params, "requestId") else {
            return;
        };
        let Some(request) = self.existing(&request_id) else {
            return;
        };
        request.from_cache = true;
        request.completed = true;
    }
}

/// Filters applied when listing requests from the store. Empty filters match
/// every request.
#[derive(Debug, Clone, Default)]
pub struct RequestFilter {
    /// Substring match against the request URL (case-insensitive).
    pub url: Option<String>,
    /// Exact method match (case-insensitive).
    pub method: Option<String>,
    /// Exact status match.
    pub status: Option<u32>,
    /// Exact resource type match (case-insensitive).
    pub resource_type: Option<String>,
    /// Only requests that failed at the network layer (`failure_reason` or
    /// `blocked_reason` present) when true, or succeeded when false.
    pub failed: Option<bool>,
    /// Only in-flight requests (no `loadingFinished`/`loadingFailed` yet) when
    /// true, or completed requests when false.
    pub pending: Option<bool>,
}

impl RequestFilter {
    pub fn is_empty(&self) -> bool {
        self.url.is_none()
            && self.method.is_none()
            && self.status.is_none()
            && self.resource_type.is_none()
            && self.failed.is_none()
            && self.pending.is_none()
    }

    pub fn matches(&self, request: &NetworkRequest) -> bool {
        if let Some(url) = &self.url {
            if !request.url.to_lowercase().contains(&url.to_lowercase()) {
                return false;
            }
        }
        if let Some(method) = &self.method {
            if !request.method.eq_ignore_ascii_case(method) {
                return false;
            }
        }
        if let Some(status) = self.status {
            if request.status != Some(status) {
                return false;
            }
        }
        if let Some(resource_type) = &self.resource_type {
            let matches = request
                .resource_type
                .as_ref()
                .map(|actual| actual.eq_ignore_ascii_case(resource_type))
                .unwrap_or(false);
            if !matches {
                return false;
            }
        }
        if let Some(failed) = self.failed {
            let is_failed = request.failure_reason.is_some() || request.blocked_reason.is_some();
            if is_failed != failed {
                return false;
            }
        }
        if let Some(pending) = self.pending {
            if !request.completed != pending {
                return false;
            }
        }
        true
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn u32_field(value: &Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as u32)
}

fn f64_field(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

fn headers_object(value: &Value) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    if let Some(object) = value.as_object() {
        for (name, value) in object {
            if let Some(value) = value.as_str() {
                headers.insert(name.clone(), value.to_string());
            }
        }
    }
    headers
}

fn timing_from_value(value: &Value) -> NetworkTiming {
    NetworkTiming {
        request_time: f64_field(value, "requestTime"),
        proxy_start: f64_field(value, "proxyStart"),
        proxy_end: f64_field(value, "proxyEnd"),
        dns_start: f64_field(value, "dnsStart"),
        dns_end: f64_field(value, "dnsEnd"),
        connect_start: f64_field(value, "connectStart"),
        connect_end: f64_field(value, "connectEnd"),
        ssl_start: f64_field(value, "sslStart"),
        ssl_end: f64_field(value, "sslEnd"),
        send_start: f64_field(value, "sendStart"),
        send_end: f64_field(value, "sendEnd"),
        receive_headers_end: f64_field(value, "receiveHeadersEnd"),
    }
}

fn timing_to_json(timing: &NetworkTiming) -> Value {
    json!({
        "request_time": timing.request_time,
        "proxy_start": timing.proxy_start,
        "proxy_end": timing.proxy_end,
        "dns_start": timing.dns_start,
        "dns_end": timing.dns_end,
        "connect_start": timing.connect_start,
        "connect_end": timing.connect_end,
        "ssl_start": timing.ssl_start,
        "ssl_end": timing.ssl_end,
        "send_start": timing.send_start,
        "send_end": timing.send_end,
        "receive_headers_end": timing.receive_headers_end,
    })
}

/// Network throttling conditions, mirroring CDP `Network.emulateNetworkConditions`.
#[derive(Debug, Clone, PartialEq)]
pub struct ThrottleConditions {
    pub offline: bool,
    pub latency_ms: u32,
    pub download_throughput_bps: i64,
    pub upload_throughput_bps: i64,
    pub connection_type: Option<String>,
}

impl Default for ThrottleConditions {
    fn default() -> Self {
        Self {
            offline: false,
            latency_ms: 0,
            download_throughput_bps: -1,
            upload_throughput_bps: -1,
            connection_type: None,
        }
    }
}

impl ThrottleConditions {
    /// Serialize to CDP `Network.emulateNetworkConditions` params.
    pub fn to_cdp_params(&self) -> Value {
        let mut params = json!({
            "offline": self.offline,
            "latency": self.latency_ms,
            "downloadThroughput": self.download_throughput_bps,
            "uploadThroughput": self.upload_throughput_bps,
        });
        if let Some(connection_type) = &self.connection_type {
            params["connectionType"] = json!(connection_type);
        }
        params
    }
}

/// Named network-throttle presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottlePreset {
    Online,
    Offline,
    Slow3G,
    Fast3G,
}

impl ThrottlePreset {
    pub fn conditions(self) -> ThrottleConditions {
        match self {
            ThrottlePreset::Online => ThrottleConditions::default(),
            ThrottlePreset::Offline => ThrottleConditions {
                offline: true,
                ..ThrottleConditions::default()
            },
            ThrottlePreset::Slow3G => ThrottleConditions {
                latency_ms: 2_000,
                download_throughput_bps: 50_000,
                upload_throughput_bps: 50_000,
                connection_type: Some("cellular3g".to_string()),
                ..ThrottleConditions::default()
            },
            ThrottlePreset::Fast3G => ThrottleConditions {
                latency_ms: 563,
                download_throughput_bps: 1_474_560,
                upload_throughput_bps: 563_200,
                connection_type: Some("cellular3g".to_string()),
                ..ThrottleConditions::default()
            },
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().replace('_', "-").as_str() {
            "online" | "none" => Some(ThrottlePreset::Online),
            "offline" => Some(ThrottlePreset::Offline),
            "slow-3g" => Some(ThrottlePreset::Slow3G),
            "fast-3g" => Some(ThrottlePreset::Fast3G),
            _ => None,
        }
    }
}

/// A single CDP `Fetch` request pattern (URL pattern + request stage).
#[derive(Debug, Clone, PartialEq)]
pub struct InterceptionPattern {
    pub url_pattern: String,
    pub request_stage: Option<String>,
}

impl InterceptionPattern {
    /// Serialize to a CDP `Fetch.enable` `RequestPattern` entry.
    pub fn to_cdp_params(&self) -> Value {
        let mut params = json!({ "urlPattern": self.url_pattern });
        if let Some(request_stage) = &self.request_stage {
            params["requestStage"] = json!(request_stage);
        }
        params
    }
}

/// The active interception scope for a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InterceptionConfig {
    pub patterns: Vec<InterceptionPattern>,
}

/// Mutable network control state, tracked per session so the GUI and AIUI can
/// surface what is currently being perturbed.
#[derive(Debug, Clone, Default)]
pub struct NetworkControlState {
    pub offline: bool,
    pub throttle: Option<ThrottleConditions>,
    pub cache_disabled: bool,
    pub bypass_service_worker: bool,
    pub blocked_urls: Vec<String>,
    pub extra_http_headers: HashMap<String, String>,
    pub user_agent: Option<String>,
    pub interception: Option<InterceptionConfig>,
}

#[cfg(test)]
mod control_tests {
    use super::*;

    #[test]
    fn throttle_preset_parsing_and_conditions() {
        assert_eq!(
            ThrottlePreset::parse("slow-3g"),
            Some(ThrottlePreset::Slow3G)
        );
        assert_eq!(
            ThrottlePreset::parse("SLOW_3G"),
            Some(ThrottlePreset::Slow3G)
        );
        assert_eq!(
            ThrottlePreset::parse("offline"),
            Some(ThrottlePreset::Offline)
        );
        assert_eq!(ThrottlePreset::parse("bogus"), None);
        assert!(ThrottlePreset::Offline.conditions().offline);
        assert_eq!(ThrottlePreset::Slow3G.conditions().latency_ms, 2_000);
    }

    #[test]
    fn throttle_conditions_serialize_to_cdp_params() {
        let params = ThrottlePreset::Fast3G.conditions().to_cdp_params();
        assert_eq!(params["latency"].as_u64(), Some(563));
        assert_eq!(params["offline"].as_bool(), Some(false));
        assert_eq!(params["connectionType"].as_str(), Some("cellular3g"));
        let online = ThrottleConditions::default().to_cdp_params();
        assert!(online.get("connectionType").is_none());
    }

    #[test]
    fn interception_pattern_serializes_to_cdp_request_pattern() {
        let scoped = InterceptionPattern {
            url_pattern: "https://api.example.com/*".to_string(),
            request_stage: Some("Response".to_string()),
        };
        let params = scoped.to_cdp_params();
        assert_eq!(
            params["urlPattern"].as_str(),
            Some("https://api.example.com/*")
        );
        assert_eq!(params["requestStage"].as_str(), Some("Response"));

        let stage_optional = InterceptionPattern {
            url_pattern: "*://localhost/*".to_string(),
            request_stage: None,
        };
        let params = stage_optional.to_cdp_params();
        assert!(params.get("requestStage").is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ingests_request_lifecycle_into_typed_store() {
        let mut store = NetworkStore::new();

        store.ingest(&json!({
            "method": "Network.requestWillBeSent",
            "params": {
                "requestId": "1",
                "documentURL": "https://example.com/page",
                "type": "XHR",
                "request": {
                    "url": "https://example.com/api/items",
                    "method": "POST",
                    "headers": { "Content-Type": "application/json" },
                    "postData": "{\"q\":\"x\"}"
                },
                "initiator": { "type": "script" }
            }
        }));
        store.ingest(&json!({
            "method": "Network.responseReceived",
            "params": {
                "requestId": "1",
                "response": {
                    "url": "https://example.com/api/items",
                    "status": 404,
                    "statusText": "Not Found",
                    "mimeType": "application/json",
                    "headers": { "Content-Type": "application/json" },
                    "timing": { "requestTime": 1.5, "dnsStart": 0.1 }
                }
            }
        }));
        store.ingest(&json!({
            "method": "Network.loadingFailed",
            "params": {
                "requestId": "1",
                "errorText": "net::ERR_ABORTED",
                "blockedReason": "cors"
            }
        }));

        assert_eq!(store.len(), 1);
        let request = store.request("1").expect("request should be tracked");
        assert_eq!(request.url, "https://example.com/api/items");
        assert_eq!(request.method, "POST");
        assert_eq!(request.status, Some(404));
        assert_eq!(request.mime_type.as_deref(), Some("application/json"));
        assert_eq!(request.resource_type.as_deref(), Some("XHR"));
        assert_eq!(request.post_data.as_deref(), Some("{\"q\":\"x\"}"));
        assert_eq!(request.failure_reason.as_deref(), Some("net::ERR_ABORTED"));
        assert_eq!(request.blocked_reason.as_deref(), Some("cors"));
        assert_eq!(
            request
                .request_headers
                .get("Content-Type")
                .map(String::as_str),
            Some("application/json")
        );
        let timing = request.timing.as_ref().expect("timing should be captured");
        assert_eq!(timing.request_time, Some(1.5));
    }

    #[test]
    fn preserves_arrival_order() {
        let mut store = NetworkStore::new();
        for id in ["a", "b", "c"] {
            store.ingest(&json!({
                "method": "Network.requestWillBeSent",
                "params": { "requestId": id, "request": { "url": id, "method": "GET" } }
            }));
        }
        let ids: Vec<&str> = store
            .requests()
            .map(|request| request.request_id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn preserves_set_cookie_from_response_extra_info() {
        let mut store = NetworkStore::new();
        store.ingest(&json!({
            "method": "Network.requestWillBeSent",
            "params": {
                "requestId": "1",
                "request": { "url": "https://example.com/", "method": "GET" }
            }
        }));
        // responseReceivedExtraInfo carries Set-Cookie; responseReceived redacts
        // it. Both events must merge so Set-Cookie survives (ISSUE-0037).
        store.ingest(&json!({
            "method": "Network.responseReceivedExtraInfo",
            "params": {
                "requestId": "1",
                "statusCode": 200,
                "headers": { "set-cookie": "session=abc; Path=/" }
            }
        }));
        store.ingest(&json!({
            "method": "Network.responseReceived",
            "params": {
                "requestId": "1",
                "response": { "status": 200, "headers": { "Content-Type": "text/html" } }
            }
        }));

        let request = store.request("1").expect("request should be tracked");
        assert_eq!(
            request.response_headers.get("set-cookie").map(String::as_str),
            Some("session=abc; Path=/")
        );
        assert_eq!(
            request.response_headers.get("Content-Type").map(String::as_str),
            Some("text/html")
        );
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    use serde_json::json;

    fn store_with_requests() -> NetworkStore {
        let mut store = NetworkStore::new();
        store.ingest(&json!({
            "method": "Network.requestWillBeSent",
            "params": {
                "requestId": "ok",
                "type": "XHR",
                "request": { "url": "https://api.example.com/items", "method": "GET" }
            }
        }));
        store.ingest(&json!({
            "method": "Network.responseReceived",
            "params": { "requestId": "ok", "response": { "status": 200 } }
        }));
        store.ingest(&json!({
            "method": "Network.loadingFinished",
            "params": { "requestId": "ok" }
        }));
        store.ingest(&json!({
            "method": "Network.requestWillBeSent",
            "params": {
                "requestId": "failed",
                "type": "Script",
                "request": { "url": "https://cdn.example.com/app.js", "method": "POST" }
            }
        }));
        store.ingest(&json!({
            "method": "Network.loadingFailed",
            "params": { "requestId": "failed", "errorText": "net::ERR_BLOCKED_BY_CLIENT" }
        }));
        store.ingest(&json!({
            "method": "Network.requestWillBeSent",
            "params": {
                "requestId": "pending",
                "type": "Fetch",
                "request": { "url": "https://api.example.com/pending", "method": "GET" }
            }
        }));
        store
    }

    #[test]
    fn request_filter_matches_each_axis() {
        let store = store_with_requests();

        let url = RequestFilter {
            url: Some("cdn.example.com".to_string()),
            ..Default::default()
        };
        assert_eq!(store.requests().filter(|r| url.matches(r)).count(), 1);

        let method = RequestFilter {
            method: Some("post".to_string()),
            ..Default::default()
        };
        assert_eq!(store.requests().filter(|r| method.matches(r)).count(), 1);

        let status = RequestFilter {
            status: Some(200),
            ..Default::default()
        };
        assert_eq!(store.requests().filter(|r| status.matches(r)).count(), 1);

        let resource_type = RequestFilter {
            resource_type: Some("xhr".to_string()),
            ..Default::default()
        };
        assert_eq!(
            store
                .requests()
                .filter(|r| resource_type.matches(r))
                .count(),
            1
        );

        let failed = RequestFilter {
            failed: Some(true),
            ..Default::default()
        };
        assert_eq!(store.requests().filter(|r| failed.matches(r)).count(), 1);

        let pending = RequestFilter {
            pending: Some(true),
            ..Default::default()
        };
        assert_eq!(store.requests().filter(|r| pending.matches(r)).count(), 1);

        let empty = RequestFilter::default();
        assert!(empty.is_empty());
        assert_eq!(store.requests().filter(|r| empty.matches(r)).count(), 3);
    }

    #[test]
    fn request_to_json_exposes_core_fields() {
        let store = store_with_requests();
        let request = store.request("ok").expect("ok request");
        let value = request.to_json();
        assert_eq!(value["request_id"].as_str(), Some("ok"));
        assert_eq!(value["status"].as_u64(), Some(200));
        assert_eq!(value["method"].as_str(), Some("GET"));
        assert_eq!(value["resource_type"].as_str(), Some("XHR"));
        assert_eq!(value["completed"].as_bool(), Some(true));
        assert!(value["failure_reason"].is_null());

        let failed = store.request("failed").expect("failed request");
        let value = failed.to_json();
        assert_eq!(value["completed"].as_bool(), Some(true));
        assert!(value["failure_reason"].as_str().is_some());

        let pending = store.request("pending").expect("pending request");
        assert_eq!(pending.to_json()["completed"].as_bool(), Some(false));
    }
}
