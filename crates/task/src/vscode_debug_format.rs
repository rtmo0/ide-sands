use serde::Deserialize;
use util::ResultExt as _;

use crate::{
    BuildTaskDefinition, DebugScenario, DebugTaskFile, EnvVariableReplacer, TcpArgumentsTemplate,
    vscode_format::{VsCodeInput, input_defaults},
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct VsCodeDebugTaskDefinition {
    r#type: String,
    name: String,
    #[serde(default)]
    port: Option<u16>,
    /// Label of a task from `tasks.json` to run before the debug session starts.
    #[serde(default)]
    pre_launch_task: Option<String>,
    #[serde(default)]
    post_debug_task: Option<String>,
    #[serde(flatten)]
    other_attributes: serde_json::Value,
}

impl VsCodeDebugTaskDefinition {
    fn try_to_zed(mut self, replacer: &EnvVariableReplacer) -> anyhow::Result<DebugScenario> {
        let label = replacer.replace(&self.name);
        if let Some(post_debug_task) = &self.post_debug_task {
            log::warn!(
                "Ignoring `postDebugTask` `{post_debug_task}` of debug configuration `{label}`: not supported"
            );
        }
        let mut config = replacer.replace_value(self.other_attributes);
        if serde_json::to_string(&config).is_ok_and(|config| config.contains("${input:")) {
            anyhow::bail!(
                "debug configuration `{label}` uses an input without a default value, which Sands cannot prompt for"
            );
        }
        let adapter = task_type_to_adapter_name(&self.r#type, &config);
        if let Some(config) = config.as_object_mut()
            && adapter == "JavaScript"
        {
            config.insert("type".to_owned(), self.r#type.clone().into());
            if let Some(port) = self.port.take() {
                config.insert("port".to_owned(), port.into());
            }
        }
        let definition = DebugScenario {
            label: label.into(),
            build: self
                .pre_launch_task
                .map(|task_label| BuildTaskDefinition::ByName(task_label.into())),
            adapter: adapter.into(),
            tcp_connection: self.port.map(|port| TcpArgumentsTemplate {
                port: Some(port),
                host: None,
                timeout: None,
            }),
            config,
        };
        Ok(definition)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VsCodeDebugTaskFile {
    #[serde(default)]
    version: Option<String>,
    configurations: Vec<VsCodeDebugTaskDefinition>,
    #[serde(default)]
    inputs: Vec<VsCodeInput>,
}

impl TryFrom<VsCodeDebugTaskFile> for DebugTaskFile {
    type Error = anyhow::Error;

    fn try_from(file: VsCodeDebugTaskFile) -> Result<Self, Self::Error> {
        let replacer = EnvVariableReplacer::vscode().with_inputs(input_defaults(&file.inputs));
        let templates = file
            .configurations
            .into_iter()
            .filter_map(|config| config.try_to_zed(&replacer).log_err())
            .collect::<Vec<_>>();
        Ok(DebugTaskFile(templates))
    }
}

/// Maps a VS Code debugger `type` to the Sands adapter that speaks the same configuration.
fn task_type_to_adapter_name(task_type: &str, config: &serde_json::Value) -> String {
    match task_type {
        "pwa-node" | "node" | "node-terminal" | "chrome" | "pwa-chrome" | "edge" | "pwa-edge"
        | "msedge" | "pwa-msedge" => "JavaScript",
        "go" => "Delve",
        "php" => "Xdebug",
        // The C/C++ extension's `cppdbg` drives either GDB or LLDB through `MIMode`.
        "cppdbg" if config.get("MIMode").and_then(|mode| mode.as_str()) == Some("lldb") => {
            "CodeLLDB"
        }
        "cppdbg" | "gdb" => "GDB",
        "lldb" | "lldb-dap" | "codelldb" => "CodeLLDB",
        "debugpy" | "python" => "Debugpy",
        "rdbg" => "rdbg",
        _ => task_type,
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{BuildTaskDefinition, DebugScenario, DebugTaskFile, VariableName};

    use super::VsCodeDebugTaskFile;

    #[test]
    fn test_parsing_vscode_launch_json() {
        let raw = r#"
            {
                "version": "0.2.0",
                "configurations": [
                    {
                        "name": "Debug my JS app",
                        "request": "launch",
                        "type": "node",
                        "program": "${workspaceFolder}/xyz.js",
                        "showDevDebugOutput": false,
                        "stopOnEntry": true,
                        "args": ["--foo", "${workspaceFolder}/thing"],
                        "cwd": "${workspaceFolder}/${env:FOO}/sub",
                        "env": {
                            "X": "Y"
                        },
                        "port": 17
                    },
                ]
            }
        "#;
        let parsed: VsCodeDebugTaskFile =
            serde_json_lenient::from_str(raw).expect("deserializing launch.json");
        let zed = DebugTaskFile::try_from(parsed).expect("converting to Zed debug templates");
        pretty_assertions::assert_eq!(
            zed,
            DebugTaskFile(vec![DebugScenario {
                label: "Debug my JS app".into(),
                adapter: "JavaScript".into(),
                config: json!({
                    "request": "launch",
                    "program": "${ZED_WORKTREE_ROOT}/xyz.js",
                    "showDevDebugOutput": false,
                    "stopOnEntry": true,
                    "args": [
                        "--foo",
                        "${ZED_WORKTREE_ROOT}/thing",
                    ],
                    "cwd": "${ZED_WORKTREE_ROOT}/${FOO}/sub",
                    "env": {
                        "X": "Y",
                    },
                    "type": "node",
                    "port": 17,
                }),
                tcp_connection: None,
                build: None
            }])
        );
    }

    #[test]
    fn test_command_pickmyprocess_replacement() {
        let raw = r#"
            {
                "version": "0.2.0",
                "configurations": [
                    {
                        "name": "Attach to Process",
                        "request": "attach",
                        "type": "cppdbg",
                        "processId": "${command:pickMyProcess}"
                    }
                ]
            }
        "#;
        let parsed: VsCodeDebugTaskFile =
            serde_json_lenient::from_str(raw).expect("deserializing launch.json");
        let zed = DebugTaskFile::try_from(parsed).expect("converting to Zed debug templates");

        let expected_placeholder = format!("${{{}}}", VariableName::PickProcessId);
        pretty_assertions::assert_eq!(
            zed,
            DebugTaskFile(vec![DebugScenario {
                label: "Attach to Process".into(),
                // `cppdbg` defaults to GDB on Linux, which is what sandboxes run.
                adapter: "GDB".into(),
                config: json!({
                    "request": "attach",
                    "processId": expected_placeholder,
                }),
                tcp_connection: None,
                build: None
            }])
        );
    }

    #[test]
    fn test_pre_launch_task_types_and_inputs() {
        let raw = r#"
            {
                "version": "0.2.0",
                "inputs": [
                    { "id": "port", "type": "promptString", "default": "8080" },
                    { "id": "target", "type": "promptString" }
                ],
                "configurations": [
                    {
                        "name": "Sands",
                        "type": "lldb-dap",
                        "request": "launch",
                        "preLaunchTask": "Build Sands",
                        "program": "${workspaceFolder}/target/debug/sands",
                        "args": ["--port", "${input:port}", "${fileBasenameNoExtension}"]
                    },
                    {
                        "name": "macOS LLDB",
                        "type": "cppdbg",
                        "request": "launch",
                        "MIMode": "lldb",
                        "program": "a.out"
                    },
                    {
                        "name": "Needs a prompt",
                        "type": "gdb",
                        "request": "launch",
                        "program": "${input:target}"
                    }
                ]
            }
        "#;
        let parsed: VsCodeDebugTaskFile =
            serde_json_lenient::from_str(raw).expect("deserializing launch.json");
        let zed = DebugTaskFile::try_from(parsed).expect("converting to Zed debug templates");

        pretty_assertions::assert_eq!(
            zed,
            DebugTaskFile(vec![
                DebugScenario {
                    label: "Sands".into(),
                    adapter: "CodeLLDB".into(),
                    config: json!({
                        "request": "launch",
                        "program": format!("${{{}}}/target/debug/sands", VariableName::WorktreeRoot),
                        "args": ["--port", "8080", format!("${{{}}}", VariableName::Stem)],
                    }),
                    tcp_connection: None,
                    build: Some(BuildTaskDefinition::ByName("Build Sands".into())),
                },
                DebugScenario {
                    label: "macOS LLDB".into(),
                    adapter: "CodeLLDB".into(),
                    config: json!({
                        "request": "launch",
                        "MIMode": "lldb",
                        "program": "a.out",
                    }),
                    tcp_connection: None,
                    build: None,
                },
            ])
        );
    }
}
