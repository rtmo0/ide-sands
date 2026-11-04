use std::path::PathBuf;

use anyhow::Context as _;
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    PathPromptOptions, SharedString, Task, WeakEntity,
};
use sbx::{CreateOptions, Sbx};
use ui::{
    Button, ButtonCommon as _, ButtonStyle, Clickable as _, Color, Context, Disableable as _,
    Headline, HeadlineSize, InteractiveElement, IntoElement, Label, LabelCommon as _, LabelSize,
    ParentElement, Render, Styled, StyledExt, ToggleButtonGroup, ToggleButtonGroupStyle,
    ToggleButtonSimple, Window, h_flex, rems, v_flex,
};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{ModalView, OpenMode, Workspace};

use crate::{open_sandbox, projects_panel::ProjectsPanel};

/// Sandboxes for projects run the plain `shell` agent; coding agents are added per project later.
const PROJECT_AGENT: &str = "shell";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    LocalRepository,
    GitUrl,
}

enum ProjectSource {
    LocalRepository(PathBuf),
    GitUrl(String),
}

enum CreationState {
    Editing,
    Creating(SharedString),
    Failed(SharedString),
}

pub(crate) struct NewProjectModal {
    workspace: WeakEntity<Workspace>,
    name: Entity<InputField>,
    source_kind: SourceKind,
    local_path: Entity<InputField>,
    git_url: Entity<InputField>,
    state: CreationState,
    _task: Option<Task<()>>,
}

impl NewProjectModal {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            InputField::new(window, cx, "Defaults to the repository name")
                .label("Project name")
                .tab_index(1)
        });
        let local_path = cx.new(|cx| {
            InputField::new(window, cx, "/path/to/git/repository")
                .label("Local folder")
                .tab_index(2)
        });
        let git_url = cx.new(|cx| {
            InputField::new(window, cx, "https://github.com/owner/repository.git")
                .label("Git URL")
                .tab_index(2)
        });
        Self {
            workspace,
            name,
            source_kind: SourceKind::LocalRepository,
            local_path,
            git_url,
            state: CreationState::Editing,
            _task: None,
        }
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select".into()),
        });
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let selected = match paths.await {
                Ok(Ok(paths)) => paths.and_then(|paths| paths.into_iter().next()),
                Ok(Err(error)) => {
                    this.update(cx, |this, cx| {
                        this.state = CreationState::Failed(format!("{error:#}").into());
                        cx.notify();
                    })
                    .log_err();
                    None
                }
                // The dialog was dismissed without a result.
                Err(_) => None,
            };
            if let Some(path) = selected {
                this.update_in(cx, |this, window, cx| {
                    let path = path.to_string_lossy().into_owned();
                    this.local_path
                        .update(cx, |field, cx| field.set_text(&path, window, cx));
                })
                .log_err();
            }
        }));
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.state, CreationState::Creating(_)) {
            return;
        }
        let source = match self.source_kind {
            SourceKind::LocalRepository => ProjectSource::LocalRepository(PathBuf::from(
                self.local_path.read(cx).text(cx).trim(),
            )),
            SourceKind::GitUrl => {
                ProjectSource::GitUrl(self.git_url.read(cx).text(cx).trim().to_string())
            }
        };
        let name = match project_name(self.name.read(cx).text(cx).trim(), &source) {
            Ok(name) => name,
            Err(message) => {
                self.state = CreationState::Failed(message.into());
                cx.notify();
                return;
            }
        };

        self.state = CreationState::Creating("Creating the sandbox and fetching the code…".into());
        cx.notify();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(create_project(name.clone(), source))
                .await;
            match result {
                Ok(directory) => {
                    set_up_toolchains(&this, &name, cx).await;
                    this.update_in(cx, |this, window, cx| {
                        if let Some(workspace) = this.workspace.upgrade()
                            && let Some(panel) = workspace.read(cx).panel::<ProjectsPanel>(cx)
                        {
                            panel.update(cx, |panel, cx| panel.refresh(window, cx));
                        }
                        cx.emit(DismissEvent);
                        open_sandbox(name, Some(directory), OpenMode::Activate, window, cx);
                    })
                    .log_err();
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.state = CreationState::Failed(format!("{error:#}").into());
                        cx.notify();
                    })
                    .log_err();
                }
            }
        }));
    }
}

/// Installs the toolchains the project's files call for. A failure does not block opening the
/// project: the Sandbox panel lists what is missing and can retry.
async fn set_up_toolchains(
    this: &WeakEntity<NewProjectModal>,
    name: &str,
    cx: &mut gpui::AsyncWindowContext,
) {
    let probe_name = name.to_string();
    let missing = cx
        .background_spawn(async move {
            let sbx = Sbx::locate()?;
            let probe = sbx.probe(&probe_name).await?;
            anyhow::Ok(
                probe
                    .toolchains
                    .into_iter()
                    .filter(|status| status.is_missing())
                    .map(|status| status.toolchain)
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .log_err()
        .unwrap_or_default();
    if missing.is_empty() {
        return;
    }
    let names = missing
        .iter()
        .map(|toolchain| toolchain.display_name())
        .collect::<Vec<_>>()
        .join(", ");
    this.update(cx, |this, cx| {
        this.state = CreationState::Creating(
            format!("Installing {names}. This can take a few minutes…").into(),
        );
        cx.notify();
    })
    .log_err();
    let install_name = name.to_string();
    cx.background_spawn(async move {
        let sbx = Sbx::locate()?;
        sbx.install_toolchains(&install_name, &missing).await?;
        anyhow::Ok(())
    })
    .await
    .log_err();
}

impl NewProjectModal {
    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.create(window, cx);
    }

    fn render_source(&self, cx: &mut Context<Self>) -> impl IntoElement {
        match self.source_kind {
            SourceKind::LocalRepository => v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_end()
                        .child(v_flex().flex_1().child(self.local_path.clone()))
                        .child(
                            Button::new("browse-local-folder", "Browse…")
                                .style(ButtonStyle::Outlined)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.browse(window, cx)),
                                ),
                        ),
                )
                .child(
                    Label::new(
                        "Must be a Git repository. The sandbox works on its own clone; its \
                         commits appear in this repository as the sandbox-<name> remote.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
            SourceKind::GitUrl => v_flex().gap_1().child(self.git_url.clone()).child(
                Label::new(
                    "Cloned inside the sandbox. Private repositories need `sbx secret set github`.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            ),
        }
    }
}

/// Creates the sandbox, brings the code into it and returns the directory to open.
async fn create_project(name: String, source: ProjectSource) -> anyhow::Result<String> {
    let sbx = Sbx::locate()?;
    sbx.check().await?;
    match source {
        ProjectSource::LocalRepository(path) => {
            anyhow::ensure!(
                path.is_absolute() && path.is_dir(),
                "{} is not a folder",
                path.display()
            );
            anyhow::ensure!(
                path.join(".git").exists(),
                "{} is not a Git repository. Run `git init` in it first.",
                path.display()
            );
            sbx.create(&CreateOptions {
                name: name.clone(),
                agent: PROJECT_AGENT.to_string(),
                workspaces: vec![path],
                clone_workspace: true,
                ..Default::default()
            })
            .await?;
            Ok(sbx.default_directory(&name).await?)
        }
        ProjectSource::GitUrl(url) => {
            let directory_name = repository_directory_name(&url)
                .with_context(|| format!("cannot derive a folder name from {url:?}"))?;
            sbx.create(&CreateOptions {
                name: name.clone(),
                agent: PROJECT_AGENT.to_string(),
                ..Default::default()
            })
            .await?;
            let target = format!("{}/{directory_name}", sbx.default_directory(&name).await?);
            let output = sbx
                .exec(
                    &name,
                    &["git", "clone", "--", url.as_str(), target.as_str()],
                )
                .await?;
            if output.status.success() {
                return Ok(target);
            }
            let clone_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            // The sandbox was created for this clone only, so remove it instead of leaving an
            // empty project behind.
            match sbx.remove(&name).await {
                Ok(()) => anyhow::bail!("git clone failed: {clone_error}"),
                Err(remove_error) => anyhow::bail!(
                    "git clone failed: {clone_error}. Removing sandbox {name} also failed: {remove_error}"
                ),
            }
        }
    }
}

/// Uses the typed name, or derives one from the source, and checks it against `sbx` naming rules.
fn project_name(typed: &str, source: &ProjectSource) -> Result<String, String> {
    let name = if typed.is_empty() {
        let derived = match source {
            ProjectSource::LocalRepository(path) if path.as_os_str().is_empty() => {
                return Err("Choose a local folder".to_string());
            }
            ProjectSource::GitUrl(url) if url.is_empty() => {
                return Err("Enter a Git URL".to_string());
            }
            ProjectSource::LocalRepository(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            ProjectSource::GitUrl(url) => repository_directory_name(url),
        };
        derived
            .map(|name| sanitize_name(&name))
            .ok_or_else(|| "Enter a project name".to_string())?
    } else {
        typed.to_string()
    };
    validate_name(&name)?;
    Ok(name)
}

/// `sbx` names: at least two characters, starting with a letter or digit, then letters, digits,
/// hyphens and periods. `default` is reserved.
fn validate_name(name: &str) -> Result<(), String> {
    let starts_alphanumeric = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric());
    let allowed = name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '.'));
    if name.len() < 2 || !starts_alphanumeric || !allowed || name == "default" {
        return Err(format!(
            "{name:?} is not a valid project name: use at least two letters, digits, hyphens or \
             periods, starting with a letter or digit"
        ));
    }
    Ok(())
}

fn sanitize_name(raw: &str) -> String {
    let replaced: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect();
    replaced
        .trim_start_matches(|character: char| !character.is_ascii_alphanumeric())
        .to_string()
}

/// The folder `git clone` would create for `url`, restricted to safe characters.
fn repository_directory_name(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed.rsplit(['/', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        });
    safe.then(|| name.to_string())
}

impl ModalView for NewProjectModal {}

impl EventEmitter<DismissEvent> for NewProjectModal {}

impl Focusable for NewProjectModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for NewProjectModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let creating = matches!(self.state, CreationState::Creating(_));
        let status = match &self.state {
            CreationState::Editing => None,
            CreationState::Creating(message) => Some(
                Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
            CreationState::Failed(message) => Some(
                Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            ),
        };

        v_flex()
            .key_context("NewProjectModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(34.))
            .p_4()
            .gap_3()
            .child(Headline::new("New Project").size(HeadlineSize::Small))
            .child(self.name.clone())
            .child(
                ToggleButtonGroup::single_row(
                    "project-source",
                    [
                        ToggleButtonSimple::new(
                            "Local Folder",
                            cx.listener(|this, _, _, cx| {
                                this.source_kind = SourceKind::LocalRepository;
                                cx.notify();
                            }),
                        ),
                        ToggleButtonSimple::new(
                            "Git Repository",
                            cx.listener(|this, _, _, cx| {
                                this.source_kind = SourceKind::GitUrl;
                                cx.notify();
                            }),
                        ),
                    ],
                )
                .style(ToggleButtonGroupStyle::Outlined)
                .auto_width()
                .selected_index(match self.source_kind {
                    SourceKind::LocalRepository => 0,
                    SourceKind::GitUrl => 1,
                }),
            )
            .child(self.render_source(cx))
            .children(status)
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel-new-project", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("create-project", "Create")
                            .style(ButtonStyle::Filled)
                            .disabled(creating)
                            .on_click(cx.listener(|this, _, window, cx| this.create(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Creates and removes real sandboxes: `cargo test -p sandbox_ui -- --ignored`.
    #[test]
    #[ignore]
    fn creates_projects_with_real_sbx() -> TestResult {
        smol::block_on(async {
            let sbx = Sbx::locate()?;

            let directory = create_project(
                "sands-test-git".to_string(),
                ProjectSource::GitUrl("https://github.com/octocat/Hello-World.git".to_string()),
            )
            .await;
            let listing = sbx
                .exec(
                    "sands-test-git",
                    &["ls", directory.as_deref().unwrap_or("/")],
                )
                .await;
            sbx.remove("sands-test-git").await?;
            let directory = directory?;
            assert!(directory.ends_with("/Hello-World"), "{directory}");
            assert!(String::from_utf8_lossy(&listing?.stdout).contains("README"));

            let repository = std::env::temp_dir().join("sands-test-local-repository");
            if repository.exists() {
                std::fs::remove_dir_all(&repository)?;
            }
            std::fs::create_dir_all(&repository)?;
            std::fs::write(repository.join("README.md"), "local\n")?;
            let initialized = util::command::new_command("git")
                .args(["init", "-q"])
                .current_dir(&repository)
                .status()
                .await?;
            assert!(initialized.success());
            let repository = repository.canonicalize()?;

            let directory = create_project(
                "sands-test-local".to_string(),
                ProjectSource::LocalRepository(repository.clone()),
            )
            .await;
            sbx.remove("sands-test-local").await?;
            std::fs::remove_dir_all(&repository)?;
            assert_eq!(directory?, repository.to_string_lossy());
            Ok(())
        })
    }

    #[test]
    fn derives_repository_directory_names() {
        assert_eq!(
            repository_directory_name("https://github.com/rtmo0/ide-sands.git").as_deref(),
            Some("ide-sands")
        );
        assert_eq!(
            repository_directory_name("git@github.com:rtmo0/ide-sands").as_deref(),
            Some("ide-sands")
        );
        assert_eq!(
            repository_directory_name("https://example.com/group/repo/").as_deref(),
            Some("repo")
        );
        assert_eq!(repository_directory_name("https://example.com/.."), None);
        assert_eq!(repository_directory_name("https://example.com/a b"), None);
        assert_eq!(repository_directory_name(""), None);
    }

    #[test]
    fn validates_names() {
        assert!(validate_name("spike").is_ok());
        assert!(validate_name("my-project.v2").is_ok());
        assert!(validate_name("x").is_err());
        assert!(validate_name("-project").is_err());
        assert!(validate_name("my project").is_err());
        assert!(validate_name("default").is_err());
    }

    #[test]
    fn derives_project_names_from_sources() {
        assert_eq!(
            project_name(
                "",
                &ProjectSource::GitUrl("https://github.com/rtmo0/IDE_Sands.git".into())
            ),
            Ok("ide-sands".to_string())
        );
        assert_eq!(
            project_name(
                "",
                &ProjectSource::LocalRepository(PathBuf::from("/work/My Repo"))
            ),
            Ok("my-repo".to_string())
        );
        assert_eq!(
            project_name("custom", &ProjectSource::GitUrl(String::new())),
            Ok("custom".to_string())
        );
        assert!(project_name("", &ProjectSource::GitUrl(String::new())).is_err());
        assert!(project_name("", &ProjectSource::LocalRepository(PathBuf::new())).is_err());
    }
}
