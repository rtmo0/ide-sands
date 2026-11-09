use std::collections::HashMap;

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::lock::Mutex;
use sbx::{Sbx, ShellOutput, ShellSession};
use serde_json::Value as JsonValue;

use crate::{
    resources::{ObjectRef, ResourceKind, builtin_kind},
    table::parse_quantity,
};

/// kubectl gives up instead of hanging the panel when the API server is unreachable.
const REQUEST_TIMEOUT: &str = "--request-timeout=20s";

/// Where Sands keeps manifests it opens for editing inside the sandbox.
pub const MANIFEST_DIRECTORY: &str = "/home/agent/.sands/kubernetes";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub cpu: f64,
    pub memory: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextInfo {
    pub context: String,
    pub server: String,
    pub namespace: String,
}

/// One cluster connected to the sandbox, as its kubeconfig context, with a quick health check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterStatus {
    pub context: ContextInfo,
    pub current: bool,
    pub version: Option<String>,
    pub ready_nodes: usize,
    pub total_nodes: Option<usize>,
    pub error: Option<String>,
}

/// The objects of one list request. Without cluster-wide list rights the list falls back to the
/// context namespace and says so in `notice`.
#[derive(Clone, Debug, Default)]
pub struct Listing {
    pub items: Vec<JsonValue>,
    pub notice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub revision: u64,
    pub current: bool,
    pub created_at: Option<String>,
    pub description: String,
    pub status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelmRelease {
    pub name: String,
    pub namespace: String,
    pub revision: String,
    pub updated: String,
    pub status: String,
    pub chart: String,
    pub app_version: String,
}

/// The Kubernetes cluster connected to one project sandbox. kubectl and helm run inside the
/// sandbox through one long-lived shell, so the token never leaves the sbx proxy.
pub struct Cluster {
    sbx: Sbx,
    sandbox: String,
    session: Mutex<Option<ShellSession>>,
}

impl Cluster {
    pub fn new(sbx: Sbx, sandbox: impl Into<String>) -> Self {
        Self {
            sbx,
            sandbox: sandbox.into(),
            session: Mutex::new(None),
        }
    }

    pub fn sandbox(&self) -> &str {
        &self.sandbox
    }

    /// Runs a bash script in the sandbox, starting the shell session on first use and once more
    /// if it broke (for example because the sandbox was restarted).
    pub async fn shell(&self, script: &str) -> Result<ShellOutput> {
        let mut session = self.session.lock().await;
        for attempt in 0..2 {
            if session.is_none() {
                *session = Some(ShellSession::start(&self.sbx, &self.sandbox).await?);
            }
            let Some(running) = session.as_mut() else {
                continue;
            };
            match running.run(script).await {
                Ok(output) => return Ok(output),
                Err(error) => {
                    *session = None;
                    if attempt == 1 {
                        return Err(error.into());
                    }
                }
            }
        }
        Err(anyhow!("the sandbox shell could not be started"))
    }

    pub async fn kubectl(&self, arguments: &[&str]) -> Result<ShellOutput> {
        self.shell(&kubectl_command(arguments)).await
    }

    pub async fn kubectl_success(&self, arguments: &[&str]) -> Result<String> {
        let output = self.kubectl(arguments).await?;
        ensure_success(&format!("kubectl {}", arguments.join(" ")), &output)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub async fn kubectl_json(&self, arguments: &[&str]) -> Result<JsonValue> {
        let output = self.kubectl(arguments).await?;
        let command = format!("kubectl {}", arguments.join(" "));
        ensure_success(&command, &output)?;
        serde_json::from_slice(&output.stdout)
            .with_context(|| format!("{command} returned invalid JSON"))
    }

    /// Runs several kubectl commands in parallel inside the sandbox and returns their outputs in
    /// order. One round trip instead of one per command keeps panels responsive.
    pub async fn kubectl_batch(&self, commands: &[Vec<String>]) -> Result<Vec<ShellOutput>> {
        if commands.is_empty() {
            return Ok(Vec::new());
        }
        let mut script = String::from("batch_dir=$(mktemp -d)\n");
        for (index, command) in commands.iter().enumerate() {
            let arguments = command.iter().map(String::as_str).collect::<Vec<_>>();
            script.push_str(&format!(
                "{{ {} > \"$batch_dir/{index}.out\" 2> \"$batch_dir/{index}.err\"; echo $? > \"$batch_dir/{index}.status\"; }} &\n",
                kubectl_command(&arguments)
            ));
        }
        script.push_str("wait\n");
        script.push_str(&format!(
            "for index in $(seq 0 {}); do printf '%s %s %s\\n' \"$(cat \"$batch_dir/$index.status\")\" \"$(base64 -w0 < \"$batch_dir/$index.out\")\" \"$(base64 -w0 < \"$batch_dir/$index.err\")\"; done\n",
            commands.len() - 1
        ));
        script.push_str("rm -rf \"$batch_dir\"\n");
        let output = self.shell(&script).await?;
        ensure_success("kubectl batch", &output)?;
        let text = String::from_utf8_lossy(&output.stdout);
        let outputs = text
            .lines()
            .map(|line| {
                let mut parts = line.splitn(3, ' ');
                let status = parts
                    .next()
                    .and_then(|status| status.trim().parse::<i32>().ok())
                    .unwrap_or(-1);
                let mut decode = || {
                    STANDARD
                        .decode(parts.next().unwrap_or_default().trim())
                        .unwrap_or_default()
                };
                let stdout = decode();
                let stderr = decode();
                ShellOutput {
                    status,
                    stdout,
                    stderr,
                }
            })
            .collect::<Vec<_>>();
        if outputs.len() != commands.len() {
            bail!(
                "kubectl batch returned {} results for {} commands",
                outputs.len(),
                commands.len()
            );
        }
        Ok(outputs)
    }

    pub async fn context(&self) -> Result<Option<ContextInfo>> {
        let output = self
            .kubectl(&["config", "view", "--minify", "-o", "json"])
            .await?;
        if !output.success() {
            return Ok(None);
        }
        let configuration: JsonValue = serde_json::from_slice(&output.stdout)
            .context("kubectl config view returned invalid JSON")?;
        let Some(context) = configuration
            .get("current-context")
            .and_then(JsonValue::as_str)
            .filter(|context| !context.is_empty())
        else {
            return Ok(None);
        };
        Ok(Some(ContextInfo {
            context: context.to_string(),
            server: configuration
                .pointer("/clusters/0/cluster/server")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string(),
            namespace: configuration
                .pointer("/contexts/0/context/namespace")
                .and_then(JsonValue::as_str)
                .unwrap_or("default")
                .to_string(),
        }))
    }

    /// Every context of the sandbox kubeconfig, and which one kubectl uses now.
    pub async fn contexts(&self) -> Result<(Vec<ContextInfo>, Option<String>)> {
        let output = self.kubectl(&["config", "view", "-o", "json"]).await?;
        if !output.success() {
            return Ok((Vec::new(), None));
        }
        let configuration: JsonValue = serde_json::from_slice(&output.stdout)
            .context("kubectl config view returned invalid JSON")?;
        Ok(parse_contexts(&configuration))
    }

    pub async fn use_context(&self, context: &str) -> Result<()> {
        self.kubectl_success(&["config", "use-context", context])
            .await
            .map(|_| ())
    }

    /// Checks every connected cluster in one round trip: server version and node readiness.
    pub async fn statuses(&self) -> Result<Vec<ClusterStatus>> {
        let (contexts, current) = self.contexts().await?;
        let commands = contexts
            .iter()
            .flat_map(|context| {
                [
                    vec!["version", "-o", "json"],
                    vec!["get", "nodes", "-o", "json"],
                ]
                .map(|command| {
                    // An unreachable cluster should not hold up the others for long.
                    ["--context", context.context.as_str(), "--request-timeout=8s"]
                        .into_iter()
                        .chain(command)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let outputs = self.kubectl_batch(&commands).await?;
        Ok(contexts
            .into_iter()
            .enumerate()
            .map(|(index, context)| {
                let version_output = outputs.get(index * 2);
                let nodes = outputs
                    .get(index * 2 + 1)
                    .filter(|output| output.success())
                    .map(parse_list_output);
                let version = version_output
                    .filter(|output| output.success())
                    .and_then(|output| serde_json::from_slice::<JsonValue>(&output.stdout).ok())
                    .and_then(|version| {
                        version
                            .pointer("/serverVersion/gitVersion")
                            .and_then(JsonValue::as_str)
                            .map(str::to_string)
                    });
                let error = match version_output {
                    Some(output) if output.success() => None,
                    Some(output) => Some(
                        String::from_utf8_lossy(&output.stderr)
                            .lines()
                            .find(|line| !line.trim().is_empty())
                            .unwrap_or("the cluster did not answer")
                            .to_string(),
                    ),
                    None => Some("the cluster was not checked".to_string()),
                };
                ClusterStatus {
                    current: current.as_deref() == Some(context.context.as_str()),
                    version,
                    ready_nodes: nodes
                        .iter()
                        .flatten()
                        .filter(|node| crate::table::node_ready(node))
                        .count(),
                    total_nodes: nodes.as_ref().map(Vec::len),
                    error,
                    context,
                }
            })
            .collect())
    }

    pub async fn server_version(&self) -> Result<String> {
        let version = self.kubectl_json(&["version", "-o", "json"]).await?;
        Ok(version
            .pointer("/serverVersion/gitVersion")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown")
            .to_string())
    }

    pub async fn list(&self, kind: &ResourceKind, namespace: Option<&str>) -> Result<Listing> {
        let mut listing = self
            .list_batch(&[(kind.clone(), namespace.map(str::to_string))])
            .await?;
        listing.pop().context("kubectl returned no listing")?
    }

    /// Lists several kinds in one round trip. Each result fails on its own, so one forbidden kind
    /// does not hide the others.
    pub async fn list_batch(
        &self,
        requests: &[(ResourceKind, Option<String>)],
    ) -> Result<Vec<Result<Listing>>> {
        let commands = requests
            .iter()
            .map(|(kind, namespace)| list_command(kind, namespace.as_deref(), true))
            .collect::<Vec<_>>();
        let outputs = self.kubectl_batch(&commands).await?;
        let mut results = Vec::with_capacity(outputs.len());
        for ((kind, namespace), output) in requests.iter().zip(outputs) {
            let all_namespaces = kind.namespaced && namespace.as_deref().is_none_or(str::is_empty);
            if !output.success() && all_namespaces && is_forbidden(&output) {
                // Without cluster-wide rights, show the context namespace instead of nothing.
                let command = list_command(kind, None, false);
                let command = command.iter().map(String::as_str).collect::<Vec<_>>();
                let fallback = self.kubectl(&command).await?;
                results.push(parse_listing(kind, &fallback).map(|mut listing| {
                    listing.notice = Some(format!(
                        "No permission to list {} in all namespaces; showing the context namespace.",
                        kind.title
                    ));
                    listing
                }));
                continue;
            }
            results.push(parse_listing(kind, &output));
        }
        Ok(results)
    }

    pub async fn get(&self, reference: &ObjectRef) -> Result<JsonValue> {
        let mut arguments = vec!["get".to_string()];
        arguments.extend(reference.kubectl_arguments());
        arguments.extend(["-o".to_string(), "json".to_string()]);
        let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        self.kubectl_json(&arguments).await
    }

    pub async fn yaml(&self, reference: &ObjectRef) -> Result<String> {
        let mut arguments = vec!["get".to_string()];
        arguments.extend(reference.kubectl_arguments());
        arguments.extend(["-o".to_string(), "yaml".to_string()]);
        let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        self.kubectl_success(&arguments).await
    }

    pub async fn namespaces(&self) -> Result<Vec<String>> {
        let output = self
            .kubectl(&[
                "get",
                "namespaces",
                "-o",
                "jsonpath={.items[*].metadata.name}",
            ])
            .await?;
        if !output.success() {
            return Ok(Vec::new());
        }
        let mut namespaces = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        namespaces.sort();
        Ok(namespaces)
    }

    pub async fn custom_kinds(&self) -> Result<Vec<ResourceKind>> {
        let Some(definitions) = builtin_kind("crds") else {
            return Ok(Vec::new());
        };
        let listing = self.list(definitions, None).await?;
        let mut kinds = listing
            .items
            .iter()
            .filter_map(ResourceKind::from_definition)
            .collect::<Vec<_>>();
        kinds.sort_by(|a, b| a.group.cmp(&b.group).then(a.kind.cmp(&b.kind)));
        Ok(kinds)
    }

    /// Events about one object, newest first.
    pub async fn events_for(&self, reference: &ObjectRef) -> Result<Vec<JsonValue>> {
        let selector = format!(
            "involvedObject.name={},involvedObject.kind={}",
            reference.name, reference.kind.kind
        );
        let mut arguments = vec!["get", "events", "--field-selector", selector.as_str()];
        match reference.namespace.as_deref() {
            Some(namespace) => arguments.extend(["--namespace", namespace]),
            None => arguments.push("--all-namespaces"),
        }
        arguments.extend(["-o", "json"]);
        let output = self.kubectl(&arguments).await?;
        if !output.success() {
            return Ok(Vec::new());
        }
        let mut events = parse_items(&output.stdout)?;
        events.sort_by_key(|event| std::cmp::Reverse(crate::table::event_time(event)));
        Ok(events)
    }

    pub async fn pod_metrics(&self, namespace: Option<&str>) -> Option<HashMap<String, Usage>> {
        let mut arguments = vec!["get", "--raw"];
        let path = match namespace.filter(|namespace| !namespace.is_empty()) {
            Some(namespace) => format!("/apis/metrics.k8s.io/v1beta1/namespaces/{namespace}/pods"),
            None => "/apis/metrics.k8s.io/v1beta1/pods".to_string(),
        };
        arguments.push(&path);
        let output = self.kubectl(&arguments).await.ok()?;
        output
            .success()
            .then(|| parse_pod_metrics(&output.stdout))
            .flatten()
    }

    pub async fn node_metrics(&self) -> Option<HashMap<String, Usage>> {
        let output = self
            .kubectl(&["get", "--raw", "/apis/metrics.k8s.io/v1beta1/nodes"])
            .await
            .ok()?;
        output
            .success()
            .then(|| parse_node_metrics(&output.stdout))
            .flatten()
    }

    pub async fn delete(&self, reference: &ObjectRef) -> Result<()> {
        let mut arguments = vec!["delete".to_string()];
        arguments.extend(reference.kubectl_arguments());
        arguments.push("--wait=false".to_string());
        self.run_owned(&arguments).await.map(|_| ())
    }

    pub async fn scale(&self, reference: &ObjectRef, replicas: u32) -> Result<()> {
        let mut arguments = vec!["scale".to_string()];
        arguments.extend(reference.kubectl_arguments());
        arguments.push(format!("--replicas={replicas}"));
        self.run_owned(&arguments).await.map(|_| ())
    }

    pub async fn restart(&self, reference: &ObjectRef) -> Result<()> {
        let mut arguments = vec!["rollout".to_string(), "restart".to_string()];
        arguments.extend(reference.kubectl_arguments());
        self.run_owned(&arguments).await.map(|_| ())
    }

    pub async fn rollout_history(&self, reference: &ObjectRef) -> Result<Vec<Revision>> {
        let object = self.get(reference).await?;
        let current = object
            .pointer("/metadata/annotations/deployment.kubernetes.io~1revision")
            .and_then(JsonValue::as_str)
            .and_then(|revision| revision.parse::<u64>().ok());
        let mut revisions = match reference.kind.id.as_str() {
            "deployments" => {
                let Some(replica_sets) = builtin_kind("replicasets") else {
                    return Ok(Vec::new());
                };
                let listing = self
                    .list(replica_sets, reference.namespace.as_deref())
                    .await?;
                let uid = object.pointer("/metadata/uid").and_then(JsonValue::as_str);
                listing
                    .items
                    .iter()
                    .filter(|replica_set| owned_by(replica_set, uid))
                    .filter_map(|replica_set| {
                        let revision = replica_set
                            .pointer("/metadata/annotations/deployment.kubernetes.io~1revision")?
                            .as_str()?
                            .parse::<u64>()
                            .ok()?;
                        Some(Revision {
                            revision,
                            current: Some(revision) == current,
                            created_at: string_at(replica_set, "/metadata/creationTimestamp"),
                            description: crate::table::images(
                                replica_set
                                    .pointer("/spec/template")
                                    .unwrap_or(&JsonValue::Null),
                            ),
                            status: None,
                        })
                    })
                    .collect::<Vec<_>>()
            }
            "statefulsets" | "daemonsets" => {
                let output = self
                    .kubectl(&[
                        "get",
                        "controllerrevisions",
                        "--namespace",
                        reference.namespace.as_deref().unwrap_or("default"),
                        "-o",
                        "json",
                    ])
                    .await?;
                ensure_success("kubectl get controllerrevisions", &output)?;
                let uid = object.pointer("/metadata/uid").and_then(JsonValue::as_str);
                let current_name = string_at(&object, "/status/updateRevision")
                    .or_else(|| string_at(&object, "/status/currentRevision"));
                parse_items(&output.stdout)?
                    .iter()
                    .filter(|revision| owned_by(revision, uid))
                    .filter_map(|revision| {
                        let name = string_at(revision, "/metadata/name");
                        Some(Revision {
                            revision: revision.get("revision")?.as_u64()?,
                            current: name.is_some() && name == current_name,
                            created_at: string_at(revision, "/metadata/creationTimestamp"),
                            description: crate::table::images(
                                revision
                                    .pointer("/data/spec/template")
                                    .unwrap_or(&JsonValue::Null),
                            ),
                            status: None,
                        })
                    })
                    .collect::<Vec<_>>()
            }
            _ => Vec::new(),
        };
        revisions.sort_by_key(|revision| std::cmp::Reverse(revision.revision));
        if !revisions.iter().any(|revision| revision.current)
            && let Some(newest) = revisions.first_mut()
        {
            newest.current = true;
        }
        Ok(revisions)
    }

    pub async fn rollback(&self, reference: &ObjectRef, revision: u64) -> Result<()> {
        let mut arguments = vec!["rollout".to_string(), "undo".to_string()];
        arguments.extend(reference.kubectl_arguments());
        arguments.push(format!("--to-revision={revision}"));
        self.run_owned(&arguments).await.map(|_| ())
    }

    pub async fn set_suspended(&self, reference: &ObjectRef, suspended: bool) -> Result<()> {
        let mut arguments = vec!["patch".to_string()];
        arguments.extend(reference.kubectl_arguments());
        arguments.extend([
            "--type=merge".to_string(),
            "-p".to_string(),
            format!("{{\"spec\":{{\"suspend\":{suspended}}}}}"),
        ]);
        self.run_owned(&arguments).await.map(|_| ())
    }

    /// Starts a Job from a CronJob's template, like `kubectl create job --from=cronjob/...`.
    pub async fn trigger(&self, reference: &ObjectRef) -> Result<String> {
        let job_name = format!(
            "{}-manual-{}",
            reference.name.chars().take(40).collect::<String>(),
            chrono::Utc::now().format("%H%M%S")
        );
        let mut arguments = vec![
            "create".to_string(),
            "job".to_string(),
            job_name.clone(),
            format!("--from=cronjob/{}", reference.name),
        ];
        if let Some(namespace) = &reference.namespace {
            arguments.extend(["--namespace".to_string(), namespace.clone()]);
        }
        self.run_owned(&arguments).await?;
        Ok(job_name)
    }

    pub async fn set_cordoned(&self, node: &str, cordoned: bool) -> Result<()> {
        self.kubectl_success(&[if cordoned { "cordon" } else { "uncordon" }, node])
            .await
            .map(|_| ())
    }

    /// Evicts the node's pods, respecting PodDisruptionBudgets.
    pub async fn drain(&self, node: &str) -> Result<String> {
        self.kubectl_success(&[
            "drain",
            node,
            "--ignore-daemonsets",
            "--delete-emptydir-data",
            "--timeout=300s",
        ])
        .await
    }

    /// Replaces the live object with the manifest at `path`. The manifest keeps the
    /// resourceVersion it was read with, so a concurrent change fails instead of being lost.
    pub async fn replace(&self, path: &str) -> Result<String> {
        self.kubectl_success(&["replace", "-f", path]).await
    }

    pub async fn apply(&self, path: &str, dry_run: bool) -> Result<String> {
        let mut arguments = vec!["apply", "--server-side", "--force-conflicts", "-f", path];
        if dry_run {
            arguments.push("--dry-run=server");
        }
        self.kubectl_success(&arguments).await
    }

    /// `kubectl diff` exits with 1 when there are differences, which is not a failure.
    pub async fn diff(&self, path: &str) -> Result<String> {
        let output = self.kubectl(&["diff", "--server-side", "-f", path]).await?;
        if output.status > 1 {
            ensure_success("kubectl diff", &output)?;
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Writes `contents` to `path` inside the sandbox.
    pub async fn write_file(&self, path: &str, contents: &str) -> Result<()> {
        let script = format!(
            "mkdir -p \"$(dirname {path})\" && printf '%s' {contents} | base64 -d > {path}",
            path = shell_quote(path),
            contents = shell_quote(&STANDARD.encode(contents)),
        );
        let output = self.shell(&script).await?;
        ensure_success(&format!("write {path}"), &output)
    }

    /// Opens the live object for editing: writes its YAML, without the fields the server
    /// manages, to a file in the sandbox and returns the file's path.
    pub async fn export_for_editing(&self, reference: &ObjectRef) -> Result<String> {
        let mut object = self.get(reference).await?;
        if let Some(metadata) = object
            .get_mut("metadata")
            .and_then(JsonValue::as_object_mut)
        {
            metadata.remove("managedFields");
        }
        let yaml =
            serde_yaml::to_string(&object).context("failed to convert the object to YAML")?;
        let path = format!(
            "{MANIFEST_DIRECTORY}/{}/{}/{}.yaml",
            reference.namespace.as_deref().unwrap_or("_cluster"),
            reference.kind.plural,
            safe_file_name(&reference.name)
        );
        self.write_file(&path, &yaml).await?;
        Ok(path)
    }

    /// Sets or removes (`value: None`) one key of a Secret or ConfigMap.
    pub async fn set_data_key(
        &self,
        reference: &ObjectRef,
        key: &str,
        value: Option<&str>,
    ) -> Result<()> {
        let encoded = match (reference.kind.id.as_str(), value) {
            ("secrets", Some(value)) => JsonValue::String(STANDARD.encode(value)),
            (_, Some(value)) => JsonValue::String(value.to_string()),
            (_, None) => JsonValue::Null,
        };
        let patch = serde_json::json!({ "data": { key: encoded } });
        let mut arguments = vec!["patch".to_string()];
        arguments.extend(reference.kubectl_arguments());
        arguments.extend([
            "--type=merge".to_string(),
            "-p".to_string(),
            patch.to_string(),
        ]);
        self.run_owned(&arguments).await.map(|_| ())
    }

    pub async fn helm_available(&self) -> Result<bool> {
        Ok(self.shell("command -v helm >/dev/null").await?.success())
    }

    pub async fn helm_releases(&self, namespace: Option<&str>) -> Result<Vec<HelmRelease>> {
        let mut command = vec!["helm", "list", "-o", "json", "--max", "1000"];
        match namespace.filter(|namespace| !namespace.is_empty()) {
            Some(namespace) => command.extend(["--namespace", namespace]),
            None => command.push("--all-namespaces"),
        }
        let output = self.shell(&shell_command(&command)).await?;
        ensure_success("helm list", &output)?;
        let releases: Vec<JsonValue> =
            serde_json::from_slice(&output.stdout).context("helm returned invalid JSON")?;
        Ok(releases
            .iter()
            .filter_map(|release| {
                Some(HelmRelease {
                    name: release.get("name")?.as_str()?.to_string(),
                    namespace: text_field(release, "namespace"),
                    revision: release.get("revision").map(json_scalar).unwrap_or_default(),
                    updated: text_field(release, "updated"),
                    status: text_field(release, "status"),
                    chart: text_field(release, "chart"),
                    app_version: text_field(release, "app_version"),
                })
            })
            .collect())
    }

    pub async fn helm(&self, arguments: &[&str]) -> Result<String> {
        let mut command = vec!["helm"];
        command.extend_from_slice(arguments);
        let output = self.shell(&shell_command(&command)).await?;
        ensure_success(&format!("helm {}", arguments.join(" ")), &output)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub async fn helm_history(&self, name: &str, namespace: &str) -> Result<Vec<Revision>> {
        let history = self
            .helm(&["history", name, "--namespace", namespace, "-o", "json"])
            .await?;
        let entries: Vec<JsonValue> =
            serde_json::from_str(&history).context("helm returned invalid JSON")?;
        let mut revisions = entries
            .iter()
            .filter_map(|entry| {
                let status = text_field(entry, "status");
                Some(Revision {
                    revision: entry.get("revision")?.as_u64()?,
                    current: status == "deployed",
                    created_at: entry
                        .get("updated")
                        .and_then(JsonValue::as_str)
                        .map(str::to_string),
                    description: format!(
                        "{} {}",
                        text_field(entry, "chart"),
                        text_field(entry, "description")
                    )
                    .trim()
                    .to_string(),
                    status: Some(status),
                })
            })
            .collect::<Vec<_>>();
        revisions.sort_by_key(|revision| std::cmp::Reverse(revision.revision));
        Ok(revisions)
    }

    async fn run_owned(&self, arguments: &[String]) -> Result<String> {
        let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        self.kubectl_success(&arguments).await
    }
}

fn parse_contexts(configuration: &JsonValue) -> (Vec<ContextInfo>, Option<String>) {
    let entries = |key: &str| {
        configuration
            .get(key)
            .and_then(JsonValue::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let clusters = entries("clusters");
    let contexts = entries("contexts")
        .iter()
        .filter_map(|context| {
            let name = context.get("name")?.as_str()?.to_string();
            let cluster = context.pointer("/context/cluster").and_then(JsonValue::as_str);
            let server = clusters
                .iter()
                .find(|candidate| candidate.get("name").and_then(JsonValue::as_str) == cluster)
                .and_then(|candidate| candidate.pointer("/cluster/server"))
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            Some(ContextInfo {
                context: name,
                server,
                namespace: context
                    .pointer("/context/namespace")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("default")
                    .to_string(),
            })
        })
        .collect();
    let current = configuration
        .get("current-context")
        .and_then(JsonValue::as_str)
        .filter(|context| !context.is_empty())
        .map(str::to_string);
    (contexts, current)
}

pub fn shell_quote(argument: &str) -> String {
    if !argument.is_empty()
        && argument.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '.' | '/' | ':' | '=' | ',' | '@')
        })
    {
        return argument.to_string();
    }
    format!("'{}'", argument.replace('\'', "'\\''"))
}

fn shell_command(arguments: &[&str]) -> String {
    arguments
        .iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

fn kubectl_command(arguments: &[&str]) -> String {
    let mut command = vec!["kubectl", REQUEST_TIMEOUT];
    command.extend_from_slice(arguments);
    shell_command(&command)
}

/// `all_namespaces` lists every namespace when none is given; otherwise kubectl uses the
/// context namespace.
fn list_command(kind: &ResourceKind, namespace: Option<&str>, all_namespaces: bool) -> Vec<String> {
    let mut command = vec!["get".to_string(), kind.kubectl_type()];
    if kind.namespaced {
        match namespace.filter(|namespace| !namespace.is_empty()) {
            Some(namespace) => command.extend(["--namespace".to_string(), namespace.to_string()]),
            None if all_namespaces => command.push("--all-namespaces".to_string()),
            None => {}
        }
    }
    command.extend(["-o".to_string(), "json".to_string()]);
    command
}

fn parse_listing(kind: &ResourceKind, output: &ShellOutput) -> Result<Listing> {
    ensure_success(&format!("kubectl get {}", kind.kubectl_type()), output)?;
    Ok(Listing {
        items: parse_items(&output.stdout)?,
        notice: None,
    })
}

fn parse_items(stdout: &[u8]) -> Result<Vec<JsonValue>> {
    let list: JsonValue =
        serde_json::from_slice(stdout).context("kubectl returned invalid JSON")?;
    Ok(match list {
        JsonValue::Object(mut list) => match list.remove("items") {
            Some(JsonValue::Array(items)) => items,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    })
}

pub(crate) fn parse_pod_metrics(stdout: &[u8]) -> Option<HashMap<String, Usage>> {
    let metrics: JsonValue = serde_json::from_slice(stdout).ok()?;
    Some(
        metrics
            .get("items")?
            .as_array()?
            .iter()
            .filter_map(|item| {
                let name = item.pointer("/metadata/name")?.as_str()?;
                let namespace = item.pointer("/metadata/namespace")?.as_str()?;
                let mut usage = Usage::default();
                for container in item.get("containers")?.as_array()? {
                    usage.cpu += parse_quantity(string_at(container, "/usage/cpu").as_deref());
                    usage.memory +=
                        parse_quantity(string_at(container, "/usage/memory").as_deref());
                }
                Some((format!("{namespace}/{name}"), usage))
            })
            .collect(),
    )
}

pub(crate) fn parse_node_metrics(stdout: &[u8]) -> Option<HashMap<String, Usage>> {
    let metrics: JsonValue = serde_json::from_slice(stdout).ok()?;
    Some(
        metrics
            .get("items")?
            .as_array()?
            .iter()
            .filter_map(|item| {
                Some((
                    item.pointer("/metadata/name")?.as_str()?.to_string(),
                    Usage {
                        cpu: parse_quantity(string_at(item, "/usage/cpu").as_deref()),
                        memory: parse_quantity(string_at(item, "/usage/memory").as_deref()),
                    },
                ))
            })
            .collect(),
    )
}

pub(crate) fn parse_list_output(output: &ShellOutput) -> Vec<JsonValue> {
    if output.success() {
        parse_items(&output.stdout).unwrap_or_default()
    } else {
        Vec::new()
    }
}

fn owned_by(object: &JsonValue, uid: Option<&str>) -> bool {
    let Some(uid) = uid else {
        return false;
    };
    object
        .pointer("/metadata/ownerReferences")
        .and_then(JsonValue::as_array)
        .is_some_and(|owners| {
            owners
                .iter()
                .any(|owner| owner.get("uid").and_then(JsonValue::as_str) == Some(uid))
        })
}

fn string_at(value: &JsonValue, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(JsonValue::as_str)
        .map(str::to_string)
}

fn text_field(value: &JsonValue, key: &str) -> String {
    value.get(key).map(json_scalar).unwrap_or_default()
}

fn json_scalar(value: &JsonValue) -> String {
    match value {
        JsonValue::String(value) => value.clone(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::Bool(value) => value.to_string(),
        _ => String::new(),
    }
}

fn safe_file_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn is_forbidden(output: &ShellOutput) -> bool {
    let error = String::from_utf8_lossy(&output.stderr);
    error.contains("Forbidden") || error.contains("forbidden")
}

pub fn ensure_success(command: &str, output: &ShellOutput) -> Result<()> {
    if output.success() {
        return Ok(());
    }
    let message = output
        .error_line()
        .unwrap_or_else(|| format!("exited with status {}", output.status));
    Err(anyhow!("{command} failed: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_arguments() {
        assert_eq!(shell_quote("get"), "get");
        assert_eq!(
            shell_quote("deployments.v1.apps/web"),
            "deployments.v1.apps/web"
        );
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(
            kubectl_command(&["get", "pods", "-o", "jsonpath={.items}"]),
            "kubectl --request-timeout=20s get pods -o 'jsonpath={.items}'"
        );
    }

    #[test]
    fn parses_metrics() {
        let pods = br#"{"items":[{"metadata":{"name":"web","namespace":"apps"},"containers":[
            {"usage":{"cpu":"250m","memory":"64Mi"}},{"usage":{"cpu":"250000000n","memory":"64Mi"}}]}]}"#;
        let usage = parse_pod_metrics(pods).and_then(|metrics| metrics.get("apps/web").cloned());
        assert_eq!(
            usage,
            Some(Usage {
                cpu: 0.5,
                memory: 128.0 * 1024.0 * 1024.0
            })
        );
        let nodes =
            br#"{"items":[{"metadata":{"name":"node"},"usage":{"cpu":"2","memory":"1Gi"}}]}"#;
        assert_eq!(
            parse_node_metrics(nodes).and_then(|metrics| metrics.get("node").cloned()),
            Some(Usage {
                cpu: 2.0,
                memory: 1024.0 * 1024.0 * 1024.0
            })
        );
    }
}
