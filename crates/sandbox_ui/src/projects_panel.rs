use std::path::{Path, PathBuf};

use file_icons::FileIcons;
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, Pixels, SharedString, Subscription, Task, WeakEntity,
    actions, px,
};
use sbx::{SandboxInfo, SandboxStatus};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    OpenMode, Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelSizeState},
};

use crate::{
    connected_sandbox, connections,
    delete_project::DeleteProjectModal,
    design::{Tone, caption, icon_button, primary_button, status_dot},
    list_sandboxes,
    new_project_modal::{NewProjectModal, agent_icon},
    open_sandbox,
    sbx_problem::{Problem, ProblemFixed, SbxProblemView, problem_view},
    settings_review,
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

/// A language of a project: its name and a file name whose icon stands for it.
type ProjectLanguage = (&'static str, &'static str);

#[derive(Clone)]
struct Project {
    info: SandboxInfo,
    languages: Vec<ProjectLanguage>,
}

/// The last project list, shared by the Projects panels of all windows and projects.
struct ProjectsCache(Vec<Project>);

impl gpui::Global for ProjectsCache {}

enum ProjectList {
    Loading,
    Loaded(Vec<Project>),
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
    _connections_subscription: Subscription,
    _settings_review_subscription: Subscription,
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
                    _connections_subscription: connections::observe(cx),
                    _settings_review_subscription: settings_review::observe(cx),
                };
                panel.refresh(window, cx);
                panel
            })
        })
    }

    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._refresh = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let sandboxes = list_sandboxes().await?;
                    Ok(sandboxes
                        .into_iter()
                        .map(|info| Project {
                            languages: detect_languages(&info.workspaces),
                            info,
                        })
                        .collect::<Vec<_>>())
                })
                .await;
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
            ProjectList::Loaded(projects) => Some(projects.len()),
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
            ProjectList::Loaded(projects) => projects,
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
            .children(sandboxes.iter().enumerate().map(|(index, project)| {
                let sandbox = &project.info;
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
                let labels = project_labels(project, cx);
                let to_approve = settings_review::pending_count(&sandbox.name, cx);
                let name = sandbox.name.clone();
                let delete_info = sandbox.clone();
                let workspace = self.workspace.clone();
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
                    .group("project-row")
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
                                    .when(to_approve > 0, |this| {
                                        this.child(
                                            Label::new(format!("{to_approve} to approve"))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Warning),
                                        )
                                    })
                                    .when(to_approve == 0, |this| {
                                        this.children(last_used.filter(|_| !opening).map(|age| {
                                            Label::new(age)
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                        }))
                                    }),
                            )
                            .when(opening || !repositories.is_empty(), |this| {
                                this.child(
                                    Label::new(if opening {
                                        "Connecting…".to_string()
                                    } else {
                                        repositories.join(" · ")
                                    })
                                    .size(LabelSize::XSmall)
                                    .color(if opening { Color::Accent } else { Color::Muted })
                                    .truncate(),
                                )
                            })
                            .child(h_flex().pt_0p5().gap_1().flex_wrap().children(labels)),
                    )
                    .child(
                        div()
                            .invisible()
                            .group_hover("project-row", |style| style.visible())
                            .child(
                                icon_button(
                                    ("delete-project", index),
                                    IconName::Trash,
                                    if selected {
                                        "Open another project to delete this one"
                                    } else {
                                        "Delete project…"
                                    },
                                )
                                .disabled(selected)
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    let Some(workspace) = workspace.upgrade() else {
                                        return;
                                    };
                                    let sandbox = delete_info.clone();
                                    workspace.update(cx, |workspace, cx| {
                                        let weak_workspace = cx.entity().downgrade();
                                        workspace.toggle_modal(window, cx, |window, cx| {
                                            DeleteProjectModal::new(
                                                &sandbox,
                                                weak_workspace,
                                                window,
                                                cx,
                                            )
                                        });
                                    });
                                }),
                            ),
                    )
            }))
    }
}

/// What a project consists of and is connected to, as small labels: the agent, the languages of
/// its repositories and the Kubernetes clusters of its sandbox.
fn project_labels(project: &Project, cx: &App) -> Vec<AnyElement> {
    let mut labels = vec![label(
        Icon::new(agent_icon(&project.info.agent)).color(Color::Muted),
        crate::agent_display_name(&project.info.agent),
        cx,
    )];
    for (language, sample_file) in &project.languages {
        let icon = match FileIcons::get_icon(Path::new(sample_file), cx) {
            Some(path) => Icon::from_path(path),
            None => Icon::new(IconName::FileCode).color(Color::Muted),
        };
        labels.push(label(icon, language, cx));
    }
    let connected = connections::connections(&project.info.name, cx);
    if !connected.kubernetes.is_empty() {
        labels.push(label(
            Icon::new(IconName::Kubernetes).color(Color::Muted),
            &connected.kubernetes.join(", "),
            cx,
        ));
    }
    labels
}

fn label(icon: Icon, text: &str, cx: &App) -> AnyElement {
    h_flex()
        .flex_none()
        .h(px(18.))
        .px_1()
        .gap_1()
        .rounded_sm()
        .bg(cx.theme().colors().element_background)
        .child(icon.size(IconSize::XSmall))
        .child(
            Label::new(text.to_string())
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .single_line(),
        )
        .into_any_element()
}

/// Finds the languages of a project from the source files of its repositories on the host, most
/// files first. A Flutter app lists its platform code too: Kotlin and Java for Android, Swift and
/// Objective-C for iOS and macOS, C++ for the desktop runners. Dependencies, build output and
/// generated platform files are skipped, and the walk is bounded so huge trees stay cheap.
fn detect_languages(workspaces: &[String]) -> Vec<ProjectLanguage> {
    const EXTENSIONS: [(&str, ProjectLanguage); 31] = [
        ("go", ("Go", "main.go")),
        ("rs", ("Rust", "main.rs")),
        ("ts", ("TypeScript", "index.ts")),
        ("tsx", ("TypeScript", "index.ts")),
        ("mts", ("TypeScript", "index.ts")),
        ("js", ("JavaScript", "index.js")),
        ("jsx", ("JavaScript", "index.js")),
        ("mjs", ("JavaScript", "index.js")),
        ("py", ("Python", "main.py")),
        ("dart", ("Dart", "main.dart")),
        ("kt", ("Kotlin", "Main.kt")),
        ("kts", ("Kotlin", "Main.kt")),
        ("java", ("Java", "Main.java")),
        ("swift", ("Swift", "main.swift")),
        ("m", ("Objective-C", "main.m")),
        ("mm", ("Objective-C", "main.m")),
        ("c", ("C", "main.c")),
        ("cc", ("C++", "main.cpp")),
        ("cpp", ("C++", "main.cpp")),
        ("cxx", ("C++", "main.cpp")),
        ("hpp", ("C++", "main.cpp")),
        ("cs", ("C#", "Program.cs")),
        ("rb", ("Ruby", "main.rb")),
        ("php", ("PHP", "index.php")),
        ("scala", ("Scala", "Main.scala")),
        ("ex", ("Elixir", "main.ex")),
        ("exs", ("Elixir", "main.ex")),
        ("lua", ("Lua", "main.lua")),
        ("zig", ("Zig", "main.zig")),
        ("hs", ("Haskell", "Main.hs")),
        ("sh", ("Shell", "script.sh")),
    ];
    const FLUTTER: ProjectLanguage = ("Flutter", "main.dart");
    const MAX_DEPTH: usize = 12;
    const MAX_DIRECTORIES: usize = 5000;
    const SKIPPED: [&str; 16] = [
        "node_modules",
        "vendor",
        "target",
        "build",
        "dist",
        "out",
        "Pods",
        "DerivedData",
        "ephemeral",
        "Flutter",
        "generated",
        "third_party",
        "__pycache__",
        "venv",
        "env",
        "site-packages",
    ];

    let mut counts: Vec<(ProjectLanguage, usize)> = Vec::new();
    let mut is_flutter = false;
    let mut queue = workspaces
        .iter()
        .map(|workspace| (PathBuf::from(workspace), 0))
        .collect::<std::collections::VecDeque<_>>();
    let mut visited = 0;
    while let Some((directory, depth)) = queue.pop_front() {
        visited += 1;
        if visited > MAX_DIRECTORIES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if depth < MAX_DEPTH
                    && !file_name.starts_with('.')
                    && !SKIPPED.contains(&file_name.as_ref())
                {
                    queue.push_back((entry.path(), depth + 1));
                }
                continue;
            }
            if file_name == "pubspec.yaml"
                && std::fs::read_to_string(entry.path())
                    .is_ok_and(|pubspec| pubspec.contains("sdk: flutter"))
            {
                is_flutter = true;
            }
            let Some(extension) = Path::new(file_name.as_ref())
                .extension()
                .and_then(|extension| extension.to_str())
            else {
                continue;
            };
            let Some(language) = EXTENSIONS
                .iter()
                .find(|(candidate, _)| *candidate == extension)
                .map(|(_, language)| *language)
            else {
                continue;
            };
            match counts.iter_mut().find(|(found, _)| *found == language) {
                Some((_, count)) => *count += 1,
                None => counts.push((language, 1)),
            }
        }
    }
    // Configuration scripts of a TypeScript project are JavaScript; it is not a language of it.
    if counts.iter().any(|((name, _), _)| *name == "TypeScript") {
        counts.retain(|((name, _), _)| *name != "JavaScript");
    }
    counts.sort_by(|(_, left), (_, right)| right.cmp(left));
    counts
        .into_iter()
        .map(|(language, _)| {
            if is_flutter && language.0 == "Dart" {
                FLUTTER
            } else {
                language
            }
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_every_language_of_a_flutter_app_and_skips_dependencies() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let write = |path: &str, contents: &str| -> anyhow::Result<()> {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, contents)?;
            Ok(())
        };
        write("apps/backend/iam/go.mod", "module iam")?;
        write("apps/backend/iam/cmd/server/main.go", "")?;
        write("apps/backend/iam/server.go", "")?;
        write("apps/backend/iam/handler.go", "")?;
        write("apps/mobile/pubspec.yaml", "dependencies:\n  flutter:\n    sdk: flutter\n")?;
        write("apps/mobile/lib/main.dart", "")?;
        write("apps/mobile/lib/app.dart", "")?;
        write(
            "apps/mobile/android/app/src/main/kotlin/com/example/app/MainActivity.kt",
            "",
        )?;
        write("apps/mobile/ios/Runner/AppDelegate.swift", "")?;
        write("apps/mobile/ios/Runner/GeneratedPluginRegistrant.m", "")?;
        write("apps/mobile/ios/Pods/Library/Thing.swift", "")?;
        write("web/node_modules/left-pad/index.rs", "")?;
        write("web/src/app.ts", "")?;
        write("web/eslint.config.js", "")?;

        let languages = detect_languages(&[root.to_string_lossy().into_owned()]);
        let mut names = languages.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        assert_eq!(names.first(), Some(&"Go"));
        names.sort_unstable();
        assert_eq!(
            names,
            ["Flutter", "Go", "Kotlin", "Objective-C", "Swift", "TypeScript"]
        );
        Ok(())
    }
}
