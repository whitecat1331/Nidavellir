use std::collections::BTreeMap;
use std::time::Duration;

use agent::containers::ContainerDriver;
use agent::{ContainerStatus, VmNodeStatus, VmStatus};
use gpui::{
    App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, ListAlignment,
    ListState, Render, Task, WeakEntity, Window, actions, list, px,
};
use ui::{Button, IconName, Label, LabelSize, WithScrollbar, prelude::*};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(devops, [ToggleDevOpsPanel]);

const DEVOPS_PANEL_KEY: &str = "DevOpsPanel";
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// A bottom-dock panel showing the health of every VM and container registered
/// in the seeded vms.json / containers.json. It holds no infra of its own; the
/// targets and the health logic both come from the agent crate, which reads the
/// seeded registries.
pub struct DevOpsPanel {
    focus_handle: FocusHandle,
    list_state: ListState,
    rows: Vec<Row>,
    _refresh_task: Task<()>,
}

/// One rendered row in the flat list backing the panel.
enum Row {
    SectionHeader(String),
    NodeHeader(VmNodeStatus),
    Vm(VmStatus),
    HostHeader(String),
    Container(ContainerStatus),
}

impl Focusable for DevOpsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DevOpsPanel {}

impl DevOpsPanel {
    pub fn new(
        _workspace: &Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                list_state: ListState::new(0, ListAlignment::Top, px(24.0)),
                rows: Vec::new(),
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
                DevOpsPanel::new(workspace, window, cx)
            })
        })
    }
    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        let http_client = cx.http_client();
        self._refresh_task = cx.spawn(async move |this, cx| {
            loop {
                let vm_node_statuses = agent::vm_node_statuses(cx, http_client.clone()).await;
                let vm_statuses = agent::vm_statuses(cx, http_client.clone()).await;
                let container_statuses = agent::container_statuses().await;
                if this
                    .update(cx, |panel, cx| {
                        panel.rows = build_rows(vm_node_statuses, vm_statuses, container_statuses);
                        panel.list_state.reset(panel.rows.len());
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

    fn render_row(&self, ix: usize) -> AnyElement {
        match self.rows.get(ix) {
            Some(Row::SectionHeader(title)) => section_header(title).into_any_element(),
            Some(Row::NodeHeader(node)) => render_node_header(node).into_any_element(),
            Some(Row::Vm(status)) => render_vm_row(status).into_any_element(),
            Some(Row::HostHeader(host)) => render_host_header(host).into_any_element(),
            Some(Row::Container(status)) => render_container_row(status).into_any_element(),
            None => div().h_full().into_any_element(),
        }
    }
}

/// Flattens the three status sweeps into the panel's row list, grouping VMs
/// under their Proxmox node and containers under their SSH host.
fn build_rows(
    vm_node_statuses: Vec<VmNodeStatus>,
    vm_statuses: Vec<VmStatus>,
    container_statuses: Vec<ContainerStatus>,
) -> Vec<Row> {
    let mut rows = Vec::new();

    rows.push(Row::SectionHeader(format!(
        "Virtual machines ({})",
        vm_statuses.len()
    )));
    for node in vm_node_statuses {
        rows.push(Row::NodeHeader(node.clone()));
        for status in vm_statuses.iter().filter(|status| status.node == node.node) {
            rows.push(Row::Vm(status.clone()));
        }
    }

    rows.push(Row::SectionHeader(format!(
        "Containers ({})",
        container_statuses.len()
    )));
    let mut by_host: BTreeMap<String, Vec<ContainerStatus>> = BTreeMap::new();
    for status in container_statuses {
        by_host.entry(status.host.clone()).or_default().push(status);
    }
    for (host, statuses) in by_host {
        rows.push(Row::HostHeader(host));
        for status in statuses {
            rows.push(Row::Container(status));
        }
    }

    rows
}

fn section_header(title: &str) -> Div {
    h_flex().h_full().px_2().items_center().child(
        Label::new(title.to_string())
            .size(LabelSize::Small)
            .color(Color::Muted),
    )
}

fn render_node_header(node: &VmNodeStatus) -> Div {
    let name = if node.danger {
        format!("{} ⚠", node.node)
    } else {
        node.node.clone()
    };
    let (summary, color) = match &node.error {
        Some(_) => ("unreachable".to_string(), Color::Error),
        None => (node_resource_summary(node), Color::Muted),
    };
    h_flex()
        .h_full()
        .px_2()
        .justify_between()
        .child(
            Label::new(name)
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD),
        )
        .child(Label::new(summary).size(LabelSize::Small).color(color))
}

fn node_resource_summary(node: &VmNodeStatus) -> String {
    match (
        node.cpu,
        node.memory_used,
        node.memory_total,
        node.rootfs_used,
        node.rootfs_total,
    ) {
        (
            Some(cpu),
            Some(memory_used),
            Some(memory_total),
            Some(rootfs_used),
            Some(rootfs_total),
        ) => format!(
            "cpu {:.0}% · mem {} / {} · disk {} / {}",
            cpu * 100.0,
            format_bytes(memory_used),
            format_bytes(memory_total),
            format_bytes(rootfs_used),
            format_bytes(rootfs_total)
        ),
        _ => "—".to_string(),
    }
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
        .h_full()
        .pl_4()
        .pr_2()
        .justify_between()
        .child(Label::new(name))
        .child(Label::new(state_text).color(color))
}

fn render_host_header(host: &str) -> Div {
    h_flex().h_full().px_2().justify_between().child(
        Label::new(format!("host · {host}"))
            .size(LabelSize::Small)
            .weight(FontWeight::SEMIBOLD),
    )
}

fn render_container_row(status: &ContainerStatus) -> Div {
    let (state_text, color) = match (&status.state, &status.error) {
        (Some(_), _) => ("healthy".to_string(), Color::Success),
        (None, Some(_)) => ("unreachable".to_string(), Color::Error),
        (None, None) => ("unknown".to_string(), Color::Muted),
    };
    let name = format!("{} · {}", status.target, driver_label(status.driver));
    h_flex()
        .h_full()
        .pl_4()
        .pr_2()
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

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl Render for DevOpsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .child(Label::new("DevOps"))
                    .child(
                        Button::new("devops-refresh", "Refresh").on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.schedule_refresh(cx);
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .id("devops-rows")
                    .flex_1()
                    .min_h_0()
                    .child(
                        list(
                            self.list_state.clone(),
                            cx.processor(|this, ix, _window, _cx| this.render_row(ix)),
                        )
                        .size_full(),
                    )
                    .vertical_scrollbar_for(&self.list_state, window, cx),
            )
    }
}

impl Panel for DevOpsPanel {
    fn persistent_name() -> &'static str {
        "DevOpsPanel"
    }

    fn panel_key() -> &'static str {
        DEVOPS_PANEL_KEY
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
        Some(IconName::Server)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("DevOps")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleDevOpsPanel)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

/// Registers the panel toggle action on every new workspace.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(
            |workspace: &mut Workspace, _: &ToggleDevOpsPanel, window, cx| {
                workspace.toggle_panel_focus::<DevOpsPanel>(window, cx);
            },
        );
    })
    .detach();
}
