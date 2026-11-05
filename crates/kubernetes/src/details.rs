use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;

use crate::{
    cluster::{Cluster, Revision, Usage},
    resources::{ObjectRef, ResourceKind, builtin_kind, kind_for},
    table::{
        Action, Resource, Table, TableContext, Tone, array_at, build_table, container_state,
        container_status, event_time, external_addresses, format_bytes, format_cpu, images,
        label_selector_string, node_address, node_roles, node_status, owner_of, parse_quantity,
        pod_resource, pod_restarts, pod_selector, pod_status, row_actions, scalar_at,
        selector_string, str_at, timestamp_at, workload_health,
    },
};

#[derive(Clone, Debug, PartialEq)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
    pub tone: Option<Tone>,
    pub link: Option<ObjectRef>,
}

impl KeyValue {
    fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            key: key.into(),
            value: if value.is_empty() {
                "—".to_string()
            } else {
                value
            },
            tone: None,
            link: None,
        }
    }

    fn time(key: impl Into<String>, time: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Self {
        Self::new(
            key,
            time.map(|time| {
                format!(
                    "{} ({} ago)",
                    time.format("%Y-%m-%d %H:%M:%S UTC"),
                    crate::table::format_age(time, now)
                )
            })
            .unwrap_or_else(|| "never".to_string()),
        )
    }

    fn linked(mut self, link: Option<ObjectRef>) -> Self {
        self.link = link;
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Condition {
    pub condition_type: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub time: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContainerInfo {
    pub name: String,
    pub init: bool,
    pub image: String,
    pub state: String,
    pub tone: Tone,
    pub ready: bool,
    pub restarts: u64,
    pub ports: Vec<(String, u16, String)>,
    pub requests: String,
    pub limits: String,
    pub usage: Option<String>,
    pub last_state: Option<String>,
    pub command: Option<String>,
    pub mounts: Vec<String>,
    pub environment: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DataEntry {
    pub key: String,
    pub value: String,
    pub binary: bool,
    pub size: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServicePort {
    pub name: String,
    pub port: u16,
    pub target_port: String,
    pub protocol: String,
    pub node_port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventRow {
    pub event_type: String,
    pub reason: String,
    pub message: String,
    pub count: u64,
    pub time: Option<DateTime<Utc>>,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageRow {
    pub label: String,
    pub used: Option<f64>,
    pub requested: f64,
    pub limit: f64,
    pub capacity: f64,
    pub resource: Option<Resource>,
}

impl UsageRow {
    pub fn format(&self, value: f64) -> String {
        match self.resource {
            Some(resource) => resource.format(value),
            None => format!("{value}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Section {
    KeyValues {
        title: String,
        items: Vec<KeyValue>,
    },
    Labels {
        title: String,
        items: Vec<(String, String)>,
    },
    Conditions {
        title: String,
        items: Vec<Condition>,
    },
    Containers {
        title: String,
        items: Vec<ContainerInfo>,
        pod: ObjectRef,
    },
    Table {
        title: String,
        table: Table,
    },
    Data {
        title: String,
        items: Vec<DataEntry>,
        secret: bool,
    },
    Ports {
        title: String,
        items: Vec<ServicePort>,
        service: ObjectRef,
    },
    Events {
        title: String,
        items: Vec<EventRow>,
    },
    Revisions {
        title: String,
        items: Vec<Revision>,
        helm: bool,
    },
    Usage {
        title: String,
        items: Vec<UsageRow>,
    },
    Text {
        title: String,
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Details {
    pub reference: ObjectRef,
    pub title: String,
    pub subtitle: String,
    pub status: Option<(String, Tone)>,
    pub actions: Vec<Action>,
    pub sections: Vec<Section>,
}

fn link(kind_id: &str, name: &str, namespace: Option<&str>) -> Option<ObjectRef> {
    builtin_kind(kind_id)
        .filter(|_| !name.is_empty())
        .map(|kind| ObjectRef::new(kind.clone(), name, namespace.map(str::to_string)))
}

pub async fn build_details(
    cluster: &Cluster,
    reference: &ObjectRef,
    custom_kinds: &[ResourceKind],
) -> Result<Details> {
    let now = Utc::now();
    let object = cluster.get(reference).await?;
    let kind = &reference.kind;
    let namespace = reference.namespace.as_deref();
    let mut sections = vec![Section::KeyValues {
        title: "Overview".to_string(),
        items: overview(&object, custom_kinds, now),
    }];
    let mut status = None;

    match kind.id.as_str() {
        "pods" => {
            status = Some(pod_status(&object));
            let usage = cluster.pod_metrics(namespace).await.and_then(|metrics| {
                metrics
                    .get(&format!(
                        "{}/{}",
                        namespace.unwrap_or_default(),
                        reference.name
                    ))
                    .cloned()
            });
            sections.push(Section::KeyValues {
                title: "Pod".to_string(),
                items: pod_info(&object),
            });
            sections.push(Section::Usage {
                title: "Resources".to_string(),
                items: pod_usage_rows(&object, usage.as_ref()),
            });
            sections.push(Section::Containers {
                title: "Containers".to_string(),
                items: containers(&object, usage.as_ref()),
                pod: reference.clone(),
            });
            sections.push(conditions(&object));
            sections.push(Section::KeyValues {
                title: "Volumes".to_string(),
                items: volumes(&object, namespace),
            });
        }
        "deployments" | "statefulsets" | "daemonsets" | "replicasets" | "jobs" => {
            let health = workload_health(&object, &kind.id);
            status = Some((
                format!("{}/{} ready", health.ready, health.desired),
                health.tone,
            ));
            sections.push(Section::KeyValues {
                title: "Spec".to_string(),
                items: workload_info(&kind.id, &object),
            });
            if let Some(pods) =
                owned_pods(cluster, &object, &kind.id, namespace, custom_kinds, now).await?
            {
                sections.push(pods);
            }
            if matches!(
                kind.id.as_str(),
                "deployments" | "statefulsets" | "daemonsets"
            ) {
                sections.push(Section::Revisions {
                    title: "Rollout History".to_string(),
                    items: cluster.rollout_history(reference).await.unwrap_or_default(),
                    helm: false,
                });
            }
            sections.push(conditions(&object));
        }
        "cronjobs" => {
            let suspended =
                object.pointer("/spec/suspend").and_then(JsonValue::as_bool) == Some(true);
            status = Some(if suspended {
                ("Suspended".to_string(), Tone::Warn)
            } else {
                ("Active".to_string(), Tone::Ok)
            });
            sections.push(Section::KeyValues {
                title: "Schedule".to_string(),
                items: vec![
                    KeyValue::new("Schedule", str_at(&object, "/spec/schedule")),
                    KeyValue::new("Time Zone", {
                        let zone = str_at(&object, "/spec/timeZone");
                        if zone.is_empty() {
                            "controller default"
                        } else {
                            zone
                        }
                    }),
                    KeyValue::new("Concurrency Policy", {
                        let policy = str_at(&object, "/spec/concurrencyPolicy");
                        if policy.is_empty() { "Allow" } else { policy }
                    }),
                    KeyValue::time(
                        "Last Schedule",
                        timestamp_at(&object, "/status/lastScheduleTime"),
                        now,
                    ),
                    KeyValue::time(
                        "Last Success",
                        timestamp_at(&object, "/status/lastSuccessfulTime"),
                        now,
                    ),
                    KeyValue::new(
                        "Images",
                        images(
                            object
                                .pointer("/spec/jobTemplate/spec/template")
                                .unwrap_or(&JsonValue::Null),
                        ),
                    ),
                ],
            });
            if let Some(jobs_kind) = builtin_kind("jobs") {
                let uid = str_at(&object, "/metadata/uid");
                let jobs = cluster.list(jobs_kind, namespace).await?;
                let owned = jobs
                    .items
                    .into_iter()
                    .filter(|job| {
                        array_at(job, "/metadata/ownerReferences")
                            .iter()
                            .any(|owner| str_at(owner, "/uid") == uid)
                    })
                    .collect::<Vec<_>>();
                sections.push(Section::Table {
                    title: "Jobs".to_string(),
                    table: build_table(jobs_kind, &owned, &context_with(custom_kinds), now),
                });
            }
        }
        "services" => {
            sections.push(Section::KeyValues {
                title: "Connection".to_string(),
                items: vec![
                    KeyValue::new("Type", {
                        let service_type = str_at(&object, "/spec/type");
                        if service_type.is_empty() {
                            "ClusterIP"
                        } else {
                            service_type
                        }
                    }),
                    KeyValue::new("Cluster IP", {
                        let addresses = array_at(&object, "/spec/clusterIPs")
                            .iter()
                            .filter_map(JsonValue::as_str)
                            .collect::<Vec<_>>()
                            .join(", ");
                        if addresses.is_empty() {
                            str_at(&object, "/spec/clusterIP").to_string()
                        } else {
                            addresses
                        }
                    }),
                    KeyValue::new("External", external_addresses(&object)),
                    KeyValue::new(
                        "Selector",
                        selector_string(object.pointer("/spec/selector")),
                    ),
                    KeyValue::new("Session Affinity", {
                        let affinity = str_at(&object, "/spec/sessionAffinity");
                        if affinity.is_empty() {
                            "None"
                        } else {
                            affinity
                        }
                    }),
                    KeyValue::new(
                        "DNS",
                        format!("{}.{}.svc", reference.name, namespace.unwrap_or("default")),
                    ),
                ],
            });
            sections.push(Section::Ports {
                title: "Ports".to_string(),
                items: array_at(&object, "/spec/ports")
                    .iter()
                    .map(|port| ServicePort {
                        name: str_at(port, "/name").to_string(),
                        port: port
                            .get("port")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or_default() as u16,
                        target_port: scalar_at(port, "/targetPort"),
                        protocol: {
                            let protocol = str_at(port, "/protocol");
                            if protocol.is_empty() { "TCP" } else { protocol }.to_string()
                        },
                        node_port: port
                            .get("nodePort")
                            .and_then(JsonValue::as_u64)
                            .map(|port| port as u16),
                    })
                    .collect(),
                service: reference.clone(),
            });
            if let Some(pods) =
                owned_pods(cluster, &object, &kind.id, namespace, custom_kinds, now).await?
            {
                sections.push(pods);
            }
        }
        "ingresses" => {
            let mut rules = Vec::new();
            for rule in array_at(&object, "/spec/rules") {
                let host = str_at(rule, "/host");
                for path in array_at(rule, "/http/paths") {
                    let service = str_at(path, "/backend/service/name");
                    let port = {
                        let number = scalar_at(path, "/backend/service/port/number");
                        if number.is_empty() {
                            str_at(path, "/backend/service/port/name").to_string()
                        } else {
                            number
                        }
                    };
                    rules.push(
                        KeyValue::new(
                            format!(
                                "{}{}",
                                if host.is_empty() { "*" } else { host },
                                str_at(path, "/path")
                            ),
                            format!("{service}:{port}"),
                        )
                        .linked(link("services", service, namespace)),
                    );
                }
            }
            let tls = array_at(&object, "/spec/tls")
                .iter()
                .map(|tls| {
                    KeyValue::new(
                        array_at(tls, "/hosts")
                            .iter()
                            .filter_map(JsonValue::as_str)
                            .collect::<Vec<_>>()
                            .join(", "),
                        str_at(tls, "/secretName"),
                    )
                    .linked(link(
                        "secrets",
                        str_at(tls, "/secretName"),
                        namespace,
                    ))
                })
                .collect::<Vec<_>>();
            sections.push(Section::KeyValues {
                title: "Rules".to_string(),
                items: rules,
            });
            if !tls.is_empty() {
                sections.push(Section::KeyValues {
                    title: "TLS".to_string(),
                    items: tls,
                });
            }
        }
        "nodes" => {
            status = Some(node_status(&object));
            sections.extend(node_sections(cluster, &object, custom_kinds, now).await?);
            sections.push(conditions(&object));
        }
        "secrets" | "configmaps" => {
            let secret = kind.id == "secrets";
            let mut items = Vec::new();
            for field in ["data", "binaryData"] {
                let Some(data) = object.get(field).and_then(JsonValue::as_object) else {
                    continue;
                };
                for (key, value) in data {
                    let raw = value.as_str().unwrap_or_default();
                    let decoded = if secret || field == "binaryData" {
                        STANDARD.decode(raw).unwrap_or_default()
                    } else {
                        raw.as_bytes().to_vec()
                    };
                    let size = decoded.len();
                    let (value, binary) = match String::from_utf8(decoded) {
                        Ok(text) => (text, false),
                        Err(_) => (format!("{size} bytes of binary data"), true),
                    };
                    items.push(DataEntry {
                        key: key.clone(),
                        value,
                        binary,
                        size,
                    });
                }
            }
            items.sort_by(|a, b| a.key.cmp(&b.key));
            sections.push(Section::Data {
                title: "Data".to_string(),
                items,
                secret,
            });
        }
        "pvc" => {
            sections.push(Section::KeyValues {
                title: "Claim".to_string(),
                items: vec![
                    KeyValue::new("Status", str_at(&object, "/status/phase")),
                    KeyValue::new("Volume", str_at(&object, "/spec/volumeName")).linked(link(
                        "pv",
                        str_at(&object, "/spec/volumeName"),
                        None,
                    )),
                    KeyValue::new(
                        "Requested",
                        str_at(&object, "/spec/resources/requests/storage"),
                    ),
                    KeyValue::new("Capacity", str_at(&object, "/status/capacity/storage")),
                    KeyValue::new("Storage Class", str_at(&object, "/spec/storageClassName"))
                        .linked(link(
                            "storageclasses",
                            str_at(&object, "/spec/storageClassName"),
                            None,
                        )),
                    KeyValue::new("Access Modes", list(&object, "/spec/accessModes")),
                ],
            });
        }
        "namespaces" => {
            if let Some(quotas) = builtin_kind("resourcequotas") {
                let listing = cluster.list(quotas, Some(&reference.name)).await?;
                if !listing.items.is_empty() {
                    sections.push(Section::Table {
                        title: "Resource Quotas".to_string(),
                        table: build_table(
                            quotas,
                            &listing.items,
                            &context_with(custom_kinds),
                            now,
                        ),
                    });
                }
            }
        }
        "roles" | "clusterroles" => {
            sections.push(Section::KeyValues {
                title: "Rules".to_string(),
                items: array_at(&object, "/rules")
                    .iter()
                    .map(|rule| {
                        let resources = list(rule, "/resources");
                        let groups = array_at(rule, "/apiGroups")
                            .iter()
                            .filter_map(JsonValue::as_str)
                            .map(|group| if group.is_empty() { "core" } else { group })
                            .collect::<Vec<_>>()
                            .join(", ");
                        KeyValue::new(
                            if resources.is_empty() {
                                list(rule, "/nonResourceURLs")
                            } else {
                                format!("{resources} ({groups})")
                            },
                            list(rule, "/verbs"),
                        )
                    })
                    .collect(),
            });
        }
        "rolebindings" | "clusterrolebindings" => {
            let role_kind = if str_at(&object, "/roleRef/kind") == "ClusterRole" {
                "clusterroles"
            } else {
                "roles"
            };
            let mut items = vec![
                KeyValue::new(
                    "Role",
                    format!(
                        "{}/{}",
                        str_at(&object, "/roleRef/kind"),
                        str_at(&object, "/roleRef/name")
                    ),
                )
                .linked(link(
                    role_kind,
                    str_at(&object, "/roleRef/name"),
                    namespace,
                )),
            ];
            for subject in array_at(&object, "/subjects") {
                let subject_namespace = str_at(subject, "/namespace");
                items.push(
                    KeyValue::new(
                        str_at(subject, "/kind"),
                        if subject_namespace.is_empty() {
                            str_at(subject, "/name").to_string()
                        } else {
                            format!("{subject_namespace}/{}", str_at(subject, "/name"))
                        },
                    )
                    .linked(
                        (str_at(subject, "/kind") == "ServiceAccount")
                            .then(|| {
                                link(
                                    "serviceaccounts",
                                    str_at(subject, "/name"),
                                    Some(subject_namespace),
                                )
                            })
                            .flatten(),
                    ),
                );
            }
            sections.push(Section::KeyValues {
                title: "Binding".to_string(),
                items,
            });
        }
        "events" => {
            let involved = object.get("involvedObject").unwrap_or(&JsonValue::Null);
            let involved_namespace = involved.get("namespace").and_then(JsonValue::as_str);
            sections.push(Section::KeyValues {
                title: "Event".to_string(),
                items: vec![
                    KeyValue::new("Type", str_at(&object, "/type")),
                    KeyValue::new("Reason", str_at(&object, "/reason")),
                    KeyValue::new("Message", str_at(&object, "/message")),
                    KeyValue::new(
                        "Object",
                        format!(
                            "{}/{}",
                            str_at(involved, "/kind"),
                            str_at(involved, "/name")
                        ),
                    )
                    .linked(
                        kind_for(
                            involved.get("apiVersion").and_then(JsonValue::as_str),
                            str_at(involved, "/kind"),
                            custom_kinds,
                        )
                        .map(|kind| {
                            ObjectRef::new(
                                kind,
                                str_at(involved, "/name"),
                                involved_namespace.map(str::to_string),
                            )
                        }),
                    ),
                    KeyValue::new("Source", str_at(&object, "/source/component")),
                    KeyValue::new("Count", scalar_at(&object, "/count")),
                    KeyValue::time("Last Seen", event_time(&object), now),
                ],
            });
        }
        _ => {
            if let Some(spec) = object.get("spec").filter(|spec| !spec.is_null()) {
                sections.push(Section::Text {
                    title: "Spec".to_string(),
                    text: serde_yaml::to_string(spec).unwrap_or_default(),
                });
            }
            if object.pointer("/status/conditions").is_some() {
                sections.push(conditions(&object));
            }
        }
    }

    sections.push(labels("Labels", object.pointer("/metadata/labels")));
    sections.push(labels(
        "Annotations",
        object.pointer("/metadata/annotations"),
    ));
    if kind.id != "events" {
        sections.push(Section::Events {
            title: "Events".to_string(),
            items: cluster
                .events_for(reference)
                .await
                .unwrap_or_default()
                .iter()
                .map(|event| EventRow {
                    event_type: str_at(event, "/type").to_string(),
                    reason: str_at(event, "/reason").to_string(),
                    message: str_at(event, "/message").to_string(),
                    count: event.get("count").and_then(JsonValue::as_u64).unwrap_or(1),
                    time: event_time(event),
                    source: str_at(event, "/source/component").to_string(),
                })
                .collect(),
        });
    }

    Ok(Details {
        reference: reference.clone(),
        title: reference.name.clone(),
        subtitle: match namespace {
            Some(namespace) => format!("{} · {namespace}", kind.kind),
            None => kind.kind.clone(),
        },
        status,
        actions: row_actions(kind, &object),
        sections,
    })
}

pub async fn build_helm_details(
    cluster: &Cluster,
    name: &str,
    namespace: &str,
) -> Result<Vec<Section>> {
    let status = cluster
        .helm(&["status", name, "--namespace", namespace, "-o", "json"])
        .await?;
    let status: JsonValue = serde_json::from_str(&status).unwrap_or_default();
    let values = cluster
        .helm(&["get", "values", name, "--namespace", namespace, "--all"])
        .await
        .unwrap_or_default();
    let manifest = cluster
        .helm(&["get", "manifest", name, "--namespace", namespace])
        .await
        .unwrap_or_default();
    let mut sections = vec![Section::KeyValues {
        title: "Release".to_string(),
        items: vec![
            KeyValue::new(
                "Chart",
                format!(
                    "{}-{}",
                    str_at(&status, "/chart/metadata/name"),
                    str_at(&status, "/chart/metadata/version")
                ),
            ),
            KeyValue::new("App Version", str_at(&status, "/chart/metadata/appVersion")),
            KeyValue::new("Status", str_at(&status, "/info/status")),
            KeyValue::new("Revision", scalar_at(&status, "/version")),
            KeyValue::new("Namespace", namespace).linked(link("namespaces", namespace, None)),
            KeyValue::new("Updated", str_at(&status, "/info/last_deployed")),
            KeyValue::new("Description", str_at(&status, "/info/description")),
        ],
    }];
    let notes = str_at(&status, "/info/notes");
    if !notes.is_empty() {
        sections.push(Section::Text {
            title: "Notes".to_string(),
            text: notes.to_string(),
        });
    }
    sections.push(Section::Revisions {
        title: "History".to_string(),
        items: cluster
            .helm_history(name, namespace)
            .await
            .unwrap_or_default(),
        helm: true,
    });
    sections.push(Section::Text {
        title: "Values".to_string(),
        text: values,
    });
    sections.push(Section::Text {
        title: "Manifest".to_string(),
        text: manifest,
    });
    Ok(sections)
}

fn context_with(custom_kinds: &[ResourceKind]) -> TableContext {
    TableContext {
        custom_kinds: custom_kinds.to_vec(),
        ..Default::default()
    }
}

fn overview(
    object: &JsonValue,
    custom_kinds: &[ResourceKind],
    now: DateTime<Utc>,
) -> Vec<KeyValue> {
    let mut items = vec![KeyValue::time(
        "Created",
        timestamp_at(object, "/metadata/creationTimestamp"),
        now,
    )];
    let namespace = object
        .pointer("/metadata/namespace")
        .and_then(JsonValue::as_str);
    if let Some(namespace) = namespace {
        items.push(KeyValue::new("Namespace", namespace).linked(link(
            "namespaces",
            namespace,
            None,
        )));
    }
    if let Some((owner_kind, owner_name)) = owner_of(object) {
        items.push(
            KeyValue::new("Controlled By", format!("{owner_kind}/{owner_name}")).linked(
                kind_for(None, owner_kind, custom_kinds)
                    .map(|kind| ObjectRef::new(kind, owner_name, namespace.map(str::to_string))),
            ),
        );
    }
    items.push(KeyValue::new("UID", str_at(object, "/metadata/uid")));
    let finalizers = list(object, "/metadata/finalizers");
    if !finalizers.is_empty() {
        items.push(KeyValue::new("Finalizers", finalizers));
    }
    items
}

fn pod_info(pod: &JsonValue) -> Vec<KeyValue> {
    let namespace = pod
        .pointer("/metadata/namespace")
        .and_then(JsonValue::as_str);
    let node = str_at(pod, "/spec/nodeName");
    let service_account = str_at(pod, "/spec/serviceAccountName");
    vec![
        KeyValue::new("Node", node).linked(link("nodes", node, None)),
        KeyValue::new("Pod IP", {
            let addresses = list(pod, "/status/podIPs");
            if addresses.is_empty() {
                str_at(pod, "/status/podIP").to_string()
            } else {
                addresses
            }
        }),
        KeyValue::new("Host IP", str_at(pod, "/status/hostIP")),
        KeyValue::new("QoS Class", str_at(pod, "/status/qosClass")),
        KeyValue::new("Service Account", service_account).linked(link(
            "serviceaccounts",
            service_account,
            namespace,
        )),
        KeyValue::new("Priority Class", str_at(pod, "/spec/priorityClassName")),
        KeyValue::new("Restart Policy", str_at(pod, "/spec/restartPolicy")),
        KeyValue::new("Restarts", pod_restarts(pod).to_string()),
        KeyValue::new(
            "Node Selector",
            selector_string(pod.pointer("/spec/nodeSelector")),
        ),
        KeyValue::new(
            "Tolerations",
            array_at(pod, "/spec/tolerations").len().to_string(),
        ),
    ]
}

fn pod_usage_rows(pod: &JsonValue, usage: Option<&Usage>) -> Vec<UsageRow> {
    [Resource::Cpu, Resource::Memory]
        .into_iter()
        .map(|resource| UsageRow {
            label: match resource {
                Resource::Cpu => "CPU".to_string(),
                Resource::Memory => "Memory".to_string(),
            },
            used: usage.map(|usage| resource.of(usage)),
            requested: pod_resource(pod, "requests", resource),
            limit: pod_resource(pod, "limits", resource),
            capacity: 0.0,
            resource: Some(resource),
        })
        .collect()
}

fn resources_of(container: &JsonValue, field: &str) -> String {
    container
        .pointer(&format!("/resources/{field}"))
        .and_then(JsonValue::as_object)
        .map(|resources| {
            resources
                .iter()
                .map(|(key, value)| format!("{key}: {}", value.as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn containers(pod: &JsonValue, usage: Option<&Usage>) -> Vec<ContainerInfo> {
    let single = array_at(pod, "/spec/containers").len() == 1;
    let mut items = Vec::new();
    for (pointer, init) in [("/spec/initContainers", true), ("/spec/containers", false)] {
        for container in array_at(pod, pointer) {
            let name = str_at(container, "/name");
            let status = container_status(pod, name, init);
            let (state, tone) = container_state(status);
            let last_state = status
                .and_then(|status| status.pointer("/lastState/terminated"))
                .map(|terminated| {
                    format!(
                        "{} (exit code {}) at {}",
                        str_at(terminated, "/reason"),
                        scalar_at(terminated, "/exitCode"),
                        str_at(terminated, "/finishedAt")
                    )
                });
            let command = [list(container, "/command"), list(container, "/args")]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            items.push(ContainerInfo {
                name: name.to_string(),
                init,
                image: str_at(container, "/image").to_string(),
                state,
                tone,
                ready: status
                    .and_then(|status| status.get("ready"))
                    .and_then(JsonValue::as_bool)
                    == Some(true),
                restarts: status
                    .and_then(|status| status.get("restartCount"))
                    .and_then(JsonValue::as_u64)
                    .unwrap_or_default(),
                ports: array_at(container, "/ports")
                    .iter()
                    .map(|port| {
                        (
                            str_at(port, "/name").to_string(),
                            port.get("containerPort")
                                .and_then(JsonValue::as_u64)
                                .unwrap_or_default() as u16,
                            {
                                let protocol = str_at(port, "/protocol");
                                if protocol.is_empty() { "TCP" } else { protocol }.to_string()
                            },
                        )
                    })
                    .collect(),
                requests: resources_of(container, "requests"),
                limits: resources_of(container, "limits"),
                // metrics-server reports per pod here, so usage is exact only for single-container
                // pods.
                usage: usage.filter(|_| single && !init).map(|usage| {
                    format!(
                        "CPU {} · Memory {}",
                        format_cpu(usage.cpu),
                        format_bytes(usage.memory)
                    )
                }),
                last_state,
                command: (!command.is_empty()).then_some(command),
                mounts: array_at(container, "/volumeMounts")
                    .iter()
                    .map(|mount| {
                        format!(
                            "{} ← {}{}",
                            str_at(mount, "/mountPath"),
                            str_at(mount, "/name"),
                            if mount.get("readOnly").and_then(JsonValue::as_bool) == Some(true) {
                                " (ro)"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect(),
                environment: array_at(container, "/env")
                    .iter()
                    .map(|variable| {
                        let value = if let Some(value) =
                            variable.get("value").and_then(JsonValue::as_str)
                        {
                            value.to_string()
                        } else if let Some(reference) = variable.pointer("/valueFrom/secretKeyRef")
                        {
                            format!(
                                "secret {}/{}",
                                str_at(reference, "/name"),
                                str_at(reference, "/key")
                            )
                        } else if let Some(reference) =
                            variable.pointer("/valueFrom/configMapKeyRef")
                        {
                            format!(
                                "configmap {}/{}",
                                str_at(reference, "/name"),
                                str_at(reference, "/key")
                            )
                        } else if let Some(reference) = variable.pointer("/valueFrom/fieldRef") {
                            format!("field {}", str_at(reference, "/fieldPath"))
                        } else {
                            String::new()
                        };
                        (str_at(variable, "/name").to_string(), value)
                    })
                    .collect(),
            });
        }
    }
    items
}

fn volumes(pod: &JsonValue, namespace: Option<&str>) -> Vec<KeyValue> {
    array_at(pod, "/spec/volumes")
        .iter()
        .map(|volume| {
            let name = str_at(volume, "/name");
            let (source, link_to) = if let Some(claim) = volume
                .pointer("/persistentVolumeClaim/claimName")
                .and_then(JsonValue::as_str)
            {
                (format!("PVC {claim}"), link("pvc", claim, namespace))
            } else if let Some(config_map) = volume
                .pointer("/configMap/name")
                .and_then(JsonValue::as_str)
            {
                (
                    format!("ConfigMap {config_map}"),
                    link("configmaps", config_map, namespace),
                )
            } else if let Some(secret) = volume
                .pointer("/secret/secretName")
                .and_then(JsonValue::as_str)
            {
                (
                    format!("Secret {secret}"),
                    link("secrets", secret, namespace),
                )
            } else if volume.get("emptyDir").is_some() {
                ("emptyDir".to_string(), None)
            } else if let Some(path) = volume.pointer("/hostPath/path").and_then(JsonValue::as_str)
            {
                (format!("hostPath {path}"), None)
            } else if volume.get("projected").is_some() {
                ("projected".to_string(), None)
            } else {
                (
                    volume
                        .as_object()
                        .and_then(|volume| volume.keys().find(|key| *key != "name").cloned())
                        .unwrap_or_default(),
                    None,
                )
            };
            KeyValue::new(name, source).linked(link_to)
        })
        .collect()
}

fn workload_info(kind_id: &str, object: &JsonValue) -> Vec<KeyValue> {
    let template = object.pointer("/spec/template").unwrap_or(&JsonValue::Null);
    let mut items = vec![
        KeyValue::new(
            "Selector",
            label_selector_string(object.pointer("/spec/selector")),
        ),
        KeyValue::new("Images", images(template)),
    ];
    match kind_id {
        "deployments" => {
            items.push(KeyValue::new(
                "Strategy",
                str_at(object, "/spec/strategy/type"),
            ));
            items.push(KeyValue::new(
                "Replicas",
                format!(
                    "{} desired, {} updated, {} available, {} unavailable",
                    scalar_at(object, "/spec/replicas"),
                    scalar_or_zero(object, "/status/updatedReplicas"),
                    scalar_or_zero(object, "/status/availableReplicas"),
                    scalar_or_zero(object, "/status/unavailableReplicas"),
                ),
            ));
        }
        "statefulsets" => {
            items.push(KeyValue::new(
                "Service",
                str_at(object, "/spec/serviceName"),
            ));
            items.push(KeyValue::new(
                "Update Strategy",
                str_at(object, "/spec/updateStrategy/type"),
            ));
            items.push(KeyValue::new(
                "Pod Management",
                str_at(object, "/spec/podManagementPolicy"),
            ));
        }
        "daemonsets" => {
            items.push(KeyValue::new(
                "Update Strategy",
                str_at(object, "/spec/updateStrategy/type"),
            ));
            items.push(KeyValue::new(
                "Node Selector",
                selector_string(object.pointer("/spec/template/spec/nodeSelector")),
            ));
        }
        "jobs" => {
            items.push(KeyValue::new(
                "Completions",
                scalar_at(object, "/spec/completions"),
            ));
            items.push(KeyValue::new(
                "Parallelism",
                scalar_at(object, "/spec/parallelism"),
            ));
            items.push(KeyValue::new(
                "Backoff Limit",
                scalar_at(object, "/spec/backoffLimit"),
            ));
            items.push(KeyValue::new(
                "Result",
                format!(
                    "{} succeeded, {} failed, {} active",
                    scalar_or_zero(object, "/status/succeeded"),
                    scalar_or_zero(object, "/status/failed"),
                    scalar_or_zero(object, "/status/active"),
                ),
            ));
        }
        _ => {}
    }
    items
}

fn scalar_or_zero(object: &JsonValue, pointer: &str) -> String {
    let value = scalar_at(object, pointer);
    if value.is_empty() {
        "0".to_string()
    } else {
        value
    }
}

/// The pods a workload or service selects, as a table with metrics.
async fn owned_pods(
    cluster: &Cluster,
    object: &JsonValue,
    kind_id: &str,
    namespace: Option<&str>,
    custom_kinds: &[ResourceKind],
    now: DateTime<Utc>,
) -> Result<Option<Section>> {
    let Some(pods_kind) = builtin_kind("pods") else {
        return Ok(None);
    };
    let selector = match kind_id {
        "jobs" => Some(format!("job-name={}", str_at(object, "/metadata/name"))),
        _ => pod_selector(object, kind_id),
    };
    let Some(selector) = selector else {
        return Ok(None);
    };
    let mut arguments = vec!["get", "pods", "-l", selector.as_str(), "-o", "json"];
    if let Some(namespace) = namespace {
        arguments.extend(["--namespace", namespace]);
    }
    let output = cluster.kubectl(&arguments).await?;
    let pods = crate::cluster::parse_list_output(&output);
    let context = TableContext {
        pod_metrics: cluster.pod_metrics(namespace).await,
        custom_kinds: custom_kinds.to_vec(),
        ..Default::default()
    };
    Ok(Some(Section::Table {
        title: format!("Pods ({})", pods.len()),
        table: build_table(pods_kind, &pods, &context, now),
    }))
}

async fn node_sections(
    cluster: &Cluster,
    node: &JsonValue,
    custom_kinds: &[ResourceKind],
    now: DateTime<Utc>,
) -> Result<Vec<Section>> {
    let name = str_at(node, "/metadata/name");
    let selector = format!("spec.nodeName={name}");
    let commands = vec![
        vec![
            "get".to_string(),
            "pods".to_string(),
            "--all-namespaces".to_string(),
            "--field-selector".to_string(),
            selector,
            "-o".to_string(),
            "json".to_string(),
        ],
        vec![
            "get".to_string(),
            "--raw".to_string(),
            format!("/apis/metrics.k8s.io/v1beta1/nodes/{name}"),
        ],
    ];
    let outputs = cluster.kubectl_batch(&commands).await?;
    let pods = outputs
        .first()
        .map(crate::cluster::parse_list_output)
        .unwrap_or_default();
    let usage = outputs
        .get(1)
        .filter(|output| output.success())
        .and_then(|output| serde_json::from_slice::<JsonValue>(&output.stdout).ok())
        .map(|metrics| Usage {
            cpu: parse_quantity(metrics.pointer("/usage/cpu").and_then(JsonValue::as_str)),
            memory: parse_quantity(metrics.pointer("/usage/memory").and_then(JsonValue::as_str)),
        });
    let active = pods
        .iter()
        .filter(|pod| !matches!(str_at(pod, "/status/phase"), "Succeeded" | "Failed"))
        .collect::<Vec<_>>();
    let sum = |field: &str, resource: Resource| -> f64 {
        active
            .iter()
            .flat_map(|pod| array_at(pod, "/spec/containers"))
            .map(|container| {
                parse_quantity(
                    container
                        .pointer(&format!("/resources/{field}/{}", resource.key()))
                        .and_then(JsonValue::as_str),
                )
            })
            .sum()
    };
    let allocatable = |key: &str| {
        parse_quantity(
            node.pointer(&format!("/status/allocatable/{key}"))
                .and_then(JsonValue::as_str),
        )
    };
    let info = node.pointer("/status/nodeInfo").unwrap_or(&JsonValue::Null);
    let pod_context = TableContext {
        pod_metrics: cluster.pod_metrics(None).await,
        custom_kinds: custom_kinds.to_vec(),
        ..Default::default()
    };
    let Some(pods_kind) = builtin_kind("pods") else {
        return Ok(Vec::new());
    };
    Ok(vec![
        Section::KeyValues {
            title: "Node".to_string(),
            items: vec![
                KeyValue::new("Roles", node_roles(node)),
                KeyValue::new("Internal IP", node_address(node, "InternalIP")),
                KeyValue::new("External IP", node_address(node, "ExternalIP")),
                KeyValue::new("Hostname", node_address(node, "Hostname")),
                KeyValue::new(
                    "OS",
                    format!(
                        "{} ({})",
                        str_at(info, "/osImage"),
                        str_at(info, "/architecture")
                    ),
                ),
                KeyValue::new("Kernel", str_at(info, "/kernelVersion")),
                KeyValue::new(
                    "Container Runtime",
                    str_at(info, "/containerRuntimeVersion"),
                ),
                KeyValue::new("Kubelet", str_at(info, "/kubeletVersion")),
                KeyValue::new("Pod CIDR", str_at(node, "/spec/podCIDR")),
                KeyValue::new(
                    "Taints",
                    array_at(node, "/spec/taints")
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
                        .join(", "),
                ),
            ],
        },
        Section::Usage {
            title: "Capacity".to_string(),
            items: vec![
                UsageRow {
                    label: "CPU".to_string(),
                    used: usage.as_ref().map(|usage| usage.cpu),
                    requested: sum("requests", Resource::Cpu),
                    limit: sum("limits", Resource::Cpu),
                    capacity: allocatable("cpu"),
                    resource: Some(Resource::Cpu),
                },
                UsageRow {
                    label: "Memory".to_string(),
                    used: usage.as_ref().map(|usage| usage.memory),
                    requested: sum("requests", Resource::Memory),
                    limit: sum("limits", Resource::Memory),
                    capacity: allocatable("memory"),
                    resource: Some(Resource::Memory),
                },
                UsageRow {
                    label: "Pods".to_string(),
                    used: Some(active.len() as f64),
                    requested: 0.0,
                    limit: 0.0,
                    capacity: allocatable("pods"),
                    resource: None,
                },
            ],
        },
        Section::Table {
            title: format!("Pods ({})", pods.len()),
            table: build_table(pods_kind, &pods, &pod_context, now),
        },
    ])
}

fn conditions(object: &JsonValue) -> Section {
    Section::Conditions {
        title: "Conditions".to_string(),
        items: array_at(object, "/status/conditions")
            .iter()
            .map(|condition| Condition {
                condition_type: str_at(condition, "/type").to_string(),
                status: str_at(condition, "/status").to_string(),
                reason: str_at(condition, "/reason").to_string(),
                message: str_at(condition, "/message").to_string(),
                time: timestamp_at(condition, "/lastTransitionTime"),
            })
            .collect(),
    }
}

fn labels(title: &str, values: Option<&JsonValue>) -> Section {
    Section::Labels {
        title: title.to_string(),
        items: values
            .and_then(JsonValue::as_object)
            .map(|values| {
                values
                    .iter()
                    .map(|(key, value)| {
                        (key.clone(), value.as_str().unwrap_or_default().to_string())
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn list(value: &JsonValue, pointer: &str) -> String {
    array_at(value, pointer)
        .iter()
        .map(|item| match item {
            JsonValue::String(text) => text.clone(),
            JsonValue::Object(object) => object
                .values()
                .filter_map(JsonValue::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::build_overview;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Reads the lab cluster in the `sands-k8s-lab` sandbox (kind, with web, broken, tick and the
    /// demo Helm release): `cargo test -p kubernetes -- --ignored`.
    #[test]
    #[ignore]
    fn reads_a_real_cluster() -> TestResult {
        smol::block_on(async {
            let cluster = Cluster::new(sbx::Sbx::locate()?, "sands-k8s-lab");
            let context = cluster.context().await?.ok_or("no context")?;
            assert_eq!(context.context, "kind-lab");

            let overview = build_overview(&cluster, context, None).await?;
            assert_eq!(
                (overview.nodes_ready, overview.nodes_total),
                (1, 1),
                "{:?}",
                overview.notices
            );
            assert!(overview.cpu.allocatable > 0.0);
            let deployments = overview
                .workloads
                .iter()
                .find(|summary| summary.kind.id == "deployments")
                .ok_or("no deployments summary")?;
            assert!(deployments.error >= 1, "broken is failing");

            let pods = crate::resources::builtin_kind("pods").ok_or("no pods kind")?;
            let listing = cluster.list(pods, Some("default")).await?;
            let table = crate::table::build_table(
                pods,
                &listing.items,
                &TableContext::default(),
                Utc::now(),
            );
            assert!(
                table
                    .rows
                    .iter()
                    .any(|row| row.reference.name.starts_with("web-"))
            );

            let deployment = ObjectRef::new(
                crate::resources::builtin_kind("deployments")
                    .ok_or("no kind")?
                    .clone(),
                "web",
                Some("default".to_string()),
            );
            let details = build_details(&cluster, &deployment, &[]).await?;
            assert!(details.sections.iter().any(|section| matches!(
                section,
                Section::Table { title, table } if title.starts_with("Pods") && table.rows.len() == 2
            )));
            assert!(details.sections.iter().any(|section| matches!(
                section,
                Section::Revisions { items, .. } if items.iter().any(|revision| revision.current)
            )));

            let secret = ObjectRef::new(
                crate::resources::builtin_kind("secrets")
                    .ok_or("no kind")?
                    .clone(),
                "app-secret",
                Some("default".to_string()),
            );
            let details = build_details(&cluster, &secret, &[]).await?;
            assert!(details.sections.iter().any(|section| matches!(
                section,
                Section::Data { items, .. } if items.iter().any(|entry| entry.value == "hunter2")
            )));

            let releases = cluster.helm_releases(None).await?;
            assert!(releases.iter().any(|release| release.name == "demo"));
            let sections = build_helm_details(&cluster, "demo", "default").await?;
            assert!(sections.len() >= 3);

            let path = cluster.export_for_editing(&deployment).await?;
            let diff = cluster.diff(&path).await?;
            assert!(
                diff.is_empty(),
                "an untouched export matches the live object: {diff}"
            );
            Ok(())
        })
    }
}
