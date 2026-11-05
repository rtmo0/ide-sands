use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;

use crate::{
    cluster::{Cluster, ContextInfo, parse_list_output, parse_node_metrics, parse_pod_metrics},
    resources::{ObjectRef, ResourceKind, builtin_kind, kind_for},
    table::{Tone, event_time, node_ready, parse_quantity, pod_status, str_at, workload_health},
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceTotals {
    pub usage: Option<f64>,
    pub requests: f64,
    pub limits: f64,
    pub allocatable: f64,
    pub capacity: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkloadSummary {
    pub kind: ResourceKind,
    pub total: usize,
    pub ok: usize,
    pub warn: usize,
    pub error: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Warning {
    pub reason: String,
    pub message: String,
    pub object: String,
    pub link: Option<ObjectRef>,
    pub count: u64,
    pub time: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Overview {
    pub context: ContextInfo,
    pub version: String,
    pub namespace: Option<String>,
    pub namespaces: Vec<String>,
    pub nodes_ready: usize,
    pub nodes_total: usize,
    pub pods_active: usize,
    pub pods_capacity: f64,
    pub cpu: ResourceTotals,
    pub memory: ResourceTotals,
    pub metrics_available: bool,
    pub workloads: Vec<WorkloadSummary>,
    pub warnings: Vec<Warning>,
    pub notices: Vec<String>,
}

const WORKLOADS: [&str; 6] = [
    "pods",
    "deployments",
    "statefulsets",
    "daemonsets",
    "jobs",
    "cronjobs",
];

/// The Lens "Cluster" page: capacity, usage, workload health and recent warnings, for the whole
/// cluster or one namespace.
pub async fn build_overview(
    cluster: &Cluster,
    context: ContextInfo,
    namespace: Option<&str>,
) -> Result<Overview> {
    let namespace = namespace.filter(|namespace| !namespace.is_empty());
    let scope = |command: &mut Vec<String>| match namespace {
        Some(namespace) => command.extend(["--namespace".to_string(), namespace.to_string()]),
        None => command.push("--all-namespaces".to_string()),
    };
    let mut commands = vec![
        vec![
            "get".to_string(),
            "nodes".to_string(),
            "-o".to_string(),
            "json".to_string(),
        ],
        vec![
            "get".to_string(),
            "--raw".to_string(),
            "/apis/metrics.k8s.io/v1beta1/nodes".to_string(),
        ],
        vec![
            "get".to_string(),
            "--raw".to_string(),
            match namespace {
                Some(namespace) => {
                    format!("/apis/metrics.k8s.io/v1beta1/namespaces/{namespace}/pods")
                }
                None => "/apis/metrics.k8s.io/v1beta1/pods".to_string(),
            },
        ],
        vec!["version".to_string(), "-o".to_string(), "json".to_string()],
        vec![
            "get".to_string(),
            "namespaces".to_string(),
            "-o".to_string(),
            "jsonpath={.items[*].metadata.name}".to_string(),
        ],
    ];
    let mut events = vec![
        "get".to_string(),
        "events".to_string(),
        "--field-selector".to_string(),
        "type=Warning".to_string(),
        "-o".to_string(),
        "json".to_string(),
    ];
    scope(&mut events);
    commands.push(events);
    let workload_kinds = WORKLOADS
        .iter()
        .filter_map(|id| builtin_kind(id))
        .collect::<Vec<_>>();
    for kind in &workload_kinds {
        let mut command = vec![
            "get".to_string(),
            kind.kubectl_type(),
            "-o".to_string(),
            "json".to_string(),
        ];
        scope(&mut command);
        commands.push(command);
    }
    let outputs = cluster.kubectl_batch(&commands).await?;
    let mut outputs = outputs.into_iter();
    let mut next = || {
        outputs.next().unwrap_or_else(|| sbx::ShellOutput {
            status: -1,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    };
    let nodes_output = next();
    let node_metrics_output = next();
    let pod_metrics_output = next();
    let version_output = next();
    let namespaces_output = next();
    let events_output = next();
    let workload_outputs = workload_kinds
        .iter()
        .map(|kind| (*kind, next()))
        .collect::<Vec<_>>();

    let mut notices = Vec::new();
    let nodes = parse_list_output(&nodes_output);
    if !nodes_output.success() {
        notices.push(format!(
            "Nodes: {}",
            nodes_output
                .error_line()
                .unwrap_or_else(|| "not available".to_string())
        ));
    }
    let node_metrics = node_metrics_output
        .success()
        .then(|| parse_node_metrics(&node_metrics_output.stdout))
        .flatten();
    let pod_metrics = pod_metrics_output
        .success()
        .then(|| parse_pod_metrics(&pod_metrics_output.stdout))
        .flatten();
    let metrics_available = if namespace.is_some() {
        pod_metrics.is_some()
    } else {
        node_metrics.is_some()
    };
    if !metrics_available {
        notices.push("CPU and memory usage need metrics-server in the cluster.".to_string());
    }

    let mut pods = Vec::new();
    let mut workloads = Vec::new();
    for (kind, output) in &workload_outputs {
        let items = parse_list_output(output);
        if !output.success() && !crate::cluster::is_forbidden(output) {
            notices.push(format!(
                "{}: {}",
                kind.title,
                output
                    .error_line()
                    .unwrap_or_else(|| "not available".to_string())
            ));
        }
        let mut summary = WorkloadSummary {
            kind: (*kind).clone(),
            total: items.len(),
            ok: 0,
            warn: 0,
            error: 0,
        };
        for item in &items {
            let tone = match kind.id.as_str() {
                "pods" => pod_status(item).1,
                "cronjobs" => {
                    if item.pointer("/spec/suspend").and_then(JsonValue::as_bool) == Some(true) {
                        Tone::Warn
                    } else {
                        Tone::Ok
                    }
                }
                id => workload_health(item, id).tone,
            };
            match tone {
                Tone::Error => summary.error += 1,
                Tone::Warn => summary.warn += 1,
                Tone::Ok | Tone::Info | Tone::Muted => summary.ok += 1,
            }
        }
        if kind.id == "pods" {
            pods = items;
        }
        workloads.push(summary);
    }

    let active_pods = pods
        .iter()
        .filter(|pod| !matches!(str_at(pod, "/status/phase"), "Succeeded" | "Failed"))
        .collect::<Vec<_>>();
    let container_total = |resource: &str, field: &str| -> f64 {
        active_pods
            .iter()
            .flat_map(|pod| crate::table::array_at(pod, "/spec/containers"))
            .map(|container| {
                parse_quantity(
                    container
                        .pointer(&format!("/resources/{field}/{resource}"))
                        .and_then(JsonValue::as_str),
                )
            })
            .sum()
    };
    let node_total = |key: &str, field: &str| -> f64 {
        nodes
            .iter()
            .map(|node| {
                parse_quantity(
                    node.pointer(&format!("/status/{field}/{key}"))
                        .and_then(JsonValue::as_str),
                )
            })
            .sum()
    };
    let usage = |pick: fn(&crate::cluster::Usage) -> f64| -> Option<f64> {
        if namespace.is_some() {
            pod_metrics
                .as_ref()
                .map(|metrics| metrics.values().map(pick).sum())
        } else {
            node_metrics
                .as_ref()
                .map(|metrics| metrics.values().map(pick).sum())
        }
    };

    let mut events = parse_list_output(&events_output);
    events.sort_by_key(|event| std::cmp::Reverse(event_time(event)));
    let warnings = events
        .iter()
        .take(50)
        .map(|event| {
            let involved = event
                .get("involvedObject")
                .or_else(|| event.get("regarding"))
                .unwrap_or(&JsonValue::Null);
            let kind_name = str_at(involved, "/kind");
            let name = str_at(involved, "/name");
            let object_namespace = involved
                .get("namespace")
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            Warning {
                reason: str_at(event, "/reason").to_string(),
                message: str_at(event, "/message").to_string(),
                object: match &object_namespace {
                    Some(namespace) => format!("{namespace}/{kind_name}/{name}"),
                    None => format!("{kind_name}/{name}"),
                },
                link: kind_for(
                    involved.get("apiVersion").and_then(JsonValue::as_str),
                    kind_name,
                    &[],
                )
                .filter(|_| !name.is_empty())
                .map(|kind| ObjectRef::new(kind, name, object_namespace)),
                count: event.get("count").and_then(JsonValue::as_u64).unwrap_or(1),
                time: event_time(event),
            }
        })
        .collect();

    let version = serde_json::from_slice::<JsonValue>(&version_output.stdout)
        .ok()
        .and_then(|version| {
            version
                .pointer("/serverVersion/gitVersion")
                .and_then(JsonValue::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let mut namespaces = String::from_utf8_lossy(&namespaces_output.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    namespaces.sort();

    Ok(Overview {
        context,
        version,
        namespace: namespace.map(str::to_string),
        namespaces,
        nodes_ready: nodes.iter().filter(|node| node_ready(node)).count(),
        nodes_total: nodes.len(),
        pods_active: active_pods.len(),
        pods_capacity: node_total("pods", "allocatable"),
        cpu: ResourceTotals {
            usage: usage(|usage| usage.cpu),
            requests: container_total("cpu", "requests"),
            limits: container_total("cpu", "limits"),
            allocatable: node_total("cpu", "allocatable"),
            capacity: node_total("cpu", "capacity"),
        },
        memory: ResourceTotals {
            usage: usage(|usage| usage.memory),
            requests: container_total("memory", "requests"),
            limits: container_total("memory", "limits"),
            allocatable: node_total("memory", "allocatable"),
            capacity: node_total("memory", "capacity"),
        },
        metrics_available,
        workloads,
        warnings,
        notices,
    })
}
