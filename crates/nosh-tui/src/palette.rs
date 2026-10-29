//! The command catalogue and the pure palette/form helpers: a port of `apps/tui/src/palette.ts`.
//!
//! Specified by tui-workspace.md §3 (3.1-3.10, golden vectors §3.11) = tui-dialogs-visual.md §2 (the
//! two agree, DV-2), with workspace CA-5 for the fingerprint. Lengths count UTF-16 units.

// Remove when implemented: the todo!() bodies below leave items and parameters unused.
#![allow(dead_code, unused_variables)]

use crate::view_model::PendingAction;

/// `dialogText`'s default `maxLength`.
pub const DIALOG_TEXT_MAX: usize = 240;

/// `CommandItem['mode']`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandMode {
    /// Opens another dialog by `id`.
    Dialog,
    /// Runs `command` through the workspace's `submit(command, true)`.
    Execute,
    /// Puts `command` (ending in one space) into the composer.
    Compose,
    /// Detaches.
    Quit,
}

/// `CommandItem`: one palette entry; `None` fields are absent in TS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandItem {
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub description: Option<&'static str>,
    pub keywords: Option<&'static str>,
    /// The workspace shortcut shown beside the title; it must match the workspace key handler.
    pub keybind: Option<&'static str>,
    pub command: &'static str,
    pub mode: CommandMode,
}

/// `COMMANDS`: the 29 palette commands in this order, every string verbatim (workspace §3.2).
pub const COMMANDS: [CommandItem; 29] = [
    CommandItem {
        id: "projects",
        title: "Switch project",
        category: "Project",
        description: Some("Choose a registered repository"),
        keywords: Some("open directory workspace"),
        keybind: Some("ctrl+o"),
        command: "/projects",
        mode: CommandMode::Dialog,
    },
    CommandItem {
        id: "open",
        title: "Open project",
        category: "Project",
        description: Some("Open an existing repository and begin intake"),
        keywords: Some("directory folder path"),
        keybind: None,
        command: "/open",
        mode: CommandMode::Dialog,
    },
    CommandItem {
        id: "new",
        title: "New project",
        category: "Project",
        description: Some("Create a repository and begin intake"),
        keywords: Some("create directory folder"),
        keybind: None,
        command: "/new",
        mode: CommandMode::Dialog,
    },
    CommandItem {
        id: "models",
        title: "Select model",
        category: "Model",
        description: Some("Choose provider and model"),
        keywords: Some("model provider authenticated"),
        keybind: Some("f2"),
        command: "/models",
        mode: CommandMode::Dialog,
    },
    CommandItem {
        id: "thinking",
        title: "Thinking level",
        category: "Model",
        description: Some("Choose a supported level for the selected model"),
        keywords: Some("reasoning effort variant"),
        keybind: Some("ctrl+t"),
        command: "/thinking",
        mode: CommandMode::Dialog,
    },
    CommandItem {
        id: "chat",
        title: "Conversation",
        category: "Research",
        description: Some("Return to the current conversation"),
        keywords: None,
        keybind: Some("esc"),
        command: "/chat",
        mode: CommandMode::Execute,
    },
    CommandItem {
        id: "map",
        title: "Research map",
        category: "Research",
        description: Some("Live tree of missions, directions, autoresearch, workers and jobs"),
        keywords: Some("graph tree visual overview dashboard progress experiments"),
        keybind: Some("ctrl+g"),
        command: "/map",
        mode: CommandMode::Execute,
    },
    CommandItem {
        id: "status",
        title: "Research status",
        category: "Research",
        description: Some("Missions, directions, autoresearch and agents"),
        keywords: None,
        keybind: None,
        command: "/status",
        mode: CommandMode::Execute,
    },
    CommandItem {
        id: "jobs",
        title: "Supervised jobs",
        category: "Research",
        description: Some("Inspect jobs and resource use"),
        keywords: None,
        keybind: None,
        command: "/jobs",
        mode: CommandMode::Execute,
    },
    CommandItem {
        id: "approvals",
        title: "Review approvals",
        category: "Research",
        description: Some("Inspect proposals and the project contract"),
        keywords: None,
        keybind: None,
        command: "/approvals",
        mode: CommandMode::Execute,
    },
    CommandItem {
        id: "paths",
        title: "Project paths",
        category: "Project",
        description: Some("Repository, paper and artifact paths"),
        keywords: None,
        keybind: None,
        command: "/paths",
        mode: CommandMode::Execute,
    },
    CommandItem {
        id: "refresh",
        title: "Refresh current view",
        category: "Workspace",
        description: None,
        keywords: None,
        keybind: None,
        command: "/refresh",
        mode: CommandMode::Execute,
    },
    CommandItem {
        id: "confirm",
        title: "Review staged action",
        category: "Actions",
        description: Some("Review before explicitly applying a mutation"),
        keywords: Some("confirm approve apply"),
        keybind: None,
        command: "/confirm",
        mode: CommandMode::Dialog,
    },
    CommandItem {
        id: "discard",
        title: "Discard staged action",
        category: "Actions",
        description: Some("Dismiss without applying the action"),
        keywords: None,
        keybind: None,
        command: "/discard",
        mode: CommandMode::Execute,
    },
    CommandItem {
        id: "job",
        title: "Inspect a job",
        category: "Research",
        description: Some("/job <id>"),
        keywords: None,
        keybind: None,
        command: "/job ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "tail",
        title: "Read job output",
        category: "Research",
        description: Some("/tail <id> [stdout|stderr]"),
        keywords: None,
        keybind: None,
        command: "/tail ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "export-paper",
        title: "Export paper",
        category: "Actions",
        description: Some(
            "/export-paper — render docs/paper.md and paper.bib into .nosh/paper-export",
        ),
        keywords: None,
        keybind: None,
        command: "/export-paper",
        mode: CommandMode::Execute,
    },
    CommandItem {
        id: "cancel",
        title: "Stage job cancellation",
        category: "Actions",
        description: Some("/cancel <job-id> — confirmation required"),
        keywords: None,
        keybind: None,
        command: "/cancel ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "checkpoint",
        title: "Stage job checkpoint",
        category: "Actions",
        description: Some("/checkpoint <job-id> — confirmation required"),
        keywords: None,
        keybind: None,
        command: "/checkpoint ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "approve",
        title: "Stage proposal approval",
        category: "Actions",
        description: Some("/approve <proposal-id> <inspected-version>"),
        keywords: None,
        keybind: None,
        command: "/approve ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "start",
        title: "Stage approve and start",
        category: "Actions",
        description: Some("/start <missions|directions|autoresearch> <id> <inspected-version>"),
        keywords: Some("run launch begin approve"),
        keybind: None,
        command: "/start ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "transition",
        title: "Stage state transition",
        category: "Actions",
        description: Some("/transition <family> <id> <inspected-version> <state>"),
        keywords: None,
        keybind: None,
        command: "/transition ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "retry",
        title: "Stage node retry",
        category: "Actions",
        description: Some("/retry <missions|directions> <id> <inspected-version> <node-id>"),
        keywords: None,
        keybind: None,
        command: "/retry ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "steer",
        title: "Stage Mission steer",
        category: "Actions",
        description: Some("/steer <mission-id> <inspected-version> <message>"),
        keywords: None,
        keybind: None,
        command: "/steer ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "control",
        title: "Stage mission control",
        category: "Actions",
        description: Some("/control <id> <inspected-version> <pause|resume|stop>"),
        keywords: None,
        keybind: None,
        command: "/control ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "amend-contract",
        title: "Stage contract amendment",
        category: "Actions",
        description: Some(
            "/amend-contract <JSON-file-path> — next approved Project contract version",
        ),
        keywords: None,
        keybind: None,
        command: "/amend-contract ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "create",
        title: "Stage research entity creation",
        category: "Actions",
        description: Some("/create <family> <JSON-file-path>"),
        keywords: None,
        keybind: None,
        command: "/create ",
        mode: CommandMode::Compose,
    },
    CommandItem {
        id: "help",
        title: "Command reference",
        category: "Workspace",
        description: Some("All commands and safety notes"),
        keywords: None,
        keybind: None,
        command: "/help",
        mode: CommandMode::Execute,
    },
    CommandItem {
        id: "quit",
        title: "Quit NOSH",
        category: "Workspace",
        description: Some("Detach only; daemon and jobs keep running"),
        keywords: None,
        keybind: Some("ctrl+c"),
        command: "/quit",
        mode: CommandMode::Quit,
    },
];

/// `PaletteItem`: what `filterPalette` searches and the dialogs list. `disabled` items are dropped
/// before filtering.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaletteItem {
    pub id: String,
    pub title: String,
    pub category: String,
    pub description: Option<String>,
    pub keywords: Option<String>,
    pub disabled: bool,
    pub keybind: Option<String>,
}

impl AsRef<PaletteItem> for PaletteItem {
    fn as_ref(&self) -> &PaletteItem {
        self
    }
}

impl From<&CommandItem> for PaletteItem {
    /// The command's palette fields (`disabled` false).
    fn from(command: &CommandItem) -> Self {
        todo!()
    }
}

/// `ProjectFormValues`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectFormValues {
    pub path: String,
    pub working_title: String,
    pub create_repository: bool,
}

/// `dialogText(String(value), maxLength)`: C0/C1 controls become spaces, then the first
/// `max_length` UTF-16 units (workspace §3.3). Callers stringify non-strings with `js::string`
/// first (`undefined` → `"undefined"`).
pub fn dialog_text(value: &str, max_length: usize) -> String {
    todo!()
}

/// `filterPalette`: the positions in `items` of the enabled items matching every token of `query`,
/// best score first, ties in original order; all enabled items in order for an empty query
/// (workspace §3.4, §3.5).
pub fn filter_palette<T: AsRef<PaletteItem>>(items: &[T], query: &str) -> Vec<usize> {
    todo!()
}

/// `moveSelection`: wraps both ways; -1 when `count < 1` (workspace §3.6).
pub fn move_selection(index: i64, delta: i64, count: i64) -> i64 {
    todo!()
}

/// `slashQuery`: the text after `/` when `value` is a lone slash-command name (no whitespace
/// anywhere), else `None` (workspace §3.7).
pub fn slash_query(value: &str) -> Option<&str> {
    todo!()
}

/// `validateProjectForm`: the first failing rule's message, or `None` (workspace §3.8; the
/// non-boolean `createRepository` rule cannot occur with this type).
pub fn validate_project_form(values: &ProjectFormValues) -> Option<&'static str> {
    todo!()
}

/// `projectFormCommand`: `/open {"path":…,"workingTitle":…,"createRepository":…}` with trimmed
/// values, or the validation message (workspace §3.9).
pub fn project_form_command(values: &ProjectFormValues) -> Result<String, String> {
    todo!()
}

/// `actionFingerprint`: `JSON.stringify([projectId, path, body])` with `body` in insertion order
/// (workspace §3.10, CA-5).
pub fn action_fingerprint(action: &PendingAction) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_verbatim() {
        assert_eq!(COMMANDS.len(), 29);
        let ids: Vec<&str> = COMMANDS.iter().map(|c| c.id).collect();
        assert_eq!(ids[0], "projects");
        assert_eq!(ids[14], "job");
        assert_eq!(ids[28], "quit");
        for command in COMMANDS {
            let trailing = command.command.len() - command.command.trim_end().len();
            if command.mode == CommandMode::Compose {
                assert_eq!(trailing, 1, "{}", command.id);
            } else {
                assert_eq!(trailing, 0, "{}", command.id);
            }
        }
        assert_eq!(COMMANDS[11].description, None);
        assert_eq!(
            COMMANDS[16].description,
            Some("/export-paper — render docs/paper.md and paper.bib into .nosh/paper-export")
        );
        assert_eq!(COMMANDS[15].description, Some("/tail <id> [stdout|stderr]"));
        let keybinds: Vec<&str> = COMMANDS.iter().filter_map(|c| c.keybind).collect();
        assert_eq!(
            keybinds,
            ["ctrl+o", "f2", "ctrl+t", "esc", "ctrl+g", "ctrl+c"]
        );
    }
}
