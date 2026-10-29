//! The command catalogue and the pure palette/form helpers: a port of `apps/tui/src/palette.ts`.
//!
//! Specified by tui-workspace.md §3 (3.1-3.10, golden vectors §3.11) = tui-dialogs-visual.md §2 (the
//! two agree, DV-2), with workspace CA-5 for the fingerprint. Lengths count UTF-16 units.

use crate::js::{self, JsObject, JsValue};
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
        PaletteItem {
            id: command.id.into(),
            title: command.title.into(),
            category: command.category.into(),
            description: command.description.map(Into::into),
            keywords: command.keywords.map(Into::into),
            disabled: false,
            keybind: command.keybind.map(Into::into),
        }
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
    let cleaned: String = value
        .chars()
        .map(|char| if is_control(char) { ' ' } else { char })
        .collect();
    js::slice_to(&cleaned, max_length)
}

/// `/[\x00-\x1f\x7f-\x9f]/`.
fn is_control(char: char) -> bool {
    matches!(char, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// `matchScore`: exact 120, prefix 100, substring `70 - min(position, 30)`, subsequence 5..=25,
/// else -1. `needle` is lower-case; positions are UTF-16 units.
fn match_score(text: &str, needle: &str) -> i64 {
    let value = js::js_lowercase(text);
    if value == needle {
        return 120;
    }
    if value.starts_with(needle) {
        return 100;
    }
    if let Some(position) = js::index_of(&value, needle, 0) {
        return 70 - position.min(30) as i64;
    }
    let (mut cursor, mut first, mut last) = (0, None, 0);
    for char in needle.chars() {
        let Some(found) = js::index_of(&value, char.encode_utf8(&mut [0; 4]), cursor) else {
            return -1;
        };
        first.get_or_insert(found);
        last = found;
        cursor = found + 1;
    }
    let spread = last as i64 - first.unwrap_or(0) as i64 - js::length(needle) as i64 + 1;
    25 - spread.min(20)
}

/// `filterPalette`: the positions in `items` of the enabled items matching every token of `query`,
/// best score first, ties in original order; all enabled items in order for an empty query
/// (workspace §3.4, §3.5).
pub fn filter_palette<T: AsRef<PaletteItem>>(items: &[T], query: &str) -> Vec<usize> {
    let query = js::js_trim(query);
    let query = js::js_lowercase(query.strip_prefix('/').unwrap_or(query));
    let tokens: Vec<&str> = js::js_split_ws(&query)
        .into_iter()
        .filter(|token| !token.is_empty())
        .collect();
    let mut ranked: Vec<(usize, i64)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| !item.as_ref().disabled)
        .filter_map(|(position, item)| {
            let item = item.as_ref();
            let fields = [
                (item.id.as_str(), 4),
                (item.title.as_str(), 3),
                (item.category.as_str(), 1),
                (item.description.as_deref().unwrap_or(""), 1),
                (item.keywords.as_deref().unwrap_or(""), 2),
            ];
            let mut total = 0;
            for token in &tokens {
                let best = fields
                    .iter()
                    .map(|(text, weight)| match match_score(text, token) {
                        score if score < 0 => -1,
                        score => score * weight,
                    })
                    .max()
                    .unwrap_or(-1);
                if best < 0 {
                    return None;
                }
                total += best;
            }
            Some((position, total))
        })
        .collect();
    // A stable sort keeps ties in their original order.
    ranked.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    ranked.into_iter().map(|(position, _)| position).collect()
}

/// `moveSelection`: wraps both ways; -1 when `count < 1` (workspace §3.6).
pub fn move_selection(index: i64, delta: i64, count: i64) -> i64 {
    if count < 1 {
        return -1;
    }
    ((index.max(0) + delta) % count + count) % count
}

/// `slashQuery`: the text after `/` when `value` is a lone slash-command name (no whitespace
/// anywhere), else `None` (workspace §3.7).
pub fn slash_query(value: &str) -> Option<&str> {
    value
        .strip_prefix('/')
        .filter(|name| !name.chars().any(js::is_js_whitespace))
}

/// `validateProjectForm`: the first failing rule's message, or `None` (workspace §3.8; the
/// non-boolean `createRepository` rule cannot occur with this type).
pub fn validate_project_form(values: &ProjectFormValues) -> Option<&'static str> {
    let (path, title) = (&values.path, &values.working_title);
    if js::js_trim(path).is_empty() {
        Some("Enter a repository path.")
    } else if js::js_trim(title).is_empty() {
        Some("Enter a working title.")
    } else if path.chars().chain(title.chars()).any(is_control) {
        Some("Path and title must not contain control characters.")
    } else if js::length(path) > 4096 {
        Some("Repository path is too long (4096 characters maximum).")
    } else if js::length(title) > 240 {
        Some("Working title is too long (240 characters maximum).")
    } else {
        None
    }
}

/// `projectFormCommand`: `/open {"path":…,"workingTitle":…,"createRepository":…}` with trimmed
/// values, or the validation message (workspace §3.9).
pub fn project_form_command(values: &ProjectFormValues) -> Result<String, String> {
    if let Some(error) = validate_project_form(values) {
        return Err(error.into());
    }
    let mut body = JsObject::new();
    body.set("path", js::js_trim(&values.path).into());
    body.set("workingTitle", js::js_trim(&values.working_title).into());
    body.set("createRepository", values.create_repository.into());
    Ok(format!("/open {}", js::json_stringify(&body.into())))
}

/// `actionFingerprint`: `JSON.stringify([projectId, path, body])` with `body` in insertion order
/// (workspace §3.10, CA-5).
pub fn action_fingerprint(action: &PendingAction) -> String {
    js::json_stringify(&JsValue::Array(vec![
        action.project_id.as_str().into(),
        action.path.as_str().into(),
        action.body.clone().into(),
    ]))
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

    fn ids(items: &[PaletteItem], query: &str) -> String {
        let found = filter_palette(items, query);
        found
            .iter()
            .map(|&position| items[position].id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The `filterPalette` vectors of workspace §3.11, palette and slash-menu titles.
    #[test]
    fn filter_palette_golden_vectors() {
        let palette: Vec<PaletteItem> = COMMANDS.iter().map(PaletteItem::from).collect();
        let slash: Vec<PaletteItem> = COMMANDS
            .iter()
            .map(|command| PaletteItem {
                title: format!("{}  {}", command.command.trim_end(), command.title),
                ..command.into()
            })
            .collect();
        let all = COMMANDS.map(|command| command.id).join(", ");
        assert_eq!(ids(&palette, ""), all);
        assert_eq!(ids(&slash, " "), all);
        for (query, expected) in [
            ("model", "models, thinking"),
            ("MODEL", "models, thinking"),
            ("help", "help, map"),
            ("thinking", "thinking"),
            (
                "map",
                "map, confirm, export-paper, discard, quit, start, amend-contract",
            ),
            ("q", "quit, cancel, checkpoint"),
            ("/job", "job, jobs, tail, cancel, checkpoint, quit, map"),
            ("confirm apply", "confirm"),
            ("xyz", ""),
            (
                "open",
                "open, projects, models, export-paper, paths, map, discard, approvals, approve, start, transition, retry, steer, control, amend-contract",
            ),
        ] {
            assert_eq!(ids(&palette, query), expected, "{query}");
            assert_eq!(ids(&slash, query), expected, "slash {query}");
        }
        assert_eq!(
            ids(&palette, "st"),
            "status, start, steer, cancel, checkpoint, approve, transition, retry, control, amend-contract, create, confirm, discard, chat, projects, job, models, open, tail, refresh, thinking, new, paths, jobs, approvals, help, map, export-paper"
        );
        assert_eq!(
            ids(&slash, "st"),
            "status, start, steer, retry, cancel, create, approve, control, checkpoint, transition, confirm, amend-contract, discard, chat, job, projects, open, tail, models, paths, thinking, new, refresh, jobs, approvals, help, map, export-paper"
        );
        assert_eq!(
            ids(&palette, "ca"),
            "cancel, discard, chat, models, create, amend-contract, job, map, paths, help, status, new, confirm, start, projects, open, export-paper, thinking, jobs, approvals, quit, transition, steer, checkpoint, control"
        );
        assert_eq!(
            ids(&slash, "ca"),
            "cancel, discard, chat, models, create, amend-contract, job, map, paths, help, status, new, control, confirm, checkpoint, start, projects, open, export-paper, thinking, jobs, approvals, quit, transition, steer"
        );
    }

    /// The remaining §3.11 vectors.
    #[test]
    fn helper_golden_vectors() {
        for ((index, delta, count), expected) in [
            ((0, 1, 0), -1),
            ((5, 1, 3), 0),
            ((-1, 1, 3), 1),
            ((-1, -1, 3), 2),
            ((0, -1, 3), 2),
            ((2, 1, 3), 0),
            ((5, 8, 10), 3),
            ((0, -8, 10), 2),
            ((3, 0, 5), 3),
            ((-5, 0, 5), 0),
            ((7, 0, 5), 2),
        ] {
            assert_eq!(move_selection(index, delta, count), expected);
        }
        for (value, expected) in [
            ("", None),
            ("/", Some("")),
            ("/he", Some("he")),
            ("/model x", None),
            ("hello", None),
            (" /x", None),
            ("/a/b", Some("a/b")),
            ("/\t", None),
            ("//", Some("/")),
        ] {
            assert_eq!(slash_query(value), expected, "{value:?}");
        }
        assert_eq!(
            dialog_text("a\u{1b}b\nc\u{7f}d\u{85}e\u{a0}f", DIALOG_TEXT_MAX),
            "a b c d e\u{a0}f"
        );
        assert_eq!(dialog_text(&"x".repeat(250), DIALOG_TEXT_MAX).len(), 240);
        assert_eq!(dialog_text("abcdef", 3), "abc");

        let form = |path: &str, title: &str| ProjectFormValues {
            path: path.into(),
            working_title: title.into(),
            create_repository: true,
        };
        for (values, expected) in [
            (form("", "t"), Some("Enter a repository path.")),
            (form("  ", "t"), Some("Enter a repository path.")),
            (form("/r", " "), Some("Enter a working title.")),
            (
                form("/r\u{1}", "t"),
                Some("Path and title must not contain control characters."),
            ),
            (
                form(&"p".repeat(4097), "t"),
                Some("Repository path is too long (4096 characters maximum)."),
            ),
            (
                form("/r", &"t".repeat(241)),
                Some("Working title is too long (240 characters maximum)."),
            ),
            (form("/r", "t"), None),
        ] {
            assert_eq!(validate_project_form(&values), expected);
        }
        assert_eq!(
            project_form_command(&form("  C:\\research\\x \"q\" ", " Study é ")).unwrap(),
            r#"/open {"path":"C:\\research\\x \"q\"","workingTitle":"Study é","createRepository":true}"#
        );
        assert_eq!(
            project_form_command(&form("", "")).unwrap_err(),
            "Enter a repository path."
        );

        let body = crate::js::js_json_parse(
            r#"{"projectId":"p","idempotencyKey":"tui-1","z":1,"a":[1,"x"]}"#,
        )
        .unwrap();
        let action = PendingAction {
            path: "/jobs/j1/cancel".into(),
            body: body.as_object().unwrap().clone(),
            project_id: "p".into(),
            title: String::new(),
            fields: Vec::new(),
        };
        assert_eq!(
            action_fingerprint(&action),
            r#"["p","/jobs/j1/cancel",{"projectId":"p","idempotencyKey":"tui-1","z":1,"a":[1,"x"]}]"#
        );
    }
}
