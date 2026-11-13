use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    hash::{DefaultHasher, Hash as _, Hasher as _},
    path::PathBuf,
    sync::Mutex,
};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use collections::HashSet;
use gpui::{
    App, AppContext as _, DismissEvent, EventEmitter, FocusHandle, Focusable, PromptLevel,
    SharedString, Task, WeakEntity, actions,
};
use sbx::Sbx;
use serde_json::{Map, Value};
use settings::parse_json_with_comments;
use task::{DebugTaskFile, TaskTemplates, VsCodeDebugTaskFile, VsCodeTaskFile};
use ui::{Checkbox, Headline, HeadlineSize, ToggleState, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::{
    connected_sandbox,
    design::{caption, primary_button, secondary_button},
    settings_review,
};

actions!(
    sands,
    [
        /// Moves the project's VS Code, Zed and agent settings into its `.sands` folder.
        MigrateProjectSettings
    ]
);

/// The tool whose settings a file belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Zed,
    VsCode,
    Claude,
    Agents,
}

impl Origin {
    fn label(self) -> &'static str {
        match self {
            Origin::Zed => "Zed",
            Origin::VsCode => "VS Code",
            Origin::Claude => "Claude",
            Origin::Agents => "Agents",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Settings,
    Tasks,
    Debug,
    Mcp,
    Skills,
    Agent,
    Instructions,
}

#[derive(Clone, Debug, PartialEq)]
struct MigrationItem {
    origin: Origin,
    kind: Kind,
    /// Path relative to the repository root.
    source: String,
    /// Path relative to `.sands`, or `None` when the source cannot be moved.
    target: Option<String>,
    note: String,
    /// Moving this gives agents new abilities, so the user should look at it.
    widens_access: bool,
    selected: bool,
}

/// The settings files of one repository, read from the sandbox with a single `sbx exec`.
#[derive(Clone, Debug, Default, PartialEq)]
struct RepositoryFiles {
    root: String,
    files: BTreeMap<String, String>,
    /// Skill folders, such as `.claude/skills/release`.
    skills: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Change {
    Write { path: String, content: String },
    CopyDirectory { from: String, to: String },
    Remove { path: String },
}

/// Files bigger than this are left alone: settings files are small, and the whole listing goes
/// through one `sbx exec` output.
const MAX_FILE_SIZE: usize = 256 * 1024;

const SCAN_SCRIPT: &str = r#"cd "$1" 2>/dev/null || exit 0
for file in .zed/settings.json .zed/tasks.json .zed/debug.json .vscode/tasks.json \
  .vscode/launch.json .vscode/mcp.json .mcp.json AGENTS.md CLAUDE.md .sands/settings.json \
  .sands/tasks.json .sands/debug.json .sands/mcp.json .sands/instructions.md .claude/agents/*.md \
  .sands/agents/*.md; do
  if [ -f "$file" ] && [ "$(wc -c < "$file")" -le 262144 ]; then
    printf 'file %s %s\n' "$file" "$(base64 -w0 < "$file")"
  fi
done
for skill in .agents/skills/*/SKILL.md .claude/skills/*/SKILL.md .sands/skills/*/SKILL.md; do
  if [ -f "$skill" ]; then
    printf 'skill %s\n' "${skill%/SKILL.md}"
  fi
done
exit 0"#;

const SKILL_ROOTS: [(&str, Origin); 2] = [
    (".agents/skills", Origin::Agents),
    (".claude/skills", Origin::Claude),
];

fn parse_scan(root: &str, output: &str) -> Result<RepositoryFiles> {
    let mut repository = RepositoryFiles {
        root: root.to_string(),
        ..Default::default()
    };
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("file ") {
            // Base64 has no spaces, so the last space separates the path from the content.
            let (path, encoded) = rest
                .rsplit_once(' ')
                .with_context(|| format!("unexpected scan line: {line}"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .with_context(|| format!("{path} is not valid base64"))?;
            if bytes.len() > MAX_FILE_SIZE {
                continue;
            }
            let content =
                String::from_utf8(bytes).with_context(|| format!("{path} is not UTF-8 text"))?;
            repository.files.insert(path.to_string(), content);
        } else if let Some(path) = line.strip_prefix("skill ") {
            repository.skills.push(path.to_string());
        }
    }
    Ok(repository)
}

fn parse_object(content: &str) -> Result<Map<String, Value>> {
    if content.trim().is_empty() {
        return Ok(Map::new());
    }
    parse_json_with_comments::<Map<String, Value>>(content)
}

fn parse_array(content: &str) -> Result<Vec<Value>> {
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }
    parse_json_with_comments::<Vec<Value>>(content)
}

fn vscode_tasks(content: &str) -> Result<Vec<Value>> {
    let tasks = parse_json_with_comments::<VsCodeTaskFile>(content)?;
    let tasks = TaskTemplates::try_from(tasks)?;
    array(serde_json::to_value(tasks)?)
}

fn vscode_launch(content: &str) -> Result<Vec<Value>> {
    let launch = parse_json_with_comments::<VsCodeDebugTaskFile>(content)?;
    let scenarios = DebugTaskFile::try_from(launch)?;
    array(serde_json::to_value(scenarios)?)
}

fn array(value: Value) -> Result<Vec<Value>> {
    match value {
        Value::Array(items) => Ok(items),
        other => anyhow::bail!("expected a list, got {other}"),
    }
}

/// MCP servers in the shape `.sands/mcp.json` keeps: a command with arguments and environment, or
/// a URL with headers. Settings that only meant something to the original tool are dropped.
fn mcp_servers(source: &str, content: &str) -> Result<Map<String, Value>> {
    let object = parse_object(content)?;
    let key = match source {
        ".mcp.json" => "mcpServers",
        ".vscode/mcp.json" => "servers",
        _ => "context_servers",
    };
    let Some(servers) = object.get(key) else {
        return Ok(Map::new());
    };
    let servers = servers
        .as_object()
        .with_context(|| format!("`{key}` in {source} is not an object"))?;
    Ok(servers
        .iter()
        .filter_map(|(name, server)| {
            let server = server.as_object()?;
            let kept = server
                .iter()
                .filter(|(field, _)| {
                    matches!(
                        field.as_str(),
                        "command" | "args" | "env" | "url" | "headers"
                    )
                })
                .map(|(field, value)| (field.clone(), value.clone()))
                .collect::<Map<_, _>>();
            (!kept.is_empty()).then(|| (name.clone(), Value::Object(kept)))
        })
        .collect())
}

/// Claude Code names a model in an agent's frontmatter; Sands names the model tier instead.
fn convert_agent_definition(content: &str) -> (String, Option<(String, &'static str)>) {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (content.to_string(), None);
    }
    let mut converted = vec!["---".to_string()];
    let mut mapping = None;
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter && line.trim() == "---" {
            in_frontmatter = false;
        } else if in_frontmatter && let Some(model) = line.strip_prefix("model:").map(str::trim) {
            let tier = match model.trim_matches(|c| c == '"' || c == '\'') {
                name if name.contains("opus") => Some("plan"),
                name if name.contains("sonnet") || name == "inherit" => Some("work"),
                name if name.contains("haiku") => Some("fast"),
                _ => None,
            };
            if let Some(tier) = tier {
                converted.push(format!("tier: {tier}"));
                mapping = Some((model.to_string(), tier));
                continue;
            }
        }
        converted.push(line.to_string());
    }
    let mut converted = converted.join("\n");
    if content.ends_with('\n') {
        converted.push('\n');
    }
    (converted, mapping)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn plan(repository: &RepositoryFiles) -> Vec<MigrationItem> {
    let files = &repository.files;
    let mut items = Vec::new();
    let mut push = |origin, kind, source: &str, result: Result<(String, String, bool)>| {
        let item = match result {
            Ok((target, note, widens_access)) => MigrationItem {
                origin,
                kind,
                source: source.to_string(),
                target: Some(target),
                note,
                widens_access,
                selected: true,
            },
            Err(error) => MigrationItem {
                origin,
                kind,
                source: source.to_string(),
                target: None,
                note: format!("Can't read it: {error:#}"),
                widens_access: false,
                selected: false,
            },
        };
        items.push(item);
    };

    if let Some(content) = files.get(".zed/settings.json") {
        push(
            Origin::Zed,
            Kind::Settings,
            ".zed/settings.json",
            parse_object(content).map(|settings| {
                let note = if settings.contains_key("context_servers") {
                    "Same format; MCP servers move to mcp.json"
                } else {
                    "Same format"
                };
                ("settings.json".to_string(), note.to_string(), false)
            }),
        );
        if let Ok(servers) = mcp_servers(".zed/settings.json", content)
            && !servers.is_empty()
        {
            push(
                Origin::Zed,
                Kind::Mcp,
                ".zed/settings.json",
                Ok((
                    "mcp.json".to_string(),
                    format!("{} MCP server(s), will run in the sandbox", servers.len()),
                    true,
                )),
            );
        }
    }
    let task_sources: [(&str, Origin, fn(&str) -> Result<Vec<Value>>, &str); 4] = [
        (".zed/tasks.json", Origin::Zed, parse_array, "tasks.json"),
        (
            ".vscode/tasks.json",
            Origin::VsCode,
            vscode_tasks,
            "tasks.json",
        ),
        (".zed/debug.json", Origin::Zed, parse_array, "debug.json"),
        (
            ".vscode/launch.json",
            Origin::VsCode,
            vscode_launch,
            "debug.json",
        ),
    ];
    for (source, origin, convert, target) in task_sources {
        let Some(content) = files.get(source) else {
            continue;
        };
        let kind = if target == "tasks.json" {
            Kind::Tasks
        } else {
            Kind::Debug
        };
        let what = if kind == Kind::Tasks {
            "task"
        } else {
            "configuration"
        };
        push(
            origin,
            kind,
            source,
            convert(content).map(|items| {
                let how = if origin == Origin::VsCode {
                    "converted"
                } else {
                    "same format"
                };
                (
                    target.to_string(),
                    format!("{} {what}(s), {how}", items.len()),
                    false,
                )
            }),
        );
    }
    for (source, origin) in [
        (".mcp.json", Origin::Claude),
        (".vscode/mcp.json", Origin::VsCode),
    ] {
        let Some(content) = files.get(source) else {
            continue;
        };
        push(
            origin,
            Kind::Mcp,
            source,
            mcp_servers(source, content).map(|servers| {
                (
                    "mcp.json".to_string(),
                    format!("{} MCP server(s), will run in the sandbox", servers.len()),
                    true,
                )
            }),
        );
    }
    for (root, origin) in SKILL_ROOTS {
        let count = repository
            .skills
            .iter()
            .filter(|skill| skill.starts_with(&format!("{root}/")))
            .count();
        if count > 0 {
            push(
                origin,
                Kind::Skills,
                root,
                Ok((
                    "skills/".to_string(),
                    format!("{count} skill(s), agents use them"),
                    true,
                )),
            );
        }
    }
    for (source, content) in files.range(".claude/agents/".to_string()..) {
        if !source.starts_with(".claude/agents/") {
            break;
        }
        let (_, mapping) = convert_agent_definition(content);
        let note = match mapping {
            Some((model, tier)) => format!("Role; model: {model} → tier: {tier}"),
            None => "Role".to_string(),
        };
        push(
            Origin::Claude,
            Kind::Agent,
            source,
            Ok((format!("agents/{}", file_name(source)), note, true)),
        );
    }
    for (source, origin) in [("AGENTS.md", Origin::Agents), ("CLAUDE.md", Origin::Claude)] {
        if files.contains_key(source) {
            push(
                origin,
                Kind::Instructions,
                source,
                Ok((
                    "instructions.md".to_string(),
                    "Copied; every agent reads it".to_string(),
                    true,
                )),
            );
        }
    }
    items
}

fn merge_by_label(base: &mut Vec<Value>, additions: Vec<Value>) {
    let mut labels = base
        .iter()
        .filter_map(|item| item.get("label")?.as_str().map(str::to_string))
        .collect::<HashSet<_>>();
    for item in additions {
        let label = item
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_string);
        if label
            .as_ref()
            .is_none_or(|label| labels.insert(label.clone()))
        {
            base.push(item);
        }
    }
}

fn pretty(value: &Value) -> Result<String> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    Ok(text)
}

/// What moving the selected items does to the repository. Existing `.sands` files are kept and
/// only gain entries they don't have yet.
fn changes(
    repository: &RepositoryFiles,
    items: &[MigrationItem],
    remove_originals: bool,
) -> Result<Vec<Change>> {
    let files = &repository.files;
    let selected = |kind: Kind| {
        items
            .iter()
            .filter(move |item| item.selected && item.target.is_some() && item.kind == kind)
    };
    let mut changes = Vec::new();

    let settings_sources = selected(Kind::Settings).collect::<Vec<_>>();
    if !settings_sources.is_empty() {
        let mut settings = match files.get(".sands/settings.json") {
            Some(content) => parse_object(content).context("reading .sands/settings.json")?,
            None => Map::new(),
        };
        for item in settings_sources {
            let content = files.get(&item.source).context("source disappeared")?;
            for (key, value) in parse_object(content)? {
                if key != "context_servers" {
                    settings.entry(key).or_insert(value);
                }
            }
        }
        changes.push(Change::Write {
            path: ".sands/settings.json".to_string(),
            content: pretty(&Value::Object(settings))?,
        });
    }

    for (kind, target) in [
        (Kind::Tasks, ".sands/tasks.json"),
        (Kind::Debug, ".sands/debug.json"),
    ] {
        let sources = selected(kind).collect::<Vec<_>>();
        if sources.is_empty() {
            continue;
        }
        let mut entries = match files.get(target) {
            Some(content) => parse_array(content).with_context(|| format!("reading {target}"))?,
            None => Vec::new(),
        };
        for item in sources {
            let content = files.get(&item.source).context("source disappeared")?;
            let converted = match item.source.as_str() {
                ".vscode/tasks.json" => vscode_tasks(content)?,
                ".vscode/launch.json" => vscode_launch(content)?,
                _ => parse_array(content)?,
            };
            merge_by_label(&mut entries, converted);
        }
        changes.push(Change::Write {
            path: target.to_string(),
            content: pretty(&Value::Array(entries))?,
        });
    }

    let mcp_sources = selected(Kind::Mcp).collect::<Vec<_>>();
    if !mcp_sources.is_empty() {
        let mut servers = match files.get(".sands/mcp.json") {
            Some(content) => parse_object(content)
                .context("reading .sands/mcp.json")?
                .get("servers")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            None => Map::new(),
        };
        for item in mcp_sources {
            let content = files.get(&item.source).context("source disappeared")?;
            for (name, server) in mcp_servers(&item.source, content)? {
                servers.entry(name).or_insert(server);
            }
        }
        let mut object = Map::new();
        object.insert("servers".to_string(), Value::Object(servers));
        changes.push(Change::Write {
            path: ".sands/mcp.json".to_string(),
            content: pretty(&Value::Object(object))?,
        });
    }

    let existing_skills = repository
        .skills
        .iter()
        .filter_map(|skill| skill.strip_prefix(".sands/skills/"))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let mut copied_skills = BTreeSet::new();
    for item in selected(Kind::Skills) {
        for skill in &repository.skills {
            let Some(name) = skill.strip_prefix(&format!("{}/", item.source)) else {
                continue;
            };
            if existing_skills.contains(name) || !copied_skills.insert(name.to_string()) {
                continue;
            }
            changes.push(Change::CopyDirectory {
                from: skill.clone(),
                to: format!(".sands/skills/{name}"),
            });
        }
    }

    for item in selected(Kind::Agent) {
        let target = format!(".sands/agents/{}", file_name(&item.source));
        if files.contains_key(&target) {
            continue;
        }
        let content = files.get(&item.source).context("source disappeared")?;
        changes.push(Change::Write {
            path: target,
            content: convert_agent_definition(content).0,
        });
    }

    let instructions = selected(Kind::Instructions)
        .filter_map(|item| files.get(&item.source))
        .map(|content| content.trim_end().to_string())
        .fold(Vec::<String>::new(), |mut parts, part| {
            if !parts.contains(&part) {
                parts.push(part);
            }
            parts
        });
    if !instructions.is_empty() && !files.contains_key(".sands/instructions.md") {
        changes.push(Change::Write {
            path: ".sands/instructions.md".to_string(),
            content: format!("{}\n", instructions.join("\n\n")),
        });
    }

    if remove_originals {
        let mut removed = BTreeSet::new();
        for item in items
            .iter()
            .filter(|item| item.selected && item.target.is_some())
        {
            if removed.insert(item.source.clone()) {
                changes.push(Change::Remove {
                    path: item.source.clone(),
                });
            }
        }
    }
    Ok(changes)
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn apply_script(root: &str, changes: &[Change]) -> String {
    let mut script = format!("set -e\ncd {}\n", shell_quote(root));
    for change in changes {
        match change {
            Change::Write { path, content } => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(content);
                let parent = path.rsplit_once('/').map_or(".", |(parent, _)| parent);
                script.push_str(&format!(
                    "mkdir -p {}\nprintf '%s' '{encoded}' | base64 -d > {}\n",
                    shell_quote(parent),
                    shell_quote(path)
                ));
            }
            Change::CopyDirectory { from, to } => {
                let parent = to.rsplit_once('/').map_or(".", |(parent, _)| parent);
                script.push_str(&format!(
                    "mkdir -p {}\ncp -R -- {} {}\n",
                    shell_quote(parent),
                    shell_quote(from),
                    shell_quote(to)
                ));
            }
            Change::Remove { path } => {
                script.push_str(&format!("rm -rf -- {}\n", shell_quote(path)));
            }
        }
    }
    script
}

/// Identifies what a project offered to migrate, so the same offer is not shown again after the
/// user moved it or said "Not now".
fn fingerprint(repositories: &[(RepositoryFiles, Vec<MigrationItem>)]) -> String {
    let mut hasher = DefaultHasher::new();
    for (repository, items) in repositories {
        repository.root.hash(&mut hasher);
        for item in items {
            item.source.hash(&mut hasher);
            if let Some(content) = repository.files.get(&item.source) {
                content.hash(&mut hasher);
            }
        }
    }
    format!("{:016x}", hasher.finish())
}

fn answered_path() -> PathBuf {
    paths::data_dir().join("settings_migration.json")
}

fn load_answered() -> BTreeMap<String, String> {
    match fs::read(answered_path()) {
        Ok(contents) => serde_json::from_slice(&contents)
            .context("settings_migration.json is not valid JSON")
            .log_err()
            .unwrap_or_default(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => {
            log::error!("failed to read settings_migration.json: {error}");
            BTreeMap::new()
        }
    }
}

fn remember_answer(sandbox: String, fingerprint: String, cx: &mut App) {
    cx.background_spawn(async move {
        let mut answered = load_answered();
        answered.insert(sandbox, fingerprint);
        let path = answered_path();
        if let Some(directory) = path.parent() {
            fs::create_dir_all(directory)?;
        }
        fs::write(&path, serde_json::to_vec_pretty(&answered)?)
            .with_context(|| format!("failed to write {}", path.display()))
    })
    .detach_and_log_err(cx);
}

async fn scan(sbx: &Sbx, sandbox: &str, roots: &[String]) -> Result<Vec<RepositoryFiles>> {
    let mut repositories = Vec::new();
    for root in roots {
        let output = sbx
            .exec(sandbox, &["bash", "-c", SCAN_SCRIPT, "scan", root.as_str()])
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "reading the settings in {root} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        repositories.push(parse_scan(root, &String::from_utf8_lossy(&output.stdout))?);
    }
    Ok(repositories)
}

fn sandbox_roots(workspace: &Workspace, cx: &App) -> Vec<String> {
    workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
        .collect()
}

/// Sandboxes already checked in this session; scanning costs an `sbx exec` per repository.
static SCANNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        workspace.register_action(|workspace, _: &MigrateProjectSettings, window, cx| {
            offer(workspace, true, window, cx);
        });
        if connected_sandbox(workspace, cx).is_none() {
            return;
        }
        let worktree_store = workspace.project().read(cx).worktree_store();
        cx.subscribe_in(
            &worktree_store,
            window,
            |workspace, _, event, window, cx| {
                if matches!(
                    event,
                    project::worktree_store::WorktreeStoreEvent::WorktreeAdded(_)
                ) {
                    offer(workspace, false, window, cx);
                }
            },
        )
        .detach();
        offer(workspace, false, window, cx);
    })
    .detach();
}

/// Looks for settings to migrate and opens the migration dialog. Unless `asked`, a project is
/// checked once per session and an offer the user already answered is not shown again.
fn offer(workspace: &mut Workspace, asked: bool, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(sandbox) = connected_sandbox(workspace, cx) else {
        if asked {
            let prompt = window.prompt(
                PromptLevel::Info,
                "Open a Sands project first",
                Some("Settings are moved inside a project's sandbox."),
                &["OK"],
                cx,
            );
            cx.background_spawn(prompt).detach();
        }
        return;
    };
    let roots = sandbox_roots(workspace, cx);
    if roots.is_empty() {
        return;
    }
    if !asked {
        let Ok(mut scanned) = SCANNED.lock() else {
            return;
        };
        if !scanned.get_or_insert_default().insert(sandbox.clone()) {
            return;
        }
    }
    cx.spawn_in(window, async move |workspace, cx| {
        let sbx = Sbx::locate()?;
        let repositories = scan(&sbx, &sandbox, &roots).await?;
        let repositories = repositories
            .into_iter()
            .map(|repository| {
                let items = plan(&repository);
                (repository, items)
            })
            .filter(|(_, items)| !items.is_empty())
            .collect::<Vec<_>>();
        if repositories.is_empty() {
            if asked {
                let prompt = workspace.update_in(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Info,
                        "Nothing to move",
                        Some("This project has no VS Code, Zed or agent settings outside .sands."),
                        &["OK"],
                        cx,
                    )
                })?;
                prompt.await.log_err();
            }
            return anyhow::Ok(());
        }
        let fingerprint = fingerprint(&repositories);
        if !asked {
            let answered = cx.background_spawn(async move { load_answered() }).await;
            if answered.get(&sandbox) == Some(&fingerprint) {
                return Ok(());
            }
        }
        workspace.update_in(cx, |workspace, window, cx| {
            let weak_workspace = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, |_, cx| {
                MigrationModal::new(sandbox, repositories, fingerprint, weak_workspace, cx)
            });
        })?;
        Ok(())
    })
    .detach_and_log_err(cx);
}

enum ApplyState {
    Editing,
    Applying,
    Failed(SharedString),
}

pub(crate) struct MigrationModal {
    sandbox: String,
    repositories: Vec<(RepositoryFiles, Vec<MigrationItem>)>,
    fingerprint: String,
    remove_originals: bool,
    state: ApplyState,
    focus_handle: FocusHandle,
    _workspace: WeakEntity<Workspace>,
    _task: Option<Task<()>>,
}

impl MigrationModal {
    fn new(
        sandbox: String,
        repositories: Vec<(RepositoryFiles, Vec<MigrationItem>)>,
        fingerprint: String,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            sandbox,
            repositories,
            fingerprint,
            remove_originals: false,
            state: ApplyState::Editing,
            focus_handle: cx.focus_handle(),
            _workspace: workspace,
            _task: None,
        }
    }

    fn selected_count(&self) -> usize {
        self.repositories
            .iter()
            .flat_map(|(_, items)| items)
            .filter(|item| item.selected && item.target.is_some())
            .count()
    }

    fn not_now(&mut self, cx: &mut Context<Self>) {
        remember_answer(self.sandbox.clone(), self.fingerprint.clone(), cx);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.not_now(cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.apply(cx);
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ApplyState::Applying) || self.selected_count() == 0 {
            return;
        }
        let scripts = self
            .repositories
            .iter()
            .map(|(repository, items)| {
                changes(repository, items, self.remove_originals)
                    .map(|changes| {
                        let written = changes
                            .iter()
                            .filter_map(|change| match change {
                                Change::Write { path, .. } => Some(path.clone()),
                                Change::CopyDirectory { to, .. } => Some(to.clone()),
                                Change::Remove { .. } => None,
                            })
                            .collect::<Vec<_>>();
                        (
                            repository.root.clone(),
                            apply_script(&repository.root, &changes),
                            written,
                        )
                    })
                    .with_context(|| format!("preparing {}", repository.root))
            })
            .collect::<Result<Vec<_>>>();
        let scripts = match scripts {
            Ok(scripts) => scripts,
            Err(error) => {
                self.state = ApplyState::Failed(format!("{error:#}").into());
                cx.notify();
                return;
            }
        };
        self.state = ApplyState::Applying;
        cx.notify();
        let sandbox = self.sandbox.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let sbx = Sbx::locate()?;
                for (root, script, written) in &scripts {
                    let output = sbx.exec(&sandbox, &["bash", "-c", script.as_str()]).await?;
                    anyhow::ensure!(
                        output.status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                    // The user chose these files in this dialog, so they don't wait for
                    // approval like files an agent writes.
                    settings_review::approve_written(&sbx, &sandbox, root, written).await?;
                }
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    remember_answer(this.sandbox.clone(), this.fingerprint.clone(), cx);
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.state =
                        ApplyState::Failed(format!("Moving the settings failed: {error:#}").into());
                    cx.notify();
                }
            })
            .log_err();
        }));
    }

    fn render_item(
        &self,
        repository_index: usize,
        item_index: usize,
        item: &MigrationItem,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let movable = item.target.is_some();
        let note_color = if !movable {
            Color::Error
        } else if item.widens_access {
            Color::Warning
        } else {
            Color::Muted
        };
        let icon = match item.origin {
            Origin::Claude => Some(IconName::AiClaude),
            _ => None,
        };
        h_flex()
            .id(("migration-item", repository_index * 1000 + item_index))
            .min_h_10()
            .px_4()
            .gap_3()
            .border_t_1()
            .border_color(colors.border_variant)
            .child(
                Checkbox::new(
                    ("migration-check", repository_index * 1000 + item_index),
                    if item.selected {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .disabled(!movable)
                .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                    if let Some(item) = this
                        .repositories
                        .get_mut(repository_index)
                        .and_then(|(_, items)| items.get_mut(item_index))
                    {
                        item.selected = *state == ToggleState::Selected;
                        cx.notify();
                    }
                })),
            )
            .child(
                h_flex()
                    .w(rems(15.))
                    .flex_none()
                    .gap_1p5()
                    .children(icon.map(|icon| Icon::new(icon).size(IconSize::Small)))
                    .child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(colors.element_background)
                            .child(
                                Label::new(item.origin.label())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        Label::new(item.source.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .truncate(),
                    ),
            )
            .child(Label::new("→").size(LabelSize::Small).color(Color::Muted))
            .child(
                div().w(rems(10.)).flex_none().child(
                    Label::new(
                        item.target
                            .clone()
                            .map_or_else(|| "—".to_string(), |target| format!(".sands/{target}")),
                    )
                    .size(LabelSize::Small)
                    .buffer_font(cx)
                    .truncate(),
                ),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(item.note.clone())
                        .size(LabelSize::Small)
                        .color(note_color)
                        .truncate(),
                ),
            )
    }

    fn result_lines(&self) -> Vec<(String, bool)> {
        let mut targets = BTreeMap::<String, bool>::new();
        for item in self
            .repositories
            .iter()
            .flat_map(|(_, items)| items)
            .filter(|item| item.selected)
        {
            if let Some(target) = &item.target {
                let widens_access = targets.entry(target.clone()).or_default();
                *widens_access |= item.widens_access;
            }
        }
        targets.into_iter().collect()
    }
}

impl ModalView for MigrationModal {}

impl EventEmitter<DismissEvent> for MigrationModal {}

impl Focusable for MigrationModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MigrationModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.selected_count();
        let applying = matches!(self.state, ApplyState::Applying);
        let several_repositories = self.repositories.len() > 1;
        let mut rows = Vec::new();
        for (repository_index, (repository, items)) in self.repositories.iter().enumerate() {
            if several_repositories {
                rows.push(
                    div()
                        .px_4()
                        .pt_3()
                        .pb_1()
                        .child(
                            Label::new(repository.root.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                        .into_any_element(),
                );
            }
            for (item_index, item) in items.iter().enumerate() {
                rows.push(
                    self.render_item(repository_index, item_index, item, cx)
                        .into_any_element(),
                );
            }
        }
        let status = match &self.state {
            ApplyState::Editing => None,
            ApplyState::Applying => Some(caption("Moving…").into_any_element()),
            ApplyState::Failed(message) => Some(
                Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element(),
            ),
        };
        let result_lines = self.result_lines();
        let colors = cx.theme().colors();
        let any_widening = result_lines.iter().any(|(_, widens)| *widens);

        v_flex()
            .key_context("MigrationModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(60.))
            .max_h(rems(42.))
            .overflow_hidden()
            .child(
                v_flex()
                    .px_5()
                    .pt_4()
                    .pb_3()
                    .gap_1()
                    .child(
                        Headline::new("Move project settings into .sands/")
                            .size(HeadlineSize::Small),
                    )
                    .child(caption(
                        "This project has settings for VS Code, Zed or coding agents. Sands reads \
                         only .sands/, so move what you want to keep. The original files stay \
                         unless you choose to delete them.",
                    )),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        v_flex()
                            .id("migration-items")
                            .flex_1()
                            .min_w_0()
                            .max_h(rems(28.))
                            .overflow_y_scroll()
                            .pb_2()
                            .children(rows),
                    )
                    .child(
                        v_flex()
                            .w(rems(16.))
                            .flex_none()
                            .self_stretch()
                            .p_4()
                            .gap_2()
                            .border_l_1()
                            .border_color(colors.border_variant)
                            .bg(colors.panel_background)
                            .child(
                                Label::new("RESULT")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(".sands/").size(LabelSize::Small).buffer_font(cx))
                            .children(result_lines.into_iter().map(|(target, widens)| {
                                Label::new(format!("  {target}"))
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(if widens {
                                        Color::Warning
                                    } else {
                                        Color::Default
                                    })
                            }))
                            .when(any_widening, |this| {
                                this.child(
                                    div().pt_2().child(
                                        Label::new(
                                            "Yellow items give agents new abilities: \
                                             MCP servers, skills, roles and instructions. \
                                             Look them over before you move them.",
                                        )
                                        .size(LabelSize::Small)
                                        .color(Color::Warning),
                                    ),
                                )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .px_5()
                    .py_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(
                        Checkbox::new(
                            "migration-remove-originals",
                            if self.remove_originals {
                                ToggleState::Selected
                            } else {
                                ToggleState::Unselected
                            },
                        )
                        .label("Delete the original files afterwards")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            |this, state: &ToggleState, _, cx| {
                                this.remove_originals = *state == ToggleState::Selected;
                                cx.notify();
                            },
                        )),
                    )
                    .child(div().flex_1().min_w_0().children(status))
                    .child(
                        secondary_button("migration-not-now", "Not now")
                            .disabled(applying)
                            .on_click(cx.listener(|this, _, _, cx| this.not_now(cx))),
                    )
                    .child(
                        primary_button(
                            "migration-move",
                            format!("Move {count} item{}", if count == 1 { "" } else { "s" }),
                        )
                        .disabled(applying || count == 0)
                        .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(files: &[(&str, &str)], skills: &[&str]) -> RepositoryFiles {
        RepositoryFiles {
            root: "/workspace/app".to_string(),
            files: files
                .iter()
                .map(|(path, content)| (path.to_string(), content.to_string()))
                .collect(),
            skills: skills.iter().map(|skill| skill.to_string()).collect(),
        }
    }

    fn written(changes: &[Change], path: &str) -> Value {
        let content = changes
            .iter()
            .find_map(|change| match change {
                Change::Write {
                    path: written,
                    content,
                } if written == path => Some(content),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{path} was not written: {changes:?}"));
        serde_json::from_str(content).unwrap()
    }

    #[test]
    fn scan_output_is_parsed() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("{\"tab_size\": 2}");
        let output = format!(
            "file .zed/settings.json {encoded}\nskill .claude/skills/release\nskill .sands/skills/old\n"
        );
        let parsed = parse_scan("/workspace/app", &output).unwrap();
        assert_eq!(parsed.files[".zed/settings.json"], "{\"tab_size\": 2}");
        assert_eq!(
            parsed.skills,
            [".claude/skills/release", ".sands/skills/old"]
        );
    }

    #[test]
    fn zed_settings_move_without_their_mcp_servers() {
        let repository = repository(
            &[(
                ".zed/settings.json",
                r#"{
                    // comments are allowed
                    "tab_size": 2,
                    "context_servers": { "db": { "command": "db-mcp", "args": ["--ro"], "remote": false } }
                }"#,
            )],
            &[],
        );
        let items = plan(&repository);
        assert_eq!(
            items
                .iter()
                .map(|item| (item.kind, item.target.as_deref()))
                .collect::<Vec<_>>(),
            [
                (Kind::Settings, Some("settings.json")),
                (Kind::Mcp, Some("mcp.json"))
            ]
        );
        let changes = changes(&repository, &items, false).unwrap();
        assert_eq!(
            written(&changes, ".sands/settings.json"),
            json!({ "tab_size": 2 })
        );
        assert_eq!(
            written(&changes, ".sands/mcp.json"),
            json!({ "servers": { "db": { "command": "db-mcp", "args": ["--ro"] } } })
        );
    }

    #[test]
    fn existing_sands_settings_win() {
        let repository = repository(
            &[
                (
                    ".zed/settings.json",
                    r#"{ "tab_size": 2, "hard_tabs": true }"#,
                ),
                (".sands/settings.json", r#"{ "tab_size": 4 }"#),
            ],
            &[],
        );
        let items = plan(&repository);
        let changes = changes(&repository, &items, false).unwrap();
        assert_eq!(
            written(&changes, ".sands/settings.json"),
            json!({ "tab_size": 4, "hard_tabs": true })
        );
    }

    #[test]
    fn vscode_tasks_are_converted_and_merged_by_label() {
        let repository = repository(
            &[
                (
                    ".vscode/tasks.json",
                    r#"{ "version": "2.0.0", "tasks": [
                        { "label": "build", "type": "shell", "command": "make" },
                        { "label": "test", "type": "shell", "command": "make test" }
                    ] }"#,
                ),
                (
                    ".zed/tasks.json",
                    r#"[{ "label": "build", "command": "cargo build" }]"#,
                ),
            ],
            &[],
        );
        let items = plan(&repository);
        assert!(items.iter().all(|item| item.target.is_some()), "{items:?}");
        let changes = changes(&repository, &items, false).unwrap();
        let tasks = written(&changes, ".sands/tasks.json");
        let labels = tasks
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["label"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(labels, ["build", "test"]);
        assert_eq!(tasks[0]["command"], "cargo build");
    }

    #[test]
    fn unreadable_files_are_reported_and_not_moved() {
        let repository = repository(&[(".vscode/launch.json", "{ not json")], &[]);
        let items = plan(&repository);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].target, None);
        assert!(!items[0].selected);
        assert!(
            items[0].note.starts_with("Can't read it"),
            "{}",
            items[0].note
        );
        assert_eq!(changes(&repository, &items, true).unwrap(), []);
    }

    #[test]
    fn claude_mcp_skills_agents_and_instructions() {
        let repository = repository(
            &[
                (
                    ".mcp.json",
                    r#"{ "mcpServers": { "docs": { "type": "http", "url": "https://docs.example/mcp" } } }"#,
                ),
                (
                    ".claude/agents/reviewer.md",
                    "---\nname: reviewer\nmodel: haiku\n---\nReview the diff.\n",
                ),
                ("CLAUDE.md", "Use tabs.\n"),
                ("AGENTS.md", "Use tabs.\n"),
            ],
            &[
                ".claude/skills/release",
                ".claude/skills/kept",
                ".sands/skills/kept",
            ],
        );
        let items = plan(&repository);
        assert!(
            items
                .iter()
                .filter(|item| item.kind != Kind::Settings)
                .all(|item| item.widens_access)
        );
        let changes = changes(&repository, &items, true).unwrap();
        assert_eq!(
            written(&changes, ".sands/mcp.json"),
            json!({ "servers": { "docs": { "url": "https://docs.example/mcp" } } })
        );
        assert!(changes.contains(&Change::Write {
            path: ".sands/agents/reviewer.md".to_string(),
            content: "---\nname: reviewer\ntier: fast\n---\nReview the diff.\n".to_string(),
        }));
        assert!(changes.contains(&Change::CopyDirectory {
            from: ".claude/skills/release".to_string(),
            to: ".sands/skills/release".to_string(),
        }));
        assert!(!changes.iter().any(|change| matches!(
            change,
            Change::CopyDirectory { to, .. } if to == ".sands/skills/kept"
        )));
        assert!(changes.contains(&Change::Write {
            path: ".sands/instructions.md".to_string(),
            content: "Use tabs.\n".to_string(),
        }));
        assert!(changes.contains(&Change::Remove {
            path: ".claude/skills".to_string()
        }));
    }

    #[test]
    fn script_quotes_paths() {
        let script = apply_script(
            "/workspace/it's",
            &[Change::CopyDirectory {
                from: ".claude/skills/a'; rm -rf ~; '".to_string(),
                to: ".sands/skills/a'; rm -rf ~; '".to_string(),
            }],
        );
        assert!(script.contains(r"cd '/workspace/it'\''s'"), "{script}");
        assert!(
            script.contains(r"cp -R -- '.claude/skills/a'\''; rm -rf ~; '\''' "),
            "{script}"
        );
    }

    use serde_json::json;
}
