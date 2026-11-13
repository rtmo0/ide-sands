use std::time::Duration;

use anyhow::Result;
use collections::HashMap;
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Global,
    SharedString, Subscription, Task, actions,
};
use sands_project::{ChangeKind, FileChange, Files, PendingChange, Review, Snapshot};
use sbx::Sbx;
use ui::{Headline, HeadlineSize, prelude::*};
use util::ResultExt as _;
use workspace::{
    ModalView, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

use crate::{
    connected_sandbox,
    design::{Tone, badge, caption, primary_button, secondary_button},
};

actions!(
    sands,
    [
        /// Shows the changes to the project's `.sands` settings that wait for approval.
        ReviewSettingsChanges
    ]
);

/// How many `.sands` changes wait for approval in each open project, for the Projects panel.
#[derive(Default)]
pub(crate) struct PendingSettingsChanges(HashMap<String, usize>);

impl Global for PendingSettingsChanges {}

pub(crate) fn pending_count(sandbox: &str, cx: &App) -> usize {
    cx.try_global::<PendingSettingsChanges>()
        .and_then(|pending| pending.0.get(sandbox).copied())
        .unwrap_or(0)
}

pub(crate) fn observe<T: 'static>(cx: &mut gpui::Context<T>) -> Subscription {
    cx.observe_global::<PendingSettingsChanges>(|_, cx| cx.notify())
}

pub(crate) fn set_pending_count(sandbox: &str, count: usize, cx: &mut App) {
    let pending = cx.default_global::<PendingSettingsChanges>();
    if pending.0.get(sandbox).copied().unwrap_or(0) == count {
        return;
    }
    if count == 0 {
        pending.0.remove(sandbox);
    } else {
        pending.0.insert(sandbox.to_string(), count);
    }
}

#[derive(Clone, Debug)]
struct RepositoryReview {
    root: String,
    review: Review,
}

fn count(repositories: &[RepositoryReview]) -> usize {
    repositories
        .iter()
        .map(|repository| repository.review.pending.len())
        .sum()
}

/// Compares each repository's `.sands` with the approved snapshot. Files that need no approval
/// are taken into the snapshot right away.
async fn load(sandbox: String, roots: Vec<String>) -> Result<Vec<RepositoryReview>> {
    let sbx = Sbx::locate()?;
    let mut snapshot = Snapshot::load(&sandbox)?;
    let mut repositories = Vec::new();
    let mut snapshot_changed = false;
    for root in roots {
        let current = sands_project::scan(&sbx, &sandbox, &root).await?;
        let approved = snapshot
            .repositories
            .get(&root)
            .cloned()
            .unwrap_or_default();
        let review = sands_project::review(&approved, &current);
        if !review.applied.is_empty() {
            snapshot.approve(&root, &review.applied);
            snapshot_changed = true;
        }
        repositories.push(RepositoryReview { root, review });
    }
    if snapshot_changed {
        snapshot.save(&sandbox)?;
    }
    Ok(repositories)
}

/// Records files the user just put into `.sands` through Sands itself, such as a migration, as
/// approved. `written` holds paths relative to the repository root; a folder covers its files.
pub(crate) async fn approve_written(
    sbx: &Sbx,
    sandbox: &str,
    root: &str,
    written: &[String],
) -> Result<()> {
    let current = sands_project::scan(sbx, sandbox, root).await?;
    let mut snapshot = Snapshot::load(sandbox)?;
    let approved = snapshot.repositories.get(root).cloned().unwrap_or_default();
    let changes = written_changes(&approved, &current, written);
    snapshot.approve(root, &changes);
    snapshot.save(sandbox)
}

fn written_changes(approved: &Files, current: &Files, written: &[String]) -> Vec<FileChange> {
    let is_written = |path: &str| {
        written.iter().any(|written| {
            path == written
                || path
                    .strip_prefix(written.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    current
        .iter()
        .filter(|(path, state)| is_written(path) && approved.get(*path) != Some(*state))
        .map(|(path, state)| FileChange {
            path: path.clone(),
            before: approved.get(path).cloned(),
            after: Some(state.clone()),
        })
        .collect()
}

fn sandbox_roots(workspace: &Workspace, cx: &App) -> Vec<String> {
    workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
        .collect()
}

/// Rechecks a project's `.sands` shortly after it changes, and tells the user about new
/// changes that wait for approval.
struct Watcher {
    check: Option<Task<()>>,
    notified: usize,
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        workspace.register_action(|workspace, _: &ReviewSettingsChanges, window, cx| {
            open_review(workspace, window, cx);
        });
        if connected_sandbox(workspace, cx).is_none() {
            return;
        }
        let watcher = cx.new(|_| Watcher {
            check: None,
            notified: 0,
        });
        let project = workspace.project().clone();
        cx.subscribe_in(&project, window, {
            let watcher = watcher.clone();
            move |workspace, _, event, window, cx| {
                let touches_settings = match event {
                    project::Event::WorktreeUpdatedEntries(_, changes) => {
                        changes.iter().any(|(path, _, _)| {
                            path.components()
                                .any(|component| component == sands_project::SETTINGS_FOLDER)
                        })
                    }
                    project::Event::WorktreeAdded(_) => true,
                    _ => false,
                };
                if touches_settings {
                    schedule_check(workspace, &watcher, window, cx);
                }
            }
        })
        .detach();
        schedule_check(workspace, &watcher, window, cx);
    })
    .detach();
}

fn schedule_check(
    workspace: &mut Workspace,
    watcher: &Entity<Watcher>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(sandbox) = connected_sandbox(workspace, cx) else {
        return;
    };
    let roots = sandbox_roots(workspace, cx);
    if roots.is_empty() {
        return;
    }
    let task = cx.spawn_in(window, {
        let watcher = watcher.downgrade();
        async move |workspace, cx| {
            // Agents write several files in a row; one check after they settle is enough.
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let repositories = match cx.background_spawn(load(sandbox.clone(), roots)).await {
                Ok(repositories) => repositories,
                Err(error) => {
                    log::error!("checking the .sands settings of {sandbox} failed: {error:#}");
                    return;
                }
            };
            let pending = count(&repositories);
            workspace
                .update_in(cx, |workspace, window, cx| {
                    set_pending_count(&sandbox, pending, cx);
                    let notified = watcher
                        .update(cx, |watcher, _| {
                            std::mem::replace(&mut watcher.notified, pending)
                        })
                        .unwrap_or(0);
                    if pending > 0 && pending != notified {
                        notify(workspace, &sandbox, pending, window, cx);
                    }
                })
                .log_err();
        }
    });
    watcher.update(cx, |watcher, _| watcher.check = Some(task));
}

struct SettingsChangesNotification;

fn notify(
    workspace: &mut Workspace,
    sandbox: &str,
    pending: usize,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let id = NotificationId::composite::<SettingsChangesNotification>(SharedString::from(
        sandbox.to_string(),
    ));
    workspace.show_notification(id, cx, |cx| {
        cx.new(|cx| {
            MessageNotification::new(
                format!(
                    ".sands/ changed: {pending} change{} need{} your approval before agents get \
                     them.",
                    if pending == 1 { "" } else { "s" },
                    if pending == 1 { "s" } else { "" },
                ),
                cx,
            )
            .primary_message("Review")
            .primary_icon(IconName::Check)
            .primary_on_click(|window, cx| {
                window.dispatch_action(Box::new(ReviewSettingsChanges), cx);
                cx.emit(DismissEvent);
            })
        })
    });
}

fn open_review(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(sandbox) = connected_sandbox(workspace, cx) else {
        return;
    };
    let roots = sandbox_roots(workspace, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let repositories = cx.background_spawn(load(sandbox.clone(), roots)).await?;
        workspace.update_in(cx, |workspace, window, cx| {
            set_pending_count(&sandbox, count(&repositories), cx);
            if count(&repositories) == 0 {
                struct NothingToReview;
                workspace.show_notification(
                    NotificationId::unique::<NothingToReview>(),
                    cx,
                    |cx| {
                        cx.new(|cx| {
                            MessageNotification::new(".sands/ matches the approved settings.", cx)
                        })
                    },
                );
                return;
            }
            workspace.toggle_modal(window, cx, |_, cx| {
                SettingsReviewModal::new(sandbox, repositories, cx)
            });
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    Waiting,
    Accepted,
    Rejected,
}

enum ApplyState {
    Editing,
    Applying,
    Failed(SharedString),
}

pub(crate) struct SettingsReviewModal {
    sandbox: String,
    repositories: Vec<RepositoryReview>,
    /// One decision per pending change, in the order of `repositories`.
    decisions: Vec<Vec<Decision>>,
    selected: (usize, usize),
    state: ApplyState,
    focus_handle: FocusHandle,
    _task: Option<Task<()>>,
}

impl SettingsReviewModal {
    fn new(sandbox: String, repositories: Vec<RepositoryReview>, cx: &mut Context<Self>) -> Self {
        let decisions = repositories
            .iter()
            .map(|repository| vec![Decision::Waiting; repository.review.pending.len()])
            .collect();
        let selected = repositories
            .iter()
            .position(|repository| !repository.review.pending.is_empty())
            .map_or((0, 0), |index| (index, 0));
        Self {
            sandbox,
            repositories,
            decisions,
            selected,
            state: ApplyState::Editing,
            focus_handle: cx.focus_handle(),
            _task: None,
        }
    }

    fn decided(&self) -> usize {
        self.decisions
            .iter()
            .flatten()
            .filter(|decision| **decision != Decision::Waiting)
            .count()
    }

    fn total(&self) -> usize {
        count(&self.repositories)
    }

    fn decide(
        &mut self,
        repository: usize,
        change: usize,
        decision: Decision,
        cx: &mut Context<Self>,
    ) {
        if let Some(slot) = self
            .decisions
            .get_mut(repository)
            .and_then(|decisions| decisions.get_mut(change))
        {
            *slot = decision;
            cx.notify();
        }
    }

    fn reject_all(&mut self, cx: &mut Context<Self>) {
        for decision in self.decisions.iter_mut().flatten() {
            *decision = Decision::Rejected;
        }
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ApplyState::Applying) || self.decided() == 0 {
            return;
        }
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        for (repository, decisions) in self.repositories.iter().zip(&self.decisions) {
            for (change, decision) in repository.review.pending.iter().zip(decisions) {
                let target = match decision {
                    Decision::Accepted => &mut accepted,
                    Decision::Rejected => &mut rejected,
                    Decision::Waiting => continue,
                };
                target.push((repository.root.clone(), change.files.clone()));
            }
        }
        let remaining = self.total() - self.decided();
        self.state = ApplyState::Applying;
        cx.notify();
        let sandbox = self.sandbox.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let sandbox = sandbox.clone();
                    async move {
                        let sbx = Sbx::locate()?;
                        for (root, files) in &rejected {
                            sands_project::revert(&sbx, &sandbox, root, files).await?;
                        }
                        let mut snapshot = Snapshot::load(&sandbox)?;
                        for (root, files) in &accepted {
                            snapshot.approve(root, files);
                        }
                        snapshot.save(&sandbox)
                    }
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    set_pending_count(&this.sandbox, remaining, cx);
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.state = ApplyState::Failed(
                        format!("Applying the decisions failed: {error:#}").into(),
                    );
                    cx.notify();
                }
            })
            .log_err();
        }));
    }

    fn render_change(
        &self,
        repository: usize,
        index: usize,
        change: &PendingChange,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let selected = self.selected == (repository, index);
        let decision = self
            .decisions
            .get(repository)
            .and_then(|decisions| decisions.get(index))
            .copied()
            .unwrap_or(Decision::Waiting);
        let (risk, tone) = if change.widens_access {
            ("widens access", Tone::Error)
        } else {
            ("changes behaviour", Tone::Warn)
        };
        let (decision_label, decision_color) = match decision {
            Decision::Waiting => ("Waiting", Color::Muted),
            Decision::Accepted => ("✓ Accepted", Color::Success),
            Decision::Rejected => ("✕ Rejected", Color::Error),
        };
        let id = repository * 10_000 + index;
        let file = change
            .files
            .first()
            .map(|file| file.path.clone())
            .unwrap_or_default();
        v_flex()
            .id(("settings-change", id))
            .p_3()
            .gap_1p5()
            .rounded_lg()
            .border_1()
            .border_color(if selected {
                colors.border_focused
            } else {
                colors.border_variant
            })
            .when(selected, |this| this.bg(colors.element_selected))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = (repository, index);
                cx.notify();
            }))
            .child(
                h_flex().gap_2().child(badge(risk, tone, cx)).child(
                    Label::new(change.title.clone())
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::MEDIUM)
                        .truncate(),
                ),
            )
            .child(
                Label::new(if change.files.len() > 1 {
                    format!("{file} and {} more", change.files.len() - 1)
                } else {
                    file
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .buffer_font(cx)
                .truncate(),
            )
            .child(
                Label::new(change.effect.clone())
                    .size(LabelSize::Small)
                    .color(Color::Default),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(decision_label)
                            .size(LabelSize::Small)
                            .color(decision_color),
                    )
                    .child(div().flex_1())
                    .child(
                        secondary_button(("settings-reject", id), "Reject").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.decide(repository, index, Decision::Rejected, cx)
                            },
                        )),
                    )
                    .child(
                        secondary_button(("settings-accept", id), "Accept").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.decide(repository, index, Decision::Accepted, cx)
                            },
                        )),
                    ),
            )
    }

    fn render_details(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(change) = self
            .repositories
            .get(self.selected.0)
            .and_then(|repository| repository.review.pending.get(self.selected.1))
        else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors();
        let mut details = v_flex().gap_4().child(
            h_flex()
                .gap_2()
                .child(Headline::new(change.title.clone()).size(HeadlineSize::XSmall))
                .child(if change.widens_access {
                    badge("widens access", Tone::Error, cx).into_any_element()
                } else {
                    badge("changes behaviour", Tone::Warn, cx).into_any_element()
                }),
        );
        for (file_index, file) in change.files.iter().enumerate() {
            let kind = match file.kind() {
                ChangeKind::Added => "added",
                ChangeKind::Modified => "changed",
                ChangeKind::Removed => "removed",
            };
            let lines = match file.diff() {
                Some(diff) => diff
                    .lines()
                    .skip(2)
                    .take(400)
                    .map(|line| {
                        let (color, background) = if line.starts_with('+') {
                            (
                                Color::Created,
                                Some(colors.version_control_added.opacity(0.12)),
                            )
                        } else if line.starts_with('-') {
                            (
                                Color::Deleted,
                                Some(colors.version_control_deleted.opacity(0.12)),
                            )
                        } else if line.starts_with("@@") {
                            (Color::Muted, None)
                        } else {
                            (Color::Default, None)
                        };
                        div()
                            .px_3()
                            .when_some(background, |this, background| this.bg(background))
                            .child(
                                Label::new(line.to_string())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(color),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>(),
                None => vec![
                    div()
                        .px_3()
                        .child(caption("Binary or large file; only its hash is compared."))
                        .into_any_element(),
                ],
            };
            details = details.child(
                v_flex()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border_variant)
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .px_3()
                            .py_1p5()
                            .gap_2()
                            .bg(colors.panel_background)
                            .border_b_1()
                            .border_color(colors.border_variant)
                            .child(
                                Label::new(file.path.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(kind).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                    .child(
                        v_flex()
                            .id(("settings-diff", file_index))
                            .py_2()
                            .max_h(rems(24.))
                            .overflow_y_scroll()
                            .children(lines),
                    ),
            );
        }
        if change.widens_access {
            details = details.child(
                h_flex()
                    .p_3()
                    .gap_2()
                    .items_start()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.status_bar_background)
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(
                            "Agents in the sandbox can write .sands/. Accept only what you or \
                             your team meant to add.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                    ),
            );
        }
        details.into_any_element()
    }
}

impl ModalView for SettingsReviewModal {}

impl EventEmitter<DismissEvent> for SettingsReviewModal {}

impl Focusable for SettingsReviewModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsReviewModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.total();
        let decided = self.decided();
        let applying = matches!(self.state, ApplyState::Applying);
        let mut cards = Vec::new();
        let several = self.repositories.len() > 1;
        for (repository_index, repository) in self.repositories.iter().enumerate() {
            if repository.review.pending.is_empty() {
                continue;
            }
            if several {
                cards.push(
                    Label::new(repository.root.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(cx)
                        .into_any_element(),
                );
            }
            for (index, change) in repository.review.pending.iter().enumerate() {
                cards.push(
                    self.render_change(repository_index, index, change, cx)
                        .into_any_element(),
                );
            }
        }
        let applied = self
            .repositories
            .iter()
            .flat_map(|repository| &repository.review.applied)
            .map(|change| change.path.clone())
            .collect::<Vec<_>>();
        let details = self.render_details(cx);
        let status = match &self.state {
            ApplyState::Editing => None,
            ApplyState::Applying => Some(caption("Applying…").into_any_element()),
            ApplyState::Failed(message) => Some(
                Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element(),
            ),
        };
        let colors = cx.theme().colors();

        v_flex()
            .key_context("SettingsReviewModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(72.))
            .h(rems(44.))
            .overflow_hidden()
            .child(
                h_flex()
                    .px_5()
                    .py_4()
                    .gap_3()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Medium)
                            .color(Color::Warning),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Headline::new(format!(
                                    ".sands/ changed — {total} change{} need{} your approval",
                                    if total == 1 { "" } else { "s" },
                                    if total == 1 { "s" } else { "" },
                                ))
                                .size(HeadlineSize::Small),
                            )
                            .child(caption(
                                "Accepted changes become the project's approved settings. \
                                 Rejected ones are put back in the sandbox as they were.",
                            )),
                    )
                    .child(
                        secondary_button("settings-reject-all", "Reject all")
                            .disabled(applying)
                            .on_click(cx.listener(|this, _, _, cx| this.reject_all(cx))),
                    )
                    .child(
                        primary_button(
                            "settings-apply",
                            format!(
                                "Apply {decided} decision{}",
                                if decided == 1 { "" } else { "s" }
                            ),
                        )
                        .disabled(applying || decided == 0)
                        .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(
                        v_flex()
                            .id("settings-changes")
                            .w(rems(28.))
                            .flex_none()
                            .h_full()
                            .p_3()
                            .gap_2()
                            .overflow_y_scroll()
                            .border_r_1()
                            .border_color(colors.border_variant)
                            .children(cards)
                            .when(!applied.is_empty(), |this| {
                                this.child(
                                    v_flex()
                                        .p_3()
                                        .gap_1()
                                        .rounded_lg()
                                        .border_1()
                                        .border_dashed()
                                        .border_color(colors.border_variant)
                                        .child(
                                            Label::new("Applied without approval")
                                                .size(LabelSize::Small)
                                                .weight(gpui::FontWeight::MEDIUM),
                                        )
                                        .child(caption(format!(
                                            "{} — they only run inside the sandbox.",
                                            applied.join(", ")
                                        ))),
                                )
                            }),
                    )
                    .child(
                        v_flex()
                            .id("settings-change-details")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .p_5()
                            .overflow_y_scroll()
                            .child(details),
                    ),
            )
            .children(status.map(|status| {
                h_flex()
                    .px_5()
                    .py_2()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(status)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sands_project::FileState;

    #[test]
    fn written_folders_cover_their_files() {
        let approved = Files::new();
        let current = [
            (".sands/skills/release/SKILL.md", "a"),
            (".sands/skills/release-notes/SKILL.md", "b"),
            (".sands/mcp.json", "{}"),
            (".sands/instructions.md", "x"),
        ]
        .into_iter()
        .map(|(path, text)| (path.to_string(), FileState::from_text(text)))
        .collect::<Files>();
        let changes = written_changes(
            &approved,
            &current,
            &[
                ".sands/skills/release".to_string(),
                ".sands/mcp.json".to_string(),
            ],
        );
        let paths = changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, [".sands/mcp.json", ".sands/skills/release/SKILL.md"]);
    }
}
