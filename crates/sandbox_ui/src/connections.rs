use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::Context as _;
use gpui::{App, AppContext as _, Global, TaskExt as _};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

/// What a project is connected to besides its sandbox. It is kept on the host so the Projects
/// panel can label every project without starting its sandbox.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProjectConnections {
    /// Kubeconfig contexts connected to the sandbox.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kubernetes: Vec<String>,
}

#[derive(Default)]
pub(crate) struct ConnectionsStore(BTreeMap<String, ProjectConnections>);

impl Global for ConnectionsStore {}

fn store_path() -> PathBuf {
    paths::data_dir().join("project_connections.json")
}

pub(crate) fn init(cx: &mut App) {
    let projects = match fs::read(store_path()) {
        Ok(contents) => serde_json::from_slice(&contents)
            .context("project_connections.json is not valid JSON")
            .log_err()
            .unwrap_or_default(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::default(),
        Err(error) => {
            log::error!("failed to read project connections: {error}");
            BTreeMap::default()
        }
    };
    cx.set_global(ConnectionsStore(projects));
}

/// Re-renders the observing view whenever a project's connections change.
pub(crate) fn observe<T: 'static>(cx: &mut gpui::Context<T>) -> gpui::Subscription {
    cx.observe_global::<ConnectionsStore>(|_, cx| cx.notify())
}

pub(crate) fn connections(sandbox: &str, cx: &App) -> ProjectConnections {
    cx.try_global::<ConnectionsStore>()
        .and_then(|store| store.0.get(sandbox).cloned())
        .unwrap_or_default()
}

pub(crate) fn set_kubernetes(sandbox: &str, contexts: Vec<String>, cx: &mut App) {
    let store = cx.default_global::<ConnectionsStore>();
    let entry = store.0.entry(sandbox.to_string()).or_default();
    if entry.kubernetes == contexts {
        return;
    }
    entry.kubernetes = contexts;
    if store
        .0
        .get(sandbox)
        .is_some_and(|entry| entry == &ProjectConnections::default())
    {
        store.0.remove(sandbox);
    }
    let contents = serde_json::to_vec_pretty(&store.0);
    cx.background_spawn(async move {
        let path = store_path();
        if let Some(directory) = path.parent() {
            fs::create_dir_all(directory)?;
        }
        fs::write(&path, contents?).with_context(|| format!("failed to write {}", path.display()))
    })
    .detach_and_log_err(cx);
}
