use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use agent::containers::{ContainerDriver, ContainerOperation};
use agent::vms::VmOperation;
use agent::{ContainerStatus, DevOpsConfig, PingStatuses, Reachability, VmNodeStatus, VmStatus};
use editor::{Editor, EditorEvent};
use gpui::{
    App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, ListAlignment,
    ListState, Pixels, Render, Stateful, Task, WeakEntity, Window, actions, list, px,
};
use ui::{Button, ButtonSize, Checkbox, IconName, Label, LabelSize, WithScrollbar, prelude::*};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(devops, [ToggleDevOpsPanel]);

const DEVOPS_PANEL_KEY: &str = "DevOpsPanel";
/// How often the scheduler wakes to check whether a signal is due.
const SCHEDULER_TICK: Duration = Duration::from_millis(500);
/// Upper bound for a parsed poll interval (one year). Any longer interval could
/// overflow the scheduler's `Instant + Duration` arithmetic; this is far beyond
/// any sane poll cadence and just guards against a panic.
const MAX_INTERVAL_SECS: u64 = 365 * 24 * 60 * 60;
/// Fixed column widths so the dock reads as a stable grid.
const NAME_COL_WIDTH: Pixels = px(140.);
const STATE_COL_WIDTH: Pixels = px(84.);
const PING_COL_WIDTH: Pixels = px(56.);

/// A bottom-dock panel showing the health of every VM and container registered
/// in the seeded vms.json / containers.json. It holds no infra of its own; the
/// targets and the health logic both come from the agent crate, which reads the
/// seeded registries.
pub struct DevOpsPanel {
    focus_handle: FocusHandle,
    list_state: ListState,
    rows: Vec<Row>,
    last_error: Option<String>,
    config: DevOpsConfig,
    pings: PingStatuses,
    node_reports: Vec<VmNodeStatus>,
    vm_reports: Vec<VmStatus>,
    container_reports: Vec<ContainerStatus>,
    active: bool,
    ping_editor: Entity<Editor>,
    proxmox_editor: Entity<Editor>,
    container_editor: Entity<Editor>,
    _task: Task<()>,
}

/// Which poll signal an interval control edits.
#[derive(Debug, Clone, Copy)]
enum IntervalKind {
    Ping,
    Proxmox,
    Container,
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
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let config = DevOpsConfig::load(DevOpsConfig::default_path()).unwrap_or_else(|error| {
                log::error!("failed to load devops config, using defaults: {error}");
                DevOpsConfig::default()
            });
            let ping_editor =
                new_interval_editor(IntervalKind::Ping, config.ping_interval_secs, window, cx);
            let proxmox_editor = new_interval_editor(
                IntervalKind::Proxmox,
                config.proxmox_interval_secs,
                window,
                cx,
            );
            let container_editor = new_interval_editor(
                IntervalKind::Container,
                config.container_interval_secs,
                window,
                cx,
            );
            let mut this = Self {
                focus_handle: cx.focus_handle(),
                list_state: ListState::new(0, ListAlignment::Top, px(24.0)),
                rows: Vec::new(),
                last_error: None,
                config,
                pings: PingStatuses::default(),
                node_reports: Vec::new(),
                vm_reports: Vec::new(),
                container_reports: Vec::new(),
                active: false,
                ping_editor,
                proxmox_editor,
                container_editor,
                _task: Task::ready(()),
            };
            this.start_scheduler(cx);
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

    /// Starts the single scheduler loop that runs the ping and report sweeps on
    /// their own cadences. When the panel is not live (unchecked or closed) the
    /// loop idles without making any network calls.
    fn start_scheduler(&mut self, cx: &mut Context<Self>) {
        let http_client = cx.http_client();
        self._task = cx.spawn(async move |this, cx| {
            let mut next_ping = Instant::now();
            let mut next_proxmox = Instant::now();
            let mut next_container = Instant::now();
            let mut initialized = false;
            loop {
                let (running, ping_interval, proxmox_interval, container_interval) = match this
                    .read_with(cx, |panel, _cx| {
                        (
                            panel.live(),
                            panel.ping_interval(),
                            panel.proxmox_interval(),
                            panel.container_interval(),
                        )
                    }) {
                    Ok(intervals) => intervals,
                    Err(_) => break,
                };
                let now = Instant::now();
                if running && !initialized {
                    let (pings, nodes, vms, containers) = futures::join!(
                        agent::ping_statuses(cx),
                        agent::vm_node_statuses(cx, http_client.clone()),
                        agent::vm_statuses(cx, http_client.clone()),
                        agent::container_statuses(),
                    );
                    if this
                        .update(cx, |panel, cx| {
                            panel.pings = pings;
                            panel.node_reports = nodes;
                            panel.vm_reports = vms;
                            panel.container_reports = containers;
                            panel.rebuild_rows(cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                    next_ping = Instant::now() + ping_interval;
                    next_proxmox = Instant::now() + proxmox_interval;
                    next_container = Instant::now() + container_interval;
                    initialized = true;
                } else if running {
                    if now >= next_ping {
                        let pings = agent::ping_statuses(cx).await;
                        if this
                            .update(cx, |panel, cx| {
                                panel.pings = pings;
                                panel.rebuild_rows(cx);
                            })
                            .is_err()
                        {
                            break;
                        }
                        next_ping = Instant::now() + ping_interval;
                    }
                    if now >= next_proxmox {
                        let nodes = agent::vm_node_statuses(cx, http_client.clone()).await;
                        let vms = agent::vm_statuses(cx, http_client.clone()).await;
                        if this
                            .update(cx, |panel, cx| {
                                panel.node_reports = nodes;
                                panel.vm_reports = vms;
                                panel.rebuild_rows(cx);
                            })
                            .is_err()
                        {
                            break;
                        }
                        next_proxmox = Instant::now() + proxmox_interval;
                    }
                    if now >= next_container {
                        let containers = agent::container_statuses().await;
                        if this
                            .update(cx, |panel, cx| {
                                panel.container_reports = containers;
                                panel.rebuild_rows(cx);
                            })
                            .is_err()
                        {
                            break;
                        }
                        next_container = Instant::now() + container_interval;
                    }
                }
                cx.background_executor().timer(SCHEDULER_TICK).await;
            }
        });
    }

    /// One-shot full refresh, used after a lifecycle action so the result shows
    /// promptly regardless of the live/scheduled cadence.
    fn refresh_now(&mut self, cx: &mut Context<Self>) {
        let http_client = cx.http_client();
        cx.spawn(async move |this, cx| {
            let (pings, nodes, vms, containers) = futures::join!(
                agent::ping_statuses(cx),
                agent::vm_node_statuses(cx, http_client.clone()),
                agent::vm_statuses(cx, http_client.clone()),
                agent::container_statuses(),
            );
            this.update(cx, |panel, cx| {
                panel.pings = pings;
                panel.node_reports = nodes;
                panel.vm_reports = vms;
                panel.container_reports = containers;
                panel.rebuild_rows(cx);
            })
            .ok();
        })
        .detach();
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        self.rows = build_rows(
            &self.node_reports,
            &self.vm_reports,
            &self.container_reports,
        );
        self.list_state.reset(self.rows.len());
        cx.notify();
    }

    /// Whether polling should run: the live checkbox is on and the dock is open.
    fn live(&self) -> bool {
        self.config.live && self.active
    }

    fn ping_interval(&self) -> Duration {
        let secs = if self.config.live {
            self.config.live_ping_interval_secs
        } else {
            self.config.ping_interval_secs
        };
        Duration::from_secs(secs.max(1))
    }

    fn proxmox_interval(&self) -> Duration {
        let secs = if self.config.live {
            self.config.live_proxmox_interval_secs
        } else {
            self.config.proxmox_interval_secs
        };
        Duration::from_secs(secs.max(1))
    }

    fn container_interval(&self) -> Duration {
        let secs = if self.config.live {
            self.config.live_container_interval_secs
        } else {
            self.config.container_interval_secs
        };
        Duration::from_secs(secs.max(1))
    }

    fn set_live(&mut self, live: bool, cx: &mut Context<Self>) {
        self.config.live = live;
        self.save_config();
        cx.notify();
    }

    fn set_interval(&mut self, kind: IntervalKind, seconds: u64, cx: &mut Context<Self>) {
        match kind {
            IntervalKind::Ping => self.config.ping_interval_secs = seconds,
            IntervalKind::Proxmox => self.config.proxmox_interval_secs = seconds,
            IntervalKind::Container => self.config.container_interval_secs = seconds,
        }
        self.save_config();
        cx.notify();
    }

    fn current_interval_secs(&self, kind: IntervalKind) -> u64 {
        match kind {
            IntervalKind::Ping => self.config.ping_interval_secs,
            IntervalKind::Proxmox => self.config.proxmox_interval_secs,
            IntervalKind::Container => self.config.container_interval_secs,
        }
    }

    /// Commits a free-form interval editor on blur. Invalid input reverts the
    /// editor text to the previous interval without changing the config.
    fn commit_interval(
        &mut self,
        kind: IntervalKind,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = editor.read(cx).text(cx);
        match parse_interval(&text) {
            Some(seconds) => self.set_interval(kind, seconds, cx),
            None => {
                let previous = format_interval(self.current_interval_secs(kind));
                editor.update(cx, |editor, cx| editor.set_text(previous, window, cx));
            }
        }
    }

    fn save_config(&self) {
        if let Err(error) = self.config.save(DevOpsConfig::default_path()) {
            log::error!("failed to save devops config: {error}");
        }
    }

    fn live_checkbox(&self, cx: &mut Context<Self>) -> Checkbox {
        let weak = cx.weak_entity();
        Checkbox::new(
            "devops-live",
            if self.config.live {
                ToggleState::Selected
            } else {
                ToggleState::Unselected
            },
        )
        .label("Live")
        .label_size(LabelSize::Small)
        .on_click(move |state, _window, cx| {
            let live = *state == ToggleState::Selected;
            weak.update(cx, |this, cx| this.set_live(live, cx)).ok();
        })
    }

    fn live_checkbox(&self, cx: &mut Context<Self>) -> Checkbox {
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
                panel.refresh_now(cx);
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
                panel.refresh_now(cx);
            })
            .ok();
        })
        .detach();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.rows.get(ix) {
            Some(Row::SectionHeader(title)) => section_header(title).into_any_element(),
            Some(Row::NodeHeader(node)) => {
                let ping = self.ping_for_node(&node.node);
                render_node_header(node, ping).into_any_element()
            }
            Some(Row::Vm(status)) => {
                let ping = self.ping_for_vm(&status.node, &status.vmid);
                render_vm_row(status, ping, cx).into_any_element()
            }
            Some(Row::HostHeader(host)) => render_host_header(host).into_any_element(),
            Some(Row::Container(status)) => {
                let ping = self.ping_for_host(&status.target);
                render_container_row(status, ping, cx).into_any_element()
            }
            None => div().h_full().into_any_element(),
        }
    }

    fn ping_for_node(&self, node: &str) -> Reachability {
        self.pings
            .nodes
            .get(node)
            .copied()
            .unwrap_or(Reachability::NotApplicable)
    }

    fn ping_for_vm(&self, node: &str, vmid: &str) -> Reachability {
        self.pings
            .vms
            .get(&(node.to_string(), vmid.to_string()))
            .copied()
            .unwrap_or(Reachability::NotApplicable)
    }

    fn ping_for_host(&self, target: &str) -> Reachability {
        self.pings
            .hosts
            .get(target)
            .copied()
            .unwrap_or(Reachability::NotApplicable)
    }
}

/// Creates a single-line editor seeded with the current interval value, wired so
/// that blurring it commits the (possibly invalid) typed duration.
fn new_interval_editor(
    kind: IntervalKind,
    current_secs: u64,
    window: &mut Window,
    cx: &mut Context<DevOpsPanel>,
) -> Entity<Editor> {
    let editor = cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text(format_interval(current_secs), window, cx);
        editor
    });
    cx.subscribe_in(
        &editor,
        window,
        move |this, editor, event: &EditorEvent, window, cx| {
            if matches!(event, EditorEvent::Blurred) {
                this.commit_interval(kind, &editor, window, cx);
            }
        },
    )
    .detach();
    editor
}

/// A fixed-width flex column cell so per-row stats and buttons align across the
/// whole list.
fn fixed_cell(width: Pixels, child: impl IntoElement) -> Div {
    div().w(width).flex_shrink_0().child(child)
}

/// Renders one interval label plus its free-form single-line editor.
fn interval_field(label: &'static str, editor: &Entity<Editor>) -> Div {
    h_flex()
        .gap_1()
        .items_center()
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        .child(div().w(px(64.)).child(editor.clone()))
}

/// Flattens the three status sweeps into the panel's row list, grouping VMs
/// under their Proxmox node and containers under their SSH host.
fn build_rows(
    vm_node_statuses: &[VmNodeStatus],
    vm_statuses: &[VmStatus],
    container_statuses: &[ContainerStatus],
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
        by_host
            .entry(status.host.clone())
            .or_default()
            .push(status.clone());
    }
    for (host, statuses) in by_host {
        rows.push(Row::HostHeader(host));
        for status in statuses {
            rows.push(Row::Container(status));
        }
    }

    rows
}

fn section_header(title: &str) -> Stateful<Div> {
    let id = if title.starts_with("Virtual machines") {
        "devops-vm-section"
    } else {
        "devops-container-section"
    };
    h_flex().id(id).h_full().px_2().items_center().child(
        Label::new(title.to_string())
            .size(LabelSize::Small)
            .color(Color::Muted),
    )
}

fn render_node_header(node: &VmNodeStatus, ping: Reachability) -> Stateful<Div> {
    let name = if node.danger {
        format!("{} ⚠", node.node)
    } else {
        node.node.clone()
    };
    let (resource, resource_color) = match &node.error {
        Some(_) => ("unreachable".to_string(), Color::Error),
        None => (node_resource_summary(node), Color::Muted),
    };
    h_flex()
        .id(format!("devops-node-{}", node.node))
        .h_full()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .child(fixed_cell(
            NAME_COL_WIDTH,
            Label::new(name)
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD)
                .truncate(),
        ))
        .child(
            Label::new(resource)
                .size(LabelSize::Small)
                .color(resource_color)
                .flex_1()
                .truncate(),
        )
        .child(div().w(STATE_COL_WIDTH).flex_shrink_0())
        .child(fixed_cell(PING_COL_WIDTH, ping_label(&ping)))
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

fn render_vm_row(
    status: &VmStatus,
    ping: Reachability,
    cx: &mut Context<DevOpsPanel>,
) -> Stateful<Div> {
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
        .id(format!("devops-vm-{}-{}", status.node, status.vmid))
        .h_full()
        .pl_4()
        .pr_2()
        .gap_2()
        .items_center()
        .child(fixed_cell(NAME_COL_WIDTH, Label::new(name).truncate()))
        .child(
            Label::new(detail)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .flex_1()
                .truncate(),
        )
        .child(fixed_cell(
            STATE_COL_WIDTH,
            Label::new(state_text).color(color),
        ))
        .child(fixed_cell(PING_COL_WIDTH, ping_label(&ping)))
        .child(vm_buttons(cx, &target, vmid).flex_shrink_0())
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

fn render_host_header(host: &str) -> Stateful<Div> {
    h_flex()
        .id(format!("devops-host-{}", host))
        .h_full()
        .px_2()
        .justify_between()
        .child(
            Label::new(format!("host · {host}"))
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD)
                .truncate(),
        )
}

fn render_container_row(
    status: &ContainerStatus,
    ping: Reachability,
    cx: &mut Context<DevOpsPanel>,
) -> Stateful<Div> {
    let (state_text, color) = match (&status.state, &status.error) {
        (Some(_), _) => ("healthy".to_string(), Color::Success),
        (None, Some(_)) => ("unreachable".to_string(), Color::Error),
        (None, None) => ("unknown".to_string(), Color::Muted),
    };
    let name = format!("{} · {}", status.target, driver_label(status.driver));
    let detail = container_detail(status);
    let target = status.target.clone();

    h_flex()
        .id(format!("devops-container-{}", status.target))
        .h_full()
        .pl_4()
        .pr_2()
        .gap_2()
        .items_center()
        .child(fixed_cell(NAME_COL_WIDTH, Label::new(name).truncate()))
        .child(
            Label::new(detail)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .flex_1()
                .truncate(),
        )
        .child(fixed_cell(
            STATE_COL_WIDTH,
            Label::new(state_text).color(color),
        ))
        .child(fixed_cell(PING_COL_WIDTH, ping_label(&ping)))
        .child(container_buttons(cx, &target).flex_shrink_0())
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

fn parse_interval(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|character: char| !character.is_ascii_digit());
    let (digits, unit) = match split {
        Some(index) => (&text[..index], text[index..].trim()),
        None => (text, ""),
    };
    if digits.is_empty() {
        return None;
    }
    let value: u64 = digits.parse().ok()?;
    let multiplier = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return None,
    };
    let seconds = value.checked_mul(multiplier)?;
    if seconds == 0 || seconds > MAX_INTERVAL_SECS {
        return None;
    }
    Some(seconds)
}

fn format_interval(seconds: u64) -> String {
    if seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
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
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(self.live_checkbox(cx))
                            .child(interval_field("ping", &self.ping_editor))
                            .child(interval_field("proxmox", &self.proxmox_editor))
                            .child(interval_field("docker", &self.container_editor)),
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

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        cx.notify();
    }

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
