use std::{collections::HashMap, fs, io::Write as _, sync::Arc};

use anyhow::{Context as _, Result};
use credentials_provider::CredentialsProvider;
use gpui::{App, AppContext as _, AsyncApp, BorrowAppContext as _, Global, TaskExt as _};
use model_gateway::{Call, Gateway, Provider, Route};
use sands_project::{ProjectFile, Snapshot};
use sbx::{NetworkDecision, Sbx};
use util::ResultExt as _;
use workspace::Workspace;

use crate::connected_sandbox;

/// A fixed port, so the sandbox's network rule stays valid from one Sands run to the next.
const PORT: u16 = 47390;

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com";
const OPEN_AI_API_URL: &str = "https://api.openai.com/v1";

struct GatewayState {
    gateway: Gateway,
    /// The token of each open project, for this run of Sands.
    tokens: HashMap<String, String>,
}

impl Global for GatewayState {}

fn calls_path(project: &str) -> std::path::PathBuf {
    paths::data_dir()
        .join("projects")
        .join(project)
        .join("model_calls.jsonl")
}

fn append_call(call: Call) {
    let path = calls_path(&call.project);
    let result = (|| -> Result<()> {
        if let Some(directory) = path.parent() {
            fs::create_dir_all(directory)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&call)?)?;
        Ok(())
    })();
    if let Err(error) = result {
        log::error!("writing {} failed: {error:#}", path.display());
    }
}

pub(crate) fn init(cx: &mut App) {
    let log: model_gateway::CallLog = Arc::new(append_call);
    match Gateway::start(PORT, cx.http_client(), log) {
        Ok(gateway) => cx.set_global(GatewayState {
            gateway,
            tokens: HashMap::new(),
        }),
        Err(error) => {
            log::error!("the model gateway is off: {error:#}");
            return;
        }
    }
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        if let Some(sandbox) = connected_sandbox(workspace, cx) {
            connect(sandbox, cx);
        }
    })
    .detach();
}

/// The sandbox proxy sees requests to `host.docker.internal` as going to `localhost`, so that is
/// the host its policy has to allow.
fn gateway_rule(port: u16) -> String {
    format!("localhost:{port}")
}

/// The models a project's approved `.sands/project.yaml` allows, or `None` for any model.
fn allowed_models(snapshot: &Snapshot) -> Option<Vec<String>> {
    let project = snapshot.repositories.values().find_map(|files| {
        let text = files.get(".sands/project.yaml")?.text.as_deref()?;
        ProjectFile::parse(text).log_err()
    })?;
    let models = [
        project.models.plan,
        project.models.work,
        project.models.fast,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    (!models.is_empty()).then_some(models)
}

async fn host_key(
    url: &str,
    env_var: &str,
    credentials: &Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> Option<String> {
    if let Ok(key) = std::env::var(env_var)
        && !key.is_empty()
    {
        return Some(key);
    }
    let (_, password) = language_model::read_credentials(credentials.as_ref(), url, cx)
        .await
        .log_err()
        .flatten()?;
    String::from_utf8(password).log_err()
}

/// Registers the project with the gateway and points its sandbox at it.
fn connect(sandbox: String, cx: &mut App) {
    let credentials = sands_credentials_provider::global(cx);
    cx.spawn(async move |cx| {
        let snapshot = {
            let sandbox = sandbox.clone();
            cx.background_spawn(async move { Snapshot::load(&sandbox) })
                .await?
        };
        let mut keys = HashMap::new();
        if let Some(key) = host_key(ANTHROPIC_API_URL, "ANTHROPIC_API_KEY", &credentials, cx).await
        {
            keys.insert(Provider::Anthropic, key);
        }
        if let Some(key) = host_key(OPEN_AI_API_URL, "OPENAI_API_KEY", &credentials, cx).await {
            keys.insert(Provider::OpenAi, key);
        }
        let route = Route {
            project: sandbox.clone(),
            allowed_models: allowed_models(&snapshot),
            keys,
        };
        let base_url = cx.update(|cx| {
            cx.has_global::<GatewayState>().then(|| {
                cx.update_global::<GatewayState, _>(|state, _| {
                    let token = match state.tokens.get(&sandbox) {
                        Some(token) => {
                            state.gateway.update(token, route);
                            token.clone()
                        }
                        None => state.gateway.register(route),
                    };
                    let base_url = state.gateway.base_url(&token, Provider::Anthropic);
                    state.tokens.insert(sandbox.clone(), token);
                    (base_url, state.gateway.port())
                })
            })
        });
        let Some((base_url, port)) = base_url else {
            return anyhow::Ok(());
        };
        cx.background_spawn(async move {
            let sbx = Sbx::locate()?;
            sbx.add_network_rule(&sandbox, NetworkDecision::Allow, &gateway_rule(port))
                .await
                .context("allowing the sandbox to reach the model gateway")?;
            sbx.point_claude_at(&sandbox, Some(&base_url)).await?;
            anyhow::Ok(())
        })
        .await
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sands_project::FileState;

    /// Reaches a gateway on the host from a real sandbox: `cargo test -p sandbox_ui -- --ignored`.
    #[test]
    #[ignore]
    fn a_sandbox_reaches_the_gateway_through_its_proxy()
    -> crate::new_project_modal::tests::TestResult {
        use crate::new_project_modal::{ProjectSource, tests::plan};

        let upstream = tiny_http::Server::http("127.0.0.1:0").map_err(|error| error.to_string())?;
        let upstream_url = format!(
            "http://{}",
            upstream.server_addr().to_ip().ok_or("no address")?
        );
        std::thread::spawn(move || {
            for request in upstream.incoming_requests() {
                request
                    .respond(tiny_http::Response::from_string("{\"ok\":true}"))
                    .ok();
            }
        });
        let http: Arc<dyn http_client::HttpClient> = Arc::new(reqwest_client::ReqwestClient::new());
        let gateway = Gateway::start_with_upstreams(
            0,
            http,
            Arc::new(|_| {}),
            HashMap::from_iter([(Provider::Anthropic, upstream_url)]),
        )?;
        let token = gateway.register(Route {
            project: "sands-test-gateway".to_string(),
            allowed_models: Some(vec!["anthropic/claude-work".to_string()]),
            keys: HashMap::from_iter([(Provider::Anthropic, "host-secret".to_string())]),
        });
        let base_url = gateway.base_url(&token, Provider::Anthropic);
        let name = "sands-test-gateway";
        smol::block_on(async {
            let sbx = Sbx::locate()?;
            let result = async {
                crate::provisioning::provision(&plan(
                    name,
                    vec![ProjectSource::GitUrl(
                        "https://github.com/octocat/Hello-World.git".to_string(),
                    )],
                ))
                .await?;
                sbx.add_network_rule(name, NetworkDecision::Allow, &gateway_rule(gateway.port()))
                    .await?;
                let call = |model: &'static str| {
                    let url = format!("{base_url}/v1/messages");
                    let body = format!("{{\"model\":\"{model}\"}}");
                    let sbx = sbx.clone();
                    async move {
                        let output = sbx
                            .exec(
                                name,
                                &[
                                    "curl",
                                    "-s",
                                    "-o",
                                    "/dev/stderr",
                                    "-w",
                                    "%{http_code}",
                                    "-H",
                                    "content-type: application/json",
                                    "-d",
                                    &body,
                                    &url,
                                ],
                            )
                            .await?;
                        anyhow::Ok(String::from_utf8_lossy(&output.stdout).into_owned())
                    }
                };
                let allowed = call("claude-work").await?;
                let refused = call("claude-plan").await?;
                sbx.point_claude_at(name, Some(&base_url)).await?;
                let settings = sbx
                    .exec(name, &["sh", "-c", "cat \"$HOME/.claude/settings.json\""])
                    .await?;
                anyhow::Ok((
                    allowed,
                    refused,
                    String::from_utf8_lossy(&settings.stdout).into_owned(),
                ))
            }
            .await;
            if let Err(error) = sbx.remove(name).await {
                eprintln!("cleaning up {name} failed: {error}");
            }
            let snapshot = paths::data_dir().join("projects").join(name);
            if snapshot.exists() {
                fs::remove_dir_all(snapshot)?;
            }
            let (allowed, refused, settings) = result?;
            assert_eq!(allowed, "200");
            assert_eq!(refused, "403");
            assert!(settings.contains(&base_url), "{settings}");
            Ok(())
        })
    }

    #[test]
    fn allowed_models_come_from_the_approved_project_file() {
        let mut snapshot = Snapshot::default();
        assert_eq!(allowed_models(&snapshot), None);
        snapshot.repositories.insert(
            "/workspace/app".to_string(),
            [(
                ".sands/project.yaml".to_string(),
                FileState::from_text(
                    "models:\n  plan: anthropic/claude-opus-4-1\n  fast: anthropic/claude-haiku-4-5\n",
                ),
            )]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            allowed_models(&snapshot),
            Some(vec![
                "anthropic/claude-opus-4-1".to_string(),
                "anthropic/claude-haiku-4-5".to_string()
            ])
        );
    }
}
