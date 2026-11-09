use std::{future::Future, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, PathPromptOptions, Pixels, SharedString, Subscription, Task, WeakEntity, actions,
    px,
};
use kubernetes::{KubernetesOverview, PreparedConnection};
use sbx::{
    ConfiguredResources, NetworkActivity, NetworkDecision, NetworkLog, PolicyRules, PublishedPort,
    SandboxInfo, SandboxProbe, SandboxStatus, Sbx, SbxError,
};
use ui::{Divider, DividerColor, Indicator, Tooltip, prelude::*};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelSizeState},
};

use crate::{
    connected_sandbox,
    design::{
        Meter, Tone, badge, caption, icon_button, key_value, primary_button, secondary_button,
        section_header, status_dot,
    },
    new_project_modal::agent_icon,
    sbx_problem::{Problem, ProblemFixed, SbxProblemView, problem_view},
};

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

/// The last snapshot of every sandbox, shared by the Sandbox panels of all projects.
#[derive(Default)]
struct SnapshotCache(collections::HashMap<String, Arc<Snapshot>>);

impl gpui::Global for SnapshotCache {}

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

enum PanelState {
    NoSandbox,
    Loading,
    Loaded(Arc<Snapshot>),
    Failed(SharedString),
    Problem(Entity<SbxProblemView>),
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
    _refresh: Task<()>,
    _action: Task<()>,
    _problem_subscription: Option<Subscription>,
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
                let mut panel = Self {
                    workspace: workspace_handle,
                    focus_handle: cx.focus_handle(),
                    position: DockPosition::Right,
                    // Each project has its own panel; the last snapshot of its sandbox shows at
                    // once instead of a loading state.
                    state: connected_sandbox(workspace, cx)
                        .and_then(|name| cx.try_global::<SnapshotCache>()?.0.get(&name).cloned())
                        .map(PanelState::Loaded)
                        .unwrap_or(PanelState::Loading),
                    active: false,
                    page: PanelPage::Overview,
                    port_input,
                    network_resource_input,
                    repository_url_input,
                    action_error: None,
                    installing_toolchains: false,
                    connecting_kubernetes: false,
                    _refresh: Task::ready(()),
                    _action: Task::ready(()),
                    _problem_subscription: None,
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
            PanelState::NoSandbox
            | PanelState::Loading
            | PanelState::Failed(_)
            | PanelState::Problem(_) => None,
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
                    let result = match name {
                        None => None,
                        Some(name) => Some(cx.background_spawn(load_snapshot(name)).await),
                    };
                    let updated = this.update_in(cx, |this, window, cx| {
                        this.state = match result {
                            None => PanelState::NoSandbox,
                            Some(Ok(snapshot)) => {
                                let snapshot = Arc::new(snapshot);
                                cx.default_global::<SnapshotCache>()
                                    .0
                                    .insert(snapshot.info.name.clone(), snapshot.clone());
                                PanelState::Loaded(snapshot)
                            }
                            Some(Err(error)) => match Problem::of_any(&error) {
                                Some(problem) => this.problem_state(problem, window, cx),
                                None => PanelState::Failed(format!("{error:#}").into()),
                            },
                        };
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

    /// Keeps the current problem view while the problem lasts, so a sign-in in progress
    /// survives the periodic refresh.
    fn problem_state(
        &mut self,
        problem: Problem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PanelState {
        if let PanelState::Problem(view) = &self.state
            && view.read(cx).problem() == &problem
        {
            return PanelState::Problem(view.clone());
        }
        let view = problem_view(problem, cx);
        self._problem_subscription = Some(cx.subscribe_in(
            &view,
            window,
            |this, _, _: &ProblemFixed, window, cx| this.schedule_refresh(window, cx),
        ));
        PanelState::Problem(view)
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

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, status, agent) = match &self.state {
            PanelState::Loaded(snapshot) => (
                snapshot.info.name.clone(),
                Some(snapshot.info.status),
                Some(snapshot.info.agent.clone()),
            ),
            _ => ("Sandbox".to_string(), None, None),
        };
        let (status_label, tone) = match status {
            Some(SandboxStatus::Running) => ("Running", Tone::Ok),
            Some(SandboxStatus::Stopped) => ("Stopped", Tone::Muted),
            Some(SandboxStatus::Unknown) => ("Unknown", Tone::Warn),
            None => ("", Tone::Muted),
        };

        h_flex()
            .h(px(36.))
            .pl_3()
            .pr_1p5()
            .gap_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .when(status.is_some(), |this| this.child(status_dot(tone, cx)))
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .truncate(),
                    )
                    .when(status.is_some(), |this| this.child(caption(status_label)))
                    .when_some(agent, |this, agent| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Icon::new(agent_icon(&agent))
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(caption(crate::agent_display_name(&agent).to_string())),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .when(status == Some(SandboxStatus::Running), |this| {
                        this.child(
                            icon_button("stop-sandbox", IconName::Stop, "Stop sandbox").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.run_sbx_action(
                                        |sbx, name| async move { sbx.stop(&name).await },
                                        window,
                                        cx,
                                    )
                                }),
                            ),
                        )
                    })
                    .when(status == Some(SandboxStatus::Stopped), |this| {
                        this.child(
                            icon_button("start-sandbox", IconName::PlayFilled, "Start sandbox")
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
                        icon_button("refresh-sandbox", IconName::RotateCw, "Refresh").on_click(
                            cx.listener(|this, _, window, cx| this.schedule_refresh(window, cx)),
                        ),
                    ),
            )
    }

    fn render_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let tabs = [
            (PanelPage::Overview, "Overview"),
            (PanelPage::Kubernetes, "Kubernetes"),
            (PanelPage::Traffic, "Network"),
            (PanelPage::Settings, "Settings"),
        ];
        h_flex()
            .h(px(32.))
            .px_3()
            .gap_4()
            .border_b_1()
            .border_color(colors.border_variant)
            .children(tabs.into_iter().map(|(page, title)| {
                let selected = self.page == page;
                v_flex()
                    .id(title)
                    .h_full()
                    .justify_center()
                    .cursor_pointer()
                    .border_b_2()
                    .border_color(if selected {
                        colors.border_focused
                    } else {
                        gpui::transparent_black()
                    })
                    .child(Label::new(title).size(LabelSize::Small).color(if selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.page = page;
                        cx.notify();
                    }))
            }))
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = match &self.state {
            PanelState::NoSandbox => {
                return message("Open a project from the Projects panel to see its sandbox.")
                    .into_any_element();
            }
            PanelState::Loading => return message("Loading sandbox…").into_any_element(),
            PanelState::Problem(view) => return view.clone().into_any_element(),
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
            .child(section(
                "Agent",
                IconName::Sparkle,
                render_agent(snapshot, cx),
                cx,
            ))
            .child(section(
                "Resources",
                IconName::Box,
                render_resources(snapshot, cx),
                cx,
            ))
            .child(section(
                "Repositories",
                IconName::GitBranch,
                render_repositories(snapshot, cx),
                cx,
            ))
            .child(section("Toolchains", IconName::ToolHammer, toolchains, cx))
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
            .child(
                h_flex()
                    .px_3()
                    .py_3()
                    .gap_4()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(metric_card("Allowed requests", allowed, Color::Success, cx))
                    .child(metric_card("Blocked requests", blocked, Color::Error, cx)),
            )
            .child(section(
                "Network traffic",
                IconName::Public,
                render_network(snapshot, cx),
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
                    h_flex().child(
                        primary_button("connect-kubernetes", import_label)
                            .disabled(self.connecting_kubernetes)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.connect_kubernetes(window, cx)
                            })),
                    ),
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
                    h_flex().child(
                        secondary_button("replace-kubernetes", replace_label)
                            .disabled(self.connecting_kubernetes)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.connect_kubernetes(window, cx)
                            })),
                    ),
                )
                .into_any_element(),
            KubernetesState::Connected(overview) => v_flex()
                .gap_3()
                .child(render_kubernetes_summary(overview, cx))
                .child(
                    h_flex()
                        .gap_2()
                        .child(primary_button("open-kubernetes", "Open Kubernetes").on_click(
                            |_, window, cx| {
                                window.dispatch_action(
                                    crate::kubernetes_view::OpenKubernetes.boxed_clone(),
                                    cx,
                                )
                            },
                        ))
                        .child(
                            secondary_button("replace-kubernetes", replace_label)
                                .disabled(self.connecting_kubernetes)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.connect_kubernetes(window, cx)
                                })),
                        ),
                )
                .child(caption(
                    "kubectl and helm are available to this project's terminals and agents.",
                ))
                .into_any_element(),
        };
        v_flex()
            .child(section("Cluster", IconName::Blocks, content, cx))
            .into_any_element()
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
                            .child(status_dot(
                                match (&status.version, status.used_by_project) {
                                    (None, _) => Tone::Error,
                                    (Some(_), true) => Tone::Ok,
                                    (Some(_), false) => Tone::Muted,
                                },
                                cx,
                            ))
                            .child(
                                Label::new(status.toolchain.display_name())
                                    .size(LabelSize::Small)
                                    .color(if status.used_by_project {
                                        Color::Default
                                    } else {
                                        Color::Muted
                                    }),
                            ),
                    )
                    .child(version.buffer_font(cx))
            }))
            .when(!missing.is_empty() || self.installing_toolchains, |this| {
                this.child(
                    h_flex().pt_1().child(
                        primary_button("install-toolchains", install_label)
                            .disabled(self.installing_toolchains)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.install_missing_toolchains(window, cx)
                            })),
                    ),
                )
            })
            .when(
                probe
                    .toolchains
                    .iter()
                    .any(|status| !status.used_by_project),
                |this| {
                    this.child(caption(
                        "Dimmed toolchains are installed but the project does not use them.",
                    ))
                },
            )
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
                                    .child(badge(
                                        rule.decision.clone(),
                                        if decision_color == Color::Success {
                                            Tone::Ok
                                        } else {
                                            Tone::Error
                                        },
                                        cx,
                                    ))
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
    let mut probe = probe?;
    if let Some(found) = &probe
        && !found.claude_status_hook
        && (info.agent == "claude" || found.agents.iter().any(|agent| agent == "claude"))
    {
        // The status line reports Claude's limits; it only exists once Sands installs it.
        if sbx
            .install_claude_status_hook(&name)
            .await
            .log_err()
            .is_some()
        {
            probe = sbx.probe(&name).await.log_err().or(probe);
        }
    }
    let kubernetes = match kubernetes {
        Ok(Some(overview)) => KubernetesState::Connected(overview),
        Ok(None) => KubernetesState::NotConnected,
        Err(error) => KubernetesState::Failed(format!("{error:#}").into()),
    };
    Ok(Snapshot {
        info,
        configured,
        probe,
        ports: ports?,
        network: network.log_err().unwrap_or_default(),
        rules: rules.log_err().unwrap_or_default(),
        kubernetes,
    })
}

fn render_kubernetes_summary(overview: &KubernetesOverview, cx: &App) -> impl IntoElement {
    let nodes = match overview.total_nodes {
        Some(total) => format!("{}/{total} ready", overview.ready_nodes),
        None => "—".to_string(),
    };
    let pods = match overview.total_pods {
        Some(total) => format!("{}/{total} running", overview.running_pods),
        None => "—".to_string(),
    };
    let healthy = overview
        .total_nodes
        .is_none_or(|total| total == overview.ready_nodes);
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .child(status_dot(if healthy { Tone::Ok } else { Tone::Error }, cx))
                .child(
                    Label::new(overview.context.clone())
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM)
                        .truncate(),
                ),
        )
        .child(caption(format!("{} · {}", overview.server, overview.version)).truncate())
        .child(key_value("Nodes", Label::new(nodes).size(LabelSize::Small)))
        .child(key_value("Pods", Label::new(pods).size(LabelSize::Small)))
        .child(key_value(
            "Namespace",
            Label::new(overview.namespace.clone()).size(LabelSize::Small),
        ))
}

fn render_resources(snapshot: &Snapshot, _cx: &App) -> impl IntoElement {
    let configured = snapshot.configured.clone().unwrap_or_default();
    let Some(probe) = &snapshot.probe else {
        return v_flex().child(muted("Start the sandbox to see its resources."));
    };
    let cpus = probe.cpus.unwrap_or(1).max(1);
    let load = probe
        .load_average
        .as_deref()
        .and_then(|load| load.split_whitespace().next())
        .and_then(|load| load.parse::<f32>().ok())
        .unwrap_or_default();
    let memory = probe
        .memory_total_bytes
        .zip(probe.memory_available_bytes)
        .map(|(total, available)| (total.saturating_sub(available), total));
    let disk = probe.disk_used_bytes.zip(probe.disk_total_bytes);
    let share = |usage: Option<(u64, u64)>| {
        usage
            .map(|(used, total)| used as f32 / total.max(1) as f32)
            .unwrap_or_default()
    };
    let amounts = |usage: Option<(u64, u64)>| {
        usage
            .map(|(used, total)| format!("{} / {}", format_bytes(used), format_bytes(total)))
            .unwrap_or_else(|| "—".to_string())
    };
    v_flex()
        .gap_3()
        .child(
            Meter::new(load / cpus as f32)
                .label("CPU")
                .value_text(format!("load {load:.2} on {cpus} cores"))
                .caption(match configured.cpus {
                    Some(limit) => format!("limited to {limit} cores"),
                    None => "all host cores".to_string(),
                })
                .usage_tone(),
        )
        .child(
            Meter::new(share(memory))
                .label("Memory")
                .value_text(amounts(memory))
                .caption(
                    configured
                        .memory
                        .map(|memory| format!("limited to {memory}"))
                        .unwrap_or_else(|| "half of host memory".to_string()),
                )
                .usage_tone(),
        )
        .child(
            Meter::new(share(disk))
                .label("Disk")
                .value_text(amounts(disk))
                .usage_tone(),
        )
}

/// Limits, cost and context of the project's agent, from what the agent last reported.
fn render_agent(snapshot: &Snapshot, cx: &App) -> impl IntoElement {
    let agent = snapshot.info.agent.as_str();
    let probe = snapshot.probe.as_ref();
    let usage = probe.and_then(|probe| {
        probe
            .agent_usage
            .iter()
            .find(|usage| usage.agent == agent)
            .or_else(|| probe.agent_usage.first())
    });
    let header = h_flex()
        .gap_2()
        .child(
            Icon::new(agent_icon(
                usage.map(|usage| usage.agent.as_str()).unwrap_or(agent),
            ))
            .size(IconSize::Small)
            .color(Color::Muted),
        )
        .child(
            Label::new(
                crate::agent_display_name(usage.map(|usage| usage.agent.as_str()).unwrap_or(agent))
                    .to_string(),
            )
            .size(LabelSize::Small)
            .weight(FontWeight::MEDIUM),
        )
        .children(
            usage
                .and_then(|usage| usage.plan.clone())
                .map(|plan| badge(plan, Tone::Info, cx)),
        )
        .children(usage.and_then(|usage| usage.model.clone()).map(caption));

    let model_requests = snapshot
        .network
        .allowed
        .iter()
        .filter(|activity| is_model_api(&activity.host))
        .collect::<Vec<_>>();
    let requests_row = (!model_requests.is_empty()).then(|| {
        let count = model_requests
            .iter()
            .map(|activity| activity.count)
            .sum::<u64>();
        let last = model_requests
            .iter()
            .filter_map(|activity| activity.last_seen.as_deref())
            .max()
            .map(relative_time)
            .unwrap_or_default();
        key_value(
            "Model requests",
            Label::new(format!("{count} · last {last}")).size(LabelSize::Small),
        )
    });

    let Some(usage) = usage else {
        let hint = if agent == "shell" {
            "This project has no coding agent. Choose one when you create a project."
        } else if probe.is_none() {
            "Start the sandbox to see the agent's limits."
        } else {
            "Limits appear after the agent's first reply. Claude reports them only on Pro and \
             Max plans."
        };
        return v_flex()
            .gap_2()
            .child(header)
            .child(muted(hint))
            .children(requests_row);
    };

    let now = Utc::now().timestamp();
    v_flex()
        .gap_3()
        .child(header)
        .when(usage.windows.is_empty(), |this| {
            this.child(muted("The agent has not reported subscription limits."))
        })
        .children(usage.windows.iter().map(|window| {
            let resets = window
                .resets_at
                .filter(|resets_at| *resets_at > now)
                .map(|resets_at| format!("resets in {}", format_reset(resets_at, now)))
                .unwrap_or_else(|| "reset time unknown".to_string());
            Meter::new((window.used_percent / 100.0) as f32)
                .label(window.label.clone())
                .value_text(format!("{:.0}%", window.used_percent))
                .caption(resets)
                .usage_tone()
        }))
        .child(
            v_flex()
                .when_some(usage.session_cost_usd, |this, cost| {
                    this.child(key_value(
                        "Session cost",
                        Label::new(format!("${cost:.2}")).size(LabelSize::Small),
                    ))
                })
                .when_some(usage.context_used_percent, |this, context| {
                    this.child(key_value(
                        "Context used",
                        Label::new(format!("{context:.0}%")).size(LabelSize::Small),
                    ))
                })
                .when_some(usage.session_tokens, |this, tokens| {
                    this.child(key_value(
                        "Session tokens",
                        Label::new(format_count(tokens)).size(LabelSize::Small),
                    ))
                })
                .children(requests_row),
        )
        .children(usage.updated_at.map(|updated| {
            caption(format!(
                "Reported {}",
                relative_seconds((now - updated).max(0))
            ))
        }))
}

/// "2h 14m", or "3d 4h" for windows a few days away.
pub(crate) fn format_reset(resets_at: i64, now: i64) -> String {
    let minutes = (resets_at - now).max(0) / 60;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    let local = chrono::DateTime::from_timestamp(resets_at, 0)
        .map(|time| time.with_timezone(&chrono::Local));
    let at = local
        .map(|time| {
            if days == 0 {
                time.format(" · %H:%M").to_string()
            } else {
                time.format(" · %a %H:%M").to_string()
            }
        })
        .unwrap_or_default();
    if days > 0 {
        format!("{days}d {hours}h{at}")
    } else if hours > 0 {
        format!("{hours}h {minutes}m{at}")
    } else {
        format!("{minutes}m{at}")
    }
}

fn format_count(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{:.1}K", count as f64 / 1_000.0),
        _ => format!("{:.1}M", count as f64 / 1_000_000.0),
    }
}

fn relative_seconds(seconds: i64) -> String {
    match seconds {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
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

fn metric_card(label: &'static str, value: u64, color: Color, _cx: &App) -> impl IntoElement {
    v_flex()
        .flex_1()
        .gap_0p5()
        .child(
            Label::new(value.to_string())
                .size(LabelSize::Large)
                .weight(FontWeight::SEMIBOLD)
                .color(color),
        )
        .child(caption(label))
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
            h_flex()
                .gap_2()
                .items_start()
                .child(
                    div().pt_0p5().child(
                        Icon::new(IconName::GitBranch)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_between()
                                .child(
                                    Label::new(name)
                                        .size(LabelSize::Small)
                                        .weight(FontWeight::MEDIUM)
                                        .truncate(),
                                )
                                .when_some(repository.branch, |this, branch| {
                                    this.child(
                                        Label::new(branch)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Accent)
                                            .buffer_font(cx),
                                    )
                                }),
                        )
                        .child(
                            Label::new(repository.remote.unwrap_or(repository.path))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
        }))
}

fn render_network(snapshot: &Snapshot, cx: &App) -> impl IntoElement {
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
                        .child(badge(
                            rule.decision.clone(),
                            if color == Color::Success {
                                Tone::Ok
                            } else {
                                Tone::Error
                            },
                            cx,
                        ))
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

/// A titled group of the panel, separated from the next one by a hairline.
fn section(
    title: &'static str,
    _icon: IconName,
    content: impl IntoElement,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .px_3()
        .pt_2()
        .pb_3()
        .gap_1p5()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .child(section_header(title, None))
        .child(content)
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
    fn formats_limit_resets() {
        let now = 1_793_880_000;
        assert!(format_reset(now + 2 * 3600 + 14 * 60, now).starts_with("2h 14m · "));
        assert!(format_reset(now + 3 * 86400 + 4 * 3600, now).starts_with("3d 4h · "));
        assert!(format_reset(now + 90, now).starts_with("1m · "));
        assert_eq!(format_count(950), "950");
        assert_eq!(format_count(12_345), "12.3K");
        assert_eq!(relative_seconds(7200), "2h ago");
    }

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
