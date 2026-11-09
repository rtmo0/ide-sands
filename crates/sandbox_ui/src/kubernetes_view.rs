use std::{collections::BTreeSet, ops::Range, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use chrono::Utc;
use collections::HashSet;
use editor::Editor;
use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable,
    PromptLevel, SharedString, Subscription, Task, WeakEntity, actions, px, relative,
};
use kubernetes::{
    Action as ResourceAction, Category, Cell, Cluster, ContextInfo, DataEntry, Details,
    HelmRelease, KeyValue, ObjectRef, Overview, ResourceKind, Row, Section, Table, TableContext,
    Tone, builtin_kind, builtin_kinds, format_age, shell_quote,
};
use language::BufferEvent;
use sbx::Sbx;
use task::{SpawnInTerminal, TaskId};
use ui::{
    ColumnWidthConfig, ContextMenu, DropdownMenu, Indicator, ListItem, ListItemSpacing,
    Table as UiTable, TableInteractionState, Toggleable as _, Tooltip, prelude::*,
};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{
    OpenOptions, OpenVisible, SaveIntent, Toast, Workspace,
    item::{Item, ItemEvent},
    notifications::{DetachAndPromptErr as _, NotificationId},
};

use crate::connected_sandbox;

actions!(
    kubernetes,
    [
        /// Opens the Kubernetes explorer for the cluster connected to the project's sandbox.
        OpenKubernetes,
        /// Applies the manifest in the active editor to the project's cluster, after a dry run.
        ApplyManifest,
        /// Shows how the manifest in the active editor differs from the live objects.
        DiffManifest
    ]
);

const REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const SIDEBAR_WIDTH: f32 = 196.;
const DETAILS_WIDTH: f32 = 460.;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenKubernetes, window, cx| {
            open_kubernetes(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &ApplyManifest, window, cx| {
            apply_active_manifest(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &DiffManifest, window, cx| {
            diff_active_manifest(workspace, window, cx);
        });
    })
    .detach();
}

fn open_kubernetes(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(sandbox) = connected_sandbox(workspace, cx) else {
        show_toast(workspace, "Open a sandbox project to use Kubernetes.", cx);
        return;
    };
    let existing = workspace
        .active_pane()
        .read(cx)
        .items()
        .find_map(|item| item.downcast::<KubernetesView>());
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak_workspace = workspace.weak_handle();
    let view = cx.new(|cx| KubernetesView::new(weak_workspace, sandbox, window, cx));
    // The explorer needs the width: tables, the details drawer and the sidebar sit side by side.
    workspace
        .right_dock()
        .update(cx, |dock, cx| dock.set_open(false, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

struct KubernetesToast;

fn show_toast(workspace: &mut Workspace, message: impl Into<String>, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<KubernetesToast>(), message.into()).autohide(),
        cx,
    );
}

/// The active editor's file inside the sandbox, saved first so kubectl reads what is on screen.
fn active_manifest(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<(String, Task<Result<()>>)> {
    let item = workspace.active_item(cx)?;
    let project_path = item.project_path(cx)?;
    let path = workspace
        .project()
        .read(cx)
        .absolute_path(&project_path, cx)?;
    let save = workspace.save_active_item(SaveIntent::Save, window, cx);
    Some((path.to_string_lossy().into_owned(), save))
}

fn apply_active_manifest(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(sandbox) = connected_sandbox(workspace, cx) else {
        show_toast(workspace, "Open a sandbox project to apply manifests.", cx);
        return;
    };
    let Some((path, save)) = active_manifest(workspace, window, cx) else {
        show_toast(workspace, "Open a manifest in the editor first.", cx);
        return;
    };
    cx.spawn_in(window, async move |workspace, cx| {
        save.await?;
        let cluster = Arc::new(Cluster::new(Sbx::locate()?, sandbox));
        let preview = cx
            .background_spawn({
                let cluster = cluster.clone();
                let path = path.clone();
                async move { cluster.apply(&path, true).await }
            })
            .await?;
        let answer = workspace
            .update_in(cx, |_, window, cx| {
                window.prompt(
                    PromptLevel::Info,
                    "Apply this manifest to the cluster?",
                    Some(&format!("Server dry run:\n{}", preview.trim())),
                    &["Apply", "Cancel"],
                    cx,
                )
            })?
            .await?;
        if answer != 0 {
            return anyhow::Ok(());
        }
        let result = cx
            .background_spawn(async move { cluster.apply(&path, false).await })
            .await?;
        workspace.update(cx, |workspace, cx| {
            show_toast(workspace, result.trim().to_string(), cx)
        })?;
        anyhow::Ok(())
    })
    .detach_and_prompt_err("Failed to apply the manifest", window, cx, |_, _, _| None);
}

fn diff_active_manifest(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((path, save)) = active_manifest(workspace, window, cx) else {
        show_toast(workspace, "Open a manifest in the editor first.", cx);
        return;
    };
    cx.spawn_in(window, async move |workspace, cx| {
        save.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            run_in_terminal(
                workspace,
                "Kubernetes diff",
                &format!(
                    "kubectl diff --server-side -f {} && echo 'No differences.'",
                    shell_quote(&path)
                ),
                window,
                cx,
            );
        })?;
        anyhow::Ok(())
    })
    .detach_and_prompt_err("Failed to diff the manifest", window, cx, |_, _, _| None);
}

/// Runs a script in a new terminal of the remote project, that is inside the sandbox.
fn run_in_terminal(
    workspace: &mut Workspace,
    label: &str,
    script: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace
        .spawn_in_terminal(
            SpawnInTerminal {
                id: TaskId(format!("kubernetes-{label}")),
                full_label: label.to_string(),
                label: label.to_string(),
                command: Some("bash".to_string()),
                args: vec!["-lc".to_string(), script.to_string()],
                command_label: script.to_string(),
                use_new_terminal: true,
                allow_concurrent_runs: true,
                ..Default::default()
            },
            window,
            cx,
        )
        .detach();
}

#[derive(Clone, Debug, PartialEq)]
enum Page {
    Overview,
    Kind(ResourceKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailsTab {
    Details,
    Yaml,
}

#[derive(Clone, Debug, PartialEq)]
enum Selection {
    Object(ObjectRef),
    HelmRelease { name: String, namespace: String },
}

impl Selection {
    fn title(&self) -> String {
        match self {
            Selection::Object(reference) => reference.name.clone(),
            Selection::HelmRelease { name, .. } => name.clone(),
        }
    }
}

/// Input the drawer asks for before running an action.
#[derive(Clone, Debug, PartialEq)]
enum Prompt {
    Scale(ObjectRef),
    PortForward(ObjectRef),
    SetKey(ObjectRef, Option<String>),
}

struct Loadable<T> {
    value: Option<T>,
    loading: bool,
    error: Option<SharedString>,
}

impl<T> Default for Loadable<T> {
    fn default() -> Self {
        Self {
            value: None,
            loading: false,
            error: None,
        }
    }
}

impl<T> Loadable<T> {
    fn start(&mut self) {
        self.loading = true;
        self.error = None;
    }

    fn finish(&mut self, result: Result<T>) {
        self.loading = false;
        match result {
            Ok(value) => {
                self.value = Some(value);
                self.error = None;
            }
            Err(error) => self.error = Some(format!("{error:#}").into()),
        }
    }
}

struct DrawerContent {
    selection: Selection,
    details: Option<Details>,
    sections: Vec<Section>,
    yaml: Option<String>,
}

/// An OpenLens-style explorer of the cluster connected to one project sandbox.
pub struct KubernetesView {
    workspace: WeakEntity<Workspace>,
    sandbox: String,
    cluster: Arc<Cluster>,
    focus_handle: FocusHandle,
    context: Loadable<Option<ContextInfo>>,
    page: Page,
    namespace: Option<String>,
    namespaces: Vec<String>,
    custom_kinds: Vec<ResourceKind>,
    collapsed: HashSet<Category>,
    search: Entity<InputField>,
    prompt_input: Entity<InputField>,
    overview: Loadable<Overview>,
    table: Loadable<Table>,
    sort: Option<(usize, bool)>,
    selection: Option<Selection>,
    drawer: Loadable<DrawerContent>,
    details_tab: DetailsTab,
    prompt: Option<Prompt>,
    revealed: BTreeSet<String>,
    history: Vec<Selection>,
    status: Option<(SharedString, bool)>,
    table_state: Entity<TableInteractionState>,
    _refresh: Task<()>,
    _load: Task<()>,
    _drawer: Task<()>,
    _action: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl KubernetesView {
    fn new(
        workspace: WeakEntity<Workspace>,
        sandbox: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputField::new(window, cx, "Search by name, namespace or label"));
        let prompt_input = cx.new(|cx| InputField::new(window, cx, ""));
        let view = cx.entity().downgrade();
        let search_editor = search.read(cx).editor().clone();
        let subscriptions = vec![search_editor.subscribe(
            Box::new(move |event, _, cx| {
                if event == ui_input::ErasedEditorEvent::BufferEdited {
                    view.update(cx, |this, cx| this.search_changed(cx))
                        .log_err();
                }
            }),
            window,
            cx,
        )];
        let cluster = Arc::new(match Sbx::locate() {
            Ok(sbx) => Cluster::new(sbx, sandbox.clone()),
            Err(_) => Cluster::new(Sbx::with_executable(PathBuf::from("sbx")), sandbox.clone()),
        });
        let mut view = Self {
            workspace,
            sandbox,
            cluster,
            focus_handle: cx.focus_handle(),
            context: Loadable::default(),
            page: Page::Overview,
            namespace: None,
            namespaces: Vec::new(),
            custom_kinds: Vec::new(),
            collapsed: HashSet::default(),
            search,
            prompt_input,
            overview: Loadable::default(),
            table: Loadable::default(),
            sort: None,
            selection: None,
            drawer: Loadable::default(),
            details_tab: DetailsTab::Details,
            prompt: None,
            revealed: BTreeSet::new(),
            history: Vec::new(),
            status: None,
            table_state: cx.new(|cx| TableInteractionState::new(cx)),
            _refresh: Task::ready(()),
            _load: Task::ready(()),
            _drawer: Task::ready(()),
            _action: Task::ready(()),
            _subscriptions: subscriptions,
        };
        view.collapsed.insert(Category::AccessControl);
        view.collapsed.insert(Category::Custom);
        view.start(window, cx);
        view
    }

    fn search_changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// Loads the context and custom kinds once, then refreshes the visible page periodically.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.context.start();
        let cluster = self.cluster.clone();
        self._refresh = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn({
                    let cluster = cluster.clone();
                    async move {
                        let context = cluster.context().await?;
                        let custom_kinds = match &context {
                            Some(_) => cluster.custom_kinds().await.unwrap_or_default(),
                            None => Vec::new(),
                        };
                        anyhow::Ok((context, custom_kinds))
                    }
                })
                .await;
            let connected = this
                .update_in(cx, |this, window, cx| {
                    let connected = match result {
                        Ok((context, custom_kinds)) => {
                            let connected = context.is_some();
                            this.custom_kinds = custom_kinds;
                            this.context.finish(Ok(context));
                            connected
                        }
                        Err(error) => {
                            this.context.finish(Err(error));
                            false
                        }
                    };
                    if connected {
                        this.reload(window, cx);
                    }
                    cx.notify();
                    connected
                })
                .unwrap_or(false);
            if !connected {
                return;
            }
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let updated = this.update_in(cx, |this, window, cx| {
                    if !this.page_loading() {
                        this.reload(window, cx);
                    }
                    if let Some(selection) = this.selection.clone()
                        && !this.drawer.loading
                        && this.prompt.is_none()
                    {
                        this.load_drawer(selection, false, window, cx);
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        cx.notify();
    }

    fn page_loading(&self) -> bool {
        match self.page {
            Page::Overview => self.overview.loading,
            Page::Kind(_) => self.table.loading,
        }
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Some(context)) = self.context.value.clone() else {
            return;
        };
        let cluster = self.cluster.clone();
        let namespace = self.namespace.clone();
        match self.page.clone() {
            Page::Overview => {
                self.overview.start();
                self._load = cx.spawn_in(window, async move |this, cx| {
                    let result = cx
                        .background_spawn(async move {
                            kubernetes::build_overview(&cluster, context, namespace.as_deref())
                                .await
                        })
                        .await;
                    this.update(cx, |this, cx| {
                        if let Ok(overview) = &result {
                            this.namespaces = overview.namespaces.clone();
                        }
                        this.overview.finish(result);
                        cx.notify();
                    })
                    .log_err();
                });
            }
            Page::Kind(kind) => {
                self.table.start();
                let custom_kinds = self.custom_kinds.clone();
                self._load = cx.spawn_in(window, async move |this, cx| {
                    let result = cx
                        .background_spawn(async move {
                            load_table(&cluster, &kind, namespace.as_deref(), custom_kinds).await
                        })
                        .await;
                    this.update(cx, |this, cx| {
                        this.table.finish(result);
                        cx.notify();
                    })
                    .log_err();
                });
            }
        }
        cx.notify();
    }

    fn show(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != page {
            self.page = page;
            self.table = Loadable::default();
            self.sort = None;
        }
        self.reload(window, cx);
    }

    fn set_namespace(
        &mut self,
        namespace: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.namespace = namespace;
        self.table = Loadable::default();
        self.reload(window, cx);
    }

    fn select(&mut self, selection: Selection, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(current) = self.selection.take()
            && current != selection
        {
            self.history.push(current);
        }
        self.prompt = None;
        self.revealed.clear();
        self.details_tab = DetailsTab::Details;
        self.drawer = Loadable::default();
        self.load_drawer(selection, true, window, cx);
    }

    fn open_link(&mut self, reference: ObjectRef, window: &mut Window, cx: &mut Context<Self>) {
        if reference.name.is_empty() {
            self.show(Page::Kind(reference.kind), window, cx);
        } else {
            self.select(Selection::Object(reference), window, cx);
        }
    }

    fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.history.pop() {
            self.prompt = None;
            self.drawer = Loadable::default();
            self.load_drawer(previous, true, window, cx);
        }
    }

    fn close_drawer(&mut self, cx: &mut Context<Self>) {
        self.selection = None;
        self.history.clear();
        self.prompt = None;
        self.drawer = Loadable::default();
        cx.notify();
    }

    fn load_drawer(
        &mut self,
        selection: Selection,
        with_yaml: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selection = Some(selection.clone());
        self.drawer.start();
        let cluster = self.cluster.clone();
        let custom_kinds = self.custom_kinds.clone();
        let keep_yaml = self
            .drawer
            .value
            .as_ref()
            .filter(|content| content.selection == selection)
            .and_then(|content| content.yaml.clone());
        let load_yaml = with_yaml || self.details_tab == DetailsTab::Yaml;
        self._drawer = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    match &selection {
                        Selection::Object(reference) => {
                            let details =
                                kubernetes::build_details(&cluster, reference, &custom_kinds)
                                    .await?;
                            let yaml = if load_yaml {
                                Some(cluster.yaml(reference).await?)
                            } else {
                                keep_yaml
                            };
                            anyhow::Ok(DrawerContent {
                                selection: selection.clone(),
                                details: Some(details),
                                sections: Vec::new(),
                                yaml,
                            })
                        }
                        Selection::HelmRelease { name, namespace } => {
                            let sections =
                                kubernetes::build_helm_details(&cluster, name, namespace).await?;
                            anyhow::Ok(DrawerContent {
                                selection: selection.clone(),
                                details: None,
                                sections,
                                yaml: None,
                            })
                        }
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.drawer.finish(result);
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn set_status(
        &mut self,
        message: impl Into<SharedString>,
        error: bool,
        cx: &mut Context<Self>,
    ) {
        self.status = Some((message.into(), error));
        cx.notify();
    }

    /// Runs a change against the cluster, then refreshes the page and the drawer.
    fn run_change<F, Future>(
        &mut self,
        done: &'static str,
        change: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(Arc<Cluster>) -> Future + Send + 'static,
        Future: std::future::Future<Output = Result<String>> + Send + 'static,
    {
        let cluster = self.cluster.clone();
        self.set_status(format!("{done}…"), false, cx);
        self._action = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { change(cluster).await })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(message) if message.trim().is_empty() => this.set_status(done, false, cx),
                    Ok(message) => this.set_status(message.trim().to_string(), false, cx),
                    Err(error) => this.set_status(format!("{error:#}"), true, cx),
                }
                this.reload(window, cx);
                if let Some(selection) = this.selection.clone() {
                    this.load_drawer(selection, true, window, cx);
                }
            })
            .log_err();
        });
    }

    fn confirm_then(
        &mut self,
        question: String,
        detail: String,
        confirm: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &question,
            Some(&detail),
            &[confirm, "Cancel"],
            cx,
        );
        self._action = cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| then(this, window, cx))
                .log_err();
        });
    }

    fn terminal(
        &mut self,
        label: String,
        script: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                run_in_terminal(workspace, &label, &script, window, cx)
            });
        }
    }

    fn run_action(
        &mut self,
        action: ResourceAction,
        reference: ObjectRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = reference
            .kubectl_arguments()
            .iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ");
        match action {
            ResourceAction::Logs => {
                let all = if reference.kind.id == "pods" {
                    "--all-containers"
                } else {
                    "--all-containers --prefix --max-log-requests=20"
                };
                self.terminal(
                    format!("Logs: {}", reference.name),
                    format!("kubectl logs --follow --tail=500 {all} {target}"),
                    window,
                    cx,
                );
            }
            ResourceAction::Shell => self.open_shell(&reference, None, window, cx),
            ResourceAction::PortForward => {
                self.prompt = Some(Prompt::PortForward(reference));
                self.prompt_input.update(cx, |input, cx| input.set_text("8080:80", window, cx));
                cx.notify();
            }
            ResourceAction::Scale => {
                self.prompt = Some(Prompt::Scale(reference));
                self.prompt_input.update(cx, |input, cx| input.set_text("1", window, cx));
                cx.notify();
            }
            ResourceAction::Restart => self.confirm_then(
                format!("Restart {}?", reference.display()),
                "Pods are replaced one by one following the rollout strategy.".to_string(),
                "Restart",
                window,
                cx,
                move |this, window, cx| {
                    this.run_change(
                        "Restarted",
                        move |cluster: Arc<Cluster>| async move { cluster.restart(&reference).await.map(|_| String::new()) },
                        window,
                        cx,
                    )
                },
            ),
            ResourceAction::Delete => self.confirm_then(
                format!("Delete {}?", reference.display()),
                "This changes the connected Kubernetes cluster and cannot be undone.".to_string(),
                "Delete",
                window,
                cx,
                move |this, window, cx| {
                    this.close_drawer(cx);
                    this.run_change(
                        "Deleted",
                        move |cluster: Arc<Cluster>| async move { cluster.delete(&reference).await.map(|_| String::new()) },
                        window,
                        cx,
                    )
                },
            ),
            ResourceAction::Trigger => self.run_change(
                "Job started",
                move |cluster: Arc<Cluster>| async move { {
                    cluster
                        .trigger(&reference)
                        .await
                        .map(|job| format!("Started job {job}"))
                } },
                window,
                cx,
            ),
            ResourceAction::Suspend | ResourceAction::Resume => {
                let suspend = action == ResourceAction::Suspend;
                self.run_change(
                    if suspend { "Suspended" } else { "Resumed" },
                    move |cluster: Arc<Cluster>| async move { {
                        cluster.set_suspended(&reference, suspend).await.map(|_| String::new())
                    } },
                    window,
                    cx,
                )
            }
            ResourceAction::Cordon | ResourceAction::Uncordon => {
                let cordon = action == ResourceAction::Cordon;
                self.run_change(
                    if cordon { "Cordoned" } else { "Uncordoned" },
                    move |cluster: Arc<Cluster>| async move { {
                        cluster.set_cordoned(&reference.name, cordon).await.map(|_| String::new())
                    } },
                    window,
                    cx,
                )
            }
            ResourceAction::Drain => self.confirm_then(
                format!("Drain node {}?", reference.name),
                "The node is cordoned and its pods are evicted, respecting PodDisruptionBudgets."
                    .to_string(),
                "Drain",
                window,
                cx,
                move |this, window, cx| {
                    this.run_change(
                        "Drained",
                        move |cluster: Arc<Cluster>| async move { cluster.drain(&reference.name).await },
                        window,
                        cx,
                    )
                },
            ),
            ResourceAction::EditYaml => self.edit_yaml(reference, window, cx),
        }
    }

    fn open_shell(
        &mut self,
        pod: &ObjectRef,
        container: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut command = format!("kubectl exec -it {}", shell_quote(&pod.name));
        if let Some(namespace) = &pod.namespace {
            command.push_str(&format!(" --namespace {}", shell_quote(namespace)));
        }
        if let Some(container) = container {
            command.push_str(&format!(" --container {}", shell_quote(container)));
        }
        command.push_str(
            " -- sh -c 'command -v bash >/dev/null && exec bash; command -v ash >/dev/null && exec ash; exec sh'",
        );
        self.terminal(format!("Shell: {}", pod.name), command, window, cx);
    }

    fn container_logs(
        &mut self,
        pod: &ObjectRef,
        container: &str,
        previous: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut command = format!(
            "kubectl logs {} --container {} --tail=500",
            shell_quote(&pod.name),
            shell_quote(container)
        );
        if let Some(namespace) = &pod.namespace {
            command.push_str(&format!(" --namespace {}", shell_quote(namespace)));
        }
        command.push_str(if previous { " --previous" } else { " --follow" });
        self.terminal(
            format!("Logs: {}/{container}", pod.name),
            command,
            window,
            cx,
        );
    }

    /// Forwards `local:remote` from the resource and publishes the local port on the host, so
    /// the browser on the host can reach it.
    fn port_forward(
        &mut self,
        reference: ObjectRef,
        mapping: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parsed = match mapping.split_once(':') {
            Some((local, remote)) => local
                .trim()
                .parse::<u16>()
                .ok()
                .zip(remote.trim().parse::<u16>().ok()),
            None => mapping.trim().parse::<u16>().ok().map(|port| (port, port)),
        };
        let Some((local, remote)) = parsed else {
            self.set_status("Enter a port mapping such as 8080:80", true, cx);
            return;
        };
        let target = reference
            .kubectl_arguments()
            .iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ");
        self.terminal(
            format!("Port forward: {} {local}:{remote}", reference.name),
            format!("kubectl port-forward --address 0.0.0.0 {target} {local}:{remote}"),
            window,
            cx,
        );
        let sandbox = self.sandbox.clone();
        self._action = cx.spawn_in(window, async move |this, cx| {
            let published = cx
                .background_spawn(async move {
                    let sbx = Sbx::locate()?;
                    let existing = sbx.ports(&sandbox).await?;
                    if let Some(port) = existing.into_iter().find(|port| port.sandbox_port == local)
                    {
                        return anyhow::Ok(port);
                    }
                    Ok(sbx.publish_port(&sandbox, local, None).await?)
                })
                .await;
            this.update(cx, |this, cx| match published {
                Ok(port) => this.set_status(
                    format!(
                        "Forwarding {}:{remote} → http://127.0.0.1:{} on the host",
                        reference.name, port.host_port
                    ),
                    false,
                    cx,
                ),
                Err(error) => {
                    this.set_status(format!("Publishing the port failed: {error:#}"), true, cx)
                }
            })
            .log_err();
        });
    }

    /// Writes the live object to a file in the sandbox and opens it. Saving the file replaces the
    /// object; the file keeps its resourceVersion, so a concurrent change is reported, not lost.
    fn edit_yaml(&mut self, reference: ObjectRef, window: &mut Window, cx: &mut Context<Self>) {
        let cluster = self.cluster.clone();
        let workspace = self.workspace.clone();
        self.set_status(format!("Opening {}…", reference.display()), false, cx);
        self._action = cx.spawn_in(window, async move |this, cx| {
            let path = cx
                .background_spawn({
                    let cluster = cluster.clone();
                    async move { cluster.export_for_editing(&reference).await }
                })
                .await;
            let path = match path {
                Ok(path) => path,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.set_status(format!("{error:#}"), true, cx)
                    })
                    .log_err();
                    return;
                }
            };
            let opened = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from(&path),
                    OpenOptions {
                        visible: Some(OpenVisible::None),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            });
            let item = match opened {
                Ok(task) => task.await,
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| match item {
                Ok(item) => {
                    let buffer = item
                        .downcast::<Editor>()
                        .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton());
                    if let Some(buffer) = buffer {
                        let subscription = cx.subscribe(&buffer, move |this, _, event, cx| {
                            if matches!(event, BufferEvent::Saved) {
                                this.replace_from_file(path.clone(), cx);
                            }
                        });
                        this._subscriptions.push(subscription);
                        this.set_status("Saving the file applies it to the cluster.", false, cx);
                    }
                }
                Err(error) => this.set_status(format!("{error:#}"), true, cx),
            })
            .log_err();
        });
    }

    fn replace_from_file(&mut self, path: String, cx: &mut Context<Self>) {
        let cluster = self.cluster.clone();
        self.set_status("Applying the saved manifest…", false, cx);
        self._action = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { cluster.replace(&path).await })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(output) => this.set_status(output.trim().to_string(), false, cx),
                Err(error) => this.set_status(
                    format!(
                        "{error:#}. Reopen the object with Edit YAML if it changed on the server."
                    ),
                    true,
                    cx,
                ),
            })
            .log_err();
        });
    }

    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let text = self.prompt_input.read(cx).text(cx);
        match prompt {
            Prompt::Scale(reference) => match text.trim().parse::<u32>() {
                Ok(replicas) => self.run_change(
                    "Scaled",
                    move |cluster: Arc<Cluster>| async move {
                        cluster
                            .scale(&reference, replicas)
                            .await
                            .map(|_| String::new())
                    },
                    window,
                    cx,
                ),
                Err(_) => {
                    self.prompt = Some(Prompt::Scale(reference));
                    self.set_status("Replicas must be a whole number", true, cx);
                }
            },
            Prompt::PortForward(reference) => self.port_forward(reference, &text, window, cx),
            Prompt::SetKey(reference, key) => {
                let (key, value) = match key {
                    Some(key) => (key, text),
                    None => match text.split_once('=') {
                        Some((key, value)) => (key.trim().to_string(), value.to_string()),
                        None => {
                            self.prompt = Some(Prompt::SetKey(reference, None));
                            self.set_status("Enter KEY=value", true, cx);
                            return;
                        }
                    },
                };
                self.run_change(
                    "Saved",
                    move |cluster: Arc<Cluster>| async move {
                        {
                            cluster
                                .set_data_key(&reference, &key, Some(&value))
                                .await
                                .map(|_| String::new())
                        }
                    },
                    window,
                    cx,
                );
            }
        }
        cx.notify();
    }

    fn sorted_rows(&self, cx: &App) -> Vec<Row> {
        let Some(table) = &self.table.value else {
            return Vec::new();
        };
        let query = self.search.read(cx).text(cx).trim().to_lowercase();
        let mut rows = table
            .rows
            .iter()
            .filter(|row| {
                query.is_empty()
                    || row.search.contains(&query)
                    || row
                        .cells
                        .iter()
                        .any(|cell| cell.text.to_lowercase().contains(&query))
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some((column, ascending)) = self.sort {
            rows.sort_by(|a, b| {
                let ordering = match (a.cells.get(column), b.cells.get(column)) {
                    (Some(a), Some(b)) => match (a.sort, b.sort) {
                        (Some(x), Some(y)) => {
                            x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
                        }
                        _ => a.text.to_lowercase().cmp(&b.text.to_lowercase()),
                    },
                    _ => std::cmp::Ordering::Equal,
                };
                if ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            });
        }
        rows
    }

    fn visible_columns(&self) -> Vec<usize> {
        let Some(table) = &self.table.value else {
            return Vec::new();
        };
        table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| !(column.namespace_column && self.namespace.is_some()))
            .map(|(index, _)| index)
            .collect()
    }
}

async fn load_table(
    cluster: &Cluster,
    kind: &ResourceKind,
    namespace: Option<&str>,
    custom_kinds: Vec<ResourceKind>,
) -> Result<Table> {
    if kind.is_helm_release() {
        return Ok(helm_table(&cluster.helm_releases(namespace).await?, kind));
    }
    let mut requests = vec![(kind.clone(), namespace.map(str::to_string))];
    let pods_kind = builtin_kind("pods").context("the pods kind is missing")?;
    if kind.id == "nodes" {
        requests.push((pods_kind.clone(), None));
    }
    let mut listings = cluster.list_batch(&requests).await?.into_iter();
    let listing = listings.next().context("kubectl returned no listing")??;
    let mut context = TableContext {
        custom_kinds,
        ..Default::default()
    };
    match kind.id.as_str() {
        "pods" => context.pod_metrics = cluster.pod_metrics(namespace).await,
        "nodes" => {
            if let Some(Ok(pods)) = listings.next() {
                let mut per_node = std::collections::HashMap::new();
                for pod in &pods.items {
                    if let Some(node) = pod.pointer("/spec/nodeName").and_then(|node| node.as_str())
                    {
                        *per_node.entry(node.to_string()).or_insert(0) += 1;
                    }
                }
                context.pods_per_node = Some(per_node);
            }
            context.node_metrics = cluster.node_metrics().await;
        }
        _ => {}
    }
    let mut table = kubernetes::build_table(kind, &listing.items, &context, Utc::now());
    table.notice = listing.notice;
    Ok(table)
}

fn helm_table(releases: &[HelmRelease], kind: &ResourceKind) -> Table {
    let columns = [
        ("name", "Name", false, false),
        ("namespace", "Namespace", false, true),
        ("chart", "Chart", false, false),
        ("revision", "Revision", true, false),
        ("appVersion", "App Version", false, false),
        ("updated", "Updated", false, false),
        ("status", "Status", false, false),
    ]
    .into_iter()
    .map(
        |(id, title, align_right, namespace_column)| kubernetes::Column {
            id,
            title,
            align_right,
            namespace_column,
        },
    )
    .collect();
    let rows = releases
        .iter()
        .map(|release| {
            let tone = match release.status.as_str() {
                "deployed" => Tone::Ok,
                "failed" => Tone::Error,
                _ => Tone::Warn,
            };
            Row {
                key: format!("{}/{}", release.namespace, release.name),
                reference: ObjectRef::new(
                    kind.clone(),
                    release.name.clone(),
                    Some(release.namespace.clone()),
                ),
                cells: vec![
                    Cell::text(release.name.clone()),
                    Cell::text(release.namespace.clone()),
                    Cell::text(release.chart.clone()),
                    Cell {
                        sort: release.revision.parse().ok(),
                        ..Cell::text(release.revision.clone())
                    },
                    Cell::text(release.app_version.clone()),
                    Cell::text(
                        release
                            .updated
                            .split('.')
                            .next()
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    Cell {
                        tone: Some(tone),
                        badge: true,
                        ..Cell::text(release.status.clone())
                    },
                ],
                actions: Vec::new(),
                search: format!("{} {} {}", release.name, release.namespace, release.chart)
                    .to_lowercase(),
            }
        })
        .collect();
    Table {
        columns,
        rows,
        notice: None,
    }
}

/// Column widths sized for typical content, so names and statuses are not cut off; the table
/// scrolls horizontally when they do not fit.
fn column_width(id: &str) -> f32 {
    match id {
        "name" => 300.,
        "namespace" => 150.,
        "message" => 420.,
        "object" | "owner" | "endpoints" | "used" | "subjects" | "hosts" | "selector" => 220.,
        "node" | "chart" | "image" | "claim" | "provisioner" | "reference" | "targets" => 180.,
        "status" | "conditions" | "type" | "reason" | "role" | "group" | "storageClass" => 140.,
        "cpu" | "memory" | "ports" | "clusterIP" | "external" | "ip" | "version" => 130.,
        "containers" | "pods" | "keys" | "schedule" | "updated" | "lastSeen" | "lastSchedule" => {
            110.
        }
        _ => 90.,
    }
}

fn tone_color(tone: Tone) -> Color {
    match tone {
        Tone::Ok => Color::Success,
        Tone::Warn => Color::Warning,
        Tone::Error => Color::Error,
        Tone::Info => Color::Accent,
        Tone::Muted => Color::Muted,
    }
}

fn badge(text: impl Into<SharedString>, tone: Tone, cx: &App) -> impl IntoElement {
    let status = cx.theme().status();
    let (background, border) = match tone {
        Tone::Ok => (status.success_background, status.success_border),
        Tone::Warn => (status.warning_background, status.warning_border),
        Tone::Error => (status.error_background, status.error_border),
        Tone::Info => (status.info_background, status.info_border),
        Tone::Muted => (
            cx.theme().colors().element_background,
            cx.theme().colors().border_variant,
        ),
    };
    div()
        .flex_none()
        .px_1p5()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(background)
        .child(
            Label::new(text)
                .size(LabelSize::XSmall)
                .color(tone_color(tone))
                .single_line(),
        )
}

fn bar(fraction: f64, color: gpui::Hsla, cx: &App) -> impl IntoElement {
    div()
        .h_1()
        .w_full()
        .rounded_full()
        .bg(cx.theme().colors().element_background)
        .child(
            div()
                .h_full()
                .rounded_full()
                .bg(color)
                .w(relative(fraction.clamp(0.0, 1.0) as f32)),
        )
}

fn tone_hsla(tone: Tone, cx: &App) -> gpui::Hsla {
    let status = cx.theme().status();
    match tone {
        Tone::Ok => status.success,
        Tone::Warn => status.warning,
        Tone::Error => status.error,
        Tone::Info => status.info,
        Tone::Muted => cx.theme().colors().text_muted,
    }
}

fn muted(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

fn card(cx: &App) -> gpui::Div {
    v_flex()
        .p_3()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().panel_background)
}

fn section_title(title: impl Into<SharedString>) -> impl IntoElement {
    Label::new(title)
        .size(LabelSize::Small)
        .weight(gpui::FontWeight::SEMIBOLD)
}

impl KubernetesView {
    fn render_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let context = self.context.value.clone().flatten();
        let this = cx.entity().downgrade();
        let namespaces = self.namespaces.clone();
        let namespace_menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            let all = this.clone();
            menu = menu.entry("All namespaces", None, move |window, cx| {
                all.update(cx, |view, cx| view.set_namespace(None, window, cx))
                    .log_err();
            });
            menu = menu.separator();
            for namespace in &namespaces {
                let view = this.clone();
                let namespace = namespace.clone();
                menu = menu.entry(namespace.clone(), None, move |window, cx| {
                    let namespace = namespace.clone();
                    view.update(cx, |view, cx| {
                        view.set_namespace(Some(namespace), window, cx)
                    })
                    .log_err();
                });
            }
            menu
        });
        let title = match &self.page {
            Page::Overview => "Cluster Overview".to_string(),
            Page::Kind(kind) => kind.title.clone(),
        };
        h_flex()
            .px_3()
            .py_2()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().toolbar_background)
            .child(
                h_flex()
                    .gap_2()
                    .min_w_0()
                    .flex_1()
                    .child(Icon::new(IconName::Blocks).color(Color::Accent))
                    .child(Label::new(title).weight(gpui::FontWeight::SEMIBOLD))
                    .when_some(context, |this, context| {
                        this.child(
                            Label::new(format!("{} · {}", context.context, context.server))
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .when(matches!(self.page, Page::Kind(_)), |this| {
                this.child(div().w(px(260.)).child(self.search.clone()))
            })
            .child(
                DropdownMenu::new(
                    "kubernetes-namespace",
                    self.namespace
                        .clone()
                        .unwrap_or_else(|| "All namespaces".to_string()),
                    namespace_menu,
                )
                .trigger_size(ButtonSize::Compact),
            )
            .child(
                IconButton::new("kubernetes-refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut items = v_flex().p_1p5().gap_px().child(
            ListItem::new("kubernetes-overview")
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(self.page == Page::Overview)
                .start_slot(
                    Icon::new(IconName::Blocks)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new("Overview").size(LabelSize::Small))
                .on_click(cx.listener(|this, _, window, cx| this.show(Page::Overview, window, cx))),
        );
        for category in Category::ALL {
            let collapsed = self.collapsed.contains(&category);
            let mut kinds = builtin_kinds()
                .iter()
                .filter(|kind| kind.category == category)
                .cloned()
                .collect::<Vec<_>>();
            if category == Category::Custom {
                kinds.extend(self.custom_kinds.iter().cloned());
            }
            items = items.child(
                ListItem::new(SharedString::from(format!("category-{}", category.title())))
                    .spacing(ListItemSpacing::Dense)
                    .start_slot(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(category.title().to_uppercase())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed.remove(&category) {
                            this.collapsed.insert(category);
                        }
                        cx.notify();
                    })),
            );
            if collapsed {
                continue;
            }
            for kind in kinds {
                let selected = self.page == Page::Kind(kind.clone());
                let title = kind.title.clone();
                items = items.child(
                    ListItem::new(SharedString::from(format!("kind-{}", kind.id)))
                        .spacing(ListItemSpacing::Dense)
                        .indent_level(1)
                        .toggle_state(selected)
                        .child(Label::new(title).size(LabelSize::Small).truncate())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.show(Page::Kind(kind.clone()), window, cx)
                        })),
                );
            }
        }
        v_flex()
            .id("kubernetes-sidebar")
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().panel_background)
            .child(items)
    }

    fn render_overview(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(overview) = &self.overview.value else {
            return self.render_placeholder(
                &self.overview.error,
                "Loading the cluster overview…",
                cx,
            );
        };
        let scope = match &overview.namespace {
            Some(namespace) => format!("namespace {namespace}"),
            None => "cluster".to_string(),
        };
        let resource_card = |title: String,
                             totals: &kubernetes::ResourceTotals,
                             format: fn(f64) -> String,
                             cx: &App| {
            let allocatable = totals.allocatable.max(f64::EPSILON);
            let rows = [
                ("Usage", totals.usage, Tone::Info),
                ("Requests", Some(totals.requests), Tone::Warn),
                ("Limits", Some(totals.limits), Tone::Error),
            ];
            card(cx)
                .flex_1()
                .min_w(px(260.))
                .child(section_title(title))
                .children(rows.into_iter().map(|(label, value, tone)| {
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex().justify_between().child(muted(label)).child(
                                Label::new(match value {
                                    Some(value) => format!(
                                        "{} ({}%)",
                                        format(value),
                                        ((value / allocatable) * 100.0).round()
                                    ),
                                    None => "—".to_string(),
                                })
                                .size(LabelSize::Small),
                            ),
                        )
                        .child(bar(
                            value.unwrap_or_default() / allocatable,
                            tone_hsla(tone, cx),
                            cx,
                        ))
                }))
                .child(muted(format!(
                    "of {} allocatable",
                    format(totals.allocatable)
                )))
        };
        let stat = |title: &'static str,
                    value: String,
                    detail: String,
                    tone: Option<Tone>,
                    link: Option<&'static str>,
                    cx: &mut Context<Self>| {
            card(cx)
                .id(title)
                .flex_1()
                .min_w(px(160.))
                .when(link.is_some(), |this| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().element_hover))
                })
                .when_some(link, |this, kind_id| {
                    this.on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(kind) = builtin_kind(kind_id) {
                            this.show(Page::Kind(kind.clone()), window, cx);
                        }
                    }))
                })
                .child(muted(title))
                .child(
                    h_flex()
                        .gap_1()
                        .items_baseline()
                        .child(Headline::new(value).size(HeadlineSize::Medium))
                        .child(muted(detail)),
                )
                .when_some(tone, |this, tone| {
                    this.child(bar(1.0, tone_hsla(tone, cx), cx))
                })
        };
        let nodes_tone = if overview.nodes_ready < overview.nodes_total {
            Tone::Error
        } else {
            Tone::Ok
        };
        let workloads = overview
            .workloads
            .iter()
            .map(|summary| {
                let kind = summary.kind.clone();
                let total = summary.total.max(1) as f64;
                card(cx)
                    .id(SharedString::from(format!("workload-{}", summary.kind.id)))
                    .flex_1()
                    .min_w(px(150.))
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.show(Page::Kind(kind.clone()), window, cx)
                    }))
                    .child(muted(summary.kind.title.clone()))
                    .child(Headline::new(summary.total.to_string()).size(HeadlineSize::Medium))
                    .child(
                        h_flex()
                            .h_1()
                            .w_full()
                            .rounded_full()
                            .overflow_hidden()
                            .bg(cx.theme().colors().element_background)
                            .children(
                                [
                                    (summary.ok, Tone::Ok),
                                    (summary.warn, Tone::Warn),
                                    (summary.error, Tone::Error),
                                ]
                                .into_iter()
                                .filter(|(count, _)| *count > 0)
                                .map(|(count, tone)| {
                                    div()
                                        .h_full()
                                        .bg(tone_hsla(tone, cx))
                                        .w(relative((count as f64 / total) as f32))
                                }),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .when(summary.ok > 0, |this| {
                                this.child(
                                    Label::new(format!("{} ok", summary.ok))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Success),
                                )
                            })
                            .when(summary.warn > 0, |this| {
                                this.child(
                                    Label::new(format!("{} pending", summary.warn))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                )
                            })
                            .when(summary.error > 0, |this| {
                                this.child(
                                    Label::new(format!("{} failing", summary.error))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Error),
                                )
                            }),
                    )
            })
            .collect::<Vec<_>>();
        let now = Utc::now();
        let warnings = overview
            .warnings
            .iter()
            .enumerate()
            .map(|(index, warning)| {
                let link = warning.link.clone();
                h_flex()
                    .id(("warning", index))
                    .px_2()
                    .py_1()
                    .gap_3()
                    .rounded_sm()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .when_some(link, |this, link| {
                        this.cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_link(link.clone(), window, cx)
                            }))
                    })
                    .child(h_flex().w(px(150.)).flex_none().child(badge(
                        warning.reason.clone(),
                        Tone::Warn,
                        cx,
                    )))
                    .child(
                        div().w(px(260.)).flex_none().child(
                            Label::new(warning.object.clone())
                                .size(LabelSize::Small)
                                .color(Color::Accent)
                                .truncate(),
                        ),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(warning.message.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    )
                    .child(muted(format!("×{}", warning.count)))
                    .child(
                        div().w(px(56.)).flex_none().child(muted(
                            warning
                                .time
                                .map(|time| format_age(time, now))
                                .unwrap_or_default(),
                        )),
                    )
            })
            .collect::<Vec<_>>();
        v_flex()
            .id("kubernetes-overview")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            .children(overview.notices.iter().map(|notice| {
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Info)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(muted(notice.clone()))
            }))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_3()
                    .child(stat(
                        "Nodes",
                        overview.nodes_ready.to_string(),
                        format!("/ {} ready", overview.nodes_total),
                        Some(nodes_tone),
                        Some("nodes"),
                        cx,
                    ))
                    .child(stat(
                        "Pods",
                        overview.pods_active.to_string(),
                        format!("/ {} allocatable", overview.pods_capacity),
                        None,
                        Some("pods"),
                        cx,
                    ))
                    .child(stat(
                        "Version",
                        overview.version.clone(),
                        String::new(),
                        None,
                        None,
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_3()
                    .child(resource_card(
                        format!("CPU · {scope}"),
                        &overview.cpu,
                        kubernetes::format_cpu,
                        cx,
                    ))
                    .child(resource_card(
                        format!("Memory · {scope}"),
                        &overview.memory,
                        kubernetes::format_bytes,
                        cx,
                    )),
            )
            .child(section_title("Workloads"))
            .child(h_flex().flex_wrap().gap_3().children(workloads))
            .child(section_title(format!(
                "Warnings ({})",
                overview.warnings.len()
            )))
            .child(
                card(cx)
                    .p_1()
                    .gap_0()
                    .when(overview.warnings.is_empty(), |this| {
                        this.child(div().p_2().child(muted("No warning events.")))
                    })
                    .children(warnings),
            )
            .into_any_element()
    }

    fn render_placeholder(
        &self,
        error: &Option<SharedString>,
        loading: &'static str,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(match error {
                Some(error) => Label::new(error.clone())
                    .color(Color::Error)
                    .into_any_element(),
                None => muted(loading).into_any_element(),
            })
            .into_any_element()
    }

    fn render_table(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(table) = &self.table.value else {
            return self.render_placeholder(&self.table.error, "Loading…", cx);
        };
        let columns = self.visible_columns();
        if columns.is_empty() {
            return self.render_placeholder(&None, "Nothing to show.", cx);
        }
        let rows = self.sorted_rows(cx);
        let row_count = rows.len();
        let headers = columns
            .iter()
            .filter_map(|index| {
                table
                    .columns
                    .get(*index)
                    .map(|column| (*index, column.clone()))
            })
            .map(|(index, column)| {
                let sorted = self
                    .sort
                    .filter(|(sorted, _)| *sorted == index)
                    .map(|(_, ascending)| ascending);
                h_flex()
                    .id(SharedString::from(format!("header-{}", column.id)))
                    .gap_1()
                    .cursor_pointer()
                    .when(column.align_right, |this| this.justify_end())
                    .child(
                        Label::new(column.title)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when_some(sorted, |this, ascending| {
                        this.child(
                            Icon::new(if ascending {
                                IconName::ArrowUp
                            } else {
                                IconName::ArrowDown
                            })
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sort = match this.sort {
                            Some((sorted, true)) if sorted == index => Some((index, false)),
                            Some((sorted, false)) if sorted == index => None,
                            _ => Some((index, true)),
                        };
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let selected_key = match &self.selection {
            Some(Selection::Object(reference)) => Some(format!(
                "{}/{}",
                reference.namespace.as_deref().unwrap_or_default(),
                reference.name
            )),
            Some(Selection::HelmRelease { name, namespace }) => Some(format!("{namespace}/{name}")),
            None => None,
        };
        let widths = columns
            .iter()
            .filter_map(|index| table.columns.get(*index))
            .map(|column| column_width(column.id))
            .collect::<Vec<_>>();
        let total_width = widths.iter().sum::<f32>();
        let mut width_config = ColumnWidthConfig::explicit(widths.into_iter().map(px).collect());
        if let ColumnWidthConfig::Static { table_width, .. } = &mut width_config {
            *table_width = Some(px(total_width).into());
        }
        let rows = Arc::new(rows);
        let row_columns = Arc::new(columns.clone());
        let view = cx.entity().downgrade();
        let map_rows = rows.clone();
        let map_view = view.clone();
        v_flex()
            .size_full()
            .when_some(table.notice.clone(), |this, notice| {
                this.child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .child(
                            Icon::new(IconName::Info)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(muted(notice)),
                )
            })
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .justify_between()
                    .child(muted(format!("{row_count} items")))
                    .when(self.table.loading, |this| this.child(muted("Refreshing…"))),
            )
            .child(
                div().flex_1().min_h_0().child(
                    UiTable::new(columns.len())
                        .interactable(&self.table_state)
                        .width_config(width_config)
                        .striped()
                        .header(headers)
                        .empty_table_callback(|_, _| {
                            div()
                                .p_3()
                                .child(muted("No matching objects."))
                                .into_any_element()
                        })
                        .map_row(move |(index, row), _window, cx| {
                            let selected = map_rows
                                .get(index)
                                .is_some_and(|row| Some(&row.key) == selected_key.as_ref());
                            let Some(target) = map_rows.get(index).cloned() else {
                                return row.into_any_element();
                            };
                            let view = map_view.clone();
                            row.cursor_pointer()
                                .when(selected, |row| row.bg(cx.theme().colors().element_selected))
                                .on_click(move |_, window, cx| {
                                    view.update(cx, |this, cx| {
                                        let selection = if target.reference.kind.is_helm_release() {
                                            Selection::HelmRelease {
                                                name: target.reference.name.clone(),
                                                namespace: target
                                                    .reference
                                                    .namespace
                                                    .clone()
                                                    .unwrap_or_default(),
                                            }
                                        } else {
                                            Selection::Object(target.reference.clone())
                                        };
                                        this.select(selection, window, cx);
                                    })
                                    .log_err();
                                })
                                .into_any_element()
                        })
                        .uniform_list("kubernetes-table", row_count, {
                            move |range: Range<usize>, _window, cx| {
                                range
                                    .filter_map(|index| rows.get(index))
                                    .map(|row| {
                                        row_columns
                                            .iter()
                                            .map(|column| {
                                                let align_right = view
                                                    .upgrade()
                                                    .and_then(|view| {
                                                        view.read(cx)
                                                            .table
                                                            .value
                                                            .as_ref()
                                                            .and_then(|table| {
                                                                table.columns.get(*column)
                                                            })
                                                            .map(|column| column.align_right)
                                                    })
                                                    .unwrap_or(false);
                                                match row.cells.get(*column) {
                                                    Some(cell) => {
                                                        render_cell(cell, align_right, &view, cx)
                                                    }
                                                    None => div().into_any_element(),
                                                }
                                            })
                                            .collect()
                                    })
                                    .collect()
                            }
                        }),
                ),
            )
            .into_any_element()
    }

    fn render_drawer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let selection = self.selection.clone()?;
        let content = self
            .drawer
            .value
            .as_ref()
            .filter(|content| content.selection == selection);
        let details = content.and_then(|content| content.details.as_ref());
        let (title, subtitle, status) = match (details, &selection) {
            (Some(details), _) => (
                details.title.clone(),
                details.subtitle.clone(),
                details.status.clone(),
            ),
            (None, Selection::HelmRelease { namespace, .. }) => (
                selection.title(),
                format!("Helm release · {namespace}"),
                None,
            ),
            (None, Selection::Object(reference)) => {
                (reference.name.clone(), reference.kind.kind.clone(), None)
            }
        };
        let actions = details
            .map(|details| details.actions.clone())
            .unwrap_or_default();
        let reference = match &selection {
            Selection::Object(reference) => Some(reference.clone()),
            Selection::HelmRelease { .. } => None,
        };

        let header = v_flex()
            .p_3()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_1()
                    .when(!self.history.is_empty(), |this| {
                        this.child(
                            IconButton::new("drawer-back", IconName::ArrowLeft)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Back"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.go_back(window, cx)),
                                ),
                        )
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(title)
                                    .weight(gpui::FontWeight::SEMIBOLD)
                                    .truncate(),
                            )
                            .child(muted(subtitle).truncate()),
                    )
                    .when_some(status, |this, (text, tone)| {
                        this.child(badge(text, tone, cx))
                    })
                    .child(
                        IconButton::new("drawer-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Close"))
                            .on_click(cx.listener(|this, _, _, cx| this.close_drawer(cx))),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .children(
                        reference
                            .clone()
                            .into_iter()
                            .flat_map(|reference| {
                                actions
                                    .iter()
                                    .map(move |action| (reference.clone(), *action))
                                    .collect::<Vec<_>>()
                            })
                            .map(|(reference, action)| {
                                Button::new(
                                    SharedString::from(format!("action-{}", action.title())),
                                    action.title(),
                                )
                                .style(if action == ResourceAction::Delete {
                                    ButtonStyle::Tinted(ui::TintColor::Error)
                                } else {
                                    ButtonStyle::Outlined
                                })
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.run_action(action, reference.clone(), window, cx)
                                    },
                                ))
                            }),
                    )
                    .when_some(
                        match &selection {
                            Selection::HelmRelease { name, namespace } => {
                                Some((name.clone(), namespace.clone()))
                            }
                            Selection::Object(_) => None,
                        },
                        |this, (name, namespace)| {
                            this.child(
                                Button::new("helm-uninstall", "Uninstall")
                                    .style(ButtonStyle::Tinted(ui::TintColor::Error))
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let name = name.clone();
                                        let namespace = namespace.clone();
                                        this.confirm_then(
                                            format!("Uninstall Helm release {name}?"),
                                            "Every resource of the release is deleted.".to_string(),
                                            "Uninstall",
                                            window,
                                            cx,
                                            move |this, window, cx| {
                                                this.close_drawer(cx);
                                                this.run_change(
                                                    "Uninstalled",
                                                    move |cluster: Arc<Cluster>| async move {
                                                        cluster
                                                            .helm(&[
                                                                "uninstall",
                                                                &name,
                                                                "--namespace",
                                                                &namespace,
                                                            ])
                                                            .await
                                                    },
                                                    window,
                                                    cx,
                                                )
                                            },
                                        )
                                    })),
                            )
                        },
                    ),
            )
            .when_some(self.render_prompt(cx), |this, prompt| this.child(prompt))
            .when(reference.is_some(), |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("tab-details", "Details")
                                .style(ButtonStyle::Subtle)
                                .toggle_state(self.details_tab == DetailsTab::Details)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.details_tab = DetailsTab::Details;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("tab-yaml", "YAML")
                                .style(ButtonStyle::Subtle)
                                .toggle_state(self.details_tab == DetailsTab::Yaml)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.details_tab = DetailsTab::Yaml;
                                    let missing = this
                                        .drawer
                                        .value
                                        .as_ref()
                                        .is_none_or(|content| content.yaml.is_none());
                                    if missing && let Some(selection) = this.selection.clone() {
                                        this.load_drawer(selection, true, window, cx);
                                    }
                                    cx.notify();
                                })),
                        ),
                )
            });

        let body = match content {
            None => v_flex()
                .p_3()
                .child(match &self.drawer.error {
                    Some(error) => Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error)
                        .into_any_element(),
                    None => muted("Loading…").into_any_element(),
                })
                .into_any_element(),
            Some(content) if self.details_tab == DetailsTab::Yaml && reference.is_some() => {
                v_flex()
                    .p_3()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("yaml-edit", "Edit in Editor")
                                    .style(ButtonStyle::Filled)
                                    .label_size(LabelSize::Small)
                                    .start_icon(Icon::new(IconName::Pencil).size(IconSize::Small))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(reference) = reference.clone() {
                                            this.edit_yaml(reference, window, cx);
                                        }
                                    })),
                            )
                            .child({
                                let yaml = content.yaml.clone().unwrap_or_default();
                                Button::new("yaml-copy", "Copy")
                                    .style(ButtonStyle::Outlined)
                                    .label_size(LabelSize::Small)
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            yaml.clone(),
                                        ))
                                    })
                            }),
                    )
                    .child(code_block(
                        content
                            .yaml
                            .clone()
                            .unwrap_or_else(|| "Loading…".to_string()),
                        cx,
                    ))
                    .into_any_element()
            }
            Some(content) => {
                let sections = match &content.details {
                    Some(details) => details.sections.as_slice(),
                    None => content.sections.as_slice(),
                };
                v_flex()
                    .p_3()
                    .gap_4()
                    .children(sections.iter().enumerate().filter_map(|(index, section)| {
                        self.render_section(index, section, reference.as_ref(), cx)
                    }))
                    .into_any_element()
            }
        };

        Some(
            v_flex()
                .w(px(DETAILS_WIDTH))
                .flex_none()
                .h_full()
                .border_l_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().panel_background)
                .child(header)
                .child(
                    div()
                        .id("kubernetes-drawer-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(body),
                )
                .into_any_element(),
        )
    }

    fn render_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.prompt.as_ref()?;
        let label = match prompt {
            Prompt::Scale(_) => "Replicas",
            Prompt::PortForward(_) => "Local port:remote port",
            Prompt::SetKey(_, Some(_)) => "New value",
            Prompt::SetKey(_, None) => "KEY=value",
        };
        Some(
            v_flex()
                .gap_1()
                .child(muted(label))
                .child(
                    h_flex()
                        .gap_1()
                        .child(div().flex_1().child(self.prompt_input.clone()))
                        .child(
                            Button::new("prompt-submit", "Apply")
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_prompt(window, cx)
                                })),
                        )
                        .child(
                            Button::new("prompt-cancel", "Cancel")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.prompt = None;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_section(
        &self,
        index: usize,
        section: &Section,
        reference: Option<&ObjectRef>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = Utc::now();
        let (title, body): (String, AnyElement) = match section {
            Section::KeyValues { title, items } => {
                if items.is_empty() {
                    return None;
                }
                (
                    title.clone(),
                    v_flex()
                        .gap_1()
                        .children(
                            items
                                .iter()
                                .enumerate()
                                .map(|(row, item)| self.render_key_value(index, row, item, cx)),
                        )
                        .into_any_element(),
                )
            }
            Section::Labels { title, items } => {
                if items.is_empty() {
                    return None;
                }
                (
                    title.clone(),
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .children(items.iter().map(|(key, value)| {
                            div()
                                .px_1p5()
                                .rounded_sm()
                                .bg(cx.theme().colors().element_background)
                                .child(
                                    Label::new(format!("{key}={value}"))
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx),
                                )
                        }))
                        .into_any_element(),
                )
            }
            Section::Conditions { title, items } => {
                if items.is_empty() {
                    return None;
                }
                (
                    title.clone(),
                    v_flex()
                        .gap_1()
                        .children(items.iter().map(|condition| {
                            let tone = match condition.status.as_str() {
                                "True" => Tone::Ok,
                                "False" => Tone::Warn,
                                _ => Tone::Muted,
                            };
                            h_flex()
                                .gap_2()
                                .child(badge(condition.condition_type.clone(), tone, cx))
                                .child(
                                    div().flex_1().min_w_0().child(
                                        muted(format!(
                                            "{} {}",
                                            condition.reason, condition.message
                                        ))
                                        .truncate(),
                                    ),
                                )
                                .child(muted(
                                    condition
                                        .time
                                        .map(|time| format_age(time, now))
                                        .unwrap_or_default(),
                                ))
                        }))
                        .into_any_element(),
                )
            }
            Section::Containers { title, items, pod } => (
                title.clone(),
                v_flex()
                    .gap_2()
                    .children(
                        items
                            .iter()
                            .enumerate()
                            .map(|(container_index, container)| {
                                let pod_for_logs = pod.clone();
                                let pod_for_previous = pod.clone();
                                let pod_for_shell = pod.clone();
                                let name_for_logs = container.name.clone();
                                let name_for_previous = container.name.clone();
                                let name_for_shell = container.name.clone();
                                card(cx)
                                    .p_2()
                                    .gap_1()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                Indicator::dot().color(tone_color(container.tone)),
                                            )
                                            .child(
                                                Label::new(if container.init {
                                                    format!("{} (init)", container.name)
                                                } else {
                                                    container.name.clone()
                                                })
                                                .weight(gpui::FontWeight::MEDIUM),
                                            )
                                            .child(div().flex_1())
                                            .child(badge(
                                                container.state.clone(),
                                                container.tone,
                                                cx,
                                            )),
                                    )
                                    .child(
                                        muted(container.image.clone()).buffer_font(cx).truncate(),
                                    )
                                    .child(muted(format!(
                                        "{} restarts{}{}",
                                        container.restarts,
                                        if container.requests.is_empty() {
                                            String::new()
                                        } else {
                                            format!(" · requests {}", container.requests)
                                        },
                                        if container.limits.is_empty() {
                                            String::new()
                                        } else {
                                            format!(" · limits {}", container.limits)
                                        },
                                    )))
                                    .when_some(container.usage.clone(), |this, usage| {
                                        this.child(muted(usage))
                                    })
                                    .when_some(container.last_state.clone(), |this, last| {
                                        this.child(muted(format!("Last state: {last}")))
                                    })
                                    .when_some(container.command.clone(), |this, command| {
                                        this.child(muted(command).buffer_font(cx).truncate())
                                    })
                                    .when(!container.ports.is_empty(), |this| {
                                        this.child(h_flex().flex_wrap().gap_1().children(
                                            container.ports.iter().map(|(name, port, protocol)| {
                                                let pod = pod.clone();
                                                let port = *port;
                                                Button::new(
                                                    SharedString::from(format!(
                                                        "forward-{container_index}-{port}"
                                                    )),
                                                    format!(
                                                        "{}{port}/{protocol}",
                                                        if name.is_empty() {
                                                            String::new()
                                                        } else {
                                                            format!("{name} ")
                                                        }
                                                    ),
                                                )
                                                .style(ButtonStyle::Outlined)
                                                .label_size(LabelSize::XSmall)
                                                .start_icon(
                                                    Icon::new(IconName::ArrowUpRight)
                                                        .size(IconSize::XSmall),
                                                )
                                                .tooltip(Tooltip::text("Port forward"))
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.prompt =
                                                            Some(Prompt::PortForward(pod.clone()));
                                                        this.prompt_input.update(
                                                            cx,
                                                            |input, cx| {
                                                                input.set_text(
                                                                    &format!("{port}:{port}"),
                                                                    window,
                                                                    cx,
                                                                )
                                                            },
                                                        );
                                                        cx.notify();
                                                    },
                                                ))
                                            }),
                                        ))
                                    })
                                    .when(!container.environment.is_empty(), |this| {
                                        this.child(v_flex().children(
                                            container.environment.iter().map(|(name, value)| {
                                                muted(format!("{name}={value}"))
                                                    .buffer_font(cx)
                                                    .truncate()
                                            }),
                                        ))
                                    })
                                    .when(!container.mounts.is_empty(), |this| {
                                        this.child(
                                            v_flex().children(
                                                container
                                                    .mounts
                                                    .iter()
                                                    .map(|mount| muted(mount.clone()).truncate()),
                                            ),
                                        )
                                    })
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .child(
                                                Button::new(
                                                    SharedString::from(format!(
                                                        "container-logs-{container_index}"
                                                    )),
                                                    "Logs",
                                                )
                                                .style(ButtonStyle::Outlined)
                                                .label_size(LabelSize::XSmall)
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.container_logs(
                                                            &pod_for_logs,
                                                            &name_for_logs,
                                                            false,
                                                            window,
                                                            cx,
                                                        )
                                                    },
                                                )),
                                            )
                                            .when(container.restarts > 0, |this| {
                                                this.child(
                                                    Button::new(
                                                        SharedString::from(format!(
                                                            "container-previous-{container_index}"
                                                        )),
                                                        "Previous Logs",
                                                    )
                                                    .style(ButtonStyle::Outlined)
                                                    .label_size(LabelSize::XSmall)
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.container_logs(
                                                                &pod_for_previous,
                                                                &name_for_previous,
                                                                true,
                                                                window,
                                                                cx,
                                                            )
                                                        },
                                                    )),
                                                )
                                            })
                                            .when(!container.init, |this| {
                                                this.child(
                                                    Button::new(
                                                        SharedString::from(format!(
                                                            "container-shell-{container_index}"
                                                        )),
                                                        "Shell",
                                                    )
                                                    .style(ButtonStyle::Outlined)
                                                    .label_size(LabelSize::XSmall)
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.open_shell(
                                                                &pod_for_shell,
                                                                Some(&name_for_shell),
                                                                window,
                                                                cx,
                                                            )
                                                        },
                                                    )),
                                                )
                                            }),
                                    )
                            }),
                    )
                    .into_any_element(),
            ),
            Section::Table { title, table } => (
                title.clone(),
                v_flex()
                    .gap_px()
                    .when(table.rows.is_empty(), |this| this.child(muted("None")))
                    .children(table.rows.iter().enumerate().map(|(row_index, row)| {
                        let reference = row.reference.clone();
                        let status = table
                            .columns
                            .iter()
                            .position(|column| column.id == "status")
                            .and_then(|column| row.cells.get(column))
                            .cloned();
                        let summary = row
                            .cells
                            .iter()
                            .zip(&table.columns)
                            .filter(|(_, column)| {
                                !matches!(column.id, "name" | "namespace" | "status" | "containers")
                            })
                            .filter(|(cell, _)| cell.text != "—" && !cell.text.is_empty())
                            .map(|(cell, column)| format!("{} {}", column.title, cell.text))
                            .collect::<Vec<_>>()
                            .join(" · ");
                        h_flex()
                            .id(SharedString::from(format!(
                                "section-{index}-row-{row_index}"
                            )))
                            .px_1()
                            .py_0p5()
                            .gap_2()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(cx.theme().colors().element_hover))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_link(reference.clone(), window, cx)
                            }))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        Label::new(row.reference.name.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Accent)
                                            .truncate(),
                                    )
                                    .child(
                                        Label::new(summary)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                            )
                            .when_some(status, |this, status| {
                                this.child(badge(
                                    status.text,
                                    status.tone.unwrap_or(Tone::Muted),
                                    cx,
                                ))
                            })
                    }))
                    .into_any_element(),
            ),
            Section::Data {
                title,
                items,
                secret,
            } => {
                let reference = reference.cloned();
                let add_reference = reference.clone();
                (
                    title.clone(),
                    v_flex()
                        .gap_1()
                        .when(items.is_empty(), |this| this.child(muted("No keys")))
                        .children(items.iter().enumerate().map(|(row, entry)| {
                            self.render_data_entry(
                                index,
                                row,
                                entry,
                                *secret,
                                reference.clone(),
                                cx,
                            )
                        }))
                        .when_some(add_reference, |this, reference| {
                            this.child(
                                Button::new(
                                    SharedString::from(format!("data-add-{index}")),
                                    "Add Key",
                                )
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.prompt = Some(Prompt::SetKey(reference.clone(), None));
                                        this.prompt_input
                                            .update(cx, |input, cx| input.set_text("", window, cx));
                                        cx.notify();
                                    },
                                )),
                            )
                        })
                        .into_any_element(),
                )
            }
            Section::Ports {
                title,
                items,
                service,
            } => {
                if items.is_empty() {
                    return None;
                }
                (
                    title.clone(),
                    v_flex()
                        .gap_1()
                        .children(items.iter().enumerate().map(|(row, port)| {
                            let service = service.clone();
                            let port_number = port.port;
                            h_flex()
                                .gap_2()
                                .child(
                                    Label::new(format!(
                                        "{}{}/{} → {}{}",
                                        if port.name.is_empty() {
                                            String::new()
                                        } else {
                                            format!("{} ", port.name)
                                        },
                                        port.port,
                                        port.protocol,
                                        port.target_port,
                                        port.node_port
                                            .map(|node_port| format!(" (node {node_port})"))
                                            .unwrap_or_default()
                                    ))
                                    .size(LabelSize::Small),
                                )
                                .child(div().flex_1())
                                .child(
                                    Button::new(
                                        SharedString::from(format!("service-forward-{row}")),
                                        "Forward",
                                    )
                                    .style(ButtonStyle::Outlined)
                                    .label_size(LabelSize::XSmall)
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.prompt =
                                                Some(Prompt::PortForward(service.clone()));
                                            this.prompt_input.update(cx, |input, cx| {
                                                input.set_text(
                                                    &format!("{port_number}:{port_number}"),
                                                    window,
                                                    cx,
                                                )
                                            });
                                            cx.notify();
                                        },
                                    )),
                                )
                        }))
                        .into_any_element(),
                )
            }
            Section::Events { title, items } => (
                title.clone(),
                v_flex()
                    .gap_1()
                    .when(items.is_empty(), |this| this.child(muted("No events")))
                    .children(items.iter().take(30).map(|event| {
                        v_flex()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(badge(
                                        event.reason.clone(),
                                        if event.event_type == "Warning" {
                                            Tone::Warn
                                        } else {
                                            Tone::Muted
                                        },
                                        cx,
                                    ))
                                    .child(div().flex_1())
                                    .child(muted(format!(
                                        "×{} · {}",
                                        event.count,
                                        event
                                            .time
                                            .map(|time| format_age(time, now))
                                            .unwrap_or_default()
                                    ))),
                            )
                            .child(Label::new(event.message.clone()).size(LabelSize::Small))
                    }))
                    .into_any_element(),
            ),
            Section::Revisions { title, items, helm } => {
                if items.is_empty() {
                    return None;
                }
                (title.clone(), v_flex().gap_1().children(items.iter().map(|revision| {
                    let number = revision.revision;
                    let helm = *helm;
                    let selection = self.selection.clone();
                    let reference = reference.cloned();
                    h_flex()
                        .gap_2()
                        .child(Label::new(format!("#{number}")).size(LabelSize::Small).weight(gpui::FontWeight::MEDIUM))
                        .when(revision.current, |this| this.child(badge("current", Tone::Ok, cx)))
                        .child(div().flex_1().min_w_0().child(muted(revision.description.clone()).truncate()))
                        .child(muted(revision.created_at.clone().map(|time| time.split('.').next().unwrap_or_default().replace('T', " ")).unwrap_or_default()))
                        .when(!revision.current, |this| {
                            this.child(
                                Button::new(SharedString::from(format!("rollback-{number}")), "Roll Back")
                                    .style(ButtonStyle::Outlined)
                                    .label_size(LabelSize::XSmall)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let selection = selection.clone();
                                        let reference = reference.clone();
                                        this.confirm_then(
                                            format!("Roll back to revision {number}?"),
                                            "The previous configuration is applied to the cluster.".to_string(),
                                            "Roll Back",
                                            window,
                                            cx,
                                            move |this, window, cx| {
                                                this.run_change(
                                                    "Rolled back",
                                                    move |cluster: Arc<Cluster>| async move { match (helm, selection, reference) {
                                                        (true, Some(Selection::HelmRelease { name, namespace }), _) => {
                                                            cluster.helm(&["rollback", &name, &number.to_string(), "--namespace", &namespace]).await
                                                        }
                                                        (false, _, Some(reference)) => cluster.rollback(&reference, number).await.map(|_| String::new()),
                                                        _ => Ok(String::new()),
                                                    } },
                                                    window,
                                                    cx,
                                                )
                                            },
                                        )
                                    })),
                            )
                        })
                })).into_any_element())
            }
            Section::Usage { title, items } => (
                title.clone(),
                v_flex()
                    .gap_2()
                    .children(items.iter().map(|row| {
                        let capacity = if row.capacity > 0.0 {
                            row.capacity
                        } else {
                            row.limit
                                .max(row.requested)
                                .max(row.used.unwrap_or_default())
                        };
                        let capacity = capacity.max(f64::EPSILON);
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .justify_between()
                                    .child(Label::new(row.label.clone()).size(LabelSize::Small))
                                    .child(muted(format!(
                                        "used {} · requested {} · limit {}{}",
                                        row.used
                                            .map(|used| row.format(used))
                                            .unwrap_or_else(|| "—".to_string()),
                                        if row.requested > 0.0 {
                                            row.format(row.requested)
                                        } else {
                                            "—".to_string()
                                        },
                                        if row.limit > 0.0 {
                                            row.format(row.limit)
                                        } else {
                                            "—".to_string()
                                        },
                                        if row.capacity > 0.0 {
                                            format!(" · of {}", row.format(row.capacity))
                                        } else {
                                            String::new()
                                        },
                                    ))),
                            )
                            .child(bar(
                                row.used.unwrap_or_default() / capacity,
                                tone_hsla(Tone::Info, cx),
                                cx,
                            ))
                    }))
                    .into_any_element(),
            ),
            Section::Text { title, text } => {
                if text.trim().is_empty() {
                    return None;
                }
                (
                    title.clone(),
                    code_block(text.clone(), cx).into_any_element(),
                )
            }
        };
        Some(
            v_flex()
                .gap_2()
                .child(section_title(title))
                .child(body)
                .into_any_element(),
        )
    }

    fn render_key_value(
        &self,
        section: usize,
        row: usize,
        item: &KeyValue,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let link = item.link.clone();
        h_flex()
            .id(SharedString::from(format!("kv-{section}-{row}")))
            .gap_2()
            .items_start()
            .child(div().w(px(130.)).flex_none().child(muted(item.key.clone())))
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(item.value.clone()).size(LabelSize::Small).color(
                        if link.is_some() {
                            Color::Accent
                        } else {
                            item.tone.map(tone_color).unwrap_or(Color::Default)
                        },
                    ),
                ),
            )
            .when_some(link, |this, link| {
                this.cursor_pointer().on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.open_link(link.clone(), window, cx)
                    }),
                )
            })
            .into_any_element()
    }

    fn render_data_entry(
        &self,
        section: usize,
        row: usize,
        entry: &DataEntry,
        secret: bool,
        reference: Option<ObjectRef>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let revealed = !secret || self.revealed.contains(&entry.key);
        let key = entry.key.clone();
        let value = entry.value.clone();
        let edit_reference = reference.clone();
        let edit_key = entry.key.clone();
        let edit_value = entry.value.clone();
        let delete_reference = reference;
        let delete_key = entry.key.clone();
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(entry.key.clone())
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::MEDIUM),
                    )
                    .child(muted(format!("{} bytes", entry.size)))
                    .child(div().flex_1())
                    .when(secret, |this| {
                        let key = key.clone();
                        this.child(
                            IconButton::new(
                                SharedString::from(format!("reveal-{section}-{row}")),
                                if revealed {
                                    IconName::EyeOff
                                } else {
                                    IconName::Eye
                                },
                            )
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(if revealed { "Hide" } else { "Reveal" }))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if !this.revealed.remove(&key) {
                                        this.revealed.insert(key.clone());
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .child(
                        IconButton::new(
                            SharedString::from(format!("copy-{section}-{row}")),
                            IconName::Copy,
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Copy value"))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(value.clone()))
                        }),
                    )
                    .when(!entry.binary, |this| {
                        this.child(
                            IconButton::new(
                                SharedString::from(format!("edit-{section}-{row}")),
                                IconName::Pencil,
                            )
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Edit value"))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if let Some(reference) = edit_reference.clone() {
                                        this.prompt =
                                            Some(Prompt::SetKey(reference, Some(edit_key.clone())));
                                        this.prompt_input.update(cx, |input, cx| {
                                            input.set_text(&edit_value, window, cx)
                                        });
                                        cx.notify();
                                    }
                                },
                            )),
                        )
                    })
                    .child(
                        IconButton::new(
                            SharedString::from(format!("delete-{section}-{row}")),
                            IconName::Trash,
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Delete key"))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                let Some(reference) = delete_reference.clone() else {
                                    return;
                                };
                                let key = delete_key.clone();
                                this.confirm_then(
                                    format!("Delete key {key}?"),
                                    format!("The key is removed from {}.", reference.display()),
                                    "Delete",
                                    window,
                                    cx,
                                    move |this, window, cx| {
                                        this.run_change(
                                            "Deleted",
                                            move |cluster: Arc<Cluster>| async move {
                                                cluster
                                                    .set_data_key(&reference, &key, None)
                                                    .await
                                                    .map(|_| String::new())
                                            },
                                            window,
                                            cx,
                                        )
                                    },
                                )
                            },
                        )),
                    ),
            )
            .child(code_block(
                if revealed {
                    entry.value.clone()
                } else {
                    "••••••••".to_string()
                },
                cx,
            ))
            .into_any_element()
    }
}

fn code_block(text: String, cx: &App) -> impl IntoElement {
    div()
        .p_2()
        .rounded_sm()
        .bg(cx.theme().colors().editor_background)
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .child(Label::new(text).size(LabelSize::Small).buffer_font(cx))
}

fn render_cell(
    cell: &Cell,
    align_right: bool,
    view: &WeakEntity<KubernetesView>,
    cx: &mut App,
) -> AnyElement {
    if !cell.dots.is_empty() {
        return h_flex()
            .gap_0p5()
            .children(
                cell.dots
                    .iter()
                    .map(|(tone, _)| Indicator::dot().color(tone_color(*tone))),
            )
            .into_any_element();
    }
    if cell.badge {
        return badge(cell.text.clone(), cell.tone.unwrap_or(Tone::Muted), cx).into_any_element();
    }
    let color = if cell.link.is_some() {
        Color::Accent
    } else {
        cell.tone.map(tone_color).unwrap_or(Color::Default)
    };
    let label = Label::new(cell.text.clone())
        .size(LabelSize::Small)
        .color(color)
        .truncate();
    let content = match cell.ratio {
        Some(ratio) => v_flex()
            .w_full()
            .gap_0p5()
            .child(
                h_flex()
                    .when(align_right, |this| this.justify_end())
                    .child(label),
            )
            .child(bar(
                ratio,
                tone_hsla(cell.tone.unwrap_or(Tone::Info), cx),
                cx,
            ))
            .into_any_element(),
        None => h_flex()
            .w_full()
            .when(align_right, |this| this.justify_end())
            .child(label)
            .into_any_element(),
    };
    match cell.link.clone() {
        Some(link) => {
            let view = view.clone();
            div()
                .id(SharedString::from(format!(
                    "link-{}-{}",
                    link.kind.id, link.name
                )))
                .w_full()
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    view.update(cx, |this, cx| this.open_link(link.clone(), window, cx))
                        .log_err();
                })
                .child(content)
                .into_any_element()
        }
        None => content,
    }
}

impl EventEmitter<ItemEvent> for KubernetesView {}

impl Focusable for KubernetesView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for KubernetesView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match &self.context.value {
            Some(Some(context)) => format!("Kubernetes: {}", context.context).into(),
            _ => "Kubernetes".into(),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Blocks))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for KubernetesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = v_flex()
            .key_context("KubernetesView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background);
        match &self.context.value {
            None => {
                return root.child(self.render_placeholder(
                    &self.context.error,
                    "Connecting to the cluster…",
                    cx,
                ));
            }
            Some(None) => {
                return root.child(
                    v_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .child(
                            Icon::new(IconName::Blocks)
                                .size(IconSize::XLarge)
                                .color(Color::Muted),
                        )
                        .child(Label::new(
                            "No Kubernetes cluster is connected to this project.",
                        ))
                        .child(muted(
                            "Import a kubeconfig from the Kubernetes page of the Sandbox panel.",
                        )),
                );
            }
            Some(Some(_)) => {}
        }
        let status = self.status.clone();
        let main = match self.page {
            Page::Overview => self.render_overview(cx),
            Page::Kind(_) => self.render_table(cx),
        };
        root.child(self.render_toolbar(window, cx))
            .when_some(status, |this, (message, error)| {
                this.child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .bg(if error {
                            cx.theme().status().error_background
                        } else {
                            cx.theme().status().info_background
                        })
                        .child(Label::new(message).size(LabelSize::Small).color(if error {
                            Color::Error
                        } else {
                            Color::Default
                        }))
                        .child(div().flex_1())
                        .child(
                            IconButton::new("dismiss-status", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.status = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_sidebar(cx))
                    .child(div().flex_1().min_w_0().h_full().child(main))
                    .children(self.render_drawer(cx)),
            )
    }
}
