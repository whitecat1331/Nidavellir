use anyhow::{Result, anyhow, bail};
use http_client::Method;
use serde_json::json;

use crate::containers::{AssembledCommand, assemble_ssh};
use crate::vms::{VmOperation, VmTarget};

/// A fully-assembled Proxmox API request. Pure: no network I/O and no secret
/// material — the tool resolves the token secret and performs the send.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxmoxApiRequest {
    pub method: Method,
    /// Path relative to `base_url`, e.g. `/nodes/compy/qemu/100/status/current`.
    pub path: String,
    /// JSON body for POST requests.
    pub body: Option<serde_json::Value>,
    /// Mutating requests must use the scoped write token, not the read token.
    pub write: bool,
}

fn require_vmid(vmid: Option<u32>) -> Result<u32> {
    vmid.ok_or_else(|| anyhow!("operation requires a `vmid`"))
}

/// Assembles the `/api2/json` request that performs `operation` against
/// `target`. `base_url` (and the token) are read from the target's API config by
/// the caller; this function only decides method, path, and body.
pub fn assemble_api_request(
    target: &VmTarget,
    operation: VmOperation,
    vmid: Option<u32>,
    snapname: Option<&str>,
    migrate_target: Option<&str>,
) -> Result<ProxmoxApiRequest> {
    if target.api.is_none() {
        bail!(
            "target \"{}\" has no `api` transport configured",
            target.node
        );
    }
    let node = &target.node;
    let vm_path = |id: u32| format!("/nodes/{node}/qemu/{id}");
    let get = |path: String| ProxmoxApiRequest {
        method: Method::GET,
        path,
        body: None,
        write: false,
    };
    let post = |path: String, body: Option<serde_json::Value>| ProxmoxApiRequest {
        method: Method::POST,
        path,
        body,
        write: true,
    };

    match operation {
        VmOperation::Status => Ok(get(format!(
            "{}/status/current",
            vm_path(require_vmid(vmid)?)
        ))),
        VmOperation::Config => Ok(get(format!("{}/config", vm_path(require_vmid(vmid)?)))),
        VmOperation::Cluster => Ok(get("/cluster/status".to_string())),
        VmOperation::Nodes => Ok(get("/nodes".to_string())),
        VmOperation::NodeStatus => Ok(get(format!("/nodes/{node}/status"))),
        VmOperation::Storage => Ok(get("/storage".to_string())),
        VmOperation::Tasks => Ok(get("/cluster/tasks".to_string())),
        VmOperation::Start => Ok(post(
            format!("{}/status/start", vm_path(require_vmid(vmid)?)),
            None,
        )),
        VmOperation::Stop => Ok(post(
            format!("{}/status/stop", vm_path(require_vmid(vmid)?)),
            None,
        )),
        VmOperation::Shutdown => Ok(post(
            format!("{}/status/shutdown", vm_path(require_vmid(vmid)?)),
            None,
        )),
        VmOperation::Reboot => Ok(post(
            format!("{}/status/reboot", vm_path(require_vmid(vmid)?)),
            None,
        )),
        VmOperation::Reset => Ok(post(
            format!("{}/status/reset", vm_path(require_vmid(vmid)?)),
            None,
        )),
        VmOperation::Snapshot => {
            let snapname = snapname.ok_or_else(|| anyhow!("snapshot requires `snapname`"))?;
            Ok(post(
                format!("{}/snapshot", vm_path(require_vmid(vmid)?)),
                Some(json!({ "snapname": snapname })),
            ))
        }
        VmOperation::Rollback => {
            let snapname = snapname.ok_or_else(|| anyhow!("rollback requires `snapname`"))?;
            Ok(post(
                format!(
                    "{}/snapshot/{snapname}/rollback",
                    vm_path(require_vmid(vmid)?)
                ),
                None,
            ))
        }
        VmOperation::Migrate => {
            let target_node =
                migrate_target.ok_or_else(|| anyhow!("migrate requires `migrate_target`"))?;
            Ok(post(
                format!("{}/migrate", vm_path(require_vmid(vmid)?)),
                Some(json!({ "target": target_node })),
            ))
        }
        VmOperation::Shell => bail!("operation `shell` is SSH-only; set `transport: \"ssh\"`"),
    }
}

/// Assembles the `ssh` invocation that performs `operation` against `target`.
///
/// Reuses the container tool's SSH assembly path. `Shell` is the escape hatch
/// for anything the API does not model: QMP screendump, `qm monitor`, journal,
/// and host-level debugging, expressed as a raw node command in `args`.
pub fn assemble_ssh_command(
    target: &VmTarget,
    operation: VmOperation,
    vmid: Option<u32>,
    snapname: Option<&str>,
    migrate_target: Option<&str>,
    args: &[String],
) -> Result<AssembledCommand> {
    let ssh = target.ssh.as_ref().ok_or_else(|| {
        anyhow!(
            "target \"{}\" has no `ssh` transport configured",
            target.node
        )
    })?;

    let id = || require_vmid(vmid);
    let remote = match operation {
        VmOperation::Status => format!("qm status {}", id()?),
        VmOperation::Config => format!("qm config {}", id()?),
        VmOperation::Cluster => "pvecm status".to_string(),
        VmOperation::Start => format!("qm start {}", id()?),
        VmOperation::Stop => format!("qm stop {}", id()?),
        VmOperation::Shutdown => format!("qm shutdown {}", id()?),
        VmOperation::Reboot => format!("qm reboot {}", id()?),
        VmOperation::Reset => format!("qm reset {}", id()?),
        VmOperation::Snapshot => {
            let snapname = snapname.ok_or_else(|| anyhow!("snapshot requires `snapname`"))?;
            format!("qm snapshot {} {snapname}", id()?)
        }
        VmOperation::Rollback => {
            let snapname = snapname.ok_or_else(|| anyhow!("rollback requires `snapname`"))?;
            format!("qm rollback {} {snapname}", id()?)
        }
        VmOperation::Migrate => {
            let target_node =
                migrate_target.ok_or_else(|| anyhow!("migrate requires `migrate_target`"))?;
            format!("qm migrate {} {target_node}", id()?)
        }
        VmOperation::Shell => {
            if args.is_empty() {
                bail!("operation `shell` requires `args`");
            }
            args.join(" ")
        }
        VmOperation::Nodes
        | VmOperation::NodeStatus
        | VmOperation::Storage
        | VmOperation::Tasks => {
            bail!(
                "operation `{}` is API-only; use the `api` transport",
                operation.as_str()
            );
        }
    };

    Ok(assemble_ssh(
        &ssh.host,
        ssh.identity_file.as_deref(),
        remote,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vms::{ProxmoxApiConfig, ProxmoxSshConfig, VmInfo};

    fn target() -> VmTarget {
        VmTarget {
            driver: crate::vms::VmDriver::Proxmox,
            node: "compy".to_string(),
            vms: [(
                "100".to_string(),
                VmInfo {
                    name: "quetzalcoatlus".to_string(),
                    danger: true,
                    ips: Vec::new(),
                    os: None,
                    roles: Vec::new(),
                    probe_port: None,
                },
            )]
            .into_iter()
            .collect(),
            api: Some(ProxmoxApiConfig {
                base_url: "https://10.0.0.24:8006/api2/json".to_string(),
                token_id: "agent@pve!vm-read".to_string(),
                token_secret_url: "proxmox://compy/read-token".to_string(),
                write_token_id: Some("agent@pve!vm-write".to_string()),
                write_token_secret_url: Some("proxmox://compy/write-token".to_string()),
                tls_ca: None,
                tls_fingerprint: None,
            }),
            ssh: Some(ProxmoxSshConfig {
                host: "root@10.0.0.24".to_string(),
                identity_file: None,
            }),
            ips: Vec::new(),
            danger: false,
        }
    }

    #[test]
    fn status_assembles_get() {
        let request =
            assemble_api_request(&target(), VmOperation::Status, Some(100), None, None).unwrap();
        assert_eq!(request.method, Method::GET);
        assert_eq!(request.path, "/nodes/compy/qemu/100/status/current");
        assert!(!request.write);
    }

    #[test]
    fn mutating_operations_use_the_write_token() {
        for (operation, path) in [
            (VmOperation::Start, "/nodes/compy/qemu/100/status/start"),
            (VmOperation::Stop, "/nodes/compy/qemu/100/status/stop"),
            (
                VmOperation::Shutdown,
                "/nodes/compy/qemu/100/status/shutdown",
            ),
            (VmOperation::Reboot, "/nodes/compy/qemu/100/status/reboot"),
            (VmOperation::Reset, "/nodes/compy/qemu/100/status/reset"),
        ] {
            let request =
                assemble_api_request(&target(), operation, Some(100), None, None).unwrap();
            assert_eq!(request.method, Method::POST);
            assert_eq!(request.path, path);
            assert!(request.write, "{operation:?} must use the write token");
        }
    }

    #[test]
    fn snapshot_rollback_and_migrate_emit_bodies() {
        let snapshot = assemble_api_request(
            &target(),
            VmOperation::Snapshot,
            Some(100),
            Some("pre"),
            None,
        )
        .unwrap();
        assert_eq!(snapshot.path, "/nodes/compy/qemu/100/snapshot");
        assert_eq!(snapshot.body, Some(json!({ "snapname": "pre" })));

        let rollback = assemble_api_request(
            &target(),
            VmOperation::Rollback,
            Some(100),
            Some("pre"),
            None,
        )
        .unwrap();
        assert_eq!(rollback.path, "/nodes/compy/qemu/100/snapshot/pre/rollback");
        assert!(rollback.body.is_none());

        let migrate = assemble_api_request(
            &target(),
            VmOperation::Migrate,
            Some(100),
            None,
            Some("spino"),
        )
        .unwrap();
        assert_eq!(migrate.path, "/nodes/compy/qemu/100/migrate");
        assert_eq!(migrate.body, Some(json!({ "target": "spino" })));
    }

    #[test]
    fn read_endpoints_are_node_agnostic_where_appropriate() {
        let request =
            assemble_api_request(&target(), VmOperation::Nodes, None, None, None).unwrap();
        assert_eq!(request.path, "/nodes");
        let request =
            assemble_api_request(&target(), VmOperation::NodeStatus, None, None, None).unwrap();
        assert_eq!(request.path, "/nodes/compy/status");
    }

    #[test]
    fn shell_is_rejected_by_the_api_transport() {
        let result =
            assemble_api_request(&target(), VmOperation::Shell, None, None, None).unwrap_err();
        assert!(result.to_string().contains("SSH-only"));
    }

    #[test]
    fn ssh_status_assembles_qm_command() {
        let command =
            assemble_ssh_command(&target(), VmOperation::Status, Some(100), None, None, &[])
                .unwrap();
        assert_eq!(command.program, "ssh");
        assert_eq!(
            command.args.last().map(String::as_str),
            Some("qm status 100")
        );
    }

    #[test]
    fn ssh_shell_joins_args_into_a_raw_command() {
        let command = assemble_ssh_command(
            &target(),
            VmOperation::Shell,
            None,
            None,
            None,
            &["qm".to_string(), "monitor".to_string(), "100".to_string()],
        )
        .unwrap();
        assert_eq!(
            command.args.last().map(String::as_str),
            Some("qm monitor 100")
        );
    }

    #[test]
    fn ssh_rejects_api_only_reads() {
        let result =
            assemble_ssh_command(&target(), VmOperation::Nodes, None, None, None, &[]).unwrap_err();
        assert!(result.to_string().contains("API-only"));
    }
}
