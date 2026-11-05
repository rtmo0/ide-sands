use std::sync::OnceLock;

use serde_json::Value as JsonValue;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Cluster,
    Workloads,
    Config,
    Network,
    Storage,
    AccessControl,
    Helm,
    Custom,
}

impl Category {
    pub const ALL: [Self; 8] = [
        Self::Cluster,
        Self::Workloads,
        Self::Config,
        Self::Network,
        Self::Storage,
        Self::AccessControl,
        Self::Helm,
        Self::Custom,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Cluster => "Cluster",
            Self::Workloads => "Workloads",
            Self::Config => "Config",
            Self::Network => "Network",
            Self::Storage => "Storage",
            Self::AccessControl => "Access Control",
            Self::Helm => "Helm",
            Self::Custom => "Custom Resources",
        }
    }
}

/// A resource type shown in the explorer: a built-in kind, Helm releases, or a custom resource
/// registered from its CustomResourceDefinition.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceKind {
    pub id: String,
    pub kind: String,
    pub group: String,
    pub version: String,
    pub plural: String,
    pub namespaced: bool,
    pub title: String,
    pub category: Category,
    pub custom: bool,
}

/// Helm releases are listed with the Helm CLI rather than the Kubernetes API.
pub const HELM_RELEASES: &str = "helmreleases";

fn kind(
    id: &str,
    kind: &str,
    api_version: &str,
    plural: &str,
    namespaced: bool,
    title: &str,
    category: Category,
) -> ResourceKind {
    let (group, version) = match api_version.split_once('/') {
        Some((group, version)) => (group, version),
        None => ("", api_version),
    };
    ResourceKind {
        id: id.to_string(),
        kind: kind.to_string(),
        group: group.to_string(),
        version: version.to_string(),
        plural: plural.to_string(),
        namespaced,
        title: title.to_string(),
        category,
        custom: false,
    }
}

pub fn builtin_kinds() -> &'static [ResourceKind] {
    static KINDS: OnceLock<Vec<ResourceKind>> = OnceLock::new();
    KINDS.get_or_init(|| {
        use Category::*;
        vec![
            kind("nodes", "Node", "v1", "nodes", false, "Nodes", Cluster),
            kind(
                "namespaces",
                "Namespace",
                "v1",
                "namespaces",
                false,
                "Namespaces",
                Cluster,
            ),
            kind("events", "Event", "v1", "events", true, "Events", Cluster),
            kind("pods", "Pod", "v1", "pods", true, "Pods", Workloads),
            kind(
                "deployments",
                "Deployment",
                "apps/v1",
                "deployments",
                true,
                "Deployments",
                Workloads,
            ),
            kind(
                "daemonsets",
                "DaemonSet",
                "apps/v1",
                "daemonsets",
                true,
                "DaemonSets",
                Workloads,
            ),
            kind(
                "statefulsets",
                "StatefulSet",
                "apps/v1",
                "statefulsets",
                true,
                "StatefulSets",
                Workloads,
            ),
            kind(
                "replicasets",
                "ReplicaSet",
                "apps/v1",
                "replicasets",
                true,
                "ReplicaSets",
                Workloads,
            ),
            kind("jobs", "Job", "batch/v1", "jobs", true, "Jobs", Workloads),
            kind(
                "cronjobs", "CronJob", "batch/v1", "cronjobs", true, "CronJobs", Workloads,
            ),
            kind(
                "configmaps",
                "ConfigMap",
                "v1",
                "configmaps",
                true,
                "ConfigMaps",
                Config,
            ),
            kind(
                "secrets", "Secret", "v1", "secrets", true, "Secrets", Config,
            ),
            kind(
                "resourcequotas",
                "ResourceQuota",
                "v1",
                "resourcequotas",
                true,
                "Resource Quotas",
                Config,
            ),
            kind(
                "limitranges",
                "LimitRange",
                "v1",
                "limitranges",
                true,
                "Limit Ranges",
                Config,
            ),
            kind(
                "hpa",
                "HorizontalPodAutoscaler",
                "autoscaling/v2",
                "horizontalpodautoscalers",
                true,
                "HPA",
                Config,
            ),
            kind(
                "pdb",
                "PodDisruptionBudget",
                "policy/v1",
                "poddisruptionbudgets",
                true,
                "Pod Disruption Budgets",
                Config,
            ),
            kind(
                "priorityclasses",
                "PriorityClass",
                "scheduling.k8s.io/v1",
                "priorityclasses",
                false,
                "Priority Classes",
                Config,
            ),
            kind(
                "services", "Service", "v1", "services", true, "Services", Network,
            ),
            kind(
                "endpoints",
                "Endpoints",
                "v1",
                "endpoints",
                true,
                "Endpoints",
                Network,
            ),
            kind(
                "ingresses",
                "Ingress",
                "networking.k8s.io/v1",
                "ingresses",
                true,
                "Ingresses",
                Network,
            ),
            kind(
                "ingressclasses",
                "IngressClass",
                "networking.k8s.io/v1",
                "ingressclasses",
                false,
                "Ingress Classes",
                Network,
            ),
            kind(
                "networkpolicies",
                "NetworkPolicy",
                "networking.k8s.io/v1",
                "networkpolicies",
                true,
                "Network Policies",
                Network,
            ),
            kind(
                "pvc",
                "PersistentVolumeClaim",
                "v1",
                "persistentvolumeclaims",
                true,
                "Persistent Volume Claims",
                Storage,
            ),
            kind(
                "pv",
                "PersistentVolume",
                "v1",
                "persistentvolumes",
                false,
                "Persistent Volumes",
                Storage,
            ),
            kind(
                "storageclasses",
                "StorageClass",
                "storage.k8s.io/v1",
                "storageclasses",
                false,
                "Storage Classes",
                Storage,
            ),
            kind(
                "serviceaccounts",
                "ServiceAccount",
                "v1",
                "serviceaccounts",
                true,
                "Service Accounts",
                AccessControl,
            ),
            kind(
                "roles",
                "Role",
                "rbac.authorization.k8s.io/v1",
                "roles",
                true,
                "Roles",
                AccessControl,
            ),
            kind(
                "clusterroles",
                "ClusterRole",
                "rbac.authorization.k8s.io/v1",
                "clusterroles",
                false,
                "Cluster Roles",
                AccessControl,
            ),
            kind(
                "rolebindings",
                "RoleBinding",
                "rbac.authorization.k8s.io/v1",
                "rolebindings",
                true,
                "Role Bindings",
                AccessControl,
            ),
            kind(
                "clusterrolebindings",
                "ClusterRoleBinding",
                "rbac.authorization.k8s.io/v1",
                "clusterrolebindings",
                false,
                "Cluster Role Bindings",
                AccessControl,
            ),
            kind(
                "crds",
                "CustomResourceDefinition",
                "apiextensions.k8s.io/v1",
                "customresourcedefinitions",
                false,
                "Definitions",
                Custom,
            ),
            kind(
                HELM_RELEASES,
                "HelmRelease",
                "v1",
                "secrets",
                true,
                "Releases",
                Helm,
            ),
        ]
    })
}

pub fn builtin_kind(id: &str) -> Option<&'static ResourceKind> {
    builtin_kinds().iter().find(|kind| kind.id == id)
}

impl ResourceKind {
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }

    /// The fully qualified resource type for kubectl, so kinds with the same plural in
    /// different groups (Knative and core `services`) cannot be confused.
    pub fn kubectl_type(&self) -> String {
        if self.group.is_empty() {
            self.plural.clone()
        } else {
            format!("{}.{}.{}", self.plural, self.version, self.group)
        }
    }

    pub fn is_helm_release(&self) -> bool {
        self.id == HELM_RELEASES
    }

    /// The kind served by a CustomResourceDefinition's storage version, if it serves any.
    pub fn from_definition(definition: &JsonValue) -> Option<Self> {
        let spec = definition.get("spec")?;
        let group = spec.get("group")?.as_str()?;
        let names = spec.get("names")?;
        let kind = names.get("kind")?.as_str()?;
        let plural = names.get("plural")?.as_str()?;
        let versions = spec.get("versions")?.as_array()?;
        let served = |version: &&JsonValue| {
            version.get("served").and_then(JsonValue::as_bool) != Some(false)
        };
        let version = versions
            .iter()
            .filter(served)
            .find(|version| version.get("storage").and_then(JsonValue::as_bool) == Some(true))
            .or_else(|| versions.iter().find(served))?
            .get("name")?
            .as_str()?;
        Some(Self {
            id: format!("cr:{group}/{plural}"),
            kind: kind.to_string(),
            group: group.to_string(),
            version: version.to_string(),
            plural: plural.to_string(),
            namespaced: spec.get("scope").and_then(JsonValue::as_str) == Some("Namespaced"),
            title: kind.to_string(),
            category: Category::Custom,
            custom: true,
        })
    }
}

/// Finds the explorer kind of an object reference such as an owner or an event's subject.
pub fn kind_for(
    api_version: Option<&str>,
    kind: &str,
    custom: &[ResourceKind],
) -> Option<ResourceKind> {
    let candidates = builtin_kinds()
        .iter()
        .chain(custom)
        .filter(|candidate| !candidate.is_helm_release() && candidate.kind == kind);
    let mut first = None;
    for candidate in candidates {
        if api_version.is_none_or(|api_version| candidate.api_version() == api_version) {
            return Some(candidate.clone());
        }
        first.get_or_insert(candidate);
    }
    first.cloned()
}

/// A reference to one object, enough to fetch or change it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjectRef {
    pub kind: ResourceKind,
    pub name: String,
    pub namespace: Option<String>,
}

impl ObjectRef {
    pub fn new(kind: ResourceKind, name: impl Into<String>, namespace: Option<String>) -> Self {
        let namespace = if kind.namespaced { namespace } else { None };
        Self {
            kind,
            name: name.into(),
            namespace,
        }
    }

    pub fn of(kind: &ResourceKind, object: &JsonValue) -> Option<Self> {
        let metadata = object.get("metadata")?;
        Some(Self::new(
            kind.clone(),
            metadata.get("name")?.as_str()?,
            metadata
                .get("namespace")
                .and_then(JsonValue::as_str)
                .map(str::to_string),
        ))
    }

    /// `kind/name` arguments with the namespace flag, for kubectl.
    pub fn kubectl_arguments(&self) -> Vec<String> {
        let mut arguments = vec![format!("{}/{}", self.kind.kubectl_type(), self.name)];
        if let Some(namespace) = &self.namespace {
            arguments.extend(["--namespace".to_string(), namespace.clone()]);
        }
        arguments
    }

    pub fn display(&self) -> String {
        match &self.namespace {
            Some(namespace) => format!("{}/{}/{}", self.kind.kind, namespace, self.name),
            None => format!("{}/{}", self.kind.kind, self.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_kubectl_types_and_finds_kinds() {
        let deployments = builtin_kind("deployments").map(ResourceKind::kubectl_type);
        assert_eq!(deployments.as_deref(), Some("deployments.v1.apps"));
        assert_eq!(
            builtin_kind("pods")
                .map(ResourceKind::kubectl_type)
                .as_deref(),
            Some("pods")
        );
        assert_eq!(
            kind_for(Some("apps/v1"), "ReplicaSet", &[]).map(|kind| kind.id),
            Some("replicasets".to_string())
        );
        assert_eq!(kind_for(None, "HelmRelease", &[]), None);
    }

    #[test]
    fn registers_custom_kinds_from_definitions() {
        let definition = serde_json::json!({
            "spec": {
                "group": "cert-manager.io",
                "scope": "Namespaced",
                "names": { "kind": "Certificate", "plural": "certificates" },
                "versions": [
                    { "name": "v1alpha1", "served": false, "storage": false },
                    { "name": "v1", "served": true, "storage": true }
                ]
            }
        });
        let kind = ResourceKind::from_definition(&definition);
        assert_eq!(
            kind.as_ref().map(ResourceKind::kubectl_type).as_deref(),
            Some("certificates.v1.cert-manager.io")
        );
        assert_eq!(kind.map(|kind| kind.namespaced), Some(true));
    }
}
