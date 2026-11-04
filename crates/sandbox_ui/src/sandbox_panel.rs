use std::{future::Future, time::Duration};

use chrono::{DateTime, Utc};
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable,
    Pixels, SharedString, Task, WeakEntity, actions, px,
};
use sbx::{
    ConfiguredResources, NetworkActivity, NetworkLog, PolicyRules, PublishedPort, SandboxInfo,
    SandboxProbe, SandboxStatus, Sbx, SbxError,
};
use ui::{Chip, Divider, DividerColor, Indicator, ProgressBar, Tooltip, prelude::*};
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
const RECENT_HOSTS: usize = 8;

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
}

enum PanelState {
    NoSandbox,
    Loading,
    Loaded(Box<Snapshot>),
    Failed(SharedString),
}

/// Shows the sandbox behind the current project: its resources, repositories, agents, ports
/// and network activity.
pub struct SandboxPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    state: PanelState,
    active: bool,
    port_input: Entity<InputField>,
    action_error: Option<SharedString>,
    installing_toolchains: bool,
    _refresh: Task<()>,
    _action: Task<()>,
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
                let mut panel = Self {
                    workspace: workspace_handle,
                    focus_handle: cx.focus_handle(),
                    position: DockPosition::Right,
                    state: PanelState::Loading,
                    active: false,
                    port_input,
                    action_error: None,
                    installing_toolchains: false,
                    _refresh: Task::ready(()),
                    _action: Task::ready(()),
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

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        let ports = self.render_ports(snapshot, cx);
        let toolchains = self.render_toolchains(snapshot, cx);

        v_flex()
            .p_3()
            .gap_4()
            .when_some(self.action_error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
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
                render_repositories(snapshot),
                cx,
            ))
            .child(section(
                "Agents",
                IconName::Sparkle,
                render_agents(snapshot),
                cx,
            ))
            .child(section("Ports", IconName::Link, ports, cx))
            .child(section(
                "Network",
                IconName::Public,
                render_network(snapshot),
                cx,
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
    let (probe, ports, network, rules) = futures::join!(
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
    );
    Ok(Snapshot {
        info,
        configured,
        probe: probe?,
        ports: ports?,
        network: network.log_err().unwrap_or_default(),
        rules: rules.log_err().unwrap_or_default(),
    })
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

fn render_repositories(snapshot: &Snapshot) -> impl IntoElement {
    let repositories = snapshot
        .probe
        .as_ref()
        .map(|probe| probe.repositories.clone())
        .unwrap_or_default();
    v_flex()
        .gap_1()
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
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(name))
                        .when_some(repository.branch, |this, branch| {
                            this.child(Chip::new(branch).label_color(Color::Accent))
                        }),
                )
                .child(muted(repository.remote.unwrap_or(repository.path)))
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
fn parse_port_mapping(input: &str) -> Option<(Option<u16>, u16)> {
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
