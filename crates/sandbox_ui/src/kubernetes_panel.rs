use std::{path::PathBuf, sync::Arc, time::Duration};

use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, PathPromptOptions, Pixels, PromptLevel, SharedString, Task, WeakEntity, actions,
    px,
};
use kubernetes::{Cluster, ClusterStatus, KubeconfigEntry, PreparedConnection};
use sbx::Sbx;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelSizeState},
};

use crate::{
    connected_sandbox, connections,
    design::{
        Tone, badge, caption, icon_button, primary_button, secondary_button, section_header,
        status_dot,
    },
    kubernetes_view::{KubernetesView, OpenKubernetes},
};

actions!(
    sands,
    [
        /// Toggles focus on the panel with the Kubernetes clusters of the current project.
        ToggleKubernetesPanel
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleKubernetesPanel, window, cx| {
            workspace.toggle_panel_focus::<KubernetesPanel>(window, cx);
        });
    })
    .detach();
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// The last cluster list of every sandbox, shared by the Kubernetes panels of all projects.
#[derive(Default)]
struct ClusterCache(collections::HashMap<String, Arc<Vec<ClusterStatus>>>);

impl gpui::Global for ClusterCache {}

enum PanelState {
    NoSandbox,
    Loading,
    Loaded(Arc<Vec<ClusterStatus>>),
    Failed(SharedString),
}

/// The contexts of a kubeconfig file the user picked, to choose which ones to connect.
struct ContextPicker {
    path: PathBuf,
    entries: Vec<KubeconfigEntry>,
}

/// Lists the Kubernetes clusters connected to the project's sandbox: one per kubeconfig context,
/// such as dev, stage and prod. kubectl runs in the sandbox; tokens stay on the host.
pub struct KubernetesPanel {
    workspace: WeakEntity<Workspace>,
    /// The project's sandbox. A workspace keeps its remote connection, so this never changes, and
    /// the dock activates panels while the workspace is being updated, when it cannot be read.
    sandbox: Option<String>,
    focus_handle: FocusHandle,
    position: DockPosition,
    state: PanelState,
    active: bool,
    picker: Option<ContextPicker>,
    busy: Option<SharedString>,
    error: Option<SharedString>,
    cluster: Option<(String, Arc<Cluster>)>,
    _refresh: Task<()>,
    _action: Task<()>,
}

impl KubernetesPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let workspace_handle = workspace.weak_handle();
            let sandbox = connected_sandbox(workspace, cx);
            let state = match &sandbox {
                None => PanelState::NoSandbox,
                Some(name) => cx
                    .try_global::<ClusterCache>()
                    .and_then(|cache| cache.0.get(name).cloned())
                    .map(PanelState::Loaded)
                    .unwrap_or(PanelState::Loading),
            };
            cx.new(|cx| Self {
                workspace: workspace_handle,
                sandbox,
                focus_handle: cx.focus_handle(),
                position: DockPosition::Right,
                state,
                active: false,
                picker: None,
                busy: None,
                error: None,
                cluster: None,
                _refresh: Task::ready(()),
                _action: Task::ready(()),
            })
        })
    }

    fn sandbox_name(&self, _cx: &App) -> Option<String> {
        self.sandbox.clone()
    }

    /// One shell session per sandbox, reused by every request of this panel.
    fn cluster(&mut self, name: &str) -> anyhow::Result<Arc<Cluster>> {
        if let Some((existing, cluster)) = &self.cluster
            && existing == name
        {
            return Ok(cluster.clone());
        }
        let cluster = Arc::new(Cluster::new(Sbx::locate()?, name));
        self.cluster = Some((name.to_string(), cluster.clone()));
        Ok(cluster)
    }

    /// Reloads now, then periodically while the panel is visible. Checking clusters runs
    /// commands in the sandbox, so a hidden panel does not poll.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.sandbox_name(cx) else {
            self.state = PanelState::NoSandbox;
            cx.notify();
            return;
        };
        let cluster = match self.cluster(&name) {
            Ok(cluster) => cluster,
            Err(error) => {
                self.state = PanelState::Failed(format!("{error:#}").into());
                cx.notify();
                return;
            }
        };
        self._refresh = cx.spawn_in(window, async move |this, cx| {
            loop {
                let result = cx
                    .background_spawn({
                        let cluster = cluster.clone();
                        async move { cluster.statuses().await }
                    })
                    .await;
                let keep_polling = this.update(cx, |this, cx| {
                    match result {
                        Ok(statuses) => {
                            connections::set_kubernetes(
                                &name,
                                statuses
                                    .iter()
                                    .map(|status| status.context.context.clone())
                                    .collect(),
                                cx,
                            );
                            let statuses = Arc::new(statuses);
                            cx.default_global::<ClusterCache>()
                                .0
                                .insert(name.clone(), statuses.clone());
                            this.state = PanelState::Loaded(statuses);
                        }
                        Err(error) => {
                            this.state = PanelState::Failed(format!("{error:#}").into());
                        }
                    }
                    cx.notify();
                    this.active
                });
                if !matches!(keep_polling, Ok(true)) {
                    break;
                }
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
    }

    fn choose_kubeconfig(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose Kubeconfig".into()),
        });
        self.error = None;
        cx.notify();
        self._action = cx.spawn_in(window, async move |this, cx| {
            let path = match paths.await {
                Ok(Ok(paths)) => paths.and_then(|paths| paths.into_iter().next()),
                Ok(Err(error)) => {
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("{error:#}").into());
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
                Err(_) => None,
            };
            let Some(path) = path else {
                return;
            };
            let result = cx
                .background_spawn({
                    let path = path.clone();
                    async move { kubernetes::read_kubeconfig_contexts(&path) }
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(entries) => this.picker = Some(ContextPicker { path, entries }),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn connect(&mut self, context: String, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(name), Some(picker)) = (self.sandbox_name(cx), self.picker.as_ref()) else {
            return;
        };
        let path = picker.path.clone();
        self.busy = Some(format!("Connecting {context}…").into());
        self.error = None;
        cx.notify();
        self._action = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let connection = PreparedConnection::from_file(&path, &context)?;
                    connection.provision(&Sbx::locate()?, &name).await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}").into());
                }
                this.refresh(window, cx);
                this.restart_explorers(window, cx);
            })
            .log_err();
        });
    }

    fn make_current(&mut self, context: String, window: &mut Window, cx: &mut Context<Self>) {
        self.run_cluster_action(
            format!("Switching to {context}…"),
            move |cluster| async move { cluster.use_context(&context).await },
            window,
            cx,
        );
    }

    fn open_explorer(
        &mut self,
        status: &ClusterStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if status.current {
            window.dispatch_action(OpenKubernetes.boxed_clone(), cx);
            return;
        }
        let context = status.context.context.clone();
        let Some(name) = self.sandbox_name(cx) else {
            return;
        };
        let cluster = match self.cluster(&name) {
            Ok(cluster) => cluster,
            Err(error) => {
                self.error = Some(format!("{error:#}").into());
                cx.notify();
                return;
            }
        };
        self.busy = Some(format!("Switching to {context}…").into());
        cx.notify();
        self._action = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { cluster.use_context(&context).await })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(()) => {
                        this.restart_explorers(window, cx);
                        window.dispatch_action(OpenKubernetes.boxed_clone(), cx);
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                this.refresh(window, cx);
            })
            .log_err();
        });
    }

    fn disconnect(&mut self, context: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.sandbox_name(cx) else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Disconnect {context}?"),
            Some("The cluster is removed from this project's kubeconfig and its token from the sandbox proxy."),
            &["Disconnect", "Cancel"],
            cx,
        );
        self._action = cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| {
                this.busy = Some(format!("Disconnecting {context}…").into());
                this.error = None;
                cx.notify();
            })
            .log_err();
            let result = cx
                .background_spawn(async move {
                    kubernetes::disconnect_context(&Sbx::locate()?, &name, &context).await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}").into());
                }
                this.refresh(window, cx);
                this.restart_explorers(window, cx);
            })
            .log_err();
        });
    }

    fn run_cluster_action<F, Fut>(
        &mut self,
        busy: String,
        action: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(Arc<Cluster>) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let Some(name) = self.sandbox_name(cx) else {
            return;
        };
        let cluster = match self.cluster(&name) {
            Ok(cluster) => cluster,
            Err(error) => {
                self.error = Some(format!("{error:#}").into());
                cx.notify();
                return;
            }
        };
        self.busy = Some(busy.into());
        self.error = None;
        cx.notify();
        self._action = cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(action(cluster)).await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                if let Err(error) = result {
                    this.error = Some(format!("{error:#}").into());
                }
                this.refresh(window, cx);
                this.restart_explorers(window, cx);
            })
            .log_err();
        });
    }

    /// Open explorers read the current context once; after a switch they start over.
    fn restart_explorers(&self, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let views = workspace
            .read(cx)
            .items_of_type::<KubernetesView>(cx)
            .collect::<Vec<_>>();
        for view in views {
            view.update(cx, |view, cx| view.restart(window, cx));
        }
    }

    fn connected_contexts(&self) -> Vec<String> {
        match &self.state {
            PanelState::Loaded(statuses) => statuses
                .iter()
                .map(|status| status.context.context.clone())
                .collect(),
            PanelState::NoSandbox | PanelState::Loading | PanelState::Failed(_) => Vec::new(),
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_sandbox = !matches!(self.state, PanelState::NoSandbox);
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
                    .child(
                        Icon::new(IconName::Kubernetes)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("Kubernetes")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .when_some(self.sandbox_name(cx), |this, name| {
                        this.child(caption(name).truncate())
                    }),
            )
            .when(has_sandbox, |this| {
                this.child(
                    h_flex()
                        .gap_0p5()
                        .child(
                            icon_button("refresh-clusters", IconName::RotateCw, "Refresh")
                                .disabled(self.busy.is_some())
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.refresh(window, cx)),
                                ),
                        )
                        .child(
                            icon_button("add-cluster", IconName::Plus, "Add Cluster")
                                .disabled(self.busy.is_some())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.choose_kubeconfig(window, cx)
                                })),
                        ),
                )
            })
    }

    fn render_picker(&self, picker: &ContextPicker, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = self.connected_contexts();
        let file_name = picker
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| picker.path.to_string_lossy().into_owned());
        v_flex()
            .px_3()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(section_header(
                format!("Contexts in {file_name}"),
                Some(
                    secondary_button("close-context-picker", "Done")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.picker = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                ),
            ))
            .children(picker.entries.iter().enumerate().map(|(index, entry)| {
                let is_connected = connected.contains(&entry.context);
                let context = entry.context.clone();
                h_flex()
                    .py_1()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .child(
                                        Label::new(entry.context.clone())
                                            .size(LabelSize::Small)
                                            .truncate(),
                                    )
                                    .when(entry.current, |this| {
                                        this.child(caption("current").size(LabelSize::XSmall))
                                    }),
                            )
                            .child(
                                caption(entry.server.clone())
                                    .size(LabelSize::XSmall)
                                    .truncate(),
                            )
                            .when_some(entry.problem.clone(), |this, problem| {
                                this.child(
                                    Label::new(problem)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                )
                            }),
                    )
                    .child(if is_connected {
                        badge("Connected", Tone::Ok, cx).into_any_element()
                    } else {
                        secondary_button(("connect-context", index), "Connect")
                            .disabled(entry.problem.is_some() || self.busy.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.connect(context.clone(), window, cx)
                            }))
                            .into_any_element()
                    })
            }))
    }

    fn render_cluster(
        &self,
        index: usize,
        status: &ClusterStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tone = match (&status.error, status.total_nodes) {
            (Some(_), _) => Tone::Error,
            (None, Some(total)) if total != status.ready_nodes => Tone::Warn,
            (None, _) => Tone::Ok,
        };
        let detail = match &status.error {
            Some(error) => error.clone(),
            None => {
                let mut parts = Vec::new();
                if let Some(version) = &status.version {
                    parts.push(version.clone());
                }
                if let Some(total) = status.total_nodes {
                    parts.push(format!("{}/{total} nodes ready", status.ready_nodes));
                }
                parts.push(format!("ns {}", status.context.namespace));
                parts.join(" · ")
            }
        };
        let context = status.context.context.clone();
        let is_current = status.current;
        let busy = self.busy.is_some();
        let open_status = status.clone();
        h_flex()
            .id(("cluster", index))
            .px_3()
            .py_1p5()
            .gap_2()
            .items_start()
            .when(is_current, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .when(!is_current && !busy, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener({
                        let context = context.clone();
                        move |this, _, window, cx| this.make_current(context.clone(), window, cx)
                    }))
            })
            .child(div().pt(px(5.)).child(status_dot(tone, cx)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Label::new(context.clone())
                                    .size(LabelSize::Small)
                                    .weight(if is_current {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .truncate(),
                            )
                            .when(is_current, |this| {
                                this.child(badge("current", Tone::Info, cx))
                            }),
                    )
                    .child(
                        caption(status.context.server.clone())
                            .size(LabelSize::XSmall)
                            .truncate(),
                    )
                    .child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(if status.error.is_some() {
                                Color::Error
                            } else {
                                Color::Muted
                            })
                            .truncate(),
                    ),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        icon_button(
                            ("open-cluster", index),
                            IconName::ArrowUpRight,
                            "Open Explorer",
                        )
                        .disabled(busy)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_explorer(&open_status, window, cx)
                            },
                        )),
                    )
                    .child(
                        icon_button(("remove-cluster", index), IconName::Trash, "Disconnect")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.disconnect(context.clone(), window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let statuses = match &self.state {
            PanelState::NoSandbox => {
                return message("Open a project from the Projects panel to connect its clusters.")
                    .into_any_element();
            }
            PanelState::Loading => return message("Checking clusters…").into_any_element(),
            PanelState::Failed(error) => {
                return v_flex()
                    .p_3()
                    .gap_2()
                    .child(
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        secondary_button("retry-clusters", "Retry")
                            .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                    )
                    .into_any_element();
            }
            PanelState::Loaded(statuses) => statuses.clone(),
        };
        if statuses.is_empty() {
            return v_flex()
                .p_3()
                .gap_3()
                .child(caption(
                    "No clusters are connected to this project. Add one or more contexts from a \
                     kubeconfig: kubectl runs inside the sandbox, the token stays on this Mac.",
                ))
                .child(
                    primary_button("add-first-cluster", "Add Cluster")
                        .disabled(self.busy.is_some())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.choose_kubeconfig(window, cx)),
                        ),
                )
                .into_any_element();
        }
        let has_current = statuses.iter().any(|status| status.current);
        let rows = statuses
            .iter()
            .enumerate()
            .map(|(index, status)| self.render_cluster(index, status, cx))
            .collect::<Vec<_>>();
        v_flex()
            .py_2()
            .child(div().px_3().child(section_header(
                format!("Clusters · {}", statuses.len()),
                None,
            )))
            .children(rows)
            .when(has_current, |this| {
                this.child(
                    div().px_3().pt_3().child(
                        primary_button("open-explorer", "Open Explorer")
                            .disabled(self.busy.is_some())
                            .on_click(|_, window, cx| {
                                window.dispatch_action(OpenKubernetes.boxed_clone(), cx)
                            }),
                    ),
                )
            })
            .into_any_element()
    }
}

fn message(text: &'static str) -> impl IntoElement {
    v_flex().p_3().child(caption(text))
}

impl Render for KubernetesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("sands-kubernetes-panel")
            .key_context("KubernetesPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_header(cx))
            .when_some(self.busy.clone(), |this, busy| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .child(caption(busy).size(LabelSize::XSmall)),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .mx_3()
                        .mt_2()
                        .p_2()
                        .rounded_md()
                        .bg(cx.theme().status().error_background)
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .when_some(self.picker.as_ref(), |this, picker| {
                this.child(self.render_picker(picker, cx))
            })
            .child(
                v_flex()
                    .id("sands-kubernetes-panel-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(self.render_body(cx)),
            )
    }
}

impl Focusable for KubernetesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for KubernetesPanel {}

impl Panel for KubernetesPanel {
    fn persistent_name() -> &'static str {
        "SandsKubernetesPanel"
    }

    fn panel_key() -> &'static str {
        "SandsKubernetesPanel"
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
        Some(IconName::Kubernetes)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Kubernetes")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        ToggleKubernetesPanel.boxed_clone()
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let became_visible = active && !self.active;
        self.active = active;
        if became_visible {
            self.refresh(window, cx);
        }
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}
