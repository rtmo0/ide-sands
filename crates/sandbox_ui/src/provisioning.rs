use anyhow::{Context as _, Result};
use base64::Engine as _;
use gpui::SharedString;
use sands_project::ProjectFile;
use sbx::{CreateOptions, NetworkMode, Sbx};

use crate::{
    new_project_modal::{ProjectPlan, ProjectSource, primary_repository},
    settings_review,
};

/// One thing creating a project does. Every step checks first whether its work is already done,
/// so a failed creation can be retried from the start without repeating what succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StepKind {
    Check,
    Sandbox,
    Network,
    /// A repository cloned into the sandbox workspace, by its index in the plan.
    Repository(usize),
    Toolchains,
    ProjectFile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StepState {
    Waiting,
    Running,
    Done(Option<SharedString>),
    /// The step had nothing to do.
    Skipped(SharedString),
    /// The step failed, but the project works without it.
    Warning(SharedString),
    Failed(SharedString),
}

#[derive(Clone, Debug)]
pub(crate) struct ProvisionStep {
    pub kind: StepKind,
    pub label: SharedString,
    pub state: StepState,
}

/// What earlier steps found out, for the steps after them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Provisioned {
    /// Set once this creation made the sandbox, so a retry reuses it and a cancel may remove it.
    pub created_sandbox: bool,
    workspace: Option<String>,
    repository_directories: Vec<String>,
}

impl Provisioned {
    /// The folder to open: the only repository, or the workspace that holds all of them.
    pub fn directory(&self) -> Option<String> {
        match self.repository_directories.as_slice() {
            [single] => Some(single.clone()),
            _ => self.workspace.clone(),
        }
    }
}

pub(crate) fn steps(plan: &ProjectPlan) -> Vec<ProvisionStep> {
    let step = |kind, label: String| ProvisionStep {
        kind,
        label: label.into(),
        state: StepState::Waiting,
    };
    let primary = primary_repository(&plan.repositories);
    let mut steps = vec![
        step(StepKind::Check, "Check Docker Sandboxes".to_string()),
        step(
            StepKind::Sandbox,
            match primary.and_then(|index| plan.repositories.get(index)) {
                Some(source) => {
                    format!("Create sandbox {} with {}", plan.name, source.short_name())
                }
                None => format!("Create sandbox {}", plan.name),
            },
        ),
        step(StepKind::Network, "Apply the network policy".to_string()),
    ];
    for (index, source) in plan.repositories.iter().enumerate() {
        if Some(index) != primary {
            steps.push(step(
                StepKind::Repository(index),
                format!("Clone {}", source.short_name()),
            ));
        }
    }
    steps.push(step(StepKind::Toolchains, "Install toolchains".to_string()));
    steps.push(step(
        StepKind::ProjectFile,
        "Write .sands/project.yaml".to_string(),
    ));
    steps
}

/// Runs one step. `Ok` holds the finished state; an `Err` stops the creation.
pub(crate) async fn run_step(
    sbx: &Sbx,
    plan: &ProjectPlan,
    kind: &StepKind,
    provisioned: &mut Provisioned,
) -> Result<StepState> {
    match kind {
        StepKind::Check => {
            sbx.check().await?;
            for source in &plan.repositories {
                if let ProjectSource::LocalRepository(path) = source {
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
                }
            }
            Ok(StepState::Done(None))
        }
        StepKind::Sandbox => create_sandbox(sbx, plan, provisioned).await,
        StepKind::Network => {
            let settings = &plan.settings;
            sbx.apply_network_mode(&plan.name, settings.network_mode, &settings.allowed_hosts)
                .await?;
            let detail = match settings.network_mode {
                NetworkMode::AllowAll => "Everything allowed".to_string(),
                NetworkMode::Balanced => "Common developer hosts allowed".to_string(),
                NetworkMode::DenyAll => "Everything blocked".to_string(),
            };
            Ok(StepState::Done(Some(detail.into())))
        }
        StepKind::Repository(index) => clone_repository(sbx, plan, *index, provisioned).await,
        StepKind::Toolchains => Ok(install_toolchains(sbx, &plan.name).await),
        StepKind::ProjectFile => write_project_file(sbx, plan, provisioned).await,
    }
}

async fn workspace(sbx: &Sbx, name: &str, provisioned: &mut Provisioned) -> Result<String> {
    if let Some(workspace) = &provisioned.workspace {
        return Ok(workspace.clone());
    }
    let workspace = sbx.default_directory(name).await?;
    provisioned.workspace = Some(workspace.clone());
    Ok(workspace)
}

async fn create_sandbox(
    sbx: &Sbx,
    plan: &ProjectPlan,
    provisioned: &mut Provisioned,
) -> Result<StepState> {
    let primary = primary_repository(&plan.repositories);
    let exists = sbx.get(&plan.name).await.is_ok();
    let state = if exists {
        anyhow::ensure!(
            provisioned.created_sandbox,
            "A sandbox named {} already exists. Choose another project name.",
            plan.name
        );
        StepState::Skipped("Already created".into())
    } else {
        sbx.scope_network_policy_to_sandboxes().await?;
        let (workspaces, read_only_workspaces) = plan
            .repositories
            .iter()
            .enumerate()
            .filter_map(|(index, source)| match source {
                ProjectSource::LocalRepository(path) => Some((index, path.clone())),
                ProjectSource::GitUrl(_) => None,
            })
            .partition::<Vec<_>, _>(|(index, _)| Some(*index) == primary);
        let settings = &plan.settings;
        sbx.create(&CreateOptions {
            name: plan.name.clone(),
            agent: plan.agent.to_string(),
            workspaces: workspaces.into_iter().map(|(_, path)| path).collect(),
            clone_workspace: primary.is_some(),
            read_only_workspaces: read_only_workspaces
                .into_iter()
                .map(|(_, path)| path)
                .collect(),
            publish: settings.publish.clone(),
            cpus: settings.cpus,
            memory: settings.memory.clone(),
            ..Default::default()
        })
        .await?;
        provisioned.created_sandbox = true;
        StepState::Done(None)
    };
    if primary.is_some() {
        let workspace = workspace(sbx, &plan.name, provisioned).await?;
        if !provisioned.repository_directories.contains(&workspace) {
            provisioned.repository_directories.insert(0, workspace);
        }
    }
    Ok(state)
}

async fn clone_repository(
    sbx: &Sbx,
    plan: &ProjectPlan,
    index: usize,
    provisioned: &mut Provisioned,
) -> Result<StepState> {
    let source = plan
        .repositories
        .get(index)
        .context("the repository is no longer in the plan")?;
    let directory_name = source
        .clone_directory_name()
        .context("cannot derive a folder name for the repository")?;
    let workspace = workspace(sbx, &plan.name, provisioned).await?;
    let target = format!("{workspace}/{directory_name}");
    let already_cloned = sbx
        .exec(&plan.name, &["test", "-d", &format!("{target}/.git")])
        .await?
        .status
        .success();
    let state = if already_cloned {
        StepState::Skipped("Already cloned".into())
    } else {
        match sbx.clone_repository(&plan.name, &source.display()).await {
            Ok(_) => StepState::Done(None),
            Err(error)
                if matches!(source, ProjectSource::GitUrl(_))
                    && plan.settings.network_mode != NetworkMode::AllowAll =>
            {
                anyhow::bail!(
                    "{error}. If the host is blocked, allow it in the project's network settings."
                )
            }
            Err(error) => return Err(error.into()),
        }
    };
    if !provisioned.repository_directories.contains(&target) {
        provisioned.repository_directories.push(target);
    }
    Ok(state)
}

/// Installs the toolchains the project's files call for. A failure does not stop the
/// creation: the Sandbox panel lists what is missing and can retry.
async fn install_toolchains(sbx: &Sbx, name: &str) -> StepState {
    let result = async {
        let probe = sbx.probe(name).await?;
        let missing = probe
            .toolchains
            .into_iter()
            .filter(|status| status.is_missing())
            .map(|status| status.toolchain)
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return anyhow::Ok(StepState::Skipped("Nothing to install".into()));
        }
        let names = missing
            .iter()
            .map(|toolchain| toolchain.display_name())
            .collect::<Vec<_>>()
            .join(", ");
        sbx.install_toolchains(name, &missing).await?;
        Ok(StepState::Done(Some(names.into())))
    }
    .await;
    result.unwrap_or_else(|error| {
        StepState::Warning(format!("{error:#}. Retry from the Sandbox panel.").into())
    })
}

/// The project description that goes with the code: the agents and allowed hosts chosen here.
fn project_file(plan: &ProjectPlan) -> ProjectFile {
    let mut project = ProjectFile::default();
    if plan.agent != "shell" {
        project.agents.push(plan.agent.to_string());
    }
    if plan.settings.network_mode != NetworkMode::AllowAll {
        project.network.allow = plan.settings.allowed_hosts.clone();
    }
    project
}

async fn write_project_file(
    sbx: &Sbx,
    plan: &ProjectPlan,
    provisioned: &mut Provisioned,
) -> Result<StepState> {
    let Some(root) = provisioned.repository_directories.first().cloned() else {
        return Ok(StepState::Skipped("No repository".into()));
    };
    let path = format!("{}/project.yaml", sands_project::SETTINGS_FOLDER);
    let exists = sbx
        .exec(&plan.name, &["test", "-e", &format!("{root}/{path}")])
        .await?
        .status
        .success();
    if exists {
        return Ok(StepState::Skipped("The repository already has one".into()));
    }
    let content = serde_yaml::to_string(&project_file(plan))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(content);
    let output = sbx
        .exec(
            &plan.name,
            &[
                "sh",
                "-c",
                "mkdir -p \"$1/.sands\" && printf '%s' \"$2\" | base64 -d > \"$1/.sands/project.yaml\"",
                "sh",
                root.as_str(),
                encoded.as_str(),
            ],
        )
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "writing {path} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    settings_review::approve_written(sbx, &plan.name, &root, &[path]).await?;
    Ok(StepState::Done(None))
}

/// Runs every step in order without a UI and returns the folder to open.
#[cfg(test)]
pub(crate) async fn provision(plan: &ProjectPlan) -> Result<String> {
    let sbx = Sbx::locate()?;
    let mut provisioned = Provisioned::default();
    for step in steps(plan) {
        if let StepState::Warning(message) =
            run_step(&sbx, plan, &step.kind, &mut provisioned).await?
        {
            log::warn!("{}: {message}", step.label);
        }
    }
    provisioned
        .directory()
        .context("the project has no folder to open")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_project_modal::SandboxSettings;

    fn plan(
        repositories: Vec<ProjectSource>,
        agent: &'static str,
        mode: NetworkMode,
    ) -> ProjectPlan {
        ProjectPlan {
            name: "shop".to_string(),
            agent,
            settings: SandboxSettings {
                network_mode: mode,
                allowed_hosts: vec!["api.example.com".to_string()],
                cpus: None,
                memory: None,
                publish: Vec::new(),
            },
            repositories,
        }
    }

    #[test]
    fn steps_follow_the_repositories() {
        let plan = plan(
            vec![
                ProjectSource::GitUrl("https://github.com/acme/web.git".to_string()),
                ProjectSource::LocalRepository("/Users/me/api".into()),
            ],
            "claude",
            NetworkMode::Balanced,
        );
        let labels = steps(&plan)
            .into_iter()
            .map(|step| step.label.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "Check Docker Sandboxes",
                "Create sandbox shop with api",
                "Apply the network policy",
                "Clone web",
                "Install toolchains",
                "Write .sands/project.yaml",
            ]
        );
    }

    #[test]
    fn project_file_records_the_agent_and_allowed_hosts() {
        let balanced = project_file(&plan(Vec::new(), "codex", NetworkMode::Balanced));
        assert_eq!(balanced.agents, ["codex"]);
        assert_eq!(balanced.network.allow, ["api.example.com"]);
        let open = project_file(&plan(Vec::new(), "shell", NetworkMode::AllowAll));
        assert!(open.agents.is_empty() && open.network.allow.is_empty());
    }

    #[test]
    fn directory_is_the_only_repository_or_the_workspace() {
        let mut provisioned = Provisioned {
            workspace: Some("/home/agent/workspace".to_string()),
            ..Default::default()
        };
        provisioned
            .repository_directories
            .push("/home/agent/workspace/web".to_string());
        assert_eq!(
            provisioned.directory().as_deref(),
            Some("/home/agent/workspace/web")
        );
        provisioned
            .repository_directories
            .push("/home/agent/workspace/api".to_string());
        assert_eq!(
            provisioned.directory().as_deref(),
            Some("/home/agent/workspace")
        );
    }
}
