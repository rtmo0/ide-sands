use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;

use crate::{
    cluster::Usage,
    resources::{ObjectRef, ResourceKind, kind_for},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tone {
    Ok,
    Warn,
    Error,
    Info,
    Muted,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cell {
    pub text: String,
    pub tone: Option<Tone>,
    pub badge: bool,
    /// Sorts numerically when set, by text otherwise.
    pub sort: Option<f64>,
    pub tooltip: Option<String>,
    pub link: Option<ObjectRef>,
    /// Share of a total, drawn as a bar.
    pub ratio: Option<f64>,
    /// Container states, drawn as dots.
    pub dots: Vec<(Tone, String)>,
}

impl Cell {
    pub fn text(value: impl Into<String>) -> Self {
        let text = value.into();
        Self {
            text: if text.is_empty() {
                "—".to_string()
            } else {
                text
            },
            ..Default::default()
        }
    }

    fn toned(value: impl Into<String>, tone: Tone) -> Self {
        Self {
            tone: Some(tone),
            ..Self::text(value)
        }
    }

    fn badge(value: impl Into<String>, tone: Tone) -> Self {
        Self {
            badge: true,
            ..Self::toned(value, tone)
        }
    }

    fn number(value: f64, display: impl Into<String>) -> Self {
        Self {
            sort: Some(value),
            ..Self::text(display)
        }
    }

    fn count(value: u64) -> Self {
        Self::number(value as f64, value.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub id: &'static str,
    pub title: &'static str,
    pub align_right: bool,
    /// Hidden while the table shows a single namespace.
    pub namespace_column: bool,
}

const fn column(id: &'static str, title: &'static str) -> Column {
    Column {
        id,
        title,
        align_right: false,
        namespace_column: false,
    }
}

const fn right(id: &'static str, title: &'static str) -> Column {
    Column {
        id,
        title,
        align_right: true,
        namespace_column: false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Logs,
    Shell,
    PortForward,
    Scale,
    Restart,
    Trigger,
    Suspend,
    Resume,
    Cordon,
    Uncordon,
    Drain,
    EditYaml,
    Delete,
}

impl Action {
    pub fn title(self) -> &'static str {
        match self {
            Self::Logs => "Logs",
            Self::Shell => "Shell",
            Self::PortForward => "Port Forward",
            Self::Scale => "Scale",
            Self::Restart => "Restart",
            Self::Trigger => "Trigger",
            Self::Suspend => "Suspend",
            Self::Resume => "Resume",
            Self::Cordon => "Cordon",
            Self::Uncordon => "Uncordon",
            Self::Drain => "Drain",
            Self::EditYaml => "Edit YAML",
            Self::Delete => "Delete",
        }
    }

    /// Actions that change the cluster in a way that is hard to undo ask before running.
    pub fn is_dangerous(self) -> bool {
        matches!(self, Self::Delete | Self::Drain | Self::Restart)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: String,
    pub reference: ObjectRef,
    pub cells: Vec<Cell>,
    pub actions: Vec<Action>,
    /// Text the search box matches in addition to the cells: labels and the namespace.
    pub search: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    pub notice: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct TableContext {
    pub pod_metrics: Option<HashMap<String, Usage>>,
    pub node_metrics: Option<HashMap<String, Usage>>,
    pub pods_per_node: Option<HashMap<String, usize>>,
    pub custom_kinds: Vec<ResourceKind>,
}

type CellFn<'a> = Box<dyn Fn(&JsonValue) -> Cell + 'a>;

pub fn build_table(
    kind: &ResourceKind,
    items: &[JsonValue],
    context: &TableContext,
    now: DateTime<Utc>,
) -> Table {
    let specs = column_specs(kind, items, context, now);
    let columns = specs.iter().map(|(column, _)| column.clone()).collect();
    let rows = items
        .iter()
        .filter_map(|object| {
            let reference = ObjectRef::of(kind, object)?;
            let labels = object
                .pointer("/metadata/labels")
                .and_then(JsonValue::as_object)
                .map(|labels| {
                    labels
                        .iter()
                        .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or_default()))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            Some(Row {
                key: format!(
                    "{}/{}",
                    reference.namespace.as_deref().unwrap_or_default(),
                    reference.name
                ),
                cells: specs.iter().map(|(_, cell)| cell(object)).collect(),
                actions: row_actions(kind, object),
                search: format!(
                    "{} {} {labels}",
                    reference.name,
                    reference.namespace.as_deref().unwrap_or_default()
                )
                .to_lowercase(),
                reference,
            })
        })
        .collect();
    Table {
        columns,
        rows,
        notice: None,
    }
}

pub fn row_actions(kind: &ResourceKind, object: &JsonValue) -> Vec<Action> {
    let mut actions = match kind.id.as_str() {
        "pods" => vec![Action::Logs, Action::Shell, Action::PortForward],
        "deployments" | "statefulsets" => {
            vec![Action::Logs, Action::Scale, Action::Restart]
        }
        "replicasets" => vec![Action::Logs, Action::Scale],
        "daemonsets" => vec![Action::Logs, Action::Restart],
        "jobs" => vec![Action::Logs],
        "cronjobs" => {
            let suspended =
                object.pointer("/spec/suspend").and_then(JsonValue::as_bool) == Some(true);
            vec![
                Action::Trigger,
                if suspended {
                    Action::Resume
                } else {
                    Action::Suspend
                },
            ]
        }
        "services" => vec![Action::PortForward],
        "nodes" => {
            let cordoned = object
                .pointer("/spec/unschedulable")
                .and_then(JsonValue::as_bool)
                == Some(true);
            vec![
                if cordoned {
                    Action::Uncordon
                } else {
                    Action::Cordon
                },
                Action::Drain,
            ]
        }
        _ => Vec::new(),
    };
    if kind.id != "events" {
        actions.push(Action::EditYaml);
        actions.push(Action::Delete);
    }
    actions
}

fn column_specs<'a>(
    kind: &'a ResourceKind,
    items: &'a [JsonValue],
    context: &'a TableContext,
    now: DateTime<Utc>,
) -> Vec<(Column, CellFn<'a>)> {
    let name: (Column, CellFn<'a>) = (
        column("name", "Name"),
        Box::new(|object| Cell::text(str_at(object, "/metadata/name"))),
    );
    let namespace = || -> (Column, CellFn<'a>) {
        (
            Column {
                namespace_column: true,
                ..column("namespace", "Namespace")
            },
            Box::new(|object| Cell::text(str_at(object, "/metadata/namespace"))),
        )
    };
    let age = move || -> (Column, CellFn<'a>) {
        (
            right("age", "Age"),
            Box::new(move |object| {
                age_cell(timestamp_at(object, "/metadata/creationTimestamp"), now)
            }),
        )
    };
    let owner = || -> (Column, CellFn<'a>) {
        (
            column("owner", "Controlled By"),
            Box::new(|object| owner_cell(object, &context.custom_kinds)),
        )
    };
    let count_at = |id: &'static str,
                    title: &'static str,
                    pointer: &'static str,
                    default: u64|
     -> (Column, CellFn<'a>) {
        (
            right(id, title),
            Box::new(move |object| {
                Cell::count(
                    object
                        .pointer(pointer)
                        .and_then(JsonValue::as_u64)
                        .unwrap_or(default),
                )
            }),
        )
    };
    let text_at =
        |id: &'static str, title: &'static str, pointer: &'static str| -> (Column, CellFn<'a>) {
            (
                column(id, title),
                Box::new(move |object| Cell::text(str_at(object, pointer))),
            )
        };
    let workload_pods = |kind_id: &'a str| -> (Column, CellFn<'a>) {
        (
            column("pods", "Pods"),
            Box::new(move |object| {
                let health = workload_health(object, kind_id);
                Cell {
                    tone: Some(health.tone),
                    sort: Some(if health.desired == 0 {
                        1.0
                    } else {
                        health.ready as f64 / health.desired as f64
                    }),
                    ..Cell::text(format!("{}/{}", health.ready, health.desired))
                }
            }),
        )
    };

    match kind.id.as_str() {
        "pods" => {
            let usage = |object: &JsonValue| -> Option<&'a Usage> {
                let key = format!(
                    "{}/{}",
                    str_at(object, "/metadata/namespace"),
                    str_at(object, "/metadata/name")
                );
                context.pod_metrics.as_ref()?.get(&key)
            };
            vec![
                name,
                namespace(),
                (
                    column("containers", "Containers"),
                    Box::new(|object| Cell {
                        dots: container_dots(object),
                        sort: Some(array_len(object, "/spec/containers") as f64),
                        ..Cell::default()
                    }),
                ),
                (
                    right("cpu", "CPU"),
                    Box::new(move |object| {
                        pod_usage_cell(object, usage(object).map(|usage| usage.cpu), Resource::Cpu)
                    }),
                ),
                (
                    right("memory", "Memory"),
                    Box::new(move |object| {
                        pod_usage_cell(
                            object,
                            usage(object).map(|usage| usage.memory),
                            Resource::Memory,
                        )
                    }),
                ),
                (
                    right("restarts", "Restarts"),
                    Box::new(|object| {
                        let restarts = pod_restarts(object);
                        Cell {
                            tone: (restarts > 0).then_some(Tone::Warn),
                            ..Cell::count(restarts)
                        }
                    }),
                ),
                owner(),
                (
                    column("node", "Node"),
                    Box::new(|object| {
                        let node = str_at(object, "/spec/nodeName");
                        Cell {
                            link: crate::resources::builtin_kind("nodes")
                                .filter(|_| !node.is_empty())
                                .map(|kind| ObjectRef::new(kind.clone(), node, None)),
                            ..Cell::text(node)
                        }
                    }),
                ),
                text_at("qos", "QoS", "/status/qosClass"),
                age(),
                (
                    column("status", "Status"),
                    Box::new(|object| {
                        let (text, tone) = pod_status(object);
                        Cell::badge(text, tone)
                    }),
                ),
            ]
        }
        "nodes" => {
            let usage_cell = move |object: &JsonValue, resource: Resource| {
                let name = str_at(object, "/metadata/name");
                let used = context
                    .node_metrics
                    .as_ref()
                    .and_then(|metrics| metrics.get(name))
                    .map(|usage| resource.of(usage));
                let allocatable = parse_quantity(
                    object
                        .pointer(&format!("/status/allocatable/{}", resource.key()))
                        .and_then(JsonValue::as_str),
                );
                match used {
                    Some(used) if allocatable > 0.0 => ratio_cell(
                        used,
                        allocatable,
                        "allocatable",
                        resource,
                        used / allocatable,
                    ),
                    _ => Cell::text(""),
                }
            };
            vec![
                name,
                (
                    right("cpu", "CPU"),
                    Box::new(move |object| usage_cell(object, Resource::Cpu)),
                ),
                (
                    right("memory", "Memory"),
                    Box::new(move |object| usage_cell(object, Resource::Memory)),
                ),
                (
                    right("pods", "Pods"),
                    Box::new(|object| {
                        let capacity = parse_quantity(
                            object
                                .pointer("/status/allocatable/pods")
                                .and_then(JsonValue::as_str),
                        );
                        match &context.pods_per_node {
                            Some(pods) => {
                                let count = pods
                                    .get(str_at(object, "/metadata/name"))
                                    .copied()
                                    .unwrap_or_default();
                                Cell::number(count as f64, format!("{count}/{capacity}"))
                            }
                            None => Cell::text(format!("{capacity}")),
                        }
                    }),
                ),
                (
                    column("roles", "Roles"),
                    Box::new(|object| Cell::text(node_roles(object))),
                ),
                text_at("version", "Version", "/status/nodeInfo/kubeletVersion"),
                (
                    column("ip", "Internal IP"),
                    Box::new(|object| Cell::text(node_address(object, "InternalIP"))),
                ),
                (
                    right("taints", "Taints"),
                    Box::new(|object| {
                        let taints = object
                            .pointer("/spec/taints")
                            .and_then(JsonValue::as_array)
                            .map(Vec::as_slice)
                            .unwrap_or_default();
                        Cell {
                            tooltip: Some(
                                taints
                                    .iter()
                                    .map(|taint| {
                                        format!(
                                            "{}={}:{}",
                                            str_at(taint, "/key"),
                                            str_at(taint, "/value"),
                                            str_at(taint, "/effect")
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            )
                            .filter(|tooltip| !tooltip.is_empty()),
                            ..Cell::count(taints.len() as u64)
                        }
                    }),
                ),
                age(),
                (
                    column("status", "Status"),
                    Box::new(|object| {
                        let (text, tone) = node_status(object);
                        Cell::badge(text, tone)
                    }),
                ),
            ]
        }
        "namespaces" => vec![
            name,
            (
                column("labels", "Labels"),
                Box::new(|object| Cell::text(selector_string(object.pointer("/metadata/labels")))),
            ),
            age(),
            (
                column("status", "Status"),
                Box::new(|object| {
                    let phase = object
                        .pointer("/status/phase")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("Active");
                    Cell::badge(
                        phase,
                        if phase == "Terminating" {
                            Tone::Warn
                        } else {
                            Tone::Ok
                        },
                    )
                }),
            ),
        ],
        "events" => vec![
            (
                column("type", "Type"),
                Box::new(|object| {
                    let event_type = str_at(object, "/type");
                    Cell::badge(
                        event_type,
                        if event_type == "Warning" {
                            Tone::Warn
                        } else {
                            Tone::Muted
                        },
                    )
                }),
            ),
            text_at("reason", "Reason", "/reason"),
            (
                column("message", "Message"),
                Box::new(|object| Cell {
                    tooltip: Some(str_at(object, "/message").to_string()),
                    ..Cell::text(str_at(object, "/message"))
                }),
            ),
            namespace(),
            (
                column("object", "Involved Object"),
                Box::new(|object| {
                    let involved = object
                        .get("involvedObject")
                        .or_else(|| object.get("regarding"))
                        .unwrap_or(&JsonValue::Null);
                    let kind_name = str_at(involved, "/kind");
                    let name = str_at(involved, "/name");
                    let link = kind_for(
                        involved.get("apiVersion").and_then(JsonValue::as_str),
                        kind_name,
                        &context.custom_kinds,
                    )
                    .filter(|_| !name.is_empty())
                    .map(|kind| {
                        ObjectRef::new(
                            kind,
                            name,
                            involved
                                .get("namespace")
                                .and_then(JsonValue::as_str)
                                .map(str::to_string),
                        )
                    });
                    Cell {
                        link,
                        ..Cell::text(format!("{kind_name}/{name}"))
                    }
                }),
            ),
            (
                column("source", "Source"),
                Box::new(|object| {
                    let source = str_at(object, "/source/component");
                    Cell::text(if source.is_empty() {
                        str_at(object, "/reportingComponent")
                    } else {
                        source
                    })
                }),
            ),
            count_at("count", "Count", "/count", 1),
            (
                right("lastSeen", "Last Seen"),
                Box::new(move |object| age_cell(event_time(object), now)),
            ),
        ],
        "deployments" => vec![
            name,
            namespace(),
            workload_pods("deployments"),
            count_at("replicas", "Replicas", "/spec/replicas", 1),
            count_at("updated", "Up-to-date", "/status/updatedReplicas", 0),
            count_at("available", "Available", "/status/availableReplicas", 0),
            age(),
            (
                column("conditions", "Conditions"),
                Box::new(conditions_cell),
            ),
        ],
        "daemonsets" => vec![
            name,
            namespace(),
            workload_pods("daemonsets"),
            count_at("desired", "Desired", "/status/desiredNumberScheduled", 0),
            count_at("current", "Current", "/status/currentNumberScheduled", 0),
            count_at("available", "Available", "/status/numberAvailable", 0),
            (
                column("nodeSelector", "Node Selector"),
                Box::new(|object| {
                    Cell::text(selector_string(
                        object.pointer("/spec/template/spec/nodeSelector"),
                    ))
                }),
            ),
            age(),
        ],
        "statefulsets" => vec![
            name,
            namespace(),
            workload_pods("statefulsets"),
            count_at("replicas", "Replicas", "/spec/replicas", 1),
            text_at("service", "Service", "/spec/serviceName"),
            age(),
        ],
        "replicasets" => vec![
            name,
            namespace(),
            workload_pods("replicasets"),
            count_at("desired", "Desired", "/spec/replicas", 1),
            count_at("current", "Current", "/status/replicas", 0),
            owner(),
            age(),
        ],
        "jobs" => vec![
            name,
            namespace(),
            workload_pods("jobs"),
            (
                right("duration", "Duration"),
                Box::new(move |object| {
                    let start = timestamp_at(object, "/status/startTime");
                    let end = timestamp_at(object, "/status/completionTime").unwrap_or(now);
                    match start {
                        Some(start) => {
                            let seconds = (end - start).num_seconds().max(0);
                            Cell::number(seconds as f64, format_duration(seconds))
                        }
                        None => Cell::text(""),
                    }
                }),
            ),
            owner(),
            age(),
            (
                column("status", "Status"),
                Box::new(|object| {
                    let (text, tone) = job_status(object);
                    Cell::badge(text, tone)
                }),
            ),
        ],
        "cronjobs" => vec![
            name,
            namespace(),
            text_at("schedule", "Schedule", "/spec/schedule"),
            (
                column("suspend", "Suspend"),
                Box::new(|object| {
                    if object.pointer("/spec/suspend").and_then(JsonValue::as_bool) == Some(true) {
                        Cell::badge("Suspended", Tone::Warn)
                    } else {
                        Cell::toned("False", Tone::Muted)
                    }
                }),
            ),
            (
                right("active", "Active"),
                Box::new(|object| Cell::count(array_len(object, "/status/active") as u64)),
            ),
            (
                right("lastSchedule", "Last Schedule"),
                Box::new(move |object| {
                    age_cell(timestamp_at(object, "/status/lastScheduleTime"), now)
                }),
            ),
            age(),
        ],
        "configmaps" => vec![
            name,
            namespace(),
            (
                column("keys", "Keys"),
                Box::new(|object| keys_cell(object, &["data", "binaryData"])),
            ),
            age(),
        ],
        "secrets" => vec![
            name,
            namespace(),
            text_at("type", "Type", "/type"),
            (
                column("keys", "Keys"),
                Box::new(|object| keys_cell(object, &["data"])),
            ),
            age(),
        ],
        "resourcequotas" => vec![
            name,
            namespace(),
            (
                column("used", "Used / Hard"),
                Box::new(|object| {
                    let hard = object
                        .pointer("/status/hard")
                        .and_then(JsonValue::as_object);
                    let used = object.pointer("/status/used");
                    Cell::text(
                        hard.map(|hard| {
                            hard.iter()
                                .map(|(key, limit)| {
                                    format!(
                                        "{key}: {}/{}",
                                        used.and_then(|used| used.get(key))
                                            .and_then(JsonValue::as_str)
                                            .unwrap_or("0"),
                                        limit.as_str().unwrap_or_default()
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                    )
                }),
            ),
            age(),
        ],
        "limitranges" => vec![name, namespace(), age()],
        "hpa" => vec![
            name,
            namespace(),
            (
                column("reference", "Reference"),
                Box::new(|object| {
                    Cell::text(format!(
                        "{}/{}",
                        str_at(object, "/spec/scaleTargetRef/kind"),
                        str_at(object, "/spec/scaleTargetRef/name")
                    ))
                }),
            ),
            (
                column("targets", "Targets"),
                Box::new(|object| Cell::text(hpa_targets(object))),
            ),
            count_at("min", "Min Pods", "/spec/minReplicas", 1),
            count_at("max", "Max Pods", "/spec/maxReplicas", 0),
            count_at("replicas", "Replicas", "/status/currentReplicas", 0),
            age(),
        ],
        "pdb" => vec![
            name,
            namespace(),
            (
                column("min", "Min Available"),
                Box::new(|object| Cell::text(scalar_at(object, "/spec/minAvailable"))),
            ),
            (
                column("max", "Max Unavailable"),
                Box::new(|object| Cell::text(scalar_at(object, "/spec/maxUnavailable"))),
            ),
            count_at(
                "allowed",
                "Allowed Disruptions",
                "/status/disruptionsAllowed",
                0,
            ),
            age(),
        ],
        "priorityclasses" => vec![
            name,
            count_at("value", "Value", "/value", 0),
            (
                column("default", "Global Default"),
                Box::new(|object| {
                    Cell::text(
                        if object.get("globalDefault").and_then(JsonValue::as_bool) == Some(true) {
                            "True"
                        } else {
                            "False"
                        },
                    )
                }),
            ),
            age(),
        ],
        "services" => vec![
            name,
            namespace(),
            text_at("type", "Type", "/spec/type"),
            text_at("clusterIP", "Cluster IP", "/spec/clusterIP"),
            (
                column("ports", "Ports"),
                Box::new(|object| Cell::text(service_ports(object))),
            ),
            (
                column("external", "External IP"),
                Box::new(|object| Cell::text(external_addresses(object))),
            ),
            (
                column("selector", "Selector"),
                Box::new(|object| Cell::text(selector_string(object.pointer("/spec/selector")))),
            ),
            age(),
        ],
        "endpoints" => vec![
            name,
            namespace(),
            (
                column("endpoints", "Endpoints"),
                Box::new(|object| {
                    let mut addresses = Vec::new();
                    for subset in array_at(object, "/subsets") {
                        let ports = array_at(subset, "/ports");
                        for address in array_at(subset, "/addresses") {
                            let ip = str_at(address, "/ip");
                            if ports.is_empty() {
                                addresses.push(ip.to_string());
                            }
                            for port in ports {
                                addresses.push(format!("{ip}:{}", scalar_at(port, "/port")));
                            }
                        }
                    }
                    let shown = addresses
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ");
                    let more = addresses.len().saturating_sub(5);
                    Cell::number(
                        addresses.len() as f64,
                        if addresses.is_empty() {
                            "<none>".to_string()
                        } else if more > 0 {
                            format!("{shown} +{more} more")
                        } else {
                            shown
                        },
                    )
                }),
            ),
            age(),
        ],
        "ingresses" => vec![
            name,
            namespace(),
            (
                column("class", "Class"),
                Box::new(|object| {
                    let class = str_at(object, "/spec/ingressClassName");
                    Cell::text(if class.is_empty() {
                        str_at(object, "/metadata/annotations/kubernetes.io~1ingress.class")
                    } else {
                        class
                    })
                }),
            ),
            (
                column("hosts", "Hosts"),
                Box::new(|object| {
                    Cell::text(
                        array_at(object, "/spec/rules")
                            .iter()
                            .map(|rule| str_at(rule, "/host"))
                            .filter(|host| !host.is_empty())
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                }),
            ),
            (
                column("address", "Load Balancer"),
                Box::new(|object| Cell::text(external_addresses(object))),
            ),
            age(),
        ],
        "ingressclasses" => vec![
            name,
            text_at("controller", "Controller", "/spec/controller"),
            (
                column("default", "Default"),
                Box::new(|object| {
                    Cell::text(
                        if str_at(
                            object,
                            "/metadata/annotations/ingressclass.kubernetes.io~1is-default-class",
                        ) == "true"
                        {
                            "Yes"
                        } else {
                            ""
                        },
                    )
                }),
            ),
            age(),
        ],
        "networkpolicies" => vec![
            name,
            namespace(),
            (
                column("podSelector", "Pod Selector"),
                Box::new(|object| {
                    let selector = selector_string(object.pointer("/spec/podSelector/matchLabels"));
                    Cell::text(if selector.is_empty() {
                        "<all pods>".to_string()
                    } else {
                        selector
                    })
                }),
            ),
            (
                column("types", "Policy Types"),
                Box::new(|object| Cell::text(string_list(object, "/spec/policyTypes"))),
            ),
            age(),
        ],
        "pvc" => vec![
            name,
            namespace(),
            (
                column("status", "Status"),
                Box::new(|object| {
                    let phase = str_at(object, "/status/phase");
                    Cell::badge(
                        phase,
                        if phase == "Bound" {
                            Tone::Ok
                        } else {
                            Tone::Warn
                        },
                    )
                }),
            ),
            text_at("volume", "Volume", "/spec/volumeName"),
            (
                right("capacity", "Capacity"),
                Box::new(|object| quantity_cell(object, "/status/capacity/storage")),
            ),
            (
                column("access", "Access Modes"),
                Box::new(|object| Cell::text(string_list(object, "/spec/accessModes"))),
            ),
            text_at("storageClass", "Storage Class", "/spec/storageClassName"),
            age(),
        ],
        "pv" => vec![
            name,
            (
                right("capacity", "Capacity"),
                Box::new(|object| quantity_cell(object, "/spec/capacity/storage")),
            ),
            (
                column("access", "Access Modes"),
                Box::new(|object| Cell::text(string_list(object, "/spec/accessModes"))),
            ),
            text_at(
                "reclaim",
                "Reclaim Policy",
                "/spec/persistentVolumeReclaimPolicy",
            ),
            (
                column("status", "Status"),
                Box::new(|object| {
                    let phase = str_at(object, "/status/phase");
                    Cell::badge(
                        phase,
                        match phase {
                            "Bound" => Tone::Ok,
                            "Available" => Tone::Info,
                            _ => Tone::Warn,
                        },
                    )
                }),
            ),
            (
                column("claim", "Claim"),
                Box::new(|object| {
                    let name = str_at(object, "/spec/claimRef/name");
                    let namespace = str_at(object, "/spec/claimRef/namespace");
                    Cell {
                        link: crate::resources::builtin_kind("pvc")
                            .filter(|_| !name.is_empty())
                            .map(|kind| {
                                ObjectRef::new(kind.clone(), name, Some(namespace.to_string()))
                            }),
                        ..Cell::text(if name.is_empty() {
                            String::new()
                        } else {
                            format!("{namespace}/{name}")
                        })
                    }
                }),
            ),
            text_at("storageClass", "Storage Class", "/spec/storageClassName"),
            age(),
        ],
        "storageclasses" => vec![
            name,
            text_at("provisioner", "Provisioner", "/provisioner"),
            text_at("reclaim", "Reclaim Policy", "/reclaimPolicy"),
            text_at("binding", "Binding Mode", "/volumeBindingMode"),
            (
                column("default", "Default"),
                Box::new(|object| {
                    Cell::text(
                        if str_at(
                            object,
                            "/metadata/annotations/storageclass.kubernetes.io~1is-default-class",
                        ) == "true"
                        {
                            "Yes"
                        } else {
                            ""
                        },
                    )
                }),
            ),
            age(),
        ],
        "serviceaccounts" => vec![
            name,
            namespace(),
            (
                right("secrets", "Secrets"),
                Box::new(|object| Cell::count(array_len(object, "/secrets") as u64)),
            ),
            age(),
        ],
        "roles" => vec![
            name,
            namespace(),
            (
                right("rules", "Rules"),
                Box::new(|object| Cell::count(array_len(object, "/rules") as u64)),
            ),
            age(),
        ],
        "clusterroles" => vec![
            name,
            (
                right("rules", "Rules"),
                Box::new(|object| Cell::count(array_len(object, "/rules") as u64)),
            ),
            age(),
        ],
        "rolebindings" => vec![
            name,
            namespace(),
            (column("role", "Role"), Box::new(role_reference_cell)),
            (column("subjects", "Subjects"), Box::new(subjects_cell)),
            age(),
        ],
        "clusterrolebindings" => vec![
            name,
            (column("role", "Role"), Box::new(role_reference_cell)),
            (column("subjects", "Subjects"), Box::new(subjects_cell)),
            age(),
        ],
        "crds" => vec![
            (
                column("kind", "Resource"),
                Box::new(|object| Cell::text(str_at(object, "/spec/names/kind"))),
            ),
            text_at("group", "Group", "/spec/group"),
            (
                column("version", "Version"),
                Box::new(|object| {
                    Cell::text(
                        array_at(object, "/spec/versions")
                            .iter()
                            .filter(|version| {
                                version.get("served").and_then(JsonValue::as_bool) != Some(false)
                            })
                            .map(|version| str_at(version, "/name"))
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                }),
            ),
            text_at("scope", "Scope", "/spec/scope"),
            age(),
        ],
        _ if kind.custom => custom_columns(kind, items, now, name, namespace, age),
        _ => {
            let mut columns = vec![name];
            if kind.namespaced {
                columns.push(namespace());
            }
            columns.push(age());
            columns
        }
    }
}

/// Columns of a custom resource: its CRD's printer columns are not known from the objects, so
/// common status fields are shown instead.
fn custom_columns<'a>(
    kind: &'a ResourceKind,
    items: &'a [JsonValue],
    _now: DateTime<Utc>,
    name: (Column, CellFn<'a>),
    namespace: impl Fn() -> (Column, CellFn<'a>),
    age: impl Fn() -> (Column, CellFn<'a>),
) -> Vec<(Column, CellFn<'a>)> {
    let mut columns = vec![name];
    if kind.namespaced {
        columns.push(namespace());
    }
    let has_ready = items.iter().any(|item| condition(item, "Ready").is_some());
    if has_ready {
        columns.push((
            column("ready", "Ready"),
            Box::new(|object| match condition(object, "Ready") {
                Some(("True", _)) => Cell::badge("True", Tone::Ok),
                Some((status, reason)) => Cell {
                    tooltip: (!reason.is_empty()).then(|| reason.to_string()),
                    ..Cell::badge(status, Tone::Warn)
                },
                None => Cell::text(""),
            }),
        ));
    }
    columns.push(age());
    columns
}

fn condition<'a>(object: &'a JsonValue, condition_type: &str) -> Option<(&'a str, &'a str)> {
    array_at(object, "/status/conditions")
        .iter()
        .find(|condition| str_at(condition, "/type") == condition_type)
        .map(|condition| (str_at(condition, "/status"), str_at(condition, "/message")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Cpu,
    Memory,
}

impl Resource {
    pub fn key(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
        }
    }

    pub fn of(self, usage: &Usage) -> f64 {
        match self {
            Self::Cpu => usage.cpu,
            Self::Memory => usage.memory,
        }
    }

    pub fn format(self, value: f64) -> String {
        match self {
            Self::Cpu => format_cpu(value),
            Self::Memory => format_bytes(value),
        }
    }
}

fn ratio_cell(used: f64, total: f64, of: &str, resource: Resource, sort: f64) -> Cell {
    let ratio = used / total;
    Cell {
        text: format!("{} ({}%)", resource.format(used), (ratio * 100.0).round()),
        sort: Some(sort),
        ratio: Some(ratio),
        tooltip: Some(format!(
            "{} of {} {of}",
            resource.format(used),
            resource.format(total)
        )),
        tone: Some(if ratio > 0.9 {
            Tone::Error
        } else if ratio > 0.75 {
            Tone::Warn
        } else {
            Tone::Ok
        }),
        ..Cell::default()
    }
}

/// The pod total of a request or limit, only when every container sets it: otherwise the pod is
/// unbounded.
pub fn pod_resource(pod: &JsonValue, field: &str, resource: Resource) -> f64 {
    let containers = array_at(pod, "/spec/containers");
    if containers.is_empty() {
        return 0.0;
    }
    let mut total = 0.0;
    for container in containers {
        let value = container
            .pointer(&format!("/resources/{field}/{}", resource.key()))
            .and_then(JsonValue::as_str);
        if value.is_none() {
            return 0.0;
        }
        total += parse_quantity(value);
    }
    total
}

fn pod_usage_cell(pod: &JsonValue, used: Option<f64>, resource: Resource) -> Cell {
    let Some(used) = used else {
        return Cell::text("");
    };
    let limit = pod_resource(pod, "limits", resource);
    if limit > 0.0 {
        return ratio_cell(used, limit, "limit", resource, used);
    }
    let request = pod_resource(pod, "requests", resource);
    if request > 0.0 {
        return ratio_cell(used, request, "requested", resource, used);
    }
    Cell {
        tooltip: Some(format!(
            "{}, no requests or limits set",
            resource.format(used)
        )),
        ..Cell::number(used, resource.format(used))
    }
}

fn owner_cell(object: &JsonValue, custom_kinds: &[ResourceKind]) -> Cell {
    let Some((owner_kind, owner_name)) = owner_of(object) else {
        return Cell::text("");
    };
    Cell {
        link: kind_for(None, owner_kind, custom_kinds).map(|kind| {
            ObjectRef::new(
                kind,
                owner_name,
                object
                    .pointer("/metadata/namespace")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
            )
        }),
        ..Cell::text(format!("{owner_kind}/{owner_name}"))
    }
}

fn conditions_cell(object: &JsonValue) -> Cell {
    let conditions = array_at(object, "/status/conditions");
    let active = conditions
        .iter()
        .filter(|condition| str_at(condition, "/status") == "True")
        .map(|condition| str_at(condition, "/type"))
        .collect::<Vec<_>>();
    let bad = conditions.iter().any(|condition| {
        let (condition_type, status) = (str_at(condition, "/type"), str_at(condition, "/status"));
        (condition_type == "Available" && status != "True")
            || (condition_type == "ReplicaFailure" && status == "True")
    });
    Cell::toned(
        active.join(" "),
        if bad {
            Tone::Error
        } else if active.is_empty() {
            Tone::Muted
        } else {
            Tone::Ok
        },
    )
}

fn keys_cell(object: &JsonValue, fields: &[&str]) -> Cell {
    let keys = fields
        .iter()
        .filter_map(|field| object.get(*field).and_then(JsonValue::as_object))
        .flat_map(|data| data.keys().cloned())
        .collect::<Vec<_>>();
    Cell {
        tooltip: Some(keys.join("\n")).filter(|keys| !keys.is_empty()),
        ..Cell::number(keys.len() as f64, keys.join(", "))
    }
}

fn quantity_cell(object: &JsonValue, pointer: &str) -> Cell {
    let value = str_at(object, pointer);
    Cell::number(parse_quantity(Some(value)), value)
}

fn role_reference_cell(object: &JsonValue) -> Cell {
    Cell::text(format!(
        "{}/{}",
        str_at(object, "/roleRef/kind"),
        str_at(object, "/roleRef/name")
    ))
}

fn subjects_cell(object: &JsonValue) -> Cell {
    Cell::text(
        array_at(object, "/subjects")
            .iter()
            .map(|subject| format!("{}:{}", str_at(subject, "/kind"), str_at(subject, "/name")))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn hpa_targets(object: &JsonValue) -> String {
    let current = array_at(object, "/status/currentMetrics");
    array_at(object, "/spec/metrics")
        .iter()
        .enumerate()
        .map(|(index, metric)| {
            let resource = str_at(metric, "/resource/name");
            let target = scalar_at(metric, "/resource/target/averageUtilization");
            let now = current
                .get(index)
                .map(|current| scalar_at(current, "/resource/current/averageUtilization"))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "<unknown>".to_string());
            if resource.is_empty() {
                str_at(metric, "/type").to_string()
            } else {
                format!("{resource}: {now}%/{target}%")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn service_ports(object: &JsonValue) -> String {
    array_at(object, "/spec/ports")
        .iter()
        .map(|port| {
            let node_port = scalar_at(port, "/nodePort");
            let protocol = str_at(port, "/protocol");
            format!(
                "{}{}/{}",
                scalar_at(port, "/port"),
                if node_port.is_empty() {
                    String::new()
                } else {
                    format!(":{node_port}")
                },
                if protocol.is_empty() { "TCP" } else { protocol }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn external_addresses(object: &JsonValue) -> String {
    let mut addresses = array_at(object, "/status/loadBalancer/ingress")
        .iter()
        .map(|ingress| {
            let ip = str_at(ingress, "/ip");
            if ip.is_empty() {
                str_at(ingress, "/hostname").to_string()
            } else {
                ip.to_string()
            }
        })
        .collect::<Vec<_>>();
    addresses.extend(
        array_at(object, "/spec/externalIPs")
            .iter()
            .filter_map(JsonValue::as_str)
            .map(str::to_string),
    );
    if addresses.is_empty() && str_at(object, "/spec/type") == "ExternalName" {
        return str_at(object, "/spec/externalName").to_string();
    }
    addresses.join(", ")
}

const ERROR_REASONS: [&str; 16] = [
    "CrashLoopBackOff",
    "Error",
    "ImagePullBackOff",
    "ErrImagePull",
    "InvalidImageName",
    "CreateContainerConfigError",
    "CreateContainerError",
    "RunContainerError",
    "OOMKilled",
    "Failed",
    "Evicted",
    "ContainerStatusUnknown",
    "Unknown",
    "NodeLost",
    "DeadlineExceeded",
    "BackoffLimitExceeded",
];

pub fn reason_tone(reason: &str) -> Tone {
    if ERROR_REASONS.contains(&reason)
        || reason.starts_with("Init:Error")
        || reason.starts_with("Init:CrashLoop")
        || reason.starts_with("Signal:")
        || reason.starts_with("ExitCode:")
    {
        Tone::Error
    } else if reason == "Running" {
        Tone::Ok
    } else if reason == "Completed" || reason == "Succeeded" {
        Tone::Muted
    } else {
        Tone::Warn
    }
}

/// The STATUS column of `kubectl get pods`.
pub fn pod_status(pod: &JsonValue) -> (String, Tone) {
    let status = pod.get("status").unwrap_or(&JsonValue::Null);
    let mut reason = [str_at(status, "/reason"), str_at(status, "/phase")]
        .into_iter()
        .find(|reason| !reason.is_empty())
        .unwrap_or("Unknown")
        .to_string();

    let init_count = array_len(pod, "/spec/initContainers");
    let mut initializing = false;
    for (index, container) in array_at(status, "/initContainerStatuses")
        .iter()
        .enumerate()
    {
        let terminated = container.pointer("/state/terminated");
        let waiting = container.pointer("/state/waiting");
        if terminated.is_some_and(|terminated| {
            terminated.get("exitCode").and_then(JsonValue::as_i64) == Some(0)
        }) {
            continue;
        }
        // Restartable init containers (sidecars) that are running count as done.
        if container.get("started").and_then(JsonValue::as_bool) == Some(true)
            && container.pointer("/state/running").is_some()
        {
            continue;
        }
        initializing = true;
        reason = if let Some(terminated) = terminated {
            let terminated_reason = str_at(terminated, "/reason");
            if !terminated_reason.is_empty() {
                format!("Init:{terminated_reason}")
            } else if terminated.get("signal").is_some() {
                format!("Init:Signal:{}", scalar_at(terminated, "/signal"))
            } else {
                format!("Init:ExitCode:{}", scalar_at(terminated, "/exitCode"))
            }
        } else if let Some(waiting_reason) = waiting
            .map(|waiting| str_at(waiting, "/reason"))
            .filter(|reason| !reason.is_empty() && *reason != "PodInitializing")
        {
            format!("Init:{waiting_reason}")
        } else {
            format!("Init:{index}/{init_count}")
        };
        break;
    }

    if !initializing {
        let mut has_running = false;
        for container in array_at(status, "/containerStatuses").iter().rev() {
            let waiting_reason = str_at(container, "/state/waiting/reason");
            if !waiting_reason.is_empty() {
                reason = waiting_reason.to_string();
            } else if let Some(terminated) = container.pointer("/state/terminated") {
                let terminated_reason = str_at(terminated, "/reason");
                reason = if !terminated_reason.is_empty() {
                    terminated_reason.to_string()
                } else if terminated.get("signal").is_some() {
                    format!("Signal:{}", scalar_at(terminated, "/signal"))
                } else {
                    format!("ExitCode:{}", scalar_at(terminated, "/exitCode"))
                };
            } else if container.get("ready").and_then(JsonValue::as_bool) == Some(true)
                && container.pointer("/state/running").is_some()
            {
                has_running = true;
            }
        }
        if reason == "Completed" && has_running {
            reason = "Running".to_string();
        }
    }

    if pod.pointer("/metadata/deletionTimestamp").is_some() {
        reason = if str_at(status, "/reason") == "NodeLost" {
            "Unknown".to_string()
        } else {
            "Terminating".to_string()
        };
    }

    let tone = match reason.as_str() {
        "Running" => {
            let ready = array_at(status, "/containerStatuses")
                .iter()
                .all(|container| container.get("ready").and_then(JsonValue::as_bool) == Some(true));
            if ready { Tone::Ok } else { Tone::Warn }
        }
        "Terminating" => Tone::Muted,
        reason => reason_tone(reason),
    };
    (reason, tone)
}

pub fn container_state(status: Option<&JsonValue>) -> (String, Tone) {
    let Some(status) = status else {
        return ("Waiting".to_string(), Tone::Warn);
    };
    if status.pointer("/state/running").is_some() {
        return if status.get("ready").and_then(JsonValue::as_bool) == Some(true) {
            ("Running".to_string(), Tone::Ok)
        } else {
            ("Running (not ready)".to_string(), Tone::Warn)
        };
    }
    if let Some(waiting) = status.pointer("/state/waiting") {
        let reason = str_at(waiting, "/reason");
        let reason = if reason.is_empty() { "Waiting" } else { reason };
        return (reason.to_string(), reason_tone(reason));
    }
    if let Some(terminated) = status.pointer("/state/terminated") {
        let reason = str_at(terminated, "/reason");
        let exit_code = terminated
            .get("exitCode")
            .and_then(JsonValue::as_i64)
            .unwrap_or_default();
        let text = if reason.is_empty() {
            format!("ExitCode:{exit_code}")
        } else {
            reason.to_string()
        };
        return (
            text,
            if exit_code == 0 {
                Tone::Muted
            } else {
                Tone::Error
            },
        );
    }
    ("Unknown".to_string(), Tone::Warn)
}

pub fn container_status<'a>(pod: &'a JsonValue, name: &str, init: bool) -> Option<&'a JsonValue> {
    let pointer = if init {
        "/status/initContainerStatuses"
    } else {
        "/status/containerStatuses"
    };
    array_at(pod, pointer)
        .iter()
        .find(|status| str_at(status, "/name") == name)
}

pub fn container_dots(pod: &JsonValue) -> Vec<(Tone, String)> {
    let mut dots = Vec::new();
    for (pointer, init) in [("/spec/initContainers", true), ("/spec/containers", false)] {
        for container in array_at(pod, pointer) {
            let name = str_at(container, "/name");
            let (text, tone) = container_state(container_status(pod, name, init));
            let label = if init {
                format!("{name} (init)")
            } else {
                name.to_string()
            };
            dots.push((tone, format!("{label}: {text}")));
        }
    }
    dots
}

pub fn pod_restarts(pod: &JsonValue) -> u64 {
    array_at(pod, "/status/containerStatuses")
        .iter()
        .filter_map(|container| container.get("restartCount").and_then(JsonValue::as_u64))
        .sum()
}

pub fn node_ready(node: &JsonValue) -> bool {
    array_at(node, "/status/conditions")
        .iter()
        .any(|condition| {
            str_at(condition, "/type") == "Ready" && str_at(condition, "/status") == "True"
        })
}

pub fn node_status(node: &JsonValue) -> (String, Tone) {
    let ready = node_ready(node);
    let mut parts = vec![if ready { "Ready" } else { "NotReady" }.to_string()];
    if node
        .pointer("/spec/unschedulable")
        .and_then(JsonValue::as_bool)
        == Some(true)
    {
        parts.push("SchedulingDisabled".to_string());
    }
    for condition in array_at(node, "/status/conditions") {
        let condition_type = str_at(condition, "/type");
        if condition_type != "Ready" && str_at(condition, "/status") == "True" {
            parts.push(condition_type.to_string());
        }
    }
    let tone = if !ready {
        Tone::Error
    } else if parts.len() > 1 {
        Tone::Warn
    } else {
        Tone::Ok
    };
    (parts.join(", "), tone)
}

pub fn node_roles(node: &JsonValue) -> String {
    let roles = node
        .pointer("/metadata/labels")
        .and_then(JsonValue::as_object)
        .map(|labels| {
            labels
                .keys()
                .filter_map(|label| label.strip_prefix("node-role.kubernetes.io/"))
                .filter(|role| !role.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    if roles.is_empty() {
        "<none>".to_string()
    } else {
        roles
    }
}

pub fn node_address(node: &JsonValue, address_type: &str) -> String {
    array_at(node, "/status/addresses")
        .iter()
        .find(|address| str_at(address, "/type") == address_type)
        .map(|address| str_at(address, "/address").to_string())
        .unwrap_or_default()
}

pub struct WorkloadHealth {
    pub ready: u64,
    pub desired: u64,
    pub tone: Tone,
}

/// Health of a workload controller from its ready and desired replicas.
pub fn workload_health(object: &JsonValue, kind_id: &str) -> WorkloadHealth {
    let number = |pointer: &str, default: u64| {
        object
            .pointer(pointer)
            .and_then(JsonValue::as_u64)
            .unwrap_or(default)
    };
    let (ready, desired) = match kind_id {
        "deployments" | "statefulsets" | "replicasets" => (
            number("/status/readyReplicas", 0),
            number("/spec/replicas", 1),
        ),
        "daemonsets" => (
            number("/status/numberReady", 0),
            number("/status/desiredNumberScheduled", 0),
        ),
        "jobs" => {
            let ready = number("/status/succeeded", 0);
            let desired = number("/spec/completions", 1);
            let failed = array_at(object, "/status/conditions")
                .iter()
                .any(|condition| {
                    str_at(condition, "/type") == "Failed" && str_at(condition, "/status") == "True"
                });
            let tone = if failed {
                Tone::Error
            } else if ready >= desired {
                Tone::Muted
            } else if number("/status/active", 0) > 0 {
                Tone::Ok
            } else {
                Tone::Warn
            };
            return WorkloadHealth {
                ready,
                desired,
                tone,
            };
        }
        _ => {
            return WorkloadHealth {
                ready: 0,
                desired: 0,
                tone: Tone::Muted,
            };
        }
    };
    let tone = if desired == 0 {
        Tone::Muted
    } else if ready >= desired {
        Tone::Ok
    } else if ready == 0 {
        Tone::Error
    } else {
        Tone::Warn
    };
    WorkloadHealth {
        ready,
        desired,
        tone,
    }
}

fn job_status(job: &JsonValue) -> (String, Tone) {
    for condition in array_at(job, "/status/conditions") {
        if str_at(condition, "/status") != "True" {
            continue;
        }
        match str_at(condition, "/type") {
            "Complete" => return ("Complete".to_string(), Tone::Muted),
            "Failed" => return ("Failed".to_string(), Tone::Error),
            "Suspended" => return ("Suspended".to_string(), Tone::Warn),
            _ => {}
        }
    }
    if job
        .pointer("/status/active")
        .and_then(JsonValue::as_u64)
        .unwrap_or_default()
        > 0
    {
        ("Running".to_string(), Tone::Ok)
    } else {
        ("Pending".to_string(), Tone::Warn)
    }
}

pub fn owner_of(object: &JsonValue) -> Option<(&str, &str)> {
    let owners = array_at(object, "/metadata/ownerReferences");
    let owner = owners
        .iter()
        .find(|owner| owner.get("controller").and_then(JsonValue::as_bool) == Some(true))
        .or_else(|| owners.first())?;
    Some((str_at(owner, "/kind"), str_at(owner, "/name")))
}

pub fn selector_string(selector: Option<&JsonValue>) -> String {
    selector
        .and_then(JsonValue::as_object)
        .map(|selector| {
            selector
                .iter()
                .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

/// A LabelSelector (matchLabels and matchExpressions) in kubectl's `-l` syntax.
pub fn label_selector_string(selector: Option<&JsonValue>) -> String {
    let Some(selector) = selector else {
        return String::new();
    };
    let mut parts = vec![selector_string(selector.get("matchLabels"))];
    for expression in array_at(selector, "/matchExpressions") {
        let key = str_at(expression, "/key");
        let values = string_list(expression, "/values");
        parts.push(match str_at(expression, "/operator") {
            "In" => format!("{key} in ({values})"),
            "NotIn" => format!("{key} notin ({values})"),
            "Exists" => key.to_string(),
            "DoesNotExist" => format!("!{key}"),
            _ => continue,
        });
    }
    parts.retain(|part| !part.is_empty());
    parts.join(",")
}

/// The pod selector of a workload or service, if it selects any pods.
pub fn pod_selector(object: &JsonValue, kind_id: &str) -> Option<String> {
    let selector = match kind_id {
        "services" => selector_string(object.pointer("/spec/selector")),
        "cronjobs" => return None,
        _ => label_selector_string(object.pointer("/spec/selector")),
    };
    (!selector.is_empty()).then_some(selector)
}

pub fn images(template: &JsonValue) -> String {
    array_at(template, "/spec/containers")
        .iter()
        .map(|container| str_at(container, "/image"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn event_time(event: &JsonValue) -> Option<DateTime<Utc>> {
    [
        "/lastTimestamp",
        "/eventTime",
        "/series/lastObservedTime",
        "/firstTimestamp",
        "/metadata/creationTimestamp",
    ]
    .into_iter()
    .find_map(|pointer| timestamp_at(event, pointer))
}

fn age_cell(time: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Cell {
    match time {
        Some(time) => Cell {
            tooltip: Some(time.to_rfc3339()),
            ..Cell::number(-(time.timestamp() as f64), format_age(time, now))
        },
        None => Cell::text(""),
    }
}

/// Parses a Kubernetes quantity ("250m", "1.5Gi", "129e6", "1200000n") into base units: cores
/// for CPU, bytes for memory.
pub fn parse_quantity(quantity: Option<&str>) -> f64 {
    let Some(quantity) = quantity
        .map(str::trim)
        .filter(|quantity| !quantity.is_empty())
    else {
        return 0.0;
    };
    let split = quantity
        .char_indices()
        .find(|(index, character)| {
            character.is_ascii_alphabetic()
                && !(matches!(character, 'e' | 'E')
                    && quantity[index + 1..]
                        .chars()
                        .next()
                        .is_some_and(|next| next.is_ascii_digit() || next == '-' || next == '+'))
        })
        .map(|(index, _)| index)
        .unwrap_or(quantity.len());
    let (number, suffix) = quantity.split_at(split);
    let Ok(value) = number.parse::<f64>() else {
        return 0.0;
    };
    let multiplier = match suffix {
        "Ki" => 1024f64,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        "Ei" => 1024f64.powi(6),
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" | "K" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        _ => 1.0,
    };
    value * multiplier
}

/// CPU cores like `kubectl top`: millicores below one core.
pub fn format_cpu(cores: f64) -> String {
    if !cores.is_finite() || cores <= 0.0 {
        return "0".to_string();
    }
    if cores < 1.0 {
        return format!("{}m", ((cores * 1000.0).round() as u64).max(1));
    }
    if cores >= 10.0 {
        return format!("{cores:.1}");
    }
    let formatted = format!("{cores:.2}");
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// Bytes in binary units (Ki, Mi, Gi…).
pub fn format_bytes(bytes: f64) -> String {
    if !bytes.is_finite() || bytes <= 0.0 {
        return "0".to_string();
    }
    const UNITS: [&str; 6] = ["B", "Ki", "Mi", "Gi", "Ti", "Pi"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let digits = if value >= 100.0 || unit == 0 { 0 } else { 1 };
    format!("{value:.digits$}{}", UNITS[unit])
}

/// kubectl-style age: "12s", "5m", "3h12m", "4d6h", "2y40d".
pub fn format_age(time: DateTime<Utc>, now: DateTime<Utc>) -> String {
    format_duration((now - time).num_seconds().max(0))
}

pub fn format_duration(total_seconds: i64) -> String {
    let seconds = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    if total_minutes == 0 {
        return format!("{seconds}s");
    }
    let minutes = total_minutes % 60;
    if total_minutes < 10 {
        return if seconds > 0 {
            format!("{total_minutes}m{seconds}s")
        } else {
            format!("{total_minutes}m")
        };
    }
    if total_minutes < 60 {
        return format!("{total_minutes}m");
    }
    let total_hours = total_minutes / 60;
    let hours = total_hours % 24;
    if total_hours < 8 {
        return if minutes > 0 {
            format!("{total_hours}h{minutes}m")
        } else {
            format!("{total_hours}h")
        };
    }
    if total_hours < 24 {
        return format!("{total_hours}h");
    }
    let total_days = total_hours / 24;
    if total_days < 8 {
        return if hours > 0 {
            format!("{total_days}d{hours}h")
        } else {
            format!("{total_days}d")
        };
    }
    if total_days < 365 {
        return format!("{total_days}d");
    }
    let years = total_days / 365;
    let days = total_days % 365;
    if days > 0 {
        format!("{years}y{days}d")
    } else {
        format!("{years}y")
    }
}

pub(crate) fn str_at<'a>(value: &'a JsonValue, pointer: &str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
}

/// A string or number field as text, for fields like `minAvailable` that may be either.
pub(crate) fn scalar_at(value: &JsonValue, pointer: &str) -> String {
    match value.pointer(pointer) {
        Some(JsonValue::String(text)) => text.clone(),
        Some(JsonValue::Number(number)) => number.to_string(),
        Some(JsonValue::Bool(flag)) => flag.to_string(),
        _ => String::new(),
    }
}

pub(crate) fn array_at<'a>(value: &'a JsonValue, pointer: &str) -> &'a [JsonValue] {
    value
        .pointer(pointer)
        .and_then(JsonValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn array_len(value: &JsonValue, pointer: &str) -> usize {
    array_at(value, pointer).len()
}

fn string_list(value: &JsonValue, pointer: &str) -> String {
    array_at(value, pointer)
        .iter()
        .filter_map(JsonValue::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn timestamp_at(value: &JsonValue, pointer: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.pointer(pointer)?.as_str()?)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_formats_quantities() {
        assert_eq!(parse_quantity(Some("250m")), 0.25);
        assert_eq!(parse_quantity(Some("1.5Gi")), 1.5 * 1024f64.powi(3));
        assert_eq!(parse_quantity(Some("129e6")), 129e6);
        assert!((parse_quantity(Some("1200000n")) - 0.0012).abs() < 1e-12);
        assert_eq!(parse_quantity(Some("2")), 2.0);
        assert_eq!(parse_quantity(Some("garbage")), 0.0);
        assert_eq!(parse_quantity(None), 0.0);
        assert_eq!(format_cpu(0.25), "250m");
        assert_eq!(format_cpu(1.5), "1.5");
        assert_eq!(format_cpu(2.0), "2");
        assert_eq!(format_cpu(12.34), "12.3");
        assert_eq!(format_bytes(512.0), "512B");
        assert_eq!(format_bytes(1536.0 * 1024.0), "1.5Mi");
        assert_eq!(format_bytes(300.0 * 1024f64.powi(3)), "300Gi");
    }

    #[test]
    fn formats_ages_like_kubectl() {
        assert_eq!(format_duration(12), "12s");
        assert_eq!(format_duration(5 * 60 + 3), "5m3s");
        assert_eq!(format_duration(42 * 60), "42m");
        assert_eq!(format_duration(3 * 3600 + 12 * 60), "3h12m");
        assert_eq!(format_duration(20 * 3600), "20h");
        assert_eq!(format_duration(4 * 86400 + 6 * 3600), "4d6h");
        assert_eq!(format_duration(40 * 86400), "40d");
        assert_eq!(format_duration(770 * 86400), "2y40d");
    }

    #[test]
    fn derives_pod_status_like_kubectl() {
        let crashing = json!({
            "spec": { "containers": [{ "name": "app" }] },
            "status": {
                "phase": "Running",
                "containerStatuses": [{
                    "name": "app", "ready": false, "restartCount": 4,
                    "state": { "waiting": { "reason": "CrashLoopBackOff" } }
                }]
            }
        });
        assert_eq!(
            pod_status(&crashing),
            ("CrashLoopBackOff".to_string(), Tone::Error)
        );
        assert_eq!(pod_restarts(&crashing), 4);

        let initializing = json!({
            "spec": { "initContainers": [{ "name": "migrate" }], "containers": [{ "name": "app" }] },
            "status": {
                "phase": "Pending",
                "initContainerStatuses": [{ "name": "migrate", "state": { "running": {} } }]
            }
        });
        assert_eq!(
            pod_status(&initializing),
            ("Init:0/1".to_string(), Tone::Warn)
        );

        let running = json!({
            "spec": { "containers": [{ "name": "app" }] },
            "status": {
                "phase": "Running",
                "containerStatuses": [{ "name": "app", "ready": true, "state": { "running": {} } }]
            }
        });
        assert_eq!(pod_status(&running), ("Running".to_string(), Tone::Ok));

        let terminating = json!({
            "metadata": { "deletionTimestamp": "2026-01-01T00:00:00Z" },
            "status": { "phase": "Running" }
        });
        assert_eq!(
            pod_status(&terminating),
            ("Terminating".to_string(), Tone::Muted)
        );
    }

    #[test]
    fn builds_tables_with_links_and_actions() {
        let Some(kind) = crate::resources::builtin_kind("pods") else {
            panic!("pods kind is registered");
        };
        let pod = json!({
            "metadata": {
                "name": "web-1", "namespace": "apps", "creationTimestamp": "2026-11-05T10:00:00Z",
                "labels": { "app": "web" },
                "ownerReferences": [{ "kind": "ReplicaSet", "name": "web-abc", "controller": true }]
            },
            "spec": {
                "nodeName": "node-1",
                "containers": [{ "name": "app", "resources": { "limits": { "cpu": "500m" } } }]
            },
            "status": { "phase": "Running", "containerStatuses": [{ "name": "app", "ready": true, "state": { "running": {} } }] }
        });
        let context = TableContext {
            pod_metrics: Some(HashMap::from([(
                "apps/web-1".to_string(),
                Usage {
                    cpu: 0.25,
                    memory: 0.0,
                },
            )])),
            ..Default::default()
        };
        let now = DateTime::parse_from_rfc3339("2026-11-05T10:05:00Z")
            .map(|time| time.with_timezone(&Utc))
            .unwrap_or_default();
        let table = build_table(kind, &[pod], &context, now);
        let column = |id: &str| table.columns.iter().position(|column| column.id == id);
        let row = &table.rows[0];
        let cell = |id: &str| column(id).and_then(|index| row.cells.get(index));
        assert_eq!(row.key, "apps/web-1");
        assert_eq!(cell("age").map(|cell| cell.text.as_str()), Some("5m"));
        assert_eq!(
            cell("cpu").map(|cell| cell.text.as_str()),
            Some("250m (50%)")
        );
        assert_eq!(
            cell("status").map(|cell| cell.text.as_str()),
            Some("Running")
        );
        assert_eq!(
            cell("owner")
                .and_then(|cell| cell.link.as_ref())
                .map(|link| link.kind.id.as_str()),
            Some("replicasets")
        );
        assert_eq!(
            cell("node")
                .and_then(|cell| cell.link.as_ref())
                .map(|link| link.name.as_str()),
            Some("node-1")
        );
        assert!(row.actions.contains(&Action::Logs) && row.actions.contains(&Action::Delete));
        assert!(row.search.contains("app=web"));
    }
}
