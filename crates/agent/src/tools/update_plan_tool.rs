use crate::{AgentTool, SubagentRole, ToolCallEventStream, ToolInput};
use agent_client_protocol::v2::schema as acp;
use gpui::{App, SharedString, Task};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Records the plan for the current task, shown to the user as a checklist next to the chat.
///
/// - Use it for work of three or more steps, before starting, and whenever a step changes state.
/// - Send the whole plan every time, with every step's current status; it replaces the last one.
/// - Keep exactly one step `in_progress` while work goes on.
/// - Give each step the `role` that does it (`worker`, `explorer` or `reviewer`) when you hand it
///   to a subagent with `spawn_agent`, so the user sees who works on what.
/// - Steps that touch different files can run in parallel; say which files a step changes in its
///   text when it matters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct UpdatePlanToolInput {
    /// Every step of the plan, in order.
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct PlanStep {
    /// What the step achieves, in a short sentence.
    pub content: String,
    pub status: PlanStepStatus,
    /// Who carries out the step, when it goes to a subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<SubagentRole>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl PlanStepStatus {
    fn to_acp(self) -> acp::PlanEntryStatus {
        match self {
            PlanStepStatus::Pending => acp::PlanEntryStatus::Pending,
            PlanStepStatus::InProgress => acp::PlanEntryStatus::InProgress,
            PlanStepStatus::Completed => acp::PlanEntryStatus::Completed,
            PlanStepStatus::Cancelled => acp::PlanEntryStatus::Cancelled,
        }
    }
}

/// The key under each plan entry's `_meta` where Sands keeps the step's role.
pub const PLAN_ROLE_META_KEY: &str = "sands_role";

pub fn plan_entries(steps: &[PlanStep]) -> Vec<acp::PlanEntry> {
    steps
        .iter()
        .map(|step| {
            let entry = acp::PlanEntry::new(
                step.content.clone(),
                acp::PlanEntryPriority::Medium,
                step.status.to_acp(),
            );
            match step.role.and_then(|role| serde_json::to_value(role).ok()) {
                Some(role) => {
                    let mut meta = acp::Meta::new();
                    meta.insert(PLAN_ROLE_META_KEY.to_string(), role);
                    entry.meta(meta)
                }
                None => entry,
            }
        })
        .collect()
}

pub struct UpdatePlanTool;

impl AgentTool for UpdatePlanTool {
    type Input = UpdatePlanToolInput;
    type Output = String;

    const NAME: &'static str = "update_plan";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Think
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => {
                let done = input
                    .steps
                    .iter()
                    .filter(|step| step.status == PlanStepStatus::Completed)
                    .count();
                format!("Plan: {done} of {} steps done", input.steps.len()).into()
            }
            Err(_) => "Update plan".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |_cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            if input.steps.is_empty() {
                return Err("A plan needs at least one step.".to_string());
            }
            let in_progress = input
                .steps
                .iter()
                .filter(|step| step.status == PlanStepStatus::InProgress)
                .count();
            event_stream.update_plan(plan_entries(&input.steps));
            let done = input
                .steps
                .iter()
                .filter(|step| step.status == PlanStepStatus::Completed)
                .count();
            let mut output = format!(
                "Plan updated: {done} of {} steps done.",
                input.steps.len()
            );
            if in_progress > 1 {
                output.push_str(" Note: more than one step is in progress.");
            }
            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn sends_the_plan_to_the_thread(cx: &mut gpui::TestAppContext) {
        let (event_stream, mut events) = ToolCallEventStream::test();
        let input = ToolInput::resolved(serde_json::json!({
            "steps": [
                { "content": "Cart model", "status": "completed", "role": "worker" },
                { "content": "Cart API", "status": "in_progress", "role": "worker" },
                { "content": "Review", "status": "pending", "role": "reviewer" },
            ]
        }));
        let output = cx
            .update(|cx| Arc::new(UpdatePlanTool).run(input, event_stream, cx))
            .await
            .unwrap();
        assert_eq!(output, "Plan updated: 1 of 3 steps done.");
        let entries = events.expect_plan().await;
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.status.clone())
                .collect::<Vec<_>>(),
            [
                acp::PlanEntryStatus::Completed,
                acp::PlanEntryStatus::InProgress,
                acp::PlanEntryStatus::Pending
            ]
        );
    }

    #[gpui::test]
    async fn refuses_an_empty_plan(cx: &mut gpui::TestAppContext) {
        let (event_stream, _events) = ToolCallEventStream::test();
        let input = ToolInput::resolved(serde_json::json!({ "steps": [] }));
        let result = cx
            .update(|cx| Arc::new(UpdatePlanTool).run(input, event_stream, cx))
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn steps_become_plan_entries_with_their_role() {
        let entries = plan_entries(&[
            PlanStep {
                content: "Add the cart model".to_string(),
                status: PlanStepStatus::Completed,
                role: Some(SubagentRole::Worker),
            },
            PlanStep {
                content: "Review the diff".to_string(),
                status: PlanStepStatus::Pending,
                role: None,
            },
        ]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].status, acp::PlanEntryStatus::Completed);
        assert_eq!(
            entries[0]
                .meta
                .as_ref()
                .and_then(|meta| meta.get(PLAN_ROLE_META_KEY))
                .and_then(|role| role.as_str()),
            Some("worker")
        );
        assert!(entries[1].meta.is_none());
    }
}
