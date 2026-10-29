//! Task worktrees, as `apps/noshd/src/task-worktree.ts`: every Mission or Direction task edits its
//! own Git worktree and branch, never the user's main checkout.

use crate::Result;
use crate::task_postflight::git_output;
use nosh_research::research_control::{Host, ResearchControl, resumable_attempt};
use nosh_sys::git::GitWorkspaceManager;
use nosh_wire::js::string;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Root of every NOSH task or experiment worktree; the runtime starts workspace sessions here.
pub fn task_worktree_path(repository_root: &Path, worktree_id: &str) -> PathBuf {
    repository_root
        .join(".nosh")
        .join("worktrees")
        .join(worktree_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWorktree {
    pub worktree_id: String,
    pub branch: String,
    pub path: PathBuf,
}

/// Gives one task its own worktree and branch from the current checkout. Creation is a durable
/// external operation, replayed idempotently after a restart. A resumed retry is checked out at
/// its earlier attempt's commit while its Task Packet keeps `starting_commit` as the diff base.
pub fn create_task_worktree<H: Host>(
    research: &ResearchControl<H>,
    project: &Value,
    task_id: &str,
    starting_commit: &str,
    checkout_commit: Option<&str>,
) -> Result<TaskWorktree> {
    let checkout_commit = checkout_commit.unwrap_or(starting_commit);
    let project_id = string(project.get("projectId"));
    let repository_root = PathBuf::from(string(project.get("repositoryRoot")));
    let worktree_id = format!("wt_{}", &task_id[4..]);
    let branch = format!("nosh/task-{}", &task_id[4..16]);
    let worktrees = repository_root.join(".nosh").join("worktrees");
    let state = repository_root.join(".nosh").join("git");
    // Self-ignoring directories keep the main checkout clean even without Project .gitignore entries.
    for directory in [&worktrees, &state] {
        std::fs::create_dir_all(directory)?;
        let ignore = directory.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(&ignore, "*\n")?;
        }
    }
    let mut request =
        json!({ "branch": branch, "baseCommit": starting_commit, "worktreeId": worktree_id });
    if checkout_commit != starting_commit {
        request["checkoutCommit"] = json!(checkout_commit);
    }
    let intent = research.begin_external_operation(
        &project_id,
        "git.worktree.create",
        &format!("git-task-worktree:{task_id}"),
        &request,
    )?;
    let manager = GitWorkspaceManager::new(&repository_root, &worktrees, &state)?;
    let path = manager.create(&branch, checkout_commit, &worktree_id)?;
    research.complete_external_operation(
        &project_id,
        &string(intent.get("intentId")),
        &json!({ "branch": branch, "worktree": path.to_string_lossy() }),
    )?;
    Ok(TaskWorktree {
        worktree_id,
        branch,
        path,
    })
}

/// Where a retried worker resumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumePoint {
    /// The ending commit of the node's latest postflight-passed attempt.
    Resume { task_id: String, commit: String },
    /// Such an attempt exists but cannot be resumed (`missing_commit` or `head_moved`).
    Skipped {
        task_id: String,
        commit: Option<String>,
        reason: &'static str,
    },
}

/// The ending commit of the node's latest postflight-passed attempt, when that commit still
/// contains `head` (every integrated change); when such an attempt cannot be resumed, why not.
pub fn resume_point<H: Host>(
    research: &ResearchControl<H>,
    project: &Value,
    events: &[Value],
    node_id: &str,
    head: &str,
) -> Result<Option<ResumePoint>> {
    let attempt = resumable_attempt(events, node_id);
    let Some(task_id) = attempt.get("taskId").and_then(Value::as_str) else {
        return Ok(None);
    };
    let project_id = string(project.get("projectId"));
    let completion =
        research.terminal_record(&project_id, task_id, Some("general-worker-completion"))?;
    let commit = completion
        .as_ref()
        .and_then(|record| record.pointer("/codeChanges/endingCommit"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if commit.as_deref() == Some(head) {
        return Ok(None);
    }
    let repository_root = PathBuf::from(string(project.get("repositoryRoot")));
    let git = |args: &[&str]| git_output(&repository_root, args).0 == Some(0);
    let skipped = |reason| ResumePoint::Skipped {
        task_id: task_id.to_owned(),
        commit: commit.clone(),
        reason,
    };
    let Some(found) = commit.as_deref() else {
        return Ok(Some(skipped("missing_commit")));
    };
    if !git(&["cat-file", "-e", &format!("{found}^{{commit}}")]) {
        return Ok(Some(skipped("missing_commit")));
    }
    if !git(&["merge-base", "--is-ancestor", head, found]) {
        return Ok(Some(skipped("head_moved")));
    }
    Ok(Some(ResumePoint::Resume {
        task_id: task_id.to_owned(),
        commit: found.to_owned(),
    }))
}

/// Tells a resumed worker that its worktree already holds the earlier attempt's work, and that its
/// runs must be its own.
pub fn resume_instruction(commit: &str) -> String {
    format!(
        "Your worktree already contains an earlier attempt's work at this node (commit {}), which passed postflight. Keep what is correct and fix what the failures above name; do not start over. Re-run the declared commands and cite those runs: an earlier attempt's runs do not count for this one.",
        nosh_wire::js::slice_to(commit, 12)
    )
}
