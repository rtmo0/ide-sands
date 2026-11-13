use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use fs::{Fs, RemoveOptions};
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Task, WeakEntity,
};
use sbx::{SandboxInfo, Sbx};
use ui::{Checkbox, Headline, HeadlineSize, ToggleState, prelude::*};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::{
    connections,
    design::{caption, primary_button, secondary_button},
    projects_panel::ProjectsPanel,
    settings_review,
};

/// A repository inside the sandbox and the work in it that exists nowhere else.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SandboxRepository {
    path: String,
    uncommitted: usize,
    /// Commits on local branches that no remote branch contains.
    unpushed: usize,
}

const UNSAVED_WORK_SCRIPT: &str = r#"workspace="$1"
for directory in "$workspace" "$workspace"/*/; do
  directory=${directory%/}
  [ -e "$directory/.git" ] || continue
  uncommitted=$(git -C "$directory" status --porcelain 2>/dev/null | wc -l)
  unpushed=$(git -C "$directory" log --branches --not --remotes --oneline 2>/dev/null | wc -l)
  printf 'repository\t%s\t%s\t%s\n' "$directory" "$uncommitted" "$unpushed"
done"#;

fn parse_unsaved_work(output: &str) -> Vec<SandboxRepository> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            if fields.next()? != "repository" {
                return None;
            }
            let path = fields.next()?.to_string();
            let uncommitted = fields.next()?.trim().parse().ok()?;
            let unpushed = fields.next()?.trim().parse().ok()?;
            Some(SandboxRepository {
                path,
                uncommitted,
                unpushed,
            })
        })
        .collect()
}

async fn unsaved_work(name: String) -> Result<Vec<SandboxRepository>> {
    let sbx = Sbx::locate()?;
    let workspace = sbx.default_directory(&name).await?;
    let output = sbx
        .exec(
            &name,
            &[
                "bash",
                "-c",
                UNSAVED_WORK_SCRIPT,
                "check",
                workspace.as_str(),
            ],
        )
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "checking the repositories failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(parse_unsaved_work(&String::from_utf8_lossy(&output.stdout)))
}

fn sandbox_remote(name: &str) -> String {
    format!("sandbox-{name}")
}

async fn git(directory: &PathBuf, arguments: &[&str]) -> Result<std::process::Output> {
    util::command::new_command("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .await
        .with_context(|| format!("running git in {}", directory.display()))
}

async fn has_sandbox_remote(directory: &PathBuf, name: &str) -> bool {
    git(directory, &["remote", "get-url", &sandbox_remote(name)])
        .await
        .is_ok_and(|output| output.status.success())
}

/// Copies the sandbox's branches into the host repository as `sands/<project>/<branch>`, so they
/// survive the sandbox and its `sandbox-<project>` remote.
async fn bring_to_host(name: String, host_repositories: Vec<PathBuf>) -> Result<usize> {
    let mut fetched = 0;
    for directory in &host_repositories {
        if !has_sandbox_remote(directory, &name).await {
            continue;
        }
        let refspec = format!("+refs/heads/*:refs/heads/sands/{name}/*");
        let output = git(directory, &["fetch", &sandbox_remote(&name), &refspec]).await?;
        anyhow::ensure!(
            output.status.success(),
            "fetching into {} failed: {}",
            directory.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        fetched += 1;
    }
    Ok(fetched)
}

struct Deletion {
    name: String,
    host_repositories: Vec<PathBuf>,
    trash: Vec<PathBuf>,
}

/// Removes the sandbox and what the host keeps for it. Folders to trash are left to the caller,
/// which has the file system.
async fn delete(deletion: &Deletion) -> Result<()> {
    let sbx = Sbx::locate()?;
    sbx.remove(&deletion.name).await?;
    for directory in &deletion.host_repositories {
        if deletion.trash.contains(directory)
            || !has_sandbox_remote(directory, &deletion.name).await
        {
            continue;
        }
        let output = git(
            directory,
            &["remote", "remove", &sandbox_remote(&deletion.name)],
        )
        .await?;
        anyhow::ensure!(
            output.status.success(),
            "removing the sandbox remote from {} failed: {}",
            directory.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    sands_project::forget(&deletion.name)
}

async fn trash(directories: &[PathBuf], fs: Arc<dyn Fs>) -> Result<()> {
    for directory in directories {
        fs.trash(
            directory,
            RemoveOptions {
                recursive: true,
                ignore_if_not_exists: true,
            },
        )
        .await
        .with_context(|| format!("moving {} to the Trash", directory.display()))?;
    }
    Ok(())
}

enum Check {
    Running,
    Done(Vec<SandboxRepository>),
    Failed(SharedString),
}

enum DeleteState {
    Idle,
    Working(SharedString),
    Failed(SharedString),
}

pub(crate) struct DeleteProjectModal {
    name: String,
    host_repositories: Vec<PathBuf>,
    trash: Vec<bool>,
    check: Check,
    fetched: Option<usize>,
    state: DeleteState,
    confirmation: Entity<InputField>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    _task: Option<Task<()>>,
    _check: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl DeleteProjectModal {
    pub(crate) fn new(
        sandbox: &SandboxInfo,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = sandbox.name.clone();
        let host_repositories = sandbox
            .workspaces
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let confirmation = cx
            .new(|cx| InputField::new(window, cx, &name).label("Type the project name to confirm"));
        let _subscriptions = vec![cx.observe(&confirmation, |_, _, cx| cx.notify())];
        let check = cx.spawn({
            let name = name.clone();
            async move |this, cx| {
                let result = cx.background_spawn(unsaved_work(name)).await;
                this.update(cx, |this, cx| {
                    this.check = match result {
                        Ok(repositories) => Check::Done(repositories),
                        Err(error) => Check::Failed(format!("{error:#}").into()),
                    };
                    cx.notify();
                })
                .log_err();
            }
        });
        Self {
            trash: vec![false; host_repositories.len()],
            name,
            host_repositories,
            check: Check::Running,
            fetched: None,
            state: DeleteState::Idle,
            confirmation,
            workspace,
            focus_handle: cx.focus_handle(),
            _task: None,
            _check: check,
            _subscriptions,
        }
    }

    fn busy(&self) -> bool {
        matches!(self.state, DeleteState::Working(_))
    }

    fn confirmed(&self, cx: &App) -> bool {
        self.confirmation.read(cx).text(cx).trim() == self.name
    }

    fn bring_to_host(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.state = DeleteState::Working("Bringing the sandbox branches to your Mac…".into());
        cx.notify();
        let name = self.name.clone();
        let host_repositories = self.host_repositories.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(bring_to_host(name, host_repositories))
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(fetched) => {
                        this.fetched = Some(fetched);
                        this.state = DeleteState::Idle;
                    }
                    Err(error) => this.state = DeleteState::Failed(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() || !self.confirmed(cx) {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let fs = workspace.read(cx).app_state().fs.clone();
        let deletion = Deletion {
            name: self.name.clone(),
            host_repositories: self.host_repositories.clone(),
            trash: self
                .host_repositories
                .iter()
                .zip(&self.trash)
                .filter(|(_, trash)| **trash)
                .map(|(directory, _)| directory.clone())
                .collect(),
        };
        self.state = DeleteState::Working(format!("Deleting {}…", self.name).into());
        cx.notify();
        let name = self.name.clone();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    delete(&deletion).await?;
                    trash(&deletion.trash, fs).await
                })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    connections::set_kubernetes(&name, Vec::new(), cx);
                    settings_review::set_pending_count(&name, 0, cx);
                    if let Some(workspace) = this.workspace.upgrade()
                        && let Some(panel) = workspace.read(cx).panel::<ProjectsPanel>(cx)
                    {
                        panel.update(cx, |panel, cx| panel.refresh(window, cx));
                    }
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.state = DeleteState::Failed(format!("{error:#}").into());
                    cx.notify();
                }
            })
            .log_err();
        }));
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if !self.busy() {
            cx.emit(DismissEvent);
        }
    }

    fn render_unsaved_work(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.check {
            Check::Running => {
                caption("Looking for work that exists only in the sandbox…").into_any_element()
            }
            Check::Failed(message) => Label::new(format!(
                "Couldn't check the repositories: {message}. Anything not pushed will be lost."
            ))
            .size(LabelSize::Small)
            .color(Color::Warning)
            .into_any_element(),
            Check::Done(repositories) => {
                let at_risk = repositories
                    .iter()
                    .filter(|repository| repository.uncommitted > 0 || repository.unpushed > 0)
                    .collect::<Vec<_>>();
                if at_risk.is_empty() {
                    return h_flex()
                        .gap_2()
                        .child(
                            Icon::new(IconName::Check)
                                .size(IconSize::Small)
                                .color(Color::Success),
                        )
                        .child(caption(
                            "Everything in the sandbox is committed and pushed.",
                        ))
                        .into_any_element();
                }
                let colors = cx.theme().colors();
                let any_unpushed = at_risk.iter().any(|repository| repository.unpushed > 0);
                v_flex()
                    .p_3()
                    .gap_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border_variant)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Small)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new("This work exists only in the sandbox")
                                    .size(LabelSize::Small)
                                    .weight(gpui::FontWeight::MEDIUM),
                            ),
                    )
                    .children(at_risk.iter().map(|repository| {
                        let mut parts = Vec::new();
                        if repository.uncommitted > 0 {
                            parts.push(format!("{} uncommitted file(s)", repository.uncommitted));
                        }
                        if repository.unpushed > 0 {
                            parts.push(format!("{} unpushed commit(s)", repository.unpushed));
                        }
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(repository.path.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx),
                            )
                            .child(caption(parts.join(", ")))
                    }))
                    .when(any_unpushed && !self.host_repositories.is_empty(), |this| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .child(
                                    secondary_button("bring-to-host", "Bring branches to my Mac")
                                        .disabled(self.busy())
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.bring_to_host(cx)),
                                        ),
                                )
                                .children(self.fetched.map(|fetched| {
                                    caption(format!(
                                        "Copied into {fetched} repositor{} as sands/{}/…",
                                        if fetched == 1 { "y" } else { "ies" },
                                        self.name
                                    ))
                                })),
                        )
                    })
                    .when(
                        at_risk.iter().any(|repository| repository.uncommitted > 0),
                        |this| {
                            this.child(caption(
                            "Uncommitted files can't be brought over. Commit them in the sandbox \
                             first if you need them.",
                        ))
                        },
                    )
                    .into_any_element()
            }
        }
    }
}

impl ModalView for DeleteProjectModal {}

impl EventEmitter<DismissEvent> for DeleteProjectModal {}

impl Focusable for DeleteProjectModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.confirmation.focus_handle(cx)
    }
}

impl Render for DeleteProjectModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.busy();
        let confirmed = self.confirmed(cx);
        let unsaved_work = self.render_unsaved_work(cx);
        let status = match &self.state {
            DeleteState::Idle => None,
            DeleteState::Working(message) => Some(caption(message.clone()).into_any_element()),
            DeleteState::Failed(message) => Some(
                Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element(),
            ),
        };
        let trash_rows = self
            .host_repositories
            .iter()
            .enumerate()
            .map(|(index, directory)| {
                let checked = self.trash.get(index).copied().unwrap_or(false);
                Checkbox::new(
                    ("trash-repository", index),
                    if checked {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label(format!("Move {} to the Trash", directory.display()))
                .label_size(LabelSize::Small)
                .disabled(busy)
                .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                    if let Some(slot) = this.trash.get_mut(index) {
                        *slot = *state == ToggleState::Selected;
                        cx.notify();
                    }
                }))
                .into_any_element()
            })
            .collect::<Vec<_>>();
        let colors = cx.theme().colors();

        v_flex()
            .key_context("DeleteProjectModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(36.))
            .child(
                v_flex()
                    .px_5()
                    .pt_4()
                    .pb_3()
                    .gap_1()
                    .child(
                        Headline::new(format!("Delete project {}", self.name))
                            .size(HeadlineSize::Small),
                    )
                    .child(caption(
                        "The sandbox goes away with everything in it: clones, toolchains, agents \
                         and their settings, the project's secrets, network rules and approved \
                         .sands settings. Repositories on GitHub and other servers stay.",
                    )),
            )
            .child(
                v_flex()
                    .px_5()
                    .py_3()
                    .gap_3()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(unsaved_work)
                    .when(!trash_rows.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new("Repositories on your Mac")
                                        .size(LabelSize::Small)
                                        .weight(gpui::FontWeight::MEDIUM),
                                )
                                .child(caption(
                                    "Kept unless you tick them. Ticked folders go to the Trash, \
                                     so you can still restore them.",
                                ))
                                .children(trash_rows),
                        )
                    })
                    .child(self.confirmation.clone()),
            )
            .child(
                h_flex()
                    .px_5()
                    .py_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(div().flex_1().min_w_0().children(status))
                    .child(
                        Button::new("cancel-delete-project", "Cancel")
                            .disabled(busy)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        primary_button("delete-project", "Delete Project")
                            .disabled(busy || !confirmed)
                            .on_click(cx.listener(|this, _, window, cx| this.delete(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates and deletes a real sandbox: `cargo test -p sandbox_ui -- --ignored`.
    #[test]
    #[ignore]
    fn deletes_a_project_and_keeps_its_branches() -> crate::new_project_modal::tests::TestResult {
        use crate::new_project_modal::{
            ProjectSource,
            tests::{init_repository, plan},
        };

        let name = "sands-test-delete";
        let root = std::env::temp_dir().join("sands-test-delete-repository");
        init_repository(&root)?;
        smol::block_on(async {
            let sbx = Sbx::locate()?;
            let directory = crate::provisioning::provision(&plan(
                name,
                vec![ProjectSource::LocalRepository(root.clone())],
            ))
            .await?;
            let commit = sbx
                .exec(
                    name,
                    &[
                        "sh",
                        "-c",
                        "cd \"$1\" && echo agent > agent.txt && git add agent.txt && git -c user.name=Agent -c user.email=agent@example.com commit -qm 'Agent work'",
                        "sh",
                        directory.as_str(),
                    ],
                )
                .await?;
            assert!(
                commit.status.success(),
                "{}",
                String::from_utf8_lossy(&commit.stderr)
            );

            let repositories = unsaved_work(name.to_string()).await?;
            let primary = repositories
                .iter()
                .find(|repository| repository.path == directory)
                .ok_or("the workspace repository was not found")?;
            assert!(primary.unpushed >= 1, "{repositories:?}");

            let fetched = bring_to_host(name.to_string(), vec![root.clone()]).await?;
            let branches = git(&root, &["branch", "--list", "sands/*"]).await?;
            delete(&Deletion {
                name: name.to_string(),
                host_repositories: vec![root.clone()],
                trash: Vec::new(),
            })
            .await?;
            let still_listed = sbx.get(name).await.is_ok();
            let remote_left = has_sandbox_remote(&root, name).await;
            std::fs::remove_dir_all(&root)?;

            assert_eq!(fetched, 1);
            assert!(
                String::from_utf8_lossy(&branches.stdout).contains(&format!("sands/{name}/")),
                "{}",
                String::from_utf8_lossy(&branches.stdout)
            );
            assert!(!still_listed);
            assert!(!remote_left);
            Ok(())
        })
    }

    #[test]
    fn unsaved_work_is_parsed() {
        let output = "repository\t/home/agent/workspace\t0\t2\nrepository\t/home/agent/workspace/web\t       3\t0\nnoise\n";
        assert_eq!(
            parse_unsaved_work(output),
            [
                SandboxRepository {
                    path: "/home/agent/workspace".to_string(),
                    uncommitted: 0,
                    unpushed: 2,
                },
                SandboxRepository {
                    path: "/home/agent/workspace/web".to_string(),
                    uncommitted: 3,
                    unpushed: 0,
                },
            ]
        );
    }
}
