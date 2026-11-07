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
    /// Host directories mounted into the sandbox.
    #[serde(default)]
    pub workspaces: Vec<String>,
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
#[derive(Clone, Debug, Default, PartialEq)]
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
    /// Usage of the agents that report it, newest data per agent.
    pub agent_usage: Vec<AgentUsage>,
    /// Whether Claude Code in the sandbox reports its status to Sands.
    pub claude_status_hook: bool,
}

/// What `sbx login` asks the user to do in the browser.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginPrompt {
    /// The one-time device code, such as `ABCD-EFGH`.
    pub code: Option<String>,
    /// The page that asks for the code.
    pub url: Option<String>,
}

impl LoginPrompt {
    /// Picks the code and the URL out of a line of `sbx login` output. Returns whether either
    /// was new.
    fn read(&mut self, line: &str) -> bool {
        let mut changed = false;
        for word in line.split_whitespace() {
            let word = word.trim_matches(|character: char| {
                matches!(character, '"' | '\'' | '(' | ')' | '<' | '>')
            });
            if (word.starts_with("https://") || word.starts_with("http://"))
                && self.url.as_deref() != Some(word)
            {
                self.url = Some(word.trim_end_matches(['.', ',']).to_string());
                changed = true;
            } else {
                let word = word.trim_end_matches(['.', ',', ':']);
                if is_device_code(word) && self.code.as_deref() != Some(word) {
                    self.code = Some(word.to_string());
                    changed = true;
                }
            }
        }
        changed
    }
}

/// Device codes look like `ABCD-EFGH`: two groups of four uppercase letters or digits.
fn is_device_code(word: &str) -> bool {
    let Some((first, second)) = word.split_once('-') else {
        return false;
    };
    let group = |part: &str| {
        part.len() == 4
            && part
                .chars()
                .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
    };
    group(first) && group(second)
}

/// One usage limit of an agent subscription, such as the 5-hour or the weekly window.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f64,
    /// Unix time in seconds when the window starts over.
    pub resets_at: Option<i64>,
}

/// What an agent last reported about its limits, cost and context.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentUsage {
    /// The `sbx` agent name: `claude` or `codex`.
    pub agent: String,
    pub plan: Option<String>,
    pub model: Option<String>,
    pub windows: Vec<UsageWindow>,
    pub session_cost_usd: Option<f64>,
    pub context_used_percent: Option<f64>,
    pub session_tokens: Option<u64>,
    /// Unix time in seconds of the report.
    pub updated_at: Option<i64>,
}

/// Saves the JSON Claude Code passes to its status line, where the Sandbox panel reads it, then
/// prints the user's own status line if there was one.
const CLAUDE_STATUSLINE_SCRIPT: &str = r#"#!/bin/sh
input=$(cat)
status_dir="$HOME/.sands/agent-status"
mkdir -p "$status_dir"
printf '%s' "$input" > "$status_dir/claude.json.tmp" && mv "$status_dir/claude.json.tmp" "$status_dir/claude.json"
if [ -s "$HOME/.sands/claude-statusline-original" ]; then
  printf '%s' "$input" | sh -c "$(cat "$HOME/.sands/claude-statusline-original")"
else
  printf '%s' "$input" | python3 -c '
import json, sys
data = json.load(sys.stdin)
parts = [(data.get("model") or {}).get("display_name") or "Claude"]
context = (data.get("context_window") or {}).get("used_percentage")
if context is not None:
    parts.append("%d%% context" % context)
five_hour = ((data.get("rate_limits") or {}).get("five_hour") or {}).get("used_percentage")
if five_hour is not None:
    parts.append("5h %d%%" % five_hour)
print(" · ".join(parts))
' 2>/dev/null || echo Claude
fi
"#;

/// Points Claude Code's status line at [`CLAUDE_STATUSLINE_SCRIPT`], keeping an existing status
/// line command to chain to. Leaves a settings file that is not valid JSON alone.
const CLAUDE_STATUS_HOOK_INSTALLER: &str = r#"set -e
mkdir -p "$HOME/.sands" "$HOME/.claude"
cat > "$HOME/.sands/claude-statusline.sh" <<'SANDS_STATUSLINE'
SCRIPT
SANDS_STATUSLINE
chmod +x "$HOME/.sands/claude-statusline.sh"
python3 - <<'SANDS_SETTINGS'
import json, os
path = os.path.expanduser("~/.claude/settings.json")
ours = os.path.expanduser("~/.sands/claude-statusline.sh")
try:
    with open(path) as file:
        settings = json.load(file)
except FileNotFoundError:
    settings = {}
current = (settings.get("statusLine") or {}).get("command")
if current and current != ours:
    with open(os.path.expanduser("~/.sands/claude-statusline-original"), "w") as file:
        file.write(current)
settings["statusLine"] = {"type": "command", "command": ours}
with open(path + ".sands-tmp", "w") as file:
    json.dump(settings, file, indent=2)
os.replace(path + ".sands-tmp", path)
SANDS_SETTINGS
"#;

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
claude_status="$HOME/.sands/agent-status/claude.json"
[ -s "$claude_status" ] && echo "agent_status=claude|$(stat -c %Y "$claude_status")|$(base64 -w0 < "$claude_status")"
grep -qs 'sands/claude-statusline.sh' "$HOME/.claude/settings.json" && echo "claude_status_hook=1"
codex_session=$(find "$HOME/.codex/sessions" -name '*.jsonl' -type f -printf '%T@ %p\n' 2>/dev/null | sort -n | tail -n 1 | cut -d ' ' -f 2-)
if [ -n "$codex_session" ]; then
  codex_event=$(grep '"used_percent"' "$codex_session" | tail -n 1)
  [ -n "$codex_event" ] && echo "agent_status=codex|$(stat -c %Y "$codex_session")|$(printf '%s' "$codex_event" | base64 -w0)"
fi
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

    /// Starts the Docker Sandboxes daemon in the background. It takes a few seconds to accept
    /// commands; callers wait for [`DiagnoseReport::ensure_ready`] to stop reporting it.
    pub async fn start_daemon(&self) -> Result<(), SbxError> {
        self.run_success(&["daemon", "start", "--detach"]).await
    }

    /// Signs in to Docker with the browser device flow. `on_prompt` receives the one-time code
    /// and the confirmation URL as soon as `sbx login` prints them; the call returns when the
    /// sign-in finishes.
    pub async fn login(
        &self,
        mut on_prompt: impl FnMut(LoginPrompt) + Send,
    ) -> Result<(), SbxError> {
        use futures::{AsyncBufReadExt as _, AsyncWriteExt as _};

        let command = "sbx login";
        // The device code goes to stdout and the progress to stderr; one stream keeps the order.
        let mut child = new_command("/bin/sh")
            .arg("-c")
            .arg("exec \"$0\" login 2>&1")
            .arg(&self.executable)
            .stdin(util::command::Stdio::piped())
            .stdout(util::command::Stdio::piped())
            .stderr(util::command::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| SbxError::Spawn {
                command: command.to_string(),
                source,
            })?;
        let mut stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| SbxError::CommandFailed {
            command: command.to_string(),
            message: "sbx login has no output pipe".to_string(),
        })?;
        let mut lines = futures::io::BufReader::new(stdout).lines();
        let mut output = Vec::new();
        let mut prompt = LoginPrompt::default();
        while let Some(line) = futures::StreamExt::next(&mut lines).await {
            let Ok(line) = line else {
                break;
            };
            let changed = prompt.read(&line);
            output.push(line);
            if changed {
                on_prompt(prompt.clone());
                // `sbx login` waits for Enter before it opens the browser.
                if let Some(mut input) = stdin.take() {
                    input.write_all(b"\n").await.ok();
                    input.flush().await.ok();
                    stdin = Some(input);
                }
            }
        }
        let status = child.status().await.map_err(|source| SbxError::Spawn {
            command: command.to_string(),
            source,
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(SbxError::CommandFailed {
                command: command.to_string(),
                message: output
                    .iter()
                    .rev()
                    .map(|line| line.trim())
                    .find(|line| !line.is_empty())
                    .unwrap_or("sign-in failed")
                    .to_string(),
            })
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

    /// Makes Claude Code in the sandbox report its limits, cost and context through its status
    /// line, so the Sandbox panel can show them. Safe to run again.
    pub async fn install_claude_status_hook(&self, name: &str) -> Result<(), SbxError> {
        let script =
            CLAUDE_STATUS_HOOK_INSTALLER.replace("SCRIPT", CLAUDE_STATUSLINE_SCRIPT.trim_end());
        let output = self.exec(name, &["bash", "-lc", script.as_str()]).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(classify_failure(
                "install the Claude status line",
                &output.stderr,
            ))
        }
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

/// Parses `agent|<modified unix time>|<base64 JSON>` from the probe script.
fn parse_agent_status(value: &str) -> Option<AgentUsage> {
    use base64::Engine as _;

    let mut parts = value.splitn(3, '|');
    let agent = parts.next()?;
    let modified = parts
        .next()
        .and_then(|time| time.trim().parse::<i64>().ok());
    let json = base64::engine::general_purpose::STANDARD
        .decode(parts.next()?.trim())
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let mut usage = match agent {
        "claude" => parse_claude_status(&json),
        "codex" => parse_codex_event(&json),
        _ => return None,
    };
    usage.updated_at = usage.updated_at.or(modified);
    Some(usage)
}

/// The JSON Claude Code passes to its status line command.
fn parse_claude_status(json: &serde_json::Value) -> AgentUsage {
    let number = |pointer: &str| json.pointer(pointer).and_then(serde_json::Value::as_f64);
    let window = |key: &str, label: &str| {
        Some(UsageWindow {
            label: label.to_string(),
            used_percent: number(&format!("/rate_limits/{key}/used_percentage"))?,
            resets_at: json
                .pointer(&format!("/rate_limits/{key}/resets_at"))
                .and_then(serde_json::Value::as_i64),
        })
    };
    AgentUsage {
        agent: "claude".to_string(),
        plan: None,
        model: json
            .pointer("/model/display_name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        windows: [
            window("five_hour", "5-hour limit"),
            window("seven_day", "Weekly limit"),
        ]
        .into_iter()
        .flatten()
        .collect(),
        session_cost_usd: number("/cost/total_cost_usd"),
        context_used_percent: number("/context_window/used_percentage"),
        session_tokens: None,
        updated_at: None,
    }
}

/// A `token_count` event from a Codex session log.
fn parse_codex_event(json: &serde_json::Value) -> AgentUsage {
    let limits = json.pointer("/payload/rate_limits");
    let window = |key: &str| {
        let window = limits?.get(key)?;
        let minutes = window.get("window_minutes")?.as_u64()?;
        Some(UsageWindow {
            label: match minutes {
                300 => "5-hour limit".to_string(),
                10080 => "Weekly limit".to_string(),
                minutes if minutes % 1440 == 0 => format!("{}-day limit", minutes / 1440),
                minutes => format!("{}-hour limit", minutes / 60),
            },
            used_percent: window.get("used_percent")?.as_f64()?,
            resets_at: window.get("resets_at").and_then(serde_json::Value::as_i64),
        })
    };
    let info = json.pointer("/payload/info");
    let context_window = info
        .and_then(|info| info.get("model_context_window"))
        .and_then(serde_json::Value::as_f64);
    let last_input = info
        .and_then(|info| info.pointer("/last_token_usage/input_tokens"))
        .and_then(serde_json::Value::as_f64);
    AgentUsage {
        agent: "codex".to_string(),
        plan: limits
            .and_then(|limits| limits.get("plan_type"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        model: None,
        windows: [window("primary"), window("secondary")]
            .into_iter()
            .flatten()
            .collect(),
        session_cost_usd: None,
        context_used_percent: context_window
            .zip(last_input)
            .filter(|(window, _)| *window > 0.0)
            .map(|(window, input)| (input / window * 100.0).min(100.0)),
        session_tokens: info
            .and_then(|info| info.pointer("/total_token_usage/total_tokens"))
            .and_then(serde_json::Value::as_u64),
        updated_at: json
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_rfc3339_seconds),
    }
}

/// Seconds since the Unix epoch of an RFC 3339 UTC timestamp such as `2026-11-05T13:06:25.653Z`.
fn parse_rfc3339_seconds(timestamp: &str) -> Option<i64> {
    let (date, time) = timestamp.trim_end_matches('Z').split_once('T')?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let mut time = time.split(':');
    let hours = time.next()?.parse::<i64>().ok()?;
    let minutes = time.next()?.parse::<i64>().ok()?;
    let seconds = time.next()?.split('.').next()?.parse::<i64>().ok()?;
    // Days from civil, Howard Hinnant's algorithm.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146097 + day_of_era - 719468;
    Some(days * 86400 + hours * 3600 + minutes * 60 + seconds)
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
            "claude_status_hook" => probe.claude_status_hook = value == "1",
            "agent_status" => {
                if let Some(usage) = parse_agent_status(value) {
                    probe
                        .agent_usage
                        .retain(|existing| existing.agent != usage.agent);
                    probe.agent_usage.push(usage);
                }
            }
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

/// Runs each request in a fresh `bash` and answers with one line, so binary and multi-line
/// output survive: `<id> <exit status> <base64 stdout> <base64 stderr>`.
const SESSION_LOOP: &str = r#"
while IFS=' ' read -r request_id request_script; do
  request_dir=$(mktemp -d)
  printf '%s' "$request_script" | base64 -d > "$request_dir/script"
  bash "$request_dir/script" > "$request_dir/stdout" 2> "$request_dir/stderr" < /dev/null
  request_status=$?
  printf '%s %s %s %s\n' "$request_id" "$request_status" "$(base64 -w0 < "$request_dir/stdout")" "$(base64 -w0 < "$request_dir/stderr")"
  rm -rf "$request_dir"
done
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ShellOutput {
    pub fn success(&self) -> bool {
        self.status == 0
    }

    /// The first non-empty line of stderr, for error messages.
    pub fn error_line(&self) -> Option<String> {
        first_error_line(&self.stderr)
    }
}

/// A login shell kept running inside a sandbox. Every `sbx exec` costs about a second before the
/// command starts, which is too slow for panels that query the sandbox on every refresh.
pub struct ShellSession {
    _child: util::command::Child,
    stdin: Box<dyn futures::AsyncWrite + Send + Unpin>,
    stdout: futures::io::BufReader<Box<dyn futures::AsyncRead + Send + Unpin>>,
    next_request: u64,
}

impl ShellSession {
    pub async fn start(sbx: &Sbx, name: &str) -> Result<Self, SbxError> {
        let arguments = ["exec", "-i", name, "bash", "-lc", SESSION_LOOP];
        let mut child = new_command(&sbx.executable)
            .args(arguments)
            .stdin(util::command::Stdio::piped())
            .stdout(util::command::Stdio::piped())
            .stderr(util::command::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| SbxError::Spawn {
                command: display_command(&arguments[..4]),
                source,
            })?;
        let closed = || SbxError::CommandFailed {
            command: display_command(&arguments[..4]),
            message: "the sandbox shell has no pipes".to_string(),
        };
        let stdin: Box<dyn futures::AsyncWrite + Send + Unpin> =
            Box::new(child.stdin.take().ok_or_else(closed)?);
        let stdout: Box<dyn futures::AsyncRead + Send + Unpin> =
            Box::new(child.stdout.take().ok_or_else(closed)?);
        Ok(Self {
            _child: child,
            stdin,
            stdout: futures::io::BufReader::new(stdout),
            next_request: 0,
        })
    }

    /// Runs `script` with `bash` inside the sandbox. A failed command is an `Ok` output with a
    /// non-zero status; an `Err` means the session itself broke and must be started again.
    pub async fn run(&mut self, script: &str) -> Result<ShellOutput, SbxError> {
        use base64::Engine as _;
        use futures::{AsyncBufReadExt as _, AsyncWriteExt as _};

        let broken = |message: String| SbxError::CommandFailed {
            command: "sbx exec -i bash".to_string(),
            message,
        };
        self.next_request += 1;
        let request = self.next_request;
        let encoded = base64::engine::general_purpose::STANDARD.encode(script);
        self.stdin
            .write_all(format!("{request} {encoded}\n").as_bytes())
            .await
            .map_err(|error| broken(format!("writing to the sandbox shell failed: {error}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| broken(format!("writing to the sandbox shell failed: {error}")))?;
        loop {
            let mut line = String::new();
            let read =
                self.stdout.read_line(&mut line).await.map_err(|error| {
                    broken(format!("reading the sandbox shell failed: {error}"))
                })?;
            if read == 0 {
                return Err(broken("the sandbox shell exited".to_string()));
            }
            if let Some(output) = parse_session_response(request, &line)? {
                return Ok(output);
            }
        }
    }
}

/// Parses a response line. Lines for other requests, such as a request whose caller was
/// cancelled before reading its answer, are skipped by returning `None`.
fn parse_session_response(request: u64, line: &str) -> Result<Option<ShellOutput>, SbxError> {
    use base64::Engine as _;

    let invalid = || SbxError::CommandFailed {
        command: "sbx exec -i bash".to_string(),
        message: "the sandbox shell sent an invalid response".to_string(),
    };
    let mut parts = line.trim_end_matches('\n').splitn(4, ' ');
    let id = parts.next().and_then(|id| id.parse::<u64>().ok());
    if id != Some(request) {
        return Ok(None);
    }
    let status = parts
        .next()
        .and_then(|status| status.parse::<i32>().ok())
        .ok_or_else(invalid)?;
    let decode = |part: Option<&str>| {
        base64::engine::general_purpose::STANDARD
            .decode(part.unwrap_or_default())
            .map_err(|_| invalid())
    };
    let stdout = decode(parts.next())?;
    let stderr = decode(parts.next())?;
    Ok(Some(ShellOutput {
        status,
        stdout,
        stderr,
    }))
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
        .find(|line| {
            !line.is_empty()
                && !line.starts_with('✓')
                && !line.starts_with("try:")
                // `sbx` prints warnings, such as an expired sentinel session, before the error.
                && !line.starts_with("WARN:")
        })
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

    #[test]
    fn reads_login_prompts() {
        let mut prompt = LoginPrompt::default();
        assert!(!prompt.read("Signing in to Docker…"));
        assert!(prompt.read("Your one-time device confirmation code is: XK7Q-M2PL"));
        assert!(prompt.read(
            "Press ENTER to open your browser or submit your device code here: https://login.docker.com/activate"
        ));
        assert!(!prompt.read("Your one-time device confirmation code is: XK7Q-M2PL"));
        assert_eq!(
            prompt,
            LoginPrompt {
                code: Some("XK7Q-M2PL".to_string()),
                url: Some("https://login.docker.com/activate".to_string()),
            }
        );
        assert!(!is_device_code("non-code"));
    }

    #[test]
    fn parses_agent_usage() {
        use base64::Engine as _;

        let encode = |json: &str| base64::engine::general_purpose::STANDARD.encode(json);
        let claude = r#"{"model":{"display_name":"Opus"},"cost":{"total_cost_usd":1.25},
            "context_window":{"used_percentage":23},
            "rate_limits":{"five_hour":{"used_percentage":38.5,"resets_at":1793901132},
            "seven_day":{"used_percentage":28,"resets_at":1794421046}}}"#;
        let probe = parse_probe(&format!(
            "agent_status=claude|1793900000|{}\nclaude_status_hook=1\n",
            encode(claude)
        ));
        assert!(probe.claude_status_hook);
        assert_eq!(probe.agent_usage.len(), 1);
        let usage = &probe.agent_usage[0];
        assert_eq!(usage.model.as_deref(), Some("Opus"));
        assert_eq!(usage.session_cost_usd, Some(1.25));
        assert_eq!(usage.updated_at, Some(1793900000));
        assert_eq!(
            usage.windows,
            vec![
                UsageWindow {
                    label: "5-hour limit".to_string(),
                    used_percent: 38.5,
                    resets_at: Some(1793901132),
                },
                UsageWindow {
                    label: "Weekly limit".to_string(),
                    used_percent: 28.0,
                    resets_at: Some(1794421046),
                },
            ]
        );

        let codex = r#"{"timestamp":"2026-11-05T13:06:25.653Z","type":"event_msg","payload":{
            "type":"token_count","info":{"total_token_usage":{"total_tokens":5000},
            "last_token_usage":{"input_tokens":64000},"model_context_window":256000},
            "rate_limits":{"primary":{"used_percent":59.0,"window_minutes":300,"resets_at":1793901132},
            "secondary":{"used_percent":41.0,"window_minutes":10080,"resets_at":1794421046},
            "plan_type":"plus"}}}"#;
        let probe = parse_probe(&format!("agent_status=codex|1|{}\n", encode(codex)));
        let usage = &probe.agent_usage[0];
        assert_eq!(usage.plan.as_deref(), Some("plus"));
        assert_eq!(usage.windows[0].label, "5-hour limit");
        assert_eq!(usage.windows[1].used_percent, 41.0);
        assert_eq!(usage.context_used_percent, Some(25.0));
        assert_eq!(usage.session_tokens, Some(5000));
        assert_eq!(usage.updated_at, Some(1793883985));
        assert_eq!(parse_rfc3339_seconds("1970-01-02T00:00:01Z"), Some(86401));
        assert!(!parse_probe("").claude_status_hook);
    }

    #[test]
    fn parses_session_responses() -> TestResult {
        assert_eq!(parse_session_response(2, "1 0 YQ== \n")?, None);
        assert_eq!(
            parse_session_response(2, "2 3 b3V0 ZXJy\n")?,
            Some(ShellOutput {
                status: 3,
                stdout: b"out".to_vec(),
                stderr: b"err".to_vec(),
            })
        );
        assert_eq!(
            parse_session_response(2, "2 0  \n")?,
            Some(ShellOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        );
        assert!(parse_session_response(2, "2 zero  \n").is_err());
        Ok(())
    }

    /// Installs the Claude status hook in the `sands-k8s-lab` sandbox, feeds the status line a
    /// sample report and reads it back with the probe: `cargo test -p sbx -- --ignored`.
    #[test]
    #[ignore]
    fn reports_claude_usage_from_a_real_sandbox() -> TestResult {
        smol::block_on(async {
            let sbx = Sbx::locate()?;
            let name = "sands-k8s-lab";
            sbx.install_claude_status_hook(name).await?;
            sbx.install_claude_status_hook(name).await?;
            let sample = r#"{"model":{"display_name":"Opus"},"rate_limits":{"five_hour":{"used_percentage":12,"resets_at":1793901132}}}"#;
            let output = sbx
                .exec(
                    name,
                    &[
                        "sh",
                        "-c",
                        &format!("printf '%s' '{sample}' | $HOME/.sands/claude-statusline.sh"),
                    ],
                )
                .await?;
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                "Opus · 5h 12%"
            );
            let probe = sbx.probe(name).await?;
            assert!(probe.claude_status_hook);
            let usage = probe
                .agent_usage
                .iter()
                .find(|usage| usage.agent == "claude")
                .ok_or("no Claude usage")?;
            assert_eq!(usage.windows[0].used_percent, 12.0);
            assert!(usage.updated_at.is_some());
            sbx.exec(
                name,
                &[
                    "rm",
                    "-rf",
                    "/home/agent/.sands/agent-status",
                    "/home/agent/.claude/settings.json",
                ],
            )
            .await?;
            Ok(())
        })
    }

    /// Runs a shell session in a real sandbox: `cargo test -p sbx -- --ignored`.
    #[test]
    #[ignore]
    fn runs_commands_in_a_shell_session() -> TestResult {
        smol::block_on(async {
            let sbx = Sbx::locate()?;
            let name = sbx
                .list()
                .await?
                .into_iter()
                .next()
                .map(|sandbox| sandbox.name)
                .ok_or("no sandbox to test with")?;
            let mut session = ShellSession::start(&sbx, &name).await?;
            let output = session.run("printf 'a\\nb'; echo oops >&2; exit 4").await?;
            assert_eq!(output.status, 4);
            assert_eq!(output.stdout, b"a\nb");
            assert_eq!(output.stderr, b"oops\n");
            let output = session.run("head -c 300000 /dev/zero").await?;
            assert!(output.success());
            assert_eq!(output.stdout.len(), 300000);
            Ok(())
        })
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
