//! The agent's usage limits in the status bar, like the usage readouts of agent IDEs: the
//! highest limit at a glance, every limit and its reset time on hover.

use std::time::Duration;

use gpui::{App, AppContext as _, Task, WeakEntity};
use sbx::{AgentUsage, SandboxStatus, Sbx};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

use crate::{
    connected_sandbox,
    design::Tone,
    new_project_modal::agent_icon,
    sandbox_panel::{ToggleSandboxPanel, format_reset},
};

/// Agents report after each reply, so a minute is fresh enough and keeps `sbx exec` rare.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let indicator = cx.new(|cx| AgentStatusIndicator::new(workspace.weak_handle(), cx));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(indicator, window, cx)
        });
    })
    .detach();
}

pub(crate) struct AgentStatusIndicator {
    usage: Option<AgentUsage>,
    _refresh: Task<()>,
}

impl AgentStatusIndicator {
    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                let sandbox = workspace
                    .read_with(cx, |workspace, cx| connected_sandbox(workspace, cx))
                    .ok()
                    .flatten();
                let usage = match sandbox {
                    Some(name) => cx
                        .background_spawn(async move { load_usage(&name).await })
                        .await
                        .log_err()
                        .flatten(),
                    None => None,
                };
                if this
                    .update(cx, |this, cx| {
                        this.usage = usage;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
        Self {
            usage: None,
            _refresh: refresh,
        }
    }
}

/// The usage of the sandbox's agent, without starting a stopped sandbox.
async fn load_usage(name: &str) -> anyhow::Result<Option<AgentUsage>> {
    let sbx = Sbx::locate()?;
    let info = sbx.get(name).await?;
    if info.status != SandboxStatus::Running {
        return Ok(None);
    }
    let usage = sbx.agent_usage(name).await?;
    Ok(usage
        .iter()
        .find(|usage| usage.agent == info.agent)
        .or_else(|| usage.first())
        .cloned()
        .filter(|usage| !usage.windows.is_empty()))
}

/// "5h 38% · wk 28%".
fn summary(usage: &AgentUsage) -> String {
    usage
        .windows
        .iter()
        .map(|window| {
            let label = match window.label.as_str() {
                "5-hour limit" => "5h".to_string(),
                "Weekly limit" => "wk".to_string(),
                other => other.trim_end_matches(" limit").to_string(),
            };
            format!("{label} {:.0}%", window.used_percent)
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn tone(usage: &AgentUsage) -> Tone {
    let highest = usage
        .windows
        .iter()
        .map(|window| window.used_percent)
        .fold(0.0, f64::max);
    if highest > 90.0 {
        Tone::Error
    } else if highest > 75.0 {
        Tone::Warn
    } else {
        Tone::Muted
    }
}

fn tooltip_text(usage: &AgentUsage) -> String {
    let now = chrono::Utc::now().timestamp();
    let mut lines = vec![crate::agent_display_name(&usage.agent).to_string()];
    for window in &usage.windows {
        let resets = window
            .resets_at
            .filter(|resets_at| *resets_at > now)
            .map(|resets_at| format!(", resets in {}", format_reset(resets_at, now)))
            .unwrap_or_default();
        lines.push(format!(
            "{} {:.0}%{resets}",
            window.label, window.used_percent
        ));
    }
    lines.join("\n")
}

impl Render for AgentStatusIndicator {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some(usage) = &self.usage else {
            return div().into_any_element();
        };
        let tone = tone(usage);
        let tooltip = tooltip_text(usage);
        h_flex()
            .id("agent-usage")
            .px_1()
            .gap_1()
            .cursor_pointer()
            .child(
                Icon::new(agent_icon(&usage.agent))
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(summary(usage))
                    .size(LabelSize::XSmall)
                    .color(tone.color()),
            )
            .tooltip(Tooltip::text(tooltip))
            .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleSandboxPanel), cx))
            .into_any_element()
    }
}

impl StatusItemView for AgentStatusIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    /// Shown only in sandbox projects whose agent reports limits.
    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sbx::UsageWindow;

    #[test]
    fn summarizes_limits() {
        let usage = AgentUsage {
            agent: "claude".to_string(),
            windows: vec![
                UsageWindow {
                    label: "5-hour limit".to_string(),
                    used_percent: 38.4,
                    resets_at: None,
                },
                UsageWindow {
                    label: "Weekly limit".to_string(),
                    used_percent: 91.0,
                    resets_at: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(summary(&usage), "5h 38% · wk 91%");
        assert_eq!(tone(&usage), Tone::Error);
        assert_eq!(
            tooltip_text(&usage),
            "Claude Code\n5-hour limit 38%\nWeekly limit 91%"
        );
    }
}
