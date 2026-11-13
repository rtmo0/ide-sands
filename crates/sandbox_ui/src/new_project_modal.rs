use std::path::PathBuf;

use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    PathPromptOptions, SharedString, Subscription, Task, WeakEntity,
};
use sbx::{NetworkMode, Sbx};
use ui::{Headline, HeadlineSize, Toggleable as _, prelude::*};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{ModalView, OpenMode, Workspace};

use crate::{
    AGENTS,
    design::{
        OptionCard, Slider, Tone, badge, caption, icon_button, primary_button, secondary_button,
        section_header, slider_field, stepper,
    },
    open_sandbox,
    projects_panel::ProjectsPanel,
    provisioning::{self, ProvisionStep, Provisioned, StepState},
    sandbox_panel::parse_port_mapping,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Sandbox,
    Repositories,
    Agent,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    LocalRepository,
    GitUrl,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProjectSource {
    LocalRepository(PathBuf),
    GitUrl(String),
}

impl ProjectSource {
    pub(crate) fn display(&self) -> String {
        match self {
            ProjectSource::LocalRepository(path) => path.to_string_lossy().into_owned(),
            ProjectSource::GitUrl(url) => url.clone(),
        }
    }

    /// The folder a clone of the repository gets inside the sandbox workspace.
    pub(crate) fn clone_directory_name(&self) -> Option<String> {
        repository_directory_name(&self.display())
    }

    /// The repository's folder or URL basename, for progress messages.
    pub(crate) fn short_name(&self) -> String {
        match self {
            ProjectSource::LocalRepository(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.display()),
            ProjectSource::GitUrl(url) => {
                repository_directory_name(url).unwrap_or_else(|| url.clone())
            }
        }
    }
}

/// The first local repository becomes the sandbox workspace instead of being cloned into it.
pub(crate) fn primary_repository(repositories: &[ProjectSource]) -> Option<usize> {
    repositories
        .iter()
        .position(|source| matches!(source, ProjectSource::LocalRepository(_)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SandboxSettings {
    pub network_mode: NetworkMode,
    pub allowed_hosts: Vec<String>,
    pub cpus: Option<u32>,
    pub memory: Option<String>,
    pub publish: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectPlan {
    pub name: String,
    pub agent: &'static str,
    pub settings: SandboxSettings,
    pub repositories: Vec<ProjectSource>,
}

enum CreationState {
    Editing,
    Creating(SharedString),
    Failed(SharedString),
}

/// The project being set up, step by step.
struct Provisioning {
    plan: ProjectPlan,
    steps: Vec<ProvisionStep>,
    provisioned: Provisioned,
}

pub(crate) struct NewProjectModal {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    step: Step,
    name: Entity<InputField>,
    network_mode: NetworkMode,
    allowed_hosts: Entity<InputField>,
    host: HostResources,
    cpu_slider: Entity<Slider>,
    memory_slider: Entity<Slider>,
    ports: Entity<InputField>,
    repositories: Vec<ProjectSource>,
    source_kind: SourceKind,
    local_path: Entity<InputField>,
    git_url: Entity<InputField>,
    agent: usize,
    state: CreationState,
    provisioning: Option<Provisioning>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Memory slider granularity; `sbx create --memory` takes binary units down to 512 MiB.
const MEMORY_STEP_GIB: f64 = 0.5;

/// The host limits `sbx create` applies: CPUs up to the host's, memory from 512 MiB up to 75% of
/// the host, defaulting to half of it (at most 32 GiB).
#[derive(Clone, Copy, Debug, PartialEq)]
struct HostResources {
    cpus: u32,
    memory_gib: f64,
}

impl HostResources {
    fn detect() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|count| count.get() as u32)
            .unwrap_or(4);
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let memory_gib = system.total_memory() as f64 / 1024f64.powi(3);
        Self {
            cpus,
            memory_gib: if memory_gib > 0.0 { memory_gib } else { 8.0 },
        }
    }

    fn snap(gib: f64) -> f64 {
        ((gib / MEMORY_STEP_GIB).floor() * MEMORY_STEP_GIB).max(MEMORY_STEP_GIB)
    }

    fn total_memory_gib(self) -> f64 {
        Self::snap(self.memory_gib)
    }

    fn max_memory_gib(self) -> f64 {
        Self::snap(self.memory_gib * 0.75)
    }

    fn default_memory_gib(self) -> f64 {
        Self::snap((self.memory_gib * 0.5).min(32.0)).min(self.max_memory_gib())
    }

    /// The `sbx create` limits for the slider values; values equal to the defaults are left to
    /// `sbx`, so they keep following the host.
    fn limits(self, cpus: f64, memory_gib: f64) -> (Option<u32>, Option<String>) {
        let cpus = cpus.round() as u32;
        let cpus = (cpus < self.cpus).then_some(cpus.max(1));
        let memory = ((memory_gib - self.default_memory_gib()).abs() > f64::EPSILON)
            .then(|| format!("{}m", (memory_gib * 1024.0).round() as u64));
        (cpus, memory)
    }
}

fn format_gib(gib: f64) -> String {
    if gib.fract() == 0.0 {
        format!("{gib:.0} GB")
    } else {
        format!("{gib:.1} GB")
    }
}

impl NewProjectModal {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            InputField::new(window, cx, "Defaults to the first repository's name")
                .label("Project name")
                .tab_index(1)
        });
        let allowed_hosts = cx.new(|cx| {
            InputField::new(window, cx, "api.example.com, *.internal.example.com")
                .label("Also allow")
                .tab_index(2)
        });
        let host = HostResources::detect();
        let cpu_slider = cx.new(|_| Slider::new(host.cpus as f64, 1.0, host.cpus as f64, 1.0));
        let memory_slider = cx.new(|_| {
            Slider::new(
                host.default_memory_gib(),
                MEMORY_STEP_GIB,
                host.total_memory_gib(),
                MEMORY_STEP_GIB,
            )
            .with_limit(host.max_memory_gib())
        });
        let _subscriptions = vec![
            cx.observe(&cpu_slider, |_, _, cx| cx.notify()),
            cx.observe(&memory_slider, |_, _, cx| cx.notify()),
        ];
        let ports = cx.new(|cx| {
            InputField::new(window, cx, "3000, 8080:80")
                .label("Published ports")
                .tab_index(5)
        });
        let local_path =
            cx.new(|cx| InputField::new(window, cx, "/path/to/git/repository").tab_index(1));
        let git_url = cx.new(|cx| {
            InputField::new(window, cx, "https://github.com/owner/repository.git").tab_index(1)
        });
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            step: Step::Sandbox,
            name,
            network_mode: NetworkMode::Balanced,
            allowed_hosts,
            host,
            cpu_slider,
            memory_slider,
            ports,
            repositories: Vec::new(),
            source_kind: SourceKind::LocalRepository,
            local_path,
            git_url,
            agent: 0,
            state: CreationState::Editing,
            provisioning: None,
            _task: None,
            _subscriptions,
        }
    }

    fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.state = CreationState::Failed(message.into());
        cx.notify();
    }

    fn sandbox_settings(&self, cx: &App) -> Result<SandboxSettings, String> {
        let (cpus, memory) = self.host.limits(
            self.cpu_slider.read(cx).value(),
            self.memory_slider.read(cx).value(),
        );
        let allowed_hosts = if self.network_mode == NetworkMode::AllowAll {
            Vec::new()
        } else {
            split_list(&self.allowed_hosts.read(cx).text(cx))
        };
        Ok(SandboxSettings {
            network_mode: self.network_mode,
            allowed_hosts,
            cpus,
            memory,
            publish: port_mappings(&self.ports.read(cx).text(cx))?,
        })
    }

    fn go_to(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        self.step = step;
        self.state = CreationState::Editing;
        match step {
            Step::Sandbox => self.name.focus_handle(cx).focus(window, cx),
            Step::Repositories => self.source_field().focus_handle(cx).focus(window, cx),
            Step::Agent => self.focus_handle.focus(window, cx),
        }
        cx.notify();
    }

    fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.step {
            Step::Sandbox => {
                let typed_name = self.name.read(cx).text(cx);
                let typed_name = typed_name.trim();
                let validation = self.sandbox_settings(cx).map(|_| ()).and_then(|()| {
                    match typed_name.is_empty() {
                        true => Ok(()),
                        false => validate_name(typed_name),
                    }
                });
                match validation {
                    Ok(()) => self.go_to(Step::Repositories, window, cx),
                    Err(message) => self.fail(message, cx),
                }
            }
            Step::Repositories => {
                // A source typed but not yet added is what the user means to continue with.
                if !self.source_field().read(cx).text(cx).trim().is_empty()
                    && !self.add_repository(window, cx)
                {
                    return;
                }
                match validate_repositories(&self.repositories) {
                    Ok(()) => self.go_to(Step::Agent, window, cx),
                    Err(message) => self.fail(message, cx),
                }
            }
            Step::Agent => self.create(window, cx),
        }
    }

    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.step {
            Step::Sandbox => {}
            Step::Repositories => self.go_to(Step::Sandbox, window, cx),
            Step::Agent => self.go_to(Step::Repositories, window, cx),
        }
    }

    fn source_field(&self) -> &Entity<InputField> {
        match self.source_kind {
            SourceKind::LocalRepository => &self.local_path,
            SourceKind::GitUrl => &self.git_url,
        }
    }

    /// Adds the typed source to the list and returns whether it was accepted.
    fn add_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let text = self.source_field().read(cx).text(cx).trim().to_string();
        let source = match self.source_kind {
            SourceKind::LocalRepository => {
                let path = PathBuf::from(&text);
                if !path.is_absolute() {
                    self.fail("Choose a local folder by its full path", cx);
                    return false;
                }
                ProjectSource::LocalRepository(path)
            }
            SourceKind::GitUrl => ProjectSource::GitUrl(text),
        };
        let mut repositories = self.repositories.clone();
        repositories.push(source);
        if let Err(message) = validate_repositories(&repositories) {
            self.fail(message, cx);
            return false;
        }
        self.repositories = repositories;
        self.source_field()
            .update(cx, |field, cx| field.clear(window, cx));
        self.state = CreationState::Editing;
        cx.notify();
        true
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
                    this.update(cx, |this, cx| this.fail(format!("{error:#}"), cx))
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
                    this.add_repository(window, cx);
                })
                .log_err();
            }
        }));
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.state, CreationState::Creating(_)) {
            return;
        }
        let settings = match self.sandbox_settings(cx) {
            Ok(settings) => settings,
            Err(message) => return self.fail(message, cx),
        };
        if let Err(message) = validate_repositories(&self.repositories) {
            return self.fail(message, cx);
        }
        let name = match project_name(
            self.name.read(cx).text(cx).trim(),
            self.repositories.first(),
        ) {
            Ok(name) => name,
            Err(message) => return self.fail(message, cx),
        };
        let Some((agent, _)) = AGENTS.get(self.agent) else {
            return self.fail("Choose an agent", cx);
        };
        let plan = ProjectPlan {
            name,
            agent,
            settings,
            repositories: self.repositories.clone(),
        };

        self.provisioning = Some(Provisioning {
            steps: provisioning::steps(&plan),
            plan,
            provisioned: Provisioned::default(),
        });
        self.run_provisioning(window, cx);
    }

    /// Runs every step from the first; steps whose work is already done skip themselves, so this
    /// also retries after a failure.
    fn run_provisioning(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provisioning) = self.provisioning.as_mut() else {
            return;
        };
        for step in &mut provisioning.steps {
            step.state = StepState::Waiting;
        }
        let plan = provisioning.plan.clone();
        let kinds = provisioning
            .steps
            .iter()
            .map(|step| step.kind.clone())
            .collect::<Vec<_>>();
        let mut provisioned = provisioning.provisioned.clone();
        self.state = CreationState::Creating("Setting up the project…".into());
        cx.notify();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let sbx = match Sbx::locate() {
                Ok(sbx) => sbx,
                Err(error) => {
                    this.update(cx, |this, cx| this.fail(error.to_string(), cx))
                        .log_err();
                    return;
                }
            };
            for (index, kind) in kinds.into_iter().enumerate() {
                this.update(cx, |this, cx| {
                    this.set_step_state(index, StepState::Running, cx)
                })
                .log_err();
                let (result, updated) = cx
                    .background_spawn({
                        let sbx = sbx.clone();
                        let plan = plan.clone();
                        let mut provisioned = provisioned.clone();
                        async move {
                            let result =
                                provisioning::run_step(&sbx, &plan, &kind, &mut provisioned).await;
                            (result, provisioned)
                        }
                    })
                    .await;
                provisioned = updated;
                let failed = result.is_err();
                let state =
                    result.unwrap_or_else(|error| StepState::Failed(format!("{error:#}").into()));
                let saved = provisioned.clone();
                this.update(cx, |this, cx| {
                    if let Some(provisioning) = this.provisioning.as_mut() {
                        provisioning.provisioned = saved;
                    }
                    this.set_step_state(index, state, cx);
                    if failed {
                        this.fail(
                            "Setting up the project stopped. Fix the problem and retry, or \
                             remove the sandbox.",
                            cx,
                        );
                    }
                })
                .log_err();
                if failed {
                    return;
                }
            }
            let directory = provisioned.directory();
            this.update_in(cx, |this, window, cx| {
                if let Some(workspace) = this.workspace.upgrade()
                    && let Some(panel) = workspace.read(cx).panel::<ProjectsPanel>(cx)
                {
                    panel.update(cx, |panel, cx| panel.refresh(window, cx));
                }
                cx.emit(DismissEvent);
                open_sandbox(plan.name.clone(), directory, OpenMode::Activate, window, cx);
            })
            .log_err();
        }));
    }

    fn set_step_state(&mut self, index: usize, state: StepState, cx: &mut Context<Self>) {
        if let Some(step) = self
            .provisioning
            .as_mut()
            .and_then(|provisioning| provisioning.steps.get_mut(index))
        {
            step.state = state;
            cx.notify();
        }
    }

    /// Removes the sandbox this creation made and goes back to the settings.
    fn remove_sandbox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provisioning) = self.provisioning.as_ref() else {
            return;
        };
        if !provisioning.provisioned.created_sandbox {
            self.provisioning = None;
            self.state = CreationState::Editing;
            cx.notify();
            return;
        }
        let name = provisioning.plan.name.clone();
        self.state = CreationState::Creating(format!("Removing sandbox {name}…").into());
        cx.notify();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { Sbx::locate()?.remove(&name).await })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.provisioning = None;
                    this.state = CreationState::Editing;
                    cx.notify();
                }
                Err(error) => this.fail(format!("Removing the sandbox failed: {error}"), cx),
            })
            .log_err();
        }));
    }

    fn render_provisioning(&self, provisioning: &Provisioning) -> impl IntoElement {
        v_flex()
            .gap_1()
            .children(provisioning.steps.iter().map(|step| {
                let (icon, color) = match &step.state {
                    StepState::Waiting => (IconName::Circle, Color::Muted),
                    StepState::Running => (IconName::ArrowCircle, Color::Accent),
                    StepState::Done(_) => (IconName::Check, Color::Success),
                    StepState::Skipped(_) => (IconName::Dash, Color::Muted),
                    StepState::Warning(_) => (IconName::Warning, Color::Warning),
                    StepState::Failed(_) => (IconName::XCircle, Color::Error),
                };
                let detail = match &step.state {
                    StepState::Done(detail) => detail.clone(),
                    StepState::Skipped(detail)
                    | StepState::Warning(detail)
                    | StepState::Failed(detail) => Some(detail.clone()),
                    StepState::Waiting | StepState::Running => None,
                };
                let detail_color = match &step.state {
                    StepState::Failed(_) => Color::Error,
                    StepState::Warning(_) => Color::Warning,
                    _ => Color::Muted,
                };
                h_flex()
                    .py_1p5()
                    .gap_3()
                    .items_start()
                    .child(
                        div()
                            .pt_0p5()
                            .child(Icon::new(icon).size(IconSize::Small).color(color)),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(Label::new(step.label.clone()).size(LabelSize::Small).color(
                                if step.state == StepState::Waiting {
                                    Color::Muted
                                } else {
                                    Color::Default
                                },
                            ))
                            .children(detail.map(|detail| {
                                Label::new(detail)
                                    .size(LabelSize::Small)
                                    .color(detail_color)
                            })),
                    )
            }))
    }
}

fn split_list(text: &str) -> Vec<String> {
    text.split(|character: char| character == ',' || character.is_whitespace())
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn port_mappings(text: &str) -> Result<Vec<String>, String> {
    split_list(text)
        .into_iter()
        .map(|item| match parse_port_mapping(&item) {
            Some((Some(host_port), sandbox_port)) => Ok(format!("{host_port}:{sandbox_port}")),
            Some((None, sandbox_port)) => Ok(sandbox_port.to_string()),
            None => Err(format!(
                "{item:?} is not a port. Use 3000, or 8080:3000 for a fixed host port."
            )),
        })
        .collect()
}

/// Every cloned repository needs a distinct folder inside the sandbox workspace.
fn validate_repositories(repositories: &[ProjectSource]) -> Result<(), String> {
    if repositories.is_empty() {
        return Err("Add at least one repository".to_string());
    }
    let primary = primary_repository(repositories);
    let mut directory_names = collections::HashSet::default();
    for (index, source) in repositories.iter().enumerate() {
        if let ProjectSource::GitUrl(url) = source
            && url.is_empty()
        {
            return Err("Enter a Git URL".to_string());
        }
        if Some(index) == primary {
            continue;
        }
        let directory_name = source.clone_directory_name().ok_or_else(|| {
            format!(
                "Cannot derive a folder name from {:?}: use letters, digits, '-', '_' and '.'",
                source.display()
            )
        })?;
        if !directory_names.insert(directory_name.clone()) {
            return Err(format!(
                "Two repositories would share the folder {directory_name:?}"
            ));
        }
    }
    Ok(())
}

/// Uses the typed name, or derives one from the first repository, and checks it against `sbx`
/// naming rules.
fn project_name(typed: &str, first: Option<&ProjectSource>) -> Result<String, String> {
    let name = if typed.is_empty() {
        first
            .and_then(|source| match source {
                ProjectSource::LocalRepository(path) => path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
                ProjectSource::GitUrl(url) => repository_directory_name(url),
            })
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

impl NewProjectModal {
    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.state, CreationState::Creating(_)) {
            cx.emit(DismissEvent);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.next(window, cx);
    }

    fn render_sandbox_step(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let network_card = |mode: NetworkMode,
                            icon: IconName,
                            title: &'static str,
                            description: &'static str,
                            cx: &mut Context<Self>| {
            OptionCard::new(SharedString::from(title), icon, title)
                .description(description)
                .selected(self.network_mode == mode)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.network_mode = mode;
                    cx.notify();
                }))
        };
        let cpus = self.cpu_slider.read(cx).value();
        let memory = self.memory_slider.read(cx).value();
        let cpu_text = if cpus as u32 >= self.host.cpus {
            format!("{} (all)", self.host.cpus)
        } else {
            format!("{cpus:.0}")
        };
        let memory_text = if (memory - self.host.default_memory_gib()).abs() < f64::EPSILON {
            format!("{} (default)", format_gib(memory))
        } else {
            format_gib(memory)
        };
        v_flex()
            .gap_5()
            .child(self.name.clone())
            .child(
                v_flex()
                    .gap_2()
                    .child(section_header("Network", None))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_stretch()
                            .child(network_card(
                                NetworkMode::AllowAll,
                                IconName::Public,
                                "Allow all",
                                "Any host on the internet.",
                                cx,
                            ))
                            .child(network_card(
                                NetworkMode::Balanced,
                                IconName::Box,
                                "Balanced",
                                "Git, package registries, toolchains and model APIs.",
                                cx,
                            ))
                            .child(network_card(
                                NetworkMode::DenyAll,
                                IconName::Lock,
                                "Deny all",
                                "Only the agent's provider and the hosts below.",
                                cx,
                            )),
                    )
                    .when(self.network_mode != NetworkMode::AllowAll, |this| {
                        this.child(self.allowed_hosts.clone()).child(caption(
                            "Hostnames, wildcards, IP addresses or CIDRs, separated by commas.",
                        ))
                    }),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_header("Resources", None))
                    .child(
                        h_flex()
                            .gap_6()
                            .child(div().flex_1().child(slider_field(
                                "CPU cores",
                                cpu_text,
                                &self.cpu_slider,
                                "1",
                                self.host.cpus.to_string(),
                            )))
                            .child(div().flex_1().child(slider_field(
                                "Memory",
                                memory_text,
                                &self.memory_slider,
                                format_gib(MEMORY_STEP_GIB),
                                format!("{} total", format_gib(self.host.total_memory_gib())),
                            ))),
                    )
                    .child(caption(format!(
                        "Resources are fixed when the sandbox is created. A sandbox can use up to \
                         {} of this Mac's {}: sbx leaves the rest to macOS.",
                        format_gib(self.host.max_memory_gib()),
                        format_gib(self.host.total_memory_gib()),
                    ))),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_header("Ports", None))
                    .child(self.ports.clone())
                    .child(caption(
                        "Sandbox ports to publish on 127.0.0.1. HOST:SANDBOX fixes the host port.",
                    )),
            )
    }

    fn render_repositories_step(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let source_input = h_flex()
            .gap_2()
            .items_center()
            .child(div().flex_1().child(self.source_field().clone()))
            .when(self.source_kind == SourceKind::LocalRepository, |this| {
                this.child(
                    secondary_button("browse-local-folder", "Browse…")
                        .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx))),
                )
            })
            .child(
                primary_button("add-repository", "Add").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.add_repository(window, cx);
                    },
                )),
            );
        let source_hint = match self.source_kind {
            SourceKind::LocalRepository => {
                "The first local repository is the main one: the sandbox works on its private \
                 clone, and commits reach the host as the sandbox-<name> remote. Other local \
                 repositories are mounted read-only and cloned inside."
            }
            SourceKind::GitUrl => {
                "Cloned inside the sandbox. Private repositories need `sbx secret set github`."
            }
        };
        let primary = primary_repository(&self.repositories);
        let repository_count = self.repositories.len();

        v_flex()
            .gap_5()
            .child(
                v_flex()
                    .gap_2()
                    .child(section_header(
                        "Repositories",
                        (repository_count > 0)
                            .then(|| caption(repository_count.to_string()).into_any_element()),
                    ))
                    .when(self.repositories.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .py_6()
                                .gap_1()
                                .items_center()
                                .rounded_md()
                                .border_1()
                                .border_dashed()
                                .border_color(colors.border)
                                .child(
                                    Icon::new(IconName::FolderAdd)
                                        .size(IconSize::Medium)
                                        .color(Color::Muted),
                                )
                                .child(Label::new("No repositories yet").size(LabelSize::Small))
                                .child(caption("Add a local folder or a Git URL below.")),
                        )
                    })
                    .children(self.repositories.iter().enumerate().map(|(index, source)| {
                        let (icon, name) = match source {
                            ProjectSource::LocalRepository(path) => (
                                IconName::Folder,
                                path.file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| source.display()),
                            ),
                            ProjectSource::GitUrl(url) => (
                                IconName::GitBranch,
                                repository_directory_name(url).unwrap_or_else(|| url.clone()),
                            ),
                        };
                        h_flex()
                            .px_2p5()
                            .py_2()
                            .gap_2p5()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.surface_background)
                            .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(
                                                Label::new(name)
                                                    .size(LabelSize::Small)
                                                    .weight(FontWeight::MEDIUM),
                                            )
                                            .when(Some(index) == primary, |this| {
                                                this.child(badge("primary", Tone::Info, cx))
                                            }),
                                    )
                                    .child(
                                        Label::new(source.display())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .buffer_font(cx)
                                            .truncate(),
                                    ),
                            )
                            .child(
                                icon_button(
                                    ("remove-repository", index),
                                    IconName::Close,
                                    "Remove",
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if index < this.repositories.len() {
                                            this.repositories.remove(index);
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                    })),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(section_header("Add a repository", None))
                    .child(
                        h_flex().gap_2().children(
                            [
                                (
                                    SourceKind::LocalRepository,
                                    IconName::Folder,
                                    "Local folder",
                                ),
                                (SourceKind::GitUrl, IconName::Github, "Git URL"),
                            ]
                            .into_iter()
                            .map(|(kind, icon, title)| {
                                Button::new(SharedString::from(title), title)
                                    .style(ButtonStyle::Subtle)
                                    .start_icon(Icon::new(icon).size(IconSize::Small))
                                    .toggle_state(self.source_kind == kind)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.source_kind = kind;
                                        this.source_field().focus_handle(cx).focus(window, cx);
                                        cx.notify();
                                    }))
                            }),
                        ),
                    )
                    .child(source_input)
                    .child(caption(source_hint)),
            )
    }

    fn render_agent_step(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cards = AGENTS
            .iter()
            .enumerate()
            .map(|(index, (name, display_name))| {
                OptionCard::new(("agent", index), agent_icon(name), *display_name)
                    .description(agent_description(name))
                    .selected(index == self.agent)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.agent = index;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        let mut cards = cards.into_iter();
        while let Some(first) = cards.next() {
            let second = cards
                .next()
                .unwrap_or_else(|| div().flex_1().into_any_element());
            rows.push(h_flex().gap_2().items_stretch().child(first).child(second));
        }
        v_flex()
            .gap_2()
            .child(section_header("Agent", None))
            .children(rows)
            .child(caption(
                "The agent runs only inside the sandbox and signs in through `sbx` secrets.",
            ))
    }
}

pub(crate) fn agent_icon(agent: &str) -> IconName {
    match agent {
        "shell" => IconName::Terminal,
        "claude" => IconName::AiClaude,
        "codex" => IconName::AiOpenAi,
        "gemini" => IconName::AiGemini,
        "opencode" => IconName::AiOpenCode,
        "copilot" => IconName::Github,
        "docker-agent" => IconName::Box,
        _ => IconName::Sparkle,
    }
}

fn agent_description(agent: &str) -> &'static str {
    match agent {
        "shell" => "A plain sandbox: terminals and tools, no coding agent.",
        "claude" => "Anthropic's coding agent.",
        "codex" => "OpenAI's coding agent.",
        "copilot" => "GitHub Copilot CLI.",
        "cursor" => "Cursor's command-line agent.",
        "gemini" => "Google's Gemini CLI.",
        "opencode" => "Open-source agent for any model provider.",
        "kiro" => "Amazon's spec-driven agent.",
        "devin" => "Cognition's Devin CLI.",
        "droid" => "Factory's Droid agent.",
        "docker-agent" => "Docker's agent.",
        _ => "",
    }
}

impl ModalView for NewProjectModal {}

impl EventEmitter<DismissEvent> for NewProjectModal {}

impl Focusable for NewProjectModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.step {
            Step::Sandbox => self.name.focus_handle(cx),
            Step::Repositories => self.source_field().focus_handle(cx),
            Step::Agent => self.focus_handle.clone(),
        }
    }
}

impl Render for NewProjectModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let creating = matches!(self.state, CreationState::Creating(_));
        let status = match &self.state {
            CreationState::Editing => None,
            CreationState::Creating(message) => Some(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::ArrowCircle)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(caption(message.clone())),
            ),
            CreationState::Failed(message) => Some(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(message.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    ),
            ),
        };
        let step_index = match self.step {
            Step::Sandbox => 0,
            Step::Repositories => 1,
            Step::Agent => 2,
        };
        let body = match (&self.provisioning, self.step) {
            (Some(provisioning), _) => self.render_provisioning(provisioning).into_any_element(),
            (None, Step::Sandbox) => self.render_sandbox_step(cx).into_any_element(),
            (None, Step::Repositories) => self.render_repositories_step(cx).into_any_element(),
            (None, Step::Agent) => self.render_agent_step(cx).into_any_element(),
        };
        let provisioning_stopped = self.provisioning.is_some() && !creating;
        let first_step = self.step == Step::Sandbox;
        let last_step = self.step == Step::Agent;
        let colors = cx.theme().colors();

        v_flex()
            .key_context("NewProjectModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(46.))
            .h(rems(46.))
            .overflow_hidden()
            .child(
                v_flex()
                    .px_5()
                    .pt_4()
                    .pb_3()
                    .gap_3()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(
                        v_flex()
                            .child(Headline::new("New Project").size(HeadlineSize::Small))
                            .child(caption(
                                "A sandbox with your repositories and one coding agent.",
                            )),
                    )
                    .child(stepper(
                        &["Sandbox", "Repositories", "Agent"],
                        step_index,
                        cx,
                    )),
            )
            .child(
                div()
                    .id("new-project-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_5()
                    .py_4()
                    .child(body),
            )
            .child(
                h_flex()
                    .px_5()
                    .py_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(div().flex_1().min_w_0().children(status))
                    .when(provisioning_stopped, |this| {
                        this.child(
                            Button::new("remove-new-project", "Remove sandbox").on_click(
                                cx.listener(|this, _, window, cx| this.remove_sandbox(window, cx)),
                            ),
                        )
                        .child(
                            primary_button("retry-new-project", "Retry").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.run_provisioning(window, cx)
                                }),
                            ),
                        )
                    })
                    .when(!provisioning_stopped, |this| {
                        this.child(if first_step {
                            Button::new("cancel-new-project", "Cancel")
                                .disabled(creating)
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
                        } else {
                            Button::new("back-new-project", "Back")
                                .disabled(creating)
                                .on_click(cx.listener(|this, _, window, cx| this.back(window, cx)))
                        })
                        .child(
                            primary_button(
                                "next-new-project",
                                if last_step {
                                    "Create Project"
                                } else {
                                    "Continue"
                                },
                            )
                            .disabled(creating)
                            .on_click(cx.listener(|this, _, window, cx| this.next(window, cx))),
                        )
                    }),
            )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;

    pub(crate) fn plan(name: &str, repositories: Vec<ProjectSource>) -> ProjectPlan {
        ProjectPlan {
            name: name.to_string(),
            agent: "shell",
            settings: SandboxSettings {
                network_mode: NetworkMode::Balanced,
                allowed_hosts: Vec::new(),
                cpus: None,
                memory: None,
                publish: vec!["18080:8080".to_string()],
            },
            repositories,
        }
    }

    pub(crate) fn init_repository(path: &std::path::Path) -> TestResult {
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
        std::fs::create_dir_all(path)?;
        std::fs::write(path.join("README.md"), "local\n")?;
        let git = |arguments: &[&str]| {
            smol::block_on(
                util::command::new_command("git")
                    .args(arguments)
                    .current_dir(path)
                    .status(),
            )
        };
        assert!(git(&["init", "-q"])?.success());
        assert!(git(&["add", "."])?.success());
        assert!(
            git(&[
                "-c",
                "user.name=Sands",
                "-c",
                "user.email=sands@example.com",
                "commit",
                "-qm",
                "Initial commit",
            ])?
            .success()
        );
        Ok(())
    }

    /// Creates and removes real sandboxes: `cargo test -p sandbox_ui -- --ignored`.
    #[test]
    #[ignore]
    fn creates_projects_with_real_sbx() -> TestResult {
        smol::block_on(async {
            let sbx = Sbx::locate()?;

            let directory = provisioning::provision(&plan(
                "sands-test-git",
                vec![ProjectSource::GitUrl(
                    "https://github.com/octocat/Hello-World.git".to_string(),
                )],
            ))
            .await;
            let directory = match directory {
                Ok(directory) => directory,
                Err(error) => {
                    if let Err(remove_error) = sbx.remove("sands-test-git").await {
                        eprintln!("cleaning up sands-test-git failed: {remove_error}");
                    }
                    return Err(error.into());
                }
            };
            let listing = sbx
                .exec("sands-test-git", &["ls", directory.as_str()])
                .await;
            let blocked = sbx
                .exec(
                    "sands-test-git",
                    &[
                        "curl",
                        "-s",
                        "-o",
                        "/dev/null",
                        "-w",
                        "%{http_code}",
                        "https://example.com",
                    ],
                )
                .await;
            let ports = sbx.ports("sands-test-git").await;
            sbx.remove("sands-test-git").await?;
            assert!(directory.ends_with("/Hello-World"), "{directory}");
            assert!(String::from_utf8_lossy(&listing?.stdout).contains("README"));
            assert_eq!(String::from_utf8_lossy(&blocked?.stdout), "403");
            assert!(ports?.iter().any(|port| port.sandbox_port == 8080));

            let root = std::env::temp_dir().join("sands-test-local-repositories");
            let primary = root.join("primary");
            let secondary = root.join("secondary");
            init_repository(&primary)?;
            init_repository(&secondary)?;
            let primary = primary.canonicalize()?;
            let secondary = secondary.canonicalize()?;

            let directory = provisioning::provision(&plan(
                "sands-test-local",
                vec![
                    ProjectSource::LocalRepository(primary.clone()),
                    ProjectSource::LocalRepository(secondary.clone()),
                ],
            ))
            .await;
            let nested = format!("{}/secondary/README.md", primary.display());
            let nested_listing = sbx.exec("sands-test-local", &["cat", &nested]).await;
            let status = sbx
                .exec(
                    "sands-test-local",
                    &[
                        "git",
                        "-C",
                        &primary.to_string_lossy(),
                        "status",
                        "--porcelain",
                    ],
                )
                .await;
            let host_write = sbx
                .exec(
                    "sands-test-local",
                    &["touch", &format!("{}/written", secondary.display())],
                )
                .await;
            sbx.remove("sands-test-local").await?;
            let host_untouched = !secondary.join("written").exists();
            std::fs::remove_dir_all(&root)?;
            assert_eq!(directory?, primary.to_string_lossy());
            assert_eq!(String::from_utf8_lossy(&nested_listing?.stdout), "local\n");
            // The nested clone stays out of the workspace's status; only the project settings
            // Sands just wrote are new.
            assert_eq!(String::from_utf8_lossy(&status?.stdout), "?? .sands/\n");
            assert!(!host_write?.status.success());
            assert!(host_untouched);
            for project in ["sands-test-git", "sands-test-local"] {
                let snapshot = paths::data_dir().join("projects").join(project);
                if snapshot.exists() {
                    std::fs::remove_dir_all(snapshot)?;
                }
            }
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
    fn maps_sliders_to_sbx_limits() {
        let host = HostResources {
            cpus: 14,
            memory_gib: 24.0,
        };
        assert_eq!(host.default_memory_gib(), 12.0);
        assert_eq!(host.max_memory_gib(), 18.0);
        assert_eq!(host.total_memory_gib(), 24.0);
        assert_eq!(host.limits(14.0, 12.0), (None, None));
        assert_eq!(host.limits(4.0, 8.5), (Some(4), Some("8704m".to_string())));
        let big = HostResources {
            cpus: 64,
            memory_gib: 256.0,
        };
        assert_eq!(big.default_memory_gib(), 32.0);
        assert_eq!(big.max_memory_gib(), 192.0);
        assert_eq!(format_gib(0.5), "0.5 GB");
        assert_eq!(format_gib(12.0), "12 GB");
    }

    #[test]
    fn parses_port_lists_and_hosts() {
        assert_eq!(port_mappings(""), Ok(Vec::new()));
        assert_eq!(
            port_mappings("3000, 8080:80 5173"),
            Ok(vec![
                "3000".to_string(),
                "8080:80".to_string(),
                "5173".to_string()
            ])
        );
        assert!(port_mappings("3000, web").is_err());
        assert_eq!(
            split_list(" api.example.com,*.example.org  10.0.0.0/8 "),
            ["api.example.com", "*.example.org", "10.0.0.0/8"]
        );
    }

    #[test]
    fn validates_repository_lists() {
        assert!(validate_repositories(&[]).is_err());
        assert!(validate_repositories(&[ProjectSource::GitUrl(String::new())]).is_err());
        assert!(
            validate_repositories(&[
                ProjectSource::LocalRepository(PathBuf::from("/work/api")),
                ProjectSource::GitUrl("https://github.com/owner/web.git".into()),
            ])
            .is_ok()
        );
        assert!(
            validate_repositories(&[
                ProjectSource::GitUrl("https://github.com/owner/api.git".into()),
                ProjectSource::GitUrl("https://gitlab.com/group/api".into()),
            ])
            .is_err()
        );
        assert!(
            validate_repositories(&[ProjectSource::LocalRepository(PathBuf::from(
                "/work/My Repo"
            ))])
            .is_ok()
        );
        assert!(
            validate_repositories(&[
                ProjectSource::LocalRepository(PathBuf::from("/work/api")),
                ProjectSource::LocalRepository(PathBuf::from("/work/My Docs")),
            ])
            .is_err()
        );
    }

    #[test]
    fn derives_project_names_from_the_first_repository() {
        assert_eq!(
            project_name(
                "",
                Some(&ProjectSource::GitUrl(
                    "https://github.com/rtmo0/IDE_Sands.git".into()
                ))
            ),
            Ok("ide-sands".to_string())
        );
        assert_eq!(
            project_name(
                "",
                Some(&ProjectSource::LocalRepository(PathBuf::from(
                    "/work/My Repo"
                )))
            ),
            Ok("my-repo".to_string())
        );
        assert_eq!(project_name("custom", None), Ok("custom".to_string()));
        assert!(project_name("", None).is_err());
    }
}
