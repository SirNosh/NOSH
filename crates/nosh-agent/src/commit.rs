//! Task commits, as `commitWorkspaceFiles` and `commitAllWorkspaceChanges`: the daemon commits a
//! worker's edits through Git plumbing in an isolated index, so only explicit, scope-checked,
//! unlinked regular files enter the commit and the real index is never left half-updated.
//!
//! Git runs with a hardened environment of its own (`gitPlumbingEnvironment`): no inherited `GIT_*`
//! variables, no system or global config, hooks, pagers, editors, fsmonitor or signing, and
//! `safe.directory` for exactly the workspace and its owning repository. That is deliberately
//! narrower than `nosh_sys::git::worktree_git_environment`, which serves NOSH's own worktree
//! management under the user's configuration.

use crate::policy::{self, SessionOptions, TaskWorkspace, safe_git_object_id};
use crate::tools::fs_error;
use nosh_sys::paths::{forward_slashes, realpath_native, relative_inside, resolve};
use regex::Regex;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

/// The commits this session made: HEAD may be the starting commit or one of these.
#[derive(Debug, Clone, Default)]
pub struct CommitAuthority {
    pub heads: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub commit: String,
    pub paths: Vec<String>,
}

struct IndexEntry {
    mode: String,
    object_id: String,
}

/// Commits exactly `paths` (each an explicit relative workspace path inside the write scopes, or a
/// deliberate deletion of an indexed file) on the Task branch.
#[allow(clippy::too_many_lines)]
pub fn commit_workspace_files(
    options: &SessionOptions,
    authority: &mut CommitAuthority,
    message: &str,
    paths: &[String],
) -> Result<Commit, String> {
    let workspace = match (&options.task_permissions, &options.task_workspace) {
        (Some(permissions), Some(workspace)) if permissions.git_commit => workspace,
        _ => {
            return Err(
                "Git commit requires Task Packet commit authority and workspace scopes".into(),
            );
        }
    };
    policy::assert_enforceable_task_permissions(options)?;
    let commit_message = message.trim_matches(crate::terminal::is_js_whitespace);
    if commit_message.is_empty() || commit_message.contains('\0') || commit_message.len() > 500 {
        return Err("Git commit message must be nonempty and at most 500 bytes".into());
    }
    if paths.is_empty() || paths.len() > 100 {
        return Err("Git commit requires one to 100 explicit paths".into());
    }
    let root = resolve(&options.cwd).map_err(|error| error.to_string())?;
    let mut requested: Vec<(String, PathBuf)> = Vec::new();
    for requested_path in paths {
        if !safe_git_path(requested_path) {
            return Err(
                "Git commit paths must be explicit relative workspace paths outside .git".into(),
            );
        }
        let target =
            policy::resolve_workspace_path(&options.cwd, Some(workspace), requested_path, true)?;
        let relative = relative_inside(&root, &target)
            .ok()
            .flatten()
            .map(|path| forward_slashes(&path))
            .unwrap_or_default();
        if requested.iter().any(|(path, _)| *path == relative) {
            return Err("Git commit paths must be unique".into());
        }
        requested.push((relative, target));
    }
    let git = Git::new(options);
    let branch_ref = assert_commit_workspace_state(&git, options, workspace)?;
    let old_head = git
        .run(&["rev-parse", "--verify", "HEAD"], None, None)?
        .trim()
        .to_owned();
    if !safe_git_object_id(&old_head)
        || (old_head != workspace.starting_commit && !authority.heads.contains(&old_head))
    {
        return Err(
            "Git HEAD is not the Task workspace starting commit or this session's commit lineage"
                .into(),
        );
    }
    if !git.run(&["ls-files", "-u"], None, None)?.trim().is_empty() {
        return Err("Git commit rejects a conflicted index".into());
    }
    if !nul_paths(&git.run(
        &["diff-index", "--cached", "--name-only", "-z", "HEAD", "--"],
        None,
        None,
    )?)
    .is_empty()
    {
        return Err("Git commit rejects pre-staged index changes".into());
    }
    let real_index = index_entries(&git.run(&["ls-files", "--stage", "-z"], None, None)?)?;
    let mut dirty = workspace_index_differences(&git, &root, &real_index)?;
    dirty.extend(nul_paths(&git.run(
        &["ls-files", "--others", "--exclude-standard", "-z"],
        None,
        None,
    )?));
    if dirty
        .iter()
        .any(|path| !requested.iter().any(|(requested, _)| requested == path))
    {
        return Err(
            "Git commit rejects dirty paths outside the explicit Task Packet path set".into(),
        );
    }

    let index_directory =
        temporary_directory("nosh-git-index-").map_err(|error| error.to_string())?;
    let index_path = index_directory.join("index");
    let index = Some(index_path.as_path());
    let mut candidate_commit = String::new();
    let mut ref_advanced = false;
    let outcome = (|| -> Result<Commit, String> {
        git.run(&["read-tree", &old_head], None, index)?;
        let entries = index_entries(&git.run(&["ls-files", "--stage", "-z"], None, index)?)?;
        for (relative, target) in &requested {
            let indexed = entries.get(relative);
            if indexed.is_some_and(|entry| entry.mode == "160000") {
                return Err("Git commit rejects submodules".into());
            }
            let stat = match std::fs::symlink_metadata(target) {
                Ok(stat) => Some(stat),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(fs_error(&error, "lstat", target)),
            };
            let Some(stat) = stat else {
                if indexed.is_none() {
                    return Err("Git commit deletion must name an indexed regular file".into());
                }
                git.run(&["update-index", "--remove", "--", relative], None, index)?;
                continue;
            };
            policy::assert_unlinked_workspace_path(&root, target)?;
            if !unlinked_regular(&stat, target) {
                return Err("Git commit paths must name unlinked regular files or deliberate indexed deletions".into());
            }
            let target_text = target.to_string_lossy();
            let object_id = git
                .run(
                    &["hash-object", "-w", "--no-filters", "--", &target_text],
                    None,
                    index,
                )?
                .trim()
                .to_owned();
            let after_hash = std::fs::symlink_metadata(target)
                .map_err(|error| fs_error(&error, "lstat", target))?;
            if !unlinked_regular(&after_hash, target) {
                return Err("Git commit path became linked or nonregular while hashing".into());
            }
            if !safe_git_object_id(&object_id) {
                return Err("Git hash-object returned an invalid object ID".into());
            }
            let mode = git_file_mode(&after_hash, indexed.map(|entry| entry.mode.as_str()));
            git.run(
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("{mode},{object_id},{relative}"),
                ],
                None,
                index,
            )?;
        }
        let tree = git.run(&["write-tree"], None, index)?.trim().to_owned();
        let parent_tree = git
            .run(
                &["rev-parse", "--verify", &format!("{old_head}^{{tree}}")],
                None,
                None,
            )?
            .trim()
            .to_owned();
        if !safe_git_object_id(&tree) || tree == parent_tree {
            return Err("Git commit requires at least one effective change".into());
        }
        candidate_commit = git
            .run(
                &["commit-tree", &tree, "-p", &old_head],
                Some(&format!("{commit_message}\n")),
                index,
            )?
            .trim()
            .to_owned();
        if !safe_git_object_id(&candidate_commit) {
            return Err("Git commit-tree returned an invalid commit ID".into());
        }
        let changed_paths = nul_paths(&git.run(
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                "-z",
                &candidate_commit,
            ],
            None,
            index,
        )?);
        let requested_paths: Vec<String> = requested.iter().map(|(path, _)| path.clone()).collect();
        if !same_path_set(&changed_paths, &requested_paths) {
            return Err(
                "Git commit changed paths do not exactly match the explicit Task Packet path set"
                    .into(),
            );
        }
        git.run(
            &["update-ref", &branch_ref, &candidate_commit, &old_head],
            None,
            None,
        )?;
        ref_advanced = true;
        if git
            .run(&["rev-parse", "--verify", "HEAD"], None, None)?
            .trim()
            != candidate_commit
        {
            return Err("Git commit did not atomically advance the checked-out branch".into());
        }
        git.run(&["read-tree", &candidate_commit], None, None)?;
        let post_commit_index =
            index_entries(&git.run(&["ls-files", "--stage", "-z"], None, None)?)?;
        let mut post_commit_dirty: BTreeSet<String> = nul_paths(&git.run(
            &[
                "diff-index",
                "--cached",
                "--name-only",
                "-z",
                &candidate_commit,
                "--",
            ],
            None,
            None,
        )?)
        .into_iter()
        .collect();
        post_commit_dirty.extend(workspace_index_differences(
            &git,
            &root,
            &post_commit_index,
        )?);
        post_commit_dirty.extend(nul_paths(&git.run(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            None,
            None,
        )?));
        if !post_commit_dirty.is_empty() {
            return Err("Git index or worktree changed while advancing the commit".into());
        }
        Ok(Commit {
            commit: candidate_commit.clone(),
            paths: changed_paths,
        })
    })();
    // As in TypeScript, failing to remove the temporary index fails (and rolls back) the commit.
    let outcome = outcome.and_then(|commit| {
        std::fs::remove_dir_all(&index_directory)
            .map(|()| commit)
            .map_err(|error| fs_error(&error, "rm", &index_directory))
    });
    match outcome {
        Ok(commit) => {
            authority.heads.insert(commit.commit.clone());
            Ok(commit)
        }
        Err(error) => {
            let mut failures = 0;
            if ref_advanced {
                failures += usize::from(
                    git.run(
                        &["update-ref", &branch_ref, &old_head, &candidate_commit],
                        None,
                        None,
                    )
                    .is_err(),
                );
                failures += usize::from(git.run(&["read-tree", &old_head], None, None).is_err());
            }
            failures += usize::from(
                std::fs::remove_dir_all(&index_directory).is_err() && index_directory.exists(),
            );
            if failures > 0 {
                return Err("Git commit failed and cleanup or rollback encountered errors".into());
            }
            Err(error)
        }
    }
}

/// The daemon's commit on a worker's behalf: every uncommitted, non-ignored change in the task
/// worktree, through the same scope-checked plumbing as an explicit commit. `None` for a clean
/// worktree.
pub fn commit_all_workspace_changes(
    options: &SessionOptions,
    message: &str,
) -> Result<Option<Commit>, String> {
    let Some(workspace) = &options.task_workspace else {
        return Err("Daemon commits require a task workspace".into());
    };
    let git = Git::new(options);
    let root = resolve(&options.cwd).map_err(|error| error.to_string())?;
    let entries = index_entries(&git.run(&["ls-files", "--stage", "-z"], None, None)?)?;
    let mut dirty: Vec<String> = Vec::new();
    for path in workspace_index_differences(&git, &root, &entries)?
        .into_iter()
        .chain(nul_paths(&git.run(
            &["ls-files", "--others", "--exclude-standard", "-z"],
            None,
            None,
        )?))
    {
        if !dirty.contains(&path) {
            dirty.push(path);
        }
    }
    if dirty.is_empty() {
        return Ok(None);
    }
    let head = git
        .run(&["rev-parse", "--verify", "HEAD"], None, None)?
        .trim()
        .to_owned();
    // Only daemon commits advance a task branch, so a descendant of the starting commit is this task's own lineage.
    if head != workspace.starting_commit {
        git.run(
            &[
                "merge-base",
                "--is-ancestor",
                &workspace.starting_commit,
                &head,
            ],
            None,
            None,
        )?;
    }
    commit_workspace_files(
        options,
        &mut CommitAuthority {
            heads: HashSet::from([head]),
        },
        message,
        &dirty,
    )
    .map(Some)
}

fn assert_commit_workspace_state(
    git: &Git,
    options: &SessionOptions,
    workspace: &TaskWorkspace,
) -> Result<String, String> {
    let root = git.run(&["rev-parse", "--show-toplevel"], None, None)?;
    if !same_workspace_path(Path::new(root.trim()), &options.cwd) {
        return Err("Git worktree root does not match the assigned Task workspace".into());
    }
    let branch = git
        .run(&["symbolic-ref", "--quiet", "--short", "HEAD"], None, None)?
        .trim()
        .to_owned();
    let branch_ref = format!("refs/heads/{branch}");
    if branch.is_empty() || branch != workspace.branch || !safe_branch_ref(&branch_ref) {
        return Err("Git checked-out branch does not match the exact Task Packet branch".into());
    }
    Ok(branch_ref)
}

/// Git reports long paths; the native realpath also expands Windows 8.3 short names.
fn same_workspace_path(left: &Path, right: &Path) -> bool {
    let normalize = |value: &Path| {
        let path = resolve(value).unwrap_or_else(|_| value.to_path_buf());
        let path = realpath_native(&path).unwrap_or(path);
        let text = path.to_string_lossy().into_owned();
        if cfg!(windows) {
            text.to_lowercase()
        } else {
            text
        }
    };
    normalize(left) == normalize(right)
}

fn same_path_set(left: &[String], right: &[String]) -> bool {
    let (mut a, mut b) = (left.to_vec(), right.to_vec());
    a.sort();
    b.sort();
    a == b
}

/// Git plumbing for one workspace, with the hardened environment and the agent's identity.
struct Git {
    cwd: PathBuf,
    environment: Vec<(String, String)>,
}

impl Git {
    fn new(options: &SessionOptions) -> Self {
        let mut environment = git_plumbing_environment(&options.cwd);
        // Agent commits are authored by the agent itself; plumbing ignores the user's global identity.
        let (name, email) = (
            format!("NOSH {}", options.role.as_str()),
            format!("{}@agents.nosh.invalid", options.agent_id),
        );
        for key in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
            environment.push((key.into(), name.clone()));
        }
        for key in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
            environment.push((key.into(), email.clone()));
        }
        Self {
            cwd: options.cwd.clone(),
            environment,
        }
    }

    /// `spawnSync("git", args, { input, timeout: 60 s, maxBuffer: 1 MB })`: stdout, or an error
    /// naming the subcommand.
    fn run(
        &self,
        args: &[&str],
        input: Option<&str>,
        index: Option<&Path>,
    ) -> Result<String, String> {
        let name = args.first().copied().unwrap_or_default();
        let mut command = nosh_sys::process::hidden("git");
        command
            .args(args)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(self.environment.iter().map(|(key, value)| (key, value)));
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        let failed = |code: &str| format!("Git plumbing {name} failed: spawnSync git {code}");
        let (status, output) = match nosh_sys::process::run_with_input(
            &mut command,
            input.map(|text| text.as_bytes().to_vec()),
            Duration::from_secs(60),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Err(failed(if error.kind() == io::ErrorKind::NotFound {
                    "ENOENT"
                } else {
                    "UNKNOWN"
                }));
            }
        };
        if status.is_none() {
            return Err(failed("ETIMEDOUT"));
        }
        if output.stdout.len() > 1_000_000 || output.stderr.len() > 1_000_000 {
            return Err(failed("ENOBUFS"));
        }
        if !output.status.success() {
            return Err(format!("Git plumbing {name} failed"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The inherited environment without `GIT_*` variables, plus Git settings that keep user and
/// repository configuration from running code or changing what plumbing reports.
fn git_plumbing_environment(workspace: &Path) -> Vec<(String, String)> {
    let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut environment: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| !key.to_uppercase().starts_with("GIT_"))
        .collect();
    let pinned = [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", null_device),
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_PAGER", "cat"),
        ("GIT_EDITOR", "true"),
        ("GIT_ASKPASS", "true"),
        ("GIT_CONFIG_COUNT", "9"),
        ("GIT_CONFIG_KEY_0", "core.hooksPath"),
        ("GIT_CONFIG_VALUE_0", null_device),
        ("GIT_CONFIG_KEY_1", "diff.external"),
        ("GIT_CONFIG_VALUE_1", ""),
        ("GIT_CONFIG_KEY_2", "core.pager"),
        ("GIT_CONFIG_VALUE_2", "cat"),
        ("GIT_CONFIG_KEY_3", "core.fsmonitor"),
        ("GIT_CONFIG_VALUE_3", "false"),
        ("GIT_CONFIG_KEY_4", "commit.gpgSign"),
        ("GIT_CONFIG_VALUE_4", "false"),
        ("GIT_CONFIG_KEY_5", "diff.renames"),
        ("GIT_CONFIG_VALUE_5", "false"),
        ("GIT_CONFIG_KEY_6", "core.attributesFile"),
        ("GIT_CONFIG_VALUE_6", null_device),
        // Global config is ignored here, so filesystems without ownership (FAT/exFAT, some network
        // drives) need explicit trust. Trust only the assigned workspace and its owning repository.
        ("GIT_CONFIG_KEY_7", "safe.directory"),
        ("GIT_CONFIG_KEY_8", "safe.directory"),
    ];
    environment.extend(
        pinned
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
    );
    environment.push(("GIT_CONFIG_VALUE_7".into(), git_safe_path(workspace)));
    environment.push((
        "GIT_CONFIG_VALUE_8".into(),
        git_safe_path(&owning_repository(workspace)),
    ));
    environment
}

fn git_safe_path(path: &Path) -> String {
    forward_slashes(&resolve(path).unwrap_or_else(|_| path.to_path_buf()))
}

// JavaScript's multiline `$` also ends a line at `\r`, and its `.` matches no line terminator.
static GITDIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?mR)^gitdir:\s*([^\n\r\u{2028}\u{2029}]+)$").expect("a valid pattern")
});

/// A linked worktree's `.git` file points at `<repository>/.git/worktrees/<id>`; returns
/// `<repository>` (the workspace itself for a main checkout).
fn owning_repository(workspace: &Path) -> PathBuf {
    let marker = std::fs::read_to_string(workspace.join(".git")).ok();
    let git_dir = marker
        .as_deref()
        .and_then(|marker| GITDIR.captures(marker))
        .map(|captures| {
            captures[1]
                .trim_matches(crate::terminal::is_js_whitespace)
                .to_owned()
        });
    if let Some(git_dir) = git_dir.filter(|dir| !dir.is_empty())
        && let Ok(resolved) = resolve(&workspace.join(git_dir))
        && let Some(repository) = resolved
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
    {
        return repository.to_path_buf();
    }
    workspace.to_path_buf()
}

static STAGE_ENTRY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("^([0-7]{6}) ([0-9a-fA-F]+) ([0-3])\t([^\n\r\u{2028}\u{2029}]+)$")
        .expect("a valid pattern")
});

fn index_entries(output: &str) -> Result<HashMap<String, IndexEntry>, String> {
    let mut entries = HashMap::new();
    for value in nul_paths(output) {
        let invalid = || "Git index returned an invalid stage entry".to_owned();
        let captures = STAGE_ENTRY.captures(&value).ok_or_else(invalid)?;
        let (mode, object_id, stage, path) =
            (&captures[1], &captures[2], &captures[3], &captures[4]);
        if !safe_git_object_id(object_id) || !safe_git_path(path) || entries.contains_key(path) {
            return Err(invalid());
        }
        if stage != "0" {
            return Err("Git commit rejects a conflicted index".into());
        }
        entries.insert(
            path.to_owned(),
            IndexEntry {
                mode: mode.to_owned(),
                object_id: object_id.to_lowercase(),
            },
        );
    }
    Ok(entries)
}

/// Indexed paths whose worktree file is missing, linked, nonregular, or has other content.
/// `ls-files` order is kept, as TypeScript's `Map` keeps it.
fn workspace_index_differences(
    git: &Git,
    root: &Path,
    entries: &HashMap<String, IndexEntry>,
) -> Result<Vec<String>, String> {
    let mut ordered: Vec<(&String, &IndexEntry)> = entries.iter().collect();
    ordered.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
    let mut dirty = Vec::new();
    let mut hashable: Vec<(&String, Stamp)> = Vec::new();
    for (path, entry) in ordered {
        if entry.mode != "100644" && entry.mode != "100755" {
            return Err("Git commit rejects repositories with tracked symlinks, submodules, or unsupported indexed modes".into());
        }
        let target = resolve(&root.join(path)).map_err(|error| error.to_string())?;
        policy::assert_unlinked_workspace_path(root, &target)?;
        let stat = match std::fs::symlink_metadata(&target) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                dirty.push(path.clone());
                continue;
            }
            Err(error) => return Err(fs_error(&error, "lstat", &target)),
        };
        if !unlinked_regular(&stat, &target) {
            dirty.push(path.clone());
            continue;
        }
        if !cfg!(windows) && git_file_mode(&stat, Some(&entry.mode)) != entry.mode {
            dirty.push(path.clone());
        }
        hashable.push((path, stamp(&stat, &target)));
    }
    if hashable.is_empty() {
        return Ok(dirty);
    }
    let input: String = hashable
        .iter()
        .map(|(path, _)| format!("{path}\n"))
        .collect();
    let output = git.run(
        &["hash-object", "--no-filters", "--stdin-paths"],
        Some(&input),
        None,
    )?;
    let hashes: Vec<&str> = output
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.is_empty())
        .collect();
    if hashes.len() != hashable.len() || hashes.iter().any(|hash| !safe_git_object_id(hash)) {
        return Err("Git no-filter worktree hash failed".into());
    }
    for ((path, before), hash) in hashable.into_iter().zip(hashes) {
        let target = resolve(&root.join(path)).map_err(|error| error.to_string())?;
        let stat = std::fs::symlink_metadata(&target)
            .map_err(|error| fs_error(&error, "lstat", &target))?;
        if !unlinked_regular(&stat, &target) || stamp(&stat, &target) != before {
            return Err("Git worktree path changed while hashing".into());
        }
        let indexed = &entries[path];
        if !cfg!(windows)
            && git_file_mode(&stat, Some(&indexed.mode)) != indexed.mode
            && !dirty.contains(path)
        {
            dirty.push(path.clone());
        }
        if hash.to_lowercase() != indexed.object_id && !dirty.contains(path) {
            dirty.push(path.clone());
        }
    }
    Ok(dirty)
}

/// What identifies one version of a file: Node's `dev:ino:size:mode:mtimeMs:ctimeMs`.
#[derive(PartialEq)]
struct Stamp {
    identity: Option<(u64, u64)>,
    size: u64,
    readonly: bool,
    executable: bool,
    modified: Option<SystemTime>,
    changed: Option<SystemTime>,
}

fn stamp(stat: &std::fs::Metadata, path: &Path) -> Stamp {
    Stamp {
        identity: nosh_sys::fs::file_identity(path).ok(),
        size: stat.len(),
        readonly: stat.permissions().readonly(),
        executable: is_executable(stat),
        modified: stat.modified().ok(),
        changed: changed_time(stat),
    }
}

#[cfg(unix)]
fn changed_time(stat: &std::fs::Metadata) -> Option<SystemTime> {
    use std::os::unix::fs::MetadataExt;
    let nanos = u64::try_from(stat.ctime()).ok()? * 1_000_000_000
        + u64::try_from(stat.ctime_nsec()).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos))
}

/// Windows has no stable change time in std; creation time stands in, with the volume file index
/// catching a file replaced under the same name.
#[cfg(windows)]
fn changed_time(stat: &std::fs::Metadata) -> Option<SystemTime> {
    stat.created().ok()
}

#[cfg(unix)]
fn is_executable(stat: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    stat.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable(_stat: &std::fs::Metadata) -> bool {
    false
}

fn unlinked_regular(stat: &std::fs::Metadata, path: &Path) -> bool {
    stat.is_file() && nosh_sys::fs::hard_link_count(path).is_ok_and(|links| links <= 1)
}

/// Windows has no executable bit: an indexed 100755 file stays executable.
fn git_file_mode(stat: &std::fs::Metadata, indexed_mode: Option<&str>) -> &'static str {
    if cfg!(windows) && indexed_mode == Some("100755") {
        return "100755";
    }
    if is_executable(stat) {
        "100755"
    } else {
        "100644"
    }
}

fn nul_paths(output: &str) -> Vec<String> {
    output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

fn safe_branch_ref(value: &str) -> bool {
    static BRANCH: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new("^refs/heads/[A-Za-z0-9][A-Za-z0-9._/-]*$").expect("a valid pattern")
    });
    BRANCH.is_match(value)
        && !value.contains("..")
        && !value.contains("//")
        && !value.ends_with('/')
        && !value.ends_with('.')
        && !value
            .split('/')
            .any(|part| part == "." || part.starts_with('.') || part.ends_with(".lock"))
}

fn safe_git_path(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\0', '\r', '\n', ':'])
        && !value.starts_with('-')
        && !value.starts_with(['/', '\\'])
        && !value
            .replace('\\', "/")
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn temporary_directory(prefix: &str) -> io::Result<PathBuf> {
    loop {
        let path = std::env::temp_dir().join(format!("{prefix}{}", &nosh_wire::random_hex()[..6]));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}
