use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Output,
};

use semver::{Version, VersionReq};
use serde::{Deserialize, de::DeserializeOwned};
use util::command::new_command;

/// `sbx` is a closed-source CLI whose interface has already changed once
/// (`docker sandbox` → `sbx`), so Sands only drives versions it was tested against.
pub const SUPPORTED_VERSIONS: &str = ">=0.50.0, <0.51.0";

#[derive(Debug, thiserror::Error)]
pub enum SbxError {
    #[error("Docker Sandboxes CLI (`sbx`) is not installed or not on PATH")]
    NotInstalled,
    #[error("sbx {found} is not supported, Sands requires sbx {required}")]
    UnsupportedVersion { found: String, required: String },
    #[error("the Docker Sandboxes daemon is not running: {message}")]
    DaemonNotRunning { message: String },
    #[error("not signed in to Docker, run `sbx login`")]
    NotLoggedIn,
    #[error("sandbox '{name}' not found")]
    SandboxNotFound { name: String },
    #[error("`{command}` failed: {message}")]
    CommandFailed { command: String, message: String },
    #[error("failed to run `{command}`")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("unexpected output from `{command}`")]
    InvalidOutput {
        command: String,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct VersionInfo {
    pub client: ClientVersion,
    #[serde(default)]
    pub server: Option<ServerVersion>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ClientVersion {
    pub version: String,
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ServerVersion {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SandboxInfo {
    pub name: String,
    pub id: String,
    pub agent: String,
    pub status: SandboxStatus,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SandboxStatus {
    Running,
    Stopped,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PublishedPort {
    #[serde(default)]
    pub host_ip: Option<String>,
    pub host_port: u16,
    pub sandbox_port: u16,
    #[serde(default)]
    pub protocol: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct LifecycleResult {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Hosts a sandbox reached through the proxy, aggregated by host.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct NetworkLog {
    #[serde(default)]
    pub allowed: Vec<NetworkActivity>,
    #[serde(default)]
    pub blocked: Vec<NetworkActivity>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct NetworkActivity {
    pub host: String,
    #[serde(default)]
    pub count: u64,
    #[serde(default)]
    pub first_seen: Option<String>,
    #[serde(default)]
    pub last_seen: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct PolicyRules {
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct PolicyRule {
    pub id: String,
    pub decision: String,
    pub resource_type: String,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// Resource limits a sandbox was created with. `None` means the `sbx` default
/// (all host CPUs, half of the host memory).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfiguredResources {
    pub cpus: Option<u32>,
    pub memory: Option<String>,
    pub docker_disk: Option<String>,
}

/// What a sandbox looks like from the inside, collected with a single `exec`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxProbe {
    pub cpus: Option<u32>,
    pub memory_total_bytes: Option<u64>,
    pub memory_available_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    pub disk_used_bytes: Option<u64>,
    pub load_average: Option<String>,
    /// TCP ports something listens on inside the sandbox, sorted and deduplicated.
    pub listening_ports: Vec<u16>,
    pub repositories: Vec<SandboxRepository>,
    /// Agent CLIs found on the sandbox `PATH`.
    pub agents: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxRepository {
    pub path: String,
    pub branch: Option<String>,
    pub remote: Option<String>,
}

/// Agent CLIs Sands looks for inside sandboxes.
pub const KNOWN_AGENT_COMMANDS: [&str; 7] = [
    "claude",
    "codex",
    "gemini",
    "opencode",
    "copilot",
    "cursor-agent",
    "kiro",
];

const PROBE_SCRIPT: &str = r#"
echo "cpus=$(nproc)"
awk '/^MemTotal:/ {print "memory_total_kb=" $2} /^MemAvailable:/ {print "memory_available_kb=" $2}' /proc/meminfo
df -B1 --output=size,used / | tail -n 1 | awk '{print "disk_total=" $1; print "disk_used=" $2}'
echo "load=$(cut -d ' ' -f 1-3 /proc/loadavg)"
cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | awk '$4 == "0A" {split($2, address, ":"); print "listen=" address[2]}'
find "$PWD" -maxdepth 3 -name .git -prune 2>/dev/null | while read -r git_dir; do
  repository=$(dirname "$git_dir")
  echo "repository=$repository|$(git -C "$repository" rev-parse --abbrev-ref HEAD 2>/dev/null)|$(git -C "$repository" remote get-url origin 2>/dev/null)"
done
for agent in AGENTS; do command -v "$agent" >/dev/null 2>&1 && echo "agent=$agent"; done
"#;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct DiagnoseReport {
    pub checks: Vec<DiagnoseCheck>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct DiagnoseCheck {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub hint: String,
}

impl DiagnoseReport {
    fn check(&self, name: &str) -> Option<&DiagnoseCheck> {
        self.checks.iter().find(|check| check.name == name)
    }

    /// Reports the first problem that blocks using sandboxes, so the UI can show one actionable error.
    pub fn ensure_ready(&self) -> Result<(), SbxError> {
        if let Some(daemon) = self.check("Daemon")
            && daemon.status != "pass"
        {
            let message = [daemon.message.as_str(), daemon.hint.as_str()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(": ");
            return Err(SbxError::DaemonNotRunning { message });
        }
        if let Some(authentication) = self.check("Authentication")
            && authentication.status != "pass"
        {
            return Err(SbxError::NotLoggedIn);
        }
        Ok(())
    }

    /// Whether `sbx setup ssh` has added the `*.sbx` hosts to the SSH client config.
    /// The check passes either way, only its message tells the two states apart.
    pub fn ssh_configured(&self) -> bool {
        self.check("SSH client config")
            .is_some_and(|check| check.message == "configured")
    }
}

/// SSH host alias that `sbx setup ssh` routes to the sandbox through `sbx ssh proxy`.
pub fn ssh_host(sandbox_name: &str) -> String {
    format!("{sandbox_name}.sbx")
}

#[derive(Clone, Debug, Default)]
pub struct CreateOptions {
    pub name: String,
    /// Built-in agent name (`shell`, `claude`, `codex`, ...) or a sandbox kit reference.
    pub agent: String,
    /// Host directories to bind-mount. Empty keeps the sandbox fully detached from the host.
    pub workspaces: Vec<PathBuf>,
    /// Instead of bind-mounting, mount the host Git repository read-only and work on a private
    /// clone inside the sandbox (`--clone`). Its commits reach the host through the
    /// `sandbox-<name>` Git remote.
    pub clone_workspace: bool,
    pub template: Option<String>,
    pub cpus: Option<u32>,
    pub memory: Option<String>,
    pub environment: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct Sbx {
    executable: PathBuf,
}

impl Sbx {
    pub fn locate() -> Result<Self, SbxError> {
        which::which("sbx")
            .map(Self::with_executable)
            .map_err(|_| SbxError::NotInstalled)
    }

    pub fn with_executable(executable: PathBuf) -> Self {
        Self { executable }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub async fn version(&self) -> Result<VersionInfo, SbxError> {
        self.run_json(&["version", "--json"]).await
    }

    /// Verifies the CLI version, the daemon and the Docker login, in that order.
    pub async fn check(&self) -> Result<VersionInfo, SbxError> {
        let info = self.version().await?;
        ensure_supported_version(&info.client.version)?;
        self.diagnose().await?.ensure_ready()?;
        Ok(info)
    }

    pub async fn diagnose(&self) -> Result<DiagnoseReport, SbxError> {
        let arguments = ["diagnose", "--json"];
        let command = display_command(&arguments);
        let output = self.run(&arguments).await?;
        // A failing check makes `diagnose` exit non-zero while still printing the full report.
        match parse_json(&command, &output.stdout) {
            Ok(report) => Ok(report),
            Err(error) if output.status.success() => Err(error),
            Err(_) => Err(classify_failure(&command, &output.stderr)),
        }
    }

    pub async fn list(&self) -> Result<Vec<SandboxInfo>, SbxError> {
        self.run_json(&["ls", "--json"]).await
    }

    pub async fn get(&self, name: &str) -> Result<SandboxInfo, SbxError> {
        self.list()
            .await?
            .into_iter()
            .find(|sandbox| sandbox.name == name)
            .ok_or_else(|| SbxError::SandboxNotFound {
                name: name.to_string(),
            })
    }

    pub async fn create(&self, options: &CreateOptions) -> Result<(), SbxError> {
        let arguments = create_arguments(options);
        let output = self.run(&arguments).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(classify_failure(
                &display_command(&arguments),
                &output.stderr,
            ))
        }
    }

    /// `sbx` has no `start` command: any `exec` starts a stopped sandbox first.
    pub async fn start(&self, name: &str) -> Result<(), SbxError> {
        let output = self.exec(name, &["true"]).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(classify_failure(
                &display_command(&["exec", name, "true"]),
                &output.stderr,
            ))
        }
    }

    /// Adds an `Include` for the generated `*.sbx` host config to `~/.ssh/config`. Idempotent.
    pub async fn setup_ssh(&self) -> Result<(), SbxError> {
        let arguments = ["setup", "ssh"];
        let output = self.run(&arguments).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(classify_failure(
                &display_command(&arguments),
                &output.stderr,
            ))
        }
    }

    /// The directory commands start in, which is the sandbox workspace. Starts the sandbox if it
    /// is stopped.
    pub async fn default_directory(&self, name: &str) -> Result<String, SbxError> {
        let output = self.exec(name, &["pwd"]).await?;
        let directory = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if output.status.success() && directory.starts_with('/') {
            Ok(directory)
        } else {
            Err(SbxError::CommandFailed {
                command: display_command(&["exec", name, "pwd"]),
                message: first_error_line(&output.stderr)
                    .unwrap_or_else(|| format!("unexpected working directory {directory:?}")),
            })
        }
    }

    pub async fn stop(&self, name: &str) -> Result<(), SbxError> {
        self.run_lifecycle(&["stop", name, "--json"]).await
    }

    pub async fn remove(&self, name: &str) -> Result<(), SbxError> {
        self.run_lifecycle(&["rm", "--force", name, "--json"]).await
    }

    /// Runs a command inside the sandbox. A non-zero exit of the command itself is returned in
    /// the output, only failures of `sbx` (such as an unknown sandbox) become errors.
    pub async fn exec(
        &self,
        name: &str,
        command: &[impl AsRef<OsStr>],
    ) -> Result<Output, SbxError> {
        let mut arguments: Vec<OsString> = vec!["exec".into(), name.into()];
        arguments.extend(command.iter().map(|part| part.as_ref().to_os_string()));
        let output = self.run(&arguments).await?;
        if !output.status.success()
            && let Some(error) = sbx_error_from_stderr(&output.stderr)
        {
            return Err(error);
        }
        Ok(output)
    }

    pub async fn ports(&self, name: &str) -> Result<Vec<PublishedPort>, SbxError> {
        self.run_json(&["ports", name, "--json"]).await
    }

    /// Publishes a sandbox port on the host. Without `host_port`, `sbx` picks an ephemeral port.
    pub async fn publish_port(
        &self,
        name: &str,
        sandbox_port: u16,
        host_port: Option<u16>,
    ) -> Result<PublishedPort, SbxError> {
        let mapping = match host_port {
            Some(host_port) => format!("{host_port}:{sandbox_port}"),
            None => sandbox_port.to_string(),
        };
        let command = display_command(&["ports", name, "--publish", &mapping, "--json"]);
        let published: Vec<PublishedPort> = self
            .run_json(&["ports", name, "--publish", &mapping, "--json"])
            .await?;
        published
            .into_iter()
            .find(|port| port.sandbox_port == sandbox_port)
            .ok_or_else(|| SbxError::CommandFailed {
                command,
                message: format!("port {sandbox_port} was not reported as published"),
            })
    }

    pub async fn unpublish_port(&self, name: &str, port: &PublishedPort) -> Result<(), SbxError> {
        let mapping = format!("{}:{}", port.host_port, port.sandbox_port);
        self.run_json::<Vec<PublishedPort>>(&["ports", name, "--unpublish", &mapping, "--json"])
            .await?;
        Ok(())
    }

    /// Hosts the sandbox reached through the proxy, most recent first as reported by `sbx`.
    pub async fn network_log(&self, name: &str, limit: usize) -> Result<NetworkLog, SbxError> {
        let limit = limit.to_string();
        self.run_json(&["policy", "log", name, "--json", "--limit", &limit])
            .await
    }

    /// Network and filesystem rules that apply to the sandbox, global ones included.
    pub async fn policy_rules(&self, name: &str) -> Result<PolicyRules, SbxError> {
        self.run_json(&["policy", "ls", name, "--json"]).await
    }

    /// Collects resources, listening ports, repositories and agents from inside the sandbox.
    /// Starts the sandbox if it is stopped.
    pub async fn probe(&self, name: &str) -> Result<SandboxProbe, SbxError> {
        let script = PROBE_SCRIPT.replace("AGENTS", &KNOWN_AGENT_COMMANDS.join(" "));
        let output = self.exec(name, &["sh", "-c", script.as_str()]).await?;
        if !output.status.success() && output.stdout.is_empty() {
            return Err(classify_failure(
                &display_command(&["exec", name, "sh"]),
                &output.stderr,
            ));
        }
        Ok(parse_probe(&String::from_utf8_lossy(&output.stdout)))
    }

    async fn run(&self, arguments: &[impl AsRef<OsStr>]) -> Result<Output, SbxError> {
        new_command(&self.executable)
            .args(arguments)
            .output()
            .await
            .map_err(|source| SbxError::Spawn {
                command: display_command(arguments),
                source,
            })
    }

    async fn run_json<T: DeserializeOwned>(
        &self,
        arguments: &[impl AsRef<OsStr>],
    ) -> Result<T, SbxError> {
        let output = self.run(arguments).await?;
        if !output.status.success() {
            return Err(classify_failure(
                &display_command(arguments),
                &output.stderr,
            ));
        }
        parse_json(&display_command(arguments), &output.stdout)
    }

    async fn run_lifecycle(&self, arguments: &[&str]) -> Result<(), SbxError> {
        let output = self.run(arguments).await?;
        let command = display_command(arguments);
        if let Some(error) = sbx_error_from_stderr(&output.stderr) {
            return Err(error);
        }
        let results: Vec<LifecycleResult> = parse_json(&command, &output.stdout)?;
        match results.into_iter().find(|result| result.action == "failed") {
            Some(failed) => Err(SbxError::CommandFailed {
                command,
                message: failed
                    .error
                    .or(failed.name)
                    .unwrap_or_else(|| "unknown error".to_string()),
            }),
            None if output.status.success() => Ok(()),
            None => Err(classify_failure(&command, &output.stderr)),
        }
    }
}

/// Reads the limits a sandbox was created with from the `sbx` daemon's runtime record. The
/// record is internal to `sbx` 0.50, so a missing or changed file yields `None` rather than an
/// error.
pub fn configured_resources(sandbox_name: &str) -> Option<ConfiguredResources> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct RuntimeRecord {
        spec: RuntimeSpec,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct RuntimeSpec {
        #[serde(default, rename = "CPUs")]
        cpus: u32,
        #[serde(default)]
        memory: String,
        #[serde(default)]
        din_d_volume_size: String,
    }

    let path = runtime_records_dir()?.join(format!("{sandbox_name}.json"));
    let contents = std::fs::read(path).ok()?;
    let record: RuntimeRecord = serde_json::from_slice(&contents).ok()?;
    let non_empty = |value: String| (!value.is_empty()).then_some(value);
    Some(ConfiguredResources {
        cpus: (record.spec.cpus > 0).then_some(record.spec.cpus),
        memory: non_empty(record.spec.memory),
        docker_disk: non_empty(record.spec.din_d_volume_size),
    })
}

fn runtime_records_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(dirs::data_dir()?.join("com.docker.sandboxes/sandboxes/sandboxd/runtimes"))
    } else {
        None
    }
}

fn parse_probe(output: &str) -> SandboxProbe {
    let mut probe = SandboxProbe::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key {
            "cpus" => probe.cpus = value.parse().ok(),
            "memory_total_kb" => {
                probe.memory_total_bytes = value.parse::<u64>().ok().map(|kb| kb * 1024)
            }
            "memory_available_kb" => {
                probe.memory_available_bytes = value.parse::<u64>().ok().map(|kb| kb * 1024)
            }
            "disk_total" => probe.disk_total_bytes = value.parse().ok(),
            "disk_used" => probe.disk_used_bytes = value.parse().ok(),
            "load" if !value.is_empty() => probe.load_average = Some(value.to_string()),
            "listen" => {
                if let Ok(port) = u16::from_str_radix(value, 16) {
                    probe.listening_ports.push(port);
                }
            }
            "repository" => {
                let mut parts = value.splitn(3, '|');
                let Some(path) = parts.next().filter(|path| !path.is_empty()) else {
                    continue;
                };
                let non_empty = |part: Option<&str>| {
                    part.map(str::trim)
                        .filter(|part| !part.is_empty())
                        .map(str::to_string)
                };
                probe.repositories.push(SandboxRepository {
                    path: path.to_string(),
                    branch: non_empty(parts.next()),
                    remote: non_empty(parts.next()),
                });
            }
            "agent" if !value.is_empty() => probe.agents.push(value.to_string()),
            _ => {}
        }
    }
    probe.listening_ports.sort_unstable();
    probe.listening_ports.dedup();
    probe
}

pub fn ensure_supported_version(version: &str) -> Result<(), SbxError> {
    let unsupported = || SbxError::UnsupportedVersion {
        found: version.to_string(),
        required: SUPPORTED_VERSIONS.to_string(),
    };
    let parsed =
        Version::parse(version.trim().trim_start_matches('v')).map_err(|_| unsupported())?;
    let required = VersionReq::parse(SUPPORTED_VERSIONS).map_err(|_| unsupported())?;
    if required.matches(&parsed) {
        Ok(())
    } else {
        Err(unsupported())
    }
}

fn create_arguments(options: &CreateOptions) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec![
        "create".into(),
        "--no-progress".into(),
        "--name".into(),
        options.name.clone().into(),
    ];
    if let Some(template) = &options.template {
        arguments.extend(["--template".into(), template.into()]);
    }
    if let Some(cpus) = options.cpus {
        arguments.extend(["--cpus".into(), cpus.to_string().into()]);
    }
    if let Some(memory) = &options.memory {
        arguments.extend(["--memory".into(), memory.into()]);
    }
    for (key, value) in &options.environment {
        arguments.extend(["--env".into(), format!("{key}={value}").into()]);
    }
    if options.clone_workspace {
        arguments.push("--clone".into());
    }
    arguments.push(options.agent.clone().into());
    arguments.extend(
        options
            .workspaces
            .iter()
            .map(|workspace| workspace.as_os_str().to_os_string()),
    );
    arguments
}

fn parse_json<T: DeserializeOwned>(command: &str, stdout: &[u8]) -> Result<T, SbxError> {
    serde_json::from_slice(stdout).map_err(|source| SbxError::InvalidOutput {
        command: command.to_string(),
        source,
    })
}

fn display_command(arguments: &[impl AsRef<OsStr>]) -> String {
    std::iter::once("sbx".to_string())
        .chain(
            arguments
                .iter()
                .map(|argument| argument.as_ref().to_string_lossy().into_owned()),
        )
        .collect::<Vec<_>>()
        .join(" ")
}

fn first_error_line(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('✓') && !line.starts_with("try:"))
        .map(|line| {
            line.trim_start_matches("error:")
                .trim_start_matches("Error:")
                .trim()
                .to_string()
        })
}

/// Maps the messages `sbx` 0.50 prints for known failure modes to typed errors.
fn sbx_error_from_stderr(stderr: &[u8]) -> Option<SbxError> {
    let line = first_error_line(stderr)?;
    let lowercase = line.to_lowercase();
    if let Some(rest) = line.strip_prefix("sandbox '")
        && let Some((name, tail)) = rest.split_once('\'')
        && tail.trim() == "not found"
    {
        return Some(SbxError::SandboxNotFound {
            name: name.to_string(),
        });
    }
    if lowercase.contains("sbx login")
        || lowercase.contains("not logged in")
        || lowercase.contains("not authenticated")
    {
        return Some(SbxError::NotLoggedIn);
    }
    if lowercase.contains("daemon")
        && (lowercase.contains("not running") || lowercase.contains("connect"))
    {
        return Some(SbxError::DaemonNotRunning { message: line });
    }
    None
}

fn classify_failure(command: &str, stderr: &[u8]) -> SbxError {
    sbx_error_from_stderr(stderr).unwrap_or_else(|| SbxError::CommandFailed {
        command: command.to_string(),
        message: first_error_line(stderr).unwrap_or_else(|| "no error output".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn parses_probe_output() {
        let probe = parse_probe(
            "cpus=14\nmemory_total_kb=12417584\nmemory_available_kb=12030432\n\
             disk_total=20957446144\ndisk_used=45277184\nload=0.10 0.05 0.01\n\
             listen=1F90\nlisten=0016\nlisten=1F90\n\
             repository=/work/t.project|main|https://github.com/rtmo0/t.project.git\n\
             repository=/work/t.project/vendor/lib||\nagent=claude\nnoise\n",
        );
        assert_eq!(probe.cpus, Some(14));
        assert_eq!(probe.memory_total_bytes, Some(12417584 * 1024));
        assert_eq!(probe.disk_used_bytes, Some(45277184));
        assert_eq!(probe.load_average.as_deref(), Some("0.10 0.05 0.01"));
        assert_eq!(probe.listening_ports, vec![22, 8080]);
        assert_eq!(probe.repositories.len(), 2);
        assert_eq!(probe.repositories[0].branch.as_deref(), Some("main"));
        assert_eq!(probe.repositories[1].remote, None);
        assert_eq!(probe.agents, vec!["claude".to_string()]);
    }

    #[test]
    fn parses_network_log_and_policy_rules() -> TestResult {
        let log: NetworkLog = parse_json(
            "sbx policy log",
            br#"{"allowed": [{"sandbox_id": "t", "host": "api.anthropic.com:443", "proxy_type": "forward",
                 "count": 3, "first_seen": "2026-11-04T10:58:42+03:00", "last_seen": "2026-11-04T11:00:00+03:00"}],
                 "blocked": []}"#,
        )?;
        assert_eq!(log.allowed[0].host, "api.anthropic.com:443");
        assert_eq!(log.allowed[0].count, 3);
        let rules: PolicyRules = parse_json(
            "sbx policy ls",
            br#"{"rules": [{"id": "default-allow-all", "decision": "allow", "resource_type": "network",
                 "resources": ["**"], "scope": "global", "status": "active", "editable": true}]}"#,
        )?;
        assert_eq!(rules.rules[0].resources, vec!["**".to_string()]);
        Ok(())
    }

    #[test]
    fn parses_version() -> TestResult {
        let info: VersionInfo = parse_json(
            "sbx version",
            br#"{
              "client": {"version": "v0.50.1", "revision": "ddb0268d"},
              "server": {"state": "running", "version": "v0.50.1", "revision": "ddb0268d", "api_version": "0.44.0"}
            }"#,
        )?;
        assert_eq!(info.client.version, "v0.50.1");
        assert_eq!(
            info.server.and_then(|server| server.state),
            Some("running".to_string())
        );
        Ok(())
    }

    #[test]
    fn checks_supported_versions() {
        assert!(ensure_supported_version("v0.50.1").is_ok());
        assert!(ensure_supported_version("0.50.0").is_ok());
        assert!(matches!(
            ensure_supported_version("v0.49.9"),
            Err(SbxError::UnsupportedVersion { .. })
        ));
        assert!(matches!(
            ensure_supported_version("v0.51.0"),
            Err(SbxError::UnsupportedVersion { .. })
        ));
        assert!(matches!(
            ensure_supported_version("dev"),
            Err(SbxError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn parses_sandbox_list() -> TestResult {
        let sandboxes: Vec<SandboxInfo> = parse_json(
            "sbx ls",
            br#"[
              {"name": "spike", "id": "5f04eece", "agent": "shell", "status": "stopped",
               "last_used_at": "2026-11-03T12:27:36.953433Z", "created_at": "2026-11-03T11:33:20Z"},
              {"name": "next", "id": "6a", "agent": "claude", "status": "provisioning"}
            ]"#,
        )?;
        assert_eq!(sandboxes.len(), 2);
        assert_eq!(sandboxes[0].status, SandboxStatus::Stopped);
        assert_eq!(sandboxes[1].status, SandboxStatus::Unknown);
        assert_eq!(sandboxes[1].created_at, None);
        Ok(())
    }

    #[test]
    fn parses_ports() -> TestResult {
        let published: Vec<PublishedPort> = parse_json(
            "sbx ports",
            br#"[{"action": "published", "host_ip": "127.0.0.1", "host_port": 49152, "sandbox_port": 8080, "protocol": "tcp4"}]"#,
        )?;
        assert_eq!(published[0].host_port, 49152);
        assert_eq!(published[0].sandbox_port, 8080);

        let unpublished: Vec<PublishedPort> = parse_json(
            "sbx ports",
            br#"[{"action": "unpublished", "host_port": 49152, "sandbox_port": 8080}]"#,
        )?;
        assert_eq!(unpublished[0].host_ip, None);
        Ok(())
    }

    #[test]
    fn classifies_known_failures() {
        assert!(matches!(
            sbx_error_from_stderr(b"error: sandbox 'nonexistent' not found\n  try: sbx ls\n"),
            Some(SbxError::SandboxNotFound { name }) if name == "nonexistent"
        ));
        assert!(matches!(
            sbx_error_from_stderr(b"error: not logged in, run `sbx login`\n"),
            Some(SbxError::NotLoggedIn)
        ));
        assert!(matches!(
            sbx_error_from_stderr(b"error: cannot connect to the sandboxd daemon\n"),
            Some(SbxError::DaemonNotRunning { .. })
        ));
        assert!(
            sbx_error_from_stderr(b"  \xe2\x9c\x93 sandbox spike \xe2\x80\x94 started\n").is_none()
        );
        assert!(matches!(
            classify_failure("sbx rm x", b"error: something else\n"),
            SbxError::CommandFailed { message, .. } if message == "something else"
        ));
    }

    #[test]
    fn diagnose_reports_first_blocking_problem() -> TestResult {
        let report: DiagnoseReport = parse_json(
            "sbx diagnose",
            br#"{"version": "v0.50.1", "summary": {}, "checks": [
              {"name": "Daemon", "status": "pass", "message": "healthy", "detail": "", "hint": ""},
              {"name": "Authentication", "status": "fail", "message": "not signed in", "detail": "", "hint": "sbx login"}
            ]}"#,
        )?;
        assert!(matches!(report.ensure_ready(), Err(SbxError::NotLoggedIn)));
        assert!(!report.ssh_configured());
        Ok(())
    }

    #[test]
    fn detects_ssh_configuration() -> TestResult {
        let parse = |message: &str| -> Result<DiagnoseReport, SbxError> {
            let json = format!(
                r#"{{"checks": [{{"name": "SSH client config", "status": "pass", "message": "{message}"}}]}}"#
            );
            parse_json("sbx diagnose", json.as_bytes())
        };
        assert!(parse("configured")?.ssh_configured());
        assert!(!parse("not configured")?.ssh_configured());
        assert_eq!(ssh_host("spike"), "spike.sbx");
        Ok(())
    }

    #[test]
    fn builds_create_arguments() {
        let arguments = create_arguments(&CreateOptions {
            name: "project".to_string(),
            agent: "shell".to_string(),
            workspaces: vec![PathBuf::from("/work/repo")],
            cpus: Some(4),
            memory: Some("8g".to_string()),
            environment: vec![("KEY".to_string(), "value".to_string())],
            clone_workspace: true,
            template: None,
        });
        let arguments: Vec<String> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            arguments,
            [
                "create",
                "--no-progress",
                "--name",
                "project",
                "--cpus",
                "4",
                "--memory",
                "8g",
                "--env",
                "KEY=value",
                "--clone",
                "shell",
                "/work/repo"
            ]
        );
    }

    /// Talks to the real `sbx` on this machine: `cargo test -p sbx -- --ignored`.
    #[test]
    #[ignore]
    fn real_sbx_is_ready() -> TestResult {
        smol::block_on(async {
            let sbx = Sbx::locate()?;
            let info = sbx.check().await?;
            assert!(info.client.version.starts_with('v'));
            sbx.list().await?;
            assert!(matches!(
                sbx.ports("sands-test-missing-sandbox").await,
                Err(SbxError::SandboxNotFound { .. })
            ));
            Ok(())
        })
    }
}
