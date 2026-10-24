//! Panel-facing DevOps configuration and reachability aggregation.
//!
//! `devops.json` holds the panel's persistent settings (the three poll
//! intervals and the live-toggle default); [`PingStatuses`] is the cheap
//! TCP-connect reachability sweep, kept separate from the heavier Proxmox/SSH
//! report sweeps so the panel can run them on independent cadences.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use gpui::AsyncApp;
use serde::{Deserialize, Serialize};

use crate::containers::ContainerStore;
use crate::reachability::{HOST_PROBE_PORT, NODE_PROBE_PORT, Reachability, bare_host, probe_tcp};
use crate::vms::{VmStore, VmTarget};

/// Persistent DevOps panel settings, stored as `devops.json` in the data dir.
///
/// Every field carries a serde default so a missing or partial file loads clean;
/// nothing is hardcoded at the call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevOpsConfig {
    /// Whether the live (continuous) poll mode is on.
    #[serde(default = "default_live")]
    pub live: bool,
    /// Normal-mode reachability ping interval, in seconds.
    #[serde(default = "default_normal_interval")]
    pub ping_interval_secs: u64,
    /// Normal-mode Proxmox report interval, in seconds.
    #[serde(default = "default_normal_interval")]
    pub proxmox_interval_secs: u64,
    /// Normal-mode container report interval, in seconds.
    #[serde(default = "default_normal_interval")]
    pub container_interval_secs: u64,
    /// Live-mode reachability ping interval, in seconds.
    #[serde(default = "default_live_ping_interval")]
    pub live_ping_interval_secs: u64,
    /// Live-mode Proxmox report interval, in seconds.
    #[serde(default = "default_live_report_interval")]
    pub live_proxmox_interval_secs: u64,
    /// Live-mode container report interval, in seconds.
    #[serde(default = "default_live_report_interval")]
    pub live_container_interval_secs: u64,
}

fn default_live() -> bool {
    false
}

fn default_normal_interval() -> u64 {
    60
}

fn default_live_ping_interval() -> u64 {
    1
}

fn default_live_report_interval() -> u64 {
    5
}

impl Default for DevOpsConfig {
    fn default() -> Self {
        Self {
            live: false,
            ping_interval_secs: 60,
            proxmox_interval_secs: 60,
            container_interval_secs: 60,
            live_ping_interval_secs: 1,
            live_proxmox_interval_secs: 5,
            live_container_interval_secs: 5,
        }
    }
}

impl DevOpsConfig {
    /// The default on-disk location, mirroring `vms.json`/`containers.json`.
    pub fn default_path() -> PathBuf {
        paths::data_dir().join("devops.json")
    }

    /// Loads the config from `path`. A missing file yields the defaults.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parsing devops config {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("reading devops config {}", path.display()))
            }
        }
    }

    /// Persists the config to `path`.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let contents =
            serde_json::to_string_pretty(self).with_context(|| "serializing devops config")?;
        std::fs::write(path, contents)
            .with_context(|| format!("writing devops config {}", path.display()))
    }
}

/// Reachability of every registered target, keyed for the panel to attach to its
/// report rows. `Reachability::NotApplicable` means there is no probeable
/// endpoint for that target (e.g. a VM with no IP yet).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PingStatuses {
    /// Node id → reachability of its management endpoint.
    pub nodes: BTreeMap<String, Reachability>,
    /// Container target id → reachability of its SSH host.
    pub hosts: BTreeMap<String, Reachability>,
    /// `(node, vmid)` → reachability of the VM's IP (when known).
    pub vms: BTreeMap<(String, String), Reachability>,
}

/// Probes every node, container host, and VM reachability in one cheap sweep.
/// This is the ping half of the panel; the heavier Proxmox/SSH report sweeps run
/// separately on their own intervals.
pub async fn ping_statuses(cx: &AsyncApp) -> PingStatuses {
    let mut pings = PingStatuses::default();

    if let Ok(store) = VmStore::load(VmStore::default_path()) {
        for (node, target) in &store.targets {
            let node_ping = match node_probe_endpoint(target) {
                Some((host, port)) => probe_tcp(cx.background_executor(), &host, port).await,
                None => Reachability::NotApplicable,
            };
            pings.nodes.insert(node.clone(), node_ping);

            for (vmid, info) in &target.vms {
                let port = info.probe_port.unwrap_or(HOST_PROBE_PORT);
                let ping = best_reachability(cx, &info.ips, port).await;
                pings.vms.insert((node.clone(), vmid.clone()), ping);
            }
        }
    }

    if let Ok(store) = ContainerStore::load(ContainerStore::default_path()) {
        for (target_id, target) in &store.targets {
            let ping = probe_tcp(
                cx.background_executor(),
                bare_host(&target.host),
                target.probe_port.unwrap_or(HOST_PROBE_PORT),
            )
            .await;
            pings.hosts.insert(target_id.clone(), ping);
        }
    }

    pings
}

/// Derives a node's probeable management endpoint: the Proxmox API host/port
/// when an `api` transport is configured, otherwise the SSH host on port 22.
fn node_probe_endpoint(target: &VmTarget) -> Option<(String, u16)> {
    if let Some(api) = target.api.as_ref() {
        if let Ok(url) = url::Url::parse(&api.base_url) {
            let host = url.host_str()?.to_string();
            let port = url.port_or_known_default().unwrap_or(NODE_PROBE_PORT);
            return Some((host, port));
        }
    }
    if let Some(ssh) = target.ssh.as_ref() {
        let host = bare_host(&ssh.host);
        if !host.is_empty() {
            return Some((host.to_string(), HOST_PROBE_PORT));
        }
    }
    None
}

/// Derives a node's management IP(s) from its transport config (Proxmox API
/// host + SSH host), deduplicated. Nodes carry no separate `ips` registry field;
/// their address lives in `api.base_url` / `ssh.host`, so this surfaces it for
/// the panel without a schema change.
pub fn node_ips(target: &VmTarget) -> Vec<String> {
    let mut ips = Vec::new();
    if let Some(api) = target.api.as_ref() {
        if let Ok(url) = url::Url::parse(&api.base_url) {
            if let Some(host) = url.host_str() {
                ips.push(host.to_string());
            }
        }
    }
    if let Some(ssh) = target.ssh.as_ref() {
        let host = bare_host(&ssh.host);
        if !host.is_empty() && !ips.iter().any(|ip| ip == host) {
            ips.push(host.to_string());
        }
    }
    ips
}

/// Probes every IP in `ips` and returns the best reachability: the lowest
/// latency if any IP is reachable, `Timeout` if none answer, or
/// `NotApplicable` when there are no IPs to probe.
async fn best_reachability(cx: &AsyncApp, ips: &[String], port: u16) -> Reachability {
    let mut best = None;
    for ip in ips {
        let ip = ip.trim();
        if ip.is_empty() {
            continue;
        }
        let probe = probe_tcp(cx.background_executor(), ip, port).await;
        best = Some(match best {
            Some(Reachability::Reachable(existing)) => match probe {
                Reachability::Reachable(latency) => {
                    Reachability::Reachable(existing.min(latency))
                }
                _ => Reachability::Reachable(existing),
            },
            _ => probe,
        });
    }
    best.unwrap_or(Reachability::NotApplicable)
}
