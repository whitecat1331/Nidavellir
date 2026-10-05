use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How a target is operated: a Docker Compose stack or a systemd unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerDriver {
    Compose,
    Systemd,
}

/// One reachable ops target: an SSH endpoint plus the driver that operates it.
///
/// This is the agnostic schema — the tool knows only the shape, never the hosts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerTarget {
    /// SSH endpoint — `user@host`, an `~/.ssh/config` alias, or a bare host.
    pub host: String,
    /// Optional SSH identity file. A leading `~/` is expanded to the home dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    /// How to operate this target.
    pub driver: ContainerDriver,
    /// Compose: the project directory on the remote host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Compose: the compose file, relative to `dir`. Defaults to the driver default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Systemd: the unit name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// The set of configured targets, persisted as a single JSON file.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerStore {
    #[serde(default)]
    pub targets: BTreeMap<String, ContainerTarget>,
}

impl ContainerStore {
    /// The default on-disk location, mirroring `automations.json`.
    pub fn default_path() -> PathBuf {
        paths::data_dir().join("containers.json")
    }

    /// Loads targets from `path`. A missing file yields an empty store.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parsing container config {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error)
                .with_context(|| format!("reading container config {}", path.display())),
        }
    }

    /// The target registered under `id`, if any.
    pub fn target(&self, id: &str) -> Option<&ContainerTarget> {
        self.targets.get(id)
    }
}

/// A remote ops operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContainerOperation {
    /// List containers (compose).
    Ps,
    /// Show logs (compose).
    Logs,
    /// List images (compose).
    Images,
    /// Live container stats (compose).
    Stats,
    /// Show a unit's status (systemd).
    Status,
    /// Exit 0 when the unit is active (systemd).
    IsActive,
    /// Pull images (compose).
    Pull,
    /// Create and start services (compose).
    Up,
    /// Stop and remove services (compose).
    Down,
    /// Restart a service or unit.
    Restart,
    /// Run a command in a service container (compose).
    Exec,
    /// Remove stopped containers (compose).
    Rm,
    /// Run a one-off command (compose).
    Run,
}

impl ContainerOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ps => "ps",
            Self::Logs => "logs",
            Self::Images => "images",
            Self::Stats => "stats",
            Self::Status => "status",
            Self::IsActive => "is-active",
            Self::Pull => "pull",
            Self::Up => "up",
            Self::Down => "down",
            Self::Restart => "restart",
            Self::Exec => "exec",
            Self::Rm => "rm",
            Self::Run => "run",
        }
    }

    /// Whether the operation mutates remote state and therefore needs confirmation.
    pub fn is_mutating(self) -> bool {
        matches!(
            self,
            Self::Pull
                | Self::Up
                | Self::Down
                | Self::Restart
                | Self::Exec
                | Self::Rm
                | Self::Run
        )
    }
}

/// A fully-assembled command to execute locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl AssembledCommand {
    /// A human-readable rendering for logs and errors.
    pub fn display(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.iter().map(|arg| shell_quote(arg)));
        parts.join(" ")
    }
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_./:@=".contains(character))
    {
        arg.to_string()
    } else {
        format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Assembles the `ssh` invocation that performs `operation` against `target`.
///
/// The returned command is argv-only (no shell): `ssh` plus flags, the host, and
/// the remote command as a single argument that `ssh` forwards to the remote
/// shell verbatim.
pub fn assemble_command(
    target: &ContainerTarget,
    operation: ContainerOperation,
    service: Option<&str>,
    args: &[String],
) -> Result<AssembledCommand> {
    let remote = match target.driver {
        ContainerDriver::Compose => {
            let dir = target
                .dir
                .as_deref()
                .ok_or_else(|| anyhow!("compose target is missing `dir`"))?;
            let mut remote = format!("cd {dir} && docker compose");
            if let Some(file) = target.file.as_deref() {
                remote.push_str(&format!(" -f {file}"));
            }
            remote.push(' ');
            remote.push_str(operation.as_str());
            if let Some(service) = service {
                remote.push(' ');
                remote.push_str(service);
            }
            for arg in args {
                remote.push(' ');
                remote.push_str(arg);
            }
            remote
        }
        ContainerDriver::Systemd => {
            let unit = target
                .service
                .as_deref()
                .ok_or_else(|| anyhow!("systemd target is missing `service`"))?;
            if !matches!(
                operation,
                ContainerOperation::Status | ContainerOperation::IsActive | ContainerOperation::Restart
            ) {
                bail!(
                    "operation `{}` is not valid for a systemd target",
                    operation.as_str()
                );
            }
            let mut remote = format!("systemctl {} {unit}", operation.as_str());
            for arg in args {
                remote.push(' ');
                remote.push_str(arg);
            }
            remote
        }
    };

    let mut ssh_args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=15".to_string(),
    ];
    if let Some(identity) = target.identity_file.as_deref() {
        ssh_args.push("-i".to_string());
        ssh_args.push(expand_home(identity));
    }
    ssh_args.push(target.host.clone());
    ssh_args.push(remote);

    Ok(AssembledCommand {
        program: "ssh".to_string(),
        args: ssh_args,
    })
}

fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        paths::home_dir()
            .join(rest)
            .to_string_lossy()
            .into_owned()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compose_target(dir: &str) -> ContainerTarget {
        ContainerTarget {
            host: "deploy@example.com".to_string(),
            identity_file: None,
            driver: ContainerDriver::Compose,
            dir: Some(dir.to_string()),
            file: None,
            service: None,
        }
    }

    fn systemd_target(host: &str, service: &str) -> ContainerTarget {
        ContainerTarget {
            host: host.to_string(),
            identity_file: None,
            driver: ContainerDriver::Systemd,
            dir: None,
            file: None,
            service: Some(service.to_string()),
        }
    }

    #[test]
    fn operation_mutating_classification() {
        for read_only in [
            ContainerOperation::Ps,
            ContainerOperation::Logs,
            ContainerOperation::Images,
            ContainerOperation::Stats,
            ContainerOperation::Status,
            ContainerOperation::IsActive,
        ] {
            assert!(!read_only.is_mutating(), "{read_only:?} should be read-only");
        }
        for mutating in [
            ContainerOperation::Pull,
            ContainerOperation::Up,
            ContainerOperation::Down,
            ContainerOperation::Restart,
            ContainerOperation::Exec,
            ContainerOperation::Rm,
            ContainerOperation::Run,
        ] {
            assert!(mutating.is_mutating(), "{mutating:?} should be mutating");
        }
    }

    #[test]
    fn compose_ps_assembles_ssh_invocation() {
        let command =
            assemble_command(&compose_target("/opt/app"), ContainerOperation::Ps, None, &[])
                .unwrap();

        assert_eq!(command.program, "ssh");
        assert_eq!(
            command.args,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "deploy@example.com",
                "cd /opt/app && docker compose ps",
            ]
        );
    }

    #[test]
    fn compose_exec_assembles_service_and_args() {
        let mut target = compose_target("/opt/app");
        target.file = Some("deploy/docker-compose.yml".to_string());
        let command = assemble_command(
            &target,
            ContainerOperation::Exec,
            Some("gallery"),
            &["bash".to_string(), "-c".to_string(), "ls".to_string()],
        )
        .unwrap();

        assert_eq!(
            command.args.last().map(String::as_str),
            Some("cd /opt/app && docker compose -f deploy/docker-compose.yml exec gallery bash -c ls")
        );
    }

    #[test]
    fn systemd_status_assembles_systemctl() {
        let command = assemble_command(
            &systemd_target("postgres.internal", "postgresql"),
            ContainerOperation::Status,
            None,
            &[],
        )
        .unwrap();

        assert_eq!(
            command.args.last().map(String::as_str),
            Some("systemctl status postgresql")
        );
    }

    #[test]
    fn systemd_rejects_compose_only_operation() {
        let result = assemble_command(
            &systemd_target("postgres.internal", "postgresql"),
            ContainerOperation::Ps,
            None,
            &[],
        );
        assert!(result.is_err());
    }

    #[test]
    fn identity_file_is_appended() {
        let mut target = compose_target("/opt/app");
        target.identity_file = Some("/abs/key".to_string());
        let command =
            assemble_command(&target, ContainerOperation::Ps, None, &[]).unwrap();

        assert_eq!(
            &command.args[..6],
            &[
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "-i",
                "/abs/key",
            ]
        );
    }

    #[test]
    fn store_loads_targets_from_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("containers.json");

        std::fs::write(
            &path,
            r#"{
                "targets": {
                    "ims": {
                        "host": "deploy@example.com",
                        "driver": "compose",
                        "dir": "/opt/access_replacement"
                    }
                }
            }"#,
        )
        .unwrap();

        let store = ContainerStore::load(&path).unwrap();
        assert_eq!(
            store.target("ims"),
            Some(&compose_target("/opt/access_replacement"))
        );
        assert_eq!(store.target("missing"), None);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = ContainerStore::load(dir.path().join("missing.json")).unwrap();
        assert!(store.targets.is_empty());
    }
}
