use std::{future::Future, time::Duration};

use chrono::{DateTime, Utc};
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable,
    PathPromptOptions, Pixels, PromptLevel, SharedString, Task, WeakEntity, actions, px,
};
use kubernetes::{
    HelmRelease, KubernetesOverview, PreparedConnection, ResourceKind, ResourceSummary,
};
use sbx::{
    ConfiguredResources, NetworkActivity, NetworkDecision, NetworkLog, PolicyRules, PublishedPort,
    SandboxInfo, SandboxProbe, SandboxStatus, Sbx, SbxError,
};
use task::{SpawnInTerminal, TaskId};
use ui::{
    Chip, Divider, DividerColor, Indicator, ListItem, ProgressBar, ToggleButtonGroup,
    ToggleButtonGroupStyle, ToggleButtonSimple, Toggleable, Tooltip, prelude::*,
};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelSizeState},
};

use crate::connected_sandbox;

actions!(
    sands,
    [
        /// Toggles focus on the panel that shows the current project's sandbox.
        ToggleSandboxPanel
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleSandboxPanel, window, cx| {
            workspace.toggle_panel_focus::<SandboxPanel>(window, cx);
        });
    })
    .detach();
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
const NETWORK_LOG_LIMIT: usize = 50;
const RECENT_HOSTS: usize = NETWORK_LOG_LIMIT;

/// Hosts agents use to reach model providers, as seen by the sandbox proxy.
const MODEL_API_HOSTS: [&str; 9] = [
    "api.anthropic.com",
    "api.openai.com",
    "chatgpt.com",
    "generativelanguage.googleapis.com",
    "openrouter.ai",
    "api.mistral.ai",
    "api.x.ai",
    "api.deepseek.com",
    "api.githubcopilot.com",
];

struct Snapshot {
    info: SandboxInfo,
    configured: Option<ConfiguredResources>,
    probe: Option<SandboxProbe>,
    ports: Vec<PublishedPort>,
    network: NetworkLog,
    rules: PolicyRules,
    kubernetes: KubernetesState,
}

enum KubernetesState {
    NotConnected,
    Connected(KubernetesOverview),
    Failed(SharedString),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KubernetesPage {
    Overview,
    Resources,
    Helm,
    Doctor,
}

enum KubernetesResourcesState {
    Idle,
    Loading,
    Loaded(Vec<ResourceSummary>),
    Failed(SharedString),
}

enum KubernetesDetailsState {
    None,
    Loading(ResourceSummary),
    Loaded(ResourceSummary, String),
    Failed(ResourceSummary, SharedString),
}

enum HelmState {
    Idle,
    Loading,
    NotInstalled,
    Loaded(Vec<HelmRelease>),
    Failed(SharedString),
}

enum PanelState {
    NoSandbox,
    Loading,
    Loaded(Box<Snapshot>),
    Failed(SharedString),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelPage {
    Overview,
    Kubernetes,
    Traffic,
    Settings,
}

/// Shows the sandbox behind the current project: its resources, repositories, agents, ports
/// and network activity.
pub struct SandboxPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    state: PanelState,
    active: bool,
    page: PanelPage,
    port_input: Entity<InputField>,
    network_resource_input: Entity<InputField>,
    repository_url_input: Entity<InputField>,
    action_error: Option<SharedString>,
    installing_toolchains: bool,
    connecting_kubernetes: bool,
    kubernetes_page: KubernetesPage,
    kubernetes_resource_kind: ResourceKind,
    kubernetes_namespace_input: Entity<InputField>,
    kubernetes_port_forward_input: Entity<InputField>,
    kubernetes_scale_input: Entity<InputField>,
    kubernetes_resources: KubernetesResourcesState,
    kubernetes_details: KubernetesDetailsState,
    helm_releases: HelmState,
    _refresh: Task<()>,
    _action: Task<()>,
    _kubernetes_task: Task<()>,
}

impl SandboxPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let workspace_handle = workspace.weak_handle();
            cx.new(|cx| {
                let port_input = cx.new(|cx| InputField::new(window, cx, "3000 or 8080:3000"));
                let network_resource_input = cx.new(|cx| {
                    InputField::new(window, cx, "api.example.com:443 or *.example.com")
                        .label("Host, wildcard or CIDR")
                });
                let repository_url_input = cx.new(|cx| {
                    InputField::new(window, cx, "https://github.com/owner/repository.git")
                        .label("Git repository URL")
                });
                let kubernetes_namespace_input = cx.new(|cx| {
                    InputField::new(window, cx, "All namespaces")
                        .label("Namespace (empty means all)")
                });
                let kubernetes_port_forward_input =
                    cx.new(|cx| InputField::new(window, cx, "8080:80").label("Local:remote port"));
                let kubernetes_scale_input =
                    cx.new(|cx| InputField::new(window, cx, "3").label("Replicas"));
                let mut panel = Self {
                    workspace: workspace_handle,
                    focus_handle: cx.focus_handle(),
                    position: DockPosition::Right,
                    state: PanelState::Loading,
                    active: false,
                    page: PanelPage::Overview,
                    port_input,
                    network_resource_input,
                    repository_url_input,
                    action_error: None,
                    installing_toolchains: false,
                    connecting_kubernetes: false,
                    kubernetes_page: KubernetesPage::Overview,
                    kubernetes_resource_kind: ResourceKind::Pod,
                    kubernetes_namespace_input,
                    kubernetes_port_forward_input,
                    kubernetes_scale_input,
                    kubernetes_resources: KubernetesResourcesState::Idle,
                    kubernetes_details: KubernetesDetailsState::None,
                    helm_releases: HelmState::Idle,
                    _refresh: Task::ready(()),
                    _action: Task::ready(()),
                    _kubernetes_task: Task::ready(()),
                };
                panel.schedule_refresh(window, cx);
                panel
            })
        })
    }

    fn sandbox_name(&self, cx: &App) -> Option<String> {
        let workspace = self.workspace.upgrade()?;
        connected_sandbox(workspace.read(cx), cx)
    }

    fn snapshot_name(&self) -> Option<String> {
        match &self.state {
            PanelState::Loaded(snapshot) => Some(snapshot.info.name.clone()),
            PanelState::NoSandbox | PanelState::Loading | PanelState::Failed(_) => None,
        }
    }

    /// Reloads now, then every [`REFRESH_INTERVAL`] while the panel is visible.
    fn schedule_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._refresh = cx.spawn_in(window, async move |this, cx| {
            let mut first = true;
            loop {
                let Ok((name, visible)) =
                    this.update(cx, |this, cx| (this.sandbox_name(cx), this.active))
                else {
                    break;
                };
                if first || visible {
                    first = false;
                    let state = match name {
                        None => PanelState::NoSandbox,
                        Some(name) => match cx.background_spawn(load_snapshot(name)).await {
                            Ok(snapshot) => PanelState::Loaded(Box::new(snapshot)),
                            Err(error) => PanelState::Failed(format!("{error:#}").into()),
                        },
                    };
                    let updated = this.update(cx, |this, cx| {
                        this.state = state;
                        cx.notify();
                    });
                    if updated.is_err() {
                        break;
                    }
                }
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
    }

    fn run_sbx_action<F>(
        &mut self,
        action: impl FnOnce(Sbx, String) -> F + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: Future<Output = Result<(), SbxError>> + Send + 'static,
    {
        let Some(name) = self.snapshot_name() else {
            return;
        };
        self.action_error = None;
        cx.notify();
        self._action = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let sbx = Sbx::locate()?;
                    action(sbx, name).await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if let Err(error) = result {
                    this.action_error = Some(error.to_string().into());
                }
                this.installing_toolchains = false;
                this.schedule_refresh(window, cx);
                cx.notify();
            })
            .log_err();
        });
    }

    fn publish_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.port_input.read(cx).text(cx);
        match parse_port_mapping(&input) {
            Some((host_port, sandbox_port)) => {
                self.port_input
                    .update(cx, |field, cx| field.clear(window, cx));
                self.run_sbx_action(
                    move |sbx, name| async move {
                        sbx.publish_port(&name, sandbox_port, host_port)
                            .await
                            .map(|_| ())
                    },
                    window,
                    cx,
                );
            }
            None => {
                self.action_error = Some(
                    format!(
                        "{input:?} is not a port. Use 3000, or 8080:3000 for a fixed host port."
                    )
                    .into(),
                );
                cx.notify();
            }
        }
    }

    fn add_network_rule_from_input(
        &mut self,
        decision: NetworkDecision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resource = self
            .network_resource_input
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        if resource.is_empty() || resource.chars().any(char::is_whitespace) {
            self.action_error =
                Some("Enter one hostname, wildcard, IP address or CIDR without spaces".into());
            cx.notify();
            return;
        }
        self.network_resource_input
            .update(cx, |field, cx| field.clear(window, cx));
        self.run_sbx_action(
            move |sbx, name| async move { sbx.add_network_rule(&name, decision, &resource).await },
            window,
            cx,
        );
    }

    fn clone_repository_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self
            .repository_url_input
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        if url.is_empty() {
            self.action_error = Some("Enter a Git repository URL".into());
            cx.notify();
            return;
        }
        self.repository_url_input
            .update(cx, |field, cx| field.clear(window, cx));
        self.run_sbx_action(
            move |sbx, name| async move { sbx.clone_repository(&name, &url).await.map(|_| ()) },
            window,
            cx,
        );
    }

    fn connect_kubernetes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.snapshot_name() else {
            return;
        };
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import kubeconfig".into()),
        });
        self.connecting_kubernetes = true;
        self.action_error = None;
        cx.notify();
        self._action = cx.spawn_in(window, async move |this, cx| {
            let path = match paths.await {
                Ok(Ok(paths)) => paths.and_then(|paths| paths.into_iter().next()),
                Ok(Err(error)) => {
                    this.update(cx, |this, cx| {
                        this.connecting_kubernetes = false;
                        this.action_error = Some(format!("{error:#}").into());
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
                Err(_) => None,
            };
            let Some(path) = path else {
                this.update(cx, |this, cx| {
                    this.connecting_kubernetes = false;
                    cx.notify();
                })
                .log_err();
                return;
            };
            let result = cx
                .background_spawn(async move {
                    let connection = PreparedConnection::from_file(&path)?;
                    let sbx = Sbx::locate()?;
                    connection.provision(&sbx, &name).await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.connecting_kubernetes = false;
                if let Err(error) = result {
                    this.action_error = Some(format!("{error:#}").into());
                }
                this.schedule_refresh(window, cx);
                cx.notify();
            })
            .log_err();
        });
    }

    fn load_kubernetes_resources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.snapshot_name() else {
            return;
        };
        let kind = self.kubernetes_resource_kind;
        let namespace = self
            .kubernetes_namespace_input
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        self.kubernetes_resources = KubernetesResourcesState::Loading;
        self.kubernetes_details = KubernetesDetailsState::None;
        cx.notify();
        self._kubernetes_task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let sbx = Sbx::locate()?;
                    kubernetes::list_resources(
                        &sbx,
                        &name,
                        kind,
                        (!namespace.is_empty()).then_some(namespace.as_str()),
                    )
                    .await
                })
                .await;
            this.update(cx, |this, cx| {
                this.kubernetes_resources = match result {
                    Ok(resources) => KubernetesResourcesState::Loaded(resources),
                    Err(error) => KubernetesResourcesState::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .log_err();
        });
    }

    fn cycle_kubernetes_resource_kind(
        &mut self,
        offset: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = ResourceKind::ALL
            .iter()
            .position(|kind| *kind == self.kubernetes_resource_kind)
            .unwrap_or_default();
        let count = ResourceKind::ALL.len() as isize;
        let index = (current as isize + offset).rem_euclid(count) as usize;
        if let Some(kind) = ResourceKind::ALL.get(index).copied() {
            self.kubernetes_resource_kind = kind;
            self.load_kubernetes_resources(window, cx);
        }
    }

    fn load_kubernetes_details(
        &mut self,
        resource: ResourceSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.snapshot_name() else {
            return;
        };
        self.kubernetes_details = KubernetesDetailsState::Loading(resource.clone());
        cx.notify();
        self._kubernetes_task = cx.spawn_in(window, async move |this, cx| {
            let resource_for_request = resource.clone();
            let result = cx
                .background_spawn(async move {
                    let sbx = Sbx::locate()?;
                    kubernetes::resource_yaml(&sbx, &name, &resource_for_request).await
                })
                .await;
            this.update(cx, |this, cx| {
                this.kubernetes_details = match result {
                    Ok(yaml) => KubernetesDetailsState::Loaded(resource, yaml),
                    Err(error) => {
                        KubernetesDetailsState::Failed(resource, format!("{error:#}").into())
                    }
                };
                cx.notify();
            })
            .log_err();
        });
    }

    fn load_helm_releases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.snapshot_name() else {
            return;
        };
        self.helm_releases = HelmState::Loading;
        cx.notify();
        self._kubernetes_task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let sbx = Sbx::locate()?;
                    kubernetes::helm_releases(&sbx, &name).await
                })
                .await;
            this.update(cx, |this, cx| {
                this.helm_releases = match result {
                    Ok(Some(releases)) => HelmState::Loaded(releases),
                    Ok(None) => HelmState::NotInstalled,
                    Err(error) => HelmState::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .log_err();
        });
    }

    fn open_kubectl_terminal(
        &mut self,
        label: String,
        arguments: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let command_label = format!("kubectl {}", arguments.join(" "));
            workspace
                .spawn_in_terminal(
                    SpawnInTerminal {
                        id: TaskId(format!("kubernetes-{label}")),
                        full_label: label.clone(),
                        label,
                        command: Some("kubectl".to_string()),
                        args: arguments,
                        command_label,
                        use_new_terminal: true,
                        allow_concurrent_runs: true,
                        ..Default::default()
                    },
                    window,
                    cx,
                )
                .detach();
        });
    }

    fn open_resource_command(
        &mut self,
        operation: &'static str,
        resource: ResourceSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let port_mapping = if operation == "port-forward" {
            let mapping = self
                .kubernetes_port_forward_input
                .read(cx)
                .text(cx)
                .trim()
                .to_string();
            let valid = mapping.split_once(':').is_some_and(|(local, remote)| {
                local.parse::<u16>().is_ok() && remote.parse::<u16>().is_ok()
            });
            if !valid {
                self.action_error = Some("Enter a port mapping such as 8080:80".into());
                cx.notify();
                return;
            }
            mapping
        } else {
            String::new()
        };
        let replicas = if operation == "scale" {
            let replicas = self
                .kubernetes_scale_input
                .read(cx)
                .text(cx)
                .trim()
                .to_string();
            if replicas.parse::<u32>().is_err() {
                self.action_error = Some("Replicas must be a non-negative whole number".into());
                cx.notify();
                return;
            }
            replicas
        } else {
            String::new()
        };
        let mut arguments = match operation {
            "logs" => vec![
                "logs".to_string(),
                format!("{}/{}", resource.kind.resource_name(), resource.name),
                "--follow".to_string(),
                "--all-containers".to_string(),
            ],
            "shell" => vec![
                "exec".to_string(),
                "-it".to_string(),
                resource.name.clone(),
                "--".to_string(),
                "sh".to_string(),
            ],
            "port-forward" => vec![
                "port-forward".to_string(),
                format!("{}/{}", resource.kind.resource_name(), resource.name),
                port_mapping,
            ],
            "edit" => vec![
                "edit".to_string(),
                resource.kind.resource_name().to_string(),
                resource.name.clone(),
            ],
            "scale" => vec![
                "scale".to_string(),
                format!("{}/{}", resource.kind.resource_name(), resource.name),
                format!("--replicas={replicas}"),
            ],
            _ => return,
        };
        if let Some(namespace) = resource.namespace.as_deref() {
            arguments.splice(1..1, ["--namespace".to_string(), namespace.to_string()]);
        }
        self.open_kubectl_terminal(
            format!("Kubernetes {operation}: {}", resource.name),
            arguments,
            window,
            cx,
        );
    }

    fn restart_kubernetes_resource(
        &mut self,
        resource: ResourceSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.snapshot_name() else {
            return;
        };
        self._kubernetes_task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let sbx = Sbx::locate()?;
                    kubernetes::restart_resource(&sbx, &name, &resource).await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if let Err(error) = result {
                    this.action_error = Some(format!("{error:#}").into());
                }
                this.load_kubernetes_resources(window, cx);
            })
            .log_err();
        });
    }

    fn delete_kubernetes_resource(
        &mut self,
        resource: ResourceSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.snapshot_name() else {
            return;
        };
        let prompt = window.prompt(
            PromptLevel::Warning,
            &format!(
                "Delete {} {}?",
                resource.kind.resource_name(),
                resource.name
            ),
            Some("This operation changes the connected Kubernetes cluster."),
            &["Delete", "Cancel"],
            cx,
        );
        self._kubernetes_task = cx.spawn_in(window, async move |this, cx| {
            if prompt.await != Ok(0) {
                return;
            }
            let result = cx
                .background_spawn(async move {
                    let sbx = Sbx::locate()?;
                    kubernetes::delete_resource(&sbx, &name, &resource).await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if let Err(error) = result {
                    this.action_error = Some(format!("{error:#}").into());
                }
                this.load_kubernetes_resources(window, cx);
            })
            .log_err();
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, status) = match &self.state {
            PanelState::Loaded(snapshot) => {
                (snapshot.info.name.clone(), Some(snapshot.info.status))
            }
            _ => ("Sandbox".to_string(), None),
        };
        let (status_label, status_color) = match status {
            Some(SandboxStatus::Running) => ("Running", Color::Success),
            Some(SandboxStatus::Stopped) => ("Stopped", Color::Muted),
            Some(SandboxStatus::Unknown) => ("Unknown", Color::Warning),
            None => ("", Color::Muted),
        };

        h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .child(Icon::new(IconName::Server).color(Color::Muted))
                    .child(Label::new(title).truncate())
                    .when(status.is_some(), |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(Indicator::dot().color(status_color))
                                .child(
                                    Label::new(status_label)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .when(status == Some(SandboxStatus::Running), |this| {
                        this.child(
                            IconButton::new("stop-sandbox", IconName::Stop)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Stop sandbox"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.run_sbx_action(
                                        |sbx, name| async move { sbx.stop(&name).await },
                                        window,
                                        cx,
                                    )
                                })),
                        )
                    })
                    .when(status == Some(SandboxStatus::Stopped), |this| {
                        this.child(
                            IconButton::new("start-sandbox", IconName::PlayFilled)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Start sandbox"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.run_sbx_action(
                                        |sbx, name| async move { sbx.start(&name).await },
                                        window,
                                        cx,
                                    )
                                })),
                        )
                    })
                    .child(
                        IconButton::new("refresh-sandbox", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.schedule_refresh(window, cx)
                                }),
                            ),
                    ),
            )
    }

    fn render_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                ToggleButtonGroup::single_row(
                    "sandbox-page",
                    [
                        ToggleButtonSimple::new(
                            "Overview",
                            cx.listener(|this, _, _, cx| {
                                this.page = PanelPage::Overview;
                                cx.notify();
                            }),
                        ),
                        ToggleButtonSimple::new(
                            "Kubernetes",
                            cx.listener(|this, _, _, cx| {
                                this.page = PanelPage::Kubernetes;
                                cx.notify();
                            }),
                        ),
                        ToggleButtonSimple::new(
                            "Traffic",
                            cx.listener(|this, _, _, cx| {
                                this.page = PanelPage::Traffic;
                                cx.notify();
                            }),
                        ),
                        ToggleButtonSimple::new(
                            "Settings",
                            cx.listener(|this, _, _, cx| {
                                this.page = PanelPage::Settings;
                                cx.notify();
                            }),
                        ),
                    ],
                )
                .style(ToggleButtonGroupStyle::Outlined)
                .selected_index(match self.page {
                    PanelPage::Overview => 0,
                    PanelPage::Kubernetes => 1,
                    PanelPage::Traffic => 2,
                    PanelPage::Settings => 3,
                }),
            )
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = match &self.state {
            PanelState::NoSandbox => {
                return message("Open a project from the Projects panel to see its sandbox.")
                    .into_any_element();
            }
            PanelState::Loading => return message("Loading sandbox…").into_any_element(),
            PanelState::Failed(error) => {
                return v_flex()
                    .p_3()
                    .child(
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .into_any_element();
            }
            PanelState::Loaded(snapshot) => snapshot,
        };
        let content = match self.page {
            PanelPage::Overview => self.render_overview(snapshot, cx),
            PanelPage::Kubernetes => self.render_kubernetes(snapshot, cx),
            PanelPage::Traffic => self.render_traffic(snapshot, cx),
            PanelPage::Settings => self.render_settings(snapshot, cx),
        };
        v_flex()
            .when_some(self.action_error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .mx_3()
                        .mt_3()
                        .p_2()
                        .rounded_md()
                        .bg(cx.theme().status().error_background)
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(content)
            .into_any_element()
    }

    fn render_overview(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        let toolchains = self.render_toolchains(snapshot, cx);
        v_flex()
            .p_3()
            .gap_4()
            .child(section(
                "Resources",
                IconName::Box,
                render_resources(snapshot, cx),
                cx,
            ))
            .child(section("Toolchains", IconName::ToolHammer, toolchains, cx))
            .child(section(
                "Repositories",
                IconName::GitBranch,
                render_repositories(snapshot, cx),
                cx,
            ))
            .child(section(
                "Agents",
                IconName::Sparkle,
                render_agents(snapshot),
                cx,
            ))
            .into_any_element()
    }

    fn render_traffic(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        let allowed = snapshot
            .network
            .allowed
            .iter()
            .map(|activity| activity.count)
            .sum::<u64>();
        let blocked = snapshot
            .network
            .blocked
            .iter()
            .map(|activity| activity.count)
            .sum::<u64>();
        v_flex()
            .p_3()
            .gap_4()
            .child(
                h_flex()
                    .gap_2()
                    .child(metric_card("Allowed", allowed, Color::Success, cx))
                    .child(metric_card("Blocked", blocked, Color::Error, cx)),
            )
            .child(section(
                "Network traffic",
                IconName::Public,
                render_network(snapshot),
                cx,
            ))
            .child(section(
                "Published ports",
                IconName::Link,
                render_published_ports(snapshot),
                cx,
            ))
            .into_any_element()
    }

    fn render_kubernetes(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        let import_label = if self.connecting_kubernetes {
            "Connecting…"
        } else {
            "Import kubeconfig"
        };
        let replace_label = if self.connecting_kubernetes {
            "Connecting…"
        } else {
            "Replace kubeconfig"
        };
        let content = match &snapshot.kubernetes {
            KubernetesState::NotConnected => v_flex()
                .gap_2()
                .child(muted("No Kubernetes cluster is connected to this project."))
                .child(
                    Button::new("connect-kubernetes", import_label)
                        .style(ButtonStyle::Filled)
                        .full_width()
                        .disabled(self.connecting_kubernetes)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.connect_kubernetes(window, cx)
                        })),
                )
                .child(muted(
                    "The active context is copied into this sandbox. Its bearer token remains on the host and is injected only into requests to the cluster API.",
                ))
                .into_any_element(),
            KubernetesState::Failed(error) => v_flex()
                .gap_2()
                .child(
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .child(
                    Button::new("replace-kubernetes", replace_label)
                        .style(ButtonStyle::Outlined)
                        .full_width()
                        .disabled(self.connecting_kubernetes)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.connect_kubernetes(window, cx)
                        })),
                )
                .into_any_element(),
            KubernetesState::Connected(overview) => v_flex()
                .gap_3()
                .child(self.render_kubernetes_navigation(cx))
                .child(match self.kubernetes_page {
                    KubernetesPage::Overview => render_kubernetes_overview(overview, cx)
                        .child(muted(
                            "kubectl is available to this project's terminals and agents.",
                        ))
                        .child(
                            Button::new("replace-kubernetes", replace_label)
                                .style(ButtonStyle::Outlined)
                                .full_width()
                                .disabled(self.connecting_kubernetes)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.connect_kubernetes(window, cx)
                                })),
                        )
                        .into_any_element(),
                    KubernetesPage::Resources => self.render_kubernetes_resources(cx),
                    KubernetesPage::Helm => self.render_helm(cx),
                    KubernetesPage::Doctor => render_kubernetes_doctor(overview).into_any_element(),
                })
                .into_any_element(),
        };
        v_flex()
            .p_3()
            .child(section("Cluster", IconName::Blocks, content, cx))
            .into_any_element()
    }

    fn render_kubernetes_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        ToggleButtonGroup::single_row(
            "kubernetes-page",
            [
                ToggleButtonSimple::new(
                    "Overview",
                    cx.listener(|this, _, _, cx| {
                        this.kubernetes_page = KubernetesPage::Overview;
                        cx.notify();
                    }),
                ),
                ToggleButtonSimple::new(
                    "Resources",
                    cx.listener(|this, _, window, cx| {
                        this.kubernetes_page = KubernetesPage::Resources;
                        if matches!(this.kubernetes_resources, KubernetesResourcesState::Idle) {
                            this.load_kubernetes_resources(window, cx);
                        } else {
                            cx.notify();
                        }
                    }),
                ),
                ToggleButtonSimple::new(
                    "Helm",
                    cx.listener(|this, _, window, cx| {
                        this.kubernetes_page = KubernetesPage::Helm;
                        if matches!(this.helm_releases, HelmState::Idle) {
                            this.load_helm_releases(window, cx);
                        } else {
                            cx.notify();
                        }
                    }),
                ),
                ToggleButtonSimple::new(
                    "Doctor",
                    cx.listener(|this, _, _, cx| {
                        this.kubernetes_page = KubernetesPage::Doctor;
                        cx.notify();
                    }),
                ),
            ],
        )
        .style(ToggleButtonGroupStyle::Outlined)
        .selected_index(match self.kubernetes_page {
            KubernetesPage::Overview => 0,
            KubernetesPage::Resources => 1,
            KubernetesPage::Helm => 2,
            KubernetesPage::Doctor => 3,
        })
    }

    fn render_kubernetes_resources(&self, cx: &mut Context<Self>) -> AnyElement {
        let controls = v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("previous-kubernetes-kind", IconName::ChevronLeft)
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cycle_kubernetes_resource_kind(-1, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Label::new(self.kubernetes_resource_kind.display_name())),
                    )
                    .child(
                        IconButton::new("next-kubernetes-kind", IconName::ChevronRight)
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.cycle_kubernetes_resource_kind(1, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("refresh-kubernetes-resources", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.load_kubernetes_resources(window, cx)
                            })),
                    ),
            )
            .child(self.kubernetes_namespace_input.clone());
        let resources = match &self.kubernetes_resources {
            KubernetesResourcesState::Idle => muted("Choose a resource kind.").into_any_element(),
            KubernetesResourcesState::Loading => muted("Loading resources…").into_any_element(),
            KubernetesResourcesState::Failed(error) => Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            KubernetesResourcesState::Loaded(resources) if resources.is_empty() => {
                muted("No resources found.").into_any_element()
            }
            KubernetesResourcesState::Loaded(resources) => v_flex()
                .gap_1()
                .children(
                    resources
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(|(index, resource)| {
                            let selected = match &self.kubernetes_details {
                                KubernetesDetailsState::Loading(selected)
                                | KubernetesDetailsState::Loaded(selected, _)
                                | KubernetesDetailsState::Failed(selected, _) => {
                                    selected.kind == resource.kind
                                        && selected.namespace == resource.namespace
                                        && selected.name == resource.name
                                }
                                KubernetesDetailsState::None => false,
                            };
                            let resource_for_click = resource.clone();
                            ListItem::new(index)
                                .toggle_state(selected)
                                .child(
                                    v_flex()
                                        .min_w_0()
                                        .child(Label::new(resource.name.clone()).truncate())
                                        .child(muted(format_resource_metadata(&resource))),
                                )
                                .end_slot(muted(resource.status))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.load_kubernetes_details(
                                        resource_for_click.clone(),
                                        window,
                                        cx,
                                    )
                                }))
                        }),
                )
                .into_any_element(),
        };
        v_flex()
            .gap_2()
            .child(controls)
            .child(resources)
            .child(self.render_kubernetes_details(cx))
            .into_any_element()
    }

    fn render_kubernetes_details(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.kubernetes_details {
            KubernetesDetailsState::None => div().into_any_element(),
            KubernetesDetailsState::Loading(resource) => {
                muted(format!("Loading {}…", resource.name)).into_any_element()
            }
            KubernetesDetailsState::Failed(_, error) => Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            KubernetesDetailsState::Loaded(resource, yaml) => {
                let resource_for_logs = resource.clone();
                let resource_for_shell = resource.clone();
                let resource_for_forward = resource.clone();
                let resource_for_edit = resource.clone();
                let resource_for_scale = resource.clone();
                let resource_for_restart = resource.clone();
                let resource_for_delete = resource.clone();
                v_flex()
                    .gap_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .when(
                        matches!(resource.kind, ResourceKind::Pod | ResourceKind::Service),
                        |this| this.child(self.kubernetes_port_forward_input.clone()),
                    )
                    .when(
                        matches!(
                            resource.kind,
                            ResourceKind::Deployment | ResourceKind::StatefulSet
                        ),
                        |this| this.child(self.kubernetes_scale_input.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .when(resource.kind.supports_logs(), |this| {
                                this.child(
                                    Button::new("kubernetes-resource-logs", "Logs")
                                        .style(ButtonStyle::Outlined)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_resource_command(
                                                "logs",
                                                resource_for_logs.clone(),
                                                window,
                                                cx,
                                            )
                                        })),
                                )
                            })
                            .when(resource.kind == ResourceKind::Pod, |this| {
                                this.child(
                                    Button::new("kubernetes-resource-shell", "Shell")
                                        .style(ButtonStyle::Outlined)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_resource_command(
                                                "shell",
                                                resource_for_shell.clone(),
                                                window,
                                                cx,
                                            )
                                        })),
                                )
                            })
                            .when(
                                matches!(resource.kind, ResourceKind::Pod | ResourceKind::Service),
                                |this| {
                                    this.child(
                                        Button::new("kubernetes-resource-forward", "Forward")
                                            .style(ButtonStyle::Outlined)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.open_resource_command(
                                                    "port-forward",
                                                    resource_for_forward.clone(),
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                },
                            )
                            .when(resource.kind.supports_restart(), |this| {
                                this.child(
                                    Button::new("kubernetes-resource-restart", "Restart")
                                        .style(ButtonStyle::Outlined)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.restart_kubernetes_resource(
                                                resource_for_restart.clone(),
                                                window,
                                                cx,
                                            )
                                        })),
                                )
                            })
                            .when(
                                matches!(
                                    resource.kind,
                                    ResourceKind::Deployment | ResourceKind::StatefulSet
                                ),
                                |this| {
                                    this.child(
                                        Button::new("kubernetes-resource-scale", "Scale")
                                            .style(ButtonStyle::Outlined)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.open_resource_command(
                                                    "scale",
                                                    resource_for_scale.clone(),
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                },
                            )
                            .child(
                                Button::new("kubernetes-resource-edit", "Edit YAML")
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_resource_command(
                                            "edit",
                                            resource_for_edit.clone(),
                                            window,
                                            cx,
                                        )
                                    })),
                            )
                            .child(
                                Button::new("kubernetes-resource-delete", "Delete")
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.delete_kubernetes_resource(
                                            resource_for_delete.clone(),
                                            window,
                                            cx,
                                        )
                                    })),
                            ),
                    )
                    .child(
                        v_flex()
                            .p_2()
                            .rounded_md()
                            .bg(cx.theme().colors().editor_background)
                            .children(yaml.lines().take(200).map(|line| {
                                Label::new(line.to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                            })),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_helm(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.helm_releases {
            HelmState::Idle => muted("Open Helm to load releases.").into_any_element(),
            HelmState::Loading => muted("Loading Helm releases…").into_any_element(),
            HelmState::NotInstalled => v_flex()
                .gap_2()
                .child(muted("Helm is not installed in this project sandbox."))
                .child(muted(
                    "Install Helm in the project terminal to manage releases.",
                ))
                .into_any_element(),
            HelmState::Failed(error) => Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            HelmState::Loaded(releases) if releases.is_empty() => {
                muted("No Helm releases found.").into_any_element()
            }
            HelmState::Loaded(releases) => v_flex()
                .gap_1()
                .child(
                    Button::new("refresh-helm-releases", "Refresh")
                        .style(ButtonStyle::Outlined)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.load_helm_releases(window, cx)),
                        ),
                )
                .children(releases.iter().map(|release| {
                    v_flex()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            row()
                                .child(Label::new(release.name.clone()))
                                .child(muted(release.status.clone())),
                        )
                        .child(muted(format!(
                            "{} · {} · revision {}",
                            release.namespace, release.chart, release.revision
                        )))
                }))
                .into_any_element(),
        }
    }

    fn render_settings(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .p_3()
            .gap_4()
            .child(section(
                "Resource limits",
                IconName::Box,
                render_resource_settings(snapshot),
                cx,
            ))
            .child(section(
                "Repositories",
                IconName::GitBranch,
                self.render_repository_settings(snapshot, cx),
                cx,
            ))
            .child(section(
                "Ports",
                IconName::Link,
                self.render_ports(snapshot, cx),
                cx,
            ))
            .child(section(
                "Network policy",
                IconName::Settings,
                self.render_network_settings(snapshot, cx),
                cx,
            ))
            .into_any_element()
    }

    fn render_repository_settings(
        &self,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let repository_count = snapshot
            .probe
            .as_ref()
            .map(|probe| probe.repositories.len())
            .unwrap_or_default();
        v_flex()
            .gap_2()
            .child(
                row()
                    .child(Label::new("Connected repositories"))
                    .child(muted(repository_count.to_string())),
            )
            .child(self.repository_url_input.clone())
            .child(
                Button::new("clone-project-repository", "Add repository")
                    .style(ButtonStyle::Filled)
                    .full_width()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.clone_repository_from_input(window, cx)
                    })),
            )
            .child(muted(
                "The repository is cloned into this project's sandbox and remains isolated from the host.",
            ))
            .into_any_element()
    }

    fn install_missing_toolchains(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let PanelState::Loaded(snapshot) = &self.state else {
            return;
        };
        let missing = missing_toolchains(snapshot);
        if missing.is_empty() || self.installing_toolchains {
            return;
        }
        self.installing_toolchains = true;
        self.run_sbx_action(
            move |sbx, name| async move { sbx.install_toolchains(&name, &missing).await },
            window,
            cx,
        );
    }

    fn render_toolchains(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        let Some(probe) = &snapshot.probe else {
            return muted("Start the sandbox to see its toolchains.").into_any_element();
        };
        let missing = missing_toolchains(snapshot);
        let install_label = if self.installing_toolchains {
            "Installing… this can take a few minutes".to_string()
        } else {
            format!(
                "Install {}",
                missing
                    .iter()
                    .map(|toolchain| toolchain.display_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        v_flex()
            .gap_1()
            .when(probe.toolchains.is_empty(), |this| {
                this.child(muted("No toolchains detected."))
            })
            .children(probe.toolchains.iter().map(|status| {
                let version = match &status.version {
                    Some(version) => muted(version.clone()),
                    None => Label::new("missing")
                        .size(LabelSize::Small)
                        .color(Color::Error),
                };
                row()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Indicator::dot().color(if status.version.is_some() {
                                Color::Success
                            } else {
                                Color::Error
                            }))
                            .child(Label::new(status.toolchain.display_name()))
                            .when(!status.used_by_project, |this| {
                                this.child(muted("not used by the project"))
                            }),
                    )
                    .child(version)
            }))
            .when(!missing.is_empty() || self.installing_toolchains, |this| {
                this.child(
                    Button::new("install-toolchains", install_label)
                        .style(ButtonStyle::Filled)
                        .full_width()
                        .disabled(self.installing_toolchains)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.install_missing_toolchains(window, cx)
                        })),
                )
            })
            .into_any_element()
    }

    fn render_ports(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        let published_sandbox_ports = snapshot
            .ports
            .iter()
            .map(|port| port.sandbox_port)
            .collect::<Vec<_>>();
        let unpublished = snapshot
            .probe
            .as_ref()
            .map(|probe| {
                probe
                    .listening_ports
                    .iter()
                    .copied()
                    .filter(|port| !published_sandbox_ports.contains(port))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        v_flex()
            .gap_1()
            .when(
                snapshot.ports.is_empty() && unpublished.is_empty(),
                |this| this.child(muted("Nothing is listening or published yet.")),
            )
            .children(snapshot.ports.iter().enumerate().map(|(index, port)| {
                let host_ip = port
                    .host_ip
                    .clone()
                    .unwrap_or_else(|| "127.0.0.1".to_string());
                let url = format!("http://{host_ip}:{}", port.host_port);
                let unpublish = port.clone();
                row()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Indicator::dot().color(Color::Success))
                            .child(Label::new(format!(
                                "{host_ip}:{} → {}",
                                port.host_port, port.sandbox_port
                            ))),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                IconButton::new(("open-port", index), IconName::ArrowUpRight)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Open in browser"))
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                            .child(
                                IconButton::new(("unpublish-port", index), IconName::Close)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Unpublish"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let port = unpublish.clone();
                                        this.run_sbx_action(
                                            move |sbx, name| async move {
                                                sbx.unpublish_port(&name, &port).await
                                            },
                                            window,
                                            cx,
                                        )
                                    })),
                            ),
                    )
            }))
            .children(unpublished.into_iter().map(|sandbox_port| {
                row()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Indicator::dot().color(Color::Muted))
                            .child(Label::new(format!("{sandbox_port}")))
                            .child(muted("listening, not published")),
                    )
                    .child(
                        Button::new(("publish-port", sandbox_port as usize), "Publish")
                            .style(ButtonStyle::Subtle)
                            .size(ButtonSize::Compact)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.run_sbx_action(
                                    move |sbx, name| async move {
                                        sbx.publish_port(&name, sandbox_port, None)
                                            .await
                                            .map(|_| ())
                                    },
                                    window,
                                    cx,
                                )
                            })),
                    )
            }))
            .child(
                h_flex()
                    .pt_1()
                    .gap_2()
                    .items_end()
                    .child(div().flex_1().child(self.port_input.clone()))
                    .child(
                        Button::new("publish-custom-port", "Publish")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.publish_from_input(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_network_settings(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_2()
            .children(
                snapshot
                    .rules
                    .rules
                    .iter()
                    .filter(|rule| rule.resource_type == "network")
                    .enumerate()
                    .map(|(index, rule)| {
                        let decision_color = if rule.decision == "allow" {
                            Color::Success
                        } else {
                            Color::Error
                        };
                        let rule_id = rule.id.clone();
                        let removable = rule.scope.as_deref() != Some("global");
                        row()
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .gap_2()
                                    .child(
                                        Chip::new(rule.decision.clone())
                                            .label_color(decision_color),
                                    )
                                    .child(
                                        Label::new(rule.resources.join(", "))
                                            .size(LabelSize::Small)
                                            .truncate(),
                                    ),
                            )
                            .when(removable, |this| {
                                this.child(
                                    IconButton::new(
                                        ("remove-network-rule", index),
                                        IconName::Close,
                                    )
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Remove project rule"))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            let rule_id = rule_id.clone();
                                            this.run_sbx_action(
                                                move |sbx, name| async move {
                                                    sbx.remove_network_rule(&name, &rule_id).await
                                                },
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                                )
                            })
                    }),
            )
            .when(
                !snapshot
                    .rules
                    .rules
                    .iter()
                    .any(|rule| rule.resource_type == "network"),
                |this| this.child(muted("No network policy rules are visible.")),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant))
            .child(self.network_resource_input.clone())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("allow-network-resource", "Allow")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_network_rule_from_input(NetworkDecision::Allow, window, cx)
                            })),
                    )
                    .child(
                        Button::new("deny-network-resource", "Block")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_network_rule_from_input(NetworkDecision::Deny, window, cx)
                            })),
                    ),
            )
            .child(muted(
                "Rules added here apply only to this project. Global policy rules are read-only.",
            ))
            .into_any_element()
    }
}

fn missing_toolchains(snapshot: &Snapshot) -> Vec<sbx::Toolchain> {
    snapshot
        .probe
        .iter()
        .flat_map(|probe| &probe.toolchains)
        .filter(|status| status.is_missing())
        .map(|status| status.toolchain)
        .collect()
}

async fn load_snapshot(name: String) -> anyhow::Result<Snapshot> {
    let sbx = Sbx::locate()?;
    let info = sbx.get(&name).await?;
    let configured = sbx::configured_resources(&name);
    let running = info.status == SandboxStatus::Running;
    // Probing runs a command in the sandbox, which would start a stopped one.
    let (probe, ports, network, rules, kubernetes) = futures::join!(
        async {
            if running {
                sbx.probe(&name).await.map(Some)
            } else {
                Ok(None)
            }
        },
        async {
            if running {
                sbx.ports(&name).await
            } else {
                Ok(Vec::new())
            }
        },
        sbx.network_log(&name, NETWORK_LOG_LIMIT),
        sbx.policy_rules(&name),
        async {
            if running {
                kubernetes::probe(&sbx, &name).await
            } else {
                Ok(None)
            }
        },
    );
    let kubernetes = match kubernetes {
        Ok(Some(overview)) => KubernetesState::Connected(overview),
        Ok(None) => KubernetesState::NotConnected,
        Err(error) => KubernetesState::Failed(format!("{error:#}").into()),
    };
    Ok(Snapshot {
        info,
        configured,
        probe: probe?,
        ports: ports?,
        network: network.log_err().unwrap_or_default(),
        rules: rules.log_err().unwrap_or_default(),
        kubernetes,
    })
}

fn render_kubernetes_overview(overview: &KubernetesOverview, _cx: &App) -> gpui::Div {
    v_flex()
        .gap_2()
        .child(
            row()
                .child(Label::new("Context"))
                .child(muted(overview.context.clone())),
        )
        .child(
            row()
                .child(Label::new("API server"))
                .child(muted(overview.server.clone()).truncate()),
        )
        .child(
            row()
                .child(Label::new("Namespace"))
                .child(muted(overview.namespace.clone())),
        )
        .child(
            row()
                .child(Label::new("Kubernetes"))
                .child(muted(overview.version.clone())),
        )
        .child(
            row().child(Label::new("Nodes")).child(muted(
                overview
                    .total_nodes
                    .map(|total| format!("{} / {total} ready", overview.ready_nodes))
                    .unwrap_or_else(|| "not permitted".to_string()),
            )),
        )
        .child(
            row().child(Label::new("Pods")).child(muted(
                overview
                    .total_pods
                    .map(|total| format!("{} / {total} running", overview.running_pods))
                    .unwrap_or_else(|| "not permitted".to_string()),
            )),
        )
}

fn render_kubernetes_doctor(overview: &KubernetesOverview) -> gpui::Div {
    let nodes_healthy = overview
        .total_nodes
        .is_some_and(|total| total > 0 && overview.ready_nodes == total);
    let pods_healthy = overview
        .total_pods
        .is_some_and(|total| overview.running_pods == total);
    v_flex()
        .gap_2()
        .child(doctor_row(
            "API server",
            overview.version != "unknown",
            overview.version.clone(),
        ))
        .child(doctor_row(
            "Nodes",
            nodes_healthy,
            overview
                .total_nodes
                .map(|total| format!("{} of {total} ready", overview.ready_nodes))
                .unwrap_or_else(|| "permission denied".to_string()),
        ))
        .child(doctor_row(
            "Pods",
            pods_healthy,
            overview
                .total_pods
                .map(|total| format!("{} of {total} running", overview.running_pods))
                .unwrap_or_else(|| "permission denied".to_string()),
        ))
}

fn doctor_row(label: &'static str, healthy: bool, detail: String) -> gpui::Div {
    row()
        .child(
            h_flex()
                .gap_2()
                .child(Indicator::dot().color(if healthy {
                    Color::Success
                } else {
                    Color::Warning
                }))
                .child(Label::new(label)),
        )
        .child(muted(detail))
}

fn format_resource_metadata(resource: &ResourceSummary) -> String {
    let mut metadata = Vec::new();
    if let Some(namespace) = &resource.namespace {
        metadata.push(namespace.clone());
    }
    if let Some(ready) = &resource.ready {
        metadata.push(format!("ready {ready}"));
    }
    if let Some(restarts) = resource.restarts {
        metadata.push(format!("{restarts} restarts"));
    }
    metadata.join(" · ")
}

fn render_resources(snapshot: &Snapshot, cx: &App) -> impl IntoElement {
    let configured = snapshot.configured.clone().unwrap_or_default();
    let Some(probe) = &snapshot.probe else {
        return v_flex().child(muted("Start the sandbox to see its resources."));
    };

    let cpu_limit = match configured.cpus {
        Some(cpus) => format!("limit {cpus}"),
        None => "all host CPUs".to_string(),
    };
    let memory_limit = configured
        .memory
        .map(|memory| format!("limit {memory}"))
        .unwrap_or_else(|| "half of host memory".to_string());

    v_flex()
        .gap_2()
        .child(
            row()
                .child(Label::new(format!(
                    "{} CPUs",
                    probe
                        .cpus
                        .map(|cpus| cpus.to_string())
                        .unwrap_or_else(|| "?".to_string())
                )))
                .child(muted(format!(
                    "{cpu_limit} · load {}",
                    probe.load_average.clone().unwrap_or_default()
                ))),
        )
        .child(usage_bar(
            "memory",
            "Memory",
            probe
                .memory_total_bytes
                .zip(probe.memory_available_bytes)
                .map(|(total, available)| (total.saturating_sub(available), total)),
            memory_limit,
            cx,
        ))
        .child(usage_bar(
            "disk",
            "Disk",
            probe.disk_used_bytes.zip(probe.disk_total_bytes),
            configured
                .docker_disk
                .map(|size| format!("Docker {size}"))
                .unwrap_or_default(),
            cx,
        ))
}

fn render_resource_settings(snapshot: &Snapshot) -> impl IntoElement {
    let configured = snapshot.configured.clone().unwrap_or_default();
    let cpus = configured
        .cpus
        .map(|cpus| cpus.to_string())
        .unwrap_or_else(|| "Auto (all host CPUs)".to_string());
    let memory = configured
        .memory
        .unwrap_or_else(|| "Auto (half of host memory)".to_string());
    let disk = configured
        .docker_disk
        .unwrap_or_else(|| "Docker default".to_string());
    v_flex()
        .gap_2()
        .child(row().child(Label::new("CPU limit")).child(muted(cpus)))
        .child(row().child(Label::new("Memory limit")).child(muted(memory)))
        .child(row().child(Label::new("Docker disk")).child(muted(disk)))
        .child(Divider::horizontal().color(DividerColor::BorderVariant))
        .child(muted(
            "Docker Sandboxes fixes resource limits at creation time. Choose them in New Project; changing an existing project requires recreation.",
        ))
}

fn metric_card(label: &'static str, value: u64, color: Color, cx: &App) -> impl IntoElement {
    v_flex()
        .flex_1()
        .p_2()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().element_background)
        .child(
            Label::new(value.to_string())
                .size(LabelSize::Large)
                .color(color),
        )
        .child(muted(label))
}

fn render_published_ports(snapshot: &Snapshot) -> impl IntoElement {
    v_flex()
        .gap_1()
        .when(snapshot.ports.is_empty(), |this| {
            this.child(muted("No ports are published to the host."))
        })
        .children(snapshot.ports.iter().map(|port| {
            row()
                .child(Label::new(format!("{}", port.sandbox_port)))
                .child(muted(format!(
                    "{}:{} · {}",
                    port.host_ip.as_deref().unwrap_or("127.0.0.1"),
                    port.host_port,
                    port.protocol.as_deref().unwrap_or("tcp")
                )))
        }))
}

fn usage_bar(
    id: &'static str,
    title: &'static str,
    usage: Option<(u64, u64)>,
    note: String,
    cx: &App,
) -> impl IntoElement {
    let (used, total) = usage.unwrap_or((0, 0));
    v_flex()
        .gap_1()
        .child(
            row()
                .child(Label::new(format!(
                    "{title}  {} of {}",
                    format_bytes(used),
                    format_bytes(total)
                )))
                .child(muted(note)),
        )
        .child(ProgressBar::new(id, used as f32, total.max(1) as f32, cx))
}

fn render_repositories(snapshot: &Snapshot, cx: &App) -> impl IntoElement {
    let repositories = snapshot
        .probe
        .as_ref()
        .map(|probe| probe.repositories.clone())
        .unwrap_or_default();
    v_flex()
        .gap_2()
        .when(repositories.is_empty(), |this| {
            this.child(muted("No Git repositories in the workspace."))
        })
        .children(repositories.into_iter().map(|repository| {
            let name = repository
                .path
                .rsplit('/')
                .next()
                .unwrap_or(repository.path.as_str())
                .to_string();
            v_flex()
                .p_2()
                .gap_1()
                .rounded_md()
                .border_1()
                .border_l_2()
                .border_color(cx.theme().colors().border_focused)
                .bg(cx.theme().colors().editor_background)
                .child(
                    h_flex()
                        .justify_between()
                        .gap_2()
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::GitBranch)
                                        .size(IconSize::Small)
                                        .color(Color::Accent),
                                )
                                .child(Label::new(name).truncate()),
                        )
                        .when_some(repository.branch, |this, branch| {
                            this.child(Chip::new(branch).label_color(Color::Accent))
                        }),
                )
                .child(muted(repository.path))
                .when_some(repository.remote, |this, remote| {
                    this.child(
                        Label::new(remote)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                })
        }))
}

fn render_agents(snapshot: &Snapshot) -> impl IntoElement {
    let agents = snapshot
        .probe
        .as_ref()
        .map(|probe| probe.agents.clone())
        .unwrap_or_default();
    let mut requests = snapshot
        .network
        .allowed
        .iter()
        .filter(|activity| is_model_api(&activity.host))
        .cloned()
        .collect::<Vec<_>>();
    requests.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));

    v_flex()
        .gap_2()
        .child(
            h_flex()
                .gap_1()
                .flex_wrap()
                .when(agents.is_empty(), |this| {
                    this.child(muted("No agent CLIs installed in the sandbox."))
                })
                .children(agents.into_iter().map(Chip::new)),
        )
        .child(
            Label::new("Recent model requests")
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .when(requests.is_empty(), |this| {
            this.child(muted("No requests to model APIs yet."))
        })
        .children(requests.into_iter().map(activity_row))
}

fn render_network(snapshot: &Snapshot) -> impl IntoElement {
    let mut recent = snapshot.network.allowed.clone();
    recent.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    recent.truncate(RECENT_HOSTS);
    let blocked = snapshot.network.blocked.clone();

    v_flex()
        .gap_1()
        .children(
            snapshot
                .rules
                .rules
                .iter()
                .filter(|rule| rule.resource_type == "network")
                .map(|rule| {
                    let color = if rule.decision == "allow" {
                        Color::Success
                    } else {
                        Color::Error
                    };
                    h_flex()
                        .gap_2()
                        .child(Chip::new(rule.decision.clone()).label_color(color))
                        .child(Label::new(rule.resources.join(", ")).size(LabelSize::Small))
                        .child(muted(rule.scope.clone().unwrap_or_default()))
                }),
        )
        .child(Divider::horizontal().color(DividerColor::BorderVariant))
        .when(!blocked.is_empty(), |this| {
            this.child(
                Label::new(format!("{} blocked hosts", blocked.len()))
                    .size(LabelSize::Small)
                    .color(Color::Error),
            )
            .children(blocked.into_iter().map(activity_row))
        })
        .when(recent.is_empty(), |this| {
            this.child(muted("No network activity yet."))
        })
        .children(recent.into_iter().map(activity_row))
}

fn activity_row(activity: NetworkActivity) -> impl IntoElement {
    row()
        .child(Label::new(activity.host).size(LabelSize::Small).truncate())
        .child(muted(format!(
            "{} × · {}",
            activity.count,
            activity
                .last_seen
                .as_deref()
                .map(relative_time)
                .unwrap_or_default()
        )))
}

fn section(
    title: &'static str,
    icon: IconName,
    content: impl IntoElement,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .gap_1p5()
        .child(
            h_flex()
                .gap_1p5()
                .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                .child(Label::new(title).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(
            v_flex()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().element_background)
                .child(content),
        )
}

fn row() -> gpui::Div {
    h_flex().w_full().gap_2().justify_between()
}

fn muted(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

fn message(text: &'static str) -> impl IntoElement {
    v_flex().p_3().child(muted(text))
}

fn is_model_api(host: &str) -> bool {
    let host = host.split(':').next().unwrap_or(host);
    MODEL_API_HOSTS.contains(&host)
}

/// Parses `3000` (any free host port) or `8080:3000` (host port 8080).
pub(crate) fn parse_port_mapping(input: &str) -> Option<(Option<u16>, u16)> {
    let input = input.trim();
    match input.split_once(':') {
        Some((host, sandbox)) => Some((
            Some(host.trim().parse().ok()?),
            sandbox.trim().parse().ok()?,
        )),
        None => Some((None, input.parse().ok()?)),
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn relative_time(timestamp: &str) -> String {
    let Ok(time) = DateTime::parse_from_rfc3339(timestamp) else {
        return String::new();
    };
    let seconds = (Utc::now() - time.with_timezone(&Utc)).num_seconds().max(0);
    match seconds {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

impl Render for SandboxPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("sands-sandbox-panel")
            .key_context("SandboxPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_header(cx))
            .child(self.render_navigation(cx))
            .child(
                v_flex()
                    .id("sands-sandbox-panel-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(self.render_body(cx)),
            )
    }
}

impl Focusable for SandboxPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SandboxPanel {}

impl Panel for SandboxPanel {
    fn persistent_name() -> &'static str {
        "SandsSandboxPanel"
    }

    fn panel_key() -> &'static str {
        "SandsSandboxPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(340.)
    }

    fn initial_size_state(&self, _window: &Window, _cx: &App) -> PanelSizeState {
        PanelSizeState {
            size: None,
            flex: None,
        }
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Box)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Sandbox")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        ToggleSandboxPanel.boxed_clone()
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let became_visible = active && !self.active;
        self.active = active;
        if became_visible {
            self.schedule_refresh(window, cx);
        }
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_mappings() {
        assert_eq!(parse_port_mapping("3000"), Some((None, 3000)));
        assert_eq!(parse_port_mapping(" 8080:3000 "), Some((Some(8080), 3000)));
        assert_eq!(parse_port_mapping("http"), None);
        assert_eq!(parse_port_mapping("8080:"), None);
        assert_eq!(parse_port_mapping("70000"), None);
    }

    #[test]
    fn formats_bytes_and_model_hosts() {
        assert_eq!(format_bytes(45_277_184), "43 MB");
        assert_eq!(format_bytes(12_417_584 * 1024), "12 GB");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert!(is_model_api("api.anthropic.com:443"));
        assert!(!is_model_api("github.com:443"));
    }
}
