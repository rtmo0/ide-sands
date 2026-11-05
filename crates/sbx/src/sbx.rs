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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkDecision {
    Allow,
    Deny,
}

/// What a sandbox may reach on top of `sbx`'s default deny-all policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetworkMode {
    AllowAll,
    #[default]
    Balanced,
    DenyAll,
}

/// Development traffic for the languages Sands supports, modeled on the `sbx` balanced policy:
/// Git hosting, package registries, toolchain downloads, OS packages, container registries,
/// model providers and certificate checks.
pub const BALANCED_NETWORK_HOSTS: &[&str] = &[
    "github.com",
    "**.github.com",
    "**.githubusercontent.com",
    "gitlab.com",
    "**.gitlab.com",
    "bitbucket.org",
    "go.dev",
    "dl.google.com",
    "golang.org",
    "proxy.golang.org",
    "sum.golang.org",
    "pypi.org",
    "**.pythonhosted.org",
    "bootstrap.pypa.io",
    "astral.sh",
    "nodejs.org",
    "npmjs.org",
    "registry.npmjs.org",
    "yarnpkg.com",
    "**.yarnpkg.com",
    "deb.nodesource.com",
    "crates.io",
    "**.crates.io",
    "rustup.rs",
    "sh.rustup.rs",
    "static.rust-lang.org",
    "pub.dev",
    "pub.dartlang.org",
    "storage.googleapis.com",
    "nuget.org",
    "**.nuget.org",
    "dot.net",
    "**.dot.net",
    "dotnet.microsoft.com",
    "**.dotnet.microsoft.com",
    "packages.microsoft.com",
    "apt.llvm.org",
    "archive.ubuntu.com",
    "security.ubuntu.com",
    "ports.ubuntu.com",
    "docker.io",
    "**.docker.io",
    "docker.com",
    "**.docker.com",
    "ghcr.io",
    "mcr.microsoft.com",
    "api.anthropic.com",
    "**.anthropic.com",
    "claude.ai",
    "**.claude.ai",
    "api.openai.com",
    "**.openai.com",
    "chatgpt.com",
    "**.chatgpt.com",
    "generativelanguage.googleapis.com",
    "**.githubcopilot.com",
    "openrouter.ai",
    "**.lencr.org",
    "**.digicert.com",
    "**.pki.goog",
];

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
    /// Toolchains the project uses or that are installed, in [`Toolchain::ALL`] order.
    pub toolchains: Vec<ToolchainStatus>,
}

/// Language toolchains Sands sets up inside sandboxes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Toolchain {
    Go,
    Python,
    Node,
    Flutter,
    Rust,
    DotNet,
    Cpp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolchainStatus {
    pub toolchain: Toolchain,
    /// Whether the workspace has files that need this toolchain, such as `go.mod`.
    pub used_by_project: bool,
    /// The installed version, or `None` when the toolchain is missing.
    pub version: Option<String>,
}

impl ToolchainStatus {
    pub fn is_missing(&self) -> bool {
        self.used_by_project && self.version.is_none()
    }
}

impl Toolchain {
    pub const ALL: [Toolchain; 7] = [
        Toolchain::Go,
        Toolchain::Python,
        Toolchain::Node,
        Toolchain::Flutter,
        Toolchain::Rust,
        Toolchain::DotNet,
        Toolchain::Cpp,
    ];

    pub fn display_name(self) -> &'static str {
        match self {
            Toolchain::Go => "Go",
            Toolchain::Python => "Python",
            Toolchain::Node => "Node.js",
            Toolchain::Flutter => "Flutter / Dart",
            Toolchain::Rust => "Rust",
            Toolchain::DotNet => ".NET",
            Toolchain::Cpp => "C / C++",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Toolchain::Go => "go",
            Toolchain::Python => "python",
            Toolchain::Node => "node",
            Toolchain::Flutter => "flutter",
            Toolchain::Rust => "rust",
            Toolchain::DotNet => "dotnet",
            Toolchain::Cpp => "cpp",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|toolchain| toolchain.id() == id)
    }

    /// The toolchain a project file implies, by file name.
    fn for_marker(file_name: &str) -> Option<Self> {
        match file_name {
            "go.mod" => Some(Toolchain::Go),
            "pyproject.toml" | "requirements.txt" | "setup.py" | "Pipfile" => {
                Some(Toolchain::Python)
            }
            "package.json" => Some(Toolchain::Node),
            "pubspec.yaml" => Some(Toolchain::Flutter),
            "Cargo.toml" => Some(Toolchain::Rust),
            "CMakeLists.txt" | "meson.build" => Some(Toolchain::Cpp),
            _ if file_name.ends_with(".csproj") || file_name.ends_with(".sln") => {
                Some(Toolchain::DotNet)
            }
            _ => None,
        }
    }

    /// Idempotent installation script. Language servers and debug adapters that Sands
    /// downloads on demand are not installed here, only what they need to run.
    fn install_script(self) -> &'static str {
        match self {
            Toolchain::Go => {
                r#"command -v go >/dev/null || { version=$(curl -fsSL 'https://go.dev/VERSION?m=text' | head -n 1); curl -fsSL "https://go.dev/dl/$version.linux-$(dpkg --print-architecture).tar.gz" | sudo tar -C /usr/local -xz; }
add_path /usr/local/go/bin
add_path '$HOME/go/bin'"#
            }
            Toolchain::Python => "apt_install python3 python3-venv python3-pip",
            Toolchain::Node => "command -v node >/dev/null || apt_install nodejs npm",
            Toolchain::Flutter => {
                r#"[ -x "$HOME/flutter/bin/flutter" ] || git clone --quiet --depth 1 -b stable https://github.com/flutter/flutter.git "$HOME/flutter"
"$HOME/flutter/bin/flutter" --disable-analytics >/dev/null 2>&1
"$HOME/flutter/bin/flutter" --version >/dev/null
add_path '$HOME/flutter/bin'
add_path '$HOME/.pub-cache/bin'"#
            }
            Toolchain::Rust => {
                r#"command -v rustup >/dev/null || [ -x "$HOME/.cargo/bin/rustup" ] || curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --component rust-analyzer --component rust-src
add_path '$HOME/.cargo/bin'"#
            }
            Toolchain::DotNet => {
                r#"command -v dotnet >/dev/null || apt_install dotnet-sdk-10.0
if [ ! -x "$HOME/.local/share/netcoredbg/netcoredbg" ]; then
  case "$(uname -m)" in aarch64) netcoredbg_arch=linux-arm64 ;; *) netcoredbg_arch=linux-amd64 ;; esac
  mkdir -p "$HOME/.local/share"
  curl -fsSL "https://github.com/Samsung/netcoredbg/releases/latest/download/netcoredbg-$netcoredbg_arch.tar.gz" | tar -xz -C "$HOME/.local/share"
fi
add_path '$HOME/.dotnet/tools'
add_path '$HOME/.local/share/netcoredbg'"#
            }
            Toolchain::Cpp => "apt_install clang clangd gdb cmake",
        }
    }
}

const TOOLCHAIN_SCRIPT_PRELUDE: &str = r#"set -e
apt_updated=
apt_install() {
  [ -n "$apt_updated" ] || { sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq; apt_updated=1; }
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null
}
add_path() {
  for profile in "$HOME/.profile" "$HOME/.bashrc"; do
    line="export PATH="$1:\$PATH""
    grep -qxF "$line" "$profile" 2>/dev/null || echo "$line" >> "$profile"
  done
}
"#;

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
find "$PWD" -maxdepth 5 \( -name node_modules -o -name .git -o -name build -o -name target -o -name .dart_tool \) -prune -o \( -name go.mod -o -name pyproject.toml -o -name requirements.txt -o -name setup.py -o -name Pipfile -o -name package.json -o -name pubspec.yaml -o -name Cargo.toml -o -name '*.csproj' -o -name '*.sln' -o -name CMakeLists.txt -o -name meson.build \) -print 2>/dev/null | while read -r marker; do echo "marker=${marker##*/}"; done
command -v go >/dev/null 2>&1 && echo "toolchain=go|$(go version 2>/dev/null | cut -d ' ' -f 3)"
command -v python3 >/dev/null 2>&1 && echo "toolchain=python|$(python3 --version 2>&1 | cut -d ' ' -f 2)"
command -v node >/dev/null 2>&1 && echo "toolchain=node|$(node --version 2>/dev/null)"
command -v dart >/dev/null 2>&1 && echo "toolchain=flutter|$(dart --version 2>&1 | head -n 1 | cut -d ' ' -f 4)"
command -v rustc >/dev/null 2>&1 && echo "toolchain=rust|$(rustc --version 2>/dev/null | cut -d ' ' -f 2)"
command -v dotnet >/dev/null 2>&1 && echo "toolchain=dotnet|$(dotnet --version 2>/dev/null)"
command -v clangd >/dev/null 2>&1 && echo "toolchain=cpp|$(clangd --version 2>/dev/null | head -n 1 | grep -o '[0-9][0-9.]*' | head -n 1)"
exit 0
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
    /// Host directories mounted read-only at their host paths.
    pub read_only_workspaces: Vec<PathBuf>,
    /// Port mappings in `sbx create --publish` syntax, e.g. `8080` or `3000:3000`.
    pub publish: Vec<String>,
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

    pub async fn copy_to(
        &self,
        name: &str,
        source: &Path,
        destination: &str,
    ) -> Result<(), SbxError> {
        let sandbox_destination = format!("{name}:{destination}");
        self.run_success_os(&[
            OsString::from("cp"),
            source.as_os_str().to_os_string(),
            OsString::from(sandbox_destination),
        ])
        .await
    }

    /// Stores a host-side bearer token whose placeholder is replaced only for requests from one
    /// sandbox to one host. `value_file` must remain readable after this returns because `sbx`
    /// invokes the resolver again when it refreshes the secret.
    pub async fn set_custom_secret_from_file(
        &self,
        name: &str,
        host: &str,
        placeholder: &str,
        value_file: &Path,
    ) -> Result<(), SbxError> {
        let value_file = value_file.to_string_lossy();
        if value_file.contains('\'') {
            return Err(SbxError::CommandFailed {
                command: "sbx secret set-custom".to_string(),
                message: "the secret file path contains an unsupported quote".to_string(),
            });
        }
        let resolver = format!("/bin/cat '{value_file}'");
        self.run_success(&[
            "secret",
            "set-custom",
            "--sandbox",
            name,
            "--host",
            host,
            "--placeholder",
            placeholder,
            "--command",
            &resolver,
        ])
        .await
    }

    pub async fn remove_custom_secret(
        &self,
        name: &str,
        placeholder: &str,
    ) -> Result<(), SbxError> {
        self.run_success(&[
            "secret",
            "rm",
            "--sandbox",
            name,
            "--placeholder",
            placeholder,
            "--force",
        ])
        .await
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

    /// Adds a network rule scoped to one sandbox.
    pub async fn add_network_rule(
        &self,
        name: &str,
        decision: NetworkDecision,
        resource: &str,
    ) -> Result<(), SbxError> {
        let decision = match decision {
            NetworkDecision::Allow => "allow",
            NetworkDecision::Deny => "deny",
        };
        self.run_success(&["policy", decision, "network", "--sandbox", name, resource])
            .await
    }

    /// Allows what `mode` permits plus `allowed_hosts`. Denied traffic needs no rules because
    /// [`Self::scope_network_policy_to_sandboxes`] leaves `sbx`'s default deny-all in place.
    pub async fn apply_network_mode(
        &self,
        name: &str,
        mode: NetworkMode,
        allowed_hosts: &[String],
    ) -> Result<(), SbxError> {
        let mode_hosts: &[&str] = match mode {
            NetworkMode::AllowAll => &["**"],
            NetworkMode::Balanced => BALANCED_NETWORK_HOSTS,
            NetworkMode::DenyAll => &[],
        };
        let hosts = mode_hosts
            .iter()
            .copied()
            .chain(allowed_hosts.iter().map(String::as_str))
            .collect::<Vec<_>>();
        if hosts.is_empty() {
            return Ok(());
        }
        self.add_network_rule(name, NetworkDecision::Allow, &hosts.join(","))
            .await
    }

    /// A global allow-all rule (the `sbx` default) would override every sandbox's own allow
    /// list, because a sandbox deny rule for `**` also blocks its own allow rules. This moves
    /// that rule onto each existing sandbox, so they keep their access, and removes it, so new
    /// sandboxes start from the default deny-all and get only the rules Sands gives them.
    pub async fn scope_network_policy_to_sandboxes(&self) -> Result<(), SbxError> {
        let policy: PolicyRules = self.run_json(&["policy", "ls", "--json"]).await?;
        let global_allow_all = policy
            .rules
            .into_iter()
            .filter(|rule| {
                rule.resource_type == "network"
                    && rule.decision == "allow"
                    && rule.scope.as_deref() == Some("global")
                    && rule.resources == ["**"]
            })
            .collect::<Vec<_>>();
        if global_allow_all.is_empty() {
            return Ok(());
        }
        for sandbox in self.list().await? {
            self.add_network_rule(&sandbox.name, NetworkDecision::Allow, "**")
                .await?;
        }
        for rule in global_allow_all {
            self.run_success(&["policy", "rm", "network", "--id", &rule.id, "--force"])
                .await?;
        }
        Ok(())
    }

    /// Removes a network rule scoped to one sandbox. Global policy rules cannot be removed here.
    pub async fn remove_network_rule(&self, name: &str, rule_id: &str) -> Result<(), SbxError> {
        self.run_success(&[
            "policy",
            "rm",
            "network",
            "--sandbox",
            name,
            "--id",
            rule_id,
            "--force",
        ])
        .await
    }

    /// Clones a Git repository (a URL, or a host path mounted into the sandbox) into the sandbox
    /// workspace and returns the clone's path.
    pub async fn clone_repository(&self, name: &str, source: &str) -> Result<String, SbxError> {
        let directory_name =
            repository_directory_name(source).ok_or_else(|| SbxError::CommandFailed {
                command: "git clone".to_string(),
                message: format!("cannot derive a safe directory name from {source:?}"),
            })?;
        let workspace = self.default_directory(name).await?;
        let target = format!("{workspace}/{directory_name}");
        let output = self
            .exec(name, &["git", "clone", "--", source, target.as_str()])
            .await?;
        if !output.status.success() {
            return Err(SbxError::CommandFailed {
                command: format!("git clone {source}"),
                message: first_error_line(&output.stderr)
                    .unwrap_or_else(|| "git clone failed without error output".to_string()),
            });
        }
        // With `--clone` the workspace is itself a repository, and the nested clone would show
        // up in it as untracked files.
        let exclude_entry = format!("/{directory_name}/");
        let output = self
            .exec(
                name,
                &[
                    "sh",
                    "-c",
                    "exclude=$(git -C \"$1\" rev-parse --git-path info/exclude 2>/dev/null) || exit 0; \
                     cd \"$1\" && mkdir -p \"$(dirname \"$exclude\")\" && \
                     { grep -qxF \"$2\" \"$exclude\" 2>/dev/null || echo \"$2\" >> \"$exclude\"; }",
                    "sh",
                    workspace.as_str(),
                    exclude_entry.as_str(),
                ],
            )
            .await?;
        if !output.status.success() {
            return Err(SbxError::CommandFailed {
                command: format!("exclude {directory_name} from {workspace}"),
                message: first_error_line(&output.stderr)
                    .unwrap_or_else(|| "updating info/exclude failed".to_string()),
            });
        }
        Ok(target)
    }

    /// Collects resources, listening ports, repositories and agents from inside the sandbox.
    /// Starts the sandbox if it is stopped.
    pub async fn probe(&self, name: &str) -> Result<SandboxProbe, SbxError> {
        let script = PROBE_SCRIPT.replace("AGENTS", &KNOWN_AGENT_COMMANDS.join(" "));
        // A login shell picks up toolchains that installation added to `~/.profile`.
        let output = self.exec(name, &["bash", "-lc", script.as_str()]).await?;
        if !output.status.success() && output.stdout.is_empty() {
            return Err(classify_failure(
                &display_command(&["exec", name, "sh"]),
                &output.stderr,
            ));
        }
        Ok(parse_probe(&String::from_utf8_lossy(&output.stdout)))
    }

    /// Installs toolchains inside the sandbox. This can take minutes (Flutter alone is close to
    /// a gigabyte), so callers should show progress.
    pub async fn install_toolchains(
        &self,
        name: &str,
        toolchains: &[Toolchain],
    ) -> Result<(), SbxError> {
        if toolchains.is_empty() {
            return Ok(());
        }
        let script = toolchain_install_script(toolchains);
        let output = self.exec(name, &["bash", "-lc", script.as_str()]).await?;
        if output.status.success() {
            return Ok(());
        }
        let names = toolchains
            .iter()
            .map(|toolchain| toolchain.display_name())
            .collect::<Vec<_>>()
            .join(", ");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last_lines = stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        Err(SbxError::CommandFailed {
            command: format!("install {names} in sandbox {name}"),
            message: last_lines,
        })
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

    async fn run_success(&self, arguments: &[&str]) -> Result<(), SbxError> {
        let output = self.run(arguments).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(classify_failure(
                &display_command(arguments),
                &output.stderr,
            ))
        }
    }

    async fn run_success_os(&self, arguments: &[OsString]) -> Result<(), SbxError> {
        let output = self.run(arguments).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(classify_failure(
                &display_command(arguments),
                &output.stderr,
            ))
        }
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

fn toolchain_install_script(toolchains: &[Toolchain]) -> String {
    let mut script = String::from(TOOLCHAIN_SCRIPT_PRELUDE);
    for toolchain in toolchains {
        script.push_str(toolchain.install_script());
        script.push('\n');
    }
    script
}

fn repository_directory_name(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        });
    safe.then(|| name.to_string())
}

fn parse_probe(output: &str) -> SandboxProbe {
    let mut probe = SandboxProbe::default();
    let mut used = std::collections::HashSet::new();
    let mut installed = Vec::new();
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
            "marker" => {
                if let Some(toolchain) = Toolchain::for_marker(value) {
                    used.insert(toolchain);
                }
            }
            "toolchain" => {
                if let Some((id, version)) = value.split_once('|')
                    && let Some(toolchain) = Toolchain::from_id(id)
                {
                    let version = version.trim();
                    installed.push((
                        toolchain,
                        if version.is_empty() {
                            "installed"
                        } else {
                            version
                        }
                        .to_string(),
                    ));
                }
            }
            _ => {}
        }
    }
    probe.toolchains = Toolchain::ALL
        .into_iter()
        .filter_map(|toolchain| {
            let version = installed
                .iter()
                .find(|(installed, _)| *installed == toolchain)
                .map(|(_, version)| version.clone());
            let used_by_project = used.contains(&toolchain);
            (used_by_project || version.is_some()).then_some(ToolchainStatus {
                toolchain,
                used_by_project,
                version,
            })
        })
        .collect();
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
    for mapping in &options.publish {
        arguments.extend(["--publish".into(), mapping.into()]);
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
    arguments.extend(options.read_only_workspaces.iter().map(|workspace| {
        let mut argument = workspace.as_os_str().to_os_string();
        argument.push(":ro");
        argument
    }));
    arguments
}

fn parse_json<T: DeserializeOwned>(command: &str, stdout: &[u8]) -> Result<T, SbxError> {
    // `sbx` occasionally prints an update notice after an otherwise valid JSON response.
    T::deserialize(&mut serde_json::Deserializer::from_slice(stdout)).map_err(|source| {
        SbxError::InvalidOutput {
            command: command.to_string(),
            source,
        }
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
    fn detects_toolchains() {
        let probe = parse_probe(
            "marker=go.mod\nmarker=pubspec.yaml\nmarker=requirements.txt\nmarker=App.csproj\n\
             marker=README.md\ntoolchain=go|go1.27.3\ntoolchain=python|3.14.4\ntoolchain=node|v22.22.1\n",
        );
        let summary = probe
            .toolchains
            .iter()
            .map(|status| {
                (
                    status.toolchain,
                    status.used_by_project,
                    status.version.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                (Toolchain::Go, true, Some("go1.27.3".to_string())),
                (Toolchain::Python, true, Some("3.14.4".to_string())),
                (Toolchain::Node, false, Some("v22.22.1".to_string())),
                (Toolchain::Flutter, true, None),
                (Toolchain::DotNet, true, None),
            ]
        );
        let missing = probe
            .toolchains
            .iter()
            .filter(|status| status.is_missing())
            .map(|status| status.toolchain)
            .collect::<Vec<_>>();
        assert_eq!(missing, vec![Toolchain::Flutter, Toolchain::DotNet]);
        let script = toolchain_install_script(&missing);
        assert!(script.starts_with("set -e"));
        assert!(script.contains("flutter.git") && script.contains("dotnet-sdk-10.0"));
    }

    #[test]
    fn derives_safe_repository_directory_names() {
        assert_eq!(
            repository_directory_name("https://github.com/owner/service-api.git"),
            Some("service-api".to_string())
        );
        assert_eq!(
            repository_directory_name("git@github.com:owner/web_app.git"),
            Some("web_app".to_string())
        );
        assert_eq!(repository_directory_name("https://example.com/../"), None);
        assert_eq!(repository_directory_name(""), None);
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
        let info: VersionInfo = parse_json(
            "sbx version",
            b"{\"client\":{\"version\":\"v0.50.1\"}}\nA newer sbx version is available\n",
        )?;
        assert_eq!(info.client.version, "v0.50.1");
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
            read_only_workspaces: vec![PathBuf::from("/work/docs")],
            publish: vec!["8080".to_string(), "3000:3000".to_string()],
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
                "--publish",
                "8080",
                "--publish",
                "3000:3000",
                "--clone",
                "shell",
                "/work/repo",
                "/work/docs:ro"
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
