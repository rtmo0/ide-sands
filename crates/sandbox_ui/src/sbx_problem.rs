//! Explains why Docker Sandboxes cannot be used right now and fixes it with one click: starting
//! the daemon or signing in to Docker.

use gpui::{App, AppContext as _, EventEmitter, FontWeight, SharedString, Task};
use sbx::{LoginPrompt, Sbx, SbxError};
use ui::prelude::*;
use util::ResultExt as _;

use crate::design::{caption, primary_button, secondary_button};

const INSTALL_URL: &str = "https://docs.docker.com/ai/sandboxes/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    NotInstalled,
    DaemonNotRunning,
    NotLoggedIn,
}

impl Problem {
    /// The problem a button can fix, if the error is one.
    pub(crate) fn of(error: &SbxError) -> Option<Self> {
        match error {
            SbxError::NotInstalled => Some(Self::NotInstalled),
            SbxError::DaemonNotRunning { .. } => Some(Self::DaemonNotRunning),
            SbxError::NotLoggedIn => Some(Self::NotLoggedIn),
            _ => None,
        }
    }

    /// Like [`Problem::of`], for errors that wrap an [`SbxError`].
    pub(crate) fn of_any(error: &anyhow::Error) -> Option<Self> {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<SbxError>().and_then(Self::of))
    }
}

enum FixState {
    Idle,
    Working(SharedString),
    SigningIn(LoginPrompt),
    Failed(SharedString),
}

/// Emitted once the problem is fixed, so the owner can reload.
pub(crate) struct ProblemFixed;

pub(crate) struct SbxProblemView {
    problem: Problem,
    state: FixState,
    _task: Task<()>,
}

impl EventEmitter<ProblemFixed> for SbxProblemView {}

impl SbxProblemView {
    pub(crate) fn new(problem: Problem) -> Self {
        Self {
            problem,
            state: FixState::Idle,
            _task: Task::ready(()),
        }
    }

    pub(crate) fn problem(&self) -> &Problem {
        &self.problem
    }

    fn fix(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.problem {
            Problem::NotInstalled => cx.open_url(INSTALL_URL),
            Problem::DaemonNotRunning => {
                self.state = FixState::Working("Starting Docker Sandboxes…".into());
                self._task = cx.spawn_in(window, async move |this, cx| {
                    let started = cx
                        .background_spawn(async move { Sbx::locate()?.start_daemon().await })
                        .await;
                    let result = match started {
                        Ok(()) => wait_for_daemon(cx).await,
                        Err(error) => Err(error),
                    };
                    this.update(cx, |this, cx| this.finish(result, cx))
                        .log_err();
                });
            }
            Problem::NotLoggedIn => {
                self.state = FixState::Working("Starting sign-in…".into());
                let (sender, mut receiver) = futures::channel::mpsc::unbounded::<LoginPrompt>();
                self._task = cx.spawn_in(window, async move |this, cx| {
                    let login = cx.background_spawn(async move {
                        Sbx::locate()?
                            .login(move |prompt| {
                                sender.unbounded_send(prompt).ok();
                            })
                            .await
                    });
                    let mut opened_browser = false;
                    while let Some(prompt) = futures::StreamExt::next(&mut receiver).await {
                        let url = prompt.url.clone();
                        let updated = this.update(cx, |this, cx| {
                            this.state = FixState::SigningIn(prompt);
                            cx.notify();
                        });
                        if updated.is_err() {
                            return;
                        }
                        // sbx opens the browser itself after Enter; this covers it failing to.
                        if let Some(url) = url
                            && !opened_browser
                        {
                            opened_browser = true;
                            cx.update(|_, cx| cx.open_url(&url)).log_err();
                        }
                    }
                    let result = login.await;
                    this.update(cx, |this, cx| this.finish(result, cx))
                        .log_err();
                });
            }
        }
        cx.notify();
    }

    fn finish(&mut self, result: Result<(), SbxError>, cx: &mut Context<Self>) {
        match result {
            Ok(()) => {
                self.state = FixState::Idle;
                cx.emit(ProblemFixed);
            }
            Err(error) => {
                self.state = FixState::Failed(error.to_string().into());
                if let Some(problem) = Problem::of(&error) {
                    self.problem = problem;
                }
            }
        }
        cx.notify();
    }
}

impl Render for SbxProblemView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (icon, title, description, action) = match self.problem {
            Problem::NotInstalled => (
                IconName::Download,
                "Docker Sandboxes is not installed",
                "Sands runs every project in a Docker Sandbox. Install the sbx CLI, then refresh.",
                "Installation Guide",
            ),
            Problem::DaemonNotRunning => (
                IconName::Server,
                "Docker Sandboxes is not running",
                "Projects cannot start until the sandbox daemon runs.",
                "Start Docker Sandboxes",
            ),
            Problem::NotLoggedIn => (
                IconName::Person,
                "Sign in to Docker",
                "Docker Sandboxes needs your Docker account to start sandboxes.",
                "Sign In",
            ),
        };
        let colors = cx.theme().colors();
        let busy = matches!(self.state, FixState::Working(_) | FixState::SigningIn(_));
        v_flex()
            .m_2()
            .p_3()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.surface_background)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(icon).size(IconSize::Small).color(Color::Warning))
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(caption(description))
            .map(|this| match &self.state {
                FixState::Idle => this,
                FixState::Working(message) => this.child(caption(message.clone())),
                FixState::Failed(message) => this.child(
                    Label::new(message.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                ),
                FixState::SigningIn(prompt) => this.child(
                    v_flex()
                        .gap_1()
                        .child(caption("Confirm this code in the browser:"))
                        .child(
                            Label::new(
                                prompt
                                    .code
                                    .clone()
                                    .unwrap_or_else(|| "waiting for the code…".to_string()),
                            )
                            .size(LabelSize::Large)
                            .weight(FontWeight::SEMIBOLD)
                            .buffer_font(cx),
                        )
                        .when_some(prompt.url.clone(), |this, url| {
                            this.child(
                                secondary_button("open-login-page", "Open Browser")
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                        }),
                ),
            })
            .when(!busy, |this| {
                this.child(
                    h_flex().child(
                        primary_button("fix-sbx-problem", action)
                            .on_click(cx.listener(|this, _, window, cx| this.fix(window, cx))),
                    ),
                )
            })
    }
}

/// Waits up to half a minute for a freshly started daemon to accept commands. A missing
/// sign-in is fine here: the daemon runs, and the panel then offers to sign in.
async fn wait_for_daemon(cx: &mut gpui::AsyncWindowContext) -> Result<(), SbxError> {
    let mut last_error = None;
    for _ in 0..30 {
        let check = cx
            .background_spawn(async move {
                Sbx::locate()?
                    .diagnose()
                    .await
                    .and_then(|report| report.ensure_ready())
            })
            .await;
        match check {
            Ok(()) | Err(SbxError::NotLoggedIn) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        cx.background_executor()
            .timer(std::time::Duration::from_secs(1))
            .await;
    }
    Err(last_error.unwrap_or(SbxError::DaemonNotRunning {
        message: "the daemon did not start".to_string(),
    }))
}

pub(crate) fn problem_view(problem: Problem, cx: &mut App) -> gpui::Entity<SbxProblemView> {
    cx.new(|_| SbxProblemView::new(problem))
}
