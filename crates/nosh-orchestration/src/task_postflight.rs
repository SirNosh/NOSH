//! Deterministic Git postflight of a worker's task, as `apps/noshd/src/task-postflight.ts`:
//! changed paths, worktree state, and the runs a completion may cite.

use crate::{Result, rejected};
use nosh_sys::git::worktree_git_environment;
use nosh_wire::js::string;
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;

/// Whether only the paper write-up changed between `(run commit, ending commit)`.
pub type WriteUp<'a> = &'a dyn Fn(&str, &str) -> bool;

/// `spawnSync("git", ["-C", cwd, ...args], { encoding: "utf8", env: worktreeGitEnvironment(cwd) })`:
/// the exit status (`None` when Git could not start) and untrimmed stdout and stderr.
pub(crate) fn git_output(cwd: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let output = nosh_sys::process::hidden("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .envs(worktree_git_environment(cwd))
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) => (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ),
        Err(_) => (None, String::new(), String::new()),
    }
}

fn status_text(status: Option<i32>) -> String {
    status.map_or_else(|| "spawn-error".to_owned(), |code| code.to_string())
}

/// Every path a diff touches, both sides of a rename or copy included.
pub fn git_changed_paths(
    cwd: &Path,
    starting_commit: &str,
    ending_commit: &str,
) -> Result<Vec<String>> {
    let (status, stdout, stderr) = git_output(
        cwd,
        &[
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            starting_commit,
            ending_commit,
        ],
    );
    if status != Some(0) {
        let stderr = stderr.trim();
        return rejected(if stderr.is_empty() {
            format!("Git postflight diff failed: status {}", status_text(status))
        } else {
            stderr.to_owned()
        });
    }
    let fields: Vec<&str> = stdout.split('\0').collect();
    let mut paths: Vec<String> = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let status = fields[index];
        index += 1;
        if status.is_empty() {
            continue;
        }
        let count = if status.starts_with('R') || status.starts_with('C') {
            2
        } else {
            1
        };
        for _ in 0..count {
            let path = fields.get(index).copied().unwrap_or_default();
            index += 1;
            if path.is_empty() {
                return rejected("Git postflight diff has an incomplete path record");
            }
            if !paths.iter().any(|known| known == path) {
                paths.push(path.to_owned());
            }
        }
    }
    Ok(paths)
}

/// HEAD of a task worktree, and whether it has uncommitted or untracked (non-ignored) changes.
pub fn git_worktree_state(cwd: &Path) -> Result<(String, bool)> {
    let git = |args: &[&str]| -> Result<String> {
        let (status, stdout, stderr) = git_output(cwd, args);
        if status != Some(0) {
            let stderr = stderr.trim();
            return rejected(if stderr.is_empty() {
                format!("git {} failed", args[0])
            } else {
                stderr.to_owned()
            });
        }
        Ok(stdout)
    };
    let head = git(&["rev-parse", "HEAD"])?.trim().to_owned();
    let dirty = !git(&["status", "--porcelain", "--untracked-files=all"])?
        .trim()
        .is_empty();
    Ok((head, dirty))
}

/// Paper write-up committed after a cited run does not change what that run measured.
const WRITE_UP_PATHS: [&str; 2] = ["docs/paper.md", "docs/paper.bib"];

/// Whether only the paper write-up changed between a run's commit and the ending commit.
pub fn write_up_only(repository_root: &Path) -> impl Fn(&str, &str) -> bool + '_ {
    move |run_commit, ending_commit| {
        let (status, _, _) = git_output(
            repository_root,
            &["merge-base", "--is-ancestor", run_commit, ending_commit],
        );
        if status != Some(0) {
            return false;
        }
        git_changed_paths(repository_root, run_commit, ending_commit).is_ok_and(|paths| {
            !paths.is_empty()
                && paths
                    .iter()
                    .all(|path| WRITE_UP_PATHS.contains(&path.as_str()))
        })
    }
}

/// A completion criterion as `citedRunIssues` reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct CitedCriterion {
    pub criterion_id: String,
    pub worker_claim: Option<String>,
    pub validator_run_ids: Vec<String>,
}

/// A cited `job_` validator must be this task's own `nosh_run` that passed on the ending commit
/// (or on an ancestor followed only by paper write-up) with a clean worktree. A criterion deferred
/// to the independent Review may cite clean passing runs on any of the task's commits. Jobs the
/// daemon itself ran for the task (`daemon_jobs`) are authoritative validators too.
pub fn cited_run_issues(
    runs: &[Value],
    task_id: &str,
    ending_commit: &str,
    criteria: &[CitedCriterion],
    daemon_jobs: &[String],
    write_up: Option<WriteUp<'_>>,
) -> Vec<String> {
    let mut issues = Vec::new();
    for criterion in criteria {
        let cited = criterion
            .validator_run_ids
            .iter()
            .filter(|id| id.starts_with("job_") && !daemon_jobs.contains(id));
        for id in cited {
            let criterion_id = &criterion.criterion_id;
            let run = runs.iter().find(|run| {
                run.get("jobId").and_then(Value::as_str) == Some(id)
                    && run.get("taskId").and_then(Value::as_str) == Some(task_id)
            });
            let Some(run) = run else {
                issues.push(format!(
                    "criterion {criterion_id} cites {id}, which is not a nosh_run of this task"
                ));
                continue;
            };
            let clean = run.get("dirty") == Some(&Value::Bool(false));
            if run.get("state").and_then(Value::as_str) != Some("completed")
                || run.get("exitCode").and_then(Value::as_f64) != Some(0.0)
            {
                issues.push(format!(
                    "criterion {criterion_id} cites {id}, which did not pass (exit {})",
                    string(run.get("exitCode"))
                ));
            } else if criterion.worker_claim.as_deref() == Some("deferred_to_review") {
                if !clean {
                    issues.push(format!(
                        "criterion {criterion_id} cites {id}, which ran on uncommitted edits"
                    ));
                }
            } else if !clean
                || (run.get("commit").and_then(Value::as_str) != Some(ending_commit)
                    && !write_up.is_some_and(|write_up| {
                        write_up(&string(run.get("commit")), ending_commit)
                    }))
            {
                issues.push(format!("criterion {criterion_id} cites {id}, which did not run on the clean ending commit {ending_commit} (only the paper write-up, docs/paper.md and docs/paper.bib, may be committed after a cited run)"));
            }
        }
    }
    issues
}

/// `cited_run_issues` for a general-worker completion record; other records cite no runs.
pub fn completion_run_issues(
    runs: &[Value],
    task_id: &str,
    completion: &Value,
    daemon_jobs: &[String],
    write_up: Option<WriteUp<'_>>,
) -> Vec<String> {
    let (Some(ending), Some(criteria)) = (
        completion
            .pointer("/codeChanges/endingCommit")
            .and_then(Value::as_str),
        completion.get("criteria").and_then(Value::as_array),
    ) else {
        return Vec::new();
    };
    let criteria: Vec<CitedCriterion> = criteria
        .iter()
        .map(|criterion| CitedCriterion {
            criterion_id: string(criterion.get("criterionId")),
            worker_claim: Some(string(criterion.get("workerClaim"))),
            validator_run_ids: criterion
                .get("validatorRunIds")
                .and_then(Value::as_array)
                .map(|ids| ids.iter().map(|id| string(Some(id))).collect())
                .unwrap_or_default(),
        })
        .collect();
    cited_run_issues(runs, task_id, ending, &criteria, daemon_jobs, write_up)
}

/// A worker's claimed code change, checked against Git and its Task Packet's workspace
/// (`{ startingCommit, branch, writeScopes, protectedScopes }`). `code_changes` is
/// `{ startingCommit, endingCommit, changedPaths, branch }`.
pub fn validate_git_completion(cwd: &Path, workspace: &Value, code_changes: &Value) -> Vec<String> {
    let mut issues: Vec<String> = Vec::new();
    let add = |issues: &mut Vec<String>, issue: String| {
        if !issues.contains(&issue) {
            issues.push(issue);
        }
    };
    let text = |value: &Value, key: &str| string(value.get(key));
    let starting = text(workspace, "startingCommit");
    let ending = text(code_changes, "endingCommit");
    let branch = text(workspace, "branch");
    if code_changes.get("startingCommit") != workspace.get("startingCommit") {
        add(
            &mut issues,
            "starting commit differs from Task Packet".into(),
        );
    }
    if code_changes.get("branch") != workspace.get("branch") {
        add(&mut issues, "branch differs from Task Packet".into());
    }
    let git = |issues: &mut Vec<String>, args: &[&str]| -> Option<String> {
        let (status, stdout, stderr) = git_output(cwd, args);
        if status != Some(0) {
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                format!("status {}", status_text(status))
            } else {
                stderr.to_owned()
            };
            add(issues, format!("Git {} failed: {detail}", args.join(" ")));
            return None;
        }
        Some(stdout.trim().to_owned())
    };
    let actual_branch = git(&mut issues, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    if actual_branch.is_some_and(|actual| actual != branch) {
        add(
            &mut issues,
            "checked-out branch differs from Task Packet".into(),
        );
    }
    let resolved = git(&mut issues, &["rev-parse", &format!("{ending}^{{commit}}")]);
    let head = git(&mut issues, &["rev-parse", "HEAD"]);
    if resolved.as_deref() != Some(ending.as_str()) || head.as_deref() != Some(ending.as_str()) {
        add(
            &mut issues,
            "ending commit is not the checked-out immutable HEAD".into(),
        );
    }
    let (status, _, stderr) = git_output(cwd, &["merge-base", "--is-ancestor", &starting, &ending]);
    if status == Some(1) {
        add(
            &mut issues,
            "starting commit is not an ancestor of ending commit".into(),
        );
    } else if status != Some(0) {
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            format!("status {}", status_text(status))
        } else {
            stderr.to_owned()
        };
        add(
            &mut issues,
            format!("Git merge-base --is-ancestor failed: {detail}"),
        );
    }
    if git(&mut issues, &["status", "--porcelain"]).is_some_and(|status| !status.is_empty()) {
        add(&mut issues, "workspace is dirty after completion".into());
    }
    let claimed: Vec<String> = code_changes
        .get("changedPaths")
        .and_then(Value::as_array)
        .map(|paths| paths.iter().map(|path| string(Some(path))).collect())
        .unwrap_or_default();
    let mut unique = claimed.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != claimed.len() {
        add(
            &mut issues,
            "completion changedPaths contains duplicates".into(),
        );
    }
    let scopes = |key: &str| -> Vec<String> {
        workspace
            .get(key)
            .and_then(Value::as_array)
            .map(|scopes| scopes.iter().map(|scope| string(Some(scope))).collect())
            .unwrap_or_default()
    };
    match git_changed_paths(cwd, &starting, &ending) {
        Ok(actual) => {
            let mut claimed = claimed;
            claimed.sort();
            let mut sorted = actual.clone();
            sorted.sort();
            if claimed != sorted {
                add(
                    &mut issues,
                    "completion changed path set differs from Git".into(),
                );
            }
            let protected = scopes("protectedScopes");
            if actual
                .iter()
                .any(|path| protected.iter().any(|scope| matches_scope(path, scope)))
            {
                add(&mut issues, "completion changed a protected path".into());
            }
            let writable = scopes("writeScopes");
            if actual
                .iter()
                .any(|path| !writable.iter().any(|scope| matches_scope(path, scope)))
            {
                add(
                    &mut issues,
                    "completion changed a path outside its Task Packet write scope".into(),
                );
            }
        }
        Err(error) => {
            let message = error.to_string();
            add(
                &mut issues,
                if message.is_empty() {
                    "Git changed-path derivation failed".into()
                } else {
                    message
                },
            );
        }
    }
    issues
}

fn matches_scope(path: &str, scope: &str) -> bool {
    if scope == "**" {
        return true;
    }
    let prefix = scope.strip_suffix("/**").unwrap_or(scope);
    path == prefix || path.starts_with(&format!("{prefix}/"))
}
