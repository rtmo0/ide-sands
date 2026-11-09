use std::{
    fmt, fs,
    io::Write as _,
    net::IpAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use sbx::{NetworkDecision, Sbx};

mod cluster;
mod details;
mod overview;
mod resources;
mod table;

pub use cluster::{
    Cluster, ClusterStatus, ContextInfo, HelmRelease, Listing, MANIFEST_DIRECTORY, Revision, Usage, shell_quote,
};
pub use details::{
    Condition, ContainerInfo, DataEntry, Details, EventRow, KeyValue, Section, ServicePort,
    UsageRow, build_details, build_helm_details,
};
pub use overview::{Overview, ResourceTotals, Warning, WorkloadSummary, build_overview};
pub use resources::{
    Category, HELM_RELEASES, ObjectRef, ResourceKind, builtin_kind, builtin_kinds, kind_for,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;
pub use table::{
    Action, Cell, Column, Resource, Row, Table, TableContext, Tone, build_table, format_age,
    format_bytes, format_cpu, parse_quantity,
};
use tempfile::NamedTempFile;
use url::Url;

const SANDBOX_KUBECONFIG: &str = "/home/agent/.kube/config";
const KUBECTL_INSTALL_SCRIPT: &str = r#"set -e
if ! command -v kubectl >/dev/null 2>&1; then
  architecture=$(dpkg --print-architecture)
  case "$architecture" in amd64) kubectl_arch=amd64 ;; arm64) kubectl_arch=arm64 ;; *) echo "unsupported architecture: $architecture" >&2; exit 1 ;; esac
  kubectl_version=$(curl -fsSL https://dl.k8s.io/release/stable.txt)
  curl -fsSL "https://dl.k8s.io/release/$kubectl_version/bin/linux/$kubectl_arch/kubectl" -o /tmp/sands-kubectl
  sudo install -m 0755 /tmp/sands-kubectl /usr/local/bin/kubectl
  rm -f /tmp/sands-kubectl
fi
if ! command -v helm >/dev/null 2>&1; then
  curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash >/dev/null
fi
mkdir -p "$HOME/.kube"
chmod 700 "$HOME/.kube"
chmod 600 "$HOME/.kube/config"
"#;

pub struct PreparedConnection {
    pub context: String,
    pub server: String,
    pub namespace: String,
    host: String,
    network_resource: String,
    placeholder: String,
    token: String,
    kubeconfig: String,
}

impl fmt::Debug for PreparedConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedConnection")
            .field("context", &self.context)
            .field("server", &self.server)
            .field("namespace", &self.namespace)
            .field("host", &self.host)
            .field("network_resource", &self.network_resource)
            .field("placeholder", &self.placeholder)
            .field("token", &"[redacted]")
            .field("kubeconfig", &self.kubeconfig)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KubernetesOverview {
    pub context: String,
    pub server: String,
    pub namespace: String,
    pub version: String,
    pub ready_nodes: usize,
    pub total_nodes: Option<usize>,
    pub running_pods: usize,
    pub total_pods: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Kubeconfig {
    #[serde(default = "api_version")]
    api_version: String,
    #[serde(default = "config_kind")]
    kind: String,
    #[serde(default)]
    preferences: YamlValue,
    #[serde(default)]
    clusters: Vec<NamedCluster>,
    #[serde(default)]
    users: Vec<NamedUser>,
    #[serde(default)]
    contexts: Vec<NamedContext>,
    #[serde(default, rename = "current-context")]
    current_context: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct NamedCluster {
    name: String,
    cluster: KubeconfigCluster,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct KubeconfigCluster {
    server: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    certificate_authority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    certificate_authority_data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    insecure_skip_tls_verify: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tls_server_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disable_compression: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proxy_url: Option<String>,
    #[serde(flatten)]
    other: std::collections::BTreeMap<String, YamlValue>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct NamedUser {
    name: String,
    user: User,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct User {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, rename = "tokenFile", skip_serializing_if = "Option::is_none")]
    token_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_certificate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_certificate_data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_key_data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exec: Option<YamlValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_provider: Option<YamlValue>,
    #[serde(flatten)]
    other: std::collections::BTreeMap<String, YamlValue>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct NamedContext {
    name: String,
    context: KubeContext,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct KubeContext {
    cluster: String,
    user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    #[serde(flatten)]
    other: std::collections::BTreeMap<String, YamlValue>,
}

fn api_version() -> String {
    "v1".to_string()
}

fn config_kind() -> String {
    "Config".to_string()
}

/// A context of a kubeconfig file on the host, and why Sands cannot connect it, if it cannot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KubeconfigEntry {
    pub context: String,
    pub server: String,
    pub namespace: String,
    pub current: bool,
    pub problem: Option<String>,
}

/// Lists the contexts of a kubeconfig file so the user can pick which ones to connect.
pub fn read_kubeconfig_contexts(path: &Path) -> Result<Vec<KubeconfigEntry>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read kubeconfig {}", path.display()))?;
    let base_directory = path.parent().unwrap_or_else(|| Path::new("."));
    kubeconfig_contexts(&contents, base_directory)
}

fn kubeconfig_contexts(contents: &str, base_directory: &Path) -> Result<Vec<KubeconfigEntry>> {
    let config: Kubeconfig =
        serde_yaml::from_str(contents).context("the kubeconfig is not valid YAML")?;
    if config.contexts.is_empty() {
        bail!("the kubeconfig contains no contexts");
    }
    Ok(config
        .contexts
        .iter()
        .map(|context| KubeconfigEntry {
            context: context.name.clone(),
            server: config
                .clusters
                .iter()
                .find(|cluster| cluster.name == context.context.cluster)
                .map(|cluster| cluster.cluster.server.clone())
                .unwrap_or_default(),
            namespace: context
                .context
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            current: context.name == config.current_context,
            problem: PreparedConnection::from_config(&config, base_directory, Some(&context.name))
                .err()
                .map(|error| format!("{error:#}")),
        })
        .collect())
}

impl PreparedConnection {
    pub fn from_file(path: &Path, context: &str) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read kubeconfig {}", path.display()))?;
        let base_directory = path.parent().unwrap_or_else(|| Path::new("."));
        let config: Kubeconfig =
            serde_yaml::from_str(&contents).context("the kubeconfig is not valid YAML")?;
        Self::from_config(&config, base_directory, Some(context))
    }

    pub fn from_yaml(contents: &str, base_directory: &Path) -> Result<Self> {
        let config: Kubeconfig =
            serde_yaml::from_str(contents).context("the kubeconfig is not valid YAML")?;
        Self::from_config(&config, base_directory, None)
    }

    /// Prepares one context of `config`; without a requested context, the current one.
    fn from_config(
        config: &Kubeconfig,
        base_directory: &Path,
        requested_context: Option<&str>,
    ) -> Result<Self> {
        let context_name = match requested_context {
            Some(context) => context.to_string(),
            None if config.current_context.is_empty() => match config.contexts.as_slice() {
                [context] => context.name.clone(),
                [] => bail!("the kubeconfig contains no contexts"),
                _ => bail!("the kubeconfig has no current-context; select one before importing"),
            },
            None => config.current_context.clone(),
        };
        let mut context = config
            .contexts
            .iter()
            .find(|context| context.name == context_name)
            .cloned()
            .with_context(|| format!("context {context_name:?} is missing"))?;
        let mut cluster = config
            .clusters
            .iter()
            .find(|cluster| cluster.name == context.context.cluster)
            .cloned()
            .with_context(|| format!("cluster {:?} is missing", context.context.cluster))?;
        let mut user = config
            .users
            .iter()
            .find(|user| user.name == context.context.user)
            .cloned()
            .with_context(|| format!("user {:?} is missing", context.context.user))?;

        inline_certificate_authority(&mut cluster.cluster, base_directory)?;
        let token = take_token(&mut user.user, base_directory)?;
        if cluster.cluster.proxy_url.is_some() {
            bail!("proxy-url kubeconfigs are not supported yet");
        }
        cluster.cluster.other.clear();
        user.user.other.clear();
        let server_url = Url::parse(&cluster.cluster.server)
            .with_context(|| format!("invalid Kubernetes API URL {:?}", cluster.cluster.server))?;
        if server_url.scheme() != "https" {
            bail!("only HTTPS Kubernetes API servers are supported");
        }
        let host = server_url
            .host_str()
            .context("the Kubernetes API URL has no host")?
            .to_string();
        if host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        {
            bail!(
                "the Kubernetes API uses a loopback address that is not reachable from the project sandbox"
            );
        }
        let port = server_url.port_or_known_default().unwrap_or(443);
        let network_resource = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let placeholder = placeholder_for(&context_name);
        user.user.token = Some(placeholder.clone());
        // Contexts from different files often share cluster and user names ("default",
        // "kind-kind"); naming both after the context keeps the merged sandbox kubeconfig apart.
        cluster.name = context_name.clone();
        user.name = context_name.clone();
        context.context.cluster = context_name.clone();
        context.context.user = context_name.clone();

        let namespace = context
            .context
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let sanitized = Kubeconfig {
            api_version: config.api_version.clone(),
            kind: config.kind.clone(),
            preferences: config.preferences.clone(),
            clusters: vec![cluster],
            users: vec![user],
            contexts: vec![context],
            current_context: context_name.clone(),
        };
        let kubeconfig = serde_yaml::to_string(&sanitized)
            .context("failed to serialize the project kubeconfig")?;
        Ok(Self {
            context: context_name,
            server: server_url.to_string(),
            namespace,
            host,
            network_resource,
            placeholder,
            token,
            kubeconfig,
        })
    }

    /// Adds this context to the sandbox kubeconfig, next to the clusters already connected, and
    /// makes it the current one. The token stays on the host; the sbx proxy substitutes it.
    pub async fn provision(&self, sbx: &Sbx, sandbox_name: &str) -> Result<()> {
        let existing = read_sandbox_kubeconfig(sbx, sandbox_name).await?;
        let previous_placeholder = existing
            .as_ref()
            .and_then(|config| user_token_for_context(config, &self.context));
        let network_rules = sbx.policy_rules(sandbox_name).await?;
        let network_is_allowed = network_rules.rules.iter().any(|rule| {
            rule.decision == "allow"
                && rule.resource_type == "network"
                && rule.resources.contains(&self.network_resource)
        });
        if !network_is_allowed {
            sbx.add_network_rule(sandbox_name, NetworkDecision::Allow, &self.network_resource)
                .await?;
        }
        let token_file = store_token(sandbox_name, &self.context, &self.token)?;
        sbx.set_custom_secret_from_file(sandbox_name, &self.host, &self.placeholder, &token_file)
            .await?;

        let addition: Kubeconfig = serde_yaml::from_str(&self.kubeconfig)
            .context("the prepared kubeconfig is not valid YAML")?;
        let merged = merge_kubeconfig(existing, addition);
        write_sandbox_kubeconfig(sbx, sandbox_name, &merged).await?;
        let output = sbx
            .exec(sandbox_name, &["bash", "-lc", KUBECTL_INSTALL_SCRIPT])
            .await?;
        if !output.status.success() {
            bail!(
                "installing kubectl failed: {}",
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("no error output")
            );
        }
        probe(sbx, sandbox_name)
            .await?
            .context("the Kubernetes connection was not created")?;
        if let Some(previous_placeholder) = previous_placeholder
            && previous_placeholder != self.placeholder
            && !placeholder_in_use(&merged, &previous_placeholder)
        {
            sbx.remove_custom_secret(sandbox_name, &previous_placeholder)
                .await?;
        }
        Ok(())
    }
}

/// Removes one context from the sandbox kubeconfig together with the token secret behind it.
/// If it was the current context, another connected one becomes current.
pub async fn disconnect_context(sbx: &Sbx, sandbox_name: &str, context: &str) -> Result<()> {
    let Some(mut config) = read_sandbox_kubeconfig(sbx, sandbox_name).await? else {
        return Ok(());
    };
    let placeholder = user_token_for_context(&config, context);
    remove_context(&mut config, context);
    write_sandbox_kubeconfig(sbx, sandbox_name, &config).await?;
    if let Some(placeholder) = placeholder
        && !placeholder_in_use(&config, &placeholder)
    {
        sbx.remove_custom_secret(sandbox_name, &placeholder).await?;
    }
    remove_stored_token(sandbox_name, context)?;
    Ok(())
}

fn placeholder_for(context: &str) -> String {
    let sanitized = context
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("sands-k8s-{sanitized}")
}

async fn read_sandbox_kubeconfig(sbx: &Sbx, sandbox_name: &str) -> Result<Option<Kubeconfig>> {
    let output = sbx
        .exec(sandbox_name, &["cat", SANDBOX_KUBECONFIG])
        .await?;
    if !output.status.success() || output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let config: Kubeconfig = serde_yaml::from_slice(&output.stdout)
        .context("the project kubeconfig in the sandbox is not valid YAML")?;
    Ok(Some(config))
}

async fn write_sandbox_kubeconfig(sbx: &Sbx, sandbox_name: &str, config: &Kubeconfig) -> Result<()> {
    let contents =
        serde_yaml::to_string(config).context("failed to serialize the project kubeconfig")?;
    let mut file = NamedTempFile::new().context("failed to create a temporary kubeconfig")?;
    file.write_all(contents.as_bytes())
        .context("failed to write the temporary kubeconfig")?;
    file.flush()
        .context("failed to flush the temporary kubeconfig")?;
    sbx.exec(sandbox_name, &["mkdir", "-p", "/home/agent/.kube"])
        .await?;
    sbx.copy_to(sandbox_name, file.path(), SANDBOX_KUBECONFIG)
        .await?;
    Ok(())
}

/// The token placeholder of the user behind `context`, when Sands created it.
fn user_token_for_context(config: &Kubeconfig, context: &str) -> Option<String> {
    let user = &config
        .contexts
        .iter()
        .find(|candidate| candidate.name == context)?
        .context
        .user;
    config
        .users
        .iter()
        .find(|candidate| &candidate.name == user)?
        .user
        .token
        .clone()
        .filter(|token| token.starts_with("sands-k8s-"))
}

fn placeholder_in_use(config: &Kubeconfig, placeholder: &str) -> bool {
    config
        .users
        .iter()
        .any(|user| user.user.token.as_deref() == Some(placeholder))
}

fn merge_kubeconfig(existing: Option<Kubeconfig>, addition: Kubeconfig) -> Kubeconfig {
    let Some(mut merged) = existing else {
        return addition;
    };
    for cluster in addition.clusters {
        merged
            .clusters
            .retain(|candidate| candidate.name != cluster.name);
        merged.clusters.push(cluster);
    }
    for user in addition.users {
        merged.users.retain(|candidate| candidate.name != user.name);
        merged.users.push(user);
    }
    for context in addition.contexts {
        merged
            .contexts
            .retain(|candidate| candidate.name != context.name);
        merged.contexts.push(context);
    }
    merged.current_context = addition.current_context;
    prune_unreferenced(&mut merged);
    merged
}

fn remove_context(config: &mut Kubeconfig, context: &str) {
    config.contexts.retain(|candidate| candidate.name != context);
    if config.current_context == context {
        config.current_context = config
            .contexts
            .first()
            .map(|context| context.name.clone())
            .unwrap_or_default();
    }
    prune_unreferenced(config);
}

/// Drops clusters and users no context refers to, such as those of a replaced import.
fn prune_unreferenced(config: &mut Kubeconfig) {
    let contexts = &config.contexts;
    config.clusters.retain(|cluster| {
        contexts
            .iter()
            .any(|context| context.context.cluster == cluster.name)
    });
    config
        .users
        .retain(|user| contexts.iter().any(|context| context.context.user == user.name));
}

fn token_root() -> PathBuf {
    paths::data_dir().join("secrets").join("kubernetes")
}

fn store_token(sandbox_name: &str, context: &str, token: &str) -> Result<PathBuf> {
    store_token_in(&token_root(), sandbox_name, context, token)
}

fn remove_stored_token(sandbox_name: &str, context: &str) -> Result<()> {
    let path = token_path_in(&token_root(), sandbox_name, context);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove Kubernetes token {}", path.display())),
    }
}

fn token_path_in(root: &Path, sandbox_name: &str, context: &str) -> PathBuf {
    root.join(URL_SAFE_NO_PAD.encode(sandbox_name.as_bytes()))
        .join(format!("{}.token", URL_SAFE_NO_PAD.encode(context.as_bytes())))
}

fn store_token_in(root: &Path, sandbox_name: &str, context: &str, token: &str) -> Result<PathBuf> {
    let token_path = token_path_in(root, sandbox_name, context);
    let sandbox_directory = token_path
        .parent()
        .context("the Kubernetes token path has no directory")?
        .to_path_buf();
    fs::create_dir_all(&sandbox_directory).with_context(|| {
        format!(
            "failed to create Kubernetes secret directory {}",
            sandbox_directory.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(&sandbox_directory, fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "failed to secure Kubernetes secret directory {}",
                    sandbox_directory.display()
                )
            },
        )?;
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    let mut token_file = options.open(&token_path).with_context(|| {
        format!(
            "failed to open Kubernetes token file {}",
            token_path.display()
        )
    })?;
    token_file.write_all(token.as_bytes()).with_context(|| {
        format!(
            "failed to write Kubernetes token file {}",
            token_path.display()
        )
    })?;
    token_file.flush().with_context(|| {
        format!(
            "failed to flush Kubernetes token file {}",
            token_path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        token_file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to secure Kubernetes token file {}",
                    token_path.display()
                )
            })?;
    }
    Ok(token_path)
}

fn inline_certificate_authority(
    cluster: &mut KubeconfigCluster,
    base_directory: &Path,
) -> Result<()> {
    let Some(path) = cluster.certificate_authority.take() else {
        return Ok(());
    };
    if cluster.certificate_authority_data.is_some() {
        return Ok(());
    }
    let path = resolve_path(base_directory, &path);
    let certificate = fs::read(&path)
        .with_context(|| format!("failed to read certificate authority {}", path.display()))?;
    cluster.certificate_authority_data = Some(STANDARD.encode(certificate));
    Ok(())
}

fn take_token(user: &mut User, base_directory: &Path) -> Result<String> {
    if user.client_certificate.is_some()
        || user.client_certificate_data.is_some()
        || user.client_key.is_some()
        || user.client_key_data.is_some()
    {
        bail!(
            "client-certificate authentication is not supported yet; use a service-account token"
        );
    }
    if user.exec.is_some() || user.auth_provider.is_some() {
        bail!(
            "exec and auth-provider authentication are not supported yet; export a service-account token kubeconfig"
        );
    }
    if let Some(token) = user.token.take().filter(|token| !token.trim().is_empty()) {
        return Ok(token);
    }
    let path = user
        .token_file
        .take()
        .context("the selected kubeconfig user has no bearer token")?;
    let path = resolve_path(base_directory, &path);
    let token = fs::read_to_string(&path)
        .with_context(|| format!("failed to read token file {}", path.display()))
        .map(|token| token.trim().to_string())?;
    if token.is_empty() {
        bail!("the selected kubeconfig user's token file is empty");
    }
    Ok(token)
}

fn resolve_path(base_directory: &Path, path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home_directory) = dirs::home_dir()
    {
        return home_directory.join(relative);
    }
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_directory.join(path)
    }
}

/// A short summary of the sandbox's Kubernetes connection, or `None` without one.
pub async fn probe(sbx: &Sbx, sandbox_name: &str) -> Result<Option<KubernetesOverview>> {
    let cluster = Cluster::new(sbx.clone(), sandbox_name);
    let Some(context) = cluster.context().await? else {
        return Ok(None);
    };
    let commands = [
        vec!["version", "-o", "json"],
        vec!["get", "nodes", "-o", "json"],
        vec!["get", "pods", "--all-namespaces", "-o", "json"],
    ]
    .map(|command| command.into_iter().map(str::to_string).collect::<Vec<_>>());
    let outputs = cluster.kubectl_batch(&commands).await?;
    let output = |index: usize| outputs.get(index).filter(|output| output.success());
    let version = output(0)
        .and_then(|output| serde_json::from_slice::<JsonValue>(&output.stdout).ok())
        .and_then(|version| {
            version
                .pointer("/serverVersion/gitVersion")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());
    let nodes = output(1).map(cluster::parse_list_output);
    let pods = output(2).map(cluster::parse_list_output);
    Ok(Some(KubernetesOverview {
        context: context.context,
        server: context.server,
        namespace: context.namespace,
        version,
        ready_nodes: nodes
            .iter()
            .flatten()
            .filter(|node| table::node_ready(node))
            .count(),
        total_nodes: nodes.as_ref().map(Vec::len),
        running_pods: pods
            .iter()
            .flatten()
            .filter(|pod| {
                pod.pointer("/status/phase").and_then(JsonValue::as_str) == Some("Running")
            })
            .count(),
        total_pods: pods.as_ref().map(Vec::len),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_only_the_current_context_and_hides_the_token() -> Result<()> {
        let yaml = r#"
apiVersion: v1
kind: Config
clusters:
  - name: production-cluster
    cluster:
      server: https://api.example.com:6443
      certificate-authority-data: Y2E=
  - name: unrelated
    cluster:
      server: https://other.example.com
users:
  - name: production-user
    user:
      token: secret-token
      username: leaked-user
      password: leaked-password
  - name: unrelated
    user:
      token: other-secret
contexts:
  - name: production
    context:
      cluster: production-cluster
      user: production-user
      namespace: apps
  - name: unrelated
    context:
      cluster: unrelated
      user: unrelated
current-context: production
"#;
        let connection = PreparedConnection::from_yaml(yaml, Path::new("/tmp"))?;
        assert_eq!(connection.context, "production");
        assert_eq!(connection.namespace, "apps");
        assert_eq!(connection.network_resource, "api.example.com:6443");
        assert!(!connection.kubeconfig.contains("secret-token"));
        assert!(!connection.kubeconfig.contains("leaked-user"));
        assert!(!connection.kubeconfig.contains("leaked-password"));
        assert!(!connection.kubeconfig.contains("other-secret"));
        assert!(!connection.kubeconfig.contains("other.example.com"));
        assert!(connection.kubeconfig.contains("sands-k8s-production"));
        assert!(!format!("{connection:?}").contains("secret-token"));
        Ok(())
    }

    #[test]
    fn inlines_relative_certificate_authority_and_token_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("ca.pem"), "certificate")?;
        fs::write(directory.path().join("token"), "token-from-file\n")?;
        let yaml = r#"
clusters:
  - name: cluster
    cluster:
      server: https://api.example.com
      certificate-authority: ca.pem
users:
  - name: user
    user:
      tokenFile: token
contexts:
  - name: context
    context: { cluster: cluster, user: user }
current-context: context
"#;
        let connection = PreparedConnection::from_yaml(yaml, directory.path())?;
        assert_eq!(connection.token, "token-from-file");
        assert!(
            connection
                .kubeconfig
                .contains(&STANDARD.encode("certificate"))
        );
        assert!(!connection.kubeconfig.contains("ca.pem"));
        Ok(())
    }

    #[test]
    fn stores_tokens_outside_sandbox_named_paths() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let token_path = store_token_in(
            directory.path(),
            "../../project name",
            "../context",
            "secret-token",
        )?;

        assert!(token_path.starts_with(directory.path()));
        assert_eq!(token_path.parent().map(Path::parent), Some(Some(directory.path())));
        assert_eq!(fs::read_to_string(&token_path)?, "secret-token");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            assert_eq!(
                fs::metadata(&token_path)?.permissions().mode() & 0o777,
                0o600
            );
        }
        Ok(())
    }

    #[test]
    fn rejects_credentials_that_would_have_to_enter_the_sandbox() {
        let yaml = r#"
clusters:
  - name: cluster
    cluster: { server: https://api.example.com }
users:
  - name: user
    user: { client-key-data: c2VjcmV0 }
contexts:
  - name: context
    context: { cluster: cluster, user: user }
current-context: context
"#;
        let error = PreparedConnection::from_yaml(yaml, Path::new("/tmp"))
            .expect_err("client keys must not be copied into the sandbox");
        assert!(error.to_string().contains("service-account token"));
    }

    const TWO_CONTEXTS: &str = r#"
clusters:
  - name: shared
    cluster: { server: https://dev.example.com }
  - name: other
    cluster: { server: https://127.0.0.1:6443 }
users:
  - name: shared
    user: { token: dev-token }
  - name: other
    user: { token: local-token }
contexts:
  - name: dev
    context: { cluster: shared, user: shared, namespace: apps }
  - name: local
    context: { cluster: other, user: other }
current-context: local
"#;

    #[test]
    fn lists_contexts_and_why_some_cannot_connect() -> Result<()> {
        let entries = kubeconfig_contexts(TWO_CONTEXTS, Path::new("/tmp"))?;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].context, "dev");
        assert_eq!(entries[0].server, "https://dev.example.com");
        assert_eq!(entries[0].namespace, "apps");
        assert!(!entries[0].current);
        assert_eq!(entries[0].problem, None);
        assert!(entries[1].current);
        assert!(entries[1].problem.as_deref().is_some_and(|problem| problem.contains("loopback")));
        Ok(())
    }

    #[test]
    fn merges_contexts_and_replaces_reimported_ones() -> Result<()> {
        let config: Kubeconfig = serde_yaml::from_str(TWO_CONTEXTS)?;
        let dev = PreparedConnection::from_config(&config, Path::new("/tmp"), Some("dev"))?;
        let dev: Kubeconfig = serde_yaml::from_str(&dev.kubeconfig)?;
        assert_eq!(dev.clusters[0].name, "dev");
        assert_eq!(dev.users[0].name, "dev");

        let staging = PreparedConnection::from_yaml(
            r#"
clusters: [{ name: shared, cluster: { server: "https://staging.example.com" } }]
users: [{ name: shared, user: { token: staging-token } }]
contexts: [{ name: staging, context: { cluster: shared, user: shared } }]
current-context: staging
"#,
            Path::new("/tmp"),
        )?;
        let staging: Kubeconfig = serde_yaml::from_str(&staging.kubeconfig)?;

        let mut merged = merge_kubeconfig(Some(dev.clone()), staging);
        assert_eq!(merged.current_context, "staging");
        let names = |config: &Kubeconfig| {
            config
                .contexts
                .iter()
                .map(|context| context.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&merged), ["dev", "staging"]);
        assert_eq!(merged.clusters.len(), 2);
        assert_eq!(
            user_token_for_context(&merged, "staging").as_deref(),
            Some("sands-k8s-staging")
        );

        merged = merge_kubeconfig(Some(merged), dev);
        assert_eq!(names(&merged), ["staging", "dev"]);
        assert_eq!(merged.users.len(), 2);

        remove_context(&mut merged, "dev");
        assert_eq!(names(&merged), ["staging"]);
        assert_eq!(merged.current_context, "staging");
        assert_eq!(merged.clusters.len(), 1);
        assert!(!placeholder_in_use(&merged, "sands-k8s-dev"));
        Ok(())
    }

    #[test]
    fn placeholders_only_use_header_safe_characters() {
        assert_eq!(
            placeholder_for("arn:aws:eks:eu-west-1:123:cluster/prod"),
            "sands-k8s-arn-aws-eks-eu-west-1-123-cluster-prod"
        );
    }

    #[test]
    fn rejects_loopback_api_servers() {
        let yaml = r#"
clusters:
  - name: cluster
    cluster: { server: https://127.0.0.1:6443 }
users:
  - name: user
    user: { token: token }
contexts:
  - name: context
    context: { cluster: cluster, user: user }
current-context: context
"#;
        let error = PreparedConnection::from_yaml(yaml, Path::new("/tmp"))
            .expect_err("loopback on the host is not loopback in the sandbox");
        assert!(error.to_string().contains("loopback"));
    }
}
