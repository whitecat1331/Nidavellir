use std::sync::Arc;
use std::time::Duration;

use crate::proxmox::{ProxmoxApiRequest, assemble_api_request, assemble_ssh_command};
use crate::vms::{
    GatingTier, VmOperation, VmStore, VmTarget, VmTransport, gating_tier, resolve_vmid,
    validate_input,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v1 as acp;
use futures::AsyncReadExt as _;
use gpui::{App, AsyncApp, SharedString, Task};
use http_client::{AsyncBody, HttpClient, Method, StatusCode};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How often to poll a Proxmox task before giving up.
const TASK_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Maximum task polls (≈ 60s). Snapshot/rollback/migrate can outlive this; the
/// tool reports the still-running task rather than hanging forever.
const TASK_POLL_MAX: usize = 120;

/// Operate a VM or its hypervisor node on a configured Proxmox cluster.
///
/// Targets are defined by the operator in `vms.json` (next to `containers.json`
/// and `automations.json`); this tool only resolves a target id to a node/VMID
/// and assembles the request or command. The default `api` transport talks to
/// the Proxmox REST API with a scoped API token; the `ssh` transport is an
/// explicit escalation to a root shell (always confirmed) for the surface the
/// API does not model and for out-of-band recovery. There is never a silent
/// `api` → `ssh` failover.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct VmToolInput {
    /// The target id from `vms.json` (a Proxmox node).
    pub target: String,
    /// The operation to perform.
    pub operation: VmOperation,
    /// Which transport to use. Defaults to `api`; set `ssh` for an explicit
    /// root-shell escalation. SSH-only operations (`shell`) require it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<VmTransport>,
    /// The VMID to operate on (required for VM-level operations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmid: Option<u32>,
    /// Snapshot name (required for `snapshot`/`rollback`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapname: Option<String>,
    /// Target node (required for `migrate`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrate_target: Option<String>,
    /// Extra arguments (for `shell`: the node command to run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Set to `true` to confirm a mutating or SSH operation.
    #[serde(default)]
    pub confirm: bool,
    /// Required explanation for danger-tier operations (stop/reboot of an
    /// infra-critical VM or node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VmToolOutput {
    Success { message: String },
    Error { error: String },
}

impl From<VmToolOutput> for LanguageModelToolResultContent {
    fn from(value: VmToolOutput) -> Self {
        match value {
            VmToolOutput::Success { message } => message.into(),
            VmToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct VmTool;

impl AgentTool for VmTool {
    type Input = VmToolInput;
    type Output = VmToolOutput;

    const NAME: &'static str = "vm";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Execute
    }

    fn allow_in_restricted_mode() -> bool {
        false
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => {
                let vmid = input
                    .vmid
                    .map(|vmid| format!(":{vmid}"))
                    .unwrap_or_default();
                format!(
                    "VM {} on {}{}",
                    input.operation.as_str(),
                    input.target,
                    vmid
                )
                .into()
            }
            Err(_) => "Running VM operation".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let shared_http_client = cx.http_client();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| VmToolOutput::Error {
                error: format!("Failed to receive vm tool input: {error}"),
            })?;
            run_operation(input, shared_http_client, cx).await
        })
    }
}

/// One VM's read-only health, for non-agent consumers such as the infra
/// dashboard panel. Holds no infra of its own — the targets come from the
/// seeded `vms.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmStatus {
    /// `vms.json` target id (the Proxmox node).
    pub node: String,
    /// VMID as registered in `vms.json`.
    pub vmid: String,
    /// Human-readable VM name.
    pub name: String,
    /// Whether this VM is infra-critical.
    pub danger: bool,
    /// LAN / Tailscale / etc. IPs (enriched registry, D6).
    pub ips: Vec<String>,
    /// Operating system (enriched registry, D6).
    pub os: Option<String>,
    /// Human-readable role(s) (enriched registry, D6).
    pub roles: Vec<String>,
    /// Live memory usage in bytes (`data.mem`).
    pub memory_used: Option<u64>,
    /// Live memory maximum in bytes (`data.maxmem`).
    pub memory_total: Option<u64>,
    /// Live CPU count (`data.cpus`).
    pub cpus: Option<u64>,
    /// Live uptime in seconds (`data.uptime`).
    pub uptime: Option<u64>,
    /// Proxmox `status` field (`running`, `stopped`, …) when reachable.
    pub state: Option<String>,
    /// The failure reason when the status could not be read.
    pub error: Option<String>,
}

fn tool_error(error: VmToolOutput) -> String {
    match error {
        VmToolOutput::Error { error } => error,
        VmToolOutput::Success { message } => message,
    }
}

/// Queries `status/current` for every VM registered in `vms.json`, best-effort:
/// each VM is queried independently and an unreachable node/VM yields a row
/// with `state: None` + `error` rather than aborting the sweep.
pub async fn vm_statuses(cx: &AsyncApp, shared_http_client: Arc<dyn HttpClient>) -> Vec<VmStatus> {
    let store = match VmStore::load(VmStore::default_path()) {
        Ok(store) => store,
        Err(error) => {
            return vec![VmStatus {
                node: String::new(),
                vmid: String::new(),
                name: String::new(),
                danger: false,
                ips: Vec::new(),
                os: None,
                roles: Vec::new(),
                memory_used: None,
                memory_total: None,
                cpus: None,
                uptime: None,
                state: None,
                error: Some(format!("Failed to load vm config: {error}")),
            }];
        }
    };

    let mut statuses = Vec::new();
    for (node, target) in &store.targets {
        let api = match target.api.as_ref() {
            Some(api) => api,
            None => {
                for (vmid, info) in &target.vms {
                    statuses.push(VmStatus {
                        node: node.clone(),
                        vmid: vmid.clone(),
                        name: info.name.clone(),
                        danger: info.danger,
                        ips: info.ips.clone(),
                        os: info.os.clone(),
                        roles: info.roles.clone(),
                        memory_used: None,
                        memory_total: None,
                        cpus: None,
                        uptime: None,
                        state: None,
                        error: Some(format!(
                            "target \"{node}\" has no `api` transport configured"
                        )),
                    });
                }
                continue;
            }
        };

        let http_client = match resolve_http_client(target, shared_http_client.clone()) {
            Ok(client) => client,
            Err(error) => {
                let message = tool_error(error);
                for (vmid, info) in &target.vms {
                    statuses.push(VmStatus {
                        node: node.clone(),
                        vmid: vmid.clone(),
                        name: info.name.clone(),
                        danger: info.danger,
                        ips: info.ips.clone(),
                        os: info.os.clone(),
                        roles: info.roles.clone(),
                        memory_used: None,
                        memory_total: None,
                        cpus: None,
                        uptime: None,
                        state: None,
                        error: Some(message.clone()),
                    });
                }
                continue;
            }
        };

        for (vmid, info) in &target.vms {
            let mut status = VmStatus {
                node: node.clone(),
                vmid: vmid.clone(),
                name: info.name.clone(),
                danger: info.danger,
                ips: info.ips.clone(),
                os: info.os.clone(),
                roles: info.roles.clone(),
                memory_used: None,
                memory_total: None,
                cpus: None,
                uptime: None,
                state: None,
                error: None,
            };

            let vmid_number = match vmid.parse::<u32>() {
                Ok(number) => number,
                Err(error) => {
                    status.error = Some(format!("invalid VMID: {error}"));
                    statuses.push(status);
                    continue;
                }
            };

            let request = match assemble_api_request(
                target,
                VmOperation::Status,
                Some(vmid_number),
                None,
                None,
            ) {
                Ok(request) => request,
                Err(error) => {
                    status.error = Some(error.to_string());
                    statuses.push(status);
                    continue;
                }
            };

            let secret = match read_secret(cx, &api.token_secret_url).await {
                Ok(secret) => secret,
                Err(error) => {
                    status.error = Some(tool_error(error));
                    statuses.push(status);
                    continue;
                }
            };

            let value = match call_api(
                &http_client,
                &request,
                &api.token_id,
                &secret,
                &api.base_url,
            )
            .await
            {
                Ok(value) => value,
                Err(error) => {
                    status.error = Some(tool_error(error));
                    statuses.push(status);
                    continue;
                }
            };

            let data = value.get("data");
            status.state = data
                .and_then(|data| data.get("status"))
                .and_then(|state| state.as_str())
                .map(str::to_string);
            status.memory_used = data
                .and_then(|data| data.get("mem"))
                .and_then(|mem| mem.as_u64());
            status.memory_total = data
                .and_then(|data| data.get("maxmem"))
                .and_then(|maxmem| maxmem.as_u64());
            status.cpus = data
                .and_then(|data| data.get("cpus"))
                .and_then(|cpus| cpus.as_u64());
            status.uptime = data
                .and_then(|data| data.get("uptime"))
                .and_then(|uptime| uptime.as_u64());
            statuses.push(status);
        }
    }
    statuses
}

/// One Proxmox node's read-only health, for non-agent consumers such as the
/// infra dashboard panel. Holds no infra of its own — the targets come from the
/// seeded `vms.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct VmNodeStatus {
    /// `vms.json` target id (the Proxmox node).
    pub node: String,
    /// Whether this node is infra-critical.
    pub danger: bool,
    /// Fraction of node CPU in use (0.0..=1.0) from `/nodes/{node}/status`.
    pub cpu: Option<f64>,
    /// Total node memory in bytes.
    pub memory_total: Option<u64>,
    /// Used node memory in bytes.
    pub memory_used: Option<u64>,
    /// Total root filesystem size in bytes.
    pub rootfs_total: Option<u64>,
    /// Used root filesystem space in bytes.
    pub rootfs_used: Option<u64>,
    /// Live node uptime in seconds.
    pub uptime: Option<u64>,
    /// The failure reason when the status could not be read.
    pub error: Option<String>,
}

/// Queries `/nodes/{node}/status` for every node registered in `vms.json`,
/// best-effort: each node is queried independently and an unreachable node
/// yields a row with `error` rather than aborting the sweep.
pub async fn vm_node_statuses(
    cx: &AsyncApp,
    shared_http_client: Arc<dyn HttpClient>,
) -> Vec<VmNodeStatus> {
    let store = match VmStore::load(VmStore::default_path()) {
        Ok(store) => store,
        Err(error) => {
            return vec![VmNodeStatus {
                node: String::new(),
                danger: false,
                cpu: None,
                memory_total: None,
                memory_used: None,
                rootfs_total: None,
                rootfs_used: None,
                uptime: None,
                error: Some(format!("Failed to load vm config: {error}")),
            }];
        }
    };

    let mut statuses = Vec::new();
    for (node, target) in &store.targets {
        let mut status = VmNodeStatus {
            node: node.clone(),
            danger: target.danger,
            cpu: None,
            memory_total: None,
            memory_used: None,
            rootfs_total: None,
            rootfs_used: None,
            uptime: None,
            error: None,
        };

        let Some(api) = target.api.as_ref() else {
            status.error = Some(format!(
                "target \"{node}\" has no `api` transport configured"
            ));
            statuses.push(status);
            continue;
        };

        let http_client = match resolve_http_client(target, shared_http_client.clone()) {
            Ok(client) => client,
            Err(error) => {
                status.error = Some(tool_error(error));
                statuses.push(status);
                continue;
            }
        };

        let request = match assemble_api_request(target, VmOperation::NodeStatus, None, None, None)
        {
            Ok(request) => request,
            Err(error) => {
                status.error = Some(error.to_string());
                statuses.push(status);
                continue;
            }
        };

        let secret = match read_secret(cx, &api.token_secret_url).await {
            Ok(secret) => secret,
            Err(error) => {
                status.error = Some(tool_error(error));
                statuses.push(status);
                continue;
            }
        };

        let value = match call_api(
            &http_client,
            &request,
            &api.token_id,
            &secret,
            &api.base_url,
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                status.error = Some(tool_error(error));
                statuses.push(status);
                continue;
            }
        };

        let data = value.get("data");
        status.cpu = data
            .and_then(|data| data.get("cpu"))
            .and_then(|cpu| cpu.as_f64());
        status.memory_total = data
            .and_then(|data| data.get("memory"))
            .and_then(|memory| memory.get("total"))
            .and_then(|total| total.as_u64());
        status.memory_used = data
            .and_then(|data| data.get("memory"))
            .and_then(|memory| memory.get("used"))
            .and_then(|used| used.as_u64());
        status.rootfs_total = data
            .and_then(|data| data.get("rootfs"))
            .and_then(|rootfs| rootfs.get("total"))
            .and_then(|total| total.as_u64());
        status.rootfs_used = data
            .and_then(|data| data.get("rootfs"))
            .and_then(|rootfs| rootfs.get("used"))
            .and_then(|used| used.as_u64());
        status.uptime = data
            .and_then(|data| data.get("uptime"))
            .and_then(|uptime| uptime.as_u64());
        statuses.push(status);
    }
    statuses
}

/// Performs a VM operation against the configured targets on disk.
async fn run_operation(
    input: VmToolInput,
    shared_http_client: Arc<dyn HttpClient>,
    cx: &AsyncApp,
) -> Result<VmToolOutput, VmToolOutput> {
    let store = VmStore::load(VmStore::default_path()).map_err(|error| VmToolOutput::Error {
        error: format!("Failed to load vm config: {error}"),
    })?;

    let target = store
        .target(&input.target)
        .cloned()
        .ok_or_else(|| VmToolOutput::Error {
            error: format!(
                "Unknown vm target \"{}\". Configured targets: {}",
                input.target,
                store.targets.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        })?;

    let transport = input.transport.unwrap_or(VmTransport::Api);

    validate_input(
        input.operation,
        input.vmid,
        input.snapname.as_deref(),
        input.migrate_target.as_deref(),
        &input.args,
    )
    .map_err(|error| VmToolOutput::Error {
        error: error.to_string(),
    })?;

    let vmid = resolve_vmid(&target, input.vmid).map_err(|error| VmToolOutput::Error {
        error: error.to_string(),
    })?;

    let tier = gating_tier(input.operation, transport, &target, vmid);
    enforce_tier(tier, &input)?;

    let http_client = resolve_http_client(&target, shared_http_client)?;

    match transport {
        VmTransport::Api => {
            run_api(
                http_client,
                cx,
                target,
                input.operation,
                vmid,
                input.snapname,
                input.migrate_target,
            )
            .await
        }
        VmTransport::Ssh => {
            run_ssh(
                target,
                input.operation,
                vmid,
                input.snapname,
                input.migrate_target,
                input.args,
            )
            .await
        }
    }
}

/// Public, panel-facing mutation entry point that reuses the private
/// `run_operation` (gating/validation/transport/token).
pub async fn vm_operation(
    cx: &AsyncApp,
    target: String,
    operation: VmOperation,
    vmid: Option<u32>,
    confirm: bool,
    reason: Option<String>,
) -> Result<VmToolOutput, VmToolOutput> {
    let shared_http_client = cx.update(|cx| cx.http_client());
    run_operation(
        VmToolInput {
            target,
            operation,
            transport: None,
            vmid,
            snapname: None,
            migrate_target: None,
            args: Vec::new(),
            confirm,
            reason,
        },
        shared_http_client,
        cx,
    )
    .await
}

/// Powers a VM off: graceful shutdown first, then a hard stop if the guest
/// ignores ACPI. A `shutdown` task can report success while the VM is still
/// `running` (`poll_task` does not inspect `exitstatus`), so re-check the live
/// status before declaring success and fall back to a hard stop.
pub async fn vm_stop(
    cx: &AsyncApp,
    target: String,
    vmid: u32,
) -> Result<VmToolOutput, VmToolOutput> {
    vm_operation(
        cx,
        target.clone(),
        VmOperation::Shutdown,
        Some(vmid),
        true,
        None,
    )
    .await?;

    let http_client = cx.update(|cx| cx.http_client());
    let statuses = vm_statuses(cx, http_client).await;
    let still_running = statuses.iter().any(|status| {
        status.node == target
            && status.vmid == vmid.to_string()
            && status.state.as_deref() == Some("running")
    });

    if still_running {
        return vm_operation(cx, target, VmOperation::Stop, Some(vmid), true, None).await;
    }
    Ok(VmToolOutput::Success {
        message: format!("VM {vmid} stopped"),
    })
}

/// Resolves the HTTP client for the API transport: the shared client by
/// default, or a pinned client when the target's `tls_ca` names a PEM file of
/// certificates to trust (e.g. a hypervisor's self-signed node certs).
/// Verification is never disabled — the pinned client simply restricts the trust
/// anchors to those certs.
fn resolve_http_client(
    target: &VmTarget,
    shared: Arc<dyn HttpClient>,
) -> Result<Arc<dyn HttpClient>, VmToolOutput> {
    let Some(tls_ca) = target.api.as_ref().and_then(|api| api.tls_ca.as_deref()) else {
        return Ok(shared);
    };

    let pem = std::fs::read(tls_ca).map_err(|error| VmToolOutput::Error {
        error: format!("Failed to read pinned certificate file {tls_ca:?}: {error}"),
    })?;
    let client =
        reqwest_client::ReqwestClient::pinned(&pem).map_err(|error| VmToolOutput::Error {
            error: format!("Failed to build pinned TLS client for {tls_ca:?}: {error}"),
        })?;
    Ok(Arc::new(client))
}

fn enforce_tier(tier: GatingTier, input: &VmToolInput) -> Result<(), VmToolOutput> {
    let operation = input.operation.as_str();
    let target = &input.target;
    let vmid = input
        .vmid
        .map(|vmid| format!(":{vmid}"))
        .unwrap_or_default();

    let missing = |requirement: &str| VmToolOutput::Error {
        error: format!(
            "`{operation}` on `{target}{vmid}` {requirement}; re-run with `confirm: true` to proceed."
        ),
    };

    match tier {
        GatingTier::Read => Ok(()),
        GatingTier::Confirm => {
            if input.confirm {
                Ok(())
            } else {
                Err(missing("is mutating"))
            }
        }
        GatingTier::Danger => {
            if !input.confirm {
                Err(missing("targets an infra-critical VM or node"))
            } else if input
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
            {
                Err(VmToolOutput::Error {
                    error: format!(
                        "`{operation}` on `{target}{vmid}` targets an infra-critical VM or node; \
                         re-run with a non-empty `reason` explaining why."
                    ),
                })
            } else {
                Ok(())
            }
        }
        GatingTier::Ssh => {
            if input.confirm {
                Ok(())
            } else {
                Err(missing(
                    "is a root-shell escalation over the `ssh` transport",
                ))
            }
        }
    }
}

/// Runs an operation over the Proxmox REST API, polling async tasks to
/// completion and returning the JSON result as a human-readable message.
async fn run_api(
    http_client: Arc<dyn HttpClient>,
    cx: &AsyncApp,
    target: VmTarget,
    operation: VmOperation,
    vmid: Option<u32>,
    snapname: Option<String>,
    migrate_target: Option<String>,
) -> Result<VmToolOutput, VmToolOutput> {
    let api = target.api.as_ref().ok_or_else(|| VmToolOutput::Error {
        error: format!(
            "target \"{}\" has no `api` transport configured",
            target.node
        ),
    })?;
    let base_url = api.base_url.clone();

    let request = assemble_api_request(
        &target,
        operation,
        vmid,
        snapname.as_deref(),
        migrate_target.as_deref(),
    )
    .map_err(|error| VmToolOutput::Error {
        error: error.to_string(),
    })?;

    let (token_id, secret_url) = if request.write {
        let id = api
            .write_token_id
            .as_ref()
            .ok_or_else(|| VmToolOutput::Error {
                error: format!(
                    "`{}` requires a write token, but target \"{}\" has none configured",
                    operation.as_str(),
                    target.node
                ),
            })?;
        let url = api
            .write_token_secret_url
            .as_ref()
            .ok_or_else(|| VmToolOutput::Error {
                error: format!(
                    "target \"{}\" has a write token id but no write token secret URL",
                    target.node
                ),
            })?;
        (id.clone(), url.clone())
    } else {
        (api.token_id.clone(), api.token_secret_url.clone())
    };

    let secret = read_secret(cx, &secret_url).await?;
    let value = call_api(&http_client, &request, &token_id, &secret, &base_url).await?;

    // Async tasks return a UPID string; poll them to completion so the result
    // reflects the final state rather than just "task accepted".
    let message = match value.get("data").and_then(|data| data.as_str()) {
        Some(upid) if upid.starts_with("UPID:") => {
            let status_request = ProxmoxApiRequest {
                method: Method::GET,
                // Task status is node-scoped: the cluster-scoped
                // `/cluster/tasks/{upid}/status` endpoint returns HTTP 501
                // ("No 'get' handler") on PVE 8.x, so poll the node the VM
                // (and therefore the task) lives on.
                path: format!("/nodes/{}/tasks/{upid}/status", target.node),
                body: None,
                write: false,
            };
            let final_value = poll_task(
                &http_client,
                cx,
                &status_request,
                &token_id,
                &secret,
                &base_url,
                upid,
            )
            .await?;
            serde_json::to_string_pretty(final_value.get("data").unwrap_or(&final_value))
                .unwrap_or_else(|_| final_value.to_string())
        }
        _ => serde_json::to_string_pretty(value.get("data").unwrap_or(&value))
            .unwrap_or_else(|_| value.to_string()),
    };

    Ok(VmToolOutput::Success { message })
}

/// Runs an operation over SSH by assembling and executing an `ssh` command.
async fn run_ssh(
    target: VmTarget,
    operation: VmOperation,
    vmid: Option<u32>,
    snapname: Option<String>,
    migrate_target: Option<String>,
    args: Vec<String>,
) -> Result<VmToolOutput, VmToolOutput> {
    let assembled = assemble_ssh_command(
        &target,
        operation,
        vmid,
        snapname.as_deref(),
        migrate_target.as_deref(),
        &args,
    )
    .map_err(|error| VmToolOutput::Error {
        error: error.to_string(),
    })?;

    let output = util::command::new_command(&assembled.program)
        .args(&assembled.args)
        .output()
        .await
        .map_err(|error| VmToolOutput::Error {
            error: format!("Failed to run `{}`: {error}", assembled.display()),
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut message = String::new();
    if !stdout.trim().is_empty() {
        message.push_str(stdout.trim());
    }
    if !stderr.trim().is_empty() {
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(stderr.trim());
    }
    if !output.status.success() {
        return Err(VmToolOutput::Error {
            error: format!(
                "`{}` exited with {}: {message}",
                assembled.display(),
                output.status
            ),
        });
    }
    if message.is_empty() {
        message = format!("`{}` completed successfully.", assembled.display());
    }
    Ok(VmToolOutput::Success { message })
}

/// Reads a token secret from the platform keychain. Secrets are never stored in
/// `vms.json`; the config only carries the keychain URL they live at.
async fn read_secret(cx: &AsyncApp, url: &str) -> Result<String, VmToolOutput> {
    let credentials = cx
        .update(|cx| cx.read_credentials(url))
        .await
        .map_err(|error| VmToolOutput::Error {
            error: format!("Failed to read token secret from keychain ({url}): {error}"),
        })?;
    let Some((_username, secret)) = credentials else {
        return Err(VmToolOutput::Error {
            error: format!(
                "No token secret stored under keychain URL {url}; seed it before using the `vm` tool."
            ),
        });
    };
    Ok(String::from_utf8_lossy(&secret).into_owned())
}

/// Sends one Proxmox API request and returns the parsed JSON response.
async fn call_api(
    http_client: &Arc<dyn HttpClient>,
    request: &ProxmoxApiRequest,
    token_id: &str,
    token_secret: &str,
    base_url: &str,
) -> Result<serde_json::Value, VmToolOutput> {
    let full_url = format!("{}{}", base_url.trim_end_matches('/'), request.path);
    let mut builder = http_client::Request::builder()
        .method(request.method.clone())
        .uri(full_url.as_str())
        .header(
            "Authorization",
            format!("PVEAPIToken={token_id}={token_secret}"),
        );
    let body = match &request.body {
        Some(value) => {
            builder = builder.header("Content-Type", "application/json");
            AsyncBody::from(http_client::Json(value.clone()))
        }
        None => AsyncBody::empty(),
    };
    let request = builder.body(body).map_err(|error| VmToolOutput::Error {
        error: format!("Failed to build Proxmox API request for {full_url}: {error}"),
    })?;

    let mut response = http_client
        .send(request)
        .await
        .map_err(|error| VmToolOutput::Error {
            error: format!(
                "Proxmox API request to {full_url} failed: {}. \
                 (Timeout/unreachable => network or VPN down — check `tailscale status`; \
                 certificate/UnknownIssuer => TLS pinning or CA mismatch; \
                 connection refused => node up but its control plane is down.) \
                 Use `transport: \"ssh\"` for out-of-band recovery. This tool \
                 never fails over automatically.",
                error.root_cause()
            ),
        })?;

    let status = response.status();
    let mut bytes = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| VmToolOutput::Error {
            error: format!("Failed to read Proxmox API response: {error}"),
        })?;

    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| VmToolOutput::Error {
            error: format!(
                "Proxmox API returned non-JSON (HTTP {status}): {}",
                String::from_utf8_lossy(&bytes)
            ),
        })?;

    if !status.is_success() {
        return Err(VmToolOutput::Error {
            error: format_proxmox_error(status, &value),
        });
    }
    Ok(value)
}

/// Polls a Proxmox task (`UPID:…`) until it reaches a terminal state.
async fn poll_task(
    http_client: &Arc<dyn HttpClient>,
    cx: &AsyncApp,
    status_request: &ProxmoxApiRequest,
    token_id: &str,
    token_secret: &str,
    base_url: &str,
    upid: &str,
) -> Result<serde_json::Value, VmToolOutput> {
    for _ in 0..TASK_POLL_MAX {
        cx.background_executor().timer(TASK_POLL_INTERVAL).await;
        let value = call_api(
            http_client,
            status_request,
            token_id,
            token_secret,
            base_url,
        )
        .await?;
        let status = value
            .get("data")
            .and_then(|data| data.get("status"))
            .and_then(|status| status.as_str())
            .unwrap_or("unknown");
        if status == "stopped" {
            return Ok(value);
        }
    }
    Err(VmToolOutput::Error {
        error: format!(
            "task {upid} did not reach a terminal state within {}s; poll it again with `tasks`",
            TASK_POLL_MAX * TASK_POLL_INTERVAL.as_millis() as usize / 1000
        ),
    })
}

fn format_proxmox_error(status: StatusCode, value: &serde_json::Value) -> String {
    let detail = value
        .get("errors")
        .map(|errors| errors.to_string())
        .unwrap_or_else(|| value.to_string());
    format!("Proxmox API error (HTTP {}): {detail}", status.as_u16())
}
