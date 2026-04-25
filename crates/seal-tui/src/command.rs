//! Command definitions for the command palette.
//! TODO: DELETE THIS LINE

use crate::message::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    Quit,
    SelectTheme,
    ToggleDiffView,
    ToggleDiffWrap,
    ToggleSidebar,
    OpenFileInEditor,
}

#[derive(Clone)]
pub struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub id: CommandId,
    pub category: &'static str,
    pub shortcut: Option<&'static str>,
    /// Whether this command represents the currently active state (shows bullet).
    pub active: bool,
}

#[must_use]
pub fn get_commands() -> Vec<CommandSpec> {
    vec![
        // --- View ---
        CommandSpec {
            name: "Toggle diff view",
            description: "Toggle between unified and side-by-side diff",
            id: CommandId::ToggleDiffView,
            category: "View",
            shortcut: Some("v"),
            active: false,
        },
        CommandSpec {
            name: "Toggle line wrap",
            description: "Toggle line wrapping in diffs",
            id: CommandId::ToggleDiffWrap,
            category: "View",
            shortcut: Some("w"),
            active: false,
        },
        CommandSpec {
            name: "Toggle sidebar",
            description: "Show or hide the file sidebar",
            id: CommandId::ToggleSidebar,
            category: "View",
            shortcut: Some("s"),
            active: false,
        },
        CommandSpec {
            name: "Select theme",
            description: "Choose a theme from the list",
            id: CommandId::SelectTheme,
            category: "View",
            shortcut: None,
            active: false,
        },
        // --- Session ---
        CommandSpec {
            name: "Open in editor",
            description: "Open the current file in an external editor",
            id: CommandId::OpenFileInEditor,
            category: "Session",
            shortcut: Some("o"),
            active: false,
        },
        CommandSpec {
            name: "Quit",
            description: "Quit the application",
            id: CommandId::Quit,
            category: "Session",
            shortcut: Some("q"),
            active: false,
        },
    ]
}

#[must_use]
pub const fn command_id_to_message(id: CommandId) -> Message {
    match id {
        CommandId::Quit => Message::Quit,
        CommandId::SelectTheme => Message::ShowThemePicker,
        CommandId::ToggleDiffView => Message::ToggleDiffView,
        CommandId::ToggleDiffWrap => Message::ToggleDiffWrap,
        CommandId::ToggleSidebar => Message::ToggleSidebar,
        CommandId::OpenFileInEditor => Message::OpenFileInEditor,
    }
}
