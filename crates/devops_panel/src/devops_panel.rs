use std::collections::BTreeMap;
use std::time::Duration;

use agent::containers::{ContainerDriver, ContainerOperation};
use agent::vms::VmOperation;
use agent::{ContainerStatus, Reachability, VmNodeStatus, VmStatus};
use gpui::{
    App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, ListAlignment,
    ListState, Render, Task, WeakEntity, Window, actions, list, px,
};
use ui::{Button, ButtonSize, IconName, Label, LabelSize, WithScrollbar, prelude::*};
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
    last_error: Option<String>,
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
                last_error: None,
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
                let container_statuses = agent::container_statuses(cx).await;
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

    fn run_vm_action(
        &mut self,
        target: String,
        vmid: u32,
        operation: VmOperation,
        cx: &mut Context<Self>,
    ) {
        // "Stop" is graceful-then-verified-then-hard: a completed shutdown task
        // does not guarantee the guest actually powered off, so `vm_stop`
        // re-checks the live status and falls back to a hard stop.
        let is_stop = matches!(operation, VmOperation::Shutdown);
        cx.spawn(async move |this, cx| {
            let result = if is_stop {
                agent::vm_stop(cx, target, vmid).await
            } else {
                agent::vm_operation(cx, target, operation, Some(vmid), true, None).await
            };
            let error = result.err().map(|output| match output {
                agent::VmToolOutput::Success { message } => message,
                agent::VmToolOutput::Error { error } => error,
            });
            this.update(cx, |panel, cx| {
                panel.last_error = error;
                panel.schedule_refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn run_container_action(
        &mut self,
        target: String,
        operation: ContainerOperation,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let error = match agent::container_operation(target, operation, None, true).await {
                Ok(_) => None,
                Err(output) => Some(match output {
                    agent::ContainerToolOutput::Success { message } => message,
                    agent::ContainerToolOutput::Error { error } => error,
                }),
            };
            this.update(cx, |panel, cx| {
                panel.last_error = error;
                panel.schedule_refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.rows.get(ix) {
            Some(Row::SectionHeader(title)) => section_header(title).into_any_element(),
            Some(Row::NodeHeader(node)) => render_node_header(node).into_any_element(),
            Some(Row::Vm(status)) => render_vm_row(status, cx).into_any_element(),
            Some(Row::HostHeader(host)) => render_host_header(host).into_any_element(),
            Some(Row::Container(status)) => render_container_row(status, cx).into_any_element(),
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
    let (resource, resource_color) = match &node.error {
        Some(_) => ("unreachable".to_string(), Color::Error),
        None => (node_resource_summary(node), Color::Muted),
    };
    v_flex()
        .h_full()
        .px_2()
        .py_1()
        .gap_0p5()
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .justify_between()
                .gap_2()
                .child(
                    Label::new(name)
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD)
                        .truncate(),
                )
                .child(ping_label(&node.ping)),
        )
        .child(
            Label::new(resource)
                .size(LabelSize::Small)
                .color(resource_color)
                .truncate(),
        )
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
        ) => {
            let mut summary = format!(
                "cpu {:.0}% · mem {} / {} · disk {} / {}",
                cpu * 100.0,
                format_bytes(memory_used),
                format_bytes(memory_total),
                format_bytes(rootfs_used),
                format_bytes(rootfs_total)
            );
            if let Some(uptime) = node.uptime {
                summary.push_str(&format!(" · up {}", format_uptime(uptime)));
            }
            summary
        }
        _ => "—".to_string(),
    }
}

fn render_vm_row(status: &VmStatus, cx: &mut Context<DevOpsPanel>) -> Div {
    let (state_text, color) = vm_state(status);
    let name = if status.danger {
        format!("{} ⚠", status.name)
    } else {
        status.name.clone()
    };
    let target = status.node.clone();
    let vmid = status.vmid.parse::<u32>().unwrap_or(0);
    let detail = vm_detail(status);

    h_flex()
        .h_full()
        .pl_4()
        .pr_2()
        .gap_2()
        .min_w_0()
        .justify_between()
        .child(
            h_flex()
                .gap_2()
                .min_w_0()
                .child(Label::new(name).truncate())
                .child(
                    Label::new(detail)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(Label::new(state_text).color(color))
                .child(vm_buttons(cx, &target, vmid)),
        )
}

/// Maps a VM's live Proxmox state to a display label + colour. A `stopped` VM
/// is rendered as `off` so a powered-off guest reads unambiguously.
fn vm_state(status: &VmStatus) -> (String, Color) {
    match (&status.state, &status.error) {
        (Some(state), _) if state == "running" => ("running".to_string(), Color::Success),
        (Some(state), _) if state == "stopped" => ("off".to_string(), Color::Muted),
        (Some(state), _) => (state.clone(), Color::Warning),
        (None, Some(_)) => ("unreachable".to_string(), Color::Error),
        (None, None) => ("unknown".to_string(), Color::Muted),
    }
}

fn vm_detail(status: &VmStatus) -> String {
    let mut parts = vec![format!("vmid {}", status.vmid)];
    if !status.roles.is_empty() {
        parts.push(status.roles.join(", "));
    }
    if let Some(ip) = &status.ip {
        parts.push(ip.clone());
    }
    if let Some(os) = &status.os {
        parts.push(os.clone());
    }
    if let (Some(used), Some(total)) = (status.memory_used, status.memory_total) {
        parts.push(format!(
            "mem {} / {}",
            format_bytes(used),
            format_bytes(total)
        ));
    }
    parts.join(" · ")
}

fn vm_buttons(cx: &mut Context<DevOpsPanel>, target: &str, vmid: u32) -> Div {
    h_flex()
        .gap_1()
        .child(vm_button(
            cx,
            "vm-start",
            target,
            vmid,
            VmOperation::Start,
            "Start",
        ))
        .child(vm_button(
            cx,
            "vm-stop",
            target,
            vmid,
            VmOperation::Shutdown,
            "Stop",
        ))
        .child(vm_button(
            cx,
            "vm-restart",
            target,
            vmid,
            VmOperation::Reboot,
            "Restart",
        ))
}

fn vm_button(
    cx: &mut Context<DevOpsPanel>,
    kind: &'static str,
    target: &str,
    vmid: u32,
    operation: VmOperation,
    label: &'static str,
) -> Button {
    let target = target.to_string();
    Button::new(format!("{kind}-{target}-{vmid}"), label)
        .size(ButtonSize::Compact)
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.run_vm_action(target.clone(), vmid, operation, cx);
        }))
}

fn render_host_header(host: &str) -> Div {
    h_flex().h_full().px_2().justify_between().child(
        Label::new(format!("host · {host}"))
            .size(LabelSize::Small)
            .weight(FontWeight::SEMIBOLD)
            .truncate(),
    )
}

fn render_container_row(status: &ContainerStatus, cx: &mut Context<DevOpsPanel>) -> Div {
    let (state_text, color) = match (&status.state, &status.error) {
        (Some(_), _) => ("healthy".to_string(), Color::Success),
        (None, Some(_)) => ("unreachable".to_string(), Color::Error),
        (None, None) => ("unknown".to_string(), Color::Muted),
    };
    let name = format!("{} · {}", status.target, driver_label(status.driver));
    let detail = container_detail(status);
    let target = status.target.clone();

    h_flex()
        .h_full()
        .pl_4()
        .pr_2()
        .gap_2()
        .min_w_0()
        .justify_between()
        .child(
            h_flex()
                .gap_2()
                .min_w_0()
                .child(Label::new(name).truncate())
                .child(
                    Label::new(detail)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(Label::new(state_text).color(color))
                .child(ping_label(&status.ping))
                .child(container_buttons(cx, &target)),
        )
}

fn container_detail(status: &ContainerStatus) -> String {
    let mut parts = Vec::new();
    if !status.roles.is_empty() {
        parts.push(status.roles.join(", "));
    }
    if let Some(ip) = &status.ip {
        parts.push(ip.clone());
    }
    if let Some(os) = &status.os {
        parts.push(os.clone());
    }
    parts.join(" · ")
}

fn container_buttons(cx: &mut Context<DevOpsPanel>, target: &str) -> Div {
    h_flex()
        .gap_1()
        .child(container_button(
            cx,
            "container-start",
            target,
            ContainerOperation::Start,
            "Start",
        ))
        .child(container_button(
            cx,
            "container-stop",
            target,
            ContainerOperation::Stop,
            "Stop",
        ))
        .child(container_button(
            cx,
            "container-restart",
            target,
            ContainerOperation::Restart,
            "Restart",
        ))
}

fn container_button(
    cx: &mut Context<DevOpsPanel>,
    kind: &'static str,
    target: &str,
    operation: ContainerOperation,
    label: &'static str,
) -> Button {
    let target = target.to_string();
    Button::new(format!("{kind}-{target}"), label)
        .size(ButtonSize::Compact)
        .on_click(cx.listener(move |this, _event, _window, cx| {
            this.run_container_action(target.clone(), operation, cx);
        }))
}

fn driver_label(driver: ContainerDriver) -> &'static str {
    match driver {
        ContainerDriver::Compose => "docker compose",
        ContainerDriver::Systemd => "systemd",
    }
}

fn ping_label(ping: &Reachability) -> Label {
    let (text, color) = match ping {
        Reachability::Reachable(latency) => (format_ping(latency), Color::Muted),
        Reachability::Timeout => ("timeout".to_string(), Color::Warning),
        Reachability::NotApplicable => ("n/a".to_string(), Color::Muted),
    };
    Label::new(text).size(LabelSize::Small).color(color)
}

fn format_ping(latency: &Duration) -> String {
    let millis = latency.as_secs_f64() * 1000.0;
    if millis >= 1.0 {
        format!("{millis:.0} ms")
    } else {
        format!("{millis:.1} ms")
    }
}

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let minutes = (seconds % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
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
            .when_some(self.last_error.clone(), |this, error| {
                this.child(
                    h_flex().px_2().pb_1().child(
                        Label::new(error)
                            .size(LabelSize::Small)
                            .color(Color::Error)
                            .truncate(),
                    ),
                )
            })
            .child(
                div()
                    .id("devops-rows")
                    .flex_1()
                    .min_h_0()
                    .child(
                        list(
                            self.list_state.clone(),
                            cx.processor(|this, ix, _window, cx| this.render_row(ix, cx)),
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
