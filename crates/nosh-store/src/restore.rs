//! Restoring a Project from a backup, as `restoreBackup`, `scheduleProjectRestore`, and
//! `applyScheduledRestores` in `packages/persistence/src/backup.ts`.
//!
//! A restore runs only after complete preflight. Every replaced path is staged next to its
//! destination and swapped in with a rollback copy; Git refs, HEAD, index, and worktree are
//! restored in phases, each undone on failure. Once the caller's `on_commit` succeeds the restore
//! is final: a later cleanup failure is reported as [`Error::RestoreCommitted`], never rolled back.

use crate::backup::{
    BACKUP_UNITS, BackupManifest, DATABASE_SIDECARS, GitHead, RegisteredProject, TreeEntry,
    assert_git_clean, assert_git_head_consistency, assert_no_linked_ancestors,
    assert_regular_contained_file, assert_required_commits, assert_unlinked_from_filesystem_root,
    backup_path, capture_git_head, checked_worktree_path, contained, copy_tree, git_refs,
    git_tree_entries, hash_files, jobs_in, other_job_fingerprints, parent, read_git_head,
    read_required_commits, refs_from, reject_symlinks, run_git, run_git_input, safe_git_object_id,
    same_path, split_nul, verify_backup, verify_bundle, write_json_atomic,
};
use crate::{Error, HostRegistry, Result, rejected};
use nosh_sys::paths::resolve;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// Hooks for the restore phases (`onPhase`, `onCommit`); returning an error aborts and rolls back.
#[derive(Default)]
pub struct RestoreOptions<'a> {
    pub on_phase: Option<PhaseHook<'a>>,
    pub on_commit: Option<CommitHook<'a>>,
}

/// Called with each phase name: `after-refs`, `after-head-tree`, `after-filesystem-swap`, `before-success`.
pub type PhaseHook<'a> = Box<dyn FnMut(&str) -> Result<()> + 'a>;
/// Called once everything is verified; after it succeeds the restore is final.
pub type CommitHook<'a> = Box<dyn FnMut() -> Result<()> + 'a>;

/// What a restore must have produced.
struct Expected<'a> {
    manifest: &'a BackupManifest,
    refs: &'a BTreeMap<String, String>,
    head: &'a GitHead,
    jobs: &'a [String],
    other_jobs: &'a BTreeMap<String, String>,
}

struct Swap {
    destination: PathBuf,
    staged: Option<PathBuf>,
    rollback: Option<PathBuf>,
    moved_rollback: bool,
    moved_staged: bool,
}

fn committed_maintenance(operation: &str, error: &Error) -> Error {
    Error::RestoreCommitted(format!(
        "Restore committed, but {operation} failed: {error}"
    ))
}

fn remove_committed_restore_plan(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(committed_maintenance(
            "scheduled restore plan removal",
            &Error::Io(error),
        )),
        _ => Ok(()),
    }
}

/// Records a pending restore of `backup_id` for the next daemon start.
pub fn schedule_project_restore(
    data_directory: &Path,
    project: &RegisteredProject,
    backup_id: &str,
) -> Result<()> {
    let backup_directory = backup_path(data_directory, backup_id)?;
    let manifest = verify_backup(&backup_directory)?;
    if manifest.project.project_id != project.project_id
        || !same_path(
            Path::new(&manifest.project.repository_root),
            Path::new(&project.repository_root),
        )
    {
        return rejected(
            "Backup Project identity or repository root does not match the registered Project",
        );
    }
    let data_root = resolve(data_directory)?;
    let plans_root = data_root.join("restore-plans");
    assert_no_linked_ancestors(&data_root, &plans_root, "Restore plan root")?;
    std::fs::create_dir_all(&plans_root)?;
    assert_no_linked_ancestors(&data_root, &plans_root, "Restore plan root")?;
    let plan = json!({ "version": 1, "state": "pending", "project": project, "backupId": backup_id, "backupDirectory": backup_directory, "scheduledAt": nosh_wire::now_utc() });
    write_json_atomic(
        &plans_root.join(format!("{}.json", project.project_id)),
        &plan,
    )
}

/// Applies every pending restore plan (the daemon calls this at start, before anything else
/// opens the Projects); returns how many plans completed.
pub fn apply_scheduled_restores(data_directory: &Path) -> Result<usize> {
    let data_root = resolve(data_directory)?;
    let plans_root = data_root.join("restore-plans");
    assert_no_linked_ancestors(&data_root, &plans_root, "Restore plan root")?;
    if !plans_root.exists() {
        return Ok(0);
    }
    let registry = HostRegistry::open(&data_root.join("host.sqlite"))?;
    let mut restored = 0;
    for entry in std::fs::read_dir(&plans_root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_file() || !name.ends_with(".json") {
            continue;
        }
        let path = plans_root.join(&name);
        let plan: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let truthy = |v: &Value| v.as_str().is_some_and(|s| !s.is_empty());
        let state_ok = matches!(plan.get("state"), None | Some(Value::String(_)))
            && plan
                .get("state")
                .and_then(Value::as_str)
                .is_none_or(|s| s == "pending" || s == "committed");
        let well_formed = plan["version"] == 1
            && state_ok
            && truthy(&plan["project"]["projectId"])
            && truthy(&plan["project"]["databasePath"])
            && truthy(&plan["project"]["repositoryRoot"])
            && truthy(&plan["backupId"])
            && truthy(&plan["backupDirectory"]);
        if !well_formed {
            return rejected(format!("Restore plan is malformed: {name}"));
        }
        if plan["state"] == "committed" {
            remove_committed_restore_plan(&path)?;
            restored += 1;
            continue;
        }
        let planned = &plan["project"];
        let project = registry
            .list()?
            .into_iter()
            .find(|candidate| candidate["projectId"] == planned["projectId"]);
        let Some(project) = project.filter(|p| {
            same_path(
                Path::new(p["repositoryRoot"].as_str().unwrap_or_default()),
                Path::new(planned["repositoryRoot"].as_str().unwrap_or_default()),
            ) && same_path(
                Path::new(p["databasePath"].as_str().unwrap_or_default()),
                Path::new(planned["databasePath"].as_str().unwrap_or_default()),
            )
        }) else {
            return rejected(format!("Restore plan Project identity is invalid: {name}"));
        };
        let backup_directory =
            backup_path(&data_root, plan["backupId"].as_str().unwrap_or_default())?;
        if resolve(Path::new(
            plan["backupDirectory"].as_str().unwrap_or_default(),
        ))? != backup_directory
        {
            return rejected(format!("Restore plan backup identity is invalid: {name}"));
        }
        let mut committed_plan = plan.clone();
        committed_plan["state"] = json!("committed");
        let options = RestoreOptions {
            on_phase: None,
            on_commit: Some(Box::new(|| write_json_atomic(&path, &committed_plan))),
        };
        let text = |key: &str| project[key].as_str().unwrap_or_default().to_owned();
        let outcome = restore_backup(
            Path::new(&text("repositoryRoot")),
            &data_root,
            &backup_directory,
            &text("projectId"),
            Path::new(&text("databasePath")),
            true,
            options,
        );
        if let Err(error) = outcome {
            if matches!(error, Error::RestoreCommitted(_))
                && let Err(cleanup) = remove_committed_restore_plan(&path)
            {
                let both = Error::Rejected(format!(
                    "Rollback-data and scheduled-plan cleanup failed ({error}; {cleanup})"
                ));
                return Err(committed_maintenance("rollback-data cleanup", &both));
            }
            return Err(error);
        }
        remove_committed_restore_plan(&path)?;
        restored += 1;
    }
    Ok(restored)
}

/// Restores a Project's repository state, research units, Jobs, and database from a backup.
pub fn restore_backup(
    project_root: &Path,
    data_directory: &Path,
    backup_directory: &Path,
    project_id: &str,
    database_path: &Path,
    daemon_stopped: bool,
    mut options: RestoreOptions<'_>,
) -> Result<BackupManifest> {
    let manifest = verify_backup(backup_directory)?;
    let target_root = resolve(project_root)?;
    let data_root = resolve(data_directory)?;
    let database = resolve(database_path)?;
    if !daemon_stopped {
        return rejected("Refusing restore while noshd is running; stop the daemon first");
    }
    if manifest.project.project_id != project_id {
        return rejected("Backup belongs to a different Project");
    }
    if !same_path(Path::new(&manifest.project.repository_root), &target_root) {
        return rejected("Backup repository root does not match the registered restore target");
    }
    if !target_root.is_dir() {
        return rejected("Project repository root does not exist");
    }
    assert_no_linked_ancestors(&target_root, &target_root, "Restore target root")?;
    assert_no_linked_ancestors(&data_root, &data_root, "Restore data root")?;
    assert_no_linked_ancestors(parent(&database), &database, "Restore database")?;
    assert_git_clean(&target_root, &[])?;
    let original_head = capture_git_head(&target_root)?;
    let original_tracked = tracked_paths(&target_root)?;
    let current_refs = git_refs(&target_root, "Current Git ref snapshot")?;
    assert_git_head_consistency(&original_head, &current_refs, "Current Git HEAD")?;
    let expected_refs = refs_from(&std::fs::read_to_string(
        backup_directory.join("git-refs.txt"),
    )?)?;
    let desired_head = read_git_head(backup_directory)?;
    assert_git_head_consistency(&desired_head, &expected_refs, "Backup Git HEAD")?;
    let commits = read_required_commits(backup_directory)?;
    let none = HashSet::new();
    let backup_jobs = jobs_in(&backup_directory.join("jobs"), project_id, "Backup", &none)?;
    let current_jobs = jobs_in(&data_root.join("jobs"), project_id, "Current", &none)?;
    let other_jobs = other_job_fingerprints(&data_root.join("jobs"), project_id, "Current", &none)?;
    if backup_jobs.iter().any(|job| other_jobs.contains_key(job)) {
        return rejected("Backup Project job collides with an unrelated Project job");
    }
    assert_regular_contained_file(backup_directory, "nosh.sqlite", "Backup database")?;

    let mut swaps: Vec<Swap> = Vec::new();
    let (mut refs_mutated, mut head_mutated, mut filesystem_mutated, mut rollback_eligible) =
        (false, false, false, true);
    let mut body = || -> Result<()> {
        let stage =
            |source: Option<PathBuf>, destination: PathBuf, swaps: &mut Vec<Swap>| -> Result<()> {
                if swaps
                    .iter()
                    .any(|swap| same_path(&swap.destination, &destination))
                {
                    return rejected(format!(
                        "Restore has duplicate destination: {}",
                        destination.display()
                    ));
                }
                swaps.push(Swap {
                    destination,
                    staged: source,
                    rollback: None,
                    moved_rollback: false,
                    moved_staged: false,
                });
                Ok(())
            };
        for unit in BACKUP_UNITS {
            let destination = resolve(&target_root.join(unit))?;
            if !contained(&target_root, &destination) {
                return rejected(format!("Restore path escapes allowed root: {unit}"));
            }
            assert_no_linked_ancestors(
                &target_root,
                &destination,
                &format!("Restore destination {unit}"),
            )?;
            let source = backup_directory.join(unit);
            if !source.exists() {
                if destination.exists() {
                    stage(None, destination, &mut swaps)?;
                }
                continue;
            }
            reject_symlinks(&source)?;
            let adjacent = adjacent_path(&destination, "restore");
            std::fs::create_dir_all(parent(&adjacent))?;
            assert_no_linked_ancestors(
                &target_root,
                &adjacent,
                &format!("Restore staging {unit}"),
            )?;
            copy_tree(&source, &adjacent)?;
            stage(Some(adjacent), destination, &mut swaps)?;
        }
        for job_id in &backup_jobs {
            let destination = resolve(&data_root.join("jobs").join(job_id))?;
            if !contained(&data_root, &destination) {
                return rejected(format!("Restore path escapes allowed root: jobs/{job_id}"));
            }
            assert_no_linked_ancestors(
                &data_root,
                &destination,
                &format!("Restore job destination {job_id}"),
            )?;
            let source = backup_directory.join("jobs").join(job_id);
            reject_symlinks(&source)?;
            let adjacent = adjacent_path(&destination, "restore");
            std::fs::create_dir_all(parent(&adjacent))?;
            assert_no_linked_ancestors(
                &data_root,
                &adjacent,
                &format!("Restore job staging {job_id}"),
            )?;
            copy_tree(&source, &adjacent)?;
            stage(Some(adjacent), destination, &mut swaps)?;
        }
        for job_id in current_jobs.iter().filter(|job| !backup_jobs.contains(job)) {
            stage(
                None,
                resolve(&data_root.join("jobs").join(job_id))?,
                &mut swaps,
            )?;
        }
        for suffix in DATABASE_SIDECARS {
            let sidecar = crate::backup::with_suffix(&database, suffix);
            if std::fs::symlink_metadata(&sidecar).is_err() {
                continue;
            }
            assert_no_linked_ancestors(
                parent(&database),
                &sidecar,
                &format!("Restore database sidecar {suffix}"),
            )?;
            stage(None, sidecar, &mut swaps)?;
        }
        let database_adjacent = adjacent_path(&database, "restore-db");
        std::fs::create_dir_all(parent(&database_adjacent))?;
        assert_no_linked_ancestors(
            parent(&database),
            &database_adjacent,
            "Restore database staging",
        )?;
        std::fs::copy(backup_directory.join("nosh.sqlite"), &database_adjacent)?;
        stage(Some(database_adjacent), database.clone(), &mut swaps)?;

        if manifest.git_bundle.has_refs || desired_head.commit.is_some() {
            import_bundle_objects(&target_root, &backup_directory.join("git.bundle"))?;
        }
        assert_required_commits(&target_root, &commits, "Required Git commits")?;
        for hash in expected_refs.values() {
            if !run_git(
                &target_root,
                &["cat-file", "-e", &format!("{hash}^{{object}}")],
            )
            .ok
            {
                return rejected(format!("Backup Git ref object is unavailable: {hash}"));
            }
        }

        refs_mutated = true;
        apply_ref_diff(&target_root, &current_refs, &expected_refs)?;
        phase(&mut options, "after-refs")?;
        head_mutated = true;
        materialize_git_state(&target_root, &desired_head, &original_tracked)?;
        phase(&mut options, "after-head-tree")?;
        filesystem_mutated = true;
        for swap in &mut swaps {
            apply_restore_swap(swap)?;
        }
        phase(&mut options, "after-filesystem-swap")?;
        verify_restored(
            &target_root,
            backup_directory,
            &data_root,
            &database,
            &Expected {
                manifest: &manifest,
                refs: &expected_refs,
                head: &desired_head,
                jobs: &backup_jobs,
                other_jobs: &other_jobs,
            },
            &swaps,
        )?;
        phase(&mut options, "before-success")?;
        if let Some(on_commit) = options.on_commit.as_mut() {
            on_commit()?;
        }
        rollback_eligible = false;
        cleanup_successful_restore(&swaps)
            .map_err(|error| committed_maintenance("rollback-data cleanup", &error))
    };
    let Err(error) = body() else {
        return Ok(manifest);
    };
    if !rollback_eligible {
        return Err(error);
    }
    let mut failures = Vec::new();
    let mut attempt = |label: &str, action: &mut dyn FnMut() -> Result<()>| {
        if let Err(rollback_error) = action() {
            failures.push(format!("{label}: {rollback_error}"));
        }
    };
    if filesystem_mutated {
        attempt("Filesystem rollback", &mut || {
            rollback_restore_swaps(&swaps)
        });
    }
    if head_mutated {
        attempt("Git HEAD/index/worktree rollback", &mut || {
            materialize_git_state(&target_root, &original_head, &tracked_paths(&target_root)?)
        });
    }
    if refs_mutated {
        attempt("Git ref rollback", &mut || {
            apply_ref_diff(
                &target_root,
                &git_refs(&target_root, "Rollback Git ref snapshot")?,
                &current_refs,
            )
        });
    }
    attempt("Restore staging cleanup", &mut || {
        cleanup_unapplied_restore_staging(&swaps)
    });
    if failures.is_empty() {
        return Err(error);
    }
    let mut causes = vec![error.to_string()];
    causes.extend(failures);
    Err(Error::RollbackFailed {
        summary: "Restore failed and rollback encountered errors".into(),
        causes,
    })
}

fn phase(options: &mut RestoreOptions<'_>, name: &str) -> Result<()> {
    options
        .on_phase
        .as_mut()
        .map_or(Ok(()), |on_phase| on_phase(name))
}

fn verify_restored(
    target_root: &Path,
    backup_root: &Path,
    data_root: &Path,
    database: &Path,
    expected: &Expected<'_>,
    swaps: &[Swap],
) -> Result<()> {
    let observed_refs = git_refs(target_root, "Restored Git ref snapshot")?;
    if &observed_refs != expected.refs {
        return rejected("Restored Git refs do not match backup");
    }
    let observed_head = capture_git_head(target_root)?;
    assert_git_head_consistency(&observed_head, &observed_refs, "Restored Git HEAD")?;
    if &observed_head != expected.head {
        return rejected("Restored Git HEAD does not match backup");
    }
    let owned: Vec<PathBuf> = swaps
        .iter()
        .flat_map(|swap| [swap.staged.clone(), swap.rollback.clone()])
        .flatten()
        .filter(|path| contained(target_root, path))
        .collect();
    verify_materialized_git_tree(target_root, expected.head, &owned)?;
    for unit in BACKUP_UNITS {
        assert_same_optional_path_bytes(
            &backup_root.join(unit),
            &target_root.join(unit),
            &format!("Restored unit {unit}"),
        )?;
    }
    // Ignore only rollback paths owned by this restore, not arbitrary hidden Jobs.
    let rollback_paths: HashSet<PathBuf> = swaps
        .iter()
        .filter_map(|swap| swap.rollback.as_deref())
        .map(resolve)
        .collect::<std::io::Result<_>>()?;
    let observed_jobs = jobs_in(
        &data_root.join("jobs"),
        &expected.manifest.project.project_id,
        "Restored",
        &rollback_paths,
    )?;
    if observed_jobs.iter().collect::<BTreeSet<_>>()
        != expected.jobs.iter().collect::<BTreeSet<_>>()
        || observed_jobs.len() != expected.jobs.len()
    {
        return rejected("Restored Project job identifiers do not match backup");
    }
    for job_id in expected.jobs {
        assert_same_path_bytes(
            &backup_root.join("jobs").join(job_id),
            &data_root.join("jobs").join(job_id),
            &format!("Restored Project job {job_id}"),
        )?;
    }
    if &other_job_fingerprints(
        &data_root.join("jobs"),
        &expected.manifest.project.project_id,
        "Restored",
        &rollback_paths,
    )? != expected.other_jobs
    {
        return rejected("Other Project jobs changed during restore");
    }
    assert_same_path_bytes(
        &backup_root.join("nosh.sqlite"),
        database,
        "Restored database",
    )?;
    for suffix in DATABASE_SIDECARS {
        if std::fs::symlink_metadata(crate::backup::with_suffix(database, suffix)).is_ok() {
            return rejected(format!("Restored database sidecar {suffix} is present"));
        }
    }
    Ok(())
}

fn assert_same_optional_path_bytes(expected: &Path, observed: &Path, label: &str) -> Result<()> {
    let (left, right) = (
        std::fs::symlink_metadata(expected).is_ok(),
        std::fs::symlink_metadata(observed).is_ok(),
    );
    if left != right {
        return rejected(format!("{label} presence does not match backup"));
    }
    if left {
        assert_same_path_bytes(expected, observed, label)
    } else {
        Ok(())
    }
}

fn assert_same_path_bytes(expected: &Path, observed: &Path, label: &str) -> Result<()> {
    let (left, right) = (
        std::fs::symlink_metadata(expected)?,
        std::fs::symlink_metadata(observed)?,
    );
    if left.is_symlink() || right.is_symlink() {
        return rejected(format!("{label} is symlinked"));
    }
    if left.is_file() != right.is_file() || left.is_dir() != right.is_dir() {
        return rejected(format!("{label} type does not match backup"));
    }
    if left.is_file() {
        if std::fs::read(expected)? != std::fs::read(observed)? {
            return rejected(format!("{label} bytes do not match backup"));
        }
        return Ok(());
    }
    if !left.is_dir() {
        return rejected(format!("{label} has an unsupported file type"));
    }
    let (expected_files, observed_files) = (hash_files(expected)?, hash_files(observed)?);
    if expected_files.len() != observed_files.len() {
        return rejected(format!("{label} file inventory does not match backup"));
    }
    for (a, b) in expected_files.iter().zip(&observed_files) {
        if a != b
            || std::fs::read(expected.join(&a.path))? != std::fs::read(observed.join(&b.path))?
        {
            return rejected(format!("{label} bytes do not match backup: {}", a.path));
        }
    }
    Ok(())
}

fn tracked_paths(repository: &Path) -> Result<Vec<String>> {
    let tracked = run_git(repository, &["ls-files", "-z"]);
    if !tracked.ok {
        return rejected("Could not capture tracked Git paths");
    }
    let mut paths = Vec::new();
    for path in split_nul(&tracked.stdout) {
        checked_worktree_path(repository, path)?;
        paths.push(path.replace('\\', "/"));
    }
    paths.sort();
    Ok(paths)
}

fn materialize_git_state(
    repository: &Path,
    head: &GitHead,
    tracked_before: &[String],
) -> Result<()> {
    if head.state == "symbolic" || head.state == "unborn" {
        if !run_git(
            repository,
            &[
                "symbolic-ref",
                "HEAD",
                head.reference.as_deref().unwrap_or_default(),
            ],
        )
        .ok
        {
            return rejected("Could not restore symbolic Git HEAD");
        }
    } else if !run_git(
        repository,
        &[
            "update-ref",
            "--no-deref",
            "HEAD",
            head.commit.as_deref().unwrap_or_default(),
        ],
    )
    .ok
    {
        return rejected("Could not restore detached Git HEAD");
    }
    let Some(commit) = &head.commit else {
        if !run_git(repository, &["read-tree", "--empty"]).ok {
            return rejected("Could not restore unborn Git index");
        }
        return remove_tracked_worktree_paths(repository, tracked_before);
    };
    materialize_git_tree(repository, commit, tracked_before)
}

fn materialize_git_tree(repository: &Path, commit: &str, tracked_before: &[String]) -> Result<()> {
    let entries = git_tree_entries(repository, commit)?;
    remove_tracked_worktree_paths(repository, tracked_before)?;
    if !run_git(repository, &["read-tree", commit]).ok {
        return rejected("Could not materialize Git index");
    }
    for entry in entries {
        let target = checked_worktree_path(repository, &entry.path)?;
        std::fs::create_dir_all(parent(&target))?;
        assert_no_linked_ancestors(
            repository,
            &target,
            &format!("Git tree path {}", entry.path),
        )?;
        if std::fs::symlink_metadata(&target).is_ok() {
            remove_any(&target)?;
        }
        let blob = run_git(repository, &["cat-file", "blob", &entry.object_id]);
        if !blob.ok {
            return rejected(format!("Could not read Git blob {}", entry.object_id));
        }
        use std::io::Write;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?
            .write_all(&blob.bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode(if entry.mode == "100755" { 0o755 } else { 0o644 }),
            )?;
        }
    }
    Ok(())
}

fn verify_materialized_git_tree(
    repository: &Path,
    head: &GitHead,
    allowed_untracked: &[PathBuf],
) -> Result<()> {
    let expected = match &head.commit {
        Some(commit) => git_tree_entries(repository, commit)?,
        None => Vec::new(),
    };
    if git_index_tree_entries(repository)? != expected {
        return rejected("Restored Git index does not match the selected tree");
    }
    assert_git_clean(repository, allowed_untracked)
}

fn git_index_tree_entries(repository: &Path) -> Result<Vec<TreeEntry>> {
    let listed = run_git(repository, &["ls-files", "--stage", "-z"]);
    if !listed.ok {
        return rejected("Could not enumerate restored Git index");
    }
    let mut paths = HashSet::new();
    let mut entries = Vec::new();
    for line in split_nul(&listed.stdout) {
        let unsafe_entry =
            || Error::Rejected("Restored Git index contains an unsupported or unsafe entry".into());
        let (header, path) = line.split_once('\t').ok_or_else(unsafe_entry)?;
        let fields: Vec<&str> = header.split(' ').collect();
        let [mode, object_id, "0"] = fields[..] else {
            return Err(unsafe_entry());
        };
        if !matches!(mode, "100644" | "100755")
            || !safe_git_object_id(object_id)
            || checked_worktree_path(repository, path).is_err()
            || !paths.insert(path.to_owned())
        {
            return Err(unsafe_entry());
        }
        entries.push(TreeEntry {
            mode: mode.into(),
            object_id: object_id.into(),
            path: path.into(),
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn remove_tracked_worktree_paths(repository: &Path, paths: &[String]) -> Result<()> {
    let mut unique: Vec<&String> = paths.iter().collect::<BTreeSet<_>>().into_iter().collect();
    // Deepest first, so files go before the directories that hold them.
    unique.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| b.cmp(a)));
    for path in unique {
        let target = checked_worktree_path(repository, path)?;
        if std::fs::symlink_metadata(&target).is_ok() {
            assert_no_linked_ancestors(repository, &target, &format!("Git tracked path {path}"))?;
            remove_any(&target)?;
        }
    }
    Ok(())
}

fn apply_ref_diff(
    repository: &Path,
    current: &BTreeMap<String, String>,
    expected: &BTreeMap<String, String>,
) -> Result<()> {
    let zero = zero_object_id(repository, current.values().chain(expected.values()))?;
    let names: BTreeSet<&String> = current.keys().chain(expected.keys()).collect();
    let changes: Vec<String> = names
        .into_iter()
        .filter_map(|name| match (current.get(name), expected.get(name)) {
            (previous, next) if previous == next => None,
            (previous, Some(next)) => Some(format!(
                "update {name} {next} {}",
                previous.unwrap_or(&zero)
            )),
            (Some(previous), None) => Some(format!("delete {name} {previous}")),
            (None, None) => None,
        })
        .collect();
    if changes.is_empty() {
        return Ok(());
    }
    if !run_git_input(
        repository,
        &["update-ref", "--stdin"],
        &format!("start\n{}\nprepare\ncommit\n", changes.join("\n")),
    )
    .ok
    {
        return rejected("Could not atomically restore Git refs");
    }
    Ok(())
}

fn zero_object_id<'a>(
    repository: &Path,
    object_ids: impl Iterator<Item = &'a String>,
) -> Result<String> {
    let lengths: BTreeSet<usize> = object_ids.map(String::len).collect();
    match lengths.len() {
        1 => Ok("0".repeat(*lengths.first().expect("one length"))),
        0 => {
            let format = run_git(repository, &["rev-parse", "--show-object-format"]);
            match format.stdout.trim() {
                "sha1" if format.ok => Ok("0".repeat(40)),
                "sha256" if format.ok => Ok("0".repeat(64)),
                _ => rejected("Could not determine Git object format"),
            }
        }
        _ => rejected("Git ref object formats are inconsistent"),
    }
}

fn import_bundle_objects(repository: &Path, bundle: &Path) -> Result<()> {
    verify_bundle(bundle, &[])?;
    if !run_git(
        repository,
        &["bundle", "unbundle", &resolve(bundle)?.to_string_lossy()],
    )
    .ok
    {
        return rejected("Backup Git bundle import failed");
    }
    Ok(())
}

fn apply_restore_swap(swap: &mut Swap) -> Result<()> {
    assert_unlinked_from_filesystem_root(parent(&swap.destination), "Restore swap destination")?;
    if std::fs::symlink_metadata(&swap.destination).is_ok() {
        let rollback = adjacent_path(&swap.destination, "rollback");
        std::fs::rename(&swap.destination, &rollback)?;
        swap.rollback = Some(rollback);
        swap.moved_rollback = true;
    }
    if let Some(staged) = &swap.staged {
        std::fs::rename(staged, &swap.destination)?;
        swap.moved_staged = true;
    }
    Ok(())
}

fn rollback_restore_swaps(swaps: &[Swap]) -> Result<()> {
    let mut failed = false;
    for swap in swaps.iter().rev() {
        let undone = (|| -> Result<()> {
            if swap.moved_staged && std::fs::symlink_metadata(&swap.destination).is_ok() {
                remove_any(&swap.destination)?;
            }
            if let Some(rollback) = swap
                .rollback
                .as_ref()
                .filter(|r| swap.moved_rollback && std::fs::symlink_metadata(r).is_ok())
            {
                std::fs::rename(rollback, &swap.destination)?;
            }
            Ok(())
        })();
        failed |= undone.is_err();
    }
    if failed {
        rejected("Filesystem rollback failed")
    } else {
        Ok(())
    }
}

fn cleanup_successful_restore(swaps: &[Swap]) -> Result<()> {
    let leftovers = swaps
        .iter()
        .flat_map(|swap| [swap.staged.as_ref(), swap.rollback.as_ref()])
        .flatten();
    if remove_each(leftovers) {
        Ok(())
    } else {
        rejected("Restore completed but could not remove rollback data")
    }
}

fn cleanup_unapplied_restore_staging(swaps: &[Swap]) -> Result<()> {
    if remove_each(swaps.iter().filter_map(|swap| swap.staged.as_ref())) {
        Ok(())
    } else {
        rejected("Could not remove restore staging data")
    }
}

/// Removes every existing path; whether all succeeded.
fn remove_each<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> bool {
    let mut ok = true;
    for path in paths {
        if std::fs::symlink_metadata(path).is_ok() {
            ok &= remove_any(path).is_ok();
        }
    }
    ok
}

/// `rmSync(path, { recursive: true, force: true })`.
fn remove_any(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(stat) if stat.is_dir() && !stat.is_symlink() => std::fs::remove_dir_all(path)?,
        Ok(stat) if stat.is_symlink() && path.is_dir() => std::fs::remove_dir(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(_) => {}
    }
    Ok(())
}

/// `.nosh-<purpose>-<name>-<ms>-<hex>` beside `destination` (TypeScript's debris pattern).
fn adjacent_path(destination: &Path, purpose: &str) -> PathBuf {
    let name = destination
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let random = nosh_wire::create_id(nosh_wire::IdPrefix::Cmd);
    parent(destination).join(format!(
        ".nosh-{purpose}-{name}-{millis}-{}",
        &random[4..17]
    ))
}
