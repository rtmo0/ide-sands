use std::{collections::BTreeMap, fs};

use anyhow::Context as _;
use gpui::{AppContext as _, Entity, SharedString, Task, WeakEntity};
use language_model::LanguageModelRegistry;
use sands_project::{ProjectFile, Snapshot};
use sbx::Sbx;
use serde::Deserialize;
use ui::{ContextMenu, DropdownMenu, DropdownStyle, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{
    connected_sandbox,
    design::{Tone, badge, caption, key_value, section_header},
    gateway, settings_review,
};

const PROJECT_FILE: &str = ".sands/project.yaml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Plan,
    Work,
    Fast,
}

impl Tier {
    const ALL: [Tier; 3] = [Tier::Plan, Tier::Work, Tier::Fast];

    fn name(self) -> &'static str {
        match self {
            Tier::Plan => "plan",
            Tier::Work => "work",
            Tier::Fast => "fast",
        }
    }

    fn purpose(self) -> &'static str {
        match self {
            Tier::Plan => "Planning and replanning",
            Tier::Work => "Writing code, running tests",
            Tier::Fast => "Search, review, summaries",
        }
    }

    fn get(self, project: &ProjectFile) -> Option<&String> {
        match self {
            Tier::Plan => project.models.plan.as_ref(),
            Tier::Work => project.models.work.as_ref(),
            Tier::Fast => project.models.fast.as_ref(),
        }
    }

    fn set(self, project: &mut ProjectFile, model: Option<String>) {
        let slot = match self {
            Tier::Plan => &mut project.models.plan,
            Tier::Work => &mut project.models.work,
            Tier::Fast => &mut project.models.fast,
        };
        *slot = model;
    }
}

/// A call from the gateway's log.
#[derive(Clone, Debug, Deserialize)]
struct LoggedCall {
    model: Option<String>,
    status: u16,
    allowed: bool,
    unix_time: u64,
}

/// How many calls the page reads back from the end of the log.
const RECENT_CALLS: usize = 500;

fn read_calls(project: &str) -> Vec<LoggedCall> {
    let path = gateway::calls_log(project);
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let lines = text.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(RECENT_CALLS)..]
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The model id the gateway compares with what agents send: the API name without the variant
/// suffixes Sands adds to its own model ids.
fn api_model(provider: &str, model: &str) -> String {
    let model = model
        .trim_end_matches("-thinking")
        .trim_end_matches("-latest");
    format!("{provider}/{model}")
}

struct Loaded {
    sandbox: String,
    /// The repository whose `.sands/project.yaml` describes the project.
    root: String,
    project: ProjectFile,
    calls: Vec<LoggedCall>,
}

enum PageState {
    NoProject,
    Loading,
    Loaded(Loaded),
    Failed(SharedString),
}

/// The Models page of the Sandbox panel: the project's model tiers, which the model gateway
/// enforces for agents in the sandbox, and what went through the gateway.
pub(crate) struct ModelsPage {
    workspace: WeakEntity<Workspace>,
    state: PageState,
    error: Option<SharedString>,
    _load: Task<()>,
    _save: Task<()>,
}

impl ModelsPage {
    /// The page loads when it is shown; the panel is built while the workspace is busy.
    pub(crate) fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self {
            workspace,
            state: PageState::Loading,
            error: None,
            _load: Task::ready(()),
            _save: Task::ready(()),
        }
    }

    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let workspace = workspace.read(cx);
        let Some(sandbox) = connected_sandbox(workspace, cx) else {
            self.state = PageState::NoProject;
            cx.notify();
            return;
        };
        let first_root = workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned());
        self._load = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let snapshot = Snapshot::load(&sandbox)?;
                    let described = snapshot.repositories.iter().find_map(|(root, files)| {
                        let text = files.get(PROJECT_FILE)?.text.as_deref()?;
                        Some((root.clone(), ProjectFile::parse(text)))
                    });
                    let (root, project) = match described {
                        Some((root, project)) => (root, project?),
                        None => (
                            first_root.context("the project has no folder")?,
                            ProjectFile::default(),
                        ),
                    };
                    let calls = read_calls(&sandbox);
                    anyhow::Ok(Loaded {
                        sandbox,
                        root,
                        project,
                        calls,
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(loaded) => PageState::Loaded(loaded),
                    Err(error) => PageState::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .log_err();
        });
    }

    fn set_tier(&mut self, tier: Tier, model: Option<String>, cx: &mut Context<Self>) {
        let PageState::Loaded(loaded) = &mut self.state else {
            return;
        };
        if tier.get(&loaded.project) == model.as_ref() {
            return;
        }
        tier.set(&mut loaded.project, model);
        let sandbox = loaded.sandbox.clone();
        let root = loaded.root.clone();
        let project = loaded.project.clone();
        self.error = None;
        cx.notify();
        self._save = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let sandbox = sandbox.clone();
                    async move {
                        let content = serde_yaml::to_string(&project)?;
                        let sbx = Sbx::locate()?;
                        sands_project::write(&sbx, &sandbox, &root, PROJECT_FILE, &content).await?;
                        // The user chose this here, so it counts as approved.
                        settings_review::approve_written(
                            &sbx,
                            &sandbox,
                            &root,
                            &[PROJECT_FILE.to_string()],
                        )
                        .await
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => gateway::connect(sandbox, cx),
                    Err(error) => {
                        this.error = Some(format!("Saving the tier failed: {error:#}").into());
                    }
                }
                this.reload(cx);
            })
            .log_err();
        });
    }

    fn tier_menu(
        &self,
        tier: Tier,
        current: Option<&String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let registry = LanguageModelRegistry::global(cx).read(cx);
        let mut options = registry
            .available_models(cx)
            .map(|model| {
                (
                    api_model(model.provider_id().0.as_ref(), model.id().0.as_ref()),
                    format!("{} · {}", model.provider_name().0, model.name().0),
                )
            })
            .collect::<Vec<_>>();
        options.sort();
        options.dedup_by(|a, b| a.0 == b.0);
        let current = current.cloned();
        let page = cx.entity().downgrade();
        ContextMenu::build(window, cx, move |mut menu, _, _| {
            menu =
                menu.toggleable_entry("Any model", current.is_none(), IconPosition::Start, None, {
                    let page = page.clone();
                    move |_, cx| {
                        page.update(cx, |page, cx| page.set_tier(tier, None, cx))
                            .log_err();
                    }
                });
            if options.is_empty() {
                return menu.entry("Sign in to a model provider first", None, |_, _| {});
            }
            for (value, label) in &options {
                let page = page.clone();
                let value = value.clone();
                menu = menu.toggleable_entry(
                    label.clone(),
                    current.as_ref() == Some(&value),
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        page.update(cx, |page, cx| page.set_tier(tier, Some(value.clone()), cx))
                            .log_err();
                    },
                );
            }
            menu
        })
    }

    fn render_tiers(
        &self,
        loaded: &Loaded,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .px_3()
            .pt_2()
            .pb_3()
            .gap_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(section_header("Model tiers", None))
            .child(caption(
                "Agents in the sandbox may only use these models; the gateway refuses others. \
                 Saved to .sands/project.yaml.",
            ))
            .children(Tier::ALL.into_iter().map(|tier| {
                let current = tier.get(&loaded.project);
                let label: SharedString = current
                    .cloned()
                    .unwrap_or_else(|| "Any model".to_string())
                    .into();
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(rems(3.))
                            .flex_none()
                            .child(badge(tier.name(), Tone::Info, cx)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                DropdownMenu::new(
                                    ("tier", tier as usize),
                                    label,
                                    self.tier_menu(tier, current, window, cx),
                                )
                                .style(DropdownStyle::Outlined)
                                .full_width(true),
                            )
                            .child(caption(tier.purpose()).size(LabelSize::XSmall)),
                    )
            }))
    }

    fn render_gateway(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let status = gateway::status(&loaded.sandbox, cx);
        let key = |present: Option<bool>| match present {
            Some(true) => badge("on the host", Tone::Ok, cx).into_any_element(),
            Some(false) => caption("none — the agent's own sign-in").into_any_element(),
            None => caption("—").into_any_element(),
        };
        let keys = status.and_then(|(_, keys)| keys);
        v_flex()
            .px_3()
            .pt_2()
            .pb_3()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(section_header("Model gateway", None))
            .child(key_value(
                "Status",
                match status {
                    Some((port, _)) => {
                        badge(format!("on · port {port}"), Tone::Ok, cx).into_any_element()
                    }
                    None => badge("off", Tone::Error, cx).into_any_element(),
                },
            ))
            .child(key_value(
                "Anthropic key",
                key(keys.map(|keys| keys.anthropic)),
            ))
            .child(key_value("OpenAI key", key(keys.map(|keys| keys.openai))))
    }

    fn render_calls(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let day_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|now| now.as_secs().saturating_sub(24 * 60 * 60))
            .unwrap_or(0);
        let recent = loaded
            .calls
            .iter()
            .filter(|call| call.unix_time >= day_ago)
            .collect::<Vec<_>>();
        let mut by_model = BTreeMap::<String, (usize, usize)>::new();
        for call in &recent {
            let entry = by_model
                .entry(call.model.clone().unwrap_or_else(|| "—".to_string()))
                .or_default();
            entry.0 += 1;
            if !call.allowed {
                entry.1 += 1;
            }
        }
        let latest = recent.iter().rev().take(6).map(|call| {
            let time = chrono::DateTime::from_timestamp(call.unix_time as i64, 0)
                .map(|time| {
                    time.with_timezone(&chrono::Local)
                        .format("%H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            let (text, tone) = if !call.allowed {
                ("refused".to_string(), Tone::Error)
            } else if call.status >= 400 {
                (call.status.to_string(), Tone::Warn)
            } else {
                (call.status.to_string(), Tone::Ok)
            };
            h_flex()
                .gap_2()
                .child(caption(time).size(LabelSize::XSmall))
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(call.model.clone().unwrap_or_else(|| "—".to_string()))
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .truncate(),
                    ),
                )
                .child(badge(text, tone, cx))
        });
        v_flex()
            .px_3()
            .pt_2()
            .pb_3()
            .gap_1()
            .child(section_header("Calls, last 24 hours", None))
            .when(recent.is_empty(), |this| {
                this.child(caption("No calls from agents in the sandbox yet."))
            })
            .children(by_model.into_iter().map(|(model, (calls, refused))| {
                key_value(
                    model,
                    caption(if refused > 0 {
                        format!("{calls} calls, {refused} refused")
                    } else {
                        format!("{calls} calls")
                    }),
                )
            }))
            .when(!recent.is_empty(), |this| {
                this.child(div().pt_1().child(section_header("Latest", None)))
                    .children(latest)
            })
    }
}

impl Render for ModelsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            PageState::NoProject => v_flex()
                .p_3()
                .child(caption("Open a project to set its models."))
                .into_any_element(),
            PageState::Loading => v_flex().p_3().child(caption("Loading…")).into_any_element(),
            PageState::Failed(message) => v_flex()
                .p_3()
                .child(
                    Label::new(message.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .into_any_element(),
            PageState::Loaded(loaded) => v_flex()
                .child(self.render_tiers(loaded, window, cx))
                .child(self.render_gateway(loaded, cx))
                .child(self.render_calls(loaded, cx))
                .into_any_element(),
        };
        v_flex()
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .mx_3()
                        .mt_3()
                        .p_2()
                        .rounded_md()
                        .bg(cx.theme().status().error_background)
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_models_drop_sands_variant_suffixes() {
        assert_eq!(
            api_model("anthropic", "claude-sonnet-4-5-thinking"),
            "anthropic/claude-sonnet-4-5"
        );
        assert_eq!(api_model("openai", "gpt-5"), "openai/gpt-5");
    }

    #[test]
    fn tiers_set_and_clear() {
        let mut project = ProjectFile::default();
        Tier::Fast.set(&mut project, Some("google/gemini-flash".to_string()));
        assert_eq!(project.models.fast.as_deref(), Some("google/gemini-flash"));
        Tier::Fast.set(&mut project, None);
        assert_eq!(Tier::Fast.get(&project), None);
    }
}
