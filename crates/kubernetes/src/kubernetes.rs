use std::{
    fmt, fs,
    io::Write as _,
    net::IpAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use sbx::{NetworkDecision, Sbx};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Pod,
    Deployment,
    StatefulSet,
    DaemonSet,
    Job,
    CronJob,
    Service,
    Ingress,
    ConfigMap,
    Secret,
    PersistentVolumeClaim,
    Node,
    Namespace,
    Event,
}

impl ResourceKind {
    pub const ALL: [Self; 14] = [
        Self::Pod,
        Self::Deployment,
        Self::StatefulSet,
        Self::DaemonSet,
        Self::Job,
        Self::CronJob,
        Self::Service,
        Self::Ingress,
        Self::ConfigMap,
        Self::Secret,
        Self::PersistentVolumeClaim,
        Self::Node,
        Self::Namespace,
        Self::Event,
    ];

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Pod => "Pods",
            Self::Deployment => "Deployments",
            Self::StatefulSet => "StatefulSets",
            Self::DaemonSet => "DaemonSets",
            Self::Job => "Jobs",
            Self::CronJob => "CronJobs",
            Self::Service => "Services",
            Self::Ingress => "Ingresses",
            Self::ConfigMap => "ConfigMaps",
            Self::Secret => "Secrets",
            Self::PersistentVolumeClaim => "PersistentVolumeClaims",
            Self::Node => "Nodes",
            Self::Namespace => "Namespaces",
            Self::Event => "Events",
        }
    }

    pub fn resource_name(self) -> &'static str {
        match self {
            Self::Pod => "pods",
            Self::Deployment => "deployments",
            Self::StatefulSet => "statefulsets",
            Self::DaemonSet => "daemonsets",
            Self::Job => "jobs",
            Self::CronJob => "cronjobs",
            Self::Service => "services",
            Self::Ingress => "ingresses",
            Self::ConfigMap => "configmaps",
            Self::Secret => "secrets",
            Self::PersistentVolumeClaim => "persistentvolumeclaims",
            Self::Node => "nodes",
            Self::Namespace => "namespaces",
            Self::Event => "events",
        }
    }

    pub fn is_namespaced(self) -> bool {
        !matches!(self, Self::Node | Self::Namespace)
    }

    pub fn supports_logs(self) -> bool {
        matches!(
            self,
            Self::Pod | Self::Deployment | Self::StatefulSet | Self::DaemonSet | Self::Job
        )
    }

    pub fn supports_restart(self) -> bool {
        matches!(self, Self::Deployment | Self::StatefulSet | Self::DaemonSet)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceSummary {
    pub kind: ResourceKind,
    pub namespace: Option<String>,
    pub name: String,
    pub status: String,
    pub ready: Option<String>,
    pub restarts: Option<u64>,
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelmRelease {
    pub name: String,
    pub namespace: String,
    pub revision: String,
    pub status: String,
    pub chart: String,
    pub app_version: String,
}

#[derive(Debug, Deserialize, Serialize)]
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
    cluster: Cluster,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct Cluster {
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

impl PreparedConnection {
    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read kubeconfig {}", path.display()))?;
        let base_directory = path.parent().unwrap_or_else(|| Path::new("."));
        Self::from_yaml(&contents, base_directory)
    }

    pub fn from_yaml(contents: &str, base_directory: &Path) -> Result<Self> {
        let config: Kubeconfig =
            serde_yaml::from_str(contents).context("the kubeconfig is not valid YAML")?;
        let context_name = if config.current_context.is_empty() {
            match config.contexts.as_slice() {
                [context] => context.name.clone(),
                [] => bail!("the kubeconfig contains no contexts"),
                _ => bail!("the kubeconfig has no current-context; select one before importing"),
            }
        } else {
            config.current_context.clone()
        };
        let context = config
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
        let placeholder = format!("sands-k8s-{context_name}");
        user.user.token = Some(placeholder.clone());

        let namespace = context
            .context
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let sanitized = Kubeconfig {
            api_version: config.api_version,
            kind: config.kind,
            preferences: config.preferences,
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

    pub async fn provision(&self, sbx: &Sbx, sandbox_name: &str) -> Result<()> {
        let previous_placeholder = current_placeholder(sbx, sandbox_name).await?;
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
        let token_file = store_token(sandbox_name, &self.token)?;
        sbx.set_custom_secret_from_file(sandbox_name, &self.host, &self.placeholder, &token_file)
            .await?;

        let mut file = NamedTempFile::new().context("failed to create a temporary kubeconfig")?;
        file.write_all(self.kubeconfig.as_bytes())
            .context("failed to write the temporary kubeconfig")?;
        file.flush()
            .context("failed to flush the temporary kubeconfig")?;
        sbx.exec(sandbox_name, &["mkdir", "-p", "/home/agent/.kube"])
            .await?;
        sbx.copy_to(sandbox_name, file.path(), SANDBOX_KUBECONFIG)
            .await?;
        let output = sbx
            .exec(sandbox_name, &["bash", "-lc", KUBECTL_INSTALL_SCRIPT])
            .await?;
        ensure_success("install kubectl", output)?;
        probe(sbx, sandbox_name)
            .await?
            .context("the Kubernetes connection was not created")?;
        if let Some(previous_placeholder) = previous_placeholder
            && previous_placeholder != self.placeholder
        {
            sbx.remove_custom_secret(sandbox_name, &previous_placeholder)
                .await?;
        }
        Ok(())
    }
}

fn store_token(sandbox_name: &str, token: &str) -> Result<PathBuf> {
    store_token_in(
        &paths::data_dir().join("secrets").join("kubernetes"),
        sandbox_name,
        token,
    )
}

fn store_token_in(root: &Path, sandbox_name: &str, token: &str) -> Result<PathBuf> {
    let sandbox_directory = root.join(URL_SAFE_NO_PAD.encode(sandbox_name.as_bytes()));
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

    let token_path = sandbox_directory.join("token");
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

async fn current_placeholder(sbx: &Sbx, sandbox_name: &str) -> Result<Option<String>> {
    let output = sbx
        .exec(
            sandbox_name,
            &[
                "kubectl", "config", "view", "--minify", "--raw", "-o", "json",
            ],
        )
        .await?;
    if !output.status.success() {
        return Ok(None);
    }
    let configuration: JsonValue = serde_json::from_slice(&output.stdout)
        .context("the existing project kubeconfig is not valid JSON")?;
    Ok(configuration
        .pointer("/users/0/user/token")
        .and_then(JsonValue::as_str)
        .filter(|token| token.starts_with("sands-k8s-"))
        .map(str::to_string))
}

fn inline_certificate_authority(cluster: &mut Cluster, base_directory: &Path) -> Result<()> {
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

pub async fn probe(sbx: &Sbx, sandbox_name: &str) -> Result<Option<KubernetesOverview>> {
    let context_output = sbx
        .exec(sandbox_name, &["kubectl", "config", "current-context"])
        .await?;
    if !context_output.status.success() {
        return Ok(None);
    }
    let context = String::from_utf8_lossy(&context_output.stdout)
        .trim()
        .to_string();
    if context.is_empty() {
        return Ok(None);
    }

    let configuration = kubectl_json(
        sbx,
        sandbox_name,
        &["config", "view", "--minify", "-o", "json"],
    )
    .await?;
    let server = configuration
        .pointer("/clusters/0/cluster/server")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    let namespace = configuration
        .pointer("/contexts/0/context/namespace")
        .and_then(JsonValue::as_str)
        .unwrap_or("default")
        .to_string();
    let version = kubectl_json(sbx, sandbox_name, &["version", "-o", "json"])
        .await?
        .pointer("/serverVersion/gitVersion")
        .and_then(JsonValue::as_str)
        .unwrap_or("unknown")
        .to_string();
    let nodes = kubectl_json_if_allowed(sbx, sandbox_name, &["get", "nodes", "-o", "json"]).await?;
    let pods =
        match kubectl_json_if_allowed(sbx, sandbox_name, &["get", "pods", "-A", "-o", "json"])
            .await?
        {
            Some(pods) => Some(pods),
            None => {
                kubectl_json_if_allowed(sbx, sandbox_name, &["get", "pods", "-o", "json"]).await?
            }
        };
    let node_items = nodes
        .as_ref()
        .and_then(|nodes| nodes.get("items"))
        .and_then(JsonValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let pod_items = pods
        .as_ref()
        .and_then(|pods| pods.get("items"))
        .and_then(JsonValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let ready_nodes = node_items
        .iter()
        .filter(|node| {
            node.pointer("/status/conditions")
                .and_then(JsonValue::as_array)
                .is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition.get("type").and_then(JsonValue::as_str) == Some("Ready")
                            && condition.get("status").and_then(JsonValue::as_str) == Some("True")
                    })
                })
        })
        .count();
    let running_pods = pod_items
        .iter()
        .filter(|pod| pod.pointer("/status/phase").and_then(JsonValue::as_str) == Some("Running"))
        .count();
    Ok(Some(KubernetesOverview {
        context,
        server,
        namespace,
        version,
        ready_nodes,
        total_nodes: nodes.as_ref().map(|_| node_items.len()),
        running_pods,
        total_pods: pods.as_ref().map(|_| pod_items.len()),
    }))
}

pub async fn list_resources(
    sbx: &Sbx,
    sandbox_name: &str,
    kind: ResourceKind,
    namespace: Option<&str>,
) -> Result<Vec<ResourceSummary>> {
    let mut arguments = vec!["get", kind.resource_name()];
    if kind.is_namespaced() {
        match namespace.filter(|namespace| !namespace.is_empty()) {
            Some(namespace) => arguments.extend(["--namespace", namespace]),
            None => arguments.push("--all-namespaces"),
        }
    }
    arguments.extend(["-o", "json"]);
    let resources = kubectl_json(sbx, sandbox_name, &arguments).await?;
    Ok(resources
        .get("items")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(|resource| resource_summary(kind, resource))
        .collect())
}

pub async fn resource_yaml(
    sbx: &Sbx,
    sandbox_name: &str,
    resource: &ResourceSummary,
) -> Result<String> {
    let mut arguments = vec!["get", resource.kind.resource_name(), resource.name.as_str()];
    if let Some(namespace) = resource.namespace.as_deref() {
        arguments.extend(["--namespace", namespace]);
    }
    arguments.extend(["-o", "yaml"]);
    let mut command = vec!["kubectl"];
    command.extend(arguments);
    let output = sbx.exec(sandbox_name, &command).await?;
    let output = ensure_success("kubectl get resource", output)?;
    String::from_utf8(output.stdout).context("kubectl returned non-UTF-8 YAML")
}

pub async fn delete_resource(
    sbx: &Sbx,
    sandbox_name: &str,
    resource: &ResourceSummary,
) -> Result<()> {
    let mut command = vec![
        "kubectl",
        "delete",
        resource.kind.resource_name(),
        resource.name.as_str(),
    ];
    if let Some(namespace) = resource.namespace.as_deref() {
        command.extend(["--namespace", namespace]);
    }
    let output = sbx.exec(sandbox_name, &command).await?;
    ensure_success("kubectl delete resource", output).map(|_| ())
}

pub async fn restart_resource(
    sbx: &Sbx,
    sandbox_name: &str,
    resource: &ResourceSummary,
) -> Result<()> {
    if !resource.kind.supports_restart() {
        bail!("{} cannot be restarted", resource.kind.display_name());
    }
    let mut command = vec![
        "kubectl",
        "rollout",
        "restart",
        resource.kind.resource_name(),
        resource.name.as_str(),
    ];
    if let Some(namespace) = resource.namespace.as_deref() {
        command.extend(["--namespace", namespace]);
    }
    let output = sbx.exec(sandbox_name, &command).await?;
    ensure_success("kubectl rollout restart", output).map(|_| ())
}

pub async fn helm_releases(sbx: &Sbx, sandbox_name: &str) -> Result<Option<Vec<HelmRelease>>> {
    let output = sbx
        .exec(
            sandbox_name,
            &["helm", "list", "--all-namespaces", "-o", "json"],
        )
        .await?;
    if !output.status.success()
        && String::from_utf8_lossy(&output.stderr).contains("executable file not found")
    {
        return Ok(None);
    }
    let output = ensure_success("helm list", output)?;
    let releases: Vec<JsonValue> =
        serde_json::from_slice(&output.stdout).context("helm returned invalid JSON")?;
    Ok(Some(
        releases
            .into_iter()
            .filter_map(|release| {
                Some(HelmRelease {
                    name: release.get("name")?.as_str()?.to_string(),
                    namespace: release
                        .get("namespace")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("default")
                        .to_string(),
                    revision: release.get("revision").map(json_scalar).unwrap_or_default(),
                    status: release
                        .get("status")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    chart: release
                        .get("chart")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    app_version: release
                        .get("app_version")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect(),
    ))
}

fn resource_summary(kind: ResourceKind, resource: &JsonValue) -> Option<ResourceSummary> {
    let metadata = resource.get("metadata")?;
    let name = metadata.get("name")?.as_str()?.to_string();
    let namespace = metadata
        .get("namespace")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let created_at = metadata
        .get("creationTimestamp")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let (status, ready, restarts) = resource_status(kind, resource);
    Some(ResourceSummary {
        kind,
        namespace,
        name,
        status,
        ready,
        restarts,
        created_at,
    })
}

fn resource_status(
    kind: ResourceKind,
    resource: &JsonValue,
) -> (String, Option<String>, Option<u64>) {
    let status = resource.get("status").unwrap_or(&JsonValue::Null);
    match kind {
        ResourceKind::Pod => {
            let containers = status
                .get("containerStatuses")
                .and_then(JsonValue::as_array);
            let ready = containers.map(|containers| {
                let ready = containers
                    .iter()
                    .filter(|container| {
                        container.get("ready").and_then(JsonValue::as_bool) == Some(true)
                    })
                    .count();
                format!("{ready}/{}", containers.len())
            });
            let restarts = containers.map(|containers| {
                containers
                    .iter()
                    .filter_map(|container| {
                        container.get("restartCount").and_then(JsonValue::as_u64)
                    })
                    .sum()
            });
            (
                status
                    .get("phase")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("Unknown")
                    .to_string(),
                ready,
                restarts,
            )
        }
        ResourceKind::Deployment | ResourceKind::StatefulSet | ResourceKind::DaemonSet => {
            let desired = status
                .get(if kind == ResourceKind::DaemonSet {
                    "desiredNumberScheduled"
                } else {
                    "replicas"
                })
                .and_then(JsonValue::as_u64)
                .unwrap_or_default();
            let ready_count = status
                .get(if kind == ResourceKind::DaemonSet {
                    "numberReady"
                } else {
                    "readyReplicas"
                })
                .and_then(JsonValue::as_u64)
                .unwrap_or_default();
            (
                if ready_count == desired {
                    "Ready"
                } else {
                    "Progressing"
                }
                .to_string(),
                Some(format!("{ready_count}/{desired}")),
                None,
            )
        }
        ResourceKind::Job => {
            let succeeded = status
                .get("succeeded")
                .and_then(JsonValue::as_u64)
                .unwrap_or_default();
            let failed = status
                .get("failed")
                .and_then(JsonValue::as_u64)
                .unwrap_or_default();
            let active = status
                .get("active")
                .and_then(JsonValue::as_u64)
                .unwrap_or_default();
            let status = if failed > 0 {
                "Failed"
            } else if active > 0 {
                "Running"
            } else if succeeded > 0 {
                "Complete"
            } else {
                "Pending"
            };
            (status.to_string(), None, None)
        }
        ResourceKind::Service => (
            resource
                .pointer("/spec/type")
                .and_then(JsonValue::as_str)
                .unwrap_or("ClusterIP")
                .to_string(),
            None,
            None,
        ),
        ResourceKind::PersistentVolumeClaim => (
            status
                .get("phase")
                .and_then(JsonValue::as_str)
                .unwrap_or("Pending")
                .to_string(),
            None,
            None,
        ),
        ResourceKind::Node => {
            let ready = status
                .get("conditions")
                .and_then(JsonValue::as_array)
                .is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition.get("type").and_then(JsonValue::as_str) == Some("Ready")
                            && condition.get("status").and_then(JsonValue::as_str) == Some("True")
                    })
                });
            (
                if ready { "Ready" } else { "NotReady" }.to_string(),
                None,
                None,
            )
        }
        ResourceKind::Event => (
            resource
                .get("reason")
                .and_then(JsonValue::as_str)
                .unwrap_or("Event")
                .to_string(),
            None,
            None,
        ),
        _ => ("Active".to_string(), None, None),
    }
}

fn json_scalar(value: &JsonValue) -> String {
    match value {
        JsonValue::String(value) => value.clone(),
        JsonValue::Number(value) => value.to_string(),
        _ => String::new(),
    }
}

async fn kubectl_json(sbx: &Sbx, sandbox_name: &str, arguments: &[&str]) -> Result<JsonValue> {
    let mut command = vec!["kubectl"];
    command.extend_from_slice(arguments);
    let output = sbx.exec(sandbox_name, &command).await?;
    let output = ensure_success(&format!("kubectl {}", arguments.join(" ")), output)?;
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("kubectl {} returned invalid JSON", arguments.join(" ")))
}

async fn kubectl_json_if_allowed(
    sbx: &Sbx,
    sandbox_name: &str,
    arguments: &[&str],
) -> Result<Option<JsonValue>> {
    let mut command = vec!["kubectl"];
    command.extend_from_slice(arguments);
    let output = sbx.exec(sandbox_name, &command).await?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("Forbidden") || error.contains("forbidden") {
            return Ok(None);
        }
    }
    let output = ensure_success(&format!("kubectl {}", arguments.join(" ")), output)?;
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("kubectl {} returned invalid JSON", arguments.join(" ")))
        .map(Some)
}

fn ensure_success(command: &str, output: std::process::Output) -> Result<std::process::Output> {
    if output.status.success() {
        return Ok(output);
    }
    let message = String::from_utf8_lossy(&output.stderr)
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("command failed without error output")
        .to_string();
    Err(anyhow!("{command} failed: {message}"))
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
        let token_path = store_token_in(directory.path(), "../../project name", "secret-token")?;

        assert!(token_path.starts_with(directory.path()));
        assert_eq!(
            token_path.file_name().and_then(|name| name.to_str()),
            Some("token")
        );
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
