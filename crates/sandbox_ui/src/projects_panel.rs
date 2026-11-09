use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Pixels, SharedString, Subscription, Task, WeakEntity, actions, px,
};
use sbx::{SandboxInfo, SandboxStatus};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    OpenMode, Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelSizeState},
};

use crate::{
    connected_sandbox,
    design::{Tone, caption, icon_button, primary_button, status_dot},
    list_sandboxes,
    new_project_modal::{NewProjectModal, agent_icon},
    open_sandbox,
    sbx_problem::{Problem, ProblemFixed, SbxProblemView, problem_view},
};

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

/// The last project list, shared by the Projects panels of all windows and projects.
struct ProjectsCache(Vec<SandboxInfo>);

impl gpui::Global for ProjectsCache {}

enum ProjectList {
    Loading,
    Loaded(Vec<SandboxInfo>),
    Failed(SharedString),
    /// A problem a button fixes, such as a stopped daemon or a missing sign-in.
    Problem(Entity<SbxProblemView>),
}

/// Lists Sands projects. Each project is a Docker Sandbox, so the list comes from `sbx ls`.
pub struct ProjectsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    projects: ProjectList,
    /// The project being opened: switching takes a while, so its row says so.
    opening: Option<String>,
    _refresh: Task<()>,
    _problem_subscription: Option<Subscription>,
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
                    // Every project in a window has its own panel; starting from the last known
                    // list keeps switching projects from flashing a loading state.
                    projects: match cx.try_global::<ProjectsCache>() {
                        Some(cache) => ProjectList::Loaded(cache.0.clone()),
                        None => ProjectList::Loading,
                    },
                    opening: None,
                    _refresh: Task::ready(()),
                    _problem_subscription: None,
                };
                panel.refresh(window, cx);
                panel
            })
        })
    }

    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._refresh = cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(list_sandboxes()).await;
            this.update_in(cx, |this, window, cx| {
                this.projects = match result {
                    Ok(sandboxes) => {
                        cx.set_global(ProjectsCache(sandboxes.clone()));
                        ProjectList::Loaded(sandboxes)
                    }
                    Err(error) => match Problem::of(&error) {
                        Some(problem) => {
                            let view = problem_view(problem, cx);
                            this._problem_subscription = Some(cx.subscribe_in(
                                &view,
                                window,
                                |this, _, _: &ProblemFixed, window, cx| this.refresh(window, cx),
                            ));
                            ProjectList::Problem(view)
                        }
                        None => ProjectList::Failed(error.to_string().into()),
                    },
                };
                cx.notify();
            })
            .log_err();
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = match &self.projects {
            ProjectList::Loaded(sandboxes) => Some(sandboxes.len()),
            _ => None,
        };
        h_flex()
            .h(px(36.))
            .pl_3()
            .pr_1p5()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Label::new("Projects")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .children(count.map(|count| {
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    })),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        icon_button("refresh-projects", IconName::RotateCw, "Refresh")
                            .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                    )
                    .child(
                        icon_button("new-project", IconName::Plus, "New Project").on_click(
                            |_, window, cx| window.dispatch_action(NewProject.boxed_clone(), cx),
                        ),
                    ),
            )
    }

    fn render_projects(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sandboxes = match &self.projects {
            ProjectList::Loading => {
                return v_flex().p_3().child(caption("Loading projects…"));
            }
            ProjectList::Problem(view) => return v_flex().child(view.clone()),
            ProjectList::Failed(message) => {
                return v_flex().p_3().child(
                    Label::new(message.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                );
            }
            ProjectList::Loaded(sandboxes) if sandboxes.is_empty() => {
                return v_flex()
                    .p_4()
                    .gap_3()
                    .items_center()
                    .child(
                        Icon::new(IconName::Server)
                            .size(IconSize::Medium)
                            .color(Color::Muted),
                    )
                    .child(
                        v_flex()
                            .items_center()
                            .child(Label::new("No projects yet").size(LabelSize::Small))
                            .child(caption(
                                "A project is a sandbox with your code and an agent.",
                            )),
                    )
                    .child(
                        primary_button("create-first-project", "New Project").on_click(
                            |_, window, cx| window.dispatch_action(NewProject.boxed_clone(), cx),
                        ),
                    );
            }
            ProjectList::Loaded(sandboxes) => sandboxes,
        };

        let current = self
            .workspace
            .upgrade()
            .and_then(|workspace| connected_sandbox(workspace.read(cx), cx));
        let colors = cx.theme().colors();
        let now = chrono::Utc::now();

        v_flex()
            .p_1p5()
            .gap_px()
            .children(sandboxes.iter().enumerate().map(|(index, sandbox)| {
                let selected = current.as_deref() == Some(sandbox.name.as_str());
                let opening = !selected && self.opening.as_deref() == Some(sandbox.name.as_str());
                let tone = match sandbox.status {
                    SandboxStatus::Running => Tone::Ok,
                    SandboxStatus::Stopped => Tone::Muted,
                    SandboxStatus::Unknown => Tone::Warn,
                };
                let last_used = sandbox
                    .last_used_at
                    .as_deref()
                    .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
                    .map(|time| kubernetes::format_age(time.with_timezone(&chrono::Utc), now));
                let repositories = sandbox
                    .workspaces
                    .iter()
                    .filter_map(|path| std::path::Path::new(path).file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                let mut details = vec![crate::agent_display_name(&sandbox.agent).to_string()];
                details.extend(repositories);
                let name = sandbox.name.clone();
                let hover = colors.ghost_element_hover;
                h_flex()
                    .id(("project", index))
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .items_start()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(colors.ghost_element_selected))
                    .when(!selected, |this| this.hover(move |style| style.bg(hover)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.opening = Some(name.clone());
                        cx.notify();
                        open_sandbox(name.clone(), None, OpenMode::Activate, window, cx)
                    }))
                    .child(div().pt(px(5.)).child(status_dot(tone, cx)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .justify_between()
                                    .child(
                                        Label::new(sandbox.name.clone())
                                            .size(LabelSize::Small)
                                            .weight(if selected {
                                                FontWeight::SEMIBOLD
                                            } else {
                                                FontWeight::MEDIUM
                                            })
                                            .truncate(),
                                    )
                                    .children(last_used.filter(|_| !opening).map(|age| {
                                        Label::new(age).size(LabelSize::XSmall).color(Color::Muted)
                                    })),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .min_w_0()
                                    .child(
                                        Icon::new(agent_icon(&sandbox.agent))
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(if opening {
                                            "Connecting…".to_string()
                                        } else {
                                            details.join(" · ")
                                        })
                                        .size(LabelSize::XSmall)
                                        .color(if opening { Color::Accent } else { Color::Muted })
                                        .truncate(),
                                    ),
                            ),
                    )
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
