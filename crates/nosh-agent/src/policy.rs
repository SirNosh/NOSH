//! Session options and the authority they grant: which tools a session gets, the Task Packet
//! checks enforced at the agent boundary, and the workspace path policy (`index.ts`).

use crate::terminal::{is_js_whitespace, is_terminal_tool};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Nosh,
    MissionDirector,
    ResearchDirector,
    LibrarianResearcher,
    GeneralWorker,
    Reviewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nosh => "nosh",
            Self::MissionDirector => "mission_director",
            Self::ResearchDirector => "research_director",
            Self::LibrarianResearcher => "librarian_researcher",
            Self::GeneralWorker => "general_worker",
            Self::Reviewer => "reviewer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPermissions {
    pub network: String,
    pub subprocess: String,
    pub git_commit: bool,
    pub git_push: bool,
    pub delegation: String,
    pub network_allowlist: Vec<String>,
    pub allowed_tool_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskWorkspace {
    pub worktree_id: String,
    pub branch: String,
    pub starting_commit: String,
    pub write_scopes: Vec<String>,
    pub protected_scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

/// `PiSessionOptions`: the daemon-issued scope, role, workspace and authority of one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOptions {
    pub project_id: String,
    #[serde(default)]
    pub mission_id: Option<String>,
    #[serde(default)]
    pub direction_id: Option<String>,
    #[serde(default)]
    pub autoresearch_id: Option<String>,
    #[serde(default)]
    pub experiment_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    pub agent_id: String,
    pub role: Role,
    pub cwd: PathBuf,
    pub package_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub runtime_scoped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_permissions: Option<TaskPermissions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_workspace: Option<TaskWorkspace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
}

impl SessionOptions {
    /// `options.taskId` is truthy.
    pub fn has_task(&self) -> bool {
        self.task_id.as_deref().is_some_and(|id| !id.is_empty())
    }
}

/// The typed submission tools, in the order sessions register them.
pub const SUBMISSION_TOOLS: [&str; 12] = [
    "nosh_response_submit",
    "nosh_review_submit",
    "nosh_blocker_submit",
    "nosh_graph_change_propose",
    "nosh_delegation_request",
    "nosh_handoff_create",
    "nosh_handoff_teachback",
    "nosh_experiment_propose",
    "nosh_evidence_submit",
    "nosh_episode_submit",
    "nosh_runtime_instruct",
    "nosh_project_contract_submit",
];

/// Task Packet tool id → session tool name.
fn task_tool_name(id: &str) -> Option<&'static str> {
    Some(match id {
        "tool_pi.read" => "nosh_workspace_read",
        "tool_pi.edit" => "nosh_workspace_edit",
        "tool_pi.write" => "nosh_workspace_write",
        "tool_nosh.response.submit" => "nosh_response_submit",
        "tool_nosh.review.submit" => "nosh_review_submit",
        "tool_nosh.blocker.submit" => "nosh_blocker_submit",
        "tool_nosh.graph.change.propose" => "nosh_graph_change_propose",
        "tool_nosh.delegation.request" => "nosh_delegation_request",
        "tool_nosh.handoff.create" => "nosh_handoff_create",
        "tool_nosh.handoff.teachback" => "nosh_handoff_teachback",
        "tool_nosh.experiment.propose" => "nosh_experiment_propose",
        "tool_nosh.evidence.submit" => "nosh_evidence_submit",
        "tool_nosh.episode.submit" => "nosh_episode_submit",
        "tool_nosh.runtime.instruct" => "nosh_runtime_instruct",
        "tool_nosh.network.read" => "nosh_network_read",
        "tool_nosh.project.contract.submit" => "nosh_project_contract_submit",
        _ => return None,
    })
}

/// The session tools a Task Packet grants (sorted), the role's single terminal tool for a task
/// session without one, or the explicit `tools` list otherwise.
pub fn session_tool_ids(options: &SessionOptions) -> Option<Vec<String>> {
    let Some(permissions) = &options.task_permissions else {
        if !options.has_task() {
            return options.tools.clone();
        }
        // Without a Task Packet, a task session's only terminal tool follows its role: reviewers
        // return verdicts.
        let tools: &[&str] = if options.role == Role::Reviewer {
            &["nosh_artifact_read", "nosh_review_submit"]
        } else {
            &["nosh_response_submit"]
        };
        return Some(tools.iter().map(|tool| (*tool).to_owned()).collect());
    };
    let mut permitted: Vec<String> = Vec::new();
    let mut add = |name: &str| {
        if !permitted.iter().any(|known| known == name) {
            permitted.push(name.to_owned());
        }
    };
    for id in &permissions.allowed_tool_ids {
        match id.as_str() {
            "tool_nosh.network.read" => {
                if permissions.network == "allowlisted" && !permissions.network_allowlist.is_empty()
                {
                    add("nosh_network_read");
                }
            }
            "tool_nosh.run" => {
                if permissions.subprocess == "allowlisted"
                    && options.task_workspace.is_some()
                    && options.has_task()
                {
                    add("nosh_run");
                }
            }
            // Commit authority is exercised by the daemon on the worker's behalf (before each
            // nosh_run and after the final answer), never as a model tool.
            "tool_nosh.git.commit" => {}
            "tool_pi.read" | "tool_pi.edit" | "tool_pi.write" => {
                if options.task_workspace.is_some() {
                    add(task_tool_name(id).expect("workspace tools are mapped"));
                }
                if id == "tool_pi.read" && options.task_workspace.is_some() && options.has_task() {
                    add("nosh_artifact_read");
                }
            }
            other => {
                if let Some(name) = task_tool_name(other) {
                    add(name);
                }
            }
        }
    }
    permitted.sort();
    Some(permitted)
}

/// The tools a session's Pi run may call: terminal tools are removed from scoped turns (their
/// records arrive in the terminal envelope), and `nosh_code` joins when it can call any of them.
pub fn execution_tool_ids(options: &SessionOptions, code_mode: bool) -> Option<Vec<String>> {
    let ids = session_tool_ids(options);
    let active = if options.has_task() || options.runtime_scoped {
        let candidates = ids.unwrap_or_else(|| {
            SUBMISSION_TOOLS
                .iter()
                .chain(&["read", "edit", "write", "bash", "grep", "find", "ls"])
                .map(|name| (*name).to_owned())
                .collect()
        });
        Some(
            candidates
                .into_iter()
                .filter(|name| {
                    !is_terminal_tool(name)
                        && options
                            .tools
                            .as_ref()
                            .is_none_or(|tools| tools.contains(name))
                })
                .collect::<Vec<_>>(),
        )
    } else {
        ids
    };
    // nosh_code composes tools the session already has; unscoped chat (no explicit list) never
    // gets it.
    match active {
        Some(mut active) if !code_functions(Some(&active), code_mode).is_empty() => {
            active.push("nosh_code".into());
            Some(active)
        }
        active => active,
    }
}

/// Guest function name → the session tool it calls. Typed submissions and terminal tools are
/// never reachable from a program.
pub const CODE_FUNCTIONS: [(&str, &str); 6] = [
    ("read", "nosh_workspace_read"),
    ("write", "nosh_workspace_write"),
    ("edit", "nosh_workspace_edit"),
    ("run", "nosh_run"),
    ("artifact", "nosh_artifact_read"),
    ("fetch", "nosh_network_read"),
];

/// `NOSH_CODE_MODE=off` removes `nosh_code`. TypeScript reads it on every call; the adapter reads it
/// once, since the daemon's environment does not change while it runs.
pub fn code_mode_enabled() -> bool {
    std::env::var_os("NOSH_CODE_MODE").is_none_or(|mode| mode != "off")
}

/// The guest functions this session may call: those whose tool is active, so a program never
/// exceeds the session's own authority.
pub fn code_functions(active: Option<&[String]>, code_mode: bool) -> Vec<&'static str> {
    let Some(active) = active.filter(|_| code_mode) else {
        return Vec::new();
    };
    CODE_FUNCTIONS
        .iter()
        .filter(|(_, tool)| active.iter().any(|name| name == tool))
        .map(|(name, _)| *name)
        .collect()
}

/// Rejects Task Packet authority the agent boundary cannot enforce.
pub fn assert_enforceable_task_permissions(options: &SessionOptions) -> Result<(), String> {
    let Some(permissions) = &options.task_permissions else {
        return Ok(());
    };
    let allows = |id: &str| permissions.allowed_tool_ids.iter().any(|known| known == id);
    if permissions.network == "enabled" {
        return Err("Task Packet network=enabled is unsupported at the Pi boundary; use the explicit allowlisted network reader".into());
    }
    if permissions.subprocess == "enabled" {
        return Err("Task Packet subprocess authority=enabled is unsupported; only daemon-run contract commands (nosh_run) are allowed".into());
    }
    if permissions.subprocess == "allowlisted"
        && (!allows("tool_nosh.run") || options.task_workspace.is_none() || !options.has_task())
    {
        return Err("Task Packet subprocess authority=allowlisted requires the daemon-run nosh_run tool and a task workspace".into());
    }
    if allows("tool_pi.bash") || allows("tool_nosh.subprocess") {
        return Err("Task Packets cannot expose shell or generic subprocess tools".into());
    }
    let workspace_tools_requested = ["tool_pi.read", "tool_pi.edit", "tool_pi.write"]
        .iter()
        .any(|id| allows(id));
    if (workspace_tools_requested || permissions.git_commit) && options.task_workspace.is_none() {
        return Err(
            "Task Packet workspace scopes are required for filesystem and commit tools".into(),
        );
    }
    if permissions.git_commit != allows("tool_nosh.git.commit") {
        return Err(
            "Task Packet gitCommit authority must exactly match tool_nosh.git.commit".into(),
        );
    }
    if let Some(workspace) = &options.task_workspace
        && (workspace.worktree_id.is_empty()
            || workspace.branch.is_empty()
            || workspace.write_scopes.is_empty()
            || !safe_git_object_id(&workspace.starting_commit)
            || !workspace
                .write_scopes
                .iter()
                .all(|scope| valid_workspace_scope(scope))
            || !workspace
                .protected_scopes
                .iter()
                .all(|scope| valid_workspace_scope(scope)))
    {
        return Err("Task Packet workspace scopes are invalid".into());
    }
    if permissions.git_push {
        return Err("Task Packet gitPush is unsupported at the Pi boundary; external Git push requires explicit non-Pi authority".into());
    }
    Ok(())
}

pub fn safe_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_workspace_scope(scope: &str) -> bool {
    !scope.is_empty()
        && !scope.contains('\0')
        && !scope.starts_with('/')
        && !scope.contains("\\..")
        && !scope.split('/').any(|part| part == "..")
}

/// Path comparisons ignore case on Windows, as Node's win32 path functions do.
fn policy_case(value: &str) -> String {
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value.to_owned()
    }
}

/// `workspaceMatches(path, scope)`: directors write the repository root as "." and folders as
/// "./src"; both mean the same as "**" and "src".
pub fn workspace_matches(path: &str, scope: &str) -> bool {
    let mut normalized = scope.replace('\\', "/");
    normalized.truncate(normalized.trim_end_matches('/').len());
    let mut normalized = normalized.as_str();
    while let Some(rest) = normalized.strip_prefix("./") {
        normalized = rest;
    }
    let normalized = policy_case(normalized);
    if matches!(normalized.as_str(), "**" | "." | "") {
        return true;
    }
    let prefix = normalized.strip_suffix("/**").unwrap_or(&normalized);
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// The absolute path of a relative workspace path, refusing escapes, links, Git metadata, and (when
/// `writing`) paths outside the write scopes or inside protected ones.
pub fn resolve_workspace_path(
    cwd: &Path,
    workspace: Option<&TaskWorkspace>,
    path: &str,
    writing: bool,
) -> Result<PathBuf, String> {
    let Some(workspace) = workspace else {
        return Err("Workspace scopes are required".into());
    };
    let resolve = |path: &Path| nosh_sys::paths::resolve(path).map_err(|error| error.to_string());
    // Node compares text: `C:/x` resolves to `C:\x`, so it counts as relative (and may resolve
    // inside the root).
    if path.is_empty() || path.contains('\0') || resolve(Path::new(path))?.as_os_str() == path {
        return Err("Workspace path must be a relative path".into());
    }
    let root = resolve(cwd)?;
    let escapes = || "Workspace path escapes the assigned root".to_owned();
    let target = resolve(&join_drive_relative(&root, path).ok_or_else(escapes)?)?;
    let relative = nosh_sys::paths::relative_inside(&root, &target)
        .map_err(|error| error.to_string())?
        .filter(|relative| !relative.as_os_str().is_empty())
        .ok_or_else(escapes)?;
    // Node's `relative` ignores case on Windows; re-resolving must give the target's exact text.
    if resolve(&root.join(&relative))?.as_os_str() != target.as_os_str() {
        return Err(escapes());
    }
    assert_unlinked_workspace_path(&root, &target)?;
    let policy_path = policy_case(&nosh_sys::paths::forward_slashes(&relative));
    if policy_path == ".git" || policy_path.starts_with(".git/") {
        return Err("Workspace paths cannot access Git metadata".into());
    }
    if writing
        && (!workspace
            .write_scopes
            .iter()
            .any(|scope| workspace_matches(&policy_path, scope))
            || workspace
                .protected_scopes
                .iter()
                .any(|scope| workspace_matches(&policy_path, scope)))
    {
        return Err("Workspace path is outside write scopes or protected".into());
    }
    Ok(target)
}

/// `path.resolve(root, path)` for the drive-relative form `X:rest`: on root's own drive it is
/// relative to root, as in Node. On another drive Node resolves it against that drive's current
/// directory, outside the workspace; TypeScript then accepts it (`D:x` → `D:\x`), which this port
/// refuses as an escape (`None`).
fn join_drive_relative(root: &Path, path: &str) -> Option<PathBuf> {
    let bytes = path.as_bytes();
    let drive_relative = cfg!(windows)
        && bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !matches!(bytes.get(2), Some(b'\\' | b'/'));
    if !drive_relative {
        return Some(root.join(path));
    }
    let root_drive = root.to_string_lossy().chars().next()?;
    root_drive
        .eq_ignore_ascii_case(&char::from(bytes[0]))
        .then(|| root.join(&path[2..]))
}

fn is_link(path: &Path) -> bool {
    path.exists() && std::fs::symlink_metadata(path).is_ok_and(|stat| stat.is_symlink())
}

/// No ancestor of `root`, and no component from `root` down to `target`, is a symbolic link or
/// junction.
pub fn assert_unlinked_workspace_path(root: &Path, target: &Path) -> Result<(), String> {
    if root.ancestors().any(is_link) {
        return Err("Workspace root cannot traverse a symbolic link or junction".into());
    }
    let relative = nosh_sys::paths::relative_inside(root, target)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let mut cursor = root.to_path_buf();
    for segment in relative.components() {
        cursor.push(segment);
        if is_link(&cursor) {
            return Err("Workspace paths cannot traverse a symbolic link or junction".into());
        }
    }
    Ok(())
}

/// The stable prompt-cache key of a session: Project, role and tool set, without volatile task or
/// agent ids.
pub fn nosh_prompt_cache_key(options: &SessionOptions) -> String {
    let tools = session_tool_ids(options).unwrap_or_else(|| vec!["default".into()]);
    let digest = nosh_wire::sha256(
        &json!({ "projectId": options.project_id, "role": options.role.as_str(), "tools": tools }),
    );
    format!("nosh-v1-{}", &digest[7..39])
}

/// The record parameter is opaque to the provider; name the accepted schemas and where to read
/// them (`schema_dir` holds `<name>.v1.schema.json`).
pub fn submission_tool_description(tool: &str, schema_dir: &Path) -> String {
    let names = nosh_wire::submission_tool_schemas()
        .get(tool)
        .cloned()
        .unwrap_or_default();
    let uris: Vec<String> = names
        .iter()
        .map(|name| format!("https://nosh.dev/schemas/{name}/v1"))
        .collect();
    let paths: Vec<String> = names
        .iter()
        .map(|name| {
            schema_dir
                .join(format!("{name}.v1.schema.json"))
                .display()
                .to_string()
        })
        .collect();
    format!(
        "Submit one typed NOSH record. Accepted $schema: {}. Read the JSON Schema before submitting: {}.",
        uris.join(", "),
        paths.join(", ")
    )
}

/// `PROGRESS: <note>` lines of assistant text (at most three, each at most 1,000 UTF-16 units);
/// the daemon turns each into a progress-update record.
pub fn progress_notes(text: &str) -> Vec<String> {
    text.split(['\n', '\r', '\u{2028}', '\u{2029}'])
        .filter_map(|line| {
            let rest = line
                .trim_start_matches([' ', '\t'])
                .strip_prefix("PROGRESS:")?;
            let note = rest.trim_start_matches([' ', '\t']);
            note.chars()
                .next()
                .filter(|first| !is_js_whitespace(*first))?;
            Some(nosh_wire::js::slice_to(
                note.trim_matches(is_js_whitespace),
                1_000,
            ))
        })
        .take(3)
        .collect()
}

pub const NO_MODEL_ACCOUNT: &str = "No model account is connected. Run `nosh login` in a terminal to connect a ChatGPT or Claude subscription or an API key, then send your message again.";

static MISSING_ACCOUNT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)no api key found|no model (selected|available)|not authenticated|no models available",
    )
    .expect("pattern compiles")
});

/// Pi's credential errors name Pi CLI commands and internal docs paths; NOSH users need NOSH's own
/// next step.
pub fn user_facing_pi_error(message: &str) -> String {
    if MISSING_ACCOUNT.is_match(message) {
        NO_MODEL_ACCOUNT.to_owned()
    } else {
        message.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(
        role: Role,
        task: bool,
        permissions: Option<(&[&str], &str, bool)>,
    ) -> SessionOptions {
        SessionOptions {
            project_id: "prj_1".into(),
            mission_id: None,
            direction_id: None,
            autoresearch_id: None,
            experiment_id: None,
            run_id: None,
            job_id: None,
            task_id: task.then(|| "tsk_1".into()),
            agent_id: "agt_1".into(),
            role,
            cwd: ".".into(),
            package_path: "pi-package".into(),
            tools: None,
            runtime_scoped: false,
            task_permissions: permissions.map(|(tools, subprocess, git_commit)| TaskPermissions {
                network: "disabled".into(),
                subprocess: subprocess.into(),
                git_commit,
                git_push: false,
                delegation: "request_only".into(),
                network_allowlist: Vec::new(),
                allowed_tool_ids: tools.iter().map(|tool| (*tool).to_owned()).collect(),
            }),
            task_workspace: permissions.map(|_| TaskWorkspace {
                worktree_id: "wt_1".into(),
                branch: "main".into(),
                starting_commit: "a".repeat(40),
                write_scopes: vec!["src/**".into()],
                protected_scopes: vec![".nosh/**".into()],
            }),
            thinking_level: None,
            model: None,
        }
    }

    fn names(ids: Option<Vec<String>>) -> Vec<String> {
        ids.unwrap_or_default()
    }

    #[test]
    fn execution_tools_follow_the_task_packet_and_code_mode() {
        let worker = options(
            Role::GeneralWorker,
            true,
            Some((&["tool_pi.read", "tool_pi.edit"], "disabled", false)),
        );
        let active = names(execution_tool_ids(&worker, true));
        assert_eq!(
            active,
            [
                "nosh_artifact_read",
                "nosh_workspace_edit",
                "nosh_workspace_read",
                "nosh_code"
            ]
        );
        assert!(!names(execution_tool_ids(&worker, false)).contains(&"nosh_code".to_owned()));
        // Reviewers batch-read Artifacts; Directors have no tools; unscoped chat never gets programs.
        let reviewer = names(execution_tool_ids(
            &options(Role::Reviewer, true, None),
            true,
        ));
        assert_eq!(reviewer, ["nosh_artifact_read", "nosh_code"]);
        assert_eq!(
            names(execution_tool_ids(
                &options(Role::MissionDirector, true, None),
                true
            )),
            Vec::<String>::new()
        );
        assert_eq!(
            execution_tool_ids(&options(Role::Nosh, false, None), true),
            None
        );
        // A runtime-scoped session without a list starts from Pi's built-ins minus terminal tools.
        let mut runtime = options(Role::Nosh, false, None);
        runtime.runtime_scoped = true;
        let active = names(execution_tool_ids(&runtime, true));
        assert!(
            active.contains(&"bash".to_owned())
                && !active.contains(&"nosh_response_submit".to_owned())
        );
        assert!(
            active.contains(&"nosh_runtime_instruct".to_owned())
                && !active.contains(&"nosh_code".to_owned())
        );
        // Commit authority never becomes a model tool.
        let committing = options(
            Role::GeneralWorker,
            true,
            Some((
                &[
                    "tool_pi.read",
                    "tool_nosh.git.commit",
                    "tool_nosh.response.submit",
                ],
                "disabled",
                true,
            )),
        );
        assert_eq!(
            names(session_tool_ids(&committing)),
            [
                "nosh_artifact_read",
                "nosh_response_submit",
                "nosh_workspace_read"
            ]
        );
    }

    #[test]
    fn refuses_task_packet_authority_the_boundary_cannot_enforce() {
        let base: &[&str] = &[
            "tool_pi.read",
            "tool_nosh.git.commit",
            "tool_nosh.response.submit",
        ];
        assert_eq!(
            assert_enforceable_task_permissions(&options(
                Role::GeneralWorker,
                true,
                Some((base, "disabled", true))
            )),
            Ok(())
        );
        let error =
            |options: SessionOptions| assert_enforceable_task_permissions(&options).unwrap_err();
        assert!(
            error(options(
                Role::GeneralWorker,
                true,
                Some((base, "allowlisted", true))
            ))
            .contains("subprocess authority=allowlisted")
        );
        assert!(
            error(options(
                Role::GeneralWorker,
                true,
                Some((base, "disabled", false))
            ))
            .contains("gitCommit authority")
        );
        assert!(
            error(options(
                Role::GeneralWorker,
                true,
                Some((base, "enabled", true))
            ))
            .contains("subprocess authority=enabled")
        );
        assert!(
            error(options(
                Role::GeneralWorker,
                true,
                Some((&["tool_nosh.subprocess"], "disabled", false))
            ))
            .contains("shell or generic subprocess")
        );
        let mut network = options(Role::GeneralWorker, true, Some((base, "disabled", true)));
        network.task_permissions.as_mut().unwrap().network = "enabled".into();
        assert!(error(network).contains("network=enabled"));
        let mut push = options(Role::GeneralWorker, true, Some((base, "disabled", true)));
        push.task_permissions.as_mut().unwrap().git_push = true;
        assert!(error(push).contains("gitPush"));
        let mut scopes = options(Role::GeneralWorker, true, Some((base, "disabled", true)));
        scopes.task_workspace.as_mut().unwrap().write_scopes = vec!["../out".into()];
        assert_eq!(error(scopes), "Task Packet workspace scopes are invalid");
        let mut commit = options(Role::GeneralWorker, true, Some((base, "disabled", true)));
        commit.task_workspace.as_mut().unwrap().starting_commit = "A".repeat(40);
        assert_eq!(error(commit), "Task Packet workspace scopes are invalid");
        let mut missing = options(Role::GeneralWorker, true, Some((base, "disabled", true)));
        missing.task_workspace = None;
        assert!(error(missing).contains("workspace scopes are required"));
    }
}
