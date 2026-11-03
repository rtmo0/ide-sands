use std::{path::PathBuf, sync::Arc};

use fuzzy_nucleo::{StringMatch, StringMatchCandidate};
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, Focusable, PromptLevel, SharedString,
    Subscription, Task, actions,
};
use picker::{Picker, PickerDelegate};
use recent_projects::open_remote_project;
use remote::{RemoteConnectionOptions, SshConnectionOptions};
use sbx::{SandboxInfo, SandboxStatus, Sbx};
use ui::{
    Color, Context, HighlightedLabel, Icon, IconName, InteractiveElement, IntoElement, Label,
    LabelCommon as _, LabelSize, ListItem, ListItemSpacing, ParentElement, Render, Styled,
    StyledExt, Toggleable, Window, div, h_flex, rems,
};
use util::ResultExt as _;
use workspace::{
    ModalView, MultiWorkspace, OpenMode, OpenOptions, notifications::DetachAndPromptErr,
    with_active_or_new_workspace,
};

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
                let sandboxes = cx
                    .background_spawn(async move {
                        let sbx = Sbx::locate()?;
                        sbx.check().await?;
                        sbx.list().await
                    })
                    .await;
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
        let app_state = workspace::AppState::global(cx);
        let open_options = OpenOptions {
            requesting_window: window.window_handle().downcast::<MultiWorkspace>(),
            open_mode: if secondary {
                OpenMode::NewWindow
            } else {
                OpenMode::Activate
            },
            ..Default::default()
        };

        cx.emit(DismissEvent);
        cx.spawn_in(window, async move |_, cx| {
            let sbx = Sbx::locate()?;
            let report = sbx.diagnose().await?;
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

            let directory = sbx.default_directory(&name).await?;
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
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to open sandbox", window, cx, |_, _, _| None);
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
