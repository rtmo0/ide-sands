use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, Pixels, SharedString, StatefulInteractiveElement as _, Task,
    WeakEntity, actions, px,
};
use sbx::{SandboxInfo, SandboxStatus};
use ui::{
    ActiveTheme as _, Button, ButtonCommon as _, Clickable as _, Color, Context, IconButton,
    IconName, IconSize, Indicator, IntoElement, Label, LabelCommon as _, LabelSize, ListItem,
    ListItemSpacing, ParentElement, Render, Styled, Toggleable, Tooltip, Window, h_flex, v_flex,
};
use util::ResultExt as _;
use workspace::{
    OpenMode, Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelSizeState},
};

use crate::{connected_sandbox, list_sandboxes, new_project_modal::NewProjectModal, open_sandbox};

actions!(
    sands,
    [
        /// Toggles focus on the Sands projects panel.
        ToggleProjectsPanel,
        /// Creates a Sands project: a sandbox with code from a local Git repository or a Git URL.
        NewProject,
        /// Reloads the list of Sands projects.
        RefreshProjects,
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &ToggleProjectsPanel, window, cx| {
                workspace.toggle_panel_focus::<ProjectsPanel>(window, cx);
            })
            .register_action(|workspace, _: &NewProject, window, cx| {
                let workspace_handle = workspace.weak_handle();
                workspace.toggle_modal(window, cx, |window, cx| {
                    NewProjectModal::new(workspace_handle, window, cx)
                });
            })
            .register_action(|workspace, _: &RefreshProjects, window, cx| {
                if let Some(panel) = workspace.panel::<ProjectsPanel>(cx) {
                    panel.update(cx, |panel, cx| panel.refresh(window, cx));
                }
            });
    })
    .detach();
}

enum ProjectList {
    Loading,
    Loaded(Vec<SandboxInfo>),
    Failed(SharedString),
}

/// Lists Sands projects. Each project is a Docker Sandbox, so the list comes from `sbx ls`.
pub struct ProjectsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    projects: ProjectList,
    _refresh: Task<()>,
}

impl ProjectsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let workspace_handle = workspace.weak_handle();
            cx.new(|cx| {
                let mut panel = Self {
                    workspace: workspace_handle,
                    focus_handle: cx.focus_handle(),
                    position: DockPosition::Left,
                    projects: ProjectList::Loading,
                    _refresh: Task::ready(()),
                };
                panel.refresh(window, cx);
                panel
            })
        })
    }

    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._refresh = cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(list_sandboxes()).await;
            this.update(cx, |this, cx| {
                this.projects = match result {
                    Ok(sandboxes) => ProjectList::Loaded(sandboxes),
                    Err(error) => ProjectList::Failed(error.to_string().into()),
                };
                cx.notify();
            })
            .log_err();
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_2()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new("PROJECTS")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("refresh-projects", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                    )
                    .child(
                        IconButton::new("new-project", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New Project"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(NewProject.boxed_clone(), cx)
                            }),
                    ),
            )
    }

    fn render_projects(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sandboxes = match &self.projects {
            ProjectList::Loading => {
                return v_flex().p_3().child(
                    Label::new("Loading projects…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            ProjectList::Failed(message) => {
                return v_flex().p_3().child(
                    Label::new(message.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                );
            }
            ProjectList::Loaded(sandboxes) if sandboxes.is_empty() => {
                return v_flex()
                    .p_3()
                    .gap_2()
                    .child(
                        Label::new("No projects yet.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Button::new("create-first-project", "New Project").on_click(
                        |_, window, cx| window.dispatch_action(NewProject.boxed_clone(), cx),
                    ));
            }
            ProjectList::Loaded(sandboxes) => sandboxes,
        };

        let current = self
            .workspace
            .upgrade()
            .and_then(|workspace| connected_sandbox(workspace.read(cx), cx));

        v_flex()
            .p_2()
            .gap_1()
            .children(sandboxes.iter().enumerate().map(|(index, sandbox)| {
                let (status, status_color) = match sandbox.status {
                    SandboxStatus::Running => ("running", Color::Success),
                    SandboxStatus::Stopped => ("stopped", Color::Muted),
                    SandboxStatus::Unknown => ("unknown", Color::Warning),
                };
                let name = sandbox.name.clone();
                ListItem::new(("project", index))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(current.as_deref() == Some(sandbox.name.as_str()))
                    .start_slot(ui::Icon::new(IconName::Server).color(
                        if current.as_deref() == Some(sandbox.name.as_str()) {
                            Color::Accent
                        } else {
                            Color::Muted
                        },
                    ))
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(Label::new(sandbox.name.clone()).truncate())
                            .child(
                                Label::new(crate::agent_display_name(&sandbox.agent).to_string())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .end_slot(
                        h_flex()
                            .gap_1()
                            .child(Indicator::dot().color(status_color))
                            .child(
                                Label::new(status)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .on_click(move |_, window, cx| {
                        open_sandbox(name.clone(), None, OpenMode::Activate, window, cx)
                    })
            }))
    }
}

impl Render for ProjectsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("sands-projects-panel")
            .key_context("ProjectsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .id("projects-panel-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(self.render_projects(cx)),
            )
    }
}

impl Focusable for ProjectsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ProjectsPanel {}

impl Panel for ProjectsPanel {
    fn persistent_name() -> &'static str {
        "SandsProjectsPanel"
    }

    fn panel_key() -> &'static str {
        "SandsProjectsPanel"
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
        px(260.)
    }

    fn initial_size_state(&self, _window: &Window, _cx: &App) -> PanelSizeState {
        PanelSizeState {
            size: None,
            flex: None,
        }
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Server)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Projects")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        ToggleProjectsPanel.boxed_clone()
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }

    fn activation_priority(&self) -> u32 {
        4
    }
}
