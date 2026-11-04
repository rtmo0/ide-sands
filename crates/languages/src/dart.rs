use anyhow::{Result, bail};
use async_trait::async_trait;
use gpui::AsyncApp;
use language::{LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{LanguageServerBinary, LanguageServerName, Uri};
use project::lsp_store::language_server_settings;
use serde_json::json;
use std::{future::Future, path::PathBuf, sync::Arc};

/// The Dart analysis server ships with the Dart and Flutter SDKs, so it is taken from the
/// sandbox `PATH` instead of being downloaded.
pub struct DartLspAdapter;

impl DartLspAdapter {
    const SERVER_NAME: LanguageServerName = LanguageServerName::new_static("dart");
}

impl LspInstaller for DartLspAdapter {
    type BinaryVersion = ();

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let path = delegate.which("dart".as_ref()).await?;
        Some(LanguageServerBinary {
            path,
            arguments: vec!["language-server".into(), "--protocol=lsp".into()],
            env: None,
        })
    }

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<()> {
        Ok(())
    }

    fn fetch_server_binary(
        &self,
        _: (),
        _: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        async {
            bail!(
                "The Dart SDK was not found. Install Flutter / Dart from the Sandbox panel, \
                 or add `dart` to the PATH."
            )
        }
    }

    async fn cached_server_binary(
        &self,
        _: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        None
    }
}

#[async_trait(?Send)]
impl LspAdapter for DartLspAdapter {
    fn name(&self) -> LanguageServerName {
        Self::SERVER_NAME
    }

    async fn workspace_configuration(
        self: Arc<Self>,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        _: Option<Uri>,
        cx: &mut AsyncApp,
    ) -> Result<serde_json::Value> {
        let settings = cx.update(|cx| {
            language_server_settings(delegate.as_ref(), &self.name(), cx)
                .and_then(|settings| settings.settings.clone())
        });
        Ok(json!({ "dart": settings.unwrap_or_else(|| json!({})) }))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, TestAppContext};

    #[gpui::test]
    async fn test_dart_queries_load_and_highlight(cx: &mut TestAppContext) {
        let language = crate::language("dart", tree_sitter_dart::LANGUAGE.into());
        language.set_theme(&theme::SyntaxTheme::new([
            ("keyword".to_string(), gpui::rgba(0xff0000ff).into()),
            ("type".to_string(), gpui::rgba(0x00ff00ff).into()),
            ("function".to_string(), gpui::rgba(0x0000ffff).into()),
        ]));
        let text = "class Greeter {\n  void greet() {\n    print('hi');\n  }\n}\n";
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
                .map(|chunk| chunk.text.trim().to_string())
                .collect::<Vec<_>>()
        });
        assert!(
            highlighted.iter().any(|text| text == "class"),
            "{highlighted:?}"
        );
        assert!(
            highlighted.iter().any(|text| text == "Greeter"),
            "{highlighted:?}"
        );
    }
}
