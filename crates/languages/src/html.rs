use anyhow::Result;
use async_trait::async_trait;
use gpui::AsyncApp;
use language::{LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{LanguageServerBinary, LanguageServerName, Uri};
use node_runtime::{NodeRuntime, VersionStrategy};
use project::lsp_store::language_server_settings;
use semver::Version;
use serde_json::json;
use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::{ResultExt, maybe, merge_json_value_into};

const SERVER_PATH: &str =
    "node_modules/vscode-langservers-extracted/bin/vscode-html-language-server";

fn server_binary_arguments(server_path: &Path) -> Vec<OsString> {
    vec![server_path.into(), "--stdio".into()]
}

pub struct HtmlLspAdapter {
    node: NodeRuntime,
}

impl HtmlLspAdapter {
    const PACKAGE_NAME: &str = "vscode-langservers-extracted";
    pub fn new(node: NodeRuntime) -> Self {
        HtmlLspAdapter { node }
    }
}

impl LspInstaller for HtmlLspAdapter {
    type BinaryVersion = Version;

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<Self::BinaryVersion> {
        self.node
            .npm_package_latest_version("vscode-langservers-extracted")
            .await
    }

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let path = delegate
            .which("vscode-html-language-server".as_ref())
            .await?;
        let env = delegate.shell_env().await;

        Some(LanguageServerBinary {
            path,
            env: Some(env),
            arguments: vec!["--stdio".into()],
        })
    }

    fn fetch_server_binary(
        &self,
        _latest_version: Self::BinaryVersion,
        container_dir: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        let node = self.node.clone();

        async move {
            let server_path = container_dir.join(SERVER_PATH);

            node.npm_install_latest_packages(&container_dir, &[Self::PACKAGE_NAME])
                .await?;

            Ok(LanguageServerBinary {
                path: node.binary_path().await?,
                env: None,
                arguments: server_binary_arguments(&server_path),
            })
        }
    }

    fn check_if_version_installed(
        &self,
        version: &Self::BinaryVersion,
        container_dir: &PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Option<LanguageServerBinary>> + use<> {
        let node = self.node.clone();
        let version = version.clone();
        let container_dir = container_dir.clone();

        async move {
            let server_path = container_dir.join(SERVER_PATH);

            let should_install_language_server = node
                .should_install_npm_package(
                    Self::PACKAGE_NAME,
                    &server_path,
                    &container_dir,
                    VersionStrategy::Latest(&version),
                )
                .await;

            if should_install_language_server {
                None
            } else {
                Some(LanguageServerBinary {
                    path: node.binary_path().await.ok()?,
                    env: None,
                    arguments: server_binary_arguments(&server_path),
                })
            }
        }
    }

    async fn cached_server_binary(
        &self,
        container_dir: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        get_cached_server_binary(container_dir, &self.node).await
    }
}

#[async_trait(?Send)]
impl LspAdapter for HtmlLspAdapter {
    fn name(&self) -> LanguageServerName {
        LanguageServerName("vscode-html-language-server".into())
    }

    async fn initialization_options(
        self: Arc<Self>,
        _: &Arc<dyn LspAdapterDelegate>,
        _: &mut AsyncApp,
    ) -> Result<Option<serde_json::Value>> {
        Ok(Some(json!({
            "provideFormatter": true
        })))
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<serde_json::Value> {
        let mut config = json!({});
        let project_options = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &self.name(), cx)
                .and_then(|settings| settings.settings.clone())
        });
        if let Some(override_options) = project_options {
            merge_json_value_into(override_options, &mut config);
        }
        Ok(config)
    }
}

async fn get_cached_server_binary(
    container_dir: PathBuf,
    node: &NodeRuntime,
) -> Option<LanguageServerBinary> {
    maybe!(async {
        let server_path = container_dir.join(SERVER_PATH);
        anyhow::ensure!(
            server_path.exists(),
            "missing executable in directory {server_path:?}"
        );
        Ok(LanguageServerBinary {
            path: node.binary_path().await?,
            env: None,
            arguments: server_binary_arguments(&server_path),
        })
    })
    .await
    .log_err()
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, TestAppContext};

    #[gpui::test]
    async fn test_html_queries_load_and_highlight(cx: &mut TestAppContext) {
        let language = crate::language("html", tree_sitter_html::LANGUAGE.into());
        language.set_theme(&theme::SyntaxTheme::new([
            ("tag".to_string(), gpui::rgba(0xff0000ff).into()),
            ("attribute".to_string(), gpui::rgba(0x00ff00ff).into()),
        ]));
        let text =
            "<div class=\"a\"><style>p { color: red; }</style><script>let x = 1;</script></div>";
        let buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local(text, cx);
            buffer.set_sync_parse_timeout(None);
            buffer.set_language(Some(language.clone()), cx);
            buffer
        });
        cx.run_until_parked();
        let highlighted = buffer.update(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            snapshot
                .chunks(
                    0..snapshot.len(),
                    language::LanguageAwareStyling {
                        tree_sitter: true,
                        diagnostics: false,
                    },
                )
                .filter(|chunk| chunk.syntax_highlight_id.is_some())
                .map(|chunk| chunk.text.to_string())
                .collect::<Vec<_>>()
        });
        assert!(
            highlighted.iter().any(|text| text == "div"),
            "{highlighted:?}"
        );
        assert!(
            highlighted.iter().any(|text| text == "class"),
            "{highlighted:?}"
        );
    }
}
