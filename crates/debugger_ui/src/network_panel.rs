use std::sync::Arc;
use std::time::Duration;

use agent_settings::AgentSettings;
use browser_tools::{AgentBrowserApi, DrivenBy, RequestFilter, ThrottlePreset, shared_browser_api};
use feature_flags::{FeatureFlag, FeatureFlagAppExt as _, PresenceFlag, register_feature_flag};
use gpui::{
    AnyElement, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FollowMode,
    ListAlignment, ListState, ScrollHandle, Task, WeakEntity, Window, actions, div, list, point, px,
};
use serde_json::Value;
use settings::Settings;
use ui::{
    Button, IconName, Label, StatefulInteractiveElement, Tab, TintColor, WithScrollbar, prelude::*,
};
use ui_input::InputField;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(network, [ToggleNetworkPanel]);

pub struct NetworkPanelFeatureFlag;

impl FeatureFlag for NetworkPanelFeatureFlag {
    const NAME: &'static str = "browser-network-panel";
    type Value = PresenceFlag;

    // Always on. The default `enabled_for_staff()` would hide the panel from
    // everyone in release/quick builds (staff can't be set there), so gate by
    // `enabled_for_all` to keep it testable in the fork.
    fn enabled_for_all() -> bool {
        true
    }
}
register_feature_flag!(NetworkPanelFeatureFlag);

const NETWORK_PANEL_KEY: &str = "NetworkPanel";
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// The detail pane tabs shown for the selected request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    Headers,
    Response,
    Timing,
    Initiator,
    Cookies,
}

impl DetailTab {
    fn all() -> &'static [DetailTab] {
        &[
            DetailTab::Headers,
            DetailTab::Response,
            DetailTab::Timing,
            DetailTab::Initiator,
            DetailTab::Cookies,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Self::Headers => "Headers",
            Self::Response => "Response",
            Self::Timing => "Timing",
            Self::Initiator => "Initiator",
            Self::Cookies => "Cookies",
        }
    }
}

/// A flattened request row rendered in the waterfall.
#[derive(Debug, Clone)]
struct RequestRow {
    request_id: String,
    url: String,
    method: String,
    status: Option<u32>,
    resource_type: Option<String>,
    failed: bool,
}

impl RequestRow {
    fn from_value(value: &Value) -> Self {
        Self {
            request_id: value
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            url: value
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            method: value
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            status: value
                .get("status")
                .and_then(Value::as_u64)
                .map(|s| s as u32),
            resource_type: value
                .get("resource_type")
                .and_then(Value::as_str)
                .map(str::to_string),
            failed: value.get("failure_reason").is_some() || value.get("blocked_reason").is_some(),
        }
    }
}

/// A snapshot of one session's requests + control state, fetched over CDP.
struct NetworkSnapshot {
    session_id: u64,
    driven_by: String,
    requests: Vec<Value>,
    control: Value,
}

async fn fetch_snapshot(browser_api: &AgentBrowserApi) -> Option<NetworkSnapshot> {
    let sessions = browser_api.list_sessions().await;
    if sessions.is_empty() {
        log::info!("[network-panel] fetch: no browser sessions");
        return None;
    }
    let session = sessions.first()?;
    let Some(session_id) = session.get("session_id").and_then(Value::as_u64) else {
        log::info!("[network-panel] fetch: session missing session_id");
        return None;
    };
    let driven_by = session
        .get("driven_by")
        .and_then(Value::as_str)
        .unwrap_or("idle")
        .to_string();
    let requests = match browser_api
        .list_requests(session_id, &RequestFilter::default(), 0, 500)
        .await
    {
        Ok(value) => value
            .get("requests")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Err(error) => {
            log::info!("[network-panel] fetch: list_requests failed: {error:#}");
            return None;
        }
    };
    let control = match browser_api.network_control_state(session_id).await {
        Ok(control) => control,
        Err(error) => {
            log::info!("[network-panel] fetch: network_control_state failed: {error:#}");
            return None;
        }
    };
    log::info!(
        "[network-panel] fetch: session={session_id} driven_by={driven_by} requests={}",
        requests.len()
    );
    Some(NetworkSnapshot {
        session_id,
        driven_by,
        requests,
        control,
    })
}

pub struct NetworkPanel {
    browser_api: Arc<AgentBrowserApi>,
    focus_handle: FocusHandle,
    session_id: Option<u64>,
    requests: Vec<Value>,
    control: Value,
    driven_by: String,
    filter_editor: Entity<InputField>,
    block_editor: Entity<InputField>,
    record: bool,
    preserve_log: bool,
    selected_request_id: Option<String>,
    response_body: Option<String>,
    detail_tab: DetailTab,
    list_state: ListState,
    detail_scroll_handle: ScrollHandle,
    _refresh_task: Task<()>,
}

impl NetworkPanel {
    pub fn new(
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let filter_editor = cx.new(|cx| InputField::new(window, cx, "Filter by URL or method"));
            let block_editor = cx.new(|cx| InputField::new(window, cx, "Block URL pattern"));

            let chromium_path = AgentSettings::get_global(cx).browser_chromium_path.clone();
            let http_client = workspace.project().read(cx).client().http_client();
            let browser_api = shared_browser_api(cx, chromium_path, http_client);

            let mut this = Self {
                browser_api,
                focus_handle,
                session_id: None,
                requests: Vec::new(),
                control: Value::Null,
                driven_by: "idle".to_string(),
                filter_editor,
                block_editor,
                record: true,
                preserve_log: false,
                selected_request_id: None,
                response_body: None,
                detail_tab: DetailTab::Headers,
                list_state: ListState::new(0, ListAlignment::Top, px(24.0)),
                detail_scroll_handle: ScrollHandle::new(),
                _refresh_task: Task::ready(()),
            };
            this.list_state.set_follow_mode(FollowMode::Tail);
            this.schedule_refresh(cx);
            this
        })
    }

    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: &mut gpui::AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                NetworkPanel::new(workspace, window, cx)
            })
        })
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        let browser_api = self.browser_api.clone();
        self._refresh_task = cx.spawn(async move |this, cx| {
            loop {
                if let Some(snapshot) = fetch_snapshot(&browser_api).await {
                    if this
                        .update(cx, |panel, cx| {
                            panel.apply_snapshot(snapshot, cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
    }

    fn apply_snapshot(&mut self, snapshot: NetworkSnapshot, cx: &mut Context<Self>) {
        if !self.record {
            return;
        }
        let session_id = snapshot.session_id;
        let requests_len = snapshot.requests.len();
        self.session_id = Some(session_id);
        self.driven_by = snapshot.driven_by;
        self.control = snapshot.control;
        self.requests = snapshot.requests;
        let visible = self.visible_requests(cx).len();
        if visible != self.list_state.item_count() {
            self.list_state.reset(visible);
        }
        log::info!(
            "[network-panel] apply: session={} driven_by={} requests={} visible={} list_items={}",
            session_id,
            self.driven_by,
            requests_len,
            visible,
            self.list_state.item_count()
        );
        cx.notify();
    }

    fn visible_requests(&self, cx: &App) -> Vec<&Value> {
        let filter = self.filter_editor.read(cx).text(cx).to_lowercase();
        self.requests
            .iter()
            .filter(|value| {
                filter.is_empty()
                    || value
                        .get("url")
                        .and_then(Value::as_str)
                        .map(|url| url.to_lowercase().contains(&filter))
                        .unwrap_or(false)
                    || value
                        .get("method")
                        .and_then(Value::as_str)
                        .map(|method| method.to_lowercase().contains(&filter))
                        .unwrap_or(false)
            })
            .collect()
    }

    fn selected_request(&self) -> Option<&Value> {
        let request_id = self.selected_request_id.as_ref()?;
        self.requests
            .iter()
            .find(|value| value.get("request_id").and_then(Value::as_str) == Some(request_id))
    }

    fn toggle_record(&mut self, cx: &mut Context<Self>) {
        self.record = !self.record;
        cx.notify();
    }

    fn toggle_preserve_log(&mut self, cx: &mut Context<Self>) {
        // The CDP store already accumulates across navigations, so preserve-log
        // is currently a visual flag; a future navigation-aware capture can use
        // it to decide whether to clear on top-level navigation.
        self.preserve_log = !self.preserve_log;
        cx.notify();
    }

    fn clear_requests(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
            return;
        };
        let browser_api = self.browser_api.clone();
        cx.spawn(async move |this, cx| {
            let _ = browser_api.set_driven_by(session_id, DrivenBy::Human).await;
            let _ = browser_api.clear_requests(session_id).await;
            if this
                .update(cx, |panel, cx| {
                    panel.requests.clear();
                    panel.list_state.reset(0);
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        })
        .detach();
    }

    fn cycle_throttle(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
            return;
        };
        let next = next_throttle_preset(&self.control);
        let browser_api = self.browser_api.clone();
        cx.spawn(async move |this, cx| {
            let _ = browser_api.set_driven_by(session_id, DrivenBy::Human).await;
            let conditions = next.conditions();
            let _ = browser_api.set_throttle(session_id, None, conditions).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    fn toggle_offline(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
            return;
        };
        let offline = self
            .control
            .get("offline")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let browser_api = self.browser_api.clone();
        cx.spawn(async move |this, cx| {
            let _ = browser_api.set_driven_by(session_id, DrivenBy::Human).await;
            let _ = browser_api.set_offline(session_id, None, !offline).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    fn block_url(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
            return;
        };
        let url = self.block_editor.read(cx).text(cx);
        if url.trim().is_empty() {
            return;
        }
        let browser_api = self.browser_api.clone();
        let url = url.trim().to_string();
        cx.spawn(async move |this, cx| {
            let _ = browser_api.set_driven_by(session_id, DrivenBy::Human).await;
            let _ = browser_api
                .block_urls(session_id, None, std::slice::from_ref(&url))
                .await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    fn select_request(&mut self, request_id: &str, cx: &mut Context<Self>) {
        self.selected_request_id = Some(request_id.to_string());
        self.response_body = None;
        reset_detail_scroll(&self.detail_scroll_handle);
        let Some(session_id) = self.session_id else {
            return;
        };
        let browser_api = self.browser_api.clone();
        let request_id = request_id.to_string();
        cx.spawn(async move |this, cx| {
            let body = browser_api
                .get_response_body(session_id, None, &request_id, 0, 65_536)
                .await
                .ok();
            if this
                .update(cx, |panel, cx| {
                    panel.response_body = body
                        .as_ref()
                        .and_then(|value| value.get("body"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        })
        .detach();
        cx.notify();
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let throttle_label = throttle_label(&self.control);
        let offline = self
            .control
            .get("offline")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let driven_by = match self.driven_by.as_str() {
            "human" => "Human",
            "agent" => "Agent",
            _ => "Idle",
        };
        h_flex()
            .gap_1()
            .p_1()
            .child(
                Label::new(format!("driven by: {driven_by}"))
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
            .child(
                Button::new("record", if self.record { "Pause" } else { "Record" })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_record(cx))),
            )
            .child(
                Button::new("clear", "Clear")
                    .on_click(cx.listener(|this, _, _, cx| this.clear_requests(cx))),
            )
            .child(
                Button::new(
                    "preserve-log",
                    if self.preserve_log {
                        "Preserve On"
                    } else {
                        "Preserve Off"
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_preserve_log(cx))),
            )
            .child(
                Button::new("throttle", throttle_label.to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_throttle(cx))),
            )
            .child(
                Button::new("offline", offline_label(offline))
                    .toggle_state(offline)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_offline(cx))),
            )
            .child(self.filter_editor.clone())
            .child(self.block_editor.clone())
            .child(
                Button::new("block", "Block")
                    .on_click(cx.listener(|this, _, _, cx| this.block_url(cx))),
            )
    }

    fn render_entry(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let visible = self.visible_requests(cx);
        let Some(value) = visible.get(ix) else {
            return div().into_any_element();
        };
        let row = RequestRow::from_value(value);
        let status = row
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| {
                if row.failed {
                    "ERR".to_string()
                } else {
                    "…".to_string()
                }
            });
        let selected = self
            .selected_request_id
            .as_deref()
            .map(|id| id == row.request_id)
            .unwrap_or(false);
        let request_id = row.request_id.clone();
        div()
            .id(format!("request-row-{request_id}"))
            .px_1()
            .py_1()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .when(selected, |style| {
                style.bg(cx.theme().colors().element_selected)
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_request(&request_id, cx);
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(row.method.clone()))
                    .child(Label::new(status))
                    .child(Label::new(row.resource_type.clone().unwrap_or_default()))
                    .child(Label::new(row.url).truncate()),
            )
            .into_any_element()
    }

    fn render_list(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let session_count = if self.session_id.is_some() { 1 } else { 0 };
        if session_count == 0 {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(Label::new(
                    "No browser session in this window — start one with the browser tool",
                ))
                .into_any_element();
        }
        list(
            self.list_state.clone(),
            cx.processor(|this, ix, _window, cx| this.render_entry(ix, cx)),
        )
        .size_full()
        .into_any_element()
    }

    fn render_detail(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(request) = self.selected_request() else {
            return div()
                .p_2()
                .child(Label::new(
                    "Select a request to inspect its headers, body, timing, and more",
                ))
                .into_any_element();
        };
        let body = self.detail_body(request);
        v_flex()
            .id("network-detail-body")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.detail_scroll_handle)
            .p_2()
            .child(Label::new(body))
            .vertical_scrollbar_for(&self.detail_scroll_handle, window, cx)
            .into_any_element()
    }

    fn detail_body(&self, request: &Value) -> String {
        let tab = self.detail_tab;
        match tab {
            DetailTab::Headers => {
                let request_headers = request.get("request_headers");
                let response_headers = request.get("response_headers");
                format!(
                    "Request headers:\n{}\n\nResponse headers:\n{}",
                    headers_text(request_headers),
                    headers_text(response_headers)
                )
            }
            DetailTab::Response => {
                if let Some(body) = &self.response_body {
                    body.clone()
                } else {
                    "Loading response body…".to_string()
                }
            }
            DetailTab::Timing => format!(
                "{:#}",
                request.get("timing").cloned().unwrap_or(Value::Null)
            ),
            DetailTab::Initiator => format!(
                "initiator: {}\nstack: {:#}",
                request
                    .get("initiator")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                request
                    .get("initiator_stack")
                    .cloned()
                    .unwrap_or(Value::Null)
            ),
            DetailTab::Cookies => {
                let set_cookie = request
                    .get("response_headers")
                    .and_then(|headers| headers.get("set-cookie"))
                    .or_else(|| {
                        request
                            .get("response_headers")
                            .and_then(|headers| headers.get("Set-Cookie"))
                    })
                    .and_then(Value::as_str)
                    .unwrap_or("(none)");
                format!("Set-Cookie: {set_cookie}")
            }
        }
    }

    fn render_detail_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = DetailTab::all()
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let label = tab.label();
                Tab::new(("detail-tab", ix))
                    .toggle_state(self.detail_tab == *tab)
                    .child(label)
                    .on_click({
                        let tab = *tab;
                        cx.listener(move |this, _, _, cx| {
                            this.detail_tab = tab;
                            reset_detail_scroll(&this.detail_scroll_handle);
                            cx.notify();
                        })
                    })
            })
            .collect::<Vec<_>>();
        h_flex().children(tabs)
    }
}

impl Focusable for NetworkPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for NetworkPanel {}

impl Panel for NetworkPanel {
    fn persistent_name() -> &'static str {
        "NetworkPanel"
    }

    fn panel_key() -> &'static str {
        NETWORK_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        px(420.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Link)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Network Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleNetworkPanel)
    }

    fn activation_priority(&self) -> u32 {
        7
    }

    fn enabled(&self, cx: &App) -> bool {
        cx.has_flag::<NetworkPanelFeatureFlag>()
    }

    fn is_zoomed(&self, _window: &Window, _cx: &App) -> bool {
        false
    }

    fn set_zoomed(&mut self, _zoomed: bool, _window: &mut Window, _cx: &mut Context<Self>) {}
}

impl gpui::Render for NetworkPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(cx).into_any_element();
        let content = div()
            .flex_1()
            .min_h_0()
            .grid()
            .grid_cols(2)
            .grid_rows(1)
            .children(vec![
                self.render_list(window, cx).into_any_element(),
                div()
                    .size_full()
                    .flex_col()
                    .overflow_hidden()
                    .child(self.render_detail_tabs(cx))
                    .child(self.render_detail(window, cx))
                    .into_any_element(),
            ])
            .into_any_element();
        panel_root()
            .track_focus(&self.focus_handle)
            .child(toolbar)
            .child(content)
    }
}

/// The panel's root container: a flex column that fills its parent. It must
/// be `flex().flex_col()` - `flex_col()` alone only sets flex-direction, not
/// `display: flex`, so a bare `flex_col()` root is a block and the `flex_1`
/// waterfall grid row never grows to fill the panel.
fn panel_root() -> Div {
    div().flex().flex_col().size_full()
}

/// Reset the detail pane's scroll position to the top, reusing the same
/// [`ScrollHandle`]. The handle must never be replaced: `vertical_scrollbar_for`
/// caches the first handle it is given, so a fresh handle desyncs the thumb from
/// the content (ISSUE-0034).
fn reset_detail_scroll(handle: &ScrollHandle) {
    handle.set_offset(point(px(0.), px(0.)));
}

fn next_throttle_preset(control: &Value) -> ThrottlePreset {
    let offline = control
        .get("offline")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let latency = control
        .get("throttle")
        .and_then(|throttle| throttle.get("latency_ms"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if offline {
        ThrottlePreset::Online
    } else if latency == 2_000 {
        ThrottlePreset::Fast3G
    } else if latency == 563 {
        ThrottlePreset::Offline
    } else {
        ThrottlePreset::Slow3G
    }
}

fn throttle_label(control: &Value) -> &'static str {
    let offline = control
        .get("offline")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let latency = control
        .get("throttle")
        .and_then(|throttle| throttle.get("latency_ms"))
        .and_then(Value::as_u64);
    match (offline, latency) {
        (true, _) => "Offline",
        (_, Some(2_000)) => "Slow 3G",
        (_, Some(563)) => "Fast 3G",
        // Toggling offline off materialises an explicit `latency_ms: 0` even
        // though nothing is throttled, so zero means "no throttling" — not a
        // custom preset (ISSUE-0033).
        (_, Some(0)) => "Online",
        (_, Some(_)) => "Custom",
        _ => "Online",
    }
}

/// Label for the offline toggle.
///
/// State-based, matching `throttle_label`'s condition display: an offline
/// session must not show one control reading "Offline" and the adjacent toggle
/// reading "Online" (ISSUE-0031).
fn offline_label(offline: bool) -> &'static str {
    if offline {
        "Offline"
    } else {
        "Online"
    }
}

fn headers_text(value: Option<&Value>) -> String {
    let Some(object) = value.and_then(Value::as_object) else {
        return "(none)".to_string();
    };
    let mut entries: Vec<(&str, &str)> = object
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str().unwrap_or("")))
        .collect();
    entries.sort_unstable();
    entries
        .into_iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}


#[cfg(test)]
mod tests {
    use super::{headers_text, offline_label, panel_root, reset_detail_scroll, throttle_label};
    use gpui::{IntoElement, ParentElement, Pixels, ScrollHandle, Styled, TestAppContext, div, point, px, size};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[gpui::test]
    fn panel_root_grows_waterfall_grid_row(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();

        let grid_row_heights = Rc::new(RefCell::new(Vec::<Pixels>::new()));

        let content = div()
            .flex_1()
            .min_h_0()
            .grid()
            .grid_cols(2)
            .grid_rows(1)
            .on_children_prepainted({
                let heights = grid_row_heights.clone();
                move |bounds, _, _| {
                    heights.replace(bounds.iter().map(|b| b.size.height).collect());
                }
            })
            .children(vec![
                div().size_full().into_any_element(),
                div()
                    .size_full()
                    .flex_col()
                    .child(div().h(px(24.)).w_full())
                    .child(div().h(px(60.)).w_full())
                    .into_any_element(),
            ]);

        cx.draw(point(px(0.), px(0.)), size(px(1024.), px(420.)), |_, _| {
            panel_root()
                .child(div().h(px(40.)).w_full())
                .child(content)
                .into_any_element()
        });

        // 420px panel - 40px toolbar = 380px grid row. A block root (flex_col()
        // without flex()) leaves the grid row content-sized (~84px).
        let heights = grid_row_heights.borrow();
        assert_eq!(heights.len(), 2, "grid row should have two cells");
        for (i, height) in heights.iter().enumerate() {
            assert!(
                *height >= px(300.),
                "waterfall grid cell {i} should fill the panel (>=300px), got {heights:?}"
            );
        }
    }

    #[test]
    fn headers_text_sorts_header_names() {
        let headers = serde_json::json!({
            "z-last": "zebra",
            "a-first": "alpha",
            "m-middle": "mike",
        });
        let text = headers_text(Some(&headers));
        assert_eq!(text, "a-first: alpha\nm-middle: mike\nz-last: zebra");
    }

    #[test]
    fn offline_label_matches_offline_state() {
        // ISSUE-0031: the toggle used to read "Online" while offline was on,
        // contradicting the throttle label's "Offline" for the same state.
        assert_eq!(offline_label(true), "Offline");
        assert_eq!(offline_label(false), "Online");
    }

    #[test]
    fn throttle_label_treats_zero_latency_as_online() {
        // ISSUE-0033: toggling offline off materialises `latency_ms: 0`, which
        // used to be labelled "Custom".
        let unthrottled = serde_json::json!({
            "offline": false,
            "throttle": { "latency_ms": 0 },
        });
        assert_eq!(throttle_label(&unthrottled), "Online");

        // Presets, custom throttling and the offline state are unchanged.
        assert_eq!(
            throttle_label(&serde_json::json!({ "throttle": { "latency_ms": 2_000 } })),
            "Slow 3G"
        );
        assert_eq!(
            throttle_label(&serde_json::json!({ "throttle": { "latency_ms": 563 } })),
            "Fast 3G"
        );
        assert_eq!(
            throttle_label(&serde_json::json!({ "throttle": { "latency_ms": 5_000 } })),
            "Custom"
        );
        assert_eq!(throttle_label(&serde_json::json!({ "offline": true })), "Offline");
        assert_eq!(throttle_label(&serde_json::json!({})), "Online");
    }

    #[test]
    fn reset_detail_scroll_resets_offset_without_replacing_handle() {
        // The detail pane's scrollbar (vertical_scrollbar_for) caches the first
        // handle it is given, so selection/tab changes must reuse the handle
        // (reset) rather than replace it (ISSUE-0034).
        let handle = ScrollHandle::new();
        handle.set_offset(point(px(0.), px(-100.)));
        reset_detail_scroll(&handle);
        assert_eq!(handle.offset(), point(px(0.), px(0.)));
    }
}
