//! A read-only bottom-dock panel that shows the `secret-tool` audit log: which
//! named secrets the agent has used and when. It never shows (or stores) a
//! secret value — only names and operations.
//!
//! Mutations (`store`/`rotate`/`delete`) happen through the `secret-tool`
//! agent tool, which delegates all keychain access to the standalone `secretsh`
//! helper so a secret value never enters the Zed process.

use agent::SecretAuditEvent;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, Task, WeakEntity, Window,
    actions,
};
use ui::{Button, ButtonSize, IconName, Label, LabelSize, prelude::*};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(secret, [ToggleSecretPanel]);

const SECRET_PANEL_KEY: &str = "SecretPanel";

/// One rendered audit row: a secret name and its most recent operation.
struct SecretAuditRow {
    name: String,
    operation: String,
    at: String,
}

pub struct SecretPanel {
    focus_handle: FocusHandle,
    rows: Vec<SecretAuditRow>,
    last_error: Option<String>,
    _refresh: Task<()>,
}

impl Focusable for SecretPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SecretPanel {}

impl SecretPanel {
    pub fn new(
        _workspace: &Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                rows: Vec::new(),
                last_error: None,
                _refresh: Task::ready(()),
            };
            this.refresh(cx);
            this
        })
    }

    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: &mut gpui::AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                SecretPanel::new(workspace, window, cx)
            })
        })
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let load = agent::load_recent_secret_audit_events(cx, 200);
        self._refresh = cx.spawn(async move |this, cx| match load.await {
            Ok(events) => {
                this.update(cx, |panel, cx| {
                    panel.last_error = None;
                    panel.rows = events
                        .into_iter()
                        .map(|event: SecretAuditEvent| SecretAuditRow {
                            name: event.secret_name,
                            operation: event.operation,
                            at: event.at.to_rfc3339(),
                        })
                        .collect();
                    cx.notify();
                })
                .ok();
            }
            Err(error) => {
                this.update(cx, |panel, cx| {
                    panel.last_error = Some(format!("failed to load the audit log: {error}"));
                    cx.notify();
                })
                .ok();
            }
        });
    }
}

impl Render for SecretPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let refresh = cx.listener(|this: &mut SecretPanel, _, _, cx| this.refresh(cx));

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(Label::new("Secrets"))
                    .child(
                        Button::new("secret-refresh", "Refresh")
                            .size(ButtonSize::Compact)
                            .on_click(refresh),
                    ),
            )
            .child(
                v_flex()
                    .size_full()
                    .px_2()
                    .gap_1()
                    .when_some(self.last_error.clone(), |el, error| {
                        el.child(Label::new(error))
                    })
                    .when(self.rows.is_empty() && self.last_error.is_none(), |el| {
                        el.child(Label::new(
                            "No secrets used yet. Store or rotate secrets with the `secret-tool` agent tool.",
                        ))
                    })
                    .children(self.rows.iter().map(|row| {
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(Label::new(row.name.clone()).weight(gpui::FontWeight::SEMIBOLD))
                            .child(Label::new(row.operation.clone()).size(LabelSize::Small))
                            .child(Label::new(row.at.clone()).size(LabelSize::Small))
                    })),
            )
    }
}

impl Panel for SecretPanel {
    fn persistent_name() -> &'static str {
        "SecretPanel"
    }

    fn panel_key() -> &'static str {
        SECRET_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.refresh(cx);
        }
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        px(240.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Lock)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Secrets")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleSecretPanel)
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}

/// Registers the panel toggle action on every new workspace.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(
            |workspace: &mut Workspace, _: &ToggleSecretPanel, window, cx| {
                workspace.toggle_panel_focus::<SecretPanel>(window, cx);
            },
        );
    })
    .detach();
}
