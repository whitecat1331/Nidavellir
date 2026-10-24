use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a target is operated. The first — and currently only — driver is
/// Proxmox; a future `libvirt`/`esxi` driver slots in as a new variant plus its
/// own assembly module, with no change to this target schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VmDriver {
    Proxmox,
}

/// How a request reaches the hypervisor. `api` (default) talks to the Proxmox
/// REST API with a scoped API token; `ssh` is an explicit escalation to a root
/// shell on the node, for the surface the API does not model and for
/// out-of-band recovery. There is never a silent `api` → `ssh` failover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VmTransport {
    Api,
    Ssh,
}

/// Proxmox REST API transport. Secret material (token secrets) is never stored
/// here — only the non-secret token id and the keychain URL the secret lives at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxmoxApiConfig {
    /// Base URL, e.g. `https://10.0.0.12:8006/api2/json`.
    pub base_url: String,
    /// Read-only token id (`<user>!<token>`), broad across the probed nodes.
    pub token_id: String,
    /// Keychain URL under which the read token secret is stored.
    pub token_secret_url: String,
    /// Narrowly-scoped write token id (`<user>!<token>`), granted only the
    /// operations the agent may actually perform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_token_id: Option<String>,
    /// Keychain URL under which the write token secret is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_token_secret_url: Option<String>,
    /// Path to the cluster CA bundle used to pin the self-signed node certs.
    /// Verification is never disabled; when unset the standard TLS verifier
    /// still applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca: Option<String>,
    /// SHA-256 fingerprint of the node cert (alternative to `tls_ca`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_fingerprint: Option<String>,
}

/// Proxmox SSH transport (root shell escalation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxmoxSshConfig {
    /// SSH endpoint — `user@host`, an `~/.ssh/config` alias, or a bare host.
    pub host: String,
    /// Optional SSH identity file. A leading `~/` is expanded to the home dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
}

/// A single VM registered on a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmInfo {
    /// Human-readable VM name (e.g. `quetzalcoatlus`).
    pub name: String,
    /// Whether this VM is infra-critical (danger-tier for stop/reboot).
    #[serde(default)]
    pub danger: bool,
    /// LAN / Tailscale / etc. IPs, when known (enriched registry, D6).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ips: Vec<String>,
    /// Operating system, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Human-readable role(s), e.g. `["build"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Override for the reachability probe port (default 22).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_port: Option<u16>,
}

/// One reachable ops target: a hypervisor node plus the VMs it hosts.
///
/// This is the agnostic schema — the tool knows only the shape, never the hosts,
/// node names, VMIDs, or tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmTarget {
    /// How to operate this target.
    pub driver: VmDriver,
    /// The hypervisor node name within the cluster.
    pub node: String,
    /// Registered VMs on this node, keyed by VMID (JSON object keys are strings).
    #[serde(default)]
    pub vms: BTreeMap<String, VmInfo>,
    /// The node's own IPs (management + any other reachable address), when
    /// known. Falls back to the API/SSH transport host when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ips: Vec<String>,
    /// Proxmox API transport (default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ProxmoxApiConfig>,
    /// Proxmox SSH transport (explicit escalation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<ProxmoxSshConfig>,
    /// Whether this node is itself infra-critical (danger-tier).
    #[serde(default)]
    pub danger: bool,
}

/// The set of configured targets, persisted as a single JSON file.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmStore {
    #[serde(default)]
    pub targets: BTreeMap<String, VmTarget>,
}

impl VmStore {
    /// The default on-disk location, mirroring `containers.json` and
    /// `automations.json`.
    pub fn default_path() -> PathBuf {
        paths::data_dir().join("vms.json")
    }

    /// Loads targets from `path`. A missing file yields an empty store.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parsing vm config {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("reading vm config {}", path.display()))
            }
        }
    }

    /// The target registered under `id`, if any.
    pub fn target(&self, id: &str) -> Option<&VmTarget> {
        self.targets.get(id)
    }

    /// Persists the store to `path` (used by the in-app settings manager).
    pub fn save(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        let path = path.as_ref();
        let contents = serde_json::to_string_pretty(self).context("serializing vm store")?;
        std::fs::write(path, contents)
            .with_context(|| format!("writing vm store {}", path.display()))
    }
}

/// A hypervisor ops operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VmOperation {
    /// VM power/config status (`GET .../status/current`). Requires `vmid`.
    Status,
    /// VM configuration. Requires `vmid`.
    Config,
    /// Cluster status (API: `GET /cluster/status`; SSH: `pvecm status`).
    Cluster,
    /// Node list (API: `GET /nodes`).
    Nodes,
    /// Node status (API: `GET /nodes/{node}/status`).
    NodeStatus,
    /// Storage list/status (API: `GET /storage`).
    Storage,
    /// Recent cluster tasks (API: `GET /cluster/tasks`).
    Tasks,
    /// Start a stopped VM. Mutating.
    Start,
    /// Stop a VM (hard). Mutating; danger-tier on infra-critical targets.
    Stop,
    /// Graceful ACPI shutdown. Mutating.
    Shutdown,
    /// Reboot. Mutating; danger-tier on infra-critical targets.
    Reboot,
    /// Hard reset. Mutating.
    Reset,
    /// Snapshot a VM. Mutating; requires `snapname`.
    Snapshot,
    /// Roll back to a snapshot. Mutating; requires `snapname`.
    Rollback,
    /// Migrate a VM to another node. Mutating; requires `migrate_target`.
    Migrate,
    /// Raw node command (QMP screendump, `qm monitor`, journal, host debugging).
    /// SSH-only; requires `args`.
    Shell,
}

impl VmOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Config => "config",
            Self::Cluster => "cluster",
            Self::Nodes => "nodes",
            Self::NodeStatus => "node_status",
            Self::Storage => "storage",
            Self::Tasks => "tasks",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Shutdown => "shutdown",
            Self::Reboot => "reboot",
            Self::Reset => "reset",
            Self::Snapshot => "snapshot",
            Self::Rollback => "rollback",
            Self::Migrate => "migrate",
            Self::Shell => "shell",
        }
    }

    /// Whether the operation mutates hypervisor state and therefore needs
    /// confirmation (and, for the API transport, the scoped write token).
    pub fn is_mutating(self) -> bool {
        matches!(
            self,
            Self::Start
                | Self::Stop
                | Self::Shutdown
                | Self::Reboot
                | Self::Reset
                | Self::Snapshot
                | Self::Rollback
                | Self::Migrate
        )
    }

    /// Whether the operation can only be performed over the SSH transport.
    pub fn is_ssh_only(self) -> bool {
        matches!(self, Self::Shell)
    }

    /// Whether the operation addresses a specific VM (and thus needs `vmid`).
    pub fn requires_vmid(self) -> bool {
        matches!(
            self,
            Self::Status
                | Self::Config
                | Self::Start
                | Self::Stop
                | Self::Shutdown
                | Self::Reboot
                | Self::Reset
                | Self::Snapshot
                | Self::Rollback
                | Self::Migrate
        )
    }
}

/// How strongly an operation is gated before it may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatingTier {
    /// Read-only; always allowed.
    Read,
    /// Mutating; requires `confirm: true`.
    Confirm,
    /// Stop/reboot of an infra-critical VM or node; requires `confirm: true`
    /// plus a non-empty `reason`.
    Danger,
    /// Any operation on the SSH transport (a root shell, no token scoping
    /// underneath); always requires `confirm: true`.
    Ssh,
}

/// Classifies an operation into a gating tier, given the transport and target.
///
/// The SSH transport is always its own tier (root shell). Read-only API ops are
/// always allowed; mutating ops are `confirm`; and stop/reboot of an
/// infra-critical VM or node is `danger`.
pub fn gating_tier(
    operation: VmOperation,
    transport: VmTransport,
    target: &VmTarget,
    vmid: Option<u32>,
) -> GatingTier {
    if transport == VmTransport::Ssh || operation.is_ssh_only() {
        return GatingTier::Ssh;
    }
    if !operation.is_mutating() {
        return GatingTier::Read;
    }
    if is_danger_operation(operation) && is_danger_target(target, vmid) {
        return GatingTier::Danger;
    }
    GatingTier::Confirm
}

fn is_danger_operation(operation: VmOperation) -> bool {
    matches!(operation, VmOperation::Stop | VmOperation::Reboot)
}

fn is_danger_target(target: &VmTarget, vmid: Option<u32>) -> bool {
    target.danger
        || vmid
            .and_then(|id| target.vms.get(&id.to_string()))
            .is_some_and(|info| info.danger)
}

/// Resolves `vmid` against the target's registry. A VMID that is not registered
/// on the node is rejected, so the tool can only ever address seeded VMIDs.
pub fn resolve_vmid(target: &VmTarget, vmid: Option<u32>) -> Result<Option<u32>> {
    match vmid {
        None => Ok(None),
        Some(id) => {
            if target.vms.contains_key(&id.to_string()) {
                Ok(Some(id))
            } else {
                bail!(
                    "VM {id} is not registered on node \"{}\"; registered VMIDs: {}",
                    target.node,
                    target.vms.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            }
        }
    }
}

/// Validates that the operation's required arguments are present.
pub fn validate_input(
    operation: VmOperation,
    vmid: Option<u32>,
    snapname: Option<&str>,
    migrate_target: Option<&str>,
    args: &[String],
) -> Result<()> {
    if operation.requires_vmid() && vmid.is_none() {
        bail!("operation `{}` requires a `vmid`", operation.as_str());
    }
    if matches!(operation, VmOperation::Snapshot | VmOperation::Rollback) && snapname.is_none() {
        bail!("operation `{}` requires a `snapname`", operation.as_str());
    }
    if operation == VmOperation::Migrate && migrate_target.is_none() {
        bail!("operation `migrate` requires a `migrate_target` node");
    }
    if operation == VmOperation::Shell && args.is_empty() {
        bail!("operation `shell` requires `args` (the node command to run)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(danger: bool, vms: &[(&str, bool)]) -> VmTarget {
        VmTarget {
            driver: VmDriver::Proxmox,
            node: "compy".to_string(),
            vms: vms
                .iter()
                .map(|(id, danger)| {
                    (
                        id.to_string(),
                        VmInfo {
                            name: format!("vm-{id}"),
                            danger: *danger,
                            ips: Vec::new(),
                            os: None,
                            roles: Vec::new(),
                            probe_port: None,
                        },
                    )
                })
                .collect(),
            api: None,
            ssh: None,
            ips: Vec::new(),
            danger,
        }
    }

    #[test]
    fn operation_classification() {
        for read_only in [
            VmOperation::Status,
            VmOperation::Config,
            VmOperation::Cluster,
            VmOperation::Nodes,
            VmOperation::NodeStatus,
            VmOperation::Storage,
            VmOperation::Tasks,
        ] {
            assert!(
                !read_only.is_mutating(),
                "{read_only:?} should be read-only"
            );
        }
        for mutating in [
            VmOperation::Start,
            VmOperation::Stop,
            VmOperation::Shutdown,
            VmOperation::Reboot,
            VmOperation::Reset,
            VmOperation::Snapshot,
            VmOperation::Rollback,
            VmOperation::Migrate,
        ] {
            assert!(mutating.is_mutating(), "{mutating:?} should be mutating");
        }
        assert!(VmOperation::Shell.is_ssh_only());
        assert!(!VmOperation::Shell.is_mutating());
    }

    #[test]
    fn read_operations_are_always_allowed() {
        let target = target(false, &[]);
        assert_eq!(
            gating_tier(VmOperation::Status, VmTransport::Api, &target, Some(100)),
            GatingTier::Read
        );
        assert_eq!(
            gating_tier(VmOperation::Cluster, VmTransport::Api, &target, None),
            GatingTier::Read
        );
    }

    #[test]
    fn mutating_operations_require_confirmation() {
        let target = target(false, &[("100", false)]);
        assert_eq!(
            gating_tier(VmOperation::Start, VmTransport::Api, &target, Some(100)),
            GatingTier::Confirm
        );
        assert_eq!(
            gating_tier(VmOperation::Snapshot, VmTransport::Api, &target, Some(100)),
            GatingTier::Confirm
        );
    }

    #[test]
    fn stop_and_reboot_of_danger_targets_are_danger() {
        let vm_danger = target(false, &[("100", true)]);
        for operation in [VmOperation::Stop, VmOperation::Reboot] {
            assert_eq!(
                gating_tier(operation, VmTransport::Api, &vm_danger, Some(100)),
                GatingTier::Danger
            );
        }
        // A non-danger VM is still just `confirm`.
        assert_eq!(
            gating_tier(VmOperation::Stop, VmTransport::Api, &vm_danger, Some(104)),
            GatingTier::Confirm
        );

        let node_danger = target(true, &[("100", false)]);
        assert_eq!(
            gating_tier(VmOperation::Stop, VmTransport::Api, &node_danger, Some(100)),
            GatingTier::Danger
        );
    }

    #[test]
    fn shutdown_and_reset_are_confirm_even_on_danger_targets() {
        let target = target(false, &[("100", true)]);
        for operation in [VmOperation::Shutdown, VmOperation::Reset] {
            assert_eq!(
                gating_tier(operation, VmTransport::Api, &target, Some(100)),
                GatingTier::Confirm
            );
        }
    }

    #[test]
    fn ssh_transport_is_always_dangerously_gated() {
        let target = target(false, &[("100", false)]);
        assert_eq!(
            gating_tier(VmOperation::Status, VmTransport::Ssh, &target, Some(100)),
            GatingTier::Ssh
        );
        // SSH-only ops are their own tier even when the transport default is api.
        assert_eq!(
            gating_tier(VmOperation::Shell, VmTransport::Api, &target, None),
            GatingTier::Ssh
        );
    }

    #[test]
    fn resolve_vmid_rejects_unregistered_vmids() {
        let target = target(false, &[("100", false)]);
        assert_eq!(resolve_vmid(&target, Some(100)).unwrap(), Some(100));
        assert_eq!(resolve_vmid(&target, None).unwrap(), None);
        assert!(resolve_vmid(&target, Some(104)).is_err());
    }

    #[test]
    fn validate_input_checks_required_arguments() {
        assert!(validate_input(VmOperation::Status, None, None, None, &[]).is_err());
        assert!(validate_input(VmOperation::Status, Some(100), None, None, &[]).is_ok());
        assert!(validate_input(VmOperation::Snapshot, Some(100), None, None, &[]).is_err());
        assert!(validate_input(VmOperation::Snapshot, Some(100), Some("pre"), None, &[]).is_ok());
        assert!(validate_input(VmOperation::Migrate, Some(100), None, None, &[]).is_err());
        assert!(validate_input(VmOperation::Migrate, Some(100), None, Some("spino"), &[]).is_ok());
        assert!(validate_input(VmOperation::Shell, None, None, None, &[]).is_err());
        assert!(
            validate_input(
                VmOperation::Shell,
                None,
                None,
                None,
                &["qm status 100".into()]
            )
            .is_ok()
        );
    }

    #[test]
    fn store_loads_targets_from_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vms.json");

        std::fs::write(
            &path,
            r#"{
                "targets": {
                    "compy": {
                        "driver": "proxmox",
                        "node": "compy",
                        "vms": { "100": { "name": "quetzalcoatlus", "danger": true } }
                    }
                }
            }"#,
        )
        .unwrap();

        let store = VmStore::load(&path).unwrap();
        let target = store.target("compy").unwrap();
        assert_eq!(target.node, "compy");
        assert_eq!(target.vms.get("100").unwrap().name, "quetzalcoatlus");
        assert!(target.vms.get("100").unwrap().danger);
        assert_eq!(store.target("missing"), None);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::load(dir.path().join("missing.json")).unwrap();
        assert!(store.targets.is_empty());
    }
}
