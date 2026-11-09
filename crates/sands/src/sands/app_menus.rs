use gpui::{App, Menu, MenuItem, OsAction};
use project::DisableAiSettings;
use release_channel::ReleaseChannel;
use sands_actions::{Quit, assistant, debug_panel, dev, git_panel, project_panel};
use settings::Settings;
use terminal_view::terminal_panel;

/// The menu bar, kept to what a Sands project needs, in the order of JetBrains IDEs.
pub fn app_menus(cx: &mut App) -> Vec<Menu> {
    let mut view_items = vec![
        MenuItem::action("Projects", sandbox_ui::ToggleProjectsPanel),
        MenuItem::action("Files", project_panel::ToggleFocus),
        MenuItem::action("Sandbox", sandbox_ui::ToggleSandboxPanel),
        MenuItem::action("Kubernetes", sandbox_ui::OpenKubernetes),
        MenuItem::action("Terminal", terminal_panel::Toggle),
        MenuItem::action("Git", git_panel::ToggleFocus),
        MenuItem::action("Debugger", debug_panel::ToggleFocus),
    ];
    if !DisableAiSettings::get_global(cx).disable_ai {
        view_items.push(MenuItem::action("Agent", assistant::ToggleFocus));
    }
    view_items.extend([
        MenuItem::separator(),
        MenuItem::action(
            "Zoom In",
            sands_actions::IncreaseBufferFontSize { persist: false },
        ),
        MenuItem::action(
            "Zoom Out",
            sands_actions::DecreaseBufferFontSize { persist: false },
        ),
        MenuItem::action(
            "Reset Zoom",
            sands_actions::ResetBufferFontSize { persist: false },
        ),
        MenuItem::separator(),
        MenuItem::action("Toggle Left Panel", workspace::ToggleLeftDock),
        MenuItem::action("Toggle Right Panel", workspace::ToggleRightDock),
        MenuItem::action("Toggle Bottom Panel", workspace::ToggleBottomDock),
    ]);
    if ReleaseChannel::try_global(cx) == Some(ReleaseChannel::Dev) {
        view_items.extend([
            MenuItem::separator(),
            MenuItem::action("Toggle GPUI Inspector", dev::ToggleInspector),
        ]);
    }

    vec![
        Menu {
            name: "Sands".into(),
            disabled: false,
            items: vec![
                MenuItem::action("About Sands", sands_actions::About),
                MenuItem::separator(),
                MenuItem::action("Settings…", sands_actions::OpenSettings),
                MenuItem::action("Keymap…", sands_actions::OpenKeymap),
                MenuItem::separator(),
                #[cfg(target_os = "macos")]
                MenuItem::os_submenu("Services", gpui::SystemMenuType::Services),
                MenuItem::separator(),
                #[cfg(target_os = "macos")]
                MenuItem::action("Hide Sands", super::Hide),
                #[cfg(target_os = "macos")]
                MenuItem::action("Hide Others", super::HideOthers),
                #[cfg(target_os = "macos")]
                MenuItem::action("Show All", super::ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit Sands", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            disabled: false,
            items: vec![
                MenuItem::action("New Project…", sandbox_ui::NewProject),
                MenuItem::action("Open Project…", sandbox_ui::OpenSandbox),
                MenuItem::action("Open Recent…", sands_actions::OpenRecent::default()),
                MenuItem::separator(),
                MenuItem::action("New File", workspace::NewFile),
                MenuItem::separator(),
                MenuItem::action("Save", workspace::Save { save_intent: None }),
                MenuItem::action("Save As…", workspace::SaveAs),
                MenuItem::action("Save All", workspace::SaveAll { save_intent: None }),
                MenuItem::separator(),
                MenuItem::action(
                    "Close Editor",
                    workspace::CloseActiveItem {
                        save_intent: None,
                        close_pinned: true,
                    },
                ),
                MenuItem::action("Close Project", workspace::CloseProject),
                MenuItem::action("Close Window", workspace::CloseWindow),
            ],
        },
        Menu {
            name: "Edit".into(),
            disabled: false,
            items: vec![
                MenuItem::os_action("Undo", editor::actions::Undo, OsAction::Undo),
                MenuItem::os_action("Redo", editor::actions::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", editor::actions::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", editor::actions::Copy, OsAction::Copy),
                MenuItem::os_action("Paste", editor::actions::Paste, OsAction::Paste),
                MenuItem::os_action(
                    "Select All",
                    editor::actions::SelectAll,
                    OsAction::SelectAll,
                ),
                MenuItem::separator(),
                MenuItem::action("Find", search::buffer_search::Deploy::find()),
                MenuItem::action("Find in Project", workspace::DeploySearch::default()),
                MenuItem::separator(),
                MenuItem::action("Comment Line", editor::actions::ToggleComments::default()),
            ],
        },
        Menu {
            name: "View".into(),
            disabled: false,
            items: view_items,
        },
        Menu {
            name: "Navigate".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Back", workspace::GoBack),
                MenuItem::action("Forward", workspace::GoForward),
                MenuItem::separator(),
                MenuItem::action("File…", workspace::ToggleFileFinder::default()),
                MenuItem::action("Symbol…", sands_actions::outline::ToggleOutline),
                MenuItem::action("Line…", editor::actions::ToggleGoToLine),
                MenuItem::separator(),
                MenuItem::action("Declaration", editor::actions::GoToDefinition::default()),
                MenuItem::action("Usages", editor::actions::FindAllReferences::default()),
                MenuItem::separator(),
                MenuItem::action("Next Problem", editor::actions::GoToDiagnostic::default()),
                MenuItem::action(
                    "Previous Problem",
                    editor::actions::GoToPreviousDiagnostic::default(),
                ),
            ],
        },
        Menu {
            name: "Run".into(),
            disabled: false,
            items: vec![
                MenuItem::action(
                    "Run Task…",
                    sands_actions::Spawn::ViaModal {
                        reveal_target: None,
                    },
                ),
                MenuItem::action("Debug…", debugger_ui::Start),
                MenuItem::separator(),
                MenuItem::action("Resume", debugger_ui::Continue),
                MenuItem::action("Step Over", debugger_ui::StepOver),
                MenuItem::action("Step Into", debugger_ui::StepInto),
                MenuItem::action("Step Out", debugger_ui::StepOut),
                MenuItem::separator(),
                MenuItem::action("Toggle Breakpoint", editor::actions::ToggleBreakpoint),
                MenuItem::action("Clear All Breakpoints", debugger_ui::ClearAllBreakpoints),
            ],
        },
        Menu {
            name: "Window".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Minimize", super::Minimize),
                MenuItem::action("Zoom", super::Zoom),
            ],
        },
    ]
}
