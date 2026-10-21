use std::time::Duration;

use agent::containers::ContainerDriver;
use agent::{ContainerStatus, VmStatus};
use gpui::{
    actions, px, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, Render, Task,
    WeakEntity, Window,
};
use ui::{Button, IconName, Label, LabelSize, StatefulInteractiveElement, prelude::*};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(infra, [ToggleInfraPanel]);

const INFRA_PANEL_KEY: &str = "InfraPanel";
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// A read-only, bottom-dock panel showing the health of every VM and container
/// registered in the seeded `vms.json` / `containers.json`. It holds no infra of
/// its own — the targets and the health logic both come from the `agent` crate,
/// which reads the seeded registries.
pub struct InfraPanel {
    focus_handle: FocusHandle,
    vm_statuses: Vec<VmStatus>,
    container_statuses: Vec<ContainerStatus>,
    _refresh_task: Task<()>,
}

impl Focusable for InfraPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for InfraPanel {}

impl InfraPanel {
    pub fn new(
        _workspace: &Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                vm_statuses: Vec::new(),
                container_statuses: Vec::new(),
                _refresh_task: Task::ready(()),
            };
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
                InfraPanel::new(workspace, window, cx)
            })
        })
    }

    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        let http_client = cx.http_client();
        self._refresh_task = cx.spawn(async move |this, cx| {
            loop {
                let vm_statuses = agent::vm_statuses(cx, http_client.clone()).await;
                let container_statuses = agent::container_statuses().await;
                if this
                    .update(cx, |panel, cx| {
                        panel.vm_statuses = vm_statuses;
                        panel.container_statuses = container_statuses;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
    }

    fn render_rows(&self) -> Vec<Div> {
        let mut rows = Vec::new();
        rows.push(section_header(&format!(
            "Virtual machines ({})",
            self.vm_statuses.len()
        )));
        for status in &self.vm_statuses {
            rows.push(render_vm_row(status));
        }
        rows.push(section_header(&format!(
            "Containers ({})",
            self.container_statuses.len()
        )));
        for status in &self.container_statuses {
            rows.push(render_container_row(status));
        }
        rows
    }
}

fn section_header(title: &str) -> Div {
    h_flex()
        .pt_2()
        .pb_1()
        .child(
            Label::new(title.to_string())
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
}

fn render_vm_row(status: &VmStatus) -> Div {
    let (state_text, color) = match (&status.state, &status.error) {
        (Some(state), _) if state == "running" => {
            (format!("running · vmid {}", status.vmid), Color::Success)
        }
        (Some(state), _) => (format!("{state} · vmid {}", status.vmid), Color::Warning),
        (None, Some(_)) => ("unreachable".to_string(), Color::Error),
        (None, None) => ("unknown".to_string(), Color::Muted),
    };
    let name = if status.danger {
        format!("{} ⚠", status.name)
    } else {
        status.name.clone()
    };
    h_flex()
        .justify_between()
        .child(Label::new(name))
        .child(Label::new(state_text).color(color))
}

fn render_container_row(status: &ContainerStatus) -> Div {
    let (state_text, color) = match (&status.state, &status.error) {
        (Some(_), _) => ("healthy".to_string(), Color::Success),
        (None, Some(_)) => ("unreachable".to_string(), Color::Error),
        (None, None) => ("unknown".to_string(), Color::Muted),
    };
    let name = format!("{} · {}", status.target, driver_label(status.driver));
    h_flex()
        .justify_between()
        .child(Label::new(name))
        .child(Label::new(state_text).color(color))
}

fn driver_label(driver: ContainerDriver) -> &'static str {
    match driver {
        ContainerDriver::Compose => "docker compose",
        ContainerDriver::Systemd => "systemd",
    }
}

impl Render for InfraPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(Label::new("Infrastructure"))
                    .child(
                        Button::new("infra-refresh", "Refresh").on_click(
                            cx.listener(|this, _event, _window, cx| {
                                this.schedule_refresh(cx);
                            }),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .id("infra-rows")
                    .flex_1()
                    .px_2()
                    .pb_2()
                    .overflow_y_scroll()
                    .children(self.render_rows()),
            )
    }
}

impl Panel for InfraPanel {
    fn persistent_name() -> &'static str {
        "InfraPanel"
    }

    fn panel_key() -> &'static str {
        INFRA_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        px(300.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Link)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Infrastructure")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleInfraPanel)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

/// Registers the panel toggle action on every new workspace.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(
            |workspace: &mut Workspace, _: &ToggleInfraPanel, window, cx| {
                workspace.toggle_panel_focus::<InfraPanel>(window, cx);
            },
        );
    })
    .detach();
}
