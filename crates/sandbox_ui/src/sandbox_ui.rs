use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use collections::HashSet;
use fuzzy_nucleo::{StringMatch, StringMatchCandidate};
use gpui::{
    Action as _, App, AppContext as _, DismissEvent, Entity, EventEmitter, Focusable, PromptLevel,
    SharedString, Subscription, Task, actions,
};
use picker::{Picker, PickerDelegate};
use project::{
    trusted_worktrees::{PathTrust, TrustedWorktrees},
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};
use recent_projects::open_remote_project;
use remote::{RemoteConnectionOptions, SshConnectionOptions};
use sbx::{SandboxInfo, SandboxStatus, Sbx, SbxError};
use ui::{
    Color, Context, HighlightedLabel, Icon, IconName, InteractiveElement, IntoElement, Label,
    LabelCommon as _, LabelSize, ListItem, ListItemSpacing, ParentElement, Render, Styled,
    StyledExt, Toggleable, Window, div, h_flex, rems,
};
use util::ResultExt as _;
use workspace::{
    ModalView, MultiWorkspace, OpenMode, OpenOptions, Workspace, notifications::DetachAndPromptErr,
    with_active_or_new_workspace,
};

mod agent_status;
mod connections;
mod delete_project;
mod design;
mod gateway;
mod kubernetes_panel;
mod kubernetes_view;
mod models_page;
mod new_project_modal;
mod projects_panel;
mod provisioning;
mod sandbox_panel;
mod sbx_problem;
mod settings_migration;
mod settings_review;

pub use kubernetes_panel::{KubernetesPanel, ToggleKubernetesPanel};
pub use kubernetes_view::{KubernetesView, OpenKubernetes};
pub use projects_panel::{NewProject, ProjectsPanel, ToggleProjectsPanel};
pub use sandbox_panel::{SandboxPanel, ToggleSandboxPanel};
pub use settings_migration::MigrateProjectSettings;
pub use settings_review::ReviewSettingsChanges;

/// Built-in `sbx` agents, by the name `sbx create` takes and the name shown to the user. `shell`
/// is a sandbox without a coding agent.
pub(crate) const AGENTS: [(&str, &str); 11] = [
    ("shell", "No agent"),
    ("claude", "Claude Code"),
    ("codex", "Codex"),
    ("copilot", "GitHub Copilot"),
    ("cursor", "Cursor"),
    ("gemini", "Gemini CLI"),
    ("opencode", "OpenCode"),
    ("kiro", "Kiro"),
    ("devin", "Devin"),
    ("droid", "Droid"),
    ("docker-agent", "Docker Agent"),
];

pub(crate) fn agent_display_name(agent: &str) -> &str {
    AGENTS
        .iter()
        .find(|(name, _)| *name == agent)
        .map(|(_, display_name)| *display_name)
        .unwrap_or(agent)
}

actions!(
    sands,
    [
        /// Opens a Docker Sandbox as a remote project over SSH.
        OpenSandbox
    ]
);

pub fn init(cx: &mut App) {
    cx.on_action(|_: &OpenSandbox, cx| {
        with_active_or_new_workspace(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, SandboxModal::new);
        });
    });
    agent_status::init(cx);
    connections::init(cx);
    gateway::init(cx);
    kubernetes_panel::init(cx);
    kubernetes_view::init(cx);
    projects_panel::init(cx);
    sandbox_panel::init(cx);
    settings_migration::init(cx);
    settings_review::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _, cx| trust_sandbox_worktrees(workspace, cx))
        .detach();
}

/// Checks that `sbx` is usable before listing, so failures name the actual problem.
async fn list_sandboxes() -> Result<Vec<SandboxInfo>, SbxError> {
    let sbx = Sbx::locate()?;
    sbx.check().await?;
    sbx.list().await
}

/// Connects to the sandbox over SSH and opens `directory`, or the sandbox workspace directory.
/// Starts the sandbox if it is stopped and, after asking, configures the `*.sbx` SSH hosts.
pub(crate) fn open_sandbox(
    name: String,
    directory: Option<String>,
    open_mode: OpenMode,
    window: &mut Window,
    cx: &mut App,
) {
    let app_state = workspace::AppState::global(cx);
    let open_options = OpenOptions {
        requesting_window: window.window_handle().downcast::<MultiWorkspace>(),
        open_mode,
        ..Default::default()
    };

    window
        .spawn(cx, async move |cx| {
            let sbx = Sbx::locate()?;
            // `sbx diagnose` takes over a second; once it passed, a sandbox switch skips it and a
            // failing connection still reports its own error.
            if SBX_CHECKED.load(Ordering::Relaxed) {
                return open_sandbox_directory(sbx, name, directory, app_state, open_options, cx)
                    .await;
            }
            let report = sbx.diagnose().await?;
            if let Err(error) = report.ensure_ready() {
                // The Projects panel offers the fix (start the daemon, sign in) once it reloads;
                // without this the SSH connection would only time out.
                cx.update(|window, cx| {
                    window.dispatch_action(projects_panel::RefreshProjects.boxed_clone(), cx)
                })?;
                return Err(error.into());
            }
            if !report.ssh_configured() {
                let answer = cx
                    .update(|window, cx| {
                        window.prompt(
                            PromptLevel::Info,
                            "Configure SSH for Docker Sandboxes?",
                            Some(
                                "Sands connects to sandboxes over SSH. `sbx setup ssh` adds an \
                                 Include line for the *.sbx hosts to ~/.ssh/config.",
                            ),
                            &["Configure", "Cancel"],
                            cx,
                        )
                    })?
                    .await?;
                if answer != 0 {
                    return Ok(());
                }
                sbx.setup_ssh().await?;
            }
            SBX_CHECKED.store(true, Ordering::Relaxed);
            open_sandbox_directory(sbx, name, directory, app_state, open_options, cx).await
        })
        .detach_and_prompt_err("Failed to open project", window, cx, |_, _, _| None);
}

/// Whether `sbx` was found ready and SSH configured during this session.
static SBX_CHECKED: AtomicBool = AtomicBool::new(false);

async fn open_sandbox_directory(
    sbx: Sbx,
    name: String,
    directory: Option<String>,
    app_state: Arc<workspace::AppState>,
    open_options: OpenOptions,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<()> {
    let directory = match directory {
        Some(directory) => directory,
        None => default_directory(&sbx, &name).await?,
    };
    let connection_options = RemoteConnectionOptions::Ssh(SshConnectionOptions {
        host: sbx::ssh_host(&name).into(),
        nickname: Some(name),
        // Sands builds its own remote server, which cannot be downloaded inside the sandbox.
        upload_binary_over_ssh: true,
        ..Default::default()
    });
    open_remote_project(
        connection_options,
        vec![PathBuf::from(directory)],
        app_state,
        open_options,
        cx,
    )
    .await?;
    Ok(())
}

/// A sandbox's workspace directory never changes, and asking for it costs an `sbx exec`.
async fn default_directory(sbx: &Sbx, name: &str) -> anyhow::Result<String> {
    static DIRECTORIES: std::sync::Mutex<Option<collections::HashMap<String, String>>> =
        std::sync::Mutex::new(None);
    let cached = DIRECTORIES
        .lock()
        .ok()
        .and_then(|directories| directories.as_ref()?.get(name).cloned());
    if let Some(directory) = cached {
        return Ok(directory);
    }
    let directory = sbx.default_directory(name).await?;
    if let Ok(mut directories) = DIRECTORIES.lock() {
        directories
            .get_or_insert_default()
            .insert(name.to_string(), directory.clone());
    }
    Ok(directory)
}

/// The sandbox is the trust boundary in Sands: code inside it cannot reach the host, so its
/// worktrees are trusted and language servers start without the Restricted Mode prompt.
fn trust_sandbox_worktrees(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    if connected_sandbox(workspace, cx).is_none() {
        return;
    }
    let worktree_store = workspace.project().read(cx).worktree_store();
    trust_all_worktrees(&worktree_store, cx);
    cx.subscribe(&worktree_store, |_, worktree_store, event, cx| {
        if let WorktreeStoreEvent::WorktreeAdded(_) = event {
            trust_all_worktrees(&worktree_store, cx);
        }
    })
    .detach();
}

fn trust_all_worktrees(worktree_store: &Entity<WorktreeStore>, cx: &mut App) {
    let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
        return;
    };
    let worktrees: HashSet<PathTrust> = worktree_store
        .read(cx)
        .worktrees()
        .map(|worktree| PathTrust::Worktree(worktree.read(cx).id()))
        .collect();
    if !worktrees.is_empty() {
        trusted_worktrees.update(cx, |store, cx| store.trust(worktree_store, worktrees, cx));
    }
}

/// The sandbox a workspace is connected to, if it was opened through its `<name>.sbx` host.
pub(crate) fn connected_sandbox(workspace: &Workspace, cx: &App) -> Option<String> {
    match workspace.project().read(cx).remote_connection_options(cx)? {
        RemoteConnectionOptions::Ssh(options) => options
            .host
            .to_string()
            .strip_suffix(".sbx")
            .map(str::to_string),
        _ => None,
    }
}

enum SandboxList {
    Loading,
    Loaded(Vec<SandboxInfo>),
    Failed(SharedString),
}

struct SandboxSelected {
    name: String,
    secondary: bool,
}

struct SandboxPickerDismissed;

struct SandboxPickerDelegate {
    sandboxes: SandboxList,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl SandboxPickerDelegate {
    fn sandbox(&self, name: &str) -> Option<&SandboxInfo> {
        match &self.sandboxes {
            SandboxList::Loaded(sandboxes) => sandboxes.iter().find(|sandbox| sandbox.name == name),
            SandboxList::Loading | SandboxList::Failed(_) => None,
        }
    }
}

impl EventEmitter<SandboxSelected> for Picker<SandboxPickerDelegate> {}
impl EventEmitter<SandboxPickerDismissed> for Picker<SandboxPickerDelegate> {}

impl PickerDelegate for SandboxPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "sandbox-picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        Arc::from("Open a Docker Sandbox…")
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(match &self.sandboxes {
            SandboxList::Loading => "Loading Docker Sandboxes…".into(),
            SandboxList::Failed(message) => message.clone(),
            SandboxList::Loaded(sandboxes) if sandboxes.is_empty() => {
                "No sandboxes yet. Create one with `sbx create --name <name> shell`.".into()
            }
            SandboxList::Loaded(_) => "No matching sandboxes".into(),
        })
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let SandboxList::Loaded(sandboxes) = &self.sandboxes else {
            self.matches.clear();
            return Task::ready(());
        };
        let candidates = sandboxes
            .iter()
            .enumerate()
            .map(|(index, sandbox)| StringMatchCandidate::new(index, &sandbox.name))
            .collect::<Vec<_>>();
        let query = query.trim();
        self.matches = fuzzy_nucleo::match_strings(
            &candidates,
            query,
            fuzzy_nucleo::Case::smart_if_uppercase_in(query),
            fuzzy_nucleo::LengthPenalty::On,
            100,
        );
        if query.is_empty() {
            self.matches
                .sort_unstable_by_key(|matched| matched.candidate_id);
        }
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(matched) = self.matches.get(self.selected_index) {
            cx.emit(SandboxSelected {
                name: matched.string.to_string(),
                secondary,
            });
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(SandboxPickerDismissed);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let matched = self.matches.get(index)?;
        let sandbox = self.sandbox(&matched.string)?;
        let (status, status_color) = match sandbox.status {
            SandboxStatus::Running => ("running", Color::Success),
            SandboxStatus::Stopped => ("stopped", Color::Muted),
            SandboxStatus::Unknown => ("unknown", Color::Warning),
        };
        Some(
            ListItem::new(index)
                .toggle_state(selected)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .child(
                    h_flex()
                        .flex_grow_1()
                        .gap_3()
                        .child(Icon::new(IconName::Server).color(Color::Muted))
                        .child(HighlightedLabel::new(
                            matched.string.clone(),
                            matched.positions.clone(),
                        ))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Label::new(sandbox.agent.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(status)
                                        .size(LabelSize::Small)
                                        .color(status_color),
                                ),
                        ),
                ),
        )
    }
}

pub struct SandboxModal {
    picker: Entity<Picker<SandboxPickerDelegate>>,
    _load_sandboxes: Task<()>,
    _subscriptions: [Subscription; 2],
}

impl SandboxModal {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = SandboxPickerDelegate {
            sandboxes: SandboxList::Loading,
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded());

        let selected = cx.subscribe_in(
            &picker,
            window,
            |this, _, event: &SandboxSelected, window, cx| {
                this.open(event.name.clone(), event.secondary, window, cx);
            },
        );
        let dismissed = cx.subscribe_in(
            &picker,
            window,
            |_, _, _: &SandboxPickerDismissed, _, cx| cx.emit(DismissEvent),
        );

        let load_sandboxes = cx.spawn_in(window, {
            let picker = picker.downgrade();
            async move |_, cx| {
                let sandboxes = cx.background_spawn(list_sandboxes()).await;
                let sandboxes = match sandboxes {
                    Ok(sandboxes) => SandboxList::Loaded(sandboxes),
                    Err(error) => SandboxList::Failed(error.to_string().into()),
                };
                picker
                    .update_in(cx, |picker, window, cx| {
                        picker.delegate.sandboxes = sandboxes;
                        picker.refresh(window, cx);
                    })
                    .log_err();
            }
        });

        Self {
            picker,
            _load_sandboxes: load_sandboxes,
            _subscriptions: [selected, dismissed],
        }
    }

    fn open(&mut self, name: String, secondary: bool, window: &mut Window, cx: &mut Context<Self>) {
        let open_mode = if secondary {
            OpenMode::NewWindow
        } else {
            OpenMode::Activate
        };
        cx.emit(DismissEvent);
        open_sandbox(name, None, open_mode, window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl ModalView for SandboxModal {}

impl EventEmitter<DismissEvent> for SandboxModal {}

impl Focusable for SandboxModal {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for SandboxModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("SandboxModal")
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(34.))
            .flex_1()
            .overflow_hidden()
            .child(self.picker.clone())
    }
}
