use anyhow::{Context as _, bail};
use collections::{HashMap, HashSet};
use serde::Deserialize;
use util::ResultExt;

use crate::{EnvVariableReplacer, TaskTemplate, TaskTemplates, VariableName};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct TaskOptions {
    cwd: Option<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

/// Platform-specific overrides (`"linux": { ... }`) of a task's command, arguments and options.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct PlatformOverrides {
    command: Option<String>,
    args: Option<Vec<String>>,
    options: Option<TaskOptions>,
}

/// An entry of the `inputs` array shared by `tasks.json` and `launch.json`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VsCodeInput {
    id: String,
    #[serde(default)]
    default: Option<serde_json::Value>,
}

/// Sands cannot prompt for inputs, so `${input:<id>}` resolves to the input's default value.
pub(crate) fn input_defaults(inputs: &[VsCodeInput]) -> Vec<(String, String)> {
    inputs
        .iter()
        .filter_map(|input| {
            let default = match input.default.as_ref()? {
                serde_json::Value::String(value) => value.clone(),
                other => other.to_string(),
            };
            Some((input.id.clone(), default))
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
struct VsCodeTaskDefinition {
    label: String,
    command: Option<Command>,
    other_attributes: HashMap<String, serde_json_lenient::Value>,
    options: Option<TaskOptions>,
}

impl<'de> serde::Deserialize<'de> for VsCodeTaskDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct TaskHelper {
            #[serde(default)]
            label: Option<String>,
            #[serde(flatten)]
            command: Option<Command>,
            #[serde(flatten)]
            other_attributes: HashMap<String, serde_json_lenient::Value>,
            options: Option<TaskOptions>,
        }

        let helper = TaskHelper::deserialize(deserializer)?;

        let label = helper
            .label
            .unwrap_or_else(|| generate_label(&helper.command));

        Ok(VsCodeTaskDefinition {
            label,
            command: helper.command,
            other_attributes: helper.other_attributes,
            options: helper.options,
        })
    }
}

#[derive(Clone, Deserialize, PartialEq, Debug)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
enum Command {
    Npm {
        script: String,
        path: Option<String>,
    },
    Shell {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Process {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Gulp {
        task: String,
    },
}

fn generate_label(command: &Option<Command>) -> String {
    match command {
        Some(Command::Npm { script, .. }) => format!("npm: {}", script),
        Some(Command::Gulp { task }) => format!("gulp: {}", task),
        Some(Command::Shell { command, .. } | Command::Process { command, .. }) => {
            if command.trim().is_empty() {
                "shell".to_string()
            } else {
                command.clone()
            }
        }
        None => "Untitled Task".to_string(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DependsOrder {
    Parallel,
    Sequence,
}

impl VsCodeTaskDefinition {
    fn depends_on(&self) -> Vec<String> {
        match self.other_attributes.get("dependsOn") {
            Some(serde_json_lenient::Value::String(label)) => vec![label.clone()],
            Some(serde_json_lenient::Value::Array(labels)) => labels
                .iter()
                .filter_map(|label| match label {
                    serde_json_lenient::Value::String(label) => Some(label.clone()),
                    // `{ "type": ..., "task": ... }` references tasks from other providers.
                    serde_json_lenient::Value::Object(reference) => reference
                        .get("task")
                        .and_then(|task| task.as_str())
                        .map(str::to_owned),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn depends_order(&self) -> DependsOrder {
        match self.other_attributes.get("dependsOrder") {
            Some(serde_json_lenient::Value::String(order)) if order == "sequence" => {
                DependsOrder::Sequence
            }
            _ => DependsOrder::Parallel,
        }
    }

    /// Sandboxes run Linux, so the `linux` block applies on every host.
    fn linux_overrides(&self) -> anyhow::Result<PlatformOverrides> {
        match self.other_attributes.get("linux") {
            Some(value) => serde_json_lenient::from_value(value.clone())
                .with_context(|| format!("invalid `linux` block in task `{}`", self.label)),
            None => Ok(PlatformOverrides::default()),
        }
    }

    /// Converts the task's own command, ignoring `dependsOn`. Tasks that only aggregate
    /// dependencies have no command and convert to `None`.
    fn own_template(&self, replacer: &EnvVariableReplacer) -> anyhow::Result<Option<TaskTemplate>> {
        let Some(command) = self.command.clone() else {
            if self.depends_on().is_empty() {
                bail!("Missing `type` field in task `{}`", self.label);
            }
            return Ok(None);
        };
        let overrides = self.linux_overrides()?;

        let (command, args, cwd) = match command {
            Command::Npm { script, path } => (
                "npm".to_owned(),
                vec!["run".to_string(), script],
                path.map(|path| {
                    format!("{}/{}", VariableName::WorktreeRoot.template_value(), path)
                }),
            ),
            Command::Shell { command, args } | Command::Process { command, args } => (
                overrides.command.unwrap_or(command),
                overrides.args.unwrap_or(args),
                None,
            ),
            Command::Gulp { task } => ("gulp".to_owned(), vec![task], None),
        };
        // Per VSC docs, only `command`, `args` and `options` support variable substitution.
        let command = replacer.replace(&command);
        let args: Vec<String> = args.into_iter().map(|arg| replacer.replace(&arg)).collect();
        if std::iter::once(&command)
            .chain(&args)
            .any(|part| part.contains("${input:"))
        {
            bail!(
                "task `{}` uses an input without a default value, which Sands cannot prompt for",
                self.label
            );
        }
        let mut template = TaskTemplate {
            label: self.label.clone(),
            command,
            args,
            cwd: cwd.map(|cwd| replacer.replace(&cwd)),
            ..TaskTemplate::default()
        };
        for options in [self.options.clone(), overrides.options]
            .into_iter()
            .flatten()
        {
            if let Some(cwd) = options.cwd {
                template.cwd = Some(replacer.replace(&cwd));
            }
            template.env.extend(options.env);
        }
        Ok(Some(template))
    }
}

/// Converts tasks, turning `dependsOn` into one shell command that runs the dependencies (in
/// parallel by default, like VS Code) and then the task itself.
struct TaskConverter<'a> {
    definitions: HashMap<&'a str, &'a VsCodeTaskDefinition>,
    replacer: &'a EnvVariableReplacer,
}

impl TaskConverter<'_> {
    fn convert(&self, definition: &VsCodeTaskDefinition) -> anyhow::Result<TaskTemplate> {
        let mut visiting = HashSet::default();
        let command = self.shell_command(definition, &mut visiting)?;
        let own = definition.own_template(self.replacer)?;
        if definition.depends_on().is_empty()
            && let Some(own) = own
        {
            return Ok(own);
        }
        Ok(TaskTemplate {
            label: definition.label.clone(),
            command,
            ..TaskTemplate::default()
        })
    }

    fn shell_command(
        &self,
        definition: &VsCodeTaskDefinition,
        visiting: &mut HashSet<String>,
    ) -> anyhow::Result<String> {
        if !visiting.insert(definition.label.clone()) {
            bail!("task `{}` depends on itself", definition.label);
        }
        let dependencies = definition
            .depends_on()
            .iter()
            .map(|label| {
                let dependency = self.definitions.get(label.as_str()).with_context(|| {
                    format!(
                        "task `{}` depends on unknown task `{label}`",
                        definition.label
                    )
                })?;
                self.shell_command(dependency, visiting)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        visiting.remove(&definition.label);

        let mut steps = Vec::new();
        match definition.depends_order() {
            DependsOrder::Sequence => steps.extend(dependencies),
            DependsOrder::Parallel if dependencies.len() <= 1 => steps.extend(dependencies),
            DependsOrder::Parallel => {
                let started = dependencies
                    .iter()
                    .enumerate()
                    .map(|(index, step)| format!("{step} & pid{index}=$!"))
                    .collect::<Vec<_>>()
                    .join("; ");
                let waited = (0..dependencies.len())
                    .map(|index| format!("wait $pid{index}"))
                    .collect::<Vec<_>>()
                    .join(" && ");
                steps.push(format!("{{ {started}; {waited}; }}"));
            }
        }
        if let Some(own) = definition.own_template(self.replacer)? {
            steps.push(shell_step(&own));
        }
        Ok(steps.join(" && "))
    }
}

/// One task as a subshell, so its `cd` and environment do not leak into the next step.
fn shell_step(template: &TaskTemplate) -> String {
    let mut step = String::from("(");
    if let Some(cwd) = &template.cwd {
        step.push_str(&format!("cd {} && ", quote_for_shell(cwd)));
    }
    let mut environment = template.env.iter().collect::<Vec<_>>();
    environment.sort();
    for (key, value) in environment {
        step.push_str(&format!("{key}={} ", quote_for_shell(value)));
    }
    step.push_str(&template.command);
    for arg in &template.args {
        step.push(' ');
        step.push_str(&quote_for_shell(arg));
    }
    step.push(')');
    step
}

/// Double quotes keep `$VARIABLE` expansion working, which VS Code variables are converted to.
fn quote_for_shell(value: &str) -> String {
    if !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_whitespace() || "\"'`;&|<>()".contains(character))
    {
        return value.to_owned();
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`");
    format!("\"{escaped}\"")
}

/// [`VsCodeTaskFile`] is a superset of Code's task definition format.
#[derive(Debug, Deserialize, PartialEq)]
pub struct VsCodeTaskFile {
    tasks: Vec<VsCodeTaskDefinition>,
    #[serde(default)]
    inputs: Vec<VsCodeInput>,
}

impl TryFrom<VsCodeTaskFile> for TaskTemplates {
    type Error = anyhow::Error;

    fn try_from(value: VsCodeTaskFile) -> Result<Self, Self::Error> {
        let replacer = EnvVariableReplacer::vscode().with_inputs(input_defaults(&value.inputs));
        let converter = TaskConverter {
            definitions: value
                .tasks
                .iter()
                .map(|definition| (definition.label.as_str(), definition))
                .collect(),
            replacer: &replacer,
        };
        let templates = value
            .tasks
            .iter()
            .filter_map(|definition| converter.convert(definition).log_err())
            .collect();
        Ok(Self(templates))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::{
        TaskTemplate, TaskTemplates, VsCodeTaskFile,
        vscode_format::{Command, VsCodeTaskDefinition},
    };

    use super::{EnvVariableReplacer, generate_label};

    fn compare_without_other_attributes(lhs: VsCodeTaskDefinition, rhs: VsCodeTaskDefinition) {
        assert_eq!(
            VsCodeTaskDefinition {
                other_attributes: Default::default(),
                ..lhs
            },
            VsCodeTaskDefinition {
                other_attributes: Default::default(),
                ..rhs
            },
        );
    }

    #[test]
    fn test_variable_substitution() {
        let replacer = EnvVariableReplacer::new(Default::default());
        assert_eq!(replacer.replace("Food"), "Food");
        // Unknown variables are left in tact.
        assert_eq!(
            replacer.replace("$PATH is an environment variable"),
            "$PATH is an environment variable"
        );
        assert_eq!(replacer.replace("${PATH}"), "${PATH}");
        assert_eq!(replacer.replace("${PATH:food}"), "${PATH:food}");
        // And now, the actual replacing
        let replacer = EnvVariableReplacer::new(HashMap::from_iter([(
            "PATH".to_owned(),
            "ZED_PATH".to_owned(),
        )]));
        assert_eq!(replacer.replace("Food"), "Food");
        assert_eq!(
            replacer.replace("$PATH is an environment variable"),
            "${ZED_PATH} is an environment variable"
        );
        assert_eq!(replacer.replace("${PATH}"), "${ZED_PATH}");
        assert_eq!(replacer.replace("${PATH:food}"), "${ZED_PATH:food}");
    }

    #[test]
    fn can_deserialize_gulp_tasks() {
        const GULP_TASKS: &str = include_str!("../test_data/tasks-gulp.json");
        let vscode_definitions: VsCodeTaskFile = serde_json_lenient::from_str(GULP_TASKS).unwrap();

        let expected = vec![VsCodeTaskDefinition {
            label: "gulp: build tests".to_string(),
            command: Some(Command::Gulp {
                task: "build".to_string(),
            }),
            other_attributes: Default::default(),
            options: None,
        }];

        assert_eq!(vscode_definitions.tasks.len(), expected.len());
        vscode_definitions
            .tasks
            .iter()
            .zip(expected)
            .for_each(|(lhs, rhs)| compare_without_other_attributes(lhs.clone(), rhs));

        let expected = vec![TaskTemplate {
            label: "gulp: build tests".to_string(),
            command: "gulp".to_string(),
            args: vec!["build".to_string()],
            ..Default::default()
        }];

        let tasks: TaskTemplates = vscode_definitions.try_into().unwrap();
        assert_eq!(tasks.0, expected);
    }

    #[test]
    fn can_deserialize_npm_tasks() {
        const NPM_TASKS: &str = include_str!("../test_data/tasks-npm.json");
        let vscode_definitions: VsCodeTaskFile = serde_json_lenient::from_str(NPM_TASKS).unwrap();

        let expected = vec![
            VsCodeTaskDefinition {
                label: "With env".to_string(),
                command: Some(Command::Npm {
                    script: "build".to_string(),
                    path: None,
                }),
                other_attributes: Default::default(),
                options: Some(super::TaskOptions {
                    cwd: None,
                    env: HashMap::from_iter([("NODE_ENV".to_string(), "production".to_string())]),
                }),
            },
            VsCodeTaskDefinition {
                label: "With path".to_string(),
                command: Some(Command::Npm {
                    script: "build".to_string(),
                    path: Some("packages/components".to_string()),
                }),
                other_attributes: Default::default(),
                options: None,
            },
            VsCodeTaskDefinition {
                label: "With cwd".to_string(),
                command: Some(Command::Npm {
                    script: "build".to_string(),
                    path: None,
                }),
                other_attributes: Default::default(),
                options: Some(super::TaskOptions {
                    cwd: Some("${workspaceFolder}/packages/app".to_string()),
                    env: Default::default(),
                }),
            },
            VsCodeTaskDefinition {
                label: "With path and cwd".to_string(),
                command: Some(Command::Npm {
                    script: "build".to_string(),
                    path: Some("packages/components".to_string()),
                }),
                other_attributes: Default::default(),
                options: Some(super::TaskOptions {
                    cwd: Some("${workspaceFolder}/packages/app".to_string()),
                    env: Default::default(),
                }),
            },
        ];

        assert_eq!(vscode_definitions.tasks.len(), expected.len());
        vscode_definitions
            .tasks
            .iter()
            .zip(expected)
            .for_each(|(lhs, rhs)| compare_without_other_attributes(lhs.clone(), rhs));

        let expected = vec![
            TaskTemplate {
                label: "With env".to_string(),
                command: "npm".to_string(),
                args: vec!["run".to_string(), "build".to_string()],
                env: HashMap::from_iter([("NODE_ENV".to_string(), "production".to_string())]),
                ..Default::default()
            },
            TaskTemplate {
                label: "With path".to_string(),
                command: "npm".to_string(),
                args: vec!["run".to_string(), "build".to_string()],
                cwd: Some("$ZED_WORKTREE_ROOT/packages/components".to_string()),
                ..Default::default()
            },
            TaskTemplate {
                label: "With cwd".to_string(),
                command: "npm".to_string(),
                args: vec!["run".to_string(), "build".to_string()],
                cwd: Some("${ZED_WORKTREE_ROOT}/packages/app".to_string()),
                ..Default::default()
            },
            TaskTemplate {
                label: "With path and cwd".to_string(),
                command: "npm".to_string(),
                args: vec!["run".to_string(), "build".to_string()],
                cwd: Some("${ZED_WORKTREE_ROOT}/packages/app".to_string()),
                ..Default::default()
            },
        ];
        let tasks: TaskTemplates = vscode_definitions.try_into().unwrap();
        assert_eq!(tasks.0, expected);
    }

    #[test]
    fn can_deserialize_shell_tasks() {
        const SHELL_TASKS: &str = include_str!("../test_data/tasks-shell.json");
        let vscode_definitions: VsCodeTaskFile = serde_json_lenient::from_str(SHELL_TASKS).unwrap();
        let expected = vec![
            VsCodeTaskDefinition {
                label: "Without args".to_string(),
                command: Some(Command::Shell {
                    command: "cargo build --package rust-analyzer".to_string(),
                    args: Default::default(),
                }),
                options: None,
                other_attributes: Default::default(),
            },
            VsCodeTaskDefinition {
                label: "With args".to_string(),
                command: Some(Command::Shell {
                    command: "node".to_string(),
                    args: vec![
                        "${workspaceFolder}/node_modules/typescript/lib/tsc.js".to_string(),
                        "--build".to_string(),
                        "${workspaceFolder}/src".to_string(),
                        "--watch".to_string(),
                    ],
                }),
                options: None,
                other_attributes: Default::default(),
            },
            VsCodeTaskDefinition {
                label: "Ignored task without type".to_string(),
                command: None,
                options: None,
                other_attributes: Default::default(),
            },
        ];
        assert_eq!(vscode_definitions.tasks.len(), expected.len());
        vscode_definitions
            .tasks
            .iter()
            .zip(expected)
            .for_each(|(lhs, rhs)| compare_without_other_attributes(lhs.clone(), rhs));
        let expected = vec![
            TaskTemplate {
                label: "Without args".to_string(),
                command: "cargo build --package rust-analyzer".to_string(),
                ..Default::default()
            },
            TaskTemplate {
                label: "With args".to_string(),
                command: "node".to_string(),
                args: vec![
                    "${ZED_WORKTREE_ROOT}/node_modules/typescript/lib/tsc.js".to_string(),
                    "--build".to_string(),
                    "${ZED_WORKTREE_ROOT}/src".to_string(),
                    "--watch".to_string(),
                ],
                ..Default::default()
            },
            // `dependsOn` tasks run their dependencies, in parallel by default.
            TaskTemplate {
                label: "Ignored task without type".to_string(),
                command: "{ (cargo build --package rust-analyzer) & pid0=$!; (node ${ZED_WORKTREE_ROOT}/node_modules/typescript/lib/tsc.js --build ${ZED_WORKTREE_ROOT}/src --watch) & pid1=$!; wait $pid0 && wait $pid1; }".to_string(),
                ..Default::default()
            },
        ];
        let tasks: TaskTemplates = vscode_definitions.try_into().unwrap();
        assert_eq!(tasks.0, expected);
    }

    #[test]
    fn can_deserialize_rust_analyzer_tasks() {
        const RUST_ANALYZER_TASKS: &str = include_str!("../test_data/rust-analyzer.json");
        let vscode_definitions: VsCodeTaskFile =
            serde_json_lenient::from_str(RUST_ANALYZER_TASKS).unwrap();
        let expected = vec![
            VsCodeTaskDefinition {
                label: "Build Extension in Background".to_string(),
                command: Some(Command::Npm {
                    script: "watch".to_string(),
                    path: Some("editors/code/".to_string()),
                }),
                options: None,
                other_attributes: Default::default(),
            },
            VsCodeTaskDefinition {
                label: "Build Extension".to_string(),
                command: Some(Command::Npm {
                    script: "build".to_string(),
                    path: Some("editors/code/".to_string()),
                }),
                options: None,
                other_attributes: Default::default(),
            },
            VsCodeTaskDefinition {
                label: "Build Server".to_string(),
                command: Some(Command::Shell {
                    command: "cargo build --package rust-analyzer".to_string(),
                    args: Default::default(),
                }),
                options: None,
                other_attributes: Default::default(),
            },
            VsCodeTaskDefinition {
                label: "Build Server (Release)".to_string(),
                command: Some(Command::Shell {
                    command: "cargo build --release --package rust-analyzer".to_string(),
                    args: Default::default(),
                }),
                options: None,
                other_attributes: Default::default(),
            },
            VsCodeTaskDefinition {
                label: "Pretest".to_string(),
                command: Some(Command::Npm {
                    script: "pretest".to_string(),
                    path: Some("editors/code/".to_string()),
                }),
                options: None,
                other_attributes: Default::default(),
            },
            VsCodeTaskDefinition {
                label: "Build Server and Extension".to_string(),
                command: None,
                options: None,
                other_attributes: Default::default(),
            },
            VsCodeTaskDefinition {
                label: "Build Server (Release) and Extension".to_string(),
                command: None,
                options: None,
                other_attributes: Default::default(),
            },
        ];
        assert_eq!(vscode_definitions.tasks.len(), expected.len());
        vscode_definitions
            .tasks
            .iter()
            .zip(expected)
            .for_each(|(lhs, rhs)| compare_without_other_attributes(lhs.clone(), rhs));
        let expected = vec![
            TaskTemplate {
                label: "Build Extension in Background".to_string(),
                command: "npm".to_string(),
                args: vec!["run".to_string(), "watch".to_string()],
                cwd: Some("$ZED_WORKTREE_ROOT/editors/code/".to_string()),
                ..Default::default()
            },
            TaskTemplate {
                label: "Build Extension".to_string(),
                command: "npm".to_string(),
                args: vec!["run".to_string(), "build".to_string()],
                cwd: Some("$ZED_WORKTREE_ROOT/editors/code/".to_string()),
                ..Default::default()
            },
            TaskTemplate {
                label: "Build Server".to_string(),
                command: "cargo build --package rust-analyzer".to_string(),
                ..Default::default()
            },
            TaskTemplate {
                label: "Build Server (Release)".to_string(),
                command: "cargo build --release --package rust-analyzer".to_string(),
                ..Default::default()
            },
            TaskTemplate {
                label: "Pretest".to_string(),
                command: "npm".to_string(),
                args: vec!["run".to_string(), "pretest".to_string()],
                cwd: Some("$ZED_WORKTREE_ROOT/editors/code/".to_string()),
                ..Default::default()
            },
            TaskTemplate {
                label: "Build Server and Extension".to_string(),
                command: "{ (cargo build --package rust-analyzer) & pid0=$!; (cd $ZED_WORKTREE_ROOT/editors/code/ && npm run build) & pid1=$!; wait $pid0 && wait $pid1; }".to_string(),
                ..Default::default()
            },
            TaskTemplate {
                label: "Build Server (Release) and Extension".to_string(),
                command: "{ (cargo build --release --package rust-analyzer) & pid0=$!; (cd $ZED_WORKTREE_ROOT/editors/code/ && npm run build) & pid1=$!; wait $pid0 && wait $pid1; }".to_string(),
                ..Default::default()
            },
        ];
        let tasks: TaskTemplates = vscode_definitions.try_into().unwrap();
        assert_eq!(tasks.0, expected);
    }

    #[test]
    fn can_deserialize_tasks_without_labels() {
        const TASKS_WITHOUT_LABELS: &str = include_str!("../test_data/tasks-without-labels.json");
        let vscode_definitions: VsCodeTaskFile =
            serde_json_lenient::from_str(TASKS_WITHOUT_LABELS).unwrap();

        assert_eq!(vscode_definitions.tasks.len(), 4);
        assert_eq!(vscode_definitions.tasks[0].label, "npm: start");
        assert_eq!(vscode_definitions.tasks[1].label, "Explicit Label");
        assert_eq!(vscode_definitions.tasks[2].label, "gulp: build");
        assert_eq!(vscode_definitions.tasks[3].label, "echo hello");
    }

    #[test]
    fn test_generate_label() {
        assert_eq!(
            generate_label(&Some(Command::Npm {
                script: "start".to_string(),
                path: None,
            })),
            "npm: start"
        );
        assert_eq!(
            generate_label(&Some(Command::Gulp {
                task: "build".to_string()
            })),
            "gulp: build"
        );
        assert_eq!(
            generate_label(&Some(Command::Shell {
                command: "echo hello".to_string(),
                args: vec![]
            })),
            "echo hello"
        );
        assert_eq!(
            generate_label(&Some(Command::Shell {
                command: "cargo build --release".to_string(),
                args: vec![]
            })),
            "cargo build --release"
        );
        assert_eq!(
            generate_label(&Some(Command::Shell {
                command: "  ".to_string(),
                args: vec![]
            })),
            "shell"
        );
        assert_eq!(
            generate_label(&Some(Command::Shell {
                command: "".to_string(),
                args: vec![]
            })),
            "shell"
        );
        assert_eq!(generate_label(&None), "Untitled Task");
    }

    #[test]
    fn converts_depends_on_process_linux_and_inputs() {
        let raw = r#"
            {
                "version": "2.0.0",
                "inputs": [{ "id": "profile", "type": "pickString", "options": ["dev", "release"], "default": "release" }],
                "tasks": [
                    { "label": "fmt", "type": "process", "command": "cargo", "args": ["fmt"] },
                    { "label": "lint", "type": "shell", "command": "cargo clippy", "options": { "cwd": "${workspaceFolder}/crates" } },
                    {
                        "label": "build",
                        "type": "shell",
                        "command": "cargo build --profile ${input:profile}",
                        "linux": { "options": { "env": { "CC": "clang" } } },
                        "dependsOn": ["fmt", "lint"],
                        "dependsOrder": "sequence"
                    },
                    { "label": "checks", "dependsOn": ["fmt", "lint"] },
                    { "label": "loop", "type": "shell", "command": "true", "dependsOn": "loop" }
                ]
            }
        "#;
        let parsed: VsCodeTaskFile = serde_json_lenient::from_str(raw).expect("parsing tasks.json");
        let templates = TaskTemplates::try_from(parsed)
            .expect("converting tasks.json")
            .0;
        let root = format!("${{{}}}", crate::VariableName::WorktreeRoot);
        let commands = templates
            .iter()
            .map(|template| {
                (
                    template.label.as_str(),
                    template.command.clone(),
                    template.args.clone(),
                )
            })
            .collect::<Vec<_>>();
        pretty_assertions::assert_eq!(
            commands,
            vec![
                ("fmt", "cargo".to_string(), vec!["fmt".to_string()]),
                ("lint", "cargo clippy".to_string(), vec![]),
                (
                    "build",
                    format!(
                        "(cargo fmt) && (cd {root}/crates && cargo clippy) && (CC=clang cargo build --profile release)"
                    ),
                    vec![],
                ),
                (
                    "checks",
                    format!(
                        "{{ (cargo fmt) & pid0=$!; (cd {root}/crates && cargo clippy) & pid1=$!; wait $pid0 && wait $pid1; }}"
                    ),
                    vec![],
                ),
            ]
        );
    }
}
