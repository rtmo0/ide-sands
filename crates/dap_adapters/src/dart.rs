use std::{ffi::OsStr, path::Path};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use collections::HashMap;
use dap::{StartDebuggingRequestArguments, adapters::DebugTaskDefinition};
use gpui::AsyncApp;
use task::{DebugScenario, ZedDebugConfig};

use crate::*;

/// Debugs Dart and Flutter programs through the debug adapter built into the Dart and Flutter
/// SDKs (`dart debug_adapter`, `flutter debug_adapter`).
#[derive(Default)]
pub(crate) struct DartDebugAdapter;

impl DartDebugAdapter {
    const ADAPTER_NAME: &'static str = "Dart";
    /// Sandboxes have no display or emulator, so Flutter apps run as a web server that can be
    /// published to the host from the Sandbox panel.
    const DEFAULT_FLUTTER_WEB_PORT: u16 = 8686;
}

/// Whether `pubspec.yaml` in `directory` depends on the Flutter SDK. VS Code configurations use
/// `"type": "dart"` for Flutter apps too and leave this decision to the debugger.
async fn is_flutter_project(delegate: &Arc<dyn DapDelegate>, directory: &Path) -> bool {
    delegate
        .fs()
        .load(&directory.join("pubspec.yaml"))
        .await
        .is_ok_and(|pubspec| pubspec.lines().any(|line| line.trim() == "sdk: flutter"))
}

fn string_list(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Adds the device to `toolArgs`, the only place Flutter's debug adapter reads it from, unless
/// one is already there. Without a device, Flutter runs as a web server.
fn flutter_tool_args(
    mut tool_args: Vec<String>,
    device_id: Option<&str>,
    web_port: u16,
) -> Vec<String> {
    let has_device = tool_args
        .iter()
        .any(|arg| arg == "-d" || arg.starts_with("--device-id"));
    if has_device {
        return tool_args;
    }
    match device_id {
        Some(device_id) => tool_args.extend(["-d".to_string(), device_id.to_string()]),
        None => tool_args.extend([
            "-d".to_string(),
            "web-server".to_string(),
            "--web-hostname".to_string(),
            "0.0.0.0".to_string(),
            "--web-port".to_string(),
            web_port.to_string(),
        ]),
    }
    tool_args
}

#[async_trait(?Send)]
impl DebugAdapter for DartDebugAdapter {
    fn name(&self) -> DebugAdapterName {
        DebugAdapterName(Self::ADAPTER_NAME.into())
    }

    async fn config_from_zed_format(&self, zed_scenario: ZedDebugConfig) -> Result<DebugScenario> {
        let mut config = serde_json::Map::default();
        match &zed_scenario.request {
            dap::DebugRequest::Attach(_) => {
                config.insert("request".into(), "attach".into());
            }
            dap::DebugRequest::Launch(launch) => {
                config.insert("request".into(), "launch".into());
                config.insert("program".into(), launch.program.clone().into());
                if !launch.args.is_empty() {
                    config.insert("args".into(), launch.args.clone().into());
                }
                if !launch.env.is_empty() {
                    config.insert("env".into(), launch.env_json());
                }
                if let Some(cwd) = launch.cwd.as_ref() {
                    config.insert("cwd".into(), cwd.to_string_lossy().into_owned().into());
                }
            }
        }
        if let Some(stop_on_entry) = zed_scenario.stop_on_entry {
            config.insert("stopOnEntry".into(), stop_on_entry.into());
        }
        Ok(DebugScenario {
            adapter: zed_scenario.adapter,
            label: zed_scenario.label,
            build: None,
            config: serde_json::Value::Object(config),
            tcp_connection: None,
        })
    }

    fn dap_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["request"],
            "properties": {
                "request": {
                    "type": "string",
                    "enum": ["launch", "attach"],
                    "description": "Launch a program or attach to a running Dart VM"
                },
                "type": {
                    "type": "string",
                    "enum": ["dart", "flutter"],
                    "description": "Run with the Dart or the Flutter tool. Defaults to Flutter when pubspec.yaml depends on the Flutter SDK."
                },
                "program": {
                    "type": "string",
                    "description": "Entry point, relative to cwd",
                    "default": "lib/main.dart"
                },
                "cwd": {
                    "type": "string",
                    "description": "Directory of the Dart or Flutter package"
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Arguments passed to the program"
                },
                "toolArgs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Arguments passed to `dart run` or `flutter run`"
                },
                "deviceId": {
                    "type": "string",
                    "description": "Flutter device to run on. Defaults to a web server, since sandboxes have no display."
                },
                "webPort": {
                    "type": "integer",
                    "description": "Port of the Flutter web server when no device is set",
                    "default": Self::DEFAULT_FLUTTER_WEB_PORT
                },
                "flutterMode": {
                    "type": "string",
                    "enum": ["debug", "profile", "release"],
                    "default": "debug"
                },
                "vmServiceUri": {
                    "type": "string",
                    "description": "VM service to attach to"
                },
                "env": {
                    "type": "object",
                    "description": "Environment variables for the program"
                },
                "stopOnEntry": {
                    "type": "boolean",
                    "default": false
                }
            }
        })
    }

    async fn get_binary(
        &self,
        delegate: &Arc<dyn DapDelegate>,
        config: &DebugTaskDefinition,
        user_installed_path: Option<std::path::PathBuf>,
        user_args: Option<Vec<String>>,
        user_env: Option<HashMap<String, String>>,
        _: &mut AsyncApp,
    ) -> Result<DebugAdapterBinary> {
        let settings = &config.config;
        let root = delegate.worktree_root_path().to_path_buf();
        let cwd = settings
            .get("cwd")
            .and_then(|cwd| cwd.as_str())
            .map(|cwd| root.join(cwd))
            .unwrap_or_else(|| root.clone());

        let flutter = match settings.get("type").and_then(|kind| kind.as_str()) {
            Some("flutter") => true,
            Some("dart") | None => is_flutter_project(delegate, &cwd).await,
            Some(_) => false,
        };
        let tool = if flutter { "flutter" } else { "dart" };
        let tool_path = match user_installed_path.filter(|path| path.exists()) {
            Some(path) => path,
            None => delegate.which(OsStr::new(tool)).await.with_context(|| {
                format!("`{tool}` was not found. Install Flutter / Dart from the Sandbox panel.")
            })?,
        };

        let mut configuration = settings.clone();
        if let Some(configuration) = configuration.as_object_mut() {
            configuration.insert(
                "type".into(),
                if flutter { "flutter" } else { "dart" }.into(),
            );
            configuration.insert("cwd".into(), cwd.to_string_lossy().into_owned().into());
            configuration
                .entry("program")
                .or_insert_with(|| "lib/main.dart".into());
            configuration.insert("sendLogsToClient".into(), true.into());
            if flutter {
                configuration
                    .entry("flutterMode")
                    .or_insert_with(|| "debug".into());
                let web_port = settings
                    .get("webPort")
                    .and_then(|port| port.as_u64())
                    .and_then(|port| u16::try_from(port).ok())
                    .unwrap_or(Self::DEFAULT_FLUTTER_WEB_PORT);
                let device_id = settings.get("deviceId").and_then(|device| device.as_str());
                let tool_args =
                    flutter_tool_args(string_list(settings.get("toolArgs")), device_id, web_port);
                configuration.insert("toolArgs".into(), tool_args.into());
            }
        }

        let mut envs = delegate.shell_env().await;
        envs.extend(user_env.unwrap_or_default());
        if let Some(config_env) = settings.get("env").and_then(|env| env.as_object()) {
            envs.extend(config_env.iter().filter_map(|(key, value)| {
                value.as_str().map(|value| (key.clone(), value.to_string()))
            }));
        }

        Ok(DebugAdapterBinary {
            command: Some(tool_path.to_string_lossy().into_owned()),
            arguments: user_args.unwrap_or_else(|| vec!["debug_adapter".into()]),
            envs,
            cwd: Some(cwd),
            connection: None,
            request_args: StartDebuggingRequestArguments {
                request: self.request_kind(&config.config).await?,
                configuration,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::flutter_tool_args;

    #[test]
    fn flutter_runs_as_a_web_server_without_a_device() {
        assert_eq!(
            flutter_tool_args(vec!["--dart-define=ENV=dev".into()], None, 8686),
            [
                "--dart-define=ENV=dev",
                "-d",
                "web-server",
                "--web-hostname",
                "0.0.0.0",
                "--web-port",
                "8686"
            ]
        );
        assert_eq!(
            flutter_tool_args(vec![], Some("chrome"), 8686),
            ["-d", "chrome"]
        );
        assert_eq!(
            flutter_tool_args(vec!["-d".into(), "linux".into()], Some("chrome"), 8686),
            ["-d", "linux"]
        );
    }
}
